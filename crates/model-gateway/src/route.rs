//! Which upstreams serve a request, in order (docs/code/gateway.md, "Routing"): the most
//! specific route for the model (exact, then the longest prefix), else every enabled upstream
//! that serves the model in the settings' order; then health: open circuits and cooling
//! upstreams are skipped, and with affinity on a session goes back to the upstream it started
//! on while that one is healthy.
//!
//! Failover is between upstreams on errors and outages (a connection failure, a timeout, 5xx,
//! Anthropic's 529 overloaded) before the first byte reaches the client. A 429 cools that
//! upstream down for its `retry-after` and only an upstream of another provider (another
//! kind, or another host) may take over: never another key of the same provider.
//!
//! - **Circuits** ([`Health`]): an upstream opens after [`OPEN_AFTER`] failures in a row, for
//!   [`OPEN_FIRST`], doubling each time it opens again up to [`OPEN_MAX`]. Once that passes it
//!   is half open: one request is let through ([`Health::admit`]); its success closes the
//!   circuit, its failure opens it again for longer.
//! - **Cool-downs**: a 429 sets `cooling_until` (its `retry-after`, else [`COOL_DEFAULT`], at
//!   most [`COOL_MAX`]); nothing is sent to it before then. A cool-down isn't a failure: the
//!   provider is up and asked to wait.
//! - **Providers** ([`same_provider`]): the named kinds are one provider each whatever their
//!   host or key (two Anthropic keys, two Azure resources, two Bedrock regions are one
//!   provider); a compatible upstream is the provider at its host; two upstreams at one host
//!   are one provider whatever their kinds.
//! - **Affinity** ([`session_id`]): explicit session ids only, ported from CLIProxyAPI's
//!   `sdk/cliproxy/auth/selector.go` `extractSessionIDs` / `sdk/cliproxy/session/info.go`
//!   (MIT) without its message hashing: Claude Code's `X-Claude-Code-Session-Id`, Codex's
//!   `session_id`, `conversation_id`, `X-Session-Affinity`, then the body's
//!   `prompt_cache_key` and `metadata.user_id`. At most [`AFFINITY_MAX`] sessions, each
//!   forgotten [`AFFINITY_TTL`] after its last request.
//!
//! Time is passed in (milliseconds since the epoch), so the state machine is tested without
//! waiting.
//!
//! The proxy helper owns this module.

use agent_proto::gateway::pattern_matches;
use agent_proto::{CircuitState, GatewaySettings, UpstreamHealth, UpstreamKind, UpstreamSpec};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// Failures in a row that open a circuit.
pub const OPEN_AFTER: u32 = 5;
pub const OPEN_FIRST: Duration = Duration::from_secs(30);
pub const OPEN_MAX: Duration = Duration::from_secs(300);
/// A 429 without `retry-after`.
pub const COOL_DEFAULT: Duration = Duration::from_secs(30);
pub const COOL_MAX: Duration = Duration::from_secs(600);
/// A half-open circuit's one request: if it never reports (the client went away before the
/// upstream answered), another may try after this.
pub const PROBE_MAX: Duration = Duration::from_secs(960);
pub const AFFINITY_MAX: usize = 10_000;
pub const AFFINITY_TTL: Duration = Duration::from_secs(3600);
/// A session id longer than this is ignored.
const SESSION_ID_MAX: usize = 256;

fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

/// One step of a request's plan: an upstream and the model to ask it for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub upstream: String,
    pub model: String,
}

/// The targets for `model` by the settings alone, in order. `lists(upstream, model)` says
/// whether an upstream without model patterns lists `model` (`None`: not known, so it's
/// tried).
pub fn targets(settings: &GatewaySettings, model: &str, lists: impl Fn(&str, &str) -> Option<bool>) -> Vec<Target> {
    let enabled = |id: &str| settings.upstream(id).is_some_and(|u| !u.disabled);
    let route = settings.routes.iter().find(|r| r.model == model).or_else(|| {
        settings
            .routes
            .iter()
            .filter(|r| r.model.ends_with('*') && pattern_matches(&r.model, model))
            .max_by_key(|r| r.model.len())
    });
    if let Some(r) = route {
        return r
            .targets
            .iter()
            .filter(|t| enabled(&t.upstream))
            .map(|t| Target { upstream: t.upstream.clone(), model: t.model.clone().unwrap_or_else(|| model.to_string()) })
            .collect();
    }
    settings
        .upstreams
        .iter()
        .filter(|u| !u.disabled)
        .filter(|u| if u.models.is_empty() { lists(&u.id, model).unwrap_or(true) } else { u.serves(model) })
        .map(|u| Target { upstream: u.id.clone(), model: model.to_string() })
        .collect()
}

