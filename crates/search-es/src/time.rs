//! Dates the way Elasticsearch reads and writes them: java-time format patterns, the named
//! formats (`strict_date_optional_time`, `epoch_millis`, …), `||`-combined formats, time zones
//! and date math (`now-1d/d`, `2020-01-01||+1M`).
//!
//! Patterns compile to a token list used both to parse and to print. Pattern validation
//! reproduces `DateTimeFormatter.ofPattern`'s errors (`Unknown pattern letter: o`, …) because
//! mapping errors quote them. Named formats are defined with the same tokens plus three internal
//! ones that patterns can't produce (`{F}` a 1–9 digit fraction with its point, `{Z}` any zone or
//! offset, `{P}` a 3–9 digit printed fraction), with "strict" formats requiring exact field
//! widths and the others accepting shorter numbers, as in Elasticsearch.
//!
//! Instants are seconds + nanoseconds (UTC), so `date_nanos` precision and the full `date` range
//! both fit.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike};

/// A UTC instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant {
    pub secs: i64,
    pub nanos: u32,
}

impl Instant {
    pub fn from_millis(ms: i64) -> Instant {
        Instant { secs: ms.div_euclid(1000), nanos: (ms.rem_euclid(1000) * 1_000_000) as u32 }
    }

    pub fn millis(&self) -> i64 {
        self.secs.saturating_mul(1000).saturating_add((self.nanos / 1_000_000) as i64)
    }

    pub fn nanos_since_epoch(&self) -> Option<i64> {
        self.secs.checked_mul(1_000_000_000)?.checked_add(self.nanos as i64)
    }

    fn to_naive(self) -> Option<NaiveDateTime> {
        chrono::DateTime::from_timestamp(self.secs, self.nanos).map(|d| d.naive_utc())
    }

    fn from_naive_utc(n: NaiveDateTime) -> Instant {
        let u = n.and_utc();
        Instant { secs: u.timestamp(), nanos: u.timestamp_subsec_nanos() }
    }
}

/// Java's `Instant.toString()` of epoch milliseconds: ISO-8601 in UTC, seconds always, the
/// fraction in groups of three digits only when non-zero (`2021-09-26T03:09:52Z`,
/// `…:52.120Z`), years past 9999 with a `+`.
pub fn java_instant_string(ms: i64) -> String {
    let i = Instant::from_millis(ms);
    let Some(n) = i.to_naive() else { return ms.to_string() };
    let y = n.year();
    let year = if y > 9999 {
        format!("+{y}")
    } else if y < 0 {
        format!("-{:04}", -(y as i64))
    } else {
        format!("{y:04}")
    };
    let mut out = format!("{year}-{:02}-{:02}T{:02}:{:02}:{:02}", n.month(), n.day(), n.hour(), n.minute(), n.second());
    let millis = i.nanos / 1_000_000;
    if millis > 0 {
        out.push_str(&format!(".{millis:03}"));
    }
    out.push('Z');
    out
}

/// Java's `Instant.toString()`: like [`java_instant_string`] with the fraction in groups of three
/// digits down to nanoseconds.
pub fn java_instant_string_nanos(i: Instant) -> String {
    let mut out = java_instant_string(Instant { secs: i.secs, nanos: 0 }.millis());
    out.pop();
    if i.nanos != 0 {
        let digits = format!("{:09}", i.nanos);
        let keep = if i.nanos.is_multiple_of(1_000_000) { 3 } else if i.nanos.is_multiple_of(1_000) { 6 } else { 9 };
        out.push('.');
        out.push_str(&digits[..keep]);
    }
    out.push('Z');
    out
}

/// `date_nanos` range: an instant between the epoch and 2262-04-11T23:47:16.854775807Z, as
/// nanoseconds since the epoch; otherwise `DateUtils.toLong`'s message.
pub fn to_epoch_nanos(i: Instant) -> Result<i64, String> {
    if i.secs < 0 {
        return Err(format!("date[{}] is before the epoch in 1970 and cannot be stored in nanosecond resolution", java_instant_string_nanos(i)));
    }
    i.nanos_since_epoch()
        .ok_or_else(|| format!("date[{}] is after 2262-04-11T23:47:16.854775807 and cannot be stored in nanosecond resolution", java_instant_string_nanos(i)))
}

/// A time zone: a fixed offset or a tz database zone.
#[derive(Clone, Debug, PartialEq)]
pub enum Zone {
    Offset(i32),
    Named(chrono_tz::Tz),
}

impl Zone {
    pub const UTC: Zone = Zone::Offset(0);

    /// `ZoneId.of`: `Z`, `UTC`, `+01:00`, `-0530`, `+01`, `UTC+1`, `Europe/Paris`.
    pub fn parse(s: &str) -> Result<Zone, String> {
        let t = s.trim();
        if t == "Z" || t == "UTC" || t == "GMT" || t == "UT" {
            return Ok(Zone::UTC);
        }
        let off = t.strip_prefix("UTC").or_else(|| t.strip_prefix("GMT")).or_else(|| t.strip_prefix("UT")).unwrap_or(t);
        if off.starts_with(['+', '-'])
            && let Some(o) = parse_offset(off)
        {
            return Ok(Zone::Offset(o));
        }
        t.parse::<chrono_tz::Tz>().map(Zone::Named).map_err(|_| format!("Unknown time-zone ID: {t}"))
    }

    /// The offset in seconds at a UTC instant.
    pub fn offset_at_utc(&self, n: NaiveDateTime) -> i32 {
        match self {
            Zone::Offset(o) => *o,
            Zone::Named(tz) => {
                use chrono::Offset;
                tz.offset_from_utc_datetime(&n).fix().local_minus_utc()
            }
        }
    }

    /// The offset for a local date-time (earliest on overlaps, shifted forward over gaps, as java.time does).
    pub fn offset_for_local(&self, n: NaiveDateTime) -> i32 {
        match self {
            Zone::Offset(o) => *o,
            Zone::Named(tz) => {
                use chrono::Offset;
                match tz.offset_from_local_datetime(&n) {
                    chrono::LocalResult::Single(o) => o.fix().local_minus_utc(),
                    chrono::LocalResult::Ambiguous(a, _) => a.fix().local_minus_utc(),
                    chrono::LocalResult::None => tz.offset_from_utc_datetime(&(n - Duration::hours(1))).fix().local_minus_utc(),
                }
            }
        }
    }
}

/// `+HH:MM`, `+HHMM`, `+HH`, `+H` (seconds optional) to seconds.
fn parse_offset(s: &str) -> Option<i32> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) || digits.len() > 6 {
        return None;
    }
    let (h, m, sec) = match digits.len() {
        1 | 2 => (digits.parse::<i32>().ok()?, 0, 0),
        3 => (digits[..1].parse::<i32>().ok()?, digits[1..].parse::<i32>().ok()?, 0),
        4 => (digits[..2].parse::<i32>().ok()?, digits[2..].parse::<i32>().ok()?, 0),
        6 => (digits[..2].parse::<i32>().ok()?, digits[2..4].parse::<i32>().ok()?, digits[4..].parse::<i32>().ok()?),
        _ => return None,
    };
    if h > 18 || m > 59 || sec > 59 {
        return None;
    }
    Some(sign * (h * 3600 + m * 60 + sec))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Year,
    YearOfEra,
    /// ISO week-based year (the named `week*` formats).
    WeekYear,
    /// The locale's week-based year (`Y` in patterns: weeks start on Sunday, week 1 holds
    /// January 1st, as `WeekFields` of Elasticsearch's locales).
    LocWeekYear,
    Month,
    Day,
    DayOfYear,
    /// ISO week of the week-based year (named formats).
    Week,
    /// The locale's week of week-based year (`w`).
    LocWeek,
    /// The locale's week of month (`W`).
    LocWeekOfMonth,
    /// `F`: aligned week of month (days 1–7 are week 1).
    AlignedWeekOfMonth,
    /// ISO day of week, 1 = Monday.
    DayOfWeek,
    /// The locale's day of week (`e`, `c`), 1 = Sunday.
    LocDayOfWeek,
    /// `g`: modified Julian day.
    ModifiedJulianDay,
    Hour,
    /// `h`: 1–12.
    ClockHour12,
    /// `K`: 0–11.
    Hour12,
    /// `k`: 1–24.
    ClockHour24,
    Minute,
    Second,
    MilliOfDay,
    NanoOfDay,
    NanoOfSecond,
    Quarter,
}

/// Text widths of java-time `TextStyle` (3 letters short, 4 full, 5 narrow).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Width {
    Short,
    Full,
    Narrow,
}

impl Width {
    fn of(n: usize) -> Width {
        match n {
            4 => Width::Full,
            5 => Width::Narrow,
            _ => Width::Short,
        }
    }
}

/// The locales whose texts formatters print: `Locale.ROOT` (`DateFormatter.forPattern`, date
/// math index names) and English (date fields' default `locale`). Other locales print English.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DateLocale {
    #[default]
    Root,
    English,
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Lit(String),
    Num { field: Field, min: u8, max: u8, signed: bool },
    /// `yy`: two-digit year based at 2000.
    ReducedYear,
    /// Fixed-width fraction (`SSS`) without the point.
    Fraction { min: u8, max: u8, point: bool },
    /// `MMM`+ (`standalone`: `LLL`+).
    MonthText { width: Width, standalone: bool },
    /// `EEE`+, `eee`+ (`standalone`: `ccc`+).
    DayText { width: Width, standalone: bool },
    /// `QQQ`+ (`standalone`: `qqq`+).
    QuarterText { width: Width, standalone: bool },
    AmPm,
    /// `B`: day period (`narrow` for `BBBBB`).
    DayPeriod { narrow: bool },
    Era { width: Width },
    /// `O`, `OOOO`, `ZZZZ`: `GMT`, `GMT+1` / `GMT+01:00`.
    LocalizedOffset { full: bool },
    /// `X`/`x`/`Z`/`{Z}` styles: (`colon`, `seconds`, `z_for_zero`, `optional_minutes`).
    Offset { colon: bool, z_for_zero: bool, minutes: OffsetMinutes },
    /// `VV`, `z`: zone id (or any offset/zone when parsing).
    ZoneId,
    /// `{Z}`: any zone or offset (parsing); prints like `XXX`.
    AnyZone,
    Optional(Vec<Tok>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum OffsetMinutes {
    /// `+HH` or `+HHMM` (X).
    Optional,
    Required,
}

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

/// Compiles a java-time pattern (or an internal named-format definition).
fn compile_pattern(p: &str, lenient: bool) -> Result<Vec<Tok>, String> {
    compile_pattern_in(p, lenient, false)
}

/// `iso`: week letters mean ISO weeks (the named `week*` formats are built on `IsoFields`).
fn compile_pattern_in(p: &str, lenient: bool, iso: bool) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = p.chars().collect();
    let mut stack: Vec<Vec<Tok>> = vec![Vec::new()];
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '{' {
            // Internal tokens.
            let end = chars[i..].iter().position(|c| *c == '}').map(|e| e + i);
            let name: String = match end {
                Some(e) => chars[i + 1..e].iter().collect(),
                None => return Err("Pattern includes reserved character: '{'".into()),
            };
            let tok = match name.as_str() {
                "F" => Tok::Fraction { min: 1, max: 9, point: true },
                "P" => Tok::Fraction { min: 3, max: 9, point: true },
                "Z" => Tok::AnyZone,
                // The ISO-8601 parser's year: four digits, optionally negative.
                "Y" => Tok::Num { field: Field::Year, min: 4, max: 4, signed: true },
                _ => return Err("Pattern includes reserved character: '{'".into()),
            };
            stack.last_mut().unwrap().push(tok);
            i = end.unwrap() + 1;
            continue;
        }
        if c.is_ascii_alphabetic() {
            let mut n = 1;
            while i + n < chars.len() && chars[i + n] == c {
                n += 1;
            }
            let tok = letter_token(c, n, lenient, iso)?;
            stack.last_mut().unwrap().push(tok);
            i += n;
            continue;
        }
        match c {
            '\'' => {
                let mut lit = String::new();
                let mut j = i + 1;
                loop {
                    if j >= chars.len() {
                        return Err(format!("Pattern ends with an incomplete string literal: {p}"));
                    }
                    if chars[j] == '\'' {
                        if j + 1 < chars.len() && chars[j + 1] == '\'' {
                            lit.push('\'');
                            j += 2;
                            continue;
                        }
                        break;
                    }
                    lit.push(chars[j]);
                    j += 1;
                }
                if lit.is_empty() && j == i + 1 {
                    lit.push('\'');
                }
                stack.last_mut().unwrap().push(Tok::Lit(lit));
                i = j + 1;
            }
            '[' => {
                stack.push(Vec::new());
                i += 1;
            }
            ']' => {
                if stack.len() == 1 {
                    return Err("Pattern invalid as it contains ] without previous [".into());
                }
                let inner = stack.pop().unwrap();
                stack.last_mut().unwrap().push(Tok::Optional(inner));
                i += 1;
            }
            '#' | '}' => return Err(format!("Pattern includes reserved character: '{c}'")),
            _ => {
                stack.last_mut().unwrap().push(Tok::Lit(c.to_string()));
                i += 1;
            }
        }
    }
    while stack.len() > 1 {
        let inner = stack.pop().unwrap();
        stack.last_mut().unwrap().push(Tok::Optional(inner));
    }
    Ok(stack.pop().unwrap())
}

