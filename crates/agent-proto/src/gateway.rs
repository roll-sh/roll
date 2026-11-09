//! roll's model gateway (docs/code/gateway.md): one of the owner's machines runs
//! `crates/model-gateway` inside its daemon, and the owner's other machines send their providers'
//! model requests to it with a client key of their own.
//!
//! The gateway is for API keys and compatible upstreams the owner pays for. It never stores,
//! proxies or rotates consumer-subscription logins (Claude.ai or ChatGPT sign-ins), never presents
//! one client as another, and never moves between keys of one provider to get around that
//! provider's rate limits: it fails over between upstreams on errors and outages, by the owner's
//! priority.
//!
//! - **Settings** ([`GatewaySettings`]) hold no secret: upstream keys arrive sealed to the gateway
//!   daemon (`gateway.key`, the endpoints' `roll-seal-v1` box with the context
//!   [`upstream_key_context`]), and client keys are minted by the gateway, sealed straight to the
//!   machine that uses them (`gateway.client.issue`), and kept by the gateway only as a hash.
//! - **What the daemon reports** ([`GatewayInfo`], in `MachineConfig::gateway`) is the settings,
//!   where each upstream's key comes from, the clients and whether it runs. Health, usage and the
//!   request log are queries (`gateway.status`, `gateway.usage`, `gateway.log`).
//! - **The plan** ([`GatewayPlan`]) is the owner's, kept on the relay: which machine hosts the
//!   gateway, the URLs members reach it by, and which machines use it. The relay joins a machine
//!   by asking the host for a client key sealed to that machine and passing the box on in
//!   `endpoint.set`; it never calls the gateway itself.

use crate::{KeyChange, KeyState, MachineId, Millis, ProviderId, SealedSecret};
use serde::{Deserialize, Serialize};

/// The limits [`GatewaySettings::check`] holds settings to, and the queries' caps.
pub mod gateway_limits {
    pub const ID: usize = 64;
    pub const NAME: usize = 64;
    pub const URL: usize = 2048;
    /// Upstreams per gateway.
    pub const UPSTREAMS: usize = 32;
    /// Routes per gateway.
    pub const ROUTES: usize = 256;
    /// Targets per route.
    pub const TARGETS: usize = 16;
    /// Model patterns per upstream, and price rows.
    pub const MODELS: usize = 256;
    pub const MODEL: usize = 256;
    /// Extra listen addresses (besides loopback).
    pub const LISTEN: usize = 8;
    /// Client keys (one per machine that uses the gateway, plus a few by hand).
    pub const CLIENTS: usize = 256;
    /// Extra headers per upstream.
    pub const HEADERS: usize = 16;
    pub const HEADER: usize = 1024;
    /// Request log entries one `gateway.log` answers.
    pub const LOG_PAGE: u16 = 200;
    /// Rows one `gateway.usage` answers.
    pub const USAGE_ROWS: usize = 5000;
    /// The members' endpoint id (`EndpointSpec::id`) on every machine that uses the gateway.
    pub const ENDPOINT_ID: &str = "roll-gateway";
}

/// The associated data an upstream's key is sealed with (`gateway.key`), so a box made for one
/// upstream (or for an endpoint) never opens as another's key.
pub fn upstream_key_context(upstream: &str) -> String {
    format!("roll-gateway-upstream:{upstream}")
}

/// An API a request or an upstream speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireFormat {
    /// Anthropic Messages (`POST /v1/messages`): Claude Code.
    Messages,
    /// OpenAI Chat Completions (`POST /v1/chat/completions`): most ACP agents and SDKs.
    Chat,
    /// OpenAI Responses (`POST /v1/responses`): Codex.
    Responses,
}

