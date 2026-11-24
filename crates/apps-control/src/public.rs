//! Public traffic (docs/apps/http-api.md, "Public traffic"): requests for a service's hostname,
//! custom domain or local port, handed to the service's owner and through the waking proxy to
//! an instance.

use crate::live::WakeError;
use crate::{Apps, Error, lease_key};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use hyper_util::rt::TokioIo;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Every request on the apps listener: routed by `Host`.
pub fn router(apps: Arc<Apps>) -> axum::Router {
    axum::Router::new().fallback(move |mut req: Request| {
        let apps = apps.clone();
        async move {
            strip_ours(&mut req);
            // Traces from instances (`traces: true`), on this listener only: what reaches it is
            // what the internet reaches, never the API.
            if req.uri().path().starts_with(OTLP) {
                return otlp(apps, req).await;
            }
            // An environment's runtime config, read by its instances with its token.
            if req.uri().path().starts_with(CONFIG) {
                return runtime_config(apps, req).await;
            }
            // Roll's OIDC tokens for an environment's instances, with its token.
            if req.uri().path().starts_with(OIDC) {
                return oidc_token(apps, req).await;
            }
            // An instance kept awake for work after its answer, with the same token.
            if req.uri().path().starts_with(AWAKE) {
                return hold_awake(apps, req).await;
            }
            // An environment's deploy status badge, where it has one.
            if req.uri().path().starts_with(BADGE) {
                return badge(apps, req).await;
            }
            // Builds' remote cache (Turborepo, Nx), with their environment's token.
            if req.uri().path().starts_with(crate::build_cache::PATH) && req.uri().path()[crate::build_cache::PATH.len()..].starts_with('/') {
                return crate::build_cache::serve(apps, req).await;
            }
            // Next.js's cache across instances, with the environment's token.
            if req.uri().path().starts_with(crate::next_cache::PATH) && req.uri().path()[crate::next_cache::PATH.len()..].starts_with('/') {
                return crate::next_cache::serve(apps, req).await;
            }
            // An edge function's `context.next()` ([`crate::edge`]): signed, any host.
            if req.uri().path().starts_with(crate::edge::NEXT) {
                return crate::edge::next(apps, req).await;
            }
            let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("").to_string();
            // A project's status page ([`crate::status_page`]), on its own name.
            if host.starts_with("status-")
                && let Some(page) = apps.status_page_for_host(&host).await
            {
                return crate::status_page::serve(apps, page, req.uri().path()).await;
            }
            // A domain that redirects (www → apex): a 308 to the same path and query there.
            if let Some(to) = apps.redirect_for_host(&host).await {
                let pq = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
                let scheme = if apps.cfg.scheme == "https" { "https" } else { "http" };
                let mut r = StatusCode::PERMANENT_REDIRECT.into_response();
                if let Ok(l) = HeaderValue::from_str(&format!("{scheme}://{to}{pq}")) {
                    r.headers_mut().insert(header::LOCATION, l);
                }
                return r;
            }
            match apps.env_for_host(&host).await {
                // Image optimization on the environment's own host (`/__roll/image`).
                Some((service, env, None, _)) if req.uri().path() == crate::images::PATH => crate::images::serve(apps, service, env, req).await,
                Some((service, env, deployment, port)) => {
                    let mut req = req;
                    if let Some(p) = port {
                        req.extensions_mut().insert(TargetPort(p));
                    }
                    serve(apps, service, env, deployment, req).await
                }
                // A name under a project's preview domain no environment holds: roll previews'
                // (a preview on roll's own runtime), with the request as it came.
                None => match apps.preview_router_for(&host).await {
                    Some(router) => router.serve(req).await,
                    None => page(StatusCode::NOT_FOUND, "Nothing here", "No roll apps service answers on this address."),
                },
            }
        }
    })
}

/// The app's port a custom domain sends its requests to (`Domain::port`), when it isn't the
/// service's own.
#[derive(Clone, Copy)]
struct TargetPort(u16);

/// Where instances send traces: `/_roll/otlp/{service}/{env}/{token}/v1/traces`, any host.
pub const OTLP: &str = "/_roll/otlp/";

/// Where an environment's instances ask to be kept awake (`ROLL_AWAKE_URL`):
/// `/_roll/awake/<service>/<env>/<token>`, the token the OIDC address's.
pub const AWAKE: &str = "/_roll/awake/";

/// `POST …/_roll/awake/<service>/<env>/<token>?instance=<ROLL_INSTANCE>&for=<s>` (Vercel's
/// `waitUntil`): the instance isn't suspended, put to sleep or scaled in for `for` seconds (1–900,
/// default 60; a longer hold it has stays), so work after its answer (an email, a webhook, a
/// telemetry flush) finishes. On the service's owner, where its instances are. `{held_until}`
/// (unix ms); a wrong token, or an instance that isn't awake, a 404.
async fn hold_awake(apps: Arc<Apps>, req: Request) -> Response {
    let parts: Vec<String> = req.uri().path()[AWAKE.len()..].split('/').map(|s| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()).collect();
    let [id, env, token] = parts.as_slice() else { return (StatusCode::NOT_FOUND, "").into_response() };
    if req.method() != axum::http::Method::POST || !id.starts_with("svc_") || !token.starts_with("rlr_") {
        return (StatusCode::NOT_FOUND, "").into_response();
    }
    let (sid, e, t) = (id.clone(), env.clone(), token.clone());
    let ok = apps.db(move |s| Ok(s.service(&sid).is_ok() && s.oidc_token_of(&sid, &e)?.is_some_and(|k| same(&k, &t)))).await.unwrap_or(false);
    if !ok {
        return (StatusCode::NOT_FOUND, "").into_response();
    }
    let q: Vec<(String, String)> = req.uri().query().map(|q| url::form_urlencoded::parse(q.as_bytes()).into_owned().collect()).unwrap_or_default();
    let arg = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let Some(instance) = arg("instance") else { return (StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({ "error": "instance is the instance's name ($ROLL_INSTANCE)" }))).into_response() };
    let secs = match arg("for").map(|v| v.parse::<u64>()) {
        None => 60,
        Some(Ok(n)) if (1..=900).contains(&n) => n,
        _ => return (StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({ "error": "for is 1–900 seconds" }))).into_response(),
    };
    let (cluster, key, svc, env) = (apps.deps.cluster.clone(), lease_key(id), id.clone(), env.clone());
    cluster
        .dispatch(&key, req, |_req, _guard| async move {
            let Some(live) = apps.lives.lock().get(&crate::env_key(&svc, &env)).cloned() else { return (StatusCode::NOT_FOUND, "").into_response() };
            match live.hold_awake(&instance, std::time::Duration::from_secs(secs)) {
                Some(until) => {
                    let ms = code_sql::now_ms() + until.saturating_duration_since(std::time::Instant::now()).as_millis() as i64;
                    (StatusCode::OK, axum::Json(serde_json::json!({ "held_until": ms }))).into_response()
                }
                None => (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({ "error": format!("{instance} isn't one of {env}'s awake instances") }))).into_response(),
            }
        })
        .await
}

/// An environment's deploy status badge (Netlify's): `/_roll/badge/<service>/<env>.svg`.
pub const BADGE: &str = "/_roll/badge/";

/// `GET …/_roll/badge/<service>/<env>.svg`: what the environment's deploys stand at, as an SVG
/// for a README (live, deploying, waiting, failed, paused, not deployed), cached a minute. Only
/// where its `badge` is on; anything else is a 404, as a path that isn't one.
async fn badge(apps: Arc<Apps>, req: Request) -> Response {
    let rest = req.uri().path()[BADGE.len()..].to_string();
    let Some((id, env)) = rest.strip_suffix(".svg").and_then(|r| r.split_once('/')) else { return (StatusCode::NOT_FOUND, "").into_response() };
    if req.method() != axum::http::Method::GET || !id.starts_with("svc_") || env.contains('/') {
        return (StatusCode::NOT_FOUND, "").into_response();
    }
    let (sid, e) = (id.to_string(), env.to_string());
    let found = apps
        .db(move |s| {
            let Ok(svc) = s.service(&sid) else { return Ok(None) };
            let Ok(row) = s.environment(&sid, &e) else { return Ok(None) };
            if !row.badge {
                return Ok(None);
            }
            let latest = s.deployments(&sid, &e, 1, None)?.into_iter().next();
            Ok(Some((svc.name, row, latest)))
        })
        .await;
    let Ok(Some((name, row, latest))) = found else { return (StatusCode::NOT_FOUND, "").into_response() };
    use crate::model::DeployStatus as S;
    let (said, color) = match (row.paused, latest.as_ref().map(|d| (d.status, row.active.as_deref() == Some(d.id.as_str())))) {
        (true, _) => ("paused", "#6b7280"),
        (_, Some((S::Queued | S::Building | S::Deploying, _))) => ("deploying", "#4c8dff"),
        (_, Some((S::Ready, _))) => ("waiting", "#4c8dff"),
        (_, Some((S::Failed, _))) => ("failed", "#e5484d"),
        _ if row.active.is_some() => ("live", "#2fa86b"),
        _ => ("not deployed", "#6b7280"),
    };
    let svg = badge_svg(&format!("{name} {}", row.name), said, color);
    ([(header::CONTENT_TYPE, "image/svg+xml; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=60")], svg).into_response()
}

/// A flat two-part badge, label and status, widths by a rough character measure (Verdana 11).
fn badge_svg(label: &str, status: &str, color: &str) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    let width = |s: &str| 10 + s.chars().count() as u32 * 7;
    let (lw, sw) = (width(label), width(status));
    let (label, status) = (esc(label), esc(status));
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="20" role="img" aria-label="{label}: {status}"><title>{label}: {status}</title><rect width="{lw}" height="20" fill="#24292e"/><rect x="{lw}" width="{sw}" height="20" fill="{color}"/><g fill="#fff" font-family="Verdana,Geneva,DejaVu Sans,sans-serif" font-size="11" text-anchor="middle"><text x="{lx}" y="14">{label}</text><text x="{sx}" y="14">{status}</text></g></svg>"##,
        w = lw + sw,
        lx = lw / 2,
        sx = lw + sw / 2,
    )
}

/// Where an environment's instances read roll's OIDC tokens (`ROLL_OIDC_URL`):
/// `/_roll/oidc/<service>/<env>/<token>`.
pub const OIDC: &str = "/_roll/oidc/";

/// How long a signed token is answered again (ms): instances that ask before every call cost a
/// signature a minute, and a key rotation, a project that stops standing for the service or an
/// environment that turns untrusted takes effect within that minute.
const OIDC_KEEP_MS: i64 = 60 * 1000;
/// A token's life.
const OIDC_TTL_MS: i64 = 3600 * 1000;

/// `GET …/_roll/oidc/<service>/<env>/<token>[?aud=]`: `{token, expires_at}`, roll's OIDC token
/// for the environment's instances (`sub = roll-apps:<project>:<service>:<env>`) for `aud`, one
/// of `run.oidc_audiences` (the first by default), from roll secrets' issuer, an hour long. One is
/// answered again for a minute ([`OIDC_KEEP_MS`]), so instances that ask often don't each cost a
/// signature. A wrong token is a 404; an audience not in the list a 403; a project that doesn't
/// stand for the service, or an untrusted preview, a 403 saying why; no issuer here a 503.
async fn oidc_token(apps: Arc<Apps>, req: Request) -> Response {
    let parts: Vec<String> = req.uri().path()[OIDC.len()..].split('/').map(|s| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()).collect();
    let [id, env, token] = parts.as_slice() else { return (StatusCode::NOT_FOUND, "").into_response() };
    if req.method() != axum::http::Method::GET || !id.starts_with("svc_") || !token.starts_with("rlr_") {
        return (StatusCode::NOT_FOUND, "").into_response();
    }
    let (sid, e, t) = (id.clone(), env.clone(), token.clone());
    let found = apps
        .db(move |s| {
            let Ok(svc) = s.service(&sid) else { return Ok(None) };
            if !s.oidc_token_of(&sid, &e)?.is_some_and(|k| same(&k, &t)) {
                return Ok(None);
            }
            let row = s.environment(&sid, &e)?;
            Ok(Some((svc.project.clone(), svc.for_env(&row)?.run.oidc_audiences)))
        })
        .await;
    let Ok(Some((project, audiences))) = found else { return (StatusCode::NOT_FOUND, "").into_response() };
    let asked = req.uri().query().and_then(|q| url::form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == "aud").map(|(_, v)| v.into_owned()));
    let Some(aud) = asked.or_else(|| audiences.first().cloned()) else {
        return (StatusCode::FORBIDDEN, axum::Json(serde_json::json!({ "error": "this service names no audience (run.oidc_audiences)" }))).into_response();
    };
    if !audiences.contains(&aud) {
        return (StatusCode::FORBIDDEN, axum::Json(serde_json::json!({ "error": format!("{aud} isn't one of run.oidc_audiences") }))).into_response();
    }
    // An environment's URL is the audience of roll's own pushes and scheduled calls to it: no
    // instance gets one, or any service could sign a call to another's.
    if apps.roll_audience(&aud) {
        return (StatusCode::FORBIDDEN, axum::Json(serde_json::json!({ "error": format!("{aud} is on roll's apps domain: its pushes and scheduled calls are signed for it, and no instance may be") }))).into_response();
    }
    let (jwt, until) = match apps.service_token(&project, id, env, &aud).await {
        Ok(Some(t)) => t,
        Ok(None) => return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(serde_json::json!({ "error": "roll's OIDC issuer isn't on this deployment (it needs a public URL)" }))).into_response(),
        Err(e) => return (StatusCode::FORBIDDEN, axum::Json(serde_json::json!({ "error": format!("{e:#}") }))).into_response(),
    };
    (StatusCode::OK, [(header::CACHE_CONTROL, "no-store")], axum::Json(serde_json::json!({ "token": jwt, "expires_at": until }))).into_response()
}

impl Apps {
    /// Roll's OIDC token for `service` in `env` (`sub = roll-apps:<project>:<service>:<env>`) for
    /// `aud`, and when it expires (unix ms): the same one for a minute ([`OIDC_KEEP_MS`]), then signed
    /// again (so a rotation or a lost trust takes effect). None: no issuer here.
    /// Whether `aud` names a URL on the apps domain (roll's pushes' and schedules' audiences).
    pub(crate) fn roll_audience(&self, aud: &str) -> bool {
        let domain = self.cfg.domain.to_ascii_lowercase();
        url::Url::parse(aud).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase)).is_some_and(|h| h == domain || h.ends_with(&format!(".{domain}")))
    }

    pub(crate) async fn service_token(&self, project: &str, service: &str, env: &str, aud: &str) -> anyhow::Result<Option<(String, i64)>> {
        let key = format!("{service}/{env}/{aud}");
        let now = code_sql::now_ms();
        if let Some(k) = self.oidc_tokens.lock().get(&key).cloned().filter(|(_, until)| until - now > OIDC_TTL_MS - OIDC_KEEP_MS) {
            return Ok(Some(k));
        }
        let Some(jwt) = self.deps.secrets.issue(project, service, env, aud, std::time::Duration::from_millis(OIDC_TTL_MS as u64)).await? else { return Ok(None) };
        let until = now + OIDC_TTL_MS;
        self.oidc_tokens.lock().insert(key, (jwt.clone(), until));
        Ok(Some((jwt, until)))
    }
}

/// Where an environment's instances read its runtime config (`ROLL_CONFIG_URL`):
/// `/_roll/config/<service>/<env>/<token>`.
pub const CONFIG: &str = "/_roll/config/";

/// `GET …/_roll/config/<service>/<env>/<token>[?wait=S]`: `{version, config}` with the version as
/// its ETag; `If-None-Match` the version it has answers 304, or with `wait` (at most 25 s) waits
/// for the next version first. Read from the store, so any node answers; a wrong token or none
/// is a 404, as a path that isn't one.
async fn runtime_config(apps: Arc<Apps>, req: Request) -> Response {
    let parts: Vec<String> = req.uri().path()[CONFIG.len()..].split('/').map(|s| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()).collect();
    let [id, env, token] = parts.as_slice() else { return (StatusCode::NOT_FOUND, "").into_response() };
    // `rlr_` (apps' own secrets), or `rlc_` from before them (still on running instances).
    if req.method() != axum::http::Method::GET || !id.starts_with("svc_") || !(token.starts_with("rlr_") || token.starts_with("rlc_")) {
        return (StatusCode::NOT_FOUND, "").into_response();
    }
    // A deleted service's (kept for a restore) answers nothing, whatever its token.
    let (sid, e, t) = (id.clone(), env.clone(), token.clone());
    let reads = move |s: &crate::store::Store| Ok(s.service(&sid).is_ok() && s.runtime_config_tokens(&sid, &e)?.is_some_and(|(now, before)| same(&now, &t) || before.is_some_and(|b| same(&b, &t))));
    if !apps.db(reads).await.unwrap_or(false) {
        return (StatusCode::NOT_FOUND, "").into_response();
    }
    let has = req.headers().get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).map(|v| v.trim_start_matches("W/").trim_matches('"').to_string());
    let wait = req.uri().query().and_then(|q| url::form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == "wait")).and_then(|(_, v)| v.parse::<u64>().ok()).unwrap_or(0).min(25);
    // At most CONFIG_WAITERS long polls an environment on a node; past that, an answer at once.
    let key = crate::env_key(id, env);
    let waiting = wait > 0 && {
        let mut w = apps.config_waiters.lock();
        let n = w.entry(key.clone()).or_default();
        (*n < CONFIG_WAITERS).then(|| *n += 1).is_some()
    };
    let _left = Waiting(waiting.then(|| (apps.clone(), key)));
    let until = Instant::now() + std::time::Duration::from_secs(if waiting { wait } else { 0 });
    loop {
        let (sid, e) = (id.clone(), env.clone());
        let Ok(Some(version)) = apps.db(move |s| s.runtime_config_version(&sid, &e)).await else { return (StatusCode::NOT_FOUND, "").into_response() };
        let tag = format!("\"{version}\"");
        if has.as_deref() != Some(version.to_string().as_str()) {
            let (sid, e) = (id.clone(), env.clone());
            let Ok(Some((config, version))) = apps.db(move |s| s.runtime_config(&sid, &e)).await else { return (StatusCode::NOT_FOUND, "").into_response() };
            return (StatusCode::OK, [(header::ETAG, format!("\"{version}\"")), (header::CACHE_CONTROL, "no-store".to_string())], axum::Json(serde_json::json!({ "version": version, "config": config }))).into_response();
        }
        if Instant::now() >= until {
            return (StatusCode::NOT_MODIFIED, [(header::ETAG, tag)]).into_response();
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// Long polls of one environment's runtime config a node holds at once.
const CONFIG_WAITERS: usize = 64;

/// A long poll's place, given back when it answers.
struct Waiting(Option<(Arc<Apps>, String)>);

impl Drop for Waiting {
    fn drop(&mut self) {
        if let Some((apps, key)) = self.0.take() {
            let mut w = apps.config_waiters.lock();
            if let Some(n) = w.get_mut(&key) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    w.remove(&key);
                }
            }
        }
    }
}

