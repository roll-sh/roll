//! roll search node: HTTP listener, tenants, auth, wake-on-request, idle unload, TLS.
//!
//! - [`node::Node`] holds the loaded clusters: it authenticates and routes requests
//!   ([`auth`]), wakes clusters from their [`catalog::StoreProvider`], enforces per-cluster
//!   limits and unloads idle clusters.
//! - [`http`] is the hyper HTTP/1.1 listener (keep-alive, pipelining, TLS with SNI routing,
//!   PROXY protocol, gzip).
//! - [`store::ObjectStores`] keeps cluster metadata on object storage through `search-store`
//!   ([`catalog::MemoryStores`] keeps it in memory).
//! - `search-dev` (`src/bin`) serves one cluster without authentication for local use and the
//!   YAML test runner.
//!
//! See `docs/search/architecture.md` and `docs/search/contracts.md`.

pub mod auth;
pub mod catalog;
pub mod http;
pub mod net;
pub mod node;
pub mod store;

pub use catalog::{Catalog, ClusterSpec, Credential, MemoryStores, StaticCatalog, StoreProvider};
pub use http::{ListenOptions, serve, spawn};
pub use node::{LifecycleHooks, Node, NodeConfig};
pub use store::ObjectStores;
