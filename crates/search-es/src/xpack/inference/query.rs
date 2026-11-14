//! Queries that need inference, rewritten on the coordinator before any shard runs (the
//! inference plugin's query rewrite interceptors and `SemanticQueryBuilder.doRewrite`):
//!
//! - `semantic` on a `semantic_text` field;
//! - `match`, `knn` (with a `text_embedding` `query_vector_builder`) and `sparse_vector` (with a
//!   `query` text) on a `semantic_text` field;
//! - `sparse_vector` with an `inference_id` and a `query` text on any field.
//!
//! Each distinct (endpoint, text) is embedded once (`input_type: internal_search`, the field's
//! `search_inference_id`), then the query becomes what `SemanticFieldType.semanticQuery` builds:
//! `nested` (`score_mode: max`) over the field's chunks, holding `sparse_vector` on their
//! embeddings or `knn` (`k` at least 10). Where the target indices map the field differently, the
//! query is one such clause per configuration, each limited to its indices by `_index`.
//!
//! Inference runs with the cluster's authority: like Elasticsearch, which calls the inference
//! action under its own origin, a reader needs only its read privileges on the indices.

use super::TaskType;
use super::semantic::{ModelSettings, embeddings_path, chunks_path};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::query::Query;
use crate::rest::Ctx;

/// How one index maps a field the query names.
#[derive(Clone, Debug, PartialEq)]
enum FieldConfig {
    /// A `semantic_text` field: its search endpoint and recorded model settings.
    Semantic { inference_id: String, settings: Option<ModelSettings> },
    /// Another type.
    Other(String),
    Unmapped,
}

fn config(idx: &crate::meta::IndexMeta, field: &str) -> FieldConfig {
    match idx.mapping.lookup(field) {
        Some(crate::mapping::MapperRef::Field(f)) if f.ty == "semantic_text" => {
            FieldConfig::Semantic { inference_id: super::semantic::search_inference_id(f), settings: super::semantic::model_settings(f) }
        }
        Some(m) => FieldConfig::Other(m.type_name().to_string()),
        None => FieldConfig::Unmapped,
    }
}

/// A query node needing inference: the field, the text, and what to build from the embedding.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Semantic,
    Match,
    Knn,
    Sparse,
}

/// The target indices of a search, as `(name, metadata)`.
pub type Targets = Vec<(String, std::sync::Arc<crate::meta::IndexMeta>)>;

/// Whether a query tree has anything to rewrite (most don't, and skip the work).
pub fn needs_rewrite(q: &Query, targets: &Targets) -> bool {
    let semantic_field = |f: &str| targets.iter().any(|(_, i)| matches!(config(i, f), FieldConfig::Semantic { .. }));
    let mut found = false;
    visit(q, &mut |q| {
        found |= match q {
            Query::Semantic(_) => true,
            Query::Match(m) => semantic_field(&m.field),
            Query::Knn(k) => semantic_field(&k.field),
            Query::SparseVector(s) => s.query.is_some() && (s.inference_id.is_some() || semantic_field(&s.field)),
            _ => false,
        }
    });
    found
}

fn visit(q: &Query, f: &mut dyn FnMut(&Query)) {
    f(q);
    match q {
        Query::Bool(b) => b.must.iter().chain(&b.filter).chain(&b.should).chain(&b.must_not).for_each(|c| visit(c, f)),
        Query::DisMax(d) => d.queries.iter().for_each(|c| visit(c, f)),
        Query::Boosting(b) => {
            visit(&b.positive, f);
            visit(&b.negative, f);
        }
        Query::ConstantScore(c) => visit(&c.filter, f),
        Query::FunctionScore(s) => visit(&s.query, f),
        Query::ScriptScore(s) => visit(&s.query, f),
        Query::Nested(n) => visit(&n.query, f),
        Query::HasChild(h) => visit(&h.query, f),
        Query::HasParent(h) => visit(&h.query, f),
        Query::Knn(k) => k.filter.iter().for_each(|c| visit(c, f)),
        _ => {}
    }
}

/// Rewrites every node needing inference in place.
pub async fn rewrite(ctx: &Ctx<'_>, q: &mut Query, targets: &Targets, size: Option<i64>) -> EsResult<()> {
    // Collect the (endpoint, text) pairs first, embed each once, then rewrite.
    let mut texts: Vec<(String, String)> = Vec::new();
    collect(ctx, q, targets, &mut texts)?;
    let mut embedded: Vec<((String, String), super::results::Results)> = Vec::new();
    for (id, text) in texts {
        if embedded.iter().any(|(k, _)| k.0 == id && k.1 == text) {
            continue;
        }
        let mut req = super::Request::new(vec![text.clone()], std::time::Duration::from_secs(30));
        req.input_type = Some("internal_search".into());
        let (_, r) = super::run(ctx.cluster, &id, &req).await?;
        embedded.push(((id, text), r));
    }
    apply(q, targets, &embedded, size)
}

