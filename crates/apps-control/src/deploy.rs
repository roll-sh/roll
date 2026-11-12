//! Deploys (docs/apps/README.md, "Deploys"): one queue per service and environment on the
//! service's owner. A deployment is built (or its image pulled), rolled out next to the old one,
//! health-checked, and switched to with a compare-and-swap; the old instances drain.
//!
//! Also the owner's background loops: the reaper and meter, deploy-on-push from roll code's
//! land log, cron schedules, and custom domains' DNS checks.

use crate::live::Template;
use crate::logs::{LogSink, Stream};
use crate::model::*;
use crate::runtime::{BuildJob, InstanceSpec};
use crate::store::{EnvRow, NewDeployment, ServiceRow};
use crate::{Apps, Error, Result, fingerprint, lease_key, network_name, volume_name};
use bytes::Bytes;
use serde_json::Value;
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
    pub async fn deploy(self: &Arc<Self>, service: &str, env: &str, body: Value, trigger: Trigger, upload: Option<Bytes>, key: Option<&str>) -> Result<Deployment> {
        if let Some(k) = key {
            if k.is_empty() || k.len() > 200 || !k.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(Error::BadRequest("Idempotency-Key is 1–200 printable ASCII characters".into()));
            }
            let (sid, e, k) = (service.to_string(), env.to_string(), k.to_string());
            if let Some(d) = self.db(move |s| s.keyed_deployment(&sid, &e, &k)).await? {
                return Ok(d);
            }
        }
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
        let (revision, image, trigger) = match (&svc.source, upload.is_some(), image) {
            (_, true, _) => (None, None, Trigger::Upload),
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
        let d = NewDeployment { trigger, revision, message, image, plan: None, rollback_of: None, run: svc.run.clone(), upload: upload_key, status: DeployStatus::Queued };
        let (sid, e, k) = (service.to_string(), env.to_string(), key.map(str::to_string));
        let ours = d.upload.clone().filter(|_| key.is_some());
        let dep = self.db(move |s| s.create_deployment(&sid, &e, d, "", k.as_deref())).await?;
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
    pub async fn redeploy(self: &Arc<Self>, service: &str, env: &str, body: Value) -> Result<Deployment> {
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
        let mut run = svc.run.clone();
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
        };
        let (sid, e) = (service.to_string(), env.to_string());
        let dep = self.db(move |s| s.create_deployment(&sid, &e, d, "", None)).await?;
        self.deploy_log(&dep.id).line("queued (redeploy)");
        self.kick(service, env);
        Ok(dep)
    }

    /// `POST …/deploys/{dep}/rollback`: deploy an earlier deployment's image and settings
    /// again, without a build.
    pub async fn rollback(self: &Arc<Self>, service: &str, env: &str, dep: &str) -> Result<Deployment> {
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
        let d = self.db(move |s| s.deployment(&id)).await?;
        if d.service != service || d.environment != env {
            return Err(Error::NotFound(format!("no deployment {dep} of {service} in {env}")));
        }
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

    async fn run(self: &Arc<Self>, d: Deployment) {
        let cancel = CancellationToken::new();
        self.cancels.lock().insert(d.id.clone(), cancel.clone());
        let log = self.deploy_log(&d.id);
        let began = Instant::now();
        let result = self.run_inner(&d, &log, cancel.clone()).await;
        self.cancels.lock().remove(&d.id);
        let id = d.id.clone();
        match result {
            Ok(()) => {
                log.stream(Stream::System).line(format!("deployed in {:.1} s", began.elapsed().as_secs_f64()));
                info!(service = %d.service, env = %d.environment, deployment = %d.id, "apps: deployed");
            }
            Err(e) if cancel.is_cancelled() => {
                let _ = e;
            }
            Err(e) => {
                let msg = e.to_string();
                log.stream(Stream::System).line(format!("failed: {msg}"));
                let _ = self.db(move |s| s.set_status(&id, &[DeployStatus::Queued, DeployStatus::Building, DeployStatus::Deploying], DeployStatus::Failed, Some(&msg))).await;
                warn!(service = %d.service, env = %d.environment, deployment = %d.id, error = %e, "apps: a deploy failed");
            }
        }
        self.logs.flush().await;
    }

    async fn run_inner(self: &Arc<Self>, d: &Deployment, log: &LogSink, cancel: CancellationToken) -> anyhow::Result<()> {
        let (sid, e) = (d.service.clone(), d.environment.clone());
        let (svc, env) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await?;
        let id = d.id.clone();
        let (image, run) = match &d.image {
            // A rollback or a redeploy: an image roll already has, nothing to build or pull.
            Some(img) if matches!(d.trigger, Trigger::Rollback | Trigger::Settings) => {
                self.db(move |s| s.set_status(&id, &[DeployStatus::Queued, DeployStatus::Building], DeployStatus::Deploying, None)).await?;
                log.line(format!("using the image of {}", d.rollback_of.as_deref().unwrap_or("the deployment it redeploys")));
                (img.clone(), d.run.clone())
            }
            Some(img) => {
                self.db(move |s| s.set_status(&id, &[DeployStatus::Queued], DeployStatus::Building, None)).await?;
                let pinned = self.deps.runtime.pull(img, log).await?;
                log.line(format!("pinned {pinned}"));
                let (id, p) = (d.id.clone(), pinned.clone());
                self.db(move |s| s.set_built(&id, &p, None, None)).await?;
                (pinned, svc.run.clone())
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
        let tpl = Arc::new(self.template(&svc, &env, &dep).await?);
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
        let (sid, e, from, to, vh) = (svc.id.clone(), env.name.clone(), env.active.clone(), d.id.clone(), vars_hash.clone());
        // Epochs only grow on a backend that outlives the process; the in-memory one starts
        // again at 1, and a fence kept in the store would then refuse every later switch.
        let epoch = Some(&self.deps.cluster).filter(|c| c.backend_name() != "memory").and_then(|c| c.epoch(&crate::lease_key(&svc.id)));
        let ready = (!made.is_empty()).then_some(took.as_millis() as i64);
        let switched = self
            .db(move |s| {
                let ok = s.switch_active(&sid, &e, from.as_deref(), &to, ready, epoch)?;
                if ok {
                    s.set_vars_hash(&to, &vh)?;
                }
                Ok(ok)
            })
            .await?;
        if !switched {
            for i in &made {
                self.deps.runtime.remove(&i.name).await;
            }
            anyhow::bail!("another deploy switched this environment meanwhile");
        }
        live.commit(tpl, prepared).await;
        Ok(())
    }

    /// Build the deployment's tree into an image: the tree from its source, the plan from
    /// `apps-build`, the Dockerfile written in when the plan made one.
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
        let context: PathBuf = match &root {
            Some(r) => crate::model::in_tree(&tree, "the root directory", r, true)?,
            None => tree.clone(),
        };
        let overrides = svc.build.overrides();
        let ctx = context.clone();
        let plan = tokio::task::spawn_blocking(move || apps_build::plan(&apps_build::DirTree::new(ctx), &overrides)).await?.map_err(|e| anyhow::anyhow!("{e}"))?;
        for n in &plan.notes {
            log.line(format!("plan: {n}"));
        }
        let dockerfile = if plan.provider == apps_build::Provider::Dockerfile {
            let df = svc.build.dockerfile.clone().unwrap_or_else(|| "Dockerfile".into());
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
        let mut run = svc.run.clone();
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
        };
        Ok(Template { deployment: dep.id.clone(), spec, run })
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
            }
            if n.is_multiple_of(3600) {
                let before = code_sql::now_ms() / 60_000 - 7 * 24 * 60;
                let _ = self.db(move |s| s.prune_metrics(before)).await;
                let day_ago = code_sql::now_ms() - 24 * 3_600_000;
                let _ = self.db(move |s| s.prune_deploy_keys(day_ago)).await;
            }
        }
    }

    /// Services that must run without a request (workers, cron, minimum instances, queued
    /// deploys) get an owner: every node takes the leases of those whose preferred node it is.
    async fn keeper(self: Arc<Self>) {
        loop {
            if let Ok(rows) = self.db(|s| s.services(None)).await {
                for svc in rows {
                    let always = svc.run.kind != RunType::Web || svc.run.min_instances > 0;
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
                    for l in &page {
                        if let Err(e) = self.on_landing(l).await {
                            warn!(repo = %l.repo, branch = %l.branch, error = %e, "apps: queueing a deploy for a landing failed");
                        }
                        last = l.seq;
                    }
                    if last != cursor {
                        let _ = self.db(move |s| s.set_state("landings", &last.to_string())).await;
                    }
                    if page.len() == 200 {
                        continue;
                    }
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                _ = self.stop.cancelled() => return,
            }
        }
    }

    async fn on_landing(self: &Arc<Self>, l: &crate::integrations::Landing) -> Result<()> {
        let all = self.db(|s| Ok((s.services(None)?, s.all_environments()?))).await?;
        let (services, envs) = all;
        for svc in services.iter().filter(|s| matches!(&s.source, Source::Repo { repo, .. } if *repo == l.repo)) {
            for e in envs.iter().filter(|e| e.service == svc.id && !e.ephemeral && e.branch.as_deref() == Some(l.branch.as_str())) {
                let body = serde_json::json!({ "revision": l.revision.sha, "message": l.revision.message.lines().next().unwrap_or("") });
                // The owner queues it (it may be another node: the queue is in the store).
                self.deploy(&svc.id, &e.name, body, Trigger::Push, None, None).await?;
                info!(service = %svc.id, env = %e.name, revision = %l.revision.sha, "apps: deploying a push");
            }
        }
        Ok(())
    }

    /// Cron: start the runs that are due in owned cron services (a run still going when the
    /// next is due is skipped).
    async fn cron_tick(self: &Arc<Self>) {
        let Ok(services) = self.db(|s| s.services(None)).await else { return };
        let now = code_sql::now_ms();
        for svc in services.into_iter().filter(|s| s.run.kind == RunType::Cron && self.owns(&s.id)) {
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
        let (sid, e) = (service.to_string(), env.to_string());
        let running = self.db(move |s| Ok(s.runs(&sid, &e, 1)?.into_iter().any(|r| r.status == RunStatus::Running))).await?;
        let (sid, e, a) = (service.to_string(), env.to_string(), active.clone());
        if running {
            return self.db(move |s| s.add_run(&sid, &e, &a, RunStatus::Skipped)).await;
        }
        let run = self.db(move |s| s.add_run(&sid, &e, &a, RunStatus::Running)).await?;
        let live = self.load_live(service, env).await?;
        let timeout = Duration::from_secs(svc.run.timeout_s.unwrap_or(3600) as u64);
        let (this, id) = (self.clone(), run.id.clone());
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
            let _ = this.db(move |s| s.finish_run(&id, status, code)).await;
        });
        Ok(run)
    }

    /// Check pending custom domains' DNS every minute (one node).
    async fn domains_loop(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                _ = self.stop.cancelled() => return,
            }
            if !self.deps.cluster.singleton("apps:domains") {
                continue;
            }
            let Ok(pending) = self.db(|s| s.pending_domains()).await else { continue };
            for (svc, env, d) in pending {
                let _ = self.verify_domain(&svc, &env, &d.hostname).await;
            }
        }
    }

    /// Stop the background work and write what's metered (SIGTERM).
    pub async fn shutdown(&self) {
        self.stop.cancel();
    }
}
