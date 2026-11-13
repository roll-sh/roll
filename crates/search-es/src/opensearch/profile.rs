//! OpenSearch 3.9's `profile: true` output, built from the same timings the elasticsearch
//! dialect reports.
//!
//! The two products forked before Elasticsearch reworked its query phase, so a profile differs
//! in shape, not only in numbers:
//!
//! - **Concurrent segment search.** OpenSearch 3 searches a shard's segments in slices when
//!   the request has aggregations or a k-NN query (`search.concurrent_segment_search.mode:
//!   auto`; the k-NN plugin's decider votes for the latter), unless an
//!   aggregation can't run that way (`sampler`, `diversified_sampler`, `parent`), the request
//!   has `terminate_after`, or it sorts on `@timestamp` first ([`concurrent`]). A concurrent
//!   search reports collector *managers* (`QueryCollectorManager`, `NonGlobalAggCollectorManager`,
//!   ...) with reduce and per-slice times, adds `max_`/`min_`/`avg_` statistics to every query
//!   and aggregation breakdown, and merges the per-slice aggregation trees through a `HashMap`
//!   (`ConcurrentAggregationProfiler`): aggregations come in that map's iteration order.
//! - **Sequential collectors** are Lucene's and OpenSearch's own (`TopScoreDocCollector`,
//!   `SimpleFieldCollector`, `EarlyTerminatingCollector`, `MultiCollector`...), wrapped in the
//!   order `QueryPhase` chains them, not Elasticsearch's single `QueryPhaseCollector`.
//! - **Breakdowns** come from a `TreeMap` (keys sorted) without Elasticsearch's `count_weight`;
//!   the concurrent aggregation breakdown is a `HashMap` again.
//! - **Global aggregations** run as a second search with its own `searches` entry.
//!
//! Lucene's `IndexSearcher.rewrite(query, needsScores)` wraps a query that needs no scores in
//! a `ConstantScoreQuery` — OpenSearch goes through it, Elasticsearch doesn't — which the
//! query phase reproduces by wrapping a profiled query that needs no scores.
//!
//! Timings are roll's: only their structure is OpenSearch's. A slice's share of a time is the
//! time divided by the slice count (one slice reports the time itself).

use crate::json::{Map, Value};
use crate::meta::{ClusterState, IndexMeta};
use crate::obj;
use crate::search_source::SearchSource;

/// Aggregation types whose factories don't support concurrent segment search
/// (`AggregatorFactory.supportsConcurrentSegmentSearch` is false for them in 3.9.0).
const SEQUENTIAL_AGGS: &[&str] = &["sampler", "diversified_sampler", "parent"];

/// The query timing types of a concurrent breakdown's statistics (`QueryTimingType` without
/// `create_weight`, which runs once per shard).
const SLICE_QUERY_TIMINGS: &[&str] = &["advance", "build_scorer", "compute_max_score", "match", "next_doc", "score", "set_min_competitive_score", "shallow_advance"];

/// `AggregationTimingType`, in declaration order.
const AGG_TIMINGS: &[&str] = &["initialize", "build_leaf_collector", "collect", "post_collection", "build_aggregation", "reduce"];

/// The resolved `search.concurrent_segment_search.mode` of an index
/// (`DefaultSearchContext.evaluateConcurrentSearchMode`): the index's mode setting, else the
/// cluster's, else the legacy boolean `enabled` settings (true = `all`), else `auto`.
fn mode(st: &ClusterState, idx: &IndexMeta) -> String {
    let m = &st.metadata;
    if let Some(v) = idx.settings.get("index.search.concurrent_segment_search.mode").or_else(|| m.cluster_setting("search.concurrent_segment_search.mode")) {
        return v.to_ascii_lowercase();
    }
    match idx.settings.get("index.search.concurrent_segment_search.enabled").or_else(|| m.cluster_setting("search.concurrent_segment_search.enabled")) {
        Some(v) if v.eq_ignore_ascii_case("true") => "all".into(),
        Some(_) => "none".into(),
        None => "auto".into(),
    }
}