/// Who an upstream is, which decides its URL, how it's authenticated and what it speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamKind {
    /// api.anthropic.com (or `base_url`): Messages, `x-api-key`.
    Anthropic,
    /// api.openai.com/v1 (or `base_url`): Chat and Responses, a bearer key.
    Openai,
    /// Azure OpenAI: `base_url` is `https://<resource>.openai.azure.com`; the `api-key` header;
    /// Chat and Responses through its `/openai/v1` API. Model names are deployment names.
    AzureOpenai,
    /// Amazon Bedrock in `region`: Anthropic models through InvokeModel(WithResponseStream).
    /// The key is a Bedrock API key (a bearer token), or `ACCESS_KEY_ID:SECRET_ACCESS_KEY` for
    /// SigV4.
    Bedrock,
    /// Google Vertex AI in `project` and `region`: Anthropic models through `rawPredict`, other
    /// models through its OpenAI-compatible Chat endpoint. The key is a service account's JSON.
    Vertex,
    /// openrouter.ai/api/v1 (or `base_url`): Chat, a bearer key.
    Openrouter,
    /// Any base URL that speaks Messages (`<base>/v1/messages`).
    AnthropicCompatible,
    /// Any base URL that speaks Chat (`<base>/chat/completions`, `base_url` includes `/v1`), and
    /// Responses too when `responses` is set.
    OpenaiCompatible,
}

impl UpstreamKind {
    pub fn label(self) -> &'static str {
        match self {
            UpstreamKind::Anthropic => "Anthropic",
            UpstreamKind::Openai => "OpenAI",
            UpstreamKind::AzureOpenai => "Azure OpenAI",
            UpstreamKind::Bedrock => "Amazon Bedrock",
            UpstreamKind::Vertex => "Google Vertex AI",
            UpstreamKind::Openrouter => "OpenRouter",
            UpstreamKind::AnthropicCompatible => "Anthropic-compatible",
            UpstreamKind::OpenaiCompatible => "OpenAI-compatible",
        }
    }

    /// The base URL used when the spec has none (`None`: the kind needs one).
    pub fn default_base_url(self) -> Option<&'static str> {
        match self {
            UpstreamKind::Anthropic => Some("https://api.anthropic.com"),
            UpstreamKind::Openai => Some("https://api.openai.com/v1"),
            UpstreamKind::Openrouter => Some("https://openrouter.ai/api/v1"),
            // Built from `region` (and `project`).
            UpstreamKind::Bedrock | UpstreamKind::Vertex => None,
            UpstreamKind::AzureOpenai | UpstreamKind::AnthropicCompatible | UpstreamKind::OpenaiCompatible => None,
        }
    }

    /// Whether the kind needs `base_url` set.
    pub fn needs_base_url(self) -> bool {
        matches!(self, UpstreamKind::AzureOpenai | UpstreamKind::AnthropicCompatible | UpstreamKind::OpenaiCompatible)
    }
}

/// An upstream as the owner sets it: everything but its key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamSpec {
    /// `[a-z0-9_-]`, at most 64; the sealed key is bound to it.
    pub id: String,
    pub name: String,
    pub kind: UpstreamKind,
    /// `http(s)://…` without credentials; see [`UpstreamKind::default_base_url`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Bedrock and Vertex (`us-east-1`, `us-east5`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Vertex's project id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The models it serves, exact or a prefix ending in `*` (`claude-*`). Empty: whatever it
    /// lists at `/models` (kinds that can't list need patterns).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// `openai_compatible`: it also speaks Responses.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub responses: bool,
    /// Headers sent with every request to it (an organisation id, a project header). Never
    /// `authorization`, `x-api-key` or `api-key`: keys go in the key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    /// Its base URL may be a private, loopback or tailnet address (a model server on the
    /// owner's network). Off: such addresses are refused, literal or resolved.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_private: bool,
    /// The key is read on the gateway's machine from 1Password (`op://…`) instead of sealed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_ref: Option<String>,
    /// Kept but not used.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

/// One step of a route: an upstream, and the model name to ask it for (unset: the client's).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteTarget {
    pub upstream: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Where requests for a model go: its targets in order, the first healthy one serving and the
/// next taking over on failover. An alias is a route whose targets rename the model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteSpec {
    /// The model clients ask for: exact, or a prefix ending in `*`. The most specific route wins
    /// (exact, then the longest prefix).
    pub model: String,
    pub targets: Vec<RouteTarget>,
}

/// A model's price, overriding the gateway's built-in list. Micro-dollars per million tokens
/// (`3_000_000` is $3 per million).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPrice {
    /// Exact or a prefix ending in `*`, matched against the upstream's model name.
    pub model: String,
    pub input: u64,
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
}

