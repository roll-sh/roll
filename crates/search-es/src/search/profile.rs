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
