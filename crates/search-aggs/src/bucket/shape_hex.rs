//! `geohex_grid` on `geo_shape` fields: Elasticsearch's `GeoHexGridTiler`, `GeoHexVisitor` and
//! `H3CartesianUtil`.
//!
//! H3 cells are spherical hexagons, but shapes are indexed in the equirectangular plane, so a
//! cell is related to a shape through its boundary drawn in that plane: the vertices quantized
//! to the index grid, closed, polar cells closed along the antimeridian up to the pole, cells
//! wider than 180° treated as crossing the dateline (longitudes shifted by ±360° when testing).
//! A shape whose bounding box falls in one cell is checked against that cell and its ring;
//! otherwise the tiler recurses from the base cells (or the ring of the one base cell holding
//! the box), descending into the children of every cell the shape touches — plus the finer
//! cells that overlap a cell without being its children, when their own parent missed the shape
//! — and taking every descendant of a cell's center child once a cell lies inside the shape.
//! Near the poles, coarser cells always count as crossing (the planar picture breaks down
//! there). With `bounds`, cells must intersect the box (inflated per resolution while
//! recursing).
//!
//! The relation of a cell to a shape folds the relations of the shape's triangles: crossing if
//! any crosses or if some but not all are contained in the cell; contained if all are; inside
//! if some triangle holds the cell and the others miss it; else disjoint. Elasticsearch walks a
//! k-d tree of triangles whose pruning only skips disjoint ones, so a flat fold is the same.

use search_codec::points::encode::{decode_latitude, decode_longitude, encode_latitude, encode_longitude};
use search_index::query::shape::TriangleType;
use search_index::util::geo2d::{line_crosses_line_with_boundary, point_in_triangle};
use search_index::util::tessellator::orient;

use crate::error::Result;
use crate::h3;
use crate::shape_field::{GeoRelation, ShapeValue, normalize_lon};

/// `H3CartesianUtil.MAX_ARRAY_SIZE`.
pub const MAX_ARRAY_SIZE: usize = 15;

/// `H3.northPolarH3` / `southPolarH3`.
fn polar(north: bool, res: i32) -> u64 {
    h3::geo_to_h3(if north { 90.0 } else { -90.0 }, 0.0, res)
}

/// `H3CartesianUtil.isPolar`.
pub fn is_polar(h: u64) -> bool {
    let res = h3::resolution(h);
    h == polar(true, res) || h == polar(false, res)
}

fn quantize_lon(lon: f64) -> f64 {
    decode_longitude(encode_longitude(lon))
}

fn quantize_lat(lat: f64) -> f64 {
    decode_latitude(encode_latitude(lat))
}

/// Sorts the first `n` vertices by longitude (`IntroSorter`, `Double.compare`).
struct ByX<'a> {
    xs: &'a mut [f64],
    ys: &'a mut [f64],
    pivot: usize,
}

impl crate::util::IntroSortable for ByX<'_> {
    fn swap(&mut self, i: usize, j: usize) {
        self.xs.swap(i, j);
        self.ys.swap(i, j);
    }
    fn set_pivot(&mut self, i: usize) {
        self.pivot = i;
    }
    fn compare_pivot(&self, j: usize) -> std::cmp::Ordering {
        self.xs[self.pivot].total_cmp(&self.xs[j])
    }
}

type Ring = ([f64; MAX_ARRAY_SIZE], [f64; MAX_ARRAY_SIZE], usize);

/// Resolution 0 and 1 cells and the polar cells, computed once (`CACHED_H3`): every
/// recursion starts with them.
fn cached(h: u64) -> Option<&'static Ring> {
    use std::sync::OnceLock;
    static C: OnceLock<hashbrown::HashMap<u64, Ring>> = OnceLock::new();
    C.get_or_init(|| {
        let mut m = hashbrown::HashMap::new();
        let mut put = |h: u64| {
            let (mut xs, mut ys) = ([0.0; MAX_ARRAY_SIZE], [0.0; MAX_ARRAY_SIZE]);
            let n = compute_points_uncached(h, &mut xs, &mut ys);
            m.insert(h, (xs, ys, n));
        };
        for c in h3::ring::res0_cells() {
            put(c);
            for k in 0..h3::ring::children_size(c) {
                put(h3::ring::child(c, k));
            }
        }
        for res in 2..=h3::MAX_RES {
            put(polar(true, res));
            put(polar(false, res));
        }
        m
    })
    .get(&h)
}

