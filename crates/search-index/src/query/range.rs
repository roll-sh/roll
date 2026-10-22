//! Range and set queries over points and doc values, and Lucene's `IndexOrDocValuesQuery`.
//!
//! - [`PointRangeQuery`]: walks the BKD tree; cells inside the range add their docs without
//!   checking values (whole doc ranges at once for leaves of consecutive docs), crossing cells
//!   check values. A segment whose values all fall inside matches all its docs.
//! - [`PointInSetQuery`]: exact numeric values (`terms` on numbers).
//! - [`DocValuesRangeQuery`] / [`DocValuesTermsQuery`]: two-phase scorers over doc values —
//!   the approximation is the field's docs and the check reads the value — used when another
//!   clause leads the iteration, and for fields with doc values only.
//! - [`IndexOrDocValuesQuery`]: Elasticsearch's numeric/date ranges: points when the range
//!   leads, doc values when a cheaper clause leads (Lucene's 8x doc-values penalty).

use std::any::Any;
use std::sync::Arc;

use search_codec::points::encode;
use search_codec::{DocValuesReader, NO_MORE_DOCS, PointVisitor, Relation};

use crate::error::{Error, Result};
use crate::query::basic::{SetScorer, explain_const};
use crate::query::term::term_text;
use crate::schema::Schema;
use crate::search::{
    Explanation, Query, QueryRef, Scorable, ScoreMode, Scorer, ScorerSupplier, Searcher, SegmentContext, Weight,
};
use crate::util::bitset::{DocIdSet, DocIdSetBuilder};
use crate::util::java::{double_to_string, float_to_string};

/// How a point field's packed values print (Lucene's per-type `toString`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointType {
    Int,
    Long,
    Float,
    Double,
    HalfFloat,
    /// 16-byte IPv6-mapped addresses.
    InetAddress,
    /// Raw bytes (binary points), printed as Lucene's `BytesRef` hex.
    Bytes,
    /// `LatLonPoint`: dimension 0 latitude, 1 longitude.
    LatLon,
}

impl PointType {
    /// Decoded text of one dimension.
    pub fn display(self, b: &[u8]) -> String {
        match self {
            PointType::Int if b.len() == 4 => encode::i32_from(b).to_string(),
            PointType::Long if b.len() == 8 => encode::i64_from(b).to_string(),
            PointType::Float if b.len() == 4 => float_to_string(encode::sortable_f32(encode::i32_from(b))),
            PointType::Double if b.len() == 8 => double_to_string(encode::sortable_f64(encode::i64_from(b))),
            PointType::HalfFloat if b.len() == 2 => {
                let s = ((b[0] ^ 0x80) as i16) << 8 | b[1] as i16;
                float_to_string(encode::sortable_f16(s))
            }
            PointType::InetAddress if b.len() == 16 => inet_to_string(b),
            _ => {
                let mut s = String::from("[");
                for (i, x) in b.iter().enumerate() {
                    if i > 0 {
                        s.push(' ');
                    }
                    s.push_str(&format!("{x:x}"));
                }
                s.push(']');
                s
            }
        }
    }
}

/// Guava's `InetAddresses.toAddrString` of a 16-byte address (IPv4-mapped print as IPv4).
pub fn inet_to_string(b: &[u8]) -> String {
    if b[..10].iter().all(|&x| x == 0) && b[10] == 0xff && b[11] == 0xff {
        return format!("{}.{}.{}.{}", b[12], b[13], b[14], b[15]);
    }
    let words: Vec<u16> = (0..8).map(|i| u16::from_be_bytes([b[2 * i], b[2 * i + 1]])).collect();
    // Longest run of zero words (length >= 2) becomes "::".
    let (mut best, mut best_len, mut cur, mut cur_len) = (usize::MAX, 0, 0, 0);
    for (i, &w) in words.iter().enumerate() {
        if w == 0 {
            if cur_len == 0 {
                cur = i;
            }
            cur_len += 1;
            if cur_len > best_len {
                best = cur;
                best_len = cur_len;
            }
        } else {
            cur_len = 0;
        }
    }
    if best_len < 2 {
        best = usize::MAX;
    }
    let mut s = String::new();
    let mut i = 0;
    while i < 8 {
        if i == best {
            s.push_str("::");
            i += best_len;
            continue;
        }
        if !s.is_empty() && !s.ends_with(':') {
            s.push(':');
        }
        s.push_str(&format!("{:x}", words[i]));
        i += 1;
    }
    s
}

/// Docs with a point inside `[lower, upper]` in every dimension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PointRangeQuery {
    pub field: u32,
    /// Packed `dims × bytes_per_dim` bounds (inclusive).
    pub lower: Vec<u8>,
    pub upper: Vec<u8>,
    pub bytes_per_dim: usize,
    pub kind: PointType,
}

impl PointRangeQuery {
    pub fn new(field: u32, lower: Vec<u8>, upper: Vec<u8>, bytes_per_dim: usize, kind: PointType) -> Result<PointRangeQuery> {
        if lower.len() != upper.len() || bytes_per_dim == 0 || !lower.len().is_multiple_of(bytes_per_dim) {
            return Err(Error::illegal("lowerPoint and upperPoint must have the same length"));
        }
        Ok(PointRangeQuery { field, lower, upper, bytes_per_dim, kind })
    }

