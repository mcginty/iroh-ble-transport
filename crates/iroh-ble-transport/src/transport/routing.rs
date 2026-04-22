//! Address-layer helpers shared by the transport.
//!
//! All per-peer routing state lives in
//! [`crate::transport::routing_v2::Routing`] — this module is now
//! just the three stateless pieces of plumbing that don't fit
//! elsewhere:
//!
//! - [`prefix_from_endpoint`]: the canonical projection from an
//!   Ed25519 `EndpointId` to the 12-byte `KeyPrefix` embedded in the
//!   advertising service UUID.
//! - [`token_custom_addr`] / [`parse_token_addr`]: the `CustomAddr`
//!   wire format — an 8-byte little-endian `u64` token prefixed by
//!   the BLE transport id. Tokens are `routing_v2::StableConnId`
//!   values round-tripped through iroh's `CustomAddr` carrier.

use std::io;

use iroh_base::{CustomAddr, EndpointId};

use crate::transport::peer::{KEY_PREFIX_LEN, KeyPrefix};
use crate::transport::transport::BLE_TRANSPORT_ID;

/// Length in bytes of the opaque token payload embedded in a BLE
/// `CustomAddr`. The token is a little-endian `u64`
/// (`routing_v2::StableConnId::as_u64()`).
pub const TOKEN_LEN: usize = 8;

/// Project an `EndpointId` to its advertising-service `KeyPrefix`.
/// The first 12 bytes of the Ed25519 public key are embedded as the
/// low 12 bytes of our service UUID
/// (`69726f00-XXXX-XXXX-XXXX-XXXXXXXXXXXX`); both sides of a dial
/// agree on this projection without any coordination.
#[must_use]
pub fn prefix_from_endpoint(endpoint_id: &EndpointId) -> KeyPrefix {
    let mut prefix = [0u8; KEY_PREFIX_LEN];
    prefix.copy_from_slice(&endpoint_id.as_bytes()[..KEY_PREFIX_LEN]);
    prefix
}

/// Wrap a `u64` token as a BLE-transport `CustomAddr`.
#[must_use]
pub fn token_custom_addr(token: u64) -> CustomAddr {
    CustomAddr::from_parts(BLE_TRANSPORT_ID, &token.to_le_bytes())
}

/// Parse a `CustomAddr` as a BLE-transport token. Returns an error
/// if the address is for a different transport or has the wrong
/// length.
pub fn parse_token_addr(addr: &CustomAddr) -> io::Result<u64> {
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
    fn token_custom_addr_roundtrip() {
        let addr = token_custom_addr(0xdead_beef_cafe_f00d);
        assert_eq!(parse_token_addr(&addr).unwrap(), 0xdead_beef_cafe_f00d);
    }

    #[test]
    fn parse_token_addr_rejects_wrong_transport_id() {
        let wrong = CustomAddr::from_parts(0x12_34_56, &0u64.to_le_bytes());
        assert!(parse_token_addr(&wrong).is_err());
    }

    #[test]
    fn parse_token_addr_rejects_wrong_length() {
        let wrong = CustomAddr::from_parts(BLE_TRANSPORT_ID, &[0u8; 4]);
        assert!(parse_token_addr(&wrong).is_err());
    }

    #[test]
    fn prefix_from_endpoint_extracts_first_twelve_bytes() {
        let secret = iroh_base::SecretKey::from_bytes(&[7u8; 32]);
        let eid = secret.public();
        let prefix = prefix_from_endpoint(&eid);
        assert_eq!(prefix, eid.as_bytes()[..KEY_PREFIX_LEN]);
    }
}