/// OTLP/HTTP from an environment's instances, on the service's owner: the token in the path is
/// the credential (anything else is the same 404). Answered as OTLP expects: an empty export
/// response.
async fn otlp(apps: Arc<Apps>, req: Request) -> Response {
    let parts: Vec<String> = req.uri().path()[OTLP.len()..].split('/').map(|s| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()).collect();
    let [id, env, token, v1, signal] = parts.as_slice() else { return (StatusCode::NOT_FOUND, "").into_response() };
    let Some(signal) = crate::traces::Signal::of(signal) else { return (StatusCode::NOT_FOUND, "").into_response() };
    if req.method() != axum::http::Method::POST || v1 != "v1" || !id.starts_with("svc_") {
        return (StatusCode::NOT_FOUND, "").into_response();
    }
    let (id, env, token) = (id.clone(), env.clone(), token.clone());
    if !apps.otlp_token_ok(&id, &env, &token).await {
        return (StatusCode::NOT_FOUND, "").into_response();
    }
    let cluster = apps.deps.cluster.clone();
    let key = lease_key(&id);
    cluster
        .dispatch(&key, req, |req, _guard| async move {
            let json = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|c| c.starts_with("application/json"));
            let gzip = req.headers().get(header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()).is_some_and(|c| c.eq_ignore_ascii_case("gzip"));
            let body = match axum::body::to_bytes(req.into_body(), 8 << 20).await {
                Ok(b) => b,
                Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "an export is at most 8 MiB").into_response(),
            };
            match apps.ingest_otlp(&id, &env, &token, signal, json, gzip, &body).await {
                Ok(_) if json => (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], "{}").into_response(),
                Ok(_) => (StatusCode::OK, [(header::CONTENT_TYPE, "application/x-protobuf")], bytes::Bytes::new()).into_response(),
                Err(e) => e.into_response(),
            }
        })
        .await
}

/// A listener that's one environment's own port (local development without wildcard DNS).
pub fn port_router(apps: Arc<Apps>, service: String, env: String) -> axum::Router {
    axum::Router::new().fallback(move |mut req: Request| {
        let (apps, service, env) = (apps.clone(), service.clone(), env.clone());
        async move {
            strip_ours(&mut req);
            serve(apps, service, env, None, req).await
        }
    })
}

/// The edge function a request goes to first (its target service, the function and its `next`
/// address), if one runs on its path as the app would read it; never on the way back from one.
async fn edge_first(apps: &Arc<Apps>, service: &str, env: &str, t: &crate::live::Template, req: EdgeProbe) -> Option<(String, String, String)> {
    if t.edge.is_empty() || req.back {
        return None;
    }
    apps.edge_for(service, env, t, &req.path, req.client.as_deref()).await
}

/// What [`edge_first`] reads of a request.
struct EdgeProbe {
    back: bool,
    path: String,
    client: Option<String>,
}

impl EdgeProbe {
    fn of(req: &Request) -> EdgeProbe {
        EdgeProbe { back: req.headers().contains_key(VIA_EDGE), path: crate::model::normal_path(req.uri().path()), client: roll_cluster::client_addr(req) }
    }
}

/// roll's own request headers, off a visitor's request where it comes in: from here on, one is
/// roll's ([`VIA_SITE`] lets a rewrite past its target's access check).
fn strip_ours(req: &mut Request) {
    for h in [VIA_SITE, VIA_EDGE, MIRROR, TARGET, ORIGINAL] {
        req.headers_mut().remove(h);
    }
}

/// Run the request where the service lives (`deployment`: one deployment's own URL). A forward
/// to its owner goes to the owner's own route for it ([`INTERNAL`], signed by the cluster, the
/// service and the visitor's path in headers), never as a call to its API: an app's paths aren't
/// the API's, and only a node can send to that route.
pub async fn serve(apps: Arc<Apps>, service: String, env: String, deployment: Option<String>, req: Request) -> Response {
    // A request with no body can be sent again: one that met the service moving (a node that
    // had just given it up) goes on to its new owner a moment later instead of a 503.
    let empty = req.headers().get(header::CONTENT_LENGTH).is_none_or(|v| v.as_bytes() == b"0") && !req.headers().contains_key(header::TRANSFER_ENCODING);
    let again = empty.then(|| (req.method().clone(), req.uri().clone(), req.headers().clone(), req.version()));
    let r = serve_once(apps.clone(), service.clone(), env.clone(), deployment.clone(), req).await;
    let moving = r.extensions().get::<By>().is_some_and(|b| b.0 == "moving");
    match again {
        Some((method, uri, headers, version)) if moving => {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let mut req = Request::new(Body::empty());
            (*req.method_mut(), *req.uri_mut(), *req.headers_mut(), *req.version_mut()) = (method, uri, headers, version);
            serve_once(apps, service, env, deployment, req).await
        }
        _ => r,
    }
}

async fn serve_once(apps: Arc<Apps>, service: String, env: String, deployment: Option<String>, req: Request) -> Response {
    let req = match to_internal(req, &service, &env, deployment.as_deref()) {
        Ok(r) => r,
        Err(r) => return *r,
    };
    let cluster = apps.deps.cluster.clone();
    cluster
        .dispatch(&lease_key(&service), req, move |req, _guard| async move {
            let req = from_internal(req);
            proxy(apps, &service, &env, deployment.as_deref(), req).await
        })
        .await
}

/// A request roll makes itself on a schedule (`run.schedules`' `path`, Vercel's crons): an
/// extension no client can send, so it goes past the environment's allow list, firewall, rate
/// limit and access gate, as its own owner's would.
#[derive(Clone, Copy, Debug)]
pub struct Scheduled;

/// GET `path` of an environment on its owner (where the schedules run), as `host` asks, with
/// its `CRON_SECRET`: the status, or why it never answered.
pub(crate) async fn call_scheduled(apps: Arc<Apps>, service: &str, env: &str, host: &str, path: &str, secret: Option<&str>, timeout: std::time::Duration) -> std::result::Result<u16, String> {
    let mut b = Request::builder().method(axum::http::Method::GET).uri(path).header(header::HOST, host).header(header::USER_AGENT, "roll-cron/1.0");
    if let Some(s) = secret {
        b = b.header(header::AUTHORIZATION, format!("Bearer {s}"));
    }
    let mut req = b.body(Body::empty()).map_err(|e| e.to_string())?;
    req.extensions_mut().insert(Scheduled);
    let r = tokio::time::timeout(timeout, async {
        let r = proxy(apps, service, env, None, req).await;
        let status = r.status().as_u16();
        // Read (and dropped) so its instance's lease and meter end with it.
        let _ = http_body_util::BodyExt::collect(http_body_util::Limited::new(r.into_body(), 1 << 20)).await;
        status
    })
    .await
    .map_err(|_| format!("it didn't answer within {} s", timeout.as_secs()))?;
    Ok(r)
}

/// GET `path` of an environment on its owner for an uptime check (`crate::checks`), as `host`
/// asks and past its gates as a schedule's call is: the status and at most `max` bytes of the
/// answer, or why it never answered.
pub(crate) async fn call_check(apps: Arc<Apps>, service: &str, env: &str, host: &str, path: &str, timeout: std::time::Duration, max: usize) -> std::result::Result<(u16, Vec<u8>), String> {
    let mut req = Request::builder().method(axum::http::Method::GET).uri(path).header(header::HOST, host).header(header::USER_AGENT, "roll-uptime/1.0").body(Body::empty()).map_err(|e| e.to_string())?;
    req.extensions_mut().insert(Scheduled);
    tokio::time::timeout(timeout, async {
        let r = proxy(apps, service, env, None, req).await;
        let status = r.status().as_u16();
        // At most `max` read; the rest dropped, so its instance's lease and meter end with it.
        let mut body = r.into_body().into_data_stream();
        let mut out = Vec::new();
        use futures::StreamExt;
        while let Some(Ok(chunk)) = body.next().await {
            let room = max.saturating_sub(out.len());
            out.extend_from_slice(&chunk[..chunk.len().min(room)]);
            if out.len() >= max {
                break;
            }
        }
        (status, out)
    })
    .await
    .map_err(|_| format!("it didn't answer within {} s", timeout.as_secs()))
}

/// A page of an environment read for its owner's API (`GET …/fetch`; Vercel MCP's
/// `web_fetch_vercel_url`): through its own proxy, as `host` asks, past its gates as a schedule's
/// call goes (an agent checking what it deployed behind a sign-in), with `deployment` that
/// deployment's own answer. The status, the headers worth reading, and at most `max` bytes.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn call_fetch(apps: Arc<Apps>, service: &str, env: &str, host: &str, path: &str, head: bool, deployment: Option<&str>, max: usize) -> std::result::Result<(u16, axum::http::HeaderMap, Vec<u8>, bool), String> {
    let method = if head { axum::http::Method::HEAD } else { axum::http::Method::GET };
    let mut req = Request::builder().method(method).uri(path).header(header::HOST, host).header(header::USER_AGENT, "roll-fetch/1.0").header(header::ACCEPT, "text/html,*/*").body(Body::empty()).map_err(|e| e.to_string())?;
    req.extensions_mut().insert(Scheduled);
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let r = proxy(apps, service, env, deployment, req).await;
        let (status, headers) = (r.status().as_u16(), r.headers().clone());
        let mut body = r.into_body().into_data_stream();
        let (mut out, mut cut) = (Vec::new(), false);
        use futures::StreamExt;
        while let Some(Ok(chunk)) = body.next().await {
            let room = max.saturating_sub(out.len());
            out.extend_from_slice(&chunk[..chunk.len().min(room)]);
            if chunk.len() > room {
                cut = true;
                break;
            }
        }
        (status, headers, out, cut)
    })
    .await
    .map_err(|_| "it didn't answer within 30 s".to_string())
}

/// The owner's route for public traffic another node sends it ([`serve`]).
const INTERNAL: &str = "/internal/apps/public";
/// On it: the service, environment and deployment (`<service>/<env>[/<deployment>]`), and the
/// visitor's own path and query.
const TARGET: &str = "x-roll-apps-target";
const ORIGINAL: &str = "x-roll-apps-uri";

/// Address a visitor's request to [`INTERNAL`] (its own path and query in [`ORIGINAL`]).
fn to_internal(mut req: Request, service: &str, env: &str, deployment: Option<&str>) -> std::result::Result<Request, Box<Response>> {
    let original = req.extensions().get::<axum::extract::OriginalUri>().map_or(req.uri(), |o| &o.0).clone();
    let pq = original.path_and_query().map_or("/", |p| p.as_str());
    let target = match deployment {
        Some(d) => format!("{service}/{env}/{d}"),
        None => format!("{service}/{env}"),
    };
    let (Ok(pq), Ok(target)) = (HeaderValue::from_str(pq), HeaderValue::from_str(&target)) else {
        return Err(Box::new(page(StatusCode::BAD_REQUEST, "Bad address", "This address isn't one.")));
    };
    let h = req.headers_mut();
    h.insert(HeaderName::from_static(ORIGINAL), pq);
    h.insert(HeaderName::from_static(TARGET), target);
    req.extensions_mut().remove::<axum::extract::OriginalUri>();
    *req.uri_mut() = Uri::from_static(INTERNAL);
    Ok(req)
}

/// Undo [`to_internal`]: the visitor's path and query back.
fn from_internal(mut req: Request) -> Request {
    if let Some(uri) = req.headers_mut().remove(ORIGINAL).and_then(|v| v.to_str().ok().and_then(|s| s.parse::<Uri>().ok())) {
        *req.uri_mut() = uri;
    }
    req.headers_mut().remove(TARGET);
    // The route it came in on is roll's, not the visitor's.
    req.extensions_mut().remove::<axum::extract::OriginalUri>();
    req
}

/// The Horizon mark, `@roll/brand`'s export (`packages/brand/assets/horizon-dark.svg`) drawn in
/// the text's colour; a test keeps the copy equal to it.
pub(crate) const HORIZON: &str = include_str!("../assets/horizon.svg");

/// What a visitor sees when roll answers instead of the app: the mark, a title and a line, dark
/// or light as their system is. A 503 (waking, restarting, busy) looks again by itself.
fn page(status: StatusCode, title: &str, text: &str) -> Response {
    page_retrying(status, title, text, 5)
}

/// [`page`], a 503 looking again after `retry` seconds.
fn page_retrying(status: StatusCode, title: &str, text: &str, retry: u32) -> Response {
    let refresh = if status == StatusCode::SERVICE_UNAVAILABLE { format!("<meta http-equiv=refresh content={retry}>") } else { String::new() };
    let html = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\">{refresh}<title>{title}</title>\
         <style>:root{{color-scheme:dark light;--bg:#000;--fg:#ededed;--muted:#a1a1a1;--accent:#4c8dff}}\
         @media (prefers-color-scheme:light){{:root{{--bg:#fff;--fg:#0a0a0a;--muted:#666}}}}\
         body{{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--fg);font:14px system-ui,sans-serif}}\
         main{{max-width:420px;padding:24px}}svg{{display:block;width:28px;height:28px;margin:0 0 16px}}\
         h1{{font-size:18px;font-weight:600;letter-spacing:-.04em;margin:0 0 8px}}p{{color:var(--muted);margin:0;line-height:1.5}}a{{color:var(--accent)}}</style>\
         <main>{HORIZON}<h1>{title}</h1><p>{text}</p></main>"
    );
    let mut r = (status, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response();
    r.extensions_mut().insert(RollPage::Error);
    if status == StatusCode::SERVICE_UNAVAILABLE
        && let Ok(v) = HeaderValue::from_str(&retry.to_string())
    {
        r.headers_mut().insert(header::RETRY_AFTER, v);
    }
    r
}

/// An environment in maintenance: its message as text (never markup), looking again every
/// half minute.
fn maintenance_page(message: &str) -> Response {
    let text = if message.is_empty() { "This service is down for maintenance. It'll be back soon.".to_string() } else { escape(message) };
    let mut r = page_retrying(StatusCode::SERVICE_UNAVAILABLE, "Down for maintenance", &text, 30);
    r.extensions_mut().insert(RollPage::Maintenance);
    r
}

/// A static site's rewrite of this request to one of the project's services (`service:` in its
/// `_redirects`): the service's name and the path. Not for a request a rewrite already sent (a
/// site rewriting to a site that rewrites back).
fn site_rewrite(apps: &Arc<Apps>, tpl: &crate::live::Template, req: &Request) -> impl std::future::Future<Output = Option<Rewrite>> + Send + use<> {
    // Read off the request here: the future never holds it.
    let args = (!req.headers().contains_key(VIA_SITE)).then(|| tpl.site.clone()).flatten().map(|key| {
        (key, percent_encoding::percent_decode_str(req.uri().path()).decode_utf8_lossy().into_owned(), req.uri().query().map(str::to_string), crate::site_rules::Visitor::of(req.headers()))
    });
    let apps = apps.clone();
    async move {
        let (key, path, query, visitor) = args?;
        rewrite_of(&apps, key, path, query, visitor).await
    }
}

/// Where a site's rewrite sends a request: one of the project's services (its name and the
/// path), or another site's URL.
enum Rewrite {
    Service(String, String),
    External(String),
}

async fn rewrite_of(apps: &Arc<Apps>, key: String, path: String, query: Option<String>, visitor: crate::site_rules::Visitor) -> Option<Rewrite> {
    let site = apps.site(&key).await.ok()?;
    match site.rules.redirect(&path, query.as_deref(), crate::sites::exists(&site, &path), &visitor)? {
        crate::site_rules::Outcome::Proxy { service, path } => Some(Rewrite::Service(service, path)),
        crate::site_rules::Outcome::External(url) => Some(Rewrite::External(url)),
        _ => None,
    }
}

/// A site's body to another site, and the answer it reads back, at most.
const EXTERNAL_BODY: usize = 10 << 20;
const EXTERNAL_ANSWER: usize = 20 << 20;