fn default_port() -> u16 {
    8790
}

fn default_true() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

/// The gateway as the owner sets it on its machine (`gateway.configure` replaces it whole). No
/// secrets: keys come through `gateway.key` and `gateway.client.issue`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewaySettings {
    /// Off: the gateway stops (its keys, clients and history stay).
    #[serde(default)]
    pub enabled: bool,
    /// One port for every address.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Addresses (IP literals) to listen on besides the loopback addresses: the machine's LAN or
    /// tailnet address, a Docker bridge's gateway. Never an unspecified address (all of IPv4 or
    /// IPv6): the owner names each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listen: Vec<String>,
    /// In priority order: with no route for a model, the first upstream serving it serves, the
    /// next takes over on failover.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upstreams: Vec<UpstreamSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<RouteSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prices: Vec<ModelPrice>,
    /// Keep request and response bodies in the request log (64 KiB each, a day). Off by default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub log_bodies: bool,
    /// Keep a session (Claude Code's or Codex's session header, else `prompt_cache_key`) on the
    /// upstream it started on while that upstream is healthy, for prompt-cache hits.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub affinity: bool,
    /// Days of usage history kept (the request log keeps 7).
    #[serde(default = "default_retention", skip_serializing_if = "is_default_retention")]
    pub retention_days: u16,
}

fn default_retention() -> u16 {
    400
}

fn is_default_retention(d: &u16) -> bool {
    *d == default_retention()
}

impl Default for GatewaySettings {
    fn default() -> Self {
        GatewaySettings {
            enabled: false,
            port: default_port(),
            listen: Vec::new(),
            upstreams: Vec::new(),
            routes: Vec::new(),
            prices: Vec::new(),
            log_bodies: false,
            affinity: true,
            retention_days: default_retention(),
        }
    }
}

impl GatewaySettings {
    /// Trimmed, or why they can't be kept (an `invalid` reply, a 400 from the relay).
    pub fn check(mut self) -> Result<GatewaySettings, String> {
        use gateway_limits as l;
        if self.port < 1024 {
            return Err("the gateway's port is 1024 to 65535".into());
        }
        if self.listen.len() > l::LISTEN {
            return Err(format!("at most {} listen addresses", l::LISTEN));
        }
        for a in &mut self.listen {
            *a = a.trim().to_string();
            let ip: std::net::IpAddr = a.parse().map_err(|_| format!("{a:?} isn't an IP address (the gateway listens on addresses, not names)"))?;
            if ip.is_unspecified() {
                return Err("name each address the gateway listens on; 0.0.0.0 and :: would open it to every network".into());
            }
            if ip.is_multicast() {
                return Err(format!("{a} is a multicast address"));
            }
        }
        if self.upstreams.len() > l::UPSTREAMS {
            return Err(format!("at most {} upstreams", l::UPSTREAMS));
        }
        let mut ids = std::collections::HashSet::new();
        let mut upstreams = Vec::with_capacity(self.upstreams.len());
        for u in std::mem::take(&mut self.upstreams) {
            let u = u.check()?;
            if !ids.insert(u.id.clone()) {
                return Err(format!("two upstreams are called {}", u.id));
            }
            upstreams.push(u);
        }
        self.upstreams = upstreams;
        if self.routes.len() > l::ROUTES {
            return Err(format!("at most {} routes", l::ROUTES));
        }
        let mut models = std::collections::HashSet::new();
        for r in &mut self.routes {
            r.model = r.model.trim().to_string();
            check_pattern(&r.model)?;
            if !models.insert(r.model.clone()) {
                return Err(format!("two routes are for {}", r.model));
            }
            if r.targets.is_empty() || r.targets.len() > l::TARGETS {
                return Err(format!("a route has 1 to {} targets", l::TARGETS));
            }
            for t in &mut r.targets {
                if !ids.contains(&t.upstream) {
                    return Err(format!("the route for {} names {}, which isn't an upstream", r.model, t.upstream));
                }
                t.model = t.model.take().map(|m| m.trim().to_string()).filter(|m| !m.is_empty());
                if let Some(m) = &t.model {
                    check_model(m)?;
                }
            }
        }
        if self.prices.len() > l::MODELS {
            return Err(format!("at most {} prices", l::MODELS));
        }
        for p in &mut self.prices {
            p.model = p.model.trim().to_string();
            check_pattern(&p.model)?;
        }
        if self.retention_days == 0 || self.retention_days > 3650 {
            return Err("usage is kept 1 to 3650 days".into());
        }
        Ok(self)
    }

