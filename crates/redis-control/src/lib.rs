//! Serverless Redis control plane.
//!
//! Databases are served by a multi-tenant [`redis_server::node::Node`]: loaded from object
//! storage on the first connection, unloaded when idle. This crate owns what exists (the catalog:
//! SQLite on one node, the shared system database in a cluster),
//! credentials, branches and point-in-time restores, replicas, metering and pricing, and where each
//! database is served in a cluster ([`cluster`]).

pub mod api;
pub mod cluster;
pub mod control;
pub mod store;

pub use control::{Control, ControlError, DbOptions, RedisConfig};