fn letter_token(c: char, n: usize, lenient: bool, iso: bool) -> Result<Tok, String> {
    let too_many = || Err(format!("Too many pattern letters: {c}"));
    let num = |field: Field, min: usize, max: usize, signed: bool| {
        let min = if lenient { 1 } else { min };
        Ok(Tok::Num { field, min: min as u8, max: max as u8, signed })
    };
    let two = |field: Field| match n {
        1 => num(field, 1, 2, false),
        2 => num(field, 2, 2, false),
        _ => too_many(),
    };
    match c {
        'y' | 'u' | 'Y' => {
            let field = match c {
                'y' => Field::YearOfEra,
                'u' => Field::Year,
                _ if iso => Field::WeekYear,
                _ => Field::LocWeekYear,
            };
            match n {
                2 if !lenient => Ok(Tok::ReducedYear),
                1 => num(field, 1, 19, true),
                4 => num(field, 4, 19, true),
                n if n > 19 => too_many(),
                n => num(field, n, 19, true),
            }
        }
        'M' | 'L' => match n {
            1 => num(Field::Month, 1, 2, false),
            2 => num(Field::Month, 2, 2, false),
            3..=5 => Ok(Tok::MonthText { width: Width::of(n), standalone: c == 'L' }),
            _ => too_many(),
        },
        'd' => two(Field::Day),
        'D' => match n {
            1 => num(Field::DayOfYear, 1, 3, false),
            2 => num(Field::DayOfYear, 2, 3, false),
            3 => num(Field::DayOfYear, 3, 3, false),
            _ => too_many(),
        },
        'H' => two(Field::Hour),
        'h' => two(Field::ClockHour12),
        'K' => two(Field::Hour12),
        'k' => two(Field::ClockHour24),
        'm' => two(Field::Minute),
        's' => two(Field::Second),
        'w' => two(if iso { Field::Week } else { Field::LocWeek }),
        'W' | 'F' => match n {
            1 => num(if c == 'W' { Field::LocWeekOfMonth } else { Field::AlignedWeekOfMonth }, 1, 1, false),
            _ => too_many(),
        },
        'S' => Ok(Tok::Fraction { min: n as u8, max: n as u8, point: false }),
        'n' => num(Field::NanoOfSecond, n.max(1), 19, false),
        'N' => num(Field::NanoOfDay, n.max(1), 19, false),
        'A' => num(Field::MilliOfDay, n.max(1), 19, false),
        'a' => {
            if n > 1 {
                too_many()
            } else {
                Ok(Tok::AmPm)
            }
        }
        'E' => match n {
            1..=5 => Ok(Tok::DayText { width: Width::of(n), standalone: false }),
            _ => too_many(),
        },
        'e' | 'c' => match n {
            1 | 2 if c == 'c' && n == 2 => Err("Invalid pattern \"cc\"".into()),
            1 | 2 => num(if iso { Field::DayOfWeek } else { Field::LocDayOfWeek }, n, 2, false),
            3..=5 => Ok(Tok::DayText { width: Width::of(n), standalone: c == 'c' }),
            _ => too_many(),
        },
        'Q' | 'q' => match n {
            1 | 2 => num(Field::Quarter, n, 2, false),
            3..=5 => Ok(Tok::QuarterText { width: Width::of(n), standalone: c == 'q' }),
            _ => too_many(),
        },
        'G' => match n {
            1..=3 => Ok(Tok::Era { width: Width::Short }),
            4 | 5 => Ok(Tok::Era { width: Width::of(n) }),
            _ => too_many(),
        },
        'B' => match n {
            1 | 4 | 5 => Ok(Tok::DayPeriod { narrow: n == 5 }),
            _ => Err(format!("Wrong number of pattern letters: {c}")),
        },
        'X' | 'x' => {
            let z = c == 'X';
            match n {
                1 => Ok(Tok::Offset { colon: false, z_for_zero: z, minutes: OffsetMinutes::Optional }),
                2 | 4 => Ok(Tok::Offset { colon: false, z_for_zero: z, minutes: OffsetMinutes::Required }),
                3 | 5 => Ok(Tok::Offset { colon: true, z_for_zero: z, minutes: OffsetMinutes::Required }),
                _ => too_many(),
            }
        }
        'Z' => match n {
            1..=3 => Ok(Tok::Offset { colon: false, z_for_zero: false, minutes: OffsetMinutes::Required }),
            4 => Ok(Tok::LocalizedOffset { full: true }),
            5 => Ok(Tok::Offset { colon: true, z_for_zero: true, minutes: OffsetMinutes::Required }),
            _ => too_many(),
        },
        'O' => match n {
            1 | 4 => Ok(Tok::LocalizedOffset { full: n == 4 }),
            _ => Err(format!("Pattern letter count must be 1 or 4: {c}")),
        },
        'V' => {
            if n != 2 {
                Err(format!("Pattern letter count must be 2: {c}"))
            } else {
                Ok(Tok::ZoneId)
            }
        }
        'v' => Ok(Tok::ZoneId),
        'z' => {
            if n > 4 {
                too_many()
            } else {
                Ok(Tok::ZoneId)
            }
        }
        'g' => num(Field::ModifiedJulianDay, n, 19, false),
        other => Err(format!("Unknown pattern letter: {other}")),
    }
}

#[derive(Clone, Debug)]
enum Alt {
    EpochMillis,
    EpochSecond,
    Pattern { parse: Vec<Tok>, print: Vec<Tok>, week_based: bool, mode: Mode, parsers: u8 },
}

/// How an alternative turns parsed fields into an instant, which decides the error a bad value
/// gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// java-time `DateTimeFormatter.parse` with this resolver style: impossible values fail
    /// inside the parse (`Text '…' could not be parsed: …`). With `defer_date` (year-of-era
    /// patterns, which `STRICT` leaves unresolved) the date fields are only checked afterwards,
    /// by `DateFormatters.from`, whose `DateTimeException` surfaces unwrapped.
    Java { resolver: Resolver, defer_date: bool },
    /// Elasticsearch's own ISO-8601 parser: every syntax or range error is `Could not fully
    /// parse datetime`; impossible dates (February 30) fail later in `DateFormatters.from`.
    Iso,
}

/// java-time's `ResolverStyle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resolver {
    Strict,
    /// Day-of-month 29–31 past the month's end is clamped to it.
    Smart,
    /// Months and days past their ranges roll over (`strict_date`'s `ISO_LOCAL_DATE`).
    Lenient,
}

/// A date that doesn't parse, with what Elasticsearch reports for it.
#[derive(Clone, Debug, PartialEq)]
pub struct DateError {
    pub text: String,
    pub format: String,
    pub kind: DateErrorKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DateErrorKind {
    /// `cannot parse empty datetime`.
    Empty,
    /// `failed to parse date field [..] with format [..]` caused by a `DateTimeParseException`
    /// with this reason, itself caused by a `DateTimeException` when resolving failed.
    Parse { reason: String, cause: Option<String> },
    /// A `DateTimeException` from `DateFormatters.from`, not wrapped.
    Invalid(String),
}

impl DateError {
    /// The top-level exception's reason.
    pub fn reason(&self) -> String {
        match &self.kind {
            DateErrorKind::Empty => "cannot parse empty datetime".to_string(),
            DateErrorKind::Parse { .. } => format!("failed to parse date field [{}] with format [{}]", self.text, self.format),
            DateErrorKind::Invalid(msg) => msg.clone(),
        }
    }

    /// The exception chain Elasticsearch renders.
    pub fn to_es(&self) -> crate::error::EsError {
        use crate::error::EsError;
        match &self.kind {
            DateErrorKind::Empty => EsError::illegal_argument(self.reason()),
            DateErrorKind::Parse { reason, cause } => {
                let mut parse = EsError::java(400, "date_time_parse_exception", reason.clone());
                if let Some(c) = cause {
                    parse = parse.caused_by(EsError::java(400, "date_time_exception", c.clone()));
                }
                EsError::illegal_argument(self.reason()).caused_by(parse)
            }
            DateErrorKind::Invalid(msg) => EsError::java(400, "date_time_exception", msg.clone()),
        }
    }
}

/// How one alternative failed.
enum AltFail {
    /// Syntax error at this index.
    At(usize),
    /// Parsed a prefix ending here.
    Unparsed(usize),
    /// Resolving the parsed fields failed inside the parse.
    Resolve(String),
    /// The ISO parser's single message.
    Iso,
    /// Parsed, but `DateFormatters.from` can't make a date of it (unwrapped).
    Invalid(String),
}

/// Why parsed fields don't make an instant: a `DateTimeException` message, whether it concerns
/// the date (rather than the time of day), and whether it is a single field out of its range.
struct ResolveFail {
    msg: String,
    date: bool,
    range: bool,
}

impl ResolveFail {
    fn date(msg: String, range: bool) -> ResolveFail {
        ResolveFail { msg, date: true, range }
    }

