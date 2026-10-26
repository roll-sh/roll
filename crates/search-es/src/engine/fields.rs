//! The engine view of an index mapping: which engine field numbers a mapping field indexes into,
//! how (terms, points, doc values, vectors, stored), and with which analyzers.
//!
//! Elasticsearch's field mappers each write one or more Lucene fields named after the field's
//! full path (`title`, `title.raw`, `title._index_prefix`, `flat._keyed`). roll does the same:
//! a [`FieldTable`] is built from a [`Mapping`] and the index's analysis chain and lists every
//! engine field with its [`search_index::schema::FieldSchema`]. Field numbers are allocated by
//! the index ([`Numbers`]) the first time a name is seen and never reused, so segments written
//! under an older mapping stay readable after it grows. Tables are immutable and rebuilt when
//! the mapping changes (a walk over the mapping), so indexing and searching read them without
//! locks.
//!
//! The engine's metadata fields sit below [`FIRST_FIELD`]: `_source`, `_routing`, `_ignored`,
//! `_nested_path`, `_doc_count`, `_tsid` and `_feature` (`_id`, `_seq_no`, `_version` and
//! `_primary_term` are search-index's own).

use crate::analysis::es_error;
use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::mapping::{FieldMapper, Mapper, Mapping, MappingCtx, ObjectKind, ObjectMapper};
use crate::time::DateFormatter;
use parking_lot::Mutex;
use search_analysis::{Analysis, Analyzer, Normalizer};
use search_codec::{
    DocValuesType, HnswParams, IndexOptions, PointsSpec, VectorElement, VectorQuantization, VectorSimilarity, VectorSpec,
};
use search_index::schema::{FieldSchema, Schema, TermVectorOptions};
use search_index::Similarity;
use std::collections::HashMap;
use std::sync::Arc;

#[path = "../persist_hooks.rs"]
pub mod persist_hooks;

/// `_source`: the original document bytes (stored only).
pub const SOURCE_FIELD: u32 = 8;
/// `_routing`: indexed and stored.
pub const ROUTING_FIELD: u32 = 9;
/// `_ignored`: indexed, sorted-set doc values and stored.
pub const IGNORED_FIELD: u32 = 10;
/// `_nested_path`: the path of nested child documents (terms).
pub const NESTED_PATH_FIELD: u32 = 11;
/// `_doc_count`: pre-aggregated document counts (numeric doc values).
pub const DOC_COUNT_FIELD: u32 = 12;
/// `_tsid`: time series ids (sorted doc values).
pub const TSID_FIELD: u32 = 13;
/// `_feature`: `rank_feature` values as term frequencies, keyed by field name. It also holds
/// the `_field_names` entries (the field name, frequency 1) of fields nothing else marks as
/// present: `binary` without doc values.
pub const FEATURE_FIELD: u32 = 14;
/// `_ts_routing_hash`: the routing hash of time series documents (sorted doc values).
pub const TS_ROUTING_HASH_FIELD: u32 = 15;
/// User fields start here.
pub const FIRST_FIELD: u32 = 16;

/// How a field's values are indexed and queried.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Text,
    MatchOnlyText,
    /// `search_as_you_type` root field (its `._Ngram` and `._index_prefix` subfields are their
    /// own [`FieldInfo`]s of kind `Text`).
    SearchAsYouType,
    Keyword,
    ConstantKeyword,
    Wildcard,
    Version,
    Long,
    Integer,
    Short,
    Byte,
    UnsignedLong,
    Double,
    Float,
    HalfFloat,
    ScaledFloat(f64),
    TokenCount,
    Date,
    DateNanos,
    Boolean,
    Ip,
    Binary,
    GeoPoint,
    Point,
    GeoShape,
    Shape,
    IntegerRange,
    LongRange,
    FloatRange,
    DoubleRange,
    DateRange,
    IpRange,
    DenseVector,
    SparseVector,
    RankFeature,
    RankFeatures,
    Completion,
    Flattened,
    Join,
    Percolator,
    Histogram,
    AggregateMetric,
    CountedKeyword,
    Other,
}

impl Kind {
    pub fn of(ty: &str) -> Kind {
        match ty {
            "text" | "annotated_text" => Kind::Text,
            "match_only_text" => Kind::MatchOnlyText,
            "search_as_you_type" => Kind::SearchAsYouType,
            "keyword" => Kind::Keyword,
            "constant_keyword" => Kind::ConstantKeyword,
            "wildcard" => Kind::Wildcard,
            "version" => Kind::Version,
            "long" => Kind::Long,
            "integer" => Kind::Integer,
            "short" => Kind::Short,
            "byte" => Kind::Byte,
            "unsigned_long" => Kind::UnsignedLong,
            "double" => Kind::Double,
            "float" => Kind::Float,
            "half_float" => Kind::HalfFloat,
            "scaled_float" => Kind::ScaledFloat(1.0),
            "token_count" => Kind::TokenCount,
            "date" => Kind::Date,
            "date_nanos" => Kind::DateNanos,
            "boolean" => Kind::Boolean,
            "ip" => Kind::Ip,
            "binary" => Kind::Binary,
            "geo_point" => Kind::GeoPoint,
            "point" | "xy_point" => Kind::Point,
            "geo_shape" => Kind::GeoShape,
            "shape" => Kind::Shape,
            "integer_range" => Kind::IntegerRange,
            "long_range" => Kind::LongRange,
            "float_range" => Kind::FloatRange,
            "double_range" => Kind::DoubleRange,
            "date_range" => Kind::DateRange,
            "ip_range" => Kind::IpRange,
            "dense_vector" | "knn_vector" => Kind::DenseVector,
            "sparse_vector" => Kind::SparseVector,
            "rank_feature" => Kind::RankFeature,
            "rank_features" => Kind::RankFeatures,
            "completion" => Kind::Completion,
            "flattened" | "flat_object" => Kind::Flattened,
            "join" => Kind::Join,
            "percolator" => Kind::Percolator,
            "histogram" | "exponential_histogram" => Kind::Histogram,
            "aggregate_metric_double" => Kind::AggregateMetric,
            "counted_keyword" => Kind::CountedKeyword,
            _ => Kind::Other,
        }
    }

