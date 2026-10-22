//! What aggregations need from the index they run on: field metadata, queries, scripts,
//! formats and fetching, through traits `search-es` implements per index.
//!
//! Parsing ([`crate::parse`]) is index-independent; binding a request to a shard
//! ([`crate::AggRequest::collector`]) resolves every `field` through [`FieldLookup::field`] into an
//! [`AggField`]: the doc-values field number, how the values are encoded (the conventions below)
//! and the Elasticsearch type name used in error messages.
//!
//! Doc-values encodings (what `search-es` indexes):
//!
//! | values source | doc values | value |
//! | ------------- | ---------- | ----- |
//! | `long`/`integer`/`short`/`byte`/`token_count` | numeric or sorted numeric | the value |
//! | `double` | sorted numeric | `NumericUtils.doubleToSortableLong` |
//! | `float` | sorted numeric | `NumericUtils.floatToSortableInt` |
//! | `half_float` | sorted numeric | `HalfFloatPoint` sortable short |
//! | `scaled_float` | sorted numeric | `round(value * scaling_factor)` |
//! | `unsigned_long` | sorted numeric | `value ^ 2^63` (sortable) |
//! | `boolean` | sorted numeric | 0 / 1 |
//! | `date` / `date_nanos` | sorted numeric | epoch millis / nanos |
//! | `keyword`, `version`, `text` with fielddata… | sorted set | the term bytes |
//! | `ip` | sorted set | 16-byte `InetAddressPoint` encoding |
//! | `geo_point` | sorted numeric | `LatLonDocValuesField`: `lat << 32 \| lon` (encoded ints) |
//! | `point` | sorted numeric | `XYDocValuesField`: `floatToSortableInt(x) << 32 \| floatToSortableInt(y)` |
//! | `geo_shape` / `shape` | binary | search-index's shape doc value (triangles); centroid in a companion binary field ([`crate::shape_field`]) |
//! | `histogram` | binary | `(value, count)` centroids, `EncodedTDigest` encoding ([`crate::histogram_field`]) |
//! | `exponential_histogram` | binary | scale, zero bucket, summary, buckets ([`crate::exp_histogram::encode`]) |
//! | `aggregate_metric_double` | sorted numeric per metric | `min`/`max`/`sum` doubles, `value_count` integer |
//! | `*_range` | binary | all of the doc's ranges, `BinaryRangeUtil` encoding ([`crate::range_field`]) |

use std::sync::Arc;

use search_index::QueryRef;
use search_index::SegmentContext;

use crate::error::{AggError, Result};
use crate::format::{DateFormatter, NumberFormatter};
use crate::json::Json;
use crate::script::ScriptHost;

/// How a numeric field's doc values decode.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NumericType {
    Long,
    Integer,
    Short,
    Byte,
    UnsignedLong,
    Double,
    Float,
    HalfFloat,
    ScaledFloat(f64),
}

impl NumericType {
    /// Whether values are integers (`IndexNumericFieldData.isFloatingPoint() == false`).
    pub fn is_integral(self) -> bool {
        matches!(self, NumericType::Long | NumericType::Integer | NumericType::Short | NumericType::Byte | NumericType::UnsignedLong)
    }
}

/// Elasticsearch's `ValuesSourceType` of a field, with what decoding needs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SourceType {
    Numeric(NumericType),
    /// `date` (millis) or `date_nanos` (nanos).
    Date { nanos: bool },
    Boolean,
    /// Terms from a sorted-set (or sorted) dictionary.
    Keyword,
    Ip,
    GeoPoint,
    /// `point` (cartesian): `XYDocValuesField`.
    CartesianPoint,
    /// `geo_shape`: search-index's shape doc value (packed triangles), plus an optional
    /// companion binary field holding the centroid computed from the original geometry
    /// ([`crate::shape_field::encode_centroid`]).
    GeoShape { centroid: Option<u32> },
    /// `shape` (cartesian): as [`SourceType::GeoShape`], `XYEncodingUtils` coordinates.
    CartesianShape { centroid: Option<u32> },
    /// `histogram` (pre-aggregated): binary doc values of `(value, count)` centroids (see
    /// [`crate::histogram_field`]).
    Histogram,
    /// `exponential_histogram`: one binary value per doc ([`crate::exp_histogram::encode`]).
    ExponentialHistogram,
    /// `aggregate_metric_double`: numeric sub-fields per metric.
    AggregateMetric(crate::histogram_field::AggregateMetricFields),
    /// `*_range` fields: binary doc values in `BinaryRangeUtil`'s encoding (see
    /// [`crate::range_field`]).
    Range(crate::range_field::RangeType),
    /// No doc values usable by aggregations (`text` without fielddata, `binary`, …).
    None,
}

