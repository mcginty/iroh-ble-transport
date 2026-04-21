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
use crate::transport::events::{
    run_central_events, run_l2cap_accept, run_peripheral_requests, run_peripheral_state_events,
};
use crate::transport::hook::VerifiedEndpointEvent;
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
    pub verified_rx: Option<tokio::sync::mpsc::UnboundedReceiver<VerifiedEndpointEvent>>,
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
    /// Count of zero-length datagrams dropped on the L2CAP path (either
    /// before being framed on the wire, or after being received). A
    /// non-zero value indicates either an upstream noq/iroh regression
    /// handing us empty Transmits, or a misbehaving peer emitting
    /// `[0x00, 0x00]` frames. Guards iroh's `socket.rs` div-by-zero panic
    /// on `stride == 0`.
    pub empty_frames: u64,
}

pub struct BleTransport {
    handle: RegistryHandle,
    incoming_rx: tokio::sync::Mutex<Option<mpsc::Receiver<IncomingPacket>>>,
    routing: Arc<TransportRouting>,
    /// Shadow routing table (step 1 of the connection-system redesign).
    /// Observes every pipe open/close; not yet consulted for routing.
    /// Exposed via `routing_v2_snapshot()` so integration tests and
    /// telemetry can confirm mint/evict pairs balance correctly.
    routing_v2: Arc<crate::transport::routing_v2::Routing>,
    tx_bytes: Arc<AtomicU64>,
    rx_bytes: Arc<AtomicU64>,
    retransmits: Arc<AtomicU64>,
    truncations: Arc<AtomicU64>,
    empty_frames: Arc<AtomicU64>,
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
        let empty_frames = Arc::new(AtomicU64::new(0));
        let inbox_capacity_wakers: Arc<Mutex<Vec<Waker>>> = Arc::new(Mutex::new(Vec::new()));

        let iface = Arc::new(BlewDriver::new(
            Arc::clone(&central),
            Arc::clone(&peripheral),
            services,
            advertising_config,
        ));
        let routing_v2 = Arc::new(crate::transport::routing_v2::Routing::new());
        let driver = Driver::new(
            iface,
            inbox_tx.clone(),
            incoming_tx,
            Arc::clone(&retransmits),
            Arc::clone(&truncations),
            Arc::clone(&empty_frames),
            Arc::clone(&config.store),
            Arc::clone(&routing_v2),
            Arc::clone(&routing),
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
                while let Some(verified) = verified_rx.recv().await {
                    if inbox
                        .send(PeerCommand::VerifiedEndpoint {
                            endpoint_id: verified.endpoint_id,
                            token: verified.token,
                        })
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
            Arc::clone(&routing_v2),
            inbox_tx.clone(),
        ));
        tokio::spawn(run_peripheral_state_events(
            Arc::clone(&peripheral),
            Arc::clone(&routing),
            inbox_tx.clone(),
        ));
        tokio::spawn(run_peripheral_requests(
            Arc::clone(&peripheral),
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
            routing_v2,
            tx_bytes,
            rx_bytes,
            retransmits,
            truncations,
            empty_frames,
            inbox_capacity_wakers,
            store: config.store,
        })
    }

    /// Snapshot of the shadow routing table (step 1 of the redesign —
    /// pipe count only). Counts-only view; callers that need detail use
    /// `routing_v2_pipes_for_debug`.
    #[must_use]
    pub fn routing_v2_snapshot(&self) -> crate::transport::routing_v2::RoutingSnapshot {
        self.routing_v2.snapshot()
    }

    /// Shared handle to the shadow routing table, for plumbing into
    /// the `BleDedupHook` (which runs the promotion rule at TLS
    /// handshake completion). Cloning the returned `Arc` is cheap.
    /// Exposed only because the hook must hold a reference and the
    /// hook's construction site can't access private transport fields.
    #[must_use]
    pub fn routing_v2_handle(&self) -> Arc<crate::transport::routing_v2::Routing> {
        Arc::clone(&self.routing_v2)
    }

    /// Debug-only: list the pipes currently tracked by the shadow
    /// routing table. Integration tests use this to verify mint/evict
    /// balance; not intended for production use.
    #[must_use]
    pub fn routing_v2_pipes_for_debug(&self) -> Vec<crate::transport::routing_v2::Pipe> {
        self.routing_v2.pipes_for_debug()
    }

    #[must_use]
    pub fn metrics(&self) -> BleMetricsSnapshot {
        BleMetricsSnapshot {
            tx_bytes: self.tx_bytes.load(Ordering::Relaxed),
            rx_bytes: self.rx_bytes.load(Ordering::Relaxed),
            retransmits: self.retransmits.load(Ordering::Relaxed),
            truncations: self.truncations.load(Ordering::Relaxed),
            empty_frames: self.empty_frames.load(Ordering::Relaxed),
        }
    }

    pub fn address_lookup(&self) -> BleAddressLookup {
        BleAddressLookup {
            routing_v2: Arc::clone(&self.routing_v2),
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
                verified_endpoint: state.verified_endpoint,
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
    pub verified_endpoint: Option<EndpointId>,
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
            routing_v2: Arc::clone(&self.routing_v2),
            tx_bytes: Arc::clone(&self.tx_bytes),
            inbox_capacity_wakers: Arc::clone(&self.inbox_capacity_wakers),
        });
        Ok(Box::new(BleEndpoint {
            receiver: incoming_rx,
            watchable,
            sender,
            rx_bytes: Arc::clone(&self.rx_bytes),
            empty_frames: Arc::clone(&self.empty_frames),
        }))
    }
}

