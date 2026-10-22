//! `geo_bounds` and `geo_centroid` over `geo_point` and `geo_shape` doc values.
//!
//! On shapes ([`crate::shape_field`]), bounds merge each doc's extent and centroids follow
//! `GeoShapeCentroidAggregator`: per bucket only the docs of the highest dimension seen count,
//! each weighted by its stored weight (area, length, or 1 per point); results carry the
//! weighted sums (`ShapeData`) so the reduce can keep combining by dimension.
//!
//! Bounds keep the top and bottom latitudes and, separately for positive and negative
//! longitudes, the leftmost and rightmost ones, so `wrap_longitude` can pick the narrower box
//! across the dateline when rendering. Centroids sum latitudes and longitudes with
//! Elasticsearch's compensated summation per shard, and reduce by re-weighting each partial
//! centroid by its count (`count * lat`, plain sums), exactly as `InternalCentroid` does.

use search_index::SegmentContext;

use crate::collect::{Aggregator, Batch, BindCtx, grow};
use crate::error::{AggError, Result};
use crate::internal::{AggValue, CompensatedSum, Internal, Prop, ReduceCtx, RenderCtx, java_list};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::metrics::simple::{java_max, java_min};
use crate::shape_field::{DimensionalShapeType, ShapeData, ShapeReader, reduce_centroids};
use crate::request::AggDef;
use crate::values::{BoundVs, GeoReader, VsOpts, VsSpec, VsType, bind};
use crate::xparser::{Field, Tokens, XParser};

/// Which geo metric.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeoMetricKind {
    Bounds,
    Centroid,
}

/// A parsed `geo_bounds` / `geo_centroid`.
#[derive(Clone, Debug)]
pub struct GeoMetricSpec {
    pub kind: GeoMetricKind,
    pub vs: VsSpec,
    pub wrap_longitude: bool,
}

/// Parses `geo_bounds` (fields: field, missing, value_type, wrap_longitude) or `geo_centroid`
/// (field, missing, value_type, script).
pub fn parse(p: &mut XParser<'_>, kind: GeoMetricKind) -> Result<GeoMetricSpec> {
    let (name, opts) = match kind {
        GeoMetricKind::Bounds => ("geo_bounds", VsOpts::new(false, false, false)),
        GeoMetricKind::Centroid => ("geo_centroid", VsOpts::new(true, false, false)),
    };
    let mut fields = VsSpec::fields(opts);
    if kind == GeoMetricKind::Bounds {
        fields.push(Field::new("wrap_longitude", Tokens::BOOLEAN));
    }
    let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
    let mut vs = VsSpec::default();
    let mut wrap_longitude = true;
    p.object(name, &fields, |p, i| {
        if !vs.parse_field(p, names[i])? {
            wrap_longitude = p.boolean_value()?;
        }
        Ok(())
    })?;
    vs.check_required(opts)?;
    Ok(GeoMetricSpec { kind, vs, wrap_longitude })
}

/// `InternalGeoBounds`.
#[derive(Clone, Debug)]
pub struct GeoBounds {
    pub top: f64,
    pub bottom: f64,
    pub pos_left: f64,
    pub pos_right: f64,
    pub neg_left: f64,
    pub neg_right: f64,
    pub wrap_longitude: bool,
}

impl GeoBounds {
    fn empty(wrap_longitude: bool) -> GeoBounds {
        GeoBounds {
            top: f64::NEG_INFINITY,
            bottom: f64::INFINITY,
            pos_left: f64::INFINITY,
            pos_right: f64::NEG_INFINITY,
            neg_left: f64::INFINITY,
            neg_right: f64::NEG_INFINITY,
            wrap_longitude,
        }
    }

    /// `resolveBoundingBox`: `(top, left, bottom, right)`, `None` when empty.
    pub fn bbox(&self) -> Option<(f64, f64, f64, f64)> {
        if self.top.is_infinite() {
            None
        } else if self.pos_left.is_infinite() {
            Some((self.top, self.neg_left, self.bottom, self.neg_right))
        } else if self.neg_left.is_infinite() {
            Some((self.top, self.pos_left, self.bottom, self.pos_right))
        } else if self.wrap_longitude {
            let unwrapped = self.pos_right - self.neg_left;
            let wrapped = (180.0 - self.pos_left) - (-180.0 - self.neg_right);
            if unwrapped <= wrapped {
                Some((self.top, self.neg_left, self.bottom, self.pos_right))
            } else {
                Some((self.top, self.pos_left, self.bottom, self.neg_right))
            }
        } else {
            Some((self.top, self.neg_left, self.bottom, self.pos_right))
        }
    }
}

