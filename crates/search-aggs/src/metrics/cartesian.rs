//! `cartesian_bounds` and `cartesian_centroid` (x-pack spatial) over `point` fields.
//!
//! A `point` field's doc values are Lucene's `XYDocValuesField`: each coordinate is a `float`
//! stored as its sortable int, `x` in the high 32 bits and `y` in the low. Values decode to the
//! float's exact double, which is what Elasticsearch sums and reports (`100.1` comes back as
//! `100.0999984741211`).
//!
//! Bounds keep the extremes per bucket (no dateline: the plane doesn't wrap). Centroids keep
//! Kahan-compensated sums of `x` and `y` per bucket, in doc-value order, and reduce like
//! `InternalCentroid`: each partial centroid re-weighted by its count, plainly summed.

use search_index::SegmentContext;

use crate::collect::{Aggregator, Batch, BindCtx, grow};
use crate::error::{AggError, Result};
use crate::shape_field::{DimensionalShapeType, ShapeData, ShapeReader, reduce_centroids};
use crate::internal::{AggValue, CompensatedSum, Internal, Prop, ReduceCtx, RenderCtx, java_list};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::metrics::simple::{java_max, java_min};
use crate::request::{AggDef, PathElement};
use crate::values::{BoundVs, DvNum, MissingValue, VsOpts, VsSpec, VsType, bind};
use crate::xparser::XParser;

/// Which cartesian metric.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CartesianKind {
    Bounds,
    Centroid,
}

impl CartesianKind {
    fn type_name(self) -> &'static str {
        match self {
            CartesianKind::Bounds => "cartesian_bounds",
            CartesianKind::Centroid => "cartesian_centroid",
        }
    }
}

/// A parsed `cartesian_bounds` / `cartesian_centroid`.
#[derive(Clone, Debug)]
pub struct CartesianSpec {
    pub kind: CartesianKind,
    pub vs: VsSpec,
}

/// `ValuesSourceAggregationBuilder.declareFields(parser, scriptable, false, false)`: only
/// `cartesian_centroid` takes a `script`.
pub fn parse(p: &mut XParser<'_>, kind: CartesianKind) -> Result<CartesianSpec> {
    let opts = VsOpts::new(kind == CartesianKind::Centroid, false, false);
    let fields = VsSpec::fields(opts);
    let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
    let mut vs = VsSpec::default();
    p.object(kind.type_name(), &fields, |p, i| {
        vs.parse_field(p, names[i])?;
        Ok(())
    })?;
    vs.check_required(opts)?;
    Ok(CartesianSpec { kind, vs })
}

/// `XYEncodingUtils.encode` of both coordinates, packed like `XYDocValuesField`.
pub fn encode_xy(x: f64, y: f64) -> i64 {
    let enc = |v: f64| -> u32 {
        let bits = (v as f32).to_bits() as i32;
        (bits ^ ((bits >> 31) & 0x7fff_ffff)) as u32
    };
    ((enc(x) as u64) << 32 | enc(y) as u64) as i64
}

/// The `(x, y)` of an `XYDocValuesField` value.
#[inline]
pub fn decode_xy(v: i64) -> (f64, f64) {
    let dec = |s: u32| -> f64 {
        let bits = s as i32;
        f32::from_bits((bits ^ ((bits >> 31) & 0x7fff_ffff)) as u32) as f64
    };
    (dec((v as u64 >> 32) as u32), dec(v as u32))
}

fn parse_error(msg: impl Into<String>) -> AggError {
    AggError::new(400, "parse_exception", msg)
}

/// `CartesianPoint.resetFromString(value, ignoreZValue = false)`: `"x,y"` or WKT `POINT (x y)`.
pub fn parse_point_text(value: &str) -> Result<(f64, f64)> {
    if value.to_lowercase().contains("point") {
        return parse_wkt_point(value);
    }
    if value.contains(',') {
        let vals: Vec<&str> = java_split_comma(value);
        if vals.len() > 3 || vals.len() < 2 {
            return Err(parse_error(format!("failed to parse [[{}]], expected 2 or 3 coordinates but found: [{}]", vals.join(", "), vals.len())));
        }
        let coord = |s: &str, name: &str| -> Result<f64> {
            let v = crate::xparser::java_parse_double(s.trim()).map_err(|_| parse_error(format!("[{name}] must be a number")))?;
            if !v.is_finite() {
                return Err(parse_error(format!(
                    "invalid [{name}] value [{}]; must be between -3.4028234663852886E38 and 3.4028234663852886E38",
                    crate::json::java_double(v)
                )));
            }
            Ok(v)
        };
        let x = coord(vals[0], "x")?;
        let y = coord(vals[1], "y")?;
        if vals.len() > 2 {
            let z = crate::xparser::java_parse_double(vals[2].trim()).map_err(|_| parse_error("[y] must be a number"))?;
            return Err(z_value_error(z));
        }
        return Ok((x, y));
    }
    if value.contains('.') {
        return Err(parse_error(format!("failed to parse [{value}], expected 2 or 3 coordinates but found: [1]")));
    }
    let first = value.chars().next().map(String::from).unwrap_or_default();
    Err(parse_error(format!("unsupported symbol [{first}] in point [{value}]")))
}

