//! roll queues AMQP 1.0 server, compatible with RabbitMQ 4's native AMQP 1.0 support.
//!
//! - SASL PLAIN and ANONYMOUS (only when [`Config::anonymous_user`] is set).
//! - v2 addresses (`/exchanges/:x/:key`, `/queues/:q`, the anonymous terminus with a
//!   per-message `to`) and the v1 addresses of the 3.x plugin (`/exchange/x/key`,
//!   `/queue/q`, `/topic/key`, `/amq/queue/q`).
//! - Outcomes map as RabbitMQ 4 does: accepted → ack, released → requeue without counting a
//!   delivery attempt, rejected → dead-letter or drop, modified → requeue (delivery-failed)
//!   or reject (undeliverable-here).
//! - The `/management` node (HTTP over AMQP) declares, deletes, purges and binds.
//!
//! The listener shares the 0-9-1 port: [`handoff`] plugs into `mq_amqp::ServerConfig::amqp10`.

mod address;
mod conn;
mod frames;
mod message;
mod mgmt;
mod session;
mod types;

pub use conn::{AMQP_HEADER, Config, SASL_HEADER, handoff, serve, serve_connection, serve_listener};