struct BleEndpoint {
    receiver: mpsc::Receiver<IncomingPacket>,
    watchable: Watchable<Vec<CustomAddr>>,
    sender: Arc<BleSender>,
    rx_bytes: Arc<AtomicU64>,
    empty_frames: Arc<AtomicU64>,
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
                    // Defensive backstop for iroh's `socket.rs:575` div-by-zero
                    // on `stride == 0`. The pipe layer should already filter
                    // empties, but keep this guard so a future regression in
                    // the framing path cannot panic the socket driver.
                    if packet.data.is_empty() {
                        self.empty_frames.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            device = %packet.device_id,
                            "BleEndpoint::poll_recv dropping zero-length packet (would trip iroh stride=0 panic)"
                        );
                        continue;
                    }
                    if bufs[filled].len() < packet.data.len() {
                        tracing::warn!(
                            len = packet.data.len(),
                            buf_cap = bufs[filled].len(),
                            "BleEndpoint::poll_recv dropping packet: buffer too small"
                        );
                        continue;
                    }
                    // Step 2 of the connection-system redesign: stamp
                    // inbound packets with the pipe's `StableConnId`
                    // (minted by the driver at pipe-open time). This id
                    // is installed in v1's routing as a device-keyed
                    // token when the pipe opens, so `poll_send` can
                    // resolve the `CustomAddr` iroh now carries on the
                    // inbound connection. Stable across GATT→L2CAP
                    // swap (same id throughout the pipe's lifetime).
                    let token = packet.stable_conn_id.as_u64();
                    tracing::trace!(
                        device = %packet.device_id,
                        stable_conn_id = %packet.stable_conn_id,
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
    routing_v2: Arc<crate::transport::routing_v2::Routing>,
    tx_bytes: Arc<AtomicU64>,
    inbox_capacity_wakers: Arc<Mutex<Vec<Waker>>>,
}

impl std::fmt::Debug for BleSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BleSender").finish()
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
        // The token iroh hands us is a `StableConnId` minted by routing_v2
        // — either the id of a live pipe (registered via `register_pipe` /
        // `register_pipe_with_id`) or a reservation waiting for the
        // driver to open a pipe to the peer. Translate both into a
        // `DeviceId` the registry actor can route on.
        let stable_id = crate::transport::routing_v2::StableConnId::from_raw(token);
        let device_id = if let Some(d) = self.routing_v2.device_for_pipe(stable_id) {
            d
        } else if let Some((_endpoint, prefix)) = self.routing_v2.reservation_target(stable_id) {
            // Reservation path: poll_send is iroh's *trigger* to start
            // the dial. scan_hint tells us which `DeviceId` is nearby
            // under this prefix right now; hand that to the registry
            // as a `SendDatagram`, which transitions the peer from
            // Discovered → Connecting and buffers the datagram until
            // the pipe opens. The driver consumes the reservation at
            // pipe-open time so iroh's outstanding `CustomAddr`
            // resolves to the new pipe's `StableConnId`.
            match self.routing_v2.scan_hint_for_prefix(&prefix) {
                Some(d) => d,
                None => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "BLE peer reservation has no live scan hint",
                    )));
                }
            }
        } else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::NotFound,
                "unknown BLE stable-conn token",
            )));
        };
        let snap = self.snapshots.load();
        let state = snap.peer_states.get(&device_id);
        let tx_gen = state.map_or(0, |s| s.tx_gen);
        let len = transmit.contents.len();
        tracing::trace!(device = %device_id, %stable_id, len, "BleSender::poll_send");
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
    routing_v2: Arc<crate::transport::routing_v2::Routing>,
}

