//! Uptime checks (Cloud Monitoring's uptime checks, Render's and Fly's HTTP checks; docs/apps/
//! README.md, "Uptime checks"): an environment's `checks` are GETs of its own paths, through its
//! own proxy on the owner as a visitor's request would go (past its gates, as a path schedule's
//! call is), every `every_s`. One that fails twice in a row is `check.down` (once, until it
//! passes again); the next pass is `alert.resolved` for it. A check wakes a service that scales
//! to zero, so a service with one doesn't sleep through it.

use crate::model::{Check, CheckState};
use crate::{Apps, env_key};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// Failures in a row before a check is down.
const DOWN_AFTER: u32 = 2;
/// The most of an answer read to look for `contains`.
const MAX_BODY: usize = 64 << 10;

/// One check's last result in memory, on the owner ([`Apps::check_states`]).
#[derive(Clone, Debug, Default)]
pub struct Run {
    /// When its last try started (what the schedule counts from), and when the last one that came
    /// back started (what its answer is of).
    pub started_at: i64,
    pub last_at: i64,
    pub status: Option<u16>,
    pub ms: u64,
    pub error: Option<String>,
    pub failures: u32,
    pub down_since: Option<i64>,
    pub running: bool,
    /// A preview's run: the deployment it's of ([`crate::browser`]'s preview checks).
    pub deployment: Option<String>,
    /// A browser check's last pass that was over its budgets: how.
    pub over_budget: Option<String>,
}

/// Whether an answer passes: the status it expects (2xx or 3xx unless it says), and its text.
pub fn passes(c: &Check, status: u16, body: &[u8]) -> Result<(), String> {
    let status_ok = match c.status {
        Some(s) => status == s,
        None => (200..400).contains(&status),
    };
    if !status_ok {
        return Err(format!("answered {status}"));
    }
    if let Some(t) = &c.contains
        && !String::from_utf8_lossy(body).contains(t.as_str())
    {
        return Err(format!("its answer doesn't contain {t:?}"));
    }
    Ok(())
}

