//! Deploys (docs/apps/README.md, "Deploys"): one queue per service and environment on the
//! service's owner. A deployment is built (or its image pulled), rolled out next to the old one,
//! health-checked, and switched to with a compare-and-swap; the old instances drain.
//!
//! Also the owner's background loops: the reaper and meter, deploy-on-push from roll code's
//! land log, cron schedules, and custom domains' DNS checks.

use crate::live::{EnvLive, Template};
use crate::logs::{LogSink, Stream};
use crate::model::*;
use crate::runtime::{BuildJob, InstanceSpec};
use crate::store::{EnvRow, NewDeployment, ServiceRow, Switch};
use crate::{Apps, Error, PENDING_CLAIM_TTL, Result, fingerprint, lease_key, network_name, volume_name};
use bytes::Bytes;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

impl Apps {
    fn queue(&self, service: &str, env: &str) -> (Arc<Notify>, bool) {
        let mut q = self.queues.lock();
        let key = crate::env_key(service, env);
        match q.get(&key) {
            Some(n) => (n.clone(), false),
            None => {
                let n = Arc::new(Notify::new());
                q.insert(key, n.clone());
                (n, true)
            }
        }
    }

    /// Wake the environment's deploy worker (starting it if this node has none).
    pub(crate) fn kick(self: &Arc<Self>, service: &str, env: &str) {
        let (n, fresh) = self.queue(service, env);
        if fresh {
            let (this, s, e) = (self.clone(), service.to_string(), env.to_string());
            tokio::spawn(async move { this.worker(s, e).await });
        }
        n.notify_one();
    }

