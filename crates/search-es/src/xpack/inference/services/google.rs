//! Google's services, as Elasticsearch 9.5.5 sends them:
//!
//! - `googlevertexai`: text embeddings (`…/publishers/google/models/<model>:predict`), rerank
//!   (Discovery Engine's `rankingConfigs/default_ranking_config:rank`), Gemini completions and
//!   streamed chat completions (`:generateContent`, `:streamGenerateContent?alt=sse`). It
//!   authenticates with a service account: roll signs an RS256 JWT with its key
//!   (`jsonwebtoken`), trades it at Google's token endpoint (always
//!   `https://oauth2.googleapis.com/token`, never the JSON's own `token_uri`) and keeps the
//!   access token per endpoint until five minutes before it expires.
//! - `googleaistudio`: text embeddings (`:batchEmbedContents`) and Gemini completions, the API
//!   key in the `key` query parameter as Elasticsearch sends it (errors print URLs without
//!   their query).
//!
//! The node's `roll.inference.googlevertexai.url`, `roll.inference.googlevertexai.rank.url`,
//! `roll.inference.google.oauth2.url` and `roll.inference.googleaistudio.url` replace the hosts.
//! Not supported: Vertex AI's Model Garden providers (`provider` other than `google`, `url`,
//! `streaming_url`).

use super::{Config, answer_json, learned, missing_field, ordered, path_segment, take_secret};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::xpack::inference::http::{self, Call};
use crate::xpack::inference::results::{Ranked, Results};
use crate::xpack::inference::stream::{Decode, Streaming};
use crate::xpack::inference::settings::{Reader, validation};
use crate::xpack::inference::{Endpoint, Request, TaskType};
use sha2::Digest;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VERTEX_EMBEDDING_ORDER: &[&str] = &["location", "project_id", "model_id", "dimensions", "similarity", "max_input_tokens", "rate_limit", "dimensions_set_by_user"];
const VERTEX_RERANK_ORDER: &[&str] = &["project_id", "model_id", "rate_limit"];
const VERTEX_CHAT_ORDER: &[&str] = &["project_id", "location", "model_id", "provider", "rate_limit"];
const STUDIO_ORDER: &[&str] = &["model_id", "max_input_tokens", "dimensions", "similarity", "rate_limit"];
const VERTEX: &str = "Google Vertex AI";
const STUDIO: &str = "Google AI Studio";
/// What an OAuth token answer may hold.
const TOKEN_ANSWER_MAX: usize = 64 * 1024;

fn location_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 40 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn put<T: Into<Value>>(m: &mut Map, k: &str, v: Option<T>) {
    if let Some(v) = v {
        m.insert(k.into(), v.into());
    }
}

