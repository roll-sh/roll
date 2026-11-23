//! The `hugging_face` service: a Hugging Face Inference Endpoint (or any Text Embeddings
//! Inference / Text Generation Inference server) at the endpoint's `url`, fetched through
//! [`crate::xpack::inference::http`]'s guarded client: dense and sparse embeddings and rerank
//! (`{"inputs": …}`, `{"texts": …, "query": …}`) and OpenAI-shaped chat completions, as
//! Elasticsearch 9.5.5's `HuggingFaceService` sends them.

use super::{Config, bearer, learned, missing_field, oai, one_input, ordered};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::xpack::inference::http::{self, Call};
use crate::xpack::inference::results::{Results, Sparse};
use crate::xpack::inference::stream::Streaming;
use crate::xpack::inference::settings::{Reader, validation};
use crate::xpack::inference::{Endpoint, Request, TaskType};
use std::time::Duration;

const ORDER: &[&str] = &["model_id", "url", "similarity", "dimensions", "max_input_tokens", "rate_limit"];

pub fn parse(cluster: &Cluster, task: TaskType, cfg: &mut Config) -> EsResult<(Map, Map, Map)> {
    let mut r = Reader::new(std::mem::take(&mut cfg.service_settings), "service_settings");
    let mut ss = Map::new();
    if matches!(task, TaskType::Completion | TaskType::ChatCompletion)
        && let Some(m) = r.optional_str("model_id")
    {
        ss.insert("model_id".into(), m.into());
    }
    let url = r.optional_url("url");
    if url.is_none() && !r.has_errors() {
        r.error("[service_settings] does not contain the required setting [url]");
    }
    ss.insert("url".into(), url.into());
    match task {
        TaskType::TextEmbedding => {
            if let Some(s) = r.similarity() {
                ss.insert("similarity".into(), s.into());
            }
            if let Some(d) = r.optional_positive_int("dimensions") {
                ss.insert("dimensions".into(), d.into());
            }
            if let Some(m) = r.optional_positive_int("max_input_tokens") {
                ss.insert("max_input_tokens".into(), m.into());
            }
        }
        TaskType::SparseEmbedding => {
            ss.insert("max_input_tokens".into(), r.optional_positive_int("max_input_tokens").unwrap_or(512).into());
        }
        _ => {}
    }
    ss.insert("rate_limit".into(), r.rate_limit(3000));
    r.check()?;
    let mut errors = Vec::new();
    let secrets = super::api_key_secret(cluster, &mut r.map, &mut errors);
    if !errors.is_empty() {
        return Err(validation(&errors));
    }
    cfg.service_settings = r.map;
    // Elasticsearch keeps no chunking settings for Hugging Face's sparse endpoints.
    if task == TaskType::SparseEmbedding {
        cfg.chunking = None;
    }
    let ts = task_settings(task, &mut cfg.task_settings)?;
    Ok((ordered(ss, ORDER), ts, secrets))
}

/// Rerank: `top_n`, `return_documents`; the other tasks take none.
pub fn task_settings(task: TaskType, ts: &mut Map) -> EsResult<Map> {
    let mut out = Map::new();
    if task != TaskType::Rerank {
        return Ok(out);
    }
    let mut r = Reader::new(std::mem::take(ts), "task_settings");
    if let Some(n) = r.optional_positive_int("top_n") {
        out.insert("top_n".into(), n.into());
    }
    if let Some(b) = r.optional_bool("return_documents") {
        out.insert("return_documents".into(), b.into());
    }
    *ts = r.finish()?;
    Ok(out)
}

pub fn embedding_details(ep: &mut Endpoint, size: usize) {
    learned(ep, size, "cosine", ORDER);
}

async fn send(cluster: &Cluster, ep: &Endpoint, body: &Value, timeout: Duration, stream: bool) -> EsResult<http::Answer> {
    let url = ep.service_settings.get("url").and_then(Value::as_str).unwrap_or_default().to_string();
    let headers = bearer(cluster, ep, vec![("Content-Type".into(), "application/json".into())])?;
    let call = Call::json(&ep.id, url.clone(), headers, body, timeout, ep.requests_per_minute());
    let answer = if stream { http::post_stream(cluster, &call).await? } else { http::post(cluster, &call).await? };
    http::check(&ep.id, &url, &answer)?;
    Ok(answer)
}

