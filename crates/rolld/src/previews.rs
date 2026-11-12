//! roll previews in rolld (docs/previews.md): the store, the engine, `/v1/previews`, the
//! routing of preview hosts, and one branch adapter per data product.
//!
//! The adapters talk to this node's own API over loopback with the operator token, exactly as
//! `roll` and the dashboard do. That keeps one path for every product, and it's right in a
//! cluster: each route already runs on the node that owns the resource (a Redis database's or
//! a stream's lease, storage's metadata owner), whichever node the engine runs on.

use async_trait::async_trait;
use reqwest::{Method, StatusCode};
use roll_previews::Kind;
use roll_previews::ports::{Branched, Brancher, Usage};
use roll_previews::{Mode, Store};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

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
        roll_previews::runtime::RuntimeConfig { domain: domain.clone(), ..Default::default() },
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
        roll_previews::engine::Config { domain, port: Some(args.api.port()), ..Default::default() },
        store,
        workspace.clone(),
        source.clone(),
        branchers,
        Arc::new(Services { apps: RollApps::new(api.clone()), runtime: runtime.clone() }),
        Arc::new(Secrets(api.clone())),
        leader,
    );
    runtime.attach(&engine);
    let backend = roll_previews::code::Backend::new(code.previews.clone(), workspace, source);
    backend.attach(&engine);
    code.code.set_preview_backend(backend);
    drop(engine.start());
    tracing::info!("roll previews: on");
    Ok(Running { engine, runtime })
}

