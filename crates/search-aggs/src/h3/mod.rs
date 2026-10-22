//! H3, Uber's hexagonal hierarchical geospatial index, as Elasticsearch's own Java port
//! (`org.elasticsearch.h3`, Apache-2.0) computes it — `geohex_grid` keys must name the same
//! cells, including for points on cell edges, so the port follows Elasticsearch's arithmetic
//! (its vector-based face projection and jafama trigonometry, [`fast_math`]) rather than the
//! reference C library's.
//!
//! An index is a `u64`: mode, resolution, base cell and one 3-bit digit per resolution. The
//! public functions take and return degrees.

mod base_cells;
mod core;
pub mod ring;
pub mod fast_math;

pub use self::core::{CellBoundary, LatLng};

/// The finest resolution.
pub const MAX_RES: i32 = 15;

/// `H3.geoToH3(lat, lng, res)`: the cell containing the point (degrees), `res` in `0..=15`.
pub fn geo_to_h3(lat: f64, lon: f64, res: i32) -> u64 {
    debug_assert!((0..=MAX_RES).contains(&res));
    core::geo_to_h3(res, lat.to_radians(), lon.to_radians())
}

/// `H3.h3ToString`: lowercase hex, no leading zeros.
pub fn h3_to_string(h: u64) -> String {
    format!("{h:x}")
}

/// `H3.stringToH3` (`Long.parseUnsignedLong(address, 16)`).
pub fn string_to_h3(address: &str) -> Option<u64> {
    if address.is_empty() || address.starts_with('+') {
        return None;
    }
    u64::from_str_radix(address, 16).ok()
}

/// The cell's resolution.
pub fn resolution(h: u64) -> i32 {
    core::get_resolution(h)
}

/// Whether the cell is one of the 12 pentagons of its resolution.
pub fn is_pentagon(h: u64) -> bool {
    core::is_pentagon(h)
}

/// `H3.h3ToGeoBoundary`: the cell's vertices (radians), counter-clockwise.
pub fn h3_to_geo_boundary(h: u64) -> CellBoundary {
    core::h3_to_geo_boundary(h)
}

/// `H3.h3ToLatLng`: the cell's center (radians).
pub fn h3_to_lat_lng(h: u64) -> LatLng {
    core::h3_to_lat_lng(h)
}

/// The cells containing the poles, per resolution (`H3.northPolarH3` / `southPolarH3`).
fn polar(north: bool, res: i32) -> u64 {
    use std::sync::OnceLock;
    static P: OnceLock<[[u64; 16]; 2]> = OnceLock::new();
    let p = P.get_or_init(|| {
        let mut p = [[0; 16]; 2];
        for res in 0..=MAX_RES {
            p[0][res as usize] = geo_to_h3(90.0, 0.0, res);
            p[1][res as usize] = geo_to_h3(-90.0, 0.0, res);
        }
        p
    });
    p[usize::from(!north)][res as usize]
}

/// The cell's latitude/longitude bounding box in degrees, `(top, left, bottom, right)`, as
/// Elasticsearch bounds a cell for `geohex_grid`'s `bounds` filter: polar cells span every
/// longitude from their farthest vertex to the pole; other cells take their vertices' extremes,
/// widen the latitude extremes to where the great-circle edges bulge past the extreme vertex,
/// and wrap the longitudes across the dateline when the vertices span more than half the globe
/// (then `left > right`).
pub fn cell_bounds(h: u64) -> (f64, f64, f64, f64) {
    let boundary = h3_to_geo_boundary(h);
    let pts = &boundary.points;
    let res = resolution(h);
    if h == polar(true, res) {
        let min_lat = pts.iter().fold(f64::INFINITY, |m, p| m.min(p.lat));
        return (90.0, -180.0, min_lat.to_degrees(), 180.0);
    }
    if h == polar(false, res) {
        let max_lat = pts.iter().fold(f64::NEG_INFINITY, |m, p| m.max(p.lat));
        return (max_lat.to_degrees(), -180.0, -90.0, 180.0);
    }
    let n = pts.len();
    let (mut min_lat, mut max_lat) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut min_at, mut max_at) = (0, 0);
    let (mut min_lon, mut max_lon) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut max_neg_lon, mut min_pos_lon) = (f64::NEG_INFINITY, f64::INFINITY);
    for (i, p) in pts.iter().enumerate() {
        if p.lat < min_lat {
            min_lat = p.lat;
            min_at = i;
        }
        if p.lat > max_lat {
            max_lat = p.lat;
            max_at = i;
        }
        min_lon = min_lon.min(p.lon);
        max_lon = max_lon.max(p.lon);
        if p.lon < 0.0 {
            max_neg_lon = max_neg_lon.max(p.lon);
        } else {
            min_pos_lon = min_pos_lon.min(p.lon);
        }
    }
    let next = |i: usize| (i + 1) % n;
    let prev = |i: usize| if i == 0 { n - 1 } else { i - 1 };
    if min_lat < 0.0 {
        let v = pts[min_at];
        min_lat = v.great_circle_min_latitude(&pts[next(min_at)]).min(v.great_circle_min_latitude(&pts[prev(min_at)]));
    }
    if max_lat > 0.0 {
        let v = pts[max_at];
        max_lat = v.great_circle_max_latitude(&pts[next(max_at)]).max(v.great_circle_max_latitude(&pts[prev(max_at)]));
    }
    let crosses = max_lon - min_lon > std::f64::consts::PI;
    let (left, right) = if crosses { (min_pos_lon, max_neg_lon) } else { (min_lon, max_lon) };
    (max_lat.to_degrees(), left.to_degrees(), min_lat.to_degrees(), right.to_degrees())
}
