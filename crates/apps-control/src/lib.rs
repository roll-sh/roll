//! roll apps (docs/apps/README.md): services in workspace projects, built from roll code repos,
//! uploads or public images, deployed as immutable deployments into environments, and run as
//! instances that scale to zero and wake on the first request.
//!
//! - [`store`]: the catalog (`code_sql`).
//! - [`deploy`]: the deploy queue per service and environment: build, roll out, switch.
//! - [`live`]: an environment's instances on the service's owner: wake, suspend, stop, scale.
//! - [`runtime`]: where instances run (Docker; a fake in tests) and how images are built.
//! - [`public`]: the waking proxy for services' hostnames and ports.
//! - [`api`]: `/v1/apps` (docs/apps/http-api.md).
//!
//! On a fleet every service has one owner, the holder of the lease `apps:service:<id>`, which
//! runs its deploys, instances, cron and meter; other nodes forward to it.

pub mod api;
pub mod backups;
pub mod hooks;
pub mod tags;
pub mod sites;
pub mod site_rules;
pub mod tunnel;
pub mod tcp;
pub mod refs;
pub mod feedback;
pub mod scale;
pub mod registry;
pub mod config;
pub mod cron;
pub mod deploy;
pub mod drains;
pub mod integrations;
pub mod jobs;
pub mod live;
pub mod logs;
pub mod metrics;
pub mod model;
pub mod net;
pub mod notify;
pub mod public;
pub mod runtime;
pub mod shell;
pub mod spend;
pub mod store;

use integrations::{DomainTls, Resolver, Resources, SecretSource, Sources};
use live::{EnvLive, LiveConfig};
use logs::{LogHub, Stream};
use metrics::Prices;
use model::*;
use parking_lot::Mutex;
use runtime::{Builder, Runtime};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use store::{EnvRow, ServiceRow, Store};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    TooLarge(String),
    /// Allowed into the route, but not to what it would show (another product's rule).
    #[error("{0}")]
    Forbidden(String),
    #[error("apps database: {0}")]
    Db(#[from] code_sql::Error),
    #[error("{0}")]
    Internal(String),
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::Internal(e.to_string())
    }
}

/// How long a custom domain may wait for its DNS before the claim is dropped.
pub const PENDING_CLAIM_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// The ownership key of a service.
pub fn lease_key(service: &str) -> String {
    format!("apps:service:{service}")
}

#[derive(Clone, Debug)]
pub struct AppsConfig {
    pub home: PathBuf,
    /// Generated hostnames are `<slug>.<domain>`.
    pub domain: String,
    /// How generated URLs are written: scheme, and the port when it isn't the scheme's own.
    pub scheme: String,
    pub url_port: Option<u16>,
    /// A loopback-free port of its own per environment, from this range (one node only).
    pub ports: Option<(u16, u16)>,
    /// TCP services' ports (`run.tcp`), and the host clients reach them at (one node only).
    pub tcp_ports: Option<(u16, u16)>,
    pub tcp_host: String,
    /// Domains users can't add as custom domains (roll's own).
    pub reserved: Vec<String>,
    /// What custom domains point a CNAME at (default: the environment's own hostname).
    pub cname_target: Option<String>,
    pub build_timeout: Duration,
    pub live: LiveConfig,
    pub prices: Prices,
    /// The biggest `roll up` upload.
    pub upload_max: usize,
    /// The dashboard (roll code's `web_url`): a private preview's 401 page links into it.
    pub web_url: Option<String>,
    /// URLs a tenant chooses may be private addresses and plain HTTP: notification targets, and
    /// registries whose names resolve to one (local development and tests only:
    /// `ROLL_APPS_ALLOW_PRIVATE`).
    pub allow_private: bool,
    /// Between two passes over spend limits (`spend`).
    pub spend_every: Duration,
}

impl AppsConfig {
    pub fn new(home: PathBuf) -> AppsConfig {
        AppsConfig {
            home,
            domain: "apps.localhost".into(),
            scheme: "http".into(),
            url_port: Some(7080),
            ports: None,
            tcp_ports: None,
            tcp_host: "127.0.0.1".into(),
            reserved: Vec::new(),
            cname_target: None,
            build_timeout: Duration::from_secs(30 * 60),
            live: LiveConfig::default(),
            prices: Prices::default(),
            upload_max: 512 << 20,
            web_url: None,
            allow_private: false,
            spend_every: Duration::from_secs(30),
        }
    }
}

/// What roll apps runs on.
pub struct Deps {
    pub runtime: Arc<dyn Runtime>,
    pub builder: Arc<dyn Builder>,
    pub sources: Option<Arc<dyn Sources>>,
    pub secrets: Arc<dyn SecretSource>,
    pub resources: Arc<dyn Resources>,
    pub resolver: Arc<dyn Resolver>,
    /// Custom domains' certificates.
    pub tls: Arc<dyn DomainTls>,
    pub objects: Option<pg_storage::Objects>,
    pub cluster: Arc<roll_cluster::Cluster>,
    pub workspace: Option<Arc<roll_workspace::Workspace>>,
}

pub struct Apps {
    pub cfg: AppsConfig,
    pub store: Store,
    pub deps: Deps,
    pub logs: Arc<LogHub>,
    lives: Mutex<HashMap<String, Arc<EnvLive>>>,
    queues: Mutex<HashMap<String, Arc<Notify>>>,
    cancels: Mutex<HashMap<String, CancellationToken>>,
    /// Jobs running on this node: id → its environment (`env_key`). Bounded per environment and
    /// per node, and canceled when the environment goes from this node.
    running_jobs: Mutex<HashMap<String, String>>,
    /// Job containers this node runs at once, whatever jobs they belong to ([`jobs`]).
    job_slots: Arc<tokio::sync::Semaphore>,
    /// Services whose project reached its spend limit this month (`spend`), as last read.
    capped: Mutex<std::collections::HashSet<String>>,
    /// roll previews' notes for a preview's feedback button ([`Apps::set_feedback_notes`]).
    notes: std::sync::OnceLock<Arc<dyn integrations::FeedbackNotes>>,
    /// Log drains whose batch came back (the drainer sends the next).
    drain_done: Arc<Mutex<Vec<String>>>,
    /// Private environments' token hashes, by environment, as the owner last read or set them.
    access: Mutex<HashMap<String, Option<Access>>>,
    /// Bumped by every change to the access cache: a cold read caches what it read only if
    /// nothing changed meanwhile (a token rotated while it read would otherwise come back).
    access_gen: std::sync::atomic::AtomicU64,
    /// Each environment's `needs_deploy` as last worked out (it decrypts every variable and asks
    /// each linked product for its own): when, the settings generation and active deployment it
    /// was for, and the answer. Good for a minute, or until either moves.
    needs: Mutex<HashMap<String, (std::time::Instant, u64, String, bool)>>,
    /// Bumped by every change a deployment's fingerprint reads (settings, variables, links).
    settings_gen: std::sync::atomic::AtomicU64,
    /// A service changed (a deploy moved, someone changed something): its id, for the pages
    /// that follow it (`GET …/changes`). On the owner, as every such change is.
    changes: tokio::sync::broadcast::Sender<String>,
    /// The hourly volume backups are running (on their own task).
    backing_up: std::sync::atomic::AtomicBool,
    /// Watched tags are being checked (on their own task).
    watching: std::sync::atomic::AtomicBool,
    /// Scaling workers' backlogs are being read (on their own task).
    scaling: std::sync::atomic::AtomicBool,
    /// Static sites' file indexes, by image digest, as this node read them.
    sites: Mutex<HashMap<String, Arc<sites::Site>>>,
    /// Environments in a crash loop whose event is written (one per loop).
    looping: Mutex<std::collections::HashSet<String>>,
    /// For notification targets: public addresses only, no redirects.
    notify_client: reqwest::Client,
    /// Shells open over HTTP (the dashboard's terminal), by id.
    sessions: Mutex<HashMap<String, Arc<shell::Session>>>,
    /// WebSocket shells open per environment (`env_key`), counted with the HTTP sessions.
    ws_shells: Mutex<HashMap<String, Arc<std::sync::atomic::AtomicUsize>>>,
    /// Open tunnels per environment ([`tunnel::MAX_TUNNELS`]).
    tunnels: Mutex<HashMap<String, Arc<std::sync::atomic::AtomicUsize>>>,
    pub(crate) client: hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, axum::body::Body>,
    /// HTTP/2 without TLS (prior knowledge), for h2c apps ([`model::Run::h2c`]).
    pub(crate) client_h2: hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, axum::body::Body>,
    stop: CancellationToken,
}

pub(crate) fn env_key(service: &str, env: &str) -> String {
    format!("{service}/{env}")
}

/// A hostname label from `s`: lowercase, `[a-z0-9-]`, at most `max`, and never `--`: that's
/// how a deployment's own hostname (`<id>--<slug>`) is told apart, so no environment's slug may
/// look like another's deployment.
fn label(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }) {
        if !(c == '-' && out.ends_with('-')) {
            out.push(c);
        }
    }
    out.truncate(max);
    out.trim_matches('-').to_string()
}