/// `String.split(",")`: trailing empty strings are dropped.
fn java_split_comma(s: &str) -> Vec<&str> {
    let mut v: Vec<&str> = s.split(',').collect();
    while v.len() > 1 && v.last() == Some(&"") {
        v.pop();
    }
    if v.len() == 1 && v[0].is_empty() {
        v.clear();
    }
    v
}

fn z_value_error(z: f64) -> AggError {
    parse_error(format!("Exception parsing coordinates: found Z value [{}] but [ignore_z_value] parameter is [false]", crate::json::java_double(z)))
}

/// A WKT `POINT (x y)` (or `POINT EMPTY`, which has no coordinates), as
/// `WellKnownText.fromWKT` reads it; anything else is "Invalid WKT format".
fn parse_wkt_point(value: &str) -> Result<(f64, f64)> {
    let invalid = || parse_error("Invalid WKT format");
    let s = value.trim();
    let upper = s.to_uppercase();
    let rest = upper.strip_prefix("POINT").ok_or_else(|| {
        // Another geometry: `[point] supports only POINT among WKT primitives, but found X`.
        let kind = upper.split(|c: char| c == '(' || c.is_whitespace()).next().unwrap_or("");
        match kind {
            "LINESTRING" | "POLYGON" | "MULTIPOINT" | "MULTILINESTRING" | "MULTIPOLYGON" | "GEOMETRYCOLLECTION" | "BBOX" | "CIRCLE" => {
                parse_error(format!("[point] supports only POINT among WKT primitives, but found {}", kind.replace("BBOX", "ENVELOPE")))
            }
            _ => invalid(),
        }
    })?;
    let rest = rest.trim();
    let inner = rest.strip_prefix('(').and_then(|r| r.strip_suffix(')')).ok_or_else(invalid)?;
    let nums: Vec<&str> = inner.split_whitespace().collect();
    if nums.len() < 2 || nums.len() > 3 {
        return Err(invalid());
    }
    let num = |t: &str| -> Result<f64> { crate::xparser::java_parse_double(t).map_err(|_| invalid()) };
    let (x, y) = (num(nums[0])?, num(nums[1])?);
    if nums.len() == 3 {
        let z = num(nums[2])?;
        return Err(AggError::new(400, "parse_exception", "Invalid WKT format").caused_by(AggError::illegal_argument(format!(
            "found Z value [{}] but [ignore_z_value] parameter is [false]",
            crate::json::java_double(z)
        ))));
    }
    Ok((x, y))
}

/// Binds `cartesian_bounds` / `cartesian_centroid`.
pub fn create(def: &AggDef, spec: &CartesianSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let vs = bind(&spec.vs, ctx.lookup, VsType::CartesianPoint, false)?;
    if !vs.unmapped && !matches!(vs.vs_type, VsType::CartesianPoint | VsType::CartesianShape) {
        return Err(vs.unsupported(spec.kind.type_name()));
    }
    if vs.field.is_none() && vs.script.is_some() {
        // `CartesianPointValuesSourceType.getScript`.
        return Err(AggError::new(500, "unsupported_operation_exception", "point"));
    }
    let missing = match vs.missing {
        Some(MissingValue::Xy(v)) => Some(v),
        _ => None,
    };
    Ok(Box::new(CartesianAgg {
        kind: spec.kind,
        shape: vs.vs_type == VsType::CartesianShape,
        shapes: None,
        weights: Vec::new(),
        types: Vec::new(),
        field: vs.field.as_ref().map(|f| f.number),
        missing,
        vs,
        dv: DvNum::None,
        buf: Vec::new(),
        bounds: Vec::new(),
        sums: Vec::new(),
        counts: Vec::new(),
        tracker: ctx.tracker(def),
    }))
}

