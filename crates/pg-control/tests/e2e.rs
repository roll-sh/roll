//! End-to-end tests for serverless Postgres: a real control plane, proxy and Postgres computes in
//! Docker, driven over the wire protocol with a real driver and over the HTTP API.
//!
//! They need Docker and the `postgres:{16,17,18}-alpine` images, so they only run with
//! `ROLL_E2E=1` (`scripts/test-postgres.sh` sets it). Optional:
//! - `ROLL_E2E_BIND=0.0.0.0` where containers can't reach the host's loopback (Linux)
//! - `ROLL_E2E_S3_BUCKET` (+ `ROLL_E2E_S3_ENDPOINT`, AWS env vars) to use S3 instead of a directory

use futures::{SinkExt, StreamExt};
use pg_control::compute::container_name;
use pg_control::history::Target;
use pg_control::store::{Branch, Endpoint, EndpointState, LocalState, Project};
use pg_control::{Config, Control, api, reconciler};
use pg_proxy::{Proxy, ProxyConfig};
use pg_storage::{Lsn, StorageConfig};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_postgres::error::SqlState;
use tokio_postgres::{Client, NoTls};

// ── harness ──

/// One isolated control plane (own home, state, object store, ports and containers).
struct Harness {
    control: Arc<Control>,
    proxy_port: u16,
    api: String,
    http: reqwest::Client,
    home: PathBuf,
}

/// A project with its main branch and primary endpoint.
struct Db {
    project: Project,
    branch: Branch,
    ep: Endpoint,
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn unique(prefix: &str) -> String {
    // Clocks can be coarse (µs on macOS) and tests start together, so add a counter.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros();
    format!("{prefix}-{}-{}-{n}", std::process::id(), t % 1_000_000_000_000)
}

macro_rules! harness {
    () => {
        match Harness::start().await {
            Some(h) => h,
            None => return,
        }
    };
}

impl Harness {
    async fn start() -> Option<Self> {
        if std::env::var("ROLL_E2E").is_err() {
            eprintln!("skipped: set ROLL_E2E=1 to run (needs Docker)");
            return None;
        }
        let _ = tracing_subscriber::fmt().with_env_filter("pg_control=info,pg_storage=info").with_test_writer().try_init();
        // Under target/ (not the system temp dir) so Docker Desktop can always bind-mount it.
        let home = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(unique("e2e"));
        let bind: IpAddr = std::env::var("ROLL_E2E_BIND").unwrap_or_else(|_| "127.0.0.1".into()).parse().unwrap();
        let api_listener = tokio::net::TcpListener::bind((bind, 0)).await.unwrap();
        let api_port = api_listener.local_addr().unwrap().port();
        let proxy_listen = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();

        let mut config = Config::local(home.clone());
        config.public_port = proxy_listen.port();
        config.internal_url = format!("http://host.docker.internal:{api_port}");
        if let Ok(bucket) = std::env::var("ROLL_E2E_S3_BUCKET") {
            config.storage = StorageConfig::S3 {
                bucket,
                region: std::env::var("AWS_REGION").ok(),
                endpoint: std::env::var("ROLL_E2E_S3_ENDPOINT").ok(),
                prefix: Some(format!("e2e/{}", home.file_name().unwrap().to_string_lossy())),
            };
        }
        let control = Control::new(config).await.expect("control plane");

        let router = api::router(control.clone());
        tokio::spawn(async move { axum::serve(api_listener, router).await });
        let proxy = Proxy::new(api::proxy_router(control.clone()), ProxyConfig { listen: proxy_listen, default_endpoint: None });
        tokio::spawn(async move { proxy.serve().await });

        Some(Self {
            control,
            proxy_port: proxy_listen.port(),
            api: format!("http://127.0.0.1:{api_port}"),
            http: reqwest::Client::new(),
            home,
        })
    }

    /// A new project, with its compute up and its first base image in object storage.
    async fn project(&self, pg_version: Option<u32>) -> Db {
        let project = self.control.create_project(&unique("db"), pg_version).await.unwrap();
        let branch = self.control.branch_in(&project.id, "main").unwrap();
        let ep = self.primary(&branch);
        self.control.ensure_running(&ep.id).await.unwrap();
        self.wait_for_base(&branch.id).await;
        Db { ep: self.control.endpoint_or_404(&ep.id).unwrap(), project, branch }
    }

    fn primary(&self, branch: &Branch) -> Endpoint {
        self.control.store.endpoint_for_branch(&branch.id).unwrap().expect("primary endpoint")
    }