    pub fn upstream(&self, id: &str) -> Option<&UpstreamSpec> {
        self.upstreams.iter().find(|u| u.id == id)
    }
}

impl UpstreamSpec {
    /// Trimmed, or why it can't be kept.
    pub fn check(mut self) -> Result<UpstreamSpec, String> {
        use gateway_limits as l;
        if !valid_id(&self.id) {
            return Err(format!("an upstream id is 1 to {} of a-z, 0-9, - and _", l::ID));
        }
        self.name = self.name.trim().to_string();
        if self.name.is_empty() || self.name.len() > l::NAME || self.name.chars().any(|c| c.is_control() || c == '<' || c == '>') {
            return Err(format!("an upstream's name is one line of 1 to {} bytes, without < or >", l::NAME));
        }
        let text = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        self.base_url = text(self.base_url.take()).map(|u| u.trim_end_matches('/').to_string());
        self.region = text(self.region.take());
        self.project = text(self.project.take());
        self.key_ref = text(self.key_ref.take());
        match &self.base_url {
            Some(u) => check_url(u)?,
            None if self.kind.needs_base_url() => return Err(format!("{} needs a base URL", self.kind.label())),
            None => {}
        }
        let plain = |s: &str| !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
        match self.kind {
            UpstreamKind::Bedrock if !self.region.as_deref().is_some_and(plain) => return Err("Amazon Bedrock needs a region (us-east-1)".into()),
            UpstreamKind::Vertex if !self.region.as_deref().is_some_and(plain) || !self.project.as_deref().is_some_and(plain) => {
                return Err("Google Vertex AI needs a project and a region (us-east5)".into());
            }
            _ => {}
        }
        if self.region.as_deref().is_some_and(|r| !plain(r)) || self.project.as_deref().is_some_and(|p| !plain(p)) {
            return Err("a region or project is letters, digits, -, _ and .".into());
        }
        if self.responses && self.kind != UpstreamKind::OpenaiCompatible {
            return Err("only an OpenAI-compatible upstream says whether it speaks Responses".into());
        }
        if self.models.len() > l::MODELS {
            return Err(format!("at most {} models per upstream", l::MODELS));
        }
        for m in &mut self.models {
            *m = m.trim().to_string();
            check_pattern(m)?;
        }
        if matches!(self.kind, UpstreamKind::Bedrock) && self.models.is_empty() {
            return Err("name the models Amazon Bedrock serves (anthropic.claude-*)".into());
        }
        if self.headers.len() > l::HEADERS {
            return Err(format!("at most {} headers", l::HEADERS));
        }
        for (k, v) in &mut self.headers {
            *k = k.trim().to_ascii_lowercase();
            let token = !k.is_empty() && k.len() <= 128 && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
            if !token || v.len() > l::HEADER || v.chars().any(|c| c.is_control()) {
                return Err(format!("{k:?} isn't a header name and value on one line"));
            }
            if matches!(k.as_str(), "authorization" | "x-api-key" | "api-key" | "proxy-authorization" | "cookie" | "host" | "content-length" | "transfer-encoding") {
                return Err(format!("{k} is set by the gateway (put a key in the key field)"));
            }
        }
        if let Some(r) = &self.key_ref
            && (!r.starts_with("op://") || r.len() > 512 || r.chars().any(|c| c.is_control() || c.is_whitespace()))
        {
            return Err("a 1Password reference looks like op://vault/item/field".into());
        }
        Ok(self)
    }

    /// Whether it serves `model` by its patterns (an empty list: decided by what it lists).
    pub fn serves(&self, model: &str) -> bool {
        self.models.iter().any(|p| pattern_matches(p, model))
    }
}

