import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import QRCode from "qrcode";
import { onOpenUrl } from "@tauri-apps/plugin-deep-link";
import { scan, Format, checkPermissions, requestPermissions } from "@tauri-apps/plugin-barcode-scanner";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

type PeerStatus =
  | "self"
  | "connected"
  | "handshaking"
  | "connecting"
  | "reconnecting"
  | "in_topic"
  | "nearby"
  | "draining"
  | "stale"
  | "dead"
  | "unknown";

interface PeerStateUI {
  id: string;
  nickname: string | null;
  status: PeerStatus;
  ble_phase: string | null;
  ble_failures: number;
  last_seen_secs_ago: number;
}

interface BlePeerDebugUI {
  device_id: string;
  phase: string;
  consecutive_failures: number;
}

interface ChatMsgPayload {
  from_id: string;
  nickname: string;
  text: string;
  is_self: boolean;
}

interface ChatImagePayload {
  from_id: string;
  nickname: string;
  image_id: string;
  data_uri: string;
  is_self: boolean;
}

interface ImageStartPayload {
  from_id: string;
  nickname: string;
  image_id: string;
  size: number;
  is_self: boolean;
}

interface ImageProgressPayload {
  image_id: string;
  bytes: number;
  total: number;
  is_self: boolean;
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

let myId = "";
let myNickname = "";
const peers = new Map<string, PeerStateUI>();
const blePeers = new Map<string, BlePeerDebugUI>();

// Sender colour palette (8 colours, indexed by hash of EndpointId)
const SENDER_COLOURS = 8;
function senderColourClass(id: string): string {
  let hash = 0;
  for (let i = 0; i < id.length; i++) {
    hash = (hash * 31 + id.charCodeAt(i)) | 0;
  }
  return `sender-c${Math.abs(hash) % SENDER_COLOURS}`;
}

// ---------------------------------------------------------------------------
// Elements
// ---------------------------------------------------------------------------

const myIdDisplay = document.getElementById("my-id-display")!;
const nicknameDisplay = document.getElementById("nickname-display")!;
const peerIdInput = document.getElementById("peer-id-input") as HTMLTextAreaElement;
const connectBtn = document.getElementById("connect-btn") as HTMLButtonElement;
const msgInput = document.getElementById("msg-input") as HTMLInputElement;
const sendBtn = document.getElementById("send-btn") as HTMLButtonElement;
const msgForm = document.getElementById("msg-form") as HTMLFormElement;
const chatMessages = document.getElementById("chat-messages")!;
const statusPill = document.getElementById("connection-status")!;
const connectToggleBtn = document.getElementById("connect-toggle-btn") as HTMLButtonElement;
const connectPanel = document.getElementById("connect-panel")!;
const copyIdBtn = document.getElementById("copy-id-btn") as HTMLButtonElement;
const membersToggleBtn = document.getElementById("members-toggle-btn") as HTMLButtonElement;
const membersSidebar = document.getElementById("members-sidebar")!;
const membersList = document.getElementById("members-list")!;
const blePeersSection = document.getElementById("ble-peers-section")!;
const blePeersList = document.getElementById("ble-peers-list")!;
const membersCount = document.getElementById("members-count")!;
const membersBadge = document.getElementById("members-badge")!;
const sidebarCloseBtn = document.getElementById("sidebar-close-btn") as HTMLButtonElement;
const drawerOverlay = document.getElementById("drawer-overlay")!;
const editNicknameBtn = document.getElementById("edit-nickname-btn") as HTMLButtonElement;
const nicknamePanel = document.getElementById("nickname-panel")!;
const nicknameInput = document.getElementById("nickname-input") as HTMLInputElement;
const nicknameSaveBtn = document.getElementById("nickname-save-btn") as HTMLButtonElement;
const debugCheckbox = document.getElementById("debug-checkbox") as HTMLInputElement;
const attachBtn = document.getElementById("attach-btn") as HTMLButtonElement;
const scanQrBtn = document.getElementById("scan-qr-btn") as HTMLButtonElement;
const showQrBtn = document.getElementById("show-qr-btn") as HTMLButtonElement;
const qrModal = document.getElementById("qr-modal")!;
const qrModalClose = document.getElementById("qr-modal-close") as HTMLButtonElement;
const qrContainer = document.getElementById("qr-container")!;
const qrIdText = document.getElementById("qr-id-text")!;
const confirmModal = document.getElementById("confirm-modal")!;
const confirmPeerId = document.getElementById("confirm-peer-id")!;
const confirmAddBtn = document.getElementById("confirm-add-btn") as HTMLButtonElement;
const confirmCancelBtn = document.getElementById("confirm-cancel-btn") as HTMLButtonElement;

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

function setStatus(text: string, cls: string) {
  statusPill.textContent = text;
  statusPill.className = `status-pill ${cls}`;
}

function updateStatusFromPeers() {
  let connected = 0;
  let pending = 0;
  let inTopic = 0;
  for (const p of peers.values()) {
    switch (p.status) {
      case "connected":
        connected++;
        break;
      case "handshaking":
      case "connecting":
      case "reconnecting":
      case "nearby":
        pending++;
        break;
      case "in_topic":
        inTopic++;
        break;
      default:
        break;
    }
  }
  if (connected === 0 && pending === 0 && inTopic === 0) {
    setStatus("Scanning…", "ready");
  } else {
    const parts: string[] = [];
    if (connected > 0) parts.push(`${connected} connected`);
    if (pending > 0) parts.push(`${pending} pending`);
    if (inTopic > 0) parts.push(`${inTopic} in topic`);
    setStatus(parts.join(" · "), connected > 0 ? "connected" : "ready");
  }

  if (connected > 0) {
    membersBadge.textContent = String(connected);
    membersBadge.style.display = "";
  } else {
    membersBadge.style.display = "none";
  }
}

// ---------------------------------------------------------------------------
// Members list rendering
// ---------------------------------------------------------------------------

const STATUS_RANK: Record<PeerStatus, number> = {
  self: 0,
  connected: 1,
  handshaking: 2,
  connecting: 3,
  reconnecting: 4,
  in_topic: 5,
  nearby: 6,
  draining: 7,
  stale: 8,
  dead: 9,
  unknown: 10,
};

const STATUS_LABEL: Record<PeerStatus, string> = {
  self: "you",
  connected: "Connected",
  handshaking: "Handshaking…",
  connecting: "Connecting…",
  reconnecting: "Reconnecting…",
  in_topic: "In topic",
  nearby: "Nearby",
  draining: "Draining",
  stale: "Stale",
  dead: "Unreachable",
  unknown: "—",
};

function renderMembers() {
  const sorted = Array.from(peers.values()).sort((a, b) => {
    const r = (STATUS_RANK[a.status] ?? 99) - (STATUS_RANK[b.status] ?? 99);
    if (r !== 0) return r;
    return (a.nickname ?? a.id).localeCompare(b.nickname ?? b.id);
  });

  membersList.innerHTML = "";
  for (const peer of sorted) {
    const shortId = peer.id.length > 12
      ? peer.id.slice(0, 8) + "…" + peer.id.slice(-4)
      : peer.id;
    const name = peer.nickname ?? shortId;

    const row = document.createElement("div");
    row.className = "member-row";
    row.dataset.peerId = peer.id;

    const dimmed = peer.status === "stale" || peer.status === "dead";
    const statusLabel = STATUS_LABEL[peer.status] ?? peer.status;
    const failureBadge = peer.ble_failures > 0
      ? ` · ${peer.ble_failures} fail${peer.ble_failures === 1 ? "" : "s"}`
      : "";
    row.innerHTML = `
      <div class="member-dot ${peer.status}"></div>
      <div class="member-info">
        <div class="member-name ${dimmed ? "stale" : ""}">${escapeHtml(name)}</div>
        <div class="member-id">${escapeHtml(statusLabel)}${escapeHtml(failureBadge)} · ${escapeHtml(shortId)}</div>
      </div>
    `;

    row.addEventListener("click", () => {
      const existing = row.nextElementSibling;
      if (existing?.classList.contains("member-detail")) {
        existing.remove();
        return;
      }
      membersList.querySelectorAll(".member-detail").forEach((el) => el.remove());

      const detail = document.createElement("div");
      detail.className = "member-detail expanded";
      const phaseLine = debugCheckbox.checked && peer.ble_phase
        ? `<div class="member-detail-phase">BLE: ${escapeHtml(peer.ble_phase)}${peer.ble_failures > 0 ? ` · ${peer.ble_failures} failure${peer.ble_failures === 1 ? "" : "s"}` : ""}</div>`
        : "";
      detail.innerHTML = `
        <div class="member-detail-id">${escapeHtml(peer.id)}</div>
        ${phaseLine}
        <div class="member-detail-actions">
          <button class="member-detail-copy">Copy ID</button>
          <button class="member-detail-remove">Remove</button>
        </div>
      `;
      const copyBtn = detail.querySelector(".member-detail-copy") as HTMLButtonElement;
      copyBtn.addEventListener("click", async (e) => {
        e.stopPropagation();
        try {
          await navigator.clipboard.writeText(peer.id);
          copyBtn.textContent = "Copied!";
          setTimeout(() => {
            copyBtn.textContent = "Copy ID";
          }, 1500);
        } catch {
          // ignore
        }
      });
      const removeBtn = detail.querySelector(".member-detail-remove") as HTMLButtonElement;
      removeBtn.addEventListener("click", async (e) => {
        e.stopPropagation();
        if (!confirm(`Remove ${peer.nickname ?? shortId}?`)) return;
        try {
          await invoke("remove_peer", { idStr: peer.id });
        } catch (err) {
          console.error("remove_peer failed", err);
        }
      });
      row.after(detail);
    });

    membersList.appendChild(row);
  }

  membersCount.textContent = String(sorted.length);
}

function renderBlePeers() {
  if (!debugCheckbox.checked || blePeers.size === 0) {
    blePeersSection.style.display = "none";
    blePeersList.innerHTML = "";
    return;
  }
  blePeersSection.style.display = "";

  const sorted = Array.from(blePeers.values()).sort((a, b) =>
    a.device_id.localeCompare(b.device_id),
  );

  blePeersList.innerHTML = "";
  for (const peer of sorted) {
    const row = document.createElement("div");
    row.className = "ble-peer-row";
    const failureBadge = peer.consecutive_failures > 0
      ? ` · ${peer.consecutive_failures} fail${peer.consecutive_failures === 1 ? "" : "s"}`
      : "";
    row.innerHTML = `
      <div class="ble-peer-id" title="${escapeHtml(peer.device_id)}">${escapeHtml(peer.device_id)}</div>
      <div class="ble-peer-phase">${escapeHtml(peer.phase)}${escapeHtml(failureBadge)}</div>
    `;
    blePeersList.appendChild(row);
  }
}

function escapeHtml(s: string): string {
  const div = document.createElement("div");
  div.textContent = s;
  return div.innerHTML;
}

// ---------------------------------------------------------------------------
// Drawer (mobile)
// ---------------------------------------------------------------------------

function openDrawer() {
  membersSidebar.classList.add("open");
  drawerOverlay.classList.add("visible");
  membersToggleBtn.classList.add("active");
}

function closeDrawer() {
  membersSidebar.classList.remove("open");
  drawerOverlay.classList.remove("visible");
  membersToggleBtn.classList.remove("active");
}

membersToggleBtn.addEventListener("click", () => {
  if (membersSidebar.classList.contains("open")) {
    closeDrawer();
  } else {
    openDrawer();
  }
});
sidebarCloseBtn.addEventListener("click", closeDrawer);
drawerOverlay.addEventListener("click", closeDrawer);
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") closeDrawer();
});

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

