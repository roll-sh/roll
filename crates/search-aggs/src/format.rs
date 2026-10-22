//! Value formats (Elasticsearch's `DocValueFormat`) and time zones.
//!
//! A format turns bucket keys and metric values into what responses print (`key_as_string`,
//! `value_as_string`, `from_as_string`) and parses user-supplied keys back (`missing`,
//! range bounds, `include` values of numeric terms, composite `after` keys). Date and decimal
//! formats are Java's (`DateTimeFormatter` patterns with date math, `java.text.DecimalFormat`);
//! `search-es` supplies its exact implementations through [`FieldLookup`](crate::FieldLookup)
//! ([`DateFormatter`], [`NumberFormatter`]); built-in versions cover the common patterns so the
//! crate works standalone.
//!
//! [`TimeZone`] follows `java.time.ZoneId.of`: `Z`, offsets (`+01:00`), `UTC`/`GMT`/`UT` with
//! optional offsets, and tz database regions (via `chrono-tz`), with Java's error texts.

use std::sync::Arc;

use chrono::{Datelike, NaiveDate, NaiveDateTime, Offset, TimeZone as _, Timelike};

use crate::error::{AggError, Result};
use crate::json::{Json, java_double, parse_java_double, parse_java_long};

/// A `java.time.ZoneId`.
#[derive(Clone, Debug, PartialEq)]
pub enum TimeZone {
    /// A fixed offset, with the id Java prints for it (`Z`, `+01:00`, `UTC`, `GMT+02:00`).
    Fixed { offset_secs: i32, id: String },
    /// A tz database region.
    Named(chrono_tz::Tz),
}

impl Default for TimeZone {
    fn default() -> Self {
        TimeZone::utc()
    }
}

impl TimeZone {
    /// `ZoneOffset.UTC` (id `Z`).
    pub fn utc() -> TimeZone {
        TimeZone::Fixed { offset_secs: 0, id: "Z".into() }
    }

    /// `ZoneOffset.ofHours(h)`.
    pub fn of_hours(h: i64) -> Result<TimeZone> {
        if !(-18..=18).contains(&h) {
            return Err(AggError::new(
                400,
                "date_time_exception",
                format!("Zone offset hours not in valid range: value {h} is not in the range -18 to 18"),
            ));
        }
        Ok(TimeZone::Fixed { offset_secs: (h * 3600) as i32, id: offset_id((h * 3600) as i32) })
    }

