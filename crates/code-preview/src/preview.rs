//! Previews: every change can serve its tree at `http://<change>.<domain>`, running the
//! command from `roll.toml` `[preview]` against a branch of the repo's Postgres and Redis.
//!
//! Previews scale to zero like everything else on roll. Creating one costs nothing: it's a
//! record and a URL. The first request starts the container (and branches the data), the
//! request waits until it answers, and after `idle` without requests the container stops again.
//! The data branches stay until the preview is deleted (the change lands or closes).

use crate::config::{PreviewSpec, RollToml};
use crate::data::{DataBranch, DataBranches, for_container};
use crate::docker::{self, LABEL};
use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use code_control::db::now;
use code_control::model::{Change, ChangeState, Preview, PreviewPostgres, PreviewRedis, PreviewState, RepoRow};
use code_control::cluster::RepoOwnership;
use code_control::{Code, PreviewBackend};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use parking_lot::Mutex;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;
use tracing::{info, warn};

#[derive(Clone, Debug)]
pub struct PreviewConfig {
    /// Previews answer on `<label>.<domain>`.
    pub domain: String,
    /// How preview URLs are written: scheme and (if not the default for it) port.
    pub scheme: String,
    pub port: Option<u16>,
    /// Stop a preview's container after this long without requests.
    pub idle: Duration,
    /// How long a request waits for a starting preview before getting a "starting" page.
    pub wait: Duration,
    /// How long a preview may take to start answering before it's marked failed.
    pub start_timeout: Duration,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        PreviewConfig {
            domain: "preview.localhost".into(),
            scheme: "http".into(),
            port: None,
            idle: Duration::from_secs(600),
            wait: Duration::from_secs(60),
            start_timeout: Duration::from_secs(600),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Phase {
    Stopped,
    Starting,
    Running { port: u16 },
    Failed(String),
}

/// A preview this process is serving (or has served).
struct Live {
    phase: watch::Sender<Phase>,
    container: Mutex<Option<String>>,
    last_request: Mutex<Instant>,
}

pub struct Previews {
    cfg: PreviewConfig,
    code: OnceLock<Weak<Code>>,
    data: Option<Arc<dyn DataBranches>>,
    live: Mutex<HashMap<String, Arc<Live>>>,
    client: Client<HttpConnector, Body>,
}

/// `chg_abc` ↔ `chg-abc`: underscores aren't valid in host names.
pub fn label(change_id: &str) -> String {
    change_id.replace('_', "-")
}

fn change_id(label: &str) -> Option<String> {
    let rest = label.strip_prefix("chg-")?;
    (!rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| format!("chg_{rest}"))
}

fn container_name(change_id: &str) -> String {
    format!("roll-preview-{}", label(change_id))
}

impl Previews {
    pub fn new(cfg: PreviewConfig, data: Option<Arc<dyn DataBranches>>) -> Arc<Previews> {
        Arc::new(Previews {
            cfg,
            code: OnceLock::new(),
            data,
            live: Mutex::new(HashMap::new()),
            client: Client::builder(TokioExecutor::new()).build_http(),
        })
    }

    /// Connect to the control plane (which holds this backend) and start the idle reaper. In a
    /// cluster, previews follow their repo: a node that takes a repo over marks its previews
    /// asleep (their containers were on the previous owner; the next visit starts them here),
    /// and a node that loses one stops their containers.
    pub fn attach(self: &Arc<Self>, code: &Arc<Code>) {
        let _ = self.code.set(Arc::downgrade(code));
        let clustered = code.cluster().is_some();
        let mut ownership = code.subscribe_ownership();
        let this = Arc::downgrade(self);
        tokio::spawn(async move {
            docker::remove_leftovers("preview").await;
            if !clustered && let Some(p) = this.upgrade() {
                p.reconcile();
            }
            let mut reap = tokio::time::interval(Duration::from_secs(15));
            reap.tick().await;
            loop {
                tokio::select! {
                    _ = reap.tick() => {
                        let Some(p) = this.upgrade() else { return };
                        p.reap().await;
                    }
                    Some(o) = ownership.recv() => {
                        let Some(p) = this.upgrade() else { return };
                        match o {
                            RepoOwnership::Acquired(repo) => p.reconcile_repo(&repo),
                            RepoOwnership::Lost(repo) => p.stop_repo(&repo).await,
                        }
                    }
                }
            }
        });
    }

    /// The previews of a repo just taken over: none of their containers runs here.
    fn reconcile_repo(&self, repo_id: &str) {
        let Ok(code) = self.code() else { return };
        let Ok(changes) = code.db.changes(repo_id, None, usize::MAX) else { return };
        for c in changes {
            if c.preview.as_ref().is_some_and(|p| matches!(p.state, PreviewState::Creating | PreviewState::Running))
                && !self.live.lock().get(&c.id).is_some_and(|l| matches!(*l.phase.borrow(), Phase::Running { .. } | Phase::Starting))
            {
                self.persist(&c.id, |p| p.state = PreviewState::Sleeping);
            }
        }
    }

    /// A repo moved to another node: stop its previews' containers here.
    async fn stop_repo(&self, repo_id: &str) {
        let Ok(code) = self.code() else { return };
        let live: Vec<(String, Arc<Live>)> = self.live.lock().iter().map(|(k, l)| (k.clone(), l.clone())).collect();
        for (id, l) in live {
            if code.change_or_404(&id).is_ok_and(|c| c.repo_id == repo_id) {
                info!(change = %id, "the repo moved to another node; stopping its preview here");
                self.stop(&l).await;
                self.live.lock().remove(&id);
            }
        }
    }

    fn code(&self) -> anyhow::Result<Arc<Code>> {
        self.code.get().and_then(Weak::upgrade).ok_or_else(|| anyhow::anyhow!("previews aren't attached to roll code"))
    }

    pub fn url(&self, change_id: &str) -> String {
        let port = match (self.cfg.scheme.as_str(), self.cfg.port) {
            ("http", Some(80)) | ("https", Some(443)) | (_, None) => String::new(),
            (_, Some(p)) => format!(":{p}"),
        };
        format!("{}://{}.{}{port}", self.cfg.scheme, label(change_id), self.cfg.domain)
    }

    fn dir(code: &Code, change_id: &str) -> PathBuf {
        code.config.home.join("code").join("previews").join(change_id)
    }

    fn live(&self, change_id: &str) -> Arc<Live> {
        self.live
            .lock()
            .entry(change_id.to_string())
            .or_insert_with(|| {
                Arc::new(Live { phase: watch::channel(Phase::Stopped).0, container: Mutex::new(None), last_request: Mutex::new(Instant::now()) })
            })
            .clone()
    }

    /// Update the stored preview (if the change still has one) and tell the feed.
    fn persist(&self, change_id: &str, f: impl FnOnce(&mut Preview)) {
        let Ok(code) = self.code() else { return };
        let Ok(c) = code.change_or_404(change_id) else { return };
        let Some(mut p) = c.preview else { return };
        f(&mut p);
        if code.db.set_change_preview(change_id, Some(&p)).is_ok() {
            code.emit(&c.repo_id, "preview", json!({ "change": change_id, "preview": p }));
        }
    }

    async fn stop(&self, live: &Live) {
        let name = live.container.lock().take();
        if let Some(name) = name {
            docker::remove(&name).await;
        }
        live.phase.send_replace(Phase::Stopped);
    }

    /// After a restart no preview container is left, whatever the changes say: a preview
    /// stored as starting or running is asleep (the next visit wakes it).
    pub fn reconcile(&self) {
        let Ok(code) = self.code() else { return };
        let Ok(changes) = code.db.changes_with_preview() else { return };
        for c in changes {
            if c.preview.as_ref().is_some_and(|p| matches!(p.state, PreviewState::Creating | PreviewState::Running)) {
                self.persist(&c.id, |p| p.state = PreviewState::Sleeping);
            }
        }
    }

    /// Stop previews nobody has visited for `idle`.
    async fn reap(&self) {
        let idle: Vec<(String, Arc<Live>)> = self
            .live
            .lock()
            .iter()
            .filter(|(_, l)| matches!(*l.phase.borrow(), Phase::Running { .. }) && l.last_request.lock().elapsed() > self.cfg.idle)
            .map(|(k, l)| (k.clone(), l.clone()))
            .collect();
        for (id, live) in idle {
            info!(change = %id, "preview idle; stopping");
            self.stop(&live).await;
            self.persist(&id, |p| p.state = PreviewState::Sleeping);
        }
    }

    /// The host port a preview answers on, starting it if needed. Waits up to `cfg.wait`.
    async fn ready(self: &Arc<Self>, change_id: &str) -> Result<u16, Unavailable> {
        let code = self.code().map_err(|_| Unavailable::NotFound)?;
        let c = code.change_or_404(change_id).map_err(|_| Unavailable::NotFound)?;
        if c.preview.is_none() || matches!(c.state, ChangeState::Landed | ChangeState::Closed) {
            return Err(Unavailable::NotFound);
        }
        let live = self.live(change_id);
        *live.last_request.lock() = Instant::now();
        let mut rx = live.phase.subscribe();
        let start = {
            let phase = rx.borrow_and_update().clone();
            match phase {
                Phase::Running { port } => return Ok(port),
                Phase::Starting => false,
                // A failed preview gets another try on the next visit: the failure is stored
                // on the change for whoever wants to read it.
                Phase::Stopped | Phase::Failed(_) => {
                    live.phase.send_replace(Phase::Starting);
                    true
                }
            }
        };
        if start {
            let this = self.clone();
            let id = change_id.to_string();
            let l = live.clone();
            tokio::spawn(async move { this.start(&id, &l).await });
        }
        let wait = tokio::time::timeout(self.cfg.wait, async {
            loop {
                if rx.changed().await.is_err() {
                    return Err(Unavailable::NotFound);
                }
                match rx.borrow_and_update().clone() {
                    Phase::Running { port } => return Ok(port),
                    Phase::Failed(e) => return Err(Unavailable::Failed(e)),
                    Phase::Stopped => return Err(Unavailable::NotFound),
                    Phase::Starting => {}
                }
            }
        });
        wait.await.unwrap_or(Err(Unavailable::Starting))
    }

    async fn start(&self, change_id: &str, live: &Live) {
        match tokio::time::timeout(self.cfg.start_timeout, self.start_inner(change_id, live)).await {
            Ok(Ok(port)) => {
                live.phase.send_replace(Phase::Running { port });
            }
            failed => {
                let e = match failed {
                    Ok(Err(e)) => format!("{e:#}"),
                    _ => format!("didn't answer within {}s", self.cfg.start_timeout.as_secs()),
                };
                let container = live.container.lock().clone();
                let tail = match container {
                    Some(name) => docker::logs_tail(&name, 30).await,
                    None => String::new(),
                };
                self.stop(live).await;
                let error = if tail.trim().is_empty() { e } else { format!("{e}\n\n{}", tail.trim_end()) };
                warn!(change = %change_id, error = %error.lines().next().unwrap_or(""), "preview failed to start");
                let stored: String = error.chars().take(4000).collect();
                self.persist(change_id, |p| {
                    p.state = PreviewState::Failed;
                    p.error = Some(stored);
                });
                live.phase.send_replace(Phase::Failed(error));
            }
        }
    }

    async fn start_inner(&self, change_id: &str, live: &Live) -> anyhow::Result<u16> {
        let code = self.code()?;
        let c = code.change_or_404(change_id)?;
        let repo = code.repo_by_id(&c.repo_id)?;
        // The change's tree comes from the repo's pool of work trees, leased only while it's read
        // (here, for roll.toml) and copied into the container below: nothing is bind-mounted,
        // so a running preview holds no tree.
        let head = code.store.resolve_or_404(&repo.id, &c.head)?;
        let spec = {
            let tree = code.checkout(&repo.id, &head).await?;
            load_spec(tree.path()).map_err(anyhow::Error::msg)?
        };
        self.persist(change_id, |p| {
            p.state = PreviewState::Creating;
            p.error = None;
        });
        let (pg, redis) = self.branch_data(&spec, &c).await?;
        self.persist(change_id, |p| {
            p.postgres = pg.as_ref().map(|(project, b)| PreviewPostgres { project_id: project.clone(), branch_id: b.id.clone() });
            p.redis = redis.as_ref().map(|b| PreviewRedis { db_id: b.id.clone() });
        });

        let env = preview_env(&spec, &repo, &c, pg.as_ref().map(|(_, b)| b), redis.as_ref());
        let name = container_name(change_id);
        docker::remove(&name).await;
        *live.container.lock() = Some(name.clone());
        let mut args: Vec<String> = vec![
            "create".into(),
            "--name".into(),
            name.clone(),
            "--label".into(),
            format!("{LABEL}=preview"),
            "--label".into(),
            format!("{LABEL}.change={change_id}"),
            "--publish".into(),
            format!("127.0.0.1::{}", spec.port),
            "--workdir".into(),
            "/app".into(),
            "--add-host".into(),
            "host.docker.internal:host-gateway".into(),
        ];
        args.extend(docker::limits(spec.memory_mb, spec.cpus));
        for (k, v) in &env {
            args.push("--env".into());
            args.push(format!("{k}={v}"));
        }
        args.extend(["--entrypoint".into(), "sh".into(), spec.image.clone(), "-c".into(), spec.run.clone()]);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        docker::docker(&argv).await?;
        {
            let tree = code.checkout(&repo.id, &head).await?;
            docker::docker(&["cp", &format!("{}/.", tree.path().display()), &format!("{name}:/app")]).await?;
        }
        docker::docker(&["start", &name]).await?;
        let port = docker::host_port(&name, spec.port).await?;
        loop {
            if !docker::running(&name).await {
                anyhow::bail!("the preview command exited");
            }
            if answers(port).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let url = self.url(change_id);
        self.persist(change_id, |p| {
            p.state = PreviewState::Running;
            p.url = Some(url);
            p.error = None;
        });
        info!(change = %change_id, port, "preview running");
        Ok(port)
    }

    async fn branch_data(
        &self,
        spec: &PreviewSpec,
        c: &Change,
    ) -> anyhow::Result<(Option<(String, DataBranch)>, Option<DataBranch>)> {
        if spec.postgres.is_none() && spec.redis.is_none() {
            return Ok((None, None));
        }
        let Some(data) = &self.data else {
            anyhow::bail!("this server can't branch Postgres or Redis for previews");
        };
        let pg = match &spec.postgres {
            // Branch names are stable, so a restarted daemon finds the same branches.
            Some(project) => Some((project.clone(), data.postgres_branch(project, &format!("preview/{}", c.id)).await?)),
            None => None,
        };
        let redis = match &spec.redis {
            Some(db) => Some(data.redis_branch(db, &format!("preview-{}", c.id)).await?),
            None => None,
        };
        Ok((pg, redis))
    }

    /// Proxy a request for a preview host to its container.
    async fn proxy(self: &Arc<Self>, change_id: &str, mut req: Request) -> Response {
        let mut retried = false;
        let port = loop {
            let port = match self.ready(change_id).await {
                Ok(p) => p,
                Err(u) => return u.into_response(),
            };
            // A container that died underneath us: start it again, once.
            if retried || answers(port).await {
                break port;
            }
            retried = true;
            let live = self.live(change_id);
            self.stop(&live).await;
        };
        let pq = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
        let Ok(uri) = format!("http://127.0.0.1:{port}{pq}").parse::<Uri>() else {
            return (StatusCode::BAD_REQUEST, "bad request").into_response();
        };
        let upgrade = is_upgrade(req.headers());
        let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut req));
        let host = req.headers().get(header::HOST).cloned();
        *req.uri_mut() = uri;
        *req.version_mut() = axum::http::Version::HTTP_11;
        strip_hop_by_hop(req.headers_mut(), upgrade);
        if let Some(h) = host {
            req.headers_mut().insert(HeaderName::from_static("x-forwarded-host"), h);
        }
        req.headers_mut().insert(HeaderName::from_static("x-forwarded-proto"), HeaderValue::from_str(&self.cfg.scheme).unwrap_or(HeaderValue::from_static("http")));
        let mut resp = match self.client.request(req).await {
            Ok(r) => r,
            Err(e) => return (StatusCode::BAD_GATEWAY, format!("preview didn't answer: {e}")).into_response(),
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
        resp.map(Body::new)
    }

    /// The change a request's host names, if it's a preview host.
    pub fn change_for_host(&self, host: &str) -> Option<String> {
        let host = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(h, _)| h);
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let l = host.strip_suffix(&self.cfg.domain)?.strip_suffix('.')?;
        if l.contains('.') {
            return None;
        }
        change_id(l)
    }
}

fn load_spec(dir: &Path) -> Result<PreviewSpec, String> {
    match RollToml::load(dir)? {
        Some(RollToml { preview: Some(p), .. }) => Ok(p),
        _ => Err("roll.toml has no [preview] section".into()),
    }
}

/// The environment a preview's container gets: `roll.toml` env, then what roll provides.
pub fn preview_env(spec: &PreviewSpec, repo: &RepoRow, c: &Change, pg: Option<&DataBranch>, redis: Option<&DataBranch>) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = spec.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let mut set = |k: &str, v: String| {
        env.retain(|(name, _)| name != k);
        env.push((k.to_string(), v));
    };
    set("PORT", spec.port.to_string());
    set("ROLL_REPO", repo.full_name());
    set("ROLL_CHANGE", c.id.clone());
    set("ROLL_CHANGESET", c.head.clone());
    if let Some(b) = pg {
        set("DATABASE_URL", for_container(&b.url));
    }
    if let Some(b) = redis {
        set("REDIS_URL", for_container(&b.url));
    }
    env
}

