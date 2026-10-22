//! roll search node: HTTP listener, tenants, auth, wake-on-request, idle unload, TLS.
//!
//! - [`node::Node`] holds the loaded clusters: it authenticates and routes requests
//!   ([`auth`]), wakes clusters from their [`catalog::StoreProvider`], enforces per-cluster
//!   limits and unloads idle clusters.
//! - [`http`] is the hyper HTTP/1.1 listener (keep-alive, pipelining, TLS with SNI routing,
//!   PROXY protocol, gzip).
//! - [`store::ObjectStores`] keeps clusters on object storage through `search-store`: metadata
//!   as cluster states, shards through [`persist`] (WAL cut records, segments, manifests).
//!   [`catalog::MemoryStores`] keeps everything in memory.
//! - [`lease`]: cluster ownership for several nodes on one bucket (epochs fence deposed nodes).
//! - [`limits`]: per-tenant rate and byte budgets and the node-wide fair scheduler.
//! - [`meta_guard`]: cluster states are saved only if they load again (no unwakeable clusters).
//! - [`process`]: allocator and kernel tuning so idle nodes stay small (`tune_memory`).
//! - `search-dev` (`src/bin`) serves one cluster without authentication for local use and the
//!   YAML test runner.
//!
//! See `docs/search/architecture.md` and `docs/search/contracts.md`.

pub mod auth;
pub mod catalog;
pub mod http;
pub mod lease;
pub mod limits;
pub mod meta_guard;
pub mod net;
pub mod node;
pub mod persist;
pub mod process;
pub mod store;

pub use catalog::{Catalog, ClusterSpec, Credential, MemoryStores, StaticCatalog, StoreProvider, secret_hash};
pub use http::{ListenOptions, Shutdown, serve, serve_until, spawn};
pub use lease::{Leases, MemoryLeases, Owner, PgLeases, SqliteLeases};
pub use node::{LifecycleHooks, Node, NodeConfig, Usage};
pub use store::ObjectStores;