    /// `POST …/deploys`: queue a deployment of the service's source (`revision` for a repo,
    /// `image` to deploy another image), or of `upload` (a `roll up` tree).
    /// With `key` (an `Idempotency-Key`), a retry answers the deployment the first call queued.
    pub async fn deploy(self: &Arc<Self>, service: &str, env: &str, body: Value, by: By, upload: Option<Bytes>, key: Option<&str>) -> Result<Deployment> {
        self.refuse_capped(service)?;
        let o = body.as_object().cloned().unwrap_or_default();
        for k in o.keys() {
            if !["revision", "image", "message", "no_traffic", "no_cache"].contains(&k.as_str()) {
                return Err(Error::BadRequest(format!("unknown field {k}")));
            }
        }
        let text = |k: &str| -> Result<Option<String>> {
            match o.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.clone())),
                Some(_) => Err(Error::BadRequest(format!("{k} is a string"))),
            }
        };
        let (revision, image, message) = (text("revision")?, text("image")?, text("message")?);
        // Built and health-checked, not switched to: its own URL runs it (Cloud Run's
        // `--no-traffic`); a rollback to it puts it live.
        let no_traffic = match o.get("no_traffic") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(Error::BadRequest("no_traffic is a boolean".into())),
        };
        // Render's "Clear build cache & deploy": for a build a stale cached layer keeps breaking.
        let no_cache = match o.get("no_cache") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(Error::BadRequest("no_cache is a boolean".into())),
        };
        if let Some(k) = key {
            if k.is_empty() || k.len() > 200 || !k.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(Error::BadRequest("Idempotency-Key is 1–200 printable ASCII characters".into()));
            }
            let (sid, e, kk) = (service.to_string(), env.to_string(), k.to_string());
            if let Some(d) = self.db(move |s| s.keyed_deployment(&sid, &e, &kk)).await? {
                // A retry repeats its request; the same key on another one is a client's bug,
                // never silently the first request's deployment.
                // What was stored is resolved (a branch or short sha becomes the full sha) and cut
                // to 500 characters: compare like with like, and never call a branch a change.
                let full_sha = |r: &str| r.len() >= 7 && r.bytes().all(|b| b.is_ascii_hexdigit());
                let rev_differs = revision.as_ref().is_some_and(|r| full_sha(r) && !d.revision.as_deref().is_some_and(|have| have.starts_with(r.as_str())));
                let msg_differs = message.as_ref().is_some_and(|m| m.chars().take(500).collect::<String>() != d.message);
                let differs = rev_differs || msg_differs;
                if differs {
                    return Err(Error::Conflict(format!("Idempotency-Key {k} was used for another deploy ({})", d.id)));
                }
                return Ok(d);
            }
        }
        let (sid, e) = (service.to_string(), env.to_string());
        let (svc, row) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e).ok()))).await?;
        let row = match row {
            Some(r) => r,
            // Deploying into an environment makes it (previews: PUT first is optional).
            None => {
                self.put_environment(service, env, Value::Object(Default::default())).await?;
                let (sid, e) = (service.to_string(), env.to_string());
                self.db(move |s| s.environment(&sid, &e)).await?
            }
        };
        let mut message = message.unwrap_or_default();
        let trigger = by.trigger;
        let (revision, image, trigger) = match (&svc.source, upload.is_some(), image) {
            // An upload from a git checkout names its commit (for the list; nothing reads it).
            (_, true, _) => (revision.clone(), None, Trigger::Upload),
            (_, false, Some(img)) => {
                Source::Image { image: img.clone() }.check()?;
                (None, Some(img), trigger)
            }
            (Source::Upload, false, None) => return Err(Error::BadRequest("this service deploys uploads: run `roll up` in its directory, or redeploy (`POST …/redeploy`) to apply variables and settings".into())),
            (Source::Image { image }, false, None) => (None, Some(image.clone()), trigger),
            (Source::Repo { repo, .. }, false, None) => {
                let sources = self.deps.sources.as_ref().ok_or_else(|| Error::BadRequest("roll code isn't on this node".into()))?;
                let rev = match &revision {
                    Some(r) => sources.revision(repo, r).await,
                    None => sources.resolve(repo, row.branch.as_deref()).await,
                }
                .map_err(|e| Error::BadRequest(format!("{repo}: {e}")))?;
                if message.is_empty() {
                    message = rev.message.lines().next().unwrap_or("").to_string();
                }
                (Some(rev.sha), None, trigger)
            }
        };
        if message.chars().count() > 500 {
            message = message.chars().take(500).collect();
        }
        if let Some(up) = &upload
            && up.len() > self.cfg.upload_max
        {
            return Err(Error::TooLarge(format!("an upload is at most {} MiB", self.cfg.upload_max >> 20)));
        }
        let upload_key = upload.as_ref().map(|_| format!("apps/uploads/{}.tgz", random(16)));
        if let (Some(key), Some(bytes)) = (&upload_key, upload) {
            match &self.deps.objects {
                Some(o) => o.put(key, bytes).await.map_err(|e| Error::Internal(format!("storing the upload: {e}")))?,
                None => {
                    let path = self.cfg.home.join(key);
                    tokio::fs::create_dir_all(path.parent().expect("a parent")).await.map_err(anyhow::Error::from)?;
                    tokio::fs::write(&path, &bytes).await.map_err(anyhow::Error::from)?;
                }
            }
        }
        let d = NewDeployment { trigger, revision, message, image, plan: None, rollback_of: None, run: svc.for_env(&row)?.run, upload: upload_key, status: DeployStatus::Queued, actor: by.actor, no_traffic, no_cache };
        if no_cache && d.image.is_some() {
            return Err(Error::BadRequest("no_cache is for a deploy that builds; an image is pulled as it is".into()));
        }
        let (sid, e, k) = (service.to_string(), env.to_string(), key.map(str::to_string));
        let ours = d.upload.clone().filter(|_| key.is_some());
        if no_traffic && (svc.run.kind != RunType::Web || svc.run.volume.is_some()) {
            return Err(Error::BadRequest("no_traffic is for a web service without a volume (its deployment runs next to the live one)".into()));
        }
        let dep = self.db(move |s| s.create_deployment(&sid, &e, d, "", k.as_deref())).await?;
        self.changed(service);
        if let Some(up) = ours {
            let id = dep.id.clone();
            if self.db(move |s| s.deployment_upload(&id)).await.ok().flatten().as_deref() != Some(up.as_str()) {
                // A retry with the same key won the race: its deployment has its own upload.
                self.delete_upload(&up).await;
            }
        }
        self.deploy_log(&dep.id).line(format!("queued ({})", dep.trigger.as_str()));
        let _ = row;
        self.kick(service, env);
        Ok(dep)
    }

    /// `POST …/redeploy`: the active deployment's image again, with the service's current run
    /// settings and variables (variables are read when its instances are made), without a build
    /// and without moving to another revision. What a change of variables or settings needs,
    /// for every source (an upload service has no source to deploy again).
    /// With `key` (an `Idempotency-Key`), a retry answers the redeploy the first call queued, as a
    /// deploy's does (one table of keys per environment for both).
    pub async fn redeploy(self: &Arc<Self>, service: &str, env: &str, body: Value, actor: Option<String>, key: Option<&str>) -> Result<Deployment> {
        self.refuse_capped(service)?;
        if let Some(k) = key {
            if k.is_empty() || k.len() > 200 || !k.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(Error::BadRequest("Idempotency-Key is 1–200 printable ASCII characters".into()));
            }
            let (sid, e, kk) = (service.to_string(), env.to_string(), k.to_string());
            if let Some(d) = self.db(move |s| s.keyed_deployment(&sid, &e, &kk)).await? {
                if d.trigger != Trigger::Settings {
                    return Err(Error::Conflict(format!("Idempotency-Key {k} was used for another deploy ({})", d.id)));
                }
                return Ok(d);
            }
        }
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| k.as_str() != "message") {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        let message = match o.get("message") {
            None | Some(Value::Null) => None,
            Some(Value::String(m)) => Some(m.chars().take(500).collect::<String>()),
            Some(_) => return Err(Error::BadRequest("message is a string".into())),
        };
        let (sid, e) = (service.to_string(), env.to_string());
        let (svc, row) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await?;
        let active = row.active.clone().ok_or_else(|| Error::Conflict(format!("nothing is deployed in {env} yet")))?;
        let old = self.deployment(service, env, &active).await?;
        let image = old.image.clone().ok_or_else(|| Error::Conflict(format!("{active} has no image")))?;
        let mut run = self.run_for(&svc, &old, env).await;
        if run.port.is_none() {
            run.port = old.run.port;
        }
        let d = NewDeployment {
            trigger: Trigger::Settings,
            revision: old.revision.clone(),
            message: message.unwrap_or_else(|| format!("Redeploy of {}", old.id)),
            image: Some(image),
            plan: old.plan.clone(),
            rollback_of: None,
            run,
            upload: None,
            status: DeployStatus::Queued,
            actor,
            no_traffic: false,
            no_cache: false,
        };
        let (sid, e) = (service.to_string(), env.to_string());
        let k = key.map(str::to_string);
        let dep = self.db(move |s| s.create_deployment(&sid, &e, d, "", k.as_deref())).await?;
        self.deploy_log(&dep.id).line("queued (redeploy)");
        self.kick(service, env);
        Ok(dep)
    }

    /// `POST …/environments/{env}/promote` `{from}`: deploy what `from` runs (its image, the
    /// revision it was built from) to `env`, without a build, with `env`'s own variables and the
    /// service's current settings (Railway's promotion: staging's tested image to production).
    pub async fn promote(self: &Arc<Self>, service: &str, env: &str, body: Value, actor: Option<String>) -> Result<Deployment> {
        self.refuse_capped(service)?;
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| !["from", "message"].contains(&k.as_str())) {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        let from = o.get("from").and_then(Value::as_str).ok_or_else(|| Error::BadRequest("from (the environment to promote) is required".into()))?;
        if from == env {
            return Err(Error::BadRequest("from is another environment".into()));
        }
        let message = o.get("message").and_then(Value::as_str).map(|m| m.chars().take(500).collect::<String>());
        let (sid, e, f) = (service.to_string(), env.to_string(), from.to_string());
        let (svc, _, source) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?, s.environment(&sid, &f)?))).await?;
        let active = source.active.clone().ok_or_else(|| Error::Conflict(format!("nothing is deployed in {from} yet")))?;
        let old = self.deployment(service, from, &active).await?;
        let image = old.image.clone().ok_or_else(|| Error::Conflict(format!("{active} has no image")))?;
        let mut run = self.run_for(&svc, &old, env).await;
        if run.port.is_none() {
            run.port = old.run.port;
        }
        let d = NewDeployment {
            trigger: Trigger::Promote,
            revision: old.revision.clone(),
            message: message.unwrap_or_else(|| format!("Promote {} from {from}", old.id)),
            image: Some(image),
            plan: old.plan.clone(),
            rollback_of: Some(old.id.clone()),
            run,
            upload: None,
            status: DeployStatus::Queued,
            actor,
            no_traffic: false,
            no_cache: false,
        };
        let (sid, e) = (service.to_string(), env.to_string());
        let dep = self.db(move |s| s.create_deployment(&sid, &e, d, "", None)).await?;
        self.deploy_log(&dep.id).line(format!("queued (promoting {} from {from})", old.id));
        self.kick(service, env);
        Ok(dep)
    }

    /// `POST …/deploys/{dep}/rollback`: deploy an earlier deployment's image and settings
    /// again, without a build.
    /// `keep_variables`: run its image with the variables as they are; else (the default) its
    /// variables come back as they were when it was live (Heroku's and Railway's rollbacks), the
    /// change in the audit trail. A deployment that never went live has none kept.
    pub async fn rollback(self: &Arc<Self>, service: &str, env: &str, dep: &str, actor: Option<String>, keep_variables: bool) -> Result<Deployment> {
        self.refuse_capped(service)?;
        let old = self.deployment(service, env, dep).await?;
        let Some(image) = old.image.clone() else { return Err(Error::Conflict(format!("{dep} never built an image, so there's nothing to roll back to"))) };
        if old.pruned_at.is_some() {
            return Err(Error::Conflict(format!("{dep} is older than what's kept to roll back to and its image was cleaned up; deploy its revision again")));
        }
        let did = old.id.clone();
        if !keep_variables && let Some(then) = self.db(move |s| s.snapshot(&did)).await? {
            let (sid, e) = (service.to_string(), env.to_string());
            let now = self.db(move |s| s.variables(&sid, &e)).await?;
            let set: std::collections::BTreeMap<String, String> = then.vars.iter().filter(|(k, v)| now.get(*k) != Some(*v)).map(|(k, v)| (k.clone(), v.clone())).collect();
            let unset: Vec<String> = now.keys().filter(|k| !then.vars.contains_key(*k)).cloned().collect();
            if !set.is_empty() || !unset.is_empty() {
                // Sealed as they were then (and what's sealed since stays so).
                let (sid, e, s2, u2, sealed) = (service.to_string(), env.to_string(), set.clone(), unset.clone(), then.sealed);
                self.db(move |s| s.change_variables(&sid, &e, &s2, &u2, &sealed)).await?;
                self.settings_changed();
                self.audit("variables.restored", service, env, actor.clone(), json!({ "from": old.id, "set": set.keys().collect::<Vec<_>>(), "unset": unset })).await;
            }
        }
        let d = NewDeployment {
            trigger: Trigger::Rollback,
            revision: old.revision.clone(),
            message: format!("Roll back to {}", old.id),
            image: Some(image),
            plan: old.plan.clone(),
            rollback_of: Some(old.id.clone()),
            run: old.run.clone(),
            upload: None,
            status: DeployStatus::Queued,
            actor,
            no_traffic: false,
            no_cache: false,
        };
        let (sid, e) = (service.to_string(), env.to_string());
        let dep = self.db(move |s| s.create_deployment(&sid, &e, d, "", None)).await?;
        self.kick(service, env);
        Ok(dep)
    }

    /// `POST …/rollback` with no deployment: back to the one live before what runs, walking back
    /// through rollbacks (after B → A, the next goes to what came before A, never forward to B).
    pub async fn rollback_previous(self: &Arc<Self>, service: &str, env: &str, actor: Option<String>, keep_variables: bool) -> Result<Deployment> {
        let (sid, e) = (service.to_string(), env.to_string());
        let (row, all) = self.db(move |s| Ok((s.environment(&sid, &e)?, s.deployments(&sid, &e, 100, None)?))).await?;
        let Some(active) = row.active.and_then(|a| all.iter().find(|d| d.id == a).cloned()) else {
            return Err(Error::Conflict(format!("{env} has nothing live to roll back from")));
        };
        // What the live image is: a rollback's (or promotion's) original, else itself.
        let base = active.rollback_of.as_ref().and_then(|o| all.iter().find(|d| &d.id == o)).unwrap_or(&active);
        let target = all
            .iter()
            .filter(|d| d.id != active.id && d.id != base.id && d.image.is_some() && d.created_at < base.created_at)
            // Live once, then replaced (a superseded deploy never ran).
            .filter(|d| d.status == DeployStatus::Inactive)
            .max_by_key(|d| d.created_at)
            .ok_or_else(|| Error::Conflict(format!("{env} has no earlier deployment to roll back to")))?;
        let id = target.id.clone();
        self.rollback(service, env, &id, actor, keep_variables).await
    }

    pub async fn cancel(self: &Arc<Self>, service: &str, env: &str, dep: &str) -> Result<Deployment> {
        let d = self.deployment(service, env, dep).await?;
        if !d.status.pending() {
            return Err(Error::Conflict(format!("{dep} is {}: only a queued or running deploy can be canceled", d.status.as_str())));
        }
        let id = dep.to_string();
        self.db(move |s| s.set_status(&id, &[DeployStatus::Queued, DeployStatus::Building, DeployStatus::Deploying], DeployStatus::Canceled, Some("canceled"))).await?;
        if let Some(t) = self.cancels.lock().get(dep) {
            t.cancel();
        }
        self.deploy_log(dep).stream(Stream::System).line("canceled");
        self.deployment(service, env, dep).await
    }

    pub async fn deployments(self: &Arc<Self>, service: &str, env: &str, before: Option<i64>) -> Result<Vec<Deployment>> {
        let (sid, e) = (service.to_string(), env.to_string());
        self.db(move |s| {
            s.environment(&sid, &e)?;
            s.deployments(&sid, &e, 100, before)
        })
        .await
    }

    /// Old deployments of the services this node owns, past what each environment keeps
    /// (`keep_deployments`): pruned in the store, then their images (once no deployment names
    /// them), static sites and build logs go. Every ten minutes.
    pub async fn prune(self: &Arc<Self>) {
        let Ok(services) = self.db(|s| s.services(None)).await else { return };
        let (keep, day_ago) = (self.cfg.keep_deployments.max(1), code_sql::now_ms() - self.cfg.keep_recent.as_millis() as i64);
        for svc in services.into_iter().filter(|s| self.owns(&s.id)) {
            let sid = svc.id.clone();
            let Ok(pruned) = self.db(move |s| s.prune_deployments(&sid, keep, day_ago)).await else { continue };
            for p in pruned {
                if let Some(img) = &p.image {
                    self.deps.builder.remove_image(img).await;
                    // A site is keyed by its digest: another service's image with the same one
                    // (a byte-identical build) still serves it.
                    if let Some(k) = crate::sites::key(img) {
                        let kk = k.clone();
                        if !self.db(move |s| s.digest_in_use(&kk)).await.unwrap_or(true) {
                            self.drop_site(&k).await;
                        }
                    }
                }
                self.logs.delete(&format!("build:{}", p.id)).await;
            }
        }
    }

    pub async fn deployment(self: &Arc<Self>, service: &str, env: &str, dep: &str) -> Result<Deployment> {
        let id = dep.to_string();
        let mut d = self.db(move |s| s.deployment(&id)).await?;
        if d.service != service || d.environment != env {
            return Err(Error::NotFound(format!("no deployment {dep} of {service} in {env}")));
        }
        let id = dep.to_string();
        d.steps = self.db(move |s| s.steps(&id)).await?;
        Ok(d)
    }

    /// `GET …/deploys/{dep}/commits`: what a repo deployment brings over the one it's measured
    /// against (Render's deploy view): the live one, or for the live one, the one live before
    /// it. Up to 50 commits, newest first, and whether that's all; null when there's nothing to
    /// compare (not a repo deployment, the first one, a source that can't tell).
    pub async fn deploy_commits(self: &Arc<Self>, service: &str, env: &str, dep: &str) -> Result<Value> {
        let d = self.deployment(service, env, dep).await?;
        let (sid, e) = (service.to_string(), env.to_string());
        let (svc, row, all) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?, s.deployments(&sid, &e, 100, None)?))).await?;
        let none = json!({ "since": null, "commits": null, "complete": false });
        let (Source::Repo { repo, .. }, Some(to), Some(sources)) = (&svc.source, d.revision.as_deref(), self.deps.sources.clone()) else { return Ok(none) };
        let since = if row.active.as_deref() == Some(dep) {
            all.iter().filter(|x| x.created_at < d.created_at && x.revision.is_some() && x.image.is_some() && x.status == DeployStatus::Inactive).max_by_key(|x| x.created_at)
        } else {
            all.iter().find(|x| row.active.as_deref() == Some(x.id.as_str()))
        };
        let Some(since) = since.filter(|s| s.revision.is_some()) else { return Ok(none) };
        let from = since.revision.clone().unwrap_or_default();
        match sources.commits(repo, &from, to, 50).await {
            Ok(Some((commits, complete))) => Ok(json!({ "since": { "id": since.id, "revision": from }, "commits": commits, "complete": complete })),
            Ok(None) => Ok(none),
            Err(e) => Err(Error::BadRequest(format!("{repo}: {e}"))),
        }
    }

    pub fn deploy_log(self: &Arc<Self>, dep: &str) -> LogSink {
        self.logs.sink(format!("build:{dep}"), Stream::Build).deployment(dep)
    }

    /// The environment's deploy worker: one deployment at a time, oldest first, while this
    /// node owns the service.
    async fn worker(self: Arc<Self>, service: String, env: String) {
        let (notify, _) = self.queue(&service, &env);
        loop {
            if self.stop.is_cancelled() {
                break;
            }
            let (s, e) = (service.clone(), env.clone());
            let next = self.db(move |st| Ok(st.pending_deployments(&s)?.into_iter().find(|d| d.environment == e))).await;
            match next {
                Ok(Some(d)) if self.owns(&service) => {
                    self.run(d).await;
                    continue;
                }
                Ok(_) => {}
                Err(e) => warn!(service, env, error = %e, "apps: reading the deploy queue failed"),
            }
            tokio::select! {
                _ = notify.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                _ = self.stop.cancelled() => break,
            }
        }
    }

    /// A deployment's step began (for its summary; a failed write only loses the summary).
    async fn step(self: &Arc<Self>, service: &str, deployment: &str, step: &str) {
        let (id, step) = (deployment.to_string(), step.to_string());
        let _ = self.db(move |s| s.mark_step(&id, &step)).await;
        self.changed(service);
    }

    async fn run(self: &Arc<Self>, d: Deployment) {
        let cancel = CancellationToken::new();
        self.cancels.lock().insert(d.id.clone(), cancel.clone());
        let log = self.deploy_log(&d.id);
        let began = Instant::now();
        self.report(&d, "pending", "deploying").await;
        let result = self.run_inner(&d, &log, cancel.clone()).await;
        self.cancels.lock().remove(&d.id);
        let id = d.id.clone();
        match result {
            Ok(()) => {
                log.stream(Stream::System).line(format!("deployed in {:.1} s", began.elapsed().as_secs_f64()));
                info!(service = %d.service, env = %d.environment, deployment = %d.id, "apps: deployed");
                let did = d.id.clone();
                let ready = self.db(move |s| s.deployment(&did)).await.is_ok_and(|x| x.status == DeployStatus::Ready);
                self.report(&d, "success", if ready { "ready, not live (no traffic)" } else { "live" }).await;
            }
            Err(e) if cancel.is_cancelled() => {
                let _ = e;
                self.report(&d, "failure", "canceled").await;
            }
            Err(e) => {
                // roll's words go out (the commit status, notifications); what the app or the
                // build printed stays roll's to show its owner (it can hold anything).
                let detail = e.downcast_ref::<DeployFailure>().map(|f| f.detail.clone());
                self.report(&d, "failure", &e.to_string()).await;
                self.step(&d.service, &d.id, "failed").await;
                let msg = e.to_string();
                log.stream(Stream::System).line(format!("failed: {msg}"));
                if let Some(x) = &detail {
                    log.stream(Stream::System).line(x.clone());
                }
                let _ = self.db(move |s| s.fail_deployment(&id, &msg, detail.as_deref())).await;
                warn!(service = %d.service, env = %d.environment, deployment = %d.id, error = %e, "apps: a deploy failed");
            }
        }
        self.logs.flush().await;
        self.logs.retire(&format!("build:{}", d.id));
        self.changed(&d.service);
    }

    /// A repo deployment's progress as a commit status on its revision (`roll/apps/<service>/<env>`,
    /// linking to the environment): pending while it's on its way, then success or failure, as
    /// Railway's and Vercel's GitHub deployments. Best effort: a status that doesn't post is a
    /// warning, never a failed deploy.
    async fn report(self: &Arc<Self>, d: &Deployment, state: &'static str, what: &str) {
        let (Some(sources), Some(rev)) = (self.deps.sources.clone(), d.revision.clone()) else { return };
        let (sid, e) = (d.service.clone(), d.environment.clone());
        let Ok((svc, env)) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await else { return };
        let Source::Repo { repo, .. } = &svc.source else { return };
        let status = crate::integrations::CommitStatus {
            context: format!("roll/apps/{}/{}", svc.name, d.environment),
            state,
            description: what.chars().take(140).collect(),
            target_url: Some(self.url(&env.slug)),
        };
        if let Err(err) = sources.set_status(repo, &rev, &status).await {
            warn!(service = %d.service, deployment = %d.id, error = %err, "apps: a deploy's commit status didn't post");
        }
    }

    /// While a push's build runs, a newer push queued for the environment (on any node: the queue
    /// is the store) stops it: superseded, its build minutes saved, the newer one next. A deploy
    /// that's rolling out finishes. Stops watching when dropped.
    fn supersede_on_newer_push(self: &Arc<Self>, d: &Deployment, cancel: CancellationToken) -> Option<tokio_util::sync::DropGuard> {
        if d.trigger != Trigger::Push {
            return None;
        }
        let (this, d, done) = (self.clone(), d.clone(), CancellationToken::new());
        let guard = done.clone().drop_guard();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(this.cfg.supersede_every) => {}
                    _ = done.cancelled() => return,
                }
                let (sid, e, own, after) = (d.service.clone(), d.environment.clone(), d.id.clone(), d.created_at);
                match this.db(move |s| s.newer_push_queued(&sid, &e, &own, after)).await {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(err) => {
                        warn!(deployment = %d.id, error = %err, "apps: looking for a newer push failed");
                        continue;
                    }
                }
                let id = d.id.clone();
                if this.db(move |s| s.set_status(&id, &[DeployStatus::Building], DeployStatus::Superseded, Some("a newer push replaced it"))).await.unwrap_or(false) {
                    this.deploy_log(&d.id).stream(Stream::System).line("stopped: a newer push replaced it");
                    cancel.cancel();
                }
                return;
            }
        });
        Some(guard)
    }

    async fn run_inner(self: &Arc<Self>, d: &Deployment, log: &LogSink, cancel: CancellationToken) -> anyhow::Result<()> {
        // Queued before its project reached its spend limit: it doesn't build or start now.
        anyhow::ensure!(!self.capped(&d.service), "the project reached its spending limit for the month");
        let (sid, e) = (d.service.clone(), d.environment.clone());
        let (svc, env) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await?;
        // The environment's own size and scale over the service's, before roll.toml.
        let svc = svc.for_env(&env)?;
        let id = d.id.clone();
        self.step(&d.service, &d.id, "build").await;
        let (image, run) = match &d.image {
            // A rollback, a redeploy or a promotion: an image roll already has, nothing to build
            // or pull.
            Some(img) if matches!(d.trigger, Trigger::Rollback | Trigger::Settings | Trigger::Promote) => {
                self.db(move |s| s.set_status(&id, &[DeployStatus::Queued, DeployStatus::Building], DeployStatus::Deploying, None)).await?;
                log.line(format!("using the image of {}", d.rollback_of.as_deref().unwrap_or("the deployment it redeploys")));
                (img.clone(), d.run.clone())
            }
            Some(img) => {
                self.db(move |s| s.set_status(&id, &[DeployStatus::Queued], DeployStatus::Building, None)).await?;
                let sid = svc.id.clone();
                self.check_registry(img).await?;
                // The login goes only to the registry it's for, whatever the image names now.
                let login = self
                    .db(move |s| s.registry_login(&sid))
                    .await?
                    .filter(|(r, _)| crate::model::registry_key(&r.server) == crate::model::image_registry(img))
                    .map(|(r, password)| crate::runtime::RegistryLogin { server: r.server, username: r.username, password });
                let pinned = self.deps.runtime.pull(img, log, login.as_ref()).await?;
                log.line(format!("pinned {pinned}"));
                // A preview's defaults (an image service has no roll.toml), frozen with it.
                let (run, note) = crate::config::preview_defaults(svc.run.clone(), None);
                if env.ephemeral
                    && let Some(n) = &note
                {
                    log.line(n);
                }
                let run = if env.ephemeral { run } else { svc.run.clone() };
                let (id, p, r) = (d.id.clone(), pinned.clone(), run.clone());
                self.db(move |s| s.set_built(&id, &p, None, Some(&r))).await?;
                (pinned, run)
            }
            None => {
                if !self.db(move |s| s.set_status(&id, &[DeployStatus::Queued], DeployStatus::Building, None)).await? {
                    anyhow::bail!("the deployment was canceled");
                }
                let _watch = self.supersede_on_newer_push(d, cancel.clone());
                self.build(&svc, d, log, cancel.clone()).await?
            }
        };
        if cancel.is_cancelled() {
            anyhow::bail!("canceled");
        }
        // Canceled or superseded while it built (the status is the truth, whoever changed it).
        let id = d.id.clone();
        if !self.db(move |s| s.set_status(&id, &[DeployStatus::Building, DeployStatus::Deploying], DeployStatus::Deploying, None)).await? {
            cancel.cancel();
            anyhow::bail!("canceled");
        }
        let mut dep = d.clone();
        dep.image = Some(image);
        dep.run = run;
        // The plan the build just stored (a single-page app, say).
        let did = d.id.clone();
        if let Ok(stored) = self.db(move |s| s.deployment(&did)).await {
            dep.plan = stored.plan;
        }
        // A static site: its files out of the image, so they're served without an instance.
        if dep.plan.as_ref().is_some_and(|p| p["start"] == apps_build::STATIC_START)
            && dep.run.kind == RunType::Web
            && dep.run.volume.is_none()
            && let Some(img) = dep.image.clone()
        {
            match self.export_site(&img).await {
                Ok(Some(n)) => log.stream(Stream::System).line(format!("static site: {} KiB of files, served without an instance", n.div_ceil(1024))),
                Ok(None) => {}
                Err(e) => log.stream(Stream::System).line(format!("static site: its files couldn't be exported ({e:#}); a container serves it")),
            }
        }
        // Read before the instances' values are (a change in between then says "changed", the
        // safe side, never that nothing did): the secrets' digest, the environment's own
        // variables (what a rollback to it restores, kept with its switch to live) and the hash
        // of everything it reads.
        let secrets_digest = self.deps.secrets.digest(&svc.project, &env.name).await.ok().flatten();
        let (sid, e, did) = (svc.id.clone(), env.name.clone(), d.id.clone());
        let snapshot = self.db(move |s| s.seal_snapshot(&did, &sid, &e)).await?;
        let vars_hash = {
            let r = self.resolve_vars(&svc, &env, &dep.id).await?;
            fingerprint(&r.vars_for_hash(), &svc)
        };
        let tpl = Arc::new(self.template(&svc, &env, &dep).await?);
        let did = d.id.clone();
        let no_traffic = self.db(move |s| s.no_traffic(&did)).await?;
        // What was built decides: a roll.toml can make it a worker, or give it a volume.
        anyhow::ensure!(!no_traffic || (tpl.run.kind == RunType::Web && tpl.run.volume.is_none()), "no_traffic is for a web service without a volume, and this build isn't one");
        // A try-out never migrates the shared database: its pre-deploy command runs when it's put
        // live.
        if let Some(cmd) = tpl.run.pre_deploy.clone().filter(|_| !no_traffic) {
            self.step(&d.service, &d.id, "pre_deploy").await;
            self.pre_deploy(&tpl, &cmd, log, &cancel).await?;
        }
        self.step(&d.service, &d.id, "rollout").await;
        // The project's secrets as the instances read them (a digest, no value): what "secrets
        // changed since" compares with. Best effort: without one, nothing says so.
        if let Some(digest) = secrets_digest {
            let did = d.id.clone();
            if let Err(e) = self.db(move |s| s.set_secrets_digest(&did, &digest)).await {
                warn!(deployment = %d.id, error = %e, "apps: the secrets' digest wasn't kept");
            }
        }
        let live = self.live(&svc.id, &env.name);
        // Instances of the old deployment are known here (after a restart: loaded from the store).
        self.load_live(&svc.id, &env.name).await?;
        log.stream(Stream::System).line(match tpl.run.kind {
            RunType::Web => "rolling out: starting the new deployment next to the old one".to_string(),
            RunType::Worker => "rolling out the worker".to_string(),
            RunType::Cron => "the next scheduled run uses this deployment".to_string(),
        });
        let prepared = match live.prepare(&tpl, None).await {
            Ok(p) => p,
            // What the app said before it failed (a stack trace, a missing variable), in the deploy
            // log and the error: its own lines are otherwise only in the environment's log.
            Err(e) => {
                let said: Vec<_> = self.logs.follow(&format!("env:{}/{}", svc.id, env.name), 2000).0.into_iter().filter(|l| l.instance.is_some() && l.deployment.as_deref() == Some(d.id.as_str())).collect();
                let said = &said[said.len().saturating_sub(40)..];
                if !said.is_empty() {
                    log.stream(Stream::System).line("the new instance's output:");
                    for l in said {
                        log.stream(l.stream).line(format!("{}: {}", l.instance.as_deref().unwrap_or(""), l.text));
                    }
                }
                return Err(match said.iter().rev().find(|l| !l.text.trim().is_empty()) {
                    Some(l) => anyhow::Error::new(DeployFailure { what: e.to_string(), detail: format!("its last output: {}", l.text.trim().chars().take(200).collect::<String>()) }),
                    None => e,
                });
            }
        };
        let (made, took) = (prepared.made.clone(), prepared.took);
        // Canceled meanwhile, or the service moved to another node (its lease lost): nothing
        // of this rollout may stay.
        if cancel.is_cancelled() || !self.owns(&svc.id) {
            for i in &made {
                self.deps.runtime.remove(&i.name).await;
            }
            anyhow::bail!("canceled");
        }
        if !made.is_empty() {
            log.stream(Stream::System).line(format!("healthy after {:.1} s", took.as_secs_f64()));
        }
        // No traffic: healthy is as far as it goes; its own URL wakes it when asked.
        if no_traffic {
            for i in &made {
                self.deps.runtime.remove(&i.name).await;
            }
            let did = d.id.clone();
            self.db(move |s| s.set_status(&did, &[DeployStatus::Deploying], DeployStatus::Ready, None)).await?;
            let short = d.id.strip_prefix("dep_").unwrap_or(&d.id);
            log.stream(Stream::System).line(format!("ready, not live: its own URL is {}; roll back to it to put it live", self.url(&format!("{short}--{}", env.slug))));
            return Ok(());
        }
        // A gradual rollout needs something to split: the old deployment running and taking
        // requests. Asleep, it switches at once (nothing would judge the new one).
        // Not in a preview: its traffic is a reviewer or two, nothing to judge a canary by.
        let gradual = !tpl.run.rollout.steps.is_empty() && tpl.run.kind == RunType::Web && !env.ephemeral && env.active.is_some() && live.awake() > 0;
        if !tpl.run.rollout.steps.is_empty() && !gradual && tpl.run.kind == RunType::Web {
            log.stream(Stream::System).line("nothing else is running, so it switches at once");
        }
        let prepared = if gradual {
            live.begin_canary(tpl.clone(), prepared, tpl.run.rollout.steps[0]).await;
            if let Err(e) = self.canary(&svc.id, &d.id, &live, &tpl, log, &cancel).await {
                live.abort_canary().await;
                return Err(e);
            }
            None
        } else {
            Some(prepared)
        };
        match &prepared {
            Some(p) => live.rest_if_asleep(&tpl, p).await,
            // Every request to the new deployment from here: once the store says it's live, no
            // visitor gets the old one (a failed switch takes the canary out again).
            None => live.set_share(100),
        }
        let (sid, e, from, to, vh, snap) = (svc.id.clone(), env.name.clone(), env.active.clone(), d.id.clone(), vars_hash.clone(), snapshot.clone());
        let switched: anyhow::Result<()> = async {
            // Epochs only grow on a backend that outlives the process; the in-memory one starts
            // again at 1, and a fence kept in the store would then refuse every later switch.
            // A node whose lease lapsed mid-rollout has no epoch: it isn't the owner any more and
            // never switches (the node that took the service over does).
            let epoch = match Some(&self.deps.cluster).filter(|c| c.backend_name() != "memory") {
                Some(c) => Some(c.epoch(&crate::lease_key(&svc.id)).ok_or_else(|| anyhow::anyhow!("this node no longer holds the service; the node that took it over deploys"))?),
                None => None,
            };
            let ready = (!made.is_empty()).then_some(took.as_millis() as i64);
            let ok = self
                .db(move |s| s.switch_active(&Switch { service: &sid, env: &e, from: from.as_deref(), to: &to, ready_ms: ready, epoch, vars_hash: Some(&vh), snapshot: Some(&snap) }))
                .await?;
            anyhow::ensure!(ok, "another deploy switched this environment meanwhile");
            Ok(())
        }
        .await;
        // Not switched, for whatever reason: nothing of this deployment keeps running or serving.
        if let Err(e) = switched {
            match prepared {
                Some(_) => {
                    for i in &made {
                        self.deps.runtime.remove(&i.name).await;
                    }
                }
                None => live.abort_canary().await,
            }
            return Err(e);
        }
        match prepared {
            Some(p) => live.commit(tpl, p).await,
            None => {
                live.promote_canary().await;
                log.stream(Stream::System).line("the new deployment takes every request");
            }
        }
        Ok(())
    }

    /// A gradual rollout's steps (docs/apps/README.md, "Gradual rollouts"): each share of
    /// requests for `step_s`, judged as it goes. An error when it's rolled back, canceled or the
    /// service moved; the caller takes the canary out.
    async fn canary(self: &Arc<Self>, service: &str, id: &str, live: &Arc<EnvLive>, tpl: &Template, log: &LogSink, cancel: &CancellationToken) -> anyhow::Result<()> {
        let r = &tpl.run.rollout;
        let every = Duration::from_millis((r.step_s as u64 * 250).min(5000));
        let sys = log.stream(Stream::System);
        for &share in &r.steps {
            live.set_share(share);
            live.take_served();
            self.step(service, id, &format!("canary_{share}")).await;
            sys.line(format!("canary: the new deployment takes {share}% of requests for {} s", r.step_s));
            let until = tokio::time::Instant::now() + Duration::from_secs(r.step_s as u64);
            let (mut new, mut old) = (crate::live::Served::default(), crate::live::Served::default());
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(every) => {}
                    _ = cancel.cancelled() => anyhow::bail!("canceled"),
                }
                anyhow::ensure!(self.owns(service), "this node no longer holds the service; the node that took it over deploys");
                // The old one is the live one: a deployment opened by its own URL meanwhile isn't
                // either side.
                let active = live.template().map(|t| t.deployment.clone());
                for (dep, s) in live.take_served() {
                    let into = if dep == tpl.deployment {
                        &mut new
                    } else if active.as_deref() == Some(dep.as_str()) {
                        &mut old
                    } else {
                        continue;
                    };
                    into.requests += s.requests;
                    into.errors += s.errors;
                    into.crashes += s.crashes;
                }
                if let Some(why) = judge(new, old, r.error_pct) {
                    sys.line(format!("canary: rolled back at {share}%: {why}"));
                    anyhow::bail!("rolled back at {share}% of requests: {why}");
                }
                if tokio::time::Instant::now() >= until {
                    break;
                }
            }
            sys.line(format!("canary: {share}% passed ({} requests, {:.1}% 5xx)", new.requests, new.error_pct()));
        }
        Ok(())
    }

    /// The deployment's pre-deploy command, to its exit: a container of the new image with the
    /// deployment's variables on the project network, without the volume (the old instance may
    /// still be using it) and without a port. Removed afterwards, whatever happened.
    async fn pre_deploy(self: &Arc<Self>, tpl: &Template, cmd: &str, log: &LogSink, cancel: &CancellationToken) -> anyhow::Result<()> {
        let mut spec = tpl.spec.clone();
        let svc = spec.service.strip_prefix("svc_").unwrap_or(&spec.service).to_string();
        spec.name = format!("roll-apps-{svc}-{}-pre-{}", spec.env, random(6).to_ascii_lowercase());
        spec.command = Some(cmd.to_string());
        spec.port = None;
        spec.volume = None;
        spec.aliases.clear();
        let limit = self.cfg.build_timeout;
        log.stream(Stream::System).line(format!("pre-deploy: {cmd}"));
        let rt = &self.deps.runtime;
        rt.create(&spec).await?;
        let ran = async {
            rt.start(&spec.name, log.clone()).await?;
            rt.settle(&spec.name).await;
            tokio::select! {
                code = tokio::time::timeout(limit, rt.wait(&spec.name)) => match code {
                    Ok(code) => code,
                    Err(_) => anyhow::bail!("the pre-deploy command took longer than {} minutes", limit.as_secs() / 60),
                },
                _ = cancel.cancelled() => anyhow::bail!("canceled"),
            }
        }
        .await;
        let _ = rt.stop(&spec.name, Duration::from_secs(5)).await;
        rt.remove(&spec.name).await;
        match ran? {
            0 => {
                log.stream(Stream::System).line("pre-deploy done");
                Ok(())
            }
            n => anyhow::bail!("the pre-deploy command exited with {n}: nothing was switched"),
        }
    }

    /// Build the deployment's tree into an image: the tree from its source, the plan from
    /// `apps-build`, the Dockerfile written in when the plan made one.
    /// An image's registry, by name, must be a public address: dockerd resolves it itself, and
    /// speaks plain HTTP to loopback (`127.0.0.1.nip.io:5000/x` passes [`Source::check`]).
    pub(crate) async fn check_registry(&self, image: &str) -> anyhow::Result<()> {
        let Some(host) = registry_host(image) else { return Ok(()) };
        if self.cfg.allow_private {
            return Ok(());
        }
        let name = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(h, _)| h);
        let name = name.trim_start_matches('[').trim_end_matches(']');
        // A name that doesn't resolve now isn't pulled either (dockerd would resolve it again).
        // A name that changes its answer between here and dockerd's lookup isn't caught: the
        // fleet pulls through CodeBuild and Fargate, outside the node's network.
        let addrs: Vec<_> = tokio::net::lookup_host((name, 443)).await.map_err(|e| anyhow::anyhow!("{host} doesn't resolve: {e}"))?.collect();
        if addrs.is_empty() || addrs.iter().any(|a| private_ip(a.ip())) {
            anyhow::bail!("{host} resolves to a private address, which isn't a registry roll pulls from");
        }
        Ok(())
    }

    async fn build(self: &Arc<Self>, svc: &ServiceRow, d: &Deployment, log: &LogSink, cancel: CancellationToken) -> anyhow::Result<(String, Run)> {
        let began = Instant::now();
        let dir = self.cfg.home.join("apps").join("builds").join(&d.id);
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await?;
        let result = self.build_in(svc, d, log, cancel, &dir).await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
        // The image is the deployment's from here (a rollback never rebuilds): the upload goes.
        let id = d.id.clone();
        if let Ok(Some(key)) = self.db(move |s| s.deployment_upload(&id)).await {
            self.drop_upload(&d.id, &key).await;
        }
        self.live(&svc.id, &d.environment).meter.lock().build(code_sql::now_ms(), began.elapsed().as_secs_f64());
        result
    }

    async fn build_in(self: &Arc<Self>, svc: &ServiceRow, d: &Deployment, log: &LogSink, cancel: CancellationToken, dir: &Path) -> anyhow::Result<(String, Run)> {
        let tree = dir.join("tree");
        tokio::fs::create_dir_all(&tree).await?;
        let root = match (&svc.source, d.trigger) {
            (_, Trigger::Upload) => {
                let id = d.id.clone();
                let key = self.db(move |s| s.deployment_upload(&id)).await?.ok_or_else(|| anyhow::anyhow!("the upload is gone"))?;
                let bytes = match &self.deps.objects {
                    Some(o) => o.get(&key).await?.ok_or_else(|| anyhow::anyhow!("the upload is gone"))?,
                    None => Bytes::from(tokio::fs::read(self.cfg.home.join(&key)).await?),
                };
                log.line(format!("unpacking the upload ({} KiB)", bytes.len() / 1024));
                let (t, env_out) = (tree.clone(), dir.join("env"));
                tokio::task::spawn_blocking(move || roll_fetch::unpack(std::io::Cursor::new(bytes), &t, &env_out, roll_fetch::Limits::default())).await??;
                None
            }
            (Source::Repo { repo, root }, _) => {
                let rev = d.revision.as_deref().ok_or_else(|| anyhow::anyhow!("no revision to build"))?;
                log.line(format!("checking out {repo} at {}", &rev[..rev.len().min(12)]));
                let sources = self.deps.sources.as_ref().ok_or_else(|| anyhow::anyhow!("roll code isn't on this node"))?;
                sources.materialize(repo, rev, &tree).await?;
                root.clone()
            }
            (Source::Upload, _) => anyhow::bail!("this service deploys uploads"),
            (Source::Image { .. }, _) => unreachable!("images aren't built"),
        };
        let mut context: PathBuf = match &root {
            Some(r) => crate::model::in_tree(&tree, "the root directory", r, true)?,
            None => tree.clone(),
        };
        // roll.toml `[app]` in the root: config as code, over the service's settings; a preview
        // environment's defaults last.
        let (sid, e) = (svc.id.clone(), d.environment.clone());
        let ephemeral = self.db(move |s| s.environment(&sid, &e)).await?.ephemeral;
        let scope = crate::config::EnvScope { name: &d.environment, ephemeral };
        let file = crate::config::read(&context).map_err(|e| anyhow::anyhow!("{e}"))?;
        let (mut build_cfg, mut run) = match &file {
            Some(app) => {
                let (b, r, said) = app.apply(&svc.build, &svc.run, Some(scope)).map_err(|e| anyhow::anyhow!("{e}"))?;
                for l in said {
                    log.line(l);
                }
                (b, r)
            }
            None => (svc.build.clone(), svc.run.clone()),
        };
        if ephemeral {
            let note;
            (run, note) = crate::config::preview_defaults(run, file.as_ref());
            if let Some(n) = note {
                log.line(n);
            }
        }
        // A root directory inside a JS workspace builds from the repo's root (the lockfile and the
        // workspace's packages are there), unless it brings its own Dockerfile.
        let mut from_root = false;
        if let Some(r) = &root
            && build_cfg.builder == Builder::Auto
            && std::fs::symlink_metadata(context.join("Dockerfile")).is_err()
        {
            let (t, r2) = (tree.clone(), r.clone());
            if let Some(ws) = tokio::task::spawn_blocking(move || apps_build::workspace_app(&apps_build::DirTree::new(t), &r2)).await? {
                log.line(format!("plan: {}", ws.note));
                if build_cfg.build.is_none() {
                    build_cfg.build = ws.build;
                }
                if build_cfg.start.is_none() && run.start.is_none() {
                    build_cfg.start = ws.start;
                }
                context = tree.clone();
                // The repo root's Procfile is the repo's, not this package's.
                from_root = true;
            }
        }
        // The variables the build's steps see: the environment's, as its instances will.
        let (sid, e) = (svc.id.clone(), d.environment.clone());
        let env_row = self.db(move |s| s.environment(&sid, &e)).await?;
        let mut build_env = self.resolve_vars(svc, &env_row, &d.id).await?.vars;
        // Per deployment (the cache would never hit) or only for the running app.
        build_env.retain(|(k, _)| k != "ROLL_DEPLOYMENT" && k != "PORT");
        build_env.sort();
        let env_hash = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            for (k, v) in &build_env {
                h.update(k.as_bytes());
                h.update([0]);
                h.update(v.as_bytes());
                h.update([0]);
            }
            h.finalize().iter().take(16).map(|b| format!("{b:02x}")).collect::<String>()
        };
        let mut overrides = build_cfg.overrides();
        overrides.build_env = build_env.iter().map(|(k, _)| k.clone()).collect();
        // A Procfile's command for what this service runs (a cron's command is its own).
        overrides.process = match run.kind {
            _ if from_root => None,
            RunType::Web => Some("web".into()),
            RunType::Worker => Some("worker".into()),
            RunType::Cron => None,
        };
        let ctx = context.clone();
        let plan = tokio::task::spawn_blocking(move || apps_build::plan(&apps_build::DirTree::new(ctx), &overrides)).await?.map_err(|e| anyhow::anyhow!("{e}"))?;
        for n in &plan.notes {
            log.line(format!("plan: {n}"));
        }
        // The service's own cache mounts (`isolate_caches`); a deploy without the cache starts
        // them empty, and every build after it uses the new ones.
        let (sid, did) = (svc.id.clone(), d.id.clone());
        let (no_cache, cache_ns) = self.db(move |s| {
            let fresh = s.no_cache(&did)?;
            Ok((fresh, s.build_cache(&sid, fresh)?))
        })
        .await?;
        if no_cache {
            log.line("building without the cache: no cached layers, base images pulled again, the cache mounts empty");
        }
        let dockerfile = if plan.provider == apps_build::Provider::Dockerfile {
            let df = build_cfg.dockerfile.clone().unwrap_or_else(|| "Dockerfile".into());
            crate::model::in_tree(&context, "the Dockerfile", &df, false)?;
            df
        } else {
            let name = "Dockerfile.roll";
            anyhow::ensure!(std::fs::symlink_metadata(context.join(name)).is_err(), "the tree already has a {name}");
            tokio::fs::write(context.join(name), &plan.dockerfile).await?;
            if std::fs::symlink_metadata(context.join(".dockerignore")).is_err() {
                tokio::fs::write(context.join(".dockerignore"), apps_build::DOCKERIGNORE).await?;
            }
            log.line("generated Dockerfile:");
            for l in plan.dockerfile.lines() {
                log.line(format!("  {l}"));
            }
            name.to_string()
        };
        if run.port.is_none() {
            run.port = Some(plan.port);
        }
        // Heroku's release phase: the Procfile's `release:`, when the service has no pre-deploy.
        if run.pre_deploy.is_none() {
            run.pre_deploy = plan.release.clone();
        }
        // On a builder shared by services, the build reads a copy with its cache mounts
        // namespaced, outside the tree (so no `COPY . .` puts it in the image), next to the
        // Dockerfile's own `.dockerignore` if it has one; the namespace never reaches the log.
        let (dockerfile, log, layers) = if self.deps.builder.shares_caches() {
            let at = context.join(&dockerfile);
            let isolated = apps_build::isolate_caches(&tokio::fs::read(&at).await?, &cache_ns);
            let copy = dir.join("Dockerfile.isolated");
            tokio::fs::write(&copy, isolated).await?;
            let own_ignore = at.with_file_name(format!("{}.dockerignore", at.file_name().and_then(|n| n.to_str()).unwrap_or("Dockerfile")));
            if std::fs::symlink_metadata(&own_ignore).is_ok_and(|m| m.is_file()) {
                tokio::fs::copy(&own_ignore, dir.join("Dockerfile.isolated.dockerignore")).await?;
            }
            let layers = {
                use sha2::{Digest, Sha256};
                Sha256::digest(format!("layers:{cache_ns}")).iter().take(16).map(|b| format!("{b:02x}")).collect::<String>()
            };
            (copy.to_string_lossy().into_owned(), log.redacting(&cache_ns), Some(layers))
        } else {
            (dockerfile, log.clone(), None)
        };
        let log = &log;
        let plan_json = serde_json::to_value(&plan)?;
        let job = BuildJob {
            service: svc.id.clone(),
            deployment: d.id.clone(),
            context,
            dockerfile,
            tag: format!("roll-apps-{}:{}", svc.id.strip_prefix("svc_").unwrap_or(&svc.id), d.id.strip_prefix("dep_").unwrap_or(&d.id)),
            cpu: 2.0,
            memory_mb: 4096,
            timeout: self.cfg.build_timeout,
            own_dockerfile: plan.provider == apps_build::Provider::Dockerfile,
            env: build_env,
            env_hash,
            no_cache,
            // A stage of the tree's own Dockerfile; a generated one's are roll's.
            target: svc.build.target.clone().filter(|_| plan.provider == apps_build::Provider::Dockerfile),
            layers,
        };
        log.line(format!("building with {}", self.deps.builder.name()));
        let image = match self.deps.builder.build(&job, log, cancel.clone()).await {
            Ok(i) => i,
            // What failed, as BuildKit said it (the step, its last words), not only its exit code.
            Err(e) if !cancel.is_cancelled() => {
                let lines: Vec<String> = self.logs.follow(&format!("build:{}", d.id), 400).0.into_iter().map(|l| l.text).collect();
                return Err(match build_failure(&lines) {
                    Some(why) if !e.to_string().contains(&why) => anyhow::Error::new(DeployFailure { what: e.to_string(), detail: why }),
                    _ => e,
                });
            }
            Err(e) => return Err(e),
        };
        let (id, img, r) = (d.id.clone(), image.clone(), run.clone());
        self.db(move |s| s.set_built(&id, &img, Some(&plan_json), Some(&r))).await?;
        Ok((image, run))
    }

    /// What a deployment's instances are made from.
    pub async fn template(self: &Arc<Self>, svc: &ServiceRow, env: &EnvRow, dep: &Deployment) -> Result<Template> {
        let image = dep.image.clone().ok_or_else(|| Error::Internal(format!("{} has no image", dep.id)))?;
        let mut run = dep.run.clone();
        if run.kind == RunType::Web && run.port.is_none() {
            run.port = Some(8080);
        }
        let mut probe = svc.clone();
        probe.run.port = run.port;
        let resolved = self.resolve_vars(&probe, env, &dep.id).await?;
        if !resolved.unresolved.is_empty() {
            self.logs.sink(format!("env:{}/{}", svc.id, env.name), Stream::System).line(format!("references that name nothing, left empty: {}", resolved.unresolved.join(", ")));
        }
        let mut vars = resolved.vars;
        if run.kind != RunType::Web {
            vars.retain(|(k, _)| k != "PORT");
        }
        let network = network_name(&svc.project, &env.name);
        self.deps.runtime.ensure_network(&network, &svc.id).await.map_err(|e| Error::Internal(e.to_string()))?;
        // An image service's registry login, for a runtime that pulls at each start; only the
        // registry it's for gets it.
        let registry = match &svc.source {
            Source::Image { .. } => {
                let sid = svc.id.clone();
                self.db(move |s| s.registry_login(&sid))
                    .await?
                    .filter(|(r, _)| crate::model::registry_key(&r.server) == crate::model::image_registry(&image))
                    .map(|(r, password)| crate::runtime::RegistryLogin { server: r.server, username: r.username, password })
            }
            _ => None,
        };
        let spec = InstanceSpec {
            name: String::new(),
            service: svc.id.clone(),
            env: env.name.clone(),
            deployment: dep.id.clone(),
            image,
            command: run.start.clone(),
            vars,
            port: if run.kind == RunType::Web { run.port } else { None },
            cpu: run.cpu,
            memory_mb: run.memory_mb,
            network: Some(network),
            aliases: vec![svc.name.clone(), format!("{}.{}.internal", svc.name, env.name)],
            volume: run.volume.as_ref().map(|v| (volume_name(&svc.id, &env.name), v.mount.clone())),
            // A web service that scales to zero sleeps after `sleep_after`: an instance idle well
            // past that (its node gone) may end itself.
            idle_stop_s: (run.kind == RunType::Web && run.min_instances == 0).then(|| run.sleep_after_s.max(60) + 300),
            registry,
        };
        let spa = dep.plan.as_ref().is_some_and(|p| p["spa"] == true);
        // A static plan whose files were exported answers from them, with no instance.
        let site = match (dep.plan.as_ref().is_some_and(|p| p["start"] == apps_build::STATIC_START), crate::sites::key(&spec.image)) {
            (true, Some(k)) if run.kind == RunType::Web && run.volume.is_none() => {
                let kk = k.clone();
                self.db(move |s| s.site(&kk)).await?.map(|_| k)
            }
            _ => None,
        };
        Ok(Template { deployment: dep.id.clone(), spec, run, spa, site })
    }

    // ── the owner's background work ──

    /// Start the background loops. Each acts only on services this node owns.
    pub fn start(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move { this.ownership().await });
        let this = self.clone();
        tokio::spawn(async move { this.reaper().await });
        let this = self.clone();
        tokio::spawn(async move { this.private_callers().await });
        let this = self.clone();
        tokio::spawn(async move { this.keeper().await });
        let this = self.clone();
        tokio::spawn(async move { this.landings().await });
        let this = self.clone();
        tokio::spawn(async move { this.domains_loop().await });
        let this = self.clone();
        tokio::spawn(async move { this.notifier().await });
        let this = self.clone();
        tokio::spawn(async move { this.spender().await });
        let this = self.clone();
        tokio::spawn(async move { this.drainer().await });
    }

    /// Start and stop a service's work as its lease comes and goes.
    async fn ownership(self: Arc<Self>) {
        let mut rx = self.deps.cluster.subscribe_ownership();
        loop {
            let ev = tokio::select! {
                ev = rx.recv() => ev,
                _ = self.stop.cancelled() => return,
            };
            let Some(ev) = ev else { return };
            match ev {
                roll_cluster::Ownership::Acquired { key, .. } => {
                    if let Some(svc) = key.strip_prefix("apps:service:") {
                        let (this, svc) = (self.clone(), svc.to_string());
                        tokio::spawn(async move { this.resume_service(&svc).await });
                    }
                }
                roll_cluster::Ownership::Lost { key, .. } => {
                    if let Some(svc) = key.strip_prefix("apps:service:") {
                        self.release_service(svc).await;
                    }
                }
            }
        }
    }

    /// A service this node just got: clear what an earlier process left, load its
    /// environments' active deployments, resume its queued deploys.
    pub async fn resume_service(self: &Arc<Self>, service: &str) {
        // Lost again before this ran (a lease that changed hands twice): what runs is the next
        // owner's now, on a runtime the nodes share.
        if !self.owns(service) {
            return;
        }
        // The database's now: what started before is a previous owner's (or nobody's).
        let Ok(took) = self.db(|s| s.now()).await else { return };
        let known: Vec<String> = self.lives().iter().filter(|l| l.service == service).flat_map(|l| l.instances().into_iter().map(|i| i.name.clone())).collect();
        // The live deployments' instances still running (the previous owner died, they didn't)
        // stay, for `EnvLive::adopt` to keep; the rest (jobs, older deployments') go.
        let sid = service.to_string();
        let live_prefixes: Vec<String> = self
            .db(move |s| s.environments(&sid))
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|e| e.active.map(|a| crate::live::instance_prefix(service, &e.name, &a)))
            .collect();
        for name in self.deps.runtime.list(Some(service)).await {
            // A known instance's network sandbox (`<instance>-net`) and waker (`-wake`) are its own.
            let owner = name.strip_suffix("-net").or_else(|| name.strip_suffix("-wake")).unwrap_or(&name).to_string();
            if known.contains(&name) || known.contains(&owner) {
                continue;
            }
            // (Not asked whether it runs: that's the adopt's to find, and a runtime may not know
            // a task another process started until it looks.)
            if live_prefixes.iter().any(|p| owner.strip_prefix(p.as_str()).is_some_and(|n| n.parse::<u64>().is_ok())) {
                continue;
            }
            self.deps.runtime.remove(&name).await;
        }
        // Cron runs and jobs an earlier process ran had their containers removed just above: they
        // failed (and say so), and a run left `running` no longer holds the schedule back.
        let spare: Vec<String> = self.running_jobs.lock().keys().cloned().chain(self.running_runs.lock().keys().cloned()).collect();
        let sid = service.to_string();
        if self.db(move |s| s.fail_orphans(&sid, &spare, took)).await.is_ok_and(|n| n > 0) {
            self.changed(service);
        }
        let sid = service.to_string();
        let Ok(envs) = self.db(move |s| s.environments(&sid)).await else { return };
        for e in envs {
            // A passing failure (the database, a secret's provider) is tried again before what
            // the previous owner ran is given up.
            let mut loaded = Err(Error::Internal(String::new()));
            for attempt in 0..3u64 {
                if attempt > 0 {
                    tokio::time::sleep(Duration::from_millis(500 * attempt)).await;
                }
                loaded = self.load_live(service, &e.name).await;
                if loaded.is_ok() {
                    break;
                }
            }
            if let Err(err) = loaded {
                warn!(service, env = %e.name, error = %err, "apps: loading an environment failed");
                // Nothing will keep what the previous owner ran of it: it goes, as before; unless
                // the service moved on meanwhile (the next owner keeps it).
                if !self.owns(service) {
                    return;
                }
                if let Some(a) = &e.active {
                    let prefix = crate::live::instance_prefix(service, &e.name, a);
                    for name in self.deps.runtime.list(Some(service)).await {
                        let owner = name.strip_suffix("-net").or_else(|| name.strip_suffix("-wake")).unwrap_or(&name);
                        if owner.strip_prefix(prefix.as_str()).is_some_and(|n| n.parse::<u64>().is_ok()) {
                            self.deps.runtime.remove(&name).await;
                        }
                    }
                }
            }
            // A deploy a lost node had started runs again from the start.
            let (sid, en) = (service.to_string(), e.name.clone());
            if let Ok(pending) = self.db(move |s| Ok(s.pending_deployments(&sid)?.into_iter().filter(|d| d.environment == en).collect::<Vec<_>>())).await {
                for d in pending.iter().filter(|d| matches!(d.status, DeployStatus::Building | DeployStatus::Deploying)) {
                    if !self.cancels.lock().contains_key(&d.id) {
                        let id = d.id.clone();
                        let _ = self.db(move |s| s.set_status(&id, &[DeployStatus::Building, DeployStatus::Deploying], DeployStatus::Queued, None)).await;
                    }
                }
                if !pending.is_empty() {
                    self.kick(service, &e.name);
                }
            }
        }
    }

    async fn release_service(self: &Arc<Self>, service: &str) {
        // Another node owns it now: what this one cached may change there, and its shells go.
        {
            let mut cache = self.access.lock();
            self.access_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            cache.retain(|k, _| !k.starts_with(&format!("{service}/")));
        }
        self.close_sessions_of(service);
        // Its jobs and cron runs stop here too (with their variables): the new owner fails what
        // they left running.
        self.cancel_service_jobs(service);
        let runs: Vec<String> = self.running_runs.lock().iter().filter(|(_, s)| *s == service).map(|(id, _)| id.clone()).collect();
        for id in runs {
            if let Some(t) = self.cancels.lock().get(&id) {
                t.cancel();
            }
        }
        // Deploys in flight stop here; the new owner resumes them from the store.
        let sid = service.to_string();
        if let Ok(pending) = self.db(move |s| s.pending_deployments(&sid)).await {
            for d in pending {
                if let Some(t) = self.cancels.lock().get(&d.id) {
                    t.cancel();
                }
            }
        }
        // On a runtime the nodes share, what runs stays for the next owner to keep (stopping it
        // here could stop what it just kept: a handoff's release can come after its take);
        // otherwise it stops, as nobody else can reach it.
        let lives: Vec<_> = self.lives().into_iter().filter(|l| l.service == service).collect();
        let shared = self.deps.runtime.shares_instances();
        for l in lives {
            self.lives.lock().remove(&crate::env_key(&l.service, &l.env));
            if shared {
                l.let_go().await;
            } else {
                l.shutdown().await;
            }
        }
    }

    /// Delete a stored upload, then forget it. A failed delete leaves the row for the sweep.
    pub(crate) async fn drop_upload(self: &Arc<Self>, deployment: &str, key: &str) {
        if self.delete_upload(key).await {
            let id = deployment.to_string();
            let _ = self.db(move |s| s.clear_upload(&id)).await;
        }
    }

    /// Delete an upload's bytes: whether they're gone.
    async fn delete_upload(&self, key: &str) -> bool {
        match &self.deps.objects {
            Some(o) => o.delete(key).await.is_ok(),
            None => match tokio::fs::remove_file(self.cfg.home.join(key)).await {
                Ok(()) => true,
                Err(e) => e.kind() == std::io::ErrorKind::NotFound,
            },
        }
    }

    /// Uploads of owned services that no build will read: superseded or canceled before they
    /// built, or left by a process that stopped mid-build.
    async fn sweep_uploads(self: &Arc<Self>) {
        let Ok(spent) = self.db(|s| s.spent_uploads(100)).await else { return };
        for (_, id, key) in spent.into_iter().filter(|(service, _, _)| self.owns(service)) {
            self.drop_upload(&id, &key).await;
        }
    }

    /// `run.rollback_on_crash`: the live deployment, crash-looping within ten minutes of going
    /// live and not itself a rollback, is replaced by the one before it.
    async fn rollback_on_crash(self: &Arc<Self>, l: &crate::live::EnvLive) {
        let Some(tpl) = l.template().filter(|t| t.run.rollback_on_crash) else { return };
        let id = tpl.deployment.clone();
        let Ok(d) = self.db(move |s| s.deployment(&id)).await else { return };
        let Some(live_at) = d.finished_at.filter(|t| code_sql::now_ms() - t <= 10 * 60_000) else { return };
        // Its own crashes since it went live, not another deploy's failed checks or older ones.
        if d.rollback_of.is_some() || l.crashes_of(&d.id, live_at) < 3 {
            return;
        }
        // A deploy on its way (a fix, say) decides instead.
        let (sid, en) = (l.service.clone(), l.env.clone());
        if self.db(move |s| s.pending_deployments(&sid)).await.map_or(true, |p| p.iter().any(|x| x.environment == en)) {
            return;
        }
        l.log.stream(Stream::System).line(format!("{} is crash-looping within ten minutes of going live: rolling back to the deployment before it", d.id));
        let (this, sid, e) = (self.clone(), l.service.clone(), l.env.clone());
        tokio::spawn(async move {
            // The image only: today's variables stay (a credential rotated since isn't undone).
            if let Err(err) = this.rollback_previous(&sid, &e, Some("crash loop".into()), true).await {
                this.logs.sink(format!("env:{sid}/{e}"), Stream::System).line(format!("couldn't roll back: {err}"));
            }
        });
    }

    /// An environment that went into a crash loop gets one event for it (until it leaves it).
    async fn note_crash_loop(self: &Arc<Self>, l: &crate::live::EnvLive) {
        let key = format!("{}/{}", l.service, l.env);
        match l.crash_loop() {
            Some(c) => {
                if self.looping.lock().insert(key) {
                    let (sid, e, dep) = (l.service.clone(), l.env.clone(), l.template().map(|t| t.deployment.clone()));
                    let data = serde_json::json!({ "crashes": c.crashes, "last_error": c.last_error });
                    let _ = self.db(move |s| s.add_event("crash_loop", &sid, &e, dep.as_deref(), &data)).await;
                    self.rollback_on_crash(l).await;
                }
            }
            None => {
                self.looping.lock().remove(&key);
            }
        }
    }

    /// Metric alerts: the last five finished minutes against the deployment's `run.alerts`. An
    /// alert is an event once, when it starts; it can start again once it has cleared.
    pub async fn judge_alerts(self: &Arc<Self>, l: &crate::live::EnvLive) {
        let Some(tpl) = l.template() else { return };
        let (a, limit_mb) = (tpl.run.alerts.clone(), tpl.run.memory_mb);
        let now = code_sql::now_ms() / 60_000;
        let (sid, e) = (l.service.clone(), l.env.clone());
        let Ok(rows) = self.db(move |s| s.minutes(&sid, &e, now - 5)).await else { return };
        let rows: Vec<_> = rows.into_iter().filter(|r| r.minute < now).collect();
        let mut firing: Vec<(&'static str, serde_json::Value)> = Vec::new();
        if a.memory_pct > 0 && rows.len() >= 5 {
            let share = |r: &crate::store::MinuteRow| r.memory_bytes as f64 / r.instances.max(1) as f64 / (limit_mb as f64 * 1048576.0) * 100.0;
            if rows.iter().all(|r| r.instances > 0 && share(r) >= a.memory_pct as f64) {
                let value = rows.iter().map(share).fold(0.0, f64::max).round();
                firing.push(("alert.memory", serde_json::json!({ "value": value, "threshold": a.memory_pct, "limit_mb": limit_mb })));
            }
        }
        let (requests, errors): (i64, i64) = rows.iter().fold((0, 0), |(q, e), r| (q + r.requests, e + r.errors));
        if a.error_pct > 0 && requests >= 20 && errors * 100 >= a.error_pct as i64 * requests {
            firing.push(("alert.errors", serde_json::json!({ "value": (errors * 100 / requests), "threshold": a.error_pct, "requests": requests })));
        }
        if a.p95_ms > 0 {
            let slow = rows.iter().filter(|r| r.requests > 0 && r.p95_ms >= a.p95_ms as f64).count();
            if slow >= 3 {
                let value = rows.iter().map(|r| r.p95_ms).fold(0.0, f64::max).round();
                firing.push(("alert.latency", serde_json::json!({ "value": value, "threshold": a.p95_ms })));
            }
        }
        // Its output saying something often enough (system lines are roll's, and the alert's
        // own line names the pattern: only stdout and stderr count).
        if let Some(pat) = a.log_match.as_deref() {
            let key = format!("env:{}/{}", l.service, l.env);
            let since = code_sql::now_ms() - 5 * 60_000;
            let count = self.logs.count_recent(&key, pat, since) as u32;
            if count >= a.log_count {
                firing.push(("alert.log", serde_json::json!({ "pattern": pat, "count": count, "threshold": a.log_count })));
            }
        }
        for kind in ["alert.memory", "alert.errors", "alert.latency", "alert.log"] {
            let key = format!("{}/{}/{kind}", l.service, l.env);
            match firing.iter().find(|(k, _)| *k == kind) {
                Some((_, data)) => {
                    if self.looping.lock().insert(key) {
                        let (sid, e, dep, d) = (l.service.clone(), l.env.clone(), tpl.deployment.clone(), data.clone());
                        let _ = self.db(move |s| s.add_event(kind, &sid, &e, Some(&dep), &d)).await;
                        l.log.stream(Stream::System).line(format!("alert: {kind} {data}"));
                    }
                }
                None => {
                    self.looping.lock().remove(&key);
                }
            }
        }
    }

    /// Private callers at owned web services, five times a second for sleeping instances (a
    /// frozen one's wake is milliseconds; one cheap read of `/proc/<pid>/net/snmp` each), and
    /// once a second for running ones near their sleep (a dear read, at most every 5 s each).
    async fn private_callers(self: Arc<Self>) {
        let mut every = tokio::time::interval(Duration::from_millis(200));
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut n: u64 = 0;
        loop {
            tokio::select! {
                _ = every.tick() => {}
                _ = self.stop.cancelled() => return,
            }
            n += 1;
            for l in self.lives().into_iter().filter(|l| self.owns(&l.service)) {
                l.answer_private(n.is_multiple_of(5)).await;
            }
        }
    }

    /// The reaper: every second, each owned environment's tick; every minute, the meters.
    async fn reaper(self: Arc<Self>) {
        let mut every = tokio::time::interval(Duration::from_secs(1));
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut n: u64 = 0;
        loop {
            tokio::select! {
                _ = every.tick() => {}
                _ = self.stop.cancelled() => {
                    self.flush_meters(true).await;
                    return;
                }
            }
            n += 1;
            // Each environment's tick on its own task: one waiting for a cold start to finish
            // doesn't hold up the others.
            for l in self.lives().into_iter().filter(|l| self.owns(&l.service)) {
                self.note_crash_loop(&l).await;
                if l.ticking.swap(true, Ordering::SeqCst) {
                    continue;
                }
                tokio::spawn(async move {
                    l.tick().await;
                    l.ticking.store(false, Ordering::SeqCst);
                });
            }
            if n.is_multiple_of(10) {
                self.flush_meters(false).await;
                self.cron_pass(code_sql::now_ms()).await;
            }
            if n.is_multiple_of(600) {
                let day_ago = code_sql::now_ms() - 86_400_000;
                // Only with the store's answer: none would read as "nothing in use".
                if let Ok(in_use) = self.db(move |s| s.deployments_in_use(day_ago)).await {
                    let runtime = self.deps.runtime.clone();
                    tokio::spawn(async move { runtime.sweep(&in_use.into_iter().collect()).await });
                }
                let this = self.clone();
                tokio::spawn(async move { this.prune().await });
            }
            if n.is_multiple_of(60) {
                self.sweep_uploads().await;
                for l in self.lives().into_iter().filter(|l| self.owns(&l.service)) {
                    self.judge_alerts(&l).await;
                }
            }
            if n.is_multiple_of(10) && !self.scaling.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let this = self.clone();
                tokio::spawn(async move {
                    this.scale_workers().await;
                    this.scaling.store(false, std::sync::atomic::Ordering::SeqCst);
                });
            }
            if n.is_multiple_of(60) && !self.watching_secrets.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let this = self.clone();
                tokio::spawn(async move {
                    this.watch_secrets().await;
                    this.watching_secrets.store(false, std::sync::atomic::Ordering::SeqCst);
                });
            }
            if n.is_multiple_of(300) && !self.watching.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let this = self.clone();
                tokio::spawn(async move {
                    this.watch_tags().await;
                    this.watching.store(false, std::sync::atomic::Ordering::SeqCst);
                });
            }
            if n.is_multiple_of(3600) && !self.backing_up.swap(true, std::sync::atomic::Ordering::SeqCst) {
                // A volume's tar can take minutes: never on the reaper's own loop.
                let this = self.clone();
                tokio::spawn(async move {
                    this.scheduled_backups().await;
                    this.backing_up.store(false, std::sync::atomic::Ordering::SeqCst);
                });
            }
            if n.is_multiple_of(3600) {
                let before = code_sql::now_ms() / 60_000 - 7 * 24 * 60;
                let _ = self.db(move |s| s.prune_metrics(before)).await;
                let day_ago = code_sql::now_ms() - 24 * 3_600_000;
                let _ = self.db(move |s| s.prune_deploy_keys(day_ago)).await;
                let week_ago = code_sql::now_ms() - 7 * 24 * 3_600_000;
                let _ = self.db(move |s| s.prune_events(week_ago)).await;
            }
        }
    }

    /// Services that must run without a request (workers, cron, minimum instances, queued
    /// deploys) get an owner: every node takes the leases of those whose preferred node it is.
    async fn keeper(self: Arc<Self>) {
        loop {
            if let Ok(rows) = self.db(|s| s.services(None)).await {
                for svc in rows {
                    let sid = svc.id.clone();
                    // An environment's own floor counts too (production with one warm instance).
                    let floor = svc.run.min_instances > 0
                        || self.db(move |s| s.environments(&sid)).await.is_ok_and(|envs| envs.iter().any(|e| e.run.get("min_instances").and_then(Value::as_u64).is_some_and(|n| n > 0)));
                    let always = svc.run.kind != RunType::Web || floor;
                    let sid = svc.id.clone();
                    let pending = self.db(move |s| s.pending_deployments(&sid)).await.map(|p| !p.is_empty()).unwrap_or(false);
                    if !(always || pending) {
                        continue;
                    }
                    let key = lease_key(&svc.id);
                    let mine = self.deps.cluster.preferred(&key).is_none_or(|n| n.id == self.deps.cluster.node_id());
                    if mine && !self.deps.cluster.owns(&key) {
                        let _ = self.deps.cluster.lease(&key).await;
                    }
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(15)) => {}
                _ = self.stop.cancelled() => return,
            }
        }
    }

    /// Deploy on push: follow roll code's land log (durably, after a stored cursor) and queue a
    /// deploy for every environment whose branch got a landing. One node does it (singleton).
    async fn landings(self: Arc<Self>) {
        let Some(sources) = self.deps.sources.clone() else { return };
        // A landing whose deploys failed to queue is tried again (its deploys are keyed by the
        // landing, so the ones that did queue aren't queued twice), and passed over only after
        // a few tries: one broken landing never holds up every later push.
        let mut failures: HashMap<i64, u32> = HashMap::new();
        loop {
            if self.deps.cluster.singleton("apps:landings") {
                let cursor = match self.db(|s| s.get_state("landings")).await {
                    Ok(Some(c)) => c.parse().unwrap_or(0),
                    // First start: from now on, not the whole history.
                    Ok(None) => {
                        let head = sources.head().await.unwrap_or(0);
                        let _ = self.db(move |s| s.set_state("landings", &head.to_string())).await;
                        head
                    }
                    Err(_) => -1,
                };
                if cursor >= 0
                    && let Ok(page) = sources.landings(cursor, 200).await
                {
                    let mut last = cursor;
                    let mut stuck = false;
                    for l in &page {
                        if let Err(e) = self.on_landing(l).await {
                            let tries = failures.entry(l.seq).or_default();
                            *tries += 1;
                            if *tries < 5 {
                                warn!(repo = %l.repo, branch = %l.branch, error = %e, tries = *tries, "apps: queueing a deploy for a landing failed; tried again");
                                stuck = true;
                                break;
                            }
                            warn!(repo = %l.repo, branch = %l.branch, error = %e, "apps: queueing a deploy for a landing failed five times; passed over");
                        }
                        failures.remove(&l.seq);
                        last = l.seq;
                    }
                    if last != cursor {
                        let _ = self.db(move |s| s.set_state("landings", &last.to_string())).await;
                    }
                    if page.len() == 200 && !stuck {
                        continue;
                    }
                }
                self.settle_ci_waits().await;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                _ = self.stop.cancelled() => return,
            }
        }
    }

    /// The run settings a deployment of `old`'s image gets in `env` now: the service's, with
    /// roll.toml's at `old`'s revision on top (its `[app.run]` and `env`'s block), as a build
    /// would set them. A file that doesn't read leaves the service's.
    async fn run_for(self: &Arc<Self>, svc: &ServiceRow, old: &Deployment, env: &str) -> Run {
        let file = match &old.revision {
            Some(rev) => self.app_config_at(svc, rev).await,
            None => None,
        };
        let (sid, e) = (svc.id.clone(), env.to_string());
        let row = self.db(move |s| s.environment(&sid, &e)).await.ok();
        let ephemeral = row.as_ref().is_some_and(|r| r.ephemeral);
        let svc = row.as_ref().and_then(|r| svc.for_env(r).ok()).unwrap_or_else(|| svc.clone());
        let scope = crate::config::EnvScope { name: env, ephemeral };
        let run = file.as_ref().and_then(|f| f.apply(&svc.build, &svc.run, Some(scope)).ok()).map_or_else(|| svc.run.clone(), |(_, r, _)| r);
        if ephemeral { crate::config::preview_defaults(run, file.as_ref()).0 } else { run }
    }

    /// Pushes waiting for their checks: deployed once they pass (or there are none), dropped when
    /// one fails or after an hour, each said in its environment's log.
    pub async fn settle_ci_waits(self: &Arc<Self>) {
        let Some(sources) = self.deps.sources.clone() else { return };
        let Ok(waits) = self.db(|s| s.ci_waits()).await else { return };
        let now = code_sql::now_ms();
        for (w, since) in waits {
            let Ok(svc) = self.db({
                let sid = w.service.clone();
                move |s| s.service(&sid)
            })
            .await
            else {
                let (sid, e, l) = (w.service.clone(), w.env.clone(), w.landing);
                let _ = self.db(move |s| s.end_ci_wait(&sid, &e, l)).await;
                continue;
            };
            let Source::Repo { repo, .. } = &svc.source else { continue };
            let log = self.logs.sink(format!("env:{}/{}", w.service, w.env), Stream::System);
            let sha = &w.sha[..w.sha.len().min(12)];
            let done = match sources.ci(repo, &w.sha).await {
                Ok(crate::integrations::Ci::Pending) if now - since > 3_600_000 => {
                    log.line(format!("push {sha} not deployed: its checks didn't finish within an hour"));
                    true
                }
                Ok(crate::integrations::Ci::Pending) | Err(_) => false,
                Ok(crate::integrations::Ci::Failed(which)) => {
                    log.line(format!("push {sha} not deployed: its check {which} failed"));
                    true
                }
                Ok(_) => {
                    log.line(format!("push {sha}'s checks passed: deploying"));
                    let body = serde_json::json!({ "revision": w.sha, "message": w.message });
                    let by = By { trigger: Trigger::Push, actor: w.author.as_ref().map(|a| format!("git:{a}")) };
                    self.deploy(&w.service, &w.env, body, by, None, Some(&format!("landing:{}", w.landing))).await.is_ok()
                }
            };
            if done {
                let (sid, e, l) = (w.service.clone(), w.env.clone(), w.landing);
                let _ = self.db(move |s| s.end_ci_wait(&sid, &e, l)).await;
            }
        }
    }

    /// `roll.toml` `[app]` at a pushed revision (in the service's root), if it reads.
    async fn app_config_at(self: &Arc<Self>, svc: &ServiceRow, revision: &str) -> Option<crate::config::AppConfig> {
        let (Source::Repo { repo, root }, Some(sources)) = (&svc.source, &self.deps.sources) else { return None };
        let path = match root {
            Some(r) => format!("{r}/roll.toml"),
            None => "roll.toml".into(),
        };
        match sources.read_file(repo, revision, &path).await {
            Ok(Some(bytes)) => crate::config::parse(&String::from_utf8_lossy(&bytes)).ok().flatten(),
            _ => None,
        }
    }

    /// Whether `revision` changes a watched path since what `env` runs (no active deployment, or
    /// a source that can't tell: yes).
    async fn push_touches(self: &Arc<Self>, svc: &ServiceRow, env: &EnvRow, revision: &str, patterns: &[String]) -> bool {
        let (Source::Repo { repo, .. }, Some(sources)) = (&svc.source, &self.deps.sources) else { return true };
        let Some(active) = env.active.clone() else { return true };
        let Ok(Some(from)) = self.db(move |s| s.deployment(&active).map(|d| d.revision)).await else { return true };
        sources.touches(repo, &from, revision, patterns).await.unwrap_or(true)
    }

    /// Queue the deploys a landing makes. One service failing (or capped) doesn't hold up the
    /// others: they're all tried, and the first error comes back for the landing to be retried
    /// (its deploys are keyed by the landing, so those that queued aren't queued twice).
    async fn on_landing(self: &Arc<Self>, l: &crate::integrations::Landing) -> Result<()> {
        let all = self.db(|s| Ok((s.services(None)?, s.all_environments()?))).await?;
        let (services, envs) = all;
        let mut failed = None;
        for svc in services.iter().filter(|s| matches!(&s.source, Source::Repo { repo, .. } if *repo == l.repo)) {
            if self.capped(&svc.id) {
                // Paused by its project's spend limit: not a failure to retry.
                self.logs.sink(format!("env:{}/production", svc.id), Stream::System).line(format!("push {} not deployed: the project reached its spending limit", &l.revision.sha[..l.revision.sha.len().min(12)]));
                continue;
            }
            // The paths a push must touch: roll.toml's at the pushed revision (the environment's
            // block first), else the service's setting; none: every push counts.
            let file = self.app_config_at(svc, &l.revision.sha).await;
            for e in envs.iter().filter(|e| e.service == svc.id && !e.ephemeral && e.branch.as_deref() == Some(l.branch.as_str())) {
                let watch = file.as_ref().and_then(|f| f.watch_for(&e.name)).or_else(|| svc.build.watch.clone()).filter(|w| !w.is_empty());
                if let Some(patterns) = &watch
                    && !self.push_touches(svc, e, &l.revision.sha, patterns).await
                {
                    let sha = &l.revision.sha[..l.revision.sha.len().min(12)];
                    self.logs.sink(format!("env:{}/{}", svc.id, e.name), Stream::System).line(format!("push {sha} changes nothing this service watches ({}): not deployed", patterns.join(", ")));
                    continue;
                }
                // An environment that waits for CI deploys a push once its checks pass.
                if e.wait_for_ci {
                    let Source::Repo { repo, .. } = &svc.source else { continue };
                    let sources = self.deps.sources.clone().ok_or_else(|| Error::Internal("roll code isn't on this node".into()))?;
                    match sources.ci(repo, &l.revision.sha).await {
                        Ok(crate::integrations::Ci::Pending) => {
                            let w = crate::store::CiWait { service: svc.id.clone(), env: e.name.clone(), landing: l.seq, sha: l.revision.sha.clone(), message: l.revision.message.lines().next().unwrap_or("").to_string(), author: l.author.clone() };
                            self.db(move |s| s.wait_for_ci(&w)).await?;
                            self.logs.sink(format!("env:{}/{}", svc.id, e.name), Stream::System).line(format!("push {} waits for its checks", &l.revision.sha[..l.revision.sha.len().min(12)]));
                            continue;
                        }
                        Ok(crate::integrations::Ci::Failed(which)) => {
                            self.logs.sink(format!("env:{}/{}", svc.id, e.name), Stream::System).line(format!("push {} not deployed: its check {which} failed", &l.revision.sha[..l.revision.sha.len().min(12)]));
                            continue;
                        }
                        Ok(_) => {}
                        Err(err) => {
                            failed.get_or_insert(Error::Internal(format!("reading {}'s checks: {err}", l.revision.sha)));
                            continue;
                        }
                    }
                }
                let body = serde_json::json!({ "revision": l.revision.sha, "message": l.revision.message.lines().next().unwrap_or("") });
                // The owner queues it (it may be another node: the queue is in the store).
                let key = format!("landing:{}", l.seq);
                match self.deploy(&svc.id, &e.name, body, By { trigger: Trigger::Push, actor: l.author.as_ref().map(|a| format!("git:{a}")) }, None, Some(&key)).await {
                    Ok(_) => info!(service = %svc.id, env = %e.name, revision = %l.revision.sha, "apps: deploying a push"),
                    Err(err) => {
                        warn!(service = %svc.id, env = %e.name, error = %err, "apps: a push's deploy didn't queue");
                        failed.get_or_insert(err);
                    }
                }
            }
        }
        failed.map_or(Ok(()), Err)
    }

    /// Cron: start the runs that are due at `now` in owned cron services (a run still going
    /// when the next is due is skipped). The reaper's pass, every few seconds.
    pub async fn cron_pass(self: &Arc<Self>, now: i64) {
        let Ok(services) = self.db(|s| s.services(None)).await else { return };
        for svc in services.into_iter().filter(|s| s.run.kind == RunType::Cron && self.owns(&s.id) && !self.capped(&s.id)) {
            let Some(schedule) = svc.run.schedule.as_deref().and_then(|s| crate::cron::Schedule::parse(s, svc.run.timezone.as_deref()).ok()) else { continue };
            let sid = svc.id.clone();
            let Ok(envs) = self.db(move |s| s.environments(&sid)).await else { continue };
            for e in envs.into_iter().filter(|e| e.active.is_some()) {
                let key = format!("cron:{}/{}", svc.id, e.name);
                let k = key.clone();
                let last: i64 = self.db(move |s| s.get_state(&k)).await.ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(now);
                if last == now {
                    let (k, v) = (key.clone(), now.to_string());
                    let _ = self.db(move |s| s.set_state(&k, &v)).await;
                    continue;
                }
                let Some(due) = schedule.next_after(last) else { continue };
                if due > now {
                    continue;
                }
                let (k, v) = (key.clone(), due.to_string());
                let _ = self.db(move |s| s.set_state(&k, &v)).await;
                let (this, sid, env) = (self.clone(), svc.id.clone(), e.name.clone());
                tokio::spawn(async move {
                    let _ = this.run_cron(&sid, &env).await;
                });
            }
        }
    }

    /// One run of a cron service now (`POST …/runs`, or the schedule).
    pub async fn run_cron(self: &Arc<Self>, service: &str, env: &str) -> Result<CronRun> {
        let (sid, e) = (service.to_string(), env.to_string());
        let (svc, row) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await?;
        if svc.run.kind != RunType::Cron {
            return Err(Error::Conflict("only a cron service has runs".into()));
        }
        let Some(active) = row.active.clone() else { return Err(Error::Conflict("this environment has no deployment yet".into())) };
        let timeout = Duration::from_secs(svc.run.timeout_s.unwrap_or(3600) as u64);
        let (sid, e, a, t) = (service.to_string(), env.to_string(), active.clone(), timeout.as_millis() as i64 + 60_000);
        // Known here as running before its row says so: a takeover's cleanup never fails it.
        let rid = crate::model::new_id("run");
        self.running_runs.lock().insert(rid.clone(), service.to_string());
        let r2 = rid.clone();
        let run = match self.db(move |s| s.start_run(&r2, &sid, &e, &a, t)).await {
            Ok(r) if r.status != RunStatus::Skipped => r,
            other => {
                self.running_runs.lock().remove(&rid);
                return other;
            }
        };
        let live = match self.load_live(service, env).await {
            Ok(l) => l,
            Err(err) => {
                let r2 = rid.clone();
                let _ = self.db(move |s| s.finish_run(&r2, RunStatus::Failed, None)).await;
                self.running_runs.lock().remove(&rid);
                return Err(err);
            }
        };
        let (this, id, sid, run_dep) = (self.clone(), run.id.clone(), service.to_string(), run.deployment.clone());
        let cancel = CancellationToken::new();
        self.cancels.lock().insert(id.clone(), cancel.clone());
        tokio::spawn(async move {
            // Its output stays in the environment's log (drains read it there); its container's
            // name, its own id in it, is what picks its own lines out.
            let name = crate::live::run_name(&sid, &live.env, &run_dep, &id);
            let (status, code) = match live.run_to_exit(None, &[], timeout, None, cancel, Some(name)).await {
                Ok(x) if x.canceled => (RunStatus::Canceled, None),
                Ok(x) if x.timed_out => (RunStatus::TimedOut, None),
                Ok(x) if x.code == Some(0) => (RunStatus::Succeeded, Some(0)),
                Ok(x) => (RunStatus::Failed, x.code),
                Err(e) => {
                    live.log.stream(Stream::System).line(format!("the run didn't start: {e}"));
                    (RunStatus::Failed, None)
                }
            };
            this.cancels.lock().remove(&id);
            // A failed or timed-out run is an incident's start: its event (notified by default)
            // in the same transaction, and only if it was still running (a takeover may have
            // failed it already).
            let id2 = id.clone();
            let _ = this.db(move |s| s.finish_run(&id2, status, code)).await;
            this.running_runs.lock().remove(&id);
            this.changed(&sid);
        });
        Ok(run)
    }

    pub async fn cron_run(self: &Arc<Self>, service: &str, env: &str, id: &str) -> Result<CronRun> {
        let (sid, e, i) = (service.to_string(), env.to_string(), id.to_string());
        self.db(move |s| s.run(&sid, &e, &i)).await
    }

    /// `POST …/runs/{run}/cancel`: a hung run stops (and the schedule's next one isn't held back).
    pub async fn cancel_run(self: &Arc<Self>, service: &str, env: &str, id: &str) -> Result<CronRun> {
        let run = self.cron_run(service, env, id).await?;
        if run.status != RunStatus::Running {
            return Err(Error::Conflict(format!("{id} is {}: only a running run can be canceled", run.status.as_str())));
        }
        let token = self.cancels.lock().get(id).cloned();
        match token {
            Some(t) => t.cancel(),
            // Running in the store, not here (its node stopped, or it never started): it ends now
            // instead of holding the schedule back until its timeout.
            None => {
                let i = id.to_string();
                self.db(move |s| s.finish_run(&i, RunStatus::Canceled, None)).await?;
            }
        }
        Ok(run)
    }

    /// Every minute (one node): expire unverified claims older than [`PENDING_CLAIM_TTL`], check
    /// pending custom domains' DNS, and ask where verified ones' certificates are.
    async fn domains_loop(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                _ = self.stop.cancelled() => return,
            }
            if !self.deps.cluster.singleton("apps:domains") {
                continue;
            }
            let _ = self.expire_domain_claims(PENDING_CLAIM_TTL).await;
            // A day before: the owner hears it's going (and what DNS says), not only after.
            let cutoff = code_sql::now_ms() - (PENDING_CLAIM_TTL.as_millis() as i64 - 86_400_000);
            let _ = self.db(move |s| s.warn_domain_claims(cutoff)).await;
            let Ok(due) = self.db(|s| s.domains_to_check()).await else { continue };
            for (svc, env, d) in due {
                if d.status == DomainStatus::Active {
                    self.refresh_tls(&d.hostname).await;
                } else {
                    let _ = self.verify_domain(&svc, &env, &d.hostname).await;
                }
            }
        }
    }

    /// Stop the background work and write what's metered (SIGTERM).
    pub async fn shutdown(&self) {
        self.stop.cancel();
    }
}