impl std::fmt::Debug for BleAddressLookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BleAddressLookup").finish()
    }
}

/// Long-lived resolver stream for an endpoint. Yields a single
/// `CustomAddr(StableConnId)` once routing_v2 can answer "how would
/// I send to this endpoint?" — either because a routable pipe
/// exists, a pending pipe targets it, a reservation is already in
/// flight, or scan has surfaced the peer's prefix (in which case we
/// make the reservation now and yield it).
///
/// iroh 0.98 tolerates per-service `Err(_)` items without poisoning
/// sibling resolvers (n0-computer/iroh#4130), so this stream parks
/// `Pending` until one of those conditions is true. Once emitted,
/// the `StableConnId` is stable — it either points at a live pipe
/// or at a reservation that `poll_send` translates into a dial via
/// the registry's existing flow.
struct EndpointResolveStream {
    routing_v2: Arc<crate::transport::routing_v2::Routing>,
    endpoint_id: EndpointId,
    emitted: bool,
}

impl n0_future::Stream for EndpointResolveStream {
    type Item = Result<Item, address_lookup::Error>;

    fn poll_next(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.emitted {
            return Poll::Ready(None);
        }

        let endpoint_id = this.endpoint_id;
        let prefix = crate::transport::routing::prefix_from_endpoint(&endpoint_id);

        // Park the waker first so a state change racing the below
        // checks doesn't slip past us.
        this.routing_v2
            .register_endpoint_waker(endpoint_id, cx.waker());

        // 1. Already routable → yield the routable pipe's id.
        if let Some(stable_id) = this.routing_v2.routable_pipe_for(&endpoint_id) {
            return yield_stable(this, stable_id, "routable");
        }
        // 2. A pending pipe targets this endpoint → yield its id.
        if let Some(stable_id) = this.routing_v2.pending_pipe_for(&endpoint_id) {
            return yield_stable(this, stable_id, "pending");
        }
        // 3. A reservation already exists for this prefix → yield it.
        if let Some(reservation) = this.routing_v2.reservation_for_prefix(&prefix) {
            return yield_stable(this, reservation.stable_id, "reservation_existing");
        }
        // 4. Scan has surfaced this prefix → make a reservation and
        //    yield it. `poll_send` on this id will trigger the dial
        //    via the registry's SendDatagram flow the first time
        //    iroh sends anything.
        if this.routing_v2.scan_hint_for_prefix(&prefix).is_some() {
            let stable_id = this.routing_v2.reserve_outbound(endpoint_id);
            return yield_stable(this, stable_id, "reservation_new");
        }

        // Nothing yet — the scan hasn't seen this peer, no pipe exists.
        // Wait. `note_scan_hint` and `register_pending` both wake this
        // waker when relevant state arrives.
        Poll::Pending
    }
}

fn yield_stable(
    this: &mut EndpointResolveStream,
    stable_id: crate::transport::routing_v2::StableConnId,
    source: &'static str,
) -> Poll<Option<Result<Item, address_lookup::Error>>> {
    this.emitted = true;
    let token = stable_id.as_u64();
    tracing::info!(
        endpoint_id = %this.endpoint_id,
        %stable_id,
        source,
        "BleAddressLookup yielding stable-id token"
    );
    let info = EndpointInfo {
        endpoint_id: this.endpoint_id,
        data: EndpointData::new(vec![TransportAddr::Custom(token_custom_addr(token))]),
    };
    Poll::Ready(Some(Ok(Item::new(info, "iroh-ble", None))))
}

