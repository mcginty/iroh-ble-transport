//! Transport-internal routing table.
//!
//! `TransportRouting` owns two pieces of state that belong *above* the per-peer
//! state machine:
//! 1. `Token ↔ peer-identity` — the iroh-facing address allocation. Tokens are
//!    keyed on `KeyPrefix` (first 12 bytes of the Ed25519 public key) whenever
//!    possible so that a cached iroh address survives a peer changing its
//!    `DeviceId` — the typical case is Android MAC randomization on app
//!    restart. A per-`DeviceId` fallback exists for the peripheral recv path
//!    that sees an inbound connection before the central scan has noted the
//!    advertisement.
//! 2. `KeyPrefix → DeviceId` — the scan-discovery map used as a **dial hint**
//!    by `BleAddressLookup::resolve` and, indirectly, as the live indirection
//!    that lets prefix-keyed tokens follow a peer's MAC rotation.

use std::collections::HashMap;
use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Waker;

use blew::DeviceId;
use iroh_base::{CustomAddr, EndpointId};

use crate::transport::peer::{KEY_PREFIX_LEN, KeyPrefix};
use crate::transport::transport::BLE_TRANSPORT_ID;

pub const TOKEN_LEN: usize = 8;
pub type Token = u64;

#[derive(Debug, Default)]
pub struct TransportRouting {
    inner: Mutex<RoutingInner>,
    next_token: AtomicU64,
    /// Wakers parked by `BleSender::poll_send` while it waits for a
    /// prefix-keyed token to map to a `DeviceId`. Drained and woken whenever a
    /// new `KeyPrefix → DeviceId` mapping is recorded.
    send_wakers: Mutex<Vec<Waker>>,
}

#[derive(Debug, Default)]
struct RoutingInner {
    prefix_to_token: HashMap<KeyPrefix, Token>,
    device_to_token: HashMap<DeviceId, Token>,
    token_origin: HashMap<Token, TokenOrigin>,
    discovered: HashMap<KeyPrefix, DeviceId>,
}

