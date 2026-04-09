//! BLE as an iroh custom transport.
//!
//! [`BleTransport`] runs both the central (scanner) and peripheral (advertiser)
//! roles simultaneously.  It advertises the local node's identity via BLE and
//! scans for other iroh-ble peers.  QUIC datagrams are tunnelled over GATT
//! characteristics with a reliable alternating-bit protocol on top of
//! write-without-response / notifications.
//!
//! # Discovery scheme
//!
//! Each peer registers a GATT service whose UUID encodes 12 bytes of its
//! Ed25519 public key:
//!
//! ```text
//!   69726F00-XXXX-XXXX-XXXX-XXXXXXXXXXXX
//! ```
//!
//! Scanning peers identify the advertiser without connecting.
//!
//! # GATT service layout
//!
//! ```text
//! Service: IROH_SERVICE_UUID
//! ├── C2P characteristic (WRITE_WITHOUT_RESPONSE | NOTIFY)
//! │   Central writes here; peripheral receives via write events.
//! │   Carries central->peripheral data + ACKs for P2C data.
//! ├── P2C characteristic (WRITE_WITHOUT_RESPONSE | NOTIFY)
//! │   Peripheral notifies here; central receives via notification events.
//! │   Carries peripheral->central data + ACKs for C2P data.
//! ├── VERSION characteristic (READ) -- 1-byte protocol version
//! └── PSM characteristic (READ, optional) -- 2-byte LE L2CAP PSM
//! ```
//!
//! # Reliable wire protocol
//!
//! See [`reliable`] module for the sliding-window protocol details.
//! Every GATT value (write or notification) carries a 2-byte header:
//!
//! | Byte | Bits | Field    | Meaning |
//! |------|------|----------|---------|
//! | 0    | 0-3  | SEQ      | 4-bit sequence number (0-15) |
//! | 0    | 4    | FIRST    | First fragment of a datagram |
//! | 0    | 5    | LAST     | Last fragment of a datagram |
//! | 1    | 0-3  | ACK_SEQ  | Cumulative ACK sequence |
//! | 1    | 4    | ACK      | ACK_SEQ field is valid |

mod l2cap;
pub mod reliable;

use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::io;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use blew::central::{CentralEvent, ScanFilter, ScanMode, WriteType};
use blew::gatt::props::{AttributePermissions, CharacteristicProperties};
use blew::gatt::service::{GattCharacteristic, GattService};
use blew::peripheral::{AdvertisingConfig, PeripheralEvent};
use blew::types::DeviceId;
use blew::{BlewError, Central, Peripheral};
use iroh::address_lookup::{self, AddressLookup, EndpointData, EndpointInfo, Item};
use iroh::endpoint::Builder;
use iroh::endpoint::presets::Preset;
use iroh::endpoint::transports::{Addr, CustomEndpoint, CustomSender, CustomTransport, Transmit};
use iroh_base::{CustomAddr, EndpointId, TransportAddr};
use n0_watcher::Watchable;
use tokio::sync::{Mutex, mpsc};
use tokio_stream::StreamExt as _;
use tracing::{debug, info, trace, warn};
use uuid::{Uuid, uuid};

use crate::error::BleError;

use self::reliable::ReliableChannel;
/// Unique transport discriminator -- ASCII "BLE".
pub const BLE_TRANSPORT_ID: u64 = 0x42_4C_45;

/// Number of leading public-key bytes encoded in the key-service UUID.
const KEY_PREFIX_LEN: usize = 12;

/// Fixed first 4 bytes of the key-encoded advertising UUID ("iro\0").
const KEY_UUID_PREFIX: [u8; 4] = [0x69, 0x72, 0x6F, 0x00];

/// Default maximum bytes per GATT write/notification.
const DEFAULT_CHUNK_SIZE: usize = 512;

/// GATT service UUID for the iroh BLE transport.
const IROH_SERVICE_UUID: Uuid = uuid!("69726f01-8e45-4c2c-b3a5-331f3098b5c2");

/// Central-to-Peripheral data characteristic.
/// Central writes here; peripheral receives via write events.
const IROH_C2P_CHAR_UUID: Uuid = uuid!("69726f02-8e45-4c2c-b3a5-331f3098b5c2");

/// Peripheral-to-Central data characteristic.
/// Peripheral notifies here; central receives via notification events.
const IROH_P2C_CHAR_UUID: Uuid = uuid!("69726f03-8e45-4c2c-b3a5-331f3098b5c2");

/// L2CAP PSM characteristic -- 2 bytes little-endian u16, READ only.
/// Only published by peripherals that successfully called `l2cap_listener()`.
const IROH_PSM_CHAR_UUID: Uuid = uuid!("69726f04-8e45-4c2c-b3a5-331f3098b5c2");

/// Protocol version characteristic -- 1 byte, READ only.
/// Central reads this after connecting to verify wire compatibility.
const IROH_VERSION_CHAR_UUID: Uuid = uuid!("69726f05-8e45-4c2c-b3a5-331f3098b5c2");

/// Current protocol version.
///
/// Bump this when making breaking changes to the wire format (reliable
/// protocol header, GATT service layout, L2CAP framing, etc.).
const PROTOCOL_VERSION: u8 = 1;

/// Maximum connection retry attempts for transient GATT errors.
const CONNECT_RETRY_COUNT: u32 = 3;

/// Base backoff delay between connection retries.
const CONNECT_RETRY_BACKOFF_BASE: Duration = Duration::from_millis(200);

/// Time-to-live for discovered peer entries before eviction.
const DISCOVERY_TTL: Duration = Duration::from_secs(300);

/// Build a [`CustomAddr`] from a BLE device key string.
///
/// The transport uses `DeviceId` (BLE device address) as the routing key —
/// QUIC's connection IDs handle endpoint identity, so there is no need to
/// embed the `EndpointId` in the address.
fn ble_custom_addr(device_key: &str) -> CustomAddr {
    CustomAddr::from_parts(BLE_TRANSPORT_ID, device_key.as_bytes())
}

/// Extract a device key string from a [`CustomAddr`].
fn parse_custom_addr(addr: &CustomAddr) -> io::Result<String> {
    if addr.id() != BLE_TRANSPORT_ID {
        return Err(io::Error::other("not a BLE transport address"));
    }
    String::from_utf8(addr.data().to_vec())
        .map_err(|_| io::Error::other("invalid device key in BLE custom addr"))
}

