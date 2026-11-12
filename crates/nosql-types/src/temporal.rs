//! `timestamp`, `date` and `time`: calendar arithmetic, parsing and formatting.
//!
//! - `timestamp` is signed milliseconds since the Unix epoch.
//! - `date` is an unsigned 32-bit day number with the epoch at 2^31.
//! - `time` is nanoseconds since midnight, `0 ..= 86_399_999_999_999`.
//!
//! Cassandra's parsing follows `TimestampSerializer`, `SimpleDateSerializer` and `TimeSerializer`
//! (`org.apache.cassandra.serializers`, Apache-2.0): a fixed list of `java.time` patterns resolved
//! in SMART mode (day-of-month clamped to the month's end, `24:00` is the next midnight), the JVM
//! zone (UTC in the image) when none is given, and zone ids / names via the tz database.
//!
//! ScyllaDB's rules were taken from the image: years 1970..=9999 with a four-digit year,
//! one or two digit fields, at most three fraction digits, overflowing time fields carried into
//! the next unit, zones `Z`, `UTC`, `GMT` and numeric offsets glued to the time. Dates are parsed
//! leniently (`2026-13-01` is `2027-01-01`).
//!
//! Both print timestamps as `yyyy-MM-ddTHH:mm:ss.SSSZ` (`CAST`) or with a space (JSON); Cassandra
//! prints the year of era with a `+` beyond four digits, ScyllaDB the proleptic year unsigned.

use crate::{Dialect, MarshalError};

pub const MILLIS_PER_DAY: i64 = 86_400_000;
pub const NANOS_PER_DAY: i64 = 86_400_000_000_000;
/// The day number of 1970-01-01 in the `date` encoding.
pub const DATE_EPOCH: i64 = 1 << 31;

/// Days since 1970-01-01 of a proleptic Gregorian date (any `i64`-sized year).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The proleptic Gregorian date of a day number.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub fn month_length(y: i64, m: u32) -> u32 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Wall-clock milliseconds since the epoch.
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Converts a timestamp to a `date` day number. Cassandra truncates toward zero
/// (`Duration.toDays`: -1 ms is 1970-01-01); ScyllaDB floors (-1 ms is 1969-12-31).
pub fn millis_to_date(ms: i64, dialect: Dialect) -> u32 {
    let days = match dialect {
        Dialect::Cassandra => ms / MILLIS_PER_DAY,
        Dialect::Scylla => ms.div_euclid(MILLIS_PER_DAY),
    };
    (days + DATE_EPOCH) as u32
}

/// Midnight UTC of a `date`, in milliseconds.
pub fn date_to_millis(date: u32) -> i64 {
    (date as i64 - DATE_EPOCH) * MILLIS_PER_DAY
}

/// `currentDate()`.
pub fn current_date() -> u32 {
    millis_to_date(now_millis(), Dialect::Scylla)
}

/// `currentTime()`: nanoseconds since midnight UTC.
pub fn current_time() -> i64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_nanos() % NANOS_PER_DAY as u128) as i64
}

fn push_year(out: &mut String, y: i64, plus_beyond_four: bool) {
    if y < 0 {
        out.push('-');
    } else if plus_beyond_four && y > 9999 {
        out.push('+');
    }
    let a = y.unsigned_abs();
    if a < 1000 {
        out.push_str(&format!("{a:04}"));
    } else {
        out.push_str(&a.to_string());
    }
}

fn push2(out: &mut String, v: i64) {
    out.push((b'0' + (v / 10) as u8) as char);
    out.push((b'0' + (v % 10) as u8) as char);
}

/// A timestamp as `CAST(ts AS text)` (`sep = 'T'`) or `toJson` (`sep = ' '`) prints it:
/// `2026-10-20T12:00:00.123Z`.
pub fn format_timestamp(ms: i64, sep: char, dialect: Dialect) -> String {
    let days = ms.div_euclid(MILLIS_PER_DAY);
    let rem = ms.rem_euclid(MILLIS_PER_DAY);
    let (y, m, d) = civil_from_days(days);
    let mut out = String::with_capacity(32);
    match dialect {
        // `yyyy` is the year of era: 1 BC prints as 0001.
        Dialect::Cassandra => push_year(&mut out, if y <= 0 { 1 - y } else { y }, true),
        Dialect::Scylla => push_year(&mut out, y, false),
    }
    out.push('-');
    push2(&mut out, m as i64);
    out.push('-');
    push2(&mut out, d as i64);
    out.push(sep);
    push2(&mut out, rem / 3_600_000);
    out.push(':');
    push2(&mut out, rem / 60_000 % 60);
    out.push(':');
    push2(&mut out, rem / 1000 % 60);
    out.push('.');
    out.push_str(&format!("{:03}", rem % 1000));
    out.push('Z');
    out
}

/// A `date` as `yyyy-MM-dd` (ISO; Cassandra adds `+` beyond four year digits).
pub fn format_date(date: u32, dialect: Dialect) -> String {
    let (y, m, d) = civil_from_days(date as i64 - DATE_EPOCH);
    let mut out = String::with_capacity(16);
    push_year(&mut out, y, dialect == Dialect::Cassandra);
    out.push('-');
    push2(&mut out, m as i64);
    out.push('-');
    push2(&mut out, d as i64);
    out
}

/// A `time` as `HH:mm:ss.nnnnnnnnn` (both references). Out-of-range values (rejected by
/// validation) are wrapped into the day.
pub fn format_time(nanos: i64) -> String {
    let n = nanos.rem_euclid(NANOS_PER_DAY);
    let mut out = String::with_capacity(18);
    push2(&mut out, n / 3_600_000_000_000);
    out.push(':');
    push2(&mut out, n / 60_000_000_000 % 60);
    out.push(':');
    push2(&mut out, n / 1_000_000_000 % 60);
    out.push('.');
    out.push_str(&format!("{:09}", n % 1_000_000_000));
    out
}

/// cqlsh's timestamp form: `2026-10-20 12:00:00.123000+0000`.
pub fn format_timestamp_cqlsh(ms: i64) -> String {
    let days = ms.div_euclid(MILLIS_PER_DAY);
    let rem = ms.rem_euclid(MILLIS_PER_DAY);
    let (y, m, d) = civil_from_days(days);
    if !(1..=9999).contains(&y) {
        // Python's datetime cannot hold it; cqlsh prints the raw milliseconds.
        return ms.to_string();
    }
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:03}000+0000",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