/// Whether every aggregation of a request (sub-aggregations too) supports concurrent search.
fn aggs_support_concurrent(aggs: &Map) -> bool {
    aggs.values().all(|def| {
        let Value::Obj(def) = def else { return true };
        let ty = def.keys().find(|k| !matches!(k.as_str(), "aggs" | "aggregations" | "meta"));
        if ty.is_some_and(|t| SEQUENTIAL_AGGS.contains(&t.as_str())) {
            return false;
        }
        def.get("aggs").or_else(|| def.get("aggregations")).and_then(Value::as_object).is_none_or(aggs_support_concurrent)
    })
}

/// Whether OpenSearch searches a shard of `idx` concurrently for `src`
/// (`DefaultSearchContext.evaluateRequestShouldUseConcurrentSearch` with no plugin deciders).
pub fn concurrent(st: &ClusterState, idx: &IndexMeta, src: &SearchSource) -> bool {
    if idx.system {
        return false;
    }
    let mode = mode(st, idx);
    if mode != "auto" && mode != "all" {
        return false;
    }
    let first_sort_on_timestamp = matches!(src.sort.first(), Some(crate::search_source::Sort::Field { field, .. }) if field == "@timestamp");
    // An empty `aggs` object builds no factories: the request counts as one without them.
    let aggs = src.aggs.as_ref().filter(|a| !a.is_empty());
    if first_sort_on_timestamp || aggs.is_some_and(|a| !aggs_support_concurrent(a)) || src.terminate_after.is_some_and(|t| t != 0) {
        return false;
    }
    // The k-NN plugin's `ConcurrentSearchRequestDecider` votes for k-NN queries of k-NN indices.
    let knn = idx.mapping_ctx().knn && (src.os_knn || src.query.as_ref().is_some_and(knn_decides));
    mode == "all" || aggs.is_some() || knn
}

/// `KNNConcurrentSearchRequestDecider`: the decider sees every query of the tree in
/// `QueryBuilder.visit` order and keeps only its verdict on the last one, so the request
/// counts as a k-NN one when that last query is a `knn` query (a `knn` with a filter ends on
/// the filter's queries).
pub fn knn_decides(q: &crate::query::Query) -> bool {
    use crate::query::Query as Q;
    fn last(q: &Q) -> &Q {
        match q {
            Q::Bool(b) => [&b.must, &b.should, &b.must_not, &b.filter].into_iter().rev().find_map(|c| c.last()).map_or(q, last),
            Q::DisMax(d) => d.queries.last().map_or(q, last),
            Q::ConstantScore(c) => last(&c.filter),
            Q::FunctionScore(f) => last(&f.query),
            Q::Nested(n) => last(&n.query),
            // `BoostingQueryBuilder.visit` accepts its queries without descending.
            Q::Boosting(b) => &b.negative,
            Q::OsHybrid(h) => h.queries.last().map_or(q, last),
            Q::OsKnn(k) => k.filter.as_ref().map_or(q, last),
            _ => q,
        }
    }
    matches!(last(q), Q::OsKnn(_))
}

/// How many slices OpenSearch cuts a shard's segments into: `search.concurrent.max_slice_count`
/// (index, then cluster; default `max(1, min(processors / 2, 4))`) whole segments at most, or
/// Lucene's own slicing when the setting is 0. A shard without segments still reports one.
pub fn slice_count(st: &ClusterState, idx: &IndexMeta, processors: usize, max_docs: &[u32]) -> usize {
    let target = idx
        .settings
        .get("index.search.concurrent.max_slice_count")
        .or_else(|| st.metadata.cluster_setting("search.concurrent.max_slice_count"))
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or_else(|| (processors / 2).clamp(1, 4) as i64);
    let n = if target <= 0 { lucene_slices(max_docs) } else { max_docs.len().min(target as usize) };
    n.max(1)
}

/// `IndexSearcher.slices(leaves, 250_000, 5, false)`: segments largest first, each slice
/// filled until it holds 5 segments or more than 250,000 documents; a bigger segment is a
/// slice of its own.
fn lucene_slices(max_docs: &[u32]) -> usize {
    const MAX_DOCS: u64 = 250_000;
    const MAX_SEGMENTS: usize = 5;
    let mut sorted: Vec<u32> = max_docs.to_vec();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    let (mut slices, mut open, mut docs) = (0usize, 0usize, 0u64);
    for d in sorted {
        if u64::from(d) > MAX_DOCS {
            slices += 1;
            continue;
        }
        if open == 0 {
            slices += 1;
        }
        open += 1;
        docs += u64::from(d);
        if open >= MAX_SEGMENTS || docs > MAX_DOCS {
            open = 0;
            docs = 0;
        }
    }
    slices
}

