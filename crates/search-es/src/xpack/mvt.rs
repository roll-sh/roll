//! Vector tiles: `GET|POST /<index>/_mvt/<field>/<zoom>/<x>/<y>` (`search_mvt`).
//!
//! Like Elasticsearch's `RestVectorTileAction`, the request becomes one search over the tile:
//! a `geo_shape` intersects filter on the tile's bounding box (AND the user query), the hits'
//! geometries fetched through the fields API, a `geotile_grid` (or `geohex_grid`) aggregation
//! bounded to the tile with the user's metric aggregations below it, a `stats_bucket` per
//! metric for the meta layer, and an optional `geo_bounds`. The response is a Mapbox Vector
//! Tile (protobuf, version 2) with up to three layers:
//!
//! * `hits`: one feature per hit geometry, tagged `_id`, `_index` and the requested fields;
//! * `aggs`: one feature per grid cell (its box, its center or the documents' centroid), tagged
//!   `_key`, `_count` and the flattened metric results;
//! * `meta`: the tile (or the exact bounds of the hits) tagged with the flattened search
//!   response (`hits.total.value`, `_shards.*`, `aggregations.<metric>.<stat>`, ...).
//!
//! Points and boxes are encoded exactly like Elasticsearch's `SimpleFeatureFactory` (spherical
//! mercator through the same sloppy math, rounding, zig-zag commands), and property values
//! keep Elasticsearch's protobuf types (bucket counts as 64-bit `sint`, response numbers as they
//! come out of its CBOR round trip, a hit field typed by its mapping), so those tiles match it
//! byte for byte. Lines and polygons are clipped to the tile plus its buffer
//! (Sutherland–Hodgman for rings, per segment for lines) and snapped to the pixel grid, where
//! Elasticsearch runs JTS clipping and topology-preserving simplification: the geometry is the
//! same up to vertices JTS would have dropped as redundant within a pixel.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::{enc, internal};

const MERCATOR_BOUNDS: f64 = 20_037_508.34;
const MERCATOR_FACTOR: f64 = MERCATOR_BOUNDS / 180.0;
const MAX_ZOOM: i64 = 29;
const MIME_TYPE: &str = "application/vnd.mapbox-vector-tile";
const INTERNAL_PREFIX: &str = "_mvt_";
const CENTROID_AGG: &str = "_mvt_centroid";

/// `GridAggregation.GEOHEX`'s zoom → H3 resolution.
const ZOOM2RESOLUTION: [i32; 30] = [0, 0, 0, 1, 1, 2, 2, 3, 3, 3, 4, 4, 5, 6, 6, 7, 8, 9, 9, 10, 11, 11, 12, 13, 14, 14, 15, 15, 15, 15];

/// Metric aggregations (`MetricsAggregationBuilder`s) and the metrics `stats_bucket` reads.
fn metric_names(ty: &str) -> Option<&'static [&'static str]> {
    Some(match ty {
        "avg" | "sum" | "min" | "max" | "value_count" | "cardinality" | "median_absolute_deviation" | "weighted_avg" => &["value"],
        "stats" => &["count", "min", "max", "avg", "sum"],
        "extended_stats" => &[
            "count",
            "min",
            "max",
            "avg",
            "sum",
            "sum_of_squares",
            "variance",
            "variance_population",
            "variance_sampling",
            "std_deviation",
            "std_deviation_population",
            "std_deviation_sampling",
        ],
        "percentiles" | "percentile_ranks" | "geo_bounds" | "geo_centroid" | "top_hits" | "top_metrics" | "scripted_metric" | "string_stats"
        | "boxplot" | "t_test" | "matrix_stats" | "geo_line" | "cartesian_bounds" | "cartesian_centroid" => &[],
        _ => return None,
    })
}

