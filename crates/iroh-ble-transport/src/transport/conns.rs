//! Index of the live iroh connections riding each BLE pipe, and the
//! close vocabulary the transport uses when it takes a pipe away.
//!
//! Two teardown directions exist and both need this table:
//!
//! - **iroh closes first** — the hook's per-connection `closed()` watcher
//!   fires, and once the last connection on a pipe is gone the transport
//!   drains the pipe. That is what the watch-id bookkeeping here is for.
//! - **the pipe dies first** — dedup eviction, a dead link, an adapter
//!   loss. iroh has no way to learn this on its own: `CustomEndpoint`
//!   cannot report a path as dead, and iroh does not migrate an existing
//!   `Connection` onto a freshly resolved `CustomAddr` (see
//!   `EndpointResolveStream`). A connection whose only path was that pipe
//!   can never carry traffic again, so we close it here rather than
//!   leaving the application blocked on a read until its own deadline.
//!
//! Where the close shows up: the local application — the one whose pipe
//! we took away — sees `ConnectionError::LocallyClosed`, which carries
//! neither code nor reason; what it gains is that its read returns at
//! once instead of hanging. The `(code, reason)` pair below is what the
//! *peer* sees, as `ApplicationClosed`, and what our own logs record.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use iroh::endpoint::{VarInt, WeakConnectionHandle};
use iroh_base::{EndpointId, TransportAddr};

use crate::transport::routing::{StableConnId, parse_token_addr};
use crate::transport::transport::BLE_TRANSPORT_ID;

/// Close code for a conflict the peer should not redial: another
/// connection to the same endpoint is already the live one.
pub const BLE_CLOSE_CODE_CONFLICT: u32 = 0;

/// Close code for a BLE-side teardown a redial can fix.
pub const BLE_CLOSE_CODE_RETRY: u32 = 1;

/// Sent to the peer whose handshake the dedup rule rejected.
pub const BLE_CLOSE_REASON_CONFLICT: &[u8] = b"ble_conflict";

/// Sent on connections whose pipe the dedup rule evicted. Another pipe
/// to the same endpoint is already routable, so a redial succeeds
/// immediately.
pub const BLE_CLOSE_REASON_EVICTED: &[u8] = b"ble_evicted";

/// Sent on connections whose pipe died for any other reason (dead link,
/// drain, adapter loss). A redial succeeds once the peer is reachable
/// again.
pub const BLE_CLOSE_REASON_PIPE_CLOSED: &[u8] = b"ble_pipe_closed";

/// A connection handle the registry can close. Implemented for
/// [`WeakConnectionHandle`]; the trait exists so tests can drive the
/// registry without a live iroh endpoint.
pub trait ConnHandle: Send + Sync + 'static {
    /// Close this connection if — and only if — every path still open on
    /// it rides `pipe`. A connection that also holds a relay path, or a
    /// second BLE path on another live pipe, has not lost its last path
    /// and is left alone. Returns whether it was closed.
    fn close_if_only_path(&self, pipe: StableConnId, code: VarInt, reason: &[u8]) -> bool;
}

impl ConnHandle for WeakConnectionHandle {
    fn close_if_only_path(&self, pipe: StableConnId, code: VarInt, reason: &[u8]) -> bool {
        // A weak handle that no longer upgrades belongs to a connection
        // the application already dropped: nothing to close.
        let Some(conn) = self.upgrade() else {
            return false;
        };
        // `paths()` only ever lists open paths, so no closed-path filter here.
        let has_other_path = conn.paths().iter().any(|path| match path.remote_addr() {
            TransportAddr::Custom(addr) if addr.id() == BLE_TRANSPORT_ID => {
                parse_token_addr(addr).ok() != Some(pipe.as_u64())
            }
            _ => true,
        });
        if has_other_path {
            return false;
        }
        // Close before the temporary strong handle drops: if ours was the
        // last one, dropping it first would close the connection with
        // iroh's default reason instead of ours.
        conn.close(code, reason);
        true
    }
}

type ActiveConnectionKey = (EndpointId, StableConnId);

/// The connections in one (endpoint, pipe) bucket, by watch id.
type WatchedConns = HashMap<u64, Arc<dyn ConnHandle>>;

/// Live connections, bucketed by the peer they authenticated as and the
/// pipe they route over.
#[derive(Default)]
pub struct ConnectionRegistry {
    next_id: AtomicU64,
    inner: parking_lot::Mutex<HashMap<ActiveConnectionKey, WatchedConns>>,
}

