//! Signed internal requests between nodes.
//!
//! A request from one node to another carries:
//!
//! - `x-roll-code-node`: the sender's node id;
//! - `x-roll-code-ts`: the sender's wall clock, in ms since the epoch;
//! - `x-roll-code-sig`: hex HMAC-SHA256, keyed with the cluster secret, over
//!   `roll-code-sig-v1\n{node}\n{ts}\n{METHOD}\n{path_and_query}\n{hop}\n{client}`, where `hop`
//!   and `client` are the values of `x-roll-code-hop` and `x-roll-code-client` (empty when
//!   absent). Signing those two means a forwarded request's hop count and client address can't
//!   be altered or added in transit.
//!
//! The receiver recomputes the MAC and compares it in constant time, and refuses timestamps
//! more than [`MAX_SKEW`] away from its own clock, so node clocks must agree to within a minute
//! (lease timing never depends on them). The body isn't signed: put the cluster on a private
//! network (or one with TLS between nodes), as `docs/code/cluster.md` says.

use axum::http::{HeaderMap, HeaderValue, Method};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::time::Duration;

pub const NODE_HEADER: &str = "x-roll-code-node";
pub const TS_HEADER: &str = "x-roll-code-ts";
pub const SIG_HEADER: &str = "x-roll-code-sig";
/// How many nodes have forwarded the request so far (1 after the first forward).
pub const HOP_HEADER: &str = "x-roll-code-hop";
/// The original client's address, as the first node saw it.
pub const CLIENT_HEADER: &str = "x-roll-code-client";
/// Set on a 503 that means "this node can't serve that key right now; route again", as opposed
/// to a 503 from the application. Forwarders retry on it.
pub const UNAVAILABLE_HEADER: &str = "x-roll-code-unavailable";

/// Every header this crate owns. A request carrying any of them must be signed.
pub const HEADERS: [&str; 5] = [NODE_HEADER, TS_HEADER, SIG_HEADER, HOP_HEADER, CLIENT_HEADER];

/// The largest difference between the sender's and the receiver's clocks that's accepted.
pub const MAX_SKEW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    #[error("missing header {0}")]
    Missing(&'static str),
    #[error("malformed header {0}")]
    Malformed(&'static str),
    #[error("timestamp is {0} ms away from this node's clock")]
    Stale(i64),
    #[error("bad signature")]
    BadSignature,
}

fn mac(secret: &[u8], node: &str, ts: i64, method: &Method, path_and_query: &str, hop: &str, client: &str) -> Hmac<Sha256> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("HMAC takes keys of any length");
    for (i, part) in ["roll-code-sig-v1", node, &ts.to_string(), method.as_str(), path_and_query, hop, client].iter().enumerate() {
        if i > 0 {
            m.update(b"\n");
        }
        m.update(part.as_bytes());
    }
    m
}

/// The signing headers for a request (plus the hop and client headers, when given, which the
/// signature covers).
pub(crate) fn sign(secret: &[u8], node: &str, method: &Method, path_and_query: &str, hop: Option<u8>, client: Option<&str>) -> HeaderMap {
    let ts = code_sql::now_ms();
    let hop_s = hop.map(|h| h.to_string()).unwrap_or_default();
    // A client address that isn't a valid header value is dropped rather than sent mangled.
    let client = client.filter(|c| HeaderValue::from_str(c).is_ok()).unwrap_or("");
    let sig = mac(secret, node, ts, method, path_and_query, &hop_s, client).finalize().into_bytes();
    let mut h = HeaderMap::new();
    h.insert(NODE_HEADER, HeaderValue::from_str(node).expect("node ids are validated header values"));
    h.insert(TS_HEADER, HeaderValue::from(ts));
    h.insert(SIG_HEADER, HeaderValue::from_str(&hex::encode(sig)).expect("hex"));
    if hop.is_some() {
        h.insert(HOP_HEADER, HeaderValue::from_str(&hop_s).expect("digits"));
    }
    if !client.is_empty() {
        h.insert(CLIENT_HEADER, HeaderValue::from_str(client).expect("checked above"));
    }
    h
}

fn one<'a>(headers: &'a HeaderMap, name: &'static str) -> Result<Option<&'a str>, VerifyError> {
    let mut all = headers.get_all(name).iter();
    let Some(v) = all.next() else { return Ok(None) };
    if all.next().is_some() {
        return Err(VerifyError::Malformed(name));
    }
    v.to_str().map(Some).map_err(|_| VerifyError::Malformed(name))
}