    fn branch(&self, db: &Db, name: &str) -> Branch {
        self.control.branch_in(&db.project.id, name).unwrap()
    }

    async fn try_connect(&self, password: &str, endpoint: Option<&str>) -> Result<Client, tokio_postgres::Error> {
        let mut cfg = tokio_postgres::Config::new();
        cfg.host("127.0.0.1").port(self.proxy_port).user("roll").password(password).dbname("main");
        if let Some(ep) = endpoint {
            cfg.options(format!("endpoint={ep}"));
        }
        let (client, conn) = cfg.connect(NoTls).await?;
        tokio::spawn(conn);
        Ok(client)
    }

    /// Connect through the proxy, as any client would.
    async fn connect(&self, db: &Db, endpoint: &str) -> Client {
        self.try_connect(&db.project.password, Some(endpoint)).await.unwrap_or_else(|e| panic!("connect {endpoint}: {e:?}"))
    }

    async fn wait_for_base(&self, branch_id: &str) {
        wait_for("a base image", Duration::from_secs(120), || async {
            !self.control.store.bases(branch_id).unwrap().is_empty()
        })
        .await;
    }

    /// Bases on the branch's current timeline.
    async fn wait_for_base_on(&self, branch_id: &str, timeline: u32) {
        wait_for("a base image on the new timeline", Duration::from_secs(120), || async {
            self.control.store.bases(branch_id).unwrap().iter().any(|b| b.timeline == timeline)
        })
        .await;
    }

    /// Wait until the safekeeper has `lsn` durable in object storage.
    async fn wait_for_flush(&self, branch_id: &str, lsn: Lsn) {
        wait_for("WAL to reach the safekeeper", Duration::from_secs(60), || async {
            self.control.store.wal_position(branch_id).unwrap().is_some_and(|p| p.flush_lsn >= lsn.0)
        })
        .await;
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let r = self.http.get(format!("{}{path}", self.api)).send().await.unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let r = self.http.post(format!("{}{path}", self.api)).json(&body).send().await.unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }

    async fn docker_inspect(&self, endpoint_id: &str, format: &str) -> String {
        let out = tokio::process::Command::new("docker")
            .args(["inspect", "--format", format, &container_name(endpoint_id)])
            .output()
            .await
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let label = format!("label=roll.home={}", self.home.display());
        if let Ok(out) = std::process::Command::new("docker").args(["ps", "-aq", "--filter", &label]).output() {
            let ids: Vec<String> = String::from_utf8_lossy(&out.stdout).split_whitespace().map(String::from).collect();
            if !ids.is_empty() {
                let _ = std::process::Command::new("docker").args(["rm", "-f"]).args(&ids).output();
            }
        }
        if std::env::var("ROLL_E2E_KEEP").is_err() {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }
}

async fn wait_for<F, Fut>(what: &str, timeout: Duration, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = Instant::now();
    while !check().await {
        assert!(start.elapsed() < timeout, "timed out after {timeout:?} waiting for {what}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn count(c: &Client, sql: &str) -> i64 {
    c.query_one(sql, &[]).await.unwrap_or_else(|e| panic!("{sql}: {e:?}")).get(0)
}

async fn current_lsn(c: &Client) -> Lsn {
    let s: String = c.query_one("SELECT pg_current_wal_lsn()::text", &[]).await.unwrap().get(0);
    s.parse().unwrap()
}

async fn timeline(c: &Client) -> u32 {
    let f: String = c.query_one("SELECT pg_walfile_name(pg_current_wal_lsn())", &[]).await.unwrap().get(0);
    u32::from_str_radix(&f[..8], 16).unwrap()
}

/// Make the current WAL position durable everywhere (and switch segments, so it is uploaded whole).
async fn flush(h: &Harness, c: &Client, branch_id: &str) -> Lsn {
    let lsn = current_lsn(c).await;
    c.batch_execute("SELECT pg_switch_wal()").await.unwrap();
    h.wait_for_flush(branch_id, lsn).await;
    lsn
}

fn sql_state(e: &tokio_postgres::Error) -> Option<&SqlState> {
    e.as_db_error().map(|d| d.code())
}

async fn seed(c: &Client, rows: i64) {
    c.batch_execute(&format!(
        "CREATE TABLE IF NOT EXISTS t (id int PRIMARY KEY, v text);
         INSERT INTO t SELECT g, md5(g::text) FROM generate_series(1, {rows}) g ON CONFLICT DO NOTHING"
    ))
    .await
    .unwrap();
}

// ── proxy & wire protocol ──

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proxy_speaks_postgres() {
    let h = harness!();
    let db = h.project(None).await;
    let c = h.connect(&db, &db.ep.id).await;

    // Simple and extended protocol, typed parameters.
    let version: String = c.query_one("SHOW server_version_num", &[]).await.unwrap().get(0);
    assert!(version.starts_with("18"), "default is Postgres 18, got {version}");
    let stmt = c.prepare("SELECT $1::int + $2::int").await.unwrap();
    for i in 0..20 {
        let r: i32 = c.query_one(&stmt, &[&i, &1i32]).await.unwrap().get(0);
        assert_eq!(r, i + 1);
    }

    // Transactions: rollback and commit.
    c.batch_execute("CREATE TABLE acct (id int PRIMARY KEY, bal int)").await.unwrap();
    c.batch_execute("BEGIN; INSERT INTO acct VALUES (1, 100); ROLLBACK").await.unwrap();
    assert_eq!(count(&c, "SELECT count(*) FROM acct").await, 0);
    c.batch_execute("BEGIN; INSERT INTO acct VALUES (1, 100); COMMIT").await.unwrap();
    assert_eq!(count(&c, "SELECT count(*) FROM acct").await, 1);
    let err = c.batch_execute("INSERT INTO acct VALUES (1, 1)").await.unwrap_err();
    assert_eq!(sql_state(&err), Some(&SqlState::UNIQUE_VIOLATION));
    // The session survives errors.
    assert_eq!(count(&c, "SELECT 1::bigint").await, 1);

    // COPY in and out.
    c.batch_execute("CREATE TABLE bulk (id int, name text)").await.unwrap();
    let sink = c.copy_in("COPY bulk FROM STDIN").await.unwrap();
    futures::pin_mut!(sink);
    let data: String = (0..10_000).map(|i| format!("{i}\tname-{i}\n")).collect();
    sink.send(bytes::Bytes::from(data)).await.unwrap();
    assert_eq!(sink.finish().await.unwrap(), 10_000);
    let out = c.copy_out("COPY bulk TO STDOUT").await.unwrap();
    let chunks: Vec<_> = out.collect().await;
    let total: usize = chunks.into_iter().map(|b| b.unwrap().len()).sum();
    assert!(total > 100_000, "copy out returned {total} bytes");

    // A large result streams through.
    let rows = c.query("SELECT g, repeat('x', 100) FROM generate_series(1, 200000) g", &[]).await.unwrap();
    assert_eq!(rows.len(), 200_000);

    // Cancel requests reach the right backend.
    let token = c.cancel_token();
    let sleeper = tokio::spawn(async move { c.batch_execute("SELECT pg_sleep(30)").await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    token.cancel_query(NoTls).await.unwrap();
    let err = tokio::time::timeout(Duration::from_secs(10), sleeper).await.expect("cancel took effect").unwrap().unwrap_err();
    assert_eq!(sql_state(&err), Some(&SqlState::QUERY_CANCELED));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proxy_errors() {
    let h = harness!();
    let db = h.project(None).await;

    let err = h.try_connect(&db.project.password, Some("ep_does_not_exist")).await.unwrap_err();
    assert_eq!(sql_state(&err), Some(&SqlState::INVALID_CATALOG_NAME), "unknown endpoint: {err:?}");

    let err = h.try_connect(&db.project.password, None).await.unwrap_err();
    assert_eq!(sql_state(&err).map(|s| s.code()), Some("08004"), "no endpoint: {err:?}");

    let err = h.try_connect("wrong-password", Some(&db.ep.id)).await.unwrap_err();
    assert_eq!(sql_state(&err), Some(&SqlState::INVALID_PASSWORD), "bad password: {err:?}");

    // A bad attempt doesn't disturb good ones.
    let c = h.connect(&db, &db.ep.id).await;
    assert_eq!(count(&c, "SELECT 1::bigint").await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_concurrent_connections() {
    let h = harness!();
    let db = h.project(None).await;
    let h = Arc::new(h);
    let db = Arc::new(db);
    let tasks: Vec<_> = (0..50)
        .map(|i| {
            let (h, db) = (h.clone(), db.clone());
            tokio::spawn(async move {
                let c = h.connect(&db, &db.ep.id).await;
                let v: i32 = c.query_one("SELECT $1::int * 2", &[&i]).await.unwrap().get(0);
                assert_eq!(v, i * 2);
                c
            })
        })
        .collect();
    let mut clients = Vec::new();
    for t in tasks {
        clients.push(t.await.unwrap());
    }
    assert_eq!(h.control.connections(&db.ep.id), 50);
    drop(clients);
    wait_for("connections to close", Duration::from_secs(10), || async { h.control.connections(&db.ep.id) == 0 }).await;
}

// ── compute lifecycle ──

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scale_to_zero_and_wake_on_connect() {
    let h = harness!();
    let db = h.project(None).await;
    let c = h.connect(&db, &db.ep.id).await;
    seed(&c, 1000).await;
    h.control.update_endpoint(&db.ep.id, None, None, Some(1)).await.unwrap();

    // An open connection keeps it awake.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    reconciler::scale(&h.control, &mut HashMap::new()).await.unwrap();
    assert_eq!(h.control.endpoint_or_404(&db.ep.id).unwrap().state, EndpointState::Running);

    drop(c);
    wait_for("connections to close", Duration::from_secs(10), || async { h.control.connections(&db.ep.id) == 0 }).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    reconciler::scale(&h.control, &mut HashMap::new()).await.unwrap();
    assert_eq!(h.control.endpoint_or_404(&db.ep.id).unwrap().state, EndpointState::Suspended);
    assert_eq!(h.docker_inspect(&db.ep.id, "{{.State.Running}}").await, "false");

    // The next connection wakes it, with the data intact.
    let started = Instant::now();
    let c = h.connect(&db, &db.ep.id).await;
    assert_eq!(count(&c, "SELECT count(*) FROM t").await, 1000);
    eprintln!("cold start: {:?}", started.elapsed());
    assert_eq!(h.control.endpoint_or_404(&db.ep.id).unwrap().state, EndpointState::Running);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_resize_keeps_sessions() {
    let h = harness!();
    let db = h.project(None).await;
    let c = h.connect(&db, &db.ep.id).await;
    let pid: i32 = c.query_one("SELECT pg_backend_pid()", &[]).await.unwrap().get(0);
    c.batch_execute("BEGIN; CREATE TABLE r (x int); INSERT INTO r VALUES (1)").await.unwrap();

    let mb = h.control.config.mem_mb(1.0);
    h.control.resize(&db.ep, 1.0).await.unwrap();
    assert_eq!(h.docker_inspect(&db.ep.id, "{{.HostConfig.NanoCpus}}").await, "1000000000");
    assert_eq!(h.docker_inspect(&db.ep.id, "{{.HostConfig.Memory}}").await, (mb * 1024 * 1024).to_string());
    assert_eq!(h.control.endpoint_or_404(&db.ep.id).unwrap().cu, 1.0);

    // Same backend, same open transaction: no restart happened.
    let pid2: i32 = c.query_one("SELECT pg_backend_pid()", &[]).await.unwrap().get(0);
    assert_eq!(pid, pid2);
    c.batch_execute("COMMIT").await.unwrap();
    assert_eq!(count(&c, "SELECT count(*) FROM r").await, 1);

    // And back down.
    h.control.resize(&h.control.endpoint_or_404(&db.ep.id).unwrap(), 0.25).await.unwrap();
    assert_eq!(h.control.endpoint_or_404(&db.ep.id).unwrap().cu, 0.25);
    let pid3: i32 = c.query_one("SELECT pg_backend_pid()", &[]).await.unwrap().get(0);
    assert_eq!(pid, pid3);

    // Bounds are validated.
    assert!(h.control.update_endpoint(&db.ep.id, Some(4.0), Some(1.0), None).await.is_err());
}

// ── branching ──

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn branches_are_isolated_copies() {
    let h = harness!();
    let db = h.project(None).await;
    let c = h.connect(&db, &db.ep.id).await;
    seed(&c, 500).await;

    // From a running parent (online copy).
    let dev = h.control.create_branch(&db.project.id, "dev", None, None).await.unwrap();
    let dev_ep = h.primary(&dev);
    let d = h.connect(&db, &dev_ep.id).await;
    assert_eq!(count(&d, "SELECT count(*) FROM t").await, 500);

    // Writes on either side stay there.
    d.batch_execute("INSERT INTO t VALUES (100001, 'dev only')").await.unwrap();
    c.batch_execute("DELETE FROM t WHERE id <= 100").await.unwrap();
    assert_eq!(count(&d, "SELECT count(*) FROM t").await, 501);
    assert_eq!(count(&c, "SELECT count(*) FROM t").await, 400);

    // From a suspended parent (copy of its data directory), and a branch of a branch.
    drop(d);
    h.control.suspend(&dev_ep.id).await.unwrap();
    let feature = h.control.create_branch(&db.project.id, "feature/x", Some("dev"), None).await.unwrap();
    let f = h.connect(&db, &h.primary(&feature).id).await;
    assert_eq!(count(&f, "SELECT count(*) FROM t").await, 501);
    assert_eq!(feature.parent_id.as_deref(), Some(dev.id.as_str()));

    // Names are validated and unique.
    assert!(h.control.create_branch(&db.project.id, "dev", None, None).await.is_err());
    assert!(h.control.create_branch(&db.project.id, "bad name!", None, None).await.is_err());

    // Deleting removes the compute, the data and the history.
    drop(f);
    let feature_ep = h.primary(&feature);
    h.control.delete_branch(&db.project.id, &feature.id).await.unwrap();
    assert!(!h.control.config.data_path(feature.dir_name()).exists());
    assert!(h.control.objects.list(&format!("branches/{}/", feature.id)).await.unwrap().is_empty());
    let err = h.try_connect(&db.project.password, Some(&feature_ep.id)).await.unwrap_err();
    assert_eq!(sql_state(&err), Some(&SqlState::INVALID_CATALOG_NAME));
    assert!(h.control.delete_branch(&db.project.id, "main").await.is_err(), "root branch is protected");
}

// ── history: point-in-time restore, restore in place, eviction, host loss ──

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn point_in_time_branches() {
    let h = harness!();
    let db = h.project(None).await;
    let c = h.connect(&db, &db.ep.id).await;
    seed(&c, 1000).await;
    let before_lsn = flush(&h, &c, &db.branch.id).await;
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let before = unix_now();
    tokio::time::sleep(Duration::from_millis(2100)).await;

    c.batch_execute("DROP TABLE t").await.unwrap();
    flush(&h, &c, &db.branch.id).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;

    // By time.
    let at = h.control.create_branch(&db.project.id, "before-drop", None, Some(Target::Time(before))).await.unwrap();
    assert_eq!(at.parent_at, Some(before));
    let ac = h.connect(&db, &h.primary(&at).id).await;
    assert_eq!(count(&ac, "SELECT count(*) FROM t").await, 1000);
    // A PITR branch is a writable primary on its own timeline, with its own history.
    ac.batch_execute("INSERT INTO t VALUES (5000, 'after restore')").await.unwrap();
    assert!(timeline(&ac).await >= 2);
    h.wait_for_base(&at.id).await;

    // By LSN.
    let target = Target::Lsn { timeline: None, lsn: before_lsn };
    let by_lsn = h.control.create_branch(&db.project.id, "at-lsn", None, Some(target)).await.unwrap();
    let lc = h.connect(&db, &h.primary(&by_lsn).id).await;
    assert_eq!(count(&lc, "SELECT count(*) FROM t").await, 1000);

    // Latest: the drop is there.
    let latest = h.control.create_branch(&db.project.id, "latest", None, Some(Target::Latest)).await.unwrap();
    let lt = h.connect(&db, &h.primary(&latest).id).await;
    assert_eq!(count(&lt, "SELECT count(*) FROM pg_tables WHERE tablename = 't'").await, 0);

    // Outside the history window.
    let err = h.control.create_branch(&db.project.id, "too-old", None, Some(Target::Time(unix_now() - 86_400 * 30))).await;
    assert!(err.is_err(), "restoring before the first base must fail");

    let hist = h.control.history(&db.branch.id).await.unwrap();
    assert!(hist.start.is_some() && hist.end >= hist.start);
    assert!(!hist.bases.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_in_place_keeps_the_endpoint() {
    let h = harness!();
    let db = h.project(None).await;
    let c = h.connect(&db, &db.ep.id).await;
    seed(&c, 300).await;
    flush(&h, &c, &db.branch.id).await;
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let good = unix_now();
    tokio::time::sleep(Duration::from_millis(2100)).await;
    c.batch_execute("DELETE FROM t WHERE id > 100").await.unwrap();
    flush(&h, &c, &db.branch.id).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    drop(c);

    let backup = h.control.restore_branch(&db.project.id, &db.branch.id, Target::Time(good)).await.unwrap();
    assert!(backup.name.starts_with("main_old_"), "backup branch {}", backup.name);

    // Same endpoint, same connection string, earlier data.
    let c = h.connect(&db, &db.ep.id).await;
    assert_eq!(count(&c, "SELECT count(*) FROM t").await, 300);
    assert_eq!(timeline(&c).await, 2);
    let main = h.branch(&db, "main");
    assert!(!main.pending_recovery);
    assert_eq!(main.local_state, LocalState::Local);

    // The state before the restore lives on as a branch.
    let b = h.connect(&db, &h.primary(&backup).id).await;
    assert_eq!(count(&b, "SELECT count(*) FROM t").await, 100);

    // Writes after a restore are durable, and a second restore moves to the next timeline.
    h.wait_for_base_on(&db.branch.id, 2).await;
    c.batch_execute("INSERT INTO t VALUES (9001, 'new')").await.unwrap();
    let after_insert = flush(&h, &c, &db.branch.id).await;
    c.batch_execute("TRUNCATE t").await.unwrap();
    flush(&h, &c, &db.branch.id).await;
    drop(c);
    let target = Target::Lsn { timeline: Some(2), lsn: after_insert };
    h.control.restore_branch(&db.project.id, &db.branch.id, target).await.unwrap();
    let c = h.connect(&db, &db.ep.id).await;
    assert_eq!(count(&c, "SELECT count(*) FROM t").await, 301);
    assert_eq!(timeline(&c).await, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eviction_and_rehydrate() {
    let h = harness!();
    let db = h.project(None).await;
    let c = h.connect(&db, &db.ep.id).await;
    seed(&c, 2000).await;
    drop(c);

    // Running computes can't be evicted.
    assert!(h.control.evict_branch(&db.branch.id).await.is_err());

    h.control.suspend(&db.ep.id).await.unwrap();
    let dir = h.control.branch_path(&h.branch(&db, "main"));
    h.control.evict_branch(&db.branch.id).await.unwrap();
    let main = h.branch(&db, "main");
    assert_eq!(main.local_state, LocalState::Evicted);
    assert!(!dir.exists(), "local data is gone");
    assert!(h.control.store.bases(&db.branch.id).unwrap().iter().any(|b| b.kind == "snapshot"));
    h.control.evict_branch(&db.branch.id).await.unwrap(); // idempotent

    // Branching from an evicted parent restores from object storage.
    let child = h.control.create_branch(&db.project.id, "from-evicted", None, None).await.unwrap();
    let cc = h.connect(&db, &h.primary(&child).id).await;
    assert_eq!(count(&cc, "SELECT count(*) FROM t").await, 2000);

    // Connecting rehydrates, with no WAL replay needed.
    let c = h.connect(&db, &db.ep.id).await;
    assert_eq!(count(&c, "SELECT count(*) FROM t").await, 2000);
    let main = h.branch(&db, "main");
    assert_eq!(main.local_state, LocalState::Local);
    assert!(!main.pending_recovery);
    assert_eq!(timeline(&c).await, 1, "a cold snapshot restores without a new timeline");
    c.batch_execute("INSERT INTO t VALUES (99999, 'x')").await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn survives_losing_the_local_disk() {
    let h = harness!();
    let db = h.project(None).await;
    let c = h.connect(&db, &db.ep.id).await;
    // Written after the first base: only the WAL in object storage has it.
    seed(&c, 1500).await;
    c.batch_execute("UPDATE t SET v = 'changed' WHERE id <= 10").await.unwrap();
    drop(c);
    h.control.suspend(&db.ep.id).await.unwrap();

    let main = h.branch(&db, "main");
    std::fs::remove_dir_all(h.control.branch_path(&main)).unwrap();
    std::fs::remove_dir_all(h.control.config.spool_dir(&main.id)).ok();

    let c = h.connect(&db, &db.ep.id).await;
    assert_eq!(count(&c, "SELECT count(*) FROM t").await, 1500);
    assert_eq!(count(&c, "SELECT count(*) FROM t WHERE v = 'changed'").await, 10);
    c.batch_execute("INSERT INTO t VALUES (100000, 'still writable')").await.unwrap();
}

// ── read replicas ──

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_replicas() {
    let h = harness!();
    let db = h.project(None).await;
    let c = h.connect(&db, &db.ep.id).await;
    seed(&c, 700).await;
    flush(&h, &c, &db.branch.id).await;

    let replica = h.control.create_replica(&db.project.id, "main").await.unwrap();
    assert!(!replica.is_primary());
    let r = h.connect(&db, &replica.id).await;
    wait_for("the replica to catch up", Duration::from_secs(60), || async {
        r.query_one("SELECT count(*) FROM t", &[]).await.is_ok_and(|row| row.get::<_, i64>(0) == 700)
    })
    .await;
    let in_recovery: bool = r.query_one("SELECT pg_is_in_recovery()", &[]).await.unwrap().get(0);
    assert!(in_recovery);

    // It follows the primary.
    c.batch_execute("INSERT INTO t VALUES (800, 'live')").await.unwrap();
    wait_for("streaming replication", Duration::from_secs(30), || async {
        count(&r, "SELECT count(*) FROM t").await == 701
    })
    .await;

    // Read-only.
    let err = r.batch_execute("INSERT INTO t VALUES (801, 'nope')").await.unwrap_err();
    assert_eq!(sql_state(&err), Some(&SqlState::READ_ONLY_SQL_TRANSACTION));

    // It keeps serving while the primary sleeps.
    drop(c);
    h.control.suspend(&db.ep.id).await.unwrap();
    let r2 = h.connect(&db, &replica.id).await;
    assert_eq!(count(&r2, "SELECT count(*) FROM t").await, 701);
    assert_eq!(h.control.endpoint_or_404(&db.ep.id).unwrap().state, EndpointState::Suspended);

    // Primaries can't be deleted on their own; replicas can.
    assert!(h.control.delete_endpoint(&db.ep.id).await.is_err());
    drop((r, r2));
    let dir = h.control.replica_path(&h.control.endpoint_or_404(&replica.id).unwrap());
    h.control.delete_endpoint(&replica.id).await.unwrap();
    assert!(!dir.exists());
    let err = h.try_connect(&db.project.password, Some(&replica.id)).await.unwrap_err();
    assert_eq!(sql_state(&err), Some(&SqlState::INVALID_CATALOG_NAME));
}

// ── maintenance ──

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_removes_old_history() {
    let h = harness!();
    let db = h.project(None).await;
    let c = h.connect(&db, &db.ep.id).await;
    for round in 0..3 {
        seed(&c, 1000 * (round + 1)).await;
        flush(&h, &c, &db.branch.id).await;
        tokio::time::sleep(Duration::from_millis(1100)).await;
        h.control.take_base_backup(&db.branch.id).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let before = h.control.store.bases(&db.branch.id).unwrap();
    assert!(before.len() >= 4, "bases: {}", before.len());
    let bytes_before = h.control.object_bytes(&db.branch.id).await.unwrap();

    let deleted = h.control.gc_branch(&db.branch, 0).await.unwrap();
    assert!(deleted >= 3, "deleted {deleted}");
    let after = h.control.store.bases(&db.branch.id).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].id, before[0].id, "the newest base stays");
    assert!(h.control.object_bytes(&db.branch.id).await.unwrap() < bytes_before);

    // What's left still restores to the latest state.
    let b = h.control.create_branch(&db.project.id, "after-gc", None, Some(Target::Latest)).await.unwrap();
    let bc = h.connect(&db, &h.primary(&b).id).await;
    assert_eq!(count(&bc, "SELECT count(*) FROM t").await, 3000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usage_and_metering() {
    let h = harness!();
    let db = h.project(None).await;
    let c = h.connect(&db, &db.ep.id).await;
    seed(&c, 5000).await;
    flush(&h, &c, &db.branch.id).await;
    reconciler::meter_storage(&h.control).await.unwrap();

    let (status, usage) = h.get(&format!("/v1/projects/{}/usage", db.project.id)).await;
    assert_eq!(status, 200, "{usage}");
    assert!(usage["storage"]["local_bytes"].as_i64().unwrap() > 1_000_000);
    assert!(usage["storage"]["object_bytes"].as_i64().unwrap() > 100_000);
    assert!(usage["compute"]["cu_hours"].as_f64().unwrap() > 0.0);
    assert!(usage["always_on_month_usd"].as_f64().unwrap() > usage["projected_month_usd"].as_f64().unwrap());
    assert_eq!(usage["endpoints"].as_array().unwrap().len(), 1);

    let (_, project) = h.get(&format!("/v1/projects/{}", db.project.id)).await;
    let main = &project["branches"][0];
    assert!(main["storage_bytes"].as_i64().unwrap() > 0);
    assert!(main["object_bytes"].as_i64().unwrap() > 0);
    assert!(main["connection_uri"].as_str().unwrap().contains(&db.ep.id));
}

// ── HTTP API ──

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_api() {
    let h = harness!();
    let (status, health) = h.get("/v1/health").await;
    assert_eq!((status, health["ok"].as_bool()), (200, Some(true)));

    // Validation and missing resources.
    assert_eq!(h.post("/v1/projects", json!({ "name": "" })).await.0, 400);
    assert_eq!(h.post("/v1/projects", json!({ "name": "x", "pg_version": 9 })).await.0, 400);
    assert_eq!(h.get("/v1/projects/prj_nope").await.0, 404);
    assert_eq!(h.get("/v1/endpoints/ep_nope").await.0, 404);

    let (status, created) = h.post("/v1/projects", json!({ "name": "api" })).await;
    assert_eq!(status, 201, "{created}");
    let id = created["id"].as_str().unwrap().to_string();
    assert!(created["password"].as_str().is_some_and(|p| p.len() >= 16), "password is shown once");
    let ep = created["branches"][0]["endpoint"]["id"].as_str().unwrap().to_string();

    // SQL over HTTP wakes the compute.
    let (status, q) = h.post(&format!("/v1/endpoints/{ep}/query"), json!({ "sql": "SELECT 40 + 2 AS answer" })).await;
    assert_eq!(status, 200, "{q}");
    assert_eq!(q["results"][0]["rows"][0][0], json!("42"));

    let (status, b) = h.post(&format!("/v1/projects/{id}/branches"), json!({ "name": "dev" })).await;
    assert_eq!(status, 201, "{b}");
    assert_eq!(h.post(&format!("/v1/projects/{id}/branches"), json!({ "name": "dev" })).await.0, 409);
    assert_eq!(h.post(&format!("/v1/projects/{id}/branches"), json!({ "name": "y", "lsn": "nope" })).await.0, 400);
    assert_eq!(h.post(&format!("/v1/projects/{id}/branches"), json!({ "name": "y", "parent": "ghost" })).await.0, 404);

    let r = h.http.patch(format!("{}/v1/projects/{id}", h.api)).json(&json!({ "history_retention_s": 3600 })).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["history_retention_s"], json!(3600));

    let r = h.http.patch(format!("{}/v1/endpoints/{ep}", h.api)).json(&json!({ "min_cu": 8, "max_cu": 1 })).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 400);

    assert_eq!(h.post(&format!("/v1/endpoints/{ep}/suspend"), json!({})).await.0, 200);
    let (_, e) = h.get(&format!("/v1/endpoints/{ep}")).await;
    assert_eq!(e["state"], json!("suspended"));

    // The internal WAL API needs the token.
    let branch = created["branches"][0]["id"].as_str().unwrap();
    let file = "000000010000000000000001";
    assert_eq!(h.get(&format!("/internal/wal/{branch}/{file}?token=wrong")).await.0, 403);
    let token = &h.control.config.internal_token;
    let r = h.http.get(format!("{}/internal/wal/{branch}/{file}?token={token}", h.api)).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.bytes().await.unwrap().len(), 16 * 1024 * 1024);
    assert_eq!(h.get(&format!("/internal/wal/{branch}/..%2Fsecret?token={token}")).await.0, 404);

    let r = h.http.delete(format!("{}/v1/projects/{id}", h.api)).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 204);
    assert_eq!(h.get(&format!("/v1/projects/{id}")).await.0, 404);
}

// ── Postgres versions ──

async fn version_roundtrip(v: u32) {
    let h = harness!();
    let db = h.project(Some(v)).await;
    let c = h.connect(&db, &db.ep.id).await;
    let num: String = c.query_one("SHOW server_version_num", &[]).await.unwrap().get(0);
    assert!(num.starts_with(&v.to_string()), "expected Postgres {v}, got {num}");
    seed(&c, 400).await;

    let b = h.control.create_branch(&db.project.id, "copy", None, None).await.unwrap();
    let bc = h.connect(&db, &h.primary(&b).id).await;
    assert_eq!(count(&bc, "SELECT count(*) FROM t").await, 400);

    drop(c);
    h.control.suspend(&db.ep.id).await.unwrap();
    h.control.evict_branch(&db.branch.id).await.unwrap();
    let c = h.connect(&db, &db.ep.id).await;
    assert_eq!(count(&c, "SELECT count(*) FROM t").await, 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_16() {
    version_roundtrip(16).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_17() {
    version_roundtrip(17).await;
}
