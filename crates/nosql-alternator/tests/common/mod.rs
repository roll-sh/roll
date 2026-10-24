//! Shared helpers for the integration tests: a capped allocator, a server on a random port,
//! and an SDK client for it.
#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::net::SocketAddr;
use std::sync::Arc;

use aws_sdk_dynamodb::config::{BehaviorVersion, Credentials, Region};
use nosql_alternator::auth::{RoleSecret, StaticCredentials};
use nosql_alternator::memory::MemoryBackend;
use nosql_alternator::{Alternator, Config, server};

/// Fails any single allocation over 1 GiB, as Linux does, so a buffer sized from a peer's count
/// aborts the tests on every platform (macOS would hand it out lazily).
struct Capped;

const MAX_ALLOC: usize = 1 << 30;

unsafe impl GlobalAlloc for Capped {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() > MAX_ALLOC {
            std::ptr::null_mut()
        } else {
            unsafe { System.alloc(layout) }
        }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.size() > MAX_ALLOC {
            std::ptr::null_mut()
        } else {
            unsafe { System.alloc_zeroed(layout) }
        }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > MAX_ALLOC {
            std::ptr::null_mut()
        } else {
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Capped = Capped;

pub const ROLE: &str = "alice";
pub const SECRET: &str = "$6$saltsalt$hashedpassword";

/// A server on 127.0.0.1 with the in-memory backend; `auth` enforces SigV4 for role `alice`
/// (and `bob`, who cannot log in).
/// Which backend a test server stores tables in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Memory,
    /// The engine (a fresh database on temp-dir storage).
    Engine,
}

pub async fn backend(kind: Kind) -> Arc<dyn nosql_alternator::Backend> {
    match kind {
        Kind::Memory => Arc::new(MemoryBackend::new()),
        Kind::Engine => {
            let (db, store) = engine_db("e2e").await;
            // The store outlives the test's server.
            std::mem::forget(store);
            Arc::new(nosql_alternator::engine::EngineBackend::new(db))
        }
    }
}

pub async fn start(auth: bool) -> (SocketAddr, Arc<Alternator>) {
    start_on(Kind::Memory, auth).await
}

/// [`start`] on the given backend.
pub async fn start_on(kind: Kind, auth: bool) -> (SocketAddr, Arc<Alternator>) {
    let config = Config {
        enforce_authorization: auth,
        ..Config::default()
    };
    let mut alt = Alternator::new(backend(kind).await, config);
    if auth {
        alt = alt.with_credentials(Arc::new(StaticCredentials(vec![
            (
                ROLE.into(),
                RoleSecret {
                    salted_hash: SECRET.into(),
                    can_login: true,
                },
            ),
            (
                "bob".into(),
                RoleSecret {
                    salted_hash: "bobsecret".into(),
                    can_login: false,
                },
            ),
        ])));
    }
    let alt = Arc::new(alt);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(server::serve(listener, alt.clone()));
    (addr, alt)
}

/// An SDK client signing as `access_key` / `secret`.
pub fn client_as(addr: SocketAddr, access_key: &str, secret: &str) -> aws_sdk_dynamodb::Client {
    let conf = aws_sdk_dynamodb::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .endpoint_url(format!("http://{addr}"))
        .credentials_provider(Credentials::new(access_key, secret, None, None, "test"))
        .retry_config(aws_sdk_dynamodb::config::retry::RetryConfig::disabled())
        .build();
    aws_sdk_dynamodb::Client::from_conf(conf)
}

pub fn client(addr: SocketAddr) -> aws_sdk_dynamodb::Client {
    client_as(addr, ROLE, SECRET)
}

/// A fresh engine database (ScyllaDB dialect) on temp-dir storage, with its store.
pub async fn engine_db(
    id: &str,
) -> (
    std::sync::Arc<nosql_engine::Database>,
    std::sync::Arc<nosql_storage::store::Store>,
) {
    let cfg = nosql_storage::store::StoreConfig {
        memtable_budget_bytes: 64 << 20,
        auto_compaction: false,
        maintenance_interval: std::time::Duration::from_secs(3600),
        manifest_interval: std::time::Duration::from_secs(3600),
        ..Default::default()
    };
    let store = nosql_storage::store::Store::start(nosql_storage::test_objects("alternator"), cfg)
        .await
        .unwrap();
    let handle = store.open(id, Default::default()).await.unwrap();
    let db = nosql_engine::Database::open(
        nosql_engine::DbConfig::new(id, nosql_engine::Dialect::Scylla),
        handle,
    )
    .await
    .unwrap();
    (db, store)
}
