use std::sync::Arc;

use blew::DeviceId;
use iroh::endpoint::{AfterHandshakeOutcome, Connection, EndpointHooks, VarInt};
use iroh_base::{EndpointId, TransportAddr};
use tokio::sync::mpsc;

use crate::transport::conns::{
    BLE_CLOSE_CODE_CONFLICT, BLE_CLOSE_REASON_CONFLICT, ConnHandle, ConnectionRegistry,
    close_evicted_pipes,
};
use crate::transport::routing::parse_token_addr;
use crate::transport::routing::{PromoteOutcome, Routing, StableConnId};
use crate::transport::transport::BLE_TRANSPORT_ID;

/// `EndpointHooks` implementation that runs the pending→routable
/// promotion rule against `routing` the instant iroh's TLS
/// handshake binds a connection to an `EndpointId`.
///
/// Obtain an instance via [`BleTransport::dedup_hook`]:
///
/// ```no_run
/// # use iroh_ble_transport::BleTransport;
/// # use iroh::{Endpoint, SecretKey, endpoint::presets};
/// # async fn example() -> anyhow::Result<()> {
/// # let secret_key = SecretKey::generate();
/// let ble = BleTransport::builder().build(secret_key.public()).await?;
///
/// Endpoint::builder(presets::N0DisableRelay)
///     .hooks(ble.dedup_hook())
///     .add_custom_transport(ble.as_custom_transport())
///     .address_lookup(ble.address_lookup())
///     .secret_key(secret_key)
///     .bind()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// [`BleTransport::dedup_hook`]: crate::BleTransport::dedup_hook
///
/// CAVEAT: never store an `Endpoint` on the hook — doing so creates an Arc
/// cycle because the endpoint holds the hook via its own Arc. The `Arc`s
/// this struct holds all point at transport-owned state, not at the
/// endpoint, so there's no cycle.
#[derive(Debug, Clone)]
pub(crate) enum HookEvent {
    VerifiedEndpoint {
        endpoint_id: EndpointId,
        token: Option<u64>,
        /// `DeviceId`s of pipes the `promote()` rule evicted to make room
        /// for this handshake. The forwarder on the transport side turns
        /// each into a `PeerCommand::Stalled` so the old BLE pipes get
        /// drained and closed rather than zombied until their own
        /// `LinkDead` detection or BLE ACL drop.
        evicted_devices: Vec<DeviceId>,
    },
    ConnectionClosed {
        endpoint_id: EndpointId,
        stable_id: StableConnId,
    },
}

#[derive(Debug, Clone)]
pub struct BleDedupHook {
    self_endpoint: EndpointId,
    routing: Arc<Routing>,
    tx: mpsc::UnboundedSender<HookEvent>,
    connections: Arc<ConnectionRegistry>,
}

impl BleDedupHook {
    #[must_use]
    pub(crate) fn new(
        self_endpoint: EndpointId,
        routing: Arc<Routing>,
        tx: mpsc::UnboundedSender<HookEvent>,
        connections: Arc<ConnectionRegistry>,
    ) -> Self {
        Self {
            self_endpoint,
            routing,
            tx,
            connections,
        }
    }
}

impl EndpointHooks for BleDedupHook {
    async fn after_handshake<'a>(&'a self, conn: &'a Connection) -> AfterHandshakeOutcome {
        let remote_endpoint = conn.remote_id();
        // `paths()` only ever lists open paths, so no closed-path filter here.
        let token = conn
            .paths()
            .iter()
            .find_map(|path| match path.remote_addr() {
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
        let mut close_watch: Option<(StableConnId, u64)> = None;
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
                    let _ = self.tx.send(HookEvent::VerifiedEndpoint {
                        endpoint_id: remote_endpoint,
                        token: Some(token),
                        evicted_devices: Vec::new(),
                    });
                    return AfterHandshakeOutcome::Reject {
                        error_code: VarInt::from_u32(BLE_CLOSE_CODE_CONFLICT),
                        reason: BLE_CLOSE_REASON_CONFLICT.to_vec(),
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
                    // Close the connections riding the evicted pipes
                    // before the BLE teardown the forwarder kicks off
                    // below: the pipe under them is about to disappear,
                    // and iroh has no way to observe that.
                    close_evicted_pipes(&self.connections, &evicted);
                    let watch_id = self.connections.insert(
                        remote_endpoint,
                        stable_id,
                        Arc::new(conn.weak_handle()) as Arc<dyn ConnHandle>,
                    );
                    close_watch = Some((stable_id, watch_id));
                }
            }
        }

        // Forward to the actor loop; the forwarder on the transport
        // side dispatches `PeerCommand::Stalled` for each evicted
        // device so its pipe worker is properly drained.
        let _ = self.tx.send(HookEvent::VerifiedEndpoint {
            endpoint_id: remote_endpoint,
            token,
            evicted_devices,
        });
        if let Some((stable_id, watch_id)) = close_watch {
            // Never clone the `Connection` out of the hook: a strong handle
            // keeps it alive and disables close-on-drop for its real owner.
            let closed = conn.weak_handle().closed();
            let tx = self.tx.clone();
            let connections = Arc::clone(&self.connections);
            tokio::spawn(async move {
                let _ = closed.await;
                if connections.remove_and_is_empty(remote_endpoint, stable_id, watch_id) {
                    let _ = tx.send(HookEvent::ConnectionClosed {
                        endpoint_id: remote_endpoint,
                        stable_id,
                    });
                }
            });
        }
        AfterHandshakeOutcome::Accept
    }
}
