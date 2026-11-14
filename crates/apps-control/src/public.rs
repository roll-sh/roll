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
            let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("").to_string();
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
                Some((service, env, deployment)) => serve(apps, service, env, deployment, req).await,
                None => page(StatusCode::NOT_FOUND, "Nothing here", "No roll apps service answers on this address."),
            }
        }
    })
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

/// roll's own request headers, off a visitor's request where it comes in: from here on, one is
/// roll's ([`VIA_SITE`] lets a rewrite past its target's access check).
fn strip_ours(req: &mut Request) {
    for h in [VIA_SITE, TARGET, ORIGINAL] {
        req.headers_mut().remove(h);
    }
}

/// Run the request where the service lives (`deployment`: one deployment's own URL). A forward
/// to its owner goes to the owner's own route for it ([`INTERNAL`], signed by the cluster, the
/// service and the visitor's path in headers), never as a call to its API: an app's paths aren't
/// the API's, and only a node can send to that route.
pub async fn serve(apps: Arc<Apps>, service: String, env: String, deployment: Option<String>, req: Request) -> Response {
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
const HORIZON: &str = include_str!("../assets/horizon.svg");

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
fn site_rewrite(apps: &Arc<Apps>, tpl: &crate::live::Template, req: &Request) -> impl std::future::Future<Output = Option<(String, String)>> + Send + use<> {
    // Read off the request here: the future never holds it.
    let args = (!req.headers().contains_key(VIA_SITE)).then(|| tpl.site.clone()).flatten().map(|key| {
        (key, percent_encoding::percent_decode_str(req.uri().path()).decode_utf8_lossy().into_owned(), req.uri().query().map(str::to_string))
    });
    let apps = apps.clone();
    async move {
        let (key, path, query) = args?;
        rewrite_of(&apps, key, path, query).await
    }
}

async fn rewrite_of(apps: &Arc<Apps>, key: String, path: String, query: Option<String>) -> Option<(String, String)> {
    let site = apps.site(&key).await.ok()?;
    match site.rules.redirect(&path, query.as_deref(), crate::sites::exists(&site, &path))? {
        crate::site_rules::Outcome::Proxy { service, path } => Some((service, path)),
        _ => None,
    }
}

/// Marks a request a site's rewrite sent on (roll's own: `x-roll-*` never reaches an app, and a
/// visitor's is taken off where it came in, [`strip_ours`]).
const VIA_SITE: &str = "x-roll-via-site";

/// rolld's internal API (where [`Cluster::dispatch`](roll_cluster::Cluster::dispatch) forwards to
/// a service's owner) with the route roll apps' public traffic is forwarded to ([`INTERNAL`]):
/// only a request another node signed gets in (`require_signed`), and it's served as the visitor
/// sent it, to the service it names.
pub fn with_forwarded(api: axum::Router, apps: Arc<Apps>) -> axum::Router {
    let cluster = apps.deps.cluster.clone();
    let route = axum::Router::new()
        .route(INTERNAL, axum::routing::any(forwarded_public))
        .with_state(apps)
        .layer(axum::middleware::from_fn_with_state(cluster, roll_cluster::require_signed));
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
enum RollPage {
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

fn escape(s: &str) -> String {
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
}

impl SiteRequest {
    fn of(req: &Request) -> SiteRequest {
        let h = |n: &str| req.headers().get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
        SiteRequest { method: req.method().clone(), path: req.uri().path().to_string(), query: req.uri().query().map(str::to_string), if_none_match: h("if-none-match"), accept_encoding: h("accept-encoding"), range: h("range"), if_range: h("if-range"), request_id: h("x-request-id") }
    }
}

async fn serve_site(apps: &Arc<Apps>, live: &Arc<crate::live::EnvLive>, tpl: &crate::live::Template, req: SiteRequest, service: &str, env: &str, began: Instant) -> Response {
    let key = tpl.site.clone().unwrap_or_default();
    let access = Access { log: apps.logs.sink(format!("http:{service}/{env}"), crate::logs::Stream::Http), method: req.method.to_string(), path: req.path.chars().take(512).collect() };
    let request_id = req.request_id.as_deref().map(|v| v.chars().take(64).collect()).unwrap_or_else(|| crate::model::random(16));
    let done = |status: u16, bytes: u64| {
        live.meter.lock().request(code_sql::now_ms(), began.elapsed().as_secs_f64() * 1000.0, status, bytes);
        access.write(status, began, bytes, "site", &request_id, false, 0);
    };
    let head = req.method == axum::http::Method::HEAD;
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
    let resolved = match site.rules.redirect(&path, req.query.as_deref(), own) {
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
        Some(crate::site_rules::Outcome::Proxy { .. }) => {
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
async fn proxy(apps: Arc<Apps>, service: &str, env: &str, deployment: Option<&str>, req: Request) -> Response {
    let r = proxy_inner(apps.clone(), service, env, deployment, req).await;
    their_page(&apps, service, env, r).await
}

async fn proxy_inner(apps: Arc<Apps>, service: &str, env: &str, deployment: Option<&str>, mut req: Request) -> Response {
    let began = Instant::now();
    // Came in through roll: the private environment's access cookie (checked just below).
    let cookie = format!("roll_access_{env}=");
    let via_link = env.starts_with("pv-") && req.headers().get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).any(|v| v.split(';').any(|c| c.trim().starts_with(&cookie)));
    // Its allow list first (deployment URLs too: they're public), before anything wakes.
    let gate = match apps.gate(&apps.live(service, env)).await {
        Ok(g) => g,
        Err(_) => return page(StatusCode::SERVICE_UNAVAILABLE, "Unavailable", "This environment's settings couldn't be read. Try again in a few seconds."),
    };
    let client = roll_cluster::client_addr(&req);
    if !gate.admits(client.as_deref()) {
        return page(StatusCode::FORBIDDEN, "Not from here", "This service only answers visitors from the networks its owner allowed.");
    }
    // Its firewall rules (a 403 that says nothing of them).
    let ua = req.headers().get(header::USER_AGENT).map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()).unwrap_or_default();
    if !gate.firewall.is_empty() && !gate.firewall_allows(req.method().as_str(), &crate::model::normal_path(req.uri().path()), &ua) {
        return page(StatusCode::FORBIDDEN, "Not allowed", "This service doesn't answer this request.");
    }
    // Its rate limit per client, before anything wakes.
    if let Some(limit) = &gate.rate
        && let Err(wait) = apps.live(service, env).take_token(crate::live::client_ip(client.as_deref()), limit)
    {
        let mut r = page(StatusCode::TOO_MANY_REQUESTS, "Slow down", "Too many requests from your address. Try again in a moment.");
        if let Ok(v) = HeaderValue::from_str(&(wait.as_secs_f64().ceil() as u64).max(1).to_string()) {
            r.headers_mut().insert(header::RETRY_AFTER, v);
        }
        return r;
    }
    // A private environment answers only visitors with its token, before anything wakes. A
    // site's rewrite (a header only roll sets) was let in by the site, by the project's own rule.
    let rewritten = req.headers().contains_key(VIA_SITE);
    if !rewritten && let Some(turned_away) = check_access(&apps, service, env, &mut req).await {
        return turned_away;
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
    let feedback = target.is_some() && page_load;
    if feedback {
        // The page as the app wrote it, so the tag can go in as it streams.
        req.headers_mut().insert(header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    }
    let live = match apps.load_live(service, env).await {
        Ok(l) => l,
        Err(Error::Unavailable(_)) => return retry_soon(page(StatusCode::SERVICE_UNAVAILABLE, "Starting up", "The service is waking up. Try again in a moment.")),
        Err(e) => return page(StatusCode::NOT_FOUND, "Not deployed", &e.to_string()),
    };
    // In maintenance: roll's page, nothing woken. A deployment's own URL still answers (a fix
    // can be tried before maintenance ends).
    if deployment.is_none()
        && let Some(m) = &gate.maintenance
    {
        return maintenance_page(m);
    }
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
            return rewrite_to(apps, service, env, to, req).await;
        }
    }
    // A gradual rollout keeps each visitor on one deployment: their bucket, from the cookie it
    // gave them (never passed to the app), or a new one while a canary runs.
    let mut bucket = take_bucket(&mut req);
    let affine = take_cookie(&mut req, AFFINITY);
    let fresh = deployment.is_none() && bucket.is_none() && live.canary().is_some();
    if fresh {
        let mut b = [0u8; 1];
        getrandom::fill(&mut b).expect("system randomness");
        bucket = Some(b[0] % 100);
    }
    // How the lease was had, to have another if its instance turns out gone.
    let mut again: Option<Again> = None;
    let acquired = match deployment {
        // One deployment's own URL: that deployment, whatever is live.
        Some(d) => match apps.deployment_template(service, env, d).await {
            Ok(t) if t.site.is_some() => return with_feedback(feedback, serve_site(&apps, &live, &t, SiteRequest::of(&req), service, env, began).await),
            Ok(t) => {
                again = Some(Again::Deployment(Box::new(t.clone())));
                live.acquire_deployment(t).await
            }
            Err(e) => return page(StatusCode::NOT_FOUND, "No such deployment", &e.to_string()),
        },
        None => match live.may_wake().map(|_| live.route(bucket)) {
            // A static site's files answer (a gradual rollout's bucket chose which deployment's).
            Ok(Some(t)) if t.site.is_some() => {
                let mut r = with_feedback(feedback, serve_site(&apps, &live, &t, SiteRequest::of(&req), service, env, began).await);
                if let (true, Some(b)) = (fresh, bucket) {
                    let secure = if apps.cfg.scheme == "https" { "; Secure" } else { "" };
                    if let Ok(c) = HeaderValue::from_str(&format!("{BUCKET}={b}; Path=/; Max-Age=86400; HttpOnly; SameSite=Lax{secure}")) {
                        r.headers_mut().append(header::SET_COOKIE, c);
                    }
                }
                return r;
            }
            Ok(Some(t)) => {
                again = Some(Again::Route(t.clone()));
                live.acquire_preferring(t, affine.as_deref()).await
            }
            Ok(None) => Err(WakeError::NoDeployment),
            Err(e) => Err(e),
        },
    };
    let mut lease = match acquired {
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
    // The access line's method and path (no query: it can carry tokens).
    let access = Access { log: apps.logs.sink(format!("http:{service}/{env}"), crate::logs::Stream::Http), method: req.method().to_string(), path: req.uri().path().chars().take(512).collect() };
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
    let send = async { if h2 { apps.client_h2.request(req).await } else { apps.client.request(req).await } };
    // Its timeout runs to the answer's head: an upgrade, or a stream that has begun, is the
    // app's to keep.
    let sent = match lease.timeout.filter(|_| !upgrade) {
        Some(t) => match tokio::time::timeout(t, send).await {
            Ok(r) => r,
            Err(_) => {
                live.answered(&lease.inst, 504);
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
                    let mut r = Request::new(Body::empty());
                    (*r.method_mut(), *r.headers_mut(), *r.version_mut()) = (method, headers, version);
                    *r.uri_mut() = format!("http://{}{pq}", lease.addr).parse().unwrap_or_default();
                    let send = async { if h2 { apps.client_h2.request(r).await } else { apps.client.request(r).await } };
                    match lease.timeout {
                        Some(t) => match tokio::time::timeout(t, send).await {
                            Ok(r) => r,
                            Err(_) => {
                                live.answered(&lease.inst, 504);
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
            live.answered(&lease.inst, 503);
            live.meter.lock().request(code_sql::now_ms(), began.elapsed().as_secs_f64() * 1000.0, 503, 0);
            access.write(503, began, 0, &instance, &request_id, cold, waited_ms);
            return retry_soon(page(StatusCode::SERVICE_UNAVAILABLE, "Restarting", "The instance stopped; the next request starts it again."));
        }
        Err(e) => {
            live.answered(&lease.inst, 502);
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
    // Count the bytes out, and keep the instance leased until the body is sent.
    let counted = Arc::new(AtomicU64::new(0));
    let (parts, body) = resp.into_parts();
    let c = counted.clone();
    // Every frame goes through as it came, trailers too (gRPC's status is one); the lease, the
    // meter's request and the access line go when the body does.
    let done = Done { live: live.clone(), began, status, counted, access: Some((access, instance, request_id, cold, waited_ms)), lease };
    let frames = http_body_util::BodyStream::new(body).map(move |frame| {
        let _until_sent = &done;
        if let Ok(f) = &frame
            && let Some(d) = f.data_ref()
        {
            c.fetch_add(d.len() as u64, Ordering::Relaxed);
        }
        frame
    });
    with_feedback(feedback, Response::from_parts(parts, Body::new(http_body_util::StreamBody::new(frames))))
}

fn with_feedback(on: bool, r: Response) -> Response {
    if on { crate::feedback::add(r) } else { r }
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
}

impl Drop for Done {
    fn drop(&mut self) {
        let bytes = self.counted.load(Ordering::Relaxed);
        self.live.answered(&self.lease.inst, self.status);
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
    method: String,
    path: String,
}

impl Access {
    #[allow(clippy::too_many_arguments)]
    fn write(&self, status: u16, began: Instant, bytes: u64, instance: &str, request_id: &str, cold: bool, waited_ms: u64) {
        let line = serde_json::json!({
            "method": self.method, "path": self.path, "status": status, "ms": began.elapsed().as_millis() as u64,
            "bytes": bytes, "instance": instance, "request_id": request_id, "cold": cold, "waited_ms": waited_ms,
        });
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
