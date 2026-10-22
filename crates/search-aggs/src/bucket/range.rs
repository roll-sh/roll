//! Range aggregations: `range`, `date_range`, `geo_distance` (numeric ranges over values,
//! dates or distances), `ip_range` (byte ranges over IPs) and `ip_prefix` (IPs grouped by
//! network).
//!
//! Numeric ranges are sorted by `(from, to)` like Elasticsearch's `AbstractRangeBuilder`, and a
//! doc's ascending values walk them with Elasticsearch's overlap-aware binary search (each range
//! counts a doc once). Bucket ordinals are `owning * ranges + range`, so the per-range doc
//! counts are one dense array. Every range is a bucket, including empty ones; the reduce sums
//! buckets position by position.

use std::sync::Arc;

use search_index::SegmentContext;

use crate::collect::{Aggregator, Batch, BindCtx, Subs, grow};
use crate::error::{AggError, Result};
use crate::field::{NumericType, SourceType};
use crate::format::{DocValueFormat, ip_to_string, parse_ip};
use crate::internal::{AggValue, Aggs, Internal, MultiBuckets, Prop, ReduceCtx, RenderCtx, multi_bucket_property};
use crate::json::{Json, java_double};
use crate::memory::MemTracker;
use crate::request::AggDef;
use crate::util::BytesOrdMap;
use crate::values::{BoundVs, BytesReader, DocCounter, GeoReader, NumReader, VsOpts, VsSpec, VsType, bind, parse_geo_point_text};
use crate::xparser::{Field, Token, Tokens, XParser};

/// One requested range.
#[derive(Clone, Debug, PartialEq)]
pub struct RangeDef {
    pub key: Option<String>,
    pub from: Option<f64>,
    pub from_str: Option<String>,
    pub to: Option<f64>,
    pub to_str: Option<String>,
}

/// Which numeric range aggregation.
#[derive(Clone, Debug, PartialEq)]
pub enum RangeKind {
    Range,
    DateRange,
    GeoDistance { origin: (f64, f64), unit_meters: f64, plane: bool },
}

impl RangeKind {
    pub fn type_name(&self) -> &'static str {
        match self {
            RangeKind::Range => "range",
            RangeKind::DateRange => "date_range",
            RangeKind::GeoDistance { .. } => "geo_distance",
        }
    }
}

/// A parsed `range` / `date_range` / `geo_distance`.
#[derive(Clone, Debug)]
pub struct RangeSpec {
    pub kind: RangeKind,
    pub vs: VsSpec,
    pub ranges: Vec<RangeDef>,
    pub keyed: bool,
}

fn parse_range_def(p: &mut XParser<'_>) -> Result<RangeDef> {
    let fields = [
        Field::new("key", Tokens::FLOAT),
        Field::new("from", Tokens::DOUBLE_OR_NULL),
        Field::new("to", Tokens::DOUBLE_OR_NULL),
    ];
    let mut d = RangeDef { key: None, from: None, from_str: None, to: None, to_str: None };
    p.object("range", &fields, |p, i| {
        if i == 0 {
            d.key = Some(p.text()?);
            return Ok(());
        }
        let (n, s) = match p.token() {
            Token::String => (None, Some(p.text()?)),
            Token::Number => (Some(p.double_value()?), None),
            _ => (None, None),
        };
        if i == 1 {
            d.from = n;
            d.from_str = s;
        } else {
            d.to = n;
            d.to_str = s;
        }
        Ok(())
    })?;
    Ok(d)
}

/// `GeoDistanceAggregationBuilder.parseRange`: a hand-rolled parser (numbers or numeric strings,
/// `from` defaulting to 0) whose key is fixed at parse time (`*` for a zero or missing `from`).
fn parse_geo_range_def(p: &mut XParser<'_>) -> Result<RangeDef> {
    let (mut from, mut to) = (0.0f64, f64::INFINITY);
    let (mut from_str, mut to_str, mut key): (Option<String>, Option<String>, Option<String>) = (None, None, None);
    let mut name = String::new();
    while let Some(t) = p.next_token() {
        let unknown_field = |p: &XParser<'_>, name: &str| p.parsing_error(format!("Failed to parse object: unknown field [{name}] found"));
        match t {
            Token::EndObject => break,
            Token::FieldName => name = p.current_name().unwrap_or("").to_string(),
            Token::Number => match name.as_str() {
                "from" => from = p.double_value()?,
                "to" => to = p.double_value()?,
                _ => return Err(unknown_field(p, &name)),
            },
            Token::String => match name.as_str() {
                "key" => key = Some(p.text()?),
                "from" => from_str = Some(p.text()?),
                "to" => to_str = Some(p.text()?),
                _ => return Err(unknown_field(p, &name)),
            },
            Token::Null => {
                if !matches!(name.as_str(), "from" | "to" | "key") {
                    return Err(unknown_field(p, &name));
                }
            }
            other => return Err(p.parsing_error(format!("Failed to parse object: unexpected token [{other}] found"))),
        }
    }
    if from_str.is_some() || to_str.is_some() {
        // Double.parseDouble(null) throws when only one bound is a string.
        let parse = |s: &Option<String>| match s {
            Some(s) => crate::xparser::java_parse_double(s),
            None => Err(AggError::new(500, "null_pointer_exception", "Cannot invoke \"String.trim()\" because \"in\" is null")),
        };
        from = parse(&from_str)?;
        to = parse(&to_str)?;
    }
    let key = key.unwrap_or_else(|| {
        let f = if from == 0.0 { "*".to_string() } else { java_double(from) };
        let t = if to.is_infinite() { "*".to_string() } else { java_double(to) };
        format!("{f}-{t}")
    });
    Ok(RangeDef { key: Some(key), from: Some(from), from_str: None, to: Some(to), to_str: None })
}

/// `DistanceUnit.fromString`: meters per unit.
pub fn distance_unit(s: &str) -> Result<f64> {
    Ok(match s {
        "in" | "inch" => 0.0254,
        "yd" | "yards" => 0.9144,
        "ft" | "feet" => 0.3048,
        "km" | "kilometers" => 1000.0,
        "NM" | "nmi" | "nauticalmiles" => 1852.0,
        "mm" | "millimeters" => 0.001,
        "cm" | "centimeters" => 0.01,
        "mi" | "miles" => 1609.344,
        "m" | "meters" => 1.0,
        _ => return Err(AggError::illegal_argument(format!("No distance unit match [{s}]"))),
    })
}

