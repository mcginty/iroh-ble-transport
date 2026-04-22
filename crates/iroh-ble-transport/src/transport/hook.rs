use std::sync::Arc;

use blew::DeviceId;
use iroh::endpoint::{AfterHandshakeOutcome, ConnectionInfo, EndpointHooks};
use iroh_base::{EndpointId, TransportAddr};
use tokio::sync::mpsc;

use crate::transport::routing::parse_token_addr;
use crate::transport::routing::{PromoteOutcome, Routing, StableConnId};
use crate::transport::transport::BLE_TRANSPORT_ID;

/// `EndpointHooks` implementation that runs the pending→routable
/// promotion rule against `routing` the instant iroh's TLS
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
///     transport.routing_handle(),
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
#[derive(Debug, Clone)]
pub struct VerifiedEndpointEvent {
    pub endpoint_id: EndpointId,
    pub token: Option<u64>,
    /// `DeviceId`s of pipes the `promote()` rule evicted to make room
    /// for this handshake. The forwarder on the transport side turns
    /// each into a `PeerCommand::Stalled` so the old BLE pipes get
    /// drained and closed rather than zombied until their own
    /// `LinkDead` detection or BLE ACL drop.
    pub evicted_devices: Vec<DeviceId>,
}

#[derive(Debug, Clone)]
pub struct BleDedupHook {
    self_endpoint: EndpointId,
    routing: Arc<Routing>,
    tx: mpsc::UnboundedSender<VerifiedEndpointEvent>,
}

impl BleDedupHook {
    #[must_use]
    pub fn new(
        self_endpoint: EndpointId,
        routing: Arc<Routing>,
        tx: mpsc::UnboundedSender<VerifiedEndpointEvent>,
    ) -> Self {
        Self {
            self_endpoint,
            routing,
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

        // Run the promotion rule synchronously. This is the only
        // place routing's authority invariants are established.
        // Same-positional-category (peer re-dialed us) always evicts
        // the old and accepts the new — the new handshake completing
        // is authoritative evidence that the peer abandoned the old
        // connection. The evicted pipes' DeviceIds are forwarded so
        // the registry can `Stalled` them into teardown.
        let mut evicted_devices: Vec<DeviceId> = Vec::new();
        if let Some(token) = token {
            let stable_id = StableConnId::from_raw(token);
            match self
                .routing
                .promote(stable_id, &self.self_endpoint, remote_endpoint)
            {
                PromoteOutcome::Rejected => {
                    tracing::info!(
                        %remote_endpoint,
                        %stable_id,
                        "BleDedupHook: rejecting handshake (promotion rule said so)"
                    );
                    let _ = self.tx.send(VerifiedEndpointEvent {
                        endpoint_id: remote_endpoint,
                        token: Some(token),
                        evicted_devices: Vec::new(),
                    });
                    return AfterHandshakeOutcome::Reject {
                        error_code: noq_proto::VarInt::from_u32(0),
                        reason: b"ble_conflict".to_vec(),
                    };
                }
                PromoteOutcome::Accepted { evicted } => {
                    // Resolve evicted StableConnIds → DeviceIds for
                    // teardown. We do this before logging so the log
                    // can show how many pipes actually had devices
                    // registered (edge case: a pipe might have been
                    // evicted between the promote call and now).
                    for id in &evicted {
                        if let Some(dev) = self.routing.device_for_pipe(*id) {
                            evicted_devices.push(dev);
                        }
                    }
                    tracing::info!(
                        %remote_endpoint,
                        %stable_id,
                        evicted_count = evicted.len(),
                        evicted_devices = evicted_devices.len(),
                        "BleDedupHook: promoted to routable"
                    );
                }
            }
        }

        // Forward to the actor loop; the forwarder on the transport
        // side dispatches `PeerCommand::Stalled` for each evicted
        // device so its pipe worker is properly drained.
        let _ = self.tx.send(VerifiedEndpointEvent {
            endpoint_id: remote_endpoint,
            token,
            evicted_devices,
        });
        AfterHandshakeOutcome::Accept
    }
}
