//! The subset of ScyllaDB's REST API (port 10000) that ScyllaDB's `nodetool` and the test
//! suites call: node identity and status, the ring, keyspaces and tables, flush, compaction,
//! cleanup, snapshots, table statistics and histograms, the database's tracing settings (trace
//! probability and the slow query log, applied by the engine), and settings that are accepted and
//! reported (throughputs, auto compaction).
//!
//! Response shapes were captured from `scylladb/scylla:2026.3.3`: JSON scalars for single
//! values, `[{"key": …, "value": …}]` for maps, `{"message": …, "code": …}` errors (400 for a
//! bad keyspace or table, 404 for an unknown path, 500 for what ScyllaDB reports as a
//! `std::runtime_error`), and an empty `application/json` body for successful POSTs.
//!
//! The database is the one the `Host` header names (`<db>.<domain>`, like SNI), or the
//! directory's default. The presented node is the database's single virtual node.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use nosql_engine as eng;
use nosql_storage::table::TableStats;
use serde_json::{Value as Json, json};
use tokio::net::TcpListener;

use crate::directory::{Directory, Lease};

/// What `release_version` reports per dialect, and ScyllaDB's own version.
pub const SCYLLA_RELEASE_VERSION: &str = "2026.3.3-0.20261004.d80b3d6ca26b";

#[derive(Clone)]
pub struct RestState {
    pub dir: Arc<dyn Directory>,
    /// `compaction_throughput`, `stream_throughput`.
    settings: Arc<Settings>,
}

#[derive(Default)]
struct Settings {
    compaction_throughput: AtomicU64,
    stream_throughput: AtomicU64,
    started_ms: AtomicU64,
}

pub fn router(dir: Arc<dyn Directory>) -> Router {
    let settings = Arc::new(Settings::default());
    settings.started_ms.store(crate::directory::now_ms(), Ordering::Relaxed);
    Router::new().fallback(handle).with_state(RestState { dir, settings })
}

pub async fn serve(listener: TcpListener, dir: Arc<dyn Directory>) -> anyhow::Result<()> {
    axum::serve(listener, router(dir)).await?;
    Ok(())
}

/// Start the API on an ephemeral port (tests, embedding).
pub async fn start(dir: Arc<dyn Directory>) -> anyhow::Result<std::net::SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move { serve(listener, dir).await });
    Ok(addr)
}

/// An error as ScyllaDB's httpd sends it.
pub struct RestError(StatusCode, String);

impl RestError {
    fn bad(msg: impl Into<String>) -> Self {
        RestError(StatusCode::BAD_REQUEST, msg.into())
    }

    /// What ScyllaDB wraps as `std::runtime_error (…)`.
    fn runtime(msg: impl std::fmt::Display) -> Self {
        RestError(StatusCode::INTERNAL_SERVER_ERROR, format!("std::runtime_error ({msg})"))
    }

    /// A C++ exception ScyllaDB's httpd reports as a 500 (`std::invalid_argument (stol)`).
    fn runtime_named(class: &str, msg: &str) -> Self {
        RestError(StatusCode::INTERNAL_SERVER_ERROR, format!("{class} ({msg})"))
    }

    fn not_found() -> Self {
        RestError(StatusCode::NOT_FOUND, "Not found".into())
    }

    fn unsupported() -> Self {
        RestError(StatusCode::INTERNAL_SERVER_ERROR, "API call is not supported yet".into())
    }
}

