//! Painless for aggregations: search-aggs' `ScriptHost` — value scripts (`aggs` context, `_value`
//! for value scripts), `scripted_metric` (`init`/`map`/`combine` per shard, `reduce` at the end,
//! `state` / `states`), `bucket_script` / `bucket_selector`, `moving_fn` and `script_heuristic`.

use super::scripts::{DocHost, Leaf};
use crate::cluster::Cluster;
use crate::engine::fields::FieldTable;
use crate::json::Value;
use search_aggs::script::{BucketScript, HeuristicScript, LeafValuesScript, MovingFnScript, ScriptKind, ScriptValue, ScriptedMetricReduce, ScriptedMetricScripts, ScriptedMetricState, ValuesScript};
use search_aggs::{AggError, Json, ScriptHost};
use search_painless::Context;
use std::sync::Arc;

fn agg_err(e: crate::error::EsError) -> AggError {
    let mut a = AggError::new(e.status, e.kind.to_string(), e.reason().to_string());
    for (k, v) in e.meta() {
        a.meta.push((k.to_string(), super::aggs::to_json(v)));
    }
    if let Some(c) = &e.caused_by {
        a.caused_by = Some(Box::new(agg_err((**c).clone())));
    }
    a
}

fn script_err(e: search_painless::ScriptError) -> AggError {
    agg_err(crate::scripting::script_error(e))
}

/// An aggs `Script` as the query DSL's.
fn to_script(s: &search_aggs::Script) -> crate::query::script::Script {
    use crate::query::script::{Script, ScriptType};
    let params = match super::aggs::from_json(&s.params) {
        Value::Obj(m) => m,
        _ => Default::default(),
    };
    let mut options = crate::json::Map::new();
    for (k, v) in &s.options {
        options.insert(k.clone(), Value::Str(v.clone()));
    }
    match (&s.source, &s.id) {
        (Some(src), _) => Script { ty: ScriptType::Inline, lang: s.lang.clone().or(Some("painless".into())), id_or_code: src.clone(), options, params },
        (None, Some(id)) => Script { ty: ScriptType::Stored, lang: None, id_or_code: id.clone(), options, params },
        _ => Script::inline(""),
    }
}

/// The host for one shard (or the reduce, without a table).
pub struct Host {
    pub scripts: Arc<crate::scripting::ScriptService>,
    pub state: Arc<crate::meta::ClusterState>,
    pub table: Option<Arc<FieldTable>>,
    pub shard: i32,
    pub index: String,
}

impl Host {
    pub fn new(cluster: &Cluster, table: Option<Arc<FieldTable>>, shard: i32, index: String) -> Host {
        Host { scripts: cluster.scripts.clone(), state: cluster.state(), table, shard, index }
    }

    fn compile(&self, s: &search_aggs::Script, ctx: Context) -> search_aggs::Result<crate::scripting::Bound> {
        self.scripts.compile(&self.state, &to_script(s), ctx).map_err(agg_err)
    }
}

/// A painless value as an aggregation script value.
fn script_value(v: &search_painless::Value, out: &mut Vec<ScriptValue>) {
    match v.to_json() {
        serde_json::Value::Array(a) => {
            for x in a {
                out.push(json_value(&x));
            }
        }
        other => {
            let jv = json_value(&other);
            out.push(jv);
        }
    }
}

fn json_value(j: &serde_json::Value) -> ScriptValue {
    match j {
        serde_json::Value::Null => ScriptValue::Null,
        serde_json::Value::Bool(b) => ScriptValue::Bool(*b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => ScriptValue::Long(i),
            None => ScriptValue::Double(n.as_f64().unwrap_or(f64::NAN)),
        },
        serde_json::Value::String(s) => match crate::time::DateFormatter::new("strict_date_optional_time").ok().and_then(|f| f.parse_millis(s).ok()) {
            Some(ms) if s.contains('T') && s.ends_with('Z') => ScriptValue::Date(ms),
            _ => ScriptValue::Str(s.clone()),
        },
        other => ScriptValue::Str(other.to_string()),
    }
}

fn to_painless(v: &ScriptValue) -> search_painless::Value {
    use search_painless::Value as PV;
    match v {
        ScriptValue::Null => PV::Null,
        ScriptValue::Bool(b) => PV::Bool(*b),
        ScriptValue::Long(l) => PV::Long(*l),
        ScriptValue::Double(d) => PV::Double(*d),
        ScriptValue::Str(s) => PV::str(s),
        ScriptValue::Date(ms) => search_painless::value::zoned_date_time_millis(*ms),
        ScriptValue::GeoPoint(lat, lon) => PV::from_json_map(&serde_json::json!({"lat": lat, "lon": lon})),
    }
}

struct Values {
    bound: crate::scripting::Bound,
    table: Arc<FieldTable>,
    shard: i32,
    index: String,
}

struct ValuesLeaf(Leaf);

/// A runtime field's values as an aggregation values source.
struct RuntimeValues(super::runtime::Evaluator);

struct RuntimeValuesLeaf(super::runtime::EvalLeaf);

