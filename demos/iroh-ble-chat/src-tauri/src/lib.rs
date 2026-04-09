use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointId, RelayMode};
use iroh_ble_chat_protocol::{load_known_peers, save_known_peers, ChatMsg, IMAGE_ALPN};
use iroh_ble_transport::transport::BleTransport;
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
#[derive(Clone, Debug)]
enum PeerStatus {
    Direct,
    InTopic,
    Stale,
}

#[derive(Clone, Debug)]
struct PeerState {
    id: EndpointId,
    nickname: Option<String>,
    status: PeerStatus,
    last_seen: Instant,
}

#[derive(Clone, Serialize)]
struct PeerStateUI {
    id: String,
    nickname: Option<String>,
    status: String,
    last_seen_secs_ago: u64,
}

impl PeerState {
    fn to_ui(&self) -> PeerStateUI {
        let status = match self.status {
            PeerStatus::Direct => "direct",
            PeerStatus::InTopic => "in_topic",
            PeerStatus::Stale => "stale",
        };
        PeerStateUI {
            id: self.id.to_string(),
            nickname: self.nickname.clone(),
            status: status.to_string(),
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
    peers: HashMap<EndpointId, PeerState>,
    debug_enabled: Arc<AtomicBool>,
    pending_images: crate::image::PendingImages,
    transport: Option<BleTransport>,
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

    let transport = BleTransport::new(&st.secret_key).await.map_err(|e| {
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

    let ep = Endpoint::builder(transport.preset())
        .secret_key(st.secret_key.clone())
        .relay_mode(RelayMode::Disabled)
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

    let topic = gossip
        .subscribe(chat_topic_id(), known_peers.clone())
        .await
        .map_err(|e| format!("gossip subscribe: {e}"))?;

    let (sender, receiver) = topic.split();

    st.endpoint = Some(ep);
    st._router = Some(router);
    st.gossip_sender = Some(sender.clone());
    st.transport = Some(transport);

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
        receiver,
        own_id,
    ));

    tauri::async_runtime::spawn(stale_tick(app.clone(), state_arc.clone()));
    tauri::async_runtime::spawn(bandwidth_tick(app.clone(), state_arc));

    Ok(result)
}

async fn gossip_event_pump(
    app: AppHandle,
    state: Arc<Mutex<AppState>>,
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
                let peer = st.peers.entry(peer_id).or_insert_with(|| PeerState {
                    id: peer_id,
                    nickname: None,
                    status: PeerStatus::Direct,
                    last_seen: Instant::now(),
                });
                peer.status = PeerStatus::Direct;
                peer.last_seen = Instant::now();
                let ui = peer.to_ui();
                let _ = app.emit("peer-updated", &ui);

                if !topic_joined {
                    topic_joined = true;
                    let active = st
                        .peers
                        .values()
                        .filter(|p| matches!(p.status, PeerStatus::Direct))
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
                    peer.status = PeerStatus::InTopic;
                    let ui = peer.to_ui();
                    let _ = app.emit("peer-updated", &ui);
                }
            }
            iroh_gossip::api::Event::Received(msg) => {
                let Ok(chat_msg) = postcard::from_bytes::<ChatMsg>(&msg.content) else {
                    continue;
                };

                match &chat_msg {
                    ChatMsg::Hello { from, nickname } => {
                        if *from == own_id {
                            continue;
                        }
                        let mut st = state.lock().await;
                        let peer = st.peers.entry(*from).or_insert_with(|| PeerState {
                            id: *from,
                            nickname: None,
                            status: PeerStatus::InTopic,
                            last_seen: Instant::now(),
                        });
                        peer.nickname = Some(nickname.clone());
                        peer.last_seen = Instant::now();
                        let ui = peer.to_ui();
                        let _ = app.emit("peer-updated", &ui);
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
                        let peer = st.peers.entry(*from).or_insert_with(|| PeerState {
                            id: *from,
                            nickname: None,
                            status: PeerStatus::InTopic,
                            last_seen: Instant::now(),
                        });
                        peer.nickname = Some(nickname.clone());
                        peer.last_seen = Instant::now();
                        let ui = peer.to_ui();
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
                    }
                    ChatMsg::NicknameChanged { from, new_nickname } => {
                        if *from == own_id {
                            continue;
                        }
                        let mut st = state.lock().await;
                        let peer = st.peers.entry(*from).or_insert_with(|| PeerState {
                            id: *from,
                            nickname: None,
                            status: PeerStatus::InTopic,
                            last_seen: Instant::now(),
                        });
                        peer.nickname = Some(new_nickname.clone());
                        peer.last_seen = Instant::now();
                        let ui = peer.to_ui();
                        let _ = app.emit("peer-updated", &ui);
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
                    }
                }
            }
            iroh_gossip::api::Event::Lagged => {
                tracing::warn!("gossip event stream lagged");
            }
        }
    }
}

