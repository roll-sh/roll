//! roll nosql on several nodes in one process (docs/nosql/architecture.md, "On a fleet"): one
//! object store, one catalog and one lease store (roll-cluster's memory backend, or a fresh
//! table on DynamoDB Local with `ROLL_NOSQL_TEST_CLUSTER_BACKEND=dynamodb` and
//! `ROLL_CLUSTER_TEST_DYNAMODB` naming its endpoint). Each node has its internal API (forwarded
//! requests, the CQL splice hop), a CQL listener, a TLS one routed by SNI (`<id>.nosql.test`, as
//! the edge's) and Alternator.
//!
//! - requests and CQL connections that reach any node run on the database's owner;
//! - a killed owner's databases are taken over within the lease, with every acknowledged
//!   (`sync`) write, and the old owner can't write any more (fencing);
//! - a drain hands databases over with every acknowledged (`async`) write, clients only
//!   reconnecting.

mod common;

use common::{exec, texts};
use nosql_control::store::Store;
use nosql_control::{Control, NosqlConfig};
use nosql_proto::client::{Client, ClientConfig};
use roll_cluster::{Cluster, ClusterConfig, DynamoDbConfig, Memory, MemoryStore};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

const SECRET: &str = "nosql cluster test secret, 32 bytes";

enum Leases {
    Memory(MemoryStore),
    Dynamo(DynamoDbConfig),
}

struct Fleet {
    /// `*.nosql.test`, for the TLS listeners.
    cert: rcgen::CertifiedKey<rcgen::KeyPair>,
    objects: nosql_storage::Objects,
    catalog: Arc<Store>,
    leases: Leases,
    nodes: Vec<NodeUnderTest>,
    http: reqwest::Client,
    _homes: Vec<tempfile::TempDir>,
}

