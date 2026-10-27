//! `duration`: months, days and nanoseconds, each signed, all of the same sign.
//!
//! Follows `org.apache.cassandra.cql3.Duration` and `serializers.DurationSerializer`
//! (Apache-2.0): three zigzag vints on the wire, the `1y2mo3w4d5h6m7s8ms9us10ns` form, ISO 8601
//! (`P1Y2M3DT4H5M6S`, `P2W`, `P0001-02-03T04:05:06`) and the canonical text form, which never
//! uses weeks (`1y2mo25d5h6m7s8ms9us10ns`). ScyllaDB accepts and prints the same forms.

use crate::wire::{put_vint, read_vint};
use crate::{Dialect, MarshalError};

pub const NANOS_PER_MICRO: i64 = 1_000;
pub const NANOS_PER_MILLI: i64 = 1_000_000;
pub const NANOS_PER_SECOND: i64 = 1_000_000_000;
pub const NANOS_PER_MINUTE: i64 = 60 * NANOS_PER_SECOND;
pub const NANOS_PER_HOUR: i64 = 60 * NANOS_PER_MINUTE;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Duration {
    pub months: i32,
    pub days: i32,
    pub nanos: i64,
}

impl Duration {
    pub fn new(months: i32, days: i32, nanos: i64) -> Duration {
        Duration {
            months,
            days,
            nanos,
        }
    }

