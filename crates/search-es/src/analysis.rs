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
//! A chain keeps the synonym rules it loaded until something reloads it, as Elasticsearch's
//! analyzers do (a set changed with `refresh=false`, or replaced by rules the chain can't build,
//! leaves them as they were). The cache records those rules ([`AnalysisCache::loaded_synonyms`])
//! so a durable server can carry them across an unload ([`AnalysisCache::seed_loaded_synonyms`]):
//! a wake is not a restart.
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
        scripts: Some(crate::scripting::analysis_scripts()),
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
    /// The synonym sets the chain loaded, with their rules then (`None`: the set was missing).
    synonyms: Arc<LoadedSynonyms>,
}

/// Synonym sets an analysis chain loaded, by name, with the rules it loaded (`None`: missing).
pub type LoadedSynonyms = std::collections::BTreeMap<String, Option<Vec<String>>>;

/// Records which sets a build reads; answers from `seed` first (the rules a chain had loaded
/// before an unload).
struct Recording {
    inner: Arc<dyn SynonymSets>,
    seed: Option<Arc<LoadedSynonyms>>,
    read: Mutex<LoadedSynonyms>,
}

impl SynonymSets for Recording {
    fn rules(&self, set: &str) -> Option<Vec<String>> {
        let rules = match self.seed.as_ref().and_then(|s| s.get(set)) {
            Some(seeded) => seeded.clone(),
            None => self.inner.rules(set),
        };
        self.read.lock().insert(set.to_string(), rules.clone());
        rules
    }
}

/// Built analysis chains per index uuid.
#[derive(Default)]
pub struct AnalysisCache {
    entries: Mutex<HashMap<String, Arc<Entry>>>,
    /// Synonym rules to build an index's next chain with, valid at a synonyms generation
    /// ([`AnalysisCache::seed_loaded_synonyms`]).
    seeds: Mutex<HashMap<String, (u64, Arc<LoadedSynonyms>)>>,
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
        // A chain an unload left with rules the sets no longer have: build it with those, as
        // long as nothing reloaded it since.
        let seed = match previous {
            None => self.seeds.lock().remove(uuid).filter(|(g, _)| *g == synonyms_generation).map(|(_, s)| s),
            Some(_) => None,
        };
        let recording = synonyms.map(|inner| Arc::new(Recording { inner, seed, read: Mutex::new(LoadedSynonyms::new()) }));
        // Build outside the lock: building can take milliseconds.
        let built = index_analysis(settings, Some(name), recording.clone().map(|r| r as Arc<dyn SynonymSets>));
        let loaded = Arc::new(recording.map(|r| std::mem::take(&mut *r.read.lock())).unwrap_or_default());
        let analysis = match built {
            Ok(a) => a,
            // A synonym set update that doesn't build (a rule the chain eliminates, with
            // `lenient: false`) fails the reload in Elasticsearch, which keeps serving the
            // analyzers it had: keep the previous chain, marked current so it isn't retried.
            Err(_) if previous.as_ref().is_some_and(|p| p.inputs == inputs) => {
                let p = previous.expect("checked");
                let analysis = p.analysis.clone();
                self.entries.lock().insert(uuid.to_string(), Arc::new(Entry { inputs, synonyms_generation, analysis: analysis.clone(), synonyms: p.synonyms.clone() }));
                return Ok(analysis);
            }
            Err(e) => return Err(e),
        };
        let mut entries = self.entries.lock();
        if entries.len() >= 2 * live_count.max(8) {
            let live = live();
            entries.retain(|k, _| live.contains(k));
        }
        entries.insert(uuid.to_string(), Arc::new(Entry { inputs, synonyms_generation, analysis: analysis.clone(), synonyms: loaded }));
        Ok(analysis)
    }

    /// The synonym sets the cached chain of an index loaded, with the synonyms generation it is
    /// current for.
    pub fn loaded_synonyms(&self, uuid: &str) -> Option<(u64, Arc<LoadedSynonyms>)> {
        let e = self.entries.lock().get(uuid).cloned()?;
        Some((e.synonyms_generation, e.synonyms.clone()))
    }

    /// Builds the next chain of an index (none is cached) with these rules for the sets they
    /// name, if the synonyms generation is still `generation` then: what
    /// [`AnalysisCache::loaded_synonyms`] reported before an unload.
    pub fn seed_loaded_synonyms(&self, uuid: &str, generation: u64, sets: LoadedSynonyms) {
        self.seeds.lock().insert(uuid.to_string(), (generation, Arc::new(sets)));
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
        self.seeds.lock().remove(uuid);
    }

    pub fn clear(&self) {
        self.entries.lock().clear();
        self.seeds.lock().clear();
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
    fn chains_keep_the_synonym_rules_they_loaded_across_a_seed() {
        struct Sets(Mutex<HashMap<String, Vec<String>>>);
        impl SynonymSets for Sets {
            fn rules(&self, set: &str) -> Option<Vec<String>> {
                self.0.lock().get(set).cloned()
            }
        }
        let tk = "tokenizer";
        let s = settings(&format!(
            r#"{{"analysis":{{"filter":{{"syn":{{"type":"synonym","synonyms_set":"set1","updateable":true}}}},"analyzer":{{"a":{{"type":"custom","{tk}":"whitespace","filter":["syn"]}}}}}}}}"#
        ));
        let sets = Arc::new(Sets(Mutex::new([("set1".to_string(), vec!["foo => bar".to_string()])].into_iter().collect())));
        let all = || ["u1".to_string()].into_iter().collect();
        let cache = AnalysisCache::default();
        cache.get("u1", "i", &s, 3, Some(sets.clone()), 1, all).unwrap();
        let (generation, loaded) = cache.loaded_synonyms("u1").unwrap();
        assert_eq!(generation, 3);
        assert_eq!(*loaded, [("set1".to_string(), Some(vec!["foo => bar".to_string()]))].into_iter().collect::<LoadedSynonyms>());
        // The set changes without a reload; a fresh cache (a wake) seeded with what the chain
        // had loaded builds with those rules while the generation is the same…
        sets.0.lock().insert("set1".into(), vec!["foo => baz".into()]);
        let woken = AnalysisCache::default();
        woken.seed_loaded_synonyms("u1", 3, (*loaded).clone());
        woken.get("u1", "i", &s, 3, Some(sets.clone()), 1, all).unwrap();
        assert_eq!(woken.loaded_synonyms("u1").unwrap().1, loaded);
        // …and a reload (a new generation) loads the current rules.
        woken.get("u1", "i", &s, 4, Some(sets.clone()), 1, all).unwrap();
        assert_eq!(woken.loaded_synonyms("u1").unwrap().1.get("set1"), Some(&Some(vec!["foo => baz".to_string()])));
        // A seed for an older generation, or for an index that closed since, is ignored.
        let stale = AnalysisCache::default();
        stale.seed_loaded_synonyms("u1", 2, (*loaded).clone());
        stale.get("u1", "i", &s, 3, Some(sets.clone()), 1, all).unwrap();
        assert_eq!(stale.loaded_synonyms("u1").unwrap().1.get("set1"), Some(&Some(vec!["foo => baz".to_string()])));
        let closed = AnalysisCache::default();
        closed.seed_loaded_synonyms("u1", 3, (*loaded).clone());
        closed.forget("u1");
        closed.get("u1", "i", &s, 3, Some(sets), 1, all).unwrap();
        assert_eq!(closed.loaded_synonyms("u1").unwrap().1.get("set1"), Some(&Some(vec!["foo => baz".to_string()])));
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
