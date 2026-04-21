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
use std::task::Waker;
use std::time::Instant;

use blew::DeviceId;
use iroh_base::EndpointId;
use parking_lot::Mutex;

use crate::transport::peer::KeyPrefix;

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

    /// Reconstruct from a raw u64 — used when the hook parses an
    /// iroh-side `CustomAddr` back into a handle. Callers must have
    /// originally obtained the u64 via `as_u64()` on a minted id; no
    /// check is performed that the id corresponds to a live pipe.
    #[must_use]
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Test-only constructor. Production code only obtains `StableConnId`s
    /// via `Routing::register_pipe`, which guarantees monotonicity.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn for_test(n: u64) -> Self {
        Self(n)
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

/// A pipe that exists but whose iroh-TLS identity hasn't been verified
/// yet. Promotes to `Routable` (and leaves the pending pool) when
/// `after_handshake` runs the promotion rule and accepts — see step 4b.
#[derive(Debug, Clone)]
pub struct Pending {
    pub pipe: StableConnId,
    /// `Some(endpoint_id)` when we know what the outbound dial is
    /// targeting (set by the resolver in step 4c). `None` for inbound
    /// accepts — we only learn their target at `after_handshake`.
    pub target_endpoint: Option<EndpointId>,
    pub created_at: Instant,
}

/// A pipe whose iroh-TLS handshake has completed, binding it to an
/// authenticated `EndpointId`. The `routable` pool is the only
/// structure that drives routing decisions — see the authority model
/// in §9.1 of the study doc.
#[derive(Debug, Clone)]
pub struct Routable {
    pub pipe: StableConnId,
    pub endpoint_id: EndpointId,
    /// Observer-independent "who dialed" — materialized at promotion
    /// time from `(self_endpoint, remote_endpoint, direction)`.
    pub dialer: Dialer,
    pub verified_at: Instant,
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
    /// Scan-hint table: `KeyPrefix → DeviceId`. Populated whenever the
    /// central-side event pump sees an advertising packet whose service
    /// UUID matches the iroh-ble prefix shape. Answers "who might be
    /// nearby under this public-key prefix" — a hint for dialing only;
    /// never an authority for routing (see authority model in §9.1 of
    /// the study doc).
    scan_hint: HashMap<KeyPrefix, DeviceId>,
    /// Pipes that exist but haven't been authenticated yet. Mutually
    /// exclusive with `routable` — a pipe is in exactly one pool at
    /// any moment (or neither, briefly, during promotion transitions).
    pending: HashMap<StableConnId, Pending>,
    /// Pipes bound to an authenticated `EndpointId`. At most one
    /// entry per peer (invariant enforced by the promotion rule in
    /// step 4b).
    routable: HashMap<EndpointId, Routable>,
    /// Outbound-dial reservations: the resolver has yielded a
    /// `CustomAddr(stable_id)` to iroh but no pipe exists yet. When
    /// the driver opens a pipe for a prefix with a reservation, it
    /// binds the reserved id to that pipe instead of minting a fresh
    /// one — keeps iroh's `CustomAddr` valid across the dial.
    /// Keyed on `KeyPrefix` because MAC-rotation is common and the
    /// prefix is stable; lookup by `DeviceId` bridges through
    /// `scan_hint`.
    reservations: HashMap<KeyPrefix, Reservation>,
    /// Reverse map from reserved stable_id → its prefix. Used by
    /// `poll_send` to quickly answer "is this stable_id a reservation
    /// waiting for a pipe?" without scanning the whole reservations
    /// map.
    reserved_stable_ids: HashMap<StableConnId, KeyPrefix>,
    /// Wakers parked by `BleAddressLookup::resolve` while it waits
    /// for a routable / pending / reservation entry for its target
    /// endpoint. Keyed on endpoint so different resolvers don't wake
    /// each other spuriously.
    endpoint_wakers: HashMap<EndpointId, Vec<Waker>>,
}