    fn time(msg: String) -> ResolveFail {
        ResolveFail { msg, date: false, range: true }
    }
}

/// `DateTimeParseException`'s quoting: texts over 64 chars are cut with `...`.
fn quoted_text(text: &str) -> String {
    if text.chars().count() > 64 {
        let cut: String = text.chars().take(64).collect();
        format!("{cut}...")
    } else {
        text.to_string()
    }
}

/// A compiled (possibly `||`-combined) date format.
#[derive(Clone, Debug)]
pub struct DateFormatter {
    pub pattern: String,
    alts: Vec<Alt>,
    /// Whose month, day, era and quarter texts print and parse.
    pub locale: DateLocale,
}

/// Named formats: (name, parser definition, printer definition, strict).
fn named(name: &str) -> Option<(&'static str, &'static str, bool)> {
    const DOT: &str = "yyyy[-MM[-dd['T'[HH[:mm[:ss[{F}]]]][{Z}]]]]";
    const DOT_PRINT: &str = "yyyy-MM-dd'T'HH:mm:ss.SSSXXX";
    let strip = name.strip_prefix("strict_");
    let strict = strip.is_some();
    let base = strip.unwrap_or(name);
    let (parse, print): (&str, &str) = match base {
        "date_optional_time" | "iso8601" => (DOT, DOT_PRINT),
        "date_optional_time_nanos" => (DOT, "yyyy-MM-dd'T'HH:mm:ss{P}XXX"),
        "date_time" => ("yyyy-MM-dd'T'HH:mm:ss{F}{Z}", "yyyy-MM-dd'T'HH:mm:ss.SSSXXX"),
        "date_time_no_millis" => ("yyyy-MM-dd'T'HH:mm:ss{Z}", "yyyy-MM-dd'T'HH:mm:ssXXX"),
        "date" | "year_month_day" => ("yyyy-MM-dd", "yyyy-MM-dd"),
        "date_hour" => ("yyyy-MM-dd'T'HH", "yyyy-MM-dd'T'HH"),
        "date_hour_minute" => ("yyyy-MM-dd'T'HH:mm", "yyyy-MM-dd'T'HH:mm"),
        "date_hour_minute_second" => ("yyyy-MM-dd'T'HH:mm:ss", "yyyy-MM-dd'T'HH:mm:ss"),
        "date_hour_minute_second_fraction" | "date_hour_minute_second_millis" => ("yyyy-MM-dd'T'HH:mm:ss{F}", "yyyy-MM-dd'T'HH:mm:ss.SSS"),
        "hour" => ("HH", "HH"),
        "hour_minute" => ("HH:mm", "HH:mm"),
        "hour_minute_second" => ("HH:mm:ss", "HH:mm:ss"),
        "hour_minute_second_fraction" | "hour_minute_second_millis" => ("HH:mm:ss{F}", "HH:mm:ss.SSS"),
        "ordinal_date" => ("yyyy-DDD", "yyyy-DDD"),
        "ordinal_date_time" => ("yyyy-DDD'T'HH:mm:ss{F}{Z}", "yyyy-DDD'T'HH:mm:ss.SSSXXX"),
        "ordinal_date_time_no_millis" => ("yyyy-DDD'T'HH:mm:ss{Z}", "yyyy-DDD'T'HH:mm:ssXXX"),
        "time" => ("HH:mm:ss{F}{Z}", "HH:mm:ss.SSSXXX"),
        "time_no_millis" => ("HH:mm:ss{Z}", "HH:mm:ssXXX"),
        "t_time" => ("'T'HH:mm:ss{F}{Z}", "'T'HH:mm:ss.SSSXXX"),
        "t_time_no_millis" => ("'T'HH:mm:ss{Z}", "'T'HH:mm:ssXXX"),
        "week_date" => ("YYYY-'W'ww-e", "YYYY-'W'ww-e"),
        "week_date_time" => ("YYYY-'W'ww-e'T'HH:mm:ss{F}{Z}", "YYYY-'W'ww-e'T'HH:mm:ss.SSSXXX"),
        "week_date_time_no_millis" => ("YYYY-'W'ww-e'T'HH:mm:ss{Z}", "YYYY-'W'ww-e'T'HH:mm:ssXXX"),
        "weekyear" => ("YYYY", "YYYY"),
        "weekyear_week" => ("YYYY-'W'ww", "YYYY-'W'ww"),
        "weekyear_week_day" => ("YYYY-'W'ww-e", "YYYY-'W'ww-e"),
        "year" => ("yyyy", "yyyy"),
        "year_month" => ("yyyy-MM", "yyyy-MM"),
        "basic_date" => ("yyyyMMdd", "yyyyMMdd"),
        "basic_date_time" => ("yyyyMMdd'T'HHmmss{F}{Z}", "yyyyMMdd'T'HHmmss.SSSX"),
        "basic_date_time_no_millis" => ("yyyyMMdd'T'HHmmss{Z}", "yyyyMMdd'T'HHmmssX"),
        "basic_ordinal_date" => ("yyyyDDD", "yyyyDDD"),
        "basic_ordinal_date_time" => ("yyyyDDD'T'HHmmss{F}{Z}", "yyyyDDD'T'HHmmss.SSSX"),
        "basic_ordinal_date_time_no_millis" => ("yyyyDDD'T'HHmmss{Z}", "yyyyDDD'T'HHmmssX"),
        "basic_time" => ("HHmmss{F}{Z}", "HHmmss.SSSX"),
        "basic_time_no_millis" => ("HHmmss{Z}", "HHmmssX"),
        "basic_t_time" => ("'T'HHmmss{F}{Z}", "'T'HHmmss.SSSX"),
        "basic_t_time_no_millis" => ("'T'HHmmss{Z}", "'T'HHmmssX"),
        "basic_week_date" => ("YYYY'W'wwe", "YYYY'W'wwe"),
        "basic_week_date_time" => ("YYYY'W'wwe'T'HHmmss{F}{Z}", "YYYY'W'wwe'T'HHmmss.SSSX"),
        "basic_week_date_time_no_millis" => ("YYYY'W'wwe'T'HHmmss{Z}", "YYYY'W'wwe'T'HHmmssX"),
        _ => return None,
    };
    // The basic formats are fixed-width already; only `strict_` changes the extended ones.
    let strict = strict || base.starts_with("basic_");
    if !strict && name.starts_with("strict_") {
        return None;
    }
    Some((parse, print, strict))
}

const NAMED_STRICT_ONLY: &[&str] = &["strict_date_optional_time_nanos"];

/// Named formats parsed by Elasticsearch's ISO-8601 parser.
const NAMED_ISO: &[&str] = &[
    "strict_date_optional_time",
    "strict_date_optional_time_nanos",
    "iso8601",
    "strict_year_month",
    "strict_year",
    "strict_date_time",
    "strict_date_time_no_millis",
    "strict_date_hour_minute_second",
    "strict_date_hour_minute_second_fraction",
    "strict_date_hour_minute_second_millis",
];

/// Named formats built with `DateTimeFormatter.ofPattern` alone (`ResolverStyle.SMART`).
const NAMED_SMART: &[&str] = &["basic_ordinal_date", "strict_date_hour", "strict_date_hour_minute", "strict_hour", "strict_hour_minute"];

/// Named formats with several parsers (variants of the zone or fraction).
const NAMED_MULTI: &[&str] = &[
    "basic_time",
    "basic_time_no_millis",
    "basic_t_time",
    "basic_t_time_no_millis",
    "basic_date_time",
    "basic_date_time_no_millis",
    "basic_ordinal_date_time",
    "basic_ordinal_date_time_no_millis",
    "basic_week_date_time",
    "basic_week_date_time_no_millis",
    "strict_basic_week_date_time",
    "strict_basic_week_date_time_no_millis",
    "strict_ordinal_date_time",
    "strict_ordinal_date_time_no_millis",
    "strict_time",
    "strict_time_no_millis",
    "strict_t_time",
    "strict_t_time_no_millis",
    "strict_week_date_time",
    "strict_week_date_time_no_millis",
    "date_time",
    "date_time_no_millis",
    "ordinal_date_time",
    "ordinal_date_time_no_millis",
    "week_date_time",
    "week_date_time_no_millis",
    "time",
    "time_no_millis",
    "t_time",
    "t_time_no_millis",
];

/// The pattern letters outside quoted literals.
fn pattern_letters(p: &str) -> Vec<char> {
    let mut quoted = false;
    let mut out = Vec::new();
    for c in p.chars() {
        if c == '\'' {
            quoted = !quoted;
        } else if !quoted && c.is_ascii_alphabetic() && !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

fn is_known_named(name: &str) -> bool {
    if name == "epoch_millis" || name == "epoch_second" {
        return true;
    }
    if name == "date_optional_time_nanos" {
        return false;
    }
    if NAMED_STRICT_ONLY.contains(&name) {
        return true;
    }
    if let Some(base) = name.strip_prefix("strict_") {
        // strict_basic_* only exists for the week formats.
        if base.starts_with("basic_") && !base.starts_with("basic_week_date") {
            return false;
        }
        return named(base).is_some();
    }
    named(name).is_some()
}

impl DateFormatter {
    pub const DEFAULT: &'static str = "strict_date_optional_time||epoch_millis";

    /// `DateFormatter.forPattern`.
    pub fn new(pattern: &str) -> Result<DateFormatter, String> {
        if pattern.is_empty() {
            return Err("No date pattern provided".into());
        }
        let input = pattern.strip_prefix('8').unwrap_or(pattern);
        let mut alts = Vec::new();
        for part in split_combined(input) {
            if part.is_empty() {
                return Err(format!("Cannot have empty element in multi date format pattern: {input}"));
            }
            alts.push(Self::alt(part).map_err(|e| format!("Invalid format: [{part}]: {e}"))?);
        }
        Ok(DateFormatter { pattern: pattern.to_string(), alts, locale: DateLocale::Root })
    }

    /// `DateFormatter.withLocale`.
    pub fn with_locale(mut self, locale: DateLocale) -> DateFormatter {
        self.locale = locale;
        self
    }

    fn alt(part: &str) -> Result<Alt, String> {
        match part {
            "epoch_millis" => return Ok(Alt::EpochMillis),
            "epoch_second" => return Ok(Alt::EpochSecond),
            _ => {}
        }
        if is_known_named(part) {
            let base = part.strip_prefix("strict_").unwrap_or(part);
            let base = if base == "date_optional_time_nanos" { "date_optional_time_nanos" } else { base };
            let (p, pr, strict) = named(base).expect("named");
            let strict = strict || part.starts_with("strict_");
            // Named formats use the proleptic year (`uuuu`), so years before 1 parse and print
            // signed. The strict ISO-8601 ones go through Elasticsearch's own parser, which
            // takes exactly four year digits.
            let iso = (strict && matches!(base, "date_optional_time" | "date_optional_time_nanos" | "date_time" | "date_time_no_millis")) || base == "iso8601";
            let p = p.replacen("yyyy", if iso { "{Y}" } else { "uuuu" }, 1);
            let pr = pr.replacen("yyyy", "uuuu", 1);
            let mode = if NAMED_ISO.contains(&part) {
                Mode::Iso
            } else {
                let resolver = match part {
                    "strict_date" => Resolver::Lenient,
                    _ if NAMED_SMART.contains(&part) => Resolver::Smart,
                    _ => Resolver::Strict,
                };
                Mode::Java { resolver, defer_date: false }
            };
            return Ok(Alt::Pattern {
                parse: compile_pattern_in(&p, !strict, true)?,
                print: compile_pattern_in(&pr, false, true)?,
                week_based: base.contains("week"),
                mode,
                parsers: if NAMED_MULTI.contains(&part) { 2 } else { 1 },
            });
        }
        let toks = compile_pattern(part, false)?;
        // `STRICT` only resolves the date inside the parse from a proleptic year (`u`) or a
        // year-of-era with its era; otherwise `DateFormatters.from` builds it afterwards.
        let letters = pattern_letters(part);
        let in_parse = letters.contains(&'u') || (letters.contains(&'y') && letters.contains(&'G'));
        Ok(Alt::Pattern { print: toks.clone(), parse: toks, week_based: false, mode: Mode::Java { resolver: Resolver::Strict, defer_date: !in_parse }, parsers: 1 })
    }

    /// How many parsers Elasticsearch tries for this format: with more than one, a failure is
    /// reported as `Failed to parse with all enclosed parsers`.
    fn parsers(&self) -> usize {
        self.alts
            .iter()
            .map(|a| match a {
                Alt::EpochMillis | Alt::EpochSecond => 2,
                Alt::Pattern { parsers, .. } => *parsers as usize,
            })
            .sum()
    }

    /// Parses `text` to an instant. Missing fields default to their minimum, or with `round_up`
    /// to their maximum (Elasticsearch's round-up parser for `lte`/`gt` bounds).
    pub fn parse(&self, text: &str, zone: Option<&Zone>, round_up: bool) -> Result<Instant, String> {
        self.parse_detailed(text, zone, round_up).map_err(|e| e.reason())
    }

    /// [`DateFormatter::parse`] with the error Elasticsearch reports (`JavaDateFormatter.parse`
    /// then `DateFormatters.from`).
    pub fn parse_detailed(&self, text: &str, zone: Option<&Zone>, round_up: bool) -> Result<Instant, DateError> {
        let err = |kind| DateError { text: text.to_string(), format: self.pattern.clone(), kind };
        if text.is_empty() {
            return Err(err(DateErrorKind::Empty));
        }
        let multi = self.parsers() > 1;
        for alt in &self.alts {
            let fail = match parse_alt(alt, text, zone, round_up, self.locale) {
                Ok(i) => return Ok(i),
                Err(f) => f,
            };
            let q = || quoted_text(text);
            let (reason, cause) = match fail {
                AltFail::Invalid(msg) => return Err(err(DateErrorKind::Invalid(msg))),
                _ if multi => continue,
                AltFail::At(i) => (format!("Text '{}' could not be parsed at index {i}", q()), None),
                AltFail::Unparsed(i) => (format!("Text '{}' could not be parsed, unparsed text found at index {i}", q()), None),
                AltFail::Resolve(msg) => (format!("Text '{}' could not be parsed: {msg}", q()), Some(msg)),
                AltFail::Iso => ("Could not fully parse datetime".to_string(), None),
            };
            return Err(err(DateErrorKind::Parse { reason, cause }));
        }
        Err(err(DateErrorKind::Parse { reason: "Failed to parse with all enclosed parsers".into(), cause: None }))
    }

    pub fn parse_millis(&self, text: &str) -> Result<i64, String> {
        self.parse(text, None, false).map(|i| i.millis())
    }

    /// Prints with the first format of the combination.
    pub fn format(&self, i: Instant, zone: &Zone) -> String {
        match &self.alts[0] {
            Alt::EpochMillis => {
                let ms = i.millis();
                let sub = i.nanos % 1_000_000;
                if sub == 0 { ms.to_string() } else { format!("{ms}.{}", trim_zeros(&format!("{sub:06}"))) }
            }
            Alt::EpochSecond => {
                if i.nanos == 0 {
                    i.secs.to_string()
                } else {
                    format!("{}.{}", i.secs, trim_zeros(&format!("{:09}", i.nanos)))
                }
            }
            Alt::Pattern { print, .. } => {
                let Some(utc) = i.to_naive() else { return i.millis().to_string() };
                let off = zone.offset_at_utc(utc);
                let local = utc + Duration::seconds(off as i64);
                let mut out = String::new();
                print_toks(print, local, off, zone, self.locale, &mut out);
                out
            }
        }
    }

    pub fn format_millis(&self, ms: i64) -> String {
        self.format(Instant::from_millis(ms), &Zone::UTC)
    }
}

fn trim_zeros(s: &str) -> &str {
    let t = s.trim_end_matches('0');
    if t.is_empty() { "0" } else { t }
}

/// `DateFormatter.splitCombinedPatterns`.
fn split_combined(input: &str) -> Vec<&str> {
    input.split("||").collect()
}

/// Validates a mapping `format` the way Elasticsearch does when building the formatter.
pub fn validate_format(pattern: &str) -> Result<(), String> {
    DateFormatter::new(pattern).map(|_| ())
}

#[derive(Default, Debug)]
struct Parsed {
    year: Option<i64>,
    year_of_era: Option<i64>,
    week_year: Option<i64>,
    month: Option<u32>,
    day: Option<u32>,
    day_of_year: Option<u32>,
    week: Option<u32>,
    dow: Option<u32>,
    hour: Option<u32>,
    clock12: Option<u32>,
    hour12: Option<u32>,
    pm: Option<bool>,
    minute: Option<u32>,
    second: Option<u32>,
    nano: Option<u32>,
    milli_of_day: Option<u64>,
    offset: Option<i32>,
    zone: Option<Zone>,
    era_bc: bool,
    /// Week fields came from `Y`/`w` (Sunday-first weeks) rather than ISO ones.
    loc_weeks: bool,
}

fn parse_alt(alt: &Alt, text: &str, zone: Option<&Zone>, round_up: bool, locale: DateLocale) -> Result<Instant, AltFail> {
    match alt {
        Alt::EpochMillis | Alt::EpochSecond => parse_epoch(alt, text).ok_or(AltFail::At(0)),
        Alt::Pattern { parse, week_based, mode, .. } => {
            let mut p = Parsed::default();
            let bytes = text.as_bytes();
            let iso = *mode == Mode::Iso;
            let end = match_toks(parse, bytes, 0, &mut p, locale).map_err(|at| if iso { AltFail::Iso } else { AltFail::At(at) })?;
            if end != bytes.len() {
                return Err(if iso { AltFail::Iso } else { AltFail::Unparsed(end) });
            }
            let (resolver, defer_date) = match mode {
                Mode::Java { resolver, defer_date } => (*resolver, *defer_date),
                Mode::Iso => (Resolver::Strict, true),
            };
            resolve(&p, zone, round_up, *week_based, resolver, defer_date).map_err(|f| match (mode, f.date) {
                // The ISO parser range-checks every field itself.
                (Mode::Iso, _) if f.range => AltFail::Iso,
                (Mode::Iso, true) => AltFail::Invalid(f.msg),
                (Mode::Iso, false) => AltFail::Iso,
                (Mode::Java { defer_date: true, .. }, true) => AltFail::Invalid(f.msg),
                _ => AltFail::Resolve(f.msg),
            })
        }
    }
}

fn parse_epoch(alt: &Alt, text: &str) -> Option<Instant> {
    let scale: i128 = if matches!(alt, Alt::EpochMillis) { 1_000_000 } else { 1_000_000_000 };
    let (neg, body) = match text.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, text),
    };
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) || int.len() > 18 {
        return None;
    }
    let int_v: i128 = int.parse().ok()?;
    let frac_digits = if matches!(alt, Alt::EpochMillis) { 6 } else { 9 };
    if frac.len() > frac_digits {
        return None;
    }
    let frac_v: i128 = if frac.is_empty() { 0 } else { format!("{frac:0<width$}", width = frac_digits).parse().ok()? };
    let mut nanos = int_v * scale + frac_v;
    if neg {
        nanos = -nanos;
    }
    let secs = nanos.div_euclid(1_000_000_000) as i64;
    let n = nanos.rem_euclid(1_000_000_000) as u32;
    Some(Instant { secs, nanos: n })
}

