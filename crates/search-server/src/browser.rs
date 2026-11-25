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
//!   cluster's limit, so one network can't take more than a quarter of either;
//! - for a key derived from it with a `user` ([`search_es::xpack::security::derived`]), that
//!   user's: its share of the key's limit (a quarter, or less when the derived key asks for
//!   less). A derived key pays everything its parent would besides: the parent's limit is the
//!   ceiling of all its derived keys together.
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
//!
//! **Usage** ([`Usage`]; Algolia's per-key usage and logs). The owner counts each browser key's
//! requests and refusals, by [`Reason`], in memory: atomics for the minute in progress, and
//! for a derived key's `user` and a refused request's `Origin` a bounded top list
//! ([`MAX_TRACKED`] while counting, [`TOP`] kept). No request body, query or address is kept.
//! The metering pass hands the minutes on with the cluster's other observations
//! (`Node::take_observations`), and the catalog keeps them in the metrics' rings; a takeover
//! starts from what was handed on.

use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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
/// The largest request body a browser key may send when its `limits.max_body_bytes` sets
/// none (under the cluster's own `max_body_bytes`): one token never buys a parse of the
/// cluster's whole body limit.
pub const DEFAULT_MAX_BODY_BYTES: u64 = 1024 * 1024;
/// The most a key's `limits.max_body_bytes` may be.
pub const MAX_BODY_BYTES: u64 = 100 * 1024 * 1024;

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
    /// Derived keys' users, by parent key and user.
    users: Mutex<HashMap<(String, String), Bucket>>,
    /// Browser requests running: all of them, and by key.
    running: Mutex<(u64, HashMap<String, u64>)>,
}