/// Headers that are one connection's, not the request's or the answer's.
const HOP: [&str; 9] = ["connection", "keep-alive", "proxy-connection", "transfer-encoding", "te", "trailer", "upgrade", "host", "content-length"];

/// A static site's proxy rewrite to another site (`/api/* https://api.example.com/:splat 200`):
/// fetched as any URL a tenant chose is (public addresses only, no redirects followed: a 3xx
/// goes back to the visitor), the visitor's method, headers and body (at most 10 MiB) sent on
/// without roll's own (`x-roll-*`, its cookies), the answer read whole (at most 20 MiB), metered
/// and in the access log as `external`.
async fn proxy_external(apps: &Arc<Apps>, live: &Arc<crate::live::EnvLive>, service: &str, env: &str, url: String, req: Request) -> Response {
    let began = Instant::now();
    let access = Access { log: apps.logs.sink(format!("http:{service}/{env}"), crate::logs::Stream::Http), seen: Seen::of(&req), span: None, deployment: Default::default() };
    let request_id = req.headers().get("x-request-id").and_then(|v| v.to_str().ok()).map(|v| v.chars().take(64).collect::<String>()).unwrap_or_else(|| crate::model::random(16));
    let done = |status: u16, bytes: u64| {
        live.meter.lock().request(code_sql::now_ms(), began.elapsed().as_secs_f64() * 1000.0, status, bytes);
        access.write(status, began, bytes, "external", &request_id, false, 0);
    };
    let (parts, body) = req.into_parts();
    let host = parts.headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let Ok(body) = axum::body::to_bytes(body, EXTERNAL_BODY).await else {
        done(413, 0);
        return page(StatusCode::PAYLOAD_TOO_LARGE, "Too much", "This request is bigger than a proxied one may be.");
    };
    // HTTPS only, as any URL a tenant chose (plain HTTP where private targets are allowed:
    // local development and tests).
    if !url.starts_with("https://") && !apps.cfg.allow_private {
        done(502, 0);
        return page(StatusCode::BAD_GATEWAY, "Not proxied", "A site's proxy rewrite goes to an https:// URL.");
    }
    let mut out = apps.site_proxy_client.request(parts.method.clone(), &url);
    for (k, v) in parts.headers.iter() {
        let name = k.as_str();
        if HOP.contains(&name) || name.starts_with("x-roll-") || name == "cookie" {
            continue;
        }
        out = out.header(k, v);
    }
    // The visitor's cookies but roll's own (`roll_…`: access, the challenge's pass, a rollout's bucket).
    let jar: Vec<&str> = parts.headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|c| c.split(';')).map(str::trim).filter(|c| !c.is_empty() && !c.starts_with("roll_")).collect();
    if !jar.is_empty() {
        out = out.header(header::COOKIE, jar.join("; "));
    }
    out = out.header("x-forwarded-host", host).header("x-forwarded-proto", apps.cfg.scheme.as_str());
    let res = match out.body(body).send().await {
        Ok(r) => r,
        Err(e) => {
            done(502, 0);
            tracing::debug!(service, error = %e.without_url(), "apps: a site's proxy rewrite didn't answer");
            return page(StatusCode::BAD_GATEWAY, "Not answering", "The site this page proxies to didn't answer.");
        }
    };
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = crate::net::read_bounded(res, EXTERNAL_ANSWER + 1).await;
    if bytes.len() > EXTERNAL_ANSWER {
        done(502, 0);
        return page(StatusCode::BAD_GATEWAY, "Too much", "The site this page proxies to answered more than a proxied answer may be.");
    }
    let mut r = Response::new(Body::from(bytes.clone()));
    *r.status_mut() = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    for (k, v) in headers.iter() {
        if !HOP.contains(&k.as_str()) && !k.as_str().starts_with("x-roll-") {
            r.headers_mut().append(k.clone(), v.clone());
        }
    }
    done(status.as_u16(), bytes.len() as u64);
    r
}

/// Marks a request a site's rewrite sent on (roll's own: `x-roll-*` never reaches an app, and a
/// visitor's is taken off where it came in, [`strip_ours`]).
const VIA_SITE: &str = "x-roll-via-site";

/// Marks a mirrored copy (roll's own, as [`VIA_SITE`] is): its target serves it, but it's no
/// visitor's page view and no answer a rollout judges.
const MIRROR: &str = "x-roll-mirror";

/// Copies in flight on a node at most; past it, a copy isn't made (a slow or sleeping target
/// never holds more).
const MIRRORS_IN_FLIGHT: usize = 64;
static MIRRORS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MIRRORS_IN_FLIGHT);

/// Marks an edge function's `context.next()` ([`crate::edge`]): the site's files answer, no edge
/// function again (roll's own, as [`VIA_SITE`] is).
const VIA_EDGE: &str = "x-roll-via-edge";

/// A request roll sends on to another service, past its target's access check ([`VIA_SITE`]).
pub(crate) fn mark_rewritten(req: &mut Request) {
    req.headers_mut().insert(HeaderName::from_static(VIA_SITE), HeaderValue::from_static("1"));
}

/// An edge function's `next()`: past the access check, and no edge function runs on it.
pub(crate) fn mark_edge_passed(req: &mut Request) {
    mark_rewritten(req);
    req.headers_mut().insert(HeaderName::from_static(VIA_EDGE), HeaderValue::from_static("1"));
}

/// rolld's internal API (where [`Cluster::dispatch`](roll_cluster::Cluster::dispatch) forwards to
/// a service's owner) with the route roll apps' public traffic is forwarded to ([`INTERNAL`]):
/// only a request another node signed gets in (`require_signed`), and it's served as the visitor
/// sent it, to the service it names.
pub fn with_forwarded(api: axum::Router, apps: Arc<Apps>) -> axum::Router {
    let cluster = apps.deps.cluster.clone();
    let route = axum::Router::new()
        .route(INTERNAL, axum::routing::any(forwarded_public))
        .with_state(apps)
        // Only the route: a layer would wrap the router's fallback too, and the merge carries it
        // over, so every other path's 404 would answer this check's 401.
        .route_layer(axum::middleware::from_fn_with_state(cluster, roll_cluster::require_signed));
    api.merge(route)
}

async fn forwarded_public(axum::extract::State(apps): axum::extract::State<Arc<Apps>>, req: Request) -> Response {
    let Some(target) = req.headers().get(TARGET).and_then(|v| v.to_str().ok()).map(str::to_string) else {
        return page(StatusCode::BAD_REQUEST, "Bad request", "No service named.");
    };
    let mut parts = target.splitn(3, '/');
    let (Some(service), Some(env)) = (parts.next(), parts.next()) else { return page(StatusCode::BAD_REQUEST, "Bad request", "No service named.") };
    let deployment = parts.next().map(str::to_string);
    let (service, env) = (service.to_string(), env.to_string());
    // As it came, to its service where it lives (here, unless it moved on the way).
    serve(apps, service, env, deployment, from_internal(req)).await
}

/// Send `req` to the project's service `to.0` in `env` at `to.1`, where that service lives,
/// through all of its own gates (access, allow list, firewall, rate limit, maintenance).
/// A split test's visitor, sent to the same service's other environment: past its gates (this
/// environment's let them in), the path as it came.
fn split_to(apps: Arc<Apps>, service: &str, env: &str, mut req: Request) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    let (service, env) = (service.to_string(), env.to_string());
    Box::pin(async move {
        req.headers_mut().insert(HeaderName::from_static(VIA_SITE), HeaderValue::from_static("1"));
        if let Some(f) = req.extensions_mut().remove::<roll_cluster::Forwarded>()
            && let Some(c) = f.client
        {
            req.extensions_mut().insert(roll_cluster::ClientAddr(c));
        }
        serve(apps, service, env, None, req).await
    })
}

fn rewrite_to(apps: Arc<Apps>, service: &str, env: &str, to: (String, String), mut req: Request) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    // A named, boxed future: `serve` comes back here (it's the same proxy), and the compiler
    // proves it `Send` only through a type it can see.
    let (service, env) = (service.to_string(), env.to_string());
    Box::pin(async move {
        let (name, path) = to;
        let Some(target) = apps.project_service(&service, &name).await else {
            apps.live(&service, &env).log.stream(crate::logs::Stream::System).line(format!("a rewrite names service:{name}, which this project doesn't have"));
            return page(StatusCode::BAD_GATEWAY, "Not here", "This site sends the address to a service that isn't there.");
        };
        // Never above the rule's own path: no `.` or `..` segment.
        let p = path.split('?').next().unwrap_or("");
        if p.split('/').any(|s| matches!(percent_encoding::percent_decode_str(s).decode_utf8_lossy().as_ref(), "." | "..")) {
            return page(StatusCode::BAD_REQUEST, "Bad address", "The rewritten address isn't one.");
        }
        let Ok(uri) = path.parse::<Uri>() else { return page(StatusCode::BAD_REQUEST, "Bad address", "The rewritten address isn't one.") };
        *req.uri_mut() = uri;
        // A forward to the target's owner carries this path, not the one the visitor asked for.
        req.extensions_mut().remove::<axum::extract::OriginalUri>();
        req.headers_mut().insert(HeaderName::from_static(VIA_SITE), HeaderValue::from_static("1"));
        // Another service's lease from here: its own routing, as a fresh request (its hops
        // counted anew: it can't be rewritten again), the visitor's address kept.
        if let Some(f) = req.extensions_mut().remove::<roll_cluster::Forwarded>()
            && let Some(c) = f.client
        {
            req.extensions_mut().insert(roll_cluster::ClientAddr(c));
        }
        serve(apps, target, env, None, req).await
    })
}

/// How a request's lease was had: another of the same kind if its instance is gone.
enum Again {
    Deployment(Box<crate::live::Template>),
    Route(Arc<crate::live::Template>),
}

/// Which of roll's pages a response is (never an app's own answer), so an environment's own
/// page can take its place ([`their_page`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum RollPage {
    Error,
    Maintenance,
}

/// One of roll's 5xx pages, as the environment's own HTML when it has one (Heroku's error and
/// maintenance pages): the same status and `Retry-After`, never cached.
async fn their_page(apps: &Arc<Apps>, service: &str, env: &str, r: Response) -> Response {
    let Some(kind) = r.extensions().get::<RollPage>().copied() else { return r };
    if !r.status().is_server_error() {
        return r;
    }
    let Ok(gate) = apps.gate(&apps.live(service, env)).await else { return r };
    let html = match kind {
        RollPage::Error => gate.pages.error.clone(),
        RollPage::Maintenance => gate.pages.maintenance.clone(),
    };
    let Some(html) = html else { return r };
    let (mut parts, _) = r.into_parts();
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    parts.headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Response::from_parts(parts, Body::from(html))
}

pub(crate) fn escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&#39;".to_string(),
            c => c.to_string(),
        })
        .collect()
}

/// The stored form of an environment's access token: its SHA-256, hex. The token itself is only
/// ever in the visitor's cookie.
pub fn token_hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether two hashes are equal, in time that doesn't depend on where they differ.
pub(crate) fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A private environment's gate (previews): `?roll_access=<token>` sets the host-only cookie
/// `roll_access_<env>` and redirects to the same URL without the token; a request with a
/// matching cookie goes on, the cookie taken out so the app never sees the token; anything else
/// is a 401. `None` lets the request through.
async fn check_access(apps: &Arc<Apps>, service: &str, env: &str, req: &mut Request) -> Option<Response> {
    let access = match apps.access(service, env).await {
        Ok(Some(a)) => a,
        Ok(None) => return None,
        Err(_) => return Some(page(StatusCode::SERVICE_UNAVAILABLE, "Unavailable", "This environment's access couldn't be checked. Try again in a few seconds.")),
    };
    // A CORS preflight to a path `access.preflight` names: to the app (it carries no credentials;
    // the request it clears still meets the gate).
    if req.method() == axum::http::Method::OPTIONS
        && req.headers().contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
        && !access.preflight.is_empty()
        && access.preflight.iter().any(|p| crate::model::path_matches(p, &crate::model::normal_path(req.uri().path())))
    {
        return None;
    }
    // Another service of the project with roll's token for this environment (`access.callers`;
    // Cloud Run's invoker): in as that service, which the app is told. A token that doesn't
    // check out is turned away, whatever else the request carries.
    if let Some(token) = req.headers().get("x-roll-caller").and_then(|v| v.to_str().ok()).map(str::to_string) {
        return match apps.caller_admits(service, env, &access.callers, &token).await {
            Some(who) => {
                req.extensions_mut().insert(VisitorIs(who, None));
                None
            }
            None => Some(denied(apps, service, env).await),
        };
    }
    // An automation (CI's end-to-end tests) with the project's bypass secret: no cookie, no
    // redirect. The header is roll's, so it never reaches the app (`x-roll-*` is stripped).
    if let (Some(bypass), Some(given)) = (&access.bypass_hash, req.headers().get("x-roll-preview-bypass").and_then(|v| v.to_str().ok()))
        && same(&token_hash(given), bypass)
    {
        return None;
    }
    if access.signed_in || access.sso.is_some() {
        // A ticket from roll's dashboard (or a provider's sign-in), coming back: the session it opens.
        if req.uri().path() == crate::visit::PATH {
            let query = req.uri().query().unwrap_or("").to_string();
            let state = take_cookie(req, &host_only(apps, crate::visit::state_cookie(env)));
            return Some(land_visit(apps, service, env, &query, state).await);
        }
        let name = host_only(apps, crate::visit::cookie(env));
        // Signing out: the session ends, its cookie goes, and they're told (not sent to sign in
        // again at once).
        if req.uri().path() == crate::visit::LOGOUT {
            if let Some(t) = take_cookie(req, &name) {
                apps.end_visit(&t).await;
            }
            let secure = if apps.cfg.scheme == "https" { "; Secure" } else { "" };
            let mut r = page(StatusCode::OK, "Signed out", "You're signed out of this site. Open it again to sign in.");
            if let Ok(c) = HeaderValue::from_str(&format!("{name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure}")) {
                r.headers_mut().append(header::SET_COOKIE, c);
            }
            return Some(r);
        }
        if let Some(t) = take_cookie(req, &name)
            && let Some((user, who)) = apps.visitor(service, env, &t).await
        {
            // In: the environment's shared token (a link they also came in by) stays roll's. The
            // app is told who (`x-roll-visitor`, set after every client's `x-roll-*` is gone).
            take_cookie(req, &host_only(apps, format!("roll_access_{env}")));
            // And a token it can check (`visitor_token`), for the name the visitor asked (no port,
            // no trailing dot).
            let raw = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
            let host = match raw.rsplit_once(':') {
                Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) => h,
                _ => raw,
            }
            .trim_end_matches('.')
            .to_ascii_lowercase();
            let jwt = apps.visitor_jwt(service, env, &format!("{}://{host}", apps.cfg.scheme), &user, &who).await;
            req.extensions_mut().insert(VisitorIs(who, jwt));
            return None;
        }
        // No other way in: signing in. With a password too, its page and session come first.
        if access.token_hash.is_none() && access.password_hash.is_none() {
            return Some(match &access.sso {
                Some(sso) => sso_sign_in(apps, service, env, sso, req).await,
                None => sign_in(apps, service, env, req),
            });
        }
    }
    // A password ([`crate::password`]): its session, or its page's POST.
    if let Some(ph) = access.password_hash.clone() {
        if let Some(v) = take_cookie(req, &host_only(apps, crate::password::cookie(env)))
            && apps.password_session_ok(service, env, &ph, &v).await
        {
            take_cookie(req, &host_only(apps, format!("roll_access_{env}")));
            return None;
        }
        if req.uri().path() == crate::password::PATH && req.method() == axum::http::Method::POST {
            let client = crate::live::client_ip(roll_cluster::client_addr(req).as_deref());
            let Ok(body) = axum::body::to_bytes(std::mem::take(req.body_mut()), 4096).await else { return Some(StatusCode::PAYLOAD_TOO_LARGE.into_response()) };
            return Some(apps.password_post(service, env, &ph, client, &body).await);
        }
    }
    let signed_in = access.signed_in;
    let sso = access.sso.clone();
    // No way in yet: the password's page, signing in, or the 401.
    let password = access.password_hash.is_some();
    let turned_away = |req: &Request| {
        let to = crate::visit::local_path(req.uri().path_and_query().map_or("/", |p| p.as_str()));
        let page_load = req.method() == axum::http::Method::GET;
        (to, page_load)
    };
    let Some(hash) = access.token_hash else {
        if password {
            let (to, page_load) = turned_away(req);
            return Some(if page_load { crate::password::page(StatusCode::UNAUTHORIZED, &to, "") } else { denied(apps, service, env).await });
        }
        return Some(denied(apps, service, env).await);
    };
    let name = host_only(apps, format!("roll_access_{env}"));
    let query = req.uri().query().unwrap_or("").to_string();
    let mut token = None;
    let mut rest = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        match pair.strip_prefix("roll_access=") {
            Some(v) => token = Some(percent_encoding::percent_decode_str(v).decode_utf8_lossy().into_owned()),
            None => rest.push(pair),
        }
    }
    if let Some(t) = token {
        if !same(&token_hash(&t), &hash) {
            return Some(denied(apps, service, env).await);
        }
        // One leading slash, and no backslash (browsers read `/\host` as `//host`): the
        // redirect never leaves the site.
        let path = format!("/{}", req.uri().path().replace('\\', "%5C").trim_start_matches('/'));
        let location = if rest.is_empty() { path.to_string() } else { format!("{path}?{}", rest.join("&")) };
        let secure = if apps.cfg.scheme == "https" { "; Secure" } else { "" };
        let cookie = format!("{name}={t}; Path=/; HttpOnly; SameSite=Lax{secure}");
        let mut r = StatusCode::FOUND.into_response();
        let h = r.headers_mut();
        if let (Ok(l), Ok(c)) = (HeaderValue::from_str(&location), HeaderValue::from_str(&cookie)) {
            h.insert(header::LOCATION, l);
            h.insert(header::SET_COOKIE, c);
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            return Some(r);
        }
        return Some(denied(apps, service, env).await);
    }
    // Every Cookie header, minus ours; ours must match.
    let mut ok = false;
    let mut kept = Vec::new();
    for v in req.headers().get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()) {
        for c in v.split(';').map(str::trim).filter(|c| !c.is_empty()) {
            match c.strip_prefix(&format!("{name}=")) {
                Some(t) => ok |= same(&token_hash(t), &hash),
                None => kept.push(c.to_string()),
            }
        }
    }
    if !ok {
        // No token, and no session: the password, or signing in, is the other way in.
        if password {
            let (to, page_load) = turned_away(req);
            if page_load {
                return Some(crate::password::page(StatusCode::UNAUTHORIZED, &to, ""));
            }
        }
        return Some(match (&sso, signed_in) {
            (Some(s), _) => sso_sign_in(apps, service, env, s, req).await,
            (None, true) => sign_in(apps, service, env, req),
            (None, false) => denied(apps, service, env).await,
        });
    }
    req.headers_mut().remove(header::COOKIE);
    if !kept.is_empty()
        && let Ok(v) = HeaderValue::from_str(&kept.join("; "))
    {
        req.headers_mut().insert(header::COOKIE, v);
    }
    None
}

