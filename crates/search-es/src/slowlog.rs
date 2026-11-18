//! The search and indexing slow logs (`SearchSlowLog`, `IndexingSlowLog`): a shard's query or
//! fetch phase, or an index operation on its primary, that took longer than one of its index's
//! thresholds becomes an entry formatted as Elasticsearch's JSON slow log line
//! (`elasticsearch.slowlog.*`).
//!
//! - **Thresholds** are the index settings `index.search.slowlog.threshold.{query,fetch}.{warn,
//!   info,debug,trace}` and `index.indexing.slowlog.threshold.index.*` (`-1`: off, the default).
//!   Each shard caches them as atomics ([`Thresholds`]), refreshed whenever its index's settings
//!   are applied, so the hot path is one relaxed load per phase while no threshold is set. The
//!   level is the highest threshold passed (`took > threshold`, as Elasticsearch compares).
//! - **Source.** Indexing entries keep the document's source up to `index.indexing.slowlog.source`
//!   characters (1000 by default; `true` all of it, `false` or `0` none). Search entries keep
//!   the request body up to [`SEARCH_SOURCE_CHARS`]: Elasticsearch logs a search's whole source
//!   and has no setting for it (9.5.5 refuses `index.search.slowlog.source`), but roll keeps
//!   entries in memory and in its catalog, so each is bounded. No entry ever holds a header
//!   other than `X-Opaque-Id`.
//! - **Where they go.** A cluster's entries wait in its [`SlowLog`] (at most [`MAX_ENTRIES`] and
//!   [`MAX_BYTES`]; the oldest go first) until the node hands them to the control plane, which
//!   keeps the newest [`MAX_ENTRIES`] per cluster.

use crate::meta::IndexMeta;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, Ordering};

/// Entries a cluster keeps.
pub const MAX_ENTRIES: usize = 500;
/// Bytes of entries a cluster keeps in memory.
pub const MAX_BYTES: usize = 1 << 20;
/// Characters of a search's source an entry keeps.
pub const SEARCH_SOURCE_CHARS: usize = 1000;
/// The most characters of a document's source an entry keeps, whatever
/// `index.indexing.slowlog.source` says (`true` means this many).
pub const MAX_SOURCE_CHARS: usize = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Query,
    Fetch,
    Index,
}

impl Phase {
    /// The logger Elasticsearch writes the entry with.
    pub fn logger(self) -> &'static str {
        match self {
            Phase::Query => "index.search.slowlog.query",
            Phase::Fetch => "index.search.slowlog.fetch",
            Phase::Index => "index.indexing.slowlog.index",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
        }
    }
}

const LEVELS: [&str; 4] = ["warn", "info", "debug", "trace"];
const BY_RANK: [Level; 4] = [Level::Warn, Level::Info, Level::Debug, Level::Trace];

/// One phase's thresholds in nanoseconds (warn, info, debug, trace; -1: off), and the lowest
/// one set (`i64::MAX`: none), checked first.
#[derive(Debug)]
struct PhaseThresholds {
    at: [AtomicI64; 4],
    min: AtomicI64,
}

impl Default for PhaseThresholds {
    fn default() -> Self {
        PhaseThresholds { at: std::array::from_fn(|_| AtomicI64::new(-1)), min: AtomicI64::new(i64::MAX) }
    }
}

impl PhaseThresholds {
    fn set(&self, idx: &IndexMeta, prefix: &str) {
        let mut min = i64::MAX;
        for (slot, level) in self.at.iter().zip(LEVELS) {
            let key = format!("{prefix}.{level}");
            let nanos = idx
                .settings
                .get(&key)
                .and_then(|v| crate::units::TimeValue::parse(v, &key).ok())
                .map_or(-1, |t| t.nanos().clamp(-1, i64::MAX as i128) as i64);
            slot.store(nanos, Ordering::Relaxed);
            if nanos >= 0 {
                min = min.min(nanos);
            }
        }
        self.min.store(min, Ordering::Relaxed);
    }

    fn on(&self) -> bool {
        self.min.load(Ordering::Relaxed) != i64::MAX
    }

    fn level(&self, nanos: i64) -> Option<Level> {
        if nanos <= self.min.load(Ordering::Relaxed) {
            return None;
        }
        self.at.iter().zip(BY_RANK).find(|(t, _)| {
            let t = t.load(Ordering::Relaxed);
            t >= 0 && nanos > t
        }).map(|(_, l)| l)
    }
}