impl IntoResponse for RestError {
    fn into_response(self) -> Response {
        let body = json!({ "message": self.1, "code": self.0.as_u16() });
        (self.0, [(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
    }
}

fn ok(v: Json) -> Response {
    (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response()
}

/// A successful POST/DELETE: an empty JSON body.
fn done() -> Response {
    (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], "").into_response()
}

type Params = BTreeMap<String, String>;

async fn handle(State(st): State<RestState>, method: Method, uri: Uri, headers: HeaderMap, Query(q): Query<Params>) -> Response {
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).map(|h| h.rsplit_once(':').map_or(h, |(a, _)| a).to_string());
    let Some(spec) = st.dir.route_sni(host.as_deref()).await else { return RestError::not_found().into_response() };
    let lease = match st.dir.open(&spec).await {
        Ok(l) => l,
        Err(e) => return RestError(StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    crate::directory::Metrics::add(&lease.metrics.requests, 1);
    let segs: Vec<String> = uri.path().split('/').filter(|s| !s.is_empty()).map(percent_decode).collect();
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    match route(&st, &lease, &method, &segs, &q).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = (b[i + 1] as char).to_digit(16);
            let l = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (h, l) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

struct Node<'a> {
    db: &'a Arc<eng::Database>,
    ip: IpAddr,
}

impl Node<'_> {
    fn ip(&self) -> String {
        self.ip.to_string()
    }

    fn host_id(&self) -> String {
        nosql_types::timeuuid::format_uuid(&self.db.config().host_id())
    }

    fn tokens(&self) -> Vec<i64> {
        let c = self.db.config();
        let mut t = eng::system::local::tokens(&c.host_id(), c.num_tokens);
        t.sort_unstable();
        t
    }

    fn schema(&self) -> Arc<eng::schema::Schema> {
        self.db.schema()
    }

    fn keyspace(&self, name: &str) -> Result<Arc<eng::schema::Keyspace>, RestError> {
        self.schema().keyspace(name).cloned().ok_or_else(|| RestError::bad(format!("Can't find a keyspace {name}")))
    }

    /// `ks:cf`.
    fn table(&self, name: &str) -> Result<Arc<eng::schema::Table>, RestError> {
        let Some((ks, cf)) = name.split_once(':') else {
            return Err(RestError::bad("Column family name should be in keyspace:column_family format"));
        };
        self.schema().table(ks, cf).cloned().ok_or_else(|| RestError::bad(format!("Can't find a column family {cf} in keyspace {ks}")))
    }

    /// The tables of a keyspace, or just `cf` (comma-separated) of it.
    fn tables_of(&self, ks: &str, cf: Option<&String>) -> Result<Vec<Arc<eng::schema::Table>>, RestError> {
        let k = self.keyspace(ks)?;
        match cf.filter(|c| !c.is_empty()) {
            None => Ok(k.tables.values().cloned().collect()),
            Some(list) => list
                .split(',')
                .map(|c| k.tables.get(c.trim()).cloned().ok_or_else(|| RestError::bad(format!("Can't find a column family {} in keyspace {ks}", c.trim()))))
                .collect(),
        }
    }

    fn stats(&self, t: &eng::schema::Table) -> TableStats {
        self.db.store().get_table(t.table_id()).map(|s| s.stats()).unwrap_or_default()
    }

    fn all_tables(&self) -> Vec<Arc<eng::schema::Table>> {
        self.schema().keyspaces.values().flat_map(|k| k.tables.values().cloned().collect::<Vec<_>>()).collect()
    }

    fn load(&self) -> u64 {
        self.db.store().stats().values().map(|s| s.space_used + s.memtable_bytes).sum()
    }

    fn uptime_ms(&self, st: &RestState) -> u64 {
        crate::directory::now_ms().saturating_sub(st.settings.started_ms.load(Ordering::Relaxed))
    }
}

/// C++'s `std::stol` as ScyllaDB's API parses numbers: leading whitespace, a sign, then the
/// longest run of digits (`12abc` is 12); no digits fails.
fn stol(v: &str) -> Result<i64, RestError> {
    let t = v.trim_start();
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
    if end == 0 {
        return Err(RestError::runtime_named("std::invalid_argument", "stol"));
    }
    let n: i64 = digits[..end].parse().map_err(|_| RestError::runtime_named("std::out_of_range", "stol"))?;
    Ok(if neg { -n } else { n })
}

fn kv(k: impl Into<Json>, v: impl Into<Json>) -> Json {
    json!({ "key": k.into(), "value": v.into() })
}

async fn route(st: &RestState, lease: &Lease, method: &Method, p: &[&str], q: &Params) -> Result<Response, RestError> {
    let db = lease.db();
    let n = Node { db, ip: db.config().address };
    let get = *method == Method::GET;
    let post = *method == Method::POST;
    let delete = *method == Method::DELETE;
    Ok(match p {
        // storage_service: identity and state.
        ["storage_service", "host_id"] if get => ok(json!([kv(n.ip(), n.host_id())])),
        ["storage_service", "hostid", "local"] if get => ok(json!(n.host_id())),
        ["storage_service", "release_version"] if get => ok(json!(release_version(db.dialect()))),
        ["storage_service", "scylla_release_version"] if get => ok(json!(SCYLLA_RELEASE_VERSION)),
        ["storage_service", "schema_version"] if get => ok(json!(nosql_types::timeuuid::format_uuid(&n.schema().version))),
        ["storage_service", "cluster_name"] if get => ok(json!(db.config().cluster_name)),
        ["storage_service", "partitioner_name"] if get => ok(json!("org.apache.cassandra.dht.Murmur3Partitioner")),
        ["storage_service", "operation_mode"] if get => ok(json!("NORMAL")),
        ["storage_service", "is_starting"] if get => ok(json!(false)),
        ["storage_service", "is_initialized" | "gossiping" | "join_ring"] if get => ok(json!(true)),
        // `nodetool statusbinary / enablebinary / disablebinary`: CQL service of the database.
        ["storage_service", "native_transport"] if get => ok(json!(db.native_transport())),
        ["storage_service", "native_transport"] if post || delete => {
            db.set_native_transport(post);
            done()
        }
        ["storage_service", "rpc_server" | "incremental_backups"] if get => ok(json!(false)),
        ["storage_service", "nodes", "leaving" | "joining" | "moving" | "excluded"] if get => ok(json!([])),
        ["storage_service", "removal_status"] if get => ok(json!("No token removals in process.")),
        ["storage_service", "drain"] if get => ok(json!("Drained 0/0 ColumnFamilies")),
        ["storage_service", "load" | "metrics" | "load_map", ..] if get && (p.len() == 2 || p.get(2) == Some(&"load")) => {
            if p.get(1) == Some(&"load_map") { ok(json!([kv(n.ip(), n.load())])) } else { ok(json!(n.load())) }
        }
        ["storage_service", "metrics", "exceptions" | "hints_in_progress" | "total_hints"] if get => ok(json!(0)),
        ["storage_service", "generation_number"] if get => ok(json!(st.settings.started_ms.load(Ordering::Relaxed) / 1000)),
        ["storage_service", "data_file", "locations"] if get => ok(json!(["/var/lib/scylla/data"])),
        ["storage_service", "commitlog"] if get => ok(json!("/var/lib/scylla/commitlog")),
        ["storage_service", "slow_query"] if get => {
            let s = db.slow_query_log();
            ok(json!({ "enable": s.enable, "ttl": s.ttl, "threshold": s.threshold, "fast": s.fast }))
        }
        ["storage_service", "slow_query"] if post => {
            // Each given parameter changes its setting (`stol` for numbers, `true` for flags).
            let mut s = db.slow_query_log();
            let given = |k: &str| q.get(k).map(String::as_str).filter(|v| !v.is_empty());
            if let Some(v) = given("enable") {
                s.enable = v == "true";
            }
            if let Some(v) = given("fast") {
                s.fast = v == "true";
            }
            if let Some(v) = given("ttl") {
                s.ttl = stol(v)?.clamp(0, u32::MAX as i64) as u32;
            }
            if let Some(v) = given("threshold") {
                s.threshold = stol(v)?.max(0) as u64;
            }
            db.set_slow_query_log(s);
            done()
        }
        ["storage_service", "tokens"] if get => ok(json!(n.tokens().iter().map(|t| t.to_string()).collect::<Vec<_>>())),
        ["storage_service", "tokens", ep] if get => {
            if *ep == n.ip() { ok(json!(n.tokens().iter().map(|t| t.to_string()).collect::<Vec<_>>())) } else { ok(json!([])) }
        }
        ["storage_service", "tokens_endpoint"] if get => ok(Json::Array(n.tokens().iter().map(|t| kv(t.to_string(), n.ip())).collect())),
        ["storage_service", "keyspaces"] if get => {
            let ty = q.get("type").map(String::as_str).unwrap_or("all");
            let schema = n.schema();
            let names: Vec<&String> = schema
                .keyspaces
                .values()
                .filter(|k| k.kind != eng::schema::KeyspaceKind::Virtual)
                .filter(|k| match ty {
                    "user" => !k.is_builtin(),
                    "non_local_strategy" => !k.replication.class.ends_with("LocalStrategy"),
                    _ => true,
                })
                .map(|k| &k.name)
                .collect::<Vec<_>>();
            ok(json!(names))
        }
        ["storage_service", "describe_ring", ks] if get => {
            n.keyspace(ks)?;
            ok(json!(ring(&n)))
        }
        ["storage_service", "ownership"] if get => ok(json!([kv(n.ip(), "1")])),
        ["storage_service", "ownership", ks] if get => {
            let k = n.keyspace(ks)?;
            if k.replication.class.ends_with("LocalStrategy") {
                return Err(RestError::runtime("Ownership values for keyspaces with LocalStrategy are meaningless"));
            }
            ok(json!([kv(n.ip(), "1")]))
        }
        ["storage_service", "natural_endpoints", ks] if get => {
            n.keyspace(ks)?;
            ok(json!([n.ip()]))
        }
        ["storage_service", "range_to_endpoint_map", ks] if get => {
            n.keyspace(ks)?;
            ok(Json::Array(ring(&n).iter().map(|r| kv(json!([r["start_token"], r["end_token"]]), json!([n.ip()]))).collect()))
        }
        ["storage_service", "view_build_statuses", ks, _view] if get => {
            n.keyspace(ks)?;
            ok(json!([kv(n.ip(), "SUCCESS")]))
        }
        // Flush, compaction, cleanup.
        ["storage_service", "flush"] if post => {
            db.flush().await.map_err(RestError::runtime)?;
            done()
        }
        ["storage_service", "keyspace_flush", ks] if post => {
            let tables = n.tables_of(ks, q.get("cf"))?;
            for t in tables {
                if let Some(s) = db.store().get_table(t.table_id()) {
                    s.flush().await.map_err(RestError::runtime)?;
                }
            }
            db.store().write_manifest(false).await.map_err(RestError::runtime)?;
            done()
        }
        // `flush_memtables=false` compacts the SSTables alone (the memtables' live data still
        // holds back purging the tombstones that could shadow it).
        ["storage_service", "compact"] if post => {
            let flush = q.get("flush_memtables").is_none_or(|v| v.as_str() != "false");
            for t in n.all_tables() {
                compact(&n, &t, flush).await?;
            }
            done()
        }
        ["storage_service", "keyspace_compaction", ks] if post => {
            let flush = q.get("flush_memtables").is_none_or(|v| v.as_str() != "false");
            for t in n.tables_of(ks, q.get("cf"))? {
                compact(&n, &t, flush).await?;
            }
            done()
        }
        ["storage_service", "keyspace_cleanup" | "keyspace_offstrategy_compaction", ks] if post => {
            n.tables_of(ks, q.get("cf"))?;
            ok(json!(0))
        }
        ["storage_service", "cleanup_all"] if post => ok(json!(0)),
        ["storage_service", "keyspace_scrub" | "keyspace_upgrade_sstables", ks] if get => {
            n.tables_of(ks, q.get("cf"))?;
            ok(json!(0))
        }
        // POST enables, DELETE disables background compaction of the tables.
        ["storage_service", "auto_compaction", ks] if post || delete => {
            for t in n.tables_of(ks, q.get("cf"))? {
                if let Some(s) = db.store().get_table(t.table_id()) {
                    s.set_auto_compaction(post);
                }
            }
            done()
        }
        ["storage_service", "tombstone_gc", ks] if post || delete => {
            n.tables_of(ks, q.get("cf"))?;
            done()
        }
        // Snapshots.
        ["storage_service", "snapshots"] if get => ok(Json::Array(snapshots(lease).await?)),
        ["storage_service", "snapshots"] if post => {
            let tag = q.get("tag").filter(|t| !t.is_empty()).ok_or_else(|| RestError::runtime("You must supply a snapshot name."))?;
            let keyspaces: Vec<String> = match q.get("kn").filter(|k| !k.is_empty()) {
                Some(list) => list.split(',').map(|s| s.trim().to_string()).collect(),
                None => n.schema().keyspaces.values().filter(|k| !k.is_builtin()).map(|k| k.name.clone()).collect(),
            };
            let mut entries = Vec::new();
            for ks in &keyspaces {
                for t in n.tables_of(ks, q.get("cf"))? {
                    entries.push(json!({ "ks": t.keyspace, "cf": t.name }));
                }
            }
            if snapshot_tags(lease).await?.iter().any(|(t, _)| t == tag) {
                let ks = keyspaces.first().cloned().unwrap_or_default();
                return Err(RestError::runtime(format!("Keyspace {ks}: snapshot {tag} already exists.")));
            }
            db.store().snapshot(tag).await.map_err(RestError::runtime)?;
            // The listing lives beside the snapshot, so it outlives an unload.
            let info = serde_json::to_vec(&entries).map_err(RestError::runtime)?;
            db.store().put_snapshot_info(tag, info.into()).await.map_err(RestError::runtime)?;
            done()
        }
        ["storage_service", "snapshots"] if delete => {
            let tags: Vec<String> = match q.get("tag").filter(|t| !t.is_empty()) {
                Some(t) => t.split(',').map(str::to_string).collect(),
                None => snapshot_tags(lease).await?.into_iter().map(|(t, _)| t).collect(),
            };
            for t in tags {
                db.store().clear_snapshot(&t).await.map_err(RestError::runtime)?;
            }
            done()
        }
        ["storage_service", "snapshots", "size", "true"] if get => ok(json!(0)),
        // Settings that are accepted and reported.
        // A whole probability reads `0` / `1`, as ScyllaDB prints its double.
        ["storage_service", "trace_probability"] if get => {
            let p = db.trace_probability();
            if p.fract() == 0.0 { ok(json!(p as i64)) } else { ok(json!(p)) }
        }
        ["storage_service", "trace_probability"] if post => {
            let v = q.get("probability").ok_or_else(|| RestError::bad("Missing mandatory parameter 'probability'"))?;
            let p: f64 = v.parse().map_err(|_| RestError::bad(format!("Bad format in a probability value: \"{v}\"")))?;
            db.set_trace_probability(p).map_err(|_| RestError::bad("trace probability must be in a [0,1] range"))?;
            done()
        }
        ["storage_service", which @ ("compaction_throughput" | "stream_throughput")] => {
            let cell = if *which == "compaction_throughput" { &st.settings.compaction_throughput } else { &st.settings.stream_throughput };
            if get {
                ok(json!(cell.load(Ordering::Relaxed)))
            } else if post {
                let v = q.get("value").ok_or_else(|| RestError::bad("Missing mandatory parameter 'value'"))?;
                cell.store(v.parse().map_err(|_| RestError::bad(format!("Bad format in a value: \"{v}\"")))?, Ordering::Relaxed);
                done()
            } else {
                return Err(RestError::not_found());
            }
        }
        ["storage_service", "tombstone_warn_threshold" | "tombstone_failure_threshold" | "batch_size_failure_threshold"] => {
            return Err(RestError::unsupported());
        }
        // Gossip, failure detector, snitch, system.
        ["gossiper", "endpoint", "live"] if get => ok(json!([n.ip()])),
        ["gossiper", "endpoint", "down"] if get => ok(json!([])),
        ["gossiper", "downtime", _] if get => ok(json!(0)),
        ["gossiper", "generation_number", _] if get => ok(json!(st.settings.started_ms.load(Ordering::Relaxed) / 1000)),
        ["failure_detector", "simple_states"] if get => ok(json!([kv(n.ip(), "UP")])),
        ["failure_detector", "count", "endpoint", "up"] if get => ok(json!(1)),
        ["failure_detector", "count", "endpoint", "down"] if get => ok(json!(0)),
        ["failure_detector", "phi"] if get => ok(json!("8")),
        ["failure_detector", "endpoints"] if get => {
            let c = db.config();
            let generation = st.settings.started_ms.load(Ordering::Relaxed) / 1000;
            ok(json!([{
                "addrs": n.ip(),
                "generation": generation,
                "version": 1,
                "update_time": st.settings.started_ms.load(Ordering::Relaxed),
                "is_alive": true,
                "application_state": [
                    { "application_state": 0, "value": "NORMAL", "version": 1 },
                    { "application_state": 3, "value": c.datacenter, "version": 1 },
                    { "application_state": 4, "value": c.rack, "version": 1 },
                    { "application_state": 14, "value": n.host_id(), "version": 1 },
                ],
            }]))
        }
        ["snitch", "datacenter"] if get => ok(json!(db.config().datacenter)),
        ["snitch", "rack"] if get => ok(json!(db.config().rack)),
        ["snitch", "name"] if get => ok(json!("org.apache.cassandra.locator.SimpleSnitch")),
        ["system", "uptime_ms"] if get => ok(json!(n.uptime_ms(st))),
        ["system", "drop_sstable_caches"] if post => {
            db.drop_row_cache();
            done()
        }
        ["system", "highest_supported_sstable_version" | "chosen_sstable_version"] if get => ok(json!("me")),
        // Compaction manager.
        ["compaction_manager", "compactions" | "compaction_history" | "compaction_info"] if get => ok(json!([])),
        ["compaction_manager", "metrics", "pending_tasks"] if get => {
            ok(json!(pending_compactions(&n).iter().map(|(_, n)| n).sum::<u64>()))
        }
        ["compaction_manager", "metrics", "pending_tasks_by_table"] if get => ok(Json::Array(
            pending_compactions(&n)
                .into_iter()
                .map(|(t, task)| json!({ "ks": t.keyspace, "cf": t.name, "task": task }))
                .collect(),
        )),
        ["compaction_manager", "metrics", "completed_tasks" | "total_compactions_completed"] if get => {
            ok(json!(n.all_tables().iter().map(|t| n.stats(t).compactions).sum::<u64>()))
        }
        ["compaction_manager", "metrics", "bytes_compacted"] if get => ok(json!(0)),
        ["compaction_manager", "stop_compaction" | "stop_keyspace_compaction", ..] if post => done(),
        // Column families.
        ["column_family"] if get => ok(Json::Array(
            n.all_tables().iter().map(|t| json!({ "ks": t.keyspace, "cf": t.name, "type": "ColumnFamilies" })).collect(),
        )),
        ["column_family", "name"] if get => ok(json!(n.all_tables().iter().map(|t| format!("{}:{}", t.keyspace, t.name)).collect::<Vec<_>>())),
        ["column_family", "name", "keyspace"] if get => {
            let schema = n.schema();
            ok(json!(schema.keyspaces.values().filter(|k| k.kind != eng::schema::KeyspaceKind::Virtual).map(|k| &k.name).collect::<Vec<_>>()))
        }
        ["column_family", "major_compaction" | "compaction", name] if post => {
            let t = n.table(name)?;
            let flush = q.get("flush_memtables").is_none_or(|v| v.as_str() != "false");
            compact(&n, &t, flush).await?;
            done()
        }
        ["column_family", "autocompaction" | "tombstone_gc", name] => {
            n.table(name)?;
            if get { ok(json!(true)) } else { done() }
        }
        ["column_family", "compaction_strategy", name] if get => {
            let t = n.table(name)?;
            let class = t.params.compaction.get("class").cloned().unwrap_or_default();
            ok(json!(class.rsplit('.').next().unwrap_or(&class)))
        }
        ["column_family", "compression_parameters", name] if get => {
            n.table(name)?;
            ok(json!([]))
        }
        ["column_family", "built_indexes", name] if get => {
            let t = n.table(name)?;
            ok(json!(t.indexes.iter().map(|i| &i.name).collect::<Vec<_>>()))
        }
        ["column_family", "sstables", "per_level", name] if get => {
            let t = n.table(name)?;
            let s = n.stats(&t);
            let levels = if !s.sstables_per_level.is_empty() {
                s.sstables_per_level.clone()
            } else if s.sstables > 0 {
                vec![s.sstables]
            } else {
                vec![]
            };
            ok(json!(levels))
        }
        ["column_family", "sstables", "unleveled", name] if get => {
            n.table(name)?;
            ok(json!([]))
        }
        ["column_family", "metrics", rest @ ..] if get => column_family_metric(&n, rest)?,
        _ => return Err(RestError::not_found()),
    })
}

fn release_version(d: nosql_types::Dialect) -> &'static str {
    match d {
        nosql_types::Dialect::Scylla => "3.0.8",
        nosql_types::Dialect::Cassandra => "5.0.9",
    }
}

async fn compact(n: &Node<'_>, t: &eng::schema::Table, flush: bool) -> Result<(), RestError> {
    if let Some(s) = n.db.store().get_table(t.table_id()) {
        if flush {
            s.flush().await.map_err(RestError::runtime)?;
        }
        s.compact_major().await.map_err(RestError::runtime)?;
    }
    Ok(())
}

/// Tables with background compactions waiting, and how many.
fn pending_compactions(n: &Node<'_>) -> Vec<(std::sync::Arc<eng::schema::Table>, u64)> {
    n.all_tables()
        .into_iter()
        .filter_map(|t| {
            let pending = n.db.store().get_table(t.table_id())?.pending_compactions();
            (pending > 0).then_some((t, pending))
        })
        .collect()
}

/// The ring of the single virtual node: one range per token, the wrapping range first.
fn ring(n: &Node<'_>) -> Vec<Json> {
    let tokens = n.tokens();
    let c = n.db.config();
    let range = |start: i64, end: i64| {
        json!({
            "start_token": start.to_string(),
            "end_token": end.to_string(),
            "endpoints": [n.ip()],
            "rpc_endpoints": [n.ip()],
            "endpoint_details": [{ "host": n.ip(), "datacenter": c.datacenter, "rack": c.rack }],
        })
    };
    let (Some(first), Some(last)) = (tokens.first(), tokens.last()) else { return vec![] };
    let mut out = vec![range(*last, *first)];
    for w in tokens.windows(2).rev() {
        out.push(range(w[0], w[1]));
    }
    out
}

/// Snapshots taken through the REST API: tag → `{ks, cf}` entries.
async fn snapshot_tags(lease: &Lease) -> Result<Vec<(String, Vec<Json>)>, RestError> {
    let infos = lease.db().store().snapshot_infos().await.map_err(RestError::runtime)?;
    Ok(infos.into_iter().map(|(tag, raw)| (tag, serde_json::from_slice(&raw).unwrap_or_default())).collect())
}

async fn snapshots(lease: &Lease) -> Result<Vec<Json>, RestError> {
    Ok(snapshot_tags(lease)
        .await?
        .into_iter()
        .map(|(tag, entries)| {
            let value: Vec<Json> = entries
                .into_iter()
                .map(|mut e| {
                    if let Some(o) = e.as_object_mut() {
                        o.insert("total".into(), json!(0));
                        o.insert("live".into(), json!(0));
                    }
                    e
                })
                .collect();
            kv(tag, value)
        })
        .collect())
}

/// Latency/size histograms ScyllaDB reports for a table without traffic.
fn empty_hist() -> Json {
    json!({ "count": 0, "sum": 0, "min": 0, "max": 0, "variance": 0, "mean": 0 })
}

/// Bucket offsets of an estimated histogram (Cassandra's `EstimatedHistogram.newOffsets`:
/// each 1.2 times the last, at least one more).
fn offsets(first: u64, n: usize) -> Vec<u64> {
    let mut v = Vec::with_capacity(n);
    let mut last = first;
    v.push(last);
    while v.len() < n {
        let next = ((last as f64) * 1.2).round() as u64;
        last = if next == last { last + 1 } else { next };
        v.push(last);
    }
    v
}

/// Latency bucket offsets: four per power of two from 512 µs (2^9 · {1, 1.25, 1.5, 1.75}).
fn latency_offsets() -> Vec<u64> {
    let mut v: Vec<u64> = (9..25).flat_map(|p| {
        let b = 1u64 << p;
        [b, b + b / 4, b + b / 2, b + 3 * b / 4]
    })
    .collect();
    v.push(1 << 25);
    v
}

fn column_family_metric(n: &Node<'_>, rest: &[&str]) -> Result<Response, RestError> {
    // `metrics/<name>[/<sub>]/<ks:cf>` or the node-wide total without the table.
    let (name, table) = match rest {
        [] => return Err(RestError::not_found()),
        [name] => (vec![*name], None),
        [a, b] if b.contains(':') || !is_sub(b) => {
            if is_sub(b) { (vec![*a, *b], None) } else { (vec![*a], Some(*b)) }
        }
        [a, b] => (vec![*a, *b], None),
        [a, b, t] => (vec![*a, *b], Some(*t)),
        _ => return Err(RestError::not_found()),
    };
    let tables = match table {
        Some(t) => vec![n.table(t)?],
        None => n.all_tables(),
    };
    let stats: Vec<TableStats> = tables.iter().map(|t| n.stats(t)).collect();
    let sum = |f: fn(&TableStats) -> u64| stats.iter().map(f).sum::<u64>();
    let max = |f: fn(&TableStats) -> u64| stats.iter().map(f).max().unwrap_or(0);
    let name: Vec<&str> = name;
    Ok(match name.as_slice() {
        ["memtable_columns_count"] => ok(json!(sum(|s| s.memtable_rows))),
        ["memtable_on_heap_size" | "all_memtables_on_heap_size"] => ok(json!(0)),
        ["memtable_off_heap_size" | "memtable_live_data_size" | "all_memtables_off_heap_size" | "all_memtables_live_data_size"] => {
            ok(json!(sum(|s| s.memtable_bytes)))
        }
        ["memtable_switch_count"] => ok(json!(sum(|s| s.flushes))),
        ["estimated_row_count"] => ok(json!(sum(|s| s.partitions_estimate))),
        ["bloom_filter_false_positives" | "recent_bloom_filter_false_positives"] => {
            ok(json!(sum(|s| s.bloom_filter_false_positives)))
        }
        ["pending_flushes" | "pending_compactions" | "speculative_retries" | "row_cache_hit" | "row_cache_miss" | "row_cache_hit_out_of_range"
        | "index_summary_off_heap_memory_used" | "compression_metadata_off_heap_memory_used" | "snapshots_size"
        | "true_snapshots_size" | "min_row_size" | "cas_prepare" | "cas_propose" | "cas_commit"] => ok(json!(0)),
        ["bloom_filter_false_ratio" | "recent_bloom_filter_false_ratio"] => ok(json!(0)),
        ["live_ss_table_count"] => ok(json!(sum(|s| s.sstables as u64))),
        ["live_disk_space_used" | "total_disk_space_used"] => ok(json!(sum(|s| s.space_used))),
        ["max_row_size"] => ok(json!(max(|s| s.max_partition_bytes))),
        ["mean_row_size"] => {
            let parts = sum(|s| s.partitions_estimate);
            ok(json!(sum(|s| s.data_bytes).checked_div(parts).unwrap_or(0)))
        }
        ["bloom_filter_disk_space_used" | "bloom_filter_off_heap_memory_used"] => ok(json!(sum(|s| s.bloom_filter_bytes))),
        ["compression_ratio"] => {
            let raw = sum(|s| s.uncompressed_bytes);
            ok(json!(if raw == 0 { 0.0 } else { sum(|s| s.data_bytes) as f64 / raw as f64 }))
        }
        ["read"] => ok(json!(sum(|s| s.reads))),
        ["write"] => ok(json!(sum(|s| s.writes))),
        ["read_latency" | "write_latency" | "range_latency"] => ok(json!(0)),
        ["read_latency" | "write_latency" | "range_latency", "histogram"] => ok(empty_hist()),
        ["read_latency" | "write_latency" | "range_latency", "moving_average_histogram"] => {
            ok(json!({ "meter": { "rates": [0, 0, 0], "mean_rate": 0, "count": 0 }, "hist": empty_hist() }))
        }
        ["read_latency" | "write_latency" | "range_latency" | "cas_prepare" | "cas_propose" | "cas_commit", "estimated_histogram"] => {
            ok(json!({ "buckets": vec![0; 65], "bucket_offsets": latency_offsets() }))
        }
        ["estimated_row_size_histogram" | "estimated_column_count_histogram"] => ok(json!({ "buckets": [0] })),
        ["sstables_per_read_histogram"] => ok(json!({ "buckets": vec![0; 36], "bucket_offsets": offsets(1, 35) })),
        ["tombstone_scanned_histogram" | "live_scanned_histogram" | "col_update_time_delta_histogram"] => ok(empty_hist()),
        ["key_cache_hit_rate"] => return Err(RestError::unsupported()),
        _ => return Err(RestError::not_found()),
    })
}

fn is_sub(s: &str) -> bool {
    matches!(s, "histogram" | "moving_average_histogram" | "estimated_histogram" | "estimated_recent_histogram")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_offsets_match_scylla() {
        // Captured: sstables_per_read_histogram and read_latency/estimated_histogram.
        assert_eq!(
            offsets(1, 35),
            vec![1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 17, 20, 24, 29, 35, 42, 50, 60, 72, 86, 103, 124, 149, 179, 215, 258, 310, 372, 446, 535, 642, 770, 924, 1109]
        );
        let o = latency_offsets();
        assert_eq!(o.len(), 65);
        assert_eq!(&o[..10], &[512, 640, 768, 896, 1024, 1280, 1536, 1792, 2048, 2560]);
        assert_eq!(&o[60..], &[16777216, 20971520, 25165824, 29360128, 33554432]);
    }

    #[test]
    fn decoding() {
        assert_eq!(percent_decode("ks%3At"), "ks:t");
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("%"), "%");
    }
}
