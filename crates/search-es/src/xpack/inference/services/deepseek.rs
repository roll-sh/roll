//! The `deepseek` service: chat completions on `https://api.deepseek.com/chat/completions`, or
//! the endpoint's `url` (fetched through [`crate::xpack::inference::http`]'s guarded client), as
//! Elasticsearch 9.5.5's `DeepSeekService` sends them. It has no `rate_limit` setting. The node's
//! `roll.inference.deepseek.url` replaces the default host.

use super::{Config, json_bearer, oai, one_input};
use crate::cluster::Cluster;
use crate::error::EsResult;
use crate::json::{Map, Value};
use crate::xpack::inference::http::{self, Call};
use crate::xpack::inference::results::Results;
use crate::xpack::inference::settings::{Reader, validation};
use crate::xpack::inference::{Endpoint, Request, TaskType};
use std::time::Duration;

pub fn parse(cluster: &Cluster, _task: TaskType, cfg: &mut Config) -> EsResult<(Map, Map, Map)> {
    let mut r = Reader::new(std::mem::take(&mut cfg.service_settings), "service_settings");
    let mut ss = Map::new();
    ss.insert("model_id".into(), r.required_str("model_id").into());
    if let Some(u) = r.optional_url("url") {
        ss.insert("url".into(), u.into());
    }
    r.check()?;
    let mut errors = Vec::new();
    let secrets = super::api_key_secret(cluster, &mut r.map, &mut errors);
    if !errors.is_empty() {
        return Err(validation(&errors));
    }
    cfg.service_settings = r.map;
    Ok((ss, Map::new(), secrets))
}

fn style(ep: &Endpoint) -> oai::Style<'_> {
    oai::Style { model: ep.service_settings.get("model_id"), max_tokens_key: "max_tokens", stream_false: true, stream_options: true, ..oai::Style::default() }
}

async fn chat(cluster: &Cluster, ep: &Endpoint, body: &Value, timeout: Duration) -> EsResult<http::Answer> {
    let url = match ep.service_settings.get("url").and_then(Value::as_str) {
        Some(u) => u.to_string(),
        None => format!("{}/chat/completions", super::base_url(cluster, "deepseek", "https://api.deepseek.com")),
    };
    let call = Call::json(&ep.id, url.clone(), json_bearer(cluster, ep, false)?, body, timeout, None);
    let answer = http::post(cluster, &call).await?;
    http::check(&ep.id, &url, &answer)?;
    Ok(answer)
}

pub async fn infer(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Results> {
    if ep.task != TaskType::Completion {
        return Err(crate::xpack::inference::unsupported_task(ep.task, "deepseek"));
    }
    let input = one_input(req, "DeepSeek completion")?;
    let answer = chat(cluster, ep, &oai::body(style(ep), oai::user_message(input), &Map::new(), false), req.timeout).await?;
    Ok(Results::Completion(vec![oai::completion_text(&super::answer_json("deepseek", &answer)?, "DeepSeek chat completions")?]))
}

pub async fn stream_completion(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Vec<Value>> {
    let input = one_input(req, "DeepSeek completion")?;
    let answer = chat(cluster, ep, &oai::body(style(ep), oai::user_message(input), &Map::new(), true), req.timeout).await?;
    Ok(oai::completion_events(&answer.body))
}

pub async fn stream_chat(cluster: &Cluster, ep: &Endpoint, body: &Value, timeout: Duration) -> EsResult<Vec<Value>> {
    let (messages, req) = oai::unified(body)?;
    let answer = chat(cluster, ep, &oai::body(style(ep), messages, &req, true), timeout).await?;
    Ok(oai::chat_events(&answer.body))
}