/// A visitor with no session of a sign-in-gated environment's: a page load goes to roll's
/// dashboard, which signs them in and sends them back with a ticket; anything else is a 401
/// that says where.
fn sign_in(apps: &Arc<Apps>, service: &str, env: &str, req: &Request) -> Response {
    let Some(web) = apps.cfg.web_url.clone() else {
        return page(StatusCode::UNAUTHORIZED, "Sign in to open this", "This environment lets in people signed in to roll, and this node doesn't know where roll is.");
    };
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let back = format!("{}://{host}{}", apps.cfg.scheme, req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/"));
    let q = |v: &str| percent_encoding::utf8_percent_encode(v, percent_encoding::NON_ALPHANUMERIC).to_string();
    // The browser this began in: its ticket opens only there.
    let (state, binding) = crate::visit::new_state();
    let url = format!("{web}/dashboard/apps/visit?service={}&env={}&to={}&state={binding}", q(service), q(env), q(&back));
    let page_load = matches!(*req.method(), axum::http::Method::GET | axum::http::Method::HEAD)
        && req.headers().get(header::ACCEPT).and_then(|a| a.to_str().ok()).is_some_and(|a| a.contains("text/html"));
    let mut r = if page_load && let Ok(l) = HeaderValue::from_str(&url) {
        let mut r = StatusCode::FOUND.into_response();
        r.headers_mut().insert(header::LOCATION, l);
        r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        r
    } else {
        page(StatusCode::UNAUTHORIZED, "Sign in to open this", &format!("This environment lets in people signed in to roll who can open its project. <a href=\"{}\">Sign in</a>.", escape(&url)))
    };
    set_state_cookie(apps, env, &state, &mut r);
    r
}

/// The state cookie ([`crate::visit::state_cookie`]) on a response that sends someone to sign in.
fn set_state_cookie(apps: &Arc<Apps>, env: &str, state: &str, r: &mut Response) {
    let secure = if apps.cfg.scheme == "https" { "; Secure" } else { "" };
    if let Ok(c) = HeaderValue::from_str(&format!("{}={state}; Path=/; HttpOnly; SameSite=Lax; Max-Age=600{secure}", host_only(apps, crate::visit::state_cookie(env)))) {
        r.headers_mut().append(header::SET_COOKIE, c);
    }
}

/// A gate cookie's name: on HTTPS with `__Host-`, so another site under the same domain (another
/// tenant's app on `up.roll.site`) can't set one for this host.
pub(crate) fn host_only(apps: &Apps, name: String) -> String {
    if apps.cfg.scheme == "https" { format!("__Host-{name}") } else { name }
}

/// Who a sign-in-gated environment's session is ([`Apps::visitor`]): a provider's visitor's
/// email, or a roll person's user id, told to the app as `x-roll-visitor`.
#[derive(Clone)]
struct VisitorIs(String, Option<String>);

/// A visitor with no session of an environment that lets a provider sign people in
/// (`access.sso`): a page load goes to roll's sign-in with that provider, which sends them back
/// with a ticket; anything else is a 401.
fn sso_sign_in(apps: &Arc<Apps>, service: &str, env: &str, sso: &crate::model::Sso, req: &Request) -> impl std::future::Future<Output = Response> + Send + use<> {
    let raw = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let host = match raw.rsplit_once(':') {
        Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => raw,
    }
    .to_ascii_lowercase();
    let to = crate::visit::local_path(req.uri().path_and_query().map_or("/", |p| p.as_str()));
    let page_load = matches!(*req.method(), axum::http::Method::GET | axum::http::Method::HEAD) && req.headers().get(header::ACCEPT).and_then(|a| a.to_str().ok()).is_some_and(|a| a.contains("text/html"));
    let (apps, service, env, sso) = (apps.clone(), service.to_string(), env.to_string(), sso.clone());
    async move {
        // The browser this began in: its ticket opens only there.
        let (state, binding) = crate::visit::new_state();
        let mut r = sso_redirect(&apps, &service, &env, &sso, &host, &to, page_load, &binding).await;
        set_state_cookie(&apps, &env, &state, &mut r);
        r
    }
}

#[allow(clippy::too_many_arguments)]
async fn sso_redirect(apps: &Arc<Apps>, service: &str, env: &str, sso: &crate::model::Sso, host: &str, to: &str, page_load: bool, binding: &str) -> Response {
    let who = if sso.provider == "github" { "GitHub" } else { "Google" };
    match apps.sso_start(service, env, sso, host, to, binding).await {
        Some(url) if page_load => match HeaderValue::from_str(&url) {
            Ok(l) => {
                let mut r = StatusCode::FOUND.into_response();
                r.headers_mut().insert(header::LOCATION, l);
                r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                r
            }
            Err(_) => page(StatusCode::INTERNAL_SERVER_ERROR, "Try again", "The sign-in couldn't be started."),
        },
        Some(url) => page(StatusCode::UNAUTHORIZED, "Sign in to open this", &format!("This site lets in people who sign in with {who}. <a href=\"{}\">Sign in</a>.", escape(&url))),
        None => page(StatusCode::UNAUTHORIZED, "Sign in to open this", &format!("This site lets in people who sign in with {who}, and this node can't sign anyone in with it.")),
    }
}

/// `/__roll/visit?ticket=…&to=…` on the environment's own host: the ticket traded for a session
/// cookie (host-only, 12 hours), then on to `to` on the same site.
async fn land_visit(apps: &Arc<Apps>, service: &str, env: &str, query: &str, state: Option<String>) -> Response {
    let mut ticket = None;
    let mut to = "/".to_string();
    for pair in query.split('&') {
        let decode = |v: &str| percent_encoding::percent_decode_str(v).decode_utf8_lossy().into_owned();
        if let Some(v) = pair.strip_prefix("ticket=") {
            ticket = Some(decode(v));
        } else if let Some(v) = pair.strip_prefix("to=") {
            to = crate::visit::local_path(&decode(v));
        }
    }
    // Where it goes, checked before the ticket is used.
    let Ok(location) = HeaderValue::from_str(&to) else { return page(StatusCode::BAD_REQUEST, "Bad address", "This sign-in link isn't one.") };
    let Some(session) = (match (ticket, state) {
        (Some(t), Some(s)) => apps.open_visit(service, env, &t, &s).await,
        _ => None,
    }) else {
        return page(StatusCode::UNAUTHORIZED, "This sign-in link has expired", "Open the page again: roll signs you in and sends you back. (A sign-in link only works in the browser it began in.)");
    };
    let secure = if apps.cfg.scheme == "https" { "; Secure" } else { "" };
    let cookie = format!("{}={session}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{secure}", host_only(apps, crate::visit::cookie(env)), crate::visit::SESSION_S);
    let mut r = StatusCode::FOUND.into_response();
    let Ok(c) = HeaderValue::from_str(&cookie) else { return page(StatusCode::INTERNAL_SERVER_ERROR, "Try again", "The sign-in couldn't be finished.") };
    r.headers_mut().insert(header::LOCATION, location);
    r.headers_mut().insert(header::SET_COOKIE, c);
    // The state is used up.
    if let Ok(c) = HeaderValue::from_str(&format!("{}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure}", host_only(apps, crate::visit::state_cookie(env)))) {
        r.headers_mut().append(header::SET_COOKIE, c);
    }
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// A static site's request, answered from its files (`sites.rs`): no instance wakes. GET and
/// HEAD; a file, a directory's `index.html`, a single-page app's index for a page path, or its
/// `404.html`. Metered and logged as any request (instance `site`).
/// What a static site's answer needs of the request (owned: the request isn't held across a wait).
struct SiteRequest {
    method: axum::http::Method,
    path: String,
    query: Option<String>,
    if_none_match: Option<String>,
    accept_encoding: Option<String>,
    range: Option<String>,
    if_range: Option<String>,
    request_id: Option<String>,
    seen: Seen,
    /// What a `_redirects` rule's conditions read (country, languages, cookies).
    visitor: crate::site_rules::Visitor,
    /// A POST's content type and body (a form's, at most [`crate::forms::MAX_BODY`]; None past it).
    form: Option<(Option<String>, Option<bytes::Bytes>)>,
}

impl SiteRequest {
    /// [`Self::of`], with a POST's body read (a static site takes one only for its forms).
    async fn read(req: Request) -> SiteRequest {
        let mut r = SiteRequest::of(&req);
        if req.method() == axum::http::Method::POST {
            let ct = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_string);
            r.form = Some((ct, axum::body::to_bytes(req.into_body(), crate::forms::MAX_BODY).await.ok()));
        }
        r
    }

    fn of(req: &Request) -> SiteRequest {
        let h = |n: &str| req.headers().get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
        SiteRequest { method: req.method().clone(), path: req.uri().path().to_string(), query: req.uri().query().map(str::to_string), if_none_match: h("if-none-match"), accept_encoding: h("accept-encoding"), range: h("range"), if_range: h("if-range"), request_id: h("x-request-id"), seen: Seen::of(req), visitor: crate::site_rules::Visitor::of(req.headers()), form: None }
    }
}

async fn serve_site(apps: &Arc<Apps>, live: &Arc<crate::live::EnvLive>, tpl: &crate::live::Template, req: SiteRequest, service: &str, env: &str, began: Instant) -> Response {
    let key = tpl.site.clone().unwrap_or_default();
    // Who sent a form (its limit is per sender), before the access line takes what was seen.
    let client = req.seen.ip.clone();
    let access = Access { log: apps.logs.sink(format!("http:{service}/{env}"), crate::logs::Stream::Http), seen: req.seen, span: None, deployment: Default::default() };
    let request_id = req.request_id.as_deref().map(|v| v.chars().take(64).collect()).unwrap_or_else(|| crate::model::random(16));
    let done = |status: u16, bytes: u64| {
        live.meter.lock().request(code_sql::now_ms(), began.elapsed().as_secs_f64() * 1000.0, status, bytes);
        access.write(status, began, bytes, "site", &request_id, false, 0);
    };
    let head = req.method == axum::http::Method::HEAD;
    // A POST to one of its forms ([`crate::forms`]): kept, and the sender sent on.
    if let Some((ct, body)) = &req.form
        && let Ok(site) = apps.site(&key).await
        && !site.forms.is_empty()
    {
        let path = percent_encoding::percent_decode_str(&req.path).decode_utf8_lossy().into_owned();
        match apps.form_post(&site.forms, service, env, &path, client.as_deref(), ct.as_deref(), body.as_deref()).await {
            Some(Ok(next)) => {
                // Its action's page when the site has it; else roll's thanks.
                if let Some(to) = next.filter(|n| crate::sites::exists(&site, n)) {
                    done(303, 0);
                    let mut r = StatusCode::SEE_OTHER.into_response();
                    if let Ok(v) = HeaderValue::from_str(&to) {
                        r.headers_mut().insert(header::LOCATION, v);
                    }
                    return r;
                }
                done(200, 0);
                return page(StatusCode::OK, "Thanks", "Your submission was received.");
            }
            Some(Err((status, why))) => {
                done(status, 0);
                return page(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST), "Not sent", &why);
            }
            None => {}
        }
    }
    if req.method != axum::http::Method::GET && !head {
        done(405, 0);
        let mut r = StatusCode::METHOD_NOT_ALLOWED.into_response();
        r.headers_mut().insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return r;
    }
    let site = match apps.site(&key).await {
        Ok(s) => s,
        Err(e) => {
            live.log.stream(crate::logs::Stream::System).line(format!("the static site's files: {e}"));
            done(503, 0);
            return retry_soon(page(StatusCode::SERVICE_UNAVAILABLE, "Starting up", "The site's files are on their way. Try again in a few seconds."));
        }
    };
    let path = percent_encoding::percent_decode_str(&req.path).decode_utf8_lossy().into_owned();
    // Its own rules first (`_redirects`): a redirect, or another of its files.
    let not_found = || page(StatusCode::NOT_FOUND, "Not found", "There's no page at this address.");
    if crate::site_rules::is_rules_file(&path) {
        done(404, 0);
        return not_found();
    }
    let own = crate::sites::exists(&site, &path);
    // A rule that hides a path (a 404 rule) or sends it elsewhere decides it; a rewrite or no
    // rule at all leaves a missing fingerprinted file to the site's earlier deployments.
    let mut skew_ok = !own;
    let resolved = match site.rules.redirect(&path, req.query.as_deref(), own, &req.visitor) {
        Some(crate::site_rules::Outcome::Redirect(status, to)) => {
            done(status, 0);
            let mut r = Response::new(Body::empty());
            *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::MOVED_PERMANENTLY);
            if let Ok(v) = HeaderValue::from_str(&to) {
                r.headers_mut().insert(header::LOCATION, v);
            }
            return r;
        }
        // Taken before the site is served ([`site_rewrite`]).
        Some(crate::site_rules::Outcome::Proxy { .. } | crate::site_rules::Outcome::External(_)) => {
            done(404, 0);
            return not_found();
        }
        Some(crate::site_rules::Outcome::Serve(status, to)) => {
            skew_ok &= status == 200;
            crate::sites::resolve(&site, &to, false).filter(|(_, s)| *s == 200).map(|(n, _)| (n, status))
        }
        None => crate::sites::resolve(&site, &path, tpl.spa),
    };
    // A fingerprinted file the live site doesn't have may be an earlier deployment's, asked for by
    // a page loaded before the deploy (a lazy-loaded chunk): served from there (skew protection),
    // even where a single-page app's rewrite would answer its index.
    let resolved = resolved.map(|(n, s)| (n.to_string(), s));
    let earlier = match skew_ok {
        true => apps.earlier_site_file(service, env, &key, &path).await,
        false => None,
    };
    let skewed = earlier.is_some();
    let (site, key, name, status) = match (earlier, resolved) {
        (Some((s, k, n)), _) => (s, k, n, 200),
        (None, Some((n, s))) => (site, key, n, s),
        (None, None) => {
            done(404, 0);
            return not_found();
        }
    };
    let name = name.as_str();
    // The site's own `_headers` for the path, on a 304 as on the answer (a cache takes a 304's).
    let site_headers = site.rules.headers(&path);
    let ctype = crate::sites::content_type(name);
    // Compressed as Render's and Vercel's static sites are: the build's own `.br` or `.gz` of the
    // file, else gzipped here (once: a site never changes), for text-like files the client takes
    // it for. Each encoding has its ETag.
    let compress = crate::sites::compressible(ctype);
    let size = site.files[name].1;
    let pre = |ext: &str| site.files.get(&format!("{name}.{ext}")).copied();
    let (qbr, qgz) = (accept_q(req.accept_encoding.as_deref(), "br"), accept_q(req.accept_encoding.as_deref(), "gzip"));
    // Gzipped here: what this file would be, if the node gets to it (never for a HEAD's sake).
    let on_the_fly = compress && qgz > 0.0 && pre("gz").is_none() && (crate::sites::GZIP_MIN..=crate::sites::GZIP_MAX).contains(&size);
    let encoding = match () {
        _ if !compress => None,
        _ if pre("br").is_some() && qbr > 0.0 && qbr >= qgz => Some("br"),
        _ if pre("gz").is_some() && qgz > 0.0 => Some("gzip"),
        _ if pre("br").is_some() && qbr > 0.0 => Some("br"),
        _ if on_the_fly && (!head || site.gzipped(name).is_some()) => Some("gzip"),
        _ => None,
    };
    let etag_of = |encoding: Option<&str>| {
        format!("\"{}-{}{}\"", &key[..key.len().min(16)], site.files[name].0, match encoding {
            Some("br") => "-br",
            Some(_) => "-gz",
            None => "",
        })
    };
    let cache = match () {
        _ if ctype.starts_with("text/html") => "no-cache",
        // The URL asked for names its bytes (never a rewrite's stable name, nor an earlier
        // deployment's file, which a rule could hide tomorrow).
        _ if status == 200 && !skewed && name == path.trim_matches('/') && crate::sites::fingerprinted(name) => "public, max-age=31536000, immutable",
        _ => "public, max-age=300",
    };
    let etag = etag_of(encoding);
    // Weak comparison (a CDN that compresses weakens the tag: `W/"…"`), and `*`.
    let matches = |v: &str| v.split(',').map(|t| t.trim()).any(|t| t == "*" || t.strip_prefix("W/").unwrap_or(t) == etag);
    if status == 200 && req.if_none_match.as_deref().is_some_and(matches) {
        done(304, 0);
        let mut r = StatusCode::NOT_MODIFIED.into_response();
        let h = r.headers_mut();
        h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("ascii"));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
        if compress {
            h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
        }
        own_headers(h, &site_headers, compress);
        return r;
    }
    // The bytes: the build's own compressed file, ours, or the file (when ours can't be had now).
    let gz = match encoding {
        Some("gzip") if pre("gz").is_none() => site.gzip(name).await,
        _ => None,
    };
    let encoding = if encoding == Some("gzip") && pre("gz").is_none() && gz.is_none() { None } else { encoding };
    let etag = etag_of(encoding);
    let file = match encoding {
        Some("br") => pre("br").expect("chosen"),
        Some(_) if gz.is_none() => pre("gz").expect("chosen"),
        _ => site.files[name],
    };
    let (mut offset, mut len) = match &gz {
        Some(b) => (file.0, b.len() as u64),
        None => file,
    };
    // One byte range of the file as it is (a video's seek, a download resumed); several, or one
    // that doesn't parse, get the whole file, as RFC 9110 allows. `If-Range` keeps it to this
    // version of the file.
    let ranged = status == 200 && encoding.is_none() && req.if_range.as_deref().is_none_or(|v| v.trim() == etag);
    let mut range = None;
    if ranged && let Some(r) = req.range.as_deref().and_then(|r| byte_range(r, len)) {
        let Ok((start, end)) = r else {
            done(416, 0);
            let mut r = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            r.headers_mut().insert(header::CONTENT_RANGE, HeaderValue::from_str(&format!("bytes */{len}")).expect("ascii"));
            return r;
        };
        range = Some(format!("bytes {start}-{end}/{len}"));
        (offset, len) = (offset + start, end - start + 1);
    }
    let status = if range.is_some() { 206 } else { status };
    let body = if head {
        Body::empty()
    } else if let Some(b) = gz {
        Body::from(b)
    } else {
        // The file's bytes, read from the tar as they're sent.
        let tar = site.tar.clone();
        let opened = async move {
            let mut f = tokio::fs::File::open(&tar).await?;
            tokio::io::AsyncSeekExt::seek(&mut f, std::io::SeekFrom::Start(offset)).await?;
            Ok::<_, std::io::Error>(tokio::io::AsyncReadExt::take(f, len))
        };
        let stream = futures::stream::once(opened)
            .map(|r| match r {
                Ok(f) => futures::stream::unfold(Some(f), |f| async move {
                    let mut f = f?;
                    let mut buf = vec![0u8; 64 << 10];
                    match tokio::io::AsyncReadExt::read(&mut f, &mut buf).await {
                        Ok(0) => None,
                        Ok(n) => {
                            buf.truncate(n);
                            Some((Ok::<_, std::io::Error>(bytes::Bytes::from(buf)), Some(f)))
                        }
                        Err(e) => Some((Err(e), None)),
                    }
                })
                .boxed(),
                Err(e) => futures::stream::once(async move { Err(e) }).boxed(),
            })
            .flatten();
        Body::from_stream(stream)
    };
    done(status, if head { 0 } else { len });
    let mut r = Response::new(body);
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    if let Some(e) = encoding {
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static(e));
    }
    if compress {
        h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    if ranged {
        h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    }
    if let Some(cr) = range {
        h.insert(header::CONTENT_RANGE, HeaderValue::from_str(&cr).expect("ascii"));
    }
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("ascii"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(HeaderName::from_static("x-content-type-options"), HeaderValue::from_static("nosniff"));
    own_headers(h, &site_headers, compress);
    r
}

/// A site's own `_headers` over roll's (each checked when the file was read): the first a block
/// sets replaces roll's, more of one name add to it. Its own `Vary` never drops what the encoding
/// varies on (a cache would hand gzip to a client that can't read it).
fn own_headers(h: &mut axum::http::HeaderMap, headers: &[(String, String)], compress: bool) {
    let mut set = std::collections::HashSet::new();
    for (n, v) in headers {
        if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(n.as_bytes()), HeaderValue::from_str(v)) {
            if set.insert(n.clone()) {
                h.insert(n, v);
            } else {
                h.append(n, v);
            }
        }
    }
    let varies = h.get_all(header::VARY).iter().any(|v| v.to_str().is_ok_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("accept-encoding") || t.trim() == "*")));
    if compress && !varies {
        h.append(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
}

/// How much `accept` (an `Accept-Encoding`) wants `coding`: its own q, else `*`'s, else none
/// (RFC 9110: `*` stands only for codings it doesn't name).
fn accept_q(accept: Option<&str>, coding: &str) -> f32 {
    let mut star = 0.0;
    for part in accept.unwrap_or("").split(',') {
        let mut it = part.split(';');
        let name = it.next().unwrap_or("").trim();
        let q = it.find_map(|a| a.trim().split_once('=').filter(|(k, _)| k.trim().eq_ignore_ascii_case("q")).map(|(_, v)| v.trim().parse::<f32>().unwrap_or(0.0))).unwrap_or(1.0);
        if name.eq_ignore_ascii_case(coding) {
            return q;
        }
        if name == "*" {
            star = q;
        }
    }
    star
}

/// `Range: bytes=…` against a file of `len` bytes: the first and last byte, or `Err` when it
/// can't be met (a 416). None: not one range this answers (the whole file instead).
fn byte_range(h: &str, len: u64) -> Option<std::result::Result<(u64, u64), ()>> {
    let spec = h.trim().strip_prefix("bytes=")?.trim();
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    let num = |s: &str| (!s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())).then(|| s.parse::<u64>().ok()).flatten();
    Some(match (a.is_empty(), b.is_empty()) {
        // The last `b` bytes.
        (true, false) => match num(b)? {
            n if n == 0 || len == 0 => Err(()),
            n => Ok((len.saturating_sub(n), len - 1)),
        },
        (false, _) => {
            let start = num(a)?;
            let end = if b.is_empty() { u64::MAX } else { num(b)? };
            if end < start {
                return None;
            }
            if start >= len { Err(()) } else { Ok((start, end.min(len - 1))) }
        }
        (true, true) => return None,
    })
}

