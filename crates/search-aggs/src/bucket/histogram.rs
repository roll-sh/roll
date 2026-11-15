//! `histogram` and `date_histogram`.
//!
//! Shard side, each value maps to its bucket key (`floor((v - offset) / interval)` or a date
//! [`Rounding`]); keys of one doc are deduplicated (values are sorted), `hard_bounds` drop keys
//! outside them, and `(owning bucket, key)` pairs get dense ordinals. Shard results list buckets
//! by ascending key; with `min_doc_count: 0` they carry what the reduce needs to fill gaps
//! (interval, offset, rounding, `extended_bounds`, the sub-aggregations of an empty bucket).
//!
//! The reduce merges the key-ordered lists in one pass, drops buckets under `min_doc_count`,
//! fills empty buckets between the first and last key and out to `extended_bounds` (counting
//! them against `search.max_buckets` before allocating any), then applies the order.

use std::cmp::Ordering;
use std::sync::Arc;

use search_index::SegmentContext;

use crate::bucket::terms::{ReduceSorter, count_inner_buckets, validate_order};
use crate::collect::{Aggregator, Batch, BindCtx, SizedAncestor, Subs, grow};
use crate::error::{AggError, Result};
use crate::format::DocValueFormat;
use crate::internal::{AggValue, Aggs, Internal, MultiBuckets, Prop, ReduceCtx, RenderCtx, multi_bucket_property};
use crate::json::{Json, java_double};
use crate::memory::MemTracker;
use crate::request::{AggDef, BucketOrder, java_double_compare};
use crate::rounding::{DateUnit, Rounding, RoundingKind, parse_time_value_millis};
use crate::util::OrdMap;
use crate::metrics::simple::{java_max, java_min};
use crate::range_field::{self, RangeType};
use crate::values::{BoundVs, DocCounter, NumReader, VsOpts, VsSpec, VsType, bind};
use crate::xparser::{Ctor, Field, Token, Tokens, XParser};

/// `DoubleBounds`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DoubleBounds {
    pub min: Option<f64>,
    pub max: Option<f64>,
}

impl DoubleBounds {
    /// `DoubleBounds.PARSER`: a `ConstructingObjectParser` whose constructor (called by
    /// reflection, hence the wrapping) checks the bounds.
    fn parse(p: &mut XParser<'_>) -> Result<DoubleBounds> {
        let fields = [Field::new("min", Tokens::DOUBLE_OR_NULL), Field::new("max", Tokens::DOUBLE_OR_NULL)];
        let mut min = None;
        let mut max = None;
        p.constructing("double_bounds", &fields, &[0, 1], &[], |p, ev| {
            let check = |msg: String| -> AggError {
                AggError::illegal_argument("Cannot instantiate an object of org.elasticsearch.search.aggregations.bucket.histogram.DoubleBounds")
                    .caused_by(AggError::new(400, "invocation_target_exception", "").caused_by(AggError::illegal_argument(msg)))
            };
            match ev {
                Ctor::Build => {
                    if let Some(m) = min
                        && !f64::is_finite(m)
                    {
                        return Err(check(format!("min bound must be finite, got: {}", java_double(m))));
                    }
                    if let Some(m) = max
                        && !f64::is_finite(m)
                    {
                        return Err(check(format!("max bound must be finite, got: {}", java_double(m))));
                    }
                    if let (Some(a), Some(b)) = (min, max)
                        && b < a
                    {
                        return Err(check(format!("max bound [{}] must be greater than min bound [{}]", java_double(b), java_double(a))));
                    }
                }
                Ctor::Field(i) => {
                    let v = if p.token() == Token::Null { None } else { Some(p.double_value()?) };
                    if i == 0 {
                        min = v;
                    } else {
                        max = v;
                    }
                }
            }
            Ok(None)
        })?;
        Ok(DoubleBounds { min, max })
    }

    /// `contain(value)` (inclusive on both ends).
    pub fn contain(&self, v: f64) -> bool {
        !(self.max.is_some_and(|m| v > m) || self.min.is_some_and(|m| v < m))
    }

    fn to_string_java(self) -> String {
        format!("{}--{}", self.min.map(java_double).unwrap_or_default(), self.max.map(java_double).unwrap_or_default())
    }
}

/// `LongBounds` as written (numbers or date strings).
#[derive(Clone, Debug, PartialEq)]
pub struct LongBoundsSpec {
    pub min: Option<i64>,
    pub max: Option<i64>,
    pub min_str: Option<String>,
    pub max_str: Option<String>,
}

impl LongBoundsSpec {
    fn parse(p: &mut XParser<'_>) -> Result<LongBoundsSpec> {
        let fields = [
            Field::new("min", Tokens::of(&[Token::Number, Token::String, Token::Null])),
            Field::new("max", Tokens::of(&[Token::Number, Token::String, Token::Null])),
        ];
        let mut b = LongBoundsSpec { min: None, max: None, min_str: None, max_str: None };
        p.object("bounds", &fields, |p, i| {
            let (n, s) = match p.token() {
                Token::Number => (Some(p.long_value()?), None),
                Token::String => (None, Some(p.text()?)),
                _ => (None, None),
            };
            if i == 0 {
                b.min = n;
                b.min_str = s;
            } else {
                b.max = n;
                b.max_str = s;
            }
            Ok(())
        })?;
        Ok(b)
    }

    /// `parseAndValidate` then `round(rounding.withoutOffset())`.
    fn resolve(&self, agg: &str, what: &str, format: &DocValueFormat, now: i64, rounding: &Rounding) -> Result<(Option<i64>, Option<i64>)> {
        let mut min = self.min;
        let mut max = self.max;
        if let Some(s) = &self.min_str {
            min = Some(format.parse_long(s, false, now)?);
        }
        if let Some(s) = &self.max_str {
            max = Some(format.parse_long(s, false, now)?);
        }
        if let (Some(a), Some(b)) = (min, max)
            && a > b
        {
            return Err(AggError::illegal_argument(format!(
                "[{what}.min][{a}] cannot be greater than [{what}.max][{b}] for histogram aggregation [{agg}]"
            )));
        }
        let r = rounding.without_offset();
        Ok((min.map(|v| r.round_bound(v)).transpose()?, max.map(|v| r.round_bound(v)).transpose()?))
    }

    fn to_string_java(&self) -> String {
        let side = |n: Option<i64>, s: &Option<String>| -> String {
            match (n, s) {
                (Some(n), Some(s)) => format!("{n}({s})"),
                (Some(n), None) => n.to_string(),
                (None, Some(s)) => s.clone(),
                (None, None) => String::new(),
            }
        };
        format!("{}--{}", side(self.min, &self.min_str), side(self.max, &self.max_str))
    }
}

/// `order(List<BucketOrder>)` of histograms: one order as is (tie-broken unless a key order),
/// several as a compound.
fn histogram_order(list: Vec<BucketOrder>) -> BucketOrder {
    if list.len() == 1 {
        let o = list.into_iter().next().expect("one");
        if o.is_key_order() || matches!(o, BucketOrder::Compound(_)) { o } else { BucketOrder::compound(vec![o]) }
    } else {
        BucketOrder::compound(list)
    }
}

/// A parsed `histogram`.
#[derive(Clone, Debug)]
pub struct HistogramSpec {
    pub vs: VsSpec,
    pub interval: f64,
    pub offset: f64,
    pub keyed: bool,
    pub min_doc_count: u64,
    pub extended_bounds: Option<DoubleBounds>,
    pub hard_bounds: Option<DoubleBounds>,
    pub order: BucketOrder,
}

fn parse_min_doc_count(p: &XParser<'_>, name: &str) -> Result<u64> {
    let v = p.long_value()?;
    if v < 0 {
        return Err(AggError::illegal_argument(format!("[minDocCount] must be greater than or equal to 0. Found [{v}] in [{name}]")));
    }
    Ok(v as u64)
}

