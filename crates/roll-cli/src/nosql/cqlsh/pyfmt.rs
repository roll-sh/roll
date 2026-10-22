//! The bits of Python's number and date formatting cqlsh's output depends on: `'%.*g'`,
//! `repr(float)`, `timedelta(seconds=float)`'s rounding and `str(datetime)`.

use chrono::{DateTime, Datelike, NaiveDateTime, Offset, TimeZone, Timelike, Utc};

/// `(mantissa digits, exponent)` with `x = d.ddd × 10^exp`, from Rust's formatting of `x` with
/// `prec` digits after the point (`None`: the shortest round-tripping digits).
fn sci(x: f64, prec: Option<usize>) -> (String, i32) {
    let s = match prec {
        Some(p) => format!("{:.*e}", p, x.abs()),
        None => format!("{:e}", x.abs()),
    };
    let (m, e) = s.split_once('e').unwrap_or((&s, "0"));
    (m.replace('.', ""), e.parse().unwrap_or(0))
}

fn strip_zeros(s: &str) -> &str {
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.') } else { s }
}

/// Python's `'%.*g' % (precision, x)` for finite `x`.
pub fn percent_g(x: f64, precision: usize) -> String {
    let p = precision.max(1);
    let sign = if x.is_sign_negative() { "-" } else { "" };
    if x == 0.0 {
        return format!("{sign}0");
    }
    let (m, e) = sci(x, Some(p - 1));
    if (-4..p as i32).contains(&e) {
        let decimals = (p as i32 - 1 - e).max(0) as usize;
        format!("{sign}{}", strip_zeros(&format!("{:.*}", decimals, x.abs())))
    } else {
        let mant = if m.len() > 1 { format!("{}.{}", &m[..1], &m[1..]) } else { m };
        let exp = if e < 0 { format!("-{:02}", -e) } else { format!("+{e:02}") };
        format!("{sign}{}e{exp}", strip_zeros(&mant))
    }
}

/// Python's `repr(float)` / `str(float)`.
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let sign = if x.is_sign_negative() { "-" } else { "" };
    if x == 0.0 {
        return format!("{sign}0.0");
    }
    let (digits, e) = sci(x, None);
    let decpt = e + 1;
    if -4 < decpt && decpt <= 16 {
        let n = digits.len() as i32;
        let body = if decpt <= 0 {
            format!("0.{}{digits}", "0".repeat((-decpt) as usize))
        } else if decpt >= n {
            format!("{digits}{}.0", "0".repeat((decpt - n) as usize))
        } else {
            format!("{}.{}", &digits[..decpt as usize], &digits[decpt as usize..])
        };
        format!("{sign}{body}")
    } else {
        let mant = if digits.len() > 1 { format!("{}.{}", &digits[..1], &digits[1..]) } else { digits };
        let exp = if e < 0 { format!("-{:02}", -e) } else { format!("+{e:02}") };
        format!("{sign}{mant}e{exp}")
    }
}

fn round_half_even(x: f64) -> f64 {
    let r = x.round();
    if (r - x).abs() == 0.5 { 2.0 * (x / 2.0).round() } else { r }
}

/// Microseconds since the epoch of `timedelta(seconds=secs)`, with CPython's rounding.
pub fn timedelta_micros(secs: f64) -> i64 {
    let ip = secs.trunc();
    let us = (secs - ip) * 1e6;
    let whole = us.trunc();
    ip as i64 * 1_000_000 + whole as i64 + round_half_even(us - whole) as i64
}

/// `datetime.utcfromtimestamp(secs)` (round half even to microseconds).
pub fn utc_micros_from_float(secs: f64) -> i64 {
    let ip = secs.floor();
    let us = round_half_even((secs - ip) * 1e6);
    ip as i64 * 1_000_000 + us as i64
}

/// The timezone values are shown in: `$TZ` when set and known, UTC otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    Utc,
    Named(chrono_tz::Tz),
}

impl Zone {
    pub fn from_env() -> Zone {
        match std::env::var("TZ").ok().and_then(|t| t.trim_start_matches(':').parse::<chrono_tz::Tz>().ok()) {
            Some(tz) => Zone::Named(tz),
            None => Zone::Utc,
        }
    }

    /// Local wall time and the UTC offset in seconds.
    fn local(&self, utc: NaiveDateTime) -> (NaiveDateTime, i32) {
        match self {
            Zone::Utc => (utc, 0),
            Zone::Named(tz) => {
                let off = tz.offset_from_utc_datetime(&utc).fix().local_minus_utc();
                (utc + chrono::Duration::seconds(off as i64), off)
            }
        }
    }
}

/// Python's datetime range (years 1..=9999).
fn in_range(dt: &NaiveDateTime) -> bool {
    (1..=9999).contains(&dt.year())
}

