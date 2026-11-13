//! Geo-point queries: distance, bounding box, polygon and shape relations.
//!
//! Points are indexed as 2-d BKD points (encoded latitude, longitude; Lucene `LatLonPoint`)
//! and geo doc values. Bounding boxes are point ranges (split at the dateline). Distance
//! queries prune BKD cells by the circle's bounding box and accept a point when Lucene's
//! haversine sort key of the center and the decoded point is within the radius' key — the
//! exact boundary rule of `LatLonPointDistanceQuery`. Polygons test decoded points by ray
//! casting (boundary inclusive, holes excluded).

use std::any::Any;
use std::sync::Arc;

use search_codec::points::encode;
use search_codec::{PointVisitor, Relation};

use crate::error::{Error, Result};
use crate::query::basic::{SetScorer, explain_const};
use crate::query::range::{PointRangeQuery, PointType};
use crate::schema::Schema;
use crate::search::{Explanation, Query, QueryRef, ScoreMode, Scorer, Searcher, SegmentContext, Weight};
use crate::util::bitset::{DocIdSet, DocIdSetBuilder, FixedBitSet};
use crate::util::geo2d::Component;
use crate::util::tessellator::GeoPolygon;
use crate::util::geo::{haversin_meters_key, haversin_sort_key};
use crate::util::java::double_to_string;

pub(crate) const EARTH_MEAN_RADIUS_METERS: f64 = 6_371_008.771_4;

/// Lucene `GeoUtils.distanceQuerySortKey`: the smallest sort key whose distance is at least
/// `radius`.
pub fn distance_query_sort_key(radius: f64) -> f64 {
    let max = haversin_meters_key(f64::MAX);
    if radius >= max {
        return max;
    }
    let (mut lo, mut hi) = (0i64, f64::MAX.to_bits() as i64);
    while lo <= hi {
        let mid = ((lo as u64 + hi as u64) >> 1) as i64;
        let key = f64::from_bits(mid as u64);
        let r = haversin_meters_key(key);
        if r == radius {
            return key;
        } else if r > radius {
            hi = mid - 1;
        } else {
            lo = mid + 1;
        }
    }
    f64::from_bits(lo as u64)
}

/// Lucene `Rectangle.fromPointDistance` (degrees): `(min_lat, max_lat, min_lon, max_lon)`.
pub fn bbox_of_circle(lat: f64, lon: f64, radius: f64) -> (f64, f64, f64, f64) {
    use crate::util::geo::{sloppy_asin, sloppy_cos};
    let rad_lat = lat.to_radians();
    let rad_lon = lon.to_radians();
    let rd = (radius + 7e-2) / EARTH_MEAN_RADIUS_METERS;
    let (mut min_lat, mut max_lat) = (rad_lat - rd, rad_lat + rd);
    let (min_lon, max_lon);
    let (min_lat_r, max_lat_r) = ((-90f64).to_radians(), 90f64.to_radians());
    let (min_lon_r, max_lon_r) = ((-180f64).to_radians(), 180f64.to_radians());
    if min_lat > min_lat_r && max_lat < max_lat_r {
        let sin = sloppy_cos(rd - std::f64::consts::FRAC_PI_2);
        let delta = sloppy_asin(sin / sloppy_cos(rad_lat));
        let mut a = rad_lon - delta;
        if a < min_lon_r {
            a += 2.0 * std::f64::consts::PI;
        }
        let mut b = rad_lon + delta;
        if b > max_lon_r {
            b -= 2.0 * std::f64::consts::PI;
        }
        min_lon = a;
        max_lon = b;
    } else {
        min_lat = min_lat.max(min_lat_r);
        max_lat = max_lat.min(max_lat_r);
        min_lon = min_lon_r;
        max_lon = max_lon_r;
    }
    (min_lat.to_degrees(), max_lat.to_degrees(), min_lon.to_degrees(), max_lon.to_degrees())
}

pub(crate) fn check_lat(lat: f64) -> Result<()> {
    if !(-90.0..=90.0).contains(&lat) || lat.is_nan() {
        return Err(Error::illegal(format!("invalid latitude {}; must be between -90.0 and 90.0", double_to_string(lat))));
    }
    Ok(())
}

pub(crate) fn check_lon(lon: f64) -> Result<()> {
    if !(-180.0..=180.0).contains(&lon) || lon.is_nan() {
        return Err(Error::illegal(format!("invalid longitude {}; must be between -180.0 and 180.0", double_to_string(lon))));
    }
    Ok(())
}