pub fn parse_histogram(p: &mut XParser<'_>, name: &str) -> Result<HistogramSpec> {
    let opts = VsOpts::new(true, true, false);
    let mut fields = VsSpec::fields(opts);
    fields.extend_from_slice(&[
        Field::new("interval", Tokens::FLOAT),
        Field::new("offset", Tokens::FLOAT),
        Field::new("keyed", Tokens::BOOLEAN),
        Field::new("min_doc_count", Tokens::LONG),
        Field::new("extended_bounds", Tokens::OBJECT),
        Field::new("hard_bounds", Tokens::OBJECT),
        Field::new("order", Tokens::OBJECT_ARRAY),
    ]);
    let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
    let mut s = HistogramSpec {
        vs: VsSpec::default(),
        interval: 0.0,
        offset: 0.0,
        keyed: false,
        min_doc_count: 0,
        extended_bounds: None,
        hard_bounds: None,
        order: BucketOrder::KEY_ASC,
    };
    p.object("histogram", &fields, |p, i| {
        let f = names[i];
        if s.vs.parse_field(p, f)? {
            return Ok(());
        }
        match f {
            "interval" => {
                let v = p.double_value()?;
                if v <= 0.0 {
                    return Err(AggError::illegal_argument(format!("[interval] must be >0 for histogram aggregation [{name}]")));
                }
                s.interval = v;
            }
            "offset" => s.offset = p.double_value()?,
            "keyed" => s.keyed = p.boolean_value()?,
            "min_doc_count" => s.min_doc_count = parse_min_doc_count(p, name)?,
            "extended_bounds" => s.extended_bounds = Some(DoubleBounds::parse(p)?),
            "hard_bounds" => s.hard_bounds = Some(DoubleBounds::parse(p)?),
            _ => s.order = histogram_order(BucketOrder::parse_list(p)?),
        }
        Ok(())
    })?;
    s.vs.check_required(opts)?;
    Ok(s)
}

/// The interval of a `date_histogram`.
#[derive(Clone, Debug, PartialEq)]
pub enum DateInterval {
    None,
    Calendar(String),
    Fixed(String),
    /// OpenSearch's deprecated `interval`: a calendar unit (`1d`, `month`) rounds as a
    /// calendar interval, anything else as a fixed one (`DateIntervalWrapper` legacy mode).
    Legacy(String),
}

/// A parsed `date_histogram`.
#[derive(Clone, Debug)]
pub struct DateHistogramSpec {
    pub vs: VsSpec,
    pub interval: DateInterval,
    pub offset: i64,
    pub keyed: bool,
    pub min_doc_count: u64,
    pub extended_bounds: Option<LongBoundsSpec>,
    pub hard_bounds: Option<LongBoundsSpec>,
    pub order: BucketOrder,
}

/// `DateHistogramAggregationBuilder.parseStringOffset`.
pub fn parse_string_offset(s: &str) -> Result<i64> {
    let setting = "DateHistogramAggregationBuilder.parseOffset";
    if let Some(rest) = s.strip_prefix('-') {
        return Ok(-parse_time_value_millis(rest, setting)?);
    }
    parse_time_value_millis(s.strip_prefix('+').unwrap_or(s), setting)
}

/// Parses `calendar_interval` / `fixed_interval` values (shared with composite sources).
pub fn set_interval(cur: &mut DateInterval, calendar: bool, v: &str) -> Result<()> {
    if v.is_empty() {
        return Err(AggError::illegal_argument("[interval] must not be null: [date_histogram]"));
    }
    let (new, name) = if calendar {
        if DateUnit::parse_calendar(v).is_none() {
            return Err(AggError::illegal_argument(format!("The supplied interval [{v}] could not be parsed as a calendar interval.")));
        }
        (DateInterval::Calendar(v.to_string()), "calendar_interval")
    } else {
        (DateInterval::Fixed(v.to_string()), "fixed_interval")
    };
    match cur {
        DateInterval::None => {}
        DateInterval::Legacy(_) => return Err(AggError::illegal_argument(format!("Cannot use [{name}] with [interval] configuration option."))),
        DateInterval::Calendar(_) if calendar => {}
        DateInterval::Fixed(_) if !calendar => {}
        DateInterval::Calendar(_) => {
            return Err(AggError::illegal_argument(format!("Cannot use [{name}] with [calendar_interval] configuration option.")));
        }
        DateInterval::Fixed(_) => {
            return Err(AggError::illegal_argument(format!("Cannot use [{name}] with [fixed_interval] configuration option.")));
        }
    }
    if !calendar {
        parse_time_value_millis(v, "date_histogram.fixedInterval")?;
    }
    *cur = new;
    Ok(())
}

/// OpenSearch's deprecated `interval` (a string, or a number of milliseconds), refused next
/// to `calendar_interval` / `fixed_interval`.
pub fn set_legacy_interval(cur: &mut DateInterval, p: &mut XParser<'_>) -> Result<()> {
    if matches!(cur, DateInterval::Calendar(_) | DateInterval::Fixed(_)) {
        return Err(AggError::illegal_argument("Cannot use [interval] with [fixed_interval] or [calendar_interval] configuration options."));
    }
    let v = if p.token() == Token::Number { format!("{}ms", p.long_value()?) } else { p.text()? };
    if v.is_empty() {
        return Err(AggError::illegal_argument("[interval] must not be null: [date_histogram]"));
    }
    p.warnings.push("[interval] on [date_histogram] is deprecated, use [fixed_interval] or [calendar_interval] in the future.".to_string());
    *cur = DateInterval::Legacy(v);
    Ok(())
}

pub fn parse_date_histogram(p: &mut XParser<'_>, name: &str) -> Result<DateHistogramSpec> {
    let opts = VsOpts::new(true, true, true);
    let mut fields = VsSpec::fields(opts);
    fields.extend_from_slice(&[
        Field::new("calendar_interval", Tokens::STRING),
        Field::new("fixed_interval", Tokens::STRING),
        Field::new("offset", Tokens::LONG),
        Field::new("keyed", Tokens::BOOLEAN),
        Field::new("min_doc_count", Tokens::LONG),
        Field::new("extended_bounds", Tokens::OBJECT),
        Field::new("hard_bounds", Tokens::OBJECT),
        Field::new("order", Tokens::OBJECT_ARRAY),
    ]);
    if crate::dialect::opensearch() {
        fields.push(Field::new("interval", Tokens::LONG));
    }
    let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
    let mut s = DateHistogramSpec {
        vs: VsSpec::default(),
        interval: DateInterval::None,
        offset: 0,
        keyed: false,
        min_doc_count: 0,
        extended_bounds: None,
        hard_bounds: None,
        order: BucketOrder::KEY_ASC,
    };
    p.object("date_histogram", &fields, |p, i| {
        let f = names[i];
        if s.vs.parse_field(p, f)? {
            return Ok(());
        }
        match f {
            "calendar_interval" => set_interval(&mut s.interval, true, &p.text()?)?,
            "fixed_interval" => set_interval(&mut s.interval, false, &p.text()?)?,
            "interval" => set_legacy_interval(&mut s.interval, p)?,
            "offset" => {
                s.offset = if p.token() == Token::Number { p.long_value()? } else { parse_string_offset(&p.text()?)? };
            }
            "keyed" => s.keyed = p.boolean_value()?,
            "min_doc_count" => s.min_doc_count = parse_min_doc_count(p, name)?,
            "extended_bounds" => s.extended_bounds = Some(LongBoundsSpec::parse(p)?),
            "hard_bounds" => s.hard_bounds = Some(LongBoundsSpec::parse(p)?),
            _ => s.order = histogram_order(BucketOrder::parse_list(p)?),
        }
        Ok(())
    })?;
    s.vs.check_required(opts)?;
    Ok(s)
}

/// `DateIntervalWrapper.createRounding(tz, offset)`.
pub fn create_rounding(interval: &DateInterval, tz: Option<&crate::format::TimeZone>, offset: i64) -> Result<Rounding> {
    let zone = tz.cloned().unwrap_or_default();
    match interval {
        DateInterval::None => Err(AggError::illegal_argument("Invalid interval specified, must be non-null and non-empty")),
        DateInterval::Calendar(c) => {
            let u = DateUnit::parse_calendar(c).ok_or_else(|| AggError::illegal_argument(format!("Unable to parse interval [{c}]")))?;
            Rounding::new(RoundingKind::Unit(u), zone, offset)
        }
        DateInterval::Fixed(f) => {
            let ms = parse_time_value_millis(f, "DateIntervalWrapper.interval")?;
            Rounding::new(RoundingKind::Interval(ms), zone, offset)
        }
        DateInterval::Legacy(v) => match DateUnit::parse_calendar(v) {
            Some(u) => Rounding::new(RoundingKind::Unit(u), zone, offset),
            None => {
                let ms = parse_time_value_millis(v, "DateIntervalWrapper.interval").map_err(|_| AggError::illegal_argument(format!("Unable to parse interval [{v}]")))?;
                Rounding::new(RoundingKind::Interval(ms), zone, offset)
            }
        },
    }
}

/// Settings shared by numeric histogram results.
#[derive(Debug)]
pub struct HistoConfig {
    pub order: BucketOrder,
    pub min_doc_count: u64,
    pub keyed: bool,
    pub format: DocValueFormat,
    /// A date histogram over a downsampled index whose keys moved to the downsampling
    /// intervals (`downsampled_results_offset`).
    pub downsampled_offset: bool,
}

/// How a histogram aggregator maps values to keys.
enum KeyFn {
    Numeric { interval: f64, offset: f64, hard: Option<DoubleBounds> },
    Date { rounding: Rounding, hard: (Option<i64>, Option<i64>) },
}