/// A shard's slow log thresholds, from its index's settings ([`Thresholds::apply`]).
#[derive(Debug)]
pub struct Thresholds {
    query: PhaseThresholds,
    fetch: PhaseThresholds,
    index: PhaseThresholds,
    /// `index.indexing.slowlog.source` in characters.
    source: AtomicI64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds { query: Default::default(), fetch: Default::default(), index: Default::default(), source: AtomicI64::new(MAX_SOURCE_CHARS as i64) }
    }
}

impl Thresholds {
    /// Reads `idx`'s thresholds (its settings were validated when set).
    pub fn apply(&self, idx: &IndexMeta) {
        self.query.set(idx, "index.search.slowlog.threshold.query");
        self.fetch.set(idx, "index.search.slowlog.threshold.fetch");
        self.index.set(idx, "index.indexing.slowlog.threshold.index");
        // `IndexingSlowLog.INDEX_INDEXING_SLOWLOG_MAX_SOURCE_CHARS_TO_LOG_SETTING`.
        let chars = match idx.settings.get("index.indexing.slowlog.source") {
            None => MAX_SOURCE_CHARS as i64,
            Some(v) if v.eq_ignore_ascii_case("true") => MAX_SOURCE_CHARS as i64,
            Some(v) if v.eq_ignore_ascii_case("false") => 0,
            Some(v) => v.trim().parse::<i64>().map_or(MAX_SOURCE_CHARS as i64, |n| n.clamp(0, MAX_SOURCE_CHARS as i64)),
        };
        self.source.store(chars, Ordering::Relaxed);
    }

    fn of(&self, phase: Phase) -> &PhaseThresholds {
        match phase {
            Phase::Query => &self.query,
            Phase::Fetch => &self.fetch,
            Phase::Index => &self.index,
        }
    }

    /// Whether any threshold of `phase` is set: one relaxed load, so callers time a phase only
    /// when it can be logged.
    pub fn on(&self, phase: Phase) -> bool {
        self.of(phase).on()
    }

    /// The level an operation of `phase` that took `nanos` is logged at, if any.
    pub fn level(&self, phase: Phase, nanos: u64) -> Option<Level> {
        self.of(phase).level(nanos.min(i64::MAX as u64) as i64)
    }

    /// Characters of a document's source an indexing entry keeps.
    pub fn source_chars(&self) -> usize {
        self.source.load(Ordering::Relaxed).max(0) as usize
    }
}

/// What a search's entries say about the request: its body and `X-Opaque-Id`. Cheap to build
/// (the body is a slice of the request's own bytes) and read only when an entry is made.
#[derive(Clone, Debug, Default)]
pub struct Origin {
    pub source: Option<bytes::Bytes>,
    pub opaque_id: Option<String>,
}

impl Origin {
    /// `raw` (a body the request carried) as a slice of `body` when it is one, without a copy.
    pub fn of(body: &bytes::Bytes, raw: Option<&[u8]>, opaque_id: Option<&str>) -> Origin {
        let source = raw.and_then(|r| {
            let (start, end) = (body.as_ptr() as usize, body.as_ptr() as usize + body.len());
            let p = r.as_ptr() as usize;
            (!r.is_empty() && p >= start && p + r.len() <= end).then(|| body.slice_ref(r))
        });
        Origin { source, opaque_id: opaque_id.map(str::to_string) }
    }
}

/// One slow log entry.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Unix milliseconds.
    pub at_ms: i64,
    pub phase: Phase,
    pub level: Level,
    pub took_nanos: u64,
    pub index: String,
    /// The index's uuid (indexing entries name the index as `[name/uuid]`).
    pub uuid: String,
    pub shard: u32,
    /// The search's body or the document, cut at its limit.
    pub source: String,
    pub opaque_id: Option<String>,
    /// Search entries: `"5 hits"`, `"10000+ hits"`.
    pub total_hits: Option<String>,
    pub total_shards: Option<usize>,
    pub search_type: Option<&'static str>,
    /// Search entries: the request's `stats` groups.
    pub stats: Vec<String>,
    /// Indexing entries: the document's id and routing.
    pub id: Option<String>,
    pub routing: Option<String>,
}

