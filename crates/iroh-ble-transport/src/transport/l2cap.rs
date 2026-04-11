//! L2CAP data-path primitives for `BleTransport`.
//!
//! This module contains pure helpers (framing, identity exchange, I/O task
//! spawning) that operate on `AsyncRead + AsyncWrite` streams. It does not
//! depend on `BleTransportState`, so all helpers are unit-testable with
//! `L2capChannel::pair()`.
//!
//! Wire format:
//! - First 32 bytes in each direction: sender's `EndpointId` (no header).
//! - Each subsequent datagram: `[u16 LE length] [payload bytes]`.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use super::IncomingPacket;

/// Maximum datagram payload that can be sent on an L2CAP stream, in bytes.
///
/// Matches [`super::reliable::MAX_DATAGRAM_SIZE`] so the two transport paths
/// expose the same datagram size ceiling to iroh.
pub(super) const MAX_DATAGRAM_SIZE: usize = 1472;

pub(super) async fn write_framed_datagram<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    payload: &[u8],
) -> io::Result<()> {
    if payload.len() > MAX_DATAGRAM_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "datagram exceeds MAX_DATAGRAM_SIZE",
        ));
    }
    let len = u16::try_from(payload.len()).unwrap();
    writer.write_all(&len.to_le_bytes()).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Returns `Ok(None)` on clean EOF at a frame boundary.
pub(super) async fn read_framed_datagram<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 2];
    // Separate first-byte read distinguishes clean EOF from truncated frame.
    if reader.read(&mut len_buf[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut len_buf[1..]).await?;
    let len = u16::from_le_bytes(len_buf) as usize;
    if len > MAX_DATAGRAM_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds MAX_DATAGRAM_SIZE"),
        ));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

pub(super) async fn write_identity<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    id: &[u8; 32],
) -> io::Result<()> {
    writer.write_all(id).await?;
    writer.flush().await?;
    Ok(())
}

pub(super) async fn read_identity<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    timeout: std::time::Duration,
) -> io::Result<[u8; 32]> {
    let mut buf = [0u8; 32];
    match tokio::time::timeout(timeout, reader.read_exact(&mut buf)).await {
        Ok(Ok(_)) => Ok(buf),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "timed out waiting for identity",
        )),
    }
}

struct NotifyOnDrop(Arc<Notify>);

impl Drop for NotifyOnDrop {
    fn drop(&mut self) {
        self.0.notify_waiters();
    }
}