    /// `ZoneId.of(id)`.
    pub fn parse(id: &str) -> Result<TimeZone> {
        if id == "Z" {
            return Ok(TimeZone::utc());
        }
        if id.len() <= 1 || id.starts_with('+') || id.starts_with('-') {
            let secs = parse_offset(id)?;
            return Ok(TimeZone::Fixed { offset_secs: secs, id: offset_id(secs) });
        }
        if id == "UTC" || id == "GMT" || id == "UT" {
            return Ok(TimeZone::Fixed { offset_secs: 0, id: id.to_string() });
        }
        for prefix in ["UTC", "GMT", "UT"] {
            if let Some(rest) = id.strip_prefix(prefix)
                && (rest.starts_with('+') || rest.starts_with('-'))
            {
                let secs = parse_offset(rest).map_err(|_| {
                    AggError::new(400, "date_time_exception", format!("Invalid ID for offset-based ZoneId: {id}"))
                })?;
                let id = if secs == 0 { prefix.to_string() } else { format!("{prefix}{}", offset_id(secs)) };
                return Ok(TimeZone::Fixed { offset_secs: secs, id });
            }
        }
        let valid = id.as_bytes()[0].is_ascii_alphabetic()
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'~' | b'/' | b'.' | b'_' | b'+' | b'-'));
        if !valid {
            return Err(AggError::new(400, "date_time_exception", format!("Invalid ID for region-based ZoneId, invalid format: {id}")));
        }
        id.parse::<chrono_tz::Tz>()
            .map(TimeZone::Named)
            .map_err(|_| AggError::new(400, "zone_rules_exception", format!("Unknown time-zone ID: {id}")))
    }

    /// The zone id as Java prints it.
    pub fn id(&self) -> String {
        match self {
            TimeZone::Fixed { id, .. } => id.clone(),
            TimeZone::Named(tz) => tz.name().to_string(),
        }
    }

    /// Whether the offset never changes.
    pub fn is_fixed(&self) -> bool {
        matches!(self, TimeZone::Fixed { .. })
    }

    /// The offset (seconds east of UTC) at an instant.
    #[inline]
    pub fn offset_at(&self, epoch_millis: i64) -> i32 {
        match self {
            TimeZone::Fixed { offset_secs, .. } => *offset_secs,
            TimeZone::Named(tz) => {
                let secs = epoch_millis.div_euclid(1000);
                match chrono::DateTime::from_timestamp(secs, 0) {
                    Some(dt) => tz.offset_from_utc_datetime(&dt.naive_utc()).fix().local_minus_utc(),
                    None => 0,
                }
            }
        }
    }

    /// Local wall-clock millis → epoch millis, resolving gaps and overlaps like
    /// `ZonedDateTime.ofLocal` without a preferred offset: a time in a gap moves forward by the
    /// gap's length; a time in an overlap takes the earlier offset.
    pub fn local_to_utc(&self, local_millis: i64) -> i64 {
        match self {
            TimeZone::Fixed { offset_secs, .. } => local_millis - *offset_secs as i64 * 1000,
            TimeZone::Named(tz) => {
                let secs = local_millis.div_euclid(1000);
                let ms = local_millis.rem_euclid(1000);
                let Some(naive) = chrono::DateTime::from_timestamp(secs, 0).map(|d| d.naive_utc()) else {
                    return local_millis;
                };
                match tz.offset_from_local_datetime(&naive) {
                    chrono::LocalResult::Single(o) => local_millis - o.fix().local_minus_utc() as i64 * 1000,
                    chrono::LocalResult::Ambiguous(a, b) => {
                        // The earlier instant is the one with the larger offset.
                        let (oa, ob) = (a.fix().local_minus_utc(), b.fix().local_minus_utc());
                        local_millis - oa.max(ob) as i64 * 1000
                    }
                    chrono::LocalResult::None => {
                        // In a gap: use the offset before the transition (Java shifts the local
                        // time forward by the gap length, which equals using the earlier offset).
                        let before = tz.offset_from_utc_datetime(&(naive - chrono::Duration::hours(24))).fix().local_minus_utc();
                        let _ = ms;
                        local_millis - before as i64 * 1000
                    }
                }
            }
        }
    }

    /// Epoch millis → local wall-clock millis.
    #[inline]
    pub fn utc_to_local(&self, epoch_millis: i64) -> i64 {
        epoch_millis + self.offset_at(epoch_millis) as i64 * 1000
    }

    /// The next offset transition strictly after `epoch_millis` (`None` for fixed zones or when
    /// there is none within ~100 years).
    pub fn next_transition(&self, epoch_millis: i64) -> Option<i64> {
        let TimeZone::Named(_) = self else { return None };
        let start = self.offset_at(epoch_millis);
        // Transitions are at least a day apart in practice; scan by days then bisect.
        let day = 86_400_000i64;
        let mut lo = epoch_millis;
        let mut hi = epoch_millis;
        for _ in 0..(366 * 100) {
            hi = hi.checked_add(day)?;
            if self.offset_at(hi) != start {
                break;
            }
            lo = hi;
        }
        if self.offset_at(hi) == start {
            return None;
        }
        // Invariant: offset(lo) == start, offset(hi) != start; transitions fall on whole seconds.
        while hi - lo > 1000 {
            let mid = lo + (hi - lo) / 2;
            let mid = mid - mid.rem_euclid(1000);
            let mid = if mid <= lo { lo + 1000 } else { mid };
            if mid >= hi {
                break;
            }
            if self.offset_at(mid) == start {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Some(hi)
    }
}

/// Java's `ZoneOffset.getId()` for an offset.
pub fn offset_id(secs: i32) -> String {
    if secs == 0 {
        return "Z".into();
    }
    let a = secs.unsigned_abs();
    let (h, m, s) = (a / 3600, a / 60 % 60, a % 60);
    let sign = if secs < 0 { '-' } else { '+' };
    if s != 0 { format!("{sign}{h:02}:{m:02}:{s:02}") } else { format!("{sign}{h:02}:{m:02}") }
}

/// `ZoneOffset.of(String)`.
fn parse_offset(id: &str) -> Result<i32> {
    let bad = || AggError::new(400, "date_time_exception", format!("Invalid ID for ZoneOffset, invalid format: {id}"));
    let b = id.as_bytes();
    if b.is_empty() || (b[0] != b'+' && b[0] != b'-') {
        return Err(bad());
    }
    let neg = b[0] == b'-';
    let num = |s: &[u8]| -> Result<i32> {
        if s.len() != 2 || !s.iter().all(u8::is_ascii_digit) {
            return Err(bad());
        }
        Ok(((s[0] - b'0') * 10 + (s[1] - b'0')) as i32)
    };
    let r = &b[1..];
    let (h, m, s) = match r.len() {
        1 if r[0].is_ascii_digit() => ((r[0] - b'0') as i32, 0, 0),
        2 => (num(r)?, 0, 0),
        4 => (num(&r[0..2])?, num(&r[2..4])?, 0),
        5 if r[2] == b':' => (num(&r[0..2])?, num(&r[3..5])?, 0),
        6 => (num(&r[0..2])?, num(&r[2..4])?, num(&r[4..6])?),
        8 if r[2] == b':' && r[5] == b':' => (num(&r[0..2])?, num(&r[3..5])?, num(&r[6..8])?),
        _ => return Err(bad()),
    };
    if h > 18 {
        return Err(AggError::new(400, "date_time_exception", format!("Zone offset hours not in valid range: value {h} is not in the range -18 to 18")));
    }
    if m > 59 {
        return Err(AggError::new(400, "date_time_exception", format!("Zone offset minutes not in valid range: value {m} is not in the range -59 to 59")));
    }
    if s > 59 {
        return Err(AggError::new(400, "date_time_exception", format!("Zone offset seconds not in valid range: value {s} is not in the range -59 to 59")));
    }
    let total = h * 3600 + m * 60 + s;
    if total > 18 * 3600 {
        return Err(AggError::new(400, "date_time_exception", "Zone offset not in valid range: -18:00 to +18:00"));
    }
    Ok(if neg { -total } else { total })
}

/// A date formatter (Elasticsearch's `DateFormatter`): prints instants and parses dates with
/// date math (`now-1d/d`, `2020-01-01||+1M`).
pub trait DateFormatter: Send + Sync + std::fmt::Debug {
    /// The pattern it was built from.
    fn pattern(&self) -> &str;
    /// Prints an instant (`millis` since the epoch plus `nanos` within the milli) in `zone`.
    fn format(&self, millis: i64, nanos: u32, zone: &TimeZone) -> String;
    /// Parses `text` with date math into epoch millis (`round_up` for `/unit` rounding of upper
    /// bounds; `zone` for dates without an offset).
    fn parse(&self, text: &str, now_millis: i64, round_up: bool, zone: &TimeZone) -> Result<i64>;
}

/// A `java.text.DecimalFormat`.
pub trait NumberFormatter: Send + Sync + std::fmt::Debug {
    fn pattern(&self) -> &str;
    fn format_double(&self, v: f64) -> String;
    fn format_long(&self, v: i64) -> String;
    /// `NumberFormat.parse` (`None` on a `ParseException`).
    fn parse(&self, text: &str) -> Option<f64>;
    fn is_parse_integer_only(&self) -> bool {
        false
    }
}

/// How a values source prints and parses (Elasticsearch's `DocValueFormat`).
#[derive(Clone, Debug, Default)]
pub enum DocValueFormat {
    #[default]
    Raw,
    Decimal(Arc<dyn NumberFormatter>),
    DateTime {
        formatter: Arc<dyn DateFormatter>,
        zone: TimeZone,
        nanos: bool,
    },
    Ip,
    Boolean,
    GeoHash,
    GeoTile,
    UnsignedLongShifted,
    Binary,
}

impl PartialEq for DocValueFormat {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (DocValueFormat::Raw, DocValueFormat::Raw)
            | (DocValueFormat::Ip, DocValueFormat::Ip)
            | (DocValueFormat::Boolean, DocValueFormat::Boolean)
            | (DocValueFormat::GeoHash, DocValueFormat::GeoHash)
            | (DocValueFormat::GeoTile, DocValueFormat::GeoTile)
            | (DocValueFormat::UnsignedLongShifted, DocValueFormat::UnsignedLongShifted)
            | (DocValueFormat::Binary, DocValueFormat::Binary) => true,
            (DocValueFormat::Decimal(a), DocValueFormat::Decimal(b)) => a.pattern() == b.pattern(),
            (DocValueFormat::DateTime { formatter: a, zone: za, nanos: na }, DocValueFormat::DateTime { formatter: b, zone: zb, nanos: nb }) => {
                a.pattern() == b.pattern() && za == zb && na == nb
            }
            _ => false,
        }
    }
}

impl DocValueFormat {
    pub fn is_raw(&self) -> bool {
        matches!(self, DocValueFormat::Raw)
    }

    /// The default date format (`strict_date_optional_time||epoch_millis`, UTC, millis).
    pub fn default_date() -> DocValueFormat {
        DocValueFormat::DateTime { formatter: builtin_date_formatter(DEFAULT_DATE_FORMAT).expect("default format"), zone: TimeZone::utc(), nanos: false }
    }

    /// `format(long)` as the Java object Elasticsearch prints (number, string or boolean).
    pub fn format_long(&self, v: i64) -> Json {
        match self {
            DocValueFormat::Raw => Json::Int(v),
            DocValueFormat::Decimal(f) => Json::Str(f.format_long(v)),
            DocValueFormat::DateTime { formatter, zone, nanos } => {
                if v == i64::MIN || v == i64::MAX {
                    return Json::Null;
                }
                let (ms, ns) = if *nanos { (v.div_euclid(1_000_000), (v.rem_euclid(1_000_000)) as u32) } else { (v, 0) };
                Json::Str(formatter.format(ms, ns, zone))
            }
            DocValueFormat::Boolean => Json::Bool(v != 0),
            DocValueFormat::GeoHash => Json::Str(crate::geo::geohash_string(v)),
            DocValueFormat::GeoTile => Json::Str(crate::geo::geotile_string(v)),
            DocValueFormat::UnsignedLongShifted => {
                let u = (v as u64) ^ (1u64 << 63);
                Json::from(u)
            }
            DocValueFormat::Ip | DocValueFormat::Binary => Json::Int(v),
        }
    }

