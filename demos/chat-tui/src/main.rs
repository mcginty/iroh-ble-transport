//! BLE Chat -- a TUI chat app using the iroh-ble transport.
//!
//! Each node joins a shared iroh-gossip topic over BLE and broadcasts
//! messages to all peers.  The member list shows live peer status
//! (Direct / InTopic / Stale) derived from HyParView membership events.
//!
//! ## Usage
//!
//! ```sh
//! cargo run -p iroh-ble-chat-tui
//! ```
//!
//! ## Keys
//!
//! | Key | Action |
//! |-----|--------|
//! | `Enter` | Send message |
//! | `Ctrl+N` | Set your nickname |
//! | `Ctrl+A` | Add a peer by public key |
//! | `Ctrl+K` | Show your public key |
//! | `Ctrl+D` | Toggle debug mode |
//! | `↑ / ↓` | Scroll chat history |
//! | `Ctrl+C` | Quit |

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[path = "../../support/key_utils.rs"]
mod key_utils;

use bytes::Bytes;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use iroh::endpoint::presets;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointId};
use iroh_ble_chat_protocol::{ChatMsg, load_known_peers, save_known_peers};
use iroh_ble_transport::transport::BleTransport;
use iroh_ble_transport::{Central, CentralConfig, Peripheral};
use iroh_gossip::Gossip;
use iroh_gossip::proto::{HyparviewConfig, TopicId};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, Paragraph, Wrap},
};
use tokio::sync::mpsc;
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};
enum AppEvent {
    Key(KeyEvent),
    MessageReceived {
        nickname: String,
        text: String,
    },
    NicknameUpdate {
        from: EndpointId,
        new_nickname: String,
    },
    PeerUpdated {
        id: EndpointId,
        nickname: Option<String>,
        status: PeerStatus,
    },
    Log(String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum PeerStatus {
    Direct,
    InTopic,
    Stale,
}

#[derive(Clone)]
struct Member {
    id: EndpointId,
    nickname: String,
    status: PeerStatus,
    last_seen: Instant,
}

#[derive(Clone)]
struct ChatLine {
    time: String,
    sender: String,
    text: String,
    is_system: bool,
}

impl ChatLine {
    fn system(text: impl Into<String>) -> Self {
        Self {
            time: now_hhmm(),
            sender: String::new(),
            text: text.into(),
            is_system: true,
        }
    }
    fn message(sender: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            time: now_hhmm(),
            sender: sender.into(),
            text: text.into(),
            is_system: false,
        }
    }
}

fn now_hhmm() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let hours = (secs / 3600) % 24;
    let mins = (secs / 60) % 60;
    format!("{hours:02}:{mins:02}")
}

enum Modal {
    None,
    SetNickname(String),
    AddPeer(String),
    ShowKey,
}

struct AppState {
    nickname: String,
    own_id: EndpointId,
    member_order: Vec<EndpointId>,
    members: HashMap<EndpointId, Member>,
    messages: Vec<ChatLine>,
    input: String,
    modal: Modal,
    scroll: usize,
    gossip_sender: Option<iroh_gossip::api::GossipSender>,
    debug_enabled: Arc<AtomicBool>,
}

impl AppState {
    fn push_system(&mut self, text: impl Into<String>) {
        self.messages.push(ChatLine::system(text));
        if self.scroll > 0 {
            self.scroll += 1;
        }
    }

