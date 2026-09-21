# iroh-ble-transport — Project Guide for Claude

## What this is

A Rust crate providing BLE (Bluetooth Low Energy) as a custom transport for the [iroh](https://github.com/n0-computer/iroh) peer-to-peer networking stack. Every node acts as **both** central (scanner/client) and peripheral (advertiser/server) simultaneously so peers can discover and dial each other symmetrically.

Built on [blew](https://github.com/mcginty/blew) for cross-platform BLE. Wraps a state-machine "registry" that owns per-peer lifecycle, an action "driver" that talks to blew, and two data paths: **L2CAP CoC** (preferred — reliable stream, no fragmentation needed) and **GATT** (fallback — Selective-Repeat ARQ fragments QUIC datagrams across characteristic writes/notifications).

## Toolchain

- Minimum supported Rust version: **1.95** (`rust-version = "1.95"` is enforced in the workspace manifests).

## Commands

Uses [mise](https://mise.jdx.dev) for task management. `mise tasks` lists everything.

```sh
mise run build           # cargo build --workspace --all-targets --all-features
mise run check           # cargo check --workspace --all-targets --all-features (no codegen)
mise run test            # cargo nextest run --workspace --all-features
mise run lint            # cargo clippy --workspace --all-targets --all-features (depends on fmt:check)
mise run fmt             # cargo fmt --all
mise run fmt:check       # cargo fmt --all -- --check
mise run verify          # fmt + lint + test (pre-commit gate)
mise run deny            # license/vulnerability audit
mise run ci:check-ios    # cargo check --all-targets --all-features for aarch64-apple-ios
mise run ci:check-android # cargo ndk check --all-targets --all-features for arm64-v8a

# Chat demo
mise run chat:dev                # desktop dev server
mise run chat:dev:ios            # iOS device dev server
mise run chat:dev:android        # Android device dev server
mise run chat:deploy:android:all # build APK + install/launch on every connected adb device
mise run chat:deploy:ios:all     # build + install/launch on every connected iPhone
mise run chat:deploy:mobile:all  # both at once
mise run chat:logs:android       # tail logcat from all connected android devices
mise run chat:uninstall:mobile:all

# Examples
cargo run --example iroh_ble -p iroh-ble-transport            # echo + 48 KB speed test (listener)
cargo run --example iroh_ble -p iroh-ble-transport -- <id>    # echo + 48 KB speed test (dialer)
cargo run --example central -p iroh-ble-transport             # bare central (scan + GATT client)
cargo run --example peripheral -p iroh-ble-transport          # bare peripheral (advertise + serve)
```

`cargo nextest run --workspace` runs all tests; the `iroh-ble-transport` crate has a `testing` feature that exposes `transport::test_util` for in-process mock-driver integration tests.

## Style

- No comments unless the logic is non-obvious. Don't add doc comments to code you didn't change.
- Don't add features, refactor, or "improve" code beyond what was asked.
- Clippy pedantic is enabled (`pedantic = "warn"` in blew). Fix warnings, don't suppress them unless there's a good reason.
- Test with nextest: `cargo nextest run --workspace`.
- **Commit attribution**: when an AI assistant helps write a commit, use an `Assisted-by:` trailer instead of `Co-Authored-by:`, following the [Linux kernel guidance](https://docs.kernel.org/process/ai.html). Format like: `Assisted-by: crush:kimi-k2.6` or `Assisted-by: Claude:claude-opus-4-7` or `Assisted-by: Codex:gpt-5-5`.
- **Commit message style**:
  - Title: `crate: concise summary` with comma-separated clauses (no "and").
  - Body: bullet points with `-` for each logical change.
  - No `💘 Generated with Crush` or similar AI marker lines in commit messages.

## Privacy / filesystem boundary

- Do not inspect, search, or read files outside this repository unless the user explicitly asks for a specific path.
- Do not search the user's home directory, package caches, global tool installs, shell history, config directories, or credential stores while debugging project issues.
- If external tool implementation details are needed, use project-local files, checked-in documentation, or ask the user before looking elsewhere.

## Key dependencies

| Crate | Role |
|-------|------|
| `blew` (git: mcginty/blew) | Cross-platform BLE — central, peripheral, GATT, L2CAP CoC |
| `iroh 1.0` (`unstable-custom-transports`) | QUIC endpoint, custom transport trait |
| `iroh-base 1.0` | `EndpointId`, `CustomAddr`, `TransportAddr` |
| `noq-udp 1.1` | `RecvMeta` — unsealed public dep of iroh's `CustomEndpoint::poll_recv`; must track iroh's own requirement |
| `tokio 1` | Async runtime |
| `arc-swap` | Lock-free snapshot publication of registry state |
| `parking_lot` | Sync mutexes inside the registry/routing tables |

## Module structure

```
crates/iroh-ble-transport/src/
├── lib.rs                   # Re-exports from blew + transport
├── error.rs                 # BleError, BleResult, From<BlewError>
└── transport/
    ├── mod.rs               # Module tree + re-exports
    ├── transport.rs         # BleTransport: iroh CustomTransport entry point
    ├── interface.rs         # BleInterface trait — blew abstraction the driver consumes
    ├── driver.rs            # Action executor: PeerAction → BleInterface calls → PeerCommand follow-ups; per-device lanes
    ├── registry.rs          # Pure state machine (PeerEntry/PeerPhase) + actor loop
    ├── peer.rs              # PeerEntry, PeerPhase, PeerCommand, PeerAction, ChannelHandle
    ├── routing.rs           # TransportRouting: Token ↔ peer-identity, KeyPrefix → DeviceId
    ├── conns.rs             # Live iroh connections indexed by pipe; closed when their pipe dies
    ├── events.rs            # blew event pumps (run_central_events, run_peripheral_events)
    ├── pipe.rs              # Per-peer data pipe: ReliableChannel ↔ inbox/outbox channels
    ├── reliable.rs          # Selective-Repeat ARQ sliding-window protocol + fragment canary
    ├── mtu.rs               # MTU resolution (resolve_chunk_size, MIN_SANE_MTU floor)
    ├── l2cap.rs             # L2CAP framing + I/O task spawning (used by pipe.rs for L2CAP path)
    ├── store.rs             # PeerStore trait + InMemoryPeerStore
    ├── watchdog.rs          # 250 ms PeerCommand::Tick pump
    └── test_util.rs         # `testing` feature: in-process mock BleInterface
```

`iroh-ble-transport` re-exports useful blew types (`Central`, `Peripheral`, `DeviceId`, `CentralEvent`, `PeripheralEvent`, etc.) from `lib.rs` so downstream consumers don't depend on blew directly.

## Transport architecture

`BleTransport` implements iroh's `CustomTransport` trait, tunnelling QUIC datagrams over BLE. It prefers L2CAP CoC when available and falls back to GATT fragmentation.

### Discovery

Each node advertises a GATT service whose UUID encodes the first 12 bytes of its 32-byte Ed25519 public key:

```
69726f00-XXXX-XXXX-XXXX-XXXXXXXXXXXX        (KeyPrefix UUID, used in advertising)
```

Peers identify each other from advertising packets without connecting. The 12-byte `KeyPrefix` is matched against a `discovered: HashMap<KeyPrefix, DeviceId>` map maintained by `TransportRouting`.

### Addressing (token-based)

The transport hands iroh **8-byte opaque tokens** as `CustomAddr` payloads, *not* `DeviceId` strings. `TransportRouting` mints tokens in two flavours:

- **Prefix-keyed** (preferred): minted from the 12-byte `KeyPrefix`. `TokenOrigin::Prefix(prefix)` → `discovered[prefix]` is consulted **live** at every send, so a peer rotating its MAC (e.g. Android randomization on app restart) is followed transparently — iroh's cached address keeps working.
- **Device-keyed** (fallback): minted from a `DeviceId` when an inbound peripheral connection arrives before scan has noted the advertisement. Stable only for the lifetime of that `DeviceId`.

`BleAddressLookup::resolve(EndpointId)` waits until the prefix shows up in `discovered`, then returns `CustomAddr(prefix_token)`. Token `0` is reserved as a `local_addr` sentinel.

### GATT service layout

```
Service: 69726f01-8e45-4c2c-b3a5-331f3098b5c2  (IROH_SERVICE_UUID)
├── 69726f02 — C2P    (WRITE_WITHOUT_RESPONSE | NOTIFY): central→peripheral data + ACKs
├── 69726f03 — P2C    (WRITE_WITHOUT_RESPONSE | NOTIFY): peripheral→central data + ACKs
├── 69726f04 — PSM    (READ): 2-byte LE L2CAP PSM, present only when the peripheral's `l2cap_listener()` succeeded
└── 69726f05 — VERSION (READ): 1-byte protocol version, read by central after connect to verify wire compatibility
```

- Central writes to C2P; peripheral sends data via P2C notifications.
- Both characteristics carry data **and** acknowledgements (piggybacked or pure ACK).

### Connection handshake (GATT path)

1. Central connects (registry transitions `Discovered → Connecting → Handshaking → Connected`).
2. Driver opens GATT: subscribes to P2C, optionally reads PSM/VERSION.
3. Registry emits `StartDataPipe` → `pipe.rs` spins up a `ReliableChannel` and the per-peer outbound/inbound mpsc channels.
4. Both sides are ready for QUIC datagrams. **No identity exchange on the data path** — QUIC's connection IDs handle endpoint identity.

### Connection handshake (L2CAP path — preferred)

When `L2capPolicy::PreferL2cap` (the default), the central side attempts L2CAP after GATT connect succeeds:

1. Central connects via GATT (same as above: `Discovered → Connecting → Handshaking`).
2. In `Handshaking`, the registry sets `l2cap_deadline` and emits `OpenL2cap`.
3. Driver reads the PSM characteristic, then calls `blew::Central::open_l2cap_channel`. The entire sequence is wrapped in a 1.5 s timeout (`L2CAP_SELECT_TIMEOUT`).
4. On success → `OpenL2capSucceeded` → registry transitions to `Connected { path: L2cap }`, emits `StartDataPipe` with the L2CAP channel. `pipe.rs` runs `run_l2cap_pipe` which uses `spawn_l2cap_io_tasks` (length-prefixed `[u16 LE][payload]` framing, no `ReliableChannel`).
5. On failure/timeout → `OpenL2capFailed` → falls back to `Connected { path: Gatt }`, uses the GATT pipe with `ReliableChannel` as before.

The **peripheral side** runs `run_l2cap_accept`, draining the `blew::Peripheral::l2cap_listener` stream. Each accepted `(DeviceId, L2capChannel)` is sent as `PeerCommand::InboundL2capChannel`. The registry matches it to the peer by `DeviceId` and, if no data pipe is running yet, transitions to `Connected { path: L2cap }`.

The PSM characteristic (`69726f04`) is served dynamically via `PeripheralEvent::ReadRequest` in `run_peripheral_events` — the value comes from the L2CAP listener's assigned PSM.

`L2capPolicy::Disabled` skips the L2CAP attempt entirely and goes straight to GATT.

Key constants (registry.rs):

| Constant | Value | Purpose |
|----------|-------|---------|
| `L2CAP_SELECT_TIMEOUT` | 1.5 s | Max time for the PSM-read + L2CAP-open sequence; also the deadline stored on `Handshaking` and checked by the tick safety-net |

### Per-peer state machine

`registry.rs` is pure data — no I/O, no tokio. The actor loop in `Registry::run` reads `PeerCommand`s off an mpsc inbox, calls `Registry::handle` (synchronous), and dispatches the resulting `PeerAction`s to `Driver::execute`.

`PeerEntry` carries a `PeerPhase`:

```
Unknown → Discovered → Connecting → Handshaking → Connected
             ▲                                        │
             │                                        ▼
             └────────────────────────────────── Draining ──→ Dead
                                                      │          │
                                                      ▼          ▼
                                                Reconnecting  Discovered
                                                      │       (on advertisement)
                                                      ▼
                                                  Connecting
                                                      │
                                                      ▼
                                                   Restoring
```

Two edges keep a present peer out of the `Dead` tombstone:

- **`Draining → Discovered`** when the teardown was our own decision. The
  `ConnectionClosed` hook raises `PeerCommand::Stalled { cause: LocalClose }`, which
  sets `PeerEntry::resume_after_drain`; the drain then resolves back to `Discovered`
  instead of `Dead`. It resolves on the peer's disconnect callback (~2 s) with the
  `DRAINING_TIMEOUT` tick as a backstop for the peripheral role, and sends arriving
  during the window are buffered rather than rejected. A `LinkDead` stall (a wedged
  pipe, a dedup loser) still tombstones as before.
- **`Dead → Discovered`** when the peer advertises again. An advertisement is direct
  evidence the peer is present, so it outranks a `MaxRetries` or `Drained` tombstone
  rather than waiting out `DEAD_GC_TTL`. `ProtocolMismatch` and `Forgotten`
  tombstones are not resurrected — the first is a property of the peer's build, the
  second an explicit application decision.

Key constants (registry.rs):

| Constant | Value | Purpose |
|----------|-------|---------|
| `MAX_CONNECT_ATTEMPTS` | 15 | Retries before transitioning to `Dead { MaxRetries }` |
| `DRAINING_TIMEOUT` | 5 s | How long to wait in `Draining` for a clean teardown |
| `RESTORING_TIMEOUT` | 120 s | Max time after adapter-restore before giving up |
| `DEAD_GC_TTL` | 60 s | How long a `Dead` entry sticks around for dedup |

The transport itself is **passive**: it never dials on its own. Both resume edges above land in `Discovered`, which only dials when a datagram is already queued for that peer — reconnect policy stays in the application (see chat-app `reconnect_tick`).

### Lifecycle identity

Every asynchronous thing the driver does for a peer — connect, VERSION read,
L2CAP open, disconnect, refresh — completes long after the registry has moved
on, and reports back by `DeviceId`, which a peer keeps across teardown and
redial. `PeerEntry::lifecycle_id` is the identity that tells those apart:

- Allocated from a **registry-global counter** (`Registry::next_lifecycle`), so
  it is never reused — not by a retry, and not by a `PeerEntry` recreated after
  `DEAD_GC_TTL` evicted the old one. A retry counter cannot do this job:
  `PeerPhase::Connecting::attempt` resets to 0 on every fresh dial.
- Carried out on `StartConnect`, `ReadVersion`, `UpgradeToL2cap`,
  `StartDataPipe`, `CloseChannel`, `CloseNativeConnection` and `Refresh`, and
  back on `ConnectSucceeded`, `ConnectFailed`, `ProtocolVersionMismatch`,
  `OpenL2capSucceeded` and `OpenL2capFailed`. The registry drops any completion
  whose identity no longer matches the entry's.
- Retired by `Registry::abandon_outstanding`, which emits
  `PeerAction::RetireLifecycle` and mints a fresh identity. Every path that
  walks away from a connection goes through it: a new dial, a drain, `Forget`,
  adapter-off, a peripheral pipe restart, an inbound role replacement.
- **Preserved** in one case: an early `CentralReceivedP2c` notification while
  the peer is still `Connecting` is our own dial succeeding, so the entry keeps
  its identity and only the phase advances. The dial's own completion is then
  dropped on the phase, not the identity.

`PeerEntry::upgrade_gen` numbers L2CAP opens *within* one lifecycle and resets
to 0 when the lifecycle is retired, so `(lifecycle_id, upgrade_gen)` is unique.

A stale completion is dropped, never acted on — in particular a stale
`ConnectSucceeded` does **not** close its channel, because for that `DeviceId`
it is the same native connection the replacement is about to use.

### Per-device lanes (`driver.rs`)

Native-connection work is serialized per device: connect, disconnect, refresh,
the VERSION read and the L2CAP open queue on a `DeviceLane` and run one at a
time, in dispatch order, on a worker the `Driver` owns (so they die with it
rather than outliving the actor). Data-pipe I/O does not go through the lane.

The lane worker tracks a `native_owner` — the lifecycle that currently owns the
device's native connection — advanced by an `Activate` job that `StartConnect`
and `StartDataPipe` enqueue. Because ownership moves *in queue order*, two
guarantees fall out of the same rule: a teardown finishes before the retry that
reuses the connection begins, and a cleanup naming a lifecycle the device has
already replaced is skipped instead of taking the replacement down with it.

`RetireLifecycle` clears the lane's *cancellable* lifecycle immediately, which
is separate from native ownership. That split is what each `Cancellation` mode
means:

| Mode | Used by | Behaviour on retirement |
|------|---------|-------------------------|
| `Never` | disconnect, refresh, close-native | Runs anyway, but only while its lifecycle still owns the device — cleanup has to survive the retirement that ordered it |
| `WhileQueued` | connect | Skipped if not yet started; a running connect is *joined*, since dropping it mid-flight leaves the platform's GATT client in a state we can't reason about. Its result is rejected by identity instead |
| `Immediate` | VERSION read, L2CAP open | Unwound as soon as the lifecycle is retired |

Every lane job is bounded, because one that never returns blocks the device for
good — no later job can run and `native_owner` can never advance:

| Constant | Value | Purpose |
|----------|-------|---------|
| `CLEANUP_TIMEOUT` | 10 s | Bounds `disconnect` / `refresh`. On expiry the waiter is **dropped**, not detached, so no late continuation can touch ownership after the lane advances; native state is then uncertain and logged at `warn` |
| `GATT_SETUP_TIMEOUT` | 30 s | Bounds `discover_services` + `subscribe_characteristic` together inside `BlewDriver::connect`; separate from blew's own configurable connect timeout |
| `VERSION_READ_TIMEOUT` | 5 s | VERSION is optional; don't let a slow read hold up an upgrade queued behind it |

`connect` reports one error whether the link never came up or came up and then
failed GATT setup, and a peer in `Connecting` holds no channel for
`CloseChannel` to name — so `handle_connect_failed` emits
`CloseNativeConnection` to tear down whatever the dial may have left up. Its
guards are what make that safe: it is reached only when nothing has replaced
the dial.

### Routing table (`TransportRouting`)

Owns two pieces of state above the per-peer state machine:

1. `Token ↔ peer-identity` — the iroh-facing `CustomAddr` allocation.
2. `KeyPrefix → DeviceId` — the live indirection that lets prefix-keyed tokens follow MAC rotation.

`TransportRouting::note_discovery(prefix, device)` is called from `run_central_events` whenever an advertisement parses cleanly. `device_for_token(token)` is what the `BleSender` consults at every send — so updates to the `discovered` map are picked up immediately.

### Per-peer data pipe

`pipe.rs` owns the runtime side of a connected peer. `run_data_pipe` branches on `ConnectPath`:

**GATT path (`run_gatt_pipe`):**
- An `Arc<ReliableChannel>` for the Selective-Repeat ARQ protocol
- A background send loop draining the outbound mpsc → ReliableChannel
- A datagram delivery task routing reassembled datagrams → `incoming_tx` (which feeds iroh)
- An MTU resolver (`resolve_chunk_size`) that asks blew for the negotiated ATT MTU at startup, falls back if it never reaches `MIN_SANE_MTU=24`

**L2CAP path (`run_l2cap_pipe`):**
- Uses `spawn_l2cap_io_tasks` from `l2cap.rs` — length-prefixed `[u16 LE][payload]` framing over `AsyncRead + AsyncWrite`
- No `ReliableChannel` — L2CAP CoC is already reliable and stream-oriented
- Outbound datagrams are framed and written directly; inbound frames are read and delivered to `incoming_tx`

### QUIC configuration

The transport does **not** bundle a QUIC config — iroh's defaults are used unless the application overrides them via `Endpoint::builder().transport_config(...)`. For slow/lossy BLE links the chat app sets `max_idle_timeout(15s)` so iroh tears down stalled connections closer to the BLE-level disconnect detection window (`LINK_DEAD_DEADLINE` = 6 s); anything shorter risks killing the TLS handshake over the initial GATT path, which can take 3-5 s with ARQ retransmits.

## Reliable transport protocol (`src/transport/reliable.rs`)

Selective Repeat ARQ sliding window. **Wire format** (2-byte header):

```
Byte 0:
  Bits 0-3: SEQ      — 4-bit sequence number (0-15, modular)
  Bit  4:   FIRST    — first fragment of a datagram
  Bit  5:   LAST     — last fragment of a datagram
  Bits 6-7: reserved

Byte 1:
  Bits 0-3: ACK_SEQ  — cumulative ACK (all seqs up to this received)
  Bit  4:   ACK      — ACK_SEQ field is valid
  Bits 5-7: reserved

Trailer:
  1-byte FRAGMENT_CANARY (0x5A) appended to every fragment; receiver validates and strips.
  Mismatch → `truncations` counter increments and the fragment is dropped.
```

**Key parameters:**

| Constant | Value | Purpose |
|----------|-------|---------|
| `WINDOW_SIZE` | 6 | Max in-flight un-ACKed fragments |
| `INTER_FRAME_GAP` | 3 ms | Delay between fragment sends (prevents BLE buffer overflow) |
| `ACK_TIMEOUT` | 300 ms | Retransmit timeout (BLE RTT is ~120-200 ms) |
| `ACK_TIMEOUT_MAX` | 5 s | Max timeout after exponential backoff |
| `ACK_DELAY` | 15 ms | Delayed-ACK window for piggybacking |
| `LINK_DEAD_DEADLINE` | 6 s | Total time without progress before declaring the link dead |
| `SEQ_MODULUS` | 16 | Sequence number space (4-bit) |
| `SEND_QUEUE_CAPACITY` | 32 | Outbound mpsc capacity |
| `FRAGMENT_CANARY` | 0x5A | Per-fragment integrity sentinel |

Chunk size is **not** a fixed constant — it is resolved per peer at handshake time from the negotiated ATT MTU via `mtu::resolve_chunk_size`, with `MIN_SANE_MTU=24` as the floor and `DEFAULT_FALLBACK_MTU=512` if MTU never reaches the floor inside `MTU_READY_DEADLINE=1s`. `mtu::MAX_DATAGRAM_SIZE=1472` is the single source of truth for the reassembled-datagram ceiling shared by both the GATT and L2CAP paths.

**Protocol behaviour:**

- Sender sends up to `WINDOW_SIZE` fragments with `INTER_FRAME_GAP` between each.
- Receiver buffers out-of-order fragments within the window (`recv_buf`).
- Cumulative ACKs: "I have received everything up to seq X contiguously."
- On timeout: retransmits only the **oldest un-ACKed fragment** (selective, not Go-Back-N).
- ACKs are piggybacked on outgoing data; pure ACKs sent only when no data is available within `ACK_DELAY`.
- A shared `Arc<AtomicU64> retransmit_counter` is incremented per retransmit for health monitoring.
- A shared `Arc<AtomicU64> truncation_counter` is incremented whenever the fragment canary is missing/wrong.

**Important invariant:** `WINDOW_SIZE` must be < `SEQ_MODULUS / 2` (i.e. < 8) for correct modular window arithmetic.

## Telemetry

The transport exposes two snapshot APIs:

- `BleTransport::metrics() -> BleMetricsSnapshot` — `tx_bytes`, `rx_bytes`, `retransmits`, `truncations` (cumulative atomics).
- `BleTransport::snapshot_peers() -> Vec<BlePeerInfo>` — current `(device_id, phase, consecutive_failures, connect_path)` for every peer in the registry. Backed by an `arc_swap::ArcSwap<SnapshotMaps>` that the registry republishes on every state change.
- `BleTransport::device_for_endpoint(EndpointId) -> Option<DeviceId>` — resolves an iroh endpoint to a BLE device via the routing table's prefix map.
- `BleTransport::adapter_state() -> BleAdapterState` / `adapter_state_changes() -> watch::Receiver<BleAdapterState>` — local adapter power (`PoweredOn`/`PoweredOff`). Published by the registry actor after it has handled the transition, deduplicated across the central and peripheral event streams (both report every change).

The chat app polls these from `bandwidth_tick` (1 s) and `transport_state_tick` (1 s) and forwards diffs to the frontend as `bandwidth` and `ble-peer-updated`/`ble-peer-removed` Tauri events.

## Known limitations / open issues

- **No out-of-order ACK (SACK)**: GATT path's `ReliableChannel` sends cumulative ACKs only; out-of-order fragments are buffered but never explicitly acknowledged. (Not relevant for L2CAP path.)
- **Same-machine BLE on macOS**: BLE discovery does not work between two processes on the same macOS host (CoreBluetooth limitation). Always test on separate devices.
- **L2CAP MTU**: `mtu::MAX_DATAGRAM_SIZE = 1472` is shared between both paths. This may be conservative for L2CAP CoC which can negotiate larger MTUs. iroh's `initial_mtu(1200)` may also be revisitable once L2CAP performance is characterized on hardware. Set `L2capPolicy::Disabled` to force GATT-only if L2CAP causes issues on a specific platform.

## demos/iroh-ble-chat

A Tauri 2 cross-platform chat app (`demos/iroh-ble-chat`). Desktop + iOS + Android. Uses **iroh-gossip** over `BleTransport` for fan-out instead of point-to-point streams.

**Architecture:** `src-tauri/src/lib.rs` builds a `BleTransport`, mounts it on an iroh `Endpoint`, and starts an `iroh-gossip` `Router`. Chat messages are postcard-serialized `ChatMsg` values from the `iroh-ble-chat-protocol` crate (`demos/protocol`), broadcast through gossip. Image transfers stream over plain QUIC bi-streams (`IMAGE_ALPN`).

**Logging:** tester-facing logs should stay low-noise. Keep lifecycle information at `info`, actionable connection problems at `warn`, adapter/platform failures at `error`, and push per-message, handshaking, and stream-by-stream diagnostics down to `debug`/`trace`. The demo app's `Debug events` toggle is **off by default**.

**Tauri commands:** `start_node`, `get_node_id`, `set_nickname`, `add_peer`, `remove_peer`, `get_peers`, `send_message`, `send_image`, `set_debug`.

**Background tasks (spawned in `start_node`):**
- `gossip_event_pump` — drains gossip events, updates `PeerState`, emits `peer-updated`
- `bandwidth_tick` (1 s) — polls `BleTransport::metrics()`, emits `bandwidth`
- `stale_tick` (30 s) — flips peers to `GossipStatus::Stale` after 120 s without a sighting
- `transport_state_tick` (1 s) — diffs `snapshot_peers()`, emits `ble-peer-updated` / `ble-peer-removed`; also updates `PeerState.ble_path` (L2CAP/GATT) via `device_for_endpoint` and emits `peer-updated` when it changes
- `reconnect_tick` (10 s) — for every known peer not currently `GossipStatus::Direct`, calls `sender.join_peers([peer_id])`. **This is where reconnect policy lives** — the transport itself is passive.

**Frontend events:** `peer-updated`, `peer-removed`, `topic-joined`, `chat-msg`, `bandwidth`, `ble-peer-updated`, `ble-peer-removed`, image-stream events.

**Android integration:** Uses `tauri-plugin-blew` (from blew repo) for BLE setup. Registered conditionally via `#[cfg(target_os = "android")]`.

**Frontend:** Vanilla TypeScript + Vite (`demos/iroh-ble-chat/src/main.ts`).

## Example crate

```sh
# Listener (machine A):
cargo run --example iroh_ble -p iroh-ble-transport

# Dialer (machine B):
cargo run --example iroh_ble -p iroh-ble-transport -- <endpoint-id-from-A>
```

Runs an echo test + 48 KB speed test. Secret key is cached in `~/.cache/iroh-ble-example/private.key`.

`central` and `peripheral` examples exercise just the blew side without the iroh transport, useful for triaging whether a regression is in the radio layer or the transport.