    /// `format(double)`.
    pub fn format_double(&self, v: f64) -> Json {
        match self {
            DocValueFormat::Raw | DocValueFormat::UnsignedLongShifted => Json::Float(v),
            DocValueFormat::Decimal(f) => {
                if v.is_nan() {
                    Json::Str("NaN".into())
                } else {
                    Json::Str(f.format_double(v))
                }
            }
            DocValueFormat::DateTime { .. } => self.format_long(v as i64),
            DocValueFormat::Boolean => Json::Bool(v != 0.0),
            DocValueFormat::GeoHash | DocValueFormat::GeoTile => self.format_long(v as i64),
            DocValueFormat::Ip | DocValueFormat::Binary => Json::Float(v),
        }
    }

    /// `format(BytesRef)`.
    pub fn format_bytes(&self, b: &[u8]) -> Json {
        match self {
            DocValueFormat::Ip => Json::Str(ip_to_string(b)),
            DocValueFormat::Binary => Json::Str(base64(b)),
            _ => Json::Str(String::from_utf8_lossy(b).into_owned()),
        }
    }

    /// The `*_as_string` text of a double (`String.valueOf(format(v))`).
    pub fn double_string(&self, v: f64) -> String {
        match self.format_double(v) {
            Json::Str(s) => s,
            Json::Float(f) => java_double(f),
            other => other.to_java_string(),
        }
    }

    /// A `*_as_string` value: the formatted text, or `null` when the format yields none (dates
    /// outside the representable range).
    pub fn double_json(&self, v: f64) -> Json {
        match self.format_double(v) {
            Json::Null => Json::Null,
            _ => Json::Str(self.double_string(v)),
        }
    }

    /// [`DocValueFormat::double_json`] for longs.
    pub fn long_json(&self, v: i64) -> Json {
        match self.format_long(v) {
            Json::Null => Json::Null,
            other => Json::Str(match other {
                Json::Str(s) => s,
                o => o.to_java_string(),
            }),
        }
    }

    /// The `*_as_string` text of a long.
    pub fn long_string(&self, v: i64) -> String {
        match self.format_long(v) {
            Json::Str(s) => s,
            other => other.to_java_string(),
        }
    }

    /// `parseLong(value, roundUp, now)`.
    pub fn parse_long(&self, s: &str, round_up: bool, now: i64) -> Result<i64> {
        match self {
            DocValueFormat::Raw | DocValueFormat::Ip | DocValueFormat::Binary | DocValueFormat::GeoHash => {
                if let Some(v) = parse_java_long(s) {
                    return Ok(v);
                }
                let d = parse_java_double(s).ok_or_else(|| AggError::number_format(s))?;
                let d = if round_up { d.ceil() } else { d.floor() };
                Ok(java_round(d))
            }
            DocValueFormat::Decimal(f) => {
                let d = f.parse(s).ok_or_else(|| {
                    AggError::new(500, "exception", format!("Cannot parse the value [{s}] using the pattern [{}]", f.pattern()))
                })?;
                if f.is_parse_integer_only() {
                    return Ok(d as i64);
                }
                let d = if round_up { d.ceil() } else { d.floor() };
                Ok(java_round(d))
            }
            DocValueFormat::DateTime { formatter, zone, nanos } => {
                let ms = formatter.parse(s, now, round_up, zone)?;
                if *nanos {
                    ms.checked_mul(1_000_000).ok_or_else(|| {
                        AggError::illegal_argument(format!("date[{s}] is after 2262-04-11T23:47:16.854775807 and cannot be stored in nanosecond resolution"))
                    })
                } else {
                    Ok(ms)
                }
            }
            DocValueFormat::Boolean => match s {
                "false" => Ok(0),
                "true" => Ok(1),
                _ => Err(AggError::illegal_argument(format!("Cannot parse boolean [{s}], expected either [true] or [false]"))),
            },
            DocValueFormat::GeoTile => crate::geo::geotile_parse(s),
            DocValueFormat::UnsignedLongShifted => {
                let u: u64 = s.parse().map_err(|_| AggError::number_format(s))?;
                Ok((u ^ (1u64 << 63)) as i64)
            }
        }
    }

    /// `parseDouble(value, roundUp, now)`.
    pub fn parse_double(&self, s: &str, round_up: bool, now: i64) -> Result<f64> {
        match self {
            DocValueFormat::Raw | DocValueFormat::UnsignedLongShifted | DocValueFormat::Ip | DocValueFormat::Binary | DocValueFormat::GeoHash | DocValueFormat::GeoTile => {
                if s.trim().is_empty() {
                    return Err(AggError::new(400, "number_format_exception", "empty String"));
                }
                parse_java_double(s).ok_or_else(|| AggError::number_format(s))
            }
            DocValueFormat::Decimal(f) => f.parse(s).ok_or_else(|| {
                AggError::new(500, "exception", format!("Cannot parse the value [{s}] using the pattern [{}]", f.pattern()))
            }),
            DocValueFormat::DateTime { .. } | DocValueFormat::Boolean => self.parse_long(s, round_up, now).map(|v| v as f64),
        }
    }

    /// `parseBytesRef(value)`.
    pub fn parse_bytes(&self, v: &Json) -> Result<Vec<u8>> {
        let s = match v {
            Json::Str(s) => s.clone(),
            other => other.to_java_string(),
        };
        match self {
            DocValueFormat::Ip => parse_ip(&s).map(|b| b.to_vec()),
            DocValueFormat::Binary => unbase64(&s).ok_or_else(|| AggError::illegal_argument(format!("Illegal base64 character in [{s}]"))),
            _ => Ok(s.into_bytes()),
        }
    }
}

/// Java's `Math.round(double)`: the closest long, ties towards positive infinity (exactly:
/// `floor(d + 0.5)` without the rounding error of the addition), saturating.
pub fn java_round(d: f64) -> i64 {
    if d.is_nan() {
        return 0;
    }
    let f = d.floor();
    (if d - f >= 0.5 { f + 1.0 } else { f }) as i64
}

/// `DateFieldMapper.DEFAULT_DATE_TIME_FORMATTER`.
pub const DEFAULT_DATE_FORMAT: &str = "strict_date_optional_time||epoch_millis";

