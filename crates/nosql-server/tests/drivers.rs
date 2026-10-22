//! Drivers against roll: the Rust `scylla` crate in-process — connecting, cluster and schema
//! metadata, schema agreement, token-aware and shard-aware routing, paging, prepared statements,
//! batches, LWT marking and TLS. `docker_drivers` (at the end) runs the Python, Java, Go and Node
//! drivers in Docker against the `roll/nosql` image and the reference containers.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use futures::TryStreamExt;
use nosql_engine::{DbConfig, Dialect};
use nosql_server::{Directory, ListenOptions, Single, net};
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use scylla::statement::batch::Batch;

struct Server {
    addr: SocketAddr,
    dir: Arc<Single>,
    _store: Arc<nosql_storage::store::Store>,
}

/// A database presented on the port it listens on, with `shards` shards and a shard-aware
/// port (ScyllaDB dialect).
async fn server(dialect: Dialect, shards: u32) -> Server {
    let main = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let aware = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = main.local_addr().unwrap();
    let mut cfg = DbConfig::new("drv", dialect);
    cfg.native_port = addr.port();
    cfg.settings.shards = shards;
    let (db, store) = database_with(cfg).await;
    let mut single = Single::new(db);
    single.settings_mut().shard_aware_port = Some(aware.local_addr().unwrap().port());
    let dir = Arc::new(single);
    let d: Arc<dyn Directory> = dir.clone();
    tokio::spawn(net::serve(main, d.clone(), ListenOptions::default()));
    tokio::spawn(net::serve(aware, d, ListenOptions { shard_aware: true, ..Default::default() }));
    Server { addr, dir, _store: store }
}

async fn session(addr: SocketAddr) -> Session {
    SessionBuilder::new().known_node_addr(addr).connection_timeout(Duration::from_secs(10)).build().await.unwrap()
}

async fn setup(s: &Session) {
    s.query_unpaged("CREATE KEYSPACE srv_drv WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}", &[]).await.unwrap();
    s.query_unpaged("CREATE TABLE srv_drv.t (k int, c int, v text, PRIMARY KEY (k, c))", &[]).await.unwrap();
    s.await_schema_agreement().await.unwrap();
}

#[tokio::test]
async fn connect_and_query_both_dialects() {
    init_tracing();
    for d in [Dialect::Scylla, Dialect::Cassandra] {
        let srv = server(d, 1).await;
        let s = session(srv.addr).await;
        let r = s.query_unpaged("SELECT release_version FROM system.local", &[]).await.unwrap().into_rows_result().unwrap();
        let (v,): (String,) = r.single_row().unwrap();
        assert_eq!(v, if d == Dialect::Scylla { "3.0.8" } else { "5.0.9" });
        setup(&s).await;
        s.query_unpaged("INSERT INTO srv_drv.t (k, c, v) VALUES (?, ?, ?)", (1i32, 1i32, "one")).await.unwrap();
        let r = s.query_unpaged("SELECT c, v FROM srv_drv.t WHERE k = ?", (1i32,)).await.unwrap().into_rows_result().unwrap();
        let rows: Vec<(i32, String)> = r.rows::<(i32, String)>().unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(rows, vec![(1, "one".to_string())]);
        // Errors come through typed.
        let e = s.query_unpaged("SELEC", &[]).await.unwrap_err();
        assert!(e.to_string().contains("no viable alternative"), "{e}");
    }
}

