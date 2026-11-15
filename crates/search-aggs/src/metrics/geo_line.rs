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
use crate::metrics::ts_rate::SeriesOrder;
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

/// `CoreValuesSourceType.NUMERIC.getField`'s refusal of a bytes-valued field (keyword, ip).
fn expected_numeric(vs: &BoundVs) -> Option<AggError> {
    let f = vs.field.as_ref()?;
    matches!(vs.vs_type, VsType::Keyword | VsType::Ip)
        .then(|| AggError::illegal_argument(format!("Expected numeric type on field [{}], but got [{}]", f.name, f.type_name)))
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
    // Configs resolve first ("sort" before "point", the `HashMap`'s order), then
    // `validateTimeSeriesConfigs`, then the factory's source checks.
    for vs in sort.iter().chain(point.iter()) {
        if let Some(e) = expected_numeric(vs) {
            return Err(e);
        }
    }
    let time_series = ctx.time_series_request;
    let sort = match sort {
        Some(s) if time_series => {
            let name = s.field.as_ref().map(|f| f.name.as_str()).unwrap_or("");
            if name != TIMESTAMP {
                return Err(AggError::illegal_argument(format!("invalid field [sort]='{name}' configured for time-series aggregations")));
            }
            s
        }
        Some(s) => s,
        None if time_series => {
            let ts = MvsField { field: Some(TIMESTAMP.to_string()), ..Default::default() };
            bind(&ts.to_vs(), ctx.lookup, VsType::Numeric, false)?
        }
        None => return Err(AggError::illegal_argument("missing field [sort] configured for geo_line aggregations")),
    };
    for vs in std::iter::once(&sort).chain(point.iter()) {
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
    if time_series {
        let info = ctx.lookup.time_series();
        return Ok(Box::new(TsGeoLineAgg {
            size: spec.size as usize,
            desc: spec.desc,
            include_sort: spec.include_sort,
            sort,
            point,
            sort_reader: None,
            points: DvNum::None,
            order: SeriesOrder::new(info.as_ref()),
            docs: Vec::new(),
            lines: None,
            tracker: ctx.tracker(def),
        }));
    }
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

/// `DataStream.TIMESTAMP_FIELD_NAME`.
const TIMESTAMP: &str = "@timestamp";

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

    fn validate_sort(&self, name: &str, _key: Option<&str>, rest: &[PathElement]) -> Result<()> {
        Err(crate::collect::unsortable(name, rest))
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
            simplified: false,
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
    /// Lines of different partial results never overlap in sort order (empty results and
    /// time-series lines), so the reduce concatenates them.
    pub non_overlapping: bool,
    /// Built by line simplification (time-series mode), so the reduce simplifies too.
    pub simplified: bool,
}

impl GeoLine {
    fn empty(include_sort: bool, desc: bool, size: usize) -> GeoLine {
        GeoLine { line: Vec::new(), sort_vals: Vec::new(), complete: true, include_sort, desc, size, non_overlapping: true, simplified: false }
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

/// `MergedGeoLines.NonOverlapping`'s `TreeSet`: non-empty lines by first sort value, lines
/// with an equal first value dropped.
fn sorted_by_first(lines: &[GeoLine], desc: bool) -> Vec<&GeoLine> {
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
    sorted
}

/// `MergedGeoLines.NonOverlapping.mergeAndSimplify`: the concatenated lines streamed through a
/// simplifier of the final length.
fn merge_and_simplify(lines: &[GeoLine], final_len: usize, desc: bool) -> Result<(Vec<i64>, Vec<f64>)> {
    let mut simplifier = Simplifier::new(final_len);
    let mut index = 0;
    for l in sorted_by_first(lines, desc) {
        for (&p, &v) in l.line.iter().zip(&l.sort_vals) {
            let x = encode::decode_longitude((p >> 32) as i32);
            let y = encode::decode_latitude(p as i32);
            let id = simplifier.new_point(index, x, y, v);
            simplifier.consume(id)?;
            index += 1;
        }
    }
    Ok(simplifier.produce())
}

/// `MergedGeoLines.NonOverlapping.mergeAndTruncate`: whole lines by their first sort value
/// (lines with equal first values collapse, as in a `TreeSet`).
fn merge_non_overlapping(lines: &[GeoLine], final_len: usize, desc: bool) -> (Vec<i64>, Vec<f64>) {
    let sorted = sorted_by_first(lines, desc);
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
        let simplify = list.iter().any(|l| l.simplified);
        let final_len = merged.min(leader.size);
        let (line, sort_vals) = match (non_overlapping, simplify) {
            (true, false) => merge_non_overlapping(&list, final_len, leader.desc),
            (true, true) => merge_and_simplify(&list, final_len, leader.desc)?,
            (false, false) => merge_overlapping(&list, final_len, leader.desc),
            (false, true) => {
                return Err(AggError::illegal_argument("Unsupported option - simplification of overlapping geo_lines during reduce phase"));
            }
        };
        Ok(GeoLine {
            line,
            sort_vals,
            complete,
            include_sort,
            desc: leader.desc,
            size: leader.size,
            non_overlapping: leader.non_overlapping,
            simplified: leader.simplified,
        })
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


/// One document seen by the time-series `geo_line`: where it goes, where Elasticsearch's
/// time-series search visits it, and what it reads.
#[derive(Clone, Copy)]
struct TsDoc {
    ord: u32,
    series: u32,
    time: i64,
    /// The sort value (`@timestamp`), `None` without one.
    sort: Option<f64>,
    /// `(lon, lat)` of the doc's point, decoded from doc values.
    point: Option<(f64, f64)>,
}

/// `geo_line` in a time-series search (`GeoLineAggregator.TimeSeries` +
/// `TimeSeriesGeoLineBuckets`): Elasticsearch streams each bucket's points, newest first,
/// through a `StreamingGeometrySimplifier` of `size` points and keeps the simplified line
/// instead of the `size` best sort values. Collection here is per segment, so docs are
/// recorded and replayed in `_tsid`/`@timestamp` order at the end.
struct TsGeoLineAgg {
    size: usize,
    desc: bool,
    include_sort: bool,
    sort: BoundVs,
    point: PointSource,
    sort_reader: Option<NumReader>,
    points: DvNum,
    order: SeriesOrder,
    docs: Vec<TsDoc>,
    lines: Option<hashbrown::HashMap<u32, GeoLine>>,
    tracker: MemTracker,
}

impl TsGeoLineAgg {
    /// `TimeSeriesGeoLineBuckets.buildInternalGeoLine`.
    fn line_of(&self, simplifier: &Simplifier) -> GeoLine {
        let (mut line, mut sort_vals) = simplifier.produce();
        let complete = simplifier.length < self.size;
        if !self.desc {
            // Natively newest first.
            line.reverse();
            sort_vals.reverse();
        }
        GeoLine { line, sort_vals, complete, include_sort: self.include_sort, desc: self.desc, size: self.size, non_overlapping: true, simplified: true }
    }

    /// Replays the recorded docs (`Leaf.collect` + `flushBucket`).
    fn replay(&mut self) -> Result<()> {
        if self.lines.is_some() {
            return Ok(());
        }
        let docs = &self.docs;
        let order = self.order.replay_order(docs.len(), |i| (docs[i].series, docs[i].time));
        let mut lines = hashbrown::HashMap::new();
        let mut simplifier = Simplifier::new(self.size);
        let mut current: Option<u32> = None;
        for i in order {
            let d = self.docs[i];
            if current != Some(d.ord) {
                if let Some(c) = current {
                    if lines.contains_key(&c) {
                        return Err(AggError::new(500, "illegal_state_exception", format!("Geoline already exists for bucket {c}")));
                    }
                    lines.insert(c, self.line_of(&simplifier));
                    simplifier.reset();
                }
                current = Some(d.ord);
            }
            simplifier.current_sort = i64::MIN as f64;
            if let Some(v) = d.sort {
                simplifier.current_sort = v;
                if let Some((x, y)) = d.point {
                    simplifier.consume_xy(x, y)?;
                }
            }
        }
        if let Some(c) = current {
            if lines.contains_key(&c) {
                return Err(AggError::new(500, "illegal_state_exception", format!("Geoline already exists for bucket {c}")));
            }
            lines.insert(c, self.line_of(&simplifier));
        }
        self.lines = Some(lines);
        Ok(())
    }
}

impl Aggregator for TsGeoLineAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.sort_reader = if self.sort.has_values() { Some(self.sort.numeric_reader(seg)?) } else { None };
        self.points = match &self.point {
            PointSource::Geo { field: Some(f), .. } => DvNum::open(seg, *f),
            _ => DvNum::None,
        };
        self.order.set_segment(seg);
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        let mut reader = self.sort_reader.take();
        let mut buf = Vec::new();
        let res = (|| {
            for i in 0..b.len() {
                let doc = b.docs[i];
                let sort = match reader.as_mut() {
                    Some(r) => {
                        let vals = r.doubles(doc, b.score(i))?;
                        if vals.len() > 1 {
                            return Err(multi_value());
                        }
                        vals.first().copied()
                    }
                    None => None,
                };
                let mut point = None;
                if sort.is_some() {
                    // `getGeoPointField` fails for a source that isn't a geo point.
                    let missing = match &self.point {
                        PointSource::Geo { missing, .. } => *missing,
                        PointSource::Absent => return Err(AggError::illegal_argument("Could not find field name [point] in multiValuesSource")),
                        PointSource::NotGeo => return Err(AggError::illegal_argument("field [point] is not a geo_point type")),
                    };
                    buf.clear();
                    self.points.for_each(doc, |v| buf.push(v));
                    if buf.len() > 1 {
                        return Err(multi_value());
                    }
                    let encoded = match buf.first() {
                        Some(&v) => Some(lon_lat((v >> 32) as i32, v as i32)),
                        None => missing,
                    };
                    point = encoded.map(|e| (encode::decode_longitude((e >> 32) as i32), encode::decode_latitude(e as i32)));
                }
                let (series, time) = self.order.series_of(doc)?;
                self.docs.push(TsDoc { ord: b.ords[i], series, time, sort, point });
            }
            self.tracker.ensure(self.docs.len() * std::mem::size_of::<TsDoc>() + self.order.bytes())
        })();
        self.sort_reader = reader;
        res
    }

    fn post_collection(&mut self) -> Result<()> {
        self.replay()
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        self.replay()?;
        let lines = self.lines.as_ref().expect("replayed");
        Ok(owning.iter().map(|o| AggValue::GeoLine(Box::new(lines.get(o).cloned().unwrap_or_else(|| GeoLine::empty(self.include_sort, self.desc, self.size))))).collect())
    }

    fn build_empty(&self) -> AggValue {
        AggValue::GeoLine(Box::new(GeoLine::empty(self.include_sort, self.desc, self.size)))
    }

    fn needs_scores(&self) -> bool {
        false
    }

    fn validate_sort(&self, name: &str, _key: Option<&str>, rest: &[PathElement]) -> Result<()> {
        Err(crate::collect::unsortable(name, rest))
    }
}

/// A point of the simplifier (`TimeSeriesGeoLineBuckets.SimplifiablePoint`).
#[derive(Clone, Copy)]
struct SPoint {
    index: usize,
    x: f64,
    y: f64,
    sort: f64,
    encoded: i64,
    error: f64,
}

/// No object in that slot yet.
const NONE: u32 = u32::MAX;

/// Elasticsearch's `StreamingGeometrySimplifier` with the `TRIANGLE_AREA` error: keeps at most
/// `max` points; once full, each new point evicts the point whose removal changes the line's
/// area least. Which point goes on equal errors depends on `java.util.PriorityQueue`'s heap
/// layout and on which point objects are reused, so both are modelled exactly: points live in
/// an arena (ids are Java object identities), `queue` is the binary heap.
struct Simplifier {
    max: usize,
    arena: Vec<SPoint>,
    /// `points[]`: arena ids; slots past `length` keep their objects for reuse.
    points: Vec<u32>,
    length: usize,
    last_removed: u32,
    queue: Vec<u32>,
    current_sort: f64,
}

impl Simplifier {
    fn new(max: usize) -> Simplifier {
        Simplifier { max, arena: Vec::new(), points: vec![NONE; max], length: 0, last_removed: NONE, queue: Vec::new(), current_sort: 0.0 }
    }

    fn reset(&mut self) {
        self.length = 0;
        self.queue.clear();
    }

    fn new_point(&mut self, index: usize, x: f64, y: f64, sort: f64) -> u32 {
        self.arena.push(SPoint { index, x, y, sort, encoded: lon_lat(encode::encode_latitude(y), encode::encode_longitude(x)), error: 0.0 });
        (self.arena.len() - 1) as u32
    }

    fn reset_point(&mut self, id: u32, index: usize, x: f64, y: f64) -> u32 {
        let sort = self.current_sort;
        let p = &mut self.arena[id as usize];
        // `reset` keeps the error.
        p.index = index;
        p.x = x;
        p.y = y;
        p.sort = sort;
        p.encoded = lon_lat(encode::encode_latitude(y), encode::encode_longitude(x));
        id
    }

    /// `makePointErrorFor` + `consume`.
    fn consume_xy(&mut self, x: f64, y: f64) -> Result<()> {
        let index = self.length;
        let id = if index == self.max {
            if self.last_removed == NONE { self.new_point(index, x, y, self.current_sort) } else { self.reset_point(self.last_removed, index, x, y) }
        } else if self.points[index] == NONE {
            self.new_point(index, x, y, self.current_sort)
        } else {
            let id = self.reset_point(self.points[index], index, x, y);
            self.points[index] = id;
            id
        };
        self.consume(id)
    }

    fn consume(&mut self, id: u32) -> Result<()> {
        if self.length > 1 {
            let (l, m) = (self.points[self.length - 2], self.points[self.length - 1]);
            self.arena[m as usize].error = self.error(l, m, id);
            self.offer(m);
        }
        if self.length == self.max {
            let Some(remove) = self.poll() else {
                return Err(AggError::new(500, "no_such_element_exception", String::new()));
            };
            let at = self.arena[remove as usize].index;
            self.remove_and_add(at, id);
        } else {
            self.points[self.length] = id;
            self.length += 1;
        }
        Ok(())
    }

    fn remove_and_add(&mut self, to_remove: usize, id: u32) {
        self.last_removed = self.points[to_remove];
        let max = self.max;
        self.points.copy_within(to_remove + 1..max, to_remove);
        self.points[self.length - 1] = id;
        for i in to_remove..self.length {
            self.arena[self.points[i] as usize].index = i;
        }
        self.update_error_at(to_remove - 1);
        self.update_error_at(to_remove);
        if to_remove < max - 1 {
            self.update_error_at(max - 2);
        }
    }

    fn update_error_at(&mut self, index: usize) {
        if index > 0 && index + 1 < self.length {
            let (l, m, r) = (self.points[index - 1], self.points[index], self.points[index + 1]);
            let error = self.error(l, m, r);
            let p = &mut self.arena[m as usize];
            let delta = (error - p.error).abs();
            p.error = error;
            if delta > 1e-10 && self.remove(m) {
                self.offer(m);
            }
        }
    }

    /// `TriangleAreaCalculator`: Heron's formula over haversine side lengths.
    fn error(&self, left: u32, middle: u32, right: u32) -> f64 {
        let (l, m, r) = (&self.arena[left as usize], &self.arena[middle as usize], &self.arena[right as usize]);
        let dist = |a: &SPoint, b: &SPoint| search_index::util::geo::haversin_meters(a.y, a.x, b.y, b.x);
        let a = dist(l, r);
        let b = dist(r, m);
        let c = dist(m, l);
        let s = 0.5 * (a + b + c);
        let (da, db, dc) = (s - a, s - b, s - c);
        if da >= 0.0 && db >= 0.0 && dc >= 0.0 { (s * da * db * dc).sqrt() } else { 0.0 }
    }

    /// `PointError.compareTo`: `(int) Math.signum(this.error - o.error)`.
    #[inline]
    fn cmp(&self, a: u32, b: u32) -> i32 {
        let d = self.arena[a as usize].error - self.arena[b as usize].error;
        if d > 0.0 {
            1
        } else if d < 0.0 {
            -1
        } else {
            0
        }
    }

    fn sift_up(&mut self, mut k: usize, x: u32) {
        while k > 0 {
            let parent = (k - 1) >> 1;
            let e = self.queue[parent];
            if self.cmp(x, e) >= 0 {
                break;
            }
            self.queue[k] = e;
            k = parent;
        }
        self.queue[k] = x;
    }

    fn sift_down(&mut self, mut k: usize, x: u32) {
        let n = self.queue.len();
        let half = n >> 1;
        while k < half {
            let mut child = 2 * k + 1;
            let mut c = self.queue[child];
            let right = child + 1;
            if right < n && self.cmp(c, self.queue[right]) > 0 {
                child = right;
                c = self.queue[child];
            }
            if self.cmp(x, c) <= 0 {
                break;
            }
            self.queue[k] = c;
            k = child;
        }
        self.queue[k] = x;
    }

    fn offer(&mut self, id: u32) {
        self.queue.push(id);
        let k = self.queue.len() - 1;
        self.sift_up(k, id);
    }

    fn poll(&mut self) -> Option<u32> {
        let result = *self.queue.first()?;
        let x = self.queue.pop().expect("non-empty");
        if !self.queue.is_empty() {
            self.sift_down(0, x);
        }
        Some(result)
    }

    /// `PriorityQueue.remove(Object)` (identity) + `removeAt`.
    fn remove(&mut self, id: u32) -> bool {
        let Some(i) = self.queue.iter().position(|&q| q == id) else { return false };
        let s = self.queue.len() - 1;
        let moved = self.queue.pop().expect("non-empty");
        if s != i {
            self.sift_down(i, moved);
            if self.queue[i] == moved {
                self.sift_up(i, moved);
            }
        }
        true
    }

    /// The kept points, in stream order: encoded points and sort values.
    fn produce(&self) -> (Vec<i64>, Vec<f64>) {
        let ids = &self.points[..self.length];
        (ids.iter().map(|&i| self.arena[i as usize].encoded).collect(), ids.iter().map(|&i| self.arena[i as usize].sort).collect())
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
            simplified: false,
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