impl SourceType {
    pub fn is_numeric_like(self) -> bool {
        matches!(self, SourceType::Numeric(_) | SourceType::Date { .. } | SourceType::Boolean)
    }
}

/// A field as aggregations see it.
#[derive(Clone, Debug)]
pub struct AggField {
    /// Full path (`user.name`).
    pub name: String,
    /// The mapping type name (`long`, `keyword`, `date`, `text`, …) for error messages.
    pub type_name: String,
    pub source: SourceType,
    /// The shard field number holding the doc values.
    pub number: u32,
    /// The mapping's date format (`strict_date_optional_time||epoch_millis` when unset).
    pub date_format: Option<String>,
    /// The innermost `nested` object path containing the field, if any.
    pub nested_path: Option<String>,
    /// Whether the field is indexed with points (min/max bounds for date rounding).
    pub points: bool,
    /// A `time_series_metric: counter` field of a `time_series` index (Elasticsearch's
    /// `COUNTER` values source: `rate` computes a reset-aware counter rate per series, and
    /// several aggregations refuse the field).
    pub counter: bool,
}

impl AggField {
    pub fn new(name: impl Into<String>, type_name: impl Into<String>, source: SourceType, number: u32) -> AggField {
        AggField { name: name.into(), type_name: type_name.into(), source, number, date_format: None, nested_path: None, points: false, counter: false }
    }
}

/// A `nested` object path: its docs and their parents.
#[derive(Clone)]
pub struct NestedPath {
    /// Docs of this nested path.
    pub child_filter: QueryRef,
    /// Docs of the parent level (the root docs, or the enclosing nested path's docs).
    pub parent_filter: QueryRef,
}

/// Answers of [`FieldLookup::nested`].
pub enum NestedLookup {
    Nested(NestedPath),
    /// The path exists but is an object, not `nested`.
    NotNested,
    NotFound,
}

/// The `join` field of an index (parent/child relations) for `children` / `parent`.
#[derive(Clone)]
pub struct JoinRelation {
    /// The doc-values field holding, on parents, their own id and, on children, their
    /// parent's id (Elasticsearch's `<join>#<parent type>` field).
    pub join_field: u32,
    /// Docs of the parent type.
    pub parent_filter: QueryRef,
    /// Docs of the child type.
    pub child_filter: QueryRef,
}

/// One hit handed to the top-hits fetcher.
#[derive(Clone, Debug)]
pub struct FetchHit {
    /// Global doc id in the searcher.
    pub doc: u32,
    /// `NaN` when not scored.
    pub score: f32,
    /// The hit's `sort` values as the response prints them (raw: longs, doubles, strings for
    /// keyword and ip fields, `null` for missing strings); empty when sorted by score.
    pub sort: Vec<Json>,
}

/// Renders top hits (`top_hits` / `top_metrics` need documents): the fetch phase for the hits
/// of one bucket.
pub trait TopHitsFetcher: Send + Sync {
    /// Hit objects as in `hits.hits` (`_index`, `_id`, `_score`, `_source`, `sort`, `fields`,
    /// `highlight`, …) for `hits`, following the `top_hits` definition `spec` (its `_source`,
    /// `stored_fields`, `docvalue_fields`, `fields`, `script_fields`, `highlight`, `explain`,
    /// `version`, `seq_no_primary_term`).
    fn fetch(&self, searcher: &search_index::Searcher, hits: &[FetchHit], spec: &Json) -> Result<Vec<Json>>;
}

