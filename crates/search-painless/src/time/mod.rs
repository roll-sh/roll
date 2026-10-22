//! `java.time` (and `java.util.Date`): dates, times, instants, durations, periods, zones and
//! the temporal field/unit machinery behind them.
//!
//! Values are small `Copy`-ish structs over Java's own representation — a proleptic Gregorian
//! date (`year`, `month`, `day`), a nano-of-day time, offsets in seconds — with calendar math of
//! our own, so the full Java range (±999,999,999 years) and Java's exact semantics (end-of-month
//! clamping in `plusMonths`, gap/overlap resolution in zones, `until` truncation, `toString`
//! layouts) hold. Region zones get their rules from `chrono-tz` (the IANA tzdb), consulted only
//! to map instants to offsets and local times to valid offsets.
//!
//! Most of the allowlisted API reduces to a handful of generic operations — read a
//! [`Field`], set a field, add an amount of a [`Unit`], measure `until` in a unit — which is how
//! `java.time` itself is built; the native methods registered here are thin adapters over them.

pub mod format;
pub mod locale;
pub mod rules;
pub mod builder;
pub mod weeks;
pub mod legacy;
pub mod legacy_fmt;

use std::sync::Arc;

use chrono::{Offset, TimeZone};

use crate::lib_lang::{int, long};
use crate::lookup::Builder;
use crate::types::{cls, ClassId};
use crate::value::{Obj, Value};
use crate::vm::{npe, Throw, Thrown, Vm, VR};

// ------------------------------------------------------------------------------ calendar

pub const NANOS_PER_SECOND: i64 = 1_000_000_000;
pub const NANOS_PER_DAY: i64 = 86_400 * NANOS_PER_SECOND;
pub const SECONDS_PER_DAY: i64 = 86_400;
pub const MAX_YEAR: i64 = 999_999_999;
pub const MIN_YEAR: i64 = -999_999_999;
/// Epoch day of 0000-03-01 relative to 1970-01-01 (Java's `DAYS_0000_TO_1970`).
const DAYS_0000_TO_1970: i64 = 146097 * 5 - (30 * 365 + 7);

pub fn is_leap(y: i64) -> bool {
    (y & 3) == 0 && (y % 100 != 0 || y % 400 == 0)
}

pub fn month_len(y: i64, m: u8) -> u8 {
    match m {
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// A proleptic Gregorian date.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord, Hash)]
pub struct Date {
    pub y: i32,
    pub m: u8,
    pub d: u8,
}

impl Date {
    /// `LocalDate.toEpochDay`.
    pub fn epoch_day(self) -> i64 {
        let y = self.y as i64;
        let m = self.m as i64;
        let mut total = 365 * y;
        if y >= 0 {
            total += (y + 3) / 4 - (y + 99) / 100 + (y + 399) / 400;
        } else {
            total -= y / -4 - y / -100 + y / -400;
        }
        total += (367 * m - 362) / 12;
        total += self.d as i64 - 1;
        if m > 2 {
            total -= 1;
            if !is_leap(y) {
                total -= 1;
            }
        }
        total - DAYS_0000_TO_1970
    }

    /// `LocalDate.ofEpochDay`.
    pub fn from_epoch_day(ed: i64) -> Date {
        let mut zero_day = ed + DAYS_0000_TO_1970;
        zero_day -= 60;
        let mut adjust = 0;
        if zero_day < 0 {
            let adjust_cycles = (zero_day + 1) / 146097 - 1;
            adjust = adjust_cycles * 400;
            zero_day += -adjust_cycles * 146097;
        }
        let mut year_est = (400 * zero_day + 591) / 146097;
        let mut doy_est = zero_day - (365 * year_est + year_est / 4 - year_est / 100 + year_est / 400);
        if doy_est < 0 {
            year_est -= 1;
            doy_est = zero_day - (365 * year_est + year_est / 4 - year_est / 100 + year_est / 400);
        }
        year_est += adjust;
        let march_doy0 = doy_est;
        let march_month0 = (march_doy0 * 5 + 2) / 153;
        let month = (march_month0 + 2) % 12 + 1;
        let dom = march_doy0 - (march_month0 * 306 + 5) / 10 + 1;
        year_est += march_month0 / 10;
        Date { y: year_est as i32, m: month as u8, d: dom as u8 }
    }

    pub fn day_of_week(self) -> u8 {
        (self.epoch_day() + 3).rem_euclid(7) as u8 + 1
    }

    pub fn day_of_year(self) -> u16 {
        let mut doy = 0u16;
        for m in 1..self.m {
            doy += month_len(self.y as i64, m) as u16;
        }
        doy + self.d as u16
    }

    pub fn len_of_year(self) -> u16 {
        if is_leap(self.y as i64) {
            366
        } else {
            365
        }
    }

    /// `LocalDate.of` with Java's validation messages.
    pub fn of(y: i64, m: i64, d: i64) -> Result<Date, Thrown> {
        check(Field::Year, y)?;
        check(Field::MonthOfYear, m)?;
        check(Field::DayOfMonth, d)?;
        if d > 28 && d > month_len(y, m as u8) as i64 {
            if d == 29 {
                return Err(jt(dte(format!("Invalid date 'February 29' as '{y}' is not a leap year")), &["LocalDate.create(LocalDate.java:477)"]));
            }
            return Err(jt(dte(format!("Invalid date '{} {d}'", month_name(m as u8))), &["LocalDate.create(LocalDate.java:479)"]));
        }
        Ok(Date { y: y as i32, m: m as u8, d: d as u8 })
    }

    /// `resolvePreviousValid`: clamps the day to the month's length.
    pub fn clamp(y: i64, m: u8, d: u8) -> Date {
        let ml = month_len(y, m);
        Date { y: y as i32, m, d: d.min(ml) }
    }

    pub fn of_year_day(y: i64, doy: i64) -> Result<Date, Thrown> {
        check(Field::Year, y)?;
        check(Field::DayOfYear, doy)?;
        let leap = is_leap(y);
        if doy == 366 && !leap {
            return Err(dte(format!("Invalid date 'DayOfYear 366' as '{y}' is not a leap year")));
        }
        let mut m = 1u8;
        let mut rem = doy;
        loop {
            let ml = month_len(y, m) as i64;
            if rem <= ml {
                break;
            }
            rem -= ml;
            m += 1;
        }
        Ok(Date { y: y as i32, m, d: rem as u8 })
    }

    pub fn plus_months(self, n: i64) -> Result<Date, Thrown> {
        if n == 0 {
            return Ok(self);
        }
        let month_count = self.y as i64 * 12 + (self.m as i64 - 1);
        let calc = month_count.checked_add(n).ok_or_else(|| arith("long overflow"))?;
        let ny = calc.div_euclid(12);
        check(Field::Year, ny)?;
        let nm = (calc.rem_euclid(12) + 1) as u8;
        Ok(Date::clamp(ny, nm, self.d))
    }

    pub fn plus_years(self, n: i64) -> Result<Date, Thrown> {
        if n == 0 {
            return Ok(self);
        }
        let ny = (self.y as i64).checked_add(n).ok_or_else(|| arith("long overflow"))?;
        check(Field::Year, ny)?;
        Ok(Date::clamp(ny, self.m, self.d))
    }

    pub fn plus_days(self, n: i64) -> Result<Date, Thrown> {
        if n == 0 {
            return Ok(self);
        }
        // the JDK's fast path within the month and into the next one (which is also where
        // stepping past `LocalDate.MAX` fails, on the year rather than the epoch day)
        if let Some(dom) = (self.d as i64).checked_add(n)
            && dom > 0
            && dom <= 59
        {
            let len = month_len(self.y as i64, self.m) as i64;
            if dom <= len {
                return Ok(Date { d: dom as u8, ..self });
            }
            if self.m < 12 {
                return Ok(Date { m: self.m + 1, d: (dom - len) as u8, ..self });
            }
            check(Field::Year, self.y as i64 + 1)?;
            return Ok(Date { y: self.y + 1, m: 1, d: (dom - len) as u8 });
        }
        let ed = self.epoch_day().checked_add(n).ok_or_else(|| arith("long overflow"))?;
        check(Field::EpochDay, ed)?;
        Ok(Date::from_epoch_day(ed))
    }

    fn proleptic_month(self) -> i64 {
        self.y as i64 * 12 + self.m as i64 - 1
    }

    /// `LocalDate.until(end, MONTHS)` style month difference.
    fn months_until(self, end: Date) -> i64 {
        let p1 = self.proleptic_month() * 32 + self.d as i64;
        let p2 = end.proleptic_month() * 32 + end.d as i64;
        (p2 - p1) / 32
    }
}

/// Nanosecond of day.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord, Hash)]
pub struct Time(pub i64);

impl Time {
    pub fn hour(self) -> i64 {
        self.0 / (3600 * NANOS_PER_SECOND)
    }
    pub fn minute(self) -> i64 {
        (self.0 / (60 * NANOS_PER_SECOND)) % 60
    }
    pub fn second(self) -> i64 {
        (self.0 / NANOS_PER_SECOND) % 60
    }
    pub fn nano(self) -> i64 {
        self.0 % NANOS_PER_SECOND
    }
    pub fn second_of_day(self) -> i64 {
        self.0 / NANOS_PER_SECOND
    }
    pub fn of(h: i64, m: i64, s: i64, n: i64) -> Result<Time, Thrown> {
        check(Field::HourOfDay, h)?;
        check(Field::MinuteOfHour, m)?;
        check(Field::SecondOfMinute, s)?;
        check(Field::NanoOfSecond, n)?;
        Ok(Time(((h * 60 + m) * 60 + s) * NANOS_PER_SECOND + n))
    }
}

pub fn month_name(m: u8) -> &'static str {
    ["JANUARY", "FEBRUARY", "MARCH", "APRIL", "MAY", "JUNE", "JULY", "AUGUST", "SEPTEMBER", "OCTOBER", "NOVEMBER", "DECEMBER"][(m as usize).clamp(1, 12) - 1]
}

pub fn dow_name(d: u8) -> &'static str {
    ["MONDAY", "TUESDAY", "WEDNESDAY", "THURSDAY", "FRIDAY", "SATURDAY", "SUNDAY"][(d as usize).clamp(1, 7) - 1]
}

// ------------------------------------------------------------------------------ errors

pub fn dte(msg: impl Into<String>) -> Thrown {
    Throw::new(cls::DATE_TIME_EXCEPTION, Some(msg.into()))
}

fn arith(msg: &str) -> Thrown {
    Throw::new(cls::ARITHMETIC_EXCEPTION, Some(msg.to_string()))
}

fn unsupported(msg: String) -> Thrown {
    Throw::new(cls::UNSUPPORTED_TEMPORAL_TYPE_EXCEPTION, Some(msg))
}

/// Appends JDK frames (`java.base/java.time.<frame>`): the callers of the failing code,
/// innermost first.
pub(crate) fn jt(mut t: Thrown, frames: &[&str]) -> Thrown {
    for f in frames {
        if f.starts_with("java.lang.") {
            t.frames.push(format!("java.base/{f}"));
        } else {
            t.frames.push(format!("java.base/java.time.{f}"));
        }
    }
    t
}

/// `DateTimeFormatter.parse(text, query)`'s frames for a failure of the formatter.
pub(crate) fn parse_frames(t: Thrown) -> Thrown {
    let msg = t.message.clone().unwrap_or_default();
    if t.class != cls::DATE_TIME_PARSE_EXCEPTION {
        return t;
    }
    if msg.contains("could not be parsed at index") {
        jt(t, &["format.DateTimeFormatter.parseResolved0(DateTimeFormatter.java:2131)", "format.DateTimeFormatter.parse(DateTimeFormatter.java:2033)"])
    } else if msg.contains(", unparsed text found") {
        jt(t, &["format.DateTimeFormatter.parseResolved0(DateTimeFormatter.java:2134)", "format.DateTimeFormatter.parse(DateTimeFormatter.java:2033)"])
    } else {
        jt(t, &["format.DateTimeFormatter.createError(DateTimeFormatter.java:2102)", "format.DateTimeFormatter.parse(DateTimeFormatter.java:2037)"])
    }
}

pub fn parse_error(text: &str, index: usize, why: &str) -> Thrown {
    let msg = if why.is_empty() {
        format!("Text '{text}' could not be parsed at index {index}")
    } else {
        format!("Text '{text}' could not be parsed{why}")
    };
    Throw::new(cls::DATE_TIME_PARSE_EXCEPTION, Some(msg))
}

// ------------------------------------------------------------------------------ fields & units

macro_rules! fields {
    ($($v:ident = $name:literal, $lo:expr, $hi:expr;)*) => {
        /// `java.time.temporal.ChronoField`, in Java's declaration order.
        #[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
        pub enum Field { $($v,)* }
        impl Field {
            pub const ALL: &'static [Field] = &[$(Field::$v,)*];
            /// `toString()`: the display name.
            pub fn name(self) -> &'static str { match self { $(Field::$v => $name,)* } }
            pub fn range(self) -> (i64, i64) { match self { $(Field::$v => ($lo, $hi),)* } }
        }
    };
}

fields! {
    NanoOfSecond = "NanoOfSecond", 0, 999_999_999;
    NanoOfDay = "NanoOfDay", 0, 86_400 * 1_000_000_000 - 1;
    MicroOfSecond = "MicroOfSecond", 0, 999_999;
    MicroOfDay = "MicroOfDay", 0, 86_400 * 1_000_000 - 1;
    MilliOfSecond = "MilliOfSecond", 0, 999;
    MilliOfDay = "MilliOfDay", 0, 86_400 * 1000 - 1;
    SecondOfMinute = "SecondOfMinute", 0, 59;
    SecondOfDay = "SecondOfDay", 0, 86_400 - 1;
    MinuteOfHour = "MinuteOfHour", 0, 59;
    MinuteOfDay = "MinuteOfDay", 0, (24 * 60) - 1;
    HourOfAmPm = "HourOfAmPm", 0, 11;
    ClockHourOfAmPm = "ClockHourOfAmPm", 1, 12;
    HourOfDay = "HourOfDay", 0, 23;
    ClockHourOfDay = "ClockHourOfDay", 1, 24;
    AmPmOfDay = "AmPmOfDay", 0, 1;
    DayOfWeek = "DayOfWeek", 1, 7;
    AlignedDayOfWeekInMonth = "AlignedDayOfWeekInMonth", 1, 7;
    AlignedDayOfWeekInYear = "AlignedDayOfWeekInYear", 1, 7;
    DayOfMonth = "DayOfMonth", 1, 31;
    DayOfYear = "DayOfYear", 1, 366;
    EpochDay = "EpochDay", -365_243_219_162, 365_241_780_471;
    AlignedWeekOfMonth = "AlignedWeekOfMonth", 1, 5;
    AlignedWeekOfYear = "AlignedWeekOfYear", 1, 53;
    MonthOfYear = "MonthOfYear", 1, 12;
    ProlepticMonth = "ProlepticMonth", MIN_YEAR * 12, MAX_YEAR * 12 + 11;
    YearOfEra = "YearOfEra", 1, MAX_YEAR + 1;
    Year = "Year", MIN_YEAR, MAX_YEAR;
    Era = "Era", 0, 1;
    InstantSeconds = "InstantSeconds", i64::MIN, i64::MAX;
    OffsetSeconds = "OffsetSeconds", -18 * 3600, 18 * 3600;
}

