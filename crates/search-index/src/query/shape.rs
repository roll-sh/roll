//! `geo_shape` fields: indexing shapes as triangles and querying them (Lucene's `LatLonShape`,
//! `ShapeField`, `SpatialQuery`, `LatLonShapeQuery` and `LatLonShapeBoundingBoxQuery`).
//!
//! A shape is stored as triangles in a 7-dimension BKD field (4 index dimensions: the
//! triangle's bounding box `minLat, minLon, maxLat, maxLon`; then the remaining vertex and a
//! code telling where the vertices sit on the box and which edges lie on the original shape):
//! points are degenerate triangles, line segments flat ones, polygons go through the
//! [tessellator](crate::util::tessellator). The field is declared with
//! [`SHAPE_POINTS`] and each triangle is an [`IndexableField::Point`](crate::doc::IndexableField).
//!
//! [`GeoShapeQuery`] relates docs' shapes to query geometries — `intersects`, `within`,
//! `disjoint`, `contains` — exactly as Lucene does: BKD cells are related to the query by
//! their triangles' bounding boxes, leaf triangles are decoded and tested with the
//! [`Component`] rules; `within`/`disjoint` need every
//! triangle of a doc to pass (a doc failing anywhere is excluded), `contains` needs a
//! candidate triangle and none proving the query is not within the doc's shape. A single
//! rectangle uses Lucene's encoded-rectangle path (comparisons in the encoded integer space,
//! split at the dateline).
//!
//! Cartesian `shape` fields use the same triangles with `XYEncodingUtils` coordinates
//! ([`xy_point_triangles`], [`xy_line_triangles`], [`polygon_triangles`] of a
//! [`GeoPolygon::xy`]) and [`XyShapeQuery`] (Lucene's `XYShapeQuery`: components only, no
//! encoded-rectangle path, no quantization).
//!
//! Converting Elasticsearch geometries (GeoJSON/WKT, orientation, dateline splitting, query
//! quantization) is the caller's.

use std::any::Any;
use std::sync::Arc;

use search_codec::points::encode::{
    decode_latitude, decode_longitude, encode_latitude, encode_latitude_ceil, encode_longitude, encode_longitude_ceil, i32_bytes, i32_from,
};
use search_codec::{PointVisitor, PointsSpec, Relation};

use crate::error::{Error, Result};
use crate::query::basic::{SetScorer, explain_const};
use crate::query::boolean::BooleanQuery;
use crate::query::geo::ShapeRelation;
use crate::schema::Schema;
use crate::search::{Explanation, Query, QueryRef, ScoreMode, Scorer, Searcher, SegmentContext, Weight};
use crate::util::bitset::{DocIdSet, DocIdSetBuilder, FixedBitSet};
use crate::util::geo2d::{Component, WithinRelation, line_crosses_line_with_boundary, point_in_triangle};
use crate::query::xy::XyGeometry;
use crate::util::tessellator::{GeoPolygon, decode_xy, encode_xy, orient, tessellate};

/// The points layout of a `geo_shape` field (Lucene `ShapeField.TYPE`: 7 dims, 4 indexed,
/// 4 bytes each).
pub const SHAPE_POINTS: PointsSpec = PointsSpec { dims: 7, index_dims: 4, bytes_per_dim: 4 };

/// Encoded (packed) triangle bytes.
pub type PackedTriangle = [u8; 28];

const MINY_MINX_MAXY_MAXX_Y_X: i32 = 0;
const MINY_MINX_Y_X_MAXY_MAXX: i32 = 1;
const MAXY_MINX_Y_X_MINY_MAXX: i32 = 2;
const MAXY_MINX_MINY_MAXX_Y_X: i32 = 3;
const Y_MINX_MINY_X_MAXY_MAXX: i32 = 4;
const Y_MINX_MINY_MAXX_MAXY_X: i32 = 5;
const MAXY_MINX_MINY_X_Y_MAXX: i32 = 6;
const MINY_MINX_Y_MAXX_MAXY_X: i32 = 7;

/// `GeoUtils.orient` on encoded ints (exact in double).
fn iorient(ax: i32, ay: i32, bx: i32, by: i32, cx: i32, cy: i32) -> i32 {
    orient(ax as f64, ay as f64, bx as f64, by as f64, cx as f64, cy as f64)
}

/// Lucene's `ShapeField.encodeTriangle` (vertices `(x = lon, y = lat)` encoded).
#[allow(clippy::too_many_arguments)]
pub fn encode_triangle(
    mut a_y: i32,
    mut a_x: i32,
    mut ab: bool,
    mut b_y: i32,
    mut b_x: i32,
    mut bc: bool,
    mut c_y: i32,
    mut c_x: i32,
    mut ca: bool,
) -> Result<PackedTriangle> {
    if b_x < a_x || c_x < a_x {
        let (tx, ty, tb) = (a_x, a_y, ab);
        if b_x < c_x {
            (a_x, a_y, ab) = (b_x, b_y, bc);
            (b_x, b_y, bc) = (c_x, c_y, ca);
            (c_x, c_y, ca) = (tx, ty, tb);
        } else {
            (a_x, a_y, ab) = (c_x, c_y, ca);
            (c_x, c_y, ca) = (b_x, b_y, bc);
            (b_x, b_y, bc) = (tx, ty, tb);
        }
    } else if a_x == b_x && a_x == c_x && (b_y < a_y || c_y < a_y) {
        let (tx, ty, tb) = (a_x, a_y, ab);
        if b_y < c_y {
            (a_x, a_y, ab) = (b_x, b_y, bc);
            (b_x, b_y, bc) = (c_x, c_y, ca);
            (c_x, c_y, ca) = (tx, ty, tb);
        } else {
            (a_x, a_y, ab) = (c_x, c_y, ca);
            (c_x, c_y, ca) = (b_x, b_y, bc);
            (b_x, b_y, bc) = (tx, ty, tb);
        }
    }
    if iorient(a_x, a_y, b_x, b_y, c_x, c_y) == -1 {
        let (tx, ty, tb) = (b_x, b_y, ab);
        ab = bc;
        b_x = c_x;
        b_y = c_y;
        c_x = tx;
        c_y = ty;
        ca = tb;
    }
    let min_x = a_x;
    let min_y = a_y.min(b_y.min(c_y));
    let max_x = a_x.max(b_x.max(c_x));
    let max_y = a_y.max(b_y.max(c_y));
    let (mut bits, x, y);
    if min_y == a_y {
        if max_y == b_y && max_x == b_x {
            y = c_y;
            x = c_x;
            bits = MINY_MINX_MAXY_MAXX_Y_X;
        } else if max_y == c_y && max_x == c_x {
            y = b_y;
            x = b_x;
            bits = MINY_MINX_Y_X_MAXY_MAXX;
        } else {
            y = b_y;
            x = c_x;
            bits = MINY_MINX_Y_MAXX_MAXY_X;
        }
    } else if max_y == a_y {
        if min_y == b_y && max_x == b_x {
            y = c_y;
            x = c_x;
            bits = MAXY_MINX_MINY_MAXX_Y_X;
        } else if min_y == c_y && max_x == c_x {
            y = b_y;
            x = b_x;
            bits = MAXY_MINX_Y_X_MINY_MAXX;
        } else {
            y = c_y;
            x = b_x;
            bits = MAXY_MINX_MINY_X_Y_MAXX;
        }
    } else if max_x == b_x && min_y == b_y {
        y = a_y;
        x = c_x;
        bits = Y_MINX_MINY_MAXX_MAXY_X;
    } else if max_x == c_x && max_y == c_y {
        y = a_y;
        x = b_x;
        bits = Y_MINX_MINY_X_MAXY_MAXX;
    } else {
        return Err(Error::illegal("Could not encode the provided triangle"));
    }
    bits |= if ab { 1 << 3 } else { 0 };
    bits |= if bc { 1 << 4 } else { 0 };
    bits |= if ca { 1 << 5 } else { 0 };
    let mut out = [0u8; 28];
    for (k, v) in [min_y, min_x, max_y, max_x, y, x, bits].into_iter().enumerate() {
        out[k * 4..k * 4 + 4].copy_from_slice(&i32_bytes(v));
    }
    Ok(out)
}

