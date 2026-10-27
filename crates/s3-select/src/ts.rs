//! The TIMESTAMP type: Ion text timestamps with a precision and an optional UTC offset, and the
//! date functions (DATE_ADD, DATE_DIFF, EXTRACT, TO_STRING, TO_TIMESTAMP).

use std::cmp::Ordering;
use std::fmt::Write as _;

use chrono::{Datelike, Months, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, Timelike};

use crate::error::{Result, SelectError};

/// How much of a timestamp is significant (Ion timestamps carry their precision).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Precision {
    Year,
    Month,
    Day,
    Minute,
    Second,
    Fraction,
}

/// A date part for DATE_ADD, DATE_DIFF and EXTRACT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatePart {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    TimezoneHour,
    TimezoneMinute,
}

impl DatePart {
    pub(crate) fn parse(word: &str) -> Option<DatePart> {
        Some(match word.to_ascii_lowercase().as_str() {
            "year" => DatePart::Year,
            "month" => DatePart::Month,
            "day" => DatePart::Day,
            "hour" => DatePart::Hour,
            "minute" => DatePart::Minute,
            "second" => DatePart::Second,
            "timezone_hour" => DatePart::TimezoneHour,
            "timezone_minute" => DatePart::TimezoneMinute,
            _ => return None,
        })
    }

    fn precision(self) -> Precision {
        match self {
            DatePart::Year => Precision::Year,
            DatePart::Month => Precision::Month,
            DatePart::Day => Precision::Day,
            DatePart::Hour | DatePart::Minute => Precision::Minute,
            _ => Precision::Second,
        }
    }
}

/// A timestamp: the local date and time, its UTC offset in seconds (`None` is Ion's unknown
/// offset, `-00:00`, which date-only timestamps have) and its precision.
#[derive(Clone, Copy, Debug)]
pub struct Timestamp {
    pub(crate) local: NaiveDateTime,
    pub(crate) offset: Option<i32>,
    pub(crate) precision: Precision,
    /// Digits of fractional seconds shown (with [`Precision::Fraction`]), 1..=9.
    pub(crate) frac_digits: u8,
}

impl PartialEq for Timestamp {
    fn eq(&self, other: &Self) -> bool {
        self.utc() == other.utc()
    }
}

fn err_range() -> SelectError {
    SelectError::new("IllegalSqlFunctionArgument", "The timestamp is out of range (years 0001 to 9999).")
}

impl Timestamp {
    /// The instant, as UTC.
    pub fn utc(&self) -> NaiveDateTime {
        self.local - TimeDelta::seconds(self.offset.unwrap_or(0) as i64)
    }

    pub fn cmp_instant(&self, other: &Self) -> Ordering {
        self.utc().cmp(&other.utc())
    }

    /// The current time in UTC, at millisecond precision (as UTCNOW() returns it).
    pub fn now() -> Timestamp {
        let n = chrono::Utc::now().naive_utc();
        let ms = n.and_utc().timestamp_subsec_millis();
        let local = n.with_nanosecond(ms * 1_000_000).unwrap_or(n);
        Timestamp { local, offset: Some(0), precision: Precision::Fraction, frac_digits: 3 }
    }

    /// Parse a timestamp: Ion text (`2007T`, `2007-02T`, `2007-02-23`, `2007-02-23T12:14Z`,
    /// `2007-02-23T12:14:33.079-08:00`), leniently also a space instead of `T`, a missing offset
    /// (unknown), `+hhmm` offsets, and the ISO 8601 basic format (`20070223T121433Z`).
    pub fn parse(text: &str) -> Option<Timestamp> {
        let mut c = Cur { s: text.trim().as_bytes(), i: 0 };
        let year = c.digits(4)? as i32;
        if year == 0 {
            return None;
        }
        if c.peek().is_some_and(|b| b.is_ascii_digit()) {
            return parse_basic(year, &mut c);
        }
        if c.eat_t() {
            return if c.at_end() { date_only(year, 1, 1, Precision::Year) } else { None };
        }
        c.expect(b'-')?;
        let month = c.digits(2)?;
        if c.eat_t() {
            return if c.at_end() { date_only(year, month, 1, Precision::Month) } else { None };
        }
        c.expect(b'-')?;
        let day = c.digits(2)?;
        let date = NaiveDate::from_ymd_opt(year, month, day)?;
        if c.at_end() {
            return Some(date_ts(date));
        }
        match c.next()? {
            b'T' | b't' | b' ' => {}
            _ => return None,
        }
        if c.at_end() {
            return Some(date_ts(date));
        }
        parse_time(date, &mut c, true)
    }

