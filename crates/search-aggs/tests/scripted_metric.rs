//! `scripted_metric` through the [`ScriptHost`] hooks, with a native "script engine" whose
//! scripts are named by their source: the plumbing (params merging, one state per bucket,
//! fresh states for empty buckets, shard results concatenated until the final reduce) is
//! checked against what Elasticsearch documents and returns for the equivalent Painless.

mod common;

use std::sync::Arc;

use common::{TestIndex, run_with};
use search_aggs::script::{ScriptedMetricReduce, ScriptedMetricScripts, ScriptedMetricState, ValuesScript};
use search_aggs::{AggError, Json, ReduceOptions, Result, Script, ScriptHost};
use search_index::SegmentContext;

/// Scripts by source:
/// - init `zero`: `state.n = 0` (otherwise the state starts as `{}` and `n` as null → 0 here).
/// - map `count`: `state.n += params.inc ?: 1`.
/// - combine `n`: `return state.n` (`null` when no init and no doc).
/// - reduce `sum`: `states.stream().mapToLong(…).sum()` (nulls skipped); `list`: `return states`;
///   `params`: `return params`.
struct Engine;

struct Scripts {
    init: Option<String>,
    inc: i64,
}

struct State {
    n: Option<i64>,
    inc: i64,
}

impl ScriptedMetricState for State {
    fn set_segment(&mut self, _seg: &SegmentContext) -> Result<()> {
        Ok(())
    }
    fn map(&mut self, _doc: u32, _score: f32) -> Result<()> {
        self.n = Some(self.n.unwrap_or(0) + self.inc);
        Ok(())
    }
    fn combine(&mut self) -> Result<Json> {
        Ok(self.n.map_or(Json::Null, Json::Int))
    }
}

impl ScriptedMetricScripts for Scripts {
    fn new_state(&self, _params: &Json) -> Result<Box<dyn ScriptedMetricState>> {
        let n = match self.init.as_deref() {
            Some("zero") => Some(0),
            _ => None,
        };
        Ok(Box::new(State { n, inc: self.inc }))
    }
}

struct Reduce(String);

impl ScriptedMetricReduce for Reduce {
    fn reduce(&self, states: &[Json], params: &Json) -> Result<Json> {
        Ok(match self.0.as_str() {
            "sum" => Json::Int(states.iter().filter_map(Json::as_i64).sum()),
            "list" => Json::Arr(states.to_vec()),
            _ => params.clone(),
        })
    }
}

impl ScriptHost for Engine {
    fn values(&self, _script: &Script) -> Result<Arc<dyn ValuesScript>> {
        Err(AggError::illegal_argument("no values scripts"))
    }
    fn scripted_metric(&self, init: Option<&Script>, map: &Script, _combine: &Script, _params: &Json) -> Result<Arc<dyn ScriptedMetricScripts>> {
        assert_eq!(map.source.as_deref(), Some("count"));
        let inc = map.params.get("inc").and_then(Json::as_i64).unwrap_or(1);
        Ok(Arc::new(Scripts { init: init.and_then(|s| s.source.clone()), inc }))
    }
    fn scripted_metric_reduce(&self, reduce: &Script) -> Result<Arc<dyn ScriptedMetricReduce>> {
        Ok(Arc::new(Reduce(reduce.source.clone().unwrap_or_default())))
    }
}

