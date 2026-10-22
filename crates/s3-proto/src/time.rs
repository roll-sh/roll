//! Timestamps in the formats S3 uses: ISO 8601 in XML, `YYYYMMDDTHHMMSSZ` in SigV4, HTTP dates
//! in headers.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Days since 1970-01-01 of a proleptic Gregorian date.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `(year, month, day)` of days since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

fn split(secs: i64) -> (i64, u32, u32, i64, i64, i64) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (y, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

/// `2009-10-12T17:50:30.000Z` (XML timestamps).
pub fn iso8601_ms(ms: i64) -> String {
    let (y, mo, d, h, mi, s) = split(ms.div_euclid(1000));
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{:03}Z", ms.rem_euclid(1000))
}

/// `20130524T000000Z` (SigV4 `x-amz-date`).
pub fn amz_date(secs: i64) -> String {
    let (y, mo, d, h, mi, s) = split(secs);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

fn num(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn checked(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> Option<i64> {
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    Some(days_from_civil(y, mo as u32, d as u32) * 86_400 + h * 3600 + mi * 60 + s)
}

/// Unix seconds of `20130524T000000Z`.
pub fn parse_amz_date(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 16 || b[8] != b'T' || b[15] != b'Z' {
        return None;
    }
    checked(num(&s[0..4])?, num(&s[4..6])?, num(&s[6..8])?, num(&s[9..11])?, num(&s[11..13])?, num(&s[13..15])?)
}

/// Unix milliseconds of `2015-12-30T12:00:00.000Z` (fraction optional, `Z` required).
pub fn parse_iso8601(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z')?;
    let (main, frac) = match s.split_once('.') {
        Some((m, f)) => (m, Some(f)),
        None => (s, None),
    };
    let b = main.as_bytes();
    if b.len() != 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let secs = checked(num(&main[0..4])?, num(&main[5..7])?, num(&main[8..10])?, num(&main[11..13])?, num(&main[14..16])?, num(&main[17..19])?)?;
    let ms = match frac {
        Some(f) if !f.is_empty() && f.len() <= 9 => {
            let digits = num(f)?;
            let scale = 10i64.pow(f.len() as u32);
            digits * 1000 / scale
        }
        Some(_) => return None,
        None => 0,
    };
    Some(secs * 1000 + ms)
}

fn system_time(ms: i64) -> SystemTime {
    if ms >= 0 { UNIX_EPOCH + Duration::from_millis(ms as u64) } else { UNIX_EPOCH - Duration::from_millis(ms.unsigned_abs()) }
}

/// `Wed, 21 Oct 2015 07:28:00 GMT` (`Last-Modified`, `Date`).
pub fn http_date(ms: i64) -> String {
    httpdate::fmt_http_date(system_time(ms.div_euclid(1000) * 1000))
}

/// Unix seconds of an HTTP date (any of the three formats HTTP allows).
pub fn parse_http_date(s: &str) -> Option<i64> {
    let t = httpdate::parse_http_date(s.trim()).ok()?;
    Some(match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    })
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        assert_eq!(amz_date(1_369_353_600), "20130524T000000Z");
        assert_eq!(parse_amz_date("20130524T000000Z"), Some(1_369_353_600));
        assert_eq!(parse_amz_date("20130524T000000"), None);
        assert_eq!(parse_amz_date("20131324T000000Z"), None);
        assert_eq!(iso8601_ms(1_255_369_830_000), "2009-10-12T17:50:30.000Z");
        assert_eq!(iso8601_ms(1_255_369_830_042), "2009-10-12T17:50:30.042Z");
        assert_eq!(parse_iso8601("2009-10-12T17:50:30.042Z"), Some(1_255_369_830_042));
        assert_eq!(parse_iso8601("2009-10-12T17:50:30Z"), Some(1_255_369_830_000));
        assert_eq!(parse_iso8601("2009-10-12T17:50:30.5Z"), Some(1_255_369_830_500));
        assert_eq!(parse_iso8601("2009-10-12 17:50:30Z"), None);
        assert_eq!(http_date(1_445_412_480_123), "Wed, 21 Oct 2015 07:28:00 GMT");
        assert_eq!(parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT"), Some(1_445_412_480));
        assert_eq!(parse_http_date("garbage"), None);
        for days in [-800_000i64, -1, 0, 1, 11_016, 20_000, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }
}