    /// A 1-d long range (`LongPoint.newRangeQuery`).
    pub fn long(field: u32, lower: i64, upper: i64) -> PointRangeQuery {
        PointRangeQuery {
            field,
            lower: encode::i64_bytes(lower).to_vec(),
            upper: encode::i64_bytes(upper).to_vec(),
            bytes_per_dim: 8,
            kind: PointType::Long,
        }
    }

    pub fn int(field: u32, lower: i32, upper: i32) -> PointRangeQuery {
        PointRangeQuery {
            field,
            lower: encode::i32_bytes(lower).to_vec(),
            upper: encode::i32_bytes(upper).to_vec(),
            bytes_per_dim: 4,
            kind: PointType::Int,
        }
    }

    pub fn double(field: u32, lower: f64, upper: f64) -> PointRangeQuery {
        PointRangeQuery {
            field,
            lower: encode::f64_bytes(lower).to_vec(),
            upper: encode::f64_bytes(upper).to_vec(),
            bytes_per_dim: 8,
            kind: PointType::Double,
        }
    }

    pub fn float(field: u32, lower: f32, upper: f32) -> PointRangeQuery {
        PointRangeQuery {
            field,
            lower: encode::f32_bytes(lower).to_vec(),
            upper: encode::f32_bytes(upper).to_vec(),
            bytes_per_dim: 4,
            kind: PointType::Float,
        }
    }

    fn dims(&self) -> usize {
        self.lower.len() / self.bytes_per_dim
    }

    fn empty(&self) -> bool {
        let b = self.bytes_per_dim;
        (0..self.dims()).any(|d| self.lower[d * b..(d + 1) * b] > self.upper[d * b..(d + 1) * b])
    }

    /// Matches of one segment as a doc id set.
    pub(crate) fn docs(&self, seg: &SegmentContext) -> Option<DocIdSet> {
        let p = seg.reader.points(self.field)?;
        if self.empty() || p.spec().packed_len() != self.lower.len() {
            return None;
        }
        let max_doc = seg.max_doc();
        let mut v = RangeVisitor { q: self, b: DocIdSetBuilder::new(max_doc) };
        // Whole segment inside: every doc with a value.
        if v.compare(p.min_packed_value(), p.max_packed_value()) == Relation::CellInsideQuery && p.doc_count() == max_doc {
            return Some(DocIdSet::All(max_doc));
        }
        p.intersect(&mut v);
        Some(v.b.build())
    }

    fn estimate(&self, seg: &SegmentContext) -> u64 {
        let Some(p) = seg.reader.points(self.field) else { return 0 };
        if self.empty() || p.spec().packed_len() != self.lower.len() {
            return 0;
        }
        let mut v = RangeVisitor { q: self, b: DocIdSetBuilder::new(1) };
        p.estimate_doc_count(&mut v)
    }
}

struct RangeVisitor<'a> {
    q: &'a PointRangeQuery,
    b: DocIdSetBuilder,
}

impl PointVisitor for RangeVisitor<'_> {
    fn compare(&mut self, min: &[u8], max: &[u8]) -> Relation {
        let b = self.q.bytes_per_dim;
        let mut crosses = false;
        for d in 0..self.q.dims() {
            let r = d * b..(d + 1) * b;
            let (lo, hi) = (&self.q.lower[r.clone()], &self.q.upper[r.clone()]);
            if &min[r.clone()] > hi || &max[r.clone()] < lo {
                return Relation::CellOutsideQuery;
            }
            crosses |= &min[r.clone()] < lo || &max[r] > hi;
        }
        if crosses { Relation::CellCrossesQuery } else { Relation::CellInsideQuery }
    }
    fn visit(&mut self, doc: u32) {
        self.b.add(doc);
    }
    fn visit_value(&mut self, doc: u32, packed: &[u8]) {
        let b = self.q.bytes_per_dim;
        for d in 0..self.q.dims() {
            let r = d * b..(d + 1) * b;
            if packed[r.clone()] < self.q.lower[r.clone()] || packed[r.clone()] > self.q.upper[r] {
                return;
            }
        }
        self.b.add(doc);
    }
    fn visit_doc_range(&mut self, start: u32, end: u32) {
        self.b.add_range(start, end);
    }
    fn grow(&mut self, n: usize) {
        self.b.grow(n);
    }
}

struct PointRangeWeight {
    q: PointRangeQuery,
    score: f32,
    desc: String,
}

struct SetSupplier {
    f: Box<dyn FnOnce() -> Option<DocIdSet> + Send>,
    cost: u64,
    score: f32,
}

impl ScorerSupplier for SetSupplier {
    fn cost(&self) -> u64 {
        self.cost
    }
    fn get(self: Box<Self>, _lead: u64) -> Result<Box<dyn Scorer>> {
        let set = (self.f)().unwrap_or(DocIdSet::Empty);
        Ok(Box::new(SetScorer::new(Arc::new(set), self.score)))
    }
}