impl Limiter {
    /// Charges `n` tokens to a request of key `id` from `ip` (for `user` at its rate per minute,
    /// a derived key's) under the cluster's `ceiling` (per minute); `Err`: refused, retry after
    /// that many seconds, and the bucket that waits longest. A refused request takes nothing.
    pub fn check(&self, id: &str, ip: Option<IpAddr>, user: Option<(&str, u64)>, limit: KeyLimit, ceiling: u64, n: u64) -> Result<(), (u64, Reason)> {
        let now = Instant::now();
        let need = n.max(1) as f64;
        let mut cluster = self.cluster.lock();
        let mut addresses = self.addresses.lock();
        let mut keys = self.keys.lock();
        let mut users = self.users.lock();
        let mut waits = Vec::new();
        let c = cluster.get_or_insert_with(|| Bucket::new(ceiling, now));
        waits.push((c.refill(ceiling, now, need), Reason::Cluster));
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
                waits.push((addresses.entry(k.clone()).or_insert_with(|| Bucket::new(per, now)).refill(per, now, need), Reason::Address));
                nets.push(k);
            }
        }
        room(&mut keys, &id.to_string(), now);
        waits.push((keys.entry(id.to_string()).or_insert_with(|| Bucket::new(limit.per_minute, now)).refill(limit.per_minute, now, need), Reason::Key));
        let user = user.map(|(u, per)| ((id.to_string(), u.to_string()), per));
        if let Some((k, per)) = &user {
            room(&mut users, k, now);
            waits.push((users.entry(k.clone()).or_insert_with(|| Bucket::new(*per, now)).refill(*per, now, need), Reason::User));
        }
        // The first of the longest waits: the bucket that holds the request back.
        let mut refused: Option<(u64, Reason)> = None;
        for (w, why) in &waits {
            if let Err(wait) = w
                && refused.is_none_or(|(r, _)| *wait > r)
            {
                refused = Some((*wait, *why));
            }
        }
        if let Some(r) = refused {
            return Err(r);
        }
        if let Some(b) = user.and_then(|(k, _)| users.get_mut(&k)) {
            b.tokens -= need;
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
    pub fn refund(&self, id: &str, ip: Option<IpAddr>, user: Option<&str>, limit: KeyLimit, n: u64) {
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
        if let Some(u) = user
            && let Some(b) = self.users.lock().get_mut(&(id.to_string(), u.to_string()))
        {
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

/// Why a browser key's request was refused: a 429 (the first five) or another refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The key's own rate.
    Key = 0,
    /// Its rate per client address (or network).
    Address = 1,
    /// A derived key's user's share of it.
    User = 2,
    /// Requests running at once: the key's `max_concurrent`, or the cluster's browser share.
    Concurrency = 3,
    /// The cluster's browser ceiling, which every browser key draws from.
    Cluster = 4,
    /// A page whose `Origin` the cluster's CORS (or a derived key's `origins`) doesn't allow.
    Origin = 5,
    /// What the key's policy refuses: an API, a query or a parameter, more items than its
    /// `max_items`.
    Policy = 6,
    /// A body over the key's `max_body_bytes`.
    BodyTooLarge = 7,
    /// An expired, revoked or wrongly signed key (or a derived key its parent refuses).
    Invalid = 8,
}

pub const REASONS: [Reason; 9] =
    [Reason::Key, Reason::Address, Reason::User, Reason::Concurrency, Reason::Cluster, Reason::Origin, Reason::Policy, Reason::BodyTooLarge, Reason::Invalid];

impl Reason {
    pub fn name(self) -> &'static str {
        match self {
            Reason::Key => "key_rate",
            Reason::Address => "address_rate",
            Reason::User => "user_rate",
            Reason::Concurrency => "concurrency",
            Reason::Cluster => "cluster_ceiling",
            Reason::Origin => "origin",
            Reason::Policy => "policy",
            Reason::BodyTooLarge => "body_too_large",
            Reason::Invalid => "invalid_key",
        }
    }

    /// A 429.
    pub fn throttles(self) -> bool {
        (self as usize) < 5
    }
}

/// What the engine's browser policy refusals say (`search_es::xpack::security::browser`: an API,
/// a query or parameter, one of the key's `max_…` limits), told apart in the answer of an
/// admitted request: the engine refuses where it has the request parsed, deep inside it.
const POLICY_MARKERS: [&[u8]; 5] =
    [b"isn't available to a browser key", b"for a browser key", b"(the key's max_", b"; the key's max_", b"'search.allow_expensive_queries' is set to false"];

/// Whether an admitted browser key's answer is a refusal of its policy ([`Reason::Policy`]).
pub fn is_policy_refusal(status: u16, body: &[u8]) -> bool {
    matches!(status, 400 | 429) && body.len() <= 64 * 1024 && POLICY_MARKERS.iter().any(|m| body.windows(m.len()).any(|w| w == *m))
}

/// Users and origins a bucket keeps, most requests first.
pub const TOP: usize = 10;
/// Users and origins counted per key and minute before the least counted makes room
/// (Space-Saving: the newcomer starts from its count, so a frequent one is never missed).
pub const MAX_TRACKED: usize = 64;
/// Browser keys counted per loaded cluster (a cluster has at most 20; revoked and expired ones
/// still count their refusals).
pub const MAX_KEYS_COUNTED: usize = 64;
/// The longest origin kept, in bytes (normalized `scheme://host[:port]`).
pub const MAX_ORIGIN_BYTES: usize = 255;
/// Finished minutes a key keeps until they are taken; past it, the oldest two merge.
const MAX_PENDING: usize = 16;

/// One browser key's usage over a bucket (a minute, an hour, a range).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyStats {
    /// Every request the key made, refused ones too.
    pub requests: u64,
    /// Refusals, by [`Reason`].
    pub refused: [u64; REASONS.len()],
    /// A derived key's users: requests and 429s, most requests first, at most [`TOP`].
    pub users: Vec<(String, u64, u64)>,
    /// Origins of refused requests, most first, at most [`TOP`].
    pub origins: Vec<(String, u64)>,
}

impl KeyStats {
    pub fn is_empty(&self) -> bool {
        self.requests == 0 && self.refused.iter().all(|n| *n == 0)
    }

    pub fn throttled(&self) -> u64 {
        REASONS.iter().filter(|r| r.throttles()).map(|r| self.refused[*r as usize]).sum()
    }

    /// Adds `o` to this, keeping the [`TOP`] users and origins.
    pub fn merge(&mut self, o: &KeyStats) {
        self.requests += o.requests;
        for (a, b) in self.refused.iter_mut().zip(o.refused) {
            *a += b;
        }
        let mut users: HashMap<String, (u64, u64)> = self.users.drain(..).map(|(u, r, t)| (u, (r, t))).collect();
        for (u, r, t) in &o.users {
            let e = users.entry(u.clone()).or_default();
            e.0 += r;
            e.1 += t;
        }
        self.users = top(users.into_iter().map(|(u, (r, t))| (u, r, t)), |x| (x.1, x.2), |x| &x.0);
        let mut origins: HashMap<String, u64> = self.origins.drain(..).collect();
        for (o, n) in &o.origins {
            *origins.entry(o.clone()).or_default() += n;
        }
        self.origins = top(origins.into_iter(), |x| (x.1, 0), |x| &x.0);
    }