function appendMessage(msg: ChatMsgPayload) {
  const welcome = chatMessages.querySelector(".welcome");
  if (welcome) welcome.remove();

  const wrapper = document.createElement("div");
  const bubbleClass = msg.is_self ? "self" : "peer";
  wrapper.className = `message ${bubbleClass}`;

  if (!msg.is_self) {
    const nameEl = document.createElement("div");
    nameEl.className = `sender-name ${senderColourClass(msg.from_id)}`;
    nameEl.textContent = msg.nickname;
    wrapper.appendChild(nameEl);
  }

  const textEl = document.createElement("span");
  textEl.textContent = msg.text;
  wrapper.appendChild(textEl);

  chatMessages.appendChild(wrapper);
  chatMessages.scrollTop = chatMessages.scrollHeight;
}

const CIRCLE_CIRCUMFERENCE = 100.5; // 2 * PI * 16

function getOrCreatePlaceholder(imageId: string, isSelf: boolean, nickname: string, fromId: string): HTMLElement {
  const existing = document.querySelector(`[data-image-id="${imageId}"]`);
  if (existing) return existing as HTMLElement;

  const welcome = chatMessages.querySelector(".welcome");
  if (welcome) welcome.remove();

  const wrapper = document.createElement("div");
  const bubbleClass = isSelf ? "self" : "peer";
  wrapper.className = `message ${bubbleClass}`;
  wrapper.dataset.imageId = imageId;

  if (!isSelf) {
    const nameEl = document.createElement("div");
    nameEl.className = `sender-name ${senderColourClass(fromId)}`;
    nameEl.textContent = nickname;
    wrapper.appendChild(nameEl);
  }

  wrapper.insertAdjacentHTML("beforeend", `
    <div class="image-placeholder">
      <svg class="image-progress" viewBox="0 0 36 36">
        <circle class="progress-bg" cx="18" cy="18" r="16" />
        <circle class="progress-fill" cx="18" cy="18" r="16"
                stroke-dasharray="${CIRCLE_CIRCUMFERENCE}" stroke-dashoffset="${CIRCLE_CIRCUMFERENCE}" />
      </svg>
      <span class="image-status-label">${isSelf ? "Encoding..." : "Receiving..."}</span>
    </div>
  `);

  chatMessages.appendChild(wrapper);
  chatMessages.scrollTop = chatMessages.scrollHeight;
  return wrapper;
}