/// cqlsh's date form; out-of-range years print as the raw day count since the epoch.
pub fn format_date_cqlsh(date: u32) -> String {
    let days = date as i64 - DATE_EPOCH;
    let (y, m, d) = civil_from_days(days);
    if !(1..=9999).contains(&y) {
        return days.to_string();
    }
    format!("{y:04}-{m:02}-{d:02}")
}

// ---------------------------------------------------------------------------------------------
// Parsing

/// A tiny cursor over ASCII text.
struct Cur<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Cur<'a> {
    fn new(s: &'a str) -> Self {
        Cur {
            s: s.as_bytes(),
            i: 0,
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.s.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    /// Between `min` and `max` digits; returns the value.
    fn digits(&mut self, min: usize, max: usize) -> Option<i64> {
        let start = self.i;
        while self.i < self.s.len() && self.i - start < max && self.s[self.i].is_ascii_digit() {
            self.i += 1;
        }
        let n = self.i - start;
        if n < min {
            self.i = start;
            return None;
        }
        std::str::from_utf8(&self.s[start..self.i])
            .ok()?
            .parse()
            .ok()
    }

    fn rest(&self) -> &'a [u8] {
        &self.s[self.i..]
    }

    fn done(&self) -> bool {
        self.i == self.s.len()
    }
}

/// A parsed local date-time before zone resolution.
#[derive(Clone, Copy, Debug)]
struct Local {
    y: i64,
    mo: u32,
    d: u32,
    h: i64,
    mi: i64,
    s: i64,
    ms: i64,
}

/// What follows the local part.
enum Zone {
    /// The JVM default zone (UTC in the reference image).
    Default,
    Offset(i64),
    Region(chrono_tz::Tz),
}

impl Local {
    /// SMART resolution, then the instant in `zone`.
    fn resolve(mut self, zone: &Zone) -> Option<i64> {
        if !(1..=12).contains(&self.mo) || !(1..=31).contains(&self.d) {
            return None;
        }
        self.d = self.d.min(month_length(self.y, self.mo));
        if !(0..=59).contains(&self.mi) || !(0..=59).contains(&self.s) {
            return None;
        }
        let mut extra_day = 0;
        if self.h == 24 && self.mi == 0 && self.s == 0 && self.ms == 0 {
            self.h = 0;
            extra_day = 1;
        } else if !(0..=23).contains(&self.h) {
            return None;
        }
        let days = days_from_civil(self.y, self.mo, self.d).checked_add(extra_day)?;
        let local = days
            .checked_mul(MILLIS_PER_DAY)?
            .checked_add(self.h * 3_600_000 + self.mi * 60_000 + self.s * 1000 + self.ms)?;
        match zone {
            Zone::Default => Some(local),
            Zone::Offset(off) => local.checked_sub(*off),
            Zone::Region(tz) => region_offset(*tz, local).and_then(|off| local.checked_sub(off)),
        }
    }
}

/// The UTC offset (ms) of `tz` at local time `local`, resolving gaps forward and overlaps to the
/// earlier offset as `ZonedDateTime` does.
fn region_offset(tz: chrono_tz::Tz, local: i64) -> Option<i64> {
    use chrono::{Offset, TimeZone};
    let ndt = chrono::DateTime::from_timestamp_millis(local)?.naive_utc();
    match tz.offset_from_local_datetime(&ndt) {
        chrono::LocalResult::Single(o) => Some(o.fix().local_minus_utc() as i64 * 1000),
        chrono::LocalResult::Ambiguous(a, b) => {
            Some(a.fix().local_minus_utc().max(b.fix().local_minus_utc()) as i64 * 1000)
        }
        chrono::LocalResult::None => {
            // In a gap: use the offset before the transition (the instant moves forward).
            let before =
                chrono::DateTime::from_timestamp_millis(local - 3 * 3_600_000)?.naive_utc();
            match tz.offset_from_local_datetime(&before) {
                chrono::LocalResult::Single(o) => Some(o.fix().local_minus_utc() as i64 * 1000),
                _ => None,
            }
        }
    }
}

/// Zone abbreviations Java's `z` pattern resolves to a region (DST-aware, as Java does).
const ABBREVIATIONS: &[(&str, &str)] = &[
    ("PST", "America/Los_Angeles"),
    ("PDT", "America/Los_Angeles"),
    ("MST", "America/Denver"),
    ("MDT", "America/Denver"),
    ("CST", "America/Chicago"),
    ("CDT", "America/Chicago"),
    ("EST", "America/New_York"),
    ("EDT", "America/New_York"),
    ("CET", "CET"),
    ("CEST", "CET"),
    ("EET", "EET"),
    ("WET", "WET"),
    ("BST", "Europe/London"),
    ("JST", "Asia/Tokyo"),
    ("IST", "Asia/Kolkata"),
    ("HST", "Pacific/Honolulu"),
    ("AKST", "America/Anchorage"),
];

/// Java zone text (`z` / `zzzz`): ids, `UTC`/`GMT`/`UT` (optionally with an offset) and the
/// common abbreviations.
fn zone_text(t: &str) -> Option<Zone> {
    for p in ["UTC", "GMT", "UT"] {
        if let Some(rest) = t.strip_prefix(p) {
            if rest.is_empty() {
                return Some(Zone::Offset(0));
            }
            if p != "UT" || !rest.starts_with('C') {
                let mut c = Cur::new(rest);
                let off = offset_flexible(&mut c)?;
                return c.done().then_some(Zone::Offset(off));
            }
        }
    }
    if let Some((_, id)) = ABBREVIATIONS.iter().find(|(a, _)| *a == t) {
        return id.parse().ok().map(Zone::Region);
    }
    if t.contains('/') {
        return t.parse::<chrono_tz::Tz>().ok().map(Zone::Region);
    }
    None
}

/// `+H`, `+HH`, `+HH:mm`, `+HHmm` (ZoneId's prefix offsets).
fn offset_flexible(c: &mut Cur) -> Option<i64> {
    let sign = match c.peek()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    c.i += 1;
    let h = c.digits(1, 2)?;
    let m = if c.eat(b':') {
        c.digits(2, 2)?
    } else {
        c.digits(2, 2).unwrap_or(0)
    };
    (h <= 18 && m <= 59).then_some(sign * (h * 3_600_000 + m * 60_000))
}

/// Java's `X` (`+HH` or `+HHmm`, or `Z`).
fn offset_x(c: &mut Cur) -> Option<i64> {
    if c.eat(b'Z') {
        return Some(0);
    }
    let sign = match c.peek()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    c.i += 1;
    let h = c.digits(2, 2)?;
    let m = c.digits(2, 2).unwrap_or(0);
    (h <= 18 && m <= 59).then_some(sign * (h * 3_600_000 + m * 60_000))
}

