//! Redis on a cluster (redis_control::cluster): three nodes in one process, each with the RESP
//! front door, the REST data plane and an internal API on ports of their own, sharing one catalog
//! (SQLite, or Postgres with `ROLL_CODE_TEST_DATABASE`) and one object store. Leases are in
//! memory, or on DynamoDB Local (`ROLL_CLUSTER_TEST_DYNAMODB`; skipped without it unless
//! `ROLL_CLUSTER_DYNAMODB_REQUIRED=1`). Cluster timings are `ClusterConfig::for_tests`.
//!
//! Covered: connections, REST and control requests through any node reach the database's owner;
//! a killed owner's database moves with every acknowledged write; a drain hands a busy database
//! over with nothing lost and nothing but a reconnect for its clients; two nodes that both serve a
//! database can't both make writes durable.

use bytes::Bytes;
use redis_control::cluster::Front;
use redis_control::store::Store;
use redis_control::{Control, DbOptions, RedisConfig};
use redis_proto::Reply;
use redis_server::client::RespClient;
use redis_server::persist::Objects;
use roll_cluster::{Backend, Cluster, ClusterConfig, DynamoDb, DynamoDbConfig, Memory, MemoryStore};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::task::AbortHandle;

const SECRET: &str = "redis cluster test secret, 32 bytes";

#[derive(Clone, Copy, Debug)]
enum Kind {
    Memory,
    DynamoDb,
}

macro_rules! on_every_backend {
    ($($name:ident),* $(,)?) => {
        mod memory { $(#[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $name() { super::$name(super::Kind::Memory).await })* }
        mod dynamodb { $(#[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $name() { super::$name(super::Kind::DynamoDb).await })* }
    };
}

on_every_backend!(
    requests_through_any_node_reach_the_owner,
    a_killed_owners_databases_move_with_every_acknowledged_write,
    a_drain_hands_a_busy_database_over_losing_nothing,
);

struct Env {
    _dir: tempfile::TempDir,
    catalog: code_sql::Db,
    objects: Objects,
    memory: MemoryStore,
    dynamodb: Option<DynamoDbConfig>,
}

impl Env {
    fn new(kind: Kind, name: &str) -> Option<Env> {
        let _ = tracing_subscriber_init();
        let dynamodb = match kind {
            Kind::Memory => None,
            Kind::DynamoDb => match std::env::var("ROLL_CLUSTER_TEST_DYNAMODB") {
                Ok(endpoint) if !endpoint.is_empty() => {
                    let mut c = DynamoDbConfig::new(format!("t-redis-{name}-{:x}", redis_engine::util::Rng::new().next_u64()));
                    c.endpoint = Some(endpoint);
                    c.create_table = true;
                    Some(c)
                }
                _ => {
                    assert!(std::env::var("ROLL_CLUSTER_DYNAMODB_REQUIRED").as_deref() != Ok("1"), "DynamoDB Local is required (ROLL_CLUSTER_TEST_DYNAMODB)");
                    eprintln!("skipping: no DynamoDB Local (ROLL_CLUSTER_TEST_DYNAMODB)");
                    return None;
                }
            },
        };
        let dir = tempfile::tempdir().unwrap();
        let catalog = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("redis.db")), &format!("redis_{name}")).unwrap();
        Some(Env { _dir: dir, catalog, objects: redis_server::persist::test_objects(name), memory: MemoryStore::new(), dynamodb })
    }

    fn backend(&self) -> Box<dyn Backend> {
        match &self.dynamodb {
            Some(c) => Box::new(DynamoDb::connect(c.clone()).unwrap()),
            None => Box::new(Memory::new(self.memory.clone())),
        }
    }

    fn config(&self, id: &str, url: &str) -> ClusterConfig {
        let mut c = ClusterConfig::for_tests(id, url, SECRET.as_bytes());
        if self.dynamodb.is_some() {
            // A call to DynamoDB Local can take a good part of a second on a busy machine.
            for d in [&mut c.heartbeat, &mut c.node_ttl, &mut c.lease_ttl, &mut c.renew_every, &mut c.lease_margin, &mut c.sweep_every, &mut c.rebalance_every, &mut c.handoff_timeout] {
                *d *= 2;
            }
        }
        c
    }

    async fn node(&self, id: &str) -> TestNode {
        let internal = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", internal.local_addr().unwrap());
        let (backend, cfg) = (self.backend(), self.config(id, &url));
        let cluster = tokio::task::spawn_blocking(move || Cluster::join(backend, cfg)).await.unwrap().unwrap();
        let mut rcfg = RedisConfig::local(self._dir.path().join(id));
        rcfg.node_name = id.into();
        rcfg.flush_interval = Duration::from_millis(50);
        rcfg.gc_interval = None;
        rcfg.handoff_spread = Duration::from_millis(300);
        let control = Control::with_store(rcfg, self.objects.clone(), Store::open(self.catalog.clone()).unwrap()).await.unwrap();
        control.join(cluster.clone());
        let resp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rest = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (resp_addr, rest_addr) = (resp.local_addr().unwrap(), rest.local_addr().unwrap());
        let front = Front::new(control.clone(), SECRET, resp_addr).unwrap();
        let handlers = roll_edge::splice::Handlers::default();
        handlers.register(redis_control::cluster::PRODUCT, front.handler());
        let api = redis_control::api::router(control.clone())
            .layer(axum::middleware::from_fn_with_state(cluster.clone(), roll_cluster::accept_forwarded))
            .merge(redis_control::cluster::internal_router(control.clone()))
            .merge(roll_edge::splice::router(roll_edge::splice::Signer::new(SECRET).unwrap(), handlers));
        let tasks = vec![
            tokio::spawn(async move { axum::serve(internal, api.into_make_service_with_connect_info::<SocketAddr>()).await }).abort_handle(),
            tokio::spawn(front.serve_listener(resp, Default::default())).abort_handle(),
            tokio::spawn({
                let r = redis_control::cluster::rest_router(control.clone());
                async move { axum::serve(rest, r.into_make_service_with_connect_info::<SocketAddr>()).await }
            })
            .abort_handle(),
        ];
        TestNode { id: id.into(), control, cluster, url, resp: resp_addr, rest: rest_addr, tasks }
    }
}

fn tracing_subscriber_init() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "roll_cluster=info,redis_control=info,redis_server=info".into()))
        .with_test_writer()
        .try_init()
}