#[derive(Debug, Clone)]
enum TokenOrigin {
    Prefix(KeyPrefix),
    Device(DeviceId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryUpdate {
    Unchanged,
    New,
    Replaced { previous: DeviceId },
}

impl TransportRouting {
    pub fn new() -> Self {
        Self::default()
    }

    /// Tokens are always non-zero: `0` is reserved as a sentinel the transport
    /// uses for `local_addr` and anywhere else a "no peer" placeholder is
    /// needed.
    fn next_token(&self) -> Token {
        self.next_token
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
    }

    /// Mint (or recover) the stable token for `prefix`. Used on the dial side
    /// by `BleAddressLookup::resolve` and on the recv side whenever the
    /// source DeviceId has been noted in `discovered`.
    pub fn mint_token_for_prefix(&self, prefix: KeyPrefix) -> Token {
        let mut inner = self.inner.lock().expect("routing mutex poisoned");
        if let Some(&t) = inner.prefix_to_token.get(&prefix) {
            return t;
        }
        let t = self.next_token();
        inner.prefix_to_token.insert(prefix, t);
        inner.token_origin.insert(t, TokenOrigin::Prefix(prefix));
        t
    }

    /// Fallback for the peripheral recv path when we haven't seen the peer's
    /// advertising yet. Tokens minted this way are only stable for the
    /// lifetime of `device`; prefer `mint_token_for_source`.
    pub fn mint_token_for_device(&self, device: &DeviceId) -> Token {
        let mut inner = self.inner.lock().expect("routing mutex poisoned");
        if let Some(&t) = inner.device_to_token.get(device) {
            return t;
        }
        let t = self.next_token();
        inner.device_to_token.insert(device.clone(), t);
        inner
            .token_origin
            .insert(t, TokenOrigin::Device(device.clone()));
        t
    }

    /// Recv-path minting: return the stable prefix-keyed token for `device`
    /// if its advertising has been noted, otherwise fall back to a
    /// device-keyed token.
    pub fn mint_token_for_source(&self, device: &DeviceId) -> Token {
        let prefix = {
            let inner = self.inner.lock().expect("routing mutex poisoned");
            inner
                .discovered
                .iter()
                .find_map(|(p, d)| (d == device).then_some(*p))
        };
        match prefix {
            Some(prefix) => self.mint_token_for_prefix(prefix),
            None => self.mint_token_for_device(device),
        }
    }

    /// Resolve a token back to the `DeviceId` that should receive outbound
    /// traffic. For prefix-keyed tokens this follows `discovered[prefix]`
    /// live, so a peer's MAC rotation transparently redirects traffic without
    /// any iroh-level re-resolution.
    pub fn device_for_token(&self, token: Token) -> Option<DeviceId> {
        let inner = self.inner.lock().expect("routing mutex poisoned");
        match inner.token_origin.get(&token)? {
            TokenOrigin::Prefix(prefix) => inner.discovered.get(prefix).cloned(),
            TokenOrigin::Device(device) => Some(device.clone()),
        }
    }

    /// Record that `prefix` has been observed mapping to `device`.
    ///
    /// Returns `DiscoveryUpdate::Unchanged` when the mapping already pointed
    /// at this `DeviceId`. Returns `DiscoveryUpdate::New` when the prefix is
    /// being recorded for the first time. Returns
    /// `DiscoveryUpdate::Replaced { previous }` when the prefix used to map
    /// to a different `DeviceId` — callers use this to evict the stale entry
    /// (e.g. peer restart with a new MAC).
    pub fn note_discovery(&self, prefix: KeyPrefix, device: DeviceId) -> DiscoveryUpdate {
        let update = {
            let mut inner = self.inner.lock().expect("routing mutex poisoned");
            match inner.discovered.insert(prefix, device.clone()) {
                None => DiscoveryUpdate::New,
                Some(previous) if previous == device => DiscoveryUpdate::Unchanged,
                Some(previous) => DiscoveryUpdate::Replaced { previous },
            }
        };
        if !matches!(update, DiscoveryUpdate::Unchanged) {
            self.wake_send_waiters();
        }
        update
    }

    /// Park a `BleSender::poll_send` waker. Used together with a re-check of
    /// `device_for_token` in the standard register-then-check pattern so a
    /// discovery update racing the registration is not missed.
    pub fn register_send_waker(&self, waker: &Waker) {
        let mut wakers = self.send_wakers.lock().expect("send waker mutex poisoned");
        if !wakers.iter().any(|w| w.will_wake(waker)) {
            wakers.push(waker.clone());
        }
    }

    fn wake_send_waiters(&self) {
        let wakers: Vec<Waker> =
            std::mem::take(&mut *self.send_wakers.lock().expect("send waker mutex poisoned"));
        for w in wakers {
            w.wake();
        }
    }

    pub fn device_for_endpoint(&self, endpoint_id: &EndpointId) -> Option<DeviceId> {
        let prefix = prefix_from_endpoint(endpoint_id);
        self.inner
            .lock()
            .expect("routing mutex poisoned")
            .discovered
            .get(&prefix)
            .cloned()
    }

    /// Drop the device-keyed fallback token for a peer that has gone away.
    /// Prefix-keyed tokens and `discovered` entries are intentionally
    /// preserved so a later dial for the same identity re-resolves via the
    /// stable path.
    pub fn forget_device(&self, device: &DeviceId) {
        let mut inner = self.inner.lock().expect("routing mutex poisoned");
        if let Some(t) = inner.device_to_token.remove(device) {
            inner.token_origin.remove(&t);
        }
    }
}

pub fn prefix_from_endpoint(endpoint_id: &EndpointId) -> KeyPrefix {
    let mut prefix = [0u8; KEY_PREFIX_LEN];
    prefix.copy_from_slice(&endpoint_id.as_bytes()[..KEY_PREFIX_LEN]);
    prefix
}

pub fn token_custom_addr(token: Token) -> CustomAddr {
    CustomAddr::from_parts(BLE_TRANSPORT_ID, &token.to_le_bytes())
}

pub fn parse_token_addr(addr: &CustomAddr) -> io::Result<Token> {
    if addr.id() != BLE_TRANSPORT_ID {
        return Err(io::Error::other("not a BLE transport address"));
    }
    if addr.data().len() != TOKEN_LEN {
        return Err(io::Error::other("BLE custom addr has wrong length"));
    }
    let mut buf = [0u8; TOKEN_LEN];
    buf.copy_from_slice(addr.data());
    Ok(u64::from_le_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_token_for_prefix_is_idempotent() {
        let r = TransportRouting::new();
        let p: KeyPrefix = [1u8; KEY_PREFIX_LEN];
        let t1 = r.mint_token_for_prefix(p);
        let t2 = r.mint_token_for_prefix(p);
        assert_eq!(t1, t2);
    }

    #[test]
    fn device_for_token_follows_live_discovery_for_prefix_tokens() {
        let r = TransportRouting::new();
        let p: KeyPrefix = [2u8; KEY_PREFIX_LEN];
        let d1 = blew::DeviceId::from("old-mac");
        let d2 = blew::DeviceId::from("new-mac");
        r.note_discovery(p, d1.clone());
        let token = r.mint_token_for_prefix(p);
        assert_eq!(r.device_for_token(token).as_ref(), Some(&d1));
        // Simulate Android MAC randomization: same prefix, new DeviceId.
        r.note_discovery(p, d2.clone());
        assert_eq!(
            r.device_for_token(token).as_ref(),
            Some(&d2),
            "prefix-keyed token must follow discovery changes"
        );
    }

    #[test]
    fn mint_token_for_source_prefers_prefix_when_discovered() {
        let r = TransportRouting::new();
        let p: KeyPrefix = [3u8; KEY_PREFIX_LEN];
        let d = blew::DeviceId::from("dev-src");
        r.note_discovery(p, d.clone());
        let from_source = r.mint_token_for_source(&d);
        let from_prefix = r.mint_token_for_prefix(p);
        assert_eq!(from_source, from_prefix);
    }

    #[test]
    fn mint_token_for_source_falls_back_to_device_when_unknown() {
        let r = TransportRouting::new();
        let d = blew::DeviceId::from("unknown-mac");
        let t = r.mint_token_for_source(&d);
        assert_eq!(r.device_for_token(t).as_ref(), Some(&d));
    }

    #[test]
    fn distinct_prefixes_get_distinct_tokens() {
        let r = TransportRouting::new();
        let t1 = r.mint_token_for_prefix([1u8; KEY_PREFIX_LEN]);
        let t2 = r.mint_token_for_prefix([2u8; KEY_PREFIX_LEN]);
        assert_ne!(t1, t2);
    }

    #[test]
    fn discovery_map_resolves_endpoint_to_device() {
        let r = TransportRouting::new();
        let d = blew::DeviceId::from("xx");
        let secret = iroh_base::SecretKey::from_bytes(&[7u8; 32]);
        let eid = secret.public();
        let prefix: KeyPrefix = eid.as_bytes()[..KEY_PREFIX_LEN].try_into().unwrap();
        r.note_discovery(prefix, d.clone());
        assert_eq!(r.device_for_endpoint(&eid), Some(d));
    }

    #[test]
    fn note_discovery_dedupes_repeats_but_reports_device_changes() {
        let r = TransportRouting::new();
        let prefix: KeyPrefix = [9u8; KEY_PREFIX_LEN];
        let d1 = blew::DeviceId::from("aa:bb");
        let d2 = blew::DeviceId::from("cc:dd");
        assert_eq!(
            r.note_discovery(prefix, d1.clone()),
            DiscoveryUpdate::New,
            "first sighting is new"
        );
        assert_eq!(
            r.note_discovery(prefix, d1.clone()),
            DiscoveryUpdate::Unchanged,
            "repeat is unchanged"
        );
        assert_eq!(
            r.note_discovery(prefix, d2),
            DiscoveryUpdate::Replaced { previous: d1 },
            "same prefix mapping to a new device replaces and reports the previous"
        );
    }

    #[test]
    fn forget_device_clears_fallback_token() {
        let r = TransportRouting::new();
        let d = blew::DeviceId::from("gone");
        let t = r.mint_token_for_device(&d);
        r.forget_device(&d);
        assert!(r.device_for_token(t).is_none());
    }

    #[test]
    fn forget_device_leaves_prefix_token_intact() {
        let r = TransportRouting::new();
        let p: KeyPrefix = [5u8; KEY_PREFIX_LEN];
        let d = blew::DeviceId::from("still-here");
        r.note_discovery(p, d.clone());
        let t = r.mint_token_for_prefix(p);
        r.forget_device(&d);
        assert_eq!(
            r.device_for_token(t).as_ref(),
            Some(&d),
            "forget_device must not disturb prefix-keyed routing"
        );
    }

    #[test]
    fn token_custom_addr_roundtrip() {
        let addr = token_custom_addr(0xdead_beef_cafe_f00d);
        assert_eq!(parse_token_addr(&addr).unwrap(), 0xdead_beef_cafe_f00d);
    }

    #[test]
    fn parse_token_addr_rejects_wrong_transport_id() {
        let wrong = iroh_base::CustomAddr::from_parts(0x12_34_56, &0u64.to_le_bytes());
        assert!(parse_token_addr(&wrong).is_err());
    }

    #[test]
    fn parse_token_addr_rejects_wrong_length() {
        let wrong = iroh_base::CustomAddr::from_parts(
            crate::transport::transport::BLE_TRANSPORT_ID,
            &[0u8; 4],
        );
        assert!(parse_token_addr(&wrong).is_err());
    }

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::task::{Wake, Waker};

    struct CountingWaker(AtomicUsize);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, AtomicOrdering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }

    fn counting_waker() -> (Arc<CountingWaker>, Waker) {
        let inner = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&inner));
        (inner, waker)
    }

