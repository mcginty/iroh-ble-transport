#![cfg(feature = "testing")]

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::task::Waker;
use std::time::{Duration, Instant};

use bytes::Bytes;
use iroh_ble_transport::transport::{
    driver::{Driver, IncomingPacket},
    peer::{ConnectRole, FragmentSource, PeerAction, PeerCommand, PeerPhase, StallCause},
    registry::Registry,
    routing::{Routing, prefix_from_endpoint},
    store::InMemoryPeerStore,
    test_util::{CallKind, MockBleInterface},
    transport::L2capPolicy,
};
use tokio::sync::mpsc;

struct Harness {
    registry: Registry,
    driver: Driver<MockBleInterface>,
    iface: Arc<MockBleInterface>,
    rx: mpsc::Receiver<PeerCommand>,
    _incoming: mpsc::Receiver<IncomingPacket>,
    device: blew::DeviceId,
    endpoint: iroh_base::EndpointId,
}

impl Harness {
    fn new() -> Self {
        let iface = Arc::new(MockBleInterface::new());
        let (tx, rx) = mpsc::channel(64);
        let (incoming_tx, incoming) = mpsc::channel(64);
        let driver = Driver::new(
            Arc::clone(&iface),
            tx,
            incoming_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(InMemoryPeerStore::new()),
            Arc::new(Routing::new()),
        );
        Self {
            registry: Registry::new_for_test_with_policy(L2capPolicy::PreferL2cap),
            driver,
            iface,
            rx,
            _incoming: incoming,
            device: blew::DeviceId::from("lifecycle-peer"),
            endpoint: iroh_base::SecretKey::from_bytes(&[17; 32]).public(),
        }
    }

    async fn command(&mut self, command: PeerCommand) {
        for action in self.registry.handle(command) {
            self.driver.execute(action).await;
        }
    }

    async fn dial(&mut self) -> u64 {
        self.command(PeerCommand::Advertised {
            device_id: self.device.clone(),
            prefix: prefix_from_endpoint(&self.endpoint),
            rssi: None,
        })
        .await;
        self.command(PeerCommand::SendDatagram {
            device_id: self.device.clone(),
            target_endpoint: Some(self.endpoint),
            tx_gen: 0,
            datagram: Bytes::from_static(b"hello"),
            waker: Waker::noop().clone(),
        })
        .await;
        self.lifecycle()
    }

    fn lifecycle(&self) -> u64 {
        self.registry.lifecycle_id(&self.device)
    }

    async fn next(&mut self) -> PeerCommand {
        tokio::time::timeout(Duration::from_secs(2), self.rx.recv())
            .await
            .expect("driver completion timed out")
            .expect("driver inbox closed")
    }

    async fn connected(&mut self) {
        let command = self.next().await;
        assert!(matches!(command, PeerCommand::ConnectSucceeded { .. }));
        self.command(command).await;
        let command = self.next().await;
        assert!(matches!(command, PeerCommand::DataPipeReady { .. }));
        self.command(command).await;
    }

    async fn wait_call(&self, call: CallKind) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !self.iface.calls().contains(&call) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("native call timed out");
    }

    async fn fragment(&mut self, source: FragmentSource) {
        self.command(PeerCommand::InboundGattFragment {
            device_id: self.device.clone(),
            source,
            bytes: Bytes::from_static(b"early"),
        })
        .await;
        let ready = self.next().await;
        assert!(matches!(ready, PeerCommand::DataPipeReady { .. }));
        self.command(ready).await;
    }
}

#[tokio::test]
async fn early_notification_preserves_lifecycle_and_allows_teardown() {
    let mut h = Harness::new();
    h.iface.set_connect_held(true);
    let lifecycle_id = h.dial().await;
    h.wait_call(CallKind::Connect(h.device.clone())).await;
    h.fragment(FragmentSource::CentralReceivedP2c).await;
    assert_eq!(h.lifecycle(), lifecycle_id);
    assert!(matches!(&h.registry.peer(&h.device).unwrap().phase,
        PeerPhase::Connected { channel, .. } if channel.id == 0));

    h.iface.set_connect_held(false);
    let completion = h.next().await;
    assert!(matches!(completion, PeerCommand::ConnectSucceeded { .. }));
    h.command(completion).await;
    h.command(PeerCommand::Stalled {
        device_id: h.device.clone(),
        cause: StallCause::LocalClose,
    })
    .await;
    h.wait_call(CallKind::Disconnect(h.device.clone())).await;
}

