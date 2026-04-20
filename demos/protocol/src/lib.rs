//! Shared protocol types and helpers for the iroh-ble chat demo.
//!
//! Extracted from the TUI example so it can be reused by other demos
//! (e.g. mobile).

use iroh::EndpointId;
use serde::{Deserialize, Serialize};

pub const CHAT_ALPN: &[u8] = b"iroh-ble/chat/1";
pub const IMAGE_ALPN: &[u8] = b"iroh-ble-chat/image/0";
pub const RETRY_MIN: std::time::Duration = std::time::Duration::from_secs(2);
pub const RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(30);

pub fn load_known_peers(cache_dir: &std::path::Path) -> Vec<EndpointId> {
    let path = cache_dir.join("peers.txt");
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

pub fn save_known_peers(cache_dir: &std::path::Path, peers: &[EndpointId]) {
    let _ = std::fs::create_dir_all(cache_dir);
    let content = peers
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let _ = std::fs::write(cache_dir.join("peers.txt"), content);
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChatMsg {
    Hello {
        from: EndpointId,
        nickname: String,
    },
    Text {
        from: EndpointId,
        nickname: String,
        text: String,
    },
    NicknameChanged {
        from: EndpointId,
        new_nickname: String,
    },
    /// Announces an incoming image; the actual data arrives on a separate uni stream.
    ImageStart {
        from: EndpointId,
        nickname: String,
        image_id: u64,
        size: usize,
    },
}

pub fn encode_msg(msg: &ChatMsg) -> Vec<u8> {
    let payload = postcard::to_allocvec(msg).unwrap_or_default();
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(&payload);
    buf
}

pub async fn read_msg(recv: &mut iroh::endpoint::RecvStream) -> anyhow::Result<ChatMsg> {
    let mut len_buf = [0u8; 4];
    read_exact(recv, &mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    anyhow::ensure!(len <= 65_536, "message too large: {len}");
    let mut buf = vec![0u8; len];
    read_exact(recv, &mut buf).await?;
    Ok(postcard::from_bytes(&buf)?)
}

async fn read_exact(recv: &mut iroh::endpoint::RecvStream, buf: &mut [u8]) -> anyhow::Result<()> {
    let mut pos = 0;
    while pos < buf.len() {
        match recv.read(&mut buf[pos..]).await? {
            Some(n) => pos += n,
            None => anyhow::bail!("unexpected EOF after {pos} bytes"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_start_round_trip() {
        let msg = ChatMsg::ImageStart {
            from: iroh_base::SecretKey::generate().public(),
            nickname: "alice".to_string(),
            image_id: 0xDEAD_BEEF_CAFE_1234,
            size: 65432,
        };
        let encoded = postcard::to_allocvec(&msg).unwrap();
        let decoded: ChatMsg = postcard::from_bytes(&encoded).unwrap();
        match decoded {
            ChatMsg::ImageStart {
                nickname,
                image_id,
                size,
                ..
            } => {
                assert_eq!(nickname, "alice");
                assert_eq!(image_id, 0xDEAD_BEEF_CAFE_1234);
                assert_eq!(size, 65432);
            }
            other => panic!("expected ImageStart, got {other:?}"),
        }
    }

    #[test]
    fn image_start_fits_in_gossip_limit() {
        let msg = ChatMsg::ImageStart {
            from: iroh_base::SecretKey::generate().public(),
            nickname: "a".repeat(200), // 200-char nickname edge case
            image_id: u64::MAX,
            size: usize::MAX,
        };
        let encoded = postcard::to_allocvec(&msg).unwrap();
        assert!(
            encoded.len() < 65_000,
            "ImageStart must fit in gossip limit, got {} bytes",
            encoded.len()
        );
    }
}
