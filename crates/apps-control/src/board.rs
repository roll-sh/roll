//! A project's board (Railway's observability widgets, Cloud Monitoring's dashboards): the charts
//! a person picks, each one service's metric in one environment, kept by the project
//! (docs/apps/README.md, "A project's board"). The board holds only what to draw: the dashboard
//! reads each panel through the service's own metrics routes, so a board shows nothing its reader
//! couldn't read there.

use crate::{Apps, Error, Result};
use serde_json::{Value, json};
use std::sync::Arc;

/// What a panel can chart from a service's metrics (`…/metrics`): each a series there, and
/// `error_pct` (its errors over its requests). `app:<name>` charts one of the app's own series
/// (`…/app-metrics`: what it sends or roll scrapes, and its log metrics).
pub const METRICS: [&str; 12] = ["requests", "errors", "error_pct", "p50_ms", "p95_ms", "p99_ms", "cpu", "memory_bytes", "instances", "cold_starts", "egress_bytes", "over_capacity"];
pub const MAX_PANELS: usize = 24;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Panel {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub service: String,
    #[serde(default = "Panel::production")]
    pub environment: String,
    pub metric: String,
}

impl Panel {
    fn production() -> String {
        crate::model::PRODUCTION.into()
    }

    fn check(&self) -> std::result::Result<(), String> {
        if self.title.as_ref().is_some_and(|t| !(1..=60).contains(&t.chars().count()) || t.contains(['\n', '\r'])) {
            return Err("a panel's title is 1–60 characters on one line".into());
        }
        if crate::model::check_name("an environment name", &self.environment).is_err() {
            return Err(format!("{} isn't an environment's name", self.environment));
        }
        let app = self.metric.strip_prefix("app:");
        let ok = match app {
            Some(n) => (1..=100).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.:-/".contains(&b)),
            None => METRICS.contains(&self.metric.as_str()),
        };
        if !ok {
            return Err(format!("metric is one of {} or app:<the app's series>", METRICS.join(", ")));
        }
        Ok(())
    }
}

impl Apps {
    /// `GET /v1/apps/projects/{project}/board`: its panels, in order (none when nobody made one).
    pub async fn board(self: &Arc<Self>, project: &str) -> Result<Value> {
        let p = project.to_string();
        let panels: Vec<Panel> = match self.db(move |s| s.board(&p)).await? {
            Some(j) => serde_json::from_str(&j).unwrap_or_default(),
            None => Vec::new(),
        };
        Ok(json!({ "panels": panels }))
    }

    /// `PUT /v1/apps/projects/{project}/board {panels}`: the board replaced (none: removed). Each
    /// panel's service is the project's own.
    pub async fn put_board(self: &Arc<Self>, project: &str, body: Value) -> Result<Value> {
        self.project_name(project)?;
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| k.as_str() != "panels") {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        let panels: Vec<Panel> = match o.get("panels") {
            None | Some(Value::Null) => Vec::new(),
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| Error::BadRequest(format!("panels: {e}")))?,
        };
        if panels.len() > MAX_PANELS {
            return Err(Error::BadRequest(format!("a board has at most {MAX_PANELS} panels")));
        }
        for p in &panels {
            p.check().map_err(|e| Error::BadRequest(format!("panels: {e}")))?;
        }
        let p = project.to_string();
        let services = self.db(move |s| s.services(Some(&p))).await?;
        if let Some(x) = panels.iter().find(|x| !services.iter().any(|s| s.id == x.service)) {
            return Err(Error::BadRequest(format!("panels: {} isn't this project's service", x.service)));
        }
        let (p, j) = (project.to_string(), (!panels.is_empty()).then(|| serde_json::to_string(&panels).expect("panels serialize")));
        self.db(move |s| s.put_board(&p, j.as_deref())).await?;
        Ok(json!({ "panels": panels }))
    }
}
