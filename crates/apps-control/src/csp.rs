//! CSP reports (Report URI, Cloudflare Page Shield; docs/apps/README.md, "CSP reports"): with
//! `csp_reports: true`, every answer names roll's `/__roll/csp` as its `roll` reporting endpoint,
//! and a Content-Security-Policy (or its Report-Only twin) that names nowhere to report gets
//! `report-uri /__roll/csp; report-to roll`. What browsers send there, in the Reporting API's shape
//! or the legacy one, is kept grouped by (directive, blocked origin, document path, disposition),
//! with counts: URIs are cut to an origin and a path before anything is kept, so a token in a URL
//! never lands. A directive the environment has never had blocked is `alert.csp`, once.

use crate::{Apps, Result};
use axum::http::{HeaderValue, StatusCode};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

pub const BEACON: &str = "/__roll/csp";
/// What an answer's `Reporting-Endpoints` says.
pub const ENDPOINTS: &str = "roll=\"/__roll/csp\"";
/// A report request's most bytes.
pub const MAX_BODY: usize = 16 * 1024;
/// Reports read from one body (the rest are dropped).
pub const MAX_REPORTS: usize = 50;
/// Groups an environment keeps (past it, the one seen longest ago goes).
pub const MAX_GROUPS: i64 = 500;
/// Report requests an environment takes a minute on its owner (the rest are dropped, 429).
const PER_MINUTE: u32 = 300;

/// The directives a report may name (CSP Level 3, and the older ones browsers still report).
/// Anything else isn't kept, so the directives an environment has had stay a short list.
const DIRECTIVES: [&str; 29] = [
    "default-src", "script-src", "script-src-elem", "script-src-attr", "style-src", "style-src-elem", "style-src-attr", "img-src", "font-src", "connect-src", "media-src", "object-src", "frame-src", "child-src", "worker-src", "manifest-src", "prefetch-src", "fenced-frame-src", "form-action", "frame-ancestors", "base-uri", "navigate-to", "sandbox", "require-trusted-types-for", "trusted-types", "upgrade-insecure-requests", "block-all-mixed-content", "plugin-types", "webrtc",
];
/// What a report says was blocked when it wasn't a URL.
const KEYWORDS: [&str; 6] = ["inline", "eval", "wasm-eval", "trusted-types-policy", "trusted-types-sink", "self"];

/// What one report is kept as.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub directive: String,
    /// An origin (`https://cdn.example`), a scheme (`data:`), a keyword (`inline`) or `other`.
    pub blocked: String,
    /// The document's path, ids folded as routes are (`/users/:id`).
    pub path: String,
    pub disposition: &'static str,
}

/// A policy value with roll's reporting added to each policy in it that reports nowhere; one
/// that names its own `report-to` or `report-uri` is left as it is. `report-uri` beside
/// `report-to`: a browser with the Reporting API uses `report-to` and ignores `report-uri` (CSP
/// Level 3), one without it (Firefox) still reports, and over plain HTTP, where the Reporting API
/// sends nothing, too.
pub fn with_reporting(v: &HeaderValue) -> HeaderValue {
    let Ok(s) = v.to_str() else { return v.clone() };
    let policies: Vec<String> = s
        .split(',')
        .map(|p| {
            let reports = p.split(';').any(|d| matches!(d.split_ascii_whitespace().next().map(str::to_ascii_lowercase).as_deref(), Some("report-to" | "report-uri")));
            let t = p.trim().trim_end_matches(';').trim_end();
            if reports || t.is_empty() { p.trim().to_string() } else { format!("{t}; report-uri {BEACON}; report-to roll") }
        })
        .collect();
    HeaderValue::from_str(&policies.join(", ")).unwrap_or_else(|_| v.clone())
}

fn directive(s: &str) -> Option<String> {
    let d = s.split_ascii_whitespace().next()?.to_ascii_lowercase();
    DIRECTIVES.contains(&d.as_str()).then_some(d)
}

/// A blocked URI as its origin only (never its path, query or fragment).
fn blocked(s: &str) -> String {
    let s = s.trim();
    let lower = s.to_ascii_lowercase();
    if KEYWORDS.contains(&lower.as_str()) {
        return lower;
    }
    if lower.is_empty() {
        return "inline".into();
    }
    match url::Url::parse(s) {
        Ok(u) if matches!(u.scheme(), "http" | "https" | "ws" | "wss") && u.host().is_some() => u.origin().ascii_serialization().chars().take(200).collect(),
        Ok(u) => format!("{}:", u.scheme()).chars().take(40).collect(),
        // The legacy shape names a scheme alone (`data`, `blob`).
        Err(_) if lower.len() <= 20 && lower.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') => format!("{lower}:"),
        Err(_) => "other".into(),
    }
}

/// A document's URL as its path only, ids folded.
fn path(s: &str) -> Option<String> {
    let u = url::Url::parse(s).ok().filter(|u| matches!(u.scheme(), "http" | "https"))?;
    Some(crate::traffic::route_of(u.path()))
}