/// `s` cut at `chars` characters.
pub fn cut(s: &str, chars: usize) -> &str {
    match s.char_indices().nth(chars) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

impl Entry {
    /// Roughly what it holds, for the in-memory bound.
    pub fn bytes(&self) -> usize {
        160 + self.index.len() + self.uuid.len() + self.source.len() + self.opaque_id.as_ref().map_or(0, String::len) + self.id.as_ref().map_or(0, String::len) + self.routing.as_ref().map_or(0, String::len)
    }

    /// The entry as Elasticsearch's JSON slow log writes it (ECS fields and
    /// `elasticsearch.slowlog.*`).
    pub fn to_json(&self) -> Value {
        let took = crate::units::TimeValue { duration: self.took_nanos.min(i64::MAX as u64) as i64, unit: crate::units::TimeUnit::Nanos }.human();
        let at = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(self.at_ms).map(|t| t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()).unwrap_or_default();
        let mut v = json!({
            "@timestamp": at,
            "log.level": self.level.as_str(),
            "log.logger": self.phase.logger(),
            "elasticsearch.index.name": self.index,
            "elasticsearch.slowlog.took": took,
            "elasticsearch.slowlog.took_millis": self.took_nanos / 1_000_000,
            "elasticsearch.slowlog.source": self.source,
        });
        let o = v.as_object_mut().expect("an object");
        match self.phase {
            Phase::Query | Phase::Fetch => {
                o.insert("elasticsearch.slowlog.message".into(), json!(format!("[{}][{}]", self.index, self.shard)));
                o.insert("elasticsearch.shard.id".into(), json!(self.shard));
                o.insert("elasticsearch.slowlog.total_hits".into(), json!(self.total_hits.as_deref().unwrap_or("-1")));
                let stats: Vec<String> = self.stats.iter().map(|s| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))).collect();
                o.insert("elasticsearch.slowlog.stats".into(), json!(format!("[{}]", stats.join(","))));
                o.insert("elasticsearch.slowlog.search_type".into(), json!(self.search_type.unwrap_or("QUERY_THEN_FETCH")));
                o.insert("elasticsearch.slowlog.total_shards".into(), json!(self.total_shards.unwrap_or(1)));
                if let Some(id) = &self.opaque_id {
                    o.insert("elasticsearch.slowlog.id".into(), json!(id));
                }
            }
            Phase::Index => {
                o.insert("elasticsearch.slowlog.message".into(), json!(format!("[{}/{}]", self.index, self.uuid)));
                o.insert("elasticsearch.shard.id".into(), json!(self.shard));
                if let Some(id) = &self.id {
                    o.insert("elasticsearch.slowlog.id".into(), json!(id));
                }
                if let Some(r) = &self.routing {
                    o.insert("elasticsearch.slowlog.routing".into(), json!(r));
                }
                if let Some(id) = &self.opaque_id {
                    o.insert("elasticsearch.slowlog.x_opaque_id".into(), json!(id));
                }
            }
        }
        v
    }
}

#[derive(Default)]
struct Ring {
    entries: VecDeque<Entry>,
    bytes: usize,
}

/// A cluster's entries not yet handed on, newest last; at most [`MAX_ENTRIES`] and
/// [`MAX_BYTES`].
#[derive(Default)]
pub struct SlowLog {
    ring: Mutex<Ring>,
}

impl std::fmt::Debug for SlowLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlowLog").field("entries", &self.ring.lock().entries.len()).finish()
    }
}

impl SlowLog {
    pub fn push(&self, e: Entry) {
        let n = e.bytes();
        let mut r = self.ring.lock();
        r.bytes += n;
        r.entries.push_back(e);
        while r.entries.len() > MAX_ENTRIES || (r.bytes > MAX_BYTES && r.entries.len() > 1) {
            let Some(old) = r.entries.pop_front() else { break };
            r.bytes -= old.bytes();
        }
    }

    /// Takes every entry, oldest first.
    pub fn take(&self) -> Vec<Entry> {
        let mut r = self.ring.lock();
        r.bytes = 0;
        r.entries.drain(..).collect()
    }