/// The environment's variables' fingerprint (names and values, with the settings): a change
/// means the active deployment no longer matches.
fn fingerprint(vars: &[(String, String)], svc: &ServiceRow) -> String {
    let mut h = Sha256::new();
    let mut sorted: Vec<&(String, String)> = vars.iter().collect();
    sorted.sort();
    for (k, v) in sorted {
        h.update(k.as_bytes());
        h.update([0]);
        h.update(v.as_bytes());
        h.update([0]);
    }
    h.update(serde_json::to_string(&svc.build).expect("JSON").as_bytes());
    h.update(serde_json::to_string(&svc.run).expect("JSON").as_bytes());
    hex(&h.finalize()[..12])
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// What makes up an environment's variables, highest precedence last.
/// Another service's values a reference may read, and which of them are sealed.
type ServiceValues = (HashMap<String, String>, BTreeSet<String>);

pub struct ResolvedVars {
    pub vars: Vec<(String, String)>,
    pub sources: HashMap<String, &'static str>,
    /// References that named nothing ([`refs`]), as `VAR: service.NAME`: empty, and the deploy
    /// says so.
    pub unresolved: Vec<String>,
    /// Never shown again: the environment's own sealed variables, and references that read one.
    pub sealed: BTreeSet<String>,
}

impl Apps {
    pub fn new(cfg: AppsConfig, store: Store, deps: Deps) -> Arc<Apps> {
        let cfg_allow = cfg.allow_private;
        let logs = LogHub::new(deps.objects.clone());
        Arc::new(Apps {
            cfg,
            store,
            deps,
            logs,
            lives: Mutex::new(HashMap::new()),
            queues: Mutex::new(HashMap::new()),
            cancels: Mutex::new(HashMap::new()),
            running_jobs: Mutex::new(HashMap::new()),
            job_slots: Arc::new(tokio::sync::Semaphore::new(jobs::MAX_CONTAINERS)),
            capped: Mutex::new(Default::default()),
            notes: std::sync::OnceLock::new(),
            drain_done: Arc::new(Mutex::new(Vec::new())),
            access: Mutex::new(HashMap::new()),
            access_gen: std::sync::atomic::AtomicU64::new(0),
            needs: Mutex::new(HashMap::new()),
            settings_gen: std::sync::atomic::AtomicU64::new(0),
            changes: tokio::sync::broadcast::channel(256).0,
            backing_up: std::sync::atomic::AtomicBool::new(false),
            watching: std::sync::atomic::AtomicBool::new(false),
            scaling: std::sync::atomic::AtomicBool::new(false),
            sites: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            ws_shells: Mutex::new(HashMap::new()),
            tunnels: Mutex::new(HashMap::new()),
            notify_client: net::client(cfg_allow, Duration::from_secs(10)),
            looping: Mutex::new(std::collections::HashSet::new()),
            client: hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(30)).build_http(),
            client_h2: hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(30)).http2_only(true).build_http(),
            stop: CancellationToken::new(),
        })
    }

    /// Run a store call off the async runtime (it blocks on its database).
    pub async fn db<T: Send + 'static>(self: &Arc<Self>, f: impl FnOnce(&Store) -> Result<T> + Send + 'static) -> Result<T> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || f(&this.store)).await.map_err(|e| Error::Internal(e.to_string()))?
    }

    pub fn owns(&self, service: &str) -> bool {
        self.deps.cluster.owns(&lease_key(service))
    }

    // ── services ──

    fn project_name(&self, project: &str) -> Result<String> {
        match &self.deps.workspace {
            Some(ws) => match ws.get(roll_workspace::Scope::All, project) {
                Ok(p) => Ok(p.name),
                Err(roll_workspace::Error::NotFound(_)) => Err(Error::BadRequest(format!("no project {project}"))),
                Err(e) => Err(Error::Internal(e.to_string())),
            },
            None => Ok(project.strip_prefix("prj_").unwrap_or(project).to_string()),
        }
    }

    /// `actor`: who asked (the first deployment's).
    pub async fn create_service(self: &Arc<Self>, body: Value, actor: Option<String>) -> Result<Service> {
        let o = body.as_object().ok_or_else(|| Error::BadRequest("the body is an object".into()))?;
        for k in o.keys() {
            if !["project", "name", "source", "build", "run", "variables", "registry", "links", "x", "y"].contains(&k.as_str()) {
                return Err(Error::BadRequest(format!("unknown field {k}")));
            }
        }
        let project = o.get("project").and_then(Value::as_str).ok_or_else(|| Error::BadRequest("project is a workspace project id".into()))?.to_string();
        let name = o.get("name").and_then(Value::as_str).ok_or_else(|| Error::BadRequest("name is required".into()))?.to_string();
        check_name("a service name", &name)?;
        let source: Source = serde_json::from_value(o.get("source").cloned().ok_or_else(|| Error::BadRequest("source is required".into()))?).map_err(|e| Error::BadRequest(format!("source: {e}")))?;
        source.check()?;
        let build: Build = merge(&Build::default(), o.get("build"), "build")?;
        build.check()?;
        let run = Run::default().merged(o.get("run"))?;
        // Production's variables, set before its first deploy (an app that needs DATABASE_URL
        // shouldn't crash-loop first): checked now, before anything is made.
        let variables = match o.get("variables") {
            None | Some(Value::Null) => None,
            Some(Value::Object(m)) => {
                for (k, v) in m {
                    check_var_name(k)?;
                    if !v.is_string() {
                        return Err(Error::BadRequest(format!("variables.{k} is a string")));
                    }
                }
                Some(Value::Object(m.clone()))
            }
            Some(_) => return Err(Error::BadRequest("variables is an object of names to values".into())),
        };
        // The project's databases (and other resources) to link before the first deploy:
        // `[{kind, ref, prefix?}]`, `PUT …/links/{kind}/{ref}`'s.
        let links: Vec<(String, String, Value)> = match o.get("links") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) if a.len() <= 20 => a
                .iter()
                .map(|l| {
                    let kind = l["kind"].as_str().ok_or_else(|| Error::BadRequest("links[].kind is required".into()))?;
                    let r = l["ref"].as_str().ok_or_else(|| Error::BadRequest("links[].ref is required".into()))?;
                    Ok((kind.to_string(), r.to_string(), json!({ "prefix": l.get("prefix").cloned().unwrap_or(Value::Null) })))
                })
                .collect::<Result<_>>()?,
            Some(_) => return Err(Error::BadRequest("links is a list of at most 20 {kind, ref, prefix?}".into())),
        };
        // A private image's login, before its first pull (`PUT …/registry`'s body).
        let registry = match o.get("registry") {
            None | Some(Value::Null) => None,
            Some(r @ Value::Object(_)) if matches!(source, Source::Image { .. }) => Some(r.clone()),
            Some(Value::Object(_)) => return Err(Error::BadRequest("registry is for an image source".into())),
            Some(_) => return Err(Error::BadRequest("registry is {username, password, server?}".into())),
        };
        let (x, y) = (o.get("x").and_then(Value::as_f64), o.get("y").and_then(Value::as_f64));
        // A paused project makes nothing new (its first deploy would be refused anyway).
        let (p, month) = (project.clone(), crate::spend::month_of(code_sql::now_ms()).0);
        if self.db(move |s| s.project_capped(&p, month)).await? {
            return Err(Error::Conflict("this project reached its spending limit for the month: raise the limit to add services".into()));
        }
        let this = self.clone();
        let pname = {
            let p = project.clone();
            tokio::task::spawn_blocking(move || this.project_name(&p)).await.map_err(|e| Error::Internal(e.to_string()))??
        };
        // A repo's production environment follows its default branch.
        let branch = match (&source, &self.deps.sources) {
            (Source::Repo { repo, .. }, Some(src)) => {
                let _ = src.resolve(repo, None).await.map_err(|e| Error::BadRequest(format!("repo {repo}: {e}")))?;
                Some(self.default_branch(repo).await)
            }
            (Source::Repo { repo, .. }, None) => return Err(Error::BadRequest(format!("roll code isn't on this node, so {repo} can't be a source"))),
            _ => None,
        };
        let row = ServiceRow { id: new_id("svc"), project: project.clone(), name: name.clone(), source: source.clone(), build, run, created_at: self.store.now()? };
        let r = row.clone();
        self.db(move |s| s.create_service(&r)).await?;
        if let Some(ws) = self.deps.workspace.clone() {
            let (id, project) = (row.id.clone(), project.clone());
            let spot = (x.unwrap_or(0.0), y.unwrap_or(0.0));
            let placed = tokio::task::spawn_blocking(move || {
                let (x, y) = if spot == (0.0, 0.0) { free_spot(&ws, &project) } else { spot };
                ws.place(roll_workspace::Scope::All, &project, roll_workspace::Kind::Service, &id, x, y)
            })
            .await;
            if let Ok(Err(e)) = placed {
                tracing::warn!(service = %row.id, error = %e, "apps: couldn't place the service on its project's canvas");
            }
        }
        self.ensure_environment(&row, PRODUCTION, Some(branch.as_deref()), Some(false), &pname).await?;
        if let Some(set) = variables {
            self.change_variables(&row.id, PRODUCTION, json!({ "set": set })).await?;
        }
        for (kind, r, body) in links {
            // A link that doesn't check out makes no service: nothing to clean up by hand.
            if let Err(e) = self.put_link(&row.id, &kind, &r, body).await {
                let _ = self.delete_service(&row.id).await;
                return Err(e);
            }
        }
        if let Some(r) = registry
            && let Err(e) = self.put_registry(&row.id, r).await
        {
            // A login that doesn't check out makes no service: nothing to clean up by hand.
            let _ = self.delete_service(&row.id).await;
            return Err(e);
        }
        // A repo or an image deploys at once; an upload waits for `roll up`.
        if !matches!(source, Source::Upload) {
            self.deploy(&row.id, PRODUCTION, json!({}), By { trigger: Trigger::Api, actor: actor.clone() }, None, None).await?;
        }
        self.service(&row.id).await
    }

    async fn default_branch(&self, repo: &str) -> String {
        match &self.deps.sources {
            Some(s) => s.default_branch(repo).await.unwrap_or_else(|_| "main".into()),
            None => "main".into(),
        }
    }

    async fn ensure_environment(self: &Arc<Self>, svc: &ServiceRow, env: &str, branch: Option<Option<&str>>, ephemeral: Option<bool>, project_name: &str) -> Result<EnvRow> {
        check_name("an environment name", env)?;
        // `<service>-<project>[-<env>]`, the project left out when it has the service's name
        // (`roll up` names both after the directory: `web`, not `web-web`).
        let who = if label(project_name, 56) == svc.name { svc.name.clone() } else { format!("{}-{}", svc.name, project_name) };
        let base = if env == PRODUCTION { who } else { format!("{who}-{env}") };
        let base = label(&base, 56);
        let mut slugs = vec![base.clone()];
        for _ in 0..4 {
            slugs.push(format!("{base}-{}", random(4)));
        }
        let (id, env_s, branch) = (svc.id.clone(), env.to_string(), branch.map(|b| b.map(str::to_string)));
        let ports = self.cfg.ports;
        let row = self.db(move |s| s.put_environment(&id, &env_s, branch.as_ref().map(|b| b.as_deref()), ephemeral, &slugs, ports)).await?;
        Ok(row)
    }

    pub async fn services(self: &Arc<Self>, project: Option<String>) -> Result<Vec<Service>> {
        let rows = self.db(move |s| s.services(project.as_deref())).await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(self.view(r).await?);
        }
        Ok(out)
    }

    pub async fn service(self: &Arc<Self>, id: &str) -> Result<Service> {
        let id = id.to_string();
        let row = self.db(move |s| s.service(&id)).await?;
        self.view(row).await
    }

    async fn view(self: &Arc<Self>, r: ServiceRow) -> Result<Service> {
        let id = r.id.clone();
        let envs = self.db(move |s| s.environments(&id)).await?;
        let mut summaries = Vec::new();
        for e in &envs {
            summaries.push(self.summary(e).await?);
        }
        let id = r.id.clone();
        let (registry, links) = self.db(move |s| Ok((s.registry(&id)?, s.links(&id)?))).await?;
        let links = links.into_iter().map(|(kind, r#ref, _)| LinkRef { kind, r#ref }).collect();
        Ok(Service { id: r.id, project: r.project, name: r.name, source: r.source, build: r.build, run: r.run, registry, links, created_at: r.created_at, environments: summaries })
    }

    /// `PUT …/registry`: the login a service's image pulls with. `server` defaults to the image's
    /// registry (Docker Hub when it names none).
    pub async fn put_registry(self: &Arc<Self>, service: &str, body: Value) -> Result<Service> {
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| !["server", "username", "password"].contains(&k.as_str())) {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        let text = |k: &str, max: usize| -> Result<Option<String>> {
            match o.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(v)) if !v.is_empty() && v.len() <= max && !v.contains('\0') => Ok(Some(v.clone())),
                _ => Err(Error::BadRequest(format!("{k} is 1–{max} characters"))),
            }
        };
        let (username, password) = (text("username", 200)?, text("password", 4096)?);
        let (Some(username), Some(password)) = (username, password) else { return Err(Error::BadRequest("username and password are required".into())) };
        let sid = service.to_string();
        let svc = self.db(move |s| s.service(&sid)).await?;
        let Source::Image { image } = &svc.source else { return Err(Error::Conflict("only an image source pulls from a registry".into())) };
        // A login goes only to the registry the image is pulled from (deploys check it again).
        let server = match text("server", 255)? {
            Some(s) if registry_key(&s) == image_registry(image) => s,
            Some(s) => return Err(Error::BadRequest(format!("server {s} isn't {}, the registry source.image is pulled from", image_registry(image)))),
            None => registry_host(image).map(str::to_string).unwrap_or_else(|| "https://index.docker.io/v1/".into()),
        };
        let sid = service.to_string();
        self.db(move |s| s.set_registry(&sid, &server, &username, &password)).await?;
        self.service(service).await
    }

    pub async fn delete_registry(self: &Arc<Self>, service: &str) -> Result<Service> {
        let sid = service.to_string();
        self.db(move |s| s.delete_registry(&sid)).await?;
        self.service(service).await
    }

    pub fn url(&self, slug: &str) -> String {
        let port = self.cfg.url_port.filter(|p| !((self.cfg.scheme == "http" && *p == 80) || (self.cfg.scheme == "https" && *p == 443)));
        match port {
            Some(p) => format!("{}://{slug}.{}:{p}", self.cfg.scheme, self.cfg.domain),
            None => format!("{}://{slug}.{}", self.cfg.scheme, self.cfg.domain),
        }
    }

    async fn summary(self: &Arc<Self>, e: &EnvRow) -> Result<EnvironmentSummary> {
        let sid = e.service.clone();
        let public = self.db(move |s| s.service(&sid)).await.map(|s| s.run.public).unwrap_or(true);
        let (svc, env) = (e.service.clone(), e.name.clone());
        let recent = self.db(move |s| s.deployments(&svc, &env, 1)).await?;
        let active = match &e.active {
            Some(a) => {
                let a = a.clone();
                let d = self.db(move |s| s.deployment(&a)).await.ok();
                d.map(|d| DeployRef { id: d.id, revision: d.revision, created_at: d.created_at })
            }
            None => None,
        };
        let latest = recent.first().map(|d| LatestRef { id: d.id.clone(), status: d.status });
        let state = if latest.as_ref().is_some_and(|l| l.status.pending()) {
            InstanceState::Deploying
        } else {
            // Deployed but not loaded on this node yet (or between a switch and its commit):
            // nothing runs, which is asleep, not "not deployed".
            match self.lives.lock().get(&env_key(&e.service, &e.name)).map(|l| l.state()) {
                Some(InstanceState::None) | None if e.active.is_some() => InstanceState::Stopped,
                Some(s) => s,
                None => InstanceState::None,
            }
        };
        Ok(EnvironmentSummary { name: e.name.clone(), state, url: public.then(|| self.url(&e.slug)), active, latest })
    }

    pub async fn patch_service(self: &Arc<Self>, id: &str, body: Value) -> Result<Service> {
        let o = body.as_object().ok_or_else(|| Error::BadRequest("the body is an object".into()))?.clone();
        for k in o.keys() {
            if !["name", "source", "build", "run"].contains(&k.as_str()) {
                return Err(Error::BadRequest(format!("unknown field {k}")));
            }
        }
        let sid = id.to_string();
        let mut row = self.db(move |s| s.service(&sid)).await?;
        if let Some(n) = o.get("name") {
            let n = n.as_str().ok_or_else(|| Error::BadRequest("name is a string".into()))?;
            check_name("a service name", n)?;
            row.name = n.to_string();
        }
        let mut forget_login = false;
        if let Some(s) = o.get("source") {
            let source: Source = serde_json::from_value(s.clone()).map_err(|e| Error::BadRequest(format!("source: {e}")))?;
            source.check()?;
            // A registry login belongs to its registry: another one never receives it.
            let registry = |s: &Source| match s {
                Source::Image { image } => Some(image_registry(image)),
                _ => None,
            };
            forget_login = registry(&row.source) != registry(&source);
            row.source = source;
        }
        row.build = merge(&row.build, o.get("build"), "build")?;
        row.build.check()?;
        row.run = row.run.merged(o.get("run"))?;
        let r = row.clone();
        self.db(move |s| {
            s.update_service(&r)?;
            if forget_login {
                s.delete_registry(&r.id)?;
            }
            Ok(())
        })
        .await?;
        self.service(id).await
    }

    pub async fn delete_service(self: &Arc<Self>, id: &str) -> Result<()> {
        let sid = id.to_string();
        let envs = self.db(move |s| s.environments(&sid)).await?;
        for e in &envs {
            self.teardown_live(id, &e.name).await;
        }
        for name in self.deps.runtime.list(Some(id)).await {
            self.deps.runtime.remove(&name).await;
        }
        // Links to it from the project's other services go (their next deploy drops the URLs).
        let sid = id.to_string();
        if self.db(move |s| s.delete_links_to(LinkKind::Service, &sid)).await? > 0 {
            self.settings_changed();
        }
        // Its links' own credentials are revoked with it.
        let sid = id.to_string();
        for (kind, r, _, minted) in self.db(move |s| s.links_minted(&sid)).await? {
            if let Some(m) = minted {
                self.revoke_link(id, kind, &r, &m.kept).await;
            }
        }
        let sid = id.to_string();
        let deps = self.db(move |s| s.deployments_all(&sid)).await?;
        for d in deps {
            if let Some(img) = &d.image {
                self.deps.builder.remove_image(img).await;
                if let Some(k) = sites::key(img) {
                    self.drop_site(&k).await;
                }
            }
            let id = d.id.clone();
            if let Ok(Some(key)) = self.db(move |s| s.deployment_upload(&id)).await {
                self.drop_upload(&d.id, &key).await;
            }
            self.logs.delete(&format!("build:{}", d.id)).await;
        }
        let sid = id.to_string();
        let domains = self.db(move |s| s.custom_domains_of(&sid, None)).await?;
        let sid = id.to_string();
        self.db(move |s| s.delete_service(&sid)).await?;
        self.release_domains(&domains).await;
        if let Some(ws) = self.deps.workspace.clone() {
            let sid = id.to_string();
            let _ = tokio::task::spawn_blocking(move || {
                if let Some(p) = ws.list(roll_workspace::Scope::All).ok().and_then(|ps| ps.into_iter().find(|p| p.resources.iter().any(|r| r.kind == roll_workspace::Kind::Service && r.r#ref == sid))) {
                    let _ = ws.remove(roll_workspace::Scope::All, &p.id, roll_workspace::Kind::Service, &sid);
                }
            })
            .await;
        }
        self.deps.cluster.retire(&lease_key(id));
        Ok(())
    }

    // ── environments ──

    pub async fn environments(self: &Arc<Self>, service: &str) -> Result<Vec<Environment>> {
        let sid = service.to_string();
        self.db(move |s| s.service(&sid)).await?;
        let sid = service.to_string();
        let rows = self.db(move |s| s.environments(&sid)).await?;
        let mut out = Vec::new();
        for r in rows {
            out.push(self.environment_view(r).await?);
        }
        Ok(out)
    }

    pub async fn environment(self: &Arc<Self>, service: &str, env: &str) -> Result<Environment> {
        let (sid, e) = (service.to_string(), env.to_string());
        let row = self.db(move |s| s.environment(&sid, &e)).await?;
        self.environment_view(row).await
    }

    /// `GET …/pending`: what the next deploy changes (Railway's staged changes). The
    /// environment's own variables added, removed or changed since its live deployment went
    /// live (names only, from the snapshot kept at its switch; null when it has none, a
    /// deployment from before snapshots), and the run settings the service sets that the live
    /// deployment doesn't run with. Links and project secrets move `needs_deploy` too, unnamed.
    pub async fn pending(self: &Arc<Self>, service: &str, env: &str) -> Result<Value> {
        let (svc, row) = self.service_env(service, env).await?;
        let Some(active) = row.active.clone() else { return Ok(json!({ "variables": null, "run": [] })) };
        let a = active.clone();
        let (dep, then) = self.db(move |s| Ok((s.deployment(&a)?, s.snapshot(&a)?))).await?;
        let (sid, e) = (service.to_string(), env.to_string());
        let now = self.db(move |s| s.variables(&sid, &e)).await?;
        let variables = then.map(|then| {
            let then = then.vars;
            let added: Vec<&String> = now.keys().filter(|k| !then.contains_key(*k)).collect();
            let removed: Vec<&String> = then.keys().filter(|k| !now.contains_key(*k)).collect();
            let changed: Vec<&String> = now.iter().filter(|(k, v)| then.get(*k).is_some_and(|t| t != *v)).map(|(k, _)| k).collect();
            json!({ "added": added, "removed": removed, "changed": changed })
        });
        // Only what the service sets: the deployment also holds what its build worked out (a
        // Dockerfile's port, a plan's start command).
        // As the environment runs it: the service's settings with its own over them.
        let want = serde_json::to_value(svc.for_env(&row).map_or(svc.run.clone(), |s| s.run)).unwrap_or_default();
        let has = serde_json::to_value(&dep.run).unwrap_or_default();
        let run: Vec<&String> = want.as_object().into_iter().flatten().filter(|(k, v)| has.get(k.as_str()) != Some(*v)).map(|(k, _)| k).collect();
        Ok(json!({ "variables": variables, "run": run }))
    }

    /// Whether the environment's settings, variables or links changed since its active
    /// deployment `active`: cached ([`Apps::needs`]) since working it out is the expensive part of
    /// an environment's read, and the dashboard reads it every few seconds.
    async fn needs_deploy(self: &Arc<Self>, svc: &ServiceRow, r: &EnvRow, active: &str) -> Result<bool> {
        use std::sync::atomic::Ordering::SeqCst;
        let key = env_key(&r.service, &r.name);
        let seen = self.settings_gen.load(SeqCst);
        if let Some((at, g, a, v)) = self.needs.lock().get(&key)
            && *g == seen
            && a == active
            && at.elapsed() < Duration::from_secs(60)
        {
            return Ok(*v);
        }
        let a = active.to_string();
        let dep = self.db(move |s| s.deployment(&a)).await?;
        let v = match self.resolve_vars(svc, r, &dep.id).await {
            Ok(v) => fingerprint(&v.vars_for_hash(), svc) != dep.vars_hash,
            Err(_) => false,
        };
        // Kept only if nothing changed while it was worked out.
        if self.settings_gen.load(SeqCst) == seen {
            self.needs.lock().insert(key, (std::time::Instant::now(), seen, active.to_string(), v));
        }
        Ok(v)
    }

    /// `service` changed: whoever follows it reads it again.
    pub fn changed(&self, service: &str) {
        let _ = self.changes.send(service.to_string());
    }

    pub fn subscribe_changes(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.changes.subscribe()
    }

    /// What its instances are doing, in memory: a follower compares it each second (instances
    /// wake, sleep and crash without anyone changing anything).
    pub fn instances_digest(&self, service: &str) -> String {
        let mut lives: Vec<Arc<EnvLive>> = self.lives.lock().values().filter(|l| l.service == service).cloned().collect();
        lives.sort_by(|a, b| a.env.cmp(&b.env));
        let mut out = String::new();
        for l in lives {
            out.push_str(&format!("{}:{:?}:{:?};", l.env, l.state(), l.canary()));
            for i in l.instances() {
                out.push_str(&format!("{}={:?},", i.name, i.state()));
            }
        }
        out
    }

    /// Something a deployment's fingerprint reads changed: every cached `needs_deploy` is stale.
    pub fn settings_changed(&self) {
        self.settings_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    async fn environment_view(self: &Arc<Self>, r: EnvRow) -> Result<Environment> {
        let summary = self.summary(&r).await?;
        let (instances, crash_loop, canary, scaling) = match self.lives.lock().get(&env_key(&r.service, &r.name)) {
            Some(l) => (
                l.instances().iter().map(|i| Instance { id: i.name.clone(), deployment: i.deployment.clone(), state: i.state(), started_at: i.started_at(), in_flight: i.in_flight() }).collect(),
                l.crash_loop(),
                l.canary().map(|(deployment, share)| crate::model::Canary { deployment, share }),
                l.scaling().map(|(backlog, wanted)| crate::model::Scaling { backlog, wanted }),
            ),
            None => (Vec::new(), None, None, None),
        };
        let domains = self.domains(&r.service, &r.name).await?;
        let sid = r.service.clone();
        let svc = self.db(move |s| s.service(&sid)).await?;
        let svc = svc.for_env(&r).unwrap_or(svc);
        let next_run_at = match (&svc.run.schedule, &r.active) {
            (Some(sched), Some(_)) if svc.run.kind == RunType::Cron => crate::cron::Schedule::parse(sched, svc.run.timezone.as_deref()).ok().and_then(|s| s.next_after(code_sql::now_ms())),
            _ => None,
        };
        let needs_deploy = match &r.active {
            Some(a) => self.needs_deploy(&svc, &r, a).await?,
            None => false,
        };
        let private = self.access(&r.service, &r.name).await?.is_some();
        let (sid, e) = (r.service.clone(), r.name.clone());
        let waiting_for_ci = self.db(move |s| s.ci_waits()).await?.into_iter().find(|(w, _)| w.service == sid && w.env == e).map(|(w, _)| w.sha);
        Ok(Environment {
            summary,
            service: r.service.clone(),
            branch: r.branch.clone(),
            ephemeral: r.ephemeral,
            instances,
            domains,
            port: r.port,
            tcp: r.tcp_port.filter(|_| svc.run.tcp && svc.run.public && !private).map(|p| format!("{}:{p}", self.cfg.tcp_host)),
            needs_deploy,
            private,
            next_run_at,
            crash_loop,
            canary,
            run: r.run.clone(),
            hook: r.hook_hash.is_some(),
            follow_tag: r.follow_tag,
            feedback: r.feedback,
            maintenance: r.maintenance.clone().map(|m| crate::model::Maintenance { message: Some(m).filter(|m| !m.is_empty()) }),
            allow_ips: r.allow_ips.clone(),
            rate_limit: r.rate_limit,
            firewall: r.firewall.clone(),
            scaling,
            wait_for_ci: r.wait_for_ci,
            waiting_for_ci,
            created_at: r.created_at,
        })
    }

    /// `PUT …/environments/{env}`: make it (a preview environment, a staging one) or change its
    /// branch.
    pub async fn put_environment(self: &Arc<Self>, service: &str, env: &str, body: Value) -> Result<Environment> {
        let o = body.as_object().cloned().unwrap_or_default();
        for k in o.keys() {
            if !["branch", "ephemeral", "access", "variables_from", "wait_for_ci", "run", "follow_tag", "feedback", "maintenance", "allow_ips", "rate_limit", "firewall"].contains(&k.as_str()) {
                return Err(Error::BadRequest(format!("unknown field {k}")));
            }
        }
        // The new settings are checked before anything is written: a 400 changes nothing.
        // Maintenance: true or {message?} turns it on, false or null off.
        let maintenance: Option<Option<String>> = match o.get("maintenance") {
            None => None,
            Some(Value::Null | Value::Bool(false)) => Some(None),
            Some(Value::Bool(true)) => Some(Some(String::new())),
            Some(Value::Object(m)) => {
                if let Some(k) = m.keys().find(|k| k.as_str() != "message") {
                    return Err(Error::BadRequest(format!("maintenance.{k} isn't a field")));
                }
                match m.get("message") {
                    None | Some(Value::Null) => Some(Some(String::new())),
                    Some(Value::String(t)) if t.chars().count() <= 500 => Some(Some(t.trim().to_string())),
                    Some(_) => return Err(Error::BadRequest("maintenance.message is at most 500 characters".into())),
                }
            }
            Some(_) => return Err(Error::BadRequest("maintenance is true, {message?} or false".into())),
        };
        // Who gets in through its public names: networks (`a.b.c.d/n`, a bare address), or
        // null / [] for anyone.
        let allow_ips = match o.get("allow_ips") {
            None => None,
            Some(Value::Null) => Some(Vec::new()),
            Some(Value::Array(a)) if a.len() <= 100 => Some(a.iter().map(crate::model::ip_rule).collect::<Result<Vec<_>>>()?),
            Some(_) => return Err(Error::BadRequest("allow_ips is at most 100 networks, or null".into())),
        };
        let rate_limit = match o.get("rate_limit") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(v) => {
                let l: crate::model::RateLimit = serde_json::from_value(v.clone()).map_err(|e| Error::BadRequest(format!("rate_limit is {{per_minute, burst?}} or null: {e}")))?;
                l.check()?;
                Some(Some(l))
            }
        };
        let firewall = match o.get("firewall") {
            None => None,
            Some(Value::Null) => Some(Vec::new()),
            Some(Value::Array(a)) if a.len() <= crate::model::MAX_FIREWALL_RULES => Some(
                a.iter()
                    .map(|r| serde_json::from_value::<crate::model::FirewallRule>(r.clone()).map_err(|e| Error::BadRequest(format!("firewall: {e}"))).and_then(crate::model::FirewallRule::check))
                    .collect::<Result<Vec<_>>>()?,
            ),
            Some(_) => return Err(Error::BadRequest(format!("firewall is at most {} rules, or null", crate::model::MAX_FIREWALL_RULES))),
        };
        let branch = match o.get("branch") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::String(b)) if !b.is_empty() && b.len() <= 200 && !b.contains("..") => Some(Some(b.clone())),
            Some(_) => return Err(Error::BadRequest("branch is a branch name or null".into())),
        };
        let ephemeral = match o.get("ephemeral") {
            None => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => return Err(Error::BadRequest("ephemeral is a boolean".into())),
        };
        // A private environment (a preview): its hostname answers only visitors with the token.
        // `bypass_sha256`: an automation's secret (a CI run's end-to-end tests), only its SHA-256.
        let access: Option<Option<Access>> = match o.get("access") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::Object(a)) => {
                if let Some(k) = a.keys().find(|k| !["token", "bypass_sha256"].contains(&k.as_str())) {
                    return Err(Error::BadRequest(format!("access.{k} isn't a field")));
                }
                let token = a.get("token").and_then(Value::as_str).filter(|t| (20..=200).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)));
                let Some(token) = token else { return Err(Error::BadRequest("access.token is 20–200 of A-Z, a-z, 0-9, - and _".into())) };
                let bypass = match a.get("bypass_sha256") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(h)) if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) => Some(h.to_ascii_lowercase()),
                    Some(_) => return Err(Error::BadRequest("access.bypass_sha256 is a SHA-256 in hex".into())),
                };
                Some(Some(Access { token_hash: crate::public::token_hash(token), bypass_hash: bypass }))
            }
            Some(_) => return Err(Error::BadRequest("access is {token, bypass_sha256?} or null".into())),
        };
        // A new environment may start with another's variables (cloning staging from production).
        let variables_from = match o.get("variables_from") {
            None | Some(Value::Null) => None,
            Some(Value::String(f)) if f != env => Some(f.clone()),
            Some(_) => return Err(Error::BadRequest("variables_from is another environment's name".into())),
        };
        let (sid, e) = (service.to_string(), env.to_string());
        let existed = self.db(move |s| s.environment(&sid, &e)).await.is_ok();
        let copied = match (&variables_from, existed) {
            (Some(f), false) => {
                let (sid, f) = (service.to_string(), f.clone());
                // Not the sealed ones (Railway's): deploying the copy would read them, and that
                // can take less than deploying where they are.
                Some(self.db(move |s| {
                    s.environment(&sid, &f)?;
                    let sealed = s.sealed_variables(&sid, &f)?;
                    Ok(s.variables(&sid, &f)?.into_iter().filter(|(k, _)| !sealed.contains(k)).collect::<BTreeMap<_, _>>())
                })
                .await?)
            }
            _ => None,
        };
        let sid = service.to_string();
        let svc = self.db(move |s| s.service(&sid)).await?;
        let pname = {
            let this = self.clone();
            let p = svc.project.clone();
            tokio::task::spawn_blocking(move || this.project_name(&p)).await.map_err(|e| Error::Internal(e.to_string()))??
        };
        // The rest checked before anything is written too (a 400 changes nothing): run, the
        // switches, against the environment as it is (or will be made).
        let (sid, e) = (service.to_string(), env.to_string());
        let existing = self.db(move |s| s.environment(&sid, &e)).await.ok();
        if let Some(patch) = o.get("run") {
            let Value::Object(p) = patch else { return Err(Error::BadRequest("run is an object".into())) };
            if let Some(k) = p.keys().find(|k| !crate::model::ENV_RUN_FIELDS.contains(&k.as_str())) {
                return Err(Error::BadRequest(format!("run.{k} is the service's; an environment sets {}", crate::model::ENV_RUN_FIELDS.join(", "))));
            }
            if let Some(mut row) = existing.clone() {
                for (k, v) in p {
                    if v.is_null() {
                        row.run.remove(k);
                    } else {
                        row.run.insert(k.clone(), v.clone());
                    }
                }
                svc.for_env(&row).map_err(|e| Error::BadRequest(e.to_string().replace("no longer fit", "don't fit")))?;
            }
        }
        for k in ["feedback", "follow_tag", "wait_for_ci"] {
            if !matches!(o.get(k), None | Some(Value::Null | Value::Bool(_))) {
                return Err(Error::BadRequest(format!("{k} is a boolean")));
            }
        }
        if o.get("feedback") == Some(&Value::Bool(true)) && !ephemeral.unwrap_or(existing.as_ref().is_some_and(|r| r.ephemeral)) {
            return Err(Error::BadRequest("feedback is for a preview environment (ephemeral)".into()));
        }
        if o.get("follow_tag") == Some(&Value::Bool(true)) && !matches!(&svc.source, Source::Image { image } if !image.contains('@')) {
            return Err(Error::BadRequest("follow_tag is for an image service whose image has a tag (not a digest)".into()));
        }
        self.ensure_environment(&svc, env, branch.as_ref().map(|b| b.as_deref()), ephemeral, &pname).await?;
        if let Some(vars) = copied.filter(|v| !v.is_empty()) {
            let (sid, e) = (service.to_string(), env.to_string());
            self.db(move |s| s.change_variables(&sid, &e, &vars, &[], &BTreeSet::new())).await?;
        }
        // Its own size and scale: each field replaces the service's, null goes back to it.
        if let Some(patch) = o.get("run") {
            let Value::Object(p) = patch else { return Err(Error::BadRequest("run is an object".into())) };
            if let Some(k) = p.keys().find(|k| !crate::model::ENV_RUN_FIELDS.contains(&k.as_str())) {
                return Err(Error::BadRequest(format!("run.{k} is the service's; an environment sets {}", crate::model::ENV_RUN_FIELDS.join(", "))));
            }
            let (sid, e) = (service.to_string(), env.to_string());
            let mut row = self.db(move |s| s.environment(&sid, &e)).await?;
            for (k, v) in p {
                if v.is_null() {
                    row.run.remove(k);
                } else {
                    row.run.insert(k.clone(), v.clone());
                }
            }
            svc.for_env(&row).map_err(|e| Error::BadRequest(e.to_string().replace("no longer fit", "don't fit")))?;
            let (sid, e, run) = (service.to_string(), env.to_string(), row.run);
            self.db(move |s| s.set_env_run(&sid, &e, &run)).await?;
            self.settings_changed();
        }
        match o.get("feedback") {
            None | Some(Value::Null) => {}
            Some(Value::Bool(on)) => {
                let (sid, e) = (service.to_string(), env.to_string());
                let row = self.db(move |s| s.environment(&sid, &e)).await?;
                if *on && !row.ephemeral {
                    return Err(Error::BadRequest("feedback is for a preview environment (ephemeral)".into()));
                }
                let (sid, e, on) = (service.to_string(), env.to_string(), *on);
                self.db(move |s| s.set_feedback(&sid, &e, on)).await?;
            }
            Some(_) => return Err(Error::BadRequest("feedback is a boolean".into())),
        }
        if let Some(m) = maintenance {
            let (sid, e) = (service.to_string(), env.to_string());
            self.db(move |s| s.set_maintenance(&sid, &e, m.as_deref())).await?;
            self.live(service, env).forget_gate();
            self.changed(service);
        }
        if let Some(rules) = allow_ips {
            let (sid, e) = (service.to_string(), env.to_string());
            self.db(move |s| s.set_allow_ips(&sid, &e, &rules)).await?;
            self.live(service, env).forget_gate();
            self.changed(service);
        }
        if let Some(rules) = firewall {
            let (sid, e) = (service.to_string(), env.to_string());
            self.db(move |s| s.set_firewall(&sid, &e, &rules)).await?;
            self.live(service, env).forget_gate();
            self.changed(service);
        }
        if let Some(limit) = rate_limit {
            let (sid, e) = (service.to_string(), env.to_string());
            self.db(move |s| s.set_rate_limit(&sid, &e, limit.as_ref())).await?;
            self.live(service, env).forget_gate();
            self.changed(service);
        }
        match o.get("follow_tag") {
            None | Some(Value::Null) => {}
            Some(Value::Bool(on)) => {
                if *on && !matches!(&svc.source, Source::Image { image } if !image.contains('@')) {
                    return Err(Error::BadRequest("follow_tag is for an image service whose image has a tag (not a digest)".into()));
                }
                let (sid, e, on) = (service.to_string(), env.to_string(), *on);
                self.db(move |s| s.set_follow_tag(&sid, &e, on)).await?;
            }
            Some(_) => return Err(Error::BadRequest("follow_tag is a boolean".into())),
        }
        match o.get("wait_for_ci") {
            None | Some(Value::Null) => {}
            Some(Value::Bool(on)) => {
                let (sid, e, on) = (service.to_string(), env.to_string(), *on);
                self.db(move |s| s.set_wait_for_ci(&sid, &e, on)).await?;
            }
            Some(_) => return Err(Error::BadRequest("wait_for_ci is a boolean".into())),
        }
        if let Some(access) = access {
            let (sid, e, a) = (service.to_string(), env.to_string(), access.clone());
            self.db(move |s| s.set_access(&sid, &e, a.as_ref())).await?;
            self.set_access_cache(service, env, Some(access));
        }
        self.environment(service, env).await
    }

    /// Tear an environment down: instances, volume, domains, variables; deployments stay in
    /// history as `removed`.
    pub async fn delete_environment(self: &Arc<Self>, service: &str, env: &str) -> Result<()> {
        let (sid, e) = (service.to_string(), env.to_string());
        self.db(move |s| s.environment(&sid, &e)).await?;
        for d in self.store.pending_deployments(service)?.into_iter().filter(|d| d.environment == env) {
            self.cancel(service, env, &d.id).await.ok();
        }
        // The row goes first: from here no request finds the environment, so nothing makes its
        // live state (or a container, or the volume) again behind the teardown.
        let (sid, e) = (service.to_string(), env.to_string());
        let domains = self.db(move |s| s.custom_domains_of(&sid, Some(&e))).await?;
        let (sid, e) = (service.to_string(), env.to_string());
        self.db(move |s| s.delete_environment(&sid, &e)).await?;
        self.release_domains(&domains).await;
        self.teardown_live(service, env).await;
        self.set_access_cache(service, env, None);
        self.deps.runtime.remove_volume(&volume_name(service, env)).await;
        self.logs.forget(&format!("env:{service}/{env}"));
        Ok(())
    }

    /// A private environment's gate (none: public), cached on its owner.
    pub async fn access(self: &Arc<Self>, service: &str, env: &str) -> Result<Option<Access>> {
        if let Some(a) = self.access.lock().get(&env_key(service, env)) {
            return Ok(a.clone());
        }
        let seen = self.access_gen.load(std::sync::atomic::Ordering::SeqCst);
        let (sid, e) = (service.to_string(), env.to_string());
        let a = self.db(move |s| s.access(&sid, &e)).await?;
        let mut cache = self.access.lock();
        if self.access_gen.load(std::sync::atomic::Ordering::SeqCst) == seen {
            cache.insert(env_key(service, env), a.clone());
        }
        Ok(a)
    }

    /// Change the access cache after the store has the change: `Some` caches it, `None` forgets.
    pub(crate) fn set_access_cache(&self, service: &str, env: &str, access: Option<Option<Access>>) {
        let mut cache = self.access.lock();
        self.access_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match access {
            Some(a) => cache.insert(env_key(service, env), a),
            None => cache.remove(&env_key(service, env)),
        };
    }

    async fn teardown_live(&self, service: &str, env: &str) {
        self.cancel_jobs(service, env);
        let live = self.lives.lock().remove(&env_key(service, env));
        if let Some(l) = live {
            l.shutdown().await;
        }
    }

    /// The live state of an environment on this node (made on first use).
    pub fn live(self: &Arc<Self>, service: &str, env: &str) -> Arc<EnvLive> {
        self.lives
            .lock()
            .entry(env_key(service, env))
            .or_insert_with(|| {
                let l = EnvLive::new(service, env, self.deps.runtime.clone(), self.cfg.live.clone(), self.logs.sink(format!("env:{service}/{env}"), Stream::System));
                l.set_capped(self.capped.lock().contains(service));
                l
            })
            .clone()
    }

    pub fn lives(&self) -> Vec<Arc<EnvLive>> {
        self.lives.lock().values().cloned().collect()
    }

    pub async fn sleep(self: &Arc<Self>, service: &str, env: &str) -> Result<Environment> {
        self.environment(service, env).await?;
        self.live(service, env).sleep().await;
        self.environment(service, env).await
    }

    pub async fn wake(self: &Arc<Self>, service: &str, env: &str) -> Result<Environment> {
        self.environment(service, env).await?;
        self.load_live(service, env).await?;
        let live = self.live(service, env);
        live.acquire().await.map_err(|e| Error::Conflict(e.to_string()))?;
        self.environment(service, env).await
    }

    pub async fn restart(self: &Arc<Self>, service: &str, env: &str) -> Result<Environment> {
        self.environment(service, env).await?;
        self.load_live(service, env).await?;
        self.live(service, env).restart().await;
        self.environment(service, env).await
    }

    /// Make sure this node's live state knows the environment's active deployment (after a
    /// restart or a takeover).
    pub async fn load_live(self: &Arc<Self>, service: &str, env: &str) -> Result<Arc<EnvLive>> {
        if let Some(live) = self.lives.lock().get(&env_key(service, env)).cloned()
            && live.template().is_some()
        {
            return Ok(live);
        }
        // Only an environment that exists gets live state.
        let (sid, e) = (service.to_string(), env.to_string());
        let row = self.db(move |s| s.environment(&sid, &e)).await?;
        let live = self.live(service, env);
        let Some(active) = row.active.clone() else { return Ok(live) };
        let dep = self.db(move |s| s.deployment(&active)).await?;
        let sid = service.to_string();
        let svc = self.db(move |s| s.service(&sid)).await?;
        let tpl = self.template(&svc, &row, &dep).await?;
        live.adopt(Arc::new(tpl)).await;
        Ok(live)
    }

    // ── variables ──

    pub async fn resolve_vars(self: &Arc<Self>, svc: &ServiceRow, env: &EnvRow, deployment: &str) -> Result<ResolvedVars> {
        let mut vars: BTreeMap<String, (String, &'static str)> = BTreeMap::new();
        let port = svc.run.port.unwrap_or(8080);
        let roll = [
            ("PORT", port.to_string()),
            ("ROLL_SERVICE", svc.name.clone()),
            ("ROLL_SERVICE_ID", svc.id.clone()),
            ("ROLL_PROJECT", svc.project.clone()),
            ("ROLL_ENVIRONMENT", env.name.clone()),
            ("ROLL_DEPLOYMENT", deployment.to_string()),
            ("ROLL_PUBLIC_URL", self.url(&env.slug)),
            ("ROLL_PRIVATE_HOST", format!("{}.{}.internal", svc.name, env.name)),
        ];
        for (k, v) in roll {
            vars.insert(k.to_string(), (v, "roll"));
        }
        let sid = svc.id.clone();
        let links = self.db(move |s| s.links_minted(&sid)).await?;
        for (kind, r, prefix, minted) in links {
            // A link's own credentials as made, else the resource's variables now.
            let read = match minted {
                Some(m) => Ok(m.vars),
                None => self.link_vars(svc, kind, &r, &env.name).await,
            };
            match read {
                Ok(vs) => {
                    for (k, v) in vs {
                        // Another service's names are the project network's: nothing to rewrite.
                        let v = if kind == LinkKind::Service { v } else { self.deps.runtime.reach(&v) };
                        vars.insert(format!("{prefix}{k}"), (v, "link"));
                    }
                }
                Err(e) => tracing::warn!(service = %svc.id, kind = kind.as_str(), r#ref = %r, error = %e, "apps: a link's variables couldn't be read"),
            }
        }
        match self.deps.secrets.resolve(&svc.project, &env.name, &svc.id).await {
            Ok(secrets) => {
                for s in secrets {
                    // A linked resource's address, rewritten for the container as apps' own links.
                    let v = if s.linked { self.deps.runtime.reach(&s.value) } else { s.value };
                    vars.insert(s.name, (v, "secret"));
                }
            }
            Err(e) => return Err(Error::Internal(format!("the project's {} secrets couldn't be read: {e}", env.name))),
        }
        let (sid, e) = (svc.id.clone(), env.name.clone());
        let (own, mut sealed) = self.db(move |s| Ok((s.variables(&sid, &e)?, s.sealed_variables(&sid, &e)?))).await?;
        for (k, v) in own {
            vars.insert(k, (v, "service"));
        }
        // Reference variables, one level deep: against what resolved above, and other services'.
        let unresolved = self.fill_references(svc, &env.name, &mut vars, &mut sealed).await;
        let sources = vars.iter().map(|(k, (_, s))| (k.clone(), *s)).collect();
        Ok(ResolvedVars { vars: vars.into_iter().map(|(k, (v, _))| (k, v)).collect(), sources, unresolved, sealed })
    }

    /// Fill the `${{ … }}` in the service's own variables ([`refs`]); what named nothing. A
    /// reference that reads a sealed value is sealed too (`sealed`): it would show it.
    async fn fill_references(self: &Arc<Self>, svc: &ServiceRow, env: &str, vars: &mut BTreeMap<String, (String, &'static str)>, sealed: &mut BTreeSet<String>) -> Vec<String> {
        let referring: Vec<String> = vars.iter().filter(|(_, (v, s))| *s == "service" && v.contains("${{")).map(|(k, _)| k.clone()).collect();
        if referring.is_empty() {
            return Vec::new();
        }
        let own: HashMap<String, String> = vars.iter().map(|(k, (v, _))| (k.clone(), v.clone())).collect();
        // Another service's: by name in this project, the same environment.
        let mut others: HashMap<String, Option<ServiceValues>> = HashMap::new();
        for k in &referring {
            for (r, _) in refs::refs(&vars[k].0) {
                if let Some(name) = r.service.filter(|n| !others.contains_key(*n) && *n != svc.name) {
                    let found = self.service_values(&svc.project, name, env).await;
                    others.insert(name.to_string(), found);
                }
            }
        }
        let mut unresolved = Vec::new();
        let own_sealed = sealed.clone();
        for k in referring {
            let mut reads_sealed = false;
            let (filled, missing) = refs::fill(&vars[&k].0, |r| match r.service {
                None => own.get(r.var).cloned().inspect(|_| reads_sealed |= own_sealed.contains(r.var)),
                Some(n) if n == svc.name => own.get(r.var).cloned().inspect(|_| reads_sealed |= own_sealed.contains(r.var)),
                Some(n) => others.get(n).and_then(|o| o.as_ref()).and_then(|(o, s)| o.get(r.var).cloned().inspect(|_| reads_sealed |= s.contains(r.var))),
            });
            unresolved.extend(missing.into_iter().map(|m| format!("{k}: {m}")));
            if reads_sealed {
                sealed.insert(k.clone());
            }
            vars.insert(k, (filled, "reference"));
        }
        unresolved
    }

    /// What another service of `project` offers a reference in `env`: its own variables, and
    /// `PORT`, `ROLL_PRIVATE_HOST` and `ROLL_PUBLIC_URL`, and which of them are sealed. None: no
    /// such service or environment.
    async fn service_values(self: &Arc<Self>, project: &str, name: &str, env: &str) -> Option<ServiceValues> {
        let (p, n, e) = (project.to_string(), name.to_string(), env.to_string());
        let (svc, row, own, sealed) = self
            .db(move |s| {
                let Some(svc) = s.services(Some(&p))?.into_iter().find(|x| x.name == n) else { return Ok(None) };
                let row = s.environment(&svc.id, &e)?;
                let own = s.variables(&svc.id, &e)?;
                let sealed = s.sealed_variables(&svc.id, &e)?;
                Ok(Some((svc, row, own, sealed)))
            })
            .await
            .ok()
            .flatten()?;
        let mut out: HashMap<String, String> = own.into_iter().collect();
        let port = match row.active.clone() {
            Some(a) => self.db(move |s| s.deployment(&a)).await.ok().and_then(|d| d.run.port),
            None => None,
        };
        out.insert("PORT".into(), port.or(svc.run.port).unwrap_or(8080).to_string());
        out.insert("ROLL_PRIVATE_HOST".into(), format!("{}.{env}.internal", svc.name));
        if svc.run.public {
            out.insert("ROLL_PUBLIC_URL".into(), self.url(&row.slug));
        }
        Some((out, sealed))
    }

    /// Before values with references are stored: every other service they name is the
    /// project's, and the caller may see its values (a token's `reveal` on it, as reading them
    /// would need): a reference is a way to read them.
    pub async fn check_references(self: &Arc<Self>, project: &str, own: &str, env: &str, values: &[&str], who: integrations::Asker<'_>) -> Result<()> {
        let mut names: Vec<&str> = values.iter().flat_map(|v| refs::refs(v)).filter_map(|(r, _)| r.service).filter(|n| *n != own).collect();
        names.sort();
        names.dedup();
        for name in names {
            let (p, n) = (project.to_string(), name.to_string());
            let Some(target) = self.db(move |s| Ok(s.services(Some(&p))?.into_iter().find(|x| x.name == n))).await? else {
                return Err(Error::BadRequest(format!("${{{{ {name}.… }}}} names no service of this project")));
            };
            if !who.operator
                && let Some(g) = who.grant
            {
                let resource = roll_auth::scope::Resource { product: "apps".into(), id: Some(target.id.clone()), project: Some(project.to_string()), environment: Some(env.to_string()), ..Default::default() };
                if g.allows(&resource, roll_auth::scope::Action::Reveal) != roll_auth::scope::Outcome::Allow {
                    return Err(Error::Forbidden(format!("a reference to {name}'s variables needs reveal on {name}")));
                }
            }
        }
        Ok(())
    }

    /// What reads a job's output or opens a shell needs when the environment has values from
    /// roll secrets (a job may print them, a shell reads them all): roll secrets' own rule for
    /// revealing them.
    pub async fn check_secret_reveal(self: &Arc<Self>, service: &str, env: &str, who: integrations::Asker<'_>) -> Result<()> {
        let (svc, row) = self.service_env(service, env).await?;
        let r = self.resolve_vars(&svc, &row, row.active.as_deref().unwrap_or("")).await?;
        if r.sources.values().any(|s| *s == "secret") && !self.deps.secrets.may_reveal(who, &svc.project, env).await {
            return Err(Error::Forbidden(format!("this environment's values come from roll secrets, which you may not reveal in {env} (a shell or a job's output shows them)")));
        }
        Ok(())
    }

    /// The environment's variables: masked, or (`reveal`: who asks) in the clear. A value from
    /// roll secrets is shown only to whom roll secrets would show it; otherwise the reveal is a
    /// 403 naming them, never a partial answer.
    pub async fn variables(self: &Arc<Self>, service: &str, env: &str, reveal: Option<integrations::Asker<'_>>) -> Result<Value> {
        let (svc, row) = self.service_env(service, env).await?;
        let r = self.resolve_vars(&svc, &row, row.active.as_deref().unwrap_or("")).await?;
        if let Some(who) = reveal {
            let secret: Vec<&String> = r.sources.iter().filter(|(_, s)| **s == "secret").map(|(k, _)| k).collect();
            if !secret.is_empty() && !self.deps.secrets.may_reveal(who, &svc.project, env).await {
                return Err(Error::Forbidden(format!("{} come from roll secrets, which you may not reveal in {env}", secret.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", "))));
            }
            // A sealed one is never in the clear, to anyone.
            let m: serde_json::Map<String, Value> = r.vars.into_iter().filter(|(k, _)| !r.sealed.contains(k)).map(|(k, v)| (k, Value::String(v))).collect();
            return Ok(Value::Object(m));
        }
        // A reference's template is names, not a value: shown, and what an edit starts from.
        let (sid, e) = (service.to_string(), env.to_string());
        let raw = self.db(move |s| s.variables(&sid, &e)).await?;
        let vars: Vec<VarView> = r
            .vars
            .iter()
            .map(|(k, v)| {
                let source = r.sources.get(k).copied().unwrap_or("service");
                // roll's own values aren't secret.
                let sealed = r.sealed.contains(k);
                let masked = if sealed { SEALED_MASK.to_string() } else if source == "roll" { v.clone() } else { mask(v) };
                let reference = (source == "reference").then(|| raw.get(k).cloned()).flatten();
                VarView { name: k.clone(), masked, source, reference, sealed }
            })
            .collect();
        Ok(json!({ "vars": vars }))
    }

    pub async fn change_variables(self: &Arc<Self>, service: &str, env: &str, body: Value) -> Result<Value> {
        self.service_env(service, env).await?;
        let o = body.as_object().ok_or_else(|| Error::BadRequest("the body is {set?, unset?}".into()))?;
        let mut set = BTreeMap::new();
        if let Some(s) = o.get("set") {
            for (k, v) in s.as_object().ok_or_else(|| Error::BadRequest("set is an object of names to values".into()))? {
                check_var_name(k)?;
                let v = v.as_str().ok_or_else(|| Error::BadRequest(format!("{k}'s value is a string")))?;
                if v.len() > 64 << 10 || v.contains('\0') {
                    return Err(Error::BadRequest(format!("{k}'s value is at most 64 KiB, without NUL")));
                }
                set.insert(k.clone(), v.to_string());
            }
        }
        let mut unset = Vec::new();
        if let Some(u) = o.get("unset") {
            for n in u.as_array().ok_or_else(|| Error::BadRequest("unset is a list of names".into()))? {
                unset.push(n.as_str().ok_or_else(|| Error::BadRequest("unset is a list of names".into()))?.to_string());
            }
        }
        // Seal: set and run with, never shown again (Railway's sealed variables).
        let mut seal = BTreeSet::new();
        if let Some(n) = o.get("seal") {
            for n in n.as_array().ok_or_else(|| Error::BadRequest("seal is a list of names".into()))? {
                seal.insert(n.as_str().ok_or_else(|| Error::BadRequest("seal is a list of names".into()))?.to_string());
            }
        }
        if set.len() + unset.len() + seal.len() > 500 {
            return Err(Error::BadRequest("at most 500 variables at once".into()));
        }
        let (sid, e) = (service.to_string(), env.to_string());
        let have = self.db(move |s| s.variables(&sid, &e)).await?;
        if let Some(n) = seal.iter().find(|n| unset.contains(n) || !(set.contains_key(*n) || have.contains_key(*n))) {
            return Err(Error::BadRequest(format!("{n} isn't one of the environment's own variables to seal")));
        }
        let (sid, e) = (service.to_string(), env.to_string());
        self.db(move |s| s.change_variables(&sid, &e, &set, &unset, &seal)).await?;
        self.variables(service, env, None).await
    }

    async fn service_env(self: &Arc<Self>, service: &str, env: &str) -> Result<(ServiceRow, EnvRow)> {
        let (sid, e) = (service.to_string(), env.to_string());
        self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await
    }

    // ── links ──

    pub async fn links(self: &Arc<Self>, service: &str) -> Result<Vec<Link>> {
        let sid = service.to_string();
        let svc = self.db(move |s| s.service(&sid)).await?;
        let sid = service.to_string();
        let rows = self.db(move |s| s.links_minted(&sid)).await?;
        let mut out = Vec::new();
        for (kind, r, prefix, minted) in rows {
            let names = match minted {
                Some(m) => Ok(m.vars),
                None => self.link_vars(&svc, kind, &r, "production").await,
            };
            let vars = names.map(|v| v.into_iter().map(|(k, _)| format!("{prefix}{k}")).collect()).unwrap_or_default();
            out.push(Link { kind, r#ref: r, prefix, vars });
        }
        Ok(out)
    }

    pub async fn put_link(self: &Arc<Self>, service: &str, kind: &str, r#ref: &str, body: Value) -> Result<Link> {
        let kind = LinkKind::parse(kind)?;
        let sid = service.to_string();
        let svc = self.db(move |s| s.service(&sid)).await?;
        // A service's URLs are named for it unless asked otherwise (`API_URL`): a bare `URL`
        // says nothing.
        let named = || {
            let rr = r#ref.to_string();
            async move {
                let missing = format!("no service {rr} in this project");
                let target = self.db(move |s| s.service(&rr)).await.map_err(|_| Error::BadRequest(missing))?;
                let mut p: String = target.name.to_ascii_uppercase().replace('-', "_");
                if p.starts_with(|c: char| c.is_ascii_digit()) {
                    p.insert_str(0, "SERVICE_");
                }
                Ok::<String, Error>(format!("{p}_"))
            }
        };
        let prefix = match body.get("prefix") {
            None | Some(Value::Null) if kind == LinkKind::Service => named().await?,
            Some(Value::String(p)) if p.is_empty() && kind == LinkKind::Service => named().await?,
            None | Some(Value::Null) => String::new(),
            Some(Value::String(p)) if p.is_empty() => String::new(),
            Some(Value::String(p)) => {
                check_var_name(p)?;
                if p.ends_with('_') { p.clone() } else { format!("{p}_") }
            }
            Some(_) => return Err(Error::BadRequest("prefix is a variable name prefix".into())),
        };
        // Credentials the link makes for itself, once: a link that's there already keeps its own
        // (setting its prefix again never makes another).
        let (sid, r) = (service.to_string(), r#ref.to_string());
        let had = self.db(move |s| s.links_minted(&sid)).await?.into_iter().find(|(k, rr, _, _)| *k == kind && *rr == r).and_then(|(_, _, _, m)| m);
        let fresh = had.is_none();
        let minted = match had {
            Some(m) => Some(m),
            None => self.deps.resources.link(kind, r#ref, service).await.map_err(|e| Error::BadRequest(e.to_string()))?,
        };
        let vars = match &minted {
            Some(m) => m.vars.clone(),
            None => self.link_vars(&svc, kind, r#ref, "production").await.map_err(|e| Error::BadRequest(e.to_string()))?,
        };
        let (sid, r, p, m) = (service.to_string(), r#ref.to_string(), prefix.clone(), minted.clone());
        let vars = match self.db(move |s| s.put_link(&sid, kind, &r, &p, m.as_ref())).await {
            Ok(true) => vars,
            // Another request made the link first, with credentials of its own: those stay,
            // and what this one made goes again.
            Ok(false) => {
                if let Some(m) = minted.as_ref().filter(|_| fresh) {
                    self.revoke_link(service, kind, r#ref, &m.kept).await;
                }
                let (sid, r) = (service.to_string(), r#ref.to_string());
                self.db(move |s| s.links_minted(&sid)).await?.into_iter().find(|(k, rr, _, _)| *k == kind && *rr == r).and_then(|(_, _, _, m)| m).map_or(vars, |m| m.vars)
            }
            Err(e) => {
                // Not kept: what was just made for it goes again.
                if let Some(m) = minted.as_ref().filter(|_| fresh) {
                    let _ = self.deps.resources.unlink(kind, r#ref, &m.kept).await;
                }
                return Err(e);
            }
        };
        Ok(Link { kind, r#ref: r#ref.to_string(), vars: vars.into_iter().map(|(k, _)| format!("{prefix}{k}")).collect(), prefix })
    }

    /// A link's variables now, as `service` gets them in `env`: another service's URLs (apps'
    /// own), else the resource's through [`crate::integrations::Resources`].
    async fn link_vars(self: &Arc<Self>, service: &ServiceRow, kind: LinkKind, r#ref: &str, env: &str) -> anyhow::Result<Vec<(String, String)>> {
        if kind != LinkKind::Service {
            return self.deps.resources.link_vars(kind, r#ref).await;
        }
        let (rr, e) = (r#ref.to_string(), env.to_string());
        let (target, row) = self.db(move |s| Ok((s.service(&rr)?, s.environment(&rr, &e).ok()))).await.map_err(|_| anyhow::anyhow!("no service {ref} in this project"))?;
        // Checked at each read: a service moved to another project stops being reachable.
        anyhow::ensure!(target.project == service.project && target.id != service.id, "no service {ref} in this project");
        anyhow::ensure!(target.run.kind == RunType::Web, "{} isn't a web service: only a web service has URLs", target.name);
        // The port its live deployment listens on (a Dockerfile's EXPOSE, an environment's own).
        let active = row.as_ref().and_then(|r| r.active.clone());
        let port = match active {
            Some(a) => self.db(move |s| s.deployment(&a)).await.ok().and_then(|d| d.run.port),
            None => None,
        };
        let port = port.or(target.run.port).unwrap_or(8080);
        let mut v = vec![("URL".to_string(), format!("http://{}.{env}.internal:{port}", target.name))];
        if let Some(row) = row.filter(|_| target.run.public) {
            v.push(("PUBLIC_URL".to_string(), self.url(&row.slug)));
        }
        Ok(v)
    }

    pub async fn delete_link(self: &Arc<Self>, service: &str, kind: &str, r#ref: &str) -> Result<()> {
        let kind = LinkKind::parse(kind)?;
        let sid = service.to_string();
        let minted = self.db(move |s| s.links_minted(&sid)).await?.into_iter().find(|(k, rr, _, _)| *k == kind && rr == r#ref).and_then(|(_, _, _, m)| m);
        let (sid, r) = (service.to_string(), r#ref.to_string());
        self.db(move |s| s.delete_link(&sid, kind, &r)).await?;
        if let Some(m) = minted {
            self.revoke_link(service, kind, r#ref, &m.kept).await;
        }
        Ok(())
    }

    /// Revoke a link's own credentials (a failure is in the log: the link is gone either way).
    async fn revoke_link(self: &Arc<Self>, service: &str, kind: LinkKind, r#ref: &str, kept: &str) {
        if let Err(e) = self.deps.resources.unlink(kind, r#ref, kept).await {
            tracing::warn!(service, kind = kind.as_str(), r#ref, error = %e, "apps: a link's credentials couldn't be revoked");
        }
    }

    // ── domains ──

    fn target(&self, slug: &str) -> String {
        self.cfg.cname_target.clone().unwrap_or_else(|| format!("{slug}.{}", self.cfg.domain))
    }

    /// The records a custom domain needs. A CNAME to an environment's own target proves the
    /// hostname is its owner's; a shared target (`cname_target`, Cloudflare for SaaS) is every
    /// tenant's, so there the TXT challenge proves it and the CNAME only routes ("both").
    fn record(&self, host: &str) -> &'static str {
        match record_for(host) {
            "cname" if self.cfg.cname_target.is_some() => "both",
            r => r,
        }
    }

    pub async fn domains(self: &Arc<Self>, service: &str, env: &str) -> Result<Vec<Domain>> {
        let (sid, e) = (service.to_string(), env.to_string());
        let row = self.db(move |s| s.environment(&sid, &e)).await?;
        let (sid, e) = (service.to_string(), env.to_string());
        let mut custom = self.db(move |s| s.domains(&sid, &e)).await?;
        let target = self.target(&row.slug);
        let issues = self.deps.tls.issues();
        for d in &mut custom {
            d.target = target.clone();
            d.record = self.record(&d.hostname);
            if !issues {
                (d.tls, d.tls_error) = (TlsStatus::None, None);
            }
        }
        let own = Domain {
            hostname: format!("{}.{}", row.slug, self.cfg.domain),
            record: "none",
            kind: DomainKind::Roll,
            status: DomainStatus::Active,
            target: String::new(),
            token: String::new(),
            error: None,
            // The edge's wildcard covers it.
            tls: if self.cfg.scheme == "https" { TlsStatus::Issued } else { TlsStatus::None },
            tls_error: None,
            created_at: row.created_at,
            redirect_to: None,
        };
        Ok(std::iter::once(own).chain(custom).collect())
    }

    pub async fn add_domain(self: &Arc<Self>, service: &str, env: &str, body: Value) -> Result<Domain> {
        let host = body.get("hostname").and_then(Value::as_str).ok_or_else(|| Error::BadRequest("hostname is required".into()))?;
        let mut reserved: Vec<&str> = self.cfg.reserved.iter().map(String::as_str).collect();
        reserved.push(&self.cfg.domain);
        let host = check_hostname(host, &reserved)?;
        if host.starts_with("*.") && self.deps.tls.issues() && !self.deps.tls.wildcards() {
            return Err(Error::BadRequest(format!("{host}: a wildcard's certificate needs DNS-01, which this node's certificates don't do yet; add the names it would cover one by one")));
        }
        let (sid, e) = (service.to_string(), env.to_string());
        let row = self.db(move |s| s.environment(&sid, &e)).await?;
        let tls = if self.deps.tls.issues() { TlsStatus::Pending } else { TlsStatus::None };
        let (sid, e, h) = (service.to_string(), env.to_string(), host.clone());
        let mut d = self.db(move |s| s.add_domain(&sid, &e, &h, tls)).await?;
        // Only now, accepted and claimed, does the hostname go to the certificate's provider.
        if let Some((tls, tls_error)) = self.refresh_tls(&host).await {
            (d.tls, d.tls_error) = (tls, tls_error);
        }
        d.target = self.target(&row.slug);
        d.record = self.record(&d.hostname);
        Ok(d)
    }

    /// Ask the certificate's provider where a custom domain's certificate is, and keep the answer.
    /// `None`: the provider doesn't say (the edge reports on its own) or nothing issues.
    async fn refresh_tls(self: &Arc<Self>, host: &str) -> Option<(TlsStatus, Option<String>)> {
        if !self.deps.tls.issues() {
            return None;
        }
        let (tls, error) = match self.deps.tls.ensure(host).await {
            Ok(Some(state)) => state,
            Ok(None) => return None,
            // The provider didn't answer: still pending, saying why; checked again later.
            Err(e) => (TlsStatus::Pending, Some(format!("{e:#}"))),
        };
        let (h, err) = (host.to_string(), error.clone());
        self.db(move |s| s.set_domain_tls(&h, tls, err.as_deref())).await.ok()?;
        Some((tls, error))
    }

    /// A custom domain's certificate state, from whatever issues it (the edge's reports).
    pub async fn set_domain_tls(self: &Arc<Self>, host: &str, tls: TlsStatus, error: Option<String>) -> Result<bool> {
        let h = host.to_ascii_lowercase();
        self.db(move |s| s.set_domain_tls(&h, tls, error.as_deref())).await
    }

    /// Every active custom domain's hostname (what the edge holds certificates for).
    pub async fn active_custom_domains(self: &Arc<Self>) -> Result<Vec<String>> {
        self.db(|s| s.active_custom_domains()).await
    }

    /// Check a custom domain's DNS now (a CNAME to its target, or the TXT challenge), and where
    /// its certificate is.
    pub async fn verify_domain(self: &Arc<Self>, service: &str, env: &str, host: &str) -> Result<Domain> {
        let h = host.to_ascii_lowercase();
        let (sid, e) = (service.to_string(), env.to_string());
        let row = self.db(move |s| s.environment(&sid, &e)).await?;
        let found = {
            let h = h.clone();
            self.db(move |s| s.domain_owner(&h)).await?
        };
        let Some((_, _, mut d)) = found.filter(|(s, e, _)| s == service && e == env) else {
            return Err(Error::NotFound(format!("{h} isn't a domain of {service} in {env}")));
        };
        let target = self.target(&row.slug);
        let cname_ok = self.deps.resolver.cname(&h).await.contains(&target);
        let challenge = crate::model::challenge_name(&h);
        let txt_ok = self.deps.resolver.txt(&challenge).await.contains(&d.token);
        let shared = self.cfg.cname_target.is_some();
        let (status, error) = if txt_ok || (cname_ok && !shared) {
            (DomainStatus::Active, None)
        } else if shared {
            (DomainStatus::Pending, Some(format!("no TXT {challenge} with the token yet (the CNAME to {target} routes it; the TXT record proves it's yours)")))
        } else {
            (DomainStatus::Pending, Some(format!("no CNAME to {target} and no TXT {challenge} with the token yet")))
        };
        let (hh, err) = (h.clone(), error.clone());
        self.db(move |s| s.set_domain_status(&hh, status, err.as_deref())).await?;
        if let Some((tls, tls_error)) = self.refresh_tls(&h).await {
            (d.tls, d.tls_error) = (tls, tls_error);
        }
        d.status = status;
        d.error = error;
        d.target = target;
        d.record = self.record(&d.hostname);
        Ok(d)
    }

    pub async fn delete_domain(self: &Arc<Self>, service: &str, env: &str, host: &str) -> Result<()> {
        let (sid, e, h) = (service.to_string(), env.to_string(), host.to_ascii_lowercase());
        self.db(move |s| s.delete_domain(&sid, &e, &h)).await?;
        self.release_domains(&[host.to_ascii_lowercase()]).await;
        Ok(())
    }

    /// Tell the certificate's provider these hostnames are gone (their rows already are, so the
    /// claim is free whatever it answers).
    async fn release_domains(&self, hosts: &[String]) {
        for h in hosts {
            if let Err(e) = self.deps.tls.remove(h).await {
                tracing::warn!(host = %h, error = %e, "apps: a removed domain is still at its certificate's provider");
            }
        }
    }

    /// Remove the custom domains still unverified `older_than` after they were added: the
    /// hostname is free for its real owner. Returns them.
    pub async fn expire_domain_claims(self: &Arc<Self>, older_than: Duration) -> Result<Vec<String>> {
        let age = older_than.as_millis() as i64;
        // The database's clock, which stamped them.
        let expired = self.db(move |s| s.expire_domain_claims(s.now()? - age)).await?;
        let hosts: Vec<String> = expired.into_iter().map(|(svc, env, h)| {
            tracing::info!(service = %svc, env = %env, host = %h, "apps: an unverified custom domain expired");
            h
        }).collect();
        self.release_domains(&hosts).await;
        Ok(hosts)
    }

    /// `PATCH …/domains/{host} {redirect_to}`: send a custom domain's requests to another of the
    /// environment's hostnames (its own, or another custom domain that doesn't redirect), or
    /// (`null`) serve them again.
    pub async fn set_domain_redirect(self: &Arc<Self>, service: &str, env: &str, host: &str, body: Value) -> Result<Domain> {
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| k.as_str() != "redirect_to") {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        let h = host.to_ascii_lowercase();
        let to = match o.get("redirect_to") {
            None | Some(Value::Null) => None,
            Some(Value::String(t)) => Some(t.trim().trim_end_matches('.').to_ascii_lowercase()),
            Some(_) => return Err(Error::BadRequest("redirect_to is a hostname or null".into())),
        };
        let all = self.domains(service, env).await?;
        if !all.iter().any(|d| d.kind == DomainKind::Custom && d.hostname == h) {
            return Err(Error::NotFound(format!("{h} isn't a custom domain of {service} in {env}")));
        }
        if let Some(t) = &to {
            if t.starts_with("*.") {
                return Err(Error::BadRequest("redirect_to is one hostname, not a wildcard".into()));
            }
            let target = all.iter().find(|d| &d.hostname == t).ok_or_else(|| Error::BadRequest(format!("{t} isn't a hostname of this environment")))?;
            if *t == h || target.redirect_to.is_some() {
                return Err(Error::BadRequest("redirect_to is another hostname of the environment that serves it (no loops or chains)".into()));
            }
            if all.iter().any(|d| d.redirect_to.as_deref() == Some(h.as_str())) {
                return Err(Error::BadRequest(format!("another domain redirects to {h}: it serves, so it can't redirect")));
            }
        }
        let (hh, t) = (h.clone(), to.clone());
        self.db(move |s| s.set_redirect(&hh, t.as_deref())).await?;
        self.domains(service, env).await?.into_iter().find(|d| d.hostname == h).ok_or_else(|| Error::NotFound(h.clone()))
    }

    /// Where an active custom domain redirects, if it does.
    pub async fn redirect_for_host(self: &Arc<Self>, host: &str) -> Option<String> {
        let host = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(h, _)| h);
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if host.ends_with(&format!(".{}", self.cfg.domain)) {
            return None;
        }
        self.active_domain(&host).await.and_then(|(_, _, d)| d.redirect_to)
    }

    /// The active custom domain a hostname is: its own, else the wildcard one label above it.
    async fn active_domain(self: &Arc<Self>, host: &str) -> Option<(String, String, Domain)> {
        let exact = host.to_string();
        if let Some(d) = self.db(move |s| s.domain_owner(&exact)).await.ok().flatten().filter(|(_, _, d)| d.status == DomainStatus::Active) {
            return Some(d);
        }
        let wild = crate::model::covering_wildcard(host)?;
        self.db(move |s| s.domain_owner(&wild)).await.ok().flatten().filter(|(_, _, d)| d.status == DomainStatus::Active)
    }

    /// Write the audit trail's entry for a change someone made (`audit.<what>`): who, and what
    /// changed, never a value. Best effort: the change already happened.
    pub async fn audit(self: &Arc<Self>, what: &str, service: &str, env: &str, actor: Option<String>, mut data: Value) {
        data["actor"] = json!(actor);
        let (kind, sid, e) = (format!("audit.{what}"), service.to_string(), env.to_string());
        if let Err(err) = self.db(move |s| s.add_event(&kind, &sid, &e, None, &data)).await {
            tracing::warn!(service, error = %err, "apps: an audit entry wasn't written");
        }
        self.changed(service);
    }

    /// `GET /v1/apps/services/{id}/activity`: its audit trail, newest first.
    pub async fn activity(self: &Arc<Self>, service: &str) -> Result<Vec<Event>> {
        let sid = service.to_string();
        self.db(move |s| {
            s.service(&sid)?;
            s.activity(&sid, 200)
        })
        .await
    }

    /// The environment a request's `Host` names: a generated hostname, or an active custom domain.
    /// The service and environment a hostname is, and the deployment when it's one's own
    /// (`<deployment>--<slug>`: the id without `dep_`).
    pub async fn env_for_host(self: &Arc<Self>, host: &str) -> Option<(String, String, Option<String>)> {
        let found = self.host_target(host).await?;
        // A private service has no public names: the same 404 as an address nobody has, read
        // from its settings now (making it private takes effect at once, not with a deploy).
        let sid = found.0.clone();
        // A TCP service's port isn't HTTP: its names don't serve it.
        self.db(move |s| s.service(&sid)).await.ok().is_some_and(|svc| svc.run.public && !svc.run.tcp).then_some(found)
    }

    async fn host_target(self: &Arc<Self>, host: &str) -> Option<(String, String, Option<String>)> {
        let host = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(h, _)| h);
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if let Some(label) = host.strip_suffix(&self.cfg.domain).and_then(|s| s.strip_suffix('.')).filter(|s| !s.contains('.')) {
            let slug = label.to_string();
            if let Some(e) = self.db(move |s| s.environment_by_slug(&slug)).await.ok().flatten() {
                return Some((e.service, e.name, None));
            }
            let (dep, slug) = label.split_once("--").filter(|(d, s)| d.len() == 12 && d.bytes().all(|b| b.is_ascii_alphanumeric()) && !s.is_empty())?;
            let (dep, slug) = (format!("dep_{dep}"), slug.to_string());
            return self.db(move |s| s.environment_by_slug(&slug)).await.ok().flatten().map(|e| (e.service, e.name, Some(dep)));
        }
        self.active_domain(&host).await.map(|(s, e, _)| (s, e, None))
    }

    /// What the environment's public names let through ([`live::Gate`]): the live state's copy,
    /// read from the store the first time after a change. Unreadable: the store's error.
    pub(crate) async fn gate(self: &Arc<Self>, live: &EnvLive) -> Result<Arc<live::Gate>> {
        if let Some(g) = live.gate() {
            return Ok(g);
        }
        let seen = live.gate_gen();
        let (sid, e) = (live.service.clone(), live.env.clone());
        let row = self.db(move |s| s.environment(&sid, &e)).await?;
        let allow = row.allow_ips.iter().filter_map(|r| r.cidr.parse::<ipnet::IpNet>().ok()).collect();
        let g = Arc::new(live::Gate { maintenance: row.maintenance, allow, rate: row.rate_limit, firewall: row.firewall });
        live.set_gate(g.clone(), seen);
        Ok(g)
    }

    /// Where a preview's feedback button reads what's been said (roll previews, once it's on this
    /// node). Set once.
    pub fn set_feedback_notes(&self, notes: Arc<dyn integrations::FeedbackNotes>) {
        let _ = self.notes.set(notes);
    }

    pub(crate) fn feedback_notes(&self) -> Option<Arc<dyn integrations::FeedbackNotes>> {
        self.notes.get().cloned()
    }

    /// Where a preview's feedback goes (the preview in roll's dashboard), for a visitor who came
    /// in through its access link, when roll previews turned the button on and the dashboard is
    /// known.
    pub(crate) async fn feedback_target(self: &Arc<Self>, service: &str, env: &str) -> Option<String> {
        let web = self.cfg.web_url.clone()?;
        let preview = env.strip_prefix("pv-")?.to_string();
        let (sid, e) = (service.to_string(), env.to_string());
        let (svc, row) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await.ok()?;
        (row.ephemeral && row.feedback).then(|| format!("{}/dashboard/projects/{}/previews?preview={preview}", web.trim_end_matches('/'), svc.project))
    }

    /// What a deployment's own URL runs: that deployment of a web service, built and not
    /// removed, in this environment. A service with a volume has none (one instance writes it).
    pub async fn deployment_template(self: &Arc<Self>, service: &str, env: &str, dep: &str) -> Result<live::Template> {
        let (sid, e, d) = (service.to_string(), env.to_string(), dep.to_string());
        let (svc, row, d) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?, s.deployment(&d)?))).await?;
        if d.service != service || d.environment != env || d.image.is_none() || d.status == DeployStatus::Removed {
            return Err(Error::NotFound(format!("no deployment {dep} to run here")));
        }
        if d.run.kind != RunType::Web {
            return Err(Error::Conflict("only a web service's deployments have their own URL".into()));
        }
        if d.run.volume.is_some() {
            return Err(Error::Conflict("a service with a volume has no deployment URLs: one instance writes the volume".into()));
        }
        self.template(&svc.for_env(&row)?, &row, &d).await
    }

    // ── logs, metrics, usage ──

    /// An environment's last finished minute in Prometheus' text format (Fly's metrics, Render's
    /// metrics streams): gauges labelled with the service, its id and the environment.
    pub async fn metrics_prometheus(self: &Arc<Self>, service: &str, env: &str) -> Result<String> {
        let (svc, _) = self.service_env(service, env).await?;
        // The last finished minute once it's stored (the meters are written every 10 s).
        let now = code_sql::now_ms();
        let minute = now / 60_000 - if now % 60_000 >= 15_000 { 1 } else { 2 };
        let (sid, e) = (service.to_string(), env.to_string());
        let r = self.db(move |s| s.minutes(&sid, &e, minute)).await?.into_iter().find(|r| r.minute == minute).unwrap_or(store::MinuteRow { minute, ..Default::default() });
        // Label values are names and ids roll made ([a-z0-9-_]), quoted as the format wants.
        let esc = |v: &str| v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
        let labels = format!("service=\"{}\",service_id=\"{}\",environment=\"{}\"", esc(&svc.name), esc(&svc.id), esc(env));
        let mut out = String::new();
        let mut gauge = |name: &str, help: &str, value: f64, extra: &str| {
            if !out.contains(&format!("# TYPE {name} ")) {
                out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n"));
            }
            out.push_str(&format!("{name}{{{labels}{extra}}} {value}\n"));
        };
        gauge("roll_apps_requests_per_minute", "Requests in the last finished minute.", r.requests as f64, "");
        gauge("roll_apps_errors_per_minute", "5xx answers in the last finished minute.", r.errors as f64, "");
        for (q, v) in [("0.5", r.p50_ms), ("0.95", r.p95_ms), ("0.99", r.p99_ms)] {
            gauge("roll_apps_latency_ms", "Request latency in the last finished minute, by quantile.", v, &format!(",quantile=\"{q}\""));
        }
        gauge("roll_apps_cpu_seconds_per_minute", "CPU time its instances used in the last finished minute.", r.cpu, "");
        gauge("roll_apps_memory_bytes", "Memory its instances held.", r.memory_bytes as f64, "");
        gauge("roll_apps_instances", "Instances that were up.", r.instances as f64, "");
        gauge("roll_apps_cold_starts_per_minute", "Cold starts in the last finished minute.", r.cold_starts as f64, "");
        gauge("roll_apps_cold_start_ms", "Their mean time to answer.", r.cold_start_ms, "");
        Ok(out)
    }

    pub async fn metrics(self: &Arc<Self>, service: &str, env: &str, minutes: i64) -> Result<Value> {
        self.service_env(service, env).await?;
        let minutes = minutes.clamp(5, 7 * 24 * 60);
        let now_min = code_sql::now_ms() / 60_000;
        let since = now_min - minutes + 1;
        let (sid, e) = (service.to_string(), env.to_string());
        let mut rows: HashMap<i64, store::MinuteRow> = self.db(move |s| s.minutes(&sid, &e, since)).await?.into_iter().map(|r| (r.minute, r)).collect();
        if let Some(l) = self.lives.lock().get(&env_key(service, env))
            && let Some(cur) = l.meter.lock().current()
        {
            rows.insert(cur.minute, cur);
        }
        let mut out: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
        for m in since..=now_min {
            let r = rows.remove(&m).unwrap_or(store::MinuteRow { minute: m, ..Default::default() });
            out.entry("minutes").or_default().push(m.into());
            out.entry("requests").or_default().push(r.requests.into());
            out.entry("errors").or_default().push(r.errors.into());
            out.entry("p50_ms").or_default().push(r.p50_ms.into());
            out.entry("p95_ms").or_default().push(r.p95_ms.into());
            out.entry("p99_ms").or_default().push(r.p99_ms.into());
            out.entry("cpu").or_default().push(r.cpu.into());
            out.entry("memory_bytes").or_default().push(r.memory_bytes.into());
            out.entry("instances").or_default().push(r.instances.into());
            out.entry("cold_starts").or_default().push(r.cold_starts.into());
            out.entry("cold_start_ms").or_default().push(r.cold_start_ms.into());
        }
        Ok(serde_json::to_value(out).expect("JSON"))
    }

    pub async fn usage(self: &Arc<Self>, service: &str, env: Option<&str>, days: i64) -> Result<Value> {
        let days = days.clamp(1, 93);
        let now_h = code_sql::now_ms() / 3_600_000;
        let today = now_h / 24;
        let since = (today - days + 1) * 24;
        let (sid, e) = (service.to_string(), env.map(str::to_string));
        let rows = self.db(move |s| s.usage(&sid, e.as_deref(), since)).await?;
        let mut by_day: BTreeMap<i64, store::UsageAdd> = BTreeMap::new();
        for (hour, u) in rows {
            let d = by_day.entry(hour / 24).or_default();
            d.cpu_seconds += u.cpu_seconds;
            d.gb_seconds += u.gb_seconds;
            d.requests += u.requests;
            d.egress_bytes += u.egress_bytes;
            d.build_seconds += u.build_seconds;
        }
        let p = self.cfg.prices;
        let mut total = 0.0;
        let days: Vec<Value> = ((today - days + 1)..=today)
            .map(|d| {
                let u = by_day.remove(&d).unwrap_or_default();
                let usd = p.cost(&u);
                total += usd;
                json!({ "day": d, "cpu_seconds": u.cpu_seconds, "gb_seconds": u.gb_seconds, "requests": u.requests, "egress_bytes": u.egress_bytes, "build_seconds": u.build_seconds, "usd": usd })
            })
            .collect();
        Ok(json!({ "days": days, "total_usd": total }))
    }

    /// Write every environment's finished minutes (and, when `all`, the current ones).
    pub async fn flush_meters(self: &Arc<Self>, all: bool) {
        let now = code_sql::now_ms();
        for l in self.lives() {
            let minutes = l.meter.lock().take(now, all);
            if minutes.is_empty() {
                continue;
            }
            let (svc, env) = (l.service.clone(), l.env.clone());
            let _ = self
                .db(move |s| {
                    s.put_minutes(&svc, &env, &minutes.iter().map(|m| m.row()).collect::<Vec<_>>())?;
                    for m in &minutes {
                        s.add_usage(&svc, &env, m.minute / 60, &m.usage())?;
                    }
                    Ok(())
                })
                .await;
        }
        self.logs.flush().await;
    }

    pub fn overview(&self) -> Value {
        json!({ "runtime": self.deps.runtime.name(), "domain": self.cfg.domain, "prices": self.cfg.prices, "secrets": self.deps.secrets.name() })
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.stop.clone()
    }
}

impl ResolvedVars {
    /// The variables that decide whether a deployment is stale (not roll's per-deployment ones).
    fn vars_for_hash(&self) -> Vec<(String, String)> {
        self.vars.iter().filter(|(k, _)| !matches!(k.as_str(), "ROLL_DEPLOYMENT")).cloned().collect()
    }
}

/// An apex (`example.com`, `example.co.uk`) can't hold a CNAME: it proves itself with the TXT
/// challenge. Anything deeper is a CNAME.
fn record_for(host: &str) -> &'static str {
    let labels = host.split('.').count();
    let second_level = ["co", "com", "org", "net", "ac", "gov", "edu"];
    let apex = labels == 2 || (labels == 3 && second_level.contains(&host.split('.').nth(1).unwrap_or("")));
    if apex { "txt" } else { "cname" }
}

pub fn volume_name(service: &str, env: &str) -> String {
    format!("roll-apps-vol-{}-{env}", service.strip_prefix("svc_").unwrap_or(service))
}

pub fn network_name(project: &str, env: &str) -> String {
    format!("roll-apps-net-{}-{env}", project.strip_prefix("prj_").unwrap_or(project))
}

/// A free spot on a project's canvas for a new card (the same grid the dashboard uses).
fn free_spot(ws: &roll_workspace::Workspace, project: &str) -> (f64, f64) {
    let taken: Vec<(f64, f64)> = ws.get(roll_workspace::Scope::All, project).map(|p| p.resources.iter().map(|r| (r.x, r.y)).collect()).unwrap_or_default();
    let (w, h) = (208.0, 96.0);
    for row in 0..50 {
        for col in 0..3 {
            let (x, y) = (48.0 + col as f64 * (w + 56.0), 48.0 + row as f64 * (h + 44.0));
            if taken.iter().all(|(tx, ty)| (tx - x).abs() >= w + 16.0 || (ty - y).abs() >= h + 24.0) {
                return (x, y);
            }
        }
    }
    (48.0, 48.0)
}
