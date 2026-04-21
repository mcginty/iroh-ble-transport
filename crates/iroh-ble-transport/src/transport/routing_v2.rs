//! Shadow routing table v2 — step 1 of the connection-system redesign.
//!
//! See `.claude/plans/2026-04-21-connection-system-study.md` for the full
//! design. In this step the module is *pure instrumentation*: it observes
//! every BLE pipe the driver opens and closes, stamps each with a
//! `StableConnId`, and exposes a snapshot for telemetry. It does not yet
//! participate in routing decisions — the v1 `TransportRouting` +
//! `Registry` continue to own all outbound/inbound address handling.
//!
//! Later steps will graduate `Routing` into the authoritative state by
//! adding pending/routable pools and plumbing `CustomAddr`s through it.
//! The API here is intentionally minimal so subsequent steps can extend
//! it without rewriting.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use blew::DeviceId;
use iroh_base::EndpointId;
use parking_lot::Mutex;

/// Opaque handle iroh sees (wrapped in a `CustomAddr`) for a BLE pipe.
///
/// Monotonic, minted fresh every time a pipe is opened, never reused.
/// Later steps will embed the numeric value into `CustomAddr` payloads so
/// iroh's Connections carry a stable id across the pipe's lifetime.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StableConnId(u64);

impl StableConnId {
    #[must_use]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for StableConnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "conn#{}", self.0)
    }
}

/// Who initiated the BLE ACL link, from the local observer's viewpoint.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Direction {
    /// We dialed (we are central in blew terms).
    Outbound,
    /// Peer dialed us (we are peripheral).
    Inbound,
}

/// Observer-independent "who physically dialed the ACL link."
/// Materialized only after a handshake yields both endpoint_ids.
/// Not used in step 1 — carried here so subsequent steps don't need to
/// redefine it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Dialer {
    /// Lower-endpoint-id peer dialed.
    Low,
    /// Higher-endpoint-id peer dialed.
    High,
}

impl Dialer {
    /// Compute from local post-handshake knowledge. Both sides of a pair
    /// compute the same answer for the same pipe.
    #[must_use]
    pub fn compute(self_id: &EndpointId, remote_id: &EndpointId, direction: Direction) -> Self {
        let self_is_lower = self_id.as_bytes() < remote_id.as_bytes();
        match (self_is_lower, direction) {
            (true, Direction::Outbound) => Dialer::Low,
            (true, Direction::Inbound) => Dialer::High,
            (false, Direction::Outbound) => Dialer::High,
            (false, Direction::Inbound) => Dialer::Low,
        }
    }
}

/// Record of one live BLE pipe.
#[derive(Debug, Clone)]
pub struct Pipe {
    pub id: StableConnId,
    pub device_id: DeviceId,
    pub direction: Direction,
    pub created_at: Instant,
}

/// Shadow routing table. Step 1: tracks pipes only. Future steps add
/// `pending`, `routable`, `scan_hint`, resolver wakers, token payloads.
#[derive(Debug, Default)]
pub struct Routing {
    inner: Mutex<RoutingInner>,
    next_id: AtomicU64,
}

#[derive(Debug, Default)]
struct RoutingInner {
    pipes: HashMap<StableConnId, Pipe>,
}

impl Routing {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a fresh `StableConnId` and record a pipe. Caller should
    /// `evict_pipe` when the pipe's worker task terminates.
    ///
    /// In step 1 this is the only mutator; later steps will add
    /// `set_pending_target`, `promote`, etc.
    pub fn register_pipe(&self, device_id: DeviceId, direction: Direction) -> StableConnId {
        // Tokens are always non-zero. The wrapping guard is defensive —
        // a mobile app session won't exhaust u64, but we don't want a
        // hypothetical overflow silently handing out `StableConnId(0)`
        // which future code might reserve as a sentinel.
        let id = StableConnId(self.next_id.fetch_add(1, Ordering::Relaxed).wrapping_add(1));
        let pipe = Pipe {
            id,
            device_id: device_id.clone(),
            direction,
            created_at: Instant::now(),
        };
        {
            let mut inner = self.inner.lock();
            inner.pipes.insert(id, pipe);
        }
        tracing::debug!(
            %id,
            device = %device_id,
            ?direction,
            "routing_v2: pipe registered"
        );
        id
    }

    /// Remove a pipe. Called when the pipe's worker task exits.
    /// Idempotent — evicting an already-gone id is a no-op.
    pub fn evict_pipe(&self, id: StableConnId) {
        let removed = {
            let mut inner = self.inner.lock();
            inner.pipes.remove(&id)
        };
        if let Some(p) = removed {
            tracing::debug!(
                %id,
                device = %p.device_id,
                lifetime_ms = p.created_at.elapsed().as_millis(),
                "routing_v2: pipe evicted"
            );
        }
    }

