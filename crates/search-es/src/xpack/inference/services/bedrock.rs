//! The `amazonbedrock` service: text embeddings (`InvokeModel`, Amazon Titan's and Cohere's
//! request bodies) and completions (`Converse`, every provider) on Amazon Bedrock Runtime
//! (`https://bedrock-runtime.<region>.amazonaws.com`), as Elasticsearch 9.5.5's
//! `AmazonBedrockService` sends them through the AWS SDK: the same paths, bodies and
//! `amz-sdk-*` headers, signed with SigV4 by `aws-sigv4` (service `bedrock`). The node's
//! `roll.inference.amazonbedrock.url` replaces the host.
//!
//! Like the SDK client Elasticsearch builds, nothing is retried. Not supported: the streamed
//! `chat_completion` task and streamed completions (`ConverseStream` answers in AWS's binary
//! event stream).

use super::{Config, answer_json, learned, missing_field, ordered, take_secret};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::xpack::inference::http::{self, Call};
use crate::xpack::inference::results::Results;
use crate::xpack::inference::settings::{Reader, validation};
use crate::xpack::inference::{Endpoint, Request, TaskType};
use std::time::{Duration, SystemTime};

const ORDER: &[&str] = &["region", "model", "provider", "rate_limit", "dimensions", "max_input_tokens", "similarity"];
const PROVIDERS: &[&str] = &["ai21labs", "amazontitan", "anthropic", "cohere", "meta", "mistral"];

/// A region as it goes into the host name.
fn region_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub fn parse(cluster: &Cluster, task: TaskType, cfg: &mut Config) -> EsResult<(Map, Map, Map)> {
    let mut r = Reader::new(std::mem::take(&mut cfg.service_settings), "service_settings");
    let model = r.required_str("model");
    let region = r.required_str("region");
    let before = r.has_errors();
    let provider = r.optional_enum("provider", PROVIDERS);
    if provider.is_none() && !before && !r.has_errors() {
        r.error("[service_settings] does not contain the required setting [provider]");
    }
    let rate = r.rate_limit(240);
    let mut ss = Map::new();
    if task == TaskType::TextEmbedding {
        if r.map.shift_remove("dimensions").is_some() {
            r.error("[service_settings] does not allow the setting [dimensions]");
        }
        if let Some(m) = r.optional_positive_int("max_input_tokens") {
            ss.insert("max_input_tokens".into(), m.into());
        }
        if let Some(s) = r.similarity() {
            ss.insert("similarity".into(), s.into());
        }
    }
    if let Some(reg) = region.as_deref().filter(|r| !region_ok(r)) {
        r.error(format!("[service_settings] Invalid value [{reg}]. [region] must be an AWS region name"));
    }
    r.check()?;
    let provider = provider.unwrap_or_default();
    let mut errors = Vec::new();
    let access = take_secret(cluster, &mut r.map, "access_key", "service_settings", true, &mut errors);
    let secret = take_secret(cluster, &mut r.map, "secret_key", "service_settings", true, &mut errors);
    if !errors.is_empty() {
        return Err(validation(&errors));
    }
    let allowed = match task {
        TaskType::TextEmbedding => matches!(provider.as_str(), "amazontitan" | "cohere"),
        _ => true,
    };
    if !allowed {
        return Err(EsError::status(400, format!("The [{}] task type for provider [{provider}] is not available", task.name())));
    }
    let ts = task_settings(task, &mut cfg.task_settings)?;
    if ts.contains_key("top_k") && !matches!(provider.as_str(), "anthropic" | "cohere" | "mistral") {
        return Err(EsError::status(400, format!("The [top_k] task parameter is not available for provider [{provider}]")));
    }
    let mut base = Map::new();
    base.insert("region".into(), region.into());
    base.insert("model".into(), model.into());
    base.insert("provider".into(), provider.to_ascii_uppercase().into());
    base.insert("rate_limit".into(), rate);
    base.extend(ss);
    let mut secrets = Map::new();
    secrets.insert("access_key".into(), access.into());
    secrets.insert("secret_key".into(), secret.into());
    cfg.service_settings = r.map;
    Ok((ordered(base, ORDER), ts, secrets))
}