#[tokio::test]
async fn inbound_role_replacement_joins_connect_and_rejects_its_completion() {
    let mut h = Harness::new();
    h.iface.set_connect_held(true);
    let old = h.dial().await;
    h.wait_call(CallKind::Connect(h.device.clone())).await;
    h.fragment(FragmentSource::PeripheralReceivedC2p).await;
    let replacement = h.lifecycle();
    assert_ne!(replacement, old);
    assert_eq!(
        h.registry.peer(&h.device).unwrap().role,
        ConnectRole::Peripheral
    );

    h.iface.set_connect_held(false);
    let completion = h.next().await;
    assert!(
        matches!(completion, PeerCommand::ConnectSucceeded { lifecycle_id, .. } if lifecycle_id == old)
    );
    h.command(completion).await;
    assert_eq!(h.lifecycle(), replacement);
    assert!(matches!(
        h.registry.peer(&h.device).unwrap().phase,
        PeerPhase::Connected { .. }
    ));

    // Queue obsolete cleanup after the worker's ownership transfer.
    h.driver
        .execute(PeerAction::RetireLifecycle {
            device_id: h.device.clone(),
            lifecycle_id: old,
        })
        .await;
    h.driver
        .execute(PeerAction::CloseChannel {
            device_id: h.device.clone(),
            lifecycle_id: old,
            channel: iroh_ble_transport::transport::peer::ChannelHandle {
                id: 0,
                path: iroh_ble_transport::transport::peer::ConnectPath::Gatt,
            },
            reason: iroh_ble_transport::transport::peer::DisconnectReason::LocalClose,
        })
        .await;
    h.driver
        .execute(PeerAction::Refresh {
            device_id: h.device.clone(),
            lifecycle_id: replacement,
        })
        .await;
    h.wait_call(CallKind::Refresh(h.device.clone())).await;
    assert!(
        !h.iface
            .calls()
            .contains(&CallKind::Disconnect(h.device.clone()))
    );
}

#[tokio::test]
async fn retirement_cancels_running_version_and_queued_upgrade() {
    for forget in [false, true] {
        let mut h = Harness::new();
        h.iface.set_version_held(true);
        h.dial().await;
        h.connected().await;
        h.wait_call(CallKind::ReadVersion(h.device.clone())).await;
        let old = h.lifecycle();
        h.command(PeerCommand::VerifiedEndpoint {
            endpoint_id: h.endpoint,
            token: None,
        })
        .await;
        assert!(matches!(
            h.registry.peer(&h.device).unwrap().phase,
            PeerPhase::Connected {
                upgrading: true,
                ..
            }
        ));

        let command = if forget {
            PeerCommand::Forget {
                device_id: h.device.clone(),
            }
        } else {
            PeerCommand::AdapterStateChanged { powered: false }
        };
        h.command(command).await;
        assert_ne!(h.lifecycle(), old);
        // This cleanup is a barrier behind the cancelled read and upgrade.
        h.driver
            .execute(PeerAction::Refresh {
                device_id: h.device.clone(),
                lifecycle_id: old,
            })
            .await;
        h.wait_call(CallKind::Refresh(h.device.clone())).await;
        assert!(
            !h.iface
                .calls()
                .contains(&CallKind::ReadPsm(h.device.clone()))
        );
    }
}

#[tokio::test(start_paused = true)]
async fn version_timeout_unblocks_the_lifecycles_upgrade() {
    let mut h = Harness::new();
    h.iface.set_version_held(true);
    h.dial().await;
    h.connected().await;
    h.wait_call(CallKind::ReadVersion(h.device.clone())).await;
    h.command(PeerCommand::VerifiedEndpoint {
        endpoint_id: h.endpoint,
        token: None,
    })
    .await;
    tokio::time::advance(Duration::from_secs(6)).await;
    let completion = h.next().await;
    assert!(matches!(completion, PeerCommand::OpenL2capFailed { .. }));
    h.command(completion).await;
    assert!(
        h.iface
            .calls()
            .contains(&CallKind::ReadPsm(h.device.clone()))
    );
    assert!(matches!(
        h.registry.peer(&h.device).unwrap().phase,
        PeerPhase::Connected {
            upgrading: false,
            ..
        }
    ));
}

#[tokio::test]
async fn peer_gc_cannot_reuse_a_delayed_connects_lifecycle() {
    let mut h = Harness::new();
    h.iface.set_connect_held(true);
    let old = h.dial().await;
    h.wait_call(CallKind::Connect(h.device.clone())).await;
    h.command(PeerCommand::Forget {
        device_id: h.device.clone(),
    })
    .await;
    h.command(PeerCommand::Tick(Instant::now() + Duration::from_secs(61)))
        .await;
    assert!(h.registry.peer(&h.device).is_none());
    let replacement = h.dial().await;
    assert_ne!(replacement, old);
    h.iface.set_connect_held(false);
    let completion = h.next().await;
    assert!(
        matches!(completion, PeerCommand::ConnectSucceeded { lifecycle_id, .. } if lifecycle_id == old)
    );
    h.command(completion).await;
    assert!(matches!(
        h.registry.peer(&h.device).unwrap().phase,
        PeerPhase::Connecting { .. }
    ));
    h.connected().await;
    assert_eq!(h.lifecycle(), replacement);

    // Upgrade operation numbers can repeat; lifecycle identity must still fence them.
    h.command(PeerCommand::VerifiedEndpoint {
        endpoint_id: h.endpoint,
        token: None,
    })
    .await;
    let upgrade_gen = h.registry.upgrade_gen(&h.device);
    h.command(PeerCommand::OpenL2capFailed {
        device_id: h.device.clone(),
        lifecycle_id: old,
        upgrade_gen,
        error: "obsolete open".into(),
    })
    .await;
    assert!(matches!(
        h.registry.peer(&h.device).unwrap().phase,
        PeerPhase::Connected {
            upgrading: true,
            ..
        }
    ));
}