pub fn parse_vertex(cluster: &Cluster, task: TaskType, cfg: &mut Config) -> EsResult<(Map, Map, Map)> {
    let mut r = Reader::new(std::mem::take(&mut cfg.service_settings), "service_settings");
    let mut ss = Map::new();
    match task {
        TaskType::TextEmbedding => {
            let location = r.optional_str("location");
            let project = r.required_str("project_id");
            let model = r.required_str("model_id");
            let dims = r.optional_positive_int("dimensions");
            let similarity = r.similarity();
            let max_input = r.optional_positive_int("max_input_tokens");
            let rate = r.rate_limit(30_000);
            if r.map.shift_remove("dimensions_set_by_user").is_some() {
                r.error("[service_settings] does not allow the setting [dimensions_set_by_user]");
            }
            check_location(&mut r, location.as_deref());
            put(&mut ss, "location", location);
            put(&mut ss, "project_id", project);
            put(&mut ss, "model_id", model);
            put(&mut ss, "dimensions", dims);
            put(&mut ss, "similarity", similarity);
            put(&mut ss, "max_input_tokens", max_input);
            ss.insert("rate_limit".into(), rate);
            ss.insert("dimensions_set_by_user".into(), dims.is_some().into());
            ss = ordered(ss, VERTEX_EMBEDDING_ORDER);
        }
        TaskType::Rerank => {
            let project = r.required_str("project_id");
            let model = r.optional_str("model_id");
            let rate = r.rate_limit(300);
            put(&mut ss, "project_id", project);
            put(&mut ss, "model_id", model);
            ss.insert("rate_limit".into(), rate);
            ss = ordered(ss, VERTEX_RERANK_ORDER);
        }
        _ => {
            let project = r.required_str("project_id");
            let location = r.optional_str("location");
            let model = r.required_str("model_id");
            let provider = r.optional_str("provider");
            let rate = r.rate_limit(1000);
            if let Some(p) = provider.filter(|p| !p.eq_ignore_ascii_case("google")) {
                r.error(format!("[service_settings] Google Model Garden providers ([provider] [{p}]) aren't supported by roll"));
            }
            for k in ["url", "streaming_url"] {
                if r.map.shift_remove(k).is_some() {
                    r.error(format!("[service_settings] Google Model Garden URLs ([{k}]) aren't supported by roll"));
                }
            }
            check_location(&mut r, location.as_deref());
            put(&mut ss, "project_id", project);
            put(&mut ss, "location", location);
            put(&mut ss, "model_id", model);
            ss.insert("provider".into(), "GOOGLE".into());
            ss.insert("rate_limit".into(), rate);
            ss = ordered(ss, VERTEX_CHAT_ORDER);
        }
    }
    r.check()?;
    let mut errors = Vec::new();
    let sa = take_secret(cluster, &mut r.map, "service_account_json", "service_settings", true, &mut errors);
    if !errors.is_empty() {
        return Err(validation(&errors));
    }
    let mut secrets = Map::new();
    secrets.insert("service_account_json".into(), sa.into());
    cfg.service_settings = r.map;
    let ts = vertex_task_settings(task, &mut cfg.task_settings)?;
    Ok((ss, ts, secrets))
}

fn check_location(r: &mut Reader, location: Option<&str>) {
    if let Some(l) = location.filter(|l| !location_ok(l)) {
        r.error(format!("[service_settings] Invalid value [{l}]. [location] must be a Google Cloud location"));
    }
}

/// Embeddings: `input_type`, `auto_truncate`. Rerank: `top_n`. Completions: `max_tokens`,
/// `thinking_config.thinking_budget` (printed with `max_tokens`, `null` when unset, as
/// Elasticsearch prints them).
pub fn vertex_task_settings(task: TaskType, ts: &mut Map) -> EsResult<Map> {
    let mut r = Reader::new(std::mem::take(ts), "task_settings");
    let mut out = Map::new();
    match task {
        TaskType::TextEmbedding => {
            let it = r.optional_enum("input_type", &["classification", "clustering", "ingest", "internal_ingest", "internal_search", "search"]);
            let truncate = r.optional_bool("auto_truncate");
            put(&mut out, "input_type", it);
            put(&mut out, "auto_truncate", truncate);
        }
        TaskType::Rerank => put(&mut out, "top_n", r.optional_positive_int("top_n")),
        _ => {
            let thinking = r.optional_map("thinking_config").map(|m| {
                let mut t = Reader::new(m, "thinking_config");
                let budget = t.optional_int("thinking_budget");
                (budget, t.finish())
            });
            let max = r.optional_positive_int("max_tokens");
            match thinking {
                Some((budget, rest)) => {
                    rest?;
                    let mut t = Map::new();
                    put(&mut t, "thinking_budget", budget);
                    out.insert("thinking_config".into(), Value::Obj(t));
                    out.insert("max_tokens".into(), max.map_or(Value::Null, Value::from));
                }
                None => put(&mut out, "max_tokens", max),
            }
        }
    }
    *ts = r.finish()?;
    Ok(out)
}

