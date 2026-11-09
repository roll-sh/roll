//! Agent sandboxes on Fargate (`ROLL_SANDBOX_BACKEND=fargate`; docs/code/agents.md,
//! "Sandboxes"): each running sandbox is one task of the sandbox template (the sandbox image's
//! `roll agent up`, and `fetch` before it), in the isolated security group, with a task role
//! that can do nothing. Its backend id is its name, `roll-sbx-<id>`, which every task of it is
//! started as.
//!
//! A stopped sandbox has no task, so starting one again needs its env (the machine token, the
//! owner's provider keys) again: this backend keeps it sealed with `ROLL_SECRET_KEY` in S3
//! (`<prefix>sandboxes/<id>.sealed`) until the sandbox is deleted. Each start hands it to the
//! task the way checks get theirs: in a bundle behind a presigned URL, never in the `RunTask`
//! request. `fetch` writes it to `/roll/env` (owned by the sandbox's user, who removes it once
//! it's read).
//!
//! Not kept across a stop: the task's storage, so the sandbox's home (provider logins, the
//! daemon's `agent.db`) starts empty after a sleep. docs/cloud/README.md, "Isolated work".

use crate::fargate::{Fargate, Launch, container_override, fargate_size, FETCH};
use async_trait::async_trait;
use code_control::agents::sandbox::{BackendError, BackendState, SandboxBackend, SandboxSpec, Tagged};
use code_review::secret::Sealer;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStoreExt, PutPayload};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

/// The group every sandbox task is started in.
pub const GROUP: &str = "roll-sandbox";
/// The sandbox image's user (`deploy/agent/Dockerfile`), who gets the env file.
const SANDBOX_UID: u32 = 10001;

pub fn task_name(sandbox_id: &str) -> String {
    format!("roll-{}", sandbox_id.replace('_', "-"))
}

fn sandbox_id(name: &str) -> Option<String> {
    let rest = name.strip_prefix("roll-sbx-")?;
    (!rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| format!("sbx_{rest}"))
}

/// What a start needs, kept sealed.
#[derive(Serialize, Deserialize)]
struct Kept {
    env: BTreeMap<String, String>,
    cpu: u32,
    memory_gb: u32,
    disk_gb: u32,
}

pub struct FargateSandboxes {
    fargate: Arc<Fargate>,
    template: String,
    sealer: Sealer,
    /// The task each sandbox started here last has (a fresh `RunTask` may not be listed yet).
    tasks: Mutex<HashMap<String, String>>,
}

fn transient(e: impl std::fmt::Display) -> BackendError {
    BackendError::Transient(e.to_string())
}

impl FargateSandboxes {
    /// `secret_key`: `ROLL_SECRET_KEY`, which seals what a start needs.
    pub fn new(fargate: Arc<Fargate>, secret_key: &str) -> anyhow::Result<FargateSandboxes> {
        let template = fargate.cfg.sandbox_task.clone().ok_or_else(|| anyhow::anyhow!("ROLL_SANDBOX_BACKEND=fargate needs ROLL_CODE_FARGATE_SANDBOX_TASK"))?;
        let sealer = Sealer::new(secret_key).map_err(|e| anyhow::anyhow!("ROLL_SECRET_KEY: {e}"))?;
        Ok(FargateSandboxes { fargate, template, sealer, tasks: Mutex::default() })
    }

    fn kept_key(&self, sandbox_id: &str) -> ObjPath {
        ObjPath::from(format!("{}sandboxes/{sandbox_id}.sealed", self.fargate.cfg.prefix))
    }

    fn scope(sandbox_id: &str) -> String {
        format!("fargate-sandbox:{sandbox_id}")
    }

    async fn keep(&self, spec: &SandboxSpec) -> Result<(), BackendError> {
        let kept = Kept { env: spec.env.clone(), cpu: spec.size.cpu, memory_gb: spec.size.memory_gb, disk_gb: spec.size.disk_gb };
        let json = serde_json::to_string(&kept).map_err(transient)?;
        let sealed = self.sealer.seal(&Self::scope(&spec.id), &json);
        self.fargate.s3().put(&self.kept_key(&spec.id), PutPayload::from(sealed.into_bytes())).await.map_err(transient)?;
        Ok(())
    }

    async fn kept(&self, sandbox_id: &str) -> Result<Kept, BackendError> {
        let r = match self.fargate.s3().get(&self.kept_key(sandbox_id)).await {
            Ok(r) => r,
            Err(object_store::Error::NotFound { .. }) => return Err(BackendError::Permanent("what this sandbox starts with is gone".into())),
            Err(e) => return Err(transient(e)),
        };
        let bytes = r.bytes().await.map_err(transient)?;
        let sealed = String::from_utf8(bytes.to_vec()).map_err(|_| BackendError::Permanent("unreadable".into()))?;
        let json = self.sealer.open(&Self::scope(sandbox_id), &sealed).map_err(|_| BackendError::Permanent("what this sandbox starts with doesn't open with ROLL_SECRET_KEY".into()))?;
        serde_json::from_str(&json).map_err(|_| BackendError::Permanent("unreadable".into()))
    }

    /// The task of `name` that's meant to run, if any: the one started here, else what ECS lists.
    async fn task(&self, name: &str) -> Result<Option<String>, BackendError> {
        let known = self.tasks.lock().get(name).cloned();
        if let Some(arn) = known {
            match self.fargate.describe(&arn).await.map_err(transient)? {
                Some(t) if t.desired_status() != Some("STOPPED") => return Ok(Some(arn)),
                _ => {
                    self.tasks.lock().remove(name);
                }
            }
        }
        Ok(self.fargate.list(Some(name)).await.map_err(transient)?.into_iter().next())
    }