/// A `Double` between 0 and 1 (`extractOptionalDoubleInRange`): JSON integers aren't doubles.
fn unit_double(r: &mut Reader, key: &str) -> Option<f64> {
    let v = r.map.shift_remove(key)?;
    let Value::Num(crate::json::Number::Float(f)) = v else {
        r.error(format!("field [{key}] is not of the expected type. The value [{}] cannot be converted to a [Double]", crate::xpack::inference::settings::java_string(&v)));
        return None;
    };
    let shown = crate::json::write::java_double(f);
    if f < 0.0 {
        r.error(format!("[task_settings] Invalid value [{shown}]. [{key}] must be greater than or equal to [0.0]"));
        return None;
    }
    if f > 1.0 {
        r.error(format!("[task_settings] Invalid value [{shown}]. [{key}] must be less than or equal to [1.0]"));
        return None;
    }
    Some(f)
}

/// Completions' `AmazonBedrockChatCompletionTaskSettings`: `temperature`, `top_p`, `top_k`,
/// `max_new_tokens`. Embeddings take none.
pub fn task_settings(task: TaskType, ts: &mut Map) -> EsResult<Map> {
    let mut out = Map::new();
    if task == TaskType::TextEmbedding {
        return Ok(out);
    }
    let mut r = Reader::new(std::mem::take(ts), "task_settings");
    let temperature = unit_double(&mut r, "temperature");
    let top_p = unit_double(&mut r, "top_p");
    let top_k = unit_double(&mut r, "top_k");
    let max = r.optional_positive_int("max_new_tokens");
    *ts = r.finish()?;
    for (k, v) in [("temperature", temperature), ("top_p", top_p), ("top_k", top_k)] {
        if let Some(v) = v {
            out.insert(k.into(), v.into());
        }
    }
    if let Some(m) = max {
        out.insert("max_new_tokens".into(), m.into());
    }
    Ok(out)
}

/// An update may change the rate limit and both keys together.
pub fn update(cluster: &Cluster, ep: &mut Endpoint, ss: &mut Map) -> EsResult<()> {
    let mut errors = Vec::new();
    let access = take_secret(cluster, ss, "access_key", "service_settings", false, &mut errors);
    let secret = take_secret(cluster, ss, "secret_key", "service_settings", false, &mut errors);
    if !errors.is_empty() {
        return Err(validation(&errors));
    }
    match (access, secret) {
        (Some(a), Some(s)) => {
            ep.secrets.insert("access_key".into(), a.into());
            ep.secrets.insert("secret_key".into(), s.into());
        }
        (None, None) => {}
        (a, _) => {
            let missing = if a.is_none() { "access_key" } else { "secret_key" };
            return Err(validation(&[format!("[service_settings] [access_key] and [secret_key] must be updated together; missing: [{missing}]")]));
        }
    }
    super::update_rate(ep, ss, 240)
}

pub fn embedding_details(ep: &mut Endpoint, size: usize) {
    let sim = if provider(ep) == "cohere" { "dot_product" } else { "cosine" };
    learned(ep, size, sim, ORDER);
}

fn provider(ep: &Endpoint) -> String {
    ep.service_settings.get("provider").and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase()
}

/// A path segment encoded as the SDK encodes it (everything but unreserved characters).
fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn uuid() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The `InvokeModel`/`Converse` failure as Elasticsearch reports the SDK's exception.
fn failure(answer: &http::Answer, what: &str) -> (String, EsError) {
    let v = answer.json();
    let msg = v.as_ref().and_then(|v| v.get("message").or_else(|| v.get("Message"))).and_then(Value::as_str).unwrap_or_default().to_string();
    let rid = answer.header("x-amzn-requestid").unwrap_or("null");
    let detail = format!("{msg} (Service: BedrockRuntime, Status Code: {}, Request ID: {rid}) (SDK Attempt Count: 1)", answer.status);
    let kind = answer.header("x-amzn-errortype").and_then(|t| t.split(':').next()).filter(|t| !t.is_empty()).map(snake).unwrap_or_else(|| "bedrock_runtime_exception".into());
    let reason = format!("AmazonBedrock {what} failure: [{detail}]");
    (reason, EsError::java(400, kind, detail))
}

