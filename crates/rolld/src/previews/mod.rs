//! roll previews in rolld (docs/previews.md): the store, the engine, `/v1/previews`, the
//! routing of preview hosts, and one branch adapter per data product.
//!
//! The adapters talk to this node's own API over loopback with the operator token, exactly as
//! `roll` and the dashboard do. That keeps one path for every product, and it's right in a
//! cluster: each route already runs on the node that owns the resource (a Redis database's or
//! a stream's lease, storage's metadata owner), whichever node the engine runs on.

use reqwest::{Method, StatusCode};

use roll_previews::ports::Brancher;
use roll_previews::Store;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

mod data;
mod secrets;
mod services;
pub mod fleet;

pub use data::*;
pub use secrets::{Secrets, SecretsIntercepts, SecretsReveal};
pub use services::{RollApps, Services};

/// Which products run here (an adapter for each).
pub struct Products {
    pub postgres: bool,
    pub redis: bool,
    pub nosql: bool,
    pub storage: bool,
    pub streams: bool,
    pub queues: bool,
    pub tasks: bool,
}

pub struct Running {
    pub engine: Arc<roll_previews::Engine>,
    pub runtime: Arc<roll_previews::runtime::Runtime>,
    /// On a fleet: the internal route other nodes send screenshots of roll apps' instances that
    /// run here to ([`fleet::SHOT`]).
    pub internal: Option<axum::Router>,
}

/// Who may open a team-access project's previews: an account that still signs in, through the
/// roll session that asked for the ticket (signed out, revoked or expired is a no), and is an
/// admin account or the project's owner or admin now (a disabled or deleted one, or one removed
/// or demoted, is a no at once). A failed lookup is a no.
pub struct AccountsVisitors {
    pub accounts: Arc<roll_accounts::Accounts>,
    pub workspace: Arc<roll_workspace::Workspace>,
}

impl roll_previews::ports::Visitors for AccountsVisitors {
    fn names_of(&self, user: &str) -> anyhow::Result<Vec<String>> {
        let u = self.accounts.user(user).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(std::iter::once(u.handle).chain(u.identities.into_iter().filter(|i| i.provider == "github").map(|i| i.login)).collect())
    }

    fn may_open(&self, user: &str, session: &str, project: &str) -> bool {
        if !self.accounts.session_live(user, session).unwrap_or(false) {
            return false;
        }
        match self.accounts.user(user) {
            // An owner or admin of the project now (a member removed or demoted is a no at once),
            // never by who made it.
            Ok(u) if !u.disabled => u.admin || matches!(self.workspace.role_in(project, user), Ok(Some(roll_auth::roles::Role::Owner | roll_auth::roles::Role::Admin))),
            _ => false,
        }
    }
}

/// Open the store and start the engine: branches through this node's API, services as
/// containers through roll code's isolation (Docker or Fargate), roll code's changes as the
/// source, and roll code's preview backend in front of its own single-container previews.
/// `ROLL_PREVIEWS_INTERCEPT_ALLOW_PRIVATE=<root.pem>`: intercepted connections may dial private
/// addresses and trust that root too. Local development only; said loudly when on.
fn intercept_private() -> Option<std::path::PathBuf> {
    let root = std::env::var("ROLL_PREVIEWS_INTERCEPT_ALLOW_PRIVATE").ok().filter(|v| !v.trim().is_empty())?;
    tracing::warn!(root = %root, "ROLL_PREVIEWS_INTERCEPT_ALLOW_PRIVATE is set: previews' intercepted connections may reach private addresses and trust this root (local development only)");
    Some(std::path::PathBuf::from(root))
}

