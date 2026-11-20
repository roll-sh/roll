//! One-off jobs (`POST …/environments/{env}/jobs`, `roll apps run -- <cmd>`): a command in a
//! fresh container of the environment's active deployment, with its variables and on its
//! project network, as Render's one-off jobs and `heroku run` are. Its output is the log
//! `job:<id>`; it ends at its exit, its timeout or a cancel, and its container goes with it. No
//! port and no volume: the service's own instance may be using the volume.

use crate::logs::Stream;
use futures::StreamExt;
use crate::model::{Job, RunStatus};
use crate::{Apps, Error, Result};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// A job's limit when the request names none, and the most it may name.
const DEFAULT_TIMEOUT_S: u64 = 3600;
const MAX_TIMEOUT_S: u64 = 24 * 3600;
/// Jobs running at once in an environment, and on a node.
const MAX_PER_ENV: usize = 5;
const MAX_PER_NODE: usize = 50;
/// Job containers a node runs at once (a job's tasks each take one).
pub const MAX_CONTAINERS: usize = 50;
/// A job's tasks at most, how many of them run at once unless it says, and its tries after a
/// task's failure at most.
const MAX_TASKS: u64 = 100;
const PARALLEL: u32 = 10;
const MAX_RETRIES: u64 = 5;

impl Apps {
    pub async fn start_job(self: &Arc<Self>, service: &str, env: &str, body: Value) -> Result<Job> {
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| !["command", "timeout_s", "tasks", "retries", "parallelism"].contains(&k.as_str())) {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        let command = o.get("command").and_then(Value::as_str).map(str::trim).unwrap_or("");
        if command.is_empty() || command.len() > 2000 || command.contains('\0') {
            return Err(Error::BadRequest("command is a command of 1–2000 characters".into()));
        }
        let timeout = match o.get("timeout_s") {
            None | Some(Value::Null) => DEFAULT_TIMEOUT_S,
            Some(v) => v.as_u64().filter(|t| (1..=MAX_TIMEOUT_S).contains(t)).ok_or_else(|| Error::BadRequest(format!("timeout_s is 1–{MAX_TIMEOUT_S}")))?,
        };
        let tasks = match o.get("tasks") {
            None | Some(Value::Null) => 1,
            Some(v) => v.as_u64().filter(|t| (1..=MAX_TASKS).contains(t)).ok_or_else(|| Error::BadRequest(format!("tasks is 1–{MAX_TASKS}")))? as u32,
        };
        let retries = match o.get("retries") {
            None | Some(Value::Null) => 0,
            Some(v) => v.as_u64().filter(|r| *r <= MAX_RETRIES).ok_or_else(|| Error::BadRequest(format!("retries is 0–{MAX_RETRIES}")))? as u32,
        };
        let parallelism = match o.get("parallelism") {
            None | Some(Value::Null) => PARALLEL.min(tasks),
            Some(v) => v.as_u64().filter(|p| (1..=MAX_TASKS).contains(p)).ok_or_else(|| Error::BadRequest(format!("parallelism is 1–{MAX_TASKS}")))? as u32,
        };
        if self.capped(service) {
            return Err(Error::Conflict("this project reached its spending limit for the month".into()));
        }
        let live = self.load_live(service, env).await?;
        if live.paused() {
            return Err(Error::Conflict("this environment is paused: resume it to run a job".into()));
        }
        let Some(tpl) = live.template() else { return Err(Error::Conflict(format!("nothing is deployed in {env} yet"))) };
        // The slot is taken under the lock that counts them (a placeholder until the job has an
        // id), so requests at once can't all pass the count.
        let key = crate::env_key(service, env);
        let hold = format!("pending:{}", crate::model::random(12));
        {
            let mut running = self.running_jobs.lock();
            if running.values().filter(|k| **k == key).count() >= MAX_PER_ENV || running.len() >= MAX_PER_NODE {
                return Err(Error::Conflict(format!("at most {MAX_PER_ENV} jobs run at once in an environment; wait for one to end or cancel it")));
            }
            running.insert(hold.clone(), key.clone());
        }
        let (sid, e, dep, cmd) = (service.to_string(), env.to_string(), tpl.deployment.clone(), command.to_string());
        let made = self.db(move |s| s.add_job(&sid, &e, &dep, &cmd, tasks, retries, parallelism)).await;
        let job = {
            let mut running = self.running_jobs.lock();
            running.remove(&hold);
            let job = made?;
            running.insert(job.id.clone(), key);
            job
        };
        self.logs.redact_like(&format!("env:{service}/{env}"), &format!("job:{}", job.id));
        let log = self.logs.sink(format!("job:{}", job.id), Stream::Stdout);
        log.stream(Stream::System).line(format!("$ {command}"));
        let cancel = CancellationToken::new();
        self.cancels.lock().insert(job.id.clone(), cancel.clone());
        let (this, id, cmd) = (self.clone(), job.id.clone(), command.to_string());
        let sid = service.to_string();
        tokio::spawn(async move {
            // Its tasks, `parallelism` at once, each told which it is and which try; a failed or
            // timed-out task tried again up to `retries` times, never after a cancel.
            let outcomes: Vec<(RunStatus, Option<i32>)> = futures::stream::iter(0..tasks)
                .map(|n| {
                    let (live, cmd, log, cancel, slots, retry_after) = (live.clone(), cmd.clone(), log.clone(), cancel.clone(), this.job_slots.clone(), this.cfg.cron_retry_after);
                    async move {
                        let mut attempt = 1;
                        loop {
                            let outcome = {
                                let Ok(_slot) = slots.clone().acquire_owned().await else { return (RunStatus::Failed, None) };
                                let vars = [("ROLL_TASK_INDEX".to_string(), n.to_string()), ("ROLL_TASK_COUNT".to_string(), tasks.to_string()), ("ROLL_TASK_ATTEMPT".to_string(), attempt.to_string())];
                                let ran = live.run_to_exit(Some(&cmd), &vars, Duration::from_secs(timeout), Some(log.clone()), cancel.clone(), None).await;
                                match ran {
                                    Ok(e) if e.canceled => (RunStatus::Canceled, None),
                                    Ok(e) if e.timed_out => (RunStatus::TimedOut, None),
                                    Ok(e) if e.code == Some(0) => (RunStatus::Succeeded, Some(0)),
                                    Ok(e) => (RunStatus::Failed, e.code),
                                    Err(err) => {
                                        log.stream(Stream::System).line(format!("the job didn't start: {err:#}"));
                                        (RunStatus::Failed, None)
                                    }
                                }
                            };
                            let task = if tasks > 1 { format!("task {n}") } else { "the job".to_string() };
                            let again = matches!(outcome.0, RunStatus::Failed | RunStatus::TimedOut) && attempt <= retries && !cancel.is_cancelled();
                            if !again {
                                if tasks > 1 || attempt > 1 {
                                    log.stream(Stream::System).line(format!("{task}: {} (try {attempt})", outcome.0.as_str()));
                                }
                                return outcome;
                            }
                            // As a cron run's retries wait (30 s, doubling, ten minutes at most), its
                            // container slot let go meanwhile.
                            let wait = retry_after.saturating_mul(1 << (attempt - 1).min(10)).min(Duration::from_secs(600));
                            log.stream(Stream::System).line(format!("{task}: {} (try {attempt} of {}); again in {} s", outcome.0.as_str(), retries + 1, wait.as_secs()));
                            tokio::select! {
                                _ = tokio::time::sleep(wait) => {}
                                _ = cancel.cancelled() => return (RunStatus::Canceled, None),
                            }
                            attempt += 1;
                        }
                    }
                })
                .buffer_unordered(parallelism as usize)
                .collect()
                .await;
            // The job: canceled, else timed out, else the first failure, else done.
            let first = |s: RunStatus| outcomes.iter().find(|(st, _)| *st == s).copied();
            let (status, code) = first(RunStatus::Canceled).or_else(|| first(RunStatus::TimedOut)).or_else(|| first(RunStatus::Failed)).unwrap_or((RunStatus::Succeeded, Some(0)));
            log.stream(Stream::System).line(format!("{} ({})", status.as_str(), code.map_or("no exit code".into(), |c| format!("exit {c}"))));
            this.cancels.lock().remove(&id);
            this.running_jobs.lock().remove(&id);
            // A job that failed (not one canceled) is its `job.failed` event (off by default), in
            // the same transaction, if it was still running (a takeover may have failed it).
            let id2 = id.clone();
            let _ = this.db(move |s| s.finish_job(&id2, status, code)).await;
            // Whoever follows the service sees it end.
            this.changed(&sid);
            this.logs.flush().await;
            this.logs.retire(&format!("job:{id}"));
        });
        Ok(job)
    }

