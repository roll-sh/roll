//! When schedules fire: cron patterns (5 or 6 fields, `L`, `W`, `#`, `@daily` …) in any IANA
//! time zone, or `@every <duration>`.

use crate::model::Ms;
use chrono::{TimeZone, Utc};
use croner::Cron;
use croner::parser::{CronParser, Seconds};

#[derive(Clone, Debug)]
pub enum Spec {
    Cron(Box<Cron>, chrono_tz::Tz),
    Every(i64),
}

pub fn parse(pattern: &str, tz: Option<&str>) -> Result<Spec, String> {
    let pattern = pattern.trim();
    if let Some(every) = pattern.strip_prefix("@every") {
        let ms = parse_duration_ms(every.trim())?;
        if ms < 1000 {
            return Err("@every needs at least 1s".into());
        }
        return Ok(Spec::Every(ms));
    }
    let tz: chrono_tz::Tz = match tz {
        Some(t) if !t.is_empty() => t.parse().map_err(|_| format!("unknown time zone {t:?}"))?,
        _ => chrono_tz::UTC,
    };
    let cron = CronParser::builder()
        .seconds(Seconds::Optional)
        .build()
        .parse(pattern)
        .map_err(|e| format!("bad cron pattern {pattern:?}: {e}"))?;
    Ok(Spec::Cron(Box::new(cron), tz))
}

impl Spec {
    /// The first fire time strictly after `after`.
    pub fn next_after(&self, after: Ms, anchor: Ms) -> Option<Ms> {
        match self {
            Spec::Every(ms) => {
                // Fire on a fixed grid from the anchor, so restarts don't drift.
                let n = (after - anchor).div_euclid(*ms) + 1;
                Some(anchor + n * ms)
            }
            Spec::Cron(cron, tz) => {
                // Whole seconds, so fire times land exactly on the second.
                let after = after.div_euclid(1000) * 1000;
                let start = tz.from_utc_datetime(&Utc.timestamp_millis_opt(after).single()?.naive_utc());
                let next = cron.find_next_occurrence(&start, false).ok()?;
                Some(next.timestamp_millis())
            }
        }
    }

    pub fn upcoming(&self, after: Ms, anchor: Ms, n: usize) -> Vec<Ms> {
        let mut out = Vec::with_capacity(n);
        let mut t = after;
        while out.len() < n {
            match self.next_after(t, anchor) {
                Some(next) => {
                    out.push(next);
                    t = next;
                }
                None => break,
            }
        }
        out
    }
}

/// `"90s"`, `"5m"`, `"1h 30m"`, `"250ms"`, or bare milliseconds `"1500"`.
pub fn parse_duration_ms(s: &str) -> Result<i64, String> {
    let s = s.trim();
    if let Ok(ms) = s.parse::<i64>() {
        return Ok(ms);
    }
    humantime::parse_duration(s).map(|d| d.as_millis() as i64).map_err(|e| format!("bad duration {s:?}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Ms {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis()
    }

    #[test]
    fn five_field_cron() {
        let spec = parse("*/15 * * * *", None).unwrap();
        assert_eq!(spec.next_after(at("2026-01-01T10:07:00Z"), 0), Some(at("2026-01-01T10:15:00Z")));
        assert_eq!(spec.next_after(at("2026-01-01T10:15:00Z"), 0), Some(at("2026-01-01T10:30:00Z")));
        // Fire times are whole seconds whatever the millisecond the search starts from.
        assert_eq!(spec.next_after(at("2026-01-01T10:07:00Z") + 675, 0), Some(at("2026-01-01T10:15:00Z")));
        assert_eq!(spec.next_after(at("2026-01-01T10:15:00Z") + 1, 0), Some(at("2026-01-01T10:30:00Z")));
        let five = spec.upcoming(at("2026-01-01T10:07:00Z") + 431, 0, 5);
        assert!(five.iter().all(|t| t % 1000 == 0), "{five:?}");
    }

    #[test]
    fn seconds_and_nicknames() {
        let spec = parse("*/5 * * * * *", None).unwrap();
        assert_eq!(spec.next_after(at("2026-01-01T10:00:01Z"), 0), Some(at("2026-01-01T10:00:05Z")));
        let spec = parse("@daily", None).unwrap();
        assert_eq!(spec.next_after(at("2026-01-01T10:00:00Z"), 0), Some(at("2026-01-02T00:00:00Z")));
        let spec = parse("0 9 * * MON-FRI", None).unwrap();
        // 2026-01-03 is a Saturday.
        assert_eq!(spec.next_after(at("2026-01-03T12:00:00Z"), 0), Some(at("2026-01-05T09:00:00Z")));
        let spec = parse("0 0 L * *", None).unwrap();
        assert_eq!(spec.next_after(at("2026-02-03T00:00:00Z"), 0), Some(at("2026-02-28T00:00:00Z")));
    }

    #[test]
    fn time_zones_and_dst() {
        let spec = parse("0 9 * * *", Some("America/New_York")).unwrap();
        // EST is UTC-5 in January, EDT UTC-4 in July.
        assert_eq!(spec.next_after(at("2026-01-10T00:00:00Z"), 0), Some(at("2026-01-10T14:00:00Z")));
        assert_eq!(spec.next_after(at("2026-07-10T00:00:00Z"), 0), Some(at("2026-07-10T13:00:00Z")));
        assert!(parse("0 9 * * *", Some("Mars/Base")).is_err());
    }

    #[test]
    fn every() {
        let spec = parse("@every 10s", None).unwrap();
        assert_eq!(spec.next_after(1_005, 1_000), Some(11_000));
        assert_eq!(spec.next_after(11_000, 1_000), Some(21_000));
        assert_eq!(spec.upcoming(1_000, 1_000, 3), vec![11_000, 21_000, 31_000]);
        assert!(parse("@every 10ms", None).is_err());
        assert!(parse("@every soon", None).is_err());
    }

    #[test]
    fn bad_patterns() {
        assert!(parse("* * *", None).is_err());
        assert!(parse("61 * * * *", None).is_err());
        assert!(parse("", None).is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_ms("1500").unwrap(), 1500);
        assert_eq!(parse_duration_ms("2s").unwrap(), 2000);
        assert_eq!(parse_duration_ms("1h 30m").unwrap(), 5_400_000);
        assert_eq!(parse_duration_ms("250ms").unwrap(), 250);
        assert!(parse_duration_ms("x").is_err());
    }
}
