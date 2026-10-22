//! roll nosql's data plane: the CQL listeners, tenancy and scale to zero, and the HTTP APIs.
//!
//! - [`net`]: TCP/TLS listeners with SNI, the PROXY protocol and ScyllaDB's shard-aware port.
//! - [`conn`]: one CQL connection — the native-protocol state machine of each reference
//!   (handshake, compression, authentication, events, multiplexed requests).
//! - [`directory`]: routing a connection to a database ([`Directory`]), leases that keep a
//!   database loaded, per-database [`Metrics`]; [`Single`] serves one database.
//! - [`node`]: the multi-tenant [`Node`]: [`Catalog`] lookups, loading through
//!   `nosql_storage::store::Store` + `nosql_engine::Database`, idle unload, evict.
//! - [`convert`]: engine ↔ protocol types.
//! - [`http`]: the dashboard's data API (CQL over JSON, schema, table pages).
//! - [`rest`]: the subset of ScyllaDB's REST API that `nodetool` and test suites use.
//! - [`alternator`]: the DynamoDB-compatible API (wired up with `nosql-alternator`).
//! - [`metrics`]: ScyllaDB's Prometheus endpoint (`:9180/metrics`) in single mode.

pub mod alternator;
pub mod conn;
pub mod convert;
pub mod directory;
pub mod http;
pub mod metrics;
pub mod net;
pub mod node;
pub mod rest;

pub use directory::{Directory, Lease, Loaded, Metrics, MetricsSnapshot, ServerSettings, Single};
pub use net::ListenOptions;
pub use node::{Catalog, Credential, DbSpec, Node, NodeConfig, StaticCatalog};

use std::net::SocketAddr;
use std::sync::Arc;

/// Serve one database on an ephemeral local port (tests, embedding). Returns its address.
pub async fn start_single(db: Arc<nosql_engine::Database>) -> anyhow::Result<SocketAddr> {
    let dir: Arc<dyn Directory> = Arc::new(Single::new(db));
    net::start(dir, ListenOptions::default()).await
}