fn node(q: &Query) -> Option<(Kind, &str, String, Option<&str>)> {
    match q {
        Query::Semantic(s) => Some((Kind::Semantic, &s.field, s.query.clone(), None)),
        Query::Match(m) => Some((Kind::Match, &m.field, m.query.to_es_string(), None)),
        Query::Knn(k) => {
            let te = k.query_vector_builder.as_ref()?.get("text_embedding")?;
            let text = te.get("model_text")?.to_es_string();
            Some((Kind::Knn, &k.field, text, te.get("model_id").and_then(Value::as_str)))
        }
        Query::SparseVector(s) => Some((Kind::Sparse, &s.field, s.query.clone()?, s.inference_id.as_deref())),
        _ => None,
    }
}

fn collect(ctx: &Ctx, q: &Query, targets: &Targets, out: &mut Vec<(String, String)>) -> EsResult<()> {
    let st = ctx.state();
    let mut err = None;
    visit(q, &mut |q| {
        let Some((kind, field, text, explicit)) = node(q) else { return };
        if let Some(id) = explicit {
            // A `knn` builder's `model_id` may name a trained model instead (ML's path).
            if kind == Kind::Sparse || super::endpoint(ctx.cluster, &st, id).is_some() {
                out.push((id.to_string(), text.clone()));
            }
            if kind == Kind::Sparse {
                return;
            }
        }
        for (name, idx) in targets {
            match config(idx, field) {
                FieldConfig::Semantic { inference_id, settings } => {
                    if settings.is_some() {
                        out.push((inference_id, text.clone()));
                    }
                }
                FieldConfig::Other(ty) if kind == Kind::Semantic => {
                    err.get_or_insert(EsError::new(400, "query_shard_exception", format!("Field [{field}] of type [{ty}] is not supported for [semantic] query")).with_meta("index", name.clone()));
                }
                _ => {}
            }
        }
    });
    err.map_or(Ok(()), Err)
}

fn results_of<'a>(embedded: &'a [((String, String), super::results::Results)], id: &str, text: &str) -> Option<&'a super::results::Results> {
    embedded.iter().find(|((i, t), _)| i == id && t == text).map(|(_, r)| r)
}

/// The query on one configuration of the field (`SemanticFieldType.semanticQuery`).
fn semantic_query(field: &str, settings: &Option<ModelSettings>, r: Option<&super::results::Results>, size: Option<i64>, boost: f32, name: Option<&str>) -> EsResult<Value> {
    use super::results::Results;
    // No document has been embedded yet: the chunks aren't mapped either.
    if settings.is_none() {
        return Ok(obj! { "match_none" => obj! {} });
    }
    let child = match (settings, r) {
        (None, _) | (_, None) => obj! { "match_none" => obj! {} },
        (Some(s), Some(Results::Sparse(v))) if s.task == TaskType::SparseEmbedding => {
            let tokens = v.first().map(|e| super::results::weights(&e.tokens)).unwrap_or_else(Value::object);
            obj! { "sparse_vector" => obj! { "field" => embeddings_path(field), "query_vector" => tokens } }
        }
        (Some(s), Some(r)) if matches!(s.task, TaskType::TextEmbedding | TaskType::Embedding) => {
            let vector: Vec<Value> = match r {
                Results::Floats(v) => v.first().map(|e| e.iter().map(|f| crate::query::xparser::float_value(*f)).collect()).unwrap_or_default(),
                Results::Bytes(v) | Results::Bits(v) => v.first().map(|e| e.iter().map(|b| Value::from(i64::from(*b))).collect()).unwrap_or_default(),
                _ => {
                    return Err(EsError::illegal_argument(format!(
                        "Field [{field}] expected query inference results to be of type [text_embedding_result], got [{}].",
                        result_name(r)
                    )));
                }
            };
            let dims = if s.element_type.as_deref() == Some("bit") { vector.len() * 8 } else { vector.len() };
            if Some(dims as i64) != s.dimensions {
                return Err(EsError::illegal_argument(format!(
                    "Field [{field}] expected query inference results with {} dimensions, got {dims} dimensions.",
                    s.dimensions.unwrap_or(0)
                )));
            }
            let mut knn = Map::new();
            knn.insert("field".into(), embeddings_path(field).into());
            knn.insert("query_vector".into(), Value::Arr(vector));
            if let Some(k) = size {
                knn.insert("k".into(), k.max(10).into());
            }
            obj! { "knn" => Value::Obj(knn) }
        }
        (Some(s), Some(r)) => {
            let expected = if s.task == TaskType::SparseEmbedding { "text_expansion_result" } else { "text_embedding_result" };
            return Err(EsError::illegal_argument(format!("Field [{field}] expected query inference results to be of type [{expected}], got [{}].", result_name(r))));
        }
    };
    let mut nested = Map::new();
    nested.insert("path".into(), chunks_path(field).into());
    nested.insert("query".into(), child);
    nested.insert("score_mode".into(), "max".into());
    // Clauses limited to other indices by `_index` still parse on every target.
    nested.insert("ignore_unmapped".into(), true.into());
    if boost != 1.0 {
        nested.insert("boost".into(), crate::query::xparser::float_value(boost));
    }
    if let Some(n) = name {
        nested.insert("_name".into(), n.into());
    }
    Ok(obj! { "nested" => Value::Obj(nested) })
}

