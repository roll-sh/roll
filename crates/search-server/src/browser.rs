//! Rate and concurrency limits of keys a web page ships (docs/search/README.md, "Search from the
//! browser"; Algolia's `maxQueriesPerIPPerHour`). A browser key is an ordinary API key of the
//! cluster whose `metadata` carries [`METADATA`]: `{"cluster": <id>, "rate_limit": {"per_minute":
//! n, "per_ip_per_minute": m}, "limits": {"max_concurrent": c, …}}`.
//!
//! **Rates.** Each request a browser key authenticates costs one token per item it holds (an
//! `_msearch` of 50 searches costs 50), taken from every bucket or from none:
//! - the cluster's browser ceiling, which every browser key draws from;
//! - the key's own;
//! - with `per_ip_per_minute`, the key's for the client's address at three widths: the address
//!   (an IPv6 /64, which one host usually holds), its /56 and its /48 (IPv4: /32, /24, /16), at 1,
//!   4 and 16 times the per-address limit, the wider two never past a quarter of the key's or the
//!   cluster's limit, so one network can't take more than a quarter of either.
//!
//! An empty bucket answers 429 (`es_rejected_execution_exception`, as Elasticsearch rejects work
//! it can't take) with `Retry-After`. The buckets live in memory on the cluster's owner, per
//! loaded cluster, and are bounded ([`MAX_BUCKETS`]): an unload or a takeover starts them full
//! again. When the per-address ones are too many, the fullest go first (forgetting a full bucket
//! changes nothing), so a drained address keeps its debt.
//!
//! **Concurrency.** Browser requests running at once are capped per key (`max_concurrent`,
//! default 4) and per cluster (a quarter of the cluster's `max_in_flight`, at least 4 but never
//! more than half), so the owner's own clients always have permits left.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Instant;

/// The key of an API key's `metadata` that makes it a browser key.
pub const METADATA: &str = "roll_browser";
/// The most per-address buckets a loaded cluster keeps (and as many per-key ones).
pub const MAX_BUCKETS: usize = 50_000;
/// The highest limit a key, an address or a cluster's ceiling may have, per minute.
pub const MAX_PER_MINUTE: u64 = 100_000;
/// A cluster's browser ceiling when it sets none, per minute.
pub const DEFAULT_CLUSTER_PER_MINUTE: u64 = 6_000;
/// A key's concurrent requests when it sets none.
pub const DEFAULT_MAX_CONCURRENT: u64 = 4;

/// A browser key's limits, from its metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyLimit {
    pub per_minute: u64,
    pub per_ip_per_minute: Option<u64>,
    pub max_concurrent: u64,
}

impl KeyLimit {
    /// The limits a key's `metadata.roll_browser` gives (`rate_limit` missing: no rate of its own).
    pub fn new(per_minute: Option<i64>, per_ip_per_minute: Option<i64>, max_concurrent: Option<i64>) -> KeyLimit {
        let per = |n: Option<i64>| n.and_then(|n| u64::try_from(n).ok()).filter(|n| (1..=MAX_PER_MINUTE).contains(n));
        KeyLimit {
            per_minute: per(per_minute).unwrap_or(MAX_PER_MINUTE),
            per_ip_per_minute: per(per_ip_per_minute),
            max_concurrent: per(max_concurrent).unwrap_or(DEFAULT_MAX_CONCURRENT),
        }
    }
}