    /// Numeric kinds whose values are integers.
    pub fn is_integral(self) -> bool {
        matches!(self, Kind::Long | Kind::Integer | Kind::Short | Kind::Byte | Kind::TokenCount | Kind::UnsignedLong)
    }

    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            Kind::Long
                | Kind::Integer
                | Kind::Short
                | Kind::Byte
                | Kind::UnsignedLong
                | Kind::Double
                | Kind::Float
                | Kind::HalfFloat
                | Kind::ScaledFloat(_)
                | Kind::TokenCount
        )
    }

    pub fn is_date(self) -> bool {
        matches!(self, Kind::Date | Kind::DateNanos)
    }

    pub fn is_range(self) -> bool {
        matches!(self, Kind::IntegerRange | Kind::LongRange | Kind::FloatRange | Kind::DoubleRange | Kind::DateRange | Kind::IpRange)
    }

    /// Indexed as terms of keyword-like values.
    pub fn is_keyword_like(self) -> bool {
        matches!(self, Kind::Keyword | Kind::ConstantKeyword | Kind::Version | Kind::Wildcard | Kind::CountedKeyword)
    }

    pub fn is_text(self) -> bool {
        matches!(self, Kind::Text | Kind::MatchOnlyText | Kind::SearchAsYouType)
    }
}

/// One mapping field as the engine sees it.
#[derive(Clone)]
pub struct FieldInfo {
    /// Full path (`title.raw`).
    pub name: String,
    /// The mapping type name.
    pub ty: String,
    pub kind: Kind,
    /// The engine field holding terms / points / doc values.
    pub number: u32,
    pub index: bool,
    pub doc_values: bool,
    pub store: bool,
    pub norms: bool,
    pub index_options: IndexOptions,
    pub analyzer: Option<Analyzer>,
    pub search_analyzer: Option<Analyzer>,
    pub search_quote_analyzer: Option<Analyzer>,
    /// A `position_increment_gap` set on the field (the analyzer's gap otherwise).
    pub position_gap: Option<i32>,
    pub normalizer: Option<Normalizer>,
    pub ignore_above: Option<usize>,
    pub date_format: Option<Arc<DateFormatter>>,
    /// The innermost `nested` object path containing the field.
    pub nested_path: Option<String>,
    /// `index_prefixes`: (engine field, min_chars, max_chars).
    pub prefix_field: Option<(u32, usize, usize)>,
    /// `index_phrases`: the shingle field.
    pub phrase_field: Option<u32>,
    /// `search_as_you_type` subfields: shingle fields by size.
    pub shingle_fields: Vec<(usize, u32)>,
    /// `flattened`: the `._keyed` field (`key\0value` terms).
    pub keyed_field: Option<u32>,
    /// A keyed `flattened` sub-field (`labels.env`, resolved by [`FieldTable::keyed`]): the
    /// key, whose `key\0` prefixes every term of the `._keyed` field `number` is.
    pub flattened_key: Option<String>,
    /// The mapper (parameters such as `null_value`, `scaling_factor`, `format`).
    pub mapper: Arc<FieldMapper>,
    /// `fielddata: true` on text fields (sorted-set doc values are written for them).
    pub fielddata: bool,
    pub eager_global_ordinals: bool,
    /// `time_series_dimension`.
    pub dimension: bool,
    /// `time_series_metric`.
    pub metric: Option<String>,
    /// `dense_vector` spec.
    pub vector: Option<VectorSpec>,
    /// `dense_vector` similarity name as mapped.
    pub vector_similarity: Option<String>,
    /// `split_queries_on_whitespace`: the keyword field's search analyzer (a whitespace
    /// tokenizer plus the normalizer's filters, or Lucene's whitespace analyzer).
    pub whitespace_analyzer: Option<Analyzer>,
    /// `constant_keyword` value.
    pub constant_value: Option<String>,
    /// A multi-field (or subfield) of another field: the parent's path.
    pub parent: Option<String>,
    /// `similarity` mapping parameter.
    pub similarity: Option<String>,
    /// Engine subfields of a field (aggregate metric `min`/`max`/…, join `#parent`).
    pub subfields: Vec<(String, u32)>,
    /// A `dense_vector`'s earlier layouts (engine fields of index options changed since):
    /// documents written before the change keep their vectors there.
    pub vector_layouts: Vec<u32>,
}

impl std::fmt::Debug for FieldInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FieldInfo").field("name", &self.name).field("ty", &self.ty).field("number", &self.number).finish()
    }
}

impl FieldInfo {
    /// A vector field `index.mapping.exclude_source_vectors` keeps out of `_source`.
    pub fn is_source_vector(&self) -> bool {
        // OpenSearch's `knn_vector` stays in `_source` (`opensearch::knn::derive_source`).
        self.parent.is_none() && self.ty != "knn_vector" && (matches!(self.kind, Kind::DenseVector | Kind::SparseVector) || self.ty == "rank_vectors")
    }

    /// The analyzer queries run their text through (`search_analyzer`, else the index one).
    pub fn query_analyzer(&self) -> Option<&Analyzer> {
        self.search_analyzer.as_ref().or(self.analyzer.as_ref())
    }

    /// The analyzer phrase queries use (`search_quote_analyzer`).
    pub fn quote_analyzer(&self) -> Option<&Analyzer> {
        self.search_quote_analyzer.as_ref().or(self.query_analyzer())
    }

    /// Whether the field has positions (phrase queries).
    pub fn has_positions(&self) -> bool {
        self.index_options >= IndexOptions::DocsAndFreqsAndPositions
    }

    pub fn param(&self, ctx: &MappingCtx, name: &str) -> Value {
        self.mapper.effective(name, ctx)
    }

    /// Whether search can use the field's index (terms, points) or doc values.
    pub fn searchable(&self) -> bool {
        self.index || self.doc_values
    }

    pub fn subfield(&self, name: &str) -> Option<u32> {
        self.subfields.iter().find(|(n, _)| n == name).map(|(_, f)| *f)
    }
}

