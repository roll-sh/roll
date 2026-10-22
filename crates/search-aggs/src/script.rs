//! Scripts in aggregations: the parsed `script` object (Elasticsearch's `Script`) and the hooks
//! a script engine implements ([`ScriptHost`]; `search-es` binds Painless).
//!
//! Contexts, as Elasticsearch names them:
//! - `aggs` ([`ValuesScript`]): per-document values for a values source — a script instead of
//!   a `field`, or a value script over a field's values (`_value`).
//! - `aggs_init` / `aggs_map` / `aggs_combine` / `aggs_reduce` ([`ScriptedMetricScripts`]):
//!   `scripted_metric`.
//! - `bucket_aggregation` / `aggregation_selector` ([`BucketScript`]): `bucket_script` and
//!   `bucket_selector`, run at the final reduce with the `buckets_path` values as variables.
//! - `moving-function` ([`MovingFnScript`]): `moving_fn`.
//! - `script_heuristic` ([`HeuristicScript`]): significance scores.
//!
//! Shard-side scripts are compiled through [`FieldLookup`](crate::FieldLookup); reduce-side
//! ones through [`ReduceOptions::scripts`](crate::ReduceOptions).

use std::sync::Arc;

use search_index::SegmentContext;

use crate::error::{AggError, Result};
use crate::json::Json;
use crate::xparser::{Field, Token, Tokens, XParser};

/// A parsed `script` (inline source or stored id).
#[derive(Clone, Debug, PartialEq)]
pub struct Script {
    /// Inline source (`source`, or the string form).
    pub source: Option<String>,
    /// Stored script id (`id`).
    pub id: Option<String>,
    /// `lang`; `painless` by default for inline scripts, `None` for stored ones.
    pub lang: Option<String>,
    /// `params` (an object; empty when absent).
    pub params: Json,
    /// `options` (compiler options).
    pub options: Vec<(String, String)>,
}

impl Script {
    /// An inline Painless script.
    pub fn inline(source: impl Into<String>) -> Script {
        Script { source: Some(source.into()), id: None, lang: Some("painless".into()), params: Json::obj(), options: Vec::new() }
    }

    /// `Script.parse(parser)`: a string (inline Painless) or the `script` object.
    pub fn parse(p: &mut XParser<'_>) -> Result<Script> {
        if p.token() == Token::String {
            return Ok(Script::inline(p.text()?));
        }
        let mut source: Option<String> = None;
        let mut id: Option<String> = None;
        let mut lang: Option<String> = None;
        let mut params = Json::obj();
        let mut options: Vec<(String, String)> = Vec::new();
        let mut kind: Option<bool> = None; // Some(true) = inline
        const FIELDS: &[Field] = &[
            Field::new("source", Tokens::OBJECT_OR_STRING),
            Field::new("id", Tokens::STRING),
            Field::new("lang", Tokens::STRING),
            Field::new("options", Tokens::OBJECT),
            Field::new("params", Tokens::OBJECT),
        ];
        let only_one = || AggError::illegal_argument("must only use one of [source, id] when specifying a script");
        p.object("script", FIELDS, |p, i| {
            match i {
                0 => {
                    if kind.is_some() {
                        return Err(only_one());
                    }
                    kind = Some(true);
                    if p.token() == Token::StartObject {
                        let v = p.take_value();
                        source = Some(v.to_json_string());
                        options.push(("content_type".into(), "application/json".into()));
                    } else {
                        source = Some(p.text()?);
                    }
                }
                1 => {
                    if kind.is_some() {
                        return Err(only_one());
                    }
                    kind = Some(false);
                    id = Some(p.text()?);
                }
                2 => lang = Some(p.text()?),
                3 => {
                    let m = p.map()?;
                    if let Json::Obj(m) = m {
                        for (k, v) in m {
                            let v = match v {
                                Json::Str(s) => s,
                                other => other.to_java_string(),
                            };
                            options.push((k, v));
                        }
                    }
                }
                _ => params = p.map()?,
            }
            Ok(())
        })?;
        match kind {
            None => Err(AggError::illegal_argument("must specify either [source] for an inline script or [id] for a stored script")),
            Some(true) => {
                let extra: Vec<&(String, String)> = options.iter().filter(|(k, _)| k != "content_type").collect();
                if options.len() > 1 || (options.len() == 1 && !extra.is_empty()) {
                    let shown: Vec<String> = extra.iter().map(|(k, v)| format!("{k}={v}")).collect();
                    return Err(AggError::illegal_argument(format!("illegal compiler options [{{{}}}] specified", shown.join(", "))));
                }
                Ok(Script { source, id: None, lang: Some(lang.unwrap_or_else(|| "painless".into())), params, options })
            }
            Some(false) => {
                if lang.is_some() {
                    return Err(AggError::illegal_argument("illegally specified <lang> for a stored script"));
                }
                if !options.is_empty() {
                    return Err(AggError::illegal_argument("field [options] cannot be specified using a stored script"));
                }
                Ok(Script { source: None, id, lang: None, params, options })
            }
        }
    }