fn is_pipeline(ty: &str) -> bool {
    ty.ends_with("_bucket")
        || matches!(
            ty,
            "derivative" | "cumulative_sum" | "cumulative_cardinality" | "moving_avg" | "moving_fn" | "moving_percentiles" | "serial_diff"
                | "bucket_script" | "bucket_selector" | "bucket_sort" | "normalize" | "inference"
        )
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GridAgg {
    GeoTile,
    GeoHex,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GridType {
    Grid,
    Point,
    Centroid,
}

fn grid_agg_of(s: &str) -> EsResult<GridAgg> {
    match s.to_ascii_lowercase().as_str() {
        "geotile" => Ok(GridAgg::GeoTile),
        "geohex" => Ok(GridAgg::GeoHex),
        _ => Err(EsError::illegal_argument(format!("Invalid agg type [{s}]"))),
    }
}

fn grid_type_of(s: &str) -> EsResult<GridType> {
    match s.to_ascii_lowercase().as_str() {
        "grid" => Ok(GridType::Grid),
        "point" => Ok(GridType::Point),
        "centroid" => Ok(GridType::Centroid),
        _ => Err(EsError::illegal_argument(format!("Invalid grid type [{s}]"))),
    }
}

/// `Integer.parseInt` of a path parameter.
fn parse_int(s: &str) -> EsResult<i64> {
    crate::units::parse_java_int(s).map(i64::from).ok_or_else(|| EsError::java(400, "number_format_exception", format!("For input string: \"{s}\"")))
}

/// A tile's bounding box in degrees.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    min_lon: f64,
    max_lon: f64,
    min_lat: f64,
    max_lat: f64,
}

/// `GeoTileUtils.toBoundingBox(x, y, z)`, validating the combination.
fn tile_bbox(z: i64, x: i64, y: i64) -> EsResult<Rect> {
    if !(0..=MAX_ZOOM).contains(&z) {
        return Err(EsError::illegal_argument(format!("Invalid geotile_grid precision of {z}. Must be between 0 and {MAX_ZOOM}.")));
    }
    let tiles = 1i64 << z;
    if x < 0 || y < 0 || x >= tiles || y >= tiles {
        return Err(EsError::illegal_argument(format!("Zoom/X/Y combination is not valid: {z}/{x}/{y}")));
    }
    Ok(Rect {
        min_lon: search_aggs::geo::tile_to_lon(x, tiles),
        max_lon: search_aggs::geo::tile_to_lon(x + 1, tiles),
        max_lat: search_aggs::geo::tile_to_lat(y, tiles),
        min_lat: search_aggs::geo::tile_to_lat(y + 1, tiles),
    })
}

/// `GeoTileUtils.tileToLat(double, int)` (the tile center's latitude).
fn tile_to_lat_f(y: f64, tiles: f64) -> f64 {
    let n = std::f64::consts::PI - (std::f64::consts::PI * 2.0 * y) / tiles;
    search_aggs::jmath::fast_atan(search_aggs::jmath::fast_sinh(n)).to_degrees()
}

fn lon_to_mercator(lon: f64) -> f64 {
    lon * MERCATOR_FACTOR
}

/// `SphericalMercatorUtils.latToSphericalMercator` (Lucene's sloppy cosine, Elasticsearch's
/// fast log).
fn lat_to_mercator(lat: f64) -> f64 {
    let cos = search_index::util::geo::sloppy_cos((90.0 + lat) * std::f64::consts::PI / 360.0);
    // `Math.tan(Math.nextUp(0.0))`: the smallest positive double.
    let tan = ((1.0 - cos * cos).sqrt() / cos).max(f64::from_bits(1));
    let y = search_aggs::jmath::fast_log(tan) / (std::f64::consts::PI / 180.0);
    y * MERCATOR_FACTOR
}

/// Projects lon/lat to the tile's pixel grid (`SimpleFeatureFactory`).
struct Projection {
    extent: i64,
    x_scale: f64,
    y_scale: f64,
    x_translate: f64,
    y_translate: f64,
    /// The clip envelope in mercator meters: the tile grown by the buffer.
    clip: [f64; 4],
}

impl Projection {
    fn new(z: i64, tile: Rect, extent: i64, buffer: i64) -> Projection {
        let (min_x, max_x) = (lon_to_mercator(tile.min_lon), lon_to_mercator(tile.max_lon));
        let (min_y, max_y) = (lat_to_mercator(tile.min_lat), lat_to_mercator(tile.max_lat));
        let x_scale = extent as f64 / (max_x - min_x);
        let y_scale = -(extent as f64) / (max_y - min_y);
        let pixel = 2.0 * MERCATOR_BOUNDS / ((1i64 << z) as f64 * extent.max(1) as f64);
        let pad = buffer as f64 * pixel;
        Projection {
            extent,
            x_scale,
            y_scale,
            x_translate: -x_scale * min_x,
            y_translate: -y_scale * min_y,
            clip: [min_x - pad, min_y - pad, max_x + pad, max_y + pad],
        }
    }

    fn px(&self, mx: f64) -> i64 {
        (self.x_scale * mx + self.x_translate).round() as i64
    }

    fn py(&self, my: f64) -> i64 {
        (self.y_scale * my + self.y_translate).round() as i64 + self.extent
    }

    fn lon(&self, lon: f64) -> i64 {
        self.px(lon_to_mercator(lon))
    }

    fn lat(&self, lat: f64) -> i64 {
        self.py(lat_to_mercator(lat))
    }

    /// `SimpleFeatureFactory.point`: nothing outside the tile.
    fn point(&self, lon: f64, lat: f64) -> Option<Geom> {
        let x = self.lon(lon);
        if x > self.extent || x < 0 {
            return None;
        }
        let y = self.lat(lat);
        if y > self.extent || y < 0 {
            return None;
        }
        Some(Geom::points(&[(x, y)]))
    }

    /// `SimpleFeatureFactory.points`: sorted by lon then lat, repeated pixels dropped.
    fn points(&self, pts: &mut [(f64, f64)]) -> Option<Geom> {
        pts.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
        let mut px: Vec<(i64, i64)> = Vec::with_capacity(pts.len());
        for &(lon, lat) in pts.iter() {
            let x = self.lon(lon);
            if x > self.extent || x < 0 {
                continue;
            }
            let y = self.lat(lat);
            if y > self.extent || y < 0 {
                continue;
            }
            if px.last() != Some(&(x, y)) {
                px.push((x, y));
            }
        }
        (!px.is_empty()).then(|| Geom::points(&px))
    }

    /// `SimpleFeatureFactory.box`.
    fn rect(&self, min_lon: f64, max_lon: f64, min_lat: f64, max_lat: f64) -> Option<Geom> {
        let min_x = self.lon(min_lon).max(0);
        if min_x > self.extent {
            return None;
        }
        let min_y = self.lat(min_lat).min(self.extent);
        let max_x = self.lon(max_lon).min(self.extent);
        if max_x < 0 {
            return None;
        }
        let max_y = self.lat(max_lat).max(0);
        Some(if min_x == max_x {
            if min_y == max_y { Geom::points(&[(min_x, min_y)]) } else { Geom::line(&[(min_x, min_y), (min_x, max_y)]) }
        } else if min_y == max_y {
            Geom::line(&[(min_x, min_y), (max_x, min_y)])
        } else {
            let mut g = Geom { ty: 3, cmds: Vec::with_capacity(11) };
            g.cmds.push(command(1, 1));
            g.cmds.push(zigzag(min_x));
            g.cmds.push(zigzag(min_y));
            g.cmds.push(command(2, 3));
            g.cmds.extend([zigzag(0), zigzag(max_y - min_y), zigzag(max_x - min_x), zigzag(0), zigzag(0), zigzag(min_y - max_y)]);
            g.cmds.push(command(7, 1));
            g
        })
    }
}

fn command(id: u32, count: usize) -> u32 {
    (id & 0x7) | ((count as u32) << 3)
}

fn zigzag(v: i64) -> u32 {
    ((v << 1) ^ (v >> 63)) as u32
}

/// An encoded feature geometry: type (1 point, 2 line, 3 polygon) and commands.
#[derive(Clone, Debug, PartialEq)]
struct Geom {
    ty: u32,
    cmds: Vec<u32>,
}

impl Geom {
    fn points(px: &[(i64, i64)]) -> Geom {
        let mut cmds = Vec::with_capacity(1 + 2 * px.len());
        cmds.push(command(1, px.len()));
        let (mut cx, mut cy) = (0, 0);
        for &(x, y) in px {
            cmds.push(zigzag(x - cx));
            cmds.push(zigzag(y - cy));
            (cx, cy) = (x, y);
        }
        Geom { ty: 1, cmds }
    }

    fn line(px: &[(i64, i64)]) -> Geom {
        let mut g = Geom { ty: 2, cmds: Vec::new() };
        let mut cursor = (0, 0);
        g.push_path(px, false, &mut cursor);
        g
    }

    /// Appends a MoveTo + LineTo path (closed with ClosePath for rings, whose last point,
    /// equal to the first, is left out).
    fn push_path(&mut self, px: &[(i64, i64)], ring: bool, cursor: &mut (i64, i64)) {
        let pts = if ring && px.len() > 1 && px.first() == px.last() { &px[..px.len() - 1] } else { px };
        if pts.is_empty() {
            return;
        }
        self.cmds.push(command(1, 1));
        self.cmds.push(zigzag(pts[0].0 - cursor.0));
        self.cmds.push(zigzag(pts[0].1 - cursor.1));
        *cursor = pts[0];
        if pts.len() > 1 {
            self.cmds.push(command(2, pts.len() - 1));
            for &(x, y) in &pts[1..] {
                self.cmds.push(zigzag(x - cursor.0));
                self.cmds.push(zigzag(y - cursor.1));
                *cursor = (x, y);
            }
        }
        if ring {
            self.cmds.push(command(7, 1));
        }
    }
}

/// Lines and polygons in mercator meters, clipped and snapped to pixels.
mod shapes {
    use super::Projection;

    pub type Ring = Vec<[f64; 2]>;

    /// Sutherland–Hodgman against the clip envelope.
    pub fn clip_ring(ring: &[[f64; 2]], c: [f64; 4]) -> Ring {
        let mut out: Ring = ring.to_vec();
        if out.len() > 1 && out.first() == out.last() {
            out.pop();
        }
        // (axis, bound, keep-if-greater)
        for (axis, bound, greater) in [(0, c[0], true), (1, c[1], true), (0, c[2], false), (1, c[3], false)] {
            if out.is_empty() {
                break;
            }
            let input = std::mem::take(&mut out);
            let inside = |p: &[f64; 2]| if greater { p[axis] >= bound } else { p[axis] <= bound };
            let mut prev = input[input.len() - 1];
            for &cur in &input {
                let (ci, pi) = (inside(&cur), inside(&prev));
                if ci != pi {
                    let t = (bound - prev[axis]) / (cur[axis] - prev[axis]);
                    let mut p = [prev[0] + t * (cur[0] - prev[0]), prev[1] + t * (cur[1] - prev[1])];
                    p[axis] = bound;
                    out.push(p);
                }
                if ci {
                    out.push(cur);
                }
                prev = cur;
            }
        }
        out
    }

    /// Clips a line to the envelope (Liang–Barsky per segment), splitting it where it leaves.
    pub fn clip_line(line: &[[f64; 2]], c: [f64; 4]) -> Vec<Ring> {
        let mut parts: Vec<Ring> = Vec::new();
        let mut cur: Ring = Vec::new();
        for w in line.windows(2) {
            let (a, b) = (w[0], w[1]);
            let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
            let (mut t0, mut t1) = (0.0f64, 1.0f64);
            let mut visible = true;
            for (p, q) in [(-dx, a[0] - c[0]), (dx, c[2] - a[0]), (-dy, a[1] - c[1]), (dy, c[3] - a[1])] {
                if p == 0.0 {
                    if q < 0.0 {
                        visible = false;
                        break;
                    }
                } else {
                    let r = q / p;
                    if p < 0.0 {
                        t0 = t0.max(r);
                    } else {
                        t1 = t1.min(r);
                    }
                }
            }
            if !visible || t0 > t1 {
                if !cur.is_empty() {
                    parts.push(std::mem::take(&mut cur));
                }
                continue;
            }
            // Unclipped ends stay exact, so consecutive segments chain.
            let s = if t0 == 0.0 { a } else { [a[0] + t0 * dx, a[1] + t0 * dy] };
            let e = if t1 == 1.0 { b } else { [a[0] + t1 * dx, a[1] + t1 * dy] };
            if cur.last() != Some(&s) {
                if !cur.is_empty() {
                    parts.push(std::mem::take(&mut cur));
                }
                cur.push(s);
            }
            cur.push(e);
            if t1 < 1.0 {
                parts.push(std::mem::take(&mut cur));
            }
        }
        if !cur.is_empty() {
            parts.push(cur);
        }
        parts
    }

    /// Snaps to pixels, dropping repeated points.
    pub fn snap(p: &Projection, ring: &[[f64; 2]]) -> Vec<(i64, i64)> {
        let mut out: Vec<(i64, i64)> = Vec::with_capacity(ring.len());
        for q in ring {
            let v = (p.px(q[0]), p.py(q[1]));
            if out.last() != Some(&v) {
                out.push(v);
            }
        }
        out
    }

    /// Twice the signed area in pixel space (y down): positive for MVT exterior rings.
    pub fn area2(r: &[(i64, i64)]) -> i64 {
        let n = r.len();
        (0..n).map(|i| r[i].0 * r[(i + 1) % n].1 - r[(i + 1) % n].0 * r[i].1).sum()
    }
}

/// A GeoJSON position's `(lon, lat)`.
fn xy(q: &Value) -> Option<(f64, f64)> {
    let a = q.as_array()?;
    Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))
}

