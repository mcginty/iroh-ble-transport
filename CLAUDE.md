# iroh-ble-transport — Project Guide for Claude

## What this is

A Rust crate providing BLE (Bluetooth Low Energy) as a custom transport for the [iroh](https://github.com/n0-computer/iroh) peer-to-peer networking stack. Devices act as both central (scanner/client) and peripheral (advertiser/server) simultaneously to discover and connect to each other over BLE.

Uses [blew](https://github.com/mcginty/blew) (git dependency) for cross-platform BLE.

## Commands

Uses [mise](https://mise.jdx.dev) for task management. Run `mise tasks` for the full list.

```sh
mise run build                           # build all crates
mise run test                            # run all tests (nextest)
mise run lint                            # clippy
mise run fmt                             # format
mise run fmt:check                       # check formatting
mise run deny                            # license/vulnerability audit
cargo run --example iroh_ble -p iroh-ble-transport            # echo/speed test (listener)
cargo run --example iroh_ble -p iroh-ble-transport -- <id>    # echo/speed test (dialer)
```

## Style

- No comments unless the logic is non-obvious. Don't add doc comments to code you didn't change.
- Don't add features, refactor, or "improve" code beyond what was asked.
- Clippy pedantic is enabled (`pedantic = "warn"` in blew). Fix warnings, don't suppress them unless there's a good reason.
- Test with nextest: `cargo nextest run --workspace`.

## Key dependencies

| Crate | Role |
|-------|------|
| `blew` (git: mcginty/blew) | Both central and peripheral BLE roles |
| `iroh 0.97` with `unstable-custom-transports` | QUIC endpoint, custom transport trait |
| `iroh-base 0.97` | `EndpointId`, `CustomAddr`, `TransportAddr` |
| `tokio 1` | Async runtime |

## Module structure

```
crates/iroh-ble/src/
├── lib.rs                   # Re-exports from blew + error module
├── error.rs                 # BleError, BleResult, From<BlewError>
└── transport/
    ├── mod.rs               # BleTransport: iroh CustomTransport implementation
    ├── l2cap.rs             # L2CAP framing, identity exchange, I/O task spawning
    └── reliable.rs          # Selective Repeat ARQ sliding-window protocol
```

`iroh-ble` re-exports blew types (`Central`, `Peripheral`, `DeviceId`, `CentralEvent`, `PeripheralEvent`, etc.) from `lib.rs`. The transport holds `Arc<Central>` and `Arc<Peripheral>` directly and spawns two event handler tasks — one for central events (discovery, notifications) and one for peripheral events (read/write requests with RAII responders).

## Transport architecture

`BleTransport` implements iroh's `CustomTransport` trait, tunnelling QUIC datagrams over GATT.

### Discovery

Each node advertises a GATT service whose UUID encodes the first 12 bytes of its 32-byte Ed25519 public key:

```
69726F00-XXXX-XXXX-XXXX-XXXXXXXXXXXX
```

Peers identify each other from advertising packets without connecting. The key prefix in the advertising UUID is matched against the `discovered` map to resolve `EndpointId` → `DeviceId`.

### Addressing

The transport uses `DeviceId` (BLE device address string) as the `CustomAddr` data — not `EndpointId`. QUIC's connection IDs handle endpoint identity internally, so the transport only needs a stable per-peer routing key. This eliminates the need for identity exchange on the GATT data path entirely.

`AddressLookup::resolve()` maps `EndpointId` → key prefix (first 12 bytes) → `DeviceId` (via the `discovered` map) → `CustomAddr(device_key)`.

### GATT service layout

```
Service: 69726f01-8e45-4c2c-b3a5-331f3098b5c2  (IROH_SERVICE_UUID)
├── 69726f02 — C2P (WRITE_WITHOUT_RESPONSE | NOTIFY): central→peripheral data + ACKs
├── 69726f03 — P2C (WRITE_WITHOUT_RESPONSE | NOTIFY): peripheral→central data + ACKs
├── 69726f04 — PSM (READ, optional): 2-byte LE L2CAP PSM. Present only when the peripheral's `l2cap_listener()` succeeded at startup.
└── 69726f05 — VERSION (READ): 1-byte protocol version. Central reads this after connecting to verify wire compatibility.
```

- Central writes to C2P; peripheral sends data via P2C notifications.
- Both characteristics carry data AND acknowledgements (piggybacked or pure ACK).

### Connection handshake (GATT path)

1. Central connects, subscribes to P2C notifications.
2. Central creates `ReliableChannel`, sends first datagram on C2P.
3. Peripheral creates `ReliableChannel` eagerly on first incoming fragment.
4. Both sides are now ready for QUIC datagrams. No identity exchange needed.

### Connection handshake (L2CAP path)

1. Central reads PSM characteristic, opens L2CAP CoC.
2. Central writes its 32-byte EndpointId (raw, no header) — needed because L2CAP accept doesn't expose the remote DeviceId.
3. Peripheral reads identity, maps it to a DeviceId via the `discovered` map, both sides start length-prefixed datagram I/O.

### Per-peer state

Each peer gets a `PeerChannel` with:
- An `Arc<ReliableChannel>` for the reliable protocol
- A background send loop task
- A datagram delivery task (routes completed datagrams → iroh `incoming_tx`)

### Scan pausing

Scanning is paused when any active connection exists (reduces radio contention during transfers) and resumed when all connections disconnect.

### QUIC configuration (BlePreset)

- `initial_mtu(1200)`, `mtu_discovery_config(None)` — fixed MTU, no discovery
- `max_idle_timeout(300s)`, `keep_alive_interval(5s)` — long timeouts for slow BLE
- `default_path_max_idle_timeout(6s)`, `default_path_keep_alive_interval(4s)`

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
```

**Key parameters:**

| Constant | Value | Purpose |
|----------|-------|---------|
| `WINDOW_SIZE` | 6 | Max in-flight un-ACKed fragments |
| `INTER_FRAME_GAP` | 3ms | Delay between fragment sends (prevents BLE buffer overflow) |
| `ACK_TIMEOUT` | 300ms | Retransmit timeout (BLE RTT is ~120-200ms) |
| `ACK_TIMEOUT_MAX` | 5s | Max timeout after exponential backoff |
| `MAX_RETRIES` | 15 | Retransmits before declaring link dead |
| `DEFAULT_CHUNK_SIZE` | 512 | Max GATT write/notify size (bytes) |
| `MAX_DATAGRAM_SIZE` | 1472 | Max reassembled datagram |
| `SEQ_MODULUS` | 16 | Sequence number space (4-bit) |

**Protocol behaviour:**

- Sender sends up to `WINDOW_SIZE` fragments with `INTER_FRAME_GAP` between each.
- Receiver buffers out-of-order fragments within the window (`recv_buf`).
- Cumulative ACKs: "I have received everything up to seq X contiguously."
- On timeout: retransmits only the **oldest un-ACKed fragment** (selective, not Go-Back-N).
- ACKs are piggybacked on outgoing data; pure ACKs sent only when no data is available.
- `retransmit_counter` (shared `Arc<AtomicU64>`) is incremented per retransmit for health monitoring.

**Important invariant:** `WINDOW_SIZE` must be < `SEQ_MODULUS / 2` (i.e. < 8) for correct modular window arithmetic.

## Health check

Logged every 10 seconds:

```
discoveries, iroh_peers, lagged_events, discovered_peers,
peer_channels, active_connections, retransmits (per 10s interval),
tx_kbps, rx_kbps
```

`retransmits` resets to zero each 10-second health interval. High retransmits (>10/interval) indicate BLE radio congestion or link quality issues.

## Known limitations / open issues

- **No out-of-order ACK (SACK)**: Receiver sends cumulative ACKs only; no selective acknowledgement of individual out-of-order fragments beyond buffering.
- **L2CAP preferred, GATT fallback**: The transport prefers L2CAP CoC when both peers support it (Apple, Linux, Android), and falls back to GATT write-without-response + notifications otherwise.
- **Same-machine BLE**: BLE discovery does not work between two processes on the same macOS machine (CoreBluetooth limitation). Always test on separate devices.

## demos/iroh-ble-chat

A Tauri 2 cross-platform chat app (`demos/iroh-ble-chat`). Desktop + iOS + Android.

**Architecture:** `src-tauri/src/lib.rs` creates a `BleTransport` + iroh `Endpoint`, then accepts/connects peers. Chat messages use `iroh-ble-chat-protocol` (postcard-serialized `ChatMsg` over QUIC bi-streams).

**Tauri commands:** `start_node`, `connect_peer`, `send_message`, `get_node_id`. Connection handshake runs in a background task (BLE can take 10-30s).

**Android integration:** Uses `tauri-plugin-blew` (from blew repo) for BLE setup. The plugin is registered conditionally via `#[cfg(target_os = "android")]`.

**Frontend:** Vanilla TypeScript + Vite. Listens to `chat-msg` and `connection-status` Tauri events.

## demos/chat-tui

A terminal UI chat demo (`demos/chat-tui`). Uses `iroh-ble-chat-protocol` over `BleTransport`.

## Example

```sh
# Listener (machine A):
cargo run --example iroh_ble

# Dialer (machine B):
cargo run --example iroh_ble -- <endpoint-id-from-A>
```

Runs an echo test + 48KB speed test. Secret key is cached in `~/.cache/iroh-ble-example/private.key`.
