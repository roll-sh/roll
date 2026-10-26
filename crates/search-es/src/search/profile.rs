//! `profile: true`: the per-shard profile of a search (`SearchProfileResults`).
//!
//! Query trees and their timing breakdowns come from the engine's profiler (a profiled
//! searcher view); collectors, aggregations and the fetch phase are timed here around the
//! work this crate drives. Aggregators are named after Elasticsearch's implementation classes
//! for the request's aggregation types.

use crate::json::{Map, Value};
use crate::obj;
use search_index::search::profile::{ProfileNode, Profiler};

/// What a shard's query phase recorded.
#[derive(Clone, Debug, Default)]
pub struct ShardProfile {
    pub searches: Vec<Value>,
    pub aggregations: Vec<Value>,
    pub dfs: Option<Value>,
}

/// One profiled query node (`ProfileResult`).
pub fn query_node(n: &ProfileNode) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), Value::Str(n.kind.clone()));
    m.insert("description".into(), Value::Str(n.description.clone()));
    m.insert("time_in_nanos".into(), Value::from(n.time_in_nanos.max(1) as i64));
    let mut b = Map::new();
    for (k, nanos, count) in &n.breakdown {
        b.insert((*k).to_string(), Value::from(*nanos as i64));
        b.insert(format!("{k}_count"), Value::from(*count as i64));
    }
    m.insert("breakdown".into(), Value::Obj(b));
    if !n.children.is_empty() {
        m.insert("children".into(), Value::Arr(n.children.iter().map(query_node).collect()));
    }
    Value::Obj(m)
}

/// The query trees and rewrite time of a profiler.
pub fn queries(p: &Profiler) -> (Vec<Value>, i64) {
    (p.tree().iter().map(query_node).collect(), p.rewrite_time().max(1) as i64)
}

/// A `searches` entry: the query trees, rewrite time and the collector tree.
pub fn search_entry(p: &Profiler, top_collector: &str, nanos: u64, agg_names: &[String]) -> Value {
    let (query, rewrite) = queries(p);
    let nanos = nanos.max(1) as i64;
    let mut children = vec![obj! { "name" => top_collector, "reason" => "search_top_hits", "time_in_nanos" => nanos }];
    if !agg_names.is_empty() {
        children.push(obj! { "name" => format!("AggregatorCollector: [{}]", agg_names.join(", ")), "reason" => "aggregation", "time_in_nanos" => nanos });
    }
    obj! {
        "query" => Value::Arr(query),
        "rewrite_time" => rewrite,
        "collector" => Value::Arr(vec![obj! {
            "name" => "QueryPhaseCollector",
            "reason" => "search_query_phase",
            "time_in_nanos" => nanos,
            "children" => Value::Arr(children),
        }]),
    }
}

/// The `dfs.statistics` entry of a `dfs_query_then_fetch` shard (`DfsProfiler`): the term and
/// collection statistics collection, timed as one weight creation.
pub fn dfs_statistics(nanos: u64, terms: u64) -> Value {
    let n = nanos.max(1) as i64;
    obj! {
        "statistics" => obj! {
            "type" => "statistics",
            "description" => "collect term statistics",
            "time_in_nanos" => n,
            "breakdown" => obj! {
                "rewrite" => 0i64,
                "rewrite_count" => 0i64,
                "create_weight" => n / 2,
                "create_weight_count" => 1i64,
                "term_statistics" => if terms > 0 { n / 4 } else { 0 },
                "term_statistics_count" => terms as i64,
                "collection_statistics" => n / 4,
                "collection_statistics_count" => 1i64,
            },
        },
    }
}

/// The dfs entry of a top-level `knn` search.
pub fn knn_dfs_entry(p: &Profiler, nanos: u64, vector_ops: u64) -> Value {
    let (query, rewrite) = queries(p);
    obj! {
        "vector_operations_count" => vector_ops.max(1) as i64,
        "query" => Value::Arr(query),
        "rewrite_time" => rewrite,
        "collector" => Value::Arr(vec![obj! { "name" => "TopScoreDocCollector", "reason" => "search_top_hits", "time_in_nanos" => nanos.max(1) as i64 }]),
    }
}