pub fn create_histogram(def: &AggDef, spec: &HistogramSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let vs = bind(&spec.vs, ctx.lookup, VsType::Numeric, false)?;
    let range = vs.range_type();
    let pre_aggregated = matches!(vs.vs_type, VsType::Histogram | VsType::ExponentialHistogram) && !vs.unmapped;
    if !vs.vs_type.is_numeric() && !vs.unmapped && range.is_none() && !pre_aggregated {
        return Err(vs.unsupported("histogram"));
    }
    if spec.interval <= 0.0 {
        return Err(AggError::illegal_argument(format!("interval must be positive, got: {}", java_double(spec.interval))));
    }
    if let (Some(h), Some(e)) = (spec.hard_bounds, spec.extended_bounds) {
        let bad = h.max.is_some() && e.max.is_some() && h.max < e.max || h.min.is_some() && e.min.is_some() && h.min > e.min;
        if bad {
            return Err(AggError::illegal_argument(format!(
                "Extended bounds have to be inside hard bounds, hard bounds: [{}], extended bounds: [{}--{}]",
                h.to_string_java(),
                e.min.map(java_double).unwrap_or_else(|| "null".into()),
                e.max.map(java_double).unwrap_or_else(|| "null".into())
            )));
        }
    }
    let subs = Subs::bind(def, ctx)?;
    if pre_aggregated && !subs.is_empty() {
        return Err(AggError::illegal_argument("Histogram aggregation on histogram fields does not support sub-aggregations"));
    }
    validate_order(&spec.order, &subs)?;
    // `RangeHistogramAggregator`'s constructor.
    if let Some(t) = range
        && !t.is_numeric()
    {
        return Err(AggError::illegal_argument(format!("Expected numeric range type but found non-numeric range [{}]", t.name())));
    }
    let cfg = Arc::new(HistoConfig { order: spec.order.clone(), min_doc_count: spec.min_doc_count, keyed: spec.keyed, format: vs.format.clone(), downsampled_offset: false });
    let empty = if spec.min_doc_count == 0 {
        Some(Arc::new(EmptyInfo {
            interval: spec.interval,
            offset: spec.offset,
            min_bound: spec.extended_bounds.and_then(|b| b.min).unwrap_or(f64::INFINITY),
            max_bound: spec.extended_bounds.and_then(|b| b.max).unwrap_or(f64::NEG_INFINITY),
        }))
    } else {
        None
    };
    Ok(Box::new(HistoAgg {
        key_fn: KeyFn::Numeric { interval: spec.interval, offset: spec.offset, hard: spec.hard_bounds },
        range: range.map(RangeSrc::new),
        pre_aggregated,
        histograms: None,
        vs,
        reader: None,
        ords: OrdMap::new(),
        counts: Vec::new(),
        subs,
        doc_counter: DocCounter::none(),
        doc_count_field: ctx.doc_count_field,
        tracker: ctx.tracker(def),
        batch: Default::default(),
        prepared: None,
        slot_ords: Vec::new(),
        num_min: None,
        name: def.name.clone(),
        out: Out::Numeric { cfg, empty },
        starts: None,
        os: None,
        counted: None,
        nanos: false,
        seg_done: false,
    }))
}

/// A downsampled index holds values aggregated over UTC intervals
/// (`DateHistogramAggregationBuilder.innerBuild`): calendar intervals are refused, and a time
/// zone whose offset isn't a multiple of the downsampling interval moves the buckets by the
/// remainder (reported as `downsampled_results_offset`). Returns the offset to round with and
/// whether it moved.
fn downsampled_offset(spec: &DateHistogramSpec, vs: &BoundVs, downsampling: Option<&str>) -> Result<(i64, bool)> {
    let Some(downsampling) = downsampling else { return Ok((spec.offset, false)) };
    if let DateInterval::Calendar(_) = spec.interval {
        return Err(AggError::illegal_argument(format!("{} is not supported for aggregation [date_histogram] with interval type [calendar_interval]", vs.description())));
    }
    let Some(tz) = vs.time_zone.as_ref().filter(|tz| tz.id() != "UTC") else { return Ok((spec.offset, false)) };
    let (DateInterval::Fixed(i) | DateInterval::Legacy(i)) = &spec.interval else { return Ok((spec.offset, false)) };
    let parse = |s: &str| crate::rounding::parse_time_value_millis(s, "fixed_interval").ok().filter(|ms| *ms > 0);
    let (Some(resolution), Some(aggregation)) = (parse(downsampling), parse(i)) else { return Ok((spec.offset, false)) };
    if spec.vs.field.as_deref() != Some("@timestamp") {
        return Ok((spec.offset, false));
    }
    let shift = tz.offset_at(aggregation) as i64 * 1000 % resolution;
    Ok(if shift == 0 { (spec.offset, false) } else { (spec.offset + shift, true) })
}

pub fn create_date_histogram(def: &AggDef, spec: &DateHistogramSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let vs = bind(&spec.vs, ctx.lookup, VsType::Date, true)?;
    let range = vs.range_type();
    if (!vs.vs_type.is_numeric() && !vs.unmapped && range.is_none()) || vs.is_counter() {
        return Err(vs.unsupported("date_histogram"));
    }
    let (offset, downsampled_offset) = downsampled_offset(spec, &vs, ctx.lookup.downsampling_interval().as_deref())?;
    let rounding = create_rounding(&spec.interval, vs.time_zone.as_ref(), offset)?;
    let now = ctx.lookup.now_millis();
    let ext = match &spec.extended_bounds {
        Some(b) => Some(b.resolve(&def.name, "extended_bounds", &vs.format, now, &rounding)?),
        None => None,
    };
    let hard = match &spec.hard_bounds {
        Some(b) => Some(b.resolve(&def.name, "hard_bounds", &vs.format, now, &rounding)?),
        None => None,
    };
    if let (Some(e), Some(h)) = (ext, hard) {
        let bad = e.1.is_some() && h.1.is_some() && e.1 > h.1 || e.0.is_some() && h.0.is_some() && e.0 < h.0;
        if bad {
            return Err(AggError::illegal_argument(format!(
                "Extended bounds have to be inside hard bounds, hard bounds: [{}], extended bounds: [{}]",
                spec.hard_bounds.as_ref().map(|b| b.to_string_java()).unwrap_or_default(),
                spec.extended_bounds.as_ref().map(|b| b.to_string_java()).unwrap_or_default()
            )));
        }
    }
    // `SizedBucketAggregator`: `rate` sub-aggregations divide by this histogram's bucket sizes.
    let sized = SizedAncestor::new(ctx.parents.len(), &def.name, Some(rounding.clone()));
    let starts = def.subs.iter().any(|d| d.type_name == "rate").then(|| sized.starts.clone());
    let saved = ctx.sized.replace(sized);
    let subs = Subs::bind(def, ctx);
    ctx.sized = saved;
    let subs = subs?;
    validate_order(&spec.order, &subs)?;
    // `DateRangeHistogramAggregator`'s constructor.
    if let Some(t) = range
        && t != RangeType::Date
    {
        return Err(AggError::illegal_argument(format!("Expected date range type but found range type [{}]", t.name())));
    }
    let cfg = Arc::new(HistoConfig { order: spec.order.clone(), min_doc_count: spec.min_doc_count, keyed: spec.keyed, format: vs.format.clone(), downsampled_offset });
    let empty = if spec.min_doc_count == 0 { Some(Arc::new(DateEmptyInfo { rounding: rounding.without_offset(), bounds: ext })) } else { None };
    let os = if range.is_none() { OsDateDebug::new(ctx, &vs, &rounding, hard.unwrap_or((None, None)), !subs.is_empty()) } else { None };
    let nanos = vs.field.as_ref().is_some_and(|f| f.type_name == "date_nanos");
    // Buckets counted from the points tree where a segment allows it (crate::filter_rewrite).
    let counted = match range {
        None => {
            let buckets = crate::filter_rewrite::DateBuckets { rounding: rounding.clone(), hard: hard.unwrap_or((None, None)), after: None, nanos, auto: Vec::new(), target: 1 };
            crate::filter_rewrite::FilterRewrite::counting(ctx, &vs, buckets, !subs.is_empty()).map(Box::new)
        }
        Some(_) => None,
    };
    Ok(Box::new(HistoAgg {
        key_fn: KeyFn::Date { rounding: rounding.clone(), hard: hard.unwrap_or((None, None)) },
        range: range.map(RangeSrc::new),
        pre_aggregated: false,
        histograms: None,
        vs,
        reader: None,
        ords: OrdMap::new(),
        counts: Vec::new(),
        subs,
        doc_counter: DocCounter::none(),
        doc_count_field: ctx.doc_count_field,
        tracker: ctx.tracker(def),
        batch: Default::default(),
        prepared: None,
        slot_ords: Vec::new(),
        num_min: None,
        name: def.name.clone(),
        out: Out::Date { cfg, empty, offset: rounding.offset },
        starts,
        os,
        counted,
        nanos,
        seg_done: false,
    }))
}

