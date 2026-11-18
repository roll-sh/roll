//! Search analytics (Algolia's and Elastic App Search's): which queries an index is searched
//! with, which of them find nothing, and how often their results are clicked.
//!
//! - **Off by default, per index.** The index setting `index.roll.analytics.enabled` (a roll
//!   setting: Elasticsearch has none) turns it on. Query text is the tenant's data, so nothing is
//!   kept for an index that hasn't opted in, and turning it off (or deleting the index) drops
//!   what is waiting here and tells whoever keeps the rest to delete it ([`Taken::cleared`]).
//! - **What a search records**: its text ([`query_text`]: an `X-Roll-Search-Query` header, the
//!   `q` parameter, or the user's text in the body's query, retriever or `knn`), normalized
//!   ([`normalize`]), whether it found nothing, its hit count and its latency. Never a header
//!   besides that one, a credential or a document. A search with no text records nothing.
//! - **Clicks** come from Elasticsearch's own `search_click` analytics events
//!   (`POST _application/analytics/<collection>/event/search_click`): the clicked document's
//!   index, the search's query and, when the event lists the results, the document's position.
//! - **Aggregated in place, bounded.** Each index has one [`Bucket`] per hour: exact totals
//!   and two bounded heavy-hitter lists ([`TopQueries`], Space-Saving), one of every query and
//!   one of queries that found nothing. A query searched more than `searches / capacity` times in
//!   the hour is always kept. Nothing is a log of searches.
//! - **Cost.** While no index of the cluster has it on, a search pays one relaxed load
//!   ([`Analytics::on`]). Buckets wait here until the node hands them on ([`Analytics::take`],
//!   every metering pass), at most [`MAX_PENDING_HOURS`] per index.

use crate::json::Value;
use crate::meta::ClusterState;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

/// The index setting that turns analytics on.
pub const SETTING: &str = "index.roll.analytics.enabled";
/// The header a client can name a search's query text with.
pub const QUERY_HEADER: &str = "x-roll-search-query";
/// Characters of a query kept (longer ones are cut).
pub const MAX_QUERY_CHARS: usize = 128;
/// Queries an index's pending hour holds ([`TopQueries`]).
pub const PENDING_TOP: usize = 256;
/// Queries with no results an index's pending hour holds.
pub const PENDING_NO_HITS: usize = 128;
/// Hours an index keeps waiting when nobody takes them (a node without a control plane).
pub const MAX_PENDING_HOURS: usize = 48;
/// Bytes of query text a cluster keeps waiting, over all its indices: past it, a query not
/// listed yet is counted only in the totals.
pub const MAX_PENDING_BYTES: usize = 4 << 20;

/// What one query saw in a bucket.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryStats {
    pub searches: u64,
    /// Searches that found nothing.
    pub no_hits: u64,
    /// Hits summed over its searches.
    pub hits: u64,
    /// Latency summed over its searches, in microseconds.
    pub latency_us: u64,
    pub clicks: u64,
    /// Positions (1-based) summed over the clicks that had one.
    pub position_sum: u64,
    /// Clicks that had a position.
    pub positioned: u64,
}

impl QueryStats {
    pub fn add(&mut self, o: &QueryStats) {
        self.searches += o.searches;
        self.no_hits += o.no_hits;
        self.hits += o.hits;
        self.latency_us += o.latency_us;
        self.clicks += o.clicks;
        self.position_sum += o.position_sum;
        self.positioned += o.positioned;
    }

    fn to_json(self, q: &str) -> serde_json::Value {
        serde_json::json!([q, self.searches, self.no_hits, self.hits, self.latency_us, self.clicks, self.position_sum, self.positioned])
    }

    fn from_json(v: &serde_json::Value) -> Option<(String, QueryStats)> {
        let a = v.as_array()?;
        let n = |i: usize| a.get(i).and_then(serde_json::Value::as_u64).unwrap_or(0);
        Some((
            a.first()?.as_str()?.to_string(),
            QueryStats { searches: n(1), no_hits: n(2), hits: n(3), latency_us: n(4), clicks: n(5), position_sum: n(6), positioned: n(7) },
        ))
    }
}

