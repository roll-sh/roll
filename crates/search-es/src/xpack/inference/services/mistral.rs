//! The `mistral` service: embeddings (`/v1/embeddings`, `encoding_format: float`) and chat
//! completions (`/v1/chat/completions`) on `api.mistral.ai`, as Elasticsearch 9.5.5's
//! `MistralService` sends them. The node's `roll.inference.mistral.url` replaces the host.

use super::{Config, bearer, json_bearer, learned, oai, one_input, ordered, post_json};
use crate::cluster::Cluster;
use crate::error::EsResult;
use crate::json::{Map, Value};
use crate::obj;
use crate::xpack::inference::http::{self, Call};
use crate::xpack::inference::results::Results;
use crate::xpack::inference::stream::Streaming;
use crate::xpack::inference::settings::{Reader, validation};
use crate::xpack::inference::{Endpoint, Request, TaskType};
use std::time::Duration;

const ORDER: &[&str] = &["model", "dimensions", "similarity", "max_input_tokens", "rate_limit"];

pub fn parse(cluster: &Cluster, task: TaskType, cfg: &mut Config) -> EsResult<(Map, Map, Map)> {
    let mut r = Reader::new(std::mem::take(&mut cfg.service_settings), "service_settings");
    let mut ss = Map::new();
    ss.insert("model".into(), r.required_str("model").into());
    if task == TaskType::TextEmbedding {
        if let Some(d) = r.optional_positive_int("dimensions") {
            ss.insert("dimensions".into(), d.into());
        }
        if let Some(s) = r.similarity() {
            ss.insert("similarity".into(), s.into());
        }
        if let Some(m) = r.optional_positive_int("max_input_tokens") {
            ss.insert("max_input_tokens".into(), m.into());
        }
    }
    ss.insert("rate_limit".into(), r.rate_limit(240));
    r.check()?;
    let mut errors = Vec::new();
    let secrets = super::api_key_secret(cluster, &mut r.map, &mut errors);
    if !errors.is_empty() {
        return Err(validation(&errors));
    }
    cfg.service_settings = r.map;
    Ok((ordered(ss, ORDER), Map::new(), secrets))
}

pub fn embedding_details(ep: &mut Endpoint, size: usize) {
    ep.service_settings.insert("dimensions".into(), (size as i64).into());
    learned(ep, size, "dot_product", ORDER);
}

fn base(cluster: &Cluster) -> String {
    super::base_url(cluster, "mistral", "https://api.mistral.ai")
}

fn style(ep: &Endpoint) -> oai::Style<'_> {
    oai::Style { model: ep.service_settings.get("model"), max_tokens_key: "max_tokens", stream_false: true, ..oai::Style::default() }
}

async fn chat(cluster: &Cluster, ep: &Endpoint, body: &Value, timeout: Duration, stream: bool) -> EsResult<http::Answer> {
    let url = format!("{}/v1/chat/completions", base(cluster));
    let headers = bearer(cluster, ep, vec![("Content-Type".into(), "application/json".into())])?;
    let call = Call::json(&ep.id, url.clone(), headers, body, timeout, ep.requests_per_minute());
    let answer = if stream { http::post_stream(cluster, &call).await? } else { http::post(cluster, &call).await? };
    http::check(&ep.id, &url, &answer)?;
    Ok(answer)
}

pub async fn infer(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Results> {
    match ep.task {
        TaskType::TextEmbedding => {
            let body = obj! { "model" => ep.service_settings.get("model").cloned().unwrap_or(Value::Null), "input" => super::strings(&req.input), "encoding_format" => "float" };
            let v = post_json(cluster, ep, "mistral", format!("{}/v1/embeddings", base(cluster)), json_bearer(cluster, ep, false)?, &body, req.timeout).await?;
            super::openai::embeddings(&v, req.input.len())
        }
        TaskType::Completion => {
            let input = one_input(req, "Mistral completion")?;
            let answer = chat(cluster, ep, &oai::body(style(ep), oai::user_message(input), &Map::new(), false), req.timeout, false).await?;
            Ok(Results::Completion(vec![oai::completion_text(&super::answer_json("mistral", &answer)?, "Mistral chat completions")?]))
        }
        other => Err(crate::xpack::inference::unsupported_task(other, "mistral")),
    }
}

pub async fn stream_completion(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Streaming> {
    let input = one_input(req, "Mistral completion")?;
    let answer = chat(cluster, ep, &oai::body(style(ep), oai::user_message(input), &Map::new(), true), req.timeout, true).await?;
    Streaming::open(answer, oai::completion_decoder("Mistral chat completions"))
}

pub async fn stream_chat(cluster: &Cluster, ep: &Endpoint, body: &Value, timeout: Duration) -> EsResult<Streaming> {
    let (messages, req) = oai::unified(body)?;
    let answer = chat(cluster, ep, &oai::body(style(ep), messages, &req, true), timeout, true).await?;
    Streaming::open(answer, oai::chat_decoder(&ep.id))
}