/// A reservation: the resolver promised iroh a `CustomAddr(stable_id)`
/// would eventually route somewhere, and is waiting for a pipe to
/// that promise's target. When the driver opens a pipe to any device
/// whose `scan_hint` prefix matches, it binds `stable_id` to the
/// pipe (instead of minting fresh) so iroh's existing `CustomAddr`
/// keeps working.
#[derive(Debug, Clone)]
pub struct Reservation {
    pub stable_id: StableConnId,
    pub endpoint_id: EndpointId,
    pub reserved_at: Instant,
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
            scan_hints: inner.scan_hint.len(),
            pending: inner.pending.len(),
            routable: inner.routable.len(),
        }
    }

    /// Debug helper — list current pipes (for tests and tracing).
    /// Avoid in hot paths.
    #[must_use]
    pub fn pipes_for_debug(&self) -> Vec<Pipe> {
        self.inner.lock().pipes.values().cloned().collect()
    }

    // ---------- scan_hint: KeyPrefix → DeviceId (dial-hint only) ----------

    /// Record that `prefix` was last seen advertising from `device`.
    /// Overwrites any prior mapping. Hint-only; never an authority for
    /// routing — use pinned/routable state for that.
    ///
    /// Returns the `DeviceId` previously mapped to this prefix (if any),
    /// so callers can forget stale routing state in `routing_v1`.
    pub fn note_scan_hint(&self, prefix: KeyPrefix, device: DeviceId) -> Option<DeviceId> {
        let previous = {
            let mut inner = self.inner.lock();
            let previous = inner.scan_hint.insert(prefix, device.clone());
            if previous.as_ref() != Some(&device) {
                tracing::trace!(
                    ?prefix,
                    device = %device,
                    previous = ?previous,
                    "routing_v2: scan_hint updated"
                );
            }
            previous
        };
        // A fresh scan_hint for a prefix means any resolver parked on
        // an endpoint with this prefix can now make progress. Wake
        // them by iterating the endpoint_wakers map and matching on
        // prefix.
        self.wake_endpoint_waiters_for_prefix(&prefix);
        previous
    }

    fn wake_endpoint_waiters_for_prefix(&self, prefix: &KeyPrefix) {
        let to_wake: Vec<EndpointId> = {
            let inner = self.inner.lock();
            inner
                .endpoint_wakers
                .keys()
                .filter(|ep| crate::transport::routing::prefix_from_endpoint(ep) == *prefix)
                .copied()
                .collect()
        };
        for ep in to_wake {
            self.wake_endpoint_waiters(&ep);
        }
    }

    /// Clear the scan hint for `prefix`. Idempotent. Used when scan
    /// tells us a prefix went away (e.g., peer left range) or when v1
    /// evicts a mapping via `forget_device`.
    pub fn forget_scan_hint(&self, prefix: &KeyPrefix) {
        let mut inner = self.inner.lock();
        if inner.scan_hint.remove(prefix).is_some() {
            tracing::trace!(?prefix, "routing_v2: scan_hint cleared");
        }
    }

    /// Look up the DeviceId most recently seen advertising under this
    /// prefix. Step 4 will consume this from the resolver to decide
    /// whether a dial can be initiated.
    #[must_use]
    pub fn scan_hint_for_prefix(&self, prefix: &KeyPrefix) -> Option<DeviceId> {
        self.inner.lock().scan_hint.get(prefix).cloned()
    }

    // ---------- pending / routable pool bookkeeping (step 4a) ----------

    /// Register a newly-opened pipe as pending. Called by the driver
    /// right after `register_pipe`. `target_endpoint` is `Some(id)` for
    /// outbound pipes that came from a resolver dial (step 4c wires
    /// this) and `None` for inbound accepts.
    pub fn register_pending(&self, pipe: StableConnId, target_endpoint: Option<EndpointId>) {
        {
            let mut inner = self.inner.lock();
            inner.pending.insert(
                pipe,
                Pending {
                    pipe,
                    target_endpoint,
                    created_at: Instant::now(),
                },
            );
        }
        tracing::trace!(%pipe, ?target_endpoint, "routing_v2: pending registered");
        if let Some(ep) = target_endpoint {
            self.wake_endpoint_waiters(&ep);
        }
    }

    /// Remove a pending entry. Called when a pipe closes before
    /// promotion (or when the promotion rule explicitly rejects it).
    /// Idempotent.
    pub fn evict_pending(&self, pipe: StableConnId) -> Option<Pending> {
        self.inner.lock().pending.remove(&pipe)
    }

    /// Insert (or replace) a routable entry for `endpoint_id`. Step 4b
    /// calls this from the promotion rule after `decide()` returns
    /// `Accept`/`AcceptEvictingAll`. Direct test-only callers should
    /// drive the promote API instead.
    pub fn insert_routable(
        &self,
        endpoint_id: EndpointId,
        pipe: StableConnId,
        dialer: Dialer,
    ) -> Option<Routable> {
        let prev = {
            let mut inner = self.inner.lock();
            inner.pending.remove(&pipe);
            inner.routable.insert(
                endpoint_id,
                Routable {
                    pipe,
                    endpoint_id,
                    dialer,
                    verified_at: Instant::now(),
                },
            )
        };
        self.wake_endpoint_waiters(&endpoint_id);
        prev
    }

    /// Remove the routable entry for an endpoint, if any. Returns the
    /// removed entry so the caller can cascade-evict the pipe.
    pub fn evict_routable(&self, endpoint_id: &EndpointId) -> Option<Routable> {
        self.inner.lock().routable.remove(endpoint_id)
    }

    /// Evict whichever pool (if any) holds `pipe`. Called from the
    /// driver's pipe-close path — the pipe is about to disappear, so
    /// clean up both pools uniformly. Returns `true` if an entry was
    /// removed.
    pub fn evict_pipe_state(&self, pipe: StableConnId) -> bool {
        let mut inner = self.inner.lock();
        let pending_removed = inner.pending.remove(&pipe).is_some();
        // Routable is keyed on EndpointId; reverse-lookup by pipe.
        let routable_endpoint = inner
            .routable
            .iter()
            .find(|(_, r)| r.pipe == pipe)
            .map(|(k, _)| *k);
        let routable_removed = if let Some(ep) = routable_endpoint {
            inner.routable.remove(&ep).is_some()
        } else {
            false
        };
        pending_removed || routable_removed
    }

    /// Look up the pipe currently authoritatively routable for an
    /// endpoint. Step 4c's resolver consults this first.
    #[must_use]
    pub fn routable_pipe_for(&self, endpoint_id: &EndpointId) -> Option<StableConnId> {
        self.inner.lock().routable.get(endpoint_id).map(|r| r.pipe)
    }

    /// Look up a pending pipe whose target matches this endpoint. Step
    /// 4c consults this as a secondary source when the resolver can't
    /// find a routable entry.
    #[must_use]
    pub fn pending_pipe_for(&self, endpoint_id: &EndpointId) -> Option<StableConnId> {
        self.inner
            .lock()
            .pending
            .values()
            .find(|p| p.target_endpoint.as_ref() == Some(endpoint_id))
            .map(|p| p.pipe)
    }

    /// Resolve a live pipe's `StableConnId` back to the `DeviceId` it
    /// was opened against. `poll_send` consults this to translate
    /// iroh's `CustomAddr(stable_id)` into a `SendDatagram` command.
    /// Returns `None` if the id belongs to a reservation (no pipe yet)
    /// or has been evicted.
    #[must_use]
    pub fn device_for_pipe(&self, stable_id: StableConnId) -> Option<DeviceId> {
        self.inner
            .lock()
            .pipes
            .get(&stable_id)
            .map(|p| p.device_id.clone())
    }

    /// Snapshot of the current routable pool for the pipe-lifetime
    /// watchdog (step 5). Each entry is resolved into
    /// `(endpoint_id, stable_id, device_id, verified_at)` so the
    /// watchdog can cross-reference iroh's `remote_info` and, on
    /// mismatch, push the matching `DeviceId` to the registry for
    /// tearing down the BLE pipe. Entries with no corresponding pipe
    /// in `pipes` are dropped from the snapshot (the pipe was evicted
    /// between the lock releases).
    #[must_use]
    pub fn routable_entries(&self) -> Vec<RoutableSnapshot> {
        let inner = self.inner.lock();
        inner
            .routable
            .values()
            .filter_map(|r| {
                let device_id = inner.pipes.get(&r.pipe)?.device_id.clone();
                Some(RoutableSnapshot {
                    endpoint_id: r.endpoint_id,
                    stable_id: r.pipe,
                    device_id,
                    verified_at: r.verified_at,
                })
            })
            .collect()
    }

    /// Debug helpers for tests.
    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        self.inner.lock().pending.len()
    }

    #[cfg(test)]
    pub(crate) fn routable_len(&self) -> usize {
        self.inner.lock().routable.len()
    }

    #[cfg(test)]
    pub(crate) fn reservation_len(&self) -> usize {
        self.inner.lock().reservations.len()
    }

    // ---------- reservations + endpoint wakers (step 4c) ----------

    /// Mint a fresh `StableConnId` and stash it as an outbound-dial
    /// reservation for `endpoint_id`. Called by
    /// `BleAddressLookup::resolve` when `scan_hint` confirms the peer
    /// is reachable but no pipe exists yet.
    ///
    /// If a reservation for this prefix already exists, return the
    /// existing id (idempotent). This keeps `CustomAddr`s stable
    /// across repeated resolve calls for the same peer.
    pub fn reserve_outbound(&self, endpoint_id: EndpointId) -> StableConnId {
        let prefix = crate::transport::routing::prefix_from_endpoint(&endpoint_id);
        let mut inner = self.inner.lock();
        if let Some(existing) = inner.reservations.get(&prefix) {
            return existing.stable_id;
        }
        let stable_id = StableConnId(self.next_id.fetch_add(1, Ordering::Relaxed).wrapping_add(1));
        inner.reservations.insert(
            prefix,
            Reservation {
                stable_id,
                endpoint_id,
                reserved_at: Instant::now(),
            },
        );
        inner.reserved_stable_ids.insert(stable_id, prefix);
        tracing::debug!(%stable_id, %endpoint_id, ?prefix, "routing_v2: outbound reservation");
        stable_id
    }

    /// If `stable_id` is a live reservation (not yet bound to a pipe),
    /// return the target endpoint and prefix. `poll_send` uses this to
    /// translate `CustomAddr(stable_id)` into "trigger a dial for
    /// endpoint X via scan_hint[prefix]".
    #[must_use]
    pub fn reservation_target(&self, stable_id: StableConnId) -> Option<(EndpointId, KeyPrefix)> {
        let inner = self.inner.lock();
        let prefix = *inner.reserved_stable_ids.get(&stable_id)?;
        let reservation = inner.reservations.get(&prefix)?;
        Some((reservation.endpoint_id, prefix))
    }

    /// Look up the reservation for this prefix, if any. Resolver calls
    /// this to decide whether to reuse an existing reservation or mint
    /// a new one.
    #[must_use]
    pub fn reservation_for_prefix(&self, prefix: &KeyPrefix) -> Option<Reservation> {
        self.inner.lock().reservations.get(prefix).cloned()
    }

    /// Consume the reservation for `prefix` (if any). Driver calls
    /// this at pipe-open so it can bind the reserved id to the
    /// just-opened pipe.
    pub fn consume_reservation_for_prefix(&self, prefix: &KeyPrefix) -> Option<Reservation> {
        let mut inner = self.inner.lock();
        let reservation = inner.reservations.remove(prefix)?;
        inner.reserved_stable_ids.remove(&reservation.stable_id);
        Some(reservation)
    }

    /// Reverse-lookup via scan_hint: given a `DeviceId` the driver
    /// just opened a pipe to, find the prefix that's been tracking it
    /// and (if any) consume the reservation. Convenience for the
    /// pipe-open path, which knows `DeviceId` but not `KeyPrefix`.
    pub fn consume_reservation_for_device(&self, device_id: &DeviceId) -> Option<Reservation> {
        let mut inner = self.inner.lock();
        let prefix = *inner
            .scan_hint
            .iter()
            .find_map(|(p, d)| (d == device_id).then_some(p))?;
        let reservation = inner.reservations.remove(&prefix)?;
        inner.reserved_stable_ids.remove(&reservation.stable_id);
        Some(reservation)
    }

    /// Register a pipe with a caller-specified `StableConnId` (rather
    /// than minting). Driver calls this when it's consuming a
    /// reservation — the id was already handed to iroh and must be
    /// preserved. Panics in debug if the id collides with an existing
    /// pipe to catch lifecycle bugs early.
    pub fn register_pipe_with_id(
        &self,
        stable_id: StableConnId,
        device_id: DeviceId,
        direction: Direction,
    ) {
        let pipe = Pipe {
            id: stable_id,
            device_id: device_id.clone(),
            direction,
            created_at: Instant::now(),
        };
        let mut inner = self.inner.lock();
        debug_assert!(
            !inner.pipes.contains_key(&stable_id),
            "StableConnId collision: id={stable_id} already live"
        );
        inner.pipes.insert(stable_id, pipe);
        tracing::debug!(
            %stable_id,
            device = %device_id,
            ?direction,
            "routing_v2: pipe registered (reused reserved id)"
        );
    }

    /// Park a waker for a resolver waiting on any state change for
    /// `endpoint_id`. Drained by `wake_endpoint_waiters`.
    pub fn register_endpoint_waker(&self, endpoint_id: EndpointId, waker: &Waker) {
        let mut inner = self.inner.lock();
        let list = inner.endpoint_wakers.entry(endpoint_id).or_default();
        if !list.iter().any(|w| w.will_wake(waker)) {
            list.push(waker.clone());
        }
    }

    /// Drain and wake all resolvers parked on `endpoint_id`. Called
    /// from any state change that might advance a parked resolver:
    /// a new scan_hint, a new pending with matching target, a new
    /// routable entry, a new reservation.
    fn wake_endpoint_waiters(&self, endpoint_id: &EndpointId) {
        let wakers: Vec<Waker> = {
            let mut inner = self.inner.lock();
            inner
                .endpoint_wakers
                .remove(endpoint_id)
                .unwrap_or_default()
        };
        for w in wakers {
            w.wake();
        }
    }

    // ---------- promotion rule (step 4b) ----------

    /// Run the promotion rule (see `decide()`) for a pending pipe whose
    /// iroh TLS handshake just completed. Mutates `pending`/`routable`
    /// atomically, returns the decision + the list of other
    /// `StableConnId`s the caller must tear down (losers).
    ///
    /// `self_endpoint` is our local node's endpoint id (fixed at
    /// `BleTransport` construction). `remote_endpoint` is the just-
    /// authenticated peer. The pipe's `Direction` is read from the
    /// pipes map to derive the observer-symmetric `Dialer` both sides
    /// of a pair will compute identically.
    pub fn promote(
        &self,
        pipe_id: StableConnId,
        self_endpoint: &EndpointId,
        remote_endpoint: EndpointId,
    ) -> PromoteOutcome {
        let mut inner = self.inner.lock();

        // If the pipe vanished before promotion (rare race between
        // handshake completion and pipe teardown), there's nothing to
        // promote. Treat as Reject so the caller closes the iroh
        // connection — a handshake over a dead pipe can't route anyway.
        let Some(pipe) = inner.pipes.get(&pipe_id).cloned() else {
            tracing::warn!(
                %pipe_id,
                "routing_v2::promote: pipe vanished before promotion; rejecting"
            );
            return PromoteOutcome::Rejected;
        };

        let new_dialer = Dialer::compute(self_endpoint, &remote_endpoint, pipe.direction);

        // Collect contenders for this endpoint:
        //   - the existing routable entry (if any, and not us).
        //   - other pendings targeting this endpoint (outbound that
        //     named this target) — excludes inbound pendings whose
        //     target is unknown.
        let mut contenders: Vec<Contender> = Vec::new();
        if let Some(existing) = inner.routable.get(&remote_endpoint)
            && existing.pipe != pipe_id
        {
            contenders.push(Contender {
                pipe: existing.pipe,
                dialer: existing.dialer,
                is_healthy: inner.pipes.contains_key(&existing.pipe),
            });
        }
        for (pid, p) in &inner.pending {
            if *pid == pipe_id {
                continue;
            }
            if p.target_endpoint.as_ref() != Some(&remote_endpoint) {
                continue;
            }
            let Some(other_pipe) = inner.pipes.get(pid) else {
                continue;
            };
            let other_dialer =
                Dialer::compute(self_endpoint, &remote_endpoint, other_pipe.direction);
            contenders.push(Contender {
                pipe: *pid,
                dialer: other_dialer,
                is_healthy: true, // a live pending pipe is healthy by definition
            });
        }

        let decision = decide(new_dialer, &contenders);

        let (evicted, promoted) = match decision {
            Decision::Accept | Decision::AcceptEvictingAll => {
                let evicted: Vec<StableConnId> = contenders.iter().map(|c| c.pipe).collect();
                for pid in &evicted {
                    inner.pending.remove(pid);
                    // Best-effort routable eviction — the contender
                    // might have been a routable entry.
                    if let Some((ep, _)) = inner.routable.iter().find(|(_, r)| r.pipe == *pid) {
                        let ep = *ep;
                        inner.routable.remove(&ep);
                    }
                }
                // Remove the incumbent routable for this endpoint (if
                // different from anything evicted above) to make room
                // for the new one.
                inner.routable.remove(&remote_endpoint);
                // Move the promoting pipe from pending to routable.
                inner.pending.remove(&pipe_id);
                inner.routable.insert(
                    remote_endpoint,
                    Routable {
                        pipe: pipe_id,
                        endpoint_id: remote_endpoint,
                        dialer: new_dialer,
                        verified_at: Instant::now(),
                    },
                );
                (evicted, true)
            }
            Decision::Reject => (Vec::new(), false),
        };

        tracing::debug!(
            %pipe_id,
            %remote_endpoint,
            ?new_dialer,
            ?decision,
            evicted_count = evicted.len(),
            "routing_v2::promote"
        );

        if promoted {
            PromoteOutcome::Accepted { evicted }
        } else {
            PromoteOutcome::Rejected
        }
    }
}