/// `ValidationException` → `validation_exception`.
fn snake(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else if c.is_ascii_alphanumeric() {
            out.push(c);
        }
    }
    out
}

async fn send(cluster: &Cluster, ep: &Endpoint, action: &str, body: &Value, timeout: Duration) -> EsResult<Result<Value, http::Answer>> {
    let region = ep.service_settings.get("region").and_then(Value::as_str).unwrap_or_default().to_string();
    if !region_ok(&region) {
        return Err(EsError::status(400, format!("Invalid value [{region}]. [region] must be an AWS region name")));
    }
    let model = ep.service_settings.get("model").and_then(Value::as_str).unwrap_or_default();
    let base = super::base_url(cluster, "amazonbedrock", &format!("https://bedrock-runtime.{region}.amazonaws.com"));
    let url = format!("{base}/model/{}/{action}", encode_segment(model));
    let parsed = reqwest::Url::parse(&url).map_err(|_| EsError::status(400, format!("Invalid host [{base}], please check that the URL is correct.")))?;
    let host = match parsed.port() {
        Some(p) => format!("{}:{p}", parsed.host_str().unwrap_or_default()),
        None => parsed.host_str().unwrap_or_default().to_string(),
    };
    let bytes = body.to_json().into_bytes();
    let length = bytes.len().to_string();
    let invocation = uuid();
    let unsigned: Vec<(&str, &str)> = vec![
        ("amz-sdk-invocation-id", &invocation),
        ("amz-sdk-request", "attempt=1; max=1"),
        ("content-length", &length),
        ("content-type", "application/json"),
        ("host", &host),
    ];
    let access = ep.secret(cluster, "access_key")?.unwrap_or_default();
    let secret = ep.secret(cluster, "secret_key")?.unwrap_or_default();
    let identity: aws_smithy_runtime_api::client::identity::Identity = aws_credential_types::Credentials::new(access, secret, None, None, "roll-inference").into();
    let mut settings = aws_sigv4::http_request::SigningSettings::default();
    settings.payload_checksum_kind = aws_sigv4::http_request::PayloadChecksumKind::XAmzSha256;
    let signing_failed = |e: &dyn std::fmt::Display| EsError::illegal_state(format!("failed to sign the Amazon Bedrock request: {e}"));
    let params: aws_sigv4::http_request::SigningParams = aws_sigv4::sign::v4::SigningParams::builder()
        .identity(&identity)
        .region(&region)
        .name("bedrock")
        .time(SystemTime::now())
        .settings(settings)
        .build()
        .map_err(|e| signing_failed(&e))?
        .into();
    let signable = aws_sigv4::http_request::SignableRequest::new("POST", url.as_str(), unsigned.iter().copied(), aws_sigv4::http_request::SignableBody::Bytes(&bytes))
        .map_err(|e| signing_failed(&e))?;
    let (instructions, _) = aws_sigv4::http_request::sign(signable, &params).map_err(|e| signing_failed(&e))?.into_parts();
    let mut headers: Vec<(String, String)> = unsigned.iter().filter(|(k, _)| !matches!(*k, "host" | "content-length")).map(|(k, v)| (k.to_string(), v.to_string())).collect();
    for (k, v) in instructions.headers() {
        headers.push((k.to_string(), v.to_string()));
    }
    let call = Call { inference_id: &ep.id, url, headers, body: bytes, timeout, retry: |_| false, per_minute: ep.requests_per_minute() };
    let answer = http::post(cluster, &call).await?;
    if !answer.ok() {
        return Ok(Err(answer));
    }
    answer_json("amazonbedrock", &answer).map(Ok)
}