/// Build a 128-bit UUID that encodes 12 bytes of public key.
fn iroh_key_uuid(endpoint_id: &EndpointId) -> Uuid {
    let key = endpoint_id.as_bytes();
    let mut bytes = [0u8; 16];
    bytes[..4].copy_from_slice(&KEY_UUID_PREFIX);
    bytes[4..16].copy_from_slice(&key[..KEY_PREFIX_LEN]);
    Uuid::from_bytes(bytes)
}

/// Extract 12 key-prefix bytes from a key-encoded service UUID.
fn parse_iroh_key_uuid(uuid: &Uuid) -> Option<[u8; KEY_PREFIX_LEN]> {
    let bytes = uuid.as_bytes();
    if bytes[..4] != KEY_UUID_PREFIX {
        return None;
    }
    let mut prefix = [0u8; KEY_PREFIX_LEN];
    prefix.copy_from_slice(&bytes[4..16]);
    Some(prefix)
}

/// Parse the PSM value from the PSM characteristic bytes.
///
/// Returns `None` when the byte slice is too short, or the parsed value is
/// zero (reserved meaning "no L2CAP available").
fn parse_psm_value(bytes: &[u8]) -> Option<blew::l2cap::types::Psm> {
    if bytes.len() < 2 {
        return None;
    }
    let v = u16::from_le_bytes([bytes[0], bytes[1]]);
    if v == 0 {
        return None;
    }
    Some(blew::l2cap::types::Psm(v))
}

/// Extract the key prefix from a full [`EndpointId`].
fn key_to_prefix(endpoint_id: &EndpointId) -> [u8; KEY_PREFIX_LEN] {
    let mut prefix = [0u8; KEY_PREFIX_LEN];
    prefix.copy_from_slice(&endpoint_id.as_bytes()[..KEY_PREFIX_LEN]);
    prefix
}
/// Data channel carrying QUIC datagrams to/from a single peer.
///
/// Determined at connection-setup time and fixed for the lifetime of the
/// `PeerChannel`.
pub(super) enum DataChannel {
    /// GATT write-without-response + notify path using the reliable ARQ.
    Gatt(Arc<ReliableChannel>),
    /// L2CAP CoC stream path with length-prefixed datagrams.
    #[allow(dead_code)]
    L2cap {
        tx: tokio::sync::mpsc::Sender<Vec<u8>>,
        send_task: tokio::task::JoinHandle<()>,
        recv_task: tokio::task::JoinHandle<()>,
    },
}

impl DataChannel {
    #[allow(dead_code)]
    async fn send_datagram(&self, data: Vec<u8>) -> Result<(), BleError> {
        match self {
            Self::Gatt(ch) => {
                ch.enqueue_datagram(data).await;
                Ok(())
            }
            Self::L2cap { tx, .. } => tx
                .send(data)
                .await
                .map_err(|_| BleError::ChannelError("l2cap send channel closed".into())),
        }
    }
}

impl Drop for DataChannel {
    fn drop(&mut self) {
        if let Self::L2cap {
            send_task,
            recv_task,
            ..
        } = self
        {
            send_task.abort();
            recv_task.abort();
        }
    }
}

struct PeerChannel {
    channel: DataChannel,
    /// GATT-only; `None` for L2CAP (which manages its own I/O tasks).
    send_loop: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for PeerChannel {
    fn drop(&mut self) {
        if let Some(h) = self.send_loop.take() {
            h.abort();
        }
    }
}
/// A received datagram ready for delivery to iroh.
pub(super) struct IncomingPacket {
    /// BLE device key (DeviceId string) that sent this packet.
    pub(super) device_key: String,
    pub(super) data: Vec<u8>,
}

/// State shared across the transport.
///
/// # Lock ordering
///
/// When acquiring multiple locks, always follow this order to avoid deadlocks:
///
/// 1. `connection_locks` (per-device Mutex)
/// 2. `peer_channels` (tokio Mutex)
/// 3. `active_connections` (RwLock)
/// 4. `discovered` / `discovered_at` (RwLock)
/// 5. `scan_low_power` (tokio Mutex)
///
/// Never hold a higher-numbered lock while acquiring a lower-numbered one.
struct BleTransportState {
    central: Arc<Central>,
    peripheral: Arc<Peripheral>,
    local_id: EndpointId,

    discovered: RwLock<HashMap<[u8; KEY_PREFIX_LEN], DeviceId>>,
    discovered_at: RwLock<HashMap<[u8; KEY_PREFIX_LEN], std::time::Instant>>,
    peer_channels: Mutex<HashMap<String, Arc<PeerChannel>>>,
    active_connections: RwLock<HashSet<String>>,

    /// Reduced duty cycle while active connections exist to lower radio contention.
    scan_low_power: Mutex<bool>,

    /// Per-device locks to serialise connection setup.
    connection_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,

    incoming_tx: mpsc::Sender<IncomingPacket>,
    /// Taken by `bind()`; only one consumer allowed.
    incoming_rx: Mutex<Option<mpsc::Receiver<IncomingPacket>>>,

    events_total: AtomicU64,
    events_discovery: AtomicU64,
    events_discovery_iroh: AtomicU64,
    events_lagged: AtomicU64,
    bytes_tx: Arc<AtomicU64>,
    bytes_rx: Arc<AtomicU64>,
    retransmits: Arc<AtomicU64>,
}

impl std::fmt::Debug for BleTransportState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BleTransportState")
            .field("local_id", &self.local_id)
            .finish_non_exhaustive()
    }
}

/// A BLE custom transport for iroh.
///
/// Create one with [`BleTransport::new`], then pass it to an iroh
/// [`Endpoint`](iroh::Endpoint) via [`BleTransport::preset`].
#[derive(Debug, Clone)]
pub struct BleTransport {
    state: Arc<BleTransportState>,
}