/// A built-in subset of Elasticsearch date formats: the named ISO formats, `epoch_millis`,
/// `epoch_second` and `DateTimeFormatter` patterns of the common letters. Parsing accepts ISO
/// dates, epoch millis and date math.
pub fn builtin_date_formatter(pattern: &str) -> Result<Arc<dyn DateFormatter>> {
    let first = pattern.split("||").next().unwrap_or("").trim();
    let print = match first {
        "strict_date_optional_time" | "date_optional_time" | "strict_date_time" | "date_time" | "iso8601" => {
            Print::Pattern(compile_pattern("yyyy-MM-dd'T'HH:mm:ss.SSSXXX")?)
        }
        "strict_date_optional_time_nanos" => Print::Pattern(compile_pattern("yyyy-MM-dd'T'HH:mm:ss.SSSSSSSSSXXX")?),
        "epoch_millis" => Print::EpochMillis,
        "epoch_second" => Print::EpochSecond,
        "strict_date" | "date" | "yyyy-MM-dd" => Print::Pattern(compile_pattern("yyyy-MM-dd")?),
        "strict_year_month" | "year_month" => Print::Pattern(compile_pattern("yyyy-MM")?),
        "strict_year" | "year" => Print::Pattern(compile_pattern("yyyy")?),
        "basic_date" => Print::Pattern(compile_pattern("yyyyMMdd")?),
        "strict_hour_minute_second" | "hour_minute_second" => Print::Pattern(compile_pattern("HH:mm:ss")?),
        "strict_date_hour_minute_second" | "date_hour_minute_second" => Print::Pattern(compile_pattern("yyyy-MM-dd'T'HH:mm:ss")?),
        "strict_date_hour_minute" | "date_hour_minute" => Print::Pattern(compile_pattern("yyyy-MM-dd'T'HH:mm")?),
        "strict_date_hour" | "date_hour" => Print::Pattern(compile_pattern("yyyy-MM-dd'T'HH")?),
        p => Print::Pattern(compile_pattern(p)?),
    };
    Ok(Arc::new(BuiltinDateFormatter { pattern: pattern.to_string(), print }))
}

#[derive(Debug)]
enum Print {
    Pattern(Vec<Tok>),
    EpochMillis,
    EpochSecond,
}

#[derive(Debug, Clone)]
enum Tok {
    Lit(String),
    /// Field letter and count.
    F(char, usize),
}

fn compile_pattern(p: &str) -> Result<Vec<Tok>> {
    let mut out = Vec::new();
    let cs: Vec<char> = p.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c == '\'' {
            let mut lit = String::new();
            i += 1;
            while i < cs.len() {
                if cs[i] == '\'' {
                    if i + 1 < cs.len() && cs[i + 1] == '\'' {
                        lit.push('\'');
                        i += 2;
                        continue;
                    }
                    break;
                }
                lit.push(cs[i]);
                i += 1;
            }
            i += 1;
            if lit.is_empty() {
                lit.push('\'');
            }
            out.push(Tok::Lit(lit));
        } else if c.is_ascii_alphabetic() {
            let mut n = 1;
            while i + n < cs.len() && cs[i + n] == c {
                n += 1;
            }
            if !"uyYMLdDHhmsSnaEeXxZVzGkKAN".contains(c) {
                return Err(AggError::illegal_argument(format!("Invalid format: [{p}]: Unknown pattern letter: {c}")));
            }
            out.push(Tok::F(c, n));
            i += n;
        } else {
            out.push(Tok::Lit(c.to_string()));
            i += 1;
        }
    }
    Ok(out)
}

#[derive(Debug)]
struct BuiltinDateFormatter {
    pattern: String,
    print: Print,
}

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

impl DateFormatter for BuiltinDateFormatter {
    fn pattern(&self) -> &str {
        &self.pattern
    }

    fn format(&self, millis: i64, nanos: u32, zone: &TimeZone) -> String {
        match &self.print {
            Print::EpochMillis => {
                if nanos == 0 {
                    millis.to_string()
                } else {
                    format!("{millis}.{:06}", nanos).trim_end_matches('0').to_string()
                }
            }
            Print::EpochSecond => {
                let s = millis.div_euclid(1000);
                let ms = millis.rem_euclid(1000);
                if ms == 0 && nanos == 0 { s.to_string() } else { format!("{s}.{:03}", ms).trim_end_matches('0').to_string() }
            }
            Print::Pattern(toks) => {
                let off = zone.offset_at(millis);
                let local = millis + off as i64 * 1000;
                let secs = local.div_euclid(1000);
                let ms = local.rem_euclid(1000) as u32;
                let Some(dt) = chrono::DateTime::from_timestamp(secs, ms * 1_000_000 + nanos).map(|d| d.naive_utc()) else {
                    return millis.to_string();
                };
                let mut out = String::new();
                for t in toks {
                    match t {
                        Tok::Lit(l) => out.push_str(l),
                        Tok::F(c, n) => print_field(&mut out, *c, *n, &dt, off, zone),
                    }
                }
                out
            }
        }
    }

    fn parse(&self, text: &str, now_millis: i64, round_up: bool, zone: &TimeZone) -> Result<i64> {
        parse_date_math(text, now_millis, round_up, zone, matches!(self.print, Print::EpochSecond))
    }
}

fn pad(out: &mut String, v: i64, n: usize) {
    if v < 0 {
        out.push('-');
    }
    let s = v.unsigned_abs().to_string();
    for _ in s.len()..n {
        out.push('0');
    }
    out.push_str(&s);
}

fn print_field(out: &mut String, c: char, n: usize, dt: &NaiveDateTime, off: i32, zone: &TimeZone) {
    match c {
        'y' | 'u' | 'Y' => {
            let y = dt.year() as i64;
            if n == 2 {
                pad(out, y.rem_euclid(100), 2)
            } else {
                if y > 9999 && n == 4 {
                    out.push('+');
                }
                pad(out, y, n.max(1))
            }
        }
        'M' | 'L' => match n {
            1 | 2 => pad(out, dt.month() as i64, n),
            3 => out.push_str(&MONTHS[dt.month0() as usize][..3]),
            _ => out.push_str(MONTHS[dt.month0() as usize]),
        },
        'd' => pad(out, dt.day() as i64, n),
        'D' => pad(out, dt.ordinal() as i64, n),
        'H' => pad(out, dt.hour() as i64, n),
        'k' => pad(out, if dt.hour() == 0 { 24 } else { dt.hour() as i64 }, n),
        'h' => pad(out, ((dt.hour() + 11) % 12 + 1) as i64, n),
        'K' => pad(out, (dt.hour() % 12) as i64, n),
        'm' => pad(out, dt.minute() as i64, n),
        's' => pad(out, dt.second() as i64, n),
        'S' => {
            let nanos = dt.nanosecond() as i64;
            let digits = format!("{nanos:09}");
            if n <= 9 {
                out.push_str(&digits[..n]);
            } else {
                out.push_str(&digits);
                out.extend(std::iter::repeat_n('0', n - 9));
            }
        }
        'n' => pad(out, dt.nanosecond() as i64, n),
        'A' => pad(out, (dt.num_seconds_from_midnight() as i64) * 1000 + (dt.nanosecond() / 1_000_000) as i64, n),
        'a' => out.push_str(if dt.hour() < 12 { "AM" } else { "PM" }),
        'E' | 'e' => {
            let d = DAYS[dt.weekday().num_days_from_monday() as usize];
            if n >= 4 { out.push_str(d) } else { out.push_str(&d[..3]) }
        }
        'G' => out.push_str(if dt.year() > 0 { "AD" } else { "BC" }),
        'X' | 'x' | 'Z' => {
            if off == 0 && c == 'X' {
                out.push('Z');
                return;
            }
            let a = off.unsigned_abs();
            let (h, m, s) = (a / 3600, a / 60 % 60, a % 60);
            let sign = if off < 0 { '-' } else { '+' };
            match (c, n) {
                ('Z', 1..=3) => out.push_str(&format!("{sign}{h:02}{m:02}")),
                ('Z', 4) => {
                    if off == 0 {
                        out.push_str("GMT")
                    } else {
                        out.push_str(&format!("GMT{sign}{h:02}:{m:02}"))
                    }
                }
                ('Z', _) => {
                    if off == 0 {
                        out.push('Z')
                    } else {
                        out.push_str(&format!("{sign}{h:02}:{m:02}"))
                    }
                }
                (_, 1) => {
                    out.push_str(&format!("{sign}{h:02}"));
                    if m != 0 {
                        out.push_str(&format!("{m:02}"));
                    }
                }
                (_, 2) | (_, 4) => out.push_str(&format!("{sign}{h:02}{m:02}")),
                _ => {
                    out.push_str(&format!("{sign}{h:02}:{m:02}"));
                    if s != 0 {
                        out.push_str(&format!(":{s:02}"));
                    }
                }
            }
        }
        'V' | 'z' => out.push_str(&zone.id()),
        'N' => pad(out, dt.nanosecond() as i64, n),
        _ => {}
    }
}

