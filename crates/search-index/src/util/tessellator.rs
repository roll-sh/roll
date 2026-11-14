//! Polygon tessellation for `geo_shape` indexing: Lucene 10.5's `Tessellator` (an
//! ear-clipping triangulation descended from mapbox's earcut), step for step — the triangles
//! and their edge flags equal Lucene's (checked against it in `tests/geo_shape.rs`).
//!
//! The polygon (outer ring clockwise in the linked list, holes counter-clockwise, bridged into
//! the outer ring left to right) is cut into triangles whose union is the polygon. Each
//! triangle records which of its edges lie on the original boundary: `contains` queries need
//! it (an edge from the polygon crossing the query proves the query is not within the shape).
//! Large rings (more than 80 vertices with their holes) find ear candidates through a z-order
//! (Morton) list. When no ear can be cut, the remaining ring is filtered, then small local
//! self-intersections are cured, then it is split along a valid diagonal; a ring that still
//! cannot be cut is malformed, and the errors are Lucene's (Elasticsearch reports them when a
//! document is rejected).
//!
//! Geometry is computed on the original coordinates (as Lucene); the triangles carry the
//! encoded coordinates the index stores: `GeoEncodingUtils` for geographic polygons,
//! `XYEncodingUtils` (sortable float bits) for cartesian ones ([`GeoPolygon::xy`], the `shape`
//! field). Nodes live in an arena and link by index.

use search_codec::points::encode::{encode_latitude, encode_longitude, f32_sortable};

use crate::error::{Error, Result};

/// Lucene's heuristic: past this many vertices, ear checks use the z-order list.
const VERTEX_THRESHOLD: usize = 80;
const NIL: u32 = u32::MAX;

/// A polygon (Lucene's `Polygon` in degrees, or `XYPolygon` in cartesian coordinates when built
/// with [`GeoPolygon::xy`]: `lons` then hold x and `lats` y): closed rings, holes without holes.
#[derive(Clone, Debug, PartialEq)]
pub struct GeoPolygon {
    pub lats: Vec<f64>,
    pub lons: Vec<f64>,
    pub holes: Vec<GeoPolygon>,
    pub min_lat: f64,
    pub max_lat: f64,
    pub min_lon: f64,
    pub max_lon: f64,
    /// Counter-clockwise (in lon/lat axes).
    ccw: bool,
    /// Cartesian (`XYPolygon`): float coordinates, `XYEncodingUtils` encoding.
    xy: bool,
}

/// `XYEncodingUtils.checkVal`.
pub(crate) fn check_xy(v: f32) -> Result<f32> {
    if !v.is_finite() {
        return Err(Error::illegal(format!(
            "invalid value {}; must be between -3.4028234663852886E38 and 3.4028234663852886E38",
            crate::util::java::float_to_string(v)
        )));
    }
    Ok(v)
}

/// `XYEncodingUtils.encode` (the value must be finite).
#[inline]
pub fn encode_xy(v: f32) -> i32 {
    f32_sortable(v)
}

/// `XYEncodingUtils.decode`.
#[inline]
pub fn decode_xy(v: i32) -> f64 {
    search_codec::points::encode::sortable_f32(v) as f64
}

fn check_lat(lat: f64) -> Result<()> {
    if lat.is_nan() || !(-90.0..=90.0).contains(&lat) {
        return Err(Error::illegal(format!("invalid latitude {}; must be between -90.0 and 90.0", crate::util::java::double_to_string(lat))));
    }
    Ok(())
}

fn check_lon(lon: f64) -> Result<()> {
    if lon.is_nan() || !(-180.0..=180.0).contains(&lon) {
        return Err(Error::illegal(format!(
            "invalid longitude {}; must be between -180.0 and 180.0",
            crate::util::java::double_to_string(lon)
        )));
    }
    Ok(())
}

impl GeoPolygon {
    /// Validates like Lucene's `Polygon` constructor.
    pub fn new(lats: Vec<f64>, lons: Vec<f64>, holes: Vec<GeoPolygon>) -> Result<GeoPolygon> {
        use crate::util::java::double_to_string as d;
        if lats.len() != lons.len() {
            return Err(Error::illegal("polyLats and polyLons must be equal length"));
        }
        if lats.len() < 4 {
            return Err(Error::illegal("at least 4 polygon points required"));
        }
        let n = lats.len() - 1;
        if lats[0] != lats[n] {
            return Err(Error::illegal(format!(
                "first and last points of the polygon must be the same (it must close itself): polyLats[0]={} polyLats[{n}]={}",
                d(lats[0]),
                d(lats[n])
            )));
        }
        if lons[0] != lons[n] {
            return Err(Error::illegal(format!(
                "first and last points of the polygon must be the same (it must close itself): polyLons[0]={} polyLons[{n}]={}",
                d(lons[0]),
                d(lons[n])
            )));
        }
        for i in 0..lats.len() {
            check_lat(lats[i])?;
            check_lon(lons[i])?;
        }
        if holes.iter().any(|h| !h.holes.is_empty()) {
            return Err(Error::illegal("holes may not contain holes: polygons may not nest."));
        }
        let (mut min_lat, mut max_lat, mut min_lon, mut max_lon) = (lats[0], lats[0], lons[0], lons[0]);
        let mut winding = 0f64;
        let mut j = 0;
        for i in 1..n {
            min_lat = lats[i].min(min_lat);
            max_lat = lats[i].max(max_lat);
            min_lon = lons[i].min(min_lon);
            max_lon = lons[i].max(max_lon);
            winding += (lons[j] - lons[n]) * (lats[i] - lats[n]) - (lats[j] - lats[n]) * (lons[i] - lons[n]);
            j = i;
        }
        Ok(GeoPolygon { lats, lons, holes, min_lat, max_lat, min_lon, max_lon, ccw: winding < 0.0, xy: false })
    }

