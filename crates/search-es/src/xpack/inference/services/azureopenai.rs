//! The `azureopenai` service: embeddings and chat completions of a deployment in an Azure OpenAI
//! resource (`https://<resource_name>.openai.azure.com/openai/deployments/<deployment_id>/…?api-version=…`),
//! as Elasticsearch 9.5.5's `AzureOpenAiService` sends them: OpenAI's bodies without `model`, the
//! key in `api-key` or a Microsoft Entra token as `Authorization: Bearer`. The node's
//! `roll.inference.azureopenai.url` replaces `https://<resource_name>.openai.azure.com`.
//!
//! Not supported: OAuth2 client credentials (`client_secret` with `client_id`, `tenant_id`,
//! `scopes`); `api_key` or `entra_id` is required.

use super::{Config, json_headers, learned, oai, one_input, ordered, take_secret};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::xpack::inference::http::{self, Call};
use crate::xpack::inference::results::Results;
use crate::xpack::inference::stream::Streaming;
use crate::xpack::inference::settings::{Reader, validation};
use crate::xpack::inference::{Endpoint, Request, TaskType};
use std::time::Duration;

const ORDER: &[&str] = &["resource_name", "deployment_id", "api_version", "rate_limit", "dimensions", "max_input_tokens", "similarity", "dimensions_set_by_user"];
const SECRETS: &str = "[service_settings] must have exactly one field of [api_key, client_secret, entra_id] set";

fn default_rate(task: TaskType) -> i64 {
    if task == TaskType::TextEmbedding { 1440 } else { 120 }
}

/// A DNS label (what Azure resource names are): the resource name becomes the host.
pub fn host_label(s: &str) -> bool {
    !s.is_empty() && s.len() <= 63 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') && !s.starts_with('-') && !s.ends_with('-')
}

pub fn parse(cluster: &Cluster, task: TaskType, cfg: &mut Config) -> EsResult<(Map, Map, Map)> {
    let mut r = Reader::new(std::mem::take(&mut cfg.service_settings), "service_settings");
    let resource = r.required_str("resource_name");
    let deployment = r.required_str("deployment_id");
    let version = r.required_str("api_version");
    let rate = r.rate_limit(default_rate(task));
    let mut ss = Map::new();
    if task == TaskType::TextEmbedding {
        let dims = r.optional_positive_int("dimensions");
        let max_input = r.optional_positive_int("max_input_tokens");
        let similarity = r.similarity();
        if r.map.shift_remove("dimensions_set_by_user").is_some() {
            r.error("[service_settings] does not allow the setting [dimensions_set_by_user]");
        }
        if let Some(d) = dims {
            ss.insert("dimensions".into(), d.into());
        }
        if let Some(m) = max_input {
            ss.insert("max_input_tokens".into(), m.into());
        }
        if let Some(s) = similarity {
            ss.insert("similarity".into(), s.into());
        }
        ss.insert("dimensions_set_by_user".into(), dims.is_some().into());
    }
    if let Some(res) = &resource
        && !host_label(res)
    {
        r.error(format!("[service_settings] Invalid value [{res}]. [resource_name] must be a host name label (letters, digits and hyphens)"));
    }
    r.check()?;
    let mut errors = Vec::new();
    let api_key = take_secret(cluster, &mut r.map, "api_key", "service_settings", false, &mut errors);
    let entra = take_secret(cluster, &mut r.map, "entra_id", "service_settings", false, &mut errors);
    let client_secret = r.map.shift_remove("client_secret");
    if !errors.is_empty() {
        return Err(validation(&errors));
    }
    let mut given: Vec<&str> = Vec::new();
    if api_key.is_some() {
        given.push("api_key");
    }
    if client_secret.is_some() {
        given.push("client_secret");
    }
    if entra.is_some() {
        given.push("entra_id");
    }
    let mut secrets = Map::new();
    match (given.as_slice(), api_key, entra) {
        (["api_key"], Some(k), _) => {
            secrets.insert("api_key".into(), k.into());
        }
        (["entra_id"], _, Some(e)) => {
            secrets.insert("entra_id".into(), e.into());
        }
        (["client_secret"], _, _) => {
            return Err(validation(&["[service_settings] OAuth2 client credentials ([client_secret]) aren't supported by roll; set [api_key] or [entra_id]".into()]));
        }
        ([], _, _) => return Err(validation(&[SECRETS.into()])),
        _ => return Err(validation(&[format!("{SECRETS}, received: [{}]", given.join(", "))])),
    }
    let mut base = Map::new();
    base.insert("resource_name".into(), resource.into());
    base.insert("deployment_id".into(), deployment.into());
    base.insert("api_version".into(), version.into());
    base.insert("rate_limit".into(), rate);
    base.extend(ss);
    cfg.service_settings = r.map;
    let ts = task_settings(&mut cfg.task_settings)?;
    Ok((ordered(base, ORDER), ts, secrets))
}