impl Field {
    pub fn is_time_based(self) -> bool {
        (self as u8) < (Field::DayOfWeek as u8)
    }
    pub fn is_date_based(self) -> bool {
        (self as u8) >= (Field::DayOfWeek as u8) && (self as u8) <= (Field::Era as u8)
    }
    /// `ChronoField.name()`: the constant name.
    pub fn const_name(self) -> String {
        let mut out = String::new();
        for (i, c) in self.name().chars().enumerate() {
            if c.is_ascii_uppercase() && i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_uppercase());
        }
        match self {
            Field::HourOfAmPm => "HOUR_OF_AMPM".into(),
            Field::ClockHourOfAmPm => "CLOCK_HOUR_OF_AMPM".into(),
            Field::AmPmOfDay => "AMPM_OF_DAY".into(),
            _ => out,
        }
    }
    pub fn base_unit(self) -> Unit {
        use Field::*;
        match self {
            NanoOfSecond | NanoOfDay => Unit::Nanos,
            MicroOfSecond | MicroOfDay => Unit::Micros,
            MilliOfSecond | MilliOfDay => Unit::Millis,
            SecondOfMinute | SecondOfDay | InstantSeconds | OffsetSeconds => Unit::Seconds,
            MinuteOfHour | MinuteOfDay => Unit::Minutes,
            HourOfAmPm | ClockHourOfAmPm | HourOfDay | ClockHourOfDay => Unit::Hours,
            AmPmOfDay => Unit::HalfDays,
            DayOfWeek | AlignedDayOfWeekInMonth | AlignedDayOfWeekInYear | DayOfMonth | DayOfYear | EpochDay => Unit::Days,
            AlignedWeekOfMonth | AlignedWeekOfYear => Unit::Weeks,
            MonthOfYear | ProlepticMonth => Unit::Months,
            YearOfEra | Year => Unit::Years,
            Era => Unit::Eras,
        }
    }
    pub fn range_unit(self) -> Unit {
        use Field::*;
        match self {
            NanoOfSecond | MicroOfSecond | MilliOfSecond => Unit::Seconds,
            NanoOfDay | MicroOfDay | MilliOfDay | SecondOfDay | MinuteOfDay | HourOfDay | ClockHourOfDay | AmPmOfDay => Unit::Days,
            SecondOfMinute => Unit::Minutes,
            MinuteOfHour => Unit::Hours,
            HourOfAmPm | ClockHourOfAmPm => Unit::HalfDays,
            DayOfWeek | AlignedDayOfWeekInMonth | AlignedDayOfWeekInYear => Unit::Weeks,
            DayOfMonth | AlignedWeekOfMonth => Unit::Months,
            DayOfYear | AlignedWeekOfYear | MonthOfYear => Unit::Years,
            YearOfEra => Unit::Eras,
            EpochDay | ProlepticMonth | Year | Era | InstantSeconds | OffsetSeconds => Unit::Forever,
        }
    }
}

/// `ValueRange.checkValidValue` with Java's message.
pub fn check(f: Field, v: i64) -> Result<i64, Thrown> {
    let (lo, hi) = f.range();
    if v < lo || v > hi {
        let range = match f {
            Field::DayOfMonth => "1 - 28/31".to_string(),
            Field::DayOfYear => "1 - 365/366".to_string(),
            Field::AlignedWeekOfMonth => "1 - 4/5".to_string(),
            Field::YearOfEra => format!("1 - {}/{}", MAX_YEAR, MAX_YEAR + 1),
            _ => format!("{lo} - {hi}"),
        };
        let t = dte(format!("Invalid value for {} (valid values {range}): {v}", f.name()));
        return Err(jt(t, &["temporal.ValueRange.checkValidValue(ValueRange.java:319)", "temporal.ChronoField.checkValidValue(ChronoField.java:721)"]));
    }
    Ok(v)
}

/// `java.time.temporal.ChronoUnit`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum Unit {
    Nanos,
    Micros,
    Millis,
    Seconds,
    Minutes,
    Hours,
    HalfDays,
    Days,
    Weeks,
    Months,
    Years,
    Decades,
    Centuries,
    Millennia,
    Eras,
    Forever,
}

impl Unit {
    pub const ALL: &'static [Unit] = &[
        Unit::Nanos,
        Unit::Micros,
        Unit::Millis,
        Unit::Seconds,
        Unit::Minutes,
        Unit::Hours,
        Unit::HalfDays,
        Unit::Days,
        Unit::Weeks,
        Unit::Months,
        Unit::Years,
        Unit::Decades,
        Unit::Centuries,
        Unit::Millennia,
        Unit::Eras,
        Unit::Forever,
    ];
    pub fn name(self) -> &'static str {
        use Unit::*;
        match self {
            Nanos => "Nanos",
            Micros => "Micros",
            Millis => "Millis",
            Seconds => "Seconds",
            Minutes => "Minutes",
            Hours => "Hours",
            HalfDays => "HalfDays",
            Days => "Days",
            Weeks => "Weeks",
            Months => "Months",
            Years => "Years",
            Decades => "Decades",
            Centuries => "Centuries",
            Millennia => "Millennia",
            Eras => "Eras",
            Forever => "Forever",
        }
    }
    pub fn const_name(self) -> &'static str {
        use Unit::*;
        match self {
            Nanos => "NANOS",
            Micros => "MICROS",
            Millis => "MILLIS",
            Seconds => "SECONDS",
            Minutes => "MINUTES",
            Hours => "HOURS",
            HalfDays => "HALF_DAYS",
            Days => "DAYS",
            Weeks => "WEEKS",
            Months => "MONTHS",
            Years => "YEARS",
            Decades => "DECADES",
            Centuries => "CENTURIES",
            Millennia => "MILLENNIA",
            Eras => "ERAS",
            Forever => "FOREVER",
        }
    }
    /// Exact length in nanoseconds for time-based units (and the estimate for date units).
    pub fn duration(self) -> (i64, i64) {
        use Unit::*;
        match self {
            Nanos => (0, 1),
            Micros => (0, 1000),
            Millis => (0, 1_000_000),
            Seconds => (1, 0),
            Minutes => (60, 0),
            Hours => (3600, 0),
            HalfDays => (43200, 0),
            Days => (86400, 0),
            Weeks => (7 * 86400, 0),
            Months => (31556952 / 12, 0),
            Years => (31556952, 0),
            Decades => (31556952 * 10, 0),
            Centuries => (31556952 * 100, 0),
            Millennia => (31556952 * 1000, 0),
            Eras => (31556952 * 1_000_000_000, 0),
            Forever => (i64::MAX, 999_999_999),
        }
    }
    pub fn is_time_based(self) -> bool {
        self < Unit::Days
    }
    pub fn is_date_based(self) -> bool {
        self >= Unit::Days && self != Unit::Forever
    }
    pub fn nanos(self) -> i64 {
        let (s, n) = self.duration();
        s.saturating_mul(NANOS_PER_SECOND).saturating_add(n)
    }
}

// ------------------------------------------------------------------------------ zones

/// A zone: a fixed offset or a region with rules.
#[derive(Clone, Debug)]
pub enum Zone {
    Offset(i32),
    /// Region id and rules (`None` for the fixed UTC-like regions `UTC`, `GMT`, `UT`, possibly
    /// with an offset suffix, whose rules are the fixed offset).
    Region(Arc<str>, Option<chrono_tz::Tz>, i32),
}

impl PartialEq for Zone {
    fn eq(&self, o: &Zone) -> bool {
        match (self, o) {
            (Zone::Offset(a), Zone::Offset(b)) => a == b,
            (Zone::Region(a, ..), Zone::Region(b, ..)) => a == b,
            _ => false,
        }
    }
}

/// The first second of 2100: `chrono-tz` lists explicit transitions up to 2099 and holds the
/// last offset afterwards, where the JDK keeps applying the zone's last rules.
const TZ_DATA_END: i64 = 4_102_444_800;

/// A year in 2072–2099 whose calendar matches `y`'s (same leap-ness and weekday of January 1st),
/// so the weekday-based rules in force at the end of the tz data place transitions on the same
/// dates in both.
fn rule_year(y: i32) -> i32 {
    let jan1 = |y: i32| Date { y, m: 1, d: 1 }.day_of_week();
    (2072..=2099).find(|&x| is_leap(x as i64) == is_leap(y as i64) && jan1(x) == jan1(y)).unwrap_or(2099)
}

/// The UTC date-time the tz data is queried at for an instant: itself, or, from 2100 on, the
/// same date and time of the matching rule year.
fn tz_probe_utc(epoch_sec: i64) -> chrono::NaiveDateTime {
    let s = epoch_sec.clamp(-8_000_000_000_000, 8_000_000_000_000);
    let dt = chrono::DateTime::from_timestamp(s, 0).unwrap_or_default().naive_utc();
    if epoch_sec < TZ_DATA_END {
        return dt;
    }
    use chrono::Datelike;
    dt.with_year(rule_year(dt.year())).unwrap_or(dt)
}

/// [`tz_probe_utc`] for a local date-time.
fn tz_probe_local(nd: chrono::NaiveDateTime) -> chrono::NaiveDateTime {
    use chrono::Datelike;
    if nd.year() < 2100 {
        return nd;
    }
    nd.with_year(rule_year(nd.year())).unwrap_or(nd)
}

/// Total and standard offsets of `tz` at an instant, with standard and daylight time as the
/// JDK compiles the tz data ("rearguard"): a negative daylight saving (Europe/Dublin's winter
/// GMT, Morocco's Ramadan time) becomes standard time with positive daylight saving at the
/// other times of the year.
pub fn tz_offsets(tz: &chrono_tz::Tz, epoch_sec: i64) -> (i32, i32) {
    use chrono::{Offset as _, TimeZone as _};
    use chrono_tz::OffsetComponents;
    let at = |probe: chrono::NaiveDateTime| {
        let o = tz.offset_from_utc_datetime(&probe);
        (o.fix().local_minus_utc(), o.base_utc_offset().num_seconds() as i32, o.dst_offset().num_seconds() as i32)
    };
    let probe = tz_probe_utc(epoch_sec);
    let (total, base, dst) = at(probe);
    if dst < 0 {
        return (total, total);
    }
    // negative daylight saving around this instant moves the standard offset down
    for k in [-1i64, 1, -2, 2, -3, 3, -4, 4, -5, 5, -6, 6, -7, 7, -8, 8, -9, 9, -10, 10, -11, 11, -12, 12] {
        let p = probe + chrono::Duration::days(30 * k);
        let (_, b, d) = at(p);
        if d < 0 && b == base {
            return (total, base + d);
        }
    }
    (total, base)
}

impl Zone {
    pub fn utc() -> Zone {
        Zone::Offset(0)
    }
    pub fn id(&self) -> String {
        match self {
            Zone::Offset(o) => offset_id(*o),
            Zone::Region(id, ..) => id.to_string(),
        }
    }
    /// Offset in effect at an instant (epoch seconds).
    pub fn offset_at(&self, epoch_sec: i64) -> i32 {
        match self {
            Zone::Offset(o) => *o,
            Zone::Region(_, None, o) => *o,
            Zone::Region(_, Some(tz), _) => tz.offset_from_utc_datetime(&tz_probe_utc(epoch_sec)).fix().local_minus_utc(),
        }
    }
    /// Valid offsets for a local date-time: one (normal), two (overlap: earlier first) or none
    /// (gap).
    pub(crate) fn valid_offsets(&self, d: Date, t: Time) -> Vec<i32> {
        match self {
            Zone::Offset(o) => vec![*o],
            Zone::Region(_, None, o) => vec![*o],
            Zone::Region(_, Some(tz), _) => {
                let Some(nd) = chrono::NaiveDate::from_ymd_opt(d.y.clamp(-200_000, 200_000), d.m as u32, d.d as u32) else {
                    return vec![self.offset_at(0)];
                };
                let nt = chrono::NaiveTime::from_num_seconds_from_midnight_opt(t.second_of_day() as u32, t.nano() as u32).unwrap_or_default();
                match tz.offset_from_local_datetime(&tz_probe_local(nd.and_time(nt))) {
                    chrono::LocalResult::Single(o) => vec![o.fix().local_minus_utc()],
                    chrono::LocalResult::Ambiguous(a, b) => {
                        let (a, b) = (a.fix().local_minus_utc(), b.fix().local_minus_utc());
                        // the earlier instant has the larger offset
                        if a >= b {
                            vec![a, b]
                        } else {
                            vec![b, a]
                        }
                    }
                    chrono::LocalResult::None => Vec::new(),
                }
            }
        }
    }
    pub fn is_fixed(&self) -> bool {
        matches!(self, Zone::Offset(_) | Zone::Region(_, None, _))
    }
}

/// `ZoneOffset.getId()`.
pub fn offset_id(total: i32) -> String {
    if total == 0 {
        return "Z".into();
    }
    let abs = total.unsigned_abs();
    let (h, m, s) = (abs / 3600, (abs / 60) % 60, abs % 60);
    let sign = if total < 0 { '-' } else { '+' };
    if s != 0 {
        format!("{sign}{h:02}:{m:02}:{s:02}")
    } else {
        format!("{sign}{h:02}:{m:02}")
    }
}

