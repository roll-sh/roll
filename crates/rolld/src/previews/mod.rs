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

pub use data::*;
pub use secrets::Secrets;
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
}

/// Open the store and start the engine: branches through this node's API, services as
/// containers through roll code's isolation (Docker or Fargate), roll code's changes as the
/// source, and roll code's preview backend in front of its own single-container previews.
pub fn start(
    args: &crate::Args,
    home: &Path,
    code: &crate::CodeNode,
    workspace: Arc<roll_workspace::Workspace>,
    cluster: Arc<roll_cluster::Cluster>,
    on: Products,
) -> anyhow::Result<Running> {
    let store = open_store(args, home)?;
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
    let engine = roll_previews::Engine::new(
        roll_previews::engine::Config { domain, port: Some(args.api.port()), mail: Some(format!("127.0.0.1:{}", args.previews_smtp.port())), ..Default::default() },
        store.clone(),
        workspace.clone(),
        source.clone(),
        branchers,
        // roll apps' instances are screenshotted once roll apps can say where they run
        // (`Apps::reachable`, roll apps' #26): `reach` is that, wired where rolld has both.
        Arc::new(Services { apps: RollApps::new(api.clone()), runtime: runtime.clone(), reach: None }),
        Arc::new(Secrets(api.clone())),
        leader,
    );
    runtime.attach(&engine);
    if clustered {
        let _ = engine.cluster.set(cluster.clone());
    }
    let backend = roll_previews::code::Backend::new(code.previews.clone(), workspace, source);
    backend.attach(&engine);
    code.code.set_preview_backend(backend);
    // The mail catcher: previews' mail is kept with them, never sent on.
    let bound = std::net::TcpListener::bind(args.previews_smtp).and_then(|l| {
        l.set_nonblocking(true)?;
        tokio::net::TcpListener::from_std(l)
    });
    match bound {
        Ok(l) => {
            tracing::info!(addr = %args.previews_smtp, "roll previews: catching previews' mail");
            tokio::spawn(roll_previews::mail::serve(l, Arc::new(roll_previews::mail::Store(store))));
        }
        Err(e) => tracing::warn!(addr = %args.previews_smtp, error = %e, "roll previews: no mail catcher (previews' mail fails)"),
    }
    drop(engine.start());
    crate::logs::PreviewMetrics::start(&engine);
    tracing::info!("roll previews: on");
    Ok(Running { engine, runtime })
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
