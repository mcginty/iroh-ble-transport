//! `BleTransport` — iroh `CustomTransport` implementation driven by the
//! registry actor and a `BlewDriver`.

use std::io;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
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
use crate::transport::hook::{BleDedupHook, VerifiedEndpointEvent};
use crate::transport::peer::{ConnectPath, KEY_PREFIX_LEN, PeerCommand};
use crate::transport::registry::{PhaseKind, Registry, RegistryHandle, SnapshotMaps};
use crate::transport::routing::{TOKEN_LEN, parse_token_addr, token_custom_addr};
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

/// Builder for [`BleTransport`].
///
/// Use [`BleTransport::builder`] to get an instance, chain configuration
/// methods, then call [`BleTransportBuilder::build`] with your
/// `EndpointId` to construct the transport.
///
/// Defaults:
/// - [`L2capPolicy::PreferL2cap`]
/// - [`Central::new`] + [`Peripheral::new`] are constructed with
///   default config at `build` time
/// - [`InMemoryPeerStore`] (non-durable across restarts)
pub struct BleTransportBuilder {
    l2cap_policy: L2capPolicy,
    central: Option<Arc<Central>>,
    peripheral: Option<Arc<Peripheral>>,
    peer_store: Option<Arc<dyn PeerStore>>,
}

impl BleTransportBuilder {
    fn new() -> Self {
        Self {
            l2cap_policy: L2capPolicy::default(),
            central: None,
            peripheral: None,
            peer_store: None,
        }
    }

    /// Override the L2CAP policy. Defaults to [`L2capPolicy::PreferL2cap`].
    #[must_use]
    pub fn l2cap_policy(mut self, policy: L2capPolicy) -> Self {
        self.l2cap_policy = policy;
        self
    }

    /// Supply a pre-constructed [`Central`]. If omitted, [`build`] calls
    /// [`Central::new`] with default config.
    ///
    /// [`build`]: Self::build
    #[must_use]
    pub fn central(mut self, central: Arc<Central>) -> Self {
        self.central = Some(central);
        self
    }

    /// Supply a pre-constructed [`Peripheral`]. If omitted, [`build`]
    /// calls [`Peripheral::new`] with default config.
    ///
    /// [`build`]: Self::build
    #[must_use]
    pub fn peripheral(mut self, peripheral: Arc<Peripheral>) -> Self {
        self.peripheral = Some(peripheral);
        self
    }

    /// Persistent peer cache. Defaults to an [`InMemoryPeerStore`];
    /// applications that want durable state across restarts can plug in
    /// their own implementation of [`PeerStore`]. The transport writes a
    /// snapshot whenever a peer leaves `Connected`, and forgets a peer
    /// when it transitions to `Dead { MaxRetries }`.
    #[must_use]
    pub fn peer_store(mut self, store: Arc<dyn PeerStore>) -> Self {
        self.peer_store = Some(store);
        self
    }

    /// Construct the transport. `endpoint_id` is the local node's iroh
    /// `EndpointId` — the advertising-service UUID embeds its first 12
    /// bytes so peers discover each other without coordination. This
    /// must match the secret key used on the `Endpoint` that will host
    /// the transport.
    ///
    /// # Errors
    ///
    /// Propagates errors from bringing up the BLE radio (adapter not
    /// powered, GATT registration, advertising start).
    pub async fn build(self, endpoint_id: EndpointId) -> BleResult<Arc<BleTransport>> {
        let central = match self.central {
            Some(c) => c,
            None => Arc::new(Central::new().await?),
        };
        let peripheral = match self.peripheral {
            Some(p) => p,
            None => Arc::new(Peripheral::new().await?),
        };
        let store = self
            .peer_store
            .unwrap_or_else(|| Arc::new(InMemoryPeerStore::new()));
        BleTransport::construct(endpoint_id, central, peripheral, store, self.l2cap_policy).await
    }
}

impl std::fmt::Debug for BleTransportBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BleTransportBuilder")
            .field("l2cap_policy", &self.l2cap_policy)
            .field("central", &self.central.is_some())
            .field("peripheral", &self.peripheral.is_some())
            .field("peer_store", &self.peer_store.is_some())
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

