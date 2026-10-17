//! Serverless Postgres control plane.
//!
//! Projects own branches; each branch has a data directory and a read-write endpoint whose
//! compute is started on the first connection, resized live by CPU load, and stopped when idle.

pub mod api;
pub mod compute;
pub mod config;
pub mod control;
pub mod history;
pub mod ids;
pub mod reconciler;
pub mod store;

pub use config::Config;
pub use control::Control;
