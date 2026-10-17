//! Aggregations (`compaction.c`): the per-bucket contexts compaction rules keep and range
//! queries run, with the module's arithmetic as compiled.

use crate::client::jemalloc_size;

/// `TS_AGG_TYPES_T` (the numbers are saved in RDB rules).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggType {
    Min = 1,
    Max,
    Sum,
    Avg,
    Count,
    First,
    Last,
    Range,
    StdP,
    StdS,
    VarP,
    VarS,
    Twa,
    CountNan,
    CountAll,
}

const ALL: [AggType; 15] = [
    AggType::Min,
    AggType::Max,
    AggType::Sum,
    AggType::Avg,
    AggType::Count,
    AggType::First,
    AggType::Last,
    AggType::Range,
    AggType::StdP,
    AggType::StdS,
    AggType::VarP,
    AggType::VarS,
    AggType::Twa,
    AggType::CountNan,
    AggType::CountAll,
];

impl AggType {
    pub fn from_u64(v: u64) -> Option<AggType> {
        ALL.iter().copied().find(|a| *a as u64 == v)
    }

    /// `StringLenAggTypeToEnum`.
    pub fn parse(s: &[u8]) -> Option<AggType> {
        let l = s.to_ascii_lowercase();
        ALL.iter().copied().find(|a| a.lower().as_bytes() == &l[..])
    }

