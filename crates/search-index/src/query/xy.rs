//! Cartesian geometry: Elasticsearch's `point` and `shape` fields (Lucene's `XYPointField`,
//! `XYDocValuesField` and `XYShape`).
//!
//! Coordinates are floats. `XYEncodingUtils` stores them as sortable float bits
//! (`NumericUtils.floatToSortableInt`), so indexed values round-trip exactly and query
//! geometries are never quantized: a `point` field is a 2-dimension BKD field of 4-byte values
//! (`x`, then `y`) plus sorted-numeric doc values (`x` in the high 32 bits); a `shape` field
//! stores triangles exactly like `geo_shape` ([`shape`](crate::query::shape)) with
//! cartesian-encoded vertices.
//!
//! Queries are Lucene's: [`XyPointQuery`] (`XYPointInGeometryQuery`, intersects only),
//! [`XyPointDvQuery`] (`XYDocValuesPointInGeometryQuery`, two-phase over doc values) and
//! [`XyShapeQuery`](crate::query::shape::XyShapeQuery) (`XYShapeQuery`, every relation). Query
//! geometries become Lucene's `Component2D`s ([`Component`]), cartesian variants: points and
//! rectangles as given, circles with squared planar distances inside a float bounding box.
//!
//! Lucene supports only `intersects` for cartesian points; Elasticsearch builds the other
//! relations from it (`XYQueriesUtils.toXYPointQuery`), which [`point_shape_query`] reproduces:
//! `disjoint` = docs with a value minus the intersecting ones, `within` = intersecting minus
//! docs with a value outside (the inverse geometry), `contains` = every query point present.

use std::any::Any;
use std::sync::Arc;

use search_codec::points::encode::{i32_bytes, i32_from};
use search_codec::{PointVisitor, Relation};

use crate::error::{Error, Result};
use crate::query::basic::{ConstantScoreQuery, FieldExistsQuery, MatchNoneQuery, SetScorer, explain_const};
use crate::query::boolean::BooleanQuery;
use crate::query::geo::ShapeRelation;
use crate::query::range::IndexOrDocValuesQuery;
use crate::query::shape::ShapeDvQuery;
use crate::schema::Schema;
use crate::search::{Explanation, Query, QueryRef, ScoreMode, Scorer, Searcher, SegmentContext, Weight};
use crate::util::bitset::{DocIdSet, DocIdSetBuilder};
use crate::util::geo2d::Component;
use crate::util::java::float_to_string as fs;
use crate::util::tessellator::{GeoPolygon, check_xy, decode_xy, encode_xy};

// ------------------------------------------------------------------ encoding

/// `XYPointField`'s packed value (`x`, `y` as sortable float bits); Lucene's error for
/// non-finite values.
pub fn point_bytes(x: f32, y: f32) -> Result<[u8; 8]> {
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&i32_bytes(encode_xy(check_xy(x)?)));
    out[4..].copy_from_slice(&i32_bytes(encode_xy(check_xy(y)?)));
    Ok(out)
}

/// Decodes a packed `XYPointField` value.
pub fn decode_point_bytes(packed: &[u8]) -> (f32, f32) {
    (decode_xy(i32_from(&packed[0..4])) as f32, decode_xy(i32_from(&packed[4..8])) as f32)
}

/// `XYDocValuesField`'s sorted-numeric value (`x` in the high 32 bits).
pub fn dv_value(x: f32, y: f32) -> Result<i64> {
    let (ex, ey) = (encode_xy(check_xy(x)?), encode_xy(check_xy(y)?));
    Ok(((ex as i64) << 32) | (ey as u32 as i64))
}

/// Decodes an `XYDocValuesField` value to `(x, y)`.
pub fn decode_dv(v: i64) -> (f32, f32) {
    (decode_xy((v >> 32) as i32) as f32, decode_xy(v as i32) as f32)
}

// ------------------------------------------------------------------ geometries

