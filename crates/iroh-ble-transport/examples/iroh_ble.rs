//! iroh-over-BLE echo example.
//!
//! Runs in one of two roles:
//!
//! **Listener** (no arguments): starts a BLE transport, advertises, and accepts
//! incoming iroh connections. Echoes back whatever it receives on a bidirectional
//! QUIC stream.
//!
//! **Dialer** (pass the listener's endpoint ID): connects to the listener over
//! BLE, sends a message, and prints the echoed response.
//!
//! ## Usage
//!
//! On machine A (or terminal A):
//!
//! ```sh
//! cargo run --example iroh_ble
//! ```
//!
//! It will print something like:
//!
//! ```text
//! Listening. Endpoint ID:
//!   <base32-encoded-key>
//! ```
//!
//! On machine B (or terminal B on a *different* machine -- same-machine BLE
//! discovery does not work on macOS):
//!
//! ```sh
//! cargo run --example iroh_ble -- <endpoint-id-from-A>
//! ```
//!
//! If BLE discovery succeeds you will see the echo round-trip complete.

use std::time::Instant;

#[path = "../../../demos/support/key_utils.rs"]
mod key_utils;

use std::sync::Arc;

use iroh::Endpoint;
use iroh::endpoint::Connection;
use iroh::endpoint::presets;
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh_ble_transport::transport::BleTransport;
use iroh_ble_transport::{Central, Peripheral};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = sigint.recv() => {}
        _ = sigterm.recv() => {}
    }
}

const ECHO_ALPN: &[u8] = b"iroh-ble/echo/0";
const SPEED_TEST_SIZE: usize = 250_000; // ~250 KB
#[derive(Debug, Clone)]
struct Echo;

impl ProtocolHandler for Echo {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        // Accept streams in a loop so the same connection can run
        // the echo test followed by the speed test.
        loop {
            let (mut send, mut recv) = match connection.accept_bi().await {
                Ok(pair) => pair,
                Err(_) => break, // connection closed
            };

            let msg = recv
                .read_to_end(SPEED_TEST_SIZE + 4096)
                .await
                .map_err(std::io::Error::other)?;

            if msg.len() <= 256 {
                println!(
                    "  echo: {} bytes {:?}",
                    msg.len(),
                    String::from_utf8_lossy(&msg)
                );
            } else {
                println!("  echo: {} bytes (speed test payload)", msg.len());
            }

            send.write_all(&msg).await.map_err(std::io::Error::other)?;
            send.finish().map_err(std::io::Error::other)?;
        }