    pub fn lower(self) -> &'static str {
        match self {
            AggType::Min => "min",
            AggType::Max => "max",
            AggType::Sum => "sum",
            AggType::Avg => "avg",
            AggType::Count => "count",
            AggType::First => "first",
            AggType::Last => "last",
            AggType::Range => "range",
            AggType::StdP => "std.p",
            AggType::StdS => "std.s",
            AggType::VarP => "var.p",
            AggType::VarS => "var.s",
            AggType::Twa => "twa",
            AggType::CountNan => "countnan",
            AggType::CountAll => "countall",
        }
    }

    pub fn upper(self) -> String {
        self.lower().to_ascii_uppercase()
    }

    /// `isValueValid`.
    pub fn valid(self, v: f64) -> bool {
        match self {
            AggType::CountNan => v.is_nan(),
            AggType::CountAll => true,
            _ => !v.is_nan(),
        }
    }

    /// Whether the class has the TWA hooks (bucket params, neighbour samples).
    pub fn twa(self) -> bool {
        self == AggType::Twa
    }

    /// `finalizeEmpty` without a context: 0 for sums and counts, NaN otherwise (LAST carries its
    /// value, see [`Agg::finalize_empty`]).
    pub fn empty_zero(self) -> bool {
        matches!(self, AggType::Sum | AggType::Count | AggType::CountNan | AggType::CountAll)
    }

    /// What the context struct takes (MEMORY USAGE).
    pub fn context_size(self) -> usize {
        jemalloc_size(match self {
            AggType::Min | AggType::Max | AggType::Range => 16,
            AggType::Twa => 80,
            _ => 24,
        })
    }

    pub fn create(self, reverse: bool) -> Agg {
        let ctx = match self {
            AggType::Min | AggType::Max | AggType::Range => Ctx::MaxMin { min: f64::MAX, max: -f64::MAX },
            AggType::Sum | AggType::Count | AggType::CountNan | AggType::CountAll => Ctx::Single { value: 0.0, ts: 0, fresh: true },
            AggType::Last => Ctx::Single { value: f64::NAN, ts: 0, fresh: true },
            AggType::First => Ctx::First { value: 0.0, reset: true, ts: 0 },
            AggType::Avg => Ctx::Avg { val: 0.0, cnt: 0.0, overflow: false },
            AggType::StdP | AggType::StdS | AggType::VarP | AggType::VarS => Ctx::Std { sum: 0.0, sum2: 0.0, cnt: 0 },
            AggType::Twa => Ctx::Twa(Twa::new(reverse)),
        };
        Agg { kind: self, ctx }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Twa {
    pub res: f64,
    pub prev_ts: u64,
    pub prev_value: f64,
    pub bucket_start: u64,
    pub bucket_end: u64,
    pub first_ts: u64,
    pub last_ts: u64,
    pub first_bucket: bool,
    pub reverse: bool,
    pub iteration: i64,
}

impl Twa {
    fn new(reverse: bool) -> Self {
        Twa { res: 0.0, prev_ts: 0, prev_value: 0.0, bucket_start: 0, bucket_end: 0, first_ts: 0, last_ts: 0, first_bucket: true, reverse, iteration: 0 }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Ctx {
    /// SUM, COUNT*, LAST (`SingleValueContext`).
    Single { value: f64, ts: u64, fresh: bool },
    First { value: f64, reset: bool, ts: u64 },
    Avg { val: f64, cnt: f64, overflow: bool },
    Std { sum: f64, sum2: f64, cnt: u64 },
    MaxMin { min: f64, max: f64 },
    Twa(Twa),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Agg {
    pub kind: AggType,
    pub ctx: Ctx,
}

/// `variance`, with the product fused into the sum as GCC compiles it.
fn variance(sum: f64, sum2: f64, count: f64) -> f64 {
    if count == 0.0 {
        return 0.0;
    }
    let m = sum / count;
    (m * m).mul_add(count, sum2 - 2.0 * sum * sum / count) / count
}

impl Agg {
    /// `appendValue`.
    pub fn append(&mut self, value: f64, ts: u64) {
        let kind = self.kind;
        match &mut self.ctx {
            Ctx::MaxMin { min, max } => match kind {
                AggType::Max => {
                    if value > *max {
                        *max = value;
                    }
                }
                AggType::Min => {
                    if value < *min {
                        *min = value;
                    }
                }
                _ => {
                    if value > *max {
                        *max = value;
                    }
                    if value < *min {
                        *min = value;
                    }
                }
            },
            Ctx::Single { value: v, ts: t, fresh } => match kind {
                AggType::Sum => *v += value,
                AggType::Last => {
                    if *fresh || ts >= *t {
                        *v = value;
                        *t = ts;
                        *fresh = false;
                    }
                }
                _ => *v += 1.0,
            },
            Ctx::First { value: v, reset, ts: t } => {
                if *reset || ts < *t {
                    *reset = false;
                    *v = value;
                    *t = ts;
                }
            }
            Ctx::Avg { val, cnt, overflow } => {
                *cnt += 1.0;
                if ((*val < 0.0) == (value < 0.0) && val.abs() > f64::MAX - value.abs()) || *overflow {
                    // The module works in `long double` here; this is its formula in doubles.
                    let mut ld = *val / *cnt;
                    if *overflow {
                        ld *= *cnt - 1.0;
                    }
                    ld += value / *cnt;
                    *val = ld;
                    *overflow = true;
                } else {
                    *val += value;
                }
            }
            Ctx::Std { sum, sum2, cnt } => {
                *cnt += 1;
                *sum += value;
                *sum2 += value * value;
            }
            Ctx::Twa(c) => {
                let (mut t1, mut t2) = (c.prev_ts, ts);
                let (mut v1, mut v2) = (c.prev_value, value);
                if c.reverse {
                    std::mem::swap(&mut t1, &mut t2);
                    std::mem::swap(&mut v1, &mut v2);
                }
                let delta_time = t2.wrapping_sub(t1) as f64;
                let delta_val = v2 - v1;
                let ta = c.bucket_start;
                if c.iteration == 0 {
                    if !c.first_bucket {
                        c.first_ts = ta;
                        let vab = v1 + (ta.wrapping_sub(t1) as f64 * delta_val) / delta_time;
                        if !c.reverse {
                            c.res += ((vab + v2) * t2.wrapping_sub(ta) as f64) / 2.0;
                        } else {
                            c.res += ((vab + v1) * ta.wrapping_sub(t1) as f64) / 2.0;
                        }
                    } else {
                        c.first_ts = ts;
                    }
                } else {
                    c.res += ((v1 + v2) * t2.wrapping_sub(t1) as f64) / 2.0;
                }
                c.last_ts = ts;
                c.prev_ts = ts;
                c.prev_value = value;
                c.iteration += 1;
            }
        }
    }

    /// `resetContext`.
    pub fn reset(&mut self) {
        let last = self.kind == AggType::Last;
        match &mut self.ctx {
            Ctx::Single { value, ts, fresh } => {
                if !last {
                    *value = 0.0;
                    *ts = 0;
                }
                *fresh = true;
            }
            Ctx::Twa(c) => *c = Twa::new(c.reverse),
            ctx => *ctx = self.kind.create(false).ctx,
        }
    }

    /// `finalize`: Err when the context can't give a value (an empty average or deviation).
    pub fn finalize(&self) -> Result<f64, f64> {
        match &self.ctx {
            Ctx::MaxMin { min, max } => Ok(match self.kind {
                AggType::Max => *max,
                AggType::Min => *min,
                _ => max - min,
            }),
            Ctx::Single { value, .. } | Ctx::First { value, .. } => Ok(*value),
            Ctx::Avg { val, cnt, overflow } => {
                if *cnt == 0.0 {
                    Err(0.0)
                } else if *overflow {
                    Ok(*val)
                } else {
                    Ok(val / cnt)
                }
            }
            Ctx::Std { sum, sum2, cnt } => {
                if *cnt == 0 {
                    let v = 0.0;
                    return Err(if matches!(self.kind, AggType::StdP | AggType::StdS) { f64::sqrt(v) } else { v });
                }
                let c = *cnt as f64;
                let var = match self.kind {
                    AggType::VarP | AggType::StdP => variance(*sum, *sum2, c),
                    _ if *cnt == 1 => 0.0,
                    _ => variance(*sum, *sum2, c) * c / (cnt - 1) as f64,
                };
                Ok(if matches!(self.kind, AggType::StdP | AggType::StdS) { var.sqrt() } else { var })
            }
            Ctx::Twa(c) => {
                if c.last_ts == c.first_ts {
                    Ok(c.prev_value)
                } else {
                    Ok(c.res / (c.last_ts.wrapping_sub(c.first_ts) as i64).unsigned_abs() as f64)
                }
            }
        }
    }

    /// `finalize` for a context that has values, its value whatever it says.
    pub fn value(&self) -> f64 {
        match self.finalize() {
            Ok(v) | Err(v) => v,
        }
    }

    /// `finalizeEmpty`.
    pub fn finalize_empty(&self) -> f64 {
        match (&self.ctx, self.kind) {
            (Ctx::Single { value, .. }, AggType::Last) => *value,
            (_, k) if k.empty_zero() => 0.0,
            _ => f64::NAN,
        }
    }

    /// `addBucketParams` (TWA only).
    pub fn bucket_params(&mut self, start: u64, end: u64) {
        if let Ctx::Twa(c) = &mut self.ctx {
            let (s, e) = if c.reverse { (end, start) } else { (start, end) };
            c.bucket_start = s;
            c.bucket_end = e;
        }
    }

    /// `addPrevBucketLastSample` (TWA only).
    pub fn prev_bucket_last(&mut self, value: f64, ts: u64) {
        if let Ctx::Twa(c) = &mut self.ctx {
            c.prev_value = value;
            c.prev_ts = ts;
            c.first_bucket = false;
        }
    }

    /// `addNextBucketFirstSample` (TWA only).
    pub fn next_bucket_first(&mut self, value: f64, ts: u64) {
        if let Ctx::Twa(c) = &mut self.ctx {
            let (mut t1, mut t2) = (c.prev_ts, ts);
            let (mut v1, mut v2) = (c.prev_value, value);
            if c.reverse {
                std::mem::swap(&mut t1, &mut t2);
                std::mem::swap(&mut v1, &mut v2);
            }
            let delta_time = t2.wrapping_sub(t1) as f64;
            let delta_val = v2 - v1;
            let tb = c.bucket_end;
            let vab = v1 + (tb.wrapping_sub(t1) as f64 * delta_val) / delta_time;
            if !c.reverse {
                c.res += ((vab + v1) * tb.wrapping_sub(t1) as f64) / 2.0;
            } else {
                c.res += ((vab + v2) * t2.wrapping_sub(tb) as f64) / 2.0;
            }
            c.last_ts = tb;
        }
    }

    /// `getLastSample` (TWA only).
    pub fn last_sample(&self) -> Option<(u64, f64)> {
        match &self.ctx {
            Ctx::Twa(c) => Some((c.prev_ts, c.prev_value)),
            _ => None,
        }
    }

    /// `LastValueSeedLocf`.
    pub fn seed_locf(&mut self, v: f64, t: u64) {
        if let Ctx::Single { value, ts, .. } = &mut self.ctx {
            *value = v;
            *ts = t;
        }
    }

    /// `LastValueIsUnseeded`.
    pub fn unseeded(&self) -> bool {
        matches!(&self.ctx, Ctx::Single { value, .. } if value.is_nan())
    }

    /// `writeContext`: the doubles and unsigneds saved, in order.
    pub fn write(&self, out: &mut Vec<Field>) {
        match &self.ctx {
            Ctx::Single { value, .. } => out.push(Field::D(*value)),
            Ctx::First { value, reset, .. } => {
                out.push(Field::D(*value));
                out.push(Field::U(*reset as u64));
            }
            Ctx::Avg { val, cnt, overflow } => {
                out.push(Field::D(*val));
                out.push(Field::D(*cnt));
                out.push(Field::U(*overflow as u64));
            }
            Ctx::Std { sum, sum2, cnt } => {
                out.push(Field::D(*sum));
                out.push(Field::D(*sum2));
                out.push(Field::U(*cnt));
            }
            Ctx::MaxMin { min, max } => {
                out.push(Field::D(*max));
                out.push(Field::D(*min));
            }
            Ctx::Twa(c) => {
                out.push(Field::D(c.res));
                out.push(Field::U(c.prev_ts));
                out.push(Field::D(c.prev_value));
                out.push(Field::U(c.bucket_start));
                out.push(Field::U(c.bucket_end));
                out.push(Field::U(c.first_ts));
                out.push(Field::U(c.last_ts));
                out.push(Field::U(c.first_bucket as u64));
                out.push(Field::U(c.iteration as u64));
                out.push(Field::U(c.reverse as u64));
            }
        }
    }

    /// `readContext` (current encoding).
    pub fn read(&mut self, d: &mut dyn FnMut() -> Option<f64>, u: &mut dyn FnMut() -> Option<u64>) -> Option<()> {
        match &mut self.ctx {
            Ctx::Single { value, .. } => *value = d()?,
            Ctx::First { value, reset, .. } => {
                *value = d()?;
                *reset = u()? != 0;
            }
            Ctx::Avg { val, cnt, overflow } => {
                *val = d()?;
                *cnt = d()?;
                *overflow = u()? != 0;
            }
            Ctx::Std { sum, sum2, cnt } => {
                *sum = d()?;
                *sum2 = d()?;
                *cnt = u()?;
            }
            Ctx::MaxMin { min, max } => {
                *max = d()?;
                *min = d()?;
            }
            Ctx::Twa(c) => {
                c.res = d()?;
                c.prev_ts = u()?;
                c.prev_value = d()?;
                c.bucket_start = u()?;
                c.bucket_end = u()?;
                c.first_ts = u()?;
                c.last_ts = u()?;
                c.first_bucket = u()? != 0;
                c.iteration = u()? as i64;
                c.reverse = u()? != 0;
            }
        }
        Some(())
    }
}

/// A saved context field.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Field {
    D(f64),
    U(u64),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(kind: AggType, vals: &[f64]) -> f64 {
        let mut a = kind.create(false);
        for (i, v) in vals.iter().enumerate() {
            a.append(*v, i as u64);
        }
        a.value()
    }

    #[test]
    fn aggregates() {
        let v = [3.0, 1.0, 4.0, 1.0, 5.0];
        assert_eq!(run(AggType::Min, &v), 1.0);
        assert_eq!(run(AggType::Max, &v), 5.0);
        assert_eq!(run(AggType::Range, &v), 4.0);
        assert_eq!(run(AggType::Sum, &v), 14.0);
        assert_eq!(run(AggType::Avg, &v), 2.8);
        assert_eq!(run(AggType::Count, &v), 5.0);
        assert_eq!(run(AggType::First, &v), 3.0);
        assert_eq!(run(AggType::Last, &v), 5.0);
        assert!((run(AggType::VarP, &v) - 2.56).abs() < 1e-12);
        assert!((run(AggType::VarS, &v) - 3.2).abs() < 1e-12);
        assert_eq!(AggType::parse(b"STD.p"), Some(AggType::StdP));
        assert_eq!(AggType::parse(b"nope"), None);
        let mut l = AggType::Last.create(false);
        assert!(l.unseeded());
        l.append(2.0, 5);
        l.reset();
        assert_eq!(l.finalize_empty(), 2.0);
    }
}