    pub fn is_negative(&self) -> bool {
        self.months < 0 || self.days < 0 || self.nanos < 0
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6);
        put_vint(&mut out, self.months as i64);
        put_vint(&mut out, self.days as i64);
        put_vint(&mut out, self.nanos);
        out
    }

    /// Decodes without validating ranges or signs (see `validate`). `None` if truncated.
    pub fn decode(b: &[u8]) -> Option<Duration> {
        let mut pos = 0;
        let m = read_vint(b, &mut pos)?;
        let d = read_vint(b, &mut pos)?;
        let n = read_vint(b, &mut pos)?;
        Some(Duration {
            months: m as i32,
            days: d as i32,
            nanos: n,
        })
    }

    /// `DurationSerializer.validate` (Cassandra) and ScyllaDB's equivalent.
    pub fn validate(b: &[u8], dialect: Dialect) -> Result<(), MarshalError> {
        let scylla = dialect.is_scylla();
        if b.len() < 3 {
            return Err(if scylla {
                MarshalError::scylla(format!(
                    "Expected at least 3 bytes for a duration, got {}",
                    b.len()
                ))
            } else {
                MarshalError::new(format!(
                    "Expected at least 3 bytes for a duration ({})",
                    b.len()
                ))
            });
        }
        let mut pos = 0;
        let parts = (|| {
            Some((
                read_vint(b, &mut pos)?,
                read_vint(b, &mut pos)?,
                read_vint(b, &mut pos)?,
            ))
        })();
        let Some((m, d, n)) = parts else {
            // Cassandra fails with an internal error on truncated vints; ScyllaDB accepts them.
            return if scylla {
                Ok(())
            } else {
                Err(MarshalError::new("Not enough bytes to read a duration"))
            };
        };
        if i32::try_from(m).is_err() {
            return Err(err(
                scylla,
                format!("The duration months must be a 32 bits integer but was: {m}"),
            ));
        }
        if i32::try_from(d).is_err() {
            return Err(err(
                scylla,
                format!("The duration days must be a 32 bits integer but was: {d}"),
            ));
        }
        if !((m >= 0 && d >= 0 && n >= 0) || (m <= 0 && d <= 0 && n <= 0)) {
            return Err(if scylla {
                MarshalError::scylla(format!(
                    "The duration months, days, and nanoseconds must be all of the same sign ({m}, {d}, {n})"
                ))
            } else {
                MarshalError::new(format!(
                    "The duration months, days and nanoseconds must be all of the same sign ({m}, {d}, {n})"
                ))
            });
        }
        Ok(())
    }

    /// `Duration.from`: the unit form, ISO 8601, ISO week form or ISO alternative form, with an
    /// optional leading `-`.
    pub fn parse(input: &str, dialect: Dialect) -> Result<Duration, MarshalError> {
        let wrap = |m: String| {
            if dialect.is_scylla() {
                MarshalError::scylla(m)
            } else {
                MarshalError::new(m)
            }
        };
        let neg = input.starts_with('-');
        let source = if neg { &input[1..] } else { input };
        let unable = || wrap(format!("Unable to convert '{source}' to a duration"));
        let mut b = Builder {
            neg,
            months: 0,
            days: 0,
            nanos: 0,
            unit: 0,
        };
        if let Some(iso) = source.strip_prefix('P') {
            if let Some(w) = iso.strip_suffix('W') {
                let n = all_digits(w).ok_or_else(unable)?;
                b.add(3, n).map_err(wrap)?;
            } else if source.contains('-') {
                // P(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})
                let s = iso.as_bytes();
                let shape = s.len() == 19
                    && s[4] == b'-'
                    && s[7] == b'-'
                    && s[10] == b'T'
                    && s[13] == b':'
                    && s[16] == b':'
                    && [0..4, 5..7, 8..10, 11..13, 14..16, 17..19]
                        .iter()
                        .all(|r| s[r.clone()].iter().all(u8::is_ascii_digit));
                if !shape {
                    return Err(unable());
                }
                for (unit, r) in [
                    (1, 0..4),
                    (2, 5..7),
                    (4, 8..10),
                    (5, 11..13),
                    (6, 14..16),
                    (7, 17..19),
                ] {
                    b.add(unit, iso[r].parse().unwrap_or(0)).map_err(wrap)?;
                }
            } else {
                // P((\d+)Y)?((\d+)M)?((\d+)D)?(T((\d+)H)?((\d+)M)?((\d+)S)?)?
                let (date, time) = match iso.split_once('T') {
                    Some((d, t)) => (d, Some(t)),
                    None => (iso, None),
                };
                let mut groups = Vec::new();
                parse_iso_groups(date, &[('Y', 1), ('M', 2), ('D', 4)], &mut groups)
                    .ok_or_else(unable)?;
                if let Some(t) = time {
                    parse_iso_groups(t, &[('H', 5), ('M', 6), ('S', 7)], &mut groups)
                        .ok_or_else(unable)?;
                }
                for (unit, n) in groups {
                    b.add(unit, n).map_err(wrap)?;
                }
            }
        } else {
            let s = source.as_bytes();
            let mut i = 0;
            if s.is_empty() {
                return Err(unable());
            }
            while i < s.len() {
                let start = i;
                while i < s.len() && s[i].is_ascii_digit() {
                    i += 1;
                }
                if i == start {
                    return Err(unable());
                }
                let n: i64 = source[start..i].parse().map_err(|_| unable())?;
                let rest = &source[i..];
                let (unit, len) = unit_symbol(rest).ok_or_else(unable)?;
                i += len;
                b.add(unit, n).map_err(wrap)?;
            }
        }
        Ok(b.build())
    }

    /// The canonical text (`Duration.toString`): `-` if negative, then `y`, `mo`, `d`, `h`, `m`,
    /// `s`, `ms`, `us`, `ns` for each non-zero part. Zero prints as the empty string.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        if self.is_negative() {
            out.push('-');
        }
        let months = (self.months as i64).abs();
        let rem = append(&mut out, months, 12, "y");
        append(&mut out, rem, 1, "mo");
        append(&mut out, (self.days as i64).abs(), 1, "d");
        if self.nanos != 0 {
            let mut rem = self.nanos.unsigned_abs() as i128;
            for (div, unit) in [
                (NANOS_PER_HOUR, "h"),
                (NANOS_PER_MINUTE, "m"),
                (NANOS_PER_SECOND, "s"),
                (NANOS_PER_MILLI, "ms"),
                (NANOS_PER_MICRO, "us"),
                (1, "ns"),
            ] {
                let div = div as i128;
                if rem != 0 && rem >= div {
                    out.push_str(&(rem / div).to_string());
                    out.push_str(unit);
                    rem %= div;
                }
            }
        }
        out
    }
}

fn err(scylla: bool, m: String) -> MarshalError {
    if scylla {
        MarshalError::scylla(m)
    } else {
        MarshalError::new(m)
    }
}

fn append(out: &mut String, dividend: i64, divisor: i64, unit: &str) -> i64 {
    if dividend == 0 || dividend < divisor {
        return dividend;
    }
    out.push_str(&(dividend / divisor).to_string());
    out.push_str(unit);
    dividend % divisor
}

fn all_digits(s: &str) -> Option<i64> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

fn parse_iso_groups(mut s: &str, units: &[(char, u8)], out: &mut Vec<(u8, i64)>) -> Option<()> {
    let mut k = 0;
    while !s.is_empty() {
        let end = s.find(|c: char| !c.is_ascii_digit())?;
        if end == 0 {
            return None;
        }
        let sym = s[end..].chars().next()?;
        while k < units.len() && units[k].0 != sym {
            k += 1;
        }
        if k == units.len() {
            return None;
        }
        out.push((units[k].1, s[..end].parse().ok()?));
        k += 1;
        s = &s[end + sym.len_utf8()..];
    }
    Some(())
}

