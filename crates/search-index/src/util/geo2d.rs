//! Planar relations of query geometries with boxes, points, segments and triangles: Lucene's
//! `Component2D` family (`Point2D`, `Line2D`, `Polygon2D` with its `EdgeTree`,
//! `Rectangle2D`, `ComponentTree`), which `geo_shape` queries use on indexed triangles and BKD
//! cells.
//!
//! The rules are Lucene's exactly — boundary inclusion, the `EdgeTree` crossing-number test
//! with its on-edge shortcut, the polygon boundary point a triangle must contain, the
//! `WithinRelation` logic of `contains` queries — since they decide which shapes match at
//! edges and corners. Coordinates are `x = longitude`, `y = latitude` in degrees.

use search_codec::Relation;
use search_codec::points::encode::{decode_latitude, decode_longitude, encode_latitude, encode_latitude_ceil, encode_longitude, encode_longitude_ceil};

use crate::util::tessellator::{GeoPolygon, orient};

/// Lucene's `Component2D.WithinRelation`: whether a doc's triangle shows the query is within
/// the doc's shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WithinRelation {
    Candidate,
    NotWithin,
    Disjoint,
}

/// `GeoUtils.lineCrossesLineWithBoundary`.
#[allow(clippy::too_many_arguments)]
#[inline]
pub fn line_crosses_line_with_boundary(a1x: f64, a1y: f64, b1x: f64, b1y: f64, a2x: f64, a2y: f64, b2x: f64, b2y: f64) -> bool {
    orient(a2x, a2y, b2x, b2y, a1x, a1y) * orient(a2x, a2y, b2x, b2y, b1x, b1y) <= 0
        && orient(a1x, a1y, b1x, b1y, a2x, a2y) * orient(a1x, a1y, b1x, b1y, b2x, b2y) <= 0
}

#[allow(clippy::too_many_arguments)]
#[inline]
pub fn disjoint(min_x1: f64, max_x1: f64, min_y1: f64, max_y1: f64, min_x2: f64, max_x2: f64, min_y2: f64, max_y2: f64) -> bool {
    max_y1 < min_y2 || min_y1 > max_y2 || max_x1 < min_x2 || min_x1 > max_x2
}

/// Box 1 within box 2.
#[allow(clippy::too_many_arguments)]
#[inline]
pub fn within(min_x1: f64, max_x1: f64, min_y1: f64, max_y1: f64, min_x2: f64, max_x2: f64, min_y2: f64, max_y2: f64) -> bool {
    min_y2 <= min_y1 && max_y2 >= max_y1 && min_x2 <= min_x1 && max_x2 >= max_x1
}