/// An object mapper's path and kind (nested queries, `exists` on objects).
#[derive(Clone, Debug)]
pub struct ObjectInfo {
    pub path: String,
    pub nested: bool,
    pub include_in_parent: bool,
    pub include_in_root: bool,
    /// The enclosing nested path.
    pub parent_nested: Option<String>,
}

/// Field numbers of an index: stable for the life of the index.
#[derive(Default, Debug)]
pub struct Numbers {
    inner: Mutex<(HashMap<String, u32>, u32)>,
}

impl Numbers {
    /// The numbers of keys starting with `prefix`, ascending.
    pub fn with_prefix(&self, prefix: &str) -> Vec<u32> {
        let g = self.inner.lock();
        let mut v: Vec<u32> = g.0.iter().filter(|(k, _)| k.starts_with(prefix)).map(|(_, n)| *n).collect();
        v.sort_unstable();
        v
    }

    pub fn of(&self, name: &str) -> u32 {
        let mut g = self.inner.lock();
        if let Some(n) = g.0.get(name) {
            return *n;
        }
        if g.1 < FIRST_FIELD {
            g.1 = FIRST_FIELD;
        }
        let n = g.1;
        g.1 += 1;
        g.0.insert(name.to_string(), n);
        n
    }
}

/// Similarities configured for an index (`index.similarity.*`).
#[derive(Clone, Debug, Default)]
pub struct Similarities {
    pub default: Similarity,
    pub named: HashMap<String, Similarity>,
    /// The built-in `BM25` is Lucene's plain `BM25Similarity` (the opensearch dialect), not
    /// Elasticsearch's legacy one.
    pub plain_bm25: bool,
}

impl Similarities {
    /// The built-in `BM25` similarity of a dialect.
    pub fn builtin_bm25(plain: bool) -> Similarity {
        match Similarity::bm25() {
            Similarity::Bm25 { k1, b, discount_overlaps, .. } if plain => Similarity::Bm25 { k1, b, discount_overlaps, legacy: false },
            s => s,
        }
    }

    pub fn get(&self, name: &str) -> Option<Similarity> {
        match name {
            "BM25" => Some(Self::builtin_bm25(self.plain_bm25)),
            "boolean" => Some(Similarity::Boolean),
            n => self.named.get(n).cloned(),
        }
    }
}

/// Every engine field of an index, built from its mapping.
pub struct FieldTable {
    pub schema: Schema,
    /// Looked up for every field of every indexed document: a fast fixed-seed hash.
    pub fields: HashMap<String, Arc<FieldInfo>, foldhash::fast::FixedState>,
    pub by_number: HashMap<u32, Arc<FieldInfo>, foldhash::fast::FixedState>,
    /// Field aliases: alias path → target path.
    pub aliases: HashMap<String, String>,
    pub objects: HashMap<String, ObjectInfo>,
    pub mapping: Arc<Mapping>,
    pub analysis: Arc<Analysis>,
    /// The (mapping, settings, synonyms) versions the table was built from ([`super::table_key`]).
    pub key: (u64, u64, u64),
    /// The mapping has vector fields (`dense_vector`, `rank_vectors`, `sparse_vector`), which
    /// fetched `_source` leaves out by default.
    pub has_vectors: bool,
    /// `copy_to` targets and the fields copying into them (`MappingLookup.sourcePaths`):
    /// the fields API reads a target's values from its sources.
    pub copy_sources: HashMap<String, Vec<String>>,
}

impl std::fmt::Debug for FieldTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FieldTable").field("fields", &self.fields.len()).finish()
    }
}

fn internal(e: search_index::Error) -> EsError {
    EsError::internal(e.to_string())
}

/// `(priority, leaf name, full path)` of every leaf that resolves at the root through a
/// pass-through container: `passthrough` objects and `flattened` fields with `passthrough`.
fn passthrough_leaves(o: &ObjectMapper, prefix: &str, out: &mut Vec<(i64, String, String)>) {
    for (k, c) in &o.children {
        let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        match c {
            Mapper::Object(child) => {
                if let ObjectKind::PassThrough { priority, .. } = child.kind {
                    for (lk, lc) in &child.children {
                        if let Mapper::Field(_) = lc {
                            out.push((priority, lk.clone(), format!("{path}.{lk}")));
                        }
                    }
                }
                crate::stack::maybe_grow(|| passthrough_leaves(child, &path, out));
            }
            Mapper::Field(f) if f.ty == "flattened" => {
                if let Some(priority) = f.raw("passthrough").and_then(|p| p.get("priority")).and_then(Value::as_i64) {
                    for pk in f.props.keys() {
                        out.push((priority, pk.clone(), format!("{path}.{pk}")));
                    }
                }
            }
            _ => {}
        }
    }
}

impl FieldTable {
    /// The field at `path` (aliases resolved).
    pub fn get(&self, path: &str) -> Option<&Arc<FieldInfo>> {
        match self.fields.get(path) {
            Some(f) => Some(f),
            None => self.aliases.get(path).and_then(|t| self.fields.get(t)),
        }
    }

    /// `KeyedFlattenedFieldType`: `root.key` of a `flattened` field `root` (the longest
    /// mapped flattened prefix), as a field over its `._keyed` terms.
    pub fn keyed(&self, path: &str) -> Option<Arc<FieldInfo>> {
        let (root, key) = path.match_indices('.').rev().find_map(|(i, _)| {
            let f = self.get(&path[..i])?;
            (f.kind == Kind::Flattened && f.keyed_field.is_some()).then(|| (f.clone(), &path[i + 1..]))
        })?;
        if key.is_empty() {
            return None;
        }
        let mut info = (*root).clone();
        info.name = path.to_string();
        info.number = root.keyed_field?;
        info.keyed_field = None;
        info.flattened_key = Some(key.to_string());
        info.parent = None;
        Some(Arc::new(info))
    }

