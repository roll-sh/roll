//! Date rounding (Elasticsearch's `Rounding`): calendar units (`calendar_interval`) and fixed
//! intervals (`fixed_interval`) in a time zone, with an offset.
//!
//! Results equal Elasticsearch's `java.time` reference implementations
//! (`JavaTimeToMidnightRounding`, `JavaTimeNotToMidnightRounding`, `JavaTimeRounding`), which
//! its faster prepared variants are tested against:
//! - units of a day or more round to the first instant of the local day (a midnight inside a DST
//!   gap rounds to the transition; a repeated midnight to its first occurrence);
//! - hours, minutes and seconds round to the latest local truncation that is not after the
//!   value, never across an offset transition;
//! - fixed intervals round local time down to a multiple of the interval, then convert back
//!   the same way.
//!
//! Fixed-offset zones (UTC, `+01:00`) take pure arithmetic paths. Region zones use
//! [`ZoneRules`]: the zone's offset transitions between 1850 and 2150 found once (by scanning
//! `chrono-tz` and bisecting to the second) and cached process-wide, so lookups are binary
//! searches.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{Offset, TimeZone as _};

use crate::error::{AggError, Result};
use crate::format::TimeZone;

const DAY: i64 = 86_400_000;

/// Elasticsearch's `Rounding.DateTimeUnit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateUnit {
    Week,
    Year,
    Quarter,
    Month,
    Day,
    Hour,
    Minute,
    Second,
}

impl DateUnit {
    /// `DATE_FIELD_UNITS` of `calendar_interval`.
    pub fn parse_calendar(s: &str) -> Option<DateUnit> {
        Some(match s {
            "year" | "1y" | "years" => DateUnit::Year,
            "quarter" | "1q" => DateUnit::Quarter,
            "month" | "1M" | "months" => DateUnit::Month,
            "week" | "1w" => DateUnit::Week,
            "day" | "1d" => DateUnit::Day,
            "hour" | "1h" => DateUnit::Hour,
            "minute" | "1m" => DateUnit::Minute,
            "second" | "1s" => DateUnit::Second,
            _ => return None,
        })
    }

    pub fn short_name(self) -> &'static str {
        match self {
            DateUnit::Week => "week",
            DateUnit::Year => "year",
            DateUnit::Quarter => "quarter",
            DateUnit::Month => "month",
            DateUnit::Day => "day",
            DateUnit::Hour => "hour",
            DateUnit::Minute => "minute",
            DateUnit::Second => "second",
        }
    }

    /// Units of a day or more round to midnight.
    pub fn to_midnight(self) -> bool {
        matches!(self, DateUnit::Week | DateUnit::Year | DateUnit::Quarter | DateUnit::Month | DateUnit::Day)
    }

    /// Whether the unit has a fixed length in millis (`isMillisBased`).
    pub fn is_millis_based(self) -> bool {
        !matches!(self, DateUnit::Year | DateUnit::Quarter | DateUnit::Month)
    }

    /// Millis for millis-based units, months otherwise (`ratio`).
    pub fn ratio(self) -> i64 {
        match self {
            DateUnit::Week => 7 * DAY,
            DateUnit::Day => DAY,
            DateUnit::Hour => 3_600_000,
            DateUnit::Minute => 60_000,
            DateUnit::Second => 1000,
            DateUnit::Year => 12,
            DateUnit::Quarter => 3,
            DateUnit::Month => 1,
        }
    }

    /// `DateTimeUnit.extraLocalOffsetLookup()`: how far before a value offsets are looked up.
    pub(crate) fn extra_local_offset_lookup(self) -> i64 {
        match self {
            DateUnit::Week => 7 * DAY,
            DateUnit::Year => 366 * DAY,
            DateUnit::Quarter => 92 * DAY,
            DateUnit::Month => 31 * DAY,
            u => u.ratio(),
        }
    }

    /// Length of the unit's base field in millis (hours/minutes/seconds).
    fn unit_millis(self) -> i64 {
        match self {
            DateUnit::Hour => 3_600_000,
            DateUnit::Minute => 60_000,
            DateUnit::Second => 1000,
            DateUnit::Day => DAY,
            DateUnit::Week => 7 * DAY,
            _ => 31 * DAY,
        }
    }

    /// `roundFloor` of local millis (UTC calendar arithmetic).
    pub fn floor_local(self, local: i64) -> i64 {
        match self {
            DateUnit::Second | DateUnit::Minute | DateUnit::Hour | DateUnit::Day => round_floor(local, self.ratio()),
            DateUnit::Week => round_floor(local.wrapping_add(3 * DAY), 7 * DAY).wrapping_sub(3 * DAY),
            DateUnit::Month | DateUnit::Quarter | DateUnit::Year => {
                let (y, m, _) = ymd(local);
                let m = match self {
                    DateUnit::Month => m,
                    DateUnit::Quarter => ((m - 1) / 3) * 3 + 1,
                    _ => 1,
                };
                millis_of(y, m, 1)
            }
        }
    }

    /// The next local midnight after a local midnight rounded to this unit.
    fn next_local(self, local_mid: i64) -> i64 {
        match self {
            DateUnit::Day => local_mid.wrapping_add(DAY),
            DateUnit::Week => local_mid.wrapping_add(7 * DAY),
            DateUnit::Month | DateUnit::Quarter | DateUnit::Year => {
                let (y, m, _) = ymd(local_mid);
                let add = match self {
                    DateUnit::Month => 1,
                    DateUnit::Quarter => 3,
                    _ => 12,
                };
                let total = y * 12 + (m as i64 - 1) + add;
                millis_of(total.div_euclid(12), (total.rem_euclid(12) + 1) as u32, 1)
            }
            _ => local_mid.wrapping_add(self.ratio()),
        }
    }
}

