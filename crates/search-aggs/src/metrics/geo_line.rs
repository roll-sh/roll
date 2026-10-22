//! `geo_line` (x-pack spatial): the points of each bucket's documents, ordered by a sort
//! field, as a GeoJSON `LineString` feature.
//!
//! Shard side, each owning bucket keeps at most `size` (sort value, point) entries exactly the
//! way Elasticsearch's `BucketedSort` does, because which of several equal sort values survive
//! — and in what order they are emitted — is visible in the response: entries are first
//! gathered into slots `size - 1, size - 2, …`; once the last slot fills, the slots are
//! heapified (worst entry at the root) and a later document replaces the root only when it is
//! strictly better. A document with a sort value but no point still takes a slot (it just isn't
//! emitted). At build time the kept entries are sorted with Lucene's `IntroSorter`
//! ([`crate::util::intro_sort`]).
//!
//! Elasticsearch keeps the points in a separate `LongArray` that only grows when a point is
//! loaded, so a heap swap touching a slot whose document had no point can read past its end
//! and fail with an `ArrayIndexOutOfBoundsException`; the aggregator models that array's size
//! ([`crate::util::BigArraySize`]) so the same requests fail the same way.
//!
//! Partial results merge with `MergedGeoLines.Overlapping`'s k-way heap merge (truncated to
//! `size`); `complete` tells whether any bucket dropped points. Points are kept in
//! Elasticsearch's `lon << 32 | lat` encoding and rendered rounded to six decimals.

use std::cmp::Ordering;

use search_codec::points::encode;
use search_index::SegmentContext;

use crate::collect::{Aggregator, Batch, BindCtx, grow};
use crate::error::{AggError, Result};
use crate::format::java_round;
use crate::internal::{AggValue, Internal, Prop, ReduceCtx, RenderCtx, java_list};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::metrics::simple::{MvsField, parse_value_type_hint};
use crate::request::{AggDef, PathElement, java_double_compare};
use crate::util::{BigArraySize, IntroSortable, intro_sort};
use crate::values::{BoundVs, DvNum, MissingValue, NumReader, VsType, bind};
use crate::xparser::{Field, Tokens, XParser};

/// `GeoLineAggregationBuilder.MAX_PATH_SIZE`, also the default `size`.
pub const MAX_PATH_SIZE: u32 = 10_000;

/// A parsed `geo_line`.
#[derive(Clone, Debug)]
pub struct GeoLineSpec {
    pub point: Option<MvsField>,
    pub sort: Option<MvsField>,
    pub include_sort: bool,
    /// `sort_order: desc`.
    pub desc: bool,
    pub size: u32,
}

fn parse_field_config(p: &mut XParser<'_>) -> Result<MvsField> {
    crate::metrics::simple::parse_field_config(p, true)
}

/// `GeoLineAggregationBuilder.PARSER`.
pub fn parse(p: &mut XParser<'_>) -> Result<GeoLineSpec> {
    let fields = [
        Field::with_deprecated("value_type", &["valueType"], Tokens::STRING),
        Field::new("format", Tokens::STRING),
        Field::new("point", Tokens::OBJECT),
        Field::new("sort", Tokens::OBJECT),
        Field::new("sort_order", Tokens::STRING),
        Field::new("include_sort", Tokens::BOOLEAN),
        Field::new("size", Tokens::INT),
    ];
    let mut spec = GeoLineSpec { point: None, sort: None, include_sort: false, desc: false, size: MAX_PATH_SIZE };
    p.object("geo_line", &fields, |p, i| {
        match i {
            0 => {
                parse_value_type_hint(p, "geo_line")?;
            }
            1 => {
                p.text()?;
            }
            2 => spec.point = Some(parse_field_config(p)?),
            3 => spec.sort = Some(parse_field_config(p)?),
            4 => {
                let t = p.text()?;
                spec.desc = match t.to_uppercase().as_str() {
                    "ASC" => false,
                    "DESC" => true,
                    other => return Err(AggError::illegal_argument(format!("No enum constant org.elasticsearch.search.sort.SortOrder.{other}"))),
                };
            }
            5 => spec.include_sort = p.boolean_value()?,
            _ => {
                let size = p.int_value()?;
                if size <= 0 || size as u32 > MAX_PATH_SIZE {
                    return Err(AggError::illegal_argument(format!("invalid [size] value [{size}] must be a positive integer <= {MAX_PATH_SIZE}")));
                }
                spec.size = size as u32;
            }
        }
        Ok(())
    })?;
    Ok(spec)
}

