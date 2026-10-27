//! Servers on random ports over temp-dir object storage, for integration tests.
#![allow(dead_code)]

pub mod docker;

use std::alloc::{GlobalAlloc, Layout, System};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use nosql_engine::{Database, DbConfig, Dialect};
use nosql_proto::client::{Client, ClientConfig};
use nosql_proto::{Compression, Version};
use nosql_server::{Directory, ListenOptions, Node, NodeConfig, Single, StaticCatalog, net};
use nosql_storage::store::{Store, StoreConfig};

/// Fails any single allocation over 1 GiB, as Linux does, so a buffer sized from a peer's count
/// aborts the tests on every platform (macOS would hand it out lazily).
struct Capped;

const MAX_ALLOC: usize = 1 << 30;

unsafe impl GlobalAlloc for Capped {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.alloc(layout) } }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.size() > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.alloc_zeroed(layout) } }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.realloc(ptr, layout, new_size) } }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Capped = Capped;

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_test_writer().try_init();
}

/// Reference images (bump with docs/nosql/compatibility.md and the CI workflow).
pub const SCYLLA_IMAGE: &str = "scylladb/scylla:2026.3.3";
pub const CASSANDRA_IMAGE: &str = "cassandra:5.0.9";

/// A node's storage on a fresh temp directory (or S3 with `ROLL_E2E_S3_BUCKET`).
pub async fn store(name: &str) -> Arc<Store> {
    let cfg = StoreConfig { maintenance_interval: Duration::from_millis(200), ..Default::default() };
    Store::start(nosql_storage::test_objects(name), cfg).await.unwrap()
}

/// An engine database on its own store.
pub async fn database(id: &str, dialect: Dialect) -> (Arc<Database>, Arc<Store>) {
    database_with(DbConfig::new(id, dialect)).await
}

pub async fn database_with(cfg: DbConfig) -> (Arc<Database>, Arc<Store>) {
    let store = store(&cfg.id).await;
    let handle = store.open(&cfg.id, Default::default()).await.unwrap();
    (Database::open(cfg, handle).await.unwrap(), store)
}

pub struct SingleServer {
    pub addr: SocketAddr,
    pub db: Arc<Database>,
    pub dir: Arc<Single>,
    pub store: Arc<Store>,
}

/// One database served alone (AllowAll unless `f` says otherwise).
pub async fn single(dialect: Dialect) -> SingleServer {
    single_with(dialect, |_| {}, |_| {}, |_| {}).await
}

pub async fn single_with(
    dialect: Dialect,
    cfg: impl FnOnce(&mut DbConfig),
    spec: impl FnOnce(&mut nosql_server::DbSpec),
    settings: impl FnOnce(&mut nosql_server::ServerSettings),
) -> SingleServer {
    let mut c = DbConfig::new("db1", dialect);
    cfg(&mut c);
    let (db, store) = database_with(c).await;
    let mut single = Single::with_spec(db.clone(), spec);
    settings(single.settings_mut());
    let dir = Arc::new(single);
    let addr = net::start(dir.clone() as Arc<dyn Directory>, ListenOptions::default()).await.unwrap();
    SingleServer { addr, db, dir, store }
}

pub struct NodeServer {
    pub addr: SocketAddr,
    pub node: Arc<Node>,
    pub catalog: Arc<StaticCatalog>,
    pub store: Arc<Store>,
}

pub async fn node(cfg: NodeConfig, opts: ListenOptions) -> NodeServer {
    let store = store("node").await;
    let catalog = Arc::new(StaticCatalog::default());
    let node = Node::new(catalog.clone(), store.clone(), cfg);
    let addr = net::start(node.clone() as Arc<dyn Directory>, opts).await.unwrap();
    NodeServer { addr, node, catalog, store }
}

pub fn cfg(addr: SocketAddr, v: Version) -> ClientConfig {
    let mut c = ClientConfig::new(addr.to_string()).version(v);
    c.request_timeout = Some(Duration::from_secs(20));
    c
}

pub async fn connect(addr: SocketAddr, v: Version, compression: Option<Compression>) -> Client {
    Client::connect(cfg(addr, v).compression(compression)).await.unwrap()
}

/// `int` cells.
pub fn int(i: i32) -> bytes::Bytes {
    bytes::Bytes::copy_from_slice(&i.to_be_bytes())
}

pub fn text(s: &str) -> bytes::Bytes {
    bytes::Bytes::copy_from_slice(s.as_bytes())
}

