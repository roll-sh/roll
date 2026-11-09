//! roll queues broker engine.
//!
//! Protocol front ends (`mq-amqp` for AMQP 0-9-1, plus AMQP 1.0, MQTT, STOMP and the stream
//! protocol) translate their wire formats into calls on a [`Broker`]: declare and bind
//! topology, publish through exchanges, and consume from queues. Queues are actors over a
//! [`log::Log`], so the same engine runs on a laptop (local segment files) or on stateless
//! brokers whose logs are quorum-replicated across availability zones (`mq-storage`).

pub mod auth;
pub mod broker;
pub mod cluster;
pub mod deadletter;
pub mod delayed;
pub mod error;
pub mod exchange;
pub mod log;
pub mod message;
pub mod meta;
pub mod policy;
pub mod queue;
pub mod transport;
pub mod value;
pub mod vhost;
pub mod wal;

pub use broker::{AwakeGuard, Broker, BrokerConfig, ImportOptions, ImportReport, Skipped};
pub use error::{AmqpError, Result};
pub use message::{Message, MessageRef, Properties};
pub use value::{FieldTable, FieldValue};
pub use vhost::VHost;

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A random u64 (ids, salts, sampling).
pub fn rand_u64() -> u64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("random");
    u64::from_le_bytes(b)
}