    /// Ion text: `2007T`, `2007-02T`, `2007-02-23`, `2007-02-23T12:14Z`, `…T12:14:33.079-08:00`.
    pub fn ion_text(&self) -> String {
        let d = self.local;
        let mut s = String::with_capacity(32);
        match self.precision {
            Precision::Year => {
                let _ = write!(s, "{:04}T", d.year());
            }
            Precision::Month => {
                let _ = write!(s, "{:04}-{:02}T", d.year(), d.month());
            }
            Precision::Day => {
                let _ = write!(s, "{:04}-{:02}-{:02}", d.year(), d.month(), d.day());
            }
            p => {
                let _ = write!(s, "{:04}-{:02}-{:02}T{:02}:{:02}", d.year(), d.month(), d.day(), d.hour(), d.minute());
                if p >= Precision::Second {
                    let _ = write!(s, ":{:02}", d.second());
                }
                if p == Precision::Fraction {
                    let nanos = format!("{:09}", d.nanosecond().min(999_999_999));
                    s.push('.');
                    s.push_str(&nanos[..self.frac_digits.clamp(1, 9) as usize]);
                }
                match self.offset {
                    None => s.push_str("-00:00"),
                    Some(0) => s.push('Z'),
                    Some(o) => {
                        let sign = if o < 0 { '-' } else { '+' };
                        let a = o.unsigned_abs();
                        let _ = write!(s, "{sign}{:02}:{:02}", a / 3600, a / 60 % 60);
                    }
                }
            }
        }
        s
    }

    /// DATE_ADD: add `n` date parts; the precision grows as needed.
    pub fn add(&self, part: DatePart, n: i64) -> Result<Timestamp> {
        let l = self.local;
        let local = match part {
            DatePart::Year => add_months(l, n.checked_mul(12).ok_or_else(err_range)?),
            DatePart::Month => add_months(l, n),
            DatePart::Day => TimeDelta::try_days(n).and_then(|d| l.checked_add_signed(d)),
            DatePart::Hour => TimeDelta::try_hours(n).and_then(|d| l.checked_add_signed(d)),
            DatePart::Minute => TimeDelta::try_minutes(n).and_then(|d| l.checked_add_signed(d)),
            DatePart::Second => TimeDelta::try_seconds(n).and_then(|d| l.checked_add_signed(d)),
            _ => {
                return Err(SelectError::new("ParseExpectedDatePart", "DATE_ADD takes YEAR, MONTH, DAY, HOUR, MINUTE or SECOND."));
            }
        }
        .filter(|d| (1..=9999).contains(&d.year()))
        .ok_or_else(err_range)?;
        let precision = self.precision.max(part.precision());
        let offset = if self.precision < Precision::Minute && precision >= Precision::Minute { None } else { self.offset };
        Ok(Timestamp { local, offset, precision, frac_digits: self.frac_digits })
    }

