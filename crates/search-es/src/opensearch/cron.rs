//! Unix cron expressions (`minute hour day-of-month month day-of-week`) as the OpenSearch job
//! scheduler's `CronSchedule` reads them (cron-utils' UNIX definition): the next execution
//! time after an instant, in a time zone.
//!
//! Each field becomes a bit set; the search walks days (a day matches by month and by
//! day-of-month or day-of-week, ORed when both are restricted, as Vixie cron does) and takes
//! the first matching hour and minute, so it costs a few thousand set probes at most, never a
//! minute-by-minute scan.

use chrono::{Datelike, NaiveDate, TimeZone, Timelike};

struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    days_star: bool,
    weekdays_star: bool,
}

const MONTHS: [&str; 12] = ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"];
const WEEKDAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

/// A field's bit set over `lo..=hi`, and whether it was `*`.
fn field(text: &str, lo: u32, hi: u32, names: &[&str]) -> Option<(u64, bool)> {
    let value = |s: &str| -> Option<u32> {
        let up = s.to_ascii_uppercase();
        if let Some(i) = names.iter().position(|n| *n == up) {
            return Some(i as u32 + if names.len() == 12 { 1 } else { 0 });
        }
        s.parse().ok()
    };
    let mut bits = 0u64;
    let mut star = false;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().ok().filter(|s| *s > 0)?),
            None => (part, 1),
        };
        let (a, b) = if range == "*" || range == "?" {
            star = star || step == 1;
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a)?, value(b)?)
        } else {
            let a = value(range)?;
            (a, if part.contains('/') { hi } else { a })
        };
        if a < lo || b > hi || a > b {
            return None;
        }
        let mut v = a;
        while v <= b {
            bits |= 1 << v;
            v += step;
        }
    }
    Some((bits, star))
}

fn parse(expr: &str) -> Option<Cron> {
    let f: Vec<&str> = expr.split_whitespace().collect();
    if f.len() != 5 {
        return None;
    }
    let (minutes, _) = field(f[0], 0, 59, &[])?;
    let (hours, _) = field(f[1], 0, 23, &[])?;
    let (days, days_star) = field(f[2], 1, 31, &[])?;
    let (months, _) = field(f[3], 1, 12, &MONTHS)?;
    let (mut weekdays, weekdays_star) = field(f[4], 0, 7, &WEEKDAYS)?;
    // 7 is Sunday too.
    if weekdays & (1 << 7) != 0 {
        weekdays |= 1;
    }
    Some(Cron { minutes, hours: hours as u32, days: days as u32, months: months as u16, weekdays: weekdays as u8, days_star, weekdays_star })
}

impl Cron {
    fn day_matches(&self, d: NaiveDate) -> bool {
        if self.months & (1 << d.month()) == 0 {
            return false;
        }
        let dom = self.days & (1 << d.day()) != 0;
        let dow = self.weekdays & (1 << d.weekday().num_days_from_sunday()) != 0;
        match (self.days_star, self.weekdays_star) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            (false, false) => dom || dow,
        }
    }
}

/// The first time strictly after `after_ms` (epoch millis) the expression fires in `zone`, or
/// `None` for an invalid expression or zone.
pub fn next_after(expr: &str, zone: &str, after_ms: i64) -> Option<i64> {
    let cron = parse(expr)?;
    let tz: chrono_tz::Tz = zone.parse().ok()?;
    let start = tz.timestamp_millis_opt(after_ms).single()?;
    // The next whole minute after `after_ms`.
    let first = start.naive_local().with_second(0)?.with_nanosecond(0)? + chrono::Duration::minutes(1);
    let mut day = first.date();
    for _ in 0..(366 * 5) {
        if cron.day_matches(day) {
            let from = if day == first.date() { (first.hour(), first.minute()) } else { (0, 0) };
            for h in from.0..24 {
                if cron.hours & (1 << h) == 0 {
                    continue;
                }
                let m0 = if h == from.0 { from.1 } else { 0 };
                for m in m0..60 {
                    if cron.minutes & (1 << m) != 0 {
                        // A local time a DST gap skips doesn't fire.
                        let local = day.and_hms_opt(h, m, 0)?;
                        if let Some(t) = tz.from_local_datetime(&local).earliest() {
                            return Some(t.timestamp_millis());
                        }
                    }
                }
            }
        }
        day = day.succ_opt()?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis()
    }

    #[test]
    fn next_times() {
        assert_eq!(next_after("* * * * *", "UTC", at("2026-01-01T00:00:30Z")), Some(at("2026-01-01T00:01:00Z")));
        assert_eq!(next_after("0 * * * *", "UTC", at("2026-01-01T00:00:00Z")), Some(at("2026-01-01T01:00:00Z")));
        assert_eq!(next_after("*/15 9-17 * * MON-FRI", "UTC", at("2026-01-03T12:00:00Z")), Some(at("2026-01-05T09:00:00Z")));
        assert_eq!(next_after("30 2 1 JAN *", "UTC", at("2026-01-01T03:00:00Z")), Some(at("2027-01-01T02:30:00Z")));
        // Day of month and day of week restricted: either fires.
        assert_eq!(next_after("0 0 15 * 0", "UTC", at("2026-01-01T00:00:00Z")), Some(at("2026-01-04T00:00:00Z")));
        assert_eq!(next_after("0 0 * * 7", "UTC", at("2026-01-01T00:00:00Z")), Some(at("2026-01-04T00:00:00Z")));
        assert_eq!(next_after("0 9 * * *", "America/New_York", at("2026-01-01T00:00:00Z")), Some(at("2026-01-01T14:00:00Z")));
        assert_eq!(next_after("0 0 31 2 *", "UTC", 0), None);
        assert_eq!(next_after("bad", "UTC", 0), None);
        assert_eq!(next_after("61 * * * *", "UTC", 0), None);
        assert_eq!(next_after("* * * * *", "Nowhere/Zone", 0), None);
    }
}