/// A `LatLonPoint` bounding box query (split at the dateline when `min_lon > max_lon`).
pub fn bounding_box(field: u32, min_lat: f64, max_lat: f64, mut min_lon: f64, max_lon: f64) -> Result<QueryRef> {
    use crate::query::basic::{ConstantScoreQuery, MatchNoneQuery};
    use crate::query::boolean::BooleanQuery;
    for v in [min_lat, max_lat] {
        check_lat(v)?;
    }
    for v in [min_lon, max_lon] {
        check_lon(v)?;
    }
    if min_lat == 90.0 {
        return Ok(Arc::new(MatchNoneQuery::new("LatLonPoint.newBoxQuery with minLatitude=90.0")));
    }
    if min_lon == 180.0 {
        if max_lon == 180.0 {
            return Ok(Arc::new(MatchNoneQuery::new("LatLonPoint.newBoxQuery with minLongitude=maxLongitude=180.0")));
        } else if max_lon < min_lon {
            min_lon = -180.0;
        }
    }
    let enc = |lat: i32, lon: i32| {
        let mut b = encode::i32_bytes(lat).to_vec();
        b.extend_from_slice(&encode::i32_bytes(lon));
        b
    };
    let lower = enc(encode::encode_latitude_ceil(min_lat), encode::encode_longitude_ceil(min_lon));
    let upper = enc(encode::encode_latitude(max_lat), encode::encode_longitude(max_lon));
    let pr = |lo: Vec<u8>, hi: Vec<u8>| -> QueryRef {
        Arc::new(PointRangeQuery { field, lower: lo, upper: hi, bytes_per_dim: 4, kind: PointType::LatLon })
    };
    if max_lon < min_lon {
        let mut left_open = lower.clone();
        left_open[4..].copy_from_slice(&encode::i32_bytes(i32::MIN));
        let mut right_open = upper.clone();
        right_open[4..].copy_from_slice(&encode::i32_bytes(i32::MAX));
        let b = BooleanQuery::new().should(pr(left_open, upper)).should(pr(lower, right_open));
        return Ok(Arc::new(ConstantScoreQuery::new(Arc::new(b))));
    }
    Ok(pr(lower, upper))
}

/// Docs with a point within `radius` meters of a center (`LatLonPoint.newDistanceQuery`).
#[derive(Clone, Debug, PartialEq)]
pub struct GeoDistanceQuery {
    pub field: u32,
    pub lat: f64,
    pub lon: f64,
    pub radius: f64,
}

impl GeoDistanceQuery {
    pub fn new(field: u32, lat: f64, lon: f64, radius: f64) -> Result<GeoDistanceQuery> {
        if !radius.is_finite() || radius < 0.0 {
            return Err(Error::illegal(format!("radiusMeters: '{}' is invalid", double_to_string(radius))));
        }
        check_lat(lat)?;
        check_lon(lon)?;
        Ok(GeoDistanceQuery { field, lat, lon, radius })
    }

    fn docs(&self, seg: &SegmentContext) -> Option<DocIdSet> {
        let p = seg.reader.points(self.field)?;
        if p.spec().packed_len() != 8 {
            return None;
        }
        let (a, b, c, d) = bbox_of_circle(self.lat, self.lon, self.radius);
        let min_lat = encode::encode_latitude(a);
        let max_lat = encode::encode_latitude(b);
        let (min_lon, max_lon, min_lon2) = if d < c {
            (i32::MIN, encode::encode_longitude(d), encode::encode_longitude(c))
        } else {
            (encode::encode_longitude(c), encode::encode_longitude(d), i32::MAX)
        };
        let mut v = DistVisitor {
            q: self,
            key: distance_query_sort_key(self.radius),
            min_lat,
            max_lat,
            min_lon,
            max_lon,
            min_lon2,
            b: DocIdSetBuilder::new(seg.max_doc()),
        };
        p.intersect(&mut v);
        Some(v.b.build())
    }
}

struct DistVisitor<'a> {
    q: &'a GeoDistanceQuery,
    key: f64,
    min_lat: i32,
    max_lat: i32,
    min_lon: i32,
    max_lon: i32,
    min_lon2: i32,
    b: DocIdSetBuilder,
}

impl DistVisitor<'_> {
    fn matches(&self, packed: &[u8]) -> bool {
        let lat = encode::i32_from(&packed[0..4]);
        if lat > self.max_lat || lat < self.min_lat {
            return false;
        }
        let lon = encode::i32_from(&packed[4..8]);
        if (lon > self.max_lon || lon < self.min_lon) && lon < self.min_lon2 {
            return false;
        }
        haversin_sort_key(self.q.lat, self.q.lon, encode::decode_latitude(lat), encode::decode_longitude(lon)) <= self.key
    }
}