/// Encodes a GeoJSON geometry (as the fields API returns it) for the tile.
fn shape_geom(p: &Projection, v: &Value) -> Vec<Geom> {
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    let coords = v.get("coordinates");
    let pt = |c: &Value| -> Option<[f64; 2]> {
        let a = c.as_array()?;
        Some([lon_to_mercator(a.first()?.as_f64()?), lat_to_mercator(a.get(1)?.as_f64()?)])
    };
    let line = |c: &Value| -> Vec<[f64; 2]> { c.as_array().into_iter().flatten().filter_map(pt).collect() };
    let lines_geom = |ls: Vec<Vec<[f64; 2]>>| -> Option<Geom> {
        let mut g = Geom { ty: 2, cmds: Vec::new() };
        let mut cursor = (0, 0);
        for l in ls {
            for part in shapes::clip_line(&l, p.clip) {
                let px = shapes::snap(p, &part);
                if px.len() >= 2 {
                    g.push_path(&px, false, &mut cursor);
                }
            }
        }
        (!g.cmds.is_empty()).then_some(g)
    };
    let polygon_geom = |polys: Vec<Vec<Vec<[f64; 2]>>>| -> Option<Geom> {
        let mut g = Geom { ty: 3, cmds: Vec::new() };
        let mut cursor = (0, 0);
        for rings in polys {
            for (i, r) in rings.iter().enumerate() {
                let clipped = shapes::clip_ring(r, p.clip);
                let mut px = shapes::snap(p, &clipped);
                if px.len() > 1 && px.first() == px.last() {
                    px.pop();
                }
                if px.len() < 3 {
                    if i == 0 {
                        break;
                    }
                    continue;
                }
                let a = shapes::area2(&px);
                if a == 0 {
                    continue;
                }
                // Exterior rings positive, holes negative (MVT 2.1 winding).
                // Reversed like JTS's `LinearRing.reverse`: the first vertex stays first.
                if (i == 0) != (a > 0) {
                    px[1..].reverse();
                }
                g.push_path(&px, true, &mut cursor);
            }
        }
        (!g.cmds.is_empty()).then_some(g)
    };
    let Some(c) = coords else {
        if ty == "geometrycollection"
            && let Some(Value::Arr(gs)) = v.get("geometries")
        {
            return gs.iter().flat_map(|g| shape_geom(p, g)).collect();
        }
        return Vec::new();
    };
    let geom = match ty.as_str() {
        "point" => c.as_array().and_then(|a| Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))).and_then(|(lon, lat)| p.points(&mut [(lon, lat)])),
        "multipoint" => {
            let mut pts: Vec<(f64, f64)> = c.as_array().into_iter().flatten().filter_map(xy).collect();
            p.points(&mut pts)
        }
        "linestring" => lines_geom(vec![line(c)]),
        "multilinestring" => lines_geom(c.as_array().into_iter().flatten().map(line).collect()),
        "polygon" => polygon_geom(vec![c.as_array().into_iter().flatten().map(line).collect()]),
        "multipolygon" => polygon_geom(c.as_array().into_iter().flatten().map(|poly| poly.as_array().into_iter().flatten().map(line).collect()).collect()),
        "envelope" => {
            let a = c.as_array();
            let corner = |i: usize| a.and_then(|a| a.get(i)).and_then(xy);
            match (corner(0), corner(1)) {
                (Some((min_lon, max_lat)), Some((max_lon, min_lat))) => {
                    let ring = vec![[min_lon, min_lat], [max_lon, min_lat], [max_lon, max_lat], [min_lon, max_lat], [min_lon, min_lat]]
                        .into_iter()
                        .map(|q| [lon_to_mercator(q[0]), lat_to_mercator(q[1])])
                        .collect();
                    polygon_geom(vec![vec![ring]])
                }
                _ => None,
            }
        }
        _ => None,
    };
    geom.into_iter().collect()
}