/// Elasticsearch's aggregator class for an aggregation of `ty` over a field of `field_ty`.
fn aggregator_class(ty: &str, body: &Map, field_ty: Option<&str>, has_subs: bool) -> &'static str {
    let numeric = field_ty.is_some_and(|t| {
        matches!(t, "long" | "integer" | "short" | "byte" | "double" | "float" | "half_float" | "scaled_float" | "unsigned_long" | "date" | "date_nanos" | "boolean")
    });
    let hint = body.get("execution_hint").and_then(Value::as_str);
    match ty {
        "terms" | "significant_terms" => {
            if numeric {
                "NumericTermsAggregator"
            } else if hint == Some("map") || body.contains_key("script") || field_ty.is_none() {
                "MapStringTermsAggregator"
            } else {
                "GlobalOrdinalsStringTermsAggregator"
            }
        }
        "significant_text" => "MapStringTermsAggregator",
        "rare_terms" => {
            if numeric {
                "LongRareTermsAggregator"
            } else {
                "StringRareTermsAggregator"
            }
        }
        "cardinality" => {
            if numeric || field_ty.is_none() {
                "CardinalityAggregator"
            } else {
                "GlobalOrdCardinalityAggregator"
            }
        }
        "avg" => "AvgAggregator",
        "sum" => "SumAggregator",
        "min" => "MinAggregator",
        "max" => "MaxAggregator",
        "stats" => "StatsAggregator",
        "extended_stats" => "ExtendedStatsAggregator",
        "value_count" => "ValueCountAggregator",
        "percentiles" => "TDigestPercentilesAggregator",
        "percentile_ranks" => "TDigestPercentileRanksAggregator",
        "median_absolute_deviation" => "MedianAbsoluteDeviationAggregator",
        "weighted_avg" => "WeightedAvgAggregator",
        "histogram" => "NumericHistogramAggregator",
        "date_histogram" => "DateHistogramAggregator",
        "auto_date_histogram" => "AutoDateHistogramAggregator.FromSingle",
        "variable_width_histogram" => "VariableWidthHistogramAggregator",
        "range" | "date_range" | "ip_range" => {
            if has_subs {
                "RangeAggregator.Overlap"
            } else {
                "RangeAggregator.NoOverlap"
            }
        }
        "filter" => "FilterAggregator",
        "filters" | "adjacency_matrix" => "FiltersAggregator.Compatible",
        "global" => "GlobalAggregator",
        "missing" => "MissingAggregator",
        "nested" => "NestedAggregator",
        "reverse_nested" => "ReverseNestedAggregator",
        "top_hits" => "TopHitsAggregator",
        "top_metrics" => "TopMetricsAggregator",
        "composite" => "CompositeAggregator",
        "sampler" => "SamplerAggregator",
        "diversified_sampler" => "DiversifiedMapSamplerAggregator",
        "random_sampler" => "RandomSamplerAggregator",
        "geo_bounds" => "GeoBoundsAggregator",
        "geo_centroid" => "GeoCentroidAggregator",
        "geohash_grid" => "GeoHashGridAggregator",
        "geotile_grid" => "GeoTileGridAggregator",
        "geo_distance" => "RangeAggregator.NoOverlap",
        "scripted_metric" => "ScriptedMetricAggregator",
        "string_stats" => "StringStatsAggregator",
        "boxplot" => "BoxplotAggregator",
        "matrix_stats" => "MatrixStatsAggregator",
        "time_series" => "TimeSeriesAggregator",
        "multi_terms" => "MultiTermsAggregator",
        "children" => "ChildrenToParentAggregator",
        "parent" => "ParentToChildrenAggregator",
        _ => "Aggregator",
    }
}

/// Pipeline aggregations run at reduce time and have no aggregator.
fn is_pipeline(ty: &str) -> bool {
    matches!(
        ty,
        "avg_bucket"
            | "sum_bucket"
            | "min_bucket"
            | "max_bucket"
            | "stats_bucket"
            | "extended_stats_bucket"
            | "percentiles_bucket"
            | "bucket_script"
            | "bucket_selector"
            | "bucket_sort"
            | "cumulative_sum"
            | "cumulative_cardinality"
            | "derivative"
            | "moving_fn"
            | "moving_percentiles"
            | "serial_diff"
            | "normalize"
            | "inference"
            | "change_point"
            | "bucket_count_ks_test"
            | "bucket_correlation"
    )
}