/// A cartesian query geometry (Lucene's `XYGeometry` family). Build polygons with
/// [`GeoPolygon::xy`].
#[derive(Clone, Debug, PartialEq)]
pub enum XyGeometry {
    Point { x: f32, y: f32 },
    Line { xs: Vec<f32>, ys: Vec<f32> },
    Polygon(GeoPolygon),
    Rectangle { min_x: f32, max_x: f32, min_y: f32, max_y: f32 },
    Circle { x: f32, y: f32, radius: f32 },
}

impl XyGeometry {
    /// The checks of the Lucene constructors, with their messages.
    pub fn validate(&self) -> Result<()> {
        match self {
            XyGeometry::Point { x, y } => {
                check_xy(*x)?;
                check_xy(*y)?;
            }
            XyGeometry::Line { xs, ys } => {
                if xs.len() != ys.len() {
                    return Err(Error::illegal("x and y must be equal length"));
                }
                if xs.len() < 2 {
                    return Err(Error::illegal("at least 2 line points required"));
                }
                for (x, y) in xs.iter().zip(ys) {
                    check_xy(*x)?;
                    check_xy(*y)?;
                }
            }
            XyGeometry::Polygon(p) => {
                if !p.is_xy() {
                    return Err(Error::illegal("cartesian queries take cartesian polygons (GeoPolygon::xy)"));
                }
            }
            XyGeometry::Rectangle { min_x, max_x, min_y, max_y } => {
                if min_x > max_x {
                    return Err(Error::illegal(format!("minX must be lower than maxX, got {} > {}", fs(*min_x), fs(*max_x))));
                }
                if min_y > max_y {
                    return Err(Error::illegal(format!("minY must be lower than maxY, got {} > {}", fs(*min_y), fs(*max_y))));
                }
                for v in [min_x, max_x, min_y, max_y] {
                    check_xy(*v)?;
                }
            }
            XyGeometry::Circle { x, y, radius } => {
                // NaN fails the finiteness check below, as in Java.
                if *radius <= 0.0 {
                    return Err(Error::illegal(format!("radius must be bigger than 0, got {}", fs(*radius))));
                }
                if !radius.is_finite() {
                    return Err(Error::illegal(format!("radius must be finite, got {}", fs(*radius))));
                }
                check_xy(*x)?;
                check_xy(*y)?;
            }
        }
        Ok(())
    }

    /// Lucene's `Component2D` of the geometry (`toComponent2D`).
    pub fn component(&self) -> Component {
        match self {
            XyGeometry::Point { x, y } => Component::xy_point(*x, *y),
            XyGeometry::Line { xs, ys } => Component::xy_line(xs, ys),
            XyGeometry::Polygon(p) => Component::polygon(p),
            XyGeometry::Rectangle { min_x, max_x, min_y, max_y } => Component::xy_rectangle(*min_x, *max_x, *min_y, *max_y),
            XyGeometry::Circle { x, y, radius } => Component::xy_circle(*x, *y, *radius),
        }
    }

    /// Lucene's `toString` (query descriptions).
    pub fn lucene_string(&self) -> String {
        match self {
            XyGeometry::Point { x, y } => format!("XYPoint({},{})", fs(*x), fs(*y)),
            XyGeometry::Line { xs, ys } => {
                let pts: Vec<String> = xs.iter().zip(ys).map(|(x, y)| format!("[{}, {}]", fs(*x), fs(*y))).collect();
                format!("XYLine({})", pts.concat())
            }
            XyGeometry::Polygon(p) => p.lucene_string(),
            XyGeometry::Rectangle { min_x, max_x, min_y, max_y } => {
                format!("XYRectangle(x={} TO {} y={} TO {})", fs(*min_x), fs(*max_x), fs(*min_y), fs(*max_y))
            }
            XyGeometry::Circle { x, y, radius } => format!("XYCircle([{},{}] radius = {})", fs(*x), fs(*y), fs(*radius)),
        }
    }
}

