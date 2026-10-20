//! Dates, times, timestamps and intervals: a port of Postgres's `datetime.c` (ParseDateTime,
//! DecodeDateTime, DecodeTimeOnly, DecodeInterval, EncodeDateTime, EncodeInterval, ...) and the
//! core of `timestamp.c` / `date.c` (timestamp2tm, tm2timestamp, interval arithmetic, age).
//! Timestamps are microseconds since 2000-01-01 (Postgres's epoch); time zones are Postgres's
//! own tz library over the reference server's tzdata (`types::tz`).

use super::text::IntervalStyle;
use super::tz::{self, PgTz};
use super::value::{Interval, USECS_PER_DAY, USECS_PER_HOUR, USECS_PER_MINUTE, USECS_PER_SEC};
use crate::error::{Error, Result};
use crate::{bail, err};
use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use std::fmt::Write as _;
use std::sync::Arc;

/// Seconds between 1970-01-01 and 2000-01-01.
pub const UNIX_EPOCH_OFFSET_SECS: i64 = 946_684_800;
pub const DATE_NEG_INF: i32 = i32::MIN;
pub const DATE_POS_INF: i32 = i32::MAX;
pub const TS_NEG_INF: i64 = i64::MIN;
pub const TS_POS_INF: i64 = i64::MAX;
pub const MIN_TIMESTAMP: i64 = -211_813_488_000_000_000;
pub const END_TIMESTAMP: i64 = 9_223_371_331_200_000_000;
pub const POSTGRES_EPOCH_JDATE: i32 = 2_451_545;
pub const UNIX_EPOCH_JDATE: i32 = 2_440_588;
const DATETIME_MIN_JULIAN: i32 = 0;
const DATE_END_JULIAN: i32 = 2_147_483_494;
pub const TIMESTAMP_END_JULIAN: i32 = 109_203_528;
const SECS_PER_DAY: i64 = 86_400;
const MAX_TZDISP_HOUR: i32 = 15;
const MAXDATELEN: usize = 128;
const MAXDATEFIELDS: usize = 25;
const TOKMAXLEN: usize = 10;
const MAXTZLEN: usize = 10;
pub const MAX_TIMESTAMP_PRECISION: usize = 6;
pub const MAX_INTERVAL_PRECISION: usize = 6;
pub const INTERVAL_FULL_RANGE: i32 = 0x7FFF;
pub const INTERVAL_FULL_PRECISION: i32 = 0xFFFF;

pub const DAY_TAB: [[i32; 13]; 2] = [
    [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31, 0],
    [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31, 0],
];
pub const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
pub const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

// ── field types and token codes (datetime.h) ──

pub const RESERV: i32 = 0;
pub const MONTH: i32 = 1;
pub const YEAR: i32 = 2;
pub const DAY: i32 = 3;
pub const JULIAN: i32 = 4;
pub const TZ: i32 = 5;
pub const DTZ: i32 = 6;
pub const DYNTZ: i32 = 7;
pub const IGNORE_DTF: i32 = 8;
pub const AMPM: i32 = 9;
pub const HOUR: i32 = 10;
pub const MINUTE: i32 = 11;
pub const SECOND: i32 = 12;
pub const MILLISECOND: i32 = 13;
pub const MICROSECOND: i32 = 14;
pub const DOY: i32 = 15;
pub const DOW: i32 = 16;
pub const UNITS: i32 = 17;
pub const ADBC: i32 = 18;
pub const AGO: i32 = 19;
pub const ISOTIME: i32 = 23;
pub const WEEK: i32 = 24;
pub const DECADE: i32 = 25;
pub const CENTURY: i32 = 26;
pub const MILLENNIUM: i32 = 27;
pub const DTZMOD: i32 = 28;
pub const UNKNOWN_FIELD: i32 = 31;

pub const DTK_NUMBER: i32 = 0;
pub const DTK_STRING: i32 = 1;
pub const DTK_DATE: i32 = 2;
pub const DTK_TIME: i32 = 3;
pub const DTK_TZ: i32 = 4;
pub const DTK_AGO: i32 = 5;
pub const DTK_SPECIAL: i32 = 6;
pub const DTK_EARLY: i32 = 9;
pub const DTK_LATE: i32 = 10;
pub const DTK_EPOCH: i32 = 11;
pub const DTK_NOW: i32 = 12;
pub const DTK_YESTERDAY: i32 = 13;
pub const DTK_TODAY: i32 = 14;
pub const DTK_TOMORROW: i32 = 15;
pub const DTK_ZULU: i32 = 16;
pub const DTK_DELTA: i32 = 17;
pub const DTK_SECOND: i32 = 18;
pub const DTK_MINUTE: i32 = 19;
pub const DTK_HOUR: i32 = 20;
pub const DTK_DAY: i32 = 21;
pub const DTK_WEEK: i32 = 22;
pub const DTK_MONTH: i32 = 23;
pub const DTK_QUARTER: i32 = 24;
pub const DTK_YEAR: i32 = 25;
pub const DTK_DECADE: i32 = 26;
pub const DTK_CENTURY: i32 = 27;
pub const DTK_MILLENNIUM: i32 = 28;
pub const DTK_MILLISEC: i32 = 29;
pub const DTK_MICROSEC: i32 = 30;
pub const DTK_JULIAN: i32 = 31;
pub const DTK_DOW: i32 = 32;
pub const DTK_DOY: i32 = 33;
pub const DTK_TZ_HOUR: i32 = 34;
pub const DTK_TZ_MINUTE: i32 = 35;
pub const DTK_ISOYEAR: i32 = 36;
pub const DTK_ISODOW: i32 = 37;

const AM: i32 = 0;
const PM: i32 = 1;
const HR24: i32 = 2;
const BC: i32 = 1;

pub const fn dtk_m(t: i32) -> i32 {
    1 << t
}
const DTK_ALL_SECS_M: i32 = dtk_m(SECOND) | dtk_m(MILLISECOND) | dtk_m(MICROSECOND);
pub const DTK_DATE_M: i32 = dtk_m(YEAR) | dtk_m(MONTH) | dtk_m(DAY);
pub const DTK_TIME_M: i32 = dtk_m(HOUR) | dtk_m(MINUTE) | DTK_ALL_SECS_M;
const fn interval_mask(t: i32) -> i32 {
    1 << t
}

/// A `datetkn` table entry.
struct Tkn {
    token: &'static str,
    ty: i32,
    value: i32,
}

const fn tk(token: &'static str, ty: i32, value: i32) -> Tkn {
    Tkn { token, ty, value }
}

static DATETKTBL: &[Tkn] = &[
    tk("+infinity", RESERV, DTK_LATE),
    tk("-infinity", RESERV, DTK_EARLY),
    tk("ad", ADBC, 0),
    tk("allballs", RESERV, DTK_ZULU),
    tk("am", AMPM, AM),
    tk("apr", MONTH, 4),
    tk("april", MONTH, 4),
    tk("at", IGNORE_DTF, 0),
    tk("aug", MONTH, 8),
    tk("august", MONTH, 8),
    tk("bc", ADBC, BC),
    tk("d", UNITS, DTK_DAY),
    tk("dec", MONTH, 12),
    tk("december", MONTH, 12),
    tk("dow", UNITS, DTK_DOW),
    tk("doy", UNITS, DTK_DOY),
    tk("dst", DTZMOD, 3600),
    tk("epoch", RESERV, DTK_EPOCH),
    tk("feb", MONTH, 2),
    tk("february", MONTH, 2),
    tk("fri", DOW, 5),
    tk("friday", DOW, 5),
    tk("h", UNITS, DTK_HOUR),
    tk("infinity", RESERV, DTK_LATE),
    tk("isodow", UNITS, DTK_ISODOW),
    tk("isoyear", UNITS, DTK_ISOYEAR),
    tk("j", UNITS, DTK_JULIAN),
    tk("jan", MONTH, 1),
    tk("january", MONTH, 1),
    tk("jd", UNITS, DTK_JULIAN),
    tk("jul", MONTH, 7),
    tk("julian", UNITS, DTK_JULIAN),
    tk("july", MONTH, 7),
    tk("jun", MONTH, 6),
    tk("june", MONTH, 6),
    tk("m", UNITS, DTK_MONTH),
    tk("mar", MONTH, 3),
    tk("march", MONTH, 3),
    tk("may", MONTH, 5),
    tk("mm", UNITS, DTK_MINUTE),
    tk("mon", DOW, 1),
    tk("monday", DOW, 1),
    tk("nov", MONTH, 11),
    tk("november", MONTH, 11),
    tk("now", RESERV, DTK_NOW),
    tk("oct", MONTH, 10),
    tk("october", MONTH, 10),
    tk("on", IGNORE_DTF, 0),
    tk("pm", AMPM, PM),
    tk("s", UNITS, DTK_SECOND),
    tk("sat", DOW, 6),
    tk("saturday", DOW, 6),
    tk("sep", MONTH, 9),
    tk("sept", MONTH, 9),
    tk("september", MONTH, 9),
    tk("sun", DOW, 0),
    tk("sunday", DOW, 0),
    tk("t", ISOTIME, DTK_TIME),
    tk("thu", DOW, 4),
    tk("thur", DOW, 4),
    tk("thurs", DOW, 4),
    tk("thursday", DOW, 4),
    tk("today", RESERV, DTK_TODAY),
    tk("tomorrow", RESERV, DTK_TOMORROW),
    tk("tue", DOW, 2),
    tk("tues", DOW, 2),
    tk("tuesday", DOW, 2),
    tk("wed", DOW, 3),
    tk("wednesday", DOW, 3),
    tk("weds", DOW, 3),
    tk("y", UNITS, DTK_YEAR),
    tk("yesterday", RESERV, DTK_YESTERDAY),
];

static DELTATKTBL: &[Tkn] = &[
    tk("@", IGNORE_DTF, 0),
    tk("ago", AGO, 0),
    tk("c", UNITS, DTK_CENTURY),
    tk("cent", UNITS, DTK_CENTURY),
    tk("centuries", UNITS, DTK_CENTURY),
    tk("century", UNITS, DTK_CENTURY),
    tk("d", UNITS, DTK_DAY),
    tk("day", UNITS, DTK_DAY),
    tk("days", UNITS, DTK_DAY),
    tk("dec", UNITS, DTK_DECADE),
    tk("decade", UNITS, DTK_DECADE),
    tk("decades", UNITS, DTK_DECADE),
    tk("decs", UNITS, DTK_DECADE),
    tk("h", UNITS, DTK_HOUR),
    tk("hour", UNITS, DTK_HOUR),
    tk("hours", UNITS, DTK_HOUR),
    tk("hr", UNITS, DTK_HOUR),
    tk("hrs", UNITS, DTK_HOUR),
    tk("m", UNITS, DTK_MINUTE),
    tk("microsecon", UNITS, DTK_MICROSEC),
    tk("mil", UNITS, DTK_MILLENNIUM),
    tk("millennia", UNITS, DTK_MILLENNIUM),
    tk("millennium", UNITS, DTK_MILLENNIUM),
    tk("millisecon", UNITS, DTK_MILLISEC),
    tk("mils", UNITS, DTK_MILLENNIUM),
    tk("min", UNITS, DTK_MINUTE),
    tk("mins", UNITS, DTK_MINUTE),
    tk("minute", UNITS, DTK_MINUTE),
    tk("minutes", UNITS, DTK_MINUTE),
    tk("mon", UNITS, DTK_MONTH),
    tk("mons", UNITS, DTK_MONTH),
    tk("month", UNITS, DTK_MONTH),
    tk("months", UNITS, DTK_MONTH),
    tk("ms", UNITS, DTK_MILLISEC),
    tk("msec", UNITS, DTK_MILLISEC),
    tk("msecond", UNITS, DTK_MILLISEC),
    tk("mseconds", UNITS, DTK_MILLISEC),
    tk("msecs", UNITS, DTK_MILLISEC),
    tk("qtr", UNITS, DTK_QUARTER),
    tk("quarter", UNITS, DTK_QUARTER),
    tk("s", UNITS, DTK_SECOND),
    tk("sec", UNITS, DTK_SECOND),
    tk("second", UNITS, DTK_SECOND),
    tk("seconds", UNITS, DTK_SECOND),
    tk("secs", UNITS, DTK_SECOND),
    tk("timezone", UNITS, DTK_TZ),
    tk("timezone_h", UNITS, DTK_TZ_HOUR),
    tk("timezone_m", UNITS, DTK_TZ_MINUTE),
    tk("us", UNITS, DTK_MICROSEC),
    tk("usec", UNITS, DTK_MICROSEC),
    tk("usecond", UNITS, DTK_MICROSEC),
    tk("useconds", UNITS, DTK_MICROSEC),
    tk("usecs", UNITS, DTK_MICROSEC),
    tk("w", UNITS, DTK_WEEK),
    tk("week", UNITS, DTK_WEEK),
    tk("weeks", UNITS, DTK_WEEK),
    tk("y", UNITS, DTK_YEAR),
    tk("year", UNITS, DTK_YEAR),
    tk("years", UNITS, DTK_YEAR),
    tk("yr", UNITS, DTK_YEAR),
    tk("yrs", UNITS, DTK_YEAR),
];

/// `strncmp(key, token, TOKMAXLEN)`.
fn tok_cmp(key: &[u8], tok: &[u8]) -> std::cmp::Ordering {
    for i in 0..TOKMAXLEN {
        let a = key.get(i).copied().unwrap_or(0);
        let b = tok.get(i).copied().unwrap_or(0);
        if a != b {
            return a.cmp(&b);
        }
        if a == 0 {
            break;
        }
    }
    std::cmp::Ordering::Equal
}

fn datebsearch<'a>(key: &[u8], base: &'a [Tkn]) -> Option<&'a Tkn> {
    let (mut lo, mut hi) = (0usize, base.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        match tok_cmp(key, base[mid].token.as_bytes()) {
            std::cmp::Ordering::Equal => return Some(&base[mid]),
            std::cmp::Ordering::Less => hi = mid,
            std::cmp::Ordering::Greater => lo = mid + 1,
        }
    }
    None
}

/// DecodeSpecial: (type, value) of a lower-case keyword.
pub fn decode_special(lowtoken: &[u8]) -> (i32, i32) {
    match datebsearch(lowtoken, DATETKTBL) {
        Some(t) => (t.ty, t.value),
        None => (UNKNOWN_FIELD, 0),
    }
}

/// DecodeUnits: (type, value) of a lower-case unit name.
pub fn decode_units(lowtoken: &[u8]) -> (i32, i32) {
    match datebsearch(lowtoken, DELTATKTBL) {
        Some(t) => (t.ty, t.value),
        None => (UNKNOWN_FIELD, 0),
    }
}

// ── calendar ──

/// date2j: Julian day number of a (proleptic Gregorian) date.
pub fn date2j(year: i32, month: i32, day: i32) -> i32 {
    let (mut year, mut month) = (year, month);
    if month > 2 {
        month = month.wrapping_add(1);
        year = year.wrapping_add(4800);
    } else {
        month = month.wrapping_add(13);
        year = year.wrapping_add(4799);
    }
    let century = year / 100;
    let mut julian = year.wrapping_mul(365).wrapping_sub(32167);
    julian = julian
        .wrapping_add(year / 4)
        .wrapping_sub(century)
        .wrapping_add(century / 4);
    julian = julian
        .wrapping_add(7834i32.wrapping_mul(month) / 256)
        .wrapping_add(day);
    julian
}

/// j2date: (year, month, day) of a Julian day number.
pub fn j2date(jd: i32) -> (i32, i32, i32) {
    let mut julian: u32 = jd as u32;
    julian = julian.wrapping_add(32044);
    let mut quad = julian / 146_097;
    let extra = (julian - quad * 146_097) * 4 + 3;
    julian = julian.wrapping_add(60 + quad * 3 + extra / 146_097);
    quad = julian / 1461;
    julian -= quad * 1461;
    let mut y = (julian * 4 / 1461) as i32;
    julian = if y != 0 {
        (julian + 305) % 365
    } else {
        (julian + 306) % 366
    } + 123;
    y = (y as u32).wrapping_add(quad.wrapping_mul(4)) as i32;
    let year = y.wrapping_sub(4800);
    quad = julian * 2141 / 65536;
    let day = julian.wrapping_sub(7834 * quad / 256) as i32;
    let month = ((quad + 10) % 12 + 1) as i32;
    (year, month, day)
}

/// j2day: day of week (0 = Sunday) of a Julian day number.
pub fn j2day(date: i32) -> i32 {
    let d = (date as i64 + 1) % 7;
    (if d < 0 { d + 7 } else { d }) as i32
}

pub fn isleap(y: i32) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

pub fn is_valid_julian(y: i32, m: i32, _d: i32) -> bool {
    (y > -4713 || (y == -4713 && m >= 11)) && (y < 5_874_898 || (y == 5_874_898 && m < 6))
}

/// IS_VALID_DATE (Postgres day numbering).
pub fn is_valid_date(d: i32) -> bool {
    ((DATETIME_MIN_JULIAN - POSTGRES_EPOCH_JDATE)..(DATE_END_JULIAN - POSTGRES_EPOCH_JDATE)).contains(&d)
}

/// IS_VALID_TIMESTAMP.
pub fn ts_is_valid(ts: i64) -> bool {
    (MIN_TIMESTAMP..END_TIMESTAMP).contains(&ts)
}

pub fn ts_is_finite(ts: i64) -> bool {
    ts != TS_NEG_INF && ts != TS_POS_INF
}

pub fn date_is_finite(d: i32) -> bool {
    d != DATE_NEG_INF && d != DATE_POS_INF
}

pub fn date_to_naive(d: i32) -> Option<NaiveDate> {
    NaiveDate::from_num_days_from_ce_opt(d.checked_add(730_120)?)
}

pub fn naive_to_date(n: NaiveDate) -> i32 {
    n.num_days_from_ce() - 730_120
}

/// (year, month, day) → days since 2000-01-01. `year` is astronomical (0 = 1 BC).
pub fn ymd_to_date(y: i32, m: u32, d: u32) -> Option<i32> {
    if !(1..=12).contains(&m) || d < 1 || d as i32 > DAY_TAB[isleap(y) as usize][m as usize - 1] {
        return None;
    }
    if !is_valid_julian(y, m as i32, d as i32) {
        return None;
    }
    Some(date2j(y, m as i32, d as i32) - POSTGRES_EPOCH_JDATE)
}

pub fn date_to_ymd(d: i32) -> (i32, u32, u32) {
    let (y, m, day) = j2date(d.wrapping_add(POSTGRES_EPOCH_JDATE));
    (y, m as u32, day as u32)
}

/// Split a timestamp into (days since epoch, microseconds within the day).
pub fn split_ts(ts: i64) -> (i64, i64) {
    (ts.div_euclid(USECS_PER_DAY), ts.rem_euclid(USECS_PER_DAY))
}

pub fn ts_to_naive(ts: i64) -> Option<NaiveDateTime> {
    let (days, us) = split_ts(ts);
    let date = date_to_naive(i32::try_from(days).ok()?)?;
    let secs = (us / USECS_PER_SEC) as u32;
    let nanos = ((us % USECS_PER_SEC) * 1000) as u32;
    Some(date.and_time(NaiveTime::from_num_seconds_from_midnight_opt(secs, nanos)?))
}

pub fn naive_to_ts(n: NaiveDateTime) -> i64 {
    let days = naive_to_date(n.date()) as i64;
    let t = n.time();
    days * USECS_PER_DAY
        + t.num_seconds_from_midnight() as i64 * USECS_PER_SEC
        + (t.nanosecond() / 1000) as i64
}

