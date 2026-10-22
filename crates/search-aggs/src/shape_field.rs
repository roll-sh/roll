//! `geo_shape` and `shape` fields as aggregations read them (Elasticsearch's `GeoShapeValues` /
//! `CartesianShapeValues` over `GeometryDocValueReader`).
//!
//! A shape field's doc value is search-index's shape doc value
//! (`search_index::query::shape::doc_value`: a bounding-box header, then every packed triangle
//! of every value of the doc, in indexing order). From the triangles come:
//!
//! * the [`Extent`] — top, bottom, and the leftmost/rightmost encoded x of the triangles lying
//!   west and east of the antimeridian (Elasticsearch's `Extent.addRectangle` over each
//!   triangle's box), so `geo_bounds` can wrap the dateline exactly as Elasticsearch does;
//! * tile relations ([`ShapeValue::relate_tile`], `Tile2DVisitor`): which grid cells a shape
//!   touches, for `geotile_grid` / `geohash_grid`. Elasticsearch walks a k-d tree of the
//!   triangles, but its pruning only skips triangles disjoint from the tile and its relation
//!   only depends on the set of per-triangle relations, so a flat pass gives the same answer.
//!
//! The centroid (`geo_centroid` / `cartesian_centroid`) is computed when indexing from the
//! *original* geometry (before dateline splitting and tessellation: `CentroidCalculator`, so a
//! polygon spanning lon 170..190 has its centroid at 180, not near 0) and stored in a companion
//! binary doc-values field ([`encode_centroid`]: encoded x and y, the highest
//! [`DimensionalShapeType`] and the summed weight). Without that field the centroid is
//! approximated from the triangles (area-weighted triangles, length-weighted segments, points).

use search_codec::points::encode::{decode_latitude, decode_longitude, encode_latitude, encode_longitude};
use search_index::query::shape::{DecodedTriangle, TriangleType, decode_triangle, doc_value_triangles};
use search_index::util::tessellator::{decode_xy, encode_xy, orient};

use crate::error::{AggError, Result};
use crate::internal::CompensatedSum;

/// `DimensionalShapeType`: the highest dimension among a doc's geometries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum DimensionalShapeType {
    #[default]
    Point = 0,
    Line = 1,
    Polygon = 2,
}

impl DimensionalShapeType {
    pub fn from_byte(b: u8) -> Option<DimensionalShapeType> {
        Some(match b {
            0 => DimensionalShapeType::Point,
            1 => DimensionalShapeType::Line,
            2 => DimensionalShapeType::Polygon,
            _ => return None,
        })
    }
}

/// A ring or line: `(x, y)` vertices.
pub type Ring = Vec<(f64, f64)>;
/// A polygon: its shell and holes.
pub type PolygonRings = (Ring, Vec<Ring>);

/// An Elasticsearch geometry (`org.elasticsearch.geometry`), coordinates as `(x, y)` (`(lon,
/// lat)` for `geo_shape`), as given in the document — what the centroid is computed from.
#[derive(Clone, Debug, PartialEq)]
pub enum Geometry {
    Point(f64, f64),
    MultiPoint(Vec<(f64, f64)>),
    Line(Vec<(f64, f64)>),
    MultiLine(Vec<Vec<(f64, f64)>>),
    /// Closed rings: the shell, then the holes.
    Polygon(Vec<(f64, f64)>, Vec<Vec<(f64, f64)>>),
    MultiPolygon(Vec<PolygonRings>),
    Rectangle { min_x: f64, max_x: f64, min_y: f64, max_y: f64 },
    Collection(Vec<Geometry>),
    /// Rejected by the calculator (shapes index circles only through an ingest processor).
    Circle,
    LinearRing,
}

/// `CentroidCalculator`: compensated sums of weighted coordinates of the highest-dimension
/// geometries seen (points weigh 1, line segments their length, polygon rings their signed
/// area — holes subtract).
#[derive(Clone, Copy, Debug, Default)]
pub struct CentroidCalculator {
    sum_x: CompensatedSum,
    sum_y: CompensatedSum,
    sum_w: CompensatedSum,
    shape_type: DimensionalShapeType,
}

impl CentroidCalculator {
    pub fn new() -> CentroidCalculator {
        CentroidCalculator::default()
    }

    /// `add(geometry)`.
    pub fn add(&mut self, g: &Geometry) -> Result<()> {
        match g {
            Geometry::Circle => return Err(AggError::illegal_argument("invalid shape type found [Circle] while calculating centroid")),
            Geometry::LinearRing => return Err(AggError::illegal_argument("invalid shape type found [LinearRing] while calculating centroid")),
            Geometry::Collection(gs) => {
                for g in gs {
                    self.add(g)?;
                }
            }
            Geometry::Line(pts) => {
                if self.shape_type != DimensionalShapeType::Polygon {
                    self.visit_line(pts);
                }
            }
            Geometry::MultiLine(lines) => {
                if self.shape_type != DimensionalShapeType::Polygon {
                    for l in lines {
                        self.visit_line(l);
                    }
                }
            }
            Geometry::MultiPoint(pts) => {
                if self.shape_type == DimensionalShapeType::Point {
                    for &(x, y) in pts {
                        self.add_coordinate(x, y, 1.0, DimensionalShapeType::Point);
                    }
                }
            }
            Geometry::Point(x, y) => {
                if self.shape_type == DimensionalShapeType::Point {
                    self.add_coordinate(*x, *y, 1.0, DimensionalShapeType::Point);
                }
            }
            Geometry::Polygon(shell, holes) => self.visit_polygon(shell, holes),
            Geometry::MultiPolygon(ps) => {
                for (shell, holes) in ps {
                    self.visit_polygon(shell, holes);
                }
            }
            Geometry::Rectangle { min_x, max_x, min_y, max_y } => {
                let w = ((max_x - min_x) * (max_y - min_y)).abs();
                if w != 0.0 {
                    self.add_coordinate((max_x + min_x) / 2.0, (max_y + min_y) / 2.0, w, DimensionalShapeType::Polygon);
                } else {
                    self.add(&Geometry::Line(vec![(*min_x, *min_y), (*max_x, *max_y)]))?;
                }
            }
        }
        Ok(())
    }

