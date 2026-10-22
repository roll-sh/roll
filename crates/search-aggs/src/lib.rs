//! Elasticsearch-compatible aggregations over roll search doc values.
//!
//! The crate covers the whole life of an `aggs` request:
//!
//! 1. **Parse** ([`parse`]): the `aggs` object, token by token, with Elasticsearch's exception
//!    types, texts and `[line:col]` locations, into an index-independent [`AggRequest`];
//!    [`AggRequest::validate`] gives the `action_request_validation_exception` messages
//!    (pipeline paths, ordering requirements).
//! 2. **Collect** ([`AggRequest::collector`]): per shard, bound to an index through
//!    [`FieldLookup`] (field metadata, queries, scripts, formats, fetch) and a
//!    `search_index::Searcher`, an [`AggCollector`] implements `search_index::Collector`; plug it
//!    into the query's search, then [`AggCollector::finish`] builds the shard's [`Aggs`].
//!    Aggregators work in batches of doc ids over doc values (see [`collect`]).
//! 3. **Reduce** ([`reduce`]): merges shard results — partially (batched, mergeable state
//!    kept) or finally (sizes, `min_doc_count`, empty buckets, `search.max_buckets`, then the
//!    pipeline aggregations).
//! 4. **Render** ([`Aggs::to_json`]): the `aggregations` object with `typed_keys`, `meta`,
//!    keyed buckets and `*_as_string` values, numbers printed like Java.
//!
//! Module map: [`json`] (ordered JSON), [`xparser`] (XContent-style parsing), [`values`]
//! (values sources and per-segment readers), [`format`] (`DocValueFormat`, time zones),
//! [`rounding`] (date rounding), [`internal`] (results, reduce, properties), [`metrics`],
//! [`bucket`], [`pipeline`], [`memory`] (the request circuit breaker), [`range_field`] (range
//! field doc values), and the Java numerics outputs depend on: [`jmath`], [`fdlibm`]
//! (`StrictMath`), [`commons_math`] (`t_test`), [`h3`] (`geohex_grid`).
//!
//! See `docs/search/architecture.md` and `docs/search/contracts.md`.

pub mod bucket;
pub mod collect;
pub mod commons_math;
pub mod error;
pub mod exp_histogram;
pub mod field;
pub mod fdlibm;
pub mod format;
pub mod h3;
pub mod histogram_field;
pub mod geo;
pub mod internal;
pub mod jmath;
pub mod json;
pub mod memory;
pub mod metrics;
pub mod parse;
pub mod pipeline;
pub mod range_field;
pub mod request;
pub mod rounding;
pub mod script;
pub mod shape_field;
pub mod sort;
pub mod util;
pub mod values;
pub mod xparser;

pub use collect::{AggCollector, ShardOptions};
pub use error::{AggError, Result};
pub use field::{
    AggField, FetchHit, FieldLookup, HitSort, JoinRelation, NestedLookup, NestedPath, NumericType, SortMissing, SortMode, SourceType, StaticFields, TextTerms,
    TimeSeriesInfo, TopHitsFetcher,
};
pub use format::{DateFormatter, DocValueFormat, NumberFormatter, TimeZone};
pub use internal::{Agg, AggValue, Aggs, ReduceOptions};
pub use json::Json;
pub use memory::MemoryBudget;
pub use range_field::RangeType;
pub use parse::ParseOptions;
pub use request::AggRequest;
pub use script::{Script, ScriptHost, ScriptKind, ScriptValue};

/// A shard's aggregation results (the same type as a reduced result: partial reduces feed
/// further reduces).
pub type ShardAggs = Aggs;

/// Parses an `aggs` object.
pub fn parse(aggs: &Json, opts: &ParseOptions<'_>) -> Result<AggRequest> {
    parse::parse_aggs(aggs, opts)
}

/// Parses an `aggs` object given as `serde_json` (key order is `serde_json`'s).
pub fn parse_value(aggs: &serde_json::Value) -> Result<AggRequest> {
    parse::parse_aggs(&Json::from_serde(aggs), &ParseOptions::default())
}

impl AggRequest {
    /// `AggregatorFactories.Builder.validate`: messages of an
    /// `action_request_validation_exception` (empty when valid).
    pub fn validate(&self) -> Vec<String> {
        pipeline::validate(&self.aggs, &self.pipelines)
    }

    /// Binds the request to one shard. Plug the collector into the search, then call
    /// [`AggCollector::finish`].
    pub fn collector(&self, lookup: &dyn FieldLookup, searcher: &search_index::Searcher, opts: &ShardOptions) -> Result<AggCollector> {
        AggCollector::new(self, lookup, searcher, opts)
    }

    /// Runs the aggregations of one shard for `query` (a convenience over
    /// [`collector`](Self::collector) + `Searcher::search` + [`AggCollector::finish`]).
    pub fn run(&self, lookup: &dyn FieldLookup, searcher: &search_index::Searcher, query: &search_index::QueryRef, opts: &ShardOptions) -> Result<Aggs> {
        let all = opts.matches_all || query.as_any().is::<search_index::query::basic::MatchAllQuery>();
        let opts = ShardOptions { matches_all: all, ..opts.clone() };
        let mut c = self.collector(lookup, searcher, &opts)?;
        searcher.search(query, &mut c)?;
        c.finish()
    }
}

/// Merges shard (or partially reduced) results. With `final_reduce`, applies sizes, empty
/// buckets, `search.max_buckets` and the pipeline aggregations.
pub fn reduce(parts: Vec<Aggs>, req: &AggRequest, opts: &ReduceOptions) -> Result<Aggs> {
    let mut ctx = internal::ReduceCtx::new(opts);
    let mut out = Aggs::reduce_lists(parts, &mut ctx)?;
    if opts.final_reduce {
        pipeline::apply_top(&mut out, &req.aggs, &req.pipelines, &mut ctx)?;
    }
    Ok(out)
}
