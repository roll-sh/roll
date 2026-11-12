//! Checks in containers, on whichever [`Isolation`] the node uses. A tree is copied in, the
//! check runs as `sh -c <run>` with resource limits and a timeout, and the tail of its output
//! becomes the check's summary. Whatever happens, everything made for the run is removed.

use crate::config::{CheckSpec, FILE, RollToml};
use crate::docker::{self, Tail};
use crate::isolation::{Isolation, Job, Kind};
use async_trait::async_trait;
use code_control::Runner;
use code_control::db::now;
use code_control::model::{Change, Check, CheckStatus, RepoRow};
use parking_lot::Mutex;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// The check that reports a broken `roll.toml`.
pub const CONFIG_CHECK: &str = "roll.toml";
/// Summaries are stored up to 4000 characters; leave room for the status line.
const SUMMARY_BYTES: usize = 3500;

pub struct CheckRunner {
    isolation: Arc<dyn Isolation>,
    slots: Semaphore,
}

impl CheckRunner {
    /// `concurrency`: checks running at once from this node (each gets its own memory limit).
    pub fn new(isolation: Arc<dyn Isolation>, concurrency: usize) -> CheckRunner {
        CheckRunner { isolation, slots: Semaphore::new(concurrency.max(1)) }
    }
}

pub fn queued(name: &str) -> Check {
    Check { name: name.into(), status: CheckStatus::Queued, source: "runner".into(), url: None, summary: None, started_at: None, finished_at: None }
}

fn finished(name: &str, status: CheckStatus, summary: String, started: i64) -> Check {
    Check {
        name: name.into(),
        status,
        source: "runner".into(),
        url: None,
        summary: Some(summary),
        started_at: Some(started),
        finished_at: Some(now()),
    }
}

#[async_trait]
impl Runner for CheckRunner {
    async fn plan(&self, _repo: &RepoRow, dir: &Path) -> anyhow::Result<Vec<Check>> {
        Ok(match RollToml::load(dir) {
            Ok(None) => Vec::new(),
            Ok(Some(t)) => t.checks.keys().map(|n| queued(n)).collect(),
            Err(e) => vec![finished(CONFIG_CHECK, CheckStatus::Failure, e, now())],
        })
    }

    fn validate(&self, text: &str) -> Option<String> {
        RollToml::parse(text).err()
    }

    async fn remove_leftovers(&self) {
        self.isolation.remove_leftovers(Kind::Check).await;
    }

    async fn run(&self, repo: &RepoRow, change: Option<&Change>, dir: &Path, check: &Check) -> anyhow::Result<Check> {
        let spec = match RollToml::load(dir) {
            Ok(Some(t)) => t.checks.get(&check.name).cloned(),
            Ok(None) => None,
            Err(e) => return Ok(finished(&check.name, CheckStatus::Failure, e, now())),
        };
        let Some(spec) = spec else {
            return Ok(finished(&check.name, CheckStatus::Skipped, format!("{FILE} no longer declares this check"), now()));
        };
        let _slot = self.slots.acquire().await?;
        let mut env = vec![
            ("CI".to_string(), "true".to_string()),
            ("ROLL_REPO".into(), repo.full_name()),
            ("ROLL_CHECK".into(), check.name.clone()),
        ];
        if let Some(c) = change {
            env.push(("ROLL_CHANGE".into(), c.id.clone()));
            env.push(("ROLL_CHANGESET".into(), c.head.clone()));
        }
        let started = now();
        let out = run_check(&self.isolation, &spec, dir, &env).await;
        let (status, line) = match out.result {
            Outcome::Exited(0) => (CheckStatus::Success, format!("passed in {}", secs(out.elapsed))),
            Outcome::Exited(code) => (CheckStatus::Failure, format!("failed with exit code {code} after {}", secs(out.elapsed))),
            Outcome::TimedOut => (CheckStatus::Failure, format!("timed out after {}", secs(spec.timeout))),
            Outcome::Error(e) => (CheckStatus::Failure, format!("couldn't run: {e}")),
        };
        let summary = if out.log.is_empty() { line } else { format!("{line}\n\n{}", out.log) };
        Ok(finished(&check.name, status, summary, started))
    }
}

fn secs(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 60 { format!("{}m{}s", s / 60, s % 60) } else { format!("{}.{}s", s, d.subsec_millis() / 100) }
}

pub enum Outcome {
    Exited(i32),
    TimedOut,
    Error(String),
}

pub struct RunOutput {
    pub result: Outcome,
    pub log: String,
    pub elapsed: Duration,
}

/// Longest preparing a check's container (pulling its image, copying the tree in) may take.
pub const PREPARE_TIMEOUT: Duration = Duration::from_secs(600);

/// Prepare the container, run it attached, and always remove it. The check's timeout covers the
/// run, not preparing: a timeout that cut `docker create` (or `RunTask`) short could let the
/// container appear after it was removed, so it's removed again once it could have.
pub async fn run_check(isolation: &Arc<dyn Isolation>, spec: &CheckSpec, dir: &Path, env: &[(String, String)]) -> RunOutput {
    let job = Job {
        kind: Kind::Check,
        name: format!("roll-check-{}", docker::random_suffix()),
        image: spec.image.clone(),
        run: spec.run.clone(),
        env: spec.env.iter().map(|(k, v)| (k.clone(), v.clone())).chain(env.iter().cloned()).collect(),
        memory_mb: spec.memory_mb,
        cpus: spec.cpus,
        port: None,
        change: None,
        deadline: PREPARE_TIMEOUT + spec.timeout + Duration::from_secs(60),
    };
    let name = job.name.clone();
    let tail = Arc::new(Mutex::new(Tail::new(SUMMARY_BYTES)));
    let start = Instant::now();
    let result = match tokio::time::timeout(PREPARE_TIMEOUT, isolation.prepare(&job, dir)).await {
        Ok(Ok(())) => match tokio::time::timeout(spec.timeout, isolation.attach(&name, tail.clone())).await {
            Ok(Ok(code)) => Outcome::Exited(code),
            Ok(Err(e)) => Outcome::Error(format!("{e:#}")),
            Err(_) => Outcome::TimedOut,
        },
        Ok(Err(e)) => Outcome::Error(format!("{e:#}")),
        Err(_) => Outcome::Error(format!("creating the container took over {}", secs(PREPARE_TIMEOUT))),
    };
    isolation.remove(&name).await;
    if matches!(result, Outcome::TimedOut | Outcome::Error(_)) {
        late_remove(isolation, name);
    }
    let log = tail.lock().text();
    RunOutput { result, log, elapsed: start.elapsed() }
}

/// Remove a run again a minute later: a create (or `RunTask`) cut short may still finish.
fn late_remove(isolation: &Arc<dyn Isolation>, name: String) {
    let isolation = isolation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        isolation.remove(&name).await;
    });
}
