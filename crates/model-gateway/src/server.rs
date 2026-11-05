//! The inbound HTTP API (docs/code/gateway.md, "Inbound"):
//!
//! - `POST /v1/messages`, `POST /v1/messages/count_tokens` (Anthropic Messages)
//! - `POST /v1/chat/completions` (OpenAI Chat)
//! - `POST /v1/responses` (OpenAI Responses)
//! - `GET /v1/models` (both shapes: OpenAI's `data[].id` plus Anthropic's fields)
//! - `GET /health` (no key: what a member's daemon checks before relying on it)
//!
//! There is no other route: settings, keys and clients are managed by the daemon's commands
//! through [`crate::Gateway`]'s methods, never over HTTP.
//!
//! **Clients.** A client key comes as `Authorization: Bearer`, `x-api-key` or `api-key` and is
//! looked up in the store on every request (a revoked key fails at once). Without a valid one
//! the answer is a 401 in the client's API's shape.
//!
//! **What goes upstream.** The body (translated if the upstream speaks another API) and
//! headers the gateway builds: the upstream's own headers and key, `content-type`, `accept`,
//! `anthropic-version`/`anthropic-beta` for Messages upstreams, and `user-agent:
//! roll-gateway/<version> (<the client's user agent>)`, honest about both. Nothing else of the
//! client's: not its auth, cookies, session ids (read for affinity only) or `x-roll-*`
//! headers (`x-roll-thread`/`x-roll-project` are read into the request log).
//!
//! **Limits.**
//! - [`MAX_BODY`] per request body (413 over it), [`MAX_RESPONSE`] per whole upstream answer,
//!   [`crate::stream::MAX_EVENT`] per streamed event (the stream ends with an error event).
//! - [`MAX_IN_FLIGHT`] requests at once (503 over it).
//! - [`crate::net::CONNECT_TIMEOUT`] to connect, [`crate::net::FIRST_BYTE_TIMEOUT`] to an
//!   answer's headers (long thinking), [`crate::net::IDLE_TIMEOUT`] between two reads.
//!
//! **The proxy loop.** The targets come from [`crate::route`]; each is tried in turn until one
//! answers. A connection error, a timeout, 5xx, 529, 401/403 (the owner's key is wrong) or an
//! error event before the stream's first event moves on to the next (failover, counted
//! against the upstream's circuit); a 404 moves on without counting; a 429 cools the upstream
//! down and bars every other upstream of its provider for this request; any other answer goes
//! to the client. Once a stream's first event is good the gateway is committed to that
//! upstream: a failure after it ends the client's stream with an error event, never a
//! switch.
//!
//! **Pass-through.** When the upstream speaks the client's API the body goes up with only
//! `model` changed and the answer comes back byte for byte (a stream event by event, as the
//! upstream framed it), its usage read on the way. Otherwise [`crate::wire`] translates the
//! request, the answer and the stream.
//!
//! **Listeners** ([`Listeners`]): loopback (IPv4, and IPv6 where the machine has it) plus
//! `settings.listen`, all on `settings.port`. Only the addresses that change are bound or
//! closed; a closed listener stops accepting and finishes what it's serving. A bind that fails
//! sets [`GatewayRun::Failed`] and is retried every 1 s, doubling to 60 s.
//!
//! The proxy helper owns this module.

use crate::route::{self, Target, Unavailable};
use crate::store::{BODY_CAP, keep_body};
use crate::stream::{self, BedrockFrames, Early, RawEvent, SseFrames};
use crate::upstream::{Ctx, Forward, Framing, Upstream};
use crate::wire::{self, StreamTranslator, UsageTap};
use crate::{ClientRef, Gateway, Listing, Live, Record, Usage, now_ms};
use agent_proto::{Attempt, GatewayRun, GatewaySettings, UpstreamSpec, WireFormat};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::OwnedSemaphorePermit;

/// A request body.
pub const MAX_BODY: usize = 32 << 20;
/// A whole (non-streamed) upstream answer.
pub const MAX_RESPONSE: usize = 32 << 20;
/// Requests served at once.
pub const MAX_IN_FLIGHT: usize = 256;
/// An upstream error's body kept to answer with.
const MAX_ERROR_BODY: usize = 64 << 10;
/// Model lists are asked again after this.
const LIST_TTL: u64 = 600_000;
/// A model list that failed is asked again after this.
const LIST_RETRY: u64 = 60_000;
const LIST_TIMEOUT: Duration = Duration::from_secs(10);
/// Models `GET /v1/models` answers.
const MAX_OFFERED: usize = 500;
/// `x-roll-thread` / `x-roll-project`.
const ROLL_ID_MAX: usize = 128;

/// `roll-gateway/<version>`, then the client's own user agent in parentheses.
pub fn user_agent(client: Option<&str>) -> String {
    let own = concat!("roll-gateway/", env!("CARGO_PKG_VERSION"));
    let client: String = client.unwrap_or_default().chars().filter(|c| c.is_ascii_graphic() || *c == ' ').take(200).collect();
    match client.trim() {
        "" => own.to_string(),
        c => format!("{own} ({c})"),
    }
}

/// A reqwest error with its causes (reqwest's own message leaves them out).
pub fn error_chain(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        return "timed out".into();
    }
    let mut out = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        out.push_str(": ");
        out.push_str(&s.to_string());
        source = s.source();
    }
    out
}

