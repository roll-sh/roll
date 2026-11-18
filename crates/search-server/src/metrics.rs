//! Request metrics per cluster: requests, responses by class, 429 rejections and latency, per
//! kind of request ([`Kind`]), in one-minute buckets.
//!
//! A loaded cluster holds one [`Recorder`]: the minute in progress (allocated on its first
//! request of the minute, about 3 KiB) and the finished minutes not handed on yet. Recording a
//! request takes the recorder's lock, bumps four counters and one histogram bucket: no
//! allocation, no timer. The node hands the minutes to whoever meters
//! ([`crate::Node::take_observations`]); an unloaded cluster's go with it into the node's
//! carry-over, so a sleeping cluster holds nothing here.
//!
//! Latency goes into a log-bucketed histogram ([`Histogram`], HDR-like: 8 buckets per power of
//! two of microseconds, so a percentile is within about 6% of the truth) that merges exactly, so
//! minutes add up to hours and any range's percentiles come from its buckets.

use parking_lot::Mutex;
use serde_json::{Value, json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// What a request did, from its method and path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Reads that query: `_search`, `_msearch`, `_count`, async search, `_knn_search`, EQL,
    /// ES|QL, SQL, `_terms_enum`.
    Search = 0,
    /// Writes of documents: `_bulk`, `_doc`, `_create`, `_update`, by-query updates and deletes.
    Index = 1,
    /// Everything else (gets, mappings, cluster APIs, `_cat`).
    Other = 2,
}

pub const KINDS: [Kind; 3] = [Kind::Search, Kind::Index, Kind::Other];

impl Kind {
    pub fn of(method: &str, uri: &str) -> Kind {
        let path = uri.split('?').next().unwrap_or("");
        let write = !matches!(method, "GET" | "HEAD");
        let mut kind = Kind::Other;
        for seg in path.split('/') {
            match seg {
                // `POST /i/_doc/_search` indexes a document with the id `_search`.
                "_bulk" | "_doc" | "_create" | "_update" | "_update_by_query" | "_delete_by_query" if write => return Kind::Index,
                "_search" | "_msearch" | "_count" | "_async_search" | "_knn_search" | "_eql" | "_query" | "_sql" | "_terms_enum" => kind = Kind::Search,
                _ => {}
            }
        }
        kind
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Search => "search",
            Kind::Index => "index",
            Kind::Other => "other",
        }
    }
}

/// Histogram buckets: 8 per power of two of microseconds, up to about 4.5 hours.
pub const BUCKETS: usize = 256;
const SUB: u64 = 8;

/// A latency histogram in microseconds with log buckets (see the module docs).
#[derive(Clone, PartialEq, Eq)]
pub struct Histogram {
    counts: [u32; BUCKETS],
}

impl Default for Histogram {
    fn default() -> Self {
        Histogram { counts: [0; BUCKETS] }
    }
}

impl std::fmt::Debug for Histogram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.counts.iter().enumerate().filter(|(_, n)| **n > 0)).finish()
    }
}

/// The bucket of `micros`: exact below 8, then 8 per power of two.
pub fn bucket(micros: u64) -> usize {
    if micros < SUB {
        return micros as usize;
    }
    let e = 63 - micros.leading_zeros() as u64;
    let i = (e - 2) * SUB + ((micros >> (e - 3)) & (SUB - 1));
    (i as usize).min(BUCKETS - 1)
}

/// The smallest value of bucket `i`.
pub fn bucket_low(i: usize) -> u64 {
    let i = i as u64;
    if i < SUB {
        return i;
    }
    let (e, m) = (i / SUB + 2, i % SUB);
    (SUB + m) << (e - 3)
}

impl Histogram {
    pub fn record(&mut self, micros: u64) {
        let b = &mut self.counts[bucket(micros)];
        *b = b.saturating_add(1);
    }

    pub fn merge(&mut self, other: &Histogram) {
        for (a, b) in self.counts.iter_mut().zip(other.counts.iter()) {
            *a = a.saturating_add(*b);
        }
    }

    pub fn count(&self) -> u64 {
        self.counts.iter().map(|&n| n as u64).sum()
    }

