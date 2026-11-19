//! Skew protection for web services (docs/apps/README.md, "Skew protection"; Vercel's): with
//! `run.skew_protection`, a page loaded before a deploy keeps talking to the deployment that
//! served it. A request that names a deployment (`x-deployment-id`, `?dpl=`, or the cookie the
//! proxy sets on each page load) goes to that deployment's own instance, as its own URL would,
//! when it's one of the environment's last [`EARLIER`] live deployments before the live one and
//! the live one went live less than [`crate::sites::SKEW_WINDOW_MS`] ago. Never a page load (that
//! gets what's live and a new cookie), never a deployment rolled back from, never past the
//! environment's gates (they're read before this).

use crate::live::Template;
use crate::model::DeployStatus;
use crate::{Apps, env_key};
use axum::extract::Request;
use axum::http::{HeaderValue, header};
use std::sync::Arc;

/// The cookie a page load gets: the deployment that served it.
pub(crate) const COOKIE: &str = "__roll_dpl";

/// Earlier deployments a request may name.
const EARLIER: usize = 3;

/// An environment's earlier deployments, as last read for its live one.
pub(crate) struct Earlier {
    live: String,
    until: i64,
    deployments: Vec<String>,
}

/// A browser's page load (its document): what's live answers it.
pub(crate) fn navigation(req: &Request) -> bool {
    req.method() == axum::http::Method::GET
        && match req.headers().get("sec-fetch-dest").and_then(|v| v.to_str().ok()) {
            Some(dest) => dest.eq_ignore_ascii_case("document"),
            None => req.headers().get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|a| a.contains("text/html")),
        }
}

/// The deployment a request that isn't a page load names: its header, its `dpl` query, or the
/// cookie.
pub(crate) fn asked(req: &Request) -> Option<String> {
    if navigation(req) {
        return None;
    }
    named(req)
}

fn named(req: &Request) -> Option<String> {
    let ok = |s: &str| (s.starts_with("dep_") && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')).then(|| s.to_string());
    if let Some(d) = req.headers().get("x-deployment-id").and_then(|v| v.to_str().ok()) {
        return ok(d);
    }
    if let Some(q) = req.uri().query()
        && let Some((_, d)) = url::form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == "dpl")
    {
        return ok(&d);
    }
    req.headers().get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|c| c.trim().strip_prefix(COOKIE).and_then(|r| r.strip_prefix('=')).and_then(ok))
}

/// The cookie for a page load `deployment` answered.
pub(crate) fn cookie(deployment: &str, secure: bool) -> Option<HeaderValue> {
    HeaderValue::from_str(&format!("{COOKIE}={deployment}; Path=/; HttpOnly; SameSite=Lax{}", if secure { "; Secure" } else { "" })).ok()
}

impl Apps {
    /// `asked` ([`asked`]) when it's an earlier deployment that may answer instead of the live
    /// one (`live`).
    pub(crate) async fn skew_target(self: &Arc<Self>, service: &str, env: &str, live: &Template, asked: Option<String>) -> Option<String> {
        if !live.run.skew_protection || live.site.is_some() {
            return None;
        }
        let d = asked.filter(|d| *d != live.deployment)?;
        let ek = env_key(service, env);
        let cached = self.skew.lock().get(&ek).filter(|c| c.live == live.deployment).map(|c| (c.until, c.deployments.clone()));
        let (until, earlier) = match cached {
            Some(c) => c,
            None => {
                let (sid, e) = (service.to_string(), env.to_string());
                let deps = self.db(move |s| s.deployments(&sid, &e, 20, None)).await.ok()?;
                let active = deps.iter().find(|d| d.status == DeployStatus::Active);
                let until = active.and_then(|a| a.finished_at).map_or(0, |at| at + crate::sites::SKEW_WINDOW_MS);
                // Before the live one, live once: never one rolled back from, never pruned.
                let before = active.map_or(i64::MIN, |a| a.created_at);
                let earlier: Vec<String> = deps.iter().filter(|d| d.status == DeployStatus::Inactive && d.created_at < before && d.image.is_some() && d.pruned_at.is_none()).take(EARLIER).map(|d| d.id.clone()).collect();
                let mut cache = self.skew.lock();
                if cache.len() >= 10_000 {
                    cache.clear();
                }
                cache.insert(ek, Earlier { live: live.deployment.clone(), until, deployments: earlier.clone() });
                (until, earlier)
            }
        };
        (code_sql::now_ms() <= until && earlier.contains(&d)).then_some(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(b: axum::http::request::Builder) -> Request {
        b.body(axum::body::Body::empty()).unwrap()
    }

    #[test]
    fn a_request_names_its_deployment_and_a_page_load_never_does() {
        let get = || Request::builder().uri("/api/x");
        assert_eq!(named(&req(get().header("x-deployment-id", "dep_abc"))).as_deref(), Some("dep_abc"));
        assert_eq!(named(&req(Request::builder().uri("/_next/a.js?dpl=dep_abc"))).as_deref(), Some("dep_abc"));
        assert_eq!(named(&req(get().header("cookie", "a=1; __roll_dpl=dep_abc"))).as_deref(), Some("dep_abc"));
        assert_eq!(named(&req(get().header("x-deployment-id", "../etc"))), None, "only a deployment id");
        assert!(navigation(&req(get().header("sec-fetch-dest", "document"))));
        assert!(!navigation(&req(get().header("sec-fetch-dest", "script"))));
    }
}
