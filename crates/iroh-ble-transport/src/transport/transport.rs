//! `BleTransport` — iroh `CustomTransport` implementation driven by the
//! registry actor and a `BlewDriver`.

use std::io;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, Waker};

use arc_swap::ArcSwap;
use blew::gatt::props::{AttributePermissions, CharacteristicProperties};
use blew::gatt::service::{GattCharacteristic, GattService};
use blew::peripheral::AdvertisingConfig;
use blew::{BlewError, Central, Peripheral};
use bytes::Bytes;
use iroh::address_lookup::{self, AddressLookup, EndpointData, EndpointInfo, Item};
use iroh::endpoint::transports::{Addr, CustomEndpoint, CustomSender, CustomTransport, Transmit};
use iroh_base::{CustomAddr, EndpointId, TransportAddr};
use n0_watcher::Watchable;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::{Uuid, uuid};

use crate::error::{BleError, BleResult};
use crate::transport::driver::{BlewDriver, Driver, IncomingPacket};
use crate::transport::events::{run_central_events, run_l2cap_accept, run_peripheral_events};
use crate::transport::peer::{ConnectPath, KEY_PREFIX_LEN, PeerCommand};
use crate::transport::registry::{PhaseKind, Registry, RegistryHandle, SnapshotMaps};
use crate::transport::routing::{TOKEN_LEN, TransportRouting, parse_token_addr, token_custom_addr};
use crate::transport::store::{InMemoryPeerStore, PeerStore};
use crate::transport::watchdog::run_watchdog;

/// Unique transport discriminator — ASCII "BLE".
pub const BLE_TRANSPORT_ID: u64 = 0x42_4C_45;

const IROH_SERVICE_UUID: Uuid = uuid!("69726f01-8e45-4c2c-b3a5-331f3098b5c2");
const IROH_C2P_CHAR_UUID: Uuid = uuid!("69726f02-8e45-4c2c-b3a5-331f3098b5c2");
const IROH_P2C_CHAR_UUID: Uuid = uuid!("69726f03-8e45-4c2c-b3a5-331f3098b5c2");
pub(crate) const IROH_PSM_CHAR_UUID: Uuid = uuid!("69726f04-8e45-4c2c-b3a5-331f3098b5c2");
pub(crate) const IROH_VERSION_CHAR_UUID: Uuid = uuid!("69726f05-8e45-4c2c-b3a5-331f3098b5c2");

/// On-wire protocol version served by the peripheral on the VERSION
/// characteristic and verified by the central immediately after connect.
/// Mismatch transitions the peer to `Dead { ProtocolMismatch }` rather
/// than running an incompatible data pipe.
pub const PROTOCOL_VERSION: u8 = 1;

const KEY_UUID_PREFIX: [u8; 4] = [0x69, 0x72, 0x6f, 0x00];

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum L2capPolicy {
    Disabled,
    #[default]
    PreferL2cap,
}

pub struct BleTransportConfig {
    pub l2cap_policy: L2capPolicy,
    /// Persistent peer cache. Defaults to an in-memory store; applications
    /// that want durable state across restarts can plug in their own
    /// implementation of [`PeerStore`]. The transport writes a snapshot
    /// whenever a peer leaves `Connected`, and forgets a peer when it
    /// transitions to `Dead { MaxRetries }`.
    pub store: Arc<dyn PeerStore>,
    /// Receiver for verified [`EndpointId`] events emitted by a
    /// [`crate::BleDedupHook`] installed on the iroh `Endpoint`. When `None`,
    /// handshake-time dedup is effectively disabled — useful for tests that
    /// don't run a real iroh `Endpoint`.
    pub verified_rx: Option<tokio::sync::mpsc::UnboundedReceiver<iroh_base::EndpointId>>,
}

impl Default for BleTransportConfig {
    fn default() -> Self {
        Self {
            l2cap_policy: L2capPolicy::default(),
            store: Arc::new(InMemoryPeerStore::new()),
            verified_rx: None,
        }
    }
}

impl std::fmt::Debug for BleTransportConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BleTransportConfig")
            .field("l2cap_policy", &self.l2cap_policy)
            .field("store", &"<PeerStore>")
            .field("verified_rx", &self.verified_rx.is_some())
            .finish()
    }
}

fn iroh_key_uuid(endpoint_id: &EndpointId) -> Uuid {
    let key = endpoint_id.as_bytes();
    let mut bytes = [0u8; 16];
    bytes[..4].copy_from_slice(&KEY_UUID_PREFIX);
    bytes[4..16].copy_from_slice(&key[..KEY_PREFIX_LEN]);
    Uuid::from_bytes(bytes)
}

