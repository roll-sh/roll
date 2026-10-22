//! The bridge to `search-analysis`: building an index's analysis chain from its settings,
//! translating analysis errors into [`EsError`]s, and caching built chains per index.
//!
//! Elasticsearch builds an index's analyzers when the index is created (or its settings change
//! while closed, or a template is validated), so an invalid `index.analysis` definition fails the
//! request with the analysis error. roll does the same through [`index_analysis`]. Building
//! every component of an index is not free (stemmer tables, regexes, synonym maps), so
//! [`AnalysisCache`] keeps the built chain per index and rebuilds it only when the settings it
//! was built from (or the synonym sets) change.
//!
//! `search-analysis` speaks `serde_json`; its settings input is the `index.analysis` group with
//! dotted keys, which is exactly what [`Settings::by_prefix`] gives.

use crate::error::EsError;
use crate::json::{Map, Number, Value};
use crate::meta::settings::{SettingValue, Settings};
use parking_lot::Mutex;
use search_analysis::{Analysis, AnalysisError, IndexOptions, SynonymSets};
use std::collections::HashMap;
use std::sync::Arc;

/// Java exception types analysis code throws (they are not `ElasticsearchException`s, which
/// matters for `root_cause`).
const JAVA_EXCEPTIONS: &[&str] = &[
    "illegal_argument_exception",
    "illegal_state_exception",
    "number_format_exception",
    "unsupported_operation_exception",
    "array_index_out_of_bounds_exception",
    "null_pointer_exception",
    "pattern_syntax_exception",
    "class_cast_exception",
    "runtime_exception",
    "i_o_exception",
];

/// An analysis error as an [`EsError`] (type, reason, status and causes kept).
pub fn es_error(e: AnalysisError) -> EsError {
    let AnalysisError { kind, reason, status, caused_by } = e;
    let mut out = if JAVA_EXCEPTIONS.contains(&kind) { EsError::java(status, kind, reason) } else { EsError::new(status, kind, reason) };
    if let Some(c) = caused_by {
        out = out.caused_by(es_error(*c));
    }
    out
}

/// The `index.analysis` group of `settings` as `search-analysis` reads it (dotted keys).
pub fn analysis_settings_json(settings: &Settings) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for (k, v) in settings.by_prefix("index.analysis.").iter() {
        let v = match v {
            SettingValue::Str(s) => serde_json::Value::String(s.clone()),
            SettingValue::List(l) => serde_json::Value::Array(l.iter().map(|s| serde_json::Value::String(s.clone())).collect()),
            SettingValue::Null => serde_json::Value::Null,
        };
        m.insert(k.clone(), v);
    }
    serde_json::Value::Object(m)
}

