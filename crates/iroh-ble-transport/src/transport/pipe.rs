//! Per-peer data pipe task. Owns one `ReliableChannel` + byte I/O, spawned
//! by the driver on `PeerAction::StartDataPipe` and torn down when either
//! channel closes or the reliable send loop exits with `LinkDead`.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use bytes::Bytes;
use tokio::sync::mpsc;

use crate::transport::driver::IncomingPacket;
use crate::transport::interface::BleInterface;
use crate::transport::mtu::resolve_chunk_size;
use crate::transport::peer::{ConnectRole, PeerCommand, PendingSend};
use crate::transport::reliable::ReliableChannel;

/// Run one peer's data pipe.
///
/// Drives three streams in a `tokio::select!`:
/// - outbound datagrams from the registry → `ReliableChannel::enqueue_datagram`
/// - inbound GATT fragments from the registry → `ReliableChannel::receive_fragment`
/// - reassembled datagrams from the channel → `incoming_tx`
///
/// The reliable send loop runs as a sub-task; when it exits with `LinkDead`,
/// we report `PeerCommand::Stalled { device_id }` to the registry and return.
/// Any registry-initiated tear-down closes the two mpsc senders on
/// `PeerEntry.pipe`, which wakes the select! and exits cleanly.
#[allow(clippy::too_many_arguments)]
pub async fn run_data_pipe(
    iface: Arc<dyn BleInterface>,
    device_id: blew::DeviceId,
    role: ConnectRole,
    mut outbound_rx: mpsc::Receiver<PendingSend>,
    mut inbound_rx: mpsc::Receiver<Bytes>,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    registry_tx: mpsc::Sender<PeerCommand>,
    retransmit_counter: Arc<AtomicU64>,
    truncation_counter: Arc<AtomicU64>,
) {
    let chunk_size = resolve_chunk_size(iface.as_ref(), &device_id).await;
    let (channel, mut datagram_rx) =
        ReliableChannel::new(chunk_size, retransmit_counter, truncation_counter);
    let channel = Arc::new(channel);

    let send_loop_handle = {
        let channel = Arc::clone(&channel);
        let iface = Arc::clone(&iface);
        let device_id = device_id.clone();
        let span = tracing::info_span!("ble_pipe", device = %device_id);
        tokio::spawn(tracing::Instrument::instrument(
            async move {
                channel
                    .run_send_loop(move |bytes| {
                        let iface = Arc::clone(&iface);
                        let device_id = device_id.clone();
                        let role = role;
                        async move {
                            let buf = Bytes::from(bytes);
                            let result = match role {
                                ConnectRole::Central => iface.write_c2p(&device_id, buf).await,
                                ConnectRole::Peripheral => iface.notify_p2c(&device_id, buf).await,
                            };
                            result.map_err(|e| format!("{e}"))
                        }
                    })
                    .await
            },
            span,
        ))
    };

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
                        let _ = incoming_tx
                            .send(IncomingPacket {
                                device_id: device_id.clone(),
                                data: Bytes::from(data),
                            })
                            .await;
                    }
                    None => break,
                }
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

    // If we exited because the registry dropped our mpsc handles (the
    // registry-initiated teardown path: `CentralDisconnected`, `Stalled`,
    // explicit disconnect), the send loop sub-task is still parked on its
    // `wake.notified()` future. Mark the channel dead so it exits promptly
    // instead of leaking for up to `LINK_DEAD_DEADLINE`, and join it so the
    // task is collected before we return.
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

        let device_id = blew::DeviceId::from("pipe-central");
        tokio::spawn(run_data_pipe(
            iface.clone() as Arc<dyn BleInterface>,
            device_id.clone(),
            ConnectRole::Central,
            outbound_rx,
            inbound_rx,
            incoming_tx,
            registry_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
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

        tokio::spawn(run_data_pipe(
            iface.clone() as Arc<dyn BleInterface>,
            blew::DeviceId::from("pipe-peri"),
            ConnectRole::Peripheral,
            outbound_rx,
            inbound_rx,
            incoming_tx,
            registry_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
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
}
