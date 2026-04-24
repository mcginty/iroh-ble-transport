use iroh::endpoint::presets;
use iroh::{Endpoint, SecretKey};
use iroh_ble_transport::BleTransport;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let secret_key = SecretKey::generate();

    let ble = BleTransport::builder().build(secret_key.public()).await?;

    let _endpoint = Endpoint::builder(presets::N0DisableRelay)
        .hooks(ble.dedup_hook())
        .add_custom_transport(ble.as_custom_transport())
        .address_lookup(ble.address_lookup())
        .secret_key(secret_key.clone())
        .clear_ip_transports()
        .bind()
        .await?;

    println!(
        "iroh-ble-transport initialized. Endpoint ID: {}",
        secret_key.public()
    );
    Ok(())
}