/// Whether a canary step fails: the new deployment answered `error_pct` or more of at least
/// [`Rollout::MIN_REQUESTS`] requests with a 5xx, and more often than the old one (an outage
/// both share, a database down, isn't the new code's fault).
fn judge(new: crate::live::Served, old: crate::live::Served, error_pct: u8) -> Option<String> {
    // Crashes no request saw (a failed health check, a process that exits).
    if new.crashes >= 2 && new.crashes > old.crashes {
        return Some(format!("its instances crashed {} times (the old deployment's {})", new.crashes, old.crashes));
    }
    if new.requests < crate::model::Rollout::MIN_REQUESTS || new.error_pct() < error_pct as f64 || new.error_pct() <= old.error_pct() {
        return None;
    }
    Some(format!(
        "{} of its {} requests were 5xx ({:.1}%; the old deployment {:.1}%)",
        new.errors,
        new.requests,
        new.error_pct(),
        old.error_pct()
    ))
}

/// A deploy that failed with what the app or the build printed (`detail`): only roll's own
/// words (`what`) leave roll, in commit statuses and notifications; the detail is the owner's
/// (`Deployment.error_detail`, the deploy log).
#[derive(Debug)]
pub(crate) struct DeployFailure {
    what: String,
    detail: String,
}

impl std::fmt::Display for DeployFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.what)
    }
}