/// A bounded heavy-hitter list of queries (Space-Saving, Metwally et al.): at most `cap`
/// queries. A query not listed while the list is full takes the place of the one searched least,
/// and starts from its search count (so a count is at most that much over).
#[derive(Clone, Debug)]
pub struct TopQueries {
    cap: usize,
    map: HashMap<Box<str>, QueryStats>,
}

impl TopQueries {
    pub fn new(cap: usize) -> TopQueries {
        TopQueries { cap, map: HashMap::new() }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn get(&self, q: &str) -> Option<&QueryStats> {
        self.map.get(q)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &QueryStats)> {
        self.map.iter().map(|(k, v)| (&**k, v))
    }

    /// The entry of `q`, made if missing (evicting the least searched one when full) while
    /// `bytes` of text are held in all, kept up to date, is under [`MAX_PENDING_BYTES`].
    fn slot(&mut self, q: &str, bytes: &mut usize) -> Option<&mut QueryStats> {
        if !self.map.contains_key(q) {
            if *bytes >= MAX_PENDING_BYTES {
                return None;
            }
            let mut fresh = QueryStats::default();
            if self.map.len() >= self.cap.max(1) {
                let least = self.map.iter().min_by_key(|(_, s)| (s.searches, s.clicks)).map(|(k, s)| (k.clone(), s.searches));
                if let Some((k, searches)) = least {
                    self.map.remove(&k);
                    *bytes = bytes.saturating_sub(k.len());
                    fresh.searches = searches;
                }
            }
            *bytes += q.len();
            self.map.insert(q.into(), fresh);
        }
        self.map.get_mut(q)
    }

    /// Adds `other`'s counts, then keeps the `cap` most searched (a merge of two hours, or of
    /// what a node handed on into what the catalog keeps).
    pub fn merge(&mut self, other: &TopQueries) {
        for (q, s) in &other.map {
            self.map.entry(q.clone()).or_default().add(s);
        }
        self.trim();
    }

    fn trim(&mut self) {
        if self.map.len() <= self.cap {
            return;
        }
        let mut v: Vec<(Box<str>, QueryStats)> = self.map.drain().collect();
        v.sort_unstable_by(|a, b| (b.1.searches, b.1.clicks).cmp(&(a.1.searches, a.1.clicks)).then_with(|| a.0.cmp(&b.0)));
        v.truncate(self.cap);
        self.map = v.into_iter().collect();
    }

    /// The queries, most searched first (ties: most clicked, then by text).
    pub fn ranked(&self) -> Vec<(&str, &QueryStats)> {
        let mut v: Vec<(&str, &QueryStats)> = self.iter().collect();
        v.sort_unstable_by(|a, b| (b.1.searches, b.1.clicks).cmp(&(a.1.searches, a.1.clicks)).then_with(|| a.0.cmp(b.0)));
        v
    }

    /// Bytes of query text held.
    fn text_bytes(&self) -> usize {
        self.map.keys().map(|k| k.len()).sum()
    }
}

/// An index's searches over one stretch of time (an hour here; an hour or a day in the
/// catalog).
#[derive(Clone, Debug)]
pub struct Bucket {
    /// Exact totals, whatever the lists dropped.
    pub searches: u64,
    pub no_hits: u64,
    pub clicks: u64,
    pub top: TopQueries,
    /// Queries that found nothing (only `searches` is counted).
    pub empty: TopQueries,
}

impl Bucket {
    pub fn new(top: usize, empty: usize) -> Bucket {
        Bucket { searches: 0, no_hits: 0, clicks: 0, top: TopQueries::new(top), empty: TopQueries::new(empty) }
    }

    fn pending() -> Bucket {
        Bucket::new(PENDING_TOP, PENDING_NO_HITS)
    }

    pub fn is_empty(&self) -> bool {
        self.searches == 0 && self.clicks == 0
    }

    fn search(&mut self, q: &str, hits: u64, latency_us: u64, bytes: &mut usize) {
        self.searches += 1;
        if hits == 0 {
            self.no_hits += 1;
        }
        if let Some(s) = self.top.slot(q, bytes) {
            s.searches += 1;
            s.hits += hits;
            s.latency_us += latency_us;
            if hits == 0 {
                s.no_hits += 1;
            }
        }
        if hits == 0
            && let Some(s) = self.empty.slot(q, bytes)
        {
            s.searches += 1;
        }
    }

