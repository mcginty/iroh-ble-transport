use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use iroh::endpoint::{presets, QuicTransportConfig};
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointId};
use iroh_ble_chat_protocol::{load_known_peers, save_known_peers, ChatMsg, IMAGE_ALPN};
use iroh_ble_transport::transport::BleTransport;
use iroh_ble_transport::{
    BleDedupHook, BlePeerInfo, BlePeerPhase, BleTransportConfig, Central, ConnectPath,
    InMemoryPeerStore, L2capPolicy, Peripheral,
};
use iroh_gossip::proto::{HyparviewConfig, TopicId};
use iroh_gossip::Gossip;
use n0_future::StreamExt;
use serde::Serialize;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::Mutex;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, Layer};

mod image;
#[path = "../../../support/key_utils.rs"]
mod key_utils;

fn chat_topic_id() -> TopicId {
    let hash = blake3::hash(b"iroh-ble-chat-v1");
    TopicId::from_bytes(*hash.as_bytes())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GossipStatus {
    Direct,
    InTopic,
    Stale,
}

#[derive(Clone, Debug)]
struct PeerState {
    id: EndpointId,
    nickname: Option<String>,
    gossip: Option<GossipStatus>,
    ble_phase: Option<BlePeerPhase>,
    ble_path: Option<String>,
    ble_failures: u32,
    last_seen: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct PeerStateUI {
    id: String,
    nickname: Option<String>,
    status: String,
    ble_phase: Option<String>,
    ble_path: Option<String>,
    ble_failures: u32,
    last_seen_secs_ago: u64,
}

fn ble_phase_str(phase: BlePeerPhase) -> &'static str {
    match phase {
        BlePeerPhase::Unknown => "unknown",
        BlePeerPhase::Discovered => "discovered",
        BlePeerPhase::PendingDial => "pending_dial",
        BlePeerPhase::Connecting => "connecting",
        BlePeerPhase::Handshaking => "handshaking",
        BlePeerPhase::Connected => "connected",
        BlePeerPhase::Draining => "draining",
        BlePeerPhase::Reconnecting => "reconnecting",
        BlePeerPhase::Dead => "dead",
        BlePeerPhase::Restoring => "restoring",
    }
}

impl PeerState {
    fn ui_status(&self) -> &'static str {
        match self.ble_phase {
            Some(BlePeerPhase::Connected) => "connected",
            Some(BlePeerPhase::Handshaking) => "handshaking",
            Some(BlePeerPhase::Connecting) => "connecting",
            Some(BlePeerPhase::Reconnecting | BlePeerPhase::Restoring) => "reconnecting",
            Some(BlePeerPhase::PendingDial) => "pending_dial",
            Some(BlePeerPhase::Discovered | BlePeerPhase::Unknown) => {
                if matches!(self.gossip, Some(GossipStatus::Direct)) {
                    "connected"
                } else if matches!(self.gossip, Some(GossipStatus::InTopic)) {
                    "in_topic"
                } else {
                    "nearby"
                }
            }
            Some(BlePeerPhase::Draining) => "draining",
            Some(BlePeerPhase::Dead) => "dead",
            None => match self.gossip {
                Some(GossipStatus::Direct) => "connected",
                Some(GossipStatus::InTopic) => "in_topic",
                Some(GossipStatus::Stale) => "stale",
                _ => "unknown",
            },
        }
    }

    fn to_ui(&self) -> PeerStateUI {
        PeerStateUI {
            id: self.id.to_string(),
            nickname: self.nickname.clone(),
            status: self.ui_status().to_string(),
            ble_phase: self.ble_phase.map(|p| ble_phase_str(p).to_string()),
            ble_path: self.ble_path.clone(),
            ble_failures: self.ble_failures,
            last_seen_secs_ago: self.last_seen.elapsed().as_secs(),
        }
    }
}
struct AppState {
    secret_key: iroh_base::SecretKey,
    nickname: String,
    cache_dir: PathBuf,
    endpoint: Option<Endpoint>,
    /// Dropping this kills gossip, so we hold it for the app lifetime.
    _router: Option<Router>,
    gossip_sender: Option<iroh_gossip::api::GossipSender>,
    ble_transport: Option<Arc<BleTransport>>,
    peers: HashMap<EndpointId, PeerState>,
    debug_enabled: Arc<AtomicBool>,
    pending_images: crate::image::PendingImages,
}

impl AppState {
    fn own_id(&self) -> EndpointId {
        self.secret_key.public()
    }
}
fn load_nickname(cache_dir: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(cache_dir.join("nickname.txt")).ok()
}

fn save_nickname(cache_dir: &std::path::Path, nickname: &str) {
    let _ = std::fs::create_dir_all(cache_dir);
    let _ = std::fs::write(cache_dir.join("nickname.txt"), nickname);
}

fn default_nickname(id: &EndpointId) -> String {
    let short = id.fmt_short().to_string();
    format!("user_{}", &short[..short.len().min(6)])
}

fn gossip_status_str(status: Option<GossipStatus>) -> &'static str {
    match status {
        Some(GossipStatus::Direct) => "direct",
        Some(GossipStatus::InTopic) => "in_topic",
        Some(GossipStatus::Stale) => "stale",
        None => "none",
    }
}