fn style(ep: &Endpoint) -> oai::Style<'_> {
    oai::Style { model: ep.service_settings.get("model_id"), max_tokens_key: "max_completion_tokens", stream_false: true, stream_options: true, ..oai::Style::default() }
}

pub async fn infer(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Results> {
    let json = |a: &http::Answer| super::answer_json("hugging_face", a);
    match ep.task {
        TaskType::TextEmbedding => {
            let v = json(&send(cluster, ep, &obj! { "inputs" => super::strings(&req.input) }, req.timeout, false).await?)?;
            // Text Embeddings Inference answers a list of vectors; some servers wrap it.
            let list = v.as_array().or_else(|| v.get("embeddings").and_then(Value::as_array)).ok_or_else(|| missing_field("embeddings", "Hugging Face embeddings"))?;
            super::float_vectors(list, req.input.len(), "Hugging Face embeddings")
        }
        TaskType::SparseEmbedding => {
            let v = json(&send(cluster, ep, &obj! { "inputs" => super::strings(&req.input) }, req.timeout, false).await?)?;
            let list = v.as_array().ok_or_else(|| missing_field("embeddings", "Hugging Face sparse embeddings"))?;
            if list.len() != req.input.len() {
                return Err(EsError::java(500, "illegal_state_exception", format!("The Hugging Face sparse embeddings response holds [{}] embeddings for [{}] inputs", list.len(), req.input.len())));
            }
            let mut out = Vec::with_capacity(list.len());
            for e in list {
                let m = e.as_object().ok_or_else(|| missing_field("embeddings", "Hugging Face sparse embeddings"))?;
                let tokens = m.iter().map(|(t, w)| w.as_f64().map(|w| (t.clone(), w as f32))).collect::<Option<Vec<_>>>().ok_or_else(|| missing_field("embeddings", "Hugging Face sparse embeddings"))?;
                out.push(Sparse { tokens, truncated: false });
            }
            Ok(Results::Sparse(out))
        }
        TaskType::Rerank => {
            let ts = {
                let mut over = req.task_settings.clone();
                super::merge(&ep.task_settings, &task_settings(ep.task, &mut over)?)
            };
            let mut b = Map::new();
            b.insert("texts".into(), super::strings(&req.input));
            b.insert("query".into(), req.query.clone().unwrap_or_default().into());
            if let Some(r) = req.return_documents.map(Value::from).or_else(|| ts.get("return_documents").cloned()) {
                b.insert("return_text".into(), r);
            }
            if let Some(n) = req.top_n.map(Value::from).or_else(|| ts.get("top_n").cloned()) {
                b.insert("top_n".into(), n);
            }
            let top_n = req.top_n.or_else(|| ts.get("top_n").and_then(Value::as_i64));
            let v = json(&send(cluster, ep, &Value::Obj(b), req.timeout, false).await?)?;
            let list = v.as_array().ok_or_else(|| missing_field("score", "Hugging Face rerank"))?;
            let mut ranked = super::ranked(list, req.input.len(), "score", |r| r.get("text").and_then(Value::as_str).map(str::to_owned), "Hugging Face rerank")?;
            // The server may answer every document; the endpoint keeps its top `top_n`.
            if let (Results::Rerank(r), Some(n)) = (&mut ranked, top_n) {
                r.truncate(n.max(0) as usize);
            }
            Ok(ranked)
        }
        TaskType::Completion => {
            let input = one_input(req, "Hugging Face completion")?;
            let answer = send(cluster, ep, &oai::body(style(ep), oai::user_message(input), &Map::new(), false), req.timeout, false).await?;
            Ok(Results::Completion(vec![oai::completion_text(&json(&answer)?, "Hugging Face chat completions")?]))
        }
        other => Err(crate::xpack::inference::unsupported_task(other, "hugging_face")),
    }
}

pub async fn stream_completion(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Streaming> {
    let input = one_input(req, "Hugging Face completion")?;
    let answer = send(cluster, ep, &oai::body(style(ep), oai::user_message(input), &Map::new(), true), req.timeout, true).await?;
    Streaming::open(answer, oai::completion_decoder("Hugging Face chat completions"))
}

pub async fn stream_chat(cluster: &Cluster, ep: &Endpoint, body: &Value, timeout: Duration) -> EsResult<Streaming> {
    let (messages, req) = oai::unified(body)?;
    let answer = send(cluster, ep, &oai::body(style(ep), messages, &req, true), timeout, true).await?;
    Streaming::open(answer, oai::chat_decoder(&ep.id))
}