/// A response's body, refused past `cap` bytes (whatever its `content-length` says).
pub async fn read_capped(resp: reqwest::Response, cap: usize) -> Result<Vec<u8>, String> {
    read_capped_idle(resp, cap, crate::net::IDLE_TIMEOUT).await
}

/// [`read_capped`], failing when no bytes come for `idle`.
pub async fn read_capped_idle(mut resp: reqwest::Response, cap: usize, idle: Duration) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let next = tokio::time::timeout(idle, resp.chunk()).await.map_err(|_| format!("the answer stopped for {} ms", idle.as_millis()))?;
        let Some(chunk) = next.map_err(|e| error_chain(&e))? else { break };
        if out.len() + chunk.len() > cap {
            return Err(format!("the answer is over {cap} bytes"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// The addresses `settings` listen on.
pub fn addresses(settings: &GatewaySettings) -> Vec<SocketAddr> {
    let port = settings.port;
    let mut out = vec![SocketAddr::from((Ipv4Addr::LOCALHOST, port)), SocketAddr::from((Ipv6Addr::LOCALHOST, port))];
    for a in &settings.listen {
        if let Ok(ip) = a.parse::<IpAddr>() {
            let s = SocketAddr::new(ip, port);
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

/// What the gateway listens on.
#[derive(Default)]
pub struct Listeners {
    want: Vec<SocketAddr>,
    /// Each bound address and the switch that closes it.
    serving: Vec<(SocketAddr, tokio::sync::watch::Sender<bool>)>,
    retry: Option<tokio::task::JoinHandle<()>>,
    since: Option<u64>,
}

impl Gateway {
    /// Listens on exactly `want` (nothing: stopped).
    pub(crate) async fn listen(&self, want: Vec<SocketAddr>) -> GatewayRun {
        let mut l = self.inner.listeners.lock().await;
        if let Some(r) = l.retry.take() {
            r.abort();
        }
        l.serving.retain(|(a, stop)| {
            let keep = want.contains(a);
            if !keep {
                stop.send_replace(true);
            }
            keep
        });
        l.want = want;
        let (run, failed) = self.bind_missing(&mut l).await;
        if failed {
            l.retry = Some(self.retry_binding());
        }
        run
    }

    /// Binds what isn't bound yet; the state it reached and whether something failed.
    async fn bind_missing(&self, l: &mut Listeners) -> (GatewayRun, bool) {
        let mut errors = Vec::new();
        for addr in l.want.clone() {
            if l.serving.iter().any(|(a, _)| *a == addr) {
                continue;
            }
            match tokio::net::TcpListener::bind(addr).await {
                Ok(listener) => {
                    let stop = self.serve(listener);
                    l.serving.push((addr, stop));
                }
                // A machine without IPv6 loopback is fine.
                Err(e) if addr.ip() == IpAddr::V6(Ipv6Addr::LOCALHOST) => tracing::debug!("gateway: not listening on {addr}: {e}"),
                Err(e) => errors.push(format!("{addr}: {e}")),
            }
        }
        let addresses: Vec<String> = l.serving.iter().map(|(a, _)| a.to_string()).collect();
        let run = if l.want.is_empty() {
            l.since = None;
            GatewayRun::Stopped
        } else if errors.is_empty() {
            GatewayRun::Running { addresses, since: *l.since.get_or_insert_with(now_ms) }
        } else {
            l.since = None;
            GatewayRun::Failed { error: format!("can't listen on {}", errors.join("; ")), addresses }
        };
        self.inner.run.send_if_modified(|r| {
            let changed = *r != run;
            *r = run.clone();
            changed
        });
        (run, !errors.is_empty())
    }

    fn retry_binding(&self) -> tokio::task::JoinHandle<()> {
        let gw = self.clone();
        tokio::spawn(async move {
            let mut delay = Duration::from_secs(1);
            loop {
                tokio::time::sleep(delay).await;
                let mut l = gw.inner.listeners.lock().await;
                let (_, failed) = gw.bind_missing(&mut l).await;
                if !failed {
                    l.retry = None;
                    return;
                }
                delay = (delay * 2).min(Duration::from_secs(60));
            }
        })
    }

    fn serve(&self, listener: tokio::net::TcpListener) -> tokio::sync::watch::Sender<bool> {
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let app = router(self.clone());
        tokio::spawn(async move {
            let shutdown = async move {
                let _ = stopped.wait_for(|s| *s).await;
            };
            if let Err(e) = axum::serve(listener, app).with_graceful_shutdown(shutdown).await {
                tracing::warn!("gateway: serving: {e}");
            }
        });
        stop
    }

    /// Asks upstreams for their model lists when they're stale: those without model patterns
    /// (routing needs them), or every enabled one (`all`, for `GET /v1/models`).
    pub(crate) async fn ensure_lists(&self, live: &Live, all: bool) {
        let stale = |now: u64| -> Vec<Arc<Upstream>> {
            let models = self.inner.models.lock().unwrap();
            live.settings
                .upstreams
                .iter()
                .filter(|u| !u.disabled && (all || u.models.is_empty()))
                .filter(|u| models.get(&u.id).is_none_or(|l| now.saturating_sub(l.at) > if l.models.is_some() { LIST_TTL } else { LIST_RETRY }))
                .filter_map(|u| live.upstreams.get(&u.id).cloned())
                .collect()
        };
        if stale(now_ms()).is_empty() {
            return;
        }
        let _one = self.inner.listing.lock().await;
        let need = stale(now_ms());
        let token_uri = self.inner.vertex_token_uri.lock().unwrap().clone();
        let lists = need.iter().map(|up| {
            let net = &self.inner.net;
            let token_uri = token_uri.as_deref();
            async move {
                let ctx = Ctx { net, now_ms: now_ms(), vertex_token_uri: token_uri };
                let listed = tokio::time::timeout(LIST_TIMEOUT, up.list(&ctx)).await;
                let models = match listed {
                    Ok(Ok(Some(m))) => Some(m),
                    Ok(Ok(None)) => None,
                    Ok(Err(e)) => {
                        tracing::debug!("gateway: listing {}'s models: {e}", up.spec.id);
                        None
                    }
                    Err(_) => None,
                };
                (up.spec.id.clone(), models)
            }
        });
        let done = futures_util::future::join_all(lists).await;
        let now = now_ms();
        let mut models = self.inner.models.lock().unwrap();
        for (id, listed) in done {
            models.insert(id, Listing { at: now, models: listed });
        }
    }

    /// Whether `upstream` lists `model` (`None`: not known).
    fn lists(&self, upstream: &str, model: &str) -> Option<bool> {
        let models = self.inner.models.lock().unwrap();
        models.get(upstream).and_then(|l| l.models.as_ref()).map(|m| m.iter().any(|x| x == model))
    }

    /// The models the gateway offers: routes' models, each enabled upstream's exact patterns
    /// and what it lists (matching its patterns, if it has any). At most [`MAX_OFFERED`].
    pub(crate) fn offered(&self, live: &Live) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut push = |m: &str| {
            if out.len() < MAX_OFFERED && !out.iter().any(|x| x == m) {
                out.push(m.to_string());
            }
        };
        for r in &live.settings.routes {
            if !r.model.ends_with('*') {
                push(&r.model);
            }
        }
        let models = self.inner.models.lock().unwrap();
        for u in live.settings.upstreams.iter().filter(|u| !u.disabled) {
            for m in u.models.iter().filter(|m| !m.ends_with('*')) {
                push(m);
            }
            if let Some(listed) = models.get(&u.id).and_then(|l| l.models.as_ref()) {
                for m in listed.iter().filter(|m| u.models.is_empty() || u.serves(m)) {
                    push(m);
                }
            }
        }
        out
    }
}

fn router(gw: Gateway) -> Router {
    Router::new()
        .route("/health", get(|| async { axum::Json(json!({ "ok": true })) }))
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/responses", post(responses))
        .route("/v1/models", get(models))
        .fallback(|| async { (StatusCode::NOT_FOUND, axum::Json(json!({ "error": { "type": "not_found_error", "message": "no such route" } }))) })
        .with_state(gw)
}

async fn messages(State(gw): State<Gateway>, headers: HeaderMap, body: Body) -> Response {
    generate(gw, WireFormat::Messages, headers, body).await
}

async fn chat(State(gw): State<Gateway>, headers: HeaderMap, body: Body) -> Response {
    generate(gw, WireFormat::Chat, headers, body).await
}

async fn responses(State(gw): State<Gateway>, headers: HeaderMap, body: Body) -> Response {
    generate(gw, WireFormat::Responses, headers, body).await
}

/// Anthropic's error type for an HTTP status.
fn kind_for(status: u16) -> &'static str {
    match status {
        400 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        503 | 529 => "overloaded_error",
        _ => "api_error",
    }
}

/// An error answer in the client's API's shape.
fn error(format: WireFormat, status: u16, message: &str) -> Response {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    (code, axum::Json(wire::error_body(format, status, kind_for(status), message))).into_response()
}

/// An error event that ends a client's stream.
fn error_event(format: WireFormat, message: &str) -> Bytes {
    let event = match format {
        WireFormat::Messages => wire::sse::Event::named("error", wire::error_body(format, 502, "api_error", message).to_string()),
        WireFormat::Chat => wire::sse::Event::data(wire::error_body(format, 502, "api_error", message).to_string()),
        WireFormat::Responses => wire::sse::Event::named("error", json!({ "type": "error", "code": "api_error", "message": message, "param": null }).to_string()),
    };
    Bytes::from(event.encode())
}

/// The client key a request carries.
fn client_key(headers: &HeaderMap) -> Option<String> {
    let value = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty());
    if let Some(a) = value("authorization")
        && let Some(k) = a.strip_prefix("Bearer ").or_else(|| a.strip_prefix("bearer "))
    {
        return Some(k.trim().to_string());
    }
    value("x-api-key").or_else(|| value("api-key")).map(str::to_string)
}

async fn authenticate(gw: &Gateway, headers: &HeaderMap, format: WireFormat) -> Result<ClientRef, Box<Response>> {
    let refused = || error(format, 401, "a roll gateway client key is needed (Authorization: Bearer rgk_…, or x-api-key)");
    let Some(key) = client_key(headers) else { return Err(Box::new(refused())) };
    let store = gw.inner.store.clone();
    match tokio::task::spawn_blocking(move || store.verify(&key, now_ms())).await {
        Ok(Ok(Some(client))) => Ok(client),
        Ok(Ok(None)) => Err(Box::new(refused())),
        Ok(Err(e)) => {
            tracing::warn!("gateway: checking a client key: {e:#}");
            Err(Box::new(error(format, 500, "the gateway couldn't check the key")))
        }
        Err(_) => Err(Box::new(error(format, 500, "the gateway couldn't check the key"))),
    }
}

/// `x-roll-thread` / `x-roll-project`: an id, or nothing.
fn roll_id(headers: &HeaderMap, name: &str) -> Option<String> {
    let v = headers.get(name)?.to_str().ok()?.trim();
    let ok = !v.is_empty() && v.len() <= ROLL_ID_MAX && v.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'));
    ok.then(|| v.to_string())
}

fn forward(headers: &HeaderMap) -> Forward {
    let clean = |n: &str, max: usize| {
        headers.get(n).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty() && v.len() <= max && v.bytes().all(|b| b.is_ascii_graphic() || b == b' ')).map(str::to_string)
    };
    Forward {
        user_agent: user_agent(headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok())),
        anthropic_version: clean("anthropic-version", 64),
        anthropic_beta: clean("anthropic-beta", 2048),
    }
}

/// Reads a request body up to [`MAX_BODY`] and parses it.
async fn read_body(body: Body, format: WireFormat) -> Result<(Bytes, Value), Box<Response>> {
    let bytes = axum::body::to_bytes(body, MAX_BODY).await.map_err(|e| {
        Box::new(if std::error::Error::source(&e).is_some_and(|s| s.to_string().contains("length limit")) || e.to_string().contains("length limit") {
            error(format, 413, &format!("the request body is over {} MiB", MAX_BODY >> 20))
        } else {
            error(format, 400, "the request body couldn't be read")
        })
    })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| Box::new(error(format, 400, "the request body isn't JSON")))?;
    if !value.is_object() {
        return Err(Box::new(error(format, 400, "the request body isn't a JSON object")));
    }
    Ok((bytes, value))
}