pub fn update_vertex(cluster: &Cluster, ep: &mut Endpoint, ss: &mut Map) -> EsResult<()> {
    let mut errors = Vec::new();
    if let Some(sa) = take_secret(cluster, ss, "service_account_json", "service_settings", false, &mut errors) {
        ep.secrets.insert("service_account_json".into(), sa.into());
    }
    if !errors.is_empty() {
        return Err(validation(&errors));
    }
    let default = match ep.task {
        TaskType::TextEmbedding => 30_000,
        TaskType::Rerank => 300,
        _ => 1000,
    };
    super::update_rate(ep, ss, default)
}

pub fn vertex_embedding_details(ep: &mut Endpoint, size: usize) {
    learned(ep, size, "dot_product", VERTEX_EMBEDDING_ORDER);
}

pub fn parse_studio(cluster: &Cluster, task: TaskType, cfg: &mut Config) -> EsResult<(Map, Map, Map)> {
    let mut r = Reader::new(std::mem::take(&mut cfg.service_settings), "service_settings");
    let model = r.required_str("model_id");
    let mut ss = Map::new();
    ss.insert("model_id".into(), model.into());
    if task == TaskType::TextEmbedding {
        put(&mut ss, "max_input_tokens", r.optional_positive_int("max_input_tokens"));
        put(&mut ss, "dimensions", r.optional_positive_int("dimensions"));
        put(&mut ss, "similarity", r.similarity());
    }
    ss.insert("rate_limit".into(), r.rate_limit(360));
    r.check()?;
    let mut errors = Vec::new();
    let key = take_secret(cluster, &mut r.map, "api_key", "service_settings", true, &mut errors);
    if !errors.is_empty() {
        return Err(validation(&errors));
    }
    let mut secrets = Map::new();
    secrets.insert("api_key".into(), key.into());
    cfg.service_settings = r.map;
    Ok((ordered(ss, STUDIO_ORDER), Map::new(), secrets))
}

pub fn studio_embedding_details(ep: &mut Endpoint, size: usize) {
    // AI Studio's endpoints take the size the model answers with, whatever was asked.
    ep.service_settings.insert("dimensions".into(), (size as i64).into());
    learned(ep, size, "dot_product", STUDIO_ORDER);
}

/// The task type Google's embedding APIs take for an input type.
fn google_task_type(it: Option<&str>) -> Option<&'static str> {
    Some(match it? {
        "ingest" | "internal_ingest" => "RETRIEVAL_DOCUMENT",
        "search" | "internal_search" => "RETRIEVAL_QUERY",
        "classification" => "CLASSIFICATION",
        "clustering" => "CLUSTERING",
        _ => return None,
    })
}

// ---- Vertex AI's OAuth tokens ----

struct Token {
    value: String,
    expires: Instant,
}

static TOKENS: LazyLock<parking_lot::Mutex<HashMap<String, Token>>> = LazyLock::new(Default::default);

/// Tokens are used until this long before they expire.
const REFRESH_MARGIN: Duration = Duration::from_secs(300);

fn credentials_error(msg: impl Into<String>) -> EsError {
    let msg = msg.into();
    EsError::status(400, msg.clone()).caused_by(EsError::java(500, "i_o_exception", msg))
}