    /// `getX()`.
    pub fn x(&self) -> f64 {
        self.sum_x.value / self.sum_w.value
    }

    /// `getY()`.
    pub fn y(&self) -> f64 {
        self.sum_y.value / self.sum_w.value
    }

    /// `sumWeight()` (negative sums from degenerate polygons are 0).
    pub fn sum_weight(&self) -> f64 {
        let w = self.sum_w.value;
        if w < 0.0 { 0.0 } else { w }
    }

    pub fn shape_type(&self) -> DimensionalShapeType {
        self.shape_type
    }

    fn visit_polygon(&mut self, shell: &[(f64, f64)], holes: &[Vec<(f64, f64)>]) {
        let mut parts = Vec::with_capacity(1 + holes.len());
        parts.push(ring_centroid(shell, false));
        for h in holes {
            parts.push(ring_centroid(h, true));
        }
        let sum: f64 = parts.iter().fold(0.0, |s, p| s + p.2);
        if sum == 0.0 && self.shape_type != DimensionalShapeType::Polygon {
            self.visit_line(shell);
        } else {
            for (x, y, w) in parts {
                self.add_coordinate(x, y, w, DimensionalShapeType::Polygon);
            }
        }
    }

    fn visit_line(&mut self, pts: &[(f64, f64)]) {
        for w in pts.windows(2) {
            let ((x0, y0), (x1, y1)) = (w[0], w[1]);
            let (dx, dy) = (x0 - x1, y0 - y1);
            let weight = (dx * dx + dy * dy).sqrt();
            if weight == 0.0 {
                self.add_coordinate(x0, y0, 1.0, DimensionalShapeType::Point);
            } else {
                self.add_coordinate((x0 + x1) / 2.0, (y0 + y1) / 2.0, weight, DimensionalShapeType::Line);
            }
        }
    }

    fn add_coordinate(&mut self, x: f64, y: f64, w: f64, t: DimensionalShapeType) {
        if !(x.is_finite() && y.is_finite()) {
            return;
        }
        if self.shape_type == t {
            self.sum_x.add(x * w);
            self.sum_y.add(y * w);
            self.sum_w.add(w);
        } else if t > self.shape_type {
            self.sum_x = CompensatedSum::new(x * w, 0.0);
            self.sum_y = CompensatedSum::new(y * w, 0.0);
            self.sum_w = CompensatedSum::new(w, 0.0);
            self.shape_type = t;
        }
    }
}

/// `visitLinearRing`: a ring's centroid and signed area (negative for holes).
fn ring_centroid(r: &[(f64, f64)], hole: bool) -> (f64, f64, f64) {
    let mut area = 0.0;
    for w in r.windows(2) {
        area += w[0].0 * w[1].1 - w[1].0 * w[0].1;
    }
    area /= 2.0;
    let (mut sx, mut sy) = (0.0, 0.0);
    for w in r.windows(2) {
        let twice = w[0].0 * w[1].1 - w[1].0 * w[0].1;
        sx += twice * (w[0].0 + w[1].0);
        sy += twice * (w[0].1 + w[1].1);
    }
    let sign = if hole { -1.0 } else { 1.0 };
    (sx / (6.0 * area), sy / (6.0 * area), sign * area.abs())
}

/// `GeoUtils.normalizeLon`.
pub fn normalize_lon(lon: f64) -> f64 {
    let lon = if lon > 180.0 || lon <= -180.0 { centered_modulus(lon, 360.0) } else { lon };
    lon + 0.0
}

/// `GeoUtils.normalizeLat`.
pub fn normalize_lat(lat: f64) -> f64 {
    let mut lat = lat;
    if !(-90.0..=90.0).contains(&lat) {
        lat = centered_modulus(lat, 360.0);
        if lat < -90.0 {
            lat = -180.0 - lat;
        } else if lat > 90.0 {
            lat = 180.0 - lat;
        }
    }
    lat + 0.0
}

fn centered_modulus(dividend: f64, divisor: f64) -> f64 {
    let mut r = dividend % divisor;
    if r <= 0.0 {
        r += divisor;
    }
    if r > divisor / 2.0 {
        r -= divisor;
    }
    r
}

/// Encoding of a shape field's coordinates (`CoordinateEncoder`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coords {
    /// `GeoEncodingUtils` (lat/lon).
    Geo,
    /// `XYEncodingUtils` (sortable floats).
    Cartesian,
}

impl Coords {
    pub fn decode_x(self, v: i32) -> f64 {
        match self {
            Coords::Geo => decode_longitude(v),
            Coords::Cartesian => decode_cartesian(v),
        }
    }

    pub fn decode_y(self, v: i32) -> f64 {
        match self {
            Coords::Geo => decode_latitude(v),
            Coords::Cartesian => decode_cartesian(v),
        }
    }

    fn encode_x(self, v: f64) -> Result<i32> {
        match self {
            Coords::Geo => {
                if v == f64::NEG_INFINITY {
                    return Ok(i32::MIN);
                }
                if v == f64::INFINITY {
                    return Ok(i32::MAX);
                }
                let v = normalize_lon(v);
                if !(-180.0..=180.0).contains(&v) {
                    return Err(AggError::illegal_argument(format!("invalid longitude {v}; must be between -180.0 and 180.0")));
                }
                Ok(encode_longitude(v))
            }
            Coords::Cartesian => Ok(encode_cartesian(v)),
        }
    }

    fn encode_y(self, v: f64) -> Result<i32> {
        match self {
            Coords::Geo => {
                if v == f64::NEG_INFINITY {
                    return Ok(i32::MIN);
                }
                if v == f64::INFINITY {
                    return Ok(i32::MAX);
                }
                let v = normalize_lat(v);
                if !(-90.0..=90.0).contains(&v) {
                    return Err(AggError::illegal_argument(format!("invalid latitude {v}; must be between -90.0 and 90.0")));
                }
                Ok(encode_latitude(v))
            }
            Coords::Cartesian => Ok(encode_cartesian(v)),
        }
    }
}

