//! Action executor. Translates `PeerAction` into `BleInterface` calls and follow-up `PeerCommand`s on success/failure.

use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU64, AtomicUsize, Ordering};

use bytes::Bytes;
use tokio::sync::mpsc;

use crate::transport::events::run_l2cap_accept;
use crate::transport::interface::BleInterface;
use crate::transport::peer::{PeerAction, PeerCommand};
use crate::transport::pipe::run_data_pipe;
use crate::transport::store::PeerStore;

/// A fully-reassembled datagram delivered up to iroh.
///
/// `stable_conn_id` is the `routing` handle for the pipe that
/// delivered this packet. `poll_recv` stamps iroh-facing
/// `CustomAddr`s with this id, so replies from iroh route back to
/// the exact pipe the bytes came in on.
pub struct IncomingPacket {
    pub device_id: blew::DeviceId,
    pub stable_conn_id: crate::transport::routing::StableConnId,
    pub data: Bytes,
}

/// Backoff schedule (ms) for retrying `read_psm` after a GATT subscribe.
/// Android's GATT layer needs ~100-200 ms to settle before another op can
/// succeed; the first attempt is immediate, the remaining two cover the
/// slow-path before we give up and fall back to GATT.
const READ_PSM_BACKOFFS_MS: [u64; 3] = [0, 150, 400];
// VERSION is optional; leave time for a slow GATT read without holding up an upgrade indefinitely.
const VERSION_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
// Five times blew's two-second Android disconnect fallback, and small enough
// that a teardown the platform never answers does not dominate the reconnect
// the registry is already waiting to run behind it. Bounds an asynchronous
// wait, not a blocking native call or its eventual effects.
const CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
// Discovery and subscription follow the separately configured native connect
// timeout; a missing callback in either must still return a connect failure.
const GATT_SETUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn bounded_cleanup(
    device_id: &blew::DeviceId,
    lifecycle_id: u64,
    operation: &'static str,
    work: impl std::future::Future<Output = crate::error::BleResult<()>>,
) {
    match tokio::time::timeout(CLEANUP_TIMEOUT, work).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::debug!(device = %device_id, lifecycle_id, operation, %error, "native cleanup failed");
        }
        Err(_) => {
            // Drop the waiter instead of detaching it: no late Rust continuation
            // may mutate ownership after the lane advances. Native cancellation
            // is not guaranteed; any later link-down event still follows normal
            // registry recovery. Retrying cleanup here would target the same
            // uncertain device and could hold the replacement up again.
            tracing::warn!(device = %device_id, lifecycle_id, operation, "native cleanup timed out; releasing device lane with native state uncertain");
        }
    }
}

async fn bounded_gatt_setup(
    work: impl std::future::Future<Output = crate::error::BleResult<()>>,
) -> crate::error::BleResult<()> {
    tokio::time::timeout(GATT_SETUP_TIMEOUT, work)
        .await
        .map_err(|_| crate::error::BleError::Timeout {
            stage: "GATT setup",
        })?
}

/// Translate the registry's role (`Central` = we dialed, `Peripheral` =
/// they dialed) into `routing`'s observer-local `Direction`.
fn direction_for_role(
    role: crate::transport::peer::ConnectRole,
) -> crate::transport::routing::Direction {
    use crate::transport::peer::ConnectRole;
    use crate::transport::routing::Direction;
    match role {
        ConnectRole::Central => Direction::Outbound,
        ConnectRole::Peripheral => Direction::Inbound,
    }
}

/// Retry `read_psm` using the given backoff schedule.
///
/// Returns `Ok(psm)` on first success, `Err("no psm advertised")` if the
/// remote reports no PSM (no point retrying — they don't support L2CAP),
/// and `Err("read_psm: ...")` if every attempt failed with an error.
async fn read_psm_with_retry<F, Fut>(
    backoffs_ms: &[u64],
    device_label: &blew::DeviceId,
    mut read: F,
) -> Result<u16, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = crate::error::BleResult<Option<u16>>>,
{
    let mut last_err: Option<String> = None;
    for (i, delay_ms) in backoffs_ms.iter().enumerate() {
        if *delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(*delay_ms)).await;
        }
        match read().await {
            Ok(Some(psm)) => return Ok(psm),
            Ok(None) => return Err("no psm advertised".to_string()),
            Err(e) => {
                tracing::debug!(
                    device = %device_label,
                    attempt = i + 1,
                    ?e,
                    "read_psm failed; will retry"
                );
                last_err = Some(format!("{e}"));
            }
        }
    }
    Err(format!(
        "read_psm: {}",
        last_err.unwrap_or_else(|| "unknown".into())
    ))
}

fn log_peer_metric(metric: &str) {
    if let Some(error) = metric.strip_prefix("connect_failed:") {
        tracing::debug!(%error, "BLE connect attempt failed");
        return;
    }

    if let Some(device_id) = metric.strip_prefix("restore_unknown_device=") {
        tracing::debug!(device = device_id, "adapter restored unknown BLE device");
        return;
    }

    if let Some(error) = metric.strip_prefix("l2cap_fallback_to_gatt:") {
        tracing::info!(%error, "falling back to GATT after L2CAP failure");
        return;
    }

    match metric {
        "connected_pipe_wedged" => {
            tracing::warn!("active BLE data pipe made no forward progress; draining connection");
        }
        "l2cap_duplicate_accept" => {
            tracing::debug!("ignoring duplicate inbound L2CAP channel");
        }
        "l2cap_late_accept_swapped" => {
            tracing::debug!("accepted late inbound L2CAP channel and swapped active pipe");
        }
        "l2cap_late_accept_after_gatt" => {
            tracing::debug!("accepted inbound L2CAP channel after GATT path without live pipe");
        }
        _ => {
            tracing::trace!(metric = %metric, "peer metric");
        }
    }
}

type LaneWork = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

enum LaneJob {
    Activate {
        lifecycle_id: u64,
        guard: OutstandingGuard,
    },
    Work {
        lifecycle_id: u64,
        cancellation: Cancellation,
        lifecycle: tokio::sync::watch::Receiver<Option<u64>>,
        work: LaneWork,
        guard: OutstandingGuard,
    },
}

async fn run_lane(device_id: blew::DeviceId, mut rx: mpsc::UnboundedReceiver<LaneJob>) {
    let mut native_owner = None;
    while let Some(job) = rx.recv().await {
        match job {
            LaneJob::Activate {
                lifecycle_id,
                guard,
            } => {
                let _guard = guard;
                native_owner = Some(lifecycle_id);
            }
            LaneJob::Work {
                lifecycle_id,
                cancellation,
                mut lifecycle,
                work,
                guard,
            } => {
                let _guard = guard;
                if native_owner != Some(lifecycle_id) {
                    tracing::debug!(device = %device_id, lifecycle_id, ?native_owner, "dropping work for a replaced native lifecycle");
                    continue;
                }
                if cancellation != Cancellation::Never && *lifecycle.borrow() != Some(lifecycle_id)
                {
                    tracing::debug!(device = %device_id, lifecycle_id, "dropping queued work for a retired lifecycle");
                    continue;
                }
                match cancellation {
                    Cancellation::Never | Cancellation::WhileQueued => work.await,
                    Cancellation::Immediate => {
                        tokio::select! {
                            biased;
                            _ = lifecycle.wait_for(|current| *current != Some(lifecycle_id)) => {
                                tracing::debug!(device = %device_id, lifecycle_id, "cancelling work for a retired lifecycle");
                            }
                            () = work => {}
                        }
                    }
                }
            }
        }
    }
}

/// What abandoning a device does to a job on its lane.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cancellation {
    /// Cleanup survives retirement, but only runs while its lifecycle still
    /// owns the native connection in dispatch order.
    Never,
    /// Skipped while still queued; once started it runs to completion. Used
    /// for connect, where unwinding mid-flight would leave the platform's
    /// GATT client in a state we can't reason about — the result is rejected
    /// by lifecycle identity instead.
    WhileQueued,
    /// Unwound as soon as the device is abandoned. Used for work that is
    /// already bounded and safe to drop (the VERSION read, the L2CAP open).
    Immediate,
}

