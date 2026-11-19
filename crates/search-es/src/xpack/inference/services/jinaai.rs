//! The `jinaai` service: embeddings (`/v1/embeddings`) and rerank (`/v1/rerank`) on
//! `api.jina.ai`, as Elasticsearch 9.5.5's `JinaAIService` sends them. The node's
//! `roll.inference.jinaai.url` replaces the host. Not supported: the multimodal `embedding` task.

use super::{Config, json_bearer, learned, missing_field, ordered, post_json};
use crate::cluster::Cluster;
use crate::error::EsResult;
use crate::json::{Map, Value};
use crate::xpack::inference::results::Results;
use crate::xpack::inference::settings::{Reader, validation};
use crate::xpack::inference::{Endpoint, Request, TaskType};

const ORDER: &[&str] = &["model_id", "rate_limit", "dimensions", "embedding_type", "similarity", "max_input_tokens", "dimensions_set_by_user"];

pub fn parse(cluster: &Cluster, task: TaskType, cfg: &mut Config) -> EsResult<(Map, Map, Map)> {
    let mut r = Reader::new(std::mem::take(&mut cfg.service_settings), "service_settings");
    let mut ss = Map::new();
    ss.insert("model_id".into(), r.required_str("model_id").into());
    ss.insert("rate_limit".into(), r.rate_limit(2000));
    if task == TaskType::TextEmbedding {
        let dims = r.optional_positive_int("dimensions");
        if let Some(d) = dims {
            ss.insert("dimensions".into(), d.into());
        }
        ss.insert("embedding_type".into(), r.optional_enum("embedding_type", &["float", "bit", "binary"]).unwrap_or_else(|| "float".into()).into());
        if let Some(s) = r.similarity() {
            ss.insert("similarity".into(), s.into());
        }
        if let Some(m) = r.optional_positive_int("max_input_tokens") {
            ss.insert("max_input_tokens".into(), m.into());
        }
        ss.insert("dimensions_set_by_user".into(), dims.is_some().into());
    }
    r.check()?;
    let mut errors = Vec::new();
    let secrets = super::api_key_secret(cluster, &mut r.map, &mut errors);
    if !errors.is_empty() {
        return Err(validation(&errors));
    }
    cfg.service_settings = r.map;
    let ts = task_settings(task, &mut cfg.task_settings)?;
    Ok((ordered(ss, ORDER), ts, secrets))
}

/// Embeddings: `input_type`, `late_chunking`. Rerank: `top_n`, `return_documents`.
pub fn task_settings(task: TaskType, ts: &mut Map) -> EsResult<Map> {
    let mut r = Reader::new(std::mem::take(ts), "task_settings");
    let mut out = Map::new();
    if task == TaskType::TextEmbedding {
        if let Some(it) = r.optional_enum("input_type", &["classification", "clustering", "ingest", "search"]) {
            out.insert("input_type".into(), it.into());
        }
        if let Some(b) = r.optional_bool("late_chunking") {
            out.insert("late_chunking".into(), b.into());
        }
    } else {
        if let Some(n) = r.optional_positive_int("top_n") {
            out.insert("top_n".into(), n.into());
        }
        if let Some(b) = r.optional_bool("return_documents") {
            out.insert("return_documents".into(), b.into());
        }
    }
    *ts = r.finish()?;
    Ok(out)
}

fn bits(ep: &Endpoint) -> bool {
    matches!(ep.service_settings.get("embedding_type").and_then(Value::as_str), Some("bit" | "binary"))
}

pub fn embedding_details(ep: &mut Endpoint, size: usize) {
    let sim = if bits(ep) { "l2_norm" } else { "dot_product" };
    learned(ep, size, sim, ORDER);
}

fn effective(ep: &Endpoint, req: &Request) -> EsResult<Map> {
    let mut over = req.task_settings.clone();
    let parsed = task_settings(ep.task, &mut over)?;
    Ok(super::merge(&ep.task_settings, &parsed))
}

pub async fn infer(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Results> {
    let ts = effective(ep, req)?;
    let base = super::base_url(cluster, "jinaai", "https://api.jina.ai");
    let model = ep.service_settings.get("model_id").cloned().unwrap_or(Value::Null);
    let headers = json_bearer(cluster, ep, true)?;
    match ep.task {
        TaskType::TextEmbedding => {
            let mut b = Map::new();
            b.insert("input".into(), super::strings(&req.input));
            b.insert("model".into(), model);
            b.insert("embedding_type".into(), (if bits(ep) { "binary" } else { "float" }).into());
            let task = match super::input_type(req, &ts).as_deref() {
                Some("ingest" | "internal_ingest") => Some("retrieval.passage"),
                Some("search" | "internal_search") => Some("retrieval.query"),
                Some("classification") => Some("classification"),
                Some("clustering") => Some("separation"),
                _ => None,
            };
            if let Some(t) = task {
                b.insert("task".into(), t.into());
            }
            if let Some(l) = ts.get("late_chunking") {
                b.insert("late_chunking".into(), l.clone());
            }
            if ep.service_settings.get("dimensions_set_by_user").and_then(Value::as_bool) == Some(true)
                && let Some(d) = ep.service_settings.get("dimensions")
            {
                b.insert("dimensions".into(), d.clone());
            }
            let v = post_json(cluster, ep, "jinaai", format!("{base}/v1/embeddings"), headers, &Value::Obj(b), req.timeout).await?;
            let data = v.get("data").and_then(Value::as_array).ok_or_else(|| missing_field("data", "Jina AI embeddings"))?;
            let list: Vec<Value> = data.iter().map(|d| d.get("embedding").cloned().unwrap_or(Value::Null)).collect();
            if bits(ep) { super::byte_vectors(&list, req.input.len(), true, "Jina AI embeddings") } else { super::float_vectors(&list, req.input.len(), "Jina AI embeddings") }
        }
        TaskType::Rerank => {
            let mut b = Map::new();
            b.insert("model".into(), model);
            b.insert("query".into(), req.query.clone().unwrap_or_default().into());
            b.insert("documents".into(), super::strings(&req.input));
            if let Some(n) = req.top_n.map(Value::from).or_else(|| ts.get("top_n").cloned()) {
                b.insert("top_n".into(), n);
            }
            if let Some(r) = req.return_documents.map(Value::from).or_else(|| ts.get("return_documents").cloned()) {
                b.insert("return_documents".into(), r);
            }
            let v = post_json(cluster, ep, "jinaai", format!("{base}/v1/rerank"), headers, &Value::Obj(b), req.timeout).await?;
            let results = v.get("results").and_then(Value::as_array).ok_or_else(|| missing_field("results", "Jina AI rerank"))?;
            super::ranked(results, req.input.len(), "relevance_score", |r| r.pointer(&["document", "text"]).and_then(Value::as_str).map(str::to_owned), "Jina AI rerank")
        }
        other => Err(crate::xpack::inference::unsupported_task(other, "jinaai")),
    }
}
