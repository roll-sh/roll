//! Spend limits (docs/apps/README.md, "Spend limits"): a project's monthly alert threshold and
//! hard limit, in US dollars at roll's prices, over its services' metered use this calendar
//! month (UTC). Crossing the alert writes `spend.alert` to the event log (notifications send
//! it); crossing the hard limit writes `spend.limit` and caps the project: its instances stop,
//! nothing wakes or deploys, and visitors get a 402 page, until the month turns or the limit is
//! raised. One node (`apps:spend`) decides; every node keeps the capped services in memory.

use crate::{Apps, Error, Result};
use chrono::{Datelike, TimeZone, Utc};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;

/// `(month, its first millisecond)`: the month as `year * 12 + month - 1`, UTC.
pub fn month_of(now_ms: i64) -> (i64, i64) {
    let t = Utc.timestamp_millis_opt(now_ms).single().unwrap_or_default();
    let start = Utc.with_ymd_and_hms(t.year(), t.month(), 1, 0, 0, 0).single().unwrap_or_default();
    (t.year() as i64 * 12 + t.month0() as i64, start.timestamp_millis())
}

/// A project's limits and where it stands.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Limits {
    pub alert_usd: Option<f64>,
    pub hard_usd: Option<f64>,
    /// The month an alert went out for.
    pub alerted: Option<i64>,
    /// The month it's capped for.
    pub capped: Option<i64>,
}

impl Apps {
    /// `GET /v1/apps/projects/{project}/limits`: the limits, this month's spend, whether capped.
    pub async fn limits(self: &Arc<Self>, project: &str) -> Result<Value> {
        let p = project.to_string();
        let l = self.db(move |s| s.spend_limits(&p)).await?.unwrap_or_default();
        let (month, start) = month_of(code_sql::now_ms());
        let p = project.to_string();
        let used = self.db(move |s| s.project_usage(&p, start / 3_600_000)).await?;
        Ok(json!({
            "alert_usd": l.alert_usd,
            "hard_usd": l.hard_usd,
            "month_usd": round(self.cfg.prices.cost(&used)),
            "capped": l.capped == Some(month),
        }))
    }

    /// `PUT …/limits {alert_usd, hard_usd}` (null: none). A change clears this month's alert
    /// and cap: the next pass (within 30 s) decides them again against the new limits.
    pub async fn set_limits(self: &Arc<Self>, project: &str, body: Value) -> Result<Value> {
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| !["alert_usd", "hard_usd"].contains(&k.as_str())) {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        let usd = |k: &str| -> Result<Option<f64>> {
            match o.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(v) => v.as_f64().filter(|x| x.is_finite() && *x > 0.0 && *x <= 1_000_000.0).map(Some).ok_or_else(|| Error::BadRequest(format!("{k} is a number of dollars, more than 0"))),
            }
        };
        let (alert, hard) = (usd("alert_usd")?, usd("hard_usd")?);
        if let (Some(a), Some(h)) = (alert, hard)
            && a > h
        {
            return Err(Error::BadRequest("alert_usd is at most hard_usd".into()));
        }
        let p = project.to_string();
        self.db(move |s| s.set_spend_limits(&p, alert, hard)).await?;
        self.refresh_capped().await;
        self.limits(project).await
    }

    /// Whether a service's project is capped this month (as this node last read it).
    pub fn capped(&self, service: &str) -> bool {
        self.capped.lock().contains(service)
    }

    /// A 409 for anything that would spend more in a capped project (deploys of every kind).
    pub(crate) fn refuse_capped(&self, service: &str) -> Result<()> {
        if self.capped(service) {
            return Err(Error::Conflict("this project reached its spending limit for the month: raise the limit to deploy".into()));
        }
        Ok(())
    }

    /// Every node, every `spend_every`: the capped services from the store, onto the
    /// environments loaded here; and on the node holding `apps:spend`, each project's spend
    /// against its limits first.
    pub(crate) async fn spender(self: Arc<Self>) {
        loop {
            if self.deps.cluster.singleton("apps:spend") {
                self.judge_spend().await;
            }
            self.refresh_capped().await;
            tokio::select! {
                _ = tokio::time::sleep(self.cfg.spend_every) => {}
                _ = self.stop.cancelled() => return,
            }
        }
    }

    async fn judge_spend(self: &Arc<Self>) {
        let (month, start) = month_of(code_sql::now_ms());
        let Ok(all) = self.db(|s| s.all_spend_limits()).await else { return };
        for (project, l) in all {
            let p = project.clone();
            let Ok(used) = self.db(move |s| s.project_usage(&p, start / 3_600_000)).await else { continue };
            let usd = self.cfg.prices.cost(&used);
            let name = self.project_name(&project).unwrap_or_else(|_| project.clone());
            let data = |limit: f64| json!({ "project": project, "project_name": name, "usd": round(usd), "limit_usd": limit });
            if let Some(a) = l.alert_usd
                && usd >= a
                && l.alerted != Some(month)
            {
                let (p, d) = (project.clone(), data(a));
                if self.db(move |s| s.spend_crossed(&p, "spend.alert", month, &d)).await.is_ok() {
                    tracing::info!(project = %project, usd, alert = a, "apps: a project crossed its spend alert");
                }
            }
            if let Some(h) = l.hard_usd
                && usd >= h
                && l.capped != Some(month)
            {
                let (p, d) = (project.clone(), data(h));
                if self.db(move |s| s.spend_crossed(&p, "spend.limit", month, &d)).await.is_ok() {
                    tracing::warn!(project = %project, usd, "apps: a project reached its spend limit; its services are stopped");
                }
            }
        }
    }

    /// Read the capped services and tell every environment loaded here.
    pub(crate) async fn refresh_capped(self: &Arc<Self>) {
        let (month, _) = month_of(code_sql::now_ms());
        let Ok(ids) = self.db(move |s| s.capped_services(month)).await else { return };
        let ids: HashSet<String> = ids.into_iter().collect();
        for l in self.lives() {
            l.set_capped(ids.contains(&l.service));
        }
        // Jobs already running (a job lasts up to a day) stop too.
        for id in &ids {
            self.cancel_service_jobs(id);
        }
        *self.capped.lock() = ids;
    }
}

fn round(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months_are_utc_calendar_months() {
        let (m, start) = month_of(1_794_540_427_871); // 2026-11-13
        assert_eq!(m, 2026 * 12 + 10);
        assert_eq!(start, Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap().timestamp_millis());
        assert_eq!(month_of(start - 1).0, m - 1);
    }
}