pub fn parse_range(p: &mut XParser<'_>, type_name: &str) -> Result<RangeSpec> {
    let geo = type_name == "geo_distance";
    let date = type_name == "date_range";
    let opts = VsOpts::new(true, !geo, date);
    let mut fields = VsSpec::fields(opts);
    fields.push(Field::new("keyed", Tokens::BOOLEAN));
    fields.push(Field::new("ranges", Tokens::OBJECT_ARRAY));
    if geo {
        fields.push(Field::new("unit", Tokens::STRING));
        fields.push(Field::new("distance_type", Tokens::STRING));
        fields.push(Field::new("origin", Tokens::OBJECT_ARRAY_OR_STRING));
    }
    let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
    let mut vs = VsSpec::default();
    let mut ranges = Vec::new();
    let mut keyed = false;
    let mut unit = 1.0;
    let mut plane = false;
    let mut origin: Option<(f64, f64)> = None;
    p.object(type_name, &fields, |p, i| {
        let f = names[i];
        if vs.parse_field(p, f)? {
            return Ok(());
        }
        match f {
            "keyed" => keyed = p.boolean_value()?,
            "ranges" => {
                ranges.extend(p.array_items(|p| {
                    if p.token() != Token::StartObject {
                        return Err(p.parsing_error(format!("[ranges] must contain objects, but hit a {}", p.token())));
                    }
                    if geo { parse_geo_range_def(p) } else { parse_range_def(p) }
                })?);
            }
            "unit" => unit = distance_unit(&p.text()?)?,
            "distance_type" => {
                let t = p.text()?;
                plane = match t.to_ascii_lowercase().as_str() {
                    "plane" => true,
                    "arc" => false,
                    _ => return Err(AggError::illegal_argument(format!("No geo distance for [{t}]"))),
                };
            }
            _ => origin = Some(parse_origin(p)?),
        }
        Ok(())
    })?;
    vs.check_required(opts)?;
    let kind = if geo {
        let origin = origin.ok_or_else(|| AggError::illegal_argument(format!("[origin] must not be null: [{type_name}]")))?;
        RangeKind::GeoDistance { origin, unit_meters: unit, plane }
    } else if date {
        RangeKind::DateRange
    } else {
        RangeKind::Range
    };
    Ok(RangeSpec { kind, vs, ranges, keyed })
}

fn parse_origin(p: &mut XParser<'_>) -> Result<(f64, f64)> {
    match p.token() {
        Token::String => parse_geo_point_text(&Json::Str(p.text()?)),
        Token::StartArray => {
            let mut lon = f64::NAN;
            let mut lat = f64::NAN;
            loop {
                match p.next_token() {
                    Some(Token::EndArray) | None => break,
                    _ => {
                        if lon.is_nan() {
                            lon = p.double_value()?;
                        } else if lat.is_nan() {
                            lat = p.double_value()?;
                        } else {
                            return Err(p.parsing_error("malformed [origin]: a geo point array must be of the form [lon, lat]"));
                        }
                    }
                }
            }
            Ok((lat, lon))
        }
        _ => {
            let mut lat = f64::NAN;
            let mut lon = f64::NAN;
            let mut name = String::new();
            loop {
                match p.next_token() {
                    Some(Token::EndObject) | None => break,
                    Some(Token::FieldName) => name = p.current_name().unwrap_or("").to_string(),
                    Some(Token::Number) => {
                        if name == "lat" {
                            lat = p.double_value()?;
                        } else if name == "lon" {
                            lon = p.double_value()?;
                        }
                    }
                    Some(Token::StartObject | Token::StartArray) => p.skip_children(),
                    _ => {}
                }
            }
            if lat.is_nan() || lon.is_nan() {
                return Err(p.parsing_error(format!("malformed [{name}] geo point object. either [lat] or [lon] (or both) are missing")));
            }
            Ok((lat, lon))
        }
    }
}

/// A resolved numeric range.
#[derive(Clone, Debug)]
struct NumRange {
    key: Option<String>,
    from: f64,
    to: f64,
    orig_from: f64,
    orig_to: f64,
}

/// `reduceToStoredPrecision` of the field's type.
fn fix_precision(vs: &BoundVs, v: f64) -> f64 {
    if v.is_infinite() {
        return v;
    }
    match vs.field.as_ref().map(|f| f.source) {
        Some(SourceType::Numeric(NumericType::Float)) => v as f32 as f64,
        Some(SourceType::Numeric(NumericType::HalfFloat)) => {
            let bits = search_codec::points::encode::f16_bits(v as f32);
            search_codec::points::encode::f16_to_f32(bits) as f64
        }
        _ => v,
    }
}

pub fn create_range(def: &AggDef, spec: &RangeSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let geo = matches!(spec.kind, RangeKind::GeoDistance { .. });
    let default = match spec.kind {
        RangeKind::Range => VsType::Numeric,
        RangeKind::DateRange => VsType::Date,
        RangeKind::GeoDistance { .. } => VsType::GeoPoint,
    };
    let vs = bind(&spec.vs, ctx.lookup, default, spec.kind == RangeKind::DateRange)?;
    // `range` (not `date_range`) also reads pre-aggregated `histogram` fields.
    let histogram = spec.kind == RangeKind::Range && matches!(vs.vs_type, VsType::Histogram | VsType::ExponentialHistogram) && !vs.unmapped;
    if !vs.unmapped {
        let ok = if geo { vs.vs_type == VsType::GeoPoint } else { vs.vs_type.is_numeric() || histogram };
        if !ok {
            return Err(vs.unsupported(spec.kind.type_name()));
        }
    }
    let now = ctx.lookup.now_millis();
    let format = if geo { DocValueFormat::Raw } else { vs.format.clone() };
    let mut ranges = Vec::with_capacity(spec.ranges.len());
    for r in &spec.ranges {
        let from = match &r.from_str {
            Some(s) => format.parse_double(s, false, now)?,
            None => r.from.unwrap_or(f64::NEG_INFINITY),
        };
        let to = match &r.to_str {
            Some(s) => format.parse_double(s, false, now)?,
            None => r.to.unwrap_or(f64::INFINITY),
        };
        ranges.push(NumRange { key: r.key.clone(), from: fix_precision(&vs, from), to: fix_precision(&vs, to), orig_from: from, orig_to: to });
    }
    if ranges.is_empty() {
        return Err(AggError::illegal_argument(format!("No [ranges] specified for the [{}] aggregation", def.name)));
    }
    ranges.sort_by(|a, b| crate::request::java_double_compare(a.from, b.from).then(crate::request::java_double_compare(a.to, b.to)));
    let mut max_to = Vec::with_capacity(ranges.len());
    for (i, r) in ranges.iter().enumerate() {
        max_to.push(if i == 0 { r.to } else { r.to.max(max_to[i - 1]) });
    }
    let subs = Subs::bind(def, ctx)?;
    if histogram && !subs.is_empty() {
        let fields = if vs.vs_type == VsType::ExponentialHistogram { "exponential_histogram" } else { "histogram" };
        return Err(AggError::illegal_argument(format!("Range aggregation on {fields} fields does not support sub-aggregations")));
    }
    let kind = match &spec.kind {
        RangeKind::Range => RangeResultKind::Range,
        RangeKind::DateRange => RangeResultKind::DateRange,
        RangeKind::GeoDistance { .. } => RangeResultKind::GeoDistance,
    };
    let counters = if ctx.whole_segments() && subs.is_empty() && !geo && !histogram && ctx.doc_count_field.is_none() {
        range_counters(&vs, &ranges, ctx)?
    } else {
        None
    };
    Ok(Box::new(RangeAgg {
        kind,
        counters,
        segment_done: false,
        source: match &spec.kind {
            RangeKind::GeoDistance { origin, unit_meters, plane } => Src::Geo { origin: *origin, unit: *unit_meters, plane: *plane, reader: None },
            _ if histogram => Src::Histogram(None),
            _ => Src::Num(None),
        },
        vs,
        ranges: Arc::new(ranges),
        max_to,
        keyed: spec.keyed,
        format,
        counts: Vec::new(),
        subs,
        doc_counter: DocCounter::none(),
        doc_count_field: ctx.doc_count_field,
        tracker: ctx.tracker(def),
        dist: Vec::new(),
        matched: Vec::new(),
    }))
}