/// Java's `XXX` (`+HH:mm` or `Z`).
fn offset_xxx(c: &mut Cur) -> Option<i64> {
    if c.eat(b'Z') {
        return Some(0);
    }
    let sign = match c.peek()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    c.i += 1;
    let h = c.digits(2, 2)?;
    if !c.eat(b':') {
        return None;
    }
    let m = c.digits(2, 2)?;
    (h <= 18 && m <= 59).then_some(sign * (h * 3_600_000 + m * 60_000))
}

/// The offset alternatives every Cassandra pattern ends with: ` z`, ` zzzz`, ` X`, `X`, `XXX`.
fn cassandra_zone_suffix(rest: &[u8]) -> Option<Zone> {
    let text = std::str::from_utf8(rest).ok()?;
    if let Some(t) = text.strip_prefix(' ') {
        if let Some(z) = zone_text(t) {
            return Some(z);
        }
        let mut c = Cur::new(t);
        if let Some(off) = offset_x(&mut c)
            && c.done()
        {
            return Some(Zone::Offset(off));
        }
        return None;
    }
    let mut c = Cur::new(text);
    if let Some(off) = offset_x(&mut c)
        && c.done()
    {
        return Some(Zone::Offset(off));
    }
    let mut c = Cur::new(text);
    if let Some(off) = offset_xxx(&mut c)
        && c.done()
    {
        return Some(Zone::Offset(off));
    }
    None
}

/// `y-M-d('T'| )H:m[:s][.f]` and its zone suffixes.
fn cassandra_date_time(s: &str) -> Option<i64> {
    let mut c = Cur::new(s);
    let y = c.digits(1, 10)?;
    if y < 1 || !c.eat(b'-') {
        return None;
    }
    let mo = c.digits(1, 9)? as u32;
    if !c.eat(b'-') {
        return None;
    }
    let d = c.digits(1, 9)? as u32;
    if !(c.eat(b'T') || c.eat(b' ')) {
        return None;
    }
    let h = c.digits(1, 9)?;
    if !c.eat(b':') {
        return None;
    }
    let mi = c.digits(1, 9)?;
    let sec = if c.eat(b':') { c.digits(1, 9)? } else { 0 };
    let mut ms = 0;
    if c.eat(b'.') {
        let start = c.i;
        let _ = c.digits(0, 9);
        let frac = &c.s[start..c.i];
        for (k, b) in frac.iter().take(3).enumerate() {
            ms += (b - b'0') as i64 * [100, 10, 1][k];
        }
    }
    let local = Local {
        y,
        mo,
        d,
        h,
        mi,
        s: sec,
        ms,
    };
    let zone = if c.done() {
        Zone::Default
    } else {
        cassandra_zone_suffix(c.rest())?
    };
    local.resolve(&zone)
}

/// `yyyy-MM-dd` followed by a zone suffix, or ISO_DATE (`uuuu-MM-dd[offset]`).
fn cassandra_date_only(s: &str) -> Option<i64> {
    let mut c = Cur::new(s);
    // ISO_LOCAL_DATE's year: 4 digits, or a sign and up to 10 digits ('+' required beyond 4).
    let neg = c.eat(b'-');
    let plus = !neg && c.eat(b'+');
    let start = c.i;
    let y = c.digits(4, 10)?;
    let ylen = c.i - start;
    if ylen > 4 && !(neg || plus) {
        // `yyyy` (year of era) still allows it, but only with a zone suffix (handled below).
        if neg {
            return None;
        }
    }
    if plus && ylen <= 4 {
        return None;
    }
    let y = if neg { -y } else { y };
    if !c.eat(b'-') {
        return None;
    }
    let mo = c.digits(2, 2)? as u32;
    if !c.eat(b'-') {
        return None;
    }
    let d = c.digits(2, 2)? as u32;
    let local = Local {
        y,
        mo,
        d,
        h: 0,
        mi: 0,
        s: 0,
        ms: 0,
    };
    if c.done() {
        // ISO_DATE without offset.
        if ylen > 4 && !(neg || plus) {
            return None;
        }
        return local.resolve(&Zone::Default);
    }
    // ISO_DATE's offset (`+HH:MM[:ss]` / `Z`).
    let mut z = Cur::new(std::str::from_utf8(c.rest()).ok()?);
    if !(ylen > 4 && !(neg || plus))
        && let Some(off) = offset_xxx(&mut z)
        && z.done()
    {
        return local.resolve(&Zone::Offset(off));
    }
    // `yyyy-MM-dd` + zone: year of era, no sign.
    if neg || plus || y < 1 {
        return None;
    }
    local.resolve(&cassandra_zone_suffix(c.rest())?)
}

fn scylla_ts_error(s: &str, inner: &str) -> MarshalError {
    MarshalError::scylla(format!("unable to parse date '{s}': {inner}"))
}