/// Where the points come from.
enum PointSource {
    /// A `geo_point` field: its doc values and the encoded `missing` point.
    Geo { field: Option<u32>, missing: Option<i64> },
    /// No `point` configured (`Could not find field name [point]`).
    Absent,
    /// A numeric source (`field [point] is not a geo_point type`).
    NotGeo,
}

/// `GeoLineMultiValuesSource`'s message for sources that are neither numeric nor geo points.
fn unsupported_source(vs: &BoundVs) -> AggError {
    const PKG: &str = "org.elasticsearch.search.aggregations.support";
    let desc = match vs.vs_type {
        VsType::Keyword | VsType::Ip => format!("{PKG}.ValuesSource$Bytes$WithOrdinals$FieldData@0"),
        VsType::Range => format!("{PKG}.ValuesSource$Range@0"),
        _ => format!("{PKG}.ValuesSource@0"),
    };
    AggError::illegal_argument(format!("ValuesSource type {desc}is not supported for multi-valued aggregation"))
}

/// Binds `geo_line` (`GeoLineAggregationBuilder.innerBuild` + `GeoLineAggregatorFactory`).
pub fn create(def: &AggDef, spec: &GeoLineSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let sort = match &spec.sort {
        Some(s) => Some(bind(&s.to_vs(), ctx.lookup, VsType::Numeric, false)?),
        None => None,
    };
    let point = match &spec.point {
        Some(p) => Some(bind(&p.to_vs(), ctx.lookup, VsType::Numeric, false)?),
        None => None,
    };
    let Some(sort) = sort else {
        return Err(AggError::illegal_argument("missing field [sort] configured for geo_line aggregations"));
    };
    for vs in point.iter().chain(std::iter::once(&sort)) {
        if !vs.unmapped && !vs.vs_type.is_numeric() && vs.vs_type != VsType::GeoPoint {
            return Err(unsupported_source(vs));
        }
    }
    if !sort.unmapped && sort.vs_type == VsType::GeoPoint {
        return Err(AggError::illegal_argument("field [sort] is not a numeric type"));
    }
    let point = match point {
        None => PointSource::Absent,
        Some(p) if p.unmapped || p.vs_type != VsType::GeoPoint => PointSource::NotGeo,
        Some(p) => {
            let missing = match p.missing {
                Some(MissingValue::Geo(lat, lon)) => Some(lon_lat(encode::encode_latitude(lat), encode::encode_longitude(lon))),
                _ => None,
            };
            PointSource::Geo { field: p.field.as_ref().map(|f| f.number), missing }
        }
    };
    Ok(Box::new(GeoLineAgg {
        size: spec.size as usize,
        desc: spec.desc,
        include_sort: spec.include_sort,
        sort,
        point,
        sort_reader: None,
        points: DvNum::None,
        buckets: Vec::new(),
        counts: Vec::new(),
        extra: BigArraySize::new(),
        tracker: ctx.tracker(def),
    }))
}

#[inline]
fn lon_lat(lat: i32, lon: i32) -> i64 {
    ((lon as i64) << 32) | (lat as u32 as i64)
}

/// One owning bucket's `BucketedSort` slots.
#[derive(Default)]
struct Slots {
    /// Gather mode: entries in gathering order (entry `k` is slot `size - 1 - k`). Heap mode:
    /// slot order.
    vals: Vec<f64>,
    points: Vec<i64>,
    missing: Vec<bool>,
    heap: bool,
}