impl std::fmt::Debug for ConnectionRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionRegistry")
            .field("buckets", &self.inner.lock().len())
            .finish()
    }
}

impl ConnectionRegistry {
    /// Register a connection and return the watch id its `closed()`
    /// watcher must hand back to [`Self::remove_and_is_empty`].
    pub fn insert(
        &self,
        endpoint_id: EndpointId,
        stable_id: StableConnId,
        handle: Arc<dyn ConnHandle>,
    ) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.inner
            .lock()
            .entry((endpoint_id, stable_id))
            .or_default()
            .insert(id, handle);
        id
    }

    /// Drop one watch and report whether its (endpoint, pipe) bucket is
    /// now empty — i.e. whether the last connection on that pipe is gone.
    pub fn remove_and_is_empty(
        &self,
        endpoint_id: EndpointId,
        stable_id: StableConnId,
        watch_id: u64,
    ) -> bool {
        let mut inner = self.inner.lock();
        let key = (endpoint_id, stable_id);
        let Some(ids) = inner.get_mut(&key) else {
            return true;
        };
        ids.remove(&watch_id);
        if ids.is_empty() {
            inner.remove(&key);
            true
        } else {
            false
        }
    }

    /// Close every connection whose last path rides `pipe`, and return
    /// how many were closed. Buckets for all endpoints are considered:
    /// the pipe is going away regardless of who authenticated over it.
    ///
    /// Entries are left in place — the `closed()` watcher the hook
    /// installed is the sole owner of removal, and closing here is what
    /// makes it fire.
    pub fn close_pipe(&self, pipe: StableConnId, code: VarInt, reason: &[u8]) -> usize {
        // Collect under the lock, close outside it: closing wakes the
        // watcher tasks, which come straight back here to remove
        // themselves.
        let handles: Vec<Arc<dyn ConnHandle>> = {
            let inner = self.inner.lock();
            inner
                .iter()
                .filter(|((_, id), _)| *id == pipe)
                .flat_map(|(_, watches)| watches.values().cloned())
                .collect()
        };
        handles
            .iter()
            .filter(|handle| handle.close_if_only_path(pipe, code, reason))
            .count()
    }
}

/// Close every iroh connection riding a pipe the dedup rule just
/// evicted. Their only path is about to disappear and iroh has no way
/// to notice, so without this the application is left blocked on a read
/// that never returns. The pipe that won the eviction is already
/// routable by the time this runs, so a redial lands straight on it —
/// hence [`BLE_CLOSE_CODE_RETRY`].
///
/// Returns the total number of connections closed.
pub fn close_evicted_pipes(registry: &ConnectionRegistry, evicted: &[StableConnId]) -> usize {
    let mut total = 0;
    for pipe in evicted {
        let closed = registry.close_pipe(
            *pipe,
            VarInt::from_u32(BLE_CLOSE_CODE_RETRY),
            BLE_CLOSE_REASON_EVICTED,
        );
        if closed > 0 {
            tracing::info!(
                evicted_pipe = %pipe,
                closed,
                "closed iroh connections on an evicted BLE pipe"
            );
        }
        total += closed;
    }
    total
}

/// A [`ConnHandle`] that records close attempts instead of talking to
/// iroh. Lets the pipe-teardown paths be exercised without a live
/// endpoint.
#[cfg(any(test, feature = "testing"))]
#[derive(Debug, Default)]
pub struct RecordingConn {
    other_paths: bool,
    closes: parking_lot::Mutex<Vec<(u64, Vec<u8>)>>,
}

#[cfg(any(test, feature = "testing"))]
impl RecordingConn {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A connection that still holds a path the dying pipe doesn't own,
    /// and so must survive the close.
    #[must_use]
    pub fn with_other_paths() -> Arc<Self> {
        Arc::new(Self {
            other_paths: true,
            closes: parking_lot::Mutex::new(Vec::new()),
        })
    }

    /// The `(code, reason)` of every close this connection accepted.
    #[must_use]
    pub fn closes(&self) -> Vec<(u64, Vec<u8>)> {
        self.closes.lock().clone()
    }
}