/// A tag value with the Java type Elasticsearch's `MvtLayerProps` keys it by (an `Integer` and
/// a `Long` of the same value are two entries).
#[derive(Clone, Debug, PartialEq)]
enum Prop {
    Str(String),
    Int(i64),
    Long(i64),
    Float(f32),
    Double(f64),
    Bool(bool),
}

impl Prop {
    fn same(&self, o: &Prop) -> bool {
        match (self, o) {
            (Prop::Float(a), Prop::Float(b)) => a.to_bits() == b.to_bits(),
            (Prop::Double(a), Prop::Double(b)) => a.to_bits() == b.to_bits(),
            (a, b) => a == b,
        }
    }

    /// A number as Elasticsearch reads it back from CBOR: integers as `Integer` when they fit.
    fn from_json(v: &Value) -> Option<Prop> {
        Some(match v {
            Value::Str(s) => Prop::Str(s.clone()),
            Value::Bool(b) => Prop::Bool(*b),
            Value::Num(n) if n.is_integer() => {
                let i = n.as_i64()?;
                if i32::try_from(i).is_ok() { Prop::Int(i) } else { Prop::Long(i) }
            }
            Value::Num(n) => Prop::Double(n.as_f64()),
            _ => return None,
        })
    }
}

/// A layer's features and its shared key and value tables.
struct Layer {
    name: &'static str,
    extent: i64,
    keys: Vec<String>,
    values: Vec<Prop>,
    features: Vec<(Vec<u32>, Geom)>,
}

impl Layer {
    fn new(name: &'static str, extent: i64) -> Layer {
        Layer { name, extent, keys: Vec::new(), values: Vec::new(), features: Vec::new() }
    }

    fn tag(&mut self, tags: &mut Vec<u32>, key: &str, v: Prop) {
        let k = match self.keys.iter().position(|x| x == key) {
            Some(i) => i,
            None => {
                self.keys.push(key.to_string());
                self.keys.len() - 1
            }
        };
        let i = match self.values.iter().position(|x| x.same(&v)) {
            Some(i) => i,
            None => {
                self.values.push(v);
                self.values.len() - 1
            }
        };
        tags.push(k as u32);
        tags.push(i as u32);
    }

}

/// Protobuf writing (`vector_tile.proto` v2).
mod pb {
    pub fn varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    pub fn key(out: &mut Vec<u8>, field: u32, wire: u32) {
        varint(out, ((field << 3) | wire) as u64);
    }

    pub fn bytes(out: &mut Vec<u8>, field: u32, b: &[u8]) {
        key(out, field, 2);
        varint(out, b.len() as u64);
        out.extend_from_slice(b);
    }

    pub fn packed(out: &mut Vec<u8>, field: u32, vals: &[u32]) {
        if vals.is_empty() {
            return;
        }
        let mut buf = Vec::with_capacity(vals.len() * 2);
        for &v in vals {
            varint(&mut buf, v as u64);
        }
        bytes(out, field, &buf);
    }
}

impl Layer {
    fn encode(&self, out: &mut Vec<u8>) {
        let mut l = Vec::new();
        pb::bytes(&mut l, 1, self.name.as_bytes());
        for (tags, g) in &self.features {
            let mut f = Vec::new();
            pb::packed(&mut f, 2, tags);
            pb::key(&mut f, 3, 0);
            pb::varint(&mut f, g.ty as u64);
            pb::packed(&mut f, 4, &g.cmds);
            pb::bytes(&mut l, 2, &f);
        }
        for k in &self.keys {
            pb::bytes(&mut l, 3, k.as_bytes());
        }
        for v in &self.values {
            let mut b = Vec::new();
            match v {
                Prop::Str(s) => pb::bytes(&mut b, 1, s.as_bytes()),
                Prop::Float(x) => {
                    pb::key(&mut b, 2, 5);
                    b.extend_from_slice(&x.to_le_bytes());
                }
                Prop::Double(x) => {
                    pb::key(&mut b, 3, 1);
                    b.extend_from_slice(&x.to_le_bytes());
                }
                Prop::Int(i) | Prop::Long(i) => {
                    pb::key(&mut b, 6, 0);
                    pb::varint(&mut b, ((i << 1) ^ (i >> 63)) as u64);
                }
                Prop::Bool(x) => {
                    pb::key(&mut b, 7, 0);
                    pb::varint(&mut b, *x as u64);
                }
            }
            pb::bytes(&mut l, 4, &b);
        }
        pb::key(&mut l, 5, 0);
        pb::varint(&mut l, self.extent as u64);
        pb::key(&mut l, 15, 0);
        pb::varint(&mut l, 2);
        pb::bytes(out, 3, &l);
    }
}

/// The parsed request (`VectorTileRequest`).
struct Request {
    field: String,
    z: i64,
    x: i64,
    y: i64,
    tile: Rect,
    size: i64,
    fields: Vec<Value>,
    query: Option<Value>,
    runtime_mappings: Option<Value>,
    aggs: Map,
    sort: Option<Value>,
    grid_agg: GridAgg,
    grid_precision: i64,
    grid_type: GridType,
    extent: i64,
    buffer: i64,
    exact_bounds: bool,
    with_labels: bool,
    track_total_hits: Value,
}