/// `ZoneOffset.of(String)`.
pub fn parse_offset(s: &str) -> Result<i32, Thrown> {
    let bad = || dte(format!("Invalid ID for ZoneOffset, invalid format: {s}"));
    if s == "Z" {
        return Ok(0);
    }
    let b = s.as_bytes();
    if b.is_empty() || (b[0] != b'+' && b[0] != b'-') {
        return Err(bad());
    }
    let digits = |i: usize, n: usize| -> Option<i32> {
        let p = s.get(i..i + n)?;
        if p.bytes().all(|c| c.is_ascii_digit()) {
            p.parse().ok()
        } else {
            None
        }
    };
    let (h, m, sec) = match s.len() {
        2 => (digits(1, 1).ok_or_else(bad)?, 0, 0),
        3 => (digits(1, 2).ok_or_else(bad)?, 0, 0),
        5 => (digits(1, 2).ok_or_else(bad)?, digits(3, 2).ok_or_else(bad)?, 0),
        6 if b[3] == b':' => (digits(1, 2).ok_or_else(bad)?, digits(4, 2).ok_or_else(bad)?, 0),
        7 => (digits(1, 2).ok_or_else(bad)?, digits(3, 2).ok_or_else(bad)?, digits(5, 2).ok_or_else(bad)?),
        9 if b[3] == b':' && b[6] == b':' => (digits(1, 2).ok_or_else(bad)?, digits(4, 2).ok_or_else(bad)?, digits(7, 2).ok_or_else(bad)?),
        _ => return Err(bad()),
    };
    let neg = b[0] == b'-';
    offset_hms(if neg { -h } else { h }, if neg { -m } else { m }, if neg { -sec } else { sec })
}

/// `ZoneOffset.ofHoursMinutesSeconds` validation.
pub fn offset_hms(h: i32, m: i32, s: i32) -> Result<i32, Thrown> {
    if !(-18..=18).contains(&h) {
        return Err(jt(dte(format!("Zone offset hours not in valid range: value {h} is not in the range -18 to 18")), &["ZoneOffset.validate(ZoneOffset.java:383)"]));
    }
    if h > 0 && (m < 0 || s < 0) || h < 0 && (m > 0 || s > 0) || h == 0 && (m > 0 && s < 0 || m < 0 && s > 0) {
        return Err(dte("Zone offset minutes and seconds must have the same sign"));
    }
    if !(-59..=59).contains(&m) {
        return Err(dte(format!("Zone offset minutes not in valid range: value {m} is not in the range -59 to 59")));
    }
    if !(-59..=59).contains(&s) {
        return Err(dte(format!("Zone offset seconds not in valid range: value {s} is not in the range -59 to 59")));
    }
    if h.abs() == 18 && (m.abs() > 0 || s.abs() > 0) {
        return Err(dte("Zone offset not in valid range: -18:00 to +18:00"));
    }
    Ok(h * 3600 + m * 60 + s)
}

/// `ZoneId.of(String)`.
pub fn zone_of(id: &str) -> Result<Zone, Thrown> {
    if id.len() <= 1 || id.starts_with('+') || id.starts_with('-') {
        return parse_offset(id).map(Zone::Offset);
    }
    for prefix in ["UTC", "GMT", "UT"] {
        if let Some(rest) = id.strip_prefix(prefix) {
            if rest.is_empty() {
                return Ok(Zone::Region(Arc::from(prefix), None, 0));
            }
            if rest.starts_with('+') || rest.starts_with('-') {
                let off = parse_offset(rest).map_err(|_| dte(format!("Invalid ID for offset-based ZoneId: {id}")))?;
                let rid = if off == 0 { prefix.to_string() } else { format!("{prefix}{}", offset_id(off)) };
                return Ok(Zone::Region(Arc::from(rid.as_str()), None, off));
            }
        }
    }
    let valid = id.bytes().enumerate().all(|(i, c)| c.is_ascii_alphanumeric() || (i > 0 && matches!(c, b'~' | b'/' | b'.' | b'_' | b'+' | b'-')));
    if !valid || !id.as_bytes()[0].is_ascii_alphabetic() {
        let t = dte(format!("Invalid ID for region-based ZoneId, invalid format: {id}"));
        return Err(jt(t, &["ZoneRegion.checkName(ZoneRegion.java:152)", "ZoneRegion.ofId(ZoneRegion.java:117)"]));
    }
    match id.parse::<chrono_tz::Tz>() {
        Ok(tz) => Ok(Zone::Region(Arc::from(id), Some(tz), 0)),
        Err(_) => {
            let t = Throw::new(cls::ZONE_RULES_EXCEPTION, Some(format!("Unknown time-zone ID: {id}")));
            Err(jt(
                t,
                &[
                    "zone.ZoneRulesProvider.getProvider(ZoneRulesProvider.java:272)",
                    "zone.ZoneRulesProvider.getRules(ZoneRulesProvider.java:227)",
                    "ZoneRegion.ofId(ZoneRegion.java:121)",
                ],
            ))
        }
    }
}

// ------------------------------------------------------------------------------ values

/// A `java.time` value (or `java.util.Date`).
#[derive(Clone, Debug)]
pub enum TimeVal {
    Instant { secs: i64, nanos: i32 },
    Duration { secs: i64, nanos: i32 },
    Period { y: i32, m: i32, d: i32 },
    LocalDate(Date),
    LocalTime(Time),
    LocalDateTime(Date, Time),
    Zoned { d: Date, t: Time, offset: i32, zone: Zone },
    OffsetDateTime { d: Date, t: Time, offset: i32 },
    OffsetTime { t: Time, offset: i32 },
    ZoneId(Zone),
    DayOfWeek(u8),
    Month(u8),
    Year(i32),
    YearMonth(i32, u8),
    MonthDay(u8, u8),
    TUnit(Unit),
    TField(Field),
    /// `IsoFields`, `JulianFields` and `WeekFields` fields.
    XField(weeks::XField),
    /// `IsoFields.QUARTER_YEARS` / `WEEK_BASED_YEARS`.
    XUnit(weeks::XUnit),
    /// A `WeekFields` definition.
    WeekFields(weeks::WeekDef),
    /// `TextStyle`, `ResolverStyle`, `FormatStyle`, `SignStyle` constants by ordinal.
    Enum(ClassId, u8),
    /// `ValueRange`.
    Range { min_s: i64, min_l: i64, max_s: i64, max_l: i64 },
    /// `Clock.fixed` / `Clock.systemUTC`: fixed instant (or `None` for the system clock) and zone.
    Clock { fixed: Option<(i64, i32)>, zone: Zone },
    /// `java.util.Date` (epoch millis).
    UtilDate(std::cell::Cell<i64>),
    /// `TemporalAdjusters.*`.
    Adjuster(Adjuster),
    /// `DateTimeFormatter.parse(CharSequence)`'s `java.time.format.Parsed`.
    Parsed(std::rc::Rc<format::Parsed>),
    /// `ZoneId.getRules()`.
    Rules(Zone),
    /// `ZoneOffsetTransition`: epoch second and the offsets before and after.
    Transition { epoch: i64, before: i32, after: i32 },
    /// A `DateTimeFormatterBuilder` (mutable, shared by reference).
    FmtBuilder(std::rc::Rc<std::cell::RefCell<builder::FmtBuilder>>),
}

#[derive(Clone, Debug)]
pub enum Adjuster {
    FirstDayOfMonth,
    LastDayOfMonth,
    FirstDayOfNextMonth,
    FirstDayOfYear,
    LastDayOfYear,
    FirstDayOfNextYear,
    FirstInMonth(u8),
    LastInMonth(u8),
    DayOfWeekInMonth(i32, u8),
    Next(u8),
    NextOrSame(u8),
    Previous(u8),
    PreviousOrSame(u8),
}

/// A constant of a simple enum held as [`TimeVal::Enum`] (named by `enum_const_name`).
pub fn enum_value(c: ClassId, ordinal: u8) -> Value {
    time_value(TimeVal::Enum(c, ordinal))
}

/// The ordinal of such an enum constant.
pub fn enum_ordinal(v: &Value) -> Option<u8> {
    match v {
        Value::Obj(o) => match &**o {
            crate::value::Obj::Time(TimeVal::Enum(_, ord)) => Some(*ord),
            _ => None,
        },
        _ => None,
    }
}

pub fn time_value(t: TimeVal) -> Value {
    Value::obj(Obj::Time(t))
}

/// `ZonedDateTime` in UTC from epoch milliseconds (doc values of `date` fields).
pub fn zoned_utc_millis(ms: i64) -> Value {
    zoned_utc_nanos_parts(ms.div_euclid(1000), (ms.rem_euclid(1000) * 1_000_000) as i32)
}

/// `ZonedDateTime` in UTC from epoch nanoseconds (`date_nanos`).
pub fn zoned_utc_nanos(ns: i64) -> Value {
    zoned_utc_nanos_parts(ns.div_euclid(NANOS_PER_SECOND), ns.rem_euclid(NANOS_PER_SECOND) as i32)
}

fn zoned_utc_nanos_parts(secs: i64, nanos: i32) -> Value {
    let (d, t) = local_of_epoch(secs, nanos, 0);
    time_value(TimeVal::Zoned { d, t, offset: 0, zone: Zone::Region(Arc::from("Z"), None, 0) }.utc_zone())
}

impl TimeVal {
    /// Normalizes a zoned value's zone to `ZoneOffset.UTC` (`Z`), what doc values use.
    fn utc_zone(self) -> TimeVal {
        match self {
            TimeVal::Zoned { d, t, offset, .. } => TimeVal::Zoned { d, t, offset, zone: Zone::Offset(0) },
            o => o,
        }
    }
}

/// Epoch millis of a temporal value with an instant (`toEpochMilli` augmentation).
pub fn epoch_millis(v: &Value) -> Option<i64> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Time(t) => {
                let (s, n) = t.instant()?;
                Some(s * 1000 + (n as i64) / 1_000_000)
            }
            _ => None,
        },
        _ => None,
    }
}

/// Local date-time of an instant at an offset.
pub fn local_of_epoch(secs: i64, nanos: i32, offset: i32) -> (Date, Time) {
    let local = secs + offset as i64;
    let ed = local.div_euclid(SECONDS_PER_DAY);
    let sod = local.rem_euclid(SECONDS_PER_DAY);
    (Date::from_epoch_day(ed), Time(sod * NANOS_PER_SECOND + nanos as i64))
}

fn epoch_of_local(d: Date, t: Time, offset: i32) -> i64 {
    d.epoch_day() * SECONDS_PER_DAY + t.second_of_day() - offset as i64
}

/// `ZonedDateTime.ofLocal`: resolves a local date-time in a zone, preferring `preferred` in an
/// overlap and shifting forward by the gap length in a gap.
pub fn resolve_local(d: Date, t: Time, zone: &Zone, preferred: Option<i32>) -> (Date, Time, i32) {
    let offs = zone.valid_offsets(d, t);
    match offs.len() {
        1 => (d, t, offs[0]),
        2 => {
            let o = match preferred {
                Some(p) if offs.contains(&p) => p,
                _ => offs[0],
            };
            (d, t, o)
        }
        _ => {
            // gap: offset before the transition, then the one after it
            let approx = epoch_of_local(d, t, 0) - SECONDS_PER_DAY;
            let before = zone.offset_at(approx);
            let after = zone.offset_at(epoch_of_local(d, t, before));
            let shift = (after - before) as i64;
            let (nd, nt) = add_local_seconds(d, t, shift);
            (nd, nt, after)
        }
    }
}

fn add_local_seconds(d: Date, t: Time, secs: i64) -> (Date, Time) {
    let total = t.0 + secs * NANOS_PER_SECOND;
    let days = total.div_euclid(NANOS_PER_DAY);
    let nod = total.rem_euclid(NANOS_PER_DAY);
    (Date::from_epoch_day(d.epoch_day() + days), Time(nod))
}

fn zoned(d: Date, t: Time, zone: Zone, preferred: Option<i32>) -> TimeVal {
    let (d, t, offset) = resolve_local(d, t, &zone, preferred);
    TimeVal::Zoned { d, t, offset, zone }
}

fn zoned_of_instant(secs: i64, nanos: i32, zone: Zone) -> TimeVal {
    let offset = zone.offset_at(secs);
    let (d, t) = local_of_epoch(secs, nanos, offset);
    TimeVal::Zoned { d, t, offset, zone }
}

impl TimeVal {
    pub fn class(&self) -> ClassId {
        match self {
            TimeVal::Instant { .. } => cls::INSTANT,
            TimeVal::Duration { .. } => cls::DURATION,
            TimeVal::Period { .. } => cls::PERIOD,
            TimeVal::LocalDate(_) => cls::LOCAL_DATE,
            TimeVal::LocalTime(_) => cls::LOCAL_TIME,
            TimeVal::LocalDateTime(..) => cls::LOCAL_DATE_TIME,
            TimeVal::Zoned { .. } => cls::ZONED_DATE_TIME,
            TimeVal::OffsetDateTime { .. } => cls::OFFSET_DATE_TIME,
            TimeVal::OffsetTime { .. } => cls::OFFSET_TIME,
            TimeVal::ZoneId(Zone::Offset(_)) => cls::ZONE_OFFSET,
            TimeVal::ZoneId(_) => cls::ZONE_REGION,
            TimeVal::DayOfWeek(_) => cls::DAY_OF_WEEK,
            TimeVal::Month(_) => cls::MONTH,
            TimeVal::Year(_) => cls::YEAR,
            TimeVal::YearMonth(..) => cls::YEAR_MONTH,
            TimeVal::MonthDay(..) => cls::MONTH_DAY,
            TimeVal::TUnit(_) => cls::CHRONO_UNIT,
            TimeVal::TField(_) => cls::CHRONO_FIELD,
            TimeVal::XField(_) => cls::TEMPORAL_FIELD,
            TimeVal::XUnit(_) => cls::TEMPORAL_UNIT,
            TimeVal::WeekFields(_) => cls::WEEK_FIELDS,
            TimeVal::Enum(c, _) => *c,
            TimeVal::Range { .. } => cls::VALUE_RANGE,
            TimeVal::Clock { .. } => cls::CLOCK,
            TimeVal::UtilDate(_) => cls::DATE,
            TimeVal::Adjuster(_) => cls::TEMPORAL_ADJUSTER_IMPL,
            TimeVal::Parsed(_) => cls::TEMPORAL_ACCESSOR,
            TimeVal::Rules(_) => cls::ZONE_RULES,
            TimeVal::Transition { .. } => cls::ZONE_OFFSET_TRANSITION,
            TimeVal::FmtBuilder(_) => cls::DATE_TIME_FORMATTER_BUILDER,
        }
    }

