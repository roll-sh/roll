//! Bottomless storage for Postgres: object storage is the source of truth, local disk is a cache.

pub mod lsn;
pub mod objects;
pub mod receiver;
pub mod repl;

pub use lsn::Lsn;
pub use objects::{Objects, StorageConfig};