impl Weight for PointRangeWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        Ok(self.q.docs(seg).map(|s| Box::new(SetScorer::new(Arc::new(s), self.score)) as Box<dyn Scorer>))
    }
    fn scorer_supplier(&self, seg: &SegmentContext) -> Result<Option<Box<dyn ScorerSupplier>>> {
        let Some(p) = seg.reader.points(self.q.field) else { return Ok(None) };
        if self.q.empty() || p.spec().packed_len() != self.q.lower.len() {
            return Ok(None);
        }
        let cost = self.q.estimate(seg);
        let q = self.q.clone();
        let seg = seg.clone();
        Ok(Some(Box::new(SetSupplier { f: Box::new(move || q.docs(&seg)), cost, score: self.score })))
    }
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        explain_const(self, seg, doc, &self.desc, self.score)
    }
    fn count(&self, seg: &SegmentContext) -> Result<Option<u32>> {
        if seg.has_deletions() {
            return Ok(None);
        }
        let Some(p) = seg.reader.points(self.q.field) else { return Ok(Some(0)) };
        if self.q.empty() {
            return Ok(Some(0));
        }
        let mut v = RangeVisitor { q: &self.q, b: DocIdSetBuilder::new(1) };
        let r = v.compare(p.min_packed_value(), p.max_packed_value());
        // Single-valued (one point per doc) and fully inside / outside: exact without walking.
        if r == Relation::CellOutsideQuery {
            return Ok(Some(0));
        }
        if r == Relation::CellInsideQuery && p.size() == p.doc_count() as u64 {
            return Ok(Some(p.doc_count()));
        }
        Ok(None)
    }
}

/// Relation of `[min, max]` (packed, per dimension) to a query's `[lower, upper]` box.
fn relate_box(min: &[u8], max: &[u8], lower: &[u8], upper: &[u8], bytes_per_dim: usize) -> Relation {
    let mut crosses = false;
    for d in 0..lower.len() / bytes_per_dim {
        let r = d * bytes_per_dim..(d + 1) * bytes_per_dim;
        if min[r.clone()] > upper[r.clone()] || max[r.clone()] < lower[r.clone()] {
            return Relation::CellOutsideQuery;
        }
        crosses |= min[r.clone()] < lower[r.clone()] || max[r.clone()] > upper[r];
    }
    if crosses { Relation::CellCrossesQuery } else { Relation::CellInsideQuery }
}

/// Whether every segment that knows `field` can answer a `FieldExistsQuery` on it (doc values,
/// norms or vectors; Lucene's `canRewriteToFieldExistsQuery`).
fn exists_answerable(s: &Searcher, field: u32) -> bool {
    s.segments().iter().all(|seg| {
        seg.reader
            .field_info(field)
            .is_none_or(|fi| fi.doc_values != search_codec::DocValuesType::None || fi.norms || fi.vectors.is_some())
    })
}

/// Lucene's `NumericFieldStats.getStats`: global min, max and doc count of a numeric field
/// from its points (values of at most 8 bytes, decoded as sortable longs), else from the doc
/// values skip index of every segment having the field.
pub(crate) fn numeric_stats(s: &Searcher, field: u32) -> Option<(i64, i64, u64)> {
    if let Some(b) = s.point_bounds(field) {
        if b.min.is_empty() || b.min.len() > 8 {
            return None;
        }
        let decode = |p: &[u8]| -> i64 { p[1..].iter().fold(((p[0] ^ 0x80) as i8) as i64, |a, &x| (a << 8) | x as i64) };
        return Some((decode(&b.min), decode(&b.max), b.doc_count));
    }
    let mut out: Option<(i64, i64, u64)> = None;
    for seg in s.segments() {
        let Some(info) = seg.reader.field_info(field) else { continue };
        // The codec keeps skip data for every field; Lucene only for fields declared with a
        // skip index.
        if !info.doc_values_skip_index {
            return None;
        }
        let sk = match seg.reader.doc_values(field)? {
            DocValuesReader::Numeric(v) => v.skipper(),
            DocValuesReader::SortedNumeric(v) => v.skipper(),
            _ => None,
        }?;
        let (lo, hi, n) = (sk.global_min_value(), sk.global_max_value(), sk.global_doc_count() as u64);
        out = Some(match out {
            None => (lo, hi, n),
            Some((a, b, c)) => (a.min(lo), b.max(hi), c + n),
        });
    }
    out
}