/// The access token of an endpoint's service account, from the cache or a new one.
async fn access_token(cluster: &Cluster, ep: &Endpoint, timeout: Duration) -> EsResult<String> {
    let sa = ep.secret(cluster, "service_account_json")?.unwrap_or_default();
    let key = format!("{}\u{0}{}\u{0}{}", cluster.config.name, ep.id, hex(&sha2::Sha256::digest(sa.as_bytes())));
    let now = Instant::now();
    if let Some(t) = TOKENS.lock().get(&key).filter(|t| t.expires > now + REFRESH_MARGIN) {
        return Ok(t.value.clone());
    }
    let creds = crate::json::parse(sa.as_bytes()).ok().flatten().and_then(|v| v.as_object().cloned());
    let Some(creds) = creds else { return Err(credentials_error("Error reading credentials from stream: the service account JSON isn't a JSON object")) };
    let field = |k: &str| creds.get(k).and_then(Value::as_str).map(str::to_owned);
    match field("type").as_deref() {
        None => return Err(credentials_error("Error reading credentials from stream, 'type' field not specified.")),
        Some("service_account") => {}
        Some(other) => return Err(credentials_error(format!("Error reading credentials from stream, 'type' value '{other}' not recognized. Expecting 'service_account'."))),
    }
    let (Some(email), Some(pem)) = (field("client_email"), field("private_key")) else {
        return Err(credentials_error("Error reading service account credential from stream, expecting 'client_id', 'client_email', 'private_key' and 'private_key_id'."));
    };
    let token_url = format!("{}/token", super::base_url(cluster, "google.oauth2", "https://oauth2.googleapis.com"));
    let iat = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let claims = serde_json::json!({
        "aud": "https://oauth2.googleapis.com/token",
        "exp": iat + 3600,
        "iat": iat,
        "iss": email,
        "scope": "https://www.googleapis.com/auth/cloud-platform",
    });
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = field("private_key_id");
    let signing = jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes()).map_err(|_| credentials_error("Invalid PKCS#8 data."))?;
    let jwt = jsonwebtoken::encode(&header, &claims, &signing).map_err(|_| credentials_error("Failed to sign the service account's token request"))?;
    let body = format!("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={jwt}");
    let headers = vec![("Content-Type".to_string(), "application/x-www-form-urlencoded; charset=UTF-8".to_string()), ("Accept".to_string(), "*/*".to_string())];
    let call = Call { inference_id: &ep.id, url: token_url, headers, body: body.into_bytes(), timeout, retry: http::retry_server_errors, per_minute: None };
    let answer = http::post_limited(cluster, &call, TOKEN_ANSWER_MAX).await?;
    let v = answer.json();
    let token = v.as_ref().and_then(|v| v.get("access_token")).and_then(Value::as_str).filter(|_| answer.ok());
    let Some(token) = token else {
        let detail = v.as_ref().and_then(|v| v.get("error_description").or_else(|| v.get("error"))).and_then(Value::as_str).unwrap_or_default();
        return Err(credentials_error(format!("Error getting access token for service account: {} {detail}", answer.status).trim_end().to_string()));
    };
    let lifetime = v.as_ref().and_then(|v| v.get("expires_in")).and_then(Value::as_i64).unwrap_or(3600).clamp(0, 86_400) as u64;
    let mut cache = TOKENS.lock();
    if cache.len() > 10_000 {
        cache.retain(|_, t| t.expires > now);
    }
    cache.insert(key, Token { value: token.to_string(), expires: now + Duration::from_secs(lifetime) });
    Ok(token.to_string())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn ss_str<'a>(ep: &'a Endpoint, k: &str) -> Option<&'a str> {
    ep.service_settings.get(k).and_then(Value::as_str)
}

/// `https://<location>-aiplatform.googleapis.com` (or the global host).
fn vertex_host(cluster: &Cluster, ep: &Endpoint) -> EsResult<String> {
    let host = match ss_str(ep, "location") {
        None | Some("global") => "https://aiplatform.googleapis.com".to_string(),
        Some(l) if location_ok(l) => format!("https://{l}-aiplatform.googleapis.com"),
        Some(l) => return Err(EsError::status(400, format!("Invalid value [{l}]. [location] must be a Google Cloud location"))),
    };
    Ok(super::base_url(cluster, "googlevertexai", &host))
}

async fn vertex_send(cluster: &Cluster, ep: &Endpoint, url: String, body: &Value, timeout: Duration, stream: bool) -> EsResult<http::Answer> {
    let token = access_token(cluster, ep, timeout).await?;
    let headers = vec![("Content-Type".to_string(), "application/json;charset=utf-8".to_string()), ("Authorization".to_string(), format!("Bearer {token}"))];
    let call = Call::json(&ep.id, url.clone(), headers, body, timeout, ep.requests_per_minute());
    let answer = if stream { http::post_stream(cluster, &call).await? } else { http::post(cluster, &call).await? };
    http::check_google(&ep.id, &url, &answer, VERTEX)?;
    Ok(answer)
}

