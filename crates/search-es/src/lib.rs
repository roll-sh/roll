//! Elasticsearch REST semantics for roll search: mappings, settings, Query DSL, document, search and admin APIs.
//!
//! A request flows through [`rest::handle`]: media-type negotiation, routing on the trie built
//! from the vendored REST spec ([`routes`]), parameter checks, the API handler ([`api`]), then
//! rendering ([`xcontent`]: JSON/YAML/CBOR, `filter_path`, `pretty`) with Elasticsearch's headers.
//! Handlers read and update one tenant's metadata through [`cluster::Cluster`], whose state
//! ([`meta`]: indices, [`mapping`]s, settings, aliases, templates, scripts, pipelines) is an
//! immutable snapshot replaced on every change, persisted through a [`cluster::MetaStore`]
//! before it is published. [`json`] is the ordered JSON model with Jackson's number semantics and
//! error texts; [`error::EsError`] renders exceptions exactly as Elasticsearch does.
//!
//! Correctness is checked against Elasticsearch 9.5.5 with captured fixtures
//! (`tests/fixtures/rest`, `tests/fixtures/dynamic_mapping.json`, `tests/fixtures/mapping_*`).
//! See `docs/search/architecture.md` and `docs/search/contracts.md`.

pub mod analysis;
pub mod api;
pub mod breaker;
pub mod cluster;
pub mod document;
pub mod engine;
pub mod error;
pub mod failure_store;
pub mod fetch;
pub mod ingest;
pub mod join;
pub mod json;
pub mod mapping;
pub mod meta;
pub mod opensearch;
pub mod query;
pub mod remote;
pub mod rest;
pub mod routes;
pub mod scripting;
pub mod search;
pub mod search_source;
pub mod stack;
pub mod tasks;
pub mod thread_pool;
pub mod time;
pub mod tsds;
pub mod units;
pub mod version;
pub mod write;
pub mod xcontent;
pub mod xpack;

pub use cluster::{Cluster, ClusterConfig, Dialect, MemoryStore, MetaStore};
pub use error::{EsError, EsResult};
pub use rest::{EsRequest, EsResponse, handle};
