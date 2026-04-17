//! Integration tests exercising the dedup state machine via MockFabric.

#![cfg(feature = "testing")]
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use arc_swap::ArcSwap;
use atomic_waker::AtomicWaker;
use blew::{BleDevice, DeviceId};
use bytes::Bytes;
use iroh_ble_transport::transport::{
    driver::{Driver, IncomingPacket},
    peer::{ConnectRole, PeerCommand},
    registry::{PhaseKind, Registry, SnapshotMaps},
    routing::prefix_from_endpoint,
    store::InMemoryPeerStore,
    test_util::MockBleInterface,
    transport::L2capPolicy,
};
use tokio::sync::mpsc;

fn zero_counters() -> (Arc<AtomicU64>, Arc<AtomicU64>) {
    (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)))
}

fn ble_device(id: &DeviceId) -> BleDevice {
    BleDevice {
        id: id.clone(),
        name: None,
        rssi: None,
        services: vec![],
    }
}

fn waker_from_channel(tx: mpsc::Sender<()>) -> std::task::Waker {
    use std::task::{Wake, Waker};
    struct W(mpsc::Sender<()>);
    impl Wake for W {
        fn wake(self: Arc<Self>) {
            let _ = self.0.try_send(());
        }
    }
    Waker::from(Arc::new(W(tx)))
}

/// An endpoint with all bytes set to the given value — gives us a
/// controllable ordering for the dedup tiebreaker.
fn endpoint_with_byte(b: u8) -> iroh_base::EndpointId {
    let mut bytes = [0u8; 32];
    bytes[0] = b;
    iroh_base::SecretKey::from_bytes(&bytes).public()
}

struct TestNode {
    #[allow(dead_code)]
    device_id: DeviceId,
    inbox_tx: mpsc::Sender<PeerCommand>,
    snapshots: Arc<ArcSwap<SnapshotMaps>>,
    #[allow(dead_code)]
    iface: Arc<MockBleInterface>,
}

fn spawn_node_with_endpoint(
    device_id: DeviceId,
    inbox_tx: mpsc::Sender<PeerCommand>,
    inbox_rx: mpsc::Receiver<PeerCommand>,
    iface: Arc<MockBleInterface>,
    policy: L2capPolicy,
    endpoint: iroh_base::EndpointId,
) -> TestNode {
    let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(64);
    let snapshots = Arc::new(ArcSwap::from(Arc::new(SnapshotMaps::default())));
    let (retransmits, truncations) = zero_counters();

    let driver = Driver::new(
        Arc::clone(&iface),
        inbox_tx.clone(),
        incoming_tx,
        retransmits,
        truncations,
        Arc::new(InMemoryPeerStore::new()),
    );
    let registry = Registry::new_for_test_with_policy_and_endpoint(policy, endpoint);
    let snap = snapshots.clone();
    tokio::spawn(async move {
        registry
            .run(inbox_rx, driver, snap, Arc::new(AtomicWaker::new()))
            .await;
    });

    TestNode {
        device_id,
        inbox_tx,
        snapshots,
        iface,
    }
}