/// A body for the request log: one byte past [`BODY_CAP`] at most, so the store can tell a
/// cut body from one that fit.
fn clip(b: &[u8]) -> String {
    String::from_utf8_lossy(&b[..b.len().min(BODY_CAP + 1)]).into_owned()
}

/// A request on its way: what the record needs, and what each attempt needs.
struct Req {
    gw: Gateway,
    live: Arc<Live>,
    format: WireFormat,
    client: ClientRef,
    project: Option<String>,
    thread: Option<String>,
    model: String,
    stream: bool,
    body: Value,
    raw: Bytes,
    started: Instant,
    at: u64,
    attempts: Vec<Attempt>,
    session: Option<String>,
    fwd: Forward,
}

impl Req {
    fn record(&self, status: u16, usage: Usage, error: Option<String>, first_byte_ms: Option<u32>, response: &[u8]) {
        let bodies = self.live.settings.log_bodies.then(|| (clip(&self.raw), clip(response)));
        self.gw.record(Record {
            at: self.at,
            client: Some(self.client.clone()),
            project: self.project.clone(),
            thread: self.thread.clone(),
            format: self.format,
            model: self.model.clone(),
            stream: self.stream,
            status,
            ms: self.started.elapsed().as_millis() as u32,
            first_byte_ms,
            attempts: self.attempts.clone(),
            usage,
            error,
            bodies,
        });
    }