/// RAII guard for the pipe-lifetime watchdog spawned by
/// [`BleTransport::start_pipe_watchdog`]. Dropping this guard aborts
/// the watchdog task.
///
/// ## Why this isn't held inside `BleTransport`
///
/// The watchdog task needs to call `remote_info()` on the iroh
/// `Endpoint`, so it holds an `Arc<Endpoint>` internally. Iroh's
/// `Endpoint` in turn holds the transports list, which holds
/// `Arc<BleTransport>` (installed via
/// [`BleTransport::as_custom_transport`]). If `BleTransport` also
/// stored the watchdog's task handle, we'd have a reference cycle
/// (`Arc<BleTransport>` → task → `Endpoint` → `Arc<BleTransport>`)
/// and neither would ever be dropped.
///
/// Iroh 0.98 does not expose a `Weak<Endpoint>` handle that would
/// let us break the cycle. A future iroh release may add one, or an
/// `on_connection_closed` hook that obviates the watchdog entirely —
/// when that lands this guard can become an internal detail.
#[must_use = "dropping the PipeWatchdog aborts the watchdog task"]
pub struct PipeWatchdog {
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for PipeWatchdog {
    fn drop(&mut self) {
        tracing::info!("pipe_watchdog: guard dropped; aborting watchdog task");
        self.handle.abort();
    }
}

impl std::fmt::Debug for PipeWatchdog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeWatchdog").finish()
    }
}

pub struct BleTransport {
    local_id: EndpointId,
    /// Sender half of the verified-endpoint channel. Cloned into every
    /// [`BleDedupHook`] returned from [`BleTransport::dedup_hook`]; the
    /// receiver half is consumed by the forwarder task spawned at
    /// construction time.
    verified_tx: tokio::sync::mpsc::UnboundedSender<VerifiedEndpointEvent>,
    handle: RegistryHandle,
    incoming_rx: tokio::sync::Mutex<Option<mpsc::Receiver<IncomingPacket>>>,
    /// Authoritative routing table. Tracks scan hints, pending
    /// pipes, routable (post-handshake) pipes, outbound reservations,
    /// and the per-endpoint waker set. Exposed via
    /// `routing_snapshot()` for telemetry.
    routing: Arc<crate::transport::routing::Routing>,
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
    /// Start configuring a new transport. See [`BleTransportBuilder`].
    #[must_use]
    pub fn builder() -> BleTransportBuilder {
        BleTransportBuilder::new()
    }