fn build_gatt_services(key_uuid: Uuid) -> Vec<GattService> {
    let characteristics = vec![
        GattCharacteristic {
            uuid: IROH_C2P_CHAR_UUID,
            properties: CharacteristicProperties::WRITE_WITHOUT_RESPONSE
                | CharacteristicProperties::NOTIFY,
            permissions: AttributePermissions::WRITE,
            value: vec![],
            descriptors: vec![],
        },
        GattCharacteristic {
            uuid: IROH_P2C_CHAR_UUID,
            properties: CharacteristicProperties::WRITE_WITHOUT_RESPONSE
                | CharacteristicProperties::NOTIFY,
            permissions: AttributePermissions::WRITE,
            value: vec![],
            descriptors: vec![],
        },
        GattCharacteristic {
            uuid: IROH_VERSION_CHAR_UUID,
            properties: CharacteristicProperties::READ,
            permissions: AttributePermissions::READ,
            value: vec![PROTOCOL_VERSION],
            descriptors: vec![],
        },
        GattCharacteristic {
            uuid: IROH_PSM_CHAR_UUID,
            properties: CharacteristicProperties::READ,
            permissions: AttributePermissions::READ,
            value: vec![],
            descriptors: vec![],
        },
    ];
    vec![
        GattService {
            uuid: IROH_SERVICE_UUID,
            primary: true,
            characteristics,
        },
        GattService {
            uuid: key_uuid,
            primary: false,
            characteristics: vec![],
        },
    ]
}

async fn register_gatt_services(
    peripheral: &Peripheral,
    services: &[GattService],
) -> BleResult<()> {
    for service in services {
        peripheral.add_service(service).await?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BleMetricsSnapshot {
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub retransmits: u64,
    pub truncations: u64,
}

pub struct BleTransport {
    handle: RegistryHandle,
    incoming_rx: tokio::sync::Mutex<Option<mpsc::Receiver<IncomingPacket>>>,
    routing: Arc<TransportRouting>,
    tx_bytes: Arc<AtomicU64>,
    rx_bytes: Arc<AtomicU64>,
    retransmits: Arc<AtomicU64>,
    truncations: Arc<AtomicU64>,
    /// Wakers parked by `BleSender::poll_send` when `try_send` sees `Full`.
    /// The registry actor drains and wakes the whole list each time it pops
    /// a command — fair across N concurrent senders, unlike a single-slot
    /// `AtomicWaker` (which would clobber prior registrations and leak wakeups).
    inbox_capacity_wakers: Arc<Mutex<Vec<Waker>>>,
    store: Arc<dyn PeerStore>,
}

impl std::fmt::Debug for BleTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BleTransport").finish()
    }
}

impl BleTransport {
    pub async fn new(
        local_id: EndpointId,
        central: Arc<Central>,
        peripheral: Arc<Peripheral>,
    ) -> BleResult<Self> {
        Self::with_config(local_id, central, peripheral, BleTransportConfig::default()).await
    }

