//! roll's Postgres-compatible database engine. See DESIGN.md.

// Row tuples and executor entry points carry many parts by nature.
#![allow(clippy::type_complexity, clippy::too_many_arguments)]

pub mod auth;
pub mod catalog;
pub mod engine;
pub mod error;
pub mod exec;
pub mod largeobj;
pub mod notify;
pub mod plpgsql;
pub mod server;
pub mod session;
pub mod sort;
pub mod sql;
pub mod stack;
pub mod storage;
pub mod txn;
pub mod types;

pub use error::{Error, Result};
