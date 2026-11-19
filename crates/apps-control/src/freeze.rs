//! Deploy freezes (GitLab's deploy freeze periods, a change window; docs/apps/README.md, "Deploy
//! freezes"): an environment's `freeze: {until?, windows?, timezone?, reason?}` holds its
//! automatic deploys while it's on (pushes, tags, hooks and the Git watch build to `ready`, as a
//! person's hold does) and refuses a person's deploy, redeploy or promotion unless it says
//! `override: true` (written in the deployment's message and the audit log). A rollback is never
//! frozen: it's the way out. On until `until` (unix ms), or in any of its weekly `windows`
//! (`{from: "fri 17:00", to: "mon 09:00"}` in its time zone), or always when it names neither.

use crate::{Error, Result};
use chrono::{Datelike, TimeZone, Timelike};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Freeze {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<Window>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

/// A weekly span: from a day and time to another, wrapping past Sunday.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub from: String,
    pub to: String,
}

const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// Minutes into the week (Monday 00:00 is 0) of `"fri 17:00"`.
fn week_minute(s: &str) -> Option<u32> {
    let (d, t) = s.trim().split_once(' ')?;
    let day = DAYS.iter().position(|x| d.eq_ignore_ascii_case(x) || d.to_ascii_lowercase().starts_with(x))? as u32;
    let (h, m) = t.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(day * 1440 + h * 60 + m)
}

impl Freeze {
    pub fn check(&self) -> Result<()> {
        let bad = |m: &str| Err(Error::BadRequest(format!("freeze: {m}")));
        if self.windows.len() > 14 {
            return bad("at most 14 windows");
        }
        if self.windows.iter().any(|w| week_minute(&w.from).is_none() || week_minute(&w.to).is_none() || w.from == w.to) {
            return bad("a window is {from: \"fri 17:00\", to: \"mon 09:00\"} (a day and a 24-hour time each, not the same)");
        }
        if let Some(tz) = &self.timezone {
            crate::cron::zone(tz)?;
        }
        if self.reason.chars().count() > 200 {
            return bad("reason is at most 200 characters");
        }
        Ok(())
    }

    /// Whether it's on at `now` (unix ms): before `until`, in a window, or always with neither.
    pub fn on(&self, now: i64) -> bool {
        if self.until.is_none() && self.windows.is_empty() {
            return true;
        }
        if self.until.is_some_and(|u| now < u) {
            return true;
        }
        let tz = self.timezone.as_deref().and_then(|z| crate::cron::zone(z).ok()).unwrap_or(chrono_tz::UTC);
        let Some(t) = tz.timestamp_millis_opt(now).single() else { return false };
        let at = t.weekday().num_days_from_monday() * 1440 + t.hour() * 60 + t.minute();
        self.windows.iter().any(|w| match (week_minute(&w.from), week_minute(&w.to)) {
            (Some(a), Some(b)) if a < b => (a..b).contains(&at),
            (Some(a), Some(b)) => at >= a || at < b,
            _ => false,
        })
    }

    /// What a refusal or the page says.
    pub fn says(&self) -> String {
        let why = if self.reason.is_empty() { String::new() } else { format!(": {}", self.reason) };
        format!("deploys are frozen{why}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_freeze_is_on_in_its_windows_and_until_its_end() {
        let ms = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis();
        let weekend = Freeze { until: None, windows: vec![Window { from: "fri 17:00".into(), to: "mon 09:00".into() }], timezone: Some("Europe/Budapest".into()), reason: String::new() };
        weekend.check().unwrap();
        // 2026-11-20 is a Friday; Budapest is UTC+1 in November.
        assert!(!weekend.on(ms("2026-11-20T15:59:00Z")), "16:59 there");
        assert!(weekend.on(ms("2026-11-20T16:00:00Z")), "17:00 there");
        assert!(weekend.on(ms("2026-11-22T12:00:00Z")), "Sunday");
        assert!(!weekend.on(ms("2026-11-23T08:00:00Z")), "Monday 09:00 there: open");
        let until = Freeze { until: Some(1000), windows: vec![], timezone: None, reason: "launch".into() };
        assert!(until.on(999) && !until.on(1000));
        assert!(Freeze { until: None, windows: vec![], timezone: None, reason: String::new() }.on(0), "neither: on until lifted");
        assert!(Freeze { until: None, windows: vec![Window { from: "fri 25:00".into(), to: "mon 09:00".into() }], timezone: None, reason: String::new() }.check().is_err());
        assert_eq!(until.says(), "deploys are frozen: launch");
    }
}