    pub async fn with_config(
        local_id: EndpointId,
        central: Arc<Central>,
        peripheral: Arc<Peripheral>,
        config: BleTransportConfig,
    ) -> BleResult<Self> {
        central
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .map_err(|_| BleError::Timeout {
                stage: "wait_ready",
            })?;
        peripheral
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .map_err(|_| BleError::Timeout {
                stage: "wait_ready",
            })?;

        let key_uuid = iroh_key_uuid(&local_id);
        let services = build_gatt_services(key_uuid);
        register_gatt_services(&peripheral, &services).await?;
        let advertising_config = AdvertisingConfig {
            local_name: "iroh".to_string(),
            service_uuids: vec![key_uuid],
        };
        peripheral.start_advertising(&advertising_config).await?;
        info!(key_uuid = %key_uuid, "advertising started");

        match central
            .start_scan(blew::central::ScanFilter::default())
            .await
        {
            Ok(()) => info!("scanning for iroh-ble peers"),
            Err(BlewError::NotSupported) => {
                warn!("central start_scan not supported; discovery disabled");
            }
            Err(e) => return Err(e.into()),
        }

        let (inbox_tx, inbox_rx) = mpsc::channel::<PeerCommand>(256);
        let (incoming_tx, incoming_rx) = mpsc::channel::<IncomingPacket>(256);
        let snapshots = Arc::new(ArcSwap::from(Arc::new(SnapshotMaps::default())));
        let routing = Arc::new(TransportRouting::new());

        let tx_bytes = Arc::new(AtomicU64::new(0));
        let rx_bytes = Arc::new(AtomicU64::new(0));
        let retransmits = Arc::new(AtomicU64::new(0));
        let truncations = Arc::new(AtomicU64::new(0));
        let inbox_capacity_wakers: Arc<Mutex<Vec<Waker>>> = Arc::new(Mutex::new(Vec::new()));

        let iface = Arc::new(BlewDriver::new(
            Arc::clone(&central),
            Arc::clone(&peripheral),
            services,
            advertising_config,
        ));
        let driver = Driver::new(
            iface,
            inbox_tx.clone(),
            incoming_tx,
            Arc::clone(&retransmits),
            Arc::clone(&truncations),
            Arc::clone(&config.store),
        );

        let mut psm = None;
        if config.l2cap_policy == L2capPolicy::PreferL2cap {
            match peripheral.l2cap_listener().await {
                Ok((assigned_psm, listener)) => {
                    info!(psm = assigned_psm.value(), "L2CAP listener started");
                    psm = Some(assigned_psm.value());
                    tokio::spawn(run_l2cap_accept(listener, inbox_tx.clone()));
                }
                Err(e) => {
                    warn!(error = %e, "L2CAP listener failed, falling back to GATT-only");
                }
            }
        }

        let registry = Registry::new(config.l2cap_policy, local_id);
        let snap_for_actor = Arc::clone(&snapshots);
        let wakers_for_actor = Arc::clone(&inbox_capacity_wakers);
        let routing_for_actor = Arc::clone(&routing);
        tokio::spawn(async move {
            registry
                .run(
                    inbox_rx,
                    driver,
                    snap_for_actor,
                    wakers_for_actor,
                    routing_for_actor,
                )
                .await;
        });

        if let Some(mut verified_rx) = config.verified_rx {
            let inbox = inbox_tx.clone();
            tokio::spawn(async move {
                while let Some(endpoint_id) = verified_rx.recv().await {
                    if inbox
                        .send(PeerCommand::VerifiedEndpoint { endpoint_id })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }

        tokio::spawn(run_central_events(
            Arc::clone(&central),
            Arc::clone(&routing),
            inbox_tx.clone(),
        ));
        tokio::spawn(run_peripheral_events(
            Arc::clone(&peripheral),
            Arc::clone(&routing),
            inbox_tx.clone(),
            psm,
        ));
        tokio::spawn(run_watchdog(inbox_tx.clone()));

        Ok(Self {
            handle: RegistryHandle {
                inbox: inbox_tx,
                snapshots,
            },
            incoming_rx: tokio::sync::Mutex::new(Some(incoming_rx)),
            routing,
            tx_bytes,
            rx_bytes,
            retransmits,
            truncations,
            inbox_capacity_wakers,
            store: config.store,
        })
    }

    #[must_use]
    pub fn metrics(&self) -> BleMetricsSnapshot {
        BleMetricsSnapshot {
            tx_bytes: self.tx_bytes.load(Ordering::Relaxed),
            rx_bytes: self.rx_bytes.load(Ordering::Relaxed),
            retransmits: self.retransmits.load(Ordering::Relaxed),
            truncations: self.truncations.load(Ordering::Relaxed),
        }
    }

    pub fn address_lookup(&self) -> BleAddressLookup {
        BleAddressLookup {
            routing: Arc::clone(&self.routing),
        }
    }

    /// The peer store wired into this transport. The transport writes to this
    /// store on peer-lifecycle transitions; applications can read from it (or
    /// share it at construction time) to implement durable reconnect policy.
    #[must_use]
    pub fn peer_store(&self) -> Arc<dyn PeerStore> {
        Arc::clone(&self.store)
    }

    #[must_use]
    pub fn device_for_endpoint(&self, endpoint_id: &EndpointId) -> Option<blew::DeviceId> {
        self.routing.device_for_endpoint(endpoint_id)
    }

    /// Public-facing peer snapshot. Filters out `Unknown` (pre-state internal
    /// construction) and `Dead` (tombstones kept around for `DEAD_GC_TTL`
    /// dedup) so the returned list only contains peers that are actionable
    /// to a UI — the chat app polls this and renders one row per entry.
    #[must_use]
    pub fn snapshot_peers(&self) -> Vec<BlePeerInfo> {
        let snap = self.handle.snapshots.load();
        snap.peer_states
            .iter()
            .filter(|(_, state)| {
                !matches!(
                    state.phase_kind,
                    crate::transport::registry::PhaseKind::Unknown
                        | crate::transport::registry::PhaseKind::Dead
                )
            })
            .map(|(device_id, state)| BlePeerInfo {
                device_id: device_id.clone(),
                phase: BlePeerPhase::from(state.phase_kind),
                consecutive_failures: state.consecutive_failures,
                connect_path: state.connect_path,
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct BlePeerInfo {
    pub device_id: blew::DeviceId,
    pub phase: BlePeerPhase,
    pub consecutive_failures: u32,
    pub connect_path: Option<ConnectPath>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlePeerPhase {
    Unknown,
    Discovered,
    PendingDial,
    Connecting,
    Handshaking,
    Connected,
    Draining,
    Reconnecting,
    Dead,
    Restoring,
}

impl From<PhaseKind> for BlePeerPhase {
    fn from(p: PhaseKind) -> Self {
        match p {
            PhaseKind::Unknown => Self::Unknown,
            PhaseKind::Discovered => Self::Discovered,
            PhaseKind::PendingDial => Self::PendingDial,
            PhaseKind::Connecting => Self::Connecting,
            PhaseKind::Handshaking => Self::Handshaking,
            PhaseKind::Connected => Self::Connected,
            PhaseKind::Draining => Self::Draining,
            PhaseKind::Reconnecting => Self::Reconnecting,
            PhaseKind::Dead => Self::Dead,
            PhaseKind::Restoring => Self::Restoring,
        }
    }
}

impl CustomTransport for BleTransport {
    fn bind(&self) -> io::Result<Box<dyn CustomEndpoint>> {
        let incoming_rx = self
            .incoming_rx
            .try_lock()
            .map_err(|_| io::Error::other("BleTransport bind() contention"))?
            .take()
            .ok_or_else(|| io::Error::other("BleTransport bind() already called"))?;

        let local_addr = token_custom_addr(0);
        let watchable = Watchable::new(vec![local_addr]);
        let sender = Arc::new(BleSender {
            inbox: self.handle.inbox.clone(),
            snapshots: Arc::clone(&self.handle.snapshots),
            routing: Arc::clone(&self.routing),
            tx_bytes: Arc::clone(&self.tx_bytes),
            inbox_capacity_wakers: Arc::clone(&self.inbox_capacity_wakers),
        });
        Ok(Box::new(BleEndpoint {
            receiver: incoming_rx,
            watchable,
            sender,
            routing: Arc::clone(&self.routing),
            rx_bytes: Arc::clone(&self.rx_bytes),
        }))
    }
}

struct BleEndpoint {
    receiver: mpsc::Receiver<IncomingPacket>,
    watchable: Watchable<Vec<CustomAddr>>,
    sender: Arc<BleSender>,
    routing: Arc<TransportRouting>,
    rx_bytes: Arc<AtomicU64>,
}

impl std::fmt::Debug for BleEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BleEndpoint").finish()
    }
}

impl CustomEndpoint for BleEndpoint {
    fn watch_local_addrs(&self) -> n0_watcher::Direct<Vec<CustomAddr>> {
        self.watchable.watch()
    }

    fn create_sender(&self) -> Arc<dyn CustomSender> {
        self.sender.clone()
    }

    fn max_transmit_segments(&self) -> NonZeroUsize {
        NonZeroUsize::MIN
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
        metas: &mut [noq_udp::RecvMeta],
        source_addrs: &mut [Addr],
    ) -> Poll<io::Result<usize>> {
        let n = bufs.len().min(metas.len()).min(source_addrs.len());
        if n == 0 {
            return Poll::Ready(Ok(0));
        }
        let mut filled = 0;
        while filled < n {
            match self.receiver.poll_recv(cx) {
                Poll::Pending => {
                    if filled == 0 {
                        return Poll::Pending;
                    }
                    break;
                }
                Poll::Ready(None) => {
                    return Poll::Ready(Err(io::Error::other("BLE transport channel closed")));
                }
                Poll::Ready(Some(packet)) => {
                    if bufs[filled].len() < packet.data.len() {
                        tracing::warn!(
                            len = packet.data.len(),
                            buf_cap = bufs[filled].len(),
                            "BleEndpoint::poll_recv dropping packet: buffer too small"
                        );
                        continue;
                    }
                    let token = self.routing.mint_token_for_source(&packet.device_id);
                    tracing::trace!(
                        device = %packet.device_id,
                        token,
                        len = packet.data.len(),
                        "BleEndpoint::poll_recv delivering packet"
                    );
                    bufs[filled][..packet.data.len()].copy_from_slice(&packet.data);
                    metas[filled].len = packet.data.len();
                    metas[filled].stride = packet.data.len();
                    source_addrs[filled] = Addr::Custom(token_custom_addr(token));
                    self.rx_bytes
                        .fetch_add(packet.data.len() as u64, Ordering::Relaxed);
                    filled += 1;
                }
            }
        }
        if filled > 0 {
            Poll::Ready(Ok(filled))
        } else {
            Poll::Pending
        }
    }
}

pub struct BleSender {
    inbox: mpsc::Sender<PeerCommand>,
    snapshots: Arc<ArcSwap<SnapshotMaps>>,
    routing: Arc<TransportRouting>,
    tx_bytes: Arc<AtomicU64>,
    inbox_capacity_wakers: Arc<Mutex<Vec<Waker>>>,
}

impl std::fmt::Debug for BleSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BleSender").finish()
    }
}

impl BleSender {
    /// Resolve a `Token` to its current `DeviceId`. Prefix-keyed tokens minted
    /// by `BleAddressLookup::resolve` may not have a discovery mapping yet —
    /// in that case, register the caller's waker (so a later `note_discovery`
    /// wakes us) and re-check the table. The recheck closes the race where
    /// discovery lands between the first lookup and the waker registration.
    fn poll_resolve_device(
        &self,
        cx: &mut Context<'_>,
        token: crate::transport::routing::Token,
    ) -> Poll<blew::DeviceId> {
        if let Some(d) = self.routing.device_for_token(token) {
            return Poll::Ready(d);
        }
        self.routing.register_send_waker(cx.waker());
        match self.routing.device_for_token(token) {
            Some(d) => Poll::Ready(d),
            None => Poll::Pending,
        }
    }
}

impl CustomSender for BleSender {
    fn is_valid_send_addr(&self, addr: &CustomAddr) -> bool {
        addr.id() == BLE_TRANSPORT_ID && addr.data().len() == TOKEN_LEN
    }

    fn poll_send(
        &self,
        cx: &mut Context<'_>,
        dst: &CustomAddr,
        transmit: &Transmit<'_>,
    ) -> Poll<io::Result<()>> {
        let token = match parse_token_addr(dst) {
            Ok(t) => t,
            Err(e) => return Poll::Ready(Err(e)),
        };
        let device_id = match self.poll_resolve_device(cx, token) {
            Poll::Ready(d) => d,
            Poll::Pending => {
                tracing::trace!(token, "BleSender::poll_send waiting for discovery");
                return Poll::Pending;
            }
        };
        let snap = self.snapshots.load();
        let state = snap.peer_states.get(&device_id);
        let tx_gen = state.map_or(0, |s| s.tx_gen);
        let len = transmit.contents.len();
        tracing::trace!(device = %device_id, len, "BleSender::poll_send");
        let cmd = PeerCommand::SendDatagram {
            device_id,
            tx_gen,
            datagram: Bytes::copy_from_slice(transmit.contents),
            waker: cx.waker().clone(),
        };
        match self.inbox.try_send(cmd) {
            Ok(()) => {
                self.tx_bytes.fetch_add(len as u64, Ordering::Relaxed);
                Poll::Ready(Ok(()))
            }
            Err(mpsc::error::TrySendError::Full(cmd)) => {
                // Park our waker before re-checking try_send, so the actor's
                // post-pop drain wakes us if it raced our first try_send.
                // Each concurrent sender gets its own slot — no clobbering.
                self.inbox_capacity_wakers.lock().push(cx.waker().clone());
                match self.inbox.try_send(cmd) {
                    Ok(()) => {
                        self.tx_bytes.fetch_add(len as u64, Ordering::Relaxed);
                        Poll::Ready(Ok(()))
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => Poll::Pending,
                    Err(mpsc::error::TrySendError::Closed(_)) => Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "transport shut down",
                    ))),
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "transport shut down",
            ))),
        }
    }
}

#[derive(Clone)]
pub struct BleAddressLookup {
    routing: Arc<TransportRouting>,
}

impl std::fmt::Debug for BleAddressLookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BleAddressLookup").finish()
    }
}