#[inline]
pub fn contains_point(x: f64, y: f64, min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> bool {
    x >= min_x && x <= max_x && y >= min_y && y <= max_y
}

/// `Component2D.pointInTriangle` (boundary inclusive).
#[allow(clippy::too_many_arguments)]
pub fn point_in_triangle(
    min_x: f64,
    max_x: f64,
    min_y: f64,
    max_y: f64,
    x: f64,
    y: f64,
    ax: f64,
    ay: f64,
    bx: f64,
    by: f64,
    cx: f64,
    cy: f64,
) -> bool {
    if x >= min_x && x <= max_x && y >= min_y && y <= max_y {
        let a = orient(x, y, ax, ay, bx, by);
        let b = orient(x, y, bx, by, cx, cy);
        if a == 0 || b == 0 || (a < 0) == (b < 0) {
            let c = orient(x, y, cx, cy, ax, ay);
            return c == 0 || ((c < 0) == (b < 0 || a < 0));
        }
        false
    } else {
        false
    }
}

/// A box `(min_x, max_x, min_y, max_y)`.
pub type Bbox = (f64, f64, f64, f64);

fn bbox2(ax: f64, ay: f64, bx: f64, by: f64) -> Bbox {
    (ax.min(bx), ax.max(bx), ay.min(by), ay.max(by))
}

fn bbox3(ax: f64, ay: f64, bx: f64, by: f64, cx: f64, cy: f64) -> Bbox {
    (ax.min(bx).min(cx), ax.max(bx).max(cx), ay.min(by).min(cy), ay.max(by).max(cy))
}

// ------------------------------------------------------------------ EdgeTree

#[derive(Clone, Debug)]
struct Edge {
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    low: f64,
    max: f64,
    left: u32,
    right: u32,
}

const NONE: u32 = u32::MAX;
const FALSE: u8 = 0;
const TRUE: u8 = 1;
const ON_EDGE: u8 = 2;

/// Lucene's `EdgeTree`: a polygon's or line's edges in an interval tree on latitude.
#[derive(Clone, Debug)]
pub struct EdgeTree {
    e: Vec<Edge>,
    root: u32,
}

impl EdgeTree {
    /// `x`/`y` hold the vertices; edge `i` joins vertices `i` and `i + 1`.
    pub fn new(x: &[f64], y: &[f64]) -> EdgeTree {
        let mut e: Vec<Edge> = (1..x.len())
            .map(|i| {
                let (x1, y1, x2, y2) = (x[i - 1], y[i - 1], x[i], y[i]);
                Edge { x1, y1, x2, y2, low: y1.min(y2), max: y1.max(y2), left: NONE, right: NONE }
            })
            .collect();
        // Java's Arrays.sort (stable) with Double.compare on (low, max).
        e.sort_by(|a, b| a.low.total_cmp(&b.low).then(a.max.total_cmp(&b.max)));
        let n = e.len() as i64;
        let root = Self::build(&mut e, 0, n - 1);
        EdgeTree { e, root }
    }

    fn build(e: &mut [Edge], low: i64, high: i64) -> u32 {
        if low > high {
            return NONE;
        }
        let mid = ((low + high) as u64 >> 1) as i64;
        let l = Self::build(e, low, mid - 1);
        let r = Self::build(e, mid + 1, high);
        let m = mid as usize;
        e[m].left = l;
        e[m].right = r;
        if l != NONE {
            e[m].max = e[m].max.max(e[l as usize].max);
        }
        if r != NONE {
            e[m].max = e[m].max.max(e[r as usize].max);
        }
        mid as u32
    }

    /// The first vertex of the root edge (Lucene's `tree.x1, tree.y1`: a boundary point).
    pub fn root_point(&self) -> (f64, f64) {
        match self.e.get(self.root as usize) {
            Some(r) => (r.x1, r.y1),
            None => (f64::NAN, f64::NAN),
        }
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        self.root != NONE && self.pnpoly(self.root, x, y) > FALSE
    }

    fn pnpoly(&self, i: u32, x: f64, y: f64) -> u8 {
        let n = &self.e[i as usize];
        let mut res = FALSE;
        if y <= n.max {
            if (y == n.y1 && y == n.y2) || ((y <= n.y1 && y >= n.y2) != (y >= n.y1 && y <= n.y2)) {
                if (x == n.x1 && x == n.x2)
                    || ((x <= n.x1 && x >= n.x2) != (x >= n.x1 && x <= n.x2) && orient(n.x1, n.y1, n.x2, n.y2, x, y) == 0)
                {
                    return ON_EDGE;
                } else if (n.y1 > y) != (n.y2 > y) {
                    res = if x < (n.x2 - n.x1) * (y - n.y1) / (n.y2 - n.y1) + n.x1 { TRUE } else { FALSE };
                }
            }
            if n.left != NONE {
                res ^= self.pnpoly(n.left, x, y);
                if res & 0x02 == 0x02 {
                    return ON_EDGE;
                }
            }
            if n.right != NONE && y >= n.low {
                res ^= self.pnpoly(n.right, x, y);
                if res & 0x02 == 0x02 {
                    return ON_EDGE;
                }
            }
        }
        res
    }

    pub fn is_point_on_line(&self, x: f64, y: f64) -> bool {
        self.root != NONE && self.on_line(self.root, x, y)
    }

    fn on_line(&self, i: u32, x: f64, y: f64) -> bool {
        let n = &self.e[i as usize];
        if y <= n.max {
            let outside = (n.y1 < y && n.y2 < y) || (n.y1 > y && n.y2 > y) || (n.x1 < x && n.x2 < x) || (n.x1 > x && n.x2 > x);
            if !outside && orient(n.x1, n.y1, n.x2, n.y2, x, y) == 0 {
                return true;
            }
            if n.left != NONE && self.on_line(n.left, x, y) {
                return true;
            }
            if n.right != NONE && y >= n.low && self.on_line(n.right, x, y) {
                return true;
            }
        }
        false
    }

    #[allow(clippy::too_many_arguments)]
    pub fn crosses_triangle(&self, b: Bbox, ax: f64, ay: f64, bx: f64, by: f64, cx: f64, cy: f64, boundary: bool) -> bool {
        self.root != NONE && self.cross_tri(self.root, b, ax, ay, bx, by, cx, cy, boundary)
    }

    #[allow(clippy::too_many_arguments)]
    fn cross_tri(&self, i: u32, b: Bbox, ax: f64, ay: f64, bx: f64, by: f64, cx: f64, cy: f64, boundary: bool) -> bool {
        let (min_x, max_x, min_y, max_y) = b;
        let n = &self.e[i as usize];
        if min_y <= n.max {
            let (dy, ey, dx, ex) = (n.y1, n.y2, n.x1, n.x2);
            let outside = (dy < min_y && ey < min_y) || (dy > max_y && ey > max_y) || (dx < min_x && ex < min_x) || (dx > max_x && ex > max_x);
            if !outside {
                let f = if boundary { line_crosses_line_with_boundary } else { crate::util::tessellator::line_crosses_line };
                if f(dx, dy, ex, ey, ax, ay, bx, by) || f(dx, dy, ex, ey, bx, by, cx, cy) || f(dx, dy, ex, ey, cx, cy, ax, ay) {
                    return true;
                }
            }
            if n.left != NONE && self.cross_tri(n.left, b, ax, ay, bx, by, cx, cy, boundary) {
                return true;
            }
            if n.right != NONE && max_y >= n.low && self.cross_tri(n.right, b, ax, ay, bx, by, cx, cy, boundary) {
                return true;
            }
        }
        false
    }

    pub fn crosses_box(&self, b: Bbox, boundary: bool) -> bool {
        self.root != NONE && self.cross_box(self.root, b, boundary)
    }

    fn cross_box(&self, i: u32, b: Bbox, boundary: bool) -> bool {
        let (min_x, max_x, min_y, max_y) = b;
        let n = &self.e[i as usize];
        if min_y <= n.max {
            let (cy, dy, cx, dx) = (n.y1, n.y2, n.x1, n.x2);
            if contains_point(cx, cy, min_x, max_x, min_y, max_y) || contains_point(dx, dy, min_x, max_x, min_y, max_y) {
                return true;
            }
            let outside = (cy < min_y && dy < min_y) || (cy > max_y && dy > max_y) || (cx < min_x && dx < min_x) || (cx > max_x && dx > max_x);
            if !outside {
                let f = if boundary { line_crosses_line_with_boundary } else { crate::util::tessellator::line_crosses_line };
                if f(cx, cy, dx, dy, min_x, min_y, max_x, min_y)
                    || f(cx, cy, dx, dy, max_x, min_y, max_x, max_y)
                    || f(cx, cy, dx, dy, max_x, max_y, min_x, max_y)
                    || f(cx, cy, dx, dy, min_x, max_y, min_x, min_y)
                {
                    return true;
                }
            }
            if n.left != NONE && self.cross_box(n.left, b, boundary) {
                return true;
            }
            if n.right != NONE && max_y >= n.low && self.cross_box(n.right, b, boundary) {
                return true;
            }
        }
        false
    }

    pub fn crosses_line(&self, b: Bbox, ax: f64, ay: f64, bx: f64, by: f64, boundary: bool) -> bool {
        self.root != NONE && self.cross_line(self.root, b, ax, ay, bx, by, boundary)
    }

    #[allow(clippy::too_many_arguments)]
    fn cross_line(&self, i: u32, b: Bbox, a2x: f64, a2y: f64, b2x: f64, b2y: f64, boundary: bool) -> bool {
        let (min_x, max_x, min_y, max_y) = b;
        let n = &self.e[i as usize];
        if min_y <= n.max {
            let (a1x, a1y, b1x, b1y) = (n.x1, n.y1, n.x2, n.y2);
            let outside = (a1y < min_y && b1y < min_y) || (a1y > max_y && b1y > max_y) || (a1x < min_x && b1x < min_x) || (a1x > max_x && b1x > max_x);
            if !outside {
                let hit = if boundary {
                    line_crosses_line_with_boundary(a1x, a1y, b1x, b1y, a2x, a2y, b2x, b2y)
                } else {
                    crate::util::tessellator::line_crosses_line(a1x, a1y, b1x, b1y, a2x, a2y, b2x, b2y)
                };
                if hit {
                    return true;
                }
            }
            if n.left != NONE && self.cross_line(n.left, b, a2x, a2y, b2x, b2y, boundary) {
                return true;
            }
            if n.right != NONE && max_y >= n.low && self.cross_line(n.right, b, a2x, a2y, b2x, b2y, boundary) {
                return true;
            }
        }
        false
    }
}

// ------------------------------------------------------------------ components

/// A query geometry (Lucene's `Component2D`).
#[derive(Clone, Debug)]
pub enum Component {
    Point { x: f64, y: f64 },
    Line { bbox: Bbox, tree: EdgeTree },
    Polygon { bbox: Bbox, tree: EdgeTree, holes: Option<Box<Component>> },
    Rectangle { bbox: Bbox },
    /// Several components (Lucene's `ComponentTree`: any of them).
    Tree { bbox: Bbox, parts: Vec<Component> },
    /// A circle on the sphere or in the plane (Lucene's `Circle2D`).
    Circle(Box<Circle>),
    /// The complement of a component (Elasticsearch's `InverseXYGeometry`).
    Inverse(Box<Component>),
}

/// Lucene's `Circle2D` with its distance calculator: `HaversinDistance` (geo, radius in
/// meters) or `CartesianDistance` (`XYCircle`).
#[derive(Clone, Debug)]
pub enum Circle {
    Haversin(HaversinCircle),
    Cartesian(CartesianCircle),
}

/// Lucene's `Circle2D.HaversinDistance`.
#[derive(Clone, Debug)]
pub struct HaversinCircle {
    lat: f64,
    lon: f64,
    sort_key: f64,
    axis_lat: f64,
    /// `Rectangle.fromPointDistance`: (min_lat, max_lat, min_lon, max_lon); may cross the
    /// dateline.
    rect: (f64, f64, f64, f64),
    crosses: bool,
}

/// Lucene's `Circle2D.CartesianDistance`: the bounding box is `XYRectangle.fromPointDistance`
/// (float arithmetic, radius rounded up), distances are squared doubles.
#[derive(Clone, Debug)]
pub struct CartesianCircle {
    x: f64,
    y: f64,
    radius_sq: f64,
    /// (min_x, max_x, min_y, max_y).
    rect: Bbox,
}

/// `Rectangle.AXISLAT_ERROR`.
fn axislat_error() -> f64 {
    (0.1f64 / 6_371_008.771_4).to_degrees()
}

/// `Rectangle.axisLat`: the latitude where the circle touches its bounding box's meridians.
fn axis_lat(center_lat: f64, radius: f64) -> f64 {
    let pio2 = std::f64::consts::FRAC_PI_2;
    let mut l1 = center_lat.to_radians();
    let r = (radius + 7e-2) / 6_371_008.771_4;
    if l1.abs() + r >= 90f64.to_radians() {
        return if center_lat >= 0.0 { 90.0 } else { -90.0 };
    }
    l1 = if center_lat >= 0.0 { pio2 - l1 } else { l1 + pio2 };
    let mut l2 = (l1.cos() / r.cos()).acos();
    l2 = if center_lat >= 0.0 { pio2 - l2 } else { l2 - pio2 };
    l2.to_degrees()
}

/// `GeoUtils.within90LonDegrees`.
fn within_90_lon_degrees(mut lon: f64, min_lon: f64, max_lon: f64) -> bool {
    if max_lon <= lon - 180.0 {
        lon -= 360.0;
    } else if min_lon >= lon + 180.0 {
        lon += 360.0;
    }
    max_lon - lon < 90.0 && lon - min_lon < 90.0
}

impl HaversinCircle {
    fn contains(&self, x: f64, y: f64) -> bool {
        let (a, b, c, d) = self.rect;
        let inside = if self.crosses {
            contains_point(x, y, c, 180.0, a, b) || contains_point(x, y, -180.0, d, a, b)
        } else {
            contains_point(x, y, c, d, a, b)
        };
        inside && crate::util::geo::haversin_sort_key(y, x, self.lat, self.lon) <= self.sort_key
    }

    fn disjoint(&self, min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> bool {
        let (a, b, c, d) = self.rect;
        if self.crosses {
            disjoint(c, 180.0, a, b, min_x, max_x, min_y, max_y) && disjoint(-180.0, d, a, b, min_x, max_x, min_y, max_y)
        } else {
            disjoint(c, d, a, b, min_x, max_x, min_y, max_y)
        }
    }

    fn within(&self, min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> bool {
        let (a, b, c, d) = self.rect;
        if self.crosses {
            within(c, 180.0, a, b, min_x, max_x, min_y, max_y) || within(-180.0, d, a, b, min_x, max_x, min_y, max_y)
        } else {
            within(c, d, a, b, min_x, max_x, min_y, max_y)
        }
    }

    /// `GeoUtils.relate` for a cell (which never crosses the dateline).
    fn relate_cell(&self, min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> Relation {
        use crate::util::geo::haversin_sort_key as k;
        let (lat, lon, key) = (self.lat, self.lon, self.sort_key);
        let err = axislat_error();
        if (lon < min_x || lon > max_x)
            && (self.axis_lat + err < min_y || self.axis_lat - err > max_y)
            && k(lat, lon, min_y, min_x) > key
            && k(lat, lon, min_y, max_x) > key
            && k(lat, lon, max_y, min_x) > key
            && k(lat, lon, max_y, max_x) > key
        {
            return Relation::CellOutsideQuery;
        }
        if within_90_lon_degrees(lon, min_x, max_x)
            && k(lat, lon, min_y, min_x) <= key
            && k(lat, lon, min_y, max_x) <= key
            && k(lat, lon, max_y, min_x) <= key
            && k(lat, lon, max_y, max_x) <= key
        {
            return Relation::CellInsideQuery;
        }
        Relation::CellCrossesQuery
    }

    /// `Circle2D.intersectsLine` (the segment's closest point to the center, also from the
    /// center shifted by 360° when the circle crosses the dateline).
    fn intersects_line(&self, ax: f64, ay: f64, bx: f64, by: f64) -> bool {
        if self.closest_in(self.lon, ax, ay, bx, by) {
            return true;
        }
        if self.crosses {
            let shifted = if self.lon > 0.0 { self.lon - 360.0 } else { self.lon + 360.0 };
            return self.closest_in(shifted, ax, ay, bx, by);
        }
        false
    }

    fn closest_in(&self, cx: f64, ax: f64, ay: f64, bx: f64, by: f64) -> bool {
        closest_in(cx, self.lat, ax, ay, bx, by, |x, y| self.contains(x, y))
    }

    fn bbox(&self) -> Bbox {
        let (a, b, c, d) = self.rect;
        if self.crosses { (-180.0, 180.0, a, b) } else { (c, d, a, b) }
    }
}

impl CartesianCircle {
    fn contains(&self, x: f64, y: f64) -> bool {
        let (a, b, c, d) = self.rect;
        if contains_point(x, y, a, b, c, d) {
            let (dx, dy) = (x - self.x, y - self.y);
            return dx * dx + dy * dy <= self.radius_sq;
        }
        false
    }

    fn relate_cell(&self, min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> Relation {
        if contains_point(self.x, self.y, min_x, max_x, min_y, max_y) {
            if self.contains(min_x, min_y) && self.contains(max_x, min_y) && self.contains(max_x, max_y) && self.contains(min_x, max_y) {
                return Relation::CellInsideQuery;
            }
        } else {
            let mut sum = 0.0;
            if self.x < min_x {
                sum += (min_x - self.x) * (min_x - self.x);
            } else if self.x > max_x {
                sum += (max_x - self.x) * (max_x - self.x);
            }
            if self.y < min_y {
                sum += (min_y - self.y) * (min_y - self.y);
            } else if self.y > max_y {
                sum += (max_y - self.y) * (max_y - self.y);
            }
            if sum > self.radius_sq {
                return Relation::CellOutsideQuery;
            }
        }
        Relation::CellCrossesQuery
    }
}

impl Circle {
    fn center(&self) -> (f64, f64) {
        match self {
            Circle::Haversin(c) => (c.lon, c.lat),
            Circle::Cartesian(c) => (c.x, c.y),
        }
    }

    fn contains(&self, x: f64, y: f64) -> bool {
        match self {
            Circle::Haversin(c) => c.contains(x, y),
            Circle::Cartesian(c) => c.contains(x, y),
        }
    }

    fn disjoint(&self, min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> bool {
        match self {
            Circle::Haversin(c) => c.disjoint(min_x, max_x, min_y, max_y),
            Circle::Cartesian(c) => disjoint(c.rect.0, c.rect.1, c.rect.2, c.rect.3, min_x, max_x, min_y, max_y),
        }
    }

    fn within(&self, min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> bool {
        match self {
            Circle::Haversin(c) => c.within(min_x, max_x, min_y, max_y),
            Circle::Cartesian(c) => within(c.rect.0, c.rect.1, c.rect.2, c.rect.3, min_x, max_x, min_y, max_y),
        }
    }

    fn relate_cell(&self, min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> Relation {
        match self {
            Circle::Haversin(c) => c.relate_cell(min_x, max_x, min_y, max_y),
            Circle::Cartesian(c) => c.relate_cell(min_x, max_x, min_y, max_y),
        }
    }

    fn intersects_line(&self, ax: f64, ay: f64, bx: f64, by: f64) -> bool {
        match self {
            Circle::Haversin(c) => c.intersects_line(ax, ay, bx, by),
            Circle::Cartesian(c) => closest_in(c.x, c.y, ax, ay, bx, by, |x, y| c.contains(x, y)),
        }
    }

    fn bbox(&self) -> Bbox {
        match self {
            Circle::Haversin(c) => c.bbox(),
            Circle::Cartesian(c) => c.rect,
        }
    }
}

/// `Circle2D.intersectsLine`: the segment's point closest to the center, if it lies on the
/// segment, must be in the circle.
fn closest_in(cx: f64, cy: f64, ax: f64, ay: f64, bx: f64, by: f64, contains: impl Fn(f64, f64) -> bool) -> bool {
    let (apx, apy) = (cx - ax, cy - ay);
    let (abx, aby) = (bx - ax, by - ay);
    let mag = abx * abx + aby * aby;
    let dot = apx * abx + apy * aby;
    let t = dot / mag;
    if !(0.0..=1.0).contains(&t) {
        return false;
    }
    let (px, py) = (ax + abx * t, ay + aby * t);
    if px >= ax.min(bx) && px <= ax.max(bx) && py >= ay.min(by) && py <= ay.max(by) {
        return contains(px, py);
    }
    false
}

/// Lucene's `GeoEncodingUtils` quantization of query bounds.
const MIN_LON_QUANTIZED: f64 = -180.0;

fn max_lon_quantized() -> f64 {
    decode_longitude(i32::MAX)
}

impl Component {
    /// `Point2D.create`: the point moved up to the next encoded value (as indexed points
    /// round down, the query's ceiling is the matching cell).
    pub fn point(lat: f64, lon: f64) -> Component {
        let y = if lat == 90.0 { lat } else { decode_latitude(encode_latitude_ceil(lat)) };
        let x = if lon == 180.0 { lon } else { decode_longitude(encode_longitude_ceil(lon)) };
        Component::Point { x, y }
    }

    /// `Line2D.create` over `(lats, lons)`.
    pub fn line(lats: &[f64], lons: &[f64]) -> Component {
        let bbox = (fold_min(lons), fold_max(lons), fold_min(lats), fold_max(lats));
        Component::Line { bbox, tree: EdgeTree::new(lons, lats) }
    }

    /// `Polygon2D.create`.
    pub fn polygon(p: &GeoPolygon) -> Component {
        let holes = match p.holes.len() {
            0 => None,
            1 => Some(Box::new(Component::polygon(&p.holes[0]))),
            _ => Some(Box::new(Component::tree(p.holes.iter().map(Component::polygon).collect()))),
        };
        Component::Polygon { bbox: (p.min_lon, p.max_lon, p.min_lat, p.max_lat), tree: EdgeTree::new(&p.lons, &p.lats), holes }
    }

    /// `Rectangle2D.create` (quantized; split at the dateline).
    pub fn rectangle(min_lat: f64, max_lat: f64, min_lon: f64, max_lon: f64) -> Component {
        let mut min_lon = min_lon;
        let mut crosses = min_lon > max_lon;
        if min_lon == 180.0 && crosses {
            min_lon = -180.0;
            crosses = false;
        }
        let q_min_lat = decode_latitude(encode_latitude_ceil(min_lat));
        let q_max_lat = decode_latitude(encode_latitude(max_lat));
        let q_min_lon = decode_longitude(encode_longitude_ceil(min_lon));
        let q_max_lon = decode_longitude(encode_longitude(max_lon));
        if crosses {
            Component::tree(vec![
                Component::Rectangle { bbox: (MIN_LON_QUANTIZED, q_max_lon, q_min_lat, q_max_lat) },
                Component::Rectangle { bbox: (q_min_lon, max_lon_quantized(), q_min_lat, q_max_lat) },
            ])
        } else {
            Component::Rectangle { bbox: (q_min_lon, q_max_lon, q_min_lat, q_max_lat) }
        }
    }

    /// `Circle2D.create(new Circle(lat, lon, radius))` (radius in meters).
    pub fn circle(lat: f64, lon: f64, radius: f64) -> Component {
        let rect = crate::query::geo::bbox_of_circle(lat, lon, radius);
        Component::Circle(Box::new(Circle::Haversin(HaversinCircle {
            lat,
            lon,
            sort_key: crate::query::geo::distance_query_sort_key(radius),
            axis_lat: axis_lat(lat, radius),
            rect,
            crosses: rect.2 > rect.3,
        })))
    }

    /// `Point2D.create(XYPoint)`: no quantization (cartesian values are indexed exactly).
    pub fn xy_point(x: f32, y: f32) -> Component {
        Component::Point { x: x as f64, y: y as f64 }
    }

    /// `Line2D.create(XYLine)`.
    pub fn xy_line(xs: &[f32], ys: &[f32]) -> Component {
        let xs: Vec<f64> = xs.iter().map(|&v| v as f64).collect();
        let ys: Vec<f64> = ys.iter().map(|&v| v as f64).collect();
        Component::Line { bbox: (fold_min(&xs), fold_max(&xs), fold_min(&ys), fold_max(&ys)), tree: EdgeTree::new(&xs, &ys) }
    }

    /// `Rectangle2D.create(XYRectangle)`.
    pub fn xy_rectangle(min_x: f32, max_x: f32, min_y: f32, max_y: f32) -> Component {
        Component::Rectangle { bbox: (min_x as f64, max_x as f64, min_y as f64, max_y as f64) }
    }

    /// `Circle2D.create(XYCircle)`: the box is `XYRectangle.fromPointDistance` (float
    /// arithmetic, radius rounded up to the next float).
    pub fn xy_circle(x: f32, y: f32, radius: f32) -> Component {
        let r = radius.next_up();
        let rect = ((x - r).max(-f32::MAX) as f64, (x + r).min(f32::MAX) as f64, (y - r).max(-f32::MAX) as f64, (y + r).min(f32::MAX) as f64);
        Component::Circle(Box::new(Circle::Cartesian(CartesianCircle {
            x: x as f64,
            y: y as f64,
            radius_sq: radius as f64 * radius as f64,
            rect,
        })))
    }

    /// Elasticsearch's `InverseXYGeometry` (`XYQueriesUtils`): every point the components do
    /// not contain, over the whole float plane. Only point queries use it (`contains` and
    /// `relate`); the triangle relations are unsupported, as in Elasticsearch.
    pub fn inverse(inner: Component) -> Component {
        Component::Inverse(Box::new(inner))
    }

    /// `ComponentTree.create` (one component stays itself).
    pub fn tree(mut parts: Vec<Component>) -> Component {
        if parts.len() == 1 {
            return parts.pop().unwrap();
        }
        let mut bbox = (f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY);
        for p in &parts {
            let b = p.bbox();
            bbox = (bbox.0.min(b.0), bbox.1.max(b.1), bbox.2.min(b.2), bbox.3.max(b.3));
        }
        Component::Tree { bbox, parts }
    }

    pub fn bbox(&self) -> Bbox {
        match self {
            Component::Point { x, y } => (*x, *x, *y, *y),
            Component::Line { bbox, .. } | Component::Polygon { bbox, .. } | Component::Rectangle { bbox } | Component::Tree { bbox, .. } => *bbox,
            Component::Circle(c) => c.bbox(),
            Component::Inverse(_) => (-f32::MAX as f64, f32::MAX as f64, -f32::MAX as f64, f32::MAX as f64),
        }
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        match self {
            Component::Point { x: px, y: py } => x == *px && y == *py,
            Component::Line { bbox, tree } => contains_point(x, y, bbox.0, bbox.1, bbox.2, bbox.3) && tree.is_point_on_line(x, y),
            Component::Polygon { bbox, tree, holes } => {
                if contains_point(x, y, bbox.0, bbox.1, bbox.2, bbox.3) && tree.contains(x, y) {
                    return holes.as_ref().is_none_or(|h| !h.contains(x, y));
                }
                false
            }
            Component::Rectangle { bbox } => contains_point(x, y, bbox.0, bbox.1, bbox.2, bbox.3),
            Component::Tree { bbox, parts } => x <= bbox.1 && y <= bbox.3 && parts.iter().any(|p| p.contains(x, y)),
            Component::Circle(c) => c.contains(x, y),
            Component::Inverse(c) => !c.contains(x, y),
        }
    }

    /// Relation of the box with the query (`CellInsideQuery`: every point of the box is in
    /// the query).
    pub fn relate(&self, min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> Relation {
        match self {
            Component::Point { x, y } => {
                if contains_point(*x, *y, min_x, max_x, min_y, max_y) {
                    Relation::CellCrossesQuery
                } else {
                    Relation::CellOutsideQuery
                }
            }
            Component::Line { bbox, tree } => {
                if disjoint(bbox.0, bbox.1, bbox.2, bbox.3, min_x, max_x, min_y, max_y) {
                    return Relation::CellOutsideQuery;
                }
                if within(bbox.0, bbox.1, bbox.2, bbox.3, min_x, max_x, min_y, max_y) {
                    return Relation::CellCrossesQuery;
                }
                if tree.crosses_box((min_x, max_x, min_y, max_y), true) {
                    return Relation::CellCrossesQuery;
                }
                Relation::CellOutsideQuery
            }
            Component::Polygon { bbox, tree, holes } => {
                if disjoint(bbox.0, bbox.1, bbox.2, bbox.3, min_x, max_x, min_y, max_y) {
                    return Relation::CellOutsideQuery;
                }
                if within(bbox.0, bbox.1, bbox.2, bbox.3, min_x, max_x, min_y, max_y) {
                    return Relation::CellCrossesQuery;
                }
                if let Some(h) = holes {
                    match h.relate(min_x, max_x, min_y, max_y) {
                        Relation::CellCrossesQuery => return Relation::CellCrossesQuery,
                        Relation::CellInsideQuery => return Relation::CellOutsideQuery,
                        Relation::CellOutsideQuery => {}
                    }
                }
                let corners = self.corners(min_x, max_x, min_y, max_y);
                if corners == 4 {
                    if tree.crosses_box((min_x, max_x, min_y, max_y), true) {
                        return Relation::CellCrossesQuery;
                    }
                    Relation::CellInsideQuery
                } else if corners == 0 {
                    let (rx, ry) = tree.root_point();
                    if contains_point(rx, ry, min_x, max_x, min_y, max_y) {
                        return Relation::CellCrossesQuery;
                    }
                    if tree.crosses_box((min_x, max_x, min_y, max_y), true) {
                        return Relation::CellCrossesQuery;
                    }
                    Relation::CellOutsideQuery
                } else {
                    Relation::CellCrossesQuery
                }
            }
            Component::Rectangle { bbox } => {
                if disjoint(bbox.0, bbox.1, bbox.2, bbox.3, min_x, max_x, min_y, max_y) {
                    return Relation::CellOutsideQuery;
                }
                if within(min_x, max_x, min_y, max_y, bbox.0, bbox.1, bbox.2, bbox.3) {
                    return Relation::CellInsideQuery;
                }
                Relation::CellCrossesQuery
            }
            Component::Tree { bbox, parts } => {
                if !(min_y <= bbox.3 && min_x <= bbox.1) {
                    return Relation::CellOutsideQuery;
                }
                let mut rel = Relation::CellOutsideQuery;
                for p in parts {
                    match p.relate(min_x, max_x, min_y, max_y) {
                        Relation::CellOutsideQuery => {}
                        Relation::CellInsideQuery => return Relation::CellInsideQuery,
                        Relation::CellCrossesQuery => rel = Relation::CellCrossesQuery,
                    }
                }
                rel
            }
            Component::Circle(c) => {
                if c.disjoint(min_x, max_x, min_y, max_y) {
                    Relation::CellOutsideQuery
                } else if c.within(min_x, max_x, min_y, max_y) {
                    Relation::CellCrossesQuery
                } else {
                    c.relate_cell(min_x, max_x, min_y, max_y)
                }
            }
            Component::Inverse(c) => match c.relate(min_x, max_x, min_y, max_y) {
                Relation::CellInsideQuery => Relation::CellOutsideQuery,
                Relation::CellOutsideQuery => Relation::CellInsideQuery,
                Relation::CellCrossesQuery => Relation::CellCrossesQuery,
            },
        }
    }

    /// `Polygon2D.numberOfCorners` (stops early once the count is decided).
    fn corners(&self, min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> u32 {
        let mut c = 0;
        if self.contains(min_x, min_y) {
            c += 1;
        }
        if self.contains(max_x, min_y) {
            c += 1;
        }
        if c == 1 {
            return c;
        }
        if self.contains(max_x, max_y) {
            c += 1;
        }
        if c == 2 {
            return c;
        }
        if self.contains(min_x, max_y) {
            c += 1;
        }
        c
    }

    pub fn intersects_line(&self, ax: f64, ay: f64, bx: f64, by: f64) -> bool {
        self.intersects_line_b(bbox2(ax, ay, bx, by), ax, ay, bx, by)
    }

    fn intersects_line_b(&self, b: Bbox, ax: f64, ay: f64, bx: f64, by: f64) -> bool {
        let (min_x, max_x, min_y, max_y) = b;
        match self {
            Component::Point { x, y } => contains_point(*x, *y, min_x, max_x, min_y, max_y) && orient(ax, ay, bx, by, *x, *y) == 0,
            Component::Line { bbox, tree } => {
                !disjoint(bbox.0, bbox.1, bbox.2, bbox.3, min_x, max_x, min_y, max_y) && tree.crosses_line(b, ax, ay, bx, by, true)
            }
            Component::Polygon { bbox, tree, holes } => {
                if disjoint(bbox.0, bbox.1, bbox.2, bbox.3, min_x, max_x, min_y, max_y) {
                    return false;
                }
                if self.contains(ax, ay) || self.contains(bx, by) || tree.crosses_line(b, ax, ay, bx, by, true) {
                    return holes.as_ref().is_none_or(|h| !h.contains_line_b(b, ax, ay, bx, by));
                }
                false
            }
            Component::Rectangle { bbox } => {
                if disjoint(bbox.0, bbox.1, bbox.2, bbox.3, min_x, max_x, min_y, max_y) {
                    return false;
                }
                self.contains(ax, ay) || self.contains(bx, by) || rect_edges_intersect(bbox, ax, ay, bx, by)
            }
            Component::Tree { bbox, parts } => {
                min_y <= bbox.3 && min_x <= bbox.1 && parts.iter().any(|p| p.intersects_line_b(b, ax, ay, bx, by))
            }
            Component::Circle(c) => {
                !c.disjoint(min_x, max_x, min_y, max_y) && (c.contains(ax, ay) || c.contains(bx, by) || c.intersects_line(ax, ay, bx, by))
            }
            // Unsupported for the inverse (Elasticsearch throws); never matches.
            Component::Inverse(_) => false,
        }
    }

    pub fn intersects_triangle(&self, ax: f64, ay: f64, bx: f64, by: f64, cx: f64, cy: f64) -> bool {
        self.intersects_triangle_b(bbox3(ax, ay, bx, by, cx, cy), ax, ay, bx, by, cx, cy)
    }

    #[allow(clippy::too_many_arguments)]
    fn intersects_triangle_b(&self, b: Bbox, ax: f64, ay: f64, bx: f64, by: f64, cx: f64, cy: f64) -> bool {
        let (min_x, max_x, min_y, max_y) = b;
        match self {
            Component::Point { x, y } => point_in_triangle(min_x, max_x, min_y, max_y, *x, *y, ax, ay, bx, by, cx, cy),
            Component::Line { bbox, tree } => {
                if disjoint(bbox.0, bbox.1, bbox.2, bbox.3, min_x, max_x, min_y, max_y) {
                    return false;
                }
                let (rx, ry) = tree.root_point();
                point_in_triangle(min_x, max_x, min_y, max_y, rx, ry, ax, ay, bx, by, cx, cy) || tree.crosses_triangle(b, ax, ay, bx, by, cx, cy, true)
            }
            Component::Polygon { bbox, tree, holes } => {
                if disjoint(bbox.0, bbox.1, bbox.2, bbox.3, min_x, max_x, min_y, max_y) {
                    return false;
                }
                let (rx, ry) = tree.root_point();
                if self.contains(ax, ay)
                    || self.contains(bx, by)
                    || self.contains(cx, cy)
                    || point_in_triangle(min_x, max_x, min_y, max_y, rx, ry, ax, ay, bx, by, cx, cy)
                    || tree.crosses_triangle(b, ax, ay, bx, by, cx, cy, true)
                {
                    return holes.as_ref().is_none_or(|h| !h.contains_triangle_b(b, ax, ay, bx, by, cx, cy));
                }
                false
            }
            Component::Rectangle { bbox } => {
                if disjoint(bbox.0, bbox.1, bbox.2, bbox.3, min_x, max_x, min_y, max_y) {
                    return false;
                }
                self.contains(ax, ay)
                    || self.contains(bx, by)
                    || self.contains(cx, cy)
                    || point_in_triangle(min_x, max_x, min_y, max_y, bbox.0, bbox.2, ax, ay, bx, by, cx, cy)
                    || rect_edges_intersect(bbox, ax, ay, bx, by)
                    || rect_edges_intersect(bbox, bx, by, cx, cy)
                    || rect_edges_intersect(bbox, cx, cy, ax, ay)
            }
            Component::Tree { bbox, parts } => {
                min_y <= bbox.3 && min_x <= bbox.1 && parts.iter().any(|p| p.intersects_triangle_b(b, ax, ay, bx, by, cx, cy))
            }
            Component::Circle(c) => {
                !c.disjoint(min_x, max_x, min_y, max_y)
                    && (c.contains(ax, ay)
                        || c.contains(bx, by)
                        || c.contains(cx, cy)
                        || point_in_triangle(min_x, max_x, min_y, max_y, c.center().0, c.center().1, ax, ay, bx, by, cx, cy)
                        || c.intersects_line(ax, ay, bx, by)
                        || c.intersects_line(bx, by, cx, cy)
                        || c.intersects_line(cx, cy, ax, ay))
            }
            Component::Inverse(_) => false,
        }
    }

    pub fn contains_line(&self, ax: f64, ay: f64, bx: f64, by: f64) -> bool {
        self.contains_line_b(bbox2(ax, ay, bx, by), ax, ay, bx, by)
    }

    fn contains_line_b(&self, b: Bbox, ax: f64, ay: f64, bx: f64, by: f64) -> bool {
        let (min_x, max_x, min_y, max_y) = b;
        match self {
            Component::Point { .. } | Component::Line { .. } => false,
            Component::Polygon { bbox, tree, holes } => {
                if disjoint(bbox.0, bbox.1, bbox.2, bbox.3, min_x, max_x, min_y, max_y) {
                    return false;
                }
                if self.contains(ax, ay) && self.contains(bx, by) && !tree.crosses_line(b, ax, ay, bx, by, false) {
                    return holes.as_ref().is_none_or(|h| !h.intersects_line_b(b, ax, ay, bx, by));
                }
                false
            }
            Component::Rectangle { bbox } => within(min_x, max_x, min_y, max_y, bbox.0, bbox.1, bbox.2, bbox.3),
            Component::Tree { bbox, parts } => {
                min_y <= bbox.3 && min_x <= bbox.1 && parts.iter().any(|p| p.contains_line_b(b, ax, ay, bx, by))
            }
            Component::Circle(c) => !c.disjoint(min_x, max_x, min_y, max_y) && c.contains(ax, ay) && c.contains(bx, by),
            Component::Inverse(_) => false,
        }
    }

    pub fn contains_triangle(&self, ax: f64, ay: f64, bx: f64, by: f64, cx: f64, cy: f64) -> bool {
        self.contains_triangle_b(bbox3(ax, ay, bx, by, cx, cy), ax, ay, bx, by, cx, cy)
    }

    #[allow(clippy::too_many_arguments)]
    fn contains_triangle_b(&self, b: Bbox, ax: f64, ay: f64, bx: f64, by: f64, cx: f64, cy: f64) -> bool {
        let (min_x, max_x, min_y, max_y) = b;
        match self {
            Component::Point { .. } | Component::Line { .. } => false,
            Component::Polygon { bbox, tree, holes } => {
                if disjoint(bbox.0, bbox.1, bbox.2, bbox.3, min_x, max_x, min_y, max_y) {
                    return false;
                }
                if self.contains(ax, ay) && self.contains(bx, by) && self.contains(cx, cy) && !tree.crosses_triangle(b, ax, ay, bx, by, cx, cy, false) {
                    return holes.as_ref().is_none_or(|h| !h.intersects_triangle_b(b, ax, ay, bx, by, cx, cy));
                }
                false
            }
            Component::Rectangle { bbox } => within(min_x, max_x, min_y, max_y, bbox.0, bbox.1, bbox.2, bbox.3),
            Component::Tree { bbox, parts } => {
                min_y <= bbox.3 && min_x <= bbox.1 && parts.iter().any(|p| p.contains_triangle_b(b, ax, ay, bx, by, cx, cy))
            }
            Component::Circle(c) => !c.disjoint(min_x, max_x, min_y, max_y) && c.contains(ax, ay) && c.contains(bx, by) && c.contains(cx, cy),
            Component::Inverse(_) => false,
        }
    }

    /// `withinPoint`; `None` for several components (Lucene throws).
    pub fn within_point(&self, x: f64, y: f64) -> Option<WithinRelation> {
        Some(match self {
            Component::Point { .. } => {
                if self.contains(x, y) {
                    WithinRelation::Candidate
                } else {
                    WithinRelation::Disjoint
                }
            }
            Component::Tree { .. } | Component::Inverse(_) => return None,
            _ => {
                if self.contains(x, y) {
                    WithinRelation::NotWithin
                } else {
                    WithinRelation::Disjoint
                }
            }
        })
    }

    pub fn within_line(&self, ax: f64, ay: f64, ab: bool, bx: f64, by: f64) -> Option<WithinRelation> {
        let b = bbox2(ax, ay, bx, by);
        let (min_x, max_x, min_y, max_y) = b;
        Some(match self {
            Component::Point { .. } => {
                if self.intersects_line_b(b, ax, ay, bx, by) {
                    WithinRelation::Candidate
                } else {
                    WithinRelation::Disjoint
                }
            }
            Component::Line { .. } => {
                if ab && self.intersects_line_b(b, ax, ay, bx, by) {
                    WithinRelation::NotWithin
                } else {
                    WithinRelation::Disjoint
                }
            }
            Component::Polygon { bbox, tree, .. } => {
                if disjoint(bbox.0, bbox.1, bbox.2, bbox.3, min_x, max_x, min_y, max_y) {
                    WithinRelation::Disjoint
                } else if self.contains(ax, ay) || self.contains(bx, by) || (ab && tree.crosses_line(b, ax, ay, bx, by, true)) {
                    WithinRelation::NotWithin
                } else {
                    WithinRelation::Disjoint
                }
            }
            Component::Rectangle { bbox } => {
                if disjoint(bbox.0, bbox.1, bbox.2, bbox.3, min_x, max_x, min_y, max_y) {
                    WithinRelation::Disjoint
                } else if self.contains(ax, ay) || self.contains(bx, by) || (ab && rect_edges_intersect(bbox, ax, ay, bx, by)) {
                    WithinRelation::NotWithin
                } else {
                    WithinRelation::Disjoint
                }
            }
            Component::Circle(c) => {
                if c.disjoint(min_x, max_x, min_y, max_y) {
                    WithinRelation::Disjoint
                } else if c.contains(ax, ay) || c.contains(bx, by) || (ab && c.intersects_line(ax, ay, bx, by)) {
                    WithinRelation::NotWithin
                } else {
                    WithinRelation::Disjoint
                }
            }
            Component::Tree { .. } | Component::Inverse(_) => return None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn within_triangle(&self, ax: f64, ay: f64, ab: bool, bx: f64, by: f64, bc: bool, cx: f64, cy: f64, ca: bool) -> Option<WithinRelation> {
        let b = bbox3(ax, ay, bx, by, cx, cy);
        let (min_x, max_x, min_y, max_y) = b;
        // The edge test shared by lines, polygons and rectangles.
        let edges = |cross: &dyn Fn(f64, f64, f64, f64) -> bool, point: (f64, f64)| -> WithinRelation {
            let mut rel = WithinRelation::Disjoint;
            for (x1, y1, x2, y2, from) in [(ax, ay, bx, by, ab), (bx, by, cx, cy, bc), (cx, cy, ax, ay, ca)] {
                if cross(x1, y1, x2, y2) {
                    if from {
                        return WithinRelation::NotWithin;
                    }
                    rel = WithinRelation::Candidate;
                }
            }
            if rel == WithinRelation::Candidate {
                return rel;
            }
            if point_in_triangle(min_x, max_x, min_y, max_y, point.0, point.1, ax, ay, bx, by, cx, cy) {
                return WithinRelation::Candidate;
            }
            rel
        };
        Some(match self {
            Component::Point { x, y } => {
                if point_in_triangle(min_x, max_x, min_y, max_y, *x, *y, ax, ay, bx, by, cx, cy) {
                    WithinRelation::Candidate
                } else {
                    WithinRelation::Disjoint
                }
            }
            Component::Line { bbox, tree } => {
                if disjoint(bbox.0, bbox.1, bbox.2, bbox.3, min_x, max_x, min_y, max_y) {
                    return Some(WithinRelation::Disjoint);
                }
                edges(&|x1, y1, x2, y2| tree.crosses_line(b, x1, y1, x2, y2, true), tree.root_point())
            }
            Component::Polygon { bbox, tree, .. } => {
                if disjoint(bbox.0, bbox.1, bbox.2, bbox.3, min_x, max_x, min_y, max_y) {
                    return Some(WithinRelation::Disjoint);
                }
                if self.contains(ax, ay) || self.contains(bx, by) || self.contains(cx, cy) {
                    return Some(WithinRelation::NotWithin);
                }
                edges(&|x1, y1, x2, y2| tree.crosses_line(b, x1, y1, x2, y2, true), tree.root_point())
            }
            Component::Rectangle { bbox } => {
                if disjoint(bbox.0, bbox.1, bbox.2, bbox.3, min_x, max_x, min_y, max_y) {
                    return Some(WithinRelation::Disjoint);
                }
                if self.contains(ax, ay) || self.contains(bx, by) || self.contains(cx, cy) {
                    return Some(WithinRelation::NotWithin);
                }
                edges(&|x1, y1, x2, y2| rect_edges_intersect(bbox, x1, y1, x2, y2), (bbox.0, bbox.2))
            }
            Component::Circle(c) => {
                // Circle2D.withinTriangle: shape edges crossing the circle disprove it; inner
                // edges do not make a candidate, only the center inside the triangle does.
                if c.disjoint(min_x, max_x, min_y, max_y) {
                    return Some(WithinRelation::Disjoint);
                }
                if c.contains(ax, ay) || c.contains(bx, by) || c.contains(cx, cy) {
                    return Some(WithinRelation::NotWithin);
                }
                if (ab && c.intersects_line(ax, ay, bx, by)) || (bc && c.intersects_line(bx, by, cx, cy)) || (ca && c.intersects_line(cx, cy, ax, ay)) {
                    return Some(WithinRelation::NotWithin);
                }
                if point_in_triangle(min_x, max_x, min_y, max_y, c.center().0, c.center().1, ax, ay, bx, by, cx, cy) {
                    WithinRelation::Candidate
                } else {
                    WithinRelation::Disjoint
                }
            }
            Component::Tree { .. } | Component::Inverse(_) => return None,
        })
    }
}

fn fold_min(v: &[f64]) -> f64 {
    v.iter().copied().fold(f64::INFINITY, f64::min)
}

fn fold_max(v: &[f64]) -> f64 {
    v.iter().copied().fold(f64::NEG_INFINITY, f64::max)
}

/// `Rectangle2D.edgesIntersect`.
fn rect_edges_intersect(b: &Bbox, ax: f64, ay: f64, bx: f64, by: f64) -> bool {
    let (min_x, max_x, min_y, max_y) = *b;
    if ax.max(bx) < min_x || ax.min(bx) > max_x || ay.min(by) > max_y || ay.max(by) < min_y {
        return false;
    }
    line_crosses_line_with_boundary(ax, ay, bx, by, min_x, max_y, max_x, max_y)
        || line_crosses_line_with_boundary(ax, ay, bx, by, max_x, max_y, max_x, min_y)
        || line_crosses_line_with_boundary(ax, ay, bx, by, max_x, min_y, min_x, min_y)
        || line_crosses_line_with_boundary(ax, ay, bx, by, min_x, min_y, min_x, max_y)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square() -> GeoPolygon {
        GeoPolygon::new(vec![0.0, 0.0, 10.0, 10.0, 0.0], vec![0.0, 10.0, 10.0, 0.0, 0.0], vec![]).unwrap()
    }

    #[test]
    fn polygon_relations() {
        let hole = GeoPolygon::new(vec![4.0, 4.0, 6.0, 6.0, 4.0], vec![4.0, 6.0, 6.0, 4.0, 4.0], vec![]).unwrap();
        let p = GeoPolygon::new(vec![0.0, 0.0, 10.0, 10.0, 0.0], vec![0.0, 10.0, 10.0, 0.0, 0.0], vec![hole]).unwrap();
        let c = Component::polygon(&p);
        assert!(c.contains(1.0, 1.0));
        assert!(c.contains(0.0, 5.0), "boundary is inside");
        assert!(!c.contains(5.0, 5.0), "hole");
        assert!(!c.contains(4.0, 5.0), "hole boundary belongs to the hole");
        assert_eq!(c.relate(1.0, 2.0, 1.0, 2.0), Relation::CellInsideQuery);
        assert_eq!(c.relate(4.5, 5.5, 4.5, 5.5), Relation::CellOutsideQuery);
        assert_eq!(c.relate(20.0, 30.0, 20.0, 30.0), Relation::CellOutsideQuery);
        assert_eq!(c.relate(9.0, 11.0, 9.0, 11.0), Relation::CellCrossesQuery);
        assert!(c.intersects_triangle(9.0, 9.0, 12.0, 9.0, 12.0, 12.0));
        assert!(!c.intersects_triangle(4.5, 4.5, 5.5, 4.5, 5.5, 5.5));
        assert!(c.contains_triangle(1.0, 1.0, 2.0, 1.0, 2.0, 2.0));
        assert!(!c.contains_triangle(1.0, 1.0, 5.0, 1.0, 5.0, 5.0));
        assert!(c.contains_line(1.0, 1.0, 3.0, 2.0));
        assert!(!c.contains_line(1.0, 5.0, 9.0, 5.0), "crosses the hole");
        // Within: an edge from the doc's polygon crossing the query proves not within.
        let q = Component::polygon(&square());
        assert_eq!(q.within_triangle(-20.0, -5.0, true, 30.0, -5.0, true, 5.0, 40.0, true), Some(WithinRelation::Candidate));
        assert_eq!(q.within_triangle(-5.0, -5.0, true, 20.0, -5.0, true, 5.0, 20.0, true), Some(WithinRelation::NotWithin));
        // An inner edge (not from the shape) crossing the query keeps the doc a candidate; a
        // shape edge crossing it does not.
        assert_eq!(q.within_triangle(5.0, -5.0, true, 20.0, -5.0, true, 20.0, 5.0, false), Some(WithinRelation::Disjoint));
        assert_eq!(q.within_triangle(-5.0, -5.0, true, 15.0, -5.0, true, 15.0, 15.0, false), Some(WithinRelation::Candidate));
        assert_eq!(q.within_triangle(-5.0, -5.0, false, 15.0, -5.0, false, 15.0, 15.0, true), Some(WithinRelation::NotWithin));
        assert_eq!(q.within_triangle(30.0, 30.0, true, 40.0, 30.0, true, 40.0, 40.0, true), Some(WithinRelation::Disjoint));
    }

    #[test]
    fn points_lines_rectangles() {
        let p = Component::point(1.5, 2.5);
        let Component::Point { x, y } = p.clone() else { panic!() };
        assert!(x >= 2.5 && y >= 1.5);
        assert!(p.contains(x, y));
        assert_eq!(p.relate(0.0, 5.0, 0.0, 5.0), Relation::CellCrossesQuery);
        let l = Component::line(&[0.0, 10.0], &[0.0, 10.0]);
        assert!(l.contains(5.0, 5.0));
        assert!(!l.contains(5.0, 6.0));
        assert!(l.intersects_line(0.0, 10.0, 10.0, 0.0));
        assert!(l.intersects_triangle(4.0, 6.0, 6.0, 6.0, 5.0, 3.0));
        assert_eq!(l.relate(20.0, 30.0, 20.0, 30.0), Relation::CellOutsideQuery);
        let r = Component::rectangle(0.0, 10.0, 0.0, 10.0);
        assert_eq!(r.relate(1.0, 2.0, 1.0, 2.0), Relation::CellInsideQuery);
        assert!(r.intersects_line(-5.0, 5.0, 15.0, 5.0));
        assert!(!r.contains_line(-5.0, 5.0, 15.0, 5.0));
        // Across the dateline: two boxes.
        let d = Component::rectangle(-10.0, 10.0, 170.0, -170.0);
        assert!(d.contains(175.0, 0.0) && d.contains(-175.0, 0.0) && !d.contains(0.0, 0.0));
        assert_eq!(d.within_point(175.0, 0.0), None);
    }
}