    /// The script as `Script.toXContent` prints it.
    pub fn to_json(&self) -> Json {
        let mut o = Json::obj();
        if let Some(s) = &self.source {
            o.push("source", Json::str(s.clone()));
        }
        if let Some(id) = &self.id {
            o.push("id", Json::str(id.clone()));
        }
        if let Some(l) = &self.lang {
            o.push("lang", Json::str(l.clone()));
        }
        if !self.options.is_empty() {
            o.push("options", Json::Obj(self.options.iter().map(|(k, v)| (k.clone(), Json::str(v.clone()))).collect()));
        }
        if self.params.as_object().is_some_and(|m| !m.is_empty()) {
            o.push("params", self.params.clone());
        }
        o
    }
}

/// The aggregation script contexts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScriptKind {
    /// `aggs`: values per document (value scripts get `_value`).
    Aggs,
    AggsInit,
    AggsMap,
    AggsCombine,
    AggsReduce,
    /// `bucket_aggregation` (`bucket_script`).
    BucketScript,
    /// `aggregation_selector` (`bucket_selector`).
    BucketSelector,
    /// `moving-function`.
    MovingFunction,
    /// `script_heuristic`.
    Heuristic,
}

/// A value a script produced (Painless `def` results the aggregations understand).
#[derive(Clone, Debug, PartialEq)]
pub enum ScriptValue {
    Null,
    Bool(bool),
    Long(i64),
    Double(f64),
    Str(String),
    /// A date (`ZonedDateTime` / `JodaCompatibleZonedDateTime`): epoch millis.
    Date(i64),
    /// A geo point (`lat`, `lon`).
    GeoPoint(f64, f64),
}

impl ScriptValue {
    /// The value as a double (`Number.doubleValue()`; booleans as 0/1, dates as millis).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            ScriptValue::Long(l) => Some(*l as f64),
            ScriptValue::Double(d) => Some(*d),
            ScriptValue::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            ScriptValue::Date(d) => Some(*d as f64),
            _ => None,
        }
    }

    /// The value's `toString()`.
    pub fn to_java_string(&self) -> String {
        match self {
            ScriptValue::Null => "null".into(),
            ScriptValue::Bool(b) => b.to_string(),
            ScriptValue::Long(l) => l.to_string(),
            ScriptValue::Double(d) => crate::json::java_double(*d),
            ScriptValue::Str(s) => s.clone(),
            ScriptValue::Date(d) => d.to_string(),
            ScriptValue::GeoPoint(lat, lon) => format!("{lat}, {lon}"),
        }
    }
}

/// An `aggs` script compiled for one shard.
pub trait ValuesScript: Send + Sync {
    /// Whether the script reads `_score`.
    fn needs_score(&self) -> bool {
        false
    }
    /// A per-segment instance (reused for every doc of the segment).
    fn leaf(&self, seg: &SegmentContext) -> Result<Box<dyn LeafValuesScript>>;
}

/// An `aggs` script bound to a segment.
pub trait LeafValuesScript: Send {
    /// Runs the script for `doc` (`_score` = `score`; `_value` = `value` for value scripts) and
    /// appends its value(s) to `out` (a returned collection appends each element).
    fn run(&mut self, doc: u32, score: f32, value: Option<&ScriptValue>, out: &mut Vec<ScriptValue>) -> Result<()>;
}