fn parse_request(ctx: &Ctx) -> EsResult<Request> {
    let field = ctx.path_param("field").unwrap_or_default().to_string();
    let z = parse_int(ctx.path_param("zoom").unwrap_or_default())?;
    let x = parse_int(ctx.path_param("x").unwrap_or_default())?;
    let y = parse_int(ctx.path_param("y").unwrap_or_default())?;
    let tile = tile_bbox(z, x, y)?;
    let mut r = Request {
        field,
        z,
        x,
        y,
        tile,
        size: 10_000,
        fields: Vec::new(),
        query: None,
        runtime_mappings: None,
        aggs: Map::new(),
        sort: None,
        grid_agg: GridAgg::GeoTile,
        grid_precision: 8,
        grid_type: GridType::Grid,
        extent: 4096,
        buffer: 5,
        exact_bounds: false,
        with_labels: false,
        track_total_hits: Value::from(10_000i64),
    };
    let body = ctx.body_map()?.unwrap_or_default();
    // `[l:c] [vector-tile] failed to parse field [k]` at the value (objects: their end).
    let failed = |k: &str, e: EsError, at_end: bool| {
        let (l, c) = if at_end {
            ctx.raw_json().and_then(|s| crate::json::locate::value_end(s, &[crate::json::locate::Seg::Key(k)])).unwrap_or((1, 1))
        } else {
            crate::xpack::value_loc(ctx, k)
        };
        EsError::x_content_parse(l, c, format!("[vector-tile] failed to parse field [{k}]")).caused_by(e)
    };
    let int_of = |k: &str, v: &Value| -> EsResult<i64> {
        match v {
            Value::Num(n) => n.as_i64().ok_or_else(|| failed(k, EsError::illegal_argument(format!("[{k}] must be an integer")), false)),
            Value::Str(s) => crate::units::parse_java_int(s).map(i64::from).ok_or_else(|| failed(k, crate::rest::params::nfe(s), false)),
            _ => Err(failed(k, EsError::illegal_argument(format!("[{k}] must be an integer")), false)),
        }
    };
    for (k, v) in &body {
        match k.as_str() {
            "size" => {
                r.size = int_of(k, v)?;
                if r.size < 0 {
                    return Err(failed(k, EsError::illegal_argument(format!("[size] parameter cannot be negative, found [{}]", r.size)), false));
                }
            }
            "fields" => r.fields = v.as_array().cloned().unwrap_or_else(|| vec![v.clone()]),
            "query" => r.query = Some(v.clone()),
            "runtime_mappings" => r.runtime_mappings = Some(v.clone()),
            "aggs" | "aggregations" => {
                let Value::Obj(m) = v else { return Err(failed(k, EsError::illegal_argument("aggregations must be an object"), true)) };
                for (name, def) in m {
                    let ty = def.as_object().and_then(|d| d.keys().find(|t| !matches!(t.as_str(), "meta" | "aggs" | "aggregations"))).cloned().unwrap_or_default();
                    if is_pipeline(&ty) {
                        return Err(failed(k, EsError::illegal_argument(format!("Unsupported pipeline aggregation of type [{ty}]")), true));
                    }
                    if metric_names(&ty).is_none() {
                        return Err(failed(
                            k,
                            EsError::illegal_argument(format!("Unsupported aggregation of type [{ty}].Only metric aggregations are supported.")),
                            true,
                        ));
                    }
                    r.aggs.insert(name.clone(), def.clone());
                }
            }
            "sort" => r.sort = Some(v.clone()),
            "grid_agg" => r.grid_agg = grid_agg_of(&v.to_es_string()).map_err(|e| failed(k, e, false))?,
            "grid_type" => r.grid_type = grid_type_of(&v.to_es_string()).map_err(|e| failed(k, e, false))?,
            "grid_precision" => r.grid_precision = check_precision(int_of(k, v)?).map_err(|e| failed(k, e, false))?,
            "extent" => r.extent = check_non_negative("extent", int_of(k, v)?).map_err(|e| failed(k, e, false))?,
            "buffer" => r.buffer = check_non_negative("buffer", int_of(k, v)?).map_err(|e| failed(k, e, false))?,
            "exact_bounds" => r.exact_bounds = bool_of(v).ok_or_else(|| failed(k, EsError::illegal_argument("expected a boolean"), false))?,
            "with_labels" => r.with_labels = bool_of(v).ok_or_else(|| failed(k, EsError::illegal_argument("expected a boolean"), false))?,
            "track_total_hits" => r.track_total_hits = v.clone(),
            other => return Err(crate::xpack::unknown_field(ctx, "vector-tile", other)),
        }
    }
    // URL parameters win over the body.
    if ctx.has_query("size") {
        r.size = ctx.int_param("size", 10_000)?;
        if r.size < 0 {
            return Err(EsError::illegal_argument(format!("[size] parameter cannot be negative, found [{}]", r.size)));
        }
    }
    if ctx.has_query("grid_precision") {
        r.grid_precision = check_precision(ctx.int_param("grid_precision", 8)?)?;
    }
    if ctx.has_query("extent") {
        r.extent = check_non_negative("extent", ctx.int_param("extent", 4096)?)?;
    }
    if let Some(a) = ctx.query("grid_agg") {
        r.grid_agg = grid_agg_of(a)?;
    }
    if let Some(t) = ctx.query("grid_type") {
        r.grid_type = grid_type_of(t)?;
    }
    if ctx.has_query("exact_bounds") {
        r.exact_bounds = ctx.bool_param("exact_bounds", false)?;
    }
    if ctx.has_query("with_labels") {
        r.with_labels = ctx.bool_param("with_labels", false)?;
    }
    if let Some(t) = ctx.query("track_total_hits") {
        r.track_total_hits = match t {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            n => Value::from(crate::units::parse_java_int(n).map(i64::from).ok_or_else(|| crate::rest::params::nfe(n))?),
        };
    }
    for name in r.aggs.keys() {
        if name.starts_with(INTERNAL_PREFIX) {
            return Err(EsError::illegal_argument(format!("Invalid aggregation name [{name}]. Aggregation names cannot start with prefix '{INTERNAL_PREFIX}'")));
        }
    }
    Ok(r)
}

fn bool_of(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Str(s) if s == "true" => Some(true),
        Value::Str(s) if s == "false" => Some(false),
        _ => None,
    }
}

fn check_precision(p: i64) -> EsResult<i64> {
    if p < 0 {
        return Err(EsError::illegal_argument(format!("[gridPrecision] parameter cannot be negative, found [{p}]")));
    }
    if p > 8 {
        return Err(EsError::illegal_argument(format!("[gridPrecision] parameter cannot be bigger than 8, found [{p}]")));
    }
    Ok(p)
}

fn check_non_negative(name: &str, v: i64) -> EsResult<i64> {
    if v < 0 {
        return Err(EsError::illegal_argument(format!("[{name}] parameter cannot be negative, found [{v}]")));
    }
    Ok(v)
}

impl Request {
    fn agg_precision(&self) -> i64 {
        let tile = (self.z + self.grid_precision).min(MAX_ZOOM);
        match self.grid_agg {
            GridAgg::GeoTile => tile,
            GridAgg::GeoHex => ZOOM2RESOLUTION[tile as usize] as i64,
        }
    }

    /// The query's bounding box: geohex bins don't align with tiles, so their query is
    /// buffered (`GridAggregation.bufferTile`).
    fn query_box(&self) -> Rect {
        if self.grid_agg == GridAgg::GeoTile || self.z == 0 || self.grid_precision == 0 {
            return self.tile;
        }
        let tile_precision = (self.z + self.grid_precision).min(MAX_ZOOM) as usize;
        let res = ZOOM2RESOLUTION[tile_precision] as usize;
        if res < 2 {
            return Rect { min_lon: -180.0, max_lon: 180.0, max_lat: 85.0511287798066, min_lat: -85.0511287798066 };
        }
        let (mut lat_buf, mut lon_buf) = (3.7, 51.2);
        for _ in 2..res {
            lat_buf /= 2.5;
            lon_buf /= 2.5;
        }
        let norm = |lon: f64| if lon > 180.0 { lon - 360.0 } else if lon < -180.0 { lon + 360.0 } else { lon };
        Rect {
            min_lon: norm(self.tile.min_lon - lon_buf),
            max_lon: norm(self.tile.max_lon + lon_buf),
            max_lat: (self.tile.max_lat + lat_buf).min(85.0511287798066),
            min_lat: (self.tile.min_lat - lat_buf).max(-85.0511287798066),
        }
    }