impl ValuesScript for RuntimeValues {
    fn leaf(&self, seg: &search_index::SegmentContext) -> search_aggs::Result<Box<dyn LeafValuesScript>> {
        Ok(Box::new(RuntimeValuesLeaf(self.0.leaf(seg).map_err(agg_err)?)))
    }
}

impl LeafValuesScript for RuntimeValuesLeaf {
    fn run(&mut self, doc: u32, _score: f32, _value: Option<&ScriptValue>, out: &mut Vec<ScriptValue>) -> search_aggs::Result<()> {
        for v in self.0.values(doc).map_err(agg_err)? {
            out.push(super::runtime::to_script_value(&v));
        }
        Ok(())
    }
}

impl ValuesScript for Values {
    fn needs_score(&self) -> bool {
        self.bound.painless().is_some_and(|s| s.needs_score())
    }
    fn leaf(&self, seg: &search_index::SegmentContext) -> search_aggs::Result<Box<dyn LeafValuesScript>> {
        let s = self.bound.painless().ok_or_else(|| AggError::illegal_argument("not a painless script"))?;
        Ok(Box::new(ValuesLeaf(Leaf::new(s, &self.bound.params, DocHost::new(self.table.clone(), seg.clone(), self.shard, self.index.clone())))))
    }
}

impl LeafValuesScript for ValuesLeaf {
    fn run(&mut self, doc: u32, score: f32, value: Option<&ScriptValue>, out: &mut Vec<ScriptValue>) -> search_aggs::Result<()> {
        if let Some(v) = value {
            self.0.set_var("_value", to_painless(v));
        }
        let v = self.0.run(doc, score as f64).map_err(script_err)?;
        script_value(&v, out);
        Ok(())
    }
}

struct Bucket {
    bound: crate::scripting::Bound,
}

impl BucketScript for Bucket {
    fn run(&self, vars: &[(String, Json)]) -> search_aggs::Result<Json> {
        let s = self.bound.painless().ok_or_else(|| AggError::illegal_argument("not a painless script"))?;
        let mut params = match &self.bound.params {
            serde_json::Value::Object(m) => m.clone(),
            _ => serde_json::Map::new(),
        };
        for (k, v) in vars {
            params.insert(k.clone(), v.to_serde());
        }
        let mut inst = s.instance();
        inst.set_params(&serde_json::Value::Object(params));
        let v = inst.execute(&mut search_painless::NoHost).map_err(script_err)?;
        Ok(Json::from_serde(&v.to_json()))
    }
}

struct MovFn {
    bound: crate::scripting::Bound,
}

impl MovingFnScript for MovFn {
    fn run(&self, values: &[f64], params: &Json) -> search_aggs::Result<f64> {
        let s = self.bound.painless().ok_or_else(|| AggError::illegal_argument("not a painless script"))?;
        let mut inst = s.instance();
        inst.set_params(&params.to_serde());
        inst.set_var("values", search_painless::Value::double_array(values.to_vec()));
        inst.execute_f64(&mut search_painless::NoHost).map_err(script_err)
    }
}

struct Heuristic {
    bound: crate::scripting::Bound,
}

impl HeuristicScript for Heuristic {
    fn score(&self, subset_freq: i64, subset_size: i64, superset_freq: i64, superset_size: i64) -> search_aggs::Result<f64> {
        let s = self.bound.painless().ok_or_else(|| AggError::illegal_argument("not a painless script"))?;
        let mut params = match &self.bound.params {
            serde_json::Value::Object(m) => m.clone(),
            _ => serde_json::Map::new(),
        };
        params.insert("_subset_freq".into(), subset_freq.into());
        params.insert("_subset_size".into(), subset_size.into());
        params.insert("_superset_freq".into(), superset_freq.into());
        params.insert("_superset_size".into(), superset_size.into());
        let mut inst = s.instance();
        inst.set_params(&serde_json::Value::Object(params));
        inst.execute_f64(&mut search_painless::NoHost).map_err(script_err)
    }
}

struct Scripted {
    init: Option<crate::scripting::Bound>,
    map: crate::scripting::Bound,
    combine: crate::scripting::Bound,
    table: Arc<FieldTable>,
    shard: i32,
    index: String,
}

struct ScriptedState {
    state: search_painless::Value,
    params: serde_json::Value,
    map: crate::scripting::Bound,
    combine: crate::scripting::Bound,
    leaf: Option<Leaf>,
    table: Arc<FieldTable>,
    shard: i32,
    index: String,
}

// SAFETY: a state lives on the thread collecting its shard; its Painless values are not shared.
unsafe impl Send for ScriptedState {}

impl ScriptedMetricScripts for Scripted {
    fn new_state(&self, params: &Json) -> search_aggs::Result<Box<dyn ScriptedMetricState>> {
        let state = search_painless::Value::from_json_map(&serde_json::json!({}));
        let params = params.to_serde();
        if let Some(init) = &self.init {
            let s = init.painless().ok_or_else(|| AggError::illegal_argument("not a painless script"))?;
            let mut inst = s.instance();
            inst.set_params(&params);
            inst.set_var("state", state.clone());
            inst.execute(&mut search_painless::NoHost).map_err(script_err)?;
        }
        Ok(Box::new(ScriptedState { state, params, map: self.map.clone(), combine: self.combine.clone(), leaf: None, table: self.table.clone(), shard: self.shard, index: self.index.clone() }))
    }
    fn needs_score(&self) -> bool {
        self.map.painless().is_some_and(|s| s.needs_score())
    }
}