impl std::error::Error for DeployFailure {}

/// Why a build failed, from BuildKit's plain output (Docker's and CodeBuild's alike): its last
/// `ERROR:` (the step and how it ended), and that step's last line of output before it.
fn build_failure(lines: &[String]) -> Option<String> {
    let err = lines.iter().rev().find(|l| l.contains("ERROR:"))?;
    let what = err.split_once("ERROR:").map_or(err.as_str(), |(_, w)| w).trim();
    let what = what.strip_prefix("failed to solve:").unwrap_or(what).trim();
    // The failing step (the last `#12 ERROR: …`; BuildKit's summary has no number), and its own
    // output before it: `#12 0.531 npm ERR! …` (its number, then seconds).
    let step = lines.iter().enumerate().rev().find_map(|(i, l)| {
        let (n, rest) = l.strip_prefix('#')?.split_once(' ')?;
        (n.bytes().all(|b| b.is_ascii_digit()) && rest.starts_with("ERROR:")).then(|| (i, format!("#{n} ")))
    });
    let said = step.and_then(|(at, p)| {
        lines[..at].iter().rev().filter_map(|l| l.strip_prefix(p.as_str())).find_map(|r| {
            let (secs, text) = r.split_once(' ')?;
            (secs.parse::<f64>().is_ok() && !text.trim().is_empty()).then(|| text.trim().to_string())
        })
    });
    let what: String = what.chars().take(300).collect();
    Some(match said {
        Some(s) => format!("{what} (its last output: {})", s.chars().take(200).collect::<String>()),
        None => what,
    })
}