/// `XYGeometry.create`: validated geometries as one component (a tree when several).
pub(crate) fn component_of(geometries: &[XyGeometry]) -> Result<Component> {
    if geometries.is_empty() {
        return Err(Error::illegal("geometries must not be empty"));
    }
    for g in geometries {
        g.validate()?;
    }
    Ok(Component::tree(geometries.iter().map(XyGeometry::component).collect()))
}

/// Java's `hashCode` of the Lucene geometry (Elasticsearch prints the inverse geometry with
/// `Object.toString`, which shows it).
fn java_hash(g: &XyGeometry) -> i32 {
    fn fh(v: f32) -> i32 {
        (if v.is_nan() { 0x7fc0_0000 } else { v.to_bits() }) as i32
    }
    fn arr(v: impl Iterator<Item = f32>) -> i32 {
        v.fold(1i32, |h, x| h.wrapping_mul(31).wrapping_add(fh(x)))
    }
    fn poly(p: &GeoPolygon) -> i32 {
        let holes = p.holes.iter().fold(1i32, |h, x| h.wrapping_mul(31).wrapping_add(poly(x)));
        let mut h = 31i32.wrapping_add(holes);
        h = h.wrapping_mul(31).wrapping_add(arr(p.lons.iter().map(|&v| v as f32)));
        h.wrapping_mul(31).wrapping_add(arr(p.lats.iter().map(|&v| v as f32)))
    }
    match g {
        XyGeometry::Point { x, y } => fh(*x).wrapping_mul(31).wrapping_add(fh(*y)),
        XyGeometry::Line { xs, ys } => arr(xs.iter().copied()).wrapping_mul(31).wrapping_add(arr(ys.iter().copied())),
        XyGeometry::Polygon(p) => poly(p),
        XyGeometry::Rectangle { min_x, max_x, min_y, max_y } => {
            // `(int) (temp ^ (temp >>> 32))` of the sign-extended bits.
            let f = |v: f32| {
                let t = fh(v) as i64;
                (t ^ ((t as u64) >> 32) as i64) as i32
            };
            [*min_y, *max_x, *max_y].into_iter().fold(f(*min_x), |h, v| h.wrapping_mul(31).wrapping_add(f(v)))
        }
        XyGeometry::Circle { x, y, radius } => {
            fh(*x).wrapping_mul(31).wrapping_add(fh(*y)).wrapping_mul(31).wrapping_add(fh(*radius))
        }
    }
}

fn geometries_string(geometries: &[XyGeometry], inverse: bool) -> String {
    if inverse {
        let h = geometries.iter().fold(1i32, |h, g| h.wrapping_mul(31).wrapping_add(java_hash(g)));
        return format!("[org.elasticsearch.lucene.spatial.XYQueriesUtils$InverseXYGeometry@{:x}]", h as u32);
    }
    let gs: Vec<String> = geometries.iter().map(XyGeometry::lucene_string).collect();
    format!("[{}]", gs.join(", "))
}

// ------------------------------------------------------------------ point queries

/// Docs with a `point` value in any of the geometries (Lucene's `XYPointInGeometryQuery`;
/// `inverse`: a value outside all of them — Elasticsearch's `InverseXYGeometry`).
#[derive(Clone, Debug, PartialEq)]
pub struct XyPointQuery {
    pub field: u32,
    pub geometries: Vec<XyGeometry>,
    pub inverse: bool,
}

impl XyPointQuery {
    /// `XYPointField.newGeometryQuery`.
    pub fn new(field: u32, geometries: Vec<XyGeometry>) -> Result<XyPointQuery> {
        component_of(&geometries)?;
        Ok(XyPointQuery { field, geometries, inverse: false })
    }

    /// Elasticsearch's `XYPointField.newGeometryQuery(field, new InverseXYGeometry(..))`.
    pub fn inverse(field: u32, geometries: Vec<XyGeometry>) -> Result<XyPointQuery> {
        component_of(&geometries)?;
        Ok(XyPointQuery { field, geometries, inverse: true })
    }

    fn component(&self) -> Result<Component> {
        let c = component_of(&self.geometries)?;
        Ok(if self.inverse { Component::inverse(c) } else { c })
    }
}

