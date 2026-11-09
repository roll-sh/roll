//! ScyllaDB Alternator: the DynamoDB API, as ScyllaDB 2026.3 implements it.
//!
//! [`Alternator`] serves DynamoDB requests over a [`Backend`] that stores tables and items.
//! Everything clients see is decided here: the HTTP layer ([`http`]), SigV4 ([`auth`]),
//! request validation, the expression languages ([`expr`]), the legacy parameters
//! ([`legacy`]), capacity accounting ([`size`]), pagination and Streams. ScyllaDB's behaviour
//! differs from DynamoDB's in many places (messages, number equality, what is validated);
//! where they differ this crate follows ScyllaDB, as recorded by the golden files in
//! `tests/golden/` (captured from `scylladb/scylla:2026.3.3` with `tests/golden/capture.py`).
//!
//! # The seam
//!
//! [`Backend`] is the storage interface. [`memory::MemoryBackend`] implements it in memory for
//! tests; [`engine::EngineBackend`] implements it on the CQL engine, storing table `T` as the
//! CQL table `alternator_T.T` exactly as ScyllaDB does (see [`storage`] for the column layout
//! and the byte encoding of `:attrs`, and [`engine`] for the rest), so CQL clients read and
//! write Alternator tables 1:1:
//!
//! - **Catalog.** One keyspace `alternator_T` per table (NetworkTopologyStrategy, tablets).
//!   [`schema::TableSchema`] maps to the CQL schema plus the `scylla_tags` extension, which
//!   holds user tags and `system:` tags (`system:table_creation_time`, `system:ttl_attribute`,
//!   `system:write_isolation`, `system:initial_tablets`). A GSI is the materialized view
//!   `"T:gsi"` keyed by (index hash, [index range], remaining base keys); an LSI is the view
//!   `"T!:lsi"` keyed by (base hash, index range, base range) with `synchronous_updates`.
//!   Index key columns that are not base keys are computed columns extracted from `:attrs`
//!   (`{"type":"alternator_extract_from_attrs","attr_name":"x","desired_type":"S"}`). Streams
//!   are CDC on the base table.
//! - **Items.** [`Backend::write_item`] is a read-modify-write under the item's lock (the
//!   partition's LWT lock in the engine), applied with the engine's write observers so views and
//!   the CDC log follow atomically.
//! - **Reads.** [`Backend::read_range`] is a single-partition slice (Query) or a token-range
//!   scan (Scan segments), with a row limit and a byte budget, resuming after a position.
//! - **Streams** are read back from the CDC log, one shard per CDC stream; **TTL** is a
//!   background expiration scanner; **internal tables** are the engine's system tables; SigV4
//!   secrets are the roles' salted hashes ([`engine::auth::RoleCredentials`]).
//!
//! `nosql-server` serves it (`nosql_server::alternator`): routing by `Host` or access key,
//! wake-on-request, metrics.
//!
//! # Not implemented by ScyllaDB (and so not here)
//!
//! TransactWriteItems / TransactGetItems, PartiQL, backups, global tables, Kinesis, import and
//! export, contributor insights, resource policies: all answer `UnknownOperationException`.

pub mod audit;
pub mod auth;
pub mod backend;
pub mod engine;
pub mod error;
pub mod expr;
pub mod hashorder;
pub mod http;
pub mod json;
pub mod legacy;
pub mod memory;
pub mod metrics;
pub mod number;
pub mod ops;
pub mod schema;
pub mod server;
pub mod service;
pub mod size;
pub mod storage;
pub mod trace;
pub mod value;

pub use backend::Backend;
pub use error::{Error, ErrorKind, Result};
pub use service::{Alternator, Config, WriteIsolation};
