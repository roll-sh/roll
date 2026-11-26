//! The neural-search plugin's `neural` query (`NeuralQueryBuilder`), phase 1 of
//! docs/search/opensearch-neural.md: `query_text` embedded by `model_id`
//! ([`crate::opensearch::ml::embed`]), then the k-NN plugin's `knn` query on the `knn_vector`
//! field with `k`, `min_score` or `max_distance`, `filter`, `method_parameters` and
//! `expand_nested_docs` as given.
//!
//! The embedding runs on the coordinator before any shard ([`rewrite_request`]), once per
//! distinct (model, text) of the request, as the searcher (a model group's access mode applies).
//! A search pipeline's `neural_query_enricher` fills a missing `model_id` first ([`fill_models`]).
//! A browser key embeds only with the models its `limits.inference_endpoints` name, within its
//! `max_inference` texts and rate limits. Not served in phase 1: `query_image` (multimodal) and
//! the `semantic` field.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::opensearch::knn::OsKnn;
use crate::query::xparser::{Field, Tokens};
use crate::query::{Common, Query, Token, XParser, check_boost, parse_inner};
use crate::rest::Ctx;

#[derive(Clone, Debug, PartialEq)]
pub struct Neural {
    pub field: String,
    pub query_text: String,
    pub model_id: Option<String>,
    pub k: Option<i32>,
    pub min_score: Option<f32>,
    pub max_distance: Option<f32>,
    pub filter: Option<Query>,
    pub method_parameters: Option<Map>,
    pub expand_nested: Option<bool>,
    pub common: Common,
}

const DEFAULT_K: i32 = 10;

impl Neural {
    /// `NeuralQueryBuilder.fromXContent`.
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Neural> {
        const FIELDS: &[Field] = &[
            Field::new("boost", Tokens::FLOAT),
            Field::new("_name", Tokens::STRING),
            Field::new("query_text", Tokens::STRING),
            Field::new("model_id", Tokens::STRING),
            Field::new("k", Tokens::INT),
            Field::new("min_score", Tokens::FLOAT),
            Field::new("max_distance", Tokens::FLOAT),
            Field::new("filter", Tokens::OBJECT),
            Field::new("method_parameters", Tokens::OBJECT),
            Field::new("expand_nested_docs", Tokens::BOOLEAN),
            Field::new("query_image", Tokens::STRING),
        ];
        let mut q = Neural {
            field: String::new(),
            query_text: String::new(),
            model_id: None,
            k: None,
            min_score: None,
            max_distance: None,
            filter: None,
            method_parameters: None,
            expand_nested: None,
            common: Common::default(),
        };
        let mut boost = 1.0f32;
        let mut text = None;
        let mut image = false;
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartObject => {
                    if !q.field.is_empty() {
                        return Err(p.parsing_error("[neural] query doesn't support multiple fields"));
                    }
                    q.field = current.clone();
                    p.object("neural", FIELDS, |p, i| {
                        match i {
                            0 => boost = p.float_value()?,
                            1 => q.common.name = Some(p.text()?),
                            2 => text = Some(p.text()?),
                            3 => q.model_id = Some(p.text()?),
                            4 => q.k = Some(p.int_value()?),
                            5 => q.min_score = Some(p.float_value()?),
                            6 => q.max_distance = Some(p.float_value()?),
                            7 => q.filter = Some(parse_inner(p)?),
                            8 => {
                                let Value::Obj(m) = p.take_value() else { return Ok(()) };
                                q.method_parameters = Some(m);
                            }
                            9 => q.expand_nested = Some(p.boolean_value()?),
                            _ => {
                                p.text()?;
                                image = true;
                            }
                        }
                        Ok(())
                    })?;
                }
                _ => return Err(p.parsing_error("[neural] query malformed, no start_object after query name")),
            }
        }
        if image {
            return Err(EsError::illegal_argument("[neural] [query_image] (multimodal search) isn't served by roll yet"));
        }
        q.query_text = text.filter(|t| !t.is_empty()).ok_or_else(|| EsError::illegal_argument("Either query_text or query_image must be provided."))?;
        let set = [q.k.is_some(), q.min_score.is_some(), q.max_distance.is_some()].iter().filter(|x| **x).count();
        if set > 1 {
            return Err(EsError::illegal_argument("Only one of k, max_distance, or min_score can be provided"));
        }
        if set == 0 {
            q.k = Some(DEFAULT_K);
        }
        q.common.boost = check_boost(boost, || q.clone().into())?;
        Ok(q)
    }

    /// The body under `neural`.
    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("query_text".into(), self.query_text.clone().into());
        if let Some(id) = &self.model_id {
            m.insert("model_id".into(), id.clone().into());
        }
        if let Some(k) = self.k {
            m.insert("k".into(), Value::from(k as i64));
        }
        if let Some(s) = self.min_score {
            m.insert("min_score".into(), Value::from(s as f64));
        }
        if let Some(d) = self.max_distance {
            m.insert("max_distance".into(), Value::from(d as f64));
        }
        if let Some(f) = &self.filter {
            m.insert("filter".into(), f.to_value());
        }
        if let Some(mp) = &self.method_parameters {
            m.insert("method_parameters".into(), Value::Obj(mp.clone()));
        }
        if let Some(e) = self.expand_nested {
            m.insert("expand_nested_docs".into(), e.into());
        }
        self.common.write_always(&mut m);
        let mut out = Map::new();
        out.insert(self.field.clone(), Value::Obj(m));
        Value::Obj(out)
    }

    /// The `knn` query it stands for, with the embedding of its text.
    fn knn(&self, vector: Vec<f32>) -> OsKnn {
        OsKnn {
            field: self.field.clone(),
            vector,
            k: self.k,
            max_distance: self.max_distance,
            min_score: self.min_score,
            filter: self.filter.clone(),
            method_parameters: self.method_parameters.clone(),
            rescore: None,
            expand_nested: self.expand_nested,
            ignore_unmapped: false,
            common: self.common.clone(),
        }
    }
}

