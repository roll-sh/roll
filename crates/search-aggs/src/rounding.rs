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

use chrono::{Datelike, NaiveDate, Offset, TimeZone as _};

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
            DateUnit::Week => round_floor(local + 3 * DAY, 7 * DAY) - 3 * DAY,
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
            DateUnit::Day => local_mid + DAY,
            DateUnit::Week => local_mid + 7 * DAY,
            DateUnit::Month | DateUnit::Quarter | DateUnit::Year => {
                let (y, m, _) = ymd(local_mid);
                let add = match self {
                    DateUnit::Month => 1,
                    DateUnit::Quarter => 3,
                    _ => 12,
                };
                let total = y as i64 * 12 + (m as i64 - 1) + add;
                millis_of(total.div_euclid(12) as i32, (total.rem_euclid(12) + 1) as u32, 1)
            }
            _ => local_mid + self.ratio(),
        }
    }
}

/// `DateUtils.roundFloor`.
#[inline]
pub fn round_floor(v: i64, unit: i64) -> i64 {
    v - v.rem_euclid(unit)
}

/// `TimeIntervalRounding.roundKey`.
#[inline]
fn round_key(v: i64, interval: i64) -> i64 {
    v.div_euclid(interval)
}

fn ymd(local: i64) -> (i32, u32, u32) {
    let days = local.div_euclid(DAY);
    let d = NaiveDate::from_num_days_from_ce_opt((days + 719_163) as i32).unwrap_or(NaiveDate::MIN);
    (d.year(), d.month(), d.day())
}

fn millis_of(y: i32, m: u32, d: u32) -> i64 {
    match NaiveDate::from_ymd_opt(y, m, d) {
        Some(nd) => (nd.num_days_from_ce() as i64 - 719_163) * DAY,
        None => 0,
    }
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

    /// The next rounded value after the bucket containing `utc` (`nextRoundingValue`).
    pub fn next_rounding_value(&self, utc: i64) -> i64 {
        self.next_inner(utc - self.offset) + self.offset
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
            let local = utc + off;
            return match self.kind {
                RoundingKind::Unit(u) => u.floor_local(local) - off,
                RoundingKind::Interval(i) => round_key(local, i) * i - off,
            };
        }
        let rules = self.rules.as_ref().expect("named zone rules");
        match self.kind {
            RoundingKind::Unit(u) if u.to_midnight() => {
                let local = utc + rules.offset_at(utc) as i64 * 1000;
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
                    instant = p - 1;
                }
            }
        }
        instant
    }

    fn round_interval(&self, rules: &ZoneRules, interval: i64, original: i64) -> i64 {
        let mut utc = original;
        let mut attempts = 0;
        'attempt: while attempts < 5000 {
            let local = utc + rules.offset_at(utc) as i64 * 1000;
            let rounded_local = round_key(local, interval) * interval;
            let offsets = rules.valid_offsets(rounded_local);
            if !offsets.is_empty() {
                let prev = rules.previous_transition(utc + 1);
                for &o in offsets.iter().rev() {
                    let inst = rounded_local - o as i64 * 1000;
                    if let Some(p) = prev
                        && inst < p
                    {
                        attempts += 1;
                        utc = p - 1;
                        continue 'attempt;
                    }
                    if utc >= inst {
                        return inst;
                    }
                }
                return rounded_local - offsets[0] as i64 * 1000;
            } else {
                return rules.gap_transition(rounded_local);
            }
        }
        utc
    }

    fn next_inner(&self, utc: i64) -> i64 {
        match self.kind {
            RoundingKind::Unit(u) if u.to_midnight() => match self.fixed_offset_ms() {
                Some(off) => u.next_local(u.floor_local(utc + off)) - off,
                None => {
                    let rules = self.rules.as_ref().expect("rules");
                    let local = utc + rules.offset_at(utc) as i64 * 1000;
                    rules.first_time_on_day(u.next_local(u.floor_local(local)))
                }
            },
            RoundingKind::Unit(u) => {
                let um = u.unit_millis();
                let r1 = self.round_inner(utc + um);
                if utc < r1 { r1 } else { self.round_inner(utc + 2 * um) }
            }
            RoundingKind::Interval(i) => {
                if let Some(off) = self.fixed_offset_ms() {
                    return (round_key(utc + off, i) + 1) * i - off;
                }
                // Elasticsearch's search for exactly the next rounding value.
                let prev_round = self.round_inner(utc);
                let mut increment = i;
                let mut from = prev_round;
                for _ in 0..100 {
                    from += increment;
                    let rounded = self.round_inner(from);
                    if rounded <= prev_round {
                        if increment < 0 {
                            increment = -increment / 2;
                        }
                        continue;
                    }
                    let rr = self.round_inner(rounded - 1);
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
    let local = instant + rules.offset_at(instant) as i64 * 1000;
    let t = u.floor_local(local);
    let offsets = rules.valid_offsets(t);
    for &o in offsets.iter().rev() {
        let r = t - o as i64 * 1000;
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
        for probe in [local - 2 * DAY, local - DAY, local, local + DAY, local + 2 * DAY] {
            let o = self.offset_at(probe);
            if !cands.contains(&o) {
                cands.push(o);
            }
        }
        for &o in &cands {
            let inst = local - o as i64 * 1000;
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
        local - self.offset_at(local) as i64 * 1000
    }

    /// `ZoneRules.previousTransition(instant)`: the latest transition strictly before the
    /// instant rounded up to a whole second.
    pub fn previous_transition(&self, utc: i64) -> Option<i64> {
        let secs = utc.div_euclid(1000) + if utc.rem_euclid(1000) > 0 { 1 } else { 0 };
        let ms = secs * 1000;
        let i = self.transitions.partition_point(|t| t.at < ms);
        if i == 0 { None } else { Some(self.transitions[i - 1].at) }
    }

    /// `firstTimeOnDay`: the first instant of a local midnight (the transition if the midnight
    /// falls into a gap).
    pub fn first_time_on_day(&self, local_mid: i64) -> i64 {
        let offs = self.valid_offsets(local_mid);
        match offs.first() {
            Some(&o) => local_mid - o as i64 * 1000,
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