    fn search_body(&self) -> Value {
        let b = self.query_box();
        let shape = obj! {
            self.field.clone() => obj! {
                "shape" => obj! { "type" => "envelope", "coordinates" => Value::Arr(vec![Value::Arr(vec![b.min_lon.into(), b.max_lat.into()]), Value::Arr(vec![b.max_lon.into(), b.min_lat.into()])]) },
                "relation" => "intersects",
            }
        };
        let filter = obj! { "geo_shape" => shape };
        let query = match &self.query {
            Some(q) => obj! { "bool" => obj! { "filter" => Value::Arr(vec![q.clone(), filter]) } },
            None => filter,
        };
        let mut fields = self.fields.clone();
        fields.push(Value::from(self.field.clone()));
        let mut body = Map::new();
        body.insert("size".into(), Value::from(self.size));
        body.insert("_source".into(), Value::Bool(false));
        body.insert("track_total_hits".into(), self.track_total_hits.clone());
        body.insert("fields".into(), Value::Arr(fields));
        body.insert("query".into(), query);
        if let Some(rm) = &self.runtime_mappings {
            body.insert("runtime_mappings".into(), rm.clone());
        }
        let mut aggs = Map::new();
        if self.grid_precision > 0 {
            let mut grid = Map::new();
            grid.insert("field".into(), Value::from(self.field.clone()));
            grid.insert("precision".into(), Value::from(self.agg_precision()));
            grid.insert("size".into(), Value::from(65_536i64));
            if self.grid_agg == GridAgg::GeoTile || self.agg_precision() > 3 {
                let t = self.tile;
                grid.insert("bounds".into(), obj! { "top_left" => obj! { "lat" => t.max_lat, "lon" => t.min_lon }, "bottom_right" => obj! { "lat" => t.min_lat, "lon" => t.max_lon } });
            }
            let mut sub = self.aggs.clone();
            if self.grid_type == GridType::Centroid {
                sub.insert(CENTROID_AGG.into(), obj! { "geo_centroid" => obj! { "field" => self.field.clone() } });
            }
            let grid_name = if self.grid_agg == GridAgg::GeoTile { "geotile_grid" } else { "geohex_grid" };
            let mut g = Map::new();
            g.insert(grid_name.into(), Value::Obj(grid));
            if !sub.is_empty() {
                g.insert("aggs".into(), Value::Obj(sub));
            }
            aggs.insert("grid".into(), Value::Obj(g));
            aggs.insert("_count".into(), obj! { "stats_bucket" => obj! { "buckets_path" => "grid._count" } });
            for (name, def) in &self.aggs {
                let ty = def.as_object().and_then(|d| d.keys().find(|t| !matches!(t.as_str(), "meta" | "aggs" | "aggregations"))).cloned().unwrap_or_default();
                let metrics = metric_names(&ty).unwrap_or(&[]);
                for m in metrics {
                    let path = if m.contains('.') { format!("grid>{name}[{m}]") } else { format!("grid>{name}.{m}") };
                    let agg_name = if metrics.len() == 1 { name.clone() } else { format!("{name}.{m}") };
                    aggs.insert(agg_name, obj! { "stats_bucket" => obj! { "buckets_path" => path } });
                }
            }
        }
        if self.exact_bounds {
            aggs.insert("bounds".into(), obj! { "geo_bounds" => obj! { "field" => self.field.clone(), "wrap_longitude" => false } });
        }
        if !aggs.is_empty() {
            body.insert("aggs".into(), Value::Obj(aggs));
        }
        match &self.sort {
            Some(s) => {
                body.insert("sort".into(), s.clone());
            }
            // Elasticsearch sorts by the geometry's mercator diagonal (descending); points tie,
            // so they come in index order, and shapes are ordered after the fetch.
            None if self.size > 0 => {
                body.insert("sort".into(), Value::Arr(vec![Value::from("_doc")]));
            }
            None => {}
        }
        Value::Obj(body)
    }
}

/// The mercator diagonal² of a GeoJSON geometry's bounding box (the default sort).
fn mercator_diag2(v: &Value) -> f64 {
    let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    fn walk(v: &Value, b: &mut [f64; 4]) {
        if let Value::Arr(a) = v {
            if a.len() >= 2
                && let (Some(x), Some(y)) = (a[0].as_f64(), a[1].as_f64())
            {
                let (mx, my) = (lon_to_mercator(x), lat_to_mercator(y));
                b[0] = b[0].min(mx);
                b[1] = b[1].min(my);
                b[2] = b[2].max(mx);
                b[3] = b[3].max(my);
                return;
            }
            a.iter().for_each(|x| walk(x, b));
        }
    }
    if let Some(c) = v.get("coordinates") {
        walk(c, &mut b);
    }
    if b[0].is_finite() {
        let (w, h) = (b[2] - b[0], b[3] - b[1]);
        w * w + h * h
    } else {
        0.0
    }
}

/// Field types of the requested fields, to type their values like Elasticsearch's
/// `DocumentField`s (`Long`, `Integer`, `Float`, ...).
async fn field_types(ctx: &Ctx<'_>, index: &str, names: &[String]) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    if names.is_empty() {
        return out;
    }
    let uri = format!("/{}/_field_caps?fields={}&ignore_unavailable=true", enc(index), enc(&names.join(",")));
    if let Ok(Value::Obj(r)) = internal::call(ctx.cluster, "GET", &uri, None).await
        && let Some(Value::Obj(fields)) = r.get("fields")
    {
        for (f, types) in fields {
            if let Value::Obj(t) = types
                && let Some(ty) = t.keys().next()
            {
                out.insert(f.clone(), ty.clone());
            }
        }
    }
    out
}

fn typed(v: &Value, ty: Option<&str>) -> Option<Prop> {
    match (v, ty) {
        (Value::Num(n), Some("integer" | "short" | "byte")) => n.as_i64().map(Prop::Int),
        (Value::Num(n), Some("float" | "half_float")) => Some(Prop::Float(n.as_f64() as f32)),
        (Value::Num(n), Some("double" | "scaled_float")) => Some(Prop::Double(n.as_f64())),
        (Value::Num(n), _) if n.is_integer() => n.as_i64().map(Prop::Long),
        (Value::Num(n), _) => Some(Prop::Double(n.as_f64())),
        (Value::Str(s), _) => Some(Prop::Str(s.clone())),
        (Value::Bool(b), _) => Some(Prop::Bool(*b)),
        _ => None,
    }
}

/// A grid cell's center (`GeoTileUtils.zxyToGeoPoint`, or the H3 cell center).
fn cell_center(agg: GridAgg, key: &str) -> Option<(f64, f64)> {
    match agg {
        GridAgg::GeoTile => {
            let mut it = key.split('/').map(|p| p.parse::<i64>().ok());
            let (z, x, y) = (it.next()??, it.next()??, it.next()??);
            let tiles = (1i64 << z) as f64;
            Some((((x as f64 + 0.5) / tiles * 360.0) - 180.0, tile_to_lat_f(y as f64 + 0.5, tiles)))
        }
        GridAgg::GeoHex => {
            let h = search_aggs::h3::string_to_h3(key)?;
            let c = search_aggs::h3::h3_to_lat_lng(h);
            Some((c.lon.to_degrees(), c.lat.to_degrees()))
        }
    }
}

