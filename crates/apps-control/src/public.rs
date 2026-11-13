//! Public traffic (docs/apps/http-api.md, "Public traffic"): requests for a service's hostname,
//! custom domain or local port, handed to the service's owner and through the waking proxy to
//! an instance.

use crate::live::WakeError;
use crate::{Apps, lease_key};
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
    axum::Router::new().fallback(move |req: Request| {
        let apps = apps.clone();
        async move {
            let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("").to_string();
            match apps.env_for_host(&host).await {
                Some((service, env)) => serve(apps, service, env, req).await,
                None => page(StatusCode::NOT_FOUND, "Nothing here", "No roll apps service answers on this address."),
            }
        }
    })
}

/// A listener that's one environment's own port (local development without wildcard DNS).
pub fn port_router(apps: Arc<Apps>, service: String, env: String) -> axum::Router {
    axum::Router::new().fallback(move |req: Request| {
        let (apps, service, env) = (apps.clone(), service.clone(), env.clone());
        async move { serve(apps, service, env, req).await }
    })
}

/// Run the request where the service lives.
pub async fn serve(apps: Arc<Apps>, service: String, env: String, req: Request) -> Response {
    let cluster = apps.deps.cluster.clone();
    cluster
        .dispatch(&lease_key(&service), req, move |req, _guard| async move { proxy(apps, &service, &env, req).await })
        .await
}