/// Date math over ISO dates and epoch millis: `now`, `<date>||`, then `+N<unit>`, `-N<unit>`,
/// `/<unit>`.
pub fn parse_date_math(text: &str, now: i64, round_up: bool, zone: &TimeZone, epoch_seconds: bool) -> Result<i64> {
    let fail = |t: &str| {
        AggError::new(400, "parse_exception", format!("failed to parse date field [{t}] with format [strict_date_optional_time||epoch_millis]"))
    };
    let (base, math) = if let Some(rest) = text.strip_prefix("now") {
        (now, rest)
    } else if let Some((d, m)) = text.split_once("||") {
        (parse_date(d, zone, epoch_seconds).ok_or_else(|| fail(d))?, m)
    } else {
        let v = parse_date(text, zone, epoch_seconds).ok_or_else(|| fail(text))?;
        if round_up && !text.chars().all(|c| c.is_ascii_digit() || c == '-') {
            return Ok(v);
        }
        return Ok(v);
    };
    let mut t = base;
    let m = math.as_bytes();
    let mut i = 0;
    while i < m.len() {
        let op = m[i];
        i += 1;
        if op == b'/' {
            let unit = *m.get(i).ok_or_else(|| fail(text))?;
            i += 1;
            t = round_unit(t, unit, round_up, zone).ok_or_else(|| fail(text))?;
        } else if op == b'+' || op == b'-' {
            let s = i;
            while i < m.len() && m[i].is_ascii_digit() {
                i += 1;
            }
            let n: i64 = if s == i { 1 } else { std::str::from_utf8(&m[s..i]).ok().and_then(|x| x.parse().ok()).ok_or_else(|| fail(text))? };
            let unit = *m.get(i).ok_or_else(|| fail(text))?;
            i += 1;
            let n = if op == b'-' { -n } else { n };
            t = add_unit(t, unit, n, zone).ok_or_else(|| fail(text))?;
        } else {
            return Err(fail(text));
        }
    }
    Ok(t)
}

fn local_dt(t: i64, zone: &TimeZone) -> Option<NaiveDateTime> {
    let l = zone.utc_to_local(t);
    chrono::DateTime::from_timestamp(l.div_euclid(1000), (l.rem_euclid(1000) * 1_000_000) as u32).map(|d| d.naive_utc())
}

fn from_local(dt: NaiveDateTime, zone: &TimeZone) -> i64 {
    zone.local_to_utc(dt.and_utc().timestamp_millis())
}

fn add_unit(t: i64, unit: u8, n: i64, zone: &TimeZone) -> Option<i64> {
    let dt = local_dt(t, zone)?;
    let r = match unit {
        b'y' => dt.checked_add_months(chrono::Months::new((n.unsigned_abs() * 12) as u32)).filter(|_| n >= 0).or_else(|| dt.checked_sub_months(chrono::Months::new((n.unsigned_abs() * 12) as u32)))?,
        b'M' => {
            if n >= 0 {
                dt.checked_add_months(chrono::Months::new(n as u32))?
            } else {
                dt.checked_sub_months(chrono::Months::new((-n) as u32))?
            }
        }
        b'w' => dt + chrono::Duration::weeks(n),
        b'd' => dt + chrono::Duration::days(n),
        b'h' | b'H' => return Some(t + n * 3_600_000),
        b'm' => return Some(t + n * 60_000),
        b's' => return Some(t + n * 1000),
        _ => return None,
    };
    Some(from_local(r, zone))
}

fn round_unit(t: i64, unit: u8, round_up: bool, zone: &TimeZone) -> Option<i64> {
    let dt = local_dt(t, zone)?;
    let d = dt.date();
    let start = match unit {
        b'y' => NaiveDate::from_ymd_opt(d.year(), 1, 1)?.and_hms_opt(0, 0, 0)?,
        b'M' => NaiveDate::from_ymd_opt(d.year(), d.month(), 1)?.and_hms_opt(0, 0, 0)?,
        b'w' => (d - chrono::Duration::days(d.weekday().num_days_from_monday() as i64)).and_hms_opt(0, 0, 0)?,
        b'd' => d.and_hms_opt(0, 0, 0)?,
        b'h' | b'H' => d.and_hms_opt(dt.hour(), 0, 0)?,
        b'm' => d.and_hms_opt(dt.hour(), dt.minute(), 0)?,
        b's' => d.and_hms_opt(dt.hour(), dt.minute(), dt.second())?,
        _ => return None,
    };
    let s = from_local(start, zone);
    if !round_up {
        return Some(s);
    }
    let next = add_unit(s, unit, 1, zone)?;
    Some(next - 1)
}

/// ISO-8601 date-time (`yyyy-MM-dd['T'HH:mm[:ss[.fraction]]][Z|±hh:mm]`) or epoch millis.
fn parse_date(s: &str, zone: &TimeZone, epoch_seconds: bool) -> Option<i64> {
    let s = s.trim();
    if !s.is_empty() && (s.bytes().all(|b| b.is_ascii_digit()) || (s.starts_with('-') && s.len() > 1 && s[1..].bytes().all(|b| b.is_ascii_digit()))) && (s.len() != 4 || s.starts_with('-')) {
        let v: i64 = s.parse().ok()?;
        return Some(if epoch_seconds { v * 1000 } else { v });
    }
    if let Some(d) = parse_java_double(s).filter(|_| s.contains('.') && s.bytes().all(|b| b.is_ascii_digit() || b == b'.' || b == b'-')) {
        return Some(if epoch_seconds { (d * 1000.0) as i64 } else { d as i64 });
    }
    let b = s.as_bytes();
    let num = |a: usize, n: usize| -> Option<u32> {
        let t = b.get(a..a + n)?;
        if !t.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(t).ok()?.parse().ok()
    };
    let year = num(0, 4)? as i32;
    let (mut month, mut day) = (1, 1);
    let mut i = 4;
    if b.get(i) == Some(&b'-') {
        month = num(i + 1, 2)?;
        i += 3;
        if b.get(i) == Some(&b'-') {
            day = num(i + 1, 2)?;
            i += 3;
        }
    }
    let (mut h, mut mi, mut sec, mut nanos) = (0, 0, 0, 0u32);
    if b.get(i) == Some(&b'T') || b.get(i) == Some(&b' ') {
        h = num(i + 1, 2)?;
        i += 3;
        if b.get(i) == Some(&b':') {
            mi = num(i + 1, 2)?;
            i += 3;
            if b.get(i) == Some(&b':') {
                sec = num(i + 1, 2)?;
                i += 3;
                if b.get(i) == Some(&b'.') || b.get(i) == Some(&b',') {
                    i += 1;
                    let st = i;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                    let frac = std::str::from_utf8(&b[st..i]).ok()?;
                    if frac.is_empty() || frac.len() > 9 {
                        return None;
                    }
                    nanos = format!("{frac:0<9}").parse().ok()?;
                }
            }
        }
    }
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let dt = date.and_hms_nano_opt(h, mi, sec, nanos)?;
    let local = dt.and_utc().timestamp_millis();
    let rest = &s[i..];
    if rest.is_empty() {
        return Some(zone.local_to_utc(local));
    }
    if rest == "Z" {
        return Some(local);
    }
    let off = parse_offset(rest).ok()?;
    Some(local - off as i64 * 1000)
}

