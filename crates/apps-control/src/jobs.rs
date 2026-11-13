//! One-off jobs (`POST …/environments/{env}/jobs`, `roll apps run -- <cmd>`): a command in a
//! fresh container of the environment's active deployment, with its variables and on its
//! project network, as Render's one-off jobs and `heroku run` are. Its output is the log
//! `job:<id>`; it ends at its exit, its timeout or a cancel, and its container goes with it. No
//! port and no volume: the service's own instance may be using the volume.

use crate::logs::Stream;
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

impl Apps {
    pub async fn start_job(self: &Arc<Self>, service: &str, env: &str, body: Value) -> Result<Job> {
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| !["command", "timeout_s"].contains(&k.as_str())) {
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
        let live = self.load_live(service, env).await?;
        let Some(tpl) = live.template() else { return Err(Error::Conflict(format!("nothing is deployed in {env} yet"))) };
        let key = crate::env_key(service, env);
        {
            let running = self.running_jobs.lock();
            if running.values().filter(|k| **k == key).count() >= MAX_PER_ENV || running.len() >= MAX_PER_NODE {
                return Err(Error::Conflict(format!("at most {MAX_PER_ENV} jobs run at once in an environment; wait for one to end or cancel it")));
            }
        }
        let (sid, e, dep, cmd) = (service.to_string(), env.to_string(), tpl.deployment.clone(), command.to_string());
        let job = self.db(move |s| s.add_job(&sid, &e, &dep, &cmd)).await?;
        self.running_jobs.lock().insert(job.id.clone(), key);
        let log = self.logs.sink(format!("job:{}", job.id), Stream::Stdout);
        log.stream(Stream::System).line(format!("$ {command}"));
        let cancel = CancellationToken::new();
        self.cancels.lock().insert(job.id.clone(), cancel.clone());
        let (this, id, cmd) = (self.clone(), job.id.clone(), command.to_string());
        tokio::spawn(async move {
            let ran = live.run_to_exit(Some(&cmd), Duration::from_secs(timeout), Some(log.clone()), cancel, |_| {}).await;
            let (status, code) = match ran {
                Ok(e) if e.canceled => (RunStatus::Canceled, None),
                Ok(e) if e.timed_out => (RunStatus::TimedOut, None),
                Ok(e) if e.code == Some(0) => (RunStatus::Succeeded, Some(0)),
                Ok(e) => (RunStatus::Failed, e.code),
                Err(err) => {
                    log.stream(Stream::System).line(format!("the job didn't start: {err:#}"));
                    (RunStatus::Failed, None)
                }
            };
            log.stream(Stream::System).line(format!("{} ({})", status.as_str(), code.map_or("no exit code".into(), |c| format!("exit {c}"))));
            this.cancels.lock().remove(&id);
            this.running_jobs.lock().remove(&id);
            let id2 = id.clone();
            let _ = this.db(move |s| s.finish_job(&id2, status, code)).await;
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