impl Query for PointRangeQuery {
    fn weight(&self, s: &Searcher, _mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        Ok(Box::new(PointRangeWeight { q: self.clone(), score: boost, desc: self.lucene_string(s.schema()) }))
    }
    /// Lucene 10.5's `PointRangeQuery.rewrite`: by the global bounds of the field's points, no
    /// match, every doc (each segment has a point per doc), or `FieldExistsQuery` when the range
    /// covers every value.
    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        use crate::query::basic::{FieldExistsQuery, MatchAllQuery, MatchNoneQuery};
        let dims = self.dims();
        for seg in s.segments() {
            let Some(p) = seg.reader.points(self.field) else { continue };
            let spec = p.spec();
            if spec.index_dims as usize != dims {
                return Err(Error::illegal(format!(
                    "field=\"{}\" was indexed with numIndexDimensions={} but this query has numDims={dims}",
                    s.schema().name(self.field),
                    spec.index_dims
                )));
            }
            if spec.bytes_per_dim as usize != self.bytes_per_dim {
                return Err(Error::illegal(format!(
                    "field=\"{}\" was indexed with bytesPerDim={} but this query has bytesPerDim={}",
                    s.schema().name(self.field),
                    spec.bytes_per_dim,
                    self.bytes_per_dim
                )));
            }
        }
        let Some(b) = s.point_bounds(self.field) else { return Ok(Some(Arc::new(MatchNoneQuery::new("")))) };
        if b.min.len() < self.lower.len() {
            return Ok(None);
        }
        Ok(match relate_box(&b.min, &b.max, &self.lower, &self.upper, self.bytes_per_dim) {
            Relation::CellOutsideQuery => Some(Arc::new(MatchNoneQuery::new(""))),
            Relation::CellCrossesQuery => None,
            Relation::CellInsideQuery => {
                let all = s.segments().iter().all(|seg| seg.reader.points(self.field).is_some_and(|p| p.doc_count() == seg.reader.max_doc()));
                if all {
                    Some(Arc::new(MatchAllQuery))
                } else if exists_answerable(s, self.field) {
                    Some(Arc::new(FieldExistsQuery { field: self.field }))
                } else {
                    None
                }
            }
        })
    }
    fn lucene_string(&self, schema: &Schema) -> String {
        let mut s = format!("{}:", schema.name(self.field));
        let b = self.bytes_per_dim;
        for d in 0..self.dims() {
            if d > 0 {
                s.push(',');
            }
            s.push('[');
            let show = |v: &[u8]| -> String {
                if self.kind == PointType::LatLon && v.len() == 4 {
                    let x = encode::i32_from(v);
                    double_to_string(if d == 0 { encode::decode_latitude(x) } else { encode::decode_longitude(x) })
                } else {
                    self.kind.display(v)
                }
            };
            s.push_str(&show(&self.lower[d * b..(d + 1) * b]));
            s.push_str(" TO ");
            s.push_str(&show(&self.upper[d * b..(d + 1) * b]));
            s.push(']');
        }
        s
    }
    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_debug(h, "point_range", self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Docs with a point equal to one of `points` (1-d, Lucene's `PointInSetQuery`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PointInSetQuery {
    pub field: u32,
    /// Sorted unique packed values.
    pub points: Arc<Vec<Vec<u8>>>,
    pub kind: PointType,
}

impl PointInSetQuery {
    pub fn new(field: u32, points: impl IntoIterator<Item = Vec<u8>>, kind: PointType) -> PointInSetQuery {
        let mut v: Vec<Vec<u8>> = points.into_iter().collect();
        v.sort();
        v.dedup();
        PointInSetQuery { field, points: Arc::new(v), kind }
    }

    fn docs(&self, seg: &SegmentContext) -> Option<DocIdSet> {
        let p = seg.reader.points(self.field)?;
        if self.points.is_empty() || self.points[0].len() != p.spec().packed_len() {
            return None;
        }
        struct V<'a> {
            pts: &'a [Vec<u8>],
            b: DocIdSetBuilder,
        }
        impl PointVisitor for V<'_> {
            fn compare(&mut self, min: &[u8], max: &[u8]) -> Relation {
                let i = self.pts.partition_point(|x| x.as_slice() < min);
                if i < self.pts.len() && self.pts[i].as_slice() <= max {
                    if min == max { Relation::CellInsideQuery } else { Relation::CellCrossesQuery }
                } else {
                    Relation::CellOutsideQuery
                }
            }
            fn visit(&mut self, doc: u32) {
                self.b.add(doc);
            }
            fn visit_value(&mut self, doc: u32, packed: &[u8]) {
                if self.pts.binary_search_by(|x| x.as_slice().cmp(packed)).is_ok() {
                    self.b.add(doc);
                }
            }
            fn visit_doc_range(&mut self, s: u32, e: u32) {
                self.b.add_range(s, e);
            }
        }
        let mut v = V { pts: &self.points, b: DocIdSetBuilder::new(seg.max_doc()) };
        p.intersect(&mut v);
        Some(v.b.build())
    }
}

struct PointSetWeight {
    q: PointInSetQuery,
    score: f32,
    desc: String,
}

impl Weight for PointSetWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        Ok(self.q.docs(seg).map(|s| Box::new(SetScorer::new(Arc::new(s), self.score)) as Box<dyn Scorer>))
    }
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        explain_const(self, seg, doc, &self.desc, self.score)
    }
}