impl CartesianAgg {
    fn collect_shapes(&mut self, b: Batch<'_>, shapes: &mut ShapeReader) -> Result<()> {
        for i in 0..b.len() {
            let Some(v) = shapes.get(b.docs[i])? else { continue };
            let ord = b.ords[i];
            let o = ord as usize;
            match self.kind {
                CartesianKind::Bounds => {
                    grow(&mut self.bounds, ord, EMPTY_BOUNDS);
                    let bb = v.bounding_box();
                    let s = &mut self.bounds[o];
                    s[0] = java_max(s[0], bb.top);
                    s[1] = java_min(s[1], bb.bottom);
                    s[2] = java_min(s[2], bb.min_x());
                    s[3] = java_max(s[3], bb.max_x());
                }
                CartesianKind::Centroid => {
                    grow(&mut self.counts, ord, 0);
                    grow(&mut self.sums, ord, (CompensatedSum::default(), CompensatedSum::default()));
                    grow(&mut self.weights, ord, CompensatedSum::default());
                    grow(&mut self.types, ord, DimensionalShapeType::Point);
                    self.counts[o] += 1;
                    let (t, w) = (v.centroid.shape_type, v.centroid.weight);
                    let (x, y) = v.centroid_xy();
                    let (sx, sy) = &mut self.sums[o];
                    match self.types[o].cmp(&t) {
                        std::cmp::Ordering::Less => {
                            *sy = CompensatedSum::new(w * y, 0.0);
                            *sx = CompensatedSum::new(w * x, 0.0);
                            self.weights[o] = CompensatedSum::new(w, 0.0);
                            self.types[o] = t;
                        }
                        std::cmp::Ordering::Equal => {
                            sy.add(w * y);
                            sx.add(w * x);
                            self.weights[o].add(w);
                        }
                        std::cmp::Ordering::Greater => {}
                    }
                }
            }
        }
        self.tracker.ensure(self.bounds.len() * 32 + self.counts.len() * 64)
    }
}

const EMPTY_BOUNDS: [f64; 4] = [f64::NEG_INFINITY, f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY];

struct CartesianAgg {
    kind: CartesianKind,
    /// A `shape` source (`CartesianShapeBoundsAggregator` / `CartesianShapeCentroidAggregator`).
    shape: bool,
    shapes: Option<ShapeReader>,
    weights: Vec<CompensatedSum>,
    types: Vec<DimensionalShapeType>,
    field: Option<u32>,
    missing: Option<i64>,
    vs: BoundVs,
    dv: DvNum,
    buf: Vec<i64>,
    /// `[top, bottom, left, right]` per bucket.
    bounds: Vec<[f64; 4]>,
    /// `(x, y)` compensated sums per bucket.
    sums: Vec<(CompensatedSum, CompensatedSum)>,
    counts: Vec<u64>,
    tracker: MemTracker,
}

