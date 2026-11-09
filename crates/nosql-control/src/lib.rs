//! roll nosql's control plane.
//!
//! Databases are served by a multi-tenant [`nosql_server::Node`]: loaded from object storage
//! on the first connection, unloaded when idle. This crate owns what exists (SQLite), the
//! platform credentials, branches (with reset and expiry), point-in-time restores, the schema
//! diff, imports, metering and pricing, and the `/v1/nosql` API (docs/nosql/contracts.md).
//!
//! - [`store`]: SQLite (`<home>/nosql.db`): databases, imports, usage samples.
//! - [`control`]: [`Control`] — lifecycle, branches, credentials, console, metering, loops.
//! - [`settings`]: the `cassandra.yaml` / `scylla.yaml` overrides of a database.
//! - [`diff`]: a branch's schema difference against its parent, as CQL.
//! - [`import`]: `cql://` live imports with write times and TTLs.
//! - [`import_sstables`]: `s3://` / `file://` imports of Cassandra and ScyllaDB SSTable backups.
//! - [`pricing`]: the cost estimate and the managed alternatives.
//! - [`api`]: the HTTP routes.

pub mod api;
pub mod control;
pub mod diff;
pub mod import;
pub mod import_sstables;
pub mod pricing;
pub mod settings;
pub mod store;

pub use control::{Control, ControlError, NosqlConfig};