    /// Validates like Lucene's `XYPolygon` constructor (holes must be cartesian too). The
    /// bounding box and winding are computed in float arithmetic, as Lucene does.
    pub fn xy(xs: &[f32], ys: &[f32], holes: Vec<GeoPolygon>) -> Result<GeoPolygon> {
        use crate::util::java::float_to_string as f;
        if xs.len() != ys.len() {
            return Err(Error::illegal("x and y must be equal length"));
        }
        if xs.len() < 4 {
            return Err(Error::illegal("at least 4 polygon points required"));
        }
        let n = xs.len() - 1;
        if xs[0] != xs[n] {
            return Err(Error::illegal(format!(
                "first and last points of the polygon must be the same (it must close itself): x[0]={} x[{n}]={}",
                f(xs[0]),
                f(xs[n])
            )));
        }
        if ys[0] != ys[n] {
            return Err(Error::illegal(format!(
                "first and last points of the polygon must be the same (it must close itself): y[0]={} y[{n}]={}",
                f(ys[0]),
                f(ys[n])
            )));
        }
        if holes.iter().any(|h| !h.holes.is_empty()) {
            return Err(Error::illegal("holes may not contain holes: polygons may not nest."));
        }
        if holes.iter().any(|h| !h.xy) {
            return Err(Error::illegal("holes of a cartesian polygon must be cartesian"));
        }
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (check_xy(xs[0])?, xs[0], check_xy(ys[0])?, ys[0]);
        let mut winding = 0f64;
        let mut j = 0;
        for i in 1..n {
            min_x = check_xy(xs[i])?.min(min_x);
            max_x = xs[i].max(max_x);
            min_y = check_xy(ys[i])?.min(min_y);
            max_y = ys[i].max(max_y);
            winding += ((xs[j] - xs[n]) * (ys[i] - ys[n]) - (ys[j] - ys[n]) * (xs[i] - xs[n])) as f64;
            j = i;
        }
        Ok(GeoPolygon {
            lats: ys.iter().map(|&v| v as f64).collect(),
            lons: xs.iter().map(|&v| v as f64).collect(),
            holes,
            min_lat: min_y as f64,
            max_lat: max_y as f64,
            min_lon: min_x as f64,
            max_lon: max_x as f64,
            ccw: winding < 0.0,
            xy: true,
        })
    }

    /// Whether this is a cartesian (`XYPolygon`) polygon.
    pub fn is_xy(&self) -> bool {
        self.xy
    }

    /// Lucene's `toString`: `Polygon`'s `[lat, lon] ` list, `XYPolygon`'s `XYPolygon[x, y] `
    /// list with its holes.
    pub fn lucene_string(&self) -> String {
        if !self.xy {
            return lucene_polygon_string(self);
        }
        use crate::util::java::float_to_string as f;
        let mut s = String::from("XYPolygon");
        for i in 0..self.lons.len() {
            s.push('[');
            s.push_str(&f(self.lons[i] as f32));
            s.push_str(", ");
            s.push_str(&f(self.lats[i] as f32));
            s.push_str("] ");
        }
        if !self.holes.is_empty() {
            let hs: Vec<String> = self.holes.iter().map(|h| h.lucene_string()).collect();
            s.push_str(", holes=[");
            s.push_str(&hs.join(", "));
            s.push(']');
        }
        s
    }

    pub fn num_points(&self) -> usize {
        self.lats.len()
    }
}

/// One output triangle: encoded `(x = lon, y = lat)` vertices and whether each edge (ab, bc,
/// ca) lies on the polygon's boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Triangle {
    pub x: [i32; 3],
    pub y: [i32; 3],
    pub from_polygon: [bool; 3],
}

#[derive(Clone, Copy)]
struct Node {
    idx: u32,
    /// Degrees: lon, lat.
    x: f64,
    y: f64,
    ex: i32,
    ey: i32,
    morton: u64,
    prev: u32,
    next: u32,
    prev_z: u32,
    next_z: u32,
    next_edge_from_polygon: bool,
}