impl Aggregator for CartesianAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        if self.shape {
            self.shapes = Some(ShapeReader::for_source(seg, &self.vs));
            return Ok(());
        }
        self.dv = match self.field {
            Some(f) => DvNum::open(seg, f),
            None => DvNum::None,
        };
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        if let Some(mut shapes) = self.shapes.take() {
            let r = self.collect_shapes(b, &mut shapes);
            self.shapes = Some(shapes);
            return r;
        }
        if self.field.is_none() && self.missing.is_none() {
            return Ok(());
        }
        for i in 0..b.len() {
            let ord = b.ords[i];
            self.buf.clear();
            let buf = &mut self.buf;
            self.dv.for_each(b.docs[i], |v| buf.push(v));
            if self.buf.is_empty()
                && let Some(m) = self.missing
            {
                self.buf.push(m);
            }
            match self.kind {
                CartesianKind::Bounds => {
                    if self.buf.is_empty() {
                        continue;
                    }
                    grow(&mut self.bounds, ord, EMPTY_BOUNDS);
                    let s = &mut self.bounds[ord as usize];
                    for &v in &self.buf {
                        let (x, y) = decode_xy(v);
                        s[0] = java_max(s[0], y);
                        s[1] = java_min(s[1], y);
                        s[2] = java_min(s[2], x);
                        s[3] = java_max(s[3], x);
                    }
                }
                CartesianKind::Centroid => {
                    // Every collected doc sizes the arrays, with or without values.
                    grow(&mut self.counts, ord, 0);
                    grow(&mut self.sums, ord, (CompensatedSum::default(), CompensatedSum::default()));
                    let o = ord as usize;
                    self.counts[o] += self.buf.len() as u64;
                    let (sx, sy) = &mut self.sums[o];
                    for &v in &self.buf {
                        let (x, y) = decode_xy(v);
                        sx.add(x);
                        sy.add(y);
                    }
                }
            }
        }
        self.tracker.ensure(self.bounds.len() * 32 + self.counts.len() * 40)
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        Ok(owning
            .iter()
            .map(|&o| {
                let o = o as usize;
                match self.kind {
                    CartesianKind::Bounds => {
                        let s = self.bounds.get(o).copied().unwrap_or(EMPTY_BOUNDS);
                        AggValue::CartesianBounds(Box::new(CartesianBounds { top: s[0], bottom: s[1], left: s[2], right: s[3] }))
                    }
                    CartesianKind::Centroid if self.shape => {
                        if o >= self.counts.len() {
                            return AggValue::CartesianCentroid(Box::new(CartesianCentroid { centroid: None, count: 0, shape: None }));
                        }
                        let (sx, sy, w) = (self.sums[o].0.value, self.sums[o].1.value, self.weights[o].value);
                        let centroid = (w > 0.0).then(|| (sx / w, sy / w));
                        let shape = Some(ShapeData { first: sx, second: sy, weight: w, shape_type: self.types[o] });
                        AggValue::CartesianCentroid(Box::new(CartesianCentroid { centroid, count: self.counts[o], shape }))
                    }
                    CartesianKind::Centroid => {
                        let count = self.counts.get(o).copied().unwrap_or(0);
                        let centroid = (count > 0).then(|| (self.sums[o].0.value / count as f64, self.sums[o].1.value / count as f64));
                        AggValue::CartesianCentroid(Box::new(CartesianCentroid { centroid, count, shape: None }))
                    }
                }
            })
            .collect())
    }

    fn build_empty(&self) -> AggValue {
        match self.kind {
            CartesianKind::Bounds => AggValue::CartesianBounds(Box::new(CartesianBounds::empty())),
            CartesianKind::Centroid => AggValue::CartesianCentroid(Box::new(CartesianCentroid { centroid: None, count: 0, shape: None })),
        }
    }

    fn needs_scores(&self) -> bool {
        self.vs.script.as_ref().is_some_and(|s| s.needs_score())
    }

    fn validate_sort(&self, _name: &str, _key: Option<&str>, _rest: &[PathElement]) -> Result<()> {
        Err(AggError::illegal_argument(
            "Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end.",
        ))
    }
}

/// `InternalCartesianBounds`.
#[derive(Clone, Debug)]
pub struct CartesianBounds {
    pub top: f64,
    pub bottom: f64,
    pub left: f64,
    pub right: f64,
}

impl CartesianBounds {
    fn empty() -> CartesianBounds {
        let [top, bottom, left, right] = EMPTY_BOUNDS;
        CartesianBounds { top, bottom, left, right }
    }
}

impl Internal for CartesianBounds {
    fn type_name(&self) -> &'static str {
        "cartesian_bounds"
    }
    fn class_name(&self) -> &'static str {
        "InternalCartesianBounds"
    }
    fn reduce(list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let mut r = CartesianBounds::empty();
        for b in &list {
            r.top = java_max(r.top, b.top);
            r.bottom = java_min(r.bottom, b.bottom);
            r.left = java_min(r.left, b.left);
            r.right = java_max(r.right, b.right);
        }
        Ok(r)
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        if !self.top.is_infinite() {
            let point = |x: f64, y: f64| Json::obj().with("x", Json::Float(x)).with("y", Json::Float(y));
            out.push("bounds", Json::obj().with("top_left", point(self.left, self.top)).with("bottom_right", point(self.right, self.bottom)));
        }
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        let unknown = |e: &str| AggError::illegal_argument(format!("Found unknown path element [{e}] in [{name}]"));
        let missing = || AggError::new(500, "null_pointer_exception", "bounding box is null");
        match path.len() {
            1 => {
                if self.top.is_infinite() {
                    return Err(missing());
                }
                match path[0].as_str() {
                    "top" => Ok(Prop::Num(self.top)),
                    "left" => Ok(Prop::Num(self.left)),
                    "bottom" => Ok(Prop::Num(self.bottom)),
                    "right" => Ok(Prop::Num(self.right)),
                    e => Err(unknown(e)),
                }
            }
            2 => {
                if self.top.is_infinite() {
                    return Err(missing());
                }
                let (x, y) = match path[0].as_str() {
                    "top_left" => (self.left, self.top),
                    "bottom_right" => (self.right, self.bottom),
                    e => return Err(unknown(e)),
                };
                match path[1].as_str() {
                    "x" => Ok(Prop::Num(x)),
                    "y" => Ok(Prop::Num(y)),
                    e => Err(unknown(e)),
                }
            }
            _ => Err(AggError::illegal_argument(format!("path not supported for [{name}]: {}", java_list(path)))),
        }
    }
}

