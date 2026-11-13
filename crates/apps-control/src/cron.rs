//! Cron services' schedules: five fields (or `@hourly`, `@daily` …), UTC, minute resolution.

use crate::{Error, Result};
use chrono::{TimeZone, Utc};
use croner::Cron;
use croner::parser::{CronParser, Seconds};

#[derive(Clone, Debug)]
pub struct Schedule(Box<Cron>);

impl Schedule {
    pub fn parse(s: &str) -> Result<Schedule> {
        let s = s.trim();
        if s.split_whitespace().count() > 5 {
            return Err(Error::BadRequest("run.schedule has five fields (minute hour day month weekday), in UTC".into()));
        }
        let cron = CronParser::builder()
            .seconds(Seconds::Disallowed)
            .build()
            .parse(s)
            .map_err(|e| Error::BadRequest(format!("run.schedule {s:?}: {e}")))?;
        Ok(Schedule(Box::new(cron)))
    }

    /// The first time it fires strictly after `after_ms` (unix milliseconds).
    pub fn next_after(&self, after_ms: i64) -> Option<i64> {
        let start = Utc.timestamp_millis_opt(after_ms.div_euclid(1000) * 1000).single()?;
        self.0.find_next_occurrence(&start, false).ok().map(|t| t.timestamp_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_fields_in_utc() {
        let s = Schedule::parse("*/15 * * * *").unwrap();
        // 2026-01-01T00:07:00Z → 00:15.
        let t = Utc.with_ymd_and_hms(2026, 1, 1, 0, 7, 0).unwrap().timestamp_millis();
        assert_eq!(s.next_after(t), Some(t + 8 * 60_000));
        assert!(Schedule::parse("0 0 * * * *").is_err(), "no seconds field");
        assert!(Schedule::parse("@daily").is_ok());
        assert!(Schedule::parse("nope").is_err());
    }
}