/// A visitor's gradual-rollout bucket (docs/apps/README.md, "Gradual rollouts").
const BUCKET: &str = "roll_bucket";

/// A visitor's instance, for a service with affinity ([`crate::live::affinity_key`]).
const AFFINITY: &str = "roll_instance";

/// The visitor's rollout bucket from its cookie (0–99), taken out of the cookies the app sees.
fn take_bucket(req: &mut Request) -> Option<u8> {
    take_cookie(req, BUCKET).and_then(|n| n.parse::<u8>().ok()).filter(|n| *n < 100)
}

/// One of roll's cookies, taken out of the cookies the app sees.
fn take_cookie(req: &mut Request, name: &str) -> Option<String> {
    let mut found = None;
    let mut kept = Vec::new();
    let mut ours = false;
    for v in req.headers().get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()) {
        for c in v.split(';').map(str::trim).filter(|c| !c.is_empty()) {
            match c.strip_prefix(&format!("{name}=")) {
                Some(n) => {
                    ours = true;
                    found = Some(n.to_string());
                }
                None => kept.push(c.to_string()),
            }
        }
    }
    if ours {
        req.headers_mut().remove(header::COOKIE);
        if !kept.is_empty()
            && let Ok(v) = HeaderValue::from_str(&kept.join("; "))
        {
            req.headers_mut().insert(header::COOKIE, v);
        }
    }
    found
}

/// The 401 of a private environment. A preview's (`pv-…`) links into the dashboard, which signs
/// the reviewer in and sends them on with the link that has the token.
async fn denied(apps: &Arc<Apps>, service: &str, env: &str) -> Response {
    let text = "Open it from its link in roll (the change, the pull request or the dashboard).";
    let link = async {
        let web = apps.cfg.web_url.clone()?;
        let preview = env.strip_prefix("pv-")?;
        let (sid, e) = (service.to_string(), env.to_string());
        let (svc, row) = apps.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await.ok()?;
        if !row.ephemeral {
            return None;
        }
        let q = |v: &str| percent_encoding::utf8_percent_encode(v, percent_encoding::NON_ALPHANUMERIC).to_string();
        Some(format!("{web}/dashboard/projects/{}/previews?preview=pv_{}&go={}", q(&svc.project), q(preview), q(&svc.name)))
    };
    match link.await {
        Some(url) => page(StatusCode::UNAUTHORIZED, "This preview is private", &format!("<a href=\"{url}\">Open it through roll</a>: sign in, and you're sent on with its link.")),
        None => page(StatusCode::UNAUTHORIZED, "This environment is private", text),
    }
}

/// A 503 the client may retry in a second (a restart or a crowd that clears quickly).
fn retry_soon(mut r: Response) -> Response {
    r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    r
}

fn is_upgrade(h: &axum::http::HeaderMap) -> bool {
    h.contains_key(header::UPGRADE)
        && h.get(header::CONNECTION).and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("upgrade")))
}

fn strip_hop_by_hop(h: &mut axum::http::HeaderMap, keep_upgrade: bool) {
    for name in ["keep-alive", "proxy-connection", "proxy-authenticate", "proxy-authorization", "te", "trailer", "transfer-encoding"] {
        h.remove(name);
    }
    if !keep_upgrade {
        h.remove(header::CONNECTION);
        h.remove(header::UPGRADE);
    }
}

/// `x-roll-*` in an app's answer: roll's own nodes read them (a forwarding node replays a
/// request on `x-roll-code-unavailable`), so an app never sets them.
fn strip_roll_headers(h: &mut axum::http::HeaderMap) {
    let ours: Vec<_> = h.keys().filter(|k| k.as_str().starts_with("x-roll-")).cloned().collect();
    for k in ours {
        h.remove(k);
    }
}

/// The waking proxy on the owner: an instance (woken if need be), the request forwarded, the
/// answer metered.
pub(crate) async fn proxy(apps: Arc<Apps>, service: &str, env: &str, deployment: Option<&str>, req: Request) -> Response {
    let (began, seen) = (Instant::now(), Seen::of(&req));
    let path = crate::model::normal_path(&seen.path);
    let r = proxy_inner(apps.clone(), service, env, deployment, req).await;
    // An answer roll gave itself, before any instance (an allow list's, a rate limit's, a wake
    // that failed): in the access log with why, and metered, so its errors count.
    if let Some(By(by)) = r.extensions().get::<By>().copied() {
        let status = r.status().as_u16();
        // A page the owner chose (paused, in maintenance) is a request, not the app's error: no
        // 5xx alert for an environment its owner stopped.
        let metered = if matches!(by, "paused" | "maintenance") { 0 } else { status };
        apps.live(service, env).meter.lock().request(code_sql::now_ms(), began.elapsed().as_secs_f64() * 1000.0, metered, 0);
        Access { log: apps.logs.sink(format!("http:{service}/{env}"), crate::logs::Stream::Http), seen, span: None, deployment: Default::default() }.write_by(status, began, by);
    }
    let mut r = their_page(&apps, service, env, r).await;
    // A preview's answers (and a deployment's own URL, a copy of what's live) are never indexed,
    // whatever the app says (Vercel's): roll's pages, the 401 and the access link's redirect too.
    let gate = apps.gate(&apps.live(service, env)).await.ok();
    // The environment's own headers (HSTS, a CSP): the app's own of the same name win unless the
    // rule replaces them, as a static site's `_headers` do.
    if let Some(g) = &gate {
        let mut ours = Vec::new();
        for rule in g.headers.iter().filter(|h| h.matches(&path)) {
            for (k, v) in &rule.set {
                if let (Ok(name), Ok(value)) = (axum::http::HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v))
                    && (rule.replace || !r.headers().contains_key(&name))
                {
                    ours.push(name.clone());
                    r.headers_mut().insert(name, value);
                }
            }
        }
        // Then the live deployment's roll.toml headers: never over one the environment set.
        for rule in g.file_headers.iter().filter(|h| h.matches(&path)) {
            for (k, v) in &rule.set {
                if let (Ok(name), Ok(value)) = (axum::http::HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v))
                    && !ours.contains(&name)
                    && (rule.replace || !r.headers().contains_key(&name))
                {
                    r.headers_mut().insert(name, value);
                }
            }
        }
    }
    if deployment.is_some() || gate.as_ref().is_some_and(|g| g.preview) {
        r.headers_mut().insert("x-robots-tag", HeaderValue::from_static("noindex"));
    }
    r
}

