//! The `cohere` service: embeddings (`/v2/embed`), rerank (`/v2/rerank`) and completions
//! (`/v2/chat`), as Elasticsearch 9.5.5's `CohereService` sends them for endpoints created now
//! (API version `V2`). Its host is `api.cohere.ai`; `service_settings.url` (which Elasticsearch
//! reads too) or the node's `roll.inference.cohere.url` point it elsewhere.

use super::{Config, answer_json, base_url, missing_field};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::xpack::inference::http::{self, Call};
use crate::xpack::inference::results::{Ranked, Results};
use crate::xpack::inference::settings::{Reader, validation};
use crate::xpack::inference::{Endpoint, Request, TaskType};

const HOST: &str = "https://api.cohere.ai";

/// `embedding_type` values and the `embedding_types` entry each asks Cohere for.
const EMBEDDING_TYPES: &[(&str, &str)] = &[("float", "float"), ("int8", "int8"), ("byte", "int8"), ("bit", "binary"), ("binary", "binary")];

pub fn parse(cluster: &Cluster, task: TaskType, cfg: &mut Config) -> EsResult<(Map, Map, Map)> {
    let mut r = Reader::new(std::mem::take(&mut cfg.service_settings), "service_settings");
    let old_model = r.optional_str("model");
    let model = r.optional_str("model_id").or(old_model);
    let url = r.optional_url("url");
    let rate = r.rate_limit(10_000);
    let (mut similarity, mut dims, mut max_input, mut embedding_type) = (None, None, None, None);
    if task == TaskType::TextEmbedding {
        similarity = r.similarity();
        dims = r.optional_int("dimensions");
        max_input = r.optional_int("max_input_tokens");
        embedding_type = r.optional_enum("embedding_type", &["float", "int8", "byte", "bit", "binary"]);
    }
    let mut errors = Vec::new();
    let secrets = super::api_key_secret(cluster, &mut r.map, &mut errors);
    for e in errors {
        r.error(e);
    }
    r.check()?;
    if model.is_none() {
        return Err(EsError::java(400, "illegal_argument_exception", "The [service_settings.model_id] field is required for the Cohere V2 API."));
    }
    let mut ss = Map::new();
    ss.insert("model_id".into(), model.into());
    ss.insert("rate_limit".into(), rate);
    if task == TaskType::TextEmbedding {
        if let Some(s) = similarity {
            ss.insert("similarity".into(), s.into());
        }
        if let Some(d) = dims {
            ss.insert("dimensions".into(), d.into());
        }
        if let Some(m) = max_input {
            ss.insert("max_input_tokens".into(), m.into());
        }
        ss.insert("embedding_type".into(), embedding_type.unwrap_or_else(|| "float".into()).into());
    }
    if let Some(u) = url {
        ss.insert("url".into(), u.into());
    }
    ss.insert("api_version".into(), "V2".into());
    cfg.service_settings = r.map;
    let ts = task_settings(task, &mut cfg.task_settings)?;
    Ok((ss, ts, secrets))
}

/// The task settings of each task: embeddings `input_type`, `truncate`; rerank `top_n`,
/// `return_documents`, `max_chunks_per_doc`.
pub fn task_settings(task: TaskType, ts: &mut Map) -> EsResult<Map> {
    let mut r = Reader::new(std::mem::take(ts), "task_settings");
    let mut out = Map::new();
    match task {
        TaskType::TextEmbedding => {
            if let Some(it) = r.optional_str("input_type") {
                let lower = it.to_ascii_lowercase();
                if !matches!(lower.as_str(), "ingest" | "search" | "classification" | "clustering") {
                    return Err(validation(&[format!(
                        "[task_settings] Invalid value [{it}] received. [input_type] must be one of [classification, clustering, ingest, search]"
                    )]));
                }
                out.insert("input_type".into(), lower.into());
            }
            if let Some(t) = r.optional_enum("truncate", &["none", "start", "end"]) {
                out.insert("truncate".into(), t.to_ascii_uppercase().into());
            }
        }
        TaskType::Rerank => {
            if let Some(n) = r.optional_positive_int("top_n") {
                out.insert("top_n".into(), n.into());
            }
            if let Some(b) = r.optional_bool("return_documents") {
                out.insert("return_documents".into(), b.into());
            }
            if let Some(n) = r.optional_positive_int("max_chunks_per_doc") {
                out.insert("max_chunks_per_doc".into(), n.into());
            }
        }
        _ => {}
    }
    *ts = r.finish()?;
    Ok(out)
}