function updateProgress(imageId: string, bytes: number, total: number, isSelf: boolean) {
  const wrapper = getOrCreatePlaceholder(imageId, isSelf, "", "");
  const circle = wrapper.querySelector(".progress-fill") as SVGCircleElement | null;
  if (circle && total > 0) {
    const pct = Math.min(bytes / total, 1);
    circle.setAttribute("stroke-dashoffset", String(CIRCLE_CIRCUMFERENCE * (1 - pct)));
  }
}

function appendImageMessage(payload: ChatImagePayload) {
  const placeholder = document.querySelector(`[data-image-id="${payload.image_id}"]`);
  if (placeholder) {
    placeholder.remove();
  }

  const welcome = chatMessages.querySelector(".welcome");
  if (welcome) welcome.remove();

  const wrapper = document.createElement("div");
  const bubbleClass = payload.is_self ? "self" : "peer";
  wrapper.className = `message ${bubbleClass}`;

  if (!payload.is_self) {
    const nameEl = document.createElement("div");
    nameEl.className = `sender-name ${senderColourClass(payload.from_id)}`;
    nameEl.textContent = payload.nickname;
    wrapper.appendChild(nameEl);
  }

  const img = document.createElement("img");
  img.src = payload.data_uri;
  img.alt = "Image";
  wrapper.appendChild(img);

  chatMessages.appendChild(wrapper);
  chatMessages.scrollTop = chatMessages.scrollHeight;
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

async function initNode() {
  setStatus("Starting…", "starting");
  try {
    const result: { node_id: string; nickname: string } = await invoke("start_node");
    myId = result.node_id;
    myNickname = result.nickname;
    myIdDisplay.textContent = myId.slice(0, 22) + "…";
    (myIdDisplay as HTMLElement).title = myId;
    nicknameDisplay.textContent = myNickname;

    peers.set(myId, {
      id: myId,
      nickname: myNickname,
      status: "self",
      ble_phase: null,
      ble_failures: 0,
      last_seen_secs_ago: 0,
    });
    renderMembers();
    updateStatusFromPeers();

    const welcomeP = chatMessages.querySelector(".welcome p");
    if (welcomeP) {
      welcomeP.innerHTML = "Node is ready.<br>Tap ＋ to add a peer.";
    }

    msgInput.disabled = false;
    sendBtn.disabled = false;
    attachBtn.disabled = false;
  } catch (e: any) {
    console.error(e);
    const errMsg = typeof e === "string" ? e : String(e);
    myIdDisplay.textContent = "Error";
    setStatus("Failed to start", "error");

    // Remove any debug-log messages so the error is visible.
    const welcome = chatMessages.querySelector(".welcome");
    if (welcome) welcome.remove();

    const errDiv = document.createElement("div");
    errDiv.className = "welcome";
    errDiv.innerHTML = `<div class="welcome-icon">⚠️</div><p>${escapeHtml(errMsg)}<br><br><a href="#" id="retry-start">Tap to retry</a></p>`;
    chatMessages.prepend(errDiv);

    document.getElementById("retry-start")?.addEventListener("click", (ev) => {
      ev.preventDefault();
      errDiv.remove();
      initNode();
    });
  }
}

document.addEventListener("DOMContentLoaded", () => initNode());

// ---------------------------------------------------------------------------
// Copy ID
// ---------------------------------------------------------------------------

copyIdBtn.addEventListener("click", async () => {
  if (!myId) return;
  try {
    await navigator.clipboard.writeText(myId);
    copyIdBtn.textContent = "✓";
    setTimeout(() => (copyIdBtn.textContent = "⎘"), 1500);
  } catch {
    copyIdBtn.textContent = "✗";
    setTimeout(() => (copyIdBtn.textContent = "⎘"), 1500);
  }
});

// ---------------------------------------------------------------------------
// QR Code scanning
// ---------------------------------------------------------------------------

scanQrBtn.addEventListener("click", async () => {
  try {
    let perm = await checkPermissions();
    if (perm !== "granted") {
      perm = await requestPermissions();
    }
    if (perm !== "granted") {
      console.warn("Camera permission denied");
      return;
    }

    const result = await scan({ formats: [Format.QRCode] });
    const peerId = parseDeepLink(result.content) ?? result.content.trim();
    if (peerId.length > 0) {
      showConfirmDialog(peerId);
    }
  } catch (e: any) {
    const msg = typeof e === "string" ? e : String(e);
    if (!msg.includes("cancelled") && !msg.includes("canceled")) {
      console.error("QR scan failed", e);
    }
  }
});

// ---------------------------------------------------------------------------
// QR Code modal
// ---------------------------------------------------------------------------

showQrBtn.addEventListener("click", async () => {
  if (!myId) return;

  const deepLink = `iroh-ble-chat:///add-peer/${myId}`;

  // Generate QR as SVG string
  try {
    const svgString = await QRCode.toString(deepLink, {
      type: "svg",
      margin: 1,
      width: 240,
      color: { dark: "#000000", light: "#ffffff" },
    });
    qrContainer.innerHTML = svgString;
  } catch (e) {
    console.error("QR generation failed", e);
    qrContainer.innerHTML = "<p>Failed to generate QR code</p>";
  }

  qrIdText.textContent = myId;
  qrModal.style.display = "";
});

qrModalClose.addEventListener("click", () => {
  qrModal.style.display = "none";
});

qrModal.addEventListener("click", (e) => {
  if (e.target === qrModal) {
    qrModal.style.display = "none";
  }
});

// ---------------------------------------------------------------------------
// Deep link confirmation dialog
// ---------------------------------------------------------------------------

let pendingDeepLinkPeerId: string | null = null;

function showConfirmDialog(peerId: string) {
  pendingDeepLinkPeerId = peerId;
  const shortId = peerId.length > 20
    ? peerId.slice(0, 10) + "..." + peerId.slice(-10)
    : peerId;
  confirmPeerId.textContent = shortId;
  confirmPeerId.title = peerId;
  confirmModal.style.display = "";
}

function hideConfirmDialog() {
  confirmModal.style.display = "none";
  pendingDeepLinkPeerId = null;
}

confirmAddBtn.addEventListener("click", async () => {
  if (!pendingDeepLinkPeerId) return;
  const peerId = pendingDeepLinkPeerId;
  hideConfirmDialog();

  try {
    await invoke("add_peer", { idStr: peerId });
    appendMessage({
      from_id: myId,
      nickname: myNickname,
      text: `Peer added from QR code`,
      is_self: true,
    });
  } catch (e: any) {
    console.error("Failed to add peer from deep link", e);
    appendMessage({
      from_id: myId,
      nickname: myNickname,
      text: `Failed to add peer: ${e}`,
      is_self: true,
    });
  }
});

confirmCancelBtn.addEventListener("click", () => {
  hideConfirmDialog();
});

confirmModal.addEventListener("click", (e) => {
  if (e.target === confirmModal) {
    hideConfirmDialog();
  }
});

// ---------------------------------------------------------------------------
// Deep link handler (mobile)
// ---------------------------------------------------------------------------

function parseDeepLink(url: string): string | null {
  // Expected: iroh-ble-chat:///add-peer/<endpoint-id>
  try {
    const parsed = new URL(url);
    if (parsed.protocol === "iroh-ble-chat:") {
      const match = parsed.pathname.match(/^\/add-peer\/(.+)$/);
      if (match) {
        const peerId = match[1].trim();
        if (peerId.length > 0) return peerId;
      }
    }
  } catch {
    // ignore parse errors
  }
  return null;
}

// Register deep-link listener. onOpenUrl is a no-op on desktop.
onOpenUrl((urls: string[]) => {
  for (const url of urls) {
    const peerId = parseDeepLink(url);
    if (peerId) {
      showConfirmDialog(peerId);
      break; // only handle the first valid URL
    }
  }
}).catch((e) => {
  // Expected to fail on desktop where deep links aren't configured
  console.debug("Deep link registration skipped:", e);
});

// ---------------------------------------------------------------------------
// Add peer panel
// ---------------------------------------------------------------------------

connectToggleBtn.addEventListener("click", () => {
  const isCollapsed = connectPanel.classList.toggle("collapsed");
  connectToggleBtn.classList.toggle("active", !isCollapsed);
  connectToggleBtn.textContent = isCollapsed ? "＋" : "✕";
  if (!isCollapsed) {
    nicknamePanel.classList.add("collapsed");
    setTimeout(() => peerIdInput.focus(), 350);
  }
});

connectBtn.addEventListener("click", async () => {
  const peerId = peerIdInput.value.trim();
  if (!peerId) return;

  connectBtn.textContent = "Adding…";
  connectBtn.disabled = true;

  try {
    await invoke("add_peer", { idStr: peerId });
    connectBtn.textContent = "Added ✓";
    peerIdInput.value = "";
    setTimeout(() => {
      connectPanel.classList.add("collapsed");
      connectToggleBtn.classList.remove("active");
      connectToggleBtn.textContent = "＋";
      connectBtn.textContent = "Add Peer";
      connectBtn.disabled = false;
    }, 1000);
  } catch (e: any) {
    console.error(e);
    connectBtn.textContent = "Retry";
    connectBtn.disabled = false;
  }
});

// ---------------------------------------------------------------------------
// Nickname edit
// ---------------------------------------------------------------------------

editNicknameBtn.addEventListener("click", () => {
  const isCollapsed = nicknamePanel.classList.toggle("collapsed");
  if (!isCollapsed) {
    connectPanel.classList.add("collapsed");
    connectToggleBtn.classList.remove("active");
    connectToggleBtn.textContent = "＋";
    nicknameInput.value = myNickname;
    setTimeout(() => nicknameInput.focus(), 350);
  }
});

nicknameSaveBtn.addEventListener("click", async () => {
  const newNick = nicknameInput.value.trim();
  if (!newNick) return;
  try {
    await invoke("set_nickname", { nickname: newNick });
    myNickname = newNick;
    nicknameDisplay.textContent = newNick;
    nicknamePanel.classList.add("collapsed");

    const selfPeer = peers.get(myId);
    if (selfPeer) {
      selfPeer.nickname = newNick;
      renderMembers();
    }
  } catch (e) {
    console.error("Failed to set nickname", e);
  }
});

nicknameInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    e.preventDefault();
    nicknameSaveBtn.click();
  }
});

