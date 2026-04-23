use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::{QuicTransportConfig, presets};
use iroh::{Endpoint, SecretKey};
use iroh_ble_transport::{
    BleDedupHook, BleTransport, BleTransportConfig, Central, InMemoryPeerStore, L2capPolicy,
    Peripheral,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let secret_key = SecretKey::generate();
    let local_id = secret_key.public();

    // 1. Bring up the BLE radio as both central and peripheral.
    let central = Arc::new(Central::new().await?);
    let peripheral = Arc::new(Peripheral::new().await?);

    // 2. Channel carrying post-TLS "this CustomAddr belongs to this
    //    EndpointId" events from the dedup hook to the transport.
    let (verified_tx, verified_rx) = tokio::sync::mpsc::unbounded_channel();

    let ble_transport = Arc::new(
        BleTransport::with_config(
            local_id,
            central,
            peripheral,
            BleTransportConfig {
                l2cap_policy: L2capPolicy::PreferL2cap,
                store: Arc::new(InMemoryPeerStore::new()),
                verified_rx: Some(verified_rx),
            },
        )
        .await?,
    );

    // 3. BLE-tuned QUIC idle timeout. BLE disconnect detection fires
    //    within ~6s; the default 30s would leave dead Connections
    //    hanging while the UI still shows them as healthy.
    let transport_cfg = QuicTransportConfig::builder()
        .max_idle_timeout(Some(Duration::from_secs(15).try_into()?))
        .build();

    // 4. Build the endpoint. The dedup hook, address lookup, and
    //    transport config are all required for BLE to work correctly:
    //    skip any of them and connections will appear to form but never
    //    route traffic reliably.
    let lookup = ble_transport.address_lookup();
    let transport: Arc<dyn iroh::endpoint::transports::CustomTransport> = ble_transport.clone();
    let endpoint = Endpoint::builder(presets::N0DisableRelay)
        .hooks(BleDedupHook::new(
            local_id,
            ble_transport.routing_handle(),
            verified_tx,
        ))
        .add_custom_transport(transport)
        .address_lookup(lookup)
        .transport_config(transport_cfg)
        .secret_key(secret_key)
        .clear_ip_transports()
        .bind()
        .await?;

    // 5. Recommended: tear BLE pipes down as soon as iroh gives up on
    //    their Connection, instead of waiting for the next idle-timeout.
    let _watchdog = ble_transport.spawn_pipe_watchdog(Arc::new(endpoint.clone()));

    println!("iroh-ble-transport initialized. Endpoint ID: {local_id}");
    Ok(())
}