enum Src {
    /// A pre-aggregated `histogram` field.
    Histogram(Option<crate::histogram_field::PreAggReader>),
    Num(Option<NumReader>),
    Geo { origin: (f64, f64), unit: f64, plane: bool, reader: Option<GeoReader> },
}

struct RangeAgg {
    kind: RangeResultKind,
    /// Top-level, without sub-aggregations, under a query matching every doc: one range query
    /// per range whose `count` gives a segment's bucket counts without visiting its docs
    /// (Elasticsearch's filter-by-filter ranges, counted from the points index).
    counters: Option<Vec<Box<dyn search_index::Weight>>>,
    segment_done: bool,
    source: Src,
    vs: BoundVs,
    ranges: Arc<Vec<NumRange>>,
    max_to: Vec<f64>,
    keyed: bool,
    format: DocValueFormat,
    counts: Vec<u64>,
    subs: Subs,
    doc_counter: DocCounter,
    doc_count_field: Option<u32>,
    tracker: MemTracker,
    dist: Vec<f64>,
    matched: Vec<u32>,
}

/// The range queries of [`RangeAgg::counters`], for fields whose range queries select exactly
/// the values the buckets do (integers, doubles, dates: no float rounding).
fn range_counters(vs: &BoundVs, ranges: &[NumRange], ctx: &BindCtx<'_>) -> Result<Option<Vec<Box<dyn search_index::Weight>>>> {
    use crate::field::{NumericType, SourceType};
    let Some(f) = vs.field.as_ref() else { return Ok(None) };
    if vs.script.is_some() || vs.missing.is_some() || f.counter {
        return Ok(None);
    }
    let date = match f.source {
        SourceType::Numeric(NumericType::Long | NumericType::Integer | NumericType::Short | NumericType::Byte | NumericType::Double) => false,
        SourceType::Date { .. } => true,
        _ => return Ok(None),
    };
    let num = |v: f64| if v.fract() == 0.0 && v.abs() < 9.007_199_254_740_992e15 { Json::Int(v as i64) } else { Json::Float(v) };
    let mut out = Vec::with_capacity(ranges.len());
    for r in ranges {
        let mut b = Json::obj();
        if r.from.is_finite() {
            b.push("gte", num(r.from));
        } else if r.from.is_nan() {
            return Ok(None);
        }
        if r.to.is_finite() {
            b.push("lt", num(r.to));
        } else if r.to.is_nan() {
            return Ok(None);
        }
        if date {
            b.push("format", "epoch_millis");
        }
        let q = Json::obj().with("range", Json::obj().with(f.name.as_str(), b));
        let Ok(query) = ctx.lookup.query(&q) else { return Ok(None) };
        out.push(query.weight(ctx.searcher, search_index::ScoreMode::CompleteNoScores, 1.0)?);
    }
    Ok(Some(out))
}

/// Elasticsearch's `RangeAggregator.Overlap.collect` for one value: the ranges (indexes) it
/// matches from `low` on; returns the next low bound.
fn match_value(ranges: &[NumRange], max_to: &[f64], value: f64, low: usize, out: &mut Vec<u32>) -> usize {
    let n = ranges.len();
    if low >= n {
        return low;
    }
    let (mut lo, mut hi) = (low as i64, n as i64 - 1);
    let mut mid = (lo + hi) / 2;
    while lo <= hi {
        if value < ranges[mid as usize].from {
            hi = mid - 1;
        } else if value >= max_to[mid as usize] {
            lo = mid + 1;
        } else {
            break;
        }
        mid = (lo + hi) / 2;
    }
    if lo > hi {
        return lo as usize;
    }
    let (mut start_lo, mut start_hi) = (lo, mid);
    while start_lo <= start_hi {
        let m = (start_lo + start_hi) / 2;
        if value >= max_to[m as usize] {
            start_lo = m + 1;
        } else {
            start_hi = m - 1;
        }
    }
    let (mut end_lo, mut end_hi) = (mid, hi);
    while end_lo <= end_hi {
        let m = (end_lo + end_hi) / 2;
        if value < ranges[m as usize].from {
            end_hi = m - 1;
        } else {
            end_lo = m + 1;
        }
    }
    for i in start_lo..=end_hi {
        let r = &ranges[i as usize];
        if value >= r.from && value < r.to {
            out.push(i as u32);
        }
    }
    (end_hi + 1) as usize
}

impl RangeAgg {
    /// `HistoBackedRangeAggregator`: each centroid counts its count in every range it falls
    /// in, and sends the doc to that range's sub-aggregations (once per centroid).
    fn collect_histograms(&mut self, b: Batch<'_>) -> Result<()> {
        let Src::Histogram(Some(mut hist)) = std::mem::replace(&mut self.source, Src::Histogram(None)) else { return Ok(()) };
        let n = self.ranges.len() as u32;
        let ranges = self.ranges.clone();
        let r = (|| {
            for i in 0..b.len() {
                let doc = b.docs[i];
                let base = b.ords[i] * n;
                for &(v, c) in hist.centroids(doc)? {
                    for (k, range) in ranges.iter().enumerate() {
                        if v >= range.from && v < range.to {
                            let ord = base + k as u32;
                            if self.counts.len() <= ord as usize {
                                grow(&mut self.counts, ord, 0);
                                self.tracker.ensure(self.counts.len() * 8)?;
                            }
                            self.counts[ord as usize] += c;
                            if !self.subs.is_empty() {
                                self.subs.push(doc, ord, b.score(i))?;
                            }
                        }
                    }
                }
            }
            Ok(())
        })();
        self.source = Src::Histogram(Some(hist));
        r
    }
}