    /// The value at quantile `q` (0..=1), in microseconds: the middle of the bucket holding the
    /// `ceil(q × count)`th value. `None` when empty.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        let total = self.count();
        if total == 0 {
            return None;
        }
        let rank = ((q.clamp(0.0, 1.0) * total as f64).ceil() as u64).max(1);
        let mut seen = 0u64;
        for (i, &n) in self.counts.iter().enumerate() {
            seen += n as u64;
            if seen >= rank {
                let (lo, hi) = (bucket_low(i), if i + 1 < BUCKETS { bucket_low(i + 1) } else { bucket_low(i) * 2 });
                // Exact buckets (below 8 µs) hold one value.
                return Some(if hi - lo <= 1 { lo as f64 } else { (lo + hi) as f64 / 2.0 });
            }
        }
        None
    }

    /// `[[bucket, count], …]` of the non-empty buckets.
    fn to_json(&self) -> Value {
        Value::Array(self.counts.iter().enumerate().filter(|(_, n)| **n > 0).map(|(i, n)| json!([i, n])).collect())
    }

    fn from_json(v: &Value) -> Histogram {
        let mut h = Histogram::default();
        for p in v.as_array().into_iter().flatten() {
            if let (Some(i), Some(n)) = (p.get(0).and_then(Value::as_u64), p.get(1).and_then(Value::as_u64))
                && (i as usize) < BUCKETS
            {
                h.counts[i as usize] = h.counts[i as usize].saturating_add(n.min(u32::MAX as u64) as u32);
            }
        }
        h
    }
}

/// One kind's requests in a bucket of time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KindStats {
    /// Answered below 400.
    pub ok: u64,
    /// 4xx other than 429.
    pub client_errors: u64,
    /// 5xx.
    pub server_errors: u64,
    /// 429: refused by the cluster's limits (rate, requests or bytes in flight) or its thread
    /// pools.
    pub rejected: u64,
    /// Latency of every request that ran (a refusal at the door has none).
    pub latency: Histogram,
}

impl KindStats {
    pub fn requests(&self) -> u64 {
        self.ok + self.client_errors + self.server_errors + self.rejected
    }

    fn add(&mut self, status: u16, latency: Option<Duration>) {
        match status {
            429 => self.rejected += 1,
            500.. => self.server_errors += 1,
            400.. => self.client_errors += 1,
            _ => self.ok += 1,
        }
        if let Some(d) = latency {
            self.latency.record(d.as_micros().min(u64::MAX as u128) as u64);
        }
    }

    pub fn merge(&mut self, o: &KindStats) {
        self.ok += o.ok;
        self.client_errors += o.client_errors;
        self.server_errors += o.server_errors;
        self.rejected += o.rejected;
        self.latency.merge(&o.latency);
    }
}

/// Every kind's requests in a bucket of time (a minute, an hour, a chart's step).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub kinds: [KindStats; 3],
}

impl Stats {
    pub fn kind(&self, k: Kind) -> &KindStats {
        &self.kinds[k as usize]
    }

    /// Every kind together.
    pub fn all(&self) -> KindStats {
        let mut out = KindStats::default();
        for k in &self.kinds {
            out.merge(k);
        }
        out
    }

    pub fn merge(&mut self, o: &Stats) {
        for (a, b) in self.kinds.iter_mut().zip(o.kinds.iter()) {
            a.merge(b);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.kinds.iter().all(|k| k.requests() == 0)
    }

    /// The compact form the catalog keeps: `{"search":[ok,4xx,5xx,429,[[bucket,n],…]],…}`,
    /// kinds without requests left out.
    pub fn encode(&self) -> String {
        let mut o = serde_json::Map::new();
        for k in KINDS {
            let s = self.kind(k);
            if s.requests() > 0 {
                o.insert(k.name().into(), json!([s.ok, s.client_errors, s.server_errors, s.rejected, s.latency.to_json()]));
            }
        }
        Value::Object(o).to_string()
    }

    /// Reads [`Stats::encode`]'s form; anything unreadable counts as nothing.
    pub fn decode(s: &str) -> Stats {
        let mut out = Stats::default();
        let Ok(Value::Object(o)) = serde_json::from_str::<Value>(s) else { return out };
        for k in KINDS {
            let Some(a) = o.get(k.name()).and_then(Value::as_array) else { continue };
            let n = |i: usize| a.get(i).and_then(Value::as_u64).unwrap_or(0);
            out.kinds[k as usize] =
                KindStats { ok: n(0), client_errors: n(1), server_errors: n(2), rejected: n(3), latency: a.get(4).map(Histogram::from_json).unwrap_or_default() };
        }
        out
    }
}

/// The unix minute `t` falls in.
pub fn minute_of(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() / 60)
}