struct GeoLineAgg {
    size: usize,
    desc: bool,
    include_sort: bool,
    sort: BoundVs,
    point: PointSource,
    sort_reader: Option<NumReader>,
    points: DvNum,
    buckets: Vec<Slots>,
    counts: Vec<u64>,
    /// The size of `GeoLineBucketedSort.Extra.values`, a `LongArray` shared by every bucket
    /// (bucket `b`'s slot `s` is index `b * size + s`).
    extra: BigArraySize,
    tracker: MemTracker,
}

impl GeoLineAgg {
    /// `betterThan` / `docBetterThan`: `reverseMul * Double.compare(a, b) < 0`.
    #[inline]
    fn better(&self, a: f64, b: f64) -> bool {
        let c = java_double_compare(a, b);
        if self.desc { c == Ordering::Greater } else { c == Ordering::Less }
    }

    /// `Extra.loader`: the doc's point, or `None` when it has none.
    fn load_point(&self, doc: u32, out: &mut Vec<i64>) -> Result<Option<i64>> {
        let missing = match &self.point {
            PointSource::Geo { missing, .. } => *missing,
            PointSource::Absent => return Err(AggError::illegal_argument("Could not find field name [point] in multiValuesSource")),
            PointSource::NotGeo => return Err(AggError::illegal_argument("field [point] is not a geo_point type")),
        };
        out.clear();
        self.points.for_each(doc, |v| out.push(v));
        if out.len() > 1 {
            return Err(multi_value());
        }
        Ok(match out.first() {
            // Doc values hold `lat << 32 | lon`.
            Some(&v) => Some(lon_lat((v >> 32) as i32, v as i32)),
            None => missing,
        })
    }

    /// `BucketedSort.downHeap(rootIndex, parent)` over one bucket's slots (`root`: the bucket's
    /// first index in the shared arrays).
    fn down_heap(&self, s: &mut Slots, root: u64, mut parent: usize) -> Result<()> {
        let n = self.size;
        loop {
            let mut worst = parent;
            let left = parent * 2 + 1;
            if left < n {
                if self.better(s.vals[worst], s.vals[left]) {
                    worst = left;
                }
                let right = left + 1;
                if right < n && self.better(s.vals[worst], s.vals[right]) {
                    worst = right;
                }
            }
            if worst == parent {
                break;
            }
            // `extra.swap(worstIndex, parentIndex)` reads both points.
            self.extra.check(root + worst as u64)?;
            self.extra.check(root + parent as u64)?;
            s.vals.swap(worst, parent);
            s.points.swap(worst, parent);
            s.missing.swap(worst, parent);
            parent = worst;
        }
        Ok(())
    }
}

fn multi_value() -> AggError {
    AggError::illegal_argument("Encountered more than one value for a single document. Use a script to combine multiple values per doc into a single value.")
}

