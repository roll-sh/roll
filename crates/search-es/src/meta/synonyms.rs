//! Synonym sets managed through the synonyms API (`/_synonyms`).
//!
//! Elasticsearch keeps synonym sets as documents in the `.synonyms` system index; roll keeps
//! them in the cluster metadata, which gives the same semantics (rules read back sorted by id,
//! sets listed sorted by name, a set exists even when empty) without a hidden index. Analysis
//! chains read them through [`StateSynonyms`], the `search-analysis` [`SynonymSets`] hook.
//!
//! Rule validation follows `SynonymRule.validate` (including its escape-aware split), so the
//! same rule texts are accepted and rejected with the same messages.

use super::{ClusterState, IndexMeta};
use crate::json::Value;
use crate::obj;
use search_analysis::SynonymSets;
use std::collections::BTreeMap;
use std::sync::Arc;

/// `synonyms.max_synonym_rules` default.
pub const DEFAULT_MAX_RULES: i64 = 100_000;

/// Pages of `GET _synonyms…` are limited to this many results (`from + size`).
pub const MAX_RESULTS: i64 = 10_000;

/// One synonym set: rule id → rule text, sorted by id as Elasticsearch returns them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SynonymSet {
    pub rules: BTreeMap<String, String>,
}

impl SynonymSet {
    pub fn to_value(&self) -> Value {
        let mut m = crate::json::Map::new();
        for (id, s) in &self.rules {
            m.insert(id.clone(), Value::from(s.clone()));
        }
        Value::Obj(m)
    }

    pub fn from_value(v: &Value) -> SynonymSet {
        let mut rules = BTreeMap::new();
        if let Some(m) = v.as_object() {
            for (id, s) in m {
                rules.insert(id.clone(), s.to_es_string());
            }
        }
        SynonymSet { rules }
    }
}

/// All synonym sets of a cluster.
pub type Sets = BTreeMap<String, Arc<SynonymSet>>;

/// `SynonymRule.split`: splits on `sep`, keeping backslash escapes, dropping empty pieces.
fn split<'a>(s: &'a str, sep: &str) -> Vec<std::borrow::Cow<'a, str>> {
    let mut out = Vec::with_capacity(2);
    let mut cur = String::new();
    let mut i = 0;
    let b = s.as_bytes();
    while i < b.len() {
        if s[i..].starts_with(sep) {
            if !cur.is_empty() {
                out.push(std::borrow::Cow::Owned(std::mem::take(&mut cur)));
            }
            i += sep.len();
            continue;
        }
        let ch = s[i..].chars().next().expect("in bounds");
        i += ch.len_utf8();
        if ch == '\\' {
            cur.push(ch);
            let Some(next) = s[i..].chars().next() else { break };
            i += next.len_utf8();
            cur.push(next);
            continue;
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        out.push(std::borrow::Cow::Owned(cur));
    }
    out
}

/// `SynonymRule.validate`: the validation error for a rule text, if any.
pub fn validate_rule(synonyms: &str) -> Option<String> {
    if synonyms.is_empty() {
        return Some("[synonyms] field can't be empty".into());
    }
    let incorrect = || Some(format!("Incorrect syntax for [synonyms]: [{synonyms}]"));
    let blank = |side: &str| split(side, ",").iter().any(|p| p.trim().is_empty());
    let sides = split(synonyms, "=>");
    if sides.len() > 1 {
        if sides.len() != 2 {
            return Some(format!("More than one explicit mapping specified in the same synonyms rule: [{synonyms}]"));
        }
        if blank(&sides[0]) || blank(&sides[1]) {
            return incorrect();
        }
    } else if blank(synonyms) {
        return incorrect();
    }
    None
}

/// The `SynonymSets` hook analysis chains resolve `synonyms_set` through.
pub struct StateSynonyms(pub Arc<Sets>);

impl SynonymSets for StateSynonyms {
    fn rules(&self, set: &str) -> Option<Vec<String>> {
        self.0.get(set).map(|s| s.rules.values().cloned().collect())
    }
}

