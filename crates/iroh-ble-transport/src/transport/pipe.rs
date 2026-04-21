//! Per-peer data pipe task. A supervisor owns `outbound_rx` and `inbound_rx`,
//! forwarding each item to whichever worker (GATT or L2CAP) is currently
//! active. On `swap_rx.recv()`, the supervisor spawns the L2CAP worker,
//! drops the GATT worker's forwarding senders (so it flushes and exits), and
//! schedules a delayed abort after `L2CAP_HANDOVER_TIMEOUT` to bound the
//! drain tail.
//!
//! ## Handover asymmetry (hardware-test bug fix)
//!
//! The two sides of a pair do NOT swap to L2CAP simultaneously. The
//! peripheral accepts the incoming L2CAP the moment blew sees the
//! channel; the central switches only after `open_l2cap_channel`
//! returns. That gap is typically <500 ms but observed at ~270 ms with
//! Phone/Desktop pairings in the first hardware test. If QUIC's TLS
//! handshake is still in flight during this window, the central
//! transmits `ClientFinished` over its (still-GATT) pipe while the
//! peripheral has already swapped. The peripheral's supervisor would
//! drop those GATT fragments and TLS never completes on the
//! server side, surfacing as `Accepting incoming connection ended with
//! error: timed out` after iroh's 15 s accept deadline.
//!
//! Fix: `ActiveWorker::L2cap` carries an optional `GattDrain` for
//! `GATT_DRAIN_DEADLINE` after swap. During the drain, inbound GATT
//! fragments still feed the old worker's `ReliableChannel` so they
//! reassemble into datagrams and land on `incoming_tx` (the same sink
//! as L2CAP data). Outbound always goes via the new L2CAP worker. The
//! drain self-retires on deadline or on supervisor exit.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::{JoinError, JoinHandle};
use tokio::time::Duration;

/// Wraps a `JoinHandle` so the inner task is aborted (not just detached) when
/// the wrapper is dropped — including when the owning task is cancelled mid
/// `select!`. Used to keep child tasks like the GATT send loop from outliving
/// their parent worker after a supervisor swap aborts the worker.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl<T> std::future::Future for AbortOnDrop<T> {
    type Output = Result<T, JoinError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

use crate::transport::dedup::L2CAP_HANDOVER_TIMEOUT;
use crate::transport::driver::IncomingPacket;
use crate::transport::interface::BleInterface;
use crate::transport::mtu::{ATT_OVERHEAD, MIN_SANE_MTU, resolve_chunk_size};
use crate::transport::peer::{ConnectPath, ConnectRole, LivenessClock, PeerCommand, PendingSend};
use crate::transport::reliable::ReliableChannel;

/// Conservative initial chunk size for a freshly started GATT pipe, used
/// while the async MTU resolver runs in parallel. Sized to the BLE-spec
/// default ATT MTU floor (`MIN_SANE_MTU` = 24) minus ATT overhead so any
/// fragments sent before the resolver lands are safe on any peer. The
/// resolver calls `ReliableChannel::set_chunk_size` to bump this up.
const INITIAL_CHUNK_SIZE: usize = (MIN_SANE_MTU as usize) - ATT_OVERHEAD;

/// How long to keep the old GATT worker's receive path alive after a
/// swap to L2CAP. Covers the window where the peer is still writing
/// over GATT because its own L2CAP swap hasn't completed yet —
/// typically sub-second, but hardware-observed up to ~300 ms with a
/// conservative margin for slower radios and busy schedulers. Bounded
/// so a stuck drain can't hold the pipe open forever.
const GATT_DRAIN_DEADLINE: Duration = Duration::from_secs(3);