/// Whether something answers HTTP on the port. A published port accepts connections before
/// the app listens (Docker's proxy), so wait for actual bytes back.
async fn answers(port: u16) -> bool {
    let attempt = async {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        s.write_all(b"HEAD / HTTP/1.0\r\nHost: localhost\r\n\r\n").await.ok()?;
        let mut b = [0u8; 1];
        (s.read(&mut b).await.ok()? == 1).then_some(())
    };
    tokio::time::timeout(Duration::from_secs(2), attempt).await.ok().flatten().is_some()
}

fn is_upgrade(h: &HeaderMap) -> bool {
    h.contains_key(header::UPGRADE)
        && h.get(header::CONNECTION).and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("upgrade")))
}

fn strip_hop_by_hop(h: &mut HeaderMap, keep_upgrade: bool) {
    for name in ["keep-alive", "proxy-connection", "proxy-authenticate", "proxy-authorization", "te", "trailer", "transfer-encoding"] {
        h.remove(name);
    }
    if !keep_upgrade {
        h.remove(header::CONNECTION);
        h.remove(header::UPGRADE);
    }
}

enum Unavailable {
    NotFound,
    Starting,
    Failed(String),
}

impl IntoResponse for Unavailable {
    fn into_response(self) -> Response {
        match self {
            Unavailable::NotFound => page(StatusCode::NOT_FOUND, "No preview here", "This change has no preview, or it has landed or closed.", false),
            Unavailable::Starting => {
                let mut r = page(StatusCode::SERVICE_UNAVAILABLE, "Starting preview…", "This page refreshes when it's ready.", true);
                r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("2"));
                r
            }
            Unavailable::Failed(e) => page(StatusCode::BAD_GATEWAY, "The preview didn't start", &e, false),
        }
    }
}