impl Internal for GeoBounds {
    fn type_name(&self) -> &'static str {
        "geo_bounds"
    }
    fn class_name(&self) -> &'static str {
        "InternalGeoBounds"
    }
    fn reduce(list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let mut r = GeoBounds::empty(list[0].wrap_longitude);
        for b in &list {
            if b.top > r.top {
                r.top = b.top;
            }
            if b.bottom < r.bottom {
                r.bottom = b.bottom;
            }
            if b.pos_left < r.pos_left {
                r.pos_left = b.pos_left;
            }
            if b.pos_right > r.pos_right {
                r.pos_right = b.pos_right;
            }
            if b.neg_left < r.neg_left {
                r.neg_left = b.neg_left;
            }
            if b.neg_right > r.neg_right {
                r.neg_right = b.neg_right;
            }
        }
        Ok(r)
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        if let Some((top, left, bottom, right)) = self.bbox() {
            let point = |lat: f64, lon: f64| Json::obj().with("lat", Json::Float(lat)).with("lon", Json::Float(lon));
            out.push("bounds", Json::obj().with("top_left", point(top, left)).with("bottom_right", point(bottom, right)));
        }
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        let unknown = |e: &str| AggError::illegal_argument(format!("Found unknown path element [{e}] in [{name}]"));
        // A missing box makes Elasticsearch fail with an NPE; report the path as unsupported.
        let bbox = self.bbox();
        match path.len() {
            1 => {
                let (top, left, bottom, right) = bbox.ok_or_else(|| AggError::new(500, "null_pointer_exception", "bounding box is null"))?;
                match path[0].as_str() {
                    "top" => Ok(Prop::Num(top)),
                    "left" => Ok(Prop::Num(left)),
                    "bottom" => Ok(Prop::Num(bottom)),
                    "right" => Ok(Prop::Num(right)),
                    e => Err(unknown(e)),
                }
            }
            2 => {
                let (top, left, bottom, right) = bbox.ok_or_else(|| AggError::new(500, "null_pointer_exception", "bounding box is null"))?;
                let (lat, lon) = match path[0].as_str() {
                    "top_left" => (top, left),
                    "bottom_right" => (bottom, right),
                    e => return Err(unknown(e)),
                };
                match path[1].as_str() {
                    "lat" => Ok(Prop::Num(lat)),
                    "lon" => Ok(Prop::Num(lon)),
                    e => Err(unknown(e)),
                }
            }
            _ => Err(AggError::illegal_argument(format!("path not supported for [{name}]: {}", java_list(path)))),
        }
    }
}

/// `InternalGeoCentroid`.
#[derive(Clone, Debug)]
pub struct GeoCentroid {
    /// `(lat, lon)`; `None` with no points.
    pub centroid: Option<(f64, f64)>,
    pub count: u64,
    /// Shape centroids' weighted sums (`ShapeData`, latitude first).
    pub shape: Option<ShapeData>,
}

impl Internal for GeoCentroid {
    fn type_name(&self) -> &'static str {
        "geo_centroid"
    }
    fn class_name(&self) -> &'static str {
        "InternalGeoCentroid"
    }
    fn reduce(list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let (centroid, count, shape) = reduce_centroids(list.iter().map(|c| (c.centroid, c.count, c.shape)));
        Ok(GeoCentroid { centroid, count, shape })
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        if let Some((lat, lon)) = self.centroid {
            out.push("location", Json::obj().with("lat", Json::Float(lat)).with("lon", Json::Float(lon)));
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
                let (lat, lon) = point()?;
                Ok(Prop::Value(Json::obj().with("lat", Json::Float(lat)).with("lon", Json::Float(lon))))
            }
            "count" => Ok(Prop::Num(self.count as f64)),
            "lat" => Ok(Prop::Num(point()?.0)),
            "lon" => Ok(Prop::Num(point()?.1)),
            e => Err(AggError::illegal_argument(format!("Found unknown path element [{e}] in [{name}]"))),
        }
    }
}