impl Aggregator for GeoLineAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.sort_reader = if self.sort.has_values() { Some(self.sort.numeric_reader(seg)?) } else { None };
        self.points = match &self.point {
            PointSource::Geo { field: Some(f), .. } => DvNum::open(seg, *f),
            _ => DvNum::None,
        };
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        let mut reader = self.sort_reader.take();
        let mut buf = Vec::new();
        let res = (|| {
            for i in 0..b.len() {
                let (doc, ord) = (b.docs[i], b.ords[i]);
                let o = ord as usize;
                let value = match reader.as_mut() {
                    Some(r) => {
                        let vals = r.doubles(doc, b.score(i))?;
                        if vals.len() > 1 {
                            return Err(multi_value());
                        }
                        vals.first().copied()
                    }
                    None => None,
                };
                if let Some(v) = value {
                    if self.buckets.len() <= o {
                        self.buckets.resize_with(o + 1, Slots::default);
                    }
                    let mut slots = std::mem::take(&mut self.buckets[o]);
                    let root = o as u64 * self.size as u64;
                    let r = (|| -> Result<()> {
                        if slots.heap {
                            if self.better(v, slots.vals[0]) {
                                let p = self.load_point(doc, &mut buf)?;
                                if p.is_some() {
                                    self.extra.grow(root + 1);
                                }
                                slots.vals[0] = v;
                                slots.missing[0] = p.is_none();
                                slots.points[0] = p.unwrap_or(0);
                                self.down_heap(&mut slots, root, 0)?;
                            }
                        } else {
                            let slot = self.size - 1 - slots.vals.len();
                            let p = self.load_point(doc, &mut buf)?;
                            if p.is_some() {
                                self.extra.grow(root + slot as u64 + 1);
                            }
                            slots.vals.push(v);
                            slots.missing.push(p.is_none());
                            slots.points.push(p.unwrap_or(0));
                            if slot == 0 {
                                // The last slot filled: slot order, then heapify.
                                slots.vals.reverse();
                                slots.points.reverse();
                                slots.missing.reverse();
                                slots.heap = true;
                                for parent in (0..self.size / 2).rev() {
                                    self.down_heap(&mut slots, root, parent)?;
                                }
                            }
                        }
                        Ok(())
                    })();
                    self.buckets[o] = slots;
                    r?;
                }
                grow(&mut self.counts, ord, 0);
                self.counts[o] += 1;
            }
            let entries: usize = self.buckets.iter().map(|s| s.vals.capacity()).sum();
            self.tracker.ensure(entries * 17 + self.counts.len() * 8)
        })();
        self.sort_reader = reader;
        res
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        Ok(owning.iter().map(|&o| self.build_one(o as usize)).collect())
    }

    fn build_empty(&self) -> AggValue {
        AggValue::GeoLine(Box::new(GeoLine::empty(self.include_sort, self.desc, self.size)))
    }

    fn needs_scores(&self) -> bool {
        self.sort.script.as_ref().is_some_and(|s| s.needs_score())
    }

    fn validate_sort(&self, name: &str, _key: Option<&str>, _rest: &[PathElement]) -> Result<()> {
        Err(AggError::illegal_argument(format!(
            "Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end. [{name}] is not single-bucket."
        )))
    }
}

impl GeoLineAgg {
    fn build_one(&self, o: usize) -> AggValue {
        if o >= self.counts.len() {
            return self.build_empty();
        }
        let complete = self.counts[o] <= self.size as u64;
        let (mut vals, mut points) = (Vec::new(), Vec::new());
        if let Some(s) = self.buckets.get(o) {
            // Slots `start..end` in slot order (gathered entries come out newest first).
            let order: Box<dyn Iterator<Item = usize>> = if s.heap { Box::new(0..s.vals.len()) } else { Box::new((0..s.vals.len()).rev()) };
            for k in order {
                if !s.missing[k] {
                    vals.push(s.vals[k]);
                    points.push(s.points[k]);
                }
            }
        }
        let n = vals.len();
        let mut sorter = PathSorter { points: &mut points, vals: &mut vals, desc: self.desc, pivot: 0.0 };
        intro_sort(&mut sorter, 0, n);
        AggValue::GeoLine(Box::new(GeoLine {
            line: points,
            sort_vals: vals,
            complete,
            include_sort: self.include_sort,
            desc: self.desc,
            size: self.size,
            non_overlapping: false,
        }))
    }
}

/// `PathArraySorter`: points and sort values, by sort value.
struct PathSorter<'a> {
    points: &'a mut [i64],
    vals: &'a mut [f64],
    desc: bool,
    pivot: f64,
}

impl IntroSortable for PathSorter<'_> {
    fn swap(&mut self, i: usize, j: usize) {
        self.points.swap(i, j);
        self.vals.swap(i, j);
    }
    fn set_pivot(&mut self, i: usize) {
        self.pivot = self.vals[i];
    }
    fn compare_pivot(&self, j: usize) -> Ordering {
        if self.desc { java_double_compare(self.vals[j], self.pivot) } else { java_double_compare(self.pivot, self.vals[j]) }
    }
}