impl Aggregator for RangeAgg {
    fn segment_done(&self) -> bool {
        self.segment_done
    }

    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.segment_done = false;
        if let Some(ws) = &self.counters
            && !seg.has_deletions()
        {
            let mut counts = Vec::with_capacity(ws.len());
            for w in ws {
                match w.count(seg)? {
                    Some(c) => counts.push(c),
                    None => break,
                }
            }
            if counts.len() == ws.len() {
                if self.counts.len() < counts.len() {
                    self.counts.resize(counts.len(), 0);
                    self.tracker.ensure(self.counts.len() * 8)?;
                }
                for (k, c) in counts.into_iter().enumerate() {
                    self.counts[k] += c as u64;
                }
                self.segment_done = true;
            }
        }
        self.doc_counter = DocCounter::open(seg, self.doc_count_field);
        let live = !(self.vs.unmapped && self.vs.missing.is_none());
        match &mut self.source {
            Src::Num(r) => *r = if live { Some(self.vs.numeric_reader(seg)?) } else { None },
            Src::Geo { reader, .. } => *reader = if live { Some(self.vs.geo_reader(seg)?) } else { None },
            Src::Histogram(r) => *r = Some(crate::histogram_field::PreAggReader::open(seg, &self.vs)),
        }
        self.subs.set_segment(seg)
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        if self.segment_done {
            return Ok(());
        }
        let n = self.ranges.len() as u32;
        let ranges = self.ranges.clone();
        if let Src::Histogram(Some(_)) = &self.source {
            return self.collect_histograms(b);
        }
        for i in 0..b.len() {
            let doc = b.docs[i];
            self.matched.clear();
            match &mut self.source {
                Src::Num(None) | Src::Geo { reader: None, .. } | Src::Histogram(_) => return Ok(()),
                Src::Num(Some(r)) => {
                    let vals = r.doubles(doc, b.score(i))?;
                    let mut low = 0;
                    for &v in vals {
                        low = match_value(&ranges, &self.max_to, v, low, &mut self.matched);
                    }
                }
                Src::Geo { origin, unit, plane, reader: Some(r) } => {
                    self.dist.clear();
                    for &(lat, lon) in r.points(doc) {
                        let m = if *plane {
                            search_index::util::geo::plane_meters(origin.0, origin.1, lat, lon)
                        } else {
                            search_index::util::geo::haversin_meters(origin.0, origin.1, lat, lon)
                        };
                        self.dist.push(m / *unit);
                    }
                    self.dist.sort_by(|a, b| a.total_cmp(b));
                    let mut low = 0;
                    for &v in &self.dist {
                        low = match_value(&ranges, &self.max_to, v, low, &mut self.matched);
                    }
                }
            }
            if self.matched.is_empty() {
                continue;
            }
            let base = b.ords[i] * n;
            let c = self.doc_counter.count(doc);
            for k in 0..self.matched.len() {
                let ord = base + self.matched[k];
                if self.counts.len() <= ord as usize {
                    grow(&mut self.counts, ord, 0);
                    self.tracker.ensure(self.counts.len() * 8)?;
                }
                self.counts[ord as usize] += c;
                if !self.subs.is_empty() {
                    self.subs.push(doc, ord, b.score(i))?;
                }
            }
        }
        Ok(())
    }

    fn finish_segment(&mut self) -> Result<()> {
        self.subs.finish_segment()
    }

    fn post_collection(&mut self) -> Result<()> {
        self.subs.post_collection()
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        let n = self.ranges.len() as u32;
        let all: Vec<u32> = owning.iter().flat_map(|&o| (0..n).map(move |i| o * n + i)).collect();
        let mut aggs = self.subs.build(&all)?.into_iter();
        let mut out = Vec::with_capacity(owning.len());
        for &o in owning {
            let buckets = self
                .ranges
                .iter()
                .enumerate()
                .map(|(i, r)| RangeBucket {
                    key: r.key.clone(),
                    from: r.orig_from,
                    to: r.orig_to,
                    from_ip: None,
                    to_ip: None,
                    doc_count: self.counts.get((o * n + i as u32) as usize).copied().unwrap_or(0),
                    aggs: aggs.next().unwrap_or_default(),
                })
                .collect();
            out.push(AggValue::Range(Box::new(Range { kind: self.kind, buckets, keyed: self.keyed, format: self.format.clone() })));
        }
        Ok(out)
    }

    fn build_empty(&self) -> AggValue {
        let empty = self.subs.build_empty();
        let buckets = self
            .ranges
            .iter()
            .map(|r| RangeBucket { key: r.key.clone(), from: r.orig_from, to: r.orig_to, from_ip: None, to_ip: None, doc_count: 0, aggs: empty.clone() })
            .collect();
        AggValue::Range(Box::new(Range { kind: self.kind, buckets, keyed: self.keyed, format: self.format.clone() }))
    }

    fn needs_scores(&self) -> bool {
        self.subs.needs_scores() || self.vs.script.as_ref().is_some_and(|s| s.needs_score())
    }
}

/// Which range result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeResultKind {
    Range,
    DateRange,
    GeoDistance,
    IpRange,
}

/// One range bucket.
#[derive(Clone, Debug)]
pub struct RangeBucket {
    pub key: Option<String>,
    pub from: f64,
    pub to: f64,
    /// IP range bounds (`ip_range`).
    pub from_ip: Option<Vec<u8>>,
    pub to_ip: Option<Vec<u8>>,
    pub doc_count: u64,
    pub aggs: Aggs,
}

/// `InternalRange` / `InternalDateRange` / `InternalGeoDistance` / `InternalBinaryRange`.
#[derive(Clone, Debug)]
pub struct Range {
    pub kind: RangeResultKind,
    pub buckets: Vec<RangeBucket>,
    pub keyed: bool,
    pub format: DocValueFormat,
}

impl Range {
    fn bound_string(&self, v: f64) -> String {
        match self.format.format_double(v) {
            Json::Str(s) => s,
            Json::Float(f) => java_double(f),
            other => other.to_java_string(),
        }
    }

    /// `getKeyAsString()`.
    pub fn key_string(&self, b: &RangeBucket) -> String {
        if let Some(k) = &b.key {
            return k.clone();
        }
        if self.kind == RangeResultKind::IpRange {
            let f = |v: &Option<Vec<u8>>| v.as_ref().map(|b| ip_to_string(b)).unwrap_or_else(|| "*".into());
            return format!("{}-{}", f(&b.from_ip), f(&b.to_ip));
        }
        let from = if b.from.is_infinite() { "*".to_string() } else { self.bound_string(b.from) };
        let to = if b.to.is_infinite() { "*".to_string() } else { self.bound_string(b.to) };
        format!("{from}-{to}")
    }

