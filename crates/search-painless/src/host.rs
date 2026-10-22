//! The interface between a running script and its embedding (`search-es`).
//!
//! A [`Host`] answers per-document questions: doc values of a field, the `_source`, the score,
//! term statistics, document identity for `randomScore`. Everything has a default so a host
//! only implements what its context needs. Doc values are written into a caller-owned
//! [`DocValues`] buffer that is reused from document to document, so reading
//! `doc['price'].value` in a `script_score` over millions of documents does not allocate.

use crate::types::{cls, ClassId};
use crate::value::Value;
use crate::vm::{Thrown, VR};

/// Field data types as scripts see them. The kind fixes the `ScriptDocValues` class `doc[…]`
/// returns, the field class `field(…)` returns, and which [`DocValues`] buffer holds the values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DocKind {
    /// `long` (and `token_count`, …): `ScriptDocValues.Longs` over `longs`.
    #[default]
    Long,
    /// `integer`: `Longs` for `doc`, `IntegerDocValuesField` (int values) for `field`.
    Integer,
    /// `short`.
    Short,
    /// `byte`.
    Byte,
    /// `unsigned_long`: the raw 64 bits in `longs`.
    UnsignedLong,
    /// `double`: `ScriptDocValues.Doubles` over `doubles`.
    Double,
    /// `float`: values widened from `f32` in `doubles`.
    Float,
    /// `half_float`: values rounded to half precision, widened, in `doubles`.
    HalfFloat,
    /// `scaled_float`: decoded values (`round(v * factor) / factor`) in `doubles`.
    ScaledFloat,
    /// `keyword`, `text` with fielddata, `constant_keyword`, `wildcard`… (`ScriptDocValues.Strings`)
    String,
    /// `boolean`: 0/1 in `longs`.
    Boolean,
    /// `date`: epoch milliseconds in `longs`.
    Date,
    /// `date_nanos`: epoch nanoseconds in `longs`.
    DateNanos,
    /// `geo_point`: `[lat, lon]` pairs (already quantized like Lucene) in `geo`.
    GeoPoint,
    /// `geo_shape`: one shape per document. `geo` holds its centroid, the top-left and
    /// bottom-right corners of its bounding box and its label position (`[lat, lon]` each, as
    /// the doc values encode them); `longs[0]` its dimensional type (0 points, 1 lines,
    /// 2 polygons).
    GeoShape,
    /// `ip`: formatted addresses in `strings`.
    Ip,
    /// `binary`: raw bytes in `bytes`.
    Binary,
    /// `version`: formatted versions in `strings`.
    Version,
    /// `dense_vector`: one vector, in `vector` or `vector_bytes` by its
    /// [`DocValues::element`].
    DenseVector,
    /// `rank_vectors`: [`DocValues::vector_count`] vectors of equal length, one after the other
    /// in `vector` or `vector_bytes` by [`DocValues::element`].
    RankVectors,
}

impl DocKind {
    fn storage(self) -> Storage {
        use DocKind::*;
        match self {
            Long | Integer | Short | Byte | UnsignedLong | Boolean | Date | DateNanos => Storage::Longs,
            Double | Float | HalfFloat | ScaledFloat => Storage::Doubles,
            String | Ip | Version => Storage::Strings,
            GeoPoint | GeoShape => Storage::Geo,
            Binary => Storage::Bytes,
            DenseVector | RankVectors => Storage::Vector,
        }
    }
}

enum Storage {
    Longs,
    Doubles,
    Strings,
    Geo,
    Bytes,
    Vector,
}

/// Element type of a `dense_vector` field (`element_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VectorElement {
    /// `float` (the default): values in [`DocValues::vector`].
    #[default]
    Float,
    /// `bfloat16`: values (already rounded to bfloat16 by the codec) in [`DocValues::vector`];
    /// scripts treat them as float vectors.
    BFloat16,
    /// `byte`: values in [`DocValues::vector_bytes`].
    Byte,
    /// `bit`: packed bits in [`DocValues::vector_bytes`], eight dimensions per byte, the first
    /// dimension in the most significant bit.
    Bit,
}

