//! Integration test: pipe supervisor manages independent GATT + L2CAP
//! workers. L2CAP adds alongside GATT (no "swap"/retire); outbound
//! prefers L2CAP when live; inbound arrives on either.
//!
//! The prior "swap" model has been retired — see `pipe.rs` docstring
//! for context. These tests verify the new both-paths-alive
//! semantics, including the regression where late GATT fragments
//! during the handover window were dropped (caused TLS timeouts on
//! inbound accepts).

#![cfg(feature = "testing")]
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::task::{RawWaker, RawWakerVTable, Waker};
use std::time::Duration;

use bytes::Bytes;
use iroh_ble_transport::transport::driver::IncomingPacket;
use iroh_ble_transport::transport::interface::BleInterface;
use iroh_ble_transport::transport::peer::{
    ConnectPath, ConnectRole, LivenessClock, PeerCommand, PendingSend,
};
use iroh_ble_transport::transport::pipe::run_data_pipe;
use iroh_ble_transport::transport::test_util::{CallKind, MockBleInterface};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

fn noop_waker() -> Waker {
    fn no_op(_: *const ()) {}
    fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

/// Build a single-fragment ReliableChannel frame: `[seq|FIRST|LAST][ack=0][payload][canary]`.
/// The canary is the `0x5A` sentinel the receiver validates and strips.
fn make_reliable_fragment(seq: u8, payload: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(2 + payload.len() + 1);
    out.push(0x30 | (seq & 0x0F)); // FIRST | LAST | seq
    out.push(0x00); // ACK byte zeroed
    out.extend_from_slice(payload);
    out.push(0x5A); // canary
    Bytes::from(out)
}

#[tokio::test(flavor = "multi_thread")]
async fn adding_l2cap_routes_new_sends_there_and_leaves_gatt_receiving() {
    // Start GATT-only, then add L2CAP alongside. Outbound after the
    // add must land on L2CAP (preferred path), but the GATT worker
    // must remain alive and capable of receiving inbound fragments.
    let iface = Arc::new(MockBleInterface::new());
    let (outbound_tx, outbound_rx) = mpsc::channel::<PendingSend>(8);
    let (inbound_tx, inbound_rx) = mpsc::channel::<Bytes>(8);
    let (incoming_tx, mut incoming_rx) = mpsc::channel::<IncomingPacket>(8);
    let (registry_tx, _registry_rx) = mpsc::channel::<PeerCommand>(8);
    let (swap_tx, swap_rx) = mpsc::channel::<blew::L2capChannel>(1);

    let device_id = blew::DeviceId::from("both-alive");
    let pipe_handle = tokio::spawn(run_data_pipe(
        iface.clone() as Arc<dyn BleInterface>,
        device_id.clone(),
        iroh_ble_transport::transport::routing_v2::StableConnId::for_test(1),
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

    // Pre-L2CAP: one outbound datagram should hit GATT's write_c2p.
    outbound_tx
        .send(PendingSend {
            tx_gen: 1,
            datagram: Bytes::from_static(b"before-add"),
            waker: noop_waker(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if iface
                .calls()
                .iter()
                .any(|c| matches!(c, CallKind::WriteC2p { .. }))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("GATT WriteC2p observed pre-add");

    // Add L2CAP alongside GATT.
    let (central_side, peripheral_side) = blew::L2capChannel::pair(8192);
    swap_tx.send(central_side).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Post-add outbound should go to L2CAP.
    outbound_tx
        .send(PendingSend {
            tx_gen: 2,
            datagram: Bytes::from_static(b"after-add"),
            waker: noop_waker(),
        })
        .await
        .unwrap();

    let mut peri_reader = peripheral_side;
    let mut len_buf = [0u8; 2];
    tokio::time::timeout(Duration::from_secs(2), peri_reader.read_exact(&mut len_buf))
        .await
        .expect("L2CAP read timed out — post-add send didn't go to L2CAP")
        .expect("L2CAP read len");
    let len = u16::from_le_bytes(len_buf) as usize;
    assert_eq!(len, b"after-add".len());
    let mut payload = vec![0u8; len];
    peri_reader
        .read_exact(&mut payload)
        .await
        .expect("L2CAP read payload");
    assert_eq!(&payload, b"after-add");

    // GATT worker must still be alive for receiving. Deliver a valid
    // GATT fragment via inbound_rx; it should reassemble to
    // `incoming_tx`.
    inbound_tx
        .send(make_reliable_fragment(0, b"gatt-after-l2cap"))
        .await
        .unwrap();
    let pkt = tokio::time::timeout(Duration::from_secs(2), incoming_rx.recv())
        .await
        .expect("inbound GATT fragment timed out after L2CAP add")
        .expect("incoming_rx closed");
    assert_eq!(pkt.data.as_ref(), b"gatt-after-l2cap");

    // Tear down.
    drop(outbound_tx);
    drop(inbound_tx);
    drop(swap_tx);
    let _ = tokio::time::timeout(Duration::from_secs(3), pipe_handle).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn late_gatt_fragments_after_l2cap_add_still_reassemble() {
    // Regression for the "Accepting incoming connection ended with
    // error: timed out" hardware bug. In the prior (swap) design, a
    // peripheral-side L2CAP swap retired the GATT worker, so late
    // fragments from a peer whose own swap hadn't completed yet —
    // typically carrying the TLS ClientFinished — were dropped and
    // the server-side handshake timed out.
    //
    // In the new both-paths-alive design this can't happen: GATT
    // stays alive until its ACL dies. This test fires the drain
    // scenario (L2CAP add races ahead of peer's own add) and
    // verifies inbound GATT still delivers.
    let iface = Arc::new(MockBleInterface::new());
    let (outbound_tx, outbound_rx) = mpsc::channel::<PendingSend>(4);
    let (inbound_tx, inbound_rx) = mpsc::channel::<Bytes>(8);
    let (incoming_tx, mut incoming_rx) = mpsc::channel::<IncomingPacket>(8);
    let (registry_tx, _registry_rx) = mpsc::channel::<PeerCommand>(4);
    let (swap_tx, swap_rx) = mpsc::channel::<blew::L2capChannel>(1);

    let device_id = blew::DeviceId::from("handover");
    let pipe_handle = tokio::spawn(run_data_pipe(
        iface.clone() as Arc<dyn BleInterface>,
        device_id.clone(),
        iroh_ble_transport::transport::routing_v2::StableConnId::for_test(2),
        ConnectRole::Peripheral, // the role where the original bug bit
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

    // Pre-add: frag seq=0 delivers.
    inbound_tx
        .send(make_reliable_fragment(0, b"pre-add"))
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(2), incoming_rx.recv())
        .await
        .expect("pre-add datagram timeout")
        .expect("incoming_rx closed");
    assert_eq!(first.data.as_ref(), b"pre-add");

    // Add L2CAP.
    let (central_side, _peripheral_side) = blew::L2capChannel::pair(8192);
    swap_tx.send(central_side).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Post-add: frag seq=1 on GATT must STILL deliver. In the old
    // design this would be dropped (or drained for 3 s, then
    // dropped). In the new design GATT stays fully alive; no
    // timeline-dependent behaviour.
    inbound_tx
        .send(make_reliable_fragment(1, b"post-add-gatt"))
        .await
        .unwrap();
    let second = tokio::time::timeout(Duration::from_secs(2), incoming_rx.recv())
        .await
        .expect(
            "post-add GATT datagram timed out — regression: L2CAP add \
             should not disable GATT inbound reassembly",
        )
        .expect("incoming_rx closed");
    assert_eq!(second.data.as_ref(), b"post-add-gatt");

    drop(outbound_tx);
    drop(inbound_tx);
    drop(swap_tx);
    let _ = tokio::time::timeout(Duration::from_secs(3), pipe_handle).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn supervisor_shuts_down_cleanly_on_outbound_close() {
    let iface = Arc::new(MockBleInterface::new());
    let (outbound_tx, outbound_rx) = mpsc::channel::<PendingSend>(4);
    let (_inbound_tx, inbound_rx) = mpsc::channel::<Bytes>(4);
    let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(4);
    let (registry_tx, _registry_rx) = mpsc::channel::<PeerCommand>(4);
    let (_swap_tx, swap_rx) = mpsc::channel::<blew::L2capChannel>(1);

    let handle = tokio::spawn(run_data_pipe(
        iface.clone() as Arc<dyn BleInterface>,
        blew::DeviceId::from("shutdown"),
        iroh_ble_transport::transport::routing_v2::StableConnId::for_test(3),
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

    drop(outbound_tx);
    tokio::time::timeout(Duration::from_secs(3), handle)
        .await
        .expect("supervisor did not exit within bound")
        .expect("supervisor panicked");
}