/// Finished minutes a recorder keeps until they are taken; past it, the oldest two merge (the
/// metering pass takes them every minute, so only a stalled pass gets here).
const MAX_PENDING: usize = 16;

#[derive(Default)]
struct Inner {
    minute: u64,
    current: Option<Box<Stats>>,
    /// Finished minutes, oldest first.
    done: Vec<(u64, Box<Stats>)>,
}

/// A loaded cluster's minutes not handed on yet (see the module docs).
#[derive(Default)]
pub struct Recorder {
    inner: Mutex<Inner>,
}

impl Recorder {
    /// Records one request of `kind` answered with `status` at `now`; `latency` is `None` for
    /// one refused before it ran.
    pub fn record(&self, now: SystemTime, kind: Kind, status: u16, latency: Option<Duration>) {
        let minute = minute_of(now);
        let mut g = self.inner.lock();
        if g.minute != minute {
            if let Some(cur) = g.current.take() {
                let m = g.minute;
                g.done.push((m, cur));
                if g.done.len() > MAX_PENDING {
                    let (_, oldest) = g.done.remove(0);
                    g.done[0].1.merge(&oldest);
                }
            }
            g.minute = minute;
        }
        g.current.get_or_insert_with(Box::default).kinds[kind as usize].add(status, latency);
    }

    /// Takes every minute recorded, oldest first (the one in progress too: the catalog adds
    /// minutes up, so the rest of it comes with the next take).
    pub fn take(&self) -> Vec<(u64, Box<Stats>)> {
        let mut g = self.inner.lock();
        let mut out = std::mem::take(&mut g.done);
        if let Some(cur) = g.current.take() {
            out.push((g.minute, cur));
        }
        out
    }

