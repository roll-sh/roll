//! `scripted_metric`: user scripts build the metric — `init_script` creates each bucket's
//! `state`, `map_script` runs per document, `combine_script` turns a shard's state into the
//! shard result and `reduce_script` merges the shard results into the final `value`.
//!
//! The script engine sits behind [`ScriptHost`](crate::ScriptHost) (`scripted_metric` and
//! `scripted_metric_reduce`); this module owns everything around it: parsing, the required
//! scripts, the agg-level `params` merged into every script's own params (with Elasticsearch's
//! conflict error), one state per owning bucket (buckets that never saw a document still run
//! `init` + `combine`, like Elasticsearch's fresh `State`), and the reduce, which concatenates
//! shard results until the final reduce runs `reduce_script`.
//!
//! Sub-aggregations are accepted and ignored, as in Elasticsearch (the aggregator is built
//! without sub-factories).

use std::sync::Arc;

use search_index::SegmentContext;

use crate::collect::{Aggregator, Batch, BindCtx};
use crate::error::{AggError, Result};
use crate::internal::{AggValue, Internal, Prop, ReduceCtx, RenderCtx, java_list};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::request::{AggDef, PathElement};
use crate::script::{Script, ScriptedMetricScripts, ScriptedMetricState};
use crate::xparser::{Field, Tokens, XParser};

/// A parsed `scripted_metric`.
#[derive(Clone, Debug)]
pub struct ScriptedMetricSpec {
    pub init: Option<Script>,
    pub map: Script,
    pub combine: Option<Script>,
    pub reduce: Option<Script>,
    /// Agg-level `params` (`None` when absent).
    pub params: Option<Json>,
}

const FIELDS: &[Field] = &[
    Field::new("init_script", Tokens::OBJECT_OR_STRING),
    Field::new("map_script", Tokens::OBJECT_OR_STRING),
    Field::new("combine_script", Tokens::OBJECT_OR_STRING),
    Field::new("reduce_script", Tokens::OBJECT_OR_STRING),
    Field::new("params", Tokens::OBJECT),
];

/// `ScriptedMetricAggregationBuilder.PARSER` (a `ConstructingObjectParser`: `map_script` is the
/// constructor argument).
pub fn parse(p: &mut XParser<'_>) -> Result<ScriptedMetricSpec> {
    let (mut init, mut map, mut combine, mut reduce, mut params) = (None, None, None, None, None);
    p.object("scripted_metric", FIELDS, |p, i| {
        match i {
            0 => init = Some(Script::parse(p)?),
            1 => map = Some(Script::parse(p)?),
            2 => combine = Some(Script::parse(p)?),
            3 => reduce = Some(Script::parse(p)?),
            _ => params = Some(p.map()?),
        }
        Ok(())
    })?;
    let Some(map) = map else {
        return Err(AggError::illegal_argument("Required [map_script]"));
    };
    Ok(ScriptedMetricSpec { init, map, combine, reduce, params })
}

/// `ScriptedMetricAggregatorFactory.mergeParams(agg, script)`.
fn merge_params(agg: &Json, script: &Json) -> Result<Json> {
    let mut combined: Vec<(String, Json)> = script.as_object().map(|o| o.to_vec()).unwrap_or_default();
    for (k, v) in agg.as_object().unwrap_or(&[]) {
        if combined.iter().any(|(c, _)| c == k) {
            return Err(AggError::illegal_argument(format!("Parameter name \"{k}\" used in both aggregation and script parameters")));
        }
        combined.push((k.clone(), v.clone()));
    }
    Ok(Json::Obj(combined))
}

fn with_merged(script: &Script, agg: &Json) -> Result<Script> {
    Ok(Script { params: merge_params(agg, &script.params)?, ..script.clone() })
}

/// Binds `scripted_metric`.
pub fn create(def: &AggDef, spec: &ScriptedMetricSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let Some(combine) = &spec.combine else {
        return Err(AggError::illegal_argument(format!("[combine_script] must not be null: [{}]", def.name)));
    };
    let Some(reduce) = &spec.reduce else {
        return Err(AggError::illegal_argument(format!("[reduce_script] must not be null: [{}]", def.name)));
    };
    let agg_params = spec.params.clone().unwrap_or_else(Json::obj);
    // `createInternal`: the reduce script carries the merged params to the reduce phase; then
    // each `State` merges them into the map, combine and init params.
    let reduce = Arc::new(with_merged(reduce, &agg_params)?);
    let map = with_merged(&spec.map, &agg_params)?;
    let combine = with_merged(combine, &agg_params)?;
    let init = spec.init.as_ref().map(|s| with_merged(s, &agg_params)).transpose()?;
    let host = ctx.lookup.scripts().ok_or_else(|| AggError::illegal_argument("scripts are not supported"))?;
    let scripts = host.scripted_metric(init.as_ref(), &map, &combine, &agg_params)?;
    Ok(Box::new(ScriptedMetricAgg { scripts, agg_params, reduce, states: Vec::new(), live: 0, seg: None, tracker: ctx.tracker(def) }))
}

/// `ScriptedMetricAggregator.BUCKET_COST_ESTIMATE`.
const BUCKET_COST: usize = 5 * 1024;

