//! Services as containers (until roll apps provides [`Apps`]): code-preview's [`Isolation`]
//! (Docker here, a Fargate task on the fleet). A deploy costs nothing; the first request to a
//! service's host starts its container, the request waits until it answers, and after the
//! project's `sleep_after` without requests it stops again.

use crate::engine::Engine;
use crate::ports::{Apps, EnvSource, Target};
use crate::{ServiceState, Store};
use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::StreamExt;
use code_preview::isolation::{Isolation, Job, Kind as JobKind};
use code_preview::preview::{answers, is_upgrade, page, strip_hop_by_hop};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tracing::{info, warn};

/// Puts a revision's tree into a prepared job's container (roll code's pooled work trees).
#[async_trait]
pub trait Trees: Send + Sync {
    async fn prepare(&self, iso: &dyn Isolation, job: &Job, repo: &str, revision: &str) -> anyhow::Result<()>;
    /// Run a check-like job on a revision's tree (seeds): a pooled work tree, leased while the
    /// job's container is made.
    async fn run(&self, iso: &Arc<dyn Isolation>, spec: &code_preview::config::CheckSpec, repo: &str, revision: &str, env: &[(String, String)]) -> anyhow::Result<code_preview::runner::RunOutput>;
}

#[derive(Clone, Debug, PartialEq)]
enum Phase {
    Stopped,
    Starting,
    Running { addr: SocketAddr },
    Failed(String),
}

struct Live {
    phase: watch::Sender<Phase>,
    /// Bumped by every stop: a start that began before it must not leave a container running.
    stops: std::sync::atomic::AtomicU64,
    container: Mutex<Option<String>>,
    last_request: Mutex<Instant>,
    started: Mutex<Option<Instant>>,
    sleep_after: Mutex<Duration>,
}

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    /// Services answer on `<host label>.<domain>`.
    pub domain: String,
    pub scheme: String,
    /// How long a request waits for a starting service before a "starting" page.
    pub wait: Duration,
    /// How long a service may take to answer before it counts as failed.
    pub start_timeout: Duration,
    /// The dashboard (`https://roll.example`): a private preview's 401 page links there, where a
    /// signed-in person is sent on with the preview's link.
    pub dashboard: Option<String>,
    /// A headless browser for screenshots: an image, and a shell command in it that writes a
    /// PNG of `$URL` to `$OUT` (`chrome --headless --screenshot`).
    pub screenshot_image: String,
    pub screenshot_command: String,
}

/// Playwright's image has a headless Chromium shell (its path names its version).
pub const SCREENSHOT_IMAGE: &str = "mcr.microsoft.com/playwright:v1.64.1-noble";
pub const SCREENSHOT_COMMAND: &str = r#""$(ls -d /ms-playwright/chromium_headless_shell-*/chrome-headless-shell-linux64/chrome-headless-shell | head -1)" --no-sandbox --hide-scrollbars --window-size=1280,800 --screenshot="$OUT" "$URL""#;

impl Default for RuntimeConfig {
    fn default() -> Self {
        RuntimeConfig {
            domain: "preview.localhost".into(),
            scheme: "http".into(),
            wait: Duration::from_secs(60),
            start_timeout: Duration::from_secs(600),
            dashboard: None,
            screenshot_image: SCREENSHOT_IMAGE.into(),
            screenshot_command: SCREENSHOT_COMMAND.into(),
        }
    }
}

pub struct Runtime {
    cfg: RuntimeConfig,
    iso: Arc<dyn Isolation>,
    trees: Arc<dyn Trees>,
    store: Arc<Store>,
    engine: OnceLock<Weak<Engine>>,
    /// In a cluster, a preview's services run on the holder of its lease (`preview:<id>`):
    /// requests for their hosts are forwarded there.
    cluster: OnceLock<Arc<roll_cluster::Cluster>>,
    me: Weak<Runtime>,
    live: Mutex<HashMap<(String, String), Arc<Live>>>,
    client: Client<HttpConnector, Body>,
}

/// A repo's cache volume.
pub fn cache_name(repo: &str) -> String {
    format!("roll-previews-cache-{}", crate::label_part(repo, 40))
}

/// The container of a service: every one roll previews makes starts with `roll-previews-`.
pub fn container_name(host: &str) -> String {
    format!("roll-previews-{host}")
}

impl Runtime {
    pub fn new(cfg: RuntimeConfig, iso: Arc<dyn Isolation>, trees: Arc<dyn Trees>, store: Arc<Store>) -> Arc<Runtime> {
        Arc::new_cyclic(|me| Runtime {
            cfg,
            iso,
            trees,
            store,
            engine: OnceLock::new(),
            cluster: OnceLock::new(),
            me: me.clone(),
            live: Mutex::new(HashMap::new()),
            client: Client::builder(TokioExecutor::new()).build_http(),
        })
    }