    fn ctx(&self) -> (Option<String>, &crate::net::Net) {
        (self.gw.inner.vertex_token_uri.lock().unwrap().clone(), &self.gw.inner.net)
    }

    /// Answers with an error of the gateway's own (nothing was tried, or nothing answered).
    fn fail(&self, status: u16, message: &str) -> Response {
        self.record(status, Usage::default(), Some(message.to_string()), None, b"");
        error(self.format, status, message)
    }
}

/// Why an attempt didn't serve.
struct Failure {
    /// The upstream's status (0: no usable answer).
    status: u16,
    error: String,
    rate_limited: bool,
    retry_after: Option<Duration>,
    /// The upstream's error body and the API it's in.
    body: Option<(WireFormat, Vec<u8>)>,
}

enum Step {
    Done(Response),
    Next(Failure),
}

async fn generate(gw: Gateway, format: WireFormat, headers: HeaderMap, body: Body) -> Response {
    let Ok(permit) = gw.inner.inflight.clone().try_acquire_owned() else {
        return error(format, 503, "the gateway is serving as many requests as it takes; try again shortly");
    };
    let client = match authenticate(&gw, &headers, format).await {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let (raw, body) = match read_body(body, format).await {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let Some(model) = wire::request_model(&body).map(str::to_string).filter(|m| !m.is_empty() && m.len() <= agent_proto::gateway_limits::MODEL) else {
        return error(format, 400, "the request names no model");
    };
    let live = gw.live();
    let session = if live.settings.affinity { route::session_id(&headers, &body).map(|s| format!("{}/{s}", client.id)) } else { None };
    let req = Req {
        stream: wire::is_stream(format, &body),
        gw: gw.clone(),
        live,
        format,
        client,
        project: roll_id(&headers, "x-roll-project"),
        thread: roll_id(&headers, "x-roll-thread"),
        model,
        body,
        raw,
        started: Instant::now(),
        at: now_ms(),
        attempts: Vec::new(),
        session,
        fwd: forward(&headers),
    };
    proxy(req, permit).await
}

async fn proxy(mut req: Req, permit: OwnedSemaphorePermit) -> Response {
    let gw = req.gw.clone();
    let live = req.live.clone();
    gw.ensure_lists(&live, false).await;
    let targets = route::targets(&live.settings, &req.model, |u, m| gw.lists(u, m));
    if targets.is_empty() {
        return req.fail(404, &format!("no upstream of this gateway serves {}", req.model));
    }
    let order = match gw.inner.health.order(targets, req.session.as_deref(), now_ms()) {
        Ok(o) => o,
        Err(Unavailable::Cooling { until }) => {
            let secs = until.saturating_sub(now_ms()).div_ceil(1000).max(1);
            let mut r = req.fail(429, &format!("every upstream for {} asked to wait (rate limited); try again in {secs} s", req.model));
            r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(secs));
            return r;
        }
        Err(Unavailable::Open) => return req.fail(503, &format!("every upstream for {} is failing; the gateway tries them again shortly", req.model)),
    };
    let mut permit = Some(permit);
    // A provider that answered 429 (now, or still cooling from before) is barred whole: never
    // another key of it while it asks to wait.
    let now = now_ms();
    let mut barred: Vec<(UpstreamSpec, u64)> =
        live.upstreams.values().filter_map(|u| gw.inner.health.cooling_until(&u.spec.id, now).map(|until| (u.spec.clone(), until))).collect();
    let mut waited: Option<u64> = None;
    let mut last: Option<Failure> = None;
    for t in order {
        let Some(up) = live.upstreams.get(&t.upstream).cloned() else { continue };
        if let Some(until) = barred.iter().filter(|(b, _)| route::same_provider(b, &up.spec)).map(|(_, u)| *u).min() {
            waited = Some(waited.map_or(until, |w| w.min(until)));
            continue;
        }
        if !gw.inner.health.admit(&t.upstream, now_ms()) {
            continue;
        }
        match attempt(&mut req, &up, &t, &mut permit).await {
            Step::Done(r) => return r,
            Step::Next(f) => {
                if f.rate_limited {
                    let until = gw.inner.health.cooling_until(&t.upstream, now_ms()).unwrap_or_else(now_ms);
                    barred.push((up.spec.clone(), until));
                }
                last = Some(f);
            }
        }
    }
    let Some(f) = last else {
        if let Some(until) = waited {
            let secs = until.saturating_sub(now_ms()).div_ceil(1000).max(1);
            let mut r = req.fail(429, &format!("the provider for {} asked to wait (rate limited); try again in {secs} s", req.model));
            r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(secs));
            return r;
        }
        return req.fail(503, &format!("no upstream for {} can take a request now", req.model));
    };
    if f.status == 0 {
        return req.fail(502, &format!("no upstream answered: {}", f.error));
    }
    // An upstream's own refusal of the gateway's key isn't the client's: a 502.
    let status = if matches!(f.status, 401 | 403) { 502 } else { f.status };
    let mut r = match &f.body {
        Some((fmt, b)) if *fmt == req.format && status == f.status => {
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            (code, [(header::CONTENT_TYPE, "application/json")], b.clone()).into_response()
        }
        _ => {
            let status = if status == 529 && req.format != WireFormat::Messages { 503 } else { status };
            error(req.format, status, &f.error)
        }
    };
    if let Some(d) = f.retry_after {
        r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(d.as_secs().max(1)));
    }
    req.record(r.status().as_u16(), Usage::default(), Some(f.error), None, f.body.as_ref().map(|(_, b)| b.as_slice()).unwrap_or_default());
    r
}