    fn push_message(&mut self, sender: impl Into<String>, text: impl Into<String>) {
        self.messages.push(ChatLine::message(sender, text));
        if self.scroll > 0 {
            self.scroll += 1;
        }
    }
}
/// Same topic ID as the Tauri chat app so both UIs interoperate.
fn chat_topic_id() -> TopicId {
    let hash = blake3::hash(b"iroh-ble-chat-v1");
    TopicId::from_bytes(*hash.as_bytes())
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
async fn gossip_event_pump(
    app_tx: mpsc::UnboundedSender<AppEvent>,
    mut receiver: iroh_gossip::api::GossipReceiver,
    own_id: EndpointId,
) {
    while let Some(Ok(event)) = n0_future::StreamExt::next(&mut receiver).await {
        match event {
            iroh_gossip::api::Event::NeighborUp(peer_id) => {
                if peer_id == own_id {
                    continue;
                }
                let _ = app_tx.send(AppEvent::PeerUpdated {
                    id: peer_id,
                    nickname: None,
                    status: PeerStatus::Direct,
                });
            }
            iroh_gossip::api::Event::NeighborDown(peer_id) => {
                if peer_id == own_id {
                    continue;
                }
                let _ = app_tx.send(AppEvent::PeerUpdated {
                    id: peer_id,
                    nickname: None,
                    status: PeerStatus::InTopic,
                });
            }
            iroh_gossip::api::Event::Received(msg) => {
                let Ok(chat_msg) = postcard::from_bytes::<ChatMsg>(&msg.content) else {
                    continue;
                };
                match chat_msg {
                    ChatMsg::Hello { from, nickname } => {
                        if from == own_id {
                            continue;
                        }
                        let _ = app_tx.send(AppEvent::PeerUpdated {
                            id: from,
                            nickname: Some(nickname),
                            status: PeerStatus::InTopic,
                        });
                    }
                    ChatMsg::Text {
                        from,
                        nickname,
                        text,
                    } => {
                        if from == own_id {
                            continue;
                        }
                        let _ = app_tx.send(AppEvent::PeerUpdated {
                            id: from,
                            nickname: Some(nickname.clone()),
                            status: PeerStatus::InTopic,
                        });
                        let _ = app_tx.send(AppEvent::MessageReceived { nickname, text });
                    }
                    ChatMsg::NicknameChanged { from, new_nickname } => {
                        if from == own_id {
                            continue;
                        }
                        let _ = app_tx.send(AppEvent::NicknameUpdate { from, new_nickname });
                    }
                    _ => {}
                }
            }
            iroh_gossip::api::Event::Lagged => {
                let _ = app_tx.send(AppEvent::Log("gossip event stream lagged".into()));
            }
        }
    }
}
async fn stale_tick(
    app_tx: mpsc::UnboundedSender<AppEvent>,
    members: std::sync::Arc<std::sync::Mutex<HashMap<EndpointId, Member>>>,
) {
    let stale_threshold = Duration::from_secs(120);
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        let mut map = members.lock().unwrap();
        for member in map.values_mut() {
            if member.last_seen.elapsed() > stale_threshold && member.status == PeerStatus::InTopic
            {
                member.status = PeerStatus::Stale;
                let _ = app_tx.send(AppEvent::PeerUpdated {
                    id: member.id,
                    nickname: Some(member.nickname.clone()),
                    status: PeerStatus::Stale,
                });
            }
        }
    }
}
fn render(frame: &mut Frame, state: &AppState) {
    let area = frame.area();

    let outer = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(3),
    ])
    .split(area);

    let debug_on = state.debug_enabled.load(Ordering::Relaxed);
    let header_text = if debug_on {
        format!(
            "  iroh-ble chat  |  {:<16}  |  Ctrl+N nick  Ctrl+A add peer  Ctrl+K key  Ctrl+D debug  Ctrl+C quit  [DEBUG]",
            state.nickname
        )
    } else {
        format!(
            "  iroh-ble chat  |  {:<16}  |  Ctrl+N nick  Ctrl+A add peer  Ctrl+K key  Ctrl+D debug  Ctrl+C quit",
            state.nickname
        )
    };
    let header = Paragraph::new(header_text).style(
        Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(header, outer[0]);

    let body = Layout::horizontal([Constraint::Length(22), Constraint::Min(0)]).split(outer[1]);

    let mut sorted_members: Vec<&Member> = state
        .member_order
        .iter()
        .filter_map(|id| state.members.get(id))
        .collect();
    sorted_members.sort_by(|a, b| {
        fn rank(m: &Member, own_id: &EndpointId) -> u8 {
            if m.id == *own_id {
                0
            } else {
                match m.status {
                    PeerStatus::Direct => 1,
                    PeerStatus::InTopic => 2,
                    PeerStatus::Stale => 3,
                }
            }
        }
        rank(a, &state.own_id)
            .cmp(&rank(b, &state.own_id))
            .then_with(|| a.nickname.cmp(&b.nickname))
    });

    let members: Vec<ListItem> = sorted_members
        .iter()
        .map(|m| {
            let (dot, color) = if m.id == state.own_id {
                ("◆", Color::Blue)
            } else {
                match m.status {
                    PeerStatus::Direct => ("●", Color::Green),
                    PeerStatus::InTopic => ("◐", Color::Yellow),
                    PeerStatus::Stale => ("○", Color::DarkGray),
                }
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {dot} "), Style::default().fg(color)),
                Span::raw(truncate_str(&m.nickname, 17)),
            ]))
        })
        .collect();

    frame.render_widget(
        List::new(members).block(Block::bordered().title(" Members ")),
        body[0],
    );

    let inner_h = body[1].height.saturating_sub(2) as usize;
    let total = state.messages.len();
    let start = total.saturating_sub(inner_h + state.scroll);
    let lines: Vec<Line> = state.messages[start..]
        .iter()
        .take(inner_h)
        .map(chat_line_to_ratatui)
        .collect();

    let scroll_hint = if state.scroll > 0 {
        format!(" Chat  ↑{} ", state.scroll)
    } else {
        " Chat ".to_string()
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(scroll_hint))
            .wrap(Wrap { trim: false }),
        body[1],
    );

    frame.render_widget(
        Paragraph::new(format!(" > {}_", state.input)).block(Block::bordered().title(" Message ")),
        outer[2],
    );

    match &state.modal {
        Modal::None => {}
        Modal::SetNickname(s) => {
            render_input_modal(frame, area, " Set Nickname ", "Enter new nickname:", s);
        }
        Modal::AddPeer(s) => {
            render_input_modal(frame, area, " Add Peer ", "Paste the peer's public key:", s);
        }
        Modal::ShowKey => {
            render_show_key(frame, area, &state.own_id.to_string());
        }
    }
}

