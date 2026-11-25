//! The search templates an import makes: `<dest>-search`, one per Algolia replica
//! (`<dest>-search-<replica>`), and one per Algolia rule that filters or rewrites the query
//! (`<dest>-search-<rule>`).
//!
//! A template is a mustache string source (not an object), so it can take an optional
//! `filters` param: one Elasticsearch query that filters the results. The query rules are
//! layered the way Algolia's `filterPromotes` says:
//!
//! ```text
//! rule(normal rulesets,                  ← pins here show whatever the filters say
//!   filtered(rule(filtered rulesets,     ← pins of filterPromotes rules must match them
//!     filtered(organic))))
//! ```
//!
//! Rules match `rules_query` when it's sent, else `query` (Elasticsearch's rules compare the
//! text exactly; the client sends it lowercased).

use super::{Plan, RuleTemplate};
use serde_json::{json, Value};

/// The rules' match criteria: the text a client sends for them, and the search's context.
const MATCH_CRITERIA: &str = r#"{"query_string":"{{rules_query}}{{^rules_query}}{{query}}{{/rules_query}}","context":"{{context}}"}"#;

impl Plan {
    /// `x` filtered by `baked` (a rule template's own filter) and the search's `filters`.
    fn filtered(x: &str, baked: Option<&Value>) -> String {
        match baked {
            Some(b) => format!(r#"{{"bool":{{"must":[{x}],"filter":[{b}{{{{#filters}}}},{{{{#toJson}}}}filters{{{{/toJson}}}}{{{{/filters}}}}]}}}}"#),
            None => format!(r#"{{"bool":{{"must":[{x}]{{{{#filters}}}},"filter":[{{{{#toJson}}}}filters{{{{/toJson}}}}]{{{{/filters}}}}}}}}"#),
        }
    }

    fn rule_query(organic: &str, sets: &[String]) -> String {
        format!(r#"{{"rule":{{"organic":{organic},"ruleset_ids":{},"match_criteria":{MATCH_CRITERIA}}}}}"#, json!(sets))
    }

    /// The template's source for `dest`: `sort_first` before relevance (a replica's sort),
    /// and `rule`'s filter and literal query when it's a rule's template.
    pub(super) fn template_source(&self, dest: &str, sort_first: &[(String, &'static str)], rule: Option<&RuleTemplate>) -> String {
        // Typos on every word, the last also as a prefix: `bool_prefix` alone never makes its
        // last word fuzzy, so the two are said apart.
        let text = match rule.and_then(|r| r.query.as_deref()) {
            Some(q) => json!(q),
            None => json!("{{query}}"),
        };
        let words = match &self.fuzziness {
            Some(f) => json!({ "multi_match": { "query": text, "fuzziness": f, "lenient": true } }),
            None => json!({ "multi_match": { "query": text, "lenient": true } }),
        };
        let organic = if self.prefix {
            let prefix = json!({ "multi_match": { "query": text, "type": "bool_prefix", "lenient": true } });
            json!({ "bool": { "should": [words, prefix] } })
        } else {
            words
        };
        let baked = rule.and_then(|r| r.filter.as_ref());
        let mut q = Plan::filtered(&organic.to_string(), baked);
        let filtered_sets = self.filtered_rulesets(dest);
        if !filtered_sets.is_empty() {
            q = Plan::filtered(&Plan::rule_query(&q, &filtered_sets), baked);
        }
        let sets = self.rulesets(dest);
        if !sets.is_empty() {
            q = Plan::rule_query(&q, &sets);
        }
        let mut rest = serde_json::Map::new();
        if !sort_first.is_empty() || !self.sort_after.is_empty() {
            let one = |(f, o): &(String, &str)| json!({ f.clone(): { "order": o } });
            let mut sort: Vec<Value> = sort_first.iter().map(one).collect();
            sort.push(json!("_score"));
            sort.extend(self.sort_after.iter().map(one));
            rest.insert("sort".into(), json!(sort));
        }
        if let Some((f, n)) = &self.collapse {
            rest.insert("collapse".into(), if *n > 1 { json!({ "field": f, "inner_hits": { "name": "distinct", "size": n } }) } else { json!({ "field": f }) });
        }
        if !self.retrieve.is_empty() {
            rest.insert("_source".into(), json!(self.retrieve));
        }
        if let Some(n) = self.size {
            rest.insert("size".into(), json!(n));
        }
        let mut out = format!(r#"{{"query":{q}"#);
        for (k, v) in rest {
            out.push(',');
            out.push_str(&json!(k).to_string());
            out.push(':');
            out.push_str(&v.to_string());
        }
        out.push('}');
        out
    }
}

/// `pattern` with Algolia's query edits applied word by word (`remove` drops a word, `replace`
/// swaps it), or `None` when an edit isn't one roll applies.
pub(crate) fn apply_edits(pattern: &str, q: &Value) -> Option<String> {
    let mut words: Vec<String> = pattern.split_whitespace().map(str::to_string).collect();
    for w in q["remove"].as_array().into_iter().flatten() {
        let w = w.as_str()?;
        words.retain(|x| !x.eq_ignore_ascii_case(w));
    }
    for e in q["edits"].as_array().into_iter().flatten() {
        let del = e["delete"].as_str()?;
        match e["type"].as_str()? {
            "remove" => words.retain(|x| !x.eq_ignore_ascii_case(del)),
            "replace" => {
                let ins = e["insert"].as_str()?;
                for x in &mut words {
                    if x.eq_ignore_ascii_case(del) {
                        *x = ins.to_string();
                    }
                }
            }
            _ => return None,
        }
    }
    if q.as_object().is_some_and(|m| m.keys().any(|k| k != "remove" && k != "edits")) {
        return None;
    }
    Some(words.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_apply_word_by_word() {
        assert_eq!(apply_edits("cheap red chai", &json!({ "remove": ["cheap"], "edits": [{ "type": "replace", "delete": "chai", "insert": "teapot" }] })).as_deref(), Some("red teapot"));
        assert_eq!(apply_edits("a b", &json!({ "edits": [{ "type": "shuffle", "delete": "a" }] })), None);
    }

    #[test]
    fn a_template_renders_as_json_with_and_without_filters() {
        let mut p = Plan { fuzziness: Some("AUTO:4,8".into()), prefix: true, ..Default::default() };
        p.rules.push(json!({ "rule_id": "a" }));
        p.filtered_rules.push(json!({ "rule_id": "b" }));
        let src = p.template_source("d", &[], None);
        // Rendered as mustache would without `filters` (sections dropped), it's JSON.
        let bare = src
            .replace("{{#filters}},{{#toJson}}filters{{/toJson}}{{/filters}}", "")
            .replace(r#"{{#filters}},"filter":[{{#toJson}}filters{{/toJson}}]{{/filters}}"#, "")
            .replace("{{rules_query}}{{^rules_query}}{{query}}{{/rules_query}}", "q")
            .replace("{{query}}", "q")
            .replace("{{context}}", "");
        let v: Value = serde_json::from_str(&bare).unwrap_or_else(|e| panic!("{e}: {bare}"));
        assert_eq!(v["query"]["rule"]["ruleset_ids"], json!(["d-rules"]));
        assert_eq!(v["query"]["rule"]["organic"]["bool"]["must"][0]["rule"]["ruleset_ids"], json!(["d-rules-filtered"]));
    }
}