fn effective_vertex(ep: &Endpoint, req: &Request) -> EsResult<Map> {
    let mut over = req.task_settings.clone();
    let parsed = vertex_task_settings(ep.task, &mut over)?;
    Ok(super::merge(&ep.task_settings, &parsed))
}

fn gemini_url(cluster: &Cluster, ep: &Endpoint, stream: bool) -> EsResult<String> {
    let project = path_segment(ss_str(ep, "project_id").unwrap_or_default());
    let model = path_segment(ss_str(ep, "model_id").unwrap_or_default());
    let action = if stream { "streamGenerateContent?alt=sse" } else { "generateContent" };
    Ok(format!("{}/v1/projects/{project}/locations/global/publishers/google/models/{model}:{action}", vertex_host(cluster, ep)?))
}

pub async fn infer_vertex(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Results> {
    let ts = effective_vertex(ep, req)?;
    let project = path_segment(ss_str(ep, "project_id").unwrap_or_default());
    match ep.task {
        TaskType::TextEmbedding => {
            let it = req.input_type.as_deref().map(str::to_ascii_lowercase).or_else(|| ts.get("input_type").and_then(Value::as_str).map(str::to_owned));
            let task = google_task_type(it.as_deref());
            let instances: Vec<Value> = req
                .input
                .iter()
                .map(|t| {
                    let mut i = Map::new();
                    i.insert("content".into(), t.clone().into());
                    if let Some(tt) = task {
                        i.insert("task_type".into(), tt.into());
                    }
                    Value::Obj(i)
                })
                .collect();
            let mut params = Map::new();
            if let Some(a) = ts.get("auto_truncate") {
                params.insert("autoTruncate".into(), a.clone());
            }
            if ep.service_settings.get("dimensions_set_by_user").and_then(Value::as_bool) == Some(true)
                && let Some(d) = ep.service_settings.get("dimensions")
            {
                params.insert("outputDimensionality".into(), d.clone());
            }
            let location = path_segment(ss_str(ep, "location").unwrap_or("global"));
            let model = path_segment(ss_str(ep, "model_id").unwrap_or_default());
            let url = format!("{}/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:predict", vertex_host(cluster, ep)?);
            let body = obj! { "instances" => Value::Arr(instances), "parameters" => Value::Obj(params) };
            let v = answer_json("googlevertexai", &vertex_send(cluster, ep, url, &body, req.timeout, false).await?)?;
            let preds = v.get("predictions").and_then(Value::as_array).ok_or_else(|| missing_field("predictions", "Google Vertex AI embeddings"))?;
            let vectors: Vec<Value> = preds.iter().map(|p| p.pointer(&["embeddings", "values"]).cloned().unwrap_or(Value::Null)).collect();
            super::float_vectors(&vectors, req.input.len(), "Google Vertex AI embeddings")
        }
        TaskType::Rerank => {
            let mut body = Map::new();
            if let Some(m) = ep.service_settings.get("model_id") {
                body.insert("model".into(), m.clone());
            }
            body.insert("query".into(), req.query.clone().unwrap_or_default().into());
            body.insert("records".into(), Value::Arr(req.input.iter().enumerate().map(|(i, t)| obj! { "id" => i.to_string(), "content" => t.clone() }).collect()));
            if let Some(n) = req.top_n.map(Value::from).or_else(|| ts.get("top_n").cloned()) {
                body.insert("topN".into(), n);
            }
            if req.return_documents == Some(false) {
                body.insert("ignoreRecordDetailsInResponse".into(), true.into());
            }
            let base = super::base_url(cluster, "googlevertexai.rank", "https://discoveryengine.googleapis.com");
            let url = format!("{base}/v1/projects/{project}/locations/global/rankingConfigs/default_ranking_config:rank");
            let v = answer_json("googlevertexai", &vertex_send(cluster, ep, url, &Value::Obj(body), req.timeout, false).await?)?;
            let what = "Google Vertex AI rerank";
            let records = v.get("records").and_then(Value::as_array).ok_or_else(|| missing_field("records", what))?;
            let mut out = Vec::with_capacity(records.len());
            for r in records {
                let id = r.get("id").and_then(Value::as_str).ok_or_else(|| missing_field("id", what))?;
                let index = id.parse::<usize>().ok().filter(|i| *i < req.input.len()).ok_or_else(|| {
                    EsError::java(500, "illegal_state_exception", format!("Expected numeric value for record ID field in Google Vertex AI rerank response but received [{id}]"))
                })?;
                let score = r.get("score").and_then(Value::as_f64).ok_or_else(|| missing_field("score", what))?;
                let text = r.get("content").and_then(Value::as_str).map(str::to_owned);
                out.push(Ranked { index, score: score as f32, text });
            }
            Ok(Results::Rerank(out))
        }
        TaskType::Completion => {
            let input = super::one_input(req, "Google VertexAI chat completion")?;
            let body = gemini_body(&[obj! { "role" => "user", "parts" => crate::arr![obj! { "text" => input }] }], None, &ts, &Map::new());
            let v = answer_json("googlevertexai", &vertex_send(cluster, ep, gemini_url(cluster, ep, false)?, &body, req.timeout, false).await?)?;
            Ok(Results::Completion(vec![candidate_text(&v).ok_or_else(|| missing_field("candidates", "Google Vertex AI completion"))?]))
        }
        other => Err(crate::xpack::inference::unsupported_task(other, "googlevertexai")),
    }
}

/// A Gemini request: `contents`, `generationConfig` (the unified request's options, then the
/// task settings' `max_tokens` and thinking budget), `systemInstruction`.
fn gemini_body(contents: &[Value], system: Option<Value>, ts: &Map, unified: &Map) -> Value {
    let mut b = Map::new();
    b.insert("contents".into(), Value::Arr(contents.to_vec()));
    let mut g = Map::new();
    if let Some(s) = unified.get("stop").filter(|s| !matches!(s, Value::Arr(a) if a.is_empty())) {
        g.insert("stopSequences".into(), s.clone());
    }
    if let Some(t) = unified.get("temperature") {
        g.insert("temperature".into(), t.clone());
    }
    if let Some(m) = unified.get("max_completion_tokens").or_else(|| ts.get("max_tokens").filter(|v| !v.is_null())) {
        g.insert("maxOutputTokens".into(), m.clone());
    }
    if let Some(p) = unified.get("top_p") {
        g.insert("topP".into(), p.clone());
    }
    if let Some(b) = ts.get("thinking_config").and_then(|t| t.get("thinking_budget")) {
        g.insert("thinkingConfig".into(), obj! { "thinkingBudget" => b.clone() });
    }
    if !g.is_empty() {
        b.insert("generationConfig".into(), Value::Obj(g));
    }
    if let Some(s) = system {
        b.insert("systemInstruction".into(), s);
    }
    Value::Obj(b)
}

/// The text of a Gemini answer's first candidate.
fn candidate_text(v: &Value) -> Option<String> {
    let parts = v.get("candidates")?.as_array()?.first()?.pointer(&["content", "parts"])?.as_array()?;
    Some(parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect())
}

/// A streamed Gemini completion's text, one `completion` event per read.
fn gemini_completion_decoder() -> Decode {
    Box::new(|events: &[(Option<String>, String)]| -> EsResult<Vec<Value>> {
        let deltas: Vec<Value> = events.iter().filter_map(|(_, d)| crate::json::parse(d.as_bytes()).ok().flatten()).filter_map(|c| candidate_text(&c)).filter(|t| !t.is_empty()).map(|t| obj! { "delta" => t }).collect();
        Ok(if deltas.is_empty() { vec![] } else { vec![obj! { "completion" => Value::Arr(deltas) }] })
    })
}

pub async fn stream_completion_vertex(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Streaming> {
    let ts = effective_vertex(ep, req)?;
    let input = super::one_input(req, "Google VertexAI chat completion")?;
    let body = gemini_body(&[obj! { "role" => "user", "parts" => crate::arr![obj! { "text" => input }] }], None, &ts, &Map::new());
    let answer = vertex_send(cluster, ep, gemini_url(cluster, ep, true)?, &body, req.timeout, true).await?;
    Streaming::open(answer, gemini_completion_decoder())
}

/// A message's text: its string content or its text parts.
fn text_of(m: &Value) -> String {
    match m.get("content") {
        Some(Value::Str(s)) => s.clone(),
        Some(Value::Arr(parts)) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect(),
        _ => String::new(),
    }
}

pub async fn stream_chat_vertex(cluster: &Cluster, ep: &Endpoint, body: &Value, timeout: Duration) -> EsResult<Streaming> {
    let req = super::openai::unified_request(body)?;
    let mut contents = Vec::new();
    let mut system = Vec::new();
    for m in req.get("messages").and_then(Value::as_array).into_iter().flatten() {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        let text = text_of(m);
        match role {
            "system" | "developer" => system.push(obj! { "text" => text }),
            "assistant" => contents.push(obj! { "role" => "model", "parts" => crate::arr![obj! { "text" => text }] }),
            _ => contents.push(obj! { "role" => "user", "parts" => crate::arr![obj! { "text" => text }] }),
        }
    }
    let system = (!system.is_empty()).then(|| obj! { "parts" => Value::Arr(system) });
    let b = gemini_body(&contents, system, &ep.task_settings, &req);
    let answer = vertex_send(cluster, ep, gemini_url(cluster, ep, true)?, &b, timeout, true).await?;
    Streaming::open(answer, Box::new(gemini_chat_events))
}

/// A streamed Gemini chat's chunks in the unified API's shape.
fn gemini_chat_events(events: &[(Option<String>, String)]) -> EsResult<Vec<Value>> {
    let mut out = Vec::new();
    for (_, d) in events {
        let Some(c) = crate::json::parse(d.as_bytes()).ok().flatten() else { continue };
        let Some(id) = c.get("responseId").cloned() else {
            return Err(EsError::status(400, "Failed to send Google VertexAI chat completion request. Cause: Required [responseId]"));
        };
        let first = c.get("candidates").and_then(Value::as_array).and_then(|a| a.first()).cloned().unwrap_or(Value::Null);
        let mut delta = Map::new();
        if let Some(t) = candidate_text(&c) {
            delta.insert("content".into(), t.into());
        }
        if let Some(r) = first.pointer(&["content", "role"]) {
            delta.insert("role".into(), r.clone());
        }
        let mut choice = Map::new();
        choice.insert("delta".into(), Value::Obj(delta));
        if let Some(f) = first.get("finishReason") {
            choice.insert("finish_reason".into(), f.clone());
        }
        choice.insert("index".into(), first.get("index").cloned().unwrap_or(Value::from(0)));
        let mut chunk = Map::new();
        chunk.insert("id".into(), id);
        chunk.insert("choices".into(), crate::arr![Value::Obj(choice)]);
        if let Some(m) = c.get("modelVersion") {
            chunk.insert("model".into(), m.clone());
        }
        chunk.insert("object".into(), "chat.completion.chunk".into());
        if let Some(u) = c.get("usageMetadata") {
            let n = |k: &str| u.get(k).cloned().unwrap_or(Value::from(0));
            chunk.insert("usage".into(), obj! { "completion_tokens" => n("candidatesTokenCount"), "prompt_tokens" => n("promptTokenCount"), "total_tokens" => n("totalTokenCount") });
        }
        out.push(Value::Obj(chunk));
    }
    Ok(out)
}

// ---- Google AI Studio ----

fn studio_url(cluster: &Cluster, ep: &Endpoint, action: &str, key: &str, sse: bool) -> String {
    let model = path_segment(ss_str(ep, "model_id").unwrap_or_default());
    let base = super::base_url(cluster, "googleaistudio", "https://generativelanguage.googleapis.com");
    let mut q: String = url_query_value(key);
    if sse {
        q.push_str("&alt=sse");
    }
    format!("{base}/v1/models/{model}:{action}?key={q}")
}

fn url_query_value(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

async fn studio_send(cluster: &Cluster, ep: &Endpoint, action: &str, stream: bool, body: &Value, timeout: Duration) -> EsResult<http::Answer> {
    let key = ep.secret(cluster, "api_key")?.unwrap_or_default();
    let url = studio_url(cluster, ep, action, &key, stream);
    let call = Call::json(&ep.id, url.clone(), super::json_headers(), body, timeout, ep.requests_per_minute());
    let answer = if stream { http::post_stream(cluster, &call).await? } else { http::post(cluster, &call).await? };
    http::check_google(&ep.id, &url, &answer, STUDIO)?;
    Ok(answer)
}

fn studio_completion_body(input: &str) -> Value {
    obj! { "contents" => crate::arr![obj! { "parts" => crate::arr![obj! { "text" => input }], "role" => "user" }], "generationConfig" => obj! { "candidateCount" => 1 } }
}

pub async fn infer_studio(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Results> {
    match ep.task {
        TaskType::TextEmbedding => {
            let model_id = ss_str(ep, "model_id").unwrap_or_default();
            let it = req.input_type.as_deref().map(str::to_ascii_lowercase);
            // `text-embedding-004` takes no task type from callers (roll's own ingest and search
            // types still go).
            if model_id == "text-embedding-004"
                && let Some(it) = it.as_deref().filter(|it| !super::internal(it) && *it != "unspecified")
            {
                return Err(validation(&[format!("Invalid value [{it}] received. [input_type] is not allowed for model [{model_id}]")]));
            }
            let model = format!("models/{model_id}");
            let task = google_task_type(it.as_deref());
            let dims = ep.service_settings.get("dimensions").cloned();
            let requests: Vec<Value> = req
                .input
                .iter()
                .map(|t| {
                    let mut r = Map::new();
                    r.insert("model".into(), model.clone().into());
                    r.insert("content".into(), obj! { "parts" => crate::arr![obj! { "text" => t.clone() }] });
                    if let Some(d) = &dims {
                        r.insert("outputDimensionality".into(), d.clone());
                    }
                    if let Some(tt) = task {
                        r.insert("taskType".into(), tt.into());
                    }
                    Value::Obj(r)
                })
                .collect();
            let v = answer_json("googleaistudio", &studio_send(cluster, ep, "batchEmbedContents", false, &obj! { "requests" => Value::Arr(requests) }, req.timeout).await?)?;
            let list = v.get("embeddings").and_then(Value::as_array).ok_or_else(|| missing_field("embeddings", "Google AI Studio embeddings"))?;
            let vectors: Vec<Value> = list.iter().map(|e| e.get("values").cloned().unwrap_or(Value::Null)).collect();
            super::float_vectors(&vectors, req.input.len(), "Google AI Studio embeddings")
        }
        TaskType::Completion => {
            let input = super::one_input(req, "Google AI Studio completion")?;
            let v = answer_json("googleaistudio", &studio_send(cluster, ep, "generateContent", false, &studio_completion_body(input), req.timeout).await?)?;
            Ok(Results::Completion(vec![candidate_text(&v).ok_or_else(|| missing_field("candidates", "Google AI Studio completion"))?]))
        }
        other => Err(crate::xpack::inference::unsupported_task(other, "googleaistudio")),
    }
}

pub async fn stream_completion_studio(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Streaming> {
    let input = super::one_input(req, "Google AI Studio completion")?;
    let answer = studio_send(cluster, ep, "streamGenerateContent", true, &studio_completion_body(input), req.timeout).await?;
    Streaming::open(answer, gemini_completion_decoder())
}