/// Serialized native-connection work for one device.
///
/// Connect, disconnect, refresh, the VERSION read and the L2CAP open all
/// queue here and run one at a time in dispatch order. That ordering is what
/// makes a teardown finish before the retry that reuses the same native
/// connection begins, instead of the two racing as detached tasks.
struct DeviceLane {
    jobs: mpsc::UnboundedSender<LaneJob>,
    worker: tokio::task::JoinHandle<()>,
    /// Registry lifecycle currently allowed to run cancellable work.
    /// Retirement clears this immediately; native ownership changes in the worker.
    lifecycle: tokio::sync::watch::Sender<Option<u64>>,
    /// Jobs queued or running. A lane with none left can be dropped.
    outstanding: Arc<AtomicUsize>,
}

impl Drop for DeviceLane {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

struct OutstandingGuard(Arc<AtomicUsize>);

impl Drop for OutstandingGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

pub struct Driver<I: BleInterface> {
    iface: Arc<I>,
    inbox: mpsc::Sender<PeerCommand>,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    retransmit_counter: Arc<AtomicU64>,
    truncation_counter: Arc<AtomicU64>,
    empty_frames_counter: Arc<AtomicU64>,
    store: Arc<dyn PeerStore>,
    /// Authoritative routing table. Mints a `StableConnId` on every
    /// pipe open (or reuses a reservation's id) and evicts on pipe
    /// close. `poll_send` and `poll_recv` both resolve via this.
    routing: Arc<crate::transport::routing::Routing>,
    /// Live iroh connections indexed by pipe. Closed when their pipe
    /// dies — iroh cannot observe that on its own. Defaults to an empty
    /// registry so tests that build a `Driver` directly need not supply
    /// one; the real transport installs its own via `with_connections`.
    connections: Arc<crate::transport::conns::ConnectionRegistry>,
    /// Per-device serialization of native-connection work. Owned here so
    /// the workers die with the driver instead of outliving the actor.
    lanes: parking_lot::Mutex<HashMap<blew::DeviceId, DeviceLane>>,
}

impl<I: BleInterface> Driver<I> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        iface: Arc<I>,
        inbox: mpsc::Sender<PeerCommand>,
        incoming_tx: mpsc::Sender<IncomingPacket>,
        retransmit_counter: Arc<AtomicU64>,
        truncation_counter: Arc<AtomicU64>,
        empty_frames_counter: Arc<AtomicU64>,
        store: Arc<dyn PeerStore>,
        routing: Arc<crate::transport::routing::Routing>,
    ) -> Self {
        Self {
            iface,
            inbox,
            incoming_tx,
            retransmit_counter,
            truncation_counter,
            empty_frames_counter,
            store,
            routing,
            connections: Arc::new(crate::transport::conns::ConnectionRegistry::default()),
            lanes: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    fn activate(&self, device_id: &blew::DeviceId, lifecycle_id: u64) {
        let mut lanes = self.lanes.lock();
        lanes.retain(|id, lane| {
            id == device_id
                || lane.lifecycle.borrow().is_some()
                || lane.outstanding.load(Ordering::Relaxed) > 0
        });
        let lane = lanes.entry(device_id.clone()).or_insert_with(|| {
            let (jobs, rx) = mpsc::unbounded_channel::<LaneJob>();
            let worker = tokio::spawn(run_lane(device_id.clone(), rx));
            DeviceLane {
                jobs,
                worker,
                lifecycle: tokio::sync::watch::channel(None).0,
                outstanding: Arc::new(AtomicUsize::new(0)),
            }
        });
        lane.lifecycle.send_replace(Some(lifecycle_id));
        let outstanding = Arc::clone(&lane.outstanding);
        outstanding.fetch_add(1, Ordering::Relaxed);
        let guard = OutstandingGuard(outstanding);
        // Ownership changes in native dispatch order, after any earlier teardown.
        let _ = lane.jobs.send(LaneJob::Activate {
            lifecycle_id,
            guard,
        });
    }

    fn retire(&self, device_id: &blew::DeviceId, lifecycle_id: u64) {
        if let Some(lane) = self.lanes.lock().get(device_id) {
            lane.lifecycle.send_if_modified(|current| {
                if *current == Some(lifecycle_id) {
                    *current = None;
                    true
                } else {
                    false
                }
            });
        }
    }

    fn dispatch(
        &self,
        device_id: &blew::DeviceId,
        lifecycle_id: u64,
        cancellation: Cancellation,
        work: LaneWork,
    ) {
        let lanes = self.lanes.lock();
        let Some(lane) = lanes.get(device_id) else {
            tracing::debug!(device = %device_id, lifecycle_id, "device has no lane; dropping job");
            return;
        };
        let lifecycle = lane.lifecycle.subscribe();
        lane.outstanding.fetch_add(1, Ordering::Relaxed);
        let guard = OutstandingGuard(Arc::clone(&lane.outstanding));
        let job = LaneJob::Work {
            lifecycle_id,
            cancellation,
            lifecycle,
            work,
            guard,
        };
        if lane.jobs.send(job).is_err() {
            tracing::debug!(device = %device_id, "device lane closed; dropping job");
        }
    }

    /// Install the transport's connection registry. Separate from `new`
    /// so the many test call sites don't all have to pass one.
    #[must_use]
    pub fn with_connections(
        mut self,
        connections: Arc<crate::transport::conns::ConnectionRegistry>,
    ) -> Self {
        self.connections = connections;
        self
    }

    pub async fn execute(&self, action: PeerAction) {
        match action {
            PeerAction::StartConnect {
                device_id,
                attempt: _,
                lifecycle_id,
            } => {
                let iface = Arc::clone(&self.iface);
                let inbox = self.inbox.clone();
                let dev_for_job = device_id.clone();
                let dev_for_msg = device_id.clone();
                self.activate(&device_id, lifecycle_id);
                // blew enforces `CentralConfig::connect_timeout` itself
                // (15 s default, overridable by the app). On expiry it
                // refresh()+close()s the Android GATT client and
                // returns `BlewError::ConnectTimedOut`, which flows
                // through the `Err` arm below into the registry's
                // normal retry logic.
                self.dispatch(
                    &device_id,
                    lifecycle_id,
                    Cancellation::WhileQueued,
                    Box::pin(async move {
                        match iface.connect(&dev_for_job).await {
                            Ok(channel) => {
                                let _ = inbox
                                    .send(PeerCommand::ConnectSucceeded {
                                        device_id: dev_for_msg,
                                        lifecycle_id,
                                        channel,
                                    })
                                    .await;
                            }
                            Err(e) => {
                                let _ = inbox
                                    .send(PeerCommand::ConnectFailed {
                                        device_id: dev_for_msg,
                                        lifecycle_id,
                                        error: format!("{e}"),
                                    })
                                    .await;
                            }
                        }
                    }),
                );
            }

            PeerAction::ReadVersion {
                device_id,
                lifecycle_id,
            } => {
                let iface = Arc::clone(&self.iface);
                let inbox = self.inbox.clone();
                let dev_for_job = device_id.clone();
                let dev_for_msg = device_id.clone();
                self.dispatch(
                    &device_id,
                    lifecycle_id,
                    Cancellation::Immediate,
                    Box::pin(async move {
                        let want = crate::transport::transport::PROTOCOL_VERSION;
                        let result = tokio::time::timeout(
                            VERSION_READ_TIMEOUT,
                            iface.read_version(&dev_for_job),
                        ).await;
                        let Ok(result) = result else {
                            tracing::debug!(device = %dev_for_job, "VERSION read timed out; treating as skip");
                            return;
                        };
                        match result {
                            Ok(Some(got)) if got != want => {
                                let _ = inbox
                                    .send(PeerCommand::ProtocolVersionMismatch {
                                        device_id: dev_for_msg,
                                        lifecycle_id,
                                        got,
                                        want,
                                    })
                                    .await;
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::debug!(
                                    device = %dev_for_msg,
                                    ?e,
                                    "read_version returned error; treating as skip"
                                );
                            }
                        }
                    }),
                );
            }

            PeerAction::RetireLifecycle {
                device_id,
                lifecycle_id,
            } => {
                self.retire(&device_id, lifecycle_id);
            }

            PeerAction::CloseChannel {
                device_id,
                lifecycle_id,
                ..
            } => {
                let iface = Arc::clone(&self.iface);
                let dev_for_job = device_id.clone();
                self.dispatch(
                    &device_id,
                    lifecycle_id,
                    Cancellation::Never,
                    Box::pin(async move {
                        bounded_cleanup(
                            &dev_for_job,
                            lifecycle_id,
                            "disconnect",
                            iface.disconnect(&dev_for_job),
                        )
                        .await;
                    }),
                );
            }

            PeerAction::CloseNativeConnection {
                device_id,
                lifecycle_id,
            } => {
                let iface = Arc::clone(&self.iface);
                let dev_for_job = device_id.clone();
                self.dispatch(
                    &device_id,
                    lifecycle_id,
                    Cancellation::Never,
                    Box::pin(async move {
                        bounded_cleanup(
                            &dev_for_job,
                            lifecycle_id,
                            "close native connection",
                            iface.disconnect(&dev_for_job),
                        )
                        .await;
                    }),
                );
            }

            PeerAction::Refresh {
                device_id,
                lifecycle_id,
            } => {
                let iface = Arc::clone(&self.iface);
                let dev_for_job = device_id.clone();
                self.dispatch(
                    &device_id,
                    lifecycle_id,
                    Cancellation::Never,
                    Box::pin(async move {
                        bounded_cleanup(
                            &dev_for_job,
                            lifecycle_id,
                            "refresh",
                            iface.refresh(&dev_for_job),
                        )
                        .await;
                    }),
                );
            }

            PeerAction::AckSend { waker, .. } => {
                waker.wake();
            }

            PeerAction::RebuildGattServer => {
                let iface = Arc::clone(&self.iface);
                tokio::spawn(async move {
                    let _ = iface.rebuild_server().await;
                });
            }

            PeerAction::RestartAdvertising => {
                let iface = Arc::clone(&self.iface);
                tokio::spawn(async move {
                    let _ = iface.restart_advertising().await;
                });
            }

            PeerAction::RestartL2capListener => {
                let iface = Arc::clone(&self.iface);
                tokio::spawn(async move {
                    let _ = iface.restart_l2cap_listener().await;
                });
            }

            PeerAction::PutPeerStore { prefix, snapshot } => {
                let store = Arc::clone(&self.store);
                tokio::spawn(async move {
                    if let Err(e) = store.put(prefix, snapshot).await {
                        tracing::debug!(?e, "PeerStore::put failed");
                    }
                });
            }

            PeerAction::ForgetPeerStore { prefix } => {
                let store = Arc::clone(&self.store);
                tokio::spawn(async move {
                    if let Err(e) = store.forget(prefix).await {
                        tracing::debug!(?e, "PeerStore::forget failed");
                    }
                });
            }

            PeerAction::EmitMetric(ev) => {
                log_peer_metric(&ev);
            }

            PeerAction::StartDataPipe {
                device_id,
                lifecycle_id,
                tx_gen,
                role,
                target_endpoint,
                path,
                l2cap_channel,
            } => {
                tracing::debug!(device = %device_id, ?role, ?path, "StartDataPipe");
                self.activate(&device_id, lifecycle_id);
                let (outbound_tx, outbound_rx) =
                    mpsc::channel::<crate::transport::peer::PendingSend>(32);
                let (inbound_tx, inbound_rx) = mpsc::channel::<Bytes>(64);
                let (swap_tx, swap_rx) = mpsc::channel::<blew::L2capChannel>(1);
                let last_rx_at = crate::transport::peer::LivenessClock::new();
                let iface: Arc<dyn BleInterface> = Arc::clone(&self.iface) as Arc<dyn BleInterface>;
                let incoming_tx = self.incoming_tx.clone();
                let inbox = self.inbox.clone();
                let retransmit_counter = Arc::clone(&self.retransmit_counter);
                let truncation_counter = Arc::clone(&self.truncation_counter);
                let empty_frames_counter = Arc::clone(&self.empty_frames_counter);
                let dev_for_ready = device_id.clone();
                let pipe_last_rx_at = last_rx_at.clone();
                // Register the pipe with routing and enter the
                // pending pool. If the resolver previously minted a
                // reservation for this peer's intended endpoint, reuse that id
                // so iroh's outstanding `CustomAddr` stays valid
                // across the dial — only outbound pipes match
                // reservations (inbound accepts have no resolver).
                let routing = Arc::clone(&self.routing);
                let connections = Arc::clone(&self.connections);
                let direction = direction_for_role(role);
                let (stable_id, reservation_endpoint) = match target_endpoint
                    .and_then(|endpoint| routing.consume_reservation_for_endpoint(&endpoint))
                    .or_else(|| routing.consume_reservation_for_device(&device_id))
                {
                    Some(reservation) => {
                        routing.register_pipe_with_id(
                            reservation.stable_id,
                            device_id.clone(),
                            direction,
                        );
                        tracing::info!(
                            device = %device_id,
                            stable_id = %reservation.stable_id,
                            endpoint = %reservation.endpoint_id,
                            "StartDataPipe: bound pipe to resolver reservation"
                        );
                        (reservation.stable_id, Some(reservation.endpoint_id))
                    }
                    None => (routing.register_pipe(device_id.clone(), direction), None),
                };
                routing.register_pending(stable_id, reservation_endpoint);
                tokio::spawn(async move {
                    run_data_pipe(
                        iface,
                        device_id,
                        stable_id,
                        role,
                        path,
                        l2cap_channel,
                        outbound_rx,
                        inbound_rx,
                        incoming_tx,
                        inbox,
                        swap_rx,
                        retransmit_counter,
                        truncation_counter,
                        empty_frames_counter,
                        pipe_last_rx_at,
                    )
                    .await;
                    // Drop the pool entry before the pipe itself so
                    // the pool never references a non-existent pipe.
                    routing.evict_pipe_state(stable_id);
                    // The pipe is gone and iroh has no way to learn that:
                    // a connection whose only path was this pipe would
                    // otherwise sit there until the application's own
                    // deadline. Close after `evict_pipe_state` so the
                    // `ConnectionClosed` events these closes provoke find
                    // no routable entry and don't loop back as a fresh
                    // `Stalled` for a pipe that is already dead.
                    let closed = connections.close_pipe(
                        stable_id,
                        iroh::endpoint::VarInt::from_u32(
                            crate::transport::conns::BLE_CLOSE_CODE_RETRY,
                        ),
                        crate::transport::conns::BLE_CLOSE_REASON_PIPE_CLOSED,
                    );
                    if closed > 0 {
                        tracing::info!(
                            %stable_id,
                            closed,
                            "closed iroh connections riding a BLE pipe that went away"
                        );
                    }
                    routing.evict_pipe(stable_id);
                });
                let ready = PeerCommand::DataPipeReady {
                    device_id: dev_for_ready,
                    tx_gen,
                    outbound_tx,
                    inbound_tx,
                    swap_tx,
                    last_rx_at,
                };
                if self.inbox.send(ready).await.is_err() {
                    tracing::debug!("inbox closed before DataPipeReady forwarded");
                }
            }
            PeerAction::UpgradeToL2cap {
                device_id,
                lifecycle_id,
                upgrade_gen,
            } => {
                self.spawn_l2cap_open(device_id, lifecycle_id, upgrade_gen);
            }
            PeerAction::SwapPipeToL2cap {
                device_id,
                channel,
                swap_tx,
            } => {
                tokio::spawn(async move {
                    if swap_tx.send(channel).await.is_err() {
                        tracing::debug!(device = %device_id, "swap_tx closed; pipe supervisor already gone");
                    }
                });
            }
        }
    }

    fn spawn_l2cap_open(&self, device_id: blew::DeviceId, lifecycle_id: u64, upgrade_gen: u64) {
        let iface = Arc::clone(&self.iface);
        let inbox = self.inbox.clone();
        let dev_for_job = device_id.clone();
        let dev_for_msg = device_id.clone();
        self.dispatch(
            &device_id,
            lifecycle_id,
            Cancellation::Immediate,
            Box::pin(async move {
                let result = tokio::time::timeout(super::registry::L2CAP_SELECT_TIMEOUT, async {
                    let psm = read_psm_with_retry(&READ_PSM_BACKOFFS_MS, &dev_for_job, || {
                        let iface = Arc::clone(&iface);
                        let dev = dev_for_job.clone();
                        async move { iface.read_psm(&dev).await }
                    })
                    .await?;
                    iface
                        .open_l2cap(&dev_for_job, psm)
                        .await
                        .map_err(|e| format!("{e}"))
                })
                .await;
                match result {
                    Ok(Ok(channel)) => {
                        let _ = inbox
                            .send(PeerCommand::OpenL2capSucceeded {
                                device_id: dev_for_msg,
                                lifecycle_id,
                                upgrade_gen,
                                channel,
                            })
                            .await;
                    }
                    Ok(Err(error)) => {
                        let _ = inbox
                            .send(PeerCommand::OpenL2capFailed {
                                device_id: dev_for_msg,
                                lifecycle_id,
                                upgrade_gen,
                                error,
                            })
                            .await;
                    }
                    Err(_elapsed) => {
                        let _ = inbox
                            .send(PeerCommand::OpenL2capFailed {
                                device_id: dev_for_msg,
                                lifecycle_id,
                                upgrade_gen,
                                error: "l2cap select timeout".into(),
                            })
                            .await;
                    }
                }
            }),
        );
    }
}

// ====================== BlewDriver ======================
// Real BleInterface implementation backed by blew::Central + blew::Peripheral.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use blew::central::ScanFilter;
use blew::gatt::service::GattService;
use blew::l2cap::types::Psm;
use blew::peripheral::{AdvertisingConfig, Delivery};
use blew::{Central, L2capChannel, Peripheral};
use uuid::{Uuid, uuid};

use crate::transport::peer::{ChannelHandle, ConnectPath};

const C2P_CHAR_UUID: Uuid = uuid!("69726f02-8e45-4c2c-b3a5-331f3098b5c2");
const P2C_CHAR_UUID: Uuid = uuid!("69726f03-8e45-4c2c-b3a5-331f3098b5c2");
const PSM_CHAR_UUID: Uuid = uuid!("69726f04-8e45-4c2c-b3a5-331f3098b5c2");
const VERSION_CHAR_UUID: Uuid = uuid!("69726f05-8e45-4c2c-b3a5-331f3098b5c2");

pub struct BlewDriver {
    central: Arc<Central>,
    peripheral: Arc<Peripheral>,
    next_channel_id: AtomicU64,
    channels_by_device: Mutex<HashMap<blew::DeviceId, ChannelHandle>>,
    /// Stashed at construction so `rebuild_server` / `restart_advertising` can
    /// re-register the same service table and advertise with the same config
    /// after an adapter-off/on cycle wipes platform state.
    services: Vec<GattService>,
    advertising_config: AdvertisingConfig,
    /// Shared PSM value, updated when the L2CAP listener is (re)started.
    /// Zero means "no PSM advertised yet".
    psm: Arc<AtomicU16>,
    /// Inbox for spawning `run_l2cap_accept` after a listener restart.
    inbox: mpsc::Sender<PeerCommand>,
}

impl BlewDriver {
    pub fn new(
        central: Arc<Central>,
        peripheral: Arc<Peripheral>,
        services: Vec<GattService>,
        advertising_config: AdvertisingConfig,
        psm: Arc<AtomicU16>,
        inbox: mpsc::Sender<PeerCommand>,
    ) -> Self {
        Self {
            central,
            peripheral,
            next_channel_id: AtomicU64::new(1),
            channels_by_device: Mutex::new(HashMap::new()),
            services,
            advertising_config,
            psm,
            inbox,
        }
    }
}

#[async_trait]
impl BleInterface for BlewDriver {
    async fn connect(&self, device_id: &blew::DeviceId) -> crate::error::BleResult<ChannelHandle> {
        self.central.connect(device_id).await?;
        // GATT is not usable until services are discovered and P2C notifications
        // are subscribed. Android/Apple both require this explicitly before
        // write_characteristic or delivering notifications.
        bounded_gatt_setup(async {
            self.central.discover_services(device_id).await?;
            self.central
                .subscribe_characteristic(device_id, P2C_CHAR_UUID)
                .await?;
            Ok(())
        })
        .await?;
        let id = self.next_channel_id.fetch_add(1, Ordering::Relaxed);
        let handle = ChannelHandle {
            id,
            path: ConnectPath::Gatt,
        };
        self.channels_by_device
            .lock()
            .expect("channels_by_device mutex poisoned")
            .insert(device_id.clone(), handle.clone());
        Ok(handle)
    }

    async fn disconnect(&self, device_id: &blew::DeviceId) -> crate::error::BleResult<()> {
        self.central.disconnect(device_id).await?;
        self.channels_by_device
            .lock()
            .expect("channels_by_device mutex poisoned")
            .remove(device_id);
        Ok(())
    }

    async fn write_c2p(
        &self,
        device_id: &blew::DeviceId,
        bytes: Bytes,
    ) -> crate::error::BleResult<()> {
        let len = bytes.len();
        let result = self
            .central
            .write_characteristic(
                device_id,
                C2P_CHAR_UUID,
                bytes.to_vec(),
                blew::central::WriteType::WithoutResponse,
            )
            .await;
        match &result {
            Ok(()) => tracing::trace!(device = %device_id, len, "write_c2p ok"),
            // Callers (ReliableChannel) handle the error — at this layer it
            // is just "the radio refused a packet", not an operator-actionable
            // warning.
            Err(e) => tracing::debug!(device = %device_id, len, err = %e, "write_c2p err"),
        }
        result?;
        Ok(())
    }

    async fn notify_p2c(
        &self,
        device_id: &blew::DeviceId,
        bytes: Bytes,
    ) -> crate::error::BleResult<()> {
        let len = bytes.len();
        let result = self
            .peripheral
            .notify_characteristic(device_id, P2C_CHAR_UUID, bytes.to_vec())
            .await;
        match &result {
            // NoSubscriber is a success that sent nothing: the central
            // unsubscribed or dropped between our decision and the send. The
            // ReliableChannel's ACK timeout retransmits, so it is not an error
            // here, but it is not an ordinary send either.
            Ok(Delivery::NoSubscriber) => {
                tracing::debug!(device = %device_id, len, "notify_p2c dropped: no subscriber");
            }
            Ok(delivery) => {
                tracing::trace!(device = %device_id, len, ?delivery, "notify_p2c ok");
            }
            Err(e) => tracing::debug!(device = %device_id, len, err = %e, "notify_p2c err"),
        }
        result?;
        Ok(())
    }

    async fn read_psm(&self, device_id: &blew::DeviceId) -> crate::error::BleResult<Option<u16>> {
        let bytes = self
            .central
            .read_characteristic(device_id, PSM_CHAR_UUID)
            .await?;
        if bytes.len() < 2 {
            return Ok(None);
        }
        Ok(Some(u16::from_le_bytes([bytes[0], bytes[1]])))
    }

    async fn read_version(
        &self,
        device_id: &blew::DeviceId,
    ) -> crate::error::BleResult<Option<u8>> {
        match self
            .central
            .read_characteristic(device_id, VERSION_CHAR_UUID)
            .await
        {
            Ok(bytes) if bytes.is_empty() => Ok(None),
            Ok(bytes) => Ok(Some(bytes[0])),
            // Older peers may not publish VERSION; treat as "skip the check".
            Err(e) => {
                tracing::debug!(device = %device_id, ?e, "read_version failed; skipping check");
                Ok(None)
            }
        }
    }

    async fn open_l2cap(
        &self,
        device_id: &blew::DeviceId,
        psm: u16,
    ) -> crate::error::BleResult<L2capChannel> {
        let channel = self.central.open_l2cap_channel(device_id, Psm(psm)).await?;
        Ok(channel)
    }

    async fn start_scan(&self) -> crate::error::BleResult<()> {
        self.central.start_scan(ScanFilter::default()).await?;
        Ok(())
    }

    async fn stop_scan(&self) -> crate::error::BleResult<()> {
        self.central.stop_scan().await?;
        Ok(())
    }

    async fn rebuild_server(&self) -> crate::error::BleResult<()> {
        // Best-effort: adapter-cycle typically wipes the platform's service
        // table on Android, so re-adding is required; on macOS/iOS this is
        // often a no-op because CoreBluetooth restores state for us.
        if let Err(e) = self.peripheral.stop_advertising().await {
            tracing::debug!(?e, "rebuild_server: stop_advertising ignored");
        }
        for service in &self.services {
            if let Err(e) = self.peripheral.add_service(service).await {
                tracing::warn!(uuid = %service.uuid, ?e, "rebuild_server: add_service failed");
            }
        }
        Ok(())
    }

    async fn restart_advertising(&self) -> crate::error::BleResult<()> {
        if let Err(e) = self.peripheral.stop_advertising().await {
            tracing::debug!(?e, "restart_advertising: stop_advertising ignored");
        }
        self.peripheral
            .start_advertising(&self.advertising_config)
            .await?;
        Ok(())
    }

    async fn restart_l2cap_listener(&self) -> crate::error::BleResult<Option<u16>> {
        match self.peripheral.l2cap_listener().await {
            Ok((psm, listener)) => {
                let psm_val = psm.value();
                self.psm.store(psm_val, Ordering::Relaxed);
                tracing::info!(
                    psm = psm_val,
                    "L2CAP listener restarted after adapter cycle"
                );
                tokio::spawn(run_l2cap_accept(listener, self.inbox.clone()));
                Ok(Some(psm_val))
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "L2CAP listener restart failed after adapter cycle; inbound L2CAP upgrades disabled"
                );
                Ok(None)
            }
        }
    }

    async fn is_powered(&self) -> bool {
        self.central.is_powered().await.unwrap_or(false)
    }

    async fn refresh(&self, device_id: &blew::DeviceId) -> crate::error::BleResult<()> {
        #[cfg(target_os = "android")]
        {
            self.central.refresh(device_id).await?;
            Ok(())
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = device_id;
            Ok(())
        }
    }

    async fn mtu(&self, device_id: &blew::DeviceId) -> u16 {
        self.central.mtu(device_id).await
    }
}