/// `AzureOpenAiTaskSettings`: `user`, `headers`.
pub fn task_settings(ts: &mut Map) -> EsResult<Map> {
    super::openai::task_settings(ts)
}

/// An update may change the rate limit and replace the secret with one of the same kind.
pub fn update(cluster: &Cluster, ep: &mut Endpoint, ss: &mut Map) -> EsResult<()> {
    let mut errors = Vec::new();
    let api_key = take_secret(cluster, ss, "api_key", "service_settings", false, &mut errors);
    let entra = take_secret(cluster, ss, "entra_id", "service_settings", false, &mut errors);
    if !errors.is_empty() {
        return Err(validation(&errors));
    }
    let kind = if ep.secrets.contains_key("entra_id") { "entra_id" } else { "api_key" };
    for (name, v) in [("api_key", api_key), ("entra_id", entra)] {
        let Some(v) = v else { continue };
        if name != kind {
            return Err(validation(&[format!("[service_settings] only the field [{kind}] can be updated for this secret, received: [{name}]")]));
        }
        ep.secrets.insert(name.into(), v.into());
    }
    super::update_rate(ep, ss, default_rate(ep.task))
}

pub fn embedding_details(ep: &mut Endpoint, size: usize) {
    learned(ep, size, "dot_product", ORDER);
}

fn url(cluster: &Cluster, ep: &Endpoint, last: &[&str]) -> EsResult<String> {
    let ss = &ep.service_settings;
    let s = |k: &str| ss.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let resource = s("resource_name");
    if !host_label(&resource) {
        return Err(EsError::status(400, format!("Invalid value [{resource}]. [resource_name] must be a host name label")));
    }
    let base = super::base_url(cluster, "azureopenai", &format!("https://{resource}.openai.azure.com"));
    let mut u = reqwest::Url::parse(&base).map_err(|_| EsError::status(400, format!("Invalid host [{base}], please check that the URL is correct.")))?;
    {
        let Ok(mut p) = u.path_segments_mut() else { return Err(EsError::status(400, format!("Invalid host [{base}], please check that the URL is correct."))) };
        p.pop_if_empty().extend(["openai", "deployments", &s("deployment_id")]).extend(last);
    }
    u.query_pairs_mut().append_pair("api-version", &s("api_version"));
    Ok(u.to_string())
}

fn headers(cluster: &Cluster, ep: &Endpoint, ts: &Map) -> EsResult<Vec<(String, String)>> {
    let mut h = json_headers();
    if let Some(Value::Obj(extra)) = ts.get("headers") {
        for (k, v) in extra {
            h.push((k.clone(), v.to_es_string()));
        }
    }
    if let Some(k) = ep.secret(cluster, "api_key")? {
        h.push(("api-key".into(), k));
    } else if let Some(t) = ep.secret(cluster, "entra_id")? {
        h.push(("Authorization".into(), format!("Bearer {t}")));
    }
    Ok(h)
}

/// `AzureOpenAiResponseHandler`: OpenAI's mapping, with the rate limit headers Azure sends.
fn check(ep: &Endpoint, url: &str, answer: &http::Answer) -> EsResult<()> {
    if answer.status == 429 {
        let h = |k: &str| answer.header(k).unwrap_or("unknown").to_string();
        let prefix = format!(
            "Received a rate limit status code. Remaining tokens [{}]. Remaining requests [{}].",
            h("x-ratelimit-remaining-tokens"),
            h("x-ratelimit-remaining-requests")
        );
        return Err(http::status_error(&prefix, &ep.id, answer));
    }
    http::check(&ep.id, url, answer)
}