/// What a stored triangle is after decoding (Lucene's `DecodedTriangle.TYPE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriangleType {
    Point,
    Line,
    Triangle,
}

/// Lucene's `ShapeField.DecodedTriangle` (encoded coordinates).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodedTriangle {
    pub a_x: i32,
    pub a_y: i32,
    pub b_x: i32,
    pub b_y: i32,
    pub c_x: i32,
    pub c_y: i32,
    pub ab: bool,
    pub bc: bool,
    pub ca: bool,
    pub kind: TriangleType,
}

/// Lucene's `ShapeField.decodeTriangle` (+ `resolveTriangleType`); `None` for an unknown
/// code (corrupt data).
pub fn decode_triangle(t: &[u8]) -> Option<DecodedTriangle> {
    if t.len() < 28 {
        return None;
    }
    let g = |k: usize| i32_from(&t[k * 4..k * 4 + 4]);
    let bits = g(6);
    let (a_y, a_x, b_y, b_x, c_y, c_x) = match bits & 7 {
        MINY_MINX_MAXY_MAXX_Y_X => (g(0), g(1), g(2), g(3), g(4), g(5)),
        MINY_MINX_Y_X_MAXY_MAXX => (g(0), g(1), g(4), g(5), g(2), g(3)),
        MAXY_MINX_Y_X_MINY_MAXX => (g(2), g(1), g(4), g(5), g(0), g(3)),
        MAXY_MINX_MINY_MAXX_Y_X => (g(2), g(1), g(0), g(3), g(4), g(5)),
        Y_MINX_MINY_X_MAXY_MAXX => (g(4), g(1), g(0), g(5), g(2), g(3)),
        Y_MINX_MINY_MAXX_MAXY_X => (g(4), g(1), g(0), g(3), g(2), g(5)),
        MAXY_MINX_MINY_X_Y_MAXX => (g(2), g(1), g(0), g(5), g(4), g(3)),
        MINY_MINX_Y_MAXX_MAXY_X => (g(0), g(1), g(4), g(3), g(2), g(5)),
        _ => return None,
    };
    let mut d = DecodedTriangle {
        a_x,
        a_y,
        b_x,
        b_y,
        c_x,
        c_y,
        ab: bits & (1 << 3) != 0,
        bc: bits & (1 << 4) != 0,
        ca: bits & (1 << 5) != 0,
        kind: TriangleType::Triangle,
    };
    if d.a_x == d.b_x && d.a_y == d.b_y {
        if d.a_x == d.c_x && d.a_y == d.c_y {
            d.kind = TriangleType::Point;
        } else {
            d.ab = d.bc | d.ca;
            d.b_x = d.c_x;
            d.b_y = d.c_y;
            d.c_x = d.a_x;
            d.c_y = d.a_y;
            d.kind = TriangleType::Line;
        }
    } else if d.a_x == d.c_x && d.a_y == d.c_y {
        d.ab |= d.bc;
        d.kind = TriangleType::Line;
    } else if d.b_x == d.c_x && d.b_y == d.c_y {
        d.ab |= d.ca;
        d.c_x = d.a_x;
        d.c_y = d.a_y;
        d.kind = TriangleType::Line;
    }
    Some(d)
}

// ------------------------------------------------------------------ indexing

/// `LatLonShape.createIndexableFields(name, lat, lon)`.
pub fn point_triangles(lat: f64, lon: f64) -> Result<Vec<PackedTriangle>> {
    let (x, y) = (encode_longitude(lon), encode_latitude(lat));
    Ok(vec![encode_triangle(y, x, true, y, x, true, y, x, true)?])
}

/// `LatLonShape.createIndexableFields(name, line)`: one flat triangle per segment.
pub fn line_triangles(lats: &[f64], lons: &[f64]) -> Result<Vec<PackedTriangle>> {
    if lats.len() != lons.len() {
        return Err(Error::illegal("lats and lons must be equal length"));
    }
    if lats.len() < 2 {
        return Err(Error::illegal("at least 2 line points required"));
    }
    let mut out = Vec::with_capacity(lats.len() - 1);
    for j in 1..lats.len() {
        let i = j - 1;
        let (ax, ay) = (encode_longitude(lons[i]), encode_latitude(lats[i]));
        let (bx, by) = (encode_longitude(lons[j]), encode_latitude(lats[j]));
        out.push(encode_triangle(ay, ax, true, by, bx, true, ay, ax, true)?);
    }
    Ok(out)
}

/// `LatLonShape.createIndexableFields(name, polygon, checkSelfIntersections)`, or
/// `XYShape.createIndexableFields` for a cartesian polygon ([`GeoPolygon::xy`]).
pub fn polygon_triangles(p: &GeoPolygon, check_self_intersections: bool) -> Result<Vec<PackedTriangle>> {
    tessellate(p, check_self_intersections)?
        .iter()
        .map(|t| encode_triangle(t.y[0], t.x[0], t.from_polygon[0], t.y[1], t.x[1], t.from_polygon[1], t.y[2], t.x[2], t.from_polygon[2]))
        .collect()
}

/// `GeoUtils.quantizeLat` (Elasticsearch quantizes query geometries to the index grid).
pub fn quantize_lat(lat: f64) -> f64 {
    decode_latitude(encode_latitude(lat))
}

/// `GeoUtils.quantizeLon`.
pub fn quantize_lon(lon: f64) -> f64 {
    decode_longitude(encode_longitude(lon))
}

// ------------------------------------------------------------------ query