/// Binds `geo_bounds` / `geo_centroid`.
pub fn create(def: &AggDef, spec: &GeoMetricSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let vs = bind(&spec.vs, ctx.lookup, VsType::GeoPoint, false)?;
    if !vs.unmapped && !matches!(vs.vs_type, VsType::GeoPoint | VsType::GeoShape) {
        let t = if spec.kind == GeoMetricKind::Bounds { "geo_bounds" } else { "geo_centroid" };
        return Err(vs.unsupported(t));
    }
    Ok(Box::new(GeoMetricAgg {
        kind: spec.kind,
        wrap_longitude: spec.wrap_longitude,
        shape: vs.vs_type == VsType::GeoShape,
        shapes: None,
        weights: Vec::new(),
        types: Vec::new(),
        shape_bounds_size: crate::util::BigArraySize::new(),
        vs,
        reader: None,
        bounds: Vec::new(),
        lat: Vec::new(),
        lon: Vec::new(),
        counts: Vec::new(),
        tracker: ctx.tracker(def),
    }))
}

struct GeoMetricAgg {
    kind: GeoMetricKind,
    wrap_longitude: bool,
    /// A `geo_shape` source.
    shape: bool,
    shapes: Option<ShapeReader>,
    /// Shape centroids: summed weights and the bucket's dimension.
    weights: Vec<CompensatedSum>,
    types: Vec<DimensionalShapeType>,
    /// `GeoShapeBoundsAggregator`'s `tops` array: its `buildAggregation` reads it without a
    /// bounds check, so buckets past it fail as in Elasticsearch.
    shape_bounds_size: crate::util::BigArraySize,
    vs: BoundVs,
    reader: Option<GeoReader>,
    /// `[top, bottom, pos_left, pos_right, neg_left, neg_right]` per bucket.
    bounds: Vec<[f64; 6]>,
    lat: Vec<CompensatedSum>,
    lon: Vec<CompensatedSum>,
    counts: Vec<u64>,
    tracker: MemTracker,
}

impl GeoMetricAgg {
    /// `GeoShapeBoundsAggregator` / `GeoShapeCentroidAggregator`.
    fn collect_shapes(&mut self, b: Batch<'_>, shapes: &mut ShapeReader) -> Result<()> {
        for i in 0..b.len() {
            let Some(v) = shapes.get(b.docs[i])? else { continue };
            let ord = b.ords[i];
            let o = ord as usize;
            match self.kind {
                GeoMetricKind::Bounds => {
                    grow(&mut self.bounds, ord, EMPTY_BOUNDS);
                    self.shape_bounds_size.grow(ord as u64 + 1);
                    let bb = v.bounding_box();
                    let s = &mut self.bounds[o];
                    s[0] = java_max(s[0], bb.top);
                    s[1] = java_min(s[1], bb.bottom);
                    s[2] = java_min(s[2], bb.pos_left);
                    s[3] = java_max(s[3], bb.pos_right);
                    s[4] = java_min(s[4], bb.neg_left);
                    s[5] = java_max(s[5], bb.neg_right);
                }
                GeoMetricKind::Centroid => {
                    grow(&mut self.counts, ord, 0);
                    grow(&mut self.lat, ord, CompensatedSum::default());
                    grow(&mut self.lon, ord, CompensatedSum::default());
                    grow(&mut self.weights, ord, CompensatedSum::default());
                    grow(&mut self.types, ord, DimensionalShapeType::Point);
                    self.counts[o] += 1;
                    let t = v.centroid.shape_type;
                    let w = v.centroid.weight;
                    let (x, y) = v.centroid_xy();
                    match self.types[o].cmp(&t) {
                        std::cmp::Ordering::Less => {
                            self.lat[o] = CompensatedSum::new(w * y, 0.0);
                            self.lon[o] = CompensatedSum::new(w * x, 0.0);
                            self.weights[o] = CompensatedSum::new(w, 0.0);
                            self.types[o] = t;
                        }
                        std::cmp::Ordering::Equal => {
                            self.lat[o].add(w * y);
                            self.lon[o].add(w * x);
                            self.weights[o].add(w);
                        }
                        std::cmp::Ordering::Greater => {}
                    }
                }
            }
        }
        self.tracker.ensure(self.bounds.len() * 48 + self.counts.len() * 64)
    }
}

