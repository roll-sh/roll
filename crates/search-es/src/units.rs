//! Time values, byte sizes and booleans with Elasticsearch's parsing rules, error texts and
//! human-readable output (`TimeValue`, `ByteSizeValue`, `Booleans`).

use crate::error::EsError;
use crate::json::write::java_double;

/// A duration. `-1` (`MINUS_ONE`) is kept as a distinct value as Elasticsearch does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeValue {
    pub duration: i64,
    pub unit: TimeUnit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeUnit {
    Nanos,
    Micros,
    Millis,
    Seconds,
    Minutes,
    Hours,
    Days,
}

impl TimeUnit {
    fn nanos(self) -> i128 {
        match self {
            TimeUnit::Nanos => 1,
            TimeUnit::Micros => 1_000,
            TimeUnit::Millis => 1_000_000,
            TimeUnit::Seconds => 1_000_000_000,
            TimeUnit::Minutes => 60_000_000_000,
            TimeUnit::Hours => 3_600_000_000_000,
            TimeUnit::Days => 86_400_000_000_000,
        }
    }

    fn suffix(self) -> &'static str {
        match self {
            TimeUnit::Nanos => "nanos",
            TimeUnit::Micros => "micros",
            TimeUnit::Millis => "ms",
            TimeUnit::Seconds => "s",
            TimeUnit::Minutes => "m",
            TimeUnit::Hours => "h",
            TimeUnit::Days => "d",
        }
    }
}

impl TimeValue {
    pub const MINUS_ONE: TimeValue = TimeValue { duration: -1, unit: TimeUnit::Millis };
    pub const ZERO: TimeValue = TimeValue { duration: 0, unit: TimeUnit::Millis };

    pub const fn millis(ms: i64) -> TimeValue {
        TimeValue { duration: ms, unit: TimeUnit::Millis }
    }

    pub const fn seconds(s: i64) -> TimeValue {
        TimeValue { duration: s, unit: TimeUnit::Seconds }
    }

    pub const fn minutes(m: i64) -> TimeValue {
        TimeValue { duration: m, unit: TimeUnit::Minutes }
    }

    pub const fn hours(h: i64) -> TimeValue {
        TimeValue { duration: h, unit: TimeUnit::Hours }
    }

    pub fn nanos(&self) -> i128 {
        self.duration as i128 * self.unit.nanos()
    }

    pub fn as_millis(&self) -> i64 {
        (self.nanos() / 1_000_000).clamp(i64::MIN as i128, i64::MAX as i128) as i64
    }

    /// `TimeValue.parseTimeValue(value, setting)`.
    pub fn parse(value: &str, setting: &str) -> Result<TimeValue, EsError> {
        let normalized = value.to_lowercase();
        let normalized = normalized.trim();
        let (suffix, unit) = if normalized.ends_with("nanos") {
            ("nanos", TimeUnit::Nanos)
        } else if normalized.ends_with("micros") {
            ("micros", TimeUnit::Micros)
        } else if normalized.ends_with("ms") {
            ("ms", TimeUnit::Millis)
        } else if normalized.ends_with('s') {
            ("s", TimeUnit::Seconds)
        } else if value.ends_with('m') {
            // Case-sensitive: `M` would be months.
            ("m", TimeUnit::Minutes)
        } else if normalized.ends_with('h') {
            ("h", TimeUnit::Hours)
        } else if normalized.ends_with('d') {
            ("d", TimeUnit::Days)
        } else if is_minus_one(normalized) {
            return Ok(TimeValue::MINUS_ONE);
        } else if !normalized.is_empty() && normalized.bytes().all(|b| b == b'0') {
            return Ok(TimeValue::ZERO);
        } else {
            return Err(EsError::illegal_argument(format!(
                "failed to parse setting [{setting}] with value [{value}] as a time value: unit is missing or unrecognized"
            )));
        };
        let num = normalized[..normalized.len() - suffix.len()].trim();
        match parse_java_long(num) {
            Some(v) if v < -1 => Err(EsError::illegal_argument(format!(
                "failed to parse setting [{setting}] with value [{value}] as a time value: negative durations are not supported"
            ))),
            Some(v) => Ok(TimeValue { duration: v, unit }),
            None if parse_java_double(num).is_some() => {
                Err(EsError::illegal_argument(format!("failed to parse [{value}], fractional time values are not supported")))
            }
            None => Err(EsError::illegal_argument(format!("failed to parse [{value}]"))
                .caused_by(EsError::java(400, "number_format_exception", format!("For input string: \"{num}\"")))),
        }
    }

    /// `getStringRep()`: the value as it was given (`30s`, `-1`).
    pub fn string_rep(&self) -> String {
        if self.duration < 0 && self.unit == TimeUnit::Millis {
            return self.duration.to_string();
        }
        format!("{}{}", self.duration, self.unit.suffix())
    }