/// Whether OpenSearch knows a shard's hit count without collecting
/// (`TopDocsCollectorContext.shortcutTotalHitCount`, a narrower rule than Lucene's
/// `Weight.count`): after constant-score, boost and approximation wrappers, a `match_all`,
/// or a term or field-exists query on a shard without deletions.
pub fn count_shortcut(searcher: &search_index::search::Searcher, q: &search_index::QueryRef) -> bool {
    use search_index::query::{BoostQuery, ConstantScoreQuery, FieldExistsQuery, MatchAllQuery, TermQuery};
    // Only a match-all, term or field-exists leaf under wrappers and one-clause booleans can
    // rewrite to a countable query: anything else is decided without rewriting (a rewrite
    // can be costly, e.g. expanding a multi-term query).
    fn may_shortcut(q: &search_index::QueryRef) -> bool {
        use search_index::query::{BooleanQuery, Occur};
        let any = q.as_any();
        if let Some(c) = any.downcast_ref::<ConstantScoreQuery>() {
            return may_shortcut(&c.query);
        }
        if let Some(b) = any.downcast_ref::<BoostQuery>() {
            return may_shortcut(&b.query);
        }
        if let Some(b) = any.downcast_ref::<BooleanQuery>() {
            return b.clauses.len() == 1 && b.clauses[0].occur != Occur::MustNot && may_shortcut(&b.clauses[0].query);
        }
        any.is::<MatchAllQuery>() || any.is::<TermQuery>() || any.is::<FieldExistsQuery>()
    }
    if !may_shortcut(q) {
        return false;
    }
    // Deciding must not show up in a profile.
    let unprofiled;
    let searcher = if searcher.profiler().is_some() {
        unprofiled = searcher.with_profiler(None);
        &unprofiled
    } else {
        searcher
    };
    let Ok(mut q) = searcher.rewrite(q) else { return false };
    loop {
        // `ApproximateScoreQuery::as_any` already answers with the query it wraps.
        let any = q.as_any();
        if let Some(c) = any.downcast_ref::<ConstantScoreQuery>() {
            q = c.query.clone();
        } else if let Some(b) = any.downcast_ref::<BoostQuery>() {
            q = b.query.clone();
        } else {
            break;
        }
    }
    let any = q.as_any();
    if any.is::<MatchAllQuery>() {
        return true;
    }
    (any.is::<TermQuery>() || any.is::<FieldExistsQuery>()) && !searcher.segments().iter().any(|s| s.has_deletions()) && searcher.count_if_cheap(&q).ok().flatten().is_some()
}

/// What a shard's query phase chained, as OpenSearch names it.
#[derive(Debug, Default)]
pub struct Phase {
    pub concurrent: bool,
    pub slices: usize,
    /// Hits requested (0: only counting).
    pub size: usize,
    pub by_score: bool,
    /// A field sort with `search_after` (`PagingFieldCollector`).
    pub paging: bool,
    /// `size: 0` counting every hit (`track_total_hits: true` without a count shortcut):
    /// `TotalHitCountCollector` instead of `EarlyTerminatingCollector`.
    pub exact_count: bool,
    pub post_filter: bool,
    pub min_score: bool,
    pub terminate_after: bool,
    /// Top-level aggregations collected with the query, in request order.
    pub aggs: Vec<String>,
    /// Top-level `global` aggregations (their own search), in request order.
    pub globals: Vec<String>,
}

/// An aggregation collector's name suffix: `[a]` for one, `[[a, b]]` (a multi-bucket
/// collector's `toString`) for several.
fn agg_list(names: &[String]) -> String {
    if names.len() == 1 { format!("[{}]", names[0]) } else { format!("[[{}]]", names.join(", ")) }
}

