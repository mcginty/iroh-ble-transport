//! Action executor. Translates `PeerAction` into `BleInterface` calls and follow-up `PeerCommand`s on success/failure.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use bytes::Bytes;
use tokio::sync::mpsc;

use crate::transport::interface::BleInterface;
use crate::transport::peer::{PeerAction, PeerCommand};
use crate::transport::pipe::run_data_pipe;
use crate::transport::store::PeerStore;

/// A fully-reassembled datagram delivered up to iroh.
pub struct IncomingPacket {
    pub device_id: blew::DeviceId,
    pub data: Bytes,
}

pub struct Driver<I: BleInterface> {
    iface: Arc<I>,
    inbox: mpsc::Sender<PeerCommand>,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    retransmit_counter: Arc<AtomicU64>,
    truncation_counter: Arc<AtomicU64>,
    store: Arc<dyn PeerStore>,
}

impl<I: BleInterface> Driver<I> {
    pub fn new(
        iface: Arc<I>,
        inbox: mpsc::Sender<PeerCommand>,
        incoming_tx: mpsc::Sender<IncomingPacket>,
        retransmit_counter: Arc<AtomicU64>,
        truncation_counter: Arc<AtomicU64>,
        store: Arc<dyn PeerStore>,
    ) -> Self {
        Self {
            iface,
            inbox,
            incoming_tx,
            retransmit_counter,
            truncation_counter,
            store,
        }
    }