    /// Epoch seconds and nanos when the value denotes an instant.
    pub fn instant(&self) -> Option<(i64, i32)> {
        match self {
            TimeVal::Instant { secs, nanos } => Some((*secs, *nanos)),
            TimeVal::Zoned { d, t, offset, .. } | TimeVal::OffsetDateTime { d, t, offset } => Some((epoch_of_local(*d, *t, *offset), t.nano() as i32)),
            TimeVal::UtilDate(ms) => Some((ms.get().div_euclid(1000), (ms.get().rem_euclid(1000) * 1_000_000) as i32)),
            _ => None,
        }
    }

    pub fn ordinal(&self) -> i32 {
        match self {
            TimeVal::DayOfWeek(d) => *d as i32 - 1,
            TimeVal::Month(m) => *m as i32 - 1,
            TimeVal::TUnit(u) => *u as i32,
            TimeVal::TField(f) => *f as i32,
            TimeVal::Enum(_, o) => *o as i32,
            _ => 0,
        }
    }

    pub fn java_equals(&self, o: &TimeVal) -> bool {
        use TimeVal::*;
        match (self, o) {
            (Instant { secs: a, nanos: b }, Instant { secs: c, nanos: d }) => a == c && b == d,
            (Duration { secs: a, nanos: b }, Duration { secs: c, nanos: d }) => a == c && b == d,
            (Period { y: a, m: b, d: c }, Period { y: x, m: yy, d: z }) => a == x && b == yy && c == z,
            (LocalDate(a), LocalDate(b)) => a == b,
            (LocalTime(a), LocalTime(b)) => a == b,
            (LocalDateTime(a, b), LocalDateTime(c, d)) => a == c && b == d,
            (Zoned { d: a, t: b, offset: c, zone: z1 }, Zoned { d: x, t: y, offset: o2, zone: z2 }) => a == x && b == y && c == o2 && z1 == z2,
            (OffsetDateTime { d: a, t: b, offset: c }, OffsetDateTime { d: x, t: y, offset: z }) => a == x && b == y && c == z,
            (OffsetTime { t: a, offset: b }, OffsetTime { t: c, offset: d }) => a == c && b == d,
            (ZoneId(a), ZoneId(b)) => a == b,
            (DayOfWeek(a), DayOfWeek(b)) | (Month(a), Month(b)) => a == b,
            (Year(a), Year(b)) => a == b,
            (YearMonth(a, b), YearMonth(c, d)) => a == c && b == d,
            (MonthDay(a, b), MonthDay(c, d)) => a == c && b == d,
            (TUnit(a), TUnit(b)) => a == b,
            (TField(a), TField(b)) => a == b,
            (XField(a), XField(b)) => a == b,
            (XUnit(a), XUnit(b)) => a == b,
            (WeekFields(a), WeekFields(b)) => a == b,
            (Enum(a, b), Enum(c, d)) => a == c && b == d,
            (UtilDate(a), UtilDate(b)) => a.get() == b.get(),
            (Rules(a), Rules(b)) => rules::same_rules(a, b),
            (Transition { epoch: a, before: b, after: c }, Transition { epoch: x, before: y, after: z }) => a == x && b == y && c == z,
            (Range { min_s: a, min_l: b, max_s: c, max_l: d }, Range { min_s: e, min_l: f, max_s: g, max_l: h }) => a == e && b == f && c == g && d == h,
            _ => false,
        }
    }

    pub fn java_hash(&self) -> i32 {
        use TimeVal::*;
        let lh = crate::java::long_hash;
        match self {
            Instant { secs, nanos } | Duration { secs, nanos } => lh(*secs) + 51 * *nanos,
            Period { y, m, d } => y.wrapping_add(m.rotate_left(8)).wrapping_add(d.rotate_left(16)),
            LocalDate(d) => date_hash(*d),
            LocalTime(t) => lh(t.0),
            LocalDateTime(d, t) => date_hash(*d) ^ lh(t.0),
            Zoned { d, t, offset, zone } => (date_hash(*d) ^ lh(t.0)) ^ *offset ^ crate::java::string_hash(&zone.id()).rotate_left(3),
            OffsetDateTime { d, t, offset } => (date_hash(*d) ^ lh(t.0)) ^ *offset,
            OffsetTime { t, offset } => lh(t.0) ^ *offset,
            ZoneId(z) => match z {
                Zone::Offset(o) => *o,
                Zone::Region(id, ..) => crate::java::string_hash(id),
            },
            Year(y) => *y,
            YearMonth(y, m) => y ^ ((*m as i32) << 27),
            MonthDay(m, d) => ((*m as i32) << 6) + *d as i32,
            UtilDate(ms) => lh(ms.get()),
            Transition { epoch, before, after } => {
                let (d, t) = local_of_epoch(epoch + *before as i64, 0, 0);
                (date_hash(d) ^ lh(t.0)) ^ *before ^ after.rotate_left(16)
            }
            other => crate::java::string_hash(&other.to_java_string()),
        }
    }

    /// `compareTo` for comparable values of the same class.
    pub fn compare(&self, o: &TimeVal) -> Option<std::cmp::Ordering> {
        use TimeVal::*;
        Some(match (self, o) {
            (Instant { secs: a, nanos: b }, Instant { secs: c, nanos: d }) | (Duration { secs: a, nanos: b }, Duration { secs: c, nanos: d }) => (a, b).cmp(&(c, d)),
            (LocalDate(a), LocalDate(b)) => a.cmp(b),
            (LocalTime(a), LocalTime(b)) => a.cmp(b),
            (LocalDateTime(a, b), LocalDateTime(c, d)) => (a, b).cmp(&(c, d)),
            (Zoned { .. }, Zoned { .. }) | (OffsetDateTime { .. }, OffsetDateTime { .. }) => {
                let (a, b) = (self.instant()?, o.instant()?);
                a.cmp(&b).then_with(|| self.local_dt().cmp(&o.local_dt())).then_with(|| match (self, o) {
                    (Zoned { zone: z1, .. }, Zoned { zone: z2, .. }) => z1.id().cmp(&z2.id()),
                    _ => std::cmp::Ordering::Equal,
                })
            }
            (OffsetTime { t: a, offset: b }, OffsetTime { t: c, offset: d }) => {
                let x = a.0 - *b as i64 * NANOS_PER_SECOND;
                let y = c.0 - *d as i64 * NANOS_PER_SECOND;
                x.cmp(&y).then(a.cmp(c))
            }
            (DayOfWeek(a), DayOfWeek(b)) | (Month(a), Month(b)) => a.cmp(b),
            (Year(a), Year(b)) => a.cmp(b),
            (YearMonth(a, b), YearMonth(c, d)) => (a, b).cmp(&(c, d)),
            (MonthDay(a, b), MonthDay(c, d)) => (a, b).cmp(&(c, d)),
            (TUnit(a), TUnit(b)) => a.cmp(b),
            (TField(a), TField(b)) => (*a as u8).cmp(&(*b as u8)),
            (UtilDate(a), UtilDate(b)) => a.get().cmp(&b.get()),
            (ZoneId(Zone::Offset(a)), ZoneId(Zone::Offset(b))) => b.cmp(a),
            _ => return None,
        })
    }

    fn local_dt(&self) -> Option<(Date, Time)> {
        match self {
            TimeVal::LocalDateTime(d, t) | TimeVal::Zoned { d, t, .. } | TimeVal::OffsetDateTime { d, t, .. } => Some((*d, *t)),
            _ => None,
        }
    }

    pub fn to_java_string(&self) -> String {
        use TimeVal::*;
        match self {
            Parsed(p) => p.describe().trim_end_matches(" of type java.time.format.Parsed").to_string(),
            Rules(z) => rules::rules_string(z),
            Transition { epoch, before, after } => rules::transition_string(*epoch, *before, *after),
            FmtBuilder(b) => format!("java.time.format.DateTimeFormatterBuilder@{:x}", crate::value::identity_hash(&**b) as u32),
            Instant { secs, nanos } => {
                let (d, t) = local_of_epoch(*secs, *nanos, 0);
                let mut s = format_date(d);
                s.push('T');
                s.push_str(&format_time_full(t));
                s.push('Z');
                s
            }
            Duration { secs, nanos } => duration_string(*secs, *nanos),
            Period { y, m, d } => {
                if *y == 0 && *m == 0 && *d == 0 {
                    return "P0D".into();
                }
                let mut s = String::from("P");
                if *y != 0 {
                    s.push_str(&format!("{y}Y"));
                }
                if *m != 0 {
                    s.push_str(&format!("{m}M"));
                }
                if *d != 0 {
                    s.push_str(&format!("{d}D"));
                }
                s
            }
            LocalDate(d) => format_date(*d),
            LocalTime(t) => format_time(*t),
            LocalDateTime(d, t) => format!("{}T{}", format_date(*d), format_time(*t)),
            Zoned { d, t, offset, zone } => {
                let mut s = format!("{}T{}{}", format_date(*d), format_time(*t), offset_id(*offset));
                if let Zone::Region(id, ..) = zone {
                    s.push('[');
                    s.push_str(id);
                    s.push(']');
                }
                s
            }
            OffsetDateTime { d, t, offset } => format!("{}T{}{}", format_date(*d), format_time(*t), offset_id(*offset)),
            OffsetTime { t, offset } => format!("{}{}", format_time(*t), offset_id(*offset)),
            ZoneId(z) => z.id(),
            DayOfWeek(d) => dow_name(*d).into(),
            Month(m) => month_name(*m).into(),
            Year(y) => y.to_string(),
            YearMonth(y, m) => {
                let ys = if y.unsigned_abs() < 1000 {
                    if *y < 0 {
                        format!("-{:04}", y.unsigned_abs())
                    } else {
                        format!("{y:04}")
                    }
                } else if *y > 9999 {
                    format!("+{y}")
                } else {
                    y.to_string()
                };
                format!("{ys}-{m:02}")
            }
            MonthDay(m, d) => format!("--{m:02}-{d:02}"),
            TUnit(u) => u.name().into(),
            TField(f) => f.name().into(),
            XField(f) => f.name(),
            XUnit(u) => u.name().into(),
            WeekFields(w) => w.describe(),
            Enum(c, o) => enum_const_name(*c, *o).into(),
            Range { min_s, min_l, max_s, max_l } => {
                let mut s = min_s.to_string();
                if min_s != min_l {
                    s.push('/');
                    s.push_str(&min_l.to_string());
                }
                s.push_str(" - ");
                s.push_str(&max_s.to_string());
                if max_s != max_l {
                    s.push('/');
                    s.push_str(&max_l.to_string());
                }
                s
            }
            Clock { fixed, zone } => match fixed {
                Some((s, n)) => format!("FixedClock[{},{}]", Instant { secs: *s, nanos: *n }.to_java_string(), zone.id()),
                None => format!("SystemClock[{}]", zone.id()),
            },
            UtilDate(ms) => util_date_string(ms.get()),
            Adjuster(_) => "java.time.temporal.TemporalAdjusters$$Lambda".into(),
        }
    }

    /// How Elasticsearch's XContent renders the value in a script result (dates as
    /// `strict_date_optional_time` strings, other values through `toString`).
    pub fn to_xcontent_string(&self) -> String {
        match self {
            TimeVal::Zoned { d, t, offset, .. } | TimeVal::OffsetDateTime { d, t, offset } => {
                let mut s = format!("{}T{:02}:{:02}:{:02}", format_date(*d), t.hour(), t.minute(), t.second());
                let n = t.nano();
                let frac = if n % 1_000_000 == 0 {
                    format!(".{:03}", n / 1_000_000)
                } else {
                    let mut f = format!("{n:09}");
                    while f.ends_with('0') {
                        f.pop();
                    }
                    format!(".{f}")
                };
                s.push_str(&frac);
                s.push_str(&offset_id(*offset));
                s
            }
            TimeVal::Instant { secs, nanos } => {
                let (d, t) = local_of_epoch(*secs, *nanos, 0);
                TimeVal::Zoned { d, t, offset: 0, zone: Zone::Offset(0) }.to_xcontent_string()
            }
            other => other.to_java_string(),
        }
    }
}

fn date_hash(d: Date) -> i32 {
    let y = d.y;
    (y & 0xFFFFF800u32 as i32) ^ ((y << 11) + ((d.m as i32) << 6) + d.d as i32)
}

fn enum_const_name(c: ClassId, o: u8) -> &'static str {
    match c {
        cls::TEXT_STYLE => ["FULL", "FULL_STANDALONE", "SHORT", "SHORT_STANDALONE", "NARROW", "NARROW_STANDALONE"].get(o as usize).copied().unwrap_or("FULL"),
        cls::RESOLVER_STYLE => ["STRICT", "SMART", "LENIENT"].get(o as usize).copied().unwrap_or("SMART"),
        cls::FORMAT_STYLE => ["FULL", "LONG", "MEDIUM", "SHORT"].get(o as usize).copied().unwrap_or("FULL"),
        cls::SIGN_STYLE => ["NORMAL", "ALWAYS", "NEVER", "NOT_NEGATIVE", "EXCEEDS_PAD"].get(o as usize).copied().unwrap_or("NORMAL"),
        cls::NORMALIZER_FORM => ["NFD", "NFC", "NFKD", "NFKC"].get(o as usize).copied().unwrap_or("NFD"),
        cls::LOCALE_CATEGORY => ["DISPLAY", "FORMAT"].get(o as usize).copied().unwrap_or("DISPLAY"),
        cls::UNICODE_SCRIPT => crate::charblocks::data().scripts.get(o as usize).map(|s| s.0).unwrap_or(""),
        cls::ISO_CHRONOLOGY => "ISO",
        _ => "",
    }
}

/// `LocalDate.toString`.
pub fn format_date(d: Date) -> String {
    let y = d.y;
    let ys = if y.unsigned_abs() < 1000 {
        if y < 0 {
            format!("-{:04}", y.unsigned_abs())
        } else {
            format!("{y:04}")
        }
    } else if y > 9999 {
        format!("+{y}")
    } else {
        y.to_string()
    };
    format!("{ys}-{:02}-{:02}", d.m, d.d)
}

/// `LocalTime.toString`: seconds and fractions only when non-zero, fractions in groups of 3.
pub fn format_time(t: Time) -> String {
    let mut s = format!("{:02}:{:02}", t.hour(), t.minute());
    let (sec, n) = (t.second(), t.nano());
    if sec > 0 || n > 0 {
        s.push_str(&format!(":{sec:02}"));
        if n > 0 {
            s.push_str(&nano_fraction(n));
        }
    }
    s
}

