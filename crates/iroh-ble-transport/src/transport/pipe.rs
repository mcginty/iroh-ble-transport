//! Per-peer data pipe task. A supervisor owns `outbound_rx`,
//! `inbound_rx`, and an `add_l2cap_rx` that delivers an L2CAP channel
//! once one becomes available. The supervisor holds a `WorkerSet`
//! with two independent optional workers — GATT and L2CAP — and
//! routes traffic across them:
//!
//! - **Outbound** prefers L2CAP when available (higher throughput
//!   and no fragmentation overhead), falling back to GATT. A single
//!   send commits to one path; we don't duplicate.
//! - **Inbound GATT fragments** (from the registry's
//!   `InboundGattFragment` command, arriving on `inbound_rx`) always
//!   feed the GATT worker's `ReliableChannel` for reassembly.
//! - **Inbound L2CAP frames** flow directly into the L2CAP worker's
//!   reader task (no supervisor involvement). Both workers'
//!   reassembled datagrams land on the same `incoming_tx` and iroh
//!   sees a single unified stream.
//!
//! Both paths coexist until their respective radios give up: GATT
//! dies on ACL drop, L2CAP dies when the CoC stream closes. There is
//! no "swap" — just independent add/remove of paths. This replaces
//! the prior design, in which an L2CAP "swap" would retire GATT
//! eagerly on the peripheral side and silently drop late GATT
//! handshake fragments sent during the gap before the central's own
//! swap completed (fixed the "Accepting incoming connection ended
//! with error: timed out" failure).
//!
//! Supervisor exit: when `outbound_rx` closes (registry signals the
//! pipe is done) OR both paths die, the supervisor tears down the
//! workers and returns. `routing::evict_pipe` then fires from the
//! driver's spawn site.

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

