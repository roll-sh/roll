//! The `text_similarity_reranker` retriever (`TextSimilarityRankRetrieverBuilder`): its child
//! retriever's top `rank_window_size` hits (10) are scored again by a rerank endpoint against
//! `inference_text` (each value of `field` one input, a document scoring its best input), then
//! ordered by those scores, cut by `min_score` and paged by the request's `from`/`size`.
//!
//! roll runs it on the coordinator around the search: the child retriever's search returns the
//! window, the endpoint ranks it, and the page is cut from that. The reranker is supported at the
//! top of the request's `retriever` (`field` read from `_source`).

use crate::error::{EsError, EsResult};
use crate::json::Value;

/// A parsed `text_similarity_reranker`.
#[derive(Clone, Debug)]
pub struct Reranker {
    pub field: String,
    pub inference_id: String,
    pub text: String,
    pub window: i64,
    pub min_score: Option<f32>,
    /// `allow_rerank_failures`: a failed rerank keeps the child's order.
    pub allow_failures: bool,
    /// The request's own paging, applied after reranking.
    pub from: i64,
    pub size: i64,
}

/// `.rerank-v1-elasticsearch`, the default endpoint (`DEFAULT_RERANK_ID`).
const DEFAULT_ID: &str = ".rerank-v1-elasticsearch";

fn parse_err(msg: String) -> EsError {
    EsError::x_content_parse(1, 1, msg)
}

/// Takes a top-level `text_similarity_reranker` out of a search body: the body keeps its child
/// retriever and asks for the window from the first hit.
pub fn take(body: &mut Option<Value>) -> EsResult<Option<Reranker>> {
    let Some(Value::Obj(b)) = body else { return Ok(None) };
    let Some(Value::Obj(r)) = b.get("retriever") else { return Ok(None) };
    let Some(Value::Obj(spec)) = r.get("text_similarity_reranker") else { return Ok(None) };
    let spec = spec.clone();
    for k in spec.keys() {
        if !matches!(k.as_str(), "retriever" | "inference_id" | "inference_text" | "field" | "rank_window_size" | "min_score" | "filter" | "_name" | "allow_rerank_failures" | "chunk_rescorer") {
            return Err(parse_err(format!("[text_similarity_reranker] unknown field [{k}]")));
        }
    }
    let child = spec.get("retriever").cloned().ok_or_else(|| EsError::illegal_argument("[text_similarity_reranker] retriever should have exactly one inner retriever"))?;
    let need = |k: &str| spec.get(k).map(Value::to_es_string).ok_or_else(|| EsError::illegal_argument(format!("Required [{k}]")));
    let rr = Reranker {
        field: need("field")?,
        inference_id: spec.get("inference_id").map(Value::to_es_string).unwrap_or_else(|| DEFAULT_ID.into()),
        text: need("inference_text")?,
        window: spec.get("rank_window_size").and_then(Value::as_i64).unwrap_or(10),
        min_score: spec.get("min_score").and_then(Value::as_f64).map(|f| f as f32),
        allow_failures: spec.get("allow_rerank_failures").and_then(Value::as_bool) == Some(true),
        from: b.get("from").and_then(Value::as_i64).unwrap_or(0),
        size: b.get("size").and_then(Value::as_i64).unwrap_or(10),
    };
    if rr.from + rr.size > rr.window {
        return Err(EsError::action_request_validation(&[format!(
            "[text_similarity_reranker] requires [rank_window_size: {}] be greater than or equal to [size: {}]",
            rr.window, rr.size
        )]));
    }
    // The child, with the reranker's filters, fetches the window.
    let mut child = child;
    if let Some(f) = spec.get("filter")
        && let Some(Value::Obj(inner)) = child.as_object_mut().and_then(|c| c.values_mut().next())
        && !inner.contains_key("filter")
    {
        inner.insert("filter".into(), f.clone());
    }
    b.insert("retriever".into(), child);
    b.insert("from".into(), 0.into());
    b.insert("size".into(), rr.window.into());
    Ok(Some(rr))
}

