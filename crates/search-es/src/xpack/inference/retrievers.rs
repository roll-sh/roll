//! The `rrf` and `linear` retrievers' simplified form (`query` with optional `fields`), expanded
//! into explicit `retrievers` before the request is parsed, as Elasticsearch's
//! `MultiFieldsInnerRetrieverUtils` expands it against the target indices' mappings:
//!
//! - one lexical `standard` retriever: `multi_match` (`most_fields`) over the fields that aren't
//!   `semantic_text`, per group of indices with the same fields (limited by `_index` when the
//!   group isn't every index, several groups as `bool.should`);
//! - one `standard` retriever per `semantic_text` field and endpoint, a `match` (embedded by the
//!   inference rewrite), fused by an inner retriever of the same kind with the fields' weights;
//! - both fused by the outer retriever with weight 1 each (and its `normalizer` for `linear`).
//!
//! Fields default to each index's `index.query.default_field` (`*`); `name^2` weights a field.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;

/// Expands every simplified `rrf`/`linear` in a search body's `retriever`.
pub fn expand(indices: &[std::sync::Arc<crate::meta::IndexMeta>], body: &mut Value) -> EsResult<()> {
    if let Some(r) = body.as_object_mut().and_then(|b| b.get_mut("retriever")) {
        walk(indices, r)?;
    }
    Ok(())
}

/// Whether a body has an `rrf` or `linear` retriever anywhere (whose form is checked and, when
/// simplified, expanded).
pub fn uses_simplified(body: &Value) -> bool {
    fn find(v: &Value) -> bool {
        match v {
            Value::Obj(m) => {
                for (k, inner) in m {
                    if k == "rrf" || k == "linear" {
                        return true;
                    }
                    if find(inner) {
                        return true;
                    }
                }
                false
            }
            Value::Arr(a) => a.iter().any(find),
            _ => false,
        }
    }
    body.get("retriever").is_some_and(find)
}

fn walk(indices: &[std::sync::Arc<crate::meta::IndexMeta>], r: &mut Value) -> EsResult<()> {
    let Value::Obj(m) = r else { return Ok(()) };
    let Some((kind, inner)) = m.iter_mut().next() else { return Ok(()) };
    let kind = kind.clone();
    if (kind == "rrf" || kind == "linear")
        && let Value::Obj(body) = inner
    {
        if body.contains_key("query") || body.contains_key("fields") {
            *r = expand_one(indices, &kind, body)?;
            return Ok(());
        }
        if body.get("retrievers").and_then(Value::as_array).is_none_or(|r| r.is_empty()) {
            return Err(EsError::action_request_validation(&[format!("[{kind}] must provide [retrievers] or [query]")]));
        }
        if let Some(Value::Arr(children)) = body.get_mut("retrievers") {
            for c in children.iter_mut() {
                match c.as_object_mut().and_then(|cm| cm.get_mut("retriever")) {
                    Some(inner) => walk(indices, inner)?,
                    None => walk(indices, c)?,
                }
            }
        }
    }
    // Retrievers with one child retriever (`text_similarity_reranker`, `rule`, `pinned`, …).
    if let Some(Value::Obj(body)) = r.as_object_mut().and_then(|m| m.values_mut().next())
        && let Some(child) = body.get_mut("retriever")
    {
        walk(indices, child)?;
    }
    Ok(())
}