    /// Connect to the engine (which builds services' environments) and start the idle reaper.
    pub fn attach(self: &Arc<Self>, engine: &Arc<Engine>) {
        let _ = self.engine.set(Arc::downgrade(engine));
        let this = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(15));
            tick.tick().await;
            loop {
                tick.tick().await;
                let Some(rt) = this.upgrade() else { return };
                rt.reap().await;
            }
        });
    }

    /// Serve preview hosts on their preview's holder, and stop a preview's containers here when
    /// its lease moves to another node.
    pub fn set_cluster(self: &Arc<Self>, cluster: Arc<roll_cluster::Cluster>) {
        let mut ownership = cluster.subscribe_ownership();
        let _ = self.cluster.set(cluster);
        let this = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(o) = ownership.recv().await {
                let roll_cluster::Ownership::Lost { key } = o else { continue };
                let Some(preview) = key.strip_prefix("preview:") else { continue };
                let Some(rt) = this.upgrade() else { return };
                let lives: Vec<((String, String), Arc<Live>)> = rt.live.lock().iter().filter(|((p, _), _)| p == preview).map(|(k, l)| (k.clone(), l.clone())).collect();
                for ((p, s), live) in lives {
                    info!(preview = %p, service = %s, "the preview moved to another node; stopping its service here");
                    rt.stop(&p, &s, &live, None).await;
                    rt.live.lock().remove(&(p, s));
                }
            }
        });
    }

    fn engine(&self) -> anyhow::Result<Arc<Engine>> {
        self.engine.get().and_then(Weak::upgrade).ok_or_else(|| anyhow::anyhow!("the preview runtime isn't attached"))
    }

    fn live(&self, preview: &str, service: &str) -> Arc<Live> {
        self.live
            .lock()
            .entry((preview.to_string(), service.to_string()))
            .or_insert_with(|| {
                Arc::new(Live {
                    phase: watch::channel(Phase::Stopped).0,
                    stops: std::sync::atomic::AtomicU64::new(0),
                    container: Mutex::new(None),
                    last_request: Mutex::new(Instant::now()),
                    started: Mutex::new(None),
                    sleep_after: Mutex::new(Duration::from_secs(600)),
                })
            })
            .clone()
    }

    async fn stop(&self, preview: &str, service: &str, live: &Live, why: Option<String>) {
        live.stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let name = live.container.lock().take();
        if let Some(name) = name {
            self.iso.release(&name).await;
        }
        let ran = live.started.lock().take().map(|t| t.elapsed().as_millis() as i64).unwrap_or(0);
        let was_running = matches!(*live.phase.borrow(), Phase::Running { .. } | Phase::Starting);
        live.phase.send_replace(Phase::Stopped);
        if (was_running || ran > 0)
            && let Ok(e) = self.engine()
        {
            e.observed(preview, service, ServiceState::Sleeping, why, ran).await;
        }
    }

    async fn reap(&self) {
        let idle: Vec<((String, String), Arc<Live>)> = self
            .live
            .lock()
            .iter()
            .filter(|(_, l)| matches!(*l.phase.borrow(), Phase::Running { .. }) && l.last_request.lock().elapsed() > *l.sleep_after.lock())
            .map(|(k, l)| (k.clone(), l.clone()))
            .collect();
        for ((p, s), live) in idle {
            info!(preview = %p, service = %s, "preview service idle; stopping");
            self.stop(&p, &s, &live, None).await;
        }
    }

    /// Where a service answers, starting it if needed; waits up to `wait`.
    async fn ready(self: &Arc<Self>, preview: &str, service: &str, wait: Duration) -> Result<SocketAddr, Unavailable> {
        let live = self.live(preview, service);
        *live.last_request.lock() = Instant::now();
        if let Ok(e) = self.engine() {
            e.touched(preview);
        }
        let mut rx = live.phase.subscribe();
        // Clone first: a borrow held through the match would deadlock `send_replace`.
        let phase = rx.borrow_and_update().clone();
        let start = match phase {
            Phase::Running { addr } => return Ok(addr),
            Phase::Starting => false,
            Phase::Stopped | Phase::Failed(_) => {
                live.phase.send_replace(Phase::Starting);
                true
            }
        };
        if start {
            let (this, p, s, l) = (self.clone(), preview.to_string(), service.to_string(), live.clone());
            tokio::spawn(async move { this.start(&p, &s, &l).await });
        }
        let waited = tokio::time::timeout(wait, async {
            loop {
                if rx.changed().await.is_err() {
                    return Err(Unavailable::NotFound);
                }
                match rx.borrow_and_update().clone() {
                    Phase::Running { addr } => return Ok(addr),
                    Phase::Failed(e) => return Err(Unavailable::Failed(e)),
                    Phase::Stopped => return Err(Unavailable::NotFound),
                    Phase::Starting => {}
                }
            }
        });
        waited.await.unwrap_or(Err(Unavailable::Starting))
    }

    async fn start(&self, preview: &str, service: &str, live: &Live) {
        match tokio::time::timeout(self.cfg.start_timeout, self.start_inner(preview, service, live)).await {
            Ok(Ok(addr)) => {
                *live.started.lock() = Some(Instant::now());
                live.phase.send_replace(Phase::Running { addr });
                if let Ok(e) = self.engine() {
                    e.observed(preview, service, ServiceState::Running, None, 0).await;
                }
            }
            failed => {
                let e = match failed {
                    Ok(Err(e)) => format!("{e:#}"),
                    _ => format!("didn't answer within {}s", self.cfg.start_timeout.as_secs()),
                };
                let container = live.container.lock().clone();
                let tail = match container {
                    Some(name) => self.iso.logs_tail(&name, 30).await,
                    None => String::new(),
                };
                let name = live.container.lock().take();
                if let Some(name) = name {
                    self.iso.release(&name).await;
                }
                live.started.lock().take();
                let error = if tail.trim().is_empty() { e } else { format!("{e}\n\n{}", tail.trim_end()) };
                warn!(%preview, %service, error = %error.lines().next().unwrap_or(""), "preview service failed to start");
                live.phase.send_replace(Phase::Failed(error.clone()));
                if let Ok(en) = self.engine() {
                    en.observed(preview, service, ServiceState::Failed, Some(error.chars().take(4000).collect()), 0).await;
                }
            }
        }
    }

    async fn start_inner(&self, preview: &str, service: &str, live: &Live) -> anyhow::Result<SocketAddr> {
        let stops = live.stops.load(std::sync::atomic::Ordering::SeqCst);
        let engine = self.engine()?;
        let (t, env) = engine.env(preview, service).await?;
        *live.sleep_after.lock() = t.sleep_after;
        let name = container_name(&t.host);
        self.iso.remove(&name).await;
        *live.container.lock() = Some(name.clone());
        let reach = |v: &str| reach_local(&self.iso.reach(v), self.iso.name() == "docker");
        let job = Job {
            kind: JobKind::Preview,
            name: name.clone(),
            image: t.spec.image.clone(),
            run: t.spec.run.clone(),
            env: env.into_iter().map(|(k, v)| (k, reach(&v))).collect(),
            memory_mb: t.spec.memory_mb,
            cpus: t.spec.cpus,
            port: Some(t.spec.port),
            change: Some(preview.to_string()),
            // A service lives while it's visited: on Fargate it stops by itself a little after
            // `sleep_after` without a byte, if this node is gone and can't stop it.
            deadline: t.sleep_after + Duration::from_secs(120),
            // Package caches shared by every preview of the repo: a second preview installs from
            // what the first downloaded.
            cache: Some(cache_name(&t.repo)),
        };
        self.trees.prepare(&*self.iso, &job, &t.repo, &t.revision).await?;
        let addr = self.iso.start(&name, t.spec.port).await?;
        // Stopped (slept, redeployed, removed) while it was starting: don't leave it running.
        if live.stops.load(std::sync::atomic::Ordering::SeqCst) != stops {
            self.iso.release(&name).await;
            anyhow::bail!("stopped while starting");
        }
        loop {
            if !self.iso.running(&name).await {
                anyhow::bail!("the service's command exited");
            }
            if answers(addr).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        info!(%preview, %service, %addr, "preview service running");
        Ok(addr)
    }

    /// The preview and service a request's host names, if it's a preview host.
    pub fn service_for_host(&self, host: &str) -> Option<(String, String)> {
        let host = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(h, _)| h);
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let label = host.strip_suffix(&self.cfg.domain)?.strip_suffix('.')?;
        if label.contains('.') || label.is_empty() {
            return None;
        }
        self.store.service_for_host(label).ok().flatten()
    }

    /// Where a visitor's feedback goes (the preview in roll's dashboard), when they get the button:
    /// they came in through roll (the link access cookie), the preview is a change's, the rules
    /// have `feedback` on and roll's dashboard is known.
    fn feedback_target(&self, preview: &str, req: &Request) -> Option<String> {
        let web = self.cfg.dashboard.as_deref()?;
        let access = self.store.access(preview).ok().flatten()?;
        if !access.required || !has_access_cookie(preview, &access.token, req) {
            return None;
        }
        let row = self.store.row(preview).ok()?;
        row.change_id.as_ref()?;
        self.store.settings(&row.project_id).ok()?.feedback.then(|| format!("{}/dashboard/projects/{}/previews?preview={preview}", web.trim_end_matches('/'), row.project_id))
    }

    /// Link access: a request carries the preview's token in its query (it gets a cookie and is
    /// sent to the same URL without it) or in that cookie; anything else is refused. `None`:
    /// let it through.
    fn gate(&self, preview: &str, service: &str, req: &Request) -> Option<Response> {
        let crate::store::Access { token, required, bypass } = match self.store.access(preview) {
            Ok(Some(a)) => a,
            Ok(None) => return None,
            Err(_) => return Some((StatusCode::SERVICE_UNAVAILABLE, "try again").into_response()),
        };
        if !required {
            return None;
        }
        // Automation (end-to-end tests in CI): the project's bypass secret in a header, for every
        // request (no cookie, no redirect).
        let given = req.headers().get(BYPASS_HEADER).and_then(|v| v.to_str().ok());
        if let (Some(want), Some(given)) = (&bypass, given)
            && same(&crate::store::bypass_hash(given), want)
        {
            return None;
        }
        let cookie = format!("roll_access_{preview}");
        if has_access_cookie(preview, &token, req) {
            return None;
        }
        let query = req.uri().query().unwrap_or("");
        let given = query.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == "roll_access").map(|(_, v)| v);
        if !token.is_empty() && given.is_some_and(|v| same(v, &token)) {
            let rest: Vec<&str> = query.split('&').filter(|kv| !kv.starts_with("roll_access=") && !kv.is_empty()).collect();
            // Only a path on this host: `//evil.example/` would be another site.
            let path = format!("/{}", req.uri().path().trim_start_matches(['/', '\\']));
            let location = if rest.is_empty() { path } else { format!("{path}?{}", rest.join("&")) };
            let secure = if self.cfg.scheme == "https" { "; Secure" } else { "" };
            let set = format!("{cookie}={token}; Path=/; Max-Age=2592000; HttpOnly; SameSite=Lax{secure}");
            let mut r = (StatusCode::FOUND, "").into_response();
            if let (Ok(l), Ok(c)) = (HeaderValue::from_str(&location), HeaderValue::from_str(&set)) {
                r.headers_mut().insert(header::LOCATION, l);
                r.headers_mut().insert(header::SET_COOKIE, c);
                r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            }
            return Some(r);
        }
        let through = match (&self.cfg.dashboard, self.store.row(preview)) {
            (Some(web), Ok(row)) => Some(format!(
                "{}/dashboard/projects/{}/previews?preview={preview}&go={}",
                web.trim_end_matches('/'),
                row.project_id,
                service
            )),
            _ => None,
        };
        Some(private_page(through.as_deref()))
    }

    async fn proxy(self: &Arc<Self>, preview: &str, service: &str, mut req: Request, feedback: bool) -> Response {
        let mut retried = false;
        let addr = loop {
            let addr = match self.ready(preview, service, self.cfg.wait).await {
                Ok(a) => a,
                Err(u) => return u.into_response(),
            };
            // A container that died underneath us: start it again, once.
            if retried || answers(addr).await {
                break addr;
            }
            retried = true;
            let live = self.live(preview, service);
            self.stop(preview, service, &live, Some("the container stopped answering".into())).await;
        };
        let pq = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
        let Ok(uri) = format!("http://{addr}{pq}").parse::<Uri>() else {
            return (StatusCode::BAD_REQUEST, "bad request").into_response();
        };
        let upgrade = is_upgrade(req.headers());
        let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut req));
        let host = req.headers().get(header::HOST).cloned();
        *req.uri_mut() = uri;
        *req.version_mut() = axum::http::Version::HTTP_11;
        strip_hop_by_hop(req.headers_mut(), upgrade);
        // The service never sees the project's secret.
        req.headers_mut().remove(BYPASS_HEADER);
        if let Some(h) = host {
            req.headers_mut().insert(HeaderName::from_static("x-forwarded-host"), h);
        }
        req.headers_mut().insert(HeaderName::from_static("x-forwarded-proto"), HeaderValue::from_str(&self.cfg.scheme).unwrap_or(HeaderValue::from_static("http")));
        // A page that gets the feedback button is asked for whole and uncompressed.
        if feedback {
            req.headers_mut().insert(header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        }
        let mut resp = match self.client.request(req).await {
            Ok(r) => r,
            Err(e) => return (StatusCode::BAD_GATEWAY, format!("the preview didn't answer: {e}")).into_response(),
        };
        if resp.status() == StatusCode::SWITCHING_PROTOCOLS
            && let Some(client_upgrade) = client_upgrade
        {
            let upstream = hyper::upgrade::on(&mut resp);
            tokio::spawn(async move {
                if let (Ok(a), Ok(b)) = tokio::join!(client_upgrade, upstream) {
                    let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(a), &mut TokioIo::new(b)).await;
                }
            });
        } else {
            strip_hop_by_hop(resp.headers_mut(), false);
        }
        let resp = resp.map(Body::new);
        if feedback { add_feedback(resp).await } else { resp }
    }

    #[cfg(test)]
    pub(crate) fn pretend_running(&self, preview: &str, service: &str, addr: SocketAddr) {
        let live = self.live(preview, service);
        *live.started.lock() = Some(Instant::now());
        live.phase.send_replace(Phase::Running { addr });
    }
}