    fn click(&mut self, q: &str, position: Option<u32>, bytes: &mut usize) {
        self.clicks += 1;
        if let Some(s) = self.top.slot(q, bytes) {
            s.clicks += 1;
            if let Some(p) = position {
                s.position_sum += u64::from(p);
                s.positioned += 1;
            }
        }
    }

    /// Adds `other` (each list then keeps its own capacity).
    pub fn merge(&mut self, other: &Bucket) {
        self.searches += other.searches;
        self.no_hits += other.no_hits;
        self.clicks += other.clicks;
        self.top.merge(&other.top);
        self.empty.merge(&other.empty);
    }

    /// The bucket with lists of other capacities (what the catalog keeps).
    pub fn with_caps(mut self, top: usize, empty: usize) -> Bucket {
        self.top.cap = top;
        self.empty.cap = empty;
        self.top.trim();
        self.empty.trim();
        self
    }

    /// Compact JSON for storage: totals and the lists as arrays.
    pub fn encode(&self) -> String {
        serde_json::json!({
            "s": self.searches,
            "z": self.no_hits,
            "c": self.clicks,
            "q": self.top.map.iter().map(|(q, s)| s.to_json(q)).collect::<Vec<_>>(),
            "n": self.empty.map.iter().map(|(q, s)| serde_json::json!([&**q, s.searches])).collect::<Vec<_>>(),
        })
        .to_string()
    }

    /// Reads [`Bucket::encode`]'s output into lists of these capacities (anything unreadable is
    /// an empty bucket).
    pub fn decode(data: &str, top: usize, empty: usize) -> Bucket {
        let mut b = Bucket::new(top, empty);
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { return b };
        let n = |k: &str| v.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
        b.searches = n("s");
        b.no_hits = n("z");
        b.clicks = n("c");
        for e in v.get("q").and_then(serde_json::Value::as_array).into_iter().flatten() {
            if let Some((q, s)) = QueryStats::from_json(e) {
                b.top.map.insert(q.into(), s);
            }
        }
        for e in v.get("n").and_then(serde_json::Value::as_array).into_iter().flatten() {
            if let (Some(q), Some(c)) = (e.get(0).and_then(serde_json::Value::as_str), e.get(1).and_then(serde_json::Value::as_u64)) {
                b.empty.map.insert(q.into(), QueryStats { searches: c, ..Default::default() });
            }
        }
        b.top.trim();
        b.empty.trim();
        b
    }

    fn text_bytes(&self) -> usize {
        self.top.text_bytes() + self.empty.text_bytes()
    }
}

/// What a cluster's analytics hand on ([`Analytics::take`]).
#[derive(Debug, Default)]
pub struct Taken {
    /// Indices whose analytics were turned off or that were deleted (or replaced): everything
    /// kept for them goes, before `hours` are added.
    pub cleared: Vec<String>,
    /// Set by the first take after the cluster loaded: the indices with analytics on, so
    /// whatever is kept for any other index goes (a clear another node never handed on).
    pub enabled: Option<Vec<String>>,
    /// (index, unix hour, bucket), oldest first per index.
    pub hours: Vec<(String, u64, Bucket)>,
}

impl Taken {
    pub fn is_empty(&self) -> bool {
        self.cleared.is_empty() && self.enabled.is_none() && self.hours.is_empty()
    }