/// Residual state for the old GATT worker during a peripheral-side
/// L2CAP handover. Keeps the GATT worker's inbound reassembly path
/// alive (not its outbound path — outbound goes to the new L2CAP
/// worker) so in-flight QUIC/TLS fragments from a peer whose own
/// swap hasn't completed still deliver cleanly. See the module
/// docstring for the full failure mode.
struct GattDrain {
    /// Kept alive but unused during the drain. The GATT worker's
    /// select loop exits on `outbound_rx.recv() == None`, so the
    /// channel must stay open until the drain is retired; only then
    /// do we drop it to release the worker cleanly.
    _outbound_fwd_tx: mpsc::Sender<PendingSend>,
    /// Forward inbound GATT fragments here during the drain window.
    inbound_fwd_tx: mpsc::Sender<Bytes>,
    /// Signals the GATT worker to tear down on drain retirement.
    shutdown_tx: Option<oneshot::Sender<()>>,
    /// Lets the GATT worker's send loop notice teardown between ACK
    /// waits without needing the shutdown oneshot to arrive mid-wait.
    teardown_flag: Arc<AtomicBool>,
    /// JoinHandle for the GATT supervisor/worker task. Awaited bounded
    /// at retirement so the shutdown cascade doesn't leak.
    handle: JoinHandle<()>,
    /// Hard cap on drain lifetime regardless of activity. Retired when
    /// `Instant::now() >= deadline`.
    deadline: tokio::time::Instant,
}

enum ActiveWorker {
    Gatt {
        outbound_fwd_tx: mpsc::Sender<PendingSend>,
        inbound_fwd_tx: mpsc::Sender<Bytes>,
        shutdown_tx: oneshot::Sender<()>,
        teardown_flag: Arc<AtomicBool>,
        handle: JoinHandle<()>,
    },
    L2cap {
        outbound_fwd_tx: mpsc::Sender<PendingSend>,
        teardown_flag: Arc<AtomicBool>,
        handle: JoinHandle<()>,
        /// Optional drain of the old GATT worker during handover. See
        /// `GattDrain`. `None` after the drain retires.
        gatt_drain: Option<GattDrain>,
    },
}