fn effective(ep: &Endpoint, req: &Request) -> EsResult<Map> {
    let mut over = req.task_settings.clone();
    let parsed = task_settings(ep.task, &mut over)?;
    Ok(super::merge(&ep.task_settings, &parsed))
}

/// Cohere's `input_type` (`search_document` unless one is given).
fn cohere_input_type(it: Option<&str>) -> &'static str {
    match it.map(str::to_ascii_lowercase).as_deref() {
        Some("search" | "internal_search") => "search_query",
        Some("classification") => "classification",
        Some("clustering") => "clustering",
        _ => "search_document",
    }
}

/// The `Converse` request: every input a user message; `inferenceConfig` from the task settings;
/// `top_k` as an additional model response field path, as Elasticsearch writes it.
fn converse_body(inputs: &[String], ts: &Map) -> Value {
    let mut b = Map::new();
    b.insert("messages".into(), Value::Arr(inputs.iter().map(|i| obj! { "role" => "user", "content" => crate::arr![obj! { "text" => i.clone() }] }).collect()));
    let mut cfg = Map::new();
    for (from, to) in [("max_new_tokens", "maxTokens"), ("temperature", "temperature"), ("top_p", "topP")] {
        if let Some(v) = ts.get(from) {
            cfg.insert(to.into(), v.clone());
        }
    }
    if !cfg.is_empty() {
        b.insert("inferenceConfig".into(), Value::Obj(cfg));
    }
    if let Some(k) = ts.get("top_k").and_then(Value::as_f64) {
        b.insert("additionalModelResponseFieldPaths".into(), crate::arr![format!("{{\"top_k\":{:.6}}}", k as f32)]);
    }
    Value::Obj(b)
}

pub async fn infer(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Results> {
    match ep.task {
        TaskType::TextEmbedding => {
            let fail = |a: http::Answer| {
                let (reason, cause) = failure(&a, "invoke model");
                EsError::new(500, "exception", reason).caused_by(cause)
            };
            if provider(ep) == "cohere" {
                let body = obj! { "texts" => super::strings(&req.input), "input_type" => cohere_input_type(req.input_type.as_deref()) };
                let v = send(cluster, ep, "invoke", &body, req.timeout).await?.map_err(fail)?;
                let list = v.get("embeddings").and_then(Value::as_array).ok_or_else(|| missing_field("embeddings", "Amazon Bedrock Cohere embeddings"))?;
                return super::float_vectors(list, req.input.len(), "Amazon Bedrock Cohere embeddings");
            }
            // Titan embeds one text a request.
            let mut out = Vec::with_capacity(req.input.len());
            for text in &req.input {
                let v = send(cluster, ep, "invoke", &obj! { "inputText" => text.clone() }, req.timeout).await?.map_err(fail)?;
                let e = v.get("embedding").cloned().ok_or_else(|| missing_field("embedding", "Amazon Bedrock Titan embeddings"))?;
                out.push(e);
            }
            super::float_vectors(&out, req.input.len(), "Amazon Bedrock Titan embeddings")
        }
        TaskType::Completion => {
            let ts = effective(ep, req)?;
            let answer = send(cluster, ep, "converse", &converse_body(&req.input, &ts), req.timeout).await?.map_err(|a| {
                let (reason, cause) = failure(&a, "converse");
                EsError::new(500, "exception", format!("Failed to send request from inference entity id [{}]", ep.id))
                    .caused_by(EsError::new(500, "exception", format!("org.elasticsearch.ElasticsearchException: {reason}")).caused_by(EsError::new(500, "exception", reason).caused_by(cause)))
            })?;
            let content = answer.pointer(&["output", "message", "content"]).and_then(Value::as_array).ok_or_else(|| missing_field("output", "Amazon Bedrock converse"))?;
            Ok(Results::Completion(vec![content.iter().filter_map(|c| c.get("text").and_then(Value::as_str)).collect()]))
        }
        other => Err(crate::xpack::inference::unsupported_task(other, "amazonbedrock")),
    }
}