    /// Adds `newer` (taken after this): its clears drop the older hours of those indices.
    pub fn extend(&mut self, newer: Taken) {
        if !newer.cleared.is_empty() {
            self.hours.retain(|(i, _, _)| !newer.cleared.contains(i));
        }
        if let Some(on) = &newer.enabled {
            self.hours.retain(|(i, _, _)| on.contains(i));
        }
        for c in newer.cleared {
            if !self.cleared.contains(&c) {
                self.cleared.push(c);
            }
        }
        if newer.enabled.is_some() {
            self.enabled = newer.enabled;
        }
        self.hours.extend(newer.hours);
    }
}

#[derive(Default)]
struct Pending {
    /// Per index, its hours not handed on yet, oldest first.
    hours: HashMap<String, Vec<(u64, Bucket)>>,
    cleared: Vec<String>,
    /// The enabled set is due to be handed on (a fresh load).
    announce: bool,
    /// Bytes of query text in `hours`.
    bytes: usize,
}

impl Pending {
    /// Index `index`'s bucket of `hour`, and the byte count to charge its new text to.
    fn hour(&mut self, index: &str, hour: u64) -> (&mut Bucket, &mut usize) {
        if !self.hours.contains_key(index) {
            self.hours.insert(index.to_string(), Vec::new());
        }
        let hs = self.hours.get_mut(index).expect("just inserted");
        if hs.last().is_none_or(|(h, _)| *h != hour) {
            if hs.len() >= MAX_PENDING_HOURS {
                let (_, old) = hs.remove(0);
                self.bytes = self.bytes.saturating_sub(old.text_bytes());
            }
            hs.push((hour, Bucket::pending()));
        }
        (&mut hs.last_mut().expect("just pushed").1, &mut self.bytes)
    }

    fn drop_index(&mut self, index: &str) {
        if let Some(hs) = self.hours.remove(index) {
            let freed: usize = hs.iter().map(|(_, b)| b.text_bytes()).sum();
            self.bytes = self.bytes.saturating_sub(freed);
        }
    }
}

/// A cluster's search analytics: which indices have it on, and their buckets not handed on yet.
#[derive(Default)]
pub struct Analytics {
    /// Whether any index has it on: the one load a search pays while none does.
    any: AtomicBool,
    /// Index name → uuid, for each index with it on.
    enabled: RwLock<HashMap<String, String>>,
    /// [`Analytics::sync`] ran (the cluster opened).
    synced: AtomicBool,
    pending: Mutex<Pending>,
}

impl std::fmt::Debug for Analytics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Analytics").field("any", &self.any.load(Ordering::Relaxed)).finish()
    }
}

/// Whether `idx` has analytics on.
pub fn enabled(idx: &crate::meta::IndexMeta) -> bool {
    idx.settings.get(SETTING).is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn unix_hour() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() / 3600)
}

impl Analytics {
    /// Whether any index has analytics on (one relaxed load).
    #[inline]
    pub fn on(&self) -> bool {
        self.any.load(Ordering::Relaxed)
    }

    /// Follows the cluster state: the indices with analytics on now, and the ones that had it
    /// and no longer do (turned off, deleted, or replaced by an index of the same name), whose
    /// waiting buckets go and which [`Analytics::take`] hands on as cleared. The first call (the
    /// cluster opening) only learns the set, and has it handed on once.
    pub fn sync(&self, st: &ClusterState) {
        let now: HashMap<String, String> = st.metadata.indices.values().filter(|i| enabled(i)).map(|i| (i.name.clone(), i.uuid.clone())).collect();
        let mut was = self.enabled.write();
        let mut p = self.pending.lock();
        if !self.synced.swap(true, Ordering::Relaxed) {
            p.announce = true;
        } else {
            for (n, u) in was.iter() {
                if now.get(n) != Some(u) {
                    p.drop_index(n);
                    if !p.cleared.contains(n) {
                        p.cleared.push(n.clone());
                    }
                }
            }
        }
        self.any.store(!now.is_empty(), Ordering::Relaxed);
        *was = now;
    }

    /// `expressions` when each names an index with analytics on (a search that names its index:
    /// no resolving needed), else `None`.
    pub fn named(&self, expressions: &[String]) -> Option<Vec<String>> {
        let on = self.enabled.read();
        (!expressions.is_empty() && expressions.iter().all(|e| on.contains_key(e.as_str()))).then(|| expressions.to_vec())
    }

    /// The names among `names` with analytics on.
    pub fn filter(&self, names: &[String]) -> Vec<String> {
        let on = self.enabled.read();
        names.iter().filter(|n| on.contains_key(n.as_str())).cloned().collect()
    }

