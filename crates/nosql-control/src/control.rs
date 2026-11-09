//! The roll nosql control plane: databases and their settings, branches (with reset and
//! expiry), point-in-time restores, credentials, the console, imports, metering and the cost
//! estimate. Serving is done by a [`nosql_server::Node`] whose catalog is this control plane's
//! store.

use crate::pricing::{Cost, Pricing};
use crate::settings;
use crate::store::{NosqlDb, Store, UsageSample, UsageTotals};
use futures::future::BoxFuture;
use nosql_engine as eng;
use nosql_server::{Catalog, Credential, DbSpec, Directory, Lease, Node, NodeConfig, ServerSettings};
use nosql_storage::log::Durability;
use nosql_storage::store::{Store as Storage, StoreConfig};
use nosql_storage::{Objects, manifest};
use nosql_types::Dialect;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(thiserror::Error, Debug)]
pub enum ControlError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, ControlError>;

/// A storage error about the requested point in time is the caller's (400).
fn history_err(e: anyhow::Error) -> ControlError {
    let m = format!("{e:#}");
    if m.contains("older than the retained history") { ControlError::Invalid(m) } else { ControlError::Internal(e) }
}

fn engine_err(e: eng::Error) -> ControlError {
    ControlError::Internal(anyhow::anyhow!("{}", e.message))
}

#[derive(Clone)]
pub struct NosqlConfig {
    pub home: PathBuf,
    /// This node's name in object storage (commit log segments are per node).
    pub node_name: String,
    /// Host written into endpoints (without a domain).
    pub public_host: String,
    pub cql_port: u16,
    /// ScyllaDB's shard-aware port (advertised in SUPPORTED).
    pub cql_shard_aware_port: Option<u16>,
    /// A dedicated TLS shard-aware port (`native_shard_aware_transport_port_ssl`; advertised as
    /// `SCYLLA_SHARD_AWARE_PORT_SSL` while `cql_shard_aware_port` stays plain).
    pub cql_shard_aware_port_ssl: Option<u16>,
    /// The DynamoDB-compatible API (Alternator), plain HTTP.
    pub alternator_port: Option<u16>,
    /// Alternator over HTTPS (ScyllaDB's `alternator_https_port`).
    pub alternator_https_port: Option<u16>,
    /// DNS suffix of Alternator host names (`alternator.example.com` → `<id>.alternator.example.com`).
    pub alternator_domain: Option<String>,
    /// ScyllaDB's REST API (`nodetool`).
    pub rest_port: Option<u16>,
    /// The CQL listeners use TLS.
    pub tls: bool,
    /// SNI suffix (`nosql.example.com`): hosted endpoints are `<id>.<domain>`.
    pub domain: Option<String>,
    /// Single mode: connections that name no database go to this one (by name; created with
    /// `AllowAllAuthenticator` when missing).
    pub single: Option<String>,
    /// Single mode: the database's cluster name when it is created (default: its name).
    pub single_cluster_name: Option<String>,
    /// Single mode: the initial superuser's password when the database is created (default:
    /// random). The `roll/nosql` image uses `cassandra`, like the reference images.
    pub single_password: Option<String>,
    /// Single mode: the database's settings (`settings` keys), on top of
    /// `AllowAllAuthenticator` / `AllowAllAuthorizer`. When given, they replace the stored
    /// settings at every start, as a node's configuration file does on restart.
    pub single_settings: Option<Map<String, Value>>,
    pub storage: StoreConfig,
    /// How often unreferenced objects are looked for (deleted once found twice this far
    /// apart) and restore archives expired. Run on one node per bucket.
    pub gc_interval: Option<Duration>,
    /// How often expired branches are deleted.
    pub expiry_interval: Duration,
    /// Metering sample length.
    pub metering_interval: Duration,
    /// How often scheduled imports are checked.
    pub import_interval: Duration,
    /// Token ranges scanned at once per imported table.
    pub import_parallelism: usize,
    pub defaults: Defaults,
    pub pricing: Pricing,
}

#[derive(Clone, Debug, Serialize)]
pub struct Defaults {
    pub dialect: String,
    pub datacenter: String,
    pub rack: String,
    /// The initial superuser's role name.
    pub superuser: String,
    pub durability: String,
    pub idle_timeout: i64,
    pub history_retention: i64,
}

impl NosqlConfig {
    pub fn local(home: PathBuf) -> Self {
        NosqlConfig {
            home,
            node_name: "local".into(),
            public_host: "127.0.0.1".into(),
            cql_port: 59042,
            cql_shard_aware_port: Some(59043),
            cql_shard_aware_port_ssl: None,
            alternator_port: Some(58000),
            alternator_https_port: None,
            alternator_domain: None,
            rest_port: Some(59100),
            tls: false,
            domain: None,
            single: None,
            single_cluster_name: None,
            single_password: None,
            single_settings: None,
            storage: StoreConfig::default(),
            gc_interval: Some(Duration::from_secs(3600)),
            expiry_interval: Duration::from_secs(30),
            metering_interval: Duration::from_secs(60),
            import_interval: Duration::from_secs(30),
            import_parallelism: 8,
            defaults: Defaults {
                dialect: "scylla".into(),
                datacenter: "datacenter1".into(),
                rack: "rack1".into(),
                superuser: "cassandra".into(),
                durability: "async".into(),
                idle_timeout: 300,
                history_retention: 7 * 86_400,
            },
            pricing: Pricing::default(),
        }
    }
}

/// Counters seen at the previous metering sample.
#[derive(Clone, Copy, Debug, Default)]
struct Seen {
    requests: u64,
    alternator_requests: u64,
    bytes_in: u64,
    bytes_out: u64,
    reads: u64,
    writes: u64,
}

pub struct Control {
    pub cfg: NosqlConfig,
    pub store: Arc<Store>,
    pub node: Arc<Node>,
    pub storage: Arc<Storage>,
    /// Owned storage bytes last seen per database (list views don't read manifests).
    sizes: Mutex<HashMap<String, i64>>,
    pub(crate) running_imports: Mutex<HashSet<String>>,
    seen: Mutex<HashMap<String, Seen>>,
    /// The single-mode database's id.
    pub single_id: Option<String>,
}

const ID_ALPHABET: &[u8] = b"0123456789abcdefghjkmnpqrstvwxyz";
const SECRET_ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";

fn random_from(alphabet: &[u8], n: usize) -> String {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).expect("os randomness");
    bytes.iter().map(|b| alphabet[*b as usize % alphabet.len()] as char).collect()
}

/// A database id: `n` and 11 lowercase base32 characters (a DNS label).
pub fn new_db_id() -> String {
    format!("n{}", random_from(ID_ALPHABET, 11))
}

pub fn new_import_id() -> String {
    format!("imp_{}", random_from(ID_ALPHABET, 12))
}