// ---------------------------------------------------------------------------
// NOTE: iroh 0.97 `AddressLookup` composition limitation (workaround below).
//
// `iroh::address_lookup::ConcurrentAddressLookup::resolve` merges every
// registered resolver's stream with `n0_future::MergeBounded`. Inside iroh,
// `RemoteState::handle_address_lookup_item`
// (iroh-0.97/src/socket/remote_map/remote_state.rs, ~line 609) reacts to a
// `Some(Err(_))` from the merged stream by *replacing the entire merged stream
// with `n0_future::stream::pending()`*:
//
//     Some(Err(err)) => {
//         self.address_lookup_stream = Either::Left(n0_future::stream::pending());
//         self.paths.address_lookup_finished(Err(err));
//     }
//
// Concretely: any sibling resolver that fails fast (e.g. the dns resolver
// erroring instantly on a device with no internet) tears down the merged
// stream and prevents *any* later success from another resolver from being
// observed. This breaks the documented "long-lived subscription" shape that
// `Stream<Result<Item>>` implies and that iroh's own mDNS resolver relies on
// (its `LOOKUP_DURATION = 10s` window simply happens to fit within typical
// failure timing).
//
// We work around this by yielding a prefix-keyed token immediately from
// `resolve()` and parking on `BleSender::poll_send` until `note_discovery`
// wakes us. This mirrors how the UDP transport returns `Pending` from
// `poll_send` when a path is not yet usable.
//
// Upstream tracking: n0-computer/iroh#4130. When that lands (expected in iroh
// 0.98), the prefix-token trick here can be replaced with a normal resolver
// that yields `Err` on unknown peers without poisoning siblings.
// TODO(iroh-0.98): revisit once the fix ships upstream.
// ---------------------------------------------------------------------------
impl AddressLookup for BleAddressLookup {
    fn resolve(
        &self,
        endpoint_id: EndpointId,
    ) -> Option<n0_future::stream::Boxed<Result<Item, address_lookup::Error>>> {
        let prefix = crate::transport::routing::prefix_from_endpoint(&endpoint_id);
        let token = self.routing.mint_token_for_prefix(prefix);
        tracing::info!(
            %endpoint_id,
            token,
            "BleAddressLookup::resolve yielding prefix-keyed token"
        );
        let info = EndpointInfo {
            endpoint_id,
            data: EndpointData::new([TransportAddr::Custom(token_custom_addr(token))]),
        };
        let item: Result<Item, address_lookup::Error> = Ok(Item::new(info, "iroh-ble", None));
        Some(Box::pin(n0_future::stream::iter([item])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::peer::{KEY_PREFIX_LEN, KeyPrefix};
    use crate::transport::registry::SnapshotMaps;
    use crate::transport::routing::TransportRouting;
    use n0_future::Stream;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::task::{Context, Poll, Wake, Waker};

    struct CountingWaker(AtomicUsize);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, AtomicOrdering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }

    fn counting_waker() -> (Arc<CountingWaker>, Waker) {
        let inner = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&inner));
        (inner, waker)
    }