impl PointVisitor for DistVisitor<'_> {
    fn compare(&mut self, min: &[u8], max: &[u8]) -> Relation {
        let (la, lb) = (encode::i32_from(&min[0..4]), encode::i32_from(&max[0..4]));
        if la > self.max_lat || lb < self.min_lat {
            return Relation::CellOutsideQuery;
        }
        let (oa, ob) = (encode::i32_from(&min[4..8]), encode::i32_from(&max[4..8]));
        if (oa > self.max_lon || ob < self.min_lon) && ob < self.min_lon2 {
            return Relation::CellOutsideQuery;
        }
        // Inside when every corner is within the circle and the cell spans < 90° of longitude.
        let (lat0, lat1) = (encode::decode_latitude(la), encode::decode_latitude(lb));
        let (lon0, lon1) = (encode::decode_longitude(oa), encode::decode_longitude(ob));
        let within90 = lon1 - lon0 < 90.0 && (self.q.lon - lon0).abs() < 90.0 && (self.q.lon - lon1).abs() < 90.0;
        if within90
            && [(lat0, lon0), (lat0, lon1), (lat1, lon0), (lat1, lon1)]
                .iter()
                .all(|&(a, b)| haversin_sort_key(self.q.lat, self.q.lon, a, b) <= self.key)
        {
            return Relation::CellInsideQuery;
        }
        Relation::CellCrossesQuery
    }
    fn visit(&mut self, doc: u32) {
        self.b.add(doc);
    }
    fn visit_value(&mut self, doc: u32, packed: &[u8]) {
        if self.matches(packed) {
            self.b.add(doc);
        }
    }
    fn visit_doc_range(&mut self, s: u32, e: u32) {
        self.b.add_range(s, e);
    }
    fn grow(&mut self, n: usize) {
        self.b.grow(n);
    }
}

struct GeoWeight<F: Fn(&SegmentContext) -> Option<DocIdSet> + Send + Sync> {
    docs: F,
    score: f32,
    desc: String,
}

impl<F: Fn(&SegmentContext) -> Option<DocIdSet> + Send + Sync> Weight for GeoWeight<F> {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        Ok((self.docs)(seg).map(|s| Box::new(SetScorer::new(Arc::new(s), self.score)) as Box<dyn Scorer>))
    }
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        explain_const(self, seg, doc, &self.desc, self.score)
    }
}