/// The unit at the start of `s`: (unit index, byte length). Longest symbols first.
fn unit_symbol(s: &str) -> Option<(u8, usize)> {
    let lower: String = s.chars().take(2).collect::<String>().to_lowercase();
    for (sym, unit) in [("mo", 2), ("ms", 8), ("us", 9), ("µs", 9), ("ns", 10)] {
        if lower.starts_with(sym) {
            let len: usize = s
                .chars()
                .take(sym.chars().count())
                .map(char::len_utf8)
                .sum();
            return Some((unit, len));
        }
    }
    let c = s.chars().next()?.to_ascii_lowercase();
    let unit = match c {
        'y' => 1,
        'w' => 3,
        'd' => 4,
        'h' => 5,
        'm' => 6,
        's' => 7,
        _ => return None,
    };
    Some((unit, 1))
}

struct Builder {
    neg: bool,
    months: i64,
    days: i64,
    nanos: i64,
    unit: u8,
}

const UNIT_NAMES: [&str; 11] = [
    "",
    "years",
    "months",
    "weeks",
    "days",
    "hours",
    "minutes",
    "seconds",
    "milliseconds",
    "microseconds",
    "nanoseconds",
];

impl Builder {
    fn add(&mut self, unit: u8, n: i64) -> Result<(), String> {
        if unit == self.unit {
            return Err(format!(
                "Invalid duration. The {} are specified multiple times",
                UNIT_NAMES[unit as usize]
            ));
        }
        if unit < self.unit {
            return Err(format!(
                "Invalid duration. The {} should be after {}",
                UNIT_NAMES[self.unit as usize], UNIT_NAMES[unit as usize]
            ));
        }
        self.unit = unit;
        let check = |n: i64, limit: i64, name: &str| {
            if n > limit {
                Err(format!(
                    "Invalid duration. The total number of {name} must be less or equal to 2147483647"
                ))
            } else {
                Ok(())
            }
        };
        match unit {
            1 => {
                check(n, (i32::MAX as i64 - self.months) / 12, "months")?;
                self.months += n * 12;
            }
            2 => {
                check(n, i32::MAX as i64 - self.months, "months")?;
                self.months += n;
            }
            3 => {
                check(n, (i32::MAX as i64 - self.days) / 7, "days")?;
                self.days += n * 7;
            }
            4 => {
                check(n, i32::MAX as i64 - self.days, "days")?;
                self.days += n;
            }
            _ => {
                let per = [
                    NANOS_PER_HOUR,
                    NANOS_PER_MINUTE,
                    NANOS_PER_SECOND,
                    NANOS_PER_MILLI,
                    NANOS_PER_MICRO,
                    1,
                ][(unit - 5) as usize];
                check(n, (i64::MAX - self.nanos) / per, "nanoseconds")?;
                self.nanos += n * per;
            }
        }
        Ok(())
    }