    /// `toString()`: `toHumanReadableString(1)`.
    pub fn human(&self) -> String {
        self.human_with(1)
    }

    pub fn human_with(&self, fraction_pieces: usize) -> String {
        if self.duration < 0 {
            return self.duration.to_string();
        }
        let nanos = self.nanos();
        if nanos == 0 {
            return "0s".into();
        }
        let n = nanos as f64;
        let (value, suffix) = if nanos >= 86_400_000_000_000 {
            (n / 86_400_000_000_000.0, "d")
        } else if nanos >= 3_600_000_000_000 {
            (n / 3_600_000_000_000.0, "h")
        } else if nanos >= 60_000_000_000 {
            (n / 60_000_000_000.0, "m")
        } else if nanos >= 1_000_000_000 {
            (n / 1_000_000_000.0, "s")
        } else if nanos >= 1_000_000 {
            (n / 1_000_000.0, "ms")
        } else if nanos >= 1_000 {
            (n / 1_000.0, "micros")
        } else {
            (n, "nanos")
        };
        format!("{}{suffix}", format_decimal(value, fraction_pieces.min(10)))
    }
}

fn is_minus_one(s: &str) -> bool {
    s.strip_prefix('-').is_some_and(|r| r.ends_with('1') && r[..r.len() - 1].bytes().all(|b| b == b'0'))
}

/// `Long.parseLong`: optional sign, ASCII digits only.
pub fn parse_java_long(s: &str) -> Option<i64> {
    let digits = s.strip_prefix(['-', '+']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<i64>().ok()
}

/// `Integer.parseInt`.
pub fn parse_java_int(s: &str) -> Option<i32> {
    parse_java_long(s).and_then(|v| i32::try_from(v).ok())
}

/// `Double.parseDouble`: trims, accepts a trailing `d`/`f` and the special names.
pub fn parse_java_double(s: &str) -> Option<f64> {
    let t = s.trim();
    let t = t.strip_suffix(['d', 'D', 'f', 'F']).unwrap_or(t);
    match t {
        "NaN" => return Some(f64::NAN),
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    if t.is_empty() || t.contains(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '-' | '+'))) {
        return None;
    }
    t.parse::<f64>().ok()
}

/// `TimeValue.formatDecimal`.
fn format_decimal(value: f64, fraction_pieces: usize) -> String {
    let p = java_double(value);
    let ix = p.find('.').map(|i| i + 1).unwrap_or(p.len());
    let ex = p.find('E');
    let fraction_end = match ex {
        Some(e) => e,
        None => (ix + fraction_pieces).min(p.len()),
    };
    let fraction_value: u64 = p.get(ix..fraction_end).and_then(|f| f.parse().ok()).unwrap_or(0);
    if fraction_value == 0 || fraction_pieces == 0 {
        return match ex {
            Some(e) => format!("{}{}", &p[..ix - 1], &p[e..]),
            None => p[..ix - 1].to_string(),
        };
    }
    let mut frac = String::new();
    let mut truncate = 0;
    for i in 0..fraction_pieces {
        let pos = ix + i;
        if pos >= fraction_end {
            break;
        }
        let c = p.as_bytes()[pos] as char;
        if c == '0' {
            truncate += 1;
        } else {
            truncate = 0;
        }
        frac.push(c);
    }
    frac.truncate(frac.len() - truncate);
    match ex {
        Some(e) => format!("{}{}{}", &p[..ix], frac, &p[e..]),
        None => format!("{}{}", &p[..ix], frac),
    }
}

/// `Strings.format1Decimals`.
pub fn format1_decimals(value: f64, suffix: &str) -> String {
    let p = java_double(value);
    let ix = p.find('.').map(|i| i + 1).unwrap_or(p.len());
    let ec = p.find('E');
    let fraction = p.as_bytes().get(ix).copied().unwrap_or(b'0');
    match (fraction == b'0', ec) {
        (true, Some(e)) => format!("{}{}{suffix}", &p[..ix - 1], &p[e..]),
        (true, None) => format!("{}{suffix}", &p[..ix - 1]),
        (false, Some(e)) => format!("{}{}{}{suffix}", &p[..ix], fraction as char, &p[e..]),
        (false, None) => format!("{}{}{suffix}", &p[..ix], fraction as char),
    }
}

/// A size in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteSize(pub i64);

const KB: i64 = 1024;
const MB: i64 = KB * 1024;
const GB: i64 = MB * 1024;
const TB: i64 = GB * 1024;
const PB: i64 = TB * 1024;