pub fn new_password() -> String {
    random_from(SECRET_ALPHABET, 32)
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

/// Lowercase `[a-z0-9-]{1,40}`.
pub fn valid_name(n: &str) -> bool {
    (1..=40).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A datacenter, rack or cluster name: printable, no quotes, at most 64 bytes.
fn valid_label(n: &str) -> bool {
    (1..=64).contains(&n.len()) && n.chars().all(|c| !c.is_control() && c != '\'' && c != '"')
}

/// `Some(None)` for an explicit `null`, `Some(Some(v))` for a value, `None` when absent.
fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> std::result::Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

/// `POST /databases`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CreateReq {
    pub name: String,
    pub dialect: Option<String>,
    pub datacenter: Option<String>,
    pub rack: Option<String>,
    pub cluster_name: Option<String>,
    #[serde(default)]
    pub settings: Map<String, Value>,
    pub durability: Option<String>,
    pub idle_timeout: Option<i64>,
    pub history_retention: Option<i64>,
    pub read_only: Option<bool>,
    pub expires_at: Option<i64>,
    pub masking: Option<Map<String, Value>>,
}

/// `PATCH /databases/{id}`: only the fields given change. `settings` is merged (`null` removes
/// a key); `masking` replaces the policy; `expires_at: null` clears the expiry.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct PatchReq {
    pub name: Option<String>,
    pub settings: Option<Map<String, Value>>,
    pub idle_timeout: Option<i64>,
    pub durability: Option<String>,
    pub history_retention: Option<i64>,
    pub masking: Option<Map<String, Value>>,
    pub read_only: Option<bool>,
    #[serde(default, deserialize_with = "nullable")]
    pub expires_at: Option<Option<i64>>,
}

/// `POST /databases/{id}/branches`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct BranchReq {
    pub name: String,
    /// Unix seconds; default now.
    pub at: Option<i64>,
    #[serde(default)]
    pub schema_only: bool,
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub read_only: bool,
}

/// Serves the node's catalog from the store.
struct StoreCatalog {
    store: Arc<Store>,
    cfg: NosqlConfig,
}

impl Catalog for StoreCatalog {
    fn lookup(&self, id: &str) -> BoxFuture<'_, Option<DbSpec>> {
        let d = self.store.db(id).ok().flatten();
        let spec = d.and_then(|d| spec_for(&self.cfg, &d).ok());
        Box::pin(async move { spec })
    }
}

fn dialect_of(name: &str) -> Result<Dialect> {
    Dialect::parse(name).ok_or_else(|| ControlError::Invalid(format!("unknown dialect {name} (scylla or cassandra)")))
}

/// What the node serves a database as.
pub fn spec_for(cfg: &NosqlConfig, d: &NosqlDb) -> Result<DbSpec> {
    let mut spec = DbSpec::new(d.id.clone(), dialect_of(&d.dialect)?);
    let c = &mut spec.config;
    c.cluster_name = d.cluster_name.clone();
    c.datacenter = d.datacenter.clone();
    c.rack = d.rack.clone();
    c.native_port = cfg.cql_port;
    if let Ok(ip) = cfg.public_host.parse() {
        c.address = ip;
    }
    // Hosted databases authenticate and authorize like a production cluster; single mode
    // overrides this in its settings.
    c.settings.authenticator = "PasswordAuthenticator".into();
    c.settings.authorizer = "CassandraAuthorizer".into();
    settings::apply(c, &d.settings).map_err(ControlError::Invalid)?;
    spec.memtable_space = settings::memtable_space(&d.settings).map_err(ControlError::Invalid)?;
    c.masking = masking_policy(&d.masking)?;
    c.superuser = Some(eng::config::Superuser { name: d.superuser.clone(), password: d.password.clone() });
    spec.credentials.push(Credential { role: d.superuser.clone(), password: d.password.clone() });
    spec.tokens.push(d.password.clone());
    spec.durability = if d.durability == "sync" { Durability::Sync } else { Durability::Async };
    spec.history_retention = Duration::from_secs(d.history_retention_s.max(0) as u64);
    spec.idle_timeout = Some(Duration::from_secs(d.idle_timeout_s.max(0) as u64));
    spec.read_only = d.read_only;
    Ok(spec)
}

fn check_masking(m: &Map<String, Value>) -> Result<()> {
    masking_policy(m).map(|_| ())
}

/// The engine's masking policy from the stored `{"ks.table.column": "mask_inner(1, 1)"}`
/// (`nosql_engine::masking`: applied to every role, inherited by branches).
fn masking_policy(m: &Map<String, Value>) -> Result<std::collections::BTreeMap<(String, String, String), eng::config::MaskCall>> {
    let mut out = std::collections::BTreeMap::new();
    for (col, f) in m {
        let parts: Vec<&str> = col.split('.').collect();
        if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
            return Err(ControlError::Invalid(format!("masking: {col:?} is not keyspace.table.column")));
        }
        let Some(f) = f.as_str() else { return Err(ControlError::Invalid(format!("masking: {col}: expected a mask function call"))) };
        let call = eng::config::MaskCall::parse(f).map_err(|e| ControlError::Invalid(format!("masking: {col}: {e}")))?;
        out.insert((parts[0].to_string(), parts[1].to_string(), parts[2].to_string()), call);
    }
    Ok(out)
}