struct NodeUnderTest {
    control: Arc<Control>,
    cluster: Arc<Cluster>,
    api: String,
    cql: SocketAddr,
    cql_tls: SocketAddr,
    alternator: String,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl NodeUnderTest {
    fn id(&self) -> &str {
        self.cluster.node_id()
    }

    /// Like a killed process: its leases expire on their own and nothing it serves answers.
    fn kill(&self) {
        self.cluster.fault_kill();
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Fleet {
    /// `n` nodes whose new databases (and unrouted connections) have `dialect`.
    async fn new(name: &str, n: usize, dialect: &str) -> Fleet {
        let leases = match std::env::var("ROLL_NOSQL_TEST_CLUSTER_BACKEND").as_deref() {
            Ok("dynamodb") => {
                let endpoint = std::env::var("ROLL_CLUSTER_TEST_DYNAMODB").expect("ROLL_CLUSTER_TEST_DYNAMODB: DynamoDB Local's endpoint");
                let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
                let mut c = DynamoDbConfig::new(format!("t-nosql-{name}-{n:x}"));
                c.endpoint = Some(endpoint);
                c.create_table = true;
                Leases::Dynamo(c)
            }
            Ok("memory") | Err(_) => Leases::Memory(MemoryStore::new()),
            Ok(other) => panic!("ROLL_NOSQL_TEST_CLUSTER_BACKEND={other}: memory or dynamodb"),
        };
        let mut f = Fleet {
            cert: rcgen::generate_simple_self_signed(vec!["*.nosql.test".into()]).unwrap(),
            objects: nosql_storage::test_objects(&format!("cluster-{name}")),
            catalog: Arc::new(Store::memory().unwrap()),
            leases,
            nodes: Vec::new(),
            http: reqwest::Client::new(),
            _homes: Vec::new(),
        };
        for i in 0..n {
            f.add(&format!("n{i}"), dialect).await;
        }
        // Everyone sees everyone before placement is relied on.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while f.nodes.iter().any(|x| x.cluster.nodes().len() != n) {
            assert!(tokio::time::Instant::now() < deadline, "the nodes never saw each other");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        f
    }

    fn scale(&self) -> u32 {
        match self.leases {
            Leases::Memory(_) => 1,
            Leases::Dynamo(_) => 2,
        }
    }

    async fn add(&mut self, id: &str, dialect: &str) {
        let home = tempfile::tempdir().unwrap();
        let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", api_listener.local_addr().unwrap());
        let mut cc = ClusterConfig::for_tests(id, api.clone(), SECRET.as_bytes());
        let k = self.scale();
        for d in [&mut cc.heartbeat, &mut cc.node_ttl, &mut cc.lease_ttl, &mut cc.renew_every, &mut cc.lease_margin, &mut cc.sweep_every, &mut cc.rebalance_every, &mut cc.handoff_timeout] {
            *d *= k;
        }
        // A database's release (flush and all) fits in a handoff.
        cc.handoff_timeout = cc.handoff_timeout.max(Duration::from_secs(10));
        let backend: Box<dyn roll_cluster::Backend> = match &self.leases {
            Leases::Memory(m) => Box::new(Memory::new(m.clone())),
            Leases::Dynamo(c) => Box::new(roll_cluster::DynamoDb::connect(c.clone()).unwrap()),
        };
        let cluster = tokio::task::spawn_blocking(move || Cluster::join(backend, cc)).await.unwrap().unwrap();

        let cql_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cql = cql_listener.local_addr().unwrap();
        let tls_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cql_tls = tls_listener.local_addr().unwrap();
        let alt_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let alternator = format!("http://{}", alt_listener.local_addr().unwrap());
        let mut cfg = NosqlConfig::local(home.path().to_path_buf());
        cfg.domain = Some("nosql.test".into());
        cfg.defaults.dialect = dialect.into();
        cfg.node_name = id.into();
        cfg.cql_port = cql.port();
        cfg.cql_shard_aware_port = None;
        cfg.alternator_port = None;
        cfg.rest_port = None;
        cfg.gc_interval = None;
        cfg.expiry_interval = Duration::from_secs(3600);
        cfg.metering_interval = Duration::from_secs(3600);
        cfg.import_interval = Duration::from_secs(3600);
        cfg.storage.maintenance_interval = Duration::from_millis(200);
        // The ship interval is all an `async` write can lose to a crash.
        cfg.storage.log.flush_interval = Duration::from_millis(100);
        let control = Control::with_catalog(cfg, self.objects.clone(), self.catalog.clone()).await.unwrap();
        let signer = roll_edge::splice::Signer::new(SECRET).unwrap();
        let s = signer.clone();
        control.join(
            cluster.clone(),
            Arc::new(move |to: nosql_control::cluster::HopTo| {
                let s = s.clone();
                Box::pin(async move {
                    let hop = roll_edge::splice::Hop { product: "cql".into(), tenant: to.db, client: to.client, sni: to.sni, hops: 0 };
                    let io = roll_edge::splice::connect(&to.node, &s, &hop).await?;
                    Ok(Box::new(io) as Box<dyn nosql_server::conn::Io>)
                })
            }),
        );
        let handlers = roll_edge::splice::Handlers::default();
        let node = control.node.clone();
        handlers.register("cql", Arc::new(move |io, hop| nosql_control::cluster::serve_spliced(node.clone(), Box::new(io), hop.client, hop.sni)));
        let router = nosql_control::api::router(control.clone())
            .merge(nosql_control::cluster::internal_routes(control.clone(), Default::default()))
            .merge(roll_edge::splice::router(signer, handlers))
            .layer(axum::middleware::from_fn_with_state(cluster.clone(), roll_cluster::accept_forwarded));
        let dir: Arc<dyn nosql_server::Directory> = control.node.clone();
        let acceptor = nosql_server::net::tls_acceptor(self.cert.cert.pem().as_bytes(), self.cert.signing_key.serialize_pem().as_bytes()).unwrap();
        let tls_opts = nosql_server::ListenOptions { tls: Some(acceptor), ..Default::default() };
        let tls_dir = dir.clone();
        let alt = nosql_control::cluster::alternator(control.clone(), Default::default());
        let tasks = vec![
            tokio::spawn(async move {
                let _ = nosql_server::net::serve(tls_listener, tls_dir, tls_opts).await;
            }),
            tokio::spawn(async move {
                let _ = axum::serve(alt_listener, alt).await;
            }),
            tokio::spawn(async move {
                let _ = axum::serve(api_listener, router.into_make_service_with_connect_info::<SocketAddr>()).await;
            }),
            tokio::spawn(async move {
                let _ = nosql_server::net::serve(cql_listener, dir, nosql_server::ListenOptions::default()).await;
            }),
        ];
        self.nodes.push(NodeUnderTest { control, cluster, api, cql, cql_tls, alternator, tasks });
        self._homes.push(home);
    }

    /// The node holding the database's lease (and serving it), waiting for one.
    async fn owner(&self, db: &str) -> &NodeUnderTest {
        let key = nosql_control::cluster::key(db);
        for _ in 0..400 {
            if let Some(n) = self.nodes.iter().find(|n| n.cluster.owns(&key)) {
                return n;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("nobody owns {db}");
    }

    /// Nodes that aren't `but`.
    fn others<'a>(&'a self, but: &'a str) -> impl Iterator<Item = &'a NodeUnderTest> + 'a {
        self.nodes.iter().filter(move |n| n.id() != but)
    }

    async fn call(&self, at: &NodeUnderTest, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut req = self.http.request(method, format!("{}{path}", at.api));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let r = req.send().await.unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    async fn create(&self, at: &NodeUnderTest, name: &str, durability: &str) -> Value {
        let (s, v) = self.call(at, reqwest::Method::POST, "/v1/nosql/databases", Some(json!({ "name": name, "durability": durability }))).await;
        assert_eq!(s, 200, "{v}");
        v
    }

    /// A CQL connection through `at`, routed by the SASL user (`role@db`).
    async fn cql(&self, at: &NodeUnderTest, db: &Value) -> Client {
        let user = format!("cassandra@{}", db["id"].as_str().unwrap());
        let password = db["credentials"]["password"].as_str().unwrap();
        let mut last = None;
        for _ in 0..100 {
            match Client::connect(ClientConfig::new(at.cql.to_string()).credentials(user.clone(), password)).await {
                Ok(c) => return c,
                Err(e) => last = Some(e),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("no CQL connection through {}: {last:?}", at.id());
    }
}

/// Whichever node a request or a connection reaches, the database runs on its owner only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_and_connections_reach_the_owner() {
    let f = Fleet::new("route", 3, "scylla").await;
    let db = f.create(&f.nodes[0], "app", "async").await;
    let id = db["id"].as_str().unwrap().to_string();
    // The console's CQL through every node.
    for (i, n) in f.nodes.iter().enumerate() {
        let q = if i == 0 { "CREATE KEYSPACE ks WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}".to_string() } else { "SELECT * FROM system.local WHERE key = 'local'".to_string() };
        let (s, v) = f.call(n, reqwest::Method::POST, &format!("/v1/nosql/databases/{id}/cql"), Some(json!({ "query": q }))).await;
        assert_eq!(s, 200, "{v}");
        assert!(v.get("error").is_none(), "through {}: {v}", n.id());
    }
    let owner = f.owner(&id).await;
    assert!(owner.control.node.get_loaded(&id).is_some());
    // CQL connections through a node that isn't the owner are handed to it.
    let other = f.others(owner.id()).next().unwrap();
    let c = f.cql(other, &db).await;
    exec(&c, "CREATE TABLE ks.t (k int PRIMARY KEY, v text)").await;
    exec(&c, "INSERT INTO ks.t (k, v) VALUES (1, 'one')").await;
    assert_eq!(texts(&c, "SELECT v FROM ks.t WHERE k = 1").await, ["one"]);
    for n in f.others(owner.id()) {
        assert!(n.control.node.get_loaded(&id).is_none(), "{} loaded {id}", n.id());
    }
    // A TLS connection naming the database by SNI, as the edge's are, is handed over at once.
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.add(f.cert.cert.der().clone()).unwrap();
    let tls = nosql_proto::client::TlsConfig::with_roots(roots, format!("{id}.nosql.test")).unwrap();
    let password = db["credentials"]["password"].as_str().unwrap();
    let c = Client::connect(ClientConfig::new(other.cql_tls.to_string()).tls(tls).credentials("cassandra", password)).await.unwrap();
    assert_eq!(texts(&c, "SELECT v FROM ks.t WHERE k = 1").await, ["one"]);
    // Alternator through another node runs on the owner (the access key names the database).
    let (s, creds) = f.call(other, reqwest::Method::GET, &format!("/v1/nosql/databases/{id}/alternator"), None).await;
    assert_eq!(s, 200, "{creds}");
    let sdk = {
        use aws_sdk_dynamodb::config::{BehaviorVersion, Credentials, Region};
        let key = creds["access_key_id"].as_str().unwrap();
        let secret = creds["secret_access_key"].as_str().unwrap();
        let conf = aws_sdk_dynamodb::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .endpoint_url(&other.alternator)
            .credentials_provider(Credentials::new(key, secret, None, None, "test"))
            .build();
        aws_sdk_dynamodb::Client::from_conf(conf)
    };
    let tables = sdk.list_tables().send().await.unwrap();
    assert!(tables.table_names().is_empty(), "{tables:?}");
    for n in f.others(owner.id()) {
        assert!(n.control.node.get_loaded(&id).is_none(), "{} loaded {id}", n.id());
    }
    // So do requests by name.
    let (s, v) = f.call(other, reqwest::Method::GET, "/v1/nosql/databases/app", None).await;
    assert_eq!((s, v["state"].as_str()), (200, Some("running")), "{v}");
}

/// The owner dies: another node takes its databases within the lease, with every write it
/// acknowledged (`sync`), and the old owner, if it comes back to life, can't write.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_killed_owner_is_taken_over_with_every_acknowledged_write() {
    let f = Fleet::new("kill", 3, "scylla").await;
    let db = f.create(&f.nodes[0], "app", "sync").await;
    let id = db["id"].as_str().unwrap().to_string();
    let entry = &f.nodes[0];
    let c = f.cql(entry, &db).await;
    exec(&c, "CREATE KEYSPACE ks WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}").await;
    exec(&c, "CREATE TABLE ks.t (k int PRIMARY KEY, v text)").await;
    for k in 0..20 {
        exec(&c, &format!("INSERT INTO ks.t (k, v) VALUES ({k}, 'v{k}')")).await;
    }
    let owner = f.owner(&id).await;
    let dead = owner.id().to_string();
    let zombie = owner.control.node.get_loaded(&id).expect("loaded on its owner");
    owner.kill();
    let started = std::time::Instant::now();
    let survivor = f.others(&dead).next().unwrap();
    let c = f.cql(survivor, &db).await;
    let mut got = texts(&c, "SELECT v FROM ks.t").await;
    got.sort_by_key(|v| v[1..].parse::<u32>().unwrap());
    assert_eq!(got, (0..20).map(|k| format!("v{k}")).collect::<Vec<_>>());
    let new_owner = f.owner(&id).await;
    assert_ne!(new_owner.id(), dead);
    let lease = new_owner.cluster.config().lease_ttl;
    assert!(started.elapsed() < lease * 4 + Duration::from_secs(5), "takeover took {:?}", started.elapsed());
    // The dead node's database, still in its memory, can't commit anything now.
    let storage = zombie.db.store().clone();
    assert!(storage.write_manifest(false).await.is_err_and(|e| nosql_storage::manifest::is_fenced(&e)));
    assert!(storage.is_fenced());
}

/// A drain hands every database over with every acknowledged write, `async` ones included;
/// clients only see their connection close and reconnect. (Cassandra's dialect: protocol v5,
/// whose AUTH_RESPONSE comes in a segment after STARTUP's answer switched the framing; the
/// owner gets it as the client sent it.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drain_hands_databases_over_with_nothing_lost() {
    let f = Fleet::new("drain", 3, "cassandra").await;
    let db = f.create(&f.nodes[0], "app", "async").await;
    let id = db["id"].as_str().unwrap().to_string();
    let c = f.cql(&f.nodes[0], &db).await;
    exec(&c, "CREATE KEYSPACE ks WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}").await;
    exec(&c, "CREATE TABLE ks.t (k int PRIMARY KEY, v text)").await;
    let owner = f.owner(&id).await;
    let leaving = owner.id().to_string();
    // A client of the owner itself, and one through another node.
    let direct = f.cql(owner, &db).await;
    let entry = f.others(&leaving).next().unwrap();
    let through = f.cql(entry, &db).await;
    assert_eq!(through.version(), nosql_proto::Version::V5);
    for k in 0..50 {
        let c = if k % 2 == 0 { &direct } else { &through };
        exec(c, &format!("INSERT INTO ks.t (k, v) VALUES ({k}, 'v{k}')")).await;
    }
    // Acknowledged, not yet shipped (the ship interval is 100 ms): the drain must keep them.
    owner.cluster.drain(Duration::from_secs(30)).await;
    assert!(owner.control.node.get_loaded(&id).is_none(), "released");
    // The clients' connections were closed; through the same entry node, a new one reaches the
    // new owner.
    let c = f.cql(entry, &db).await;
    let got = texts(&c, "SELECT v FROM ks.t").await;
    assert_eq!(got.len(), 50, "{got:?}");
    let new_owner = f.owner(&id).await;
    assert_ne!(new_owner.id(), leaving);
    assert!(direct.query("SELECT v FROM ks.t", Default::default()).await.is_err(), "the old owner's connections are closed");
}
