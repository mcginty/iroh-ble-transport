# iroh-ble-chat

`iroh-ble-chat` is a Tauri-based demo app for exercising `iroh-ble-transport`
across desktop, Android, and iOS devices. It uses `iroh-gossip` for chat
fan-out and plain QUIC streams for image transfer.

This is a transport demo, not a production-secure messenger.

## Requirements

- Rust **1.95** or newer
- [mise](https://mise.jdx.dev) for the repo task wrappers
- Physical BLE-capable devices for meaningful testing

## Run

From the repo root:

```sh
mise run chat:dev
```

Mobile device workflows:

```sh
mise run chat:dev:android
mise run chat:dev:ios
mise run chat:deploy:mobile:all
```

## Behavior

- Peers are discovered over BLE advertisements.
- Transport connections start on GATT and may upgrade to L2CAP when available.
- Chat messages are sent over `iroh-gossip`.
- Images are transferred over a dedicated QUIC stream (`IMAGE_ALPN`).

## Logging

- `Debug events` in the UI is **off by default**.
- `info` is intended to stay low-noise and lifecycle-focused.
- `warn` is for actionable failures or degraded behavior.
- `debug` and `trace` are for transport and app diagnosis when reproducing issues.

## Notes

- On macOS, same-machine BLE discovery is not a reliable test setup; use
  separate devices.
- The app is for testing the BLE transport and reconnect/handover behavior, not
  for sensitive data.
