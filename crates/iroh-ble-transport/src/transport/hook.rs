use std::sync::Arc;

use iroh::endpoint::{AfterHandshakeOutcome, ConnectionInfo, EndpointHooks};
use iroh_base::{EndpointId, TransportAddr};
use tokio::sync::mpsc;

use crate::transport::routing::parse_token_addr;
use crate::transport::routing_v2::{PromoteOutcome, Routing, StableConnId};
use crate::transport::transport::BLE_TRANSPORT_ID;

/// `EndpointHooks` implementation that runs the pending→routable
/// promotion rule against `routing_v2` the instant iroh's TLS
/// handshake binds a connection to an `EndpointId`. See §9.5 of the
/// study doc for the rule.
///
/// Install alongside `add_custom_transport`:
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
/// let hook = BleDedupHook::new(
///     id,
///     transport.routing_v2_handle(),
///     tx,
/// );
/// iroh::Endpoint::builder()
///     .hooks(hook)
///     .add_custom_transport(transport)
///     .bind().await?;
/// # Ok(())
/// # }
/// ```
///
/// CAVEAT: never store an `Endpoint` on the hook — doing so creates an Arc
/// cycle because the endpoint holds the hook via its own Arc. The `Arc`s
/// this struct holds all point at transport-owned state, not at the
/// endpoint, so there's no cycle.
#[derive(Debug, Clone, Copy)]
pub struct VerifiedEndpointEvent {
    pub endpoint_id: EndpointId,
    pub token: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct BleDedupHook {
    self_endpoint: EndpointId,
    routing_v2: Arc<Routing>,
    tx: mpsc::UnboundedSender<VerifiedEndpointEvent>,
}

impl BleDedupHook {
    #[must_use]
    pub fn new(
        self_endpoint: EndpointId,
        routing_v2: Arc<Routing>,
        tx: mpsc::UnboundedSender<VerifiedEndpointEvent>,
    ) -> Self {
        Self {
            self_endpoint,
            routing_v2,
            tx,
        }
    }
}

impl EndpointHooks for BleDedupHook {
    async fn after_handshake<'a>(&'a self, conn: &'a ConnectionInfo) -> AfterHandshakeOutcome {
        let remote_endpoint = conn.remote_id();
        let token = conn
            .selected_path()
            .and_then(|path| match path.remote_addr() {
                TransportAddr::Custom(addr) if addr.id() == BLE_TRANSPORT_ID => {
                    parse_token_addr(addr).ok()
                }
                _ => None,
            });

        // Run the promotion rule synchronously. This is the only place
        // routing_v2's authority invariants are established — reject
        // here is the one mechanism for enforcing "one routable entry
        // per peer" against symmetric-dial or re-dial collisions.
        //
        // Step 4b: the hook reports Accept/Reject to iroh. Step 4c
        // will also coordinate tearing down pipes we evicted + the
        // pipes attached to rejected connections (iroh closes QUIC on
        // reject, but our BLE pipe survives until the peer's ACL
        // drops or its own close logic fires).
        if let Some(token) = token {
            let stable_id = StableConnId::from_raw(token);
            match self
                .routing_v2
                .promote(stable_id, &self.self_endpoint, remote_endpoint)
            {
                PromoteOutcome::Rejected => {
                    tracing::info!(
                        %remote_endpoint,
                        %stable_id,
                        "BleDedupHook: rejecting handshake (promotion rule said so)"
                    );
                    // Notify the actor loop so downstream state
                    // (legacy v1 dedup) can settle symmetrically.
                    let _ = self.tx.send(VerifiedEndpointEvent {
                        endpoint_id: remote_endpoint,
                        token: Some(token),
                    });
                    return AfterHandshakeOutcome::Reject {
                        error_code: noq_proto::VarInt::from_u32(0),
                        reason: b"ble_conflict".to_vec(),
                    };
                }
                PromoteOutcome::Accepted { evicted } => {
                    // Info: one per peer per handshake — this is the
                    // moment a peer becomes authoritatively routable
                    // in v2. Lifecycle-sized cadence, very useful for
                    // cross-referencing with symmetric-dial rejects.
                    tracing::info!(
                        %remote_endpoint,
                        %stable_id,
                        evicted_count = evicted.len(),
                        "BleDedupHook: promoted to routable"
                    );
                    // evicted pipes need to be torn down by the
                    // driver; wired in step 4c via a side channel.
                    // For 4b: pipes stay alive but their routable
                    // slot is gone, so outbound routing (after step
                    // 4c) skips them naturally.
                }
            }
        }

        // Forward to the actor loop for backward-compatible v1 flows.
        let _ = self.tx.send(VerifiedEndpointEvent {
            endpoint_id: remote_endpoint,
            token,
        });
        AfterHandshakeOutcome::Accept
    }
}