/// `retry-after-ms`, else `retry-after` in seconds.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let get = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite() && *v >= 0.0);
    get("retry-after-ms").map(|ms| Duration::from_millis(ms as u64)).or_else(|| get("retry-after").map(|s| Duration::from_millis((s * 1000.0) as u64)))
}

/// The message of an error body in any of the three shapes.
fn message_of(body: &[u8]) -> String {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let m = v.pointer("/error/message").or_else(|| v.get("message")).or_else(|| v.get("error")).and_then(Value::as_str);
    match m {
        Some(m) => m.chars().take(500).collect(),
        None => String::from_utf8_lossy(&body[..body.len().min(300)]).into_owned(),
    }
}

/// One upstream tried.
async fn attempt(req: &mut Req, up: &Arc<Upstream>, t: &Target, permit: &mut Option<OwnedSemaphorePermit>) -> Step {
    let gw = req.gw.clone();
    let upfmt = up.format_for(req.format, &t.model);
    let pass = upfmt == req.format;
    let started = Instant::now();
    let at = now_ms();
    let mut tried = |req: &mut Req, status: u16, error: Option<String>| {
        req.attempts.push(Attempt { upstream: t.upstream.clone(), model: t.model.clone(), status, ms: started.elapsed().as_millis() as u32, error });
    };
    let body = if pass {
        let mut b = req.body.clone();
        b["model"] = Value::from(t.model.as_str());
        b
    } else {
        match wire::translate_request(req.format, upfmt, req.body.clone(), &t.model) {
            Ok(b) => b,
            Err(wire::WireError::Invalid(e)) => {
                tried(req, 0, Some(e.clone()));
                return Step::Done(req.fail(400, &e));
            }
            Err(e) => {
                tried(req, 0, Some(e.to_string()));
                return Step::Next(Failure { status: 0, error: e.to_string(), rate_limited: false, retry_after: None, body: None });
            }
        }
    };
    let (token_uri, net) = req.ctx();
    let ctx = Ctx { net, now_ms: at, vertex_token_uri: token_uri.as_deref() };
    let sent = match up.generate(&ctx, upfmt, &t.model, body, req.stream, &req.fwd).await.and_then(|out| {
        let framing = out.framing;
        up.send(net, out).map(|r| (r, framing))
    }) {
        Ok(s) => s,
        Err(e) => {
            // The upstream's settings or key (no key, a private address, a token that can't
            // be had): nothing was sent, so the circuit isn't touched.
            tried(req, 0, Some(e.clone()));
            return Step::Next(Failure { status: 0, error: format!("{}: {e}", up.spec.name), rate_limited: false, retry_after: None, body: None });
        }
    };
    let (request, framing) = sent;
    let (first_byte_timeout, idle) = *gw.inner.timeouts.lock().unwrap();
    let resp = match tokio::time::timeout(first_byte_timeout, request.send()).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return failed(req, &gw, t, at, &mut tried, 0, format!("{}: {}", up.spec.name, error_chain(&e))),
        Err(_) => return failed(req, &gw, t, at, &mut tried, 0, format!("{}: no answer in {} ms", up.spec.name, first_byte_timeout.as_millis())),
    };
    let status = resp.status().as_u16();
    let headers_ms = started.elapsed().as_millis() as u32;
    if !resp.status().is_success() {
        let wait = retry_after(resp.headers());
        let body = read_capped(resp, MAX_ERROR_BODY).await.unwrap_or_default();
        let why = format!("{}: {status}: {}", up.spec.name, message_of(&body));
        let failure = Failure { status, error: why.clone(), rate_limited: status == 429, retry_after: wait, body: Some((upfmt, body)) };
        if status == 429 {
            gw.inner.health.rate_limited(&t.upstream, wait, now_ms());
            gw.note_attempt(&t.upstream, at, None);
            tried(req, status, Some(why.clone()));
            return Step::Next(failure);
        }
        if status >= 500 || matches!(status, 401 | 403) {
            gw.inner.health.failure(&t.upstream, &why, now_ms());
            gw.note_attempt(&t.upstream, at, None);
            tried(req, status, Some(why.clone()));
            return Step::Next(failure);
        }
        gw.inner.health.success(&t.upstream, now_ms());
        gw.note_attempt(&t.upstream, at, Some(headers_ms));
        tried(req, status, Some(why.clone()));
        if status == 404 {
            // That upstream doesn't have the model after all: another may.
            return Step::Next(failure);
        }
        // The request itself is wrong: the client hears it, in its own API's shape.
        let r = match failure.body {
            Some((fmt, b)) if fmt == req.format => {
                let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
                let r = (code, [(header::CONTENT_TYPE, "application/json")], b.clone()).into_response();
                req.record(status, Usage::default(), Some(failure.error), None, &b);
                r
            }
            _ => {
                req.record(status, Usage::default(), Some(failure.error.clone()), None, b"");
                error(req.format, status, &failure.error)
            }
        };
        return Step::Done(r);
    }
    if !req.stream {
        let body = match read_capped_idle(resp, MAX_RESPONSE, idle).await {
            Ok(b) => b,
            Err(e) => return failed(req, &gw, t, at, &mut tried, 0, format!("{}: {e}", up.spec.name)),
        };
        gw.inner.health.success(&t.upstream, now_ms());
        gw.note_attempt(&t.upstream, at, Some(headers_ms));
        tried(req, status, None);
        bind(req, &t.upstream);
        let parsed: Option<Value> = serde_json::from_slice(&body).ok();
        let usage = parsed.as_ref().map(|v| wire::usage_of(upfmt, v)).unwrap_or_default();
        let out = if pass {
            body
        } else {
            match parsed.map(|v| wire::translate_response(upfmt, req.format, v, &req.model)) {
                Some(Ok(v)) => serde_json::to_vec(&v).unwrap_or_default(),
                Some(Err(e)) => return Step::Done(req.fail(502, &format!("{}'s answer couldn't be translated: {e}", up.spec.name))),
                None => return Step::Done(req.fail(502, &format!("{}'s answer isn't JSON", up.spec.name))),
            }
        };
        let first_byte = req.started.elapsed().as_millis() as u32;
        req.record(status, usage, None, Some(first_byte), &out);
        return Step::Done((StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], out).into_response());
    }
    // A stream: read until its first event, which decides whether this upstream serves.
    let mut reader = Reader::new(resp, framing, idle);
    let mut mode = if pass { Mode::Pass(UsageTap::new(upfmt)) } else { Mode::Translate(Box::new(StreamTranslator::new(upfmt, req.format, &req.model))) };
    let mut first: Vec<Bytes> = Vec::new();
    loop {
        let ev = match reader.next().await {
            Ok(Some(ev)) => ev,
            Ok(None) => return failed(req, &gw, t, at, &mut tried, 0, format!("{}: the stream ended before its first event", up.spec.name)),
            Err(e) => return failed(req, &gw, t, at, &mut tried, 0, format!("{}: {e}", up.spec.name)),
        };
        if let Some(e) = &ev.event {
            match stream::early(e) {
                Early::Fine => {}
                Early::Failed(m) => return failed(req, &gw, t, at, &mut tried, 0, format!("{}: {m}", up.spec.name)),
                Early::RateLimited(m) => {
                    gw.inner.health.rate_limited(&t.upstream, None, now_ms());
                    gw.note_attempt(&t.upstream, at, None);
                    let error = format!("{}: {m}", up.spec.name);
                    tried(req, 429, Some(error.clone()));
                    return Step::Next(Failure { status: 429, error, rate_limited: true, retry_after: None, body: None });
                }
            }
        }
        match mode.push(&ev) {
            Ok(out) => first.extend(out),
            Err(e) => {
                tried(req, status, Some(e.to_string()));
                return Step::Next(Failure { status: 0, error: format!("{}'s stream couldn't be translated: {e}", up.spec.name), rate_limited: false, retry_after: None, body: None });
            }
        }
        if ev.event.is_some() {
            break;
        }
    }
    gw.inner.health.success(&t.upstream, now_ms());
    gw.note_attempt(&t.upstream, at, Some(headers_ms));
    tried(req, status, None);
    bind(req, &t.upstream);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    let permit = permit.take();
    let req = std::mem::replace(req, placeholder(req));
    tokio::spawn(pump(req, reader, mode, first, tx, permit));
    let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|x| (x, rx)) }));
    let mut r = Response::new(body);
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Step::Done(r)
}