/// `H3CartesianUtil.computePoints`: the cell's planar ring (closed), its vertex count.
pub fn compute_points(h: u64, xs: &mut [f64; MAX_ARRAY_SIZE], ys: &mut [f64; MAX_ARRAY_SIZE]) -> usize {
    if let Some((cx, cy, n)) = cached(h) {
        *xs = *cx;
        *ys = *cy;
        return *n;
    }
    compute_points_uncached(h, xs, ys)
}

fn compute_points_uncached(h: u64, xs: &mut [f64; MAX_ARRAY_SIZE], ys: &mut [f64; MAX_ARRAY_SIZE]) -> usize {
    let boundary = h3::h3_to_geo_boundary(h);
    let nb = boundary.points.len();
    for (i, p) in boundary.points.iter().enumerate() {
        xs[i] = quantize_lon(p.lon.to_degrees());
        ys[i] = quantize_lat(p.lat.to_degrees());
    }
    let res = h3::resolution(h);
    let north = h == polar(true, res);
    let south = h == polar(false, res);
    if !north && !south {
        xs[nb] = xs[0];
        ys[nb] = ys[0];
        return nb + 1;
    }
    let n = nb + 5;
    let pole = quantize_lat(if north { 90.0 } else { -90.0 });
    crate::util::intro_sort(&mut ByX { xs: &mut xs[..], ys: &mut ys[..], pivot: 0 }, 0, nb);
    let pos = |lon: f64| if lon < 0.0 { lon + 360.0 } else { lon };
    let t = (180.0 - pos(xs[0])) / (pos(xs[nb - 1]) - pos(xs[0]));
    let y = ys[0] + t * (ys[nb - 1] - ys[0]);
    let (max_lon, min_lon) = (decode_longitude(i32::MAX), decode_longitude(i32::MIN));
    xs[nb] = max_lon;
    ys[nb] = y;
    xs[nb + 1] = max_lon;
    ys[nb + 1] = pole;
    xs[nb + 2] = min_lon;
    ys[nb + 2] = pole;
    xs[nb + 3] = min_lon;
    ys[nb + 3] = y;
    xs[nb + 4] = xs[0];
    ys[nb + 4] = ys[0];
    if south {
        xs[..n].reverse();
        ys[..n].reverse();
    }
    n
}

/// `H3CartesianUtil.toBoundingBox`: `(min_x, max_x, max_y, min_y)` of the quantized vertices;
/// polar cells span every longitude, cells wider than 180° wrap (`min_x > max_x`).
pub fn to_bounding_box(h: u64) -> (f64, f64, f64, f64) {
    let boundary = h3::h3_to_geo_boundary(h);
    let (mut min_lat, mut min_lon, mut max_lat, mut max_lon) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    let (mut max_neg_lon, mut min_pos_lon) = (f64::NEG_INFINITY, f64::INFINITY);
    use crate::metrics::simple::{java_max as max, java_min as min};
    for p in &boundary.points {
        let lon = quantize_lon(p.lon.to_degrees());
        let lat = quantize_lat(p.lat.to_degrees());
        min_lat = min(min_lat, lat);
        min_lon = min(min_lon, lon);
        max_lat = max(max_lat, lat);
        max_lon = max(max_lon, lon);
        if lon < 0.0 {
            max_neg_lon = max(max_neg_lon, lon);
        } else {
            min_pos_lon = min(min_pos_lon, lon);
        }
    }
    let res = h3::resolution(h);
    if h == polar(true, res) {
        (-180.0, 180.0, 90.0, min_lat)
    } else if h == polar(false, res) {
        (-180.0, 180.0, max_lat, -90.0)
    } else if max_lon - min_lon > 180.0 {
        (min_pos_lon, max_neg_lon, max_lat, min_lat)
    } else {
        (min_lon, max_lon, max_lat, min_lat)
    }
}

