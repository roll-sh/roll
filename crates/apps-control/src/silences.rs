//! Alert silences (Cloud Monitoring's snoozes, Better Stack's maintenance windows;
//! docs/apps/README.md, "Silencing alerts"): for a while, an environment's alerts reach no
//! notification target, on-call included. The events are still kept (Events lists them), and a
//! resolve always goes out, so an incident a silence began in still closes. A status page's
//! scheduled maintenance makes one for what the page shows, for its window. A repeating one
//! (`every`: weekly windows in a time zone, the pause schedule's) quiets only inside them: a
//! nightly backup's, a weekly maintenance's.

use crate::{Apps, Error, Result};
use serde_json::{Value, json};
use std::sync::Arc;

/// The most silences open at once in an environment.
pub const MAX_OPEN: i64 = 20;
/// The longest one lasts.
pub const MAX_MS: i64 = 30 * 86_400_000;
/// The longest a repeating one lasts (none given: this long).
pub const MAX_EVERY_MS: i64 = 365 * 86_400_000;

/// What a silence can quiet: the alerts, by their event kind.
pub const KINDS: [&str; 16] = [
    "check.down", "heartbeat.missed", "crash_loop", "alert.memory", "alert.cpu", "alert.metric", "alert.errors", "alert.latency", "alert.capacity", "alert.log", "alert.route", "alert.slo", "alert.anomaly", "alert.csp", "cron.failed", "deploy.failed",
];

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Silence {
    pub id: String,
    /// When it ends (unix ms).
    pub until: i64,
    /// The kinds it quiets; empty: every one of [`KINDS`].
    pub alerts: Vec<String>,
    pub reason: String,
    pub created_at: i64,
    /// A repeating one's weekly windows: it quiets only inside them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub every: Option<crate::freeze::PauseSchedule>,
}

impl Silence {
    /// Whether it quiets an event of `kind` at `at` (a resolve never is: it closes what's open).
    pub fn quiets(&self, kind: &str, at: i64) -> bool {
        KINDS.contains(&kind) && (self.alerts.is_empty() || self.alerts.iter().any(|a| a == kind)) && self.every.as_ref().is_none_or(|e| e.on(at))
    }
}

impl Apps {
    /// `GET …/environments/{env}/silences`: those open now.
    pub async fn silences(self: &Arc<Self>, service: &str, env: &str) -> Result<Vec<Silence>> {
        let (sid, e) = (service.to_string(), env.to_string());
        self.db(move |s| {
            s.environment(&sid, &e)?;
            s.silences(&sid, &e, code_sql::now_ms())
        })
        .await
    }

    /// `POST …/environments/{env}/silences {until, alerts?, reason, every?}`: `until` within 30
    /// days, or a repeating one's (`every: {windows, timezone?}`) within a year, a year by default.
    pub async fn add_silence(self: &Arc<Self>, service: &str, env: &str, body: Value) -> Result<Silence> {
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| !matches!(k.as_str(), "until" | "alerts" | "reason" | "every")) {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        let now = code_sql::now_ms();
        let every = match o.get("every") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let e: crate::freeze::PauseSchedule = serde_json::from_value(v.clone()).map_err(|e| Error::BadRequest(format!("every is {{windows: [{{from: \"sat 02:00\", to: \"sat 04:00\"}}], timezone?}}: {e}")))?;
                e.check().map_err(|e| Error::BadRequest(e.to_string().replace("pause_schedule:", "every:")))?;
                Some(e)
            }
        };
        let longest = if every.is_some() { MAX_EVERY_MS } else { MAX_MS };
        let until = match o.get("until") {
            None | Some(Value::Null) if every.is_some() => Some(now + MAX_EVERY_MS),
            u => u.and_then(Value::as_i64).filter(|u| *u > now && *u <= now + longest),
        }
        .ok_or_else(|| Error::BadRequest(if every.is_some() { "until is when a repeating silence ends (unix ms), in the next year" } else { "until is when it ends (unix ms), in the next 30 days" }.into()))?;
        let alerts: Vec<String> = match o.get("alerts") {
            None | Some(Value::Null) => Vec::new(),
            Some(v) => serde_json::from_value(v.clone()).map_err(|_| Error::BadRequest("alerts is a list of alert kinds".into()))?,
        };
        if let Some(a) = alerts.iter().find(|a| !KINDS.contains(&a.as_str())) {
            return Err(Error::BadRequest(format!("{a} isn't an alert a silence quiets: {}", KINDS.join(", "))));
        }
        let reason = o.get("reason").and_then(Value::as_str).map(str::trim).filter(|r| (1..=200).contains(&r.chars().count()) && !r.contains(['\n', '\r'])).ok_or_else(|| Error::BadRequest("reason is 1–200 characters on one line (why it's quiet)".into()))?.to_string();
        let (sid, e) = (service.to_string(), env.to_string());
        let s = self
            .db(move |s| {
                s.environment(&sid, &e)?;
                s.add_silence(&sid, &e, until, &alerts, &reason, every.as_ref())
            })
            .await?;
        let (sid, e, data) = (service.to_string(), env.to_string(), json!({ "silence": s.id, "until": s.until, "alerts": s.alerts, "reason": s.reason, "every": s.every }));
        let _ = self.db(move |st| st.add_event("audit.silence", &sid, &e, None, &data)).await;
        Ok(s)
    }

    /// `DELETE …/environments/{env}/silences/{sid}`: alerts go out again from now.
    pub async fn end_silence(self: &Arc<Self>, service: &str, env: &str, id: &str) -> Result<()> {
        let (sid, e, i) = (service.to_string(), env.to_string(), id.to_string());
        if !self.db(move |s| s.end_silence(&sid, &e, &i)).await? {
            return Err(Error::NotFound(format!("no open silence {id}")));
        }
        Ok(())
    }

    /// Whether an event of `kind` in `service`'s `env` at `at` is quiet.
    pub(crate) async fn silenced(self: &Arc<Self>, service: &str, env: &str, kind: &str, at: i64) -> bool {
        if !KINDS.contains(&kind) || service.is_empty() {
            return false;
        }
        let (sid, e) = (service.to_string(), env.to_string());
        self.db(move |s| s.silences(&sid, &e, at)).await.unwrap_or_default().iter().any(|s| s.quiets(kind, at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repeating silence quiets only inside its weekly windows, in its time zone.
    #[test]
    fn a_repeating_silence_quiets_only_in_its_windows() {
        let every = crate::freeze::PauseSchedule { windows: vec![crate::freeze::Window { from: "sat 02:00".into(), to: "sat 04:00".into() }], timezone: Some("Europe/Berlin".into()) };
        let s = Silence { id: "sil_x".into(), until: i64::MAX, alerts: Vec::new(), reason: "backups".into(), created_at: 0, every: Some(every) };
        // Saturday 2026-11-28: 03:00 in Berlin is 02:00 UTC; 05:00 there is past the window.
        let at = |h: u32| chrono::NaiveDate::from_ymd_opt(2026, 11, 28).unwrap().and_hms_opt(h, 0, 0).unwrap().and_utc().timestamp_millis();
        assert!(s.quiets("check.down", at(2)));
        assert!(!s.quiets("check.down", at(4)));
        assert!(!s.quiets("alert.resolved", at(2)), "a resolve always goes out");
    }
}
