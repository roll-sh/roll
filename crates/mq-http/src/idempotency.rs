//! `Idempotency-Key`: the first response to a key is stored (memory + metadata store) and
//! replayed for the key's lifetime, so retried publishes don't duplicate messages.

use crate::HttpApi;
use crate::error::ApiError;
use axum::http::{HeaderMap, StatusCode};
use mq_core::meta::MetaStore;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tracing::debug;

pub const HEADER: &str = "idempotency-key";

#[derive(Clone)]
enum Entry {
    InFlight { fingerprint: String },
    Done(Stored),
}

#[derive(Clone, Debug)]
pub struct Stored {
    pub status: StatusCode,
    pub body: Value,
    fingerprint: String,
    expires_ms: u64,
}

pub enum Begin {
    /// Run the request, then call [`IdemStore::finish`] (or [`IdemStore::abort`]).
    Fresh,
    Replay(Stored),
}

pub struct IdemStore {
    meta: Arc<dyn MetaStore>,
    ttl_ms: u64,
    max: usize,
    mem: Mutex<(HashMap<String, Entry>, VecDeque<String>)>,
}

/// Storage key: the caller, the endpoint and their key, hashed.
pub fn scoped_key(identity: &str, method: &str, path: &str, key: &str) -> String {
    hex::encode(Sha256::digest(format!("{identity}\n{method} {path}\n{key}").as_bytes()))
}

pub fn fingerprint(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}

/// The request's `Idempotency-Key`, validated.
pub fn header_key(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let Some(v) = headers.get(HEADER) else { return Ok(None) };
    let s = v.to_str().map_err(|_| ApiError::bad_request("Idempotency-Key must be ASCII"))?.trim();
    if s.is_empty() || s.len() > 255 {
        return Err(ApiError::bad_request("Idempotency-Key must be 1-255 characters"));
    }
    Ok(Some(s.to_string()))
}

impl IdemStore {
    pub fn new(meta: Arc<dyn MetaStore>, ttl_ms: u64, max: usize) -> Self {
        IdemStore { meta, ttl_ms, max: max.max(1), mem: Mutex::new((HashMap::new(), VecDeque::new())) }
    }

    pub async fn begin(&self, key: &str, fingerprint: &str) -> Result<Begin, ApiError> {
        let now = mq_core::now_ms();
        let check = |s: &Stored| -> Result<Begin, ApiError> {
            if s.fingerprint != fingerprint {
                return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "idempotency_key_reused", "this Idempotency-Key was used with a different request body"));
            }
            Ok(Begin::Replay(s.clone()))
        };
        {
            let mut g = self.mem.lock();
            match g.0.get(key) {
                Some(Entry::Done(s)) if s.expires_ms > now => return check(s),
                Some(Entry::InFlight { fingerprint: f }) => {
                    return Err(if f == fingerprint {
                        ApiError::conflict("idempotency_in_progress", "a request with this Idempotency-Key is still in progress")
                    } else {
                        ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "idempotency_key_reused", "this Idempotency-Key was used with a different request body")
                    });
                }
                _ => {}
            }
            g.0.insert(key.to_string(), Entry::InFlight { fingerprint: fingerprint.to_string() });
        }
        // Another broker (or an earlier run) may have answered it.
        if let Ok(Some(v)) = self.meta.get(crate::kind::IDEMPOTENCY, key).await
            && let Some(s) = decode(&v)
            && s.expires_ms > now
        {
            let r = check(&s);
            let mut g = self.mem.lock();
            g.0.insert(key.to_string(), Entry::Done(s));
            g.1.push_back(key.to_string());
            return r;
        }
        Ok(Begin::Fresh)
    }

    pub async fn finish(&self, key: &str, status: StatusCode, body: &Value) {
        let fingerprint = match self.mem.lock().0.get(key) {
            Some(Entry::InFlight { fingerprint }) => fingerprint.clone(),
            _ => return,
        };
        // Server errors aren't replayed: the client should retry for real.
        if status.is_server_error() {
            self.abort(key);
            return;
        }
        let s = Stored { status, body: body.clone(), fingerprint, expires_ms: mq_core::now_ms() + self.ttl_ms };
        if let Err(e) = self.meta.put(crate::kind::IDEMPOTENCY, key, &encode(&s)).await {
            debug!(error = %e, "idempotency record not persisted");
        }
        let mut g = self.mem.lock();
        g.0.insert(key.to_string(), Entry::Done(s));
        g.1.push_back(key.to_string());
        while g.1.len() > self.max {
            if let Some(old) = g.1.pop_front() {
                g.0.remove(&old);
            }
        }
    }

    pub fn abort(&self, key: &str) {
        let mut g = self.mem.lock();
        if matches!(g.0.get(key), Some(Entry::InFlight { .. })) {
            g.0.remove(key);
        }
    }

    /// Drop expired records, and the oldest ones beyond the cap.
    pub async fn cleanup(&self) -> anyhow::Result<()> {
        let now = mq_core::now_ms();
        {
            let mut g = self.mem.lock();
            g.0.retain(|_, e| !matches!(e, Entry::Done(s) if s.expires_ms <= now));
            let (map, order) = &mut *g;
            order.retain(|k| map.contains_key(k));
        }
        let mut rows: Vec<(String, u64)> =
            self.meta.list(crate::kind::IDEMPOTENCY).await?.into_iter().map(|(k, v)| (k, v["expires_ms"].as_u64().unwrap_or(0))).collect();
        rows.sort_by_key(|(_, e)| *e);
        let excess = rows.len().saturating_sub(self.max);
        for (i, (k, exp)) in rows.iter().enumerate() {
            if *exp <= now || i < excess {
                self.meta.delete(crate::kind::IDEMPOTENCY, k).await?;
            }
        }
        Ok(())
    }
}