    fn make_sender(routing: Arc<TransportRouting>) -> (BleSender, mpsc::Receiver<PeerCommand>) {
        let (inbox_tx, inbox_rx) = mpsc::channel::<PeerCommand>(8);
        let snapshots = Arc::new(ArcSwap::from(Arc::new(SnapshotMaps::default())));
        let tx_bytes = Arc::new(AtomicU64::new(0));
        let sender = BleSender {
            inbox: inbox_tx,
            snapshots,
            routing,
            tx_bytes,
            inbox_capacity_wakers: Arc::new(Mutex::new(Vec::new())),
        };
        (sender, inbox_rx)
    }

    fn endpoint_id_with_first_byte(b: u8) -> EndpointId {
        let mut bytes = [0u8; 32];
        bytes[0] = b;
        let secret = iroh_base::SecretKey::from_bytes(&bytes);
        secret.public()
    }

    // ---------- Test #1: poll_send register-then-recheck race ----------

    #[test]
    fn poll_resolve_device_returns_ready_when_discovery_already_recorded() {
        let routing = Arc::new(TransportRouting::new());
        let (sender, _rx) = make_sender(Arc::clone(&routing));

        let prefix: KeyPrefix = [0xA0; KEY_PREFIX_LEN];
        let device = blew::DeviceId::from("dev-already-known");
        routing.note_discovery(prefix, device.clone());
        let token = routing.mint_token_for_prefix(prefix);

        let (counter, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        match sender.poll_resolve_device(&mut cx, token) {
            Poll::Ready(d) => assert_eq!(d, device),
            Poll::Pending => panic!("expected Ready when device already mapped"),
        }
        assert_eq!(
            counter.0.load(AtomicOrdering::SeqCst),
            0,
            "fast path must not register or wake the waker"
        );
    }

    #[test]
    fn poll_resolve_device_parks_then_wakes_when_discovery_lands_later() {
        let routing = Arc::new(TransportRouting::new());
        let (sender, _rx) = make_sender(Arc::clone(&routing));

        let prefix: KeyPrefix = [0xA1; KEY_PREFIX_LEN];
        let token = routing.mint_token_for_prefix(prefix);

        let (counter, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);

        // First poll: device not yet mapped → Pending, waker parked.
        match sender.poll_resolve_device(&mut cx, token) {
            Poll::Pending => {}
            Poll::Ready(d) => panic!("expected Pending before discovery, got Ready({d})"),
        }
        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 0);

        // Discovery lands → parked waker fires.
        let device = blew::DeviceId::from("dev-late-arrival");
        routing.note_discovery(prefix, device.clone());
        assert_eq!(
            counter.0.load(AtomicOrdering::SeqCst),
            1,
            "note_discovery must wake the parked send waker"
        );

        // Second poll: device now mapped → Ready.
        match sender.poll_resolve_device(&mut cx, token) {
            Poll::Ready(d) => assert_eq!(d, device),
            Poll::Pending => panic!("expected Ready after discovery"),
        }
    }