/// Calls `f` on every query of `q`'s tree, `q` first.
fn walk_mut(q: &mut Query, f: &mut dyn FnMut(&mut Query)) {
    f(q);
    match q {
        Query::Bool(b) => b.must.iter_mut().chain(b.filter.iter_mut()).chain(b.should.iter_mut()).chain(b.must_not.iter_mut()).for_each(|c| walk_mut(c, f)),
        Query::DisMax(d) => d.queries.iter_mut().for_each(|c| walk_mut(c, f)),
        Query::Boosting(b) => {
            walk_mut(&mut b.positive, f);
            walk_mut(&mut b.negative, f);
        }
        Query::ConstantScore(c) => walk_mut(&mut c.filter, f),
        Query::FunctionScore(s) => walk_mut(&mut s.query, f),
        Query::ScriptScore(s) => walk_mut(&mut s.query, f),
        Query::Nested(n) => walk_mut(&mut n.query, f),
        Query::HasChild(h) => walk_mut(&mut h.query, f),
        Query::HasParent(h) => walk_mut(&mut h.query, f),
        Query::OsHybrid(h) => h.queries.iter_mut().for_each(|c| walk_mut(c, f)),
        Query::OsKnn(k) => k.filter.iter_mut().for_each(|c| walk_mut(c, f)),
        Query::OsNeural(n) => n.filter.iter_mut().for_each(|c| walk_mut(c, f)),
        _ => {}
    }
}

/// `neural_query_enricher`: a `neural` query without a `model_id` gets its field's default, or
/// the pipeline's.
pub fn fill_models(q: &mut Query, default: Option<&str>, by_field: &Map) {
    walk_mut(q, &mut |q| {
        if let Query::OsNeural(n) = q
            && n.model_id.is_none()
        {
            n.model_id = by_field.get(&n.field).and_then(Value::as_str).or(default).map(str::to_owned);
        }
    });
}

/// Embeds every `neural` query of the request's query and replaces it with its `knn` query.
pub async fn rewrite_request(ctx: &Ctx<'_>, req: &mut crate::search::SearchRequest) -> EsResult<()> {
    let Some(q) = req.source.query.as_mut() else { return Ok(()) };
    let mut wanted: Vec<(String, String)> = Vec::new();
    let mut missing = false;
    walk_mut(q, &mut |q| {
        if let Query::OsNeural(n) = q {
            match &n.model_id {
                Some(m) if !wanted.iter().any(|(wm, wt)| wm == m && *wt == n.query_text) => wanted.push((m.clone(), n.query_text.clone())),
                Some(_) => {}
                None => missing = true,
            }
        }
    });
    if missing {
        return Err(EsError::illegal_argument("Invalid neural query: model_id must be provided."));
    }
    if wanted.is_empty() {
        return Ok(());
    }
    admit(ctx, &wanted)?;
    let p = crate::xpack::security::principal(ctx);
    let mut vectors: Vec<((String, String), Vec<f32>)> = Vec::new();
    for (model, text) in wanted {
        let v = crate::opensearch::ml::embed(ctx.cluster, &model, std::slice::from_ref(&text), Some(p), true).await?;
        vectors.push(((model, text), v.into_iter().next().unwrap_or_default()));
    }
    walk_mut(q, &mut |q| {
        if let Query::OsNeural(n) = q
            && let Some(m) = &n.model_id
            && let Some((_, v)) = vectors.iter().find(|((vm, vt), _)| vm == m && *vt == n.query_text)
        {
            *q = Query::OsKnn(Box::new(n.knn(v.clone())));
        }
    });
    Ok(())
}

/// A browser key's embeddings, before any call: only the models its
/// `limits.inference_endpoints` name, each text within the limits, counted against
/// `max_inference` and its rate limits.
fn admit(ctx: &Ctx, wanted: &[(String, String)]) -> EsResult<()> {
    use crate::xpack::security::browser;
    let Some(sec) = crate::xpack::security::dls::ReadSecurity::of(ctx) else { return Ok(()) };
    let Some(p) = sec.browser() else { return Ok(()) };
    if let Some((m, _)) = wanted.iter().find(|(m, _)| !p.inference_endpoints.contains(m)) {
        return Err(browser::refused(format!("model [{m}] (a browser key embeds only with the models its limits.inference_endpoints name)")));
    }
    for (_, t) in wanted {
        browser::check_inference_text(t)?;
    }
    sec.spend_inference(wanted.len())?;
    browser::charge_work(ctx, "inference", wanted.len() as u64)
}
