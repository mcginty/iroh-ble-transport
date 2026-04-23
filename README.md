[![iroh-ble-transport Crates.io](https://img.shields.io/crates/v/iroh-ble-transport)](https://crates.io/crates/iroh-ble-transport)
[![iroh-ble-transport Docs.rs](https://img.shields.io/docsrs/iroh-ble-transport)](https://docs.rs/iroh-ble-transport)

# `iroh-ble-transport`

🔥 **Warning:** 🔥 This library is experimental. A best effort will be made to resolve
bugs and follow semantic versioning, but there are no guarantees. Do not rely on it
until it has been sufficiently field-tested.

`iroh-ble-transport` is a Rust crate providing BLE (Bluetooth Low Energy) as a
custom transport for the [Iroh](https://github.com/n0-computer/iroh)
peer-to-peer networking stack. Devices act as both central (scanner/client) and
peripheral (advertiser/server) simultaneously to discover and connect to each
other over BLE.

Built on top of [blew](https://github.com/mcginty/blew), a cross-platform BLE
library for Rust.

## High-level functionality

- Each node advertises a GATT service whose UUID encodes its public key prefix
- Peers discover each other from advertising packets without connecting
- Connections start on GATT, then upgrade to L2CAP when available
- If L2CAP setup fails or times out, the connection falls back to GATT

## Supported Platforms

| Platform    |
|-------------|
| macOS / iOS |
| Linux       |
| Android     |

## Usage

```rust
use iroh_ble_transport::BleTransport;
use iroh::Endpoint;

let transport = BleTransport::new().await?;
let endpoint = Endpoint::builder()
    .add_custom_transport(transport)
    .bind()
    .await?;
```

## Demo

There is a Tauri-based (unencrypted, insecure) chat demo called BlewChat that
works on iOS, Android, macOS, and Linux. For iOS, the easiest way to install
is to [install the latest release via TestFlight](https://testflight.apple.com/join/sg71NHZU).

For other platforms, either build from source or download the release artifacts
when they're made available.

## License

This project is licensed under the [GNU Affero General Public License v3.0 or later](LICENSE).

Commercial licenses are available for use cases where the AGPL is not suitable.
Contact [me@jakebot.org](mailto:me@jakebot.org) for details.