    /// Cancel the jobs running in an environment on this node (it's deleted, or another node
    /// owns the service now): none keeps running with its variables.
    pub(crate) fn cancel_jobs(&self, service: &str, env: &str) {
        let key = crate::env_key(service, env);
        let ids: Vec<String> = self.running_jobs.lock().iter().filter(|(_, k)| **k == key).map(|(id, _)| id.clone()).collect();
        let cancels = self.cancels.lock();
        for id in ids {
            if let Some(t) = cancels.get(&id) {
                t.cancel();
            }
        }
    }

    /// Cancel every job of a service running on this node (its project reached its spend limit).
    pub(crate) fn cancel_service_jobs(&self, service: &str) {
        let prefix = format!("{service}/");
        let ids: Vec<String> = self.running_jobs.lock().iter().filter(|(_, k)| k.starts_with(&prefix)).map(|(id, _)| id.clone()).collect();
        let cancels = self.cancels.lock();
        for id in ids {
            if let Some(t) = cancels.get(&id) {
                t.cancel();
            }
        }
    }

    pub async fn job(self: &Arc<Self>, service: &str, env: &str, id: &str) -> Result<Job> {
        let (sid, e, id) = (service.to_string(), env.to_string(), id.to_string());
        self.db(move |s| s.job(&sid, &e, &id)).await
    }

    pub async fn jobs(self: &Arc<Self>, service: &str, env: &str) -> Result<Vec<Job>> {
        let (sid, e) = (service.to_string(), env.to_string());
        self.db(move |s| {
            s.environment(&sid, &e)?;
            s.jobs(&sid, &e, 50)
        })
        .await
    }

    pub async fn cancel_job(self: &Arc<Self>, service: &str, env: &str, id: &str) -> Result<Job> {
        let job = self.job(service, env, id).await?;
        if job.status != RunStatus::Running {
            return Err(Error::Conflict(format!("{id} is {}: only a running job can be canceled", job.status.as_str())));
        }
        if let Some(t) = self.cancels.lock().get(id) {
            t.cancel();
        }
        Ok(job)
    }
}