fn result_name(r: &super::results::Results) -> &'static str {
    use super::results::Results;
    match r {
        Results::Sparse(_) => "text_expansion_result",
        Results::Floats(_) | Results::Bytes(_) | Results::Bits(_) => "text_embedding_result",
        _ => "inference_result",
    }
}

fn parse(v: &Value) -> EsResult<Query> {
    crate::query::parse_query(v, None, &[]).map(|(q, _)| q)
}

/// The replacement of one node, or `None` to keep it (a `match` or `knn` on a field no target
/// maps as `semantic_text`).
fn replacement(q: &Query, targets: &Targets, embedded: &[((String, String), super::results::Results)], size: Option<i64>) -> EsResult<Option<Query>> {
    // `knn` with its own vector on a `semantic_text` field: the same `knn` on the chunks'
    // embeddings (`SemanticKnnVectorQueryRewriteInterceptor`).
    if let Query::Knn(k) = q
        && k.query_vector.is_some()
        && k.query_vector_builder.is_none()
        && targets.iter().any(|(_, i)| matches!(config(i, &k.field), FieldConfig::Semantic { .. }))
    {
        let Value::Obj(mut outer) = q.to_value() else { return Ok(None) };
        let Some(Value::Obj(mut inner)) = outer.shift_remove("knn") else { return Ok(None) };
        let boost = inner.shift_remove("boost");
        let name = inner.shift_remove("_name");
        inner.insert("field".into(), embeddings_path(&k.field).into());
        let mut nested = Map::new();
        nested.insert("path".into(), chunks_path(&k.field).into());
        nested.insert("query".into(), obj! { "knn" => Value::Obj(inner) });
        nested.insert("score_mode".into(), "max".into());
        if let Some(b) = boost {
            nested.insert("boost".into(), b);
        }
        if let Some(n) = name {
            nested.insert("_name".into(), n);
        }
        return parse(&obj! { "nested" => Value::Obj(nested) }).map(Some);
    }
    let Some((kind, field, text, explicit)) = node(q) else { return Ok(None) };
    let common = match q {
        Query::Semantic(s) => &s.common,
        Query::Match(m) => &m.common,
        Query::Knn(k) => &k.common,
        Query::SparseVector(s) => &s.common,
        _ => return Ok(None),
    };
    if kind == Kind::Sparse && let Some(id) = explicit {
        // A plain `sparse_vector` query whose tokens come from an endpoint.
        let Some(super::results::Results::Sparse(v)) = results_of(embedded, id, &text) else {
            return Err(EsError::illegal_argument(format!("Inference endpoint [{id}] doesn't produce sparse embeddings")));
        };
        let mut m = Map::new();
        m.insert("field".into(), field.into());
        m.insert("query_vector".into(), v.first().map(|e| super::results::weights(&e.tokens)).unwrap_or_else(Value::object));
        if let Query::SparseVector(s) = q
            && let Some(p) = s.prune
        {
            m.insert("prune".into(), p.into());
        }
        if common.boost != 1.0 {
            m.insert("boost".into(), crate::query::xparser::float_value(common.boost));
        }
        if let Some(n) = &common.name {
            m.insert("_name".into(), n.clone().into());
        }
        return parse(&obj! { "sparse_vector" => Value::Obj(m) }).map(Some);
    }
    // A `knn` whose builder names an endpoint, on fields that aren't `semantic_text`: the
    // endpoint's embedding as the query vector.
    if kind == Kind::Knn
        && let (Query::Knn(k), Some(id)) = (q, explicit)
        && !targets.iter().any(|(_, i)| matches!(config(i, field), FieldConfig::Semantic { .. }))
    {
        let vector = match results_of(embedded, id, &text) {
            Some(super::results::Results::Floats(v)) => v.first().cloned().unwrap_or_default(),
            Some(_) => return Err(EsError::illegal_argument(format!("Inference endpoint [{id}] doesn't produce dense embeddings"))),
            // Not an inference endpoint: left to the trained models' path.
            None => return Ok(None),
        };
        let mut k = k.clone();
        k.query_vector = Some(crate::query::specialized::VectorData::Floats(vector));
        k.query_vector_builder = None;
        return Ok(Some(Query::Knn(k)));
    }
    // Configurations of the field across the target indices, with their indices.
    let mut groups: Vec<(FieldConfig, Vec<String>)> = Vec::new();
    for (name, idx) in targets {
        let c = config(idx, field);
        match groups.iter_mut().find(|(g, _)| *g == c) {
            Some((_, v)) => v.push(name.clone()),
            None => groups.push((c, vec![name.clone()])),
        }
    }
    if kind != Kind::Semantic && !groups.iter().any(|(c, _)| matches!(c, FieldConfig::Semantic { .. })) {
        return Ok(None);
    }
    let mut clauses: Vec<(Value, Vec<String>)> = Vec::new();
    for (c, indices) in &groups {
        let clause = match c {
            FieldConfig::Semantic { inference_id, settings } => {
                let r = settings.as_ref().and_then(|_| results_of(embedded, inference_id, &text));
                semantic_query(field, settings, r, size, common.boost, common.name.as_deref())?
            }
            FieldConfig::Unmapped => obj! { "match_none" => obj! {} },
            // The original query, for the indices where the field isn't `semantic_text`.
            FieldConfig::Other(_) => q.to_value(),
        };
        clauses.push((clause, indices.clone()));
    }
    let v = if clauses.len() == 1 {
        clauses.pop().map(|(c, _)| c).unwrap_or_default()
    } else {
        let should: Vec<Value> = clauses
            .into_iter()
            .map(|(c, indices)| {
                obj! { "bool" => obj! {
                    "must" => crate::arr![c],
                    "filter" => crate::arr![obj! { "terms" => obj! { "_index" => Value::Arr(indices.into_iter().map(Value::from).collect()) } }],
                } }
            })
            .collect();
        obj! { "bool" => obj! { "should" => Value::Arr(should), "minimum_should_match" => 1 } }
    };
    parse(&v).map(Some)
}