/// Doc values of one field for the current document. Hosts clear and refill it.
#[derive(Default, Debug)]
pub struct DocValues {
    pub kind: DocKind,
    pub longs: Vec<i64>,
    pub doubles: Vec<f64>,
    strings: Vec<String>,
    nstrings: usize,
    pub geo: Vec<[f64; 2]>,
    pub bytes: Vec<Vec<u8>>,
    pub vector: Vec<f32>,
    /// A `byte` or `bit` vector (see [`VectorElement`]).
    pub vector_bytes: Vec<i8>,
    /// Element type of a `dense_vector`.
    pub element: VectorElement,
    /// The vector is only in doc values (`index: false`): Elasticsearch's `Binary*DenseVector`
    /// implementations, whose magnitude is stored ([`DocValues::magnitude`]). Indexed vectors
    /// (`*KnnDenseVector`) compute it.
    pub vector_binary: bool,
    /// Stored vector magnitude (doc-values-only vectors).
    pub magnitude: Option<f32>,
    /// Number of vectors of a `rank_vectors` field.
    pub vector_count: usize,
    /// Stored magnitudes of a `rank_vectors` field's vectors (computed when empty).
    pub magnitudes: Vec<f32>,
}

impl DocValues {
    pub fn clear(&mut self, kind: DocKind) {
        self.kind = kind;
        self.longs.clear();
        self.doubles.clear();
        self.nstrings = 0;
        self.geo.clear();
        self.bytes.clear();
        self.vector.clear();
        self.vector_bytes.clear();
        self.element = VectorElement::Float;
        self.vector_binary = false;
        self.magnitude = None;
        self.vector_count = 0;
        self.magnitudes.clear();
    }

    /// Appends a string value, reusing an allocation from a previous document.
    pub fn push_str(&mut self, s: &str) {
        if self.nstrings < self.strings.len() {
            let slot = &mut self.strings[self.nstrings];
            slot.clear();
            slot.push_str(s);
        } else {
            self.strings.push(s.to_string());
        }
        self.nstrings += 1;
    }

    pub fn strings(&self) -> &[String] {
        &self.strings[..self.nstrings]
    }

