# L2CAP Revival Design

Date: 2026-04-15

## Overview

Re-enable the dormant L2CAP data path in `iroh-ble-transport` so connected peers use L2CAP CoC by default, falling back to GATT when L2CAP is unavailable. Requires a small upstream change in blew (alpha.6) and wiring through the registry, driver, pipe, and chat demo.

## Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Accept-side identity | Upstream blew (option B) | Trivially feasible on all 3 platforms; keeps protocol simple |
| Path selection timing | Blocking pick (option A) | Bounded 1.5 s delay, avoids mid-flight pipe swap complexity |
| Default policy | `PreferL2cap` | L2CAP supported by virtually all modern devices |
| Rollout gate | `L2capPolicy` runtime knob | Allows opt-out without feature flags |
| ReliableChannel on L2CAP | Bypassed | L2CAP CoC is already reliable and stream-oriented |

## Part 1 — Upstream blew changes (alpha.6)

### API change

`Peripheral::l2cap_listener` stream item changes from `BlewResult<L2capChannel>` to `BlewResult<(DeviceId, L2capChannel)>`.

`Central::open_l2cap_channel` is unchanged (caller already knows the DeviceId).

### Per-platform wiring

**Apple (macOS/iOS).** In `didOpenL2CAPChannel` callback, `ch.peer()` is already cast to `CBCentral` for latency tuning. Call the existing `central_device_id(central)` helper and send `(device_id, l2cap)` into the accept stream. ~3 LOC.

**Android.** Change `accept_tx` type in `l2cap_state.rs` from `Sender<BlewResult<L2capChannel>>` to `Sender<BlewResult<(DeviceId, L2capChannel)>>`. In `on_channel_opened`, when `from_server`, build `DeviceId(device_addr.to_string())` and send the tuple. ~8 LOC.

**Linux.** `listener.accept()` already yields `(stream, addr)`. Keep `addr`, build `DeviceId(addr.addr.to_string())`, send tuple. ~3 LOC.

**testing.rs.** Update `l2cap_accept_tx` type, thread the connected central's DeviceId through the mock accept path. ~15 LOC.

**examples/l2cap_server.rs.** Destructure the new tuple. ~2 LOC.

### PSM characteristic value hook

Verify that blew exposes a way to update a GATT characteristic's value after registration (e.g. `Peripheral::update_characteristic_value` or a read-request responder). If not, add one in the same alpha.6 release. The transport needs this to publish the 2-byte LE PSM into `IROH_PSM_CHAR_UUID` after `l2cap_listener()` returns.

### Hardware validation

Confirm on real Apple hardware that the `CBCentral.identifier` seen in `didOpenL2CAPChannel` matches the one from GATT write/subscribe callbacks. Same CoreBluetooth session, expected to match, but needs device-level confirmation.

## Part 2 — State machine & path selection

### L2capPolicy

```rust
pub enum L2capPolicy {
    /// Never attempt L2CAP. GATT-only.
    Disabled,
    /// Try L2CAP first, GATT fallback on failure. Default.
    PreferL2cap,
}
```

Passed via `BleTransportConfig` to a new `BleTransport::with_config` constructor. The existing `BleTransport::new` signature is unchanged and always uses `PreferL2cap`. To opt out, callers use `BleTransport::with_config(..., BleTransportConfig { l2cap_policy: L2capPolicy::Disabled })`.

### Central-side connection flow

When policy is `PreferL2cap`:

```
Connecting
  → ConnectSucceeded { channel: ChannelHandle { path: Gatt } }
  → Handshaking { channel, since, l2cap_deadline: Some(now + 1500ms) }
     → OpenL2cap action dispatched (driver reads PSM + opens L2CAP)
     → OpenL2capSucceeded: → Connected { path: L2cap }, StartDataPipe(L2cap)
     → OpenL2capFailed / timeout: → Connected { path: Gatt }, StartDataPipe(Gatt)
```

When policy is `Disabled`:

```
Connecting → ConnectSucceeded → Connected { path: Gatt } → StartDataPipe(Gatt)
```

