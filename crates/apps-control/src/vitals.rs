//! Real-user Web Vitals (Vercel's Speed Insights, Netlify's Real User Metrics; docs/apps/README.md,
//! "Web Vitals"): with `vitals: true`, the proxy adds `<script src="/__roll/vitals.js" defer>` to
//! the environment's HTML pages (as the feedback button), which measures the page load in the
//! visitor's browser and sends one beacon to `/__roll/vitals` on the same host when the page is
//! hidden. Each is kept as a line of the environment's `vitals:` log (its route, the numbers, the
//! edge's country), with the log's retention; nothing about the visitor is. Read back as p75s,
//! rated by web.dev's thresholds, overall and by route.

use crate::{Apps, Result};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

pub const SCRIPT_PATH: &str = "/__roll/vitals.js";
pub const BEACON: &str = "/__roll/vitals";
pub const TAG: &[u8] = b"<script src=\"/__roll/vitals.js\" defer></script>";
const SCRIPT: &str = include_str!("../assets/vitals.js");
/// A beacon's most bytes.
pub const MAX_BODY: usize = 2048;
/// Beacons an environment keeps a minute on its owner (the rest are dropped, 429).
const PER_MINUTE: u32 = 600;
/// Lines one read looks at (newest first).
const MAX_LINES: usize = 200_000;
/// Routes an answer lists.
const MAX_ROUTES: usize = 20;

fn key(service: &str, env: &str) -> String {
    format!("vitals:{service}/{env}")
}

/// The metrics, each with web.dev's good and poor thresholds and the most a sample may say.
const METRICS: [(&str, f64, f64, f64); 5] = [("lcp", 2500.0, 4000.0, 60_000.0), ("inp", 200.0, 500.0, 60_000.0), ("cls", 0.1, 0.25, 10.0), ("fcp", 1800.0, 3000.0, 60_000.0), ("ttfb", 800.0, 1800.0, 60_000.0)];

pub fn script() -> Response {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=3600")], SCRIPT).into_response()
}

/// One page load's numbers as kept.
#[derive(Serialize, Deserialize, Default)]
struct Sample {
    route: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    m: HashMap<String, f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    country: Option<String>,
}

fn rating(name: &str, v: f64) -> &'static str {
    let (_, good, poor, _) = METRICS.iter().find(|m| m.0 == name).copied().unwrap_or(("", f64::MAX, f64::MAX, 0.0));
    if v <= good {
        "good"
    } else if v <= poor {
        "needs-improvement"
    } else {
        "poor"
    }
}

/// The 75th percentile (nearest rank).
fn p75(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let i = ((v.len() as f64 * 0.75).ceil() as usize).saturating_sub(1);
    Some(v[i.min(v.len() - 1)])
}

/// What a beacon said, checked: a path (as its route), and the metrics it has, each a finite
/// number within its bound. Nothing else is read.
fn parse(body: &[u8]) -> Option<Sample> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let path = v["path"].as_str().filter(|p| p.starts_with('/') && p.len() <= 2048)?;
    let mut m = HashMap::new();
    for (name, _, _, max) in METRICS {
        if let Some(x) = v[name].as_f64().filter(|x| x.is_finite() && *x >= 0.0 && *x <= max) {
            m.insert(name.to_string(), if name == "cls" { (x * 10_000.0).round() / 10_000.0 } else { x.round() });
        }
    }
    (!m.is_empty()).then(|| Sample { route: crate::traffic::route_of(path), m, country: None })
}

impl Apps {
    /// A page's beacon (on the environment's owner): kept, or why not.
    pub(crate) fn vitals_beacon(&self, service: &str, env: &str, body: &[u8], country: Option<String>) -> StatusCode {
        let Some(mut s) = parse(body) else { return StatusCode::BAD_REQUEST };
        let minute = code_sql::now_ms() / 60_000;
        {
            let mut seen = self.vitals_seen.lock();
            if seen.len() > 10_000 {
                seen.clear();
            }
            let e = seen.entry(crate::env_key(service, env)).or_insert((minute, 0));
            if e.0 != minute {
                *e = (minute, 0);
            }
            if e.1 >= PER_MINUTE {
                return StatusCode::TOO_MANY_REQUESTS;
            }
            e.1 += 1;
        }
        s.country = country;
        self.logs.sink(key(service, env), crate::logs::Stream::System).line(serde_json::to_string(&s).expect("a sample serializes"));
        StatusCode::NO_CONTENT
    }

    /// `GET …/environments/{env}/vitals?minutes=`: the last 60–10,080 minutes' page loads, each
    /// metric's p75 with its rating and sample count, overall and for the routes with the most
    /// samples.
    pub async fn vitals(self: &Arc<Self>, service: &str, env: &str, minutes: i64) -> Result<Value> {
        self.service_env(service, env).await?;
        let minutes = minutes.clamp(60, 10_080);
        let since = code_sql::now_ms() - minutes * 60_000;
        let (lines, cut) = self.logs.read(&key(service, env), |_| true, Some(since), None, MAX_LINES).await;
        let samples: Vec<Sample> = lines.iter().filter_map(|l| serde_json::from_str(&l.text).ok()).collect();
        let summary = |of: &[&Sample]| -> Value {
            let mut out = serde_json::Map::new();
            for (name, _, _, _) in METRICS {
                let vals: Vec<f64> = of.iter().filter_map(|s| s.m.get(name).copied()).collect();
                let n = vals.len();
                if let Some(p) = p75(vals) {
                    out.insert(name.into(), json!({ "p75": p, "rating": rating(name, p), "samples": n }));
                }
            }
            Value::Object(out)
        };
        let all: Vec<&Sample> = samples.iter().collect();
        let mut by: HashMap<&str, Vec<&Sample>> = HashMap::new();
        for s in &samples {
            by.entry(s.route.as_str()).or_default().push(s);
        }
        let mut routes: Vec<(&str, Vec<&Sample>)> = by.into_iter().collect();
        routes.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));
        routes.truncate(MAX_ROUTES);
        let routes: Vec<Value> = routes.into_iter().map(|(r, ss)| json!({ "route": r, "samples": ss.len(), "metrics": summary(&ss) })).collect();
        Ok(json!({ "minutes": minutes, "samples": samples.len(), "metrics": summary(&all), "routes": routes, "cut": cut || lines.len() >= MAX_LINES }))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_beacon_keeps_only_its_bounded_numbers_and_rates_by_p75() {
        let s = super::parse(br#"{"path":"/users/42","lcp":2400.4,"cls":0.123456,"inp":-1,"ttfb":1e9,"evil":"x"}"#).unwrap();
        assert_eq!(s.route, "/users/:id");
        assert_eq!((s.m.get("lcp"), s.m.get("cls"), s.m.get("inp"), s.m.get("ttfb")), (Some(&2400.0), Some(&0.1235), None, None));
        assert!(super::parse(br#"{"path":"/"}"#).is_none(), "no numbers, nothing kept");
        assert!(super::parse(br#"{"path":"https://evil/","lcp":1}"#).is_none());
        assert_eq!(super::p75(vec![100.0, 200.0, 300.0, 400.0]), Some(300.0));
        assert_eq!((super::rating("lcp", 2500.0), super::rating("lcp", 2600.0), super::rating("cls", 0.3)), ("good", "needs-improvement", "poor"));
    }
}