/// Matches tokens at `pos`, returning the end position, or the index where matching failed
/// (java-time's error index: the start of the token that didn't match).
fn match_toks(toks: &[Tok], s: &[u8], mut pos: usize, p: &mut Parsed, locale: DateLocale) -> Result<usize, usize> {
    for (i, t) in toks.iter().enumerate() {
        let at = pos;
        match t {
            Tok::Lit(l) => {
                if l.is_empty() {
                    continue;
                }
                let lb = l.as_bytes();
                if s.len() < pos + lb.len() || !s[pos..pos + lb.len()].eq_ignore_ascii_case(lb) {
                    return Err(at);
                }
                pos += lb.len();
            }
            Tok::Num { field, min, max, signed } => {
                let mut neg = false;
                let mut start = pos;
                let mut has_sign = false;
                if *signed && pos < s.len() && (s[pos] == b'-' || s[pos] == b'+') {
                    neg = s[pos] == b'-';
                    has_sign = true;
                    start += 1;
                }
                let avail = s[start..].iter().take_while(|b| b.is_ascii_digit()).count();
                // Adjacent value parsing: leave room for following fixed-width numbers.
                let reserved: usize = toks[i + 1..]
                    .iter()
                    .take_while(|t| matches!(t, Tok::Num { .. } | Tok::ReducedYear | Tok::Fraction { point: false, .. }))
                    .map(|t| match t {
                        Tok::Num { min, .. } => *min as usize,
                        Tok::ReducedYear => 2,
                        Tok::Fraction { min, .. } => *min as usize,
                        _ => 0,
                    })
                    .sum();
                // java-time's second pass: at least the minimum width, the rest left for them.
                let take = avail.saturating_sub(reserved).max(*min as usize).min(*max as usize).min(avail);
                if take < *min as usize || take == 0 {
                    return Err(at);
                }
                // Strict EXCEEDS_PAD (four or more letters): `+` only on values wider than the
                // pad width, which then require it; `-` always.
                if *signed && *min >= 4 && !neg && has_sign == (take <= *min as usize) {
                    return Err(at);
                }
                let v: i64 = std::str::from_utf8(&s[start..start + take]).ok().ok_or(at)?.parse().ok().ok_or(at)?;
                let v = if neg { -v } else { v };
                set_field(p, *field, v).ok_or(at)?;
                pos = start + take;
            }
            Tok::ReducedYear => {
                if s.len() < pos + 2 || !s[pos].is_ascii_digit() || !s[pos + 1].is_ascii_digit() {
                    return Err(at);
                }
                let v = ((s[pos] - b'0') * 10 + (s[pos + 1] - b'0')) as i64;
                p.year = Some(2000 + v);
                pos += 2;
            }
            Tok::Fraction { min, max, point } => {
                if *point {
                    if pos >= s.len() || (s[pos] != b'.' && s[pos] != b',') {
                        return Err(at);
                    }
                    pos += 1;
                }
                let n = s[pos..].iter().take_while(|b| b.is_ascii_digit()).count().min(*max as usize);
                if n < *min as usize || n == 0 {
                    return Err(at);
                }
                let digits = std::str::from_utf8(&s[pos..pos + n]).ok().ok_or(at)?;
                p.nano = Some(format!("{digits:0<9}").parse().ok().ok_or(at)?);
                pos += n;
            }
            // Texts parse case-sensitively, in the formatter's locale (java-time's default).
            Tok::MonthText { width, standalone } => {
                let (m, len) = match_text(&s[pos..], 12, |i| month_text(locale, *width, *standalone, i)).ok_or(at)?;
                p.month = Some(m as u32 + 1);
                pos += len;
            }
            Tok::DayText { width, standalone } => {
                let (d, len) = match_text(&s[pos..], 7, |i| day_text(locale, *width, *standalone, i)).ok_or(at)?;
                p.dow = Some(d as u32 + 1);
                pos += len;
            }
            Tok::QuarterText { width, standalone } => {
                let (_, len) = match_text(&s[pos..], 4, |i| quarter_text(locale, *width, *standalone, i as u32 + 1)).ok_or(at)?;
                pos += len;
            }
            Tok::AmPm => {
                let (i, len) = match_text(&s[pos..], 2, |i| ["AM", "PM"][i].to_string()).ok_or(at)?;
                p.pm = Some(i == 1);
                pos += len;
            }
            Tok::DayPeriod { narrow } => {
                let (_, len) = match_text(&s[pos..], DAY_PERIODS.len(), |i| day_period_text(locale, *narrow, i)).ok_or(at)?;
                pos += len;
            }
            Tok::Era { width } => {
                let (i, len) = match_text(&s[pos..], 2, |i| era_text(locale, *width, i == 0)).ok_or(at)?;
                p.era_bc = i == 0;
                pos += len;
            }
            Tok::LocalizedOffset { .. } => {
                let (off, len) = match_localized_offset(&s[pos..]).ok_or(at)?;
                p.offset = Some(off);
                pos += len;
            }
            Tok::Offset { colon, z_for_zero, minutes } => {
                let (off, len) = match_offset(&s[pos..], *colon, *z_for_zero, *minutes).ok_or(at)?;
                p.offset = Some(off);
                pos += len;
            }
            Tok::AnyZone | Tok::ZoneId => {
                let (z, len) = match_any_zone(&s[pos..]).ok_or(at)?;
                match z {
                    Zone::Offset(o) => p.offset = Some(o),
                    named => p.zone = Some(named),
                }
                pos += len;
            }
            Tok::Optional(inner) => {
                let snapshot = std::mem::take(p);
                let mut trial = Parsed { ..clone_parsed(&snapshot) };
                match match_toks(inner, s, pos, &mut trial, locale) {
                    Ok(end) => {
                        *p = trial;
                        pos = end;
                    }
                    Err(_) => *p = snapshot,
                }
            }
        }
    }
    Ok(pos)
}

fn clone_parsed(p: &Parsed) -> Parsed {
    Parsed {
        year: p.year,
        year_of_era: p.year_of_era,
        week_year: p.week_year,
        month: p.month,
        day: p.day,
        day_of_year: p.day_of_year,
        week: p.week,
        dow: p.dow,
        hour: p.hour,
        clock12: p.clock12,
        hour12: p.hour12,
        pm: p.pm,
        minute: p.minute,
        second: p.second,
        nano: p.nano,
        milli_of_day: p.milli_of_day,
        offset: p.offset,
        zone: p.zone.clone(),
        era_bc: p.era_bc,
        loc_weeks: p.loc_weeks,
    }
}

fn set_field(p: &mut Parsed, f: Field, v: i64) -> Option<()> {
    let u = || u32::try_from(v).ok();
    match f {
        Field::Year => p.year = Some(v),
        // `YEAR_OF_ERA` starts at 1 (signs belong to `uuuu`).
        Field::YearOfEra if v < 1 => return None,
        Field::YearOfEra => p.year_of_era = Some(v),
        Field::WeekYear => p.week_year = Some(v),
        Field::LocWeekYear => {
            p.week_year = Some(v);
            p.loc_weeks = true;
        }
        Field::Month => p.month = Some(u()?),
        Field::Day => p.day = Some(u()?),
        Field::DayOfYear => p.day_of_year = Some(u()?),
        Field::Week => p.week = Some(u()?),
        Field::LocWeek => {
            p.week = Some(u()?);
            p.loc_weeks = true;
        }
        Field::DayOfWeek => p.dow = Some(u()?),
        // Stored as ISO (1 = Monday) like `E`.
        Field::LocDayOfWeek => p.dow = Some(match u()? {
            d @ 1..=7 => if d == 1 { 7 } else { d - 1 },
            _ => return None,
        }),
        Field::LocWeekOfMonth | Field::AlignedWeekOfMonth | Field::ModifiedJulianDay | Field::NanoOfDay => {}
        Field::Hour => p.hour = Some(u()?),
        Field::ClockHour12 => p.clock12 = Some(u()?),
        Field::Hour12 => p.hour12 = Some(u()?),
        Field::ClockHour24 => p.hour = Some(u()? % 24),
        Field::Minute => p.minute = Some(u()?),
        Field::Second => p.second = Some(u()?),
        Field::NanoOfSecond => p.nano = Some(u()?),
        Field::MilliOfDay => p.milli_of_day = Some(v as u64),
        Field::Quarter => {}
    }
    Some(())
}

/// The longest of `n` texts that `s` starts with (java-time's text parser takes the longest).
fn match_text(s: &[u8], n: usize, text: impl Fn(usize) -> String) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for i in 0..n {
        let t = text(i);
        if !t.is_empty() && s.starts_with(t.as_bytes()) && best.is_none_or(|(_, l)| t.len() > l) {
            best = Some((i, t.len()));
        }
    }
    best
}