    /// Copies of the minutes not taken yet, oldest first.
    pub fn peek(&self) -> Vec<(u64, Stats)> {
        let g = self.inner.lock();
        let mut out: Vec<(u64, Stats)> = g.done.iter().map(|(m, s)| (*m, (**s).clone())).collect();
        if let Some(cur) = &g.current {
            out.push((g.minute, (**cur).clone()));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_log_spaced_and_contiguous() {
        for v in 0..8 {
            assert_eq!(bucket(v), v as usize);
        }
        assert_eq!((bucket(8), bucket(15), bucket(16), bucket(17), bucket(18)), (8, 15, 16, 16, 17));
        // Every bucket's low maps back to it, and the next low is the first value past it.
        for i in 0..BUCKETS - 1 {
            assert_eq!(bucket(bucket_low(i)), i, "{i}");
            assert_eq!(bucket(bucket_low(i + 1) - 1), i, "{i}");
        }
        // Past the top: the last bucket (about 4.5 hours).
        assert_eq!(bucket(u64::MAX), BUCKETS - 1);
        assert!(bucket_low(BUCKETS - 1) > 4 * 3600 * 1_000_000);
        // Within 1/8 of a power of two: a bucket is at most 12.5% wide.
        for i in 8..BUCKETS - 1 {
            let (lo, hi) = (bucket_low(i), bucket_low(i + 1));
            assert!((hi - lo) as f64 / lo as f64 <= 0.125 + 1e-9, "{i}: {lo}..{hi}");
        }
    }

    #[test]
    fn quantiles_are_within_the_bucket_error() {
        let mut h = Histogram::default();
        assert_eq!(h.quantile(0.5), None);
        // 1..=10_000 µs, uniform: p50 ≈ 5000, p95 ≈ 9500, p99 ≈ 9900.
        for v in 1..=10_000 {
            h.record(v);
        }
        assert_eq!(h.count(), 10_000);
        for (q, want) in [(0.5, 5000.0), (0.95, 9500.0), (0.99, 9900.0)] {
            let got = h.quantile(q).unwrap();
            assert!((got - want).abs() / want < 0.07, "p{q}: {got} against {want}");
        }
        // Exact at the bottom.
        let mut small = Histogram::default();
        small.record(3);
        assert_eq!(small.quantile(0.99), Some(3.0));
        // Merging is adding: two halves make the whole.
        let (mut a, mut b) = (Histogram::default(), Histogram::default());
        for v in 1..=10_000u64 {
            if v % 2 == 0 { a.record(v) } else { b.record(v) }
        }
        a.merge(&b);
        assert_eq!(a, h);
    }

    #[test]
    fn kinds_follow_the_route() {
        for (m, u, k) in [
            ("GET", "/logs/_search?q=x", Kind::Search),
            ("POST", "/_msearch", Kind::Search),
            ("POST", "/a,b/_count", Kind::Search),
            ("POST", "/_query", Kind::Search),
            ("POST", "/_bulk", Kind::Index),
            ("PUT", "/logs/_doc/1", Kind::Index),
            ("POST", "/logs/_doc/_search", Kind::Index),
            ("POST", "/logs/_update/1", Kind::Index),
            ("DELETE", "/logs/_doc/1", Kind::Index),
            ("GET", "/logs/_doc/1", Kind::Other),
            ("PUT", "/logs", Kind::Other),
            ("GET", "/_cat/indices", Kind::Other),
        ] {
            assert_eq!(Kind::of(m, u), k, "{m} {u}");
        }
    }

    #[test]
    fn stats_encode_and_merge() {
        let mut s = Stats::default();
        s.kinds[0].add(200, Some(Duration::from_micros(900)));
        s.kinds[0].add(404, Some(Duration::from_millis(2)));
        s.kinds[1].add(503, Some(Duration::from_millis(40)));
        s.kinds[1].add(429, None);
        let back = Stats::decode(&s.encode());
        assert_eq!(back, s);
        assert_eq!(s.encode().matches("other").count(), 0, "empty kinds are left out");
        assert_eq!(Stats::decode("not json"), Stats::default());
        let all = s.all();
        assert_eq!((all.requests(), all.ok, all.client_errors, all.server_errors, all.rejected, all.latency.count()), (4, 1, 1, 1, 1, 3));
        let mut twice = s.clone();
        twice.merge(&s);
        assert_eq!(twice.all().requests(), 8);
    }

    /// What recording adds to a request, alone: classifying it, the two clock reads and the
    /// recorder (`cargo test --release -p search-server metrics::tests::record_cost -- --ignored
    /// --nocapture`). The end-to-end figure is in docs/search/architecture.md, "Observability".
    #[test]
    #[ignore]
    fn record_cost() {
        let r = Recorder::default();
        let n = 5_000_000u32;
        let start = std::time::Instant::now();
        for i in 0..n {
            let t = std::time::Instant::now();
            let kind = Kind::of(std::hint::black_box("POST"), std::hint::black_box("/docs/_search"));
            r.record(SystemTime::now(), kind, std::hint::black_box(200 + (i & 1) as u16), Some(t.elapsed()));
        }
        let per = start.elapsed().as_nanos() as f64 / n as f64;
        println!("record_cost: {per:.1} ns per request");
        assert_eq!(r.take().iter().map(|(_, s)| s.all().requests()).sum::<u64>(), n as u64);
    }

    #[test]
    fn the_recorder_keeps_minutes_until_taken() {
        let r = Recorder::default();
        let at = |m: u64| UNIX_EPOCH + Duration::from_secs(m * 60 + 5);
        r.record(at(100), Kind::Search, 200, Some(Duration::from_millis(1)));
        r.record(at(100), Kind::Search, 200, Some(Duration::from_millis(3)));
        r.record(at(101), Kind::Index, 201, Some(Duration::from_millis(5)));
        let peeked = r.peek();
        assert_eq!(peeked.iter().map(|(m, s)| (*m, s.all().requests())).collect::<Vec<_>>(), [(100, 2), (101, 1)]);
        let taken = r.take();
        assert_eq!(taken.iter().map(|(m, s)| (*m, s.all().requests())).collect::<Vec<_>>(), [(100, 2), (101, 1)]);
        assert!(r.take().is_empty() && r.peek().is_empty(), "taken once");
        // The rest of a minute already taken comes with the next take.
        r.record(at(101), Kind::Other, 200, Some(Duration::from_millis(1)));
        assert_eq!(r.take().iter().map(|(m, _)| *m).collect::<Vec<_>>(), [101]);
        // A stalled taker: finished minutes stay bounded, nothing is lost.
        for m in 200..240 {
            r.record(at(m), Kind::Search, 200, None);
        }
        let kept = r.take();
        assert!(kept.len() <= MAX_PENDING + 1, "{}", kept.len());
        assert_eq!(kept.iter().map(|(_, s)| s.all().requests()).sum::<u64>(), 40);
    }
}