/// Decodes a 2-d cell and relates it to the component.
struct XyVisitor<'a> {
    c: &'a Component,
    b: DocIdSetBuilder,
}

impl XyVisitor<'_> {
    #[inline]
    fn matches(&self, packed: &[u8]) -> bool {
        self.c.contains(decode_xy(i32_from(&packed[0..4])), decode_xy(i32_from(&packed[4..8])))
    }
}

impl PointVisitor for XyVisitor<'_> {
    fn compare(&mut self, min: &[u8], max: &[u8]) -> Relation {
        let (min_x, min_y) = (decode_xy(i32_from(&min[0..4])), decode_xy(i32_from(&min[4..8])));
        let (max_x, max_y) = (decode_xy(i32_from(&max[0..4])), decode_xy(i32_from(&max[4..8])));
        self.c.relate(min_x, max_x, min_y, max_y)
    }
    fn visit(&mut self, doc: u32) {
        self.b.add(doc);
    }
    fn visit_docs(&mut self, docs: &[u32]) {
        for &d in docs {
            self.b.add(d);
        }
    }
    fn visit_doc_range(&mut self, s: u32, e: u32) {
        self.b.add_range(s, e);
    }
    fn visit_value(&mut self, doc: u32, packed: &[u8]) {
        if self.matches(packed) {
            self.b.add(doc);
        }
    }
    fn visit_value_docs(&mut self, docs: &[u32], packed: &[u8]) {
        if self.matches(packed) {
            for &d in docs {
                self.b.add(d);
            }
        }
    }
    fn grow(&mut self, n: usize) {
        self.b.grow(n);
    }
}

struct XyPointWeight {
    field: u32,
    component: Arc<Component>,
    score: f32,
    desc: String,
}

impl XyPointWeight {
    fn docs(&self, seg: &SegmentContext) -> Option<DocIdSet> {
        let p = seg.reader.points(self.field)?;
        if p.spec().dims != 2 || p.spec().bytes_per_dim != 4 {
            return None;
        }
        let max_doc = seg.max_doc();
        let root = {
            let (min, max) = (p.min_packed_value(), p.max_packed_value());
            let mut v = XyVisitor { c: &self.component, b: DocIdSetBuilder::new(0) };
            v.compare(min, max)
        };
        match root {
            Relation::CellOutsideQuery => return None,
            Relation::CellInsideQuery if p.doc_count() == max_doc => return Some(DocIdSet::All(max_doc)),
            _ => {}
        }
        let mut v = XyVisitor { c: &self.component, b: DocIdSetBuilder::new(max_doc) };
        p.intersect(&mut v);
        Some(v.b.build())
    }
}

impl Weight for XyPointWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        Ok(self.docs(seg).filter(|s| !matches!(s, DocIdSet::Empty)).map(|s| Box::new(SetScorer::new(Arc::new(s), self.score)) as Box<dyn Scorer>))
    }
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        explain_const(self, seg, doc, &self.desc, self.score)
    }
}