/// GATT path worker. Owns a `ReliableChannel` for the Selective-
/// Repeat ARQ protocol + a send loop that writes to the C2P or P2C
/// characteristic via the blew interface.
struct GattWorker {
    /// Outbound PendingSends the supervisor routes here.
    outbound_fwd_tx: mpsc::Sender<PendingSend>,
    /// Inbound GATT fragments (from `InboundGattFragment` commands)
    /// that the supervisor forwards for reassembly.
    inbound_fwd_tx: mpsc::Sender<Bytes>,
    /// One-shot shutdown signal for the worker's select loop.
    shutdown_tx: Option<oneshot::Sender<()>>,
    /// Observable by the worker's send sub-task so it can notice
    /// teardown between ACK waits.
    teardown_flag: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

/// L2CAP path worker. Reads length-prefixed frames directly from the
/// blew `L2capChannel` (no supervisor-mediated inbound) and writes
/// outbound PendingSends as framed payloads.
struct L2capWorker {
    /// Outbound PendingSends the supervisor routes here.
    outbound_fwd_tx: mpsc::Sender<PendingSend>,
    teardown_flag: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

/// Independent GATT + L2CAP workers for one peer pipe. Either may be
/// `Some` at any time; both can be `Some` simultaneously while L2CAP
/// is live alongside a still-healthy GATT ACL. Outbound prefers
/// L2CAP; inbound accepts from whichever delivers first.
struct WorkerSet {
    gatt: Option<GattWorker>,
    l2cap: Option<L2capWorker>,
}

impl WorkerSet {
    fn is_empty(&self) -> bool {
        self.gatt.is_none() && self.l2cap.is_none()
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_data_pipe(
    iface: Arc<dyn BleInterface>,
    device_id: blew::DeviceId,
    stable_conn_id: crate::transport::routing::StableConnId,
    role: ConnectRole,
    initial_path: ConnectPath,
    initial_l2cap: Option<blew::L2capChannel>,
    mut outbound_rx: mpsc::Receiver<PendingSend>,
    mut inbound_rx: mpsc::Receiver<Bytes>,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    registry_tx: mpsc::Sender<PeerCommand>,
    mut add_l2cap_rx: mpsc::Receiver<blew::L2capChannel>,
    retransmit_counter: Arc<AtomicU64>,
    truncation_counter: Arc<AtomicU64>,
    empty_frames_counter: Arc<AtomicU64>,
    last_rx_at: LivenessClock,
) {
    let mut workers = match initial_path {
        ConnectPath::Gatt => WorkerSet {
            gatt: Some(spawn_gatt_worker(
                Arc::clone(&iface),
                device_id.clone(),
                stable_conn_id,
                role,
                incoming_tx.clone(),
                registry_tx.clone(),
                Arc::clone(&retransmit_counter),
                Arc::clone(&truncation_counter),
                last_rx_at.clone(),
            )),
            l2cap: None,
        },
        ConnectPath::L2cap => {
            let Some(channel) = initial_l2cap else {
                tracing::error!(device = %device_id, "StartDataPipe(L2cap) without channel");
                return;
            };
            WorkerSet {
                gatt: None,
                l2cap: Some(spawn_l2cap_worker(
                    device_id.clone(),
                    stable_conn_id,
                    channel,
                    incoming_tx.clone(),
                    registry_tx.clone(),
                    Arc::clone(&empty_frames_counter),
                    last_rx_at.clone(),
                )),
            }
        }
    };

    loop {
        tokio::select! {
            maybe_send = outbound_rx.recv() => {
                let Some(send) = maybe_send else {
                    // Registry dropped its outbound sender → pipe is
                    // shutting down. Fall through to teardown.
                    break;
                };
                forward_outbound(&mut workers, send, &device_id, &registry_tx).await;
                if workers.is_empty() {
                    tracing::debug!(
                        device = %device_id,
                        "both paths dead after outbound send; pipe exiting"
                    );
                    break;
                }
            }
            maybe_bytes = inbound_rx.recv() => {
                let Some(bytes) = maybe_bytes else {
                    // inbound_rx only carries GATT fragments; its
                    // closure does NOT imply L2CAP is dead. But the
                    // registry closes this when the peer entry is
                    // gone, which is our signal to shut down.
                    break;
                };
                forward_inbound_gatt(&mut workers, bytes, &device_id);
                // Inbound doesn't force a path-dead check here — even
                // if GATT just died, L2CAP may still be carrying traffic.
            }
            maybe_chan = add_l2cap_rx.recv() => {
                let Some(channel) = maybe_chan else {
                    // add_l2cap_rx closed — no more L2CAP adds will
                    // ever arrive. Keep running; existing workers are
                    // unaffected. Disable the arm to stop busy-looping
                    // on a closed channel by creating a permanent
                    // pending future in its place.
                    add_l2cap_rx = mpsc::channel::<blew::L2capChannel>(1).1;
                    continue;
                };
                if workers.l2cap.is_some() {
                    tracing::debug!(
                        device = %device_id,
                        "ignoring redundant L2CAP channel (one is already live)"
                    );
                    continue;
                }
                workers.l2cap = Some(spawn_l2cap_worker(
                    device_id.clone(),
                    stable_conn_id,
                    channel,
                    incoming_tx.clone(),
                    registry_tx.clone(),
                    Arc::clone(&empty_frames_counter),
                    last_rx_at.clone(),
                ));
                tracing::info!(
                    device = %device_id,
                    has_gatt = workers.gatt.is_some(),
                    "L2CAP path added to pipe"
                );
            }
        }
    }

    teardown_worker_set(workers, device_id, L2CAP_HANDOVER_TIMEOUT).await;
}

/// Forward an outbound datagram, preferring L2CAP. On worker death
/// (channel closed or wedged past `L2CAP_HANDOVER_TIMEOUT`), the
/// corresponding worker is evicted from `workers` and we fall
/// through to the other path with the same datagram. If both paths
/// fail for a single send, the waker is woken anyway so iroh
/// observes the queue-release.
///
/// Uses `send_timeout` on L2CAP (bounded blocking) so a wedged
/// L2CAP peer doesn't stall the supervisor indefinitely. Uses
/// `try_send` on GATT (no blocking) because GATT is the fallback —
/// if GATT is also backpressured, iroh's retransmit will handle it.
///
/// Emits `PeerCommand::L2capHandoverTimeout` to the registry when
/// L2CAP is evicted due to a timeout, so the registry can flip its
/// `l2cap_upgrade_failed` flag and stop proposing L2CAP for this
/// peer for a while.
async fn forward_outbound(
    workers: &mut WorkerSet,
    send: PendingSend,
    device_id: &blew::DeviceId,
    registry_tx: &mpsc::Sender<PeerCommand>,
) {
    let mut send = Some(send);
    // Prefer L2CAP when present.
    let l2cap_send = if let Some(l2cap) = workers.l2cap.as_ref() {
        Some(
            l2cap
                .outbound_fwd_tx
                .send_timeout(
                    send.take().expect("outbound datagram available for L2CAP"),
                    L2CAP_HANDOVER_TIMEOUT,
                )
                .await,
        )
    } else {
        None
    };
    let send = if let Some(result) = l2cap_send {
        match result {
            Ok(()) => return,
            Err(mpsc::error::SendTimeoutError::Timeout(send)) => {
                tracing::warn!(
                    device = %device_id,
                    timeout_ms = L2CAP_HANDOVER_TIMEOUT.as_millis(),
                    "L2CAP outbound wedged; evicting L2CAP path and falling back to GATT"
                );
                if let Some(worker) = workers.l2cap.take() {
                    teardown_l2cap_worker(worker, device_id.clone(), L2CAP_HANDOVER_TIMEOUT).await;
                }
                let _ = registry_tx
                    .send(PeerCommand::L2capHandoverTimeout {
                        device_id: device_id.clone(),
                    })
                    .await;
                send
            }
            Err(mpsc::error::SendTimeoutError::Closed(send)) => {
                tracing::debug!(
                    device = %device_id,
                    "L2CAP path closed during outbound; falling back to GATT"
                );
                if let Some(worker) = workers.l2cap.take() {
                    teardown_l2cap_worker(worker, device_id.clone(), L2CAP_HANDOVER_TIMEOUT).await;
                }
                send
            }
        }
    } else {
        send.expect("outbound datagram available for fallback path")
    };
    if let Some(gatt) = workers.gatt.as_ref() {
        match gatt.outbound_fwd_tx.try_send(send) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Closed(send)) => {
                tracing::debug!(
                    device = %device_id,
                    "GATT path closed during outbound"
                );
                workers.gatt = None;
                send.waker.wake();
            }
            Err(mpsc::error::TrySendError::Full(send)) => {
                tracing::debug!(
                    device = %device_id,
                    "GATT outbound full; dropping datagram (iroh retries)"
                );
                send.waker.wake();
            }
        }
    } else {
        tracing::debug!(
            device = %device_id,
            "outbound datagram with no live path; dropping"
        );
        send.waker.wake();
    }
}

/// Forward an inbound GATT fragment to the GATT worker for
/// reassembly. If the GATT worker is gone (ACL dropped), the
/// fragment is dropped — there's no `ReliableChannel` alive to
/// reassemble into, and QUIC will retransmit via L2CAP if the data
/// matters.
///
/// Edge case: if the pipe was created fresh with an L2CAP path and
/// never had a GATT worker (e.g. an inbound L2CAP accept that
/// preceded any GATT session), late GATT fragments are silently
/// dropped here. In practice this is rare because GATT handshaking
/// normally precedes L2CAP; if it happens, QUIC retransmit covers it.
fn forward_inbound_gatt(workers: &mut WorkerSet, bytes: Bytes, device_id: &blew::DeviceId) {
    let Some(gatt) = workers.gatt.as_ref() else {
        tracing::trace!(
            device = %device_id,
            "inbound GATT fragment dropped: no GATT worker alive"
        );
        return;
    };
    match gatt.inbound_fwd_tx.try_send(bytes) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Closed(_)) => {
            tracing::debug!(
                device = %device_id,
                "GATT path closed while forwarding inbound; evicting worker"
            );
            workers.gatt = None;
        }
        Err(mpsc::error::TrySendError::Full(_)) => {
            // Receiver buffer saturated. Peer's ReliableChannel will
            // retransmit if this fragment mattered; no recovery here
            // is needed.
            tracing::debug!(
                device = %device_id,
                "GATT inbound full; dropping fragment (peer will retransmit)"
            );
        }
    }
}