// ---------- decide() rule (step 4b, pure function) ----------

/// A contender for the routable slot of some `EndpointId`. Used only
/// inside the promotion rule — never exposed outside `routing_v2`.
#[derive(Debug, Clone, Copy)]
struct Contender {
    pipe: StableConnId,
    dialer: Dialer,
    is_healthy: bool,
}

/// Internal decision shape. Public users see [`PromoteOutcome`], which
/// flattens Accept / AcceptEvictingAll into a single Accepted variant
/// with the eviction list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Accept,
    AcceptEvictingAll,
    Reject,
}

/// Observer-symmetric resolution of a handshake collision. See §9.5 of
/// the study doc for the derivation and the four scenarios (single
/// dial, symmetric dial, F2 reconnect, pathological retry thrash).
///
/// - No contender → `Accept` (single dial, fresh inbound).
/// - Different positional category (one lower-dialed, one higher-dialed)
///   → higher-dialed wins unconditionally (`AcceptEvictingAll` or
///   `Reject` depending on which side the new pipe is on). Resolves
///   symmetric-dial collisions without relying on gossip's
///   timing-based adopt-newest.
/// - Same positional category (both lower-dialed or both higher-dialed,
///   i.e. re-dial / F2 recovery) → health arbitrates: existing wins
///   unless it's unhealthy; otherwise the new replaces.
fn decide(new_dialer: Dialer, contenders: &[Contender]) -> Decision {
    if contenders.is_empty() {
        return Decision::Accept;
    }
    let any_existing_is_high = contenders.iter().any(|c| c.dialer == Dialer::High);
    match (new_dialer, any_existing_is_high) {
        (Dialer::High, false) => Decision::AcceptEvictingAll,
        (Dialer::Low, true) => Decision::Reject,
        _ => {
            if contenders.iter().any(|c| c.is_healthy) {
                Decision::Reject
            } else {
                Decision::AcceptEvictingAll
            }
        }
    }
}