async fn proxy_inner(apps: Arc<Apps>, service: &str, env: &str, deployment: Option<&str>, mut req: Request) -> Response {
    let began = Instant::now();
    // Came in through roll: the private environment's access cookie (checked just below).
    let cookie = format!("{}=", host_only(&apps, format!("roll_access_{env}")));
    let via_link = env.starts_with("pv-") && req.headers().get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).any(|v| v.split(';').any(|c| c.trim().starts_with(&cookie)));
    // Its allow list first (deployment URLs too: they're public), before anything wakes.
    let gate = match apps.gate(&apps.live(service, env)).await {
        Ok(g) => g,
        Err(_) => return by("unavailable", page(StatusCode::SERVICE_UNAVAILABLE, "Unavailable", "This environment's settings couldn't be read. Try again in a few seconds.")),
    };
    let client = roll_cluster::client_addr(&req);
    // roll's own call on a schedule, or a browser check's page (its run's token, for this
    // environment only, never sent on): none of the environment's gates are for it. A check's
    // page is otherwise a visitor's (its routes and skew protection apply); a scheduled call isn't.
    let check = req.headers_mut().remove("x-roll-check");
    let scheduled = req.extensions().get::<Scheduled>().is_some();
    // Back from an edge function's `next()` (signed, with the visitor's address): its gates
    // were passed on the way in, so none of them runs twice.
    let ours = scheduled || req.headers().contains_key(VIA_EDGE) || check.as_ref().and_then(|v| v.to_str().ok()).is_some_and(|t| apps.check_token_admits(t, service, env));
    if !ours && !gate.admits(client.as_deref()) {
        return by("allow_list", page(StatusCode::FORBIDDEN, "Not from here", "This service only answers visitors from the networks its owner allowed."));
    }
    // Its firewall rules (a 403 that says nothing of them).
    let ua = req.headers().get(header::USER_AGENT).map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()).unwrap_or_default();
    // A challenge page's answer, on the environment's own host: checked, then a pass.
    if !ours && req.uri().path() == crate::challenge::PATH && req.method() == axum::http::Method::POST
        && let Some(key) = &gate.challenge_key
    {
        let net = crate::challenge::network(crate::live::client_ip(client.as_deref()));
        let Ok(body) = axum::body::to_bytes(req.into_body(), 1024).await else { return StatusCode::PAYLOAD_TOO_LARGE.into_response() };
        let (mut seed, mut nonce) = (String::new(), String::new());
        for (k, v) in url::form_urlencoded::parse(&body) {
            match k.as_ref() {
                "seed" => seed = v.into_owned(),
                "nonce" => nonce = v.into_owned(),
                _ => {}
            }
        }
        if !crate::challenge::answers(key, &net, code_sql::now_ms(), &seed, &nonce) {
            return by("challenge", StatusCode::FORBIDDEN.into_response());
        }
        let until = code_sql::now_ms() / 1000 + crate::challenge::PASS_S;
        let secure = if apps.cfg.scheme == "https" { "; Secure" } else { "" };
        let cookie = format!("{}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{secure}", host_only(&apps, crate::challenge::cookie(env)), crate::challenge::pass(key, &net, until), crate::challenge::PASS_S);
        let mut r = StatusCode::NO_CONTENT.into_response();
        if let Ok(c) = HeaderValue::from_str(&cookie) {
            r.headers_mut().insert(header::SET_COOKIE, c);
        }
        r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return by("challenge", r);
    }
    let attacked = gate.attack_until.is_some_and(|u| code_sql::now_ms() < u);
    if !ours && (attacked || !(gate.firewall.is_empty() && gate.file_firewall.is_empty())) {
        let path = crate::model::normal_path(req.uri().path());
        let ip = crate::live::client_ip(client.as_deref());
        // Who the client is, only when a rule asks (a crawler's claim costs DNS lookups).
        let asks = attacked || gate.firewall.iter().chain(gate.file_firewall.iter()).any(|r| !r.bots.is_empty() || r.action == crate::model::FirewallAction::Challenge);
        let bot = match asks {
            true => crate::bots::identify(&apps.bots_verified, apps.deps.resolver.as_ref(), &ua, ip).await,
            false => None,
        };
        let asked = crate::model::Asked { method: req.method().as_str(), path: &path, raw_path: req.uri().path(), user_agent: &ua, ip, headers: req.headers(), query: req.uri().query(), bot };
        let mut hits = Vec::new();
        let mut verdict = gate.firewall_verdict(&asked, &mut hits);
        apps.live(service, env).count_hits(&hits);
        // Attack mode: nothing gets in without a pass, whatever a rule would let in (a deny
        // still denies).
        if attacked && matches!(verdict, crate::live::Verdict::Allow | crate::live::Verdict::Limit(..)) {
            verdict = crate::live::Verdict::Challenge;
        }
        match verdict {
            crate::live::Verdict::Allow => {}
            // A browser with this hour's pass goes on (the cookie is roll's: the app never sees it);
            // anything else gets the page that earns one.
            // A verified search engine can't run the page, and a challenge is never meant to
            // keep one out.
            crate::live::Verdict::Challenge if bot.is_some_and(|b| b.class == crate::bots::Class::Search && b.verified == Some(true)) => {}
            crate::live::Verdict::Challenge => {
                let key = gate.challenge_key.clone().unwrap_or_default();
                let net = crate::challenge::network(crate::live::client_ip(client.as_deref()));
                let passed = take_cookie(&mut req, &host_only(&apps, crate::challenge::cookie(env))).is_some_and(|c| crate::challenge::passes(&key, &net, code_sql::now_ms() / 1000, &c));
                if !passed {
                    let to = crate::visit::local_path(req.uri().path_and_query().map_or("/", |p| p.as_str()));
                    let mut r = (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], crate::challenge::page(&crate::challenge::seed(&key, &net, code_sql::now_ms()), &to)).into_response();
                    r.extensions_mut().insert(RollPage::Error);
                    return by("challenge", r);
                }
            }
            crate::live::Verdict::Deny => return by("firewall", page(StatusCode::FORBIDDEN, "Not allowed", "This service doesn't answer this request.")),
            // A rule's own limit (Vercel's rate-limit rules): its bucket per client, before anything wakes.
            crate::live::Verdict::Limit(bucket, limit, keyed) => {
                let (lv, ip) = (apps.live(service, env), crate::live::client_ip(client.as_deref()));
                // Keyed, each address still has ten times the rate between its keys: a new key
                // on every request isn't a way around it.
                let wide = crate::model::RateLimit { per_minute: limit.per_minute.saturating_mul(10), burst: Some(limit.burst().saturating_mul(10)) };
                let held = lv.take_token_in(bucket, ip, keyed, &limit).and_then(|()| if keyed.is_some() { lv.take_token_in(bucket, ip, None, &wide) } else { Ok(()) });
                if let Err(wait) = held {
                    let mut r = page(StatusCode::TOO_MANY_REQUESTS, "Slow down", if keyed.is_some() { "Too many requests with this key. Try again in a moment." } else { "Too many requests from your address. Try again in a moment." });
                    if let Ok(v) = HeaderValue::from_str(&(wait.as_secs_f64().ceil() as u64).max(1).to_string()) {
                        r.headers_mut().insert(header::RETRY_AFTER, v);
                    }
                    return by("rate_limit", r);
                }
            }
        }
    }
    // Its rate limit per client, before anything wakes (a cache refresh is roll's, not a client's).
    if let Some(limit) = &gate.rate
        && !ours
        && req.extensions().get::<crate::resp_cache::Refresh>().is_none()
        && let Err(wait) = apps.live(service, env).take_token(crate::live::client_ip(client.as_deref()), limit)
    {
        let mut r = page(StatusCode::TOO_MANY_REQUESTS, "Slow down", "Too many requests from your address. Try again in a moment.");
        if let Ok(v) = HeaderValue::from_str(&(wait.as_secs_f64().ceil() as u64).max(1).to_string()) {
            r.headers_mut().insert(header::RETRY_AFTER, v);
        }
        return by("rate_limit", r);
    }
    // Heartbeats (`crate::heartbeats`): a ping from outside, any method, on its own host. Its token is
    // its credential, so a private environment's pings come in too; nothing wakes.
    if let Some(token) = req.uri().path().strip_prefix(crate::heartbeats::PATH).filter(|t| !t.is_empty() && t.len() <= 64 && !t.contains('/')) {
        let token = token.to_string();
        return if apps.heartbeat_ping(service, env, &token).await { (StatusCode::OK, "ok\n").into_response() } else { page(StatusCode::NOT_FOUND, "Not found", "There's no heartbeat with this address.") };
    }
    // A private environment answers only visitors with its token, before anything wakes. A
    // site's rewrite (a header only roll sets) was let in by the site, by the project's own rule.
    // A site's rewrite was let in by the site; roll's own scheduled call by roll.
    let rewritten = req.headers().contains_key(VIA_SITE);
    // The key its visitors' tokens are signed with, public, on any of its hosts.
    if req.uri().path() == crate::visitor_token::JWKS_PATH && req.method() == axum::http::Method::GET {
        return match apps.visitor_jwks(service, env).await {
            Some(k) => ([(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "public, max-age=300")], k.to_string()).into_response(),
            None => page(StatusCode::SERVICE_UNAVAILABLE, "Unavailable", "Try again in a few seconds."),
        };
    }
    if !rewritten && !ours && let Some(turned_away) = check_access(&apps, service, env, &mut req).await {
        // The access link's own redirect is a way in, not a refusal.
        return match turned_away.status() {
            StatusCode::UNAUTHORIZED => by("private", turned_away),
            // The access couldn't be read (the store): roll's own answer, counted as one.
            StatusCode::SERVICE_UNAVAILABLE => by("unavailable", turned_away),
            _ => turned_away,
        };
    }
    // Its own redirects (Vercel's project redirects, any kind of service), before anything wakes:
    // a GET or HEAD, or any method for a 307 or 308 (which keep it).
    let reads = matches!(*req.method(), axum::http::Method::GET | axum::http::Method::HEAD);
    if !scheduled
        && let Some(rules) = &gate.redirects
        && let Some((status, to)) = rules.find(req.uri().path(), req.uri().query(), &crate::site_rules::Visitor::of(req.headers()))
        && (reads || matches!(status, 307 | 308))
        && let Ok(loc) = HeaderValue::from_str(&to)
    {
        let mut r = Response::new(Body::empty());
        *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::MOVED_PERMANENTLY);
        r.headers_mut().insert(header::LOCATION, loc);
        return by("redirect", r);
    }
    // Its routes (Vercel's rewrites to another project): the first that takes the path sends the
    // request to that service's environment of the same name, the path as it came, past the
    // target's gates as a site's rewrite is (this environment's let it in). Never again from there.
    // Never roll's own scheduled call: its `CRON_SECRET` is this service's, not the route's.
    if !req.headers().contains_key(VIA_SITE)
        && !scheduled
        && let Some(r) = gate.routes.iter().find(|r| r.takes(req.uri().path()))
    {
        let pq = req.uri().path_and_query().map_or("/", |p| p.as_str()).to_string();
        return rewrite_to(apps.clone(), service, env, (r.service.clone(), pq), req).await;
    }
    // roll previews' feedback button: its script from the preview's own host, and a page load
    // (a browser's document, else a GET asking for HTML) gets the tag.
    let target = if via_link { apps.feedback_target(service, env).await } else { None };
    if let Some(t) = &target
        && req.uri().path() == crate::feedback::PATH
    {
        return crate::feedback::script(t);
    }
    if target.is_some() && req.uri().path() == crate::feedback::NOTES {
        return crate::feedback::notes(apps.feedback_notes(), env, req.uri().query()).await;
    }
    let page_load = req.method() == axum::http::Method::GET
        && match req.headers().get("sec-fetch-dest").and_then(|v| v.to_str().ok()) {
            Some(dest) => dest.eq_ignore_ascii_case("document"),
            None => req.headers().get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|a| a.contains("text/html")),
        };
    // Web Vitals: its script and beacons on the environment's own host, and its tag on pages.
    if gate.vitals && req.uri().path() == crate::vitals::SCRIPT_PATH && req.method() == axum::http::Method::GET {
        return crate::vitals::script();
    }
    if gate.vitals && req.uri().path() == crate::vitals::BEACON && req.method() == axum::http::Method::POST {
        let country = req.headers().get("cf-ipcountry").and_then(|v| v.to_str().ok()).filter(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphabetic())).map(|c| c.to_ascii_uppercase());
        let Ok(body) = axum::body::to_bytes(req.into_body(), crate::vitals::MAX_BODY).await else { return StatusCode::PAYLOAD_TOO_LARGE.into_response() };
        return apps.vitals_beacon(service, env, &body, country).into_response();
    }
    // Custom events: on every environment's own host, as its access log's page views are.
    if req.uri().path() == crate::track::SCRIPT_PATH && req.method() == axum::http::Method::GET {
        return crate::track::script();
    }
    if req.uri().path() == crate::track::BEACON && req.method() == axum::http::Method::POST {
        let country = req.headers().get("cf-ipcountry").and_then(|v| v.to_str().ok()).filter(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphabetic())).map(|c| c.to_ascii_uppercase());
        let Ok(body) = axum::body::to_bytes(req.into_body(), crate::track::MAX_BODY).await else { return StatusCode::PAYLOAD_TOO_LARGE.into_response() };
        return apps.event_beacon(service, env, &body, country).into_response();
    }
    let feedback = Tags { feedback: target.is_some() && page_load, vitals: gate.vitals && page_load, snippets: gate.snippets.clone().filter(|_| page_load) };
    if feedback.feedback || feedback.vitals || feedback.snippets.is_some() {
        // The page as the app wrote it, so the tag can go in as it streams.
        req.headers_mut().insert(header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    }
    if feedback.snippets.is_some() {
        // Never a 304 for a page with snippets: one kept from before a snippet changed would
        // keep the old one (its bytes aren't the file's, nor its ETag).
        req.headers_mut().remove(header::IF_NONE_MATCH);
    }
    let live = match apps.load_live(service, env).await {
        Ok(l) => l,
        Err(Error::Unavailable(_)) => return by("moving", retry_soon(page(StatusCode::SERVICE_UNAVAILABLE, "Starting up", "The service is waking up. Try again in a moment."))),
        Err(e) => return by("not_deployed", page(StatusCode::NOT_FOUND, "Not deployed", &e.to_string())),
    };
    // In maintenance: roll's page, nothing woken. A deployment's own URL still answers (a fix
    // can be tried before maintenance ends).
    if deployment.is_none()
        && let Some(m) = &gate.maintenance
    {
        return by("maintenance", maintenance_page(m));
    }
    // Paused: nothing of it answers (a static site's files and rewrites, a deployment's own URL).
    if live.paused() {
        return by("paused", page(StatusCode::SERVICE_UNAVAILABLE, "Paused", "This service is paused. Its owner can resume it."));
    }
    // Skew protection (`run.skew_protection`): what a page loaded before a deploy asks for,
    // naming its deployment, goes to that one (past every gate above, as its own URL would).
    let skewed = match (deployment, live.template()) {
        (None, Some(t)) if !scheduled && t.run.skew_protection => {
            let asked = crate::skew::asked(&req);
            apps.skew_target(service, env, &t, asked).await
        }
        _ => None,
    };
    let deployment = deployment.or(skewed.as_deref());
    // A page load gets the deployment that answers it in a cookie (below).
    let skew_cookie = deployment.is_none() && live.template().is_some_and(|t| t.run.skew_protection) && crate::skew::navigation(&req);
    // A static site's rewrite to one of the project's services, before its own cookies (a
    // gradual rollout's bucket, affinity) are read: they're the target's to read too.
    if !rewritten {
        let site = match deployment {
            Some(d) => apps.deployment_template(service, env, d).await.ok().filter(|t| t.site.is_some()),
            None => live.template().filter(|t| t.site.is_some()).map(|t| (*t).clone()),
        };
        if let Some(t) = site
            && let Some(to) = site_rewrite(&apps, &t, &req).await
        {
            return match to {
                Rewrite::Service(s, p) => rewrite_to(apps, service, env, (s, p), req).await,
                Rewrite::External(url) => proxy_external(&apps, &live, service, env, url, req).await,
            };
        }
    }
    // A gradual rollout keeps each visitor on one deployment: their bucket, from the cookie it
    // gave them (never passed to the app), or a new one while a canary runs.
    let mut bucket = take_bucket(&mut req);
    let affine = take_cookie(&mut req, AFFINITY);
    // A split test's visitors are bucketed as a rollout's are (roll's own call never splits, nor
    // a request another environment sent here).
    let split = gate.split.as_ref().filter(|_| deployment.is_none() && !ours && !req.headers().contains_key(VIA_SITE));
    let fresh = deployment.is_none() && bucket.is_none() && (live.canary().is_some() || split.is_some());
    if fresh {
        let mut b = [0u8; 1];
        getrandom::fill(&mut b).expect("system randomness");
        bucket = Some(b[0] % 100);
    }
    // Mirroring (Envoy's): a copy of a bodiless request to another environment, its answer
    // dropped; never roll's own call, an upgrade, or a request another environment sent.
    if let Some(m) = gate.mirror.as_ref()
        && deployment.is_none()
        && !ours
        && !req.headers().contains_key(VIA_SITE)
        && matches!(*req.method(), axum::http::Method::GET | axum::http::Method::HEAD)
        && !is_upgrade(req.headers())
        && {
            let mut b = [0u8; 1];
            getrandom::fill(&mut b).expect("system randomness");
            (b[0] % 100) < m.percent
        }
    {
        let mut copy = Request::builder().method(req.method().clone()).uri(req.uri().clone());
        for (k, v) in req.headers() {
            copy = copy.header(k, v);
        }
        copy = copy.header(MIRROR, "1");
        if let (Ok(mut copy), Ok(permit)) = (copy.body(Body::empty()), MIRRORS.try_acquire()) {
            if let Some(c) = roll_cluster::client_addr(&req) {
                copy.extensions_mut().insert(roll_cluster::ClientAddr(c));
            }
            let (apps, service, target) = (apps.clone(), service.to_string(), m.environment.clone());
            permit.forget();
            tokio::spawn(async move {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(30), split_to(apps, &service, &target, copy)).await;
                MIRRORS.add_permits(1);
            });
        }
    }
    // In the split's share: the other environment answers, and the cookie keeps them there.
    if let (Some(sp), Some(b)) = (split, bucket)
        && sp.takes(b)
    {
        let mut r = split_to(apps.clone(), service, &sp.environment, req).await;
        if fresh {
            let secure = if apps.cfg.scheme == "https" { "; Secure" } else { "" };
            if let Ok(c) = HeaderValue::from_str(&format!("{BUCKET}={b}; Path=/; Max-Age=86400; HttpOnly; SameSite=Lax{secure}")) {
                r.headers_mut().append(header::SET_COOKIE, c);
            }
        }
        return r;
    }
    // A shared cache's answer (`resp_cache`), before anything wakes: a GET or HEAD of the live
    // deployment (no gradual rollout, no preview's feedback tag), without `Authorization`.
    let refreshing = req.extensions().get::<crate::resp_cache::Refresh>().is_some();
    let cache_key = (deployment.is_none()
        && bucket.is_none()
        && live.canary().is_none()
        && !feedback.feedback
        && !rewritten
        && !ours
        && matches!(*req.method(), axum::http::Method::GET | axum::http::Method::HEAD)
        && !req.headers().contains_key(header::AUTHORIZATION)
        && !is_upgrade(req.headers())
        // A private environment's answers are its visitors' alone.
        && matches!(apps.access(service, env).await, Ok(None)))
    .then(|| req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into()));
    let cache_env = crate::env_key(service, env);
    let cache_host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if let (Some(k), false) = (&cache_key, refreshing) {
        let head = req.method() == axum::http::Method::HEAD;
        let hit = apps.responses.get(&cache_env, &cache_host, k, req.headers(), head);
        // A cached answer is a request served: metered (its bytes, its status) and logged.
        let served = |r: &Response| {
            let bytes = r.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse::<u64>().ok()).unwrap_or(0);
            let bytes = if head { 0 } else { bytes };
            live.meter.lock().request(code_sql::now_ms(), began.elapsed().as_secs_f64() * 1000.0, r.status().as_u16(), bytes);
            Access { log: apps.logs.sink(format!("http:{service}/{env}"), crate::logs::Stream::Http), seen: Seen::of(&req), span: None, deployment: Default::default() }.write_cached(r.status().as_u16(), began, bytes);
        };
        match hit {
            Some(crate::resp_cache::Hit::Fresh(r)) => {
                served(&r);
                return with_feedback(feedback, r);
            }
            Some(crate::resp_cache::Hit::Stale { response, refresh }) => {
                served(&response);
                if refresh {
                    // Fetched again in the background, as this request was (its headers decide
                    // what it varies on; its client address passes the same allow list).
                    let mut again = Request::new(Body::empty());
                    *again.uri_mut() = req.uri().clone();
                    *again.headers_mut() = req.headers().clone();
                    again.headers_mut().remove(header::COOKIE);
                    again.extensions_mut().insert(crate::resp_cache::Refresh);
                    if let Some(c) = req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>() {
                        again.extensions_mut().insert(*c);
                    }
                    tokio::spawn(refresh_cached(apps.clone(), service.to_string(), env.to_string(), again, cache_host.clone(), k.clone()));
                }
                return with_feedback(feedback, response);
            }
            None => {}
        }
    }
    let cache_req = cache_key.as_ref().filter(|_| req.method() == axum::http::Method::GET).map(|_| req.headers().clone());
    // How the lease was had, to have another if its instance turns out gone.
    let mut again: Option<Again> = None;
    let acquired = match deployment {
        // One deployment's own URL: that deployment, whatever is live.
        Some(d) => match apps.deployment_template(service, env, d).await {
            // A deployment's own URL takes no form's submission: only what's live keeps them.
            Ok(t) if t.site.is_some() => {
                if let Some((target, name, next)) = edge_first(&apps, service, env, &t, EdgeProbe::of(&req)).await {
                    return crate::edge::to_function(apps.clone(), target, env.to_string(), name, next, req).await;
                }
                return with_feedback(feedback, serve_site(&apps, &live, &t, SiteRequest::of(&req), service, env, began).await);
            }
            Ok(t) => {
                // A web service's middleware first, on the paths it names (its `next()` comes
                // back to this deployment).
                if let Some((target, name, next)) = edge_first(&apps, service, env, &t, EdgeProbe::of(&req)).await {
                    return crate::edge::to_function(apps.clone(), target, env.to_string(), name, next, req).await;
                }
                again = Some(Again::Deployment(Box::new(t.clone())));
                live.acquire_deployment(t).await
            }
            Err(e) => return by("not_deployed", page(StatusCode::NOT_FOUND, "No such deployment", &e.to_string())),
        },
        None => match live.may_wake().map(|_| live.route(bucket)) {
            // A static site's files answer (a gradual rollout's bucket chose which deployment's).
            Ok(Some(t)) if t.site.is_some() => {
                // Its edge functions first, on the paths they name.
                if let Some((target, name, next)) = edge_first(&apps, service, env, &t, EdgeProbe::of(&req)).await {
                    return crate::edge::to_function(apps.clone(), target, env.to_string(), name, next, req).await;
                }
                let mut r = with_feedback(feedback, serve_site(&apps, &live, &t, SiteRequest::read(req).await, service, env, began).await);
                if let (true, Some(b)) = (fresh, bucket) {
                    let secure = if apps.cfg.scheme == "https" { "; Secure" } else { "" };
                    if let Ok(c) = HeaderValue::from_str(&format!("{BUCKET}={b}; Path=/; Max-Age=86400; HttpOnly; SameSite=Lax{secure}")) {
                        r.headers_mut().append(header::SET_COOKIE, c);
                    }
                }
                return r;
            }
            Ok(Some(t)) => {
                // A web service's middleware first, before anything wakes for it.
                if let Some((target, name, next)) = edge_first(&apps, service, env, &t, EdgeProbe::of(&req)).await {
                    return crate::edge::to_function(apps.clone(), target, env.to_string(), name, next, req).await;
                }
                again = Some(Again::Route(t.clone()));
                live.acquire_preferring(t, affine.as_deref()).await
            }
            Ok(None) => Err(WakeError::NoDeployment),
            Err(e) => Err(e),
        },
    };
    let mut lease = match acquired {
        Ok(l) => l,
        Err(WakeError::NoDeployment) => return by("not_deployed", page(StatusCode::NOT_FOUND, "Not deployed yet", "This service has no deployment in this environment yet.")),
        Err(WakeError::NotWeb) => return by("not_deployed", page(StatusCode::NOT_FOUND, "Not a web service", "This service is a worker or a cron job: it doesn't answer HTTP.")),
        Err(WakeError::Busy) => return by("wake", retry_soon(page(StatusCode::SERVICE_UNAVAILABLE, "Busy starting", "Too many requests are waiting for this service to start. Try again in a moment."))),
        Err(WakeError::Full(_)) => return by("full", retry_soon(page(StatusCode::TOO_MANY_REQUESTS, "Busy", "Every instance of this service is busy. Try again in a moment."))),
        Err(WakeError::Capped) => return by("capped", page(StatusCode::PAYMENT_REQUIRED, "Paused", "This project reached its spending limit for the month. Its owner can raise the limit to bring it back.")),
        Err(WakeError::Paused) => return by("paused", page(StatusCode::SERVICE_UNAVAILABLE, "Paused", "This service is paused. Its owner can resume it.")),
        Err(e) => {
            live.log.stream(crate::logs::Stream::System).line(format!("a request couldn't wake the service: {e}"));
            return by("wake", page(StatusCode::SERVICE_UNAVAILABLE, "Starting up", "The service is waking up or failing to start. Try again in a few seconds."));
        }
    };
    // The access line's method and path (no query: it can carry tokens), and with traces on
    // the proxy's own span in the request's trace (the wait, a wake, the answer).
    let span = gate.traces.then(|| ProxySpan::start(&apps, service, env, req.headers()));
    let access = Access { log: apps.logs.sink(format!("http:{service}/{env}"), crate::logs::Stream::Http), seen: Seen::of(&req), span, deployment: Default::default() };
    let mirrored = req.headers().contains_key(MIRROR);
    // A custom domain sent to another of the app's ports: that port's address on the same
    // instance (published from the deploy after the domain's port was set; until then, a 502).
    if let Some(TargetPort(p)) = req.extensions().get::<TargetPort>().copied() {
        match lease.inst.port_addr(p) {
            Some(a) => lease.addr = a,
            None => return by("port", page(StatusCode::BAD_GATEWAY, "Not on that port yet", &format!("This domain goes to the app's port {p}, which its live deployment doesn't publish: deploy again to publish it."))),
        }
    }
    let pq = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
    let Ok(uri) = format!("http://{}{pq}", lease.addr).parse::<Uri>() else {
        return (StatusCode::BAD_REQUEST, "bad request").into_response();
    };
    // A single-page app's page path (a GET for HTML, no file extension) that the app has no
    // file for answers its `index.html` instead: Vercel's and Render's SPA rewrite.
    let spa_page = live.template().is_some_and(|t| t.spa)
        && matches!(*req.method(), axum::http::Method::GET | axum::http::Method::HEAD)
        && !req.uri().path().rsplit('/').next().unwrap_or("").contains('.')
        && req.headers().get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_none_or(|a| a.contains("text/html") || a.contains("*/*"));
    let spa_retry = spa_page.then(|| (req.method().clone(), req.headers().clone()));
    let upgrade = is_upgrade(req.headers());
    let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut req));
    let host = req.headers().get(header::HOST).cloned();
    let client = roll_cluster::client_addr(&req)
        .or_else(|| req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip().to_string()));
    *req.uri_mut() = uri;
    // An h2c app (gRPC) gets HTTP/2, with `te: trailers` kept (gRPC needs it); an upgrade
    // (a WebSocket) is HTTP/1.1 whatever the app says.
    let h2 = lease.h2c && !upgrade;
    let te_trailers = h2 && req.headers().get("te").and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("trailers")));
    *req.version_mut() = if h2 { axum::http::Version::HTTP_2 } else { axum::http::Version::HTTP_11 };
    strip_hop_by_hop(req.headers_mut(), upgrade);
    if te_trailers {
        req.headers_mut().insert("te", HeaderValue::from_static("trailers"));
    }
    // Nothing a client sends may look like it came from roll.
    let roll_headers: Vec<HeaderName> = req.headers().keys().filter(|k| k.as_str().starts_with("x-roll-")).cloned().collect();
    for k in roll_headers {
        req.headers_mut().remove(k);
    }
    // Who a sign-in let in (`access.signed_in` or `access.sso`): roll's to say.
    if let Some(v) = req.extensions().get::<VisitorIs>().cloned() {
        if let Ok(who) = HeaderValue::from_str(&v.0) {
            req.headers_mut().insert(HeaderName::from_static("x-roll-visitor"), who);
        }
        if let Some(jwt) = v.1.and_then(|t| HeaderValue::from_str(&t).ok()) {
            req.headers_mut().insert(HeaderName::from_static("x-roll-visitor-jwt"), jwt);
        }
    }
    let h = req.headers_mut();
    if let Some(host) = host {
        h.insert(HeaderName::from_static("x-forwarded-host"), host);
    }
    // The app's SDK continues the trace under the proxy's span.
    if let Some(v) = access.span.as_ref().and_then(|p| HeaderValue::from_str(&p.traceparent()).ok()) {
        h.insert(HeaderName::from_static("traceparent"), v);
    }
    let proto = if apps.cfg.scheme == "https" { "https" } else { "http" };
    h.insert(HeaderName::from_static("x-forwarded-proto"), HeaderValue::from_static(proto));
    if let Some(c) = client.and_then(|c| HeaderValue::from_str(&c).ok()) {
        h.insert(HeaderName::from_static("x-forwarded-for"), c);
    }
    // When roll took the request (Heroku's, unix ms): the app's APM reads the time it queued.
    let took = code_sql::now_ms() - began.elapsed().as_millis() as i64;
    if let Ok(v) = HeaderValue::from_str(&took.to_string()) {
        h.insert(HeaderName::from_static("x-request-start"), v);
    }
    if !h.contains_key("x-request-id")
        && let Ok(v) = HeaderValue::from_str(&crate::model::random(16))
    {
        h.insert(HeaderName::from_static("x-request-id"), v);
    }
    let request_id = h.get("x-request-id").and_then(|v| v.to_str().ok()).unwrap_or("").chars().take(64).collect::<String>();
    // A request with nothing to send again (a GET, HEAD or OPTIONS without a body) that finds
    // its instance gone (killed, out of memory: nothing listens) is sent once more, to a fresh
    // one, as Fly's proxy does: the app never saw it.
    let bodiless = !upgrade
        && matches!(*req.method(), axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS)
        && !req.headers().contains_key(header::TRANSFER_ENCODING)
        && req.headers().get(header::CONTENT_LENGTH).is_none_or(|v| v == "0")
        // The body itself: an HTTP/2 GET can carry one with no length said.
        && axum::body::HttpBody::size_hint(req.body()).exact() == Some(0);
    let replay = bodiless.then(|| (req.method().clone(), pq.clone(), req.headers().clone(), req.version()));
    let (mut instance, mut cold, mut waited_ms) = (lease.inst.name.clone(), lease.cold, lease.waited.as_millis() as u64);
    *access.deployment.lock() = Some(lease.inst.deployment.clone());
    let send = async { if h2 { apps.client_h2.request(req).await } else { apps.client.request(req).await } };
    // Its timeout runs to the answer's head: an upgrade, or a stream that has begun, is the
    // app's to keep.
    let sent = match lease.timeout.filter(|_| !upgrade) {
        Some(t) => match tokio::time::timeout(t, send).await {
            Ok(r) => r,
            Err(_) => {
                if !mirrored {
                    live.answered(&lease.inst, 504, None);
                }
                live.meter.lock().request(code_sql::now_ms(), began.elapsed().as_secs_f64() * 1000.0, 504, 0);
                access.write(504, began, 0, &instance, &request_id, cold, waited_ms);
                return page(StatusCode::GATEWAY_TIMEOUT, "Timed out", &format!("The service didn't answer within its {} s.", t.as_secs()));
            }
        },
        None => send.await,
    };
    // Gone: nothing listened (the app never saw it), or a kept-alive connection broke and the
    // instance isn't running any more (it died, perhaps with the request read: a safe request
    // may be repeated, RFC 9110 9.2.2, as browsers do when a reused connection closes). Never
    // for a request with a body. The runtime can learn of a kill a moment after the connection
    // breaks, so an instance it still thinks runs is tried on a fresh connection: one nothing
    // accepts is gone; one that accepts is alive (slow, or it closed the connection itself: a
    // 502, never sent again, nothing counted against it).
    let gone = match &sent {
        Err(e) if replay.is_some() && again.is_some() => e.is_connect() || !apps.deps.runtime.running(&lease.inst.name).await || !accepts(lease.addr).await,
        _ => false,
    };
    let sent = match (sent, replay, again) {
        (Err(e), Some((method, pq, headers, version)), Some(how)) if gone => {
            live.connect_failed(&lease.inst);
            live.log.stream(crate::logs::Stream::System).line(format!("{instance} stopped answering: the request goes to a fresh instance"));
            let next = match how {
                Again::Deployment(t) => live.acquire_deployment(*t).await,
                Again::Route(t) => live.acquire_tpl(t).await,
            };
            match next {
                Ok(l) => {
                    lease = l;
                    (instance, cold, waited_ms) = (lease.inst.name.clone(), lease.cold, lease.waited.as_millis() as u64);
                    *access.deployment.lock() = Some(lease.inst.deployment.clone());
                    let mut r = Request::new(Body::empty());
                    (*r.method_mut(), *r.headers_mut(), *r.version_mut()) = (method, headers, version);
                    *r.uri_mut() = format!("http://{}{pq}", lease.addr).parse().unwrap_or_default();
                    let send = async { if h2 { apps.client_h2.request(r).await } else { apps.client.request(r).await } };
                    match lease.timeout {
                        Some(t) => match tokio::time::timeout(t, send).await {
                            Ok(r) => r,
                            Err(_) => {
                                if !mirrored {
                                    live.answered(&lease.inst, 504, None);
                                }
                                live.meter.lock().request(code_sql::now_ms(), began.elapsed().as_secs_f64() * 1000.0, 504, 0);
                                access.write(504, began, 0, &instance, &request_id, cold, waited_ms);
                                return page(StatusCode::GATEWAY_TIMEOUT, "Timed out", &format!("The service didn't answer within its {} s.", t.as_secs()));
                            }
                        },
                        None => send.await,
                    }
                }
                Err(we) => {
                    live.log.stream(crate::logs::Stream::System).line(format!("a request couldn't wake a fresh instance: {we}"));
                    Err(e)
                }
            }
        }
        (sent, _, _) => sent,
    };
    let mut resp = match sent {
        Ok(r) => r,
        Err(e) if e.is_connect() => {
            // Nothing listens any more (killed, out of memory): the next request starts a
            // fresh instance.
            live.connect_failed(&lease.inst);
            if !mirrored {
                live.answered(&lease.inst, 503, None);
            }
            live.meter.lock().request(code_sql::now_ms(), began.elapsed().as_secs_f64() * 1000.0, 503, 0);
            access.write(503, began, 0, &instance, &request_id, cold, waited_ms);
            return retry_soon(page(StatusCode::SERVICE_UNAVAILABLE, "Restarting", "The instance stopped; the next request starts it again."));
        }
        Err(e) => {
            if !mirrored {
                live.answered(&lease.inst, 502, None);
            }
            live.meter.lock().request(code_sql::now_ms(), began.elapsed().as_secs_f64() * 1000.0, 502, 0);
            access.write(502, began, 0, &instance, &request_id, cold, waited_ms);
            // What went wrong is the owner's to read (the environment's log), not a visitor's.
            live.log.stream(crate::logs::Stream::System).line(format!("a request failed: the instance closed the connection: {e}"));
            return page(StatusCode::BAD_GATEWAY, "The service didn't answer", "It closed the connection before it answered. Try again in a moment.");
        }
    };
    if resp.status() == StatusCode::NOT_FOUND
        && let Some((method, headers)) = spa_retry
        && let Ok(uri) = format!("http://{}/index.html", lease.addr).parse::<Uri>()
    {
        let mut again = axum::http::Request::builder().method(method).uri(uri).body(Body::empty()).expect("a request");
        *again.headers_mut() = headers;
        strip_hop_by_hop(again.headers_mut(), false);
        let ours: Vec<HeaderName> = again.headers().keys().filter(|k| k.as_str().starts_with("x-roll-")).cloned().collect();
        for k in ours {
            again.headers_mut().remove(k);
        }
        if let Ok(r) = apps.client.request(again).await
            && r.status().is_success()
        {
            resp = r;
        }
    }
    let status = resp.status().as_u16();
    strip_roll_headers(resp.headers_mut());
    // Kept for the next request when the app says a shared cache may keep it: its own headers,
    // taken before roll's cookies go on, and a body of known length within the bound.
    let keep = match (&cache_key, &cache_req) {
        (Some(k), Some(rh)) if status == 200 => crate::resp_cache::policy(resp.headers())
            .filter(|_| resp.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse::<usize>().ok()).is_some_and(|n| n <= crate::resp_cache::PER_BODY))
            .map(|pol| (k.clone(), rh.clone(), resp.headers().clone(), pol)),
        _ => None,
    };
    // An answer's cache tags are for roll's cache, kept or not (Netlify's and Vercel's do the same).
    for t in crate::resp_cache::TAG_HEADERS {
        resp.headers_mut().remove(t);
    }
    if resp.status() == StatusCode::SWITCHING_PROTOCOLS
        && let Some(client_upgrade) = client_upgrade
    {
        let upstream = hyper::upgrade::on(&mut resp);
        let live2 = live.clone();
        tokio::spawn(async move {
            // The socket counts as a request in flight for as long as it's open.
            let _lease = lease;
            if let (Ok(a), Ok(b)) = tokio::join!(client_upgrade, upstream)
                && let Ok((_, down)) = tokio::io::copy_bidirectional(&mut TokioIo::new(a), &mut TokioIo::new(b)).await
            {
                live2.meter.lock().request(code_sql::now_ms(), began.elapsed().as_secs_f64() * 1000.0, 101, down);
                access.write(101, began, down, &instance, &request_id, cold, waited_ms);
            }
        });
        return resp.map(Body::new);
    }
    strip_hop_by_hop(resp.headers_mut(), false);
    // Affinity: the visitor's cookie names the instance that answered.
    if lease.affinity {
        let key = crate::live::affinity_key(&lease.inst.name);
        if affine.as_deref() != Some(key.as_str()) {
            let secure = if apps.cfg.scheme == "https" { "; Secure" } else { "" };
            if let Ok(c) = HeaderValue::from_str(&format!("{AFFINITY}={key}; Path=/; HttpOnly; SameSite=Lax{secure}")) {
                resp.headers_mut().append(header::SET_COOKIE, c);
            }
        }
    }
    if let (true, Some(b)) = (fresh, bucket) {
        let secure = if apps.cfg.scheme == "https" { "; Secure" } else { "" };
        if let Ok(c) = HeaderValue::from_str(&format!("{BUCKET}={b}; Path=/; Max-Age=86400; HttpOnly; SameSite=Lax{secure}")) {
            resp.headers_mut().append(header::SET_COOKIE, c);
        }
    }
    if skew_cookie && let Some(c) = crate::skew::cookie(&lease.inst.deployment, apps.cfg.scheme == "https") {
        resp.headers_mut().append(header::SET_COOKIE, c);
    }
    // Count the bytes out, and keep the instance leased until the body is sent.
    let counted = Arc::new(AtomicU64::new(0));
    let (parts, body) = resp.into_parts();
    let c = counted.clone();
    // Every frame goes through as it came, trailers too (gRPC's status is one); the lease, the
    // meter's request and the access line go when the body does.
    let done = Done { live: live.clone(), began, status, counted, access: Some((access, instance, request_id, cold, waited_ms)), lease, mirrored, headers_ms: began.elapsed().as_secs_f64() * 1000.0 };
    let frames = http_body_util::BodyStream::new(body).map(move |frame| {
        let _until_sent = &done;
        if let Ok(f) = &frame
            && let Some(d) = f.data_ref()
        {
            c.fetch_add(d.len() as u64, Ordering::Relaxed);
        }
        frame
    });
    let body = Body::new(http_body_util::StreamBody::new(frames));
    if let Some((k, req_headers, app_headers, pol)) = keep {
        // Read whole (at most 1 MiB, its length said so), kept, then sent.
        let bytes = match http_body_util::BodyExt::collect(body).await {
            Ok(c) => c.to_bytes(),
            Err(_) => return page(StatusCode::BAD_GATEWAY, "The service didn't answer", "Its answer broke off. Try again in a moment."),
        };
        apps.responses.put(&cache_env, (&cache_host, &k), &req_headers, &app_headers, bytes.clone(), pol);
        let mut r = Response::from_parts(parts, Body::from(bytes));
        r.headers_mut().insert(HeaderName::from_static("x-roll-cache"), HeaderValue::from_static("MISS"));
        return with_feedback(feedback, r);
    }
    with_feedback(feedback, Response::from_parts(parts, body))
}

