//! Serverless Redis control plane.
//!
//! Databases are served by a multi-tenant [`redis_server::node::Node`]: loaded from object
//! storage on the first connection, unloaded when idle. This crate owns what exists (SQLite),
//! credentials, branches and point-in-time restores, replicas, metering and pricing.

pub mod api;
pub mod control;
pub mod store;

pub use control::{Control, ControlError, DbOptions, RedisConfig};
