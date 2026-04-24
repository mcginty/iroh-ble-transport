use iroh::endpoint::presets;
use iroh::{Endpoint, SecretKey};
use iroh_ble_transport::BleTransport;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let secret_key = SecretKey::generate();

    let ble = BleTransport::builder().build(secret_key.public()).await?;

    let endpoint = Endpoint::builder(presets::N0DisableRelay)
        .hooks(ble.dedup_hook())
        .add_custom_transport(ble.as_custom_transport())
        .address_lookup(ble.address_lookup())
        .secret_key(secret_key.clone())
        .clear_ip_transports()
        .bind()
        .await?;

    // Tear BLE pipes down promptly when iroh decides a Connection is dead,
    // instead of waiting for QUIC's idle timeout. Must be bound to a
    // variable: dropping the returned `PipeWatchdog` aborts the task.
    //
    // This watchdog can't live inside `BleTransport` itself because iroh
    // 0.98 has no `Weak<Endpoint>`, so holding the endpoint on the
    // transport would create a reference cycle. A future iroh release may
    // add one (or an `on_connection_closed` hook that obviates the
    // watchdog entirely), after which this line can go away.
    let _watchdog = ble.start_pipe_watchdog(&endpoint);

    println!(
        "iroh-ble-transport initialized. Endpoint ID: {}",
        secret_key.public()
    );
    Ok(())
}
