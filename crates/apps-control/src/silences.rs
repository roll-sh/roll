//! Alert silences (Cloud Monitoring's snoozes, Better Stack's maintenance windows;
//! docs/apps/README.md, "Silencing alerts"): for a while, an environment's alerts reach no
//! notification target, on-call included. The events are still kept (Events lists them), and a
//! resolve always goes out, so an incident a silence began in still closes. A status page's
//! scheduled maintenance makes one for what the page shows, for its window.

use crate::{Apps, Error, Result};
use serde_json::{Value, json};
use std::sync::Arc;

/// The most silences open at once in an environment.
pub const MAX_OPEN: i64 = 20;
/// The longest one lasts.
pub const MAX_MS: i64 = 30 * 86_400_000;

/// What a silence can quiet: the alerts, by their event kind.
pub const KINDS: [&str; 15] = [
    "check.down", "heartbeat.missed", "crash_loop", "alert.memory", "alert.cpu", "alert.metric", "alert.errors", "alert.latency", "alert.capacity", "alert.log", "alert.route", "alert.slo", "alert.anomaly", "cron.failed", "deploy.failed",
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
}

impl Silence {
    /// Whether it quiets an event of `kind` (a resolve never is: it closes what's open).
    pub fn quiets(&self, kind: &str) -> bool {
        KINDS.contains(&kind) && (self.alerts.is_empty() || self.alerts.iter().any(|a| a == kind))
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

    /// `POST …/environments/{env}/silences {until, alerts?, reason}`.
    pub async fn add_silence(self: &Arc<Self>, service: &str, env: &str, body: Value) -> Result<Silence> {
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| !matches!(k.as_str(), "until" | "alerts" | "reason")) {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        let now = code_sql::now_ms();
        let until = o.get("until").and_then(Value::as_i64).filter(|u| *u > now && *u <= now + MAX_MS).ok_or_else(|| Error::BadRequest("until is when it ends (unix ms), in the next 30 days".into()))?;
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
                s.add_silence(&sid, &e, until, &alerts, &reason)
            })
            .await?;
        let (sid, e, data) = (service.to_string(), env.to_string(), json!({ "silence": s.id, "until": s.until, "alerts": s.alerts, "reason": s.reason }));
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
        self.db(move |s| s.silences(&sid, &e, at)).await.unwrap_or_default().iter().any(|s| s.quiets(kind))
    }
}