/// A query geometry (Lucene's `LatLonGeometry`).
#[derive(Clone, Debug, PartialEq)]
pub enum QueryGeometry {
    Point { lat: f64, lon: f64 },
    Line { lats: Vec<f64>, lons: Vec<f64> },
    Polygon(GeoPolygon),
    /// May cross the dateline (`min_lon > max_lon`).
    Rectangle { min_lat: f64, max_lat: f64, min_lon: f64, max_lon: f64 },
    /// Radius in meters.
    Circle { lat: f64, lon: f64, radius: f64 },
}

impl QueryGeometry {
    fn component(&self) -> Component {
        match self {
            QueryGeometry::Point { lat, lon } => Component::point(*lat, *lon),
            QueryGeometry::Line { lats, lons } => Component::line(lats, lons),
            QueryGeometry::Polygon(p) => Component::polygon(p),
            QueryGeometry::Rectangle { min_lat, max_lat, min_lon, max_lon } => Component::rectangle(*min_lat, *max_lat, *min_lon, *max_lon),
            QueryGeometry::Circle { lat, lon, radius } => Component::circle(*lat, *lon, *radius),
        }
    }

    /// Lucene's `toString` of the geometry (in query descriptions).
    pub fn describe(&self) -> String {
        use crate::util::java::double_to_string as d;
        match self {
            QueryGeometry::Point { lat, lon } => format!("Point({},{})", d(*lon), d(*lat)),
            QueryGeometry::Line { lats, lons } => {
                let pts: Vec<String> = lats.iter().zip(lons).map(|(a, o)| format!("[{}, {}]", d(*o), d(*a))).collect();
                format!("LINE([{}])", pts.join(", "))
            }
            QueryGeometry::Polygon(p) => {
                let pts: Vec<String> = p.lats.iter().zip(&p.lons).map(|(a, o)| format!("[{}, {}] ", d(*a), d(*o))).collect();
                pts.concat()
            }
            QueryGeometry::Rectangle { min_lat, max_lat, min_lon, max_lon } => {
                format!("Rectangle(lat={} TO {} lon={} TO {})", d(*min_lat), d(*max_lat), d(*min_lon), d(*max_lon))
            }
            QueryGeometry::Circle { lat, lon, radius } => format!("CIRCLE([{},{}] radius = {} meters)", d(*lat), d(*lon), d(*radius)),
        }
    }
}

/// Docs whose `geo_shape` relates to the geometries (Lucene `LatLonShape.newGeometryQuery`).
#[derive(Clone, Debug, PartialEq)]
pub struct GeoShapeQuery {
    pub field: u32,
    pub geometries: Vec<QueryGeometry>,
    pub relation: ShapeRelation,
}

impl GeoShapeQuery {
    pub fn new(field: u32, geometries: Vec<QueryGeometry>, relation: ShapeRelation) -> GeoShapeQuery {
        GeoShapeQuery { field, geometries, relation }
    }
}

/// How stored coordinates decode: `GeoEncodingUtils` (`geo_shape`) or `XYEncodingUtils`
/// (`shape`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Coords {
    Geo,
    Xy,
}

impl Coords {
    #[inline]
    fn x(self, v: i32) -> f64 {
        match self {
            Coords::Geo => decode_longitude(v),
            Coords::Xy => decode_xy(v),
        }
    }
    #[inline]
    fn y(self, v: i32) -> f64 {
        match self {
            Coords::Geo => decode_latitude(v),
            Coords::Xy => decode_xy(v),
        }
    }
}

/// How leaf triangles and cells are tested.
enum Visitor {
    Components(Component, Coords),
    Rect(EncodedRect),
}

impl Visitor {
    fn relate(&self, rel: ShapeRelation, min: &[u8], max: &[u8]) -> Relation {
        match self {
            Visitor::Components(c, k) => {
                let min_y = k.y(i32_from(&min[0..4]));
                let min_x = k.x(i32_from(&min[4..8]));
                let max_y = k.y(i32_from(&max[8..12]));
                let max_x = k.x(i32_from(&max[12..16]));
                c.relate(min_x, max_x, min_y, max_y)
            }
            Visitor::Rect(r) => {
                if matches!(rel, ShapeRelation::Intersects | ShapeRelation::Disjoint) {
                    r.intersect_range_bbox(min, max)
                } else {
                    r.relate_range_bbox(min, max)
                }
            }
        }
    }

    /// The relation BKD cells use (transposed for `disjoint`).
    fn inner(&self, rel: ShapeRelation, min: &[u8], max: &[u8]) -> Relation {
        let r = self.relate(rel, min, max);
        if rel == ShapeRelation::Disjoint { transpose(r) } else { r }
    }

    fn intersects(&self, t: &DecodedTriangle) -> bool {
        match self {
            Visitor::Components(c, k) => {
                let (ax, ay) = (k.x(t.a_x), k.y(t.a_y));
                match t.kind {
                    TriangleType::Point => c.contains(ax, ay),
                    TriangleType::Line => c.intersects_line(ax, ay, k.x(t.b_x), k.y(t.b_y)),
                    TriangleType::Triangle => c.intersects_triangle(ax, ay, k.x(t.b_x), k.y(t.b_y), k.x(t.c_x), k.y(t.c_y)),
                }
            }
            Visitor::Rect(r) => match t.kind {
                TriangleType::Point => r.contains(t.a_x, t.a_y),
                TriangleType::Line => r.intersects_line(t.a_x, t.a_y, t.b_x, t.b_y),
                TriangleType::Triangle => r.intersects_triangle(t.a_x, t.a_y, t.b_x, t.b_y, t.c_x, t.c_y),
            },
        }
    }

    fn within(&self, t: &DecodedTriangle) -> bool {
        match self {
            Visitor::Components(c, k) => {
                let (ax, ay) = (k.x(t.a_x), k.y(t.a_y));
                match t.kind {
                    TriangleType::Point => c.contains(ax, ay),
                    TriangleType::Line => c.contains_line(ax, ay, k.x(t.b_x), k.y(t.b_y)),
                    TriangleType::Triangle => c.contains_triangle(ax, ay, k.x(t.b_x), k.y(t.b_y), k.x(t.c_x), k.y(t.c_y)),
                }
            }
            Visitor::Rect(r) => match t.kind {
                TriangleType::Point => r.contains(t.a_x, t.a_y),
                TriangleType::Line => r.contains_line(t.a_x, t.a_y, t.b_x, t.b_y),
                TriangleType::Triangle => r.contains_triangle(t.a_x, t.a_y, t.b_x, t.b_y, t.c_x, t.c_y),
            },
        }
    }

