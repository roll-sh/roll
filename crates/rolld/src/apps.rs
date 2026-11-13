//! roll apps in rolld (docs/apps/README.md): `/v1/apps` on the API, the apps listener for
//! services' hostnames (`ROLL_APPS_LISTEN`), and each environment's own port in a range
//! (`ROLL_APPS_PORTS`, one node only). Instances and builds run in Docker on this host, or as
//! Fargate tasks and CodeBuild builds with `ROLL_APPS_RUNTIME=fargate`.
//!
//! What roll apps needs from the other products is adapted here: trees and the land log from
//! roll code, connection variables from roll postgres and roll redis, custom domains'
//! certificates from the edge (`ROLL_APPS_TLS`: Cloudflare for SaaS, or the active domains
//! handed to the edge's HTTP-01 orders and their state handed back).

use apps_control::integrations::{DomainTls, Doh, EdgeTls, Landing, NoSecrets, NoTls, Resources, Revision, Sources};
use apps_control::model::{LinkKind, TlsStatus};
use apps_control::store::{Sealer, Store};
use apps_control::{Apps, AppsConfig, Deps};
use async_trait::async_trait;
use clap::Args;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};

#[derive(Args, Clone, Debug)]
pub struct AppsArgs {
    /// Where services' hostnames (`<slug>.<ROLL_APPS_DOMAIN>`) and custom domains are served.
    #[arg(id = "apps_listen", long = "apps-listen", env = "ROLL_APPS_LISTEN", default_value = "127.0.0.1:7080")]
    pub listen: SocketAddr,
    /// Generated hostnames are `<slug>.<this>` (`up.roll.site` behind the edge).
    #[arg(id = "apps_domain", long = "apps-domain", env = "ROLL_APPS_DOMAIN")]
    pub domain: Option<String>,
    /// Give each environment a port of its own from this range (`58100-58199`), for machines
    /// without wildcard DNS. One node only.
    #[arg(id = "apps_ports", long = "apps-ports", env = "ROLL_APPS_PORTS")]
    pub ports: Option<String>,
    /// Where those ports listen (default: the apps listener's address).
    #[arg(id = "apps_ports_host", long = "apps-ports-host", env = "ROLL_APPS_PORTS_HOST")]
    pub ports_host: Option<std::net::IpAddr>,
    /// DNS over HTTPS, for checking custom domains.
    #[arg(id = "apps_doh", long = "apps-doh", env = "ROLL_APPS_DOH", default_value = "https://cloudflare-dns.com/dns-query")]
    pub doh: String,
    /// Builds this node runs at once; more wait their turn (a build uses the CPUs and memory
    /// BuildKit asks for, which `docker build` can't limit per build).
    #[arg(id = "apps_builds", long = "apps-builds", env = "ROLL_APPS_BUILDS", default_value_t = 2)]
    pub builds: usize,
    /// Custom domains' certificates (docs/apps/README.md, "Networking").
    #[arg(id = "apps_tls", long = "apps-tls", env = "ROLL_APPS_TLS", value_enum, default_value_t = AppsTls::None)]
    pub tls: AppsTls,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum AppsTls {
    /// None: custom domains are served as the edge's certificates allow (or in the clear
    /// without the edge).
    #[default]
    None,
    /// The edge orders one per active custom domain over ACME HTTP-01: a node alone, with
    /// `ROLL_EDGE_HTTP` reachable on port 80 and `ROLL_ACME_DIRECTORY`.
    Acme,
    /// Cloudflare for SaaS custom hostnames on the site zone (`ROLL_CLOUDFLARE_API_TOKEN`), with
    /// `up.<site>` as the zone's fallback origin: the fleet.
    Cloudflare,
}

/// Cloudflare for SaaS as roll apps' certificate provider.
struct SaasTls(roll_edge::saas::CustomHostnames);

#[async_trait]
impl DomainTls for SaasTls {
    async fn ensure(&self, host: &str) -> anyhow::Result<Option<(TlsStatus, Option<String>)>> {
        use roll_edge::saas::HostnameCert;
        Ok(Some(match self.0.ensure(host).await? {
            HostnameCert::Pending(why) => (TlsStatus::Pending, why),
            HostnameCert::Issued => (TlsStatus::Issued, None),
            HostnameCert::Failed(why) => (TlsStatus::Failed, Some(why)),
        }))
    }