fn decode_cartesian(v: i32) -> f64 {
    match v {
        i32::MIN => f64::NEG_INFINITY,
        i32::MAX => f64::INFINITY,
        v => decode_xy(v),
    }
}

fn encode_cartesian(v: f64) -> i32 {
    if v == f64::NEG_INFINITY {
        i32::MIN
    } else if v == f64::INFINITY {
        i32::MAX
    } else {
        encode_xy(v as f32)
    }
}

/// A doc's stored centroid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Centroid {
    /// Encoded coordinates.
    pub x: i32,
    pub y: i32,
    pub shape_type: DimensionalShapeType,
    pub weight: f64,
}

/// The companion centroid doc value (`GeometryDocValueWriter`'s header): big-endian encoded x
/// and y, the shape type, the weight's bits (big-endian). Fails when the centroid is not a
/// valid coordinate (a geometry without weight).
pub fn encode_centroid(c: &CentroidCalculator, coords: Coords) -> Result<[u8; 17]> {
    let x = coords.encode_x(c.x())?;
    let y = coords.encode_y(c.y())?;
    let mut out = [0u8; 17];
    out[0..4].copy_from_slice(&x.to_be_bytes());
    out[4..8].copy_from_slice(&y.to_be_bytes());
    out[8] = c.shape_type() as u8;
    out[9..17].copy_from_slice(&c.sum_weight().to_bits().to_be_bytes());
    Ok(out)
}

/// Reads [`encode_centroid`]'s bytes.
pub fn decode_centroid(b: &[u8]) -> Option<Centroid> {
    if b.len() < 17 {
        return None;
    }
    Some(Centroid {
        x: i32::from_be_bytes(b[0..4].try_into().ok()?),
        y: i32::from_be_bytes(b[4..8].try_into().ok()?),
        shape_type: DimensionalShapeType::from_byte(b[8])?,
        weight: f64::from_bits(u64::from_be_bytes(b[9..17].try_into().ok()?)),
    })
}

/// `InternalCentroid.ShapeData`: a shape centroid's weighted sums (geo: latitude first).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShapeData {
    pub first: f64,
    pub second: f64,
    pub weight: f64,
    pub shape_type: DimensionalShapeType,
}

