# L2CAP Revival Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Re-enable the dormant L2CAP data path so peers default to L2CAP CoC, falling back to GATT on failure.

**Architecture:** Upstream blew alpha.6 adds `DeviceId` to the L2CAP accept stream. On the iroh-ble-transport side, the registry drives a blocking path-selection phase after GATT connect (`Handshaking` with a 1.5 s deadline), the pipe branches on `ConnectPath`, and the chat demo surfaces which path is active.

**Tech Stack:** Rust, blew 0.1.0-alpha.6, iroh 0.97, tokio 1, Tauri 2, TypeScript

**Spec:** `docs/superpowers/specs/2026-04-15-l2cap-revival-design.md`

---

## Prerequisite: blew alpha.6

The user will publish this release. The following changes are needed in the blew repo:

1. **`peripheral/backend.rs` + `peripheral/mod.rs`:** Change `l2cap_listener` return type from `Stream<Item = BlewResult<L2capChannel>>` to `Stream<Item = BlewResult<(DeviceId, L2capChannel)>>`.

2. **Apple (`platform/apple/peripheral.rs`):** In `didOpenL2CAPChannel_error`, after the existing `Retained::cast_unchecked(peer)` cast, call `central_device_id(&central)` and send `(device_id, l2cap)` instead of just `l2cap`.

3. **Android (`platform/android/l2cap_state.rs`):** Change `accept_tx` from `Sender<BlewResult<L2capChannel>>` to `Sender<BlewResult<(DeviceId, L2capChannel)>>`. In `on_channel_opened`, when `from_server`, pair `DeviceId(device_addr.to_string())` with the channel.

4. **Linux (`platform/linux/peripheral.rs`):** In the `l2cap_listener` accept loop, keep the `addr` from `listener.accept()` and send `(DeviceId(addr.addr.to_string()), bridge_l2cap(stream))`.

5. **`testing.rs`:** Change `l2cap_accept_tx` to `Option<UnboundedSender<BlewResult<(DeviceId, L2capChannel)>>>`. In `open_l2cap_channel`, send `(device_id.clone(), peripheral_side)`. In `l2cap_listener`, update the channel type.

6. **`examples/l2cap_server.rs`:** Destructure the new tuple.

7. Publish as blew `0.1.0-alpha.6`.

**Tell the user to publish blew alpha.6 before starting Task 1.**

---

## File Map

| File | Action | Responsibility |
|------|--------|---------------|
| `Cargo.toml` (workspace member) | Modify | Bump blew dep to alpha.6 |
| `src/transport/transport.rs` | Modify | `L2capPolicy`, `BleTransportConfig`, `with_config`, l2cap listener setup, PSM state, snapshot |
| `src/transport/peer.rs` | Modify | `PeerEntry.l2cap_channel`, `StartDataPipe.path` |
| `src/transport/registry.rs` | Modify | L2CAP path selection, `InboundL2capChannel` handler, `Handshaking` timeout, snapshot path |
| `src/transport/driver.rs` | Modify | Thread path + channel through `StartDataPipe` |
| `src/transport/pipe.rs` | Modify | L2CAP branch bypassing `ReliableChannel` |
| `src/transport/events.rs` | Modify | PSM serving in `ReadRequest`, L2CAP accept stream pump |
| `src/transport/l2cap.rs` | Modify | Remove `#![allow(dead_code)]` |
| `src/transport/mod.rs` | Modify | Re-exports |
| `src/lib.rs` | Modify | Re-exports |
| `src/transport/test_util.rs` | Modify | MockBleInterface PSM queue |
| `demos/iroh-ble-chat/src-tauri/src/lib.rs` | Modify | `BlePeerDebugUI.path`, `PeerStateUI.ble_path` |
| `demos/iroh-ble-chat/src/main.ts` | Modify | Render path in peer list |

All paths are relative to `crates/iroh-ble-transport/` unless prefixed with `demos/`.

---

### Task 1: Types, config, and peer model extensions

**Files:**
- Modify: `crates/iroh-ble-transport/Cargo.toml`
- Modify: `crates/iroh-ble-transport/src/transport/transport.rs`
- Modify: `crates/iroh-ble-transport/src/transport/peer.rs`
- Modify: `crates/iroh-ble-transport/src/transport/registry.rs`
- Modify: `crates/iroh-ble-transport/src/transport/l2cap.rs`
- Modify: `crates/iroh-ble-transport/src/transport/mod.rs`
- Modify: `crates/iroh-ble-transport/src/lib.rs`

- [ ] **Step 1: Bump blew dep to alpha.6**

In `crates/iroh-ble-transport/Cargo.toml`, change both blew lines:

```toml
blew = "0.1.0-alpha.6"
# and in [dev-dependencies]:
blew = { version = "0.1.0-alpha.6", features = ["testing"] }
```

Run: `cargo check -p iroh-ble-transport 2>&1 | head -20`

This will produce compile errors from the `l2cap_listener` signature change in blew. That's expected — we'll fix those in later tasks.

- [ ] **Step 2: Add L2capPolicy and BleTransportConfig to transport.rs**

Add after the existing constants at the top of `crates/iroh-ble-transport/src/transport/transport.rs`:

```rust
/// Controls whether the transport attempts L2CAP CoC for the data path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum L2capPolicy {
    /// Never attempt L2CAP. GATT-only.
    Disabled,
    /// Try L2CAP first on every new connection, GATT fallback on failure.
    PreferL2cap,
}

impl Default for L2capPolicy {
    fn default() -> Self {
        Self::PreferL2cap
    }
}

/// Configuration for [`BleTransport`].
#[derive(Debug, Clone)]
pub struct BleTransportConfig {
    pub l2cap_policy: L2capPolicy,
}

impl Default for BleTransportConfig {
    fn default() -> Self {
        Self {
            l2cap_policy: L2capPolicy::default(),
        }
    }
}
```

- [ ] **Step 3: Add `l2cap_channel` field to PeerEntry**

In `crates/iroh-ble-transport/src/transport/peer.rs`, add a field to `PeerEntry`:

```rust
pub struct PeerEntry {
    pub device_id: DeviceId,
    pub phase: PeerPhase,
    pub last_adv: Option<Instant>,
    pub last_rx: Option<Instant>,
    pub last_tx: Option<Instant>,
    pub consecutive_failures: u32,
    pub tx_gen: u64,
    pub pending_sends: VecDeque<PendingSend>,
    pub role: ConnectRole,
    pub pipe: Option<PipeHandles>,
    pub rx_backlog: VecDeque<Bytes>,
    pub l2cap_channel: Option<L2capChannel>,
}
```

Update `PeerEntry::new` to initialize `l2cap_channel: None`.

- [ ] **Step 4: Add `l2cap_deadline` to `PeerPhase::Handshaking`**

In `crates/iroh-ble-transport/src/transport/peer.rs`, update the `Handshaking` variant:

```rust
Handshaking {
    since: Instant,
    channel: ChannelHandle,
    l2cap_deadline: Option<Instant>,
}
```

Fix all existing references to `Handshaking` that destructure it — add `l2cap_deadline: _` or `l2cap_deadline` as needed. Check:
- `registry.rs` — `InboundGattFragment` handler (promoting from Handshaking), `CentralDisconnected` handler, `RestoreFromAdapter` handler
- `registry.rs` tests that construct `Handshaking` phases

- [ ] **Step 5: Add `path` to `PeerAction::StartDataPipe`**

In `crates/iroh-ble-transport/src/transport/peer.rs`:

```rust
PeerAction::StartDataPipe {
    device_id: DeviceId,
    role: ConnectRole,
    path: ConnectPath,
}
```

Fix all places that construct or match on `StartDataPipe` — `registry.rs` and `driver.rs`. For now, set `path: ConnectPath::Gatt` everywhere to keep existing behavior.

- [ ] **Step 6: Add `l2cap_policy` to Registry**

In `crates/iroh-ble-transport/src/transport/registry.rs`, add a field:

```rust
pub struct Registry {
    peers: HashMap<DeviceId, PeerEntry>,
    l2cap_policy: L2capPolicy,
}
```

Update `Registry::new` to accept `l2cap_policy: L2capPolicy`:

```rust
pub fn new(l2cap_policy: L2capPolicy) -> Self {
    Self {
        peers: HashMap::new(),
        l2cap_policy,
    }
}
```

Update `new_for_test` to default to `L2capPolicy::Disabled` (so existing tests are unaffected):

```rust
pub fn new_for_test() -> Self {
    Self::new(L2capPolicy::Disabled)
}
```

Update `Default` impl, and the call site in `transport.rs` `BleTransport::new`.

- [ ] **Step 7: Add `connect_path` to `PeerStateSummary`**

In `crates/iroh-ble-transport/src/transport/registry.rs`:

```rust
pub struct PeerStateSummary {
    pub phase_kind: PhaseKind,
    pub tx_gen: u64,
    pub consecutive_failures: u32,
    pub connect_path: Option<ConnectPath>,
}
```

Update `publish_snapshot` to populate `connect_path` from `PeerPhase::Connected { channel, .. } => Some(channel.path)`, `None` for all other phases.

- [ ] **Step 8: Remove dead_code allow from l2cap.rs**

In `crates/iroh-ble-transport/src/transport/l2cap.rs`, remove `#![allow(dead_code)]` at line 17.

- [ ] **Step 9: Update re-exports**

In `crates/iroh-ble-transport/src/transport/mod.rs`, add:

```rust
pub use transport::{BleTransportConfig, L2capPolicy};
```

In `crates/iroh-ble-transport/src/lib.rs`, add `BleTransportConfig`, `L2capPolicy`, and `ConnectPath` to the re-export list.

- [ ] **Step 10: Run check and fix remaining compile errors**

Run: `cargo check -p iroh-ble-transport --all-targets --features testing`

Fix any remaining compile errors from the type changes. The `l2cap_mock.rs` integration test will need its `l2cap_listener` calls updated for the new `(DeviceId, L2capChannel)` tuple from blew alpha.6.

- [ ] **Step 11: Run tests**

Run: `cargo nextest run -p iroh-ble-transport`

All existing tests should pass — we haven't changed behavior, only added fields and types.

- [ ] **Step 12: Commit**

```bash
git add -A crates/iroh-ble-transport/
git commit -m "transport: add L2capPolicy, BleTransportConfig, and peer model extensions for L2CAP revival"
```

---

### Task 2: Registry L2CAP path selection (central side)

**Files:**
- Modify: `crates/iroh-ble-transport/src/transport/registry.rs`

This task changes the `ConnectSucceeded` handler to route through `Handshaking` when L2CAP is preferred, adds handlers for `OpenL2capSucceeded` / `OpenL2capFailed`, and adds a `Tick`-based `Handshaking` timeout.

- [ ] **Step 1: Write test for ConnectSucceeded with PreferL2cap**

Add to registry.rs tests:

```rust
#[test]
fn connect_succeeded_prefer_l2cap_enters_handshaking_and_emits_open_l2cap() {
    use crate::transport::peer::{ChannelHandle, ConnectPath, ConnectRole};

    let mut reg = Registry::new(crate::transport::transport::L2capPolicy::PreferL2cap);
    let device_id = blew::DeviceId::from("dev-l2cap-1");
    reg.peers.insert(device_id.clone(), {
        let mut e = PeerEntry::new(device_id.clone());
        e.role = ConnectRole::Central;
        e.phase = PeerPhase::Connecting {
            attempt: 0,
            started: std::time::Instant::now(),
            path: ConnectPath::Gatt,
        };
        e
    });
    let ch = ChannelHandle {
        id: 42,
        path: ConnectPath::Gatt,
    };
    let actions = reg.handle(PeerCommand::ConnectSucceeded {
        device_id: device_id.clone(),
        channel: ch,
    });
    assert!(matches!(
        reg.peer(&device_id).unwrap().phase,
        PeerPhase::Handshaking {
            l2cap_deadline: Some(_),
            ..
        }
    ));
    assert!(actions
        .iter()
        .any(|a| matches!(a, PeerAction::OpenL2cap { .. })));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p iroh-ble-transport -- connect_succeeded_prefer_l2cap`