    /// The entries, oldest first, left in place.
    pub fn peek(&self) -> Vec<Entry> {
        self.ring.lock().entries.iter().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.ring.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// `TotalHits.toString`'s count: `"5 hits"`, `"10000+ hits"`.
pub fn total_hits(total: u64, at_least: bool) -> String {
    format!("{total}{} hits", if at_least { "+" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(settings: &[(&str, &str)]) -> IndexMeta {
        let mut s = crate::meta::settings::Settings::new();
        for (k, v) in settings {
            s.set(*k, *v);
        }
        IndexMeta {
            name: "i".into(),
            uuid: "u".into(),
            state: crate::meta::IndexState::Open,
            settings: s,
            mapping: std::sync::Arc::new(crate::mapping::Mapping::empty()),
            aliases: Default::default(),
            version: 1,
            mapping_version: 1,
            settings_version: 1,
            aliases_version: 1,
            routing_num_shards: 1,
            primary_terms: vec![1],
            system: false,
            recovery_source: None,
            rollover_info: Default::default(),
            dialect: Default::default(),
        }
    }

    #[test]
    fn thresholds_pick_the_highest_level_passed() {
        let t = Thresholds::default();
        assert!(!t.on(Phase::Query));
        assert_eq!(t.level(Phase::Query, u64::MAX), None);
        t.apply(&idx(&[("index.search.slowlog.threshold.query.warn", "10ms"), ("index.search.slowlog.threshold.query.info", "5ms"), ("index.indexing.slowlog.threshold.index.trace", "0ms")]));
        assert!(t.on(Phase::Query) && !t.on(Phase::Fetch) && t.on(Phase::Index));
        assert_eq!(t.level(Phase::Query, 5_000_000), None, "took > threshold, as Elasticsearch compares");
        assert_eq!(t.level(Phase::Query, 5_000_001), Some(Level::Info));
        assert_eq!(t.level(Phase::Query, 10_000_001), Some(Level::Warn));
        assert_eq!(t.level(Phase::Fetch, u64::MAX), None);
        assert_eq!(t.level(Phase::Index, 1), Some(Level::Trace));
        assert_eq!(t.level(Phase::Index, 0), None);
        // Back off: everything at -1 again.
        t.apply(&idx(&[]));
        assert!(!t.on(Phase::Query) && !t.on(Phase::Index));
    }

    #[test]
    fn indexing_source_chars() {
        let t = Thresholds::default();
        for (v, chars) in [("true", MAX_SOURCE_CHARS), ("false", 0), ("0", 0), ("12", 12), ("5000", MAX_SOURCE_CHARS)] {
            t.apply(&idx(&[("index.indexing.slowlog.source", v)]));
            assert_eq!(t.source_chars(), chars, "{v}");
        }
        t.apply(&idx(&[]));
        assert_eq!(t.source_chars(), 1000);
        assert_eq!(cut("héllo", 2), "hé");
        assert_eq!(cut("hi", 10), "hi");
    }

    fn entry(n: usize, source: usize) -> Entry {
        Entry {
            at_ms: 1_700_000_000_000 + n as i64,
            phase: Phase::Query,
            level: Level::Warn,
            took_nanos: 12_300_000,
            index: "logs".into(),
            uuid: "u".into(),
            shard: 0,
            source: "x".repeat(source),
            opaque_id: Some("trace-1".into()),
            total_hits: Some(total_hits(5, false)),
            total_shards: Some(1),
            search_type: Some("QUERY_THEN_FETCH"),
            stats: vec!["group".into()],
            id: None,
            routing: None,
        }
    }

    #[test]
    fn the_ring_is_bounded() {
        let log = SlowLog::default();
        for n in 0..MAX_ENTRIES + 20 {
            log.push(entry(n, 10));
        }
        let all = log.peek();
        assert_eq!(all.len(), MAX_ENTRIES);
        assert_eq!(all[0].at_ms, entry(20, 0).at_ms, "the oldest went first");
        // Large entries: the byte budget holds.
        for n in 0..2000 {
            log.push(entry(n, 1000));
        }
        let r = log.ring.lock();
        assert!(r.bytes <= MAX_BYTES && r.entries.iter().map(Entry::bytes).sum::<usize>() == r.bytes);
        drop(r);
        assert!(!log.take().is_empty());
        assert!(log.is_empty());
    }

    #[test]
    fn entries_read_like_elasticsearchs() {
        let v = entry(0, 3).to_json();
        assert_eq!(v["log.level"], "WARN");
        assert_eq!(v["log.logger"], "index.search.slowlog.query");
        assert_eq!(v["elasticsearch.slowlog.message"], "[logs][0]");
        assert_eq!(v["elasticsearch.slowlog.took"], "12.3ms");
        assert_eq!(v["elasticsearch.slowlog.took_millis"], 12);
        assert_eq!(v["elasticsearch.slowlog.total_hits"], "5 hits");
        assert_eq!(v["elasticsearch.slowlog.stats"], "[\"group\"]");
        assert_eq!(v["elasticsearch.slowlog.id"], "trace-1");
        assert_eq!(v["@timestamp"], "2023-11-14T22:13:20.000Z");
        let mut i = entry(0, 3);
        i.phase = Phase::Index;
        i.id = Some("1".into());
        let v = i.to_json();
        assert_eq!((v["elasticsearch.slowlog.message"].as_str(), v["elasticsearch.slowlog.id"].as_str()), (Some("[logs/u]"), Some("1")));
    }

    #[test]
    fn origin_borrows_the_body() {
        let body = bytes::Bytes::from_static(b"{\"query\":{}}");
        let o = Origin::of(&body, Some(&body[..]), Some("x"));
        assert_eq!(o.source.as_deref(), Some(&body[..]));
        // A body that isn't the request's own isn't copied.
        let other = b"{}".to_vec();
        assert!(Origin::of(&body, Some(&other), None).source.is_none());
    }
}
