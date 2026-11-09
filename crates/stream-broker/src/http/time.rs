//! RFC 3339 timestamps in UTC, for the Confluent REST API (no calendar crate needed).

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `2024-05-01T12:00:00.123Z`.
pub fn format_ms(ms: i64) -> String {
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let (y, m, d) = civil_from_days(days);
    let (h, rem) = (rem / 3_600_000, rem % 3_600_000);
    let (min, rem) = (rem / 60_000, rem % 60_000);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{:02}.{:03}Z", rem / 1000, rem % 1000)
}

/// Parse RFC 3339 (`Z` or `±hh:mm` offsets, optional fraction) into epoch milliseconds.
pub fn parse_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, sec) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut i = 19;
    let mut frac_ms = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let digits = s.get(start..i)?;
        if digits.is_empty() {
            return None;
        }
        let padded: String = digits.chars().chain(std::iter::repeat('0')).take(3).collect();
        frac_ms = padded.parse::<i64>().ok()?;
    }
    let offset_min = match s.get(i..)? {
        "Z" | "z" => 0,
        rest if rest.len() == 6 && (rest.starts_with('+') || rest.starts_with('-')) && &rest[3..4] == ":" => {
            let sign = if rest.starts_with('-') { -1 } else { 1 };
            sign * (rest[1..3].parse::<i64>().ok()? * 60 + rest[4..6].parse::<i64>().ok()?)
        }
        _ => return None,
    };
    let days = days_from_civil(y, mo, d);
    Some(((days * 86_400 + h * 3600 + mi * 60 + sec) - offset_min * 60) * 1000 + frac_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for ms in [0, 1_700_000_000_123, 951_782_400_000, -86_400_000, 4_102_444_800_000] {
            assert_eq!(parse_ms(&format_ms(ms)), Some(ms), "{}", format_ms(ms));
        }
        assert_eq!(format_ms(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(parse_ms("2021-02-05T19:14:42Z"), Some(1_612_552_482_000));
        assert_eq!(parse_ms("2021-02-05T20:14:42.5+01:00"), Some(1_612_552_482_500));
        assert_eq!(parse_ms("2021-02-05"), None);
        assert_eq!(parse_ms("2021-13-05T19:14:42Z"), None);
    }
}