/// `DateUtils.roundFloor`.
#[inline]
pub fn round_floor(v: i64, unit: i64) -> i64 {
    // Java's `long` arithmetic wraps (Long.MIN_VALUE rounds to a large positive value).
    v.wrapping_sub(v.rem_euclid(unit))
}

/// `TimeIntervalRounding.roundKey`.
#[inline]
fn round_key(v: i64, interval: i64) -> i64 {
    v.div_euclid(interval)
}

/// The proleptic Gregorian date of local millis. Plain integer arithmetic (not chrono, which
/// stops at year ±262143) so values near `Long.MIN_VALUE`/`MAX_VALUE` (wrapped offsets,
/// extreme `missing` values) round like `java.time` does.
fn ymd(local: i64) -> (i64, u32, u32) {
    let z = local.div_euclid(DAY) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Local millis of a date's midnight (the inverse of [`ymd`]), wrapping like Java's `long`.
fn millis_of(y: i64, m: u32, d: u32) -> i64 {
    days_of(y, m, d).wrapping_mul(DAY)
}

/// Days from 1970-01-01 to a proleptic Gregorian date.
fn days_of(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `TimeValue.parseTimeValue(value, settingName)` in millis.
pub fn parse_time_value_millis(s: &str, setting: &str) -> Result<i64> {
    let normalized = s.to_ascii_lowercase();
    let normalized = normalized.trim();
    let (num, unit_nanos): (&str, i128) = if let Some(n) = normalized.strip_suffix("nanos") {
        (n, 1)
    } else if let Some(n) = normalized.strip_suffix("micros") {
        (n, 1_000)
    } else if let Some(n) = normalized.strip_suffix("ms") {
        (n, 1_000_000)
    } else if let Some(n) = normalized.strip_suffix('s') {
        (n, 1_000_000_000)
    } else if s.ends_with('m') {
        (&normalized[..normalized.len() - 1], 60_000_000_000)
    } else if let Some(n) = normalized.strip_suffix('h') {
        (n, 3_600_000_000_000)
    } else if let Some(n) = normalized.strip_suffix('d') {
        (n, 86_400_000_000_000)
    } else if normalized.starts_with('-') && normalized[1..].trim_start_matches('0') == "1" {
        return Ok(-1);
    } else if !normalized.is_empty() && normalized.bytes().all(|b| b == b'0') {
        return Ok(0);
    } else {
        return Err(AggError::illegal_argument(format!(
            "failed to parse setting [{setting}] with value [{s}] as a time value: unit is missing or unrecognized"
        )));
    };
    let t = num.trim();
    match crate::json::parse_java_long(t) {
        Some(v) => {
            if v < -1 {
                return Err(AggError::illegal_argument(format!(
                    "failed to parse setting [{setting}] with value [{s}] as a time value: negative durations are not supported"
                )));
            }
            if v == -1 {
                return Ok(-1);
            }
            Ok(((v as i128 * unit_nanos) / 1_000_000) as i64)
        }
        None => {
            if crate::json::parse_java_double(t).is_some() {
                Err(AggError::illegal_argument(format!("failed to parse [{s}], fractional time values are not supported"))
                    .caused_by(AggError::number_format(t)))
            } else {
                Err(AggError::illegal_argument(format!("failed to parse [{s}]")).caused_by(AggError::number_format(t)))
            }
        }
    }
}

/// A [`Rounding`] prepared for the values of a known range ([`Rounding::prepare`]).
#[derive(Clone, Debug)]
pub struct PreparedRounding {
    max: i64,
    /// Rounding points, ascending; `points[0] <= min`.
    points: Vec<i64>,
}

impl PreparedRounding {
    /// Largest number of buckets an array covers (Elasticsearch prepares arrays of up to 128
    /// points; a few more cost one extra step of the search).
    pub const MAX_POINTS: usize = 256;

    /// The bucket index of `v`, or `None` outside the prepared range.
    #[inline]
    pub fn index(&self, v: i64) -> Option<usize> {
        let p = &self.points;
        if v < p[0] || v > self.max {
            return None;
        }
        // Branch-free lower search: the comparison becomes a conditional move.
        let (mut lo, mut n) = (0usize, p.len());
        while n > 1 {
            let half = n / 2;
            lo = if p[lo + half] <= v { lo + half } else { lo };
            n -= half;
        }
        Some(lo)
    }

    /// Whether `v` is in bucket `i`.
    #[inline]
    pub fn holds(&self, i: usize, v: i64) -> bool {
        v >= self.points[i] && self.points.get(i + 1).map_or(v <= self.max, |&next| v < next)
    }

    /// The rounding point (bucket key) of bucket `i`.
    #[inline]
    pub fn point(&self, i: usize) -> i64 {
        self.points[i]
    }

    /// Number of buckets.
    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }
}

/// What a rounding rounds to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundingKind {
    Unit(DateUnit),
    /// A fixed interval in millis.
    Interval(i64),
}

