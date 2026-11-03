//! Android launch smoke test for `iroh-ble-transport`.
//!
//! The failures this catches only happen inside a real app on a device:
//! tauri-plugin-blew handing blew the application Context while tao publishes
//! `ndk_context` in `onCreate`, blew's Kotlin managers loading through the
//! classloader `init_jvm` cached, and `BleTransport` registering its GATT
//! service, advertising and L2CAP listener on the platform stack before an
//! iroh `Endpoint` binds on top of it.
//!
//! CI installs this on an emulator and reads the verdict from logcat:
//! `IROH_BLE_SMOKE_OK`, or `IROH_BLE_SMOKE_FAIL` with the stage that failed. A
//! crash prints neither.

use iroh::endpoint::presets;
use iroh::{Endpoint, SecretKey};
use iroh_ble_transport::BleTransport;
use tauri_plugin_blew::BlewPluginConfig;

const TAG: &str = "iroh-ble-smoke";

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "android")]
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Debug)
            .with_tag(TAG),
    );
    // Rust's panic message goes to stderr, which Android discards, so a panic
    // in plugin setup would otherwise show up as a bare SIGABRT.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(&format!("IROH_BLE_SMOKE_PANIC {info}"));
        default_hook(info);
    }));

    tauri::Builder::default()
        // CI grants the runtime permissions at install; a dialog would only
        // sit in front of the activity.
        .plugin(tauri_plugin_blew::init_with_config(BlewPluginConfig {
            auto_request_permissions: false,
        }))
        .setup(|_app| {
            tauri::async_runtime::spawn(async {
                match smoke().await {
                    Ok(summary) => report(&format!("IROH_BLE_SMOKE_OK {summary}")),
                    Err(e) => report(&format!("IROH_BLE_SMOKE_FAIL {e}")),
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

async fn smoke() -> Result<String, String> {
    if !tauri_plugin_blew::is_initialized() {
        return Err("plugin: setup did not initialise blew's JNI bridge".into());
    }
    if !tauri_plugin_blew::are_ble_permissions_granted() {
        return Err("plugin: BLE permissions not granted (install with `adb install -g`)".into());
    }

    let secret_key = SecretKey::generate();
    let ble = BleTransport::builder()
        .build(secret_key.public())
        .await
        .map_err(|e| format!("transport: BleTransport::build: {e}"))?;
    let adapter = ble.adapter_state();

    let endpoint = Endpoint::builder(presets::N0DisableRelay)
        .hooks(ble.dedup_hook())
        .add_custom_transport(ble.as_custom_transport())
        .address_lookup(ble.address_lookup())
        .secret_key(secret_key)
        .clear_ip_transports()
        .bind()
        .await
        .map_err(|e| format!("endpoint: Endpoint::bind: {e}"))?;

    let id = endpoint.id().fmt_short();
    endpoint.close().await;

    Ok(format!("adapter={adapter:?} endpoint={id}"))
}

fn report(line: &str) {
    #[cfg(target_os = "android")]
    log::info!(target: TAG, "{line}");
    #[cfg(not(target_os = "android"))]
    println!("{TAG}: {line}");
}