/// The host an upstream's requests go to (lowercased), from its base URL or its kind's.
pub fn host_of(spec: &UpstreamSpec) -> String {
    if let Some(base) = &spec.base_url {
        return url::Url::parse(base).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase)).unwrap_or_default();
    }
    let region = spec.region.as_deref().unwrap_or_default();
    match spec.kind {
        UpstreamKind::Bedrock => format!("bedrock-runtime.{region}.amazonaws.com"),
        UpstreamKind::Vertex if region == "global" => "aiplatform.googleapis.com".into(),
        UpstreamKind::Vertex => format!("{region}-aiplatform.googleapis.com"),
        k => k.default_base_url().and_then(|b| url::Url::parse(b).ok()).and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default(),
    }
}

/// Whether two upstreams are one provider (a 429 from one never fails over to the other).
pub fn same_provider(a: &UpstreamSpec, b: &UpstreamSpec) -> bool {
    let named = |k: UpstreamKind| !matches!(k, UpstreamKind::AnthropicCompatible | UpstreamKind::OpenaiCompatible);
    (named(a.kind) && a.kind == b.kind) || host_of(a) == host_of(b)
}

/// The session a request belongs to, for affinity: explicit ids only (see the module doc),
/// namespaced by where it came from.
pub fn session_id(headers: &axum::http::HeaderMap, body: &serde_json::Value) -> Option<String> {
    let clean = |s: &str| {
        let s = s.trim();
        (!s.is_empty() && s.len() <= SESSION_ID_MAX && s.bytes().all(|b| b.is_ascii_graphic())).then(|| s.to_string())
    };
    let header = |names: &[&str]| names.iter().find_map(|n| headers.get(*n).and_then(|v| v.to_str().ok()).and_then(clean));
    if let Some(s) = header(&["x-claude-code-session-id"]) {
        return Some(format!("claude:{s}"));
    }
    if let Some(s) = header(&["session_id", "session-id"]) {
        return Some(format!("session:{s}"));
    }
    if let Some(s) = header(&["conversation_id", "conversation-id"]) {
        return Some(format!("conversation:{s}"));
    }
    if let Some(s) = header(&["x-session-affinity"]) {
        return Some(format!("affinity:{s}"));
    }
    if let Some(s) = body.get("prompt_cache_key").and_then(|v| v.as_str()).and_then(clean) {
        return Some(format!("pck:{s}"));
    }
    body.pointer("/metadata/user_id").and_then(|v| v.as_str()).and_then(clean).map(|s| format!("user:{s}"))
}

#[derive(Clone, Debug, Default)]
struct State {
    failures: u32,
    /// Times opened since it last succeeded (the next open lasts `OPEN_FIRST * 2^(opens-1)`).
    opens: u32,
    open_until: Option<u64>,
    /// A half-open circuit's request is out, until this.
    probe_until: Option<u64>,
    cooling_until: Option<u64>,
    last_ok_at: Option<u64>,
    last_error: Option<String>,
    last_error_at: Option<u64>,
}

impl State {
    fn circuit(&self, now: u64) -> CircuitState {
        match self.open_until {
            None => CircuitState::Closed,
            Some(t) if now < t => CircuitState::Open,
            Some(_) => CircuitState::HalfOpen,
        }
    }

    fn cooling(&self, now: u64) -> bool {
        self.cooling_until.is_some_and(|t| now < t)
    }

    /// Whether a request may go to it now (without taking a half-open circuit's one request).
    fn available(&self, now: u64) -> bool {
        if self.cooling(now) {
            return false;
        }
        match self.circuit(now) {
            CircuitState::Closed => true,
            CircuitState::Open => false,
            CircuitState::HalfOpen => self.probe_until.is_none_or(|t| now >= t),
        }
    }
}

/// Why nothing could be tried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// Every target is cooling down after a 429: the soonest is free at this time.
    Cooling { until: u64 },
    /// Every target's circuit is open.
    Open,
}

/// Every upstream's circuit and cool-down, and the session map. Kept across
/// [`crate::Gateway::apply`] (by upstream id).
#[derive(Default)]
pub struct Health {
    states: Mutex<HashMap<String, State>>,
    affinity: Mutex<HashMap<String, (String, u64)>>,
}

impl Health {
    pub fn new() -> Health {
        Health::default()
    }

    /// `targets` that may be tried now, in order: the session's upstream first when it's one of
    /// them and available. Empty: why.
    pub fn order(&self, targets: Vec<Target>, session: Option<&str>, now: u64) -> Result<Vec<Target>, Unavailable> {
        let states = self.states.lock().unwrap();
        let state = |id: &str| states.get(id).cloned().unwrap_or_default();
        let mut open: Vec<Target> = targets.iter().filter(|t| state(&t.upstream).available(now)).cloned().collect();
        if open.is_empty() {
            let cooling = targets.iter().filter_map(|t| state(&t.upstream).cooling_until.filter(|&c| now < c)).min();
            let all_cooling = !targets.is_empty() && targets.iter().all(|t| state(&t.upstream).cooling(now));
            return Err(match cooling {
                Some(until) if all_cooling => Unavailable::Cooling { until },
                _ => Unavailable::Open,
            });
        }
        drop(states);
        if let Some(upstream) = session.and_then(|s| self.session(s, now))
            && let Some(i) = open.iter().position(|t| t.upstream == upstream)
        {
            let t = open.remove(i);
            open.insert(0, t);
        }
        Ok(open)
    }