/// An attempt that failed as an outage (no answer, a timeout, 5xx, a broken stream).
fn failed(req: &mut Req, gw: &Gateway, t: &Target, at: u64, tried: &mut impl FnMut(&mut Req, u16, Option<String>), status: u16, error: String) -> Step {
    gw.inner.health.failure(&t.upstream, &error, now_ms());
    gw.note_attempt(&t.upstream, at, None);
    tried(req, status, Some(error.clone()));
    Step::Next(Failure { status, error, rate_limited: false, retry_after: None, body: None })
}

fn bind(req: &Req, upstream: &str) {
    if let Some(s) = &req.session {
        req.gw.inner.health.bind(s, upstream, now_ms());
    }
}

/// What's left of a request whose stream moved to its pump.
fn placeholder(req: &Req) -> Req {
    Req {
        gw: req.gw.clone(),
        live: req.live.clone(),
        format: req.format,
        client: req.client.clone(),
        project: None,
        thread: None,
        model: String::new(),
        stream: req.stream,
        body: Value::Null,
        raw: Bytes::new(),
        started: req.started,
        at: req.at,
        attempts: Vec::new(),
        session: None,
        fwd: Forward::default(),
    }
}

/// An upstream's stream, event by event.
struct Reader {
    resp: reqwest::Response,
    /// The longest wait for the next chunk.
    idle: Duration,
    frames: Frames,
    queue: std::collections::VecDeque<RawEvent>,
    done: bool,
}