enum Out {
    Numeric { cfg: Arc<HistoConfig>, empty: Option<Arc<EmptyInfo>> },
    Date { cfg: Arc<HistoConfig>, empty: Option<Arc<DateEmptyInfo>>, offset: i64 },
}

/// A range field's doc values and decode buffers (`RangeHistogramAggregator`,
/// `DateRangeHistogramAggregator`): every bucket a range overlaps gets the doc once.
struct RangeSrc {
    ty: RangeType,
    dv: Option<search_codec::BinaryDocValues>,
    doubles: Vec<(f64, f64)>,
    longs: Vec<(i64, i64)>,
}

impl RangeSrc {
    fn new(ty: RangeType) -> RangeSrc {
        RangeSrc { ty, dv: None, doubles: Vec::new(), longs: Vec::new() }
    }
}

/// Buckets one range value may span before the request is refused (Elasticsearch keeps adding
/// buckets until its circuit breaker trips; an unbounded range would never finish).
const MAX_RANGE_KEYS: f64 = (1u64 << 26) as f64;

fn range_too_wide(name: &str, keys: f64) -> AggError {
    let wanted = if keys.is_finite() { (keys * 16.0) as u64 } else { u64::MAX };
    crate::memory::breaking_error("request", &format!("<agg [{name}]>"), wanted, (MAX_RANGE_KEYS as u64) * 16)
}

/// A lower bound on a date bucket's length: calendar units lose at most a DST hour, and
/// local-time units of a day or less are taken at half their length.
fn min_bucket_millis(r: &Rounding) -> f64 {
    const DAY: f64 = 86_400_000.0;
    match r.kind {
        RoundingKind::Interval(i) => i.max(1) as f64,
        RoundingKind::Unit(DateUnit::Year) => 365.0 * DAY - 3_600_000.0,
        RoundingKind::Unit(DateUnit::Quarter) => 89.0 * DAY - 3_600_000.0,
        RoundingKind::Unit(DateUnit::Month) => 28.0 * DAY - 3_600_000.0,
        RoundingKind::Unit(u) => u.ratio() as f64 / 2.0,
    }
}

/// `x.floor()` without a libm call (baseline x86-64 has no rounding instruction): exact below
/// 2^52 in magnitude, where truncating to an integer is exact; zeros (their sign), NaN and
/// larger values go to `f64::floor`.
#[inline]
fn floor(x: f64) -> f64 {
    if x != 0.0 && x.abs() < 4_503_599_627_370_496.0 {
        let t = x as i64 as f64;
        if t > x { t - 1.0 } else { t }
    } else {
        x.floor()
    }
}

/// Most keys a numeric histogram caches bucket ordinals for, per segment.
const NUM_SLOTS: f64 = 4096.0;

struct HistoAgg {
    /// Batch-decoded doc values (fields without script or `missing`).
    batch: crate::values::NumBatch,
    key_fn: KeyFn,
    /// Set for range fields (collected from binary doc values instead of `reader`).
    range: Option<RangeSrc>,
    /// A pre-aggregated `histogram` field (`HistoBackedHistogramAggregator`).
    pre_aggregated: bool,
    histograms: Option<crate::histogram_field::PreAggReader>,
    vs: BoundVs,
    reader: Option<NumReader>,
    /// Keys: double bits of the rounded key index (numeric) or the rounded millis (date).
    ords: OrdMap,
    counts: Vec<u64>,
    subs: Subs,
    doc_counter: DocCounter,
    doc_count_field: Option<u32>,
    tracker: MemTracker,
    name: String,
    out: Out,
    /// Bucket starts published to direct `rate` sub-aggregations.
    starts: Option<crate::collect::BucketStarts>,
    /// date_histogram: the rounding prepared for the segment's value range, and per prepared
    /// bucket the `(owning << 32 | ord)` it was last added as (`u64::MAX`: not yet), so a
    /// value costs a binary search and no hash lookup.
    prepared: Option<crate::rounding::PreparedRounding>,
    slot_ords: Vec<u64>,
    /// histogram: the smallest key of the segment's value range; key `k` then caches its
    /// `(owning << 32 | ord)` in `slot_ords[k - min]` (a range of at most `NUM_SLOTS` keys).
    num_min: Option<f64>,
    /// Profiled opensearch-dialect date histograms: OpenSearch's leaf collector counts and
    /// filter rewrite replay.
    os: Option<Box<OsDateDebug>>,
    /// date_histogram counted from the points tree on the segments that allow it.
    counted: Option<Box<crate::filter_rewrite::FilterRewrite>>,
    /// A `date_nanos` field (the counted ranges are in nanoseconds, keys in milliseconds).
    nanos: bool,
    /// The current segment was counted from its points: no document of it is collected.
    seg_done: bool,
}

/// OpenSearch's `DateHistogramAggregator.collectDebugInfo`: which leaf collector each segment
/// would have used (single-valued, multi-valued, skip list, no-op) unless the filter rewrite
/// optimization counted it from the points tree.
struct OsDateDebug {
    rewrite: Option<crate::filter_rewrite::FilterRewrite>,
    single: u64,
    multi: u64,
    no_op: u64,
}

impl OsDateDebug {
    fn new(ctx: &BindCtx<'_>, vs: &BoundVs, rounding: &crate::rounding::Rounding, hard: (Option<i64>, Option<i64>), has_sub: bool) -> Option<Box<OsDateDebug>> {
        if !crate::dialect::opensearch() || ctx.profiler.is_none() {
            return None;
        }
        let nanos = vs.field.as_ref().is_some_and(|f| f.type_name == "date_nanos");
        let buckets = crate::filter_rewrite::DateBuckets { rounding: rounding.clone(), hard, after: None, nanos, auto: Vec::new(), target: 1 };
        let rewrite = crate::filter_rewrite::FilterRewrite::date(ctx, vs, buckets, has_sub, usize::MAX);
        Some(Box::new(OsDateDebug { rewrite, single: 0, multi: 0, no_op: 0 }))
    }

    fn segment(&mut self, vs: &BoundVs, seg: &SegmentContext) {
        if self.rewrite.as_mut().is_some_and(|r| r.try_segment(seg)) {
            return;
        }
        let Some(f) = vs.field.as_ref().filter(|_| !vs.unmapped || vs.missing.is_some()) else {
            self.no_op += 1;
            return;
        };
        if vs.script.is_none() && seg.reader.sorted_numeric(f.number).is_some_and(|d| !d.is_single_valued()) {
            self.multi += 1;
        } else {
            self.single += 1;
        }
    }

    fn debug_into(&self, o: &mut Json) {
        // The filter rewrite counts go in first, as OpenSearch adds them.
        if let Some(r) = &self.rewrite {
            r.debug_into(o);
        }
        o.push("single_valued_collectors_used", Json::from(self.single));
        o.push("multi_valued_collectors_used", Json::from(self.multi));
        o.push("skip_list_collectors_used", Json::from(0u64));
        o.push("no_op_collectors_used", Json::from(self.no_op));
    }
}

impl HistoAgg {
    /// Like [`add`](Self::add) but counting `n` (a histogram centroid's count).
    fn add_n(&mut self, owning: u32, key: u64, doc: u32, score: f32, n: u64) -> Result<()> {
        let (ord, new) = self.ords.add(owning, key);
        if new {
            grow(&mut self.counts, ord, 0);
            if self.counts.len() * 8 > self.tracker.reserved() as usize {
                self.tracker.ensure(self.counts.len() * 8 + self.ords.ram_bytes())?;
            }
        }
        self.counts[ord as usize] += n;
        if !self.subs.is_empty() {
            self.subs.push(doc, ord, score)?;
        }
        Ok(())
    }

    /// Every centroid goes to its bucket with its count; a doc reaches a bucket's
    /// sub-aggregations once per centroid in it.
    fn collect_histograms(&mut self, b: Batch<'_>, hist: &mut crate::histogram_field::PreAggReader) -> Result<()> {
        let KeyFn::Numeric { interval, offset, hard } = self.key_fn else { return Ok(()) };
        for i in 0..b.len() {
            let doc = b.docs[i];
            for &(v, c) in hist.centroids(doc)? {
                let key = ((v - offset) / interval).floor();
                if hard.is_none_or(|h| h.contain(key * interval)) {
                    self.add_n(b.ords[i], key.to_bits(), doc, b.score(i), c)?;
                }
            }
        }
        Ok(())
    }