/// An update may change the rate limit (and `max_input_tokens` of embeddings).
pub fn update(ep: &mut Endpoint, ss: &mut Map) -> EsResult<()> {
    let mut r = Reader::new(std::mem::take(ss), "service_settings");
    let current = ep.service_settings.get("rate_limit").and_then(|v| v.get("requests_per_minute")).and_then(Value::as_i64).unwrap_or(10_000);
    let rate = r.rate_limit(current);
    let max_input = if ep.task == TaskType::TextEmbedding { r.optional_int("max_input_tokens") } else { None };
    *ss = r.finish()?;
    ep.service_settings.insert("rate_limit".into(), rate);
    if let Some(m) = max_input {
        ep.service_settings.insert("max_input_tokens".into(), m.into());
    }
    Ok(())
}

/// After validation: the dimensions, and the default similarity of the embedding type.
pub fn embedding_details(ep: &mut Endpoint, size: usize) {
    let ty = ep.service_settings.get("embedding_type").and_then(Value::as_str).unwrap_or("float").to_string();
    let ss = &mut ep.service_settings;
    if !ss.contains_key("similarity") {
        let sim = if matches!(ty.as_str(), "bit" | "binary") { "l2_norm" } else { "cosine" };
        ss.shift_insert(2, "similarity".into(), sim.into());
    }
    let pos = ss.get_index_of("similarity").map_or(2, |i| i + 1);
    ss.shift_remove("dimensions");
    ss.shift_insert(pos, "dimensions".into(), (size as i64).into());
}

fn endpoint_url(cluster: &Cluster, ep: &Endpoint, path: &str) -> String {
    let base = match ep.service_settings.get("url").and_then(Value::as_str) {
        Some(u) => u.trim_end_matches('/').to_string(),
        None => base_url(cluster, "cohere", HOST),
    };
    format!("{base}/v2/{path}")
}

async fn send(cluster: &Cluster, ep: &Endpoint, path: &str, body: Value, timeout: std::time::Duration) -> EsResult<Value> {
    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    if let Some(k) = ep.secret(cluster, "api_key")? {
        headers.push(("Authorization".into(), format!("Bearer {k}")));
    }
    headers.push(("Request-Source".into(), "unspecified:elasticsearch".into()));
    let url = endpoint_url(cluster, ep, path);
    let call = Call { inference_id: &ep.id, url: url.clone(), headers, body: body.to_json().into_bytes(), timeout, retry: http::retry_server_errors, per_minute: ep.requests_per_minute() };
    let answer = http::post(cluster, &call).await?;
    http::check(&ep.id, &url, &answer)?;
    answer_json("cohere", &answer)
}

/// `CohereUtils.inputTypeToString`.
fn input_type(it: &str) -> Option<&'static str> {
    Some(match it.to_ascii_lowercase().as_str() {
        "ingest" | "internal_ingest" => "search_document",
        "search" | "internal_search" => "search_query",
        "classification" => "classification",
        "clustering" => "clustering",
        _ => return None,
    })
}

