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
use serde_json::Value;
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
            if !["revision", "image", "message"].contains(&k.as_str()) {
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
        let d = NewDeployment { trigger, revision, message, image, plan: None, rollback_of: None, run: svc.for_env(&row)?.run, upload: upload_key, status: DeployStatus::Queued, actor: by.actor };
        let (sid, e, k) = (service.to_string(), env.to_string(), key.map(str::to_string));
        let ours = d.upload.clone().filter(|_| key.is_some());
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
    pub async fn redeploy(self: &Arc<Self>, service: &str, env: &str, body: Value, actor: Option<String>) -> Result<Deployment> {
        self.refuse_capped(service)?;
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
        };
        let (sid, e) = (service.to_string(), env.to_string());
        let dep = self.db(move |s| s.create_deployment(&sid, &e, d, "", None)).await?;
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
        };
        let (sid, e) = (service.to_string(), env.to_string());
        let dep = self.db(move |s| s.create_deployment(&sid, &e, d, "", None)).await?;
        self.deploy_log(&dep.id).line(format!("queued (promoting {} from {from})", old.id));
        self.kick(service, env);
        Ok(dep)
    }

    /// `POST …/deploys/{dep}/rollback`: deploy an earlier deployment's image and settings
    /// again, without a build.
    pub async fn rollback(self: &Arc<Self>, service: &str, env: &str, dep: &str, actor: Option<String>) -> Result<Deployment> {
        self.refuse_capped(service)?;
        let old = self.deployment(service, env, dep).await?;
        let Some(image) = old.image.clone() else { return Err(Error::Conflict(format!("{dep} never built an image, so there's nothing to roll back to"))) };
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
        };
        let (sid, e) = (service.to_string(), env.to_string());
        let dep = self.db(move |s| s.create_deployment(&sid, &e, d, "", None)).await?;
        self.kick(service, env);
        Ok(dep)
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

    pub async fn deployments(self: &Arc<Self>, service: &str, env: &str) -> Result<Vec<Deployment>> {
        let (sid, e) = (service.to_string(), env.to_string());
        self.db(move |s| {
            s.environment(&sid, &e)?;
            s.deployments(&sid, &e, 100)
        })
        .await
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
                self.report(&d, "success", "live").await;
            }
            Err(e) if cancel.is_cancelled() => {
                let _ = e;
                self.report(&d, "failure", "canceled").await;
            }
            Err(e) => {
                self.report(&d, "failure", &e.to_string()).await;
                self.step(&d.service, &d.id, "failed").await;
                let msg = e.to_string();
                log.stream(Stream::System).line(format!("failed: {msg}"));
                let _ = self.db(move |s| s.fail_deployment(&id, &msg)).await;
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
                self.build(&svc, d, log, cancel.clone()).await?
            }
        };
        if cancel.is_cancelled() {
            anyhow::bail!("canceled");
        }
        let id = d.id.clone();
        self.db(move |s| s.set_status(&id, &[DeployStatus::Building, DeployStatus::Deploying], DeployStatus::Deploying, None)).await?;
        let mut dep = d.clone();
        dep.image = Some(image);
        dep.run = run;
        // The plan the build just stored (a single-page app, say).
        let did = d.id.clone();
        if let Ok(stored) = self.db(move |s| s.deployment(&did)).await {
            dep.plan = stored.plan;
        }
        let tpl = Arc::new(self.template(&svc, &env, &dep).await?);
        if let Some(cmd) = tpl.run.pre_deploy.clone() {
            self.step(&d.service, &d.id, "pre_deploy").await;
            self.pre_deploy(&tpl, &cmd, log, &cancel).await?;
        }
        self.step(&d.service, &d.id, "rollout").await;
        let vars_hash = {
            let r = self.resolve_vars(&svc, &env, &dep.id).await?;
            fingerprint(&r.vars_for_hash(), &svc)
        };
        let live = self.live(&svc.id, &env.name);
        // Instances of the old deployment are known here (after a restart: loaded from the store).
        self.load_live(&svc.id, &env.name).await?;
        log.stream(Stream::System).line(match tpl.run.kind {
            RunType::Web => "rolling out: starting the new deployment next to the old one".to_string(),
            RunType::Worker => "rolling out the worker".to_string(),
            RunType::Cron => "the next scheduled run uses this deployment".to_string(),
        });
        let prepared = live.prepare(&tpl, None).await?;
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
        // A gradual rollout needs something to split: the old deployment running and taking
        // requests. Asleep, it switches at once (nothing would judge the new one).
        let gradual = !tpl.run.rollout.steps.is_empty() && tpl.run.kind == RunType::Web && env.active.is_some() && live.awake() > 0;
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
        let (sid, e, from, to, vh) = (svc.id.clone(), env.name.clone(), env.active.clone(), d.id.clone(), vars_hash.clone());
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
                .db(move |s| s.switch_active(&Switch { service: &sid, env: &e, from: from.as_deref(), to: &to, ready_ms: ready, epoch, vars_hash: Some(&vh) }))
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
                for (dep, s) in live.take_served() {
                    let into = if dep == tpl.deployment { &mut new } else { &mut old };
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
    async fn check_registry(&self, image: &str) -> anyhow::Result<()> {
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
        let ctx = context.clone();
        let plan = tokio::task::spawn_blocking(move || apps_build::plan(&apps_build::DirTree::new(ctx), &overrides)).await?.map_err(|e| anyhow::anyhow!("{e}"))?;
        for n in &plan.notes {
            log.line(format!("plan: {n}"));
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
        };
        log.line(format!("building with {}", self.deps.builder.name()));
        let image = self.deps.builder.build(&job, log, cancel).await?;
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
        let mut vars = self.resolve_vars(&probe, env, &dep.id).await?.vars;
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
        Ok(Template { deployment: dep.id.clone(), spec, run, spa })
    }

    // ── the owner's background work ──

    /// Start the background loops. Each acts only on services this node owns.
    pub fn start(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move { this.ownership().await });
        let this = self.clone();
        tokio::spawn(async move { this.reaper().await });
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
        let known: Vec<String> = self.lives().iter().filter(|l| l.service == service).flat_map(|l| l.instances().into_iter().map(|i| i.name.clone())).collect();
        for name in self.deps.runtime.list(Some(service)).await {
            if !known.contains(&name) {
                self.deps.runtime.remove(&name).await;
            }
        }
        // Jobs an earlier process ran had their containers removed just above.
        let sid = service.to_string();
        let _ = self.db(move |s| s.fail_running_jobs(&sid)).await;
        let sid = service.to_string();
        let Ok(envs) = self.db(move |s| s.environments(&sid)).await else { return };
        for e in envs {
            if let Err(err) = self.load_live(service, &e.name).await {
                warn!(service, env = %e.name, error = %err, "apps: loading an environment failed");
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
        // Deploys in flight stop here; the new owner resumes them from the store.
        let sid = service.to_string();
        if let Ok(pending) = self.db(move |s| s.pending_deployments(&sid)).await {
            for d in pending {
                if let Some(t) = self.cancels.lock().get(&d.id) {
                    t.cancel();
                }
            }
        }
        let lives: Vec<_> = self.lives().into_iter().filter(|l| l.service == service).collect();
        for l in lives {
            self.lives.lock().remove(&crate::env_key(&l.service, &l.env));
            l.shutdown().await;
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

    /// An environment that went into a crash loop gets one event for it (until it leaves it).
    async fn note_crash_loop(self: &Arc<Self>, l: &crate::live::EnvLive) {
        let key = format!("{}/{}", l.service, l.env);
        match l.crash_loop() {
            Some(c) => {
                if self.looping.lock().insert(key) {
                    let (sid, e, dep) = (l.service.clone(), l.env.clone(), l.template().map(|t| t.deployment.clone()));
                    let data = serde_json::json!({ "crashes": c.crashes, "last_error": c.last_error });
                    let _ = self.db(move |s| s.add_event("crash_loop", &sid, &e, dep.as_deref(), &data)).await;
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
        let (a, limit_mb) = (tpl.run.alerts, tpl.run.memory_mb);
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
        for kind in ["alert.memory", "alert.errors", "alert.latency"] {
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
                self.cron_tick().await;
            }
            if n.is_multiple_of(60) {
                self.sweep_uploads().await;
                for l in self.lives().into_iter().filter(|l| self.owns(&l.service)) {
                    self.judge_alerts(&l).await;
                }
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

    /// Cron: start the runs that are due in owned cron services (a run still going when the
    /// next is due is skipped).
    async fn cron_tick(self: &Arc<Self>) {
        let Ok(services) = self.db(|s| s.services(None)).await else { return };
        let now = code_sql::now_ms();
        for svc in services.into_iter().filter(|s| s.run.kind == RunType::Cron && self.owns(&s.id) && !self.capped(&s.id)) {
            let Some(schedule) = svc.run.schedule.as_deref().and_then(|s| crate::cron::Schedule::parse(s).ok()) else { continue };
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
        let run = self.db(move |s| s.start_run(&sid, &e, &a, t)).await?;
        if run.status == RunStatus::Skipped {
            return Ok(run);
        }
        let live = self.load_live(service, env).await?;
        let (this, id, sid, e, dep) = (self.clone(), run.id.clone(), service.to_string(), env.to_string(), active.clone());
        tokio::spawn(async move {
            let (status, code) = match live.run_once(timeout).await {
                Ok((_, _, true)) => (RunStatus::TimedOut, None),
                Ok((_, Some(0), _)) => (RunStatus::Succeeded, Some(0)),
                Ok((_, code, _)) => (RunStatus::Failed, code),
                Err(e) => {
                    live.log.stream(Stream::System).line(format!("the run didn't start: {e}"));
                    (RunStatus::Failed, None)
                }
            };
            let failed = matches!(status, RunStatus::Failed | RunStatus::TimedOut);
            let data = serde_json::json!({ "run": id, "status": status.as_str(), "exit_code": code });
            let _ = this.db(move |s| s.finish_run(&id, status, code)).await;
            // A failed or timed-out run is an incident's start: an event (notified by default).
            if failed {
                let _ = this.db(move |s| s.add_event("cron.failed", &sid, &e, Some(&dep), &data)).await;
            }
        });
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