impl Query for GeoDistanceQuery {
    fn weight(&self, s: &Searcher, _mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let q = self.clone();
        Ok(Box::new(GeoWeight { docs: move |seg: &SegmentContext| q.docs(seg), score: boost, desc: self.lucene_string(s.schema()) }))
    }
    fn lucene_string(&self, schema: &Schema) -> String {
        format!(
            "{}:{},{} +/- {} meters",
            schema.name(self.field),
            double_to_string(self.lat),
            double_to_string(self.lon),
            double_to_string(self.radius)
        )
    }
    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_debug(h, "geo_distance", self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A polygon ring (closed or not) of `(lat, lon)` with holes.
#[derive(Clone, Debug, PartialEq)]
pub struct Polygon {
    pub outer: Vec<(f64, f64)>,
    pub holes: Vec<Vec<(f64, f64)>>,
}

/// Point-in-ring by ray casting; points on an edge count as inside.
fn in_ring(ring: &[(f64, f64)], lat: f64, lon: f64) -> bool {
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (yi, xi) = ring[i];
        let (yj, xj) = ring[j];
        // Boundary: collinear and within the segment's box.
        let cross = (xj - xi) * (lat - yi) - (yj - yi) * (lon - xi);
        if cross == 0.0 && lon >= xi.min(xj) && lon <= xi.max(xj) && lat >= yi.min(yj) && lat <= yi.max(yj) {
            return true;
        }
        if (yi > lat) != (yj > lat) && lon < (xj - xi) * (lat - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

impl Polygon {
    /// As Lucene's polygon (rings closed), with Lucene's `Polygon` validation errors.
    pub fn try_geo_polygon(&self) -> Result<GeoPolygon> {
        fn ring(r: &[(f64, f64)]) -> (Vec<f64>, Vec<f64>) {
            let mut lats: Vec<f64> = r.iter().map(|p| p.0).collect();
            let mut lons: Vec<f64> = r.iter().map(|p| p.1).collect();
            if !r.is_empty() && r.first() != r.last() {
                lats.push(r[0].0);
                lons.push(r[0].1);
            }
            (lats, lons)
        }
        let mut holes = Vec::new();
        for h in &self.holes {
            let (a, o) = ring(h);
            holes.push(GeoPolygon::new(a, o, vec![])?);
        }
        let (a, o) = ring(&self.outer);
        GeoPolygon::new(a, o, holes)
    }

    /// As Lucene's polygon (rings closed); `None` when a ring is degenerate or invalid.
    pub fn to_geo_polygon(&self) -> Option<GeoPolygon> {
        fn ring(r: &[(f64, f64)]) -> (Vec<f64>, Vec<f64>) {
            let mut lats: Vec<f64> = r.iter().map(|p| p.0).collect();
            let mut lons: Vec<f64> = r.iter().map(|p| p.1).collect();
            if r.first() != r.last() {
                lats.push(r[0].0);
                lons.push(r[0].1);
            }
            (lats, lons)
        }
        if self.outer.is_empty() {
            return None;
        }
        let mut holes = Vec::new();
        for h in &self.holes {
            if h.is_empty() {
                continue;
            }
            let (a, o) = ring(h);
            holes.push(GeoPolygon::new(a, o, vec![]).ok()?);
        }
        let (a, o) = ring(&self.outer);
        GeoPolygon::new(a, o, holes).ok()
    }

    pub fn contains(&self, lat: f64, lon: f64) -> bool {
        in_ring(&self.outer, lat, lon) && !self.holes.iter().any(|h| in_ring(h, lat, lon) && !on_ring(h, lat, lon))
    }

}

fn on_ring(ring: &[(f64, f64)], lat: f64, lon: f64) -> bool {
    let n = ring.len();
    let mut j = n.wrapping_sub(1);
    for i in 0..n {
        let (yi, xi) = ring[i];
        let (yj, xj) = ring[j];
        let cross = (xj - xi) * (lat - yi) - (yj - yi) * (lon - xi);
        if cross == 0.0 && lon >= xi.min(xj) && lon <= xi.max(xj) && lat >= yi.min(yj) && lat <= yi.max(yj) {
            return true;
        }
        j = i;
    }
    false
}

/// A shape for point relation queries.
#[derive(Clone, Debug, PartialEq)]
pub enum GeoShape {
    Point(f64, f64),
    /// `(min_lat, max_lat, min_lon, max_lon)`.
    Rectangle(f64, f64, f64, f64),
    Polygon(Polygon),
    Circle { lat: f64, lon: f64, radius: f64 },
    /// A line string of `(lat, lon)`: points exactly on it match.
    Line(Vec<(f64, f64)>),
}


/// How shapes relate to the doc's points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShapeRelation {
    Intersects,
    Within,
    Disjoint,
    Contains,
}

/// Geo points related to shapes (`geo_shape`/`geo_polygon` queries on `geo_point` fields).
#[derive(Clone, Debug, PartialEq)]
pub struct GeoPointShapeQuery {
    pub field: u32,
    pub shapes: Vec<GeoShape>,
    pub relation: ShapeRelation,
}

/// A shape as `LatLonPointQuery` sees it.
enum PointShape {
    C(Component),
    Empty,
}

impl PointShape {
    fn of(s: &GeoShape) -> PointShape {
        match s {
            GeoShape::Point(lat, lon) => PointShape::C(Component::point(*lat, *lon)),
            GeoShape::Rectangle(a, b, c, d) => PointShape::C(Component::rectangle(*a, *b, *c, *d)),
            GeoShape::Line(pts) if pts.len() >= 2 => {
                let lats: Vec<f64> = pts.iter().map(|p| p.0).collect();
                let lons: Vec<f64> = pts.iter().map(|p| p.1).collect();
                PointShape::C(Component::line(&lats, &lons))
            }
            GeoShape::Line(_) => PointShape::Empty,
            GeoShape::Polygon(p) => match p.to_geo_polygon() {
                Some(g) => PointShape::C(Component::polygon(&g)),
                None => PointShape::Empty,
            },
            GeoShape::Circle { lat, lon, radius } => PointShape::C(Component::circle(*lat, *lon, *radius)),
        }
    }

    fn contains(&self, lon: f64, lat: f64) -> bool {
        match self {
            PointShape::C(c) => c.contains(lon, lat),
            PointShape::Empty => false,
        }
    }

    fn relate(&self, min_lon: f64, max_lon: f64, min_lat: f64, max_lat: f64) -> Relation {
        match self {
            PointShape::C(c) => c.relate(min_lon, max_lon, min_lat, max_lat),
            PointShape::Empty => Relation::CellOutsideQuery,
        }
    }
}

/// Lucene's `LatLonPointQuery` over a segment's points for one relation and shape set.
fn point_relation_docs(seg: &SegmentContext, field: u32, shapes: &[PointShape], rel: ShapeRelation) -> Option<DocIdSet> {
    let p = seg.reader.points(field)?;
    if p.spec().packed_len() != 8 {
        return None;
    }
    let max_doc = seg.max_doc();
    let relate = |min: &[u8], max: &[u8]| -> Relation {
        let (min_lat, min_lon) = (encode::decode_latitude(encode::i32_from(&min[0..4])), encode::decode_longitude(encode::i32_from(&min[4..8])));
        let (max_lat, max_lon) = (encode::decode_latitude(encode::i32_from(&max[0..4])), encode::decode_longitude(encode::i32_from(&max[4..8])));
        let mut r = Relation::CellOutsideQuery;
        for s in shapes {
            match s.relate(min_lon, max_lon, min_lat, max_lat) {
                Relation::CellOutsideQuery => {}
                Relation::CellInsideQuery => return Relation::CellInsideQuery,
                Relation::CellCrossesQuery => r = Relation::CellCrossesQuery,
            }
        }
        r
    };
    let inner = |min: &[u8], max: &[u8]| -> Relation {
        let r = relate(min, max);
        if rel == ShapeRelation::Disjoint {
            match r {
                Relation::CellInsideQuery => Relation::CellOutsideQuery,
                Relation::CellOutsideQuery => Relation::CellInsideQuery,
                c => c,
            }
        } else {
            r
        }
    };
    let contains = |packed: &[u8]| -> bool {
        let (lat, lon) = (encode::decode_latitude(encode::i32_from(&packed[0..4])), encode::decode_longitude(encode::i32_from(&packed[4..8])));
        shapes.iter().any(|s| s.contains(lon, lat))
    };
    let root = inner(p.min_packed_value(), p.max_packed_value());
    if root == Relation::CellOutsideQuery {
        return None;
    }
    if p.doc_count() == max_doc && root == Relation::CellInsideQuery {
        return Some(DocIdSet::All(max_doc));
    }
    let mut result = FixedBitSet::new(max_doc);
    let mut excluded = FixedBitSet::new(max_doc);
    struct V<'a, I: Fn(&[u8], &[u8]) -> Relation, C: Fn(&[u8]) -> bool> {
        inner: &'a I,
        contains: &'a C,
        all: bool,
        disjoint: bool,
        result: &'a mut FixedBitSet,
        excluded: &'a mut FixedBitSet,
    }
    impl<I: Fn(&[u8], &[u8]) -> Relation, C: Fn(&[u8]) -> bool> PointVisitor for V<'_, I, C> {
        fn compare(&mut self, min: &[u8], max: &[u8]) -> Relation {
            (self.inner)(min, max)
        }
        fn visit(&mut self, doc: u32) {
            self.result.set(doc);
        }
        fn visit_value(&mut self, doc: u32, packed: &[u8]) {
            let pass = (self.contains)(packed) != self.disjoint;
            if !self.all {
                if pass {
                    self.result.set(doc);
                }
            } else if !self.excluded.get(doc) {
                if pass {
                    self.result.set(doc);
                } else {
                    self.excluded.set(doc);
                }
            }
        }
    }
    let all = matches!(rel, ShapeRelation::Within | ShapeRelation::Disjoint);
    let disjoint = rel == ShapeRelation::Disjoint;
    p.intersect(&mut V { inner: &inner, contains: &contains, all, disjoint, result: &mut result, excluded: &mut excluded });
    if all {
        result.and_not(&excluded);
        // Docs with points in cells failing as a whole.
        struct Inv<'a, I: Fn(&[u8], &[u8]) -> Relation> {
            inner: &'a I,
            result: &'a mut FixedBitSet,
        }
        impl<I: Fn(&[u8], &[u8]) -> Relation> PointVisitor for Inv<'_, I> {
            fn compare(&mut self, min: &[u8], max: &[u8]) -> Relation {
                match (self.inner)(min, max) {
                    Relation::CellInsideQuery => Relation::CellOutsideQuery,
                    Relation::CellOutsideQuery => Relation::CellInsideQuery,
                    c => c,
                }
            }
            fn visit(&mut self, doc: u32) {
                self.result.clear(doc);
            }
            fn visit_value(&mut self, _doc: u32, _packed: &[u8]) {}
        }
        p.intersect(&mut Inv { inner: &inner, result: &mut result });
    }
    let mut out = DocIdSetBuilder::new(max_doc);
    let mut d = result.next_set_bit(0);
    while d < max_doc {
        out.add(d);
        d = result.next_set_bit(d + 1);
    }
    Some(out.build())
}