impl ActiveWorker {
    fn is_l2cap(&self) -> bool {
        matches!(self, ActiveWorker::L2cap { .. })
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_data_pipe(
    iface: Arc<dyn BleInterface>,
    device_id: blew::DeviceId,
    stable_conn_id: crate::transport::routing_v2::StableConnId,
    role: ConnectRole,
    initial_path: ConnectPath,
    initial_l2cap: Option<blew::L2capChannel>,
    outbound_rx: mpsc::Receiver<PendingSend>,
    inbound_rx: mpsc::Receiver<Bytes>,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    registry_tx: mpsc::Sender<PeerCommand>,
    swap_rx: mpsc::Receiver<blew::L2capChannel>,
    retransmit_counter: Arc<AtomicU64>,
    truncation_counter: Arc<AtomicU64>,
    empty_frames_counter: Arc<AtomicU64>,
    last_rx_at: LivenessClock,
) {
    run_pipe_supervisor(
        iface,
        device_id,
        stable_conn_id,
        role,
        initial_path,
        initial_l2cap,
        outbound_rx,
        inbound_rx,
        incoming_tx,
        registry_tx,
        swap_rx,
        retransmit_counter,
        truncation_counter,
        empty_frames_counter,
        last_rx_at,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn run_pipe_supervisor(
    iface: Arc<dyn BleInterface>,
    device_id: blew::DeviceId,
    stable_conn_id: crate::transport::routing_v2::StableConnId,
    role: ConnectRole,
    initial_path: ConnectPath,
    initial_l2cap: Option<blew::L2capChannel>,
    mut outbound_rx: mpsc::Receiver<PendingSend>,
    mut inbound_rx: mpsc::Receiver<Bytes>,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    registry_tx: mpsc::Sender<PeerCommand>,
    swap_rx: mpsc::Receiver<blew::L2capChannel>,
    retransmit_counter: Arc<AtomicU64>,
    truncation_counter: Arc<AtomicU64>,
    empty_frames_counter: Arc<AtomicU64>,
    last_rx_at: LivenessClock,
) {
    let mut swap_rx: Option<mpsc::Receiver<blew::L2capChannel>> = Some(swap_rx);
    let mut active = match initial_path {
        ConnectPath::Gatt => spawn_gatt_worker(
            Arc::clone(&iface),
            device_id.clone(),
            stable_conn_id,
            role,
            incoming_tx.clone(),
            registry_tx.clone(),
            Arc::clone(&retransmit_counter),
            Arc::clone(&truncation_counter),
            last_rx_at.clone(),
        ),
        ConnectPath::L2cap => {
            let Some(channel) = initial_l2cap else {
                tracing::error!(device = %device_id, "StartDataPipe(L2cap) without channel");
                return;
            };
            spawn_l2cap_worker(
                device_id.clone(),
                stable_conn_id,
                channel,
                incoming_tx.clone(),
                registry_tx.clone(),
                Arc::clone(&empty_frames_counter),
                last_rx_at.clone(),
            )
        }
    };

    let mut l2cap_timeout_reported = false;

    loop {
        tokio::select! {
            maybe_send = outbound_rx.recv() => {
                let Some(send) = maybe_send else { break; };
                match forward_outbound(&active, send, &device_id, &registry_tx, &mut l2cap_timeout_reported).await {
                    ForwardResult::Ok => {}
                    ForwardResult::WorkerGone => break,
                    ForwardResult::L2capTimeout => break,
                }
            }
            maybe_bytes = inbound_rx.recv() => {
                let Some(bytes) = maybe_bytes else { break; };
                match &mut active {
                    ActiveWorker::Gatt { inbound_fwd_tx, .. } => {
                        if inbound_fwd_tx.send(bytes).await.is_err() {
                            break;
                        }
                    }
                    // Post-swap: L2CAP reads from its own channel directly,
                    // but the peer may still be writing over GATT while its
                    // own swap completes. Route these late fragments to the
                    // drain's GATT worker so its ReliableChannel can
                    // reassemble them into QUIC datagrams — otherwise TLS
                    // handshake packets that land in this window are lost
                    // and the server-side accept times out after 15 s.
                    ActiveWorker::L2cap { gatt_drain: Some(drain), .. } => {
                        if drain.inbound_fwd_tx.try_send(bytes).is_err() {
                            // Drain's GATT worker exited early (channel
                            // full or closed). Fragment is dropped; the
                            // peer's ReliableChannel will retransmit.
                            tracing::debug!(
                                device = %device_id,
                                "l2cap drain: dropping fragment, gatt worker gone or saturated"
                            );
                        }
                    }
                    ActiveWorker::L2cap { gatt_drain: None, .. } => {
                        // Drain already retired; fragments past this point
                        // cannot be reassembled (no ReliableChannel to
                        // reassemble into). If they matter, the peer's
                        // QUIC layer will retransmit via L2CAP.
                        tracing::debug!(
                            device = %device_id,
                            "post-drain: dropping late GATT fragment (drain retired)"
                        );
                    }
                }
            }
            () = wait_drain_deadline(&active) => {
                if let ActiveWorker::L2cap { gatt_drain, .. } = &mut active
                    && let Some(drain) = gatt_drain.take()
                {
                    tracing::debug!(
                        device = %device_id,
                        "l2cap drain: deadline reached, retiring gatt worker"
                    );
                    retire_gatt_drain(drain, device_id.clone(), L2CAP_HANDOVER_TIMEOUT);
                }
            }
            maybe_chan = recv_swap(&mut swap_rx) => {
                let Some(channel) = maybe_chan else {
                    // swap_tx was dropped; disable the swap arm permanently so
                    // the select loop does not busy-poll on a closed recv.
                    swap_rx = None;
                    continue;
                };
                if active.is_l2cap() {
                    tracing::debug!(
                        device = %device_id,
                        "ignoring redundant L2CAP swap request; already on L2CAP"
                    );
                    continue;
                }
                let new_active = spawn_l2cap_worker(
                    device_id.clone(),
                    stable_conn_id,
                    channel,
                    incoming_tx.clone(),
                    registry_tx.clone(),
                    Arc::clone(&empty_frames_counter),
                    last_rx_at.clone(),
                );
                let old = std::mem::replace(&mut active, new_active);
                let drain = gatt_drain_from_old(old);
                if let (Some(drain), ActiveWorker::L2cap { gatt_drain, .. }) =
                    (drain, &mut active)
                {
                    tracing::debug!(
                        device = %device_id,
                        old_path = "Gatt",
                        new_path = "L2cap",
                        drain_ms = GATT_DRAIN_DEADLINE.as_millis(),
                        "swapped to L2CAP; keeping GATT worker alive for drain window"
                    );
                    *gatt_drain = Some(drain);
                }
            }
        }
    }

    // Supervisor exiting: drop the forwarding senders so the active worker
    // observes outbound/inbound EOF and tears itself down, then wait briefly
    // for it to exit (so its send sub-tasks and any outstanding ACK flushes
    // are joined) before returning. The join is bounded so a wedged worker
    // cannot hold the caller hostage — on timeout we abort the task rather
    // than letting the JoinHandle drop (which would only detach it).
    let mut handle = match active {
        ActiveWorker::Gatt {
            teardown_flag,
            handle,
            ..
        } => {
            teardown_flag.store(true, Ordering::Relaxed);
            handle
        }
        ActiveWorker::L2cap {
            teardown_flag,
            handle,
            gatt_drain,
            ..
        } => {
            if let Some(drain) = gatt_drain {
                retire_gatt_drain(drain, device_id.clone(), L2CAP_HANDOVER_TIMEOUT);
            }
            teardown_flag.store(true, Ordering::Relaxed);
            handle
        }
    };
    if tokio::time::timeout(L2CAP_HANDOVER_TIMEOUT, &mut handle)
        .await
        .is_err()
    {
        handle.abort();
        tracing::debug!(
            device = %device_id,
            "pipe supervisor: worker did not exit within handover timeout; aborted during teardown"
        );
    }
}

/// Resolves when the L2cap drain's deadline elapses. If the active
/// worker is `Gatt` or the drain is already `None`, pends forever so
/// the supervisor's `select!` arm is effectively disabled.
async fn wait_drain_deadline(active: &ActiveWorker) {
    match active {
        ActiveWorker::L2cap {
            gatt_drain: Some(drain),
            ..
        } => tokio::time::sleep_until(drain.deadline).await,
        _ => std::future::pending().await,
    }
}

/// Convert an old `ActiveWorker::Gatt` into a `GattDrain`. Keeps the
/// outbound forward sender alive so the GATT worker's select loop
/// does not exit on a closed outbound channel — the supervisor
/// simply stops pushing new sends through it. Inbound reassembly and
/// ACK plumbing stay live for the drain window so late fragments
/// from a peer whose own L2CAP swap hasn't landed still deliver.
/// Returns `None` if the input was already an L2cap worker
/// (defensive; the swap path only fires from Gatt).
fn gatt_drain_from_old(old: ActiveWorker) -> Option<GattDrain> {
    match old {
        ActiveWorker::Gatt {
            outbound_fwd_tx,
            inbound_fwd_tx,
            shutdown_tx,
            teardown_flag,
            handle,
        } => Some(GattDrain {
            _outbound_fwd_tx: outbound_fwd_tx,
            inbound_fwd_tx,
            shutdown_tx: Some(shutdown_tx),
            teardown_flag,
            handle,
            deadline: tokio::time::Instant::now() + GATT_DRAIN_DEADLINE,
        }),
        ActiveWorker::L2cap { .. } => None,
    }
}

/// Tear down a GATT drain: signal shutdown, set teardown flag, and
/// wait bounded for the worker to join. On deadline, abort.
fn retire_gatt_drain(drain: GattDrain, device_id: blew::DeviceId, timeout: Duration) {
    tokio::spawn(async move {
        let GattDrain {
            _outbound_fwd_tx,
            inbound_fwd_tx,
            mut shutdown_tx,
            teardown_flag,
            mut handle,
            ..
        } = drain;
        // Drop both forward senders so the GATT worker's select loop
        // observes outbound and inbound EOF; it breaks out and tears
        // down. `_outbound_fwd_tx` was held only for this moment.
        drop(_outbound_fwd_tx);
        drop(inbound_fwd_tx);
        teardown_flag.store(true, Ordering::Relaxed);
        if let Some(s) = shutdown_tx.take() {
            let _ = s.send(());
        }
        match tokio::time::timeout(timeout, &mut handle).await {
            Ok(Ok(())) => {
                tracing::debug!(
                    device = %device_id,
                    "gatt drain retired cleanly"
                );
            }
            Ok(Err(join_err)) => {
                tracing::debug!(
                    device = %device_id,
                    ?join_err,
                    "gatt drain worker join error"
                );
            }
            Err(_elapsed) => {
                handle.abort();
                tracing::debug!(
                    device = %device_id,
                    "gatt drain did not retire within timeout; aborted"
                );
            }
        }
    });
}

/// Poll the optional swap receiver. If `None`, never resolves — lets the
/// supervisor's `select!` ignore the arm without busy-looping.
async fn recv_swap(
    rx: &mut Option<mpsc::Receiver<blew::L2capChannel>>,
) -> Option<blew::L2capChannel> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

enum ForwardResult {
    Ok,
    WorkerGone,
    L2capTimeout,
}

async fn forward_outbound(
    active: &ActiveWorker,
    send: PendingSend,
    device_id: &blew::DeviceId,
    registry_tx: &mpsc::Sender<PeerCommand>,
    l2cap_timeout_reported: &mut bool,
) -> ForwardResult {
    match active {
        ActiveWorker::Gatt {
            outbound_fwd_tx, ..
        } => {
            if outbound_fwd_tx.send(send).await.is_err() {
                return ForwardResult::WorkerGone;
            }
            ForwardResult::Ok
        }
        ActiveWorker::L2cap {
            outbound_fwd_tx, ..
        } => match tokio::time::timeout(L2CAP_HANDOVER_TIMEOUT, outbound_fwd_tx.send(send)).await {
            Ok(Ok(())) => ForwardResult::Ok,
            Ok(Err(_closed)) => ForwardResult::WorkerGone,
            Err(_elapsed) => {
                if !*l2cap_timeout_reported {
                    *l2cap_timeout_reported = true;
                    let _ = registry_tx
                        .send(PeerCommand::L2capHandoverTimeout {
                            device_id: device_id.clone(),
                        })
                        .await;
                }
                ForwardResult::L2capTimeout
            }
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_gatt_worker(
    iface: Arc<dyn BleInterface>,
    device_id: blew::DeviceId,
    stable_conn_id: crate::transport::routing_v2::StableConnId,
    role: ConnectRole,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    registry_tx: mpsc::Sender<PeerCommand>,
    retransmit_counter: Arc<AtomicU64>,
    truncation_counter: Arc<AtomicU64>,
    last_rx_at: LivenessClock,
) -> ActiveWorker {
    let (outbound_fwd_tx, outbound_fwd_rx) = mpsc::channel::<PendingSend>(32);
    let (inbound_fwd_tx, inbound_fwd_rx) = mpsc::channel::<Bytes>(64);
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let teardown_flag = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(run_gatt_pipe(
        iface,
        device_id,
        stable_conn_id,
        role,
        outbound_fwd_rx,
        inbound_fwd_rx,
        shutdown_rx,
        Arc::clone(&teardown_flag),
        incoming_tx,
        registry_tx,
        retransmit_counter,
        truncation_counter,
        last_rx_at,
    ));
    ActiveWorker::Gatt {
        outbound_fwd_tx,
        inbound_fwd_tx,
        shutdown_tx,
        teardown_flag,
        handle,
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_l2cap_worker(
    device_id: blew::DeviceId,
    stable_conn_id: crate::transport::routing_v2::StableConnId,
    channel: blew::L2capChannel,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    registry_tx: mpsc::Sender<PeerCommand>,
    empty_frames_counter: Arc<AtomicU64>,
    last_rx_at: LivenessClock,
) -> ActiveWorker {
    let (outbound_fwd_tx, outbound_fwd_rx) = mpsc::channel::<PendingSend>(32);
    let teardown_flag = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(run_l2cap_pipe(
        device_id,
        stable_conn_id,
        channel,
        outbound_fwd_rx,
        Arc::clone(&teardown_flag),
        incoming_tx,
        registry_tx,
        empty_frames_counter,
        last_rx_at,
    ));
    ActiveWorker::L2cap {
        outbound_fwd_tx,
        teardown_flag,
        handle,
        gatt_drain: None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_gatt_pipe(
    iface: Arc<dyn BleInterface>,
    device_id: blew::DeviceId,
    stable_conn_id: crate::transport::routing_v2::StableConnId,
    role: ConnectRole,
    mut outbound_rx: mpsc::Receiver<PendingSend>,
    mut inbound_rx: mpsc::Receiver<Bytes>,
    mut shutdown_rx: oneshot::Receiver<()>,
    teardown_flag: Arc<AtomicBool>,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    registry_tx: mpsc::Sender<PeerCommand>,
    retransmit_counter: Arc<AtomicU64>,
    truncation_counter: Arc<AtomicU64>,
    last_rx_at: LivenessClock,
) {
    // Start with a conservative chunk size so the select loop can begin
    // processing inbound fragments immediately. Blocking on the MTU resolver
    // here would starve inbound reassembly for up to `MTU_READY_DEADLINE`
    // (≈3s) — enough to collide with an L2CAP accept landing mid-handshake.
    let (channel, mut datagram_rx) =
        ReliableChannel::new(INITIAL_CHUNK_SIZE, retransmit_counter, truncation_counter);
    let channel = Arc::new(channel);

    let resolver_handle = {
        let channel = Arc::clone(&channel);
        let iface = Arc::clone(&iface);
        let device_id = device_id.clone();
        tokio::spawn(async move {
            let chunk_size = resolve_chunk_size(iface.as_ref(), &device_id).await;
            channel.set_chunk_size(chunk_size);
        })
    };
    let _resolver_guard = AbortOnDrop(resolver_handle);

    let send_loop_handle = {
        let channel = Arc::clone(&channel);
        let iface = Arc::clone(&iface);
        let device_id = device_id.clone();
        let send_loop_teardown = Arc::clone(&teardown_flag);
        let span = tracing::info_span!("ble_pipe", device = %device_id);
        tokio::spawn(tracing::Instrument::instrument(
            async move {
                channel
                    .run_send_loop(
                        move |bytes| {
                            let iface = Arc::clone(&iface);
                            let device_id = device_id.clone();
                            let role = role;
                            async move {
                                let buf = Bytes::from(bytes);
                                let result = match role {
                                    ConnectRole::Central => iface.write_c2p(&device_id, buf).await,
                                    ConnectRole::Peripheral => {
                                        iface.notify_p2c(&device_id, buf).await
                                    }
                                };
                                result.map_err(|e| format!("{e}"))
                            }
                        },
                        {
                            let teardown_flag = Arc::clone(&send_loop_teardown);
                            move || teardown_flag.load(Ordering::Relaxed)
                        },
                    )
                    .await
            },
            span,
        ))
    };

    // If the supervisor aborts us mid-swap, the send loop is a separately
    // spawned task — dropping its JoinHandle would only detach it, leaving an
    // orphan that keeps retransmitting ghost fragments over the now-handed-off
    // channel for `LINK_DEAD_DEADLINE` (≈6 s). Guard it so cancellation here
    // also tears down the send loop.
    let send_loop_handle = AbortOnDrop(send_loop_handle);
    tokio::pin!(send_loop_handle);
    let mut link_dead = false;
    let mut send_loop_done = false;
    loop {
        tokio::select! {
            maybe_send = outbound_rx.recv() => {
                match maybe_send {
                    Some(send) => {
                        let _ = channel.enqueue_datagram(send.datagram.to_vec()).await;
                        send.waker.wake();
                    }
                    None => break,
                }
            }
            maybe_bytes = inbound_rx.recv() => {
                match maybe_bytes {
                    Some(bytes) => channel.receive_fragment(&bytes).await,
                    None => break,
                }
            }
            maybe_datagram = datagram_rx.recv() => {
                match maybe_datagram {
                    Some(data) => {
                        tracing::trace!(
                            device = %device_id,
                            len = data.len(),
                            "pipe reassembled datagram -> incoming_tx"
                        );
                        last_rx_at.bump();
                        let _ = incoming_tx
                            .send(IncomingPacket {
                                device_id: device_id.clone(),
                                stable_conn_id,
                                data: Bytes::from(data),
                            })
                            .await;
                    }
                    None => break,
                }
            }
            shutdown = &mut shutdown_rx => {
                let _ = shutdown;
                teardown_flag.store(true, Ordering::Relaxed);
                tracing::trace!(device = %device_id, "gatt pipe quiesce requested");
                break;
            }
            join = &mut send_loop_handle => {
                send_loop_done = true;
                if let Ok(Err(_link_dead)) = join {
                    link_dead = true;
                }
                break;
            }
        }
    }

    if !send_loop_done {
        channel.mark_dead().await;
        let _ = (&mut send_loop_handle).await;
    }

    if link_dead {
        let _ = registry_tx
            .send(PeerCommand::Stalled {
                device_id: device_id.clone(),
            })
            .await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_l2cap_pipe(
    device_id: blew::DeviceId,
    stable_conn_id: crate::transport::routing_v2::StableConnId,
    channel: blew::L2capChannel,
    mut outbound_rx: mpsc::Receiver<PendingSend>,
    teardown_flag: Arc<AtomicBool>,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    registry_tx: mpsc::Sender<PeerCommand>,
    empty_frames_counter: Arc<AtomicU64>,
    last_rx_at: LivenessClock,
) {
    let (reader, writer) = tokio::io::split(channel);

    let (l2cap_tx, _send_task, _recv_task, done) = crate::transport::l2cap::spawn_l2cap_io_tasks(
        reader,
        writer,
        device_id.clone(),
        stable_conn_id,
        incoming_tx,
        last_rx_at,
        Arc::clone(&teardown_flag),
        Arc::clone(&empty_frames_counter),
    );

    let mut io_died = false;
    loop {
        tokio::select! {
            maybe_send = outbound_rx.recv() => {
                match maybe_send {
                    Some(send) => {
                        let datagram = send.datagram.to_vec();
                        tracing::trace!(
                            device = %device_id,
                            tx_gen = send.tx_gen,
                            len = datagram.len(),
                            "l2cap pipe got outbound"
                        );
                        // Guard iroh's `socket.rs:575` div-by-zero panic on the
                        // peer: if we forward a zero-length Transmit onto the
                        // wire, the remote's poll_recv hands iroh `stride = 0`
                        // and it panics. Ack the send so the waker unblocks
                        // (iroh treats it as delivered), but skip the wire.
                        if datagram.is_empty() {
                            empty_frames_counter.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(
                                device = %device_id,
                                tx_gen = send.tx_gen,
                                "l2cap pipe dropping zero-length outbound datagram; not forwarding to peer"
                            );
                            send.waker.wake();
                            continue;
                        }
                        match l2cap_tx.send(datagram).await {
                            Ok(()) => {
                                tracing::trace!(
                                    device = %device_id,
                                    tx_gen = send.tx_gen,
                                    "l2cap pipe forwarded outbound to send task"
                                );
                            }
                            Err(_closed) => {
                                if teardown_flag.load(Ordering::Relaxed) {
                                    tracing::debug!(
                                        device = %device_id,
                                        "l2cap pipe: send task channel closed during teardown; stopping"
                                    );
                                } else {
                                    tracing::warn!(
                                        device = %device_id,
                                        "l2cap pipe: send task channel closed unexpectedly; stopping"
                                    );
                                }
                                send.waker.wake();
                                io_died = true;
                                break;
                            }
                        }
                        send.waker.wake();
                    }
                    None => break,
                }
            }
            _ = done.notified() => {
                io_died = true;
                break;
            }
        }
    }

    if io_died {
        let _ = registry_tx
            .send(PeerCommand::Stalled {
                device_id: device_id.clone(),
            })
            .await;
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;
    use crate::transport::peer::PendingSend;
    use crate::transport::test_util::{CallKind, MockBleInterface};
    use std::task::{RawWaker, RawWakerVTable, Waker};

    fn noop_waker() -> Waker {
        fn no_op(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
    }

    #[tokio::test]
    async fn outbound_datagram_reaches_iface_write_c2p() {
        let iface = Arc::new(MockBleInterface::new());
        let (outbound_tx, outbound_rx) = mpsc::channel::<PendingSend>(4);
        let (_inbound_tx, inbound_rx) = mpsc::channel::<Bytes>(4);
        let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(4);
        let (registry_tx, _registry_rx) = mpsc::channel::<PeerCommand>(4);
        let (_swap_tx, swap_rx) = mpsc::channel::<blew::L2capChannel>(1);

        let device_id = blew::DeviceId::from("pipe-central");
        tokio::spawn(run_data_pipe(
            iface.clone() as Arc<dyn BleInterface>,
            device_id.clone(),
            crate::transport::routing_v2::StableConnId::for_test(1),
            ConnectRole::Central,
            ConnectPath::Gatt,
            None,
            outbound_rx,
            inbound_rx,
            incoming_tx,
            registry_tx,
            swap_rx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            LivenessClock::new(),
        ));

        outbound_tx
            .send(PendingSend {
                tx_gen: 1,
                datagram: Bytes::from_static(b"hello-pipe"),
                waker: noop_waker(),
            })
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let calls = iface.calls();
                if calls.iter().any(|c| matches!(c, CallKind::WriteC2p { .. })) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("expected WriteC2p call");
    }

    #[tokio::test]
    async fn peripheral_role_uses_notify_p2c() {
        let iface = Arc::new(MockBleInterface::new());
        let (outbound_tx, outbound_rx) = mpsc::channel::<PendingSend>(4);
        let (_inbound_tx, inbound_rx) = mpsc::channel::<Bytes>(4);
        let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(4);
        let (registry_tx, _registry_rx) = mpsc::channel::<PeerCommand>(4);
        let (_swap_tx, swap_rx) = mpsc::channel::<blew::L2capChannel>(1);

        tokio::spawn(run_data_pipe(
            iface.clone() as Arc<dyn BleInterface>,
            blew::DeviceId::from("pipe-peri"),
            crate::transport::routing_v2::StableConnId::for_test(2),
            ConnectRole::Peripheral,
            ConnectPath::Gatt,
            None,
            outbound_rx,
            inbound_rx,
            incoming_tx,
            registry_tx,
            swap_rx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            LivenessClock::new(),
        ));

        outbound_tx
            .send(PendingSend {
                tx_gen: 1,
                datagram: Bytes::from_static(b"peri-out"),
                waker: noop_waker(),
            })
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if iface
                    .calls()
                    .iter()
                    .any(|c| matches!(c, CallKind::NotifyP2c { .. }))
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("expected NotifyP2c call");
    }

    #[tokio::test]
    async fn gatt_worker_quiesce_exits_without_stalled() {
        let iface = Arc::new(MockBleInterface::new());
        let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(4);
        let (registry_tx, mut registry_rx) = mpsc::channel::<PeerCommand>(4);

        let worker = spawn_gatt_worker(
            iface as Arc<dyn BleInterface>,
            blew::DeviceId::from("pipe-quiesce"),
            crate::transport::routing_v2::StableConnId::for_test(99),
            ConnectRole::Central,
            incoming_tx,
            registry_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            LivenessClock::new(),
        );

        let (shutdown_tx, handle) = match worker {
            ActiveWorker::Gatt {
                shutdown_tx,
                handle,
                ..
            } => (shutdown_tx, handle),
            ActiveWorker::L2cap { .. } => panic!("expected GATT worker"),
        };

        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("gatt worker should exit promptly")
            .expect("gatt worker should not panic");

        // Worker exit drops its registry_tx clone, so `Disconnected` is the
        // expected steady state here. Either Empty or Disconnected satisfies
        // "nothing was ever sent"; only an `Ok(...)` would indicate a leaked
        // Stalled notification.
        let got = registry_rx.try_recv();
        assert!(
            got.is_err(),
            "quiesce must not emit any PeerCommand; got {got:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn l2cap_pipe_drops_zero_length_outbound_and_counts() {
        let iface = Arc::new(MockBleInterface::new());
        let (outbound_tx, outbound_rx) = mpsc::channel::<PendingSend>(4);
        let (_inbound_tx, inbound_rx) = mpsc::channel::<Bytes>(4);
        let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(4);
        let (registry_tx, _registry_rx) = mpsc::channel::<PeerCommand>(4);
        let (_swap_tx, swap_rx) = mpsc::channel::<blew::L2capChannel>(1);

        let (central_side, peripheral_side) = blew::L2capChannel::pair(8192);
        let empty_frames = Arc::new(AtomicU64::new(0));

        let _pipe = tokio::spawn(run_data_pipe(
            iface as Arc<dyn BleInterface>,
            blew::DeviceId::from("l2cap-empty-out"),
            crate::transport::routing_v2::StableConnId::for_test(3),
            ConnectRole::Central,
            ConnectPath::L2cap,
            Some(central_side),
            outbound_rx,
            inbound_rx,
            incoming_tx,
            registry_tx,
            swap_rx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::clone(&empty_frames),
            LivenessClock::new(),
        ));

        // Empty outbound must not be framed onto the wire; only the real one
        // must reach the peer.
        outbound_tx
            .send(PendingSend {
                tx_gen: 1,
                datagram: Bytes::new(),
                waker: noop_waker(),
            })
            .await
            .unwrap();
        outbound_tx
            .send(PendingSend {
                tx_gen: 2,
                datagram: Bytes::from_static(b"post-empty"),
                waker: noop_waker(),
            })
            .await
            .unwrap();

        let (mut peri_rd, _peri_wr) = tokio::io::split(peripheral_side);
        let got = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            crate::transport::l2cap::read_framed_datagram(&mut peri_rd),
        )
        .await
        .expect("peer should see a framed datagram")
        .expect("read_framed_datagram must succeed")
        .expect("frame present");
        assert_eq!(got, b"post-empty");
        assert_eq!(
            empty_frames.load(Ordering::Relaxed),
            1,
            "outbound empty must be counted exactly once"
        );
    }
}
