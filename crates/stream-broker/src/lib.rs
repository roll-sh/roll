//! roll stream: a Kafka-compatible broker with bottomless object storage, multi-tenant streams
//! and a connectionless HTTP data plane.

pub mod auth;
pub mod broker;
pub mod compaction;
pub mod config;
pub mod error;
pub mod groups;
pub mod handlers;
pub mod http;
pub mod ids;
pub mod partitioner;
pub mod records;
pub mod server;
pub mod store;
pub mod tls;
pub mod topic_config;
pub mod txn;

pub use broker::Broker;
pub use config::Config;