fn encode(s: &Stored) -> Value {
    json!({"status": s.status.as_u16(), "body": s.body, "fingerprint": s.fingerprint, "expires_ms": s.expires_ms})
}

fn decode(v: &Value) -> Option<Stored> {
    Some(Stored {
        status: StatusCode::from_u16(v["status"].as_u64()? as u16).ok()?,
        body: v["body"].clone(),
        fingerprint: v["fingerprint"].as_str()?.into(),
        expires_ms: v["expires_ms"].as_u64()?,
    })
}

pub async fn cleanup_loop(api: Weak<HttpApi>) {
    let Some(mut stop) = api.upgrade().map(|a| a.stop_rx()) else { return };
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(600)) => {}
            _ = stop.wait_for(|s| *s) => return,
        }
        let Some(a) = api.upgrade() else { return };
        if let Err(e) = a.idem.cleanup().await {
            debug!(error = %e, "idempotency cleanup failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replay_and_mismatch() {
        let meta = mq_core::meta::MemMeta::new();
        let s = IdemStore::new(meta.clone(), 60_000, 2);
        assert!(matches!(s.begin("k", "f1").await.unwrap(), Begin::Fresh));
        assert_eq!(s.begin("k", "f1").await.err().unwrap().code, "idempotency_in_progress");
        s.finish("k", StatusCode::OK, &json!({"ok": 1})).await;
        match s.begin("k", "f1").await.unwrap() {
            Begin::Replay(r) => assert_eq!(r.body, json!({"ok": 1})),
            Begin::Fresh => panic!("expected replay"),
        }
        assert_eq!(s.begin("k", "f2").await.err().unwrap().status, StatusCode::UNPROCESSABLE_ENTITY);
        // A fresh store (another broker) replays from the metadata store.
        let s2 = IdemStore::new(meta.clone(), 60_000, 2);
        assert!(matches!(s2.begin("k", "f1").await.unwrap(), Begin::Replay(_)));
        // Server errors aren't stored.
        assert!(matches!(s.begin("e", "f").await.unwrap(), Begin::Fresh));
        s.finish("e", StatusCode::INTERNAL_SERVER_ERROR, &json!({})).await;
        assert!(matches!(s.begin("e", "f").await.unwrap(), Begin::Fresh));
        // The cap applies to stored records.
        for k in ["a", "b", "c"] {
            s.begin(k, "f").await.unwrap();
            s.finish(k, StatusCode::OK, &json!({})).await;
        }
        s.cleanup().await.unwrap();
        assert!(meta.list(crate::kind::IDEMPOTENCY).await.unwrap().len() <= 2);
    }
}