fn chat_line_to_ratatui(line: &ChatLine) -> Line<'_> {
    if line.is_system {
        let style = if line.text.starts_with("[DBG]") {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC)
        };
        let prefix = if line.text.starts_with("[DBG]") {
            "  "
        } else {
            "  \u{2500} "
        };
        Line::from(Span::styled(format!("{prefix}{}", line.text), style))
    } else {
        Line::from(vec![
            Span::styled(
                format!("[{}] ", line.time),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("{}: ", line.sender),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(line.text.clone()),
        ])
    }
}

fn render_input_modal(frame: &mut Frame, area: Rect, title: &str, prompt: &str, input: &str) {
    let modal = centered_rect(64, 7, area);
    frame.render_widget(Clear, modal);
    let block = Block::bordered()
        .title(title)
        .style(Style::default().bg(Color::DarkGray).fg(Color::White));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!("  {prompt}")),
            Line::from(""),
            Line::from(format!("  > {input}_")),
            Line::from(""),
            Line::from(Span::styled(
                "  Enter: confirm   Esc: cancel",
                Style::default().fg(Color::Gray),
            )),
        ]),
        inner,
    );
}

fn render_show_key(frame: &mut Frame, area: Rect, key: &str) {
    let modal = centered_rect(72, 9, area);
    frame.render_widget(Clear, modal);
    let block = Block::bordered()
        .title(" Your Public Key -- share with others so they can add you ")
        .style(Style::default().bg(Color::DarkGray).fg(Color::White));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("  {key}"),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Select and copy from your terminal, then share it.",
                Style::default().fg(Color::Gray),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Esc / Enter: close",
                Style::default().fg(Color::Gray),
            )),
        ]),
        inner,
    );
}

fn centered_rect(width_pct: u16, height: u16, area: Rect) -> Rect {
    let w = (area.width * width_pct / 100).min(area.width);
    let h = height.min(area.height);
    Rect::new(
        area.x + (area.width.saturating_sub(w)) / 2,
        area.y + (area.height.saturating_sub(h)) / 2,
        w,
        h,
    )
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}...")
    }
}
fn modal_input_mut(modal: &mut Modal) -> Option<&mut String> {
    match modal {
        Modal::SetNickname(s) | Modal::AddPeer(s) => Some(s),
        _ => None,
    }
}
async fn run_app(
    mut terminal: Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut AppState,
    event_rx: &mut mpsc::UnboundedReceiver<AppEvent>,
    shared_members: &std::sync::Arc<std::sync::Mutex<HashMap<EndpointId, Member>>>,
) -> anyhow::Result<()> {
    let mut ticker = tokio::time::interval(Duration::from_millis(200));
    terminal.draw(|f| render(f, state))?;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                    {
                    let map = shared_members.lock().unwrap();
                    for (id, member) in map.iter() {
                        if let Some(m) = state.members.get_mut(id)
                            && m.status != member.status
                        {
                            m.status = member.status.clone();
                        }
                    }
                }
                terminal.draw(|f| render(f, state))?;
            }
            event = event_rx.recv() => {
                let Some(event) = event else { break };
                let quit = handle_event(event, state, shared_members).await?;
                terminal.draw(|f| render(f, state))?;
                if quit { break; }
            }
        }
    }
    Ok(())
}

