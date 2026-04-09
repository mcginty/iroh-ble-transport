//! BLE peripheral (server) example.
//!
//! Advertises a single GATT service and handles read/write requests from
//! connected centrals. Also pushes a notification every 2 seconds to any
//! subscribed centrals.
//!
//! Run with:
//!   cargo run --example peripheral -p iroh-ble
//!
//! Then run `cargo run --example central -p iroh-ble` on the same or another
//! machine to connect to it.

use iroh_ble::{
    AdvertisingConfig, AttributePermissions, CharacteristicProperties, GattCharacteristic,
    GattService, Peripheral, PeripheralEvent,
};
use std::time::Duration;
use tokio::time;
use tokio_stream::StreamExt as _;
use uuid::{Uuid, uuid};

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = sigint.recv() => {}
        _ = sigterm.recv() => {}
    }
}

// Must match the central example.
const SERVICE_UUID: Uuid = uuid!("12345678-0000-1000-8000-00805f9b34fb");
const CHAR_UUID: Uuid = uuid!("12345678-0001-1000-8000-00805f9b34fb");
const DEVICE_NAME: &str = "iroh-ble-example";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let peripheral: Peripheral = Peripheral::new().await?;
    let mut events = peripheral.events();

    // CoreBluetooth initialises asynchronously.
    if !peripheral.is_powered().await? {
        println!("Waiting for Bluetooth adapter...");
        while let Some(event) = events.next().await {
            match event {
                PeripheralEvent::AdapterStateChanged { powered: true } => break,
                PeripheralEvent::AdapterStateChanged { powered: false } => {
                    eprintln!("Bluetooth is powered off. Please enable Bluetooth and retry.");
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    peripheral
        .add_service(&GattService {
            uuid: SERVICE_UUID,
            primary: true,
            characteristics: vec![GattCharacteristic {
                uuid: CHAR_UUID,
                properties: CharacteristicProperties::READ
                    | CharacteristicProperties::WRITE
                    | CharacteristicProperties::NOTIFY,
                permissions: AttributePermissions::READ | AttributePermissions::WRITE,
                // CoreBluetooth crashes if a writable characteristic has a static value.
                value: vec![],
                descriptors: vec![],
            }],
        })
        .await?;

    peripheral
        .start_advertising(&AdvertisingConfig {
            local_name: DEVICE_NAME.to_string(),
            service_uuids: vec![SERVICE_UUID],
        })
        .await?;

    println!("Advertising as '{DEVICE_NAME}'. Waiting for connections...");
    println!("Press Ctrl+C to stop.\n");

    let mut counter: u32 = 0;
    let mut notify_interval = time::interval(Duration::from_secs(2));
    notify_interval.tick().await; // skip the immediate first tick

    loop {
        tokio::select! {
            event = events.next() => {
                match event {
                    Some(event) => handle_event(event, &peripheral).await?,
                    None => break,
                }
            }

            _ = notify_interval.tick() => {
                counter += 1;
                let value = format!("count={counter}").into_bytes();
                println!("-> notify: count={counter}");
                if let Err(e) = peripheral
                    .notify_characteristic(CHAR_UUID, value)
                    .await
                {
                    eprintln!("notify error: {e}");
                }
            }

            _ = shutdown_signal() => {
                println!("\nStopping advertisement...");
                peripheral.stop_advertising().await?;
                break;
            }
        }
    }

    Ok(())
}

async fn handle_event(
    event: PeripheralEvent,
    _peripheral: &Peripheral,
) -> Result<(), Box<dyn std::error::Error>> {
    match event {
        PeripheralEvent::AdapterStateChanged { powered } => {
            println!("Adapter powered: {powered}");
        }

        PeripheralEvent::SubscriptionChanged {
            client_id,
            char_uuid,
            subscribed,
        } => {
            let action = if subscribed {
                "subscribed to"
            } else {
                "unsubscribed from"
            };
            println!("<- {client_id} {action} {char_uuid}");
        }

        PeripheralEvent::ReadRequest {
            client_id,
            char_uuid,
            offset,
            responder,
            ..
        } => {
            println!("<- read  from {client_id}  char={char_uuid}  offset={offset}");
            responder.respond(b"hello from peripheral".to_vec());
        }

        PeripheralEvent::WriteRequest {
            client_id,
            char_uuid,
            value,
            responder,
            ..
        } => {
            let text = String::from_utf8_lossy(&value);
            println!("<- write from {client_id}  char={char_uuid}  value={text:?}");
            if let Some(r) = responder {
                r.success();
            }
        }
    }
    Ok(())
}