    /// Takes `upstream` for a request now: `false` if it became unavailable (cooling, open, or
    /// a half-open circuit whose one request is already out).
    pub fn admit(&self, upstream: &str, now: u64) -> bool {
        let mut states = self.states.lock().unwrap();
        let s = states.entry(upstream.to_string()).or_default();
        if !s.available(now) {
            return false;
        }
        if s.circuit(now) == CircuitState::HalfOpen {
            s.probe_until = Some(now + ms(PROBE_MAX));
        }
        true
    }

    /// It answered (any status that isn't a failure): the circuit closes.
    pub fn success(&self, upstream: &str, now: u64) {
        let mut states = self.states.lock().unwrap();
        let s = states.entry(upstream.to_string()).or_default();
        s.failures = 0;
        s.opens = 0;
        s.open_until = None;
        s.probe_until = None;
        s.last_ok_at = Some(now);
    }

    /// It failed (no answer, a timeout, 5xx, 529).
    pub fn failure(&self, upstream: &str, error: &str, now: u64) {
        let mut states = self.states.lock().unwrap();
        let s = states.entry(upstream.to_string()).or_default();
        let half_open = s.circuit(now) == CircuitState::HalfOpen;
        s.failures = s.failures.saturating_add(1);
        s.last_error = Some(error.chars().take(500).collect());
        s.last_error_at = Some(now);
        if half_open || s.failures >= OPEN_AFTER {
            s.opens = s.opens.saturating_add(1);
            let factor = 1u64 << s.opens.saturating_sub(1).min(16);
            s.open_until = Some(now + ms(OPEN_FIRST).saturating_mul(factor).min(ms(OPEN_MAX)));
            s.probe_until = None;
        }
    }

    /// It answered 429: nothing goes to it until `retry_after` has passed.
    pub fn rate_limited(&self, upstream: &str, retry_after: Option<Duration>, now: u64) {
        let mut states = self.states.lock().unwrap();
        let s = states.entry(upstream.to_string()).or_default();
        let wait = retry_after.unwrap_or(COOL_DEFAULT).min(COOL_MAX);
        s.cooling_until = Some(now + ms(wait));
        s.last_error = Some("429: rate limited".into());
        s.last_error_at = Some(now);
        // A 429 answered: if this was a half-open circuit's request, let the next one try.
        s.probe_until = None;
    }

    /// Keeps `session` on `upstream`.
    pub fn bind(&self, session: &str, upstream: &str, now: u64) {
        let mut map = self.affinity.lock().unwrap();
        if map.len() >= AFFINITY_MAX && !map.contains_key(session) {
            map.retain(|_, (_, at)| now.saturating_sub(*at) < ms(AFFINITY_TTL));
            if map.len() >= AFFINITY_MAX {
                // Still full of live sessions: forget the oldest tenth.
                let mut ats: Vec<u64> = map.values().map(|(_, at)| *at).collect();
                ats.sort_unstable();
                let cut = ats[AFFINITY_MAX / 10];
                map.retain(|_, (_, at)| *at > cut);
            }
        }
        map.insert(session.to_string(), (upstream.to_string(), now));
    }

    /// The upstream `session` is on, if it's still remembered.
    pub fn session(&self, session: &str, now: u64) -> Option<String> {
        let map = self.affinity.lock().unwrap();
        map.get(session).filter(|(_, at)| now.saturating_sub(*at) < ms(AFFINITY_TTL)).map(|(u, _)| u.clone())
    }

    pub fn sessions(&self) -> usize {
        self.affinity.lock().unwrap().len()
    }

    /// Forgets upstreams that are gone, and sessions on them.
    pub fn retain(&self, ids: &[&str]) {
        self.states.lock().unwrap().retain(|k, _| ids.contains(&k.as_str()));
        self.affinity.lock().unwrap().retain(|_, (u, _)| ids.contains(&u.as_str()));
    }

    /// `upstream`'s health now (`requests_hour` and `p50_ms` are filled in by the caller).
    pub fn health(&self, upstream: &str, now: u64) -> UpstreamHealth {
        let states = self.states.lock().unwrap();
        let s = states.get(upstream).cloned().unwrap_or_default();
        let circuit = s.circuit(now);
        UpstreamHealth {
            upstream: upstream.to_string(),
            circuit,
            failures: s.failures,
            open_until: s.open_until.filter(|_| circuit != CircuitState::Closed),
            cooling_until: s.cooling_until.filter(|&t| now < t),
            last_ok_at: s.last_ok_at,
            last_error: s.last_error,
            last_error_at: s.last_error_at,
            requests_hour: 0,
            p50_ms: None,
        }
    }
}