fn page(status: StatusCode, title: &str, text: &str) -> Response {
    let html = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><title>{title}</title>\
         <style>body{{margin:0;min-height:100vh;display:grid;place-items:center;background:#000;color:#ededed;font:14px system-ui,sans-serif}}\
         div{{max-width:420px;padding:24px}}h1{{font-size:18px;font-weight:600;letter-spacing:-.02em;margin:0 0 8px}}p{{color:#a1a1a1;margin:0}}</style>\
         <div><h1>{title}</h1><p>{text}</p></div>"
    );
    let mut r = (status, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response();
    if status == StatusCode::SERVICE_UNAVAILABLE {
        r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
    }
    r
}

/// The stored form of an environment's access token: its SHA-256, hex. The token itself is only
/// ever in the visitor's cookie.
pub fn token_hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether two hashes are equal, in time that doesn't depend on where they differ.
fn same(a: &str, b: &str) -> bool {
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
    let hash = access.token_hash;
    // An automation (CI's end-to-end tests) with the project's bypass secret: no cookie, no
    // redirect. The header is roll's, so it never reaches the app (`x-roll-*` is stripped).
    if let (Some(bypass), Some(given)) = (&access.bypass_hash, req.headers().get("x-roll-preview-bypass").and_then(|v| v.to_str().ok()))
        && same(&token_hash(given), bypass)
    {
        return None;
    }
    let name = format!("roll_access_{env}");
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
        // One leading slash: `//host/x` would send the visitor to another site.
        let path = format!("/{}", req.uri().path().trim_start_matches('/'));
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
        return Some(denied(apps, service, env).await);
    }
    req.headers_mut().remove(header::COOKIE);
    if !kept.is_empty()
        && let Ok(v) = HeaderValue::from_str(&kept.join("; "))
    {
        req.headers_mut().insert(header::COOKIE, v);
    }
    None
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
        Some(url) => page(StatusCode::UNAUTHORIZED, "This preview is private", &format!("<a href=\"{url}\" style=\"color:#4c8dff\">Open it through roll</a>: sign in, and you're sent on with its link.")),
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
async fn proxy(apps: Arc<Apps>, service: &str, env: &str, mut req: Request) -> Response {
    let began = Instant::now();
    // A private environment answers only visitors with its token, before anything wakes.
    if let Some(turned_away) = check_access(&apps, service, env, &mut req).await {
        return turned_away;
    }
    let live = match apps.load_live(service, env).await {
        Ok(l) => l,
        Err(e) => return page(StatusCode::NOT_FOUND, "Not deployed", &e.to_string()),
    };
    let lease = match live.acquire().await {
        Ok(l) => l,
        Err(WakeError::NoDeployment) => return page(StatusCode::NOT_FOUND, "Not deployed yet", "This service has no deployment in this environment yet."),
        Err(WakeError::NotWeb) => return page(StatusCode::NOT_FOUND, "Not a web service", "This service is a worker or a cron job: it doesn't answer HTTP."),
        Err(WakeError::Busy) => return retry_soon(page(StatusCode::SERVICE_UNAVAILABLE, "Busy starting", "Too many requests are waiting for this service to start. Try again in a moment.")),
        Err(WakeError::Capped) => return page(StatusCode::PAYMENT_REQUIRED, "Paused", "This project reached its spending limit for the month. Its owner can raise the limit to bring it back."),
        Err(e) => {
            live.log.stream(crate::logs::Stream::System).line(format!("a request couldn't wake the service: {e}"));
            return page(StatusCode::SERVICE_UNAVAILABLE, "Starting up", "The service is waking up or failing to start. Try again in a few seconds.");
        }
    };
    let pq = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
    let Ok(uri) = format!("http://{}{pq}", lease.addr).parse::<Uri>() else {
        return (StatusCode::BAD_REQUEST, "bad request").into_response();
    };
    let upgrade = is_upgrade(req.headers());
    let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut req));
    let host = req.headers().get(header::HOST).cloned();
    let client = roll_cluster::client_addr(&req)
        .or_else(|| req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip().to_string()));
    *req.uri_mut() = uri;
    *req.version_mut() = axum::http::Version::HTTP_11;
    strip_hop_by_hop(req.headers_mut(), upgrade);
    // Nothing a client sends may look like it came from roll.
    let roll_headers: Vec<HeaderName> = req.headers().keys().filter(|k| k.as_str().starts_with("x-roll-")).cloned().collect();
    for k in roll_headers {
        req.headers_mut().remove(k);
    }
    let h = req.headers_mut();
    if let Some(host) = host {
        h.insert(HeaderName::from_static("x-forwarded-host"), host);
    }
    let proto = if apps.cfg.scheme == "https" { "https" } else { "http" };
    h.insert(HeaderName::from_static("x-forwarded-proto"), HeaderValue::from_static(proto));
    if let Some(c) = client.and_then(|c| HeaderValue::from_str(&c).ok()) {
        h.insert(HeaderName::from_static("x-forwarded-for"), c);
    }
    if !h.contains_key("x-request-id")
        && let Ok(v) = HeaderValue::from_str(&crate::model::random(16))
    {
        h.insert(HeaderName::from_static("x-request-id"), v);
    }
    let mut resp = match apps.client.request(req).await {
        Ok(r) => r,
        Err(e) if e.is_connect() => {
            // Nothing listens any more (killed, out of memory): the next request starts a
            // fresh instance.
            live.connect_failed(&lease.inst);
            live.meter.lock().request(code_sql::now_ms(), began.elapsed().as_secs_f64() * 1000.0, 503, 0);
            return retry_soon(page(StatusCode::SERVICE_UNAVAILABLE, "Restarting", "The instance stopped; the next request starts it again."));
        }
        Err(e) => {
            live.meter.lock().request(code_sql::now_ms(), began.elapsed().as_secs_f64() * 1000.0, 502, 0);
            return page(StatusCode::BAD_GATEWAY, "The service didn't answer", &format!("The instance closed the connection: {e}"));
        }
    };
    let status = resp.status().as_u16();
    strip_roll_headers(resp.headers_mut());
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
            }
        });
        return resp.map(Body::new);
    }
    strip_hop_by_hop(resp.headers_mut(), false);
    // Count the bytes out, and keep the instance leased until the body is sent.
    let counted = Arc::new(AtomicU64::new(0));
    let (parts, body) = resp.into_parts();
    let c = counted.clone();
    let stream = http_body_util::BodyStream::new(body).filter_map(move |frame| {
        let c = c.clone();
        async move {
            match frame {
                Ok(f) => f.into_data().ok().map(|d| {
                    c.fetch_add(d.len() as u64, Ordering::Relaxed);
                    Ok::<_, hyper::Error>(d)
                }),
                Err(e) => Some(Err(e)),
            }
        }
    });
    let done = Done { live: live.clone(), began, status, counted, _lease: lease };
    let stream = stream.chain(futures::stream::once(async move {
        drop(done);
        Ok(bytes::Bytes::new())
    }));
    Response::from_parts(parts, Body::from_stream(stream))
}

/// Meters a request when its body is sent (or the client went away).
struct Done {
    live: Arc<crate::live::EnvLive>,
    began: Instant,
    status: u16,
    counted: Arc<AtomicU64>,
    _lease: crate::live::Lease,
}

impl Drop for Done {
    fn drop(&mut self) {
        self.live.meter.lock().request(code_sql::now_ms(), self.began.elapsed().as_secs_f64() * 1000.0, self.status, self.counted.load(Ordering::Relaxed));
    }
}