    fn contains(&self, t: &DecodedTriangle) -> Result<WithinRelation> {
        match self {
            Visitor::Components(c, k) => {
                let (ax, ay) = (k.x(t.a_x), k.y(t.a_y));
                let r = match t.kind {
                    TriangleType::Point => c.within_point(ax, ay),
                    TriangleType::Line => c.within_line(ax, ay, t.ab, k.x(t.b_x), k.y(t.b_y)),
                    TriangleType::Triangle => {
                        c.within_triangle(ax, ay, t.ab, k.x(t.b_x), k.y(t.b_y), t.bc, k.x(t.c_x), k.y(t.c_y), t.ca)
                    }
                };
                r.ok_or_else(|| Error::illegal("withinTriangle is not supported for shapes with more than one component"))
            }
            Visitor::Rect(r) => {
                if r.wraps {
                    return Err(Error::illegal("withinTriangle is not supported for rectangles crossing the date line"));
                }
                Ok(match t.kind {
                    TriangleType::Point => {
                        if r.contains(t.a_x, t.a_y) {
                            WithinRelation::NotWithin
                        } else {
                            WithinRelation::Disjoint
                        }
                    }
                    TriangleType::Line => r.within_line(t.a_x, t.a_y, t.ab, t.b_x, t.b_y),
                    TriangleType::Triangle => r.within_triangle(t.a_x, t.a_y, t.ab, t.b_x, t.b_y, t.bc, t.c_x, t.c_y, t.ca),
                })
            }
        }
    }
}

fn transpose(r: Relation) -> Relation {
    match r {
        Relation::CellInsideQuery => Relation::CellOutsideQuery,
        Relation::CellOutsideQuery => Relation::CellInsideQuery,
        Relation::CellCrossesQuery => Relation::CellCrossesQuery,
    }
}

/// Lucene's `EncodedLatLonRectangle` (bounding-box queries on shapes).
#[derive(Clone, Copy, Debug)]
struct EncodedRect {
    min_x: i32,
    max_x: i32,
    min_y: i32,
    max_y: i32,
    wraps: bool,
    /// `(minY, minX, maxY, maxX)` of the east box, and of the west box when wrapping.
    bbox: [i32; 4],
    west: Option<[i32; 4]>,
}

const MIN_LON_ENCODED: i32 = i32::MIN;
const MAX_LON_ENCODED: i32 = i32::MAX;

impl EncodedRect {
    fn new(min_lat: f64, max_lat: f64, min_lon: f64, max_lon: f64) -> EncodedRect {
        let vmin = if min_lon == 180.0 && min_lon > max_lon { -180.0 } else { min_lon };
        let min_x = encode_longitude_ceil(vmin);
        let max_x = encode_longitude(max_lon);
        let min_y = encode_latitude_ceil(min_lat);
        let max_y = encode_latitude(max_lat);
        let wraps = vmin > max_lon;
        let (bbox, west) = if wraps {
            ([min_y, min_x, max_y, MAX_LON_ENCODED], Some([min_y, MIN_LON_ENCODED, max_y, max_x]))
        } else {
            ([min_y, min_x, max_y, max_x], None)
        };
        EncodedRect { min_x, max_x, min_y, max_y, wraps, bbox, west }
    }

    fn contains(&self, x: i32, y: i32) -> bool {
        if y < self.min_y || y > self.max_y {
            return false;
        }
        if self.wraps { !(x > self.max_x && x < self.min_x) } else { !(x > self.max_x || x < self.min_x) }
    }

    fn relate_range_bbox(&self, min: &[u8], max: &[u8]) -> Relation {
        let r = compare_bbox(&self.bbox, min, max);
        if let Some(w) = &self.west
            && r == Relation::CellOutsideQuery
        {
            return compare_bbox(w, min, max);
        }
        r
    }

    fn intersect_range_bbox(&self, min: &[u8], max: &[u8]) -> Relation {
        let r = intersect_bbox(&self.bbox, min, max);
        if let Some(w) = &self.west
            && r == Relation::CellOutsideQuery
        {
            return intersect_bbox(w, min, max);
        }
        r
    }

    fn intersects_line(&self, ax: i32, ay: i32, bx: i32, by: i32) -> bool {
        if self.contains(ax, ay) || self.contains(bx, by) {
            return true;
        }
        if ay.max(by) < self.min_y || ay.min(by) > self.max_y {
            return false;
        }
        if self.wraps {
            if ax.min(bx) > self.max_x && ax.max(bx) < self.min_x {
                return false;
            }
        } else if ax.min(bx) > self.max_x || ax.max(bx) < self.min_x {
            return false;
        }
        self.edge_intersects_query(ax, ay, bx, by)
    }

    fn intersects_triangle(&self, ax: i32, ay: i32, bx: i32, by: i32, cx: i32, cy: i32) -> bool {
        if self.contains(ax, ay) || self.contains(bx, by) || self.contains(cx, cy) {
            return true;
        }
        let (t_min_y, t_max_y) = (ay.min(by).min(cy), ay.max(by).max(cy));
        if t_max_y < self.min_y || t_min_y > self.max_y {
            return false;
        }
        let (t_min_x, t_max_x) = (ax.min(bx).min(cx), ax.max(bx).max(cx));
        if self.wraps {
            if t_min_x > self.max_x && t_max_x < self.min_x {
                return false;
            }
        } else if t_min_x > self.max_x || t_max_x < self.min_x {
            return false;
        }
        let f = |v: i32| v as f64;
        point_in_triangle(
            f(t_min_x),
            f(t_max_x),
            f(t_min_y),
            f(t_max_y),
            f(self.min_x),
            f(self.min_y),
            f(ax),
            f(ay),
            f(bx),
            f(by),
            f(cx),
            f(cy),
        ) || self.edge_intersects_query(ax, ay, bx, by)
            || self.edge_intersects_query(bx, by, cx, cy)
            || self.edge_intersects_query(cx, cy, ax, ay)
    }

    fn contains_line(&self, ax: i32, ay: i32, bx: i32, by: i32) -> bool {
        if ay < self.min_y || by < self.min_y || ay > self.max_y || by > self.max_y {
            return false;
        }
        if self.wraps {
            (ax >= self.min_x && bx >= self.min_x) || (ax <= self.max_x && bx <= self.max_x)
        } else {
            ax >= self.min_x && bx >= self.min_x && ax <= self.max_x && bx <= self.max_x
        }
    }

    fn contains_triangle(&self, ax: i32, ay: i32, bx: i32, by: i32, cx: i32, cy: i32) -> bool {
        if ay < self.min_y || by < self.min_y || cy < self.min_y || ay > self.max_y || by > self.max_y || cy > self.max_y {
            return false;
        }
        if self.wraps {
            (ax >= self.min_x && bx >= self.min_x && cx >= self.min_x) || (ax <= self.max_x && bx <= self.max_x && cx <= self.max_x)
        } else {
            ax >= self.min_x && bx >= self.min_x && cx >= self.min_x && ax <= self.max_x && bx <= self.max_x && cx <= self.max_x
        }
    }

    fn within_line(&self, ax: i32, ay: i32, ab: bool, bx: i32, by: i32) -> WithinRelation {
        if self.contains(ax, ay) || self.contains(bx, by) {
            return WithinRelation::NotWithin;
        }
        if ab && edge_intersects_box(ax, ay, bx, by, self.min_x, self.max_x, self.min_y, self.max_y) {
            return WithinRelation::NotWithin;
        }
        WithinRelation::Disjoint
    }