    fn render_bucket(&self, b: &RangeBucket, rc: &RenderCtx) -> (String, Json) {
        let key = self.key_string(b);
        let mut o = Json::obj();
        if !self.keyed {
            o.push("key", Json::Str(key.clone()));
        }
        if self.kind == RangeResultKind::IpRange {
            if let Some(f) = &b.from_ip {
                o.push("from", Json::Str(ip_to_string(f)));
            }
            if let Some(t) = &b.to_ip {
                o.push("to", Json::Str(ip_to_string(t)));
            }
        } else {
            if !b.from.is_infinite() {
                o.push("from", Json::Float(b.from));
                if !self.format.is_raw() {
                    o.push("from_as_string", self.format.format_double(b.from));
                }
            }
            if !b.to.is_infinite() {
                o.push("to", Json::Float(b.to));
                if !self.format.is_raw() {
                    o.push("to_as_string", self.format.format_double(b.to));
                }
            }
        }
        o.push("doc_count", Json::from(b.doc_count));
        b.aggs.render_into(&mut o, rc);
        (key, o)
    }
}

impl Internal for Range {
    fn type_name(&self) -> &'static str {
        match self.kind {
            RangeResultKind::Range => "range",
            RangeResultKind::DateRange => "date_range",
            RangeResultKind::GeoDistance => "geo_distance",
            RangeResultKind::IpRange => "ip_range",
        }
    }
    fn class_name(&self) -> &'static str {
        match self.kind {
            RangeResultKind::Range => "InternalRange",
            RangeResultKind::DateRange => "InternalDateRange",
            RangeResultKind::GeoDistance => "InternalGeoDistance",
            RangeResultKind::IpRange => "InternalBinaryRange",
        }
    }
    fn must_reduce_single(&self) -> bool {
        true
    }
    fn reduce(list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let mut it = list.into_iter();
        let mut first = it.next().expect("non-empty");
        let rest: Vec<Range> = it.collect();
        let n = first.buckets.len();
        let mut out = Vec::with_capacity(n);
        for (i, b) in first.buckets.drain(..).enumerate() {
            let mut doc_count = b.doc_count;
            let mut lists = vec![b.aggs];
            for r in &rest {
                if let Some(o) = r.buckets.get(i) {
                    doc_count += o.doc_count;
                    lists.push(o.aggs.clone());
                }
            }
            let aggs = Aggs::reduce_hashed(lists, ctx)?;
            out.push(RangeBucket { aggs, doc_count, ..b });
        }
        ctx.consume_buckets(out.len())?;
        first.buckets = out;
        Ok(first)
    }
    fn render(&self, out: &mut Json, rc: &RenderCtx) {
        if self.keyed {
            out.push("buckets", Json::Obj(self.buckets.iter().map(|b| self.render_bucket(b, rc)).collect()));
        } else {
            out.push("buckets", Json::Arr(self.buckets.iter().map(|b| self.render_bucket(b, rc).1).collect()));
        }
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_bucket_property(self, name, path)
    }
}

impl MultiBuckets for Range {
    fn len(&self) -> usize {
        self.buckets.len()
    }
    fn doc_count(&self, i: usize) -> u64 {
        self.buckets[i].doc_count
    }
    fn key(&self, i: usize) -> Json {
        Json::Str(self.key_string(&self.buckets[i]))
    }
    fn key_string(&self, i: usize) -> String {
        Range::key_string(self, &self.buckets[i])
    }
    fn aggs(&self, i: usize) -> &Aggs {
        &self.buckets[i].aggs
    }
    fn aggs_mut(&mut self, i: usize) -> &mut Aggs {
        &mut self.buckets[i].aggs
    }
    fn retain_buckets(&mut self, keep: &[bool]) {
        let mut i = 0;
        self.buckets.retain(|_| {
            i += 1;
            keep[i - 1]
        });
    }
    fn reorder(&mut self, order: &[usize]) {
        let mut slots: Vec<Option<RangeBucket>> = self.buckets.drain(..).map(Some).collect();
        self.buckets = order.iter().map(|&i| slots[i].take().expect("bucket")).collect();
    }
}