impl Query for XyPointQuery {
    fn weight(&self, s: &Searcher, _mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        Ok(Box::new(XyPointWeight { field: self.field, component: Arc::new(self.component()?), score: boost, desc: self.lucene_string(s.schema()) }))
    }
    fn lucene_string(&self, schema: &Schema) -> String {
        format!("XYPointInGeometryQuery: field={}:{}", schema.name(self.field), geometries_string(&self.geometries, self.inverse))
    }
    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_debug(h, "xy_point", self)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Docs with an `XYDocValuesField` value in any of the geometries (Lucene's
/// `XYDocValuesPointInGeometryQuery`; `inverse` as in [`XyPointQuery`]). Two-phase over the
/// field's doc values.
#[derive(Clone, Debug, PartialEq)]
pub struct XyPointDvQuery {
    pub field: u32,
    pub geometries: Vec<XyGeometry>,
    pub inverse: bool,
}

impl XyPointDvQuery {
    /// `XYDocValuesField.newSlowGeometryQuery`.
    pub fn new(field: u32, geometries: Vec<XyGeometry>) -> Result<XyPointDvQuery> {
        component_of(&geometries)?;
        Ok(XyPointDvQuery { field, geometries, inverse: false })
    }

    /// The doc-values form of [`XyPointQuery::inverse`].
    pub fn inverse(field: u32, geometries: Vec<XyGeometry>) -> Result<XyPointDvQuery> {
        component_of(&geometries)?;
        Ok(XyPointDvQuery { field, geometries, inverse: true })
    }
}

struct XyDvWeight {
    field: u32,
    component: Arc<Component>,
    score: f32,
    desc: String,
}

impl Weight for XyDvWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let Some(v) = seg.reader.sorted_numeric(self.field).cloned() else { return Ok(None) };
        let docs = v.docs().clone();
        let c = self.component.clone();
        let check: Box<dyn FnMut(u32) -> bool + Send> = Box::new(move |d| {
            v.values(d).any(|x| {
                let (px, py) = (decode_xy((x >> 32) as i32), decode_xy(x as i32));
                c.contains(px, py)
            })
        });
        Ok(Some(Box::new(crate::query::range::DvScorer::new(&docs, check, self.score, 1000.0))))
    }
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        explain_const(self, seg, doc, &self.desc, self.score)
    }
}