fn index() -> (TestIndex, Vec<search_index::Shard>) {
    let mapping = Json::parse(r#"{"properties":{"k":{"type":"keyword"},"n":{"type":"long"}}}"#).unwrap();
    let index = TestIndex::from_mapping(&mapping);
    let docs: Vec<(String, Json)> =
        (0..30).map(|i| (i.to_string(), Json::parse(&format!(r#"{{"k":"k{}","n":{i}}}"#, i % 3)).unwrap())).collect();
    // Three shards of 10 docs, two segments each.
    let shards = docs.chunks(10).map(|c| index.shard(c, 2)).collect();
    (index, shards)
}

fn run(body: &str, final_reduce: bool) -> Result<Json> {
    let (index, shards) = index();
    let searchers: Vec<_> = shards.iter().map(|s| s.searcher()).collect();
    let opts = ReduceOptions { final_reduce, ..ReduceOptions::default() };
    run_with(&index, &searchers, body, false, Some(Arc::new(Engine)), &opts)
}

#[test]
fn sums_across_shards() {
    let r = run(r#"{"aggs":{"m":{"scripted_metric":{"init_script":"zero","map_script":"count","combine_script":"n","reduce_script":"sum"}}}}"#, true).unwrap();
    assert_eq!(r.to_json_string(), r#"{"m":{"value":30}}"#);
    // The reduce script sees one state per shard, in shard order.
    let r = run(r#"{"aggs":{"m":{"scripted_metric":{"map_script":{"source":"count","params":{"inc":2}},"combine_script":"n","reduce_script":"list"}}}}"#, true).unwrap();
    assert_eq!(r.to_json_string(), r#"{"m":{"value":[20,20,20]}}"#);
}

#[test]
fn buckets_get_their_own_state() {
    let body = r#"{"query":{"range":{"n":{"lt":20}}},"aggs":{"t":{"terms":{"field":"k","min_doc_count":0},"aggs":{"m":{"scripted_metric":{"init_script":"zero","map_script":"count","combine_script":"n","reduce_script":"list"}}}}}}"#;
    let r = run(body, true).unwrap();
    // The third shard matches nothing: its buckets still run init + combine (0). The order of
    // the states follows the bucket merge order of the terms reduce.
    let buckets = r.get("t").unwrap().get("buckets").unwrap().as_array().unwrap();
    let got: Vec<(String, Vec<i64>)> = buckets
        .iter()
        .map(|b| {
            let mut v: Vec<i64> = b.get("m").unwrap().get("value").unwrap().as_array().unwrap().iter().map(|x| x.as_i64().unwrap()).collect();
            v.sort();
            (b.get("key").unwrap().as_str().unwrap().to_string(), v)
        })
        .collect();
    assert_eq!(got, vec![("k0".into(), vec![0, 3, 4]), ("k1".into(), vec![0, 3, 4]), ("k2".into(), vec![0, 3, 3])]);
}

#[test]
fn params_merge_into_the_reduce_script() {
    let r = run(r#"{"aggs":{"m":{"scripted_metric":{"map_script":"count","combine_script":"n","reduce_script":{"source":"params","params":{"r":1}},"params":{"a":2}}}}}"#, true).unwrap();
    assert_eq!(r.to_json_string(), r#"{"m":{"value":{"r":1,"a":2}}}"#);
    let e = run(r#"{"aggs":{"m":{"scripted_metric":{"map_script":{"source":"count","params":{"a":1}},"combine_script":"n","reduce_script":"sum","params":{"a":2}}}}}"#, true).unwrap_err();
    assert_eq!(e.reason, "Parameter name \"a\" used in both aggregation and script parameters");
}

#[test]
fn errors_and_sub_aggregations() {
    let e = run(r#"{"aggs":{"m":{"scripted_metric":{"map_script":"count","reduce_script":"sum"}}}}"#, true).unwrap_err();
    assert_eq!(e.reason, "[combine_script] must not be null: [m]");
    let e = run(r#"{"aggs":{"m":{"scripted_metric":{"combine_script":"n"}}}}"#, true).unwrap_err();
    assert_eq!(e.reason, "Required [map_script]");
    // Sub-aggregations are accepted and never built.
    let r = run(r#"{"aggs":{"m":{"scripted_metric":{"map_script":"count","combine_script":"n","reduce_script":"sum"},"aggs":{"x":{"max":{"field":"n"}}}}}}"#, true).unwrap();
    assert_eq!(r.to_json_string(), r#"{"m":{"value":30}}"#);
}