/// A built-in `java.text.DecimalFormat` with `Locale.ROOT` symbols and `HALF_EVEN` rounding:
/// positive/negative subpatterns, quoted prefix/suffix literals, `#`, `0`, grouping, `%`, `‰`
/// and scientific notation (`0.###E0`).
pub fn builtin_decimal_format(pattern: &str) -> Result<Arc<dyn NumberFormatter>> {
    Ok(Arc::new(DecimalFmt::new(pattern)?))
}

#[derive(Debug, Clone)]
struct DecimalFmt {
    pattern: String,
    pos_prefix: String,
    pos_suffix: String,
    neg_prefix: String,
    neg_suffix: String,
    min_int: usize,
    max_int: usize,
    min_frac: usize,
    max_frac: usize,
    grouping: usize,
    multiplier: i32,
    decimal_always: bool,
    exp_digits: Option<usize>,
}

impl DecimalFmt {
    fn new(pattern: &str) -> Result<DecimalFmt> {
        let bad = |why: &str| AggError::illegal_argument(format!("{why} in pattern \"{pattern}\""));
        let (pos, neg) = split_subpattern(pattern);
        let (prefix, number, suffix) = split_affixes(&pos);
        let mut multiplier = 1;
        let affix_text = |a: &str, mult: &mut i32| -> String {
            let mut out = String::new();
            let mut quoted = false;
            let cs: Vec<char> = a.chars().collect();
            let mut i = 0;
            while i < cs.len() {
                let c = cs[i];
                if c == '\'' {
                    if i + 1 < cs.len() && cs[i + 1] == '\'' {
                        out.push('\'');
                        i += 2;
                        continue;
                    }
                    quoted = !quoted;
                } else if !quoted && c == '%' {
                    *mult = 100;
                    out.push('%');
                } else if !quoted && c == '\u{2030}' {
                    *mult = 1000;
                    out.push('\u{2030}');
                } else if !quoted && c == '\u{a4}' {
                    out.push_str("XXX");
                } else {
                    out.push(c);
                }
                i += 1;
            }
            out
        };
        let pos_prefix = affix_text(&prefix, &mut multiplier);
        let pos_suffix = affix_text(&suffix, &mut multiplier);
        let (neg_prefix, neg_suffix) = match neg {
            Some(n) => {
                let (np, _, ns) = split_affixes(&n);
                let mut m2 = multiplier;
                (affix_text(&np, &mut m2), affix_text(&ns, &mut m2))
            }
            None => (format!("-{pos_prefix}"), pos_suffix.clone()),
        };
        let (mantissa, exp) = match number.find('E') {
            Some(i) => (&number[..i], Some(&number[i + 1..])),
            None => (number.as_str(), None),
        };
        let (int_part, frac_part) = match mantissa.find('.') {
            Some(i) => (&mantissa[..i], Some(&mantissa[i + 1..])),
            None => (mantissa, None),
        };
        let grouping = match int_part.rfind(',') {
            Some(i) => int_part.len() - i - 1,
            None => 0,
        };
        let int_digits: String = int_part.chars().filter(|c| *c != ',').collect();
        if int_digits.chars().any(|c| c != '#' && c != '0') {
            return Err(bad("Malformed pattern"));
        }
        let min_int = int_digits.chars().filter(|c| *c == '0').count();
        let (min_frac, max_frac) = match frac_part {
            Some(f) => {
                if f.chars().any(|c| c != '#' && c != '0') {
                    return Err(bad("Malformed pattern"));
                }
                (f.chars().filter(|c| *c == '0').count(), f.len())
            }
            None => (0, 0),
        };
        let exp_digits = exp.map(|e| e.chars().filter(|c| *c == '0').count().max(1));
        Ok(DecimalFmt {
            pattern: pattern.to_string(),
            pos_prefix,
            pos_suffix,
            neg_prefix,
            neg_suffix,
            min_int,
            max_int: if exp.is_some() { int_digits.len() } else { usize::MAX },
            min_frac,
            max_frac,
            grouping,
            multiplier,
            decimal_always: frac_part.is_some() && max_frac == 0,
            exp_digits,
        })
    }

    fn format_digits(&self, digits: &[u8], mut point: i64, negative: bool) -> String {
        // `digits` is the significant decimal digits, `point` the position of the decimal point.
        let mut out = String::new();
        if let Some(ed) = self.exp_digits {
            // Scientific: one integer digit per `max_int` rule (simplified to min_int digits).
            let int_digits = self.min_int.max(1) as i64;
            let exp = point - int_digits;
            point = int_digits;
            let total_max = int_digits as usize + self.max_frac;
            let mut d = digits.to_vec();
            let (rd, carry) = round_half_even_digits(&d, total_max);
            d = rd;
            let mut exp = exp;
            if carry {
                exp += 1;
            }
            let mut mant = String::new();
            for i in 0..int_digits as usize {
                mant.push(*d.get(i).unwrap_or(&b'0') as char);
            }
            let mut frac: String = d.iter().skip(int_digits as usize).map(|b| *b as char).collect();
            while frac.len() > self.min_frac && frac.ends_with('0') {
                frac.pop();
            }
            while frac.len() < self.min_frac {
                frac.push('0');
            }
            out.push_str(&mant);
            if !frac.is_empty() {
                out.push('.');
                out.push_str(&frac);
            }
            out.push('E');
            if exp < 0 {
                out.push('-');
            }
            out.push_str(&format!("{:0width$}", exp.unsigned_abs(), width = ed));
            let _ = point;
            return self.affix(out, negative);
        }
        // Round to max_frac fraction digits.
        let keep = (point + self.max_frac as i64).max(0) as usize;
        let (d, carry) = if keep < digits.len() { round_half_even_digits(digits, keep) } else { (digits.to_vec(), false) };
        if carry {
            point += 1;
        }
        let d: Vec<u8> = d;
        let int_len = point.max(0) as usize;
        let mut int_str: String = (0..int_len).map(|i| *d.get(i).unwrap_or(&b'0') as char).collect();
        let int_str_trim = int_str.trim_start_matches('0').to_string();
        int_str = int_str_trim;
        if int_str.len() > self.max_int {
            int_str = int_str[int_str.len() - self.max_int..].to_string();
        }
        while int_str.len() < self.min_int {
            int_str.insert(0, '0');
        }
        let mut frac = String::new();
        for i in 0..self.max_frac {
            let idx = point + i as i64;
            let c = if idx < 0 { b'0' } else { *d.get(idx as usize).unwrap_or(&b'0') };
            frac.push(c as char);
        }
        while frac.len() > self.min_frac && frac.ends_with('0') {
            frac.pop();
        }
        if int_str.is_empty() && frac.is_empty() {
            int_str.push('0');
        }
        if self.grouping > 0 && int_str.len() > self.grouping {
            let mut g = String::new();
            let n = int_str.len();
            for (i, c) in int_str.chars().enumerate() {
                if i > 0 && (n - i).is_multiple_of(self.grouping) {
                    g.push(',');
                }
                g.push(c);
            }
            int_str = g;
        }
        out.push_str(&int_str);
        if !frac.is_empty() || self.decimal_always {
            out.push('.');
            out.push_str(&frac);
        }
        let all_zero = !d.iter().take((point + self.max_frac as i64).max(0) as usize).any(|c| *c != b'0');
        self.affix(out, negative && !all_zero)
    }