/// `HH:mm:ss` plus a 3/6/9-digit fraction (as `Instant.toString` prints).
fn format_time_full(t: Time) -> String {
    let mut s = format!("{:02}:{:02}:{:02}", t.hour(), t.minute(), t.second());
    let n = t.nano();
    if n > 0 {
        s.push_str(&nano_fraction(n));
    }
    s
}

fn nano_fraction(n: i64) -> String {
    if n % 1_000_000 == 0 {
        format!(".{:03}", n / 1_000_000)
    } else if n % 1000 == 0 {
        format!(".{:06}", n / 1000)
    } else {
        format!(".{n:09}")
    }
}

/// `Duration.toString`.
fn duration_string(secs: i64, nanos: i32) -> String {
    if secs == 0 && nanos == 0 {
        return "PT0S".into();
    }
    let effective = if secs < 0 && nanos > 0 { secs + 1 } else { secs };
    let hours = effective / 3600;
    let minutes = (effective % 3600) / 60;
    let s = effective % 60;
    let mut out = String::from("PT");
    if hours != 0 {
        out.push_str(&format!("{hours}H"));
    }
    if minutes != 0 {
        out.push_str(&format!("{minutes}M"));
    }
    if s == 0 && nanos == 0 && out.len() > 2 {
        return out;
    }
    if secs < 0 && nanos > 0 {
        if s == 0 {
            out.push_str("-0");
        } else {
            out.push_str(&s.to_string());
        }
    } else {
        out.push_str(&s.to_string());
    }
    if nanos > 0 {
        let frac = if secs < 0 { 2 * NANOS_PER_SECOND - nanos as i64 } else { nanos as i64 + NANOS_PER_SECOND };
        let mut f = frac.to_string();
        while f.ends_with('0') {
            f.pop();
        }
        f.replace_range(0..1, ".");
        out.push_str(&f);
    }
    out.push('S');
    out
}

/// `Date.toString` in UTC (the zone Elasticsearch's JVM runs in).
/// `Date.toString()` in GMT: Julian dates before the Gregorian cutover, and years before the
/// common era printed without their era (as `java.util.Date` does).
fn util_date_string(ms: i64) -> String {
    let ed = ms.div_euclid(86_400_000);
    let tod = ms.rem_euclid(86_400_000) / 1000;
    let (y, m, d) = if ed >= legacy::DEFAULT_CUTOVER / 86_400_000 { legacy::gregorian_from_ed(ed) } else { legacy::julian_from_ed(ed) };
    let y = if y <= 0 { 1 - y } else { y };
    let dow = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][legacy::dow(ed) as usize - 1];
    let mon = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][m as usize - 1];
    format!("{dow} {mon} {d:02} {:02}:{:02}:{:02} GMT {y}", tod / 3600, (tod / 60) % 60, tod % 60)
}

// ------------------------------------------------------------------------------ generic field access

fn unsupported_field(f: Field) -> Thrown {
    unsupported(format!("Unsupported field: {}", f.name()))
}

fn unsupported_unit(u: Unit) -> Thrown {
    unsupported(format!("Unsupported unit: {}", u.name()))
}

fn time_field(t: Time, f: Field) -> Option<i64> {
    use Field::*;
    Some(match f {
        NanoOfSecond => t.nano(),
        NanoOfDay => t.0,
        MicroOfSecond => t.nano() / 1000,
        MicroOfDay => t.0 / 1000,
        MilliOfSecond => t.nano() / 1_000_000,
        MilliOfDay => t.0 / 1_000_000,
        SecondOfMinute => t.second(),
        SecondOfDay => t.second_of_day(),
        MinuteOfHour => t.minute(),
        MinuteOfDay => t.hour() * 60 + t.minute(),
        HourOfAmPm => t.hour() % 12,
        ClockHourOfAmPm => {
            let h = t.hour() % 12;
            if h == 0 {
                12
            } else {
                h
            }
        }
        HourOfDay => t.hour(),
        ClockHourOfDay => {
            if t.hour() == 0 {
                24
            } else {
                t.hour()
            }
        }
        AmPmOfDay => t.hour() / 12,
        _ => return None,
    })
}

fn date_field(d: Date, f: Field) -> Option<i64> {
    use Field::*;
    Some(match f {
        DayOfWeek => d.day_of_week() as i64,
        AlignedDayOfWeekInMonth => ((d.d as i64 - 1) % 7) + 1,
        AlignedDayOfWeekInYear => ((d.day_of_year() as i64 - 1) % 7) + 1,
        DayOfMonth => d.d as i64,
        DayOfYear => d.day_of_year() as i64,
        EpochDay => d.epoch_day(),
        AlignedWeekOfMonth => ((d.d as i64 - 1) / 7) + 1,
        AlignedWeekOfYear => ((d.day_of_year() as i64 - 1) / 7) + 1,
        MonthOfYear => d.m as i64,
        ProlepticMonth => d.proleptic_month(),
        YearOfEra => {
            if d.y >= 1 {
                d.y as i64
            } else {
                1 - d.y as i64
            }
        }
        Year => d.y as i64,
        Era => {
            if d.y >= 1 {
                1
            } else {
                0
            }
        }
        _ => return None,
    })
}

impl TimeVal {
    /// `TemporalAccessor.getLong(field)`.
    pub fn get_long(&self, f: Field) -> Result<i64, Thrown> {
        use TimeVal::*;
        let r = match self {
            Parsed(p) => return p.get_long(f),
            Instant { secs, nanos } => match f {
                Field::NanoOfSecond => Some(*nanos as i64),
                Field::MicroOfSecond => Some(*nanos as i64 / 1000),
                Field::MilliOfSecond => Some(*nanos as i64 / 1_000_000),
                Field::InstantSeconds => Some(*secs),
                _ => None,
            },
            LocalDate(d) => date_field(*d, f),
            LocalTime(t) => time_field(*t, f),
            LocalDateTime(d, t) => time_field(*t, f).or_else(|| date_field(*d, f)),
            Zoned { d, t, offset, .. } | OffsetDateTime { d, t, offset } => match f {
                Field::InstantSeconds => Some(epoch_of_local(*d, *t, *offset)),
                Field::OffsetSeconds => Some(*offset as i64),
                _ => time_field(*t, f).or_else(|| date_field(*d, f)),
            },
            OffsetTime { t, offset } => match f {
                Field::OffsetSeconds => Some(*offset as i64),
                _ => time_field(*t, f),
            },
            ZoneId(Zone::Offset(o)) => (f == Field::OffsetSeconds).then_some(*o as i64),
            DayOfWeek(d) => (f == Field::DayOfWeek).then_some(*d as i64),
            Month(m) => (f == Field::MonthOfYear).then_some(*m as i64),
            Year(y) => match f {
                Field::Year => Some(*y as i64),
                Field::YearOfEra => Some(if *y >= 1 { *y as i64 } else { 1 - *y as i64 }),
                Field::Era => Some(if *y >= 1 { 1 } else { 0 }),
                _ => None,
            },
            YearMonth(y, m) => match f {
                Field::MonthOfYear => Some(*m as i64),
                Field::ProlepticMonth => Some(*y as i64 * 12 + *m as i64 - 1),
                Field::Year => Some(*y as i64),
                Field::YearOfEra => Some(if *y >= 1 { *y as i64 } else { 1 - *y as i64 }),
                Field::Era => Some(if *y >= 1 { 1 } else { 0 }),
                _ => None,
            },
            MonthDay(m, d) => match f {
                Field::MonthOfYear => Some(*m as i64),
                Field::DayOfMonth => Some(*d as i64),
                _ => None,
            },
            _ => None,
        };
        r.ok_or_else(|| unsupported_field(f))
    }

    pub fn is_supported(&self, f: Field) -> bool {
        self.get_long(f).is_ok()
    }

    /// `TemporalAccessor.get(field)`: int range check for wide fields.
    pub fn get_int(&self, f: Field) -> Result<i32, Thrown> {
        if matches!(f, Field::NanoOfDay | Field::MicroOfDay | Field::EpochDay | Field::ProlepticMonth | Field::InstantSeconds) {
            let wide = matches!(self, TimeVal::Instant { .. }) && f == Field::InstantSeconds;
            if !wide {
                return Err(unsupported(format!("Invalid field '{}' for get() method, use getLong() instead", f.name())));
            }
            return Err(unsupported(format!("Unsupported field: {}", f.name())));
        }
        Ok(self.get_long(f)? as i32)
    }

    /// `with(field, value)`.
    pub fn with_field(&self, f: Field, v: i64) -> Result<TimeVal, Thrown> {
        use TimeVal::*;
        Ok(match self {
            LocalDate(d) => LocalDate(date_with(*d, f, v)?),
            LocalTime(t) => LocalTime(time_with(*t, f, v)?),
            LocalDateTime(d, t) => {
                if f.is_time_based() {
                    LocalDateTime(*d, time_with(*t, f, v)?)
                } else {
                    LocalDateTime(date_with(*d, f, v)?, *t)
                }
            }
            Zoned { d, t, offset, zone } => match f {
                Field::InstantSeconds => zoned_of_instant(v, t.nano() as i32, zone.clone()),
                Field::OffsetSeconds => {
                    let o = check(Field::OffsetSeconds, v)? as i32;
                    if zone.valid_offsets(*d, *t).contains(&o) {
                        Zoned { d: *d, t: *t, offset: o, zone: zone.clone() }
                    } else {
                        self.clone()
                    }
                }
                _ if f.is_time_based() => zoned(*d, time_with(*t, f, v)?, zone.clone(), Some(*offset)),
                _ => zoned(date_with(*d, f, v)?, *t, zone.clone(), Some(*offset)),
            },
            OffsetDateTime { d, t, offset } => match f {
                Field::InstantSeconds => {
                    let (nd, nt) = local_of_epoch(v, t.nano() as i32, *offset);
                    OffsetDateTime { d: nd, t: nt, offset: *offset }
                }
                Field::OffsetSeconds => OffsetDateTime { d: *d, t: *t, offset: check(f, v)? as i32 },
                _ if f.is_time_based() => OffsetDateTime { d: *d, t: time_with(*t, f, v)?, offset: *offset },
                _ => OffsetDateTime { d: date_with(*d, f, v)?, t: *t, offset: *offset },
            },
            OffsetTime { t, offset } => match f {
                Field::OffsetSeconds => OffsetTime { t: *t, offset: check(f, v)? as i32 },
                _ => OffsetTime { t: time_with(*t, f, v)?, offset: *offset },
            },
            Instant { secs, nanos } => match f {
                Field::MilliOfSecond => Instant { secs: *secs, nanos: check(f, v)? as i32 * 1_000_000 },
                Field::MicroOfSecond => Instant { secs: *secs, nanos: check(f, v)? as i32 * 1000 },
                Field::NanoOfSecond => Instant { secs: *secs, nanos: check(f, v)? as i32 },
                Field::InstantSeconds => Instant { secs: v, nanos: *nanos },
                _ => return Err(unsupported_field(f)),
            },
            Year(_) => match f {
                Field::Year => Year(check(f, v)? as i32),
                Field::YearOfEra => Year(check(f, v)? as i32),
                _ => return Err(unsupported_field(f)),
            },
            YearMonth(y, m) => match f {
                Field::MonthOfYear => YearMonth(*y, check(f, v)? as u8),
                Field::Year => YearMonth(check(f, v)? as i32, *m),
                Field::ProlepticMonth => YearMonth(v.div_euclid(12) as i32, (v.rem_euclid(12) + 1) as u8),
                _ => return Err(unsupported_field(f)),
            },
            _ => return Err(unsupported_field(f)),
        })
    }

    /// `plus(amount, unit)`.
    pub fn plus_unit(&self, n: i64, u: Unit) -> Result<TimeVal, Thrown> {
        use TimeVal::*;
        Ok(match self {
            Instant { secs, nanos } => {
                let (s, ns) = match u {
                    Unit::Nanos => plus_secs_nanos(*secs, *nanos, 0, n)?,
                    Unit::Micros => plus_secs_nanos(*secs, *nanos, n.div_euclid(1_000_000), n.rem_euclid(1_000_000) * 1000)?,
                    Unit::Millis => plus_secs_nanos(*secs, *nanos, n.div_euclid(1000), n.rem_euclid(1000) * 1_000_000)?,
                    Unit::Seconds => plus_secs_nanos(*secs, *nanos, n, 0)?,
                    Unit::Minutes => plus_secs_nanos(*secs, *nanos, n.checked_mul(60).ok_or_else(|| arith("long overflow"))?, 0)?,
                    Unit::Hours => plus_secs_nanos(*secs, *nanos, n.checked_mul(3600).ok_or_else(|| arith("long overflow"))?, 0)?,
                    Unit::HalfDays => plus_secs_nanos(*secs, *nanos, n.checked_mul(43200).ok_or_else(|| arith("long overflow"))?, 0)?,
                    Unit::Days => plus_secs_nanos(*secs, *nanos, n.checked_mul(86400).ok_or_else(|| arith("long overflow"))?, 0)?,
                    _ => return Err(unsupported_unit(u)),
                };
                check_instant(s)?;
                Instant { secs: s, nanos: ns }
            }
            LocalDate(d) => LocalDate(date_plus(*d, n, u)?),
            LocalTime(t) => LocalTime(time_plus(*t, n, u)?.1),
            LocalDateTime(d, t) => {
                let (nd, nt) = ldt_plus(*d, *t, n, u)?;
                LocalDateTime(nd, nt)
            }
            Zoned { d, t, offset, zone } => {
                if u.is_date_based() {
                    let nd = date_plus(*d, n, u)?;
                    zoned(nd, *t, zone.clone(), Some(*offset))
                } else {
                    // time units operate on the instant
                    let (secs, nanos) = self.instant().unwrap();
                    let Instant { secs: s, nanos: ns } = (Instant { secs, nanos }).plus_unit_time(n, u)? else { unreachable!() };
                    zoned_of_instant(s, ns, zone.clone())
                }
            }
            OffsetDateTime { d, t, offset } => {
                let (nd, nt) = ldt_plus(*d, *t, n, u)?;
                OffsetDateTime { d: nd, t: nt, offset: *offset }
            }
            OffsetTime { t, offset } => OffsetTime { t: time_plus(*t, n, u)?.1, offset: *offset },
            Year(y) => match u {
                Unit::Years => Year(check(Field::Year, *y as i64 + n)? as i32),
                Unit::Decades => Year(check(Field::Year, *y as i64 + n * 10)? as i32),
                Unit::Centuries => Year(check(Field::Year, *y as i64 + n * 100)? as i32),
                Unit::Millennia => Year(check(Field::Year, *y as i64 + n * 1000)? as i32),
                _ => return Err(unsupported_unit(u)),
            },
            YearMonth(y, m) => {
                let months = match u {
                    Unit::Months => n,
                    Unit::Years => n * 12,
                    Unit::Decades => n * 120,
                    Unit::Centuries => n * 1200,
                    Unit::Millennia => n * 12000,
                    _ => return Err(unsupported_unit(u)),
                };
                let total = *y as i64 * 12 + *m as i64 - 1 + months;
                YearMonth(check(Field::Year, total.div_euclid(12))? as i32, (total.rem_euclid(12) + 1) as u8)
            }
            _ => return Err(unsupported_unit(u)),
        })
    }