/// ScyllaDB's timestamp string parser.
fn scylla_timestamp(s: &str) -> Result<i64, MarshalError> {
    let generic = || {
        scylla_ts_error(
            s,
            &format!("marshaling error: Unable to parse timestamp from '{s}'"),
        )
    };
    let mut c = Cur::new(s);
    let y = c.digits(4, 4).ok_or_else(generic)?;
    if c.peek().is_some_and(|b| b.is_ascii_digit()) || !c.eat(b'-') {
        return Err(generic());
    }
    let mo = c.digits(1, 9).ok_or_else(generic)?;
    if !c.eat(b'-') {
        return Err(generic());
    }
    let d = c.digits(1, 9).ok_or_else(generic)?;
    let (mut h, mut mi, mut sec, mut ms) = (0i64, 0i64, 0i64, 0i64);
    let save = c.i;
    if c.eat(b'T') || c.eat(b' ') {
        match (c.digits(1, 9), c.eat(b':')) {
            (Some(hh), true) => {
                h = hh;
                mi = c.digits(1, 9).ok_or_else(generic)?;
                if c.eat(b':') {
                    sec = c.digits(1, 9).ok_or_else(generic)?;
                    if c.peek() == Some(b'.') && c.s.get(c.i + 1).is_some_and(u8::is_ascii_digit) {
                        c.i += 1;
                        let start = c.i;
                        while c.peek().is_some_and(|b| b.is_ascii_digit()) {
                            c.i += 1;
                        }
                        let n = c.i - start;
                        if n > 3 {
                            return Err(scylla_ts_error(
                                s,
                                &format!(
                                    "marshaling error: Milliseconds length exceeds expected ({n})"
                                ),
                            ));
                        }
                        let v: i64 = std::str::from_utf8(&c.s[start..c.i])
                            .unwrap_or("0")
                            .parse()
                            .unwrap_or(0);
                        ms = v * [100, 10, 1][n - 1];
                    }
                }
            }
            _ => c.i = save,
        }
    }
    let rest = std::str::from_utf8(c.rest()).unwrap_or("");
    let offset = if rest.is_empty() {
        0
    } else {
        let lower = rest.to_ascii_lowercase();
        // Named zones may be padded with spaces (`'2011-02-03  Z '`); numeric offsets may not.
        match lower.trim_matches(' ') {
            "z" | "utc" | "gmt" => 0,
            _ => {
                let mut z = Cur::new(&lower);
                let sign = if z.eat(b'+') {
                    1
                } else if z.eat(b'-') {
                    -1
                } else {
                    0
                };
                let parsed = (sign != 0)
                    .then(|| {
                        let hh = z.digits(2, 2)?;
                        let mm = if z.eat(b':') {
                            z.digits(2, 2)?
                        } else {
                            z.digits(2, 2).unwrap_or(0)
                        };
                        z.done().then_some(sign * (hh * 3_600_000 + mm * 60_000))
                    })
                    .flatten();
                parsed.ok_or_else(|| {
                    scylla_ts_error(
                        s,
                        &format!("marshaling error: Unable to parse timezone '{lower}'"),
                    )
                })?
            }
        }
    };
    if !(1400..=9999).contains(&y) {
        return Err(scylla_ts_error(
            s,
            "boost::wrapexcept<boost::gregorian::bad_year> (Year is out of valid range: 1400..9999)",
        ));
    }
    if !(1..=12).contains(&mo) {
        return Err(scylla_ts_error(
            s,
            "boost::wrapexcept<boost::gregorian::bad_month> (Month number is out of range 1..12)",
        ));
    }
    if !(1..=31).contains(&d) {
        return Err(scylla_ts_error(
            s,
            "boost::wrapexcept<boost::gregorian::bad_day_of_month> (Day of month value is out of range 1..31)",
        ));
    }
    if d as u32 > month_length(y, mo as u32) {
        return Err(scylla_ts_error(
            s,
            "boost::wrapexcept<boost::gregorian::bad_day_of_month> (Day of month is not valid for year)",
        ));
    }
    // Only a local date-time (no zone) goes through the pre-1970-refusing conversion.
    if y < 1970 && rest.is_empty() {
        return Err(scylla_ts_error(
            s,
            "boost::wrapexcept<std::out_of_range> (Cannot convert dates prior to Jan 1, 1970)",
        ));
    }
    let days = days_from_civil(y, mo as u32, d as u32);
    Ok(days * MILLIS_PER_DAY + h * 3_600_000 + mi * 60_000 + sec * 1000 + ms - offset)
}

fn is_integer_text(s: &str) -> bool {
    let d = s.strip_prefix('-').unwrap_or(s);
    !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())
}

/// Parses a timestamp string (`TimestampType.fromString`): `now`, integer milliseconds, or a
/// date-time in one of the reference's formats.
pub fn parse_timestamp(s: &str, dialect: Dialect) -> Result<i64, MarshalError> {
    if s.eq_ignore_ascii_case("now") {
        return Ok(now_millis());
    }
    if is_integer_text(s) {
        return s.parse().map_err(|_| match dialect {
            Dialect::Cassandra => {
                MarshalError::new(format!("Unable to make long (for date) from: '{s}'"))
            }
            Dialect::Scylla => MarshalError::scylla(format!("Invalid number format '{s}'")),
        });
    }
    match dialect {
        Dialect::Cassandra => cassandra_date_time(s)
            .or_else(|| cassandra_date_only(s))
            .ok_or_else(|| MarshalError::new(format!("Unable to parse a date/time from '{s}'"))),
        Dialect::Scylla => scylla_timestamp(s),
    }
}

/// Parses a `date` string (`SimpleDateType.fromString`): a raw unsigned day number or
/// `yyyy-MM-dd`. Returns the encoded day number.
pub fn parse_date(s: &str, dialect: Dialect) -> Result<u32, MarshalError> {
    match dialect {
        Dialect::Cassandra => parse_date_cassandra(s),
        Dialect::Scylla => parse_date_scylla(s),
    }
}

const MIN_DATE_DAYS: i64 = -(1 << 31);
const MAX_DATE_DAYS: i64 = (1 << 31) - 1;

fn parse_date_cassandra(s: &str) -> Result<u32, MarshalError> {
    if is_integer_text(s) {
        return match s.parse::<i64>() {
            Ok(v) if (0..=u32::MAX as i64).contains(&v) => Ok(v as u32),
            _ => Err(MarshalError::new(format!(
                "Unable to make unsigned int (for date) from: '{s}'"
            ))),
        };
    }
    let coerce = || MarshalError::new(format!("Unable to coerce '{s}' to a formatted date (long)"));
    // ISO_LOCAL_DATE, STRICT: `uuuu-MM-dd`, sign required beyond four year digits.
    let mut c = Cur::new(s);
    let neg = c.eat(b'-');
    let plus = !neg && c.eat(b'+');
    let start = c.i;
    let y = c.digits(4, 10).ok_or_else(coerce)?;
    let ylen = c.i - start;
    if (ylen > 4 && !(neg || plus)) || (plus && ylen <= 4) {
        return Err(coerce());
    }
    let y = if neg { -y } else { y };
    if !c.eat(b'-') {
        return Err(coerce());
    }
    let mo = c.digits(2, 2).ok_or_else(coerce)? as u32;
    if !c.eat(b'-') {
        return Err(coerce());
    }
    let d = c.digits(2, 2).ok_or_else(coerce)? as u32;
    if !c.done() || !(1..=12).contains(&mo) || d < 1 || d > month_length(y, mo) {
        return Err(coerce());
    }
    let days = days_from_civil(y, mo, d);
    if days < MIN_DATE_DAYS {
        return Err(MarshalError::new(format!(
            "Input date {s} is less than min supported date -5877641-06-23T00:00Z"
        )));
    }
    if days > MAX_DATE_DAYS {
        return Err(MarshalError::new(format!(
            "Input date {s} is greater than max supported date +5881580-07-11T00:00Z"
        )));
    }
    Ok((days + DATE_EPOCH) as u32)
}

