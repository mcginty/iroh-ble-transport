//! Image compression pipeline: decode -> resize -> AVIF encode.

use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use std::collections::HashMap;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;

const MAX_DIMENSION: u32 = 800;
const AVIF_QUALITY: f32 = 35.0;
const AVIF_SPEED: u8 = 10;

/// CPU-intensive -- call from `spawn_blocking`.
pub fn encode_image_from_bytes(data: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Cursor;
    let total_start = std::time::Instant::now();

    let img = image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|e| format!("Failed to guess image format: {e}"))?
        .decode()
        .map_err(|e| format!("Failed to decode image: {e}"))?;
    let decode_ms = total_start.elapsed().as_millis();

    let (orig_w, orig_h) = (img.width(), img.height());
    let resize_start = std::time::Instant::now();
    let resized = img.width() > MAX_DIMENSION || img.height() > MAX_DIMENSION;
    let img = if resized {
        img.resize(
            MAX_DIMENSION,
            MAX_DIMENSION,
            image::imageops::FilterType::CatmullRom,
        )
    } else {
        img
    };
    let resize_ms = resize_start.elapsed().as_millis();
    let (final_w, final_h) = (img.width(), img.height());

    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    let pixels: Vec<rgb::RGBA8> = rgba
        .pixels()
        .map(|p| rgb::RGBA8::new(p[0], p[1], p[2], p[3]))
        .collect();

    let encode_start = std::time::Instant::now();
    let encoder = ravif::Encoder::new()
        .with_quality(AVIF_QUALITY)
        .with_speed(AVIF_SPEED);
    let encoded = encoder
        .encode_rgba(ravif::Img::new(&pixels, w, h))
        .map_err(|e| format!("AVIF encode failed: {e}"))?;

    let encode_ms = encode_start.elapsed().as_millis();
    let total_ms = total_start.elapsed().as_millis();
    tracing::debug!(
        input_bytes = data.len(),
        orig = %format!("{orig_w}x{orig_h}"),
        final_size = %format!("{final_w}x{final_h}"),
        resized,
        avif_bytes = encoded.avif_file.len(),
        decode_ms,
        resize_ms,
        encode_ms,
        total_ms,
        "AVIF encode complete"
    );

    Ok(encoded.avif_file)
}

pub fn avif_to_data_uri(avif_bytes: &[u8]) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(avif_bytes);
    format!("data:image/avif;base64,{b64}")
}

#[derive(Clone, Debug)]
pub struct PendingImage {
    pub from_id: String,
    pub nickname: String,
    pub size: usize,
}

pub type PendingImages = Arc<Mutex<HashMap<u64, PendingImage>>>;

#[derive(Clone, Debug)]
pub struct ImageReceiver {
    pub app: AppHandle,
    pub pending: PendingImages,
}

impl ProtocolHandler for ImageReceiver {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        tracing::debug!(
            remote = %connection.remote_id().fmt_short(),
            "IMAGE_ALPN connection accepted"
        );
        loop {
            let mut recv = match connection.accept_uni().await {
                Ok(r) => {
                    tracing::debug!(
                        remote = %connection.remote_id().fmt_short(),
                        "accepted uni stream for image"
                    );
                    r
                }
                Err(e) => {
                    tracing::debug!(
                        remote = %connection.remote_id().fmt_short(),
                        err = %e,
                        "IMAGE_ALPN connection closed"
                    );
                    break;
                }
            };

            let app = self.app.clone();
            let pending = self.pending.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_image_stream(&mut recv, &app, &pending).await {
                    tracing::warn!("image receive error: {e}");
                }
            });
        }
        Ok(())
    }
}

async fn handle_image_stream(
    recv: &mut iroh::endpoint::RecvStream,
    app: &AppHandle,
    pending: &PendingImages,
) -> Result<(), String> {
    tracing::debug!("image stream: reading header");
    let mut id_buf = [0u8; 8];
    read_exact_uni(recv, &mut id_buf).await?;
    let image_id = u64::from_le_bytes(id_buf);
    tracing::debug!(image_id, "image stream: got header");

    let info = wait_for_pending(pending, image_id).await;

    let recv_start = std::time::Instant::now();
    const MAX_IMAGE_SIZE: usize = 2 * 1024 * 1024;
    let mut avif_data = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        match recv.read(&mut buf).await {
            Ok(Some(n)) => {
                avif_data.extend_from_slice(&buf[..n]);
                if avif_data.len() > MAX_IMAGE_SIZE {
                    return Err("image too large".to_string());
                }
                if let Some(ref info) = info {
                    let _ = app.emit(
                        "image-progress",
                        serde_json::json!({
                            "image_id": image_id.to_string(),
                            "bytes": avif_data.len(),
                            "total": info.size,
                            "is_self": false,
                        }),
                    );
                }
            }
            Ok(None) => break, // EOF
            Err(e) => return Err(format!("stream read error: {e}")),
        }
    }

    let recv_ms = recv_start.elapsed().as_millis();
    let kbps = (avif_data.len() as u128 * 8)
        .checked_div(recv_ms)
        .unwrap_or(0);
    tracing::debug!(
        image_id,
        bytes = avif_data.len(),
        recv_ms,
        kbps,
        "image stream: receive complete"
    );

    let data_uri = avif_to_data_uri(&avif_data);
    let (from_id, nickname) = match info {
        Some(i) => (i.from_id, i.nickname),
        None => ("unknown".to_string(), "Unknown".to_string()),
    };

    let _ = app.emit(
        "chat-image",
        serde_json::json!({
            "from_id": from_id,
            "nickname": nickname,
            "image_id": image_id.to_string(),
            "data_uri": data_uri,
            "is_self": false,
        }),
    );

    pending.lock().await.remove(&image_id);

    Ok(())
}

async fn wait_for_pending(pending: &PendingImages, image_id: u64) -> Option<PendingImage> {
    if let Some(info) = pending.lock().await.get(&image_id) {
        return Some(info.clone());
    }
    // Gossip ImageStart may arrive after the stream opens -- retry briefly.
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if let Some(info) = pending.lock().await.get(&image_id) {
            return Some(info.clone());
        }
    }
    tracing::warn!(image_id, "no ImageStart received for uni stream");
    None
}

async fn read_exact_uni(
    recv: &mut iroh::endpoint::RecvStream,
    buf: &mut [u8],
) -> Result<(), String> {
    let mut pos = 0;
    while pos < buf.len() {
        match recv.read(&mut buf[pos..]).await {
            Ok(Some(n)) => pos += n,
            Ok(None) => return Err(format!("unexpected EOF after {pos} bytes")),
            Err(e) => return Err(format!("read error: {e}")),
        }
    }
    Ok(())
}