const EMPTY_BOUNDS: [f64; 6] = [f64::NEG_INFINITY, f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY];

impl Aggregator for GeoMetricAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        if self.shape {
            self.shapes = Some(ShapeReader::for_source(seg, &self.vs));
            return Ok(());
        }
        self.reader = if self.vs.unmapped && self.vs.missing.is_none() { None } else { Some(self.vs.geo_reader(seg)?) };
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        if let Some(mut shapes) = self.shapes.take() {
            let r = self.collect_shapes(b, &mut shapes);
            self.shapes = Some(shapes);
            return r;
        }
        let Some(reader) = self.reader.as_mut() else { return Ok(()) };
        for i in 0..b.len() {
            let pts = reader.points(b.docs[i]);
            if pts.is_empty() {
                continue;
            }
            let ord = b.ords[i];
            match self.kind {
                GeoMetricKind::Bounds => {
                    grow(&mut self.bounds, ord, EMPTY_BOUNDS);
                    let s = &mut self.bounds[ord as usize];
                    for &(lat, lon) in pts {
                        s[0] = java_max(s[0], lat);
                        s[1] = java_min(s[1], lat);
                        if lon >= 0.0 {
                            s[2] = java_min(s[2], lon);
                            s[3] = java_max(s[3], lon);
                        } else {
                            s[4] = java_min(s[4], lon);
                            s[5] = java_max(s[5], lon);
                        }
                    }
                }
                GeoMetricKind::Centroid => {
                    grow(&mut self.counts, ord, 0);
                    grow(&mut self.lat, ord, CompensatedSum::default());
                    grow(&mut self.lon, ord, CompensatedSum::default());
                    let o = ord as usize;
                    self.counts[o] += pts.len() as u64;
                    for &(lat, lon) in pts {
                        self.lat[o].add(lat);
                        self.lon[o].add(lon);
                    }
                }
            }
        }
        self.tracker.ensure(self.bounds.len() * 48 + self.counts.len() * 40)?;
        Ok(())
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        if self.shape && self.kind == GeoMetricKind::Bounds {
            for &o in owning {
                self.shape_bounds_size.check(o as u64)?;
            }
        }
        Ok(owning
            .iter()
            .map(|&o| {
                let o = o as usize;
                match self.kind {
                    GeoMetricKind::Bounds => {
                        let s = self.bounds.get(o).copied().unwrap_or(EMPTY_BOUNDS);
                        AggValue::GeoBounds(Box::new(GeoBounds {
                            top: s[0],
                            bottom: s[1],
                            pos_left: s[2],
                            pos_right: s[3],
                            neg_left: s[4],
                            neg_right: s[5],
                            wrap_longitude: self.wrap_longitude,
                        }))
                    }
                    GeoMetricKind::Centroid if self.shape => {
                        if o >= self.counts.len() {
                            return AggValue::GeoCentroid(Box::new(GeoCentroid { centroid: None, count: 0, shape: None }));
                        }
                        let (lat, lon, w) = (self.lat[o].value, self.lon[o].value, self.weights[o].value);
                        let centroid = (w > 0.0).then(|| (lat / w, lon / w));
                        let shape = Some(ShapeData { first: lat, second: lon, weight: w, shape_type: self.types[o] });
                        AggValue::GeoCentroid(Box::new(GeoCentroid { centroid, count: self.counts[o], shape }))
                    }
                    GeoMetricKind::Centroid => {
                        let count = self.counts.get(o).copied().unwrap_or(0);
                        let centroid = (count > 0).then(|| (self.lat[o].value / count as f64, self.lon[o].value / count as f64));
                        AggValue::GeoCentroid(Box::new(GeoCentroid { centroid, count, shape: None }))
                    }
                }
            })
            .collect())
    }

    fn build_empty(&self) -> AggValue {
        match self.kind {
            GeoMetricKind::Bounds => AggValue::GeoBounds(Box::new(GeoBounds::empty(self.wrap_longitude))),
            GeoMetricKind::Centroid => AggValue::GeoCentroid(Box::new(GeoCentroid { centroid: None, count: 0, shape: None })),
        }
    }

    fn validate_sort(&self, _name: &str, _key: Option<&str>, _rest: &[crate::request::PathElement]) -> Result<()> {
        Err(AggError::illegal_argument(
            "Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end.",
        ))
    }
}