    /// [`add`](Self::add) of a numeric key through the segment's slot cache.
    #[inline]
    fn add_numeric(&mut self, owning: u32, key: f64, doc: u32, score: f32) -> Result<()> {
        // -0.0 is a bucket of its own (keys are double bits, as in Elasticsearch).
        if let Some(min) = self.num_min
            && !(key == 0.0 && key.is_sign_negative())
        {
            let at = key - min;
            if at >= 0.0 && (at as usize) < self.slot_ords.len() {
                let at = at as usize;
                let c = self.slot_ords[at];
                if c != u64::MAX && c >> 32 == u64::from(owning) {
                    return self.add_existing(c as u32, doc, score);
                }
                let ord = self.add_ord(owning, key.to_bits(), doc, score)?;
                self.slot_ords[at] = u64::from(owning) << 32 | u64::from(ord);
                return Ok(());
            }
        }
        self.add(owning, key.to_bits(), doc, score)
    }

    #[inline]
    fn add(&mut self, owning: u32, key: u64, doc: u32, score: f32) -> Result<()> {
        self.add_ord(owning, key, doc, score).map(|_| ())
    }

    /// [`add`](Self::add), returning the bucket's ordinal.
    #[inline]
    fn add_ord(&mut self, owning: u32, key: u64, doc: u32, score: f32) -> Result<u32> {
        let (ord, new) = self.ords.add(owning, key);
        if new {
            grow(&mut self.counts, ord, 0);
            if self.counts.len() * 8 > self.tracker.reserved() as usize {
                self.tracker.ensure(self.counts.len() * 8 + self.ords.ram_bytes())?;
            }
        }
        self.add_existing(ord, doc, score)?;
        Ok(ord)
    }

    /// Counts `doc` into the existing bucket `ord`.
    #[inline]
    fn add_existing(&mut self, ord: u32, doc: u32, score: f32) -> Result<()> {
        self.counts[ord as usize] += self.doc_counter.count(doc);
        if !self.subs.is_empty() {
            self.subs.push(doc, ord, score)?;
        }
        Ok(())
    }
}

impl HistoAgg {
    /// [`collect`](Aggregator::collect) over batch-decoded doc values.
    fn collect_batch(&mut self, b: Batch<'_>, batch: &crate::values::NumBatch, decode: crate::values::Decode) -> Result<()> {
        match &self.key_fn {
            KeyFn::Numeric { interval, offset, hard } => {
                let (interval, offset, hard) = (*interval, *offset, *hard);
                for (p, raw) in batch.docs() {
                    let doc = b.docs[p];
                    let mut prev = f64::NEG_INFINITY;
                    let mut first = true;
                    for &rv in raw {
                        let key = floor((decode.to_f64(rv) - offset) / interval);
                        if !first && key == prev {
                            continue;
                        }
                        first = false;
                        prev = key;
                        if hard.is_none_or(|h| h.contain(key * interval)) {
                            self.add_numeric(b.ords[p], key, doc, b.score(p))?;
                        }
                    }
                }
            }
            KeyFn::Date { rounding, hard } => {
                let rounding = rounding.clone();
                let hard = *hard;
                // The prepared rounding is moved out while the loop borrows `self` mutably.
                let prepared = self.prepared.take();
                let res = self.collect_dates(b, batch, decode, &rounding, hard, prepared.as_ref());
                self.prepared = prepared;
                return res;
            }
        }
        Ok(())
    }

    /// The date arm of [`collect_batch`](Self::collect_batch): single values inside the
    /// segment's prepared range go through the array and the per-bucket ordinal cache.
    fn collect_dates(&mut self, b: Batch<'_>, batch: &crate::values::NumBatch, decode: crate::values::Decode, rounding: &Rounding, hard: (Option<i64>, Option<i64>), prepared: Option<&crate::rounding::PreparedRounding>) -> Result<()> {
        for (p, raw) in batch.docs() {
            let doc = b.docs[p];
            if let (Some(pr), [v]) = (prepared, raw)
                && let Some(i) = pr.index(decode.to_i64(*v))
            {
                let k = pr.point(i);
                if hard.1.is_some_and(|m| k >= m) || hard.0.is_some_and(|m| k < m) {
                    continue;
                }
                let owning = b.ords[p];
                let cached = self.slot_ords[i];
                if cached != u64::MAX && cached >> 32 == u64::from(owning) {
                    self.add_existing(cached as u32, doc, b.score(p))?;
                } else {
                    let ord = self.add_ord(owning, k as u64, doc, b.score(p))?;
                    self.slot_ords[i] = u64::from(owning) << 32 | u64::from(ord);
                }
                continue;
            }
            if raw.len() == 1 {
                let k = rounding.round(decode.to_i64(raw[0]));
                if !(hard.1.is_some_and(|m| k >= m) || hard.0.is_some_and(|m| k < m)) {
                    self.add(b.ords[p], k as u64, doc, b.score(p))?;
                }
                continue;
            }
            let mut prev: Option<i64> = None;
            let mut keys: smallvec::SmallVec<[i64; 4]> = smallvec::SmallVec::new();
            for &rv in raw {
                let k = rounding.round(decode.to_i64(rv));
                if prev == Some(k) {
                    continue;
                }
                prev = Some(k);
                if hard.1.is_some_and(|m| k >= m) || hard.0.is_some_and(|m| k < m) {
                    continue;
                }
                keys.push(k);
            }
            for k in keys {
                self.add(b.ords[p], k as u64, doc, b.score(p))?;
            }
        }
        Ok(())
    }

    fn collect_ranges(&mut self, b: Batch<'_>, mut src: RangeSrc) -> Result<RangeSrc> {
        let Some(dv) = src.dv.take() else { return Ok(src) };
        let res = (|| {
            for i in 0..b.len() {
                let doc = b.docs[i];
                let Some(bytes) = dv.get(doc) else { continue };
                match &self.key_fn {
                    KeyFn::Numeric { interval, offset, hard } => {
                        let (interval, offset, hard) = (*interval, *offset, *hard);
                        range_field::decode_f64(src.ty, bytes, &mut src.doubles)?;
                        let mut prev = f64::NEG_INFINITY;
                        for &(from, to) in &src.doubles {
                            let from = match hard.and_then(|h| h.min) {
                                Some(m) => java_max(from, m),
                                None => from,
                            };
                            let to = match hard.and_then(|h| h.max) {
                                Some(m) => java_min(to, m),
                                None => to,
                            };
                            let start = ((from - offset) / interval).floor();
                            let end = ((to - offset) / interval).floor();
                            let mut key = java_max(start, prev);
                            // An infinite bound never finishes (`-inf + 1 == -inf`).
                            let span = end - key;
                            if key <= end && (span.is_nan() || span >= MAX_RANGE_KEYS) {
                                return Err(range_too_wide(self.tracker_name(), span));
                            }
                            while key <= end {
                                if key != prev {
                                    self.add(b.ords[i], key.to_bits(), doc, b.score(i))?;
                                }
                                key += 1.0;
                            }
                            if end > prev {
                                prev = end;
                            }
                        }
                    }
                    KeyFn::Date { rounding, hard } => {
                        let rounding = rounding.clone();
                        let hard = *hard;
                        range_field::decode_i64(bytes, &mut src.longs)?;
                        let mut prev = i64::MIN;
                        for &(from, to) in &src.longs {
                            let from = hard.0.map_or(from, |m| from.max(m));
                            let to = hard.1.map_or(to, |m| to.min(m));
                            let start = rounding.round(from);
                            let end = rounding.round(to);
                            let mut key = start.max(prev);
                            let estimate = (end as f64 - key as f64) / min_bucket_millis(&rounding);
                            if key <= end && estimate >= MAX_RANGE_KEYS {
                                return Err(range_too_wide(self.tracker_name(), estimate));
                            }
                            while key <= end {
                                if key != prev {
                                    self.add(b.ords[i], key as u64, doc, b.score(i))?;
                                }
                                let next = rounding.next_rounding_value(key);
                                if next <= key {
                                    break;
                                }
                                key = next;
                            }
                            if end > prev {
                                prev = end;
                            }
                        }
                    }
                }
            }
            Ok(())
        })();
        src.dv = Some(dv);
        res.map(|()| src)
    }

    fn tracker_name(&self) -> &str {
        &self.name
    }
}