/// `InternalCentroid`'s leader reducer over `(centroid, count, shape data)` partials: shape
/// partials combine by dimension (the highest wins, equal ones add up, the count is theirs);
/// point partials are re-weighted by count.
pub fn reduce_centroids(parts: impl Iterator<Item = (Option<(f64, f64)>, u64, Option<ShapeData>)>) -> (Option<(f64, f64)>, u64, Option<ShapeData>) {
    let (mut first, mut second, mut total) = (f64::NAN, f64::NAN, 0u64);
    let mut shape: Option<ShapeData> = None;
    let mut shape_count = 0u64;
    for (centroid, count, data) in parts {
        if count == 0 {
            continue;
        }
        match (data, centroid) {
            (Some(d), _) if d.weight > 0.0 => match &mut shape {
                Some(s) if d.shape_type < s.shape_type => {}
                Some(s) if d.shape_type == s.shape_type => {
                    s.first += d.first;
                    s.second += d.second;
                    s.weight += d.weight;
                    shape_count += count;
                }
                _ => {
                    shape = Some(d);
                    shape_count = count;
                }
            },
            (_, Some((a, b))) => {
                if let Some(s) = &mut shape {
                    s.first += count as f64 * a;
                    s.second += count as f64 * b;
                    s.weight += count as f64;
                    shape_count += count;
                } else {
                    total += count;
                    if first.is_nan() {
                        first = count as f64 * a;
                        second = count as f64 * b;
                    } else {
                        first += count as f64 * a;
                        second += count as f64 * b;
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(s) = shape {
        let c = (s.weight > 0.0).then(|| (s.first / s.weight, s.second / s.weight));
        return (c, shape_count, Some(s));
    }
    let c = if first.is_nan() { None } else { Some((first / total as f64, second / total as f64)) };
    (c, total, None)
}

/// `Extent`: the encoded bounds, with the x range split at 0 (west / east of the
/// antimeridian-opposite meridian) so dateline-crossing boxes can be rebuilt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    pub top: i32,
    pub bottom: i32,
    pub neg_left: i32,
    pub neg_right: i32,
    pub pos_left: i32,
    pub pos_right: i32,
}

impl Default for Extent {
    fn default() -> Self {
        Extent { top: i32::MIN, bottom: i32::MAX, neg_left: i32::MAX, neg_right: i32::MIN, pos_left: i32::MAX, pos_right: i32::MIN }
    }
}

impl Extent {
    /// `addRectangle`.
    pub fn add_rectangle(&mut self, min_x: i32, min_y: i32, max_x: i32, max_y: i32) {
        self.bottom = self.bottom.min(min_y);
        self.top = self.top.max(max_y);
        if min_x < 0 && max_x < 0 {
            self.neg_left = self.neg_left.min(min_x);
            self.neg_right = self.neg_right.max(max_x);
        } else if min_x < 0 {
            self.neg_left = self.neg_left.min(min_x);
            self.pos_right = self.pos_right.max(max_x);
            // The extent cannot wrap the dateline any more.
            self.neg_right = 0;
            self.pos_left = 0;
        } else {
            self.pos_left = self.pos_left.min(min_x);
            self.pos_right = self.pos_right.max(max_x);
        }
    }

    pub fn min_x(&self) -> i32 {
        self.neg_left.min(self.pos_left)
    }

    pub fn max_x(&self) -> i32 {
        self.neg_right.max(self.pos_right)
    }
}

/// `ShapeValues.BoundingBox`: the decoded extent (unset sides are infinite).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoundingBox {
    pub top: f64,
    pub bottom: f64,
    pub neg_left: f64,
    pub neg_right: f64,
    pub pos_left: f64,
    pub pos_right: f64,
}

impl BoundingBox {
    pub fn min_x(&self) -> f64 {
        crate::metrics::simple::java_min(self.neg_left, self.pos_left)
    }

    pub fn max_x(&self) -> f64 {
        crate::metrics::simple::java_max(self.neg_right, self.pos_right)
    }

    pub fn min_y(&self) -> f64 {
        self.bottom
    }

    pub fn max_y(&self) -> f64 {
        self.top
    }
}

/// `GeoRelation`: how a query cell relates to a shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeoRelation {
    Crosses,
    /// The cell lies inside the shape.
    Inside,
    Disjoint,
    /// The cell contains the whole shape.
    Contains,
}

/// One doc's shape: decoded triangles, extent and centroid.
#[derive(Clone, Debug)]
pub struct ShapeValue {
    pub coords: Coords,
    pub triangles: Vec<DecodedTriangle>,
    pub extent: Extent,
    pub centroid: Centroid,
}

impl ShapeValue {
    fn empty(coords: Coords) -> ShapeValue {
        ShapeValue {
            coords,
            triangles: Vec::new(),
            extent: Extent::default(),
            centroid: Centroid { x: 0, y: 0, shape_type: DimensionalShapeType::Point, weight: 0.0 },
        }
    }

    /// A doc value (and stored centroid) on its own; `None` without triangles.
    pub fn from_doc_value(dv: &[u8], centroid: Option<&[u8]>, coords: Coords) -> Result<Option<ShapeValue>> {
        let mut v = ShapeValue::empty(coords);
        Ok(if v.load(dv, centroid)? { Some(v) } else { None })
    }

    /// Loads a doc value (and its stored centroid, if any). `false` for a value without
    /// triangles.
    fn load(&mut self, dv: &[u8], centroid: Option<&[u8]>) -> Result<bool> {
        self.triangles.clear();
        self.extent = Extent::default();
        for t in doc_value_triangles(dv) {
            let d = decode_triangle(t).ok_or_else(|| AggError::new(500, "illegal_state_exception", "corrupt shape doc value"))?;
            let (min_x, max_x) = (d.a_x.min(d.b_x).min(d.c_x), d.a_x.max(d.b_x).max(d.c_x));
            let (min_y, max_y) = (d.a_y.min(d.b_y).min(d.c_y), d.a_y.max(d.b_y).max(d.c_y));
            self.extent.add_rectangle(min_x, min_y, max_x, max_y);
            self.triangles.push(d);
        }
        if self.triangles.is_empty() {
            return Ok(false);
        }
        self.centroid = match centroid.and_then(decode_centroid) {
            Some(c) => c,
            None => self.approximate_centroid(),
        };
        Ok(true)
    }

    /// The centroid from the triangles (no stored centroid): area-weighted triangles, else
    /// length-weighted segments, else points.
    fn approximate_centroid(&self) -> Centroid {
        let c = self.coords;
        let mut calc = CentroidCalculator::new();
        for t in &self.triangles {
            let (ax, ay) = (c.decode_x(t.a_x), c.decode_y(t.a_y));
            let (bx, by) = (c.decode_x(t.b_x), c.decode_y(t.b_y));
            match t.kind {
                TriangleType::Point => calc.add_coordinate(ax, ay, 1.0, DimensionalShapeType::Point),
                TriangleType::Line => {
                    let w = ((ax - bx) * (ax - bx) + (ay - by) * (ay - by)).sqrt();
                    calc.add_coordinate((ax + bx) / 2.0, (ay + by) / 2.0, w, DimensionalShapeType::Line);
                }
                TriangleType::Triangle => {
                    let (cx, cy) = (c.decode_x(t.c_x), c.decode_y(t.c_y));
                    let w = ((bx - ax) * (cy - ay) - (cx - ax) * (by - ay)).abs() / 2.0;
                    calc.add_coordinate((ax + bx + cx) / 3.0, (ay + by + cy) / 3.0, w, DimensionalShapeType::Polygon);
                }
            }
        }
        let x = c.encode_x(calc.x()).unwrap_or(0);
        let y = c.encode_y(calc.y()).unwrap_or(0);
        Centroid { x, y, shape_type: calc.shape_type(), weight: calc.sum_weight() }
    }

    /// `boundingBox()`.
    pub fn bounding_box(&self) -> BoundingBox {
        let (c, e) = (self.coords, &self.extent);
        let (neg_left, neg_right) = if e.neg_left == i32::MAX && e.neg_right == i32::MIN {
            (f64::INFINITY, f64::NEG_INFINITY)
        } else {
            (c.decode_x(e.neg_left), c.decode_x(e.neg_right))
        };
        let (pos_left, pos_right) = if e.pos_left == i32::MAX && e.pos_right == i32::MIN {
            (f64::INFINITY, f64::NEG_INFINITY)
        } else {
            (c.decode_x(e.pos_left), c.decode_x(e.pos_right))
        };
        BoundingBox { top: c.decode_y(e.top), bottom: c.decode_y(e.bottom), neg_left, neg_right, pos_left, pos_right }
    }

    /// The centroid's decoded `(x, y)`.
    pub fn centroid_xy(&self) -> (f64, f64) {
        (self.coords.decode_x(self.centroid.x), self.coords.decode_y(self.centroid.y))
    }

    /// `GeoShapeValue.relate(minX, maxX, minY, maxY)` (`Tile2DVisitor`) for an encoded cell.
    pub fn relate_tile(&self, min_x: i32, max_x: i32, min_y: i32, max_y: i32) -> GeoRelation {
        let q = Tile { min_x, max_x, min_y, max_y };
        let e = &self.extent;
        let (ex0, ex1, ey0, ey1) = (e.min_x(), e.max_x(), e.bottom, e.top);
        if ex0 > max_x || ex1 < min_x || ey0 > max_y || ey1 < min_y {
            return GeoRelation::Disjoint;
        }
        if min_x <= ex0 && max_x >= ex1 && min_y <= ey0 && max_y >= ey1 {
            return GeoRelation::Contains;
        }
        let mut rel = GeoRelation::Disjoint;
        for t in &self.triangles {
            match t.kind {
                TriangleType::Point => {
                    if q.contains(t.a_x, t.a_y) {
                        return GeoRelation::Crosses;
                    }
                }
                TriangleType::Line => {
                    if q.intersects_line(t.a_x, t.a_y, t.b_x, t.b_y) {
                        return GeoRelation::Crosses;
                    }
                }
                TriangleType::Triangle => match q.relate_triangle(t) {
                    GeoRelation::Crosses => return GeoRelation::Crosses,
                    GeoRelation::Disjoint => {}
                    r => rel = r,
                },
            }
        }
        rel
    }
}

/// `Tile2DVisitor`'s query box (encoded, inclusive).
struct Tile {
    min_x: i32,
    max_x: i32,
    min_y: i32,
    max_y: i32,
}

#[inline]
fn iorient(ax: i32, ay: i32, bx: i32, by: i32, cx: i32, cy: i32) -> i32 {
    orient(ax as f64, ay as f64, bx as f64, by as f64, cx as f64, cy as f64)
}

impl Tile {
    #[inline]
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.min_x && x <= self.max_x && y >= self.min_y && y <= self.max_y
    }

    fn intersects_line(&self, ax: i32, ay: i32, bx: i32, by: i32) -> bool {
        if self.contains(ax, ay) || self.contains(bx, by) {
            return true;
        }
        let (t_min_x, t_max_x, t_min_y, t_max_y) = (ax.min(bx), ax.max(bx), ay.min(by), ay.max(by));
        if t_max_x < self.min_x || t_min_x > self.max_x || t_min_y > self.max_y || t_max_y < self.min_y {
            return false;
        }
        self.edge_intersects(ax, ay, bx, by)
    }

    fn relate_triangle(&self, t: &DecodedTriangle) -> GeoRelation {
        let (ax, ay, bx, by, cx, cy) = (t.a_x, t.a_y, t.b_x, t.b_y, t.c_x, t.c_y);
        let (t_min_x, t_max_x) = (ax.min(bx).min(cx), ax.max(bx).max(cx));
        let (t_min_y, t_max_y) = (ay.min(by).min(cy), ay.max(by).max(cy));
        if t_max_x < self.min_x || t_min_x > self.max_x || t_min_y > self.max_y || t_max_y < self.min_y {
            return GeoRelation::Disjoint;
        }
        if self.contains(ax, ay) || self.contains(bx, by) || self.contains(cx, cy) {
            return GeoRelation::Crosses;
        }
        let mut within = false;
        for (p, q, from_shape) in [((ax, ay), (bx, by), t.ab), ((bx, by), (cx, cy), t.bc), ((cx, cy), (ax, ay), t.ca)] {
            if self.edge_intersects(p.0, p.1, q.0, q.1) {
                if from_shape {
                    return GeoRelation::Crosses;
                }
                within = true;
            }
        }
        if within || point_in_triangle(t_min_x, t_max_x, t_min_y, t_max_y, self.min_x, self.min_y, ax, ay, bx, by, cx, cy) {
            return GeoRelation::Inside;
        }
        GeoRelation::Disjoint
    }

    fn edge_intersects(&self, ax: i32, ay: i32, bx: i32, by: i32) -> bool {
        let (min_x, max_x, min_y, max_y) = (self.min_x, self.max_x, self.min_y, self.max_y);
        if ax.max(bx) < min_x || ax.min(bx) > max_x || ay.max(by) < min_y || ay.min(by) > max_y {
            return false;
        }
        let crosses = |p: (i32, i32), q: (i32, i32)| {
            iorient(ax, ay, bx, by, p.0, p.1) * iorient(ax, ay, bx, by, q.0, q.1) <= 0
                && iorient(p.0, p.1, q.0, q.1, ax, ay) * iorient(p.0, p.1, q.0, q.1, bx, by) <= 0
        };
        // top, right, bottom, left
        crosses((min_x, max_y), (max_x, max_y))
            || crosses((max_x, max_y), (max_x, min_y))
            || crosses((max_x, min_y), (min_x, min_y))
            || crosses((min_x, min_y), (min_x, max_y))
    }
}

/// `Tile2DVisitor.pointInTriangle` (on encoded coordinates, compared as doubles).
#[allow(clippy::too_many_arguments)]
fn point_in_triangle(min_x: i32, max_x: i32, min_y: i32, max_y: i32, x: i32, y: i32, ax: i32, ay: i32, bx: i32, by: i32, cx: i32, cy: i32) -> bool {
    if x >= min_x && x <= max_x && y >= min_y && y <= max_y {
        let a = iorient(x, y, ax, ay, bx, by);
        let b = iorient(x, y, bx, by, cx, cy);
        if a == 0 || b == 0 || (a < 0) == (b < 0) {
            let c = iorient(x, y, cx, cy, ax, ay);
            return c == 0 || ((c < 0) == (b < 0 || a < 0));
        }
        false
    } else {
        false
    }
}

/// A segment's shape doc values (and stored centroids).
pub struct ShapeReader {
    dv: Option<search_codec::BinaryDocValues>,
    centroids: Option<search_codec::BinaryDocValues>,
    /// `missing`: a doc value and its centroid for docs without a value.
    missing: Option<(Vec<u8>, [u8; 17])>,
    cur: ShapeValue,
}

impl ShapeReader {
    pub fn open(seg: &search_index::SegmentContext, field: Option<u32>, centroid: Option<u32>, coords: Coords) -> ShapeReader {
        ShapeReader {
            dv: field.and_then(|f| seg.reader.binary(f).cloned()),
            centroids: centroid.and_then(|f| seg.reader.binary(f).cloned()),
            missing: None,
            cur: ShapeValue::empty(coords),
        }
    }

    /// A segment reader for a bound values source (field, centroid field, `missing`).
    pub fn for_source(seg: &search_index::SegmentContext, vs: &crate::values::BoundVs) -> ShapeReader {
        use crate::field::SourceType;
        let (field, centroid, coords) = match vs.field.as_ref().map(|f| (f.number, f.source)) {
            Some((n, SourceType::GeoShape { centroid })) => (Some(n), centroid, Coords::Geo),
            Some((n, SourceType::CartesianShape { centroid })) => (Some(n), centroid, Coords::Cartesian),
            _ => (None, None, if vs.vs_type == crate::values::VsType::CartesianShape { Coords::Cartesian } else { Coords::Geo }),
        };
        let mut r = ShapeReader::open(seg, field, centroid, coords);
        if let Some(crate::values::MissingValue::Shape(dv, c)) = &vs.missing {
            r.missing = Some((dv.clone(), *c));
        }
        r
    }

    /// The doc's shape (`None` without a value).
    pub fn get(&mut self, doc: u32) -> Result<Option<&ShapeValue>> {
        if let Some(b) = self.dv.as_ref().and_then(|d| d.get(doc)) {
            let c = self.centroids.as_ref().and_then(|d| d.get(doc));
            return Ok(if self.cur.load(b, c)? { Some(&self.cur) } else { None });
        }
        match &self.missing {
            Some((dv, c)) => Ok(if self.cur.load(dv, Some(c))? { Some(&self.cur) } else { None }),
            None => Ok(None),
        }
    }
}

// ------------------------------------------------------------------ indexing helpers

/// The packed triangles of a geometry as Lucene's `LatLonShape` / `XYShape` index it: points,
/// one flat triangle per line segment, tessellated polygons (rectangles as polygons). No
/// dateline splitting or ring orientation fixes: that is `search-es`'s `GeoShapeIndexer`; this
/// serves `missing` values and tests.
pub fn shape_triangles(g: &Geometry, coords: Coords) -> Result<Vec<search_index::query::shape::PackedTriangle>> {
    use search_index::query::shape as sh;
    use search_index::util::tessellator::GeoPolygon;
    let mut out = Vec::new();
    let poly = |shell: &[(f64, f64)], holes: &[Vec<(f64, f64)>]| -> Result<GeoPolygon> {
        let ring = |r: &[(f64, f64)]| -> Result<GeoPolygon> {
            Ok(match coords {
                Coords::Geo => GeoPolygon::new(r.iter().map(|p| p.1).collect(), r.iter().map(|p| p.0).collect(), Vec::new())?,
                Coords::Cartesian => {
                    let xs: Vec<f32> = r.iter().map(|p| p.0 as f32).collect();
                    let ys: Vec<f32> = r.iter().map(|p| p.1 as f32).collect();
                    GeoPolygon::xy(&xs, &ys, Vec::new())?
                }
            })
        };
        let holes = holes.iter().map(|h| ring(h)).collect::<Result<Vec<_>>>()?;
        Ok(match coords {
            Coords::Geo => GeoPolygon::new(shell.iter().map(|p| p.1).collect(), shell.iter().map(|p| p.0).collect(), holes)?,
            Coords::Cartesian => {
                let xs: Vec<f32> = shell.iter().map(|p| p.0 as f32).collect();
                let ys: Vec<f32> = shell.iter().map(|p| p.1 as f32).collect();
                GeoPolygon::xy(&xs, &ys, holes)?
            }
        })
    };
    let point = |x: f64, y: f64| -> Result<Vec<sh::PackedTriangle>> {
        Ok(match coords {
            Coords::Geo => sh::point_triangles(y, x)?,
            Coords::Cartesian => sh::xy_point_triangles(x as f32, y as f32)?,
        })
    };
    let line = |pts: &[(f64, f64)]| -> Result<Vec<sh::PackedTriangle>> {
        Ok(match coords {
            Coords::Geo => sh::line_triangles(&pts.iter().map(|p| p.1).collect::<Vec<_>>(), &pts.iter().map(|p| p.0).collect::<Vec<_>>())?,
            Coords::Cartesian => {
                sh::xy_line_triangles(&pts.iter().map(|p| p.0 as f32).collect::<Vec<_>>(), &pts.iter().map(|p| p.1 as f32).collect::<Vec<_>>())?
            }
        })
    };
    match g {
        Geometry::Point(x, y) => out.extend(point(*x, *y)?),
        Geometry::MultiPoint(ps) => {
            for &(x, y) in ps {
                out.extend(point(x, y)?);
            }
        }
        Geometry::Line(ps) => out.extend(line(ps)?),
        Geometry::MultiLine(ls) => {
            for l in ls {
                out.extend(line(l)?);
            }
        }
        Geometry::Polygon(shell, holes) => out.extend(sh::polygon_triangles(&poly(shell, holes)?, false)?),
        Geometry::MultiPolygon(ps) => {
            for (shell, holes) in ps {
                out.extend(sh::polygon_triangles(&poly(shell, holes)?, false)?);
            }
        }
        Geometry::Rectangle { min_x, max_x, min_y, max_y } => {
            let r = [(*min_x, *min_y), (*max_x, *min_y), (*max_x, *max_y), (*min_x, *max_y), (*min_x, *min_y)];
            out.extend(sh::polygon_triangles(&poly(&r, &[])?, false)?);
        }
        Geometry::Collection(gs) => {
            for g in gs {
                out.extend(shape_triangles(g, coords)?);
            }
        }
        Geometry::Circle | Geometry::LinearRing => return Err(AggError::illegal_argument("invalid shape type")),
    }
    Ok(out)
}

/// `ShapeValues.missing(String)`: a WKT geometry as a doc value and its centroid.
pub fn parse_missing_shape(text: &str, geo: bool) -> Result<crate::values::MissingValue> {
    let coords = if geo { Coords::Geo } else { Coords::Cartesian };
    let g = parse_wkt(text)?;
    let wrap = |e: AggError| AggError::illegal_argument(format!("Can't apply missing value [{text}]")).caused_by(e);
    let triangles = shape_triangles(&g, coords).map_err(wrap)?;
    let mut calc = CentroidCalculator::new();
    calc.add(&g).map_err(wrap)?;
    let centroid = encode_centroid(&calc, coords).map_err(wrap)?;
    Ok(crate::values::MissingValue::Shape(search_index::query::shape::doc_value(&triangles).to_vec(), centroid))
}

/// Parses WKT (`WellKnownText.fromWKT`, coerced: unclosed polygon rings are closed): `POINT`,
/// `MULTIPOINT`, `LINESTRING`, `MULTILINESTRING`, `POLYGON`, `MULTIPOLYGON`,
/// `GEOMETRYCOLLECTION`, `BBOX` (`minX, maxX, maxY, minY`), with an optional Z ignored.
pub fn parse_wkt(text: &str) -> Result<Geometry> {
    let mut p = Wkt { toks: wkt_tokens(text), pos: 0 };
    let g = p.geometry()?;
    if p.pos < p.toks.len() {
        return Err(wkt_error(format!("expected end of WKT string but found additional text: {}", p.toks[p.pos])));
    }
    Ok(g)
}

fn wkt_error(msg: String) -> AggError {
    AggError::new(400, "parse_exception", msg)
}

fn wkt_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+') {
            cur.push(c);
        } else {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            if matches!(c, '(' | ')' | ',') {
                out.push(c.to_string());
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

struct Wkt {
    toks: Vec<String>,
    pos: usize,
}

impl Wkt {
    fn next(&mut self) -> Result<String> {
        let t = self.toks.get(self.pos).cloned().ok_or_else(|| wkt_error("expected word but found: END-OF-STREAM".into()))?;
        self.pos += 1;
        Ok(t)
    }

    fn peek(&self) -> Option<&str> {
        self.toks.get(self.pos).map(String::as_str)
    }

    fn expect(&mut self, t: &str) -> Result<()> {
        let got = self.next()?;
        if got != t {
            return Err(wkt_error(format!("expected {t} but found: {got}")));
        }
        Ok(())
    }

    fn number(&mut self) -> Result<f64> {
        let t = self.next()?;
        t.parse::<f64>().map_err(|_| wkt_error(format!("expected number but found: {t}")))
    }

    /// `x y [z]`.
    fn point(&mut self) -> Result<(f64, f64)> {
        let x = self.number()?;
        let y = self.number()?;
        if self.peek().is_some_and(|t| t != "," && t != ")") {
            self.number()?;
        }
        Ok((x, y))
    }

    /// `EMPTY` or `(`; `false` for `EMPTY`.
    fn open(&mut self) -> Result<bool> {
        let t = self.next()?;
        if t.eq_ignore_ascii_case("empty") {
            return Ok(false);
        }
        if t != "(" {
            return Err(wkt_error(format!("expected EMPTY or ( but found: {t}")));
        }
        Ok(true)
    }

    fn points(&mut self) -> Result<Vec<(f64, f64)>> {
        let mut pts = vec![self.point()?];
        while self.peek() == Some(",") {
            self.pos += 1;
            pts.push(self.point()?);
        }
        self.expect(")")?;
        Ok(pts)
    }

    fn ring(&mut self) -> Result<Vec<(f64, f64)>> {
        self.expect("(")?;
        let mut r = self.points()?;
        if r.first() != r.last() {
            let f = r[0];
            r.push(f);
        }
        Ok(r)
    }

    fn polygon_body(&mut self) -> Result<PolygonRings> {
        let shell = self.ring()?;
        let mut holes = Vec::new();
        while self.peek() == Some(",") {
            self.pos += 1;
            holes.push(self.ring()?);
        }
        self.expect(")")?;
        Ok((shell, holes))
    }

    fn geometry(&mut self) -> Result<Geometry> {
        let kind = self.next()?.to_lowercase();
        let empty = |k: &str| AggError::illegal_argument(format!("Unsupported empty {k}"));
        Ok(match kind.as_str() {
            "point" => {
                if !self.open()? {
                    return Err(empty("point"));
                }
                let p = self.point()?;
                self.expect(")")?;
                Geometry::Point(p.0, p.1)
            }
            "multipoint" => {
                if !self.open()? {
                    return Ok(Geometry::MultiPoint(Vec::new()));
                }
                let mut pts = Vec::new();
                loop {
                    if self.peek() == Some("(") {
                        self.pos += 1;
                        pts.push(self.point()?);
                        self.expect(")")?;
                    } else {
                        pts.push(self.point()?);
                    }
                    if self.peek() == Some(",") {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                self.expect(")")?;
                Geometry::MultiPoint(pts)
            }
            "linestring" => {
                if !self.open()? {
                    return Ok(Geometry::Line(Vec::new()));
                }
                Geometry::Line(self.points()?)
            }
            "multilinestring" => {
                if !self.open()? {
                    return Ok(Geometry::MultiLine(Vec::new()));
                }
                let mut ls = Vec::new();
                loop {
                    self.expect("(")?;
                    ls.push(self.points()?);
                    if self.peek() == Some(",") {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                self.expect(")")?;
                Geometry::MultiLine(ls)
            }
            "polygon" => {
                if !self.open()? {
                    return Err(empty("polygon"));
                }
                let (shell, holes) = self.polygon_body()?;
                Geometry::Polygon(shell, holes)
            }
            "multipolygon" => {
                if !self.open()? {
                    return Ok(Geometry::MultiPolygon(Vec::new()));
                }
                let mut ps = Vec::new();
                loop {
                    self.expect("(")?;
                    ps.push(self.polygon_body()?);
                    if self.peek() == Some(",") {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                self.expect(")")?;
                Geometry::MultiPolygon(ps)
            }
            "geometrycollection" => {
                if !self.open()? {
                    return Ok(Geometry::Collection(Vec::new()));
                }
                let mut gs = vec![self.geometry()?];
                while self.peek() == Some(",") {
                    self.pos += 1;
                    gs.push(self.geometry()?);
                }
                self.expect(")")?;
                Geometry::Collection(gs)
            }
            "bbox" | "envelope" => {
                if !self.open()? {
                    return Err(empty("bbox"));
                }
                let min_x = self.number()?;
                self.expect(",")?;
                let max_x = self.number()?;
                self.expect(",")?;
                let max_y = self.number()?;
                self.expect(",")?;
                let min_y = self.number()?;
                self.expect(")")?;
                Geometry::Rectangle { min_x, max_x, min_y, max_y }
            }
            other => return Err(AggError::illegal_argument(format!("Unknown geometry type: {other}"))),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use search_index::query::shape::{doc_value, line_triangles, point_triangles, polygon_triangles};
    use search_index::util::tessellator::GeoPolygon;

    fn shape(triangles: Vec<search_index::query::shape::PackedTriangle>) -> ShapeValue {
        let mut v = ShapeValue::empty(Coords::Geo);
        assert!(v.load(&doc_value(&triangles), None).unwrap());
        v
    }

    #[test]
    fn centroids_follow_elasticsearch_rules() {
        let mut c = CentroidCalculator::new();
        c.add(&Geometry::Point(1.0, 2.0)).unwrap();
        c.add(&Geometry::Line(vec![(0.0, 0.0), (4.0, 0.0)])).unwrap();
        assert_eq!((c.shape_type(), c.x(), c.y(), c.sum_weight()), (DimensionalShapeType::Line, 2.0, 0.0, 4.0));
        // A polygon outranks lines; its hole subtracts.
        c.add(&Geometry::Polygon(vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)], vec![vec![(0.0, 0.0), (5.0, 0.0), (5.0, 5.0), (0.0, 5.0), (0.0, 0.0)]])).unwrap();
        assert_eq!(c.shape_type(), DimensionalShapeType::Polygon);
        assert_eq!(c.sum_weight(), 75.0);
        assert!((c.x() - 70.0 / 12.0).abs() < 1e-12);
        // The original geometry, not the dateline-split one: lon 170..190 centers at 180.
        let mut d = CentroidCalculator::new();
        d.add(&Geometry::Rectangle { min_x: 170.0, max_x: 190.0, min_y: -1.0, max_y: 1.0 }).unwrap();
        assert_eq!(d.x(), 180.0);
        let enc = encode_centroid(&d, Coords::Geo).unwrap();
        let back = decode_centroid(&enc).unwrap();
        assert_eq!((back.x, back.shape_type, back.weight), (i32::MAX, DimensionalShapeType::Polygon, 40.0));
        assert!(CentroidCalculator::new().add(&Geometry::Circle).is_err());
        assert_eq!(normalize_lon(190.0), -170.0);
        assert_eq!(normalize_lon(-180.0), 180.0);
        assert_eq!(normalize_lat(95.0), 85.0);
    }

    #[test]
    fn extent_splits_at_the_prime_meridian() {
        let mut e = Extent::default();
        e.add_rectangle(-10, 0, -5, 1);
        e.add_rectangle(3, -2, 9, 4);
        assert_eq!((e.neg_left, e.neg_right, e.pos_left, e.pos_right, e.bottom, e.top), (-10, -5, 3, 9, -2, 4));
        e.add_rectangle(-1, 0, 1, 0);
        assert_eq!((e.neg_right, e.pos_left), (0, 0));
    }

    #[test]
    fn tile_relations() {
        let square = GeoPolygon::new(vec![0.0, 0.0, 10.0, 10.0, 0.0], vec![0.0, 10.0, 10.0, 0.0, 0.0], vec![]).unwrap();
        let v = shape(polygon_triangles(&square, false).unwrap());
        let enc = |lat0: f64, lat1: f64, lon0: f64, lon1: f64| (encode_longitude(lon0), encode_longitude(lon1), encode_latitude(lat0), encode_latitude(lat1));
        let r = |b: (i32, i32, i32, i32)| v.relate_tile(b.0, b.1, b.2, b.3);
        assert_eq!(r(enc(2.0, 3.0, 2.0, 3.0)), GeoRelation::Inside);
        assert_eq!(r(enc(-5.0, 20.0, -5.0, 20.0)), GeoRelation::Contains);
        assert_eq!(r(enc(5.0, 20.0, 5.0, 20.0)), GeoRelation::Crosses);
        assert_eq!(r(enc(20.0, 30.0, 20.0, 30.0)), GeoRelation::Disjoint);
        let bb = v.bounding_box();
        assert_eq!((bb.top, bb.bottom, bb.pos_left), (9.999999990686774, 0.0, 0.0));
        assert_eq!(bb.neg_left, f64::INFINITY);
        let line = shape(line_triangles(&[0.0, 5.0], &[0.0, 5.0]).unwrap());
        assert_eq!(line.relate_tile(encode_longitude(1.0), encode_longitude(2.0), encode_latitude(1.0), encode_latitude(2.0)), GeoRelation::Crosses);
        assert_eq!(line.relate_tile(encode_longitude(3.0), encode_longitude(4.0), encode_latitude(0.0), encode_latitude(1.0)), GeoRelation::Disjoint);
        let missing = parse_missing_shape("POLYGON ((0 0, 10 0, 10 10, 0 10, 0 0))", true).unwrap();
        let crate::values::MissingValue::Shape(dv, c) = missing else { panic!() };
        let mut m = ShapeValue::empty(Coords::Geo);
        assert!(m.load(&dv, Some(&c)).unwrap());
        assert_eq!(m.bounding_box(), bb);
        assert_eq!(m.centroid.shape_type, DimensionalShapeType::Polygon);
        assert_eq!(parse_missing_shape("1,1", true).unwrap_err().reason, "Unknown geometry type: 1");
        assert_eq!(parse_wkt("MULTIPOINT ((1 2), (3 4))").unwrap(), Geometry::MultiPoint(vec![(1.0, 2.0), (3.0, 4.0)]));
        assert_eq!(parse_wkt("BBOX (1, 2, 4, 3)").unwrap(), Geometry::Rectangle { min_x: 1.0, max_x: 2.0, min_y: 3.0, max_y: 4.0 });
        let pt = shape(point_triangles(1.0, 1.0).unwrap());
        assert_eq!(pt.centroid.shape_type, DimensionalShapeType::Point);
        assert_eq!(pt.centroid_xy(), (decode_longitude(encode_longitude(1.0)), decode_latitude(encode_latitude(1.0))));
    }
}