    /// The indices with analytics on, sorted.
    pub fn enabled_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.enabled.read().keys().cloned().collect();
        v.sort_unstable();
        v
    }

    /// Whether index `name` has analytics on.
    pub fn is_on(&self, name: &str) -> bool {
        self.on() && self.enabled.read().contains_key(name)
    }

    /// Records a search of `indices` (each with analytics on) for `query` (normalized).
    pub fn record_search(&self, indices: &[String], query: &str, hits: u64, latency: std::time::Duration) {
        if indices.is_empty() {
            return;
        }
        let hour = unix_hour();
        let us = latency.as_micros().min(u64::MAX as u128) as u64;
        let mut p = self.pending.lock();
        for i in indices {
            let (b, bytes) = p.hour(i, hour);
            b.search(query, hits, us, bytes);
        }
    }

    /// Records a click on a result of a search of index `index` (with analytics on) for `query`
    /// (normalized), at 1-based `position` when known.
    pub fn record_click(&self, index: &str, query: &str, position: Option<u32>) {
        let hour = unix_hour();
        let mut p = self.pending.lock();
        let (b, bytes) = p.hour(index, hour);
        b.click(query, position, bytes);
    }

    /// Everything waiting, handed on once.
    pub fn take(&self) -> Taken {
        // `enabled` before `pending`, the order `sync` takes them in.
        let names = self.enabled_names();
        let mut p = self.pending.lock();
        let enabled = std::mem::replace(&mut p.announce, false).then_some(names);
        p.bytes = 0;
        let mut hours: Vec<(String, u64, Bucket)> = Vec::new();
        for (index, hs) in p.hours.drain() {
            for (h, b) in hs {
                if !b.is_empty() {
                    hours.push((index.clone(), h, b));
                }
            }
        }
        hours.sort_unstable_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        Taken { cleared: std::mem::take(&mut p.cleared), enabled, hours }
    }

    /// The buckets waiting, left in place (an API read between two passes).
    pub fn peek(&self) -> Vec<(String, u64, Bucket)> {
        let p = self.pending.lock();
        p.hours.iter().flat_map(|(i, hs)| hs.iter().map(move |(h, b)| (i.clone(), *h, b.clone()))).collect()
    }

    /// Bytes of query text waiting (memory accounting and tests).
    pub fn pending_bytes(&self) -> usize {
        self.pending.lock().bytes
    }
}

/// A query's text as analytics keep it: whitespace collapsed, lower-cased, at most
/// [`MAX_QUERY_CHARS`] characters. `None` when nothing is left.
pub fn normalize(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len().min(MAX_QUERY_CHARS * 4));
    let mut chars = 0;
    for word in text.split_whitespace() {
        if chars > 0 {
            if chars >= MAX_QUERY_CHARS {
                break;
            }
            out.push(' ');
            chars += 1;
        }
        for c in word.chars().flat_map(char::to_lowercase) {
            if chars >= MAX_QUERY_CHARS {
                break;
            }
            out.push(c);
            chars += 1;
        }
    }
    let t = out.trim_end();
    (!t.is_empty()).then(|| t.to_string())
}

/// The text a search was made with: the `X-Roll-Search-Query` header, else the `q` parameter,
/// else the user's text in the body ([`body_text`]), normalized.
pub fn query_text(header: Option<&str>, q: Option<&str>, body: Option<&Value>) -> Option<String> {
    if let Some(h) = header {
        return normalize(h);
    }
    if let Some(q) = q {
        return normalize(q);
    }
    body.and_then(body_text).and_then(normalize)
}

/// How deep the body's queries and retrievers are followed for text.
const MAX_DEPTH: usize = 12;

/// The user's text in a search body: the first of its query's full-text queries
/// (`query_string`, `simple_query_string`, `multi_match`, `match` and its phrase forms,
/// `combined_fields`, `semantic`, `sparse_vector`, a `knn` query's `query_vector_builder`),
/// through compound queries' scoring clauses (never a filter), then its retriever's, then its
/// `knn` section's.
pub fn body_text(body: &Value) -> Option<&str> {
    body.get("query")
        .and_then(|q| from_query(q, 0))
        .or_else(|| body.get("retriever").and_then(|r| from_retriever(r, 0)))
        .or_else(|| body.get("knn").and_then(from_knn))
}