    /// Mapped field paths matching a pattern (`*` wildcards), aliases included, sorted.
    pub fn matching(&self, pattern: &str) -> Vec<String> {
        if !pattern.contains('*') {
            return if self.get(pattern).is_some() { vec![pattern.to_string()] } else { Vec::new() };
        }
        let mut out: Vec<String> = self
            .fields
            .iter()
            .filter(|(_, f)| !f.name.contains("._") || f.parent.is_none())
            .map(|(k, _)| k)
            .chain(self.aliases.keys())
            .filter(|k| crate::xcontent::filter::glob_match(pattern, k))
            .cloned()
            .collect();
        out.sort();
        out
    }

    pub fn nested_paths(&self) -> impl Iterator<Item = &ObjectInfo> {
        self.objects.values().filter(|o| o.nested)
    }

    /// The nested object of `path` (or the innermost nested object containing it).
    pub fn nested_of(&self, path: &str) -> Option<&ObjectInfo> {
        self.objects.get(path).filter(|o| o.nested)
    }

    /// Builds the table for `mapping`.
    pub fn build(mapping: Arc<Mapping>, analysis: Arc<Analysis>, ctx: &MappingCtx, numbers: &Numbers, sims: &Similarities) -> EsResult<FieldTable> {
        let mut t = FieldTable {
            schema: Schema::new().with_similarity(sims.default.clone()),
            fields: HashMap::default(),
            by_number: HashMap::default(),
            aliases: HashMap::new(),
            objects: HashMap::new(),
            mapping: mapping.clone(),
            analysis,
            key: (0, 0, 0),
            has_vectors: false,
            copy_sources: HashMap::new(),
        };
        let s = &mut t.schema;
        s.add_field(FieldSchema::new(SOURCE_FIELD, "_source")).map_err(internal)?;
        s.add_field(FieldSchema::new(ROUTING_FIELD, "_routing").indexed(IndexOptions::Docs, false)).map_err(internal)?;
        s.add_field(FieldSchema::new(IGNORED_FIELD, "_ignored").indexed(IndexOptions::Docs, false).with_doc_values(DocValuesType::SortedSet))
            .map_err(internal)?;
        s.add_field(FieldSchema::new(NESTED_PATH_FIELD, "_nested_path").indexed(IndexOptions::Docs, false)).map_err(internal)?;
        s.add_field(FieldSchema::new(DOC_COUNT_FIELD, "_doc_count").with_doc_values(DocValuesType::Numeric)).map_err(internal)?;
        s.add_field(FieldSchema::new(TSID_FIELD, "_tsid").indexed(IndexOptions::Docs, false).with_doc_values(DocValuesType::Sorted))
            .map_err(internal)?;
        s.add_field(FieldSchema::new(FEATURE_FIELD, "_feature").indexed(IndexOptions::DocsAndFreqs, false)).map_err(internal)?;
        s.add_field(FieldSchema::new(TS_ROUTING_HASH_FIELD, "_ts_routing_hash").with_doc_values(DocValuesType::Sorted)).map_err(internal)?;
        let root = mapping.root.clone();
        t.walk(&root, "", None, ctx, numbers, sims)?;
        // Pass-through leaves also resolve by their name without the prefix; when several
        // pass-through containers have one, the highest `priority` wins (the mapping rejects
        // equal priorities).
        let mut pass = Vec::new();
        passthrough_leaves(&root, "", &mut pass);
        pass.sort_by_key(|p| std::cmp::Reverse(p.0));
        for (_, name, target) in pass {
            t.aliases.entry(name).or_insert(target);
        }
        t.has_vectors = t.fields.values().any(|f| f.is_source_vector());
        let mut copies: Vec<(String, String)> =
            t.fields.values().flat_map(|f| f.mapper.copy_to.iter().map(|target| (target.clone(), f.name.clone()))).collect();
        copies.sort();
        copies.dedup();
        for (target, source) in copies {
            t.copy_sources.entry(target).or_default().push(source);
        }
        Ok(t)
    }

    fn walk(&mut self, o: &ObjectMapper, prefix: &str, nested: Option<&str>, ctx: &MappingCtx, numbers: &Numbers, sims: &Similarities) -> EsResult<()> {
        for (k, c) in &o.children {
            let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            match c {
                Mapper::Object(child) => {
                    let (is_nested, ip, ir) = match &child.kind {
                        ObjectKind::Nested { include_in_parent, include_in_root } => {
                            (true, include_in_parent.unwrap_or(false), include_in_root.unwrap_or(false))
                        }
                        _ => (false, false, false),
                    };
                    self.objects.insert(
                        path.clone(),
                        ObjectInfo { path: path.clone(), nested: is_nested, include_in_parent: ip, include_in_root: ir, parent_nested: nested.map(str::to_string) },
                    );
                    let inner = if is_nested { Some(path.clone()) } else { nested.map(str::to_string) };
                    crate::stack::maybe_grow(|| self.walk(child, &path, inner.as_deref(), ctx, numbers, sims))?;
                }
                Mapper::Field(f) => {
                    self.add_field(f, &path, nested, None, ctx, numbers, sims)?;
                    for (mk, mf) in &f.fields {
                        let mpath = format!("{path}.{mk}");
                        self.add_field(mf, &mpath, nested, Some(&path), ctx, numbers, sims)?;
                    }
                    // Mapped sub-fields of a `flattened` field are fields of their own
                    // (`<path>.<key>`).
                    if f.ty == "flattened" {
                        for (pk, pf) in &f.props {
                            self.add_field(pf, &format!("{path}.{pk}"), nested, None, ctx, numbers, sims)?;
                        }
                    }
                }
                Mapper::Alias(a) => {
                    self.aliases.insert(path, a.path.clone());
                }
            }
        }
        Ok(())
    }

    fn analyzer(&self, name: &str) -> EsResult<Analyzer> {
        if name == "default" {
            return Ok(self.analysis.default_analyzer());
        }
        self.analysis.analyzer(name).ok_or_else(|| EsError::illegal_argument(format!("analyzer [{name}] has not been configured in mappings")))
    }

    fn search_analyzer(&self, name: &str) -> EsResult<Analyzer> {
        if name == "default" {
            return Ok(self.analysis.default_analyzer());
        }
        self.analysis
            .search_analyzer(name)
            .or_else(|| self.analysis.analyzer(name))
            .ok_or_else(|| EsError::illegal_argument(format!("analyzer [{name}] has not been configured in mappings")))
    }