impl GeoPointShapeQuery {
    /// Lucene's `LatLonPoint.newGeometryQuery`: `contains` holds only for points (each query
    /// point must be one of the doc's), the other relations run `LatLonPointQuery`.
    fn docs(&self, seg: &SegmentContext) -> Option<DocIdSet> {
        if self.relation == ShapeRelation::Contains {
            if self.shapes.is_empty() || !self.shapes.iter().all(|s| matches!(s, GeoShape::Point(..))) {
                return None;
            }
            let mut acc: Option<DocIdSet> = None;
            for s in &self.shapes {
                let set = point_relation_docs(seg, self.field, &[PointShape::of(s)], ShapeRelation::Intersects)?;
                acc = Some(match acc {
                    None => set,
                    Some(prev) => {
                        let mut b = DocIdSetBuilder::new(seg.max_doc());
                        let mut it = prev.iter();
                        let mut d = it.next();
                        while d != search_codec::NO_MORE_DOCS {
                            if set.contains(d) {
                                b.add(d);
                            }
                            d = it.next();
                        }
                        b.build()
                    }
                });
            }
            return acc;
        }
        let shapes: Vec<PointShape> = self.shapes.iter().map(PointShape::of).collect();
        point_relation_docs(seg, self.field, &shapes, self.relation)
    }
}