impl Aggregator for HistoAgg {
    fn profile_type(&self) -> &'static str {
        match (&self.out, self.range.is_some()) {
            (Out::Numeric { .. }, false) => "NumericHistogramAggregator",
            (Out::Numeric { .. }, true) => "RangeHistogramAggregator",
            (Out::Date { .. }, _) => "DateHistogramAggregator",
        }
    }
    fn profile_debug(&self) -> Json {
        let mut o = Json::obj();
        o.push("total_buckets", Json::from(self.ords.len() as u64));
        if let Some(os) = &self.os {
            os.debug_into(&mut o);
        }
        o
    }
    fn segment_done(&self) -> bool {
        self.seg_done
    }

    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        if let Some(os) = &mut self.os {
            os.segment(&self.vs, seg);
        }
        self.seg_done = false;
        if let (Some(fr), KeyFn::Date { rounding, .. }) = (&self.counted, &self.key_fn)
            && let Some(counts) = fr.count_segment(seg)
        {
            let rounding = rounding.clone();
            for ((lo, _), n) in counts {
                let ms = if self.nanos { lo.div_euclid(1_000_000) } else { lo };
                let (ord, new) = self.ords.add(0, rounding.round(ms) as u64);
                if new {
                    grow(&mut self.counts, ord, 0);
                    if self.counts.len() * 8 > self.tracker.reserved() as usize {
                        self.tracker.ensure(self.counts.len() * 8 + self.ords.ram_bytes())?;
                    }
                }
                self.counts[ord as usize] += n;
            }
            self.seg_done = true;
            self.reader = None;
            return self.subs.set_segment(seg);
        }
        self.doc_counter = DocCounter::open(seg, self.doc_count_field);
        if let Some(src) = &mut self.range {
            src.dv = self.vs.field.as_ref().and_then(|f| seg.reader.binary(f.number).cloned());
            self.reader = None;
        } else if self.pre_aggregated {
            self.histograms = Some(crate::histogram_field::PreAggReader::open(seg, &self.vs));
            self.reader = None;
        } else {
            self.reader = if self.vs.unmapped && self.vs.missing.is_none() { None } else { Some(self.vs.numeric_reader(seg)?) };
        }
        self.prepared = None;
        self.num_min = None;
        if let (KeyFn::Numeric { interval, offset, .. }, Some(r)) = (&self.key_fn, &self.reader)
            && let Some((dv, decode)) = r.raw()
            && let Some((lo, hi)) = dv.raw_bounds()
        {
            // Any key in range has its slot: the cache is keyed by the key itself, so the
            // bounds only size it (they needn't decode monotonically).
            let (a, b) = (((decode.to_f64(lo) - offset) / interval).floor(), ((decode.to_f64(hi) - offset) / interval).floor());
            let (min, max) = (a.min(b), a.max(b));
            if min.is_finite() && max.is_finite() && max - min < NUM_SLOTS {
                self.num_min = Some(min);
                self.slot_ords.clear();
                self.slot_ords.resize((max - min) as usize + 1, u64::MAX);
            }
        }
        if let (KeyFn::Date { rounding, .. }, Some(r)) = (&self.key_fn, &self.reader)
            && let Some((dv, decode @ (crate::values::Decode::Long | crate::values::Decode::Nanos))) = r.raw()
            && let Some((lo, hi)) = dv.raw_bounds()
        {
            self.prepared = rounding.prepare(decode.to_i64(lo), decode.to_i64(hi));
            self.slot_ords.clear();
            self.slot_ords.resize(self.prepared.as_ref().map_or(0, |p| p.len()), u64::MAX);
        }
        self.subs.set_segment(seg)
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        if let Some(src) = self.range.take() {
            let src = self.collect_ranges(b, src);
            return src.map(|s| self.range = Some(s));
        }
        if let Some(mut hist) = self.histograms.take() {
            let r = self.collect_histograms(b, &mut hist);
            self.histograms = Some(hist);
            return r;
        }
        let Some(mut r) = self.reader.take() else { return Ok(()) };
        if let Some((dv, decode)) = r.raw() {
            let mut batch = std::mem::take(&mut self.batch);
            dv.load_batch(b.docs, &mut batch);
            let res = self.collect_batch(b, &batch, decode);
            self.batch = batch;
            self.reader = Some(r);
            return res;
        }
        let res = (|| {
            match &self.key_fn {
                KeyFn::Numeric { interval, offset, hard } => {
                    let (interval, offset, hard) = (*interval, *offset, *hard);
                    for i in 0..b.len() {
                        let doc = b.docs[i];
                        let vals = r.doubles(doc, b.score(i))?;
                        let mut prev = f64::NEG_INFINITY;
                        let mut first = true;
                        let n = vals.len();
                        for j in 0..n {
                            let key = ((r_val(vals, j) - offset) / interval).floor();
                            if !first && key == prev {
                                continue;
                            }
                            first = false;
                            prev = key;
                            if hard.is_none_or(|h| h.contain(key * interval)) {
                                let bits = key.to_bits();
                                // `vals` borrows the reader; copy what's left first.
                                self.add(b.ords[i], bits, doc, b.score(i))?;
                            }
                        }
                    }
                }
                KeyFn::Date { rounding, hard } => {
                    let rounding = rounding.clone();
                    let hard = *hard;
                    for i in 0..b.len() {
                        let doc = b.docs[i];
                        let vals = r.longs(doc, b.score(i))?;
                        let mut prev: Option<i64> = None;
                        let mut keys: smallvec::SmallVec<[i64; 4]> = smallvec::SmallVec::new();
                        for &v in vals.iter() {
                            let k = rounding.round(v);
                            if prev == Some(k) {
                                continue;
                            }
                            prev = Some(k);
                            if hard.1.is_some_and(|m| k >= m) || hard.0.is_some_and(|m| k < m) {
                                continue;
                            }
                            keys.push(k);
                        }
                        for k in keys {
                            self.add(b.ords[i], k as u64, doc, b.score(i))?;
                        }
                    }
                }
            }
            Ok(())
        })();
        self.reader = Some(r);
        res
    }

    fn finish_segment(&mut self) -> Result<()> {
        self.subs.finish_segment()
    }

    fn post_collection(&mut self) -> Result<()> {
        self.subs.post_collection()
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        let mut by_owning: hashbrown::HashMap<u32, Vec<u32>> = hashbrown::HashMap::new();
        for (o, k) in self.ords.keys().iter().enumerate() {
            by_owning.entry(k.0).or_default().push(o as u32);
        }
        // `buildAggregationsForVariableBuckets` builds the sub-aggregations in bucket ordinal
        // order (which `rare_terms` below can observe), then sorts each histogram by key. A
        // date_histogram whose shard range has few enough rounding points runs as a `range`
        // (`FromDateRange`), whose buckets are in key order; that is assumed here.
        let mut per: Vec<Vec<u32>> = owning.iter().map(|own| by_owning.get(own).cloned().unwrap_or_default()).collect();
        if let Out::Date { .. } = &self.out {
            for ords in &mut per {
                ords.sort_by_key(|&a| self.ords.key(a).1 as i64);
            }
        }
        let all: Vec<u32> = per.iter().flatten().copied().collect();
        if let Some(starts) = &self.starts {
            SizedAncestor::publish(starts, all.iter().map(|&o| (o, Some(self.ords.key(o).1 as i64))));
        }
        let mut built = self.subs.build(&all)?.into_iter();
        let per: Vec<Vec<(u32, Aggs)>> = per
            .into_iter()
            .map(|ords| {
                let mut v: Vec<(u32, Aggs)> = ords.into_iter().map(|o| (o, built.next().unwrap_or_default())).collect();
                match &self.out {
                    Out::Numeric { .. } => v.sort_by(|a, b| java_double_compare(f64::from_bits(self.ords.key(a.0).1), f64::from_bits(self.ords.key(b.0).1))),
                    Out::Date { .. } => v.sort_by_key(|a| self.ords.key(a.0).1 as i64),
                }
                v
            })
            .collect();
        let mut out = Vec::with_capacity(owning.len());
        for ords in per {
            match &self.out {
                Out::Numeric { cfg, empty } => {
                    let KeyFn::Numeric { interval, offset, .. } = &self.key_fn else { unreachable!() };
                    let buckets = ords
                        .into_iter()
                        .map(|(o, aggs)| HistoBucket { key: f64::from_bits(self.ords.key(o).1) * interval + offset, doc_count: self.counts[o as usize], aggs })
                        .collect();
                    out.push(AggValue::Histogram(Box::new(Histogram {
                        buckets,
                        cfg: cfg.clone(),
                        empty: empty.clone(),
                        empty_aggs: if empty.is_some() { Some(self.subs.build_empty()) } else { None },
                    })));
                }
                Out::Date { cfg, empty, offset, .. } => {
                    let buckets = ords
                        .into_iter()
                        .map(|(o, aggs)| DateBucket { key: self.ords.key(o).1 as i64, doc_count: self.counts[o as usize], aggs })
                        .collect();
                    out.push(AggValue::DateHistogram(Box::new(DateHistogram {
                        buckets,
                        cfg: cfg.clone(),
                        offset: *offset,
                        empty: empty.clone(),
                        empty_aggs: if empty.is_some() { Some(self.subs.build_empty()) } else { None },
                    })));
                }
            }
        }
        Ok(out)
    }

    fn build_empty(&self) -> AggValue {
        match &self.out {
            Out::Numeric { cfg, empty } => AggValue::Histogram(Box::new(Histogram {
                buckets: Vec::new(),
                cfg: cfg.clone(),
                empty: empty.clone(),
                empty_aggs: if empty.is_some() { Some(self.subs.build_empty()) } else { None },
            })),
            Out::Date { cfg, empty, offset, .. } => AggValue::DateHistogram(Box::new(DateHistogram {
                buckets: Vec::new(),
                cfg: cfg.clone(),
                offset: *offset,
                empty: empty.clone(),
                empty_aggs: if empty.is_some() { Some(self.subs.build_empty()) } else { None },
            })),
        }
    }

    fn needs_scores(&self) -> bool {
        self.subs.needs_scores() || self.vs.script.as_ref().is_some_and(|s| s.needs_score())
    }
}