impl BleTransport {
    /// Initialise the BLE transport.
    pub async fn new(secret_key: &iroh::SecretKey) -> Result<Self, BleError> {
        let local_id = secret_key.public();
        info!(endpoint_id = %local_id, "initialising BLE transport");

        let peripheral: Peripheral = Peripheral::new().await?;
        let central: Central = Central::new().await?;

        // Subscribe before checking power state so we don't miss the initial update.
        let mut periph_events = peripheral.events();

        let peripheral = Arc::new(peripheral);
        let central = Arc::new(central);

        if !peripheral.is_powered().await? {
            info!("waiting for Bluetooth adapter to power on");
            tokio::time::timeout(std::time::Duration::from_secs(15), async {
                loop {
                    match periph_events.next().await {
                        Some(PeripheralEvent::AdapterStateChanged { powered: true }) => {
                            info!("Bluetooth adapter powered on");
                            break Ok(());
                        }
                        Some(PeripheralEvent::AdapterStateChanged { powered: false }) => {
                            // Transient state (Unknown / Resetting): keep waiting.
                        }
                        None => {
                            break Err(BleError::PeripheralError(
                                "event stream closed during init".to_string(),
                            ));
                        }
                        _ => {}
                    }
                }
            })
            .await
            .map_err(|_| {
                BleError::PeripheralError(
                    "timed out waiting for Bluetooth to power on (is BT enabled and authorised?)"
                        .to_string(),
                )
            })??;
        }

        // Start L2CAP listener before GATT registration so the PSM is available
        // for the PSM characteristic value.
        let l2cap_psm = match peripheral.l2cap_listener().await {
            Ok((psm, stream)) => {
                info!(psm = psm.value(), "L2CAP listener active");
                Some((psm, stream))
            }
            Err(BlewError::NotSupported) => {
                info!("L2CAP not supported on this backend; using GATT data path only");
                None
            }
            Err(e) => {
                warn!(
                    ?e,
                    "l2cap_listener failed; continuing with GATT data path only"
                );
                None
            }
        };

        register_gatt_services(
            &peripheral,
            &local_id,
            l2cap_psm.as_ref().map(|(psm, _)| *psm),
        )
        .await?;

        let (incoming_tx, incoming_rx) = mpsc::channel::<IncomingPacket>(256);

        let state = Arc::new(BleTransportState {
            central,
            peripheral,
            local_id,
            discovered: RwLock::new(HashMap::new()),
            discovered_at: RwLock::new(HashMap::new()),
            peer_channels: Mutex::new(HashMap::new()),
            active_connections: RwLock::new(HashSet::new()),
            connection_locks: Mutex::new(HashMap::new()),
            incoming_tx: incoming_tx.clone(),
            incoming_rx: Mutex::new(Some(incoming_rx)),
            events_total: AtomicU64::new(0),
            events_discovery: AtomicU64::new(0),
            events_discovery_iroh: AtomicU64::new(0),
            events_lagged: AtomicU64::new(0),
            bytes_tx: Arc::new(AtomicU64::new(0)),
            bytes_rx: Arc::new(AtomicU64::new(0)),
            retransmits: Arc::new(AtomicU64::new(0)),
            scan_low_power: Mutex::new(false),
        });

        if let Some((_, stream)) = l2cap_psm {
            let accept_state = Arc::clone(&state);
            tokio::spawn(async move {
                l2cap_accept_loop(accept_state, Box::pin(stream)).await;
            });
        }

        spawn_event_loops(&state, incoming_tx.clone(), periph_events);
        spawn_health_check(Arc::clone(&state));

        state.central.start_scan(ScanFilter::default()).await?;
        info!("scanning for iroh-ble peers (unfiltered)");

        Ok(BleTransport { state })
    }

    /// Return an [`AddressLookup`] backed by BLE discovery.
    pub fn address_lookup(&self) -> BleAddressLookup {
        BleAddressLookup {
            state: self.state.clone(),
        }
    }

    /// Return a [`Preset`] that wires this transport into an iroh endpoint.
    pub fn preset(&self) -> BlePreset {
        BlePreset {
            transport: Arc::new(self.clone()),
            lookup: self.address_lookup(),
        }
    }

    /// Snapshot of cumulative byte counters for bandwidth computation.
    pub fn stats(&self) -> TransportStats {
        TransportStats {
            tx_bytes: self.state.bytes_tx.load(Ordering::Relaxed),
            rx_bytes: self.state.bytes_rx.load(Ordering::Relaxed),
        }
    }
}

/// Cumulative transport statistics.
#[derive(Debug, Clone, Copy)]
pub struct TransportStats {
    /// Total bytes sent over BLE since transport creation.
    pub tx_bytes: u64,
    /// Total bytes received over BLE since transport creation.
    pub rx_bytes: u64,
}

async fn register_gatt_services(
    peripheral: &Peripheral,
    local_id: &EndpointId,
    l2cap_psm: Option<blew::l2cap::types::Psm>,
) -> Result<Uuid, BleError> {
    let mut characteristics = vec![
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
    ];
    characteristics.push(GattCharacteristic {
        uuid: IROH_VERSION_CHAR_UUID,
        properties: CharacteristicProperties::READ,
        permissions: AttributePermissions::READ,
        value: vec![PROTOCOL_VERSION],
        descriptors: vec![],
    });
    if let Some(psm) = l2cap_psm {
        characteristics.push(GattCharacteristic {
            uuid: IROH_PSM_CHAR_UUID,
            properties: CharacteristicProperties::READ,
            permissions: AttributePermissions::READ,
            value: psm.value().to_le_bytes().to_vec(),
            descriptors: vec![],
        });
    }
    peripheral
        .add_service(&GattService {
            uuid: IROH_SERVICE_UUID,
            primary: true,
            characteristics,
        })
        .await?;

    let key_uuid = iroh_key_uuid(local_id);
    peripheral
        .add_service(&GattService {
            uuid: key_uuid,
            primary: false,
            characteristics: vec![],
        })
        .await?;
    debug!(service = %IROH_SERVICE_UUID, key_service = %key_uuid, "registered GATT services");

    peripheral
        .start_advertising(&AdvertisingConfig {
            local_name: "iroh".to_string(),
            service_uuids: vec![key_uuid],
        })
        .await?;
    info!(key_uuid = %key_uuid, "advertising started");

    Ok(key_uuid)
}