/// Aggregator profiles for the request's `aggs` (one per aggregator, sub-aggregations as
/// children), with the shard's collection time spread over them.
pub fn aggregations(aggs: &Map, field_type: &dyn Fn(&str) -> Option<String>, nanos: u64, docs: u64) -> Vec<Value> {
    let mut out = Vec::new();
    for (name, def) in aggs {
        let Value::Obj(def) = def else { continue };
        let subs = def.get("aggs").or_else(|| def.get("aggregations")).and_then(Value::as_object);
        let Some((ty, body)) = def.iter().find(|(k, _)| !matches!(k.as_str(), "aggs" | "aggregations" | "meta")) else { continue };
        if is_pipeline(ty) {
            continue;
        }
        let body = body.as_object().cloned().unwrap_or_default();
        let fty = body.get("field").and_then(Value::as_str).and_then(field_type);
        let mut sub_aggs = subs.map(|s| aggregations(s, field_type, nanos / 4, docs)).unwrap_or_default();
        let class = aggregator_class(ty, &body, fty.as_deref(), !sub_aggs.is_empty());
        let t = (nanos / 4).max(1) as i64;
        let mut b = Map::new();
        for k in ["reduce", "build_aggregation", "build_leaf_collector", "collect", "initialize", "post_collection"] {
            let (v, c) = match k {
                "reduce" => (0, 0),
                "collect" => (t, docs as i64),
                "build_leaf_collector" => (t, 1),
                _ => (t, 1),
            };
            b.insert(k.into(), Value::from(v));
            b.insert(format!("{k}_count"), Value::from(c));
        }
        let mut m = Map::new();
        m.insert("type".into(), Value::Str(class.into()));
        m.insert("description".into(), Value::Str(name.clone()));
        m.insert("time_in_nanos".into(), Value::from(t * 5));
        m.insert("breakdown".into(), Value::Obj(b));
        if !sub_aggs.is_empty() {
            m.insert("children".into(), Value::Arr(std::mem::take(&mut sub_aggs)));
        }
        out.push(Value::Obj(m));
    }
    out
}

/// Names the aggregators the engine's profiler left without a class (an aggregator that
/// doesn't report one) after their request definitions, as [`aggregations`] would.
pub fn fill_types(nodes: &mut [Value], aggs: &Map, field_type: &dyn Fn(&str) -> Option<String>) {
    for node in nodes {
        let Value::Obj(m) = node else { continue };
        let Some(Value::Obj(def)) = m.get("description").and_then(Value::as_str).and_then(|d| aggs.get(d)) else { continue };
        let subs = def.get("aggs").or_else(|| def.get("aggregations")).and_then(Value::as_object);
        if m.get("type").and_then(Value::as_str).is_some_and(str::is_empty)
            && let Some((ty, body)) = def.iter().find(|(k, _)| !matches!(k.as_str(), "aggs" | "aggregations" | "meta"))
        {
            let body = body.as_object().cloned().unwrap_or_default();
            let fty = body.get("field").and_then(Value::as_str).and_then(field_type);
            let class = aggregator_class(ty, &body, fty.as_deref(), subs.is_some_and(|s| !s.is_empty()));
            m.insert("type".into(), Value::Str(class.into()));
        }
        if let (Some(subs), Some(Value::Arr(children))) = (subs, m.get_mut("children")) {
            fill_types(children, subs, field_type);
        }
    }
}