    /// As the catalog keeps it, and as the usage route answers it.
    pub fn to_json(&self) -> serde_json::Value {
        let mut refused = serde_json::Map::new();
        for r in REASONS {
            refused.insert(r.name().into(), self.refused[r as usize].into());
        }
        serde_json::json!({
            "requests": self.requests,
            "throttled": self.throttled(),
            "refused": refused,
            "users": self.users.iter().map(|(u, r, t)| serde_json::json!({ "user": u, "requests": r, "throttled": t })).collect::<Vec<_>>(),
            "origins": self.origins.iter().map(|(o, n)| serde_json::json!({ "origin": o, "refused": n })).collect::<Vec<_>>(),
        })
    }

    pub fn encode(&self) -> String {
        self.to_json().to_string()
    }

    /// What [`KeyStats::encode`] wrote (anything else reads as nothing), bounded as kept.
    pub fn decode(s: &str) -> KeyStats {
        let v: serde_json::Value = serde_json::from_str(s).unwrap_or_default();
        let n = |v: &serde_json::Value| v.as_u64().unwrap_or(0);
        let mut k = KeyStats { requests: n(&v["requests"]), ..Default::default() };
        for r in REASONS {
            k.refused[r as usize] = n(&v["refused"][r.name()]);
        }
        let list = |f: &str| v[f].as_array().cloned().unwrap_or_default().into_iter().take(TOP);
        k.users = list("users").filter_map(|u| Some((u["user"].as_str()?.to_string(), n(&u["requests"]), n(&u["throttled"])))).collect();
        k.origins = list("origins").filter_map(|o| Some((o["origin"].as_str()?.to_string(), n(&o["refused"])))).collect();
        k
    }
}

/// The [`TOP`] of `items` by `count`, ties by name.
fn top<T>(items: impl Iterator<Item = T>, count: impl Fn(&T) -> (u64, u64), name: impl Fn(&T) -> &String) -> Vec<T> {
    let mut v: Vec<T> = items.collect();
    v.sort_by(|a, b| count(b).cmp(&count(a)).then_with(|| name(a).cmp(name(b))));
    v.truncate(TOP);
    v
}

/// A refused request's `Origin` as kept: normalized, at most [`MAX_ORIGIN_BYTES`] (`None`:
/// not an origin).
pub fn kept_origin(o: &str) -> Option<String> {
    let mut o = crate::cors::normalize_origin(o).ok()?;
    if o.len() > MAX_ORIGIN_BYTES {
        let mut end = MAX_ORIGIN_BYTES;
        while !o.is_char_boundary(end) {
            end -= 1;
        }
        o.truncate(end);
    }
    Some(o)
}

/// Adds `by` to `name`'s count in `map`, making room Space-Saving's way when it is full.
fn bump<V: Copy + Default + Ord>(map: &mut HashMap<String, V>, name: &str, by: impl Fn(&mut V)) {
    if !map.contains_key(name) && map.len() >= MAX_TRACKED {
        let Some((least, v)) = map.iter().min_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0))).map(|(k, v)| (k.clone(), *v)) else { return };
        map.remove(&least);
        map.insert(name.to_string(), v);
    }
    by(map.entry(name.to_string()).or_default());
}

#[derive(Default)]
struct Detail {
    /// The minute `counts` and the lists below are of.
    minute: u64,
    users: HashMap<String, (u64, u64)>,
    origins: HashMap<String, u64>,
    /// Finished minutes, oldest first.
    done: Vec<(u64, KeyStats)>,
}

struct KeyUsage {
    /// `Detail::minute`, read without the lock.
    minute: AtomicU64,
    requests: AtomicU64,
    refused: [AtomicU64; REASONS.len()],
    detail: Mutex<Detail>,
}

