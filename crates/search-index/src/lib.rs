//! roll search's shard engine: indexing buffer, refresh, flush, merges, queries, BM25 scoring,
//! collectors and highlighting — one shard of an Elasticsearch-compatible index.
//!
//! The engine knows nothing about JSON or mappings: `search-es` turns documents into
//! [`IndexableDoc`]s against a [`Schema`] of numbered fields, and the Query DSL into [`Query`]
//! trees. Modules, bottom up:
//!
//! - [`schema`], [`doc`]: the field model and documents.
//! - [`buffer`]: the per-thread in-memory segment (term hash, byte-slice postings) that the
//!   codec writes as a segment.
//! - [`shard`]: concurrent writes with Elasticsearch versioning, the live version map and
//!   realtime get, refresh (point-in-time [`Searcher`]s), flush (commit points for the store),
//!   merges ([`merge`]: tiered policy, BP reordering, index sorting), segments opened while
//!   still downloading (`shard::gate`).
//! - [`search`]: the Query → Weight → Scorer → Collector model, concurrent search (segments
//!   and intra-segment partitions on an on-demand worker pool) and the filter cache; [`similarity`]: BM25 and the other Lucene similarities with Lucene's
//!   explanations.
//! - [`query`]: every query type (term, multi-term, boolean, phrases and phrase prefixes, spans,
//!   intervals, ranges — index-sorted ones by doc id range —, function score, kNN, nested and
//!   parent/child joins, geo points (index and doc values) and shapes (index and doc values),
//!   cartesian points and shapes, more-like-this, terms
//!   set, rank features, BM25F combined fields, distance features, scripts, capped scores,
//!   binary-doc-value confirmation, `match_only_text` source confirmation); [`collector`]: top
//!   hits, sorting (fields, geo distance, scripts), counting, composition, `terminate_after`,
//!   collapsing, rescoring; [`highlight`]: unified/plain/fvh highlighting; [`termvectors`]:
//!   stored term vectors and the `_termvectors` statistics.
//! - Suggesters: [`suggest`] (completion), [`spell`] (term), [`phrase_suggest`] (phrase).
//!
//! See `docs/search/architecture.md` and `docs/search/contracts.md`.

pub mod buffer;
pub mod collector;
pub mod doc;
pub mod error;
pub mod highlight;
pub mod merge;
pub mod query;
pub mod phrase_suggest;
pub mod schema;
pub mod search;
pub mod shard;
pub mod similarity;
pub mod spell;
pub mod suggest;
pub mod termvectors;
pub mod util;

pub use doc::{DocValue, IndexableDoc, IndexableField, PreToken, StoredField};
pub use error::{Error, Result};
pub use schema::{FieldSchema, Schema, TermVectorOptions};
pub use search::{
    BulkScorer, Collector, CollectorManager, Explanation, LeafCollector, Query, QueryRef, Scorable, ScoreMode, Scorer,
    SearchControl, Searcher, SegmentContext, Weight,
};
pub use shard::{
    DeleteOp, FlushOutcome, FlushedSegment, IndexOp, OpResult, OpType, Origin, SegmentGate, SegmentHandle, Shard, ShardConfig,
    ShardStats, StoredDoc, VersionConflict, VersionType,
};
pub use similarity::Similarity;