/// A stale cached answer fetched again through the proxy (which keeps the new one), boxed: the
/// proxy is what calls it.
fn refresh_cached(apps: Arc<Apps>, service: String, env: String, req: Request, host: String, key: String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
        let _ = proxy(apps.clone(), &service, &env, None, req).await;
        apps.responses.refresh_failed(&crate::env_key(&service, &env), &host, &key);
    })
}

/// What a page gets put in: the environment's snippets, and roll's own scripts (the feedback
/// button for a preview's link visitors, Web Vitals).
#[derive(Clone)]
struct Tags {
    feedback: bool,
    vitals: bool,
    snippets: Option<(bytes::Bytes, bytes::Bytes)>,
}

fn with_feedback(t: Tags, r: Response) -> Response {
    let r = match &t.snippets {
        Some((head, body)) => crate::feedback::snippets(r, head, body),
        None => r,
    };
    let r = if t.feedback { crate::feedback::add(r) } else { r };
    if t.vitals { crate::feedback::tag(r, crate::vitals::TAG, false) } else { r }
}

/// Meters a request when its body is sent (or the client went away).
struct Done {
    live: Arc<crate::live::EnvLive>,
    began: Instant,
    status: u16,
    counted: Arc<AtomicU64>,
    /// Its access line: where it goes, the instance, the request id, a cold start's wait.
    access: Option<(Access, String, String, bool, u64)>,
    lease: crate::live::Lease,
    /// A mirrored copy ([`MIRROR`]): served and metered, never judged by a rollout.
    mirrored: bool,
    /// How long its answer's headers took: what a rollout judges (a stream's body is the app's
    /// pace, or a slow client's).
    headers_ms: f64,
}