    async fn remove(&self, host: &str) -> anyhow::Result<()> {
        self.0.delete(host).await
    }
}

/// roll apps' active custom domains, for the edge's HTTP-01 certificates, and their state back.
struct AppsDomains(Arc<Apps>);

impl roll_edge::domains::CustomDomains for AppsDomains {
    fn active(&self) -> futures::future::BoxFuture<'_, anyhow::Result<Vec<String>>> {
        Box::pin(async move { self.0.active_custom_domains().await.map_err(|e| anyhow::anyhow!("{e}")) })
    }

    fn report<'a>(&'a self, host: &'a str, state: &'a roll_edge::domains::CertState) -> futures::future::BoxFuture<'a, ()> {
        use roll_edge::domains::CertState;
        let (tls, error) = match state {
            CertState::Issued { .. } => (TlsStatus::Issued, None),
            CertState::Failed(why) => (TlsStatus::Failed, Some(why.clone())),
        };
        Box::pin(async move {
            if let Err(e) = self.0.set_domain_tls(host, tls, error).await {
                warn!(host, error = %e, "apps: a custom domain's certificate state wasn't kept");
            }
        })
    }
}

/// The certificate provider `ROLL_APPS_TLS` names.
fn domain_tls(args: &crate::Args, single: bool) -> anyhow::Result<Arc<dyn DomainTls>> {
    let e = &args.edge;
    Ok(match args.apps.tls {
        AppsTls::None => Arc::new(NoTls),
        AppsTls::Acme => {
            anyhow::ensure!(e.enabled, "ROLL_APPS_TLS=acme needs the edge (ROLL_EDGE=1), which serves the certificates");
            anyhow::ensure!(single, "ROLL_APPS_TLS=acme is for a node alone (the node holding an HTTP-01 token must answer it); on a fleet use cloudflare");
            Arc::new(EdgeTls)
        }
        AppsTls::Cloudflare => {
            anyhow::ensure!(e.enabled, "ROLL_APPS_TLS=cloudflare needs the edge (ROLL_EDGE=1), Cloudflare's fallback origin");
            let token = e.cloudflare_api_token.as_deref().ok_or_else(|| anyhow::anyhow!("ROLL_APPS_TLS=cloudflare needs ROLL_CLOUDFLARE_API_TOKEN (SSL and Certificates edit on the site zone)"))?;
            let cf = Arc::new(roll_edge::acme::Cloudflare::new(token, &e.cloudflare_api)?);
            let hosts = e.hostnames();
            Arc::new(SaasTls(roll_edge::saas::CustomHostnames::new(cf, &hosts.site_domain, vec![hosts.domain.clone(), hosts.site_domain.clone()])))
        }
    })
}

fn parse_ports(s: &str) -> anyhow::Result<(u16, u16)> {
    let (a, b) = s.split_once('-').ok_or_else(|| anyhow::anyhow!("ROLL_APPS_PORTS is a range like 58100-58199"))?;
    let (a, b): (u16, u16) = (a.trim().parse()?, b.trim().parse()?);
    anyhow::ensure!(a > 0 && a <= b && b - a < 2000, "ROLL_APPS_PORTS is a range of at most 2000 ports");
    Ok((a, b))
}

/// roll code as roll apps' source of trees and landings.
#[derive(Clone)]
struct CodeSources(Arc<code_control::Code>);