    fn plus_unit_time(&self, n: i64, u: Unit) -> Result<TimeVal, Thrown> {
        match u {
            Unit::Nanos | Unit::Micros | Unit::Millis | Unit::Seconds | Unit::Minutes | Unit::Hours | Unit::HalfDays => self.plus_unit(n, u),
            _ => Err(unsupported_unit(u)),
        }
    }

    /// `until(end, unit)`.
    pub fn until(&self, end: &TimeVal, u: Unit) -> Result<i64, Thrown> {
        use TimeVal::*;
        match (self, end) {
            (LocalDate(a), _) => {
                let b = to_local_date(end)?;
                date_until(*a, b, u)
            }
            (LocalTime(a), _) => {
                let b = match end {
                    LocalTime(t) => *t,
                    LocalDateTime(_, t) | Zoned { t, .. } | OffsetDateTime { t, .. } | OffsetTime { t, .. } => *t,
                    _ => return Err(dte(format!("Unable to obtain LocalTime from TemporalAccessor: {}", end.describe()))),
                };
                time_until(b.0 - a.0, u)
            }
            (LocalDateTime(d1, t1), _) => {
                let (d2, t2) = match end {
                    LocalDateTime(d, t) | Zoned { d, t, .. } | OffsetDateTime { d, t, .. } => (*d, *t),
                    _ => return Err(dte(format!("Unable to obtain LocalDateTime from TemporalAccessor: {}", end.describe()))),
                };
                ldt_until(*d1, *t1, d2, t2, u)
            }
            (Zoned { zone, offset, .. }, _) => {
                let other = match end {
                    Zoned { .. } | OffsetDateTime { .. } | Instant { .. } => {
                        let (s, n) = end.instant().unwrap();
                        zoned_of_instant(s, n, zone.clone())
                    }
                    _ => return Err(dte(format!("Unable to obtain ZonedDateTime from TemporalAccessor: {}", end.describe()))),
                };
                let _ = offset;
                if u.is_date_based() {
                    let (Some((d1, t1)), Some((d2, t2))) = (self.local_dt(), other.local_dt()) else { unreachable!() };
                    ldt_until(d1, t1, d2, t2, u)
                } else {
                    let (a, b) = (self.instant().unwrap(), other.instant().unwrap());
                    instant_until(a, b, u)
                }
            }
            (OffsetDateTime { offset, .. }, _) => {
                let (s, n) = end.instant().ok_or_else(|| dte(format!("Unable to obtain OffsetDateTime from TemporalAccessor: {}", end.describe())))?;
                let (d2, t2) = local_of_epoch(s, n, *offset);
                let (d1, t1) = self.local_dt().unwrap();
                ldt_until(d1, t1, d2, t2, u)
            }
            (Instant { secs, nanos }, _) => {
                let b = end.instant().ok_or_else(|| dte(format!("Unable to obtain Instant from TemporalAccessor: {}", end.describe())))?;
                instant_until((*secs, *nanos), b, u)
            }
            (Year(a), Year(b)) => {
                let d = *b as i64 - *a as i64;
                match u {
                    Unit::Years => Ok(d),
                    Unit::Decades => Ok(d / 10),
                    Unit::Centuries => Ok(d / 100),
                    Unit::Millennia => Ok(d / 1000),
                    _ => Err(unsupported_unit(u)),
                }
            }
            (YearMonth(y1, m1), YearMonth(y2, m2)) => {
                let d = (*y2 as i64 * 12 + *m2 as i64) - (*y1 as i64 * 12 + *m1 as i64);
                match u {
                    Unit::Months => Ok(d),
                    Unit::Years => Ok(d / 12),
                    Unit::Decades => Ok(d / 120),
                    Unit::Centuries => Ok(d / 1200),
                    Unit::Millennia => Ok(d / 12000),
                    _ => Err(unsupported_unit(u)),
                }
            }
            _ => Err(unsupported_unit(u)),
        }
    }

    /// A description used in "Unable to obtain X from TemporalAccessor" messages.
    pub fn describe(&self) -> String {
        format!("{} of type {}", self.to_java_string(), crate::lookup::java_binary_name(self.class()))
    }

    /// `truncatedTo(unit)`.
    pub fn truncated(&self, u: Unit) -> Result<TimeVal, Thrown> {
        use TimeVal::*;
        let trunc_time = |t: Time| -> Result<Time, Thrown> {
            if u == Unit::Nanos {
                return Ok(t);
            }
            if u == Unit::Days {
                return Ok(Time(0));
            }
            if u > Unit::Days {
                return Err(unsupported("Unit is too large to be used for truncation".to_string()));
            }
            let un = u.nanos();
            if NANOS_PER_DAY % un != 0 {
                return Err(unsupported("Unit must divide into a standard day without remainder".to_string()));
            }
            Ok(Time((t.0 / un) * un))
        };
        Ok(match self {
            LocalTime(t) => LocalTime(trunc_time(*t)?),
            LocalDateTime(d, t) => LocalDateTime(*d, trunc_time(*t)?),
            Zoned { d, t, offset, zone } => zoned(*d, trunc_time(*t)?, zone.clone(), Some(*offset)),
            OffsetDateTime { d, t, offset } => OffsetDateTime { d: *d, t: trunc_time(*t)?, offset: *offset },
            OffsetTime { t, offset } => OffsetTime { t: trunc_time(*t)?, offset: *offset },
            Instant { secs, nanos } => {
                if u == Unit::Nanos {
                    return Ok(self.clone());
                }
                let un = u.nanos();
                if u > Unit::Days {
                    return Err(unsupported("Unit is too large to be used for truncation".to_string()));
                }
                if NANOS_PER_DAY % un != 0 {
                    return Err(unsupported("Unit must divide into a standard day without remainder".to_string()));
                }
                let nod = secs.rem_euclid(SECONDS_PER_DAY) * NANOS_PER_SECOND + *nanos as i64;
                let result = (nod / un) * un;
                let (s, n) = plus_secs_nanos(*secs, *nanos, 0, result - nod)?;
                Instant { secs: s, nanos: n }
            }
            Duration { secs, nanos } => {
                if u == Unit::Nanos {
                    return Ok(self.clone());
                }
                let un = u.nanos() as i128;
                if u > Unit::Days {
                    return Err(unsupported("Unit is too large to be used for truncation".to_string()));
                }
                let total = *secs as i128 * NANOS_PER_SECOND as i128 + *nanos as i128;
                let r = (total / un) * un;
                let (s, n) = (r.div_euclid(NANOS_PER_SECOND as i128) as i64, r.rem_euclid(NANOS_PER_SECOND as i128) as i32);
                Duration { secs: s, nanos: n }
            }
            _ => return Err(unsupported_unit(u)),
        })
    }
}

fn check_instant(s: i64) -> Result<(), Thrown> {
    if !(-31557014167219200..=31556889864403199).contains(&s) {
        return Err(dte("Instant exceeds minimum or maximum instant"));
    }
    Ok(())
}

pub fn plus_secs_nanos(secs: i64, nanos: i32, add_s: i64, add_n: i64) -> Result<(i64, i32), Thrown> {
    let s = secs.checked_add(add_s).ok_or_else(|| arith("long overflow"))?;
    let s = s.checked_add(add_n.div_euclid(NANOS_PER_SECOND)).ok_or_else(|| arith("long overflow"))?;
    let n = nanos as i64 + add_n.rem_euclid(NANOS_PER_SECOND);
    let s = s.checked_add(n / NANOS_PER_SECOND).ok_or_else(|| arith("long overflow"))?;
    Ok((s, (n % NANOS_PER_SECOND) as i32))
}

fn date_plus(d: Date, n: i64, u: Unit) -> Result<Date, Thrown> {
    let mul = |a: i64, b: i64| a.checked_mul(b).ok_or_else(|| arith("long overflow"));
    match u {
        Unit::Days => d.plus_days(n),
        Unit::Weeks => d.plus_days(mul(n, 7)?),
        Unit::Months => d.plus_months(n),
        Unit::Years => d.plus_years(n),
        Unit::Decades => d.plus_years(mul(n, 10)?),
        Unit::Centuries => d.plus_years(mul(n, 100)?),
        Unit::Millennia => d.plus_years(mul(n, 1000)?),
        Unit::Eras => {
            let era = if d.y >= 1 { 1 } else { 0 };
            let ne = check(Field::Era, era + n)?;
            date_with(d, Field::Era, ne)
        }
        _ => Err(unsupported_unit(u)),
    }
}

/// Adds a time unit to a time: returns the day overflow and the new time.
fn time_plus(t: Time, n: i64, u: Unit) -> Result<(i64, Time), Thrown> {
    let nanos: i128 = match u {
        Unit::Nanos => n as i128,
        Unit::Micros => n as i128 * 1000,
        Unit::Millis => n as i128 * 1_000_000,
        Unit::Seconds => n as i128 * NANOS_PER_SECOND as i128,
        Unit::Minutes => n as i128 * 60 * NANOS_PER_SECOND as i128,
        Unit::Hours => n as i128 * 3600 * NANOS_PER_SECOND as i128,
        Unit::HalfDays => n as i128 * 43200 * NANOS_PER_SECOND as i128,
        _ => return Err(unsupported_unit(u)),
    };
    let total = t.0 as i128 + nanos;
    let days = total.div_euclid(NANOS_PER_DAY as i128) as i64;
    Ok((days, Time(total.rem_euclid(NANOS_PER_DAY as i128) as i64)))
}

fn ldt_plus(d: Date, t: Time, n: i64, u: Unit) -> Result<(Date, Time), Thrown> {
    if u.is_date_based() {
        return Ok((date_plus(d, n, u)?, t));
    }
    let (days, nt) = time_plus(t, n, u)?;
    Ok((d.plus_days(days)?, nt))
}

fn date_with(d: Date, f: Field, v: i64) -> Result<Date, Thrown> {
    use Field::*;
    check(f, v)?;
    Ok(match f {
        DayOfWeek => d.plus_days(v - d.day_of_week() as i64)?,
        AlignedDayOfWeekInMonth => d.plus_days(v - date_field(d, f).unwrap())?,
        AlignedDayOfWeekInYear => d.plus_days(v - date_field(d, f).unwrap())?,
        DayOfMonth => Date::of(d.y as i64, d.m as i64, v)?,
        DayOfYear => Date::of_year_day(d.y as i64, v)?,
        EpochDay => Date::from_epoch_day(v),
        AlignedWeekOfMonth => d.plus_days((v - date_field(d, f).unwrap()) * 7)?,
        AlignedWeekOfYear => d.plus_days((v - date_field(d, f).unwrap()) * 7)?,
        MonthOfYear => Date::clamp(d.y as i64, v as u8, d.d),
        ProlepticMonth => d.plus_months(v - d.proleptic_month())?,
        YearOfEra => Date::clamp(if d.y >= 1 { v } else { 1 - v }, d.m, d.d),
        Year => Date::clamp(v, d.m, d.d),
        Era => {
            let era = if d.y >= 1 { 1 } else { 0 };
            if era == v {
                d
            } else {
                Date::clamp(1 - d.y as i64, d.m, d.d)
            }
        }
        _ => return Err(unsupported_field(f)),
    })
}

fn time_with(t: Time, f: Field, v: i64) -> Result<Time, Thrown> {
    use Field::*;
    check(f, v)?;
    let (h, m, s, n) = (t.hour(), t.minute(), t.second(), t.nano());
    Ok(match f {
        NanoOfSecond => Time::of(h, m, s, v)?,
        NanoOfDay => Time(v),
        MicroOfSecond => Time::of(h, m, s, v * 1000)?,
        MicroOfDay => Time(v * 1000),
        MilliOfSecond => Time::of(h, m, s, v * 1_000_000)?,
        MilliOfDay => Time(v * 1_000_000),
        SecondOfMinute => Time::of(h, m, v, n)?,
        SecondOfDay => Time(v * NANOS_PER_SECOND + n),
        MinuteOfHour => Time::of(h, v, s, n)?,
        MinuteOfDay => Time::of(v / 60, v % 60, s, n)?,
        HourOfAmPm => Time::of((h / 12) * 12 + v, m, s, n)?,
        ClockHourOfAmPm => Time::of((h / 12) * 12 + if v == 12 { 0 } else { v }, m, s, n)?,
        HourOfDay => Time::of(v, m, s, n)?,
        ClockHourOfDay => Time::of(if v == 24 { 0 } else { v }, m, s, n)?,
        AmPmOfDay => Time::of(h % 12 + v * 12, m, s, n)?,
        _ => return Err(unsupported_field(f)),
    })
}

fn date_until(a: Date, b: Date, u: Unit) -> Result<i64, Thrown> {
    Ok(match u {
        Unit::Days => b.epoch_day() - a.epoch_day(),
        Unit::Weeks => (b.epoch_day() - a.epoch_day()) / 7,
        Unit::Months => a.months_until(b),
        Unit::Years => a.months_until(b) / 12,
        Unit::Decades => a.months_until(b) / 120,
        Unit::Centuries => a.months_until(b) / 1200,
        Unit::Millennia => a.months_until(b) / 12000,
        Unit::Eras => (if b.y >= 1 { 1 } else { 0 }) - (if a.y >= 1 { 1 } else { 0 }),
        _ => return Err(unsupported_unit(u)),
    })
}

fn time_until(diff: i64, u: Unit) -> Result<i64, Thrown> {
    Ok(match u {
        Unit::Nanos => diff,
        Unit::Micros => diff / 1000,
        Unit::Millis => diff / 1_000_000,
        Unit::Seconds => diff / NANOS_PER_SECOND,
        Unit::Minutes => diff / (60 * NANOS_PER_SECOND),
        Unit::Hours => diff / (3600 * NANOS_PER_SECOND),
        Unit::HalfDays => diff / (43200 * NANOS_PER_SECOND),
        _ => return Err(unsupported_unit(u)),
    })
}