fn parse_date_scylla(s: &str) -> Result<u32, MarshalError> {
    // `-?\d+-\d+-\d+`, normalised leniently (month 13 rolls into the next year).
    let mut c = Cur::new(s);
    let neg = c.eat(b'-');
    let ymd = (|| {
        let y = c.digits(1, 10)?;
        if !c.eat(b'-') {
            return None;
        }
        let m = c.digits(1, 10)?;
        if !c.eat(b'-') {
            return None;
        }
        let d = c.digits(1, 10)?;
        Some((if neg { -y } else { y }, m, d))
    })();
    if let Some((y, m, d)) = ymd {
        if !c.done() {
            return Err(MarshalError::scylla(format!(
                "Unable to coerce '{s}' to a formatted date (long)"
            )));
        }
        let months = y * 12 + (m - 1);
        let days =
            days_from_civil(months.div_euclid(12), (months.rem_euclid(12) + 1) as u32, 1) + (d - 1);
        if days > MAX_DATE_DAYS {
            return Err(MarshalError::scylla(format!(
                "Input date {s} is greater than max supported date 5881580-07-11"
            )));
        }
        if days < MIN_DATE_DAYS {
            return Err(MarshalError::scylla(format!(
                "Input date {s} is less than min supported date -5877641-06-23"
            )));
        }
        return Ok((days + DATE_EPOCH) as u32);
    }
    match s.parse::<u64>() {
        Ok(v) if v <= u32::MAX as u64 && s.bytes().all(|b| b.is_ascii_digit()) => Ok(v as u32),
        _ => Err(MarshalError::scylla(format!(
            "Unable to make unsigned int (for date) from {s}"
        ))),
    }
}

/// Parses a `time` string (`TimeType.fromString`): raw nanoseconds or `hh:mm:ss[.fffffffff]`.
pub fn parse_time(s: &str, dialect: Dialect) -> Result<i64, MarshalError> {
    // Raw nanoseconds are Cassandra-only; ScyllaDB treats them as a malformed time.
    if dialect == Dialect::Cassandra && is_integer_text(s) {
        return match s.parse::<i64>() {
            Ok(v) if (0..NANOS_PER_DAY).contains(&v) => Ok(v),
            _ => Err(MarshalError::new(format!(
                "Unable to make long (for time) from: '{s}'"
            ))),
        };
    }
    match dialect {
        Dialect::Cassandra => parse_time_strict(s.trim(), false).map_err(|_| {
            MarshalError::new(format!(
                "(TimeType) Unable to coerce '{s}' to a formatted time (long)"
            ))
        }),
        Dialect::Scylla => parse_time_strict(s.trim(), true).map_err(|e| {
            if matches!(e, TimeError::Stol) {
                // `std::stol` threw: ScyllaDB answers with a server error whose text is just this.
                return MarshalError::new("stol");
            }
            MarshalError::scylla(match e {
                TimeError::Stol => unreachable!(),
                TimeError::Format => "Timestamp format must be hh:mm:ss[.fffffffff]".to_string(),
                TimeError::Hour(v) => format!("Hour out of bounds ({v})."),
                TimeError::Minute(v) => format!("Minute out of bounds ({v})."),
                TimeError::Second(v) => format!("Second out of bounds ({v})."),
                TimeError::Nanos => format!("more than 9 nanosecond digits: {s}"),
            })
        }),
    }
}

enum TimeError {
    Format,
    /// A field is not a number (ScyllaDB's `std::stol` failure).
    Stol,
    Hour(i64),
    Minute(i64),
    Second(i64),
    Nanos,
}

/// `Integer.parseInt`: an optional sign and digits.
fn java_int(s: &str) -> Option<i64> {
    let d = s.strip_prefix(['-', '+']).unwrap_or(s);
    if d.is_empty() || d.len() > 10 || !d.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v: i64 = s.parse().ok()?;
    i32::try_from(v).ok().map(|v| v as i64)
}