/// One `ip_range` range as requested.
#[derive(Clone, Debug, PartialEq)]
pub struct IpRangeDef {
    pub key: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

/// A parsed `ip_range`.
#[derive(Clone, Debug)]
pub struct IpRangeSpec {
    pub vs: VsSpec,
    pub ranges: Vec<IpRangeDef>,
    pub keyed: bool,
}

/// `InetAddresses.parseCidr` → the range's `(from, to)` strings (`to` exclusive).
fn cidr_range(mask: &str) -> Result<(Option<String>, Option<String>)> {
    let bad = || AggError::illegal_argument(format!("Expected [ip/prefix] but was [{mask}]"));
    let (ip, len) = mask.split_once('/').ok_or_else(bad)?;
    let len: u32 = len.parse().map_err(|_| bad())?;
    let v4 = !ip.contains(':');
    let bytes = parse_ip(ip)?;
    let total = if v4 { 32 } else { 128 };
    if len > total {
        return Err(AggError::illegal_argument(format!("Illegal prefix length [{len}] in [{mask}]. Must be 0-{total} for IPv{}", if v4 { 4 } else { 6 })));
    }
    let mut lower = bytes;
    let mut upper = bytes;
    let start_bit = if v4 { 96 } else { 0 } + len as usize;
    for i in start_bit..128 {
        let m = 1u8 << (7 - (i & 7));
        lower[i >> 3] &= !m;
        upper[i >> 3] |= m;
    }
    let addr_str = |b: &[u8; 16]| ip_to_string(b);
    // `InetAddressPoint.MIN_VALUE` is `::`; an IPv4 range never starts there.
    let from = if !v4 && lower.iter().all(|x| *x == 0) { None } else { Some(addr_str(&lower)) };
    let to = if !v4 && upper.iter().all(|x| *x == 0xff) {
        None
    } else {
        let mut next = upper;
        for i in (0..16).rev() {
            if next[i] == 0xff {
                next[i] = 0;
            } else {
                next[i] += 1;
                break;
            }
        }
        Some(addr_str(&next))
    };
    Ok((from, to))
}

pub fn parse_ip_range(p: &mut XParser<'_>) -> Result<IpRangeSpec> {
    let opts = VsOpts::new(false, false, false);
    let mut fields = VsSpec::fields(opts);
    fields.push(Field::new("keyed", Tokens::BOOLEAN));
    fields.push(Field::new("ranges", Tokens::OBJECT_ARRAY));
    let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
    let mut vs = VsSpec::default();
    let mut ranges = Vec::new();
    let mut keyed = false;
    p.object("ip_range", &fields, |p, i| {
        let f = names[i];
        if vs.parse_field(p, f)? {
            return Ok(());
        }
        if f == "keyed" {
            keyed = p.boolean_value()?;
            return Ok(());
        }
        ranges.extend(p.array_items(|p| {
            if p.token() != Token::StartObject {
                return Err(p.parsing_error(format!("[ranges] must contain objects, but hit a {}", p.token())));
            }
            let (mut key, mut from, mut to, mut mask) = (None, None, None, None);
            loop {
                match p.next_token() {
                    Some(Token::EndObject) | None => break,
                    Some(Token::FieldName) => {}
                    Some(_) => match p.current_name().unwrap_or("") {
                        "key" => key = Some(p.text()?),
                        "from" => from = p.text_or_null()?,
                        "to" => to = p.text_or_null()?,
                        "mask" => mask = Some(p.text()?),
                        other => return Err(p.parsing_error(format!("Unexpected ip range parameter: [{other}]"))),
                    },
                }
            }
            if let Some(m) = mask {
                let (f, t) = cidr_range(&m)?;
                Ok(IpRangeDef { key: Some(key.unwrap_or(m)), from: f, to: t })
            } else {
                for s in [&from, &to].into_iter().flatten() {
                    parse_ip(s)?;
                }
                Ok(IpRangeDef { key, from, to })
            }
        })?);
        Ok(())
    })?;
    vs.check_required(opts)?;
    Ok(IpRangeSpec { vs, ranges, keyed })
}

#[derive(Clone, Debug)]
struct IpRangeR {
    key: Option<String>,
    from: Option<[u8; 16]>,
    to: Option<[u8; 16]>,
}

pub fn create_ip_range(def: &AggDef, spec: &IpRangeSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let vs = bind(&spec.vs, ctx.lookup, VsType::Ip, false)?;
    if !vs.unmapped && vs.vs_type != VsType::Ip {
        return Err(vs.unsupported("ip_range"));
    }
    if spec.ranges.is_empty() {
        return Err(AggError::illegal_argument(format!("No [ranges] specified for the [{}] aggregation", def.name)));
    }
    let mut ranges = Vec::new();
    for r in &spec.ranges {
        ranges.push(IpRangeR {
            key: r.key.clone(),
            from: r.from.as_deref().map(parse_ip).transpose()?,
            to: r.to.as_deref().map(parse_ip).transpose()?,
        });
    }
    // BinaryRangeAggregator sorts by from (null first) then to (null last).
    ranges.sort_by(|a, b| {
        let f = match (&a.from, &b.from) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, _) => std::cmp::Ordering::Less,
            (_, None) => std::cmp::Ordering::Greater,
            (Some(x), Some(y)) => x.cmp(y),
        };
        f.then_with(|| match (&a.to, &b.to) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, _) => std::cmp::Ordering::Greater,
            (_, None) => std::cmp::Ordering::Less,
            (Some(x), Some(y)) => x.cmp(y),
        })
    });
    let subs = Subs::bind(def, ctx)?;
    Ok(Box::new(IpRangeAgg {
        vs,
        ranges: Arc::new(ranges),
        keyed: spec.keyed,
        reader: None,
        counts: Vec::new(),
        subs,
        doc_counter: DocCounter::none(),
        doc_count_field: ctx.doc_count_field,
        matched: Vec::new(),
        tracker: ctx.tracker(def),
    }))
}

struct IpRangeAgg {
    vs: BoundVs,
    ranges: Arc<Vec<IpRangeR>>,
    keyed: bool,
    reader: Option<BytesReader>,
    counts: Vec<u64>,
    subs: Subs,
    doc_counter: DocCounter,
    doc_count_field: Option<u32>,
    matched: Vec<bool>,
    tracker: MemTracker,
}

impl Aggregator for IpRangeAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.doc_counter = DocCounter::open(seg, self.doc_count_field);
        self.reader = if self.vs.unmapped && self.vs.missing.is_none() { None } else { Some(self.vs.bytes_reader(seg)?) };
        self.subs.set_segment(seg)
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        let Some(r) = self.reader.as_mut() else { return Ok(()) };
        let n = self.ranges.len();
        self.matched.resize(n, false);
        for i in 0..b.len() {
            let doc = b.docs[i];
            let k = r.load(doc, b.score(i))?;
            if k == 0 {
                continue;
            }
            self.matched.iter_mut().for_each(|m| *m = false);
            for j in 0..k {
                let v = r.value(j);
                for (ri, rg) in self.ranges.iter().enumerate() {
                    let ge = rg.from.as_ref().is_none_or(|f| v >= f.as_slice());
                    let lt = rg.to.as_ref().is_none_or(|t| v < t.as_slice());
                    if ge && lt {
                        self.matched[ri] = true;
                    }
                }
            }
            let c = self.doc_counter.count(doc);
            for ri in 0..n {
                if !self.matched[ri] {
                    continue;
                }
                let ord = b.ords[i] * n as u32 + ri as u32;
                if self.counts.len() <= ord as usize {
                    grow(&mut self.counts, ord, 0);
                    self.tracker.ensure(self.counts.len() * 8)?;
                }
                self.counts[ord as usize] += c;
                if !self.subs.is_empty() {
                    self.subs.push(doc, ord, b.score(i))?;
                }
            }
        }
        Ok(())
    }

    fn finish_segment(&mut self) -> Result<()> {
        self.subs.finish_segment()
    }

    fn post_collection(&mut self) -> Result<()> {
        self.subs.post_collection()
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        let n = self.ranges.len() as u32;
        let all: Vec<u32> = owning.iter().flat_map(|&o| (0..n).map(move |i| o * n + i)).collect();
        let mut aggs = self.subs.build(&all)?.into_iter();
        let mut out = Vec::new();
        for &o in owning {
            let buckets = self
                .ranges
                .iter()
                .enumerate()
                .map(|(i, r)| RangeBucket {
                    key: r.key.clone(),
                    from: f64::NEG_INFINITY,
                    to: f64::INFINITY,
                    from_ip: r.from.map(|b| b.to_vec()),
                    to_ip: r.to.map(|b| b.to_vec()),
                    doc_count: self.counts.get((o * n + i as u32) as usize).copied().unwrap_or(0),
                    aggs: aggs.next().unwrap_or_default(),
                })
                .collect();
            out.push(AggValue::Range(Box::new(Range { kind: RangeResultKind::IpRange, buckets, keyed: self.keyed, format: DocValueFormat::Ip })));
        }
        Ok(out)
    }

    fn build_empty(&self) -> AggValue {
        let empty = self.subs.build_empty();
        let buckets = self
            .ranges
            .iter()
            .map(|r| RangeBucket {
                key: r.key.clone(),
                from: f64::NEG_INFINITY,
                to: f64::INFINITY,
                from_ip: r.from.map(|b| b.to_vec()),
                to_ip: r.to.map(|b| b.to_vec()),
                doc_count: 0,
                aggs: empty.clone(),
            })
            .collect();
        AggValue::Range(Box::new(Range { kind: RangeResultKind::IpRange, buckets, keyed: self.keyed, format: DocValueFormat::Ip }))
    }

    fn needs_scores(&self) -> bool {
        self.subs.needs_scores()
    }
}

