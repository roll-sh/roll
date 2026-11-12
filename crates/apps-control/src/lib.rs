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
pub mod config;
pub mod cron;
pub mod deploy;
pub mod integrations;
pub mod jobs;
pub mod live;
pub mod logs;
pub mod metrics;
pub mod model;
pub mod public;
pub mod runtime;
pub mod shell;
pub mod store;

use integrations::{Resolver, Resources, SecretSource, Sources};
use live::{EnvLive, LiveConfig};
use logs::{LogHub, Stream};
use metrics::Prices;
use model::*;
use parking_lot::Mutex;
use runtime::{Builder, Runtime};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
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
}

impl AppsConfig {
    pub fn new(home: PathBuf) -> AppsConfig {
        AppsConfig {
            home,
            domain: "apps.localhost".into(),
            scheme: "http".into(),
            url_port: Some(7080),
            ports: None,
            reserved: Vec::new(),
            cname_target: None,
            build_timeout: Duration::from_secs(30 * 60),
            live: LiveConfig::default(),
            prices: Prices::default(),
            upload_max: 512 << 20,
            web_url: None,
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
    /// Private environments' token hashes, by environment, as the owner last read or set them.
    access: Mutex<HashMap<String, Option<String>>>,
    /// Shells open over HTTP (the dashboard's terminal), by id.
    sessions: Mutex<HashMap<String, Arc<shell::Session>>>,
    pub(crate) client: hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, axum::body::Body>,
    stop: CancellationToken,
}

fn env_key(service: &str, env: &str) -> String {
    format!("{service}/{env}")
}

/// A hostname label from `s`: lowercase, `[a-z0-9-]`, at most `max`.
fn label(s: &str, max: usize) -> String {
    let mut out: String = s.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
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
pub struct ResolvedVars {
    pub vars: Vec<(String, String)>,
    pub sources: HashMap<String, &'static str>,
}

impl Apps {
    pub fn new(cfg: AppsConfig, store: Store, deps: Deps) -> Arc<Apps> {
        let logs = LogHub::new(deps.objects.clone());
        Arc::new(Apps {
            cfg,
            store,
            deps,
            logs,
            lives: Mutex::new(HashMap::new()),
            queues: Mutex::new(HashMap::new()),
            cancels: Mutex::new(HashMap::new()),
            access: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            client: hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(30)).build_http(),
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
            Some(ws) => match ws.get(project) {
                Ok(p) => Ok(p.name),
                Err(roll_workspace::Error::NotFound(_)) => Err(Error::BadRequest(format!("no project {project}"))),
                Err(e) => Err(Error::Internal(e.to_string())),
            },
            None => Ok(project.strip_prefix("prj_").unwrap_or(project).to_string()),
        }
    }

    pub async fn create_service(self: &Arc<Self>, body: Value) -> Result<Service> {
        let o = body.as_object().ok_or_else(|| Error::BadRequest("the body is an object".into()))?;
        for k in o.keys() {
            if !["project", "name", "source", "build", "run", "variables", "x", "y"].contains(&k.as_str()) {
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
        let (x, y) = (o.get("x").and_then(Value::as_f64), o.get("y").and_then(Value::as_f64));
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
                ws.place(&project, roll_workspace::Kind::Service, &id, x, y)
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
        // A repo or an image deploys at once; an upload waits for `roll up`.
        if !matches!(source, Source::Upload) {
            self.deploy(&row.id, PRODUCTION, json!({}), Trigger::Api, None, None).await?;
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
        Ok(Service { id: r.id, project: r.project, name: r.name, source: r.source, build: r.build, run: r.run, created_at: r.created_at, environments: summaries })
    }

    pub fn url(&self, slug: &str) -> String {
        let port = self.cfg.url_port.filter(|p| !((self.cfg.scheme == "http" && *p == 80) || (self.cfg.scheme == "https" && *p == 443)));
        match port {
            Some(p) => format!("{}://{slug}.{}:{p}", self.cfg.scheme, self.cfg.domain),
            None => format!("{}://{slug}.{}", self.cfg.scheme, self.cfg.domain),
        }
    }

    async fn summary(self: &Arc<Self>, e: &EnvRow) -> Result<EnvironmentSummary> {
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
        Ok(EnvironmentSummary { name: e.name.clone(), state, url: Some(self.url(&e.slug)), active, latest })
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
        if let Some(s) = o.get("source") {
            let source: Source = serde_json::from_value(s.clone()).map_err(|e| Error::BadRequest(format!("source: {e}")))?;
            source.check()?;
            row.source = source;
        }
        row.build = merge(&row.build, o.get("build"), "build")?;
        row.build.check()?;
        row.run = row.run.merged(o.get("run"))?;
        let r = row.clone();
        self.db(move |s| s.update_service(&r)).await?;
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
        let sid = id.to_string();
        let deps = self.db(move |s| s.deployments_all(&sid)).await?;
        for d in deps {
            if let Some(img) = &d.image {
                self.deps.builder.remove_image(img).await;
            }
            let id = d.id.clone();
            if let Ok(Some(key)) = self.db(move |s| s.deployment_upload(&id)).await {
                self.drop_upload(&d.id, &key).await;
            }
            self.logs.delete(&format!("build:{}", d.id)).await;
        }
        let sid = id.to_string();
        self.db(move |s| s.delete_service(&sid)).await?;
        if let Some(ws) = self.deps.workspace.clone() {
            let sid = id.to_string();
            let _ = tokio::task::spawn_blocking(move || {
                if let Some(p) = ws.list().ok().and_then(|ps| ps.into_iter().find(|p| p.resources.iter().any(|r| r.kind == roll_workspace::Kind::Service && r.r#ref == sid))) {
                    let _ = ws.remove(&p.id, roll_workspace::Kind::Service, &sid);
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

    async fn environment_view(self: &Arc<Self>, r: EnvRow) -> Result<Environment> {
        let summary = self.summary(&r).await?;
        let instances = match self.lives.lock().get(&env_key(&r.service, &r.name)) {
            Some(l) => l.instances().iter().map(|i| Instance { id: i.name.clone(), deployment: i.deployment.clone(), state: i.state(), started_at: i.started_at(), in_flight: i.in_flight() }).collect(),
            None => Vec::new(),
        };
        let domains = self.domains(&r.service, &r.name).await?;
        let sid = r.service.clone();
        let svc = self.db(move |s| s.service(&sid)).await?;
        let next_run_at = match (&svc.run.schedule, &r.active) {
            (Some(sched), Some(_)) if svc.run.kind == RunType::Cron => crate::cron::Schedule::parse(sched).ok().and_then(|s| s.next_after(code_sql::now_ms())),
            _ => None,
        };
        let needs_deploy = match &r.active {
            Some(a) => {
                let a = a.clone();
                let dep = self.db(move |s| s.deployment(&a)).await?;
                match self.resolve_vars(&svc, &r, &dep.id).await {
                    Ok(v) => fingerprint(&v.vars_for_hash(), &svc) != dep.vars_hash,
                    Err(_) => false,
                }
            }
            None => false,
        };
        let private = self.access_hash(&r.service, &r.name).await?.is_some();
        Ok(Environment { summary, service: r.service.clone(), branch: r.branch.clone(), ephemeral: r.ephemeral, instances, domains, port: r.port, needs_deploy, private, next_run_at, created_at: r.created_at })
    }

    /// `PUT …/environments/{env}`: make it (a preview environment, a staging one) or change its
    /// branch.
    pub async fn put_environment(self: &Arc<Self>, service: &str, env: &str, body: Value) -> Result<Environment> {
        let o = body.as_object().cloned().unwrap_or_default();
        for k in o.keys() {
            if !["branch", "ephemeral", "access"].contains(&k.as_str()) {
                return Err(Error::BadRequest(format!("unknown field {k}")));
            }
        }
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
        let access: Option<Option<String>> = match o.get("access") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::Object(a)) => match a.get("token").and_then(Value::as_str) {
                Some(t) if a.len() == 1 && (20..=200).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)) => Some(Some(crate::public::token_hash(t))),
                _ => return Err(Error::BadRequest("access is {token} (20–200 of A-Z, a-z, 0-9, - and _) or null".into())),
            },
            Some(_) => return Err(Error::BadRequest("access is {token} or null".into())),
        };
        let sid = service.to_string();
        let svc = self.db(move |s| s.service(&sid)).await?;
        let pname = {
            let this = self.clone();
            let p = svc.project.clone();
            tokio::task::spawn_blocking(move || this.project_name(&p)).await.map_err(|e| Error::Internal(e.to_string()))??
        };
        self.ensure_environment(&svc, env, branch.as_ref().map(|b| b.as_deref()), ephemeral, &pname).await?;
        if let Some(hash) = access {
            let (sid, e, h) = (service.to_string(), env.to_string(), hash.clone());
            self.db(move |s| s.set_access(&sid, &e, h.as_deref())).await?;
            self.access.lock().insert(env_key(service, env), hash);
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
        self.db(move |s| s.delete_environment(&sid, &e)).await?;
        self.teardown_live(service, env).await;
        self.access.lock().remove(&env_key(service, env));
        self.deps.runtime.remove_volume(&volume_name(service, env)).await;
        self.logs.forget(&format!("env:{service}/{env}"));
        Ok(())
    }

    /// A private environment's token hash (none: public), cached on its owner.
    pub async fn access_hash(self: &Arc<Self>, service: &str, env: &str) -> Result<Option<String>> {
        if let Some(h) = self.access.lock().get(&env_key(service, env)) {
            return Ok(h.clone());
        }
        let (sid, e) = (service.to_string(), env.to_string());
        let h = self.db(move |s| s.access_hash(&sid, &e)).await?;
        self.access.lock().insert(env_key(service, env), h.clone());
        Ok(h)
    }

    async fn teardown_live(&self, service: &str, env: &str) {
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
            .or_insert_with(|| EnvLive::new(service, env, self.deps.runtime.clone(), self.cfg.live.clone(), self.logs.sink(format!("env:{service}/{env}"), Stream::System)))
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
        let links = self.db(move |s| s.links(&sid)).await?;
        for (kind, r, prefix) in links {
            match self.deps.resources.link_vars(kind, &r).await {
                Ok(vs) => {
                    for (k, v) in vs {
                        vars.insert(format!("{prefix}{k}"), (self.deps.runtime.reach(&v), "link"));
                    }
                }
                Err(e) => tracing::warn!(service = %svc.id, kind = kind.as_str(), r#ref = %r, error = %e, "apps: a link's variables couldn't be read"),
            }
        }
        match self.deps.secrets.resolve(&svc.project, &env.name, &svc.id).await {
            Ok(secrets) => {
                for (k, v) in secrets {
                    vars.insert(k, (v, "secret"));
                }
            }
            Err(e) => return Err(Error::Internal(format!("the project's {} secrets couldn't be read: {e}", env.name))),
        }
        let (sid, e) = (svc.id.clone(), env.name.clone());
        for (k, v) in self.db(move |s| s.variables(&sid, &e)).await? {
            vars.insert(k, (v, "service"));
        }
        let sources = vars.iter().map(|(k, (_, s))| (k.clone(), *s)).collect();
        Ok(ResolvedVars { vars: vars.into_iter().map(|(k, (v, _))| (k, v)).collect(), sources })
    }

    pub async fn variables(self: &Arc<Self>, service: &str, env: &str, reveal: bool) -> Result<Value> {
        let (svc, row) = self.service_env(service, env).await?;
        let r = self.resolve_vars(&svc, &row, row.active.as_deref().unwrap_or("")).await?;
        if reveal {
            let m: serde_json::Map<String, Value> = r.vars.into_iter().map(|(k, v)| (k, Value::String(v))).collect();
            return Ok(Value::Object(m));
        }
        let vars: Vec<VarView> = r
            .vars
            .iter()
            .map(|(k, v)| {
                let source = r.sources.get(k).copied().unwrap_or("service");
                // roll's own values aren't secret.
                let masked = if source == "roll" { v.clone() } else { mask(v) };
                VarView { name: k.clone(), masked, source }
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
        if set.len() + unset.len() > 500 {
            return Err(Error::BadRequest("at most 500 variables at once".into()));
        }
        let (sid, e) = (service.to_string(), env.to_string());
        self.db(move |s| s.change_variables(&sid, &e, &set, &unset)).await?;
        self.variables(service, env, false).await
    }

    async fn service_env(self: &Arc<Self>, service: &str, env: &str) -> Result<(ServiceRow, EnvRow)> {
        let (sid, e) = (service.to_string(), env.to_string());
        self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await
    }

    // ── links ──

    pub async fn links(self: &Arc<Self>, service: &str) -> Result<Vec<Link>> {
        let sid = service.to_string();
        self.db(move |s| s.service(&sid)).await?;
        let sid = service.to_string();
        let rows = self.db(move |s| s.links(&sid)).await?;
        let mut out = Vec::new();
        for (kind, r, prefix) in rows {
            let vars = self.deps.resources.link_vars(kind, &r).await.map(|v| v.into_iter().map(|(k, _)| format!("{prefix}{k}")).collect()).unwrap_or_default();
            out.push(Link { kind, r#ref: r, prefix, vars });
        }
        Ok(out)
    }

    pub async fn put_link(self: &Arc<Self>, service: &str, kind: &str, r#ref: &str, body: Value) -> Result<Link> {
        let kind = LinkKind::parse(kind)?;
        let sid = service.to_string();
        self.db(move |s| s.service(&sid)).await?;
        let prefix = match body.get("prefix") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(p)) if p.is_empty() => String::new(),
            Some(Value::String(p)) => {
                check_var_name(p)?;
                if p.ends_with('_') { p.clone() } else { format!("{p}_") }
            }
            Some(_) => return Err(Error::BadRequest("prefix is a variable name prefix".into())),
        };
        let vars = self.deps.resources.link_vars(kind, r#ref).await.map_err(|e| Error::BadRequest(e.to_string()))?;
        let (sid, r, p) = (service.to_string(), r#ref.to_string(), prefix.clone());
        self.db(move |s| s.put_link(&sid, kind, &r, &p)).await?;
        Ok(Link { kind, r#ref: r#ref.to_string(), vars: vars.into_iter().map(|(k, _)| format!("{prefix}{k}")).collect(), prefix })
    }

    pub async fn delete_link(self: &Arc<Self>, service: &str, kind: &str, r#ref: &str) -> Result<()> {
        let kind = LinkKind::parse(kind)?;
        let (sid, r) = (service.to_string(), r#ref.to_string());
        self.db(move |s| s.delete_link(&sid, kind, &r)).await
    }

    // ── domains ──

    fn target(&self, slug: &str) -> String {
        self.cfg.cname_target.clone().unwrap_or_else(|| format!("{slug}.{}", self.cfg.domain))
    }

    pub async fn domains(self: &Arc<Self>, service: &str, env: &str) -> Result<Vec<Domain>> {
        let (sid, e) = (service.to_string(), env.to_string());
        let row = self.db(move |s| s.environment(&sid, &e)).await?;
        let (sid, e) = (service.to_string(), env.to_string());
        let mut custom = self.db(move |s| s.domains(&sid, &e)).await?;
        let target = self.target(&row.slug);
        for d in &mut custom {
            d.target = target.clone();
            d.record = record_for(&d.hostname);
        }
        let own = Domain {
            hostname: format!("{}.{}", row.slug, self.cfg.domain),
            record: "none",
            kind: DomainKind::Roll,
            status: DomainStatus::Active,
            target: String::new(),
            token: String::new(),
            error: None,
            created_at: row.created_at,
        };
        Ok(std::iter::once(own).chain(custom).collect())
    }

    pub async fn add_domain(self: &Arc<Self>, service: &str, env: &str, body: Value) -> Result<Domain> {
        let host = body.get("hostname").and_then(Value::as_str).ok_or_else(|| Error::BadRequest("hostname is required".into()))?;
        let mut reserved: Vec<&str> = self.cfg.reserved.iter().map(String::as_str).collect();
        reserved.push(&self.cfg.domain);
        let host = check_hostname(host, &reserved)?;
        let (sid, e) = (service.to_string(), env.to_string());
        let row = self.db(move |s| s.environment(&sid, &e)).await?;
        let (sid, e, h) = (service.to_string(), env.to_string(), host.clone());
        let mut d = self.db(move |s| s.add_domain(&sid, &e, &h)).await?;
        d.target = self.target(&row.slug);
        d.record = record_for(&d.hostname);
        Ok(d)
    }

    /// Check a custom domain's DNS now: a CNAME to its target, or the TXT challenge.
    pub async fn verify_domain(self: &Arc<Self>, service: &str, env: &str, host: &str) -> Result<Domain> {
        let h = host.to_ascii_lowercase();
        let (sid, e) = (service.to_string(), env.to_string());
        let row = self.db(move |s| s.environment(&sid, &e)).await?;
        let found = {
            let h = h.clone();
            self.db(move |s| s.domain_owner(&h)).await?
        };
        let Some((s, en, mut d)) = found.filter(|(s, e, _)| s == service && e == env) else {
            return Err(Error::NotFound(format!("{h} isn't a domain of {service} in {env}")));
        };
        let _ = (s, en);
        let target = self.target(&row.slug);
        let cname_ok = self.deps.resolver.cname(&h).await.contains(&target);
        let txt_ok = self.deps.resolver.txt(&format!("_roll-challenge.{h}")).await.contains(&d.token);
        let (status, error) = if cname_ok || txt_ok {
            (DomainStatus::Active, None)
        } else {
            (DomainStatus::Pending, Some(format!("no CNAME to {target} and no TXT _roll-challenge.{h} with the token yet")))
        };
        let (hh, err) = (h.clone(), error.clone());
        self.db(move |s| s.set_domain_status(&hh, status, err.as_deref())).await?;
        d.status = status;
        d.error = error;
        d.target = target;
        d.record = record_for(&d.hostname);
        Ok(d)
    }

    pub async fn delete_domain(self: &Arc<Self>, service: &str, env: &str, host: &str) -> Result<()> {
        let (sid, e, h) = (service.to_string(), env.to_string(), host.to_ascii_lowercase());
        self.db(move |s| s.delete_domain(&sid, &e, &h)).await
    }

    /// The environment a request's `Host` names: a generated hostname, or an active custom domain.
    pub async fn env_for_host(self: &Arc<Self>, host: &str) -> Option<(String, String)> {
        let host = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(h, _)| h);
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if let Some(slug) = host.strip_suffix(&self.cfg.domain).and_then(|s| s.strip_suffix('.')).filter(|s| !s.contains('.')) {
            let slug = slug.to_string();
            return self.db(move |s| s.environment_by_slug(&slug)).await.ok().flatten().map(|e| (e.service, e.name));
        }
        self.db(move |s| s.domain_owner(&host)).await.ok().flatten().filter(|(_, _, d)| d.status == DomainStatus::Active).map(|(s, e, _)| (s, e))
    }

    // ── logs, metrics, usage ──

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
    let taken: Vec<(f64, f64)> = ws.get(project).map(|p| p.resources.iter().map(|r| (r.x, r.y)).collect()).unwrap_or_default();
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