#[cfg(test)]
mod read_psm_tests {
    use super::*;
    use crate::error::BleError;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn succeeds_on_first_attempt_without_sleeping() {
        let dev = blew::DeviceId::from("dev");
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = Arc::clone(&calls);
        let psm = read_psm_with_retry(&[0, 150, 400], &dev, || {
            let calls = Arc::clone(&calls_c);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Some(0x0080u16))
            }
        })
        .await
        .unwrap();
        assert_eq!(psm, 0x0080);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn retries_transient_errors_then_succeeds() {
        let dev = blew::DeviceId::from("dev");
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = Arc::clone(&calls);
        let psm = read_psm_with_retry(&[0, 150, 400], &dev, || {
            let calls = Arc::clone(&calls_c);
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Err(BleError::Protocol(format!("busy {n}")))
                } else {
                    Ok(Some(0x0081u16))
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(psm, 0x0081);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn no_psm_advertised_does_not_retry() {
        let dev = blew::DeviceId::from("dev");
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = Arc::clone(&calls);
        let err = read_psm_with_retry(&[0, 150, 400], &dev, || {
            let calls = Arc::clone(&calls_c);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            }
        })
        .await
        .unwrap_err();
        assert_eq!(err, "no psm advertised");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn all_attempts_fail_returns_last_error() {
        let dev = blew::DeviceId::from("dev");
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = Arc::clone(&calls);
        let err = read_psm_with_retry(&[0, 150, 400], &dev, || {
            let calls = Arc::clone(&calls_c);
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                Err::<Option<u16>, _>(BleError::Protocol(format!("boom {n}")))
            }
        })
        .await
        .unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(err.starts_with("read_psm:"), "unexpected: {err}");
        assert!(err.contains("boom 2"), "expected last error, got: {err}");
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;
    use crate::transport::test_util::{CallKind, MockBleInterface};
    use bytes::Bytes;

    #[test]
    fn incoming_packet_carries_device_id() {
        let pkt = IncomingPacket {
            device_id: blew::DeviceId::from("test"),
            stable_conn_id: crate::transport::routing::StableConnId::for_test(1),
            data: Bytes::from_static(b"x"),
        };
        assert_eq!(pkt.device_id, blew::DeviceId::from("test"));
    }

    #[tokio::test]
    async fn start_data_pipe_spawns_pipe_and_emits_data_pipe_ready() {
        use crate::transport::peer::{ConnectPath, ConnectRole};

        let iface = Arc::new(MockBleInterface::new());
        let (tx, mut rx) = mpsc::channel(16);
        let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(4);
        let driver = Driver::new(
            iface,
            tx,
            incoming_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(crate::transport::store::InMemoryPeerStore::new()),
            Arc::new(crate::transport::routing::Routing::new()),
        );

        driver
            .execute(PeerAction::StartDataPipe {
                device_id: blew::DeviceId::from("start-pipe"),
                lifecycle_id: 1,
                tx_gen: 7,
                role: ConnectRole::Central,
                target_endpoint: None,
                path: ConnectPath::Gatt,
                l2cap_channel: None,
            })
            .await;

        let cmd = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match cmd {
            PeerCommand::DataPipeReady {
                device_id, tx_gen, ..
            } => {
                assert_eq!(device_id, blew::DeviceId::from("start-pipe"));
                assert_eq!(tx_gen, 7);
            }
            other => panic!("expected DataPipeReady, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn start_data_pipe_spawns_pipe_and_emits_data_pipe_ready_peripheral() {
        use crate::transport::peer::{ConnectPath, ConnectRole};

        let iface = Arc::new(MockBleInterface::new());
        let (tx, mut rx) = mpsc::channel(16);
        let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(4);
        let driver = Driver::new(
            iface,
            tx,
            incoming_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(crate::transport::store::InMemoryPeerStore::new()),
            Arc::new(crate::transport::routing::Routing::new()),
        );

        driver
            .execute(PeerAction::StartDataPipe {
                device_id: blew::DeviceId::from("start-pipe-peri"),
                lifecycle_id: 1,
                tx_gen: 9,
                role: ConnectRole::Peripheral,
                target_endpoint: None,
                path: ConnectPath::Gatt,
                l2cap_channel: None,
            })
            .await;

        let cmd = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match cmd {
            PeerCommand::DataPipeReady {
                device_id, tx_gen, ..
            } => {
                assert_eq!(device_id, blew::DeviceId::from("start-pipe-peri"));
                assert_eq!(tx_gen, 9);
            }
            other => panic!("expected DataPipeReady, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pipe_exit_closes_the_iroh_connections_riding_that_pipe() {
        // A pipe going away is invisible to iroh: it cannot report a
        // path as dead, and it never migrates an existing Connection
        // onto a newly resolved CustomAddr. Left alone, a connection
        // whose only path was this pipe sits there until the
        // application's own deadline fires.
        use crate::transport::conns::{
            BLE_CLOSE_CODE_RETRY, BLE_CLOSE_REASON_PIPE_CLOSED, ConnHandle, ConnectionRegistry,
            RecordingConn,
        };
        use crate::transport::peer::{ConnectPath, ConnectRole};

        let iface = Arc::new(MockBleInterface::new());
        let (tx, mut rx) = mpsc::channel(16);
        let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(4);
        let routing = Arc::new(crate::transport::routing::Routing::new());
        let connections = Arc::new(ConnectionRegistry::default());
        let driver = Driver::new(
            iface,
            tx,
            incoming_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(crate::transport::store::InMemoryPeerStore::new()),
            Arc::clone(&routing),
        )
        .with_connections(Arc::clone(&connections));

        driver
            .execute(PeerAction::StartDataPipe {
                device_id: blew::DeviceId::from("dying-pipe"),
                lifecycle_id: 1,
                tx_gen: 1,
                role: ConnectRole::Central,
                target_endpoint: None,
                path: ConnectPath::Gatt,
                l2cap_channel: None,
            })
            .await;

        let ready = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let pipes = routing.pipes_for_debug();
        assert_eq!(pipes.len(), 1);
        let stable_id = pipes[0].id;

        let endpoint = iroh_base::SecretKey::from_bytes(&[0x59u8; 32]).public();
        let conn = RecordingConn::new();
        connections.insert(
            endpoint,
            stable_id,
            Arc::clone(&conn) as Arc<dyn ConnHandle>,
        );

        // Dropping DataPipeReady drops the outbound sender, which is how
        // the registry signals a pipe is done: the supervisor exits.
        drop(ready);

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while conn.closes().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("pipe exit must close the connections riding it");

        assert_eq!(
            conn.closes(),
            vec![(
                u64::from(BLE_CLOSE_CODE_RETRY),
                BLE_CLOSE_REASON_PIPE_CLOSED.to_vec()
            )]
        );
        assert!(
            routing.pipes_for_debug().is_empty(),
            "the pipe is evicted from routing as before"
        );
    }

    #[tokio::test]
    async fn start_data_pipe_consumes_reservation_for_target_endpoint() {
        // Step 4c contract: if the resolver previously minted a
        // reservation for this peer's endpoint, StartDataPipe must bind
        // the opened pipe to the *reserved* StableConnId (not a fresh
        // mint). Otherwise iroh's outstanding `CustomAddr` would point
        // at a dead reservation forever.
        use crate::transport::peer::{ConnectPath, ConnectRole};

        let iface = Arc::new(MockBleInterface::new());
        let (tx, mut rx) = mpsc::channel(16);
        let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(4);
        let routing = Arc::new(crate::transport::routing::Routing::new());
        let driver = Driver::new(
            iface,
            tx,
            incoming_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(crate::transport::store::InMemoryPeerStore::new()),
            Arc::clone(&routing),
        );

        // Pre-seed: scan_hint maps the peer's prefix → device_id, and
        // the resolver reserves a stable_id for the endpoint. Mirrors what happens when
        // iroh asks to dial a peer the scanner has just surfaced.
        let endpoint = iroh_base::SecretKey::from_bytes(&[0x57u8; 32]).public();
        let prefix = crate::transport::routing::prefix_from_endpoint(&endpoint);
        let device_id = blew::DeviceId::from("reserved-peer");
        routing.note_scan_hint(prefix, device_id.clone());
        let reserved_id = routing.reserve_outbound(endpoint);

        driver
            .execute(PeerAction::StartDataPipe {
                device_id: device_id.clone(),
                lifecycle_id: 1,
                tx_gen: 3,
                role: ConnectRole::Central,
                target_endpoint: Some(endpoint),
                path: ConnectPath::Gatt,
                l2cap_channel: None,
            })
            .await;

        // The only live pipe must carry the reserved id.
        let pipes = routing.pipes_for_debug();
        assert_eq!(pipes.len(), 1);
        assert_eq!(
            pipes[0].id, reserved_id,
            "StartDataPipe must reuse the reserved StableConnId"
        );
        // Reservation is consumed.
        assert_eq!(routing.reservation_len(), 0);
        // And the pending entry carries the endpoint target picked up
        // from the reservation, so promote() has the context it needs.
        assert_eq!(routing.pending_pipe_for(&endpoint), Some(reserved_id));

        // Drain the ready command so rx is clean.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await;
    }

    #[tokio::test]
    async fn start_data_pipe_consumes_reservation_after_scan_hint_flip() {
        use crate::transport::peer::{ConnectPath, ConnectRole};

        let iface = Arc::new(MockBleInterface::new());
        let (tx, mut rx) = mpsc::channel(16);
        let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(4);
        let routing = Arc::new(crate::transport::routing::Routing::new());
        let driver = Driver::new(
            iface,
            tx,
            incoming_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(crate::transport::store::InMemoryPeerStore::new()),
            Arc::clone(&routing),
        );

        let endpoint = iroh_base::SecretKey::from_bytes(&[0x58u8; 32]).public();
        let prefix = crate::transport::routing::prefix_from_endpoint(&endpoint);
        let old_device = blew::DeviceId::from("reserved-peer-old");
        let new_device = blew::DeviceId::from("reserved-peer-new");
        routing.note_scan_hint(prefix, old_device.clone());
        let reserved_id = routing.reserve_outbound(endpoint);

        routing.note_scan_hint(prefix, new_device);

        driver
            .execute(PeerAction::StartDataPipe {
                device_id: old_device,
                lifecycle_id: 1,
                tx_gen: 3,
                role: ConnectRole::Central,
                target_endpoint: Some(endpoint),
                path: ConnectPath::Gatt,
                l2cap_channel: None,
            })
            .await;

        let pipes = routing.pipes_for_debug();
        assert_eq!(pipes.len(), 1);
        assert_eq!(
            pipes[0].id, reserved_id,
            "StartDataPipe must keep the reserved StableConnId even if scan_hint flipped"
        );
        assert_eq!(routing.reservation_len(), 0);
        assert_eq!(routing.pending_pipe_for(&endpoint), Some(reserved_id));

        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await;
    }

    #[tokio::test]
    async fn start_data_pipe_registers_pending_and_evicts_on_close() {
        // StartDataPipe adds the new pipe to the pending pool with
        // target_endpoint=None; pipe close evicts from whichever pool
        // (pending or routable) held it. Without this, `promote` would
        // see no pending entry to promote, and the resolver wouldn't
        // find the pipe.
        use crate::transport::peer::{ConnectPath, ConnectRole};

        let iface = Arc::new(MockBleInterface::new());
        let (tx, mut rx) = mpsc::channel(16);
        let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(4);
        let routing = Arc::new(crate::transport::routing::Routing::new());
        let driver = Driver::new(
            iface,
            tx,
            incoming_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(crate::transport::store::InMemoryPeerStore::new()),
            Arc::clone(&routing),
        );

        assert_eq!(routing.snapshot().pending, 0);

        driver
            .execute(PeerAction::StartDataPipe {
                device_id: blew::DeviceId::from("pending-peer"),
                lifecycle_id: 1,
                tx_gen: 1,
                role: ConnectRole::Central,
                target_endpoint: None,
                path: ConnectPath::Gatt,
                l2cap_channel: None,
            })
            .await;

        assert_eq!(
            routing.snapshot().pending,
            1,
            "StartDataPipe must register the pipe as pending"
        );
        let pipes = routing.pipes_for_debug();
        assert_eq!(pipes.len(), 1);
        let pipe_id = pipes[0].id;

        // Drive the pipe to exit and watch both pipes + pending drain.
        let cmd = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let (outbound_tx, inbound_tx) = match cmd {
            PeerCommand::DataPipeReady {
                outbound_tx,
                inbound_tx,
                ..
            } => (outbound_tx, inbound_tx),
            other => panic!("expected DataPipeReady, got {other:?}"),
        };
        drop(outbound_tx);
        drop(inbound_tx);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let snap = routing.snapshot();
                if snap.pending == 0 && snap.pipes == 0 {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("pipe close must evict both pending and pipe entries");

        // The pipe id is non-reusable regardless — invariant from step
        // 1 — and the routable pool stays empty too (no hook fired).
        assert_eq!(routing.snapshot().routable, 0);
        let _ = pipe_id; // just a reference for future debugging
    }

    #[tokio::test]
    async fn shadow_routing_mints_and_evicts_around_pipe_lifetime() {
        // Step 1 invariant: every StartDataPipe produces exactly one
        // shadow-routing pipe registration, and pipe teardown evicts it.
        // This is the end-to-end version of the routing unit tests —
        // drives the registration via the real Driver code path so that
        // future refactors of the spawn site can't silently drop the
        // mint/evict symmetry.
        use crate::transport::peer::{ConnectPath, ConnectRole};

        let iface = Arc::new(MockBleInterface::new());
        let (tx, mut rx) = mpsc::channel(16);
        let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(4);
        let routing = Arc::new(crate::transport::routing::Routing::new());
        let driver = Driver::new(
            iface,
            tx,
            incoming_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(crate::transport::store::InMemoryPeerStore::new()),
            Arc::clone(&routing),
        );

        assert_eq!(routing.snapshot().pipes, 0);

        driver
            .execute(PeerAction::StartDataPipe {
                device_id: blew::DeviceId::from("shadow-peer"),
                lifecycle_id: 1,
                tx_gen: 1,
                role: ConnectRole::Central,
                target_endpoint: None,
                path: ConnectPath::Gatt,
                l2cap_channel: None,
            })
            .await;

        // Mint is synchronous inside execute(), so the count should be 1
        // before the DataPipeReady command arrives.
        assert_eq!(
            routing.snapshot().pipes,
            1,
            "StartDataPipe must register exactly one shadow pipe"
        );

        // Capture the DataPipeReady so we can drop its senders to end the
        // pipe worker.
        let cmd = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let (outbound_tx, inbound_tx) = match cmd {
            PeerCommand::DataPipeReady {
                outbound_tx,
                inbound_tx,
                ..
            } => (outbound_tx, inbound_tx),
            other => panic!("expected DataPipeReady, got {other:?}"),
        };

        // Pipe worker exits when both outbound and inbound channels close.
        // Dropping the senders here closes them; supervisor then exits its
        // select loop and run_data_pipe returns, firing evict_pipe.
        drop(outbound_tx);
        drop(inbound_tx);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if routing.snapshot().pipes == 0 {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("shadow pipe must be evicted once worker exits");
    }

    #[tokio::test]
    async fn shadow_routing_tracks_direction_from_role() {
        use crate::transport::peer::{ConnectPath, ConnectRole};
        use crate::transport::routing::Direction;

        let iface = Arc::new(MockBleInterface::new());
        let (tx, mut rx) = mpsc::channel(16);
        let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(4);
        let routing = Arc::new(crate::transport::routing::Routing::new());
        let driver = Driver::new(
            iface,
            tx,
            incoming_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(crate::transport::store::InMemoryPeerStore::new()),
            Arc::clone(&routing),
        );

        driver
            .execute(PeerAction::StartDataPipe {
                device_id: blew::DeviceId::from("central-peer"),
                lifecycle_id: 1,
                tx_gen: 1,
                role: ConnectRole::Central,
                target_endpoint: None,
                path: ConnectPath::Gatt,
                l2cap_channel: None,
            })
            .await;
        driver
            .execute(PeerAction::StartDataPipe {
                device_id: blew::DeviceId::from("peripheral-peer"),
                lifecycle_id: 1,
                tx_gen: 1,
                role: ConnectRole::Peripheral,
                target_endpoint: None,
                path: ConnectPath::Gatt,
                l2cap_channel: None,
            })
            .await;

        let mut pipes = routing.pipes_for_debug();
        pipes.sort_by_key(|p| p.device_id.to_string());
        assert_eq!(pipes.len(), 2);
        // "central-peer" < "peripheral-peer" lexicographically.
        assert_eq!(pipes[0].device_id, blew::DeviceId::from("central-peer"));
        assert_eq!(
            pipes[0].direction,
            Direction::Outbound,
            "Central role → Outbound"
        );
        assert_eq!(pipes[1].device_id, blew::DeviceId::from("peripheral-peer"));
        assert_eq!(
            pipes[1].direction,
            Direction::Inbound,
            "Peripheral role → Inbound"
        );

        // Drain both DataPipeReady commands so rx is clean for other tests.
        for _ in 0..2 {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await;
        }
    }

    #[tokio::test]
    async fn upgrade_to_l2cap_reads_psm_and_emits_open_l2cap_succeeded() {
        let iface = Arc::new(MockBleInterface::new());
        let device_id = blew::DeviceId::from("upgrade");
        let psm = 0x0080u16;
        iface.seed_psm(Some(psm));
        let (chan, _other) = blew::L2capChannel::pair(1024);
        iface.on_open_l2cap(device_id.clone(), psm, Ok(chan));

        let (tx, mut rx) = mpsc::channel(16);
        let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(4);
        let driver = Driver::new(
            iface,
            tx,
            incoming_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(crate::transport::store::InMemoryPeerStore::new()),
            Arc::new(crate::transport::routing::Routing::new()),
        );

        driver.activate(&device_id, 1);
        driver
            .execute(PeerAction::UpgradeToL2cap {
                lifecycle_id: 1,
                device_id: device_id.clone(),
                upgrade_gen: 1,
            })
            .await;

        let cmd = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match cmd {
            PeerCommand::OpenL2capSucceeded { device_id: got, .. } => {
                assert_eq!(got, device_id);
            }
            other => panic!("expected OpenL2capSucceeded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn swap_pipe_to_l2cap_sends_channel_to_swap_tx() {
        let iface = Arc::new(MockBleInterface::new());
        let (tx, _rx) = mpsc::channel(16);
        let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(4);
        let driver = Driver::new(
            iface,
            tx,
            incoming_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(crate::transport::store::InMemoryPeerStore::new()),
            Arc::new(crate::transport::routing::Routing::new()),
        );

        let (swap_tx, mut swap_rx) = mpsc::channel::<blew::L2capChannel>(1);
        let (chan, _other) = blew::L2capChannel::pair(1024);
        driver
            .execute(PeerAction::SwapPipeToL2cap {
                device_id: blew::DeviceId::from("swap-dev"),
                channel: chan,
                swap_tx,
            })
            .await;

        let received = tokio::time::timeout(std::time::Duration::from_secs(1), swap_rx.recv())
            .await
            .expect("timed out waiting for channel on swap_rx")
            .expect("swap_rx closed unexpectedly");
        drop(received);
    }

    #[tokio::test]
    async fn start_connect_spawns_connect_and_forwards_success() {
        let iface = Arc::new(MockBleInterface::new());
        let (tx, mut rx) = mpsc::channel(16);
        let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(1);
        let driver = Driver::new(
            iface.clone(),
            tx,
            incoming_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(crate::transport::store::InMemoryPeerStore::new()),
            Arc::new(crate::transport::routing::Routing::new()),
        );
        let device_id = blew::DeviceId::from("x");
        driver
            .execute(PeerAction::StartConnect {
                device_id: device_id.clone(),
                attempt: 0,
                lifecycle_id: 1,
            })
            .await;
        let cmd = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(cmd, PeerCommand::ConnectSucceeded { .. }));
        iface.assert_called(&CallKind::Connect(device_id));
    }

    fn test_driver(
        iface: Arc<MockBleInterface>,
    ) -> (Driver<MockBleInterface>, mpsc::Receiver<PeerCommand>) {
        let (tx, rx) = mpsc::channel(16);
        let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(4);
        let driver = Driver::new(
            iface,
            tx,
            incoming_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(crate::transport::store::InMemoryPeerStore::new()),
            Arc::new(crate::transport::routing::Routing::new()),
        );
        (driver, rx)
    }

    async fn next_command(rx: &mut mpsc::Receiver<PeerCommand>) -> PeerCommand {
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for a command")
            .expect("inbox closed")
    }

    /// Both operations name the same native connection, so a teardown that is
    /// still in flight when the replacement dial is dispatched must finish
    /// first — otherwise the platform sees a connect and a disconnect for one
    /// device at once, and whichever lands last wins.
    #[tokio::test]
    async fn teardown_completes_before_the_replacement_dial_touches_the_device() {
        let iface = Arc::new(MockBleInterface::new());
        let device_id = blew::DeviceId::from("serialize-dev");
        let (driver, mut rx) = test_driver(Arc::clone(&iface));

        driver
            .execute(PeerAction::StartConnect {
                device_id: device_id.clone(),
                attempt: 0,
                lifecycle_id: 1,
            })
            .await;
        assert!(matches!(
            next_command(&mut rx).await,
            PeerCommand::ConnectSucceeded { .. }
        ));

        iface.hold_disconnect();
        driver
            .execute(PeerAction::CloseChannel {
                lifecycle_id: 1,
                device_id: device_id.clone(),
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
                reason: crate::transport::peer::DisconnectReason::LinkLoss,
            })
            .await;
        // The replacement dial is dispatched while the teardown is still open.
        driver
            .execute(PeerAction::StartConnect {
                device_id: device_id.clone(),
                attempt: 1,
                lifecycle_id: 2,
            })
            .await;

        let disconnected = |calls: &[CallKind]| {
            calls
                .iter()
                .filter(|c| matches!(c, CallKind::Disconnect(_)))
                .count()
        };
        let connected = |calls: &[CallKind]| {
            calls
                .iter()
                .filter(|c| matches!(c, CallKind::Connect(_)))
                .count()
        };
        for _ in 0..50 {
            if disconnected(&iface.calls()) == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            disconnected(&iface.calls()),
            1,
            "teardown must have started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            connected(&iface.calls()),
            1,
            "the replacement dial must wait for the teardown to finish"
        );

        iface.release_disconnect();
        let cmd = next_command(&mut rx).await;
        match cmd {
            PeerCommand::ConnectSucceeded { lifecycle_id, .. } => assert_eq!(lifecycle_id, 2),
            other => panic!("expected the replacement's ConnectSucceeded, got {other:?}"),
        }
        assert_eq!(connected(&iface.calls()), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn hung_cleanup_expires_before_the_replacement_dial_runs() {
        for refresh in [false, true] {
            let iface = Arc::new(MockBleInterface::new());
            let device_id = blew::DeviceId::from("hung-cleanup");
            let (driver, mut rx) = test_driver(Arc::clone(&iface));
            driver
                .execute(PeerAction::StartConnect {
                    device_id: device_id.clone(),
                    attempt: 0,
                    lifecycle_id: 1,
                })
                .await;
            assert!(matches!(
                next_command(&mut rx).await,
                PeerCommand::ConnectSucceeded { .. }
            ));

            let action = if refresh {
                iface.set_refresh_held(true);
                PeerAction::Refresh {
                    device_id: device_id.clone(),
                    lifecycle_id: 1,
                }
            } else {
                iface.hold_disconnect();
                PeerAction::CloseChannel {
                    device_id: device_id.clone(),
                    lifecycle_id: 1,
                    channel: ChannelHandle {
                        id: 1,
                        path: ConnectPath::Gatt,
                    },
                    reason: crate::transport::peer::DisconnectReason::LocalClose,
                }
            };
            driver.execute(action).await;
            // The worker has entered the backend wait before the clock advances.
            for _ in 0..100 {
                if iface.cleanup_waiters() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert_eq!(iface.cleanup_waiters(), 1);
            driver
                .execute(PeerAction::RetireLifecycle {
                    device_id: device_id.clone(),
                    lifecycle_id: 1,
                })
                .await;
            driver
                .execute(PeerAction::StartConnect {
                    device_id: device_id.clone(),
                    attempt: 1,
                    lifecycle_id: 2,
                })
                .await;

            tokio::time::advance(CLEANUP_TIMEOUT - std::time::Duration::from_secs(1)).await;
            assert!(rx.try_recv().is_err());
            assert_eq!(
                iface
                    .calls()
                    .iter()
                    .filter(|c| matches!(c, CallKind::Connect(_)))
                    .count(),
                1
            );
            assert_eq!(
                iface.cleanup_waiters(),
                1,
                "retirement must not cancel teardown early"
            );
            tokio::time::advance(std::time::Duration::from_secs(2)).await;
            assert!(matches!(
                next_command(&mut rx).await,
                PeerCommand::ConnectSucceeded {
                    lifecycle_id: 2,
                    ..
                }
            ));
            assert_eq!(
                iface.cleanup_waiters(),
                0,
                "the timed-out future must be dropped, not detached"
            );

            // Releasing the old backend waiter must not resume a Rust cleanup
            // continuation against lifecycle 2. The replacement can still work.
            iface.release_disconnect();
            iface.set_refresh_held(false);
            iface.seed_version(Some(
                crate::transport::transport::PROTOCOL_VERSION.wrapping_add(1),
            ));
            driver
                .execute(PeerAction::ReadVersion {
                    device_id: device_id.clone(),
                    lifecycle_id: 2,
                })
                .await;
            assert!(matches!(
                next_command(&mut rx).await,
                PeerCommand::ProtocolVersionMismatch {
                    lifecycle_id: 2,
                    ..
                }
            ));
            assert_eq!(
                iface
                    .calls()
                    .iter()
                    .filter(|c| matches!(c, CallKind::Connect(_)))
                    .count(),
                2
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_error_does_not_block_the_replacement_dial() {
        let iface = Arc::new(MockBleInterface::new());
        let device_id = blew::DeviceId::from("failed-cleanup");
        iface.on_disconnect(device_id.clone(), Err(crate::error::BleError::NotConnected));
        let (driver, mut rx) = test_driver(Arc::clone(&iface));
        driver
            .execute(PeerAction::StartConnect {
                device_id: device_id.clone(),
                attempt: 0,
                lifecycle_id: 1,
            })
            .await;
        let _ = next_command(&mut rx).await;
        driver
            .execute(PeerAction::CloseChannel {
                device_id: device_id.clone(),
                lifecycle_id: 1,
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
                reason: crate::transport::peer::DisconnectReason::LocalClose,
            })
            .await;
        driver
            .execute(PeerAction::RetireLifecycle {
                device_id: device_id.clone(),
                lifecycle_id: 1,
            })
            .await;
        driver
            .execute(PeerAction::StartConnect {
                device_id: device_id.clone(),
                attempt: 1,
                lifecycle_id: 2,
            })
            .await;
        assert!(matches!(
            next_command(&mut rx).await,
            PeerCommand::ConnectSucceeded {
                lifecycle_id: 2,
                ..
            }
        ));
        iface.assert_called(&CallKind::Disconnect(device_id));
    }

    #[tokio::test(start_paused = true)]
    async fn gatt_setup_deadline_covers_discovery_and_subscription_together() {
        let started = tokio::time::Instant::now();
        let result = bounded_gatt_setup(async {
            // Discovery consumes most of the shared deadline.
            tokio::time::sleep(GATT_SETUP_TIMEOUT - std::time::Duration::from_secs(1)).await;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            Ok(())
        })
        .await;
        assert!(matches!(
            result,
            Err(crate::error::BleError::Timeout {
                stage: "GATT setup"
            })
        ));
        assert_eq!(started.elapsed(), GATT_SETUP_TIMEOUT);

        let result = bounded_gatt_setup(std::future::pending()).await;
        assert!(matches!(
            result,
            Err(crate::error::BleError::Timeout {
                stage: "GATT setup"
            })
        ));
        let result = bounded_gatt_setup(async { Err(crate::error::BleError::NotConnected) }).await;
        assert!(matches!(result, Err(crate::error::BleError::NotConnected)));
    }

    /// Announce a lifecycle through the same action as a registry-created pipe.
    async fn announce_channel(
        driver: &Driver<MockBleInterface>,
        device_id: &blew::DeviceId,
        lifecycle_id: u64,
        rx: &mut mpsc::Receiver<PeerCommand>,
    ) {
        driver
            .execute(PeerAction::StartDataPipe {
                device_id: device_id.clone(),
                lifecycle_id,
                tx_gen: lifecycle_id,
                role: crate::transport::peer::ConnectRole::Central,
                target_endpoint: None,
                path: ConnectPath::Gatt,
                l2cap_channel: None,
            })
            .await;
        assert!(matches!(
            next_command(rx).await,
            PeerCommand::DataPipeReady { .. }
        ));
    }

    fn disconnects(iface: &MockBleInterface) -> usize {
        iface
            .calls()
            .iter()
            .filter(|c| matches!(c, CallKind::Disconnect(_)))
            .count()
    }

    /// `disconnect` is addressed by device, not by channel, so a close naming
    /// a channel the device has already replaced would take the replacement
    /// down with it.
    #[tokio::test]
    async fn closing_a_replaced_channel_leaves_the_replacement_connected() {
        let iface = Arc::new(MockBleInterface::new());
        let device_id = blew::DeviceId::from("replaced-chan-dev");
        let (driver, mut rx) = test_driver(Arc::clone(&iface));

        announce_channel(&driver, &device_id, 1, &mut rx).await;
        announce_channel(&driver, &device_id, 2, &mut rx).await;

        // A close for the first channel arrives late.
        driver
            .execute(PeerAction::CloseChannel {
                lifecycle_id: 1,
                device_id: device_id.clone(),
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
                reason: crate::transport::peer::DisconnectReason::LinkLoss,
            })
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            disconnects(&iface),
            0,
            "a stale close must not disconnect the device; got {:?}",
            iface.calls()
        );

        // The live channel still closes normally.
        driver
            .execute(PeerAction::CloseChannel {
                lifecycle_id: 2,
                device_id: device_id.clone(),
                channel: ChannelHandle {
                    id: 2,
                    path: ConnectPath::Gatt,
                },
                reason: crate::transport::peer::DisconnectReason::LinkLoss,
            })
            .await;
        for _ in 0..50 {
            if disconnects(&iface) == 1 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the live channel's close never reached the interface");
    }

    /// A close we are going to skip must not retire the live channel's work
    /// on its way past: a cancelled upgrade reports nothing back, so the
    /// registry would sit in `Connected { upgrading: true }` for good.
    #[tokio::test]
    async fn a_stale_close_does_not_cancel_the_live_channels_upgrade() {
        let iface = Arc::new(MockBleInterface::new());
        let device_id = blew::DeviceId::from("stale-close-upgrade-dev");
        let psm = 0x0080u16;
        iface.seed_psm(Some(psm));
        let (chan, _other) = blew::L2capChannel::pair(1024);
        iface.on_open_l2cap(device_id.clone(), psm, Ok(chan));
        // Keep the lane busy so the upgrade is still queued when the stale
        // close is dispatched.
        iface.set_connect_delay(std::time::Duration::from_millis(200));
        let (driver, mut rx) = test_driver(Arc::clone(&iface));

        announce_channel(&driver, &device_id, 2, &mut rx).await;
        driver
            .execute(PeerAction::StartConnect {
                device_id: device_id.clone(),
                attempt: 0,
                lifecycle_id: 2,
            })
            .await;
        driver
            .execute(PeerAction::UpgradeToL2cap {
                lifecycle_id: 2,
                device_id: device_id.clone(),
                upgrade_gen: 1,
            })
            .await;
        driver
            .execute(PeerAction::CloseChannel {
                lifecycle_id: 1,
                device_id: device_id.clone(),
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
                reason: crate::transport::peer::DisconnectReason::LinkLoss,
            })
            .await;

        loop {
            match next_command(&mut rx).await {
                PeerCommand::OpenL2capSucceeded { upgrade_gen, .. } => {
                    assert_eq!(upgrade_gen, 1);
                    break;
                }
                PeerCommand::OpenL2capFailed { error, .. } => {
                    panic!("upgrade should have succeeded, got {error}")
                }
                _ => {}
            }
        }
        assert_eq!(
            disconnects(&iface),
            0,
            "the stale close must not have torn the device down"
        );
    }
}