/// Where a preview's pages load the feedback button from (its own host, so `script-src 'self'`
/// allows it), and the tag that loads it.
pub const FEEDBACK_PATH: &str = "/__roll/feedback.js";
const FEEDBACK_TAG: &[u8] = b"<script src=\"/__roll/feedback.js\" defer></script>";
/// The biggest page the button is added to; a bigger one passes through untouched.
const FEEDBACK_MAX: usize = 2 << 20;

/// `page` with the feedback button's tag before its last `</body>` (or at its end).
fn with_tag(mut page: Vec<u8>) -> Vec<u8> {
    let lower = page.to_ascii_lowercase();
    let at = lower.windows(7).rposition(|w| w == b"</body>").unwrap_or(page.len());
    page.splice(at..at, FEEDBACK_TAG.iter().copied());
    page
}

/// An HTML page from the service with the feedback button added; anything compressed, bigger
/// than [`FEEDBACK_MAX`] or not HTML passes through as it came.
async fn add_feedback(resp: Response) -> Response {
    let html = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.trim_start().to_ascii_lowercase().starts_with("text/html"));
    let plain = resp.headers().get(header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()).is_none_or(|e| e.eq_ignore_ascii_case("identity"));
    if resp.status() != StatusCode::OK || !html || !plain {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    let mut stream = body.into_data_stream();
    let mut page = Vec::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(c) => page.extend_from_slice(&c),
            Err(e) => return (StatusCode::BAD_GATEWAY, format!("the preview's page broke off: {e}")).into_response(),
        }
        if page.len() > FEEDBACK_MAX {
            let head = futures::stream::once(async move { Ok::<_, axum::Error>(Bytes::from(page)) });
            return Response::from_parts(parts, Body::from_stream(head.chain(stream)));
        }
    }
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(with_tag(page)))
}