/// A parsed `ip_prefix`.
#[derive(Clone, Debug)]
pub struct IpPrefixSpec {
    pub vs: VsSpec,
    pub prefix_length: Option<i32>,
    pub is_ipv6: bool,
    pub append_prefix_length: bool,
    pub keyed: bool,
    pub min_doc_count: u64,
}

pub fn parse_ip_prefix(p: &mut XParser<'_>) -> Result<IpPrefixSpec> {
    let opts = VsOpts::new(false, false, false);
    let mut fields = VsSpec::fields(opts);
    fields.extend_from_slice(&[
        Field::new("prefix_length", Tokens::INT),
        Field::new("is_ipv6", Tokens::BOOLEAN),
        Field::new("min_doc_count", Tokens::LONG),
        Field::new("append_prefix_length", Tokens::BOOLEAN),
        Field::new("keyed", Tokens::BOOLEAN),
    ]);
    let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
    let mut s = IpPrefixSpec { vs: VsSpec::default(), prefix_length: None, is_ipv6: false, append_prefix_length: false, keyed: false, min_doc_count: 1 };
    p.object("ip_prefix", &fields, |p, i| {
        let f = names[i];
        if s.vs.parse_field(p, f)? {
            return Ok(());
        }
        match f {
            "prefix_length" => {
                let v = p.int_value()?;
                if v < 0 {
                    return Err(AggError::illegal_argument(format!(
                        "[prefix_length] must be in range [0, {}] while value is [{v}]",
                        if s.is_ipv6 { 128 } else { 32 }
                    )));
                }
                s.prefix_length = Some(v);
            }
            "is_ipv6" => s.is_ipv6 = p.boolean_value()?,
            "min_doc_count" => {
                let v = p.long_value()?;
                if v < 1 {
                    return Err(AggError::illegal_argument(format!("[min_doc_count] must be in range [1, 2147483647] while value is [{v}]")));
                }
                s.min_doc_count = v as u64;
            }
            "append_prefix_length" => s.append_prefix_length = p.boolean_value()?,
            _ => s.keyed = p.boolean_value()?,
        }
        Ok(())
    })?;
    s.vs.check_required(opts)?;
    if s.prefix_length.is_none() {
        return Err(AggError::illegal_argument("Required [prefix_length]"));
    }
    Ok(s)
}

/// `extractNetmask(prefixLength, isIpv6)`: 4 bytes for IPv4, 16 for IPv6.
pub fn netmask(prefix: u32, v6: bool) -> Vec<u8> {
    let mut m = vec![0u8; if v6 { 16 } else { 4 }];
    let bytes = (prefix / 8) as usize;
    for b in m.iter_mut().take(bytes) {
        *b = 0xff;
    }
    let bits = prefix % 8;
    if bits > 0 {
        let mut rem = 0u8;
        for j in 0..bits {
            rem |= 1 << (7 - j);
        }
        m[bytes] = rem;
    }
    m
}

pub fn create_ip_prefix(def: &AggDef, spec: &IpPrefixSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let vs = bind(&spec.vs, ctx.lookup, VsType::Ip, false)?;
    if !vs.unmapped && vs.vs_type != VsType::Ip {
        return Err(vs.unsupported("ip_prefix"));
    }
    let pl = spec.prefix_length.unwrap_or(0);
    let max = if spec.is_ipv6 { 128 } else { 32 };
    if pl < 0 || pl > max {
        return Err(AggError::illegal_argument(format!("[prefix_length] must be in range [0, {max}] while value is [{pl}]")));
    }
    let subs = Subs::bind(def, ctx)?;
    Ok(Box::new(IpPrefixAgg {
        mask: netmask(pl as u32, spec.is_ipv6),
        cfg: Arc::new(IpPrefixConfig {
            is_ipv6: spec.is_ipv6,
            prefix_length: pl as u32,
            append_prefix_length: spec.append_prefix_length,
            keyed: spec.keyed,
            min_doc_count: spec.min_doc_count,
        }),
        vs,
        reader: None,
        ords: BytesOrdMap::new(),
        counts: Vec::new(),
        subs,
        doc_counter: DocCounter::none(),
        doc_count_field: ctx.doc_count_field,
        tracker: ctx.tracker(def),
    }))
}

/// Settings of `ip_prefix` results.
#[derive(Debug)]
pub struct IpPrefixConfig {
    pub is_ipv6: bool,
    pub prefix_length: u32,
    pub append_prefix_length: bool,
    pub keyed: bool,
    pub min_doc_count: u64,
}

struct IpPrefixAgg {
    mask: Vec<u8>,
    cfg: Arc<IpPrefixConfig>,
    vs: BoundVs,
    reader: Option<BytesReader>,
    ords: BytesOrdMap,
    counts: Vec<u64>,
    subs: Subs,
    doc_counter: DocCounter,
    doc_count_field: Option<u32>,
    tracker: MemTracker,
}

impl Aggregator for IpPrefixAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.doc_counter = DocCounter::open(seg, self.doc_count_field);
        self.reader = if self.vs.unmapped && self.vs.missing.is_none() { None } else { Some(self.vs.bytes_reader(seg)?) };
        self.subs.set_segment(seg)
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        let Some(r) = self.reader.as_mut() else { return Ok(()) };
        let mut subnet = vec![0u8; self.mask.len()];
        let mut prev = vec![0u8; self.mask.len()];
        let off = if self.mask.len() == 4 { 12 } else { 0 };
        for i in 0..b.len() {
            let doc = b.docs[i];
            let k = r.load(doc, b.score(i))?;
            let mut have_prev = false;
            for j in 0..k {
                let v = r.value(j);
                if v.len() != 16 {
                    continue;
                }
                for (x, m) in self.mask.iter().enumerate() {
                    subnet[x] = v[x + off] & m;
                }
                if have_prev && subnet == prev {
                    continue;
                }
                have_prev = true;
                prev.copy_from_slice(&subnet);
                let (ord, _) = self.ords.add(b.ords[i], &subnet);
                grow(&mut self.counts, ord, 0);
                self.counts[ord as usize] += self.doc_counter.count(doc);
                if !self.subs.is_empty() {
                    self.subs.push(doc, ord, b.score(i))?;
                }
            }
        }
        self.tracker.ensure(self.counts.len() * 8 + self.ords.ram_bytes())?;
        Ok(())
    }

    fn finish_segment(&mut self) -> Result<()> {
        self.subs.finish_segment()
    }

    fn post_collection(&mut self) -> Result<()> {
        self.subs.post_collection()
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        let mut per: Vec<Vec<u32>> = Vec::new();
        for &o in owning {
            let mut ords: Vec<u32> = (0..self.ords.len() as u32).filter(|&x| self.ords.owning(x) == o).collect();
            ords.sort_by(|&a, &b| self.ords.get_bytes(a).cmp(self.ords.get_bytes(b)));
            per.push(ords);
        }
        let all: Vec<u32> = per.iter().flatten().copied().collect();
        let mut aggs = self.subs.build(&all)?.into_iter();
        Ok(per
            .into_iter()
            .map(|ords| {
                let buckets = ords
                    .iter()
                    .map(|&x| IpPrefixBucket { key: self.ords.get_bytes(x).to_vec(), doc_count: self.counts[x as usize], aggs: aggs.next().unwrap_or_default() })
                    .collect();
                AggValue::IpPrefix(Box::new(IpPrefix { cfg: self.cfg.clone(), buckets }))
            })
            .collect())
    }

    fn build_empty(&self) -> AggValue {
        AggValue::IpPrefix(Box::new(IpPrefix { cfg: self.cfg.clone(), buckets: Vec::new() }))
    }

    fn needs_scores(&self) -> bool {
        self.subs.needs_scores()
    }
}

