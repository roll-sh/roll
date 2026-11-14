//! `rate`: a sum (of values, of value counts or of doc counts) per date-histogram bucket,
//! divided by the bucket's size in a time unit.
//!
//! The rate needs an enclosing `SizedBucketAggregator` — a `date_histogram`, or a `composite`
//! with exactly one `date_histogram` source — published to it through [`BindCtx::sized`]. As a
//! direct child it divides by its own bucket's size (`roundingSize(bucketStart, unit)`, which is
//! where calendar months get their real number of days); deeper down it divides by the
//! ancestor's nominal size (`roundingSize(unit)`), which only works when the units are compatible.
//!
//! Sums are Kahan sums in a `BigArrays`-grown array; owning ordinals past the array's (oversized)
//! length build `InternalRate(0, 1)` without a divisor, exactly like Elasticsearch, which matters
//! for the "first divisor wins" cross-shard reduce.

use search_index::SegmentContext;

use crate::collect::{Aggregator, Batch, BindCtx, SizedAncestor, single_value_validate_sort};
use crate::error::{AggError, Result};
use crate::format::DocValueFormat;
use crate::internal::{AggValue, CompensatedSum, Rate};
use crate::memory::MemTracker;
use crate::request::{AggDef, PathElement};
use crate::rounding::DateUnit;
use crate::values::{BoundVs, DocCounter, NumReader, VsOpts, VsSpec, VsType, bind};
use crate::xparser::{Field, Tokens, XParser};

/// `RateMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateMode {
    Sum,
    ValueCount,
}

/// A parsed `rate`.
#[derive(Clone, Debug)]
pub struct RateSpec {
    pub vs: VsSpec,
    pub unit: Option<DateUnit>,
    pub mode: Option<RateMode>,
}

const OPTS: VsOpts = VsOpts { scriptable: true, formattable: true, timezone: false, field_required: false };

pub fn parse(p: &mut XParser<'_>) -> Result<RateSpec> {
    let mut fields = VsSpec::fields(OPTS);
    fields.push(Field::new("unit", Tokens::STRING));
    fields.push(Field::new("mode", Tokens::STRING));
    let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
    let mut spec = RateSpec { vs: VsSpec::default(), unit: None, mode: None };
    p.object("rate", &fields, |p, i| {
        match names[i] {
            "unit" => {
                let t = p.text()?;
                spec.unit = Some(DateUnit::parse_calendar(&t).ok_or_else(|| AggError::illegal_argument(format!("Unsupported unit {t}")))?);
            }
            "mode" => {
                // `RateMode.valueOf(name.toUpperCase(Locale.ROOT))`.
                let t = p.text()?.to_uppercase();
                spec.mode = Some(match t.as_str() {
                    "SUM" => RateMode::Sum,
                    "VALUE_COUNT" => RateMode::ValueCount,
                    _ => return Err(AggError::illegal_argument(format!("No enum constant org.elasticsearch.xpack.analytics.rate.RateMode.{t}"))),
                });
            }
            other => {
                spec.vs.parse_field(p, other)?;
            }
        }
        Ok(())
    })?;
    Ok(spec)
}

// One per aggregator: variant sizes don't matter.
#[allow(clippy::large_enum_variant)]
enum Source {
    /// No field or script: sums doc counts (`_doc_count` aware).
    DocCount(DocCounter),
    Values { vs: BoundVs, reader: Option<NumReader>, mode: RateMode },
    /// A pre-aggregated `histogram` field (`HistogramRateAggregator`: `sum` adds each centroid's
    /// value — not weighted by its count — and `value_count` its count).
    Histogram { field: Option<u32>, reader: Option<crate::histogram_field::HistogramReader>, mode: RateMode },
}