/// The only key of an object and its value.
fn single(v: &Value) -> Option<(&str, &Value)> {
    let m = v.as_object()?;
    m.iter().next().map(|(k, v)| (k.as_str(), v))
}

fn each(v: &Value) -> Vec<&Value> {
    match v {
        Value::Arr(a) => a.iter().collect(),
        Value::Obj(_) => vec![v],
        _ => Vec::new(),
    }
}

fn str_of(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

fn builder_text(v: &Value) -> Option<&str> {
    str_of(v.get("query_vector_builder")?.get("text_embedding")?.get("model_text"))
}

fn from_knn(v: &Value) -> Option<&str> {
    each(v).into_iter().find_map(builder_text)
}

fn from_query(q: &Value, depth: usize) -> Option<&str> {
    if depth > MAX_DEPTH {
        return None;
    }
    let (name, b) = single(q)?;
    let d = depth + 1;
    match name {
        "query_string" | "simple_query_string" | "multi_match" | "combined_fields" | "semantic" | "sparse_vector" => str_of(b.get("query")),
        "match" | "match_phrase" | "match_phrase_prefix" | "match_bool_prefix" => {
            let (_, f) = single(b)?;
            match f {
                Value::Str(s) if !s.trim().is_empty() => Some(s.as_str()),
                _ => str_of(f.get("query")),
            }
        }
        // OpenSearch's neural query.
        "neural" => single(b).and_then(|(_, f)| str_of(f.get("query_text"))),
        "knn" => builder_text(b),
        "bool" => ["must", "should"].iter().filter_map(|k| b.get(k)).flat_map(each).find_map(|c| from_query(c, d)),
        "dis_max" | "hybrid" => b.get("queries").into_iter().flat_map(each).find_map(|c| from_query(c, d)),
        "function_score" | "script_score" | "nested" | "has_child" | "has_parent" => b.get("query").and_then(|c| from_query(c, d)),
        "boosting" => b.get("positive").and_then(|c| from_query(c, d)),
        "pinned" | "rule" => b.get("organic").and_then(|c| from_query(c, d)),
        _ => None,
    }
}

fn from_retriever(r: &Value, depth: usize) -> Option<&str> {
    if depth > MAX_DEPTH {
        return None;
    }
    let (name, b) = single(r)?;
    let d = depth + 1;
    match name {
        "standard" => b.get("query").and_then(|q| from_query(q, d)),
        "knn" => builder_text(b),
        // The simplified form's text, else the first child's.
        "rrf" | "linear" => str_of(b.get("query")).or_else(|| {
            b.get("retrievers").into_iter().flat_map(each).find_map(|c| {
                // `linear`'s children are `{ "retriever": …, "weight": … }`.
                c.get("retriever").map_or_else(|| from_retriever(c, d), |inner| from_retriever(inner, d))
            })
        }),
        "text_similarity_reranker" => str_of(b.get("inference_text")).or_else(|| b.get("retriever").and_then(|c| from_retriever(c, d))),
        "rule" | "pinned" | "rescorer" | "diversify" => b.get("retriever").and_then(|c| from_retriever(c, d)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Value {
        crate::json::parse(s.as_bytes()).unwrap().unwrap()
    }

    #[test]
    fn text_from_requests() {
        let t = |s: &str| query_text(None, None, Some(&parse(s)));
        assert_eq!(t(r#"{"query":{"match":{"title":"Red  Shoes"}}}"#).as_deref(), Some("red shoes"));
        assert_eq!(t(r#"{"query":{"match":{"title":{"query":"boots","operator":"and"}}}}"#).as_deref(), Some("boots"));
        assert_eq!(t(r#"{"query":{"bool":{"filter":[{"term":{"k":"x"}}],"must":{"multi_match":{"query":"laptop","fields":["a"]}}}}}"#).as_deref(), Some("laptop"));
        assert_eq!(t(r#"{"query":{"bool":{"filter":{"match":{"brand":"acme"}}}}}"#), None, "filters aren't the user's text");
        assert_eq!(t(r#"{"query":{"term":{"k":"x"}}}"#), None);
        assert_eq!(t(r#"{"query":{"function_score":{"query":{"query_string":{"query":"tv"}}}}}"#).as_deref(), Some("tv"));
        assert_eq!(t(r#"{"query":{"semantic":{"field":"f","query":"Cheap flights"}}}"#).as_deref(), Some("cheap flights"));
        assert_eq!(t(r#"{"knn":{"field":"v","k":3,"query_vector_builder":{"text_embedding":{"model_id":"m","model_text":"Hello"}}}}"#).as_deref(), Some("hello"));
        assert_eq!(t(r#"{"retriever":{"rrf":{"retrievers":[{"standard":{"query":{"match":{"t":"sofa"}}}},{"knn":{"field":"v"}}]}}}"#).as_deref(), Some("sofa"));
        assert_eq!(t(r#"{"retriever":{"text_similarity_reranker":{"inference_text":"why","retriever":{"standard":{"query":{"match":{"t":"x"}}}}}}}"#).as_deref(), Some("why"));
        assert_eq!(t(r#"{"retriever":{"linear":{"retrievers":[{"retriever":{"standard":{"query":{"match":{"t":"lamp"}}}},"weight":2}]}}}"#).as_deref(), Some("lamp"));
        assert_eq!(query_text(Some("  From Header "), Some("q"), Some(&parse(r#"{"query":{"match":{"t":"body"}}}"#))).as_deref(), Some("from header"));
        assert_eq!(query_text(None, Some("title:shoes"), None).as_deref(), Some("title:shoes"));
        let long = "é".repeat(500);
        assert_eq!(normalize(&long).unwrap().chars().count(), MAX_QUERY_CHARS);
        assert_eq!(normalize("   "), None);
    }

    #[test]
    fn heavy_hitters_are_bounded_and_keep_the_frequent() {
        let mut b = Bucket::new(8, 4);
        let mut bytes = 0;
        for i in 0..10_000u64 {
            // "hot" is every 5th search (over 1/8 of them, so always kept); the rest all differ.
            if i % 5 == 0 {
                b.search("hot", 3, 100, &mut bytes);
            } else {
                b.search(&format!("rare {i}"), 0, 100, &mut bytes);
            }
        }
        assert_eq!((b.searches, b.no_hits), (10_000, 8_000));
        assert_eq!(bytes, b.text_bytes(), "the byte count follows evictions");
        assert!(b.top.len() <= 8 && b.empty.len() <= 4);
        let hot = b.top.get("hot").expect("a frequent query is kept");
        assert!(hot.searches >= 2_000 && hot.no_hits == 0, "{hot:?}");
        assert_eq!(b.top.ranked()[0].0, "hot");
        let round = Bucket::decode(&b.encode(), 8, 4);
        assert_eq!((round.searches, round.no_hits, round.top.get("hot")), (b.searches, b.no_hits, b.top.get("hot")));
    }

    #[test]
    fn clicks_and_merges() {
        let n = &mut 0;
        let mut a = Bucket::new(4, 4);
        a.search("shoes", 10, 1000, n);
        a.click("shoes", Some(2), n);
        a.click("shoes", None, n);
        let mut b = Bucket::new(4, 4);
        b.search("shoes", 8, 3000, n);
        b.click("shoes", Some(4), n);
        a.merge(&b);
        let s = a.top.get("shoes").unwrap();
        assert_eq!((s.searches, s.hits, s.latency_us, s.clicks, s.position_sum, s.positioned), (2, 18, 4000, 3, 6, 2));
        assert_eq!(a.clicks, 3);
    }

    /// What recording adds to a cheap `term` search (`cargo test --release -p search-es
    /// analytics::tests::record_cost -- --ignored --nocapture`): the same search on a 10,000
    /// document index through the REST handler, with analytics off and with them on and the
    /// search recorded (its text from `X-Roll-Search-Query`), in alternating rounds. The figures
    /// are in docs/search/README.md, "Search analytics".
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn record_cost() {
        let c = crate::Cluster::in_memory("cost").await;
        let call = |m: &str, uri: &str, body: String| crate::rest::handle(&c, crate::EsRequest::json(m, uri, body));
        assert!(call("PUT", "/docs", r#"{"mappings":{"properties":{"k":{"type":"keyword"},"t":{"type":"text"}}}}"#.into()).await.status < 300);
        for chunk in 0..10 {
            let mut bulk = String::new();
            for i in 0..1000 {
                let n = chunk * 1000 + i;
                bulk.push_str(&format!("{{\"index\":{{\"_id\":\"{n}\"}}}}\n{{\"k\":\"k{}\",\"t\":\"word{n} common\"}}\n", n % 100));
            }
            let mut req = crate::EsRequest::json("POST", "/docs/_bulk", bulk);
            req.headers = vec![("content-type".into(), "application/x-ndjson".into())];
            assert!(crate::rest::handle(&c, req).await.status < 300);
        }
        assert!(call("POST", "/docs/_refresh", String::new()).await.status < 300);
        let search = || {
            let mut req = crate::EsRequest::json("POST", "/docs/_search", r#"{"query":{"term":{"k":"k7"}},"size":10}"#);
            req.headers.push((QUERY_HEADER.into(), "running shoes".into()));
            crate::rest::handle(&c, req)
        };
        let n = 5_000u32;
        let round = async |on: bool| {
            let body = format!(r#"{{"index.roll.analytics.enabled":{}}}"#, if on { "true" } else { "null" });
            assert!(call("PUT", "/docs/_settings", body).await.status < 300);
            assert_eq!(c.analytics.on(), on);
            for _ in 0..2_000 {
                search().await;
            }
            let start = crate::test_support::thread_cpu();
            for _ in 0..n {
                assert_eq!(search().await.status, 200);
            }
            (crate::test_support::thread_cpu() - start).as_nanos() as f64 / f64::from(n)
        };
        // Alternating rounds, so load from other tenants falls on both sides alike; medians.
        let (mut off, mut on) = (Vec::new(), Vec::new());
        for _ in 0..12 {
            off.push(round(false).await);
            on.push(round(true).await);
        }
        let median = |v: &[f64]| {
            let mut v = v.to_vec();
            v.sort_by(f64::total_cmp);
            (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0
        };
        let (m_off, m_on) = (median(&off), median(&on));
        println!("record_cost: off {off:.0?} ns\nrecord_cost: on  {on:.0?} ns\nrecord_cost: median {m_off:.0} vs {m_on:.0} ns per search: {:+.2}%", (m_on / m_off - 1.0) * 100.0);
        let taken = c.analytics.take();
        assert!(taken.hours.iter().any(|(_, _, b)| b.top.get("running shoes").is_some()), "the searches were recorded");
        // The recording path alone, as a search takes it: the text, the clock, the indices, the
        // record.
        let a = &c.analytics;
        let names = vec!["docs".to_string()];
        assert!(call("PUT", "/docs/_settings", r#"{"index.roll.analytics.enabled":true}"#.into()).await.status < 300);
        let reps = 1_000_000u32;
        let start = crate::test_support::thread_cpu();
        for _ in 0..reps {
            let text = query_text(Some(std::hint::black_box("running shoes")), None, None).unwrap();
            let from = std::time::Instant::now();
            let on = a.named(std::hint::black_box(&names)).unwrap_or_default();
            a.record_search(&on, &text, 3, from.elapsed());
        }
        let alone = (crate::test_support::thread_cpu() - start).as_nanos() as f64 / f64::from(reps);
        println!("record_cost: recording alone {alone:.0} ns per search, {:.2}% of the median search", alone / m_off * 100.0);
    }

    #[test]
    fn taken_clears_drop_older_hours() {
        let mut t = Taken { hours: vec![("a".into(), 1, Bucket::new(1, 1)), ("b".into(), 1, Bucket::new(1, 1))], ..Default::default() };
        t.extend(Taken { cleared: vec!["a".into()], hours: vec![("a".into(), 2, Bucket::new(1, 1))], ..Default::default() });
        assert_eq!(t.hours.iter().map(|(i, h, _)| (i.as_str(), *h)).collect::<Vec<_>>(), [("b", 1), ("a", 2)]);
        assert_eq!(t.cleared, ["a"]);
    }
}