    /// Borrow-less snapshot of the current pipe population. Intended for
    /// metrics/telemetry; must not be used for routing.
    #[must_use]
    pub fn snapshot(&self) -> RoutingSnapshot {
        let inner = self.inner.lock();
        RoutingSnapshot {
            pipes: inner.pipes.len(),
        }
    }

    /// Debug helper — list current pipes (for tests and tracing).
    /// Avoid in hot paths.
    #[must_use]
    pub fn pipes_for_debug(&self) -> Vec<Pipe> {
        self.inner.lock().pipes.values().cloned().collect()
    }
}

/// Counts-only view of the shadow routing state. Does not leak any
/// identifiers; callers that need identifiers take the explicit
/// `pipes_for_debug` path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RoutingSnapshot {
    pub pipes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(s: &str) -> DeviceId {
        DeviceId::from(s)
    }

    #[test]
    fn mint_is_monotonic_and_nonzero() {
        let r = Routing::new();
        let a = r.register_pipe(dev("a"), Direction::Outbound);
        let b = r.register_pipe(dev("b"), Direction::Inbound);
        let c = r.register_pipe(dev("c"), Direction::Outbound);
        assert_ne!(a.as_u64(), 0, "id 0 reserved as sentinel");
        assert!(a.as_u64() < b.as_u64());
        assert!(b.as_u64() < c.as_u64());
    }

    #[test]
    fn register_and_evict_balances_snapshot() {
        let r = Routing::new();
        assert_eq!(r.snapshot().pipes, 0);

        let id1 = r.register_pipe(dev("one"), Direction::Outbound);
        let id2 = r.register_pipe(dev("two"), Direction::Inbound);
        assert_eq!(r.snapshot().pipes, 2);

        r.evict_pipe(id1);
        assert_eq!(r.snapshot().pipes, 1);

        r.evict_pipe(id2);
        assert_eq!(r.snapshot().pipes, 0);
    }

    #[test]
    fn evict_is_idempotent_for_unknown_id() {
        let r = Routing::new();
        let id = r.register_pipe(dev("peer"), Direction::Outbound);
        r.evict_pipe(id);
        r.evict_pipe(id); // no panic, no underflow
        assert_eq!(r.snapshot().pipes, 0);
    }

    #[test]
    fn stable_conn_ids_are_not_reused_after_evict() {
        let r = Routing::new();
        let first = r.register_pipe(dev("peer"), Direction::Outbound);
        r.evict_pipe(first);
        let second = r.register_pipe(dev("peer"), Direction::Outbound);
        assert_ne!(first, second, "StableConnId must not recycle");
        assert!(second.as_u64() > first.as_u64());
    }

    #[test]
    fn pipes_for_debug_reflects_register_and_evict() {
        let r = Routing::new();
        let id1 = r.register_pipe(dev("x"), Direction::Outbound);
        let id2 = r.register_pipe(dev("y"), Direction::Inbound);

        let mut pipes = r.pipes_for_debug();
        pipes.sort_by_key(|p| p.id);
        assert_eq!(pipes.len(), 2);
        assert_eq!(pipes[0].id, id1);
        assert_eq!(pipes[0].direction, Direction::Outbound);
        assert_eq!(pipes[1].id, id2);
        assert_eq!(pipes[1].direction, Direction::Inbound);

        r.evict_pipe(id1);
        let pipes = r.pipes_for_debug();
        assert_eq!(pipes.len(), 1);
        assert_eq!(pipes[0].id, id2);
    }

    #[test]
    fn dialer_compute_is_observer_symmetric() {
        let low = iroh_base::SecretKey::from_bytes(&[0x01u8; 32]).public();
        let high = iroh_base::SecretKey::from_bytes(&[0xFEu8; 32]).public();
        assert!(low.as_bytes() < high.as_bytes(), "test invariant");

        // Pipe A: low dialed high. Low-side sees Outbound, high-side sees Inbound.
        // Both sides should compute Dialer::Low.
        assert_eq!(
            Dialer::compute(&low, &high, Direction::Outbound),
            Dialer::Low
        );
        assert_eq!(
            Dialer::compute(&high, &low, Direction::Inbound),
            Dialer::Low
        );

        // Pipe B: high dialed low. High-side sees Outbound, low-side sees Inbound.
        // Both sides should compute Dialer::High.
        assert_eq!(
            Dialer::compute(&high, &low, Direction::Outbound),
            Dialer::High
        );
        assert_eq!(
            Dialer::compute(&low, &high, Direction::Inbound),
            Dialer::High
        );
    }
}