fn ldt_until(d1: Date, t1: Time, d2: Date, t2: Time, u: Unit) -> Result<i64, Thrown> {
    if u.is_date_based() {
        let mut end_date = d2;
        if end_date > d1 && t2 < t1 {
            end_date = end_date.plus_days(-1)?;
        } else if end_date < d1 && t2 > t1 {
            end_date = end_date.plus_days(1)?;
        }
        return date_until(d1, end_date, u);
    }
    let days = (d2.epoch_day() - d1.epoch_day()) as i128;
    let diff = days * NANOS_PER_DAY as i128 + (t2.0 - t1.0) as i128;
    let per: i128 = match u {
        Unit::Nanos => 1,
        Unit::Micros => 1000,
        Unit::Millis => 1_000_000,
        Unit::Seconds => NANOS_PER_SECOND as i128,
        Unit::Minutes => 60 * NANOS_PER_SECOND as i128,
        Unit::Hours => 3600 * NANOS_PER_SECOND as i128,
        Unit::HalfDays => 43200 * NANOS_PER_SECOND as i128,
        _ => return Err(unsupported_unit(u)),
    };
    let r = diff / per;
    i64::try_from(r).map_err(|_| arith("long overflow"))
}

fn instant_until(a: (i64, i32), b: (i64, i32), u: Unit) -> Result<i64, Thrown> {
    let diff = (b.0 as i128 - a.0 as i128) * NANOS_PER_SECOND as i128 + (b.1 as i128 - a.1 as i128);
    let per: i128 = match u {
        Unit::Nanos => 1,
        Unit::Micros => 1000,
        Unit::Millis => 1_000_000,
        Unit::Seconds => NANOS_PER_SECOND as i128,
        Unit::Minutes => 60 * NANOS_PER_SECOND as i128,
        Unit::Hours => 3600 * NANOS_PER_SECOND as i128,
        Unit::HalfDays => 43200 * NANOS_PER_SECOND as i128,
        Unit::Days => 86400 * NANOS_PER_SECOND as i128,
        _ => return Err(unsupported_unit(u)),
    };
    i64::try_from(diff / per).map_err(|_| arith("long overflow"))
}

fn to_local_date(v: &TimeVal) -> Result<Date, Thrown> {
    match v {
        TimeVal::LocalDate(d) | TimeVal::LocalDateTime(d, _) | TimeVal::Zoned { d, .. } | TimeVal::OffsetDateTime { d, .. } => Ok(*d),
        _ => Err(dte(format!("Unable to obtain LocalDate from TemporalAccessor: {}", v.describe()))),
    }
}

// ------------------------------------------------------------------------------ ISO parsing

/// Minimal recursive-descent parsing for the ISO formats `parse(CharSequence)` accepts.
struct Iso<'a> {
    s: &'a [u8],
    i: usize,
    text: &'a str,
}

impl Iso<'_> {
    fn fail(&self) -> Thrown {
        parse_error(self.text, self.i, "")
    }
    fn digits(&mut self, min: usize, max: usize) -> Result<i64, Thrown> {
        let start = self.i;
        while self.i < self.s.len() && self.s[self.i].is_ascii_digit() && self.i - start < max {
            self.i += 1;
        }
        if self.i - start < min {
            self.i = start;
            return Err(self.fail());
        }
        Ok(std::str::from_utf8(&self.s[start..self.i]).unwrap().parse().unwrap_or(0))
    }
    fn lit(&mut self, c: u8) -> Result<(), Thrown> {
        if self.s.get(self.i) == Some(&c) {
            self.i += 1;
            Ok(())
        } else {
            Err(self.fail())
        }
    }
    fn year(&mut self) -> Result<i64, Thrown> {
        let sign = self.s.get(self.i).copied().filter(|c| *c == b'+' || *c == b'-');
        if sign.is_some() {
            self.i += 1;
        }
        let start = self.i;
        let y = self.digits(4, 10)?;
        // SignStyle.EXCEEDS_PAD (strict): more than four digits need a '+', and a '+' needs more
        let len = self.i - start;
        if sign == Some(b'+') && len <= 4 {
            self.i = start - 1;
            return Err(self.fail());
        }
        if sign.is_none() && len > 4 {
            self.i = start;
            return Err(self.fail());
        }
        Ok(if sign == Some(b'-') { -y } else { y })
    }
    fn date(&mut self) -> Result<Date, Thrown> {
        let y = self.year()?;
        self.lit(b'-')?;
        let m = self.digits(2, 2)?;
        self.lit(b'-')?;
        let d = self.digits(2, 2)?;
        Date::of(y, m, d).map_err(|e| self.resolve_error(e))
    }
    fn time(&mut self) -> Result<Time, Thrown> {
        let h = self.digits(2, 2)?;
        self.lit(b':')?;
        let m = self.digits(2, 2)?;
        let mut s = 0;
        let mut n = 0;
        if self.s.get(self.i) == Some(&b':') {
            self.i += 1;
            s = self.digits(2, 2)?;
            if self.s.get(self.i) == Some(&b'.') || self.s.get(self.i) == Some(&b',') {
                self.i += 1;
                let start = self.i;
                let f = self.digits(0, 9)?;
                let len = self.i - start;
                n = f * 10i64.pow(9 - len as u32);
            }
        }
        Time::of(h, m, s, n).map_err(|e| self.resolve_error(e))
    }
    fn offset(&mut self) -> Result<i32, Thrown> {
        match self.s.get(self.i) {
            Some(b'Z') | Some(b'z') => {
                self.i += 1;
                Ok(0)
            }
            Some(b'+') | Some(b'-') => {
                let neg = self.s[self.i] == b'-';
                self.i += 1;
                let h = self.digits(2, 2)?;
                let mut m = 0;
                let mut s = 0;
                if self.s.get(self.i) == Some(&b':') {
                    self.i += 1;
                    m = self.digits(2, 2)?;
                    if self.s.get(self.i) == Some(&b':') {
                        self.i += 1;
                        s = self.digits(2, 2)?;
                    }
                }
                let sign = if neg { -1 } else { 1 };
                offset_hms(sign * h as i32, sign * m as i32, sign * s as i32).map_err(|e| self.resolve_error(e))
            }
            _ => Err(self.fail()),
        }
    }
    fn zone_id(&mut self) -> Result<Option<Zone>, Thrown> {
        if self.s.get(self.i) != Some(&b'[') {
            return Ok(None);
        }
        self.i += 1;
        let start = self.i;
        while self.i < self.s.len() && self.s[self.i] != b']' {
            self.i += 1;
        }
        let id = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("");
        let at = start;
        self.lit(b']')?;
        match zone_of(id) {
            Ok(z) => Ok(Some(z)),
            Err(_) => {
                self.i = at;
                Err(self.fail())
            }
        }
    }
    fn end(&self) -> Result<(), Thrown> {
        if self.i != self.s.len() {
            let rest = &self.text[self.i..];
            return Err(parse_error(self.text, self.i, &format!(", unparsed text found at index {}", self.i)).with_rest(rest));
        }
        Ok(())
    }
    fn resolve_error(&self, e: Thrown) -> Thrown {
        let msg = e.message.clone().unwrap_or_default();
        Throw::new(cls::DATE_TIME_PARSE_EXCEPTION, Some(format!("Text '{}' could not be parsed: {msg}", self.text))).with_cause(&e)
    }
}

trait WithRest {
    fn with_rest(self, rest: &str) -> Self;
}
impl WithRest for Thrown {
    fn with_rest(self, _rest: &str) -> Thrown {
        self
    }
}

fn iso<'a>(text: &'a str) -> Iso<'a> {
    Iso { s: text.as_bytes(), i: 0, text }
}

pub fn parse_local_date(text: &str) -> Result<Date, Thrown> {
    let mut p = iso(text);
    let d = p.date()?;
    p.end()?;
    Ok(d)
}

pub fn parse_local_time(text: &str) -> Result<Time, Thrown> {
    let mut p = iso(text);
    let t = p.time()?;
    p.end()?;
    Ok(t)
}

pub fn parse_local_date_time(text: &str) -> Result<(Date, Time), Thrown> {
    let mut p = iso(text);
    let d = p.date()?;
    if p.s.get(p.i).map(|c| c.to_ascii_uppercase()) != Some(b'T') {
        return Err(p.fail());
    }
    p.i += 1;
    let t = p.time()?;
    p.end()?;
    Ok((d, t))
}

pub fn parse_offset_date_time(text: &str) -> Result<(Date, Time, i32), Thrown> {
    let mut p = iso(text);
    let d = p.date()?;
    if p.s.get(p.i).map(|c| c.to_ascii_uppercase()) != Some(b'T') {
        return Err(p.fail());
    }
    p.i += 1;
    let t = p.time()?;
    let o = p.offset()?;
    p.end()?;
    Ok((d, t, o))
}

pub fn parse_zoned(text: &str) -> Result<TimeVal, Thrown> {
    let mut p = iso(text);
    let d = p.date()?;
    if p.s.get(p.i).map(|c| c.to_ascii_uppercase()) != Some(b'T') {
        return Err(p.fail());
    }
    p.i += 1;
    let t = p.time()?;
    let o = match p.s.get(p.i) {
        Some(b'[') => None,
        _ => Some(p.offset()?),
    };
    let z = p.zone_id()?;
    p.end()?;
    match (o, z) {
        (Some(o), None) => Ok(TimeVal::Zoned { d, t, offset: o, zone: Zone::Offset(o) }),
        // `ZonedDateTime.from` uses the instant the offset fixes, then moves it into the zone
        (Some(o), Some(z)) => Ok(zoned_of_instant(epoch_of_local(d, t, o), t.nano() as i32, z)),
        (None, Some(z)) => Ok(zoned(d, t, z, None)),
        (None, None) => Err(p.fail()),
    }
}

pub fn parse_instant(text: &str) -> Result<(i64, i32), Thrown> {
    let mut p = iso(text);
    let d = p.date()?;
    if p.s.get(p.i).map(|c| c.to_ascii_uppercase()) != Some(b'T') {
        return Err(p.fail());
    }
    p.i += 1;
    let h = p.digits(2, 2)?;
    p.lit(b':')?;
    let m = p.digits(2, 2)?;
    p.lit(b':')?;
    let mut s = p.digits(2, 2)?;
    let mut n = 0;
    if p.s.get(p.i) == Some(&b'.') {
        p.i += 1;
        let start = p.i;
        let f = p.digits(0, 9)?;
        n = f * 10i64.pow(9 - (p.i - start) as u32);
    }
    let o = p.offset()?;
    p.end()?;
    // leap seconds (`23:59:60`) are smoothed to `:59`
    if s == 60 {
        s = 59;
    }
    let t = Time::of(h, m, s, n).map_err(|e| p.resolve_error(e))?;
    Ok((epoch_of_local(d, t, o), n as i32))
}

/// `Duration.parse`.
pub fn parse_duration(text: &str) -> Result<(i64, i32), Thrown> {
    let bad = || Throw::new(cls::DATE_TIME_PARSE_EXCEPTION, Some("Text cannot be parsed to a Duration".to_string()));
    let up = text.to_ascii_uppercase();
    let (neg, rest) = match up.strip_prefix('-') {
        Some(r) => (true, r.to_string()),
        None => (false, up.strip_prefix('+').unwrap_or(&up).to_string()),
    };
    let rest = rest.strip_prefix('P').ok_or_else(bad)?;
    let (day_part, time_part) = match rest.find('T') {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };
    let mut total: i128 = 0;
    let mut any = false;
    let num = |s: &str| -> Result<i128, Thrown> { s.parse::<i128>().map_err(|_| bad()) };
    if !day_part.is_empty() {
        let d = day_part.strip_suffix('D').ok_or_else(bad)?;
        total += num(d)? * 86400 * NANOS_PER_SECOND as i128;
        any = true;
    }
    if let Some(tp) = time_part {
        if tp.is_empty() {
            return Err(bad());
        }
        let mut cur = String::new();
        for c in tp.chars() {
            match c {
                'H' => {
                    total += num(&cur)? * 3600 * NANOS_PER_SECOND as i128;
                    cur.clear();
                    any = true;
                }
                'M' => {
                    total += num(&cur)? * 60 * NANOS_PER_SECOND as i128;
                    cur.clear();
                    any = true;
                }
                'S' => {
                    let (ip, fp) = match cur.find(['.', ',']) {
                        Some(i) => (&cur[..i], &cur[i + 1..]),
                        None => (&cur[..], ""),
                    };
                    let sec = num(ip)?;
                    let mut frac: i128 = 0;
                    if !fp.is_empty() {
                        if fp.len() > 9 || !fp.bytes().all(|b| b.is_ascii_digit()) {
                            return Err(bad());
                        }
                        frac = format!("{fp:0<9}").parse::<i128>().unwrap();
                        if ip.starts_with('-') {
                            frac = -frac;
                        }
                    }
                    total += sec * NANOS_PER_SECOND as i128 + frac;
                    cur.clear();
                    any = true;
                }
                _ => cur.push(c),
            }
        }
        if !cur.is_empty() {
            return Err(bad());
        }
    }
    if !any {
        return Err(bad());
    }
    if neg {
        total = -total;
    }
    let s = total.div_euclid(NANOS_PER_SECOND as i128);
    let n = total.rem_euclid(NANOS_PER_SECOND as i128);
    Ok((i64::try_from(s).map_err(|_| bad())?, n as i32))
}

/// `Period.parse`.
pub fn parse_period(text: &str) -> Result<(i32, i32, i32), Thrown> {
    let bad = || Throw::new(cls::DATE_TIME_PARSE_EXCEPTION, Some("Text cannot be parsed to a Period".to_string()));
    let up = text.to_ascii_uppercase();
    let (neg, rest) = match up.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, up.strip_prefix('+').unwrap_or(&up)),
    };
    let rest = rest.strip_prefix('P').ok_or_else(bad)?;
    if rest.is_empty() {
        return Err(bad());
    }
    let (mut y, mut m, mut w, mut d) = (0i64, 0i64, 0i64, 0i64);
    let mut cur = String::new();
    for c in rest.chars() {
        let v = || cur.parse::<i64>().map_err(|_| bad());
        match c {
            'Y' => {
                y = v()?;
                cur.clear();
            }
            'M' => {
                m = v()?;
                cur.clear();
            }
            'W' => {
                w = v()?;
                cur.clear();
            }
            'D' => {
                d = v()?;
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        return Err(bad());
    }
    let sign = if neg { -1 } else { 1 };
    let days = d + w * 7;
    let fit = |x: i64| i32::try_from(x * sign).map_err(|_| bad());
    Ok((fit(y)?, fit(m)?, fit(days)?))
}

// ------------------------------------------------------------------------------ natives

fn tv(v: &Value) -> Result<&TimeVal, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Time(t) => Ok(t),
            _ => Err(crate::vm::cce(format!("{} is not a temporal", crate::lookup::type_name(v.runtime_ty())))),
        },
        Value::Null => Err(npe("temporal")),
        _ => Err(crate::vm::cce(format!("{} is not a temporal", crate::lookup::type_name(v.runtime_ty())))),
    }
}