fn key(directive_s: Option<&str>, blocked_s: Option<&str>, document: Option<&str>, disposition: Option<&str>) -> Option<Key> {
    Some(Key {
        directive: directive(directive_s?)?,
        blocked: blocked(blocked_s.unwrap_or("")),
        path: path(document?)?,
        disposition: if disposition == Some("report") { "report" } else { "enforce" },
    })
}

/// What a body said: the Reporting API's list (`application/reports+json`, its `csp-violation`
/// reports) or the legacy `{"csp-report": …}` (`application/csp-report`), at most
/// [`MAX_REPORTS`] read. None: neither shape.
pub fn parse(body: &[u8]) -> Option<Vec<Key>> {
    let v: Value = serde_json::from_slice(body).ok()?;
    if let Some(list) = v.as_array() {
        return Some(
            list.iter()
                .take(MAX_REPORTS)
                .filter(|r| r["type"] == "csp-violation")
                .filter_map(|r| {
                    let b = &r["body"];
                    key(b["effectiveDirective"].as_str(), b["blockedURL"].as_str(), b["documentURL"].as_str().or(r["url"].as_str()), b["disposition"].as_str())
                })
                .collect(),
        );
    }
    let r = v.get("csp-report")?;
    Some(key(r["effective-directive"].as_str().or(r["violated-directive"].as_str()), r["blocked-uri"].as_str(), r["document-uri"].as_str(), r["disposition"].as_str()).into_iter().collect())
}

/// Whether a request's type is one a browser sends reports as.
pub fn report_type(content_type: Option<&str>) -> bool {
    let t = content_type.and_then(|t| t.split(';').next()).map(|t| t.trim().to_ascii_lowercase());
    matches!(t.as_deref(), Some("application/reports+json" | "application/csp-report" | "application/json"))
}

impl Apps {
    /// A report request (on the environment's owner): kept, or why not.
    pub(crate) async fn csp_beacon(self: &Arc<Self>, service: &str, env: &str, body: &[u8]) -> StatusCode {
        let Some(keys) = parse(body) else { return StatusCode::BAD_REQUEST };
        let minute = code_sql::now_ms() / 60_000;
        {
            let mut seen = self.csp_seen.lock();
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
        if keys.is_empty() {
            return StatusCode::NO_CONTENT;
        }
        let mut counts: HashMap<Key, i64> = HashMap::new();
        for k in keys {
            *counts.entry(k).or_default() += 1;
        }
        let counts: Vec<(Key, i64)> = counts.into_iter().collect();
        let (sid, e) = (service.to_string(), env.to_string());
        match self.db(move |s| s.add_csp_reports(&sid, &e, &counts)).await {
            Ok(new) => {
                let log = self.logs.sink(format!("env:{service}/{env}"), crate::logs::Stream::System);
                for k in new {
                    log.line(format!("alert: alert.csp: {} blocked {} on {} for the first time", k.directive, k.blocked, k.path));
                }
                StatusCode::NO_CONTENT
            }
            Err(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    /// `GET …/environments/{env}/csp-reports?minutes=`: the groups seen in the last 60–10,080
    /// minutes, the most frequent first (their counts since they were first seen).
    pub async fn csp_reports(self: &Arc<Self>, service: &str, env: &str, minutes: i64) -> Result<Value> {
        let (row_svc, row) = self.service_env(service, env).await?;
        let minutes = minutes.clamp(60, 10_080);
        let since = code_sql::now_ms() - minutes * 60_000;
        let (sid, e) = (row_svc.id, row.name);
        let groups = self.db(move |s| s.csp_reports(&sid, &e, since)).await?;
        let groups: Vec<Value> = groups
            .into_iter()
            .map(|(k, count, first, last)| json!({ "directive": k.directive, "blocked": k.blocked, "path": k.path, "disposition": k.disposition, "count": count, "first_seen": first, "last_seen": last }))
            .collect();
        Ok(json!({ "minutes": minutes, "on": row.csp_reports, "groups": groups }))
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    #[test]
    fn a_policy_that_reports_nowhere_gets_roll_and_one_that_does_is_kept() {
        let v = |s: &str| super::with_reporting(&HeaderValue::from_str(s).unwrap()).to_str().unwrap().to_string();
        assert_eq!(v("default-src 'self';"), "default-src 'self'; report-uri /__roll/csp; report-to roll");
        assert_eq!(v("default-src 'self'; Report-To theirs, img-src *"), "default-src 'self'; Report-To theirs, img-src *; report-uri /__roll/csp; report-to roll");
        assert_eq!(v("script-src 'none'; report-uri https://r.example/x"), "script-src 'none'; report-uri https://r.example/x");
    }

    #[test]
    fn a_report_keeps_only_origins_and_paths() {
        assert_eq!(super::blocked("https://cdn.example:8443/a.js?token=s#f"), "https://cdn.example:8443");
        assert_eq!((super::blocked("data"), super::blocked("data:image/png;base64,xx"), super::blocked("Inline")), ("data:".into(), "data:".into(), "inline".into()));
        assert_eq!(super::path("https://site.example/users/42/edit?token=s#x").as_deref(), Some("/users/:id/edit"));
        assert!(super::parse(br#"{"csp-report":{"violated-directive":"evil-src x","document-uri":"https://a/"}}"#).unwrap().is_empty(), "an unknown directive");
    }
}