fn spawn_health_check(state: Arc<BleTransportState>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        interval.tick().await;
        let mut prev_tx = 0u64;
        let mut prev_rx = 0u64;
        loop {
            interval.tick().await;
            let disc = state.events_discovery.load(Ordering::Relaxed);
            let iroh = state.events_discovery_iroh.load(Ordering::Relaxed);
            let lagged = state.events_lagged.load(Ordering::Relaxed);
            let peers = state.discovered.read().len();
            let channels = state.peer_channels.try_lock().map(|c| c.len()).unwrap_or(0);
            let active = state.active_connections.read().len();
            let retransmits = state.retransmits.swap(0, Ordering::Relaxed);

            let window_secs = 10.0f64;
            let tx_now = state.bytes_tx.load(Ordering::Relaxed);
            let rx_now = state.bytes_rx.load(Ordering::Relaxed);
            let tx_bytes = tx_now - prev_tx;
            let rx_bytes = rx_now - prev_rx;
            prev_tx = tx_now;
            prev_rx = rx_now;
            let tx_kbps = (tx_bytes as f64 * 8.0) / (window_secs * 1000.0);
            let rx_kbps = (rx_bytes as f64 * 8.0) / (window_secs * 1000.0);

            trace!(
                discoveries = disc,
                iroh_peers = iroh,
                lagged_events = lagged,
                discovered_peers = peers,
                peer_channels = channels,
                active_connections = active,
                retransmits,
                tx_kbps = format!("{tx_kbps:.1}"),
                rx_kbps = format!("{rx_kbps:.1}"),
                "health check"
            );

            // Evict discovered peers that haven't been seen for 5 minutes.
            {
                let ttl = DISCOVERY_TTL;
                let now = std::time::Instant::now();
                let mut at = state.discovered_at.write();
                let mut disc_map = state.discovered.write();
                at.retain(|k, ts| {
                    if now.duration_since(*ts) > ttl {
                        disc_map.remove(k);
                        false
                    } else {
                        true
                    }
                });
            }

            // Restart scan periodically to pick up new peers.
            // On Android, avoid restarting -- the OS throttles scan starts
            // (max 5 per 30 s) and silently drops results after that.
            #[cfg(not(target_os = "android"))]
            {
                let is_low_power = *state.scan_low_power.lock().await;
                let mode = if is_low_power {
                    ScanMode::LowPower
                } else {
                    ScanMode::LowLatency
                };
                let _ = state.central.stop_scan().await;
                if let Err(e) = state
                    .central
                    .start_scan(ScanFilter {
                        mode,
                        ..Default::default()
                    })
                    .await
                {
                    warn!(err = %e, "failed to restart scan");
                }
            }
        }
    });
}

fn spawn_event_loops(
    state: &Arc<BleTransportState>,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    periph_events: impl tokio_stream::Stream<Item = PeripheralEvent> + Send + Unpin + 'static,
) {
    {
        let handler_state = Arc::clone(state);
        let tx = incoming_tx.clone();
        let mut events = state.central.events();
        tokio::spawn(async move {
            while let Some(event) = events.next().await {
                handler_state.events_total.fetch_add(1, Ordering::Relaxed);
                handle_central_event(event, Arc::clone(&handler_state), &tx).await;
            }
        });
    }

    {
        let handler_state = Arc::clone(state);
        let tx = incoming_tx;
        let mut events = periph_events;
        tokio::spawn(async move {
            while let Some(event) = events.next().await {
                handler_state.events_total.fetch_add(1, Ordering::Relaxed);
                handle_peripheral_event(event, Arc::clone(&handler_state), &tx).await;
            }
        });
    }
}

impl CustomTransport for BleTransport {
    fn bind(&self) -> io::Result<Box<dyn CustomEndpoint>> {
        let incoming_rx = self
            .state
            .incoming_rx
            .try_lock()
            .map_err(|_| io::Error::other("BLE transport bind() contention"))?
            .take()
            .ok_or_else(|| io::Error::other("BLE transport bind() already called"))?;

        // Local addr uses "local" as a sentinel — the actual routing is
        // done via DeviceId-based CustomAddrs returned by AddressLookup.
        let watchable = Watchable::new(vec![ble_custom_addr("local")]);
        let sender = Arc::new(BleSender {
            state: self.state.clone(),
        });

        Ok(Box::new(BleEndpoint {
            receiver: incoming_rx,
            watchable,
            sender,
        }))
    }
}
#[derive(Debug)]
struct BleEndpoint {
    receiver: mpsc::Receiver<IncomingPacket>,
    watchable: Watchable<Vec<CustomAddr>>,
    sender: Arc<BleSender>,
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
        cx: &mut Context,
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
                        trace!(
                            len = packet.data.len(),
                            buf_len = bufs[filled].len(),
                            "poll_recv: buffer too small, skipping packet"
                        );
                        continue;
                    }
                    trace!(
                        device = %packet.device_key,
                        len = packet.data.len(),
                        "poll_recv: delivering packet to iroh"
                    );
                    bufs[filled][..packet.data.len()].copy_from_slice(&packet.data);
                    metas[filled].len = packet.data.len();
                    metas[filled].stride = packet.data.len();
                    source_addrs[filled] = Addr::Custom(ble_custom_addr(&packet.device_key));
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
struct BleSender {
    state: Arc<BleTransportState>,
}

impl std::fmt::Debug for BleSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BleSender").finish()
    }
}

impl CustomSender for BleSender {
    fn is_valid_send_addr(&self, addr: &CustomAddr) -> bool {
        addr.id() == BLE_TRANSPORT_ID && !addr.data().is_empty()
    }

    fn poll_send(
        &self,
        cx: &mut Context,
        dst: &CustomAddr,
        transmit: &Transmit<'_>,
    ) -> Poll<io::Result<()>> {
        let device_key = match parse_custom_addr(dst) {
            Ok(k) => k,
            Err(e) => return Poll::Ready(Err(e)),
        };

        let peer_channel = {
            let channels = match self.state.peer_channels.try_lock() {
                Ok(c) => c,
                Err(_) => {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
            };
            channels.get(&device_key).map(Arc::clone)
        };

        let peer_channel = match peer_channel {
            Some(c) => c,
            None => {
                let state = self.state.clone();
                let data = transmit.contents.to_vec();
                let waker = cx.waker().clone();
                tokio::spawn(async move {
                    if let Err(e) = ensure_connected_and_send(&state, &device_key, data).await {
                        warn!(device = %device_key, err = %e, "send failed (no channel)");
                    }
                    waker.wake();
                });
                return Poll::Ready(Ok(()));
            }
        };

        let data = transmit.contents.to_vec();
        let waker = cx.waker().clone();
        match &peer_channel.channel {
            DataChannel::Gatt(rel) => {
                let enqueued = rel.try_enqueue_datagram(data.clone());
                match enqueued {
                    Some(true) => Poll::Ready(Ok(())),
                    Some(false) => {
                        rel.register_send_waker(cx.waker());
                        Poll::Pending
                    }
                    None => {
                        let rel = Arc::clone(rel);
                        tokio::spawn(async move {
                            rel.enqueue_datagram(data).await;
                            waker.wake();
                        });
                        Poll::Ready(Ok(()))
                    }
                }
            }
            DataChannel::L2cap { tx, .. } => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    if tx.send(data).await.is_err() {
                        warn!("l2cap send channel closed");
                    }
                    waker.wake();
                });
                Poll::Ready(Ok(()))
            }
        }
    }
}