async fn handle_event(
    event: AppEvent,
    state: &mut AppState,
    shared_members: &std::sync::Arc<std::sync::Mutex<HashMap<EndpointId, Member>>>,
) -> anyhow::Result<bool> {
    match event {
        AppEvent::Key(key) => {
            return handle_key(key, state).await;
        }

        AppEvent::PeerUpdated {
            id,
            nickname,
            status,
        } => {
            let entry = state.members.entry(id).or_insert_with(|| {
                state.member_order.push(id);
                Member {
                    id,
                    nickname: id.fmt_short().to_string(),
                    status: PeerStatus::InTopic,
                    last_seen: Instant::now(),
                }
            });
            if matches!(status, PeerStatus::Direct) || !matches!(entry.status, PeerStatus::Direct) {
                entry.status = status;
            }
            if let Some(nick) = nickname {
                entry.nickname = nick;
            }
            entry.last_seen = Instant::now();

            shared_members.lock().unwrap().insert(id, entry.clone());
        }

        AppEvent::MessageReceived { nickname, text } => {
            state.push_message(nickname, text);
        }

        AppEvent::NicknameUpdate { from, new_nickname } => {
            if let Some(m) = state.members.get_mut(&from) {
                let old = std::mem::replace(&mut m.nickname, new_nickname.clone());
                m.last_seen = Instant::now();
                shared_members.lock().unwrap().insert(from, m.clone());
                state.push_system(format!("{old} is now known as {new_nickname}"));
            }
        }

        AppEvent::Log(msg) => {
            state.push_system(msg);
        }
    }
    Ok(false)
}

