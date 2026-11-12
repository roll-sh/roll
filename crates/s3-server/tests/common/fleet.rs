//! Several storage nodes in one process, on one object store: a roll-cluster on the memory
//! backend (one `MemoryStore` the nodes share), or on DynamoDB Local when
//! `ROLL_CLUSTER_TEST_DYNAMODB` names its endpoint.

use super::*;
use roll_cluster::{Backend as ClusterBackend, Cluster, ClusterConfig, DynamoDb, DynamoDbConfig, Memory, MemoryStore};
use s3_server::cluster::{KEY, NodeConfig, Services, StorageNode, Surface};
use std::time::{Duration, Instant};

pub const CLUSTER_SECRET: &[u8] = b"storage cluster test secret 0123";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Memory,
    DynamoDb,
}

enum Leases {
    Memory(MemoryStore),
    Dynamo { endpoint: String, table: String },
}

/// One node: its cluster membership, its storage, and the S3 listener clients use.
pub struct Node {
    pub id: String,
    pub cluster: Arc<Cluster>,
    pub storage: Arc<StorageNode>,
    pub endpoint: String,
    pub port: u16,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Node {
    pub fn client(&self) -> aws_sdk_s3::Client {
        client(&self.endpoint, ACCESS, SECRET)
    }

    pub fn owns(&self) -> bool {
        self.storage.status().owner
    }

    /// Die as a killed process: no handoff, no clean-up. Its leases run out on their own.
    pub fn kill(&self) {
        self.cluster.fault_kill();
        self.storage.fault_kill();
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Called with each new owner's node id and server (to set an event sink, say).
pub type OnOwner = Arc<dyn Fn(&str, Arc<Server>) + Send + Sync>;

pub struct Fleet {
    pub nodes: Vec<Node>,
    pub backend: Arc<dyn s3_store::Backend>,
    leases: Leases,
    tweak: Arc<dyn Fn(&mut Config) + Send + Sync>,
    deliver: Option<OnOwner>,
    pub dir: tempfile::TempDir,
}

/// DynamoDB Local's endpoint, or `None` (with a message): the DynamoDB tests then skip.
pub fn dynamodb_endpoint() -> Option<String> {
    let e = std::env::var("ROLL_CLUSTER_TEST_DYNAMODB").ok().filter(|e| !e.is_empty());
    if e.is_none() {
        eprintln!("skipping: ROLL_CLUSTER_TEST_DYNAMODB isn't set (DynamoDB Local's endpoint)");
    }
    e
}

impl Fleet {
    /// `n` nodes on `kind`'s leases (`None` when DynamoDB Local isn't there).
    pub async fn start(kind: Kind, name: &str, n: usize) -> Option<Fleet> {
        Self::start_with(kind, name, n, |_| {}, None).await
    }

    /// `deliver`: called with each new owner's id and server (to set an event sink).
    pub async fn start_with(
        kind: Kind,
        name: &str,
        n: usize,
        tweak: impl Fn(&mut Config) + Send + Sync + 'static,
        deliver: Option<OnOwner>,
    ) -> Option<Fleet> {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "s3_server=info,s3_store=info,roll_cluster=info".into()))
            .with_test_writer()
            .try_init();
        let leases = match kind {
            Kind::Memory => Leases::Memory(MemoryStore::new()),
            Kind::DynamoDb => {
                let mut r = [0u8; 8];
                getrandom::fill(&mut r).unwrap();
                Leases::Dynamo { endpoint: dynamodb_endpoint()?, table: format!("s3-{name}-{}", hex::encode(r)) }
            }
        };
        let backend = s3_store::test_backend(&format!("cluster-{name}")).await;
        let mut f = Fleet { nodes: Vec::new(), backend, leases, tweak: Arc::new(tweak), deliver, dir: tempfile::tempdir().unwrap() };
        for _ in 0..n {
            f.add().await;
        }
        f.settle().await;
        Some(f)
    }

    /// Start one more node.
    pub async fn add(&mut self) -> &Node {
        let i = self.nodes.len();
        let id = format!("node-{i}");
        let api = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", api.local_addr().unwrap());
        let backend: Box<dyn ClusterBackend> = match &self.leases {
            Leases::Memory(m) => Box::new(Memory::new(m.clone())),
            Leases::Dynamo { endpoint, table } => {
                let mut c = DynamoDbConfig::new(table.clone());
                c.endpoint = Some(endpoint.clone());
                c.create_table = true;
                Box::new(tokio::task::block_in_place(|| DynamoDb::connect(c)).unwrap())
            }
        };
        let cfg = ClusterConfig::for_tests(id.clone(), url, CLUSTER_SECRET);
        let cluster = tokio::task::block_in_place(|| Cluster::join(backend, cfg)).unwrap();
        let mut store = s3_store::StoreConfig::new(self.dir.path().join(&id)).manual();
        store.node = id.clone();
        store.commit_ms = 2;
        store.fence_check = Duration::from_millis(200);
        let mut ncfg = NodeConfig::new(store);
        ncfg.follow_every = Duration::from_millis(50);
        ncfg.check_every = Duration::from_millis(500);
        ncfg.open_wait = Duration::from_secs(3);
        let mut scfg = Config { spool_dir: self.dir.path().to_path_buf(), ..Config::default() };
        (self.tweak)(&mut scfg);
        let deliver = self.deliver.clone();
        let me = id.clone();
        let open: s3_server::cluster::Open = Arc::new(move |store: Arc<s3_store::Store>| {
            let (scfg, deliver, me) = (scfg.clone(), deliver.clone(), me.clone());
            Box::pin(async move {
                store.ensure_key(ACCESS, SECRET, "admin").await?;
                let server = Server::new(store, scfg);
                if let Some(d) = deliver {
                    d(&me, server.clone());
                }
                Ok(Services { s3: s3_server::router(server), api: None, control: None })
            })
        });
        let storage = StorageNode::start(cluster.clone(), self.backend.clone(), ncfg, open).await.unwrap();
        let internal = storage.internal_routes().merge(cluster.router());
        let s3 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = s3.local_addr().unwrap().port();
        let front = storage.router(Surface::S3);
        let tasks = vec![
            tokio::spawn(async move { axum::serve(api, internal.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap() }),
            tokio::spawn(async move { axum::serve(s3, front.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap() }),
        ];
        self.nodes.push(Node { id, cluster, storage, endpoint: format!("http://127.0.0.1:{port}"), port, tasks });
        self.nodes.last().unwrap()
    }

    /// Wait until the node placement prefers serves the metadata (nodes that joined after the
    /// first took it are handed it by rebalancing).
    pub async fn settle(&self) {
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            let preferred = self.nodes[0].cluster.preferred(KEY).map(|n| n.id);
            if let Some(o) = self.owner()
                && preferred.as_deref() == Some(self.nodes[o].id.as_str())
                && self.nodes.iter().all(|n| n.cluster.nodes().len() == self.nodes.len())
            {
                return;
            }
            assert!(Instant::now() < until, "the metadata didn't settle on its preferred node");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Tenures each node started so far.
    pub fn tenures(&self) -> Vec<u64> {
        self.nodes.iter().map(|n| n.storage.status().tenures).collect()
    }

    /// The node that owns the metadata and serves it.
    pub fn owner(&self) -> Option<usize> {
        let owners: Vec<usize> = (0..self.nodes.len()).filter(|i| self.nodes[*i].owns()).collect();
        (owners.len() == 1).then(|| owners[0])
    }

    /// Wait (up to 20 s) for exactly one node to serve the metadata.
    pub async fn wait_owner(&self) -> usize {
        self.wait_owner_not(None).await
    }

    /// … one that isn't `old`.
    pub async fn wait_owner_not(&self, old: Option<usize>) -> usize {
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(o) = self.owner().filter(|o| Some(*o) != old) {
                return o;
            }
            assert!(Instant::now() < until, "no node owns the metadata");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A node that doesn't own the metadata, other than `except` (nodes killed or drained).
    pub fn non_owner(&self, except: &[usize]) -> usize {
        let o = self.owner();
        (0..self.nodes.len()).find(|i| Some(*i) != o && !except.contains(i)).expect("a node that isn't the owner")
    }

    /// Drain the owner (a planned stop) and wait for another node to serve the metadata.
    pub async fn drain_owner(&self) -> (usize, usize) {
        let old = self.wait_owner().await;
        self.nodes[old].cluster.drain(Duration::from_secs(10)).await;
        let new = self.wait_owner_not(Some(old)).await;
        (old, new)
    }

    /// The owner's store.
    pub fn store(&self) -> Arc<s3_store::Store> {
        self.nodes[self.owner().expect("an owner")].storage.store().expect("the owner's store")
    }

    /// A test server view of node `i`: its endpoint and client, with the owner's store.
    pub fn server(&self, i: usize) -> TestServer {
        let n = &self.nodes[i];
        TestServer { store: self.store(), endpoint: n.endpoint.clone(), port: n.port, client: n.client(), dir: tempfile::tempdir().unwrap() }
    }
}

/// Wait until `f` holds (up to `secs`).
pub async fn eventually(secs: u64, what: &str, mut f: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub fn lease_key() -> &'static str {
    KEY
}
