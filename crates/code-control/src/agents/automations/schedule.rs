//! When `schedule` and `interval` triggers fire: cron patterns (5 fields, or 6 with seconds;
//! `L`, `W`, `#`, `@daily` …) in an IANA time zone (daylight saving included), and fixed
//! intervals on a grid from the automation's creation, so a restart doesn't drift.

use agent_proto::automation::{Trigger, limits};
use chrono::{TimeZone, Utc};
use croner::Cron;
use croner::parser::{CronParser, Seconds};

pub enum Schedule {
    Cron(Box<Cron>, chrono_tz::Tz),
    Every(i64),
}

impl Schedule {
    /// The trigger's schedule, if it has one; an error says what's wrong with it.
    pub fn of(t: &Trigger) -> Result<Option<Schedule>, String> {
        match t {
            Trigger::Schedule { cron, timezone } => {
                let tz: chrono_tz::Tz = timezone.trim().parse().map_err(|_| format!("unknown time zone {timezone:?} (an IANA name, like Europe/Budapest)"))?;
                let pattern = cron.trim();
                if pattern.is_empty() || pattern.len() > 200 {
                    return Err("cron must be 1 to 200 characters".into());
                }
                let c = CronParser::builder().seconds(Seconds::Optional).build().parse(pattern).map_err(|e| format!("bad cron pattern {pattern:?}: {e}"))?;
                Ok(Some(Schedule::Cron(Box::new(c), tz)))
            }
            Trigger::Interval { every_s } => {
                if *every_s < limits::MIN_INTERVAL_S || *every_s > 366 * 86_400 {
                    return Err(format!("every_s must be {} to {} seconds", limits::MIN_INTERVAL_S, 366 * 86_400));
                }
                Ok(Some(Schedule::Every(*every_s as i64 * 1000)))
            }
            _ => Ok(None),
        }
    }

    /// The first firing strictly after `after` (unix ms); `anchor` is where an interval's grid
    /// starts.
    pub fn next_after(&self, after: i64, anchor: i64) -> Option<i64> {
        match self {
            Schedule::Every(ms) => Some(anchor + ((after - anchor).div_euclid(*ms) + 1) * ms),
            Schedule::Cron(cron, tz) => {
                let after = after.div_euclid(1000) * 1000;
                let start = tz.from_utc_datetime(&Utc.timestamp_millis_opt(after).single()?.naive_utc());
                cron.find_next_occurrence(&start, false).ok().map(|t| t.timestamp_millis())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(s: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis()
    }

    #[test]
    fn cron_in_a_time_zone() {
        let s = Schedule::of(&Trigger::Schedule { cron: "0 3 * * 1-5".into(), timezone: "Europe/Budapest".into() }).unwrap().unwrap();
        // Friday 2026-11-06 03:00 in Budapest (UTC+1) is 02:00 UTC; the next is Monday.
        assert_eq!(s.next_after(ms("2026-11-05T12:00:00Z"), 0), Some(ms("2026-11-06T02:00:00Z")));
        assert_eq!(s.next_after(ms("2026-11-06T02:00:00Z"), 0), Some(ms("2026-11-09T02:00:00Z")));
        // Summer time: 03:00 is 01:00 UTC.
        assert_eq!(s.next_after(ms("2026-07-01T12:00:00Z"), 0), Some(ms("2026-07-02T01:00:00Z")));
        let secs = Schedule::of(&Trigger::Schedule { cron: "*/30 * * * * *".into(), timezone: "UTC".into() }).unwrap().unwrap();
        assert_eq!(secs.next_after(ms("2026-11-05T12:00:10Z"), 0), Some(ms("2026-11-05T12:00:30Z")));
    }

    #[test]
    fn intervals_keep_their_grid() {
        let s = Schedule::of(&Trigger::Interval { every_s: 3600 }).unwrap().unwrap();
        let anchor = ms("2026-11-05T10:15:00Z");
        assert_eq!(s.next_after(ms("2026-11-05T12:00:00Z"), anchor), Some(ms("2026-11-05T12:15:00Z")));
        assert_eq!(s.next_after(ms("2026-11-05T12:15:00Z"), anchor), Some(ms("2026-11-05T13:15:00Z")));
    }

    #[test]
    fn bad_schedules() {
        for t in [
            Trigger::Schedule { cron: "61 * * * *".into(), timezone: "UTC".into() },
            Trigger::Schedule { cron: "0 3 * * *".into(), timezone: "Mars/Olympus".into() },
            Trigger::Schedule { cron: "".into(), timezone: "UTC".into() },
            Trigger::Interval { every_s: 5 },
        ] {
            assert!(Schedule::of(&t).is_err(), "{t:?}");
        }
        assert!(Schedule::of(&Trigger::Manual).unwrap().is_none());
    }
}