struct TestNode {
    id: String,
    control: Arc<Control>,
    cluster: Arc<Cluster>,
    /// The internal API (control routes, forwarded REST, splices).
    url: String,
    resp: SocketAddr,
    rest: SocketAddr,
    tasks: Vec<AbortHandle>,
}

impl TestNode {
    fn loaded(&self, db: &str) -> bool {
        self.control.node.get_loaded(db).is_some()
    }

    /// Die like a killed process: no handoff, no flush, nothing served from now on (its log
    /// shipper keeps running, as a stalled process's might).
    fn kill(&self) {
        self.cluster.fault_kill();
        for t in &self.tasks {
            t.abort();
        }
    }
}

async fn wait_for(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while !f() {
        if Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    true
}

/// A database on the shared catalog, and its token.
fn create(node: &TestNode, name: &str, durability: &str) -> (String, String) {
    let d = node.control.create(name, DbOptions { durability: Some(durability.into()), ..Default::default() }).unwrap();
    let token = format!("{}_{}", d.id, d.password);
    (d.id, token)
}

async fn client(at: SocketAddr, token: &str) -> RespClient {
    let mut c = RespClient::connect(at).await.unwrap();
    assert_eq!(c.cmd(["AUTH", token]).await.unwrap(), Reply::Ok);
    c
}

/// Connect as a client library reconnects: a connection that closes before AUTH is answered
/// (it reached the old owner just as the database left) is tried again. Never an error reply.
async fn reconnect(at: SocketAddr, token: &str) -> RespClient {
    for _ in 0..50 {
        let mut c = RespClient::connect(at).await.unwrap();
        match c.cmd(["AUTH", token]).await {
            Ok(Reply::Ok) => return c,
            Ok(other) => panic!("AUTH answered {other:?}: a moving database shows clients nothing but a closed connection"),
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    panic!("no connection to the database's new owner")
}

fn bulk(s: &str) -> Reply {
    Reply::Bulk(Bytes::copy_from_slice(s.as_bytes()))
}

async fn requests_through_any_node_reach_the_owner(kind: Kind) {
    let Some(env) = Env::new(kind, "route") else { return };
    let nodes = [env.node("a").await, env.node("b").await, env.node("c").await];
    assert!(wait_for(Duration::from_secs(10), || nodes.iter().all(|n| n.cluster.nodes().len() == 3)).await);
    let (id, token) = create(&nodes[0], "app", "async");
    // RESP through every node: one database, served by one of them.
    for (i, n) in nodes.iter().enumerate() {
        let mut c = client(n.resp, &token).await;
        assert_eq!(c.cmd(["INCR", "hits"]).await.unwrap(), Reply::Int(i as i64 + 1), "through {}", n.id);
    }
    let owners: Vec<&TestNode> = nodes.iter().filter(|n| n.loaded(&id)).collect();
    assert_eq!(owners.len(), 1, "loaded on exactly one node");
    let owner = owners[0];
    let other = nodes.iter().find(|n| n.id != owner.id).unwrap();
    // Pipelined commands after the AUTH that named the database arrive with it.
    let mut c = RespClient::connect(other.resp).await.unwrap();
    c.send_raw(format!("PING\r\nAUTH {token}\r\nGET hits\r\n").as_bytes()).await.unwrap();
    assert!(matches!(c.read().await.unwrap(), Reply::Error(e) if e.starts_with("NOAUTH")));
    assert_eq!(c.read().await.unwrap(), Reply::Ok);
    assert_eq!(c.read().await.unwrap(), bulk("3"));
    // REST through a node that isn't the owner.
    let http = reqwest::Client::new();
    let r = http.post(format!("http://{}/", other.rest)).bearer_auth(&token).body(r#"["INCR","hits"]"#).send().await.unwrap();
    let (status, body) = (r.status(), r.text().await.unwrap());
    assert_eq!((status.as_u16(), body.as_str()), (200, r#"{"result":4}"#));
    // The control API's view of the database is its owner's.
    let r: serde_json::Value = http.get(format!("{}/v1/redis/databases/{id}", other.url)).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["status"]["state"], "active", "{r}");
    assert!(nodes.iter().filter(|n| n.id != owner.id).all(|n| !n.loaded(&id)));
    // A read replica is served by another node than its primary, and follows it there.
    let replica = owner.control.create_replica(&id).unwrap();
    let mut c = client(owner.resp, &format!("{}_{}", replica.id, replica.password)).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while c.cmd(["GET", "hits"]).await.unwrap() != bulk("4") {
        assert!(Instant::now() < deadline, "the replica never caught up");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let at: Vec<&str> = nodes.iter().filter(|n| n.loaded(&replica.id)).map(|n| n.id.as_str()).collect();
    assert!(at.len() == 1 && at[0] != owner.id, "replica on {at:?}, primary on {}", owner.id);
}

async fn a_killed_owners_databases_move_with_every_acknowledged_write(kind: Kind) {
    let Some(env) = Env::new(kind, "kill") else { return };
    let nodes = [env.node("a").await, env.node("b").await, env.node("c").await];
    assert!(wait_for(Duration::from_secs(10), || nodes.iter().all(|n| n.cluster.nodes().len() == 3)).await);
    let (id, token) = create(&nodes[0], "orders", "sync");
    let mut c = client(nodes[0].resp, &token).await;
    for i in 0..50 {
        assert_eq!(c.cmd(["RPUSH", "log", &i.to_string()]).await.unwrap(), Reply::Int(i + 1));
    }
    let owner = nodes.iter().find(|n| n.loaded(&id)).unwrap();
    owner.kill();
    let survivor = nodes.iter().find(|n| n.id != owner.id).unwrap();
    // Through a survivor, the database comes back on another node once the lease lapses, with
    // every acknowledged write: the old owner's log was fenced and replayed.
    let started = Instant::now();
    let mut c = client(survivor.resp, &token).await;
    assert_eq!(c.cmd(["LLEN", "log"]).await.unwrap(), Reply::Int(50));
    let lease = owner.cluster.config().lease_ttl + owner.cluster.config().lease_margin;
    assert!(started.elapsed() < lease * 3, "took {:?}", started.elapsed());
    // The old owner, still running, can't make a write durable any more: its next segment hits the fence.
    let stale = owner.control.node.get_loaded(&id).unwrap();
    {
        let mut e = stale.engine(0);
        let mut cl = redis_engine::Client::new(1);
        e.call(&mut cl, &[Bytes::from_static(b"RPUSH"), Bytes::from_static(b"log"), Bytes::from_static(b"zombie")]);
    }
    assert!(wait_for(Duration::from_secs(5), || !owner.loaded(&id)).await, "the old owner drops a database taken over");
    assert_eq!(c.cmd(["RPUSH", "log", "after"]).await.unwrap(), Reply::Int(51));
}

async fn a_drain_hands_a_busy_database_over_losing_nothing(kind: Kind) {
    let Some(env) = Env::new(kind, "drain") else { return };
    let nodes = [env.node("a").await, env.node("b").await, env.node("c").await];
    assert!(wait_for(Duration::from_secs(10), || nodes.iter().all(|n| n.cluster.nodes().len() == 3)).await);
    let (id, token) = create(&nodes[0], "busy", "async");
    let mut c = client(nodes[0].resp, &token).await;
    assert_eq!(c.cmd(["PING"]).await.unwrap(), Reply::Status("PONG".into()));
    let owner = nodes.iter().find(|n| n.loaded(&id)).unwrap();
    let entry = nodes.iter().find(|n| n.id != owner.id).unwrap().resp;
    // Clients writing through a node that isn't the owner, reconnecting when a connection closes.
    let writers: Vec<_> = (0..4)
        .map(|w| {
            let token = token.clone();
            tokio::spawn(async move {
                let (mut acked, mut reconnects, mut c) = (0i64, 0, client(entry, &token).await);
                let until = Instant::now() + Duration::from_secs(3);
                while Instant::now() < until {
                    match c.cmd(["RPUSH", &format!("w{w}"), &acked.to_string()]).await {
                        Ok(Reply::Int(n)) => {
                            assert_eq!(n, acked + 1, "writer {w}: every acknowledged write is there, once");
                            acked += 1;
                        }
                        Ok(other) => panic!("writer {w}: {other:?}: a handoff shows clients nothing but a closed connection"),
                        Err(_) => {
                            reconnects += 1;
                            // An unanswered write may or may not have happened: start again from what's there.
                            c = reconnect(entry, &token).await;
                            let Reply::Int(n) = c.cmd(["LLEN", &format!("w{w}")]).await.unwrap() else { panic!() };
                            assert!(n == acked || n == acked + 1, "writer {w}: {n} after {acked} acknowledged");
                            acked = n;
                        }
                    }
                }
                (acked, reconnects)
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(500)).await;
    owner.cluster.drain(Duration::from_secs(10)).await;
    assert!(!owner.loaded(&id), "drained");
    let mut total_reconnects = 0;
    let mut c = reconnect(entry, &token).await;
    for (w, h) in writers.into_iter().enumerate() {
        let (acked, reconnects) = h.await.unwrap();
        total_reconnects += reconnects;
        assert!(acked > 0);
        assert_eq!(c.cmd(["LLEN", &format!("w{w}")]).await.unwrap(), Reply::Int(acked));
    }
    assert!(total_reconnects >= 4, "every client reconnected once ({total_reconnects})");
    let new_owner = nodes.iter().find(|n| n.loaded(&id)).unwrap();
    assert_ne!(new_owner.id, owner.id);
}

/// Two nodes that both serve a database (no cluster to tell them apart): the one that loaded it
/// last fenced the other, whose `sync` client never sees a write acknowledged again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn of_two_nodes_serving_a_database_only_the_last_to_load_it_can_write() {
    let env = Env::new(Kind::Memory, "race").unwrap();
    let start = |name: &str| {
        let mut rcfg = RedisConfig::local(env._dir.path().join(name));
        rcfg.node_name = name.into();
        rcfg.flush_interval = Duration::from_millis(50);
        rcfg.gc_interval = None;
        Control::with_store(rcfg, env.objects.clone(), Store::open(env.catalog.clone()).unwrap())
    };
    let (a, b) = (start("a").await.unwrap(), start("b").await.unwrap());
    let d = a.create("race", DbOptions { durability: Some("sync".into()), ..Default::default() }).unwrap();
    let token = format!("{}_{}", d.id, d.password);
    let serve = |c: &Arc<Control>| {
        let dir: Arc<dyn redis_server::Directory> = c.node.clone();
        async move {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            tokio::spawn(redis_server::net::serve(l, dir, Default::default()));
            addr
        }
    };
    let (at_a, at_b) = (serve(&a).await, serve(&b).await);
    let mut ca = client(at_a, &token).await;
    assert_eq!(ca.cmd(["SET", "k", "a"]).await.unwrap(), Reply::Ok);
    let mut cb = client(at_b, &token).await;
    assert_eq!(cb.cmd(["GET", "k"]).await.unwrap(), bulk("a"));
    assert_eq!(cb.cmd(["SET", "k", "b"]).await.unwrap(), Reply::Ok);
    // A's write can't reach storage past B's fence: its client gets no OK, only a closed connection.
    assert!(ca.cmd(["SET", "k", "lost"]).await.is_err());
    assert!(wait_for(Duration::from_secs(5), || a.node.get_loaded(&d.id).is_none()).await);
    assert_eq!(cb.cmd(["GET", "k"]).await.unwrap(), bulk("b"));
    let c = Control::with_store(RedisConfig { node_name: "c".into(), gc_interval: None, ..RedisConfig::local(env._dir.path().join("c")) }, env.objects.clone(), Store::open(env.catalog.clone()).unwrap())
        .await
        .unwrap();
    let mut cc = client(serve(&c).await, &token).await;
    assert_eq!(cc.cmd(["GET", "k"]).await.unwrap(), bulk("b"));
}