pub(super) fn spawn_l2cap_io_tasks<R, W>(
    reader: R,
    writer: W,
    device_key: String,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    bytes_tx: Arc<AtomicU64>,
    bytes_rx: Arc<AtomicU64>,
) -> (
    mpsc::Sender<Vec<u8>>,
    JoinHandle<()>,
    JoinHandle<()>,
    Arc<Notify>,
)
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Vec<u8>>(64);

    let done = Arc::new(Notify::new());

    let dk = device_key.clone();
    let send_guard = NotifyOnDrop(Arc::clone(&done));
    let mut writer = writer;
    let send_task = tokio::spawn(async move {
        let _guard = send_guard;
        while let Some(datagram) = outbound_rx.recv().await {
            let len = datagram.len() as u64 + 2;
            if let Err(e) = write_framed_datagram(&mut writer, &datagram).await {
                warn!(device = %dk, ?e, "l2cap send task exiting on error");
                break;
            }
            bytes_tx.fetch_add(len, Ordering::Relaxed);
        }
        debug!(device = %dk, "l2cap send task exiting");
    });

    let recv_guard = NotifyOnDrop(Arc::clone(&done));
    let mut reader = reader;
    let recv_task = tokio::spawn(async move {
        let _guard = recv_guard;
        loop {
            match read_framed_datagram(&mut reader).await {
                Ok(Some(data)) => {
                    let len = data.len() as u64 + 2;
                    bytes_rx.fetch_add(len, Ordering::Relaxed);
                    if incoming_tx
                        .send(IncomingPacket {
                            device_key: device_key.clone(),
                            data,
                        })
                        .await
                        .is_err()
                    {
                        debug!(device = %device_key, "incoming_tx closed, exiting recv task");
                        break;
                    }
                }
                Ok(None) => {
                    debug!(device = %device_key, "l2cap recv task EOF");
                    break;
                }
                Err(e) => {
                    warn!(device = %device_key, ?e, "l2cap recv task exiting on error");
                    break;
                }
            }
        }
    });

    (outbound_tx, send_task, recv_task, done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use blew::l2cap::L2capChannel;
    use tokio::io::split;

    #[tokio::test]
    async fn round_trip_single_datagram() {
        let (a, b) = L2capChannel::pair(8192);
        let (mut a_rd, mut a_wr) = split(a);
        let (mut b_rd, mut b_wr) = split(b);

        let payload = b"hello world".to_vec();
        write_framed_datagram(&mut a_wr, &payload).await.unwrap();
        let got = read_framed_datagram(&mut b_rd).await.unwrap().unwrap();
        assert_eq!(got, payload);

        let payload2 = b"reply".to_vec();
        write_framed_datagram(&mut b_wr, &payload2).await.unwrap();
        let got2 = read_framed_datagram(&mut a_rd).await.unwrap().unwrap();
        assert_eq!(got2, payload2);
    }

    #[tokio::test]
    async fn back_to_back_datagrams() {
        let (a, b) = L2capChannel::pair(8192);
        let (_a_rd, mut a_wr) = split(a);
        let (mut b_rd, _b_wr) = split(b);

        for i in 0_u8..5 {
            let payload = vec![i; 10];
            write_framed_datagram(&mut a_wr, &payload).await.unwrap();
        }
        for i in 0_u8..5 {
            let got = read_framed_datagram(&mut b_rd).await.unwrap().unwrap();
            assert_eq!(got, vec![i; 10]);
        }
    }

    #[tokio::test]
    async fn oversized_payload_rejected_by_writer() {
        let (a, _b) = L2capChannel::pair(8192);
        let (_rd, mut wr) = split(a);
        let too_big = vec![0_u8; MAX_DATAGRAM_SIZE + 1];
        let err = write_framed_datagram(&mut wr, &too_big).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn clean_eof_returns_none() {
        let (a, b) = L2capChannel::pair(8192);
        let (_a_rd, mut a_wr) = split(a);
        let (mut b_rd, _b_wr) = split(b);
        a_wr.shutdown().await.unwrap();
        let got = read_framed_datagram(&mut b_rd).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn truncated_length_prefix_is_error() {
        let (a, b) = L2capChannel::pair(8192);
        let (_a_rd, mut a_wr) = split(a);
        let (mut b_rd, _b_wr) = split(b);
        a_wr.write_all(&[0x01]).await.unwrap();
        a_wr.shutdown().await.unwrap();
        let err = read_framed_datagram(&mut b_rd).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn truncated_payload_is_error() {
        let (a, b) = L2capChannel::pair(8192);
        let (_a_rd, mut a_wr) = split(a);
        let (mut b_rd, _b_wr) = split(b);
        a_wr.write_all(&10_u16.to_le_bytes()).await.unwrap();
        a_wr.write_all(&[1, 2, 3]).await.unwrap();
        a_wr.shutdown().await.unwrap();
        let err = read_framed_datagram(&mut b_rd).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn exchange_identities_symmetric() {
        let (a, b) = L2capChannel::pair(8192);
        let (mut a_rd, mut a_wr) = split(a);
        let (mut b_rd, mut b_wr) = split(b);

        let id_a = [1_u8; 32];
        let id_b = [2_u8; 32];

        let fut_a = async {
            write_identity(&mut a_wr, &id_a).await.unwrap();
            read_identity(&mut a_rd, std::time::Duration::from_secs(1))
                .await
                .unwrap()
        };
        let fut_b = async {
            write_identity(&mut b_wr, &id_b).await.unwrap();
            read_identity(&mut b_rd, std::time::Duration::from_secs(1))
                .await
                .unwrap()
        };
        let (got_a, got_b) = tokio::join!(fut_a, fut_b);
        assert_eq!(got_a, id_b);
        assert_eq!(got_b, id_a);
    }

    #[tokio::test]
    async fn read_identity_times_out_if_peer_never_writes() {
        let (_a, b) = L2capChannel::pair(8192);
        let (mut b_rd, _b_wr) = split(b);
        let err = read_identity(&mut b_rd, std::time::Duration::from_millis(50))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn spawn_l2cap_io_tasks_round_trips_datagrams() {
        use tokio::sync::mpsc;

        let (central_side, peripheral_side) = L2capChannel::pair(8192);

        let (a_rd, a_wr) = split(central_side);
        let (incoming_tx, mut incoming_rx) = mpsc::channel(16);
        let device_key = "test-device".to_string();
        let (tx, _send_task, _recv_task, _done) = super::spawn_l2cap_io_tasks(
            a_rd,
            a_wr,
            device_key.clone(),
            incoming_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        );

        let (mut b_rd, mut b_wr) = split(peripheral_side);

        tx.send(b"ping".to_vec()).await.unwrap();
        let got = super::read_framed_datagram(&mut b_rd)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got, b"ping");

        super::write_framed_datagram(&mut b_wr, b"pong")
            .await
            .unwrap();
        let pkt = tokio::time::timeout(std::time::Duration::from_secs(1), incoming_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pkt.data, b"pong");
        assert_eq!(pkt.device_key, device_key);
    }

    #[tokio::test]
    async fn read_identity_errors_on_short_stream() {
        let (a, b) = L2capChannel::pair(8192);
        let (_a_rd, mut a_wr) = split(a);
        a_wr.write_all(&[1_u8; 10]).await.unwrap();
        a_wr.shutdown().await.unwrap();
        drop(a_wr);
        let (mut b_rd, _b_wr) = split(b);
        let err = read_identity(&mut b_rd, std::time::Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn done_notify_fires_on_task_exit() {
        let (a, b) = L2capChannel::pair(8192);
        let (a_rd, a_wr) = split(a);
        let (incoming_tx, _incoming_rx) = mpsc::channel(16);
        let (_tx, _send_task, _recv_task, done) = super::spawn_l2cap_io_tasks(
            a_rd,
            a_wr,
            "test-device".to_string(),
            incoming_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        );

        drop(b);

        tokio::time::timeout(std::time::Duration::from_secs(1), done.notified())
            .await
            .expect("done notify should fire when I/O task exits");
    }

    #[tokio::test]
    async fn done_notify_fires_on_task_abort() {
        let (a, _b) = L2capChannel::pair(8192);
        let (a_rd, a_wr) = split(a);
        let (incoming_tx, _incoming_rx) = mpsc::channel(16);
        let (_tx, send_task, recv_task, done) = super::spawn_l2cap_io_tasks(
            a_rd,
            a_wr,
            "test-device".to_string(),
            incoming_tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        );

        send_task.abort();
        recv_task.abort();

        tokio::time::timeout(std::time::Duration::from_secs(1), done.notified())
            .await
            .expect("done notify should fire when tasks are aborted");
    }
}
