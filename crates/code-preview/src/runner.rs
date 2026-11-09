//! Checks in containers. A tree is copied in (never bind-mounted, so a check can't write to the
//! host and leaves nothing root-owned behind), the check runs as `sh -c <run>` with resource
//! limits and a timeout, and the tail of its output becomes the check's summary.

use crate::config::{CheckSpec, FILE, RollToml};
use crate::docker::{self, LABEL, Tail};
use async_trait::async_trait;
use code_control::Runner;
use code_control::db::now;
use code_control::model::{Change, Check, CheckStatus, RepoRow};
use parking_lot::Mutex;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::Semaphore;

/// The check that reports a broken `roll.toml`.
pub const CONFIG_CHECK: &str = "roll.toml";
/// Summaries are stored up to 4000 characters; leave room for the status line.
const SUMMARY_BYTES: usize = 3500;

pub struct DockerRunner {
    slots: Semaphore,
}

impl DockerRunner {
    /// `concurrency`: checks running at once on this host (each gets its own memory limit).
    pub fn new(concurrency: usize) -> DockerRunner {
        DockerRunner { slots: Semaphore::new(concurrency.max(1)) }
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
impl Runner for DockerRunner {
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
        let out = run_check(&spec, dir, &env).await;
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

/// Longest creating a check's container (pulling its image) and copying the tree in may take.
const PREPARE_TIMEOUT: Duration = Duration::from_secs(600);

/// Create the container, copy the tree in, run it attached, and always remove it. The check's
/// timeout covers the run, not creating the container: a timeout that cut `docker create` short
/// could let the container appear after it was removed.
pub async fn run_check(spec: &CheckSpec, dir: &Path, env: &[(String, String)]) -> RunOutput {
    let name = format!("roll-check-{}", docker::random_suffix());
    let tail = Arc::new(Mutex::new(Tail::new(SUMMARY_BYTES)));
    let start = Instant::now();
    let result = match tokio::time::timeout(PREPARE_TIMEOUT, prepare(&name, spec, dir, env)).await {
        Ok(Ok(())) => match tokio::time::timeout(spec.timeout, run_attached(&name, tail.clone())).await {
            Ok(Ok(code)) => Outcome::Exited(code),
            Ok(Err(e)) => Outcome::Error(format!("{e:#}")),
            Err(_) => Outcome::TimedOut,
        },
        Ok(Err(e)) => Outcome::Error(format!("{e:#}")),
        Err(_) => {
            // `docker create` may still finish: remove the container again once it could have.
            let late = name.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(60)).await;
                docker::remove(&late).await;
            });
            Outcome::Error(format!("creating the container took over {}", secs(PREPARE_TIMEOUT)))
        }
    };
    docker::remove(&name).await;
    let log = tail.lock().text();
    RunOutput { result, log, elapsed: start.elapsed() }
}

/// Create the container and copy the tree in.
async fn prepare(name: &str, spec: &CheckSpec, dir: &Path, env: &[(String, String)]) -> anyhow::Result<()> {
    let mut args: Vec<String> = vec![
        "create".into(),
        "--name".into(),
        name.into(),
        "--label".into(),
        format!("{LABEL}=check"),
        "--workdir".into(),
        "/workspace".into(),
        "--add-host".into(),
        "host.docker.internal:host-gateway".into(),
    ];
    args.extend(docker::limits(spec.memory_mb, spec.cpus));
    for (k, v) in spec.env.iter().chain(env.iter().map(|(k, v)| (k, v))) {
        args.push("--env".into());
        args.push(format!("{k}={v}"));
    }
    args.extend(["--entrypoint".into(), "sh".into(), spec.image.clone(), "-c".into(), spec.run.clone()]);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    docker::docker(&argv).await?;
    let src = format!("{}/.", dir.display());
    docker::docker(&["cp", &src, &format!("{name}:/workspace")]).await?;
    Ok(())
}

/// Start the prepared container attached; its exit code.
async fn run_attached(name: &str, tail: Arc<Mutex<Tail>>) -> anyhow::Result<i32> {
    let mut child = Command::new("docker")
        .args(["start", "--attach", name])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let out = tokio::spawn(docker::drain(child.stdout.take().expect("piped"), tail.clone()));
    let err = tokio::spawn(docker::drain(child.stderr.take().expect("piped"), tail));
    let status = child.wait().await?;
    let _ = out.await;
    let _ = err.await;
    Ok(status.code().unwrap_or(-1))
}