/// Drop forwarding senders on every live worker, signal teardown, and
/// wait bounded for each to exit. Aborts any that don't meet the
/// deadline so a wedged worker can't hold the pipe supervisor past
/// teardown.
async fn teardown_worker_set(workers: WorkerSet, device_id: blew::DeviceId, timeout: Duration) {
    let WorkerSet { gatt, l2cap } = workers;
    if let Some(w) = gatt {
        teardown_gatt_worker(w, device_id.clone(), timeout).await;
    }
    if let Some(w) = l2cap {
        teardown_l2cap_worker(w, device_id.clone(), timeout).await;
    }
}

async fn teardown_gatt_worker(w: GattWorker, device_id: blew::DeviceId, timeout: Duration) {
    let GattWorker {
        outbound_fwd_tx,
        inbound_fwd_tx,
        mut shutdown_tx,
        teardown_flag,
        mut handle,
    } = w;
    // Dropping the forwarding senders closes the worker's input
    // channels; the GATT worker's select loop breaks and its
    // ReliableChannel's send sub-task joins.
    drop(outbound_fwd_tx);
    drop(inbound_fwd_tx);
    teardown_flag.store(true, Ordering::Relaxed);
    if let Some(s) = shutdown_tx.take() {
        let _ = s.send(());
    }
    if tokio::time::timeout(timeout, &mut handle).await.is_err() {
        handle.abort();
        tracing::debug!(
            device = %device_id,
            "GATT worker did not exit within teardown timeout; aborted"
        );
    }
}