impl GeoShape {
    /// Lucene's geometry constructors' checks (`Point`, `Rectangle`, `Circle`, `Line`,
    /// `Polygon`) and their messages.
    pub fn validate(&self) -> Result<()> {
        match self {
            GeoShape::Point(lat, lon) => {
                check_lat(*lat)?;
                check_lon(*lon)
            }
            GeoShape::Rectangle(min_lat, max_lat, min_lon, max_lon) => {
                check_lat(*min_lat)?;
                check_lat(*max_lat)?;
                check_lon(*min_lon)?;
                check_lon(*max_lon)
            }
            GeoShape::Circle { lat, lon, radius } => {
                check_lat(*lat)?;
                check_lon(*lon)?;
                if !radius.is_finite() || *radius < 0.0 {
                    return Err(Error::illegal(format!("radiusMeters: '{}' is invalid", double_to_string(*radius))));
                }
                Ok(())
            }
            GeoShape::Line(pts) => {
                if pts.len() < 2 {
                    return Err(Error::illegal("at least 2 line points required"));
                }
                for (lat, lon) in pts {
                    check_lat(*lat)?;
                    check_lon(*lon)?;
                }
                Ok(())
            }
            GeoShape::Polygon(p) => p.try_geo_polygon().map(|_| ()),
        }
    }

    /// Lucene's geometry `toString` (`Circle`, `Polygon`, `Rectangle`, `Point`, `Line`).
    pub fn lucene_string(&self) -> String {
        let d = double_to_string;
        match self {
            GeoShape::Point(lat, lon) => format!("Point({},{})", d(*lon), d(*lat)),
            GeoShape::Rectangle(min_lat, max_lat, min_lon, max_lon) => format!(
                "Rectangle(lat={} TO {} lon={} TO {}{})",
                d(*min_lat),
                d(*max_lat),
                d(*min_lon),
                d(*max_lon),
                if max_lon < min_lon { " [crosses dateline!]" } else { "" }
            ),
            GeoShape::Circle { lat, lon, radius } => format!("Circle([{},{}] radius = {} meters)", d(*lat), d(*lon), d(*radius)),
            GeoShape::Line(pts) => {
                let mut b = String::from("Line(");
                for (lat, lon) in pts {
                    b.push_str(&format!("[{}, {}]", d(*lon), d(*lat)));
                }
                b.push(')');
                b
            }
            GeoShape::Polygon(p) => {
                fn ring(r: &[(f64, f64)]) -> String {
                    let mut b = String::from("Polygon");
                    let closed = r.first() == r.last();
                    for (lat, lon) in r.iter().chain((!closed).then(|| &r[0])) {
                        b.push_str(&format!("[{}, {}] ", double_to_string(*lat), double_to_string(*lon)));
                    }
                    b
                }
                if p.outer.is_empty() {
                    return "Polygon".into();
                }
                let mut b = ring(&p.outer);
                let holes: Vec<String> = p.holes.iter().filter(|h| !h.is_empty()).map(|h| ring(h)).collect();
                if !holes.is_empty() {
                    b.push_str(&format!(", holes=[{}]", holes.join(", ")));
                }
                b
            }
        }
    }
}