async fn bandwidth_tick(app: AppHandle, state: Arc<Mutex<AppState>>) {
    let mut prev_tx = 0u64;
    let mut prev_rx = 0u64;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let transport = {
            let st = state.lock().await;
            st.transport.clone()
        };
        let Some(transport) = transport else {
            continue;
        };
        let stats = transport.stats();
        let tx_delta = stats.tx_bytes.saturating_sub(prev_tx);
        let rx_delta = stats.rx_bytes.saturating_sub(prev_rx);
        prev_tx = stats.tx_bytes;
        prev_rx = stats.rx_bytes;
        let tx_kbps = (tx_delta as f64 * 8.0) / 1000.0;
        let rx_kbps = (rx_delta as f64 * 8.0) / 1000.0;
        let _ = app.emit(
            "bandwidth",
            serde_json::json!({ "tx_kbps": tx_kbps, "rx_kbps": rx_kbps }),
        );
    }
}

async fn stale_tick(app: AppHandle, state: Arc<Mutex<AppState>>) {
    let stale_threshold = std::time::Duration::from_secs(120);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        let mut st = state.lock().await;
        for peer in st.peers.values_mut() {
            if peer.last_seen.elapsed() > stale_threshold
                && matches!(peer.status, PeerStatus::InTopic)
            {
                peer.status = PeerStatus::Stale;
                let ui = peer.to_ui();
                let _ = app.emit("peer-updated", &ui);
            }
        }
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

        if !(target.starts_with("iroh_ble") || target.starts_with("iroh_gossip")) {
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
        let peer = PeerState {
            id: peer_id,
            nickname: None,
            status: PeerStatus::InTopic,
            last_seen: Instant::now(),
        };
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
async fn send_message(
    text: String,
    app: AppHandle,
    state: State<'_, Arc<Mutex<AppState>>>,
) -> Result<(), String> {
    let st = state.lock().await;
    let sender = st.gossip_sender.as_ref().ok_or("Node not started")?.clone();
    let own_id = st.own_id();
    let nickname = st.nickname.clone();
    drop(st);

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
        .filter(|p| matches!(p.status, PeerStatus::Direct))
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
        last_seen_secs_ago: 0,
    }];

    let mut others: Vec<PeerStateUI> = st.peers.values().map(|p| p.to_ui()).collect();
    others.sort_by(|a, b| {
        let rank = |s: &str| match s {
            "direct" => 0,
            "in_topic" => 1,
            "stale" => 2,
            _ => 3,
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
                peers: HashMap::new(),
                debug_enabled: debug_enabled2.clone(),
                pending_images: Default::default(),
                transport: None,
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
                        "iroh_ble=debug,iroh_gossip=info,iroh_ble_chat=info,warn",
                    )
                });
            tracing_subscriber::registry()
                .with(env_filter)
                .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
                .with(debug_layer)
                .init();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_node_id,
            start_node,
            add_peer,
            send_message,
            send_image,
            set_nickname,
            get_peers,
            set_debug,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
