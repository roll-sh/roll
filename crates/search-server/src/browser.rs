//! Rate limits of keys a web page ships (docs/search/README.md, "Search from the browser";
//! Algolia's `maxQueriesPerIPPerHour`). A browser key is an ordinary API key of the cluster
//! whose `metadata` carries [`METADATA`]: `{"rate_limit": {"per_minute": n, "per_ip_per_minute": m?}}`.
//! Each request it authenticates takes a token from the key's bucket and, with `per_ip_per_minute`,
//! from the bucket of the key and the client's address. An empty bucket answers 429
//! (`es_rejected_execution_exception`, as Elasticsearch rejects work it can't take) with
//! `Retry-After`.
//!
//! The buckets live in memory on the cluster's owner, per loaded cluster, and are bounded
//! ([`MAX_BUCKETS`]): an unload or a takeover starts them full again, and a flood of addresses
//! evicts idle ones first. The key's own bucket is kept apart from the per-address ones, so
//! addresses never push it out.

use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Instant;

/// The key of an API key's `metadata` that makes it a browser key.
pub const METADATA: &str = "roll_browser";
/// The most per-address buckets a loaded cluster keeps (and as many per-key ones).
pub const MAX_BUCKETS: usize = 50_000;
/// The highest limit a key may carry, per minute.
pub const MAX_PER_MINUTE: u64 = 1_000_000;

/// A browser key's limits, from its metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyLimit {
    pub per_minute: u64,
    pub per_ip_per_minute: Option<u64>,
}

impl KeyLimit {
    /// The limits a key's `metadata.roll_browser.rate_limit` gives (`None`: no limit).
    pub fn new(per_minute: Option<i64>, per_ip_per_minute: Option<i64>) -> Option<KeyLimit> {
        let per = |n: Option<i64>| n.and_then(|n| u64::try_from(n).ok()).filter(|n| (1..=MAX_PER_MINUTE).contains(n));
        Some(KeyLimit { per_minute: per(per_minute)?, per_ip_per_minute: per(per_ip_per_minute) })
    }