fn apply(q: &mut Query, targets: &Targets, embedded: &[((String, String), super::results::Results)], size: Option<i64>) -> EsResult<()> {
    if let Some(r) = replacement(q, targets, embedded, size)? {
        *q = r;
        return Ok(());
    }
    match q {
        Query::Bool(b) => {
            for c in b.must.iter_mut().chain(b.filter.iter_mut()).chain(b.should.iter_mut()).chain(b.must_not.iter_mut()) {
                apply(c, targets, embedded, size)?;
            }
        }
        Query::DisMax(d) => {
            for c in d.queries.iter_mut() {
                apply(c, targets, embedded, size)?;
            }
        }
        Query::Boosting(b) => {
            apply(&mut b.positive, targets, embedded, size)?;
            apply(&mut b.negative, targets, embedded, size)?;
        }
        Query::ConstantScore(c) => apply(&mut c.filter, targets, embedded, size)?,
        Query::FunctionScore(f) => apply(&mut f.query, targets, embedded, size)?,
        Query::ScriptScore(s) => apply(&mut s.query, targets, embedded, size)?,
        Query::Nested(n) => apply(&mut n.query, targets, embedded, size)?,
        Query::HasChild(h) => apply(&mut h.query, targets, embedded, size)?,
        Query::HasParent(h) => apply(&mut h.query, targets, embedded, size)?,
        Query::Knn(k) => {
            for f in k.filter.iter_mut() {
                apply(f, targets, embedded, size)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Rewrites a search request's query (`_search`), when its targets hold `semantic_text` fields
/// or it embeds a query text.
pub async fn rewrite_request(ctx: &Ctx<'_>, req: &mut crate::search::SearchRequest) -> EsResult<()> {
    if ctx.cluster.dialect != crate::cluster::Dialect::Elasticsearch || req.source.query.is_none() {
        return Ok(());
    }
    let st = ctx.state();
    let Ok(resolved) = crate::search::resolve_targets(ctx.cluster, &st, req) else { return Ok(()) };
    let targets: Targets = resolved.into_iter().map(|(i, _, _)| (i.name.clone(), i)).collect();
    let size = Some(req.source.size.map_or(10, |s| s as i64));
    let Some(q) = req.source.query.as_mut() else { return Ok(()) };
    if !needs_rewrite(q, &targets) {
        return Ok(());
    }
    rewrite(ctx, q, &targets, size).await
}