    /// DATE_DIFF: whole date parts from `a` to `b` (negative when `b` is earlier).
    pub fn diff(part: DatePart, a: &Timestamp, b: &Timestamp) -> Result<i64> {
        let (x, y) = (a.utc(), b.utc());
        Ok(match part {
            DatePart::Year | DatePart::Month => {
                let mut months = (y.year() as i64 - x.year() as i64) * 12 + (y.month() as i64 - x.month() as i64);
                let rest = |d: &NaiveDateTime| (d.day(), d.time());
                if months > 0 && rest(&y) < rest(&x) {
                    months -= 1;
                } else if months < 0 && rest(&y) > rest(&x) {
                    months += 1;
                }
                if part == DatePart::Year { months / 12 } else { months }
            }
            DatePart::Day => (y - x).num_days(),
            DatePart::Hour => (y - x).num_hours(),
            DatePart::Minute => (y - x).num_minutes(),
            DatePart::Second => (y - x).num_seconds(),
            _ => {
                return Err(SelectError::new("ParseExpectedDatePart", "DATE_DIFF takes YEAR, MONTH, DAY, HOUR, MINUTE or SECOND."));
            }
        })
    }

    /// EXTRACT: a field of the local time, or of the offset.
    pub fn extract(&self, part: DatePart) -> i64 {
        let d = self.local;
        let off = self.offset.unwrap_or(0) as i64;
        match part {
            DatePart::Year => d.year() as i64,
            DatePart::Month => d.month() as i64,
            DatePart::Day => d.day() as i64,
            DatePart::Hour => d.hour() as i64,
            DatePart::Minute => d.minute() as i64,
            DatePart::Second => d.second() as i64,
            DatePart::TimezoneHour => off / 3600,
            DatePart::TimezoneMinute => off / 60 % 60,
        }
    }

    /// TO_STRING: format with an S3 Select (Java-style) pattern such as `'y-MM-dd''T''H:m:ssX'`.
    pub fn format(&self, pattern: &str) -> Result<String> {
        let d = self.local;
        let off = self.offset.unwrap_or(0);
        let mut out = String::with_capacity(pattern.len() + 16);
        let chars: Vec<char> = pattern.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '\'' {
                // A quoted literal; '' is a quote.
                if chars.get(i + 1) == Some(&'\'') {
                    out.push('\'');
                    i += 2;
                    continue;
                }
                i += 1;
                loop {
                    match chars.get(i) {
                        None => {
                            return Err(SelectError::new(
                                "EvaluatorUnterminatedTimestampFormatPatternToken",
                                "The timestamp format pattern contains an unterminated token in the SQL expression.",
                            ));
                        }
                        Some('\'') if chars.get(i + 1) == Some(&'\'') => {
                            out.push('\'');
                            i += 2;
                        }
                        Some('\'') => {
                            i += 1;
                            break;
                        }
                        Some(&ch) => {
                            out.push(ch);
                            i += 1;
                        }
                    }
                }
                continue;
            }
            if !c.is_ascii_alphabetic() {
                out.push(c);
                i += 1;
                continue;
            }
            let mut n = 1;
            while chars.get(i + n) == Some(&c) {
                n += 1;
            }
            i += n;
            let pad = |out: &mut String, v: i64, w: usize| {
                let _ = write!(out, "{v:0w$}");
            };
            match c {
                'y' => match n {
                    2 => pad(&mut out, (d.year() % 100) as i64, 2),
                    1 => pad(&mut out, d.year() as i64, 1),
                    _ => pad(&mut out, d.year() as i64, n),
                },
                'M' => match n {
                    1 | 2 => pad(&mut out, d.month() as i64, n),
                    3 => out.push_str(&MONTHS[d.month0() as usize][..3]),
                    4 => out.push_str(MONTHS[d.month0() as usize]),
                    5 => out.push_str(&MONTHS[d.month0() as usize][..1]),
                    _ => return Err(bad_symbol(c, n)),
                },
                'd' if n <= 2 => pad(&mut out, d.day() as i64, n),
                'a' if n == 1 => out.push_str(if d.hour() < 12 { "AM" } else { "PM" }),
                'h' if n <= 2 => pad(&mut out, (d.hour() as i64 + 11) % 12 + 1, n),
                'H' if n <= 2 => pad(&mut out, d.hour() as i64, n),
                'm' if n <= 2 => pad(&mut out, d.minute() as i64, n),
                's' if n <= 2 => pad(&mut out, d.second() as i64, n),
                'S' if n <= 9 => {
                    let nanos = format!("{:09}", d.nanosecond().min(999_999_999));
                    out.push_str(&nanos[..n]);
                }
                'n' if n == 1 => {
                    let _ = write!(out, "{}", d.nanosecond().min(999_999_999));
                }
                'X' | 'x' if n <= 5 => {
                    if c == 'X' && off == 0 {
                        out.push('Z');
                    } else {
                        let sign = if off < 0 { '-' } else { '+' };
                        let a = off.unsigned_abs();
                        let (h, m) = (a / 3600, a / 60 % 60);
                        match n {
                            1 if m == 0 => {
                                let _ = write!(out, "{sign}{h:02}");
                            }
                            1 | 2 | 4 => {
                                let _ = write!(out, "{sign}{h:02}{m:02}");
                            }
                            _ => {
                                let _ = write!(out, "{sign}{h:02}:{m:02}");
                            }
                        }
                    }
                }
                _ => return Err(bad_symbol(c, n)),
            }
        }
        Ok(out)
    }
}