/// A plain page for preview states: dark first, light when the browser asks.
fn page(status: StatusCode, title: &str, detail: &str, refresh: bool) -> Response {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let meta = if refresh { "<meta http-equiv=\"refresh\" content=\"2\">" } else { "" };
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">{meta}<title>{t} · roll</title><style>\
         :root{{color-scheme:dark light;--bg:#000;--fg:#ededed;--muted:#8f8f8f}}\
         @media (prefers-color-scheme:light){{:root{{--bg:#fff;--fg:#111;--muted:#666}}}}\
         body{{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--fg);\
         font:14px/1.5 ui-sans-serif,system-ui,sans-serif}}main{{max-width:560px;padding:24px}}\
         h1{{font-size:20px;font-weight:600;letter-spacing:-0.04em;margin:0 0 8px}}\
         pre{{color:var(--muted);white-space:pre-wrap;font:12px/1.6 ui-monospace,monospace;margin:0}}</style></head>\
         <body><main><h1>{t}</h1><pre>{d}</pre></main></body></html>",
        t = esc(title),
        d = esc(detail),
    );
    (status, [(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], html).into_response()
}

/// Middleware for the API listener: requests for a preview host go to the preview, everything
/// else passes through. In a cluster a preview runs on its repo's owner, so the request is
/// forwarded there (plain HTTP: a WebSocket upgrade only works on the owner itself).
pub async fn route(State(p): State<Arc<Previews>>, req: Request, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).map(str::to_string).or_else(|| req.uri().host().map(str::to_string));
    let Some(change) = host.and_then(|h| p.change_for_host(&h)) else { return next.run(req).await };
    let cluster = p.code().ok().and_then(|c| c.cluster().cloned().zip(c.change_or_404(&change).ok().map(|c| c.repo_id)));
    match cluster {
        Some((cluster, repo_id)) => {
            let p2 = p.clone();
            cluster.dispatch(&repo_id, req, move |req, guard| async move {
                let resp = p2.proxy(&change, req).await;
                drop(guard);
                resp
            })
            .await
        }
        None => p.proxy(&change, req).await,
    }
}