    /// Number of values.
    pub fn len(&self) -> usize {
        match self.kind.storage() {
            Storage::Longs => self.longs.len(),
            Storage::Doubles => self.doubles.len(),
            Storage::Strings => self.nstrings,
            Storage::Geo if self.kind == DocKind::GeoShape => usize::from(!self.geo.is_empty()),
            Storage::Geo => self.geo.len(),
            Storage::Bytes => self.bytes.len(),
            // `RankVectorsScriptDocValues.size()` counts the vectors
            Storage::Vector if self.kind == DocKind::RankVectors => self.vector_count,
            Storage::Vector => usize::from(!self.vector.is_empty() || !self.vector_bytes.is_empty()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A document's `dense_vector` borrowed from the host's storage ([`Host::with_vector`]).
#[derive(Clone, Copy, Debug)]
pub struct VectorView<'a> {
    pub element: VectorElement,
    /// `float` / `bfloat16` values.
    pub floats: &'a [f32],
    /// `byte` values or packed `bit`s.
    pub bytes: &'a [i8],
    /// Doc-values-only vector (`index: false`).
    pub binary: bool,
    /// Stored magnitude (doc-values-only vectors).
    pub magnitude: Option<f32>,
}

/// Lexical statistics for `_termStats` (score scripts over `match` queries).
#[derive(Clone, Debug, Default)]
pub struct TermStats {
    pub unique_terms_count: i32,
    pub matched_terms_count: i32,
    pub doc_freq: Vec<f64>,
    pub total_term_freq: Vec<f64>,
    pub term_freq: Vec<f64>,
    /// Positions of each term in the document.
    pub term_positions: Vec<Vec<f64>>,
}

/// What a host reports when it cannot answer.
#[derive(Clone, Debug, PartialEq)]
pub struct HostError {
    /// Elasticsearch exception type, e.g. `illegal_argument_exception`.
    pub kind: &'static str,
    pub reason: String,
    /// Java frames to report above the script frame (`org.elasticsearch.server@9.5.5/…`),
    /// innermost first.
    pub frames: Vec<String>,
}

impl HostError {
    pub fn illegal_argument(reason: impl Into<String>) -> HostError {
        HostError { kind: "illegal_argument_exception", reason: reason.into(), frames: Vec::new() }
    }
    pub fn with_frames(mut self, frames: &[&str]) -> HostError {
        self.frames = frames.iter().map(|f| f.to_string()).collect();
        self
    }
}

/// Per-document data a running script reads.
pub trait Host {
    /// `_score` of the current document.
    fn score(&mut self) -> f64 {
        0.0
    }

    /// Fills `out` with the doc values of `field` for the current document. `Ok(false)` means
    /// the field is not mapped (Elasticsearch: `No field found for [field] in mapping`).
    fn doc_values(&mut self, field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        let _ = (field, out);
        Ok(false)
    }

    /// [`Host::doc_values`] for the `slot`-th field of [`crate::Script::doc_fields`] (the
    /// `doc['literal']` reads of the script). Hosts that resolve those fields once per segment
    /// answer here without looking the name up again; the default forwards to `doc_values`.
    fn doc_values_at(&mut self, slot: usize, field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        let _ = slot;
        self.doc_values(field, out)
    }

    /// Values of `field` for the field API (`field('f')`, `$('f', d)`). Elasticsearch loads
    /// these with `FielddataOperation.SCRIPT`, which falls back to `_source` where `doc[...]`
    /// fails (a `text` field without fielddata reads its source values as strings, sorted);
    /// the default forwards to `doc_values`.
    fn field_values(&mut self, field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        self.doc_values(field, out)
    }

    /// Whether `field` is mapped (`doc.containsKey(field)`).
    fn has_field(&mut self, field: &str) -> bool {
        let mut dv = DocValues::default();
        matches!(self.doc_values(field, &mut dv), Ok(true))
    }

    /// The `_source` of the current document (a map), for `params._source` and `ctx._source`
    /// in contexts that read it lazily.
    fn source(&mut self) -> Option<Value> {
        None
    }

    /// Term statistics of the current document (`_termStats`).
    fn term_stats(&mut self) -> Option<TermStats> {
        None
    }

    /// Document id within the shard (segment base + doc), for `randomScore`.
    fn doc_id(&mut self) -> i32 {
        0
    }

    /// Shard id and index name, mixed into the `randomScore` seed. An empty index name means
    /// the index cannot be looked up (as in the `_execute` API), which `randomScore` reports.
    fn shard(&mut self) -> (i32, String) {
        (0, String::new())
    }

    /// Stored values of `field` for `params._fields['field']` of search scripts
    /// (`LeafStoredFieldsLookup`): `Ok(None)` when the field is not mapped ("No field found
    /// for [field] in mapping"), an empty list when it is mapped but not stored or missing.
    /// Values in the field's display types (`String`, `Long`, `Double`, dates as
    /// `ZonedDateTime` via [`crate::value::zoned_date_time_millis`], …).
    fn stored_field(&mut self, field: &str) -> Result<Option<Vec<Value>>, HostError> {
        let _ = field;
        Ok(None)
    }

    /// Current time in milliseconds (`ctx._now` is set by the caller; this backs
    /// `metadata().getNow()` when the host tracks it).
    fn now_millis(&mut self) -> i64 {
        0
    }

    /// Lends the current document's `dense_vector` of `field` to `f` without copying it into
    /// a [`DocValues`] buffer — the vector score functions call this first. `Ok(None)`: not
    /// offered (the default; [`Host::doc_values`] is used); `Ok(Some(false))`: the document
    /// has no vector; `Ok(Some(true))`: `f` ran.
    fn with_vector(&mut self, field: &str, f: &mut dyn FnMut(&VectorView<'_>)) -> Result<Option<bool>, HostError> {
        let _ = (field, f);
        Ok(None)
    }

    /// Polled every 1,000 loop iterations of an instance made cancellable with
    /// [`crate::Instance::set_cancellable`] (Elasticsearch's `ContextIndexSearcher::checkCancelled`:
    /// search timeouts and task cancellation). An error stops the script; it is reported as a
    /// `script_exception` caused by it.
    fn check_cancelled(&mut self) -> Result<(), HostError> {
        Ok(())
    }
}

/// A host that provides nothing (for contexts without documents).
pub struct NoHost;
impl Host for NoHost {}

/// An object a host exposes to scripts with its own members.
pub trait HostObject {
    fn class(&self) -> ClassId {
        cls::HOST_OBJECT
    }
    fn call(&self, name: &str, args: &[Value]) -> VR;
    fn get(&self, name: &str) -> VR;
    fn set(&self, name: &str, v: Value) -> Result<(), Thrown>;
    fn iter(&self) -> Option<Vec<Value>> {
        None
    }
    fn to_java_string(&self) -> String;
    fn to_json(&self) -> serde_json::Value {
        serde_json::Value::String(self.to_java_string())
    }
    fn equals(&self, other: &Value) -> bool {
        let _ = other;
        false
    }
}