pub fn days_in_month(y: i32, m: u32) -> u32 {
    DAY_TAB[isleap(y) as usize][(m as usize).clamp(1, 12) - 1] as u32
}

pub fn is_leap(y: i32) -> bool {
    isleap(y)
}

fn ts_range_check(ts: i64) -> Result<i64> {
    if !ts_is_valid(ts) {
        bail!(DATETIME_FIELD_OVERFLOW, "timestamp out of range");
    }
    Ok(ts)
}

/// Day of week of a date, 0 = Sunday.
pub fn dow(date: i32) -> u32 {
    j2day(date.wrapping_add(POSTGRES_EPOCH_JDATE)) as u32
}

pub fn iso_week(date: i32) -> (i32, u32) {
    let (y, m, d) = date_to_ymd(date);
    (
        date2isoyear(y, m as i32, d as i32),
        date2isoweek(y, m as i32, d as i32) as u32,
    )
}

pub fn doy(date: i32) -> u32 {
    let (y, _, _) = date_to_ymd(date);
    (date
        .wrapping_add(POSTGRES_EPOCH_JDATE)
        .wrapping_sub(date2j(y, 1, 1))
        + 1) as u32
}

/// Unix epoch seconds of a timestamp, as a float.
pub fn ts_epoch(ts: i64) -> f64 {
    (ts as f64) / USECS_PER_SEC as f64 + UNIX_EPOCH_OFFSET_SECS as f64
}

// ── ISO weeks (timestamp.c) ──

/// date2isoweek.
pub fn date2isoweek(year: i32, mon: i32, mday: i32) -> i32 {
    let dayn = date2j(year, mon, mday);
    let mut day4 = date2j(year, 1, 4);
    let mut day0 = j2day(day4 - 1);
    if dayn < day4 - day0 {
        day4 = date2j(year - 1, 1, 4);
        day0 = j2day(day4 - 1);
    }
    let mut result = (dayn - (day4 - day0)) / 7 + 1;
    if result >= 52 {
        let day4 = date2j(year + 1, 1, 4);
        let day0 = j2day(day4 - 1);
        if dayn >= day4 - day0 {
            result = (dayn - (day4 - day0)) / 7 + 1;
        }
    }
    result
}

/// date2isoyear.
pub fn date2isoyear(year: i32, mon: i32, mday: i32) -> i32 {
    let mut year = year;
    let dayn = date2j(year, mon, mday);
    let mut day4 = date2j(year, 1, 4);
    let mut day0 = j2day(day4 - 1);
    if dayn < day4 - day0 {
        day4 = date2j(year - 1, 1, 4);
        day0 = j2day(day4 - 1);
        year -= 1;
    }
    let result = (dayn - (day4 - day0)) / 7 + 1;
    if result >= 52 {
        let day4 = date2j(year + 1, 1, 4);
        let day0 = j2day(day4 - 1);
        if dayn >= day4 - day0 {
            year += 1;
        }
    }
    year
}

/// date2isoyearday.
pub fn date2isoyearday(year: i32, mon: i32, mday: i32) -> i32 {
    date2j(year, mon, mday) - isoweek2j(date2isoyear(year, mon, mday), 1) + 1
}

/// isoweek2j: Julian day of the Monday of ISO week `week` of ISO year `year`.
pub fn isoweek2j(year: i32, week: i32) -> i32 {
    let day4 = date2j(year, 1, 4);
    let day0 = j2day(day4 - 1);
    ((week - 1) * 7) + (day4 - day0)
}

/// isoweek2date: (year, mon, mday) of the Monday of an ISO week.
pub fn isoweek2date(woy: i32, year: i32) -> (i32, i32, i32) {
    j2date(isoweek2j(year, woy))
}

/// isoweekdate2date: ISO week + ISO weekday (1 = Monday .. 7).
pub fn isoweekdate2date(isoweek: i32, wday: i32, year: i32) -> (i32, i32, i32) {
    let mut jday = isoweek2j(year, isoweek);
    if wday > 1 {
        jday += wday - 2;
    } else {
        jday += 6;
    }
    j2date(jday)
}

// ── time zones ──

/// A session time zone (a Postgres `pg_tz`).
#[derive(Clone)]
pub struct TimeZone(pub Arc<PgTz>);

impl PartialEq for TimeZone {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || self.0.name == other.0.name
    }
}

impl std::fmt::Debug for TimeZone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TimeZone({})", self.0.name)
    }
}

impl Default for TimeZone {
    fn default() -> Self {
        TimeZone::parse("UTC").unwrap_or_else(TimeZone::gmt)
    }
}

impl TimeZone {
    pub fn gmt() -> TimeZone {
        TimeZone(tz::pg_tzset("GMT").expect("GMT zone"))
    }

    /// The `TimeZone` setting's check hook (check_timezone): `INTERVAL '...'`, a number of
    /// hours, or a zone name / POSIX spec.
    pub fn parse(name: &str) -> Option<TimeZone> {
        check_timezone(name, IntervalStyle::Postgres).ok()
    }

    /// pg_tzset_offset: `gmtoff` seconds west of UTC (POSIX sign convention).
    pub fn from_gmtoff(gmtoff: i64) -> Option<TimeZone> {
        tz::pg_tzset_offset(gmtoff).map(TimeZone)
    }

    /// A fixed zone `east` seconds east of UTC.
    pub fn fixed(east: i32) -> TimeZone {
        Self::from_gmtoff(-(east as i64)).unwrap_or_else(TimeZone::gmt)
    }

    pub fn pg(&self) -> &PgTz {
        &self.0
    }

    /// The canonical name (what SHOW TimeZone prints).
    pub fn name(&self) -> String {
        self.0.name.clone()
    }

    /// Local time at a UTC instant (Postgres microseconds), as pg_localtime gives it.
    pub fn localtime(&self, ts: i64) -> Option<tz::LocalTm> {
        let secs = ts.div_euclid(USECS_PER_SEC) + UNIX_EPOCH_OFFSET_SECS;
        tz::pg_localtime(secs, &self.0)
    }

    /// Offset (seconds east of UTC) in effect at a UTC instant (Postgres microseconds).
    pub fn offset_at_utc(&self, ts: i64) -> i32 {
        self.localtime(ts).map_or(0, |t| t.gmtoff)
    }

    /// Offset (seconds east) for a local wall-clock time (DetermineTimeZoneOffset).
    pub fn offset_for_local(&self, local: i64) -> i32 {
        match timestamp2tm(local, None) {
            Some((tm, _)) => -determine_tz_offset(&tm, self).0,
            None => 0,
        }
    }

    /// Abbreviation at an instant (for to_char's TZ).
    pub fn abbrev_at_utc(&self, ts: i64) -> String {
        self.localtime(ts).map(|t| t.zone).unwrap_or_default()
    }
}

/// check_timezone: the zone a `TimeZone` setting names, or the error SET reports.
pub fn check_timezone(name: &str, style: IntervalStyle) -> Result<TimeZone> {
    let invalid = || {
        err!(
            INVALID_PARAMETER_VALUE,
            "invalid value for parameter \"TimeZone\": \"{name}\""
        )
    };
    let out_of_range = || invalid().detail("UTC timezone offset is out of range.");
    if name.len() >= 8 && name.is_char_boundary(8) && name[..8].eq_ignore_ascii_case("interval") {
        let v = name[8..].trim_start();
        let v = v.strip_prefix('\'').ok_or_else(invalid)?;
        let end = v.find('\'').ok_or_else(invalid)?;
        if end + 1 != v.len() {
            return Err(invalid());
        }
        let iv = parse_interval_ex(&v[..end], style, -1)?;
        if iv.months != 0 {
            return Err(invalid().detail("Cannot specify months in time zone interval."));
        }
        if iv.days != 0 {
            return Err(invalid().detail("Cannot specify days in time zone interval."));
        }
        return TimeZone::from_gmtoff(-(iv.micros / USECS_PER_SEC)).ok_or_else(out_of_range);
    }
    if let Some((h, rest)) = strtod_prefix(name.as_bytes(), 0)
        && rest == name.len()
    {
        return TimeZone::from_gmtoff((-h * 3600.0) as i64).ok_or_else(out_of_range);
    }
    let tz = tz::pg_tzset(name).ok_or_else(invalid)?;
    if !tz::pg_tz_acceptable(&tz) {
        return Err(err!(
            INVALID_PARAMETER_VALUE,
            "time zone \"{name}\" appears to use leap seconds"
        )
        .detail("PostgreSQL does not support leap seconds."));
    }
    Ok(TimeZone(tz))
}

/// DecodeTimezoneNameToTz (AT TIME ZONE, date_trunc's zone, ...): an abbreviation, then a
/// zone name; "time zone ... not recognized" otherwise.
pub fn decode_timezone_name_to_tz(name: &str, cx: &DtCtx) -> Result<TimeZone> {
    let mut low = name.to_lowercase();
    if low.len() > 63 {
        let mut n = 63;
        while !low.is_char_boundary(n) {
            n -= 1;
        }
        low.truncate(n);
    }
    let (ty, off, z) = decode_timezone_abbrev(low.as_bytes(), cx)
        .map_err(|e| dt_parse_error(e, name, ""))?;
    match ty {
        TZ | DTZ => Ok(TimeZone::from_gmtoff(-(off as i64)).unwrap_or_else(TimeZone::gmt)),
        DYNTZ => Ok(z.unwrap_or_else(TimeZone::gmt)),
        _ => match tz::pg_tzset(name) {
            Some(t) => Ok(TimeZone(t)),
            None => bail!(INVALID_PARAMETER_VALUE, "time zone \"{name}\" not recognized"),
        },
    }
}

/// DecodeTimezoneName: Fixed(offset east), Dynamic(abbrev zone) or Zone.
pub enum TzName {
    Fixed(i32),
    Dynamic(TimeZone),
    Zone(TimeZone),
}

pub fn decode_timezone_name(name: &str, cx: &DtCtx) -> Result<TzName> {
    let low = name.to_lowercase();
    let (ty, off, z) = decode_timezone_abbrev(low.as_bytes(), cx)
        .map_err(|e| dt_parse_error(e, name, ""))?;
    match ty {
        TZ | DTZ => Ok(TzName::Fixed(off)),
        DYNTZ => Ok(TzName::Dynamic(z.unwrap_or_else(TimeZone::gmt))),
        _ => match tz::pg_tzset(name) {
            Some(t) => Ok(TzName::Zone(TimeZone(t))),
            None => bail!(INVALID_PARAMETER_VALUE, "time zone \"{name}\" not recognized"),
        },
    }
}

/// `+05`, `-08:00`, `+0530`, `+05:30:15` → seconds east of UTC.
pub fn parse_offset(s: &str) -> Option<i32> {
    decode_timezone(s.trim().as_bytes()).ok().map(|w| -w)
}

/// Seconds east of UTC → `+05`, `-08`, `+05:30`, `+05:30:15`.
pub fn format_offset(east: i32) -> String {
    let mut s = String::new();
    encode_timezone(&mut s, -east, DateFmt::Iso);
    s
}

/// A fixed-offset abbreviation from the default abbreviation set (seconds east).
pub fn abbrev_offset(lower: &str) -> Option<i32> {
    let tbl = tz::abbrev_table("Default")?;
    let key = lower.as_bytes();
    let i = tbl
        .binary_search_by(|e| tok_cmp(e.token.as_bytes(), key))
        .ok()?;
    match tbl[i].kind {
        tz::AbbrevKind::Fixed(o, _) => Some(o),
        tz::AbbrevKind::Dynamic(_) => None,
    }
}

// ── DateStyle ──

/// Field order for ambiguous numeric dates (the second half of DateStyle).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DateOrder {
    #[default]
    Mdy,
    Dmy,
    Ymd,
}

/// Output format (the first half of DateStyle), plus XSD for xml.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DateFmt {
    #[default]
    Iso,
    Sql,
    Postgres,
    German,
    Xsd,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DateStyle {
    pub fmt: DateFmt,
    pub order: DateOrder,
}

impl DateStyle {
    /// From the normalised setting, e.g. "SQL, DMY".
    pub fn parse(s: &str) -> DateStyle {
        let mut d = DateStyle::default();
        for part in s.split(',').map(|p| p.trim().to_ascii_lowercase()) {
            match part.as_str() {
                "sql" => d.fmt = DateFmt::Sql,
                "postgres" => d.fmt = DateFmt::Postgres,
                "german" => d.fmt = DateFmt::German,
                "dmy" => d.order = DateOrder::Dmy,
                "ymd" => d.order = DateOrder::Ymd,
                _ => {}
            }
        }
        d
    }
}

// ── broken-down times ──

/// `struct pg_tm` with a full year and 1-based month.
#[derive(Clone, Debug, Default)]
pub struct Tm {
    pub year: i32,
    pub mon: i32,
    pub mday: i32,
    pub hour: i32,
    pub min: i32,
    pub sec: i32,
    pub wday: i32,
    pub yday: i32,
    /// -1 unknown, 0 standard, 1 DST.
    pub isdst: i32,
    /// Seconds east of UTC.
    pub gmtoff: i64,
    pub zone: Option<String>,
}

/// `struct pg_itm`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Itm {
    pub usec: i32,
    pub sec: i32,
    pub min: i32,
    pub hour: i64,
    pub mday: i32,
    pub mon: i32,
    pub year: i32,
}

/// `struct pg_itm_in`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ItmIn {
    pub usec: i64,
    pub mday: i32,
    pub mon: i32,
    pub year: i32,
}

/// dt2time: (hour, min, sec, fsec) of a time-of-day in microseconds.
pub fn dt2time(jd: i64) -> (i32, i32, i32, i32) {
    let mut time = jd;
    let hour = time / USECS_PER_HOUR;
    time -= hour * USECS_PER_HOUR;
    let min = time / USECS_PER_MINUTE;
    time -= min * USECS_PER_MINUTE;
    let sec = time / USECS_PER_SEC;
    let fsec = time - sec * USECS_PER_SEC;
    (hour as i32, min as i32, sec as i32, fsec as i32)
}

pub fn time2t(hour: i32, min: i32, sec: i32, fsec: i32) -> i64 {
    (((hour as i64 * 60 + min as i64) * 60) + sec as i64) * USECS_PER_SEC + fsec as i64
}

/// timestamp2tm. With `zone`, the result is local time there (`tm.gmtoff`, `tm.zone`,
/// `tm.isdst` set); without, plain fields with `isdst = -1`. None = out of range.
pub fn timestamp2tm(dt: i64, zone: Option<&TimeZone>) -> Option<(Tm, i32)> {
    let mut time = dt;
    let mut date = time / USECS_PER_DAY;
    if date != 0 {
        time -= date * USECS_PER_DAY;
    }
    if time < 0 {
        time += USECS_PER_DAY;
        date -= 1;
    }
    date += POSTGRES_EPOCH_JDATE as i64;
    if date < 0 || date > i32::MAX as i64 {
        return None;
    }
    let (year, mon, mday) = j2date(date as i32);
    let (hour, min, sec, fsec) = dt2time(time);
    let mut tm = Tm {
        year,
        mon,
        mday,
        hour,
        min,
        sec,
        isdst: -1,
        ..Default::default()
    };
    let Some(zone) = zone else {
        return Some((tm, fsec));
    };
    let utime = (dt - fsec as i64) / USECS_PER_SEC + UNIX_EPOCH_OFFSET_SECS;
    match tz::pg_localtime(utime, &zone.0) {
        Some(tx) => {
            tm.year = tx.year;
            tm.mon = tx.mon;
            tm.mday = tx.mday;
            tm.hour = tx.hour;
            tm.min = tx.min;
            tm.sec = tx.sec;
            tm.isdst = tx.isdst as i32;
            tm.gmtoff = tx.gmtoff as i64;
            tm.zone = Some(tx.zone);
        }
        None => {
            tm.isdst = -1;
            tm.gmtoff = 0;
        }
    }
    Some((tm, fsec))
}

/// tm2timestamp; `tzp` is the zone in seconds west of UTC.
pub fn tm2timestamp(tm: &Tm, fsec: i32, tzp: Option<i32>) -> Option<i64> {
    if !is_valid_julian(tm.year, tm.mon, tm.mday) {
        return None;
    }
    let date = (date2j(tm.year, tm.mon, tm.mday) - POSTGRES_EPOCH_JDATE) as i64;
    let time = time2t(tm.hour, tm.min, tm.sec, fsec);
    let mut result = date.checked_mul(USECS_PER_DAY)?.checked_add(time)?;
    if let Some(tz) = tzp {
        result = result.checked_add(tz as i64 * USECS_PER_SEC)?;
    }
    if !ts_is_valid(result) {
        return None;
    }
    Some(result)
}

/// interval2itm.
pub fn interval2itm(span: &Interval) -> Itm {
    let mut time = span.micros;
    let hour = time / USECS_PER_HOUR;
    time -= hour * USECS_PER_HOUR;
    let min = time / USECS_PER_MINUTE;
    time -= min * USECS_PER_MINUTE;
    let sec = time / USECS_PER_SEC;
    time -= sec * USECS_PER_SEC;
    Itm {
        year: span.months / 12,
        mon: span.months % 12,
        mday: span.days,
        hour,
        min: min as i32,
        sec: sec as i32,
        usec: time as i32,
    }
}

/// itm2interval; None on overflow (or an infinite result).
pub fn itm2interval(itm: &Itm) -> Option<Interval> {
    let total = itm.year as i64 * 12 + itm.mon as i64;
    if total > i32::MAX as i64 || total < i32::MIN as i64 {
        return None;
    }
    let mut time = itm.hour.checked_mul(USECS_PER_HOUR)?;
    time = time.checked_add(itm.min as i64 * USECS_PER_MINUTE)?;
    time = time.checked_add(itm.sec as i64 * USECS_PER_SEC)?;
    time = time.checked_add(itm.usec as i64)?;
    let iv = Interval::new(total as i32, itm.mday, time);
    if iv.is_infinite() {
        return None;
    }
    Some(iv)
}

/// itmin2interval.
pub fn itmin2interval(itm: &ItmIn) -> Option<Interval> {
    let total = itm.year as i64 * 12 + itm.mon as i64;
    if total > i32::MAX as i64 || total < i32::MIN as i64 {
        return None;
    }
    Some(Interval::new(total as i32, itm.mday, itm.usec))
}

// ── errors ──

/// A DTERR code.
#[derive(Clone, Debug, PartialEq)]
pub enum DtErr {
    BadFormat,
    FieldOverflow,
    MdFieldOverflow,
    IntervalOverflow,
    TzdispOverflow,
    BadTimezone(String),
    BadZoneAbbrev(String, String),
}

/// DateTimeParseError.
pub fn dt_parse_error(e: DtErr, s: &str, datatype: &str) -> Error {
    match e {
        DtErr::FieldOverflow => err!(
            DATETIME_FIELD_OVERFLOW,
            "date/time field value out of range: \"{s}\""
        ),
        DtErr::MdFieldOverflow => err!(
            DATETIME_FIELD_OVERFLOW,
            "date/time field value out of range: \"{s}\""
        )
        .hint("Perhaps you need a different \"DateStyle\" setting."),
        DtErr::IntervalOverflow => err!(
            INTERVAL_FIELD_OVERFLOW,
            "interval field value out of range: \"{s}\""
        ),
        DtErr::TzdispOverflow => err!(
            INVALID_TIME_ZONE_DISPLACEMENT_VALUE,
            "time zone displacement out of range: \"{s}\""
        ),
        DtErr::BadTimezone(z) => err!(INVALID_PARAMETER_VALUE, "time zone \"{z}\" not recognized"),
        DtErr::BadZoneAbbrev(z, a) => err!(CONFIG_FILE_ERROR, "time zone \"{z}\" not recognized")
            .detail(format!(
                "This time zone name appears in the configuration file for time zone abbreviation \"{a}\"."
            )),
        DtErr::BadFormat => err!(
            INVALID_DATETIME_FORMAT,
            "invalid input syntax for type {datatype}: \"{s}\""
        ),
    }
}