impl Query for PointInSetQuery {
    fn weight(&self, s: &Searcher, _mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        Ok(Box::new(PointSetWeight { q: self.clone(), score: boost, desc: self.lucene_string(s.schema()) }))
    }
    fn lucene_string(&self, schema: &Schema) -> String {
        let vals: Vec<String> = self.points.iter().map(|p| self.kind.display(p)).collect();
        format!("{}:{{{}}}", schema.name(self.field), vals.join(" "))
    }
    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_tag(h, "point_set");
        h.write_u32(self.field);
        crate::search::hash_debug(h, "kind", &self.kind);
        h.write_usize(self.points.len());
        for p in self.points.iter() {
            crate::search::hash_bytes(h, p);
        }
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Bounds of a doc values range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DvRange {
    /// Inclusive long bounds (numeric and sorted-numeric fields; doubles as sortable longs).
    Long { lower: i64, upper: i64 },
    /// Term bounds (sorted and sorted-set fields); `None` = open.
    Bytes { lower: Option<Vec<u8>>, upper: Option<Vec<u8>>, include_lower: bool, include_upper: bool },
}

/// A range check on doc values (Lucene's `SortedNumericDocValuesField.newSlowRangeQuery` and
/// `SortedSetDocValuesField.newSlowRangeQuery`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocValuesRangeQuery {
    pub field: u32,
    pub range: DvRange,
}

/// Iterates a doc values field's docs, confirming each against a predicate.
pub(crate) struct DvScorer {
    docs: search_codec::DocSetCursor,
    check: Box<dyn FnMut(u32) -> bool + Send>,
    score: f32,
    cost: u64,
    doc: u32,
    match_cost: f32,
}

impl DvScorer {
    pub(crate) fn new(docs: &search_codec::DocSet, check: Box<dyn FnMut(u32) -> bool + Send>, score: f32, match_cost: f32) -> DvScorer {
        DvScorer { cost: docs.len() as u64, docs: docs.cursor(), check, score, doc: u32::MAX, match_cost }
    }
}

impl Scorable for DvScorer {
    fn score(&mut self) -> f32 {
        self.score
    }
}

impl Scorer for DvScorer {
    fn doc(&self) -> u32 {
        self.doc
    }
    fn next(&mut self) -> u32 {
        loop {
            let d = self.approx_next();
            if d == NO_MORE_DOCS || self.matches() {
                return d;
            }
        }
    }
    fn advance(&mut self, t: u32) -> u32 {
        let d = self.approx_advance(t);
        if d == NO_MORE_DOCS || self.matches() {
            return d;
        }
        self.next()
    }
    fn cost(&self) -> u64 {
        self.cost
    }
    fn has_two_phase(&self) -> bool {
        true
    }
    fn approx_next(&mut self) -> u32 {
        self.doc = self.docs.next();
        self.doc
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        self.doc = self.docs.advance(t);
        self.doc
    }
    fn matches(&mut self) -> bool {
        (self.check)(self.doc)
    }
    fn match_cost(&self) -> f32 {
        self.match_cost
    }
    fn max_score(&mut self, _upto: u32) -> f32 {
        self.score
    }
}

impl DocValuesRangeQuery {
    fn scorer(&self, seg: &SegmentContext, score: f32) -> Option<Box<dyn Scorer>> {
        let dv = seg.reader.doc_values(self.field)?.clone();
        match (&self.range, dv) {
            (DvRange::Long { lower, upper }, _) if lower > upper => None,
            (DvRange::Long { lower, upper }, DocValuesReader::Numeric(v)) => {
                let (lo, hi) = (*lower, *upper);
                let docs = v.docs().clone();
                Some(Box::new(DvScorer::new(&docs, Box::new(move |d| v.get(d).is_some_and(|x| x >= lo && x <= hi)), score, 2.0)))
            }
            (DvRange::Long { lower, upper }, DocValuesReader::SortedNumeric(v)) => {
                let (lo, hi) = (*lower, *upper);
                let docs = v.docs().clone();
                Some(Box::new(DvScorer::new(
                    &docs,
                    Box::new(move |d| v.values(d).any(|x| x >= lo && x <= hi)),
                    score,
                    2.0,
                )))
            }
            (DvRange::Bytes { lower, upper, include_lower, include_upper }, DocValuesReader::Sorted(v)) => {
                let (lo, hi) = ord_range(v.dict(), lower, upper, *include_lower, *include_upper)?;
                let docs = v.docs().clone();
                Some(Box::new(DvScorer::new(&docs, Box::new(move |d| v.ord(d).is_some_and(|o| o >= lo && o <= hi)), score, 2.0)))
            }
            (DvRange::Bytes { lower, upper, include_lower, include_upper }, DocValuesReader::SortedSet(v)) => {
                let (lo, hi) = ord_range(v.dict(), lower, upper, *include_lower, *include_upper)?;
                let docs = v.docs().clone();
                Some(Box::new(DvScorer::new(
                    &docs,
                    Box::new(move |d| v.ords(d).any(|o| o >= lo && o <= hi)),
                    score,
                    2.0,
                )))
            }
            _ => None,
        }
    }
}

/// The inclusive ordinal range of a term range, `None` when empty.
fn ord_range(
    dict: &search_codec::TermDict,
    lower: &Option<Vec<u8>>,
    upper: &Option<Vec<u8>>,
    incl_lo: bool,
    incl_hi: bool,
) -> Option<(u64, u64)> {
    let n = dict.len();
    if n == 0 {
        return None;
    }
    let lo = match lower {
        None => 0,
        Some(l) => match dict.seek(l) {
            Ok(o) => {
                if incl_lo {
                    o
                } else {
                    o + 1
                }
            }
            Err(ins) => ins,
        },
    };
    let hi = match upper {
        None => n - 1,
        Some(u) => match dict.seek(u) {
            Ok(o) => {
                if incl_hi {
                    o
                } else {
                    o.checked_sub(1)?
                }
            }
            Err(ins) => ins.checked_sub(1)?,
        },
    };
    (lo <= hi && lo < n).then_some((lo, hi))
}