/// A date rounding.
#[derive(Clone, Debug)]
pub struct Rounding {
    pub kind: RoundingKind,
    pub zone: TimeZone,
    pub offset: i64,
    rules: Option<Arc<ZoneRules>>,
}

impl PartialEq for Rounding {
    fn eq(&self, o: &Self) -> bool {
        self.kind == o.kind && self.zone == o.zone && self.offset == o.offset
    }
}

impl Rounding {
    pub fn new(kind: RoundingKind, zone: TimeZone, offset: i64) -> Result<Rounding> {
        if let RoundingKind::Interval(i) = kind
            && i < 1
        {
            return Err(AggError::illegal_argument("Zero or negative time interval not supported"));
        }
        let rules = match &zone {
            TimeZone::Named(tz) => Some(ZoneRules::of(*tz)),
            TimeZone::Fixed { .. } => None,
        };
        Ok(Rounding { kind, zone, offset, rules })
    }

    /// `withoutOffset()`.
    pub fn without_offset(&self) -> Rounding {
        Rounding { offset: 0, ..self.clone() }
    }

    /// Rounds an instant down.
    #[inline]
    pub fn round(&self, utc: i64) -> i64 {
        self.round_inner(utc.wrapping_sub(self.offset)).wrapping_add(self.offset)
    }

    /// `Rounding.round(v)`, which is `prepare(v, v).round(v)`: how `extended_bounds` and
    /// `hard_bounds` are rounded. Preparing looks up the zone's offsets over a window around
    /// `v` computed in wrapping `long` arithmetic, and `LocalTimeOffset.lookup` refuses a
    /// window whose ends crossed ("[min] must be <= [max]", e.g. for `Long.MIN_VALUE`); units
    /// that round to midnight then step to the next rounding value through `java.time`, whose
    /// exact arithmetic fails past `Long.MAX_VALUE` ("long overflow").
    pub fn round_bound(&self, v: i64) -> Result<i64> {
        let v = v.wrapping_sub(self.offset);
        let (min_lookup, max_lookup) = match self.kind {
            RoundingKind::Unit(u) => {
                let max = if u.to_midnight() { v } else { v.wrapping_add(2 * u.unit_millis()) };
                (v.wrapping_sub(2 * u.extra_local_offset_lookup()), max)
            }
            RoundingKind::Interval(i) => (v.wrapping_sub(i), v.wrapping_add(i)),
        };
        if min_lookup > max_lookup {
            return Err(AggError::illegal_argument(format!("[{min_lookup}] must be <= [{max_lookup}]")));
        }
        let rounded = self.round_inner(v);
        if let RoundingKind::Unit(u) = self.kind
            && u.to_midnight()
            && self.next_overflows(u, rounded, v)
        {
            return Err(AggError::new(500, "arithmetic_exception", "long overflow"));
        }
        Ok(rounded.wrapping_add(self.offset))
    }