impl Apps {
    /// The checks of every environment of an owned service that are due at `now` (deployed, not
    /// paused or in maintenance, not a preview): each runs on its own task.
    pub(crate) async fn checks_pass(self: &Arc<Self>, services: &[crate::store::ServiceRow], now: i64) {
        let Ok(all_envs) = self.db(|s| s.all_environments()).await else { return };
        for svc in services.iter().filter(|s| self.owns(&s.id) && !self.capped(&s.id)) {
            for e in all_envs.iter().filter(|e| e.service == svc.id && e.active.is_some() && !e.ephemeral && !e.paused && e.maintenance.is_none() && !e.checks.is_empty()) {
                for c in &e.checks {
                    let key = format!("{}/{}", env_key(&svc.id, &e.name), c.name);
                    {
                        let mut runs = self.check_states.lock();
                        let r = runs.entry(key.clone()).or_default();
                        if r.running || now - r.started_at < c.every_s as i64 * 1000 {
                            continue;
                        }
                        r.running = true;
                        r.started_at = now;
                    }
                    let url = self.url(&e.slug);
                    let host = url::Url::parse(&url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
                    let (this, sid, en, c) = (self.clone(), svc.id.clone(), e.name.clone(), c.clone());
                    tokio::spawn(async move { this.run_check(&sid, &en, &host, &url, &c, key, now).await });
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_check(self: &Arc<Self>, service: &str, env: &str, host: &str, url: &str, c: &Check, key: String, started: i64) {
        let began = std::time::Instant::now();
        let (status, verdict) = match &c.browser {
            // A browser check: its page in Chromium, then its script ([`crate::browser`]).
            Some(_) => {
                let at = format!("{}{}", url.trim_end_matches('/'), c.path);
                // Over a budget isn't down: the page answered. It's said on the check's state.
                match self.run_browser_check(service, env, &at, c, &tokio_util::sync::CancellationToken::new(), None).await {
                    Ok(p) => {
                        self.check_states.lock().entry(key.clone()).or_default().over_budget = p.over_budget;
                        (p.status, Ok(()))
                    }
                    Err(e) => (None, Err(e)),
                }
            }
            // From outside: its real URL over the internet, as a visitor's request goes.
            None if c.from.as_deref() == Some("public") => match self.check_from_public(service, env, url, c).await {
                Ok((s, body)) => (Some(s), passes(c, s, &body)),
                Err(e) => (None, Err(e)),
            },
            None => match crate::public::call_check(self.clone(), service, env, host, &c.path, Duration::from_secs(c.timeout_s as u64), MAX_BODY).await {
                Ok((s, body)) => (Some(s), passes(c, s, &body)),
                Err(e) => (None, Err(e)),
            },
        };
        let ms = began.elapsed().as_millis() as u64;
        let now = code_sql::now_ms();
        let (went_down, came_up, up) = {
            let mut runs = self.check_states.lock();
            let r = runs.entry(key).or_default();
            r.running = false;
            // Its answer and the time it's of, together: never one try's answer at another's time.
            r.last_at = started;
            r.status = status;
            r.ms = ms;
            match &verdict {
                Ok(()) => {
                    r.error = None;
                    r.failures = 0;
                    (false, r.down_since.take().is_some(), true)
                }
                Err(e) => {
                    r.error = Some(e.clone());
                    r.failures += 1;
                    let down = r.failures >= DOWN_AFTER && r.down_since.is_none();
                    if down {
                        r.down_since = Some(now);
                    }
                    (down, false, r.down_since.is_none())
                }
            }
        };
        let log = self.logs.sink(format!("env:{service}/{env}"), crate::logs::Stream::System);
        // Its day on a status page: a pass or a failure.
        // Back up is what the store last said too: a restart or a takeover forgets `down_since`.
        let was_down = self.status_count(crate::status_page::check_key(service, env, &c.name), verdict.is_ok(), up).await;
        let came_up = came_up || (up && was_down);
        let (sid, e) = (service.to_string(), env.to_string());
        if went_down || came_up {
            self.status_incident(service, crate::status_page::check_key(service, env, &c.name), went_down).await;
        }
        if went_down {
            let why = verdict.as_ref().err().cloned().unwrap_or_default();
            log.line(format!("uptime check {}: GET {} is down: {why}", c.name, c.path));
            let data = json!({ "check": c.name, "path": c.path, "http_status": status, "error": why });
            if let Err(err) = self.db(move |s| s.add_event("check.down", &sid, &e, None, &data)).await {
                tracing::warn!(service, env, error = %err, "apps: an uptime check's check.down wasn't recorded");
            }
        } else if came_up {
            log.line(format!("uptime check {}: GET {} is up again", c.name, c.path));
            let data = json!({ "alert": "check.down", "check": c.name });
            if let Err(err) = self.db(move |s| s.add_event("alert.resolved", &sid, &e, None, &data)).await {
                tracing::warn!(service, env, error = %err, "apps: an uptime check's alert.resolved wasn't recorded");
            }
        }
    }

    /// A public check's request (`from: public`): to the environment's own URL, or one of its
    /// active custom domains over HTTPS, through the client for URLs a tenant chose (public
    /// addresses only, no redirects, a bounded read).
    async fn check_from_public(self: &Arc<Self>, service: &str, env: &str, url: &str, c: &Check) -> Result<(u16, Vec<u8>), String> {
        let at = match &c.host {
            None => format!("{}{}", url.trim_end_matches('/'), c.path),
            Some(h) => {
                let (sid, e, h) = (service.to_string(), env.to_string(), h.clone());
                let ours = self.db(move |s| s.domains(&sid, &e)).await.map_err(|e| e.to_string())?.into_iter().any(|d| d.hostname == h && d.status == crate::model::DomainStatus::Active);
                if !ours {
                    return Err(format!("{h} isn't one of this environment's active domains"));
                }
                format!("https://{h}{}", c.path)
            }
        };
        let client = crate::net::client(self.cfg.allow_private, Duration::from_secs(c.timeout_s as u64));
        let res = client.get(&at).header(reqwest::header::USER_AGENT, "roll-uptime/1.0 (from outside)").send().await.map_err(|e| {
            // What failed (DNS, TLS, the connection), from the error's own chain.
            let mut why = e.to_string();
            let mut src = std::error::Error::source(&e);
            while let Some(s) = src {
                why = format!("{why}: {s}");
                src = s.source();
            }
            why.chars().take(300).collect::<String>()
        })?;
        let status = res.status().as_u16();
        Ok((status, crate::net::read_bounded(res, MAX_BODY).await))
    }

    /// An environment's checks as they stand, on this node (its owner runs them).
    pub(crate) fn check_states_of(&self, service: &str, env: &str, checks: &[Check]) -> Vec<CheckState> {
        let runs = self.check_states.lock();
        checks
            .iter()
            .map(|c| {
                let r = runs.get(&format!("{}/{}", env_key(service, env), c.name)).cloned().unwrap_or_default();
                // Known once a try has come back (an answer, or why there was none).
                let tried = r.status.is_some() || r.error.is_some();
                let failure_at = self.check_failures.lock().get(&format!("{}/{}", env_key(service, env), c.name)).copied();
                let metrics = self.check_metrics.lock().get(&format!("{}/{}", env_key(service, env), c.name)).cloned();
                CheckState { name: c.name.clone(), up: tried.then_some(r.down_since.is_none()), failures: r.failures, last_at: tried.then_some(r.last_at), status: r.status, ms: tried.then_some(r.ms), error: r.error, down_since: r.down_since, failure_at, deployment: r.deployment, metrics, over_budget: r.over_budget }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_passes_by_its_status_and_text() {
        let c = |status: Option<u16>, contains: Option<&str>| Check { name: "x".into(), path: "/".into(), every_s: 60, timeout_s: 10, status, contains: contains.map(String::from), browser: None, budgets: None, from: None, host: None };
        assert!(passes(&c(None, None), 301, b"").is_ok(), "2xx or 3xx by default");
        assert_eq!(passes(&c(None, None), 503, b""), Err("answered 503".into()));
        assert!(passes(&c(Some(204), None), 204, b"").is_ok());
        assert!(passes(&c(Some(204), None), 200, b"").is_err(), "the status it says");
        assert!(passes(&c(None, Some("ok")), 200, b"{\"status\":\"ok\"}").is_ok());
        assert!(passes(&c(None, Some("ok")), 200, b"degraded").is_err());
    }
}