impl ShapeRelation {
    /// Lucene's `QueryRelation` name.
    pub fn lucene_name(self) -> &'static str {
        match self {
            ShapeRelation::Intersects => "INTERSECTS",
            ShapeRelation::Within => "WITHIN",
            ShapeRelation::Disjoint => "DISJOINT",
            ShapeRelation::Contains => "CONTAINS",
        }
    }
}

impl Query for GeoPointShapeQuery {
    fn weight(&self, s: &Searcher, _mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        for g in &self.shapes {
            g.validate()?;
        }
        let q = self.clone();
        Ok(Box::new(GeoWeight { docs: move |seg: &SegmentContext| q.docs(seg), score: boost, desc: self.lucene_string(s.schema()) }))
    }
    /// Lucene's `LatLonPointQuery.toString` (the relation is not printed).
    fn lucene_string(&self, schema: &Schema) -> String {
        let mut b = format!("LatLonPointQuery: field={}:[", schema.name(self.field));
        for g in &self.shapes {
            b.push_str(&g.lucene_string());
            b.push(',');
        }
        b.push(']');
        b
    }
    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_debug(h, "geo_shape", self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Lucene's doc-values geo queries over `LatLonDocValuesField`s (latitude in the high 32 bits
/// of a sorted-numeric value): the `dvQuery` side of Elasticsearch's geo `IndexOrDocValuesQuery`s,
/// and the only way to query `geo_point` fields with `index: false`. Two-phase: the field's docs,
/// confirmed by decoding their values.
#[derive(Clone, Debug, PartialEq)]
pub struct GeoPointDvQuery {
    pub field: u32,
    pub kind: GeoDvKind,
}

/// What a [`GeoPointDvQuery`] checks.
#[derive(Clone, Debug, PartialEq)]
pub enum GeoDvKind {
    /// `LatLonDocValuesQuery`: a relation to shapes (on decoded coordinates). Lucene's
    /// `contains` here holds when some value is one of the query points.
    Shapes { shapes: Vec<GeoShape>, relation: ShapeRelation },
    /// `LatLonDocValuesBoxQuery`: encoded bounds (`min` rounded up, as `encodeCeil`).
    Box { min_lat: i32, max_lat: i32, min_lon: i32, max_lon: i32, crosses_dateline: bool },
}

impl GeoPointDvQuery {
    /// `LatLonDocValuesField.newSlowBoxQuery`.
    pub fn bounding_box(field: u32, min_lat: f64, max_lat: f64, mut min_lon: f64, max_lon: f64) -> Result<QueryRef> {
        use crate::query::basic::MatchNoneQuery;
        for v in [min_lat, max_lat] {
            check_lat(v)?;
        }
        for v in [min_lon, max_lon] {
            check_lon(v)?;
        }
        if min_lat == 90.0 {
            return Ok(Arc::new(MatchNoneQuery::new("LatLonDocValuesField.newBoxQuery with minLatitude=90.0")));
        }
        if min_lon == 180.0 {
            if max_lon == 180.0 {
                return Ok(Arc::new(MatchNoneQuery::new("LatLonDocValuesField.newBoxQuery with minLongitude=maxLongitude=180.0")));
            } else if max_lon < min_lon {
                min_lon = -180.0;
            }
        }
        Ok(Arc::new(GeoPointDvQuery {
            field,
            kind: GeoDvKind::Box {
                crosses_dateline: min_lon > max_lon,
                min_lat: encode::encode_latitude_ceil(min_lat),
                max_lat: encode::encode_latitude(max_lat),
                min_lon: encode::encode_longitude_ceil(min_lon),
                max_lon: encode::encode_longitude(max_lon),
            },
        }))
    }

    /// `LatLonDocValuesField.newSlowGeometryQuery`: one rectangle intersecting is the box
    /// query; `contains` takes points only.
    pub fn geometry(field: u32, relation: ShapeRelation, shapes: Vec<GeoShape>) -> Result<QueryRef> {
        use crate::query::basic::MatchNoneQuery;
        if let (ShapeRelation::Intersects, [GeoShape::Rectangle(a, b, c, d)]) = (relation, shapes.as_slice()) {
            return Self::bounding_box(field, *a, *b, *c, *d);
        }
        if relation == ShapeRelation::Contains && shapes.iter().any(|g| !matches!(g, GeoShape::Point(..))) {
            return Ok(Arc::new(MatchNoneQuery::new("Contains LatLonDocValuesField.newSlowGeometryQuery with non-point geometries")));
        }
        if shapes.is_empty() {
            return Err(Error::illegal("geometries must not be empty"));
        }
        for g in &shapes {
            g.validate()?;
        }
        if relation == ShapeRelation::Within && shapes.iter().any(|g| matches!(g, GeoShape::Line(_))) {
            return Err(Error::illegal("LatLonDocValuesPointQuery does not support WITHIN queries with line geometries"));
        }
        Ok(Arc::new(GeoPointDvQuery { field, kind: GeoDvKind::Shapes { shapes, relation } }))
    }
}

impl Query for GeoPointDvQuery {
    fn weight(&self, s: &Searcher, _mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        Ok(Box::new(GeoDvWeight { q: self.clone(), score: boost, desc: self.lucene_string(s.schema()) }))
    }
    fn lucene_string(&self, schema: &Schema) -> String {
        let f = schema.name(self.field);
        match &self.kind {
            GeoDvKind::Shapes { shapes, relation } => {
                let g: Vec<String> = shapes.iter().map(GeoShape::lucene_string).collect();
                format!("{f}:{}:geometries([{}])", relation.lucene_name(), g.join(", "))
            }
            GeoDvKind::Box { min_lat, max_lat, min_lon, max_lon, .. } => format!(
                "{f}:box(minLat={}, maxLat={}, minLon={}, maxLon={})",
                double_to_string(encode::decode_latitude(*min_lat)),
                double_to_string(encode::decode_latitude(*max_lat)),
                double_to_string(encode::decode_longitude(*min_lon)),
                double_to_string(encode::decode_longitude(*max_lon))
            ),
        }
    }
    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_debug(h, "geo_dv", self)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct GeoDvWeight {
    q: GeoPointDvQuery,
    score: f32,
    desc: String,
}

impl Weight for GeoDvWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let Some(v) = seg.reader.sorted_numeric(self.q.field).cloned() else { return Ok(None) };
        let docs = v.docs().clone();
        let check: Box<dyn FnMut(u32) -> bool + Send> = match &self.q.kind {
            &GeoDvKind::Box { min_lat, max_lat, min_lon, max_lon, crosses_dateline } => Box::new(move |d| {
                v.values(d).any(|x| {
                    let (lat, lon) = ((x >> 32) as i32, x as i32);
                    lat >= min_lat
                        && lat <= max_lat
                        && if crosses_dateline { lon >= min_lon || lon <= max_lon } else { lon >= min_lon && lon <= max_lon }
                })
            }),
            GeoDvKind::Shapes { shapes, relation } => {
                let shapes: Vec<PointShape> = shapes.iter().map(PointShape::of).collect();
                let relation = *relation;
                Box::new(move |d| {
                    let mut hit = |x: i64| {
                        let (lat, lon) = crate::util::geo::decode_geo_dv(x);
                        shapes.iter().any(|s| s.contains(lon, lat))
                    };
                    match relation {
                        ShapeRelation::Intersects | ShapeRelation::Contains => v.values(d).any(&mut hit),
                        ShapeRelation::Within => v.values(d).all(&mut hit),
                        ShapeRelation::Disjoint => !v.values(d).any(&mut hit),
                    }
                })
            }
        };
        let cost = if matches!(self.q.kind, GeoDvKind::Box { .. }) { 5.0 } else { 1000.0 };
        Ok(Some(Box::new(crate::query::range::DvScorer::new(&docs, check, self.score, cost))))
    }
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        explain_const(self, seg, doc, &self.desc, self.score)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_key_round_trips() {
        for r in [0.0, 1.0, 1000.0, 123456.7, 2e7] {
            let k = distance_query_sort_key(r);
            assert!(haversin_meters_key(k) >= r);
        }
        let p = Polygon { outer: vec![(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 0.0)], holes: vec![] };
        assert!(p.contains(5.0, 5.0));
        assert!(p.contains(0.0, 5.0));
        assert!(!p.contains(11.0, 5.0));
        let (a, b, c, d) = bbox_of_circle(0.0, 0.0, 111_195.0);
        assert!((a + 1.0).abs() < 0.01 && (b - 1.0).abs() < 0.01 && (c + 1.0).abs() < 0.01 && (d - 1.0).abs() < 0.01);
    }
}