    /// `maybeUseArray(v, v)` of a prepared to-midnight rounding: from `round(v)`, step with
    /// `java.time`'s `nextRoundingValue` (true calendar truncation, then one unit) while the
    /// step stays `<= v`; whether a step leaves the range of epoch millis.
    fn next_overflows(&self, u: DateUnit, rounded: i64, v: i64) -> bool {
        let mut r = rounded as i128;
        for _ in 0..128 {
            let Ok(ri) = i64::try_from(r) else { return true };
            let off = match self.fixed_offset_ms() {
                Some(o) => o,
                None => self.rules.as_ref().map_or(0, |z| z.offset_at(ri) as i64 * 1000),
            } as i128;
            let local = r + off;
            let next_local = match u {
                DateUnit::Day => local.div_euclid(DAY as i128) * DAY as i128 + DAY as i128,
                DateUnit::Week => {
                    let (d3, w) = (3 * DAY as i128, 7 * DAY as i128);
                    (local + d3).div_euclid(w) * w - d3 + w
                }
                _ => {
                    let Ok(l) = i64::try_from(local) else { return true };
                    let (y, m, _) = ymd(l);
                    let add = match u {
                        DateUnit::Month => 1,
                        DateUnit::Quarter => 3 - (m as i64 - 1) % 3,
                        _ => 13 - m as i64,
                    };
                    let total = y * 12 + (m as i64 - 1) + add;
                    days_of(total.div_euclid(12), (total.rem_euclid(12) + 1) as u32, 1) as i128 * DAY as i128
                }
            };
            let next = next_local - off;
            if next > i64::MAX as i128 || next < i64::MIN as i128 {
                return true;
            }
            if next > v as i128 || next <= r {
                return false;
            }
            r = next;
        }
        false
    }

    /// The rounding of the values in `[min, max]` as a sorted array of its rounding points
    /// (Elasticsearch's `ArrayRounding` from `prepare(min, max)`): a value rounds to the
    /// largest point not after it, a branch-free binary search instead of calendar arithmetic
    /// per value. `None` when the range spans more than [`PreparedRounding::MAX_POINTS`]
    /// buckets, or when a point fails the self-check below.
    ///
    /// Each point is checked to round to itself and the instant before it to round to the
    /// previous point; rounding is monotonic, so every value between two points then rounds
    /// to the lower one, and the array answers exactly what [`round`](Self::round) would.
    pub fn prepare(&self, min: i64, max: i64) -> Option<PreparedRounding> {
        if min > max {
            return None;
        }
        let first = self.round(min);
        if first > min {
            return None;
        }
        let mut points = Vec::with_capacity(16);
        points.push(first);
        loop {
            let last = *points.last()?;
            let next = self.next_rounding_value(last);
            if next <= last || next == i64::MAX {
                return None;
            }
            if next > max {
                break;
            }
            if points.len() == PreparedRounding::MAX_POINTS || self.round(next) != next || self.round(next - 1) != last {
                return None;
            }
            points.push(next);
        }
        Some(PreparedRounding { max, points })
    }

    /// The next rounded value after the bucket containing `utc` (`nextRoundingValue`).
    pub fn next_rounding_value(&self, utc: i64) -> i64 {
        self.next_inner(utc.wrapping_sub(self.offset)).wrapping_add(self.offset)
    }

    fn fixed_offset_ms(&self) -> Option<i64> {
        match &self.zone {
            TimeZone::Fixed { offset_secs, .. } => Some(*offset_secs as i64 * 1000),
            TimeZone::Named(_) => None,
        }
    }

    #[inline]
    fn round_inner(&self, utc: i64) -> i64 {
        if let Some(off) = self.fixed_offset_ms() {
            let local = utc.wrapping_add(off);
            return match self.kind {
                RoundingKind::Unit(u) => u.floor_local(local).wrapping_sub(off),
                RoundingKind::Interval(i) => round_key(local, i).wrapping_mul(i).wrapping_sub(off),
            };
        }
        let rules = self.rules.as_ref().expect("named zone rules");
        match self.kind {
            RoundingKind::Unit(u) if u.to_midnight() => {
                let local = utc.wrapping_add(rules.offset_at(utc) as i64 * 1000);
                rules.first_time_on_day(u.floor_local(local))
            }
            RoundingKind::Unit(u) => self.round_not_to_midnight(rules, u, utc),
            RoundingKind::Interval(i) => self.round_interval(rules, i, utc),
        }
    }

    fn round_not_to_midnight(&self, rules: &ZoneRules, u: DateUnit, utc: i64) -> i64 {
        let mut instant = utc;
        for _ in 0..10_000 {
            let truncated = truncate_as_local_time(rules, u, instant);
            let prev = rules.previous_transition(instant);
            match prev {
                None => return truncated.unwrap_or(instant),
                Some(p) => {
                    if let Some(t) = truncated
                        && p <= t
                    {
                        return t;
                    }
                    instant = p.wrapping_sub(1);
                }
            }
        }
        instant
    }

