//! A Redis-compatible data engine.
//!
//! [`Engine`] holds one shard: 16 logical databases, with every Redis data type, expiry,
//! eviction, transactions, pub/sub hooks, blocking operations and scripting. It is pure and
//! synchronous; networking, tenancy, persistence and replication live in `redis-server`.

pub mod acl;
pub mod bloom;
pub mod client;
pub mod clients;
pub mod commands;
pub mod config;
pub mod cx;
pub mod db;
pub mod decimal;
pub mod dict;
pub mod engine;
pub mod hub;
pub mod json;
pub mod latency;
pub mod modules;
pub mod musl_math;
pub mod notify;
pub mod object;
pub mod pack;
pub mod rdb;
pub mod search;
pub mod tre;
pub mod ts;
pub mod types;
pub mod util;
pub mod value;
pub mod vset;

pub use client::Client;
pub use config::Config;
pub use engine::{Effect, Engine, Mode, Outcome};
pub use hub::{Hub, Sink};
pub use redis_proto::Reply;
pub use value::{Entry, Kind, Str, Value};