// ---------------------------------------------------------------------------
// Send message
// ---------------------------------------------------------------------------

msgForm.addEventListener("submit", async (e) => {
  e.preventDefault();
  const text = msgInput.value.trim();
  if (!text) return;

  msgInput.value = "";

  try {
    await invoke("send_message", { text });
  } catch (e) {
    console.error("Failed to send", e);
    appendMessage({
      from_id: myId,
      nickname: myNickname,
      text: "Failed to send message",
      is_self: true,
    });
  }
});

attachBtn.addEventListener("click", async () => {
  try {
    await invoke("send_image");
  } catch (e: any) {
    console.error("Failed to send image", e);
    appendMessage({
      from_id: myId,
      nickname: myNickname,
      text: "Failed to send image: " + e,
      is_self: true,
    });
  }
});

// ---------------------------------------------------------------------------
// Tauri events
// ---------------------------------------------------------------------------

listen("chat-msg", (event: any) => {
  const payload: ChatMsgPayload = event.payload;
  appendMessage(payload);
});

listen("peer-updated", (event: any) => {
  const peer: PeerStateUI = event.payload;
  peers.set(peer.id, peer);
  renderMembers();
  updateStatusFromPeers();
});

listen("peer-removed", (event: any) => {
  peers.delete(event.payload.id);
  renderMembers();
  updateStatusFromPeers();
});