enum Frames {
    Sse(SseFrames),
    Bedrock(BedrockFrames),
}

impl Reader {
    fn new(resp: reqwest::Response, framing: Framing, idle: Duration) -> Reader {
        let frames = match framing {
            Framing::Http => Frames::Sse(SseFrames::new()),
            Framing::EventStream => Frames::Bedrock(BedrockFrames::new()),
        };
        Reader { resp, idle, frames, queue: Default::default(), done: false }
    }

    /// The next event (`None`: the stream ended).
    async fn next(&mut self) -> Result<Option<RawEvent>, String> {
        loop {
            if let Some(e) = self.queue.pop_front() {
                return Ok(Some(e));
            }
            if self.done {
                return Ok(None);
            }
            let chunk = tokio::time::timeout(self.idle, self.resp.chunk()).await.map_err(|_| format!("the stream stopped for {} s", self.idle.as_secs()))?;
            match chunk {
                Ok(Some(c)) => {
                    let events = match &mut self.frames {
                        Frames::Sse(f) => f.feed(&c),
                        Frames::Bedrock(f) => f.feed(&c),
                    };
                    self.queue.extend(events.map_err(|e| e.to_string())?);
                }
                Ok(None) => {
                    self.done = true;
                    if let Frames::Sse(f) = &mut self.frames {
                        self.queue.extend(f.finish());
                    }
                }
                Err(e) => return Err(format!("the stream broke: {}", error_chain(&e))),
            }
        }
    }
}

/// How a stream reaches the client.
enum Mode {
    /// Byte for byte (usage read on the way).
    Pass(UsageTap),
    Translate(Box<StreamTranslator>),
}

impl Mode {
    fn push(&mut self, ev: &RawEvent) -> Result<Vec<Bytes>, wire::WireError> {
        match self {
            Mode::Pass(tap) => {
                if let Some(e) = &ev.event {
                    tap.push(e);
                }
                Ok(vec![ev.raw.clone()])
            }
            Mode::Translate(tr) => match &ev.event {
                Some(e) => Ok(tr.push(e)?.into_iter().map(|e| Bytes::from(e.encode())).collect()),
                None => Ok(Vec::new()),
            },
        }
    }