/// Outcome of a promotion attempt. `Accepted` carries the list of
/// `StableConnId`s the caller must tear down (losing pipes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromoteOutcome {
    Accepted { evicted: Vec<StableConnId> },
    Rejected,
}

/// One routable entry surfaced to the pipe-lifetime watchdog (step 5).
/// Pairs the routable pool's identity with the underlying pipe's
/// `DeviceId` so the watchdog can issue a targeted teardown to the
/// registry without racing through a second lock.
#[derive(Debug, Clone)]
pub struct RoutableSnapshot {
    pub endpoint_id: EndpointId,
    pub stable_id: StableConnId,
    pub device_id: DeviceId,
    pub verified_at: Instant,
}

/// Counts-only view of the shadow routing state. Does not leak any
/// identifiers; callers that need identifiers take the explicit
/// `pipes_for_debug` path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RoutingSnapshot {
    pub pipes: usize,
    pub scan_hints: usize,
    pub pending: usize,
    pub routable: usize,
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
    fn note_scan_hint_is_idempotent_and_returns_previous() {
        let r = Routing::new();
        let prefix: KeyPrefix = [10u8; 12];
        let d1 = dev("mac-aa");
        let d2 = dev("mac-bb");

        assert_eq!(r.note_scan_hint(prefix, d1.clone()), None);
        assert_eq!(r.scan_hint_for_prefix(&prefix).as_ref(), Some(&d1));
        assert_eq!(r.snapshot().scan_hints, 1);

        // Same device — no-op, returns the same device as "previous"
        // so callers can detect the unchanged case if they need to.
        assert_eq!(
            r.note_scan_hint(prefix, d1.clone()),
            Some(d1.clone()),
            "re-noting the same device returns it as the previous mapping"
        );
        assert_eq!(r.snapshot().scan_hints, 1);

        // Different device → replace, return old.
        assert_eq!(r.note_scan_hint(prefix, d2.clone()), Some(d1));
        assert_eq!(r.scan_hint_for_prefix(&prefix).as_ref(), Some(&d2));
        assert_eq!(r.snapshot().scan_hints, 1);
    }

    #[test]
    fn forget_scan_hint_is_idempotent() {
        let r = Routing::new();
        let prefix: KeyPrefix = [11u8; 12];
        let d = dev("peer");
        r.note_scan_hint(prefix, d);
        assert_eq!(r.snapshot().scan_hints, 1);
        r.forget_scan_hint(&prefix);
        assert_eq!(r.snapshot().scan_hints, 0);
        r.forget_scan_hint(&prefix); // no panic on missing
        assert!(r.scan_hint_for_prefix(&prefix).is_none());
    }

    #[test]
    fn scan_hint_is_independent_from_pipes() {
        // Dial hints and live pipes live in orthogonal state. Recording
        // a hint must not create a pipe entry, and opening a pipe must
        // not create a hint. Keeps the authority model clean (§9.1 of
        // the study doc): scan_hint is pre-authentication, pipes are
        // post-dial.
        let r = Routing::new();
        let prefix: KeyPrefix = [12u8; 12];
        let d = dev("peer-c");

        r.note_scan_hint(prefix, d.clone());
        assert_eq!(r.snapshot().pipes, 0);

        let id = r.register_pipe(dev("mac-of-peer-c"), Direction::Outbound);
        // scan_hint count unaffected by pipe registration.
        assert_eq!(r.snapshot().scan_hints, 1);
        r.evict_pipe(id);
        // And pipe eviction doesn't affect scan_hint.
        assert_eq!(r.snapshot().scan_hints, 1);
    }

    fn test_endpoint(seed: u8) -> EndpointId {
        iroh_base::SecretKey::from_bytes(&[seed; 32]).public()
    }

    // ---------- pending / routable pool bookkeeping (step 4a) ----------

    #[test]
    fn register_pending_records_entry() {
        let r = Routing::new();
        let id = r.register_pipe(dev("p"), Direction::Outbound);
        assert_eq!(r.pending_len(), 0);
        r.register_pending(id, None);
        assert_eq!(r.pending_len(), 1);
        assert_eq!(r.snapshot().pending, 1);
    }

    #[test]
    fn register_pending_with_target_is_findable_by_endpoint() {
        let r = Routing::new();
        let id = r.register_pipe(dev("p"), Direction::Outbound);
        let ep = test_endpoint(1);
        r.register_pending(id, Some(ep));
        assert_eq!(r.pending_pipe_for(&ep), Some(id));
        // Unknown endpoint doesn't match.
        assert_eq!(r.pending_pipe_for(&test_endpoint(2)), None);
    }

    #[test]
    fn evict_pending_removes_entry() {
        let r = Routing::new();
        let id = r.register_pipe(dev("p"), Direction::Outbound);
        r.register_pending(id, None);
        assert!(r.evict_pending(id).is_some());
        assert_eq!(r.pending_len(), 0);
        assert!(r.evict_pending(id).is_none(), "idempotent");
    }

    #[test]
    fn insert_routable_removes_pending_and_keys_by_endpoint() {
        let r = Routing::new();
        let id = r.register_pipe(dev("p"), Direction::Outbound);
        let ep = test_endpoint(3);
        r.register_pending(id, Some(ep));
        assert_eq!(r.pending_len(), 1);

        let prev = r.insert_routable(ep, id, Dialer::Low);
        assert!(prev.is_none(), "no prior routable");
        assert_eq!(r.pending_len(), 0, "promotion removes the pending entry");
        assert_eq!(r.routable_len(), 1);
        assert_eq!(r.routable_pipe_for(&ep), Some(id));
    }

    #[test]
    fn insert_routable_returns_previous_entry_on_replace() {
        let r = Routing::new();
        let ep = test_endpoint(4);
        let old = r.register_pipe(dev("old"), Direction::Outbound);
        let new = r.register_pipe(dev("new"), Direction::Outbound);

        r.insert_routable(ep, old, Dialer::Low);
        let prev = r.insert_routable(ep, new, Dialer::High);
        assert!(prev.is_some());
        assert_eq!(prev.unwrap().pipe, old);
        assert_eq!(r.routable_pipe_for(&ep), Some(new));
        assert_eq!(r.routable_len(), 1, "routable uniqueness invariant");
    }

    #[test]
    fn evict_pipe_state_removes_from_whichever_pool() {
        let r = Routing::new();
        let ep = test_endpoint(5);

        // Pipe A: routable.
        let a = r.register_pipe(dev("a"), Direction::Outbound);
        r.insert_routable(ep, a, Dialer::Low);
        assert!(r.evict_pipe_state(a), "routable removed");
        assert_eq!(r.routable_len(), 0);

        // Pipe B: pending.
        let b = r.register_pipe(dev("b"), Direction::Outbound);
        r.register_pending(b, None);
        assert!(r.evict_pipe_state(b), "pending removed");
        assert_eq!(r.pending_len(), 0);

        // Pipe C: in neither pool.
        let c = r.register_pipe(dev("c"), Direction::Outbound);
        assert!(
            !r.evict_pipe_state(c),
            "no pool entry — evict_pipe_state returns false, no panic"
        );
    }

    #[test]
    fn routable_pipe_for_returns_none_for_unknown_endpoint() {
        let r = Routing::new();
        assert!(r.routable_pipe_for(&test_endpoint(9)).is_none());
    }

    #[test]
    fn pools_are_independent_of_pipes_map() {
        // The pipes map tracks "a pipe exists right now." The pending
        // and routable pools track "how is this pipe classified for
        // routing purposes." Authority-model invariant: these are
        // orthogonal dimensions.
        let r = Routing::new();
        let id = r.register_pipe(dev("x"), Direction::Outbound);
        assert_eq!(r.snapshot().pipes, 1);
        assert_eq!(r.snapshot().pending, 0);
        assert_eq!(r.snapshot().routable, 0);

        r.register_pending(id, None);
        assert_eq!(r.snapshot().pipes, 1);
        assert_eq!(r.snapshot().pending, 1);

        r.evict_pipe(id);
        // Evicting the pipe does NOT automatically clean up the pool
        // entry; the driver is responsible for calling evict_pipe_state
        // first. This asymmetry is deliberate: it keeps the routing
        // layer unopinionated about lifecycle ordering, and the driver
        // has the complete picture.
        assert_eq!(r.snapshot().pipes, 0);
        assert_eq!(r.snapshot().pending, 1);
    }

    // ---------- decide() rule (step 4b) ----------

    fn contender(dialer: Dialer, is_healthy: bool) -> Contender {
        Contender {
            pipe: StableConnId::for_test(999),
            dialer,
            is_healthy,
        }
    }

    #[test]
    fn decide_accepts_when_no_contenders() {
        assert_eq!(decide(Dialer::Low, &[]), Decision::Accept);
        assert_eq!(decide(Dialer::High, &[]), Decision::Accept);
    }

    #[test]
    fn decide_symmetric_collision_higher_dialed_wins() {
        // Classic two-sides-dialed-simultaneously. Both peers run this
        // function; both reach the same conclusion because `Dialer` is
        // observer-symmetric.
        let low_contender = [contender(Dialer::Low, true)];
        assert_eq!(
            decide(Dialer::High, &low_contender),
            Decision::AcceptEvictingAll,
            "higher-dialed new pipe evicts lower-dialed contender"
        );

        let high_contender = [contender(Dialer::High, true)];
        assert_eq!(
            decide(Dialer::Low, &high_contender),
            Decision::Reject,
            "lower-dialed new pipe loses to higher-dialed contender"
        );
    }

    #[test]
    fn decide_same_category_existing_wins_when_healthy() {
        // Re-dial path: same direction, existing is healthy → reject
        // the new redundant connection.
        for dialer in [Dialer::Low, Dialer::High] {
            let existing = [contender(dialer, true)];
            assert_eq!(
                decide(dialer, &existing),
                Decision::Reject,
                "healthy same-category existing wins ({dialer:?})"
            );
        }
    }

    #[test]
    fn decide_same_category_unhealthy_existing_loses() {
        // F2 recovery: stale routable with dead pipe, new arrival takes
        // over.
        for dialer in [Dialer::Low, Dialer::High] {
            let stale = [contender(dialer, false)];
            assert_eq!(
                decide(dialer, &stale),
                Decision::AcceptEvictingAll,
                "unhealthy same-category existing loses ({dialer:?})"
            );
        }
    }

    // ---------- promote() integration (step 4b) ----------

    #[test]
    fn promote_single_dial_accepts() {
        // No contenders → Accept. Endpoint ordering irrelevant (no
        // contenders) but pinned anyway for stability.
        let me = iroh_base::SecretKey::from_bytes(&[0x01u8; 32]).public();
        let peer = iroh_base::SecretKey::from_bytes(&[0xFEu8; 32]).public();
        let r = Routing::new();
        let id = r.register_pipe(dev("peer-mac"), Direction::Outbound);
        r.register_pending(id, Some(peer));

        let outcome = r.promote(id, &me, peer);
        assert!(matches!(outcome, PromoteOutcome::Accepted { evicted } if evicted.is_empty()));
        assert_eq!(r.pending_len(), 0);
        assert_eq!(r.routable_len(), 1);
        assert_eq!(r.routable_pipe_for(&peer), Some(id));
    }

    #[test]
    fn promote_symmetric_dial_converges_on_higher_dialed() {
        // Both sides dialed. Local = low endpoint. One pipe is
        // outbound (we dialed = Low dialer), the other inbound (they
        // dialed = High dialer). Run promote in both orders; the High
        // always wins.
        let me = iroh_base::SecretKey::from_bytes(&[0x01u8; 32]).public();
        let peer = iroh_base::SecretKey::from_bytes(&[0xFEu8; 32]).public();
        assert!(me.as_bytes() < peer.as_bytes(), "test invariant: me < peer");

        // Case A: outbound pipe's handshake completes first.
        {
            let r = Routing::new();
            let outbound = r.register_pipe(dev("peer-mac-a"), Direction::Outbound);
            let inbound = r.register_pipe(dev("peer-cbcentral-a"), Direction::Inbound);
            r.register_pending(outbound, Some(peer));
            r.register_pending(inbound, None);

            let first = r.promote(outbound, &me, peer);
            // outbound→we dialed, we are low → Dialer::Low. existing
            // pending (inbound = they dialed, they are high →
            // Dialer::High) but no target_endpoint yet, so it's NOT a
            // contender in this call. Accept uncontested.
            assert!(matches!(first, PromoteOutcome::Accepted { evicted } if evicted.is_empty()));
            assert_eq!(r.routable_pipe_for(&peer), Some(outbound));

            // Now inbound's hook fires. inbound Dialer::High,
            // existing routable Dialer::Low → AcceptEvictingAll.
            let second = r.promote(inbound, &me, peer);
            match second {
                PromoteOutcome::Accepted { evicted } => {
                    assert_eq!(evicted, vec![outbound]);
                }
                _ => panic!("inbound must win"),
            }
            assert_eq!(r.routable_pipe_for(&peer), Some(inbound));
        }

        // Case B: inbound pipe's handshake completes first.
        {
            let r = Routing::new();
            let outbound = r.register_pipe(dev("peer-mac-b"), Direction::Outbound);
            let inbound = r.register_pipe(dev("peer-cbcentral-b"), Direction::Inbound);
            r.register_pending(outbound, Some(peer));
            r.register_pending(inbound, None);

            // inbound (Dialer::High for us) promotes first. existing
            // pending is outbound (Dialer::Low), targeting peer — so
            // it IS a contender.
            let first = r.promote(inbound, &me, peer);
            match first {
                PromoteOutcome::Accepted { evicted } => {
                    assert_eq!(evicted, vec![outbound]);
                }
                _ => panic!("inbound (higher-dialed) should win"),
            }
            assert_eq!(r.routable_pipe_for(&peer), Some(inbound));

            // outbound's hook would fire next, but its pending entry
            // was just evicted. Promoting it finds the pipe gone or
            // pending gone — we still re-run the rule against the
            // existing routable (which is inbound, High-dialed).
            // outbound is Low, existing is High → Reject.
            let second = r.promote(outbound, &me, peer);
            assert!(matches!(second, PromoteOutcome::Rejected));
            assert_eq!(r.routable_pipe_for(&peer), Some(inbound));
        }
    }

    #[test]
    fn promote_reconnect_replaces_unhealthy_existing() {
        // F2 scenario: stale routable whose pipe is gone, new pipe to
        // the same peer in the same direction → new replaces.
        //
        // Pin me<peer so both pipes get Dialer::Low (Outbound from
        // the low side), putting the test in the "same-category,
        // arbitrate on health" branch.
        let me = iroh_base::SecretKey::from_bytes(&[0x01u8; 32]).public();
        let peer = iroh_base::SecretKey::from_bytes(&[0xFEu8; 32]).public();
        assert!(me.as_bytes() < peer.as_bytes(), "test invariant: me < peer");

        let r = Routing::new();
        let stale = r.register_pipe(dev("old-mac"), Direction::Outbound);
        r.insert_routable(peer, stale, Dialer::Low);
        r.evict_pipe(stale); // BLE link died; pipe gone but routable stale
        assert!(r.routable_pipe_for(&peer).is_some());

        let fresh = r.register_pipe(dev("new-mac"), Direction::Outbound);
        r.register_pending(fresh, Some(peer));
        let outcome = r.promote(fresh, &me, peer);

        match outcome {
            PromoteOutcome::Accepted { evicted } => {
                assert!(
                    evicted.contains(&stale),
                    "stale routable must be in the evicted list"
                );
            }
            _ => panic!("unhealthy existing should lose"),
        }
        assert_eq!(r.routable_pipe_for(&peer), Some(fresh));
    }

    #[test]
    fn promote_same_direction_healthy_existing_wins() {
        // Redundant re-dial: new pipe, same direction, existing pipe
        // still healthy → reject the new one.
        //
        // Must pin the endpoint ordering explicitly — seed bytes alone
        // don't imply a byte-wise ordering of the derived public keys.
        let me = iroh_base::SecretKey::from_bytes(&[0x01u8; 32]).public();
        let peer = iroh_base::SecretKey::from_bytes(&[0xFEu8; 32]).public();
        assert!(me.as_bytes() < peer.as_bytes(), "test invariant: me < peer");
        // Both outbound → both Dialer::Low (lower endpoint dialed).

        let r = Routing::new();
        let existing = r.register_pipe(dev("a"), Direction::Outbound);
        r.insert_routable(peer, existing, Dialer::Low);

        let redundant = r.register_pipe(dev("b"), Direction::Outbound);
        r.register_pending(redundant, Some(peer));
        let outcome = r.promote(redundant, &me, peer);

        assert!(matches!(outcome, PromoteOutcome::Rejected));
        assert_eq!(r.routable_pipe_for(&peer), Some(existing));
        assert_eq!(
            r.pending_len(),
            1,
            "rejected pending stays — caller tears it down separately"
        );
    }

    #[test]
    fn promote_rejects_when_pipe_gone() {
        // Race: pipe is torn down before promote runs.
        let r = Routing::new();
        let me = test_endpoint(11);
        let peer = test_endpoint(12);
        let ghost = StableConnId::for_test(4242);
        r.register_pending(ghost, Some(peer));
        // No pipe was registered for this id.
        let outcome = r.promote(ghost, &me, peer);
        assert!(matches!(outcome, PromoteOutcome::Rejected));
    }

    // ---------- reservations + endpoint wakers (step 4c) ----------

    struct CountingWaker(std::sync::atomic::AtomicUsize);
    impl std::task::Wake for CountingWaker {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        fn wake_by_ref(self: &std::sync::Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    fn counting_waker() -> (std::sync::Arc<CountingWaker>, std::task::Waker) {
        let inner = std::sync::Arc::new(CountingWaker(std::sync::atomic::AtomicUsize::new(0)));
        let waker = std::task::Waker::from(std::sync::Arc::clone(&inner));
        (inner, waker)
    }

    fn prefix_of(ep: &EndpointId) -> KeyPrefix {
        crate::transport::routing::prefix_from_endpoint(ep)
    }

    #[test]
    fn reserve_outbound_is_idempotent_per_prefix() {
        let r = Routing::new();
        let ep = test_endpoint(31);
        let a = r.reserve_outbound(ep);
        let b = r.reserve_outbound(ep);
        assert_eq!(a, b, "second reserve for same prefix returns same id");
        assert_eq!(r.reservation_len(), 1);
    }

    #[test]
    fn reservation_target_roundtrips_prefix_and_endpoint() {
        let r = Routing::new();
        let ep = test_endpoint(32);
        let id = r.reserve_outbound(ep);
        let (got_ep, got_prefix) = r.reservation_target(id).expect("reservation present");
        assert_eq!(got_ep, ep);
        assert_eq!(got_prefix, prefix_of(&ep));
    }

    #[test]
    fn consume_reservation_for_prefix_removes_both_sides() {
        let r = Routing::new();
        let ep = test_endpoint(33);
        let id = r.reserve_outbound(ep);
        let prefix = prefix_of(&ep);
        let taken = r.consume_reservation_for_prefix(&prefix).expect("taken");
        assert_eq!(taken.stable_id, id);
        assert_eq!(taken.endpoint_id, ep);
        assert_eq!(r.reservation_len(), 0);
        assert!(r.reservation_target(id).is_none());
        // Idempotent second call.
        assert!(r.consume_reservation_for_prefix(&prefix).is_none());
    }

    #[test]
    fn consume_reservation_for_device_uses_scan_hint() {
        let r = Routing::new();
        let ep = test_endpoint(34);
        let prefix = prefix_of(&ep);
        let device = dev("mac-34");
        r.note_scan_hint(prefix, device.clone());
        let id = r.reserve_outbound(ep);
        let taken = r
            .consume_reservation_for_device(&device)
            .expect("device is tracked under reserved prefix");
        assert_eq!(taken.stable_id, id);
        assert_eq!(r.reservation_len(), 0);
    }

    #[test]
    fn consume_reservation_for_device_returns_none_without_scan_hint() {
        let r = Routing::new();
        let ep = test_endpoint(35);
        r.reserve_outbound(ep);
        // No scan_hint for this prefix → can't translate the device back.
        assert!(
            r.consume_reservation_for_device(&dev("unknown-mac"))
                .is_none()
        );
    }

    #[test]
    fn register_pipe_with_id_inserts_without_minting_new() {
        let r = Routing::new();
        let ep = test_endpoint(36);
        let id = r.reserve_outbound(ep);
        r.register_pipe_with_id(id, dev("mac-36"), Direction::Outbound);
        assert_eq!(r.snapshot().pipes, 1);
        assert_eq!(r.device_for_pipe(id).as_ref(), Some(&dev("mac-36")));
    }

    #[test]
    fn routable_entries_resolves_routable_pipes_with_device_ids() {
        let r = Routing::new();
        let ep1 = test_endpoint(50);
        let ep2 = test_endpoint(51);
        let p1 = r.register_pipe(dev("mac-1"), Direction::Outbound);
        let p2 = r.register_pipe(dev("mac-2"), Direction::Inbound);
        r.insert_routable(ep1, p1, Dialer::Low);
        r.insert_routable(ep2, p2, Dialer::High);

        let mut snap = r.routable_entries();
        snap.sort_by_key(|e| e.stable_id);
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].endpoint_id, ep1);
        assert_eq!(snap[0].stable_id, p1);
        assert_eq!(snap[0].device_id, dev("mac-1"));
        assert_eq!(snap[1].endpoint_id, ep2);
        assert_eq!(snap[1].stable_id, p2);
        assert_eq!(snap[1].device_id, dev("mac-2"));
    }

    #[test]
    fn routable_entries_drops_routable_with_missing_pipe() {
        // If the pipes map lost the entry for a routable id (race between
        // evict_pipe and evict_routable), the watchdog snapshot skips it
        // rather than handing a stale DeviceId back. The next tick will
        // pick up the eviction cleanly.
        let r = Routing::new();
        let ep = test_endpoint(52);
        let id = r.register_pipe(dev("ghost-mac"), Direction::Outbound);
        r.insert_routable(ep, id, Dialer::Low);
        r.evict_pipe(id);
        assert_eq!(r.routable_entries().len(), 0);
    }

    #[test]
    fn device_for_pipe_follows_register_and_evict() {
        let r = Routing::new();
        let id = r.register_pipe(dev("alive"), Direction::Outbound);
        assert_eq!(r.device_for_pipe(id).as_ref(), Some(&dev("alive")));
        r.evict_pipe(id);
        assert!(r.device_for_pipe(id).is_none());
    }

    #[test]
    fn endpoint_waker_fires_when_routable_inserted() {
        let r = Routing::new();
        let ep = test_endpoint(37);
        let (counter, waker) = counting_waker();
        r.register_endpoint_waker(ep, &waker);
        let id = r.register_pipe(dev("peer"), Direction::Outbound);
        r.insert_routable(ep, id, Dialer::Low);
        assert_eq!(counter.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn endpoint_waker_fires_when_pending_registered_with_target() {
        let r = Routing::new();
        let ep = test_endpoint(38);
        let (counter, waker) = counting_waker();
        r.register_endpoint_waker(ep, &waker);
        let id = r.register_pipe(dev("peer"), Direction::Outbound);
        r.register_pending(id, Some(ep));
        assert_eq!(counter.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn endpoint_waker_fires_when_scan_hint_lands_for_prefix() {
        let r = Routing::new();
        let ep = test_endpoint(39);
        let (counter, waker) = counting_waker();
        r.register_endpoint_waker(ep, &waker);
        r.note_scan_hint(prefix_of(&ep), dev("first-sighting"));
        assert_eq!(counter.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn endpoint_waker_does_not_fire_for_unrelated_prefix() {
        let r = Routing::new();
        let ep_a = test_endpoint(40);
        let ep_b = test_endpoint(41);
        let (counter, waker) = counting_waker();
        r.register_endpoint_waker(ep_a, &waker);
        // Scan hints for a different prefix must not wake ep_a's waiters.
        r.note_scan_hint(prefix_of(&ep_b), dev("unrelated"));
        assert_eq!(counter.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn register_endpoint_waker_dedupes() {
        let r = Routing::new();
        let ep = test_endpoint(42);
        let (counter, waker) = counting_waker();
        r.register_endpoint_waker(ep, &waker);
        r.register_endpoint_waker(ep, &waker);
        r.register_endpoint_waker(ep, &waker);
        // Wake once via scan_hint.
        r.note_scan_hint(prefix_of(&ep), dev("x"));
        assert_eq!(
            counter.0.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "dedup: each unique waker fires exactly once"
        );
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