impl KeyUsage {
    /// The minute in progress, as counted so far, its counters emptied (`take`) or not.
    fn current(&self, d: &mut Detail, take: bool) -> KeyStats {
        let read = |a: &AtomicU64| if take { a.swap(0, Ordering::Relaxed) } else { a.load(Ordering::Relaxed) };
        let mut k = KeyStats { requests: read(&self.requests), ..Default::default() };
        for (i, a) in self.refused.iter().enumerate() {
            k.refused[i] = read(a);
        }
        let (users, origins) = if take { (std::mem::take(&mut d.users), std::mem::take(&mut d.origins)) } else { (d.users.clone(), d.origins.clone()) };
        k.users = top(users.into_iter().map(|(u, (r, t))| (u, r, t)), |x| (x.1, x.2), |x| &x.0);
        k.origins = top(origins.into_iter(), |x| (x.1, 0), |x| &x.0);
        k
    }

    /// Moves the minute in progress to the finished ones when `minute` is another.
    fn roll(&self, d: &mut Detail, minute: u64) {
        if d.minute == minute {
            return;
        }
        let k = self.current(d, true);
        if !k.is_empty() {
            d.done.push((d.minute, k));
            if d.done.len() > MAX_PENDING {
                let (_, oldest) = d.done.remove(0);
                d.done[0].1.merge(&oldest);
            }
        }
        d.minute = minute;
        self.minute.store(minute, Ordering::Release);
    }
}

/// One loaded cluster's browser key usage not handed on yet (the module docs, "Usage").
#[derive(Default)]
pub struct Usage {
    keys: RwLock<HashMap<String, Arc<KeyUsage>>>,
}

impl Usage {
    fn key(&self, id: &str, minute: u64) -> Option<Arc<KeyUsage>> {
        if let Some(k) = self.keys.read().get(id) {
            return Some(k.clone());
        }
        let mut keys = self.keys.write();
        if !keys.contains_key(id) && keys.len() >= MAX_KEYS_COUNTED {
            return None;
        }
        let k = keys.entry(id.to_string()).or_insert_with(|| {
            Arc::new(KeyUsage {
                minute: AtomicU64::new(minute),
                requests: AtomicU64::new(0),
                refused: Default::default(),
                detail: Mutex::new(Detail { minute, ..Default::default() }),
            })
        });
        Some(k.clone())
    }

