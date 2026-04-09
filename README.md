# `iroh-ble-transport`

🔥 Warning 🔥 This library is in alpha state and is subject to change without
backwards-compatibility until otherwise noted.

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
- Upon connection, clients will try to connect via L2CAP, with a GATT fallback

## Supported Platforms

| Platform | Backend | Status |
|----------|---------|--------|
| macOS / iOS | CoreBluetooth (via `objc2`) | Supported |
| Linux | BlueZ (via `bluer`) | Supported |
| Android | JNI + Kotlin (via `jni` and `ndk-context`) | Supported |

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

## Examples

```sh
# Echo + speed test (listener)
cargo run --example iroh_ble -p iroh-ble-transport

# Echo + speed test (dialer)
cargo run --example iroh_ble -p iroh-ble-transport -- <endpoint-id>
```

## Demo App

There's a Tauri-based demo group chat app to show off Iroh BLE connectivity in
`demos/iroh-ble-chat`. More details on this app will follow, but please do not
use it for anything sensitive, it is not secure or in any way was written as a
toy solely to demonstrate the underlying transport.

## License

This project is licensed under the [GNU Affero General Public License v3.0 or later](LICENSE).

Commercial licenses are available for use cases where the AGPL is not suitable.
Contact [me@jakebot.org](mailto:me@jakebot.org) for details.