async fn send(cluster: &Cluster, ep: &Endpoint, url: String, ts: &Map, body: &Value, timeout: Duration, stream: bool) -> EsResult<http::Answer> {
    let call = Call::json(&ep.id, url.clone(), headers(cluster, ep, ts)?, body, timeout, ep.requests_per_minute());
    let answer = if stream { http::post_stream(cluster, &call).await? } else { http::post(cluster, &call).await? };
    check(ep, &url, &answer)?;
    Ok(answer)
}

fn effective(ep: &Endpoint, req: &Request) -> EsResult<Map> {
    let mut over = req.task_settings.clone();
    let parsed = task_settings(&mut over)?;
    Ok(super::merge(&ep.task_settings, &parsed))
}

fn user(ts: &Map) -> Option<&str> {
    ts.get("user").and_then(Value::as_str).filter(|u| !u.is_empty())
}

/// `AzureOpenAiCompletionRequestEntity`: `messages` (`role` first), `n`, `user`, `stream`.
fn completion_body(input: &str, ts: &Map, stream: bool) -> Value {
    let mut b = Map::new();
    b.insert("messages".into(), crate::arr![obj! { "role" => "user", "content" => input }]);
    b.insert("n".into(), 1.into());
    if let Some(u) = user(ts) {
        b.insert("user".into(), u.into());
    }
    if stream {
        b.insert("stream".into(), true.into());
    }
    Value::Obj(b)
}

pub async fn infer(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Results> {
    let ts = effective(ep, req)?;
    match ep.task {
        TaskType::TextEmbedding => {
            let it = req.input_type.as_deref().map(str::to_ascii_lowercase);
            if let Some(it) = it.as_deref().filter(|it| !super::internal(it) && *it != "unspecified") {
                return Err(validation(&[format!("Invalid input_type [{it}]. The input_type option is not supported by this service")]));
            }
            let mut body = Map::new();
            body.insert("input".into(), super::strings(&req.input));
            if let Some(u) = user(&ts) {
                body.insert("user".into(), u.into());
            }
            if let Some(it) = it.filter(|it| super::internal(it)) {
                body.insert("input_type".into(), it.into());
            }
            if ep.service_settings.get("dimensions_set_by_user").and_then(Value::as_bool) == Some(true)
                && let Some(d) = ep.service_settings.get("dimensions")
            {
                body.insert("dimensions".into(), d.clone());
            }
            body.insert("encoding_format".into(), "base64".into());
            let answer = send(cluster, ep, url(cluster, ep, &["embeddings"])?, &ts, &Value::Obj(body), req.timeout, false).await?;
            super::openai::embeddings(&super::answer_json("azureopenai", &answer)?, req.input.len())
        }
        TaskType::Completion => {
            let input = one_input(req, "Azure OpenAI completion")?;
            let answer = send(cluster, ep, url(cluster, ep, &["chat", "completions"])?, &ts, &completion_body(input, &ts, false), req.timeout, false).await?;
            Ok(Results::Completion(vec![oai::completion_text(&super::answer_json("azureopenai", &answer)?, "Azure OpenAI completions")?]))
        }
        other => Err(crate::xpack::inference::unsupported_task(other, "azureopenai")),
    }
}

pub async fn stream_completion(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Streaming> {
    let ts = effective(ep, req)?;
    let input = one_input(req, "Azure OpenAI completion")?;
    let answer = send(cluster, ep, url(cluster, ep, &["chat", "completions"])?, &ts, &completion_body(input, &ts, true), req.timeout, true).await?;
    Streaming::open(answer, oai::completion_decoder("Azure OpenAI completions"))
}

pub async fn stream_chat(cluster: &Cluster, ep: &Endpoint, body: &Value, timeout: Duration) -> EsResult<Streaming> {
    let (messages, req) = oai::unified(body)?;
    let ts = ep.task_settings.clone();
    // Azure picks the model from the deployment: a request's `model` isn't sent.
    let mut req = req;
    req.shift_remove("model");
    let style = oai::Style { max_tokens_key: "max_completion_tokens", stream_options: true, user: user(&ts), ..oai::Style::default() };
    let b = oai::body(style, messages, &req, true);
    let answer = send(cluster, ep, url(cluster, ep, &["chat", "completions"])?, &ts, &b, timeout, true).await?;
    Streaming::open(answer, oai::chat_decoder(&ep.id))
}
