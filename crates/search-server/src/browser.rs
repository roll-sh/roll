//! Rate limits of keys a web page ships (docs/search/README.md, "Search from the browser";
//! Algolia's `maxQueriesPerIPPerHour`). A browser key is an ordinary API key of the cluster
//! whose `metadata` carries [`METADATA`]: `{"cluster": <id>, "rate_limit": {"per_minute": n,
//! "per_ip_per_minute": m?}}`. Each request it authenticates takes a token from three buckets, or
//! from none: the cluster's browser ceiling (every browser key draws from it), the key's, and with
//! `per_ip_per_minute` the key's for the client's address. An IPv6 client counts by its /64 (one
//! host usually holds a whole /64), an IPv4 one by its address. An empty bucket answers 429
//! (`es_rejected_execution_exception`, as Elasticsearch rejects work it can't take) with
//! `Retry-After`.
//!
//! The buckets live in memory on the cluster's owner, per loaded cluster, and are bounded
//! ([`MAX_BUCKETS`]): an unload or a takeover starts them full again. When the per-address ones
//! are too many, the fullest go first (forgetting a full bucket changes nothing), so a drained
//! address keeps its debt. The key's own bucket is kept apart from the per-address ones, so
//! addresses never push it out.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::time::Instant;

/// The key of an API key's `metadata` that makes it a browser key.
pub const METADATA: &str = "roll_browser";
/// The most per-address buckets a loaded cluster keeps (and as many per-key ones).
pub const MAX_BUCKETS: usize = 50_000;
/// The highest limit a key, an address or a cluster's ceiling may have, per minute.
pub const MAX_PER_MINUTE: u64 = 100_000;
/// A cluster's browser ceiling when it sets none, per minute.
pub const DEFAULT_CLUSTER_PER_MINUTE: u64 = 6_000;

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
}

/// The address a client is counted by: an IPv6 one by its /64.
fn counted(ip: IpAddr) -> IpAddr {
    match crate::allow::canonical(ip) {
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & !((1u128 << 64) - 1))),
        v4 => v4,
    }
}

struct Bucket {
    tokens: f64,
    last: Instant,
    per_minute: u64,
}

impl Bucket {
    fn new(per_minute: u64, now: Instant) -> Bucket {
        Bucket { tokens: per_minute as f64, last: now, per_minute }
    }

    /// Takes the limit as it is now (a changed limit keeps what the bucket holds, within the new
    /// burst) and refills up to `now`; `Err`: seconds until it holds a token again.
    fn refill(&mut self, per_minute: u64, now: Instant) -> Result<(), u64> {
        self.per_minute = per_minute;
        let burst = per_minute as f64;
        let rate = burst / 60.0;
        self.tokens = (self.tokens + now.saturating_duration_since(self.last).as_secs_f64() * rate).min(burst);
        self.last = now;
        if self.tokens >= 1.0 { Ok(()) } else { Err(((1.0 - self.tokens) / rate).ceil().max(1.0) as u64) }
    }

    /// How full it would be now, 0 to 1.
    fn fill(&self, now: Instant) -> f64 {
        let n = self.per_minute.max(1) as f64;
        ((self.tokens + now.saturating_duration_since(self.last).as_secs_f64() * n / 60.0) / n).min(1.0)
    }
}

/// One loaded cluster's buckets.
#[derive(Default)]
pub struct Limiter {
    cluster: Mutex<Option<Bucket>>,
    keys: Mutex<HashMap<String, Bucket>>,
    addresses: Mutex<HashMap<(String, IpAddr), Bucket>>,
}

