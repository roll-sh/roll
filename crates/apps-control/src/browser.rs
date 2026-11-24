//! Browser checks (Checkly's, through Vercel; docs/apps/README.md, "Uptime checks"): a check with
//! a `browser` script loads its path in Chromium and runs the script against the page, in a
//! container of roll's checker image: Playwright's own (it holds the browsers) with the pinned
//! `playwright-core` library and roll's runner (`browser/run.mjs`), built once a service through
//! its builder and reused. The container runs on the environment's network with none of the app's
//! variables; Playwright fetches the environment's host's requests through the apps listener, as
//! instances reach roll, with a token of the run's own that lets it past the environment's gates as uptime checks go
//! ([`Apps::check_token_admits`]). Its last line is the result. Metered to the service as a run is.

use crate::Apps;
use crate::model::Check;
use std::sync::Arc;
use std::time::Duration;

/// Playwright's version: its image and the library, one release.
pub const PLAYWRIGHT: &str = "1.65.1";
const RUNNER: &str = include_str!("browser/run.mjs");
/// What the container runs.
const COMMAND: &str = "exec node /roll-check/run.mjs";
/// Where the runner leaves a failure's screenshot and trace.
const KEEP: &str = "/tmp/roll-check";
/// A failure's files, as kept and served.
pub const SCREENSHOT: &str = "screenshot.png";
pub const TRACE: &str = "trace.zip";

/// Where a check's last failure's files are kept in object storage (`env` `None`: all of a
/// service's).
pub fn failure_prefix(service: &str, env: Option<&str>, check: Option<&str>) -> String {
    match (env, check) {
        (Some(e), Some(c)) => format!("apps/checks/{service}/{e}/{c}/"),
        (Some(e), None) => format!("apps/checks/{service}/{e}/"),
        _ => format!("apps/checks/{service}/"),
    }
}

/// The screenshot and trace in a run's kept archive.
fn failure_files(tar: &[u8]) -> Vec<(&'static str, Vec<u8>)> {
    let mut out = Vec::new();
    let mut a = tar::Archive::new(tar);
    let Ok(entries) = a.entries() else { return out };
    for mut e in entries.flatten() {
        let name = e.path().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
        let Some(which) = [SCREENSHOT, TRACE].into_iter().find(|w| name.as_deref() == Some(*w)) else { continue };
        let mut buf = Vec::new();
        use std::io::Read;
        if e.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
            out.push((which, buf));
        }
    }
    out
}

/// The checker image's Dockerfile.
pub fn dockerfile() -> String {
    use base64::Engine;
    let runner = base64::engine::general_purpose::STANDARD.encode(RUNNER);
    format!(
        "FROM mcr.microsoft.com/playwright:v{PLAYWRIGHT}-noble\n\
         RUN mkdir -p /roll-check && cd /roll-check && npm init -y >/dev/null \\\n    && PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 npm install --no-audit --no-fund playwright-core@{PLAYWRIGHT} \\\n    && echo {runner} | base64 -d > /roll-check/run.mjs\n\
         USER pwuser\n\
         CMD [\"node\", \"/roll-check/run.mjs\"]\n"
    )
}

/// The lab metrics a run's result says, if it measured them.
fn metrics(lines: &[String]) -> Option<serde_json::Value> {
    let v: serde_json::Value = lines.iter().rev().find_map(|l| l.strip_prefix("ROLL-CHECK ")).and_then(|j| serde_json::from_str(j).ok())?;
    v.get("metrics").filter(|m| m.is_object()).cloned()
}

/// A run's result as the runner says it (`ROLL-CHECK {…}`, its last such line).
/// A run that passed: the page's status, and how it's over its budgets if it is (its own verdict:
/// the page answered and the script passed).
#[derive(Clone, Debug, PartialEq)]
pub struct Passed {
    pub status: Option<u16>,
    pub over_budget: Option<String>,
}