const MONTHS: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

fn bad_symbol(c: char, n: usize) -> SelectError {
    SelectError::new(
        "EvaluatorInvalidTimestampFormatPatternSymbol",
        format!("The timestamp format pattern contains a symbol that is not valid: {}.", c.to_string().repeat(n)),
    )
}

fn add_months(d: NaiveDateTime, n: i64) -> Option<NaiveDateTime> {
    let m = Months::new(u32::try_from(n.unsigned_abs()).ok()?);
    if n >= 0 { d.checked_add_months(m) } else { d.checked_sub_months(m) }
}

fn date_only(year: i32, month: u32, day: u32, precision: Precision) -> Option<Timestamp> {
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    Some(Timestamp { local: date.and_time(NaiveTime::MIN), offset: None, precision, frac_digits: 0 })
}

fn date_ts(date: NaiveDate) -> Timestamp {
    Timestamp { local: date.and_time(NaiveTime::MIN), offset: None, precision: Precision::Day, frac_digits: 0 }
}

/// `hh:mm[:ss[.fff]][offset]` (extended) or `hhmm[ss[.fff]][offset]` (basic), then the end.
fn parse_time(date: NaiveDate, c: &mut Cur<'_>, extended: bool) -> Option<Timestamp> {
    let hh = c.digits(2)?;
    if extended {
        c.expect(b':')?;
    }
    let mm = c.digits(2)?;
    let (mut precision, mut ss, mut nanos, mut frac_digits) = (Precision::Minute, 0, 0, 0u8);
    let has_seconds = if extended { c.eat(b':') } else { c.peek().is_some_and(|b| b.is_ascii_digit()) };
    if has_seconds {
        ss = c.digits(2)?;
        precision = Precision::Second;
        if c.eat(b'.') || c.eat(b',') {
            let (n, d) = c.fraction()?;
            nanos = n;
            frac_digits = d;
            precision = Precision::Fraction;
        }
    }
    let offset = c.offset()?;
    if !c.at_end() {
        return None;
    }
    let time = NaiveTime::from_hms_nano_opt(hh, mm, ss, nanos)?;
    Some(Timestamp { local: date.and_time(time), offset, precision, frac_digits })
}

fn parse_basic(year: i32, c: &mut Cur<'_>) -> Option<Timestamp> {
    let month = c.digits(2)?;
    let day = c.digits(2)?;
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    if c.at_end() {
        return Some(date_ts(date));
    }
    if !c.eat_t() {
        return None;
    }
    if c.at_end() {
        return Some(date_ts(date));
    }
    parse_time(date, c, false)
}

struct Cur<'a> {
    s: &'a [u8],
    i: usize,
}