/// Wait until both nodes have exactly one peer in the given phase, then
/// return the summaries.
async fn wait_both_connected(
    a: &TestNode,
    b: &TestNode,
    timeout: Duration,
) {
    tokio::time::timeout(timeout, async {
        loop {
            let a_snap = a.snapshots.load();
            let b_snap = b.snapshots.load();
            let a_connected = a_snap
                .peer_states
                .values()
                .filter(|s| s.phase_kind == PhaseKind::Connected)
                .count();
            let b_connected = b_snap
                .peer_states
                .values()
                .filter(|s| s.phase_kind == PhaseKind::Connected)
                .count();
            if a_connected >= 1 && b_connected >= 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("timed out waiting for both nodes to reach Connected");
}

// ─── Task 19 ─────────────────────────────────────────────────────────────────

/// Two nodes discover each other. Node A (higher EndpointId) dials as Central;
/// after both reach Connected and receive VerifiedEndpoint for their peer,
/// each side ends up with exactly one Connected peer. The dedup tiebreaker
/// keeps the Central entry on A's side and the Peripheral entry on B's side.
#[tokio::test(flavor = "multi_thread")]
async fn symmetric_dial_resolves_to_one_pipe_per_side() {
    // A has a higher endpoint byte → A's prefix > B's prefix →
    // A dials immediately; B defers via fairness window.
    let ep_a = endpoint_with_byte(0xFF);
    let ep_b = endpoint_with_byte(0x01);

    let dev_a = DeviceId::from("dedup-a");
    let dev_b = DeviceId::from("dedup-b");

    let (a_inbox_tx, a_inbox_rx) = mpsc::channel::<PeerCommand>(256);
    let (b_inbox_tx, b_inbox_rx) = mpsc::channel::<PeerCommand>(256);

    // The iface for A has its c2p hook route to B's inbox (as a GATT fragment),
    // and p2c hook for B routes to A's inbox. We set these up manually since
    // we need per-node ifaces with distinct endpoints.
    let iface_a = Arc::new(MockBleInterface::new());
    let iface_b = Arc::new(MockBleInterface::new());

    // Wire A's c2p writes → B's inbox as InboundGattFragment from dev_a.
    {
        let inbox = b_inbox_tx.clone();
        let from = dev_a.clone();
        iface_a.set_on_c2p_write(Box::new(move |_target, bytes| {
            let cmd = PeerCommand::InboundGattFragment {
                device_id: from.clone(),
                source: iroh_ble_transport::transport::peer::FragmentSource::PeripheralReceivedC2p,
                bytes,
            };
            let _ = inbox.try_send(cmd);
        }));
    }
    // Wire B's p2c notifies → A's inbox.
    {
        let inbox = a_inbox_tx.clone();
        let from = dev_b.clone();
        iface_b.set_on_p2c_notify(Box::new(move |_target, bytes| {
            let cmd = PeerCommand::InboundGattFragment {
                device_id: from.clone(),
                source: iroh_ble_transport::transport::peer::FragmentSource::CentralReceivedP2c,
                bytes,
            };
            let _ = inbox.try_send(cmd);
        }));
    }

    let a = spawn_node_with_endpoint(
        dev_a.clone(),
        a_inbox_tx.clone(),
        a_inbox_rx,
        iface_a,
        L2capPolicy::Disabled,
        ep_a,
    );
    let b = spawn_node_with_endpoint(
        dev_b.clone(),
        b_inbox_tx.clone(),
        b_inbox_rx,
        iface_b,
        L2capPolicy::Disabled,
        ep_b,
    );

    // A advertises B using B's actual endpoint-derived prefix. Since A's prefix
    // (from ep_a=0xFF) > B's prefix (from ep_b=0x01), A does NOT defer → dials.
    let prefix_b = prefix_from_endpoint(&ep_b);
    let prefix_a = prefix_from_endpoint(&ep_a);

    let (waker_tx, _) = mpsc::channel::<()>(1);
    let waker = waker_from_channel(waker_tx);

    a.inbox_tx
        .send(PeerCommand::Advertised {
            prefix: prefix_b,
            device: ble_device(&dev_b),
            rssi: None,
        })
        .await
        .unwrap();
    a.inbox_tx
        .send(PeerCommand::SendDatagram {
            device_id: dev_b.clone(),
            tx_gen: 0,
            datagram: Bytes::from_static(b"hello-from-a"),
            waker,
        })
        .await
        .unwrap();

    // B advertises A to itself (B's prefix is 0x01, A's prefix is 0xFF).
    // B's my_prefix=0x01 < A's prefix=0xFF → B would defer. However, since
    // we want to test the dedup path we also send a VerifiedEndpoint, and the
    // peripheral-side pipe gets materialised via InboundGattFragment when A
    // writes to B.
    b.inbox_tx
        .send(PeerCommand::Advertised {
            prefix: prefix_a,
            device: ble_device(&dev_a),
            rssi: None,
        })
        .await
        .unwrap();

    // Wait for A to reach Connected with B (A connected as Central).
    wait_both_connected(&a, &b, Duration::from_secs(10)).await;

    // Send VerifiedEndpoint to both sides so the dedup pass can run.
    // On A: it has a Connected entry for dev_b. A's should_win(Central, ep_a, ep_b)
    // → ep_a > ep_b → keeps Central → no prune.
    // On B: it has a Connected entry for dev_a (materialised as Peripheral via
    // InboundGattFragment). B's should_win(Peripheral, ep_b, ep_a) → ep_b < ep_a
    // → keeps Peripheral → no prune.
    a.inbox_tx
        .send(PeerCommand::VerifiedEndpoint {
            endpoint_id: ep_b,
        })
        .await
        .unwrap();
    b.inbox_tx
        .send(PeerCommand::VerifiedEndpoint {
            endpoint_id: ep_a,
        })
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Assert: each side has exactly one Connected peer.
    let a_snap = a.snapshots.load();
    let b_snap = b.snapshots.load();

    let a_connected: Vec<_> = a_snap
        .peer_states
        .iter()
        .filter(|(_, s)| s.phase_kind == PhaseKind::Connected)
        .collect();
    let b_connected: Vec<_> = b_snap
        .peer_states
        .iter()
        .filter(|(_, s)| s.phase_kind == PhaseKind::Connected)
        .collect();

    assert_eq!(
        a_connected.len(),
        1,
        "A should have exactly one Connected peer; got {:?}",
        a_connected
    );
    assert_eq!(
        b_connected.len(),
        1,
        "B should have exactly one Connected peer; got {:?}",
        b_connected
    );

    // A's Connected entry must be the Central role (A has higher endpoint).
    let a_peer = a_snap.peer_states.get(&dev_b).expect("A has no entry for B");
    assert_eq!(
        a_peer.phase_kind,
        PhaseKind::Connected,
        "A's entry for B should be Connected"
    );
    assert_eq!(
        a_peer.role,
        ConnectRole::Central,
        "A (higher endpoint) keeps the Central role after dedup"
    );

    // B's Connected entry must be the Peripheral role.
    let b_peer = b_snap.peer_states.get(&dev_a).expect("B has no entry for A");
    assert_eq!(
        b_peer.phase_kind,
        PhaseKind::Connected,
        "B's entry for A should be Connected"
    );
    assert_eq!(
        b_peer.role,
        ConnectRole::Peripheral,
        "B (lower endpoint) keeps the Peripheral role after dedup"
    );
}

