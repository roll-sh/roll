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
/// client address for anonymous ones (the forwarded address only when the operator trusts the
/// proxy in front), and one shared bucket for anonymous callers whose address is unknown.
pub fn key(caller: &Caller, token: Option<&str>, headers: &HeaderMap, peer: Option<SocketAddr>, trust_forwarded: bool) -> String {
    match caller {
        Caller::Admin if token.is_some() => format!("admin:{}", token.map(crate::hash_token).unwrap_or_default()),
        Caller::Owner { id, token: name, .. } => format!("owner:{id}:{name}"),
        Caller::App { id, .. } => format!("app:{id}"),
        // Anonymous callers, and everyone on an open daemon (they're all admin there).
        Caller::Admin | Caller::Anonymous => {
            let forwarded = trust_forwarded
                .then(|| headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).and_then(|v| v.split(',').next()).map(|v| v.trim().to_string()))
                .flatten()
                .filter(|v| !v.is_empty());
            match (forwarded, peer) {
                (Some(ip), _) => format!("ip:{ip}"),
                (None, Some(p)) => format!("ip:{}", p.ip()),
                (None, None) => "anonymous".into(),
            }
        }
    }
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

fn too_many(msg: &str) -> Box<Response> {
    let mut r = (StatusCode::TOO_MANY_REQUESTS, Json(json!({ "error": msg }))).into_response();
    r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
    Box::new(r)
}