type DtResult<T> = std::result::Result<T, DtErr>;

// ── C numeric parsing helpers ──

/// strtol over bytes from `i`: (value, end, overflowed). `end == i` when no digits.
fn strtol(s: &[u8], i: usize) -> (i64, usize, bool) {
    let mut p = i;
    while p < s.len() && matches!(s[p], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
        p += 1;
    }
    let mut neg = false;
    if p < s.len() && (s[p] == b'+' || s[p] == b'-') {
        neg = s[p] == b'-';
        p += 1;
    }
    let start = p;
    let mut v: i128 = 0;
    let mut over = false;
    while p < s.len() && s[p].is_ascii_digit() {
        if !over {
            v = v * 10 + (s[p] - b'0') as i128;
            if v > i64::MAX as i128 + 1 {
                over = true;
            }
        }
        p += 1;
    }
    if p == start {
        return (0, i, false);
    }
    if neg {
        v = -v;
    }
    if over || v > i64::MAX as i128 || v < i64::MIN as i128 {
        return (if neg { i64::MIN } else { i64::MAX }, p, true);
    }
    (v as i64, p, false)
}

/// strtoint: as strtol but out-of-int-range is ERANGE.
fn strtoint(s: &[u8], i: usize) -> (i32, usize, bool) {
    let (v, end, over) = strtol(s, i);
    if over || v > i32::MAX as i64 || v < i32::MIN as i64 {
        return (if v < 0 { i32::MIN } else { i32::MAX }, end, true);
    }
    (v as i32, end, false)
}

/// atoi on a digit string (wrapping like C).
fn atoi(s: &[u8]) -> i32 {
    let (v, _, _) = strtol(s, 0);
    v as i32
}