impl CodeSources {
    fn repo(&self, full: &str) -> anyhow::Result<code_control::model::RepoRow> {
        let (o, n) = full.split_once('/').ok_or_else(|| anyhow::anyhow!("a repo is owner/name"))?;
        self.0.find_repo(o, n).map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn revision_of(&self, repo: &code_control::model::RepoRow, cs: &code_store::ChangesetId) -> anyhow::Result<Revision> {
        let (commit, _) = self.0.store.derive_git(&repo.id, cs)?;
        let message = self.0.store.changeset(&repo.id, cs)?.map(|c| c.summary()).unwrap_or_default();
        Ok(Revision { sha: commit.to_string(), message })
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(f).await?
}

#[async_trait]
impl Sources for CodeSources {
    async fn resolve(&self, repo: &str, branch: Option<&str>) -> anyhow::Result<Revision> {
        let (this, repo, branch) = (self.clone(), repo.to_string(), branch.map(str::to_string));
        blocking(move || {
            let r = this.repo(&repo)?;
            let b = branch.unwrap_or_else(|| r.default_branch.clone());
            let cs = this.0.store.get_ref(&r.id, &code_store::full_ref(&b))?.ok_or_else(|| anyhow::anyhow!("nothing has landed on {b} yet"))?;
            this.revision_of(&r, &cs)
        })
        .await
    }

    async fn default_branch(&self, repo: &str) -> anyhow::Result<String> {
        let (this, repo) = (self.clone(), repo.to_string());
        blocking(move || Ok(this.repo(&repo)?.default_branch)).await
    }

    async fn revision(&self, repo: &str, rev: &str) -> anyhow::Result<Revision> {
        let (this, repo, rev) = (self.clone(), repo.to_string(), rev.to_string());
        blocking(move || {
            let r = this.repo(&repo)?;
            let rev = if rev == "HEAD" { r.default_branch.clone() } else { rev };
            let cs = this.0.store.resolve(&r.id, &rev)?.ok_or_else(|| anyhow::anyhow!("no revision {rev}"))?;
            this.revision_of(&r, &cs)
        })
        .await
    }

    async fn materialize(&self, repo: &str, revision: &str, dir: &Path) -> anyhow::Result<()> {
        let (this, repo_s, rev) = (self.clone(), repo.to_string(), revision.to_string());
        let (r, cs) = blocking(move || {
            let r = this.repo(&repo_s)?;
            let cs = this.0.store.resolve(&r.id, &rev)?.ok_or_else(|| anyhow::anyhow!("no revision {rev}"))?;
            Ok((r, cs))
        })
        .await?;
        self.0.store.materialize(&r.id, &cs, dir).await?;
        Ok(())
    }

    async fn landings(&self, after: i64, limit: usize) -> anyhow::Result<Vec<Landing>> {
        let this = self.clone();
        blocking(move || {
            let mut names: HashMap<String, Option<code_control::model::RepoRow>> = HashMap::new();
            let mut out = Vec::new();
            for (seq, l) in this.0.store.land_log(after, limit)? {
                let Some(branch) = l.ref_name.strip_prefix("refs/heads/") else {
                    // Tags and roll's own refs (`refs/roll/…`) never deploy.
                    out.push(Landing { seq, repo: String::new(), branch: String::new(), revision: Revision { sha: String::new(), message: String::new() }, author: None });
                    continue;
                };
                let row = names.entry(l.repo.clone()).or_insert_with(|| this.0.repo_by_id(&l.repo).ok()).clone();
                let Some(r) = row else { continue };
                // Who the deployment it makes names: the landed commit's author.
                let author = this.0.store.changeset(&l.repo, &l.changeset).ok().flatten().map(|c| c.author.name()).filter(|n| !n.is_empty());
                out.push(Landing { seq, repo: r.full_name(), branch: branch.to_string(), revision: Revision { sha: l.git_commit.to_string(), message: l.summary.clone() }, author });
            }
            Ok(out)
        })
        .await
    }

    async fn head(&self) -> anyhow::Result<i64> {
        let this = self.clone();
        blocking(move || this.0.store.land_log_head()).await
    }

    async fn read_file(&self, repo: &str, revision: &str, path: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let (this, repo_s, rev) = (self.clone(), repo.to_string(), revision.to_string());
        let (r, cs) = blocking(move || {
            let r = this.repo(&repo_s)?;
            let cs = this.0.store.resolve(&r.id, &rev)?.ok_or_else(|| anyhow::anyhow!("no revision {rev}"))?;
            Ok((r, cs))
        })
        .await?;
        let f = self.0.store.read_file(&r.id, &cs, path).await?;
        // A symlink or a submodule isn't a file here, and a big one isn't config.
        Ok(f.filter(|f| f.mode.is_file() && f.data.len() <= 64 << 10).map(|f| f.data.to_vec()))
    }

    async fn touches(&self, repo: &str, from: &str, to: &str, patterns: &[String]) -> anyhow::Result<bool> {
        let (this, repo, from, to) = (self.clone(), repo.to_string(), from.to_string(), to.to_string());
        let patterns: Vec<code_control::review::pattern::Pattern> = patterns.iter().filter_map(|p| code_control::review::pattern::Pattern::new(p)).collect();
        blocking(move || {
            let r = this.repo(&repo)?;
            let at = |rev: &str| this.0.store.resolve(&r.id, rev)?.ok_or_else(|| anyhow::anyhow!("no revision {rev}"));
            let (a, b) = (at(&from)?, at(&to)?);
            Ok(this.0.store.compare(&r.id, &a, &b)?.iter().any(|d| code_control::review::pattern::any_match(&patterns, d.path.as_str())))
        })
        .await
    }

    async fn set_status(&self, repo: &str, sha: &str, st: &apps_control::integrations::CommitStatus) -> anyhow::Result<()> {
        let (this, repo, sha) = (self.clone(), repo.to_string(), sha.to_string());
        let body = serde_json::json!({ "state": st.state, "context": st.context, "description": st.description, "target_url": st.target_url });
        blocking(move || {
            let r = this.repo(&repo)?;
            this.0.post_status(&code_control::Caller::Admin, &r, &sha, &body).map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(())
        })
        .await
    }

    async fn ci(&self, repo: &str, sha: &str) -> anyhow::Result<apps_control::integrations::Ci> {
        use apps_control::integrations::Ci;
        let (this, repo, sha) = (self.clone(), repo.to_string(), sha.to_string());
        blocking(move || {
            let r = this.repo(&repo)?;
            let v = this.0.get_statuses(&r, &sha).map_err(|e| anyhow::anyhow!("{e}"))?;
            // Deploy statuses aren't checks of the code: roll apps' own, and roll previews'
            // (`roll/preview`, about a preview environment: pending while it waits for a slot,
            // failed for roll's own reasons), which would otherwise hold production back.
            let check = |ctx: &str| !ctx.starts_with("roll/apps") && !ctx.starts_with("roll/preview");
            let theirs: Vec<&serde_json::Value> = v["statuses"].as_array().into_iter().flatten().filter(|s| check(s["context"].as_str().unwrap_or(""))).collect();
            Ok(if theirs.is_empty() {
                Ci::None
            } else if let Some(f) = theirs.iter().find(|s| matches!(s["state"].as_str(), Some("failure" | "error"))) {
                Ci::Failed(f["context"].as_str().unwrap_or("a check").to_string())
            } else if theirs.iter().any(|s| s["state"] == "pending") {
                Ci::Pending
            } else {
                Ci::Passed
            })
        })
        .await
    }
}

/// The data products' connection variables for links.
struct RollResources {
    postgres: Option<Arc<pg_control::Control>>,
    redis: Option<Arc<redis_control::Control>>,
}

#[async_trait]
impl Resources for RollResources {
    async fn link_vars(&self, kind: LinkKind, r#ref: &str) -> anyhow::Result<Vec<(String, String)>> {
        match kind {
            LinkKind::Postgres => {
                let c = self.postgres.clone().ok_or_else(|| anyhow::anyhow!("roll postgres isn't on this node"))?;
                let r = r#ref.to_string();
                blocking(move || {
                    let project = c.store.project(&r)?.ok_or_else(|| anyhow::anyhow!("no Postgres project {r}"))?;
                    let main = c.store.branches(&project.id)?.into_iter().find(|b| b.parent_id.is_none()).ok_or_else(|| anyhow::anyhow!("{r} has no main branch"))?;
                    let ep = c.store.endpoints(&project.id)?.into_iter().find(|e| e.branch_id == main.id && e.is_primary()).ok_or_else(|| anyhow::anyhow!("{r} has no endpoint"))?;
                    let uri = c.connection_uri(&project, &ep.id);
                    Ok(vec![("DATABASE_URL".into(), uri)])
                })
                .await
            }
            LinkKind::Redis => {
                let c = self.redis.clone().ok_or_else(|| anyhow::anyhow!("roll redis isn't on this node"))?;
                let r = r#ref.to_string();
                blocking(move || {
                    let d = c.get(&r).map_err(|e| anyhow::anyhow!("{e}"))?;
                    let conn = c.connection(&d.id, &d.password);
                    let mut v = vec![("REDIS_URL".to_string(), conn["redis_url"].as_str().unwrap_or("").to_string())];
                    if let Some(u) = conn["rest_url"].as_str() {
                        v.push(("REDIS_REST_URL".into(), u.to_string()));
                        v.push(("REDIS_REST_TOKEN".into(), conn["rest_token"].as_str().unwrap_or("").to_string()));
                    }
                    Ok(v)
                })
                .await
            }
            other => anyhow::bail!("links to {} aren't wired on this node yet", other.as_str()),
        }
    }
}

pub struct AppsNode {
    pub apps: Arc<Apps>,
    listen: SocketAddr,
    ports_host: std::net::IpAddr,
}

/// The variable-sealing key: `ROLL_SECRET_KEY`, else a key of this node's own made once.
fn sealer(home: &Path) -> anyhow::Result<Sealer> {
    if let Some(k) = std::env::var("ROLL_SECRET_KEY").ok().filter(|k| k.trim().len() >= 32) {
        return Sealer::new(&k);
    }
    let path = home.join("apps").join("key");
    if let Ok(k) = std::fs::read_to_string(&path) {
        return Sealer::new(&k);
    }
    std::fs::create_dir_all(path.parent().expect("a parent"))?;
    let key = apps_control::model::random(48);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    std::io::Write::write_all(&mut o.open(&path)?, key.as_bytes())?;
    Sealer::new(&key)
}

#[allow(clippy::too_many_arguments)]
pub async fn start(
    args: &crate::Args,
    home: &Path,
    storage: &pg_storage::StorageConfig,
    cluster: Arc<roll_cluster::Cluster>,
    workspace: Arc<roll_workspace::Workspace>,
    code: Option<Arc<code_control::Code>>,
    postgres: Option<Arc<pg_control::Control>>,
    redis: Option<Arc<redis_control::Control>>,
) -> anyhow::Result<AppsNode> {
    let a = &args.apps;
    let single = cluster.backend_name() == "memory";
    let target = match args.code.cluster.system_database.as_deref().filter(|u| !u.is_empty()) {
        Some(url) => code_sql::Target::Postgres { url: url.to_string(), schema: "apps".into() },
        None => code_sql::Target::Sqlite(home.join("apps.db")),
    };
    let db = match &target {
        code_sql::Target::Sqlite(p) => code_sql::Db::open_sqlite(p, code_sql::SqliteOptions { readers: 4, cache_kib: 4 << 10 })?,
        code_sql::Target::Postgres { url, schema } => {
            let pg = code_sql::PgOptions { max_connections: args.db_budget()?.apps, ca: args.code.code_db_ca.clone(), ..Default::default() };
            tokio::task::block_in_place(|| code_sql::Db::connect(url, schema, pg))?
        }
    };
    let store = tokio::task::block_in_place(|| Store::new(db, sealer(home)?).map_err(|e| anyhow::anyhow!("{e}")))?;
    let mut cfg = AppsConfig::new(home.to_path_buf());
    cfg.web_url = Some(args.web_url.trim_end_matches('/').to_string()).filter(|u| !u.is_empty());
    // Local development only: notification targets on this machine or a private network.
    cfg.allow_private = std::env::var("ROLL_APPS_ALLOW_PRIVATE").is_ok_and(|v| v == "1" || v == "true");
    let edge = crate::edge::Public::of(args);
    cfg.domain = a.domain.clone().unwrap_or_else(|| if edge.is_some() { format!("up.{}", args.edge.site_domain) } else { "apps.localhost".into() });
    if edge.is_some() {
        cfg.scheme = "https".into();
        cfg.url_port = None;
        cfg.reserved = vec![args.edge.domain.clone(), args.edge.site_domain.clone()];
        // Cloudflare for SaaS: customers point at the zone's fallback origin.
        if a.tls == AppsTls::Cloudflare {
            cfg.cname_target = Some(cfg.domain.clone());
        }
    } else {
        cfg.url_port = Some(a.listen.port());
    }
    if let Some(p) = &a.ports {
        anyhow::ensure!(single, "ROLL_APPS_PORTS is for a node alone: on a fleet services answer on their hostnames");
        cfg.ports = Some(parse_ports(p)?);
    }
    let objects = match storage {
        // A local directory is this node's alone: logs stay with it.
        pg_storage::StorageConfig::Local(_) if !single => None,
        s => Some(pg_storage::Objects::new(s)?),
    };
    // On the fleet (`ROLL_APPS_RUNTIME=fargate`): instances as Fargate tasks and builds in
    // CodeBuild, so no customer code runs on this node. Otherwise Docker on this host.
    let fleet = apps_control::runtime::aws::FleetConfig::from_lookup(|k| std::env::var(k).ok(), home.join("apps").join("builds"))?;
    let (runtime, builder): (Arc<dyn apps_control::runtime::Runtime>, Arc<dyn apps_control::runtime::Builder>) = match fleet {
        Some(f) => {
            info!(cluster = %f.cluster, project = %f.codebuild_project, "apps: instances run as Fargate tasks, builds in CodeBuild");
            (apps_control::runtime::fargate::Fargate::new(f.clone()).await?, apps_control::runtime::codebuild::CodeBuild::new(f).await?)
        }
        None => (
            Arc::new(apps_control::runtime::docker::Docker::new(home.join("apps").join("scratch"))),
            Arc::new(apps_control::runtime::docker::DockerBuilder::new(a.builds.max(1))),
        ),
    };
    let on_docker = runtime.name() == "docker";
    let deps = Deps {
        runtime,
        builder,
        sources: code.map(|c| Arc::new(CodeSources(c)) as Arc<dyn Sources>),
        secrets: Arc::new(NoSecrets),
        resources: Arc::new(RollResources { postgres, redis }),
        resolver: Arc::new(Doh::new(&a.doh)),
        tls: domain_tls(args, single)?,
        objects,
        cluster,
        workspace: Some(workspace),
    };
    let apps = Apps::new(cfg, store, deps);
    apps.start();
    if on_docker && !apps_control::runtime::docker::available().await {
        warn!("apps: docker isn't reachable, so services can't build or run on this node");
    }
    info!(domain = %apps.cfg.domain, listen = %a.listen, "apps: services answer on <slug>.{}", apps.cfg.domain);
    Ok(AppsNode { apps, listen: a.listen, ports_host: a.ports_host.unwrap_or(a.listen.ip()) })
}

impl AppsNode {
    pub fn router(&self) -> axum::Router {
        apps_control::api::router(self.apps.clone())
    }

    /// Services' hostnames and custom domains, for the edge's HTTPS listener.
    /// Deploy hooks: no API token (the URL is the secret), so outside the gate.
    pub fn hooks_router(&self) -> axum::Router {
        apps_control::api::hooks_router(self.apps.clone())
    }

    pub fn public_router(&self) -> axum::Router {
        apps_control::public::router(self.apps.clone())
    }

    /// The active custom domains, for the edge's HTTP-01 certificates (`ROLL_APPS_TLS=acme`).
    pub fn custom_domains(&self) -> Arc<dyn roll_edge::domains::CustomDomains> {
        Arc::new(AppsDomains(self.apps.clone()))
    }

    /// The apps listener, and each environment's own port as environments appear.
    pub async fn serve(&self) -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind(self.listen).await?;
        let public = apps_control::public::router(self.apps.clone());
        let main = axum::serve(listener, public.into_make_service_with_connect_info::<SocketAddr>()).into_future();
        let ports = self.serve_ports();
        tokio::select! {
            r = main => Ok(r?),
            r = ports => r,
        }
    }

    async fn serve_ports(&self) -> anyhow::Result<()> {
        if self.apps.cfg.ports.is_none() {
            return std::future::pending().await;
        }
        let mut bound: HashMap<u16, tokio::task::JoinHandle<()>> = HashMap::new();
        loop {
            let envs = self.apps.db(|s| s.all_environments()).await.unwrap_or_default();
            let want: HashMap<u16, (String, String)> = envs.into_iter().filter_map(|e| e.port.map(|p| (p, (e.service, e.name)))).collect();
            bound.retain(|p, h| {
                let keep = want.contains_key(p);
                if !keep {
                    h.abort();
                }
                keep
            });
            for (port, (svc, env)) in want {
                if bound.contains_key(&port) {
                    continue;
                }
                match tokio::net::TcpListener::bind((self.ports_host, port)).await {
                    Ok(l) => {
                        let r = apps_control::public::port_router(self.apps.clone(), svc, env);
                        bound.insert(port, tokio::spawn(async move {
                            let _ = axum::serve(l, r.into_make_service_with_connect_info::<SocketAddr>()).await;
                        }));
                    }
                    Err(e) => warn!(port, error = %e, "apps: an environment's port couldn't be bound"),
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    pub async fn shutdown(&self) {
        self.apps.shutdown().await;
        self.apps.flush_meters(true).await;
    }
}
