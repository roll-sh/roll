//! roll tasks: durable HTTP messages, queues, schedules, events and workflows.
//!
//! - **Messages** are delivered to HTTP endpoints with retries, backoff, delays, callbacks,
//!   deduplication and a dead-letter queue, signed with Standard Webhooks.
//! - **Queues** add parallelism, rate limits, FIFO groups, pausing and pull consumers.
//! - **Topics** fan a message out to many endpoints. **Schedules** fire on cron or intervals.
//! - **Workflows**: events start durable functions whose steps are memoised, can sleep, wait
//!   for events, invoke each other, and are retried, throttled, debounced and batched.
//!
//! Everything is a message in a sharded due index, worked by any number of stateless workers.

pub mod api;
pub mod cluster;
pub mod cost;
pub mod dispatch;
pub mod engine;
pub mod expr;
pub mod flow;
pub mod ids;
pub mod model;
pub mod pull;
pub mod schedule;
pub mod restore;
pub mod schedules;
pub mod sign;
pub mod tenants;
pub mod store;
pub mod workflow;

pub use dispatch::{Worker, start};
pub use engine::{Config, PublishRequest, TaskError, Tasks};