#[async_trait]
impl PreviewBackend for Previews {
    async fn create(&self, _repo: &RepoRow, change: &Change, dir: &Path) -> anyhow::Result<Preview> {
        let created_at = change.preview.as_ref().map_or_else(now, |p| p.created_at);
        let stored = change.preview.clone();
        let spec = load_spec(dir);
        // The tree changed: whatever runs serves the old one. The next visit starts the new one.
        let live = self.live.lock().get(&change.id).cloned();
        if let Some(live) = live {
            self.stop(&live).await;
        }
        Ok(match spec {
            Ok(_) => Preview {
                state: PreviewState::Sleeping,
                url: Some(self.url(&change.id)),
                postgres: stored.as_ref().and_then(|p| p.postgres.clone()),
                redis: stored.as_ref().and_then(|p| p.redis.clone()),
                error: None,
                created_at,
            },
            Err(e) => Preview { state: PreviewState::Failed, url: None, postgres: None, redis: None, error: Some(e), created_at },
        })
    }

    async fn delete(&self, _repo: &RepoRow, change: &Change) -> anyhow::Result<()> {
        let live = self.live.lock().remove(&change.id);
        if let Some(live) = live {
            self.stop(&live).await;
        } else {
            docker::remove(&container_name(&change.id)).await;
        }
        if let Some(p) = &change.preview
            && let Some(data) = &self.data
        {
            if let Some(pg) = &p.postgres
                && let Err(e) = data.delete_postgres(&pg.project_id, &pg.branch_id).await
            {
                warn!(change = %change.id, error = %e, "deleting the preview's Postgres branch");
            }
            if let Some(r) = &p.redis
                && let Err(e) = data.delete_redis(&r.db_id).await
            {
                warn!(change = %change.id, error = %e, "deleting the preview's Redis branch");
            }
        }
        if let Ok(code) = self.code() {
            // Trees from before previews used the pooled work trees.
            let _ = tokio::fs::remove_dir_all(Self::dir(&code, &change.id)).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RollToml;

    fn previews() -> Arc<Previews> {
        Previews::new(PreviewConfig { port: Some(7070), ..PreviewConfig::default() }, None)
    }

    #[test]
    fn hosts_map_to_changes() {
        let p = previews();
        assert_eq!(p.url("chg_abc123"), "http://chg-abc123.preview.localhost:7070");
        assert_eq!(p.change_for_host("chg-abc123.preview.localhost:7070").as_deref(), Some("chg_abc123"));
        assert_eq!(p.change_for_host("CHG-ABC123.Preview.Localhost").as_deref(), Some("chg_abc123"));
        assert_eq!(p.change_for_host("127.0.0.1:7070"), None);
        assert_eq!(p.change_for_host("preview.localhost"), None);
        assert_eq!(p.change_for_host("x.chg-abc.preview.localhost"), None);
        assert_eq!(p.change_for_host("chg-.preview.localhost"), None);
        assert_eq!(p.change_for_host("evil-preview.localhost"), None);
        let https = Previews::new(PreviewConfig { scheme: "https".into(), port: Some(443), domain: "preview.roll.sh".into(), ..PreviewConfig::default() }, None);
        assert_eq!(https.url("chg_x1"), "https://chg-x1.preview.roll.sh");
    }

    fn change() -> Change {
        Change {
            id: "chg_abc".into(),
            repo_id: "repo_1".into(),
            repo: "acme/web".into(),
            number: 7,
            title: "t".into(),
            body: String::new(),
            author: "agent".into(),
            state: ChangeState::Open,
            base: "cs_0".into(),
            head: "cs_1".into(),
            revision: 1,
            work_item: None,
            branch: None,
            github_pr: None,
            github_url: None,
            trusted: true,
            run_approved: false,
            draft: false,
            checks: Vec::new(),
            preview: None,
            landed_as: None,
            conflict: None,
            queue: None,
            blockers: Vec::new(),
            review: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn env_injects_data_urls_reachable_from_the_container() {
        let spec = RollToml::parse("[preview]\nimage = \"node:22\"\nrun = \"x\"\nport = 3000\nenv = { PORT = \"1\", NODE_ENV = \"preview\" }\n")
            .unwrap()
            .preview
            .unwrap();
        let repo: RepoRow = RepoRow {
            id: "repo_1".into(),
            owner_id: "own_1".into(),
            owner: "acme".into(),
            name: "web".into(),
            description: None,
            visibility: code_control::model::Visibility::Public,
            source: "native".into(),
            default_branch: "main".into(),
            land_policy: code_control::model::LandPolicy::PostMerge,
            required_checks: Vec::new(),
            github: None,
            import: None,
            created_at: 0,
            updated_at: 0,
        };
        let pg = DataBranch { id: "br_1".into(), url: "postgres://roll:pw@127.0.0.1:55432/main?options=endpoint%3Dep_1".into() };
        let redis = DataBranch { id: "rdb_2".into(), url: "redis://default:t@127.0.0.1:56380".into() };
        let env: HashMap<String, String> = preview_env(&spec, &repo, &change(), Some(&pg), Some(&redis)).into_iter().collect();
        assert_eq!(env["PORT"], "3000", "roll's PORT wins over roll.toml's");
        assert_eq!(env["NODE_ENV"], "preview");
        assert_eq!(env["ROLL_REPO"], "acme/web");
        assert_eq!(env["ROLL_CHANGE"], "chg_abc");
        assert_eq!(env["DATABASE_URL"], "postgres://roll:pw@host.docker.internal:55432/main?options=endpoint%3Dep_1");
        assert_eq!(env["REDIS_URL"], "redis://default:t@host.docker.internal:56380");
        let bare: HashMap<String, String> = preview_env(&spec, &repo, &change(), None, None).into_iter().collect();
        assert!(!bare.contains_key("DATABASE_URL") && !bare.contains_key("REDIS_URL"));
    }

    /// An upstream that answers `HEAD` (the liveness probe), echoes the path and forwarded host
    /// for plain requests, and switches protocols then echoes bytes for upgrades.
    async fn upstream() -> u16 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut b = [0u8; 1024];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        let Ok(n) = s.read(&mut b).await else { return };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&b[..n]);
                    }
                    let head = String::from_utf8_lossy(&buf).to_string();
                    let lower = head.to_ascii_lowercase();
                    if head.starts_with("HEAD") {
                        let _ = s.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n").await;
                    } else if lower.contains("upgrade: websocket") {
                        let _ = s.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n").await;
                        loop {
                            match s.read(&mut b).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => {
                                    if s.write_all(&b[..n]).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                    } else {
                        let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                        let fwd = lower.lines().find_map(|l| l.strip_prefix("x-forwarded-host: ")).unwrap_or("").trim().to_string();
                        let body = format!("path={path} fwd={fwd}");
                        let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
                    }
                });
            }
        });
        port
    }

    async fn raw(addr: std::net::SocketAddr, req: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut out)).await;
        String::from_utf8_lossy(&out).to_string()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proxies_http_and_websockets_and_reconciles_after_restart() {
        use code_control::Caller;
        use code_control::model::OwnerKind;
        let dir = tempfile::tempdir().unwrap();
        let objects = pg_storage::Objects::new(&pg_storage::StorageConfig::Local(dir.path().join("objects"))).unwrap();
        let code = Code::open(code_control::Config::local(dir.path().to_path_buf()), objects).unwrap();
        code.create_owner(&Caller::Admin, "acme", OwnerKind::Org, None).unwrap();
        let repo = code.create_repo(&Caller::Admin, &json!({ "owner": "acme", "name": "web" })).await.unwrap();
        let write = |f: &str| serde_json::from_value(json!({ "message": "m", "files": { f: { "content": "x" } } })).unwrap();
        code.land_api(&repo, &Caller::Admin, &write("a"), None, false).await.unwrap();
        let c = code.create_change(&repo, &Caller::Admin, &json!({ "title": "t", "message": "t", "files": { "b": { "content": "b" } } }), None).await.unwrap();
        let stored = Preview { state: PreviewState::Running, url: Some("http://x".into()), postgres: None, redis: None, error: None, created_at: 1 };
        code.db.set_change_preview(&c.id, Some(&stored)).unwrap();

        // A restarted daemon has no containers: a stored "running" preview is asleep.
        let p = Previews::new(PreviewConfig { domain: "preview.test".into(), ..PreviewConfig::default() }, None);
        p.attach(&code);
        p.reconcile();
        assert_eq!(code.change_or_404(&c.id).unwrap().preview.unwrap().state, PreviewState::Sleeping);

        // Pretend the container is up on `port`.
        let port = upstream().await;
        p.live(&c.id).phase.send_replace(Phase::Running { port });
        let app = axum::Router::new().fallback(|| async { "api" }).layer(axum::middleware::from_fn_with_state(p.clone(), route));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let host = format!("{}.preview.test", label(&c.id));

        let r = raw(addr, &format!("GET /hello?x=1 HTTP/1.1\r\nHost: {host}:8080\r\nConnection: close\r\n\r\n")).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(r.contains("path=/hello?x=1"), "{r}");
        assert!(r.contains(&format!("fwd={host}:8080")), "{r}");
        let r = raw(addr, "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await;
        assert!(r.ends_with("api"), "other hosts reach the API: {r}");
        let r = raw(addr, "GET / HTTP/1.1\r\nHost: chg-nope.preview.test\r\nConnection: close\r\n\r\n").await;
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");

        // WebSockets: the upgrade goes through and bytes flow both ways.
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(
            format!("GET /ws HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            assert_eq!(tokio::time::timeout(Duration::from_secs(10), s.read(&mut b)).await.unwrap().unwrap(), 1);
            head.push(b[0]);
        }
        let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
        assert!(head.starts_with("http/1.1 101"), "{head}");
        assert!(head.contains("upgrade: websocket"), "{head}");
        for msg in [&b"\x81\x04ping"[..], b"second frame"] {
            s.write_all(msg).await.unwrap();
            let mut got = vec![0u8; msg.len()];
            tokio::time::timeout(Duration::from_secs(10), s.read_exact(&mut got)).await.unwrap().unwrap();
            assert_eq!(got, msg);
        }

        // Nobody visits for `idle`: the reaper stops it and the change says it's asleep.
        code.db.set_change_preview(&c.id, Some(&Preview { state: PreviewState::Running, ..stored.clone() })).unwrap();
        let idle = Previews::new(PreviewConfig { domain: "preview.test".into(), idle: Duration::ZERO, ..PreviewConfig::default() }, None);
        idle.attach(&code);
        let live = idle.live(&c.id);
        live.phase.send_replace(Phase::Running { port });
        *live.last_request.lock() = Instant::now() - Duration::from_secs(1);
        idle.reap().await;
        assert_eq!(*live.phase.borrow(), Phase::Stopped);
        assert_eq!(code.change_or_404(&c.id).unwrap().preview.unwrap().state, PreviewState::Sleeping);

        // A landed change's preview is gone.
        code.close_change(&code.change_or_404(&c.id).unwrap()).unwrap();
        let r = raw(addr, &format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")).await;
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
    }

    #[test]
    fn hop_by_hop_headers_are_dropped() {
        let mut h = HeaderMap::new();
        h.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
        h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        h.insert(header::TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        assert!(is_upgrade(&h));
        let mut kept = h.clone();
        strip_hop_by_hop(&mut kept, true);
        assert!(kept.contains_key(header::UPGRADE) && !kept.contains_key(header::TRANSFER_ENCODING));
        strip_hop_by_hop(&mut h, false);
        assert!(h.is_empty());
    }
}