async fn handle_key(key: KeyEvent, state: &mut AppState) -> anyhow::Result<bool> {
    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
        return Ok(true);
    }

    match key.code {
        KeyCode::Esc => {
            state.modal = Modal::None;
            return Ok(false);
        }
        KeyCode::Backspace => {
            if let Some(s) = modal_input_mut(&mut state.modal) {
                s.pop();
            } else {
                state.input.pop();
            }
            return Ok(false);
        }
        KeyCode::Up => {
            if matches!(state.modal, Modal::None) {
                state.scroll = state.scroll.saturating_add(1);
            }
            return Ok(false);
        }
        KeyCode::Down => {
            if matches!(state.modal, Modal::None) {
                state.scroll = state.scroll.saturating_sub(1);
            }
            return Ok(false);
        }
        _ => {}
    }

    match &state.modal {
        Modal::None => match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                let prev = state.debug_enabled.fetch_xor(true, Ordering::Relaxed);
                let now = !prev;
                state.push_system(if now {
                    "Debug mode ON -- showing BLE + gossip events"
                } else {
                    "Debug mode OFF"
                });
            }
            (KeyModifiers::CONTROL, KeyCode::Char('n')) => {
                let cur = state.nickname.clone();
                state.modal = Modal::SetNickname(cur);
            }
            (KeyModifiers::CONTROL, KeyCode::Char('a')) => {
                state.modal = Modal::AddPeer(String::new());
            }
            (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
                state.modal = Modal::ShowKey;
            }
            (_, KeyCode::Enter) => {
                let text = state.input.trim().to_string();
                if !text.is_empty() {
                    if let Some(sender) = &state.gossip_sender {
                        let msg = ChatMsg::Text {
                            from: state.own_id,
                            nickname: state.nickname.clone(),
                            text: text.clone(),
                        };
                        let payload = postcard::to_allocvec(&msg).unwrap_or_else(|e| {
                            tracing::warn!("failed to serialize Text: {e}");
                            vec![]
                        });
                        let _ = sender.broadcast(Bytes::from(payload)).await;
                    }
                    state.push_message(format!("{} (you)", state.nickname), text);
                    state.input.clear();
                    state.scroll = 0;
                }
            }
            (mods, KeyCode::Char(c))
                if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                state.input.push(c);
            }
            _ => {}
        },
        Modal::ShowKey => {
            if matches!(key.code, KeyCode::Enter | KeyCode::Char('q')) {
                state.modal = Modal::None;
            }
        }
        Modal::SetNickname(_) | Modal::AddPeer(_) => {
            match key.code {
                KeyCode::Enter => {
                    let modal = std::mem::replace(&mut state.modal, Modal::None);
                    match modal {
                        Modal::SetNickname(raw) => {
                            let new_nick = raw.trim().to_string();
                            if !new_nick.is_empty() {
                                if let Some(sender) = &state.gossip_sender {
                                    let msg = ChatMsg::NicknameChanged {
                                        from: state.own_id,
                                        new_nickname: new_nick.clone(),
                                    };
                                    let payload = postcard::to_allocvec(&msg).unwrap_or_else(|e| {
                                        tracing::warn!("failed to serialize NicknameChanged: {e}");
                                        vec![]
                                    });
                                    let _ = sender.broadcast(Bytes::from(payload)).await;
                                }
                                let cache_dir = key_utils::cache_dir("iroh-ble-chat");
                                save_nickname(&cache_dir, &new_nick);
                                state.push_system(format!("You are now known as {new_nick}"));
                                state.nickname = new_nick;
                            }
                        }
                        Modal::AddPeer(raw) => {
                            let trimmed = raw.trim().to_string();
                            match trimmed.parse::<EndpointId>() {
                                Ok(peer_id) => {
                                    if let std::collections::hash_map::Entry::Vacant(e) =
                                        state.members.entry(peer_id)
                                    {
                                        e.insert(Member {
                                            id: peer_id,
                                            nickname: peer_id.fmt_short().to_string(),
                                            status: PeerStatus::InTopic,
                                            last_seen: Instant::now(),
                                        });
                                        state.member_order.push(peer_id);
                                        let peers_to_save: Vec<EndpointId> = state
                                            .member_order
                                            .iter()
                                            .copied()
                                            .filter(|id| *id != state.own_id)
                                            .collect();
                                        save_known_peers(
                                            &key_utils::cache_dir("iroh-ble-chat"),
                                            &peers_to_save,
                                        );
                                        state.push_system(format!(
                                            "Added peer {}",
                                            peer_id.fmt_short()
                                        ));
                                        // Tell gossip to connect
                                        if let Some(sender) = &state.gossip_sender
                                            && let Err(e) = sender.join_peers(vec![peer_id]).await
                                        {
                                            state.push_system(format!("join_peers failed: {e}"));
                                        }
                                    } else {
                                        state.push_system("Peer already in list");
                                    }
                                }
                                Err(_) => {
                                    state.push_system("Invalid public key -- paste it exactly");
                                }
                            }
                        }
                        _ => {}
                    }
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    if let Some(s) = modal_input_mut(&mut state.modal) {
                        s.push(c);
                    }
                }
                _ => {}
            }
        }
    }

    Ok(false)
}
struct TuiLogLayer {
    tx: mpsc::UnboundedSender<AppEvent>,
    debug_enabled: Arc<AtomicBool>,
}