#[inline]
fn r_val(vals: &[f64], j: usize) -> f64 {
    vals[j]
}

/// `InternalHistogram.EmptyBucketInfo` (without the sub-aggregations, kept beside it).
#[derive(Debug)]
pub struct EmptyInfo {
    pub interval: f64,
    pub offset: f64,
    pub min_bound: f64,
    pub max_bound: f64,
}

impl EmptyInfo {
    fn round(&self, key: f64) -> f64 {
        ((key - self.offset) / self.interval).floor() * self.interval + self.offset
    }

    fn next_key(&self, key: f64) -> f64 {
        self.round(key + self.interval + self.interval / 2.0)
    }
}

/// A numeric histogram bucket.
#[derive(Clone, Debug)]
pub struct HistoBucket {
    pub key: f64,
    pub doc_count: u64,
    pub aggs: Aggs,
}

/// `InternalHistogram`.
#[derive(Clone, Debug)]
pub struct Histogram {
    pub buckets: Vec<HistoBucket>,
    pub cfg: Arc<HistoConfig>,
    pub empty: Option<Arc<EmptyInfo>>,
    pub empty_aggs: Option<Aggs>,
}

const REPORT_EMPTY_EVERY: usize = 10_000;

/// Merges key-ordered bucket lists, reducing equal keys (`reduceBuckets` + `maybeAddBucket`).
fn merge_keyed<B, K: Copy>(
    lists: Vec<Vec<B>>,
    key: impl Fn(&B) -> K,
    cmp: impl Fn(K, K) -> Ordering,
    mut reduce: impl FnMut(Vec<B>, &mut ReduceCtx<'_>) -> Result<B>,
    keep: impl Fn(&B) -> bool,
    ctx: &mut ReduceCtx<'_>,
) -> Result<Vec<B>> {
    // InternalHistogram.reduceBuckets: a priority queue over the shards' key-sorted lists.
    let groups = crate::util::lucene_merge_groups(lists, |a, b| cmp(key(a), key(b)) == Ordering::Less);
    let mut out = Vec::new();
    for g in groups {
        let r = reduce(g, ctx)?;
        if keep(&r) {
            ctx.consume_buckets(1)?;
            out.push(r);
        }
    }
    Ok(out)
}

impl Histogram {
    fn add_empty_buckets(&self, list: &mut Vec<HistoBucket>, ctx: &mut ReduceCtx<'_>) -> Result<()> {
        let Some(e) = &self.empty else { return Ok(()) };
        // Count first (no allocation), in chunks so a huge range fails early.
        let mut count = 0usize;
        let mut pending = 0usize;
        let mut on_key = |_k: f64, ctx: &mut ReduceCtx<'_>| -> Result<()> {
            count += 1;
            pending += 1;
            if pending >= REPORT_EMPTY_EVERY {
                ctx.consume_buckets(pending)?;
                pending = 0;
            }
            Ok(())
        };
        iterate_empty_numeric(e, list, |k| on_key(k, ctx))?;
        ctx.consume_buckets(pending)?;
        let _ = count;
        let empty_aggs = match &self.empty_aggs {
            Some(a) => Aggs::reduce_lists(vec![a.clone()], ctx)?,
            None => Aggs::default(),
        };
        let mut keys = Vec::new();
        iterate_empty_numeric(e, list, |k| {
            keys.push(k);
            Ok(())
        })?;
        for k in keys {
            list.push(HistoBucket { key: k, doc_count: 0, aggs: empty_aggs.clone() });
        }
        list.sort_by(|a, b| java_double_compare(a.key, b.key));
        Ok(())
    }

    fn apply_order(buckets: &mut Vec<HistoBucket>, order: &BucketOrder) -> Result<()> {
        if order.is_key_order() && !order.is_key_asc() {
            buckets.reverse();
        } else if !(order.is_key_order() && order.is_key_asc()) {
            let refs: Vec<&Aggs> = buckets.iter().map(|b| &b.aggs).collect();
            let sorter = ReduceSorter::new(order, &refs)?;
            let mut idx: Vec<usize> = (0..buckets.len()).collect();
            idx.sort_by(|&i, &j| sorter.cmp(i, j, |k| buckets[k].doc_count, |a, b| java_double_compare(buckets[a].key, buckets[b].key)));
            let mut slots: Vec<Option<HistoBucket>> = buckets.drain(..).map(Some).collect();
            *buckets = idx.into_iter().map(|i| slots[i].take().expect("bucket")).collect();
        }
        Ok(())
    }
}

fn iterate_empty_numeric(e: &EmptyInfo, list: &[HistoBucket], mut on: impl FnMut(f64) -> Result<()>) -> Result<()> {
    if list.is_empty() {
        let mut key = e.round(e.min_bound);
        while key <= e.max_bound {
            on(key)?;
            key = e.next_key(key);
        }
        return Ok(());
    }
    let first = list[0].key;
    if e.min_bound.is_finite() {
        let mut key = e.round(e.min_bound);
        while key < first {
            on(key)?;
            key = e.next_key(key);
        }
    }
    for w in list.windows(2) {
        let mut key = e.next_key(w[0].key);
        while key < w[1].key {
            on(key)?;
            key = e.next_key(key);
        }
    }
    let last = list[list.len() - 1].key;
    let mut key = e.next_key(last);
    while key <= e.max_bound {
        on(key)?;
        key = e.next_key(key);
    }
    Ok(())
}