/// Where documents without a value sort (`missing`).
#[derive(Clone, Debug, PartialEq)]
pub enum SortMissing {
    /// `_last` (the default): after every value, whatever the order.
    Last,
    /// `_first`.
    First,
    /// A replacement value (a number for numeric fields, a string for keyword fields).
    Value(Json),
}

/// How a multi-valued field picks its sort value (`mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortMode {
    Min,
    Max,
    Sum,
    Avg,
    Median,
}

/// A sort key of `top_hits` / `top_metrics` resolved against the index.
#[derive(Clone, Debug)]
pub enum HitSort {
    /// `_score` (`reverse: false` is descending, Lucene's natural score order).
    Score { reverse: bool },
    /// `_doc`.
    Doc { reverse: bool },
    Field {
        field: AggField,
        reverse: bool,
        missing: SortMissing,
        /// `None`: `min` ascending, `max` descending.
        mode: Option<SortMode>,
    },
}

/// Index-level hooks of one shard's aggregation run. Every method but [`field`](Self::field)
/// has a default that fails or answers "none", so tests and simple embedders implement little.
pub trait FieldLookup: Send + Sync {
    /// The field `name` resolved for aggregations; `Ok(None)` when unmapped. Errors are
    /// Elasticsearch's (e.g. "Fielddata is disabled on [x] in [index]…").
    fn field(&self, name: &str) -> Result<Option<AggField>>;

    /// The index name (for messages such as the fielddata error).
    fn index_name(&self) -> &str {
        "index"
    }

    /// A Query DSL object as an engine query (filter, filters, adjacency_matrix,
    /// significant_terms `background_filter`).
    fn query(&self, query: &Json) -> Result<QueryRef> {
        let _ = query;
        Err(AggError::illegal_argument("queries are not supported in this context"))
    }

    /// A `nested` path.
    fn nested(&self, path: &str) -> NestedLookup {
        let _ = path;
        NestedLookup::NotFound
    }

    /// The docs of the root level (non-nested docs): `reverse_nested` without a path.
    fn root_filter(&self) -> QueryRef {
        search_index::query::nested::root_filter()
    }

    /// The documents a top-level `global` aggregation collects (`buildFilteredQuery(match_all)`:
    /// non-nested documents, plus any alias filter of the search).
    fn global_query(&self) -> QueryRef {
        self.root_filter()
    }

    /// The join relation of a type: `children` and `parent` both name the child type
    /// (`child_type`); `parent_type` is for lookups by parent type (unused by them). `None` when
    /// the index has no join field or the type is not a child type.
    fn join(&self, child_type: Option<&str>, parent_type: Option<&str>) -> Option<JoinRelation> {
        let _ = (child_type, parent_type);
        None
    }

    /// The script engine (shard-side contexts).
    fn scripts(&self) -> Option<Arc<dyn ScriptHost>> {
        None
    }

    /// A date formatter for a `format` pattern (Elasticsearch's `DateFormatter.forPattern`).
    fn date_formatter(&self, pattern: &str) -> Result<Arc<dyn DateFormatter>> {
        crate::format::builtin_date_formatter(pattern)
    }

    /// A `java.text.DecimalFormat` for a numeric `format` pattern (`Locale.ROOT` symbols).
    fn decimal_format(&self, pattern: &str) -> Result<Arc<dyn NumberFormatter>> {
        crate::format::builtin_decimal_format(pattern)
    }

    /// `now` for date math, in epoch millis (one value per request).
    fn now_millis(&self) -> i64 {
        0
    }

    /// The `_doc_count` field (pre-aggregated doc counts), if the index has one.
    fn doc_count_field(&self) -> Option<u32> {
        None
    }

    /// The fetch phase for `top_hits`.
    fn fetcher(&self) -> Option<Arc<dyn TopHitsFetcher>> {
        None
    }