fn summarize_join_targets(peers: &HashMap<EndpointId, PeerState>, targets: &[EndpointId]) -> String {
    targets
        .iter()
        .map(|peer_id| {
            if let Some(peer) = peers.get(peer_id) {
                format!(
                    "{}(gossip={},ble_phase={},ble_path={})",
                    peer_id.fmt_short(),
                    gossip_status_str(peer.gossip),
                    peer.ble_phase.map(ble_phase_str).unwrap_or("none"),
                    peer.ble_path.as_deref().unwrap_or("none"),
                )
            } else {
                format!("{}(unknown)", peer_id.fmt_short())
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[tauri::command]
async fn get_node_id(state: State<'_, Arc<Mutex<AppState>>>) -> Result<String, String> {
    let st = state.lock().await;
    Ok(st.own_id().to_string())
}

#[tauri::command]
async fn start_node(
    app: AppHandle,
    state: State<'_, Arc<Mutex<AppState>>>,
) -> Result<serde_json::Value, String> {
    let mut st = state.lock().await;
    if st.endpoint.is_some() {
        return Ok(serde_json::json!({
            "node_id": st.own_id().to_string(),
            "nickname": st.nickname.clone(),
        }));
    }

    info!("Initializing BLE transport...");

    if tauri_plugin_blew::is_emulator() {
        return Err(
            "BLE is not supported on emulators or simulators. Please run on a physical device."
                .into(),
        );
    }

    // On Android, start_node can race with the permissions dialog.
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !tauri_plugin_blew::are_ble_permissions_granted() {
            if std::time::Instant::now() > deadline {
                return Err("BLE permissions not granted. Please allow Bluetooth permissions and restart the app.".into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        info!("BLE permissions granted");
    }

    let (verified_tx, verified_rx) = tokio::sync::mpsc::unbounded_channel();
    let ble_transport: Arc<BleTransport> = {
        let central = Arc::new(Central::new().await.map_err(|e| e.to_string())?);
        let peripheral = Arc::new(Peripheral::new().await.map_err(|e| e.to_string())?);
        let local_id = st.secret_key.public();
        let transport = BleTransport::with_config(
            local_id,
            central,
            peripheral,
            BleTransportConfig {
                l2cap_policy: L2capPolicy::PreferL2cap,
                store: Arc::new(InMemoryPeerStore::new()),
                verified_rx: Some(verified_rx),
            },
        )
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("adapter not found") || msg.contains("AdapterNotFound") {
                "Bluetooth is not available on this device. A physical Bluetooth adapter is required — simulators and emulators are not supported.".to_string()
            } else if msg.contains("not powered")
                || msg.contains("timed out waiting for Bluetooth")
                || msg.contains("power on")
            {
                "Bluetooth is turned off. Please enable Bluetooth in Settings and restart the app."
                    .to_string()
            } else {
                msg
            }
        })?;
        Arc::new(transport)
    };
    let lookup = ble_transport.address_lookup();
    let transport: Arc<dyn iroh::endpoint::transports::CustomTransport> = ble_transport.clone();

    // BLE-tuned QUIC idle timeout. The default (30s) leaves a large window
    // between BLE-level disconnect detection (~6s via ReliableChannel's
    // LINK_DEAD_DEADLINE) and iroh finally marking the connection dead, which
    // manifests as the UI showing a peer as connected long after the BLE link
    // is gone. 15s is short enough to cut that gap noticeably while still
    // tolerating the multi-second pauses real BLE links exhibit.
    let transport_cfg = QuicTransportConfig::builder()
        .max_idle_timeout(Some(
            std::time::Duration::from_secs(15)
                .try_into()
                .expect("15s fits in IdleTimeout"),
        ))
        .build();

    let ep = Endpoint::builder(presets::N0DisableRelay)
        .hooks(BleDedupHook::new(verified_tx))
        .add_custom_transport(Arc::clone(&transport))
        .address_lookup(lookup)
        .transport_config(transport_cfg)
        .secret_key(st.secret_key.clone())
        .clear_ip_transports()
        .bind()
        .await
        .map_err(|e| e.to_string())?;

    let hyparview = HyparviewConfig {
        active_view_capacity: 3,
        passive_view_capacity: 12,
        shuffle_interval: std::time::Duration::from_secs(120),
        ..Default::default()
    };

    let gossip = Gossip::builder()
        .membership_config(hyparview)
        .spawn(ep.clone());

    let image_receiver = crate::image::ImageReceiver {
        app: app.clone(),
        pending: st.pending_images.clone(),
    };
    let router = Router::builder(ep.clone())
        .accept(iroh_gossip::ALPN, gossip.clone())
        .accept(IMAGE_ALPN, image_receiver)
        .spawn();

    let known_peers = load_known_peers(&st.cache_dir);

    let own_id_pre = st.own_id();
    for peer_id in &known_peers {
        if *peer_id == own_id_pre {
            continue;
        }
        let peer = st
            .peers
            .entry(*peer_id)
            .or_insert_with(|| new_peer_entry(*peer_id));
        let _ = app.emit("peer-updated", &peer.to_ui());
    }

    let topic = gossip
        .subscribe(chat_topic_id(), known_peers.clone())
        .await
        .map_err(|e| format!("gossip subscribe: {e}"))?;

    let (sender, receiver) = topic.split();

    st.endpoint = Some(ep);
    st._router = Some(router);
    st.gossip_sender = Some(sender.clone());
    st.ble_transport = Some(ble_transport.clone());

    let own_id = st.own_id();
    let nickname = st.nickname.clone();
    let result = serde_json::json!({
        "node_id": own_id.to_string(),
        "nickname": nickname,
    });

    let hello = ChatMsg::Hello {
        from: own_id,
        nickname: nickname.clone(),
    };
    let payload = postcard::to_allocvec(&hello).unwrap_or_else(|e| {
        tracing::warn!("failed to serialize Hello: {e}");
        vec![]
    });
    let _ = sender.broadcast(Bytes::from(payload)).await;

    let state_arc = state.inner().clone();
    tauri::async_runtime::spawn(gossip_event_pump(
        app.clone(),
        state_arc.clone(),
        sender,
        receiver,
        own_id,
    ));

    tauri::async_runtime::spawn(stale_tick(app.clone(), state_arc.clone()));
    tauri::async_runtime::spawn(bandwidth_tick(app.clone(), ble_transport.clone()));
    tauri::async_runtime::spawn(transport_state_tick(
        app.clone(),
        ble_transport,
        state_arc.clone(),
    ));
    tauri::async_runtime::spawn(reconnect_tick(state_arc));

    Ok(result)
}

async fn gossip_event_pump(
    app: AppHandle,
    state: Arc<Mutex<AppState>>,
    sender: iroh_gossip::api::GossipSender,
    mut receiver: iroh_gossip::api::GossipReceiver,
    own_id: EndpointId,
) {
    let mut topic_joined = false;
    while let Some(Ok(event)) = receiver.next().await {
        match event {
            iroh_gossip::api::Event::NeighborUp(peer_id) => {
                if peer_id == own_id {
                    continue;
                }
                info!(peer = %peer_id.fmt_short(), "NeighborUp");
                let mut st = state.lock().await;
                let peer = st
                    .peers
                    .entry(peer_id)
                    .or_insert_with(|| new_peer_entry(peer_id));
                peer.gossip = Some(GossipStatus::Direct);
                peer.last_seen = Instant::now();
                let ui = peer.to_ui();
                let _ = app.emit("peer-updated", &ui);
                remember_known_peer(&st.cache_dir, peer_id);

                if !topic_joined {
                    topic_joined = true;
                    let active = st
                        .peers
                        .values()
                        .filter(|p| matches!(p.gossip, Some(GossipStatus::Direct)))
                        .count();
                    let _ = app.emit("topic-joined", serde_json::json!({"active_peers": active}));
                }
            }
            iroh_gossip::api::Event::NeighborDown(peer_id) => {
                if peer_id == own_id {
                    continue;
                }
                info!(peer = %peer_id.fmt_short(), "NeighborDown");
                let mut st = state.lock().await;
                if let Some(peer) = st.peers.get_mut(&peer_id) {
                    peer.gossip = Some(GossipStatus::InTopic);
                    let ui = peer.to_ui();
                    let _ = app.emit("peer-updated", &ui);
                }
            }
            iroh_gossip::api::Event::Received(msg) => {
                let Ok(chat_msg) = postcard::from_bytes::<ChatMsg>(&msg.content) else {
                    tracing::warn!(len = msg.content.len(), "failed to decode chat message");
                    continue;
                };

                match &chat_msg {
                    ChatMsg::Hello { from, nickname } => {
                        if *from == own_id {
                            continue;
                        }
                        let mut st = state.lock().await;
                        let cache_dir = st.cache_dir.clone();
                        let (
                            needs_join_nudge,
                            gossip_status,
                            ble_phase,
                            ble_path,
                            ble_failures,
                            ui,
                        ) = {
                            let peer = st
                                .peers
                                .entry(*from)
                                .or_insert_with(|| new_peer_entry(*from));
                            peer.nickname = Some(nickname.clone());
                            let needs_join_nudge = should_nudge_join(peer.gossip);
                            if peer.gossip.is_none() {
                                peer.gossip = Some(GossipStatus::InTopic);
                            }
                            peer.last_seen = Instant::now();
                            (
                                needs_join_nudge,
                                gossip_status_str(peer.gossip),
                                peer.ble_phase.map(ble_phase_str).unwrap_or("none"),
                                peer.ble_path.clone(),
                                peer.ble_failures,
                                peer.to_ui(),
                            )
                        };
                        info!(
                            peer = %from.fmt_short(),
                            nickname = %nickname,
                            gossip = gossip_status,
                            ble_phase = ble_phase,
                            ble_path = ble_path.as_deref().unwrap_or("none"),
                            ble_failures,
                            needs_join_nudge,
                            "received Hello"
                        );
                        remember_known_peer(&cache_dir, *from);
                        let _ = app.emit("peer-updated", &ui);
                        drop(st);

                        if needs_join_nudge {
                            tracing::debug!(peer = %from.fmt_short(), "nudging join after inbound Hello");
                            if let Err(e) = sender.join_peers(vec![*from]).await {
                                tracing::debug!(peer = %from.fmt_short(), "join_peers after Hello failed: {e}");
                            } else {
                                tracing::debug!(peer = %from.fmt_short(), "join_peers after Hello ok");
                            }
                        }
                    }
                    ChatMsg::Text {
                        from,
                        nickname,
                        text,
                    } => {
                        if *from == own_id {
                            continue;
                        }
                        let mut st = state.lock().await;
                        let cache_dir = st.cache_dir.clone();
                        let (
                            needs_join_nudge,
                            gossip_status,
                            ble_phase,
                            ble_path,
                            ble_failures,
                            ui,
                        ) = {
                            let peer = st
                                .peers
                                .entry(*from)
                                .or_insert_with(|| new_peer_entry(*from));
                            peer.nickname = Some(nickname.clone());
                            let needs_join_nudge = should_nudge_join(peer.gossip);
                            if peer.gossip.is_none() {
                                peer.gossip = Some(GossipStatus::InTopic);
                            }
                            peer.last_seen = Instant::now();
                            (
                                needs_join_nudge,
                                gossip_status_str(peer.gossip),
                                peer.ble_phase.map(ble_phase_str).unwrap_or("none"),
                                peer.ble_path.clone(),
                                peer.ble_failures,
                                peer.to_ui(),
                            )
                        };
                        info!(
                            peer = %from.fmt_short(),
                            nickname = %nickname,
                            text_len = text.len(),
                            gossip = gossip_status,
                            ble_phase = ble_phase,
                            ble_path = ble_path.as_deref().unwrap_or("none"),
                            ble_failures,
                            needs_join_nudge,
                            "received Text"
                        );
                        remember_known_peer(&cache_dir, *from);
                        let _ = app.emit("peer-updated", &ui);

                        let _ = app.emit(
                            "chat-msg",
                            serde_json::json!({
                                "from_id": from.to_string(),
                                "nickname": nickname,
                                "text": text,
                                "is_self": false,
                            }),
                        );
                        drop(st);

                        if needs_join_nudge {
                            tracing::debug!(peer = %from.fmt_short(), "nudging join after inbound Text");
                            if let Err(e) = sender.join_peers(vec![*from]).await {
                                tracing::debug!(peer = %from.fmt_short(), "join_peers after Text failed: {e}");
                            } else {
                                tracing::debug!(peer = %from.fmt_short(), "join_peers after Text ok");
                            }
                        }
                    }
                    ChatMsg::NicknameChanged { from, new_nickname } => {
                        if *from == own_id {
                            continue;
                        }
                        let mut st = state.lock().await;
                        let cache_dir = st.cache_dir.clone();
                        let (
                            needs_join_nudge,
                            gossip_status,
                            ble_phase,
                            ble_path,
                            ble_failures,
                            ui,
                        ) = {
                            let peer = st
                                .peers
                                .entry(*from)
                                .or_insert_with(|| new_peer_entry(*from));
                            peer.nickname = Some(new_nickname.clone());
                            let needs_join_nudge = should_nudge_join(peer.gossip);
                            if peer.gossip.is_none() {
                                peer.gossip = Some(GossipStatus::InTopic);
                            }
                            peer.last_seen = Instant::now();
                            (
                                needs_join_nudge,
                                gossip_status_str(peer.gossip),
                                peer.ble_phase.map(ble_phase_str).unwrap_or("none"),
                                peer.ble_path.clone(),
                                peer.ble_failures,
                                peer.to_ui(),
                            )
                        };
                        info!(
                            peer = %from.fmt_short(),
                            new_nickname = %new_nickname,
                            gossip = gossip_status,
                            ble_phase = ble_phase,
                            ble_path = ble_path.as_deref().unwrap_or("none"),
                            ble_failures,
                            needs_join_nudge,
                            "received NicknameChanged"
                        );
                        remember_known_peer(&cache_dir, *from);
                        let _ = app.emit("peer-updated", &ui);
                        drop(st);

                        if needs_join_nudge {
                            tracing::debug!(
                                peer = %from.fmt_short(),
                                "nudging join after inbound NicknameChanged"
                            );
                            if let Err(e) = sender.join_peers(vec![*from]).await {
                                tracing::debug!(
                                    peer = %from.fmt_short(),
                                    "join_peers after NicknameChanged failed: {e}"
                                );
                            } else {
                                tracing::debug!(
                                    peer = %from.fmt_short(),
                                    "join_peers after NicknameChanged ok"
                                );
                            }
                        }
                    }
                    ChatMsg::ImageStart {
                        from,
                        nickname,
                        image_id,
                        size,
                    } => {
                        if *from == own_id {
                            continue;
                        }
                        let (needs_join_nudge, gossip_status, ble_phase, ble_path, ble_failures) = {
                            let mut st = state.lock().await;
                            let cache_dir = st.cache_dir.clone();
                            let (needs_join_nudge, gossip_status, ble_phase, ble_path, ble_failures, ui) = {
                                let peer = st
                                    .peers
                                    .entry(*from)
                                    .or_insert_with(|| new_peer_entry(*from));
                                peer.nickname = Some(nickname.clone());
                                let needs_join_nudge = should_nudge_join(peer.gossip);
                                if peer.gossip.is_none() {
                                    peer.gossip = Some(GossipStatus::InTopic);
                                }
                                peer.last_seen = Instant::now();
                                (
                                    needs_join_nudge,
                                    gossip_status_str(peer.gossip),
                                    peer.ble_phase.map(ble_phase_str).unwrap_or("none"),
                                    peer.ble_path.clone(),
                                    peer.ble_failures,
                                    peer.to_ui(),
                                )
                            };
                            remember_known_peer(&cache_dir, *from);
                            let _ = app.emit("peer-updated", &ui);
                            (
                                needs_join_nudge,
                                gossip_status,
                                ble_phase,
                                ble_path,
                                ble_failures,
                            )
                        };
                        info!(
                            peer = %from.fmt_short(),
                            nickname = %nickname,
                            image_id = *image_id,
                            image_size = *size,
                            gossip = gossip_status,
                            ble_phase = ble_phase,
                            ble_path = ble_path.as_deref().unwrap_or("none"),
                            ble_failures,
                            needs_join_nudge,
                            "received ImageStart"
                        );
                        let pending = {
                            let st = state.lock().await;
                            st.pending_images.clone()
                        };
                        pending.lock().await.insert(
                            *image_id,
                            crate::image::PendingImage {
                                from_id: from.to_string(),
                                nickname: nickname.clone(),
                                size: *size,
                            },
                        );

                        let _ = app.emit(
                            "image-start",
                            serde_json::json!({
                                "from_id": from.to_string(),
                                "nickname": nickname,
                                "image_id": image_id.to_string(),
                                "size": size,
                                "is_self": false,
                            }),
                        );

                        if needs_join_nudge {
                            tracing::debug!(peer = %from.fmt_short(), "nudging join after inbound ImageStart");
                            if let Err(e) = sender.join_peers(vec![*from]).await {
                                tracing::debug!(peer = %from.fmt_short(), "join_peers after ImageStart failed: {e}");
                            } else {
                                tracing::debug!(peer = %from.fmt_short(), "join_peers after ImageStart ok");
                            }
                        }
                    }
                }
            }
            iroh_gossip::api::Event::Lagged => {
                tracing::warn!("gossip event stream lagged");
            }
        }
    }
}

async fn bandwidth_tick(app: AppHandle, transport: Arc<BleTransport>) {
    let interval = std::time::Duration::from_secs(1);
    let mut prev = transport.metrics();
    let mut prev_instant = Instant::now();
    loop {
        tokio::time::sleep(interval).await;
        let now = transport.metrics();
        let elapsed = prev_instant.elapsed().as_secs_f64().max(0.001);
        let tx_delta = now.tx_bytes.saturating_sub(prev.tx_bytes);
        let rx_delta = now.rx_bytes.saturating_sub(prev.rx_bytes);
        let retransmit_delta = now.retransmits.saturating_sub(prev.retransmits);
        let truncation_delta = now.truncations.saturating_sub(prev.truncations);
        let tx_kbps = (tx_delta as f64 * 8.0 / 1000.0) / elapsed;
        let rx_kbps = (rx_delta as f64 * 8.0 / 1000.0) / elapsed;
        let _ = app.emit(
            "bandwidth",
            serde_json::json!({
                "tx_kbps": tx_kbps,
                "rx_kbps": rx_kbps,
                "retransmits": retransmit_delta,
                "truncations": truncation_delta,
            }),
        );
        prev = now;
        prev_instant = Instant::now();
    }
}

/// Periodically nudge gossip to (re)connect to known peers we don't currently
/// have a direct link to. The transport itself is intentionally passive — it
/// will not auto-retry dead peers — so reconnect policy lives here.
async fn reconnect_tick(state: Arc<Mutex<AppState>>) {
    let interval = std::time::Duration::from_secs(10);
    loop {
        tokio::time::sleep(interval).await;
        let st = state.lock().await;
        let Some(sender) = st.gossip_sender.clone() else {
            continue;
        };
        let targets = collect_join_targets(st.own_id(), &st.peers, load_known_peers(&st.cache_dir));
        drop(st);

        if targets.is_empty() {
            continue;
        }
        let count = targets.len();
        if let Err(e) = sender.join_peers(targets).await {
            tracing::debug!("reconnect_tick join_peers failed: {e}");
        } else {
            tracing::trace!("reconnect_tick nudged {count} peer(s)");
        }
    }
}

async fn stale_tick(app: AppHandle, state: Arc<Mutex<AppState>>) {
    let stale_threshold = std::time::Duration::from_secs(120);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        let mut st = state.lock().await;
        for peer in st.peers.values_mut() {
            if peer.last_seen.elapsed() > stale_threshold
                && matches!(peer.gossip, Some(GossipStatus::InTopic))
            {
                peer.gossip = Some(GossipStatus::Stale);
                let ui = peer.to_ui();
                let _ = app.emit("peer-updated", &ui);
            }
        }
    }
}

fn new_peer_entry(id: EndpointId) -> PeerState {
    PeerState {
        id,
        nickname: None,
        gossip: None,
        ble_phase: None,
        ble_path: None,
        ble_failures: 0,
        last_seen: Instant::now(),
    }
}

fn remember_known_peer(cache_dir: &std::path::Path, peer_id: EndpointId) {
    let mut known = load_known_peers(cache_dir);
    if !known.contains(&peer_id) {
        known.push(peer_id);
        save_known_peers(cache_dir, &known);
    }
}

fn should_nudge_join(gossip: Option<GossipStatus>) -> bool {
    !matches!(gossip, Some(GossipStatus::Direct))
}

fn collect_join_targets(
    own_id: EndpointId,
    peers: &HashMap<EndpointId, PeerState>,
    known_peers: Vec<EndpointId>,
) -> Vec<EndpointId> {
    let mut seen = HashSet::new();
    let mut targets = Vec::new();

    for peer_id in known_peers
        .into_iter()
        .chain(peers.keys().copied())
        .filter(|peer_id| *peer_id != own_id)
    {
        if !seen.insert(peer_id) {
            continue;
        }
        if should_nudge_join(peers.get(&peer_id).and_then(|peer| peer.gossip)) {
            targets.push(peer_id);
        }
    }

    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_peer() -> PeerState {
        let endpoint = iroh::SecretKey::from_bytes(&[7u8; 32]).public();
        new_peer_entry(endpoint)
    }

    #[test]
    fn ui_status_prefers_non_connected_ble_phase_over_stale_direct_gossip() {
        let mut peer = test_peer();
        peer.gossip = Some(GossipStatus::Direct);
        peer.ble_phase = Some(BlePeerPhase::Reconnecting);
        assert_eq!(peer.ui_status(), "reconnecting");

        peer.ble_phase = Some(BlePeerPhase::Draining);
        assert_eq!(peer.ui_status(), "draining");

        peer.ble_phase = Some(BlePeerPhase::Dead);
        assert_eq!(peer.ui_status(), "dead");
    }

    #[test]
    fn ui_status_allows_direct_gossip_to_fill_in_unknown_ble_phase() {
        let mut peer = test_peer();
        peer.gossip = Some(GossipStatus::Direct);
        peer.ble_phase = Some(BlePeerPhase::Unknown);
        assert_eq!(peer.ui_status(), "connected");

        peer.ble_phase = None;
        assert_eq!(peer.ui_status(), "connected");
    }

    #[test]
    fn collect_join_targets_includes_non_direct_peers_once() {
        let own_id = iroh::SecretKey::from_bytes(&[1u8; 32]).public();
        let direct_id = iroh::SecretKey::from_bytes(&[2u8; 32]).public();
        let in_topic_id = iroh::SecretKey::from_bytes(&[3u8; 32]).public();
        let unknown_id = iroh::SecretKey::from_bytes(&[4u8; 32]).public();

        let mut peers = HashMap::new();
        let mut direct = new_peer_entry(direct_id);
        direct.gossip = Some(GossipStatus::Direct);
        peers.insert(direct_id, direct);

        let mut in_topic = new_peer_entry(in_topic_id);
        in_topic.gossip = Some(GossipStatus::InTopic);
        peers.insert(in_topic_id, in_topic);

        peers.insert(unknown_id, new_peer_entry(unknown_id));

        let targets = collect_join_targets(
            own_id,
            &peers,
            vec![own_id, direct_id, in_topic_id, unknown_id, in_topic_id],
        );

        assert_eq!(targets, vec![in_topic_id, unknown_id]);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct BlePeerDebugUI {
    device_id: String,
    phase: String,
    consecutive_failures: u32,
    path: Option<String>,
}

impl BlePeerDebugUI {
    fn from_info(info: &BlePeerInfo) -> Self {
        Self {
            device_id: info.device_id.to_string(),
            phase: ble_phase_str(info.phase).to_string(),
            consecutive_failures: info.consecutive_failures,
            path: info.connect_path.map(|p| {
                match p {
                    ConnectPath::Gatt => "gatt",
                    ConnectPath::L2cap => "l2cap",
                }
                .to_string()
            }),
        }
    }
}

async fn transport_state_tick(
    app: AppHandle,
    transport: Arc<BleTransport>,
    state: Arc<Mutex<AppState>>,
) {
    let mut last_emitted: HashMap<String, BlePeerDebugUI> = HashMap::new();
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let snapshot = transport.snapshot_peers();

        let info_by_device: HashMap<String, &BlePeerInfo> = snapshot
            .iter()
            .map(|info| (info.device_id.to_string(), info))
            .collect();

        {
            let mut st = state.lock().await;
            for peer in st.peers.values_mut() {
                let info = transport
                    .device_for_endpoint(&peer.id)
                    .and_then(|did| info_by_device.get(&did.to_string()).copied());

                let new_phase = info.map(|i| i.phase);
                let new_failures = info.map_or(0, |i| i.consecutive_failures);
                let new_path = info.and_then(|i| i.connect_path).map(|p| match p {
                    ConnectPath::Gatt => "gatt".to_string(),
                    ConnectPath::L2cap => "l2cap".to_string(),
                });

                if peer.ble_phase != new_phase
                    || peer.ble_failures != new_failures
                    || peer.ble_path != new_path
                {
                    peer.ble_phase = new_phase;
                    peer.ble_failures = new_failures;
                    peer.ble_path = new_path;
                    let _ = app.emit("peer-updated", &peer.to_ui());
                }
            }
        }

        let mut current: HashMap<String, BlePeerDebugUI> = HashMap::with_capacity(snapshot.len());
        for info in &snapshot {
            current.insert(info.device_id.to_string(), BlePeerDebugUI::from_info(info));
        }

        for (device_id, ui) in &current {
            if last_emitted.get(device_id) == Some(ui) {
                continue;
            }
            let _ = app.emit("ble-peer-updated", ui);
        }

        for device_id in last_emitted.keys() {
            if !current.contains_key(device_id) {
                let _ = app.emit(
                    "ble-peer-removed",
                    serde_json::json!({ "device_id": device_id }),
                );
            }
        }

        last_emitted = current;
    }
}
struct DebugLogLayer {
    debug_enabled: Arc<AtomicBool>,
    app: AppHandle,
}

impl<S: tracing::Subscriber> Layer<S> for DebugLogLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if !self.debug_enabled.load(Ordering::Relaxed) {
            return;
        }

        let level = *event.metadata().level();
        let target = event.metadata().target();

        if !(target.starts_with("iroh_ble")
            || target.starts_with("iroh_gossip")
            || target.starts_with("blew"))
        {
            return;
        }
        if level > tracing::Level::DEBUG {
            return;
        }

        let mut msg = String::new();
        let mut fields = Vec::new();
        event.record(
            &mut |field: &tracing::field::Field, val: &dyn std::fmt::Debug| {
                if field.name() == "message" {
                    msg = format!("{val:?}");
                } else {
                    fields.push(format!("{}={:?}", field.name(), val));
                }
            },
        );
        if !fields.is_empty() {
            msg = format!("{msg} {}", fields.join(" "));
        }

        let _ = self.app.emit(
            "debug-log",
            serde_json::json!({
                "target": target,
                "level": format!("{level}"),
                "message": msg,
            }),
        );
    }
}

#[tauri::command]
async fn set_nickname(
    nickname: String,
    state: State<'_, Arc<Mutex<AppState>>>,
) -> Result<(), String> {
    let mut st = state.lock().await;
    st.nickname = nickname.clone();
    save_nickname(&st.cache_dir, &nickname);

    if let Some(sender) = &st.gossip_sender {
        let msg = ChatMsg::NicknameChanged {
            from: st.own_id(),
            new_nickname: nickname,
        };
        let payload = postcard::to_allocvec(&msg).unwrap_or_else(|e| {
            tracing::warn!("failed to serialize NicknameChanged: {e}");
            vec![]
        });
        let _ = sender.broadcast(Bytes::from(payload)).await;
    }
    Ok(())
}

#[tauri::command]
async fn add_peer(
    id_str: String,
    app: AppHandle,
    state: State<'_, Arc<Mutex<AppState>>>,
) -> Result<(), String> {
    let peer_id: EndpointId = id_str.parse().map_err(|e| format!("{e}"))?;

    let mut st = state.lock().await;
    let sender = st.gossip_sender.as_ref().ok_or("Node not started")?.clone();

    st.peers.entry(peer_id).or_insert_with(|| {
        let mut peer = new_peer_entry(peer_id);
        peer.ble_phase = Some(BlePeerPhase::Connecting);
        let _ = app.emit("peer-updated", &peer.to_ui());
        peer
    });

    let mut known = load_known_peers(&st.cache_dir);
    if !known.contains(&peer_id) {
        known.push(peer_id);
        save_known_peers(&st.cache_dir, &known);
    }
    drop(st);

    sender
        .join_peers(vec![peer_id])
        .await
        .map_err(|e| format!("join_peers: {e}"))?;

    Ok(())
}

#[tauri::command]
async fn remove_peer(
    id_str: String,
    app: AppHandle,
    state: State<'_, Arc<Mutex<AppState>>>,
) -> Result<(), String> {
    let peer_id: EndpointId = id_str.parse().map_err(|e| format!("{e}"))?;

    let mut st = state.lock().await;
    if peer_id == st.own_id() {
        return Err("cannot remove self".into());
    }
    st.peers.remove(&peer_id);
    let mut known = load_known_peers(&st.cache_dir);
    let before = known.len();
    known.retain(|p| p != &peer_id);
    if known.len() != before {
        save_known_peers(&st.cache_dir, &known);
    }
    drop(st);

    let _ = app.emit(
        "peer-removed",
        serde_json::json!({ "id": peer_id.to_string() }),
    );
    Ok(())
}

#[tauri::command]
async fn send_message(
    text: String,
    app: AppHandle,
    state: State<'_, Arc<Mutex<AppState>>>,
) -> Result<(), String> {
    let st = state.lock().await;
    let sender = st.gossip_sender.as_ref().ok_or("Node not started")?.clone();
    let own_id = st.own_id();
    let nickname = st.nickname.clone();
    let known_peers = load_known_peers(&st.cache_dir);
    let join_targets = collect_join_targets(own_id, &st.peers, known_peers.clone());
    let join_target_summary = summarize_join_targets(&st.peers, &join_targets);
    let direct_peers = st
        .peers
        .values()
        .filter(|peer| matches!(peer.gossip, Some(GossipStatus::Direct)))
        .count();
    let in_topic_peers = st
        .peers
        .values()
        .filter(|peer| matches!(peer.gossip, Some(GossipStatus::InTopic)))
        .count();
    let stale_peers = st
        .peers
        .values()
        .filter(|peer| matches!(peer.gossip, Some(GossipStatus::Stale)))
        .count();
    let total_peers = st.peers.len();
    drop(st);

    info!(
        text_len = text.len(),
        total_peers,
        known_peers = known_peers.len(),
        direct_peers,
        in_topic_peers,
        stale_peers,
        join_targets = join_targets.len(),
        "send_message requested"
    );
    if !join_target_summary.is_empty() {
        tracing::debug!(targets = %join_target_summary, "send_message join target summary");
    }

    if !join_targets.is_empty() {
        tracing::debug!(
            count = join_targets.len(),
            "nudging non-direct peers before broadcast"
        );
        if let Err(e) = sender.join_peers(join_targets).await {
            tracing::debug!("send_message join_peers failed: {e}");
        } else {
            tracing::debug!("send_message join_peers ok");
        }
    }

    let msg = ChatMsg::Text {
        from: own_id,
        nickname: nickname.clone(),
        text: text.clone(),
    };
    let payload = postcard::to_allocvec(&msg).unwrap_or_else(|e| {
        tracing::warn!("failed to serialize Text: {e}");
        vec![]
    });
    sender
        .broadcast(Bytes::from(payload))
        .await
        .map_err(|e| format!("broadcast: {e}"))?;
    info!(text_len = text.len(), "send_message broadcast ok");

    // Gossip doesn't deliver our own broadcasts back to us.
    let _ = app.emit(
        "chat-msg",
        serde_json::json!({
            "from_id": own_id.to_string(),
            "nickname": nickname,
            "text": text,
            "is_self": true,
        }),
    );

    Ok(())
}

#[tauri::command]
async fn send_image(app: AppHandle, state: State<'_, Arc<Mutex<AppState>>>) -> Result<(), String> {
    let file_path = app
        .dialog()
        .file()
        .add_filter("Images", &["png", "jpg", "jpeg", "heic", "webp"])
        .blocking_pick_file();

    let file_path = match file_path {
        Some(f) => f,
        None => return Ok(()), // user cancelled
    };

    let image_id: u64 = rand::random();

    let _ = app.emit(
        "image-start",
        serde_json::json!({
            "image_id": image_id.to_string(),
            "is_self": true,
            "from_id": "",
            "nickname": "",
            "size": 0,
        }),
    );

    // Tauri fs plugin handles content:// URIs on Android.
    use tauri_plugin_fs::FsExt;
    let image_bytes = app
        .fs()
        .read(file_path)
        .map_err(|e| format!("Failed to read image: {e}"))?;

    let avif_bytes =
        tokio::task::spawn_blocking(move || crate::image::encode_image_from_bytes(&image_bytes))
            .await
            .map_err(|e| format!("spawn_blocking: {e}"))??;
    let avif_size = avif_bytes.len();

    let st = state.lock().await;
    let sender = st.gossip_sender.as_ref().ok_or("Node not started")?.clone();
    let ep = st.endpoint.as_ref().ok_or("Node not started")?.clone();
    let own_id = st.own_id();
    let nickname = st.nickname.clone();
    let peer_ids: Vec<EndpointId> = st
        .peers
        .values()
        .filter(|p| matches!(p.gossip, Some(GossipStatus::Direct)))
        .map(|p| p.id)
        .collect();
    drop(st);

    let data_uri = crate::image::avif_to_data_uri(&avif_bytes);
    let _ = app.emit(
        "chat-image",
        serde_json::json!({
            "from_id": own_id.to_string(),
            "nickname": nickname,
            "image_id": image_id.to_string(),
            "data_uri": data_uri,
            "is_self": true,
        }),
    );

    let msg = ChatMsg::ImageStart {
        from: own_id,
        nickname: nickname.clone(),
        image_id,
        size: avif_size,
    };
    let payload = postcard::to_allocvec(&msg).unwrap_or_else(|e| {
        tracing::warn!("failed to serialize ImageStart: {e}");
        vec![]
    });
    let _ = sender.broadcast(Bytes::from(payload)).await;

    info!(
        peers = peer_ids.len(),
        image_id, avif_size, "sending image to peers"
    );
    let avif_bytes = Arc::new(avif_bytes);
    for peer_id in peer_ids {
        let ep = ep.clone();
        let avif_bytes = Arc::clone(&avif_bytes);
        let app2 = app.clone();
        tokio::spawn(async move {
            if let Err(e) = stream_image_to_peer(&ep, peer_id, image_id, &avif_bytes).await {
                tracing::warn!(peer = %peer_id.fmt_short(), "image stream failed: {e}");
                let _ = app2.emit(
                    "image-send-error",
                    serde_json::json!({
                        "image_id": image_id.to_string(),
                        "error": e,
                    }),
                );
            }
        });
    }

    Ok(())
}

async fn stream_image_to_peer(
    ep: &Endpoint,
    peer_id: EndpointId,
    image_id: u64,
    avif_bytes: &[u8],
) -> Result<(), String> {
    match ep.remote_info(peer_id).await {
        Some(info) => {
            let addr_count = info.addrs().count();
            info!(
                peer = %peer_id.fmt_short(),
                addr_count,
                "image stream: peer known to iroh"
            );
        }
        None => {
            tracing::warn!(
                peer = %peer_id.fmt_short(),
                "image stream: peer NOT known to iroh -- connect may fail"
            );
        }
    }

    info!(
        peer = %peer_id.fmt_short(),
        image_id,
        size = avif_bytes.len(),
        "image stream: connecting"
    );

    let addr = iroh::EndpointAddr::from(peer_id);

    let conn = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        ep.connect(addr, IMAGE_ALPN),
    )
    .await
    .map_err(|_| "connect timed out after 60s".to_string())?
    .map_err(|e| format!("connect: {e}"))?;

    info!(peer = %peer_id.fmt_short(), "image stream: connected, opening uni stream");

    let mut send = conn
        .open_uni()
        .await
        .map_err(|e| format!("open_uni: {e}"))?;

    info!(peer = %peer_id.fmt_short(), "image stream: writing header + data");

    send.write_all(&image_id.to_le_bytes())
        .await
        .map_err(|e| format!("write header: {e}"))?;

    let chunk_size = 8192;
    let mut written = 0usize;
    for chunk in avif_bytes.chunks(chunk_size) {
        send.write_all(chunk)
            .await
            .map_err(|e| format!("write data at offset {written}: {e}"))?;
        written += chunk.len();
    }

    send.finish().map_err(|e| format!("finish: {e}"))?;

    // Wait for the peer to ACK before dropping conn, otherwise
    // CONNECTION_CLOSE races with STREAM frames over slow BLE links.
    match tokio::time::timeout(std::time::Duration::from_secs(60), send.stopped()).await {
        Ok(stop_reason) => {
            info!(
                peer = %peer_id.fmt_short(),
                image_id,
                written,
                ?stop_reason,
                "image stream: complete (peer ACKed)"
            );
        }
        Err(_) => {
            tracing::warn!(
                peer = %peer_id.fmt_short(),
                image_id,
                "image stream: timed out waiting for peer ACK"
            );
        }
    }

    Ok(())
}

#[tauri::command]
async fn get_peers(state: State<'_, Arc<Mutex<AppState>>>) -> Result<Vec<PeerStateUI>, String> {
    let st = state.lock().await;
    let own_id = st.own_id();

    let mut peers: Vec<PeerStateUI> = vec![PeerStateUI {
        id: own_id.to_string(),
        nickname: Some(st.nickname.clone()),
        status: "self".to_string(),
        ble_phase: None,
        ble_path: None,
        ble_failures: 0,
        last_seen_secs_ago: 0,
    }];

    let mut others: Vec<PeerStateUI> = st.peers.values().map(PeerState::to_ui).collect();
    others.sort_by(|a, b| {
        let rank = |s: &str| match s {
            "connected" => 0,
            "handshaking" => 1,
            "connecting" => 2,
            "reconnecting" => 3,
            "in_topic" => 4,
            "nearby" => 5,
            "draining" => 6,
            "stale" => 7,
            "dead" => 8,
            _ => 9,
        };
        rank(&a.status)
            .cmp(&rank(&b.status))
            .then_with(|| a.nickname.cmp(&b.nickname))
    });
    peers.extend(others);
    Ok(peers)
}

#[tauri::command]
async fn set_debug(enabled: bool, state: State<'_, Arc<Mutex<AppState>>>) -> Result<(), String> {
    let st = state.lock().await;
    st.debug_enabled.store(enabled, Ordering::Relaxed);
    Ok(())
}
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let debug_enabled = Arc::new(AtomicBool::new(true));
    let debug_enabled2 = debug_enabled.clone();

    #[allow(unused_mut)]
    let mut builder = tauri::Builder::default();

    #[cfg(target_os = "android")]
    {
        builder = builder.plugin(tauri_plugin_blew::init());
    }

    #[cfg(mobile)]
    {
        builder = builder.plugin(tauri_plugin_barcode_scanner::init());
    }

    builder
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_deep_link::init())
        .setup(move |app| {
            let cache_dir = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| key_utils::cache_dir("iroh-ble-chat"));

            let secret_key = key_utils::load_or_generate_key_in(&cache_dir);
            let own_id: EndpointId = secret_key.public();
            let nickname = load_nickname(&cache_dir).unwrap_or_else(|| default_nickname(&own_id));

            let state = Arc::new(Mutex::new(AppState {
                secret_key,
                nickname,
                cache_dir,
                endpoint: None,
                _router: None,
                gossip_sender: None,
                ble_transport: None,
                peers: HashMap::new(),
                debug_enabled: debug_enabled2.clone(),
                pending_images: Default::default(),
            }));
            app.manage(state);

            let handle = app.handle().clone();
            let debug_layer = DebugLogLayer {
                debug_enabled: debug_enabled2,
                app: handle,
            };
            let env_filter =
                tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                    tracing_subscriber::EnvFilter::new(
                        "iroh_ble_transport=trace,iroh_gossip=info,iroh_ble_chat=debug,iroh_ble_chat_lib=debug,blew=info,warn",
                    )
                });
            let fmt_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stdout);
            #[cfg(target_os = "ios")]
            let fmt_layer = fmt_layer.with_ansi(false);

            tracing_subscriber::registry()
                .with(env_filter)
                .with(fmt_layer)
                .with(debug_layer)
                .init();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_node_id,
            start_node,
            add_peer,
            remove_peer,
            send_message,
            send_image,
            set_nickname,
            get_peers,
            set_debug,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