/// `InternalGeoLine`.
#[derive(Clone, Debug)]
pub struct GeoLine {
    /// `encodedLon << 32 | encodedLat` per point, in sort order.
    pub line: Vec<i64>,
    pub sort_vals: Vec<f64>,
    pub complete: bool,
    pub include_sort: bool,
    pub desc: bool,
    pub size: usize,
    /// Built by an aggregator that saw no bucket (`buildEmptyAggregation`).
    pub non_overlapping: bool,
}

impl GeoLine {
    fn empty(include_sort: bool, desc: bool, size: usize) -> GeoLine {
        GeoLine { line: Vec::new(), sort_vals: Vec::new(), complete: true, include_sort, desc, size, non_overlapping: true }
    }
}

/// `MergedGeoLines.Overlapping.merge`: a binary heap of line cursors, ordered by the cursor's
/// sort value (ties keep heap order, as in Elasticsearch).
fn merge_overlapping(lines: &[GeoLine], final_len: usize, desc: bool) -> (Vec<i64>, Vec<f64>) {
    let val = |c: (usize, usize)| lines[c.0].sort_vals[c.1];
    // `correctOrdering(i, j)`: `i`'s value comes after `j`'s in the sort order.
    let correct_ordering = |a: (usize, usize), b: (usize, usize)| if desc { val(a) < val(b) } else { val(a) > val(b) };
    let mut heap: Vec<(usize, usize)> = Vec::with_capacity(lines.len());
    let push = |heap: &mut Vec<(usize, usize)>, c: (usize, usize)| {
        heap.push(c);
        let mut i = heap.len() - 1;
        while i > 0 && correct_ordering(heap[(i - 1) / 2], heap[i]) {
            heap.swap((i - 1) / 2, i);
            i = (i - 1) / 2;
        }
    };
    for (i, l) in lines.iter().enumerate() {
        if !l.line.is_empty() {
            push(&mut heap, (i, 0));
        }
    }
    let (mut points, mut vals) = (Vec::with_capacity(final_len), Vec::with_capacity(final_len));
    while points.len() < final_len && !heap.is_empty() {
        let top = heap[0];
        points.push(lines[top.0].line[top.1]);
        vals.push(val(top));
        // `removeTop`: last to the root, then heapify down.
        let last = heap.pop().expect("non-empty");
        if !heap.is_empty() {
            heap[0] = last;
            let mut i = 0;
            loop {
                let left = 2 * i + 1;
                if left >= heap.len() {
                    break;
                }
                let mut child = left;
                if left + 1 < heap.len() && !correct_ordering(heap[left + 1], heap[left]) {
                    child = left + 1;
                }
                if correct_ordering(heap[child], heap[i]) {
                    break;
                }
                heap.swap(child, i);
                i = child;
            }
        }
        if top.1 + 1 < lines[top.0].line.len() {
            push(&mut heap, (top.0, top.1 + 1));
        }
    }
    (points, vals)
}

/// `MergedGeoLines.NonOverlapping.mergeAndTruncate`: whole lines by their first sort value
/// (lines with equal first values collapse, as in a `TreeSet`).
fn merge_non_overlapping(lines: &[GeoLine], final_len: usize, desc: bool) -> (Vec<i64>, Vec<f64>) {
    let mut sorted: Vec<&GeoLine> = Vec::new();
    for l in lines.iter().filter(|l| !l.line.is_empty()) {
        let key = l.sort_vals[0];
        if !sorted.iter().any(|s| java_double_compare(s.sort_vals[0], key) == Ordering::Equal) {
            sorted.push(l);
        }
    }
    sorted.sort_by(|a, b| {
        let c = java_double_compare(a.sort_vals[0], b.sort_vals[0]);
        if desc { c.reverse() } else { c }
    });
    let (mut points, mut vals) = (Vec::with_capacity(final_len), Vec::with_capacity(final_len));
    for l in sorted {
        for (p, v) in l.line.iter().zip(&l.sort_vals) {
            if points.len() >= final_len {
                break;
            }
            points.push(*p);
            vals.push(*v);
        }
    }
    (points, vals)
}