    #[allow(clippy::too_many_arguments)]
    fn within_triangle(&self, ax: i32, ay: i32, ab: bool, bx: i32, by: i32, bc: bool, cx: i32, cy: i32, ca: bool) -> WithinRelation {
        if self.contains(ax, ay) || self.contains(bx, by) || self.contains(cx, cy) {
            return WithinRelation::NotWithin;
        }
        let (t_min_y, t_max_y) = (ay.min(by).min(cy), ay.max(by).max(cy));
        if t_max_y < self.min_y || t_min_y > self.max_y {
            return WithinRelation::Disjoint;
        }
        let (t_min_x, t_max_x) = (ax.min(bx).min(cx), ax.max(bx).max(cx));
        if self.wraps {
            if t_min_x > self.max_x && t_max_x < self.min_x {
                return WithinRelation::Disjoint;
            }
        } else if t_min_x > self.max_x || t_max_x < self.min_x {
            return WithinRelation::Disjoint;
        }
        let mut rel = WithinRelation::Disjoint;
        for (x1, y1, x2, y2, from) in [(ax, ay, bx, by, ab), (bx, by, cx, cy, bc), (cx, cy, ax, ay, ca)] {
            if edge_intersects_box(x1, y1, x2, y2, self.min_x, self.max_x, self.min_y, self.max_y) {
                if from {
                    return WithinRelation::NotWithin;
                }
                rel = WithinRelation::Candidate;
            }
        }
        let f = |v: i32| v as f64;
        if rel == WithinRelation::Candidate
            || point_in_triangle(
                f(t_min_x),
                f(t_max_x),
                f(t_min_y),
                f(t_max_y),
                f(self.min_x),
                f(self.min_y),
                f(ax),
                f(ay),
                f(bx),
                f(by),
                f(cx),
                f(cy),
            )
        {
            return WithinRelation::Candidate;
        }
        rel
    }

    fn edge_intersects_query(&self, ax: i32, ay: i32, bx: i32, by: i32) -> bool {
        if self.wraps {
            return edge_intersects_box(ax, ay, bx, by, MIN_LON_ENCODED, self.max_x, self.min_y, self.max_y)
                || edge_intersects_box(ax, ay, bx, by, self.min_x, MAX_LON_ENCODED, self.min_y, self.max_y);
        }
        edge_intersects_box(ax, ay, bx, by, self.min_x, self.max_x, self.min_y, self.max_y)
    }
}

#[allow(clippy::too_many_arguments)]
fn edge_intersects_box(ax: i32, ay: i32, bx: i32, by: i32, min_x: i32, max_x: i32, min_y: i32, max_y: i32) -> bool {
    if ax.max(bx) < min_x || ax.min(bx) > max_x || ay.min(by) > max_y || ay.max(by) < min_y {
        return false;
    }
    let f = |v: i32| v as f64;
    let (ax, ay, bx, by) = (f(ax), f(ay), f(bx), f(by));
    let (min_x, max_x, min_y, max_y) = (f(min_x), f(max_x), f(min_y), f(max_y));
    line_crosses_line_with_boundary(ax, ay, bx, by, min_x, max_y, max_x, max_y)
        || line_crosses_line_with_boundary(ax, ay, bx, by, max_x, max_y, max_x, min_y)
        || line_crosses_line_with_boundary(ax, ay, bx, by, max_x, min_y, min_x, min_y)
        || line_crosses_line_with_boundary(ax, ay, bx, by, min_x, min_y, min_x, max_y)
}

/// The value at index dimension `k` (0 minY, 1 minX, 2 maxY, 3 maxX) of a packed bound.
#[inline]
fn dim(b: &[u8], k: usize) -> i32 {
    i32_from(&b[k * 4..k * 4 + 4])
}

fn disjoint_bbox(bbox: &[i32; 4], min: &[u8], max: &[u8]) -> bool {
    dim(min, 1) > bbox[3] || dim(max, 3) < bbox[1] || dim(min, 0) > bbox[2] || dim(max, 2) < bbox[0]
}

/// `compareBBoxToRangeBBox`.
fn compare_bbox(bbox: &[i32; 4], min: &[u8], max: &[u8]) -> Relation {
    if disjoint_bbox(bbox, min, max) {
        return Relation::CellOutsideQuery;
    }
    if dim(min, 1) >= bbox[1] && dim(max, 3) <= bbox[3] && dim(min, 0) >= bbox[0] && dim(max, 2) <= bbox[2] {
        return Relation::CellInsideQuery;
    }
    Relation::CellCrossesQuery
}

/// `intersectBBoxWithRangeBBox`: inside when every triangle has a corner of its box inside.
fn intersect_bbox(bbox: &[i32; 4], min: &[u8], max: &[u8]) -> Relation {
    if disjoint_bbox(bbox, min, max) {
        return Relation::CellOutsideQuery;
    }
    // (minX, minY) of every triangle inside.
    if dim(min, 1) >= bbox[1] && dim(min, 0) >= bbox[0] {
        if dim(max, 1) <= bbox[3] && dim(max, 2) <= bbox[2] {
            return Relation::CellInsideQuery;
        }
        if dim(max, 3) <= bbox[3] && dim(max, 0) <= bbox[2] {
            return Relation::CellInsideQuery;
        }
    }
    if dim(max, 3) <= bbox[3] && dim(max, 2) <= bbox[2] {
        if dim(min, 1) >= bbox[1] && dim(min, 2) >= bbox[0] {
            return Relation::CellInsideQuery;
        }
        if dim(min, 3) >= bbox[1] && dim(min, 0) >= bbox[0] {
            return Relation::CellInsideQuery;
        }
    }
    Relation::CellCrossesQuery
}