/// strtod prefix: (value, end). Accepts what C's strtod does for decimal input, inf and nan.
pub fn strtod_prefix(s: &[u8], i: usize) -> Option<(f64, usize)> {
    let mut p = i;
    while p < s.len() && s[p].is_ascii_whitespace() {
        p += 1;
    }
    let start = p;
    if p < s.len() && (s[p] == b'+' || s[p] == b'-') {
        p += 1;
    }
    let rest = &s[p..];
    let lower: Vec<u8> = rest.iter().take(8).map(|b| b.to_ascii_lowercase()).collect();
    if lower.starts_with(b"infinity") || lower.starts_with(b"inf") {
        let n = if lower.starts_with(b"infinity") { 8 } else { 3 };
        let neg = s[start] == b'-';
        return Some((if neg { f64::NEG_INFINITY } else { f64::INFINITY }, p + n));
    }
    if lower.starts_with(b"nan") {
        return Some((f64::NAN, p + 3));
    }
    let mut digits = 0;
    while p < s.len() && s[p].is_ascii_digit() {
        p += 1;
        digits += 1;
    }
    if p < s.len() && s[p] == b'.' {
        p += 1;
        while p < s.len() && s[p].is_ascii_digit() {
            p += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if p < s.len() && (s[p] == b'e' || s[p] == b'E') {
        let mut q = p + 1;
        if q < s.len() && (s[q] == b'+' || s[q] == b'-') {
            q += 1;
        }
        let ds = q;
        while q < s.len() && s[q].is_ascii_digit() {
            q += 1;
        }
        if q > ds {
            p = q;
        }
    }
    let txt = std::str::from_utf8(&s[start..p]).ok()?;
    Some((txt.parse::<f64>().ok()?, p))
}

/// ParseFraction: `s` starts with '.'.
fn parse_fraction(s: &[u8]) -> DtResult<f64> {
    if s.len() == 1 {
        return Ok(0.0);
    }
    if !s[1..].iter().all(|b| b.is_ascii_digit()) {
        return Err(DtErr::BadFormat);
    }
    let txt = std::str::from_utf8(s).map_err(|_| DtErr::BadFormat)?;
    format!("0{txt}").parse::<f64>().map_err(|_| DtErr::BadFormat)
}

fn parse_fractional_second(s: &[u8]) -> DtResult<i32> {
    let frac = parse_fraction(s)?;
    Ok((frac * 1_000_000.0).round_ties_even() as i32)
}

// ── ParseDateTime ──

/// A parsed field: lower-cased text (bytes) and its type.
#[derive(Clone, Debug)]
pub struct Field {
    pub text: Vec<u8>,
    pub ftype: i32,
}

/// ParseDateTime: split the input into typed fields. `buflen` mirrors the C work buffer.
pub fn parse_date_time(timestr: &str, buflen: usize) -> DtResult<Vec<Field>> {
    let s = timestr.as_bytes();
    let mut fields: Vec<Field> = Vec::new();
    let mut used = 0usize;
    let mut cp = 0usize;
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    macro_rules! append {
        ($buf:expr, $c:expr) => {{
            if used + 1 >= buflen {
                return Err(DtErr::BadFormat);
            }
            used += 1;
            $buf.push($c);
        }};
    }
    while cp < s.len() {
        let c = s[cp];
        if c.is_ascii_whitespace() {
            cp += 1;
            continue;
        }
        if fields.len() >= MAXDATEFIELDS {
            return Err(DtErr::BadFormat);
        }
        let mut buf: Vec<u8> = Vec::new();
        let ftype;
        if c.is_ascii_digit() {
            append!(buf, c);
            cp += 1;
            while at(cp).is_ascii_digit() {
                append!(buf, s[cp]);
                cp += 1;
            }
            if at(cp) == b':' {
                ftype = DTK_TIME;
                append!(buf, b':');
                cp += 1;
                while at(cp).is_ascii_digit() || at(cp) == b':' || at(cp) == b'.' {
                    append!(buf, s[cp]);
                    cp += 1;
                }
            } else if matches!(at(cp), b'-' | b'/' | b'.') {
                let delim = s[cp];
                append!(buf, delim);
                cp += 1;
                if at(cp).is_ascii_digit() {
                    let mut ft = if delim == b'.' { DTK_NUMBER } else { DTK_DATE };
                    while at(cp).is_ascii_digit() {
                        append!(buf, s[cp]);
                        cp += 1;
                    }
                    if at(cp) == delim {
                        ft = DTK_DATE;
                        append!(buf, delim);
                        cp += 1;
                        while at(cp).is_ascii_digit() || at(cp) == delim {
                            append!(buf, s[cp]);
                            cp += 1;
                        }
                    }
                    ftype = ft;
                } else {
                    ftype = DTK_DATE;
                    while at(cp).is_ascii_alphanumeric() || at(cp) == delim {
                        append!(buf, s[cp].to_ascii_lowercase());
                        cp += 1;
                    }
                }
            } else {
                ftype = DTK_NUMBER;
            }
        } else if c == b'.' {
            append!(buf, c);
            cp += 1;
            while at(cp).is_ascii_digit() {
                append!(buf, s[cp]);
                cp += 1;
            }
            ftype = DTK_NUMBER;
        } else if c.is_ascii_alphabetic() {
            let mut ft = DTK_STRING;
            append!(buf, c.to_ascii_lowercase());
            cp += 1;
            while at(cp).is_ascii_alphabetic() {
                append!(buf, s[cp].to_ascii_lowercase());
                cp += 1;
            }
            let is_date = matches!(at(cp), b'-' | b'/' | b'.')
                || ((at(cp) == b'+' || at(cp).is_ascii_digit())
                    && datebsearch(&buf, DATETKTBL).is_none());
            if is_date {
                ft = DTK_DATE;
                loop {
                    append!(buf, s[cp].to_ascii_lowercase());
                    cp += 1;
                    let n = at(cp);
                    if !(matches!(n, b'+' | b'-' | b'/' | b'_' | b'.' | b':')
                        || n.is_ascii_alphanumeric())
                    {
                        break;
                    }
                }
            }
            ftype = ft;
        } else if c == b'+' || c == b'-' {
            append!(buf, c);
            cp += 1;
            while at(cp).is_ascii_whitespace() && cp < s.len() {
                cp += 1;
            }
            if at(cp).is_ascii_digit() {
                ftype = DTK_TZ;
                append!(buf, s[cp]);
                cp += 1;
                while at(cp).is_ascii_digit() || matches!(at(cp), b':' | b'.' | b'-') {
                    append!(buf, s[cp]);
                    cp += 1;
                }
            } else if at(cp).is_ascii_alphabetic() {
                ftype = DTK_SPECIAL;
                append!(buf, s[cp].to_ascii_lowercase());
                cp += 1;
                while at(cp).is_ascii_alphabetic() {
                    append!(buf, s[cp].to_ascii_lowercase());
                    cp += 1;
                }
            } else {
                return Err(DtErr::BadFormat);
            }
        } else if c.is_ascii_punctuation() {
            cp += 1;
            continue;
        } else {
            return Err(DtErr::BadFormat);
        }
        used += 1; // the field's NUL terminator
        fields.push(Field { text: buf, ftype });
    }
    Ok(fields)
}

// ── decoding context ──

/// What date/time decoding needs from the session.
#[derive(Clone, Copy)]
pub struct DtCtx<'a> {
    pub tz: &'a TimeZone,
    /// Transaction start (now()).
    pub now: i64,
    pub order: DateOrder,
    pub interval_style: IntervalStyle,
    /// The `timezone_abbreviations` set.
    pub abbrevs: &'a str,
}

/// The session's fields at now() (GetCurrentTimeUsec): (tm, fsec, tz west).
fn current_time_usec(cx: &DtCtx) -> (Tm, i32, i32) {
    match timestamp2tm(cx.now, Some(cx.tz)) {
        Some((tm, fsec)) => {
            let tz = -tm.gmtoff as i32;
            (tm, fsec, tz)
        }
        None => (Tm::default(), 0, 0),
    }
}

/// DecodeTimezoneAbbrev: (ftype, offset east, dynamic zone).
fn decode_timezone_abbrev(
    lowtoken: &[u8],
    cx: &DtCtx,
) -> DtResult<(i32, i32, Option<TimeZone>)> {
    let up = String::from_utf8_lossy(lowtoken).to_ascii_uppercase();
    if let Some((isfixed, gmtoff, isdst)) = tz::pg_timezone_abbrev_is_known(&up, cx.tz.pg()) {
        let ft = if isfixed {
            if isdst { DTZ } else { TZ }
        } else {
            DYNTZ
        };
        return Ok((
            ft,
            gmtoff as i32,
            if isfixed { None } else { Some(cx.tz.clone()) },
        ));
    }
    let Some(tbl) = tz::abbrev_table(cx.abbrevs).or_else(|| tz::abbrev_table("Default")) else {
        return Ok((UNKNOWN_FIELD, 0, None));
    };
    let Ok(i) = tbl.binary_search_by(|e| tok_cmp(e.token.as_bytes(), lowtoken)) else {
        return Ok((UNKNOWN_FIELD, 0, None));
    };
    match &tbl[i].kind {
        tz::AbbrevKind::Fixed(o, dst) => Ok((if *dst { DTZ } else { TZ }, *o, None)),
        tz::AbbrevKind::Dynamic(zone) => match tz::pg_tzset(zone) {
            Some(z) => Ok((DYNTZ, 0, Some(TimeZone(z)))),
            None => Err(DtErr::BadZoneAbbrev(zone.clone(), tbl[i].token.clone())),
        },
    }
}

/// DecodeTimezoneAbbrevPrefix (for to_timestamp's TZ): (length, offset east, dynamic zone).
pub fn decode_timezone_abbrev_prefix(
    s: &str,
    cx: &DtCtx,
) -> Option<(usize, i32, Option<TimeZone>)> {
    let mut low: Vec<u8> = s
        .bytes()
        .take_while(|b| b.is_ascii_alphabetic())
        .take(TOKMAXLEN)
        .map(|b| b.to_ascii_lowercase())
        .collect();
    while !low.is_empty() {
        let up = String::from_utf8_lossy(&low).to_ascii_uppercase();
        if let Some((isfixed, gmtoff, _)) = tz::pg_timezone_abbrev_is_known(&up, cx.tz.pg()) {
            return Some(if isfixed {
                (low.len(), gmtoff as i32, None)
            } else {
                (low.len(), 0, Some(cx.tz.clone()))
            });
        }
        if let Some(tbl) = tz::abbrev_table(cx.abbrevs).or_else(|| tz::abbrev_table("Default"))
            && let Ok(i) = tbl.binary_search_by(|e| tok_cmp(e.token.as_bytes(), &low))
        {
            match &tbl[i].kind {
                tz::AbbrevKind::Dynamic(zone) => {
                    if let Some(z) = tz::pg_tzset(zone) {
                        return Some((low.len(), 0, Some(TimeZone(z))));
                    }
                }
                tz::AbbrevKind::Fixed(o, _) => return Some((low.len(), *o, None)),
            }
        }
        low.pop();
    }
    None
}

/// DecodeTimezone: numeric zone ("+05", "-08:00") → seconds west.
pub fn decode_timezone(s: &[u8]) -> DtResult<i32> {
    if s.first() != Some(&b'+') && s.first() != Some(&b'-') {
        return Err(DtErr::BadFormat);
    }
    let (hr, mut cp, over) = strtoint(s, 1);
    if over {
        return Err(DtErr::TzdispOverflow);
    }
    let (mut hr, mut min, mut sec) = (hr, 0, 0);
    if s.get(cp) == Some(&b':') {
        let (m, c, over) = strtoint(s, cp + 1);
        if over {
            return Err(DtErr::TzdispOverflow);
        }
        min = m;
        cp = c;
        if s.get(cp) == Some(&b':') {
            let (sv, c, over) = strtoint(s, cp + 1);
            if over {
                return Err(DtErr::TzdispOverflow);
            }
            sec = sv;
            cp = c;
        }
    } else if cp >= s.len() && s.len() > 3 {
        min = hr % 100;
        hr /= 100;
    }
    if !(0..=MAX_TZDISP_HOUR).contains(&hr) || !(0..60).contains(&min) || !(0..60).contains(&sec)
    {
        return Err(DtErr::TzdispOverflow);
    }
    let mut tz = (hr * 60 + min) * 60 + sec;
    if s[0] == b'-' {
        tz = -tz;
    }
    if cp < s.len() {
        return Err(DtErr::BadFormat);
    }
    Ok(-tz)
}

/// DecodeNumberField: run-together date or time digits. Returns DTK_DATE or DTK_TIME.
fn decode_number_field(
    s: &[u8],
    fmask: i32,
    tmask: &mut i32,
    tm: &mut Tm,
    fsec: &mut i32,
    is2digits: &mut bool,
) -> DtResult<i32> {
    if !s.iter().all(|b| b.is_ascii_digit() || *b == b'.') {
        return Err(DtErr::BadFormat);
    }
    let mut s = s;
    if let Some(dot) = s.iter().position(|&b| b == b'.') {
        *fsec = parse_fractional_second(&s[dot..])?;
        s = &s[..dot];
    } else if (fmask & DTK_DATE_M) != DTK_DATE_M && s.len() >= 6 {
        let len = s.len();
        *tmask = DTK_DATE_M;
        tm.mday = atoi(&s[len - 2..]);
        tm.mon = atoi(&s[len - 4..len - 2]);
        tm.year = atoi(&s[..len - 4]);
        if len - 4 == 2 {
            *is2digits = true;
        }
        return Ok(DTK_DATE);
    }
    if (fmask & DTK_TIME_M) != DTK_TIME_M {
        if s.len() == 6 {
            *tmask = DTK_TIME_M;
            tm.sec = atoi(&s[4..6]);
            tm.min = atoi(&s[2..4]);
            tm.hour = atoi(&s[..2]);
            return Ok(DTK_TIME);
        } else if s.len() == 4 {
            *tmask = DTK_TIME_M;
            tm.sec = 0;
            tm.min = atoi(&s[2..4]);
            tm.hour = atoi(&s[..2]);
            return Ok(DTK_TIME);
        }
    }
    Err(DtErr::BadFormat)
}

/// DecodeNumber: a plain number in date context.
#[allow(clippy::too_many_arguments)]
fn decode_number(
    s: &[u8],
    have_text_month: bool,
    fmask: i32,
    tmask: &mut i32,
    tm: &mut Tm,
    fsec: &mut i32,
    is2digits: &mut bool,
    order: DateOrder,
) -> DtResult<()> {
    *tmask = 0;
    let flen = s.len();
    let (val, cp, over) = strtoint(s, 0);
    if over {
        return Err(DtErr::FieldOverflow);
    }
    if cp == 0 {
        return Err(DtErr::BadFormat);
    }
    if s.get(cp) == Some(&b'.') {
        if cp > 2 {
            decode_number_field(s, fmask | DTK_DATE_M, tmask, tm, fsec, is2digits)?;
            return Ok(());
        }
        *fsec = parse_fractional_second(&s[cp..])?;
    } else if cp < s.len() {
        return Err(DtErr::BadFormat);
    }
    if flen == 3 && (fmask & DTK_DATE_M) == dtk_m(YEAR) && (1..=366).contains(&val) {
        *tmask = dtk_m(DOY) | dtk_m(MONTH) | dtk_m(DAY);
        tm.yday = val;
        return Ok(());
    }
    match fmask & DTK_DATE_M {
        0 => {
            if flen >= 3 || order == DateOrder::Ymd {
                *tmask = dtk_m(YEAR);
                tm.year = val;
            } else if order == DateOrder::Dmy {
                *tmask = dtk_m(DAY);
                tm.mday = val;
            } else {
                *tmask = dtk_m(MONTH);
                tm.mon = val;
            }
        }
        m if m == dtk_m(YEAR) => {
            *tmask = dtk_m(MONTH);
            tm.mon = val;
        }
        m if m == dtk_m(MONTH) => {
            if have_text_month {
                if flen >= 3 || order == DateOrder::Ymd {
                    *tmask = dtk_m(YEAR);
                    tm.year = val;
                } else {
                    *tmask = dtk_m(DAY);
                    tm.mday = val;
                }
            } else {
                *tmask = dtk_m(DAY);
                tm.mday = val;
            }
        }
        m if m == dtk_m(YEAR) | dtk_m(MONTH) => {
            if have_text_month {
                if flen >= 3 && *is2digits {
                    *tmask = dtk_m(DAY);
                    tm.mday = tm.year;
                    tm.year = val;
                    *is2digits = false;
                } else {
                    *tmask = dtk_m(DAY);
                    tm.mday = val;
                }
            } else {
                *tmask = dtk_m(DAY);
                tm.mday = val;
            }
        }
        m if m == dtk_m(DAY) => {
            *tmask = dtk_m(MONTH);
            tm.mon = val;
        }
        m if m == dtk_m(MONTH) | dtk_m(DAY) => {
            *tmask = dtk_m(YEAR);
            tm.year = val;
        }
        m if m == DTK_DATE_M => {
            decode_number_field(s, fmask, tmask, tm, fsec, is2digits)?;
            return Ok(());
        }
        _ => return Err(DtErr::BadFormat),
    }
    if *tmask == dtk_m(YEAR) {
        *is2digits = flen <= 2;
    }
    Ok(())
}

/// DecodeDate: a date with delimiters ("1999-01-08", "jan-08-1999").
fn decode_date(
    s: &[u8],
    fmask: i32,
    tmask: &mut i32,
    is2digits: &mut bool,
    tm: &mut Tm,
    order: DateOrder,
) -> DtResult<()> {
    let mut fmask = fmask;
    *tmask = 0;
    let mut fields: Vec<Option<&[u8]>> = Vec::new();
    let mut p = 0;
    while p < s.len() && fields.len() < MAXDATEFIELDS {
        while p < s.len() && !s[p].is_ascii_alphanumeric() {
            p += 1;
        }
        if p >= s.len() {
            return Err(DtErr::BadFormat);
        }
        let start = p;
        if s[p].is_ascii_digit() {
            while p < s.len() && s[p].is_ascii_digit() {
                p += 1;
            }
        } else if s[p].is_ascii_alphabetic() {
            while p < s.len() && s[p].is_ascii_alphabetic() {
                p += 1;
            }
        }
        let end = p;
        if p < s.len() {
            p += 1;
        }
        fields.push(Some(&s[start..end]));
    }
    let mut have_text_month = false;
    for f in fields.iter_mut() {
        let Some(t) = *f else { continue };
        if t.first().is_some_and(|b| b.is_ascii_alphabetic()) {
            let (ty, val) = decode_special(t);
            if ty == IGNORE_DTF {
                continue;
            }
            let dmask = dtk_m(ty);
            match ty {
                MONTH => {
                    tm.mon = val;
                    have_text_month = true;
                }
                _ => return Err(DtErr::BadFormat),
            }
            if fmask & dmask != 0 {
                return Err(DtErr::BadFormat);
            }
            fmask |= dmask;
            *tmask |= dmask;
            *f = None;
        }
    }
    for t in fields.iter().flatten() {
        if t.is_empty() {
            return Err(DtErr::BadFormat);
        }
        let mut dmask = 0;
        let mut fsec = 0;
        decode_number(
            t,
            have_text_month,
            fmask,
            &mut dmask,
            tm,
            &mut fsec,
            is2digits,
            order,
        )?;
        if fmask & dmask != 0 {
            return Err(DtErr::BadFormat);
        }
        fmask |= dmask;
        *tmask |= dmask;
    }
    if (fmask & !(dtk_m(DOY) | dtk_m(TZ))) != DTK_DATE_M {
        return Err(DtErr::BadFormat);
    }
    Ok(())
}

/// ValidateDate.
pub fn validate_date(fmask: i32, isjulian: bool, is2digits: bool, bc: bool, tm: &mut Tm) -> DtResult<()> {
    if fmask & dtk_m(YEAR) != 0 {
        if isjulian {
        } else if bc {
            if tm.year <= 0 {
                return Err(DtErr::FieldOverflow);
            }
            tm.year = -(tm.year - 1);
        } else if is2digits {
            if tm.year < 0 {
                return Err(DtErr::FieldOverflow);
            }
            if tm.year < 70 {
                tm.year += 2000;
            } else if tm.year < 100 {
                tm.year += 1900;
            }
        } else if tm.year <= 0 {
            return Err(DtErr::FieldOverflow);
        }
    }
    if fmask & dtk_m(DOY) != 0 {
        let (y, m, d) = j2date(date2j(tm.year, 1, 1).wrapping_add(tm.yday).wrapping_sub(1));
        tm.year = y;
        tm.mon = m;
        tm.mday = d;
    }
    if fmask & dtk_m(MONTH) != 0 && (tm.mon < 1 || tm.mon > 12) {
        return Err(DtErr::MdFieldOverflow);
    }
    if fmask & dtk_m(DAY) != 0 && (tm.mday < 1 || tm.mday > 31) {
        return Err(DtErr::MdFieldOverflow);
    }
    if (fmask & DTK_DATE_M) == DTK_DATE_M
        && tm.mday > DAY_TAB[isleap(tm.year) as usize][(tm.mon - 1) as usize]
    {
        return Err(DtErr::FieldOverflow);
    }
    Ok(())
}

/// DecodeTimeCommon: "h:m[:s[.f]]" → (hour, min, sec, usec) as a pg_itm.
fn decode_time_common(s: &[u8], range: i32) -> DtResult<Itm> {
    let mut itm = Itm::default();
    let (h, mut cp, over) = strtol(s, 0);
    if over {
        return Err(DtErr::FieldOverflow);
    }
    itm.hour = h;
    if s.get(cp) != Some(&b':') {
        return Err(DtErr::BadFormat);
    }
    let (m, c, over) = strtoint(s, cp + 1);
    if over {
        return Err(DtErr::FieldOverflow);
    }
    itm.min = m;
    cp = c;
    let mut fsec = 0;
    match s.get(cp) {
        None => {
            itm.sec = 0;
            if range == (interval_mask(MINUTE) | interval_mask(SECOND)) {
                if itm.hour > i32::MAX as i64 || itm.hour < i32::MIN as i64 {
                    return Err(DtErr::FieldOverflow);
                }
                itm.sec = itm.min;
                itm.min = itm.hour as i32;
                itm.hour = 0;
            }
        }
        Some(b'.') => {
            fsec = parse_fractional_second(&s[cp..])?;
            if itm.hour > i32::MAX as i64 || itm.hour < i32::MIN as i64 {
                return Err(DtErr::FieldOverflow);
            }
            itm.sec = itm.min;
            itm.min = itm.hour as i32;
            itm.hour = 0;
        }
        Some(b':') => {
            let (sv, c, over) = strtoint(s, cp + 1);
            if over {
                return Err(DtErr::FieldOverflow);
            }
            itm.sec = sv;
            cp = c;
            match s.get(cp) {
                Some(b'.') => fsec = parse_fractional_second(&s[cp..])?,
                None => {}
                Some(_) => return Err(DtErr::BadFormat),
            }
        }
        Some(_) => return Err(DtErr::BadFormat),
    }
    if itm.hour < 0
        || !(0..=59).contains(&itm.min)
        || !(0..=60).contains(&itm.sec)
        || !(0..=1_000_000).contains(&fsec)
    {
        return Err(DtErr::FieldOverflow);
    }
    itm.usec = fsec;
    Ok(itm)
}

/// DecodeTime (timestamps): sets tm hour/min/sec and fsec.
fn decode_time(s: &[u8], range: i32, tmask: &mut i32, tm: &mut Tm, fsec: &mut i32) -> DtResult<()> {
    *tmask = DTK_TIME_M;
    let itm = decode_time_common(s, range)?;
    if itm.hour > i32::MAX as i64 {
        return Err(DtErr::FieldOverflow);
    }
    tm.hour = itm.hour as i32;
    tm.min = itm.min;
    tm.sec = itm.sec;
    *fsec = itm.usec;
    Ok(())
}

fn int64_multiply_add(val: i64, multiplier: i64, sum: &mut i64) -> bool {
    match val.checked_mul(multiplier).and_then(|p| sum.checked_add(p)) {
        Some(v) => {
            *sum = v;
            true
        }
        None => false,
    }
}

/// DecodeTimeForInterval: adds the time to itm_in.usec.
fn decode_time_for_interval(s: &[u8], range: i32, itm_in: &mut ItmIn) -> DtResult<()> {
    let itm = decode_time_common(s, range)?;
    itm_in.usec = itm.usec as i64;
    if !int64_multiply_add(itm.hour, USECS_PER_HOUR, &mut itm_in.usec)
        || !int64_multiply_add(itm.min as i64, USECS_PER_MINUTE, &mut itm_in.usec)
        || !int64_multiply_add(itm.sec as i64, USECS_PER_SEC, &mut itm_in.usec)
    {
        return Err(DtErr::FieldOverflow);
    }
    Ok(())
}

/// time_overflows.
pub fn time_overflows(hour: i32, min: i32, sec: i32, fsec: i32) -> bool {
    if !(0..=24).contains(&hour)
        || !(0..60).contains(&min)
        || !(0..=60).contains(&sec)
        || !(0..=1_000_000).contains(&fsec)
    {
        return true;
    }
    time2t(hour, min, sec, fsec) > USECS_PER_DAY
}

/// float_time_overflows.
pub fn float_time_overflows(hour: i32, min: i32, sec: f64) -> bool {
    if !(0..=24).contains(&hour) || !(0..60).contains(&min) {
        return true;
    }
    if sec.is_nan() {
        return true;
    }
    let sec = (sec * USECS_PER_SEC as f64).round_ties_even();
    if sec < 0.0 || sec > 60.0 * USECS_PER_SEC as f64 {
        return true;
    }
    ((hour as i64 * 60 + min as i64) * 60) * USECS_PER_SEC + sec as i64 > USECS_PER_DAY
}

/// The result of DecodeDateTime / DecodeTimeOnly.
#[derive(Clone, Debug)]
pub struct Decoded {
    pub dtype: i32,
    pub tm: Tm,
    pub fsec: i32,
    /// Seconds west of UTC.
    pub tz: i32,
}

/// DecodeDateTime.
pub fn decode_date_time(fields: &mut [Field], cx: &DtCtx) -> DtResult<Decoded> {
    let nf = fields.len();
    let mut fmask = 0i32;
    let mut tmask = 0i32;
    let mut ptype = 0i32;
    let mut mer = HR24;
    let mut have_text_month = false;
    let mut isjulian = false;
    let mut is2digits = false;
    let mut bc = false;
    let mut named_tz: Option<TimeZone> = None;
    let mut abbrev_tz: Option<TimeZone> = None;
    let mut abbrev: Vec<u8> = Vec::new();
    let mut dtype = DTK_DATE;
    let mut tm = Tm {
        isdst: -1,
        ..Default::default()
    };
    let mut fsec = 0i32;
    let mut tzp = 0i32;
    for fld in fields.iter().take(nf) {
        let field = fld.text.clone();
        match fld.ftype {
            DTK_DATE => {
                if ptype == DTK_JULIAN {
                    let (jday, cp, over) = strtoint(&field, 0);
                    if over || jday < 0 {
                        return Err(DtErr::FieldOverflow);
                    }
                    let (y, m, d) = j2date(jday);
                    tm.year = y;
                    tm.mon = m;
                    tm.mday = d;
                    isjulian = true;
                    tzp = decode_timezone(&field[cp..])?;
                    tmask = DTK_DATE_M | DTK_TIME_M | dtk_m(TZ);
                    ptype = 0;
                } else if ptype != 0
                    || (fmask & (dtk_m(MONTH) | dtk_m(DAY))) == (dtk_m(MONTH) | dtk_m(DAY))
                {
                    if field.first().is_some_and(|b| b.is_ascii_digit()) || ptype != 0 {
                        if ptype != 0 {
                            if ptype != DTK_TIME {
                                return Err(DtErr::BadFormat);
                            }
                            ptype = 0;
                        }
                        if (fmask & DTK_TIME_M) == DTK_TIME_M {
                            return Err(DtErr::BadFormat);
                        }
                        let Some(dash) = field.iter().position(|&b| b == b'-') else {
                            return Err(DtErr::BadFormat);
                        };
                        tzp = decode_timezone(&field[dash..])?;
                        decode_number_field(
                            &field[..dash],
                            fmask,
                            &mut tmask,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                        tmask |= dtk_m(TZ);
                    } else {
                        let name = String::from_utf8_lossy(&field).into_owned();
                        match tz::pg_tzset(&name) {
                            Some(z) => named_tz = Some(TimeZone(z)),
                            None => return Err(DtErr::BadTimezone(name)),
                        }
                        tmask = dtk_m(TZ);
                    }
                } else {
                    decode_date(&field, fmask, &mut tmask, &mut is2digits, &mut tm, cx.order)?;
                }
            }
            DTK_TIME => {
                if ptype != 0 {
                    if ptype != DTK_TIME {
                        return Err(DtErr::BadFormat);
                    }
                    ptype = 0;
                }
                decode_time(&field, INTERVAL_FULL_RANGE, &mut tmask, &mut tm, &mut fsec)?;
                if time_overflows(tm.hour, tm.min, tm.sec, fsec) {
                    return Err(DtErr::FieldOverflow);
                }
            }
            DTK_TZ => {
                tzp = decode_timezone(&field)?;
                tmask = dtk_m(TZ);
            }
            DTK_NUMBER => {
                if ptype != 0 {
                    let (value, cp, over) = strtoint(&field, 0);
                    if over {
                        return Err(DtErr::FieldOverflow);
                    }
                    if cp < field.len() && field[cp] != b'.' {
                        return Err(DtErr::BadFormat);
                    }
                    match ptype {
                        DTK_JULIAN => {
                            if value < 0 {
                                return Err(DtErr::FieldOverflow);
                            }
                            tmask = DTK_DATE_M;
                            let (y, m, d) = j2date(value);
                            tm.year = y;
                            tm.mon = m;
                            tm.mday = d;
                            isjulian = true;
                            if cp < field.len() {
                                let time = parse_fraction(&field[cp..])? * USECS_PER_DAY as f64;
                                let (h, mi, s, f) = dt2time(time as i64);
                                tm.hour = h;
                                tm.min = mi;
                                tm.sec = s;
                                fsec = f;
                                tmask |= DTK_TIME_M;
                            }
                        }
                        DTK_TIME => {
                            decode_number_field(
                                &field,
                                fmask | DTK_DATE_M,
                                &mut tmask,
                                &mut tm,
                                &mut fsec,
                                &mut is2digits,
                            )?;
                            if tmask != DTK_TIME_M {
                                return Err(DtErr::BadFormat);
                            }
                        }
                        _ => return Err(DtErr::BadFormat),
                    }
                    ptype = 0;
                    dtype = DTK_DATE;
                } else {
                    let flen = field.len();
                    let dot = field.iter().position(|&b| b == b'.');
                    if dot.is_some() && fmask & DTK_DATE_M == 0 {
                        decode_date(&field, fmask, &mut tmask, &mut is2digits, &mut tm, cx.order)?;
                    } else if let Some(d) = dot
                        && d > 2
                    {
                        decode_number_field(
                            &field,
                            fmask,
                            &mut tmask,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                    } else if flen >= 6
                        && (fmask & DTK_DATE_M == 0 || fmask & DTK_TIME_M == 0)
                    {
                        decode_number_field(
                            &field,
                            fmask,
                            &mut tmask,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                    } else {
                        decode_number(
                            &field,
                            have_text_month,
                            fmask,
                            &mut tmask,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                            cx.order,
                        )?;
                    }
                }
            }
            DTK_STRING | DTK_SPECIAL => {
                let (mut ty, mut val, valtz) = decode_timezone_abbrev(&field, cx)?;
                if ty == UNKNOWN_FIELD {
                    (ty, val) = decode_special(&field);
                }
                if ty == IGNORE_DTF {
                    continue;
                }
                tmask = dtk_m(ty);
                match ty {
                    RESERV => match val {
                        DTK_NOW => {
                            tmask = DTK_DATE_M | DTK_TIME_M | dtk_m(TZ);
                            dtype = DTK_DATE;
                            let (t, f, z) = current_time_usec(cx);
                            tm = t;
                            fsec = f;
                            tzp = z;
                        }
                        DTK_YESTERDAY | DTK_TODAY | DTK_TOMORROW => {
                            tmask = DTK_DATE_M;
                            dtype = DTK_DATE;
                            let (cur, _, _) = current_time_usec(cx);
                            let shift = match val {
                                DTK_YESTERDAY => -1,
                                DTK_TOMORROW => 1,
                                _ => 0,
                            };
                            let (y, m, d) = j2date(date2j(cur.year, cur.mon, cur.mday) + shift);
                            tm.year = y;
                            tm.mon = m;
                            tm.mday = d;
                        }
                        DTK_ZULU => {
                            tmask = DTK_TIME_M | dtk_m(TZ);
                            dtype = DTK_DATE;
                            tm.hour = 0;
                            tm.min = 0;
                            tm.sec = 0;
                            tzp = 0;
                        }
                        DTK_EPOCH | DTK_LATE | DTK_EARLY => {
                            tmask = DTK_DATE_M | DTK_TIME_M | dtk_m(TZ);
                            dtype = val;
                        }
                        _ => return Err(DtErr::BadFormat),
                    },
                    MONTH => {
                        if fmask & dtk_m(MONTH) != 0
                            && !have_text_month
                            && fmask & dtk_m(DAY) == 0
                            && (1..=31).contains(&tm.mon)
                        {
                            tm.mday = tm.mon;
                            tmask = dtk_m(DAY);
                        }
                        have_text_month = true;
                        tm.mon = val;
                    }
                    DTZMOD => {
                        tmask |= dtk_m(DTZ);
                        tm.isdst = 1;
                        tzp -= val;
                    }
                    DTZ => {
                        tmask |= dtk_m(TZ);
                        tm.isdst = 1;
                        tzp = -val;
                    }
                    TZ => {
                        tm.isdst = 0;
                        tzp = -val;
                    }
                    DYNTZ => {
                        tmask |= dtk_m(TZ);
                        abbrev_tz = valtz;
                        abbrev = field.clone();
                    }
                    AMPM => mer = val,
                    ADBC => bc = val == BC,
                    DOW => tm.wday = val,
                    UNITS => {
                        tmask = 0;
                        if ptype != 0 {
                            return Err(DtErr::BadFormat);
                        }
                        ptype = val;
                    }
                    ISOTIME => {
                        tmask = 0;
                        if (fmask & DTK_DATE_M) != DTK_DATE_M {
                            return Err(DtErr::BadFormat);
                        }
                        if ptype != 0 {
                            return Err(DtErr::BadFormat);
                        }
                        ptype = val;
                    }
                    UNKNOWN_FIELD => {
                        let name = String::from_utf8_lossy(&field).into_owned();
                        match tz::pg_tzset(&name) {
                            Some(z) => named_tz = Some(TimeZone(z)),
                            None => return Err(DtErr::BadFormat),
                        }
                        tmask = dtk_m(TZ);
                    }
                    _ => return Err(DtErr::BadFormat),
                }
            }
            _ => return Err(DtErr::BadFormat),
        }
        if tmask & fmask != 0 {
            return Err(DtErr::BadFormat);
        }
        fmask |= tmask;
    }
    if ptype != 0 {
        return Err(DtErr::BadFormat);
    }
    if dtype == DTK_DATE {
        validate_date(fmask, isjulian, is2digits, bc, &mut tm)?;
        if mer != HR24 && tm.hour > 12 {
            return Err(DtErr::FieldOverflow);
        }
        if mer == AM && tm.hour == 12 {
            tm.hour = 0;
        } else if mer == PM && tm.hour != 12 {
            tm.hour += 12;
        }
        // (Postgres returns 1, "only a time", when the time fields are complete; every caller
        // treats that as a bad format too.)
        if (fmask & DTK_DATE_M) != DTK_DATE_M {
            return Err(DtErr::BadFormat);
        }
        if let Some(z) = &named_tz {
            if fmask & dtk_m(DTZMOD) != 0 {
                return Err(DtErr::BadFormat);
            }
            let (off, isdst) = determine_tz_offset(&tm, z);
            tzp = off;
            tm.isdst = isdst;
        }
        if let Some(z) = &abbrev_tz {
            if fmask & dtk_m(DTZMOD) != 0 {
                return Err(DtErr::BadFormat);
            }
            let (off, isdst) = determine_tz_abbrev_offset(&tm, &abbrev, z);
            tzp = off;
            tm.isdst = isdst;
        }
        if fmask & dtk_m(TZ) == 0 {
            if fmask & dtk_m(DTZMOD) != 0 {
                return Err(DtErr::BadFormat);
            }
            let (off, isdst) = determine_tz_offset(&tm, cx.tz);
            tzp = off;
            tm.isdst = isdst;
        }
    }
    Ok(Decoded {
        dtype,
        tm,
        fsec,
        tz: tzp,
    })
}

/// DetermineTimeZoneOffsetInternal: (offset west, utc time_t, isdst).
fn determine_tz_offset_internal(tm: &Tm, tz: &TimeZone) -> (i32, i64, i32) {
    if !is_valid_julian(tm.year, tm.mon, tm.mday) {
        return (0, 0, 0);
    }
    let date = (date2j(tm.year, tm.mon, tm.mday) - UNIX_EPOCH_JDATE) as i64;
    let day = date * SECS_PER_DAY;
    let sec = tm.sec as i64 + (tm.min as i64 + tm.hour as i64 * 60) * 60;
    let mytime = day + sec;
    let prevtime = mytime - SECS_PER_DAY;
    let Ok(((before_gmtoff, before_isdst), after)) = tz::pg_next_dst_boundary(prevtime, tz.pg())
    else {
        return (0, 0, 0);
    };
    let Some((boundary, (after_gmtoff, after_isdst))) = after else {
        return (-(before_gmtoff as i32), mytime - before_gmtoff, before_isdst as i32);
    };
    let beforetime = mytime - before_gmtoff;
    let aftertime = mytime - after_gmtoff;
    if beforetime < boundary && aftertime < boundary {
        return (-(before_gmtoff as i32), beforetime, before_isdst as i32);
    }
    if beforetime > boundary && aftertime >= boundary {
        return (-(after_gmtoff as i32), aftertime, after_isdst as i32);
    }
    if beforetime > aftertime {
        return (-(before_gmtoff as i32), beforetime, before_isdst as i32);
    }
    (-(after_gmtoff as i32), aftertime, after_isdst as i32)
}

/// DetermineTimeZoneOffset: (offset seconds west, isdst) of local `tm` in `tz`.
pub fn determine_tz_offset(tm: &Tm, tz: &TimeZone) -> (i32, i32) {
    let (off, _, isdst) = determine_tz_offset_internal(tm, tz);
    (off, isdst)
}

/// DetermineTimeZoneAbbrevOffset: (offset west, isdst).
pub fn determine_tz_abbrev_offset(tm: &Tm, abbr: &[u8], tz: &TimeZone) -> (i32, i32) {
    let (zone_offset, t, isdst) = determine_tz_offset_internal(tm, tz);
    let up = String::from_utf8_lossy(abbr).to_ascii_uppercase();
    match tz::pg_interpret_timezone_abbrev(&up, t, tz.pg()) {
        Some((gmtoff, d)) => (-(gmtoff as i32), d as i32),
        None => (zone_offset, isdst),
    }
}

/// DetermineTimeZoneAbbrevOffsetTS: (offset west, isdst) at a UTC timestamp.
pub fn determine_tz_abbrev_offset_ts(ts: i64, abbr: &str, tz: &TimeZone) -> Result<(i32, i32)> {
    let t = ts / USECS_PER_SEC + UNIX_EPOCH_OFFSET_SECS;
    if let Some((gmtoff, d)) = tz::pg_interpret_timezone_abbrev(&abbr.to_ascii_uppercase(), t, tz.pg())
    {
        return Ok((-(gmtoff as i32), d as i32));
    }
    let Some((tm, _)) = timestamp2tm(ts, Some(tz)) else {
        bail!(DATETIME_FIELD_OVERFLOW, "timestamp out of range");
    };
    Ok(determine_tz_offset(&tm, tz))
}

/// DecodeTimeOnly.
pub fn decode_time_only(fields: &mut [Field], cx: &DtCtx) -> DtResult<Decoded> {
    let nf = fields.len();
    let mut fmask = 0i32;
    let mut tmask = 0i32;
    let mut ptype = 0i32;
    let mut isjulian = false;
    let mut is2digits = false;
    let mut bc = false;
    let mut mer = HR24;
    let mut named_tz: Option<TimeZone> = None;
    let mut abbrev_tz: Option<TimeZone> = None;
    let mut abbrev: Vec<u8> = Vec::new();
    let mut dtype = DTK_TIME;
    let mut tm = Tm {
        isdst: -1,
        ..Default::default()
    };
    let mut fsec = 0i32;
    let mut tzp = 0i32;
    for i in 0..nf {
        let field = fields[i].text.clone();
        match fields[i].ftype {
            DTK_DATE => {
                if i == 0 && nf >= 2 && (fields[nf - 1].ftype == DTK_DATE || fields[1].ftype == DTK_TIME)
                {
                    decode_date(&field, fmask, &mut tmask, &mut is2digits, &mut tm, cx.order)?;
                } else if field.first().is_some_and(|b| b.is_ascii_digit()) {
                    if (fmask & DTK_TIME_M) == DTK_TIME_M {
                        return Err(DtErr::BadFormat);
                    }
                    let Some(dash) = field.iter().position(|&b| b == b'-') else {
                        return Err(DtErr::BadFormat);
                    };
                    tzp = decode_timezone(&field[dash..])?;
                    let r = decode_number_field(
                        &field[..dash],
                        fmask | DTK_DATE_M,
                        &mut tmask,
                        &mut tm,
                        &mut fsec,
                        &mut is2digits,
                    )?;
                    fields[i].ftype = r;
                    tmask |= dtk_m(TZ);
                } else {
                    let name = String::from_utf8_lossy(&field).into_owned();
                    match tz::pg_tzset(&name) {
                        Some(z) => named_tz = Some(TimeZone(z)),
                        None => return Err(DtErr::BadTimezone(name)),
                    }
                    fields[i].ftype = DTK_TZ;
                    tmask = dtk_m(TZ);
                }
            }
            DTK_TIME => {
                if ptype != 0 {
                    if ptype != DTK_TIME {
                        return Err(DtErr::BadFormat);
                    }
                    ptype = 0;
                }
                decode_time(&field, INTERVAL_FULL_RANGE, &mut tmask, &mut tm, &mut fsec)?;
            }
            DTK_TZ => {
                tzp = decode_timezone(&field)?;
                tmask = dtk_m(TZ);
            }
            DTK_NUMBER => {
                if ptype != 0 {
                    let (value, cp, over) = strtoint(&field, 0);
                    if over {
                        return Err(DtErr::FieldOverflow);
                    }
                    if cp < field.len() && field[cp] != b'.' {
                        return Err(DtErr::BadFormat);
                    }
                    match ptype {
                        DTK_JULIAN => {
                            if value < 0 {
                                return Err(DtErr::FieldOverflow);
                            }
                            tmask = DTK_DATE_M;
                            let (y, m, d) = j2date(value);
                            tm.year = y;
                            tm.mon = m;
                            tm.mday = d;
                            isjulian = true;
                            if cp < field.len() {
                                let time = parse_fraction(&field[cp..])? * USECS_PER_DAY as f64;
                                let (h, mi, s, f) = dt2time(time as i64);
                                tm.hour = h;
                                tm.min = mi;
                                tm.sec = s;
                                fsec = f;
                                tmask |= DTK_TIME_M;
                            }
                        }
                        DTK_TIME => {
                            let r = decode_number_field(
                                &field,
                                fmask | DTK_DATE_M,
                                &mut tmask,
                                &mut tm,
                                &mut fsec,
                                &mut is2digits,
                            )?;
                            fields[i].ftype = r;
                            if tmask != DTK_TIME_M {
                                return Err(DtErr::BadFormat);
                            }
                        }
                        _ => return Err(DtErr::BadFormat),
                    }
                    ptype = 0;
                    dtype = DTK_DATE;
                } else {
                    let flen = field.len();
                    if let Some(dot) = field.iter().position(|&b| b == b'.') {
                        if i == 0 && nf >= 2 && fields[nf - 1].ftype == DTK_DATE {
                            decode_date(&field, fmask, &mut tmask, &mut is2digits, &mut tm, cx.order)?;
                        } else if dot > 2 {
                            let r = decode_number_field(
                                &field,
                                fmask | DTK_DATE_M,
                                &mut tmask,
                                &mut tm,
                                &mut fsec,
                                &mut is2digits,
                            )?;
                            fields[i].ftype = r;
                        } else {
                            return Err(DtErr::BadFormat);
                        }
                    } else if flen > 4 {
                        let r = decode_number_field(
                            &field,
                            fmask | DTK_DATE_M,
                            &mut tmask,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                        fields[i].ftype = r;
                    } else {
                        decode_number(
                            &field,
                            false,
                            fmask | DTK_DATE_M,
                            &mut tmask,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                            cx.order,
                        )?;
                    }
                }
            }
            DTK_STRING | DTK_SPECIAL => {
                let (mut ty, mut val, valtz) = decode_timezone_abbrev(&field, cx)?;
                if ty == UNKNOWN_FIELD {
                    (ty, val) = decode_special(&field);
                }
                if ty == IGNORE_DTF {
                    continue;
                }
                tmask = dtk_m(ty);
                match ty {
                    RESERV => match val {
                        DTK_NOW => {
                            tmask = DTK_TIME_M;
                            dtype = DTK_TIME;
                            let (t, f, _) = current_time_usec(cx);
                            tm = t;
                            fsec = f;
                        }
                        DTK_ZULU => {
                            tmask = DTK_TIME_M | dtk_m(TZ);
                            dtype = DTK_TIME;
                            tm.hour = 0;
                            tm.min = 0;
                            tm.sec = 0;
                            tm.isdst = 0;
                        }
                        _ => return Err(DtErr::BadFormat),
                    },
                    DTZMOD => {
                        tmask |= dtk_m(DTZ);
                        tm.isdst = 1;
                        tzp -= val;
                    }
                    DTZ => {
                        tmask |= dtk_m(TZ);
                        tm.isdst = 1;
                        tzp = -val;
                        fields[i].ftype = DTK_TZ;
                    }
                    TZ => {
                        tm.isdst = 0;
                        tzp = -val;
                        fields[i].ftype = DTK_TZ;
                    }
                    DYNTZ => {
                        tmask |= dtk_m(TZ);
                        abbrev_tz = valtz;
                        abbrev = field.clone();
                        fields[i].ftype = DTK_TZ;
                    }
                    AMPM => mer = val,
                    ADBC => bc = val == BC,
                    UNITS | ISOTIME => {
                        tmask = 0;
                        if ptype != 0 {
                            return Err(DtErr::BadFormat);
                        }
                        ptype = val;
                    }
                    UNKNOWN_FIELD => {
                        let name = String::from_utf8_lossy(&field).into_owned();
                        match tz::pg_tzset(&name) {
                            Some(z) => named_tz = Some(TimeZone(z)),
                            None => return Err(DtErr::BadFormat),
                        }
                        tmask = dtk_m(TZ);
                    }
                    _ => return Err(DtErr::BadFormat),
                }
            }
            _ => return Err(DtErr::BadFormat),
        }
        if tmask & fmask != 0 {
            return Err(DtErr::BadFormat);
        }
        fmask |= tmask;
    }
    if ptype != 0 {
        return Err(DtErr::BadFormat);
    }
    validate_date(fmask, isjulian, is2digits, bc, &mut tm)?;
    if mer != HR24 && tm.hour > 12 {
        return Err(DtErr::FieldOverflow);
    }
    if mer == AM && tm.hour == 12 {
        tm.hour = 0;
    } else if mer == PM && tm.hour != 12 {
        tm.hour += 12;
    }
    if time_overflows(tm.hour, tm.min, tm.sec, fsec) {
        return Err(DtErr::FieldOverflow);
    }
    if (fmask & DTK_TIME_M) != DTK_TIME_M {
        return Err(DtErr::BadFormat);
    }
    let date_or_now = |tm: &Tm, fmask: i32| -> DtResult<Tm> {
        let mut tmp = if fmask & DTK_DATE_M == 0 {
            current_time_usec(cx).0
        } else {
            if (fmask & DTK_DATE_M) != DTK_DATE_M {
                return Err(DtErr::BadFormat);
            }
            Tm {
                year: tm.year,
                mon: tm.mon,
                mday: tm.mday,
                ..Default::default()
            }
        };
        tmp.hour = tm.hour;
        tmp.min = tm.min;
        tmp.sec = tm.sec;
        Ok(tmp)
    };
    if let Some(z) = &named_tz {
        if fmask & dtk_m(DTZMOD) != 0 {
            return Err(DtErr::BadFormat);
        }
        if let Some(g) = tz::pg_get_timezone_offset(z.pg()) {
            tzp = -(g as i32);
        } else {
            if (fmask & DTK_DATE_M) != DTK_DATE_M {
                return Err(DtErr::BadFormat);
            }
            let (off, isdst) = determine_tz_offset(&tm, z);
            tzp = off;
            tm.isdst = isdst;
        }
    }
    if let Some(z) = &abbrev_tz {
        if fmask & dtk_m(DTZMOD) != 0 {
            return Err(DtErr::BadFormat);
        }
        let tmp = date_or_now(&tm, fmask)?;
        let (off, isdst) = determine_tz_abbrev_offset(&tmp, &abbrev, z);
        tzp = off;
        tm.isdst = isdst;
    }
    if fmask & dtk_m(TZ) == 0 {
        if fmask & dtk_m(DTZMOD) != 0 {
            return Err(DtErr::BadFormat);
        }
        let tmp = date_or_now(&tm, fmask)?;
        let (off, isdst) = determine_tz_offset(&tmp, cx.tz);
        tzp = off;
        tm.isdst = isdst;
    }
    Ok(Decoded {
        dtype,
        tm,
        fsec,
        tz: tzp,
    })
}

// ── DecodeInterval ──

fn adjust_fract_microseconds(frac: f64, scale: i64, itm: &mut ItmIn) -> bool {
    if frac == 0.0 {
        return true;
    }
    let frac = frac * scale as f64;
    let mut usec = frac as i64;
    let rem = frac - usec as f64;
    if rem > 0.5 {
        usec += 1;
    } else if rem < -0.5 {
        usec -= 1;
    }
    match itm.usec.checked_add(usec) {
        Some(v) => {
            itm.usec = v;
            true
        }
        None => false,
    }
}

fn adjust_fract_days(frac: f64, scale: i32, itm: &mut ItmIn) -> bool {
    if frac == 0.0 {
        return true;
    }
    let frac = frac * scale as f64;
    let extra_days = frac as i32;
    match itm.mday.checked_add(extra_days) {
        Some(v) => itm.mday = v,
        None => return false,
    }
    adjust_fract_microseconds(frac - extra_days as f64, USECS_PER_DAY, itm)
}

fn adjust_fract_years(frac: f64, scale: i32, itm: &mut ItmIn) -> bool {
    let extra_months = (frac * scale as f64 * 12.0).round_ties_even() as i32;
    match itm.mon.checked_add(extra_months) {
        Some(v) => {
            itm.mon = v;
            true
        }
        None => false,
    }
}

fn adjust_microseconds(val: i64, fval: f64, scale: i64, itm: &mut ItmIn) -> bool {
    if !int64_multiply_add(val, scale, &mut itm.usec) {
        return false;
    }
    adjust_fract_microseconds(fval, scale, itm)
}

fn adjust_days(val: i64, scale: i32, itm: &mut ItmIn) -> bool {
    if val < i32::MIN as i64 || val > i32::MAX as i64 {
        return false;
    }
    match (val as i32)
        .checked_mul(scale)
        .and_then(|d| itm.mday.checked_add(d))
    {
        Some(v) => {
            itm.mday = v;
            true
        }
        None => false,
    }
}

fn adjust_months(val: i64, itm: &mut ItmIn) -> bool {
    if val < i32::MIN as i64 || val > i32::MAX as i64 {
        return false;
    }
    match itm.mon.checked_add(val as i32) {
        Some(v) => {
            itm.mon = v;
            true
        }
        None => false,
    }
}

fn adjust_years(val: i64, scale: i32, itm: &mut ItmIn) -> bool {
    if val < i32::MIN as i64 || val > i32::MAX as i64 {
        return false;
    }
    match (val as i32)
        .checked_mul(scale)
        .and_then(|y| itm.year.checked_add(y))
    {
        Some(v) => {
            itm.year = v;
            true
        }
        None => false,
    }
}

/// DecodeInterval: (dtype, itm_in).
pub fn decode_interval(
    fields: &[Field],
    range: i32,
    style: IntervalStyle,
) -> DtResult<(i32, ItmIn)> {
    let nf = fields.len();
    let mut force_negative = false;
    let mut is_before = false;
    let mut parsing_unit_val = false;
    let mut fmask = 0i32;
    let mut tmask;
    let mut ty = IGNORE_DTF;
    let mut dtype = DTK_DELTA;
    let mut itm = ItmIn::default();
    if style == IntervalStyle::SqlStandard && nf > 0 && fields[0].text.first() == Some(&b'-') {
        force_negative = true;
        for f in &fields[1..] {
            if matches!(f.text.first(), Some(b'-') | Some(b'+')) {
                force_negative = false;
                break;
            }
        }
    }
    for i in (0..nf).rev() {
        let field = &fields[i].text;
        let mut ftype = fields[i].ftype;
        if ftype == DTK_TIME {
            decode_time_for_interval(field, range, &mut itm)?;
            if force_negative && itm.usec > 0 {
                itm.usec = -itm.usec;
            }
            ty = DTK_DAY;
            parsing_unit_val = false;
            tmask = DTK_TIME_M;
            if tmask & fmask != 0 {
                return Err(DtErr::BadFormat);
            }
            fmask |= tmask;
            continue;
        }
        if ftype == DTK_TZ {
            if field[1..].contains(&b':') {
                let mut probe = itm;
                if decode_time_for_interval(&field[1..], range, &mut probe).is_ok() {
                    itm = probe;
                    if field[0] == b'-' {
                        if itm.usec == i64::MIN {
                            return Err(DtErr::FieldOverflow);
                        }
                        itm.usec = -itm.usec;
                    }
                    if force_negative && itm.usec > 0 {
                        itm.usec = -itm.usec;
                    }
                    ty = DTK_DAY;
                    parsing_unit_val = false;
                    tmask = DTK_TIME_M;
                    if tmask & fmask != 0 {
                        return Err(DtErr::BadFormat);
                    }
                    fmask |= tmask;
                    continue;
                }
            }
            ftype = DTK_NUMBER;
        }
        match ftype {
            DTK_DATE | DTK_NUMBER => {
                if ty == IGNORE_DTF {
                    ty = match range {
                        r if r == interval_mask(YEAR) => DTK_YEAR,
                        r if r == interval_mask(MONTH)
                            || r == interval_mask(YEAR) | interval_mask(MONTH) =>
                        {
                            DTK_MONTH
                        }
                        r if r == interval_mask(DAY) => DTK_DAY,
                        r if r == interval_mask(HOUR)
                            || r == interval_mask(DAY) | interval_mask(HOUR) =>
                        {
                            DTK_HOUR
                        }
                        r if r == interval_mask(MINUTE)
                            || r == interval_mask(HOUR) | interval_mask(MINUTE)
                            || r == interval_mask(DAY)
                                | interval_mask(HOUR)
                                | interval_mask(MINUTE) =>
                        {
                            DTK_MINUTE
                        }
                        _ => DTK_SECOND,
                    };
                }
                let (mut val, cp, over) = strtol(field, 0);
                if over {
                    return Err(DtErr::FieldOverflow);
                }
                let mut fval: f64;
                if field.get(cp) == Some(&b'-') {
                    let (mut val2, cp2, over2) = strtoint(field, cp + 1);
                    if over2 || !(0..12).contains(&val2) {
                        return Err(DtErr::FieldOverflow);
                    }
                    if cp2 < field.len() {
                        return Err(DtErr::BadFormat);
                    }
                    ty = DTK_MONTH;
                    if field[0] == b'-' {
                        val2 = -val2;
                    }
                    val = val
                        .checked_mul(12)
                        .and_then(|v| v.checked_add(val2 as i64))
                        .ok_or(DtErr::FieldOverflow)?;
                    fval = 0.0;
                } else if field.get(cp) == Some(&b'.') {
                    fval = parse_fraction(&field[cp..])?;
                    if field[0] == b'-' {
                        fval = -fval;
                    }
                } else if cp == field.len() {
                    fval = 0.0;
                } else {
                    return Err(DtErr::BadFormat);
                }
                if force_negative {
                    if val > 0 {
                        val = -val;
                    }
                    if fval > 0.0 {
                        fval = -fval;
                    }
                }
                let ok;
                match ty {
                    DTK_MICROSEC => {
                        ok = adjust_microseconds(val, fval, 1, &mut itm);
                        tmask = dtk_m(MICROSECOND);
                    }
                    DTK_MILLISEC => {
                        ok = adjust_microseconds(val, fval, 1000, &mut itm);
                        tmask = dtk_m(MILLISECOND);
                    }
                    DTK_SECOND => {
                        ok = adjust_microseconds(val, fval, USECS_PER_SEC, &mut itm);
                        tmask = if fval == 0.0 {
                            dtk_m(SECOND)
                        } else {
                            DTK_ALL_SECS_M
                        };
                    }
                    DTK_MINUTE => {
                        ok = adjust_microseconds(val, fval, USECS_PER_MINUTE, &mut itm);
                        tmask = dtk_m(MINUTE);
                    }
                    DTK_HOUR => {
                        ok = adjust_microseconds(val, fval, USECS_PER_HOUR, &mut itm);
                        tmask = dtk_m(HOUR);
                        ty = DTK_DAY;
                    }
                    DTK_DAY => {
                        ok = adjust_days(val, 1, &mut itm)
                            && adjust_fract_microseconds(fval, USECS_PER_DAY, &mut itm);
                        tmask = dtk_m(DAY);
                    }
                    DTK_WEEK => {
                        ok = adjust_days(val, 7, &mut itm) && adjust_fract_days(fval, 7, &mut itm);
                        tmask = dtk_m(WEEK);
                    }
                    DTK_MONTH => {
                        ok = adjust_months(val, &mut itm) && adjust_fract_days(fval, 30, &mut itm);
                        tmask = dtk_m(MONTH);
                    }
                    DTK_YEAR => {
                        ok = adjust_years(val, 1, &mut itm) && adjust_fract_years(fval, 1, &mut itm);
                        tmask = dtk_m(YEAR);
                    }
                    DTK_DECADE => {
                        ok = adjust_years(val, 10, &mut itm)
                            && adjust_fract_years(fval, 10, &mut itm);
                        tmask = dtk_m(DECADE);
                    }
                    DTK_CENTURY => {
                        ok = adjust_years(val, 100, &mut itm)
                            && adjust_fract_years(fval, 100, &mut itm);
                        tmask = dtk_m(CENTURY);
                    }
                    DTK_MILLENNIUM => {
                        ok = adjust_years(val, 1000, &mut itm)
                            && adjust_fract_years(fval, 1000, &mut itm);
                        tmask = dtk_m(MILLENNIUM);
                    }
                    _ => return Err(DtErr::BadFormat),
                }
                if !ok {
                    return Err(DtErr::FieldOverflow);
                }
                parsing_unit_val = false;
            }
            DTK_STRING | DTK_SPECIAL => {
                if parsing_unit_val {
                    return Err(DtErr::BadFormat);
                }
                let (mut t, mut uval) = decode_units(field);
                if t == UNKNOWN_FIELD {
                    (t, uval) = decode_special(field);
                }
                if t == IGNORE_DTF {
                    continue;
                }
                tmask = 0;
                match t {
                    UNITS => {
                        ty = uval;
                        parsing_unit_val = true;
                    }
                    AGO => {
                        if i != nf - 1 {
                            return Err(DtErr::BadFormat);
                        }
                        is_before = true;
                        ty = uval;
                    }
                    RESERV => {
                        tmask = DTK_DATE_M | DTK_TIME_M;
                        if uval != DTK_LATE && uval != DTK_EARLY {
                            return Err(DtErr::BadFormat);
                        }
                        if i != nf - 1 {
                            return Err(DtErr::BadFormat);
                        }
                        dtype = uval;
                    }
                    _ => return Err(DtErr::BadFormat),
                }
            }
            _ => return Err(DtErr::BadFormat),
        }
        if tmask & fmask != 0 {
            return Err(DtErr::BadFormat);
        }
        fmask |= tmask;
    }
    if fmask == 0 {
        return Err(DtErr::BadFormat);
    }
    if parsing_unit_val {
        return Err(DtErr::BadFormat);
    }
    if is_before {
        if itm.usec == i64::MIN
            || itm.mday == i32::MIN
            || itm.mon == i32::MIN
            || itm.year == i32::MIN
        {
            return Err(DtErr::FieldOverflow);
        }
        itm.usec = -itm.usec;
        itm.mday = -itm.mday;
        itm.mon = -itm.mon;
        itm.year = -itm.year;
    }
    Ok((dtype, itm))
}

/// ParseISO8601Number: (end, integer part, fraction).
fn parse_iso8601_number(s: &[u8], i: usize) -> DtResult<(usize, i64, f64)> {
    let c = s.get(i).copied().unwrap_or(0);
    if !(c.is_ascii_digit() || c == b'-' || c == b'.') {
        return Err(DtErr::BadFormat);
    }
    let (val, end) = strtod_prefix(s, i).ok_or(DtErr::BadFormat)?;
    if end == i {
        return Err(DtErr::BadFormat);
    }
    if val.is_nan() || !(-1.0e15..=1.0e15).contains(&val) {
        return Err(DtErr::FieldOverflow);
    }
    let ipart = if val >= 0.0 {
        val.floor() as i64
    } else {
        -((-val).floor() as i64)
    };
    Ok((end, ipart, val - ipart as f64))
}

fn iso8601_integer_width(s: &[u8], start: usize) -> usize {
    let mut p = start;
    if s.get(p) == Some(&b'-') {
        p += 1;
    }
    s[p..].iter().take_while(|b| b.is_ascii_digit()).count()
}

/// DecodeISO8601Interval.
pub fn decode_iso8601_interval(s: &str) -> DtResult<(i32, ItmIn)> {
    let s = s.as_bytes();
    let mut itm = ItmIn::default();
    let mut datepart = true;
    let mut havefield = false;
    if s.len() < 2 || s[0] != b'P' {
        return Err(DtErr::BadFormat);
    }
    let mut p = 1;
    macro_rules! chk {
        ($e:expr) => {
            if !$e {
                return Err(DtErr::FieldOverflow);
            }
        };
    }
    while p < s.len() {
        if s[p] == b'T' {
            datepart = false;
            havefield = false;
            p += 1;
            continue;
        }
        let fieldstart = p;
        let (end, val, fval) = parse_iso8601_number(s, p)?;
        p = end;
        let unit = s.get(p).copied().unwrap_or(0);
        p += 1;
        if datepart {
            match unit {
                b'Y' => chk!(adjust_years(val, 1, &mut itm) && adjust_fract_years(fval, 1, &mut itm)),
                b'M' => chk!(adjust_months(val, &mut itm) && adjust_fract_days(fval, 30, &mut itm)),
                b'W' => chk!(adjust_days(val, 7, &mut itm) && adjust_fract_days(fval, 7, &mut itm)),
                b'D' => chk!(
                    adjust_days(val, 1, &mut itm)
                        && adjust_fract_microseconds(fval, USECS_PER_DAY, &mut itm)
                ),
                b'T' | 0 | b'-' => {
                    if unit != b'-' && iso8601_integer_width(s, fieldstart) == 8 && !havefield {
                        chk!(
                            adjust_years(val / 10000, 1, &mut itm)
                                && adjust_months((val / 100) % 100, &mut itm)
                                && adjust_days(val % 100, 1, &mut itm)
                                && adjust_fract_microseconds(fval, USECS_PER_DAY, &mut itm)
                        );
                        if unit == 0 {
                            return Ok((DTK_DELTA, itm));
                        }
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    if havefield {
                        return Err(DtErr::BadFormat);
                    }
                    chk!(adjust_years(val, 1, &mut itm) && adjust_fract_years(fval, 1, &mut itm));
                    if unit == 0 {
                        return Ok((DTK_DELTA, itm));
                    }
                    if unit == b'T' {
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    let (end, val, fval) = parse_iso8601_number(s, p)?;
                    p = end;
                    chk!(adjust_months(val, &mut itm) && adjust_fract_days(fval, 30, &mut itm));
                    if p >= s.len() {
                        return Ok((DTK_DELTA, itm));
                    }
                    if s[p] == b'T' {
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    if s[p] != b'-' {
                        return Err(DtErr::BadFormat);
                    }
                    p += 1;
                    let (end, val, fval) = parse_iso8601_number(s, p)?;
                    p = end;
                    chk!(
                        adjust_days(val, 1, &mut itm)
                            && adjust_fract_microseconds(fval, USECS_PER_DAY, &mut itm)
                    );
                    if p >= s.len() {
                        return Ok((DTK_DELTA, itm));
                    }
                    if s[p] == b'T' {
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    return Err(DtErr::BadFormat);
                }
                _ => return Err(DtErr::BadFormat),
            }
        } else {
            match unit {
                b'H' => chk!(adjust_microseconds(val, fval, USECS_PER_HOUR, &mut itm)),
                b'M' => chk!(adjust_microseconds(val, fval, USECS_PER_MINUTE, &mut itm)),
                b'S' => chk!(adjust_microseconds(val, fval, USECS_PER_SEC, &mut itm)),
                0 | b':' => {
                    if unit == 0 && iso8601_integer_width(s, fieldstart) == 6 && !havefield {
                        chk!(
                            adjust_microseconds(val / 10000, 0.0, USECS_PER_HOUR, &mut itm)
                                && adjust_microseconds((val / 100) % 100, 0.0, USECS_PER_MINUTE, &mut itm)
                                && adjust_microseconds(val % 100, 0.0, USECS_PER_SEC, &mut itm)
                                && adjust_fract_microseconds(fval, 1, &mut itm)
                        );
                        return Ok((DTK_DELTA, itm));
                    }
                    if havefield {
                        return Err(DtErr::BadFormat);
                    }
                    chk!(adjust_microseconds(val, fval, USECS_PER_HOUR, &mut itm));
                    if unit == 0 {
                        return Ok((DTK_DELTA, itm));
                    }
                    let (end, val, fval) = parse_iso8601_number(s, p)?;
                    p = end;
                    chk!(adjust_microseconds(val, fval, USECS_PER_MINUTE, &mut itm));
                    if p >= s.len() {
                        return Ok((DTK_DELTA, itm));
                    }
                    if s[p] != b':' {
                        return Err(DtErr::BadFormat);
                    }
                    p += 1;
                    let (end, val, fval) = parse_iso8601_number(s, p)?;
                    p = end;
                    chk!(adjust_microseconds(val, fval, USECS_PER_SEC, &mut itm));
                    if p >= s.len() {
                        return Ok((DTK_DELTA, itm));
                    }
                    return Err(DtErr::BadFormat);
                }
                _ => return Err(DtErr::BadFormat),
            }
        }
        havefield = true;
    }
    Ok((DTK_DELTA, itm))
}

// ── encoding ──

/// AppendSeconds.
fn append_seconds(out: &mut String, sec: i32, fsec: i32, precision: usize, fillzeros: bool) {
    let s = sec.unsigned_abs();
    if fillzeros {
        let _ = write!(out, "{s:02}");
    } else {
        let _ = write!(out, "{s}");
    }
    if fsec != 0 {
        let mut value = fsec.unsigned_abs();
        out.push('.');
        let mut digits = vec![b'0'; precision];
        let mut end = precision;
        let mut gotnonzero = false;
        let mut p = precision;
        while p > 0 {
            p -= 1;
            let rem = value % 10;
            value /= 10;
            if rem != 0 {
                gotnonzero = true;
            }
            if gotnonzero {
                digits[p] = b'0' + rem as u8;
            } else {
                end = p;
            }
        }
        if value != 0 {
            let _ = write!(out, "{}", fsec.unsigned_abs());
            return;
        }
        out.push_str(std::str::from_utf8(&digits[..end]).unwrap_or(""));
    }
}

/// EncodeTimezone: `tz` is seconds west.
fn encode_timezone(out: &mut String, tz: i32, style: DateFmt) {
    let mut sec = tz.unsigned_abs();
    let mut min = sec / 60;
    sec -= min * 60;
    let hour = min / 60;
    min -= hour * 60;
    out.push(if tz <= 0 { '+' } else { '-' });
    if sec != 0 {
        let _ = write!(out, "{hour:02}:{min:02}:{sec:02}");
    } else if min != 0 || style == DateFmt::Xsd {
        let _ = write!(out, "{hour:02}:{min:02}");
    } else {
        let _ = write!(out, "{hour:02}");
    }
}

fn year_disp(y: i32) -> i64 {
    if y > 0 { y as i64 } else { -(y as i64 - 1) }
}

/// EncodeDateOnly.
pub fn encode_date_only(tm: &Tm, style: DateStyle) -> String {
    let y = year_disp(tm.year);
    let (m, d) = (tm.mon, tm.mday);
    let mut s = match (style.fmt, style.order) {
        (DateFmt::Iso | DateFmt::Xsd, _) => format!("{y:04}-{m:02}-{d:02}"),
        (DateFmt::Sql, DateOrder::Dmy) => format!("{d:02}/{m:02}/{y:04}"),
        (DateFmt::Sql, _) => format!("{m:02}/{d:02}/{y:04}"),
        (DateFmt::German, _) => format!("{d:02}.{m:02}.{y:04}"),
        (DateFmt::Postgres, DateOrder::Dmy) => format!("{d:02}-{m:02}-{y:04}"),
        (DateFmt::Postgres, _) => format!("{m:02}-{d:02}-{y:04}"),
    };
    if tm.year <= 0 {
        s.push_str(" BC");
    }
    s
}

/// EncodeTimeOnly; `tz` is seconds west.
pub fn encode_time_only(tm: &Tm, fsec: i32, tz: Option<i32>, style: DateFmt) -> String {
    let mut s = format!("{:02}:{:02}:", tm.hour, tm.min);
    append_seconds(&mut s, tm.sec, fsec, MAX_TIMESTAMP_PRECISION, true);
    if let Some(tz) = tz {
        encode_timezone(&mut s, tz, style);
    }
    s
}

/// EncodeDateTime; `tz` is seconds west, `tzn` the abbreviation (if printing a zone).
pub fn encode_date_time(
    tm: &Tm,
    fsec: i32,
    print_tz: bool,
    tz: i32,
    tzn: Option<&str>,
    style: DateStyle,
) -> String {
    let print_tz = print_tz && tm.isdst >= 0;
    let y = year_disp(tm.year);
    let mut s = String::with_capacity(40);
    let tzname = |s: &mut String| {
        if let Some(n) = tzn {
            s.push(' ');
            s.extend(n.chars().take(MAXTZLEN));
            true
        } else {
            false
        }
    };
    match style.fmt {
        DateFmt::Iso | DateFmt::Xsd => {
            let _ = write!(
                s,
                "{y:04}-{:02}-{:02}{}{:02}:{:02}:",
                tm.mon,
                tm.mday,
                if style.fmt == DateFmt::Iso { ' ' } else { 'T' },
                tm.hour,
                tm.min
            );
            append_seconds(&mut s, tm.sec, fsec, MAX_TIMESTAMP_PRECISION, true);
            if print_tz {
                encode_timezone(&mut s, tz, style.fmt);
            }
        }
        DateFmt::Sql => {
            if style.order == DateOrder::Dmy {
                let _ = write!(s, "{:02}/{:02}", tm.mday, tm.mon);
            } else {
                let _ = write!(s, "{:02}/{:02}", tm.mon, tm.mday);
            }
            let _ = write!(s, "/{y:04} {:02}:{:02}:", tm.hour, tm.min);
            append_seconds(&mut s, tm.sec, fsec, MAX_TIMESTAMP_PRECISION, true);
            if print_tz && !tzname(&mut s) {
                encode_timezone(&mut s, tz, style.fmt);
            }
        }
        DateFmt::German => {
            let _ = write!(
                s,
                "{:02}.{:02}.{y:04} {:02}:{:02}:",
                tm.mday, tm.mon, tm.hour, tm.min
            );
            append_seconds(&mut s, tm.sec, fsec, MAX_TIMESTAMP_PRECISION, true);
            if print_tz && !tzname(&mut s) {
                encode_timezone(&mut s, tz, style.fmt);
            }
        }
        DateFmt::Postgres => {
            let wday = j2day(date2j(tm.year, tm.mon, tm.mday));
            s.push_str(&DAYS[wday as usize][..3]);
            s.push(' ');
            let mon = MONTHS[(tm.mon - 1).clamp(0, 11) as usize];
            if style.order == DateOrder::Dmy {
                let _ = write!(s, "{:02} {mon}", tm.mday);
            } else {
                let _ = write!(s, "{mon} {:02}", tm.mday);
            }
            let _ = write!(s, " {:02}:{:02}:", tm.hour, tm.min);
            append_seconds(&mut s, tm.sec, fsec, MAX_TIMESTAMP_PRECISION, true);
            let _ = write!(s, " {y:04}");
            if print_tz && !tzname(&mut s) {
                s.push(' ');
                encode_timezone(&mut s, tz, style.fmt);
            }
        }
    }
    if tm.year <= 0 {
        s.push_str(" BC");
    }
    s
}

/// EncodeInterval.
pub fn encode_interval(itm: &Itm, style: IntervalStyle) -> String {
    let (mut year, mut mon) = (itm.year as i64, itm.mon as i64);
    let (mut mday, mut hour) = (itm.mday as i64, itm.hour);
    let (mut min, mut sec, mut fsec) = (itm.min as i64, itm.sec, itm.usec);
    let mut is_before = false;
    let mut is_zero = true;
    let mut out = String::new();
    match style {
        IntervalStyle::SqlStandard => {
            let has_negative =
                year < 0 || mon < 0 || mday < 0 || hour < 0 || min < 0 || sec < 0 || fsec < 0;
            let has_positive =
                year > 0 || mon > 0 || mday > 0 || hour > 0 || min > 0 || sec > 0 || fsec > 0;
            let has_year_month = year != 0 || mon != 0;
            let has_day_time = mday != 0 || hour != 0 || min != 0 || sec != 0 || fsec != 0;
            let has_day = mday != 0;
            let sql_standard_value =
                !(has_negative && has_positive) && !(has_year_month && has_day_time);
            if has_negative && sql_standard_value {
                out.push('-');
                year = -year;
                mon = -mon;
                mday = -mday;
                hour = hour.wrapping_neg();
                min = -min;
                sec = -sec;
                fsec = -fsec;
            }
            if !has_negative && !has_positive {
                out.push('0');
            } else if !sql_standard_value {
                let year_sign = if year < 0 || mon < 0 { '-' } else { '+' };
                let day_sign = if mday < 0 { '-' } else { '+' };
                let sec_sign = if hour < 0 || min < 0 || sec < 0 || fsec < 0 {
                    '-'
                } else {
                    '+'
                };
                let _ = write!(
                    out,
                    "{year_sign}{}-{} {day_sign}{} {sec_sign}{}:{:02}:",
                    year.abs(),
                    mon.abs(),
                    mday.unsigned_abs(),
                    hour.unsigned_abs(),
                    min.abs()
                );
                append_seconds(&mut out, sec, fsec, MAX_INTERVAL_PRECISION, true);
            } else if has_year_month {
                let _ = write!(out, "{year}-{mon}");
            } else if has_day {
                let _ = write!(out, "{mday} {hour}:{min:02}:");
                append_seconds(&mut out, sec, fsec, MAX_INTERVAL_PRECISION, true);
            } else {
                let _ = write!(out, "{hour}:{min:02}:");
                append_seconds(&mut out, sec, fsec, MAX_INTERVAL_PRECISION, true);
            }
        }
        IntervalStyle::Iso8601 => {
            if year == 0 && mon == 0 && mday == 0 && hour == 0 && min == 0 && sec == 0 && fsec == 0
            {
                return "PT0S".into();
            }
            out.push('P');
            let part = |out: &mut String, v: i64, u: char| {
                if v != 0 {
                    let _ = write!(out, "{v}{u}");
                }
            };
            part(&mut out, year, 'Y');
            part(&mut out, mon, 'M');
            part(&mut out, mday, 'D');
            if hour != 0 || min != 0 || sec != 0 || fsec != 0 {
                out.push('T');
            }
            part(&mut out, hour, 'H');
            part(&mut out, min, 'M');
            if sec != 0 || fsec != 0 {
                if sec < 0 || fsec < 0 {
                    out.push('-');
                }
                append_seconds(&mut out, sec, fsec, MAX_INTERVAL_PRECISION, false);
                out.push('S');
            }
        }
        IntervalStyle::Postgres => {
            let part = |out: &mut String, v: i64, u: &str, is_zero: &mut bool, is_before: &mut bool| {
                if v == 0 {
                    return;
                }
                let _ = write!(
                    out,
                    "{}{}{v} {u}{}",
                    if !*is_zero { " " } else { "" },
                    if *is_before && v > 0 { "+" } else { "" },
                    if v != 1 { "s" } else { "" }
                );
                *is_before = v < 0;
                *is_zero = false;
            };
            part(&mut out, year, "year", &mut is_zero, &mut is_before);
            part(&mut out, mon, "mon", &mut is_zero, &mut is_before);
            part(&mut out, mday, "day", &mut is_zero, &mut is_before);
            if is_zero || hour != 0 || min != 0 || sec != 0 || fsec != 0 {
                let minus = hour < 0 || min < 0 || sec < 0 || fsec < 0;
                let _ = write!(
                    out,
                    "{}{}{:02}:{:02}:",
                    if is_zero { "" } else { " " },
                    if minus {
                        "-"
                    } else if is_before {
                        "+"
                    } else {
                        ""
                    },
                    hour.unsigned_abs(),
                    min.abs()
                );
                append_seconds(&mut out, sec, fsec, MAX_INTERVAL_PRECISION, true);
            }
        }
        IntervalStyle::PostgresVerbose => {
            out.push('@');
            let part = |out: &mut String, v: i64, u: &str, is_zero: &mut bool, is_before: &mut bool| {
                if v == 0 {
                    return;
                }
                let mut v = v;
                if *is_zero {
                    *is_before = v < 0;
                    v = v.wrapping_abs();
                } else if *is_before {
                    v = -v;
                }
                let _ = write!(out, " {v} {u}{}", if v == 1 { "" } else { "s" });
                *is_zero = false;
            };
            part(&mut out, year, "year", &mut is_zero, &mut is_before);
            part(&mut out, mon, "mon", &mut is_zero, &mut is_before);
            part(&mut out, mday, "day", &mut is_zero, &mut is_before);
            part(&mut out, hour, "hour", &mut is_zero, &mut is_before);
            part(&mut out, min, "min", &mut is_zero, &mut is_before);
            if sec != 0 || fsec != 0 {
                out.push(' ');
                if sec < 0 || (sec == 0 && fsec < 0) {
                    if is_zero {
                        is_before = true;
                    } else if !is_before {
                        out.push('-');
                    }
                } else if is_before {
                    out.push('-');
                }
                append_seconds(&mut out, sec, fsec, MAX_INTERVAL_PRECISION, false);
                out.push_str(if sec.abs() != 1 || fsec != 0 {
                    " secs"
                } else {
                    " sec"
                });
                is_zero = false;
            }
            if is_zero {
                out.push_str(" 0");
            }
            if is_before {
                out.push_str(" ago");
            }
        }
    }
    out
}

// ── output entry points ──

pub fn format_date_styled(d: i32, style: DateStyle) -> String {
    match d {
        DATE_NEG_INF => "-infinity".into(),
        DATE_POS_INF => "infinity".into(),
        _ => {
            let (year, mon, mday) = j2date(d.wrapping_add(POSTGRES_EPOCH_JDATE));
            encode_date_only(
                &Tm {
                    year,
                    mon,
                    mday,
                    ..Default::default()
                },
                style,
            )
        }
    }
}

pub fn format_date(d: i32) -> String {
    format_date_styled(d, DateStyle::default())
}

pub fn format_timestamp_styled(ts: i64, style: DateStyle) -> String {
    match ts {
        TS_NEG_INF => "-infinity".into(),
        TS_POS_INF => "infinity".into(),
        _ => match timestamp2tm(ts, None) {
            Some((tm, fsec)) => encode_date_time(&tm, fsec, false, 0, None, style),
            None => "(timestamp out of range)".into(),
        },
    }
}

pub fn format_timestamp(ts: i64) -> String {
    format_timestamp_styled(ts, DateStyle::default())
}

pub fn format_timestamptz_styled(ts: i64, tz: &TimeZone, style: DateStyle) -> String {
    match ts {
        TS_NEG_INF => "-infinity".into(),
        TS_POS_INF => "infinity".into(),
        _ => match timestamp2tm(ts, Some(tz)) {
            Some((tm, fsec)) => {
                let tzw = -tm.gmtoff as i32;
                encode_date_time(&tm, fsec, true, tzw, tm.zone.as_deref(), style)
            }
            None => "(timestamp out of range)".into(),
        },
    }
}

pub fn format_timestamptz(ts: i64, tz: &TimeZone) -> String {
    format_timestamptz_styled(ts, tz, DateStyle::default())
}

/// EncodeDateTime with USE_XSD_DATES for a finite timestamp (`tz` for timestamptz).
pub fn format_timestamp_xsd(ts: i64, tz: Option<&TimeZone>) -> String {
    let style = DateStyle {
        fmt: DateFmt::Xsd,
        order: DateOrder::Mdy,
    };
    match timestamp2tm(ts, tz) {
        Some((tm, fsec)) => {
            let tzw = -tm.gmtoff as i32;
            encode_date_time(&tm, fsec, tz.is_some(), tzw, None, style)
        }
        None => String::new(),
    }
}

/// JsonEncodeDateTime for a timestamp (`tz` for timestamptz: the zone to show it in).
pub fn json_encode_timestamp(ts: i64, tz: Option<&TimeZone>) -> String {
    match ts {
        TS_NEG_INF => "-infinity".into(),
        TS_POS_INF => "infinity".into(),
        _ => format_timestamp_xsd(ts, tz),
    }
}

/// JsonEncodeDateTime for a date.
pub fn json_encode_date(d: i32) -> String {
    format_date_styled(
        d,
        DateStyle {
            fmt: DateFmt::Xsd,
            order: DateOrder::Mdy,
        },
    )
}

/// JsonEncodeDateTime for a timetz (`west`: the zone in seconds west).
pub fn json_encode_timetz(us: i64, west: i32) -> String {
    let (hour, min, sec, fsec) = dt2time(us);
    encode_time_only(
        &Tm {
            hour,
            min,
            sec,
            ..Default::default()
        },
        fsec,
        Some(west),
        DateFmt::Xsd,
    )
}

/// `HH:MM:SS[.ffffff]` (time_out).
pub fn format_time_of_day(us: i64) -> String {
    let (hour, min, sec, fsec) = dt2time(us);
    encode_time_only(
        &Tm {
            hour,
            min,
            sec,
            ..Default::default()
        },
        fsec,
        None,
        DateFmt::Iso,
    )
}

/// timetz_out; `west` is the zone in seconds west.
pub fn format_timetz(us: i64, west: i32) -> String {
    let (hour, min, sec, fsec) = dt2time(us);
    encode_time_only(
        &Tm {
            hour,
            min,
            sec,
            ..Default::default()
        },
        fsec,
        Some(west),
        DateFmt::Iso,
    )
}

pub fn format_interval_styled(iv: &Interval, style: IntervalStyle) -> String {
    if iv.is_pos_inf() {
        return "infinity".into();
    }
    if iv.is_neg_inf() {
        return "-infinity".into();
    }
    encode_interval(&interval2itm(iv), style)
}

pub fn format_interval(iv: &Interval) -> String {
    format_interval_styled(iv, IntervalStyle::Postgres)
}

pub fn format_interval_sql(iv: &Interval) -> String {
    format_interval_styled(iv, IntervalStyle::SqlStandard)
}

pub fn format_interval_verbose(iv: &Interval) -> String {
    format_interval_styled(iv, IntervalStyle::PostgresVerbose)
}

pub fn format_interval_iso(iv: &Interval) -> String {
    format_interval_styled(iv, IntervalStyle::Iso8601)
}

// ── input entry points ──

/// Current transaction timestamp, needed for 'now', 'today' etc.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    pub now: i64,
}

impl Clock {
    pub fn system() -> Clock {
        let d = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Clock {
            now: d.as_micros() as i64 - UNIX_EPOCH_OFFSET_SECS * USECS_PER_SEC,
        }
    }
}

fn ctx<'a>(tz: &'a TimeZone, clock: Clock, order: DateOrder) -> DtCtx<'a> {
    DtCtx {
        tz,
        now: clock.now,
        order,
        interval_style: IntervalStyle::Postgres,
        abbrevs: "Default",
    }
}

fn decode_dt(s: &str, buflen: usize, cx: &DtCtx, datatype: &str) -> Result<Decoded> {
    let mut f = parse_date_time(s, buflen).map_err(|e| dt_parse_error(e, s, datatype))?;
    decode_date_time(&mut f, cx).map_err(|e| dt_parse_error(e, s, datatype))
}

/// date_in.
pub fn parse_date_cx(s: &str, cx: &DtCtx) -> Result<i32> {
    let d = decode_dt(s, MAXDATELEN + 1, cx, "date")?;
    let mut tm = d.tm;
    match d.dtype {
        DTK_DATE => {}
        DTK_EPOCH => {
            tm.year = 1970;
            tm.mon = 1;
            tm.mday = 1;
        }
        DTK_LATE => return Ok(DATE_POS_INF),
        DTK_EARLY => return Ok(DATE_NEG_INF),
        _ => return Err(dt_parse_error(DtErr::BadFormat, s, "date")),
    }
    if !is_valid_julian(tm.year, tm.mon, tm.mday) {
        bail!(DATETIME_FIELD_OVERFLOW, "date out of range: \"{s}\"");
    }
    let date = date2j(tm.year, tm.mon, tm.mday) - POSTGRES_EPOCH_JDATE;
    if !is_valid_date(date) {
        bail!(DATETIME_FIELD_OVERFLOW, "date out of range: \"{s}\"");
    }
    Ok(date)
}

pub fn parse_date(s: &str, tz: &TimeZone, clock: Clock, order: DateOrder) -> Result<i32> {
    parse_date_cx(s, &ctx(tz, clock, order))
}

fn timestamp_in(s: &str, cx: &DtCtx, with_tz: bool) -> Result<i64> {
    let ty = if with_tz {
        "timestamp with time zone"
    } else {
        "timestamp"
    };
    let d = decode_dt(s, MAXDATELEN + MAXDATEFIELDS, cx, ty)?;
    match d.dtype {
        DTK_DATE => tm2timestamp(&d.tm, d.fsec, with_tz.then_some(d.tz))
            .ok_or_else(|| err!(DATETIME_FIELD_OVERFLOW, "timestamp out of range: \"{s}\"")),
        DTK_EPOCH => Ok(-UNIX_EPOCH_OFFSET_SECS * USECS_PER_SEC),
        DTK_LATE => Ok(TS_POS_INF),
        DTK_EARLY => Ok(TS_NEG_INF),
        _ => Err(dt_parse_error(DtErr::BadFormat, s, ty)),
    }
}

/// timestamp_in.
pub fn parse_timestamp_cx(s: &str, cx: &DtCtx) -> Result<i64> {
    timestamp_in(s, cx, false)
}

/// timestamptz_in.
pub fn parse_timestamptz_cx(s: &str, cx: &DtCtx) -> Result<i64> {
    timestamp_in(s, cx, true)
}

pub fn parse_timestamp(s: &str, tz: &TimeZone, clock: Clock, order: DateOrder) -> Result<i64> {
    parse_timestamp_cx(s, &ctx(tz, clock, order))
}

pub fn parse_timestamptz(s: &str, tz: &TimeZone, clock: Clock, order: DateOrder) -> Result<i64> {
    parse_timestamptz_cx(s, &ctx(tz, clock, order))
}

fn decode_t(s: &str, cx: &DtCtx, datatype: &str) -> Result<Decoded> {
    let mut f = parse_date_time(s, MAXDATELEN + 1).map_err(|e| dt_parse_error(e, s, datatype))?;
    decode_time_only(&mut f, cx).map_err(|e| dt_parse_error(e, s, datatype))
}

/// time_in.
pub fn parse_time_cx(s: &str, cx: &DtCtx) -> Result<i64> {
    let d = decode_t(s, cx, "time")?;
    Ok(time2t(d.tm.hour, d.tm.min, d.tm.sec, d.fsec))
}

/// timetz_in: (time, zone seconds west).
pub fn parse_timetz_cx(s: &str, cx: &DtCtx) -> Result<(i64, i32)> {
    let d = decode_t(s, cx, "time with time zone")?;
    Ok((time2t(d.tm.hour, d.tm.min, d.tm.sec, d.fsec), d.tz))
}

pub fn parse_time(s: &str) -> Result<i64> {
    let tz = TimeZone::default();
    parse_time_cx(s, &ctx(&tz, Clock::system(), DateOrder::Mdy))
}

pub fn parse_timetz(s: &str, tz: &TimeZone, clock: Clock) -> Result<(i64, i32)> {
    parse_timetz_cx(s, &ctx(tz, clock, DateOrder::Mdy))
}

/// interval_in with an IntervalStyle and typmod.
pub fn parse_interval_ex(input: &str, style: IntervalStyle, typmod: i32) -> Result<Interval> {
    let range = if typmod >= 0 {
        (typmod >> 16) & 0x7FFF
    } else {
        INTERVAL_FULL_RANGE
    };
    let mut r = parse_date_time(input, 256)
        .and_then(|f| decode_interval(&f, range, style));
    if matches!(r, Err(DtErr::BadFormat)) {
        r = decode_iso8601_interval(input);
    }
    let (dtype, itm) = match r {
        Ok(x) => x,
        Err(e) => {
            let e = if e == DtErr::FieldOverflow {
                DtErr::IntervalOverflow
            } else {
                e
            };
            return Err(dt_parse_error(e, input, "interval"));
        }
    };
    let iv = match dtype {
        DTK_DELTA => itmin2interval(&itm)
            .ok_or_else(|| err!(DATETIME_FIELD_OVERFLOW, "interval out of range"))?,
        DTK_LATE => Interval::POS_INF,
        DTK_EARLY => Interval::NEG_INF,
        _ => return Err(dt_parse_error(DtErr::BadFormat, input, "interval")),
    };
    adjust_interval_for_typmod(iv, typmod)
}

/// Parse interval input (postgres IntervalStyle, no typmod).
pub fn parse_interval(input: &str) -> Result<Interval> {
    parse_interval_ex(input, IntervalStyle::Postgres, -1)
}

/// AdjustIntervalForTypmod.
pub fn adjust_interval_for_typmod(iv: Interval, typmod: i32) -> Result<Interval> {
    if iv.is_infinite() || typmod < 0 {
        return Ok(iv);
    }
    let range = (typmod >> 16) & 0x7FFF;
    let precision = typmod & 0xFFFF;
    let mut r = iv;
    let m = interval_mask;
    if range == INTERVAL_FULL_RANGE {
    } else if range == m(YEAR) {
        r.months = (r.months / 12) * 12;
        r.days = 0;
        r.micros = 0;
    } else if range == m(MONTH) || range == m(YEAR) | m(MONTH) {
        r.days = 0;
        r.micros = 0;
    } else if range == m(DAY) {
        r.micros = 0;
    } else if range == m(HOUR) || range == m(DAY) | m(HOUR) {
        r.micros = (r.micros / USECS_PER_HOUR) * USECS_PER_HOUR;
    } else if range == m(MINUTE)
        || range == m(DAY) | m(HOUR) | m(MINUTE)
        || range == m(HOUR) | m(MINUTE)
    {
        r.micros = (r.micros / USECS_PER_MINUTE) * USECS_PER_MINUTE;
    }
    if precision != INTERVAL_FULL_PRECISION {
        if !(0..=MAX_INTERVAL_PRECISION as i32).contains(&precision) {
            bail!(
                INVALID_PARAMETER_VALUE,
                "interval({precision}) precision must be between 0 and 6"
            );
        }
        let scale = 10i64.pow(6 - precision as u32);
        let offset = if precision == 6 { 0 } else { scale / 2 };
        if r.micros >= 0 {
            r.micros = r.micros.checked_add(offset).ok_or_else(interval_overflow)?;
        } else {
            r.micros = r.micros.checked_sub(offset).ok_or_else(interval_overflow)?;
        }
        r.micros -= r.micros % scale;
    }
    Ok(r)
}

/// Apply an interval typmod (`interval hour to minute`, `interval(2)`).
pub fn interval_apply_typmod(iv: Interval, typmod: i32) -> Interval {
    adjust_interval_for_typmod(iv, typmod).unwrap_or(iv)
}

/// AdjustTimestampForTypmod / AdjustTimeForTypmod: round to `typmod` fractional digits.
pub fn round_micros(us: i64, typmod: i32) -> i64 {
    if !(0..6).contains(&typmod) || us == TS_POS_INF || us == TS_NEG_INF {
        return us;
    }
    let scale = 10i64.pow(6 - typmod as u32);
    let offset = scale / 2;
    if us >= 0 {
        us.saturating_add(offset) / scale * scale
    } else {
        -((us.saturating_neg().saturating_add(offset)) / scale * scale)
    }
}

// ── arithmetic (timestamp.c) ──

pub fn interval_overflow() -> Error {
    err!(DATETIME_FIELD_OVERFLOW, "interval out of range")
}

fn ts_overflow() -> Error {
    err!(DATETIME_FIELD_OVERFLOW, "timestamp out of range")
}

/// Month arithmetic on broken-down time (shared by the timestamp + interval paths).
fn add_months(tm: &mut Tm, months: i32) -> Result<()> {
    tm.mon = tm.mon.checked_add(months).ok_or_else(ts_overflow)?;
    if tm.mon > 12 {
        tm.year += (tm.mon - 1) / 12;
        tm.mon = ((tm.mon - 1) % 12) + 1;
    } else if tm.mon < 1 {
        tm.year += tm.mon / 12 - 1;
        tm.mon = tm.mon % 12 + 12;
    }
    let dim = DAY_TAB[isleap(tm.year) as usize][(tm.mon - 1) as usize];
    if tm.mday > dim {
        tm.mday = dim;
    }
    Ok(())
}

/// timestamp_pl_interval.
pub fn ts_add_interval(ts: i64, iv: &Interval) -> Result<i64> {
    if iv.is_neg_inf() {
        if ts == TS_POS_INF {
            return Err(ts_overflow());
        }
        return Ok(TS_NEG_INF);
    }
    if iv.is_pos_inf() {
        if ts == TS_NEG_INF {
            return Err(ts_overflow());
        }
        return Ok(TS_POS_INF);
    }
    if !ts_is_finite(ts) {
        return Ok(ts);
    }
    let mut ts = ts;
    if iv.months != 0 {
        let (mut tm, fsec) = timestamp2tm(ts, None).ok_or_else(ts_overflow)?;
        add_months(&mut tm, iv.months)?;
        ts = tm2timestamp(&tm, fsec, None).ok_or_else(ts_overflow)?;
    }
    if iv.days != 0 {
        let (mut tm, fsec) = timestamp2tm(ts, None).ok_or_else(ts_overflow)?;
        let julian = date2j(tm.year, tm.mon, tm.mday)
            .checked_add(iv.days)
            .filter(|j| *j >= 0)
            .ok_or_else(ts_overflow)?;
        (tm.year, tm.mon, tm.mday) = j2date(julian);
        ts = tm2timestamp(&tm, fsec, None).ok_or_else(ts_overflow)?;
    }
    ts = ts.checked_add(iv.micros).ok_or_else(ts_overflow)?;
    ts_range_check(ts)
}

/// timestamptz_pl_interval_internal: months and days move in local time in `tz`.
pub fn tstz_add_interval(ts: i64, iv: &Interval, tz: &TimeZone) -> Result<i64> {
    if iv.is_neg_inf() {
        if ts == TS_POS_INF {
            return Err(ts_overflow());
        }
        return Ok(TS_NEG_INF);
    }
    if iv.is_pos_inf() {
        if ts == TS_NEG_INF {
            return Err(ts_overflow());
        }
        return Ok(TS_POS_INF);
    }
    if !ts_is_finite(ts) {
        return Ok(ts);
    }
    let mut ts = ts;
    if iv.months != 0 {
        let (mut tm, fsec) = timestamp2tm(ts, Some(tz)).ok_or_else(ts_overflow)?;
        add_months(&mut tm, iv.months)?;
        let (off, _) = determine_tz_offset(&tm, tz);
        ts = tm2timestamp(&tm, fsec, Some(off)).ok_or_else(ts_overflow)?;
    }
    if iv.days != 0 {
        let (mut tm, fsec) = timestamp2tm(ts, Some(tz)).ok_or_else(ts_overflow)?;
        let julian = date2j(tm.year, tm.mon, tm.mday)
            .checked_add(iv.days)
            .filter(|j| *j >= -1)
            .ok_or_else(ts_overflow)?;
        (tm.year, tm.mon, tm.mday) = j2date(julian);
        let (off, _) = determine_tz_offset(&tm, tz);
        ts = tm2timestamp(&tm, fsec, Some(off)).ok_or_else(ts_overflow)?;
    }
    ts = ts.checked_add(iv.micros).ok_or_else(ts_overflow)?;
    ts_range_check(ts)
}

/// interval_um.
pub fn interval_neg(iv: &Interval) -> Result<Interval> {
    if iv.is_neg_inf() {
        return Ok(Interval::POS_INF);
    }
    if iv.is_pos_inf() {
        return Ok(Interval::NEG_INF);
    }
    let r = Interval::new(
        iv.months.checked_neg().ok_or_else(interval_overflow)?,
        iv.days.checked_neg().ok_or_else(interval_overflow)?,
        iv.micros.checked_neg().ok_or_else(interval_overflow)?,
    );
    if r.is_infinite() {
        return Err(interval_overflow());
    }
    Ok(r)
}

/// interval_pl.
pub fn interval_add(a: &Interval, b: &Interval) -> Result<Interval> {
    if a.is_neg_inf() {
        if b.is_pos_inf() {
            return Err(interval_overflow());
        }
        return Ok(Interval::NEG_INF);
    }
    if a.is_pos_inf() {
        if b.is_neg_inf() {
            return Err(interval_overflow());
        }
        return Ok(Interval::POS_INF);
    }
    if b.is_infinite() {
        return Ok(*b);
    }
    let r = Interval::new(
        a.months.checked_add(b.months).ok_or_else(interval_overflow)?,
        a.days.checked_add(b.days).ok_or_else(interval_overflow)?,
        a.micros.checked_add(b.micros).ok_or_else(interval_overflow)?,
    );
    if r.is_infinite() {
        return Err(interval_overflow());
    }
    Ok(r)
}

/// interval_mi.
pub fn interval_sub(a: &Interval, b: &Interval) -> Result<Interval> {
    if a.is_neg_inf() {
        if b.is_neg_inf() {
            return Err(interval_overflow());
        }
        return Ok(Interval::NEG_INF);
    }
    if a.is_pos_inf() {
        if b.is_pos_inf() {
            return Err(interval_overflow());
        }
        return Ok(Interval::POS_INF);
    }
    if b.is_neg_inf() {
        return Ok(Interval::POS_INF);
    }
    if b.is_pos_inf() {
        return Ok(Interval::NEG_INF);
    }
    let r = Interval::new(
        a.months.checked_sub(b.months).ok_or_else(interval_overflow)?,
        a.days.checked_sub(b.days).ok_or_else(interval_overflow)?,
        a.micros.checked_sub(b.micros).ok_or_else(interval_overflow)?,
    );
    if r.is_infinite() {
        return Err(interval_overflow());
    }
    Ok(r)
}

fn fits_i32(v: f64) -> bool {
    !v.is_nan() && v >= i32::MIN as f64 && v < -(i32::MIN as f64)
}

fn fits_i64(v: f64) -> bool {
    !v.is_nan() && v >= i64::MIN as f64 && v < -(i64::MIN as f64)
}

fn tsround(v: f64) -> f64 {
    (v * 1_000_000.0).round_ties_even() / 1_000_000.0
}

/// interval_sign.
pub fn interval_sign(iv: &Interval) -> i32 {
    match iv.cmp_key().cmp(&0) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// Shared tail of interval_mul / interval_div: `mf`, `df`, `tf` are the scaled fields.
fn interval_scale(span: &Interval, mf: f64, df: f64, tf: f64) -> Result<Interval> {
    if !fits_i32(mf) {
        return Err(interval_overflow());
    }
    let month = mf as i32;
    if !fits_i32(df) {
        return Err(interval_overflow());
    }
    let mut day = df as i32;
    let month_remainder_days = tsround((mf - month as f64) * 30.0);
    let mut sec_remainder =
        tsround((df - day as f64 + month_remainder_days - (month_remainder_days as i32) as f64) * 86400.0);
    if sec_remainder.abs() >= 86400.0 {
        day = day
            .checked_add((sec_remainder / 86400.0) as i32)
            .ok_or_else(interval_overflow)?;
        sec_remainder -= ((sec_remainder / 86400.0) as i32) as f64 * 86400.0;
    }
    day = day
        .checked_add(month_remainder_days as i32)
        .ok_or_else(interval_overflow)?;
    let rd = (tf + sec_remainder * USECS_PER_SEC as f64).round_ties_even();
    if !fits_i64(rd) {
        return Err(interval_overflow());
    }
    let r = Interval::new(month, day, rd as i64);
    let _ = span;
    if r.is_infinite() {
        return Err(interval_overflow());
    }
    Ok(r)
}

/// interval_mul.
pub fn interval_mul(span: &Interval, factor: f64) -> Result<Interval> {
    if factor.is_nan() {
        return Err(interval_overflow());
    }
    if span.is_infinite() {
        if factor == 0.0 {
            return Err(interval_overflow());
        }
        return if factor < 0.0 {
            interval_neg(span)
        } else {
            Ok(*span)
        };
    }
    if factor.is_infinite() {
        let isign = interval_sign(span);
        if isign == 0 {
            return Err(interval_overflow());
        }
        return Ok(if factor * (isign as f64) < 0.0 {
            Interval::NEG_INF
        } else {
            Interval::POS_INF
        });
    }
    interval_scale(
        span,
        span.months as f64 * factor,
        span.days as f64 * factor,
        span.micros as f64 * factor,
    )
}

/// interval_div.
pub fn interval_div(span: &Interval, factor: f64) -> Result<Interval> {
    if factor == 0.0 {
        bail!(DIVISION_BY_ZERO, "division by zero");
    }
    if factor.is_nan() {
        return Err(interval_overflow());
    }
    if span.is_infinite() {
        if factor.is_infinite() {
            return Err(interval_overflow());
        }
        return if factor < 0.0 {
            interval_neg(span)
        } else {
            Ok(*span)
        };
    }
    interval_scale(
        span,
        span.months as f64 / factor,
        span.days as f64 / factor,
        span.micros as f64 / factor,
    )
}

/// timestamp_mi.
pub fn ts_diff(a: i64, b: i64) -> Result<Interval> {
    if !ts_is_finite(a) || !ts_is_finite(b) {
        if a == TS_NEG_INF {
            if b == TS_NEG_INF {
                return Err(interval_overflow());
            }
            return Ok(Interval::NEG_INF);
        }
        if a == TS_POS_INF {
            if b == TS_POS_INF {
                return Err(interval_overflow());
            }
            return Ok(Interval::POS_INF);
        }
        return Ok(if b == TS_NEG_INF {
            Interval::POS_INF
        } else {
            Interval::NEG_INF
        });
    }
    let d = a.checked_sub(b).ok_or_else(interval_overflow)?;
    justify_hours(Interval::new(0, 0, d))
}

/// interval_justify_hours.
pub fn justify_hours(iv: Interval) -> Result<Interval> {
    if iv.is_infinite() {
        return Ok(iv);
    }
    let mut time = iv.micros;
    let wholeday = time / USECS_PER_DAY;
    if wholeday != 0 {
        time -= wholeday * USECS_PER_DAY;
    }
    let mut day = i32::try_from(iv.days as i64 + wholeday).map_err(|_| interval_overflow())?;
    if day > 0 && time < 0 {
        time += USECS_PER_DAY;
        day -= 1;
    } else if day < 0 && time > 0 {
        time -= USECS_PER_DAY;
        day += 1;
    }
    Ok(Interval::new(iv.months, day, time))
}

/// interval_justify_days.
pub fn justify_days(iv: Interval) -> Result<Interval> {
    if iv.is_infinite() {
        return Ok(iv);
    }
    let wholemonth = iv.days / 30;
    let mut day = iv.days - wholemonth * 30;
    let mut month = iv.months.checked_add(wholemonth).ok_or_else(interval_overflow)?;
    if month > 0 && day < 0 {
        day += 30;
        month -= 1;
    } else if month < 0 && day > 0 {
        day -= 30;
        month += 1;
    }
    Ok(Interval::new(month, day, iv.micros))
}

/// interval_justify_interval.
pub fn justify_interval(iv: Interval) -> Result<Interval> {
    if iv.is_infinite() {
        return Ok(iv);
    }
    let (mut month, mut day, mut time) = (iv.months, iv.days, iv.micros);
    if (day > 0 && time > 0) || (day < 0 && time < 0) {
        let wholemonth = day / 30;
        day -= wholemonth * 30;
        month = month.checked_add(wholemonth).ok_or_else(interval_overflow)?;
    }
    let wholeday = time / USECS_PER_DAY;
    if wholeday != 0 {
        time -= wholeday * USECS_PER_DAY;
    }
    day = (day as i64 + wholeday) as i32;
    let wholemonth = day / 30;
    day -= wholemonth * 30;
    month = month.checked_add(wholemonth).ok_or_else(interval_overflow)?;
    if month > 0 && (day < 0 || (day == 0 && time < 0)) {
        day += 30;
        month -= 1;
    } else if month < 0 && (day > 0 || (day == 0 && time > 0)) {
        day -= 30;
        month += 1;
    }
    if day > 0 && time < 0 {
        time += USECS_PER_DAY;
        day -= 1;
    } else if day < 0 && time > 0 {
        time -= USECS_PER_DAY;
        day += 1;
    }
    Ok(Interval::new(month, day, time))
}

/// timestamp_age / timestamptz_age (with `tz`, both are first rotated to local time there).
pub fn age_tz(dt1: i64, dt2: i64, tz: Option<&TimeZone>) -> Result<Interval> {
    if dt1 == TS_NEG_INF {
        if dt2 == TS_NEG_INF {
            return Err(interval_overflow());
        }
        return Ok(Interval::NEG_INF);
    }
    if dt1 == TS_POS_INF {
        if dt2 == TS_POS_INF {
            return Err(interval_overflow());
        }
        return Ok(Interval::POS_INF);
    }
    if dt2 == TS_NEG_INF {
        return Ok(Interval::POS_INF);
    }
    if dt2 == TS_POS_INF {
        return Ok(Interval::NEG_INF);
    }
    let (Some((tm1, fsec1)), Some((tm2, fsec2))) = (timestamp2tm(dt1, tz), timestamp2tm(dt2, tz))
    else {
        return Err(ts_overflow());
    };
    let mut tm = Itm {
        usec: fsec1 - fsec2,
        sec: tm1.sec - tm2.sec,
        min: tm1.min - tm2.min,
        hour: (tm1.hour - tm2.hour) as i64,
        mday: tm1.mday - tm2.mday,
        mon: tm1.mon - tm2.mon,
        year: tm1.year - tm2.year,
    };
    let flip = |tm: &mut Itm| {
        tm.usec = -tm.usec;
        tm.sec = -tm.sec;
        tm.min = -tm.min;
        tm.hour = -tm.hour;
        tm.mday = -tm.mday;
        tm.mon = -tm.mon;
        tm.year = -tm.year;
    };
    if dt1 < dt2 {
        flip(&mut tm);
    }
    while tm.usec < 0 {
        tm.usec += USECS_PER_SEC as i32;
        tm.sec -= 1;
    }
    while tm.sec < 0 {
        tm.sec += 60;
        tm.min -= 1;
    }
    while tm.min < 0 {
        tm.min += 60;
        tm.hour -= 1;
    }
    while tm.hour < 0 {
        tm.hour += 24;
        tm.mday -= 1;
    }
    while tm.mday < 0 {
        if dt1 < dt2 {
            tm.mday += DAY_TAB[isleap(tm1.year) as usize][(tm1.mon - 1) as usize];
        } else {
            tm.mday += DAY_TAB[isleap(tm2.year) as usize][(tm2.mon - 1) as usize];
        }
        tm.mon -= 1;
    }
    while tm.mon < 0 {
        tm.mon += 12;
        tm.year -= 1;
    }
    if dt1 < dt2 {
        flip(&mut tm);
    }
    itm2interval(&tm).ok_or_else(interval_overflow)
}

/// `age(a, b)` for timestamps without time zone.
pub fn age(a: i64, b: i64) -> Result<Interval> {
    age_tz(a, b, None)
}

pub fn epoch_to_ts(secs: f64) -> Result<i64> {
    if secs.is_nan() {
        bail!(DATETIME_FIELD_OVERFLOW, "timestamp cannot be NaN");
    }
    if secs.is_infinite() {
        return Ok(if secs > 0.0 { TS_POS_INF } else { TS_NEG_INF });
    }
    let us = ((secs - UNIX_EPOCH_OFFSET_SECS as f64) * USECS_PER_SEC as f64).round_ties_even();
    if us.abs() >= 9.2e18 {
        bail!(
            DATETIME_FIELD_OVERFLOW,
            "timestamp out of range: \"{secs}\""
        );
    }
    ts_range_check(us as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tz() -> TimeZone {
        TimeZone::default()
    }

    fn clock() -> Clock {
        Clock { now: 0 }
    }

    #[test]
    fn dates() {
        let d = parse_date("2024-02-29", &tz(), clock(), DateOrder::Mdy).unwrap();
        assert_eq!(format_date(d), "2024-02-29");
        assert_eq!(
            format_date(parse_date("01/15/2024", &tz(), clock(), DateOrder::Mdy).unwrap()),
            "2024-01-15"
        );
        assert_eq!(
            format_date(parse_date("Jan 15, 2024", &tz(), clock(), DateOrder::Mdy).unwrap()),
            "2024-01-15"
        );
        assert_eq!(
            format_date(parse_date("15-Jan-2024", &tz(), clock(), DateOrder::Mdy).unwrap()),
            "2024-01-15"
        );
        assert_eq!(
            format_date(parse_date("20240115", &tz(), clock(), DateOrder::Mdy).unwrap()),
            "2024-01-15"
        );
        assert_eq!(
            format_date(parse_date("0044-03-15 BC", &tz(), clock(), DateOrder::Mdy).unwrap()),
            "0044-03-15 BC"
        );
        assert_eq!(
            format_date(parse_date("epoch", &tz(), clock(), DateOrder::Mdy).unwrap()),
            "1970-01-01"
        );
        assert!(parse_date("2023-02-29", &tz(), clock(), DateOrder::Mdy).is_err());
        assert!(parse_date("garbage", &tz(), clock(), DateOrder::Mdy).is_err());
    }

    #[test]
    fn timestamps() {
        let t =
            parse_timestamp("2024-01-15 10:30:00.123456", &tz(), clock(), DateOrder::Mdy).unwrap();
        assert_eq!(format_timestamp(t), "2024-01-15 10:30:00.123456");
        let t = parse_timestamp("2024-01-15T10:30:00.5", &tz(), clock(), DateOrder::Mdy).unwrap();
        assert_eq!(format_timestamp(t), "2024-01-15 10:30:00.5");
        let t =
            parse_timestamptz("2024-01-15 10:30:00+05:30", &tz(), clock(), DateOrder::Mdy).unwrap();
        assert_eq!(format_timestamptz(t, &tz()), "2024-01-15 05:00:00+00");
        let ny = TimeZone::parse("America/New_York").unwrap();
        assert_eq!(format_timestamptz(t, &ny), "2024-01-15 00:00:00-05");
        let t = parse_timestamptz(
            "2024-07-01 12:00 America/New_York",
            &tz(),
            clock(),
            DateOrder::Mdy,
        )
        .unwrap();
        assert_eq!(format_timestamptz(t, &tz()), "2024-07-01 16:00:00+00");
        let t = parse_timestamptz("2024-01-15T10:30:00Z", &tz(), clock(), DateOrder::Mdy).unwrap();
        assert_eq!(format_timestamptz(t, &tz()), "2024-01-15 10:30:00+00");
        let t = parse_timestamp("2024-01-15 10:30 PM", &tz(), clock(), DateOrder::Mdy).unwrap();
        assert_eq!(format_timestamp(t), "2024-01-15 22:30:00");
    }

    #[test]
    fn intervals() {
        let f = |s: &str| format_interval(&parse_interval(s).unwrap());
        assert_eq!(
            f("1 year 2 months 3 days 04:05:06"),
            "1 year 2 mons 3 days 04:05:06"
        );
        assert_eq!(f("1 day"), "1 day");
        assert_eq!(f("-1 day 2 hours"), "-1 days +02:00:00");
        assert_eq!(f("1.5 days"), "1 day 12:00:00");
        assert_eq!(f("1.5 months"), "1 mon 15 days");
        assert_eq!(f("@ 1 day ago"), "-1 days");
        assert_eq!(f("10"), "00:00:10");
        assert_eq!(f("90 minutes"), "01:30:00");
        assert_eq!(f("P1Y2M3DT4H5M6S"), "1 year 2 mons 3 days 04:05:06");
        assert_eq!(f("1-2"), "1 year 2 mons");
        assert_eq!(f("0"), "00:00:00");
        assert_eq!(f("1 day -00:00:01"), "1 day -00:00:01");
        assert_eq!(f("2 weeks"), "14 days");
        assert_eq!(f("100 ms"), "00:00:00.1");
    }

    #[test]
    fn arithmetic() {
        let t = parse_timestamp("2024-01-31", &tz(), clock(), DateOrder::Mdy).unwrap();
        let r = ts_add_interval(t, &parse_interval("1 month").unwrap()).unwrap();
        assert_eq!(format_timestamp(r), "2024-02-29 00:00:00");
        let a = parse_timestamp("2024-03-15", &tz(), clock(), DateOrder::Mdy).unwrap();
        let b = parse_timestamp("2023-01-20 12:00", &tz(), clock(), DateOrder::Mdy).unwrap();
        assert_eq!(
            format_interval(&age(a, b).unwrap()),
            "1 year 1 mon 25 days 12:00:00"
        );
        assert_eq!(
            format_interval(&ts_diff(a, b).unwrap()),
            "419 days 12:00:00"
        );
    }
}