struct DvRangeWeight {
    q: DocValuesRangeQuery,
    score: f32,
    desc: String,
}

impl Weight for DvRangeWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        Ok(self.q.scorer(seg, self.score))
    }
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        explain_const(self, seg, doc, &self.desc, self.score)
    }
}

impl Query for DocValuesRangeQuery {
    fn weight(&self, s: &Searcher, _mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        Ok(Box::new(DvRangeWeight { q: self.clone(), score: boost, desc: self.lucene_string(s.schema()) }))
    }
    /// Lucene 10.5's `SortedNumericDocValuesRangeQuery` / `SortedSetDocValuesRangeQuery`
    /// rewrites: open bounds are `FieldExistsQuery`; numeric ranges outside the field's global
    /// bounds match nothing, and ranges covering them match every doc when every doc has a
    /// value.
    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        use crate::query::basic::{FieldExistsQuery, MatchAllQuery, MatchNoneQuery};
        match &self.range {
            DvRange::Long { lower: i64::MIN, upper: i64::MAX } => Ok(Some(Arc::new(FieldExistsQuery { field: self.field }))),
            DvRange::Long { lower, upper } if lower > upper => Ok(Some(Arc::new(MatchNoneQuery::new("")))),
            DvRange::Long { lower, upper } => Ok(match numeric_stats(s, self.field) {
                Some((min, max, _)) if *lower > max || *upper < min => Some(Arc::new(MatchNoneQuery::new(""))),
                Some((min, max, n)) if *lower <= min && *upper >= max && n == s.max_doc() as u64 => Some(Arc::new(MatchAllQuery)),
                _ => None,
            }),
            DvRange::Bytes { lower: None, upper: None, .. } => Ok(Some(Arc::new(FieldExistsQuery { field: self.field }))),
            DvRange::Bytes { .. } => Ok(None),
        }
    }
    fn lucene_string(&self, schema: &Schema) -> String {
        let f = schema.name(self.field);
        match &self.range {
            DvRange::Long { lower, upper } => format!("{f}:[{lower} TO {upper}]"),
            DvRange::Bytes { lower, upper, include_lower, include_upper } => {
                let b = |t: &Option<Vec<u8>>| t.as_ref().map_or("*".to_string(), |t| term_text(t));
                format!(
                    "{f}:{}{} TO {}{}",
                    if *include_lower { '[' } else { '{' },
                    b(lower),
                    b(upper),
                    if *include_upper { ']' } else { '}' }
                )
            }
        }
    }
    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_debug(h, "dv_range", self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Docs whose doc values contain one of the terms (keyword fields with doc values only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocValuesTermsQuery {
    pub field: u32,
    /// Sorted unique terms.
    pub terms: Arc<Vec<Vec<u8>>>,
}

impl DocValuesTermsQuery {
    pub fn new(field: u32, terms: impl IntoIterator<Item = Vec<u8>>) -> DocValuesTermsQuery {
        let mut v: Vec<Vec<u8>> = terms.into_iter().collect();
        v.sort();
        v.dedup();
        DocValuesTermsQuery { field, terms: Arc::new(v) }
    }
}

struct DvTermsWeight {
    q: DocValuesTermsQuery,
    score: f32,
    desc: String,
}

impl Weight for DvTermsWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let Some(dv) = seg.reader.doc_values(self.q.field) else { return Ok(None) };
        let ords = |dict: &search_codec::TermDict| -> Vec<u64> {
            let mut v: Vec<u64> = self.q.terms.iter().filter_map(|t| dict.seek(t).ok()).collect();
            v.sort_unstable();
            v
        };
        let s: Option<Box<dyn Scorer>> = match dv.clone() {
            DocValuesReader::Sorted(v) => {
                let o = ords(v.dict());
                if o.is_empty() {
                    None
                } else {
                    let docs = v.docs().clone();
                    Some(Box::new(DvScorer::new(&docs, Box::new(move |d| v.ord(d).is_some_and(|x| o.binary_search(&x).is_ok())), self.score, 3.0)))
                }
            }
            DocValuesReader::SortedSet(v) => {
                let o = ords(v.dict());
                if o.is_empty() {
                    None
                } else {
                    let docs = v.docs().clone();
                    Some(Box::new(DvScorer::new(&docs, Box::new(move |d| v.ords(d).any(|x| o.binary_search(&x).is_ok())), self.score, 3.0)))
                }
            }
            _ => None,
        };
        Ok(s)
    }
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        explain_const(self, seg, doc, &self.desc, self.score)
    }
}