impl Query for XyPointDvQuery {
    fn weight(&self, s: &Searcher, _mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let c = component_of(&self.geometries)?;
        let c = if self.inverse { Component::inverse(c) } else { c };
        Ok(Box::new(XyDvWeight { field: self.field, component: Arc::new(c), score: boost, desc: self.lucene_string(s.schema()) }))
    }
    fn lucene_string(&self, schema: &Schema) -> String {
        format!("{}:geometries({})", schema.name(self.field), geometries_string(&self.geometries, self.inverse))
    }
    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_debug(h, "xy_point_dv", self)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Elasticsearch's intersects query on a `point` field (`XYQueriesUtils.buildIntersectsQuery`):
/// the BKD query, wrapped with its doc-values twin when the field has both, or the doc-values
/// query alone for `index: false`.
fn intersects(field: u32, indexed: bool, doc_values: bool, geometries: Vec<XyGeometry>, inverse: bool) -> Result<QueryRef> {
    let dv = || -> Result<QueryRef> {
        Ok(Arc::new(if inverse { XyPointDvQuery::inverse(field, geometries.clone())? } else { XyPointDvQuery::new(field, geometries.clone())? }))
    };
    if !indexed {
        return dv();
    }
    let index: QueryRef = Arc::new(if inverse { XyPointQuery::inverse(field, geometries.clone())? } else { XyPointQuery::new(field, geometries.clone())? });
    if doc_values { Ok(Arc::new(IndexOrDocValuesQuery { index, dv: dv()? })) } else { Ok(index) }
}

/// Elasticsearch's `XYQueriesUtils.toXYPointQuery`: a `shape` query on a `point` field with any
/// relation, built from Lucene's intersects-only point queries. At least one of `indexed` /
/// `doc_values` must hold.
pub fn point_shape_query(field: u32, relation: ShapeRelation, geometries: Vec<XyGeometry>, indexed: bool, doc_values: bool) -> Result<QueryRef> {
    if !indexed && !doc_values {
        return Err(Error::illegal("a point query needs an indexed field or doc values"));
    }
    match relation {
        ShapeRelation::Intersects => intersects(field, indexed, doc_values, geometries, false),
        ShapeRelation::Disjoint => {
            let all: QueryRef = if doc_values {
                Arc::new(FieldExistsQuery { field })
            } else {
                let everything = XyGeometry::Rectangle { min_x: -f32::MAX, max_x: f32::MAX, min_y: -f32::MAX, max_y: f32::MAX };
                intersects(field, indexed, doc_values, vec![everything], false)?
            };
            Ok(Arc::new(BooleanQuery::new().filter(all).must_not(intersects(field, indexed, doc_values, geometries, false)?)))
        }
        ShapeRelation::Contains => {
            if !geometries.iter().all(|g| matches!(g, XyGeometry::Point { .. })) {
                return Ok(Arc::new(MatchNoneQuery::new("")));
            }
            if geometries.len() == 1 {
                return intersects(field, indexed, doc_values, geometries, false);
            }
            let mut b = BooleanQuery::new();
            for g in geometries {
                b = b.filter(intersects(field, indexed, doc_values, vec![g], false)?);
            }
            Ok(Arc::new(b))
        }
        ShapeRelation::Within => Ok(Arc::new(
            BooleanQuery::new()
                .filter(intersects(field, indexed, doc_values, geometries.clone(), false)?)
                .must_not(intersects(field, indexed, doc_values, geometries, true)?),
        )),
    }
}

/// Elasticsearch's `XYQueriesUtils.toXYShapeQuery`: `XYShape.newGeometryQuery` on indexed
/// `shape` fields, wrapped with its doc-values twin ([`ShapeDvQuery`]) when the field has
/// both, or the doc-values query alone for `index: false`. No geometries match nothing.
pub fn shape_query(field: u32, relation: ShapeRelation, geometries: Vec<XyGeometry>, indexed: bool, doc_values: bool) -> Result<QueryRef> {
    if !indexed && !doc_values {
        return Err(Error::illegal("a shape query needs an indexed field or doc values"));
    }
    if geometries.is_empty() {
        return Ok(Arc::new(MatchNoneQuery::new("")));
    }
    let dv = || -> Result<QueryRef> { Ok(Arc::new(ShapeDvQuery::xy(field, relation, geometries.clone())?)) };
    if !indexed {
        return dv();
    }
    let index = index_shape_query(field, relation, geometries.clone())?;
    if doc_values { Ok(Arc::new(IndexOrDocValuesQuery { index, dv: dv()? })) } else { Ok(index) }
}

/// `XYShape.newGeometryQuery`: `contains` over several geometries is a conjunction.
fn index_shape_query(field: u32, relation: ShapeRelation, geometries: Vec<XyGeometry>) -> Result<QueryRef> {
    if relation == ShapeRelation::Contains && geometries.len() > 1 {
        let mut b = BooleanQuery::new();
        for g in geometries {
            b = b.must(index_shape_query(field, relation, vec![g])?);
        }
        return Ok(Arc::new(ConstantScoreQuery::new(Arc::new(b))));
    }
    Ok(Arc::new(crate::query::shape::XyShapeQuery::new(field, geometries, relation)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_round_trips() {
        for (x, y) in [(0.0f32, -0.0f32), (1.5, -2.25), (f32::MAX, -f32::MAX), (1e-30, 3.4e38), (f32::MIN_POSITIVE, -1e-45)] {
            assert_eq!(decode_point_bytes(&point_bytes(x, y).unwrap()), (x, y));
            assert_eq!(decode_dv(dv_value(x, y).unwrap()), (x, y));
        }
        // Sortable: byte order equals numeric order.
        let a = point_bytes(-1.0, 0.0).unwrap();
        let b = point_bytes(2.0, 0.0).unwrap();
        assert!(a[..4] < b[..4]);
        let e = point_bytes(f32::NAN, 0.0).unwrap_err().to_string();
        assert!(e.contains("invalid value NaN; must be between -3.4028234663852886E38 and 3.4028234663852886E38"), "{e}");
    }

    #[test]
    fn geometry_validation_and_strings() {
        let r = XyGeometry::Rectangle { min_x: 2.0, max_x: 1.0, min_y: 0.0, max_y: 1.0 };
        assert_eq!(r.validate().unwrap_err().to_string(), "minX must be lower than maxX, got 2.0 > 1.0");
        let c = XyGeometry::Circle { x: 0.0, y: 0.0, radius: 0.0 };
        assert_eq!(c.validate().unwrap_err().to_string(), "radius must be bigger than 0, got 0.0");
        let c = XyGeometry::Circle { x: 0.0, y: 0.0, radius: f32::INFINITY };
        assert_eq!(c.validate().unwrap_err().to_string(), "radius must be finite, got Infinity");
        let l = XyGeometry::Line { xs: vec![1.0], ys: vec![1.0] };
        assert_eq!(l.validate().unwrap_err().to_string(), "at least 2 line points required");
        assert_eq!(XyGeometry::Point { x: 1.0, y: 2.5 }.lucene_string(), "XYPoint(1.0,2.5)");
        assert_eq!(XyGeometry::Rectangle { min_x: -1.0, max_x: 1.0, min_y: 0.0, max_y: 1e8 }.lucene_string(), "XYRectangle(x=-1.0 TO 1.0 y=0.0 TO 1.0E8)");
        assert_eq!(XyGeometry::Line { xs: vec![0.0, 1.0], ys: vec![2.0, 3.0] }.lucene_string(), "XYLine([0.0, 2.0][1.0, 3.0])");
        assert_eq!(XyGeometry::Circle { x: 1.0, y: 2.0, radius: 3.0 }.lucene_string(), "XYCircle([1.0,2.0] radius = 3.0)");
        let p = GeoPolygon::xy(&[0.0, 1.0, 1.0, 0.0], &[0.0, 0.0, 1.0, 0.0], vec![]).unwrap();
        assert_eq!(XyGeometry::Polygon(p).lucene_string(), "XYPolygon[0.0, 0.0] [1.0, 0.0] [1.0, 1.0] [0.0, 0.0] ");
        let e = GeoPolygon::xy(&[0.0, 1.0, 1.0, 0.5], &[0.0, 0.0, 1.0, 0.0], vec![]).unwrap_err().to_string();
        assert_eq!(e, "first and last points of the polygon must be the same (it must close itself): x[0]=0.0 x[3]=0.5");
    }

    #[test]
    fn inverse_geometry_prints_its_java_hash() {
        // Captured from Elasticsearch 9.5.5 profiles.
        let r = vec![XyGeometry::Rectangle { min_x: 0.0, max_x: 10.0, min_y: 0.0, max_y: 10.0 }];
        assert_eq!(geometries_string(&r, true), "[org.elasticsearch.lucene.spatial.XYQueriesUtils$InverseXYGeometry@2400001f]");
        let pts = vec![XyGeometry::Point { x: 1.5, y: 2.5 }, XyGeometry::Point { x: 3.0, y: 4.0 }];
        assert_eq!(geometries_string(&pts, true), "[org.elasticsearch.lucene.spatial.XYQueriesUtils$InverseXYGeometry@1be003c1]");
    }

    #[test]
    fn cartesian_circle() {
        let c = Component::xy_circle(10.0, 10.0, 5.0);
        assert!(c.contains(13.0, 14.0), "on the boundary");
        assert!(!c.contains(13.0, 14.001));
        assert_eq!(c.relate(9.0, 11.0, 9.0, 11.0), Relation::CellInsideQuery);
        assert_eq!(c.relate(20.0, 30.0, 20.0, 30.0), Relation::CellOutsideQuery);
        assert_eq!(c.relate(14.0, 20.0, 14.0, 20.0), Relation::CellOutsideQuery, "corner beyond the radius");
        assert_eq!(c.relate(12.0, 20.0, 12.0, 20.0), Relation::CellCrossesQuery);
        assert!(c.intersects_line(0.0, 10.0, 20.0, 10.0));
        assert!(!c.intersects_line(0.0, 20.0, 20.0, 20.0));
        let inv = Component::inverse(c);
        assert!(!inv.contains(10.0, 10.0) && inv.contains(30.0, 30.0));
        assert_eq!(inv.relate(9.0, 11.0, 9.0, 11.0), Relation::CellOutsideQuery);
        assert_eq!(inv.relate(20.0, 30.0, 20.0, 30.0), Relation::CellInsideQuery);
    }
}