/// The feedback button's script, sending notes to `target` (the preview in roll's dashboard).
fn feedback_script(target: &str) -> Response {
    let js = include_str!("feedback.js").replace("__ROLL_TARGET__", &serde_json::to_string(target).unwrap_or_else(|_| "\"\"".into()));
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8"), (header::CACHE_CONTROL, "private, max-age=300")], js).into_response()
}

/// The biggest screenshot kept (a 1280×800 PNG of an ordinary page is 10–200 KiB).
pub const SCREENSHOT_MAX: usize = 2 << 20;

/// The header automation sends a project's bypass secret in (docs/previews.md, "Safety").
pub const BYPASS_HEADER: &str = "x-roll-preview-bypass";

/// A private preview's answer to a request without its link: where to get in, if roll knows.
/// Whether `req` carries the preview's link access cookie.
fn has_access_cookie(preview: &str, token: &str, req: &Request) -> bool {
    let cookie = format!("roll_access_{preview}");
    req.headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .any(|(k, v)| k == cookie && !token.is_empty() && same(v, token))
}

fn private_page(through: Option<&str>) -> Response {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    let action = match through {
        Some(u) => format!("<p><a href=\"{}\">Open it through roll</a>: sign in, and you're sent on with its link.</p>", esc(u)),
        None => "<p>Open it from its link in roll.</p>".into(),
    };
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>Private preview · roll</title><style>\
         :root{{color-scheme:dark light;--bg:#000;--fg:#ededed;--muted:#8f8f8f;--accent:#4c8dff}}\
         @media (prefers-color-scheme:light){{:root{{--bg:#fff;--fg:#111;--muted:#666}}}}\
         body{{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--fg);\
         font:14px/1.5 ui-sans-serif,system-ui,sans-serif}}main{{max-width:520px;padding:24px}}\
         h1{{font-size:20px;font-weight:600;letter-spacing:-0.04em;margin:0 0 8px}}p{{color:var(--muted);margin:0}}\
         a{{color:var(--accent)}}</style></head><body><main><h1>This preview is private</h1>{action}</main></body></html>"
    );
    (StatusCode::UNAUTHORIZED, [(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], html).into_response()
}

/// Equal, in time that doesn't depend on where they differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// On Docker, a container reaches the host as `host.docker.internal`: rewrite loopback hosts
/// in URLs and `host:port` lists (`KAFKA_BOOTSTRAP_SERVERS`) too.
fn reach_local(v: &str, docker: bool) -> String {
    if !docker {
        return v.to_string();
    }
    let mut out = v.to_string();
    for local in ["127.0.0.1", "localhost", "0.0.0.0", "[::1]"] {
        for (pre, post) in [("//", ":"), ("@", ":"), ("", ":")] {
            let needle = format!("{pre}{local}{post}");
            if pre.is_empty() {
                // A bare `host:port` (at the start, or after a comma).
                let parts: Vec<String> = out
                    .split(',')
                    .map(|p| match p.trim().strip_prefix(&format!("{local}:")) {
                        Some(rest) if rest.bytes().all(|b| b.is_ascii_digit()) && !rest.is_empty() => format!("host.docker.internal:{rest}"),
                        _ => p.to_string(),
                    })
                    .collect();
                out = parts.join(",");
            } else {
                out = out.replace(&needle, &format!("{pre}host.docker.internal{post}"));
            }
        }
    }
    out
}

enum Unavailable {
    NotFound,
    Starting,
    Failed(String),
}

impl IntoResponse for Unavailable {
    fn into_response(self) -> Response {
        match self {
            Unavailable::NotFound => page(StatusCode::NOT_FOUND, "No preview here", "This preview was deleted, or it is waiting for approval.", false),
            Unavailable::Starting => {
                let mut r = page(StatusCode::SERVICE_UNAVAILABLE, "Waking the preview…", "This page refreshes when it's ready.", true);
                r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("2"));
                r
            }
            Unavailable::Failed(e) => page(StatusCode::BAD_GATEWAY, "The preview didn't start", &e, false),
        }
    }
}

#[async_trait]
impl Apps for Runtime {
    async fn deploy(&self, t: &Target) -> anyhow::Result<Option<String>> {
        // Whatever runs serves an older revision (or older URLs): the next request starts the
        // new one.
        let live = self.live(&t.preview, &t.service);
        *live.sleep_after.lock() = t.sleep_after;
        let running = !matches!(*live.phase.borrow(), Phase::Stopped);
        if running {
            self.stop(&t.preview, &t.service, &live, None).await;
        }
        Ok(None)
    }

    async fn wake(&self, preview: &str, service: &str) -> anyhow::Result<()> {
        let me = self.me.upgrade().ok_or_else(|| anyhow::anyhow!("the preview runtime is stopping"))?;
        match me.ready(preview, service, self.cfg.start_timeout).await {
            Ok(_) | Err(Unavailable::Starting) => Ok(()),
            Err(Unavailable::Failed(e)) => Err(anyhow::anyhow!(e)),
            Err(Unavailable::NotFound) => Err(anyhow::anyhow!("it stopped while starting")),
        }
    }

    async fn sleep(&self, preview: &str, service: &str) {
        let live = self.live.lock().get(&(preview.to_string(), service.to_string())).cloned();
        if let Some(live) = live {
            self.stop(preview, service, &live, None).await;
        }
    }

    async fn remove(&self, preview: &str, service: &str) -> anyhow::Result<()> {
        let live = self.live.lock().remove(&(preview.to_string(), service.to_string()));
        if let Some(live) = live {
            self.stop(preview, service, &live, None).await;
        }
        if let Ok(Some(host)) = self.store.services(preview).map(|v| v.into_iter().find(|s| s.name == service).map(|s| s.host)) {
            // Whichever node started it.
            self.iso.remove(&container_name(&host)).await;
        }
        // The repo's cache goes with its last preview service.
        if let Ok(Some(svc)) = self.store.services(preview).map(|v| v.into_iter().find(|s| s.name == service))
            && !self.store.repo_has_other_services(&svc.repo, preview).unwrap_or(true)
        {
            self.iso.drop_cache(&cache_name(&svc.repo)).await;
        }
        Ok(())
    }

    async fn logs(&self, preview: &str, service: &str, lines: usize) -> Vec<String> {
        let Ok(Some(host)) = self.store.services(preview).map(|v| v.into_iter().find(|s| s.name == service).map(|s| s.host)) else { return Vec::new() };
        self.iso.logs_tail(&container_name(&host), lines.clamp(1, 2000)).await.lines().map(str::to_string).collect()
    }

    async fn follow(&self, preview: &str, service: &str, lines: usize) -> Option<tokio::sync::mpsc::Receiver<String>> {
        let host = self.store.services(preview).ok()?.into_iter().find(|s| s.name == service)?.host;
        self.iso.follow(&container_name(&host), lines.clamp(1, 2000)).await
    }

    /// The service's page as a browser first shows it (a PNG): the browser runs in the service
    /// container's own network (it reaches nothing the service can't), bounded in memory, CPU and
    /// time, and goes when done.
    async fn screenshot(&self, preview: &str, service: &str) -> anyhow::Result<Option<Vec<u8>>> {
        use base64::Engine as _;
        if self.iso.name() != "docker" {
            return Ok(None);
        }
        let Some(host) = self.store.services(preview)?.into_iter().find(|s| s.name == service).map(|s| s.host) else { return Ok(None) };
        // The browser image once, in the background task screenshots run in (a cold pull isn't
        // bounded by the screenshot's 90 s).
        if code_preview::docker::docker(&["image", "inspect", "--format", "{{.Id}}", &self.cfg.screenshot_image]).await.is_err() {
            code_preview::docker::docker(&["pull", "--quiet", &self.cfg.screenshot_image]).await?;
        }
        let me = self.me.upgrade().ok_or_else(|| anyhow::anyhow!("the preview runtime is stopping"))?;
        match me.ready(preview, service, self.cfg.wait).await {
            Ok(_) => {}
            Err(Unavailable::Failed(e)) => anyhow::bail!("the service didn't start: {e}"),
            Err(_) => anyhow::bail!("the service didn't start in time"),
        }
        let name = container_name(&host);
        // The port the runtime published (the service's own), not whatever else its image exposes.
        let ports = code_preview::docker::docker(&["inspect", "-f", "{{range $p, $_ := .HostConfig.PortBindings}}{{$p}} {{end}}", &name]).await?;
        let port = ports.split_whitespace().next().and_then(|p| p.split('/').next()).ok_or_else(|| anyhow::anyhow!("the service publishes no port"))?.to_string();
        let shot = format!("roll-previews-shot-{host}");
        let mut args: Vec<String> = vec!["run".into(), "--rm".into(), "--name".into(), shot.clone(), "--network".into(), format!("container:{name}")];
        // Cleaned up with the runtime's own containers after a crash.
        args.extend(["--label".into(), format!("{}=preview", code_preview::docker::LABEL)]);
        if let Some(l) = code_preview::docker::home_label() {
            args.extend(["--label".into(), l]);
        }
        // The page is the preview's code: no capabilities, no new privileges, nothing written but /tmp.
        args.extend(["--cap-drop".into(), "ALL".into(), "--security-opt".into(), "no-new-privileges".into(), "--read-only".into(), "--tmpfs".into(), "/tmp:rw,size=256m".into()]);
        args.extend(code_preview::docker::limits(1024, 1.0));
        args.extend(["--env".into(), format!("URL=http://127.0.0.1:{port}/"), "--env".into(), "OUT=/tmp/s.png".into(), "--env".into(), "HOME=/tmp".into()]);
        args.extend([self.cfg.screenshot_image.clone(), "sh".into(), "-c".into(), format!("{} >/dev/null 2>&1 && base64 -w0 /tmp/s.png", self.cfg.screenshot_command)]);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = match tokio::time::timeout(Duration::from_secs(90), code_preview::docker::docker(&argv)).await {
            Ok(r) => r?,
            Err(_) => {
                code_preview::docker::remove(&shot).await;
                anyhow::bail!("the screenshot took over 90 s");
            }
        };
        let png = base64::engine::general_purpose::STANDARD.decode(out.trim()).map_err(|e| anyhow::anyhow!("the screenshot isn't a PNG: {e}"))?;
        anyhow::ensure!(png.starts_with(b"\x89PNG") && png.len() <= SCREENSHOT_MAX, "the screenshot isn't a PNG of at most {SCREENSHOT_MAX} bytes");
        Ok(Some(png))
    }

    async fn seed(&self, job: &crate::ports::SeedJob) -> anyhow::Result<(i32, String)> {
        use code_preview::runner::Outcome;
        let docker = self.iso.name() == "docker";
        let env: Vec<(String, String)> = job.env.iter().map(|(k, v)| (k.clone(), reach_local(&self.iso.reach(v), docker))).collect();
        let spec = code_preview::config::CheckSpec {
            image: job.seed.image.clone(),
            run: job.seed.run.clone(),
            timeout: Duration::from_secs(job.seed.timeout_s as u64),
            memory_mb: job.seed.memory_mb,
            cpus: 1.0,
            env: Default::default(),
        };
        let out = self.trees.run(&self.iso, &spec, &job.repo, &job.revision, &env).await?;
        match out.result {
            Outcome::Exited(code) => Ok((code, out.log)),
            Outcome::TimedOut => Ok((124, format!("timed out after {}s\n{}", job.seed.timeout_s, out.log))),
            Outcome::Error(e) => Err(anyhow::anyhow!(e)),
        }
    }

    fn status(&self, preview: &str, service: &str) -> Option<ServiceState> {
        let live = self.live.lock().get(&(preview.to_string(), service.to_string())).cloned()?;
        let phase = live.phase.borrow().clone();
        Some(match phase {
            Phase::Stopped => ServiceState::Sleeping,
            Phase::Starting => ServiceState::Deploying,
            Phase::Running { .. } => ServiceState::Running,
            Phase::Failed(_) => ServiceState::Failed,
        })
    }
}

/// Middleware for the API listener: requests for a preview service's host go to it, the rest
/// pass through.
pub async fn route(State(rt): State<Arc<Runtime>>, req: Request, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).map(str::to_string).or_else(|| req.uri().host().map(str::to_string));
    let Some((preview, service)) = host.and_then(|h| rt.service_for_host(&h)) else { return next.run(req).await };
    if let Some(r) = rt.gate(&preview, &service, &req) {
        return r;
    }
    // The feedback button: its script, and pages (a browser's GET for HTML) that load it.
    let target = rt.feedback_target(&preview, &req);
    if req.uri().path() == FEEDBACK_PATH
        && let Some(t) = &target
    {
        return feedback_script(t);
    }
    let accepts_html = req.headers().get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|a| a.contains("text/html"));
    let feedback = target.is_some() && req.method() == axum::http::Method::GET && accepts_html;
    match rt.cluster.get().cloned() {
        Some(cluster) => {
            let rt2 = rt.clone();
            cluster
                .dispatch(&crate::engine::preview_key(&preview.clone()), req, move |req, guard| async move {
                    let resp = rt2.proxy(&preview, &service, req, feedback).await;
                    drop(guard);
                    resp
                })
                .await
        }
        None => rt.proxy(&preview, &service, req, feedback).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct NoTrees;

    #[async_trait]
    impl Trees for NoTrees {
        async fn prepare(&self, _: &dyn Isolation, _: &Job, _: &str, _: &str) -> anyhow::Result<()> {
            anyhow::bail!("not in this test")
        }
        async fn run(&self, _: &Arc<dyn Isolation>, _: &code_preview::config::CheckSpec, _: &str, _: &str, _: &[(String, String)]) -> anyhow::Result<code_preview::runner::RunOutput> {
            anyhow::bail!("not in this test")
        }
    }

    /// A preview service's host reaches its container; other hosts reach the API.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn service_hosts_reach_their_containers() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&code_sql::Target::Sqlite(dir.path().join("p.db")), Default::default()).unwrap());
        let (row, _) = store
            .create(&crate::store::New {
                project_id: "prj_1".into(),
                key: "pr1".into(),
                project_name: "shop".into(),
                repo: "acme/web".into(),
                change_id: Some("chg_1".into()),
                branch: None,
                number: Some(1),
                title: "t".into(),
                github_pr: Some(1),
                head: "cs_1".into(),
                trusted: true,
                state: crate::State::Ready,
                expires_at: None,
                created_by: "roll".into(),
            })
            .unwrap();
        store.want_service(&row.id, "web", "acme/web", "cs_1", 512, &|h| format!("http://{h}.preview.test")).unwrap();
        let rt = Runtime::new(
            RuntimeConfig { domain: "preview.test".into(), dashboard: Some("https://roll.test".into()), ..RuntimeConfig::default() },
            Arc::new(code_preview::docker::Docker),
            Arc::new(NoTrees),
            store,
        );
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up = upstream.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = upstream.accept().await else { return };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                    let secret = if head.to_ascii_lowercase().contains("x-roll-preview-bypass") { " and the bypass secret" } else { "" };
                    let (kind, body) = match path.as_str() {
                        "/page" => ("text/html", format!("<html><BODY><p>asked for {}</p></BODY></html>", if head.to_ascii_lowercase().contains("accept-encoding: identity") { "identity" } else { "any" })),
                        _ => ("text/plain", format!("service saw {path}{secret}")),
                    };
                    let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
                });
            }
        });
        rt.pretend_running(&row.id, "web", up);
        let app = axum::Router::new().fallback(|| async { "api" }).layer(axum::middleware::from_fn_with_state(rt.clone(), route));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let get = |host: &'static str| async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            s.write_all(format!("GET /hello HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).await.unwrap();
            out
        };
        // Link access (the default): no token, no preview; the link's token turns into a cookie.
        assert!(get("web-pr1-shop.preview.test:7070").await.starts_with("HTTP/1.1 401"));
        let token = rt.store.access(&row.id).unwrap().unwrap().token;
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("GET /hello?roll_access={token}&x=1 HTTP/1.1\r\nHost: web-pr1-shop.preview.test\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        assert!(out.starts_with("HTTP/1.1 302") && out.contains("location: /hello?x=1"), "{out}");
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("GET /hello HTTP/1.1\r\nHost: web-pr1-shop.preview.test\r\nCookie: a=b; roll_access_{}={token}\r\nConnection: close\r\n\r\n", row.id).as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        assert!(out.ends_with("service saw /hello"), "{out}");
        // Automation: the project's bypass secret in a header, which the service never sees.
        let raw = |head: String| async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            s.write_all(head.as_bytes()).await.unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).await.unwrap();
            out
        };
        let with = |secret: &str| format!("GET /hello HTTP/1.1\r\nHost: web-pr1-shop.preview.test\r\nX-Roll-Preview-Bypass: {secret}\r\nConnection: close\r\n\r\n");
        // The feedback button: on a page a visitor through roll asked for as HTML (asked for whole),
        // its script from the preview's own host; nothing for anyone else.
        let page = |accept: &str| format!("GET /page HTTP/1.1\r\nHost: web-pr1-shop.preview.test\r\nAccept: {accept}\r\nCookie: roll_access_{}={token}\r\nConnection: close\r\n\r\n", row.id);
        let html = raw(page("text/html,*/*")).await;
        assert!(html.ends_with(r#"<p>asked for identity</p><script src="/__roll/feedback.js" defer></script></BODY></html>"#), "{html}");
        assert!(raw(page("application/json")).await.ends_with("<p>asked for any</p></BODY></html>"), "only a browser's page");
        let script = raw(format!("GET /__roll/feedback.js HTTP/1.1\r\nHost: web-pr1-shop.preview.test\r\nCookie: roll_access_{}={token}\r\nConnection: close\r\n\r\n", row.id)).await;
        assert!(script.contains("text/javascript") && script.contains(&format!(r#""https://roll.test/dashboard/projects/prj_1/previews?preview={}""#, row.id)), "{script}");
        assert!(raw("GET /__roll/feedback.js HTTP/1.1\r\nHost: web-pr1-shop.preview.test\r\nConnection: close\r\n\r\n".into()).await.starts_with("HTTP/1.1 401"));
        rt.store.put_settings("prj_1", &crate::Settings { feedback: false, ..Default::default() }).unwrap();
        assert!(raw(page("text/html")).await.ends_with("<p>asked for any</p></BODY></html>"), "the rule turns it off");
        let (secret, _) = rt.store.rotate_bypass("prj_1").unwrap();
        assert!(raw(with(&secret)).await.ends_with("service saw /hello"));
        assert!(raw(with("rpb_wrong")).await.starts_with("HTTP/1.1 401"));
        rt.store.rotate_bypass("prj_1").unwrap();
        assert!(raw(with(&secret)).await.starts_with("HTTP/1.1 401"), "a rotated secret stops working");
        let (secret, _) = rt.store.rotate_bypass("prj_1").unwrap();
        rt.store.update(&row.id, |r| r.trusted = false).unwrap();
        assert!(raw(with(&secret)).await.starts_with("HTTP/1.1 401"), "a fork's preview never opens to the secret");
        rt.store.update(&row.id, |r| r.trusted = true).unwrap();
        rt.store.put_settings("prj_1", &crate::Settings { access: crate::settings::Access::Public, ..Default::default() }).unwrap();
        assert!(get("web-pr1-shop.preview.test:7070").await.ends_with("service saw /hello"), "public previews need no link");
        assert!(get("localhost").await.ends_with("api"));
        assert!(get("nope-pr1-shop.preview.test").await.ends_with("api"), "an unknown label isn't a preview");
    }

    #[test]
    fn loopback_hosts_are_rewritten_for_docker() {
        assert_eq!(reach_local("postgres://u:p@127.0.0.1:55432/main", true), "postgres://u:p@host.docker.internal:55432/main");
        assert_eq!(reach_local("http://localhost:7070/x", true), "http://host.docker.internal:7070/x");
        assert_eq!(reach_local("127.0.0.1:9092,localhost:9093", true), "host.docker.internal:9092,host.docker.internal:9093");
        assert_eq!(reach_local("db.roll.sh:5432", true), "db.roll.sh:5432");
        assert_eq!(reach_local("http://127.0.0.1:1", false), "http://127.0.0.1:1");
    }
}