    fn affix(&self, body: String, negative: bool) -> String {
        if negative { format!("{}{}{}", self.neg_prefix, body, self.neg_suffix) } else { format!("{}{}{}", self.pos_prefix, body, self.pos_suffix) }
    }
}

/// Rounds a digit string half-even to `keep` digits; returns the digits (length `keep`, or
/// `keep + 1` digits shifted when a carry adds one) and whether a carry added a digit.
fn round_half_even_digits(digits: &[u8], keep: usize) -> (Vec<u8>, bool) {
    if keep >= digits.len() {
        return (digits.to_vec(), false);
    }
    let mut d: Vec<u8> = digits[..keep].to_vec();
    let rest = &digits[keep..];
    let first = rest[0];
    let more = rest[1..].iter().any(|c| *c != b'0');
    let up = first > b'5' || (first == b'5' && (more || d.last().is_some_and(|l| (l - b'0') % 2 == 1)));
    if !up {
        return (d, false);
    }
    let mut i = d.len();
    loop {
        if i == 0 {
            d.insert(0, b'1');
            return (d, true);
        }
        i -= 1;
        if d[i] == b'9' {
            d[i] = b'0';
        } else {
            d[i] += 1;
            return (d, false);
        }
    }
}

fn split_subpattern(p: &str) -> (String, Option<String>) {
    let mut quoted = false;
    for (i, c) in p.char_indices() {
        if c == '\'' {
            quoted = !quoted;
        } else if c == ';' && !quoted {
            return (p[..i].to_string(), Some(p[i + 1..].to_string()));
        }
    }
    (p.to_string(), None)
}

fn split_affixes(p: &str) -> (String, String, String) {
    let cs: Vec<char> = p.chars().collect();
    let mut quoted = false;
    let mut start = None;
    let mut end = 0;
    for (i, c) in cs.iter().enumerate() {
        if *c == '\'' {
            quoted = !quoted;
            continue;
        }
        if !quoted && "#0,.E".contains(*c) {
            if start.is_none() {
                start = Some(i);
            }
            end = i + 1;
        }
    }
    match start {
        Some(s) => (cs[..s].iter().collect(), cs[s..end].iter().collect(), cs[end..].iter().collect()),
        None => (p.to_string(), String::new(), String::new()),
    }
}

/// The exact decimal digits of a finite double's magnitude scaled by `mult`: (digits, point).
fn decimal_digits(v: f64) -> (Vec<u8>, i64) {
    // Shortest round-trip digits (what Java's DigitList starts from).
    let s = format!("{:e}", v.abs());
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let exp: i64 = exp.parse().unwrap_or(0);
    let digits: Vec<u8> = mant.bytes().filter(|b| *b != b'.').collect();
    (digits, exp + 1)
}

impl NumberFormatter for DecimalFmt {
    fn pattern(&self) -> &str {
        &self.pattern
    }

    fn format_double(&self, v: f64) -> String {
        if v.is_nan() {
            return "NaN".into();
        }
        let negative = v.is_sign_negative() && v != 0.0 || (v == 0.0 && v.is_sign_negative());
        let scaled = v * self.multiplier as f64;
        if scaled.is_infinite() {
            return self.affix("\u{221e}".into(), negative);
        }
        if scaled == 0.0 {
            return self.format_digits(b"0", 1, negative);
        }
        let (d, p) = decimal_digits(scaled);
        self.format_digits(&d, p, negative)
    }

    fn format_long(&self, v: i64) -> String {
        let negative = v < 0;
        let m = (v as i128) * self.multiplier as i128;
        let s = m.unsigned_abs().to_string();
        let p = s.len() as i64;
        self.format_digits(s.as_bytes(), p, negative)
    }

    fn parse(&self, text: &str) -> Option<f64> {
        let (body, neg) = if !self.neg_prefix.is_empty() && text.starts_with(&self.neg_prefix) && text.ends_with(&self.neg_suffix) {
            (&text[self.neg_prefix.len()..text.len() - self.neg_suffix.len()], true)
        } else if text.starts_with(&self.pos_prefix) && text.ends_with(&self.pos_suffix) && text.len() >= self.pos_prefix.len() + self.pos_suffix.len() {
            (&text[self.pos_prefix.len()..text.len() - self.pos_suffix.len()], false)
        } else {
            return None;
        };
        let mut end = 0;
        let b = body.as_bytes();
        while end < b.len() && (b[end].is_ascii_digit() || b[end] == b',' || b[end] == b'.' || b[end] == b'E' || (b[end] == b'-' && end > 0 && b[end - 1] == b'E')) {
            end += 1;
        }
        if end == 0 {
            return None;
        }
        let clean: String = body[..end].chars().filter(|c| *c != ',').collect();
        let v: f64 = clean.parse().ok()?;
        let v = v / self.multiplier as f64;
        Some(if neg { -v } else { v })
    }
}

/// `NetworkAddress.format(InetAddressPoint.decode(bytes))`.
pub fn ip_to_string(b: &[u8]) -> String {
    if b.len() == 4 {
        return format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]);
    }
    if b.len() != 16 {
        return String::from_utf8_lossy(b).into_owned();
    }
    if b[..10].iter().all(|x| *x == 0) && b[10] == 0xff && b[11] == 0xff {
        return format!("{}.{}.{}.{}", b[12], b[13], b[14], b[15]);
    }
    let groups: Vec<u16> = (0..8).map(|i| u16::from_be_bytes([b[2 * i], b[2 * i + 1]])).collect();
    // Longest run of zero groups (length >= 2), leftmost on ties (RFC 5952).
    let (mut best_start, mut best_len) = (usize::MAX, 0);
    let mut i = 0;
    while i < 8 {
        if groups[i] == 0 {
            let s = i;
            while i < 8 && groups[i] == 0 {
                i += 1;
            }
            if i - s > best_len && i - s >= 2 {
                best_start = s;
                best_len = i - s;
            }
        } else {
            i += 1;
        }
    }
    let mut out = String::new();
    let mut i = 0;
    while i < 8 {
        if i == best_start {
            out.push_str("::");
            i += best_len;
            continue;
        }
        if !out.is_empty() && !out.ends_with(':') {
            out.push(':');
        }
        out.push_str(&format!("{:x}", groups[i]));
        i += 1;
    }
    out
}