#[cfg(any(test, feature = "testing"))]
impl ConnHandle for RecordingConn {
    fn close_if_only_path(&self, _pipe: StableConnId, code: VarInt, reason: &[u8]) -> bool {
        if self.other_paths {
            return false;
        }
        self.closes
            .lock()
            .push((code.into_inner(), reason.to_vec()));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use RecordingConn as FakeConn;

    fn endpoint(seed: u8) -> EndpointId {
        iroh_base::SecretKey::from_bytes(&[seed; 32]).public()
    }

    #[test]
    fn close_pipe_closes_every_connection_on_that_pipe() {
        let registry = ConnectionRegistry::default();
        let pipe = StableConnId::for_test(1);
        let first = FakeConn::new();
        let second = FakeConn::new();
        registry.insert(endpoint(1), pipe, Arc::clone(&first) as Arc<dyn ConnHandle>);
        registry.insert(
            endpoint(2),
            pipe,
            Arc::clone(&second) as Arc<dyn ConnHandle>,
        );

        let closed = registry.close_pipe(
            pipe,
            VarInt::from_u32(BLE_CLOSE_CODE_RETRY),
            BLE_CLOSE_REASON_EVICTED,
        );

        assert_eq!(closed, 2, "both connections on the evicted pipe are closed");
        for conn in [&first, &second] {
            assert_eq!(
                conn.closes(),
                vec![(
                    u64::from(BLE_CLOSE_CODE_RETRY),
                    BLE_CLOSE_REASON_EVICTED.to_vec()
                )]
            );
        }
    }

    #[test]
    fn close_pipe_leaves_connections_on_other_pipes_alone() {
        let registry = ConnectionRegistry::default();
        let dying = StableConnId::for_test(1);
        let survivor = StableConnId::for_test(2);
        let conn = FakeConn::new();
        registry.insert(
            endpoint(1),
            survivor,
            Arc::clone(&conn) as Arc<dyn ConnHandle>,
        );

        let closed = registry.close_pipe(
            dying,
            VarInt::from_u32(BLE_CLOSE_CODE_RETRY),
            BLE_CLOSE_REASON_EVICTED,
        );

        assert_eq!(closed, 0);
        assert!(conn.closes().is_empty());
    }

    #[test]
    fn close_pipe_spares_a_connection_that_still_has_another_path() {
        let registry = ConnectionRegistry::default();
        let pipe = StableConnId::for_test(1);
        let multipath = FakeConn::with_other_paths();
        let single = FakeConn::new();
        registry.insert(
            endpoint(1),
            pipe,
            Arc::clone(&multipath) as Arc<dyn ConnHandle>,
        );
        registry.insert(
            endpoint(2),
            pipe,
            Arc::clone(&single) as Arc<dyn ConnHandle>,
        );

        let closed = registry.close_pipe(
            pipe,
            VarInt::from_u32(BLE_CLOSE_CODE_RETRY),
            BLE_CLOSE_REASON_PIPE_CLOSED,
        );

        assert_eq!(closed, 1, "only the single-path connection is closed");
        assert!(multipath.closes().is_empty());
        assert_eq!(
            single.closes(),
            vec![(
                u64::from(BLE_CLOSE_CODE_RETRY),
                BLE_CLOSE_REASON_PIPE_CLOSED.to_vec()
            )]
        );
    }

    #[test]
    fn close_pipe_on_an_unknown_pipe_is_a_noop() {
        let registry = ConnectionRegistry::default();
        assert_eq!(
            registry.close_pipe(
                StableConnId::for_test(9),
                VarInt::from_u32(BLE_CLOSE_CODE_RETRY),
                BLE_CLOSE_REASON_PIPE_CLOSED
            ),
            0
        );
    }

    #[test]
    fn only_reports_empty_after_last_watch_is_removed() {
        let registry = ConnectionRegistry::default();
        let endpoint_id = endpoint(1);
        let stable_id = StableConnId::for_test(7);

        let first = registry.insert(endpoint_id, stable_id, FakeConn::new());
        let second = registry.insert(endpoint_id, stable_id, FakeConn::new());

        assert!(
            !registry.remove_and_is_empty(endpoint_id, stable_id, first),
            "first close must not report empty while another connection is active"
        );
        assert!(
            registry.remove_and_is_empty(endpoint_id, stable_id, second),
            "last close reports the peer/stable-id bucket empty"
        );
    }

    #[test]
    fn buckets_by_stable_id() {
        let registry = ConnectionRegistry::default();
        let endpoint_id = endpoint(2);
        let old_id = StableConnId::for_test(8);
        let new_id = StableConnId::for_test(9);

        let old_watch = registry.insert(endpoint_id, old_id, FakeConn::new());
        let _new_watch = registry.insert(endpoint_id, new_id, FakeConn::new());

        assert!(
            registry.remove_and_is_empty(endpoint_id, old_id, old_watch),
            "old stable-id bucket is empty even if replacement stable-id has an active connection"
        );
    }
}