    pub async fn execute(&self, action: PeerAction) {
        match action {
            PeerAction::StartConnect {
                device_id,
                attempt: _,
            } => {
                let iface = Arc::clone(&self.iface);
                let inbox = self.inbox.clone();
                let dev_for_msg = device_id.clone();
                tokio::spawn(async move {
                    match iface.connect(&device_id).await {
                        Ok(channel) => {
                            let _ = inbox
                                .send(PeerCommand::ConnectSucceeded {
                                    device_id: dev_for_msg,
                                    channel,
                                })
                                .await;
                        }
                        Err(e) => {
                            let _ = inbox
                                .send(PeerCommand::ConnectFailed {
                                    device_id: dev_for_msg,
                                    error: format!("{e}"),
                                })
                                .await;
                        }
                    }
                });
            }

            PeerAction::ReadVersion { device_id } => {
                let iface = Arc::clone(&self.iface);
                let inbox = self.inbox.clone();
                let dev_for_msg = device_id.clone();
                tokio::spawn(async move {
                    let want = crate::transport::transport::PROTOCOL_VERSION;
                    match iface.read_version(&device_id).await {
                        Ok(Some(got)) if got != want => {
                            let _ = inbox
                                .send(PeerCommand::ProtocolVersionMismatch {
                                    device_id: dev_for_msg,
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
                });
            }

            PeerAction::OpenL2cap { device_id } => {
                self.spawn_l2cap_open(device_id);
            }

            PeerAction::CloseChannel { device_id, .. } => {
                let iface = Arc::clone(&self.iface);
                tokio::spawn(async move {
                    let _ = iface.disconnect(&device_id).await;
                });
            }

            PeerAction::Refresh { device_id, .. } => {
                let iface = Arc::clone(&self.iface);
                tokio::spawn(async move {
                    let _ = iface.refresh(&device_id).await;
                });
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
                tracing::trace!(metric = %ev, "peer metric");
            }

            PeerAction::StartDataPipe {
                device_id,
                role,
                path,
                l2cap_channel,
            } => {
                tracing::debug!(device = %device_id, ?role, ?path, "StartDataPipe");
                let (outbound_tx, outbound_rx) =
                    mpsc::channel::<crate::transport::peer::PendingSend>(32);
                let (inbound_tx, inbound_rx) = mpsc::channel::<Bytes>(64);
                let iface: Arc<dyn BleInterface> = Arc::clone(&self.iface) as Arc<dyn BleInterface>;
                let incoming_tx = self.incoming_tx.clone();
                let inbox = self.inbox.clone();
                let retransmit_counter = Arc::clone(&self.retransmit_counter);
                let truncation_counter = Arc::clone(&self.truncation_counter);
                let dev_for_ready = device_id.clone();
                tokio::spawn(async move {
                    run_data_pipe(
                        iface,
                        device_id,
                        role,
                        path,
                        l2cap_channel,
                        outbound_rx,
                        inbound_rx,
                        incoming_tx,
                        inbox,
                        retransmit_counter,
                        truncation_counter,
                    )
                    .await;
                });
                let ready = PeerCommand::DataPipeReady {
                    device_id: dev_for_ready,
                    outbound_tx,
                    inbound_tx,
                };
                if self.inbox.send(ready).await.is_err() {
                    tracing::debug!("inbox closed before DataPipeReady forwarded");
                }
            }
            PeerAction::UpgradeToL2cap { device_id } => {
                self.spawn_l2cap_open(device_id);
            }
            PeerAction::SwapPipeToL2cap { .. } | PeerAction::RevertToGattPipe { .. } => {}
        }
    }

    fn spawn_l2cap_open(&self, device_id: blew::DeviceId) {
        let iface = Arc::clone(&self.iface);
        let inbox = self.inbox.clone();
        let dev_for_msg = device_id.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(super::registry::L2CAP_SELECT_TIMEOUT, async {
                let psm = match iface.read_psm(&device_id).await {
                    Ok(Some(psm)) => psm,
                    Ok(None) => return Err("no psm advertised".to_string()),
                    Err(e) => return Err(format!("read_psm: {e}")),
                };
                iface
                    .open_l2cap(&device_id, psm)
                    .await
                    .map_err(|e| format!("{e}"))
            })
            .await;
            match result {
                Ok(Ok(channel)) => {
                    let _ = inbox
                        .send(PeerCommand::OpenL2capSucceeded {
                            device_id: dev_for_msg,
                            channel,
                        })
                        .await;
                }
                Ok(Err(error)) => {
                    let _ = inbox
                        .send(PeerCommand::OpenL2capFailed {
                            device_id: dev_for_msg,
                            error,
                        })
                        .await;
                }
                Err(_elapsed) => {
                    let _ = inbox
                        .send(PeerCommand::OpenL2capFailed {
                            device_id: dev_for_msg,
                            error: "l2cap select timeout".into(),
                        })
                        .await;
                }
            }
        });
    }
}

// ====================== BlewDriver ======================
// Real BleInterface implementation backed by blew::Central + blew::Peripheral.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use blew::central::ScanFilter;
use blew::gatt::service::GattService;
use blew::l2cap::types::Psm;
use blew::peripheral::AdvertisingConfig;
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
}

impl BlewDriver {
    pub fn new(
        central: Arc<Central>,
        peripheral: Arc<Peripheral>,
        services: Vec<GattService>,
        advertising_config: AdvertisingConfig,
    ) -> Self {
        Self {
            central,
            peripheral,
            next_channel_id: AtomicU64::new(1),
            channels_by_device: Mutex::new(HashMap::new()),
            services,
            advertising_config,
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
        self.central.discover_services(device_id).await?;
        self.central
            .subscribe_characteristic(device_id, P2C_CHAR_UUID)
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
            Err(e) => tracing::warn!(device = %device_id, len, err = %e, "write_c2p err"),
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
            Ok(()) => tracing::trace!(device = %device_id, len, "notify_p2c ok"),
            Err(e) => tracing::warn!(device = %device_id, len, err = %e, "notify_p2c err"),
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
        // Re-opening requires plumbing the fresh listener stream back to the
        // accept supervisor and publishing the new PSM to the peripheral read
        // responder; neither is wired yet. Log for now.
        tracing::warn!("restart_l2cap_listener: not yet implemented end-to-end");
        Ok(None)
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

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;
    use crate::transport::test_util::{CallKind, MockBleInterface};
    use bytes::Bytes;

    #[test]
    fn incoming_packet_carries_device_id() {
        let pkt = IncomingPacket {
            device_id: blew::DeviceId::from("test"),
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
            Arc::new(crate::transport::store::InMemoryPeerStore::new()),
        );

        driver
            .execute(PeerAction::StartDataPipe {
                device_id: blew::DeviceId::from("start-pipe"),
                role: ConnectRole::Central,
                path: ConnectPath::Gatt,
                l2cap_channel: None,
            })
            .await;

        let cmd = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match cmd {
            PeerCommand::DataPipeReady { device_id, .. } => {
                assert_eq!(device_id, blew::DeviceId::from("start-pipe"));
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
            Arc::new(crate::transport::store::InMemoryPeerStore::new()),
        );

        driver
            .execute(PeerAction::StartDataPipe {
                device_id: blew::DeviceId::from("start-pipe-peri"),
                role: ConnectRole::Peripheral,
                path: ConnectPath::Gatt,
                l2cap_channel: None,
            })
            .await;

        let cmd = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match cmd {
            PeerCommand::DataPipeReady { device_id, .. } => {
                assert_eq!(device_id, blew::DeviceId::from("start-pipe-peri"));
            }
            other => panic!("expected DataPipeReady, got {other:?}"),
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
            Arc::new(crate::transport::store::InMemoryPeerStore::new()),
        );

        driver
            .execute(PeerAction::UpgradeToL2cap {
                device_id: device_id.clone(),
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
            Arc::new(crate::transport::store::InMemoryPeerStore::new()),
        );
        let device_id = blew::DeviceId::from("x");
        driver
            .execute(PeerAction::StartConnect {
                device_id: device_id.clone(),
                attempt: 0,
            })
            .await;
        let cmd = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(cmd, PeerCommand::ConnectSucceeded { .. }));
        iface.assert_called(&CallKind::Connect(device_id));
    }
}