pub fn start(
    args: &crate::Args,
    home: &Path,
    code: &crate::CodeNode,
    workspace: Arc<roll_workspace::Workspace>,
    cluster: Arc<roll_cluster::Cluster>,
    on: Products,
    apps: Option<Arc<apps_control::Apps>>,
) -> anyhow::Result<Running> {
    let store = open_store(args, home)?;
    // A preview's own variables sealed at rest with the deployment key (as roll's reviewer keys
    // are); without one they're kept as written.
    if let Some(key) = args.secret_key() {
        match code_review::secret::Sealer::new(&key) {
            Ok(s) => {
                store.set_sealer(Arc::new(VarsSealer(s)))?;
            }
            Err(e) => tracing::warn!(error = %e, "previews: a preview's own variables aren't sealed"),
        }
    }
    let api = Api::new(args.api);
    let mut branchers: Vec<Arc<dyn Brancher>> = Vec::new();
    if on.postgres {
        branchers.push(Arc::new(Postgres(api.clone())));
    }
    if on.redis {
        branchers.push(Arc::new(Redis(api.clone())));
    }
    if on.nosql {
        branchers.push(Arc::new(Nosql(api.clone())));
    }
    if on.storage {
        branchers.push(Arc::new(Bucket(api.clone())));
    }
    if on.streams {
        branchers.push(Arc::new(Stream(api.clone())));
    }
    if on.queues {
        branchers.push(Arc::new(Queue(api.clone())));
    }
    // roll search (feature/roll-search): its routes answer once it's in this rolld.
    branchers.push(Arc::new(Search(api.clone())));
    if on.tasks {
        let public = args.code_public_url.clone().unwrap_or_else(|| format!("http://{}:{}", args.public_host, args.api.port()));
        branchers.push(Arc::new(Tasks { api: api.clone(), public }));
    }
    let domain = args.code_preview_domain.trim_matches('.').to_ascii_lowercase();
    let source = roll_previews::code::CodeSource::new(code.code.clone());
    let runtime = roll_previews::runtime::Runtime::new(
        roll_previews::runtime::RuntimeConfig {
            domain: domain.clone(),
            dashboard: Some(code.code.config.web_url.clone()).filter(|w| !w.trim().is_empty()),
            // An idle service frozen this long before it's stopped (0: stopped at once), at most
            // this many on the node.
            warm_for: std::time::Duration::from_secs(60 * std::env::var("ROLL_PREVIEWS_WARM_MINUTES").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(8 * 60).min(24 * 60)),
            warm_max: std::env::var("ROLL_PREVIEWS_WARM_MAX").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(32).min(1000),
            // Each preview's interception socket (keys a fork's preview never holds).
            intercept_dir: Some(home.join("previews").join("intercept")),
            // Local development only: a private TLS echo host for the live check.
            intercept_private: intercept_private(),
            ..Default::default()
        },
        code.isolation.clone(),
        source.clone(),
        store.clone(),
    );
    let clustered = cluster.backend_name() != "memory";
    if clustered {
        runtime.set_cluster(cluster.clone());
    }
    let leader = {
        let c = cluster.clone();
        Box::new(move || !clustered || c.singleton("previews")) as Box<dyn Fn() -> bool + Send + Sync>
    };
    // roll apps takes `feedback` on an environment, and adds the button itself.
    let mut roll_apps = RollApps::new(api.clone());
    roll_apps.feedback = apps.is_some();
    let apps_for_notes = apps.clone();
    let shots: Option<Arc<dyn fleet::Shoot>> = apps.clone().map(|a| Arc::new(services::AppsShots { reach: Arc::new(services::AppsReach(a)), runtime: runtime.clone() }) as Arc<dyn fleet::Shoot>);
    let engine = roll_previews::Engine::new(
        {
            let mut c = roll_previews::engine::Config { domain, port: Some(args.api.port()), mail: Some(format!("127.0.0.1:{}", args.previews_smtp.port())), otlp: Some(format!("127.0.0.1:{}", args.api.port())), ..Default::default() };
            if let Some(s) = args.previews_compare_every {
                (c.routes_every, c.new_errors_every) = (std::time::Duration::from_secs(s), std::time::Duration::from_secs(s));
            }
            c
        },
        store.clone(),
        workspace,
        source.clone(),
        branchers,
        // roll apps' instances are screenshotted where roll apps says they run (`Apps::reachable`):
        // on a fleet, on their service's holder (`fleet`).
        Arc::new(Services { apps: roll_apps, runtime: runtime.clone(), shots: shots.clone(), fleet: clustered.then(|| cluster.clone()) }),
        Arc::new(Secrets(api.clone())),
        leader,
    );
    runtime.attach(&engine);
    // Its services' spans, read as roll apps reads its own.
    let _ = engine.span_reader.set(Arc::new(services::AppsSpans));
    // roll apps' proxy lists a preview page's notes from the engine (`Engine::notes_for`).
    if let Some(a) = &apps_for_notes {
        a.set_feedback_notes(Arc::new(services::Notes(Arc::downgrade(&engine))));
        // A project's own preview domain: names apps' ingress doesn't hold go to previews' router.
        a.set_preview_router(Arc::new(services::DomainRouter(runtime.clone())));
        // Its pushes to branches of a Git URL service: those branches' previews.
        a.set_git_pushes(Arc::new(services::GitPushes(Arc::downgrade(&engine))));
        // And their status and pull request comment, where the code lives (roll's GitHub App).
        let _ = engine.git_report.set(Arc::new(services::GitReport { api: api.clone(), code: code.code.clone() }));
        // roll.toml services' output to the project's log drains that take previews'.
        let _ = engine.drains.set(Arc::new(services::AppsDrains { apps: a.clone(), wanted: Default::default() }));
    }
    if clustered {
        let _ = engine.cluster.set(cluster.clone());
    }
    let backend = roll_previews::code::Backend::new(code.previews.clone(), source);
    backend.attach(&engine);
    code.code.set_preview_backend(backend);
    // The mail catcher: previews' mail is kept with them, never sent on.
    match crate::listen::bind(args.previews_smtp) {
        Ok(l) => {
            tracing::info!(addr = %args.previews_smtp, "roll previews: catching previews' mail");
            tokio::spawn(roll_previews::mail::serve(l, Arc::new(roll_previews::mail::Store(store))));
        }
        Err(e) => tracing::warn!(addr = %args.previews_smtp, error = %e, "roll previews: no mail catcher (previews' mail fails)"),
    }
    drop(engine.start());
    crate::logs::PreviewMetrics::start(&engine);
    tracing::info!("roll previews: on");
    let internal = shots.filter(|_| clustered).map(|s| fleet::internal_router(s, cluster.clone()));
    Ok(Running { engine, runtime, internal })
}