    #[allow(clippy::too_many_arguments)]
    fn add_field(&mut self, f: &FieldMapper, path: &str, nested: Option<&str>, parent: Option<&str>, ctx: &MappingCtx, numbers: &Numbers, sims: &Similarities) -> EsResult<()> {
        let mut kind = Kind::of(f.ty);
        if let Kind::ScaledFloat(_) = kind {
            kind = Kind::ScaledFloat(f.effective("scaling_factor", ctx).as_f64().unwrap_or(1.0));
        }
        let b = |name: &str, d: bool| -> bool {
            if f.def().param_index(name).is_some() {
                match f.effective(name, ctx) {
                    Value::Bool(b) => b,
                    Value::Obj(_) => true,
                    _ => d,
                }
            } else {
                d
            }
        };
        // A vector field's layout (dims, element, similarity, quantization) can change through
        // mapping updates; each layout is its own engine field, which never changes once written.
        let number = if kind == Kind::DenseVector {
            let spec = vector_spec(f, ctx);
            if spec.dims > 0 { numbers.of(&format!("{path}\u{0}{spec:?}")) } else { numbers.of(path) }
        } else {
            numbers.of(path)
        };
        let index = b("index", !matches!(kind, Kind::Binary | Kind::Histogram | Kind::AggregateMetric));
        let default_dv = !matches!(
            kind,
            Kind::Text
                | Kind::MatchOnlyText
                | Kind::SearchAsYouType
                | Kind::Binary
                | Kind::Completion
                | Kind::Percolator
                | Kind::SparseVector
                | Kind::RankFeature
                | Kind::RankFeatures
        );
        let doc_values = b("doc_values", default_dv);
        let store = b("store", false);
        let ignore_above = || f.effective("ignore_above", ctx).as_i64().filter(|v| *v >= 0 && *v < i32::MAX as i64).map(|v| v as usize);
        let mut info = FieldInfo {
            name: path.to_string(),
            ty: f.ty.to_string(),
            kind,
            number,
            index,
            doc_values,
            store,
            norms: false,
            index_options: IndexOptions::None,
            analyzer: None,
            search_analyzer: None,
            search_quote_analyzer: None,
            position_gap: None,
            normalizer: None,
            ignore_above: None,
            date_format: None,
            nested_path: nested.map(str::to_string),
            prefix_field: None,
            phrase_field: None,
            shingle_fields: Vec::new(),
            keyed_field: None,
            flattened_key: None,
            mapper: Arc::new(f.clone()),
            fielddata: b("fielddata", false),
            eager_global_ordinals: b("eager_global_ordinals", false),
            dimension: b("time_series_dimension", false),
            metric: f.effective("time_series_metric", ctx).as_str().map(str::to_string),
            vector: None,
            vector_similarity: None,
            whitespace_analyzer: None,
            constant_value: None,
            parent: parent.map(str::to_string),
            similarity: f.effective("similarity", ctx).as_str().map(str::to_string),
            subfields: Vec::new(),
            vector_layouts: Vec::new(),
        };
        let mut fs = FieldSchema::new(number, path);
        let sim = info.similarity.as_deref().and_then(|s| sims.get(s));
        match kind {
            Kind::Text | Kind::SearchAsYouType => {
                let opts = match f.effective("index_options", ctx).as_str() {
                    Some("docs") => IndexOptions::Docs,
                    Some("freqs") => IndexOptions::DocsAndFreqs,
                    Some("offsets") => IndexOptions::DocsAndFreqsAndPositionsAndOffsets,
                    _ => IndexOptions::DocsAndFreqsAndPositions,
                };
                let a = self.analyzer(f.effective("analyzer", ctx).as_str().unwrap_or("default"))?;
                if f.is_set("position_increment_gap") {
                    info.position_gap = f.effective("position_increment_gap", ctx).as_i64().map(|g| g as i32);
                }
                let sa = match f.raw("search_analyzer").and_then(Value::as_str) {
                    Some(n) => self.search_analyzer(n)?,
                    None => match f.raw("analyzer") {
                        Some(_) => a.clone(),
                        None => self.analysis.default_search_analyzer().unwrap_or_else(|| a.clone()),
                    },
                };
                let qa = match f.raw("search_quote_analyzer").and_then(Value::as_str) {
                    Some(n) => self.search_analyzer(n)?,
                    None => match (f.raw("search_analyzer"), f.raw("analyzer")) {
                        (Some(_), _) | (_, Some(_)) => sa.clone(),
                        _ => self.analysis.default_search_quote_analyzer().unwrap_or_else(|| sa.clone()),
                    },
                };
                info.index_options = if index { opts } else { IndexOptions::None };
                info.norms = index && b("norms", true);
                if index {
                    fs = fs.indexed(opts, info.norms);
                }
                if doc_values {
                    // Elasticsearch 9.5's `doc_values: true` on text: the whole value, as a
                    // keyword would store it.
                    fs = fs.with_doc_values(DocValuesType::SortedSet);
                }
                if let Some(tv) = term_vector(f, ctx) {
                    fs = fs.with_term_vectors(tv);
                }
                if info.fielddata {
                    // A field of its own: `fielddata` can be switched on later, and an existing
                    // engine field never changes how it is indexed.
                    let sub = format!("{path}._fielddata");
                    let n = numbers.of(&sub);
                    self.schema.add_field(FieldSchema::new(n, sub).with_doc_values(DocValuesType::SortedSet)).map_err(internal)?;
                    info.subfields.push(("fielddata".into(), n));
                }
                if let Some(s) = sim.clone() {
                    fs = fs.with_similarity(s);
                }
                info.analyzer = Some(a.clone());
                info.search_analyzer = Some(sa.clone());
                info.search_quote_analyzer = Some(qa);
                if kind == Kind::SearchAsYouType {
                    let max = f.effective("max_shingle_size", ctx).as_i64().unwrap_or(3).clamp(2, 4) as usize;
                    for size in 2..=max {
                        let sub = format!("{path}._{size}gram");
                        let n = numbers.of(&sub);
                        let sh = a.with_shingles(size).map_err(es_error)?;
                        let ssh = sa.with_shingles(size).map_err(es_error)?;
                        self.sub_text(&sub, n, &info, sh, ssh, opts, sim.clone())?;
                        info.shingle_fields.push((size, n));
                    }
                    let sub = format!("{path}._index_prefix");
                    let n = numbers.of(&sub);
                    let pa = a.with_shingles_and_prefixes(max).map_err(es_error)?;
                    let psa = sa.with_shingles(max).map_err(es_error)?;
                    self.sub_text(&sub, n, &info, pa, psa, IndexOptions::DocsAndFreqsAndPositions.min(opts), sim.clone())?;
                    info.prefix_field = Some((n, 1, 19));
                }
                if let Some(Value::Obj(p)) = f.raw("index_prefixes") {
                    let min = p.get("min_chars").and_then(Value::as_i64).unwrap_or(2) as usize;
                    let max = p.get("max_chars").and_then(Value::as_i64).unwrap_or(5) as usize;
                    let sub = format!("{path}._index_prefix");
                    let n = numbers.of(&sub);
                    let gap = info.position_gap.unwrap_or_else(|| a.position_increment_gap());
                    let pa = a.with_index_prefixes(min, max, gap);
                    let po = opts.min(IndexOptions::DocsAndFreqsAndPositions);
                    self.sub_text(&sub, n, &info, pa, sa.clone(), po, sim.clone())?;
                    info.prefix_field = Some((n, min, max));
                }
                if b("index_phrases", false) {
                    let sub = format!("{path}._index_phrase");
                    let n = numbers.of(&sub);
                    let gap = info.position_gap.unwrap_or_else(|| a.position_increment_gap());
                    let pa = a.with_index_phrases(gap);
                    let psa = sa.with_index_phrases(gap);
                    self.sub_text(&sub, n, &info, pa, psa, opts, sim.clone())?;
                    info.phrase_field = Some(n);
                }
            }
            Kind::MatchOnlyText => {
                // OpenSearch's `match_only_text` takes text's analyzers and `fielddata`
                // (its definition has the parameters); Elasticsearch's is always standard.
                let os = f.def().param_index("analyzer").is_some();
                let a = match f.raw("analyzer").and_then(Value::as_str) {
                    Some(n) if os => self.analyzer(n)?,
                    _ => self.analysis.default_analyzer(),
                };
                info.index_options = if index { IndexOptions::Docs } else { IndexOptions::None };
                if index {
                    fs = fs.indexed(IndexOptions::Docs, false);
                }
                if doc_values {
                    fs = fs.with_doc_values(DocValuesType::SortedSet);
                }
                let sa = match f.raw("search_analyzer").and_then(Value::as_str) {
                    Some(n) if os => self.search_analyzer(n)?,
                    _ if os && f.raw("analyzer").is_some() => a.clone(),
                    _ => self.analysis.default_search_analyzer().unwrap_or_else(|| a.clone()),
                };
                info.analyzer = Some(a.clone());
                info.search_analyzer = Some(sa);
                info.search_quote_analyzer = info.search_analyzer.clone();
                if os && info.fielddata {
                    let sub = format!("{path}._fielddata");
                    let n = numbers.of(&sub);
                    self.schema.add_field(FieldSchema::new(n, sub).with_doc_values(DocValuesType::SortedSet)).map_err(internal)?;
                    info.subfields.push(("fielddata".into(), n));
                }
            }
            Kind::Keyword | Kind::Version | Kind::CountedKeyword => {
                let opts = match f.effective("index_options", ctx).as_str() {
                    Some("freqs") => IndexOptions::DocsAndFreqs,
                    _ => IndexOptions::Docs,
                };
                info.norms = index && b("norms", false);
                if index {
                    info.index_options = opts;
                    fs = fs.indexed(opts, info.norms);
                }
                if doc_values {
                    fs = fs.with_doc_values(DocValuesType::SortedSet);
                }
                // `counted_keyword`: each doc's occurrence count per distinct value (the
                // mapper's `<name>_count` binary doc values).
                if info.kind == Kind::CountedKeyword {
                    let sub = format!("{path}\u{0}count");
                    let n = numbers.of(&sub);
                    self.schema.add_field(FieldSchema::new(n, sub).with_doc_values(DocValuesType::Binary)).map_err(internal)?;
                    info.subfields.push(("count".into(), n));
                }
                if let Some(s) = sim.clone() {
                    fs = fs.with_similarity(s);
                }
                if let Some(n) = f.effective("normalizer", ctx).as_str() {
                    info.normalizer =
                        Some(self.analysis.normalizer(n).ok_or_else(|| EsError::illegal_argument(format!("normalizer [{n}] not found for field [{path}]")))?);
                    if b("split_queries_on_whitespace", false) {
                        info.whitespace_analyzer = self.analysis.whitespace_normalizer(n).map(|w| w.analyzer().clone());
                    }
                } else if b("split_queries_on_whitespace", false) {
                    info.whitespace_analyzer = self.analysis.analyzer("whitespace");
                }
                info.ignore_above = ignore_above();
                if let Some(tv) = term_vector(f, ctx) {
                    fs = fs.with_term_vectors(tv);
                }
            }
            Kind::ConstantKeyword => {
                info.constant_value = f.effective("value", ctx).as_str().map(str::to_string);
            }
            Kind::Wildcard => {
                fs = fs.with_doc_values(DocValuesType::SortedSet).indexed(IndexOptions::Docs, false);
                info.index_options = IndexOptions::Docs;
                info.index = true;
                info.doc_values = true;
                info.ignore_above = ignore_above();
                // OpenSearch's `wildcard` takes a normalizer (`default` is none).
                if let Some(n) = f.raw("normalizer").and_then(Value::as_str).filter(|n| *n != "default") {
                    info.normalizer =
                        Some(self.analysis.normalizer(n).ok_or_else(|| EsError::illegal_argument(format!("normalizer [{n}] not found for field [{path}]")))?);
                }
            }
            Kind::Long | Kind::Date | Kind::DateNanos | Kind::ScaledFloat(_) | Kind::UnsignedLong | Kind::Double => {
                if index {
                    fs = fs.with_points(PointsSpec::new(1, 8));
                }
                if doc_values {
                    fs = fs.with_doc_values(DocValuesType::SortedNumeric);
                }
                if kind.is_date() {
                    info.date_format = Some(f.date_formatter(ctx)?);
                }
            }
            Kind::Integer | Kind::Short | Kind::Byte | Kind::Float | Kind::TokenCount => {
                if index {
                    fs = fs.with_points(PointsSpec::new(1, 4));
                }
                if doc_values {
                    fs = fs.with_doc_values(DocValuesType::SortedNumeric);
                }
                if kind == Kind::TokenCount {
                    info.analyzer = Some(self.analyzer(f.effective("analyzer", ctx).as_str().unwrap_or("default"))?);
                }
            }
            Kind::HalfFloat => {
                if index {
                    fs = fs.with_points(PointsSpec::new(1, 2));
                }
                if doc_values {
                    fs = fs.with_doc_values(DocValuesType::SortedNumeric);
                }
            }
            Kind::Boolean => {
                if index {
                    fs = fs.indexed(IndexOptions::Docs, false);
                    info.index_options = IndexOptions::Docs;
                }
                if doc_values {
                    fs = fs.with_doc_values(DocValuesType::SortedNumeric);
                }
            }
            Kind::Ip => {
                if index {
                    fs = fs.with_points(PointsSpec::new(1, 16));
                }
                if doc_values {
                    fs = fs.with_doc_values(DocValuesType::SortedSet);
                }
            }
            Kind::Binary | Kind::Histogram => {
                if doc_values || kind == Kind::Histogram {
                    fs = fs.with_doc_values(DocValuesType::Binary);
                }
            }
            Kind::GeoPoint | Kind::Point => {
                if index {
                    fs = fs.with_points(PointsSpec::new(2, 4));
                }
                if doc_values {
                    fs = fs.with_doc_values(DocValuesType::SortedNumeric);
                }
            }
            Kind::GeoShape | Kind::Shape => {
                if index {
                    fs = fs.with_points(search_index::query::shape::SHAPE_POINTS);
                }
                if doc_values {
                    fs = fs.with_doc_values(DocValuesType::Binary);
                    // The doc's centroid, computed from its original coordinates (Elasticsearch
                    // keeps it in the doc value's header); `\0` keeps it out of field listings.
                    let sub = format!("{path}\u{0}centroid");
                    let n = numbers.of(&sub);
                    self.schema.add_field(FieldSchema::new(n, sub).with_doc_values(DocValuesType::Binary)).map_err(internal)?;
                    info.subfields.push(("centroid".into(), n));
                }
            }
            Kind::IntegerRange | Kind::FloatRange => {
                if index {
                    fs = fs.with_points(PointsSpec::new(2, 4));
                }
                if doc_values {
                    fs = fs.with_doc_values(DocValuesType::Binary);
                }
            }
            Kind::LongRange | Kind::DoubleRange | Kind::DateRange => {
                if index {
                    fs = fs.with_points(PointsSpec::new(2, 8));
                }
                if doc_values {
                    fs = fs.with_doc_values(DocValuesType::Binary);
                }
                if kind == Kind::DateRange {
                    info.date_format = Some(f.date_formatter(ctx)?);
                }
            }
            Kind::IpRange => {
                if index {
                    fs = fs.with_points(PointsSpec::new(2, 16));
                }
                if doc_values {
                    fs = fs.with_doc_values(DocValuesType::Binary);
                }
            }
            Kind::DenseVector => {
                let spec = vector_spec(f, ctx);
                if spec.dims > 0 && index {
                    info.vector_layouts = numbers.with_prefix(&format!("{path}\u{0}")).into_iter().filter(|n| *n != number).collect();
                }
                info.vector_similarity = if f.ty == "knn_vector" {
                    crate::opensearch::knn::similarity_name(f, ctx)
                } else {
                    f.effective("similarity", ctx).as_str().map(str::to_string)
                };
                info.vector = Some(spec);
                info.doc_values = false;
                if spec.dims > 0 {
                    if index {
                        fs = fs.with_vectors(spec);
                    } else {
                        fs = fs.with_doc_values(DocValuesType::Binary);
                    }
                }
            }
            Kind::SparseVector | Kind::RankFeatures => {
                fs = fs.indexed(IndexOptions::DocsAndFreqs, false);
                info.index_options = IndexOptions::DocsAndFreqs;
                info.doc_values = false;
            }
            Kind::RankFeature => {
                info.doc_values = false;
            }
            Kind::Completion => {
                // Docs-only postings of the analyzed keys, next to the suggester's FST
                // (`CompletionFieldMapper`: `IndexOptions.DOCS`, with norms: one token per input).
                fs = fs.with_suggest().indexed(IndexOptions::Docs, true);
                info.index_options = IndexOptions::Docs;
                info.doc_values = false;
                let a = self.analyzer(f.effective("analyzer", ctx).as_str().unwrap_or("simple"))?;
                let sa = match f.raw("search_analyzer").and_then(Value::as_str) {
                    Some(n) => self.search_analyzer(n)?,
                    None => a.clone(),
                };
                info.analyzer = Some(a);
                info.search_analyzer = Some(sa);
            }
            Kind::Flattened => {
                fs = fs.indexed(IndexOptions::Docs, false);
                if doc_values {
                    fs = fs.with_doc_values(DocValuesType::SortedSet);
                }
                let sub = format!("{path}._keyed");
                let n = numbers.of(&sub);
                let mut kfs = FieldSchema::new(n, sub).indexed(IndexOptions::Docs, false);
                if doc_values {
                    kfs = kfs.with_doc_values(DocValuesType::SortedSet);
                }
                self.schema.add_field(kfs).map_err(internal)?;
                info.keyed_field = Some(n);
                info.ignore_above = ignore_above();
                info.index_options = IndexOptions::Docs;
            }
            Kind::Join => {
                fs = fs.indexed(IndexOptions::Docs, false).with_doc_values(DocValuesType::SortedSet);
                info.index_options = IndexOptions::Docs;
                // One `<join>#<parent>` sorted field per parent type holds the parent's id.
                if let Some(Value::Obj(rels)) = f.raw("relations") {
                    for parent in rels.keys() {
                        let sub = format!("{path}#{parent}");
                        let n = numbers.of(&sub);
                        self.schema
                            .add_field(FieldSchema::new(n, sub.clone()).indexed(IndexOptions::Docs, false).with_doc_values(DocValuesType::Sorted))
                            .map_err(internal)?;
                        info.subfields.push((parent.clone(), n));
                    }
                }
            }
            Kind::AggregateMetric => {
                for m in ["min", "max", "sum", "value_count"] {
                    let sub = format!("{path}.{m}");
                    let n = numbers.of(&sub);
                    self.schema.add_field(FieldSchema::new(n, sub).with_doc_values(DocValuesType::SortedNumeric)).map_err(internal)?;
                    info.subfields.push((m.to_string(), n));
                }
            }
            // The stored query (`query_builder_field`), read back by `percolate` queries.
            Kind::Percolator => {
                fs = fs.with_doc_values(DocValuesType::Binary);
            }
            Kind::Other => {}
        }
        self.schema.add_field(fs).map_err(internal)?;
        // A join field's `<join>#<parent>` fields (`ParentIdFieldMapper`) are keyword-like
        // fields of their own: term queries, sorts, aggregations and doc values read them.
        if info.kind == Kind::Join {
            for (parent, n) in &info.subfields {
                let mut sub = info.clone();
                sub.name = format!("{path}#{parent}");
                sub.number = *n;
                sub.kind = Kind::Keyword;
                sub.subfields = Vec::new();
                let sub = Arc::new(sub);
                self.by_number.insert(*n, sub.clone());
                self.fields.insert(sub.name.clone(), sub);
            }
        }
        let info = Arc::new(info);
        self.by_number.insert(number, info.clone());
        self.fields.insert(path.to_string(), info);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn sub_text(&mut self, name: &str, n: u32, parent: &FieldInfo, a: Analyzer, sa: Analyzer, opts: IndexOptions, sim: Option<Similarity>) -> EsResult<()> {
        let mut fs = FieldSchema::new(n, name).indexed(opts, parent.norms);
        if let Some(s) = sim {
            fs = fs.with_similarity(s);
        }
        self.schema.add_field(fs).map_err(internal)?;
        let mut info = parent.clone();
        info.name = name.to_string();
        info.number = n;
        info.kind = Kind::Text;
        info.ty = "text".into();
        info.analyzer = Some(a);
        info.search_analyzer = Some(sa.clone());
        info.search_quote_analyzer = Some(sa);
        info.index_options = opts;
        info.prefix_field = None;
        info.phrase_field = None;
        info.shingle_fields = Vec::new();
        info.store = false;
        info.fielddata = false;
        info.parent = Some(parent.name.clone());
        let info = Arc::new(info);
        self.by_number.insert(n, info.clone());
        self.fields.insert(name.to_string(), info);
        Ok(())
    }
}

/// The vector index layout of a `dense_vector` mapping (Elasticsearch's defaults: `int8_hnsw`
/// for floats under 384 dims, `bbq_hnsw` from 384, `hnsw` for bytes and bits).
pub fn vector_spec(f: &FieldMapper, ctx: &MappingCtx) -> VectorSpec {
    if f.ty == "knn_vector" {
        return crate::opensearch::knn::vector_spec(f, ctx);
    }
    let dims = f.effective("dims", ctx).as_i64().unwrap_or(0).max(0) as u32;
    let element = match f.effective("element_type", ctx).as_str() {
        Some("byte") => VectorElement::Byte,
        Some("bit") => VectorElement::Bit,
        _ => VectorElement::Float32,
    };
    let similarity = match f.effective("similarity", ctx).as_str() {
        Some("l2_norm") => VectorSimilarity::L2Norm,
        Some("dot_product") => VectorSimilarity::DotProduct,
        Some("max_inner_product") => VectorSimilarity::MaxInnerProduct,
        _ if element == VectorElement::Bit => VectorSimilarity::L2Norm,
        _ => VectorSimilarity::Cosine,
    };
    let opts = f.effective("index_options", ctx);
    let default_ty = match element {
        VectorElement::Float32 if dims >= 384 => "bbq_hnsw",
        VectorElement::Float32 => "int8_hnsw",
        _ => "hnsw",
    };
    let ty = opts.get("type").and_then(Value::as_str).unwrap_or(default_ty);
    let m = opts.get("m").and_then(Value::as_i64).unwrap_or(16) as u16;
    let ef = opts.get("ef_construction").and_then(Value::as_i64).unwrap_or(100) as u16;
    let hnsw = if ty.contains("flat") { None } else { Some(HnswParams { m, ef_construction: ef }) };
    let ci = opts.get("confidence_interval").and_then(Value::as_f64).map(|v| v as f32);
    let quantization = match ty {
        "int8_hnsw" | "int8_flat" => VectorQuantization::Scalar { bits: 8, confidence_interval: ci },
        "int4_hnsw" | "int4_flat" => VectorQuantization::Scalar { bits: 4, confidence_interval: ci },
        "bbq_hnsw" | "bbq_flat" | "bbq_disk" => VectorQuantization::Binary,
        _ => VectorQuantization::None,
    };
    VectorSpec { dims, element, similarity, hnsw, quantization }
}

fn term_vector(f: &FieldMapper, ctx: &MappingCtx) -> Option<TermVectorOptions> {
    let tv = f.effective("term_vector", ctx);
    Some(match tv.as_str()? {
        "yes" => TermVectorOptions::default(),
        "with_positions" => TermVectorOptions { positions: true, ..Default::default() },
        "with_offsets" => TermVectorOptions { offsets: true, ..Default::default() },
        "with_positions_offsets" => TermVectorOptions { positions: true, offsets: true, payloads: false },
        "with_positions_payloads" => TermVectorOptions { positions: true, offsets: false, payloads: true },
        "with_positions_offsets_payloads" => TermVectorOptions { positions: true, offsets: true, payloads: true },
        _ => return None,
    })
}