    fn round_interval(&self, rules: &ZoneRules, interval: i64, original: i64) -> i64 {
        let mut utc = original;
        let mut attempts = 0;
        'attempt: while attempts < 5000 {
            let local = utc.wrapping_add(rules.offset_at(utc) as i64 * 1000);
            let rounded_local = round_key(local, interval).wrapping_mul(interval);
            let offsets = rules.valid_offsets(rounded_local);
            if !offsets.is_empty() {
                let prev = rules.previous_transition(utc.wrapping_add(1));
                for &o in offsets.iter().rev() {
                    let inst = rounded_local.wrapping_sub(o as i64 * 1000);
                    if let Some(p) = prev
                        && inst < p
                    {
                        attempts += 1;
                        utc = p.wrapping_sub(1);
                        continue 'attempt;
                    }
                    if utc >= inst {
                        return inst;
                    }
                }
                return rounded_local.wrapping_sub(offsets[0] as i64 * 1000);
            } else {
                return rules.gap_transition(rounded_local);
            }
        }
        utc
    }

    fn next_inner(&self, utc: i64) -> i64 {
        match self.kind {
            RoundingKind::Unit(u) if u.to_midnight() => match self.fixed_offset_ms() {
                Some(off) => u.next_local(u.floor_local(utc.wrapping_add(off))).wrapping_sub(off),
                None => {
                    let rules = self.rules.as_ref().expect("rules");
                    let local = utc.wrapping_add(rules.offset_at(utc) as i64 * 1000);
                    rules.first_time_on_day(u.next_local(u.floor_local(local)))
                }
            },
            RoundingKind::Unit(u) => {
                let um = u.unit_millis();
                let r1 = self.round_inner(utc.wrapping_add(um));
                if utc < r1 { r1 } else { self.round_inner(utc.wrapping_add(2 * um)) }
            }
            RoundingKind::Interval(i) => {
                if let Some(off) = self.fixed_offset_ms() {
                    return round_key(utc.wrapping_add(off), i).wrapping_add(1).wrapping_mul(i).wrapping_sub(off);
                }
                // Elasticsearch's search for exactly the next rounding value.
                let prev_round = self.round_inner(utc);
                let mut increment = i;
                let mut from = prev_round;
                for _ in 0..100 {
                    from = from.wrapping_add(increment);
                    let rounded = self.round_inner(from);
                    if rounded <= prev_round {
                        if increment < 0 {
                            increment = -increment / 2;
                        }
                        continue;
                    }
                    let rr = self.round_inner(rounded.wrapping_sub(1));
                    if rr > prev_round {
                        if increment > 0 {
                            increment = -increment / 2;
                        }
                        continue;
                    }
                    return rounded;
                }
                self.round_inner(from)
            }
        }
    }

    /// `roundingSize(utcMillis, unit)` (rate aggregation).
    pub fn rounding_size_at(&self, utc: i64, unit: DateUnit) -> Result<f64> {
        match self.kind {
            RoundingKind::Unit(u) => {
                if u.is_millis_based() {
                    if unit.is_millis_based() {
                        Ok(u.ratio() as f64 / unit.ratio() as f64)
                    } else {
                        Err(AggError::illegal_argument(format!(
                            "Cannot use month-based rate unit [{}] with non-month based calendar interval histogram [{}] only week, day, hour, minute and second are supported for this histogram",
                            unit.short_name(),
                            u.short_name()
                        )))
                    }
                } else if unit.is_millis_based() {
                    // `PreparedOffsetRounding.roundingSize` hands the (offset) bucket key to the
                    // rounding without the offset.
                    Ok((self.next_inner(utc) - utc) as f64 / unit.ratio() as f64)
                } else {
                    Ok(u.ratio() as f64 / unit.ratio() as f64)
                }
            }
            RoundingKind::Interval(i) => {
                if unit.is_millis_based() {
                    Ok(i as f64 / unit.ratio() as f64)
                } else {
                    Err(AggError::illegal_argument(format!(
                        "Cannot use month-based rate unit [{}] with fixed interval based histogram, only week, day, hour, minute and second are supported for this histogram",
                        unit.short_name()
                    )))
                }
            }
        }
    }

    /// `roundingSize(unit)` without an instant (rate over a whole histogram).
    pub fn rounding_size(&self, unit: DateUnit) -> Result<f64> {
        match self.kind {
            RoundingKind::Unit(u) if !u.is_millis_based() && unit.is_millis_based() => Err(AggError::illegal_argument(format!(
                "Cannot use non month-based rate unit [{}] with calendar interval histogram [{}] only month, quarter and year are supported for this histogram",
                unit.short_name(),
                u.short_name()
            ))),
            _ => self.rounding_size_at(0, unit),
        }
    }
}