fn result(lines: &[String]) -> Result<Passed, String> {
    let Some(v) = lines.iter().rev().find_map(|l| l.strip_prefix("ROLL-CHECK ")).and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok()) else {
        return Err("the browser didn't say how the check went".into());
    };
    let status = v["status"].as_u64().map(|s| s as u16);
    match v["ok"].as_bool() {
        Some(true) => Ok(Passed { status, over_budget: v["over_budget"].as_str().map(str::to_string) }),
        _ => Err(v["error"].as_str().unwrap_or("the check failed").to_string()),
    }
}

impl Apps {
    /// The service's checker image, built the first time (and again after a restart, from the
    /// builder's cache).
    async fn checker_image(self: &Arc<Self>, service: &str, log: &crate::logs::LogSink, cancel: &tokio_util::sync::CancellationToken) -> anyhow::Result<String> {
        let tag = format!("roll-apps-{service}:browser-check-{}", PLAYWRIGHT.replace('.', "-"));
        if self.checker_images.lock().contains(&tag) {
            return Ok(tag);
        }
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("Dockerfile"), dockerfile())?;
        let job = crate::runtime::BuildJob {
            service: service.to_string(),
            deployment: "browser-check".into(),
            context: dir.path().to_path_buf(),
            dockerfile: "Dockerfile".into(),
            tag: tag.clone(),
            cpu: 1.0,
            memory_mb: 2048,
            timeout: Duration::from_secs(900),
            env: Vec::new(),
            env_hash: String::new(),
            own_dockerfile: false,
            no_cache: false,
            target: None,
            layers: None,
            egress: None,
            priority: false,
        };
        let image = self.deps.builder.build(&job, log, cancel.clone()).await?;
        self.checker_images.lock().insert(image.clone());
        Ok(image)
    }

    /// One browser check of `service`'s `env` at `url` (its public URL and the check's path):
    /// the page's status, or why it failed. `cancel` ends it (its container still removed).
    /// `head`: a preview's run of that deployment, whose metrics and failure are kept only while
    /// it's still the run the check's state is of.
    pub(crate) async fn run_browser_check(self: &Arc<Self>, service: &str, env: &str, url: &str, c: &Check, cancel: &tokio_util::sync::CancellationToken, head: Option<&str>) -> Result<Passed, String> {
        let Some(base) = self.cfg.otlp_base.clone() else { return Err("this node has no apps listener address for browser checks".into()) };
        let log = self.logs.sink(format!("env:{service}/{env}"), crate::logs::Stream::System);
        let image = self.checker_image(service, &log, cancel).await.map_err(|e| format!("roll's browser couldn't be built: {e:#}"))?;
        let token = roll_auth::credentials::new_secret("rlc_");
        self.check_tokens.lock().insert(token.clone(), (service.to_string(), env.to_string()));
        // Its own log, read for the result after (and then forgotten).
        let key = format!("check:{}", crate::model::new_id("r"));
        let reached = self.deps.runtime.reach(base.trim_end_matches('/'));
        let proxy = reached.split_once("://").map_or(reached.as_str(), |(_, rest)| rest).trim_end_matches('/').to_string();
        use base64::Engine;
        let mut vars = vec![
            ("ROLL_CHECK_URL".to_string(), url.to_string()),
            ("ROLL_CHECK_PROXY".to_string(), proxy),
            ("ROLL_CHECK_TOKEN".to_string(), token.clone()),
            ("ROLL_CHECK_SCRIPT".to_string(), base64::engine::general_purpose::STANDARD.encode(c.browser.as_deref().unwrap_or(""))),
            ("ROLL_CHECK_TIMEOUT_MS".to_string(), (c.timeout_s as u64 * 1000).to_string()),
        ];
        if let Some(s) = c.status {
            vars.push(("ROLL_CHECK_STATUS".into(), s.to_string()));
        }
        if let Some(t) = &c.contains {
            vars.push(("ROLL_CHECK_CONTAINS".into(), t.clone()));
        }
        if let Some(b) = &c.budgets {
            vars.push(("ROLL_CHECK_BUDGETS".into(), serde_json::to_string(b).unwrap_or_default()));
        }
        let live = self.live(service, env);
        // The browser's own start and close on top of the check's timeout.
        let ran = live.run_checker(&image, COMMAND, vars, Duration::from_secs(c.timeout_s as u64 + 30), self.logs.sink(key.clone(), crate::logs::Stream::Stdout), KEEP, cancel.clone()).await;
        self.check_tokens.lock().remove(&token);
        let lines: Vec<String> = self.logs.tail(&key, 50, None).await.into_iter().map(|l| l.text).collect();
        self.logs.forget(&key);
        let state_key = format!("{}/{}", crate::env_key(service, env), c.name);
        // A newer head's run decides a preview's check: this one keeps nothing.
        let current = head.is_none_or(|d| self.check_states.lock().get(&state_key).is_some_and(|r| r.deployment.as_deref() == Some(d)));
        // Its lab metrics, whichever way it went (none measured: none shown).
        if current {
            match metrics(&lines) {
                Some(m) => self.check_metrics.lock().insert(state_key.clone(), m),
                None => self.check_metrics.lock().remove(&state_key),
            };
        }
        let (verdict, kept) = match ran {
            Ok(exit) if exit.canceled => (Err("canceled".to_string()), None),
            Ok(exit) if exit.timed_out => (Err(format!("it didn't finish within {} s", c.timeout_s + 30)), None),
            Ok(exit) => (result(&lines), exit.kept),
            Err(e) => (Err(format!("roll's browser didn't start: {e:#}")), None),
        };
        // A failure's screenshot and trace kept, in place of the last one's.
        if verdict.is_err()
            && current
            && let (Some(tar), Some(objects)) = (kept, self.deps.objects.clone())
        {
            let files = failure_files(&tar);
            if !files.is_empty() {
                let prefix = failure_prefix(service, Some(env), Some(&c.name));
                let _ = objects.delete_prefix(&prefix).await;
                for (name, bytes) in files {
                    let _ = objects.put(&format!("{prefix}{name}"), bytes.into()).await;
                }
                self.check_failures.lock().insert(format!("{}/{}", crate::env_key(service, env), c.name), code_sql::now_ms());
            }
        }
        verdict
    }

    /// A browser check's last failure's screenshot or trace (`which`: [`SCREENSHOT`], [`TRACE`]).
    pub async fn check_failure_file(self: &Arc<Self>, service: &str, env: &str, check: &str, which: &str) -> crate::Result<Vec<u8>> {
        self.service_env(service, env).await?;
        let objects = self.deps.objects.clone().ok_or_else(|| crate::Error::NotFound("this node keeps no check failures".into()))?;
        let key = format!("{}{which}", failure_prefix(service, Some(env), Some(check)));
        match objects.get(&key).await {
            Ok(Some(b)) => Ok(b.to_vec()),
            _ => Err(crate::Error::NotFound(format!("no failure of the check {check} is kept"))),
        }
    }

    /// A service's (or one environment's) kept check failures deleted, with it.
    pub(crate) async fn drop_check_failures(self: &Arc<Self>, service: &str, env: Option<&str>) {
        if let Some(objects) = self.deps.objects.clone() {
            let _ = objects.delete_prefix(&failure_prefix(service, env, None)).await;
        }
        let key = match env {
            Some(e) => format!("{}/", crate::env_key(service, e)),
            None => format!("{service}/"),
        };
        self.check_failures.lock().retain(|k, _| !k.starts_with(&key));
        self.check_metrics.lock().retain(|k, _| !k.starts_with(&key));
        self.check_states.lock().retain(|k, _| !k.starts_with(&key));
        if let Some(e) = env
            && let Some(run) = self.preview_check_runs.lock().remove(&crate::env_key(service, e))
        {
            run.cancel();
        }
    }

    /// A preview's head (a pv- environment's deployment that just went live): the production
    /// environment's browser checks, once each, against its own URL (Vercel's deployment checks;
    /// roll previews folds them into its status). Each result names its deployment, and a newer
    /// head's run replaces this one's.
    pub(crate) fn spawn_preview_checks(self: &Arc<Self>, service: &str, env: &str, deployment: &str) {
        let (this, service, env, deployment) = (self.clone(), service.to_string(), env.to_string(), deployment.to_string());
        // A newer head ends the last one's run (its Chromium containers go).
        let cancel = tokio_util::sync::CancellationToken::new();
        if let Some(old) = self.preview_check_runs.lock().insert(crate::env_key(&service, &env), cancel.clone()) {
            old.cancel();
        }
        tokio::spawn(async move {
            let sid = service.clone();
            let Ok(prod) = this.db(move |s| s.environment(&sid, "production")).await else { return };
            let checks: Vec<Check> = prod.checks.into_iter().filter(|c| c.browser.is_some()).collect();
            if checks.is_empty() {
                return;
            }
            let (sid, en) = (service.clone(), env.clone());
            let Ok(row) = this.db(move |s| s.environment(&sid, &en)).await else { return };
            let url = this.url(&format!("{}--{}", deployment.trim_start_matches("dep_"), row.slug));
            let key = |c: &Check| format!("{}/{}", crate::env_key(&service, &env), c.name);
            // The last head's results go before this one's run starts.
            for c in &checks {
                this.check_states.lock().insert(key(c), crate::checks::Run { running: true, deployment: Some(deployment.clone()), started_at: code_sql::now_ms(), ..Default::default() });
                this.check_failures.lock().remove(&key(c));
            }
            for c in &checks {
                let began = std::time::Instant::now();
                let started = code_sql::now_ms();
                if cancel.is_cancelled() {
                    return;
                }
                let got = this.run_browser_check(&service, &env, &format!("{}{}", url.trim_end_matches('/'), c.path), c, &cancel, Some(&deployment)).await;
                let mut runs = this.check_states.lock();
                let r = runs.entry(key(c)).or_default();
                // A newer head started meanwhile: its own run decides.
                if r.deployment.as_deref() != Some(deployment.as_str()) {
                    continue;
                }
                r.running = false;
                r.last_at = started;
                r.ms = began.elapsed().as_millis() as u64;
                // A preview fails what production would: over a budget too.
                match got {
                    Ok(Passed { status, over_budget: None }) => (r.status, r.error, r.failures, r.down_since) = (status, None, 0, None),
                    Ok(Passed { status, over_budget: Some(why) }) => (r.status, r.error, r.failures, r.down_since) = (status, Some(why), 1, Some(code_sql::now_ms())),
                    Err(why) => (r.status, r.error, r.failures, r.down_since) = (None, Some(why), 1, Some(code_sql::now_ms())),
                }
            }
            this.changed(&service);
        });
    }

    /// Whether a request names a browser check's token for this environment: roll's own visit,
    /// past the environment's gates.
    pub(crate) fn check_token_admits(&self, token: &str, service: &str, env: &str) -> bool {
        self.check_tokens.lock().get(token).is_some_and(|(s, e)| s == service && e == env)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_runner_says_how_it_went() {
        let l = |s: &str| vec!["chromium started".to_string(), s.to_string()];
        assert_eq!(super::result(&l(r#"ROLL-CHECK {"ok":true,"status":200,"ms":812}"#)), Ok(super::Passed { status: Some(200), over_budget: None }));
        assert_eq!(super::result(&l(r#"ROLL-CHECK {"ok":true,"status":200,"over_budget":"over its budget: LCP 3 ms > 1 ms"}"#)).unwrap().over_budget.as_deref(), Some("over its budget: LCP 3 ms > 1 ms"));
        assert_eq!(super::result(&l(r#"ROLL-CHECK {"ok":false,"status":200,"error":"locator('h1') not visible"}"#)), Err("locator('h1') not visible".into()));
        assert!(super::result(&l("Segmentation fault")).is_err());
        assert!(super::dockerfile().contains("playwright-core@1.65.1") && super::dockerfile().starts_with("FROM mcr.microsoft.com/playwright:v1.65.1-noble"));
    }
}
