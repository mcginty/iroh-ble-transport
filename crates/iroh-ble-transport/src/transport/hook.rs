use iroh::endpoint::{AfterHandshakeOutcome, ConnectionInfo, EndpointHooks};
use iroh_base::EndpointId;
use tokio::sync::mpsc;

/// `EndpointHooks` implementation that forwards each verified [`EndpointId`]
/// to [`BleTransport`] for connection dedup. Install alongside
/// `add_custom_transport`:
///
/// ```no_run
/// # use iroh_ble_transport::{BleDedupHook, BleTransport, BleTransportConfig};
/// # use std::sync::Arc;
/// # async fn example() -> anyhow::Result<()> {
/// # let (id, c, p) = todo!();
/// let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
/// let transport = BleTransport::with_config(id, c, p, BleTransportConfig {
///     verified_rx: Some(rx),
///     ..Default::default()
/// }).await?;
/// iroh::Endpoint::builder()
///     .hooks(BleDedupHook::new(tx))
///     .add_custom_transport(transport)
///     .bind().await?;
/// # Ok(())
/// # }
/// ```
///
/// CAVEAT: never store an `Endpoint` on the hook — doing so creates an Arc
/// cycle because the endpoint holds the hook via its own Arc. This struct only
/// stores the mpsc sender.
#[derive(Debug, Clone)]
pub struct BleDedupHook {
    tx: mpsc::UnboundedSender<EndpointId>,
}

impl BleDedupHook {
    #[must_use]
    pub fn new(tx: mpsc::UnboundedSender<EndpointId>) -> Self {
        Self { tx }
    }
}

impl EndpointHooks for BleDedupHook {
    async fn after_handshake<'a>(&'a self, conn: &'a ConnectionInfo) -> AfterHandshakeOutcome {
        let _ = self.tx.send(conn.remote_id());
        AfterHandshakeOutcome::Accept
    }
}