    async fn launch(&self, sandbox_id: &str) -> Result<(), BackendError> {
        let name = task_name(sandbox_id);
        let kept = self.kept(sandbox_id).await?;
        let bundle = format!("{name}-{}", crate::docker::random_suffix());
        self.fargate.upload(&bundle, kept.env, None).await.map_err(transient)?;
        let url = self.fargate.bundle_url(&bundle).await.map_err(transient)?;
        let (cpu, memory) = fargate_size(kept.cpu.max(1) as f64, kept.memory_gb.max(1) << 10);
        // Fargate gives 20 GiB unless asked; 21-200 when asked.
        let disk = (kept.disk_gb > 20).then(|| kept.disk_gb.min(200) as i32);
        let containers = vec![container_override(FETCH, Fargate::fetch_command(&url, "/roll/tree", Some(SANDBOX_UID)))];
        let launched = self
            .fargate
            .launch(Launch { name: &name, group: GROUP, task_def: &self.template, spot: false, cpu, memory, disk_gib: disk, containers })
            .await;
        let arn = match launched {
            Ok(arn) => arn,
            Err(e) => {
                self.fargate.delete_bundle(&bundle).await;
                return Err(transient(format!("{e:#}")));
            }
        };
        self.tasks.lock().insert(name, arn.clone());
        // The bundle goes once `fetch` has it (or the task stopped).
        let fargate = self.fargate.clone();
        tokio::spawn(async move {
            let fetched = tokio::time::timeout(
                Duration::from_secs(900),
                fargate.wait(&arn, |t| t.containers().iter().any(|c| c.name() == Some(FETCH) && c.exit_code().is_some())),
            )
            .await;
            if fetched.is_err() {
                warn!(task = %arn, "a sandbox's fetch took over 15 minutes; deleting its bundle");
            }
            fargate.delete_bundle(&bundle).await;
        });
        Ok(())
    }

    async fn stop_all(&self, name: &str, reason: &str) -> Result<(), BackendError> {
        let known = self.tasks.lock().remove(name);
        if let Some(arn) = known {
            self.fargate.stop_task(&arn, reason).await;
        }
        for arn in self.fargate.list(Some(name)).await.map_err(transient)? {
            self.fargate.stop_task(&arn, reason).await;
        }
        Ok(())
    }
}

fn check(name: &str) -> Result<String, BackendError> {
    sandbox_id(name).ok_or_else(|| BackendError::Permanent(format!("{name:?} isn't a sandbox task")))
}

#[async_trait]
impl SandboxBackend for FargateSandboxes {
    fn name(&self) -> &'static str {
        "fargate"
    }

    async fn create(&self, spec: &SandboxSpec) -> Result<String, BackendError> {
        let name = task_name(&spec.id);
        if self.task(&name).await?.is_some() {
            return Ok(name);
        }
        self.keep(spec).await?;
        self.launch(&spec.id).await?;
        Ok(name)
    }

    async fn find(&self, sandbox_id: &str) -> Result<Option<String>, BackendError> {
        let name = task_name(sandbox_id);
        Ok(self.task(&name).await?.map(|_| name))
    }

    async fn tagged(&self) -> Result<Vec<Tagged>, BackendError> {
        let arns = self.fargate.list(None).await.map_err(transient)?;
        let tasks = self.fargate.describe_all(&arns).await.map_err(transient)?;
        Ok(tasks
            .iter()
            .filter(|t| t.group() == Some(GROUP))
            .filter_map(|t| {
                let name = t.started_by()?;
                Some(Tagged { backend_id: name.to_string(), sandbox_id: sandbox_id(name)? })
            })
            .collect())
    }

    async fn status(&self, backend_id: &str) -> Result<BackendState, BackendError> {
        check(backend_id)?;
        let Some(arn) = self.task(backend_id).await? else { return Ok(BackendState::Stopped) };
        let Some(t) = self.fargate.describe(&arn).await.map_err(transient)? else { return Ok(BackendState::Stopped) };
        Ok(match t.last_status().unwrap_or("") {
            "PROVISIONING" | "PENDING" | "ACTIVATING" => BackendState::Creating,
            "RUNNING" => BackendState::Running,
            "STOPPED" if t.stop_code().is_some_and(|c| c.as_str() == "TaskFailedToStart") => {
                BackendState::Failed(t.stopped_reason().unwrap_or("the task failed to start").to_string())
            }
            "STOPPED" => BackendState::Stopped,
            _ => BackendState::Busy,
        })
    }

    async fn start(&self, backend_id: &str) -> Result<(), BackendError> {
        let id = check(backend_id)?;
        if self.task(backend_id).await?.is_some() {
            return Ok(());
        }
        self.launch(&id).await
    }

    async fn stop(&self, backend_id: &str) -> Result<(), BackendError> {
        check(backend_id)?;
        self.stop_all(backend_id, "roll: the sandbox stopped").await
    }

    async fn delete(&self, backend_id: &str) -> Result<(), BackendError> {
        let id = check(backend_id)?;
        self.stop_all(backend_id, "roll: the sandbox was deleted").await?;
        match self.fargate.s3().delete(&self.kept_key(&id)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(transient(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_map_to_sandboxes() {
        assert_eq!(task_name("sbx_abc1"), "roll-sbx-abc1");
        assert_eq!(sandbox_id("roll-sbx-abc1").as_deref(), Some("sbx_abc1"));
        for bad in ["roll-sbx-", "roll-check-x", "roll-sbx-a-b", "sbx-a"] {
            assert!(sandbox_id(bad).is_none(), "{bad}");
        }
    }
}