Expected: FAIL — `ConnectSucceeded` still goes straight to `Connected`.

- [ ] **Step 3: Implement L2CAP path in ConnectSucceeded handler**

In `registry.rs`, modify the `ConnectSucceeded` arm (around line 231):

```rust
PeerCommand::ConnectSucceeded { device_id, channel } => {
    if let Some(entry) = self.peers.get_mut(&device_id)
        && matches!(entry.phase, PeerPhase::Connecting { .. })
    {
        entry.consecutive_failures = 0;

        if self.l2cap_policy == crate::transport::transport::L2capPolicy::PreferL2cap {
            entry.phase = PeerPhase::Handshaking {
                since: now,
                channel,
                l2cap_deadline: Some(now + L2CAP_SELECT_TIMEOUT),
            };
            actions.push(PeerAction::OpenL2cap {
                device_id: device_id.clone(),
            });
        } else {
            entry.tx_gen += 1;
            let tx_gen = entry.tx_gen;
            entry.phase = PeerPhase::Connected {
                since: now,
                channel,
                tx_gen,
            };
            let role = entry.role;
            actions.push(PeerAction::StartDataPipe {
                device_id: device_id.clone(),
                role,
                path: crate::transport::peer::ConnectPath::Gatt,
            });
        }
    }
}
```

Add the constant near the other timeout constants:

```rust
const L2CAP_SELECT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);
const HANDSHAKING_L2CAP_WALL: std::time::Duration = std::time::Duration::from_millis(2000);
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p iroh-ble-transport -- connect_succeeded_prefer_l2cap`

Expected: PASS.

- [ ] **Step 5: Write test for ConnectSucceeded with Disabled policy (regression)**

```rust
#[test]
fn connect_succeeded_disabled_policy_goes_straight_to_connected() {
    use crate::transport::peer::{ChannelHandle, ConnectPath, ConnectRole};

    let mut reg = Registry::new(crate::transport::transport::L2capPolicy::Disabled);
    let device_id = blew::DeviceId::from("dev-gatt-only");
    reg.peers.insert(device_id.clone(), {
        let mut e = PeerEntry::new(device_id.clone());
        e.role = ConnectRole::Central;
        e.phase = PeerPhase::Connecting {
            attempt: 0,
            started: std::time::Instant::now(),
            path: ConnectPath::Gatt,
        };
        e
    });
    let ch = ChannelHandle {
        id: 1,
        path: ConnectPath::Gatt,
    };
    let actions = reg.handle(PeerCommand::ConnectSucceeded {
        device_id: device_id.clone(),
        channel: ch,
    });
    assert!(matches!(
        reg.peer(&device_id).unwrap().phase,
        PeerPhase::Connected { .. }
    ));
    assert!(actions.iter().any(|a| matches!(
        a,
        PeerAction::StartDataPipe {
            path: ConnectPath::Gatt,
            ..
        }
    )));
}
```

- [ ] **Step 6: Run to verify it passes (should already pass)**

Run: `cargo nextest run -p iroh-ble-transport -- connect_succeeded_disabled_policy`

Expected: PASS.

- [ ] **Step 7: Write test for OpenL2capSucceeded**

```rust
#[test]
fn open_l2cap_succeeded_promotes_handshaking_to_connected_l2cap() {
    use crate::transport::peer::{ChannelHandle, ConnectPath, ConnectRole};

    let mut reg = Registry::new(crate::transport::transport::L2capPolicy::PreferL2cap);
    let device_id = blew::DeviceId::from("dev-l2cap-ok");
    reg.peers.insert(device_id.clone(), {
        let mut e = PeerEntry::new(device_id.clone());
        e.role = ConnectRole::Central;
        e.phase = PeerPhase::Handshaking {
            since: std::time::Instant::now(),
            channel: ChannelHandle {
                id: 1,
                path: ConnectPath::Gatt,
            },
            l2cap_deadline: Some(std::time::Instant::now() + std::time::Duration::from_secs(1)),
        };
        e
    });

    let (ch_a, _ch_b) = blew::L2capChannel::pair(8192);
    let actions = reg.handle(PeerCommand::OpenL2capSucceeded {
        device_id: device_id.clone(),
        channel: ch_a,
    });

    let entry = reg.peer(&device_id).unwrap();
    match &entry.phase {
        PeerPhase::Connected { channel, tx_gen, .. } => {
            assert_eq!(channel.path, ConnectPath::L2cap);
            assert_eq!(*tx_gen, 1);
        }
        other => panic!("expected Connected, got {other:?}"),
    }
    assert!(entry.l2cap_channel.is_some());
    assert!(actions.iter().any(|a| matches!(
        a,
        PeerAction::StartDataPipe {
            path: ConnectPath::L2cap,
            ..
        }
    )));
}
```

- [ ] **Step 8: Run test to verify it fails**

Run: `cargo nextest run -p iroh-ble-transport -- open_l2cap_succeeded_promotes`

Expected: FAIL — no handler for `OpenL2capSucceeded`.

- [ ] **Step 9: Implement OpenL2capSucceeded handler**

In the `handle` method's match block, add (or replace the catch-all match for this variant):

```rust
PeerCommand::OpenL2capSucceeded { device_id, channel } => {
    if let Some(entry) = self.peers.get_mut(&device_id)
        && matches!(entry.phase, PeerPhase::Handshaking { .. })
    {
        let gatt_channel = match &entry.phase {
            PeerPhase::Handshaking { channel, .. } => channel.clone(),
            _ => unreachable!(),
        };
        let _ = gatt_channel;
        entry.tx_gen += 1;
        let tx_gen = entry.tx_gen;
        let l2cap_handle = crate::transport::peer::ChannelHandle {
            id: gatt_channel.id,
            path: ConnectPath::L2cap,
        };
        entry.phase = PeerPhase::Connected {
            since: now,
            channel: l2cap_handle,
            tx_gen,
        };
        entry.l2cap_channel = Some(channel);
        let role = entry.role;
        actions.push(PeerAction::StartDataPipe {
            device_id: device_id.clone(),
            role,
            path: ConnectPath::L2cap,
        });
    }
}
```