impl<S: tracing::Subscriber> Layer<S> for TuiLogLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let level = *event.metadata().level();
        let target = event.metadata().target();
        let debug_on = self.debug_enabled.load(Ordering::Relaxed);

        let pass =
            if debug_on && (target.starts_with("iroh_ble") || target.starts_with("iroh_gossip")) {
                level <= tracing::Level::DEBUG
            } else {
                level <= tracing::Level::WARN
            };

        if !pass {
            return;
        }

        let mut msg = String::new();
        event.record(
            &mut |field: &tracing::field::Field, val: &dyn std::fmt::Debug| {
                if field.name() == "message" {
                    msg = format!("{val:?}");
                }
            },
        );

        let prefix = if level > tracing::Level::WARN {
            "[DBG]"
        } else {
            &format!("[{level}]")
        };
        let _ = self
            .tx
            .send(AppEvent::Log(format!("{prefix} {target}: {msg}")));
    }
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let debug_enabled = Arc::new(AtomicBool::new(false));
    let (app_tx, mut app_rx) = mpsc::unbounded_channel::<AppEvent>();

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("iroh_ble=debug,iroh_gossip=debug,warn"));
    tracing_subscriber::registry()
        .with(
            TuiLogLayer {
                tx: app_tx.clone(),
                debug_enabled: debug_enabled.clone(),
            }
            .with_filter(env_filter),
        )
        .init();

    let secret = key_utils::load_or_generate_key("iroh-ble-chat");
    let own_id: EndpointId = secret.public();
    let cache_dir = key_utils::cache_dir("iroh-ble-chat");

    let initial_nick = load_nickname(&cache_dir).unwrap_or_else(|| default_nickname(&own_id));

    eprintln!("Initialising BLE transport...");
    let central = Arc::new(
        Central::with_config(CentralConfig {
            restore_identifier: Some("org.jakebot.chat-tui.central".into()),
        })
        .await?,
    );
    let peripheral = Arc::new(Peripheral::new().await?);
    let transport = BleTransport::new(own_id, central, peripheral).await?;
    let lookup = transport.address_lookup();
    let transport: Arc<dyn iroh::endpoint::transports::CustomTransport> = Arc::new(transport);

    let ep = Endpoint::builder(presets::N0DisableRelay)
        .add_custom_transport(Arc::clone(&transport))
        .address_lookup(lookup)
        .secret_key(secret)
        .clear_ip_transports()
        .bind()
        .await?;

    let hyparview = HyparviewConfig {
        active_view_capacity: 3,
        passive_view_capacity: 12,
        shuffle_interval: Duration::from_secs(120),
        ..Default::default()
    };

    let gossip = Gossip::builder()
        .membership_config(hyparview)
        .spawn(ep.clone());

    let router = Router::builder(ep.clone())
        .accept(iroh_gossip::ALPN, gossip.clone())
        .spawn();

    let known_peers = load_known_peers(&cache_dir);

    let topic = gossip
        .subscribe(chat_topic_id(), known_peers.clone())
        .await?;

    let (sender, receiver) = topic.split();

    let hello = ChatMsg::Hello {
        from: own_id,
        nickname: initial_nick.clone(),
    };
    let payload = postcard::to_allocvec(&hello).unwrap_or_else(|e| {
        tracing::warn!("failed to serialize Hello: {e}");
        vec![]
    });
    let _ = sender.broadcast(Bytes::from(payload)).await;

    tokio::spawn(gossip_event_pump(app_tx.clone(), receiver, own_id));

    let shared_members: std::sync::Arc<std::sync::Mutex<HashMap<EndpointId, Member>>> =
        std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
    tokio::spawn(stale_tick(app_tx.clone(), shared_members.clone()));

    let key_tx = app_tx.clone();
    tokio::spawn(async move {
        let mut stream = EventStream::new();
        loop {
            match stream.next().await {
                Some(Ok(Event::Key(k))) => {
                    if key_tx.send(AppEvent::Key(k)).is_err() {
                        break;
                    }
                }
                Some(Err(_)) | None => break,
                _ => {}
            }
        }
    });

    let mut initial_members_map = HashMap::new();
    let mut member_order = Vec::new();
    for &peer_id in &known_peers {
        let member = Member {
            id: peer_id,
            nickname: peer_id.fmt_short().to_string(),
            status: PeerStatus::InTopic,
            last_seen: Instant::now(),
        };
        initial_members_map.insert(peer_id, member.clone());
        member_order.push(peer_id);
        shared_members.lock().unwrap().insert(peer_id, member);
    }

    let mut state = AppState {
        nickname: initial_nick,
        own_id,
        member_order,
        members: initial_members_map,
        messages: vec![
            ChatLine::system("Welcome to iroh-ble chat!"),
            ChatLine::system(format!("Your key: {own_id}")),
            ChatLine::system("Ctrl+A add peer  Ctrl+N nickname  Ctrl+K show key  Ctrl+C quit"),
        ],
        input: String::new(),
        modal: Modal::None,
        scroll: 0,
        gossip_sender: Some(sender),
        debug_enabled: debug_enabled.clone(),
    };

    state.members.insert(
        own_id,
        Member {
            id: own_id,
            nickname: state.nickname.clone(),
            status: PeerStatus::Direct,
            last_seen: Instant::now(),
        },
    );
    state.member_order.insert(0, own_id);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let result = run_app(terminal, &mut state, &mut app_rx, &shared_members).await;

    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;

    router.shutdown().await?;
    ep.close().await;

    result
}