/// `InternalGeoLine.roundDegrees`.
fn round_degrees(d: f64) -> f64 {
    java_round(d * 1e6) as f64 / 1e6
}

impl Internal for GeoLine {
    fn type_name(&self) -> &'static str {
        "geo_line"
    }

    fn class_name(&self) -> &'static str {
        "InternalGeoLine"
    }

    fn must_reduce_single(&self) -> bool {
        true
    }

    fn reduce(list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let leader = &list[0];
        let merged: usize = list.iter().map(|l| l.line.len()).sum();
        let complete = list.iter().all(|l| l.complete) && merged <= leader.size;
        let include_sort = list.iter().all(|l| l.include_sort);
        let non_overlapping = list.iter().all(|l| l.non_overlapping);
        let final_len = merged.min(leader.size);
        let (line, sort_vals) =
            if non_overlapping { merge_non_overlapping(&list, final_len, leader.desc) } else { merge_overlapping(&list, final_len, leader.desc) };
        Ok(GeoLine { line, sort_vals, complete, include_sort, desc: leader.desc, size: leader.size, non_overlapping: leader.non_overlapping })
    }

    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        let coords: Vec<Json> = self
            .line
            .iter()
            .map(|&p| {
                let lon = encode::decode_longitude((p >> 32) as i32);
                let lat = encode::decode_latitude(p as i32);
                Json::Arr(vec![Json::Float(round_degrees(lon)), Json::Float(round_degrees(lat))])
            })
            .collect();
        // A `HashMap`: "coordinates" iterates before "type".
        let geometry = if coords.len() == 1 {
            Json::obj().with("coordinates", coords.into_iter().next().expect("one")).with("type", Json::str("Point"))
        } else {
            Json::obj().with("coordinates", Json::Arr(coords)).with("type", Json::str("LineString"))
        };
        out.push("type", Json::str("Feature"));
        out.push("geometry", geometry);
        let mut props = Json::obj().with("complete", Json::Bool(self.complete));
        if self.include_sort {
            props.push("sort_values", Json::Arr(self.sort_vals.iter().map(|&v| Json::Float(v)).collect()));
        }
        out.push("properties", props);
    }

    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        if path.len() == 1 && path[0] == "value" {
            return Ok(Prop::Value(Json::Arr(self.line.iter().map(|&p| Json::Int(p)).collect())));
        }
        Err(AggError::illegal_argument(format!("path not supported for [{name}]: {}", java_list(path))))
    }

    fn ram_bytes(&self) -> usize {
        64 + self.line.len() * 16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(points: &[(i32, i32)], vals: &[f64]) -> GeoLine {
        GeoLine {
            line: points.iter().map(|&(lat, lon)| lon_lat(lat, lon)).collect(),
            sort_vals: vals.to_vec(),
            complete: true,
            include_sort: true,
            desc: false,
            size: 4,
            non_overlapping: false,
        }
    }

    #[test]
    fn overlapping_merge_interleaves_and_truncates() {
        let a = line(&[(1, 1), (2, 2), (3, 3)], &[1.0, 4.0, 6.0]);
        let b = line(&[(10, 10), (20, 20)], &[2.0, 5.0]);
        let (pts, vals) = merge_overlapping(&[a, b], 4, false);
        assert_eq!(vals, [1.0, 2.0, 4.0, 5.0]);
        assert_eq!(pts[1], lon_lat(10, 10));
    }

    #[test]
    fn lon_lat_packing() {
        let v = lon_lat(-5, 7);
        assert_eq!((v >> 32) as i32, 7);
        assert_eq!(v as i32, -5);
        assert_eq!(round_degrees(2.2500004), 2.25);
        assert_eq!(round_degrees(-1.0000006), -1.000001);
    }
}