/// Exact, or a prefix ending in `*` (`claude-*`; `*` alone matches everything).
pub fn pattern_matches(pattern: &str, model: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => model.starts_with(prefix),
        None => pattern == model,
    }
}

fn check_model(m: &str) -> Result<(), String> {
    if m.is_empty() || m.len() > gateway_limits::MODEL || m.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(format!("{m:?} isn't a model name (1 to {} bytes, no spaces)", gateway_limits::MODEL));
    }
    Ok(())
}

fn check_pattern(p: &str) -> Result<(), String> {
    check_model(p)?;
    if p.trim_end_matches('*').contains('*') {
        return Err(format!("{p:?}: a model pattern has at most one *, at its end"));
    }
    Ok(())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= gateway_limits::ID && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn check_url(url: &str) -> Result<(), String> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"));
    let Some(rest) = rest else { return Err("an upstream's base URL starts with http:// or https://".into()) };
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    if url.len() > gateway_limits::URL || authority.is_empty() || url.chars().any(|c| c.is_control() || c.is_whitespace()) || url.contains('#') || url.contains('?') {
        return Err(format!("an upstream's base URL is http(s)://host[:port][/path], at most {} bytes", gateway_limits::URL));
    }
    if authority.contains('@') {
        return Err("put the key in the key field, not in the URL".into());
    }
    Ok(())
}

/// Where an upstream's key comes from (never any part of it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamKey {
    pub upstream: String,
    pub key: KeyState,
}

/// A client key (`rgk_…`): one per machine that uses the gateway, and any made by hand. The
/// gateway keeps only its hash; nothing can show it again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayClient {
    /// `gwc_…`.
    pub id: String,
    pub name: String,
    /// The machine it was issued for (usage is counted by it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<MachineId>,
    pub created_at: Millis,
}

/// Whether the gateway is serving.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GatewayRun {
    /// Off (`enabled: false`), or never set up.
    #[default]
    Stopped,
    /// Listening on `addresses` (`ip:port`), since `since`.
    Running { addresses: Vec<String>, since: Millis },
    /// It couldn't start, or stopped on its own; the daemon tries again (with backoff) and says
    /// why here. Addresses it did bind are still served.
    Failed {
        error: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        addresses: Vec<String>,
    },
}

/// The gateway as its daemon reports it, in `MachineConfig::gateway` (unset: never set up here).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayInfo {
    pub settings: GatewaySettings,
    /// One per upstream.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<UpstreamKey>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clients: Vec<GatewayClient>,
    #[serde(default)]
    pub run: GatewayRun,
}

/// An upstream's circuit (closed: in use).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CircuitState {
    Closed,
    /// Skipped until `open_until` after failing; then one request tries it (half open).
    Open,
    HalfOpen,
}

/// An upstream's health, as `gateway.status` reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamHealth {
    pub upstream: String,
    pub circuit: CircuitState,
    /// Failures in a row.
    #[serde(default)]
    pub failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_until: Option<Millis>,
    /// The provider asked to wait (a 429's `retry-after`): nothing is sent to it before this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooling_until: Option<Millis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_at: Option<Millis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_at: Option<Millis>,
    /// Requests in the last hour, and the median time to the first byte among them.
    #[serde(default)]
    pub requests_hour: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_ms: Option<u32>,
}

/// `gateway.status`'s answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayStatus {
    pub run: GatewayRun,
    pub upstreams: Vec<UpstreamHealth>,
    /// When each client last sent a request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clients_seen: Vec<ClientSeen>,
    /// The models the gateway offers (`GET /v1/models`): routes' models and what the upstreams
    /// list, at most 500.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientSeen {
    pub client: String,
    pub at: Millis,
}

/// What `gateway.test` found for one upstream: its key opened, it answered its model list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamCheck {
    pub upstream: String,
    pub ok: bool,
    #[serde(default)]
    pub ms: u32,
    /// How many models it listed (kinds that can't list: its patterns).
    #[serde(default)]
    pub models: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What usage is grouped by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageBy {
    #[default]
    Model,
    Machine,
    Project,
    Thread,
    Upstream,
}

/// The bucket usage is summed over.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageBucket {
    Hour,
    #[default]
    Day,
}

