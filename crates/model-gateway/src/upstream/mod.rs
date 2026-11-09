//! Each upstream kind (docs/code/gateway.md, "Upstreams"): the URL for a request in the API it
//! speaks, its auth and its stream framing.
//!
//! - Anthropic and Anthropic-compatible: `<base>/v1/messages`, `x-api-key`, `anthropic-version`.
//!   (A compatible one also gets the key as a bearer token: those servers take either.)
//! - OpenAI, OpenRouter, OpenAI-compatible: `<base>/chat/completions` and `<base>/responses`,
//!   a bearer key (a compatible one may have none: a local model server).
//! - Azure OpenAI: `<base>/openai/v1/{chat/completions,responses}`, the `api-key` header.
//! - Bedrock: `bedrock-runtime.<region>.amazonaws.com/model/<id>/invoke(-with-response-stream)`,
//!   a Bedrock API key (bearer) or SigV4 ([`sigv4`]); its event stream ([`eventstream`])
//!   decoded into Messages events. The body is Messages without `model` and `stream`, with
//!   `anthropic_version: "bedrock-2023-05-31"` (and the client's `anthropic-beta` as
//!   `anthropic_beta`).
//! - Vertex: `<region>-aiplatform.googleapis.com/v1/projects/<p>/locations/<r>/publishers/
//!   anthropic/models/<m>:(stream)rawPredict` for Claude (Messages without `model`, with
//!   `anthropic_version: "vertex-2023-10-16"`), the `endpoints/openapi/chat/completions`
//!   endpoint for other models; an access token from the service account's JWT ([`vertex`]),
//!   cached until a minute before it expires.
//!
//! A `base_url` set on Bedrock or Vertex replaces the regional host (a VPC endpoint, or a mock
//! in tests). Nothing here reads the client's headers beyond what [`Forward`] carries.
//!
//! The proxy helper owns this module.

pub mod eventstream;
pub mod sigv4;
pub mod vertex;

use crate::Secret;
use crate::net::Net;
use agent_proto::{UpstreamKind, UpstreamSpec, WireFormat};
use serde_json::Value;
use std::time::Duration;

/// A model list is read up to this.
const MAX_LIST: usize = 4 << 20;
/// Models kept from one upstream's list.
pub const MAX_MODELS: usize = 500;
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// What of the client's request goes upstream besides its body.
#[derive(Clone, Debug, Default)]
pub struct Forward {
    /// `roll-gateway/<version> (<the client's own user agent>)`.
    pub user_agent: String,
    /// Messages upstreams only.
    pub anthropic_version: Option<String>,
    pub anthropic_beta: Option<String>,
}

/// How an upstream's answer is framed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// One JSON body (or an SSE stream when the request asked for one).
    Http,
    /// Bedrock's event stream: each `chunk` is one Messages stream event.
    EventStream,
}

/// A request ready to send.
pub struct Outgoing {
    pub method: reqwest::Method,
    pub url: url::Url,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub framing: Framing,
}

impl std::fmt::Debug for Outgoing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Headers hold the key: only their names.
        let names: Vec<&str> = self.headers.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("Outgoing").field("method", &self.method).field("url", &self.url.as_str()).field("headers", &names).finish()
    }
}

/// What a request needs from the gateway besides the upstream.
pub struct Ctx<'a> {
    pub net: &'a Net,
    pub now_ms: u64,
    /// Tests only: where Vertex tokens come from instead of Google.
    pub vertex_token_uri: Option<&'a str>,
}

/// An upstream with its key.
pub struct Upstream {
    pub spec: UpstreamSpec,
    key: Option<Secret>,
    vertex_token: tokio::sync::Mutex<Option<(Secret, u64)>>,
}

impl std::fmt::Debug for Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upstream").field("id", &self.spec.id).field("kind", &self.spec.kind).field("key", &self.key.is_some()).finish()
    }
}

fn is_claude(model: &str) -> bool {
    model.starts_with("claude")
}

impl Upstream {
    pub fn new(spec: UpstreamSpec, key: Option<Secret>) -> Upstream {
        Upstream { spec, key: key.filter(|k| !k.expose().trim().is_empty()), vertex_token: tokio::sync::Mutex::new(None) }
    }