/// roll's reviewer's sealer (AES-256-GCM under `ROLL_SECRET_KEY`) for a preview's own variables,
/// bound to the preview (`preview-vars:<id>`).
struct VarsSealer(code_review::secret::Sealer);

impl roll_previews::ports::Sealer for VarsSealer {
    fn seal(&self, aad: &str, plain: &str) -> String {
        self.0.seal(aad, plain)
    }

    fn open(&self, aad: &str, sealed: &str) -> Option<String> {
        self.0.open(aad, sealed).ok()
    }
}

pub fn open_store(args: &crate::Args, home: &Path) -> anyhow::Result<Arc<Store>> {
    let target = match args.code.cluster.system_database.as_deref().filter(|u| !u.is_empty()) {
        Some(url) => code_sql::Target::Postgres { url: url.to_string(), schema: "previews".into() },
        None => code_sql::Target::Sqlite(home.join("previews.db")),
    };
    let pg = code_sql::PgOptions { max_connections: args.db_budget()?.previews, ca: args.code.code_db_ca.clone(), ..Default::default() };
    Ok(Arc::new(tokio::task::block_in_place(|| Store::open(&target, pg))?))
}

/// This node's own API, as the operator.
#[derive(Clone)]
pub struct Api {
    base: String,
    token: Option<String>,
    http: reqwest::Client,
}

/// An answer that isn't a 2xx: its status and the API's `error`.
#[derive(Debug)]
pub struct Refused {
    pub status: StatusCode,
    pub error: String,
    /// The API's machine-readable reason (`{"code": "mismatch"}`), when it gave one.
    pub code: Option<String>,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.error, self.status.as_u16())
    }
}

impl std::error::Error for Refused {}

fn status_of(e: &anyhow::Error) -> Option<StatusCode> {
    e.downcast_ref::<Refused>().map(|r| r.status)
}

fn is_404(e: &anyhow::Error) -> bool {
    status_of(e) == Some(StatusCode::NOT_FOUND)
}

impl Api {
    pub fn new(listen: std::net::SocketAddr) -> Api {
        let ip = if listen.ip().is_unspecified() { std::net::IpAddr::from([127, 0, 0, 1]) } else { listen.ip() };
        let base = format!("http://{}", std::net::SocketAddr::new(ip, listen.port()));
        let token = std::env::var("ROLL_ADMIN_TOKEN").ok().filter(|t| !t.trim().is_empty());
        let http = reqwest::Client::builder().timeout(Duration::from_secs(600)).no_proxy().build().expect("an HTTP client");
        Api { base, token, http }
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> anyhow::Result<Value> {
        self.call_with(method, path, body, None).await
    }

    /// [`Api::call`] with an `Idempotency-Key`.
    async fn call_with(&self, method: Method, path: &str, body: Option<Value>, key: Option<&str>) -> anyhow::Result<Value> {
        let mut req = self.http.request(method.clone(), format!("{}{path}", self.base));
        if let Some(k) = key {
            req = req.header("idempotency-key", k);
        }
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.map_err(|e| anyhow::anyhow!("{method} {path}: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let v: Value = if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::String(text)) };
        if !status.is_success() {
            let error = v["error"].as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
            let code = v["code"].as_str().map(str::to_string);
            return Err(Refused { status, error, code }.into());
        }
        Ok(v)
    }

    async fn get(&self, path: &str) -> anyhow::Result<Value> {
        self.call(Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        self.call(Method::POST, path, Some(body)).await
    }

    /// Delete, counting "already gone" as done.
    async fn delete(&self, path: &str) -> anyhow::Result<()> {
        match self.call(Method::DELETE, path, None).await {
            Ok(_) => Ok(()),
            Err(e) if is_404(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

fn seg(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn s(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

fn gb(bytes: i64) -> f64 {
    bytes.max(0) as f64 / 1e9
}

/// Pick up after a crash between "branched" and "recorded": a name that's taken is our branch
/// only when `ours` says so; anything else is a real conflict.
fn conflict_is_ours(e: &anyhow::Error) -> bool {
    status_of(e) == Some(StatusCode::CONFLICT)
}