    /// Counts a request of browser key `id` at unix minute `minute` (`request`: a new one;
    /// otherwise a refusal of one counted already), refused for `why`, for a derived key's
    /// `user`, from the page `origin` (kept only for a refusal).
    pub fn count(&self, minute: u64, id: &str, request: bool, why: Option<Reason>, user: Option<&str>, origin: Option<&str>) {
        let Some(k) = self.key(id, minute) else { return };
        let origin = why.and(origin).and_then(kept_origin);
        if user.is_some() || origin.is_some() || k.minute.load(Ordering::Acquire) != minute {
            let mut d = k.detail.lock();
            k.roll(&mut d, minute);
            if let Some(u) = user {
                let throttled = why.is_some_and(Reason::throttles);
                bump(&mut d.users, u, |(r, t)| {
                    *r += u64::from(request);
                    *t += u64::from(throttled);
                });
            }
            if let Some(o) = &origin {
                bump(&mut d.origins, o, |n| *n += 1);
            }
        }
        if request {
            k.requests.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(r) = why {
            k.refused[r as usize].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Takes every key's minutes, oldest first (the one in progress too: the catalog adds
    /// them up, so the rest of it comes with the next take).
    pub fn take(&self) -> Vec<(String, u64, KeyStats)> {
        self.read(true)
    }

    /// Copies of what [`Usage::take`] would take.
    pub fn peek(&self) -> Vec<(String, u64, KeyStats)> {
        self.read(false)
    }

    fn read(&self, take: bool) -> Vec<(String, u64, KeyStats)> {
        let keys: Vec<(String, Arc<KeyUsage>)> = self.keys.read().iter().map(|(id, k)| (id.clone(), k.clone())).collect();
        let mut out = Vec::new();
        for (id, k) in keys {
            let mut d = k.detail.lock();
            let done = if take { std::mem::take(&mut d.done) } else { d.done.clone() };
            out.extend(done.into_iter().map(|(m, s)| (id.clone(), m, s)));
            let minute = d.minute;
            let cur = k.current(&mut d, take);
            if !cur.is_empty() {
                out.push((id.clone(), minute, cur));
            }
        }
        out
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
        assert!(l.check("k", Some(a), None, lim, 100, 1).is_ok());
        assert!(l.check("k", Some(a), None, lim, 100, 1).is_ok());
        // The address is out; another network still has the key's last token.
        assert_eq!(l.check("k", Some(a), None, lim, 100, 1), Err((30, Reason::Address)));
        assert!(l.check("k", Some(b), None, lim, 100, 1).is_ok());
        // Now the key is out: refused, and nothing is taken.
        assert_eq!(l.check("k", Some(b), None, lim, 100, 1), Err((20, Reason::Key)));
        // Every key draws from the cluster's ceiling.
        let c = Limiter::default();
        assert!(c.check("k1", Some(a), None, lim, 2, 1).is_ok());
        assert!(c.check("k2", Some(b), None, lim, 2, 1).is_ok());
        assert_eq!(c.check("k3", Some(b), None, lim, 2, 1), Err((30, Reason::Cluster)), "a ceiling of two a minute, both taken");
    }

    #[test]
    fn a_request_costs_a_token_per_item() {
        let l = Limiter::default();
        assert!(l.check("k", None, None, lim(10, 10), 1000, 7).is_ok());
        assert!(l.check("k", None, None, lim(10, 10), 1000, 4).is_err(), "3 tokens left can't cover 4 items");
        assert!(l.check("k", None, None, lim(10, 10), 1000, 3).is_ok());
        assert!(l.check("k", None, None, lim(10, 10), 1000, 11).is_err(), "more items than the key's burst");
    }

    #[test]
    fn a_network_counts_together() {
        // One /64 is one address.
        let l = Limiter::default();
        let k = lim(10_000, 2);
        assert!(l.check("k", Some("2001:db8:1:2::1".parse().unwrap()), None, k, 10_000, 1).is_ok());
        assert!(l.check("k", Some("2001:db8:1:2:ffff::9".parse().unwrap()), None, k, 10_000, 1).is_ok());
        assert!(l.check("k", Some("2001:db8:1:2:abcd::77".parse().unwrap()), None, k, 10_000, 1).is_err(), "the same /64");
        // Many /64s of one /56 share its 4x: 8 requests, then throttled.
        let l = Limiter::default();
        let ok = (0..64u16).filter(|i| l.check("k", Some(format!("2001:db8:1:{i:x}::1").parse().unwrap()), None, k, 10_000, 1).is_ok()).count();
        assert_eq!(ok, 8, "a /56 gets 4 times one address's limit");
        // A /48's share of the key is at most a quarter.
        let l = Limiter::default();
        let k = lim(40, 10);
        let ok = (0..16u16).filter(|i| l.check("k", Some(format!("2001:db8:7:{:x}00::1", i).parse().unwrap()), None, k, 10_000, 1).is_ok()).count();
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

    #[test]
    fn usage_counts_by_key_reason_and_user_and_keeps_the_top() {
        let u = Usage::default();
        for i in 0..(MAX_TRACKED as u64 + 5) {
            u.count(7, "k", true, None, Some(&format!("u{i}")), None);
        }
        for _ in 0..3 {
            u.count(7, "k", true, Some(Reason::User), Some("heavy"), Some("https://Shop.example.com:443/"));
        }
        u.count(8, "k", true, Some(Reason::Invalid), None, Some("not an origin"));
        let got = u.take();
        assert_eq!(got.iter().map(|(k, m, s)| (k.as_str(), *m, s.requests)).collect::<Vec<_>>(), [("k", 7, MAX_TRACKED as u64 + 8), ("k", 8, 1)]);
        let m7 = &got[0].2;
        assert_eq!((m7.refused[Reason::User as usize], m7.throttled()), (3, 3));
        assert_eq!(m7.users.len(), TOP);
        assert_eq!(m7.users[0].0, "heavy", "a frequent user is never pushed out: {:?}", m7.users);
        assert_eq!(m7.origins, [("https://shop.example.com".to_string(), 3)]);
        assert!(got[1].2.origins.is_empty());
        assert_eq!(KeyStats::decode(&m7.encode()), *m7);
        assert!(u.take().is_empty(), "taken once");
    }
}