impl Query for DocValuesTermsQuery {
    fn weight(&self, s: &Searcher, _mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        Ok(Box::new(DvTermsWeight { q: self.clone(), score: boost, desc: self.lucene_string(s.schema()) }))
    }
    fn lucene_string(&self, schema: &Schema) -> String {
        let parts: Vec<String> = self.terms.iter().map(|t| term_text(t)).collect();
        format!("{}:({})", schema.name(self.field), parts.join(" "))
    }
    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_tag(h, "dv_terms");
        h.write_u32(self.field);
        h.write_usize(self.terms.len());
        for t in self.terms.iter() {
            crate::search::hash_bytes(h, t);
        }
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Points when the range leads iteration, doc values when a much cheaper clause leads
/// (Lucene's `IndexOrDocValuesQuery`). Explanations come from the doc-values side, as in Lucene.
#[derive(Clone, Debug)]
pub struct IndexOrDocValuesQuery {
    pub index: QueryRef,
    pub dv: QueryRef,
}

struct IodvWeight {
    index: Box<dyn Weight>,
    dv: Box<dyn Weight>,
}

struct IodvSupplier {
    index: Box<dyn ScorerSupplier>,
    dv: Option<Box<dyn ScorerSupplier>>,
}

impl ScorerSupplier for IodvSupplier {
    fn cost(&self) -> u64 {
        self.index.cost()
    }
    fn get(self: Box<Self>, lead: u64) -> Result<Box<dyn Scorer>> {
        let threshold = self.index.cost() >> 3;
        match self.dv {
            Some(dv) if threshold > lead => dv.get(lead),
            _ => self.index.get(lead),
        }
    }
}

impl Weight for IodvWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        self.index.scorer(seg)
    }
    fn scorer_supplier(&self, seg: &SegmentContext) -> Result<Option<Box<dyn ScorerSupplier>>> {
        let Some(index) = self.index.scorer_supplier(seg)? else { return Ok(None) };
        let dv = self.dv.scorer_supplier(seg)?;
        Ok(Some(Box::new(IodvSupplier { index, dv })))
    }
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        self.dv.explain(seg, doc)
    }
    fn count(&self, seg: &SegmentContext) -> Result<Option<u32>> {
        self.index.count(seg)
    }
}

impl Query for IndexOrDocValuesQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let index = s.rewrite(&self.index)?.weight(s, mode, boost)?;
        let dv = s.rewrite(&self.dv)?.weight(s, mode, boost)?;
        Ok(Box::new(IodvWeight { index, dv }))
    }
    /// Lucene 10.5: either side rewriting to match-all (then match-none) decides; otherwise
    /// both sides rewrite.
    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        use crate::query::basic::{MatchAllQuery, MatchNoneQuery};
        let i = s.rewrite(&self.index)?;
        let d = s.rewrite(&self.dv)?;
        if i.as_any().is::<MatchAllQuery>() || d.as_any().is::<MatchAllQuery>() {
            return Ok(Some(Arc::new(MatchAllQuery)));
        }
        if i.as_any().is::<MatchNoneQuery>() || d.as_any().is::<MatchNoneQuery>() {
            return Ok(Some(Arc::new(MatchNoneQuery::new(""))));
        }
        if !Arc::ptr_eq(&i, &self.index) || !Arc::ptr_eq(&d, &self.dv) {
            return Ok(Some(Arc::new(IndexOrDocValuesQuery { index: i, dv: d })));
        }
        Ok(None)
    }
    fn lucene_string(&self, schema: &Schema) -> String {
        format!(
            "IndexOrDocValuesQuery(indexQuery={}, dvQuery={})",
            self.index.lucene_string(schema),
            self.dv.lucene_string(schema)
        )
    }
    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        // Both sides match the same docs; the index side identifies them.
        crate::search::hash_tag(h, "index_or_dv");
        self.index.cache_identity(h)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Lucene's `IndexSortSortedNumericDocValuesRangeQuery` (what Elasticsearch builds for long,
/// integer and date ranges on the field an index is sorted by): in a segment sorted by the
/// field (primary key, numeric, at most one value per doc) the matching docs are one contiguous
/// doc id range, found by two binary searches over the doc values — no BKD walk, an exact count
/// for free. Other segments run `fallback` (the usual `IndexOrDocValuesQuery`). Bounds are in
/// the doc values' integer space.
#[derive(Clone, Debug)]
pub struct IndexSortRangeQuery {
    pub field: u32,
    pub lower: i64,
    pub upper: i64,
    pub fallback: QueryRef,
}

