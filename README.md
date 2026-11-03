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

This library currently supports iOS, macOS, Android, and Linux.

### Android

- **Android 13 (API 33) or later.** blew's Android library declares
  `minSdk 33`; an app with a lower `minSdk` fails its manifest merge. In a
  Tauri app, set `bundle.android.minSdkVersion` to `33` in `tauri.conf.json`
  and `minSdk = 33` in `gen/android/app/build.gradle.kts`.
- **iroh's system DNS needs `ndk_context`, which blew no longer publishes.**
  iroh reads the system DNS servers through `ndk_context`; when nothing has
  published it, iroh falls back to public resolvers. BLE is unaffected, but
  DNS-dependent IP connectivity suffers on networks that block or rewrite
  public DNS (enterprise networks, captive portals, Private DNS).
  - **Tauri 2.12 and later:** tao publishes `ndk_context` itself, so there is
    nothing to do. Do **not** publish it yourself — not with
    `ndk_context::initialize_android_context`, not with a helper that wraps
    it, not with `install_android_context_once`. A second publish aborts the
    process.
  - **Tauri 2.11 and earlier:** nothing publishes it. Publish it from the
    handles blew already holds, once the plugin is registered (for example in
    your `setup` hook) and before binding the iroh `Endpoint`. This needs a
    direct `blew` dependency at the same version `tauri-plugin-blew` uses:

    ```rust
    #[cfg(target_os = "android")]
    fn publish_ndk_context_for_iroh() {
        let (Some(vm), Some(ctx)) = (
            blew::platform::android::java_vm(),
            blew::platform::android::application_context(),
        ) else {
            return;
        };
        unsafe {
            tauri_plugin_blew::install_android_context_once(
                vm.get_raw().cast(),
                ctx.as_raw().cast(),
            );
        }
    }
    ```

    Remove the call when you move to Tauri 2.12: tao's own publish in
    `onCreate` would then abort the app.
  - **`panic = "abort"`:** iroh detects a missing `ndk_context` with
    `catch_unwind`, which does nothing under `panic = "abort"`. Without a
    published context, such an app aborts on its first DNS read instead of
    falling back.

## Usage

BLE is a high-touch transport — it needs more wiring than the IP-based
defaults (a dedup hook and an address lookup):

```rust
use iroh::endpoint::presets;
use iroh::{Endpoint, SecretKey};
use iroh_ble_transport::BleTransport;

async fn bind_ble_endpoint() -> anyhow::Result<Endpoint> {
    let secret_key = SecretKey::generate();
    let ble = BleTransport::builder().build(secret_key.public()).await?;

    let endpoint = Endpoint::builder(presets::N0DisableRelay)
        .hooks(ble.dedup_hook())
        .add_custom_transport(ble.as_custom_transport())
        .address_lookup(ble.address_lookup())
        .secret_key(secret_key)
        .clear_ip_transports()
        .bind()
        .await?;

    Ok(endpoint)
}
```

See:

- [`examples/initialization.rs`](crates/iroh-ble-transport/examples/initialization.rs)
  — the minimum viable setup, end to end.
- [`examples/iroh_ble.rs`](crates/iroh-ble-transport/examples/iroh_ble.rs)
  — echo + 48 KB speed test over a dialer/listener pair.
- [`examples/central.rs`](crates/iroh-ble-transport/examples/central.rs) and
  [`examples/peripheral.rs`](crates/iroh-ble-transport/examples/peripheral.rs)
  — the bare BLE roles without the iroh transport, useful for working out
  whether a problem is in the radio layer or above it.
- [`demos/iroh-ble-chat`](demos/iroh-ble-chat) — production-grade
  iroh-gossip app built on this transport.

**Every example needs two machines.** A single Bluetooth controller does not
discover its own advertisements, so a dialer and a listener — or a central and
a peripheral — running on one host will never find each other, and blew always
binds the default adapter, so adding a second one does not help. On macOS this
holds even between two processes on the same machine.

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