/// `GMT`, `GMT+1`, `GMT+01:00`, `GMT-5:30`.
fn match_localized_offset(s: &[u8]) -> Option<(i32, usize)> {
    if !s.starts_with(b"GMT") {
        return None;
    }
    let sign = match s.get(3) {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return Some((0, 3)),
    };
    let digits = |from: usize| s[from..].iter().take_while(|b| b.is_ascii_digit()).count();
    let hl = digits(4);
    if hl == 0 || hl > 2 {
        return None;
    }
    let h: i32 = std::str::from_utf8(&s[4..4 + hl]).ok()?.parse().ok()?;
    let mut len = 4 + hl;
    let mut m = 0;
    if s.get(len) == Some(&b':') && digits(len + 1) >= 2 {
        m = std::str::from_utf8(&s[len + 1..len + 3]).ok()?.parse().ok()?;
        len += 3;
    }
    (h <= 18 && m <= 59).then_some((sign * (h * 3600 + m * 60), len))
}

#[allow(dead_code)]
fn match_name(s: &[u8], names: &[&str]) -> Option<(usize, usize)> {
    for (i, n) in names.iter().enumerate() {
        let full = n.as_bytes();
        if s.len() >= full.len() && s[..full.len()].eq_ignore_ascii_case(full) {
            return Some((i, full.len()));
        }
    }
    for (i, n) in names.iter().enumerate() {
        let short = &n.as_bytes()[..3];
        if s.len() >= 3 && s[..3].eq_ignore_ascii_case(short) {
            return Some((i, 3));
        }
    }
    None
}

fn match_offset(s: &[u8], colon: bool, z_for_zero: bool, minutes: OffsetMinutes) -> Option<(i32, usize)> {
    if z_for_zero && s.first() == Some(&b'Z') {
        return Some((0, 1));
    }
    let sign = match s.first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let d = |i: usize| s.get(i).filter(|b| b.is_ascii_digit()).map(|b| (b - b'0') as i32);
    let h = d(1)? * 10 + d(2)?;
    let mut len = 3;
    let mut m = 0;
    if colon {
        if s.get(3) == Some(&b':') {
            m = d(4)? * 10 + d(5)?;
            len = 6;
        } else if minutes == OffsetMinutes::Required {
            return None;
        }
    } else if let (Some(a), Some(b)) = (d(3), d(4)) {
        m = a * 10 + b;
        len = 5;
    } else if minutes == OffsetMinutes::Required {
        return None;
    }
    if h > 18 || m > 59 {
        return None;
    }
    Some((sign * (h * 3600 + m * 60), len))
}

fn match_any_zone(s: &[u8]) -> Option<(Zone, usize)> {
    if s.first() == Some(&b'Z') {
        return Some((Zone::UTC, 1));
    }
    if matches!(s.first(), Some(b'+') | Some(b'-')) {
        // +HH:MM, +HHMM, +HH, +HH:MM:SS
        let n = s[1..].iter().take_while(|b| b.is_ascii_digit() || **b == b':').count();
        let text = std::str::from_utf8(&s[..1 + n]).ok()?;
        return parse_offset(text).map(|o| (Zone::Offset(o), 1 + n));
    }
    // A zone id: letters, digits, '/', '_', '+', '-'.
    let n = s.iter().take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'+' | b'-' | b':')).count();
    if n == 0 {
        return None;
    }
    let text = std::str::from_utf8(&s[..n]).ok()?;
    Zone::parse(text).ok().map(|z| (z, n))
}

/// `ChronoField.checkValidIntValue`'s message.
fn out_of_range(field: &str, range: &str, v: impl std::fmt::Display) -> String {
    format!("Invalid value for {field} (valid values {range}): {v}")
}

/// `LocalDate.of` (and `ofYearDay`) with java-time's messages.
fn local_date(y: i64, month: u32, day: u32) -> Result<NaiveDate, ResolveFail> {
    if !(1..=12).contains(&month) {
        return Err(ResolveFail::date(out_of_range("MonthOfYear", "1 - 12", month), true));
    }
    if !(1..=31).contains(&day) {
        return Err(ResolveFail::date(out_of_range("DayOfMonth", "1 - 28/31", day), true));
    }
    let yi = i32::try_from(y).map_err(|_| ResolveFail::date(out_of_range("Year", "-999999999 - 999999999", y), true))?;
    NaiveDate::from_ymd_opt(yi, month, day).ok_or_else(|| {
        let msg = if month == 2 && day == 29 {
            format!("Invalid date 'February 29' as '{y}' is not a leap year")
        } else {
            format!("Invalid date '{} {day}'", MONTHS[month as usize - 1].to_ascii_uppercase())
        };
        ResolveFail::date(msg, false)
    })
}

fn year_day(y: i64, doy: u32) -> Result<NaiveDate, ResolveFail> {
    if !(1..=366).contains(&doy) {
        return Err(ResolveFail::date(out_of_range("DayOfYear", "1 - 365/366", doy), true));
    }
    let yi = i32::try_from(y).map_err(|_| ResolveFail::date(out_of_range("Year", "-999999999 - 999999999", y), true))?;
    NaiveDate::from_yo_opt(yi, doy).ok_or_else(|| ResolveFail::date(format!("Invalid date 'DayOfYear 366' as '{y}' is not a leap year"), false))
}

/// The date of year/month/day fields under `resolver`.
fn resolve_ymd(y: i64, month: u32, day: u32, resolver: Resolver) -> Result<NaiveDate, ResolveFail> {
    match resolver {
        Resolver::Strict => local_date(y, month, day),
        Resolver::Smart => {
            if (1..=12).contains(&month) && (29..=31).contains(&day) {
                let last = i32::try_from(y).ok().and_then(|yi| last_day_of_month(yi, month)).unwrap_or(31);
                return local_date(y, month, day.min(last));
            }
            local_date(y, month, day)
        }
        Resolver::Lenient => {
            // `LocalDate.of(y, 1, 1).plusMonths(month - 1).plusDays(day - 1)`.
            let fail = || ResolveFail::date(out_of_range("Year", "-999999999 - 999999999", y), true);
            let base = NaiveDate::from_ymd_opt(i32::try_from(y).map_err(|_| fail())?, 1, 1).ok_or_else(fail)?;
            let months = month as i64 - 1;
            let shifted = if months >= 0 {
                base.checked_add_months(chrono::Months::new(months as u32))
            } else {
                base.checked_sub_months(chrono::Months::new((-months) as u32))
            };
            shifted.and_then(|d| d.checked_add_signed(Duration::days(day as i64 - 1))).ok_or_else(fail)
        }
    }
}

/// Turns parsed fields into an instant. Missing fields default to their minimum (or maximum
/// with `round_up`); the year defaults to 1970 as in `DateFormatters.from`.
fn resolve(p: &Parsed, zone: Option<&Zone>, round_up: bool, week_based: bool, resolver: Resolver, defer_date: bool) -> Result<Instant, ResolveFail> {
    let generic = || ResolveFail::date("Unable to obtain a date from the parsed fields".to_string(), false);
    let year = p.year.or(p.year_of_era.map(|y| if p.era_bc { 1 - y } else { y }));
    let date = if week_based || (p.week_year.is_some() && year.is_none()) {
        let wy = p.week_year.or(year).unwrap_or(1970);
        let week = p.week.unwrap_or(if round_up { 52 } else { 1 });
        let dow = p.dow.unwrap_or(if round_up { 7 } else { 1 });
        let weekday = dow.checked_sub(1).and_then(|d| chrono::Weekday::try_from(d as u8).ok()).ok_or_else(generic)?;
        let wyi = i32::try_from(wy).map_err(|_| generic())?;
        if p.loc_weeks {
            let start = loc_week1_start(wyi).ok_or_else(generic)?;
            let date = start.checked_add_signed(Duration::days((week as i64 - 1) * 7 + weekday.num_days_from_sunday() as i64)).ok_or_else(generic)?;
            // Weeks past the year's last (or week 0) don't exist.
            if loc_week(date) != (wyi, week) {
                return Err(generic());
            }
            date
        } else {
            NaiveDate::from_isoywd_opt(wyi, week, weekday).ok_or_else(generic)?
        }
    } else if let Some(doy) = p.day_of_year {
        year_day(year.unwrap_or(1970), doy)?
    } else {
        let y = year.unwrap_or(1970);
        let month = p.month.unwrap_or(if round_up && p.day.is_none() { 12 } else { 1 });
        let day = match p.day {
            Some(d) => d,
            None if round_up => i32::try_from(y).ok().and_then(|yi| last_day_of_month(yi, month)).unwrap_or(1),
            None => 1,
        };
        resolve_ymd(y, month, day, resolver)?
    };
    // A parsed day of week must agree with the date (`Parsed.crossCheck`), unless the date is
    // only built afterwards.
    if let Some(d) = p.dow
        && !week_based
        && !defer_date
        && date.weekday().number_from_monday() != d
    {
        return Err(ResolveFail::date(
            format!("Conflict found: Field DayOfWeek {} differs from DayOfWeek {d} derived from {}", date.weekday().number_from_monday(), date.format("%Y-%m-%d")),
            false,
        ));
    }
    let hour = match (p.hour, p.clock12, p.hour12, p.pm) {
        (Some(h), _, _, _) => {
            if h > 23 {
                return Err(ResolveFail::time(out_of_range("HourOfDay", "0 - 23", h)));
            }
            h
        }
        (None, Some(c), _, pm) => {
            if !(1..=12).contains(&c) {
                return Err(ResolveFail::time(out_of_range("ClockHourOfAmPm", "1 - 12", c)));
            }
            (c % 12) + if pm == Some(true) { 12 } else { 0 }
        }
        (None, None, Some(h), pm) => {
            if h > 11 {
                return Err(ResolveFail::time(out_of_range("HourOfAmPm", "0 - 11", h)));
            }
            h + if pm == Some(true) { 12 } else { 0 }
        }
        _ => {
            if round_up {
                23
            } else {
                0
            }
        }
    };
    let minute = p.minute.unwrap_or(if round_up { 59 } else { 0 });
    if minute > 59 {
        return Err(ResolveFail::time(out_of_range("MinuteOfHour", "0 - 59", minute)));
    }
    let second = p.second.unwrap_or(if round_up { 59 } else { 0 });
    if second > 59 {
        return Err(ResolveFail::time(out_of_range("SecondOfMinute", "0 - 59", second)));
    }
    let nano = p.nano.unwrap_or(if round_up { 999_999_999 } else { 0 });
    let mut time = NaiveTime::from_hms_nano_opt(hour, minute, second, nano).ok_or_else(|| ResolveFail::time(out_of_range("NanoOfSecond", "0 - 999999999", nano)))?;
    if let Some(ms) = p.milli_of_day {
        time = NaiveTime::from_num_seconds_from_midnight_opt((ms / 1000) as u32, ((ms % 1000) * 1_000_000) as u32)
            .ok_or_else(|| ResolveFail::time(out_of_range("MilliOfDay", "0 - 86399999", ms)))?;
    }
    let local = NaiveDateTime::new(date, time);
    let off = match (&p.offset, &p.zone) {
        (Some(o), _) => *o,
        (None, Some(z)) => z.offset_for_local(local),
        (None, None) => zone.map(|z| z.offset_for_local(local)).unwrap_or(0),
    };
    Ok(Instant::from_naive_utc(local - Duration::seconds(off as i64)))
}

fn last_day_of_month(y: i32, m: u32) -> Option<u32> {
    let next = if m == 12 { NaiveDate::from_ymd_opt(y + 1, 1, 1)? } else { NaiveDate::from_ymd_opt(y, m + 1, 1)? };
    Some(next.pred_opt()?.day())
}

/// First day of week 1 of `year` with Sunday-first weeks where week 1 holds January 1st.
fn loc_week1_start(year: i32) -> Option<NaiveDate> {
    let jan1 = NaiveDate::from_ymd_opt(year, 1, 1)?;
    jan1.checked_sub_signed(Duration::days(jan1.weekday().num_days_from_sunday() as i64))
}

/// (week-based year, week) with Sunday-first weeks, week 1 holding January 1st.
fn loc_week(d: NaiveDate) -> (i32, u32) {
    let y = d.year();
    let wy = match loc_week1_start(y + 1) {
        Some(next) if d >= next => y + 1,
        _ => y,
    };
    let start = loc_week1_start(wy).unwrap_or(d);
    (wy, ((d - start).num_days() / 7 + 1) as u32)
}

