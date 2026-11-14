//! Limits on open server-sent event streams (`/events` and MCP's event-stream answers): per
//! caller and in total. A stream holds a [`Permit`] for as long as it's open; past a limit the
//! request gets a 429 instead of a stream.

use crate::{Caller, Code};
use axum::Json;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use parking_lot::Mutex;
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Default)]
pub struct Streams {
    open: Mutex<HashMap<String, usize>>,
}

/// One open stream; dropping it (the client went away, the stream ended) frees its slot.
pub struct Permit {
    code: Arc<Code>,
    key: String,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut open = self.code.streams.open.lock();
        if let Some(n) = open.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                open.remove(&self.key);
            }
        }
        if let Some(n) = open.get_mut(TOTAL) {
            *n -= 1;
            if *n == 0 {
                open.remove(TOTAL);
            }
        }
    }
}

const TOTAL: &str = "\0total";

impl Streams {
    /// Open streams for `key` (tests and diagnostics).
    pub fn open(&self, key: &str) -> usize {
        self.open.lock().get(key).copied().unwrap_or(0)
    }
}

/// Who a stream counts against: the token for signed-in callers (a hash, never the token), the
/// client address for anonymous ones (the forwarded address, as the proxy in front saw it, only when
/// the operator trusts that proxy), and one shared bucket for anonymous callers whose address is
/// unknown.
pub fn key(caller: &Caller, token: Option<&str>, headers: &HeaderMap, peer: Option<SocketAddr>, trust_forwarded: bool) -> String {
    match caller {
        Caller::Admin if token.is_some() => format!("admin:{}", token.map(crate::hash_token).unwrap_or_default()),
        Caller::Owner { id, token: name, .. } => format!("owner:{id}:{name}"),
        Caller::App { id, .. } => format!("app:{id}"),
        // Anonymous callers, and everyone on an open daemon (they're all admin there).
        Caller::Admin | Caller::Anonymous => {
            let forwarded = trust_forwarded.then(|| forwarded_client(headers, peer.map(|p| p.ip()))).flatten().map(|ip| ip.to_string());
            match (forwarded, peer) {
                (Some(ip), _) => format!("ip:{ip}"),
                (None, Some(p)) => format!("ip:{}", p.ip()),
                (None, None) => "anonymous".into(),
            }
        }
    }
}

/// The client the trusted proxy in front saw: over every `X-Forwarded-For` line, the right-most
/// entry that isn't the proxy itself (`peer`); entries left of it are whatever the client sent.
/// Only an address counts: an entry that doesn't parse means none, and the peer counts.
fn forwarded_client(headers: &HeaderMap, peer: Option<std::net::IpAddr>) -> Option<std::net::IpAddr> {
    let all = headers.get_all("x-forwarded-for").iter().filter_map(|v| v.to_str().ok()).collect::<Vec<_>>().join(",");
    let peer = peer.map(|p| p.to_canonical());
    for hop in all.split(',').map(str::trim).filter(|h| !h.is_empty()).rev() {
        let ip = hop.parse::<std::net::IpAddr>().ok()?.to_canonical();
        if Some(ip) != peer {
            return Some(ip);
        }
    }
    None
}

impl Code {
    /// A slot for one more stream for `key`, or the 429 to answer with.
    pub fn stream_permit(self: &Arc<Self>, key: String) -> Result<Permit, Box<Response>> {
        let (per, total) = (self.config.max_streams_per_caller, self.config.max_streams);
        let mut open = self.streams.open.lock();
        let mine = open.get(&key).copied().unwrap_or(0);
        let all = open.get(TOTAL).copied().unwrap_or(0);
        if all >= total {
            return Err(too_many(&format!("the server has {total} event streams open; try again later")));
        }
        // Anonymous callers whose address is unknown share one bucket: give it the total.
        if key != "anonymous" && mine >= per {
            return Err(too_many(&format!("too many event streams open for this caller (at most {per}); close one first")));
        }
        *open.entry(key.clone()).or_insert(0) += 1;
        *open.entry(TOTAL.into()).or_insert(0) += 1;
        Ok(Permit { code: self.clone(), key })
    }
}

pub(crate) fn too_many(msg: &str) -> Box<Response> {
    let mut r = (StatusCode::TOO_MANY_REQUESTS, Json(json!({ "error": msg }))).into_response();
    r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
    Box::new(r)
}

#[cfg(test)]
mod forwarded_tests {
    /// Behind a trusted proxy, a client can't pick its bucket by writing X-Forwarded-For itself.
    #[test]
    fn the_client_is_what_the_proxy_saw() {
        let mut h = axum::http::HeaderMap::new();
        h.append("x-forwarded-for", "1.1.1.1, 2.2.2.2".parse().unwrap());
        h.append("x-forwarded-for", "203.0.113.9".parse().unwrap());
        assert_eq!(super::forwarded_client(&h, None), Some("203.0.113.9".parse().unwrap()));
        // A proxy that lists itself last is skipped.
        let mut own = axum::http::HeaderMap::new();
        own.insert("x-forwarded-for", "1.1.1.1, 198.51.100.1, 10.0.0.1".parse().unwrap());
        assert_eq!(super::forwarded_client(&own, Some("10.0.0.1".parse().unwrap())), Some("198.51.100.1".parse().unwrap()));
        let mut bad = axum::http::HeaderMap::new();
        bad.insert("x-forwarded-for", "1.1.1.1, nonsense".parse().unwrap());
        assert_eq!(super::forwarded_client(&bad, None), None, "an entry that isn't an address counts for nothing");
    }
}