/// Binds `rate`.
pub fn create(def: &AggDef, spec: &RateSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let by_doc_count = spec.vs.field.is_none() && spec.vs.script.is_none();
    if by_doc_count && spec.mode.is_some() {
        return Err(AggError::illegal_argument("The mode parameter is only supported with field or script"));
    }
    let (source, format) = if by_doc_count {
        (Source::DocCount(DocCounter::none()), DocValueFormat::Raw)
    } else {
        let vs = bind(&spec.vs, ctx.lookup, VsType::Numeric, false)?;
        if !vs.unmapped && vs.vs_type != VsType::Numeric && vs.vs_type != VsType::Histogram {
            return Err(vs.unsupported("rate"));
        }
        if vs.is_counter() && vs.script.is_none() {
            // `TimeSeriesValuesSourceType.COUNTER` → `TimeSeriesRateAggregator`.
            return crate::metrics::ts_rate::create(def, vs, spec.unit, ctx);
        }
        let format = vs.format.clone();
        if vs.unmapped && vs.script.is_none() && vs.missing.is_some() {
            // An unmapped field has no field context: with a `missing` value the aggregator
            // counts docs (`computeWithDocCount`); without one it collects nothing.
            (Source::DocCount(DocCounter::none()), format)
        } else if vs.vs_type == VsType::Histogram && !vs.unmapped {
            let field = vs.field.as_ref().map(|f| f.number);
            (Source::Histogram { field, reader: None, mode: spec.mode.unwrap_or(RateMode::Sum) }, format)
        } else {
            (Source::Values { vs, reader: None, mode: spec.mode.unwrap_or(RateMode::Sum) }, format)
        }
    };
    let Some(ancestor) = ctx.sized.clone() else {
        return Err(AggError::illegal_argument(
            "The rate aggregation can only be used inside a date histogram aggregation or composite aggregation with one date histogram value source",
        ));
    };
    let direct = ancestor.depth + 1 == ctx.parents.len();
    Ok(Box::new(RateAgg {
        source,
        doc_count_field: ctx.doc_count_field,
        sums: Vec::new(),
        cap: 1,
        unit: spec.unit,
        ancestor,
        direct,
        format,
        tracker: ctx.tracker(def),
    }))
}

/// `BigArrays.overSize(minSize, DOUBLE_PAGE_SIZE, Double.BYTES)`.
fn over_size(min: usize) -> usize {
    const PAGE: usize = 16384 / 8;
    if min < PAGE { (min + (min >> 3).max(3)).min(PAGE) } else { min.div_ceil(PAGE) * PAGE }
}

struct RateAgg {
    source: Source,
    doc_count_field: Option<u32>,
    sums: Vec<CompensatedSum>,
    /// The length of Elasticsearch's `sums` array (initially 1, grown with `overSize`).
    cap: usize,
    unit: Option<DateUnit>,
    ancestor: SizedAncestor,
    /// Whether the ancestor is the direct parent (sizes per bucket).
    direct: bool,
    format: DocValueFormat,
    tracker: MemTracker,
}

impl RateAgg {
    #[inline]
    fn slot(&mut self, ord: u32) -> Result<&mut CompensatedSum> {
        let i = ord as usize;
        if i >= self.sums.len() {
            if i + 1 > self.cap {
                self.cap = over_size(i + 1);
            }
            self.sums.resize(i + 1, CompensatedSum::default());
            self.tracker.ensure(self.sums.capacity() * std::mem::size_of::<CompensatedSum>())?;
        }
        Ok(&mut self.sums[i])
    }

    /// `AbstractRateAggregator.divisor(owningBucketOrd)`.
    fn divisor(&self, ord: u32) -> Result<f64> {
        let Some(rounding) = &self.ancestor.rounding else {
            return Err(AggError::illegal_argument(format!(
                "aggregation [{}] does not have exactly one date_histogram value source; exactly one is required when using with rate aggregation",
                self.ancestor.name
            )));
        };
        let Some(unit) = self.unit else { return Ok(1.0) };
        if !self.direct {
            return rounding.rounding_size(unit);
        }
        let start = {
            let starts = self.ancestor.starts.lock().unwrap_or_else(|e| e.into_inner());
            starts.get(ord as usize).copied()
        };
        match start {
            Some(Some(s)) => rounding.rounding_size_at(s, unit),
            // A composite bucket whose date is missing (`missing_bucket`): Java unboxes null.
            Some(None) => Err(AggError::new(500, "null_pointer_exception", "Cannot invoke \"java.lang.Long.longValue()\" because \"value\" is null")),
            None => rounding.rounding_size(unit),
        }
    }

    fn value(&self, ord: u32) -> Result<Rate> {
        if ord as usize >= self.cap {
            return Ok(self.empty());
        }
        let sum = self.sums.get(ord as usize).map_or(0.0, |s| s.value);
        Ok(Rate { sum, divisor: self.divisor(ord)?, format: self.format.clone() })
    }