pub async fn infer(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Results> {
    let model = ep.service_settings.get("model_id").cloned().unwrap_or(Value::Null);
    let texts = Value::Arr(req.input.iter().map(|s| Value::from(s.as_str())).collect());
    match ep.task {
        TaskType::TextEmbedding => {
            let mut over = req.task_settings.clone();
            let ts = super::merge(&ep.task_settings, &task_settings(ep.task, &mut over)?);
            let ty = ep.service_settings.get("embedding_type").and_then(Value::as_str).unwrap_or("float");
            let wire = EMBEDDING_TYPES.iter().find(|(t, _)| *t == ty).map_or("float", |(_, w)| *w);
            let mut body = Map::new();
            body.insert("texts".into(), texts);
            body.insert("model".into(), model);
            let it = req.input_type.as_deref().and_then(input_type).or_else(|| ts.get("input_type").and_then(Value::as_str).and_then(input_type));
            if let Some(it) = it {
                body.insert("input_type".into(), it.into());
            }
            body.insert("embedding_types".into(), crate::arr![wire]);
            if let Some(t) = ts.get("truncate") {
                body.insert("truncate".into(), t.clone());
            }
            let v = send(cluster, ep, "embed", Value::Obj(body), req.timeout).await?;
            embeddings(&v, wire, req.input.len())
        }
        TaskType::Rerank => {
            let mut over = req.task_settings.clone();
            let ts = super::merge(&ep.task_settings, &task_settings(ep.task, &mut over)?);
            let mut body = Map::new();
            body.insert("model".into(), model);
            body.insert("query".into(), req.query.clone().unwrap_or_default().into());
            body.insert("documents".into(), texts);
            if let Some(b) = req.return_documents.map(Value::from).or_else(|| ts.get("return_documents").cloned()) {
                body.insert("return_documents".into(), b);
            }
            if let Some(n) = req.top_n.map(Value::from).or_else(|| ts.get("top_n").cloned()) {
                body.insert("top_n".into(), n);
            }
            if let Some(n) = ts.get("max_chunks_per_doc") {
                body.insert("max_chunks_per_doc".into(), n.clone());
            }
            let v = send(cluster, ep, "rerank", Value::Obj(body), req.timeout).await?;
            let results = v.get("results").and_then(Value::as_array).ok_or_else(|| missing_field("results", "Cohere rerank"))?;
            let mut out = Vec::with_capacity(results.len());
            for r in results {
                let index = r.get("index").and_then(Value::as_i64).filter(|i| *i >= 0 && (*i as usize) < req.input.len()).ok_or_else(|| missing_field("index", "Cohere rerank"))?;
                let score = r.get("relevance_score").and_then(Value::as_f64).ok_or_else(|| missing_field("relevance_score", "Cohere rerank"))?;
                let text = r.pointer(&["document", "text"]).and_then(Value::as_str).map(str::to_owned);
                out.push(Ranked { index: index as usize, score: score as f32, text });
            }
            Ok(Results::Rerank(out))
        }
        TaskType::Completion => {
            if req.input.len() != 1 {
                return Err(EsError::status(400, "Cohere completion only accepts 1 input"));
            }
            let body = obj! {
                "messages" => crate::arr![obj! { "role" => "user", "content" => req.input[0].clone() }],
                "model" => model,
                "stream" => false,
            };
            let v = send(cluster, ep, "chat", body, req.timeout).await?;
            let text: String = v
                .pointer(&["message", "content"])
                .and_then(Value::as_array)
                .ok_or_else(|| missing_field("message", "Cohere completion"))?
                .iter()
                .filter_map(|c| c.get("text").and_then(Value::as_str))
                .collect();
            Ok(Results::Completion(vec![text]))
        }
        other => Err(crate::xpack::inference::unsupported_task(other, "cohere")),
    }
}

/// `embeddings` of a v2 answer (by type), or a v1 answer's plain list of float vectors.
fn embeddings(v: &Value, wire: &str, expected: usize) -> EsResult<Results> {
    let e = v.get("embeddings").ok_or_else(|| missing_field("embeddings", "Cohere embeddings"))?;
    let list = match e {
        Value::Obj(by_type) => by_type.get(wire).and_then(Value::as_array).ok_or_else(|| missing_field(wire, "Cohere embeddings"))?,
        Value::Arr(a) => a,
        _ => return Err(missing_field("embeddings", "Cohere embeddings")),
    };
    if list.len() != expected {
        return Err(EsError::java(500, "illegal_state_exception", format!("The Cohere embeddings response holds [{}] embeddings for [{expected}] inputs", list.len())));
    }
    let nums = |x: &Value| -> EsResult<Vec<f64>> {
        x.as_array().ok_or_else(|| missing_field("embeddings", "Cohere embeddings"))?.iter().map(|n| n.as_f64().ok_or_else(|| missing_field("embeddings", "Cohere embeddings"))).collect()
    };
    Ok(match wire {
        "int8" => Results::Bytes(list.iter().map(|x| nums(x).map(|v| v.into_iter().map(|f| f.clamp(-128.0, 127.0) as i8).collect())).collect::<EsResult<_>>()?),
        "binary" => Results::Bits(list.iter().map(|x| nums(x).map(|v| v.into_iter().map(|f| f.clamp(-128.0, 127.0) as i8).collect())).collect::<EsResult<_>>()?),
        _ => Results::Floats(list.iter().map(|x| nums(x).map(|v| v.into_iter().map(|f| f as f32).collect())).collect::<EsResult<_>>()?),
    })
}