/// `profile.request`: the coordinator's view of the request (`SearchProfileResults`): the
/// requested index expressions and the search source as `SearchSourceBuilder.toXContent`
/// prints it, in its field order (queries normalized, the other sections as given).
pub fn request(src: &crate::search_source::SearchSource, indices: &[String]) -> Value {
    use crate::search_source::{Sort, SortOrder, TrackTotalHits};
    let mut m = Map::new();
    if let Some(v) = src.from {
        m.insert("from".into(), Value::from(v as i64));
    }
    if let Some(v) = src.size {
        m.insert("size".into(), Value::from(v as i64));
    }
    if let Some(t) = &src.timeout {
        m.insert("timeout".into(), Value::Str(t.string_rep()));
    }
    if let Some(v) = src.terminate_after {
        m.insert("terminate_after".into(), Value::from(v as i64));
    }
    if let Some(q) = &src.query {
        m.insert("query".into(), q.to_value());
    }
    if let Some(q) = &src.post_filter {
        m.insert("post_filter".into(), q.to_value());
    }
    if let Some(v) = src.min_score {
        m.insert("min_score".into(), Value::from(v as f64));
    }
    for (k, v) in [("version", src.version), ("seq_no_primary_term", src.seq_no_primary_term), ("explain", src.explain)] {
        if let Some(b) = v {
            m.insert(k.into(), Value::Bool(b));
        }
    }
    if src.profile {
        m.insert("profile".into(), Value::Bool(true));
    }
    if let Some(fs) = &src.source {
        let v = if fs.includes.is_empty() && fs.excludes.is_empty() {
            Value::Bool(fs.fetch)
        } else {
            let list = |l: &[String]| Value::Arr(l.iter().map(|s| Value::Str(s.clone())).collect());
            obj! { "includes" => list(&fs.includes), "excludes" => list(&fs.excludes) }
        };
        m.insert("_source".into(), v);
    }
    if !src.sort.is_empty() {
        let order = |o: &SortOrder| if matches!(o, SortOrder::Asc) { "asc" } else { "desc" };
        let sorts: Vec<Value> = src
            .sort
            .iter()
            .filter_map(|s| match s {
                Sort::Score { order: o } => Some(obj! { "_score" => obj! { "order" => order(o) } }),
                Sort::Field { field, order: o, .. } => {
                    let o = o.as_ref().map(order).unwrap_or(if field == "_score" { "desc" } else { "asc" });
                    Some(obj! { field.as_str() => obj! { "order" => o } })
                }
                _ => None,
            })
            .collect();
        m.insert("sort".into(), Value::Arr(sorts));
    }
    if src.track_scores {
        m.insert("track_scores".into(), Value::Bool(true));
    }
    // `SearchSourceBuilder` keeps the setting as an int: `true` is `Integer.MAX_VALUE`,
    // `false` is -1.
    match &src.track_total_hits {
        Some(TrackTotalHits::Accurate) => {
            m.insert("track_total_hits".into(), Value::from(i64::from(i32::MAX)));
        }
        Some(TrackTotalHits::Disabled) => {
            m.insert("track_total_hits".into(), Value::from(-1i64));
        }
        Some(TrackTotalHits::UpTo(n)) => {
            m.insert("track_total_hits".into(), Value::from(*n as i64));
        }
        None => {}
    }
    if let Some(sa) = &src.search_after {
        m.insert("search_after".into(), Value::Arr(sa.clone()));
    }
    for (k, v) in [("highlight", &src.highlight), ("aggregations", &src.aggs), ("suggest", &src.suggest), ("ext", &src.ext), ("runtime_mappings", &src.runtime_mappings), ("retriever", &src.retriever)] {
        if let Some(v) = v {
            m.insert(k.into(), Value::Obj(v.clone()));
        }
    }
    obj! {
        "source" => Value::Obj(m),
        "indices" => Value::Arr(indices.iter().map(|s| Value::Str(s.clone())).collect()),
    }
}

/// Timings of the fetch phase of one shard.
#[derive(Clone, Debug, Default)]
pub struct FetchTimes {
    pub nanos: u64,
    pub hits: u64,
}