- [ ] **Step 10: Run test to verify it passes**

Run: `cargo nextest run -p iroh-ble-transport -- open_l2cap_succeeded_promotes`

Expected: PASS.

- [ ] **Step 11: Write test for OpenL2capFailed (GATT fallback)**

```rust
#[test]
fn open_l2cap_failed_falls_back_to_gatt() {
    use crate::transport::peer::{ChannelHandle, ConnectPath, ConnectRole};

    let mut reg = Registry::new(crate::transport::transport::L2capPolicy::PreferL2cap);
    let device_id = blew::DeviceId::from("dev-l2cap-fail");
    reg.peers.insert(device_id.clone(), {
        let mut e = PeerEntry::new(device_id.clone());
        e.role = ConnectRole::Central;
        e.phase = PeerPhase::Handshaking {
            since: std::time::Instant::now(),
            channel: ChannelHandle {
                id: 1,
                path: ConnectPath::Gatt,
            },
            l2cap_deadline: Some(std::time::Instant::now() + std::time::Duration::from_secs(1)),
        };
        e
    });

    let actions = reg.handle(PeerCommand::OpenL2capFailed {
        device_id: device_id.clone(),
        error: "no psm advertised".into(),
    });

    let entry = reg.peer(&device_id).unwrap();
    match &entry.phase {
        PeerPhase::Connected { channel, .. } => {
            assert_eq!(channel.path, ConnectPath::Gatt);
        }
        other => panic!("expected Connected(Gatt), got {other:?}"),
    }
    assert!(actions.iter().any(|a| matches!(
        a,
        PeerAction::StartDataPipe {
            path: ConnectPath::Gatt,
            ..
        }
    )));
    assert!(actions
        .iter()
        .any(|a| matches!(a, PeerAction::EmitMetric(_))));
}
```

- [ ] **Step 12: Run test to verify it fails**

Run: `cargo nextest run -p iroh-ble-transport -- open_l2cap_failed_falls_back`

Expected: FAIL.

- [ ] **Step 13: Implement OpenL2capFailed handler**

```rust
PeerCommand::OpenL2capFailed { device_id, error } => {
    if let Some(entry) = self.peers.get_mut(&device_id)
        && matches!(entry.phase, PeerPhase::Handshaking { .. })
    {
        let channel = match &entry.phase {
            PeerPhase::Handshaking { channel, .. } => channel.clone(),
            _ => unreachable!(),
        };
        entry.tx_gen += 1;
        let tx_gen = entry.tx_gen;
        entry.phase = PeerPhase::Connected {
            since: now,
            channel,
            tx_gen,
        };
        let role = entry.role;
        actions.push(PeerAction::StartDataPipe {
            device_id: device_id.clone(),
            role,
            path: ConnectPath::Gatt,
        });
        actions.push(PeerAction::EmitMetric(format!(
            "l2cap_fallback_to_gatt:{error}"
        )));
    }
}
```

- [ ] **Step 14: Run test to verify it passes**

Run: `cargo nextest run -p iroh-ble-transport -- open_l2cap_failed_falls_back`

Expected: PASS.

- [ ] **Step 15: Write test for Handshaking L2CAP timeout on Tick**

```rust
#[test]
fn tick_past_handshaking_l2cap_wall_falls_back_to_gatt() {
    use crate::transport::peer::{ChannelHandle, ConnectPath, ConnectRole};

    let mut reg = Registry::new(crate::transport::transport::L2capPolicy::PreferL2cap);
    let device_id = blew::DeviceId::from("dev-l2cap-timeout");
    let old = std::time::Instant::now() - std::time::Duration::from_secs(5);
    reg.peers.insert(device_id.clone(), {
        let mut e = PeerEntry::new(device_id.clone());
        e.role = ConnectRole::Central;
        e.phase = PeerPhase::Handshaking {
            since: old,
            channel: ChannelHandle {
                id: 1,
                path: ConnectPath::Gatt,
            },
            l2cap_deadline: Some(old + std::time::Duration::from_millis(1500)),
        };
        e
    });

    let actions = reg.handle(PeerCommand::Tick(std::time::Instant::now()));

    let entry = reg.peer(&device_id).unwrap();
    match &entry.phase {
        PeerPhase::Connected { channel, .. } => {
            assert_eq!(channel.path, ConnectPath::Gatt);
        }
        other => panic!("expected Connected(Gatt), got {other:?}"),
    }
    assert!(actions.iter().any(|a| matches!(
        a,
        PeerAction::StartDataPipe {
            path: ConnectPath::Gatt,
            ..
        }
    )));
}
```

- [ ] **Step 16: Run test to verify it fails**

Run: `cargo nextest run -p iroh-ble-transport -- tick_past_handshaking_l2cap_wall`

Expected: FAIL.

- [ ] **Step 17: Implement Handshaking timeout in Tick handler**

In the `Tick` handler's decision loop (around line 440), add a new `TickAction` variant and matching logic:

```rust
enum TickAction {
    StartConnect { attempt: u32 },
    DrainingToDead,
    RestoringToDead,
    HandshakingL2capTimeout { channel: ChannelHandle },
}
```

In the loop over peers:

```rust
PeerPhase::Handshaking {
    l2cap_deadline: Some(deadline),
    since,
    channel,
} if tick_now.saturating_duration_since(*since) > HANDSHAKING_L2CAP_WALL => {
    Some(TickAction::HandshakingL2capTimeout {
        channel: channel.clone(),
    })
}
```

In the action execution loop:

```rust
TickAction::HandshakingL2capTimeout { channel } => {
    entry.tx_gen += 1;
    let tx_gen = entry.tx_gen;
    entry.phase = PeerPhase::Connected {
        since: tick_now,
        channel,
        tx_gen,
    };
    let role = entry.role;
    actions.push(PeerAction::StartDataPipe {
        device_id: device_id.clone(),
        role,
        path: crate::transport::peer::ConnectPath::Gatt,
    });
    actions.push(PeerAction::EmitMetric(
        "l2cap_handshaking_timeout".to_string(),
    ));
}
```

- [ ] **Step 18: Run test to verify it passes**

Run: `cargo nextest run -p iroh-ble-transport -- tick_past_handshaking_l2cap_wall`

Expected: PASS.

- [ ] **Step 19: Run full test suite**

Run: `cargo nextest run -p iroh-ble-transport`

All tests should pass.

- [ ] **Step 20: Commit**

```bash
git add crates/iroh-ble-transport/src/transport/registry.rs
git commit -m "registry: L2CAP path selection on ConnectSucceeded with GATT fallback"
```

---

### Task 3: Registry InboundL2capChannel handler

**Files:**
- Modify: `crates/iroh-ble-transport/src/transport/registry.rs`

- [ ] **Step 1: Write test for InboundL2capChannel creating a new peripheral entry**

```rust
#[test]
fn inbound_l2cap_channel_creates_peripheral_entry_with_l2cap_path() {
    use crate::transport::peer::{ConnectPath, ConnectRole};

    let mut reg = Registry::new(crate::transport::transport::L2capPolicy::PreferL2cap);
    let device_id = blew::DeviceId::from("dev-l2cap-inbound");
    let (ch_a, _ch_b) = blew::L2capChannel::pair(8192);

    let actions = reg.handle(PeerCommand::InboundL2capChannel {
        device_id: device_id.clone(),
        channel: ch_a,
    });

    let entry = reg.peer(&device_id).unwrap();
    assert_eq!(entry.role, ConnectRole::Peripheral);
    match &entry.phase {
        PeerPhase::Connected { channel, .. } => {
            assert_eq!(channel.path, ConnectPath::L2cap);
        }
        other => panic!("expected Connected(L2cap), got {other:?}"),
    }
    assert!(entry.l2cap_channel.is_some());
    assert!(actions.iter().any(|a| matches!(
        a,
        PeerAction::StartDataPipe {
            path: ConnectPath::L2cap,
            ..
        }
    )));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p iroh-ble-transport -- inbound_l2cap_channel_creates`

Expected: FAIL — no handler for `InboundL2capChannel`.

- [ ] **Step 3: Implement InboundL2capChannel handler**

In the `handle` method, replace the catch-all that handles `InboundL2capChannel` (currently falls to `_ => {}`):

```rust
PeerCommand::InboundL2capChannel { device_id, channel } => {
    use std::collections::hash_map::Entry;
    match self.peers.entry(device_id.clone()) {
        Entry::Vacant(v) => {
            let mut e = PeerEntry::new(device_id.clone());
            e.role = crate::transport::peer::ConnectRole::Peripheral;
            e.tx_gen = 1;
            let l2cap_handle = crate::transport::peer::ChannelHandle {
                id: 0,
                path: ConnectPath::L2cap,
            };
            e.phase = PeerPhase::Connected {
                since: now,
                channel: l2cap_handle,
                tx_gen: 1,
            };
            e.l2cap_channel = Some(channel);
            v.insert(e);
            actions.push(PeerAction::StartDataPipe {
                device_id: device_id.clone(),
                role: crate::transport::peer::ConnectRole::Peripheral,
                path: ConnectPath::L2cap,
            });
        }
        Entry::Occupied(mut o) => {
            let entry = o.get_mut();
            let has_pipe = entry.pipe.is_some();
            if has_pipe {
                let metric = match &entry.phase {
                    PeerPhase::Connected { channel, .. }
                        if channel.path == ConnectPath::L2cap =>
                    {
                        "l2cap_duplicate_accept"
                    }
                    _ => "l2cap_late_accept_after_gatt",
                };
                actions.push(PeerAction::EmitMetric(metric.to_string()));
            } else {
                entry.role = crate::transport::peer::ConnectRole::Peripheral;
                entry.tx_gen += 1;
                let tx_gen = entry.tx_gen;
                let l2cap_handle = crate::transport::peer::ChannelHandle {
                    id: 0,
                    path: ConnectPath::L2cap,
                };
                entry.phase = PeerPhase::Connected {
                    since: now,
                    channel: l2cap_handle,
                    tx_gen,
                };
                entry.l2cap_channel = Some(channel);
                actions.push(PeerAction::StartDataPipe {
                    device_id: device_id.clone(),
                    role: crate::transport::peer::ConnectRole::Peripheral,
                    path: ConnectPath::L2cap,
                });
            }
        }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p iroh-ble-transport -- inbound_l2cap_channel_creates`

Expected: PASS.

- [ ] **Step 5: Write test for InboundL2capChannel dropped when GATT pipe exists**

```rust
#[test]
fn inbound_l2cap_channel_dropped_when_pipe_exists() {
    use crate::transport::peer::{ChannelHandle, ConnectPath, PipeHandles};

    let mut reg = Registry::new(crate::transport::transport::L2capPolicy::PreferL2cap);
    let device_id = blew::DeviceId::from("dev-l2cap-late");

    let (outbound_tx, _outbound_rx) =
        tokio::sync::mpsc::channel::<crate::transport::peer::PendingSend>(4);
    let (inbound_tx, _inbound_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(4);

    reg.peers.insert(device_id.clone(), {
        let mut e = PeerEntry::new(device_id.clone());
        e.phase = PeerPhase::Connected {
            since: std::time::Instant::now(),
            channel: ChannelHandle {
                id: 1,
                path: ConnectPath::Gatt,
            },
            tx_gen: 1,
        };
        e.pipe = Some(PipeHandles {
            outbound_tx,
            inbound_tx,
        });
        e
    });

    let (ch_a, _ch_b) = blew::L2capChannel::pair(8192);
    let actions = reg.handle(PeerCommand::InboundL2capChannel {
        device_id: device_id.clone(),
        channel: ch_a,
    });

    assert!(actions
        .iter()
        .any(|a| matches!(a, PeerAction::EmitMetric(s) if s.contains("l2cap_late_accept_after_gatt"))));
    assert!(matches!(
        reg.peer(&device_id).unwrap().phase,
        PeerPhase::Connected {
            channel: ChannelHandle {
                path: ConnectPath::Gatt,
                ..
            },
            ..
        }
    ));
}
```