impl ScriptedMetricState for ScriptedState {
    fn set_segment(&mut self, seg: &search_index::SegmentContext) -> search_aggs::Result<()> {
        let s = self.map.painless().ok_or_else(|| AggError::illegal_argument("not a painless script"))?;
        let mut leaf = Leaf::new(s, &self.params, DocHost::new(self.table.clone(), seg.clone(), self.shard, self.index.clone()));
        leaf.set_var("state", self.state.clone());
        self.leaf = Some(leaf);
        Ok(())
    }
    fn map(&mut self, doc: u32, score: f32) -> search_aggs::Result<()> {
        if let Some(l) = &mut self.leaf {
            l.run(doc, score as f64).map_err(script_err)?;
        }
        Ok(())
    }
    fn combine(&mut self) -> search_aggs::Result<Json> {
        let s = self.combine.painless().ok_or_else(|| AggError::illegal_argument("not a painless script"))?;
        let mut inst = s.instance();
        inst.set_params(&self.params);
        inst.set_var("state", self.state.clone());
        let v = inst.execute(&mut search_painless::NoHost).map_err(script_err)?;
        Ok(Json::from_serde(&v.to_json()))
    }
}

struct Reduce {
    bound: crate::scripting::Bound,
}

impl ScriptedMetricReduce for Reduce {
    fn reduce(&self, states: &[Json], params: &Json) -> search_aggs::Result<Json> {
        let s = self.bound.painless().ok_or_else(|| AggError::illegal_argument("not a painless script"))?;
        let mut inst = s.instance();
        inst.set_params(&params.to_serde());
        let list = serde_json::Value::Array(states.iter().map(|j| j.to_serde()).collect());
        inst.set_var("states", search_painless::Value::from_json_map(&list));
        let v = inst.execute(&mut search_painless::NoHost).map_err(script_err)?;
        Ok(Json::from_serde(&v.to_json()))
    }
}

impl ScriptHost for Host {
    fn values(&self, script: &search_aggs::Script) -> search_aggs::Result<Arc<dyn ValuesScript>> {
        // A runtime field aggregated by name.
        if let Some(f) = super::runtime::from_agg_params(&super::aggs::from_json(&script.params)) {
            let table = self.table.clone().ok_or_else(|| AggError::illegal_argument("runtime fields need an index"))?;
            let ev = super::runtime::Evaluator::new(&self.scripts, &self.state, &table, self.shard, &self.index, f).map_err(agg_err)?;
            return Ok(Arc::new(RuntimeValues(ev)));
        }
        let bound = self.compile(script, Context::Aggs)?;
        let table = self.table.clone().ok_or_else(|| AggError::illegal_argument("value scripts need an index"))?;
        Ok(Arc::new(Values { bound, table, shard: self.shard, index: self.index.clone() }))
    }
    fn bucket_script(&self, script: &search_aggs::Script, kind: ScriptKind) -> search_aggs::Result<Arc<dyn BucketScript>> {
        let ctx = if kind == ScriptKind::BucketSelector { Context::AggregationSelector } else { Context::BucketAggregation };
        Ok(Arc::new(Bucket { bound: self.compile(script, ctx)? }))
    }
    fn moving_fn(&self, script: &search_aggs::Script) -> search_aggs::Result<Arc<dyn MovingFnScript>> {
        Ok(Arc::new(MovFn { bound: self.compile(script, Context::MovingFunction)? }))
    }
    fn heuristic(&self, script: &search_aggs::Script) -> search_aggs::Result<Arc<dyn HeuristicScript>> {
        Ok(Arc::new(Heuristic { bound: self.compile(script, Context::ScriptHeuristic)? }))
    }
    fn scripted_metric(&self, init: Option<&search_aggs::Script>, map: &search_aggs::Script, combine: &search_aggs::Script, _params: &Json) -> search_aggs::Result<Arc<dyn ScriptedMetricScripts>> {
        let table = self.table.clone().ok_or_else(|| AggError::illegal_argument("scripted metrics need an index"))?;
        Ok(Arc::new(Scripted {
            init: init.map(|s| self.compile(s, Context::AggsInit)).transpose()?,
            map: self.compile(map, Context::AggsMap)?,
            combine: self.compile(combine, Context::AggsCombine)?,
            table,
            shard: self.shard,
            index: self.index.clone(),
        }))
    }
    fn scripted_metric_reduce(&self, reduce: &search_aggs::Script) -> search_aggs::Result<Arc<dyn ScriptedMetricReduce>> {
        Ok(Arc::new(Reduce { bound: self.compile(reduce, Context::AggsReduce)? }))
    }
}