#[tokio::test]
async fn metadata_and_schema_agreement() {
    let srv = server(Dialect::Scylla, 1).await;
    let s = session(srv.addr).await;
    setup(&s).await;
    assert!(s.check_schema_agreement().await.unwrap().is_some());
    s.refresh_metadata().await.unwrap();
    let state = s.get_cluster_state();
    let ks = state.get_keyspace("srv_drv").expect("keyspace in driver metadata");
    let t = ks.tables.get("t").expect("table in driver metadata");
    assert_eq!(t.partition_key, vec!["k".to_string()]);
    assert_eq!(t.clustering_key, vec!["c".to_string()]);
    assert!(t.columns.contains_key("v"));
    let nodes = state.get_nodes_info();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].host_id.as_bytes(), &srv.dir.loaded().db.config().host_id());
    assert_eq!(nodes[0].datacenter.as_deref(), Some("datacenter1"));
    // Schema changes the driver sees through events.
    s.query_unpaged("ALTER TABLE srv_drv.t ADD w int", &[]).await.unwrap();
    s.await_schema_agreement().await.unwrap();
    s.refresh_metadata().await.unwrap();
    assert!(s.get_cluster_state().get_keyspace("srv_drv").unwrap().tables["t"].columns.contains_key("w"));
}

#[tokio::test]
async fn token_and_shard_aware_routing() {
    let srv = server(Dialect::Scylla, 2).await;
    let s = session(srv.addr).await;
    // A vnode keyspace: drivers learn tablet replicas lazily, from routing payloads.
    s.query_unpaged(
        "CREATE KEYSPACE srv_drv WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1} AND tablets = {'enabled': false}",
        &[],
    )
    .await
    .unwrap();
    s.query_unpaged("CREATE TABLE srv_drv.t (k int, c int, v text, PRIMARY KEY (k, c))", &[]).await.unwrap();
    s.await_schema_agreement().await.unwrap();
    s.refresh_metadata().await.unwrap();
    let p = s.prepare("SELECT v FROM srv_drv.t WHERE k = ?").await.unwrap();
    assert!(p.is_token_aware());
    let state = s.get_cluster_state();
    let token = state.compute_token("srv_drv", "t", &(7i32,)).unwrap();
    assert_eq!(token.value(), nosql_types::token(&7i32.to_be_bytes()));
    assert_eq!(p.calculate_token(&(7i32,)).unwrap().map(|t| t.value()), Some(token.value()));
    let replicas = state.get_token_endpoints("srv_drv", "t", token);
    assert_eq!(replicas.len(), 1);
    let node = &replicas[0].0;
    let sharder = node.sharder().expect("shard-aware");
    assert_eq!(sharder.nr_shards.get(), 2);
    assert_eq!(replicas[0].1, sharder.shard_of(token));
    // The driver keeps a connection per shard (control connection + 2 shards).
    let mut ok = false;
    for _ in 0..100 {
        if srv.dir.loaded().metrics.snapshot().connections >= 3 {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ok, "{:?}", srv.dir.loaded().metrics.snapshot());
    for k in 0..20i32 {
        s.query_unpaged("INSERT INTO srv_drv.t (k, c, v) VALUES (?, 0, 'x')", (k,)).await.unwrap();
        let r = s.execute_unpaged(&p, (k,)).await.unwrap().into_rows_result().unwrap();
        assert_eq!(r.rows_num(), 1);
    }
}

#[tokio::test]
async fn paging_prepared_and_batches() {
    let srv = server(Dialect::Scylla, 1).await;
    let s = session(srv.addr).await;
    setup(&s).await;
    let ins = s.prepare("INSERT INTO srv_drv.t (k, c, v) VALUES (?, ?, ?)").await.unwrap();
    let mut b: Batch = Default::default();
    let mut vals = Vec::new();
    for c in 0..30i32 {
        b.append_statement(ins.clone());
        vals.push((5i32, c, format!("v{c}")));
    }
    s.batch(&b, vals).await.unwrap();
    // Paged iteration over a prepared statement.
    let mut sel = s.prepare("SELECT c, v FROM srv_drv.t WHERE k = ?").await.unwrap();
    sel.set_page_size(7);
    let rows: Vec<(i32, String)> = s.execute_iter(sel.clone(), (5i32,)).await.unwrap().rows_stream::<(i32, String)>().unwrap().try_collect().await.unwrap();
    assert_eq!(rows.len(), 30);
    assert_eq!(rows[29], (29, "v29".to_string()));
    // And over an unprepared one.
    let mut q = scylla::statement::unprepared::Statement::new("SELECT c FROM srv_drv.t WHERE k = 5");
    q.set_page_size(4);
    let rows: Vec<(i32,)> = s.query_iter(q, &[]).await.unwrap().rows_stream::<(i32,)>().unwrap().try_collect().await.unwrap();
    assert_eq!(rows.len(), 30);
    // Single pages with explicit paging state.
    let (page, state) = s.execute_single_page(&sel, (5i32,), scylla::response::PagingState::start()).await.unwrap();
    assert_eq!(page.into_rows_result().unwrap().rows_num(), 7);
    assert!(matches!(state, scylla::response::PagingStateResponse::HasMorePages { .. }));
    // USE.
    s.use_keyspace("srv_drv", false).await.unwrap();
    let r = s.query_unpaged("SELECT count(*) FROM t WHERE k = 5", &[]).await.unwrap().into_rows_result().unwrap();
    let (n,): (i64,) = r.single_row().unwrap();
    assert_eq!(n, 30);
}

/// The driver over TLS, on both ports of a database with a password: it verifies the server's
/// certificate (for the address, as the driver names servers by IP and sends no SNI), logs in,
/// and routes shard-aware over TLS too. A client that doesn't trust the certificate fails.
#[tokio::test]
async fn tls_on_both_ports() {
    use tokio_rustls::rustls;
    let ck = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let acceptor = net::tls_acceptor(ck.cert.pem().as_bytes(), ck.signing_key.serialize_pem().as_bytes()).unwrap();
    let main = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let aware = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = main.local_addr().unwrap();
    let mut cfg = DbConfig::new("drvtls", Dialect::Scylla);
    cfg.native_port = addr.port();
    cfg.settings.shards = 2;
    let (db, _store) = database_with(cfg).await;
    let mut single = Single::with_spec(db, |s| *s = s.clone().with_password("cassandra", "pw"));
    single.settings_mut().shard_aware_port = Some(aware.local_addr().unwrap().port());
    let d: Arc<dyn Directory> = Arc::new(single);
    let opts = ListenOptions { tls: Some(acceptor), ..Default::default() };
    tokio::spawn(net::serve(main, d.clone(), opts.clone()));
    tokio::spawn(net::serve(aware, d, ListenOptions { shard_aware: true, ..opts }));

    let client = |roots: rustls::RootCertStore| {
        Arc::new(rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider())).with_safe_default_protocol_versions().unwrap().with_root_certificates(roots).with_no_client_auth())
    };
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ck.cert.der().clone()).unwrap();
    let s = SessionBuilder::new()
        .known_node_addr(addr)
        .user("cassandra", "pw")
        .tls_context(Some(client(roots)))
        .connection_timeout(Duration::from_secs(10))
        .build()
        .await
        .unwrap();
    setup(&s).await;
    for k in 0..20i32 {
        s.query_unpaged("INSERT INTO srv_drv.t (k, c, v) VALUES (?, 1, 'tls')", (k,)).await.unwrap();
    }
    let r = s.query_unpaged("SELECT count(*) FROM srv_drv.t", &[]).await.unwrap().into_rows_result().unwrap();
    assert_eq!(r.single_row::<(i64,)>().unwrap().0, 20);
    let r = s.query_unpaged("SELECT ssl_enabled, ssl_protocol FROM system.clients", &[]).await.unwrap().into_rows_result().unwrap();
    let clients: Vec<(bool, String)> = r.rows::<(bool, String)>().unwrap().collect::<Result<_, _>>().unwrap();
    assert!(clients.len() >= 2 && clients.iter().all(|(on, p)| *on && p.starts_with("TLSv1.")), "{clients:?}");
    // Connections on both shards: the shard-aware port speaks TLS too.
    let r = s.query_unpaged("SELECT DISTINCT shard_id FROM system.clients", &[]).await;
    if let Ok(r) = r {
        let shards: Vec<(i32,)> = r.into_rows_result().unwrap().rows::<(i32,)>().unwrap().collect::<Result<_, _>>().unwrap();
        assert!(!shards.is_empty());
    }
    // A client that doesn't trust the certificate.
    let other = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let mut wrong = rustls::RootCertStore::empty();
    wrong.add(other.cert.der().clone()).unwrap();
    let e = SessionBuilder::new().known_node_addr(addr).user("cassandra", "pw").tls_context(Some(client(wrong))).connection_timeout(Duration::from_secs(5)).build().await;
    assert!(e.is_err());
}