#[cfg(test)]
mod build_failure_tests {
    use super::build_failure;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(str::to_string).collect()
    }

    #[test]
    fn names_the_failing_step_and_what_it_said() {
        let log = lines(
            "#11 [build 4/6] RUN npm ci
#11 0.412 npm ERR! code ENOTFOUND
#11 0.413 npm ERR! network request to https://registry.npmjs.org/left-pad failed
#11 ERROR: process \"/bin/sh -c npm ci\" did not complete successfully: exit code: 1
------
 > [build 4/6] RUN npm ci:
------
ERROR: failed to solve: process \"/bin/sh -c npm ci\" did not complete successfully: exit code: 1",
        );
        // The last ERROR is the summary (no step number): the step's own output is still found.
        let why = build_failure(&log).unwrap();
        assert!(why.starts_with("process \"/bin/sh -c npm ci\" did not complete successfully: exit code: 1") && why.ends_with("left-pad failed)"), "{why}");
        let log2 = lines(
            "#11 [build 4/6] RUN npm ci
#11 0.413 npm ERR! network request to https://registry.npmjs.org/left-pad failed
#11 ERROR: process \"/bin/sh -c npm ci\" did not complete successfully: exit code: 1",
        );
        assert_eq!(
            build_failure(&log2).unwrap(),
            "process \"/bin/sh -c npm ci\" did not complete successfully: exit code: 1 (its last output: npm ERR! network request to https://registry.npmjs.org/left-pad failed)"
        );
        assert_eq!(build_failure(&lines("#2 DONE")), None);
        assert_eq!(build_failure(&lines("ERROR: failed to read dockerfile: open Dockerfile: no such file or directory")).unwrap(), "failed to read dockerfile: open Dockerfile: no such file or directory");
    }
}