/// The `fetch` profile: stored-field loading plus one child per sub-phase that ran
/// (alphabetical, like Elasticsearch's output).
pub fn fetch(t: &FetchTimes, stored_none: bool, phases: &[&str]) -> Value {
    let n = t.nanos.max(4) as i64;
    let hits = t.hits.max(1) as i64;
    let mut b = Map::new();
    b.insert("load_stored_fields".into(), Value::from(n / 4));
    b.insert("load_stored_fields_count".into(), Value::from(hits));
    b.insert("next_reader".into(), Value::from(n / 4));
    b.insert("next_reader_count".into(), Value::from(1i64));
    b.insert("load_source".into(), Value::from(if stored_none { 0 } else { n / 4 }));
    b.insert("load_source_count".into(), Value::from(if stored_none { 0 } else { hits }));
    let stored: Vec<Value> = if stored_none { Vec::new() } else { ["_id", "_routing", "_source"].iter().map(|s| Value::Str(s.to_string())).collect() };
    let mut m = obj! {
        "type" => "fetch",
        "description" => "",
        "time_in_nanos" => n,
        "breakdown" => Value::Obj(b),
        "debug" => obj! { "stored_fields" => Value::Arr(stored) },
    };
    if !stored_none && !phases.is_empty() {
        let mut names: Vec<&str> = phases.to_vec();
        names.sort_unstable();
        let per = (n / (names.len() as i64 + 1)).max(1);
        let children: Vec<Value> = names
            .iter()
            .map(|p| {
                let mut c = obj! {
                    "type" => *p,
                    "description" => "",
                    "time_in_nanos" => per,
                    "breakdown" => obj! { "process_count" => hits, "process" => per / 2, "next_reader" => per / 2, "next_reader_count" => 1i64 },
                };
                if *p == "FetchSourcePhase"
                    && let Value::Obj(m) = &mut c
                {
                    m.insert("debug".into(), obj! { "fast_path" => hits });
                }
                c
            })
            .collect();
        if let Value::Obj(mm) = &mut m {
            mm.insert("children".into(), Value::Arr(children));
        }
    }
    m
}

/// What OpenSearch's fetch profile (`FetchProfiler`, 3.3+) reports for one shard: the
/// sub-phases that ran (in `SearchModule`'s registration order), whether stored fields and
/// `_source` were loaded, the inner hits fetched per hit and the `top_hits` aggregations.
#[derive(Clone, Debug, Default)]
pub struct OsFetch<'a> {
    pub phases: Vec<&'static str>,
    pub stored: bool,
    pub source: bool,
    pub inner_hits: Vec<&'a str>,
    pub top_hits: Vec<String>,
    /// The shard matched documents: its `top_hits` aggregations fetched some (even with
    /// `size: 0`, which fetches no hits of its own).
    pub matched: bool,
}

/// The `fetch` sub-phases OpenSearch registers, in order, and when each one runs.
pub const OS_FETCH_PHASES: [&str; 10] = [
    "ExplainPhase",
    "FetchDocValuesPhase",
    "ScriptFieldsPhase",
    "FetchSourcePhase",
    "FetchFieldsPhase",
    "FetchVersionPhase",
    "SeqNoPrimaryTermPhase",
    "MatchedQueriesPhase",
    "HighlightPhase",
    "FetchScorePhase",
];

/// One OpenSearch fetch profile entry (`type` = `description`).
fn os_fetch_entry(name: &str, nanos: i64, hits: i64, stored: bool, source: bool, phases: &[&str]) -> Value {
    let t = (nanos / 8).max(1);
    let mut b = Map::new();
    let mut put = |k: &str, v: i64, c: i64| {
        b.insert(k.into(), Value::from(if c == 0 { 0 } else { v }));
        b.insert(format!("{k}_count"), Value::from(c));
    };
    put("build_sub_phase_processors", t, 1);
    put("create_stored_fields_visitor", t, 1);
    put("get_next_reader", t, 1);
    put("load_source", t, if source { hits } else { 0 });
    put("load_stored_fields", t, if stored { hits } else { 0 });
    let mut m = obj! { "type" => name, "description" => name, "time_in_nanos" => nanos.max(1), "breakdown" => Value::Obj(b) };
    if !phases.is_empty() {
        let per = (t / 2).max(1);
        let children: Vec<Value> = phases
            .iter()
            .map(|p| {
                obj! {
                    "type" => *p,
                    "description" => *p,
                    "time_in_nanos" => per * 2,
                    "breakdown" => obj! { "process" => per, "process_count" => hits, "set_next_reader" => per, "set_next_reader_count" => 1i64 },
                }
            })
            .collect();
        if let Value::Obj(mm) = &mut m {
            mm.insert("children".into(), Value::Arr(children));
        }
    }
    m
}