/// The docs of one segment (Lucene's `SpatialQuery` scorers).
fn shape_docs(seg: &SegmentContext, field: u32, v: &Visitor, rel: ShapeRelation) -> Result<Option<DocIdSet>> {
    let Some(p) = seg.reader.points(field) else { return Ok(None) };
    if p.spec().packed_len() != 28 || p.spec().index_dims != 4 {
        return Ok(None);
    }
    let max_doc = seg.max_doc();
    let (gmin, gmax) = (p.min_packed_value(), p.max_packed_value());
    let root = v.inner(rel, gmin, gmax);
    if root == Relation::CellOutsideQuery || (root == Relation::CellInsideQuery && rel == ShapeRelation::Contains) {
        return Ok(None);
    }
    if p.doc_count() == max_doc && root == Relation::CellInsideQuery {
        return Ok(Some(DocIdSet::All(max_doc)));
    }
    let mut result = FixedBitSet::new(max_doc);
    let mut excluded = FixedBitSet::new(max_doc);
    let mut err: Option<Error> = None;
    struct V<'a> {
        v: &'a Visitor,
        rel: ShapeRelation,
        result: &'a mut FixedBitSet,
        excluded: &'a mut FixedBitSet,
        err: &'a mut Option<Error>,
    }
    impl PointVisitor for V<'_> {
        fn compare(&mut self, min: &[u8], max: &[u8]) -> Relation {
            self.v.inner(self.rel, min, max)
        }
        fn visit(&mut self, doc: u32) {
            if self.rel == ShapeRelation::Contains {
                self.excluded.set(doc);
            } else {
                self.result.set(doc);
            }
        }
        fn visit_value(&mut self, doc: u32, packed: &[u8]) {
            let Some(t) = decode_triangle(packed) else { return };
            match self.rel {
                ShapeRelation::Intersects => {
                    if !self.result.get(doc) && self.v.intersects(&t) {
                        self.result.set(doc);
                    }
                }
                ShapeRelation::Within | ShapeRelation::Disjoint => {
                    if !self.excluded.get(doc) {
                        let pass = if self.rel == ShapeRelation::Within { self.v.within(&t) } else { !self.v.intersects(&t) };
                        if pass {
                            self.result.set(doc);
                        } else {
                            self.excluded.set(doc);
                        }
                    }
                }
                ShapeRelation::Contains => {
                    if !self.excluded.get(doc) {
                        match self.v.contains(&t) {
                            Ok(WithinRelation::Candidate) => self.result.set(doc),
                            Ok(WithinRelation::NotWithin) => self.excluded.set(doc),
                            Ok(WithinRelation::Disjoint) => {}
                            Err(e) => {
                                if self.err.is_none() {
                                    *self.err = Some(e);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    p.intersect(&mut V { v, rel, result: &mut result, excluded: &mut excluded, err: &mut err });
    if let Some(e) = err {
        return Err(e);
    }
    if rel != ShapeRelation::Intersects {
        result.and_not(&excluded);
    }
    if matches!(rel, ShapeRelation::Within | ShapeRelation::Disjoint) {
        // Docs with triangles in cells failing as a whole (Lucene's shallow inverse visitor).
        struct Inv<'a> {
            v: &'a Visitor,
            rel: ShapeRelation,
            result: &'a mut FixedBitSet,
        }
        impl PointVisitor for Inv<'_> {
            fn compare(&mut self, min: &[u8], max: &[u8]) -> Relation {
                transpose(self.v.inner(self.rel, min, max))
            }
            fn visit(&mut self, doc: u32) {
                self.result.clear(doc);
            }
            fn visit_value(&mut self, _doc: u32, _packed: &[u8]) {}
        }
        p.intersect(&mut Inv { v, rel, result: &mut result });
    }
    let mut b = DocIdSetBuilder::new(max_doc);
    let mut d = result.next_set_bit(0);
    while d < max_doc {
        b.add(d);
        d = result.next_set_bit(d + 1);
    }
    Ok(Some(b.build()))
}

struct ShapeWeight {
    field: u32,
    visitor: Arc<Visitor>,
    relation: ShapeRelation,
    score: f32,
    desc: String,
}

impl Weight for ShapeWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        Ok(shape_docs(seg, self.field, &self.visitor, self.relation)?
            .filter(|s| !matches!(s, DocIdSet::Empty))
            .map(|s| Box::new(SetScorer::new(Arc::new(s), self.score)) as Box<dyn Scorer>))
    }
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        explain_const(self, seg, doc, &self.desc, self.score)
    }
}

impl Query for GeoShapeQuery {
    fn weight(&self, s: &Searcher, _mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let visitor = match self.geometries.as_slice() {
            [QueryGeometry::Rectangle { min_lat, max_lat, min_lon, max_lon }] => Visitor::Rect(EncodedRect::new(*min_lat, *max_lat, *min_lon, *max_lon)),
            [] => return crate::query::basic::MatchNoneQuery::new("empty geometries").weight(s, ScoreMode::CompleteNoScores, boost),
            gs => Visitor::Components(Component::tree(gs.iter().map(|g| g.component()).collect()), Coords::Geo),
        };
        Ok(Box::new(ShapeWeight { field: self.field, visitor: Arc::new(visitor), relation: self.relation, score: boost, desc: self.lucene_string(s.schema()) }))
    }

    /// Lucene's rewrites: `contains` on several geometries (or a rectangle across the
    /// dateline) is a conjunction of one query per part.
    fn rewrite(&self, _s: &Searcher) -> Result<Option<QueryRef>> {
        if self.relation != ShapeRelation::Contains {
            return Ok(None);
        }
        let parts: Vec<QueryGeometry> = match self.geometries.as_slice() {
            [QueryGeometry::Rectangle { min_lat, max_lat, min_lon, max_lon }] if min_lon > max_lon => vec![
                QueryGeometry::Rectangle { min_lat: *min_lat, max_lat: *max_lat, min_lon: *min_lon, max_lon: 180.0 },
                QueryGeometry::Rectangle { min_lat: *min_lat, max_lat: *max_lat, min_lon: -180.0, max_lon: *max_lon },
            ],
            gs if gs.len() > 1 => gs.to_vec(),
            _ => return Ok(None),
        };
        let mut b = BooleanQuery::new();
        for g in parts {
            b = b.must(Arc::new(GeoShapeQuery { field: self.field, geometries: vec![g], relation: ShapeRelation::Contains }));
        }
        Ok(Some(Arc::new(crate::query::basic::ConstantScoreQuery::new(Arc::new(b)))))
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        if let [g @ QueryGeometry::Rectangle { .. }] = self.geometries.as_slice() {
            return format!("LatLonShapeBoundingBoxQuery: field={}:{}", schema.name(self.field), g.describe());
        }
        let gs: Vec<String> = self.geometries.iter().map(|g| format!("{},", g.describe())).collect();
        format!("LatLonShapeQuery: field={}:[{}]", schema.name(self.field), gs.concat())
    }

    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_debug(h, "geo_shape_field", self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ------------------------------------------------------------------ cartesian (`shape`)

/// `XYShape.createIndexableFields(name, x, y)`.
pub fn xy_point_triangles(x: f32, y: f32) -> Result<Vec<PackedTriangle>> {
    XyGeometry::Point { x, y }.validate()?;
    let (x, y) = (encode_xy(x), encode_xy(y));
    Ok(vec![encode_triangle(y, x, true, y, x, true, y, x, true)?])
}

/// `XYShape.createIndexableFields(name, line)`: one flat triangle per segment.
pub fn xy_line_triangles(xs: &[f32], ys: &[f32]) -> Result<Vec<PackedTriangle>> {
    XyGeometry::Line { xs: xs.to_vec(), ys: ys.to_vec() }.validate()?;
    let mut out = Vec::with_capacity(xs.len() - 1);
    for j in 1..xs.len() {
        let i = j - 1;
        let (ax, ay) = (encode_xy(xs[i]), encode_xy(ys[i]));
        let (bx, by) = (encode_xy(xs[j]), encode_xy(ys[j]));
        out.push(encode_triangle(ay, ax, true, by, bx, true, ay, ax, true)?);
    }
    Ok(out)
}

/// Docs whose `shape` (cartesian triangles) relates to the geometries (Lucene's
/// `XYShapeQuery`; build it through [`xy::shape_query`](crate::query::xy::shape_query), which
/// splits `contains` over several geometries as `XYShape.newGeometryQuery` does).
#[derive(Clone, Debug, PartialEq)]
pub struct XyShapeQuery {
    pub field: u32,
    pub geometries: Vec<XyGeometry>,
    pub relation: ShapeRelation,
}

impl XyShapeQuery {
    /// Validates the geometries (Lucene's constructor messages).
    pub fn new(field: u32, geometries: Vec<XyGeometry>, relation: ShapeRelation) -> Result<XyShapeQuery> {
        crate::query::xy::component_of(&geometries)?;
        Ok(XyShapeQuery { field, geometries, relation })
    }
}

impl Query for XyShapeQuery {
    fn weight(&self, s: &Searcher, _mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let visitor = Visitor::Components(crate::query::xy::component_of(&self.geometries)?, Coords::Xy);
        Ok(Box::new(ShapeWeight { field: self.field, visitor: Arc::new(visitor), relation: self.relation, score: boost, desc: self.lucene_string(s.schema()) }))
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        let gs: Vec<String> = self.geometries.iter().map(|g| format!("{},", g.lucene_string())).collect();
        format!("XYShapeQuery: field={}:[{}]", schema.name(self.field), gs.concat())
    }

    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_debug(h, "xy_shape_field", self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Elasticsearch's `geo_shape` query on a `geo_shape` field: `LatLonShape.newGeometryQuery`
/// ([`GeoShapeQuery`]) wrapped with its doc-values twin ([`ShapeDvQuery`]) when the field has
/// both, or the doc-values query alone for `index: false`. Geometries are the caller's
/// (quantized). No geometries match nothing.
pub fn geo_shape_query(field: u32, relation: ShapeRelation, geometries: Vec<QueryGeometry>, indexed: bool, doc_values: bool) -> Result<QueryRef> {
    if !indexed && !doc_values {
        return Err(Error::illegal("a shape query needs an indexed field or doc values"));
    }
    if geometries.is_empty() {
        return Ok(Arc::new(crate::query::basic::MatchNoneQuery::new("")));
    }
    let dv = || -> QueryRef { Arc::new(ShapeDvQuery::geo(field, relation, geometries.clone())) };
    if !indexed {
        return Ok(dv());
    }
    let index: QueryRef = Arc::new(GeoShapeQuery::new(field, geometries.clone(), relation));
    if doc_values { Ok(Arc::new(crate::query::range::IndexOrDocValuesQuery { index, dv: dv() })) } else { Ok(index) }
}

// ------------------------------------------------------------------ doc values

/// Marks a [`doc_value`] with a bounding box header (a bare concatenation of triangles has a
/// length divisible by 28; this layout's never is).
const DV_BBOX: u8 = 0xB0;
const DV_HEADER: usize = 17;

/// The binary doc value of a shape field for [`ShapeDvQuery`]: the bounding box of every
/// triangle (`minY, minX, maxY, maxX` as packed index dimensions), then the doc's packed
/// triangles (every value of the field). The box lets a query settle most docs without
/// decoding a triangle, as Elasticsearch's triangle tree does at its root.
pub fn doc_value(triangles: &[PackedTriangle]) -> bytes::Bytes {
    if triangles.is_empty() {
        return bytes::Bytes::new();
    }
    let mut bb = [i32::MAX, i32::MAX, i32::MIN, i32::MIN];
    for t in triangles {
        bb[0] = bb[0].min(dim(t, 0));
        bb[1] = bb[1].min(dim(t, 1));
        bb[2] = bb[2].max(dim(t, 2));
        bb[3] = bb[3].max(dim(t, 3));
    }
    let mut out = Vec::with_capacity(DV_HEADER + triangles.len() * 28);
    out.push(DV_BBOX);
    for v in bb {
        out.extend_from_slice(&i32_bytes(v));
    }
    for t in triangles {
        out.extend_from_slice(t);
    }
    out.into()
}

/// The packed triangles of a [`doc_value`] (bare concatenations included).
pub fn doc_value_triangles(bytes: &[u8]) -> std::slice::ChunksExact<'_, u8> {
    split_doc_value(bytes).1.chunks_exact(28)
}

/// The bounding box of a [`doc_value`] as encoded `[minY, minX, maxY, maxX]`, if it has one.
pub fn doc_value_bbox(bytes: &[u8]) -> Option<[i32; 4]> {
    split_doc_value(bytes).0.map(|h| [dim(h, 0), dim(h, 1), dim(h, 2), dim(h, 3)])
}

fn split_doc_value(bytes: &[u8]) -> (Option<&[u8]>, &[u8]) {
    if bytes.len() % 28 == DV_HEADER % 28 && bytes.first() == Some(&DV_BBOX) {
        (Some(&bytes[1..DV_HEADER]), &bytes[DV_HEADER..])
    } else {
        (None, bytes)
    }
}

/// The geometries of a [`ShapeDvQuery`].
#[derive(Clone, Debug, PartialEq)]
pub enum ShapeGeometries {
    /// `geo_shape` (Elasticsearch's `LatLonShapeDocValuesQuery`).
    Geo(Vec<QueryGeometry>),
    /// `shape` (Elasticsearch's `CartesianShapeDocValuesQuery`).
    Xy(Vec<XyGeometry>),
}

/// Docs whose shape, read from a binary doc value of packed triangles ([`doc_value`]), relates
/// to the geometries: Elasticsearch's `ShapeDocValuesQuery`, the `dvQuery` of shape
/// `IndexOrDocValuesQuery`s and the query of `index: false` shape fields. Two-phase; the
/// relations are the index query's (`intersects`: a triangle intersects; `disjoint`: none
/// does; `within`: all are within; `contains`: per geometry, a candidate triangle and none
/// proving the geometry is not within the shape).
#[derive(Clone, Debug, PartialEq)]
pub struct ShapeDvQuery {
    pub field: u32,
    pub geometries: ShapeGeometries,
    pub relation: ShapeRelation,
}

impl ShapeDvQuery {
    pub fn geo(field: u32, relation: ShapeRelation, geometries: Vec<QueryGeometry>) -> ShapeDvQuery {
        ShapeDvQuery { field, geometries: ShapeGeometries::Geo(geometries), relation }
    }

    /// Validates the geometries (Lucene's messages).
    pub fn xy(field: u32, relation: ShapeRelation, geometries: Vec<XyGeometry>) -> Result<ShapeDvQuery> {
        crate::query::xy::component_of(&geometries)?;
        Ok(ShapeDvQuery { field, geometries: ShapeGeometries::Xy(geometries), relation })
    }

    /// The visitors a doc must pass: one component of every geometry, or one per geometry for
    /// `contains` (geo rectangles across the dateline split in two, as Elasticsearch does).
    fn visitors(&self) -> Result<Vec<Visitor>> {
        let contains = self.relation == ShapeRelation::Contains;
        Ok(match &self.geometries {
            ShapeGeometries::Geo(gs) if contains => {
                let mut out = Vec::new();
                for g in gs {
                    match g {
                        QueryGeometry::Rectangle { min_lat, max_lat, min_lon, max_lon } if min_lon > max_lon => {
                            out.push(Visitor::Components(Component::rectangle(*min_lat, *max_lat, *min_lon, 180.0), Coords::Geo));
                            out.push(Visitor::Components(Component::rectangle(*min_lat, *max_lat, -180.0, *max_lon), Coords::Geo));
                        }
                        g => out.push(Visitor::Components(g.component(), Coords::Geo)),
                    }
                }
                out
            }
            ShapeGeometries::Geo(gs) => {
                if gs.is_empty() {
                    return Err(Error::illegal("geometries must not be empty"));
                }
                vec![Visitor::Components(Component::tree(gs.iter().map(|g| g.component()).collect()), Coords::Geo)]
            }
            ShapeGeometries::Xy(gs) if contains => {
                gs.iter().map(|g| Visitor::Components(g.component(), Coords::Xy)).collect()
            }
            ShapeGeometries::Xy(gs) => vec![Visitor::Components(crate::query::xy::component_of(gs)?, Coords::Xy)],
        })
    }
}

/// Whether a doc's triangles relate to one visitor's geometry.
fn dv_relates(v: &Visitor, rel: ShapeRelation, bytes: &[u8]) -> bool {
    let (header, body) = split_doc_value(bytes);
    if let Some(h) = header
        && !body.is_empty()
    {
        // The box as a BKD cell holding every triangle: each triangle's min corner lies in
        // [min, max] of dims 0-1 and its max corner in dims 2-3. Outside the query no
        // triangle touches it; inside, every triangle lies within it.
        let mut min = [0u8; 16];
        let mut max = [0u8; 16];
        min[0..8].copy_from_slice(&h[0..8]);
        min[8..16].copy_from_slice(&h[0..8]);
        max[0..8].copy_from_slice(&h[8..16]);
        max[8..16].copy_from_slice(&h[8..16]);
        match (v.relate(rel, &min, &max), rel) {
            (Relation::CellOutsideQuery, ShapeRelation::Disjoint) => return true,
            (Relation::CellOutsideQuery, _) => return false,
            (Relation::CellInsideQuery, ShapeRelation::Intersects | ShapeRelation::Within) => return true,
            (Relation::CellInsideQuery, ShapeRelation::Disjoint) => return false,
            _ => {}
        }
    }
    let mut tris = body.chunks_exact(28).filter_map(decode_triangle);
    match rel {
        ShapeRelation::Intersects => tris.any(|t| v.intersects(&t)),
        ShapeRelation::Disjoint => !tris.any(|t| v.intersects(&t)),
        ShapeRelation::Within => tris.all(|t| v.within(&t)),
        ShapeRelation::Contains => {
            let mut candidate = false;
            for t in tris {
                match v.contains(&t) {
                    Ok(WithinRelation::Candidate) => candidate = true,
                    Ok(WithinRelation::Disjoint) => {}
                    Ok(WithinRelation::NotWithin) | Err(_) => return false,
                }
            }
            candidate
        }
    }
}

struct ShapeDvWeight {
    field: u32,
    visitors: Arc<Vec<Visitor>>,
    relation: ShapeRelation,
    score: f32,
    desc: String,
}

impl Weight for ShapeDvWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let Some(search_codec::DocValuesReader::Binary(values)) = seg.reader.doc_values(self.field) else { return Ok(None) };
        let values = values.clone();
        let docs = values.docs().clone();
        let (visitors, rel) = (self.visitors.clone(), self.relation);
        let check: Box<dyn FnMut(u32) -> bool + Send> =
            Box::new(move |d| values.get(d).is_some_and(|b| visitors.iter().all(|v| dv_relates(v, rel, b))));
        Ok(Some(Box::new(crate::query::range::DvScorer::new(&docs, check, self.score, 1000.0))))
    }
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        explain_const(self, seg, doc, &self.desc, self.score)
    }
}

impl Query for ShapeDvQuery {
    fn weight(&self, s: &Searcher, _mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        Ok(Box::new(ShapeDvWeight {
            field: self.field,
            visitors: Arc::new(self.visitors()?),
            relation: self.relation,
            score: boost,
            desc: self.lucene_string(s.schema()),
        }))
    }

    /// Elasticsearch's `ShapeDocValuesQuery.toString`: `field:RELATION:[geometries]`.
    fn lucene_string(&self, schema: &Schema) -> String {
        let gs: Vec<String> = match &self.geometries {
            ShapeGeometries::Geo(gs) => gs.iter().map(QueryGeometry::describe).collect(),
            ShapeGeometries::Xy(gs) => gs.iter().map(XyGeometry::lucene_string).collect(),
        };
        format!("{}:{}:[{}]", schema.name(self.field), self.relation.lucene_name(), gs.join(", "))
    }

    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_debug(h, "shape_dv", self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triangles_round_trip() {
        // Every vertex order and orientation decodes to the same triangle, counter-clockwise.
        let pts = [(10, 20), (40, 5), (25, 50)];
        for perm in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
            let [a, b, c] = perm.map(|i| pts[i]);
            let enc = encode_triangle(a.1, a.0, true, b.1, b.0, false, c.1, c.0, true).unwrap();
            let d = decode_triangle(&enc).unwrap();
            assert_eq!(d.kind, TriangleType::Triangle);
            assert!(iorient(d.a_x, d.a_y, d.b_x, d.b_y, d.c_x, d.c_y) >= 0);
            let mut got = vec![(d.a_x, d.a_y), (d.b_x, d.b_y), (d.c_x, d.c_y)];
            got.sort();
            let mut want = pts.to_vec();
            want.sort();
            assert_eq!(got, want);
            // (Edge flags follow Lucene, including its reorientation, which moves `bc` onto
            // `ab`.)
            // The bounding box leads the packed value.
            assert_eq!(dim(&enc, 0), 5);
            assert_eq!(dim(&enc, 1), 10);
            assert_eq!(dim(&enc, 2), 50);
            assert_eq!(dim(&enc, 3), 40);
        }
        let p = decode_triangle(&point_triangles(10.0, 20.0).unwrap()[0]).unwrap();
        assert_eq!(p.kind, TriangleType::Point);
        let l = decode_triangle(&line_triangles(&[0.0, 1.0], &[0.0, 1.0]).unwrap()[0]).unwrap();
        assert_eq!(l.kind, TriangleType::Line);
        assert!(l.ab);
        assert!(decode_triangle(&[0u8; 10]).is_none());
    }
}