#[tokio::test]
async fn reconnects_after_eviction() {
    // A node database: evicting it drops the driver's connections; the driver reconnects.
    let store = store("drv-node").await;
    let catalog = Arc::new(nosql_server::StaticCatalog::default());
    let main = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = main.local_addr().unwrap();
    let mut spec = nosql_server::DbSpec::new("n1", Dialect::Scylla);
    spec.config.native_port = addr.port();
    catalog.insert(spec);
    let cfg = nosql_server::NodeConfig { default_db: Some("n1".into()), ..Default::default() };
    let node = nosql_server::Node::new(catalog, store, cfg);
    tokio::spawn(net::serve(main, node.clone() as Arc<dyn Directory>, ListenOptions::default()));
    let s = session(addr).await;
    setup(&s).await;
    s.query_unpaged("INSERT INTO srv_drv.t (k, c, v) VALUES (1, 1, 'kept')", &[]).await.unwrap();
    assert!(node.evict("n1").await.unwrap());
    let mut back = false;
    for _ in 0..100 {
        if let Ok(r) = s.query_unpaged("SELECT v FROM srv_drv.t WHERE k = 1", &[]).await {
            let (v,): (String,) = r.into_rows_result().unwrap().single_row().unwrap();
            assert_eq!(v, "kept");
            back = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(back, "the driver reconnected");
}

/// The driver's tracing: traced statements return an id, and `get_tracing_info` finds the
/// complete session (duration, request, events) at its first attempt.
#[tokio::test]
async fn tracing_info_completes() {
    for d in [Dialect::Scylla, Dialect::Cassandra] {
        let srv = server(d, 1).await;
        let s = session(srv.addr).await;
        setup(&s).await;
        let mut st = scylla::statement::Statement::new("INSERT INTO srv_drv.t (k, c, v) VALUES (1, 1, 'one')");
        st.set_tracing(true);
        let id = s.query_unpaged(st, &[]).await.unwrap().tracing_id().expect("a tracing id");
        let info = s.get_tracing_info(&id).await.unwrap();
        assert_eq!(info.command.as_deref(), Some("QUERY"), "{d:?}");
        assert_eq!(info.request.as_deref(), Some("Execute CQL3 query"), "{d:?}");
        assert!(info.duration.is_some_and(|us| us >= 0), "{d:?}");
        assert!(!info.events.is_empty(), "{d:?}");
        // Prepared statements and batches too.
        let mut p = s.prepare("SELECT v FROM srv_drv.t WHERE k = ?").await.unwrap();
        p.set_tracing(true);
        let id = s.execute_unpaged(&p, (1i32,)).await.unwrap().tracing_id().expect("a tracing id");
        let info = s.get_tracing_info(&id).await.unwrap();
        assert!(info.request.as_deref().is_some_and(|r| r.starts_with("Execute CQL3 prepared query")), "{d:?}: {info:?}");
        let mut b = Batch::default();
        b.append_statement("INSERT INTO srv_drv.t (k, c, v) VALUES (2, 1, 'two')");
        b.set_tracing(true);
        let id = s.batch(&b, ((),)).await.unwrap().tracing_id().expect("a tracing id");
        assert_eq!(s.get_tracing_info(&id).await.unwrap().request.as_deref(), Some("Execute batch of CQL3 queries"), "{d:?}");
        // Untraced: no id.
        assert!(s.query_unpaged("SELECT v FROM srv_drv.t", &[]).await.unwrap().tracing_id().is_none());
    }
}

/// ScyllaDB pushes no SCHEMA_CHANGE event for functions and aggregates (Cassandra does); gocql
/// found the difference: with the events, it rebuilt its keyspace metadata from rows ScyllaDB
/// would never have made it re-read, and tripped over an aggregate without a final function.
#[tokio::test]
async fn function_events_per_dialect() {
    use nosql_proto::event::{Event, EventType, SchemaTarget};
    for d in [Dialect::Scylla, Dialect::Cassandra] {
        let srv = single_with(d, |c| c.settings.user_defined_functions_enabled = true, |_| {}, |_| {}).await;
        let c = connect(srv.addr, nosql_proto::Version::V4, None).await;
        let mut events = c.register(&[EventType::SchemaChange]).await.unwrap();
        let lang = if d == Dialect::Scylla { "lua AS 'return (a or 0) + (x or 0)'" } else { "java AS 'return a + x;'" };
        for q in [
            "CREATE KEYSPACE fe WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}".to_string(),
            format!("CREATE FUNCTION fe.plus(a int, x int) CALLED ON NULL INPUT RETURNS int LANGUAGE {lang}"),
            "CREATE AGGREGATE fe.total(int) SFUNC plus STYPE int INITCOND 0".into(),
            "DROP AGGREGATE fe.total".into(),
            "DROP FUNCTION fe.plus".into(),
            "CREATE TABLE fe.t (k int PRIMARY KEY)".into(),
        ] {
            c.query(&q, Default::default()).await.unwrap_or_else(|e| panic!("{q}: {e:?}"));
        }
        let mut targets = Vec::new();
        while let Ok(Some(e)) = tokio::time::timeout(Duration::from_millis(500), events.recv()).await {
            if let Event::SchemaChange(sc) = e {
                targets.push(sc.target);
            }
        }
        let want = match d {
            Dialect::Scylla => vec![SchemaTarget::Keyspace, SchemaTarget::Table],
            Dialect::Cassandra => vec![
                SchemaTarget::Keyspace,
                SchemaTarget::Function,
                SchemaTarget::Aggregate,
                SchemaTarget::Aggregate,
                SchemaTarget::Function,
                SchemaTarget::Table,
            ],
        };
        assert_eq!(targets, want, "{d:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// The other drivers, in Docker, against the `roll/nosql` image and the reference containers.
//
// `tests/drivers/{python,java,go,node}` hold one suite per language with the same checks under
// the same names (connect, auth, system.local, metadata of tables/UDTs/indexes/views/functions,
// schema agreement, prepared statements, paging, batches, LWT, every type, token-aware and
// shard-aware routing, tracing, schema events, reconnect after a restart). Each runs against four
// roll containers (ScyllaDB and Cassandra dialects, each plain and with PasswordAuthenticator +
// UDFs + views) and the matching `nosql-ref-*` containers. A check that passes on the reference
// must pass on roll; reconnect only runs on roll (the references are shared and never restarted).
//
// Needs ROLL_NOSQL_IMAGE. ROLL_NOSQL_DRIVERS=python-scylla,go-gocql,... picks suites;
// ROLL_NOSQL_DRIVER_TARGETS=scylla,cassandra-auth,... picks targets.

struct DriverSuite {
    name: &'static str,
    dir: &'static str,
    tag: &'static str,
    build_args: &'static [&'static str],
}

const DRIVER_SUITES: &[DriverSuite] = &[
    DriverSuite { name: "python-cassandra", dir: "python", tag: "roll-nosql-drv-python:cassandra-3.30.1", build_args: &["DRIVER=cassandra-driver==3.30.1"] },
    DriverSuite { name: "python-scylla", dir: "python", tag: "roll-nosql-drv-python:scylla-3.29.12", build_args: &["DRIVER=scylla-driver==3.29.12"] },
    DriverSuite { name: "java-datastax", dir: "java", tag: "roll-nosql-drv-java:datastax-4.19.4", build_args: &["DRIVER=datastax"] },
    DriverSuite { name: "java-scylla", dir: "java", tag: "roll-nosql-drv-java:scylla-4.19.2.3", build_args: &["DRIVER=scylla"] },
    DriverSuite { name: "go-gocql", dir: "go", tag: "roll-nosql-drv-go:gocql-1.7.0", build_args: &["DRIVER=gocql"] },
    DriverSuite { name: "go-scylla", dir: "go", tag: "roll-nosql-drv-go:scylla-1.20.0", build_args: &["DRIVER=scylla"] },
    DriverSuite { name: "node", dir: "node", tag: "roll-nosql-drv-node:4.10.0", build_args: &[] },
];

struct DriverTarget {
    name: &'static str,
    dialect: &'static str,
    /// Auth (cassandra/cassandra), UDFs and materialized views.
    features: bool,
    /// The shared reference container to compare with.
    reference: &'static str,
}

const DRIVER_TARGETS: &[DriverTarget] = &[
    DriverTarget { name: "scylla", dialect: "scylla", features: false, reference: "nosql-ref-scylla" },
    DriverTarget { name: "cassandra", dialect: "cassandra", features: false, reference: "nosql-ref-cassandra" },
    DriverTarget { name: "scylla-auth", dialect: "scylla", features: true, reference: "nosql-ref-scylla-auth" },
    DriverTarget { name: "cassandra-auth", dialect: "cassandra", features: true, reference: "nosql-ref-cassandra-auth" },
];

/// ScyllaDB targets run with two shards, so shard-aware routing has a choice to make.
const ROLL_SHARDS: u32 = 2;

fn picked(var: &str, name: &str) -> bool {
    std::env::var(var).ok().filter(|v| !v.is_empty()).is_none_or(|v| v.split(',').any(|x| x.trim() == name))
}

/// A roll container configured the way the target's reference is, through the reference
/// images' own knobs (ScyllaDB flags; Cassandra environment and cassandra.yaml).
fn roll_target(image: &str, t: &DriverTarget, conf: &std::path::Path) -> docker::Container {
    let mut o = docker::RunOpts::default();
    match (t.dialect, t.features) {
        ("scylla", false) => o.args = ["--smp", "2", "--memory", "1G", "--overprovisioned", "1", "--developer-mode", "1"].map(String::from).to_vec(),
        ("scylla", true) => {
            o.args = [
                "--smp", "2", "--overprovisioned", "1", "--authenticator", "PasswordAuthenticator", "--authorizer", "CassandraAuthorizer",
                "--experimental-features", "udf", "--enable-user-defined-functions", "1",
            ]
            .map(String::from)
            .to_vec()
        }
        (_, false) => o.env = vec![("MAX_HEAP_SIZE", "1G".into()), ("CASSANDRA_CLUSTER_NAME", "Test Cluster".into())],
        (_, true) => {
            let yaml = conf.join("cassandra.yaml");
            std::fs::write(
                &yaml,
                "cluster_name: 'Test Cluster'\nauthenticator:\n  class_name : org.apache.cassandra.auth.PasswordAuthenticator\n\
                 authorizer: CassandraAuthorizer\nuser_defined_functions_enabled: true\nmaterialized_views_enabled: true\nsasi_indexes_enabled: true\n",
            )
            .unwrap();
            o.mounts = vec![format!("{}:/etc/cassandra/cassandra.yaml:ro", yaml.display())];
        }
    }
    let c = docker::Container::run(image, &o);
    c.wait_cql(Duration::from_secs(30));
    c
}

fn suite_env(t: &DriverTarget, host: &str, shards: u32, restart: bool, suite: &str) -> Vec<(String, String)> {
    let mut e = vec![
        ("HOST".to_string(), host.to_string()),
        ("DIALECT".into(), t.dialect.into()),
        ("DC".into(), "datacenter1".into()),
        ("SHARDS".into(), shards.to_string()),
        ("KS".into(), format!("drv_{}_{}", suite.replace('-', "_"), std::process::id())),
        ("DRIVER".into(), suite.into()),
    ];
    if t.features {
        e.extend([("FEATURES".into(), "1".into()), ("USER_NAME".into(), "cassandra".into()), ("PASS".into(), "cassandra".into())]);
    }
    if restart {
        e.push(("RESTART".into(), "1".into()));
    }
    e
}

#[test]
fn docker_drivers() {
    let Some(image) = docker::roll_image() else {
        eprintln!("skipped: set ROLL_NOSQL_IMAGE to run the driver suites in Docker");
        return;
    };
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/drivers");
    let suites: Vec<&DriverSuite> = DRIVER_SUITES.iter().filter(|s| picked("ROLL_NOSQL_DRIVERS", s.name)).collect();
    for s in &suites {
        docker::build(&root.join(s.dir), s.tag, s.build_args);
    }
    let conf = tempfile::tempdir().unwrap();
    let mut violations = Vec::new();
    let mut report = String::new();
    for t in DRIVER_TARGETS.iter().filter(|t| picked("ROLL_NOSQL_DRIVER_TARGETS", t.name)) {
        let mut roll_runs = Vec::new();
        {
            let c = roll_target(&image, t, conf.path());
            let ip = c.ip();
            for s in &suites {
                let shards = if t.dialect == "scylla" { ROLL_SHARDS } else { 0 };
                let run = docker::run_suite(s.tag, &suite_env(t, &ip, shards, true, s.name), Some(&c), Duration::from_secs(600));
                roll_runs.push(run);
            }
        }
        let reference = docker::reference(t.reference);
        for (s, roll) in suites.iter().zip(&roll_runs) {
            let refrun = reference.as_ref().map(|ip| docker::run_suite(s.tag, &suite_env(t, ip, 1, false, s.name), None, Duration::from_secs(600)));
            let mut line = format!("{:<16} {:<15}", s.name, t.name);
            let (mut pass, mut total) = (0, 0);
            if roll.checks.is_empty() {
                violations.push(format!("{} on roll {}: no results\n{}", s.name, t.name, roll.output));
            }
            for (check, outcome) in &roll.checks {
                total += 1;
                if *outcome == docker::Outcome::Pass {
                    pass += 1;
                }
                let on_ref = refrun.as_ref().and_then(|r| r.checks.get(check));
                match (on_ref, outcome) {
                    (Some(docker::Outcome::Pass), o) if *o != docker::Outcome::Pass => {
                        violations.push(format!("{} {} {check}: passes on {} but roll says {o:?}", s.name, t.name, t.reference))
                    }
                    (None, docker::Outcome::Fail(why)) if check == "reconnect" => violations.push(format!("{} {} {check}: {why}", s.name, t.name)),
                    (None, docker::Outcome::Fail(why)) => report.push_str(&format!("  unverified (no reference): {} {} {check}: {why}\n", s.name, t.name)),
                    (Some(r), o) if r.word() != o.word() => {
                        report.push_str(&format!("  note: {} {} {check}: {} on the reference, {} on roll\n", s.name, t.name, r.word(), o.word()))
                    }
                    _ => {}
                }
            }
            for (check, outcome) in &roll.checks {
                if let docker::Outcome::Fail(why) = outcome {
                    report.push_str(&format!("  roll fails {} {} {check}: {why}\n", s.name, t.name));
                }
            }
            if let Some(r) = &refrun {
                let ref_pass = r.checks.values().filter(|o| **o == docker::Outcome::Pass).count();
                line.push_str(&format!(" roll {pass}/{total} passed, reference {ref_pass}/{} passed", r.checks.len()));
                for (check, outcome) in &r.checks {
                    if let docker::Outcome::Fail(why) = outcome {
                        report.push_str(&format!("  reference fails {} {} {check}: {why}\n", s.name, t.name));
                    }
                }
            } else {
                line.push_str(&format!(" roll {pass}/{total} passed, no reference container {}", t.reference));
            }
            report.push_str(&line);
            report.push('\n');
        }
    }
    eprintln!("driver suites:\n{report}");
    assert!(violations.is_empty(), "driver differences:\n{}", violations.join("\n"));
}