/// Reranks a rendered search answer's hits.
/// The endpoint's checks (`GetInferenceModelAction` with task `rerank`, the configured top N).
fn check_endpoint(ctx: &crate::rest::Ctx<'_>, rr: &Reranker) -> EsResult<()> {
    let st = ctx.state();
    let ep = super::endpoint(ctx.cluster, &st, &rr.inference_id).ok_or_else(|| EsError::resource_not_found(format!("Inference endpoint not found [{}]", rr.inference_id)))?;
    if ep.task != super::TaskType::Rerank {
        return Err(EsError::status(400, format!("Requested task type [rerank] does not match the inference endpoint's task type [{}]", ep.task.name())));
    }
    let top_n = if ep.task_settings.get("hide_top_n").and_then(Value::as_bool) == Some(true) { None } else { ep.task_settings.get("top_n").and_then(Value::as_i64) };
    if let Some(n) = top_n
        && n < rr.window
    {
        return Err(EsError::illegal_argument(format!(
            "Inference endpoint [{}] is configured to return the top [{n}] results, but rank_window_size is [{}]. Reduce rank_window_size to be less than or equal to the configured top N value.",
            rr.inference_id, rr.window
        )));
    }
    Ok(())
}

pub async fn apply(ctx: &crate::rest::Ctx<'_>, rr: &Reranker, answer: &mut Value) -> EsResult<()> {
    // The endpoint is checked whether or not there are hits to rerank.
    let checked = check_endpoint(ctx, rr);
    if let Err(e) = &checked
        && !rr.allow_failures
    {
        return Err(e.clone());
    }
    let Some(hits) = answer.as_object_mut().and_then(|a| a.get_mut("hits")).and_then(Value::as_object_mut).and_then(|h| h.get_mut("hits")).and_then(|v| match v { Value::Arr(a) => Some(a), _ => None }) else {
        return Ok(());
    };
    // Each value of the field is one input; a document scores its best input.
    let mut inputs: Vec<String> = Vec::new();
    let mut owner: Vec<usize> = Vec::new();
    for (i, h) in hits.iter().enumerate() {
        let values = h.get("_source").and_then(Value::as_object).and_then(|s| super::semantic::texts_at(s, &rr.field)).unwrap_or_default();
        let values = if values.is_empty() { vec![String::new()] } else { values };
        for v in values {
            inputs.push(v);
            owner.push(i);
        }
    }
    let mut scores = vec![f32::NEG_INFINITY; hits.len()];
    if !inputs.is_empty() {
        let mut req = super::Request::new(inputs, std::time::Duration::from_secs(30));
        req.query = Some(rr.text.clone());
        let run = match checked {
            Ok(()) => super::run(ctx.cluster, &rr.inference_id, &req).await,
            Err(e) => Err(e),
        };
        let ranked = match run {
            Ok((_, super::results::Results::Rerank(r))) => r,
            Ok(_) => return Err(EsError::illegal_state("Expected results to be of type [RankedDocsResults]")),
            // The child's own order and scores stand.
            Err(_) if rr.allow_failures => {
                let page: Vec<Value> = std::mem::take(hits).into_iter().skip(rr.from.max(0) as usize).take(rr.size.max(0) as usize).collect();
                *hits = page;
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        for d in ranked {
            if let Some(&doc) = owner.get(d.index)
                && d.score > scores[doc]
            {
                scores[doc] = d.score;
            }
        }
    }
    let mut ranked: Vec<(f32, usize, Value)> = std::mem::take(hits).into_iter().enumerate().map(|(i, h)| (scores[i], i, h)).collect();
    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)));
    if let Some(min) = rr.min_score {
        ranked.retain(|(s, _, _)| *s >= min);
    }
    let kept = ranked.len() as i64;
    let max = ranked.first().map(|(s, _, _)| *s);
    *hits = ranked
        .into_iter()
        .skip(rr.from.max(0) as usize)
        .take(rr.size.max(0) as usize)
        .map(|(s, _, mut h)| {
            if let Some(m) = h.as_object_mut() {
                m.insert("_score".into(), if s.is_finite() { crate::query::xparser::float_value(s) } else { Value::Null });
            }
            h
        })
        .collect();
    if let Some(h) = answer.as_object_mut().and_then(|a| a.get_mut("hits")).and_then(Value::as_object_mut) {
        h.insert("max_score".into(), max.filter(|m| m.is_finite()).map_or(Value::Null, crate::query::xparser::float_value));
        // With `min_score`, the total is the reranked documents that kept their place.
        if rr.min_score.is_some()
            && let Some(Value::Obj(t)) = h.get_mut("total")
        {
            t.insert("value".into(), kept.into());
            t.insert("relation".into(), "eq".into());
        }
    }
    Ok(())
}