/// A raw connection for byte-level probes.
pub struct Raw {
    pub sock: tokio::net::TcpStream,
    buf: bytes::BytesMut,
}

/// A response envelope as received.
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub version: u8,
    pub flags: u8,
    pub stream: i16,
    pub opcode: u8,
    pub body: bytes::Bytes,
}

impl RawFrame {
    /// The ERROR's code and message.
    pub fn error(&self) -> Option<(i32, String)> {
        if self.opcode != 0 {
            return None;
        }
        let b = &self.body;
        let code = i32::from_be_bytes(b.get(..4)?.try_into().ok()?);
        let n = u16::from_be_bytes(b.get(4..6)?.try_into().ok()?) as usize;
        Some((code, String::from_utf8_lossy(b.get(6..6 + n)?).into_owned()))
    }
}

/// An envelope: version byte, flags, stream, opcode, body.
pub fn envelope(version: u8, flags: u8, stream: i16, opcode: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![version, flags];
    v.extend_from_slice(&stream.to_be_bytes());
    v.push(opcode);
    v.extend_from_slice(&(body.len() as u32).to_be_bytes());
    v.extend_from_slice(body);
    v
}

/// A `[string map]` STARTUP body.
pub fn startup_body(opts: &[(&str, &str)]) -> Vec<u8> {
    let mut v = (opts.len() as u16).to_be_bytes().to_vec();
    for (k, val) in opts {
        v.extend_from_slice(&(k.len() as u16).to_be_bytes());
        v.extend_from_slice(k.as_bytes());
        v.extend_from_slice(&(val.len() as u16).to_be_bytes());
        v.extend_from_slice(val.as_bytes());
    }
    v
}

/// A v3/v4 QUERY body: long string, consistency ONE, flags.
pub fn query_body(q: &str, flags: u8) -> Vec<u8> {
    let mut v = (q.len() as u32).to_be_bytes().to_vec();
    v.extend_from_slice(q.as_bytes());
    v.extend_from_slice(&[0, 1, flags]);
    v
}

impl Raw {
    pub async fn connect(addr: SocketAddr) -> Raw {
        let sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        Raw { sock, buf: bytes::BytesMut::new() }
    }

    pub async fn send(&mut self, b: &[u8]) {
        use tokio::io::AsyncWriteExt;
        self.sock.write_all(b).await.unwrap();
    }

    /// The next v3+ (9-byte header) or v1/v2 (8-byte header, `short`) envelope; `None` on close
    /// or after 2 s.
    pub async fn read_as(&mut self, short: bool) -> Option<RawFrame> {
        use tokio::io::AsyncReadExt;
        let hl = if short { 8 } else { 9 };
        loop {
            if self.buf.len() >= hl {
                let len = u32::from_be_bytes(self.buf[hl - 4..hl].try_into().unwrap()) as usize;
                if self.buf.len() >= hl + len {
                    let h = self.buf.split_to(hl);
                    let body = self.buf.split_to(len).freeze();
                    let stream = if short { h[2] as i8 as i16 } else { i16::from_be_bytes([h[2], h[3]]) };
                    return Some(RawFrame { version: h[0], flags: h[1], stream, opcode: h[hl - 5], body });
                }
            }
            let mut chunk = [0u8; 8192];
            match tokio::time::timeout(Duration::from_secs(2), self.sock.read(&mut chunk)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return None,
                Ok(Ok(n)) => self.buf.extend_from_slice(&chunk[..n]),
            }
        }
    }

    pub async fn read(&mut self) -> Option<RawFrame> {
        self.read_as(false).await
    }

    /// Whether the server closed the connection (EOF within 2 s).
    pub async fn closed(&mut self) -> bool {
        use tokio::io::AsyncReadExt;
        let mut chunk = [0u8; 64];
        matches!(tokio::time::timeout(Duration::from_secs(2), self.sock.read(&mut chunk)).await, Ok(Ok(0)) | Ok(Err(_)))
    }

    /// OPTIONS + STARTUP at `v` (`0x04` …), expecting READY.
    pub async fn ready(addr: SocketAddr, v: u8) -> Raw {
        let mut r = Raw::connect(addr).await;
        r.send(&envelope(v, 0, 1, 1, &startup_body(&[("CQL_VERSION", "3.0.0")]))).await;
        let f = r.read().await.expect("READY");
        assert_eq!(f.opcode, 2, "{f:?}");
        r
    }
}