impl Internal for Histogram {
    fn type_name(&self) -> &'static str {
        "histogram"
    }
    fn class_name(&self) -> &'static str {
        "InternalHistogram"
    }
    fn must_reduce_single(&self) -> bool {
        true
    }
    fn reduce(list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let first = list[0].clone_shell();
        let min_doc_count = first.cfg.min_doc_count;
        let is_final = ctx.is_final();
        let lists: Vec<Vec<HistoBucket>> = list.into_iter().map(|h| h.buckets).collect();
        let mut buckets = merge_keyed(
            lists,
            |b| b.key,
            java_double_compare,
            |g, ctx| {
                let key = g[0].key;
                let doc_count = g.iter().map(|b| b.doc_count).sum();
                let aggs = Aggs::reduce_hashed(g.into_iter().map(|b| b.aggs).collect(), ctx)?;
                Ok(HistoBucket { key, doc_count, aggs })
            },
            |b| !is_final || b.doc_count >= min_doc_count,
            ctx,
        )?;
        if is_final {
            if min_doc_count == 0 {
                first.add_empty_buckets(&mut buckets, ctx)?;
            }
            Histogram::apply_order(&mut buckets, &first.cfg.order)?;
        }
        Ok(Histogram { buckets, ..first })
    }
    fn render(&self, out: &mut Json, rc: &RenderCtx) {
        let f = &self.cfg.format;
        let render_bucket = |b: &HistoBucket| -> (String, Json) {
            let key_str = f.double_string(b.key);
            let mut o = Json::obj();
            if !f.is_raw() {
                o.push("key_as_string", Json::Str(key_str.clone()));
            }
            o.push("key", Json::Float(b.key));
            o.push("doc_count", Json::from(b.doc_count));
            b.aggs.render_into(&mut o, rc);
            (key_str, o)
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
    fn ram_bytes(&self) -> usize {
        self.buckets.iter().map(|b| 48 + b.aggs.ram_bytes()).sum::<usize>() + 128
    }
}

impl Histogram {
    fn clone_shell(&self) -> Histogram {
        Histogram { buckets: Vec::new(), cfg: self.cfg.clone(), empty: self.empty.clone(), empty_aggs: self.empty_aggs.clone() }
    }
}

impl MultiBuckets for Histogram {
    fn len(&self) -> usize {
        self.buckets.len()
    }
    fn doc_count(&self, i: usize) -> u64 {
        self.buckets[i].doc_count
    }
    fn key(&self, i: usize) -> Json {
        Json::Float(self.buckets[i].key)
    }
    fn key_string(&self, i: usize) -> String {
        self.cfg.format.double_string(self.buckets[i].key)
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
        let mut slots: Vec<Option<HistoBucket>> = self.buckets.drain(..).map(Some).collect();
        self.buckets = order.iter().map(|&i| slots[i].take().expect("bucket")).collect();
    }
    fn numeric_key(&self, i: usize) -> Option<f64> {
        Some(self.buckets[i].key)
    }
}

/// `InternalDateHistogram.EmptyBucketInfo`.
#[derive(Debug)]
pub struct DateEmptyInfo {
    /// The rounding without its offset.
    pub rounding: Rounding,
    /// `extended_bounds`, rounded without the offset.
    pub bounds: Option<(Option<i64>, Option<i64>)>,
}

/// A date histogram bucket.
#[derive(Clone, Debug)]
pub struct DateBucket {
    pub key: i64,
    pub doc_count: u64,
    pub aggs: Aggs,
}

/// `InternalDateHistogram`.
#[derive(Clone, Debug)]
pub struct DateHistogram {
    pub buckets: Vec<DateBucket>,
    pub cfg: Arc<HistoConfig>,
    pub offset: i64,
    pub empty: Option<Arc<DateEmptyInfo>>,
    pub empty_aggs: Option<Aggs>,
}

impl DateHistogram {
    fn next_key(&self, e: &DateEmptyInfo, key: i64) -> i64 {
        e.rounding.next_rounding_value(key.wrapping_sub(self.offset)).wrapping_add(self.offset)
    }

    /// Calls `on` with every empty key, in order. Keys use Java's wrapping `long` arithmetic;
    /// a step that wraps (does not move forward) ends a run instead of looping.
    fn iterate_empty(&self, list: &[DateBucket], mut on: impl FnMut(i64) -> Result<()>) -> Result<()> {
        let Some(e) = &self.empty else { return Ok(()) };
        let off = self.offset;
        let run = |mut key: i64, end: i64, inclusive: bool, on: &mut dyn FnMut(i64) -> Result<()>| -> Result<()> {
            while key < end || (inclusive && key == end) {
                on(key)?;
                let next = self.next_key(e, key);
                if next <= key {
                    break;
                }
                key = next;
            }
            Ok(())
        };
        if let Some((bmin, bmax)) = e.bounds {
            match list.first() {
                None => {
                    if let (Some(mn), Some(mx)) = (bmin, bmax) {
                        run(mn.wrapping_add(off), mx.wrapping_add(off), true, &mut on)?;
                    }
                }
                Some(first) => {
                    if let Some(mn) = bmin {
                        run(mn.wrapping_add(off), first.key, false, &mut on)?;
                    }
                }
            }
        }
        for w in list.windows(2) {
            let next = self.next_key(e, w[0].key);
            if next > w[0].key {
                run(next, w[1].key, false, &mut on)?;
            }
        }
        if let (Some((_, Some(mx))), Some(last)) = (e.bounds, list.last())
            && mx.wrapping_add(off) > last.key
        {
            let next = self.next_key(e, last.key);
            if next > last.key {
                run(next, mx.wrapping_add(off), true, &mut on)?;
            }
        }
        Ok(())
    }

    fn add_empty_buckets(&self, list: &mut Vec<DateBucket>, ctx: &mut ReduceCtx<'_>) -> Result<()> {
        let mut pending = 0usize;
        self.iterate_empty(list, |_| {
            pending += 1;
            if pending >= REPORT_EMPTY_EVERY {
                ctx.consume_buckets(pending)?;
                pending = 0;
            }
            Ok(())
        })?;
        ctx.consume_buckets(pending)?;
        let empty_aggs = match &self.empty_aggs {
            Some(a) => Aggs::reduce_lists(vec![a.clone()], ctx)?,
            None => Aggs::default(),
        };
        let mut keys = Vec::new();
        self.iterate_empty(list, |k| {
            keys.push(k);
            Ok(())
        })?;
        for k in keys {
            list.push(DateBucket { key: k, doc_count: 0, aggs: empty_aggs.clone() });
        }
        list.sort_by_key(|b| b.key);
        Ok(())
    }

    fn clone_shell(&self) -> DateHistogram {
        DateHistogram { buckets: Vec::new(), cfg: self.cfg.clone(), offset: self.offset, empty: self.empty.clone(), empty_aggs: self.empty_aggs.clone() }
    }
}

impl Internal for DateHistogram {
    fn type_name(&self) -> &'static str {
        "date_histogram"
    }
    fn class_name(&self) -> &'static str {
        "InternalDateHistogram"
    }
    fn must_reduce_single(&self) -> bool {
        true
    }
    fn reduce(list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let first = list[0].clone_shell();
        let min_doc_count = first.cfg.min_doc_count;
        let is_final = ctx.is_final();
        let lists: Vec<Vec<DateBucket>> = list.into_iter().map(|h| h.buckets).collect();
        let mut buckets = merge_keyed(
            lists,
            |b| b.key,
            |a: i64, b: i64| a.cmp(&b),
            |g, ctx| {
                let key = g[0].key;
                let doc_count = g.iter().map(|b| b.doc_count).sum();
                let aggs = Aggs::reduce_hashed(g.into_iter().map(|b| b.aggs).collect(), ctx)?;
                Ok(DateBucket { key, doc_count, aggs })
            },
            |b| !is_final || b.doc_count >= min_doc_count,
            ctx,
        )?;
        if is_final {
            if min_doc_count == 0 {
                first.add_empty_buckets(&mut buckets, ctx)?;
            }
            let order = &first.cfg.order;
            if order.is_key_order() && !order.is_key_asc() {
                buckets.reverse();
            } else if !(order.is_key_order() && order.is_key_asc()) {
                let refs: Vec<&Aggs> = buckets.iter().map(|b| &b.aggs).collect();
                let sorter = ReduceSorter::new(order, &refs)?;
                let mut idx: Vec<usize> = (0..buckets.len()).collect();
                idx.sort_by(|&i, &j| sorter.cmp(i, j, |k| buckets[k].doc_count, |a, b| buckets[a].key.cmp(&buckets[b].key)));
                let mut slots: Vec<Option<DateBucket>> = buckets.drain(..).map(Some).collect();
                buckets = idx.into_iter().map(|i| slots[i].take().expect("bucket")).collect();
            }
        }
        Ok(DateHistogram { buckets, ..first })
    }
    fn render(&self, out: &mut Json, rc: &RenderCtx) {
        let f = &self.cfg.format;
        let render_bucket = |b: &DateBucket| -> (String, Json) {
            let key_str = f.long_string(b.key);
            let mut o = Json::obj();
            if !f.is_raw() {
                o.push("key_as_string", Json::Str(key_str.clone()));
            }
            o.push("key", Json::Int(b.key));
            o.push("doc_count", Json::from(b.doc_count));
            b.aggs.render_into(&mut o, rc);
            (key_str, o)
        };
        if self.cfg.keyed {
            out.push("buckets", Json::Obj(self.buckets.iter().map(render_bucket).collect()));
        } else {
            out.push("buckets", Json::Arr(self.buckets.iter().map(|b| render_bucket(b).1).collect()));
        }
        if self.cfg.downsampled_offset {
            out.push("downsampled_results_offset", Json::Bool(true));
        }
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_bucket_property(self, name, path)
    }
    fn ram_bytes(&self) -> usize {
        self.buckets.iter().map(|b| 48 + b.aggs.ram_bytes()).sum::<usize>() + 128
    }
}

impl MultiBuckets for DateHistogram {
    fn len(&self) -> usize {
        self.buckets.len()
    }
    fn doc_count(&self, i: usize) -> u64 {
        self.buckets[i].doc_count
    }
    fn key(&self, i: usize) -> Json {
        Json::Int(self.buckets[i].key)
    }
    fn key_string(&self, i: usize) -> String {
        self.cfg.format.long_string(self.buckets[i].key)
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
        let mut slots: Vec<Option<DateBucket>> = self.buckets.drain(..).map(Some).collect();
        self.buckets = order.iter().map(|&i| slots[i].take().expect("bucket")).collect();
    }
    fn numeric_key(&self, i: usize) -> Option<f64> {
        Some(self.buckets[i].key as f64)
    }
}

/// Count of inner buckets (for `bucket_count`-style accounting of removed buckets).
pub fn inner(aggs: &Aggs) -> usize {
    count_inner_buckets(aggs)
}

#[cfg(test)]
mod tests {
    #[test]
    fn floor_is_f64_floor() {
        let edge = [0.0, -0.0, 0.5, -0.5, 1.0, -1.0, 1e-300, -1e-300, 2.5, -2.5, 4_503_599_627_370_495.5, -4_503_599_627_370_495.5, 4_503_599_627_370_496.0, 9e15, -9e15, 1e300, f64::INFINITY, f64::NEG_INFINITY, f64::NAN, f64::MIN_POSITIVE];
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let random = (0..100_000).map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            f64::from_bits(x)
        });
        for v in edge.into_iter().chain(random) {
            assert_eq!(super::floor(v).to_bits(), v.floor().to_bits(), "{v:e}");
        }
    }
}