impl AddressLookup for BleAddressLookup {
    fn resolve(
        &self,
        endpoint_id: EndpointId,
    ) -> Option<n0_future::stream::Boxed<Result<Item, address_lookup::Error>>> {
        Some(Box::pin(EndpointResolveStream {
            routing_v2: Arc::clone(&self.routing_v2),
            endpoint_id,
            emitted: false,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::routing_v2::{Direction, Routing, StableConnId};
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

    fn endpoint_id_with_first_byte(b: u8) -> EndpointId {
        let mut bytes = [0u8; 32];
        bytes[0] = b;
        let secret = iroh_base::SecretKey::from_bytes(&bytes);
        secret.public()
    }

    fn extract_token(
        stream: &mut n0_future::stream::Boxed<Result<Item, address_lookup::Error>>,
        cx: &mut Context<'_>,
    ) -> u64 {
        match Pin::new(stream).poll_next(cx) {
            Poll::Ready(Some(Ok(item))) => {
                let addr = item
                    .endpoint_info()
                    .data
                    .addrs()
                    .next()
                    .expect("addr present");
                match addr {
                    TransportAddr::Custom(c) => parse_token_addr(c).expect("parse"),
                    _ => panic!("expected Custom addr"),
                }
            }
            other => panic!("expected Ready(Some(Ok(_))), got {other:?}"),
        }
    }

    // ---------- Test #1: resolve parks until scan or pipe appears ----------

    #[test]
    fn ble_address_lookup_resolve_parks_until_scan_hint_lands() {
        // Authority model: without a scan_hint (or a live pipe / pending /
        // reservation) the resolver can't promise iroh anything. It must
        // park, then wake when scan finally surfaces the peer's prefix.
        let routing_v2 = Arc::new(Routing::new());
        let lookup = BleAddressLookup {
            routing_v2: Arc::clone(&routing_v2),
        };
        let endpoint_id = endpoint_id_with_first_byte(0xCD);
        let mut stream = lookup
            .resolve(endpoint_id)
            .expect("resolve must return Some(stream)");

        let (counter, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);

        match Pin::new(&mut stream).poll_next(&mut cx) {
            Poll::Pending => {}
            other => panic!("expected Pending before scan_hint, got {other:?}"),
        }
        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 0);

        let prefix = crate::transport::routing::prefix_from_endpoint(&endpoint_id);
        let device = blew::DeviceId::from("dev-late");
        routing_v2.note_scan_hint(prefix, device.clone());
        assert_eq!(
            counter.0.load(AtomicOrdering::SeqCst),
            1,
            "scan_hint must wake the parked resolver stream"
        );

        let token = extract_token(&mut stream, &mut cx);
        // The token is a reservation (no pipe yet). reservation_target
        // returns the endpoint we asked about.
        let (got_ep, got_prefix) = routing_v2
            .reservation_target(StableConnId::from_raw(token))
            .expect("reservation minted");
        assert_eq!(got_ep, endpoint_id);
        assert_eq!(got_prefix, prefix);

        assert!(matches!(
            Pin::new(&mut stream).poll_next(&mut cx),
            Poll::Ready(None)
        ));
    }

    // ---------- Test #2: resolve yields pipe id when pipe already exists ----------

    #[test]
    fn ble_address_lookup_resolve_returns_routable_pipe_id_when_present() {
        // If a pipe is already routable for this endpoint, the resolver
        // yields that pipe's StableConnId — not a fresh reservation.
        // Keeps iroh's CustomAddr stable across multiple resolve calls.
        let routing_v2 = Arc::new(Routing::new());
        let lookup = BleAddressLookup {
            routing_v2: Arc::clone(&routing_v2),
        };
        let endpoint_id = endpoint_id_with_first_byte(0xBB);
        let pipe_id = routing_v2.register_pipe(blew::DeviceId::from("mac-bb"), Direction::Outbound);
        routing_v2.insert_routable(
            endpoint_id,
            pipe_id,
            crate::transport::routing_v2::Dialer::Low,
        );

        let mut stream = lookup.resolve(endpoint_id).expect("Some");
        let (_counter, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let token = extract_token(&mut stream, &mut cx);
        assert_eq!(
            token,
            pipe_id.as_u64(),
            "resolver yields the routable pipe's StableConnId"
        );
    }

    // ---------- Test #3: resolve is idempotent across repeat calls ----------

    #[test]
    fn ble_address_lookup_resolve_is_idempotent() {
        let routing_v2 = Arc::new(Routing::new());
        let lookup = BleAddressLookup {
            routing_v2: Arc::clone(&routing_v2),
        };
        let endpoint_id = endpoint_id_with_first_byte(0xEE);
        let prefix = crate::transport::routing::prefix_from_endpoint(&endpoint_id);
        routing_v2.note_scan_hint(prefix, blew::DeviceId::from("mac-ee"));

        let (_c1, w1) = counting_waker();
        let mut cx1 = Context::from_waker(&w1);
        let mut s1 = lookup.resolve(endpoint_id).expect("Some");
        let t1 = extract_token(&mut s1, &mut cx1);

        let (_c2, w2) = counting_waker();
        let mut cx2 = Context::from_waker(&w2);
        let mut s2 = lookup.resolve(endpoint_id).expect("Some");
        let t2 = extract_token(&mut s2, &mut cx2);
        assert_eq!(
            t1, t2,
            "reserve_outbound is idempotent — second resolve returns same id"
        );
    }

    // ---------- Test #4: concurrent resolvers only wake on their prefix ----------

    #[test]
    fn concurrent_resolvers_only_wake_on_their_prefix() {
        // scan_hint for prefix A must wake ep_A's parked resolver but not
        // ep_B's (distinct prefixes). Verifies wake_endpoint_waiters_for_prefix
        // filters correctly, avoiding a "wake-all" regression.
        let routing_v2 = Arc::new(Routing::new());
        let lookup = BleAddressLookup {
            routing_v2: Arc::clone(&routing_v2),
        };
        let ep_a = endpoint_id_with_first_byte(0xA0);
        let ep_b = endpoint_id_with_first_byte(0xB0);
        let prefix_a = crate::transport::routing::prefix_from_endpoint(&ep_a);
        let prefix_b = crate::transport::routing::prefix_from_endpoint(&ep_b);

        let mut sa = lookup.resolve(ep_a).expect("Some");
        let mut sb = lookup.resolve(ep_b).expect("Some");
        let (ca, wa) = counting_waker();
        let (cb, wb) = counting_waker();
        let mut cxa = Context::from_waker(&wa);
        let mut cxb = Context::from_waker(&wb);

        assert!(matches!(
            Pin::new(&mut sa).poll_next(&mut cxa),
            Poll::Pending
        ));
        assert!(matches!(
            Pin::new(&mut sb).poll_next(&mut cxb),
            Poll::Pending
        ));

        routing_v2.note_scan_hint(prefix_a, blew::DeviceId::from("dev-a"));
        assert_eq!(
            ca.0.load(AtomicOrdering::SeqCst),
            1,
            "A's waker fires on A's prefix"
        );
        assert_eq!(
            cb.0.load(AtomicOrdering::SeqCst),
            0,
            "B's waker must NOT fire on A's prefix"
        );

        let _ = extract_token(&mut sa, &mut cxa);
        assert!(matches!(
            Pin::new(&mut sb).poll_next(&mut cxb),
            Poll::Pending
        ));

        routing_v2.note_scan_hint(prefix_b, blew::DeviceId::from("dev-b"));
        assert_eq!(cb.0.load(AtomicOrdering::SeqCst), 1);
        let _ = extract_token(&mut sb, &mut cxb);
    }

    // ---------- Test #5: stream drop leaves future resolves healthy ----------

    #[test]
    fn resolver_stream_drop_before_scan_does_not_break_later_resolves() {
        let routing_v2 = Arc::new(Routing::new());
        let lookup = BleAddressLookup {
            routing_v2: Arc::clone(&routing_v2),
        };
        let endpoint_id = endpoint_id_with_first_byte(0xDD);
        let prefix = crate::transport::routing::prefix_from_endpoint(&endpoint_id);

        {
            let mut s = lookup.resolve(endpoint_id).expect("Some");
            let (_c, w) = counting_waker();
            let mut cx = Context::from_waker(&w);
            assert!(matches!(Pin::new(&mut s).poll_next(&mut cx), Poll::Pending));
        }

        routing_v2.note_scan_hint(prefix, blew::DeviceId::from("late-but-arrived"));

        let mut s2 = lookup.resolve(endpoint_id).expect("Some");
        let (_c, w) = counting_waker();
        let mut cx = Context::from_waker(&w);
        let _ = extract_token(&mut s2, &mut cx);
    }

    // ---------- Test #6: inbox-capacity wakers — every parked sender wakes ----------

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