impl Phase {
    fn node(&self, name: String, reason: &str, nanos: i64, children: Vec<Value>) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), Value::Str(name));
        m.insert("reason".into(), Value::Str(reason.into()));
        m.insert("time_in_nanos".into(), Value::from(nanos));
        if self.concurrent {
            let slices = self.slices.max(1) as i64;
            m.insert("reduce_time_in_nanos".into(), Value::from(nanos / 4));
            m.insert("max_slice_time_in_nanos".into(), Value::from(nanos));
            m.insert("min_slice_time_in_nanos".into(), Value::from(nanos / slices));
            m.insert("avg_slice_time_in_nanos".into(), Value::from(nanos / slices));
            m.insert("slice_count".into(), Value::from(slices));
        }
        if !children.is_empty() {
            m.insert("children".into(), Value::Arr(children));
        }
        Value::Obj(m)
    }

    /// The `collector` tree of the main search (`QueryPhase` / `QueryCollectorContext`):
    /// top docs or count, then terminate_after, post_filter, aggregations and min_score
    /// wrapped around it in that order.
    pub fn collector(&self, nanos: i64) -> Value {
        let nanos = nanos.max(1);
        let c = self.concurrent;
        let (top, reason) = match (self.size, c) {
            (0, false) => (if self.exact_count { "TotalHitCountCollector" } else { "EarlyTerminatingCollector" }, "search_count"),
            (0, true) => (if self.exact_count { "TotalHitCountCollectorManager" } else { "EarlyTerminatingCollectorManager" }, "search_count"),
            (_, true) => ("SimpleTopDocsCollectorManager", "search_top_hits"),
            (_, false) if self.by_score => ("TopScoreDocCollector", "search_top_hits"),
            (_, false) if self.paging => ("PagingFieldCollector", "search_top_hits"),
            (_, false) => ("SimpleFieldCollector", "search_top_hits"),
        };
        let mut cur = self.node(top.into(), reason, nanos, Vec::new());
        if self.terminate_after {
            cur = self.node("MultiCollector".into(), "search_terminate_after_count", nanos, vec![cur]);
        }
        if self.post_filter {
            cur = self.node(if c { "FilteredCollectorManager" } else { "FilteredCollector" }.into(), "search_post_filter", nanos, vec![cur]);
        }
        if !self.aggs.is_empty() {
            let (multi, agg) = if c {
                ("QueryCollectorManager", format!("NonGlobalAggCollectorManager: {}", agg_list(&self.aggs)))
            } else if self.aggs.len() == 1 {
                ("MultiCollector", format!("ProfilingAggregator: [{}]", self.aggs[0]))
            } else {
                ("MultiCollector", format!("MultiBucketCollector: {}", agg_list(&self.aggs)))
            };
            let agg = self.node(agg, "aggregation", nanos, Vec::new());
            cur = self.node(multi.into(), "search_multi", nanos, vec![cur, agg]);
        }
        if self.min_score {
            cur = self.node(if c { "MinimumCollectorManager" } else { "MinimumScoreCollector" }.into(), "search_min_score", nanos, vec![cur]);
        }
        cur
    }

    /// The collector of the `global` aggregations' search, if the request has any.
    pub fn global_collector(&self, nanos: i64) -> Option<Value> {
        if self.globals.is_empty() {
            return None;
        }
        let name = if self.concurrent {
            format!("GlobalAggCollectorManager: {}", agg_list(&self.globals))
        } else if self.globals.len() == 1 {
            format!("ProfilingAggregator: [{}]", self.globals[0])
        } else {
            format!("MultiBucketCollector: {}", agg_list(&self.globals))
        };
        Some(self.node(name, "aggregation_global", nanos.max(1), Vec::new()))
    }

    /// The shard's `searches`: the main search (its query trees: the query, then the post
    /// filter) and, with `global` aggregations, their search (the last query tree).
    pub fn searches(&self, mut queries: Vec<Value>, rewrite: i64, nanos: i64) -> Vec<Value> {
        let global_tree = if self.globals.is_empty() || queries.len() < 2 { None } else { queries.pop() };
        let queries: Vec<Value> = queries.into_iter().map(|q| self.query_node(q)).collect();
        let mut out = vec![obj! { "query" => Value::Arr(queries), "rewrite_time" => rewrite, "collector" => Value::Arr(vec![self.collector(nanos)]) }];
        if let Some(g) = self.global_collector(nanos) {
            let tree: Vec<Value> = global_tree.into_iter().map(|q| self.query_node(q)).collect();
            out.push(obj! { "query" => Value::Arr(tree), "rewrite_time" => 0i64, "collector" => Value::Arr(vec![g]) });
        }
        out
    }

    /// One query tree node (an Elasticsearch-shaped `ProfileResult`) as OpenSearch prints
    /// it: sorted breakdown keys without `count_weight`, plus the slice statistics of a
    /// concurrent search.
    pub fn query_node(&self, q: Value) -> Value {
        let Value::Obj(mut m) = q else { return q };
        let time = m.get("time_in_nanos").and_then(Value::as_i64).unwrap_or(0);
        let slices = self.slices.max(1) as i64;
        let mut out = Map::new();
        for k in ["type", "description", "time_in_nanos"] {
            if let Some(v) = m.shift_remove(k) {
                out.insert(k.into(), v);
            }
        }
        if self.concurrent {
            out.insert("max_slice_time_in_nanos".into(), Value::from(time));
            out.insert("min_slice_time_in_nanos".into(), Value::from(time / slices));
            out.insert("avg_slice_time_in_nanos".into(), Value::from(time / slices));
        }
        let mut bd: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
        if let Some(Value::Obj(b)) = m.shift_remove("breakdown") {
            for (k, v) in b {
                if k == "count_weight" || k == "count_weight_count" {
                    continue;
                }
                bd.insert(k, v);
            }
        }
        if self.concurrent {
            for t in SLICE_QUERY_TIMINGS {
                for key in [t.to_string(), format!("{t}_count")] {
                    let v = bd.get(&key).and_then(Value::as_i64).unwrap_or(0);
                    bd.insert(format!("max_{key}"), Value::from(v));
                    bd.insert(format!("min_{key}"), Value::from(v / slices));
                    bd.insert(format!("avg_{key}"), Value::from(v / slices));
                }
            }
        }
        out.insert("breakdown".into(), Value::Obj(bd.into_iter().collect()));
        if let Some(Value::Arr(children)) = m.shift_remove("children") {
            out.insert("children".into(), Value::Arr(children.into_iter().map(|c| self.query_node(c)).collect()));
        }
        Value::Obj(out)
    }

    /// The shard's aggregation trees: a sequential search keeps request order and sorts
    /// each breakdown; a concurrent one adds slice statistics and orders every level (and each
    /// breakdown) the way `ConcurrentAggregationProfiler`'s `HashMap`s iterate.
    pub fn aggregations(&self, aggs: Vec<Value>) -> Vec<Value> {
        let mut aggs: Vec<Value> = aggs.into_iter().map(|a| self.agg_node(a)).collect();
        if self.concurrent {
            // `getSliceLevelAggregationMap`: `computeIfAbsent` into a `new HashMap<>()`.
            let names: Vec<&str> = aggs.iter().map(|a| a.get("description").and_then(Value::as_str).unwrap_or("")).collect();
            let calls: Vec<usize> = (0..names.len()).collect();
            let order = search_aggs::internal::java_default_hash_map_order(&names, &calls);
            let mut slots: Vec<Option<Value>> = aggs.into_iter().map(Some).collect();
            aggs = order.into_iter().filter_map(|i| slots[i].take()).collect();
        }
        aggs
    }

    fn agg_node(&self, a: Value) -> Value {
        let Value::Obj(mut m) = a else { return a };
        let time = m.get("time_in_nanos").and_then(Value::as_i64).unwrap_or(0);
        let slices = self.slices.max(1) as i64;
        let mut out = Map::new();
        for k in ["type", "description", "time_in_nanos"] {
            if let Some(v) = m.shift_remove(k) {
                out.insert(k.into(), v);
            }
        }
        let b = match m.shift_remove("breakdown") {
            Some(Value::Obj(b)) => b,
            _ => Map::new(),
        };
        let get = |k: &str| b.get(k).and_then(Value::as_i64).unwrap_or(0);
        if self.concurrent {
            out.insert("max_slice_time_in_nanos".into(), Value::from(time));
            out.insert("min_slice_time_in_nanos".into(), Value::from(time / slices));
            out.insert("avg_slice_time_in_nanos".into(), Value::from(time / slices));
            // The keys in the order `reduceProfileResultsTree` puts them into its `HashMap`.
            let mut keys: Vec<(String, i64)> = Vec::with_capacity(36);
            for t in AGG_TIMINGS {
                keys.push((format!("{t}_count"), get(&format!("{t}_count"))));
            }
            for t in AGG_TIMINGS {
                let v = get(t);
                keys.push((format!("max_{t}"), v));
                keys.push((format!("min_{t}"), v / slices));
                keys.push((format!("avg_{t}"), v / slices));
            }
            for t in AGG_TIMINGS {
                keys.push((t.to_string(), get(t)));
            }
            for t in ["build_leaf_collector_count", "collect_count"] {
                let v = get(t);
                keys.push((format!("max_{t}"), v));
                keys.push((format!("min_{t}"), v / slices));
                keys.push((format!("avg_{t}"), v / slices));
            }
            let names: Vec<&str> = keys.iter().map(|(k, _)| k.as_str()).collect();
            let order = crate::json::java_hash_order(&names, 16);
            out.insert("breakdown".into(), Value::Obj(order.into_iter().map(|i| (keys[i].0.clone(), Value::from(keys[i].1))).collect()));
        } else {
            let sorted: std::collections::BTreeMap<String, Value> = b.into_iter().collect();
            out.insert("breakdown".into(), Value::Obj(sorted.into_iter().collect()));
        }
        if let Some(d) = m.shift_remove("debug") {
            out.insert("debug".into(), d);
        }
        if let Some(Value::Arr(children)) = m.shift_remove("children") {
            out.insert("children".into(), Value::Arr(self.aggregations(children)));
        }
        Value::Obj(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at<'a>(v: &'a Value, path: &str) -> &'a Value {
        path.split('.').fold(v, |v, k| match k.parse::<usize>() {
            Ok(i) => &v.as_array().expect("array")[i],
            Err(_) => v.get(k).unwrap_or_else(|| panic!("no {k}")),
        })
    }

    fn keys(v: &Value) -> Vec<String> {
        v.as_object().unwrap().keys().cloned().collect()
    }

    #[test]
    fn lucene_slicing() {
        assert_eq!(lucene_slices(&[]), 0);
        assert_eq!(lucene_slices(&[10, 10, 10]), 1);
        assert_eq!(lucene_slices(&[1; 5]), 1);
        assert_eq!(lucene_slices(&[1; 6]), 2);
        assert_eq!(lucene_slices(&[300_000, 10]), 2);
        // 200k + 100k passes 250k: the slice closes after the segment that crossed.
        assert_eq!(lucene_slices(&[200_000, 100_000, 10]), 2);
    }

    #[test]
    fn sequential_collectors_chain_like_the_query_phase() {
        // Captured from OpenSearch 3.9.0: `min_score` + `post_filter` on a `size: 10` search.
        let p = Phase { size: 10, by_score: true, post_filter: true, min_score: true, ..Default::default() };
        let c = p.collector(5);
        assert_eq!(at(&c, "name").as_str(), Some("MinimumScoreCollector"));
        assert_eq!(at(&c, "reason").as_str(), Some("search_min_score"));
        assert_eq!(at(&c, "children.0.name").as_str(), Some("FilteredCollector"));
        assert_eq!(at(&c, "children.0.children.0.name").as_str(), Some("TopScoreDocCollector"));
        assert_eq!(keys(at(&c, "children.0.children.0")), vec!["name", "reason", "time_in_nanos"]);
        // `terminate_after` with aggregations (sequential).
        let p = Phase { size: 10, by_score: true, terminate_after: true, aggs: vec!["t".into()], ..Default::default() };
        let c = p.collector(5);
        assert_eq!(at(&c, "name").as_str(), Some("MultiCollector"));
        assert_eq!(at(&c, "reason").as_str(), Some("search_multi"));
        assert_eq!(at(&c, "children.0.reason").as_str(), Some("search_terminate_after_count"));
        assert_eq!(at(&c, "children.0.children.0.name").as_str(), Some("TopScoreDocCollector"));
        assert_eq!(at(&c, "children.1.name").as_str(), Some("ProfilingAggregator: [t]"));
        let p = Phase { aggs: vec!["s".into(), "b".into()], ..Default::default() };
        assert_eq!(at(&p.collector(5), "children.1.name").as_str(), Some("MultiBucketCollector: [[s, b]]"));
        assert_eq!(at(&p.collector(5), "children.0.name").as_str(), Some("EarlyTerminatingCollector"));
        let p = Phase { size: 3, paging: true, ..Default::default() };
        assert_eq!(at(&p.collector(5), "name").as_str(), Some("PagingFieldCollector"));
        let p = Phase { size: 3, ..Default::default() };
        assert_eq!(at(&p.collector(5), "name").as_str(), Some("SimpleFieldCollector"));
        let p = Phase { exact_count: true, ..Default::default() };
        assert_eq!(at(&p.collector(5), "name").as_str(), Some("TotalHitCountCollector"));
    }

    #[test]
    fn concurrent_collectors_are_managers_with_slice_times() {
        let p = Phase { concurrent: true, slices: 1, min_score: true, aggs: vec!["t".into()], ..Default::default() };
        let c = p.collector(8);
        assert_eq!(at(&c, "name").as_str(), Some("MinimumCollectorManager"));
        assert_eq!(keys(&c), vec!["name", "reason", "time_in_nanos", "reduce_time_in_nanos", "max_slice_time_in_nanos", "min_slice_time_in_nanos", "avg_slice_time_in_nanos", "slice_count", "children"]);
        let multi = at(&c, "children.0");
        assert_eq!(at(multi, "name").as_str(), Some("QueryCollectorManager"));
        assert_eq!(at(multi, "children.0.name").as_str(), Some("EarlyTerminatingCollectorManager"));
        assert_eq!(at(multi, "children.1.name").as_str(), Some("NonGlobalAggCollectorManager: [t]"));
        let p = Phase { concurrent: true, slices: 1, size: 10, post_filter: true, aggs: vec!["zz".into(), "aa".into()], globals: vec!["g".into()], ..Default::default() };
        let c = p.collector(8);
        assert_eq!(at(&c, "children.0.name").as_str(), Some("FilteredCollectorManager"));
        assert_eq!(at(&c, "children.0.children.0.name").as_str(), Some("SimpleTopDocsCollectorManager"));
        assert_eq!(at(&c, "children.1.name").as_str(), Some("NonGlobalAggCollectorManager: [[zz, aa]]"));
        assert_eq!(at(&p.global_collector(8).unwrap(), "name").as_str(), Some("GlobalAggCollectorManager: [g]"));
        assert_eq!(at(&p.global_collector(8).unwrap(), "reason").as_str(), Some("aggregation_global"));
    }

    #[test]
    fn query_breakdowns_sort_and_add_slice_statistics() {
        let node = obj! {
            "type" => "TermQuery", "description" => "k:a", "time_in_nanos" => 10i64,
            "breakdown" => obj! { "create_weight" => 1i64, "create_weight_count" => 1i64, "next_doc" => 4i64, "next_doc_count" => 2i64, "advance" => 0i64, "advance_count" => 0i64, "count_weight" => 0i64, "count_weight_count" => 0i64 },
        };
        let seq = Phase::default().query_node(node.clone());
        assert_eq!(keys(&seq), vec!["type", "description", "time_in_nanos", "breakdown"]);
        assert_eq!(keys(at(&seq, "breakdown")), vec!["advance", "advance_count", "create_weight", "create_weight_count", "next_doc", "next_doc_count"]);
        let con = Phase { concurrent: true, slices: 1, ..Default::default() }.query_node(node);
        assert_eq!(keys(&con), vec!["type", "description", "time_in_nanos", "max_slice_time_in_nanos", "min_slice_time_in_nanos", "avg_slice_time_in_nanos", "breakdown"]);
        let b = keys(at(&con, "breakdown"));
        // OpenSearch's 66 keys: 18 base (no count_weight) plus max/min/avg of 8 timings and counts.
        assert_eq!(b.len(), 6 + 48);
        assert_eq!(&b[..6], ["advance", "advance_count", "avg_advance", "avg_advance_count", "avg_build_scorer", "avg_build_scorer_count"]);
        assert_eq!(at(&con, "breakdown.max_next_doc_count").as_i64(), Some(2));
    }

    #[test]
    fn concurrent_aggregation_breakdown_iterates_like_the_hashmap() {
        // The key order OpenSearch 3.9.0 printed for a concurrent `terms` aggregation.
        let want = [
            "min_build_leaf_collector", "build_aggregation_count", "post_collection", "max_collect_count", "initialize_count", "reduce_count", "avg_collect",
            "max_build_aggregation", "avg_collect_count", "max_build_leaf_collector", "min_build_leaf_collector_count", "build_aggregation", "min_initialize",
            "max_reduce", "build_leaf_collector_count", "avg_reduce", "min_collect_count", "avg_build_leaf_collector_count", "avg_build_leaf_collector",
            "max_collect", "reduce", "avg_build_aggregation", "min_post_collection", "max_initialize", "max_post_collection", "collect_count",
            "avg_post_collection", "avg_initialize", "post_collection_count", "build_leaf_collector", "min_collect", "min_build_aggregation", "initialize",
            "max_build_leaf_collector_count", "min_reduce", "collect",
        ];
        let agg = obj! { "type" => "MaxAggregator", "description" => "zz", "time_in_nanos" => 3i64, "breakdown" => obj! { "collect" => 2i64 } };
        let p = Phase { concurrent: true, slices: 1, ..Default::default() };
        let out = p.aggregations(vec![agg.clone()]);
        assert_eq!(keys(at(&Value::Arr(out.clone()), "0.breakdown")), want);
        assert_eq!(keys(at(&Value::Arr(out.clone()), "0")), vec!["type", "description", "time_in_nanos", "max_slice_time_in_nanos", "min_slice_time_in_nanos", "avg_slice_time_in_nanos", "breakdown"]);
        // Top-level aggregations come in the slice map's order: "aa" before "zz", "t" before "g".
        let named = |n: &str| obj! { "type" => "X", "description" => n, "time_in_nanos" => 1i64, "breakdown" => Map::new() };
        let order = |names: &[&str]| -> Vec<String> { p.aggregations(names.iter().map(|n| named(n)).collect()).iter().map(|a| at(a, "description").as_str().unwrap().to_string()).collect() };
        // Same bucket: `computeIfAbsent` links the later key first.
        assert_eq!(order(&["zz", "aa"]), vec!["aa", "zz"]);
        assert_eq!(order(&["g", "t"]), vec!["t", "g"]);
        assert_eq!(order(&["b", "a", "c"]), vec!["a", "b", "c"]);
        // Sequential: request order, sorted breakdown.
        let seq = Phase::default().aggregations(vec![named("zz"), named("aa")]);
        assert_eq!(at(&Value::Arr(seq.clone()), "0.description").as_str(), Some("zz"));
        let seq = Phase::default().aggregations(vec![obj! { "type" => "X", "description" => "s", "time_in_nanos" => 1i64, "breakdown" => obj! { "reduce" => 0i64, "collect" => 1i64, "build_aggregation" => 0i64 } }]);
        assert_eq!(keys(at(&Value::Arr(seq.clone()), "0.breakdown")), vec!["build_aggregation", "collect", "reduce"]);
    }

    #[test]
    fn global_aggregations_get_their_own_search() {
        let p = Phase { globals: vec!["g".into(), "g2".into()], ..Default::default() };
        let q = |d: &str| obj! { "type" => "MatchAllDocsQuery", "description" => d, "time_in_nanos" => 1i64, "breakdown" => Map::new() };
        let s = p.searches(vec![q("main"), q("global")], 7, 5);
        assert_eq!(s.len(), 2);
        assert_eq!(at(&Value::Arr(s.clone()), "0.query.0.description").as_str(), Some("main"));
        assert_eq!(at(&Value::Arr(s.clone()), "1.query.0.description").as_str(), Some("global"));
        assert_eq!(at(&Value::Arr(s.clone()), "1.collector.0.name").as_str(), Some("MultiBucketCollector: [[g, g2]]"));
        assert_eq!(keys(at(&Value::Arr(s.clone()), "0")), vec!["query", "rewrite_time", "collector"]);
    }
}