pub fn open_store(args: &crate::Args, home: &Path) -> anyhow::Result<Arc<Store>> {
    let target = match args.code.cluster.system_database.as_deref().filter(|u| !u.is_empty()) {
        Some(url) => code_sql::Target::Postgres { url: url.to_string(), schema: "previews".into() },
        None => code_sql::Target::Sqlite(home.join("previews.db")),
    };
    let pg = code_sql::PgOptions { max_connections: args.db_budget()?.workspace, ca: args.code.code_db_ca.clone(), ..Default::default() };
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
        let mut req = self.http.request(method.clone(), format!("{}{path}", self.base));
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
            return Err(Refused { status, error }.into());
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

// ── Postgres: a branch of the project's main ──

pub struct Postgres(pub Api);

impl Postgres {
    async fn find(&self, project: &str, name: &str) -> anyhow::Result<Option<Value>> {
        let p = self.0.get(&format!("/v1/projects/{}", seg(project))).await?;
        Ok(p["branches"].as_array().and_then(|b| b.iter().find(|b| b["name"] == name).cloned()))
    }
}

#[async_trait]
impl Brancher for Postgres {
    fn kind(&self) -> Kind {
        Kind::Postgres
    }

    async fn branch(&self, parent: &str, name: &str, _mode: Mode) -> anyhow::Result<Branched> {
        // Structure-only Postgres branches are in docs/previews.md's `## Next`: `schema` branches
        // everything for now.
        if let Some(b) = self.find(parent, name).await? {
            return Ok(Branched::plain(s(&b["id"]).unwrap_or_default()));
        }
        match self.0.post(&format!("/v1/projects/{}/branches", seg(parent)), json!({ "name": name })).await {
            Ok(v) => Ok(Branched::plain(s(&v["branch"]["id"]).ok_or_else(|| anyhow::anyhow!("a branch without an id"))?)),
            Err(e) if conflict_is_ours(&e) => {
                let b = self.find(parent, name).await?.ok_or(e)?;
                Ok(Branched::plain(s(&b["id"]).unwrap_or_default()))
            }
            Err(e) => Err(e),
        }
    }

    /// In place: same branch, endpoint and `DATABASE_URL`.
    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        match self.0.post(&format!("/v1/projects/{}/branches/{}/reset", seg(parent), seg(branch)), json!({})).await {
            Ok(_) => Ok(Branched::plain(branch)),
            // Gone (or an older node without the route): branch again.
            Err(e) if is_404(&e) => {
                self.delete(parent, branch, name).await?;
                self.branch(parent, name, mode).await
            }
            Err(e) => Err(e),
        }
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        let p = match self.0.get(&format!("/v1/projects/{}", seg(parent))).await {
            Ok(p) => p,
            Err(e) if is_404(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        let Some(b) = p["branches"].as_array().and_then(|bs| bs.iter().find(|b| b["id"] == branch).cloned()) else { return Ok(()) };
        anyhow::ensure!(b["name"] == name && !b["parent_id"].is_null(), "refusing to delete Postgres branch {branch}: it isn't {parent}'s branch {name}");
        self.0.delete(&format!("/v1/projects/{}/branches/{}", seg(parent), seg(branch))).await
    }

    async fn connect(&self, parent: &str, branch: &str, _kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let p = self.0.get(&format!("/v1/projects/{}", seg(parent))).await?;
        let b = p["branches"].as_array().and_then(|bs| bs.iter().find(|b| b["id"] == branch).cloned()).ok_or_else(|| anyhow::anyhow!("no Postgres branch {branch}"))?;
        let uri = s(&b["connection_uri"]).ok_or_else(|| anyhow::anyhow!("Postgres branch {branch} has no endpoint"))?;
        Ok(vec![("DATABASE_URL".into(), uri)])
    }

    /// A branch's own bytes (its local data directory, else its objects), at Postgres's rates.
    async fn usage(&self, parent: &str, branch: &str) -> anyhow::Result<Usage> {
        let p = self.0.get(&format!("/v1/projects/{}", seg(parent))).await?;
        let u = self.0.get(&format!("/v1/projects/{}/usage?days=1", seg(parent))).await?;
        let price = |k: &str, d: f64| u["pricing"][k].as_f64().unwrap_or(d);
        let b = p["branches"].as_array().and_then(|bs| bs.iter().find(|b| b["id"] == branch).cloned()).unwrap_or(Value::Null);
        let local = b["storage_bytes"].as_i64().unwrap_or(0);
        let objects = b["object_bytes"].as_i64().unwrap_or(0);
        let storage = gb(local) * price("usd_per_gb_month", 0.08) + gb(objects) * price("usd_per_gb_month_s3", 0.023);
        let cu = b["endpoint"]["min_cu"].as_f64().unwrap_or(0.25).max(0.25);
        Ok(Usage { bytes: local.max(objects), storage_usd_month: storage, active_usd_hour: cu * price("usd_per_cu_hour", 0.0408), month_to_date_usd: 0.0 })
    }
}

// ── Redis ──

pub struct Redis(pub Api);

#[async_trait]
impl Brancher for Redis {
    fn kind(&self) -> Kind {
        Kind::Redis
    }

    async fn branch(&self, parent: &str, name: &str, _mode: Mode) -> anyhow::Result<Branched> {
        match self.0.post(&format!("/v1/redis/databases/{}/branches", seg(parent)), json!({ "name": name })).await {
            Ok(v) => Ok(Branched::plain(s(&v["id"]).ok_or_else(|| anyhow::anyhow!("a database without an id"))?)),
            Err(e) if conflict_is_ours(&e) => {
                // Names are global: the database named `name` is ours only if it's the parent's.
                let d = self.0.get(&format!("/v1/redis/databases/{}", seg(name))).await?;
                let parent_id = self.0.get(&format!("/v1/redis/databases/{}", seg(parent))).await?["id"].clone();
                anyhow::ensure!(d["parent_id"] == parent_id, "a Redis database named {name} exists and isn't a branch of {parent}");
                Ok(Branched::plain(s(&d["id"]).unwrap_or_default()))
            }
            Err(e) => Err(e),
        }
    }

    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        self.delete(parent, branch, name).await?;
        self.branch(parent, name, mode).await
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        let d = match self.0.get(&format!("/v1/redis/databases/{}", seg(branch))).await {
            Ok(d) => d,
            Err(e) if is_404(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        let parent_id = match self.0.get(&format!("/v1/redis/databases/{}", seg(parent))).await {
            Ok(p) => p["id"].clone(),
            // The parent is gone: the branch still must carry our name.
            Err(e) if is_404(&e) => d["parent_id"].clone(),
            Err(e) => return Err(e),
        };
        anyhow::ensure!(d["name"] == name && d["parent_id"] == parent_id && !parent_id.is_null(), "refusing to delete Redis database {branch}: it isn't {parent}'s branch {name}");
        self.0.delete(&format!("/v1/redis/databases/{}", seg(branch))).await
    }

    async fn connect(&self, _parent: &str, branch: &str, _kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let d = self.0.get(&format!("/v1/redis/databases/{}", seg(branch))).await?;
        Ok(vec![("REDIS_URL".into(), s(&d["redis_url"]).ok_or_else(|| anyhow::anyhow!("no redis_url"))?)])
    }

    async fn usage(&self, _parent: &str, branch: &str) -> anyhow::Result<Usage> {
        let u = self.0.get(&format!("/v1/redis/databases/{}/usage?days=30", seg(branch))).await?;
        let d = self.0.get(&format!("/v1/redis/databases/{}", seg(branch))).await?;
        let bytes = u["storage_bytes"].as_i64().unwrap_or(0);
        let mb = d["max_memory_mb"].as_f64().unwrap_or(256.0);
        Ok(Usage {
            bytes,
            storage_usd_month: gb(bytes) * 0.023,
            // Memory is billed while loaded: a busy preview fills it up to its peak.
            active_usd_hour: (bytes as f64 / (1u64 << 30) as f64).max(mb.min(64.0) / 1024.0) * 0.0067,
            month_to_date_usd: u["cost"]["total_usd"].as_f64().unwrap_or(0.0),
        })
    }
}

// ── NoSQL ──

pub struct Nosql(pub Api);

#[async_trait]
impl Brancher for Nosql {
    fn kind(&self) -> Kind {
        Kind::Nosql
    }

    async fn branch(&self, parent: &str, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        let body = json!({ "name": name, "schema_only": mode == Mode::Schema });
        match self.0.post(&format!("/v1/nosql/databases/{}/branches", seg(parent)), body).await {
            Ok(v) => Ok(Branched::plain(s(&v["id"]).ok_or_else(|| anyhow::anyhow!("a database without an id"))?)),
            Err(e) if conflict_is_ours(&e) => {
                let d = self.0.get(&format!("/v1/nosql/databases/{}", seg(name))).await?;
                let parent_id = self.0.get(&format!("/v1/nosql/databases/{}", seg(parent))).await?["id"].clone();
                anyhow::ensure!(d["parent_id"] == parent_id, "a NoSQL database named {name} exists and isn't a branch of {parent}");
                Ok(Branched::plain(s(&d["id"]).unwrap_or_default()))
            }
            Err(e) => Err(e),
        }
    }

    /// NoSQL resets in place: same id, endpoint and credentials.
    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        match self.0.post(&format!("/v1/nosql/databases/{}/reset", seg(branch)), json!({})).await {
            Ok(_) => Ok(Branched::plain(branch)),
            Err(e) if is_404(&e) => self.branch(parent, name, mode).await,
            Err(e) => Err(e),
        }
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        let d = match self.0.get(&format!("/v1/nosql/databases/{}", seg(branch))).await {
            Ok(d) => d,
            Err(e) if is_404(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        let parent_id = match self.0.get(&format!("/v1/nosql/databases/{}", seg(parent))).await {
            Ok(p) => p["id"].clone(),
            Err(e) if is_404(&e) => d["parent_id"].clone(),
            Err(e) => return Err(e),
        };
        anyhow::ensure!(d["name"] == name && d["parent_id"] == parent_id && !parent_id.is_null(), "refusing to delete NoSQL database {branch}: it isn't {parent}'s branch {name}");
        self.0.delete(&format!("/v1/nosql/databases/{}", seg(branch))).await
    }

    async fn connect(&self, _parent: &str, branch: &str, _kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let d = self.0.get(&format!("/v1/nosql/databases/{}", seg(branch))).await?;
        let mut v = vec![
            ("CASSANDRA_CONTACT_POINTS".into(), s(&d["endpoints"]["cql"]).unwrap_or_default()),
            ("CASSANDRA_USERNAME".into(), s(&d["credentials"]["username"]).unwrap_or_default()),
            ("CASSANDRA_PASSWORD".into(), s(&d["credentials"]["password"]).unwrap_or_default()),
        ];
        if let Some(dc) = s(&d["datacenter"]) {
            v.push(("CASSANDRA_LOCAL_DATACENTER".into(), dc));
        }
        Ok(v)
    }

    async fn usage(&self, _parent: &str, branch: &str) -> anyhow::Result<Usage> {
        let u = self.0.get(&format!("/v1/nosql/databases/{}/usage?days=30", seg(branch))).await?;
        let bytes = u["storage_bytes"].as_i64().unwrap_or(0);
        Ok(Usage { bytes, storage_usd_month: gb(bytes) * 0.023, active_usd_hour: 0.0, month_to_date_usd: u["cost"]["total_usd"].as_f64().unwrap_or(0.0) })
    }
}

// ── Storage buckets: a copy-on-write branch plus a write key scoped to it ──

pub struct Bucket(pub Api);

impl Bucket {
    /// A fresh write key for the branch only (the secret is shown once, so it's kept).
    async fn key(&self, bucket: &str) -> anyhow::Result<String> {
        let k = self.0.post("/v1/storage/keys", json!({ "name": bucket, "permission": "write", "buckets": [bucket] })).await?;
        let info = self.0.get("/v1/storage").await.unwrap_or(Value::Null);
        Ok(json!({
            "S3_BUCKET": bucket,
            "AWS_ENDPOINT_URL_S3": info["endpoint"],
            "AWS_REGION": info["region"],
            "AWS_ACCESS_KEY_ID": k["id"],
            "AWS_SECRET_ACCESS_KEY": k["secret_access_key"],
        })
        .to_string())
    }

    /// Every key minted for the branch.
    async fn drop_keys(&self, bucket: &str) -> anyhow::Result<()> {
        let keys = self.0.get("/v1/storage/keys").await?;
        for k in keys.as_array().into_iter().flatten() {
            if k["name"] == bucket && k["buckets"].as_array().is_some_and(|b| b.len() == 1 && b[0] == bucket) {
                self.0.delete(&format!("/v1/storage/keys/{}", seg(k["id"].as_str().unwrap_or_default()))).await?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Brancher for Bucket {
    fn kind(&self) -> Kind {
        Kind::Bucket
    }

    async fn branch(&self, parent: &str, name: &str, _mode: Mode) -> anyhow::Result<Branched> {
        // Bucket names are global: the parent's name keeps it readable, the preview's makes it
        // unique (63 characters at most).
        let child = bucket_name(parent, name);
        match self.0.post(&format!("/v1/storage/buckets/{}/branches", seg(parent)), json!({ "name": child })).await {
            Ok(_) => {}
            Err(e) if conflict_is_ours(&e) => {
                let b = self.0.get(&format!("/v1/storage/buckets/{}", seg(&child))).await?;
                anyhow::ensure!(b["parent"]["name"] == parent, "a bucket named {child} exists and isn't a branch of {parent}");
            }
            Err(e) => return Err(e),
        }
        // A key from before a crash can't be read back: replace it.
        self.drop_keys(&child).await?;
        Ok(Branched { r#ref: child.clone(), kept: Some(self.key(&child).await?) })
    }

    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        self.delete(parent, branch, name).await?;
        self.branch(parent, name, mode).await
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        anyhow::ensure!(branch == bucket_name(parent, name) && branch != parent, "refusing to delete bucket {branch}: it isn't {parent}'s branch {name}");
        let b = match self.0.get(&format!("/v1/storage/buckets/{}", seg(branch))).await {
            Ok(b) => b,
            Err(e) if is_404(&e) => return self.drop_keys(branch).await,
            Err(e) => return Err(e),
        };
        anyhow::ensure!(b["parent"]["name"] == parent, "refusing to delete bucket {branch}: it isn't a branch of {parent}");
        // `force` empties the branch: only its own rows (the parent's objects are the parent's).
        self.0.delete(&format!("/v1/storage/buckets/{}?force=true", seg(branch))).await?;
        self.drop_keys(branch).await
    }

    async fn connect(&self, _parent: &str, branch: &str, kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let kept: Value = kept.and_then(|k| serde_json::from_str(k).ok()).ok_or_else(|| anyhow::anyhow!("the bucket branch {branch} has no key: reset it"))?;
        Ok(kept.as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
    }

    async fn usage(&self, _parent: &str, branch: &str) -> anyhow::Result<Usage> {
        let b = self.0.get(&format!("/v1/storage/buckets/{}", seg(branch))).await?;
        let bytes = b["bytes"].as_i64().unwrap_or(0) + b["trash_bytes"].as_i64().unwrap_or(0) + b["version_bytes"].as_i64().unwrap_or(0);
        Ok(Usage { bytes, storage_usd_month: gb(bytes) * 0.023, active_usd_hour: 0.0, month_to_date_usd: 0.0 })
    }
}

/// `<parent>-<preview branch name>`, cut to S3's 63 characters (the preview part kept whole).
pub fn bucket_name(parent: &str, name: &str) -> String {
    let room = 63usize.saturating_sub(name.len() + 1);
    let head: String = parent.chars().take(room).collect();
    format!("{}-{name}", head.trim_end_matches(['-', '.']))
}

// ── Streams: a stream with the parent's topics (no records yet; docs/previews.md `## Next`) ──

pub struct Stream(pub Api);

#[async_trait]
impl Brancher for Stream {
    fn kind(&self) -> Kind {
        Kind::Stream
    }

    async fn branch(&self, parent: &str, name: &str, _mode: Mode) -> anyhow::Result<Branched> {
        let p = self.0.get(&format!("/v1/streams/{}", seg(parent))).await?;
        let made = match self.0.post("/v1/streams", json!({ "name": name, "settings": p["settings"] })).await {
            Ok(v) => v,
            Err(e) if conflict_is_ours(&e) => {
                // From before a crash: a fresh key, since the first secret is gone.
                let v = self.0.get(&format!("/v1/streams/{}", seg(name))).await?;
                let key = self.0.post(&format!("/v1/streams/{}/keys", seg(name)), json!({ "label": "roll preview" })).await?;
                json!({ "id": v["id"], "bootstrap": v["bootstrap"], "connect": { "bootstrap": v["bootstrap"], "username": key["username"], "secret": key["secret"] } })
            }
            Err(e) => return Err(e),
        };
        let id = s(&made["id"]).ok_or_else(|| anyhow::anyhow!("a stream without an id"))?;
        for t in self.0.get(&format!("/v1/streams/{}/topics", seg(parent))).await?.as_array().into_iter().flatten() {
            let body = json!({ "name": t["name"], "partitions": t["partitions"], "configs": t["configs"] });
            match self.0.post(&format!("/v1/streams/{}/topics", seg(&id)), body).await {
                Ok(_) => {}
                Err(e) if conflict_is_ours(&e) => {}
                Err(e) => return Err(e),
            }
        }
        let c = &made["connect"];
        let kept = json!({
            "KAFKA_BOOTSTRAP_SERVERS": c["bootstrap"],
            "KAFKA_SASL_MECHANISM": "SCRAM-SHA-256",
            "KAFKA_SASL_USERNAME": c["username"],
            "KAFKA_SASL_PASSWORD": c["secret"],
        });
        Ok(Branched { r#ref: id, kept: Some(kept.to_string()) })
    }

    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        self.delete(parent, branch, name).await?;
        self.branch(parent, name, mode).await
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        let v = match self.0.get(&format!("/v1/streams/{}", seg(branch))).await {
            Ok(v) => v,
            Err(e) if is_404(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        anyhow::ensure!(v["name"] == name && branch != parent, "refusing to delete stream {branch}: it isn't the preview's stream {name}");
        self.0.delete(&format!("/v1/streams/{}", seg(branch))).await
    }

    async fn connect(&self, _parent: &str, branch: &str, kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let kept: Value = kept.and_then(|k| serde_json::from_str(k).ok()).ok_or_else(|| anyhow::anyhow!("the stream {branch} has no key: reset it"))?;
        Ok(kept.as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
    }

    async fn usage(&self, _parent: &str, branch: &str) -> anyhow::Result<Usage> {
        let u = self.0.get(&format!("/v1/streams/{}/usage?hours=24", seg(branch))).await?;
        let last = u["storage"].as_array().and_then(|a| a.last()).cloned().unwrap_or(Value::Null);
        let bytes = last["local_bytes"].as_i64().unwrap_or(0) + last["remote_bytes"].as_i64().unwrap_or(0);
        Ok(Usage { bytes, storage_usd_month: gb(bytes) * 0.023, active_usd_hour: 0.0, month_to_date_usd: 0.0 })
    }
}

// ── Queues: a vhost with the parent's definitions, no messages ──

pub struct Queue(pub Api);

#[async_trait]
impl Brancher for Queue {
    fn kind(&self) -> Kind {
        Kind::Queue
    }

    async fn branch(&self, parent: &str, name: &str, _mode: Mode) -> anyhow::Result<Branched> {
        let vhosts = self.0.get("/v1/queues/vhosts").await?;
        let parent_v = vhosts.as_array().and_then(|a| a.iter().find(|v| v["name"] == parent).cloned()).ok_or_else(|| anyhow::anyhow!("no vhost {parent}"))?;
        let exists = vhosts.as_array().is_some_and(|a| a.iter().any(|v| v["name"] == name));
        if !exists {
            let body = json!({ "name": name, "description": format!("roll preview of {parent}"), "default_queue_type": parent_v["default_queue_type"] });
            match self.0.post("/v1/queues/vhosts", body).await {
                Ok(_) => {}
                Err(e) if conflict_is_ours(&e) => {}
                Err(e) => return Err(e),
            }
        }
        // The parent's exchanges, queues, bindings, policies and parameters, moved to the branch.
        let defs = self.0.get("/v1/queues/definitions").await?;
        let mut copy = json!({});
        for key in ["exchanges", "queues", "bindings", "policies", "parameters"] {
            let items: Vec<Value> = defs[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|i| i["vhost"] == parent)
                .map(|i| {
                    let mut i = i.clone();
                    i["vhost"] = json!(name);
                    i
                })
                .collect();
            copy[key] = json!(items);
        }
        self.0.post("/v1/queues/definitions", copy).await?;
        let c = self.0.post(&format!("/v1/queues/vhosts/{}/credentials", seg(name)), json!({})).await?;
        Ok(Branched { r#ref: name.to_string(), kept: Some(json!({ "AMQP_URL": c["urls"]["amqp"] }).to_string()) })
    }

    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        self.delete(parent, branch, name).await?;
        self.branch(parent, name, mode).await
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        anyhow::ensure!(branch == name && branch != parent, "refusing to delete vhost {branch}: it isn't the preview's vhost {name}");
        self.0.delete(&format!("/v1/queues/vhosts/{}", seg(branch))).await
    }

    async fn connect(&self, _parent: &str, branch: &str, kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let kept: Value = kept.and_then(|k| serde_json::from_str(k).ok()).ok_or_else(|| anyhow::anyhow!("the vhost {branch} has no credentials: reset it"))?;
        Ok(kept.as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
    }

    async fn usage(&self, _parent: &str, branch: &str) -> anyhow::Result<Usage> {
        let vhosts = self.0.get("/v1/queues/vhosts").await?;
        let bytes = vhosts.as_array().and_then(|a| a.iter().find(|v| v["name"] == branch)).and_then(|v| v["stored_bytes"].as_i64()).unwrap_or(0);
        Ok(Usage { bytes, storage_usd_month: gb(bytes) * 0.023, active_usd_hour: 0.0, month_to_date_usd: 0.0 })
    }
}

// ── Tasks: an empty tenant (copied schedules would call production) ──

pub struct Tasks {
    pub api: Api,
    /// Where services reach the tasks API.
    pub public: String,
}

#[async_trait]
impl Brancher for Tasks {
    fn kind(&self) -> Kind {
        Kind::Tasks
    }

    async fn branch(&self, _parent: &str, name: &str, _mode: Mode) -> anyhow::Result<Branched> {
        let token = match self.api.post("/_roll/tenants", json!({ "id": name })).await {
            Ok(v) => v["token"].clone(),
            Err(e) if conflict_is_ours(&e) => self.api.post(&format!("/_roll/tenants/{}/keys", seg(name)), json!({ "name": "roll preview" })).await?["token"].clone(),
            Err(e) => return Err(e),
        };
        let kept = json!({ "ROLL_TASKS_URL": format!("{}/t/{name}", self.public.trim_end_matches('/')), "ROLL_TASKS_TOKEN": token });
        Ok(Branched { r#ref: name.to_string(), kept: Some(kept.to_string()) })
    }

    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        self.delete(parent, branch, name).await?;
        self.branch(parent, name, mode).await
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        anyhow::ensure!(branch == name && branch != parent && branch != "default", "refusing to delete tasks tenant {branch}: it isn't the preview's tenant {name}");
        self.api.delete(&format!("/_roll/tenants/{}", seg(branch))).await
    }

    async fn connect(&self, _parent: &str, branch: &str, kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let kept: Value = kept.and_then(|k| serde_json::from_str(k).ok()).ok_or_else(|| anyhow::anyhow!("the tenant {branch} has no key: reset it"))?;
        Ok(kept.as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
    }

    async fn usage(&self, _parent: &str, _branch: &str) -> anyhow::Result<Usage> {
        Ok(Usage::default())
    }
}

// ── Search: a zero-copy branch of an Elasticsearch-compatible cluster (roll search) ──

pub struct Search(pub Api);

impl Search {
    /// `url` with the branch's credentials in it (the password is shown once, so it's kept).
    fn kept(v: &Value) -> anyhow::Result<String> {
        let url = s(&v["url"]).ok_or_else(|| anyhow::anyhow!("a search branch without a url"))?;
        let (user, pass) = (s(&v["username"]).unwrap_or_default(), s(&v["password"]).unwrap_or_default());
        let with = match url.split_once("://") {
            Some((scheme, rest)) if !user.is_empty() => format!("{scheme}://{}:{}@{rest}", seg(&user), seg(&pass)),
            _ => url,
        };
        Ok(json!({ "ELASTICSEARCH_URL": with }).to_string())
    }
}

#[async_trait]
impl Brancher for Search {
    fn kind(&self) -> Kind {
        Kind::Search
    }

    async fn branch(&self, parent: &str, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        let path = format!("/v1/search/clusters/{}/branches", seg(parent));
        match self.0.post(&path, json!({ "name": name })).await {
            Ok(v) => Ok(Branched { r#ref: s(&v["id"]).ok_or_else(|| anyhow::anyhow!("a search branch without an id"))?, kept: Some(Self::kept(&v)?) }),
            // From before a crash: its password is gone, and it holds nothing yet. Make it again.
            Err(e) if conflict_is_ours(&e) => {
                let c = self.0.get(&format!("/v1/search/clusters/{}", seg(name))).await?;
                self.delete(parent, &s(&c["id"]).unwrap_or_default(), name).await?;
                let v = self.0.post(&path, json!({ "name": name })).await?;
                let _ = mode;
                Ok(Branched { r#ref: s(&v["id"]).unwrap_or_default(), kept: Some(Self::kept(&v)?) })
            }
            Err(e) => Err(e),
        }
    }

    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        self.delete(parent, branch, name).await?;
        self.branch(parent, name, mode).await
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        let c = match self.0.get(&format!("/v1/search/clusters/{}", seg(branch))).await {
            Ok(c) => c,
            Err(e) if is_404(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        let of_parent = c["parent_id"] == parent || c["parent_name"] == parent;
        anyhow::ensure!(c["name"] == name && of_parent && branch != parent, "refusing to delete search cluster {branch}: it isn't {parent}'s branch {name}");
        self.0.delete(&format!("/v1/search/clusters/{}", seg(branch))).await
    }

    async fn connect(&self, _parent: &str, branch: &str, kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let kept: Value = kept.and_then(|k| serde_json::from_str(k).ok()).ok_or_else(|| anyhow::anyhow!("the search branch {branch} has no credentials: reset it"))?;
        Ok(kept.as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
    }

    async fn usage(&self, _parent: &str, branch: &str) -> anyhow::Result<Usage> {
        let c = self.0.get(&format!("/v1/search/clusters/{}", seg(branch))).await?;
        let bytes = c["stored_bytes"].as_i64().or_else(|| c["usage"]["stored_bytes"].as_i64()).unwrap_or(0);
        Ok(Usage { bytes, storage_usd_month: gb(bytes) * 0.023, active_usd_hour: 0.0, month_to_date_usd: 0.0 })
    }
}

// ── Services: roll apps' catalog when the project has one, else roll.toml containers ──

/// roll apps (docs/apps/) over this node's API: a preview is an ephemeral environment
/// `pv-<id>` of each of the project's services, deployed at the change's commit. roll apps
/// injects the roll secrets environment of the same name; the branches' URLs are also set as
/// variables, for a project without roll secrets.
pub struct RollApps {
    api: Api,
    /// (preview, service) → service id, learned at deploy (looked up again after a restart).
    ids: std::sync::Mutex<std::collections::HashMap<(String, String), String>>,
}

impl RollApps {
    pub fn new(api: Api) -> RollApps {
        RollApps { api, ids: Default::default() }
    }

    /// The roll apps service `name` of `project`.
    async fn service_in(&self, project: &str, name: &str) -> anyhow::Result<Option<String>> {
        let list = self.api.get(&format!("/v1/apps/projects/{}/services", seg(project))).await?;
        Ok(list.as_array().into_iter().flatten().find(|x| x["name"] == name).and_then(|x| s(&x["id"])))
    }

    /// The service of `preview` named `name`: one with an environment `pv-<id>`.
    async fn find(&self, preview: &str, name: &str) -> Option<(String, String)> {
        let env = roll_previews::engine::env_name(preview);
        if let Some(id) = self.ids.lock().unwrap_or_else(|p| p.into_inner()).get(&(preview.to_string(), name.to_string())) {
            return Some((id.clone(), env));
        }
        let all = self.api.get("/v1/apps/services").await.ok()?;
        let svc = all
            .as_array()?
            .iter()
            .find(|x| x["name"] == name && x["environments"].as_array().is_some_and(|e| e.iter().any(|e| e["name"] == env.as_str())))?;
        let id = s(&svc["id"])?;
        self.ids.lock().unwrap_or_else(|p| p.into_inner()).insert((preview.to_string(), name.to_string()), id.clone());
        Some((id, env))
    }

    fn env_path(id: &str, env: &str) -> String {
        format!("/v1/apps/services/{}/environments/{}", seg(id), seg(env))
    }
}

#[async_trait]
impl roll_previews::ports::Apps for RollApps {
    async fn catalog(&self, project: &str) -> anyhow::Result<Option<Vec<roll_previews::ports::CatalogService>>> {
        let list = match self.api.get(&format!("/v1/apps/projects/{}/services", seg(project))).await {
            Ok(l) => l,
            // No roll apps on this node (or the project has none): roll.toml's services.
            Err(e) if is_404(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(Some(
            list.as_array()
                .into_iter()
                .flatten()
                .map(|x| roll_previews::ports::CatalogService {
                    name: s(&x["name"]).unwrap_or_default(),
                    repo: (x["source"]["kind"] == "repo").then(|| s(&x["source"]["repo"])).flatten(),
                    memory_mb: x["run"]["memory_mb"].as_u64().unwrap_or(512) as u32,
                })
                .collect(),
        ))
    }

    async fn deploy(&self, t: &roll_previews::ports::Target) -> anyhow::Result<Option<String>> {
        let id = self.service_in(&t.project, &t.service).await?.ok_or_else(|| anyhow::anyhow!("roll apps has no service {} in the project", t.service))?;
        self.ids.lock().unwrap_or_else(|p| p.into_inner()).insert((t.preview.clone(), t.service.clone()), id.clone());
        let path = Self::env_path(&id, &t.environment);
        self.api.call(Method::PUT, &path, Some(json!({ "branch": null, "ephemeral": true }))).await?;
        // Every variable the preview sets: the branches' plain names and the project's overrides,
        // and each prefixed link to a branched parent pointed at the branch (a link is roll apps'
        // own variable, which would still reach production).
        let mut set: serde_json::Map<String, Value> = t.vars.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        let links = self.api.get(&format!("/v1/apps/services/{}/links", seg(&id))).await.unwrap_or(Value::Null);
        for l in links.as_array().into_iter().flatten() {
            let Some((_, _, vars)) = t.branches.iter().find(|(k, parent, _)| l["kind"] == k.as_str() && l["ref"] == parent.as_str()) else { continue };
            let prefix = l["prefix"].as_str().unwrap_or("");
            for name in l["vars"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                if let Some((_, v)) = vars.iter().find(|(k, _)| name.strip_prefix(prefix) == Some(k.as_str())) {
                    set.insert(name.to_string(), json!(v));
                }
            }
        }
        if !set.is_empty() {
            self.api.call(Method::PATCH, &format!("{path}/variables"), Some(json!({ "set": set }))).await?;
        }
        // Deploys aren't idempotent: skip one the environment already has (or is building).
        let env = self.api.get(&path).await?;
        let has = |v: &Value| t.git.is_some() && v["revision"].as_str() == t.git.as_deref();
        if !(has(&env["active"]) || has(&env["latest"])) {
            let body = match &t.git {
                Some(sha) => json!({ "revision": sha, "message": format!("roll preview {}", t.preview) }),
                None => json!({ "message": format!("roll preview {}", t.preview) }),
            };
            self.api.post(&format!("{path}/deploys"), body).await?;
        }
        let env = self.api.get(&path).await?;
        Ok(s(&env["url"]))
    }

    async fn wake(&self, preview: &str, service: &str) -> anyhow::Result<()> {
        let (id, env) = self.find(preview, service).await.ok_or_else(|| anyhow::anyhow!("roll apps has no preview environment for {service}"))?;
        self.api.post(&format!("{}/wake", Self::env_path(&id, &env)), json!({})).await.map(|_| ())
    }

    async fn sleep(&self, preview: &str, service: &str) {
        if let Some((id, env)) = self.find(preview, service).await {
            let _ = self.api.post(&format!("{}/sleep", Self::env_path(&id, &env)), json!({})).await;
        }
    }

    async fn remove(&self, preview: &str, service: &str) -> anyhow::Result<()> {
        if let Some((id, env)) = self.find(preview, service).await {
            // Gone already counts as done; anything else is retried.
            self.api.delete(&Self::env_path(&id, &env)).await?;
        }
        self.ids.lock().unwrap_or_else(|p| p.into_inner()).remove(&(preview.to_string(), service.to_string()));
        Ok(())
    }

    async fn logs(&self, preview: &str, service: &str, lines: usize) -> Vec<String> {
        let Some((id, env)) = self.find(preview, service).await else { return Vec::new() };
        let v = self.api.get(&format!("{}/logs?tail={lines}", Self::env_path(&id, &env))).await.unwrap_or(Value::Null);
        v.as_array().into_iter().flatten().filter_map(|l| s(&l["text"])).collect()
    }

    fn status(&self, _preview: &str, _service: &str) -> Option<roll_previews::ServiceState> {
        None
    }
}

/// roll apps for the services in its catalog, the container runtime for `roll.toml`'s.
pub struct Services {
    pub apps: RollApps,
    pub runtime: Arc<roll_previews::runtime::Runtime>,
}

impl Services {
    /// Whether the runtime runs this service (it has a `roll.toml` spec and was started here).
    fn local(&self, preview: &str, service: &str) -> bool {
        use roll_previews::ports::Apps;
        self.runtime.status(preview, service).is_some()
    }
}

#[async_trait]
impl roll_previews::ports::Apps for Services {
    async fn catalog(&self, project: &str) -> anyhow::Result<Option<Vec<roll_previews::ports::CatalogService>>> {
        self.apps.catalog(project).await
    }

    async fn deploy(&self, t: &roll_previews::ports::Target) -> anyhow::Result<Option<String>> {
        // Catalog services come without a roll.toml spec.
        if t.spec.image.is_empty() { self.apps.deploy(t).await } else { self.runtime.deploy(t).await }
    }

    async fn wake(&self, preview: &str, service: &str) -> anyhow::Result<()> {
        if self.apps.find(preview, service).await.is_some() { self.apps.wake(preview, service).await } else { self.runtime.wake(preview, service).await }
    }

    async fn sleep(&self, preview: &str, service: &str) {
        if self.local(preview, service) {
            self.runtime.sleep(preview, service).await
        } else {
            self.apps.sleep(preview, service).await
        }
    }

    async fn remove(&self, preview: &str, service: &str) -> anyhow::Result<()> {
        self.runtime.remove(preview, service).await?;
        self.apps.remove(preview, service).await
    }

    async fn logs(&self, preview: &str, service: &str, lines: usize) -> Vec<String> {
        if self.apps.find(preview, service).await.is_some() { self.apps.logs(preview, service, lines).await } else { self.runtime.logs(preview, service, lines).await }
    }

    async fn seed(&self, job: &roll_previews::ports::SeedJob) -> anyhow::Result<(i32, String)> {
        self.runtime.seed(job).await
    }

    fn status(&self, preview: &str, service: &str) -> Option<roll_previews::ServiceState> {
        self.runtime.status(preview, service)
    }
}

// ── Secrets: an ephemeral environment of roll secrets per preview (docs/secrets.md) ──

/// roll secrets over this node's API. A project with no secrets (or a node without roll
/// secrets: every route 404s) gives its previews no environment and nothing to resolve.
pub struct Secrets(pub Api);

fn set_ops(vars: &[(String, String)]) -> Value {
    json!(vars.iter().map(|(k, v)| json!({ "op": "set", "key": k, "value": v })).collect::<Vec<_>>())
}

#[async_trait]
impl roll_previews::ports::Secrets for Secrets {
    async fn ensure(&self, project: &str, env: &str, base: &str, vars: &[(String, String)]) -> anyhow::Result<()> {
        let envs = format!("/v1/secrets/projects/{}/environments", seg(project));
        let body = json!({ "slug": env, "name": env, "kind": "ephemeral", "base": base, "labels": { "preview": env }, "secrets": set_ops(vars) });
        match self.0.post(&envs, body).await {
            Ok(_) => Ok(()),
            // It exists: bring its overrides up to date (one change).
            Err(e) if conflict_is_ours(&e) => {
                self.0.post(&format!("{envs}/{}/changes", seg(env)), json!({ "message": "roll preview", "ops": set_ops(vars) })).await?;
                Ok(())
            }
            Err(e) if is_404(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn resolve(&self, project: &str, env: &str) -> anyhow::Result<Vec<(String, String)>> {
        let path = format!("/v1/secrets/projects/{}/environments/{}/resolve", seg(project), seg(env));
        match self.0.post(&path, json!({ "path": "/", "consumer": format!("preview:{env}") })).await {
            Ok(v) => {
                if let Some(p) = v["problems"].as_array().filter(|p| !p.is_empty()) {
                    tracing::warn!(%env, problems = p.len(), "previews: unresolved secrets");
                }
                Ok(v["values"].as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
            }
            Err(e) if is_404(&e) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    async fn delete(&self, project: &str, env: &str) -> anyhow::Result<()> {
        self.0.delete(&format!("/v1/secrets/projects/{}/environments/{}?confirm={}", seg(project), seg(env), seg(env))).await
    }
}

#[cfg(test)]
mod tests {
    use super::bucket_name;

    #[test]
    fn bucket_branches_are_named_after_their_parent() {
        assert_eq!(bucket_name("assets", "pv-abc"), "assets-pv-abc");
        let long = "a".repeat(80);
        let n = bucket_name(&long, "pv-abcdefghijkl");
        assert_eq!(n.len(), 63);
        assert!(n.ends_with("-pv-abcdefghijkl"));
    }
}