- [ ] **Step 6: Run to verify it passes (should already pass)**

Run: `cargo nextest run -p iroh-ble-transport -- inbound_l2cap_channel_dropped`

Expected: PASS.

- [ ] **Step 7: Run full test suite**

Run: `cargo nextest run -p iroh-ble-transport`

- [ ] **Step 8: Commit**

```bash
git add crates/iroh-ble-transport/src/transport/registry.rs
git commit -m "registry: handle InboundL2capChannel for peripheral-side L2CAP accept"
```

---

### Task 4: Pipe L2CAP data path

**Files:**
- Modify: `crates/iroh-ble-transport/src/transport/pipe.rs`

- [ ] **Step 1: Add path and l2cap_channel parameters to run_data_pipe**

Update the signature of `run_data_pipe`:

```rust
#[allow(clippy::too_many_arguments)]
pub async fn run_data_pipe(
    iface: Arc<dyn BleInterface>,
    device_id: blew::DeviceId,
    role: ConnectRole,
    path: crate::transport::peer::ConnectPath,
    l2cap_channel: Option<blew::L2capChannel>,
    mut outbound_rx: mpsc::Receiver<PendingSend>,
    mut inbound_rx: mpsc::Receiver<Bytes>,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    registry_tx: mpsc::Sender<PeerCommand>,
    retransmit_counter: Arc<AtomicU64>,
    truncation_counter: Arc<AtomicU64>,
)
```

- [ ] **Step 2: Implement the L2CAP branch**

Restructure `run_data_pipe` to branch early on path. Wrap the existing body in the GATT branch:

```rust
pub async fn run_data_pipe(
    iface: Arc<dyn BleInterface>,
    device_id: blew::DeviceId,
    role: ConnectRole,
    path: crate::transport::peer::ConnectPath,
    l2cap_channel: Option<blew::L2capChannel>,
    mut outbound_rx: mpsc::Receiver<PendingSend>,
    mut inbound_rx: mpsc::Receiver<Bytes>,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    registry_tx: mpsc::Sender<PeerCommand>,
    retransmit_counter: Arc<AtomicU64>,
    truncation_counter: Arc<AtomicU64>,
) {
    match path {
        crate::transport::peer::ConnectPath::L2cap => {
            run_l2cap_pipe(
                device_id,
                l2cap_channel.expect("L2CAP path requires a channel"),
                outbound_rx,
                incoming_tx,
                registry_tx,
            )
            .await;
        }
        crate::transport::peer::ConnectPath::Gatt => {
            run_gatt_pipe(
                iface,
                device_id,
                role,
                outbound_rx,
                inbound_rx,
                incoming_tx,
                registry_tx,
                retransmit_counter,
                truncation_counter,
            )
            .await;
        }
    }
}
```

Extract the existing body into `run_gatt_pipe` (same signature minus `path`/`l2cap_channel`).

- [ ] **Step 3: Implement run_l2cap_pipe**

```rust
use crate::transport::l2cap::spawn_l2cap_io_tasks;

async fn run_l2cap_pipe(
    device_id: blew::DeviceId,
    channel: blew::L2capChannel,
    mut outbound_rx: mpsc::Receiver<PendingSend>,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    registry_tx: mpsc::Sender<PeerCommand>,
) {
    let (reader, writer) = tokio::io::split(channel);
    let tx_bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let rx_bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let (l2cap_tx, _send_task, _recv_task, done) = spawn_l2cap_io_tasks(
        reader,
        writer,
        device_id.clone(),
        incoming_tx,
        tx_bytes,
        rx_bytes,
    );

    loop {
        tokio::select! {
            maybe_send = outbound_rx.recv() => {
                match maybe_send {
                    Some(send) => {
                        let _ = l2cap_tx.send(send.datagram.to_vec()).await;
                        send.waker.wake();
                    }
                    None => break,
                }
            }
            _ = done.notified() => {
                break;
            }
        }
    }

    let _ = registry_tx
        .send(PeerCommand::Stalled {
            device_id: device_id.clone(),
        })
        .await;
}
```

- [ ] **Step 4: Fix driver.rs call site**

In `driver.rs`, update the `StartDataPipe` arm to pass the new parameters:

```rust
PeerAction::StartDataPipe { device_id, role, path } => {
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

    // Consume L2CAP channel from registry via a command.
    // The channel is passed through via PeerCommand::DataPipeReady.
    // For now, we need to get the channel from somewhere.
    // The driver doesn't have direct access to PeerEntry — the channel
    // needs to be threaded through the action.
```

Wait — the driver doesn't have access to `PeerEntry.l2cap_channel`. We need to add it to the action. Update `PeerAction::StartDataPipe`:

```rust
PeerAction::StartDataPipe {
    device_id: DeviceId,
    role: ConnectRole,
    path: ConnectPath,
    l2cap_channel: Option<L2capChannel>,
}
```

In `peer.rs`, add the field. In `registry.rs`, wherever `StartDataPipe` is emitted, take the `l2cap_channel` from the entry:

```rust
let l2cap_channel = entry.l2cap_channel.take();
actions.push(PeerAction::StartDataPipe {
    device_id: device_id.clone(),
    role,
    path: ConnectPath::L2cap,
    l2cap_channel,
});
```

For GATT paths, pass `l2cap_channel: None`.

Then in `driver.rs`:

```rust
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
    let iface: Arc<dyn BleInterface> =
        Arc::clone(&self.iface) as Arc<dyn BleInterface>;
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
```

- [ ] **Step 5: Add timeout to driver OpenL2cap arm**