/// One bucket of one group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRow {
    /// The bucket's start (UTC).
    pub at: Millis,
    /// The group's key (a model, a machine id, a project id, a thread id, an upstream id); empty
    /// when the request didn't say (a client that isn't roll's).
    pub key: String,
    pub requests: u64,
    #[serde(default)]
    pub errors: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    /// Micro-dollars, from list prices (or the owner's): an estimate, not a bill.
    pub cost_micros: u64,
}

/// `gateway.usage`'s answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageReport {
    pub by: UsageBy,
    pub bucket: UsageBucket,
    pub from: Millis,
    pub to: Millis,
    pub rows: Vec<UsageRow>,
    /// More rows matched than [`gateway_limits::USAGE_ROWS`]; the newest were kept.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// One upstream tried for a request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub upstream: String,
    pub model: String,
    /// The upstream's HTTP status (0: no answer).
    pub status: u16,
    pub ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A request in the log (the last 7 days; bodies only when `log_bodies` is on).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// Increasing; `gateway.log {after}` pages by it.
    pub seq: u64,
    pub at: Millis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<MachineId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    pub format: WireFormat,
    /// What the client asked for.
    pub model: String,
    pub stream: bool,
    /// What the client got (the last attempt's status, or the gateway's own error).
    pub status: u16,
    pub ms: u32,
    /// Time to the first byte sent to the client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_byte_ms: Option<u32>,
    /// Every upstream tried, in order: more than one is a failover.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<Attempt>,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub cost_micros: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Its bodies were kept (`gateway.log.body`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_body: bool,
}

/// A logged request's bodies (each at most 64 KiB, `truncated` when cut).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogBody {
    pub seq: u64,
    pub request: String,
    pub response: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// `gateway.client.issue`'s answer: the client, and its key sealed to the machine that asked
/// for it (the box opens as that machine's `roll-gateway` endpoint key, nowhere else).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuedClient {
    pub client: GatewayClient,
    pub sealed: SealedSecret,
}

/// A way the owner's machines reach the gateway: a URL the host listens on (`http://<a listen
/// address or a name for it>:<port>`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayUrl {
    /// "Tailscale", "Office LAN", "Docker sandboxes", "This computer".
    pub label: String,
    pub url: String,
}

/// A machine that uses the gateway.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayMember {
    pub machine: MachineId,
    /// The URL its providers use (one of the plan's).
    pub url: String,
    /// The providers that use it (empty: every provider that can).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderId>,
    /// Its client key (`gwc_…`) on the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    pub joined_at: Millis,
    /// Why joining last failed (the machine was offline, the host didn't answer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The owner's gateway, kept on the relay (`/v1/code/agents/{owner}/gateway`): no keys.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayPlan {
    /// The machine that runs it.
    pub host: MachineId,
    /// How members reach it, the first being the default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<GatewayUrl>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<GatewayMember>,
    /// New sandboxes join as their daemon first says hello.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub new_sandboxes: bool,
    /// When the gateway doesn't answer, a member's providers start on their own login or keys
    /// instead of failing (`EndpointSpec::fallback`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fallback: bool,
    /// The owner read the notice (every AI request from the members goes out from the host).
    #[serde(default)]
    pub acknowledged_at: Millis,
    #[serde(default)]
    pub updated_at: Millis,
}

impl GatewayPlan {
    pub fn check(mut self) -> Result<GatewayPlan, String> {
        if self.urls.len() > gateway_limits::LISTEN + 2 {
            return Err("too many gateway URLs".into());
        }
        for u in &mut self.urls {
            u.label = u.label.trim().to_string();
            u.url = u.url.trim().trim_end_matches('/').to_string();
            if u.label.is_empty() || u.label.len() > gateway_limits::NAME || u.label.chars().any(char::is_control) {
                return Err("a gateway URL's label is one line of 1 to 64 bytes".into());
            }
            check_url(&u.url)?;
        }
        if self.members.len() > gateway_limits::CLIENTS {
            return Err(format!("at most {} machines use one gateway", gateway_limits::CLIENTS));
        }
        Ok(self)
    }