/// `InternalCartesianCentroid`.
#[derive(Clone, Debug)]
pub struct CartesianCentroid {
    /// `(x, y)`; `None` with no points.
    pub centroid: Option<(f64, f64)>,
    pub count: u64,
    /// Shape centroids' weighted sums (`ShapeData`, x first).
    pub shape: Option<ShapeData>,
}

impl Internal for CartesianCentroid {
    fn type_name(&self) -> &'static str {
        "cartesian_centroid"
    }
    fn class_name(&self) -> &'static str {
        "InternalCartesianCentroid"
    }
    fn reduce(list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let (centroid, count, shape) = reduce_centroids(list.iter().map(|c| (c.centroid, c.count, c.shape)));
        Ok(CartesianCentroid { centroid, count, shape })
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        if let Some((x, y)) = self.centroid {
            out.push("location", Json::obj().with("x", Json::Float(x)).with("y", Json::Float(y)));
        }
        out.push("count", Json::from(self.count));
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        if path.len() != 1 {
            return Err(AggError::illegal_argument(format!("path not supported for [{name}]: {}", java_list(path))));
        }
        let point = || self.centroid.ok_or_else(|| AggError::new(500, "null_pointer_exception", "centroid is null"));
        match path[0].as_str() {
            "value" => {
                let (x, y) = point()?;
                Ok(Prop::Value(Json::obj().with("x", Json::Float(x)).with("y", Json::Float(y))))
            }
            "count" => Ok(Prop::Num(self.count as f64)),
            "x" => Ok(Prop::Num(point()?.0)),
            "y" => Ok(Prop::Num(point()?.1)),
            e => Err(AggError::illegal_argument(format!("Found unknown path element [{e}] in [{name}]"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xy_encoding_round_trips_floats() {
        for (x, y) in [(1.5, -2.25), (100.1, 3.0), (-0.0, 0.0), (f32::MAX as f64, f32::MIN as f64)] {
            let (dx, dy) = decode_xy(encode_xy(x, y));
            assert_eq!((dx, dy), ((x as f32) as f64, (y as f32) as f64));
        }
        assert_eq!(decode_xy(encode_xy(100.1, 0.0)).0, 100.0999984741211);
        // Sortable: larger x, larger encoded high word.
        assert!(encode_xy(-1.0, 0.0) < encode_xy(1.0, 0.0));
    }

    #[test]
    fn point_text() {
        assert_eq!(parse_point_text("1,2").unwrap(), (1.0, 2.0));
        assert_eq!(parse_point_text(" 1.5 , -3 ").unwrap(), (1.5, -3.0));
        assert_eq!(parse_point_text("POINT (3 4)").unwrap(), (3.0, 4.0));
        assert_eq!(parse_point_text("point(3.5 -4)").unwrap(), (3.5, -4.0));
        let e = parse_point_text("1,2,3,4").unwrap_err();
        assert_eq!(e.reason, "failed to parse [[1, 2, 3, 4]], expected 2 or 3 coordinates but found: [4]");
        assert_eq!(parse_point_text("1.5").unwrap_err().reason, "failed to parse [1.5], expected 2 or 3 coordinates but found: [1]");
        assert_eq!(parse_point_text("abc").unwrap_err().reason, "unsupported symbol [a] in point [abc]");
        assert_eq!(parse_point_text("a,2").unwrap_err().reason, "[x] must be a number");
        assert_eq!(parse_point_text("1,2,3").unwrap_err().reason, "Exception parsing coordinates: found Z value [3.0] but [ignore_z_value] parameter is [false]");
        assert_eq!(parse_point_text("MULTIPOINT (1 2)").unwrap_err().reason, "[point] supports only POINT among WKT primitives, but found MULTIPOINT");
        assert_eq!(parse_point_text("POINT (1)").unwrap_err().reason, "Invalid WKT format");
    }
}