impl Control {
    pub async fn new(cfg: NosqlConfig, objects: Objects) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(&cfg.home)?;
        let store = Arc::new(Store::open(&cfg.home.join("nosql.db"))?);
        store.fail_running_imports(now())?;
        let storage_cfg = StoreConfig { node: cfg.node_name.clone(), tmp_dir: Some(cfg.home.join("tmp")), ..cfg.storage.clone() };
        let storage = Storage::start(objects, storage_cfg).await?;
        let single_id = match &cfg.single {
            Some(name) => Some(ensure_single(&cfg, &store, name)?),
            None => None,
        };
        let server = ServerSettings {
            dialect: nosql_server::convert::proto_dialect(Dialect::parse(&cfg.defaults.dialect).unwrap_or(Dialect::Scylla)),
            shard_aware_port: cfg.cql_shard_aware_port.filter(|_| !cfg.tls),
            shard_aware_port_ssl: cfg.cql_shard_aware_port_ssl.or(cfg.cql_shard_aware_port.filter(|_| cfg.tls)),
            ..Default::default()
        };
        let node = Node::new(
            Arc::new(StoreCatalog { store: store.clone(), cfg: cfg.clone() }),
            storage.clone(),
            NodeConfig {
                idle_timeout: Some(Duration::from_secs(cfg.defaults.idle_timeout.max(0) as u64)),
                domain: cfg.domain.clone(),
                default_db: single_id.clone(),
                server,
                alternator_domain: cfg.alternator_domain.clone(),
            },
        );
        let control = Arc::new(Self {
            cfg,
            store,
            node,
            storage,
            sizes: Mutex::default(),
            running_imports: Mutex::default(),
            seen: Mutex::default(),
            single_id,
        });
        if let Some(id) = &control.single_id
            && let Some(d) = control.store.db(id)?
            && manifest::read_manifest(control.objects(), id).await?.is_none()
        {
            // Its first (numbered) manifest and initial superuser.
            drop(control.lease(&d).await?);
        }
        control.clone().start_loops();
        Ok(control)
    }

    pub fn objects(&self) -> &Objects {
        self.storage.objects()
    }

    fn start_loops(self: Arc<Self>) {
        let weak = Arc::downgrade(&self);
        let every = self.cfg.metering_interval;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.tick().await;
            loop {
                tick.tick().await;
                let Some(c) = weak.upgrade() else { return };
                if let Err(e) = c.sample(every.as_secs().max(1) as i64) {
                    tracing::warn!(error = %e, "nosql metering failed");
                }
            }
        });
        let weak = Arc::downgrade(&self);
        let every = self.cfg.expiry_interval;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let Some(c) = weak.upgrade() else { return };
                match c.expire_now().await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(deleted = n, "nosql expired branches deleted"),
                    Err(e) => tracing::warn!(error = %e, "nosql expiry failed"),
                }
            }
        });
        let weak = Arc::downgrade(&self);
        let every = self.cfg.import_interval;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let Some(c) = weak.upgrade() else { return };
                if let Err(e) = c.clone().run_due_imports() {
                    tracing::warn!(error = %e, "nosql import scheduling failed");
                }
            }
        });
        if let Some(every) = self.cfg.gc_interval {
            let weak = Arc::downgrade(&self);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(every).await;
                    let Some(c) = weak.upgrade() else { return };
                    match c.collect_garbage(every).await {
                        Ok((archives, st)) => tracing::info!(archives, deleted = st.deleted, bytes = st.deleted_bytes, pending = st.unreferenced - st.deleted, "nosql storage gc"),
                        Err(e) => tracing::warn!(error = %e, "nosql storage gc failed"),
                    }
                }
            });
        }
    }

    // ---- lookups ---------------------------------------------------------------------------

    pub fn get(&self, id_or_name: &str) -> Result<NosqlDb> {
        if let Some(d) = self.store.db(id_or_name)? {
            return Ok(d);
        }
        self.store.db_by_name(id_or_name)?.ok_or_else(|| ControlError::NotFound(format!("no nosql database {id_or_name}")))
    }

    pub fn list(&self) -> Result<Vec<NosqlDb>> {
        Ok(self.store.dbs()?)
    }

    pub fn spec(&self, d: &NosqlDb) -> Result<DbSpec> {
        spec_for(&self.cfg, d)
    }

    /// Loads the database if needed and takes a lease on it (it stays loaded while held).
    pub async fn lease(&self, d: &NosqlDb) -> Result<Lease> {
        self.lease_spec(self.spec(d)?).await
    }

    async fn lease_spec(&self, spec: DbSpec) -> Result<Lease> {
        let spec = Arc::new(spec);
        Directory::open(self.node.as_ref(), &spec).await.map_err(|e| match e {
            nosql_server::directory::OpenError::Closing => ControlError::Conflict(e.to_string()),
            nosql_server::directory::OpenError::Unavailable(m) => ControlError::Internal(anyhow::anyhow!("{m}")),
        })
    }

    /// An engine session acting as the database's initial superuser.
    pub fn session(lease: &Lease, d: &NosqlDb, driver: &str) -> eng::Session {
        let client = eng::ClientInfo { driver_name: Some(driver.into()), protocol_version: 4, ..Default::default() };
        let mut s = lease.db().connect(client);
        s.state.user = Some(d.superuser.clone());
        s
    }

    // ---- views -----------------------------------------------------------------------------

    /// The Alternator endpoint of a database: `https://<id>.<alternator domain>` when hosted
    /// (the `Host` names the database), else the node's address.
    pub fn alternator_endpoint(&self, id: &str) -> Option<String> {
        let host = |h: &str, p: u16| if h.contains(':') { format!("[{h}]:{p}") } else { format!("{h}:{p}") };
        match (&self.cfg.alternator_domain, self.cfg.alternator_https_port, self.cfg.alternator_port) {
            (Some(domain), Some(p), _) => Some(format!("https://{}", host(&format!("{id}.{domain}"), p))),
            (Some(domain), None, Some(p)) => Some(format!("http://{}", host(&format!("{id}.{domain}"), p))),
            (None, Some(p), _) => Some(format!("https://{}", host(&self.cfg.public_host, p))),
            (None, None, Some(p)) => Some(format!("http://{}", host(&self.cfg.public_host, p))),
            _ => None,
        }
    }

    /// The SigV4 access key id of the database's superuser: `role@id` (the access key names the
    /// database) unless the endpoint's host does.
    pub fn alternator_access_key(&self, d: &NosqlDb) -> String {
        match self.cfg.alternator_domain {
            Some(_) => d.superuser.clone(),
            None => format!("{}@{}", d.superuser, d.id),
        }
    }

    /// The database's Alternator credentials: endpoint, access key id and secret access key (the
    /// superuser's salted hash, as ScyllaDB uses it). Loads the database to read the hash.
    pub async fn alternator_credentials(&self, id: &str) -> Result<Value> {
        let d = self.get(id)?;
        let lease = self.lease(&d).await?;
        let secret = nosql_engine::auth::salted_hash(lease.db(), &d.superuser).await.map_err(engine_err)?;
        Ok(json!({
            "endpoint": self.alternator_endpoint(&d.id),
            "region": "us-east-1",
            "access_key_id": self.alternator_access_key(&d),
            "secret_access_key": secret,
            "enforced": lease.spec.requires_auth(),
        }))
    }

    fn host_for(&self, id: &str) -> String {
        match (&self.cfg.domain, self.cfg.tls) {
            (Some(domain), true) => format!("{id}.{domain}"),
            _ => self.cfg.public_host.clone(),
        }
    }

    /// The database as the API shows it (see the field list in `roll-cli`'s `nosql` module).
    pub fn view(&self, d: &NosqlDb) -> Value {
        let host = self.host_for(&d.id);
        let hp = |port: u16| if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
        let sni = self.cfg.tls && self.cfg.domain.is_some();
        let username = if sni { d.superuser.clone() } else { format!("{}@{}", d.superuser, d.id) };
        let loaded = self.node.get_loaded(&d.id);
        let metrics = self.node.metrics(&d.id).unwrap_or_default();
        let spec = self.spec(d).ok();
        let mut v = serde_json::to_value(d).unwrap_or_else(|_| json!({}));
        let extra = json!({
            "state": if loaded.is_some() { "running" } else { "idle" },
            "endpoints": {
                "cql": hp(self.cfg.cql_port),
                "cql_shard_aware": self.cfg.cql_shard_aware_port.map(hp),
                "alternator": self.alternator_endpoint(&d.id),
                "rest": self.cfg.rest_port.map(|p| format!("http://{}", hp(p))),
            },
            "alternator": {
                "endpoint": self.alternator_endpoint(&d.id),
                "access_key_id": self.alternator_access_key(d),
                "region": "us-east-1",
            },
            "tls": self.cfg.tls,
            "server_name": self.cfg.domain.as_ref().map(|dm| format!("{}.{dm}", d.id)),
            "credentials": { "username": username, "password": d.password, "role": d.superuser },
            "http_token": format!("{}_{}", d.id, d.password),
            "storage_bytes": self.sizes.lock().unwrap_or_else(|e| e.into_inner()).get(&d.id).copied(),
            "connections": metrics.connections,
            "num_tokens": spec.as_ref().map(|s| s.config.num_tokens),
            "host_id": spec.as_ref().map(|s| nosql_types::timeuuid::format_uuid(&s.config.host_id())),
            "status": {
                "loaded": loaded.is_some(),
                "users": loaded.as_ref().map(|l| l.users()),
                "idle_ms": loaded.as_ref().map(|l| l.idle_ms()),
                "metrics": metrics,
            },
        });
        if let (Some(o), Value::Object(e)) = (v.as_object_mut(), extra) {
            o.extend(e);
        }
        v
    }

    /// Bytes of SSTables the database owns (a branch's references to its parent's aren't
    /// counted: they're the parent's). Reads the manifest when the database isn't loaded.
    pub async fn storage_bytes(&self, id: &str) -> Result<i64> {
        let m = match self.node.get_loaded(id) {
            Some(l) => Some(l.db.store().manifest()),
            None => manifest::read_manifest(self.objects(), id).await?,
        };
        let own = format!("nosql/db/{id}/");
        let bytes = m.map_or(0, |m| {
            m.tables.values().flat_map(|e| e.sstables.iter()).filter(|s| s.key.starts_with(&own)).map(|s| (s.data_size + s.index_size) as i64).sum()
        });
        self.sizes.lock().unwrap_or_else(|e| e.into_inner()).insert(id.to_string(), bytes);
        Ok(bytes)
    }

    // ---- lifecycle -------------------------------------------------------------------------

    fn check(&self, d: &NosqlDb) -> Result<()> {
        if !valid_name(&d.name) {
            return Err(ControlError::Invalid("name: 1-40 lowercase letters, digits or '-'".into()));
        }
        dialect_of(&d.dialect)?;
        for (what, v) in [("datacenter", &d.datacenter), ("rack", &d.rack)] {
            if !valid_label(v) {
                return Err(ControlError::Invalid(format!("{what}: 1-64 printable characters without quotes")));
            }
        }
        if d.cluster_name.len() > 64 || d.cluster_name.chars().any(|c| c.is_control() || c == '\'') {
            return Err(ControlError::Invalid("cluster_name: at most 64 printable characters without quotes".into()));
        }
        if d.durability != "async" && d.durability != "sync" {
            return Err(ControlError::Invalid("durability must be async or sync".into()));
        }
        if d.idle_timeout_s < 0 || d.history_retention_s < 0 {
            return Err(ControlError::Invalid("idle_timeout and history_retention can't be negative".into()));
        }
        if d.history_retention_s > 35 * 86_400 {
            return Err(ControlError::Invalid("history_retention is at most 35 days".into()));
        }
        check_masking(&d.masking)?;
        self.spec(d)?;
        Ok(())
    }

    fn name_free(&self, name: &str, except: Option<&str>) -> Result<()> {
        match self.store.db_by_name(name)? {
            Some(o) if Some(o.id.as_str()) != except => Err(ControlError::Conflict(format!("a database named {name} already exists"))),
            _ => Ok(()),
        }
    }

    pub async fn create(&self, r: CreateReq) -> Result<NosqlDb> {
        let df = &self.cfg.defaults;
        let dialect = r.dialect.clone().unwrap_or_else(|| df.dialect.clone()).to_ascii_lowercase();
        let dialect = match dialect.as_str() {
            "scylladb" => "scylla".to_string(),
            _ => dialect,
        };
        if let Some(t) = r.expires_at
            && t <= now()
        {
            return Err(ControlError::Invalid("expires_at is in the past".into()));
        }
        let d = NosqlDb {
            id: new_db_id(),
            cluster_name: r.cluster_name.clone().unwrap_or_else(|| r.name.clone()),
            name: r.name,
            dialect,
            datacenter: r.datacenter.unwrap_or_else(|| df.datacenter.clone()),
            rack: r.rack.unwrap_or_else(|| df.rack.clone()),
            settings: r.settings,
            durability: r.durability.unwrap_or_else(|| df.durability.clone()).to_ascii_lowercase(),
            idle_timeout_s: r.idle_timeout.unwrap_or(df.idle_timeout),
            history_retention_s: r.history_retention.unwrap_or(df.history_retention),
            read_only: r.read_only.unwrap_or(false),
            masking: r.masking.unwrap_or_default(),
            expires_at: r.expires_at,
            parent_id: None,
            parent_at: None,
            schema_only: false,
            superuser: df.superuser.clone(),
            password: new_password(),
            created_at: now(),
        };
        self.check(&d)?;
        self.name_free(&d.name, None)?;
        self.store.insert_db(&d)?;
        // Load it now: storage writes its first manifest as a numbered version (so it can be
        // branched and restored at once) and the engine creates the initial superuser.
        if let Err(e) = self.lease(&d).await {
            let _ = self.store.delete_db(&d.id);
            let _ = self.storage.delete_database(&d.id).await;
            return Err(e);
        }
        Ok(d)
    }

    /// Change settings. On a loaded database durability, the restore window, the idle timeout,
    /// read-only and most engine settings (thresholds, guardrails, the authorizer, auth caches:
    /// `Database::set_settings`) apply at once; the settings the engine reads when it opens
    /// (`nosql_engine::live::RELOAD_SETTINGS`, `num_tokens`) reload it — connected drivers see
    /// `STATUS_CHANGE DOWN` and reconnect.
    pub async fn update(&self, id: &str, p: PatchReq) -> Result<NosqlDb> {
        let old = self.get(id)?;
        let mut d = old.clone();
        if let Some(n) = p.name {
            d.name = n;
        }
        if let Some(s) = &p.settings {
            d.settings = settings::merge(&d.settings, s);
        }
        if let Some(v) = p.idle_timeout {
            d.idle_timeout_s = v;
        }
        if let Some(v) = p.durability {
            d.durability = v.to_ascii_lowercase();
        }
        if let Some(v) = p.history_retention {
            d.history_retention_s = v;
        }
        if let Some(m) = p.masking {
            d.masking = m;
        }
        if let Some(v) = p.read_only {
            d.read_only = v;
        }
        if let Some(e) = p.expires_at {
            if let Some(t) = e
                && t <= now()
            {
                return Err(ControlError::Invalid("expires_at is in the past".into()));
            }
            d.expires_at = e;
        }
        self.check(&d)?;
        self.name_free(&d.name, Some(&d.id))?;
        self.store.update_db(&d)?;
        if let Some(l) = self.node.get_loaded(&d.id) {
            let spec = self.spec(&d)?;
            let live = d.settings == old.settings || {
                // Only `Settings` and the masking policy can change live; the rest of the
                // configuration needs a load.
                let mut opened = l.db.config().clone();
                opened.settings = spec.config.settings.clone();
                opened.superuser = spec.config.superuser.clone();
                opened.masking = spec.config.masking.clone();
                opened == spec.config && l.db.set_settings(spec.config.settings.clone()).is_ok()
            };
            if !live {
                self.node.evict(&d.id).await?;
                return Ok(d);
            }
            l.db.set_masking(spec.config.masking.clone());
            l.db.store().set_config(spec.storage_config());
            self.node.set_read_only(&d.id, d.read_only);
            self.node.set_idle_timeout(&d.id, spec.idle_timeout);
        }
        Ok(d)
    }

    /// Delete a database: its clients are disconnected, its manifests (and restore archives)
    /// deleted; its objects go once garbage collection finds nothing (no branch) uses them.
    pub async fn delete(&self, id: &str) -> Result<()> {
        let d = self.get(id)?;
        self.node.evict(&d.id).await?;
        self.storage.delete_database(&d.id).await?;
        for archive in self.archives(Some(&d.id)).await? {
            nosql_storage::gc::delete_database(self.objects(), &archive.0).await?;
        }
        self.store.delete_db(&d.id)?;
        self.sizes.lock().unwrap_or_else(|e| e.into_inner()).remove(&d.id);
        Ok(())
    }

    /// Restore archives (`<id>.<unix ms>`) with their time, of one database or all.
    async fn archives(&self, of: Option<&str>) -> Result<Vec<(String, String, i64)>> {
        let mut out = Vec::new();
        for (key, _) in self.objects().list("nosql/db/").await? {
            let Some(name) = key.strip_prefix("nosql/db/").and_then(|k| k.strip_suffix("/manifest.json")) else { continue };
            let Some((db, ms)) = name.rsplit_once('.') else { continue };
            let Ok(ms) = ms.parse::<i64>() else { continue };
            if of.is_none_or(|o| o == db) {
                out.push((name.to_string(), db.to_string(), ms));
            }
        }
        Ok(out)
    }

    fn check_window(&self, d: &NosqlDb, at: i64) -> Result<()> {
        if at > now() + 1 {
            return Err(ControlError::Invalid("that point in time is in the future".into()));
        }
        let oldest = (now() - d.history_retention_s).max(d.created_at);
        if at < oldest {
            return Err(ControlError::Invalid(format!("outside the restore window of {} (earliest is {oldest})", d.name)));
        }
        Ok(())
    }

    /// A database that was never loaded has no manifest yet: load it once.
    async fn ensure_stored(&self, d: &NosqlDb) -> Result<()> {
        if self.node.get_loaded(&d.id).is_none() && manifest::read_manifest(self.objects(), &d.id).await?.is_none() {
            drop(self.lease(d).await?);
        }
        Ok(())
    }

    /// A new database with `parent`'s data as of `at` (Unix seconds, default now). Instant and
    /// zero-copy: the branch references its parent's SSTables and log. It inherits the dialect,
    /// datacenter and cluster names, settings, masking policy, roles and the superuser's
    /// credentials.
    pub async fn branch(&self, parent: &str, r: BranchReq) -> Result<NosqlDb> {
        let p = self.get(parent)?;
        if let Some(t) = r.at {
            self.check_window(&p, t)?;
        }
        if let Some(t) = r.expires_at
            && t <= now()
        {
            return Err(ControlError::Invalid("expires_at is in the past".into()));
        }
        let d = NosqlDb {
            id: new_db_id(),
            name: r.name.clone(),
            parent_id: Some(p.id.clone()),
            parent_at: Some(r.at.unwrap_or_else(now)),
            schema_only: r.schema_only,
            read_only: r.read_only,
            expires_at: r.expires_at,
            created_at: now(),
            ..p.clone()
        };
        self.check(&d)?;
        self.name_free(&d.name, None)?;
        self.ensure_stored(&p).await?;
        self.storage.branch(&p.id, &d.id, r.at.map(|t| t * 1000)).await.map_err(history_err)?;
        self.store.insert_db(&d)?;
        if d.schema_only
            && let Err(e) = self.drop_rows(&d).await
        {
            let _ = self.delete(&d.id).await;
            return Err(e);
        }
        Ok(d)
    }

    /// Truncates every user table (schema and roles stay), then unloads so the next load uses
    /// the database's real spec (read-only branches reject TRUNCATE).
    async fn drop_rows(&self, d: &NosqlDb) -> Result<()> {
        let mut spec = self.spec(d)?;
        spec.read_only = false;
        self.node.evict(&d.id).await?;
        let lease = self.lease_spec(spec).await?;
        let schema = lease.db().schema();
        let dialect = lease.db().dialect();
        let mut s = Self::session(&lease, d, "roll control");
        for ks in schema.keyspaces.values().filter(|k| !k.is_builtin()) {
            for t in ks.tables.values().filter(|t| !t.is_view() && t.kind == eng::schema::TableKind::Regular) {
                let q = format!("TRUNCATE {}.{}", nosql_types::quote_identifier(&ks.name, dialect), nosql_types::quote_identifier(&t.name, dialect));
                s.query(&q, eng::QueryParams::default()).await.map_err(engine_err)?;
            }
        }
        lease.db().flush().await?;
        drop(s);
        drop(lease);
        self.node.evict(&d.id).await?;
        Ok(())
    }

    /// Re-branch from the parent's latest state, or its state at `at`, keeping the id,
    /// endpoint and credentials (the stable preview endpoint). The branch's own changes are
    /// discarded.
    pub async fn reset(&self, id: &str, at: Option<i64>) -> Result<NosqlDb> {
        let mut d = self.get(id)?;
        let Some(pid) = d.parent_id.clone() else { return Err(ControlError::Conflict(format!("{} is not a branch", d.name))) };
        let Some(p) = self.store.db(&pid)? else { return Err(ControlError::Conflict(format!("the parent of {} ({pid}) was deleted", d.name))) };
        if let Some(t) = at {
            self.check_window(&p, t)?;
        }
        self.ensure_stored(&p).await?;
        if let Some(t) = at {
            // Fail before discarding the branch's state.
            let current = manifest::read_manifest(self.objects(), &p.id).await?.ok_or_else(|| anyhow::anyhow!("{} has no manifest", p.id))?;
            if manifest::manifest_at(self.objects(), &p.id, &current, t * 1000).await?.is_none() {
                return Err(ControlError::Invalid(format!("{}: the requested point is older than the retained history", p.name)));
            }
        }
        self.node.evict(&d.id).await?;
        self.storage.delete_database(&d.id).await?;
        self.storage.branch(&p.id, &d.id, at.map(|t| t * 1000)).await?;
        d.parent_at = Some(at.unwrap_or_else(now));
        self.store.update_db(&d)?;
        if d.schema_only {
            self.drop_rows(&d).await?;
        }
        if d.password != p.password || d.superuser != p.superuser {
            // The parent's roles came along; keep the branch's own password.
            self.apply_password(&d).await?;
        }
        Ok(d)
    }

    /// Restore in place to `at` (Unix seconds). Clients are disconnected and reconnect to the
    /// restored state. Returns the archive id holding the replaced state.
    pub async fn restore(&self, id: &str, at: i64) -> Result<String> {
        let d = self.get(id)?;
        self.check_window(&d, at)?;
        self.ensure_stored(&d).await?;
        self.node.evict(&d.id).await?;
        let archive = self.storage.restore_at(&d.id, at * 1000, None).await.map_err(history_err)?;
        archive.ok_or_else(|| ControlError::Internal(anyhow::anyhow!("restore returned no archive")))
    }

    async fn apply_password(&self, d: &NosqlDb) -> Result<()> {
        let lease = self.lease(d).await?;
        if !eng::auth::set_password(lease.db(), &d.superuser, &d.password).await.map_err(engine_err)? {
            tracing::warn!(db = d.id, role = d.superuser, "initial superuser missing; it is created at the next load");
        }
        Ok(())
    }

    /// A new password for the initial superuser. Connected clients stay; new connections need
    /// it (the old one stops working at once, through the platform credential and the role).
    pub async fn rotate_password(&self, id: &str) -> Result<NosqlDb> {
        let mut d = self.get(id)?;
        d.password = new_password();
        self.store.update_db(&d)?;
        // An internal write: read-only databases take it too.
        self.apply_password(&d).await?;
        Ok(d)
    }

    /// Scale to zero now (clients are disconnected).
    pub async fn unload(&self, id: &str) -> Result<bool> {
        let d = self.get(id)?;
        Ok(self.node.evict(&d.id).await?)
    }

    /// Flush memtables to SSTables (when loaded).
    pub async fn flush(&self, id: &str) -> Result<bool> {
        let d = self.get(id)?;
        match self.node.get_loaded(&d.id) {
            Some(l) => {
                l.db.flush().await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Flush and major-compact every table.
    pub async fn compact(&self, id: &str) -> Result<usize> {
        let d = self.get(id)?;
        let lease = self.lease(&d).await?;
        let tables = lease.db().store().tables();
        for t in &tables {
            t.flush().await?;
            t.compact_major().await?;
        }
        Ok(tables.len())
    }

    /// Delete databases whose `expires_at` has passed. Returns how many.
    pub async fn expire_now(&self) -> Result<usize> {
        let mut n = 0;
        for d in self.store.expired(now())? {
            match self.delete(&d.id).await {
                Ok(()) => {
                    tracing::info!(db = d.id, name = d.name, "nosql database expired");
                    n += 1;
                }
                Err(e) => tracing::warn!(db = d.id, error = %e, "deleting an expired nosql database failed"),
            }
        }
        Ok(n)
    }

    /// Expire restore archives older than their database's window (all of a deleted
    /// database's), then delete unreferenced objects found unreferenced `min_age` ago.
    pub async fn collect_garbage(&self, min_age: Duration) -> Result<(usize, nosql_storage::gc::GcStats)> {
        let now_ms = now_ms();
        let mut n = 0;
        for (archive, db, ms) in self.archives(None).await? {
            let window = self.store.db(&db)?.map_or(0, |d| d.history_retention_s * 1000);
            if ms + window < now_ms {
                nosql_storage::gc::delete_database(self.objects(), &archive).await?;
                n += 1;
            }
        }
        Ok((n, self.storage.collect_garbage(min_age).await?))
    }

    // ---- console, schema, activity ---------------------------------------------------------

    /// Run one statement as the initial superuser. CQL errors are results (`{"error", "code",
    /// "code_name"}`), like the server's HTTP data API.
    pub async fn cql(&self, id: &str, r: nosql_server::http::CqlRequest) -> Result<Value> {
        let d = self.get(id)?;
        let paging_state = match r.page_state.as_deref() {
            None | Some("") => None,
            Some(p) => {
                use base64::Engine as _;
                Some(bytes::Bytes::from(base64::engine::general_purpose::STANDARD.decode(p).map_err(|_| ControlError::Invalid("invalid page_state".into()))?))
            }
        };
        let page_size = match r.page_size {
            Some(n) if n > 0 => n.min(nosql_server::http::MAX_PAGE_SIZE),
            _ => nosql_server::http::DEFAULT_PAGE_SIZE,
        };
        let lease = self.lease(&d).await?;
        // Metered like the server's HTTP data API.
        lease.metrics.requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        lease.metrics.http_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut s = Self::session(&lease, &d, "roll console");
        s.state.keyspace = r.keyspace.clone();
        s.set_tracing(r.trace);
        let started = std::time::Instant::now();
        let params = eng::QueryParams { page_size: Some(page_size), paging_state, ..Default::default() };
        let res = s.query(&r.query, params).await;
        let mut out = render(lease.db().dialect(), res);
        if let Some(o) = out.as_object_mut() {
            o.insert("elapsed_ms".into(), json!(started.elapsed().as_secs_f64() * 1000.0));
            if let Some(ks) = &s.state.keyspace {
                o.insert("keyspace".into(), json!(ks));
            }
        }
        Ok(out)
    }

    /// Keyspaces, tables (with storage statistics), types, views, indexes and `DESCRIBE`.
    pub async fn schema(&self, id: &str) -> Result<Value> {
        let d = self.get(id)?;
        let lease = self.lease(&d).await?;
        let db = lease.db().clone();
        let mut body = nosql_server::http::schema_json(&db);
        let schema = db.schema();
        if let Some(kss) = body["keyspaces"].as_array_mut() {
            for ks in kss {
                let ks_name = ks["name"].as_str().unwrap_or_default().to_string();
                for kind in ["tables", "views"] {
                    for t in ks[kind].as_array_mut().into_iter().flatten() {
                        let name = t["name"].as_str().unwrap_or_default().to_string();
                        let st = schema.table(&ks_name, &name).and_then(|t| db.store().get_table(t.table_id())).map(|t| t.stats());
                        if let (Some(o), Some(st)) = (t.as_object_mut(), st) {
                            o.insert(
                                "stats".into(),
                                json!({
                                    "sstable_count": st.sstables,
                                    "live_bytes": st.space_used,
                                    "total_bytes": st.space_used + st.memtable_bytes,
                                    "memtable_bytes": st.memtable_bytes,
                                    "partitions": st.partitions_estimate + st.memtable_partitions,
                                    "reads": st.reads + st.range_reads,
                                    "writes": st.writes,
                                    "tombstones": st.tombstones,
                                }),
                            );
                        }
                    }
                }
            }
        }
        let mut s = Self::session(&lease, &d, "roll control");
        let describe = s.query("DESCRIBE SCHEMA", eng::QueryParams::default()).await.ok().and_then(|r| r.rows().map(crate::diff::describe_text));
        if let Some(o) = body.as_object_mut() {
            o.insert("describe".into(), json!(describe));
        }
        Ok(body)
    }

    /// Connected clients and counters (only for a loaded database; an idle one has none).
    pub async fn activity(&self, id: &str) -> Result<Value> {
        let d = self.get(id)?;
        let metrics = self.node.metrics(&d.id).unwrap_or_default();
        let Some(l) = self.node.get_loaded(&d.id) else {
            return Ok(json!({ "state": "idle", "metrics": metrics, "clients": [], "slow_queries": [], "warnings": [] }));
        };
        let Some(lease) = Lease::acquire(&l) else {
            return Ok(json!({ "state": "idle", "metrics": metrics, "clients": [], "slow_queries": [], "warnings": [] }));
        };
        let dialect = lease.db().dialect();
        let mut s = Self::session(&lease, &d, "roll control");
        let clients_table = if dialect == Dialect::Cassandra { "system_views.clients" } else { "system.clients" };
        let mut clients = query_json(&mut s, &format!("SELECT * FROM {clients_table}"), dialect).await;
        // `address` as `ip:port` and `keyspace` in both dialects (what `roll nosql status` shows).
        for c in &mut clients {
            if let (Some(ip), Some(port)) = (c["address"].as_str().map(str::to_string), c["port"].as_i64()) {
                c["address"] = json!(if ip.contains(':') { format!("[{ip}]:{port}") } else { format!("{ip}:{port}") });
            }
            if c.get("keyspace").is_none() {
                c["keyspace"] = c.get("keyspace_name").cloned().unwrap_or(Value::Null);
            }
        }
        // ScyllaDB's slow query log (`POST /storage_service/slow_query?enable=true`).
        let slow = if dialect == Dialect::Scylla { query_json(&mut s, "SELECT * FROM system_traces.node_slow_log LIMIT 100", dialect).await } else { vec![] };
        // Client warnings (tombstones, batch sizes, aggregations without a partition key, …),
        // newest first.
        let warnings: Vec<Value> = lease
            .db()
            .recent_warnings()
            .into_iter()
            .rev()
            .map(|w| json!({ "at_ms": w.at_ms, "statement": w.statement, "role": w.role, "warning": w.warning }))
            .collect();
        Ok(json!({
            "state": "running",
            "metrics": metrics,
            "users": l.users(),
            "idle_ms": l.idle_ms(),
            "clients": clients,
            "slow_queries": slow,
            "warnings": warnings,
            "memory": lease.db().store().memory_stats(),
        }))
    }

    // ---- metering --------------------------------------------------------------------------

    /// One metering sample of `seconds` (also called by tests): loaded time, reads and writes,
    /// requests, connection time and traffic of every database the node served.
    pub fn sample(&self, seconds: i64) -> anyhow::Result<()> {
        let at = now();
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        for (id, m) in self.node.all_metrics() {
            if self.store.db(&id)?.is_none() {
                seen.remove(&id);
                continue;
            }
            let loaded = self.node.get_loaded(&id);
            let (reads, writes) = loaded.as_ref().map_or((0, 0), |l| {
                l.db.store().stats().values().fold((0u64, 0u64), |(r, w), s| (r + s.reads + s.range_reads, w + s.writes))
            });
            let prev = seen.get(&id).copied().unwrap_or_default();
            // Table counters restart when a database is loaded again.
            let delta = |c: u64, p: u64| if c >= p { c - p } else { c } as i64;
            let cur = Seen {
                requests: m.requests,
                alternator_requests: m.alternator_requests,
                bytes_in: m.bytes_in,
                bytes_out: m.bytes_out,
                reads: if loaded.is_some() { reads } else { prev.reads },
                writes: if loaded.is_some() { writes } else { prev.writes },
            };
            let s = UsageSample {
                at,
                seconds: if loaded.is_some() { seconds } else { 0 },
                reads: if loaded.is_some() { delta(cur.reads, prev.reads) } else { 0 },
                writes: if loaded.is_some() { delta(cur.writes, prev.writes) } else { 0 },
                requests: delta(cur.requests, prev.requests),
                alternator_requests: delta(cur.alternator_requests, prev.alternator_requests),
                connection_seconds: m.connections as i64 * seconds,
                net_in: delta(cur.bytes_in, prev.bytes_in),
                net_out: delta(cur.bytes_out, prev.bytes_out),
            };
            seen.insert(id.clone(), cur);
            if s != (UsageSample { at, ..Default::default() }) {
                self.store.record_usage(&id, &s)?;
            }
        }
        Ok(())
    }

    pub async fn usage(&self, id: &str, days: i64) -> Result<Usage> {
        let d = self.get(id)?;
        let totals = self.store.usage_since(&d.id, now() - days * 86_400)?;
        let storage_bytes = self.storage_bytes(&d.id).await?;
        let cost = self.cfg.pricing.cost(&totals, storage_bytes, days);
        let memtable_cap_bytes = self.memtable_cap(&d);
        Ok(Usage { db_id: d.id, days, totals, storage_bytes, memtable_cap_bytes, cost })
    }

    /// A database's memtable cap: its `memtable_space` setting (its plan) or the node's
    /// per-database default, never above the node's budget.
    pub fn memtable_cap(&self, d: &NosqlDb) -> u64 {
        let st = self.storage.config();
        settings::memtable_space(&d.settings).ok().flatten().unwrap_or(st.memtable_db_bytes).min(st.memtable_budget_bytes)
    }
}

#[derive(Serialize)]
pub struct Usage {
    pub db_id: String,
    pub days: i64,
    #[serde(flatten)]
    pub totals: UsageTotals,
    /// SSTables the database owns on object storage.
    pub storage_bytes: i64,
    /// Its memtable cap (plan).
    pub memtable_cap_bytes: u64,
    pub cost: Cost,
}

/// The single-mode database's id (created when missing, with `AllowAllAuthenticator` like
/// the reference images).
fn ensure_single(cfg: &NosqlConfig, store: &Store, name: &str) -> anyhow::Result<String> {
    let df = &cfg.defaults;
    let settings = single_settings(cfg)?;
    if let Some(mut d) = store.db_by_name(name)?.or(store.db(name)?) {
        // Identity is fixed at creation (data was written under it); say so instead of
        // silently serving something else than configured.
        let cluster = cfg.single_cluster_name.as_deref().unwrap_or(&d.name);
        for (what, stored, wanted) in [("dialect", &d.dialect, df.dialect.as_str()), ("datacenter", &d.datacenter, &df.datacenter), ("rack", &d.rack, &df.rack), ("cluster name", &d.cluster_name, cluster)] {
            if stored != wanted {
                tracing::warn!(db = d.name, "single-mode database keeps its {what} {stored:?} (configured {wanted:?}); remove its data to change it");
            }
        }
        if cfg.single_settings.is_some() && d.settings != settings {
            d.settings = settings;
            store.update_db(&d)?;
        }
        return Ok(d.id);
    }
    if !valid_name(name) {
        anyhow::bail!("single-mode database name {name:?}: 1-40 lowercase letters, digits or '-'");
    }
    // Empty is ScyllaDB's default (`cluster_name` unset in scylla.yaml).
    if let Some(c) = &cfg.single_cluster_name
        && !c.is_empty()
        && !valid_label(c)
    {
        anyhow::bail!("cluster name {c:?}: at most 64 printable characters without quotes");
    }
    for (what, v) in [("datacenter", &df.datacenter), ("rack", &df.rack)] {
        if !valid_label(v) {
            anyhow::bail!("{what} {v:?}: 1-64 printable characters without quotes");
        }
    }
    let d = NosqlDb {
        id: new_db_id(),
        name: name.into(),
        dialect: df.dialect.clone(),
        datacenter: df.datacenter.clone(),
        rack: df.rack.clone(),
        cluster_name: cfg.single_cluster_name.clone().unwrap_or_else(|| name.into()),
        settings,
        durability: df.durability.clone(),
        idle_timeout_s: df.idle_timeout,
        history_retention_s: df.history_retention,
        read_only: false,
        masking: Map::new(),
        expires_at: None,
        parent_id: None,
        parent_at: None,
        schema_only: false,
        superuser: df.superuser.clone(),
        password: cfg.single_password.clone().filter(|p| !p.is_empty()).unwrap_or_else(new_password),
        created_at: now(),
    };
    store.insert_db(&d)?;
    Ok(d.id)
}

/// The single-mode database's settings: `AllowAllAuthenticator` / `AllowAllAuthorizer` (as the
/// reference images ship) under the configured ones, checked like an API update.
fn single_settings(cfg: &NosqlConfig) -> anyhow::Result<Map<String, Value>> {
    let mut settings = Map::new();
    settings.insert("authenticator".into(), json!("AllowAllAuthenticator"));
    settings.insert("authorizer".into(), json!("AllowAllAuthorizer"));
    for (k, v) in cfg.single_settings.iter().flatten() {
        settings.insert(k.clone(), v.clone());
    }
    let dialect = Dialect::parse(&cfg.defaults.dialect).ok_or_else(|| anyhow::anyhow!("unknown dialect {} (scylla or cassandra)", cfg.defaults.dialect))?;
    settings::apply(&mut eng::DbConfig::new("single", dialect), &settings).map_err(|e| anyhow::anyhow!("single-mode settings: {e}"))?;
    Ok(settings)
}

/// Rows of a query as JSON objects (column → value), empty on any error.
async fn query_json(s: &mut eng::Session, q: &str, d: Dialect) -> Vec<Value> {
    let Ok(r) = s.query(q, eng::QueryParams::default()).await else { return vec![] };
    let Some(rows) = r.rows() else { return vec![] };
    let j = nosql_server::http::rows_json(rows, d);
    let names: Vec<String> = rows.metadata.columns.iter().map(|c| c.name.clone()).collect();
    j["values"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| {
            let mut o = Map::new();
            for (n, v) in names.iter().zip(row.as_array().into_iter().flatten()) {
                o.insert(n.clone(), v.clone());
            }
            Value::Object(o)
        })
        .collect()
}

/// An engine answer as the console shows it (the server's HTTP data API format).
pub fn render(d: Dialect, res: eng::Result<eng::Response>) -> Value {
    match res {
        Ok(r) => {
            let mut out = match &r.body {
                eng::ResponseBody::Void => json!({ "kind": "void" }),
                eng::ResponseBody::SetKeyspace(ks) => json!({ "kind": "set_keyspace", "keyspace": ks }),
                eng::ResponseBody::SchemaChange(c) => json!({
                    "kind": "schema_change",
                    "schema_change": { "change": c.change.name(), "target": c.target.name(), "keyspace": c.keyspace, "name": c.name, "arg_types": c.arg_types },
                }),
                eng::ResponseBody::Rows(rows) => nosql_server::http::rows_json(rows, d),
            };
            if let Some(o) = out.as_object_mut() {
                o.insert("warnings".into(), json!(r.warnings));
                o.insert("tracing_id".into(), json!(r.tracing_id.map(|t| nosql_types::timeuuid::format_uuid(&t))));
            }
            out
        }
        Err(e) => {
            let code = e.code.code();
            json!({ "error": e.message, "code": code, "code_name": nosql_proto::code::name(code), "warnings": e.warnings })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_names() {
        let id = new_db_id();
        assert_eq!(id.len(), 12);
        assert!(id.starts_with('n'));
        assert!(id.bytes().all(|b| ID_ALPHABET.contains(&b) || b == b'n'));
        assert_ne!(new_db_id(), new_db_id());
        assert_eq!(new_password().len(), 32);
        assert!(valid_name("pr-482"));
        assert!(!valid_name("PR"));
        assert!(!valid_name(""));
        assert!(!valid_name(&"a".repeat(41)));
        assert!(!valid_name("a_b"));
    }

    #[test]
    fn masking_policies() {
        let ok = |v: Value| check_masking(v.as_object().unwrap()).is_ok();
        assert!(ok(json!({"ks.t.email": "mask_inner(1, 1)", "ks.t.ssn": "mask_null()"})));
        assert!(!ok(json!({"ks.t": "mask_null()"})));
        assert!(!ok(json!({"ks..c": "mask_null()"})));
        assert!(!ok(json!({"ks.t.c": "upper(c)"})));
        assert!(!ok(json!({"ks.t.c": 5})));
        assert!(!ok(json!({"ks.t.c": "mask_null("})));
        assert!(!ok(json!({"ks.t.c": "mask_inner(x, 1)"})));
        assert!(ok(json!({"ks.t.c": "DEFAULT", "ks.t.d": "system.mask_hash('MD5')"})));
        let p = masking_policy(json!({"ks.t.email": "mask_inner(1, 1)"}).as_object().unwrap()).unwrap();
        let call = &p[&("ks".to_string(), "t".to_string(), "email".to_string())];
        assert_eq!((call.function.as_str(), call.args.len()), ("mask_inner", 2));
    }

    #[test]
    fn specs() {
        let cfg = NosqlConfig::local(std::env::temp_dir());
        let mut d = crate::store::tests::sample_db("nabc", "app");
        d.durability = "sync".into();
        d.masking = json!({"ks.t.c": "mask_null()"}).as_object().unwrap().clone();
        let s = spec_for(&cfg, &d).unwrap();
        assert_eq!(s.config.masking.len(), 1);
        assert_eq!(s.config.settings.authenticator, "PasswordAuthenticator");
        assert_eq!(s.config.settings.tombstone_warn_threshold, 10);
        assert_eq!(s.config.datacenter, "us-east-1");
        assert_eq!(s.config.native_port, 59042);
        assert!(s.platform_login("cassandra", b"pw"));
        assert_eq!(s.durability, Durability::Sync);
        assert_eq!(s.config.superuser.as_ref().unwrap().name, "cassandra");
        d.dialect = "mongo".into();
        assert!(matches!(spec_for(&cfg, &d), Err(ControlError::Invalid(_))));
    }
}