fn arg_tv<'a>(v: &'a Value, what: &str) -> Result<&'a TimeVal, Thrown> {
    match v {
        Value::Null => Err(npe(what.to_string())),
        _ => tv(v),
    }
}

fn unit_of(v: &Value) -> Result<Unit, Thrown> {
    match arg_tv(v, "unit")? {
        TimeVal::TUnit(u) => Ok(*u),
        _ => Err(crate::vm::cce("not a TemporalUnit")),
    }
}

fn field_of(v: &Value) -> Result<Field, Thrown> {
    match arg_tv(v, "field")? {
        TimeVal::TField(f) => Ok(*f),
        _ => Err(crate::vm::cce("not a TemporalField")),
    }
}

fn zone_arg(v: &Value) -> Result<Zone, Thrown> {
    match arg_tv(v, "zone")? {
        TimeVal::ZoneId(z) => Ok(z.clone()),
        _ => Err(crate::vm::cce("not a ZoneId")),
    }
}

fn offset_arg(v: &Value) -> Result<i32, Thrown> {
    match arg_tv(v, "offset")? {
        TimeVal::ZoneId(Zone::Offset(o)) => Ok(*o),
        _ => Err(crate::vm::cce("not a ZoneOffset")),
    }
}

fn r(t: TimeVal) -> VR {
    Ok(time_value(t))
}

fn text_arg(v: &Value) -> Result<String, Thrown> {
    match v {
        Value::Null => Err(npe("text")),
        other => crate::lib_lang::char_seq(other),
    }
}

/// `plus(TemporalAmount)` / `minus` with Durations and Periods.
fn plus_amount(t: &TimeVal, amount: &TimeVal, sign: i64) -> Result<TimeVal, Thrown> {
    match amount {
        TimeVal::Duration { secs, nanos } => {
            let s = secs.checked_mul(sign).ok_or_else(|| arith("long overflow"))?;
            let n = *nanos as i64 * sign;
            match t {
                TimeVal::LocalDate(_) => Err(unsupported_unit(Unit::Seconds)),
                TimeVal::Duration { secs: a, nanos: b } => {
                    let (x, y) = plus_secs_nanos(*a, *b, s, n)?;
                    Ok(TimeVal::Duration { secs: x, nanos: y })
                }
                _ => t.plus_unit(s, Unit::Seconds)?.plus_unit(n, Unit::Nanos),
            }
        }
        TimeVal::Period { y, m, d } => {
            let (y, m, d) = (*y as i64 * sign, *m as i64 * sign, *d as i64 * sign);
            match t {
                TimeVal::Instant { .. } | TimeVal::LocalTime(_) | TimeVal::OffsetTime { .. } | TimeVal::Duration { .. } => {
                    if y != 0 {
                        return Err(unsupported_unit(Unit::Years));
                    }
                    if m != 0 {
                        return Err(unsupported_unit(Unit::Months));
                    }
                    if d == 0 {
                        return Ok(t.clone());
                    }
                    t.plus_unit(d, Unit::Days)
                }
                TimeVal::Period { y: a, m: b, d: c } => Ok(TimeVal::Period {
                    y: (*a as i64 + y) as i32,
                    m: (*b as i64 + m) as i32,
                    d: (*c as i64 + d) as i32,
                }),
                _ => {
                    let total_months = y * 12 + m;
                    let mut out = t.clone();
                    if total_months != 0 {
                        out = if m == 0 { out.plus_unit(y, Unit::Years)? } else { out.plus_unit(total_months, Unit::Months)? };
                    }
                    if d != 0 {
                        out = out.plus_unit(d, Unit::Days)?;
                    }
                    Ok(out)
                }
            }
        }
        _ => Err(crate::vm::cce("not a TemporalAmount")),
    }
}

/// `with(TemporalAdjuster)`.
fn adjust(t: &TimeVal, adj: &Value, vm: &mut Vm) -> Result<TimeVal, Thrown> {
    match arg_tv(adj, "adjuster") {
        Ok(a) => match a {
            TimeVal::Adjuster(ad) => {
                let d = to_local_date(t)?;
                let nd = apply_adjuster(d, ad)?;
                replace_date(t, nd)
            }
            TimeVal::LocalDate(d) => replace_date(t, *d),
            TimeVal::LocalTime(nt) => replace_time(t, *nt),
            TimeVal::DayOfWeek(dw) => t.with_field(Field::DayOfWeek, *dw as i64),
            TimeVal::Month(m) => t.with_field(Field::MonthOfYear, *m as i64),
            TimeVal::Year(y) => t.with_field(Field::Year, *y as i64),
            TimeVal::ZoneId(Zone::Offset(o)) => t.with_field(Field::OffsetSeconds, *o as i64),
            TimeVal::Instant { secs, nanos } => t.with_field(Field::InstantSeconds, *secs)?.with_field(Field::NanoOfSecond, *nanos as i64),
            TimeVal::LocalDateTime(d, nt) => replace_time(&replace_date(t, *d)?, *nt),
            TimeVal::YearMonth(y, m) => t.with_field(Field::ProlepticMonth, *y as i64 * 12 + *m as i64 - 1),
            other => Err(dte(format!("Unsupported adjuster: {}", other.to_java_string()))),
        },
        Err(_) if crate::func::is_function(adj) => {
            let r = crate::func::call(vm, adj, &[time_value(t.clone())])?;
            Ok(tv(&r)?.clone())
        }
        Err(e) => Err(e),
    }
}

fn replace_date(t: &TimeVal, nd: Date) -> Result<TimeVal, Thrown> {
    Ok(match t {
        TimeVal::LocalDate(_) => TimeVal::LocalDate(nd),
        TimeVal::LocalDateTime(_, tt) => TimeVal::LocalDateTime(nd, *tt),
        TimeVal::Zoned { t: tt, offset, zone, .. } => zoned(nd, *tt, zone.clone(), Some(*offset)),
        TimeVal::OffsetDateTime { t: tt, offset, .. } => TimeVal::OffsetDateTime { d: nd, t: *tt, offset: *offset },
        _ => return Err(unsupported_field(Field::EpochDay)),
    })
}

fn replace_time(t: &TimeVal, nt: Time) -> Result<TimeVal, Thrown> {
    Ok(match t {
        TimeVal::LocalTime(_) => TimeVal::LocalTime(nt),
        TimeVal::LocalDateTime(d, _) => TimeVal::LocalDateTime(*d, nt),
        TimeVal::Zoned { d, offset, zone, .. } => zoned(*d, nt, zone.clone(), Some(*offset)),
        TimeVal::OffsetDateTime { d, offset, .. } => TimeVal::OffsetDateTime { d: *d, t: nt, offset: *offset },
        TimeVal::OffsetTime { offset, .. } => TimeVal::OffsetTime { t: nt, offset: *offset },
        _ => return Err(unsupported_field(Field::NanoOfDay)),
    })
}

fn apply_adjuster(d: Date, a: &Adjuster) -> Result<Date, Thrown> {
    use Adjuster::*;
    let dow = d.day_of_week() as i64;
    Ok(match a {
        FirstDayOfMonth => Date { d: 1, ..d },
        LastDayOfMonth => Date { d: month_len(d.y as i64, d.m), ..d },
        FirstDayOfNextMonth => Date { d: 1, ..d }.plus_months(1)?,
        FirstDayOfYear => Date { m: 1, d: 1, ..d },
        LastDayOfYear => Date { m: 12, d: 31, ..d },
        FirstDayOfNextYear => Date { y: d.y + 1, m: 1, d: 1 },
        FirstInMonth(w) => apply_adjuster(d, &DayOfWeekInMonth(1, *w))?,
        LastInMonth(w) => apply_adjuster(d, &DayOfWeekInMonth(-1, *w))?,
        DayOfWeekInMonth(ord, w) => {
            let w = *w as i64;
            if *ord >= 0 {
                let first = Date { d: 1, ..d };
                let cur = first.day_of_week() as i64;
                let diff = (w - cur + 7) % 7;
                first.plus_days(diff + (*ord as i64 - 1).max(0) * 7 - if *ord == 0 { 7 } else { 0 })?
            } else {
                let last = Date { d: month_len(d.y as i64, d.m), ..d };
                let cur = last.day_of_week() as i64;
                let mut diff = w - cur;
                diff = if diff == 0 { 0 } else if diff > 0 { diff - 7 } else { diff };
                last.plus_days(diff - (-(*ord as i64) - 1) * 7)?
            }
        }
        Next(w) => {
            let diff = *w as i64 - dow;
            d.plus_days(if diff <= 0 { diff + 7 } else { diff })?
        }
        NextOrSame(w) => {
            let diff = *w as i64 - dow;
            d.plus_days(if diff < 0 { diff + 7 } else { diff })?
        }
        Previous(w) => {
            let diff = dow - *w as i64;
            d.plus_days(-(if diff <= 0 { diff + 7 } else { diff }))?
        }
        PreviousOrSame(w) => {
            let diff = dow - *w as i64;
            d.plus_days(-(if diff < 0 { diff + 7 } else { diff }))?
        }
    })
}

/// `X.from(TemporalAccessor)` conversions.
pub(crate) fn convert(target: ClassId, t: &TimeVal) -> Result<TimeVal, Thrown> {
    convert_desc(target, t, &t.describe())
}

/// `X.from(TemporalAccessor)` with `desc` naming the accessor in error messages (a parsed
/// formatter result describes itself as `{…},ISO … of type java.time.format.Parsed`). Failures
/// carry the same nested causes as the JDK's `from` methods.
pub(crate) fn convert_desc(target: ClassId, t: &TimeVal, desc: &str) -> Result<TimeVal, Thrown> {
    if let TimeVal::Parsed(p) = t {
        return convert_desc(target, &p.to_value(), desc);
    }
    let fail = |name: &str| dte(format!("Unable to obtain {name} from TemporalAccessor: {desc}"));
    let wrap = |name: &str, cause: Thrown| fail(name).with_cause(&cause);
    use TimeVal::*;
    let date = || -> Result<Date, Thrown> { to_local_date(t).map_err(|_| fail("LocalDate")) };
    let time = || -> Result<Time, Thrown> {
        match t {
            LocalTime(x) | LocalDateTime(_, x) | Zoned { t: x, .. } | OffsetDateTime { t: x, .. } | OffsetTime { t: x, .. } => Ok(*x),
            _ => Err(fail("LocalTime")),
        }
    };
    let offset = || -> Result<i32, Thrown> {
        match t {
            Zoned { offset, .. } | OffsetDateTime { offset, .. } | OffsetTime { offset, .. } | ZoneId(Zone::Offset(offset)) => Ok(*offset),
            _ => Err(fail("ZoneOffset")),
        }
    };
    let zone = || -> Result<Zone, Thrown> {
        match t {
            Zoned { zone, .. } | ZoneId(zone) => Ok(zone.clone()),
            OffsetDateTime { offset, .. } | OffsetTime { offset, .. } => Ok(Zone::Offset(*offset)),
            _ => Err(fail("ZoneId")),
        }
    };
    let field = |name: &str, f: Field| t.get_long(f).map_err(|e| wrap(name, e));
    Ok(match target {
        cls::INSTANT => match t.instant() {
            Some((s, n)) if !matches!(t, UtilDate(_)) => Instant { secs: s, nanos: n },
            _ => return Err(wrap("Instant", unsupported(format!("Unsupported field: {}", Field::InstantSeconds.name())))),
        },
        cls::LOCAL_DATE => LocalDate(date()?),
        cls::LOCAL_TIME => LocalTime(time()?),
        cls::LOCAL_DATE_TIME => {
            let d = date().map_err(|e| wrap("LocalDateTime", e))?;
            LocalDateTime(d, time().map_err(|e| wrap("LocalDateTime", e))?)
        }
        cls::ZONED_DATE_TIME => {
            let z = zone().map_err(|e| wrap("ZonedDateTime", e))?;
            match t {
                Zoned { .. } => t.clone(),
                OffsetDateTime { d, t: x, offset } => Zoned { d: *d, t: *x, offset: *offset, zone: z },
                _ => {
                    let d = date().map_err(|e| wrap("ZonedDateTime", e))?;
                    let x = time().map_err(|e| wrap("ZonedDateTime", e))?;
                    zoned(d, x, z, None)
                }
            }
        }
        cls::OFFSET_DATE_TIME => {
            let o = offset().map_err(|e| wrap("OffsetDateTime", e))?;
            let d = date().map_err(|e| wrap("OffsetDateTime", wrap("LocalDateTime", e)))?;
            let x = time().map_err(|e| wrap("OffsetDateTime", wrap("LocalDateTime", e)))?;
            OffsetDateTime { d, t: x, offset: o }
        }
        cls::OFFSET_TIME => {
            let o = offset().map_err(|e| wrap("OffsetTime", e))?;
            OffsetTime { t: time().map_err(|e| wrap("OffsetTime", e))?, offset: o }
        }
        cls::ZONE_ID => ZoneId(zone()?),
        cls::ZONE_OFFSET => ZoneId(Zone::Offset(offset()?)),
        cls::DAY_OF_WEEK => DayOfWeek(field("DayOfWeek", Field::DayOfWeek)? as u8),
        cls::MONTH => Month(field("Month", Field::MonthOfYear)? as u8),
        cls::YEAR => Year(field("Year", Field::Year)? as i32),
        cls::YEAR_MONTH => YearMonth(field("YearMonth", Field::Year)? as i32, field("YearMonth", Field::MonthOfYear)? as u8),
        cls::MONTH_DAY => MonthDay(field("MonthDay", Field::MonthOfYear)? as u8, field("MonthDay", Field::DayOfMonth)? as u8),
        _ => return Err(fail("value")),
    })
}

mod natives;
pub use natives::register;

#[cfg(test)]
mod tests;