/// Establish a connection to a peer and send the first datagram.
async fn ensure_connected_and_send(
    state: &Arc<BleTransportState>,
    device_key: &str,
    data: Vec<u8>,
) -> io::Result<()> {
    let device_id: DeviceId = DeviceId::from(device_key);

    let mut last_err = None;
    for attempt in 0..CONNECT_RETRY_COUNT {
        if attempt > 0 {
            let backoff = CONNECT_RETRY_BACKOFF_BASE * 2u32.pow(attempt - 1);
            warn!(
                device = %device_key,
                attempt,
                backoff_ms = backoff.as_millis() as u64,
                "retrying connection after transient error"
            );
            tokio::time::sleep(backoff).await;
        }
        match ensure_connected(state, &device_id).await {
            Ok(()) => {
                let pc = state.peer_channels.lock().await.get(device_key).cloned();
                let Some(pc) = pc else {
                    return Err(io::Error::other(
                        "peer channel missing after ensure_connected",
                    ));
                };
                match &pc.channel {
                    DataChannel::Gatt(rel) => {
                        rel.enqueue_datagram(data).await;
                    }
                    DataChannel::L2cap { tx, .. } => {
                        tx.send(data)
                            .await
                            .map_err(|_| io::Error::other("l2cap send channel closed"))?;
                    }
                }
                return Ok(());
            }
            Err(e) => {
                let is_transient = matches!(
                    &e,
                    BleError::GattBusy(_) | BleError::Timeout | BleError::GattError(_)
                );
                if !is_transient {
                    return Err(io::Error::other(e));
                }
                warn!(device = %device_key, err = %e, "transient connection error");
                last_err = Some(io::Error::other(e));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| io::Error::other("connection failed after retries")))
}
/// Attempt to establish an L2CAP data channel to a connected peripheral.
///
/// Returns `Some(DataChannel::L2cap {...})` on success.
/// Returns `None` on any failure -- caller should fall back to GATT.
async fn try_l2cap_path(
    state: &Arc<BleTransportState>,
    device_id: &DeviceId,
) -> Option<DataChannel> {
    let psm_bytes = state
        .central
        .read_characteristic(device_id, IROH_PSM_CHAR_UUID)
        .await
        .ok()?;
    let psm = parse_psm_value(&psm_bytes)?;

    let channel = state
        .central
        .open_l2cap_channel(device_id, psm)
        .await
        .ok()?;

    let (reader, mut writer) = tokio::io::split(channel);

    // Central sends its identity so the peripheral knows who connected.
    let local_id_bytes = *state.local_id.as_bytes();
    l2cap::write_identity(&mut writer, &local_id_bytes)
        .await
        .ok()?;

    let (tx, send_task, recv_task) = l2cap::spawn_l2cap_io_tasks(
        reader,
        writer,
        device_id.to_string(),
        state.incoming_tx.clone(),
        Arc::clone(&state.bytes_tx),
        Arc::clone(&state.bytes_rx),
    );

    Some(DataChannel::L2cap {
        tx,
        send_task,
        recv_task,
    })
}

/// Accept incoming L2CAP channels and dispatch each to [`handle_incoming_l2cap`].
async fn l2cap_accept_loop<S>(state: Arc<BleTransportState>, mut stream: S)
where
    S: tokio_stream::Stream<Item = blew::BlewResult<blew::l2cap::L2capChannel>>
        + Send
        + Unpin
        + 'static,
{
    while let Some(result) = stream.next().await {
        let channel = match result {
            Ok(ch) => ch,
            Err(e) => {
                warn!(?e, "l2cap accept error");
                continue;
            }
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_incoming_l2cap(state, channel).await {
                warn!(?e, "l2cap inbound handler failed");
            }
        });
    }
    debug!("l2cap accept loop exited (stream closed)");
}

/// Handle one incoming L2CAP channel: read central's identity, install `PeerChannel`.
///
/// Called from the L2CAP accept loop per incoming channel (peripheral side).
/// The central writes its 32-byte `EndpointId` first so the peripheral can
/// look up the `DeviceId` from the discovery map and key the channel correctly.
async fn handle_incoming_l2cap(
    state: Arc<BleTransportState>,
    channel: blew::l2cap::L2capChannel,
) -> Result<(), BleError> {
    let (mut reader, writer) = tokio::io::split(channel);
    // Central sends its identity first; we read it to find the DeviceId.
    let remote_bytes = l2cap::read_identity(&mut reader, std::time::Duration::from_secs(10))
        .await
        .map_err(|e| BleError::PeripheralError(format!("l2cap identity read: {e}")))?;
    let remote_id = EndpointId::from_bytes(&remote_bytes)
        .map_err(|_| BleError::PeripheralError("l2cap: invalid remote identity".into()))?;

    // Look up the DeviceId from our discovery map.
    let device_key = state
        .discovered
        .read()
        .get(&key_to_prefix(&remote_id))
        .map(|d| d.to_string())
        .unwrap_or_else(|| {
            // Fallback: use a synthetic key if we haven't discovered this peer yet.
            use std::fmt::Write as FmtWrite;
            let hex = remote_id.as_bytes().iter().fold(String::new(), |mut s, b| {
                let _ = write!(s, "{b:02x}");
                s
            });
            format!("l2cap-{hex}")
        });

    // Register the peer channel and spawn the recv task while holding the
    // peer_channels lock. This ensures iroh can route responses via poll_send
    // as soon as the recv task delivers the first inbound QUIC packet.
    // Without this atomicity, iroh may try to respond before the channel is
    // registered, triggering a redundant GATT connection attempt.
    {
        let mut channels = state.peer_channels.lock().await;

        // Supersede any existing GATT channel for this device -- L2CAP is preferred.
        if let Some(pc) = channels.remove(&device_key) {
            if let Some(handle) = &pc.send_loop {
                handle.abort();
            }
            info!(
                device = %device_key,
                "L2CAP supersedes existing GATT channel"
            );
        }

        let (tx, send_task, recv_task) = l2cap::spawn_l2cap_io_tasks(
            reader,
            writer,
            device_key.clone(),
            state.incoming_tx.clone(),
            Arc::clone(&state.bytes_tx),
            Arc::clone(&state.bytes_rx),
        );
        let peer_channel = Arc::new(PeerChannel {
            channel: DataChannel::L2cap {
                tx,
                send_task,
                recv_task,
            },
            send_loop: None,
        });
        channels.insert(device_key.clone(), peer_channel);
    }
    // Update active_connections outside the peer_channels lock.
    let first_connection = {
        let mut active = state.active_connections.write();
        let first = active.is_empty();
        active.insert(device_key.clone());
        first
    };
    if first_connection {
        switch_scan_mode(&state, ScanMode::LowPower).await;
    }

    debug!(device = %device_key, "L2CAP inbound connection ready");
    Ok(())
}