impl ByteSize {
    /// `ByteSizeValue.parseBytesSizeValue(value, setting)`.
    pub fn parse(value: &str, setting: &str) -> Result<ByteSize, EsError> {
        match value {
            "0" | "0b" | "0B" => return Ok(ByteSize(0)),
            "1b" | "1B" => return Ok(ByteSize(1)),
            "-1" | "-1b" | "-1B" => return Ok(ByteSize(-1)),
            _ => {}
        }
        let lower = value.to_lowercase();
        let lower = lower.trim();
        let units: [(&str, i64); 10] =
            [("k", KB), ("kb", KB), ("m", MB), ("mb", MB), ("g", GB), ("gb", GB), ("t", TB), ("tb", TB), ("p", PB), ("pb", PB)];
        for (suffix, mult) in units {
            if lower.ends_with(suffix) && !(suffix.len() == 1 && lower.ends_with(&format!("{suffix}b"))) {
                let num = lower[..lower.len() - suffix.len()].trim();
                return parse_scaled(num, mult, setting, value);
            }
        }
        if let Some(num) = lower.strip_suffix('b') {
            return match parse_java_long(num.trim()) {
                Some(v) if v >= -1 => Ok(ByteSize(v)),
                Some(_) => Err(EsError::parse(format!("failed to parse setting [{setting}] with value [{value}] as a size in bytes"))),
                None => Err(EsError::parse(format!("failed to parse setting [{setting}] with value [{value}]")).caused_by(EsError::number_format(num.trim()))),
            };
        }
        Err(EsError::parse(format!("failed to parse setting [{setting}] with value [{value}] as a size in bytes: unit is missing or unrecognized")))
    }

    pub fn bytes(&self) -> i64 {
        self.0
    }

    /// `ByteSizeValue.toString()`: `1kb`, `1.5mb`, `10b`.
    pub fn human(&self) -> String {
        let b = self.0;
        let v = b as f64;
        let (value, suffix) = if b >= PB {
            (v / PB as f64, "pb")
        } else if b >= TB {
            (v / TB as f64, "tb")
        } else if b >= GB {
            (v / GB as f64, "gb")
        } else if b >= MB {
            (v / MB as f64, "mb")
        } else if b >= KB {
            (v / KB as f64, "kb")
        } else {
            (v, "b")
        };
        format1_decimals(value, suffix)
    }
}

fn parse_scaled(num: &str, mult: i64, setting: &str, value: &str) -> Result<ByteSize, EsError> {
    let bad = || EsError::parse(format!("failed to parse setting [{setting}] with value [{value}] as a size in bytes"));
    if let Some(v) = parse_java_long(num) {
        if v < -1 {
            return Err(bad());
        }
        return v.checked_mul(mult).map(ByteSize).ok_or_else(bad);
    }
    // A decimal with at most two fractional digits, rounded up.
    let plain = || EsError::parse(format!("failed to parse setting [{setting}] with value [{value}]"));
    let (int, frac) = num.split_once('.').unwrap_or((num, ""));
    if num.starts_with('-') {
        return Err(plain());
    }
    if int.is_empty() && frac.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return Err(plain());
    }
    if frac.len() > 2 {
        return Err(EsError::parse(format!("failed to parse setting [{setting}] with more than two decimals in value [{value}]")));
    }
    let int: i128 = if int.is_empty() { 0 } else { int.parse().map_err(|_| plain())? };
    let scale = 10i128.pow(frac.len() as u32);
    let frac_v: i128 = if frac.is_empty() { 0 } else { frac.parse().map_err(|_| plain())? };
    let total = (int * scale + frac_v) * mult as i128;
    let bytes = (total + scale - 1) / scale;
    i64::try_from(bytes)
        .map(ByteSize)
        .map_err(|_| EsError::parse(format!("failed to parse setting [{setting}] with value beyond {}: [{value}]", i64::MAX)))
}

/// Adds the `?human` variants Elasticsearch writes next to raw size and time fields
/// (`XContentBuilder.humanReadableField`): `store_size` before `store_size_in_bytes`,
/// `query_time` before `query_time_in_millis`, dates before `*_date_millis`/`start_time_in_millis`.
pub fn humanize(v: &mut crate::json::Value) {
    use crate::json::{Map, Value};
    match v {
        Value::Obj(m) => {
            let old = std::mem::take(m);
            let mut out = Map::with_capacity(old.len() + 4);
            for (k, mut val) in old {
                humanize(&mut val);
                if let Some(n) = val.as_i64() {
                    let human = if let Some(base) = k.strip_suffix("_in_bytes") {
                        Some((base.to_string(), ByteSize(n).human()))
                    } else if k.contains("date") || k == "start_time_in_millis" || k == "stop_time_in_millis" {
                        let base = k.trim_end_matches("_in_millis").trim_end_matches("_millis").to_string();
                        (base != k).then(|| (base, crate::time::DateFormatter::new("strict_date_time").map(|f| f.format_millis(n)).unwrap_or_default()))
                    } else if let Some(base) = k.strip_suffix("_in_millis").or_else(|| k.strip_suffix("_millis")) {
                        Some((base.to_string(), TimeValue::millis(n).human()))
                    } else {
                        k.strip_suffix("_in_nanos")
                            .or_else(|| k.strip_suffix("_nanos"))
                            .map(|base| (base.to_string(), TimeValue { duration: n, unit: TimeUnit::Nanos }.human()))
                    };
                    if let Some((hk, hv)) = human
                        && !hk.is_empty()
                        && !out.contains_key(&hk)
                    {
                        out.insert(hk, Value::Str(hv));
                    }
                }
                out.insert(k, val);
            }
            *m = out;
        }
        Value::Arr(a) => a.iter_mut().for_each(humanize),
        _ => {}
    }
}