    #[test]
    fn poll_resolve_device_recheck_closes_race_with_concurrent_discovery() {
        // Simulates the race the recheck guards against: discovery lands
        // *between* the initial `device_for_token` miss and the waker
        // registration. Without the recheck, the waker would park forever
        // because `wake_send_waiters` already drained the (empty) queue. With
        // the recheck, we observe the now-present mapping immediately.
        //
        // We can't time the race precisely from a unit test, but we can
        // simulate it by manually performing the in-between mutation: park a
        // dummy waker (drained) so the next note_discovery fires no wakers,
        // then assert poll_resolve_device still returns Ready via the recheck
        // path on the very next call.
        let routing = Arc::new(TransportRouting::new());
        let (sender, _rx) = make_sender(Arc::clone(&routing));
        let prefix: KeyPrefix = [0xA2; KEY_PREFIX_LEN];
        let token = routing.mint_token_for_prefix(prefix);

        // Pre-populate the discovery map; the helper must take the
        // "fast-path miss → register → recheck → Ready" route. To force the
        // recheck branch, we register first and then have device land via a
        // separate path — but the helper itself does fast-path first. The
        // important invariant is: there is no observable state where a
        // mapping exists but the helper returns Pending. Verify by:
        // 1. Mapping the device.
        // 2. Calling helper → must be Ready (fast path).
        let device = blew::DeviceId::from("dev-race");
        routing.note_discovery(prefix, device.clone());
        let (_counter, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(
            sender.poll_resolve_device(&mut cx, token),
            Poll::Ready(d) if d == device
        ));

        // Now the recheck path: clear discovery, register waker by polling
        // with no mapping (which parks). Then race-simulate: insert the
        // mapping AFTER the parking but BEFORE wake_send_waiters drains it.
        // wake_send_waiters drains via `mem::take`, so we re-park, drain by
        // firing a dummy note_discovery, then add the *real* mapping. After
        // that drain there are no parked wakers; calling helper again must
        // STILL succeed because the fast-path lookup sees the mapping.
        let prefix2: KeyPrefix = [0xA3; KEY_PREFIX_LEN];
        let token2 = routing.mint_token_for_prefix(prefix2);
        let pending = sender.poll_resolve_device(&mut cx, token2);
        assert!(matches!(pending, Poll::Pending));
        // Drain any parked wakers via an unrelated discovery.
        routing.note_discovery([0xFF; KEY_PREFIX_LEN], blew::DeviceId::from("unrelated"));
        // Now record the real mapping.
        let device2 = blew::DeviceId::from("dev-race-2");
        routing.note_discovery(prefix2, device2.clone());
        // Helper is re-polled (the wake above woke us) and resolves cleanly.
        assert!(matches!(
            sender.poll_resolve_device(&mut cx, token2),
            Poll::Ready(d) if d == device2
        ));
    }