The spec requires the `OpenL2cap` driver task to be wrapped in `tokio::time::timeout(L2CAP_SELECT_TIMEOUT)`. The existing arm in `driver.rs` (around line 75) has no timeout. Wrap the spawned task body:

```rust
PeerAction::OpenL2cap { device_id } => {
    let iface = Arc::clone(&self.iface);
    let inbox = self.inbox.clone();
    let dev_for_msg = device_id.clone();
    tokio::spawn(async move {
        const L2CAP_SELECT_TIMEOUT: std::time::Duration =
            std::time::Duration::from_millis(1500);
        let result = tokio::time::timeout(L2CAP_SELECT_TIMEOUT, async {
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
```

- [ ] **Step 6: Run check**

Run: `cargo check -p iroh-ble-transport --all-targets --features testing`

Fix any remaining compile errors.

- [ ] **Step 7: Run tests**

Run: `cargo nextest run -p iroh-ble-transport`

All tests should pass.

- [ ] **Step 8: Commit**

```bash
git add crates/iroh-ble-transport/src/transport/pipe.rs crates/iroh-ble-transport/src/transport/driver.rs crates/iroh-ble-transport/src/transport/peer.rs crates/iroh-ble-transport/src/transport/registry.rs
git commit -m "pipe: L2CAP data path bypassing ReliableChannel"
```

---

### Task 5: Transport startup — L2CAP listener, PSM serving, accept pump

**Files:**
- Modify: `crates/iroh-ble-transport/src/transport/transport.rs`
- Modify: `crates/iroh-ble-transport/src/transport/events.rs`

- [ ] **Step 1: Add PSM state to events.rs peripheral pump**

Update `run_peripheral_events` to accept a PSM value and serve it on `ReadRequest`:

```rust
pub async fn run_peripheral_events(
    peripheral: Arc<Peripheral>,
    _routing: Arc<TransportRouting>,
    inbox: mpsc::Sender<PeerCommand>,
    psm: Option<u16>,
) {
    use tokio_stream::StreamExt as _;
    let mut events = peripheral.events();
    while let Some(ev) = events.next().await {
        let cmd = match ev {
            // ... existing arms unchanged ...
            PeripheralEvent::ReadRequest {
                char_uuid,
                responder,
                ..
            } => {
                if char_uuid == crate::transport::transport::IROH_PSM_CHAR_UUID {
                    if let Some(psm_val) = psm {
                        responder.respond(psm_val.to_le_bytes().to_vec());
                    } else {
                        responder.respond(Vec::new());
                    }
                } else {
                    responder.respond(Vec::new());
                }
                continue;
            }
            // ... rest unchanged ...
        };
        if inbox.send(cmd).await.is_err() {
            break;
        }
    }
}
```

Make `IROH_PSM_CHAR_UUID` `pub(crate)` in `transport.rs` so `events.rs` can reference it.

- [ ] **Step 2: Add L2CAP accept pump function in events.rs**

```rust
pub async fn run_l2cap_accept(
    mut listener: impl futures_core::Stream<Item = blew::error::BlewResult<(blew::DeviceId, blew::L2capChannel)>>
        + Send
        + Unpin
        + 'static,
    inbox: mpsc::Sender<PeerCommand>,
) {
    use tokio_stream::StreamExt as _;
    while let Some(result) = listener.next().await {
        match result {
            Ok((device_id, channel)) => {
                tracing::debug!(device = %device_id, "L2CAP accept: incoming channel");
                let cmd = PeerCommand::InboundL2capChannel { device_id, channel };
                if inbox.send(cmd).await.is_err() {
                    break;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "L2CAP accept error");
                break;
            }
        }
    }
}
```

- [ ] **Step 3: Wire L2CAP listener into BleTransport::new**

Add a `with_config` constructor to `BleTransport` (keep existing `new` calling it with defaults):

```rust
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
```

Inside `with_config`, after GATT registration and before spawning the registry, add:

```rust
    let psm = if config.l2cap_policy == L2capPolicy::PreferL2cap {
        match peripheral.l2cap_listener().await {
            Ok((psm, listener)) => {
                info!(psm = psm.0, "L2CAP listener started");
                tokio::spawn(run_l2cap_accept(
                    listener,
                    inbox_tx.clone(),
                ));
                Some(psm.0)
            }
            Err(e) => {
                warn!(error = %e, "L2CAP listener failed, L2CAP accept disabled");
                None
            }
        }
    } else {
        None
    };
```

Pass `config.l2cap_policy` to `Registry::new(config.l2cap_policy)`.

Pass `psm` to the peripheral events pump:

```rust
    tokio::spawn(run_peripheral_events(
        Arc::clone(&peripheral),
        Arc::clone(&routing),
        inbox_tx.clone(),
        psm,
    ));
```

- [ ] **Step 4: Run check**

Run: `cargo check -p iroh-ble-transport --all-targets --features testing`

Fix compile errors. The `run_l2cap_accept` import needs adding to `transport.rs`.

- [ ] **Step 5: Run tests**

Run: `cargo nextest run -p iroh-ble-transport`

- [ ] **Step 6: Commit**

```bash
git add crates/iroh-ble-transport/src/transport/transport.rs crates/iroh-ble-transport/src/transport/events.rs
git commit -m "transport: wire L2CAP listener startup, PSM serving, and accept pump"
```

---

### Task 6: MockBleInterface PSM support and test fixes

**Files:**
- Modify: `crates/iroh-ble-transport/src/transport/test_util.rs`
- Modify: `crates/iroh-ble-transport/tests/l2cap_mock.rs`

- [ ] **Step 1: Add PSM response queue to MockBleInterface**

In `test_util.rs`, add to `Inner`:

```rust
psm_responses: VecDeque<Option<u16>>,
```

Initialize to empty in `MockBleInterface::new`.

Add a builder method:

```rust
pub fn seed_psm(&self, psm: Option<u16>) {
    self.inner.lock().unwrap().psm_responses.push_back(psm);
}
```

Update the `read_psm` impl to pop from the queue:

```rust
async fn read_psm(&self, device_id: &DeviceId) -> BleResult<Option<u16>> {
    let mut inner = self.inner.lock().unwrap();
    inner.calls.push(CallKind::ReadPsm(device_id.clone()));
    Ok(inner.psm_responses.pop_front().flatten())
}
```

Add `ReadPsm(DeviceId)` to the `CallKind` enum.

- [ ] **Step 2: Fix l2cap_mock.rs for blew alpha.6 tuple change**

Update the integration tests in `tests/l2cap_mock.rs` to handle the new `(DeviceId, L2capChannel)` tuple from `l2cap_listener`:

```rust
// Before:
let (psm, mut listener) = periph_ep.peripheral.l2cap_listener().await.unwrap();
// After:
let (psm, mut listener) = periph_ep.peripheral.l2cap_listener().await.unwrap();
// Stream items are now (DeviceId, L2capChannel) — update any destructuring
```

- [ ] **Step 3: Run all tests**

Run: `cargo nextest run -p iroh-ble-transport`

- [ ] **Step 4: Commit**

```bash
git add crates/iroh-ble-transport/src/transport/test_util.rs crates/iroh-ble-transport/tests/l2cap_mock.rs
git commit -m "test: MockBleInterface PSM queue, fix l2cap_mock for blew alpha.6"
```

---

### Task 7: Snapshot ConnectPath plumbing

**Files:**
- Modify: `crates/iroh-ble-transport/src/transport/transport.rs`

- [ ] **Step 1: Add connect_path to BlePeerInfo**

```rust
pub struct BlePeerInfo {
    pub device_id: blew::DeviceId,
    pub phase: BlePeerPhase,
    pub consecutive_failures: u32,
    pub connect_path: Option<ConnectPath>,
}
```

Update `snapshot_peers`:

```rust
pub fn snapshot_peers(&self) -> Vec<BlePeerInfo> {
    let snap = self.handle.snapshots.load();
    snap.peer_states
        .iter()
        .map(|(device_id, state)| BlePeerInfo {
            device_id: device_id.clone(),
            phase: BlePeerPhase::from(state.phase_kind),
            consecutive_failures: state.consecutive_failures,
            connect_path: state.connect_path,
        })
        .collect()
}
```

- [ ] **Step 2: Run tests**

Run: `cargo nextest run -p iroh-ble-transport`

- [ ] **Step 3: Commit**

```bash
git add crates/iroh-ble-transport/src/transport/transport.rs
git commit -m "transport: surface ConnectPath in BlePeerInfo snapshot"
```

---

### Task 8: Chat demo backend

**Files:**
- Modify: `demos/iroh-ble-chat/src-tauri/src/lib.rs`

- [ ] **Step 1: Add path field to BlePeerDebugUI**

```rust
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
            path: info.connect_path.map(|p| match p {
                iroh_ble_transport::ConnectPath::Gatt => "gatt",
                iroh_ble_transport::ConnectPath::L2cap => "l2cap",
            }.to_string()),
        }
    }
}
```

- [ ] **Step 2: Add ble_path to PeerStateUI**

```rust
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
```

Update `PeerState` to track `ble_path: Option<String>` and populate it from `BlePeerInfo::connect_path` in the `transport_state_tick` integration point. Update `PeerState::to_ui` to pass `ble_path` through.

- [ ] **Step 3: Run check**

Run: `cargo check -p iroh-ble-chat`

Fix compile errors.

- [ ] **Step 4: Commit**

```bash
git add demos/iroh-ble-chat/src-tauri/src/lib.rs
git commit -m "chat: surface L2CAP/GATT path in peer state UI"
```

---

### Task 9: Chat demo frontend

**Files:**
- Modify: `demos/iroh-ble-chat/src/main.ts`

- [ ] **Step 1: Update BLE peer types**

Update the TypeScript types to include `path`:

```typescript
// In the BlePeerDebugUI-equivalent type (around line 42):
interface BlePeerUpdate {
  device_id: string;
  phase: string;
  consecutive_failures: number;
  path: string | null;
}
```

And the peer state type:

```typescript
// Add to the peer state interface (around line 35):
ble_path: string | null;
```

- [ ] **Step 2: Update BLE peer list rendering**

In the BLE peers debug list rendering (around line 471), append the path:

```typescript
const pathLabel = peer.path ? ` (${peer.path})` : "";
row.innerHTML = `
  <div class="ble-peer-id" title="${escapeHtml(peer.device_id)}">${escapeHtml(peer.device_id)}</div>
  <div class="ble-peer-phase">${escapeHtml(peer.phase)}${escapeHtml(pathLabel)}${escapeHtml(failureBadge)}</div>
`;
```

- [ ] **Step 3: Update main peer info tooltip**

In the peer detail rows (around line 406), after the BLE phase row, add a path row:

```typescript
if (peer.ble_phase) {
  const phase = BLE_PHASE_LABEL[peer.ble_phase] ?? peer.ble_phase;
  rows.push(["BLE", phase]);
  if (peer.ble_path) {
    rows.push(["Path", peer.ble_path.toUpperCase()]);
  }
}
```

- [ ] **Step 4: Initialize ble_path in peer creation**

Where peers are created (around line 683), add:

```typescript
ble_path: null,
```

- [ ] **Step 5: Update ble-peer-updated event handler**

In the `ble-peer-updated` listener (around line 1136), store the path on the matching peer entry if applicable.

- [ ] **Step 6: Commit**

```bash
git add demos/iroh-ble-chat/src/main.ts
git commit -m "chat-frontend: show L2CAP/GATT path in peer list"
```

---

### Task 10: Final integration check

- [ ] **Step 1: Run full workspace check**

Run: `cargo check --workspace`

- [ ] **Step 2: Run full test suite**

Run: `cargo nextest run --workspace`

- [ ] **Step 3: Run clippy**

Run: `mise run lint`

Fix any warnings.

- [ ] **Step 4: Run fmt**

Run: `mise run fmt`

- [ ] **Step 5: Commit any fixes**

If clippy/fmt produced changes:

```bash
git add -A
git commit -m "chore: clippy + fmt fixes for L2CAP revival"
```