impl Drop for Done {
    fn drop(&mut self) {
        let bytes = self.counted.load(Ordering::Relaxed);
        if !self.mirrored {
            self.live.answered(&self.lease.inst, self.status, Some(self.headers_ms));
        }
        self.live.meter.lock().request(code_sql::now_ms(), self.began.elapsed().as_secs_f64() * 1000.0, self.status, bytes);
        if let Some((a, instance, id, cold, waited)) = self.access.take() {
            a.write(self.status, self.began, bytes, &instance, &id, cold, waited);
        }
    }
}

/// A request's access line (`http:<service>/<env>`, Railway's HTTP logs): JSON
/// `{method, path, status, ms, bytes, instance, request_id, cold, waited_ms}`.
struct Access {
    log: crate::logs::LogSink,
    seen: Seen,
    span: Option<ProxySpan>,
    /// The deployment whose instance answered, once a lease names it (a canary's or the live
    /// one's): its line says it, so metrics split by deployment.
    deployment: parking_lot::Mutex<Option<String>>,
}

/// The proxy's own span in a request's trace (traces on, Cloud Run's): it continues the
/// client's `traceparent` or starts a trace, the app's spans are its children, and a cold start's
/// wait is a child of its own (`roll wake`), so a slow first request shows why.
struct ProxySpan {
    sink: crate::logs::LogSink,
    trace: String,
    span: String,
    parent: String,
    start_ns: u64,
}

impl ProxySpan {
    fn start(apps: &Apps, service: &str, env: &str, headers: &axum::http::HeaderMap) -> ProxySpan {
        let start_ns = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
        let sink = apps.logs.sink(crate::traces::key(service, env), crate::logs::Stream::System);
        let (trace, parent) = headers.get("traceparent").and_then(|v| v.to_str().ok()).and_then(parse_traceparent).unwrap_or_else(|| (hex_id(16), String::new()));
        ProxySpan { sink, trace, span: hex_id(8), parent, start_ns }
    }

    fn traceparent(&self) -> String {
        format!("00-{}-{}-01", self.trace, self.span)
    }

    fn end(&self, seen: &Seen, status: u16, began: Instant, instance: &str, cold: bool, waited_ms: u64) {
        let end = self.start_ns + began.elapsed().as_nanos() as u64;
        let mut attrs = vec![
            ("http.request.method".to_string(), seen.method.clone()),
            ("url.path".to_string(), seen.path.clone()),
            ("http.response.status_code".to_string(), status.to_string()),
            ("roll.cold_start".to_string(), cold.to_string()),
        ];
        if !instance.is_empty() {
            attrs.push(("roll.instance".to_string(), instance.to_string()));
        }
        let root = crate::otlp::Span {
            trace: self.trace.clone(),
            span: self.span.clone(),
            parent: self.parent.clone(),
            name: format!("{} {}", seen.method, crate::traffic::route_of(&seen.path)),
            service: "roll".into(),
            start: self.start_ns,
            end,
            error: status >= 500,
            attrs,
        };
        self.sink.line(serde_json::to_string(&root).expect("a span serializes"));
        if cold && waited_ms > 0 {
            let wake = crate::otlp::Span {
                trace: self.trace.clone(),
                span: hex_id(8),
                parent: self.span.clone(),
                name: "roll wake".into(),
                service: "roll".into(),
                start: self.start_ns,
                end: (self.start_ns + waited_ms * 1_000_000).min(end),
                error: false,
                attrs: vec![("roll.instance".to_string(), instance.to_string())],
            };
            self.sink.line(serde_json::to_string(&wake).expect("a span serializes"));
        }
    }
}

/// A W3C `traceparent` (`00-<trace>-<span>-<flags>`): its trace and span ids, when it's one.
fn parse_traceparent(v: &str) -> Option<(String, String)> {
    let mut f = v.trim().split('-');
    let (ver, trace, span, flags) = (f.next()?, f.next()?, f.next()?, f.next()?);
    let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    // Ids of all zeros are invalid; version ff is.
    let id = |s: &str, n: usize| hex(s, n) && s.bytes().any(|b| b != b'0');
    (hex(ver, 2) && ver != "ff" && id(trace, 32) && id(span, 16) && hex(flags, 2)).then(|| (trace.to_string(), span.to_string()))
}

/// A random id of `bytes` bytes, in hex.
fn hex_id(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    getrandom::fill(&mut b).expect("the OS's random numbers");
    hex::encode(b)
}


/// What an access line says of the request: its method and path (no query: it can carry
/// tokens), and who asked (the address the fleet vouches for, the user agent, the host): what an
/// allow list or a firewall rule is written from.
struct Seen {
    method: String,
    path: String,
    ip: Option<String>,
    ua: String,
    host: String,
    /// A browser's page load (its document), for visitor analytics.
    page: bool,
    /// The referring site's host, when it's another site's (never its path: it can carry tokens).
    referrer: Option<String>,
    /// The visitor's country as the edge saw it (Cloudflare's `CF-IPCountry`), when there is one.
    country: Option<String>,
    /// A page view's campaign: its `utm_source`, `utm_medium` and `utm_campaign` (nothing else
    /// of the query, which can carry tokens), when it has one.
    utm: Option<[Option<String>; 3]>,
}

/// A query's UTM campaign parameters, each at most 100 printable characters.
fn utm_of(query: Option<&str>) -> Option<[Option<String>; 3]> {
    let q = query?;
    let mut out: [Option<String>; 3] = Default::default();
    for (k, v) in url::form_urlencoded::parse(q.as_bytes()) {
        let i = match k.as_ref() {
            "utm_source" => 0,
            "utm_medium" => 1,
            "utm_campaign" => 2,
            _ => continue,
        };
        let v: String = v.trim().chars().filter(|c| !c.is_control()).take(100).collect();
        if !v.is_empty() && out[i].is_none() {
            out[i] = Some(v.to_ascii_lowercase());
        }
    }
    out.iter().any(Option::is_some).then_some(out)
}

impl Seen {
    /// What analytics read, on the line only when there's something to say.
    fn mark(&self, line: &mut serde_json::Value) {
        if self.page {
            line["page"] = serde_json::Value::Bool(true);
        }
        if let Some(r) = &self.referrer {
            line["ref"] = r.clone().into();
        }
        if let Some(c) = &self.country {
            line["country"] = c.clone().into();
        }
        if let (true, Some([source, medium, campaign])) = (self.page, &self.utm) {
            line["utm"] = serde_json::json!({ "source": source, "medium": medium, "campaign": campaign });
        }
    }

    fn of(req: &Request) -> Seen {
        let text = |h: header::HeaderName, n: usize| req.headers().get(h).map(|v| String::from_utf8_lossy(v.as_bytes()).chars().take(n).collect()).unwrap_or_default();
        Seen {
            method: req.method().to_string(),
            path: req.uri().path().chars().take(512).collect(),
            ip: crate::live::client_ip(roll_cluster::client_addr(req).as_deref()).map(|ip| ip.to_string()),
            ua: text(header::USER_AGENT, 256),
            host: text(header::HOST, 255),
            page: req.method() == axum::http::Method::GET
                // A cache refresh is roll's own request, not a visitor's page view; nor a mirrored copy.
                && req.extensions().get::<crate::resp_cache::Refresh>().is_none()
                && !req.headers().contains_key(MIRROR)
                && match req.headers().get("sec-fetch-dest").and_then(|v| v.to_str().ok()) {
                    Some(dest) => dest.eq_ignore_ascii_case("document"),
                    None => req.headers().get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|a| a.contains("text/html")),
                },
            referrer: req
                .headers()
                .get(header::REFERER)
                .and_then(|v| v.to_str().ok())
                .and_then(|r| url::Url::parse(r).ok())
                .and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_ascii_lowercase()))
                .filter(|h| {
                    let own = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).map(|h| h.split(':').next().unwrap_or(h).trim_start_matches("www.").to_ascii_lowercase());
                    own.as_deref() != Some(h.as_str()) && h.len() <= 255
                }),
            country: req.headers().get("cf-ipcountry").and_then(|v| v.to_str().ok()).filter(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphabetic()) && *c != "XX" && *c != "T1").map(|c| c.to_ascii_uppercase()),
            utm: utm_of(req.uri().query()),
        }
    }
}

/// Marks an answer roll gave itself before any instance: `by` what (`allow_list`, `firewall`,
/// `rate_limit`, `private`, `maintenance`, `wake`, `capped`, `paused`, `not_deployed`, `moving`,
/// `unavailable`), for [`proxy`] to log and meter.
#[derive(Clone, Copy)]
struct By(&'static str);

fn by(why: &'static str, mut r: Response) -> Response {
    r.extensions_mut().insert(By(why));
    r
}

impl Access {
    #[allow(clippy::too_many_arguments)]
    fn write(&self, status: u16, began: Instant, bytes: u64, instance: &str, request_id: &str, cold: bool, waited_ms: u64) {
        let s = &self.seen;
        let mut line = serde_json::json!({
            "method": s.method, "path": s.path, "status": status, "ms": began.elapsed().as_millis() as u64,
            "bytes": bytes, "instance": instance, "request_id": request_id, "cold": cold, "waited_ms": waited_ms,
            "ip": s.ip, "ua": s.ua, "host": s.host,
        });
        if let Some(d) = self.deployment.lock().as_deref() {
            line["deployment"] = serde_json::json!(d);
        }
        s.mark(&mut line);
        self.log.line(line.to_string());
        if let Some(p) = &self.span {
            p.end(s, status, began, instance, cold, waited_ms);
        }
    }

    /// An answer from the cache: no instance saw it, its bytes still went out.
    fn write_cached(&self, status: u16, began: Instant, bytes: u64) {
        let s = &self.seen;
        let mut line = serde_json::json!({
            "method": s.method, "path": s.path, "status": status, "ms": began.elapsed().as_millis() as u64,
            "bytes": bytes, "by": "cache", "ip": s.ip, "ua": s.ua, "host": s.host,
        });
        s.mark(&mut line);
        self.log.line(line.to_string());
    }

    /// An answer roll gave itself (no instance saw the request).
    fn write_by(&self, status: u16, began: Instant, by: &str) {
        let s = &self.seen;
        let mut line = serde_json::json!({
            "method": s.method, "path": s.path, "status": status, "ms": began.elapsed().as_millis() as u64,
            "bytes": 0, "by": by, "ip": s.ip, "ua": s.ua, "host": s.host,
        });
        s.mark(&mut line);
        self.log.line(line.to_string());
    }
}

/// Whether anything accepts a connection at `addr` within a second (the kernel's answer, not
/// the app's: a busy app still accepts).
async fn accepts(addr: std::net::SocketAddr) -> bool {
    matches!(tokio::time::timeout(std::time::Duration::from_secs(1), tokio::net::TcpStream::connect(addr)).await, Ok(Ok(_)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_traceparent_is_continued_only_when_it_is_one() {
        use super::parse_traceparent as p;
        let ids = Some(("4bf92f3577b34da6a3ce929d0e0e4736".to_string(), "00f067aa0ba902b7".to_string()));
        assert_eq!(p("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"), ids);
        for bad in ["ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01", "00-00000000000000000000000000000000-00f067aa0ba902b7-01", "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01", "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01", "00-4bf92f35-00f067aa0ba902b7-01", "garbage"] {
            assert_eq!(p(bad), None, "{bad}");
        }
    }

    #[test]
    fn an_encoding_is_taken_by_its_own_q_and_star_only_for_the_rest() {
        use super::accept_q as q;
        assert_eq!((q(Some("gzip, br"), "br"), q(Some("gzip, br"), "gzip")), (1.0, 1.0));
        assert_eq!(q(Some("gzip;q=0, *"), "gzip"), 0.0, "named and refused: * doesn't bring it back");
        assert_eq!(q(Some("gzip;q=0, *"), "br"), 1.0);
        assert_eq!(q(Some("gzip;Q=0"), "gzip"), 0.0);
        assert_eq!(q(Some("gzip;q=1, br;q=0.1"), "br"), 0.1);
        assert_eq!((q(None, "gzip"), q(Some("identity"), "gzip")), (0.0, 0.0));
    }

    #[test]
    fn one_byte_range_is_met_and_the_rest_is_the_whole_file() {
        use super::byte_range as r;
        assert_eq!(r("bytes=0-99", 1000), Some(Ok((0, 99))));
        assert_eq!(r("bytes=900-", 1000), Some(Ok((900, 999))));
        assert_eq!(r("bytes=990-5000", 1000), Some(Ok((990, 999))));
        assert_eq!(r("bytes=-100", 1000), Some(Ok((900, 999))));
        assert_eq!(r("bytes=-5000", 1000), Some(Ok((0, 999))));
        assert_eq!(r("bytes=1000-", 1000), Some(Err(())));
        assert_eq!(r("bytes=-0", 1000), Some(Err(())));
        assert_eq!(r("bytes=0-", 0), Some(Err(())));
        for whole in ["bytes=0-1,5-6", "bytes=5-1", "items=0-1", "bytes=-", "bytes=a-b", "bytes=+1-2", "bytes=99999999999999999999-"] {
            assert_eq!(r(whole, 1000), None, "{whole}");
        }
    }

    #[test]
    fn the_pages_mark_is_the_brands_export() {
        let brand = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/brand/assets/horizon-dark.svg");
        let Ok(svg) = std::fs::read_to_string(brand) else { return };
        assert_eq!(super::HORIZON.trim(), svg.trim().replace("fill=\"#0a0a0a\"", "fill=\"currentColor\""), "regenerate crates/apps-control/assets/horizon.svg from @roll/brand");
    }
}