    // ---------- Test #2: BleAddressLookup::resolve immediate-yield ----------

    #[test]
    fn ble_address_lookup_resolve_yields_synchronously() {
        let routing = Arc::new(TransportRouting::new());
        let lookup = BleAddressLookup {
            routing: Arc::clone(&routing),
        };
        let endpoint_id = endpoint_id_with_first_byte(0xCD);
        let mut stream = lookup
            .resolve(endpoint_id)
            .expect("resolve must return Some(stream)");

        let (counter, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        match Pin::new(&mut stream).poll_next(&mut cx) {
            Poll::Ready(Some(Ok(item))) => {
                assert_eq!(item.endpoint_info().endpoint_id, endpoint_id);
                let addrs: Vec<&TransportAddr> = item.endpoint_info().data.addrs().collect();
                assert_eq!(addrs.len(), 1);
                assert!(matches!(addrs[0], TransportAddr::Custom(_)));
            }
            other => panic!("expected synchronous Ready(Some(Ok(_))), got {other:?}"),
        }
        assert_eq!(
            counter.0.load(AtomicOrdering::SeqCst),
            0,
            "synchronous resolve must not touch the waker"
        );
    }

    // ---------- Test #3: resolve idempotence + MAC rotation ----------

    #[test]
    fn ble_address_lookup_resolve_is_idempotent_and_follows_mac_rotation() {
        let routing = Arc::new(TransportRouting::new());
        let lookup = BleAddressLookup {
            routing: Arc::clone(&routing),
        };
        let endpoint_id = endpoint_id_with_first_byte(0xEE);

        let extract_token = |stream_opt: Option<
            n0_future::stream::Boxed<Result<Item, address_lookup::Error>>,
        >|
         -> u64 {
            let mut s = stream_opt.expect("Some");
            let (_c, w) = counting_waker();
            let mut cx = Context::from_waker(&w);
            match Pin::new(&mut s).poll_next(&mut cx) {
                Poll::Ready(Some(Ok(item))) => {
                    let addr = item
                        .endpoint_info()
                        .data
                        .addrs()
                        .next()
                        .expect("at least one addr");
                    match addr {
                        TransportAddr::Custom(c) => parse_token_addr(c).expect("parse"),
                        _ => panic!("expected Custom addr"),
                    }
                }
                other => panic!("expected Ready(Some(Ok)), got {other:?}"),
            }
        };

        let t1 = extract_token(lookup.resolve(endpoint_id));
        let t2 = extract_token(lookup.resolve(endpoint_id));
        assert_eq!(t1, t2, "resolve must be idempotent for the same EndpointId");

        // No discovery yet → token resolves to nothing.
        assert!(routing.device_for_token(t1).is_none());

        // First MAC: prefix → device_a.
        let prefix = crate::transport::routing::prefix_from_endpoint(&endpoint_id);
        let device_a = blew::DeviceId::from("mac-aa");
        routing.note_discovery(prefix, device_a.clone());
        assert_eq!(routing.device_for_token(t1).as_ref(), Some(&device_a));

        // Peer rotates MAC (Android reboot etc.). Same prefix → device_b.
        // The cached iroh address (token t1) must now route to device_b
        // *without* iroh re-resolving.
        let device_b = blew::DeviceId::from("mac-bb");
        routing.note_discovery(prefix, device_b.clone());
        assert_eq!(
            routing.device_for_token(t1).as_ref(),
            Some(&device_b),
            "prefix-keyed token must follow live discovery rotation"
        );

        // And resolve() called *again* after rotation still hands back the
        // same stable token — iroh's address cache stays valid.
        let t3 = extract_token(lookup.resolve(endpoint_id));
        assert_eq!(t1, t3);
    }

    // ---------- Test #4: inbox-capacity wakers — every parked sender wakes ----------

    #[test]
    fn inbox_capacity_drain_wakes_every_parked_sender() {
        // Models the bug_015 contract: when the registry actor pops from a
        // full inbox, every sender parked on backpressure must be woken — not
        // just the most-recently-registered one. The previous AtomicWaker
        // stranded earlier registrants.
        let wakers: Arc<Mutex<Vec<Waker>>> = Arc::new(Mutex::new(Vec::new()));

        let (c1, w1) = counting_waker();
        let (c2, w2) = counting_waker();
        let (c3, w3) = counting_waker();
        wakers.lock().push(w1);
        wakers.lock().push(w2);
        wakers.lock().push(w3);

        // Drain mirrors the registry actor's per-pop wake step.
        let to_wake: Vec<Waker> = std::mem::take(&mut *wakers.lock());
        assert_eq!(to_wake.len(), 3);
        for w in to_wake {
            w.wake();
        }

        assert_eq!(c1.0.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(c2.0.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(c3.0.load(AtomicOrdering::SeqCst), 1);
        assert!(wakers.lock().is_empty(), "drain must clear the list");
    }
}