impl Limiter {
    /// Counts a request of key `id` from `ip` under the cluster's `ceiling` (per minute); `Err`:
    /// refused, retry after that many seconds. A refused request takes nothing from any bucket.
    pub fn check(&self, id: &str, ip: Option<IpAddr>, limit: KeyLimit, ceiling: u64) -> Result<(), u64> {
        let now = Instant::now();
        let mut cluster = self.cluster.lock();
        let mut addresses = self.addresses.lock();
        let mut keys = self.keys.lock();
        let mut waits = Vec::new();
        let c = cluster.get_or_insert_with(|| Bucket::new(ceiling, now));
        waits.push(c.refill(ceiling, now));
        let by_ip = match (limit.per_ip_per_minute, ip) {
            (Some(per_ip), Some(ip)) => {
                let k = (id.to_string(), counted(ip));
                room(&mut addresses, &k, now);
                waits.push(addresses.entry(k.clone()).or_insert_with(|| Bucket::new(per_ip, now)).refill(per_ip, now));
                Some(k)
            }
            _ => None,
        };
        room(&mut keys, &id.to_string(), now);
        waits.push(keys.entry(id.to_string()).or_insert_with(|| Bucket::new(limit.per_minute, now)).refill(limit.per_minute, now));
        if let Some(wait) = waits.iter().filter_map(|w| w.err()).max() {
            return Err(wait);
        }
        c.tokens -= 1.0;
        if let Some(b) = by_ip.and_then(|k| addresses.get_mut(&k)) {
            b.tokens -= 1.0;
        }
        if let Some(b) = keys.get_mut(id) {
            b.tokens -= 1.0;
        }
        Ok(())
    }
}

/// Makes room for `key` in `map` when it is new and the map is at [`MAX_BUCKETS`]: the fullest
/// buckets go first (a full one is as good as none), down to three quarters.
fn room<K: std::hash::Hash + Eq + Clone>(map: &mut HashMap<K, Bucket>, key: &K, now: Instant) {
    if map.contains_key(key) || map.len() < MAX_BUCKETS {
        return;
    }
    let mut by_fill: Vec<(f64, K)> = map.iter().map(|(k, b)| (b.fill(now), k.clone())).collect();
    by_fill.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (_, k) in by_fill.into_iter().take(MAX_BUCKETS / 4) {
        map.remove(&k);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_runs_out_then_says_when_to_retry() {
        let l = Limiter::default();
        let lim = KeyLimit { per_minute: 3, per_ip_per_minute: Some(2) };
        let a: IpAddr = "203.0.113.1".parse().unwrap();
        let b: IpAddr = "203.0.113.2".parse().unwrap();
        assert!(l.check("k", Some(a), lim, 100).is_ok());
        assert!(l.check("k", Some(a), lim, 100).is_ok());
        // The address is out; another address still has the key's last token.
        assert_eq!(l.check("k", Some(a), lim, 100), Err(30));
        assert!(l.check("k", Some(b), lim, 100).is_ok());
        // Now the key is out: refused, and `b`'s own bucket keeps its token.
        assert_eq!(l.check("k", Some(b), lim, 100), Err(20));
        assert_eq!(l.addresses.lock().get(&("k".to_string(), b)).map(|x| x.tokens.round() as i64), Some(1));
        // Other keys are untouched, but every key draws from the cluster's ceiling.
        assert!(l.check("other", Some(a), lim, 100).is_ok());
        let c = Limiter::default();
        assert!(c.check("k1", Some(a), lim, 2).is_ok());
        assert!(c.check("k2", Some(b), lim, 2).is_ok());
        assert_eq!(c.check("k3", Some(b), lim, 2), Err(30), "a ceiling of two a minute, both taken");
    }

    #[test]
    fn an_ipv6_client_counts_by_its_64() {
        let l = Limiter::default();
        let lim = KeyLimit { per_minute: 1000, per_ip_per_minute: Some(2) };
        assert!(l.check("k", Some("2001:db8:1:2::1".parse().unwrap()), lim, 1000).is_ok());
        assert!(l.check("k", Some("2001:db8:1:2:ffff::9".parse().unwrap()), lim, 1000).is_ok());
        assert!(l.check("k", Some("2001:db8:1:2:abcd::77".parse().unwrap()), lim, 1000).is_err(), "the same /64");
        assert!(l.check("k", Some("2001:db8:1:3::1".parse().unwrap()), lim, 1000).is_ok(), "another /64");
    }
}