(Today's behavior, unchanged.)

### Constants

| Constant | Value | Purpose |
|----------|-------|---------|
| `L2CAP_SELECT_TIMEOUT` | 1500 ms | Deadline for read_psm + open_l2cap_channel |
| `HANDSHAKING_L2CAP_WALL` | 2000 ms | Tick-enforced wall-clock fallback if driver task lost |

### Handshaking phase extension

```rust
PeerPhase::Handshaking {
    since: Instant,
    channel: ChannelHandle,
    l2cap_deadline: Option<Instant>,  // Some when policy=PreferL2cap
}
```

On `Tick`, if `l2cap_deadline` is `Some(d)` and `now > d + 500ms`, registry synthesizes `OpenL2capFailed` to prevent getting stuck in `Handshaking`.

### OpenL2cap driver action

The driver spawns a task that:
1. Calls `iface.read_psm(&device_id)`.
2. If `Ok(Some(psm))`: calls `iface.open_l2cap(&device_id, psm)`.
3. Wraps the whole sequence in `tokio::time::timeout(L2CAP_SELECT_TIMEOUT)`.
4. Sends `OpenL2capSucceeded { device_id, channel }` or `OpenL2capFailed { device_id, error }`.

This is the existing `PeerAction::OpenL2cap` + `PeerCommand::OpenL2capSucceeded/Failed` wiring — it just needs the registry to actually emit `OpenL2cap` now.

### PeerEntry gets an L2CAP channel field

```rust
pub struct PeerEntry {
    // ... existing fields ...
    pub l2cap_channel: Option<L2capChannel>,
}
```

Set on `OpenL2capSucceeded` or `InboundL2capChannel`, consumed and cleared when the driver reads the `StartDataPipe` action.

### Peripheral-side accept path

At transport startup, if policy is `PreferL2cap`:
1. Call `peripheral.l2cap_listener()`.
2. Publish the returned 2-byte LE PSM as the value of `IROH_PSM_CHAR_UUID`.
3. Spawn a task draining the accept stream, forwarding each `(device_id, channel)` as `PeerCommand::InboundL2capChannel { device_id, channel }` (variant already exists).

Registry handling of `InboundL2capChannel`:

| Existing state | Action |
|----------------|--------|
| No entry | Create Peripheral-role entry, `Connected { path: L2cap }`, `StartDataPipe(L2cap)` |
| Entry exists, no pipe yet | Upgrade to `Connected { path: L2cap }`, `StartDataPipe(L2cap)` |
| Entry exists with live GATT pipe | Drop channel, log metric `l2cap_late_accept_after_gatt` |
| Entry exists with live L2CAP pipe | Drop channel, log metric `l2cap_duplicate_accept` |

### Routing

No changes. Token-based addressing is path-agnostic.

## Part 3 — Data pipe: L2CAP bypasses ReliableChannel

### StartDataPipe action grows a path field

```rust
PeerAction::StartDataPipe {
    device_id: DeviceId,
    role: ConnectRole,
    path: ConnectPath,
}
```

### run_data_pipe branches on path

**GATT path.** Unchanged. `ReliableChannel` + ARQ + fragment/reassemble + `iface.write_c2p` / `iface.notify_p2c`.

**L2CAP path.** Consumes `PeerEntry::l2cap_channel`:
1. Split `L2capChannel` via `tokio::io::split`.
2. Call existing `spawn_l2cap_io_tasks(reader, writer, device_id, incoming_tx, bytes_tx, bytes_rx)`.
3. Select loop: drain `outbound_rx` → L2CAP `outbound_tx`, wait on `done_notify`.
4. On I/O death → `PeerCommand::Stalled`.

No `ReliableChannel`, no fragmentation, no `inbound_rx` consumption, no retransmit/truncation counters.

### Pipe signature

```rust
pub async fn run_data_pipe(
    iface: Arc<dyn BleInterface>,
    device_id: blew::DeviceId,
    role: ConnectRole,
    path: ConnectPath,
    l2cap_channel: Option<L2capChannel>,
    outbound_rx: mpsc::Receiver<PendingSend>,
    inbound_rx: mpsc::Receiver<Bytes>,        // unused on L2CAP
    incoming_tx: mpsc::Sender<IncomingPacket>,
    registry_tx: mpsc::Sender<PeerCommand>,
    retransmit_counter: Arc<AtomicU64>,        // unused on L2CAP
    truncation_counter: Arc<AtomicU64>,        // unused on L2CAP
)
```

### Dead code removal

`#![allow(dead_code)]` removed from `l2cap.rs`.

## Part 4 — Snapshot & chat demo

### Transport snapshot

`PeerStateSummary` gains:
```rust
pub connect_path: Option<ConnectPath>,  // Some when Connected, None otherwise
```

`BlePeerInfo` gains the same field, populated from the snapshot. `ConnectPath` re-exported from `lib.rs`.

### Chat backend

`BlePeerDebugUI` gains `path: Option<String>` — `"gatt"` / `"l2cap"` / `null`.

`PeerStateUI` gains `ble_path: Option<String>` for the main peer list.

### Chat frontend

1. BLE peers debug list: append path to phase text — e.g. "connected (l2cap)".
2. Main peer info tooltip: when BLE phase is connected, add a "Path: L2CAP" / "Path: GATT" row.

No new CSS needed.

## Open items to resolve during implementation

1. **Characteristic value update API in blew.** Verify `Peripheral` exposes a way to update a characteristic's value after `add_service`. If not, add a method in blew alpha.6. The peripheral `ReadRequest` responder is already wired — we may be able to serve the PSM from a closure rather than a static value, but need to confirm.
2. **IROH_VERSION_CHAR_UUID response.** Today the VERSION characteristic also has `value: vec![]`. If we are touching the read-request path anyway, consider populating it with the actual protocol version byte (0x01). Not required for this spec — flag as follow-up.
3. **L2CAP MTU.** `l2cap.rs::MAX_DATAGRAM_SIZE` is hardcoded to 1472. Confirm this is safe for L2CAP CoC across all platforms. On Linux, `set_recv_mtu(65535)` is called on the listener side. On Apple and Android, the L2CAP CoC MTU is negotiated at connection time and may be smaller. If any platform negotiates below 1472, we need a per-channel MTU query or to lower the ceiling. Investigation during implementation.