/// The networks a client is counted in: the address (an IPv6 /64), its /56 and its /48 (IPv4:
/// the address, its /24 and its /16).
fn networks(ip: IpAddr) -> [IpAddr; 3] {
    match crate::allow::canonical(ip) {
        IpAddr::V6(v6) => {
            let n = u128::from(v6);
            let mask = |bits: u32| IpAddr::V6((n & !(u128::MAX >> bits)).into());
            [mask(64), mask(56), mask(48)]
        }
        IpAddr::V4(v4) => {
            let n = u32::from(v4);
            let mask = |bits: u32| IpAddr::V4((n & !(u32::MAX >> bits)).into());
            [IpAddr::V4(v4), mask(24), mask(16)]
        }
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
    /// burst) and refills up to `now`; `Err`: seconds until it holds `need` tokens (it never will
    /// past its burst: then a minute).
    fn refill(&mut self, per_minute: u64, now: Instant, need: f64) -> Result<(), u64> {
        self.per_minute = per_minute;
        let burst = per_minute as f64;
        let rate = burst / 60.0;
        self.tokens = (self.tokens + now.saturating_duration_since(self.last).as_secs_f64() * rate).min(burst);
        self.last = now;
        if self.tokens >= need {
            Ok(())
        } else if need > burst {
            Err(60)
        } else {
            Err(((need - self.tokens) / rate).ceil().max(1.0) as u64)
        }
    }

    /// How full it would be now, 0 to 1.
    fn fill(&self, now: Instant) -> f64 {
        let n = self.per_minute.max(1) as f64;
        ((self.tokens + now.saturating_duration_since(self.last).as_secs_f64() * n / 60.0) / n).min(1.0)
    }
}

/// One loaded cluster's buckets and running browser requests.
#[derive(Default)]
pub struct Limiter {
    cluster: Mutex<Option<Bucket>>,
    keys: Mutex<HashMap<String, Bucket>>,
    /// By key, width (0: the address, 1: /56, 2: /48) and network.
    addresses: Mutex<HashMap<(String, u8, IpAddr), Bucket>>,
    /// Browser requests running: all of them, and by key.
    running: Mutex<(u64, HashMap<String, u64>)>,
}

impl Limiter {
    /// Charges `n` tokens to a request of key `id` from `ip` under the cluster's `ceiling` (per
    /// minute); `Err`: refused, retry after that many seconds. A refused request takes nothing.
    pub fn check(&self, id: &str, ip: Option<IpAddr>, limit: KeyLimit, ceiling: u64, n: u64) -> Result<(), u64> {
        let now = Instant::now();
        let need = n.max(1) as f64;
        let mut cluster = self.cluster.lock();
        let mut addresses = self.addresses.lock();
        let mut keys = self.keys.lock();
        let mut waits = Vec::new();
        let c = cluster.get_or_insert_with(|| Bucket::new(ceiling, now));
        waits.push(c.refill(ceiling, now, need));
        let mut nets = Vec::new();
        if let (Some(per_ip), Some(ip)) = (limit.per_ip_per_minute, ip) {
            // A network's share: never past a quarter of the key's or the cluster's limit (nor
            // below one address's).
            let share = limit.per_minute.min(ceiling) / 4;
            for (width, net) in networks(ip).into_iter().enumerate() {
                let per = match width {
                    0 => per_ip,
                    1 => (per_ip * 4).min(share).max(per_ip),
                    _ => (per_ip * 16).min(share).max(per_ip),
                };
                let k = (id.to_string(), width as u8, net);
                room(&mut addresses, &k, now);
                waits.push(addresses.entry(k.clone()).or_insert_with(|| Bucket::new(per, now)).refill(per, now, need));
                nets.push(k);
            }
        }
        room(&mut keys, &id.to_string(), now);
        waits.push(keys.entry(id.to_string()).or_insert_with(|| Bucket::new(limit.per_minute, now)).refill(limit.per_minute, now, need));
        if let Some(wait) = waits.iter().filter_map(|w| w.err()).max() {
            return Err(wait);
        }
        c.tokens -= need;
        for k in &nets {
            if let Some(b) = addresses.get_mut(k) {
                b.tokens -= need;
            }
        }
        if let Some(b) = keys.get_mut(id) {
            b.tokens -= need;
        }
        Ok(())
    }

    /// Gives back `n` tokens [`Limiter::check`] took for key `id` from `ip` (a request whose
    /// further items couldn't be paid: a refused request costs nothing).
    pub fn refund(&self, id: &str, ip: Option<IpAddr>, limit: KeyLimit, n: u64) {
        let n = n as f64;
        let give = |b: &mut Bucket| b.tokens = (b.tokens + n).min(b.per_minute as f64);
        if let Some(c) = self.cluster.lock().as_mut() {
            give(c);
        }
        if let (Some(_), Some(ip)) = (limit.per_ip_per_minute, ip) {
            let mut addresses = self.addresses.lock();
            for (width, net) in networks(ip).into_iter().enumerate() {
                if let Some(b) = addresses.get_mut(&(id.to_string(), width as u8, net)) {
                    give(b);
                }
            }
        }
        if let Some(b) = self.keys.lock().get_mut(id) {
            give(b);
        }
    }

    /// Starts a request of key `id` if fewer than `per_key` of its requests and `per_cluster`
    /// browser requests in all are running; [`Limiter::leave`] ends it.
    pub fn enter(&self, id: &str, per_key: u64, per_cluster: u64) -> bool {
        let mut r = self.running.lock();
        let mine = r.1.get(id).copied().unwrap_or(0);
        if r.0 >= per_cluster || mine >= per_key {
            return false;
        }
        r.0 += 1;
        *r.1.entry(id.to_string()).or_insert(0) += 1;
        true
    }

    pub fn leave(&self, id: &str) {
        let mut r = self.running.lock();
        r.0 = r.0.saturating_sub(1);
        if let Some(n) = r.1.get_mut(id) {
            *n -= 1;
            if *n == 0 {
                r.1.remove(id);
            }
        }
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

    fn lim(per_minute: u64, per_ip: u64) -> KeyLimit {
        KeyLimit { per_minute, per_ip_per_minute: Some(per_ip), max_concurrent: 4 }
    }

    #[test]
    fn a_key_runs_out_then_says_when_to_retry() {
        let l = Limiter::default();
        let lim = lim(3, 2);
        let a: IpAddr = "203.0.113.1".parse().unwrap();
        let b: IpAddr = "198.51.100.2".parse().unwrap();
        assert!(l.check("k", Some(a), lim, 100, 1).is_ok());
        assert!(l.check("k", Some(a), lim, 100, 1).is_ok());
        // The address is out; another network still has the key's last token.
        assert_eq!(l.check("k", Some(a), lim, 100, 1), Err(30));
        assert!(l.check("k", Some(b), lim, 100, 1).is_ok());
        // Now the key is out: refused, and nothing is taken.
        assert_eq!(l.check("k", Some(b), lim, 100, 1), Err(20));
        // Every key draws from the cluster's ceiling.
        let c = Limiter::default();
        assert!(c.check("k1", Some(a), lim, 2, 1).is_ok());
        assert!(c.check("k2", Some(b), lim, 2, 1).is_ok());
        assert_eq!(c.check("k3", Some(b), lim, 2, 1), Err(30), "a ceiling of two a minute, both taken");
    }

    #[test]
    fn a_request_costs_a_token_per_item() {
        let l = Limiter::default();
        assert!(l.check("k", None, lim(10, 10), 1000, 7).is_ok());
        assert!(l.check("k", None, lim(10, 10), 1000, 4).is_err(), "3 tokens left can't cover 4 items");
        assert!(l.check("k", None, lim(10, 10), 1000, 3).is_ok());
        assert!(l.check("k", None, lim(10, 10), 1000, 11).is_err(), "more items than the key's burst");
    }

    #[test]
    fn a_network_counts_together() {
        // One /64 is one address.
        let l = Limiter::default();
        let k = lim(10_000, 2);
        assert!(l.check("k", Some("2001:db8:1:2::1".parse().unwrap()), k, 10_000, 1).is_ok());
        assert!(l.check("k", Some("2001:db8:1:2:ffff::9".parse().unwrap()), k, 10_000, 1).is_ok());
        assert!(l.check("k", Some("2001:db8:1:2:abcd::77".parse().unwrap()), k, 10_000, 1).is_err(), "the same /64");
        // Many /64s of one /56 share its 4x: 8 requests, then throttled.
        let l = Limiter::default();
        let ok = (0..64u16).filter(|i| l.check("k", Some(format!("2001:db8:1:{i:x}::1").parse().unwrap()), k, 10_000, 1).is_ok()).count();
        assert_eq!(ok, 8, "a /56 gets 4 times one address's limit");
        // A /48's share of the key is at most a quarter.
        let l = Limiter::default();
        let k = lim(40, 10);
        let ok = (0..16u16).filter(|i| l.check("k", Some(format!("2001:db8:7:{:x}00::1", i).parse().unwrap()), k, 10_000, 1).is_ok()).count();
        assert_eq!(ok, 10, "a quarter of the key's 40");
    }

    #[test]
    fn running_requests_are_capped_per_key_and_cluster() {
        let l = Limiter::default();
        assert!(l.enter("a", 2, 3) && l.enter("a", 2, 3));
        assert!(!l.enter("a", 2, 3), "the key's two");
        assert!(l.enter("b", 2, 3));
        assert!(!l.enter("c", 2, 3), "the cluster's three");
        l.leave("a");
        assert!(l.enter("c", 2, 3));
    }
}