/// OpenSearch's `fetch` profile of a shard: an array with one entry per `top_hits`
/// aggregation (when the shard matched anything), the main `fetch` and one per inner hits
/// definition (when it fetched hits); empty when it fetched nothing.
pub fn fetch_os(t: &FetchTimes, f: &OsFetch) -> Value {
    if t.hits == 0 && !(f.matched && !f.top_hits.is_empty()) {
        return Value::Arr(Vec::new());
    }
    let n = t.nanos.max(8) as i64;
    let hits = t.hits as i64;
    let mut out = Vec::new();
    if f.matched {
        for name in &f.top_hits {
            out.push(os_fetch_entry(&format!("fetch_top_hits_aggregation[{name}]"), n / 4, 1, true, true, &["FetchSourcePhase"]));
        }
    }
    if t.hits == 0 {
        return Value::Arr(out);
    }
    out.push(os_fetch_entry("fetch", n, hits, f.stored, f.source, &f.phases));
    for name in &f.inner_hits {
        out.push(os_fetch_entry(&format!("fetch_inner_hits[{name}]"), n / 4, 1, false, true, &["FetchSourcePhase"]));
    }
    Value::Arr(out)
}

/// The names of the `top_hits` aggregations of a request, depth first.
pub fn top_hits_names(aggs: &Map, out: &mut Vec<String>) {
    for (name, def) in aggs {
        let Value::Obj(def) = def else { continue };
        if def.contains_key("top_hits") {
            out.push(name.clone());
        }
        if let Some(subs) = def.get("aggs").or_else(|| def.get("aggregations")).and_then(Value::as_object) {
            top_hits_names(subs, out);
        }
    }
}

/// One shard's entry of OpenSearch's `profile.shards` (`ProfileShardResult`): no node, shard
/// or cluster fields, network times (0 on one node) and `fetch` always present.
pub fn shard_os(node: &str, index: &str, shard: usize, p: &ShardProfile, fetch: Value) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), Value::Str(format!("[{node}][{index}][{shard}]")));
    m.insert("inbound_network_time_in_millis".into(), Value::from(0i64));
    m.insert("outbound_network_time_in_millis".into(), Value::from(0i64));
    m.insert("searches".into(), Value::Arr(p.searches.clone()));
    let mut aggs = p.aggregations.clone();
    aggs.iter_mut().for_each(os_debug_order);
    m.insert("aggregations".into(), Value::Arr(aggs));
    m.insert("fetch".into(), fetch);
    Value::Obj(m)
}

/// OpenSearch prints an aggregation's `debug` map from a `HashMap`: its keys come in that
/// map's iteration order (children too).
fn os_debug_order(agg: &mut Value) {
    let Value::Obj(m) = agg else { return };
    if let Some(Value::Obj(d)) = m.get_mut("debug") {
        let keys: Vec<String> = d.keys().cloned().collect();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let order: Vec<String> = crate::opensearch::java_hashmap_order(&refs, refs.len()).into_iter().map(str::to_string).collect();
        let mut sorted = Map::new();
        for k in order {
            if let Some(v) = d.shift_remove(&k) {
                sorted.insert(k, v);
            }
        }
        *d = sorted;
    }
    if let Some(Value::Arr(children)) = m.get_mut("children") {
        children.iter_mut().for_each(os_debug_order);
    }
}

/// One shard's entry of `profile.shards`.
pub fn shard(node: &str, index: &str, shard: usize, p: &ShardProfile, fetch: Option<Value>) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), Value::Str(format!("[{node}][{index}][{shard}]")));
    m.insert("node_id".into(), Value::Str(node.to_string()));
    m.insert("shard_id".into(), Value::from(shard as i64));
    m.insert("index".into(), Value::Str(index.to_string()));
    m.insert("cluster".into(), Value::Str("(local)".into()));
    if let Some(d) = &p.dfs {
        m.insert("dfs".into(), d.clone());
    }
    m.insert("searches".into(), Value::Arr(p.searches.clone()));
    m.insert("aggregations".into(), Value::Arr(p.aggregations.clone()));
    if let Some(f) = fetch {
        m.insert("fetch".into(), f);
    }
    Value::Obj(m)
}