fn micros_to_naive(us: i64) -> Option<NaiveDateTime> {
    DateTime::<Utc>::from_timestamp_micros(us).map(|d| d.naive_utc()).filter(in_range)
}

fn offset_z(off: i32) -> String {
    let sign = if off < 0 { '-' } else { '+' };
    let a = off.abs();
    format!("{sign}{:02}{:02}", a / 3600, a / 60 % 60)
}

/// cqlsh's `timestamp` display (`%Y-%m-%d %H:%M:%S.%f%z`), through the Python driver's float
/// seconds; out-of-range values print as milliseconds. `millis_only` is COPY TO's rounding.
pub fn cqlsh_timestamp(ms: i64, zone: Zone, millis_only: bool) -> String {
    let us = timedelta_micros(ms as f64 / 1000.0);
    let Some(utc) = micros_to_naive(us) else { return ms.to_string() };
    let (dt, off) = zone.local(utc);
    let frac = if millis_only {
        format!("{:03}", dt.nanosecond() / 1_000_000)
    } else {
        format!("{:06}", dt.nanosecond() / 1000)
    };
    format!(
        "{}-{:02}-{:02} {:02}:{:02}:{:02}.{frac}{}",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        offset_z(off)
    )
}

/// `str(datetime)` of a naive or (`aware`) UTC-aware datetime, `us` microseconds since the epoch.
pub fn datetime_str(us: i64, aware: bool) -> String {
    let Some(dt) = DateTime::<Utc>::from_timestamp_micros(us).map(|d| d.naive_utc()) else { return us.to_string() };
    let mut s = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    );
    let micro = dt.nanosecond() / 1000;
    if micro != 0 {
        s.push_str(&format!(".{micro:06}"));
    }
    if aware {
        s.push_str("+00:00");
    }
    s
}

/// The local UTC offset now, in microseconds (cqlsh's `datetime_from_utc_to_local`).
pub fn local_offset_micros() -> i64 {
    chrono::Local::now().offset().local_minus_utc() as i64 * 1_000_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g_format() {
        assert_eq!(percent_g(std::f64::consts::PI, 5), "3.1416");
        assert_eq!(percent_g(123456.7, 5), "1.2346e+05");
        assert_eq!(percent_g(1e-5, 5), "1e-05");
        assert_eq!(percent_g(0.0001, 5), "0.0001");
        assert_eq!(percent_g(-0.0, 5), "-0");
        assert_eq!(percent_g(99999.5, 10), "99999.5");
        assert_eq!(percent_g(1e300, 5), "1e+300");
        assert_eq!(percent_g(1.1, 5), "1.1");
        assert_eq!(percent_g(1.0, 1), "1");
    }

    #[test]
    fn repr() {
        assert_eq!(float_repr(14.0 / 12.0), "1.1666666666666667");
        assert_eq!(float_repr(2.0), "2.0");
        assert_eq!(float_repr(14706007008009.0 / 3600e9), "4.085001946669166");
        assert_eq!(float_repr(1e16), "1e+16");
        assert_eq!(float_repr(1234567890123456.0), "1234567890123456.0");
        assert_eq!(float_repr(0.0001), "0.0001");
        assert_eq!(float_repr(0.00001), "1e-05");
        assert_eq!(float_repr(-0.5), "-0.5");
    }

    #[test]
    fn timestamps() {
        assert_eq!(cqlsh_timestamp(1_792_497_600_123, Zone::Utc, false), "2026-10-20 12:00:00.123000+0000");
        // The driver goes through float seconds: this is what cqlsh prints.
        assert_eq!(cqlsh_timestamp(253_402_300_799_999, Zone::Utc, false), "9999-12-31 23:59:59.998993+0000");
        assert_eq!(cqlsh_timestamp(253_402_300_800_000, Zone::Utc, false), "253402300800000");
        assert_eq!(cqlsh_timestamp(-1500, Zone::Utc, false), "1969-12-31 23:59:58.500000+0000");
        assert_eq!(cqlsh_timestamp(1_792_497_600_123, Zone::Utc, true), "2026-10-20 12:00:00.123+0000");
        let tz = Zone::Named("Europe/Budapest".parse().unwrap());
        assert_eq!(cqlsh_timestamp(1_792_497_600_000, tz, false), "2026-10-20 14:00:00.000000+0200");
        assert_eq!(datetime_str(1_792_497_600_000_000, false), "2026-10-20 12:00:00");
        assert_eq!(datetime_str(1_792_497_600_000_001, true), "2026-10-20 12:00:00.000001+00:00");
        assert_eq!(utc_micros_from_float(0.5), 500_000);
    }
}