/// `MultiFieldsInnerRetrieverUtils.validateParams`.
fn validate(kind: &str, body: &Map) -> EsResult<(String, Option<Vec<String>>)> {
    let query = body.get("query").map(Value::to_es_string);
    let fields: Option<Vec<String>> = body.get("fields").map(|f| match f {
        Value::Arr(a) => a.iter().map(Value::to_es_string).collect(),
        other => vec![other.to_es_string()],
    });
    let mut errors = Vec::new();
    match &query {
        None => errors.push(format!("[{kind}] [query] must be provided when [fields] is specified")),
        Some(q) if q.is_empty() => errors.push(format!("[{kind}] [query] cannot be empty")),
        _ => {}
    }
    if query.is_some() {
        if kind == "linear" && !body.contains_key("normalizer") {
            errors.push("[linear] [normalizer] must be provided when [query] is specified".into());
        }
        if fields.as_ref().is_some_and(Vec::is_empty) {
            errors.push(format!("[{kind}] [fields] cannot be empty"));
        }
        if body.get("retrievers").and_then(Value::as_array).is_some_and(|r| !r.is_empty()) {
            errors.push(format!("[{kind}] cannot combine [retrievers] and [query]"));
        }
    }
    if !errors.is_empty() {
        return Err(EsError::action_request_validation(&errors));
    }
    Ok((query.unwrap_or_default(), fields))
}

/// `QueryParserHelper.parseFieldsAndWeights`: `name^weight`.
fn parse_fields(fields: &[String], kind: &str) -> EsResult<Vec<(String, f32)>> {
    let mut out: Vec<(String, f32)> = Vec::new();
    for f in fields {
        let (name, w) = match f.rsplit_once('^') {
            Some((n, w)) => (n.to_string(), w.parse::<f32>().map_err(|_| EsError::illegal_argument(format!("failed to parse field weight [{w}]")))?),
            None => (f.clone(), 1.0),
        };
        if w < 0.0 {
            return Err(EsError::illegal_argument(format!("[{kind}] per-field weights must be non-negative")));
        }
        match out.iter_mut().find(|(n, _)| *n == name) {
            Some(e) => e.1 = w,
            None => out.push((name, w)),
        }
    }
    Ok(out)
}

fn default_fields(idx: &crate::meta::IndexMeta) -> Vec<String> {
    match idx.settings.get("index.query.default_field") {
        Some(v) => v.trim_matches(['[', ']']).split(',').map(|s| s.trim().trim_matches('"').to_string()).filter(|s| !s.is_empty()).collect(),
        None => vec!["*".to_string()],
    }
}

fn weight_value(w: f32) -> Value {
    crate::query::xparser::float_value(w)
}

fn field_list(fields: &[(String, f32)]) -> Value {
    Value::Arr(fields.iter().map(|(n, w)| if *w == 1.0 { Value::from(n.as_str()) } else { Value::from(format!("{n}^{}", crate::json::write::java_float(*w))) }).collect())
}

fn limited(q: Value, indices: &[String], all: usize) -> Value {
    if indices.len() == all {
        return q;
    }
    obj! { "bool" => obj! {
        "must" => crate::arr![q],
        "filter" => crate::arr![obj! { "terms" => obj! { "_index" => Value::Arr(indices.iter().map(|i| Value::from(i.as_str())).collect()) } }],
    } }
}

/// The weighted text fields some indices share, and those indices.
type LexicalGroup = (Vec<(String, f32)>, Vec<String>);

/// A `semantic_text` field and its endpoint, its weight, and the indices mapping it so.
type SemanticGroup = ((String, String), f32, Vec<String>);