    async fn construct(
        local_id: EndpointId,
        central: Arc<Central>,
        peripheral: Arc<Peripheral>,
        store: Arc<dyn PeerStore>,
        l2cap_policy: L2capPolicy,
    ) -> BleResult<Arc<Self>> {
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

        let tx_bytes = Arc::new(AtomicU64::new(0));
        let rx_bytes = Arc::new(AtomicU64::new(0));
        let retransmits = Arc::new(AtomicU64::new(0));
        let truncations = Arc::new(AtomicU64::new(0));
        let empty_frames = Arc::new(AtomicU64::new(0));
        let inbox_capacity_wakers: Arc<Mutex<Vec<Waker>>> = Arc::new(Mutex::new(Vec::new()));

        let psm_atomic = Arc::new(AtomicU16::new(0));
        let iface = Arc::new(BlewDriver::new(
            Arc::clone(&central),
            Arc::clone(&peripheral),
            services,
            advertising_config,
            Arc::clone(&psm_atomic),
            inbox_tx.clone(),
        ));
        let routing = Arc::new(crate::transport::routing::Routing::new());
        let driver = Driver::new(
            iface,
            inbox_tx.clone(),
            incoming_tx,
            Arc::clone(&retransmits),
            Arc::clone(&truncations),
            Arc::clone(&empty_frames),
            Arc::clone(&store),
            Arc::clone(&routing),
        );

        if l2cap_policy == L2capPolicy::PreferL2cap {
            match peripheral.l2cap_listener().await {
                Ok((assigned_psm, listener)) => {
                    let val = assigned_psm.value();
                    info!(psm = val, "L2CAP listener started");
                    psm_atomic.store(val, Ordering::Relaxed);
                    tokio::spawn(run_l2cap_accept(listener, inbox_tx.clone()));
                }
                Err(e) => {
                    warn!(error = %e, "L2CAP listener failed, falling back to GATT-only");
                }
            }
        }

        let registry = Registry::new(l2cap_policy, local_id);
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

        // Verified-endpoint channel: the hook returned from
        // `BleTransport::dedup_hook` sends events here, the forwarder
        // below translates them into `PeerCommand`s the registry acts on.
        let (verified_tx, mut verified_rx) =
            tokio::sync::mpsc::unbounded_channel::<VerifiedEndpointEvent>();
        let forwarder_inbox = inbox_tx.clone();
        tokio::spawn(async move {
            while let Some(verified) = verified_rx.recv().await {
                // Fire teardown for any pipes the promote rule
                // evicted to make room for this handshake. Each
                // `Stalled` causes the registry to drain the
                // device's pipe and close its BLE channel; the
                // pipe worker then exits, which evicts the
                // lingering routing pipe record.
                for device_id in verified.evicted_devices {
                    if forwarder_inbox
                        .send(PeerCommand::Stalled { device_id })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                if forwarder_inbox
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

        tokio::spawn(run_central_events(
            Arc::clone(&central),
            Arc::clone(&routing),
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
            psm_atomic,
        ));
        tokio::spawn(run_watchdog(inbox_tx.clone()));

        Ok(Arc::new(Self {
            local_id,
            verified_tx,
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
            empty_frames,
            inbox_capacity_wakers,
            store,
        }))
    }

    /// Coerce to the trait object iroh's `Endpoint::builder()` consumes:
    /// `endpoint_builder.add_custom_transport(ble.as_custom_transport())`.
    #[must_use]
    pub fn as_custom_transport(
        self: &Arc<Self>,
    ) -> Arc<dyn iroh::endpoint::transports::CustomTransport> {
        Arc::clone(self) as Arc<dyn iroh::endpoint::transports::CustomTransport>
    }

    /// Build the [`EndpointHooks`] implementation that runs the BLE
    /// handshake-dedup promotion rule. Pair with
    /// `endpoint_builder.hooks(ble.dedup_hook())`.
    ///
    /// [`EndpointHooks`]: iroh::endpoint::EndpointHooks
    #[must_use]
    pub fn dedup_hook(&self) -> BleDedupHook {
        BleDedupHook::new(
            self.local_id,
            Arc::clone(&self.routing),
            self.verified_tx.clone(),
        )
    }

    /// Counts-only snapshot of the routing table (pipes / scan hints /
    /// pending / routable / reservations). Callers that need identifier
    /// detail use `routing_pipes_for_debug`.
    #[must_use]
    pub fn routing_snapshot(&self) -> crate::transport::routing::RoutingSnapshot {
        self.routing.snapshot()
    }

    /// Start the pipe-lifetime watchdog. Polls `endpoint.remote_info`
    /// for every routable entry and, when iroh reports no active path,
    /// tells the registry to drain the corresponding BLE pipe. See
    /// [`crate::transport::pipe_watchdog`] for the full contract.
    ///
    /// The returned [`PipeWatchdog`] is a drop-guard: dropping it
    /// aborts the watchdog task. Bind it to a variable whose lifetime
    /// matches the endpoint (e.g. the same scope, or a field on your
    /// app state) to keep the watchdog running.
    ///
    /// `endpoint` is cloned internally (`iroh::Endpoint` is already
    /// internally `Arc`-backed, so this is cheap).
    ///
    /// This must be a separate call rather than being bundled into the
    /// transport because holding the `Endpoint` on `BleTransport` would
    /// create a reference cycle — see the module documentation on
    /// [`PipeWatchdog`] for details.
    #[must_use = "dropping the returned PipeWatchdog aborts the watchdog task"]
    pub fn start_pipe_watchdog(&self, endpoint: &iroh::Endpoint) -> PipeWatchdog {
        let handle = crate::transport::pipe_watchdog::spawn(
            Arc::new(endpoint.clone()),
            Arc::clone(&self.routing),
            self.handle.inbox.clone(),
            crate::transport::pipe_watchdog::DEFAULT_POLL_INTERVAL,
            crate::transport::pipe_watchdog::DEFAULT_GRACE_PERIOD,
        );
        PipeWatchdog { handle }
    }

    /// Debug-only: list the pipes currently tracked by the shadow
    /// routing table. Integration tests use this to verify mint/evict
    /// balance; not intended for production use.
    #[must_use]
    pub fn routing_pipes_for_debug(&self) -> Vec<crate::transport::routing::Pipe> {
        self.routing.pipes_for_debug()
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

    /// Returns `true` if the local scan has recently surfaced this
    /// peer's key prefix. Step 8 of the redesign: callers gate
    /// `join_peers`-style reconnect nudges on this so we don't pin
    /// cached peers that aren't currently in range. Surfaces only
    /// what `routing`'s scan_hint holds; no authority claim beyond
    /// "advertisement seen at some point this session."
    #[must_use]
    pub fn has_scan_hint_for_endpoint(&self, endpoint_id: &EndpointId) -> bool {
        let prefix = crate::transport::routing::prefix_from_endpoint(endpoint_id);
        self.routing.scan_hint_for_prefix(&prefix).is_some()
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
            routing: Arc::clone(&self.routing),
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

    // TODO(#4): when a peer restarts mid-session, the new inbound
    // handshake can stall on the server side for tens of seconds to
    // a few minutes before `after_handshake` fires, even though the
    // BLE pipe is up and the client side has already completed. The
    // pipe's inbound bytes flow through here on the way into iroh;
    // if the bug is on our side of the boundary, this is where the
    // instrumentation (per-stable-id byte counter, last-delivered
    // timestamp) would go to prove the packets arrive vs. sit in
    // iroh. See https://github.com/mcginty/iroh-ble-transport/issues/4.
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
                    // Stamp inbound packets with the pipe's
                    // `StableConnId` (minted by the driver at
                    // pipe-open time). `poll_send` resolves that id
                    // back to the pipe via `routing.device_for_pipe`.
                    // Stable across the pipe's full lifetime, so
                    // iroh's Connection keeps routing to the correct
                    // per-peer pipe regardless of L2CAP add/drop.
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
    routing: Arc<crate::transport::routing::Routing>,
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
        // The token iroh hands us is a `StableConnId` minted by routing
        // — either the id of a live pipe (registered via `register_pipe` /
        // `register_pipe_with_id`) or a reservation waiting for the
        // driver to open a pipe to the peer. Translate both into a
        // `DeviceId` the registry actor can route on.
        let stable_id = crate::transport::routing::StableConnId::from_raw(token);
        let (device_id, target_endpoint) = if let Some(d) = self.routing.device_for_pipe(stable_id)
        {
            (d, None)
        } else if let Some((endpoint, prefix)) = self.routing.reservation_target(stable_id) {
            // Reservation path: poll_send is iroh's *trigger* to start
            // the dial. scan_hint tells us which `DeviceId` is nearby
            // under this prefix right now; hand that to the registry
            // as a `SendDatagram`, which transitions the peer from
            // Discovered → Connecting and buffers the datagram until
            // the pipe opens. The driver consumes the reservation at
            // pipe-open time so iroh's outstanding `CustomAddr`
            // resolves to the new pipe's `StableConnId`.
            match self.routing.scan_hint_for_prefix(&prefix) {
                Some(d) => (d, Some(endpoint)),
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
            target_endpoint,
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
    routing: Arc<crate::transport::routing::Routing>,
}

impl std::fmt::Debug for BleAddressLookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BleAddressLookup").finish()
    }
}

/// Long-lived resolver stream for an endpoint. Emits a
/// `CustomAddr(StableConnId)` whenever routing's authoritative
/// answer for "how would I send to this endpoint?" changes to a new
/// `StableConnId`. Stays alive across the endpoint's lifetime so
/// that — after a pipe dies and is re-dialed — a fresh CustomAddr
/// can flow to iroh's `address_lookup_stream`, giving iroh's
/// RemoteState a new path candidate for future `Endpoint::connect`
/// calls. (iroh doesn't auto-migrate existing Connections to the
/// new path: the stale Connection still idles on its own schedule.
/// But keeping the stream alive means reconnect attempts at the
/// app layer — `join_peers`, `Endpoint::connect` — find the fresh
/// pipe immediately, without re-triggering AddressLookup.)
///
/// The answer is the first non-empty of:
///   1. routable pipe for this endpoint (authoritative post-handshake)
///   2. pending pipe whose target is this endpoint
///   3. existing outbound reservation for this prefix
///   4. scan_hint for this prefix → mint a fresh reservation
///
/// Emission rules:
///   - `Poll::Pending` when there's no answer yet OR when the answer
///     matches the last emitted `StableConnId`.
///   - `Poll::Ready(Some(Ok(Item)))` when the answer differs from
///     `last_emitted` (or this is the first emission).
///   - Never `Poll::Ready(None)`: the stream only ends when dropped.
struct EndpointResolveStream {
    routing: Arc<crate::transport::routing::Routing>,
    endpoint_id: EndpointId,
    last_emitted: Option<crate::transport::routing::StableConnId>,
}

impl n0_future::Stream for EndpointResolveStream {
    type Item = Result<Item, address_lookup::Error>;

    fn poll_next(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let endpoint_id = this.endpoint_id;
        let prefix = crate::transport::routing::prefix_from_endpoint(&endpoint_id);

        // Park the waker first so a state change racing the below
        // checks doesn't slip past us.
        this.routing
            .register_endpoint_waker(endpoint_id, cx.waker());

        let current = {
            // 1. Already routable → yield the routable pipe's id.
            if let Some(stable_id) = this.routing.routable_pipe_for(&endpoint_id) {
                Some((stable_id, "routable"))
            }
            // 2. A pending pipe targets this endpoint → yield its id.
            else if let Some(stable_id) = this.routing.pending_pipe_for(&endpoint_id) {
                Some((stable_id, "pending"))
            }
            // 3. A reservation already exists for this prefix → yield it.
            else if let Some(reservation) = this.routing.reservation_for_prefix(&prefix) {
                Some((reservation.stable_id, "reservation_existing"))
            }
            // 4. Scan has surfaced this prefix → make a reservation and
            //    yield it. `poll_send` on this id will trigger the dial
            //    via the registry's SendDatagram flow the first time
            //    iroh sends anything.
            else if this.routing.scan_hint_for_prefix(&prefix).is_some() {
                let stable_id = this.routing.reserve_outbound(endpoint_id);
                Some((stable_id, "reservation_new"))
            } else {
                None
            }
        };

        match current {
            Some((stable_id, _)) if Some(stable_id) == this.last_emitted => {
                // Answer unchanged; stay parked. A future state change
                // (new routable, new reservation, …) will wake us.
                Poll::Pending
            }
            Some((stable_id, source)) => {
                this.last_emitted = Some(stable_id);
                emit_stable(endpoint_id, stable_id, source)
            }
            None => {
                // Nothing yet — the scan hasn't seen this peer, no pipe
                // exists. Wait. `note_scan_hint` and `register_pending`
                // both wake this waker when relevant state arrives.
                Poll::Pending
            }
        }
    }
}

fn emit_stable(
    endpoint_id: EndpointId,
    stable_id: crate::transport::routing::StableConnId,
    source: &'static str,
) -> Poll<Option<Result<Item, address_lookup::Error>>> {
    let token = stable_id.as_u64();
    // Debug level: fires on every `Endpoint::connect` / `gossip.join_peers`
    // resolve, which is per-peer, per-reconnect-tick. Not lifecycle
    // enough to warrant info. The downstream "bound pipe to resolver
    // reservation" log in driver.rs carries the actual outcome.
    tracing::debug!(
        %endpoint_id,
        %stable_id,
        source,
        "BleAddressLookup yielding stable-id token"
    );
    let info = EndpointInfo {
        endpoint_id,
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
            routing: Arc::clone(&self.routing),
            endpoint_id,
            last_emitted: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::routing::{Direction, Routing, StableConnId};
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
        let routing = Arc::new(Routing::new());
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
            Poll::Pending => {}
            other => panic!("expected Pending before scan_hint, got {other:?}"),
        }
        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 0);

        let prefix = crate::transport::routing::prefix_from_endpoint(&endpoint_id);
        let device = blew::DeviceId::from("dev-late");
        routing.note_scan_hint(prefix, device.clone());
        assert_eq!(
            counter.0.load(AtomicOrdering::SeqCst),
            1,
            "scan_hint must wake the parked resolver stream"
        );

        let token = extract_token(&mut stream, &mut cx);
        // The token is a reservation (no pipe yet). reservation_target
        // returns the endpoint we asked about.
        let (got_ep, got_prefix) = routing
            .reservation_target(StableConnId::from_raw(token))
            .expect("reservation minted");
        assert_eq!(got_ep, endpoint_id);
        assert_eq!(got_prefix, prefix);

        // After first emission, the stream stays alive and parks
        // until the answer changes. Re-polling the same state
        // yields Pending — iroh keeps the CustomAddr we just handed
        // it instead of seeing a spurious new Item every poll.
        assert!(matches!(
            Pin::new(&mut stream).poll_next(&mut cx),
            Poll::Pending
        ));
    }

    // ---------- Test #2: resolve yields pipe id when pipe already exists ----------

    #[test]
    fn ble_address_lookup_resolve_returns_routable_pipe_id_when_present() {
        // If a pipe is already routable for this endpoint, the resolver
        // yields that pipe's StableConnId — not a fresh reservation.
        // Keeps iroh's CustomAddr stable across multiple resolve calls.
        let routing = Arc::new(Routing::new());
        let lookup = BleAddressLookup {
            routing: Arc::clone(&routing),
        };
        let endpoint_id = endpoint_id_with_first_byte(0xBB);
        let pipe_id = routing.register_pipe(blew::DeviceId::from("mac-bb"), Direction::Outbound);
        routing.insert_routable(endpoint_id, pipe_id, crate::transport::routing::Dialer::Low);

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
        let routing = Arc::new(Routing::new());
        let lookup = BleAddressLookup {
            routing: Arc::clone(&routing),
        };
        let endpoint_id = endpoint_id_with_first_byte(0xEE);
        let prefix = crate::transport::routing::prefix_from_endpoint(&endpoint_id);
        routing.note_scan_hint(prefix, blew::DeviceId::from("mac-ee"));

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
        let routing = Arc::new(Routing::new());
        let lookup = BleAddressLookup {
            routing: Arc::clone(&routing),
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

        routing.note_scan_hint(prefix_a, blew::DeviceId::from("dev-a"));
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

        routing.note_scan_hint(prefix_b, blew::DeviceId::from("dev-b"));
        assert_eq!(cb.0.load(AtomicOrdering::SeqCst), 1);
        let _ = extract_token(&mut sb, &mut cxb);
    }

    // ---------- Test #5: stream drop leaves future resolves healthy ----------

    #[test]
    fn resolver_stream_drop_before_scan_does_not_break_later_resolves() {
        let routing = Arc::new(Routing::new());
        let lookup = BleAddressLookup {
            routing: Arc::clone(&routing),
        };
        let endpoint_id = endpoint_id_with_first_byte(0xDD);
        let prefix = crate::transport::routing::prefix_from_endpoint(&endpoint_id);

        {
            let mut s = lookup.resolve(endpoint_id).expect("Some");
            let (_c, w) = counting_waker();
            let mut cx = Context::from_waker(&w);
            assert!(matches!(Pin::new(&mut s).poll_next(&mut cx), Poll::Pending));
        }

        routing.note_scan_hint(prefix, blew::DeviceId::from("late-but-arrived"));

        let mut s2 = lookup.resolve(endpoint_id).expect("Some");
        let (_c, w) = counting_waker();
        let mut cx = Context::from_waker(&w);
        let _ = extract_token(&mut s2, &mut cx);
    }

    // ---------- Test #5b: stream re-emits when routable pipe changes ----------

    #[test]
    fn resolver_stream_reemits_when_routable_pipe_changes() {
        // F2 recovery path: a pipe dies, gets re-dialed, and the
        // routable entry flips to a new StableConnId. The long-lived
        // resolver stream must emit the *new* CustomAddr so iroh's
        // address_lookup_stream gets the updated path candidate —
        // otherwise iroh only knows the dead CustomAddr and future
        // reconnect attempts resolve against stale state.
        let routing = Arc::new(Routing::new());
        let lookup = BleAddressLookup {
            routing: Arc::clone(&routing),
        };
        let endpoint_id = endpoint_id_with_first_byte(0xF2);

        // First pipe promoted → first routable.
        let id1 = routing.register_pipe(blew::DeviceId::from("mac-1"), Direction::Outbound);
        routing.insert_routable(endpoint_id, id1, crate::transport::routing::Dialer::Low);

        let mut stream = lookup.resolve(endpoint_id).expect("Some");
        let (counter, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);

        let t1 = extract_token(&mut stream, &mut cx);
        assert_eq!(t1, id1.as_u64(), "first emission is the first pipe's id");
        // Re-poll without state change → Pending.
        assert!(matches!(
            Pin::new(&mut stream).poll_next(&mut cx),
            Poll::Pending
        ));

        // Pipe 1 dies; pipe 2 takes its place. This is what happens
        // when the peer restarts: evict_pipe_state clears routable,
        // and a fresh promote (from a new handshake) installs a new
        // StableConnId. Eviction alone wakes the resolver so it can
        // re-poll and (if still nothing routable) park for the next
        // state change.
        routing.evict_pipe_state(id1);
        assert!(
            counter.0.load(AtomicOrdering::SeqCst) >= 1,
            "evict_pipe_state must wake the parked resolver"
        );
        // Re-poll: nothing routable (evicted), no pending, no
        // reservation, no scan_hint. Stream parks and registers a
        // fresh waker (the previous one was drained by wake_endpoint_waiters).
        assert!(matches!(
            Pin::new(&mut stream).poll_next(&mut cx),
            Poll::Pending
        ));
        let wakes_before_reinsert = counter.0.load(AtomicOrdering::SeqCst);

        let id2 = routing.register_pipe(blew::DeviceId::from("mac-2"), Direction::Outbound);
        routing.insert_routable(endpoint_id, id2, crate::transport::routing::Dialer::Low);
        assert!(
            counter.0.load(AtomicOrdering::SeqCst) > wakes_before_reinsert,
            "insert_routable must wake the parked resolver"
        );

        let t2 = extract_token(&mut stream, &mut cx);
        assert_eq!(
            t2,
            id2.as_u64(),
            "stream emits the new routable pipe's id after routable flips"
        );
        assert_ne!(t1, t2, "the new id differs from the old one");

        // Re-poll with unchanged routable → Pending again.
        assert!(matches!(
            Pin::new(&mut stream).poll_next(&mut cx),
            Poll::Pending
        ));
    }

    #[test]
    fn resolver_stream_reemits_when_promote_replaces_routable() {
        let routing = Arc::new(Routing::new());
        let lookup = BleAddressLookup {
            routing: Arc::clone(&routing),
        };
        let self_endpoint = endpoint_id_with_first_byte(0x10);
        let remote_endpoint = endpoint_id_with_first_byte(0xF3);

        let old_id = routing.register_pipe(blew::DeviceId::from("mac-old"), Direction::Outbound);
        routing.insert_routable(
            remote_endpoint,
            old_id,
            crate::transport::routing::Dialer::Low,
        );

        let mut stream = lookup.resolve(remote_endpoint).expect("Some");
        let (counter, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);

        let first = extract_token(&mut stream, &mut cx);
        assert_eq!(first, old_id.as_u64());
        assert!(matches!(
            Pin::new(&mut stream).poll_next(&mut cx),
            Poll::Pending
        ));

        let new_id = routing.register_pipe(blew::DeviceId::from("mac-new"), Direction::Outbound);
        routing.register_pending(new_id, Some(remote_endpoint));
        assert!(
            counter.0.load(AtomicOrdering::SeqCst) >= 1,
            "register_pending must wake the parked resolver"
        );
        assert!(matches!(
            Pin::new(&mut stream).poll_next(&mut cx),
            Poll::Pending
        ));
        let wakes_before_promote = counter.0.load(AtomicOrdering::SeqCst);

        match routing.promote(new_id, &self_endpoint, remote_endpoint) {
            crate::transport::routing::PromoteOutcome::Accepted { evicted } => {
                assert_eq!(evicted, vec![old_id]);
            }
            crate::transport::routing::PromoteOutcome::Rejected => {
                panic!("promote should accept the replacement pipe");
            }
        }

        assert!(
            counter.0.load(AtomicOrdering::SeqCst) > wakes_before_promote,
            "promote must wake the resolver when the routable pipe changes"
        );

        let second = extract_token(&mut stream, &mut cx);
        assert_eq!(second, new_id.as_u64());
        assert_ne!(first, second);
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

    #[cfg(feature = "testing")]
    #[tokio::test]
    async fn resolve_poll_send_and_start_data_pipe_preserve_reserved_token_after_scan_hint_flip() {
        use crate::transport::driver::{Driver, IncomingPacket};
        use crate::transport::peer::{ChannelHandle, ConnectPath, PeerAction};
        use crate::transport::registry::{Registry, SnapshotMaps};
        use crate::transport::test_util::MockBleInterface;

        fn test_transmit(contents: &[u8]) -> Transmit<'_> {
            // iroh 0.98.1 keeps `ecn` private but `poll_send` only reads the
            // public payload fields. Mirror the current layout in tests so the
            // real `BleSender::poll_send` path can still be exercised.
            unsafe {
                std::mem::transmute::<
                    (Option<noq_udp::EcnCodepoint>, &[u8], Option<usize>),
                    Transmit<'_>,
                >((None, contents, None))
            }
        }

        let routing = Arc::new(Routing::new());
        let lookup = BleAddressLookup {
            routing: Arc::clone(&routing),
        };
        let snapshots = Arc::new(ArcSwap::from_pointee(SnapshotMaps::default()));
        let (inbox_tx, mut inbox_rx) = mpsc::channel(8);
        let sender = BleSender {
            inbox: inbox_tx,
            snapshots,
            routing: Arc::clone(&routing),
            tx_bytes: Arc::new(AtomicU64::new(0)),
            inbox_capacity_wakers: Arc::new(Mutex::new(Vec::new())),
        };

        let endpoint = endpoint_id_with_first_byte(0xC1);
        let prefix = crate::transport::routing::prefix_from_endpoint(&endpoint);
        let old_device = blew::DeviceId::from("transport-old-device");
        let new_device = blew::DeviceId::from("transport-new-device");
        routing.note_scan_hint(prefix, old_device.clone());

        let mut stream = lookup.resolve(endpoint).expect("Some");
        let (_counter, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let token = extract_token(&mut stream, &mut cx);
        let reserved_id = StableConnId::from_raw(token);

        let transmit = test_transmit(b"hello");
        assert!(matches!(
            CustomSender::poll_send(&sender, &mut cx, &token_custom_addr(token), &transmit),
            Poll::Ready(Ok(()))
        ));

        let mut reg = Registry::new_for_test();
        reg.handle(PeerCommand::Advertised {
            prefix,
            device: blew::BleDevice {
                id: old_device.clone(),
                name: None,
                rssi: None,
                services: vec![],
            },
            rssi: None,
        });

        let send_cmd = inbox_rx.recv().await.expect("SendDatagram queued");
        let start_actions = reg.handle(send_cmd);
        assert!(matches!(
            start_actions.as_slice(),
            [PeerAction::StartConnect { device_id, .. }] if device_id == &old_device
        ));
        assert_eq!(
            reg.peer(&old_device).unwrap().target_endpoint,
            Some(endpoint)
        );

        routing.note_scan_hint(prefix, new_device);

        let actions = reg.handle(PeerCommand::ConnectSucceeded {
            device_id: old_device.clone(),
            channel: ChannelHandle {
                id: 9,
                path: ConnectPath::Gatt,
            },
        });
        let start_pipe = actions
            .into_iter()
            .find_map(|action| match action {
                PeerAction::StartDataPipe {
                    device_id,
                    tx_gen,
                    role,
                    target_endpoint,
                    path,
                    l2cap_channel,
                } => Some((
                    device_id,
                    tx_gen,
                    role,
                    target_endpoint,
                    path,
                    l2cap_channel,
                )),
                _ => None,
            })
            .expect("StartDataPipe emitted");
        assert_eq!(start_pipe.0, old_device);
        assert_eq!(start_pipe.3, Some(endpoint));
        assert_eq!(start_pipe.4, ConnectPath::Gatt);
        assert!(start_pipe.5.is_none());

        let iface = Arc::new(MockBleInterface::new());
        let (driver_tx, mut driver_rx) = mpsc::channel(8);
        let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(4);
        let driver = Driver::new(
            iface,
            driver_tx,
            incoming_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(crate::transport::store::InMemoryPeerStore::new()),
            Arc::clone(&routing),
        );

        driver
            .execute(PeerAction::StartDataPipe {
                device_id: start_pipe.0.clone(),
                tx_gen: start_pipe.1,
                role: start_pipe.2,
                target_endpoint: start_pipe.3,
                path: start_pipe.4,
                l2cap_channel: start_pipe.5,
            })
            .await;

        let pipes = routing.pipes_for_debug();
        assert_eq!(pipes.len(), 1);
        assert_eq!(pipes[0].id, reserved_id);
        assert_eq!(pipes[0].device_id, old_device);
        assert_eq!(routing.reservation_len(), 0);
        assert_eq!(routing.pending_pipe_for(&endpoint), Some(reserved_id));

        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), driver_rx.recv())
            .await
            .unwrap();
    }
}