/// `Booleans.parseBoolean(value)`: only `true` and `false`.
pub fn parse_bool(value: &str) -> Result<bool, EsError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(EsError::illegal_argument(format!("Failed to parse value [{value}] as only [true] or [false] are allowed."))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_values() {
        assert_eq!(TimeValue::parse("30s", "t").unwrap(), TimeValue::seconds(30));
        assert_eq!(TimeValue::parse("1M", "t").unwrap_err().reason(), "failed to parse setting [t] with value [1M] as a time value: unit is missing or unrecognized");
        assert_eq!(TimeValue::parse("-1", "t").unwrap(), TimeValue::MINUS_ONE);
        assert_eq!(TimeValue::parse("000", "t").unwrap(), TimeValue::ZERO);
        assert_eq!(TimeValue::parse("1.5s", "t").unwrap_err().reason(), "failed to parse [1.5s], fractional time values are not supported");
        assert_eq!(TimeValue::parse("xs", "t").unwrap_err().reason(), "failed to parse [xs]");
        assert_eq!(TimeValue::parse("-2s", "t").unwrap_err().reason(), "failed to parse setting [t] with value [-2s] as a time value: negative durations are not supported");
        assert_eq!(TimeValue::parse("5", "master_timeout").unwrap_err().reason(), "failed to parse setting [master_timeout] with value [5] as a time value: unit is missing or unrecognized");
        assert_eq!(TimeValue::parse("100nanos", "t").unwrap().nanos(), 100);
    }

    #[test]
    fn human_times() {
        assert_eq!(TimeValue::millis(1500).human(), "1.5s");
        assert_eq!(TimeValue::millis(60_000).human(), "1m");
        assert_eq!(TimeValue::millis(0).human(), "0s");
        assert_eq!(TimeValue::seconds(30).human(), "30s");
        assert_eq!(TimeValue::millis(100).human(), "100ms");
        assert_eq!(TimeValue { duration: 1300, unit: TimeUnit::Micros }.human(), "1.3ms");
        assert_eq!(TimeValue::seconds(30).string_rep(), "30s");
        assert_eq!(TimeValue::MINUS_ONE.string_rep(), "-1");
    }

    #[test]
    fn byte_sizes() {
        assert_eq!(ByteSize::parse("1kb", "s").unwrap(), ByteSize(1024));
        assert_eq!(ByteSize::parse("1.5mb", "s").unwrap(), ByteSize(1572864));
        assert_eq!(ByteSize::parse("1.1kb", "s").unwrap(), ByteSize(1127));
        assert_eq!(ByteSize::parse("10b", "s").unwrap(), ByteSize(10));
        assert_eq!(ByteSize::parse("10", "s").unwrap_err().reason(), "failed to parse setting [s] with value [10] as a size in bytes: unit is missing or unrecognized");
        assert_eq!(ByteSize::parse("1.123kb", "s").unwrap_err().reason(), "failed to parse setting [s] with more than two decimals in value [1.123kb]");
        assert_eq!(ByteSize(1024).human(), "1kb");
        assert_eq!(ByteSize(1536).human(), "1.5kb");
        assert_eq!(ByteSize(225).human(), "225b");
        assert_eq!(ByteSize(5 * 1024 * 1024 * 1024).human(), "5gb");
    }

    #[test]
    fn humanized_fields() {
        let mut v = crate::obj! { "store" => crate::obj! { "size_in_bytes" => 1536 }, "query_time_in_millis" => 1500, "count" => 3 };
        humanize(&mut v);
        assert_eq!(v.to_json(), r#"{"store":{"size":"1.5kb","size_in_bytes":1536},"query_time":"1.5s","query_time_in_millis":1500,"count":3}"#);
    }

    #[test]
    fn booleans() {
        assert!(parse_bool("true").unwrap());
        assert_eq!(parse_bool("yes").unwrap_err().reason(), "Failed to parse value [yes] as only [true] or [false] are allowed.");
    }
}