    fn empty(&self) -> Rate {
        Rate { sum: 0.0, divisor: 1.0, format: self.format.clone() }
    }
}

impl Aggregator for RateAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        match &mut self.source {
            Source::DocCount(c) => *c = DocCounter::open(seg, self.doc_count_field),
            Source::Values { vs, reader, .. } => {
                *reader = if vs.unmapped && vs.missing.is_none() { None } else { Some(vs.numeric_reader(seg)?) };
            }
            Source::Histogram { field, reader, .. } => *reader = Some(crate::histogram_field::HistogramReader::open(seg, *field)),
        }
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        if let Source::Histogram { reader, mode, .. } = &mut self.source {
            let mode = *mode;
            let Some(mut r) = reader.take() else { return Ok(()) };
            let res = (|| {
                for i in 0..b.len() {
                    let centroids = r.centroids(b.docs[i])?;
                    if centroids.is_empty() {
                        continue;
                    }
                    let s = self.slot(b.ords[i])?;
                    for &(v, c) in centroids {
                        s.add(match mode {
                            RateMode::Sum => v,
                            RateMode::ValueCount => c as f64,
                        });
                    }
                }
                Ok(())
            })();
            if let Source::Histogram { reader, .. } = &mut self.source {
                *reader = Some(r);
            }
            return res;
        }
        match &mut self.source {
            Source::Histogram { .. } => Ok(()),
            Source::DocCount(counter) => {
                let counter = std::mem::replace(counter, DocCounter::none());
                let r = (|| {
                    for i in 0..b.len() {
                        let n = counter.count(b.docs[i]);
                        self.slot(b.ords[i])?.add(n as f64);
                    }
                    Ok(())
                })();
                self.source = Source::DocCount(counter);
                r
            }
            Source::Values { reader, mode, .. } => {
                let mode = *mode;
                let Some(mut r) = reader.take() else { return Ok(()) };
                let res = (|| {
                    for i in 0..b.len() {
                        let vals = r.doubles(b.docs[i], b.score(i))?;
                        if vals.is_empty() {
                            continue;
                        }
                        let ord = b.ords[i];
                        // `slot` borrows self mutably; the values borrow the reader only.
                        let s = self.slot(ord)?;
                        match mode {
                            RateMode::Sum => {
                                for &v in vals.iter() {
                                    s.add(v);
                                }
                            }
                            RateMode::ValueCount => s.add(vals.len() as f64),
                        }
                    }
                    Ok(())
                })();
                if let Source::Values { reader, .. } = &mut self.source {
                    *reader = Some(r);
                }
                res
            }
        }
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        owning.iter().map(|&o| Ok(AggValue::Rate(Box::new(self.value(o)?)))).collect()
    }

    fn build_empty(&self) -> AggValue {
        AggValue::Rate(Box::new(self.empty()))
    }

    fn needs_scores(&self) -> bool {
        match &self.source {
            Source::DocCount(_) | Source::Histogram { .. } => false,
            Source::Values { vs, .. } => vs.script.as_ref().is_some_and(|s| s.needs_score()),
        }
    }

    fn validate_sort(&self, name: &str, key: Option<&str>, rest: &[PathElement]) -> Result<()> {
        single_value_validate_sort(name, key, rest)
    }

    /// `AbstractRateAggregator.metric(owningBucketOrd)`.
    fn sort_value(&self, _key: Option<&str>, _rest: &[PathElement], ord: u32) -> f64 {
        if ord as usize >= self.cap {
            return 0.0;
        }
        let sum = self.sums.get(ord as usize).map_or(0.0, |s| s.value);
        self.divisor(ord).map_or(f64::NAN, |d| sum / d)
    }
}

#[cfg(test)]
mod tests {
    use super::over_size;

    #[test]
    fn big_arrays_over_size() {
        assert_eq!(over_size(1), 4);
        assert_eq!(over_size(5), 8);
        assert_eq!(over_size(40), 45);
        // Capped at a page (`Math.min(ArrayUtil.oversize(..), pageSize)`).
        assert_eq!(over_size(2047), 2048);
        assert_eq!(over_size(1900), 2048);
        assert_eq!(over_size(2048), 2048);
        assert_eq!(over_size(2049), 4096);
    }
}