    pub fn member(&self, machine: &MachineId) -> Option<&GatewayMember> {
        self.members.iter().find(|m| &m.machine == machine)
    }
}

/// `POST …/gateway/members`: join a machine (the relay asks the host for its key, sealed to the
/// machine, and sets it there).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayJoin {
    pub machine: MachineId,
    /// Unset: the plan's first URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderId>,
}

/// The gateway commands' key changes for upstreams reuse the endpoints' [`KeyChange`].
pub type UpstreamKeyChange = KeyChange;

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(kind: UpstreamKind) -> UpstreamSpec {
        UpstreamSpec {
            id: "anthropic".into(),
            name: " Anthropic ".into(),
            kind,
            base_url: None,
            region: None,
            project: None,
            models: vec![],
            responses: false,
            headers: vec![],
            allow_private: false,
            key_ref: None,
            disabled: false,
        }
    }

    #[test]
    fn upstreams_are_trimmed_and_checked() {
        let u = upstream(UpstreamKind::Anthropic).check().unwrap();
        assert_eq!(u.name, "Anthropic");
        let bad = |kind: UpstreamKind, f: fn(&mut UpstreamSpec)| {
            let mut u = upstream(kind);
            f(&mut u);
            u.check().unwrap_err()
        };
        assert!(bad(UpstreamKind::OpenaiCompatible, |_| {}).contains("base URL"));
        assert!(bad(UpstreamKind::Bedrock, |_| {}).contains("region"));
        assert!(bad(UpstreamKind::Bedrock, |u| u.region = Some("us-east-1".into())).contains("models"));
        assert!(bad(UpstreamKind::Vertex, |u| u.region = Some("us-east5".into())).contains("project"));
        assert!(bad(UpstreamKind::Anthropic, |u| u.base_url = Some("https://k:sk@x".into())).contains("key field"));
        assert!(bad(UpstreamKind::Anthropic, |u| u.headers = vec![("Authorization".into(), "Bearer x".into())]).contains("set by the gateway"));
        assert!(bad(UpstreamKind::Anthropic, |u| u.models = vec!["a*b".into()]).contains("one *"));
        assert!(bad(UpstreamKind::Anthropic, |u| u.responses = true).contains("Responses"));
        let ok = UpstreamSpec { region: Some("us-east-1".into()), models: vec!["anthropic.claude-*".into()], ..upstream(UpstreamKind::Bedrock) };
        assert!(ok.check().is_ok());
    }

    #[test]
    fn settings_name_every_address_and_route_to_known_upstreams() {
        let s = GatewaySettings { upstreams: vec![upstream(UpstreamKind::Anthropic)], ..Default::default() };
        assert!(s.clone().check().is_ok());
        assert!(GatewaySettings { listen: vec!["0.0.0.0".into()], ..s.clone() }.check().unwrap_err().contains("every network"));
        assert!(GatewaySettings { listen: vec!["gw.local".into()], ..s.clone() }.check().unwrap_err().contains("IP"));
        assert!(GatewaySettings { listen: vec![format!(" {} ", std::net::Ipv4Addr::new(100, 64, 0, 10))], ..s.clone() }.check().is_ok());
        let route = |up: &str| RouteSpec { model: "fast".into(), targets: vec![RouteTarget { upstream: up.into(), model: Some("claude-haiku-4-5".into()) }] };
        assert!(GatewaySettings { routes: vec![route("anthropic")], ..s.clone() }.check().is_ok());
        assert!(GatewaySettings { routes: vec![route("nope")], ..s.clone() }.check().unwrap_err().contains("isn't an upstream"));
        assert!(GatewaySettings { upstreams: vec![upstream(UpstreamKind::Anthropic), upstream(UpstreamKind::Anthropic)], ..s }.check().unwrap_err().contains("two upstreams"));
    }

    #[test]
    fn patterns_are_exact_or_a_prefix() {
        assert!(pattern_matches("claude-*", "claude-opus-4-5"));
        assert!(pattern_matches("*", "anything"));
        assert!(pattern_matches("gpt-5", "gpt-5"));
        assert!(!pattern_matches("gpt-5", "gpt-5.5"));
    }
}