/// `InetAddresses.forString` → `InetAddressPoint.encode` (16 bytes; IPv4 mapped).
pub fn parse_ip(s: &str) -> Result<[u8; 16]> {
    let bad = || AggError::illegal_argument(format!("'{s}' is not an IP string literal."));
    let mut out = [0u8; 16];
    if !s.contains(':') {
        let v4 = parse_v4(s).ok_or_else(bad)?;
        out[10] = 0xff;
        out[11] = 0xff;
        out[12..].copy_from_slice(&v4);
        return Ok(out);
    }
    let (body, v4tail) = match s.rfind(':') {
        Some(i) if s[i + 1..].contains('.') => (&s[..=i], Some(parse_v4(&s[i + 1..]).ok_or_else(bad)?)),
        _ => (s, None),
    };
    let body = if v4tail.is_some() && body.ends_with(':') && !body.ends_with("::") { &body[..body.len() - 1] } else { body };
    let ngroups = if v4tail.is_some() { 6 } else { 8 };
    let parse_groups = |t: &str| -> Option<Vec<u16>> {
        if t.is_empty() {
            return Some(Vec::new());
        }
        t.split(':')
            .map(|g| if g.is_empty() || g.len() > 4 { None } else { u16::from_str_radix(g, 16).ok() })
            .collect()
    };
    let groups: Vec<u16> = if let Some(i) = body.find("::") {
        let left = parse_groups(&body[..i]).ok_or_else(bad)?;
        let right_s = &body[i + 2..];
        if right_s.contains("::") {
            return Err(bad());
        }
        let right = parse_groups(right_s).ok_or_else(bad)?;
        if left.len() + right.len() >= ngroups {
            return Err(bad());
        }
        let mut g = left;
        g.extend(std::iter::repeat_n(0, ngroups - g.len() - right.len()));
        g.extend(right);
        g
    } else {
        let g = parse_groups(body).ok_or_else(bad)?;
        if g.len() != ngroups {
            return Err(bad());
        }
        g
    };
    for (i, g) in groups.iter().enumerate() {
        out[2 * i..2 * i + 2].copy_from_slice(&g.to_be_bytes());
    }
    if let Some(v4) = v4tail {
        out[12..].copy_from_slice(&v4);
    }
    Ok(out)
}

fn parse_v4(s: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut out = [0u8; 4];
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) || (p.len() > 1 && p.starts_with('0')) {
            return None;
        }
        out[i] = p.parse::<u16>().ok().filter(|v| *v <= 255)? as u8;
    }
    Some(out)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn base64(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

fn unbase64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut n = 0;
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = B64.iter().position(|x| *x == c)? as u32;
        buf = buf << 6 | v;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((buf >> n) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zones() {
        assert_eq!(TimeZone::parse("+01:00").unwrap().id(), "+01:00");
        assert_eq!(TimeZone::parse("Z").unwrap().id(), "Z");
        assert_eq!(TimeZone::parse("UTC").unwrap().id(), "UTC");
        assert_eq!(TimeZone::parse("GMT+1").unwrap().id(), "GMT+01:00");
        assert_eq!(TimeZone::parse("Foo/Bar").unwrap_err().reason, "Unknown time-zone ID: Foo/Bar");
        assert_eq!(TimeZone::parse("utc").unwrap_err().kind, "zone_rules_exception");
        assert_eq!(
            TimeZone::parse("+25:00").unwrap_err().reason,
            "Zone offset hours not in valid range: value 25 is not in the range -18 to 18"
        );
        let paris = TimeZone::parse("Europe/Paris").unwrap();
        // 2020-03-29T01:00:00Z: Paris moves from +01:00 to +02:00.
        assert_eq!(paris.offset_at(1_585_443_599_000), 3600);
        assert_eq!(paris.offset_at(1_585_443_600_000), 7200);
        assert_eq!(paris.next_transition(1_585_000_000_000), Some(1_585_443_600_000));
        // 02:30 local doesn't exist that day: Java moves it to 03:30 (+02:00) = 01:30Z.
        let local = NaiveDate::from_ymd_opt(2020, 3, 29).unwrap().and_hms_opt(2, 30, 0).unwrap().and_utc().timestamp_millis();
        assert_eq!(paris.local_to_utc(local), 1_585_445_400_000);
    }

    #[test]
    fn dates() {
        let f = builtin_date_formatter(DEFAULT_DATE_FORMAT).unwrap();
        assert_eq!(f.format(0, 0, &TimeZone::utc()), "1970-01-01T00:00:00.000Z");
        let z = TimeZone::parse("+01:00").unwrap();
        assert_eq!(f.format(0, 0, &z), "1970-01-01T01:00:00.000+01:00");
        assert_eq!(f.parse("2020-01-02", 0, false, &TimeZone::utc()).unwrap(), 1_577_923_200_000);
        assert_eq!(f.parse("2020-01-02T00:00:00Z||+1d", 0, false, &TimeZone::utc()).unwrap(), 1_578_009_600_000);
        assert_eq!(f.parse("1000", 0, false, &TimeZone::utc()).unwrap(), -30_610_224_000_000);
        assert_eq!(f.parse("12345", 0, false, &TimeZone::utc()).unwrap(), 12345);
        assert_eq!(f.parse("now/d", 1_578_009_600_123, false, &TimeZone::utc()).unwrap(), 1_578_009_600_000);
        assert_eq!(f.parse("now/d", 1_578_009_600_123, true, &TimeZone::utc()).unwrap(), 1_578_095_999_999);
        let p = builtin_date_formatter("yyyy-MM-dd").unwrap();
        assert_eq!(p.format(1_577_923_200_000, 0, &TimeZone::utc()), "2020-01-02");
    }

    #[test]
    fn decimals() {
        let f = DecimalFmt::new("0.00").unwrap();
        assert_eq!(f.format_double(1.005), "1.00");
        assert_eq!(f.format_double(2.5), "2.50");
        assert_eq!(f.format_double(-3.0), "-3.00");
        assert_eq!(f.format_long(7), "7.00");
        let g = DecimalFmt::new("#,##0.##").unwrap();
        assert_eq!(g.format_double(1234567.891), "1,234,567.89");
        assert_eq!(g.format_double(0.0), "0");
        let h = DecimalFmt::new("#.##").unwrap();
        assert_eq!(h.format_double(0.5), ".5");
        assert_eq!(h.format_double(0.0), "0");
        assert_eq!(h.format_double(0.125), ".12");
        let p = DecimalFmt::new("0.0%").unwrap();
        assert_eq!(p.format_double(0.256), "25.6%");
        let e = DecimalFmt::new("000").unwrap();
        assert_eq!(e.format_long(5), "005");
        assert_eq!(e.parse("012"), Some(12.0));
        let s = DecimalFmt::new("0.###E0").unwrap();
        assert_eq!(s.format_double(12345.0), "1.234E4");
    }

    #[test]
    fn ips() {
        let v4 = parse_ip("192.168.1.1").unwrap();
        assert_eq!(ip_to_string(&v4), "192.168.1.1");
        let v6 = parse_ip("2001:db8::1").unwrap();
        assert_eq!(ip_to_string(&v6), "2001:db8::1");
        assert_eq!(ip_to_string(&parse_ip("::").unwrap()), "::");
        assert_eq!(ip_to_string(&parse_ip("1:0:0:1:0:0:0:1").unwrap()), "1:0:0:1::1");
        assert!(parse_ip("1.2.3").is_err());
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(unbase64("aGVsbG8="), Some(b"hello".to_vec()));
    }
}