/// `truncateAsLocalTime` of `JavaTimeNotToMidnightRounding`.
fn truncate_as_local_time(rules: &ZoneRules, u: DateUnit, instant: i64) -> Option<i64> {
    let local = instant.wrapping_add(rules.offset_at(instant) as i64 * 1000);
    let t = u.floor_local(local);
    let offsets = rules.valid_offsets(t);
    for &o in offsets.iter().rev() {
        let r = t.wrapping_sub(o as i64 * 1000);
        if r <= instant {
            return Some(r);
        }
    }
    None
}

/// One offset transition.
#[derive(Clone, Copy, Debug)]
struct Transition {
    /// Epoch millis (a whole second).
    at: i64,
    before: i32,
    after: i32,
}

/// The offset transitions of a region zone.
#[derive(Debug)]
pub struct ZoneRules {
    tz: chrono_tz::Tz,
    transitions: Vec<Transition>,
    /// Offset before the first transition.
    initial: i32,
    lo: i64,
    hi: i64,
}

fn tz_offset(tz: &chrono_tz::Tz, utc_ms: i64) -> i32 {
    match chrono::DateTime::from_timestamp(utc_ms.div_euclid(1000), 0) {
        Some(dt) => tz.offset_from_utc_datetime(&dt.naive_utc()).fix().local_minus_utc(),
        None => 0,
    }
}

impl ZoneRules {
    /// The (cached) rules of a zone.
    pub fn of(tz: chrono_tz::Tz) -> Arc<ZoneRules> {
        static CACHE: OnceLock<Mutex<HashMap<chrono_tz::Tz, Arc<ZoneRules>>>> = OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        if let Some(r) = cache.lock().expect("zone cache").get(&tz) {
            return r.clone();
        }
        let r = Arc::new(ZoneRules::build(tz));
        cache.lock().expect("zone cache").insert(tz, r.clone());
        r
    }

    fn build(tz: chrono_tz::Tz) -> ZoneRules {
        // 1850-01-01 .. 2150-01-01
        let lo = -3_786_825_600_000i64;
        let hi = 5_680_281_600_000i64;
        let step = 6 * 3_600_000i64;
        let mut transitions = Vec::new();
        let initial = tz_offset(&tz, lo);
        let mut prev_t = lo;
        let mut prev_off = initial;
        let mut t = lo + step;
        while t <= hi {
            let off = tz_offset(&tz, t);
            if off != prev_off {
                // Bisect to the second between prev_t (prev_off) and t (off).
                let (mut a, mut b) = (prev_t / 1000, t / 1000);
                while b - a > 1 {
                    let m = a + (b - a) / 2;
                    if tz_offset(&tz, m * 1000) == prev_off {
                        a = m;
                    } else {
                        b = m;
                    }
                }
                transitions.push(Transition { at: b * 1000, before: prev_off, after: tz_offset(&tz, b * 1000) });
                prev_off = off;
            }
            prev_t = t;
            t += step;
        }
        ZoneRules { tz, transitions, initial, lo, hi }
    }

    /// Whether two zones have the same offsets at every instant (`ZoneRules.equals`): a link
    /// (`Canada/Mountain`) and its zone (`America/Edmonton`) do.
    pub fn same_rules(&self, other: &ZoneRules) -> bool {
        self.initial == other.initial
            && self.transitions.len() == other.transitions.len()
            && self.transitions.iter().zip(&other.transitions).all(|(a, b)| a.at == b.at && a.before == b.before && a.after == b.after)
    }

    /// Offset (seconds) at an instant.
    #[inline]
    pub fn offset_at(&self, utc: i64) -> i32 {
        if utc < self.lo || utc > self.hi {
            return tz_offset(&self.tz, utc);
        }
        let i = self.transitions.partition_point(|t| t.at <= utc);
        if i == 0 { self.initial } else { self.transitions[i - 1].after }
    }

    /// `ZoneRules.getValidOffsets(localDateTime)`: offsets valid at a local time, earliest
    /// instant first (the pre-transition offset of an overlap first).
    pub fn valid_offsets(&self, local: i64) -> smallvec::SmallVec<[i32; 2]> {
        let mut out: smallvec::SmallVec<[i32; 2]> = smallvec::SmallVec::new();
        // Candidate offsets: those in effect around the local time.
        let mut cands: smallvec::SmallVec<[i32; 4]> = smallvec::SmallVec::new();
        for probe in [local.wrapping_sub(2 * DAY), local.wrapping_sub(DAY), local, local.wrapping_add(DAY), local.wrapping_add(2 * DAY)] {
            let o = self.offset_at(probe);
            if !cands.contains(&o) {
                cands.push(o);
            }
        }
        for &o in &cands {
            let inst = local.wrapping_sub(o as i64 * 1000);
            if self.offset_at(inst) == o && !out.contains(&o) {
                out.push(o);
            }
        }
        // Earliest instant first = largest offset first.
        out.sort_by(|a, b| b.cmp(a));
        out
    }