const MONTHS_SHORT: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Month texts: English full and narrow forms; `Locale.ROOT` only has the short one (and
/// numbers for narrow).
fn month_text(locale: DateLocale, width: Width, _standalone: bool, i: usize) -> String {
    match (locale, width) {
        (_, Width::Short) | (DateLocale::Root, Width::Full) => MONTHS_SHORT[i].to_string(),
        (DateLocale::English, Width::Full) => MONTHS[i].to_string(),
        (DateLocale::English, Width::Narrow) => MONTHS[i][..1].to_string(),
        (DateLocale::Root, Width::Narrow) => (i + 1).to_string(),
    }
}

fn day_text(locale: DateLocale, width: Width, _standalone: bool, i: usize) -> String {
    match (locale, width) {
        (DateLocale::English, Width::Full) => DAYS[i].to_string(),
        (_, Width::Narrow) => DAYS[i][..1].to_string(),
        _ => DAYS[i][..3].to_string(),
    }
}

fn quarter_text(locale: DateLocale, width: Width, standalone: bool, q: u32) -> String {
    const ORDINAL: [&str; 4] = ["1st", "2nd", "3rd", "4th"];
    match (locale, width, standalone) {
        (_, Width::Narrow, _) => q.to_string(),
        (DateLocale::English, Width::Full, _) => format!("{} quarter", ORDINAL[q as usize - 1]),
        // Root's stand-alone abbreviation is the number.
        (DateLocale::Root, Width::Short, true) => q.to_string(),
        _ => format!("Q{q}"),
    }
}

/// `bc`: before the common era.
fn era_text(locale: DateLocale, width: Width, bc: bool) -> String {
    match (locale, width) {
        (DateLocale::Root, _) => if bc { "BCE" } else { "CE" }.to_string(),
        (DateLocale::English, Width::Full) => if bc { "Before Christ" } else { "Anno Domini" }.to_string(),
        (DateLocale::English, Width::Narrow) => if bc { "B" } else { "A" }.to_string(),
        (DateLocale::English, Width::Short) => if bc { "BC" } else { "AD" }.to_string(),
    }
}

/// English day periods (`B`), as java-time resolves CLDR's rules: midnight and noon cover
/// their whole minute.
const DAY_PERIODS: [&str; 6] = ["midnight", "noon", "in the morning", "in the afternoon", "in the evening", "at night"];

fn day_period_index(minute_of_day: u32) -> usize {
    match minute_of_day {
        0 => 0,
        720 => 1,
        1..=719 => 2,
        721..=1079 => 3,
        1080..=1259 => 4,
        _ => 5,
    }
}

fn day_period_text(locale: DateLocale, narrow: bool, i: usize) -> String {
    match locale {
        // Root only knows AM and PM.
        DateLocale::Root => ["AM", "PM", "AM", "PM", "PM", "PM"][i].to_string(),
        DateLocale::English if narrow && i == 0 => "mi".to_string(),
        DateLocale::English if narrow && i == 1 => "n".to_string(),
        DateLocale::English => DAY_PERIODS[i].to_string(),
    }
}

fn print_toks(toks: &[Tok], local: NaiveDateTime, off: i32, zone: &Zone, locale: DateLocale, out: &mut String) {
    for t in toks {
        match t {
            Tok::Lit(l) => out.push_str(l),
            Tok::Num { field, min, signed, .. } => {
                let v: i64 = match field {
                    Field::Year => local.year() as i64,
                    Field::YearOfEra => {
                        let y = local.year() as i64;
                        if y <= 0 { 1 - y } else { y }
                    }
                    Field::WeekYear => local.iso_week().year() as i64,
                    Field::LocWeekYear => loc_week(local.date()).0 as i64,
                    Field::Month => local.month() as i64,
                    Field::Day => local.day() as i64,
                    Field::DayOfYear => local.ordinal() as i64,
                    Field::Week => local.iso_week().week() as i64,
                    Field::LocWeek => loc_week(local.date()).1 as i64,
                    Field::LocWeekOfMonth => {
                        let first = local.date().with_day(1).unwrap_or(local.date());
                        ((local.day() - 1 + first.weekday().num_days_from_sunday()) / 7 + 1) as i64
                    }
                    Field::AlignedWeekOfMonth => ((local.day() - 1) / 7 + 1) as i64,
                    Field::DayOfWeek => local.weekday().number_from_monday() as i64,
                    Field::LocDayOfWeek => local.weekday().number_from_sunday() as i64,
                    Field::ModifiedJulianDay => {
                        // Days since 1858-11-17.
                        local.date().num_days_from_ce() as i64 - 678_576
                    }
                    Field::Hour => local.hour() as i64,
                    Field::ClockHour12 => {
                        let h = local.hour() % 12;
                        if h == 0 { 12 } else { h as i64 }
                    }
                    Field::Hour12 => (local.hour() % 12) as i64,
                    Field::ClockHour24 => {
                        if local.hour() == 0 {
                            24
                        } else {
                            local.hour() as i64
                        }
                    }
                    Field::Minute => local.minute() as i64,
                    Field::Second => local.second() as i64,
                    Field::NanoOfSecond => local.nanosecond() as i64,
                    Field::MilliOfDay => (local.num_seconds_from_midnight() as i64) * 1000 + (local.nanosecond() / 1_000_000) as i64,
                    Field::NanoOfDay => (local.num_seconds_from_midnight() as i64) * 1_000_000_000 + local.nanosecond() as i64,
                    Field::Quarter => ((local.month() - 1) / 3 + 1) as i64,
                };
                let digits = v.unsigned_abs().to_string();
                if v < 0 {
                    out.push('-');
                } else if *signed && *min >= 4 && digits.len() > *min as usize {
                    out.push('+');
                }
                for _ in digits.len()..*min as usize {
                    out.push('0');
                }
                out.push_str(&digits);
            }
            Tok::ReducedYear => out.push_str(&format!("{:02}", local.year().rem_euclid(100))),
            Tok::Fraction { min, max, point } => {
                let nanos = format!("{:09}", local.nanosecond());
                let mut f = nanos[..*max as usize].to_string();
                while f.len() > *min as usize && f.ends_with('0') {
                    f.pop();
                }
                if *point {
                    out.push('.');
                }
                out.push_str(&f);
            }
            Tok::MonthText { width, standalone } => out.push_str(&month_text(locale, *width, *standalone, local.month0() as usize)),
            Tok::DayText { width, standalone } => out.push_str(&day_text(locale, *width, *standalone, local.weekday().num_days_from_monday() as usize)),
            Tok::QuarterText { width, standalone } => out.push_str(&quarter_text(locale, *width, *standalone, (local.month() - 1) / 3 + 1)),
            Tok::AmPm => out.push_str(if local.hour() < 12 { "AM" } else { "PM" }),
            Tok::DayPeriod { narrow } => {
                let i = day_period_index(local.hour() * 60 + local.minute());
                out.push_str(&day_period_text(locale, *narrow, i));
            }
            Tok::Era { width } => out.push_str(&era_text(locale, *width, local.year() <= 0)),
            Tok::LocalizedOffset { full } => {
                out.push_str("GMT");
                if off != 0 {
                    let a = off.abs();
                    let (h, m, sec) = (a / 3600, (a % 3600) / 60, a % 60);
                    out.push(if off < 0 { '-' } else { '+' });
                    if *full {
                        out.push_str(&format!("{h:02}:{m:02}"));
                    } else {
                        out.push_str(&h.to_string());
                        if m != 0 || sec != 0 {
                            out.push_str(&format!(":{m:02}"));
                        }
                    }
                    if sec != 0 {
                        out.push_str(&format!(":{sec:02}"));
                    }
                }
            }
            Tok::Offset { colon, z_for_zero, minutes } => print_offset(out, off, *colon, *z_for_zero, *minutes),
            Tok::AnyZone => print_offset(out, off, true, true, OffsetMinutes::Required),
            Tok::ZoneId => match zone {
                Zone::Named(tz) => out.push_str(tz.name()),
                Zone::Offset(0) => out.push('Z'),
                Zone::Offset(o) => print_offset(out, *o, true, true, OffsetMinutes::Required),
            },
            Tok::Optional(inner) => print_toks(inner, local, off, zone, locale, out),
        }
    }
}

fn print_offset(out: &mut String, off: i32, colon: bool, z_for_zero: bool, minutes: OffsetMinutes) {
    if off == 0 && z_for_zero {
        out.push('Z');
        return;
    }
    let sign = if off < 0 { '-' } else { '+' };
    let a = off.abs();
    let (h, m) = (a / 3600, (a % 3600) / 60);
    out.push(sign);
    out.push_str(&format!("{h:02}"));
    if minutes == OffsetMinutes::Optional && m == 0 {
        return;
    }
    if colon {
        out.push(':');
    }
    out.push_str(&format!("{m:02}"));
}

/// Date math errors carry Elasticsearch's `parse_exception` texts, and the date's own error
/// when the anchor didn't parse.
#[derive(Debug, Clone, PartialEq)]
pub struct DateMathError(pub String, pub Option<DateError>);

impl DateMathError {
    fn msg(m: impl Into<String>) -> DateMathError {
        DateMathError(m.into(), None)
    }

    /// The `parse_exception` with its causes.
    pub fn to_es(&self) -> crate::error::EsError {
        let e = crate::error::EsError::parse(self.0.clone());
        match &self.1 {
            Some(cause) => e.caused_by(cause.to_es()),
            None => e,
        }
    }
}

/// `JavaDateMathParser.parse`: `now…`, `<date>||<math>` or a plain date.
pub fn parse_date_math(text: &str, now_millis: i64, round_up: bool, zone: Option<&Zone>, fmt: &DateFormatter) -> Result<Instant, DateMathError> {
    let (time, math) = if let Some(rest) = text.strip_prefix("now") {
        (Instant::from_millis(now_millis), rest)
    } else {
        match text.find("||") {
            None => return parse_date_time(text, zone, round_up, fmt),
            Some(i) => (parse_date_time(&text[..i], zone, false, fmt)?, &text[i + 2..]),
        }
    };
    apply_math(math, time, round_up, zone)
}

fn parse_date_time(text: &str, zone: Option<&Zone>, round_up: bool, fmt: &DateFormatter) -> Result<Instant, DateMathError> {
    if text.is_empty() {
        return Err(DateMathError::msg("cannot parse empty datetime"));
    }
    // `JavaDateMathParser.parseDateTime` wraps whatever the formatter threw.
    fmt.parse_detailed(text, zone, round_up)
        .map_err(|e| DateMathError(format!("failed to parse date field [{text}] with format [{}]: [{}]", fmt.pattern, e.reason()), Some(e)))
}

fn apply_math(math: &str, time: Instant, round_up: bool, zone: Option<&Zone>) -> Result<Instant, DateMathError> {
    let zone = zone.cloned().unwrap_or(Zone::UTC);
    let utc = time.to_naive().ok_or_else(|| DateMathError::msg("date out of range"))?;
    let mut local = utc + Duration::seconds(zone.offset_at_utc(utc) as i64);
    let b = math.as_bytes();
    let mut i = 0;
    let err = |m: &str| DateMathError::msg(m);
    while i < b.len() {
        let c = b[i];
        i += 1;
        let (round, sign) = match c {
            b'/' => (true, 1i64),
            b'+' => (false, 1),
            b'-' => (false, -1),
            _ => return Err(err(&format!("operator not supported for date math [{math}]"))),
        };
        if i >= b.len() {
            return Err(err(&format!("truncated date math [{math}]")));
        }
        let num: i64 = if !b[i].is_ascii_digit() {
            1
        } else {
            let from = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i >= b.len() {
                return Err(err(&format!("truncated date math [{math}]")));
            }
            math[from..i].parse().map_err(|_| err(&format!("truncated date math [{math}]")))?
        };
        if round && num != 1 {
            return Err(err(&format!("rounding `/` can only be used on single unit types [{math}]")));
        }
        let unit = b[i] as char;
        i += 1;
        let midnight = |d: NaiveDate| d.and_hms_opt(0, 0, 0).unwrap();
        local = match unit {
            'y' => {
                if round {
                    let s = midnight(NaiveDate::from_yo_opt(local.year(), 1).ok_or_else(|| err("date out of range"))?);
                    if round_up { add_months(s, 12) } else { s }
                } else {
                    add_months(local, sign * num * 12)
                }
            }
            'M' => {
                if round {
                    let s = midnight(local.date().with_day(1).ok_or_else(|| err("date out of range"))?);
                    if round_up { add_months(s, 1) } else { s }
                } else {
                    add_months(local, sign * num)
                }
            }
            'w' => {
                if round {
                    let back = local.weekday().num_days_from_monday() as i64;
                    let s = midnight(local.date()) - Duration::days(back);
                    if round_up { s + Duration::weeks(1) } else { s }
                } else {
                    local + Duration::weeks(sign * num)
                }
            }
            'd' => {
                if round {
                    let s = midnight(local.date());
                    if round_up { s + Duration::days(1) } else { s }
                } else {
                    local + Duration::days(sign * num)
                }
            }
            'h' | 'H' => {
                if round {
                    let s = local.with_minute(0).and_then(|t| t.with_second(0)).and_then(|t| t.with_nanosecond(0)).unwrap();
                    if round_up { s + Duration::hours(1) } else { s }
                } else {
                    local + Duration::hours(sign * num)
                }
            }
            'm' => {
                if round {
                    let s = local.with_second(0).and_then(|t| t.with_nanosecond(0)).unwrap();
                    if round_up { s + Duration::minutes(1) } else { s }
                } else {
                    local + Duration::minutes(sign * num)
                }
            }
            's' => {
                if round {
                    let s = local.with_nanosecond(0).unwrap();
                    if round_up { s + Duration::seconds(1) } else { s }
                } else {
                    local + Duration::seconds(sign * num)
                }
            }
            other => return Err(err(&format!("unit [{other}] not supported for date math [{math}]"))),
        };
        if round && round_up {
            local -= Duration::milliseconds(1);
        }
    }
    let off = zone.offset_for_local(local);
    Ok(Instant::from_naive_utc(local - Duration::seconds(off as i64)))
}

fn add_months(t: NaiveDateTime, months: i64) -> NaiveDateTime {
    let total = t.year() as i64 * 12 + t.month0() as i64 + months;
    let y = total.div_euclid(12) as i32;
    let m = total.rem_euclid(12) as u32 + 1;
    let last = last_day_of_month(y, m).unwrap_or(28);
    let d = t.day().min(last);
    NaiveDate::from_ymd_opt(y, m, d).map(|nd| NaiveDateTime::new(nd, t.time())).unwrap_or(t)
}

pub fn now_millis() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(f: &str, s: &str) -> i64 {
        DateFormatter::new(f).unwrap().parse_millis(s).unwrap_or_else(|e| panic!("{f} {s}: {e}"))
    }

    #[test]
    fn pattern_letters_print_like_elasticsearch() {
    // Captured from Elasticsearch 9.5.5: docvalue_fields with these formats on a date_nanos field
    // (default locale: English; `locale: root` for the second table).
    const INSTANTS: [&str; 5] = ["2026-10-21T14:32:48.492123456Z", "2026-01-01T00:00:00Z", "2026-03-01T23:05:00Z", "2027-01-03T12:00:00Z", "2026-10-25T01:02:03.004Z"];
    const ENGLISH: &[(&str, [&str; 5])] = &[
        ("QQQ", ["Q4", "Q1", "Q1", "Q1", "Q4"]),
        ("QQQQ", ["4th quarter", "1st quarter", "1st quarter", "1st quarter", "4th quarter"]),
        ("QQQQQ", ["4", "1", "1", "1", "4"]),
        ("Q", ["4", "1", "1", "1", "4"]),
        ("QQ", ["04", "01", "01", "01", "04"]),
        ("qqq", ["Q4", "Q1", "Q1", "Q1", "Q4"]),
        ("qqqq", ["4th quarter", "1st quarter", "1st quarter", "1st quarter", "4th quarter"]),
        ("qqqqq", ["4", "1", "1", "1", "4"]),
        ("q", ["4", "1", "1", "1", "4"]),
        ("qq", ["04", "01", "01", "01", "04"]),
        ("N", ["52368492123456", "0", "83100000000000", "43200000000000", "3723004000000"]),
        ("NN", ["52368492123456", "00", "83100000000000", "43200000000000", "3723004000000"]),
        ("A", ["52368492", "0", "83100000", "43200000", "3723004"]),
        ("n", ["492123456", "0", "0", "0", "4000000"]),
        ("W", ["4", "1", "1", "2", "5"]),
        ("F", ["3", "1", "1", "1", "4"]),
        ("w", ["43", "1", "10", "2", "44"]),
        ("ww", ["43", "01", "10", "02", "44"]),
        ("e", ["4", "5", "1", "1", "1"]),
        ("ee", ["04", "05", "01", "01", "01"]),
        ("eee", ["Wed", "Thu", "Sun", "Sun", "Sun"]),
        ("eeee", ["Wednesday", "Thursday", "Sunday", "Sunday", "Sunday"]),
        ("eeeee", ["W", "T", "S", "S", "S"]),
        ("c", ["4", "5", "1", "1", "1"]),
        ("ccc", ["Wed", "Thu", "Sun", "Sun", "Sun"]),
        ("cccc", ["Wednesday", "Thursday", "Sunday", "Sunday", "Sunday"]),
        ("ccccc", ["W", "T", "S", "S", "S"]),
        ("g", ["61334", "61041", "61100", "61408", "61338"]),
        ("ggg", ["61334", "61041", "61100", "61408", "61338"]),
        ("O", ["GMT", "GMT", "GMT", "GMT", "GMT"]),
        ("OOOO", ["GMT", "GMT", "GMT", "GMT", "GMT"]),
        ("MMMMM", ["O", "J", "M", "J", "O"]),
        ("LLL", ["Oct", "Jan", "Mar", "Jan", "Oct"]),
        ("LLLL", ["October", "January", "March", "January", "October"]),
        ("LLLLL", ["O", "J", "M", "J", "O"]),
        ("L", ["10", "1", "3", "1", "10"]),
        ("G", ["AD", "AD", "AD", "AD", "AD"]),
        ("GG", ["AD", "AD", "AD", "AD", "AD"]),
        ("GGG", ["AD", "AD", "AD", "AD", "AD"]),
        ("GGGG", ["Anno Domini", "Anno Domini", "Anno Domini", "Anno Domini", "Anno Domini"]),
        ("GGGGG", ["A", "A", "A", "A", "A"]),
        ("B", ["in the afternoon", "midnight", "at night", "noon", "in the morning"]),
        ("BBBB", ["in the afternoon", "midnight", "at night", "noon", "in the morning"]),
        ("BBBBB", ["in the afternoon", "mi", "at night", "n", "in the morning"]),
        ("E", ["Wed", "Thu", "Sun", "Sun", "Sun"]),
        ("EEEE", ["Wednesday", "Thursday", "Sunday", "Sunday", "Sunday"]),
        ("EEEEE", ["W", "T", "S", "S", "S"]),
        ("a", ["PM", "AM", "PM", "PM", "AM"]),
        ("h", ["2", "12", "11", "12", "1"]),
        ("K", ["2", "0", "11", "0", "1"]),
        ("k", ["14", "24", "23", "12", "1"]),
        ("Y", ["2026", "2026", "2026", "2027", "2026"]),
        ("YYYY", ["2026", "2026", "2026", "2027", "2026"]),
        ("YY", ["26", "26", "26", "27", "26"]),
        ("u", ["2026", "2026", "2026", "2027", "2026"]),
        ("uu", ["26", "26", "26", "27", "26"]),
        ("D", ["294", "1", "60", "3", "298"]),
        ("DDD", ["294", "001", "060", "003", "298"]),
        ("VV", ["Z", "Z", "Z", "Z", "Z"]),
        ("z", ["Z", "Z", "Z", "Z", "Z"]),
        ("zzzz", ["Z", "Z", "Z", "Z", "Z"]),
        ("v", ["Z", "Z", "Z", "Z", "Z"]),
        ("vvvv", ["Z", "Z", "Z", "Z", "Z"]),
        ("x", ["+00", "+00", "+00", "+00", "+00"]),
        ("xx", ["+0000", "+0000", "+0000", "+0000", "+0000"]),
        ("xxx", ["+00:00", "+00:00", "+00:00", "+00:00", "+00:00"]),
        ("X", ["Z", "Z", "Z", "Z", "Z"]),
        ("Z", ["+0000", "+0000", "+0000", "+0000", "+0000"]),
        ("ZZZZ", ["GMT", "GMT", "GMT", "GMT", "GMT"]),
        ("ZZZZZ", ["Z", "Z", "Z", "Z", "Z"]),
        ("y", ["2026", "2026", "2026", "2027", "2026"]),
        ("yy", ["26", "26", "26", "27", "26"]),
        ("yyy", ["2026", "2026", "2026", "2027", "2026"]),
        ("yyyyy", ["02026", "02026", "02026", "02027", "02026"]),
        ("S", ["4", "0", "0", "0", "0"]),
        ("SS", ["49", "00", "00", "00", "00"]),
        ("SSSSSSSSS", ["492123456", "000000000", "000000000", "000000000", "004000000"]),
        ("H", ["14", "0", "23", "12", "1"]),
        ("m", ["32", "0", "5", "0", "2"]),
        ("s", ["48", "0", "0", "0", "3"]),
        ("d", ["21", "1", "1", "3", "25"]),
        ("M", ["10", "1", "3", "1", "10"]),
        ("MMM", ["Oct", "Jan", "Mar", "Jan", "Oct"]),
        ("MMMM", ["October", "January", "March", "January", "October"]),
    ];
    const ROOT: &[(&str, [&str; 5])] = &[
        ("G", ["CE", "CE", "CE", "CE", "CE"]),
        ("GGGG", ["CE", "CE", "CE", "CE", "CE"]),
        ("e", ["4", "5", "1", "1", "1"]),
        ("c", ["4", "5", "1", "1", "1"]),
        ("eee", ["Wed", "Thu", "Sun", "Sun", "Sun"]),
        ("W", ["4", "1", "1", "2", "5"]),
        ("w", ["43", "1", "10", "2", "44"]),
        ("Y", ["2026", "2026", "2026", "2027", "2026"]),
        ("YY", ["26", "26", "26", "27", "26"]),
        ("QQQ", ["Q4", "Q1", "Q1", "Q1", "Q4"]),
        ("QQQQ", ["Q4", "Q1", "Q1", "Q1", "Q4"]),
        ("qqq", ["4", "1", "1", "1", "4"]),
        ("B", ["PM", "AM", "PM", "PM", "AM"]),
        ("a", ["PM", "AM", "PM", "PM", "AM"]),
        ("E", ["Wed", "Thu", "Sun", "Sun", "Sun"]),
        ("EEEE", ["Wed", "Thu", "Sun", "Sun", "Sun"]),
        ("MMM", ["Oct", "Jan", "Mar", "Jan", "Oct"]),
        ("MMMM", ["Oct", "Jan", "Mar", "Jan", "Oct"]),
        ("ZZZZ", ["GMT", "GMT", "GMT", "GMT", "GMT"]),
        ("O", ["GMT", "GMT", "GMT", "GMT", "GMT"]),
        ("OOOO", ["GMT", "GMT", "GMT", "GMT", "GMT"]),
        ("LLL", ["Oct", "Jan", "Mar", "Jan", "Oct"]),
    ];

        let iso = DateFormatter::new("strict_date_optional_time_nanos").unwrap();
        let instants: Vec<Instant> = INSTANTS.iter().map(|t| iso.parse(t, None, false).unwrap()).collect();
        for (locale, table) in [(DateLocale::English, ENGLISH), (DateLocale::Root, ROOT)] {
            for (pattern, want) in table {
                let f = DateFormatter::new(pattern).unwrap_or_else(|e| panic!("{pattern}: {e}")).with_locale(locale);
                let got: Vec<String> = instants.iter().map(|i| f.format(*i, &Zone::UTC)).collect();
                assert_eq!(got, want.to_vec(), "{locale:?} {pattern}");
            }
        }
    }

    #[test]
    fn day_periods() {
        // `B` through the day on 2026-10-21, from Elasticsearch 9.5.5.
        let hours = ["00:00:00", "00:00:01", "01:00:00", "03:00:00", "05:59:00", "06:00:00", "09:00:00", "11:59:00", "12:00:00", "12:00:01", "13:00:00", "17:59:00", "18:00:00", "20:59:00", "21:00:00", "23:59:00"];
        let en = [
            "midnight", "midnight", "in the morning", "in the morning", "in the morning", "in the morning", "in the morning", "in the morning", "noon", "noon",
            "in the afternoon", "in the afternoon", "in the evening", "in the evening", "at night", "at night",
        ];
        let iso = DateFormatter::new(DateFormatter::DEFAULT).unwrap();
        for (i, h) in hours.iter().enumerate() {
            let t = iso.parse(&format!("2026-10-21T{h}Z"), None, false).unwrap();
            let f = DateFormatter::new("B").unwrap();
            assert_eq!(f.clone().with_locale(DateLocale::English).format(t, &Zone::UTC), en[i], "{h}");
            assert_eq!(f.with_locale(DateLocale::Root).format(t, &Zone::UTC), if i < 8 { "AM" } else { "PM" }, "{h}");
            let narrow = DateFormatter::new("BBBBB").unwrap().with_locale(DateLocale::English).format(t, &Zone::UTC);
            assert_eq!(narrow, match en[i] { "midnight" => "mi", "noon" => "n", other => other });
        }
    }

    #[test]
    fn texts_parse_case_sensitively() {
        let f = DateFormatter::new("MMM dd yyyy").unwrap().with_locale(DateLocale::English);
        assert!(f.parse("Jan 01 2020", None, false).is_ok());
        assert!(f.parse("jan 01 2020", None, false).is_err());
        assert!(f.parse("JAN 01 2020", None, false).is_err());
        let f = DateFormatter::new("yyyy-MM-dd hh a").unwrap();
        assert!(f.parse("2026-10-21 02 PM", None, false).is_ok());
        assert!(f.parse("2026-10-21 02 pm", None, false).is_err());
        let f = DateFormatter::new("MMMM d yyyy").unwrap().with_locale(DateLocale::English);
        assert_eq!(f.parse("October 21 2026", None, false).unwrap().millis(), 1_792_540_800_000);
        // Sunday-first weeks when parsing `Y`/`w`/`e` too.
        let f = DateFormatter::new("YYYY-ww-e").unwrap();
        let t = f.parse("2026-10-01", None, false).unwrap();
        assert_eq!(DateFormatter::new("yyyy-MM-dd").unwrap().format(t, &Zone::UTC), "2026-03-01");
    }

    #[test]
    fn parse_errors_match_elasticsearch() {
        // (format, text, top-level reason or `None` when the error is wrapped, the date-time
        // parse exception's reason). Captured from Elasticsearch 9.5.5 indexing into a date field.
        let p = |f: &str, t: &str| DateFormatter::new(f).unwrap().parse_detailed(t, None, false);
        let wrapped = |f: &str, t: &str| match p(f, t).unwrap_err().kind {
            DateErrorKind::Parse { reason, cause } => (reason, cause),
            other => panic!("{f} {t}: {other:?}"),
        };
        let raw = |f: &str, t: &str| match p(f, t).unwrap_err().kind {
            DateErrorKind::Invalid(msg) => msg,
            other => panic!("{f} {t}: {other:?}"),
        };
        assert_eq!(wrapped("date", "2020-1-1x").0, "Text '2020-1-1x' could not be parsed, unparsed text found at index 8");
        assert_eq!(wrapped("basic_date", "2026-02-01").0, "Text '2026-02-01' could not be parsed at index 4");
        assert_eq!(wrapped("epoch_millis", "abc").0, "Failed to parse with all enclosed parsers");
        assert_eq!(wrapped("strict_date_optional_time", "2020-13-01").0, "Could not fully parse datetime");
        assert_eq!(wrapped("strict_date_optional_time", "2020-12-01T10:00:00Zjunk").0, "Could not fully parse datetime");
        assert_eq!(raw("strict_date_optional_time", "2020-02-30"), "Invalid date 'FEBRUARY 30'");
        assert_eq!(raw(DateFormatter::DEFAULT, "2020-02-30"), "Invalid date 'FEBRUARY 30'");
        assert_eq!(wrapped(DateFormatter::DEFAULT, "2020-13-01").0, "Failed to parse with all enclosed parsers");
        assert_eq!(raw("yyyy-MM-dd", "2020-13-01"), "Invalid value for MonthOfYear (valid values 1 - 12): 13");
        assert_eq!(raw("yyyy-MM-dd", "2020-12-32"), "Invalid value for DayOfMonth (valid values 1 - 28/31): 32");
        assert_eq!(raw("yyyy-MM-dd", "2021-02-29"), "Invalid date 'February 29' as '2021' is not a leap year");
        assert_eq!(raw("MM-dd", "02-30"), "Invalid date 'FEBRUARY 30'");
        assert_eq!(raw("yyyy-DDD", "2021-400"), "Invalid value for DayOfYear (valid values 1 - 365/366): 400");
        let (reason, cause) = wrapped("uuuu-MM-dd", "2020-13-01");
        assert_eq!(reason, "Text '2020-13-01' could not be parsed: Invalid value for MonthOfYear (valid values 1 - 12): 13");
        assert_eq!(cause.as_deref(), Some("Invalid value for MonthOfYear (valid values 1 - 12): 13"));
        assert_eq!(wrapped("date", "2020-02-30").0, "Text '2020-02-30' could not be parsed: Invalid date 'FEBRUARY 30'");
        assert_eq!(
            wrapped("basic_ordinal_date", "2021366").0,
            "Text '2021366' could not be parsed: Invalid date 'DayOfYear 366' as '2021' is not a leap year"
        );
        assert_eq!(
            wrapped("yyyy-MM-dd HH:mm", "2020-12-01 10:60").0,
            "Text '2020-12-01 10:60' could not be parsed: Invalid value for MinuteOfHour (valid values 0 - 59): 60"
        );
        assert_eq!(
            wrapped("uuuu-MM-dd EEE", "2026-10-21 Thu").0,
            "Text '2026-10-21 Thu' could not be parsed: Conflict found: Field DayOfWeek 3 differs from DayOfWeek 4 derived from 2026-10-21"
        );
        // A year-of-era pattern builds the date afterwards, without the day-of-week check.
        assert!(p("yyyy-MM-dd EEE", "2026-10-21 Thu").is_ok());
        // `strict_date` resolves leniently, `strict_date_hour` smartly (clamping the day).
        let iso = |f: &str, t: &str| java_instant_string(p(f, t).unwrap().millis());
        assert_eq!(iso("strict_date", "2020-02-30"), "2020-03-01T00:00:00Z");
        assert_eq!(iso("strict_date", "2020-13-40"), "2021-02-09T00:00:00Z");
        assert_eq!(iso("strict_date", "2020-00-00"), "2019-11-30T00:00:00Z");
        assert_eq!(iso("strict_date_hour", "2020-02-30T10"), "2020-02-29T10:00:00Z");
        assert_eq!(wrapped("strict_date_hour", "2020-02-32T10").0, "Text '2020-02-32T10' could not be parsed: Invalid value for DayOfMonth (valid values 1 - 28/31): 32");
        assert_eq!(p("strict_date", "").unwrap_err().reason(), "cannot parse empty datetime");
        // Long texts are cut in the message.
        let long = format!("x{}", "1".repeat(70));
        assert_eq!(wrapped("yyyy", &long).0, format!("Text '{}...' could not be parsed at index 0", &long[..64]));
        // Nanosecond instants print like `Instant.toString()`.
        assert_eq!(java_instant_string_nanos(Instant { secs: 0, nanos: 4_000_000 }), "1970-01-01T00:00:00.004Z");
        assert_eq!(java_instant_string_nanos(Instant { secs: 0, nanos: 4_500 }), "1970-01-01T00:00:00.000004500Z");
        assert_eq!(to_epoch_nanos(Instant { secs: -1, nanos: 0 }).unwrap_err(), "date[1969-12-31T23:59:59Z] is before the epoch in 1970 and cannot be stored in nanosecond resolution");
    }

    #[test]
    fn empty_elements_in_combined_formats() {
        for p in ["strict_date||epoch_millis||", "||strict_date", "strict_date||||epoch_millis"] {
            assert_eq!(DateFormatter::new(p).unwrap_err(), format!("Cannot have empty element in multi date format pattern: {p}"));
        }
    }

    #[test]
    fn instant_strings() {
        assert_eq!(java_instant_string(1632625792000), "2021-09-26T03:09:52Z");
        assert_eq!(java_instant_string(1632625792120), "2021-09-26T03:09:52.120Z");
        assert_eq!(java_instant_string(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn named_formats() {
        let d = DateFormatter::DEFAULT;
        assert_eq!(ms(d, "2020-01-01"), 1577836800000);
        assert_eq!(ms(d, "2020-01-01T10:20:30.123Z"), 1577874030123);
        assert_eq!(ms(d, "2020-01-01T10:20:30+01:00"), 1577870430000);
        assert_eq!(ms(d, "2020"), 1577836800000);
        assert_eq!(ms(d, "2020-02"), 1580515200000);
        assert_eq!(ms(d, "1577836800000"), 1577836800000);
        // Elasticsearch's ISO parser takes a signed four-digit year before epoch millis.
        let year = |y: i32| NaiveDate::from_ymd_opt(y, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis();
        assert_eq!(ms(d, "-1000"), year(-1000));
        assert_eq!(ms(d, "-10000"), -10000);
        assert_eq!(ms(d, "-9999-01-01T00:00:00Z"), year(-9999));
        assert!(DateFormatter::new(d).unwrap().parse_millis("+9999-01-01").is_err());
        assert!(DateFormatter::new(d).unwrap().parse_millis("10000-01-01").is_err());
        assert_eq!(ms("date_optional_time", "-10000"), year(-10000));
        assert_eq!(ms("strict_date", "-12345-01-01"), NaiveDate::from_ymd_opt(-12345, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis());
        assert!(DateFormatter::new("strict_date").unwrap().parse_millis("+9999-01-01").is_err());
        assert!(DateFormatter::new("yyyy-MM-dd").unwrap().parse_millis("-9999-01-01").is_err());
        assert_eq!(DateFormatter::new(d).unwrap().format_millis(year(-1)), "-0001-01-01T00:00:00.000Z");
        assert!(DateFormatter::new(d).unwrap().parse_millis("notadate").is_err());
        assert!(DateFormatter::new(d).unwrap().parse_millis("2020-1-1").is_err());
        assert_eq!(ms("date_optional_time", "2020-1-1"), 1577836800000);
        assert!(DateFormatter::new(d).unwrap().parse_millis("2020-02-30").is_err());
        assert_eq!(ms("epoch_second", "1577836800"), 1577836800000);
        assert_eq!(ms("basic_date", "20200101"), 1577836800000);
        assert_eq!(ms("strict_date_hour_minute_second", "2020-01-01T00:00:01"), 1577836801000);
        assert_eq!(ms("strict_week_date", "2020-W01-3"), 1577836800000);
    }

    #[test]
    fn patterns() {
        assert_eq!(ms("yyyy-MM-dd HH:mm:ss", "2020-01-01 10:00:00"), 1577872800000);
        assert_eq!(ms("dd/MMM/yyyy", "01/Jan/2020"), 1577836800000);
        assert_eq!(ms("yyyyMMdd", "20200101"), 1577836800000);
        assert_eq!(ms("yyyy-MM-dd'T'HH:mm:ss.SSSZ", "2020-01-01T00:00:00.000+0000"), 1577836800000);
        let f = DateFormatter::new("yyyy-MM-dd").unwrap();
        assert_eq!(f.format_millis(1577836800000), "2020-01-01");
        let f = DateFormatter::new(DateFormatter::DEFAULT).unwrap();
        assert_eq!(f.format_millis(1577874030123), "2020-01-01T10:20:30.123Z");
    }

    #[test]
    fn pattern_errors() {
        assert_eq!(DateFormatter::new("nope_format").unwrap_err(), "Invalid format: [nope_format]: Unknown pattern letter: o");
        assert_eq!(DateFormatter::new("ddd").unwrap_err(), "Invalid format: [ddd]: Too many pattern letters: d");
        assert!(DateFormatter::new("yyyy-MM-dd||epoch_second").is_ok());
        assert!(DateFormatter::new("strict_date_optional_time_nanos").is_ok());
        assert!(DateFormatter::new("date_optional_time_nanos").is_err());
    }

    #[test]
    fn math() {
        let f = DateFormatter::new(DateFormatter::DEFAULT).unwrap();
        let now = 1577874030123; // 2020-01-01T10:20:30.123Z
        let m = |e: &str, up: bool| parse_date_math(e, now, up, None, &f).unwrap().millis();
        assert_eq!(m("now", false), now);
        assert_eq!(m("now/d", false), 1577836800000);
        assert_eq!(m("now/d", true), 1577923199999);
        assert_eq!(m("now-1d", false), now - 86_400_000);
        assert_eq!(m("now+1M/M", false), 1580515200000);
        assert_eq!(m("2020-01-31||+1M", false), 1582934400000);
        assert_eq!(m("now/w", false), 1577664000000);
        let e = parse_date_math("now+1x", now, false, None, &f).unwrap_err();
        assert_eq!(e.0, "unit [x] not supported for date math [+1x]");
        let z = Zone::parse("+12:00").unwrap();
        assert_eq!(parse_date_math("now/d", now, false, Some(&z), &f).unwrap().millis(), 1577793600000);
    }

    #[test]
    fn zones() {
        assert_eq!(Zone::parse("+01:00").unwrap(), Zone::Offset(3600));
        assert_eq!(Zone::parse("-0530").unwrap(), Zone::Offset(-19800));
        assert!(matches!(Zone::parse("Europe/Paris").unwrap(), Zone::Named(_)));
        assert!(Zone::parse("Mars/Base").is_err());
    }
}
