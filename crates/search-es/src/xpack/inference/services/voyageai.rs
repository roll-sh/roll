//! The `voyageai` service: embeddings (`/v1/embeddings`) and rerank (`/v1/rerank`) on
//! `api.voyageai.com`, as Elasticsearch 9.5.5's `VoyageAIService` sends them. The node's
//! `roll.inference.voyageai.url` replaces the host.

use super::{Config, json_bearer, learned, missing_field, ordered, post_json};
use crate::cluster::Cluster;
use crate::error::EsResult;
use crate::json::{Map, Value};
use crate::xpack::inference::results::Results;
use crate::xpack::inference::settings::{Reader, validation};
use crate::xpack::inference::{Endpoint, Request, TaskType};

const ORDER: &[&str] = &["model_id", "rate_limit", "similarity", "dimensions", "max_input_tokens", "embedding_type"];

pub fn parse(cluster: &Cluster, task: TaskType, cfg: &mut Config) -> EsResult<(Map, Map, Map)> {
    let mut r = Reader::new(std::mem::take(&mut cfg.service_settings), "service_settings");
    let mut ss = Map::new();
    ss.insert("model_id".into(), r.required_str("model_id").into());
    ss.insert("rate_limit".into(), r.rate_limit(2000));
    if task == TaskType::TextEmbedding {
        if let Some(s) = r.similarity() {
            ss.insert("similarity".into(), s.into());
        }
        if let Some(d) = r.optional_positive_int("dimensions") {
            ss.insert("dimensions".into(), d.into());
        }
        if let Some(m) = r.optional_positive_int("max_input_tokens") {
            ss.insert("max_input_tokens".into(), m.into());
        }
        ss.insert("embedding_type".into(), r.optional_enum("embedding_type", &["float", "int8", "bit"]).unwrap_or_else(|| "float".into()).into());
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

/// Embeddings: `input_type`, `truncation`. Rerank: `top_k`, `return_documents`, `truncation`.
pub fn task_settings(task: TaskType, ts: &mut Map) -> EsResult<Map> {
    let mut r = Reader::new(std::mem::take(ts), "task_settings");
    let mut out = Map::new();
    if task == TaskType::TextEmbedding {
        if let Some(it) = r.optional_enum("input_type", &["ingest", "search"]) {
            out.insert("input_type".into(), it.into());
        }
    } else {
        if let Some(k) = r.optional_positive_int("top_k") {
            out.insert("top_k".into(), k.into());
        }
        if let Some(b) = r.optional_bool("return_documents") {
            out.insert("return_documents".into(), b.into());
        }
    }
    if let Some(t) = r.optional_bool("truncation") {
        out.insert("truncation".into(), t.into());
    }
    *ts = r.finish()?;
    Ok(out)
}

pub fn embedding_details(ep: &mut Endpoint, size: usize) {
    let sim = if ep.service_settings.get("embedding_type").and_then(Value::as_str) == Some("bit") { "l2_norm" } else { "dot_product" };
    learned(ep, size, sim, ORDER);
}

fn effective(ep: &Endpoint, req: &Request) -> EsResult<Map> {
    let mut over = req.task_settings.clone();
    let parsed = task_settings(ep.task, &mut over)?;
    Ok(super::merge(&ep.task_settings, &parsed))
}

pub async fn infer(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Results> {
    let ts = effective(ep, req)?;
    let base = super::base_url(cluster, "voyageai", "https://api.voyageai.com");
    let model = ep.service_settings.get("model_id").cloned().unwrap_or(Value::Null);
    let headers = json_bearer(cluster, ep, true)?;
    let what = "VoyageAI";
    match ep.task {
        TaskType::TextEmbedding => {
            let mut b = Map::new();
            b.insert("input".into(), super::strings(&req.input));
            b.insert("model".into(), model);
            match super::input_type(req, &ts).as_deref() {
                Some("ingest" | "internal_ingest") => b.insert("input_type".into(), "document".into()),
                Some("search" | "internal_search") => b.insert("input_type".into(), "query".into()),
                _ => None,
            };
            if let Some(t) = ts.get("truncation") {
                b.insert("truncation".into(), t.clone());
            }
            if let Some(d) = ep.service_settings.get("dimensions") {
                b.insert("output_dimension".into(), d.clone());
            }
            let ty = ep.service_settings.get("embedding_type").and_then(Value::as_str).unwrap_or("float");
            b.insert("output_dtype".into(), (if ty == "bit" { "binary" } else { ty }).into());
            let v = post_json(cluster, ep, "voyageai", format!("{base}/v1/embeddings"), headers, &Value::Obj(b), req.timeout).await?;
            let data = v.get("data").and_then(Value::as_array).ok_or_else(|| missing_field("data", "VoyageAI embeddings"))?;
            let list: Vec<Value> = data.iter().map(|d| d.get("embedding").cloned().unwrap_or(Value::Null)).collect();
            match ty {
                "float" => super::float_vectors(&list, req.input.len(), "VoyageAI embeddings"),
                other => super::byte_vectors(&list, req.input.len(), other == "bit", "VoyageAI embeddings"),
            }
        }
        TaskType::Rerank => {
            let mut b = Map::new();
            b.insert("model".into(), model);
            b.insert("query".into(), req.query.clone().unwrap_or_default().into());
            b.insert("documents".into(), super::strings(&req.input));
            if let Some(r) = req.return_documents.map(Value::from).or_else(|| ts.get("return_documents").cloned()) {
                b.insert("return_documents".into(), r);
            }
            if let Some(k) = req.top_n.map(Value::from).or_else(|| ts.get("top_k").cloned()) {
                b.insert("top_k".into(), k);
            }
            if let Some(t) = ts.get("truncation") {
                b.insert("truncation".into(), t.clone());
            }
            let v = post_json(cluster, ep, "voyageai", format!("{base}/v1/rerank"), headers, &Value::Obj(b), req.timeout).await?;
            let data = v.get("data").and_then(Value::as_array).ok_or_else(|| missing_field("data", "VoyageAI rerank"))?;
            super::ranked(data, req.input.len(), "relevance_score", |r| r.get("document").and_then(Value::as_str).map(str::to_owned), what)
        }
        other => Err(crate::xpack::inference::unsupported_task(other, "voyageai")),
    }
}
