//! roll nosql's engine: one [`Database`] per tenant — schema (persisted in `system_schema`),
//! system and virtual tables, CQL preparation and execution, functions, dialects — behind the
//! facade the server uses: [`Database`], [`Session`], [`Response`], [`Error`].
//!
//! A request goes parse ([`nosql_cql`]) → prepare ([`prepare`], [`statements`]) → authorize
//! ([`hooks::Authorizer`]) → execute. See `README.md` for the module map and how to extend it.

// `Error` carries the protocol error body (code, message, typed detail, warnings) the server
// encodes; it is returned on cold paths only, and its fields are part of the facade.
#![allow(clippy::result_large_err)]

pub mod audit;
pub mod auth;
pub mod cdc;
pub mod config;
pub mod cpp;
pub mod database;
pub mod describe;
pub mod error;
pub mod functions;
pub mod guardrails;
pub mod hooks;
pub mod index;
pub mod limits;
pub mod live;
pub mod lwt;
pub mod masking;
pub mod messages;
pub mod mutation_fragments;
pub mod prepare;
pub mod rate_limit;
pub mod response;
pub mod rowttl;
pub mod schema;
pub mod session;
pub mod statements;
pub mod storage;
pub mod system;
pub mod trace;
pub mod udf;
pub mod uuid;
pub mod views;

#[cfg(test)]
mod tests;

pub use config::{DbConfig, Settings, StrictAllowFiltering};
pub use database::{Database, StoreHandle};
pub use error::{Consistency, Error, ErrorCode, ErrorDetail, Result};
pub use nosql_types::Dialect;
pub use response::{
    Batch, BatchKind, BatchStatement, ChangeTarget, ChangeType, ClientInfo, ColumnSpec, Event,
    Param, Prepared, QueryParams, Response, ResponseBody, ResultMetadata, Rows, SchemaChange,
};
pub use session::{AuthStep, Session, SessionState};