/// Analyzers of `idx` that use `set` through an updateable synonym filter (what a reload of
/// the set reloads), sorted.
pub fn analyzers_using(idx: &IndexMeta, set: &str) -> Vec<String> {
    let s = &idx.settings;
    let filters = s.by_prefix("index.analysis.filter.");
    let mut using: Vec<String> = Vec::new();
    let names: std::collections::BTreeSet<&str> = filters.keys().filter_map(|k| k.split('.').next()).collect();
    for f in names {
        let ty = filters.get(&format!("{f}.type")).unwrap_or("");
        if !matches!(ty, "synonym" | "synonym_graph") || filters.get(&format!("{f}.updateable")) != Some("true") {
            continue;
        }
        if filters.get_list(&format!("{f}.synonyms_set")).is_some_and(|l| l.iter().any(|x| x == set)) {
            using.push(f.to_string());
        }
    }
    if using.is_empty() {
        return Vec::new();
    }
    let analyzers = s.by_prefix("index.analysis.analyzer.");
    let names: std::collections::BTreeSet<&str> = analyzers.keys().filter_map(|k| k.split('.').next()).collect();
    names
        .into_iter()
        .filter(|a| analyzers.get_list(&format!("{a}.filter")).is_some_and(|l| l.iter().any(|f| using.contains(f))))
        .map(str::to_string)
        .collect()
}

/// The `reload_analyzers_details` of a synonyms update: a broadcast over every open,
/// non-hidden index (`*`), reporting the indices whose analyzers use the set.
pub fn reload_details(st: &ClusterState, set: &str, node_id: &str) -> Value {
    let (mut total, mut ok) = (0i64, 0i64);
    let mut details = Vec::new();
    for idx in st.metadata.indices.values() {
        if idx.is_closed() || idx.is_hidden() {
            continue;
        }
        let shards = idx.number_of_shards() as i64;
        total += shards * (1 + idx.number_of_replicas() as i64);
        ok += shards;
        let analyzers = analyzers_using(idx, set);
        if !analyzers.is_empty() {
            details.push(obj! {
                "index" => idx.name.clone(),
                "reloaded_analyzers" => analyzers.into_iter().map(Value::from).collect::<Vec<_>>(),
                "reloaded_node_ids" => vec![Value::from(node_id.to_string())],
            });
        }
    }
    obj! {
        "_shards" => obj! { "total" => total, "successful" => ok, "failed" => 0 },
        "reload_details" => Value::Arr(details),
    }
}

/// Indices whose analyzers use `set` (deleting it is refused while any does), sorted.
pub fn indices_using(st: &ClusterState, set: &str) -> Vec<String> {
    st.metadata
        .indices
        .values()
        .filter(|i| !i.is_closed() && !i.is_hidden() && !analyzers_using(i, set).is_empty())
        .map(|i| i.name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_validation_matches_elasticsearch() {
        assert_eq!(validate_rule(""), Some("[synonyms] field can't be empty".into()));
        assert_eq!(
            validate_rule("bye => => goodbye"),
            Some("More than one explicit mapping specified in the same synonyms rule: [bye => => goodbye]".into())
        );
        assert_eq!(validate_rule(" => goodbye"), Some("Incorrect syntax for [synonyms]: [ => goodbye]".into()));
        assert_eq!(validate_rule("bye => "), Some("Incorrect syntax for [synonyms]: [bye => ]".into()));
        assert_eq!(validate_rule("bye, goodbye,  "), Some("Incorrect syntax for [synonyms]: [bye, goodbye,  ]".into()));
        // A leading separator yields no empty piece (Elasticsearch accepts this).
        assert_eq!(validate_rule("=> x"), None);
        assert_eq!(validate_rule("a\\,b, c"), None);
        assert_eq!(validate_rule("hello, hi"), None);
        assert_eq!(validate_rule("i-pod, i pod => ipod"), None);
    }

    #[test]
    fn split_keeps_escapes() {
        let parts: Vec<String> = split("a\\,b,c", ",").into_iter().map(|c| c.into_owned()).collect();
        assert_eq!(parts, vec!["a\\,b".to_string(), "c".to_string()]);
        let parts: Vec<String> = split(",,a,,", ",").into_iter().map(|c| c.into_owned()).collect();
        assert_eq!(parts, vec!["a".to_string()]);
    }
}