    fn finish(&mut self) -> Vec<Bytes> {
        match self {
            Mode::Pass(_) => Vec::new(),
            Mode::Translate(tr) => tr.finish().into_iter().map(|e| Bytes::from(e.encode())).collect(),
        }
    }

    fn usage(&self) -> Usage {
        match self {
            Mode::Pass(tap) => tap.usage(),
            Mode::Translate(tr) => tr.usage(),
        }
    }
}

/// Moves a committed stream to the client, then records it.
async fn pump(
    req: Req,
    mut reader: Reader,
    mut mode: Mode,
    first: Vec<Bytes>,
    tx: tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    permit: Option<OwnedSemaphorePermit>,
) {
    let _permit = permit;
    let first_byte = req.started.elapsed().as_millis() as u32;
    let mut logged: Vec<u8> = Vec::new();
    let log = req.live.settings.log_bodies;
    let mut error: Option<String> = None;
    let mut gone = false;
    if !send_all(&tx, first, log, &mut logged).await {
        gone = true;
    }
    while !gone {
        let next = tokio::select! {
            n = reader.next() => n,
            _ = tx.closed() => {
                gone = true;
                break;
            }
        };
        let out = match next {
            Ok(Some(ev)) => match mode.push(&ev) {
                Ok(out) => out,
                Err(e) => {
                    error = Some(format!("the stream couldn't be translated: {e}"));
                    break;
                }
            },
            Ok(None) => {
                let out = mode.finish();
                gone = !send_all(&tx, out, log, &mut logged).await;
                break;
            }
            Err(e) => {
                error = Some(e);
                break;
            }
        };
        gone = !send_all(&tx, out, log, &mut logged).await;
    }
    if let Some(e) = &error
        && !gone
    {
        gone = !send_all(&tx, vec![error_event(req.format, e)], log, &mut logged).await;
    }
    if gone && error.is_none() {
        error = Some("the client closed the stream".into());
    }
    req.record(200, mode.usage(), error, Some(first_byte), &logged);
}

/// Sends `chunks` to the client (keeping the start for the request log when `log`); `false`:
/// the client is gone.
async fn send_all(tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>, chunks: Vec<Bytes>, log: bool, logged: &mut Vec<u8>) -> bool {
    for b in chunks {
        if log {
            keep_body(logged, &b);
        }
        if tx.send(Ok(b)).await.is_err() {
            return false;
        }
    }
    true
}

async fn count_tokens(State(gw): State<Gateway>, headers: HeaderMap, body: Body) -> Response {
    let format = WireFormat::Messages;
    let Ok(_permit) = gw.inner.inflight.clone().try_acquire_owned() else {
        return error(format, 503, "the gateway is serving as many requests as it takes; try again shortly");
    };
    if let Err(r) = authenticate(&gw, &headers, format).await {
        return *r;
    }
    let (raw, body) = match read_body(body, format).await {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let model = wire::request_model(&body).unwrap_or_default().to_string();
    let live = gw.live();
    gw.ensure_lists(&live, false).await;
    let targets = route::targets(&live.settings, &model, |u, m| gw.lists(u, m));
    let fwd = forward(&headers);
    let token_uri = gw.inner.vertex_token_uri.lock().unwrap().clone();
    for t in gw.inner.health.order(targets, None, now_ms()).unwrap_or_default() {
        let Some(up) = live.upstreams.get(&t.upstream).filter(|u| u.counts_tokens()) else { continue };
        let mut b = body.clone();
        b["model"] = Value::from(t.model.as_str());
        let ctx = Ctx { net: &gw.inner.net, now_ms: now_ms(), vertex_token_uri: token_uri.as_deref() };
        let Ok(request) = up.count_tokens(&ctx, b, &fwd).await.and_then(|out| up.send(&gw.inner.net, out)) else { continue };
        let Ok(Ok(resp)) = tokio::time::timeout(Duration::from_secs(60), request.send()).await else { continue };
        if resp.status().is_success()
            && let Ok(answer) = read_capped(resp, MAX_ERROR_BODY).await
        {
            return (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], answer).into_response();
        }
    }
    // No Anthropic upstream to ask: an estimate (about four bytes a token).
    axum::Json(json!({ "input_tokens": raw.len() / 4 })).into_response()
}

async fn models(State(gw): State<Gateway>, headers: HeaderMap) -> Response {
    let format = if headers.contains_key("anthropic-version") { WireFormat::Messages } else { WireFormat::Chat };
    if let Err(r) = authenticate(&gw, &headers, format).await {
        return *r;
    }
    let live = gw.live();
    gw.ensure_lists(&live, true).await;
    let data: Vec<Value> = gw
        .offered(&live)
        .into_iter()
        .map(|id| json!({ "id": id, "object": "model", "type": "model", "created": 0, "created_at": "1970-01-01T00:00:00Z", "owned_by": "roll-gateway", "display_name": id }))
        .collect();
    let first = data.first().and_then(|m| m.get("id")).cloned().unwrap_or(Value::Null);
    let last = data.last().and_then(|m| m.get("id")).cloned().unwrap_or(Value::Null);
    axum::Json(json!({ "object": "list", "data": data, "has_more": false, "first_id": first, "last_id": last })).into_response()
}