/// A grid cell's bounding box.
fn cell_rect(agg: GridAgg, key: &str) -> Option<Rect> {
    match agg {
        GridAgg::GeoTile => {
            let mut it = key.split('/').map(|p| p.parse::<i64>().ok());
            let (z, x, y) = (it.next()??, it.next()??, it.next()??);
            tile_bbox(z, x, y).ok()
        }
        GridAgg::GeoHex => {
            let h = search_aggs::h3::string_to_h3(key)?;
            let (top, left, bottom, right) = search_aggs::h3::cell_bounds(h);
            Some(Rect { min_lon: left, max_lon: right, min_lat: bottom, max_lat: top })
        }
    }
}

fn hex_geom(p: &Projection, key: &str) -> Option<Geom> {
    let h = search_aggs::h3::string_to_h3(key)?;
    let b = search_aggs::h3::h3_to_geo_boundary(h);
    let mut ring: Vec<Value> = b.points.iter().map(|q| Value::Arr(vec![q.lon.to_degrees().into(), q.lat.to_degrees().into()])).collect();
    if let Some(first) = ring.first().cloned() {
        ring.push(first);
    }
    shape_geom(p, &obj! { "type" => "Polygon", "coordinates" => Value::Arr(vec![Value::Arr(ring)]) }).into_iter().next()
}

pub async fn search_mvt(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let req = parse_request(ctx)?;
    let index = ctx.path_param("index").unwrap_or_default().to_string();
    let resp = internal::call(ctx.cluster, "POST", &format!("/{}/_search", enc(&index)), Some(&req.search_body())).await?;
    let proj = Projection::new(req.z, req.tile, req.extent, req.buffer);
    let _ = (req.x, req.y);
    let mut out = Vec::new();

    // Hits.
    let empty = Vec::new();
    let hits = resp.pointer(&["hits", "hits"]).and_then(Value::as_array).unwrap_or(&empty);
    if !hits.is_empty() {
        let mut layer = Layer::new("hits", req.extent);
        let mut names: Vec<String> = Vec::new();
        for h in hits {
            if let Some(Value::Obj(f)) = h.get("fields") {
                for k in f.keys() {
                    if *k != req.field && !names.contains(k) {
                        names.push(k.clone());
                    }
                }
            }
        }
        let types = field_types(ctx, &index, &names).await;
        let mut ordered: Vec<&Value> = hits.iter().collect();
        if req.sort.is_none() {
            // Bigger shapes first; points tie and keep their order.
            ordered.sort_by(|a, b| {
                let d = |h: &Value| h.pointer(&["fields", &req.field]).and_then(Value::as_array).and_then(|v| v.first()).map(mercator_diag2).unwrap_or(0.0);
                d(b).total_cmp(&d(a))
            });
        }
        for h in ordered {
            let Some(Value::Obj(fields)) = h.get("fields") else { continue };
            let Some(geo) = fields.get(&req.field).and_then(Value::as_array) else { continue };
            // A geo_point field is one multi-point feature; shapes one feature per geometry.
            let all_points = geo.iter().all(|g| g.get("type").and_then(Value::as_str).is_some_and(|t| t.eq_ignore_ascii_case("point")));
            let geoms: Vec<Geom> = if all_points {
                let mut pts: Vec<(f64, f64)> = geo
                    .iter()
                    .filter_map(|g| {
                        let c = g.get("coordinates")?.as_array()?;
                        Some((c.first()?.as_f64()?, c.get(1)?.as_f64()?))
                    })
                    .collect();
                proj.points(&mut pts).into_iter().collect()
            } else {
                geo.iter().flat_map(|g| shape_geom(&proj, g)).collect()
            };
            for g in geoms {
                let mut tags = Vec::new();
                layer.tag(&mut tags, "_id", Prop::Str(h.get("_id").map(Value::to_es_string).unwrap_or_default()));
                layer.tag(&mut tags, "_index", Prop::Str(h.get("_index").map(Value::to_es_string).unwrap_or_default()));
                for (k, v) in order_fields(fields, &req.fields) {
                    if *k == req.field {
                        continue;
                    }
                    let first = v.as_array().and_then(|a| a.first()).unwrap_or(v);
                    if let Some(p) = typed(first, types.get(k).map(String::as_str)) {
                        layer.tag(&mut tags, k, p);
                    }
                }
                layer.features.push((tags, g));
            }
        }
        layer.encode(&mut out);
    }

    // Grid cells.
    if let Some(Value::Arr(buckets)) = resp.pointer(&["aggregations", "grid", "buckets"])
        && !buckets.is_empty()
    {
        let mut layer = Layer::new("aggs", req.extent);
        for b in buckets {
            let key = b.get("key").map(Value::to_es_string).unwrap_or_default();
            let geom = match req.grid_type {
                GridType::Grid => match req.grid_agg {
                    GridAgg::GeoTile => cell_rect(req.grid_agg, &key).and_then(|r| proj.rect(r.min_lon, r.max_lon, r.min_lat, r.max_lat)),
                    GridAgg::GeoHex => hex_geom(&proj, &key),
                },
                GridType::Point => cell_center(req.grid_agg, &key).and_then(|(lon, lat)| proj.point(lon, lat)),
                GridType::Centroid => {
                    let r = cell_rect(req.grid_agg, &key);
                    let c = b.pointer(&[CENTROID_AGG, "location"]);
                    match (r, c.and_then(|c| Some((c.get("lon")?.as_f64()?, c.get("lat")?.as_f64()?)))) {
                        (Some(r), Some((lon, lat))) => proj.point(lon.clamp(r.min_lon, r.max_lon.max(r.min_lon)), lat.clamp(r.min_lat, r.max_lat.max(r.min_lat))),
                        _ => None,
                    }
                }
            };
            let Some(geom) = geom else { continue };
            let mut tags = Vec::new();
            layer.tag(&mut tags, "_key", Prop::Str(key.clone()));
            layer.tag(&mut tags, "_count", Prop::Long(b.get("doc_count").and_then(Value::as_i64).unwrap_or(0)));
            if let Value::Obj(m) = b {
                for (name, v) in m {
                    if matches!(name.as_str(), "key" | "doc_count") || name.starts_with(INTERNAL_PREFIX) {
                        continue;
                    }
                    // Each aggregation's flattened keys come out sorted (`Maps.flatten(.., ordered)`).
                    let mut flat = Vec::new();
                    flatten(name, v, &mut flat);
                    flat.sort_by(|a, b| a.0.cmp(&b.0));
                    for (k, x) in flat {
                        if let Some(p) = Prop::from_json(&x) {
                            layer.tag(&mut tags, &k, p);
                        }
                    }
                }
            }
            layer.features.push((tags, geom.clone()));
            if req.with_labels
                && let Some((lon, lat)) = cell_center(req.grid_agg, &key)
                && let Some(lg) = proj.point(lon, lat)
            {
                let mut tags = Vec::new();
                layer.tag(&mut tags, "_key", Prop::Str(key.clone()));
                layer.tag(&mut tags, "_count", Prop::Long(b.get("doc_count").and_then(Value::as_i64).unwrap_or(0)));
                layer.tag(&mut tags, "_mvt_label_position", Prop::Bool(true));
                layer.features.push((tags, lg));
            }
        }
        layer.encode(&mut out);
    }

    // Meta: the search response without hits, the grid or the bounds, flattened and sorted.
    let mut meta = Layer::new("meta", req.extent);
    let bounds = resp.pointer(&["aggregations", "bounds", "bounds"]).and_then(|b| {
        let tl = b.get("top_left")?;
        let br = b.get("bottom_right")?;
        Some((tl.get("lon")?.as_f64()?, br.get("lon")?.as_f64()?, br.get("lat")?.as_f64()?, tl.get("lat")?.as_f64()?))
    });
    let t = req.tile;
    let (min_lon, max_lon, min_lat, max_lat) = bounds.unwrap_or((t.min_lon, t.max_lon, t.min_lat, t.max_lat));
    let geom = proj.rect(min_lon, max_lon, min_lat, max_lat).unwrap_or(Geom { ty: 3, cmds: Vec::new() });
    let mut flat: Vec<(String, Value)> = Vec::new();
    let mut r = resp.clone();
    if let Value::Obj(m) = &mut r {
        if let Some(Value::Obj(h)) = m.get_mut("hits") {
            h.shift_remove("hits");
            // A sorted search scores nothing: Elasticsearch's `max_score` is null.
            if req.size > 0 || req.sort.is_some() {
                h.shift_remove("max_score");
            }
        }
        if let Some(Value::Obj(a)) = m.get_mut("aggregations") {
            a.shift_remove("grid");
            a.shift_remove("bounds");
            if a.is_empty() {
                m.shift_remove("aggregations");
            }
        }
        m.shift_remove("_clusters");
    }
    flatten("", &r, &mut flat);
    flat.sort_by(|a, b| a.0.cmp(&b.0));
    let mut tags = Vec::new();
    for (k, v) in flat {
        let p = if k == "hits.max_score" { v.as_f64().map(|f| Prop::Float(f as f32)) } else { Prop::from_json(&v) };
        if let Some(p) = p {
            meta.tag(&mut tags, &k, p);
        }
    }
    meta.features.push((tags, geom));
    meta.encode(&mut out);
    Ok(Reply::Raw(200, MIME_TYPE.to_string(), bytes::Bytes::from(out)))
}