/// `TimeSerializer.parseTimeStrictly` (loosely `java.sql.Timestamp`).
fn parse_time_strict(s: &str, scylla: bool) -> Result<i64, TimeError> {
    let first = s.find(':').ok_or(TimeError::Format)?;
    let second = s[first + 1..]
        .find(':')
        .map(|i| i + first + 1)
        .ok_or(TimeError::Format)?;
    if first == 0 || second >= s.len() - 1 {
        return Err(TimeError::Format);
    }
    let num = || {
        if scylla {
            TimeError::Stol
        } else {
            TimeError::Format
        }
    };
    let hour = java_int(&s[..first]).ok_or_else(num)?;
    if !(0..24).contains(&hour) {
        return Err(TimeError::Hour(hour));
    }
    let minute = java_int(&s[first + 1..second]).ok_or_else(num)?;
    if !(0..60).contains(&minute) {
        return Err(TimeError::Minute(minute));
    }
    let tail = &s[second + 1..];
    let (sec_text, nanos) = match tail.find('.') {
        Some(p) if p + 1 < tail.len() => {
            let frac = &tail[p + 1..];
            if frac.len() > 9 {
                return Err(if scylla {
                    TimeError::Nanos
                } else {
                    TimeError::Format
                });
            }
            if !frac.as_bytes()[0].is_ascii_digit() {
                return Err(num());
            }
            let padded = format!("{frac:0<9}");
            (&tail[..p], java_int(&padded).ok_or_else(num)?)
        }
        Some(_) => return Err(num()),
        None => (tail, 0),
    };
    let second_v = java_int(sec_text).ok_or_else(num)?;
    if !(0..60).contains(&second_v) {
        return Err(TimeError::Second(second_v));
    }
    Ok(hour * 3_600_000_000_000 + minute * 60_000_000_000 + second_v * 1_000_000_000 + nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: Dialect = Dialect::Cassandra;
    const S: Dialect = Dialect::Scylla;

    fn ts(s: &str, d: Dialect) -> String {
        format_timestamp(
            parse_timestamp(s, d).unwrap_or_else(|e| panic!("{s}: {e}")),
            'T',
            d,
        )
    }

    #[test]
    fn civil_round_trip() {
        for days in [
            -1_000_000_000i64,
            -719_528,
            -1,
            0,
            1,
            365,
            20_746,
            2_932_896,
            (1 << 31) - 1,
        ] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(2000, 2, 29), 11_016);
        assert_eq!(month_length(1900, 2), 28);
        assert_eq!(month_length(2000, 2), 29);
        assert_eq!(month_length(2026, 4), 30);
    }

    #[test]
    fn timestamp_formatting() {
        // Verified on both images (CAST(ts AS text) / toJson).
        assert_eq!(
            format_timestamp(1_792_497_600_123, 'T', C),
            "2026-10-20T12:00:00.123Z"
        );
        assert_eq!(
            format_timestamp(1_792_497_600_123, ' ', S),
            "2026-10-20 12:00:00.123Z"
        );
        assert_eq!(format_timestamp(-1, 'T', C), "1969-12-31T23:59:59.999Z");
        assert_eq!(
            format_timestamp(i64::MAX, 'T', C),
            "+292278994-08-17T07:12:55.807Z"
        );
        assert_eq!(
            format_timestamp(i64::MAX, 'T', S),
            "292278994-08-17T07:12:55.807Z"
        );
        assert_eq!(
            format_timestamp(i64::MIN, 'T', C),
            "+292275056-05-16T16:47:04.192Z"
        );
        assert_eq!(
            format_timestamp(i64::MIN, ' ', S),
            "-292275055-05-16 16:47:04.192Z"
        );
        assert_eq!(
            format_timestamp(-62_167_219_200_000, 'T', C),
            "0001-01-01T00:00:00.000Z"
        );
        assert_eq!(
            format_timestamp(-62_167_219_200_000, 'T', S),
            "0000-01-01T00:00:00.000Z"
        );
        assert_eq!(
            format_timestamp(-62_198_755_200_000, 'T', C),
            "0002-01-01T00:00:00.000Z"
        );
        assert_eq!(
            format_timestamp(-62_198_755_200_000, 'T', S),
            "-0001-01-01T00:00:00.000Z"
        );
        assert_eq!(
            format_timestamp(253_402_300_800_000, 'T', C),
            "+10000-01-01T00:00:00.000Z"
        );
        assert_eq!(
            format_timestamp(253_402_300_800_000, 'T', S),
            "10000-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn date_and_time_formatting() {
        assert_eq!(format_date(DATE_EPOCH as u32, C), "1970-01-01");
        assert_eq!(format_date(0, C), "-5877641-06-23");
        assert_eq!(format_date(0, S), "-5877641-06-23");
        assert_eq!(format_date(u32::MAX, C), "+5881580-07-11");
        assert_eq!(format_date(u32::MAX, S), "5881580-07-11");
        assert_eq!(format_date((1 << 31) - 1, C), "1969-12-31");
        assert_eq!(format_date((DATE_EPOCH - 719_893) as u32, C), "-0001-01-01");
        assert_eq!(format_date((DATE_EPOCH - 719_528) as u32, S), "0000-01-01");
        assert_eq!(
            format_date((DATE_EPOCH + 2_932_897) as u32, C),
            "+10000-01-01"
        );
        assert_eq!(format_time(45_296_123_456_789), "12:34:56.123456789");
        assert_eq!(format_time(0), "00:00:00.000000000");
        assert_eq!(format_time(NANOS_PER_DAY - 1), "23:59:59.999999999");
        assert_eq!(
            format_timestamp_cqlsh(1_792_497_600_123),
            "2026-10-20 12:00:00.123000+0000"
        );
        assert_eq!(format_timestamp_cqlsh(i64::MAX), "9223372036854775807");
        assert_eq!(format_date_cqlsh(u32::MAX), "2147483647");
        assert_eq!(format_date_cqlsh((DATE_EPOCH - 719_893) as u32), "-719893");
        assert_eq!(format_date_cqlsh(DATE_EPOCH as u32 + 20_746), "2026-10-20");
    }

    #[test]
    fn cassandra_timestamp_formats() {
        // Each accepted on cassandra:5.0.9, with the instant it stored.
        for (s, out) in [
            ("2026-10-20 12:00:00.123+0000", "2026-10-20T12:00:00.123Z"),
            ("2026-10-20", "2026-10-20T00:00:00.000Z"),
            ("1900-01-01 00:00:00+0000", "1900-01-01T00:00:00.000Z"),
            ("1969-12-31 23:00:00+0100", "1969-12-31T22:00:00.000Z"),
            ("2026-10-20T01:02:03.5+0130", "2026-10-19T23:32:03.500Z"),
            ("2026-10-20 01:02 UTC", "2026-10-20T01:02:00.000Z"),
            ("2026-10-20 01:02:03.123456789Z", "2026-10-20T01:02:03.123Z"),
            (
                "2026-10-20 1:2:3 America/New_York",
                "2026-10-20T05:02:03.000Z",
            ),
            ("2026-10-20 01:02:03 PST", "2026-10-20T08:02:03.000Z"),
            ("2026-10-20 01:02:03+01:00", "2026-10-20T00:02:03.000Z"),
            ("2026-10-20 01:02:03 +01", "2026-10-20T00:02:03.000Z"),
            ("99999-01-01 00:00:00", "+99999-01-01T00:00:00.000Z"),
            ("2026-02-30 12:00", "2026-02-28T12:00:00.000Z"),
            ("2026-10-20 24:00", "2026-10-21T00:00:00.000Z"),
            ("2026-10-20Z", "2026-10-20T00:00:00.000Z"),
            ("2026-10-20+01:00", "2026-10-19T23:00:00.000Z"),
            ("2026-10-20 GMT+02:00", "2026-10-19T22:00:00.000Z"),
            ("2026-10-20 12:00:00.", "2026-10-20T12:00:00.000Z"),
            ("2026-10-20 UTC", "2026-10-20T00:00:00.000Z"),
            ("2026-10-20 12:00 Europe/Paris", "2026-10-20T10:00:00.000Z"),
            ("-5", "1969-12-31T23:59:59.995Z"),
            ("2026-10-20 12:00:00.123+00", "2026-10-20T12:00:00.123Z"),
            ("1969-12-31 23:00:00", "1969-12-31T23:00:00.000Z"),
            ("2026-010-20 12:00", "2026-10-20T12:00:00.000Z"),
            ("2026-10-20 12:00:00 Z", "2026-10-20T12:00:00.000Z"),
            ("0000-01-01", "0001-01-01T00:00:00.000Z"),
            ("-1", "1969-12-31T23:59:59.999Z"),
            ("1000", "1970-01-01T00:00:01.000Z"),
        ] {
            assert_eq!(ts(s, C), out, "{s}");
        }
        for bad in [
            "2026-10-20 25:00",
            "garbage",
            "-0001-01-01 00:00:00+0000",
            "0000-01-01 00:00:00+0000",
            "2026-13-01 00:00",
            "2026-10-20 12:60",
            "2026-10-20 24:00:01",
            "2026-10-20 12:00:00.1234567890",
            "2026-10-20 12:00 Mars/Olympus",
            "2026-10-20 12:00 +1",
            "",
        ] {
            let e = parse_timestamp(bad, C).unwrap_err();
            assert_eq!(
                e.message,
                format!("Unable to parse a date/time from '{bad}'")
            );
        }
        assert_eq!(
            parse_timestamp("99999999999999999999", C)
                .unwrap_err()
                .message,
            "Unable to make long (for date) from: '99999999999999999999'"
        );
        let now = now_millis();
        assert!((parse_timestamp("NOW", C).unwrap() - now).abs() < 10_000);
    }

    #[test]
    fn scylla_timestamp_formats() {
        for (s, out) in [
            ("2026-10-20 12:00:00.123+0000", "2026-10-20T12:00:00.123Z"),
            ("2026-10-20", "2026-10-20T00:00:00.000Z"),
            ("1900-01-01 00:00:00+0000", "1900-01-01T00:00:00.000Z"),
            ("1969-12-31 23:00:00+0100", "1969-12-31T22:00:00.000Z"),
            ("2026-10-20T01:02:03.5+0130", "2026-10-19T23:32:03.500Z"),
            ("2026-10-20 01:02 UTC", "2026-10-20T01:02:00.000Z"),
            ("2026-10-20 25:00", "2026-10-21T01:00:00.000Z"),
            ("2026-10-20 12:00:61", "2026-10-20T12:01:01.000Z"),
            ("2026-10-20 24:00:00", "2026-10-21T00:00:00.000Z"),
            ("2026-10-20T12:00:00Z", "2026-10-20T12:00:00.000Z"),
            ("2026-10-20 12:00:00-0700", "2026-10-20T19:00:00.000Z"),
            ("2026-10-20 12:00:00 GMT", "2026-10-20T12:00:00.000Z"),
            ("2026-10-20 12:00:00 Z", "2026-10-20T12:00:00.000Z"),
            ("2026-10-20 12:00:00.123+00", "2026-10-20T12:00:00.123Z"),
            ("2026-10-20 12:00:00.1", "2026-10-20T12:00:00.100Z"),
            ("2026-1-2 3:4:5", "2026-01-02T03:04:05.000Z"),
            ("2026-10-20 01:02:03+01:00", "2026-10-20T00:02:03.000Z"),
            ("1970-01-01 00:00:00", "1970-01-01T00:00:00.000Z"),
            ("2026-10-20 24:00", "2026-10-21T00:00:00.000Z"),
            ("2026-10-20Z", "2026-10-20T00:00:00.000Z"),
            ("2026-10-20+01:00", "2026-10-19T23:00:00.000Z"),
            ("2026-10-20 12:60", "2026-10-20T13:00:00.000Z"),
            ("2026-10-20 24:00:01", "2026-10-21T00:00:01.000Z"),
            ("2026-10-20 UTC", "2026-10-20T00:00:00.000Z"),
            ("2011-02-03  Z ", "2011-02-03T00:00:00.000Z"),
            ("2011-02-03 04:05  gmt ", "2011-02-03T04:05:00.000Z"),
            ("-5", "1969-12-31T23:59:59.995Z"),
            ("2026-010-20 12:00", "2026-10-20T12:00:00.000Z"),
        ] {
            assert_eq!(ts(s, S), out, "{s}");
        }
        for (bad, inner) in [
            (
                "garbage",
                "marshaling error: Unable to parse timestamp from 'garbage'",
            ),
            (
                "99999-01-01 00:00:00",
                "marshaling error: Unable to parse timestamp from '99999-01-01 00:00:00'",
            ),
            (
                "-0001-01-01 00:00:00+0000",
                "marshaling error: Unable to parse timestamp from '-0001-01-01 00:00:00+0000'",
            ),
            (
                "2026-10-20 01:02:03.123456789Z",
                "marshaling error: Milliseconds length exceeds expected (9)",
            ),
            (
                "2026-10-20 12:00:00.123456+0000",
                "marshaling error: Milliseconds length exceeds expected (6)",
            ),
            (
                "2026-10-20 1:2:3 America/New_York",
                "marshaling error: Unable to parse timezone ' america/new_york'",
            ),
            (
                "2026-10-20 01:02:03 PST",
                "marshaling error: Unable to parse timezone ' pst'",
            ),
            (
                "2026-10-20 01:02:03 +01",
                "marshaling error: Unable to parse timezone ' +01'",
            ),
            (
                "2026-10-20 12:00:00 -07:00",
                "marshaling error: Unable to parse timezone ' -07:00'",
            ),
            (
                "2026-10-20 12",
                "marshaling error: Unable to parse timezone ' 12'",
            ),
            (
                "2026-10-20 12:00:00+1:00",
                "marshaling error: Unable to parse timezone '+1:00'",
            ),
            (
                "2026-02-30 12:00",
                "boost::wrapexcept<boost::gregorian::bad_day_of_month> (Day of month is not valid for year)",
            ),
            (
                "0000-01-01 00:00:00+0000",
                "boost::wrapexcept<boost::gregorian::bad_year> (Year is out of valid range: 1400..9999)",
            ),
            (
                "1399-12-31",
                "boost::wrapexcept<boost::gregorian::bad_year> (Year is out of valid range: 1400..9999)",
            ),
            (
                "1400-01-01",
                "boost::wrapexcept<std::out_of_range> (Cannot convert dates prior to Jan 1, 1970)",
            ),
            (
                "1969-12-31 23:00:00",
                "boost::wrapexcept<std::out_of_range> (Cannot convert dates prior to Jan 1, 1970)",
            ),
            (
                "2026-13-01 00:00",
                "boost::wrapexcept<boost::gregorian::bad_month> (Month number is out of range 1..12)",
            ),
            (
                "0000-01-01",
                "boost::wrapexcept<boost::gregorian::bad_year> (Year is out of valid range: 1400..9999)",
            ),
            (
                "2026-10-20 GMT+02:00",
                "marshaling error: Unable to parse timezone ' gmt+02:00'",
            ),
            (
                "2026-10-20 12:00 +1",
                "marshaling error: Unable to parse timezone ' +1'",
            ),
            (
                "2026-10-20 12:00:00.",
                "marshaling error: Unable to parse timezone '.'",
            ),
            (
                "2026-10-20 12:00 Europe/Paris",
                "marshaling error: Unable to parse timezone ' europe/paris'",
            ),
        ] {
            let e = parse_timestamp(bad, S).unwrap_err();
            assert_eq!(
                e.message,
                format!("marshaling error: unable to parse date '{bad}': {inner}"),
                "{bad}"
            );
        }
        assert!(parse_timestamp("now", S).is_ok());
        assert_eq!(parse_timestamp("-5", S), Ok(-5));
    }

    #[test]
    fn date_parsing() {
        let date =
            |s: &str, d| format_date(parse_date(s, d).unwrap_or_else(|e| panic!("{s}: {e}")), d);
        assert_eq!(date("2026-10-20", C), "2026-10-20");
        assert_eq!(date("-5877641-06-23", C), "-5877641-06-23");
        assert_eq!(date("+5881580-07-11", C), "+5881580-07-11");
        assert_eq!(date("-0001-01-01", C), "-0001-01-01");
        assert_eq!(date("+10000-01-01", C), "+10000-01-01");
        assert_eq!(date("0000-01-01", C), "0000-01-01");
        assert_eq!(parse_date("0", C), Ok(0));
        assert_eq!(parse_date("2147483648", C), Ok(1 << 31));
        assert_eq!(parse_date("4294967295", C), Ok(u32::MAX));
        for bad in [
            "2026-13-01",
            "5881580-07-12",
            "1970-1-1",
            "2026-02-30",
            "2026-01-05 ",
            "abc",
            "+2026-01-01",
        ] {
            assert_eq!(
                parse_date(bad, C).unwrap_err().message,
                format!("Unable to coerce '{bad}' to a formatted date (long)"),
                "{bad}"
            );
        }
        assert_eq!(
            parse_date("+5881580-07-12", C).unwrap_err().message,
            "Input date +5881580-07-12 is greater than max supported date +5881580-07-11T00:00Z"
        );
        assert_eq!(
            parse_date("-5877641-06-22", C).unwrap_err().message,
            "Input date -5877641-06-22 is less than min supported date -5877641-06-23T00:00Z"
        );
        for bad in ["4294967296", "-5"] {
            assert_eq!(
                parse_date(bad, C).unwrap_err().message,
                format!("Unable to make unsigned int (for date) from: '{bad}'")
            );
        }

        // ScyllaDB: lenient normalisation.
        assert_eq!(date("2026-13-01", S), "2027-01-01");
        assert_eq!(date("2026-02-30", S), "2026-03-02");
        assert_eq!(date("2026-1-5", S), "2026-01-05");
        assert_eq!(date("-2026-01-05", S), "-2026-01-05");
        assert_eq!(date("1970-1-1", S), "1970-01-01");
        assert_eq!(date("20260105", S), "-5822171-10-04");
        assert_eq!(date("5881580-07-11", S), "5881580-07-11");
        assert_eq!(
            parse_date("5881580-07-12", S).unwrap_err().message,
            "marshaling error: Input date 5881580-07-12 is greater than max supported date 5881580-07-11"
        );
        assert_eq!(
            parse_date("-5877641-06-22", S).unwrap_err().message,
            "marshaling error: Input date -5877641-06-22 is less than min supported date -5877641-06-23"
        );
        assert_eq!(
            parse_date("2026-01-05 ", S).unwrap_err().message,
            "marshaling error: Unable to coerce '2026-01-05 ' to a formatted date (long)"
        );
        assert_eq!(date("2026-02-30", S), "2026-03-02");
        for bad in [
            "4294967296",
            "-5",
            "+10000-01-01",
            "+5881580-07-12",
            "+2026-01-01",
        ] {
            assert_eq!(
                parse_date(bad, S).unwrap_err().message,
                format!("marshaling error: Unable to make unsigned int (for date) from {bad}")
            );
        }
    }

    #[test]
    fn time_parsing() {
        for d in [C, S] {
            assert_eq!(parse_time("12:34:56.123456789", d), Ok(45_296_123_456_789));
            assert_eq!(parse_time("1:2:3.1", d), Ok(3_723_100_000_000));
            assert_eq!(parse_time(" 01:02:03 ", d), Ok(3_723_000_000_000));
            assert_eq!(parse_time("1:2:3", d), Ok(3_723_000_000_000));
        }
        assert_eq!(parse_time("5", C), Ok(5));
        assert_eq!(parse_time("+1:+2:3", C), Ok(3_723_000_000_000));
        assert_eq!(parse_time("+1:+2:3", S), Ok(3_723_000_000_000));
        for bad in ["01:02:03.x", "a:b:c", "01:02:03."] {
            assert_eq!(parse_time(bad, S).unwrap_err().message, "stol", "{bad}");
        }
        for bad in [
            "25:00:00",
            "12:00",
            "24:00:00",
            "23:59:60",
            "23:60:00",
            "-01:02:03",
            "01:02:03.",
            "a:b:c",
            "01:02:03.x",
            "23:59:59.1234567890",
        ] {
            assert_eq!(
                parse_time(bad, C).unwrap_err().message,
                format!("(TimeType) Unable to coerce '{bad}' to a formatted time (long)"),
                "{bad}"
            );
        }
        assert_eq!(parse_time("86399999999999", C), Ok(86_399_999_999_999));
        assert_eq!(
            parse_time("86400000000000", C).unwrap_err().message,
            "Unable to make long (for time) from: '86400000000000'"
        );
        for (bad, msg) in [
            ("25:00:00", "Hour out of bounds (25)."),
            ("24:00:00", "Hour out of bounds (24)."),
            ("-01:02:03", "Hour out of bounds (-1)."),
            ("23:60:00", "Minute out of bounds (60)."),
            ("23:59:60", "Second out of bounds (60)."),
            ("12:00", "Timestamp format must be hh:mm:ss[.fffffffff]"),
            ("5", "Timestamp format must be hh:mm:ss[.fffffffff]"),
            (
                "23:59:59.1234567890",
                "more than 9 nanosecond digits: 23:59:59.1234567890",
            ),
        ] {
            assert_eq!(
                parse_time(bad, S).unwrap_err().message,
                format!("marshaling error: {msg}"),
                "{bad}"
            );
        }
    }

    #[test]
    fn date_conversions() {
        assert_eq!(millis_to_date(-1, C), DATE_EPOCH as u32);
        assert_eq!(millis_to_date(-1, S), DATE_EPOCH as u32 - 1);
        assert_eq!(millis_to_date(-86_400_000, C), DATE_EPOCH as u32 - 1);
        assert_eq!(
            millis_to_date(1_436_832_018_805, S),
            DATE_EPOCH as u32 + 16_630
        );
        assert_eq!(date_to_millis(DATE_EPOCH as u32 - 1), -86_400_000);
        let today = current_date();
        assert_eq!(today, millis_to_date(now_millis(), S));
        assert!((0..NANOS_PER_DAY).contains(&current_time()));
    }
}