struct ScriptedMetricAgg {
    scripts: Arc<dyn ScriptedMetricScripts>,
    agg_params: Json,
    reduce: Arc<Script>,
    /// Per owning ordinal: the state and whether it is bound to the current segment.
    states: Vec<Option<(Box<dyn ScriptedMetricState>, bool)>>,
    /// Number of states created (each charged `BUCKET_COST`).
    live: usize,
    seg: Option<SegmentContext>,
    tracker: MemTracker,
}

impl ScriptedMetricAgg {
    fn result(&self, value: Json) -> AggValue {
        AggValue::ScriptedMetric(Box::new(ScriptedMetric { values: vec![value], reduce: self.reduce.clone() }))
    }
}

impl Aggregator for ScriptedMetricAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        // Leaf map scripts are rebuilt lazily for the buckets the segment touches.
        for (_, bound) in self.states.iter_mut().flatten() {
            *bound = false;
        }
        self.seg = Some(seg.clone());
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        let Some(seg) = &self.seg else { return Ok(()) };
        for i in 0..b.len() {
            let ord = b.ords[i] as usize;
            if self.states.len() <= ord {
                self.states.resize_with(ord + 1, || None);
            }
            if self.states[ord].is_none() {
                self.live += 1;
                self.tracker.ensure(self.live * BUCKET_COST + self.states.capacity() * 16)?;
                self.states[ord] = Some((self.scripts.new_state(&self.agg_params)?, false));
            }
            let Some((state, bound)) = self.states[ord].as_mut() else { continue };
            if !*bound {
                state.set_segment(seg)?;
                *bound = true;
            }
            state.map(b.docs[i], b.score(i))?;
        }
        Ok(())
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        let mut out = Vec::with_capacity(owning.len());
        for &o in owning {
            let combined = match self.states.get_mut(o as usize).and_then(Option::as_mut) {
                Some((state, _)) => state.combine()?,
                // A fresh `State`: init, then combine.
                None => self.scripts.new_state(&self.agg_params)?.combine()?,
            };
            out.push(self.result(combined));
        }
        Ok(out)
    }

    fn build_empty(&self) -> AggValue {
        self.result(Json::Null)
    }

    fn needs_scores(&self) -> bool {
        self.scripts.needs_score()
    }

    fn validate_sort(&self, name: &str, _key: Option<&str>, _rest: &[PathElement]) -> Result<()> {
        Err(AggError::illegal_argument(format!(
            "Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end. [{name}] is not single-bucket."
        )))
    }
}

/// `InternalScriptedMetric`: the shard results (one per shard until the final reduce, which
/// leaves the reduce script's single result).
#[derive(Clone, Debug)]
pub struct ScriptedMetric {
    pub values: Vec<Json>,
    /// The reduce script, its params merged with the agg-level params.
    pub reduce: Arc<Script>,
}

impl ScriptedMetric {
    /// `aggregation()`.
    fn value(&self) -> Result<&Json> {
        if self.values.len() != 1 {
            return Err(AggError::new(500, "illegal_state_exception", "aggregation was not reduced"));
        }
        Ok(&self.values[0])
    }
}

impl Internal for ScriptedMetric {
    fn type_name(&self) -> &'static str {
        "scripted_metric"
    }
    fn class_name(&self) -> &'static str {
        "InternalScriptedMetric"
    }
    fn must_reduce_single(&self) -> bool {
        true
    }
    fn reduce(list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let reduce = list[0].reduce.clone();
        let states: Vec<Json> = list.into_iter().flat_map(|a| a.values).collect();
        if !ctx.is_final() {
            return Ok(ScriptedMetric { values: states, reduce });
        }
        let host = ctx.opts.scripts.clone().ok_or_else(|| AggError::illegal_argument("scripts are not supported"))?;
        let script = host.scripted_metric_reduce(&reduce)?;
        let value = script.reduce(&states, &reduce.params)?;
        Ok(ScriptedMetric { values: vec![value], reduce })
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        // An unreduced result can't be rendered by Elasticsearch either (`IllegalStateException`);
        // every rendered result went through the final reduce.
        out.push("value", self.value().cloned().unwrap_or(Json::Null));
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        if path.len() == 1 && path[0] == "value" {
            return Ok(match self.value()? {
                Json::Int(i) => Prop::Num(*i as f64),
                Json::UInt(u) => Prop::Num(*u as f64),
                Json::Float(f) => Prop::Num(*f),
                Json::Float32(f) => Prop::Num(*f as f64),
                other => Prop::Value(other.clone()),
            });
        }
        Err(AggError::illegal_argument(format!("path not supported for [{name}]: {}", java_list(path))))
    }
    fn ram_bytes(&self) -> usize {
        64 + self.values.len() * 256
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_merge_rejects_conflicts() {
        let agg = Json::parse(r#"{"a":1,"b":2}"#).unwrap();
        let merged = merge_params(&agg, &Json::parse(r#"{"c":3}"#).unwrap()).unwrap();
        assert_eq!(merged.get("a"), Some(&Json::Int(1)));
        assert_eq!(merged.get("c"), Some(&Json::Int(3)));
        let e = merge_params(&agg, &Json::parse(r#"{"b":0}"#).unwrap()).unwrap_err();
        assert_eq!(e.reason, "Parameter name \"b\" used in both aggregation and script parameters");
    }
}