    /// The limits in an API key's `metadata`, as JSON (`None`: not a browser key, or no limit).
    pub fn of(metadata: &Value) -> Option<KeyLimit> {
        let r = metadata.get(METADATA)?.get("rate_limit")?;
        KeyLimit::new(r.get("per_minute").and_then(Value::as_i64), r.get("per_ip_per_minute").and_then(Value::as_i64))
    }
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Bucket {
    /// Refills a bucket of `per_minute` (its burst) up to `now`; `Err`: seconds until it holds
    /// a token again.
    fn refill(&mut self, per_minute: u64, now: Instant) -> Result<(), u64> {
        let burst = per_minute as f64;
        let rate = burst / 60.0;
        self.tokens = (self.tokens + now.saturating_duration_since(self.last).as_secs_f64() * rate).min(burst);
        self.last = now;
        if self.tokens >= 1.0 { Ok(()) } else { Err(((1.0 - self.tokens) / rate).ceil().max(1.0) as u64) }
    }

    /// Whether it has refilled to `per_minute` (forgetting it changes nothing).
    fn full(&self, per_minute: u64, now: Instant) -> bool {
        self.tokens + now.saturating_duration_since(self.last).as_secs_f64() * per_minute as f64 / 60.0 >= per_minute as f64
    }
}

/// One loaded cluster's buckets.
#[derive(Default)]
pub struct Limiter {
    keys: Mutex<HashMap<String, (Bucket, u64)>>,
    addresses: Mutex<HashMap<(String, IpAddr), (Bucket, u64)>>,
}

impl Limiter {
    /// Counts a request of key `id` from `ip`; `Err`: refused, retry after that many seconds.
    /// A refused request takes nothing from either bucket.
    pub fn check(&self, id: &str, ip: Option<IpAddr>, limit: KeyLimit) -> Result<(), u64> {
        let now = Instant::now();
        let mut addresses = self.addresses.lock();
        let mut keys = self.keys.lock();
        let by_ip = match (limit.per_ip_per_minute, ip) {
            (Some(per_ip), Some(ip)) => Some((bucket(&mut addresses, (id.to_string(), ip), per_ip, now), per_ip)),
            _ => None,
        };
        let by_ip_ready = match &by_ip {
            Some((k, per_ip)) => addresses.get_mut(k).map_or(Ok(()), |(b, _)| b.refill(*per_ip, now)),
            None => Ok(()),
        };
        let key = bucket(&mut keys, id.to_string(), limit.per_minute, now);
        let key_ready = keys.get_mut(&key).map_or(Ok(()), |(b, _)| b.refill(limit.per_minute, now));
        match (by_ip_ready, key_ready) {
            (Ok(()), Ok(())) => {
                if let Some((k, _)) = &by_ip
                    && let Some((b, _)) = addresses.get_mut(k)
                {
                    b.tokens -= 1.0;
                }
                if let Some((b, _)) = keys.get_mut(&key) {
                    b.tokens -= 1.0;
                }
                Ok(())
            }
            (Err(a), Err(b)) => Err(a.max(b)),
            (Err(s), _) | (_, Err(s)) => Err(s),
        }
    }
}

/// Makes sure `map` has a bucket for `key` of `per_minute` (full when new) and returns the key.
fn bucket<K: std::hash::Hash + Eq + Clone>(map: &mut HashMap<K, (Bucket, u64)>, key: K, per_minute: u64, now: Instant) -> K {
    if !map.contains_key(&key) && map.len() >= MAX_BUCKETS {
        // Full buckets are as good as none; if too few are, forget an arbitrary half (those
        // addresses start full again: the key's own bucket still holds).
        map.retain(|_, (b, n)| !b.full(*n, now));
        if map.len() > MAX_BUCKETS * 7 / 8 {
            let mut drop = map.len() / 2;
            map.retain(|_, _| {
                if drop > 0 {
                    drop -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }
    let (b, n) = map.entry(key.clone()).or_insert_with(|| (Bucket { tokens: per_minute as f64, last: now }, per_minute));
    if *n != per_minute {
        // The key's limit changed: its bucket keeps what it has, within the new burst.
        *n = per_minute;
        b.tokens = b.tokens.min(per_minute as f64);
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn limits_come_from_the_keys_metadata() {
        assert_eq!(KeyLimit::of(&json!({"roll_browser": {"rate_limit": {"per_minute": 60}}})), Some(KeyLimit { per_minute: 60, per_ip_per_minute: None }));
        assert_eq!(KeyLimit::of(&json!({"other": 1})), None);
        assert_eq!(KeyLimit::of(&json!({"roll_browser": {"rate_limit": {"per_minute": 0}}})), None);
    }

    #[test]
    fn a_key_runs_out_then_says_when_to_retry() {
        let l = Limiter::default();
        let lim = KeyLimit { per_minute: 3, per_ip_per_minute: Some(2) };
        let a: IpAddr = "203.0.113.1".parse().unwrap();
        let b: IpAddr = "203.0.113.2".parse().unwrap();
        assert!(l.check("k", Some(a), lim).is_ok());
        assert!(l.check("k", Some(a), lim).is_ok());
        // The address is out; another address still has the key's last token.
        assert_eq!(l.check("k", Some(a), lim), Err(30));
        assert!(l.check("k", Some(b), lim).is_ok());
        // Now the key is out: refused, and `b`'s own bucket keeps its token.
        assert_eq!(l.check("k", Some(b), lim), Err(20));
        assert_eq!(l.addresses.lock().get(&("k".to_string(), b)).map(|(x, _)| x.tokens.round() as i64), Some(1));
        // Other keys are untouched.
        assert!(l.check("other", Some(a), lim).is_ok());
    }
}