impl Cur<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }
    fn next(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.i += 1;
        Some(b)
    }
    fn at_end(&self) -> bool {
        self.i >= self.s.len()
    }
    fn eat(&mut self, b: u8) -> bool {
        if self.peek() == Some(b) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn eat_t(&mut self) -> bool {
        self.eat(b'T') || self.eat(b't')
    }
    fn expect(&mut self, b: u8) -> Option<()> {
        self.eat(b).then_some(())
    }
    fn digits(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let b = self.peek().filter(|b| b.is_ascii_digit())?;
            v = v * 10 + (b - b'0') as u32;
            self.i += 1;
        }
        Some(v)
    }
    /// Fractional seconds: nanoseconds and the number of digits kept (at most 9).
    fn fraction(&mut self) -> Option<(u32, u8)> {
        let start = self.i;
        let mut nanos = 0u32;
        while let Some(b) = self.peek().filter(|b| b.is_ascii_digit()) {
            let k = self.i - start;
            if k < 9 {
                nanos = nanos * 10 + (b - b'0') as u32;
            }
            self.i += 1;
        }
        let n = self.i - start;
        if n == 0 {
            return None;
        }
        for _ in n..9 {
            nanos *= 10;
        }
        Some((nanos, n.min(9) as u8))
    }
    /// `Z`, `±hh:mm`, `±hhmm`, `±hh`, or nothing (an unknown offset). `-00:00` is unknown.
    fn offset(&mut self) -> Option<Option<i32>> {
        match self.peek() {
            None => Some(None),
            Some(b'Z' | b'z') => {
                self.i += 1;
                Some(Some(0))
            }
            Some(sign @ (b'+' | b'-')) => {
                self.i += 1;
                let h = self.digits(2)?;
                let m = if self.at_end() {
                    0
                } else {
                    self.eat(b':');
                    self.digits(2)?
                };
                if h > 23 || m > 59 {
                    return None;
                }
                let v = (h * 3600 + m * 60) as i32;
                if sign == b'-' && v == 0 {
                    return Some(None);
                }
                Some(Some(if sign == b'-' { -v } else { v }))
            }
            Some(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        Timestamp::parse(s).unwrap_or_else(|| panic!("parse {s}"))
    }

    #[test]
    fn parse_and_print_ion() {
        for (input, out) in [
            ("2007T", "2007T"),
            ("2007-02T", "2007-02T"),
            ("2007-02-23", "2007-02-23"),
            ("2007-02-23T", "2007-02-23"),
            ("2007-02-23T12:14Z", "2007-02-23T12:14Z"),
            ("2007-02-23T12:14:33.079-08:00", "2007-02-23T12:14:33.079-08:00"),
            ("2007-02-23T12:14:33+05:30", "2007-02-23T12:14:33+05:30"),
            ("2007-02-23 12:14:33Z", "2007-02-23T12:14:33Z"),
            ("2007-02-23T12:14:33", "2007-02-23T12:14:33-00:00"),
            ("19501231T235959Z", "1950-12-31T23:59:59Z"),
            ("20200101", "2020-01-01"),
            ("20200101T1010+0100", "2020-01-01T10:10+01:00"),
        ] {
            assert_eq!(ts(input).ion_text(), out, "{input}");
        }
        for bad in ["", "2007", "07-01-01", "2007-13-01", "2007-02-30", "2007-02-23T25:00Z", "abc", "2007-02-23T12:14:33Zjunk", "0000T"] {
            assert!(Timestamp::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn date_add_matches_aws_examples() {
        let cases = [
            (DatePart::Year, 5, "2010-01-01T", "2015-01-01"),
            (DatePart::Month, 1, "2010T", "2010-02T"),
            (DatePart::Month, 13, "2010T", "2011-02T"),
            (DatePart::Day, -1, "2017-01-10T", "2017-01-09"),
            (DatePart::Hour, 1, "2017T", "2017-01-01T01:00-00:00"),
            (DatePart::Hour, 1, "2017-01-02T03:04Z", "2017-01-02T04:04Z"),
            (DatePart::Minute, 1, "2017-01-02T03:04:05.006Z", "2017-01-02T03:05:05.006Z"),
            (DatePart::Second, 1, "2017-01-02T03:04:05.006Z", "2017-01-02T03:04:06.006Z"),
            (DatePart::Month, 1, "2020-01-31T", "2020-02-29"),
        ];
        for (part, n, input, out) in cases {
            assert_eq!(ts(input).add(part, n).unwrap().ion_text(), out, "{input} + {n} {part:?}");
        }
        assert!(ts("9999-12-31T").add(DatePart::Day, 1).is_err());
    }

    #[test]
    fn date_diff_counts_whole_parts() {
        let d = |p, a, b| Timestamp::diff(p, &ts(a), &ts(b)).unwrap();
        assert_eq!(d(DatePart::Year, "2010-01-01T", "2011-01-01T"), 1);
        assert_eq!(d(DatePart::Year, "2010T", "2010-05T"), 0);
        assert_eq!(d(DatePart::Month, "2010T", "2011T"), 12);
        assert_eq!(d(DatePart::Month, "2011T", "2010T"), -12);
        assert_eq!(d(DatePart::Day, "2010-01-01T23:00Z", "2010-01-02T01:00Z"), 0);
        assert_eq!(d(DatePart::Month, "2010-01-31T", "2010-02-28T"), 0);
        assert_eq!(d(DatePart::Hour, "2010-01-01T00:00Z", "2010-01-02T00:00Z"), 24);
        assert_eq!(d(DatePart::Year, "2001-03-04T05:06:07Z", "2002-03-05T05:06:07Z"), 1);
    }

    #[test]
    fn extract_fields() {
        let t = ts("2017-01-02T03:04:05+07:08");
        assert_eq!(t.extract(DatePart::Hour), 3);
        assert_eq!(t.extract(DatePart::Minute), 4);
        assert_eq!(t.extract(DatePart::TimezoneHour), 7);
        assert_eq!(t.extract(DatePart::TimezoneMinute), 8);
        assert_eq!(ts("2010T").extract(DatePart::Month), 1);
        assert_eq!(ts("2010-10T").extract(DatePart::Month), 10);
        assert_eq!(ts("2017-01-02T03:04:05-07:30").extract(DatePart::TimezoneMinute), -30);
    }

    #[test]
    fn to_string_patterns() {
        let t = ts("1969-07-20T20:18Z");
        for (p, out) in [
            ("MMMM d, y", "July 20, 1969"),
            ("MMM d, yyyy", "Jul 20, 1969"),
            ("M-d-yy", "7-20-69"),
            ("MM-d-y", "07-20-1969"),
            ("MMMM d, y h:m a", "July 20, 1969 8:18 PM"),
            ("y-MM-dd'T'H:m:ssX", "1969-07-20T20:18:00Z"),
            ("x", "+00"),
            ("MMMMM", "J"),
            ("'it''s' yyyy", "it's 1969"),
            ("SSS n", "000 0"),
        ] {
            assert_eq!(t.format(p).unwrap(), out, "{p}");
        }
        let t = ts("1969-07-20T20:18+08:00");
        assert_eq!(t.format("y-MM-dd'T'H:m:ssXXXX").unwrap(), "1969-07-20T20:18:00+0800");
        assert_eq!(t.format("y-MM-dd'T'H:m:ssXXXXX").unwrap(), "1969-07-20T20:18:00+08:00");
        assert_eq!(t.format("X x xx xxx").unwrap(), "+08 +08 +0800 +08:00");
        assert_eq!(ts("2001-02-03T04:05:06.789Z").format("ss.SS").unwrap(), "06.78");
        assert_eq!(t.format("q").unwrap_err().code, "EvaluatorInvalidTimestampFormatPatternSymbol");
        assert_eq!(t.format("'abc").unwrap_err().code, "EvaluatorUnterminatedTimestampFormatPatternToken");
    }
}
