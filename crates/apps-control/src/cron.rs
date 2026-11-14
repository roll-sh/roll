//! Cron services' schedules: five fields (or `@hourly`, `@daily` …), minute resolution, in UTC
//! or a time zone (`run.timezone`, IANA: its daylight saving as the zone keeps it).

use crate::{Error, Result};
use chrono::TimeZone;
use croner::Cron;
use croner::parser::{CronParser, Seconds};

#[derive(Clone, Debug)]
pub struct Schedule(Box<Cron>, chrono_tz::Tz);

/// An IANA time zone (`Europe/Budapest`), or a 400.
pub fn zone(name: &str) -> Result<chrono_tz::Tz> {
    name.parse::<chrono_tz::Tz>().map_err(|_| Error::BadRequest(format!("run.timezone {name:?} isn't an IANA time zone (Europe/Budapest, America/New_York, UTC)")))
}

impl Schedule {
    /// `tz`: the zone its fields are in (UTC when none).
    pub fn parse(s: &str, tz: Option<&str>) -> Result<Schedule> {
        let tz = tz.map(zone).transpose()?.unwrap_or(chrono_tz::UTC);
        let s = s.trim();
        if s.split_whitespace().count() > 5 {
            return Err(Error::BadRequest("run.schedule has five fields (minute hour day month weekday)".into()));
        }
        let cron = CronParser::builder()
            .seconds(Seconds::Disallowed)
            .build()
            .parse(s)
            .map_err(|e| Error::BadRequest(format!("run.schedule {s:?}: {e}")))?;
        Ok(Schedule(Box::new(cron), tz))
    }

    /// The first time it fires strictly after `after_ms` (unix milliseconds).
    pub fn next_after(&self, after_ms: i64) -> Option<i64> {
        let start = self.1.timestamp_millis_opt(after_ms.div_euclid(1000) * 1000).single()?;
        self.0.find_next_occurrence(&start, false).ok().map(|t| t.timestamp_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_zone_keeps_its_wall_clock_across_daylight_saving() {
        use chrono::Utc;
        let s = Schedule::parse("0 9 * * *", Some("Europe/Budapest")).unwrap();
        // 09:00 in Budapest is 08:00 UTC in winter (CET) and 07:00 in summer (CEST).
        let winter = Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap().timestamp_millis();
        assert_eq!(s.next_after(winter), Some(Utc.with_ymd_and_hms(2026, 3, 1, 8, 0, 0).unwrap().timestamp_millis()));
        let summer = Utc.with_ymd_and_hms(2026, 3, 30, 0, 0, 0).unwrap().timestamp_millis();
        assert_eq!(s.next_after(summer), Some(Utc.with_ymd_and_hms(2026, 3, 30, 7, 0, 0).unwrap().timestamp_millis()));
        assert!(Schedule::parse("0 9 * * *", Some("Mars/Olympus")).is_err());
    }

    #[test]
    fn five_fields_in_utc() {
        use chrono::Utc;
        let s = Schedule::parse("*/15 * * * *", None).unwrap();
        // 2026-01-01T00:07:00Z → 00:15.
        let t = Utc.with_ymd_and_hms(2026, 1, 1, 0, 7, 0).unwrap().timestamp_millis();
        assert_eq!(s.next_after(t), Some(t + 8 * 60_000));
        assert!(Schedule::parse("0 0 * * * *", None).is_err(), "no seconds field");
        assert!(Schedule::parse("@daily", None).is_ok());
        assert!(Schedule::parse("nope", None).is_err());
    }
}