/// `getNorthPolarBound` / `getSouthPolarBound`: the latitude where the polar cell starts.
fn polar_bound(north: bool, res: i32) -> f64 {
    use std::sync::OnceLock;
    static B: OnceLock<[[f64; 16]; 2]> = OnceLock::new();
    let b = B.get_or_init(|| {
        let mut b = [[0.0; 16]; 2];
        for res in 0..=h3::MAX_RES {
            b[0][res as usize] = to_bounding_box(polar(true, res)).3;
            b[1][res as usize] = to_bounding_box(polar(false, res)).2;
        }
        b
    });
    b[usize::from(!north)][res as usize]
}

#[inline]
fn norm(lon: f64, mode: i8) -> f64 {
    match mode {
        1 if lon < 0.0 => lon + 360.0,
        -1 if lon > 0.0 => lon - 360.0,
        _ => lon,
    }
}

/// `GeoHexVisitor`: one H3 cell in the plane.
pub struct HexCell {
    xs: [f64; MAX_ARRAY_SIZE],
    ys: [f64; MAX_ARRAY_SIZE],
    n: usize,
    min_x: f64,
    max_x: f64,
    min_y: f64,
    max_y: f64,
    crosses_dateline: bool,
}

impl HexCell {
    /// `reset(h3)`.
    pub fn new(h: u64) -> HexCell {
        let mut xs = [0.0; MAX_ARRAY_SIZE];
        let mut ys = [0.0; MAX_ARRAY_SIZE];
        let n = compute_points(h, &mut xs, &mut ys);
        use crate::metrics::simple::{java_max as max, java_min as min};
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY);
        for i in 0..n {
            min_x = min(min_x, xs[i]);
            max_x = max(max_x, xs[i]);
            min_y = min(min_y, ys[i]);
            max_y = max(max_y, ys[i]);
        }
        let crosses_dateline = max_x - min_x > 180.0 && !is_polar(h);
        HexCell { xs, ys, n, min_x, max_x, min_y, max_y, crosses_dateline }
    }

    pub fn min_y(&self) -> f64 {
        self.min_y
    }

    pub fn max_y(&self) -> f64 {
        self.max_y
    }

    fn boxes_disjoint(&self, min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> bool {
        if !(max_y < self.min_y || min_y > self.max_y) {
            if self.crosses_dateline { max_x < self.min_x && min_x > self.max_x } else { max_x < self.min_x || min_x > self.max_x }
        } else {
            true
        }
    }

    /// `H3CartesianUtil.relatePoint` (after the box check).
    fn relate_point(&self, x: f64, y: f64) -> GeoRelation {
        if self.boxes_disjoint(x, x, y, y) {
            return GeoRelation::Disjoint;
        }
        let mode = if self.crosses_dateline { 1 } else { 0 };
        let x = norm(x, mode);
        let mut inside = false;
        for i in 0..self.n - 1 {
            let (x1, x2) = (norm(self.xs[i], mode), norm(self.xs[i + 1], mode));
            let (y1, y2) = (self.ys[i], self.ys[i + 1]);
            if y == y1 && y == y2 || (y <= y1 && y >= y2) != (y >= y1 && y <= y2) {
                if (x == x1 && x == x2) || ((x <= x1 && x >= x2) != (x >= x1 && x <= x2) && orient(x1, y1, x2, y2, x, y) == 0) {
                    return GeoRelation::Crosses;
                } else if (y1 > y) != (y2 > y) {
                    inside ^= x < (x2 - x1) * (y - y1) / (y2 - y1) + x1;
                }
            }
        }
        if inside { GeoRelation::Contains } else { GeoRelation::Disjoint }
    }

    fn modes(&self) -> &'static [i8] {
        if self.crosses_dateline { &[1, -1] } else { &[0] }
    }

    /// `H3CartesianUtil.crossesLine` (boundary included).
    #[allow(clippy::too_many_arguments)]
    fn crosses_line(&self, min_x: f64, max_x: f64, min_y: f64, max_y: f64, ax: f64, ay: f64, bx: f64, by: f64) -> bool {
        for &m in self.modes() {
            for i in 0..self.n - 1 {
                let (cy, dy) = (self.ys[i], self.ys[i + 1]);
                let (cx, dx) = (norm(self.xs[i], m), norm(self.xs[i + 1], m));
                if cx.max(dx) < min_x || cx.min(dx) > max_x || cy.min(dy) > max_y || cy.max(dy) < min_y {
                    continue;
                }
                if line_crosses_line_with_boundary(cx, cy, dx, dy, ax, ay, bx, by) {
                    return true;
                }
            }
        }
        false
    }

    /// `H3CartesianUtil.crossesTriangle` (boundary included).
    #[allow(clippy::too_many_arguments)]
    fn crosses_triangle(&self, min_x: f64, max_x: f64, min_y: f64, max_y: f64, ax: f64, ay: f64, bx: f64, by: f64, cx: f64, cy: f64) -> bool {
        for &m in self.modes() {
            for i in 0..self.n - 1 {
                let (dy, ey) = (self.ys[i], self.ys[i + 1]);
                let (dx, ex) = (norm(self.xs[i], m), norm(self.xs[i + 1], m));
                let outside = (dy < min_y && ey < min_y) || (dy > max_y && ey > max_y) || (dx < min_x && ex < min_x) || (dx > max_x && ex > max_x);
                if !outside
                    && (line_crosses_line_with_boundary(dx, dy, ex, ey, ax, ay, bx, by)
                        || line_crosses_line_with_boundary(dx, dy, ex, ey, bx, by, cx, cy)
                        || line_crosses_line_with_boundary(dx, dy, ex, ey, cx, cy, ax, ay))
                {
                    return true;
                }
            }
        }
        false
    }

    /// `H3CartesianUtil.crossesBox` (boundary included).
    fn crosses_box(&self, min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> bool {
        for &m in self.modes() {
            for i in 0..self.n - 1 {
                let (cy, dy) = (self.ys[i], self.ys[i + 1]);
                let (cx, dx) = (norm(self.xs[i], m), norm(self.xs[i + 1], m));
                let inb = |y: f64, x: f64| y >= min_y && y <= max_y && x >= min_x && x <= max_x;
                if inb(cy, cx) || inb(dy, dx) {
                    return true;
                }
                let outside = (cy < min_y && dy < min_y) || (cy > max_y && dy > max_y) || (cx < min_x && dx < min_x) || (cx > max_x && dx > max_x);
                if !outside
                    && (line_crosses_line_with_boundary(cx, cy, dx, dy, min_x, min_y, max_x, min_y)
                        || line_crosses_line_with_boundary(cx, cy, dx, dy, max_x, min_y, max_x, max_y)
                        || line_crosses_line_with_boundary(cx, cy, dx, dy, max_x, max_y, min_x, max_y)
                        || line_crosses_line_with_boundary(cx, cy, dx, dy, min_x, max_y, min_x, min_y))
                {
                    return true;
                }
            }
        }
        false
    }

    fn edge_intersects(&self, ax: f64, ay: f64, bx: f64, by: f64) -> bool {
        let (min_x, max_x, min_y, max_y) = (ax.min(bx), ax.max(bx), ay.min(by), ay.max(by));
        !self.boxes_disjoint(min_x, max_x, min_y, max_y) && self.crosses_line(min_x, max_x, min_y, max_y, ax, ay, bx, by)
    }

    fn relate_line(&self, ax: f64, ay: f64, bx: f64, by: f64) -> GeoRelation {
        let r1 = self.relate_point(ax, ay);
        let r2 = self.relate_point(bx, by);
        if r1 != r2 || r1 == GeoRelation::Crosses {
            return GeoRelation::Crosses;
        }
        if r1 == GeoRelation::Contains {
            if self.crosses_dateline {
                let (min_x, max_x, min_y, max_y) = (ax.min(bx), ax.max(bx), ay.min(by), ay.max(by));
                if self.crosses_line(min_x, max_x, min_y, max_y, ax, ay, bx, by) {
                    return GeoRelation::Crosses;
                }
            }
            return GeoRelation::Contains;
        }
        if self.edge_intersects(ax, ay, bx, by) { GeoRelation::Crosses } else { GeoRelation::Disjoint }
    }

    #[allow(clippy::too_many_arguments)]
    fn relate_triangle(&self, ax: f64, ay: f64, ab: bool, bx: f64, by: f64, bc: bool, cx: f64, cy: f64, ca: bool) -> GeoRelation {
        let (t_min_x, t_max_x) = (ax.min(bx).min(cx), ax.max(bx).max(cx));
        let (t_min_y, t_max_y) = (ay.min(by).min(cy), ay.max(by).max(cy));
        if self.boxes_disjoint(t_min_x, t_max_x, t_min_y, t_max_y) {
            return GeoRelation::Disjoint;
        }
        let r1 = self.relate_point(ax, ay);
        let r2 = self.relate_point(bx, by);
        let r3 = self.relate_point(cx, cy);
        if r1 != r2 || r1 != r3 || r1 == GeoRelation::Crosses {
            return GeoRelation::Crosses;
        }
        if r1 == GeoRelation::Contains {
            if self.crosses_dateline && self.crosses_triangle(t_min_x, t_max_x, t_min_y, t_max_y, ax, ay, bx, by, cx, cy) {
                return GeoRelation::Crosses;
            }
            return GeoRelation::Contains;
        }
        let mut within = false;
        for (p, q, from_shape) in [((ax, ay), (bx, by), ab), ((bx, by), (cx, cy), bc), ((cx, cy), (ax, ay), ca)] {
            if self.edge_intersects(p.0, p.1, q.0, q.1) {
                if from_shape {
                    return GeoRelation::Crosses;
                }
                within = true;
            }
        }
        if within || point_in_triangle(t_min_x, t_max_x, t_min_y, t_max_y, self.xs[0], self.ys[0], ax, ay, bx, by, cx, cy) {
            return GeoRelation::Inside;
        }
        GeoRelation::Disjoint
    }

    /// `geoValue.visit(visitor); visitor.relation()`.
    pub fn relate(&self, v: &ShapeValue) -> GeoRelation {
        let bb = v.bounding_box();
        let (min_x, max_x) = (v.coords.decode_x(v.extent.min_x()), v.coords.decode_x(v.extent.max_x()));
        if self.boxes_disjoint(min_x, max_x, bb.bottom, bb.top) {
            return GeoRelation::Disjoint;
        }
        let (mut any_contains, mut any_other, mut any_inside) = (false, false, false);
        for t in &v.triangles {
            let c = v.coords;
            let (ax, ay) = (c.decode_x(t.a_x), c.decode_y(t.a_y));
            let r = match t.kind {
                TriangleType::Point => self.relate_point(ax, ay),
                TriangleType::Line => self.relate_line(ax, ay, c.decode_x(t.b_x), c.decode_y(t.b_y)),
                TriangleType::Triangle => {
                    self.relate_triangle(ax, ay, t.ab, c.decode_x(t.b_x), c.decode_y(t.b_y), t.bc, c.decode_x(t.c_x), c.decode_y(t.c_y), t.ca)
                }
            };
            match r {
                GeoRelation::Crosses => return GeoRelation::Crosses,
                GeoRelation::Contains => any_contains = true,
                GeoRelation::Inside => {
                    any_inside = true;
                    any_other = true;
                }
                GeoRelation::Disjoint => any_other = true,
            }
            if any_contains && any_other {
                return GeoRelation::Crosses;
            }
        }
        if any_contains {
            GeoRelation::Contains
        } else if any_inside {
            GeoRelation::Inside
        } else {
            GeoRelation::Disjoint
        }
    }

    /// `intersectsBbox`: the cell meets the box (wrapping when `min_x > max_x`).
    pub fn intersects_bbox(&self, min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> bool {
        if min_x > max_x {
            self.relate_bbox(min_x, 180.0, min_y, max_y) || self.relate_bbox(-180.0, max_x, min_y, max_y)
        } else {
            self.relate_bbox(min_x, max_x, min_y, max_y)
        }
    }

    fn relate_bbox(&self, min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> bool {
        if self.boxes_disjoint(min_x, max_x, min_y, max_y) {
            return false;
        }
        if min_x <= self.xs[0] && max_x >= self.xs[0] && min_y <= self.ys[0] && max_y >= self.ys[0] {
            return true;
        }
        self.relate_point(min_x, min_y) != GeoRelation::Disjoint || self.crosses_box(min_x, max_x, min_y, max_y)
    }
}

/// A `geohex_grid` `bounds` box, with the per-resolution inflated boxes used while recursing.
pub struct HexBounds {
    top: f64,
    left: f64,
    bottom: f64,
    right: f64,
    inflated: Vec<(f64, f64, f64, f64)>,
}

const INFLATE_FACTOR: f64 = 0.37;

impl HexBounds {
    /// `BoundedGeoHexGridTiler(resolution, bbox)`.
    pub fn new(resolution: i32, top: f64, left: f64, bottom: f64, right: f64) -> HexBounds {
        let inflated = (0..resolution).map(|r| inflate(r, top, left, bottom, right, INFLATE_FACTOR)).collect();
        HexBounds { top, left, bottom, right, inflated }
    }
}

/// `inflateBbox`: `(top, left, bottom, right)`.
fn inflate(res: i32, top: f64, left: f64, bottom: f64, right: f64, factor: f64) -> (f64, f64, f64, f64) {
    let min_min = to_bounding_box(h3::geo_to_h3(bottom, left, res));
    let max_max = to_bounding_box(h3::geo_to_h3(top, right, res));
    let height = |r: (f64, f64, f64, f64)| r.2 - r.3;
    let width = |r: (f64, f64, f64, f64)| if r.0 > r.1 { 360.0 + r.1 - r.0 } else { r.1 - r.0 };
    let h = crate::metrics::simple::java_max(height(min_min), height(max_max));
    let w = crate::metrics::simple::java_max(width(min_min), width(max_max));
    let min_y = (bottom - factor * h).max(-90.0);
    let max_y = (top + factor * h).min(90.0);
    let l = normalize_lon(left - factor * w);
    let r = normalize_lon(right + factor * w);
    let bbox_width = if left > right { 360.0 + right - left } else { right - left };
    if 2.0 * factor * w + bbox_width >= 360.0 { (max_y, -180.0, min_y, 180.0) } else { (max_y, l, min_y, r) }
}

/// `GeoHexGridTiler`.
pub struct HexTiler<'a> {
    pub precision: i32,
    pub bounds: Option<&'a HexBounds>,
}

impl HexTiler<'_> {
    /// `relateTile`.
    fn relate(&self, v: &ShapeValue, h: u64) -> GeoRelation {
        let cell = HexCell::new(h);
        let res = h3::resolution(h);
        let near_pole = || cell.max_y() > polar_bound(true, res) || cell.min_y() < polar_bound(false, res);
        match self.bounds {
            None => {
                if res != self.precision && near_pole() {
                    return GeoRelation::Crosses;
                }
                cell.relate(v)
            }
            Some(b) => {
                let (top, left, bottom, right) = if res != self.precision { b.inflated[res as usize] } else { (b.top, b.left, b.bottom, b.right) };
                if !cell.intersects_bbox(left, right, bottom, top) {
                    return GeoRelation::Disjoint;
                }
                if res != self.precision && near_pole() {
                    return GeoRelation::Crosses;
                }
                cell.relate(v)
            }
        }
    }

    /// `h3IntersectsBounds`.
    fn intersects_bounds(&self, h: u64) -> bool {
        let Some(b) = self.bounds else { return true };
        let res = h3::resolution(h);
        let cell = HexCell::new(h);
        let (top, left, bottom, right) = if res != self.precision { b.inflated[res as usize] } else { (b.top, b.left, b.bottom, b.right) };
        cell.intersects_bbox(left, right, bottom, top)
    }

    /// `valueInsideBounds`.
    fn value_inside_bounds(&self, v: &ShapeValue) -> bool {
        let Some(b) = self.bounds else { return true };
        let bb = v.bounding_box();
        if b.bottom <= bb.min_y() && b.top >= bb.max_y() {
            if b.right < b.left { b.left <= bb.min_x() || b.right >= bb.max_x() } else { b.left <= bb.min_x() && b.right >= bb.max_x() }
        } else {
            false
        }
    }

    fn maybe_add(h: u64, rel: GeoRelation, out: &mut Vec<i64>) {
        if rel != GeoRelation::Disjoint {
            out.push(h as i64);
        }
    }

    /// `boundsInSameCell`.
    fn same_cell(v: &ShapeValue, res: i32) -> Option<u64> {
        let b = v.bounding_box();
        let min = h3::geo_to_h3(b.min_y(), b.min_x(), res);
        let max = h3::geo_to_h3(b.max_y(), b.max_x(), res);
        if min != max {
            return None;
        }
        if is_polar(min) && (h3::geo_to_h3(b.min_y(), b.max_x(), res) != min || h3::geo_to_h3(b.max_y(), b.min_x(), res) != min) {
            return None;
        }
        Some(min)
    }

    /// `setValues`: the doc's cells (unsorted, duplicates possible).
    pub fn cells(&self, v: &ShapeValue, out: &mut Vec<i64>) -> Result<()> {
        out.clear();
        let b = v.bounding_box();
        if b.max_x() - b.min_x() < 180.0
            && let Some(cell) = Self::same_cell(v, self.precision)
        {
            let rel = self.relate(v, cell);
            Self::maybe_add(cell, rel, out);
            if rel == GeoRelation::Contains {
                return Ok(());
            }
            for i in 0..h3::ring::ring_size(cell) {
                let n = h3::ring::ring(cell, i);
                let rel = self.relate(v, n);
                Self::maybe_add(n, rel, out);
                if rel == GeoRelation::Contains {
                    return Ok(());
                }
            }
            return Ok(());
        }
        if b.max_x() - b.min_x() < 180.0
            && let Some(cell) = Self::same_cell(v, 0)
        {
            self.recurse(v, cell, 0, out);
            for i in 0..h3::ring::ring_size(cell) {
                self.recurse(v, h3::ring::ring(cell, i), 0, out);
            }
            return Ok(());
        }
        for cell in h3::ring::res0_cells() {
            self.recurse(v, cell, 0, out);
        }
        Ok(())
    }

    fn recurse(&self, v: &ShapeValue, h: u64, res: i32, out: &mut Vec<i64>) {
        let rel = self.relate(v, h);
        if res == self.precision {
            Self::maybe_add(h, rel, out);
            return;
        }
        if rel == GeoRelation::Disjoint {
            return;
        }
        let mut i = 0;
        if rel == GeoRelation::Inside {
            // Only the center child is known to lie inside its parent.
            let inside = self.value_inside_bounds(v);
            self.all_below(h3::ring::child(h, 0), res + 1, inside, out);
            i = 1;
        }
        for k in i..h3::ring::children_size(h) {
            self.recurse(v, h3::ring::child(h, k), res + 1, out);
        }
        // Finer cells overlapping this one without being its children: visit them when their
        // own parent missed the shape.
        for k in 0..h3::ring::no_children_size(h) {
            let nc = h3::ring::no_child_intersecting(h, k);
            if self.relate(v, h3::ring::parent(nc)) == GeoRelation::Disjoint {
                self.recurse(v, nc, res + 1, out);
            }
        }
    }

    /// `setAllValuesByRecursion`.
    fn all_below(&self, h: u64, res: i32, inside: bool, out: &mut Vec<i64>) {
        if inside || self.intersects_bounds(h) {
            if res == self.precision {
                out.push(h as i64);
            } else {
                for k in 0..h3::ring::children_size(h) {
                    self.all_below(h3::ring::child(h, k), res + 1, inside, out);
                }
            }
        }
    }
}