impl IndexSortRangeQuery {
    /// The docs `[first, last)` of a segment sorted by the field, or `None` when the segment
    /// isn't (or the field is multi-valued there).
    fn doc_range(&self, seg: &SegmentContext) -> Option<(u32, u32)> {
        let sort = seg.reader.info().sort.as_ref()?;
        let primary = sort.fields.first()?;
        if primary.field != self.field || primary.kind != search_codec::SortKind::Long {
            return None;
        }
        // Nested children carry no value but sit next to their parents, in the middle of the
        // order: only segments of root docs alone qualify.
        let roots = match seg.reader.doc_values(crate::schema::PRIMARY_TERM_FIELD) {
            Some(DocValuesReader::Numeric(v)) => v.docs().len() as u64,
            Some(DocValuesReader::SortedNumeric(v)) => v.docs().len() as u64,
            _ => 0,
        };
        if roots != seg.max_doc() as u64 {
            return None;
        }
        let value: Box<dyn Fn(u32) -> Option<i64>> = match seg.reader.doc_values(self.field)? {
            DocValuesReader::Numeric(v) => {
                let v = v.clone();
                Box::new(move |d| v.get(d))
            }
            DocValuesReader::SortedNumeric(v) if v.is_single_valued() => {
                let v = v.clone();
                Box::new(move |d| v.values(d).next())
            }
            _ => return None,
        };
        if self.lower > self.upper {
            return Some((0, 0));
        }
        // Docs without a value sit at one end (`missing`), so in index order every doc is
        // "before", "inside" or "after" the range, in that order.
        let missing_first = primary.missing == search_codec::SortMissing::First;
        let (lo, hi, reverse) = (self.lower, self.upper, primary.reverse);
        let before = |d: u32| match value(d) {
            None => missing_first,
            Some(v) => if reverse { v > hi } else { v < lo },
        };
        let after = |d: u32| match value(d) {
            None => !missing_first,
            Some(v) => if reverse { v < lo } else { v > hi },
        };
        let max = seg.max_doc();
        let partition = |pred: &dyn Fn(u32) -> bool, mut a: u32| {
            let mut b = max;
            while a < b {
                let mid = a + (b - a) / 2;
                if pred(mid) { a = mid + 1 } else { b = mid }
            }
            a
        };
        let first = partition(&before, 0);
        let last = partition(&|d| !after(d), first);
        Some((first, last))
    }
}

struct IndexSortRangeWeight {
    q: IndexSortRangeQuery,
    fallback: Box<dyn Weight>,
    score: f32,
    desc: String,
}

/// Every doc of `[doc, end)`.
struct DocRangeScorer {
    doc: u32,
    start: u32,
    end: u32,
    score: f32,
}

impl Scorable for DocRangeScorer {
    fn score(&mut self) -> f32 {
        self.score
    }
}

impl Scorer for DocRangeScorer {
    fn doc(&self) -> u32 {
        self.doc
    }
    fn next(&mut self) -> u32 {
        let t = if self.doc == u32::MAX { self.start } else { self.doc + 1 };
        self.advance(t)
    }
    fn advance(&mut self, t: u32) -> u32 {
        let t = t.max(self.start);
        self.doc = if t < self.end { t } else { NO_MORE_DOCS };
        self.doc
    }
    fn cost(&self) -> u64 {
        (self.end - self.start) as u64
    }
    fn max_score(&mut self, _upto: u32) -> f32 {
        self.score
    }
}

impl Weight for IndexSortRangeWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        match self.q.doc_range(seg) {
            Some((a, b)) if a < b => Ok(Some(Box::new(DocRangeScorer { doc: u32::MAX, start: a, end: b, score: self.score }))),
            Some(_) => Ok(None),
            None => self.fallback.scorer(seg),
        }
    }
    fn scorer_supplier(&self, seg: &SegmentContext) -> Result<Option<Box<dyn ScorerSupplier>>> {
        match self.q.doc_range(seg) {
            Some((a, b)) if a < b => {
                Ok(Some(Box::new(crate::search::ReadyScorer(Box::new(DocRangeScorer { doc: u32::MAX, start: a, end: b, score: self.score })))))
            }
            Some(_) => Ok(None),
            None => self.fallback.scorer_supplier(seg),
        }
    }
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        explain_const(self, seg, doc, &self.desc, self.score)
    }
    fn count(&self, seg: &SegmentContext) -> Result<Option<u32>> {
        match self.q.doc_range(seg) {
            Some((a, b)) if !seg.has_deletions() => Ok(Some(b.saturating_sub(a))),
            Some(_) => Ok(None),
            None => self.fallback.count(seg),
        }
    }
}

impl Query for IndexSortRangeQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        Ok(Box::new(IndexSortRangeWeight {
            q: self.clone(),
            fallback: s.weight_of(&self.fallback, mode, boost)?,
            score: boost,
            desc: self.lucene_string(s.schema()),
        }))
    }
    /// Lucene: open bounds are an exists query; a fallback rewriting to match-all decides.
    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        use crate::query::basic::{FieldExistsQuery, MatchAllQuery};
        if self.lower == i64::MIN && self.upper == i64::MAX {
            return Ok(Some(Arc::new(FieldExistsQuery { field: self.field })));
        }
        let f = s.rewrite(&self.fallback)?;
        if f.as_any().is::<MatchAllQuery>() {
            return Ok(Some(Arc::new(MatchAllQuery)));
        }
        if Arc::ptr_eq(&f, &self.fallback) {
            return Ok(None);
        }
        Ok(Some(Arc::new(IndexSortRangeQuery { fallback: f, ..self.clone() })))
    }
    fn lucene_string(&self, schema: &Schema) -> String {
        format!("{}:[{} TO {}]", schema.name(self.field), self.lower, self.upper)
    }
    fn visit(&self, v: &mut dyn crate::search::QueryVisitor) {
        self.fallback.visit(v);
    }
    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_tag(h, "index_sort_range");
        h.write_u32(self.field);
        h.write_i64(self.lower);
        h.write_i64(self.upper);
        true
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