listen("topic-joined", (_event: any) => {
  updateStatusFromPeers();
});

// ---------------------------------------------------------------------------
// Debug toggle
// ---------------------------------------------------------------------------

debugCheckbox.addEventListener("change", async () => {
  try {
    await invoke("set_debug", { enabled: debugCheckbox.checked });
  } catch (e) {
    console.error("Failed to set debug", e);
    debugCheckbox.checked = false;
  }
  renderBlePeers();
});

listen("ble-peer-updated", (event: any) => {
  const payload = event.payload as BlePeerDebugUI;
  blePeers.set(payload.device_id, payload);
  renderBlePeers();
});

listen("ble-peer-removed", (event: any) => {
  const payload = event.payload as { device_id: string };
  blePeers.delete(payload.device_id);
  renderBlePeers();
});

listen("debug-log", (event: any) => {
  const { target, message } = event.payload as {
    target: string;
    level: string;
    message: string;
  };

  const welcome = chatMessages.querySelector(".welcome");
  if (welcome) welcome.remove();

  const wrapper = document.createElement("div");
  wrapper.className = "message debug";

  const textEl = document.createElement("span");
  textEl.textContent = `[DBG] ${target}: ${message}`;
  wrapper.appendChild(textEl);

  chatMessages.appendChild(wrapper);
  chatMessages.scrollTop = chatMessages.scrollHeight;
});