/// Check a request's signature; the sending node's id if it's good.
pub(crate) fn verify(secret: &[u8], headers: &HeaderMap, method: &Method, path_and_query: &str) -> Result<String, VerifyError> {
    let node = one(headers, NODE_HEADER)?.ok_or(VerifyError::Missing(NODE_HEADER))?;
    let ts = one(headers, TS_HEADER)?.ok_or(VerifyError::Missing(TS_HEADER))?;
    let sig = one(headers, SIG_HEADER)?.ok_or(VerifyError::Missing(SIG_HEADER))?;
    let hop = one(headers, HOP_HEADER)?.unwrap_or("");
    let client = one(headers, CLIENT_HEADER)?.unwrap_or("");
    let ts: i64 = ts.parse().map_err(|_| VerifyError::Malformed(TS_HEADER))?;
    if !hop.is_empty() && hop.parse::<u8>().is_err() {
        return Err(VerifyError::Malformed(HOP_HEADER));
    }
    let sig = hex::decode(sig).map_err(|_| VerifyError::Malformed(SIG_HEADER))?;
    // Check the MAC before the clock, so a forged request learns nothing about our clock.
    mac(secret, node, ts, method, path_and_query, hop, client).verify_slice(&sig).map_err(|_| VerifyError::BadSignature)?;
    let off = code_sql::now_ms().saturating_sub(ts);
    if off.unsigned_abs() > MAX_SKEW.as_millis() as u64 {
        return Err(VerifyError::Stale(off));
    }
    Ok(node.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_tampering() {
        let s = b"0123456789abcdef0123456789abcdef";
        let h = sign(s, "n1", &Method::POST, "/a?b=c", Some(1), Some("10.0.0.1"));
        assert_eq!(verify(s, &h, &Method::POST, "/a?b=c").unwrap(), "n1");
        assert_eq!(verify(s, &h, &Method::GET, "/a?b=c"), Err(VerifyError::BadSignature));
        assert_eq!(verify(s, &h, &Method::POST, "/a?b=d"), Err(VerifyError::BadSignature));
        assert_eq!(verify(b"another secret, same length.....", &h, &Method::POST, "/a?b=c"), Err(VerifyError::BadSignature));
        for (name, value) in [(HOP_HEADER, "2"), (CLIENT_HEADER, "10.0.0.2"), (NODE_HEADER, "n2"), (TS_HEADER, "1")] {
            let mut t = h.clone();
            t.insert(name, HeaderValue::from_static(value));
            assert!(verify(s, &t, &Method::POST, "/a?b=c").is_err(), "{name}");
        }
        let mut t = h.clone();
        t.append(SIG_HEADER, HeaderValue::from_static("00"));
        assert_eq!(verify(s, &t, &Method::POST, "/a?b=c"), Err(VerifyError::Malformed(SIG_HEADER)));
        let mut t = h.clone();
        t.remove(SIG_HEADER);
        assert_eq!(verify(s, &t, &Method::POST, "/a?b=c"), Err(VerifyError::Missing(SIG_HEADER)));
        // Plain signing covers the absence of hop and client: adding them later breaks it.
        let plain = sign(s, "n1", &Method::GET, "/p", None, None);
        assert!(verify(s, &plain, &Method::GET, "/p").is_ok());
        let mut t = plain.clone();
        t.insert(HOP_HEADER, HeaderValue::from_static("1"));
        assert_eq!(verify(s, &t, &Method::GET, "/p"), Err(VerifyError::BadSignature));
    }

    #[test]
    fn stale_timestamps_are_refused() {
        let s = b"0123456789abcdef0123456789abcdef";
        for off in [-61_000i64, 61_000] {
            let ts = code_sql::now_ms() + off;
            let sig = mac(s, "n1", ts, &Method::GET, "/p", "", "").finalize().into_bytes();
            let mut h = HeaderMap::new();
            h.insert(NODE_HEADER, HeaderValue::from_static("n1"));
            h.insert(TS_HEADER, HeaderValue::from(ts));
            h.insert(SIG_HEADER, HeaderValue::from_str(&hex::encode(sig)).unwrap());
            assert!(matches!(verify(s, &h, &Method::GET, "/p"), Err(VerifyError::Stale(_))), "{off}");
        }
    }
}