    /// Whether it's this spec with this key (so [`crate::Gateway::apply`] keeps it, and its
    /// cached token).
    pub fn same(&self, spec: &UpstreamSpec, key: Option<&Secret>) -> bool {
        self.spec == *spec && self.key.as_ref().map(Secret::expose) == key.map(Secret::expose).filter(|k| !k.trim().is_empty())
    }

    pub fn has_key(&self) -> bool {
        self.key.is_some()
    }

    /// The APIs it speaks for `model`.
    pub fn speaks(&self, model: &str) -> &'static [WireFormat] {
        use WireFormat::*;
        match self.spec.kind {
            UpstreamKind::Anthropic | UpstreamKind::AnthropicCompatible | UpstreamKind::Bedrock => &[Messages],
            UpstreamKind::Vertex if is_claude(model) => &[Messages],
            UpstreamKind::Vertex | UpstreamKind::Openrouter => &[Chat],
            UpstreamKind::OpenaiCompatible if !self.spec.responses => &[Chat],
            UpstreamKind::Openai | UpstreamKind::AzureOpenai | UpstreamKind::OpenaiCompatible => &[Chat, Responses],
        }
    }

    /// The API a request in `inbound` goes to it in: `inbound` when it speaks it (passed
    /// through), else Messages for the Anthropic family, else Chat.
    pub fn format_for(&self, inbound: WireFormat, model: &str) -> WireFormat {
        let speaks = self.speaks(model);
        if speaks.contains(&inbound) { inbound } else { speaks[0] }
    }

    /// Whether `count_tokens` can be passed to it.
    pub fn counts_tokens(&self) -> bool {
        matches!(self.spec.kind, UpstreamKind::Anthropic | UpstreamKind::AnthropicCompatible)
    }

    fn base(&self) -> Result<url::Url, String> {
        let s = match (&self.spec.base_url, self.spec.kind) {
            (Some(b), _) => b.clone(),
            (None, UpstreamKind::Bedrock) => format!("https://bedrock-runtime.{}.amazonaws.com", self.region()),
            (None, UpstreamKind::Vertex) if self.region() == "global" => "https://aiplatform.googleapis.com".into(),
            (None, UpstreamKind::Vertex) => format!("https://{}-aiplatform.googleapis.com", self.region()),
            (None, k) => k.default_base_url().ok_or_else(|| format!("{} needs a base URL", k.label()))?.to_string(),
        };
        url::Url::parse(&s).map_err(|e| format!("base URL: {e}"))
    }

    fn region(&self) -> &str {
        self.spec.region.as_deref().unwrap_or_default()
    }

    /// `base` + `path` (the base's own path kept, `path` starting with `/`).
    fn url(&self, path: &str) -> Result<url::Url, String> {
        let mut u = self.base()?;
        let joined = format!("{}{path}", u.path().trim_end_matches('/'));
        u.set_path(&joined);
        Ok(u)
    }

    fn need_key(&self) -> Result<&Secret, String> {
        self.key.as_ref().ok_or_else(|| format!("no key is set for {}", self.spec.name))
    }

    /// The headers every request to it carries: its own extra headers, then auth.
    async fn auth(&self, ctx: &Ctx<'_>, headers: &mut Vec<(String, String)>) -> Result<(), String> {
        for (k, v) in &self.spec.headers {
            headers.push((k.clone(), v.clone()));
        }
        match self.spec.kind {
            UpstreamKind::Anthropic => headers.push(("x-api-key".into(), self.need_key()?.expose().into())),
            UpstreamKind::AnthropicCompatible => {
                if let Some(k) = &self.key {
                    headers.push(("x-api-key".into(), k.expose().into()));
                    headers.push(("authorization".into(), format!("Bearer {}", k.expose())));
                }
            }
            UpstreamKind::Openai | UpstreamKind::Openrouter => headers.push(("authorization".into(), format!("Bearer {}", self.need_key()?.expose()))),
            UpstreamKind::OpenaiCompatible => {
                if let Some(k) = &self.key {
                    headers.push(("authorization".into(), format!("Bearer {}", k.expose())));
                }
            }
            UpstreamKind::AzureOpenai => headers.push(("api-key".into(), self.need_key()?.expose().into())),
            UpstreamKind::Vertex => headers.push(("authorization".into(), format!("Bearer {}", self.vertex_token(ctx).await?.expose()))),
            // Signed (or a bearer key) once the body is final: see `bedrock_auth`.
            UpstreamKind::Bedrock => {}
        }
        Ok(())
    }

    /// Bedrock: a bearer API key, or SigV4 over the final request for `service`.
    fn bedrock_auth(&self, service: &str, out: &mut Outgoing, now_ms: u64) -> Result<(), String> {
        let key = self.need_key()?;
        let Some(creds) = sigv4::Credentials::parse(key.expose()) else {
            out.headers.push(("authorization".into(), format!("Bearer {}", key.expose().trim())));
            return Ok(());
        };
        let host = match out.url.port() {
            Some(p) => format!("{}:{p}", out.url.host_str().unwrap_or_default()),
            None => out.url.host_str().unwrap_or_default().to_string(),
        };
        let mut signed = vec![("host".to_string(), host), ("x-amz-date".to_string(), sigv4::amz_date(now_ms / 1000))];
        if out.body.is_some() {
            signed.push(("content-type".into(), "application/json".into()));
        }
        let req = sigv4::Request { method: out.method.as_str(), path: out.url.path(), query: out.url.query().unwrap_or_default(), body: out.body.as_deref().unwrap_or_default() };
        let auth = sigv4::sign(&creds, self.region(), service, &req, &mut signed);
        out.headers.retain(|(k, _)| k != "content-type");
        for (k, v) in signed {
            if k != "host" {
                out.headers.push((k, v));
            }
        }
        out.headers.push(("authorization".into(), auth));
        Ok(())
    }

    async fn vertex_token(&self, ctx: &Ctx<'_>) -> Result<Secret, String> {
        let mut cached = self.vertex_token.lock().await;
        if let Some((t, until)) = cached.as_ref()
            && ctx.now_ms + vertex::EARLY.as_millis() as u64 <= *until
        {
            return Ok(t.clone());
        }
        let sa = vertex::ServiceAccount::parse(self.need_key()?)?;
        let token_uri = match ctx.vertex_token_uri {
            Some(u) => u.to_string(),
            None => {
                if sa.token_uri.as_deref().is_some_and(|u| u != vertex::GOOGLE_TOKEN_URI) {
                    return Err(format!("the service account's token_uri isn't {}", vertex::GOOGLE_TOKEN_URI));
                }
                vertex::GOOGLE_TOKEN_URI.to_string()
            }
        };
        let url = url::Url::parse(&token_uri).map_err(|e| format!("token_uri: {e}"))?;
        let client = ctx.net.client(&url, self.spec.allow_private)?;
        let assertion = vertex::assertion(&sa, &token_uri, ctx.now_ms / 1000)?;
        let (token, until) = vertex::fetch(client, &token_uri, &assertion, ctx.now_ms).await?;
        *cached = Some((token.clone(), until));
        Ok(token)
    }

    /// A generation request: `body` is already in `format` (translated, model set) and asks for
    /// `model` (the upstream's name for it).
    pub async fn generate(&self, ctx: &Ctx<'_>, format: WireFormat, model: &str, mut body: Value, stream: bool, fwd: &Forward) -> Result<Outgoing, String> {
        let mut headers = vec![("user-agent".to_string(), fwd.user_agent.clone()), ("content-type".to_string(), "application/json".to_string())];
        let mut framing = Framing::Http;
        let url = match (self.spec.kind, format) {
            (UpstreamKind::Anthropic | UpstreamKind::AnthropicCompatible, WireFormat::Messages) => {
                headers.push(("anthropic-version".into(), fwd.anthropic_version.clone().unwrap_or_else(|| ANTHROPIC_VERSION.into())));
                if let Some(b) = &fwd.anthropic_beta {
                    headers.push(("anthropic-beta".into(), b.clone()));
                }
                self.url("/v1/messages")?
            }
            (UpstreamKind::Bedrock, WireFormat::Messages) => {
                if let Some(o) = body.as_object_mut() {
                    o.remove("model");
                    o.remove("stream");
                    o.insert("anthropic_version".into(), "bedrock-2023-05-31".into());
                    if let Some(b) = &fwd.anthropic_beta {
                        let betas: Vec<Value> = b.split(',').map(str::trim).filter(|s| !s.is_empty()).map(Value::from).collect();
                        o.insert("anthropic_beta".into(), Value::Array(betas));
                    }
                }
                if stream {
                    framing = Framing::EventStream;
                    headers.push(("accept".into(), "application/vnd.amazon.eventstream".into()));
                }
                let action = if stream { "invoke-with-response-stream" } else { "invoke" };
                self.url(&format!("/model/{}/{action}", sigv4::uri_encode(model)))?
            }
            (UpstreamKind::Vertex, WireFormat::Messages) => {
                if let Some(o) = body.as_object_mut() {
                    o.remove("model");
                    o.insert("anthropic_version".into(), "vertex-2023-10-16".into());
                }
                if let Some(b) = &fwd.anthropic_beta {
                    headers.push(("anthropic-beta".into(), b.clone()));
                }
                let action = if stream { "streamRawPredict" } else { "rawPredict" };
                self.url(&format!("{}/publishers/anthropic/models/{}:{action}", self.vertex_path(), sigv4::uri_encode(model)))?
            }
            (UpstreamKind::Vertex, WireFormat::Chat) => self.url(&format!("{}/endpoints/openapi/chat/completions", self.vertex_path()))?,
            (UpstreamKind::AzureOpenai, WireFormat::Chat) => self.url("/openai/v1/chat/completions")?,
            (UpstreamKind::AzureOpenai, WireFormat::Responses) => self.url("/openai/v1/responses")?,
            (_, WireFormat::Chat) => self.url("/chat/completions")?,
            (_, WireFormat::Responses) => self.url("/responses")?,
            (k, f) => return Err(format!("{} doesn't speak {f:?}", k.label())),
        };
        if stream && framing == Framing::Http {
            headers.push(("accept".into(), "text/event-stream".into()));
        }
        let body = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
        let mut out = Outgoing { method: reqwest::Method::POST, url, headers: Vec::new(), body: Some(body), framing };
        self.auth(ctx, &mut headers).await?;
        out.headers = headers;
        if self.spec.kind == UpstreamKind::Bedrock {
            self.bedrock_auth("bedrock", &mut out, ctx.now_ms)?;
        }
        Ok(out)
    }

    fn vertex_path(&self) -> String {
        format!("/v1/projects/{}/locations/{}", self.spec.project.as_deref().unwrap_or_default(), self.region())
    }

    /// `POST /v1/messages/count_tokens` passed on ([`Upstream::counts_tokens`] kinds only).
    pub async fn count_tokens(&self, ctx: &Ctx<'_>, body: Value, fwd: &Forward) -> Result<Outgoing, String> {
        let mut headers = vec![
            ("user-agent".to_string(), fwd.user_agent.clone()),
            ("content-type".to_string(), "application/json".to_string()),
            ("anthropic-version".to_string(), fwd.anthropic_version.clone().unwrap_or_else(|| ANTHROPIC_VERSION.into())),
        ];
        if let Some(b) = &fwd.anthropic_beta {
            headers.push(("anthropic-beta".into(), b.clone()));
        }
        self.auth(ctx, &mut headers).await?;
        let body = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
        Ok(Outgoing { method: reqwest::Method::POST, url: self.url("/v1/messages/count_tokens")?, headers, body: Some(body), framing: Framing::Http })
    }

    /// The request that lists its models (`None`: the kind can't list; its patterns stand).
    async fn list_request(&self, ctx: &Ctx<'_>) -> Result<Option<Outgoing>, String> {
        let url = match self.spec.kind {
            UpstreamKind::Anthropic | UpstreamKind::AnthropicCompatible => {
                let mut u = self.url("/v1/models")?;
                u.set_query(Some("limit=1000"));
                u
            }
            UpstreamKind::AzureOpenai => self.url("/openai/v1/models")?,
            UpstreamKind::Openai | UpstreamKind::Openrouter | UpstreamKind::OpenaiCompatible => self.url("/models")?,
            UpstreamKind::Bedrock | UpstreamKind::Vertex => return Ok(None),
        };
        let mut headers = vec![("user-agent".to_string(), crate::server::user_agent(None))];
        if matches!(self.spec.kind, UpstreamKind::Anthropic | UpstreamKind::AnthropicCompatible) {
            headers.push(("anthropic-version".into(), ANTHROPIC_VERSION.into()));
        }
        self.auth(ctx, &mut headers).await?;
        Ok(Some(Outgoing { method: reqwest::Method::GET, url, headers, body: None, framing: Framing::Http }))
    }

    /// Its exact model patterns (what a kind that can't list offers).
    pub fn pattern_models(&self) -> Vec<String> {
        self.spec.models.iter().filter(|m| !m.ends_with('*')).cloned().collect()
    }

    /// The models it lists (at most [`MAX_MODELS`]); `Ok(None)`: the kind can't list.
    pub async fn list(&self, ctx: &Ctx<'_>) -> Result<Option<Vec<String>>, String> {
        let Some(out) = self.list_request(ctx).await? else { return Ok(None) };
        let body = self.fetch_json(ctx, out, Duration::from_secs(20)).await?;
        let ids = body
            .get("data")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|m| m.get("id").and_then(Value::as_str)).filter(|id| id.len() <= 256).take(MAX_MODELS).map(str::to_string).collect())
            .unwrap_or_default();
        Ok(Some(ids))
    }

    /// The cheap auth check `gateway.test` runs: the model list (Bedrock's control plane's
    /// `foundation-models`; Vertex: a token). Never generates. The number of models it lists.
    pub async fn check(&self, ctx: &Ctx<'_>) -> Result<u32, String> {
        match self.spec.kind {
            UpstreamKind::Vertex => {
                self.vertex_token(ctx).await?;
                Ok(self.spec.models.len() as u32)
            }
            UpstreamKind::Bedrock => {
                let url = match &self.spec.base_url {
                    Some(b) => url::Url::parse(&format!("{b}/foundation-models")).map_err(|e| e.to_string())?,
                    None => url::Url::parse(&format!("https://bedrock.{}.amazonaws.com/foundation-models", self.region())).map_err(|e| e.to_string())?,
                };
                let mut out = Outgoing { method: reqwest::Method::GET, url, headers: vec![("user-agent".into(), crate::server::user_agent(None))], body: None, framing: Framing::Http };
                self.bedrock_auth("bedrock", &mut out, ctx.now_ms)?;
                let body = self.fetch_json(ctx, out, Duration::from_secs(20)).await?;
                Ok(body.get("modelSummaries").and_then(Value::as_array).map_or(0, |a| a.len() as u32))
            }
            _ => Ok(self.list(ctx).await?.map_or(0, |m| m.len() as u32)),
        }
    }

    async fn fetch_json(&self, ctx: &Ctx<'_>, out: Outgoing, timeout: Duration) -> Result<Value, String> {
        let resp = self.send(ctx.net, out)?.timeout(timeout).send().await.map_err(|e| crate::server::error_chain(&e))?;
        let status = resp.status();
        let body = crate::server::read_capped(resp, MAX_LIST).await?;
        if !status.is_success() {
            let detail: String = String::from_utf8_lossy(&body).chars().take(300).collect();
            return Err(format!("{status}: {detail}"));
        }
        serde_json::from_slice(&body).map_err(|_| "the answer isn't JSON".to_string())
    }

    /// The request, on the client its address allows.
    pub fn send(&self, net: &Net, out: Outgoing) -> Result<reqwest::RequestBuilder, String> {
        let client = net.client(&out.url, self.spec.allow_private)?;
        let mut req = client.request(out.method, out.url);
        for (k, v) in out.headers {
            req = req.header(k, v);
        }
        if let Some(b) = out.body {
            req = req.body(b);
        }
        Ok(req)
    }
}