    /// The transition whose gap contains a local time (`getTransition(localDateTime)`).
    pub fn gap_transition(&self, local: i64) -> i64 {
        for t in &self.transitions {
            let start = t.at + t.before as i64 * 1000;
            let end = t.at + t.after as i64 * 1000;
            if t.after > t.before && local >= start && local < end {
                return t.at;
            }
        }
        // Outside the cached range: the local time minus the offset after.
        local.wrapping_sub(self.offset_at(local) as i64 * 1000)
    }

    /// `ZoneRules.previousTransition(instant)`: the latest transition strictly before the
    /// instant rounded up to a whole second.
    pub fn previous_transition(&self, utc: i64) -> Option<i64> {
        let secs = utc.div_euclid(1000) + if utc.rem_euclid(1000) > 0 { 1 } else { 0 };
        let ms = secs.saturating_mul(1000);
        let i = self.transitions.partition_point(|t| t.at < ms);
        if i == 0 { None } else { Some(self.transitions[i - 1].at) }
    }

    /// `firstTimeOnDay`: the first instant of a local midnight (the transition if the midnight
    /// falls into a gap).
    pub fn first_time_on_day(&self, local_mid: i64) -> i64 {
        let offs = self.valid_offsets(local_mid);
        match offs.first() {
            Some(&o) => local_mid.wrapping_sub(o as i64 * 1000),
            None => self.gap_transition(local_mid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(s: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis()
    }

    #[test]
    fn utc_units() {
        let r = Rounding::new(RoundingKind::Unit(DateUnit::Month), TimeZone::utc(), 0).unwrap();
        assert_eq!(r.round(ms("2020-02-15T10:00:00Z")), ms("2020-02-01T00:00:00Z"));
        assert_eq!(r.next_rounding_value(ms("2020-12-15T10:00:00Z")), ms("2021-01-01T00:00:00Z"));
        let w = Rounding::new(RoundingKind::Unit(DateUnit::Week), TimeZone::utc(), 0).unwrap();
        // 2020-01-01 is a Wednesday; weeks start on Monday.
        assert_eq!(w.round(ms("2020-01-01T10:00:00Z")), ms("2019-12-30T00:00:00Z"));
        let q = Rounding::new(RoundingKind::Unit(DateUnit::Quarter), TimeZone::utc(), 0).unwrap();
        assert_eq!(q.round(ms("2020-05-15T10:00:00Z")), ms("2020-04-01T00:00:00Z"));
        let f = Rounding::new(RoundingKind::Interval(3 * 3_600_000), TimeZone::utc(), 0).unwrap();
        assert_eq!(f.round(ms("2020-01-01T10:30:00Z")), ms("2020-01-01T09:00:00Z"));
        assert_eq!(f.round(-1), -3 * 3_600_000);
        let o = Rounding::new(RoundingKind::Unit(DateUnit::Day), TimeZone::utc(), 3_600_000).unwrap();
        assert_eq!(o.round(ms("2020-01-01T00:30:00Z")), ms("2019-12-31T01:00:00Z"));
    }

    #[test]
    fn dst_zones() {
        let paris = TimeZone::parse("Europe/Paris").unwrap();
        let d = Rounding::new(RoundingKind::Unit(DateUnit::Day), paris.clone(), 0).unwrap();
        // The day of the spring transition has 23 hours.
        let start = d.round(ms("2020-03-29T12:00:00Z"));
        assert_eq!(start, ms("2020-03-28T23:00:00Z"));
        assert_eq!(d.next_rounding_value(start), ms("2020-03-29T22:00:00Z"));
        let h = Rounding::new(RoundingKind::Unit(DateUnit::Hour), paris.clone(), 0).unwrap();
        assert_eq!(h.round(ms("2020-03-29T01:30:00Z")), ms("2020-03-29T01:00:00Z"));
        assert_eq!(h.round(ms("2020-10-25T01:30:00Z")), ms("2020-10-25T01:00:00Z"));
        assert_eq!(h.round(ms("2020-10-25T00:30:00Z")), ms("2020-10-25T00:00:00Z"));
        // Midnight skipped (America/Sao_Paulo, 2018-11-04 00:00 → 01:00).
        let sp = TimeZone::parse("America/Sao_Paulo").unwrap();
        let d = Rounding::new(RoundingKind::Unit(DateUnit::Day), sp, 0).unwrap();
        assert_eq!(d.round(ms("2018-11-04T12:00:00Z")), ms("2018-11-04T03:00:00Z"));
    }

    /// A prepared rounding answers exactly what `round` does, for every unit and fixed
    /// interval, in fixed-offset zones and across DST transitions (including 30-minute and
    /// skipped-midnight zones), at range edges and on bucket boundaries.
    #[test]
    fn prepared_matches_round() {
        use rand::{Rng, SeedableRng, rngs::StdRng};
        let mut rng = StdRng::seed_from_u64(11);
        let zones = ["UTC", "+05:30", "-08:00", "Europe/Paris", "America/Sao_Paulo", "Australia/Lord_Howe", "America/St_Johns", "Asia/Kathmandu"];
        let kinds = [
            RoundingKind::Unit(DateUnit::Second),
            RoundingKind::Unit(DateUnit::Minute),
            RoundingKind::Unit(DateUnit::Hour),
            RoundingKind::Unit(DateUnit::Day),
            RoundingKind::Unit(DateUnit::Week),
            RoundingKind::Unit(DateUnit::Month),
            RoundingKind::Unit(DateUnit::Quarter),
            RoundingKind::Unit(DateUnit::Year),
            RoundingKind::Interval(90 * 60_000),
            RoundingKind::Interval(DAY),
            RoundingKind::Interval(7 * DAY + 1),
        ];
        let mut prepared = 0;
        for z in zones {
            let tz = TimeZone::parse(z).unwrap();
            for kind in kinds {
                for offset in [0, 3_600_000, -DAY / 3] {
                    let r = Rounding::new(kind, tz.clone(), offset).unwrap();
                    for _ in 0..6 {
                        // Ranges around DST-rich years, sized so most kinds fit an array.
                        let span = match kind {
                            RoundingKind::Unit(DateUnit::Second) => 100_000,
                            RoundingKind::Unit(DateUnit::Minute) => 6_000_000,
                            RoundingKind::Unit(DateUnit::Hour) | RoundingKind::Interval(_) => 8 * DAY,
                            RoundingKind::Unit(DateUnit::Day) | RoundingKind::Unit(DateUnit::Week) => 200 * DAY,
                            _ => 20 * 365 * DAY,
                        };
                        let min = ms("2018-01-01T00:00:00Z") + rng.gen_range(0..3 * 365 * DAY);
                        let max = min + rng.gen_range(0..span);
                        let Some(p) = r.prepare(min, max) else { continue };
                        prepared += 1;
                        let check = |v: i64| {
                            let i = p.index(v).unwrap_or_else(|| panic!("{z} {kind:?} {v} outside [{min}, {max}]"));
                            assert_eq!(p.point(i), r.round(v), "{z} {kind:?} offset {offset} value {v}");
                        };
                        check(min);
                        check(max);
                        for i in 1..p.len() {
                            check(p.point(i));
                            check(p.point(i) - 1);
                        }
                        for _ in 0..300 {
                            check(rng.gen_range(min..=max));
                        }
                        assert_eq!(p.index(p.point(0) - 1), None);
                        assert_eq!(p.index(max + 1), None);
                    }
                }
            }
        }
        assert!(prepared > 1000, "{prepared}");
        // Too many buckets, or an empty range: no array.
        let s = Rounding::new(RoundingKind::Unit(DateUnit::Second), TimeZone::utc(), 0).unwrap();
        assert!(s.prepare(0, 1_000_000).is_none());
        assert!(s.prepare(10, 9).is_none());
        assert!(s.prepare(i64::MIN, i64::MIN + 5).is_none() || s.prepare(i64::MIN, i64::MIN + 5).is_some_and(|p| p.point(0) == s.round(i64::MIN)));
    }

    #[test]
    fn time_values() {
        assert_eq!(parse_time_value_millis("90m", "x").unwrap(), 5_400_000);
        assert_eq!(parse_time_value_millis("1d", "x").unwrap(), DAY);
        assert_eq!(
            parse_time_value_millis("1M", "date_histogram.fixedInterval").unwrap_err().reason,
            "failed to parse setting [date_histogram.fixedInterval] with value [1M] as a time value: unit is missing or unrecognized"
        );
        assert_eq!(parse_time_value_millis("1.5h", "x").unwrap_err().reason, "failed to parse [1.5h], fractional time values are not supported");
        assert_eq!(parse_time_value_millis("-1", "x").unwrap(), -1);
    }
}