    /// `index.max_inner_result_window` (100): the bound of `top_hits` `from + size`.
    fn max_inner_result_window(&self) -> u32 {
        100
    }

    /// Resolves a `top_hits` / `top_metrics` `sort` (a string, an object or an array of them,
    /// as in a search request) into sort keys. Errors are Elasticsearch's sort parsing errors.
    fn hit_sort(&self, sort: &Json) -> Result<Vec<HitSort>> {
        let _ = sort;
        Err(AggError::illegal_argument("sorting top hits is not supported here"))
    }

    /// Analysis of `_source` text for `significant_text`.
    fn text_terms(&self) -> Option<Arc<dyn TextTerms>> {
        None
    }

    /// The shard's id within the index and the index's shard count (`random_sampler`'s seed
    /// mixes them in, as Elasticsearch does).
    fn shard_id(&self) -> (u32, u32) {
        (0, 1)
    }

    /// The index's UUID (`index.uuid`). With it, `random_sampler`'s default shard seed is
    /// Elasticsearch's `ShardId.hashCode()` exactly; without it the UUID's term is left out.
    fn index_uuid(&self) -> Option<&str> {
        None
    }

    /// The index's time series layout when it is a `time_series`-mode index (`index.mode`);
    /// `None` otherwise (the `time_series` aggregation then collects nothing).
    fn time_series(&self) -> Option<TimeSeriesInfo> {
        None
    }
}

/// What the `time_series` aggregation needs from a `time_series`-mode index.
#[derive(Clone, Debug)]
pub struct TimeSeriesInfo {
    /// The `time_series_dimension` fields (any order).
    pub dimensions: Vec<AggField>,
    /// The sorted doc-values field holding each document's `_tsid` (Elasticsearch visits
    /// series in `_tsid` order, which decides which series a `size` limit keeps and the order
    /// counter `rate`s see samples in). `None`: index order is that order.
    pub tsid_field: Option<u32>,
    /// The `@timestamp` field (counter `rate`s divide by sample time spans).
    pub timestamp_field: Option<u32>,
}

/// Re-analyzed text of documents (`significant_text`), provided by the index.
pub trait TextTerms: Send + Sync {
    /// Whether `field` has an index analyzer (text and keyword-like fields; Elasticsearch
    /// refuses the others: "Field [x] has no analyzer, but SignificantText requires an analyzed
    /// field").
    fn analyzed(&self, field: &str) -> bool;
    /// The analyzed terms of the segment's doc `doc`: one list per value of each of
    /// `source_fields` (taken from `_source`, analyzed with `field`'s index analyzer), in order.
    fn tokens(&self, seg: &SegmentContext, doc: u32, field: &str, source_fields: &[String]) -> Result<Vec<Vec<Vec<u8>>>>;
    /// The number of documents of the searcher whose `field` holds `term` (Lucene's
    /// `docFreq`), the background frequency.
    fn doc_freq(&self, searcher: &search_index::Searcher, field: &str, term: &[u8]) -> Result<u64>;
    /// A query for documents whose `field` holds `term` (background frequencies under a
    /// `background_filter`).
    fn term_query(&self, field: &str, term: &[u8]) -> Result<QueryRef>;
}

/// A lookup over a fixed list of fields (tests, simple embedders).
#[derive(Default, Clone)]
pub struct StaticFields {
    pub fields: Vec<AggField>,
    pub nested: Vec<(String, NestedPath)>,
    pub now: i64,
    pub doc_count_field: Option<u32>,
}

impl FieldLookup for StaticFields {
    fn field(&self, name: &str) -> Result<Option<AggField>> {
        Ok(self.fields.iter().find(|f| f.name == name).cloned())
    }

    fn nested(&self, path: &str) -> NestedLookup {
        match self.nested.iter().find(|(p, _)| p == path) {
            Some((_, n)) => NestedLookup::Nested(n.clone()),
            None => NestedLookup::NotFound,
        }
    }

    fn now_millis(&self) -> i64 {
        self.now
    }

    fn doc_count_field(&self) -> Option<u32> {
        self.doc_count_field
    }
}