/// `bucket_script` / `bucket_selector` scripts (reduce side).
pub trait BucketScript: Send + Sync {
    /// Runs with `vars` (the `buckets_path` values: doubles, or `null`/NaN for gaps, plus the
    /// user `params`) and returns the script's result (a number for `bucket_script`, a
    /// boolean for `bucket_selector`).
    fn run(&self, vars: &[(String, Json)]) -> Result<Json>;
}

/// `moving_fn` scripts.
pub trait MovingFnScript: Send + Sync {
    /// `values` is the window (doubles, gaps skipped); `params` the user params.
    fn run(&self, values: &[f64], params: &Json) -> Result<f64>;
}

/// `script_heuristic` scripts.
pub trait HeuristicScript: Send + Sync {
    fn score(&self, subset_freq: i64, subset_size: i64, superset_freq: i64, superset_size: i64) -> Result<f64>;
}

/// `scripted_metric`'s shard-side scripts.
pub trait ScriptedMetricScripts: Send + Sync {
    /// Runs `init_script` (if any) for a new bucket and returns its state.
    fn new_state(&self, params: &Json) -> Result<Box<dyn ScriptedMetricState>>;
    /// Whether `map_script` reads `_score`.
    fn needs_score(&self) -> bool {
        false
    }
}

/// One bucket's `scripted_metric` state on a shard.
pub trait ScriptedMetricState: Send {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()>;
    /// Runs `map_script` for `doc`.
    fn map(&mut self, doc: u32, score: f32) -> Result<()>;
    /// Runs `combine_script` and returns the shard's result for the bucket.
    fn combine(&mut self) -> Result<Json>;
}

/// `scripted_metric`'s `reduce_script` (reduce side).
pub trait ScriptedMetricReduce: Send + Sync {
    /// `states` are the shards' combined results; returns the aggregation's `value`.
    fn reduce(&self, states: &[Json], params: &Json) -> Result<Json>;
}

/// A script engine for aggregations.
pub trait ScriptHost: Send + Sync {
    fn values(&self, script: &Script) -> Result<Arc<dyn ValuesScript>>;
    fn bucket_script(&self, script: &Script, kind: ScriptKind) -> Result<Arc<dyn BucketScript>> {
        let _ = (script, kind);
        Err(unsupported())
    }
    fn moving_fn(&self, script: &Script) -> Result<Arc<dyn MovingFnScript>> {
        let _ = script;
        Err(unsupported())
    }
    fn heuristic(&self, script: &Script) -> Result<Arc<dyn HeuristicScript>> {
        let _ = script;
        Err(unsupported())
    }
    fn scripted_metric(
        &self,
        init: Option<&Script>,
        map: &Script,
        combine: &Script,
        params: &Json,
    ) -> Result<Arc<dyn ScriptedMetricScripts>> {
        let _ = (init, map, combine, params);
        Err(unsupported())
    }
    fn scripted_metric_reduce(&self, reduce: &Script) -> Result<Arc<dyn ScriptedMetricReduce>> {
        let _ = reduce;
        Err(unsupported())
    }
}

fn unsupported() -> AggError {
    AggError::illegal_argument("scripts are not supported")
}

/// What [`FieldLookup::script`](crate::FieldLookup::script) returns.
pub type AggScript = Arc<dyn ValuesScript>;

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Result<Script> {
        let j = Json::parse(src).unwrap();
        let mut p = XParser::new(&j, None, Vec::new());
        p.next_token();
        Script::parse(&mut p)
    }

    #[test]
    fn script_forms() {
        assert_eq!(parse(r#""doc['a'].value""#).unwrap().source.as_deref(), Some("doc['a'].value"));
        let s = parse(r#"{"source":"x","params":{"a":1}}"#).unwrap();
        assert_eq!(s.lang.as_deref(), Some("painless"));
        assert_eq!(s.params.get("a"), Some(&Json::Int(1)));
        assert_eq!(parse(r#"{"source":"1","id":"x"}"#).unwrap_err().caused_by.unwrap().reason, "must only use one of [source, id] when specifying a script");
        assert_eq!(parse(r#"{"lang":"painless"}"#).unwrap_err().reason, "must specify either [source] for an inline script or [id] for a stored script");
        assert!(parse(r#"{"sourcex":"1"}"#).unwrap_err().reason.contains("unknown field [sourcex] did you mean [source]?"));
    }
}