    fn build(self) -> Duration {
        let s = if self.neg { -1 } else { 1 };
        Duration {
            months: (s * self.months) as i32,
            days: (s * self.days) as i32,
            nanos: s * self.nanos,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: Dialect = Dialect::Cassandra;
    const S: Dialect = Dialect::Scylla;

    fn p(s: &str) -> Duration {
        Duration::parse(s, C).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn parse_and_print_like_both_references() {
        // toJson(du) on both images.
        for (s, out) in [
            ("1y2mo3w4d5h6m7s8ms9us10ns", "1y2mo25d5h6m7s8ms9us10ns"),
            ("P1Y2M3DT4H5M6S", "1y2mo3d4h5m6s"),
            ("P2W", "14d"),
            ("-P0001-02-03T04:05:06", "-1y2mo3d4h5m6s"),
            ("89999999mo", "7499999y11mo"),
            ("1h", "1h"),
            ("3µs", "3us"),
            ("3US", "3us"),
            ("1Mo", "1mo"),
            ("90m", "1h30m"),
            ("PT0S", ""),
            ("0d", ""),
            ("-2d", "-2d"),
            ("PT1H", "1h"),
            ("P1Y", "1y"),
        ] {
            let d = p(s);
            assert_eq!(d.to_text(), out, "{s}");
            assert_eq!(Duration::parse(s, S).unwrap(), d);
            assert_eq!(Duration::decode(&d.encode()), Some(d));
        }
        assert_eq!(
            p("1y2mo3w4d5h6m7s8ms9us10ns"),
            Duration::new(14, 25, 18_367_008_009_010)
        );
        assert_eq!(p("-1d"), Duration::new(0, -1, 0));
    }

    #[test]
    fn parse_errors() {
        for (s, m) in [
            ("1h2d", "Invalid duration. The hours should be after days"),
            (
                "1h1h",
                "Invalid duration. The hours are specified multiple times",
            ),
            ("xyz", "Unable to convert 'xyz' to a duration"),
            ("", "Unable to convert '' to a duration"),
            ("1", "Unable to convert '1' to a duration"),
            ("1x", "Unable to convert '1x' to a duration"),
            ("P1Q", "Unable to convert 'P1Q' to a duration"),
            ("P1D1M", "Unable to convert 'P1D1M' to a duration"),
            ("PxW", "Unable to convert 'PxW' to a duration"),
            (
                "P2026-1-01T00:00:00",
                "Unable to convert 'P2026-1-01T00:00:00' to a duration",
            ),
            (
                "2147483648mo",
                "Invalid duration. The total number of months must be less or equal to 2147483647",
            ),
            (
                "178956971y",
                "Invalid duration. The total number of months must be less or equal to 2147483647",
            ),
            (
                "2147483648d",
                "Invalid duration. The total number of days must be less or equal to 2147483647",
            ),
            (
                "9223372036854775807h",
                "Invalid duration. The total number of nanoseconds must be less or equal to 2147483647",
            ),
        ] {
            assert_eq!(Duration::parse(s, C).unwrap_err().message, m, "{s}");
            assert_eq!(
                Duration::parse(s, S).unwrap_err().message,
                format!("marshaling error: {m}"),
                "{s}"
            );
        }
    }

    #[test]
    fn validation_like_both_references() {
        // Raw bound values on both images.
        let c = |b: &[u8]| Duration::validate(b, C).map_err(|e| e.message);
        let s = |b: &[u8]| Duration::validate(b, S).map_err(|e| e.message);
        assert_eq!(
            c(&[]),
            Err("Expected at least 3 bytes for a duration (0)".into())
        );
        assert_eq!(
            c(&[0, 0]),
            Err("Expected at least 3 bytes for a duration (2)".into())
        );
        assert_eq!(
            s(&[]),
            Err("marshaling error: Expected at least 3 bytes for a duration, got 0".into())
        );
        assert_eq!(
            s(&[0, 0]),
            Err("marshaling error: Expected at least 3 bytes for a duration, got 2".into())
        );
        assert_eq!(c(&[0, 0, 1]), Ok(()));
        assert_eq!(c(&[2, 0, 0]), Ok(()));
        assert_eq!(c(&[2, 0, 0, 1]), Ok(()));
        assert_eq!(s(&[0xfe, 0, 0]), Ok(()));
        assert!(c(&[0xfe, 0, 0]).is_err());
        let bad = [0xf0, 0xff, 0xff, 0xff, 0xff, 0x10, 0x00];
        assert_eq!(
            c(&bad),
            Err("The duration months, days and nanoseconds must be all of the same sign (-2147483648, 8, 0)".into())
        );
        assert_eq!(
            s(&bad),
            Err("marshaling error: The duration months, days, and nanoseconds must be all of the same sign (-2147483648, 8, 0)".into())
        );
        let mut big = Vec::new();
        put_vint(&mut big, 1 << 40);
        put_vint(&mut big, 0);
        put_vint(&mut big, 0);
        assert_eq!(
            c(&big),
            Err("The duration months must be a 32 bits integer but was: 1099511627776".into())
        );
        let mut big = Vec::new();
        put_vint(&mut big, 0);
        put_vint(&mut big, -(1 << 40));
        put_vint(&mut big, 0);
        assert_eq!(
            c(&big),
            Err("The duration days must be a 32 bits integer but was: -1099511627776".into())
        );
        assert_eq!(Duration::decode(&[0, 0]), None);
    }
}

#[cfg(test)]
mod more_tests {
    use super::*;

    #[test]
    fn sign_and_codec() {
        assert!(Duration::new(0, -1, 0).is_negative());
        assert!(!Duration::new(1, 0, 0).is_negative());
        assert!(!Duration::default().is_negative());
        assert_eq!(Duration::default().to_text(), "");
        assert_eq!(Duration::new(0, 0, -1).to_text(), "-1ns");
        assert_eq!(Duration::new(-1, 0, 0).encode(), vec![1, 0, 0]);
        assert_eq!(
            Duration::new(i32::MIN, i32::MAX, i64::MIN).to_text(),
            "-178956970y8mo2147483647d2562047h47m16s854ms775us808ns"
        );
        let d = Duration::new(i32::MAX, i32::MAX, i64::MAX);
        assert_eq!(Duration::decode(&d.encode()), Some(d));
    }
}
