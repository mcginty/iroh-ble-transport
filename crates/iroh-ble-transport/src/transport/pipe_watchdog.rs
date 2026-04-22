//! Pipe-lifetime watchdog (step 5 of the connection-system redesign).
//!
//! Polls iroh's `Endpoint::remote_info` for every routable entry and
//! tears the BLE pipe down when iroh reports the Connection has no
//! active path. Without this, BLE pipes linger past QUIC death for up
//! to `max_idle_timeout` (15 s in the chat app, 300 s upstream) — a
//! holdover that pins radio time and confuses reconnect logic.
//!
//! The watchdog is *external observation*: it runs independently of
//! the registry/driver, never blocks BLE I/O, and only tells the
//! registry "the pipe for this device looks dead to iroh; please
//! drain." The registry's normal `Stalled` path does the rest (drain,
//! close channel, which causes the pipe worker to exit, which evicts
//! routing_v2 state).
//!
//! See `.claude/plans/2026-04-21-connection-system-study.md` §9.6 for
//! the full rationale.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use iroh_base::EndpointId;
use tokio::sync::mpsc;

use crate::transport::peer::PeerCommand;
use crate::transport::routing_v2::Routing;

/// Default poll cadence. Five seconds is well under the chat app's
/// 15 s `max_idle_timeout` so we catch idle-timeouts promptly without
/// hammering iroh's remote-info API.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Grace window: freshly-promoted routable entries are exempt from
/// watchdog teardown for this long. Gives iroh-gossip time to emit
/// `NeighborUp` + our first application traffic before the watchdog
/// can second-guess liveness. Set conservatively — if a pipe is truly
/// dead by 10 s post-promotion, `max_idle_timeout` still catches it.
pub const DEFAULT_GRACE_PERIOD: Duration = Duration::from_secs(10);

/// Abstraction over iroh's `Endpoint::remote_info` so the watchdog is
/// testable without spinning up a real Endpoint. Implemented for
/// `iroh::Endpoint` via `impl_for_iroh_endpoint`.
#[async_trait]
pub trait PipeWatchdogEndpoint: Send + Sync + 'static {
    /// Returns `true` if iroh currently has an active path to
    /// `endpoint_id`. The watchdog treats a `false` result as "iroh
    /// has given up on this Connection."
    async fn has_active_path(&self, endpoint_id: EndpointId) -> bool;
}

#[async_trait]
impl PipeWatchdogEndpoint for iroh::Endpoint {
    async fn has_active_path(&self, endpoint_id: EndpointId) -> bool {
        match self.remote_info(endpoint_id).await {
            Some(info) => info
                .addrs()
                .any(|a| matches!(a.usage(), iroh::endpoint::TransportAddrUsage::Active)),
            None => false,
        }
    }
}

/// Spawn the pipe-lifetime watchdog. The returned `JoinHandle` can be
/// aborted to stop the loop; the watchdog exits on its own when the
/// registry inbox closes.
///
/// `grace_period` short-circuits teardown for routable entries whose
/// `verified_at` is younger than the window, preventing the watchdog
/// from racing iroh's post-handshake setup.
pub fn spawn<E: PipeWatchdogEndpoint>(
    endpoint: Arc<E>,
    routing_v2: Arc<Routing>,
    registry_inbox: mpsc::Sender<PeerCommand>,
    poll_interval: Duration,
    grace_period: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(
        endpoint,
        routing_v2,
        registry_inbox,
        poll_interval,
        grace_period,
    ))
}

async fn run<E: PipeWatchdogEndpoint>(
    endpoint: Arc<E>,
    routing_v2: Arc<Routing>,
    registry_inbox: mpsc::Sender<PeerCommand>,
    poll_interval: Duration,
    grace_period: Duration,
) {
    // Small initial delay so newly-bound transports don't trip on
    // zero-state. Also avoids a thundering-herd of
    // `remote_info` calls at startup.
    tokio::time::sleep(poll_interval).await;
    loop {
        tick(
            endpoint.as_ref(),
            &routing_v2,
            &registry_inbox,
            grace_period,
            Instant::now(),
        )
        .await;
        if registry_inbox.is_closed() {
            break;
        }
        tokio::time::sleep(poll_interval).await;
    }
}