    #[test]
    fn note_discovery_wakes_send_waiters_on_new_prefix() {
        let r = TransportRouting::new();
        let (counter, waker) = counting_waker();
        r.register_send_waker(&waker);
        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 0);

        let prefix: KeyPrefix = [11u8; KEY_PREFIX_LEN];
        assert_eq!(
            r.note_discovery(prefix, blew::DeviceId::from("dev-late")),
            DiscoveryUpdate::New
        );

        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 1);
    }

    #[test]
    fn note_discovery_does_not_wake_on_unchanged_repeat() {
        let r = TransportRouting::new();
        let prefix: KeyPrefix = [12u8; KEY_PREFIX_LEN];
        let device = blew::DeviceId::from("steady");
        assert_eq!(
            r.note_discovery(prefix, device.clone()),
            DiscoveryUpdate::New
        );

        let (counter, waker) = counting_waker();
        r.register_send_waker(&waker);

        assert_eq!(r.note_discovery(prefix, device), DiscoveryUpdate::Unchanged);
        assert_eq!(
            counter.0.load(AtomicOrdering::SeqCst),
            0,
            "Unchanged repeat must not wake send waiters"
        );
    }

    #[test]
    fn note_discovery_wakes_multiple_send_waiters() {
        let r = TransportRouting::new();
        let mut counters = Vec::new();
        for _ in 0..4 {
            let (counter, waker) = counting_waker();
            r.register_send_waker(&waker);
            counters.push(counter);
        }

        let prefix: KeyPrefix = [13u8; KEY_PREFIX_LEN];
        r.note_discovery(prefix, blew::DeviceId::from("broadcast"));

        for c in counters {
            assert_eq!(c.0.load(AtomicOrdering::SeqCst), 1);
        }
    }

    #[test]
    fn note_discovery_wakes_on_replaced_mapping() {
        let r = TransportRouting::new();
        let prefix: KeyPrefix = [14u8; KEY_PREFIX_LEN];
        r.note_discovery(prefix, blew::DeviceId::from("old"));

        let (counter, waker) = counting_waker();
        r.register_send_waker(&waker);

        let update = r.note_discovery(prefix, blew::DeviceId::from("new"));
        assert!(matches!(update, DiscoveryUpdate::Replaced { .. }));
        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 1);
    }

    #[test]
    fn register_send_waker_dedupes_identical_waker() {
        let r = TransportRouting::new();
        let (counter, waker) = counting_waker();
        r.register_send_waker(&waker);
        r.register_send_waker(&waker);
        r.register_send_waker(&waker);

        r.note_discovery([15u8; KEY_PREFIX_LEN], blew::DeviceId::from("dedupe"));
        assert_eq!(
            counter.0.load(AtomicOrdering::SeqCst),
            1,
            "dedup means each unique waker fires once per update"
        );
    }

    #[test]
    fn send_wakers_are_drained_after_firing() {
        let r = TransportRouting::new();
        let (counter, waker) = counting_waker();
        r.register_send_waker(&waker);
        r.note_discovery([16u8; KEY_PREFIX_LEN], blew::DeviceId::from("first"));
        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 1);

        // No re-registration → second update must not re-wake the same waker.
        r.note_discovery([17u8; KEY_PREFIX_LEN], blew::DeviceId::from("second"));
        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 1);
    }
}
