//! Deploys (docs/apps/README.md, "Deploys"): one queue per service and environment on the
//! service's owner. A deployment is built (or its image pulled), rolled out next to the old one,
//! health-checked, and switched to with a compare-and-swap; the old instances drain.
//!
//! Also the owner's background loops: the reaper and meter, deploy-on-push from roll code's
//! land log, cron schedules, and custom domains' DNS checks.

use crate::config::Apply;
use crate::live::{EnvLive, Template};
use crate::logs::{LogSink, Stream};
use crate::model::*;
use crate::runtime::{BuildJob, InstanceSpec};
use crate::store::{EnvRow, NewDeployment, ServiceRow, Switch};
use crate::{Apps, Error, PENDING_CLAIM_TTL, Result, closed_network_name, fingerprint, lease_key, network_name, volume_name};
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
        self.refuse_paused(service, env).await?;
        let mut o = body.as_object().cloned().unwrap_or_default();
        // A person's deploy in a freeze: refused, or let through with `override` (said).
        if matches!(by.trigger, Trigger::Manual | Trigger::Upload) {
            let over = o.remove("override").is_some_and(|v| v == true);
            self.refuse_frozen(service, env, over, by.actor.clone()).await?;
        }
        for k in o.keys() {
            if !["revision", "image", "message", "no_traffic", "no_cache", "image_upload", "closed"].contains(&k.as_str()) {
                return Err(Error::BadRequest(format!("unknown field {k}")));
            }
        }
        self.require_closed(service, env, &o).await?;
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
        let mut no_traffic = match o.get("no_traffic") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(Error::BadRequest("no_traffic is a boolean".into())),
        };
        // A person's deploy where approvals are asked for waits at ready for them.
        if matches!(by.trigger, Trigger::Manual | Trigger::Upload) && self.waits_for_approval(service, env).await? {
            no_traffic = true;
        }
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
            (Source::Git { url, .. }, false, None) => {
                // A full commit, or a branch or tag by name (deployed once: the environment
                // keeps following its own branch).
                let (sha, named) = match &revision {
                    Some(r) if r.len() == 40 && r.bytes().all(|b| b.is_ascii_hexdigit()) => (r.to_ascii_lowercase(), None),
                    Some(r) => {
                        let (sha, kind) = crate::git_source::named(&self.git_remote(service, url).await, r).await.map_err(|e| Error::BadRequest(format!("{url}: {e:#}")))?;
                        (sha, Some(format!("{kind} {r}")))
                    }
                    None => (crate::git_source::head(&self.git_remote(service, url).await, row.branch.as_deref()).await.map_err(|e| Error::BadRequest(format!("{url}: {e:#}")))?.0, None),
                };
                if message.is_empty() {
                    message = format!("{} at {}", named.unwrap_or_else(|| row.branch.clone().unwrap_or_else(|| "HEAD".into())), &sha[..12]);
                }
                (Some(sha), None, trigger)
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
        // An upload is a tree (`roll up`) or an image built elsewhere (`--image-file`).
        let image_upload = o.get("image_upload") == Some(&Value::Bool(true));
        if image_upload && upload.is_none() {
            return Err(Error::BadRequest("image_upload is an uploaded image archive's".into()));
        }
        let upload_key = upload.as_ref().map(|_| format!("apps/uploads/{}.{}", random(16), if image_upload { "image.tar" } else { "tgz" }));
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

    /// `closed: true` in a deploy's body (roll previews' for a fork's head): refused unless the
    /// environment is closed and this node builds it closed (its own network, no secret value).
    /// A node that predates the field refuses it as unknown, so the request fails closed.
    async fn require_closed(self: &Arc<Self>, service: &str, env: &str, o: &serde_json::Map<String, Value>) -> Result<()> {
        match o.get("closed") {
            None | Some(Value::Null | Value::Bool(false)) => Ok(()),
            Some(Value::Bool(true)) => {
                let (sid, e) = (service.to_string(), env.to_string());
                let row = self.db(move |s| s.environment(&sid, &e)).await?;
                if !row.egress.closed() || !self.deps.runtime.closes_egress() {
                    return Err(Error::Conflict(format!("{env} isn't built closed here (egress roll, on a node that closes it)")));
                }
                Ok(())
            }
            Some(_) => Err(Error::BadRequest("closed is a boolean".into())),
        }
    }

    /// `POST …/redeploy`: the active deployment's image again, with the service's current run
    /// settings and variables (variables are read when its instances are made), without a build
    /// and without moving to another revision. What a change of variables or settings needs,
    /// for every source (an upload service has no source to deploy again).
    /// With `key` (an `Idempotency-Key`), a retry answers the redeploy the first call queued, as a
    /// deploy's does (one table of keys per environment for both).
    pub async fn redeploy(self: &Arc<Self>, service: &str, env: &str, body: Value, actor: Option<String>, key: Option<&str>) -> Result<Deployment> {
        self.refuse_capped(service)?;
        self.refuse_paused(service, env).await?;
        // Where approvals are asked for, it waits at ready for them.
        let waits = self.waits_for_approval(service, env).await?;
        let mut body = body;
        let over = body.as_object_mut().and_then(|o| o.remove("override")).is_some_and(|v| v == true);
        self.refuse_frozen(service, env, over, actor.clone()).await?;
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
        if let Some(k) = o.keys().find(|k| !matches!(k.as_str(), "message" | "closed")) {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        self.require_closed(service, env, &o).await?;
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
        // Without a message of its own: what it puts live, as `…/pending` names it (Heroku's
        // "Set LOG_LEVEL config vars"), names only.
        let message = match message {
            Some(m) => m,
            None => match self.pending(service, env).await.ok().as_ref().and_then(changes_said) {
                Some(said) => format!("Redeploy: {said}"),
                None => format!("Redeploy of {}", old.id),
            },
        };
        let d = NewDeployment {
            trigger: Trigger::Settings,
            revision: old.revision.clone(),
            message,
            image: Some(image),
            plan: old.plan.clone(),
            rollback_of: None,
            run,
            upload: None,
            status: DeployStatus::Queued,
            actor,
            no_traffic: waits,
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
        self.refuse_paused(service, env).await?;
        let mut o = body.as_object().cloned().unwrap_or_default();
        let over = o.remove("override").is_some_and(|v| v == true);
        self.refuse_frozen(service, env, over, actor.clone()).await?;
        let waits = self.waits_for_approval(service, env).await?;
        if let Some(k) = o.keys().find(|k| !["from", "message", "expect"].contains(&k.as_str())) {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        // What the caller looked at (roll previews' sheet, a reviewer): that deployment or nothing.
        let expect = match o.get("expect") {
            None | Some(Value::Null) => None,
            Some(Value::String(e)) => Some(e.clone()),
            Some(_) => return Err(Error::BadRequest("expect is a deployment id".into())),
        };
        let from = o.get("from").and_then(Value::as_str).ok_or_else(|| Error::BadRequest("from (the environment to promote) is required".into()))?;
        if from == env {
            return Err(Error::BadRequest("from is another environment".into()));
        }
        let message = o.get("message").and_then(Value::as_str).map(|m| m.chars().take(500).collect::<String>());
        let (sid, e, f) = (service.to_string(), env.to_string(), from.to_string());
        let (svc, _, source) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?, s.environment(&sid, &f)?))).await?;
        let active = source.active.clone().ok_or_else(|| Error::Conflict(format!("nothing is deployed in {from} yet")))?;
        // Checked against the read this promotion is made from (its image is `active`'s, whatever
        // `from` runs a moment later): what goes live is what was looked at.
        if let Some(e) = &expect
            && *e != active
        {
            return Err(Error::Conflict(format!("{from} runs {active} now, not {e}: look at it again before promoting")));
        }
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
            no_traffic: waits,
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
    /// Back to what was live is never frozen; putting a build live that never was (one held at
    /// `ready`) is a deploy, so a freeze refuses it without `over`.
    pub async fn rollback(self: &Arc<Self>, service: &str, env: &str, dep: &str, actor: Option<String>, keep_variables: bool, over: bool) -> Result<Deployment> {
        self.refuse_capped(service)?;
        self.refuse_paused(service, env).await?;
        let old = self.deployment(service, env, dep).await?;
        let (sid, e) = (service.to_string(), env.to_string());
        let needed = self.db(move |s| s.environment(&sid, &e)).await?.approvals;
        if old.status == DeployStatus::Ready {
            self.refuse_frozen(service, env, over, actor.clone()).await?;
            // A never-live build goes live with its approvals (rolling back to what was live
            // needs none), or an admin's override, said in the audit log.
            let author = self.deploy_person(dep).await.or(old.actor.clone());
            let counted = old.approved_by.iter().filter(|a| Some(a.as_str()) != author.as_deref()).count() as u32;
            if counted < needed {
                if !over {
                    return Err(Error::Conflict(format!("{dep} needs {needed} approval(s) from people other than who started it ({counted} so far): POST …/deploys/{dep}/approve, or an admin's `X-Roll-Override: 1`")));
                }
                self.audit("approvals.bypassed", service, env, actor.clone(), json!({ "deployment": dep, "approvals": counted, "needed": needed })).await;
            }
        }
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
        // Its settings as they were (a rollback over a bad start command needs them), but the
        // volume is the service's as it is now: its data lives there, and a deployment from before
        // the volume was added must not run without it.
        let mut run = old.run.clone();
        let (sid, e) = (service.to_string(), env.to_string());
        let (svc, row) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await?;
        run.volume = svc.for_env(&row).map(|s| s.run.volume).unwrap_or(svc.run.volume);
        let d = NewDeployment {
            trigger: Trigger::Rollback,
            revision: old.revision.clone(),
            message: format!("Roll back to {}", old.id),
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
        // Held from here: the next push mustn't put back what was just rolled away from (a
        // person's own hold stays theirs: putting a held build live is a rollback to it, and so
        // is an approved one, whose environment's pushes wait at ready anyway).
        let (sid, e, rid) = (service.to_string(), env.to_string(), dep.id.clone());
        let approved = needed > 0 && old.status == DeployStatus::Ready;
        if !approved && self.held(service, env).await.as_deref() != Some(MANUAL_HOLD) && self.db(move |s| s.set_hold(&sid, &e, Some(&rid))).await.unwrap_or(false) {
            self.logs.sink(format!("env:{service}/{env}"), Stream::System).line("deploys held after the rollback: pushes, hooks and tag moves aren't put live until you deploy or resume them");
        }
        self.kick(service, env);
        Ok(dep)
    }

    /// A person deployed (a deploy, a redeploy, a promotion): a hold after a rollback is lifted
    /// (a person's own hold isn't: only `held: false` ends it).
    pub async fn lift_hold(self: &Arc<Self>, service: &str, env: &str) {
        if self.held(service, env).await.as_deref() == Some(MANUAL_HOLD) {
            return;
        }
        let (sid, e) = (service.to_string(), env.to_string());
        if self.db(move |s| s.set_hold(&sid, &e, None)).await.unwrap_or(false) {
            self.logs.sink(format!("env:{service}/{env}"), Stream::System).line("deploys resumed");
            self.changed(service);
        }
    }

    /// The rollback the environment's deploys are held after, if they are.
    pub(crate) async fn held(self: &Arc<Self>, service: &str, env: &str) -> Option<String> {
        let (sid, e) = (service.to_string(), env.to_string());
        self.db(move |s| s.environment(&sid, &e)).await.ok().and_then(|r| r.held_by)
    }

    /// What automation's deploy (a push, a hook, a tag move, a CI wait) does in the
    /// environment now ([`hold_of`]).
    pub(crate) async fn hold_for(self: &Arc<Self>, service: &str, env: &str) -> Held {
        let (sid, e) = (service.to_string(), env.to_string());
        match self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await {
            Ok((svc, row)) => hold_of(&svc, &row),
            Err(_) => Held::No,
        }
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
        self.rollback(service, env, &id, actor, keep_variables, false).await
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
        let deps = self.db(move |s| {
            s.environment(&sid, &e)?;
            s.deployments(&sid, &e, 100, before)
        })
        .await?;
        self.with_scans(service, deps).await
    }

    /// Old deployments of the services this node owns, past what each environment keeps
    /// (`keep_deployments`): pruned in the store, then their images (once no deployment names
    /// them), static sites and build logs go; and deleted services kept past
    /// [`crate::DELETED_KEPT_MS`] are deleted for good (by their owner, a node taking the lease
    /// of one nobody holds); closed builds' networks nothing runs on go too
    /// ([`crate::runtime::Runtime::sweep_build_networks`]). Every ten minutes.
    pub async fn prune(self: &Arc<Self>) {
        let now = code_sql::now_ms();
        // Closed builds' networks nothing runs on, but never those of a project with a closed
        // environment here (a build may be setting one up).
        if let Ok((envs, all)) = self.db(|s| Ok((s.all_environments()?, s.services(None)?))).await {
            let closed: std::collections::HashSet<String> = envs.iter().filter(|e| e.egress.closed()).filter_map(|e| all.iter().find(|s| s.id == e.service).map(|s| s.project.clone())).collect();
            self.deps.runtime.sweep_build_networks(&closed).await;
        }
        if let Ok(deleted) = self.db(|s| s.deleted_services(None)).await {
            for (svc, at) in deleted.into_iter().filter(|(_, at)| now - at >= crate::DELETED_KEPT_MS) {
                let key = lease_key(&svc.id);
                if !self.deps.cluster.owns(&key) && self.deps.cluster.preferred(&key).is_none_or(|n| n.id == self.deps.cluster.node_id()) {
                    let _ = self.deps.cluster.lease(&key).await;
                }
                if self.owns(&svc.id)
                    && let Err(e) = self.purge_service(&svc.id).await
                {
                    warn!(service = %svc.id, deleted_at = at, error = %e, "apps: deleting a deleted service for good failed");
                }
            }
        }
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
        let id = dep.to_string();
        d.vulnerabilities = self.db(move |s| s.scan(&id)).await?.map(|(sum, _)| sum);
        let id = dep.to_string();
        d.approved_by = self.db(move |s| s.approvals_of(&id)).await?;
        Ok(d)
    }

    /// The person who started a deployment (their session's, their own token's, the operator's),
    /// where it's known: what an approval is compared with.
    pub(crate) async fn deploy_person(self: &Arc<Self>, dep: &str) -> Option<String> {
        let d = dep.to_string();
        self.db(move |s| s.deploy_person(&d)).await.ok().flatten()
    }

    /// Who started `dep`, as a person, kept for its approvals.
    pub async fn note_deploy_person(self: &Arc<Self>, dep: &str, person: Option<String>) {
        let Some(p) = person else { return };
        let d = dep.to_string();
        let _ = self.db(move |s| s.set_deploy_person(&d, &p)).await;
    }

    /// Under the environment's `approvals`, a person's deploy waits at `ready` for them: whether
    /// it does (a web service without a volume, which can wait beside the live one), or why it
    /// can't (409).
    pub(crate) async fn waits_for_approval(self: &Arc<Self>, service: &str, env: &str) -> Result<bool> {
        let (sid, e) = (service.to_string(), env.to_string());
        let Ok((svc, row)) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await else { return Ok(false) };
        if row.approvals == 0 {
            return Ok(false);
        }
        let run = svc.for_env(&row).map(|s| s.run).unwrap_or(svc.run);
        match run.kind == RunType::Web && run.volume.is_none() {
            true => Ok(true),
            false => Err(Error::Conflict(format!("{env}'s deploys need {} approval(s) before they go live, and this service's can't wait beside the live one for them", row.approvals))),
        }
    }

    /// `POST …/deploys/{dep}/approve` (GitHub Environments' required reviewers): a person who
    /// didn't start it approves a deployment waiting at `ready`; the approval that makes the
    /// environment's count puts it live (a rollback to it, as "Put live", which a freeze still
    /// stops). `who` is the person, never a token's.
    pub async fn approve(self: &Arc<Self>, service: &str, env: &str, dep: &str, who: Option<String>) -> Result<Value> {
        let d = self.deployment(service, env, dep).await?;
        let (sid, e) = (service.to_string(), env.to_string());
        let row = self.db(move |s| s.environment(&sid, &e)).await?;
        if row.approvals == 0 {
            return Err(Error::Conflict(format!("{env} asks for no approvals")));
        }
        if d.status != DeployStatus::Ready {
            return Err(Error::Conflict(format!("{dep} isn't waiting at ready to go live")));
        }
        // Something newer went live since: approving this one would put an older build back.
        if let Some(a) = row.active.clone() {
            let live = self.db(move |s| s.deployment(&a)).await?;
            let since = live.rollback_of.clone().unwrap_or(live.id.clone());
            let (s_id, dc) = (since.clone(), d.created_at);
            let newer = since != d.id && self.db(move |s| s.deployment(&s_id)).await.is_ok_and(|o| o.created_at > dc);
            if newer {
                return Err(Error::Conflict(format!("a newer deployment ({since}) went live after {dep}: deploy again to approve what's next")));
            }
        }
        let who = who.ok_or_else(|| Error::Forbidden("an approval is a person's: signed in, or the operator, never a token".into()))?;
        // Its author, as a person: whoever started it however they did (their own token is them).
        let author = self.deploy_person(dep).await.or(d.actor.clone());
        if author.as_deref() == Some(who.as_str()) {
            return Err(Error::Forbidden(format!("{who} started {dep}: someone else approves it")));
        }
        // Recorded and counted in one write: only the approval that reaches the count acts.
        let (id, w, au) = (dep.to_string(), who.clone(), author.clone());
        let (all, crossed) = self.db(move |s| s.approve(&id, &w, au.as_deref(), code_sql::now_ms(), row.approvals)).await?;
        let counted = all.iter().filter(|a| Some(a.as_str()) != author.as_deref()).count() as u32;
        self.audit("deploy.approved", service, env, Some(who.clone()), json!({ "deployment": dep, "approvals": counted, "needed": row.approvals })).await;
        let mut out = json!({ "deployment": dep, "approved_by": all, "needed": row.approvals, "live": null });
        if crossed {
            match self.rollback(service, env, dep, Some(who), true, false).await {
                Ok(live) => out["live"] = json!(live.id),
                Err(e) => out["waiting"] = json!(e.to_string()),
            }
        }
        Ok(out)
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
        let Some(rev) = d.revision.clone() else { return };
        let (sid, e) = (d.service.clone(), d.environment.clone());
        let Ok((svc, env)) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await else { return };
        // Someone else reports this environment's deploys (roll previews: one voice on a pull
        // request).
        if !env.report {
            return;
        }
        let status = crate::integrations::CommitStatus {
            context: format!("roll/apps/{}/{}", svc.name, d.environment),
            state,
            description: what.chars().take(140).collect(),
            target_url: Some(self.url(&env.slug)),
        };
        // A Git URL service connected to roll's GitHub App: through its installation, a commit
        // status and a GitHub Deployment of the environment (made when it starts, then its state).
        if let Source::Git { url, .. } = &svc.source {
            let Some((app, installation, owner, repo)) = self.github_for(&svc.id, url).await else {
                // GitLab or Bitbucket, with its Git login: the same status there.
                let remote = self.git_remote(&svc.id, url).await;
                if let (Some(host), Some(login)) = (crate::ci_hosts::host_of(url), remote.login())
                    && let Err(err) = crate::ci_hosts::post(self.cfg.allow_private, &host, login, &rev, &status).await
                {
                    warn!(service = %d.service, deployment = %d.id, error = %err, "apps: a deploy's commit status didn't post");
                }
                return;
            };
            if let Err(err) = app.status(installation, &owner, &repo, &rev, &status).await {
                warn!(service = %d.service, deployment = %d.id, error = %err, "apps: a deploy's commit status didn't post");
            }
            let did = d.id.clone();
            let known = self.db(move |s| s.github_deployment(&did)).await.ok().flatten();
            let github = match known {
                Some(g) => Some(g),
                None if state == "pending" => match app.deployment(installation, &owner, &repo, &rev, &d.environment, d.environment == crate::model::PRODUCTION, env.ephemeral).await {
                    Ok(g) => {
                        let did = d.id.clone();
                        let _ = self.db(move |s| s.set_github_deployment(&did, g)).await;
                        Some(g)
                    }
                    Err(err) => {
                        warn!(service = %d.service, deployment = %d.id, error = %err, "apps: a GitHub Deployment wasn't made");
                        None
                    }
                },
                None => None,
            };
            if let Some(g) = github {
                let gh_state = match state {
                    "pending" => "in_progress",
                    s => s,
                };
                let url = (state == "success").then(|| self.url(&env.slug));
                if let Err(err) = app.deployment_status(installation, &owner, &repo, g, gh_state, url.as_deref(), &status.description).await {
                    warn!(service = %d.service, deployment = %d.id, error = %err, "apps: a GitHub Deployment's state didn't post");
                }
            }
            return;
        }
        let (Source::Repo { repo, .. }, Some(sources)) = (&svc.source, self.deps.sources.clone()) else { return };
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
        // Queued before it was paused: it doesn't build or start now.
        anyhow::ensure!(!env.paused, "the environment is paused");
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
                let login = self.pull_login(&sid, img).await?;
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
        // Its sidecars' images pinned with it (a rollback or a redeploy runs the same ones).
        let mut run = run;
        // Every one's registry is checked, a pinned one's too (its pull comes at create).
        for sc in &run.sidecars {
            self.check_registry(&sc.image).await.map_err(|e| e.context(format!("sidecar {}", sc.name)))?;
        }
        if run.sidecars.iter().any(|s| !s.image.contains("@sha256:")) {
            for sc in run.sidecars.iter_mut().filter(|s| !s.image.contains("@sha256:")) {
                let pinned = self.deps.runtime.pull(&sc.image, log, None).await.map_err(|e| e.context(format!("sidecar {}", sc.name)))?;
                log.line(format!("sidecar {}: pinned {pinned}", sc.name));
                sc.image = pinned;
            }
            let (id, img, r) = (d.id.clone(), image.clone(), run.clone());
            self.db(move |s| s.set_built(&id, &img, None, Some(&r))).await?;
        }
        // Its image's known vulnerabilities: before the rollout with `run.block_on` (never a
        // rollback's: it's how out of a bad deploy), else beside it.
        match run.block_on.filter(|_| d.trigger != Trigger::Rollback) {
            Some(floor) => self.hold_on_vulnerabilities(&svc.id, &env.name, &d.id, &image, floor, &log.stream(Stream::System)).await?,
            None => {
                let (this, sid, en, did, img, log) = (self.clone(), svc.id.clone(), env.name.clone(), d.id.clone(), image.clone(), log.stream(Stream::System));
                tokio::spawn(async move { this.scan_deployment(&sid, &en, &did, &img, &log).await });
            }
        }
        dep.image = Some(image);
        dep.run = run;
        // The plan the build just stored (a single-page app, say).
        let did = d.id.clone();
        if let Ok(stored) = self.db(move |s| s.deployment(&did)).await {
            dep.plan = stored.plan;
        }
        // An environment with data of its own: a branch of each database linked since.
        self.ensure_branches(&svc, &env.name).await.map_err(|e| anyhow::anyhow!("data: {e:#}"))?;
        // A static site: its files out of the image, so they're served without an instance.
        if dep.plan.as_ref().is_some_and(|p| p["start"] == apps_build::STATIC_START)
            && dep.run.kind == RunType::Web
            && dep.run.volume.is_none()
            && let Some(img) = dep.image.clone()
        {
            // Its files hold no secret value (Netlify's secrets scan): one that does fails the
            // deploy (a container would serve the same file).
            let r = self.resolve_vars(&svc, &env, &dep.id).await?;
            let secrets: Vec<(String, String)> = r.vars.iter().filter(|(k, _)| r.is_secret(k)).cloned().collect();
            let checks_secrets = !secrets.is_empty();
            match self.export_site_checked(&img, secrets).await {
                Ok(Some(n)) => log.stream(Stream::System).line(format!("static site: {} KiB of files, served without an instance", n.div_ceil(1024))),
                Ok(None) => {}
                Err(e) if e.downcast_ref::<crate::sites::SecretInSite>().is_some() => return Err(e),
                // Unchecked files aren't served either: a container would serve the same ones.
                Err(e) if checks_secrets => anyhow::bail!("its files couldn't be checked for its secrets' values ({e:#}), so they aren't served: make the site smaller, or keep secret values out of its environment"),
                Err(e) => log.stream(Stream::System).line(format!("static site: its files couldn't be exported ({e:#}); a container serves it")),
            }
        }
        // Read before the instances' values are (a change in between then says "changed", the
        // safe side, never that nothing did): the secrets' digest, the environment's own
        // variables (what a rollback to it restores, kept with its switch to live) and the hash
        // of everything it reads.
        let secrets_digest = self.secrets_digest_of(&svc, &env.name).await;
        let (sid, e, did) = (svc.id.clone(), env.name.clone(), d.id.clone());
        let snapshot = self.db(move |s| s.seal_snapshot(&did, &sid, &e)).await?;
        let vars_hash = {
            let r = self.resolve_vars(&svc, &env, &dep.id).await?;
            r.check_refs()?;
            fingerprint(&r.vars_for_hash(), &svc, &env.egress)
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
        // Paused while it built: nothing of it starts or goes live.
        anyhow::ensure!(!live.paused(), "the environment was paused while it built: not put live");
        // Waiting for approvals (GitHub's environments withhold their secrets until then): built
        // and ready, but nothing of it starts with the environment's values before it's approved.
        if no_traffic && env.approvals > 0 {
            let (sid, en, did, data) = (svc.id.clone(), env.name.clone(), d.id.clone(), serde_json::json!({ "revision": d.revision, "approvals": env.approvals }));
            self.db(move |s| s.set_ready(&did, &sid, &en, &data)).await?;
            log.stream(Stream::System).line(format!("built, waiting for {} approval(s): it starts once approved, never before", env.approvals));
            return Ok(());
        }
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
            let short = d.id.strip_prefix("dep_").unwrap_or(&d.id);
            let url = self.url(&format!("{short}--{}", env.slug));
            // Ready, and someone told to put it live (`deploy.ready`), in one write: whoever sees
            // it ready sees the event too.
            let (sid, en, did, data) = (svc.id.clone(), env.name.clone(), d.id.clone(), serde_json::json!({ "revision": d.revision, "url": url }));
            self.db(move |s| s.set_ready(&did, &sid, &en, &data)).await?;
            log.stream(Stream::System).line(format!("ready, not live: its own URL is {url}; roll back to it to put it live"));
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
            // Its tree's edge settings are the environment's gate's now.
            live.forget_gate();
            // What the old deployment answered isn't this one's.
            self.responses.clear(&crate::env_key(&d.service, &d.environment));
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
        // Waiting for a person (`rollout.manual`) is undone however the rollout ends.
        struct Unwait(Arc<Apps>, String);
        impl Drop for Unwait {
            fn drop(&mut self) {
                self.0.rollout_waits.lock().remove(&self.1);
            }
        }
        let _unwait = Unwait(self.clone(), id.to_string());
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
                    into.add(&s);
                }
                if let Some(why) = judge(new, old, r.error_pct, tpl.run.slo.as_ref().map(|s| (s, r.slo_burn.unwrap_or(crate::model::Rollout::SLO_BURN))), r.p95_pct) {
                    sys.line(format!("canary: rolled back at {share}%: {why}"));
                    anyhow::bail!("rolled back at {share}% of requests: {why}");
                }
                if tokio::time::Instant::now() >= until {
                    break;
                }
            }
            sys.line(format!("canary: {share}% passed ({} requests, {:.1}% 5xx)", new.requests, new.error_pct()));
            if !r.manual {
                continue;
            }
            // Held at this split until a person says, still judged as it goes.
            // Someone has to say: they're told (`rollout.waiting`, once a step), before it shows as
            // waiting, so whoever sees it waiting sees the event.
            let (sid, en, did) = (service.to_string(), tpl.spec.env.clone(), id.to_string());
            let data = serde_json::json!({ "share": share });
            let _ = self.db(move |s| s.add_event("rollout.waiting", &sid, &en, Some(&did), &data)).await;
            let gate = Arc::new((parking_lot::Mutex::new(None), tokio::sync::Notify::new()));
            self.rollout_waits.lock().insert(id.to_string(), gate.clone());
            self.changed(service);
            sys.line(format!("canary: held at {share}% until someone advances it, puts it all live or rolls it back"));
            let said = loop {
                if let Some(a) = gate.0.lock().take() {
                    break a;
                }
                tokio::select! {
                    _ = gate.1.notified() => continue,
                    _ = tokio::time::sleep(every) => {}
                    _ = cancel.cancelled() => anyhow::bail!("canceled"),
                }
                anyhow::ensure!(self.owns(service), "this node no longer holds the service; the node that took it over deploys");
                let active = live.template().map(|t| t.deployment.clone());
                for (dep, s) in live.take_served() {
                    let into = if dep == tpl.deployment {
                        &mut new
                    } else if active.as_deref() == Some(dep.as_str()) {
                        &mut old
                    } else {
                        continue;
                    };
                    into.add(&s);
                }
                if let Some(why) = judge(new, old, r.error_pct, tpl.run.slo.as_ref().map(|s| (s, r.slo_burn.unwrap_or(crate::model::Rollout::SLO_BURN))), r.p95_pct) {
                    sys.line(format!("canary: rolled back at {share}% while held: {why}"));
                    anyhow::bail!("rolled back at {share}% of requests: {why}");
                }
            };
            self.rollout_waits.lock().remove(id);
            match said {
                crate::model::RolloutAction::Advance => sys.line("canary: advanced by a person"),
                crate::model::RolloutAction::All => {
                    sys.line("canary: put all live by a person");
                    return Ok(());
                }
                crate::model::RolloutAction::Abort => {
                    sys.line(format!("canary: rolled back at {share}% by a person"));
                    anyhow::bail!("rolled back at {share}% of requests by a person");
                }
            }
        }
        Ok(())
    }

    /// `POST …/deploys/{dep}/rollout {action}`: what a person says to a rollout held for them.
    /// 409 when it isn't waiting (not a manual rollout, between steps, done).
    pub fn rollout_action(&self, deployment: &str, action: crate::model::RolloutAction) -> Result<()> {
        let gate = self.rollout_waits.lock().get(deployment).cloned().ok_or_else(|| Error::Conflict(format!("{deployment} isn't waiting for anyone (a manual rollout waits after each step passes)")))?;
        *gate.0.lock() = Some(action);
        gate.1.notify_one();
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
        spec.vars.push(("ROLL_INSTANCE".into(), spec.name.clone()));
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
        // Metered by its builder's size where the builder gives it one.
        let factor = if self.deps.builder.sizes_builds() { svc.build.size.unwrap_or_default().factor() } else { 1.0 };
        self.live(&svc.id, &d.environment).meter.lock().build(code_sql::now_ms(), began.elapsed().as_secs_f64() * factor);
        result
    }

    async fn build_in(self: &Arc<Self>, svc: &ServiceRow, d: &Deployment, log: &LogSink, cancel: CancellationToken, dir: &Path) -> anyhow::Result<(String, Run)> {
        // An image built elsewhere: its names taken out, loaded, and tagged as a build's would be.
        if d.trigger == Trigger::Upload {
            let id = d.id.clone();
            if let Some(key) = self.db(move |s| s.deployment_upload(&id)).await?.filter(|k| k.ends_with(".image.tar")) {
                let bytes = match &self.deps.objects {
                    Some(o) => o.get(&key).await?.ok_or_else(|| anyhow::anyhow!("the upload is gone"))?,
                    None => Bytes::from(tokio::fs::read(self.cfg.home.join(&key)).await?),
                };
                log.line(format!("loading the uploaded image ({} MiB)", bytes.len() >> 20));
                let archive = dir.join("image.tar");
                let a = archive.clone();
                tokio::task::spawn_blocking(move || crate::image_upload::untagged(&bytes, &a)).await??;
                let tag = format!("roll-apps-{}:{}", svc.id.strip_prefix("svc_").unwrap_or(&svc.id), d.id.strip_prefix("dep_").unwrap_or(&d.id));
                let image = self.deps.builder.load(&archive, &tag, log).await?;
                let (id, img, r) = (d.id.clone(), image.clone(), svc.run.clone());
                self.db(move |s| s.set_built(&id, &img, None, Some(&r))).await?;
                return Ok((image, svc.run.clone()));
            }
        }
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
                if svc.build.submodules.unwrap_or(false) {
                    let allowed = svc.build.submodule_repos.clone().unwrap_or_default();
                    let n = crate::submodules::fetch(sources.as_ref(), repo, rev, &tree, &allowed, self.cfg.allow_private).await?;
                    if n > 0 {
                        log.line(format!("fetched {n} submodule(s)"));
                    }
                }
                root.clone()
            }
            (Source::Git { url, root }, _) => {
                let rev = d.revision.as_deref().ok_or_else(|| anyhow::anyhow!("no revision to build"))?;
                log.line(format!("fetching {url} at {}", &rev[..rev.len().min(12)]));
                let remote = self.git_remote(&svc.id, url).await;
                let files = crate::git_source::materialize_with(&remote, rev, &tree, svc.build.submodules.unwrap_or(false)).await?;
                log.line(format!("fetched {files} files"));
                if svc.build.lfs.unwrap_or(false) {
                    let n = crate::git_source::fetch_lfs(&remote, &tree).await?;
                    if n > 0 {
                        log.line(format!("fetched {n} Git LFS object(s)"));
                    }
                }
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
        // Its edge settings (headers, firewall, rate limit), kept with the deployment: they're the
        // gate's once it's live, and travel with its image to a promotion or a rollback.
        let edge = file.as_ref().map(crate::model::EdgeFile::from_app).transpose().map_err(|e| anyhow::anyhow!("{e}"))?.flatten();
        let id = d.id.clone();
        self.db(move |s| s.set_deploy_edge(&id, edge.as_ref())).await?;
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
        let resolved = self.resolve_vars(svc, &env_row, &d.id).await?;
        resolved.check_refs()?;
        // The environment's remote build cache (Turborepo, Nx), unless its variables name another
        // (Vercel's, a team's own). Never a closed build's: a fork's code would fill a cache the
        // trusted builds read.
        // Nor a deploy without the cache (one that suspects what's in it).
        let did = d.id.clone();
        let uncached = self.db(move |s| s.no_cache(&did)).await?;
        let cache_vars = match env_row.egress.closed() || uncached {
            true => Vec::new(),
            false => self.build_cache_vars(&svc.project, &crate::build_cache::scope(&svc.source, &svc.id, &d.environment)).await?.into_iter().filter(|(k, _)| !resolved.vars.iter().any(|(v, _)| v == k || (k.starts_with("TURBO_") && v.starts_with("TURBO_")) || (k.starts_with("NX_SELF_HOSTED") && v.starts_with("NX_SELF_HOSTED")))).collect(),
        };
        // Its secret values never reach the build's log, whatever a step prints.
        let mut hidden = resolved.secret_values();
        hidden.extend(cache_vars.iter().filter(|(k, _)| k.ends_with("TOKEN")).map(|(_, v)| v.clone()));
        self.logs.redact(&format!("build:{}", d.id), &d.id, hidden);
        let closed_vars = env_row.egress.closed().then(|| resolved.without_secrets());
        let mut build_env = resolved.vars;
        // Instances' own addresses (kept awake, roll's OIDC tokens) are the running instances',
        // never a build's: a build step can't mint the service's cloud tokens, and services that
        // build alike share a build.
        build_env.retain(|(k, _)| !matches!(k.as_str(), "ROLL_AWAKE_URL" | "ROLL_OIDC_URL"));
        // A closed environment builds on a network of its project's own with no route out and no
        // way to the host (`Runtime::build_egress`): its steps reach public hosts only, through
        // roll's proxy (packages, base images), never the node's services, private ranges or its
        // metadata. It still gets no secret value: none of what roll's logs hide (sealed and roll
        // secrets values, links' credentials, URL passwords), nor a variable that holds one. Its
        // code can't send what it never has.
        let build_egress = match env_row.egress.closed() {
            // Never on the default network instead: a node that can't build it closed fails it.
            true => Some(self.deps.runtime.build_egress(&svc.project).await.map_err(|e| anyhow::anyhow!("a closed build's network: {e:#}"))?.ok_or_else(|| anyhow::anyhow!("this node can't build a closed environment (it needs Docker here)"))?),
            false => None,
        };
        if build_egress.is_some() {
            log.line("a closed environment builds on a network of its own: out only through roll's proxy, to public hosts");
        }
        if let Some(kept) = closed_vars {
            let before = build_env.len();
            build_env = kept;
            if build_env.len() < before {
                log.line(format!("a closed environment builds without its secret values ({} left out; its instances get them)", before - build_env.len()));
            }
        }
        // What's only for its instances never reaches a step.
        let (sid, e) = (svc.id.clone(), d.environment.clone());
        let runtime_only = self.db(move |s| s.runtime_only(&sid, &e)).await?;
        if !runtime_only.is_empty() {
            build_env.retain(|(k, _)| !runtime_only.contains(k));
            log.line(format!("{} variable(s) only for its instances, left out of the build", runtime_only.len()));
        }
        build_env.extend(cache_vars);
        // Per deployment (the cache would never hit) or only for the running app.
        build_env.retain(|(k, _)| k != "ROLL_DEPLOYMENT" && k != "PORT");
        // The commit changes every push: out of the hash and the secrets, or no build would hit
        // the cache. A tree's own Dockerfile still gets it as an `ARG` it declares, which Docker
        // keys its own cache on.
        let commit = build_env.iter().position(|(k, _)| k == "ROLL_GIT_COMMIT").map(|i| build_env.remove(i));
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
        // Variables kept as files are files in the build's steps too, at the same paths.
        let (sid, e) = (svc.id.clone(), d.environment.clone());
        overrides.build_files = self.db(move |s| s.variable_files(&sid, &e)).await?.into_iter().filter(|(k, _)| overrides.build_env.contains(k)).collect();
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
        // vercel.json's crons: schedules that call their paths, unless the service already
        // calls that path (its own schedules win).
        if run.kind == RunType::Web && !run.tcp {
            for (i, c) in plan.crons.iter().enumerate() {
                let name = format!("vercel-{}", i + 1);
                if run.schedules.len() < 20 && !run.schedules.iter().any(|s| s.path.as_deref() == Some(c.path.as_str()) || s.name == name) {
                    log.line(format!("plan: vercel.json's cron {} calls {}", c.schedule, c.path));
                    run.schedules.push(crate::model::Scheduled { name, schedule: c.schedule.clone(), command: String::new(), path: Some(c.path.clone()), timezone: None, timeout_s: None, auth: None, cpu: None, memory_mb: None });
                }
            }
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
        let mut plan_json = serde_json::to_value(&plan)?;
        // The images it builds on, to watch for their fixes (`watch_bases`).
        let base_text = if plan.provider == apps_build::Provider::Dockerfile { std::fs::read_to_string(context.join(&dockerfile)).unwrap_or_default() } else { plan.dockerfile.clone() };
        let bases = apps_build::base_images(&base_text);
        if plan.provider == apps_build::Provider::Dockerfile {
            build_env.extend(commit);
        }
        // `build.size`: what the builder gives it, where it can (one node's Docker can't).
        let size = svc.build.size.unwrap_or_default();
        if size != crate::model::BuildSize::Small {
            match self.deps.builder.sizes_builds() {
                true => log.line(format!("a {} builder: {} vCPU, {} GiB", serde_json::to_value(size).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default(), size.resources().0, size.resources().1 / 1024)),
                false => log.line("build.size: this node's builds share its BuildKit, unsized (it applies on the fleet's builders)"),
            }
        }
        let job = BuildJob {
            service: svc.id.clone(),
            deployment: d.id.clone(),
            context,
            dockerfile,
            tag: format!("roll-apps-{}:{}", svc.id.strip_prefix("svc_").unwrap_or(&svc.id), d.id.strip_prefix("dep_").unwrap_or(&d.id)),
            cpu: size.resources().0,
            memory_mb: size.resources().1,
            timeout: build_timeout(svc.build.timeout_s, self.cfg.build_timeout, log),
            own_dockerfile: plan.provider == apps_build::Provider::Dockerfile,
            env: build_env,
            env_hash,
            no_cache,
            // A stage of the tree's own Dockerfile; a generated one's are roll's.
            target: svc.build.target.clone().filter(|_| plan.provider == apps_build::Provider::Dockerfile),
            layers,
            egress: build_egress,
            priority: !ephemeral,
        };
        // The same inputs built before in this project (Railway's skipped builds): the same tree,
        // Dockerfile, target and build variables, and its image still kept. Never with `no_cache`.
        let build_key = if job.no_cache { None } else { Some(build_key(&job, &plan.dockerfile, self.deps.builder.name()).await?) };
        let reused = match build_key.clone() {
            Some(k) => {
                let p = svc.project.clone();
                self.db(move |s| s.built_image(&p, &k)).await?
            }
            None => None,
        };
        let image = if let Some((from, image)) = reused {
            log.line(format!("{from} was built from the same source, settings and build variables: its image goes on, no build"));
            image
        } else {
        log.line(format!("building with {}", self.deps.builder.name()));
        match self.deps.builder.build(&job, log, cancel.clone()).await {
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
        }
        };
        // Each base's digest as the registry says it now (what the build just resolved): all
        // asked at once, ten seconds for the lot.
        let asked = futures::future::join_all(bases.iter().map(|b| async move { (b, tokio::time::timeout(std::time::Duration::from_secs(10), self.deps.runtime.remote_digest(b, None)).await) })).await;
        let mut pinned = Vec::new();
        for (b, r) in asked {
            if let Ok(Ok(p)) = r
                && let Some((_, digest)) = p.rsplit_once('@')
            {
                pinned.push(serde_json::json!({ "image": b, "digest": digest }));
            }
        }
        if !pinned.is_empty() {
            plan_json["bases"] = serde_json::Value::Array(pinned);
        }
        let (id, img, r) = (d.id.clone(), image.clone(), run.clone());
        self.db(move |s| {
            s.set_built(&id, &img, Some(&plan_json), Some(&r))?;
            match &build_key {
                Some(k) => s.set_build_key(&id, k),
                None => Ok(()),
            }
        })
        .await?;
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
        resolved.check_refs()?;
        // Its secret values never reach any of the environment's logs, whoever prints them: its
        // output, its requests' paths, its spans and its metrics.
        let secrets = resolved.secret_values();
        for k in [format!("env:{}/{}", svc.id, env.name), format!("http:{}/{}", svc.id, env.name), crate::traces::key(&svc.id, &env.name), crate::traces::metric_key(&svc.id, &env.name)] {
            self.logs.redact(&k, &dep.id, secrets.clone());
        }
        if !resolved.unresolved.is_empty() {
            self.logs.sink(format!("env:{}/{}", svc.id, env.name), Stream::System).line(format!("references that name nothing, left empty: {}", resolved.unresolved.join(", ")));
        }
        for n in &resolved.notices {
            self.logs.sink(format!("env:{}/{}", svc.id, env.name), Stream::System).line(n.clone());
        }
        let mut vars = resolved.vars;
        if run.kind != RunType::Web {
            vars.retain(|(k, _)| k != "PORT");
        }
        // Variables kept as files (Render's secret files): out of the environment, into the
        // instance at their paths.
        let (sid, e) = (svc.id.clone(), env.name.clone());
        let file_of = self.db(move |s| s.variable_files(&sid, &e)).await?;
        let mut files = Vec::new();
        vars.retain(|(k, v)| match file_of.get(k) {
            Some(path) => {
                files.push((path.clone(), v.clone()));
                false
            }
            None => true,
        });
        // With `egress` `roll`: a network with no route out, and the listed hosts through its
        // proxy (the variables every HTTP client reads; the app's own win). Nothing outside roll
        // is reached any other way, so a runtime that can't close it never runs it open.
        let closed = env.egress.closed();
        if closed && !self.deps.runtime.closes_egress() {
            return Err(Error::Conflict("its egress is \"roll\", and this node's runtime can't close an environment's network".into()));
        }
        let network = if closed { closed_network_name(&svc.project, &env.name) } else { network_name(&svc.project, &env.name) };
        self.deps.runtime.ensure_network(&network, &svc.id, closed).await.map_err(|e| Error::Internal(e.to_string()))?;
        if closed && !env.egress.hosts.is_empty() {
            let proxy = self.deps.runtime.egress_proxy(&network, &svc.id, &env.egress.hosts).await.map_err(|e| Error::Internal(e.to_string()))?;
            let sid = svc.project.clone();
            let names: Vec<String> = self.db(move |s| s.services(Some(&sid))).await?.into_iter().map(|s| s.name).collect();
            for (k, v) in egress_vars(&proxy, &names, &vars) {
                if !vars.iter().any(|(n, _)| n == &k) {
                    vars.push((k, v));
                }
            }
            // Keys used without being seen (roll secrets' placeholders): its proxies swap them for
            // the hosts the keys are bound to, and its instances trust the interception CA (with
            // the host's roots, one file every runtime reads).
            match self.deps.secrets.intercept(&svc.project, &env.name).await {
                Ok(Some(i)) => {
                    self.deps.runtime.set_interception(&network, Some(i.interception.clone()));
                    files.push((INTERCEPT_CA.to_string(), trust_bundle(&i.ca_pem)));
                    for k in ["NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE"] {
                        if !vars.iter().any(|(n, _)| n == k) {
                            vars.push((k.to_string(), INTERCEPT_CA.to_string()));
                        }
                    }
                    self.intercept_cas.lock().insert(crate::env_key(&svc.id, &env.name), ca_digest(&i.ca_pem));
                }
                Ok(None) => self.deps.runtime.set_interception(&network, None),
                Err(e) => {
                    self.deps.runtime.set_interception(&network, None);
                    self.logs.sink(format!("env:{}/{}", svc.id, env.name), Stream::System).line(format!("keys bound to hosts can't be swapped by its proxy now ({e:#}): their placeholders reach no one"));
                }
            }
        }
        // An image service's registry login, for a runtime that pulls at each start; only the
        // registry it's for gets it.
        let registry = match &svc.source {
            Source::Image { .. } => {
                self.pull_login(&svc.id, &image).await?
            }
            _ => None,
        };
        // Other ports its custom domains send to (Railway's target ports).
        let (sid, en) = (svc.id.clone(), env.name.clone());
        let domain_ports = self.db(move |s| s.domain_ports(&sid, &en)).await?;
        // Each sidecar gets its own settings and only the variables it names.
        let sidecars: Vec<crate::runtime::SidecarSpec> = run
            .sidecars
            .iter()
            .filter(|_| matches!(run.kind, RunType::Web | RunType::Worker))
            .map(|sc| crate::runtime::SidecarSpec {
                name: sc.name.clone(),
                image: sc.image.clone(),
                command: sc.command.clone(),
                vars: sc.env.iter().map(|(k, v)| (k.clone(), v.clone())).chain(sc.variables.iter().filter_map(|n| vars.iter().find(|(k, _)| k == n).cloned())).collect(),
                cpu: sc.cpu,
                memory_mb: sc.memory_mb,
            })
            .collect();
        let spec = InstanceSpec {
            name: String::new(),
            service: svc.id.clone(),
            env: env.name.clone(),
            deployment: dep.id.clone(),
            image,
            command: run.start.clone(),
            vars,
            port: if run.kind == RunType::Web { run.port } else { None },
            // Custom domains' own ports, and its metrics endpoint's when that's another (never a
            // closed instance's: nothing reaches it but through its sandbox).
            ports: if closed {
                Vec::new()
            } else {
                let mut p = if run.kind == RunType::Web { domain_ports } else { Vec::new() };
                if let Some(mp) = run.metrics.as_ref().and_then(|m| m.port)
                    && Some(mp) != run.port.filter(|_| run.kind == RunType::Web)
                    && !p.contains(&mp)
                {
                    p.push(mp);
                }
                p
            },
            cpu: run.cpu,
            memory_mb: run.memory_mb,
            network: Some(network),
            closed,
            aliases: vec![svc.name.clone(), format!("{}.{}.internal", svc.name, env.name)],
            volume: run.volume.as_ref().map(|v| (volume_name(&svc.id, &env.name), v.mount.clone())),
            // A web service that scales to zero sleeps after `sleep_after`: an instance idle well
            // past that (its node gone) may end itself.
            idle_stop_s: (run.kind == RunType::Web && run.min_instances == 0).then(|| run.sleep_after_s.max(60) + 300),
            registry,
            files,
            health: run.health_command.as_ref().filter(|_| run.kind == RunType::Worker).map(|c| crate::runtime::HealthCheck {
                command: c.clone(),
                every: Duration::from_secs(run.health_every_s.unwrap_or(10) as u64),
                failures: run.health_failures.unwrap_or(3),
                start: Duration::from_secs(run.health_timeout_s as u64),
            }),
            sidecars,
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
        // Its edge functions, run by `<name>-fn` (what Run its functions makes).
        let edge: Vec<apps_build::edge::EdgeFn> = match &site {
            Some(_) => dep.plan.as_ref().and_then(|p| serde_json::from_value(p["edge"].clone()).ok()).unwrap_or_default(),
            None => Vec::new(),
        };
        let edge_service: String = format!("{}-fn", svc.name).chars().take(40).collect();
        Ok(Template { deployment: dep.id.clone(), spec, run, spa, site, edge, edge_service })
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
        // A deleted service runs nothing (its lease is held only to delete it for good).
        let sid = service.to_string();
        if self.db(move |s| s.service(&sid)).await.is_err() {
            return;
        }
        // The database's now: what started before is a previous owner's (or nobody's).
        let Ok(took) = self.db(|s| s.now()).await else { return };
        let known: Vec<String> = self.lives().iter().filter(|l| l.service == service).flat_map(|l| l.instances().into_iter().map(|i| i.name.clone())).collect();
        // The live deployments' instances still running (the previous owner died, they didn't)
        // stay, for `EnvLive::adopt` to keep; the rest (jobs, older deployments') go.
        let sid = service.to_string();
        let mut live_prefixes: Vec<String> = self
            .db(move |s| s.environments(&sid))
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|e| e.active.map(|a| crate::live::instance_prefix(service, &e.name, &a)))
            .collect();
        // And a deploy this node is running right now (the lease can be taken while a new
        // service's first deploy is preparing): its instances aren't a previous owner's.
        let sid = service.to_string();
        let pending = self.db(move |s| s.pending_deployments(&sid)).await.unwrap_or_default();
        live_prefixes.extend(pending.iter().filter(|d| self.cancels.lock().contains_key(&d.id)).map(|d| crate::live::instance_prefix(service, &d.environment, &d.id)));
        // A cron run or a job this node started since it took the service (the schedule fires as
        // soon as it owns it) is its own, not a leftover.
        let own_runs: Vec<String> = self.running_runs.lock().iter().filter(|(_, (s, _))| *s == service).map(|(id, _)| format!("run-{}", id.strip_prefix("run_").unwrap_or(id))).collect();
        let own_jobs = self.running_jobs.lock().values().any(|k| k.starts_with(&format!("{service}/")));
        for name in self.deps.runtime.list(Some(service)).await {
            // A known instance's network sandbox (`<instance>-net`) and waker (`-wake`) are its own.
            let owner = name.strip_suffix("-net").or_else(|| name.strip_suffix("-wake")).unwrap_or(&name).to_string();
            if known.contains(&name) || known.contains(&owner) || own_runs.iter().any(|r| name.ends_with(r.as_str())) || (own_jobs && name.contains("-run")) {
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
        // Its environments that follow the project's secrets: watched here now.
        self.watch_secrets_of(service).await;
    }

    async fn release_service(self: &Arc<Self>, service: &str) {
        // Its cached answers: the next owner's deploys never clear this node's.
        self.responses.clear_service(service);
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
        let runs: Vec<String> = self.running_runs.lock().iter().filter(|(_, (s, _))| *s == service).map(|(id, _)| id.clone()).collect();
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
                l.retire().await;
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
        // Five minutes for the alerts; six hours with an objective, for its burn.
        let back = if tpl.run.slo.is_some() { crate::slo::LONGEST } else { 5 };
        let Ok(all) = self.db(move |s| s.minutes(&sid, &e, now - back)).await else { return };
        let all: Vec<_> = all.into_iter().filter(|r| r.minute < now).collect();
        let rows: Vec<_> = all.iter().filter(|r| r.minute >= now - 5).cloned().collect();
        let mut firing: Vec<(&'static str, serde_json::Value)> = Vec::new();
        if a.memory_pct > 0 && rows.len() >= 5 {
            let share = |r: &crate::store::MinuteRow| r.memory_bytes as f64 / r.instances.max(1) as f64 / (limit_mb as f64 * 1048576.0) * 100.0;
            if rows.iter().all(|r| r.instances > 0 && share(r) >= a.memory_pct as f64) {
                let value = rows.iter().map(share).fold(0.0, f64::max).round();
                firing.push(("alert.memory", serde_json::json!({ "value": value, "threshold": a.memory_pct, "limit_mb": limit_mb })));
            }
        }
        // CPU: what its instances used of their `cpu` (CPU seconds a minute over instances × cpu × 60).
        if a.cpu_pct > 0 && rows.len() >= 5 && tpl.run.cpu > 0.0 {
            let share = |r: &crate::store::MinuteRow| r.cpu / (r.instances.max(1) as f64 * tpl.run.cpu * 60.0) * 100.0;
            if rows.iter().all(|r| r.instances > 0 && share(r) >= a.cpu_pct as f64) {
                let value = rows.iter().map(share).fold(0.0, f64::max).round();
                firing.push(("alert.cpu", serde_json::json!({ "value": value, "threshold": a.cpu_pct, "cpu": tpl.run.cpu })));
            }
        }
        let (requests, errors): (i64, i64) = rows.iter().fold((0, 0), |(q, e), r| (q + r.requests, e + r.errors));
        if a.error_pct > 0 && requests >= 20 && errors * 100 >= a.error_pct as i64 * requests {
            firing.push(("alert.errors", serde_json::json!({ "value": (errors * 100 / requests), "threshold": a.error_pct, "requests": requests })));
        }
        // Saturation: requests that found every instance busy at max_instances.
        let over: i64 = rows.iter().map(|r| r.over_capacity).sum();
        if a.capacity_pct > 0 && requests >= 20 && over * 100 >= a.capacity_pct as i64 * requests {
            firing.push(("alert.capacity", serde_json::json!({ "value": (over * 100 / requests), "threshold": a.capacity_pct, "requests": requests, "max_instances": tpl.run.max_instances, "concurrency": tpl.run.concurrency })));
        }
        if a.p95_ms > 0 {
            let slow = rows.iter().filter(|r| r.requests > 0 && r.p95_ms >= a.p95_ms as f64).count();
            if slow >= 3 {
                let value = rows.iter().map(|r| r.p95_ms).fold(0.0, f64::max).round();
                firing.push(("alert.latency", serde_json::json!({ "value": value, "threshold": a.p95_ms })));
            }
        }
        // Its log-based metrics: the minute's matching lines, as gauges beside its own metrics.
        if !tpl.run.log_metrics.is_empty() {
            let key = format!("env:{}/{}", l.service, l.env);
            let since = code_sql::now_ms() - 60_000;
            let sink = self.logs.sink(crate::traces::metric_key(&l.service, &l.env), Stream::System);
            let at = code_sql::now_ms() as u64 * 1_000_000;
            for m in &tpl.run.log_metrics {
                let n = self.logs.count_recent(&key, &m.pattern, since);
                let p = crate::otlp::Point { name: m.name.clone(), kind: "gauge".into(), unit: String::new(), time: at, value: n as f64, count: None, counter: false, attrs: vec![("source".into(), "log".into())] };
                sink.line(serde_json::to_string(&p).expect("a point serializes"));
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
        if let Some(slo) = &tpl.run.slo
            && let Some((indicator, rate, window)) = crate::slo::burning(slo, &all, now)
        {
            firing.push(("alert.slo", serde_json::json!({ "indicator": indicator, "burn_rate": (rate * 10.0).round() / 10.0, "window": window, "objective": slo.objective })));
        }
        // The app's own gauges: each judgment that finds one past its threshold counts toward its
        // run of minutes; one that doesn't (or finds nothing) starts it again.
        let mut metric_firing: Vec<(String, serde_json::Value)> = Vec::new();
        for m in &a.metrics {
            let key = format!("{}/{}/{}/{}", l.service, l.env, m.name, m.sum);
            let value = self.gauge(&l.service, &l.env, &m.name, m.sum).await.ok();
            let run = {
                let mut streaks = self.metric_streaks.lock();
                let n = streaks.entry(key).or_default();
                *n = if value.is_some_and(|v| m.breached(v)) { n.saturating_add(1) } else { 0 };
                *n
            };
            if run >= m.minutes {
                let (op, threshold) = match m.above {
                    Some(t) => ("above", t),
                    None => ("below", m.below.unwrap_or_default()),
                };
                let mut data = serde_json::json!({ "metric": m.name, "value": value, "minutes": m.minutes });
                data[op] = serde_json::json!(threshold);
                metric_firing.push((m.name.clone(), data));
            }
        }
        for m in &a.metrics {
            let key = format!("{}/{}/alert.metric:{}", l.service, l.env, m.name);
            match metric_firing.iter().find(|(n, _)| *n == m.name) {
                Some((_, data)) => {
                    if self.looping.lock().insert(key) {
                        let (sid, e, dep, d) = (l.service.clone(), l.env.clone(), tpl.deployment.clone(), data.clone());
                        let _ = self.db(move |s| s.add_event("alert.metric", &sid, &e, Some(&dep), &d)).await;
                        l.log.stream(Stream::System).line(format!("alert: alert.metric {data}"));
                    }
                }
                None => {
                    if self.looping.lock().remove(&key) {
                        let (sid, e, dep, d) = (l.service.clone(), l.env.clone(), tpl.deployment.clone(), serde_json::json!({ "alert": "alert.metric", "metric": m.name }));
                        let _ = self.db(move |s| s.add_event("alert.resolved", &sid, &e, Some(&dep), &d)).await;
                        l.log.stream(Stream::System).line(format!("alert: alert.metric {} resolved", m.name));
                    }
                }
            }
        }
        for kind in ["alert.memory", "alert.cpu", "alert.errors", "alert.latency", "alert.capacity", "alert.log", "alert.slo"] {
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
                    // It was firing and isn't now: said once, to whoever heard the alert.
                    if self.looping.lock().remove(&key) {
                        let (sid, e, dep) = (l.service.clone(), l.env.clone(), tpl.deployment.clone());
                        let _ = self.db(move |s| s.add_event("alert.resolved", &sid, &e, Some(&dep), &serde_json::json!({ "alert": kind }))).await;
                        l.log.stream(Stream::System).line(format!("alert: {kind} resolved"));
                    }
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
            if n.is_multiple_of(crate::prom::EVERY_S) {
                self.scrape_pass();
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
                    self.note_new_errors(&l).await;
                }
            }
            // roll's GitHub App's events for the services here, every second.
            if self.github.get().is_some() && !self.github_running.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let this = self.clone();
                tokio::spawn(async move {
                    this.run_github_events().await;
                    this.github_running.store(false, std::sync::atomic::Ordering::SeqCst);
                });
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
                // A safety net: each watched environment has a long poll of its own; this starts
                // any that isn't running (a resumed environment) and checks once.
                tokio::spawn(async move {
                    this.watch_secrets().await;
                    this.watching_secrets.store(false, std::sync::atomic::Ordering::SeqCst);
                });
            }
            if n.is_multiple_of(300) && !self.watching.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let this = self.clone();
                tokio::spawn(async move {
                    this.watch_tags().await;
                    this.watch_git().await;
                    this.watching.store(false, std::sync::atomic::Ordering::SeqCst);
                });
            }
            if n.is_multiple_of(3600) && !self.watching_bases.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let this = self.clone();
                tokio::spawn(async move {
                    this.watch_bases().await;
                    this.watching_bases.store(false, std::sync::atomic::Ordering::SeqCst);
                });
            }
            if n.is_multiple_of(3600) && !self.backing_up.swap(true, std::sync::atomic::Ordering::SeqCst) {
                // A volume's tar can take minutes: never on the reaper's own loop.
                let this = self.clone();
                tokio::spawn(async move {
                    this.measure_volumes().await;
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
                    // Pending deploys, or the GitHub App's events waiting to run there.
                    let pending = self.db(move |s| Ok(!s.pending_deployments(&sid)?.is_empty() || s.has_github_events(&sid)?)).await.unwrap_or(false);
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

    /// What a commit's checks say: roll code's for a repo service, GitHub's (through roll's
    /// GitHub App) for a Git URL service connected to it. Anything else has none to read.
    pub(crate) async fn ci_of(self: &Arc<Self>, svc: &ServiceRow, sha: &str) -> anyhow::Result<crate::integrations::Ci> {
        match &svc.source {
            Source::Repo { repo, .. } => match self.deps.sources.clone() {
                Some(sources) => sources.ci(repo, sha).await,
                None => anyhow::bail!("roll code isn't on this node"),
            },
            Source::Git { url, .. } => {
                if let Some((app, installation, owner, repo)) = self.github_for(&svc.id, url).await {
                    return app.ci(installation, &owner, &repo, sha).await;
                }
                // GitLab's or Bitbucket's statuses, with its Git login's token.
                let remote = self.git_remote(&svc.id, url).await;
                match (crate::ci_hosts::host_of(url), remote.login()) {
                    (Some(host), Some(login)) => crate::ci_hosts::read(self.cfg.allow_private, &host, login, sha).await,
                    _ => anyhow::bail!("this Git URL service's checks can't be read (roll's GitHub App, or a GitLab or Bitbucket login)"),
                }
            }
            _ => Ok(crate::integrations::Ci::None),
        }
    }

    /// A Git URL push to an environment that waits for CI: it waits (its checks are read until
    /// they pass, fail or an hour goes by; GitHub's may not exist yet when the push arrives, so
    /// none is waited on for a minute too).
    pub(crate) async fn wait_for_git_ci(self: &Arc<Self>, svc: &ServiceRow, env: &str, sha: &str, message: &str, actor: &str) -> Result<()> {
        let w = crate::store::CiWait { service: svc.id.clone(), env: env.to_string(), landing: 0, sha: sha.to_string(), message: message.to_string(), author: Some(actor.to_string()) };
        self.db(move |s| s.wait_for_ci(&w)).await?;
        self.logs.sink(format!("env:{}/{env}", svc.id), Stream::System).line(format!("push {} waits for its checks", &sha[..sha.len().min(12)]));
        Ok(())
    }

    /// Pushes waiting for their checks: deployed once they pass (or there are none), dropped when
    /// one fails or after an hour, each said in its environment's log.
    pub async fn settle_ci_waits(self: &Arc<Self>) {
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
            let git = matches!(svc.source, Source::Git { .. });
            let log = self.logs.sink(format!("env:{}/{}", w.service, w.env), Stream::System);
            let sha = &w.sha[..w.sha.len().min(12)];
            let done = match self.ci_of(&svc, &w.sha).await {
                // GitHub's checks may not have started when the push came: a minute for them to.
                Ok(crate::integrations::Ci::None) if git && now - since < 60_000 => false,
                Ok(crate::integrations::Ci::Pending) if now - since > 3_600_000 => {
                    log.line(format!("push {sha} not deployed: its checks didn't finish within an hour"));
                    true
                }
                Ok(crate::integrations::Ci::Pending) | Err(_) => false,
                Ok(crate::integrations::Ci::Failed(which)) => {
                    log.line(format!("push {sha} not deployed: its check {which} failed"));
                    true
                }
                Ok(_) if self.refuse_paused(&w.service, &w.env).await.is_err() => {
                    log.line(format!("push {sha} not deployed: its checks passed while the environment is paused"));
                    true
                }
                Ok(_) => {
                    let hold = self.hold_for(&w.service, &w.env).await;
                    if let Held::Skip(why) = &hold {
                        log.line(format!("push {sha} not deployed: its checks passed, and {why}"));
                        let (sid, e, l) = (w.service.clone(), w.env.clone(), w.landing);
                        let _ = self.db(move |s| s.end_ci_wait(&sid, &e, l)).await;
                        continue;
                    }
                    log.line(format!("push {sha}'s checks passed: {}", if matches!(hold, Held::Ready) { "built and held for a person to put live" } else { "deploying" }));
                    let body = hold.body(serde_json::json!({ "revision": w.sha, "message": w.message }));
                    let by = By { trigger: Trigger::Push, actor: w.author.as_ref().map(|a| if git { a.clone() } else { format!("git:{a}") }) };
                    let key = if git { format!("git:{}:{}", w.env, w.sha) } else { format!("landing:{}", w.landing) };
                    self.deploy(&w.service, &w.env, body, by, None, Some(&key)).await.is_ok()
                }
            };
            if done {
                let (sid, e, l) = (w.service.clone(), w.env.clone(), w.landing);
                let _ = self.db(move |s| s.end_ci_wait(&sid, &e, l)).await;
            }
        }
    }

    /// `roll.toml` `[app]` at a pushed revision (in the service's root), if it reads.
    pub(crate) async fn app_config_at(self: &Arc<Self>, svc: &ServiceRow, revision: &str) -> Option<crate::config::AppConfig> {
        let root = match &svc.source {
            Source::Repo { root, .. } | Source::Git { root, .. } => root,
            _ => return None,
        };
        let path = match root {
            Some(r) => format!("{r}/roll.toml"),
            None => "roll.toml".into(),
        };
        let read = match (&svc.source, &self.deps.sources) {
            (Source::Repo { repo, .. }, Some(sources)) => sources.read_file(repo, revision, &path).await,
            (Source::Git { url, .. }, _) => crate::git_source::read_file(&self.git_remote(&svc.id, url).await, revision, &path).await,
            _ => return None,
        };
        match read {
            Ok(Some(bytes)) => crate::config::parse(&String::from_utf8_lossy(&bytes)).ok().flatten(),
            _ => None,
        }
    }

    /// Why a Git URL service's push at `sha` isn't deployed to `env`, if it isn't: a skip tag in
    /// its message, or watch paths (roll.toml's at that commit, the environment's block first,
    /// else the service's) it changes none of. The hook and the five-minute watch both ask, with
    /// roll.toml as read at `sha` (`app_config_at`, once per commit).
    pub(crate) async fn git_push_skip(self: &Arc<Self>, svc: &ServiceRow, env: &EnvRow, sha: &str, message: &str, file: Option<&crate::config::AppConfig>) -> Option<String> {
        if let Some(tag) = skip_tag(message) {
            return Some(format!("its message says {tag}"));
        }
        let watch = file.and_then(|f| f.watch_for(&env.name)).or_else(|| svc.build.watch.clone()).filter(|w| !w.is_empty())?;
        if self.push_touches(svc, env, sha, &watch).await {
            return None;
        }
        Some(format!("it changes nothing this service watches ({})", watch.join(", ")))
    }

    /// Whether `revision` changes a watched path since what `env` runs (no active deployment, or
    /// a source that can't tell: yes).
    pub(crate) async fn push_touches(self: &Arc<Self>, svc: &ServiceRow, env: &EnvRow, revision: &str, patterns: &[String]) -> bool {
        // A Git URL's: the paths between the live commit and this one, matched as a repo's are.
        if let (Source::Git { url, .. }, Some(sources)) = (&svc.source, &self.deps.sources) {
            let Some(active) = env.active.clone() else { return true };
            let Ok(Some(from)) = self.db(move |s| s.deployment(&active).map(|d| d.revision)).await else { return true };
            return match crate::git_source::changed_paths(&self.git_remote(&svc.id, url).await, &from, revision).await {
                Ok(Some(paths)) => sources.any_path_matches(patterns, &paths),
                Ok(None) => true,
                Err(e) => {
                    self.logs.sink(format!("env:{}/{}", svc.id, env.name), Stream::System).line(format!("{}'s changed paths couldn't be read ({e:#}): deployed as if it touched a watched one", &revision[..revision.len().min(12)]));
                    true
                }
            };
        }
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
                if e.paused {
                    self.logs.sink(format!("env:{}/{}", svc.id, e.name), Stream::System).line(format!("push {} not deployed: the environment is paused", &l.revision.sha[..l.revision.sha.len().min(12)]));
                    continue;
                }
                let hold = hold_of(svc, e);
                if let Held::Skip(why) = &hold {
                    self.logs.sink(format!("env:{}/{}", svc.id, e.name), Stream::System).line(format!("push {} not deployed: {why}", &l.revision.sha[..l.revision.sha.len().min(12)]));
                    continue;
                }
                // `[skip deploy]` or `[skip ci]` in its message (Render's `[skip render]`): a push
                // that says so isn't deployed, which the log and the timeline say.
                if let Some(tag) = skip_tag(&l.revision.message) {
                    self.logs.sink(format!("env:{}/{}", svc.id, e.name), Stream::System).line(format!("push {} not deployed: its message says {tag}", &l.revision.sha[..l.revision.sha.len().min(12)]));
                    let (sid, en, data) = (svc.id.clone(), e.name.clone(), serde_json::json!({ "revision": l.revision.sha, "tag": tag }));
                    let _ = self.db(move |s| s.add_event("deploy.skipped", &sid, &en, None, &data)).await;
                    continue;
                }
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
                let body = hold.body(serde_json::json!({ "revision": l.revision.sha, "message": l.revision.message.lines().next().unwrap_or("") }));
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
    /// Closed environments whose instances trust an interception CA roll secrets has moved on from
    /// (near its end, or ended early: a binding narrowed, a placeholder revoked): their proxies
    /// take the new one at once; running instances are redeployed (the live image again, rolled)
    /// to trust it, and a sleeping environment's next start does (its template's bundle swapped),
    /// with no redeploy. Each minute, on the owner.
    async fn refresh_intercepts(self: &Arc<Self>, services: &[crate::store::ServiceRow]) {
        let kept: Vec<(String, String)> = self.intercept_cas.lock().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (key, trusted) in kept {
            let Some((sid, env)) = key.split_once('/').map(|(s, e)| (s.to_string(), e.to_string())) else { continue };
            let Some(svc) = services.iter().find(|s| s.id == sid).filter(|s| self.owns(&s.id)) else { continue };
            let network = crate::closed_network_name(&svc.project, &env);
            let log = self.logs.sink(format!("env:{sid}/{env}"), Stream::System);
            match self.deps.secrets.intercept(&svc.project, &env).await {
                Ok(Some(i)) if ca_digest(&i.ca_pem) != trusted => {
                    self.deps.runtime.set_interception(&network, Some(i.interception.clone()));
                    let live = self.live(&sid, &env);
                    if live.awake() > 0 {
                        match self.redeploy(&sid, &env, json!({ "message": "roll's interception CA renewed" }), Some("roll".into()), None).await {
                            Ok(d) => log.line(format!("the interception CA changed (near its end, or a key's binding moved): redeploying ({}) to trust the next", d.id)),
                            Err(e) => log.line(format!("the interception CA changed, and the redeploy that hands the instances the next didn't start: {e}")),
                        }
                    } else {
                        live.set_file(INTERCEPT_CA, &trust_bundle(&i.ca_pem));
                        self.intercept_cas.lock().insert(key.clone(), ca_digest(&i.ca_pem));
                        log.line("the interception CA changed while it slept: its next start trusts the new one");
                    }
                }
                Ok(None) => {
                    self.deps.runtime.set_interception(&network, None);
                    self.intercept_cas.lock().remove(&key);
                    log.line("no key of its is bound to hosts now: its proxies tunnel everything again");
                }
                _ => {}
            }
        }
    }

    /// Owned environments' stored logs past their retention (`log_retention_days`, a week by
    /// default and always for a preview) deleted, chunk by chunk.
    /// Logs kept past the first week (30 or 90 days) are metered too: this hour's GB-hour of
    /// what's stored past it now (`stored_gb_hours`), once an hour however many passes run.
    async fn expire_logs_pass(self: &Arc<Self>, services: &[crate::store::ServiceRow], now: i64) {
        let Ok(envs) = self.db(|s| s.all_environments()).await else { return };
        for svc in services.iter().filter(|s| self.owns(&s.id)) {
            for e in envs.iter().filter(|e| e.service == svc.id) {
                let days = if e.ephemeral { crate::logs::KEEP_DAYS } else { e.log_retention_days.unwrap_or(crate::logs::KEEP_DAYS) };
                let before = now - days as i64 * 86_400_000;
                let week = now - crate::logs::KEEP_DAYS as i64 * 86_400_000;
                let mut past_week = 0u64;
                for key in crate::logs::env_keys(&svc.id, &e.name) {
                    self.logs.expire(&key, before).await;
                    if days > crate::logs::KEEP_DAYS {
                        past_week += self.logs.stored_before(&key, week).await;
                    }
                }
                if past_week > 0 {
                    let (sid, en, gb) = (svc.id.clone(), e.name.clone(), past_week as f64 / 1e9);
                    let _ = self.db(move |s| s.meter_stored(&sid, &en, "logs", now / 3_600_000, gb)).await;
                }
                // Its jobs' logs, each whole once its job started before then: those started
                // since the last pass's cutoff (kept, so a node that was down catches up).
                let mark = format!("job-logs-expired:{}/{}", svc.id, e.name);
                let (sid, en, m) = (svc.id.clone(), e.name.clone(), mark.clone());
                let from = self.db(move |s| s.get_state(&m)).await.ok().flatten().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
                if let Ok(ids) = self.db(move |s| s.jobs_started(&sid, &en, from, before)).await {
                    for id in ids {
                        self.logs.delete(&format!("job:{id}")).await;
                    }
                    let v = before.to_string();
                    let _ = self.db(move |s| s.set_state(&mark, &v)).await;
                }
            }
        }
    }

    pub async fn cron_pass(self: &Arc<Self>, now: i64) {
        let Ok(services) = self.db(|s| s.services(None)).await else { return };
        self.schedules_pass(&services, now).await;
        self.push_pass(&services).await;
        self.ecr_refresh_pass(&services).await;
        self.checks_pass(&services, now).await;
        self.heartbeats_pass(&services, now).await;
        self.rescan_pass(&services, now).await;
        self.pause_pass(&services, now).await;
        if now - self.logs_expired.load(std::sync::atomic::Ordering::Relaxed) >= 3_600_000 {
            self.logs_expired.store(now, std::sync::atomic::Ordering::Relaxed);
            self.expire_logs_pass(&services, now).await;
            self.build_cache_pass(&services, now).await;
        }
        if now - self.intercepts_checked.load(std::sync::atomic::Ordering::Relaxed) >= 60_000 {
            self.intercepts_checked.store(now, std::sync::atomic::Ordering::Relaxed);
            self.refresh_intercepts(&services).await;
        }
        if now - self.status_swept.load(std::sync::atomic::Ordering::Relaxed) >= 60_000 {
            self.status_swept.store(now, std::sync::atomic::Ordering::Relaxed);
            self.status_sweep(&services).await;
        }
        for svc in services.into_iter().filter(|s| s.run.kind == RunType::Cron && self.owns(&s.id) && !self.capped(&s.id)) {
            let Some(schedule) = svc.run.schedule.as_deref().and_then(|s| crate::cron::Schedule::parse(s, svc.run.timezone.as_deref()).ok()) else { continue };
            let sid = svc.id.clone();
            let Ok(envs) = self.db(move |s| s.environments(&sid)).await else { continue };
            for e in envs.into_iter().filter(|e| e.active.is_some()) {
                let key = format!("cron:{}/{}", svc.id, e.name);
                // Paused: skipped, and its schedule moves on, so resuming doesn't make up the runs.
                if e.paused {
                    let (k, v) = (key.clone(), now.to_string());
                    let _ = self.db(move |s| s.set_state(&k, &v)).await;
                    continue;
                }
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
                let due = latest_due(&schedule, due, now);
                let (k, v) = (key.clone(), due.to_string());
                let _ = self.db(move |s| s.set_state(&k, &v)).await;
                let (this, sid, env) = (self.clone(), svc.id.clone(), e.name.clone());
                tokio::spawn(async move {
                    let _ = this.run_cron(&sid, &env).await;
                });
            }
        }
    }

    /// Any owned service's `run.schedules` (the environment's own run first): each due one
    /// starts a job, unless the last it started still runs; a paused environment's move on.
    async fn schedules_pass(self: &Arc<Self>, services: &[crate::store::ServiceRow], now: i64) {
        // One read of every environment a pass, not one per service.
        let Ok(all_envs) = self.db(|s| s.all_environments()).await else { return };
        for svc in services.iter().filter(|s| self.owns(&s.id) && !self.capped(&s.id)) {
            // A preview environment never runs production's schedules (one per open pull request).
            // In maintenance, nothing is called on a schedule (it would be answered roll's page).
            for e in all_envs.iter().filter(|e| e.service == svc.id && e.active.is_some() && !e.ephemeral && e.maintenance.is_none()) {
                // What the live deployment runs with: roll.toml's `[[app.run.schedules]]` are in
                // its template, not in the service's settings.
                let (run, secret) = match self.lives.lock().get(&crate::env_key(&svc.id, &e.name)).and_then(|l| l.template()) {
                    Some(t) => (t.run.clone(), t.spec.vars.iter().find(|(k, _)| k == "CRON_SECRET").map(|(_, v)| v.clone())),
                    None => (svc.for_env(e).map(|s| s.run).unwrap_or_else(|_| svc.run.clone()), None),
                };
                for sc in &run.schedules {
                    let Ok(schedule) = crate::cron::Schedule::parse(&sc.schedule, sc.timezone.as_deref()) else { continue };
                    let key = format!("sched:{}/{}/{}", svc.id, e.name, sc.name);
                    let k = key.clone();
                    let last: Option<i64> = self.db(move |s| s.get_state(&k)).await.ok().flatten().and_then(|v| v.parse().ok());
                    // New (or paused): counted from now, so nothing is made up.
                    let Some(last) = last.filter(|_| !e.paused) else {
                        let (k, v) = (key.clone(), now.to_string());
                        let _ = self.db(move |s| s.set_state(&k, &v)).await;
                        continue;
                    };
                    let Some(due) = schedule.next_after(last).filter(|d| *d <= now) else { continue };
                    // The latest due time that has passed: after an outage one run, not every one missed.
                    let due = latest_due(&schedule, due, now);
                    let (k, v) = (key.clone(), due.to_string());
                    let _ = self.db(move |s| s.set_state(&k, &v)).await;
                    let log = self.logs.sink(format!("env:{}/{}", svc.id, e.name), Stream::System);
                    // A path: called through the environment's own proxy, skipped while the last
                    // call is still out; a failure is `cron.failed`, as a command's is.
                    if let Some(path) = sc.path.clone() {
                        if !self.calling.lock().insert(key.clone()) {
                            log.line(format!("scheduled {}: skipped, its last call to {path} is still out", sc.name));
                            continue;
                        }
                        let aud = self.url(&e.slug);
                        let host = url::Url::parse(&aud).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
                        let (this, sid, en, name, secret, project, oidc) = (self.clone(), svc.id.clone(), e.name.clone(), sc.name.clone(), secret.clone(), svc.project.clone(), sc.auth.is_some());
                        let timeout = std::time::Duration::from_secs(sc.timeout_s.unwrap_or(300) as u64);
                        tokio::spawn(async move {
                            let began = std::time::Instant::now();
                            // `oidc`: roll's token for the service instead of CRON_SECRET.
                            let r = match oidc {
                                true => match this.service_token(&project, &sid, &en, &aud).await {
                                    Ok(Some((jwt, _))) => crate::public::call_scheduled(this.clone(), &sid, &en, &host, &path, Some(&jwt), timeout).await,
                                    Ok(None) => Err("roll issues no token for this environment".to_string()),
                                    Err(e) => Err(format!("its token: {e:#}")),
                                },
                                false => crate::public::call_scheduled(this.clone(), &sid, &en, &host, &path, secret.as_deref(), timeout).await,
                            };
                            this.calling.lock().remove(&key);
                            let ms = began.elapsed().as_millis() as u64;
                            let call = match &r {
                                Ok(s) => crate::model::ScheduleCall { at: code_sql::now_ms(), status: Some(*s), ms, error: None },
                                Err(e) => crate::model::ScheduleCall { at: code_sql::now_ms(), status: None, ms, error: Some(e.clone()) },
                            };
                            let log = this.logs.sink(format!("env:{sid}/{en}"), Stream::System);
                            match &r {
                                Ok(s) => log.line(format!("scheduled {name}: GET {path} answered {s} in {ms} ms")),
                                Err(e) => log.line(format!("scheduled {name}: GET {path}: {e}")),
                            }
                            let failed = !matches!(r, Ok(s) if (200..400).contains(&s));
                            let (k, v) = (format!("sched-call:{sid}/{en}/{name}"), serde_json::to_string(&call).expect("a call serializes"));
                            let data = serde_json::json!({ "schedule": name, "path": path, "http_status": call.status, "error": call.error });
                            let _ = this
                                .db(move |s| {
                                    // The event first: whoever sees the call's outcome sees it said.
                                    if failed {
                                        s.add_event("cron.failed", &sid, &en, None, &data)?;
                                    }
                                    s.set_state(&k, &v)
                                })
                                .await;
                        });
                        continue;
                    }
                    // Skipped while the last one it started still runs.
                    let jk = format!("sched-job:{}/{}/{}", svc.id, e.name, sc.name);
                    let j = jk.clone();
                    if let Ok(Some(prev)) = self.db(move |s| s.get_state(&j)).await {
                        let (sid, en) = (svc.id.clone(), e.name.clone());
                        if self.db(move |s| s.job(&sid, &en, &prev)).await.is_ok_and(|j| j.status == RunStatus::Running) {
                            log.line(format!("scheduled {}: skipped, its last job still runs", sc.name));
                            continue;
                        }
                    }
                    match self.start_job(&svc.id, &e.name, serde_json::json!({ "command": sc.command, "timeout_s": sc.timeout_s, "cpu": sc.cpu, "memory_mb": sc.memory_mb })).await {
                        Ok(job) => {
                            log.line(format!("scheduled {}: job {} started", sc.name, job.id));
                            let id = job.id.clone();
                            let _ = self.db(move |s| s.set_state(&jk, &id)).await;
                        }
                        Err(err) => log.line(format!("scheduled {}: didn't start: {err}", sc.name)),
                    }
                }
            }
        }
    }

    /// One run of a cron service now (`POST …/runs`, or the schedule).
    pub async fn run_cron(self: &Arc<Self>, service: &str, env: &str) -> Result<CronRun> {
        self.run_cron_try(service, env, 1).await
    }

    /// One try of a run: a failed or timed-out one (not a canceled one) is tried again after a
    /// backoff while `run.retries` allows, its `cron.failed` only when no try follows.
    fn run_cron_try<'a>(self: &'a Arc<Self>, service: &'a str, env: &'a str, attempt: u32) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<CronRun>> + Send + 'a>> {
        Box::pin(self.run_cron_inner(service, env, attempt))
    }

    async fn run_cron_inner(self: &Arc<Self>, service: &str, env: &str, attempt: u32) -> Result<CronRun> {
        let (sid, e) = (service.to_string(), env.to_string());
        let (svc, row) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await?;
        if svc.run.kind != RunType::Cron {
            return Err(Error::Conflict("only a cron service has runs".into()));
        }
        let Some(active) = row.active.clone() else { return Err(Error::Conflict("this environment has no deployment yet".into())) };
        if row.paused {
            return Err(Error::Conflict("this environment is paused: resume it to run".into()));
        }
        let timeout = Duration::from_secs(svc.run.timeout_s.unwrap_or(3600) as u64);
        let (sid, e, a, t) = (service.to_string(), env.to_string(), active.clone(), timeout.as_millis() as i64 + 60_000);
        // Known here as running before its row says so: a takeover's cleanup never fails it.
        let rid = crate::model::new_id("run");
        self.running_runs.lock().insert(rid.clone(), (service.to_string(), env.to_string()));
        // Its cancel works from the moment its row says running (a cancel before it starts stops
        // it at once rather than marking a run that then goes ahead).
        let cancel = CancellationToken::new();
        self.cancels.lock().insert(rid.clone(), cancel.clone());
        let r2 = rid.clone();
        let run = match self.db(move |s| s.start_run(&r2, &sid, &e, &a, t, attempt)).await {
            Ok(r) if r.status != RunStatus::Skipped => r,
            other => {
                self.running_runs.lock().remove(&rid);
                self.cancels.lock().remove(&rid);
                return other;
            }
        };
        let live = match self.load_live(service, env).await {
            Ok(l) => l,
            Err(err) => {
                let r2 = rid.clone();
                let _ = self.db(move |s| s.finish_run(&r2, RunStatus::Failed, None, false)).await;
                self.running_runs.lock().remove(&rid);
                self.cancels.lock().remove(&rid);
                return Err(err);
            }
        };
        let (this, id, sid, run_dep) = (self.clone(), run.id.clone(), service.to_string(), run.deployment.clone());
        let retries = svc.run.retries.unwrap_or(0) as u32;
        tokio::spawn(async move {
            // Its output stays in the environment's log (drains read it there); its container's
            // name, its own id in it, is what picks its own lines out.
            let name = crate::live::run_name(&sid, &live.env, &run_dep, &id);
            let (status, code) = match live.run_to_exit(None, &[], timeout, None, cancel, Some(name), None).await {
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
            // Tried again while retries allow (never a canceled one): only the last try's failure
            // is an incident.
            let retry = matches!(status, RunStatus::Failed | RunStatus::TimedOut) && attempt <= retries;
            // A failed or timed-out run is an incident's start: its event (notified by default)
            // in the same transaction, and only if it was still running (a takeover may have
            // failed it already).
            let id2 = id.clone();
            let still = this.db(move |s| s.finish_run(&id2, status, code, retry)).await.unwrap_or(false);
            this.running_runs.lock().remove(&id);
            this.changed(&sid);
            if retry && still {
                let wait = this.cfg.cron_retry_after.saturating_mul(1 << (attempt - 1).min(10)).min(Duration::from_secs(600));
                live.log.stream(Stream::System).line(format!("the run {}: trying again in {} s (try {} of {})", status.as_str().replace('_', " "), wait.as_secs().max(1), attempt + 1, retries + 1));
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = this.stop.cancelled() => return,
                }
                // Still this node's, still a cron service, not paused: run_cron_try says no otherwise.
                if this.owns(&sid) {
                    let env = live.env.clone();
                    if let Err(e) = this.run_cron_try(&sid, &env, attempt + 1).await {
                        live.log.stream(Stream::System).line(format!("the retry didn't start: {e}"));
                    }
                }
            }
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
                self.db(move |s| s.finish_run(&i, RunStatus::Canceled, None, false)).await?;
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
fn judge(new: crate::live::Served, old: crate::live::Served, error_pct: u8, slo: Option<(&crate::model::Slo, f64)>, p95_pct: Option<u32>) -> Option<String> {
    // Slower than the old one by more than it allows (Flagger's request duration), each side
    // with enough answers timed.
    if let (Some(pct), Some(n), Some(o)) = (p95_pct, new.p95_ms(), old.p95_ms())
        && new.requests >= crate::model::Rollout::MIN_REQUESTS
        && old.requests >= crate::model::Rollout::MIN_REQUESTS
        && n > o * (1.0 + pct as f64 / 100.0)
    {
        return Some(format!("its p95 was about {n:.0} ms, the old deployment's {o:.0} ms (the step allows {pct}% worse)"));
    }
    // Crashes no request saw (a failed health check, a process that exits).
    if new.crashes >= 2 && new.crashes > old.crashes {
        return Some(format!("its instances crashed {} times (the old deployment's {})", new.crashes, old.crashes));
    }
    // The objective's budget, spent too fast by the new one (and faster than the old one).
    if let Some((s, max)) = slo
        && new.requests >= crate::model::Rollout::MIN_REQUESTS
        && new.error_pct() > old.error_pct()
    {
        let burn = (new.error_pct() / 100.0) / (1.0 - s.objective / 100.0);
        if burn >= max {
            return Some(format!("it spent the error budget of the {}% objective {burn:.1}× as fast as it lasts ({} of its {} requests were 5xx; the step allows {max}×)", s.objective, new.errors, new.requests));
        }
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


/// How long a build may take: the service's `build.timeout_s`, held to the node's own limit.
fn build_timeout(own: Option<u32>, node: Duration, log: &LogSink) -> Duration {
    match own.map(|s| Duration::from_secs(s.into())) {
        Some(t) if t > node => {
            log.line(format!("build.timeout_s is {} minutes; this node builds for at most {}", t.as_secs() / 60, node.as_secs() / 60));
            node
        }
        Some(t) => t,
        None => node,
    }
}

/// A closed environment's proxy variables (`proxy` for its listed hosts): `HTTP_PROXY` and
/// `HTTPS_PROXY` in both cases, and `NO_PROXY` naming what's reached directly: the host, its
/// links' relays and the project's services (by name and `<name>.<env>.internal`).
/// Where a closed environment's instances find the interception CA, with the host's roots.
pub const INTERCEPT_CA: &str = "/etc/roll/intercept-ca.pem";

/// A CA's PEM, as which one instances trust is kept.
fn ca_digest(pem: &str) -> String {
    use sha2::Digest;
    hex::encode(&sha2::Sha256::digest(pem.as_bytes())[..12])
}

/// The host's roots (the instance's own may be older or missing) and the interception CA.
fn trust_bundle(ca_pem: &str) -> String {
    let roots = ["/etc/ssl/certs/ca-certificates.crt", "/etc/pki/tls/certs/ca-bundle.crt", "/etc/ssl/cert.pem"].iter().find_map(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    format!("{}\n{}\n", roots.trim_end(), ca_pem.trim())
}

fn egress_vars(proxy: &str, services: &[String], vars: &[(String, String)]) -> Vec<(String, String)> {
    let mut direct: Vec<String> = ["localhost", "127.0.0.1", "::1", "host.docker.internal", ".internal"].map(str::to_string).to_vec();
    direct.extend(services.iter().cloned());
    direct.extend(crate::runtime::links::ports(vars).into_iter().map(crate::runtime::links::relay_host));
    let direct = direct.join(",");
    let proxies = ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"].map(|k| (k.to_string(), proxy.to_string()));
    proxies.into_iter().chain(["NO_PROXY", "no_proxy"].map(|k| (k.to_string(), direct.clone()))).collect()
}

/// A manual rollout's wait: what a person said, and its wake.
pub(crate) type RolloutGate = Arc<(parking_lot::Mutex<Option<crate::model::RolloutAction>>, tokio::sync::Notify)>;

/// What a person's hold (`PUT … {held: true}`) is kept as, where a rollback keeps its own id.
pub const MANUAL_HOLD: &str = "manual";

/// An environment's deploys as automation finds them: going live, built to `ready` for a person
/// to put live (held by a person, a web service without a volume, which runs beside the live
/// one), or not made (held after a rollback, or held where nothing can wait beside the live one).
pub(crate) enum Held {
    No,
    Ready,
    Skip(String),
}

pub(crate) fn hold_of(svc: &crate::store::ServiceRow, e: &crate::store::EnvRow) -> Held {
    // A freeze holds as a person's hold does, saying why.
    if let Some(f) = e.freeze.as_ref().filter(|f| f.on(code_sql::now_ms())) {
        return match svc.run.kind == RunType::Web && svc.run.volume.is_none() {
            true => Held::Ready,
            false => Held::Skip(format!("{}, and this service's can't wait beside the live one", f.says())),
        };
    }
    // Approvals: what automation builds waits at ready for them.
    if e.approvals > 0 {
        return match svc.run.kind == RunType::Web && svc.run.volume.is_none() {
            true => Held::Ready,
            false => Held::Skip(format!("deploys need {} approval(s), and this service's can't wait beside the live one for them", e.approvals)),
        };
    }
    match e.held_by.as_deref() {
        None => Held::No,
        Some(MANUAL_HOLD) if svc.run.kind == RunType::Web && svc.run.volume.is_none() => Held::Ready,
        Some(MANUAL_HOLD) => Held::Skip("deploys are held, and this service's can't wait beside the live one: deploy it by hand".into()),
        Some(r) => Held::Skip(format!("deploys are held after the rollback {r}")),
    }
}

impl Held {
    /// The deploy's body as the hold has it: `no_traffic` when it waits for a person.
    pub(crate) fn body(&self, mut body: serde_json::Value) -> serde_json::Value {
        if matches!(self, Held::Ready) {
            body["no_traffic"] = serde_json::json!(true);
        }
        body
    }
}

#[cfg(test)]
mod judge_tests {
    use super::judge;
    use crate::live::Served;

    #[test]
    fn a_step_that_spends_the_objectives_budget_too_fast_fails() {
        let served = |requests: u64, errors: u64| Served { requests, errors, ..Default::default() };
        let slo = crate::model::Slo { objective: 99.0, latency_ms: None, window_days: 28 };
        // 3% of 100 is under the 5% rule, but 3× the 99% objective's budget: past a 2× step.
        assert_eq!(judge(served(100, 3), served(100, 0), 5, None, None), None);
        assert!(judge(served(100, 3), served(100, 0), 5, Some((&slo, 2.0)), None).unwrap().contains("3.0× as fast"));
        assert_eq!(judge(served(100, 3), served(100, 0), 5, Some((&slo, 14.4)), None), None, "under the default burn");
        assert_eq!(judge(served(100, 3), served(100, 5), 5, Some((&slo, 2.0)), None), None, "the old one is worse");
        assert_eq!(judge(served(10, 3), served(100, 0), 5, Some((&slo, 2.0)), None), None, "too few requests to say");
    }

    #[test]
    fn a_step_much_slower_than_the_old_deployment_fails() {
        let timed = |ms: f64| {
            let mut s = Served { requests: 100, ..Default::default() };
            for _ in 0..100 {
                s.took(ms);
            }
            s
        };
        assert!(judge(timed(400.0), timed(100.0), 5, None, Some(50)).unwrap().contains("p95"));
        assert_eq!(judge(timed(120.0), timed(100.0), 5, None, Some(50)), None, "within 50%");
        assert_eq!(judge(timed(400.0), timed(100.0), 5, None, None), None, "off unless asked");
    }
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


/// What `…/pending` says changed, as a line (`API_KEY added, LOG_LEVEL changed, run.memory_mb,
/// the project's secrets`); none when it names nothing. At most 300 characters.
pub(crate) fn changes_said(p: &Value) -> Option<String> {
    let names = |k: &str| p["variables"][k].as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect::<Vec<_>>()).unwrap_or_default();
    let mut parts = Vec::new();
    for (k, word) in [("added", "added"), ("changed", "changed"), ("removed", "removed")] {
        let n = names(k);
        if !n.is_empty() {
            parts.push(format!("{} {word}", n.join(", ")));
        }
    }
    let run: Vec<String> = p["run"].as_array().into_iter().flatten().filter_map(Value::as_str).map(|k| format!("run.{k}")).collect();
    if !run.is_empty() {
        parts.push(run.join(", "));
    }
    if p["secrets"] == true {
        parts.push("the project's secrets".into());
    }
    let said = parts.join("; ");
    (!said.is_empty()).then(|| said.chars().take(300).collect())
}

/// The last due time at or before `now`, from one that is (`due`): what a pass records, so the
/// runs an outage missed aren't made up one by one.
fn latest_due(schedule: &crate::cron::Schedule, mut due: i64, now: i64) -> i64 {
    while let Some(next) = schedule.next_after(due).filter(|n| *n <= now) {
        due = next;
    }
    due
}

/// What in a commit's message skips its deploy: `[skip deploy]`, `[skip ci]`, `[skip roll]` and
/// each the other way round (`[ci skip]`), in any case, anywhere in it.
pub(crate) fn skip_tag(message: &str) -> Option<&'static str> {
    const TAGS: [&str; 6] = ["[skip deploy]", "[deploy skip]", "[skip ci]", "[ci skip]", "[skip roll]", "[roll skip]"];
    let m = message.to_ascii_lowercase();
    TAGS.into_iter().find(|t| m.contains(t))
}

#[cfg(test)]
mod skip_tests {
    #[test]
    fn an_outage_makes_up_one_run_not_every_one() {
        let s = crate::cron::Schedule::parse("*/5 * * * *", None).unwrap();
        let t0 = 1_767_225_600_000; // 2026-01-01 00:00 UTC
        let first = s.next_after(t0).unwrap();
        // A day down: the latest due time before now, not the first one missed.
        let now = t0 + 86_400_000 + 60_000;
        assert_eq!(super::latest_due(&s, first, now), t0 + 86_400_000);
    }

    #[test]
    fn a_message_can_skip_its_deploy() {
        assert_eq!(super::skip_tag("docs: typo\n\n[Skip CI]"), Some("[skip ci]"));
        assert_eq!(super::skip_tag("readme [deploy skip]"), Some("[deploy skip]"));
        assert_eq!(super::skip_tag("skip the cache"), None);
    }
}

/// What a build's image depends on: every file of its context (path, kind and bytes, in order),
/// its Dockerfile, target, build variables and builder. Two builds with the same key make the
/// same image, so the second reuses the first's. roll's names for the service itself
/// (`ROLL_SERVICE`, `ROLL_SERVICE_ID`, `ROLL_PRIVATE_HOST`, `ROLL_PUBLIC_URL`) are left out, so a
/// web service and a worker of one repository share a build; the environment's name
/// (`ROLL_ENVIRONMENT`) is in, so a preview's build is never production's.
async fn build_key(job: &BuildJob, dockerfile: &str, builder: &str) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    let context = job.context.clone();
    let mut h = tokio::task::spawn_blocking(move || -> anyhow::Result<Sha256> {
        let mut h = Sha256::new();
        let mut stack = vec![context.clone()];
        let mut files = Vec::new();
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir)? {
                let e = e?;
                let (p, t) = (e.path(), e.file_type()?);
                if t.is_dir() {
                    stack.push(p);
                } else {
                    files.push((p, t.is_symlink()));
                }
            }
        }
        files.sort();
        for (p, link) in files {
            h.update(p.strip_prefix(&context).unwrap_or(&p).to_string_lossy().as_bytes());
            h.update([0, link as u8]);
            if link {
                h.update(std::fs::read_link(&p)?.to_string_lossy().as_bytes());
            } else {
                let mut f = std::fs::File::open(&p)?;
                std::io::copy(&mut f, &mut h)?;
            }
            h.update([0]);
        }
        Ok(h)
    })
    .await??;
    for part in [dockerfile, job.target.as_deref().unwrap_or(""), builder, if job.own_dockerfile { "own" } else { "generated" }] {
        h.update(part.as_bytes());
        h.update([0]);
    }
    let mut vars: Vec<&(String, String)> = job.env.iter().filter(|(k, _)| !["ROLL_SERVICE", "ROLL_SERVICE_ID", "ROLL_PRIVATE_HOST", "ROLL_PUBLIC_URL"].contains(&k.as_str())).collect();
    vars.sort();
    for (k, v) in vars {
        h.update(k.as_bytes());
        h.update([0]);
        h.update(v.as_bytes());
        h.update([0]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}