fn int_setting(settings: &Settings, key: &str, default: i64) -> i64 {
    settings.get(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// The index-level options analysis components read.
pub fn index_options(settings: &Settings, index: Option<&str>, synonyms: Option<Arc<dyn SynonymSets>>) -> IndexOptions {
    IndexOptions {
        max_ngram_diff: int_setting(settings, "index.max_ngram_diff", 1) as i32,
        max_shingle_diff: int_setting(settings, "index.max_shingle_diff", 3) as i32,
        max_token_count: int_setting(settings, "index.analyze.max_token_count", 10_000).max(0) as usize,
        index_name: index.map(str::to_string),
        synonym_sets: synonyms,
        ..IndexOptions::default()
    }
}

/// Builds an index's analysis from its settings, failing with Elasticsearch's error.
pub fn index_analysis(settings: &Settings, index: Option<&str>, synonyms: Option<Arc<dyn SynonymSets>>) -> Result<Analysis, EsError> {
    let opts = index_options(settings, index, synonyms);
    Analysis::from_settings(&analysis_settings_json(settings), &opts).map_err(es_error)
}

/// Validates the analysis settings of an index (or template) by building them; settings without
/// an `index.analysis` group need no work.
pub fn validate_settings(settings: &Settings, index: Option<&str>) -> Result<(), EsError> {
    if !settings.keys().any(|k| k.starts_with("index.analysis.")) {
        return Ok(());
    }
    index_analysis(settings, index, None).map(|_| ())
}

/// Deprecation warnings Elasticsearch's analysis factories log while an index's chains are
/// built (`edge_ngram`'s `side`).
pub fn deprecation_warnings(settings: &Settings) -> Vec<String> {
    let mut out = Vec::new();
    for (k, v) in settings.iter() {
        let Some(name) = k.strip_prefix("index.analysis.filter.").and_then(|r| r.strip_suffix(".type")) else { continue };
        let ty = v.as_str().unwrap_or_default();
        if matches!(ty, "edge_ngram" | "edgeNGram") && settings.contains(&format!("index.analysis.filter.{name}.side")) {
            let w = "The [side] parameter is deprecated and will be removed. Use a [reverse] before and after the [edge_ngram] instead.".to_string();
            if !out.contains(&w) {
                out.push(w);
            }
        }
    }
    out
}

/// The settings an index's analysis depends on (what the cache compares).
fn analysis_inputs(settings: &Settings) -> Settings {
    let mut s = settings.by_prefix("index.analysis.");
    for k in ["index.max_ngram_diff", "index.max_shingle_diff", "index.analyze.max_token_count"] {
        if let Some(v) = settings.get_value(k) {
            s.insert(k, v.clone());
        }
    }
    s
}

struct Entry {
    inputs: Settings,
    synonyms_generation: u64,
    analysis: Analysis,
}

/// Built analysis chains per index uuid.
#[derive(Default)]
pub struct AnalysisCache {
    entries: Mutex<HashMap<String, Arc<Entry>>>,
}

impl AnalysisCache {
    /// The analysis of an index, built on first use and whenever its analysis settings or the
    /// cluster's synonym sets changed since. `live` gives the uuids of the indices that still
    /// exist; it is called only when the cache has grown past twice `live_count`, to drop the
    /// entries of deleted indices.
    #[allow(clippy::too_many_arguments)]
    pub fn get(
        &self,
        uuid: &str,
        name: &str,
        settings: &Settings,
        synonyms_generation: u64,
        synonyms: Option<Arc<dyn SynonymSets>>,
        live_count: usize,
        live: impl FnOnce() -> std::collections::HashSet<String>,
    ) -> Result<Analysis, EsError> {
        let inputs = analysis_inputs(settings);
        let previous = self.entries.lock().get(uuid).cloned();
        if let Some(e) = &previous
            && e.inputs == inputs
            && e.synonyms_generation == synonyms_generation
        {
            return Ok(e.analysis.clone());
        }
        // Build outside the lock: building can take milliseconds.
        let analysis = match index_analysis(settings, Some(name), synonyms) {
            Ok(a) => a,
            // A synonym set update that doesn't build (a rule the chain eliminates, with
            // `lenient: false`) fails the reload in Elasticsearch, which keeps serving the
            // analyzers it had: keep the previous chain, marked current so it isn't retried.
            Err(_) if previous.as_ref().is_some_and(|p| p.inputs == inputs) => {
                let p = previous.expect("checked");
                let analysis = p.analysis.clone();
                self.entries.lock().insert(uuid.to_string(), Arc::new(Entry { inputs, synonyms_generation, analysis: analysis.clone() }));
                return Ok(analysis);
            }
            Err(e) => return Err(e),
        };
        let mut entries = self.entries.lock();
        if entries.len() >= 2 * live_count.max(8) {
            let live = live();
            entries.retain(|k, _| live.contains(k));
        }
        entries.insert(uuid.to_string(), Arc::new(Entry { inputs, synonyms_generation, analysis: analysis.clone() }));
        Ok(analysis)
    }

    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drops the chain of one index (its shards closed: a reopen builds from scratch, as
    /// Elasticsearch's shards do).
    pub fn forget(&self, uuid: &str) {
        self.entries.lock().remove(uuid);
    }

    pub fn clear(&self) {
        self.entries.lock().clear();
    }
}

/// Keys of `_analyze` output objects in Elasticsearch's order; others follow alphabetically
/// (token attributes are printed from a sorted map).
const KEY_ORDER: &[&str] = &[
    "detail",
    "custom_analyzer",
    "analyzer",
    "charfilters",
    "tokenizer",
    "tokenfilters",
    "name",
    "filtered_text",
    "tokens",
    "token",
    "start_offset",
    "end_offset",
    "type",
    "position",
];

/// Converts `search-analysis` JSON output into roll's ordered JSON, with Elasticsearch's key
/// order for `_analyze` responses (`serde_json` maps are sorted).
pub fn analyze_output(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => Value::Num(if let Some(i) = n.as_i64() {
            Number::Int(i)
        } else if let Some(u) = n.as_u64() {
            Number::UInt(u)
        } else {
            Number::Float(n.as_f64().unwrap_or(0.0))
        }),
        serde_json::Value::String(s) => Value::Str(s.clone()),
        serde_json::Value::Array(a) => Value::Arr(a.iter().map(analyze_output).collect()),
        serde_json::Value::Object(o) => {
            let mut m = Map::with_capacity(o.len());
            for k in KEY_ORDER {
                if let Some(v) = o.get(*k) {
                    m.insert((*k).to_string(), analyze_output(v));
                }
            }
            for (k, v) in o {
                if !KEY_ORDER.contains(&k.as_str()) {
                    m.insert(k.clone(), analyze_output(v));
                }
            }
            Value::Obj(m)
        }
    }
}

/// roll JSON to `serde_json` (inline component definitions of `_analyze`).
pub fn to_serde(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Num(n) => match n {
            Number::Int(i) => serde_json::Value::from(*i),
            Number::UInt(u) => serde_json::Value::from(*u),
            Number::Big(s) => serde_json::Value::String(s.to_string()),
            Number::Float(f) => serde_json::Number::from_f64(*f).map(serde_json::Value::Number).unwrap_or(serde_json::Value::Null),
        },
        Value::Str(s) => serde_json::Value::String(s.clone()),
        Value::Arr(a) => serde_json::Value::Array(a.iter().map(to_serde).collect()),
        Value::Obj(m) => serde_json::Value::Object(m.iter().map(|(k, v)| (k.clone(), to_serde(v))).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(json: &str) -> Settings {
        let mut s = Settings::from_json(&crate::json::parse(json.as_bytes()).unwrap().unwrap()).unwrap();
        s.normalize_prefix("index.");
        s
    }

    #[test]
    fn invalid_definitions_fail_with_elasticsearch_texts() {
        let e = index_analysis(&settings(r#"{"analysis":{"analyzer":{"a":{"type":"custom","tokenizer":"nope"}}}}"#), Some("i"), None).err().unwrap();
        assert_eq!(e.kind, "illegal_argument_exception");
        assert_eq!(e.reason(), "Custom Analyzer [a] failed to find tokenizer under name [nope]");
        assert!(!e.es);
        let e = index_analysis(&settings(r#"{"analysis":{"filter":{"f":{"type":"nope"}}}}"#), Some("i"), None).err().unwrap();
        assert_eq!(e.reason(), "Unknown filter type [nope] for [f]");
        let e = index_analysis(&settings(r#"{"analysis":{"analyzer":{"a":{"tokenizer":"standard","filter":["nope"]}}}}"#), Some("i"), None).err().unwrap();
        assert_eq!(e.reason(), "Custom Analyzer [a] failed to find filter under name [nope]");
    }

    #[test]
    fn cache_rebuilds_only_on_change() {
        let cache = AnalysisCache::default();
        let s1 = settings(r#"{"analysis":{"analyzer":{"a":{"type":"custom","tokenizer":"whitespace"}}}}"#);
        let all = || ["u1".to_string()].into_iter().collect();
        let a1 = cache.get("u1", "i", &s1, 0, None, 1, all).unwrap();
        let a2 = cache.get("u1", "i", &s1, 0, None, 1, all).unwrap();
        assert!(a1.analyzer("a").is_some() && a2.analyzer("a").is_some());
        assert_eq!(cache.len(), 1);
        let s2 = settings(r#"{"analysis":{"analyzer":{"b":{"type":"custom","tokenizer":"whitespace"}}}}"#);
        let a3 = cache.get("u1", "i", &s2, 0, None, 1, all).unwrap();
        assert!(a3.analyzer("b").is_some());
        // Entries of deleted indices are dropped once the cache outgrows the live indices.
        for i in 0..20 {
            cache.get(&format!("x{i}"), "j", &s2, 0, None, 1, || ["x19".to_string()].into_iter().collect()).unwrap();
        }
        assert!(cache.len() < 20);
    }

    #[test]
    fn output_keys_follow_elasticsearch_order() {
        let v = serde_json::json!({"tokens":[{"type":"word","token":"a","position":0,"end_offset":1,"start_offset":0,"positionLength":2}]});
        assert_eq!(
            analyze_output(&v).to_json(),
            r#"{"tokens":[{"token":"a","start_offset":0,"end_offset":1,"type":"word","position":0,"positionLength":2}]}"#
        );
    }
}