        Ok(())
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("iroh_ble=info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let remote_id = args.get(1).map(|s| s.as_str());

    match remote_id {
        None => run_listener().await,
        Some(id) => run_dialer(id).await,
    }
}

async fn run_listener() -> Result<(), Box<dyn std::error::Error>> {
    let secret = key_utils::load_or_generate_key("iroh-ble-example");
    let public = secret.public();

    println!("Initialising BLE transport...");
    let central = Arc::new(Central::new().await?);
    let peripheral = Arc::new(Peripheral::new().await?);
    let transport = BleTransport::new(public, central, peripheral).await?;
    let lookup = transport.address_lookup();
    let transport: Arc<dyn iroh::endpoint::transports::CustomTransport> = Arc::new(transport);

    let ep = Endpoint::builder(presets::N0DisableRelay)
        .add_custom_transport(transport)
        .address_lookup(lookup)
        .secret_key(secret)
        .clear_ip_transports()
        .bind()
        .await?;

    println!("Listening. Endpoint ID:");
    println!("  {public}");
    println!();
    println!("On another machine run:");
    println!("  cargo run --example iroh_ble -- {public}");
    println!();
    println!("Waiting for connections... (Ctrl+C to stop)");

    let router = Router::builder(ep).accept(ECHO_ALPN, Echo).spawn();

    shutdown_signal().await;
    println!("\nShutting down...");
    router.shutdown().await?;
    Ok(())
}

fn format_throughput(bytes: usize, elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs_f64();
    let bits_per_sec = (bytes as f64 * 8.0) / secs;
    let bytes_per_sec = bytes as f64 / secs;
    if bits_per_sec >= 1_000_000.0 {
        format!(
            "{:.1} Mbit/s ({:.1} KB/s) in {:.1}s",
            bits_per_sec / 1_000_000.0,
            bytes_per_sec / 1024.0,
            secs,
        )
    } else {
        format!(
            "{:.1} kbit/s ({:.1} KB/s) in {:.1}s",
            bits_per_sec / 1000.0,
            bytes_per_sec / 1024.0,
            secs,
        )
    }
}

async fn send_with_progress(
    send: &mut iroh::endpoint::SendStream,
    data: &[u8],
    label: &str,
    report_interval: std::time::Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let total = data.len();
    let chunk_size = 16 * 1024; // 16 KB write chunks
    let mut written = 0usize;
    let start = Instant::now();
    let mut last_report = start;

    for chunk in data.chunks(chunk_size) {
        send.write_all(chunk).await?;
        written += chunk.len();

        let now = Instant::now();
        if now.duration_since(last_report) >= report_interval {
            let elapsed = now.duration_since(start);
            let pct = (written as f64 / total as f64) * 100.0;
            info!(
                direction = label,
                sent_kb = written / 1024,
                total_kb = total / 1024,
                progress = format!("{pct:.0}%"),
                throughput = %format_throughput(written, elapsed),
                "speed test progress"
            );
            last_report = now;
        }
    }
    send.finish()?;
    Ok(())
}

async fn recv_with_progress(
    recv: &mut iroh::endpoint::RecvStream,
    label: &str,
    report_interval: std::time::Duration,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut buf = Vec::new();
    let mut tmp = vec![0u8; 16 * 1024];
    let start = Instant::now();
    let mut last_report = start;

    while let Some(n) = recv.read(&mut tmp).await? {
        buf.extend_from_slice(&tmp[..n]);

        let now = Instant::now();
        if now.duration_since(last_report) >= report_interval {
            let elapsed = now.duration_since(start);
            info!(
                direction = label,
                received_kb = buf.len() / 1024,
                throughput = %format_throughput(buf.len(), elapsed),
                "speed test progress"
            );
            last_report = now;
        }
    }
    Ok(buf)
}

async fn run_speed_test(conn: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    let payload = vec![0xABu8; SPEED_TEST_SIZE];
    let interval = std::time::Duration::from_secs(5);
    println!(
        "Speed test: sending {} KB round-trip...",
        SPEED_TEST_SIZE / 1024
    );
    let start = Instant::now();
    {
        let (mut send, mut recv) = conn.open_bi().await?;
        send_with_progress(&mut send, &payload, "upload", interval).await?;
        info!("upload complete, waiting for echo...");
        let response = recv_with_progress(&mut recv, "download", interval).await?;
        if response.len() != SPEED_TEST_SIZE {
            warn!(
                expected = SPEED_TEST_SIZE,
                got = response.len(),
                "echo size mismatch"
            );
        }
    }
    let rtt = start.elapsed();
    println!("  round-trip: {}", format_throughput(SPEED_TEST_SIZE, rtt));
    println!(
        "  one-way (est): {}",
        format_throughput(SPEED_TEST_SIZE, rtt / 2)
    );
    println!(
        "\nSpeed test: sending {} KB round-trip (2nd run)...",
        SPEED_TEST_SIZE / 1024
    );
    let start = Instant::now();
    {
        let (mut send, mut recv) = conn.open_bi().await?;
        send_with_progress(&mut send, &payload, "upload-2", interval).await?;
        info!("upload complete, waiting for echo...");
        let response = recv_with_progress(&mut recv, "download-2", interval).await?;
        if response.len() != SPEED_TEST_SIZE {
            warn!(
                expected = SPEED_TEST_SIZE,
                got = response.len(),
                "echo size mismatch"
            );
        }
    }
    let rtt2 = start.elapsed();
    println!("  round-trip: {}", format_throughput(SPEED_TEST_SIZE, rtt2));
    println!(
        "  one-way (est): {}",
        format_throughput(SPEED_TEST_SIZE, rtt2 / 2)
    );

    println!("\nSpeed test complete.");
    Ok(())
}

async fn run_dialer(remote_str: &str) -> Result<(), Box<dyn std::error::Error>> {
    let remote_id: iroh::EndpointId = remote_str.parse()?;

    let secret = key_utils::load_or_generate_key("iroh-ble-example");
    let local_id = secret.public();

    println!("Initialising BLE transport...");
    let central = Arc::new(Central::new().await?);
    let peripheral = Arc::new(Peripheral::new().await?);
    let transport = BleTransport::new(local_id, central, peripheral).await?;
    let lookup = transport.address_lookup();
    let transport: Arc<dyn iroh::endpoint::transports::CustomTransport> = Arc::new(transport);

    let ep = Endpoint::builder(presets::N0DisableRelay)
        .add_custom_transport(transport)
        .address_lookup(lookup)
        .secret_key(secret)
        .clear_ip_transports()
        .bind()
        .await?;

    println!("Scanning for peer {}...", remote_id.fmt_short());
    println!("(Make sure the listener is running on another machine)");

    let mut attempts = 0;
    loop {
        attempts += 1;
        if attempts > 60 {
            eprintln!("Gave up after 60 s -- peer not found via BLE.");
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        print!(".");
        use std::io::Write;
        std::io::stdout().flush()?;

        let addr = iroh::EndpointAddr::from(remote_id);
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            ep.connect(addr, ECHO_ALPN),
        )
        .await
        {
            Ok(Ok(conn)) => {
                println!("\nConnected to {}!", remote_id.fmt_short());
                {
                    let (mut send, mut recv) = conn.open_bi().await?;
                    let msg = b"hello over BLE!";
                    send.write_all(msg).await?;
                    send.finish()?;
                    println!("  sent: {:?}", std::str::from_utf8(msg).unwrap());

                    let response = recv.read_to_end(4096).await?;
                    println!("  echo: {:?}", String::from_utf8_lossy(&response));
                }
                println!();
                run_speed_test(&conn).await?;

                conn.close(0u32.into(), b"done");
                break;
            }
            Ok(Err(e)) => {
                if attempts % 10 == 0 {
                    eprintln!("\n  (connect attempt {attempts} failed: {e})");
                }
            }
            Err(_) => {}
        }
    }

    ep.close().await;
    Ok(())
}
