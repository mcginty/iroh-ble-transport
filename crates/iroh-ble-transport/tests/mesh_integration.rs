#![cfg(feature = "testing")]

//! Multi-node integration tests exercising the registry under concurrent-peer
//! load. Each test spins up 3 nodes sharing a single `MockFabric`, each with
//! its own `Registry + Driver + MockBleInterface`. All tests use
//! `L2capPolicy::Disabled` (GATT-only).

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use arc_swap::ArcSwap;
use atomic_waker::AtomicWaker;
use blew::{BleDevice, DeviceId};
use bytes::Bytes;
use iroh_ble_transport::transport::{
    driver::{Driver, IncomingPacket},
    peer::{KEY_PREFIX_LEN, KeyPrefix, PeerCommand},
    registry::{Registry, SnapshotMaps},
    store::InMemoryPeerStore,
    test_util::MockFabric,
    transport::L2capPolicy,
};
use tokio::sync::mpsc;

fn zero_counters() -> (Arc<AtomicU64>, Arc<AtomicU64>) {
    (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)))
}

fn prefix_for(byte: u8) -> KeyPrefix {
    [byte; KEY_PREFIX_LEN]
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

struct TestNode {
    device_id: DeviceId,
    inbox_tx: mpsc::Sender<PeerCommand>,
    incoming_rx: mpsc::Receiver<IncomingPacket>,
    snapshots: Arc<ArcSwap<SnapshotMaps>>,
}

fn spawn_node(fabric: &MockFabric, device_id: DeviceId, policy: L2capPolicy) -> TestNode {
    let (inbox_tx, inbox_rx) = mpsc::channel::<PeerCommand>(256);
    let (incoming_tx, incoming_rx) = mpsc::channel::<IncomingPacket>(64);
    let snapshots = Arc::new(ArcSwap::from(Arc::new(SnapshotMaps::default())));
    let iface = fabric.add_node(device_id.clone(), inbox_tx.clone());
    let (retransmits, truncations) = zero_counters();

    let driver = Driver::new(
        iface,
        inbox_tx.clone(),
        incoming_tx,
        retransmits,
        truncations,
        Arc::new(InMemoryPeerStore::new()),
    );
    let registry = Registry::new(policy);
    let snap = snapshots.clone();
    tokio::spawn(async move {
        registry
            .run(inbox_rx, driver, snap, Arc::new(AtomicWaker::new()))
            .await;
    });

    TestNode {
        device_id,
        inbox_tx,
        incoming_rx,
        snapshots,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn triangle_send_fans_out_to_both_peers() {
    let fabric = MockFabric::new();
    let a = spawn_node(&fabric, DeviceId::from("a"), L2capPolicy::Disabled);
    let mut b = spawn_node(&fabric, DeviceId::from("b"), L2capPolicy::Disabled);
    let mut c = spawn_node(&fabric, DeviceId::from("c"), L2capPolicy::Disabled);

    a.inbox_tx
        .send(PeerCommand::Advertised {
            prefix: prefix_for(0xB1),
            device: ble_device(&b.device_id),
            rssi: None,
        })
        .await
        .unwrap();
    a.inbox_tx
        .send(PeerCommand::Advertised {
            prefix: prefix_for(0xC1),
            device: ble_device(&c.device_id),
            rssi: None,
        })
        .await
        .unwrap();

    let (waker_tx, _waker_rx) = mpsc::channel::<()>(4);
    let waker = waker_from_channel(waker_tx);
    a.inbox_tx
        .send(PeerCommand::SendDatagram {
            device_id: b.device_id.clone(),
            tx_gen: 0,
            datagram: Bytes::from_static(b"to-b"),
            waker: waker.clone(),
        })
        .await
        .unwrap();
    a.inbox_tx
        .send(PeerCommand::SendDatagram {
            device_id: c.device_id.clone(),
            tx_gen: 0,
            datagram: Bytes::from_static(b"to-c"),
            waker,
        })
        .await
        .unwrap();

    let got_b = tokio::time::timeout(std::time::Duration::from_secs(10), b.incoming_rx.recv())
        .await
        .expect("B never received datagram")
        .expect("B incoming closed");
    let got_c = tokio::time::timeout(std::time::Duration::from_secs(10), c.incoming_rx.recv())
        .await
        .expect("C never received datagram")
        .expect("C incoming closed");

    assert_eq!(got_b.device_id, a.device_id);
    assert_eq!(got_b.data.as_ref(), b"to-b");
    assert_eq!(got_c.device_id, a.device_id);
    assert_eq!(got_c.data.as_ref(), b"to-c");

    let snapshot = a.snapshots.load();
    let state_b = snapshot.peer_states.get(&b.device_id).expect("A has no B");
    let state_c = snapshot.peer_states.get(&c.device_id).expect("A has no C");
    assert!(state_b.tx_gen > 0, "A's tx_gen for B should have advanced");
    assert!(state_c.tx_gen > 0, "A's tx_gen for C should have advanced");
}