listen("image-start", (event: any) => {
  const payload: ImageStartPayload = event.payload;
  getOrCreatePlaceholder(payload.image_id, payload.is_self, payload.nickname, payload.from_id);
});

listen("image-progress", (event: any) => {
  const payload: ImageProgressPayload = event.payload;
  updateProgress(payload.image_id, payload.bytes, payload.total, payload.is_self);
});

listen("chat-image", (event: any) => {
  const payload: ChatImagePayload = event.payload;
  appendImageMessage(payload);
});

listen("image-send-error", (event: any) => {
  const { image_id, error } = event.payload;
  const placeholder = document.querySelector(`[data-image-id="${image_id}"]`);
  if (placeholder) {
    const label = placeholder.querySelector(".image-status-label");
    if (label) label.textContent = `Failed: ${error}`;
    const progress = placeholder.querySelector(".image-progress") as SVGElement | null;
    if (progress) progress.style.display = "none";
  }
});

// ---------------------------------------------------------------------------
// Bandwidth indicator
// ---------------------------------------------------------------------------

const bwTx = document.getElementById("bw-tx")!;
const bwRx = document.getElementById("bw-rx")!;
const bwRtx = document.getElementById("bw-rtx")!;
const bwTrunc = document.getElementById("bw-trunc")!;

let rtxTotal = 0;
let truncTotal = 0;

listen("bandwidth", (event: any) => {
  const { tx_kbps, rx_kbps, retransmits, truncations } = event.payload as {
    tx_kbps: number;
    rx_kbps: number;
    retransmits: number;
    truncations: number;
  };
  bwTx.textContent = tx_kbps < 10 ? tx_kbps.toFixed(1) : Math.round(tx_kbps).toString();
  bwRx.textContent = rx_kbps < 10 ? rx_kbps.toFixed(1) : Math.round(rx_kbps).toString();
  rtxTotal += retransmits ?? 0;
  truncTotal += truncations ?? 0;
  bwRtx.textContent = rtxTotal.toString();
  bwTrunc.textContent = truncTotal.toString();
});