/// An `ip_prefix` bucket.
#[derive(Clone, Debug)]
pub struct IpPrefixBucket {
    pub key: Vec<u8>,
    pub doc_count: u64,
    pub aggs: Aggs,
}

/// `InternalIpPrefix`.
#[derive(Clone, Debug)]
pub struct IpPrefix {
    pub cfg: Arc<IpPrefixConfig>,
    pub buckets: Vec<IpPrefixBucket>,
}

impl IpPrefix {
    fn key_string(&self, b: &IpPrefixBucket) -> String {
        let k = ip_to_string(&b.key);
        if self.cfg.append_prefix_length { format!("{k}/{}", self.cfg.prefix_length) } else { k }
    }
}

impl Internal for IpPrefix {
    fn type_name(&self) -> &'static str {
        "ip_prefix"
    }
    fn class_name(&self) -> &'static str {
        "InternalIpPrefix"
    }
    fn must_reduce_single(&self) -> bool {
        true
    }
    fn reduce(list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let cfg = list[0].cfg.clone();
        let mut all: Vec<IpPrefixBucket> = list.into_iter().flat_map(|l| l.buckets).collect();
        all.sort_by(|a, b| a.key.cmp(&b.key));
        let mut out: Vec<IpPrefixBucket> = Vec::new();
        let mut group: Vec<IpPrefixBucket> = Vec::new();
        let flush = |group: &mut Vec<IpPrefixBucket>, out: &mut Vec<IpPrefixBucket>, ctx: &mut ReduceCtx<'_>| -> Result<()> {
            if group.is_empty() {
                return Ok(());
            }
            let key = group[0].key.clone();
            let doc_count = group.iter().map(|b| b.doc_count).sum();
            let aggs = Aggs::reduce_hashed(group.drain(..).map(|b| b.aggs).collect(), ctx)?;
            if !ctx.is_final() || doc_count >= cfg.min_doc_count {
                out.push(IpPrefixBucket { key, doc_count, aggs });
            }
            Ok(())
        };
        for b in all {
            if group.first().is_some_and(|g| g.key != b.key) {
                flush(&mut group, &mut out, ctx)?;
            }
            group.push(b);
        }
        flush(&mut group, &mut out, ctx)?;
        ctx.consume_buckets(out.len())?;
        Ok(IpPrefix { cfg, buckets: out })
    }
    fn render(&self, out: &mut Json, rc: &RenderCtx) {
        let render_bucket = |b: &IpPrefixBucket| -> (String, Json) {
            let key = self.key_string(b);
            let mut o = Json::obj();
            if !self.cfg.keyed {
                o.push("key", Json::Str(key.clone()));
            }
            if !self.cfg.is_ipv6 {
                o.push("netmask", Json::Str(ip_to_string(&netmask(self.cfg.prefix_length, false))));
            }
            o.push("doc_count", Json::from(b.doc_count));
            o.push("is_ipv6", Json::Bool(self.cfg.is_ipv6));
            o.push("prefix_length", Json::from(self.cfg.prefix_length));
            b.aggs.render_into(&mut o, rc);
            (key, o)
        };
        if self.cfg.keyed {
            out.push("buckets", Json::Obj(self.buckets.iter().map(render_bucket).collect()));
        } else {
            out.push("buckets", Json::Arr(self.buckets.iter().map(|b| render_bucket(b).1).collect()));
        }
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_bucket_property(self, name, path)
    }
}

impl MultiBuckets for IpPrefix {
    fn len(&self) -> usize {
        self.buckets.len()
    }
    fn doc_count(&self, i: usize) -> u64 {
        self.buckets[i].doc_count
    }
    fn key(&self, i: usize) -> Json {
        Json::Str(self.key_string(&self.buckets[i]))
    }
    fn key_string(&self, i: usize) -> String {
        IpPrefix::key_string(self, &self.buckets[i])
    }
    fn aggs(&self, i: usize) -> &Aggs {
        &self.buckets[i].aggs
    }
    fn aggs_mut(&mut self, i: usize) -> &mut Aggs {
        &mut self.buckets[i].aggs
    }
    fn retain_buckets(&mut self, keep: &[bool]) {
        let mut i = 0;
        self.buckets.retain(|_| {
            i += 1;
            keep[i - 1]
        });
    }
    fn reorder(&mut self, order: &[usize]) {
        let mut slots: Vec<Option<IpPrefixBucket>> = self.buckets.drain(..).map(Some).collect();
        self.buckets = order.iter().map(|&i| slots[i].take().expect("bucket")).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr() {
        assert_eq!(cidr_range("10.0.0.0/25").unwrap(), (Some("10.0.0.0".into()), Some("10.0.0.128".into())));
        assert_eq!(cidr_range("0.0.0.0/0").unwrap().0, Some("0.0.0.0".into()));
        assert_eq!(cidr_range("::/0").unwrap(), (None, None));
        assert_eq!(ip_to_string(&netmask(8, false)), "255.0.0.0");
        assert_eq!(ip_to_string(&netmask(20, false)), "255.255.240.0");
    }

    #[test]
    fn overlap_matching() {
        let r = |f: f64, t: f64| NumRange { key: None, from: f, to: t, orig_from: f, orig_to: t };
        let ranges = vec![r(0.0, 10.0), r(5.0, 6.0), r(20.0, f64::INFINITY)];
        let mut max_to = Vec::new();
        for (i, x) in ranges.iter().enumerate() {
            max_to.push(if i == 0 { x.to } else { x.to.max(max_to[i - 1]) });
        }
        let mut out = Vec::new();
        let low = match_value(&ranges, &max_to, 5.5, 0, &mut out);
        assert_eq!(out, vec![0, 1]);
        out.clear();
        match_value(&ranges, &max_to, 25.0, low, &mut out);
        assert_eq!(out, vec![2]);
    }
}