async fn teardown_l2cap_worker(w: L2capWorker, device_id: blew::DeviceId, timeout: Duration) {
    let L2capWorker {
        outbound_fwd_tx,
        teardown_flag,
        mut handle,
    } = w;
    drop(outbound_fwd_tx);
    teardown_flag.store(true, Ordering::Relaxed);
    if tokio::time::timeout(timeout, &mut handle).await.is_err() {
        handle.abort();
        tracing::debug!(
            device = %device_id,
            "L2CAP worker did not exit within teardown timeout; aborted"
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_gatt_worker(
    iface: Arc<dyn BleInterface>,
    device_id: blew::DeviceId,
    stable_conn_id: crate::transport::routing::StableConnId,
    role: ConnectRole,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    registry_tx: mpsc::Sender<PeerCommand>,
    retransmit_counter: Arc<AtomicU64>,
    truncation_counter: Arc<AtomicU64>,
    last_rx_at: LivenessClock,
) -> GattWorker {
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
    GattWorker {
        outbound_fwd_tx,
        inbound_fwd_tx,
        shutdown_tx: Some(shutdown_tx),
        teardown_flag,
        handle,
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_l2cap_worker(
    device_id: blew::DeviceId,
    stable_conn_id: crate::transport::routing::StableConnId,
    channel: blew::L2capChannel,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    registry_tx: mpsc::Sender<PeerCommand>,
    empty_frames_counter: Arc<AtomicU64>,
    last_rx_at: LivenessClock,
) -> L2capWorker {
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
    L2capWorker {
        outbound_fwd_tx,
        teardown_flag,
        handle,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_gatt_pipe(
    iface: Arc<dyn BleInterface>,
    device_id: blew::DeviceId,
    stable_conn_id: crate::transport::routing::StableConnId,
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
    stable_conn_id: crate::transport::routing::StableConnId,
    channel: blew::L2capChannel,
    mut outbound_rx: mpsc::Receiver<PendingSend>,
    teardown_flag: Arc<AtomicBool>,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    registry_tx: mpsc::Sender<PeerCommand>,
    empty_frames_counter: Arc<AtomicU64>,
    last_rx_at: LivenessClock,
) {
    let (reader, writer) = tokio::io::split(channel);

    let (l2cap_tx, send_task, recv_task, done) = crate::transport::l2cap::spawn_l2cap_io_tasks(
        reader,
        writer,
        device_id.clone(),
        stable_conn_id,
        incoming_tx,
        last_rx_at,
        Arc::clone(&teardown_flag),
        Arc::clone(&empty_frames_counter),
    );
    let _send_task = AbortOnDrop(send_task);
    let _recv_task = AbortOnDrop(recv_task);

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
                        // Keep mixed-version peers from seeing zero-length
                        // L2CAP datagrams. Ack the send so iroh's waker
                        // unblocks, but skip the wire.
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
    use std::sync::atomic::AtomicBool;
    use std::task::{RawWaker, RawWakerVTable, Waker};
    use tokio::sync::Notify;

    fn noop_waker() -> Waker {
        fn no_op(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
    }

    fn make_blocked_l2cap_worker() -> (L2capWorker, Arc<Notify>, Arc<AtomicBool>) {
        let (outbound_fwd_tx, outbound_fwd_rx) = mpsc::channel::<PendingSend>(1);
        outbound_fwd_tx
            .try_send(PendingSend {
                tx_gen: 0,
                datagram: Bytes::from_static(b"occupied"),
                waker: noop_waker(),
            })
            .expect("seed the queue so send_timeout hits the full branch");

        let release = Arc::new(Notify::new());
        let released = Arc::new(AtomicBool::new(false));
        let teardown_flag = Arc::new(AtomicBool::new(false));

        let release_c = Arc::clone(&release);
        let released_c = Arc::clone(&released);
        let teardown_c = Arc::clone(&teardown_flag);
        let handle = tokio::spawn(async move {
            let _keep_receiver_alive = outbound_fwd_rx;
            loop {
                if teardown_c.load(Ordering::Relaxed) {
                    return;
                }
                tokio::select! {
                    _ = release_c.notified() => {
                        released_c.store(true, Ordering::SeqCst);
                        return;
                    }
                    _ = tokio::task::yield_now() => {}
                }
            }
        });

        (
            L2capWorker {
                outbound_fwd_tx,
                teardown_flag,
                handle,
            },
            release,
            released,
        )
    }

    fn make_closed_l2cap_worker() -> (L2capWorker, Arc<Notify>, Arc<AtomicBool>) {
        let (outbound_fwd_tx, outbound_fwd_rx) = mpsc::channel::<PendingSend>(1);

        let release = Arc::new(Notify::new());
        let released = Arc::new(AtomicBool::new(false));
        let teardown_flag = Arc::new(AtomicBool::new(false));

        let release_c = Arc::clone(&release);
        let released_c = Arc::clone(&released);
        let teardown_c = Arc::clone(&teardown_flag);
        let handle = tokio::spawn(async move {
            drop(outbound_fwd_rx);
            loop {
                if teardown_c.load(Ordering::Relaxed) {
                    return;
                }
                tokio::select! {
                    _ = release_c.notified() => {
                        released_c.store(true, Ordering::SeqCst);
                        return;
                    }
                    _ = tokio::task::yield_now() => {}
                }
            }
        });

        (
            L2capWorker {
                outbound_fwd_tx,
                teardown_flag,
                handle,
            },
            release,
            released,
        )
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
            crate::transport::routing::StableConnId::for_test(1),
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
            crate::transport::routing::StableConnId::for_test(2),
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
            crate::transport::routing::StableConnId::for_test(99),
            ConnectRole::Central,
            incoming_tx,
            registry_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            LivenessClock::new(),
        );

        let GattWorker {
            shutdown_tx,
            handle,
            ..
        } = worker;

        shutdown_tx
            .expect("fresh GATT worker has shutdown_tx")
            .send(())
            .unwrap();
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
            crate::transport::routing::StableConnId::for_test(3),
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

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn l2cap_timeout_path_stops_worker_before_dropping_it() {
        let (registry_tx, mut registry_rx) = mpsc::channel::<PeerCommand>(4);
        let (worker, release, released) = make_blocked_l2cap_worker();
        let device_id = blew::DeviceId::from("l2cap-timeout");

        let mut task = tokio::spawn(async move {
            let mut workers = WorkerSet {
                gatt: None,
                l2cap: Some(worker),
            };
            forward_outbound(
                &mut workers,
                PendingSend {
                    tx_gen: 1,
                    datagram: Bytes::from_static(b"timeout-me"),
                    waker: noop_waker(),
                },
                &device_id,
                &registry_tx,
            )
            .await;
            workers
        });

        tokio::task::yield_now().await;
        tokio::time::advance(L2CAP_HANDOVER_TIMEOUT + Duration::from_millis(1)).await;
        let workers = (&mut task).await.expect("forward_outbound should complete");

        assert!(
            workers.l2cap.is_none(),
            "timed-out L2CAP worker must be evicted"
        );
        match registry_rx.recv().await.expect("timeout metric command") {
            PeerCommand::L2capHandoverTimeout { device_id: got } => {
                assert_eq!(got, blew::DeviceId::from("l2cap-timeout"));
            }
            other => panic!("expected L2capHandoverTimeout, got {other:?}"),
        }

        release.notify_one();
        tokio::task::yield_now().await;
        assert!(
            !released.load(Ordering::SeqCst),
            "timed-out L2CAP worker must have been stopped before it was dropped"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn l2cap_closed_path_stops_worker_before_dropping_it() {
        let (registry_tx, mut registry_rx) = mpsc::channel::<PeerCommand>(4);
        let (worker, release, released) = make_closed_l2cap_worker();
        let device_id = blew::DeviceId::from("l2cap-closed");

        let mut task = tokio::spawn(async move {
            let mut workers = WorkerSet {
                gatt: None,
                l2cap: Some(worker),
            };
            forward_outbound(
                &mut workers,
                PendingSend {
                    tx_gen: 1,
                    datagram: Bytes::from_static(b"closed-me"),
                    waker: noop_waker(),
                },
                &device_id,
                &registry_tx,
            )
            .await;
            workers
        });

        let workers = tokio::time::timeout(Duration::from_secs(1), &mut task)
            .await
            .expect("forward_outbound should complete promptly on closed L2CAP")
            .expect("forward_outbound task should not panic");

        assert!(
            workers.l2cap.is_none(),
            "closed L2CAP worker must be evicted"
        );
        let got = registry_rx.try_recv();
        assert!(
            got.is_err(),
            "closed L2CAP path must not emit a timeout metric; got {got:?}"
        );

        release.notify_one();
        tokio::task::yield_now().await;
        assert!(
            !released.load(Ordering::SeqCst),
            "closed L2CAP worker must have been stopped before it was dropped"
        );
    }
}