/// A hit's fields in the order the request named them (the fields API's fetch order).
fn order_fields<'a>(fields: &'a Map, requested: &[Value]) -> Vec<(&'a String, &'a Value)> {
    let pattern = |r: &Value| -> String {
        match r {
            Value::Obj(m) => m.get("field").map(Value::to_es_string).unwrap_or_default(),
            other => other.to_es_string(),
        }
    };
    let rank = |k: &str| requested.iter().position(|r| crate::xcontent::filter::glob_match(&pattern(r), k)).unwrap_or(usize::MAX);
    let mut out: Vec<(&String, &Value)> = fields.iter().collect();
    out.sort_by_key(|(k, _)| rank(k));
    out
}

fn flatten(prefix: &str, v: &Value, out: &mut Vec<(String, Value)>) {
    match v {
        Value::Obj(m) => {
            for (k, x) in m {
                flatten(&if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") }, x, out);
            }
        }
        Value::Arr(a) => {
            for (i, x) in a.iter().enumerate() {
                flatten(&format!("{prefix}.{i}"), x, out);
            }
        }
        Value::Null => {}
        other => out.push((prefix.to_string(), other.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pixel coordinates a real Elasticsearch 9.5.5 encoded (`9, zz(x), zz(y)` point commands).
    #[test]
    fn points_match_elasticsearch() {
        let p = Projection::new(0, tile_bbox(0, 0, 0).unwrap(), 4096, 5);
        assert_eq!(p.point(34.25, -21.76).unwrap().cmds, vec![9, 4876, 4604]);
        assert_eq!(p.point(34.30, -21.70).unwrap().cmds, vec![9, 4876, 4602]);
        assert_eq!(p.point(-20.0, 10.0).unwrap().cmds, vec![9, 3640, 3868]);
        let p = Projection::new(3, tile_bbox(3, 4, 4).unwrap(), 4096, 5);
        assert_eq!(p.point(34.25, -21.76).unwrap().cmds, vec![9, 6236, 4060]);
        assert!(p.point(-20.0, 10.0).is_none());
    }

    #[test]
    fn boxes_match_elasticsearch() {
        let t = tile_bbox(0, 0, 0).unwrap();
        let p = Projection::new(0, t, 4096, 5);
        assert_eq!(p.rect(t.min_lon, t.max_lon, t.min_lat, t.max_lat).unwrap().cmds, vec![9, 0, 8192, 26, 0, 8191, 8192, 0, 0, 8192, 15]);
        let c = tile_bbox(8, 152, 143).unwrap();
        assert_eq!(p.rect(c.min_lon, c.max_lon, c.min_lat, c.max_lat).unwrap().cmds, vec![9, 4864, 4608, 26, 0, 31, 32, 0, 0, 32, 15]);
        let p = Projection::new(3, tile_bbox(3, 4, 4).unwrap(), 4096, 5);
        let c = tile_bbox(5, 19, 17).unwrap();
        assert_eq!(p.rect(c.min_lon, c.max_lon, c.min_lat, c.max_lat).unwrap().cmds, vec![9, 6144, 4096, 26, 0, 2047, 2048, 0, 0, 2048, 15]);
    }

    #[test]
    fn tile_validation() {
        assert_eq!(tile_bbox(1, 2, 0).unwrap_err().reason(), "Zoom/X/Y combination is not valid: 1/2/0");
        assert_eq!(tile_bbox(30, 0, 0).unwrap_err().reason(), "Invalid geotile_grid precision of 30. Must be between 0 and 29.");
        assert_eq!(parse_int("a").unwrap_err().reason(), "For input string: \"a\"");
    }

    #[test]
    fn clipping() {
        let c = [0.0, 0.0, 10.0, 10.0];
        let r = shapes::clip_ring(&[[-5.0, -5.0], [15.0, -5.0], [15.0, 15.0], [-5.0, 15.0], [-5.0, -5.0]], c);
        assert_eq!(r.len(), 4);
        assert!(r.iter().all(|p| p[0] >= 0.0 && p[0] <= 10.0 && p[1] >= 0.0 && p[1] <= 10.0));
        let parts = shapes::clip_line(&[[-5.0, 5.0], [5.0, 5.0], [15.0, 5.0], [15.0, 6.0], [5.0, 6.0]], c);
        assert_eq!(parts, vec![vec![[0.0, 5.0], [5.0, 5.0], [10.0, 5.0]], vec![[10.0, 6.0], [5.0, 6.0]]]);
        assert!(shapes::area2(&[(0, 4096), (0, 0), (4096, 0), (4096, 4096)]) > 0);
    }

    #[test]
    fn props_dedupe_by_java_type() {
        let mut l = Layer::new("aggs", 4096);
        let mut tags = Vec::new();
        l.tag(&mut tags, "_count", Prop::Long(2));
        l.tag(&mut tags, "s.count", Prop::Int(2));
        l.tag(&mut tags, "x", Prop::Int(2));
        assert_eq!(l.values, vec![Prop::Long(2), Prop::Int(2)]);
        assert_eq!(tags, vec![0, 0, 1, 1, 2, 1]);
    }
}