/// One pass of the watchdog loop. Exposed for tests so they can drive
/// the rule deterministically without waiting for the sleep.
pub(crate) async fn tick<E: PipeWatchdogEndpoint>(
    endpoint: &E,
    routing_v2: &Routing,
    registry_inbox: &mpsc::Sender<PeerCommand>,
    grace_period: Duration,
    now: Instant,
) {
    let entries = routing_v2.routable_entries();
    for entry in entries {
        // Skip freshly-promoted entries so iroh has time to exchange
        // initial traffic before we declare the path dead.
        if now.saturating_duration_since(entry.verified_at) < grace_period {
            continue;
        }
        if endpoint.has_active_path(entry.endpoint_id).await {
            continue;
        }
        tracing::info!(
            endpoint_id = %entry.endpoint_id,
            stable_id = %entry.stable_id,
            device = %entry.device_id,
            "pipe_watchdog: iroh reports no active path; tearing pipe down"
        );
        // Synchronously evict from the routable pool so subsequent
        // `poll_send` for this endpoint fails fast. The pending pool
        // is uncommon but safe to clean up as well.
        if routing_v2.evict_routable(&entry.endpoint_id).is_some() {
            routing_v2.evict_pipe_state(entry.stable_id);
        }
        // Ask the registry to drain the BLE pipe — this fires
        // `CloseChannel`, which ends the pipe worker, which in turn
        // evicts the pipe record from `routing_v2.pipes`.
        if registry_inbox
            .send(PeerCommand::Stalled {
                device_id: entry.device_id,
            })
            .await
            .is_err()
        {
            // Inbox closed; the transport is shutting down.
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::peer::LivenessClock;
    use crate::transport::routing_v2::{Dialer, Direction, StableConnId};
    use std::collections::HashSet;
    use std::sync::Mutex;

    struct MockEndpoint {
        active: Mutex<HashSet<EndpointId>>,
    }

    impl MockEndpoint {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                active: Mutex::new(HashSet::new()),
            })
        }

        fn set_active(&self, ep: EndpointId, active: bool) {
            let mut guard = self.active.lock().unwrap();
            if active {
                guard.insert(ep);
            } else {
                guard.remove(&ep);
            }
        }
    }

    #[async_trait]
    impl PipeWatchdogEndpoint for MockEndpoint {
        async fn has_active_path(&self, endpoint_id: EndpointId) -> bool {
            self.active.lock().unwrap().contains(&endpoint_id)
        }
    }

    fn test_endpoint(seed: u8) -> EndpointId {
        iroh_base::SecretKey::from_bytes(&[seed; 32]).public()
    }

    fn dev(s: &str) -> blew::DeviceId {
        blew::DeviceId::from(s)
    }

    #[tokio::test]
    async fn tick_is_noop_when_endpoint_is_active() {
        let routing = Arc::new(Routing::new());
        let ep = test_endpoint(1);
        let id = routing.register_pipe(dev("mac"), Direction::Outbound, LivenessClock::new());
        routing.insert_routable(ep, id, Dialer::Low);

        let endpoint = MockEndpoint::new();
        endpoint.set_active(ep, true);

        let (tx, mut rx) = mpsc::channel::<PeerCommand>(4);
        // Pretend the entry was verified long ago so grace doesn't help.
        tick(
            endpoint.as_ref(),
            &routing,
            &tx,
            Duration::from_secs(10),
            Instant::now() + Duration::from_secs(3600),
        )
        .await;

        assert_eq!(routing.routable_len(), 1, "active path means no eviction");
        // No Stalled command should have been emitted.
        assert!(rx.try_recv().is_err(), "no teardown for active endpoint");
    }

    #[tokio::test]
    async fn tick_evicts_and_stalls_when_endpoint_has_no_active_path() {
        let routing = Arc::new(Routing::new());
        let ep = test_endpoint(2);
        let id = routing.register_pipe(dev("mac-b"), Direction::Outbound, LivenessClock::new());
        routing.insert_routable(ep, id, Dialer::High);

        let endpoint = MockEndpoint::new();
        // endpoint has no active path (default)

        let (tx, mut rx) = mpsc::channel::<PeerCommand>(4);
        tick(
            endpoint.as_ref(),
            &routing,
            &tx,
            Duration::from_secs(10),
            Instant::now() + Duration::from_secs(3600),
        )
        .await;

        assert_eq!(routing.routable_len(), 0, "routable entry evicted");
        match rx.try_recv().expect("Stalled emitted") {
            PeerCommand::Stalled { device_id } => {
                assert_eq!(device_id, dev("mac-b"));
            }
            other => panic!("expected Stalled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tick_respects_grace_period_for_fresh_promotion() {
        // Freshly-promoted routable entries are exempt from teardown so
        // iroh has a chance to exchange initial traffic before the
        // watchdog weighs in.
        let routing = Arc::new(Routing::new());
        let ep = test_endpoint(3);
        let id = routing.register_pipe(dev("mac-c"), Direction::Outbound, LivenessClock::new());
        routing.insert_routable(ep, id, Dialer::Low);

        let endpoint = MockEndpoint::new();
        // Not active.

        let (tx, mut rx) = mpsc::channel::<PeerCommand>(4);
        // `now` is close to `verified_at` — grace applies.
        tick(
            endpoint.as_ref(),
            &routing,
            &tx,
            Duration::from_secs(10),
            Instant::now(),
        )
        .await;

        assert_eq!(
            routing.routable_len(),
            1,
            "entry inside grace window must not be evicted"
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn tick_handles_empty_routable_pool() {
        let routing = Arc::new(Routing::new());
        let endpoint = MockEndpoint::new();
        let (tx, mut rx) = mpsc::channel::<PeerCommand>(4);
        tick(
            endpoint.as_ref(),
            &routing,
            &tx,
            Duration::from_secs(10),
            Instant::now(),
        )
        .await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn tick_tears_down_multiple_dead_endpoints_in_one_pass() {
        let routing = Arc::new(Routing::new());
        let ep_a = test_endpoint(4);
        let ep_b = test_endpoint(5);
        let ep_c = test_endpoint(6);
        let ia = routing.register_pipe(dev("a"), Direction::Outbound, LivenessClock::new());
        let ib = routing.register_pipe(dev("b"), Direction::Inbound, LivenessClock::new());
        let ic = routing.register_pipe(dev("c"), Direction::Outbound, LivenessClock::new());
        routing.insert_routable(ep_a, ia, Dialer::Low);
        routing.insert_routable(ep_b, ib, Dialer::High);
        routing.insert_routable(ep_c, ic, Dialer::Low);

        let endpoint = MockEndpoint::new();
        endpoint.set_active(ep_b, true); // only B has an active path

        let (tx, mut rx) = mpsc::channel::<PeerCommand>(8);
        tick(
            endpoint.as_ref(),
            &routing,
            &tx,
            Duration::from_secs(10),
            Instant::now() + Duration::from_secs(3600),
        )
        .await;

        // B survives, A and C are evicted.
        assert_eq!(routing.routable_len(), 1);
        assert_eq!(routing.routable_pipe_for(&ep_b), Some(ib));

        let mut stalled: HashSet<blew::DeviceId> = HashSet::new();
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                PeerCommand::Stalled { device_id } => {
                    stalled.insert(device_id);
                }
                other => panic!("unexpected command {other:?}"),
            }
        }
        assert!(stalled.contains(&dev("a")));
        assert!(stalled.contains(&dev("c")));
        assert!(!stalled.contains(&dev("b")));
    }

    #[tokio::test]
    async fn tick_skips_routable_with_missing_pipe() {
        let routing = Arc::new(Routing::new());
        let ep = test_endpoint(7);
        let id = routing.register_pipe(dev("ghost"), Direction::Outbound, LivenessClock::new());
        routing.insert_routable(ep, id, Dialer::Low);
        // Simulate a race: pipe evicted but routable still present.
        routing.evict_pipe(id);

        let endpoint = MockEndpoint::new();
        // Not active.

        let (tx, mut rx) = mpsc::channel::<PeerCommand>(4);
        tick(
            endpoint.as_ref(),
            &routing,
            &tx,
            Duration::from_secs(10),
            Instant::now() + Duration::from_secs(3600),
        )
        .await;

        // routable_entries() filters out the orphan, so the watchdog
        // should not emit a teardown.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn spawn_stops_when_registry_inbox_closes() {
        let routing = Arc::new(Routing::new());
        let endpoint = MockEndpoint::new();
        let (tx, rx) = mpsc::channel::<PeerCommand>(1);
        drop(rx); // close inbox
        let _ = StableConnId::for_test(0); // keep import live in tests

        let handle = spawn(
            endpoint,
            routing,
            tx,
            Duration::from_millis(10),
            Duration::from_millis(10),
        );
        // Expect the task to exit on its own now that the inbox is closed.
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("watchdog must exit on inbox close")
            .expect("watchdog task must not panic");
    }
}