async fn register_peer_channel(
    state: &Arc<BleTransportState>,
    device_key: String,
    peer_channel: Arc<PeerChannel>,
) {
    state
        .peer_channels
        .lock()
        .await
        .insert(device_key.clone(), peer_channel);
    let first_connection = {
        let mut active = state.active_connections.write();
        let first = active.is_empty();
        active.insert(device_key);
        first
    };
    if first_connection {
        switch_scan_mode(state, ScanMode::LowPower).await;
    }
}

async fn unregister_peer(state: &Arc<BleTransportState>, device_key: &str) {
    state.peer_channels.lock().await.remove(device_key);
    let no_connections = {
        let mut active = state.active_connections.write();
        active.remove(device_key);
        active.is_empty()
    };
    if no_connections {
        switch_scan_mode(state, ScanMode::LowLatency).await;
    }
}

fn spawn_gatt_send_loop<F, Fut>(
    channel: Arc<ReliableChannel>,
    device_key: String,
    state: Arc<BleTransportState>,
    send_fn: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn(Vec<u8>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<(), BlewError>> + Send,
{
    tokio::spawn(async move {
        channel
            .run_send_loop(|data| {
                let len = data.len();
                let bytes_tx = state.bytes_tx.clone();
                let fut = send_fn(data);
                async move {
                    let result = fut.await.map_err(|e| e.to_string());
                    if result.is_ok() {
                        bytes_tx.fetch_add(len as u64, Ordering::Relaxed);
                    }
                    result
                }
            })
            .await;
        warn!(device = %device_key, "send loop ended, cleaning up peer");
        unregister_peer(&state, &device_key).await;
    })
}

async fn switch_scan_mode(state: &Arc<BleTransportState>, mode: ScanMode) {
    let mut current = state.scan_low_power.lock().await;
    let want_low_power = mode == ScanMode::LowPower;
    if *current == want_low_power {
        return;
    }
    *current = want_low_power;
    drop(current);

    let label = if want_low_power {
        "low-power"
    } else {
        "full-speed"
    };
    info!("switching BLE scan to {label}");

    let _ = state.central.stop_scan().await;
    if let Err(e) = state
        .central
        .start_scan(ScanFilter {
            mode,
            ..Default::default()
        })
        .await
    {
        warn!(err = %e, "failed to restart scan in {label} mode");
    }
}

/// Ensure we are connected to `device_id` as a central, with a data channel
/// established (L2CAP preferred, GATT fallback).
async fn ensure_connected(
    state: &Arc<BleTransportState>,
    device_id: &DeviceId,
) -> Result<(), BleError> {
    let device_key = device_id.to_string();

    if state.peer_channels.lock().await.contains_key(&device_key) {
        return Ok(());
    }

    let lock = {
        let mut locks = state.connection_locks.lock().await;
        locks
            .entry(device_key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    let _guard = lock.lock().await;

    if state.peer_channels.lock().await.contains_key(&device_key) {
        return Ok(());
    }

    info!(device = %device_key, "connecting to peer");

    let result = ensure_connected_inner(state, device_id, device_key.clone()).await;
    state.connection_locks.lock().await.remove(&device_key);

    result
}

async fn ensure_connected_inner(
    state: &Arc<BleTransportState>,
    device_id: &DeviceId,
    device_key: String,
) -> Result<(), BleError> {
    state
        .central
        .connect(device_id)
        .await
        .map_err(BleError::from)?;
    debug!(device = %device_key, "BLE connection established");

    state
        .central
        .discover_services(device_id)
        .await
        .map_err(BleError::from)?;
    debug!(device = %device_key, "GATT services discovered");

    // Check protocol version before proceeding.
    let version_bytes = state
        .central
        .read_characteristic(device_id, IROH_VERSION_CHAR_UUID)
        .await
        .map_err(BleError::from)?;
    let remote_version = *version_bytes
        .first()
        .ok_or_else(|| BleError::GattError("empty version characteristic".into()))?;
    if remote_version != PROTOCOL_VERSION {
        return Err(BleError::ProtocolVersionMismatch {
            local: PROTOCOL_VERSION,
            remote: remote_version,
        });
    }
    debug!(device = %device_key, version = remote_version, "protocol version verified");

    // Try L2CAP first — it's the preferred high-throughput path.
    if let Some(data_channel) = try_l2cap_path(state, device_id).await {
        debug!(%device_key, "established L2CAP data path");
        let pc = Arc::new(PeerChannel {
            channel: data_channel,
            send_loop: None,
        });
        register_peer_channel(state, device_key.clone(), pc).await;
        info!(device = %device_key, "outbound L2CAP connection fully established");
        return Ok(());
    }

    // GATT fallback.
    debug!(%device_key, "using GATT data path");

    state
        .central
        .subscribe_characteristic(device_id, IROH_P2C_CHAR_UUID)
        .await
        .map_err(BleError::from)?;
    debug!(device = %device_key, "subscribed to P2C notifications");

    let (channel, datagram_rx) =
        ReliableChannel::new(DEFAULT_CHUNK_SIZE, Arc::clone(&state.retransmits));
    let channel = Arc::new(channel);

    let send_loop = {
        let central = Arc::clone(&state.central);
        let device = device_id.clone();
        spawn_gatt_send_loop(
            channel.clone(),
            device_key.clone(),
            state.clone(),
            move |data| {
                let central = Arc::clone(&central);
                let device = device.clone();
                async move {
                    central
                        .write_characteristic(
                            &device,
                            IROH_C2P_CHAR_UUID,
                            data,
                            WriteType::WithoutResponse,
                        )
                        .await
                }
            },
        )
    };

    let pc = Arc::new(PeerChannel {
        channel: DataChannel::Gatt(Arc::clone(&channel)),
        send_loop: Some(send_loop),
    });

    register_peer_channel(state, device_key.clone(), pc).await;
    spawn_datagram_delivery(device_key.clone(), datagram_rx, state.incoming_tx.clone());

    info!(device = %device_key, "outbound connection fully established");

    Ok(())
}
async fn handle_central_event(
    event: CentralEvent,
    state: Arc<BleTransportState>,
    incoming_tx: &mpsc::Sender<IncomingPacket>,
) {
    match event {
        CentralEvent::DeviceDiscovered(device) => {
            state.events_discovery.fetch_add(1, Ordering::Relaxed);

            let prefix = device.services.iter().find_map(parse_iroh_key_uuid);

            if let Some(prefix) = prefix {
                state.events_discovery_iroh.fetch_add(1, Ordering::Relaxed);
                let is_new = !state.discovered.read().contains_key(&prefix);
                if is_new {
                    info!(
                        device = %device.id,
                        name = device.name.as_deref().unwrap_or("N/A"),
                        n_services = device.services.len(),
                        "discovered iroh peer via key UUID"
                    );
                }
                state.discovered.write().insert(prefix, device.id);
                state
                    .discovered_at
                    .write()
                    .insert(prefix, std::time::Instant::now());
            }
        }

        CentralEvent::CharacteristicNotification {
            device_id,
            char_uuid,
            value,
        } => {
            if char_uuid == IROH_P2C_CHAR_UUID && !value.is_empty() {
                state
                    .bytes_rx
                    .fetch_add(value.len() as u64, Ordering::Relaxed);
                handle_incoming_fragment(&device_id, &value, &state, incoming_tx).await;
            }
        }

        CentralEvent::DeviceConnected { device_id } => {
            debug!(device = %device_id, "BLE DeviceConnected");
        }

        CentralEvent::DeviceDisconnected { device_id } => {
            let device_key = device_id.to_string();
            info!(device = %device_key, "peer disconnected");
            // Drop immediately to abort background tasks rather than waiting
            // for MAX_RETRIES * ACK_TIMEOUT_MAX (~75s) of zombie retransmits.
            unregister_peer(&state, &device_key).await;
        }
    }
}
async fn handle_peripheral_event(
    event: PeripheralEvent,
    state: Arc<BleTransportState>,
    incoming_tx: &mpsc::Sender<IncomingPacket>,
) {
    match event {
        PeripheralEvent::ReadRequest { responder, .. } => {
            responder.respond(vec![]);
        }

        PeripheralEvent::WriteRequest {
            client_id,
            char_uuid,
            value,
            responder,
            ..
        } => {
            if char_uuid == IROH_C2P_CHAR_UUID && !value.is_empty() {
                state
                    .bytes_rx
                    .fetch_add(value.len() as u64, Ordering::Relaxed);
                handle_incoming_fragment(&client_id, &value, &state, incoming_tx).await;
            }
            if let Some(r) = responder {
                r.success();
            }
        }

        PeripheralEvent::SubscriptionChanged {
            client_id,
            char_uuid,
            subscribed,
        } => {
            trace!(
                device = %client_id,
                char = %char_uuid,
                subscribed,
                "SubscriptionChanged"
            );
        }

        PeripheralEvent::AdapterStateChanged { powered } => {
            trace!(powered, "adapter state changed");
        }
    }
}
/// Handle an incoming fragment (write or notification) for a peer.
///
/// If no channel exists yet for this peer (inbound connection), one is created
/// eagerly. The first reliable datagram is expected to be the 32-byte identity.
async fn handle_incoming_fragment(
    remote_device: &DeviceId,
    value: &[u8],
    state: &Arc<BleTransportState>,
    incoming_tx: &mpsc::Sender<IncomingPacket>,
) {
    let device_key = remote_device.to_string();

    {
        let channels = state.peer_channels.lock().await;
        if let Some(pc) = channels.get(&device_key) {
            if let DataChannel::Gatt(rel) = &pc.channel {
                let rel = Arc::clone(rel);
                drop(channels);
                rel.receive_fragment(value).await;
            }
            return;
        }
    }

    // New inbound peer -- create a ReliableChannel eagerly so we can
    // process fragments immediately. The first complete datagram from
    // the reliable channel will be the 32-byte identity.
    let (channel, datagram_rx) =
        ReliableChannel::new(DEFAULT_CHUNK_SIZE, Arc::clone(&state.retransmits));
    let channel = Arc::new(channel);

    // Feed the current fragment before spawning the send loop so it
    // isn't lost.
    channel.receive_fragment(value).await;

    let send_loop = {
        let peripheral = Arc::clone(&state.peripheral);
        spawn_gatt_send_loop(
            channel.clone(),
            device_key.clone(),
            state.clone(),
            move |data| {
                let peripheral = Arc::clone(&peripheral);
                async move {
                    peripheral
                        .notify_characteristic(IROH_P2C_CHAR_UUID, data)
                        .await
                }
            },
        )
    };

    let pc = Arc::new(PeerChannel {
        channel: DataChannel::Gatt(Arc::clone(&channel)),
        send_loop: Some(send_loop),
    });

    register_peer_channel(state, device_key.clone(), pc).await;
    spawn_datagram_delivery(device_key, datagram_rx, incoming_tx.clone());
}

/// Spawn a task that routes completed datagrams from a peer channel to iroh.
fn spawn_datagram_delivery(
    device_key: String,
    mut rx: mpsc::Receiver<Vec<u8>>,
    incoming_tx: mpsc::Sender<IncomingPacket>,
) {
    tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            trace!(
                device = %device_key,
                len = data.len(),
                "RX datagram -> iroh"
            );
            if incoming_tx
                .send(IncomingPacket {
                    device_key: device_key.clone(),
                    data,
                })
                .await
                .is_err()
            {
                warn!(device = %device_key, "incoming packet channel closed");
                break;
            }
        }
        trace!(device = %device_key, "datagram delivery task ended");
    });
}
#[derive(Debug, Clone)]
pub struct BleAddressLookup {
    state: Arc<BleTransportState>,
}

/// How long `resolve()` waits for BLE discovery before giving up.
const BLE_RESOLVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Poll interval when waiting for BLE discovery.
const BLE_RESOLVE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

impl AddressLookup for BleAddressLookup {
    fn resolve(
        &self,
        endpoint_id: EndpointId,
    ) -> Option<n0_future::stream::Boxed<Result<Item, address_lookup::Error>>> {
        let prefix = key_to_prefix(&endpoint_id);

        if let Some(device_id) = self.state.discovered.read().get(&prefix).cloned() {
            let device_key = device_id.to_string();
            debug!(peer = %endpoint_id.fmt_short(), device = %device_key, "resolve: found via BLE");
            let info = EndpointInfo {
                endpoint_id,
                data: EndpointData::new([TransportAddr::Custom(ble_custom_addr(&device_key))]),
            };
            return Some(Box::pin(n0_future::stream::once(Ok(Item::new(
                info, "iroh-ble", None,
            )))));
        }

        debug!(peer = %endpoint_id.fmt_short(), "resolve: waiting for BLE discovery");
        let state = self.state.clone();
        Some(Box::pin(async_stream::try_stream! {
            let deadline = tokio::time::Instant::now() + BLE_RESOLVE_TIMEOUT;
            loop {
                tokio::time::sleep(BLE_RESOLVE_POLL_INTERVAL).await;
                let device_key = state.discovered.read().get(&prefix).map(|d| d.to_string());
                if let Some(device_key) = device_key {
                    debug!(peer = %endpoint_id.fmt_short(), device = %device_key, "resolve: discovered via BLE after wait");
                    let info = EndpointInfo {
                        endpoint_id,
                        data: EndpointData::new([TransportAddr::Custom(ble_custom_addr(&device_key))]),
                    };
                    yield Item::new(info, "iroh-ble", None);
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    trace!(peer = %endpoint_id.fmt_short(), "resolve: BLE discovery timed out");
                    break;
                }
            }
        }))
    }
}
#[derive(Debug)]
pub struct BlePreset {
    transport: Arc<BleTransport>,
    lookup: BleAddressLookup,
}

impl Preset for BlePreset {
    fn apply(self, builder: Builder) -> Builder {
        let transport_config = iroh::endpoint::QuicTransportConfig::builder()
            .mtu_discovery_config(None)
            .initial_mtu(1200)
            .max_idle_timeout(Some(DISCOVERY_TTL.try_into().unwrap()))
            .keep_alive_interval(std::time::Duration::from_secs(5))
            .default_path_max_idle_timeout(std::time::Duration::from_secs(6))
            .default_path_keep_alive_interval(std::time::Duration::from_secs(4))
            .build();

        builder
            .transport_config(transport_config)
            .add_custom_transport(self.transport)
            .address_lookup(self.lookup)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use blew::l2cap::types::Psm;

    fn make_endpoint_id(seed: u8) -> EndpointId {
        let mut secret_bytes = [0u8; 32];
        secret_bytes[0] = seed;
        let secret = iroh::SecretKey::from_bytes(&secret_bytes);
        secret.public()
    }
    #[test]
    fn test_key_uuid_roundtrip() {
        let id = make_endpoint_id(1);
        let key_bytes = id.as_bytes();

        let uuid = iroh_key_uuid(&id);
        let prefix = parse_iroh_key_uuid(&uuid).expect("should parse");

        assert_eq!(prefix, key_bytes[..KEY_PREFIX_LEN]);
    }

    #[test]
    fn test_key_uuid_prefix_bytes() {
        let id = make_endpoint_id(2);
        let key_bytes = id.as_bytes();
        let uuid = iroh_key_uuid(&id);
        let bytes = uuid.as_bytes();

        assert_eq!(&bytes[..4], &KEY_UUID_PREFIX);
        assert_eq!(&bytes[4..16], &key_bytes[..12]);
    }

    #[test]
    fn test_parse_iroh_key_uuid_rejects_wrong_prefix() {
        let uuid = uuid!("12345678-0000-0000-0000-000000000000");
        assert!(parse_iroh_key_uuid(&uuid).is_none());
    }

    #[test]
    fn test_parse_iroh_key_uuid_rejects_service_uuid() {
        // IROH_SERVICE_UUID uses prefix 69726F01, not 69726F00, so it must
        // not parse as a key-UUID.
        let result = parse_iroh_key_uuid(&IROH_SERVICE_UUID);
        assert!(result.is_none());
    }
    #[test]
    fn test_key_to_prefix() {
        let id = make_endpoint_id(3);
        let key_bytes = id.as_bytes();
        let prefix = key_to_prefix(&id);
        assert_eq!(prefix, key_bytes[..KEY_PREFIX_LEN]);
    }
    #[test]
    fn test_custom_addr_roundtrip() {
        let device_key = "AA:BB:CC:DD:EE:FF";
        let addr = ble_custom_addr(device_key);
        let parsed = parse_custom_addr(&addr).expect("should parse");
        assert_eq!(parsed, device_key);
    }

    #[test]
    fn test_parse_custom_addr_wrong_transport_id() {
        let addr = CustomAddr::from_parts(0x12345, &[0u8; 32]);
        let result = parse_custom_addr(&addr);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_custom_addr_invalid_utf8() {
        let addr = CustomAddr::from_parts(BLE_TRANSPORT_ID, &[0xFF, 0xFE]);
        let result = parse_custom_addr(&addr);
        assert!(result.is_err());
    }
    #[test]
    fn test_characteristic_uuids_are_distinct() {
        let uuids = [
            IROH_SERVICE_UUID,
            IROH_C2P_CHAR_UUID,
            IROH_P2C_CHAR_UUID,
            IROH_PSM_CHAR_UUID,
            IROH_VERSION_CHAR_UUID,
        ];
        for (i, a) in uuids.iter().enumerate() {
            for (j, b) in uuids.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "UUIDs at index {i} and {j} must be distinct");
                }
            }
        }
    }
    #[test]
    fn test_different_keys_different_prefixes() {
        let id_a = make_endpoint_id(10);
        let id_b = make_endpoint_id(20);
        assert_ne!(key_to_prefix(&id_a), key_to_prefix(&id_b));
    }
    #[test]
    fn parse_psm_valid_two_bytes() {
        assert_eq!(parse_psm_value(&[0x01, 0x10]), Some(Psm(0x1001)));
    }

    #[test]
    fn parse_psm_zero_is_none() {
        assert_eq!(parse_psm_value(&[0, 0]), None);
    }

    #[test]
    fn parse_psm_too_short_is_none() {
        assert_eq!(parse_psm_value(&[0x01]), None);
        assert_eq!(parse_psm_value(&[]), None);
    }

    #[test]
    fn parse_psm_extra_bytes_uses_first_two() {
        assert_eq!(parse_psm_value(&[0x01, 0x10, 0xFF]), Some(Psm(0x1001)));
    }
}