fn expand_one(indices: &[std::sync::Arc<crate::meta::IndexMeta>], kind: &str, body: &Map) -> EsResult<Value> {
    let (query, fields) = validate(kind, body)?;
    let given = match &fields {
        Some(f) => parse_fields(f, kind)?,
        None => Vec::new(),
    };
    let all = indices.len();
    // Lexical: the non-inference fields per index, indices with equal fields grouped.
    let mut lexical: Vec<LexicalGroup> = Vec::new();
    // Semantic: (field, endpoint) → weight and indices.
    let mut semantic: Vec<SemanticGroup> = Vec::new();
    for idx in indices {
        let wanted = if given.is_empty() { parse_fields(&default_fields(idx), kind)? } else { given.clone() };
        let inference: Vec<(String, String)> =
            super::semantic::fields(&idx.mapping).into_iter().map(|(p, f)| (p, super::semantic::inference_id(f))).collect();
        // `getMatchingInferenceFields`: patterns against the index's inference fields.
        for (path, id) in &inference {
            let Some((_, w)) = wanted.iter().find(|(pat, _)| crate::xcontent::filter::glob_match(pat, path)) else { continue };
            let key = (path.clone(), id.clone());
            match semantic.iter_mut().find(|(k, _, _)| *k == key) {
                Some((_, weight, names)) => {
                    if *weight != *w {
                        return Err(EsError::illegal_argument(format!("field [{path}] has different weights in indices [{}] and [{}]", names[0], idx.name)));
                    }
                    names.push(idx.name.clone());
                }
                None => semantic.push((key, *w, vec![idx.name.clone()])),
            }
        }
        let non_inference: Vec<(String, f32)> = wanted.into_iter().filter(|(n, _)| !inference.iter().any(|(p, _)| p == n)).collect();
        if non_inference.is_empty() {
            continue;
        }
        match lexical.iter_mut().find(|(f, _)| *f == non_inference) {
            Some((_, names)) => names.push(idx.name.clone()),
            None => lexical.push((non_inference, vec![idx.name.clone()])),
        }
    }
    let linear = kind == "linear";
    let normalizer = body.get("normalizer").cloned();
    let entry = |retriever: Value, weight: f32| -> Value {
        let mut m = Map::new();
        m.insert("retriever".into(), retriever);
        m.insert("weight".into(), weight_value(weight));
        if linear && let Some(n) = &normalizer {
            m.insert("normalizer".into(), n.clone());
        }
        Value::Obj(m)
    };
    let mut inner: Vec<Value> = Vec::new();
    if !lexical.is_empty() {
        let mut queries: Vec<Value> = lexical
            .iter()
            .map(|(f, names)| limited(obj! { "multi_match" => obj! { "query" => query.clone(), "type" => "most_fields", "fields" => field_list(f), "lenient" => true } }, names, all))
            .collect();
        let q = if queries.len() == 1 { queries.pop().unwrap_or_default() } else { obj! { "bool" => obj! { "should" => Value::Arr(queries) } } };
        inner.push(entry(obj! { "standard" => obj! { "query" => q } }, 1.0));
    }
    if !semantic.is_empty() {
        let children: Vec<Value> = semantic
            .iter()
            .map(|((field, _), w, names)| {
                let mut m = Map::new();
                m.insert(field.clone(), Value::from(query.clone()));
                entry(obj! { "standard" => obj! { "query" => limited(obj! { "match" => Value::Obj(m) }, names, all) } }, *w)
            })
            .collect();
        let mut fused = Map::new();
        fused.insert("retrievers".into(), Value::Arr(children));
        for k in ["rank_window_size", "rank_constant"] {
            if let Some(v) = body.get(k) {
                fused.insert(k.into(), v.clone());
            }
        }
        if linear {
            fused.shift_remove("rank_constant");
        }
        let mut wrapper = Map::new();
        wrapper.insert(kind.to_string(), Value::Obj(fused));
        inner.push(entry(Value::Obj(wrapper), 1.0));
    }
    if inner.is_empty() {
        // No target index has a matching field (an expression matching no index).
        return Ok(obj! { "standard" => obj! { "query" => obj! { "match_none" => obj! {} } } });
    }
    let mut outer = Map::new();
    outer.insert("retrievers".into(), Value::Arr(inner));
    for k in ["rank_window_size", "rank_constant", "filter", "min_score", "_name", "normalizer"] {
        if let Some(v) = body.get(k) {
            outer.insert(k.into(), v.clone());
        }
    }
    if linear {
        outer.shift_remove("rank_constant");
    } else {
        outer.shift_remove("normalizer");
    }
    let mut wrapper = Map::new();
    wrapper.insert(kind.to_string(), Value::Obj(outer));
    Ok(Value::Obj(wrapper))
}