/// Lucene's `BitUtil.interleave(even, odd)`.
fn interleave(even: u32, odd: u32) -> u64 {
    fn spread(v: u32) -> u64 {
        let mut v = v as u64;
        v = (v | (v << 16)) & 0x0000_FFFF_0000_FFFF;
        v = (v | (v << 8)) & 0x00FF_00FF_00FF_00FF;
        v = (v | (v << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
        v = (v | (v << 2)) & 0x3333_3333_3333_3333;
        v = (v | (v << 1)) & 0x5555_5555_5555_5555;
        v
    }
    (spread(odd) << 1) | spread(even)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Init,
    Cure,
    Split,
}

struct Tess {
    n: Vec<Node>,
    out: Vec<Triangle>,
    /// Work guard: hostile rings cannot spin forever.
    budget: u64,
    /// `XYEncodingUtils` vertices.
    xy: bool,
}

#[inline]
fn area(ax: f64, ay: f64, bx: f64, by: f64, cx: f64, cy: f64) -> f64 {
    (by - ay) * (cx - bx) - (bx - ax) * (cy - by)
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn point_in_ear(x: f64, y: f64, ax: f64, ay: f64, bx: f64, by: f64, cx: f64, cy: f64) -> bool {
    (cx - x) * (ay - y) - (ax - x) * (cy - y) >= 0.0
        && (ax - x) * (by - y) - (bx - x) * (ay - y) >= 0.0
        && (bx - x) * (cy - y) - (cx - x) * (by - y) >= 0.0
}

/// Lucene's `Tessellator.linesIntersect` (strict).
#[allow(clippy::too_many_arguments)]
pub fn lines_intersect(ax0: f64, ay0: f64, ax1: f64, ay1: f64, bx0: f64, by0: f64, bx1: f64, by1: f64) -> bool {
    (area(ax0, ay0, ax1, ay1, bx0, by0) > 0.0) != (area(ax0, ay0, ax1, ay1, bx1, by1) > 0.0)
        && (area(bx0, by0, bx1, by1, ax0, ay0) > 0.0) != (area(bx0, by0, bx1, by1, ax1, ay1) > 0.0)
}

/// `GeoUtils.orient`.
#[inline]
pub fn orient(ax: f64, ay: f64, bx: f64, by: f64, cx: f64, cy: f64) -> i32 {
    let v1 = (bx - ax) * (cy - ay);
    let v2 = (cx - ax) * (by - ay);
    if v1 > v2 {
        1
    } else if v1 < v2 {
        -1
    } else {
        0
    }
}

/// `GeoUtils.lineCrossesLine` (proper crossing).
#[allow(clippy::too_many_arguments)]
pub fn line_crosses_line(a1x: f64, a1y: f64, b1x: f64, b1y: f64, a2x: f64, a2y: f64, b2x: f64, b2y: f64) -> bool {
    orient(a2x, a2y, b2x, b2y, a1x, a1y) * orient(a2x, a2y, b2x, b2y, b1x, b1y) < 0
        && orient(a1x, a1y, b1x, b1y, a2x, a2y) * orient(a1x, a1y, b1x, b1y, b2x, b2y) < 0
}

/// `GeoUtils.lineOverlapLine` (collinear).
#[allow(clippy::too_many_arguments)]
pub fn line_overlap_line(a1x: f64, a1y: f64, b1x: f64, b1y: f64, a2x: f64, a2y: f64, b2x: f64, b2y: f64) -> bool {
    orient(a2x, a2y, b2x, b2y, a1x, a1y) == 0
        && orient(a2x, a2y, b2x, b2y, b1x, b1y) == 0
        && orient(a1x, a1y, b1x, b1y, a2x, a2y) == 0
        && orient(a1x, a1y, b1x, b1y, b2x, b2y) == 0
}

const MALFORMED: &str = "Unable to Tessellate shape. Possible malformed shape detected.";

impl Tess {
    fn x(&self, i: u32) -> f64 {
        self.n[i as usize].x
    }
    fn y(&self, i: u32) -> f64 {
        self.n[i as usize].y
    }
    fn next(&self, i: u32) -> u32 {
        self.n[i as usize].next
    }
    fn prev(&self, i: u32) -> u32 {
        self.n[i as usize].prev
    }
    fn idx(&self, i: u32) -> u32 {
        self.n[i as usize].idx
    }

    fn tick(&mut self) -> Result<()> {
        if self.budget == 0 {
            return Err(Error::illegal(MALFORMED));
        }
        self.budget -= 1;
        Ok(())
    }

    fn new_node(&mut self, x: f64, y: f64, idx: u32) -> u32 {
        let (ex, ey) = if self.xy { (encode_xy(x as f32), encode_xy(y as f32)) } else { (encode_longitude(x), encode_latitude(y)) };
        let morton = interleave((ex as u32) ^ 0x8000_0000, (ey as u32) ^ 0x8000_0000);
        let i = self.n.len() as u32;
        self.n.push(Node { idx, x, y, ex, ey, morton, prev: i, next: i, prev_z: i, next_z: i, next_edge_from_polygon: true });
        i
    }

    fn insert_node(&mut self, x: f64, y: f64, idx: u32, last: Option<u32>) -> u32 {
        let node = self.new_node(x, y, idx);
        if let Some(last) = last {
            let ln = self.n[last as usize].next;
            let lnz = self.n[last as usize].next_z;
            let nd = &mut self.n[node as usize];
            nd.next = ln;
            nd.next_z = ln;
            nd.prev = last;
            nd.prev_z = last;
            self.n[ln as usize].prev = node;
            if lnz != NIL {
                self.n[lnz as usize].prev_z = node;
            }
            self.n[last as usize].next = node;
            self.n[last as usize].next_z = node;
        }
        node
    }

    fn remove_node(&mut self, node: u32, edge_from_polygon: bool) {
        let Node { prev, next, prev_z, next_z, .. } = self.n[node as usize];
        self.n[next as usize].prev = prev;
        self.n[prev as usize].next = next;
        self.n[prev as usize].next_edge_from_polygon = edge_from_polygon;
        if prev_z != NIL {
            self.n[prev_z as usize].next_z = next_z;
        }
        if next_z != NIL {
            self.n[next_z as usize].prev_z = prev_z;
        }
    }

    fn vertex_eq(&self, a: u32, b: u32) -> bool {
        self.x(a) == self.x(b) && self.y(a) == self.y(b)
    }

    fn vertex_eq_xy(&self, a: u32, x: f64, y: f64) -> bool {
        self.x(a) == x && self.y(a) == y
    }

    fn area3(&self, a: u32, b: u32, c: u32) -> f64 {
        area(self.x(a), self.y(a), self.x(b), self.y(b), self.x(c), self.y(c))
    }

    /// A ring in the requested winding (`cw`), its closing duplicate removed, filtered.
    fn create_list(&mut self, lons: &[f64], lats: &[f64], poly_ccw: bool, start_index: u32, want_cw: bool) -> Option<u32> {
        let mut last: Option<u32> = None;
        let mut index = start_index;
        // Lucene's WindingOrder: CW wanted and the polygon is CW → keep the order.
        if want_cw != poly_ccw {
            for i in 0..lons.len() {
                last = Some(self.insert_node(lons[i], lats[i], index, last));
                index += 1;
            }
        } else {
            for i in (0..lons.len()).rev() {
                last = Some(self.insert_node(lons[i], lats[i], index, last));
                index += 1;
            }
        }
        let mut last = last?;
        let ln = self.next(last);
        if self.vertex_eq(last, ln) {
            self.remove_node(last, true);
            last = ln;
        }
        Some(self.filter_points(last, None))
    }

    fn filter_points(&mut self, start: u32, end: Option<u32>) -> u32 {
        let mut end = end.unwrap_or(start);
        let mut node = start;
        loop {
            let mut cont = false;
            let next = self.next(node);
            let prev = self.prev(node);
            let collinear = self.area3(prev, node, next) == 0.0;
            if self.vertex_eq(node, next)
                || self.vertex_eq(prev, next)
                || ((self.n[prev as usize].next_edge_from_polygon == self.n[node as usize].next_edge_from_polygon
                    || self.point_in_line(prev, node, self.x(next), self.y(next)))
                    && collinear)
            {
                let e = self.n[prev as usize].next_edge_from_polygon;
                self.remove_node(node, e);
                node = prev;
                end = prev;
                if node == next {
                    break;
                }
                cont = true;
            } else {
                node = next;
            }
            if !(cont || node != end) {
                break;
            }
        }
        end
    }

    fn point_in_line(&self, a: u32, b: u32, lon: f64, lat: f64) -> bool {
        let (ax, ay, bx, by) = (self.x(a), self.y(a), self.x(b), self.y(b));
        let (dxc, dyc, dxl, dyl) = (lon - ax, lat - ay, bx - ax, by - ay);
        if dxc * dyl - dyc * dxl == 0.0 {
            if dxl.abs() >= dyl.abs() {
                if dxl > 0.0 { ax <= lon && lon <= bx } else { bx <= lon && lon <= ax }
            } else if dyl > 0.0 {
                ay <= lat && lat <= by
            } else {
                by <= lat && lat <= ay
            }
        } else {
            false
        }
    }

    fn point_in_line_n(&self, a: u32, b: u32, p: u32) -> bool {
        self.point_in_line(a, b, self.x(p), self.y(p))
    }

    fn fetch_leftmost(&self, start: u32) -> u32 {
        let mut node = start;
        let mut left = start;
        loop {
            if self.x(node) < self.x(left) || (self.x(node) == self.x(left) && self.y(node) < self.y(left)) {
                left = node;
            }
            node = self.next(node);
            if node == start {
                break;
            }
        }
        left
    }

    /// Splits the ring by joining `a` and `b` with copies; returns `b`'s copy.
    fn split_polygon(&mut self, a: u32, b: u32, edge_from_polygon: bool) -> u32 {
        let a2 = self.n.len() as u32;
        let na = self.n[a as usize];
        self.n.push(na);
        let b2 = self.n.len() as u32;
        let nb = self.n[b as usize];
        self.n.push(nb);
        let an = self.next(a);
        let bp = self.prev(b);
        {
            let x = &mut self.n[a as usize];
            x.next = b;
            x.next_edge_from_polygon = edge_from_polygon;
            x.next_z = b;
        }
        self.n[b as usize].prev = a;
        self.n[b as usize].prev_z = a;
        self.n[a2 as usize].next = an;
        self.n[a2 as usize].next_z = an;
        self.n[an as usize].prev = a2;
        self.n[an as usize].prev_z = a2;
        {
            let x = &mut self.n[b2 as usize];
            x.next = a2;
            x.next_edge_from_polygon = edge_from_polygon;
            x.next_z = a2;
        }
        self.n[a2 as usize].prev = b2;
        self.n[a2 as usize].prev_z = b2;
        self.n[bp as usize].next = b2;
        self.n[bp as usize].next_z = b2;
        b2
    }

    fn is_locally_inside(&self, a: u32, b: u32) -> bool {
        let (p, nx) = (self.prev(a), self.next(a));
        let ar = self.area3(p, a, nx);
        if ar == 0.0 {
            false
        } else if ar < 0.0 {
            self.area3(a, b, nx) >= 0.0 && self.area3(a, p, b) >= 0.0
        } else {
            self.area3(a, b, p) <= 0.0 || self.area3(a, nx, b) <= 0.0
        }
    }

    fn eliminate_holes(&mut self, poly: &GeoPolygon, mut outer: u32) -> Result<u32> {
        let mut holes: Vec<(u32, usize)> = Vec::new();
        let mut index = poly.num_points() as u32;
        for (hi, h) in poly.holes.iter().enumerate() {
            let Some(list) = self.create_list(&h.lons, &h.lats, h.ccw, index, false) else { continue };
            if list == self.next(list) {
                return Err(Error::illegal(format!("Points are all coplanar in hole: {}", h.lucene_string())));
            }
            holes.push((self.fetch_leftmost(list), hi));
            index += h.num_points() as u32;
        }
        holes.sort_by(|&(a, _), &(b, _)| {
            let mut diff = self.x(a) - self.x(b);
            if diff == 0.0 {
                diff = self.y(a) - self.y(b);
                if diff == 0.0 {
                    let ma = self.y(self.prev(a)).min(self.y(self.next(a)));
                    let mb = self.y(self.prev(b)).min(self.y(self.next(b)));
                    diff = ma - mb;
                }
            }
            diff.partial_cmp(&0.0).unwrap_or(std::cmp::Ordering::Equal)
        });
        for (node, hi) in holes {
            let h = &poly.holes[hi];
            if let Some(result) = self.eliminate_hole(node, outer, h.min_lon, h.max_lon, h.min_lat, h.max_lat)? {
                let rn = self.next(result);
                outer = self.filter_points(result, Some(rn));
            } else {
                let mut s = if h.xy { xy_vertices_to_geojson(&h.lons, &h.lats) } else { verticles_to_geojson(&h.lats, &h.lons) };
                if s.len() > 100 {
                    s.truncate(100);
                    s.push_str("...");
                }
                return Err(Error::illegal(format!("Illegal hole detected: {s}")));
            }
        }
        Ok(self.filter_points(outer, None))
    }

    /// Links the hole into the outer ring; returns where the outer ring continues, or `None`
    /// when no bridge exists (the hole is not inside).
    fn eliminate_hole(&mut self, hole: u32, outer: u32, min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> Result<Option<u32>> {
        if let Some(r) = self.merge_shared_vertices(hole, outer, min_x, max_x, min_y, max_y)? {
            return Ok(Some(r));
        }
        let Some(bridge) = self.fetch_hole_bridge(hole, outer)? else { return Ok(None) };
        let from_polygon = self.point_in_line_n(bridge, self.next(bridge), hole)
            || self.point_in_line_n(bridge, self.prev(bridge), hole)
            || self.point_in_line_n(hole, self.next(hole), bridge)
            || self.point_in_line_n(hole, self.prev(hole), bridge);
        self.split_polygon(bridge, hole, from_polygon);
        Ok(Some(outer))
    }

    /// Merges the hole at a vertex it shares with the ring (preferring the hole's leftmost
    /// vertex, first match in ring order, as earcut.js); returns where the outer ring
    /// continues, or `None` without a shared vertex.
    fn merge_shared_vertices(&mut self, hole: u32, outer: u32, min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> Result<Option<u32>> {
        let mut shared: Option<u32> = None;
        let mut connection = NIL;
        let mut leftmost_connection: Option<u32> = None;
        let mut next = outer;
        loop {
            self.tick()?;
            let (x, y) = (self.x(next), self.y(next));
            if y >= min_y && y <= max_y && x >= min_x && x <= max_x {
                // getSharedVertex: the first hole node equal to `next`.
                let mut h = hole;
                let mut found = None;
                loop {
                    if self.vertex_eq(h, next) {
                        found = Some(h);
                        break;
                    }
                    h = self.next(h);
                    if h == hole {
                        break;
                    }
                }
                if let Some(v) = found {
                    if self.vertex_eq(v, hole) && leftmost_connection.is_none() {
                        leftmost_connection = Some(next);
                    }
                    match shared {
                        None => {
                            shared = Some(v);
                            connection = next;
                        }
                        Some(s) if s == v => connection = self.shared_inside_vertex(s, connection, next),
                        _ => {}
                    }
                }
            }
            next = self.next(next);
            if next == outer {
                break;
            }
        }
        if let Some(lc) = leftmost_connection
            && self.idx(lc) >= self.idx(connection)
        {
            self.split_polygon(lc, hole, true);
            return Ok(Some(if lc != connection { lc } else { outer }));
        }
        if let Some(s) = shared {
            self.split_polygon(connection, s, true);
            return Ok(Some(outer));
        }
        Ok(None)
    }

    fn shared_inside_vertex(&self, hole_v: u32, a: u32, b: u32) -> u32 {
        let hn = self.next(hole_v);
        let a1 = self.area3(self.prev(a), hole_v, hn);
        let a2 = self.area3(self.prev(b), hole_v, hn);
        if (a1 < 0.0) != (a2 < 0.0) {
            if a1 < a2 { a } else { b }
        } else {
            let angle = |p: u32, q: u32, r: u32| -> f64 {
                let (ax, ay) = (self.x(p) - self.x(q), self.y(p) - self.y(q));
                let (cx, cy) = (self.x(r) - self.x(q), self.y(r) - self.y(q));
                let dot = ax * cx + ay * cy;
                (dot / ((ax * ax + ay * ay).sqrt() * (cx * cx + cy * cy).sqrt())).acos()
            };
            if angle(self.prev(a), a, hn) < angle(self.prev(b), b, hn) { a } else { b }
        }
    }

    fn fetch_hole_bridge(&mut self, hole: u32, outer: u32) -> Result<Option<u32>> {
        let mut p = outer;
        let mut qx = f64::NEG_INFINITY;
        let (hx, hy) = (self.x(hole), self.y(hole));
        let mut connection: Option<u32> = None;
        loop {
            self.tick()?;
            let pn = self.next(p);
            let (px, py, nx, ny) = (self.x(p), self.y(p), self.x(pn), self.y(pn));
            if hy <= py && hy >= ny && ny != py {
                let x = px + (hy - py) * (nx - px) / (ny - py);
                if x <= hx && x > qx {
                    qx = x;
                    if x == hx {
                        if hy == py {
                            return Ok(Some(p));
                        }
                        if hy == ny {
                            return Ok(Some(pn));
                        }
                    }
                    connection = Some(if px < nx { p } else { pn });
                }
            }
            p = pn;
            if p == outer {
                break;
            }
        }
        let Some(mut connection) = connection else { return Ok(None) };
        if hx == qx {
            return Ok(Some(self.prev(connection)));
        }
        let stop = connection;
        let (mx, my) = (self.x(connection), self.y(connection));
        let mut tan_min = f64::INFINITY;
        p = connection;
        loop {
            self.tick()?;
            let (px, py) = (self.x(p), self.y(p));
            if hx >= px
                && px >= mx
                && hx != px
                && point_in_ear(px, py, if hy < my { hx } else { qx }, hy, mx, my, if hy < my { qx } else { hx }, hy)
            {
                let tan = (hy - py).abs() / (hx - px);
                if (tan < tan_min || (tan == tan_min && px > self.x(connection))) && self.is_locally_inside(p, hole) {
                    connection = p;
                    tan_min = tan;
                }
            }
            p = self.next(p);
            if p == stop {
                break;
            }
        }
        Ok(Some(connection))
    }

    fn sort_by_morton(&mut self, start: u32) {
        let pz = self.n[start as usize].prev_z;
        if pz != NIL {
            self.n[pz as usize].next_z = NIL;
        }
        self.n[start as usize].prev_z = NIL;
        self.tatham_sort(start);
    }

    fn sort_by_morton_with_reset(&mut self, start: u32) {
        let mut next = start;
        loop {
            let (p, nx) = (self.prev(next), self.next(next));
            self.n[next as usize].prev_z = p;
            self.n[next as usize].next_z = nx;
            next = nx;
            if next == start {
                break;
            }
        }
        self.sort_by_morton(start);
    }

    /// Simon Tatham's linked-list merge sort by morton code (stable).
    fn tatham_sort(&mut self, mut list: u32) {
        let mut in_size = 1usize;
        loop {
            let mut p = list;
            list = NIL;
            let mut tail = NIL;
            let mut merges = 0;
            while p != NIL {
                merges += 1;
                let mut q = p;
                let mut p_size = 0;
                for _ in 0..in_size {
                    if q == NIL {
                        break;
                    }
                    p_size += 1;
                    q = self.n[q as usize].next_z;
                }
                let mut q_size = in_size;
                while p_size > 0 || (q_size > 0 && q != NIL) {
                    let e;
                    if p_size != 0 && (q_size == 0 || q == NIL || self.n[p as usize].morton <= self.n[q as usize].morton) {
                        e = p;
                        p = self.n[p as usize].next_z;
                        p_size -= 1;
                    } else {
                        e = q;
                        q = self.n[q as usize].next_z;
                        q_size -= 1;
                    }
                    if tail != NIL {
                        self.n[tail as usize].next_z = e;
                    } else {
                        list = e;
                    }
                    self.n[e as usize].prev_z = tail;
                    tail = e;
                }
                p = q;
            }
            if tail != NIL {
                self.n[tail as usize].next_z = NIL;
            }
            in_size *= 2;
            if merges <= 1 {
                break;
            }
        }
    }

    fn morton_range(&self, a: u32, b: u32, c: Option<u32>) -> (u64, u64) {
        let xs = [self.n[a as usize].ex, self.n[b as usize].ex, c.map_or(self.n[b as usize].ex, |c| self.n[c as usize].ex)];
        let ys = [self.n[a as usize].ey, self.n[b as usize].ey, c.map_or(self.n[b as usize].ey, |c| self.n[c as usize].ey)];
        let flip = |v: i32| (v as u32) ^ 0x8000_0000;
        let (min_x, max_x) = (flip(*xs.iter().min().unwrap()), flip(*xs.iter().max().unwrap()));
        let (min_y, max_y) = (flip(*ys.iter().min().unwrap()), flip(*ys.iter().max().unwrap()));
        (interleave(min_x, min_y), interleave(max_x, max_y))
    }

    /// Whether `p` (another vertex) blocks the ear (lies in it and is reflex).
    fn blocks_ear(&self, p: u32, ear: u32) -> bool {
        let (ep, en) = (self.prev(ear), self.next(ear));
        point_in_ear(self.x(p), self.y(p), self.x(ep), self.y(ep), self.x(ear), self.y(ear), self.x(en), self.y(en))
            && self.area3(self.prev(p), p, self.next(p)) >= 0.0
    }

    fn is_ear(&mut self, ear: u32, morton: bool) -> Result<bool> {
        if morton {
            return self.morton_is_ear(ear);
        }
        let mut node = self.next(self.next(ear));
        let stop = self.prev(ear);
        while node != stop {
            self.tick()?;
            if self.blocks_ear(node, ear) {
                return Ok(false);
            }
            node = self.next(node);
        }
        Ok(true)
    }

    fn morton_is_ear(&mut self, ear: u32) -> Result<bool> {
        let (ep, en) = (self.prev(ear), self.next(ear));
        let (min_z, max_z) = self.morton_range(ep, ear, Some(en));
        let (pi, ni) = (self.idx(ep), self.idx(en));
        let mut p = self.n[ear as usize].prev_z;
        let mut n = self.n[ear as usize].next_z;
        let ok = |t: &Tess, q: u32| t.idx(q) != pi && t.idx(q) != ni && t.blocks_ear(q, ear);
        while p != NIL && self.n[p as usize].morton >= min_z && n != NIL && self.n[n as usize].morton <= max_z {
            self.tick()?;
            if ok(self, p) {
                return Ok(false);
            }
            p = self.n[p as usize].prev_z;
            if ok(self, n) {
                return Ok(false);
            }
            n = self.n[n as usize].next_z;
        }
        while p != NIL && self.n[p as usize].morton >= min_z {
            self.tick()?;
            if ok(self, p) {
                return Ok(false);
            }
            p = self.n[p as usize].prev_z;
        }
        while n != NIL && self.n[n as usize].morton <= max_z {
            self.tick()?;
            if ok(self, n) {
                return Ok(false);
            }
            n = self.n[n as usize].next_z;
        }
        Ok(true)
    }

    fn edge_from_polygon(&mut self, a: u32, b: u32, morton: bool) -> Result<bool> {
        if morton {
            return self.morton_edge_from_polygon(a, b);
        }
        let mut next = a;
        loop {
            self.tick()?;
            if let Some(r) = self.edge_at(next, a, b) {
                return Ok(r);
            }
            next = self.next(next);
            if next == a {
                break;
            }
        }
        Ok(false)
    }

    /// Whether `a`–`b` lies on `q`'s next or previous edge, and that edge's flag.
    fn edge_at(&self, q: u32, a: u32, b: u32) -> Option<bool> {
        let qn = self.next(q);
        if self.point_in_line_n(q, qn, a) && self.point_in_line_n(q, qn, b) {
            return Some(self.n[q as usize].next_edge_from_polygon);
        }
        let qp = self.prev(q);
        if self.point_in_line_n(q, qp, a) && self.point_in_line_n(q, qp, b) {
            return Some(self.n[qp as usize].next_edge_from_polygon);
        }
        None
    }

    fn morton_edge_from_polygon(&mut self, a: u32, b: u32) -> Result<bool> {
        let (min_z, max_z) = self.morton_range(a, b, None);
        let mut p = self.n[a as usize].prev_z;
        let mut n = self.n[a as usize].next_z;
        while p != NIL && self.n[p as usize].morton >= min_z && n != NIL && self.n[n as usize].morton <= max_z {
            self.tick()?;
            if let Some(r) = self.edge_at(p, a, b) {
                return Ok(r);
            }
            p = self.n[p as usize].prev_z;
            if let Some(r) = self.edge_at(n, a, b) {
                return Ok(r);
            }
            n = self.n[n as usize].next_z;
        }
        while p != NIL && self.n[p as usize].morton >= min_z {
            self.tick()?;
            if let Some(r) = self.edge_at(p, a, b) {
                return Ok(r);
            }
            p = self.n[p as usize].prev_z;
        }
        while n != NIL && self.n[n as usize].morton <= max_z {
            self.tick()?;
            if let Some(r) = self.edge_at(n, a, b) {
                return Ok(r);
            }
            n = self.n[n as usize].next_z;
        }
        Ok(false)
    }

    fn push_triangle(&mut self, a: u32, ab: bool, b: u32, bc: bool, c: u32, ca: bool) {
        let (na, nb, nc) = (self.n[a as usize], self.n[b as usize], self.n[c as usize]);
        self.out.push(Triangle { x: [na.ex, nb.ex, nc.ex], y: [na.ey, nb.ey, nc.ey], from_polygon: [ab, bc, ca] });
    }

    fn earcut(&mut self, mut ear: u32, mut state: State, morton: bool) -> Result<()> {
        'earcut: loop {
            if self.prev(ear) == self.next(ear) {
                return Ok(());
            }
            let mut stop = ear;
            loop {
                self.tick()?;
                let prev = self.prev(ear);
                let next = self.next(ear);
                let reflex = self.area3(prev, ear, next) >= 0.0;
                if !reflex && self.is_ear(ear, morton)? {
                    let ab = self.n[prev as usize].next_edge_from_polygon;
                    let bc = self.n[ear as usize].next_edge_from_polygon;
                    let ca = self.edge_from_polygon(prev, next, morton)?;
                    self.push_triangle(prev, ab, ear, bc, next, ca);
                    self.remove_node(ear, ca);
                    ear = self.next(next);
                    stop = ear;
                    if self.prev(ear) == self.next(ear) {
                        break;
                    }
                    continue;
                }
                ear = next;
                if ear == stop {
                    match state {
                        State::Init => {
                            ear = self.filter_points(ear, None);
                            state = State::Cure;
                            continue 'earcut;
                        }
                        State::Cure => {
                            ear = self.cure_local_intersections(ear, morton)?;
                            state = State::Split;
                            continue 'earcut;
                        }
                        State::Split => {
                            if !self.split_earcut(ear, morton)? {
                                return Err(Error::illegal(MALFORMED));
                            }
                            break;
                        }
                    }
                }
                if self.prev(ear) == self.next(ear) {
                    break;
                }
            }
            return Ok(());
        }
    }

    fn cure_local_intersections(&mut self, mut start: u32, morton: bool) -> Result<u32> {
        let mut node = start;
        loop {
            self.tick()?;
            let next = self.next(node);
            let a = self.prev(node);
            let b = self.next(next);
            if !self.vertex_eq(a, b)
                && lines_intersect(self.x(a), self.y(a), self.x(node), self.y(node), self.x(next), self.y(next), self.x(b), self.y(b))
                && self.is_locally_inside(a, b)
                && self.is_locally_inside(b, a)
                && !self.intersecting_polygon(a, self.x(a), self.y(a), self.x(b), self.y(b))?
            {
                let ab = if self.next(a) == node { self.n[a as usize].next_edge_from_polygon } else { self.edge_from_polygon(a, node, morton)? };
                let bc = if self.next(node) == b { self.n[node as usize].next_edge_from_polygon } else { self.edge_from_polygon(node, b, morton)? };
                let ca = if self.next(b) == a { self.n[b as usize].next_edge_from_polygon } else { self.edge_from_polygon(a, b, morton)? };
                // Lucene adds this triangle twice.
                self.push_triangle(a, ab, node, bc, b, ca);
                self.push_triangle(a, ab, node, bc, b, ca);
                self.remove_node(node, ca);
                let nn = self.next(node);
                self.remove_node(nn, ca);
                node = b;
                start = b;
            }
            node = self.next(node);
            if node == start {
                break;
            }
        }
        Ok(node)
    }

    fn intersecting_polygon(&mut self, start: u32, x0: f64, y0: f64, x1: f64, y1: f64) -> Result<bool> {
        let mut node = start;
        loop {
            self.tick()?;
            let next = self.next(node);
            if !self.vertex_eq_xy(node, x0, y0)
                && !self.vertex_eq_xy(node, x1, y1)
                && lines_intersect(self.x(node), self.y(node), self.x(next), self.y(next), x0, y0, x1, y1)
            {
                return Ok(true);
            }
            node = next;
            if node == start {
                break;
            }
        }
        Ok(false)
    }

    fn signed_area(&self, start: u32, end: u32) -> f64 {
        let mut next = start;
        let mut sum = 0.0;
        loop {
            let nn = self.next(next);
            sum += self.area3(next, nn, end);
            next = nn;
            if self.next(next) == end {
                break;
            }
        }
        sum
    }

    fn middle_insert(&self, start: u32, x0: f64, y0: f64, x1: f64, y1: f64) -> bool {
        let mut node = start;
        let mut inside = false;
        // Lucene divides by 2.0f (a float literal, exact in double).
        let (ldx, ldy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
        loop {
            let next = self.next(node);
            let (nx, ny, px, py) = (self.x(next), self.y(next), self.x(node), self.y(node));
            if (py > ldy) != (ny > ldy) && ldx < (nx - px) / (ny - py) + px {
                inside = !inside;
            }
            node = next;
            if node == start {
                break;
            }
        }
        inside
    }

    fn valid_diagonal(&mut self, a: u32, b: u32) -> Result<bool> {
        if self.idx(self.next(a)) == self.idx(b)
            || self.idx(self.prev(a)) == self.idx(b)
            || !self.is_locally_inside(self.prev(a), b)
            || !self.is_locally_inside(self.next(b), a)
            || self.signed_area(a, b) >= 0.0
            || self.signed_area(b, a) >= 0.0
        {
            return Ok(false);
        }
        if self.vertex_eq(a, b) {
            return Ok(true);
        }
        Ok(self.is_locally_inside(a, b)
            && self.is_locally_inside(b, a)
            && self.middle_insert(a, self.x(a), self.y(a), self.x(b), self.y(b))
            && self.area3(self.prev(a), a, b) != 0.0
            && self.area3(a, b, self.next(b)) != 0.0
            && self.area3(self.next(a), a, b) != 0.0
            && self.area3(a, b, self.prev(b)) != 0.0
            && !self.intersecting_polygon(a, self.x(a), self.y(a), self.x(b), self.y(b))?)
    }

    fn split_earcut(&mut self, start: u32, morton: bool) -> Result<bool> {
        let mut search = start;
        loop {
            let next = self.next(search);
            let mut diagonal = self.next(next);
            while diagonal != self.prev(search) {
                self.tick()?;
                if self.idx(search) != self.idx(diagonal) && self.valid_diagonal(search, diagonal)? {
                    let efp = self.edge_from_polygon(search, diagonal, morton)?;
                    let mut split = self.split_polygon(search, diagonal, efp);
                    let sn = self.next(search);
                    search = self.filter_points(search, Some(sn));
                    let spn = self.next(split);
                    split = self.filter_points(split, Some(spn));
                    if morton {
                        self.sort_by_morton_with_reset(search);
                        self.sort_by_morton_with_reset(split);
                    }
                    self.earcut(search, State::Init, morton)?;
                    self.earcut(split, State::Init, morton)?;
                    return Ok(true);
                }
                diagonal = self.next(diagonal);
            }
            search = self.next(search);
            if search == start {
                break;
            }
        }
        Ok(self.signed_area(start, start) == 0.0)
    }

    fn check_intersection(&mut self, a: u32, morton: bool) -> Result<()> {
        let mut next = self.next(a);
        let stop = self.prev(a);
        loop {
            let mut inner = self.next(next);
            if morton {
                self.morton_check_intersection(next, inner)?;
            } else {
                let inner_stop = self.prev(next);
                loop {
                    self.tick()?;
                    self.check_intersection_point(next, inner)?;
                    inner = self.next(inner);
                    if inner == inner_stop {
                        break;
                    }
                }
            }
            next = self.next(next);
            if next == stop {
                break;
            }
        }
        Ok(())
    }

    fn morton_check_intersection(&mut self, a: u32, b: u32) -> Result<()> {
        let an = self.next(a);
        let (min_z, max_z) = self.morton_range(a, an, None);
        let mut p = self.n[b as usize].prev_z;
        let mut n = self.n[b as usize].next_z;
        while p != NIL && self.n[p as usize].morton >= min_z && n != NIL && self.n[n as usize].morton <= max_z {
            self.tick()?;
            self.check_intersection_point(p, a)?;
            p = self.n[p as usize].prev_z;
            self.check_intersection_point(n, a)?;
            n = self.n[n as usize].next_z;
        }
        while p != NIL && self.n[p as usize].morton >= min_z {
            self.tick()?;
            self.check_intersection_point(p, a)?;
            p = self.n[p as usize].prev_z;
        }
        while n != NIL && self.n[n as usize].morton <= max_z {
            self.tick()?;
            self.check_intersection_point(n, a)?;
            n = self.n[n as usize].next_z;
        }
        Ok(())
    }

    fn check_intersection_point(&self, a: u32, b: u32) -> Result<()> {
        if a == b {
            return Ok(());
        }
        let (an, bn) = (self.next(a), self.next(b));
        let (ax, ay, anx, any) = (self.x(a), self.y(a), self.x(an), self.y(an));
        let (bx, by, bnx, bny) = (self.x(b), self.y(b), self.x(bn), self.y(bn));
        if ay.max(any) <= by.min(bny) || ay.min(any) >= by.max(bny) || ax.max(anx) <= bx.min(bnx) || ax.min(anx) >= bx.max(bnx) {
            return Ok(());
        }
        use crate::util::java::double_to_string as d;
        if line_crosses_line(ax, ay, anx, any, bx, by, bnx, bny) {
            let a1 = any - ay;
            let b1 = ax - anx;
            let c1 = a1 * ax + b1 * ay;
            let a2 = bny - by;
            let b2 = bx - bnx;
            let c2 = a2 * bx + b2 * by;
            let det = a1 * b2 - a2 * b1;
            let x = (b2 * c1 - b1 * c2) / det;
            let y = (a1 * c2 - a2 * c1) / det;
            return Err(Error::illegal(format!("Polygon self-intersection at lat={} lon={}", d(y), d(x))));
        }
        if self.n[a as usize].next_edge_from_polygon
            && self.n[b as usize].next_edge_from_polygon
            && line_overlap_line(ax, ay, anx, any, bx, by, bnx, bny)
        {
            return Err(Error::illegal(format!("Polygon ring self-intersection at lat={} lon={}", d(ay), d(ax))));
        }
        Ok(())
    }
}

/// Lucene's `Polygon.verticesToGeoJSON`.
fn verticles_to_geojson(lats: &[f64], lons: &[f64]) -> String {
    use crate::util::java::double_to_string as d;
    let mut s = String::from("[");
    for i in 0..lats.len() {
        s.push('[');
        s.push_str(&d(lons[i]));
        s.push_str(", ");
        s.push_str(&d(lats[i]));
        s.push(']');
        if i != lats.len() - 1 {
            s.push_str(", ");
        }
    }
    s.push(']');
    s
}

/// Lucene's `XYPolygon.verticesToGeoJSON`.
fn xy_vertices_to_geojson(xs: &[f64], ys: &[f64]) -> String {
    use crate::util::java::float_to_string as f;
    let pts: Vec<String> = xs.iter().zip(ys).map(|(x, y)| format!("[{}, {}]", f(*x as f32), f(*y as f32))).collect();
    format!("[{}]", pts.join(", "))
}

/// Lucene's `Polygon.toString`.
fn lucene_polygon_string(p: &GeoPolygon) -> String {
    use crate::util::java::double_to_string as d;
    let mut s = String::new();
    for i in 0..p.lats.len() {
        s.push('[');
        s.push_str(&d(p.lats[i]));
        s.push_str(", ");
        s.push_str(&d(p.lons[i]));
        s.push_str("] ");
    }
    s
}

/// Lucene's `Tessellator.tessellate(polygon, checkSelfIntersections)`.
pub fn tessellate(poly: &GeoPolygon, check_self_intersections: bool) -> Result<Vec<Triangle>> {
    let total: usize = poly.num_points() + poly.holes.iter().map(|h| h.num_points()).sum::<usize>();
    // Work bound: quadratic in the vertices (ear clipping is), generous for real shapes.
    let budget = (total as u64 + 16).saturating_mul(total as u64 + 16).saturating_mul(64).max(1 << 20);
    let mut t = Tess { n: Vec::with_capacity(total * 2), out: Vec::with_capacity(total), budget, xy: poly.xy };
    let Some(mut outer) = t.create_list(&poly.lons, &poly.lats, poly.ccw, 0, true) else {
        return Err(Error::illegal("Malformed shape detected in Tessellator!"));
    };
    if outer == t.next(outer) || outer == t.next(t.next(outer)) {
        return Err(Error::illegal("at least three non-collinear points required"));
    }
    if !poly.holes.is_empty() {
        outer = t.eliminate_holes(poly, outer)?;
    }
    let morton = total > VERTEX_THRESHOLD;
    if morton {
        t.sort_by_morton(outer);
    }
    if check_self_intersections {
        t.check_intersection(outer, morton)?;
    }
    t.earcut(outer, State::Init, morton)?;
    if t.out.is_empty() {
        return Err(Error::illegal(MALFORMED));
    }
    Ok(t.out)
}

#[cfg(test)]
mod tests {
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    use super::*;
    use search_codec::points::encode::{decode_latitude, decode_longitude};

    fn ring(pts: &[(f64, f64)]) -> (Vec<f64>, Vec<f64>) {
        let mut lats: Vec<f64> = pts.iter().map(|p| p.1).collect();
        let mut lons: Vec<f64> = pts.iter().map(|p| p.0).collect();
        lats.push(pts[0].1);
        lons.push(pts[0].0);
        (lats, lons)
    }

    fn poly(pts: &[(f64, f64)], holes: &[&[(f64, f64)]]) -> GeoPolygon {
        let (lats, lons) = ring(pts);
        let hs = holes
            .iter()
            .map(|h| {
                let (a, b) = ring(h);
                GeoPolygon::new(a, b, vec![]).unwrap()
            })
            .collect();
        GeoPolygon::new(lats, lons, hs).unwrap()
    }

    fn tri_area(t: &Triangle) -> f64 {
        let x: Vec<f64> = t.x.iter().map(|&v| decode_longitude(v)).collect();
        let y: Vec<f64> = t.y.iter().map(|&v| decode_latitude(v)).collect();
        ((x[1] - x[0]) * (y[2] - y[0]) - (x[2] - x[0]) * (y[1] - y[0])).abs() / 2.0
    }

    fn ring_area(p: &[(f64, f64)]) -> f64 {
        let mut s = 0.0;
        for i in 0..p.len() {
            let j = (i + 1) % p.len();
            s += p[i].0 * p[j].1 - p[j].0 * p[i].1;
        }
        s.abs() / 2.0
    }

    #[test]
    fn squares_and_holes_cover_their_area() {
        let sq = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        for p in [poly(&sq, &[]), poly(&sq.iter().rev().copied().collect::<Vec<_>>(), &[])] {
            let t = tessellate(&p, true).unwrap();
            assert_eq!(t.len(), 2);
            let a: f64 = t.iter().map(tri_area).sum();
            assert!((a - 100.0).abs() < 1e-6, "{a}");
            // Every outer edge is from the polygon; the diagonal is not.
            let from: usize = t.iter().map(|t| t.from_polygon.iter().filter(|&&b| b).count()).sum();
            assert_eq!(from, 4);
        }
        let hole = [(2.0, 2.0), (2.0, 4.0), (4.0, 4.0), (4.0, 2.0)];
        let t = tessellate(&poly(&sq, &[&hole]), true).unwrap();
        let a: f64 = t.iter().map(tri_area).sum();
        assert!((a - 96.0).abs() < 1e-6, "{a}");
    }

    #[test]
    fn random_star_polygons_cover_their_area() {
        let mut rng = StdRng::seed_from_u64(3);
        for _ in 0..200 {
            // Star-shaped (simple) polygons around a center, small and large (z-order path).
            let n = if rng.gen_bool(0.5) { rng.gen_range(3..20) } else { rng.gen_range(81..300) };
            let (cx, cy) = (rng.gen_range(-100.0..100.0), rng.gen_range(-50.0..50.0));
            // Evenly spread angles (jittered less than half a step) keep the polygon simple.
            let step = std::f64::consts::TAU / n as f64;
            let angles: Vec<f64> = (0..n).map(|i| (i as f64 + rng.gen_range(-0.4..0.4)) * step).collect();
            let pts: Vec<(f64, f64)> = angles
                .iter()
                .map(|a| {
                    let r = rng.gen_range(1.0..20.0);
                    (cx + r * a.cos(), cy + r * a.sin())
                })
                .collect();
            if pts.len() < 3 {
                continue;
            }
            let p = poly(&pts, &[]);
            match tessellate(&p, false) {
                Ok(t) => {
                    let a: f64 = t.iter().map(tri_area).sum();
                    let want = ring_area(&pts);
                    assert!((a - want).abs() / want < 1e-4, "area {a} vs {want} ({} pts)", pts.len());
                }
                Err(e) => panic!("{e} for {} pts: {pts:?}", pts.len()),
            }
        }
    }

    #[test]
    fn malformed_shapes_fail_like_lucene() {
        let e = GeoPolygon::new(vec![0.0, 1.0, 0.0], vec![0.0, 1.0, 0.0], vec![]).unwrap_err();
        assert!(e.to_string().contains("at least 4 polygon points required"));
        let e = GeoPolygon::new(vec![0.0, 1.0, 1.0, 2.0], vec![0.0, 1.0, 2.0, 0.0], vec![]).unwrap_err();
        assert!(e.to_string().contains("it must close itself"), "{e}");
        // Collinear points.
        let p = poly(&[(0.0, 0.0), (1.0, 1.0), (2.0, 2.0)], &[]);
        let e = tessellate(&p, true).unwrap_err();
        assert!(e.to_string().contains("at least three non-collinear points required"), "{e}");
        // A bow tie crosses itself.
        let p = poly(&[(0.0, 0.0), (10.0, 10.0), (10.0, 0.0), (0.0, 10.0)], &[]);
        let e = tessellate(&p, true).unwrap_err();
        assert!(e.to_string().contains("Polygon self-intersection at lat=5.0 lon=5.0"), "{e}");
        // A hole outside the shell.
        let sq = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        let p = poly(&sq, &[&[(20.0, 20.0), (20.0, 22.0), (22.0, 22.0), (22.0, 20.0)]]);
        let e = tessellate(&p, true).unwrap_err();
        assert!(e.to_string().contains("Illegal hole detected"), "{e}");
    }
}
