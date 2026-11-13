//! Bringing one preview to its desired state, and deleting one.

use super::*;

impl Engine {
    pub(super) async fn converge_one(&self, r: Row) {
        let id = r.id.clone();
        let deleting = r.desired == Desired::Deleted;
        let result = if deleting { self.teardown(&r).await } else { self.converge(&r).await };
        match result {
            Ok(()) => {}
            Err(e) => {
                let msg = format!("{e:#}");
                warn!(preview = %id, error = %msg, "previews: converging");
                let now = self.store.now_ms().unwrap_or_default();
                let updated = self.store.update(&id, |row| {
                    row.attempts += 1;
                    // Asked for again meanwhile (deleted, reset): now, not after the backoff.
                    row.next_at = if row.generation == r.generation { now + (BACKOFF_MS << (row.attempts - 1).clamp(0, 10)).min(MAX_BACKOFF_MS) } else { 0 };
                    row.error = Some(msg.chars().take(4000).collect());
                    if !deleting {
                        row.state = State::Failed;
                    }
                });
                if let Ok(row) = updated {
                    let _ = self.store.log(&id, &format!("failed: {}", msg.lines().next().unwrap_or("")));
                    if row.attempts == 1 {
                        self.report(row).await;
                    }
                }
            }
        }
    }

    pub(super) fn brancher(&self, kind: Kind) -> anyhow::Result<&Arc<dyn Brancher>> {
        self.branchers.get(&kind).ok_or_else(|| anyhow::anyhow!("roll {} isn't on this node, so it can't be branched", kind.as_str()))
    }

    /// Bring one active preview to its desired state.
    pub(super) async fn converge(&self, r: &Row) -> anyhow::Result<()> {
        let s = self.store.settings(&r.project_id)?;
        // A fork's head waits for a writer's approval; nothing of it runs meanwhile.
        if let Some(change) = &r.change_id {
            let Some(c) = self.source.change(change).await? else {
                self.mark_deleted(&r.id, "the change was deleted")?;
                return Ok(());
            };
            // The change moved on since this pass read it: converge again on its new head (the
            // next pass records it), never deploying a head nobody checked.
            if c.head != r.head && c.open {
                self.store.update(&r.id, |row| row.next_at = 0)?;
                return Ok(());
            }
            if !c.may_run() {
                for svc in self.store.services(&r.id)? {
                    self.apps.sleep(&r.id, &svc.name).await;
                    self.store.set_service(&r.id, &svc.name, ServiceState::Sleeping, None, None)?;
                }
                let row = self.store.update(&r.id, |row| {
                    row.state = State::AwaitingApproval;
                    row.next_at = if row.generation == r.generation { SETTLED } else { 0 };
                    row.attempts = 0;
                    row.error = None;
                })?;
                self.report(row).await;
                return Ok(());
            }
        }
        if r.state == State::Queued {
            self.store.update(&r.id, |row| row.next_at = if row.generation == r.generation { SETTLED } else { 0 })?;
            return Ok(());
        }
        if r.state == State::AwaitingApproval {
            self.store.update(&r.id, |row| row.state = State::Creating)?;
            self.store.log(&r.id, "approved: starting")?;
        }
        let project = self.workspace.get(&r.project_id)?;
        let name = branch_name(&r.id);

        // 1. Data: a branch of every data resource of the project the rules don't skip.
        let wanted: Vec<(Kind, String, Mode)> = project
            .resources
            .iter()
            .filter_map(|res| Kind::of_workspace(res.kind).map(|k| (k, res.r#ref.clone())))
            .filter_map(|(k, parent)| s.mode_of(k, &parent).map(|m| (k, parent, m)))
            .collect();
        // A resource whose rule changed (branch ↔ schema) is branched again in its new mode: a
        // project that moves to structure-only stops keeping full copies at once.
        let changed: Vec<ResourceRow> = self
            .store
            .resources(&r.id)?
            .into_iter()
            .filter(|x| wanted.iter().any(|(k, p, m)| *k == x.kind && *p == x.parent && *m != x.mode))
            .collect();
        for res in changed {
            self.delete_branch(&r.id, &res, &name).await?;
            self.store.drop_resource(&r.id, res.kind, &res.parent)?;
            self.store.log(&r.id, &format!("{}:{} moves to {} mode: branched again", res.kind.as_str(), res.parent, res.mode.as_str()))?;
        }
        for gone in self.store.want_resources(&r.id, &wanted)? {
            self.delete_branch(&r.id, &gone, &name).await?;
            self.store.drop_resource(&r.id, gone.kind, &gone.parent)?;
        }
        let now = self.store.now_ms()?;
        let reset_due = s.reset_every_hours > 0 && r.last_reset_at.unwrap_or(r.created_at) + s.reset_every_hours as i64 * 3_600_000 <= now;
        let asked: Vec<String> = r.reset_requested.clone().unwrap_or_default();
        let mut reset_any = false;
        let mut problems: Vec<String> = Vec::new();
        // Nothing serves data while it's replaced (and until its seed and masks have run again).
        let resetting = self.store.resources(&r.id)?.iter().any(|x| x.r#ref.is_some() && (reset_due || asked.iter().any(|a| a == "*" || *a == format!("{}:{}", x.kind.as_str(), x.parent))));
        if resetting {
            for svc in self.store.services(&r.id)? {
                self.apps.sleep(&r.id, &svc.name).await;
                self.store.set_service(&r.id, &svc.name, ServiceState::Sleeping, None, None)?;
            }
        }
        for res in self.store.resources(&r.id)? {
            let key = format!("{}:{}", res.kind.as_str(), res.parent);
            let b = match self.brancher(res.kind) {
                Ok(b) => b.clone(),
                Err(e) => {
                    let msg = e.to_string();
                    self.store.set_resource(&r.id, res.kind, &res.parent, |x| {
                        x.state = ResourceState::Failed;
                        x.error = Some(msg.clone());
                    })?;
                    problems.push(msg);
                    continue;
                }
            };
            match &res.r#ref {
                None => {
                    let made = b.branch(&res.parent, &name, res.mode).await.map_err(|e| anyhow::anyhow!("branching {key}: {e:#}"))?;
                    // Recorded at once (a crash mustn't lose it), Ready only once it's masked.
                    self.store.set_resource(&r.id, res.kind, &res.parent, |x| {
                        x.r#ref = Some(made.r#ref.clone());
                        x.kept = made.kept.clone();
                        x.state = ResourceState::Pending;
                        x.error = None;
                    })?;
                    self.store.log(&r.id, &format!("branched {key} as {}", made.r#ref))?;
                    self.store.set_seed_needed(&r.id, true)?;
                    self.mask(&r.id, &b, &key, &made.r#ref, &s).await?;
                    self.store.set_resource(&r.id, res.kind, &res.parent, |x| x.state = ResourceState::Ready)?;
                }
                Some(branch) if reset_due || asked.iter().any(|a| a == "*" || *a == key) => {
                    self.store.set_resource(&r.id, res.kind, &res.parent, |x| x.state = ResourceState::Resetting)?;
                    let fresh = b.reset(&res.parent, branch, res.kept.as_deref(), &name, res.mode).await.map_err(|e| anyhow::anyhow!("resetting {key}: {e:#}"))?;
                    self.store.set_resource(&r.id, res.kind, &res.parent, |x| {
                        x.r#ref = Some(fresh.r#ref.clone());
                        x.kept = fresh.kept.clone();
                        x.state = ResourceState::Resetting;
                        x.error = None;
                        x.bytes = 0;
                    })?;
                    self.store.log(&r.id, &format!("reset {key} from its parent"))?;
                    self.store.set_seed_needed(&r.id, true)?;
                    self.mask(&r.id, &b, &key, &fresh.r#ref, &s).await?;
                    self.store.set_resource(&r.id, res.kind, &res.parent, |x| x.state = ResourceState::Ready)?;
                    reset_any = true;
                }
                // Made (or reset) but not finished: masks again, then Ready.
                Some(branch) if res.state != ResourceState::Ready => {
                    self.mask(&r.id, &b, &key, branch, &s).await?;
                    self.store.set_resource(&r.id, res.kind, &res.parent, |x| {
                        x.state = ResourceState::Ready;
                        x.error = None;
                    })?;
                }
                Some(_) => {}
            }
        }
        if reset_any || reset_due || !asked.is_empty() {
            self.store.update(&r.id, |row| {
                // Only what this converge did: a reset asked for meanwhile stays asked.
                if row.reset_requested == r.reset_requested {
                    row.reset_requested = None;
                }
                if reset_any || reset_due {
                    row.last_reset_at = Some(now);
                }
            })?;
        }

        // 2. What the branches use (every `usage_every`), and the size cap.
        let usage_due = self.usage_read.lock().get(&r.id).is_none_or(|t| *t + self.cfg.usage_every.as_millis() as i64 <= now);
        if usage_due {
            for res in self.store.resources(&r.id)? {
                let (Some(branch), Ok(b)) = (&res.r#ref, self.brancher(res.kind)) else { continue };
                match b.usage(&res.parent, branch).await {
                    Ok(u) => self.store.set_resource(&r.id, res.kind, &res.parent, |x| {
                        x.bytes = u.bytes;
                        x.usd_month = u.storage_usd_month;
                        x.usd_hour = u.active_usd_hour;
                        x.spent_usd = u.month_to_date_usd;
                    })?,
                    Err(e) => warn!(preview = %r.id, kind = res.kind.as_str(), error = %e, "previews: reading usage"),
                }
            }
            self.usage_read.lock().insert(r.id.clone(), now);
        }
        let bytes: i64 = self.store.resources(&r.id)?.iter().map(|x| x.bytes).sum();
        if bytes as u64 > s.max_bytes {
            for svc in self.store.services(&r.id)? {
                self.apps.sleep(&r.id, &svc.name).await;
            }
            let msg = format!("its branches hold {bytes} bytes, over the project's max_bytes ({}): reset it to start again", s.max_bytes);
            let row = self.store.update(&r.id, |row| {
                row.state = State::Sleeping;
                row.error = Some(msg.clone());
                row.next_at = if row.generation == r.generation { SETTLED } else { 0 };
            })?;
            self.report(row).await;
            return Ok(());
        }

        // 3. The secrets environment: its base, the project's overrides, the branches' URLs.
        if is_production(&s.secrets.base) && (!s.secrets.allow_production || !r.trusted) {
            anyhow::bail!("this preview may not inherit the production environment {:?}", s.secrets.base);
        }
        let urls = self.resource_env(&r.id).await?;
        let branches = self.branch_vars(&r.id).await?;
        let (access_token, bypass) = match self.store.access(&r.id)? {
            Some(crate::store::Access { token: t, required: true, bypass }) if !t.is_empty() => (Some(t), bypass),
            _ => (None, None),
        };
        let mut vars: Vec<(String, String)> = s.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (k, v) in urls {
            set(&mut vars, &k, v);
        }
        // Every converge (at least every few minutes, for usage) pushes its expiry out: only a
        // preview nobody converges any more lets it lapse.
        let trusted = s.secrets.allow_production && r.trusted;
        let expires_at = self.store.now_ms()? + SECRETS_ENV_MARGIN_MS;
        self.secrets.ensure(&r.project_id, &env_name(&r.id), &s.secrets.base, &vars, trusted, expires_at).await.map_err(|e| anyhow::anyhow!("secrets: {e:#}"))?;

        // 3b. The project's seed, once on fresh data, before anything serves it.
        if let Some(seed) = &s.seed
            && self.store.seed_needed(&r.id)?
        {
            self.run_job(r, &s, &vars, "seed", seed).await?;
            self.store.set_seed_needed(&r.id, false)?;
            self.store.log(&r.id, "seeded")?;
        }
        // 3c. The release (migrations) at each new head, on this data, before services deploy.
        if let Some(release) = &s.release
            && self.store.released(&r.id)?.as_deref() != Some(r.head.as_str())
        {
            self.run_job(r, &s, &vars, "release", release).await?;
            self.store.set_released(&r.id, &r.head)?;
            self.store.log(&r.id, "released")?;
        }
        // 3d. What that did to the schema, once per head (best effort: a reviewer's aid). Only
        // after a seed or release could have changed it: reading the parent's starts its compute.
        if s.seed.is_none() && s.release.is_none() {
            self.store.clear_schema_changes(&r.id)?;
        } else if self.store.schema_head(&r.id)?.as_deref() != Some(r.head.as_str()) {
            self.read_schema(r).await;
        }

        // 4. Services: the change's repo at its head, every other repo at its default head.
        // From roll apps' catalog when the project has services there, else the repos'
        // `roll.toml`.
        let repos: Vec<String> = project.resources.iter().filter(|x| x.kind == roll_workspace::Kind::Repo).map(|x| x.r#ref.clone()).collect();
        // (spec, repo, revision, whether roll apps runs it)
        let mut wanted: Vec<(ServiceSpec, String, String, bool)> = Vec::new();
        let head_of = |repo: String| async move {
            if repo == r.repo { Ok::<_, anyhow::Error>(Some(r.head.clone())) } else { self.source.head(&repo, None).await }
        };
        match self.apps.catalog(&r.project_id).await.map_err(|e| anyhow::anyhow!("reading the project's services: {e:#}"))? {
            Some(catalog) if !catalog.is_empty() => {
                for c in catalog {
                    let rev = match &c.repo {
                        Some(repo) => match head_of(repo.clone()).await? {
                            Some(rev) => rev,
                            None => continue,
                        },
                        None => String::new(),
                    };
                    let spec = ServiceSpec { name: c.name.clone(), image: String::new(), run: String::new(), port: 0, memory_mb: c.memory_mb, cpus: 0.0, env: Vec::new() };
                    wanted.push((spec, c.repo.unwrap_or_default(), rev, true));
                }
            }
            _ => {
                for repo in &repos {
                    let Some(rev) = head_of(repo.clone()).await? else { continue };
                    for spec in self.source.services(repo, &rev).await.map_err(|e| anyhow::anyhow!("reading {repo}'s services: {e:#}"))? {
                        wanted.push((spec, repo.clone(), rev.clone(), false));
                    }
                }
            }
        }
        let mut keep: Vec<String> = Vec::new();
        for (spec, repo, rev, on_apps) in wanted {
            keep.push(spec.name.clone());
            let svc = self.store.want_service(&r.id, &spec.name, &repo, &rev, spec.memory_mb, &|h| self.cfg.url(h))?;
            // A redeploy when the revision moved, or what it's deployed with did (its variables:
            // a reset's new URLs, changed overrides; its access): the runtime restarts it on its
            // next request, roll apps redeploys it.
            // The bypass only matters to a backend that checks access itself (roll apps): the
            // runtime's gate reads it on each request, so a rotation restarts nothing there.
            let fingerprint = config_fingerprint(&vars, access_token.is_some(), bypass.as_deref().filter(|_| on_apps));
            let config_moved = self.store.service_config(&r.id, &spec.name)?.as_deref() != Some(fingerprint.as_str());
            if svc.deployed.as_deref() != Some(&rev) || reset_any || config_moved {
                let git = if repo.is_empty() || rev.is_empty() { None } else { self.source.git_commit(&repo, &rev).await? };
                let t = Target {
                    project: r.project_id.clone(),
                    preview: r.id.clone(),
                    service: spec.name.clone(),
                    host: svc.host.clone(),
                    repo: repo.clone(),
                    revision: rev.clone(),
                    git,
                    spec: spec.clone(),
                    sleep_after: Duration::from_secs(s.sleep_after_s as u64),
                    environment: env_name(&r.id),
                    vars: vars.clone(),
                    branches: branches.clone(),
                    access_token: access_token.clone(),
                    bypass_sha256: bypass.clone(),
                };
                self.store.set_service(&r.id, &spec.name, ServiceState::Deploying, None, None)?;
                match self.apps.deploy(&t).await {
                    Ok(url) => {
                        if let Some(url) = url {
                            self.store.set_service_url(&r.id, &spec.name, &url)?;
                        }
                        let state = self.apps.status(&r.id, &spec.name).unwrap_or(ServiceState::Sleeping);
                        self.store.set_service(&r.id, &spec.name, state, Some(&rev), None)?;
                        self.store.set_service_config(&r.id, &spec.name, &fingerprint)?;
                        self.store.log(&r.id, &format!("deployed {} at {}", spec.name, short(&rev)))?;
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        self.store.set_service(&r.id, &spec.name, ServiceState::Failed, None, Some(&msg))?;
                        anyhow::bail!("deploying {}: {msg}", spec.name);
                    }
                }
            }
        }
        for svc in self.store.services(&r.id)? {
            if !keep.contains(&svc.name) {
                self.apps.remove(&r.id, &svc.name).await.map_err(|e| anyhow::anyhow!("removing {}: {e:#}", svc.name))?;
                self.store.drop_service(&r.id, &svc.name)?;
                self.store.log(&r.id, &format!("removed service {}", svc.name))?;
            }
        }

        // 5. Settled.
        let running = self.store.services(&r.id)?.iter().any(|x| x.state == ServiceState::Running);
        let error = (!problems.is_empty()).then(|| problems.join("; "));
        let was = r.state;
        let row = self.store.update(&r.id, |row| {
            row.state = if running || row.state != State::Sleeping { State::Ready } else { State::Sleeping };
            row.error = error.clone();
            row.attempts = 0;
            row.next_at = if row.generation == r.generation { SETTLED } else { 0 };
        })?;
        if was != row.state || was == State::Creating || was == State::Updating {
            self.store.log(&r.id, row.state.as_str())?;
        }
        self.report(row).await;
        Ok(())
    }

    /// The project's column masks for a fresh branch, if it has any for this resource.
    /// Run the project's seed or release to its end on the preview's tree: its secrets
    /// environment (as services get it: untrusted unless the project allows production and the
    /// change is trusted), the branches and overrides on top.
    async fn run_job(&self, r: &Row, s: &Settings, vars: &[(String, String)], what: &str, job: &crate::settings::Seed) -> anyhow::Result<()> {
        let untrusted = !(s.secrets.allow_production && r.trusted);
        let mut env = self.resolve_secrets(r, untrusted).await?;
        for (k, v) in vars {
            set(&mut env, k, v.clone());
        }
        set(&mut env, "ROLL_PREVIEW", r.id.clone());
        set(&mut env, "ROLL_REPO", r.repo.clone());
        set(&mut env, "ROLL_CHANGESET", r.head.clone());
        let run = crate::ports::SeedJob { preview: r.id.clone(), repo: r.repo.clone(), revision: r.head.clone(), seed: job.clone(), env };
        self.store.log(&r.id, &format!("{what}: {}", job.run.lines().next().unwrap_or("")))?;
        let (code, tail) = self.apps.seed(&run).await.map_err(|e| anyhow::anyhow!("{what}: {e:#}"))?;
        if code != 0 {
            anyhow::bail!("the {what} command failed with exit code {code}\n\n{}", tail.trim_end());
        }
        Ok(())
    }

    /// The Postgres branches' schema against their parents' (`schema::diff`), kept for `head`.
    /// A failure is logged and read again on the next converge.
    async fn read_schema(&self, r: &Row) {
        let read = async {
            let mut all: Option<Vec<String>> = None;
            let resources = self.store.resources(&r.id)?;
            let several = resources.iter().filter(|x| x.kind == Kind::Postgres).count() > 1;
            for res in resources.iter().filter(|x| x.kind == Kind::Postgres) {
                let Some(branch) = &res.r#ref else { continue };
                let Some((parent, mine)) = self.brancher(res.kind)?.schema(&res.parent, branch).await? else { continue };
                let lines = crate::schema::diff(&parent, &mine);
                let out = all.get_or_insert_with(Vec::new);
                if several && !lines.is_empty() {
                    out.push(format!("# {}", res.parent));
                }
                out.extend(lines);
            }
            anyhow::Ok(all.map(crate::schema::cap))
        };
        match read.await {
            Ok(changes) => {
                if let Err(e) = self.store.set_schema_changes(&r.id, &r.head, changes.as_deref()) {
                    warn!(preview = %r.id, error = %e, "previews: keeping the schema's changes");
                }
            }
            Err(e) => {
                // An older head's changes aren't this one's: none shown until a read works.
                let _ = self.store.clear_schema_changes(&r.id);
                let _ = self.store.log(&r.id, &format!("couldn't read the schema's changes: {e:#}"));
            }
        }
    }

    pub(super) async fn mask(&self, id: &str, b: &Arc<dyn Brancher>, key: &str, branch: &str, s: &Settings) -> anyhow::Result<()> {
        let Some(cols) = s.masking.get(key).filter(|c| !c.is_empty()) else { return Ok(()) };
        let parent = key.split_once(':').map_or("", |(_, p)| p);
        b.mask(parent, branch, cols).await.map_err(|e| anyhow::anyhow!("masking {key}: {e:#}"))?;
        self.store.log(id, &format!("masked {} column(s) of {key}", cols.len()))?;
        Ok(())
    }

    /// The variables every service of the preview gets for its branches.
    pub(super) async fn resource_env(&self, id: &str) -> anyhow::Result<Vec<(String, String)>> {
        let mut seen: BTreeMap<Kind, usize> = BTreeMap::new();
        let mut env = Vec::new();
        for (kind, parent, vars) in self.branch_vars(id).await? {
            let n = seen.entry(kind).or_default();
            let suffix = (*n > 0).then(|| env_suffix(&parent));
            *n += 1;
            for (k, v) in suffixed(vars, suffix.as_deref()) {
                set(&mut env, &k, v);
            }
        }
        Ok(env)
    }

    /// Every branch's own variables, unprefixed, with its kind and parent.
    pub(super) async fn branch_vars(&self, id: &str) -> anyhow::Result<Vec<crate::ports::BranchVars>> {
        let mut out = Vec::new();
        for res in self.store.resources(id)? {
            let (Some(branch), Ok(b)) = (&res.r#ref, self.brancher(res.kind)) else { continue };
            let vars = b.connect(&res.parent, branch, res.kept.as_deref()).await.map_err(|e| anyhow::anyhow!("connecting to the {} branch: {e:#}", res.kind.as_str()))?;
            out.push((res.kind, res.parent.clone(), vars));
        }
        Ok(out)
    }

    pub(super) async fn delete_branch(&self, id: &str, res: &ResourceRow, name: &str) -> anyhow::Result<()> {
        // No ref recorded: a branch made just before a crash (or a timeout) may still exist under
        // the preview's name. Find it, so nothing outlives the preview.
        let found;
        let branch = match &res.r#ref {
            Some(b) => b,
            None => match self.brancher(res.kind) {
                Ok(b) => match b.find(&res.parent, name).await? {
                    Some(f) => {
                        found = f;
                        &found
                    }
                    None => return Ok(()),
                },
                Err(_) => return Ok(()),
            },
        };
        // A branch must never be its parent: refuse rather than delete production.
        if branch == &res.parent {
            anyhow::bail!("refusing to delete {} {branch}: it's the preview's parent", res.kind.as_str());
        }
        self.store.set_resource(id, res.kind, &res.parent, |x| x.state = ResourceState::Deleting)?;
        self.brancher(res.kind)?.delete(&res.parent, branch, name).await.map_err(|e| anyhow::anyhow!("deleting the {} branch {branch}: {e:#}", res.kind.as_str()))?;
        self.store.log(id, &format!("deleted the {} branch {branch}", res.kind.as_str()))?;
        Ok(())
    }

    /// Delete a preview: services, secrets, branches, then the row. Each step is idempotent.
    pub(super) async fn teardown(&self, r: &Row) -> anyhow::Result<()> {
        if r.state != State::Deleting {
            let row = self.store.update(&r.id, |row| row.state = State::Deleting)?;
            self.report(row).await;
        }
        for svc in self.store.services(&r.id)? {
            self.apps.remove(&r.id, &svc.name).await.map_err(|e| anyhow::anyhow!("removing {}: {e:#}", svc.name))?;
            self.store.drop_service(&r.id, &svc.name)?;
        }
        self.secrets.delete(&r.project_id, &env_name(&r.id)).await.map_err(|e| anyhow::anyhow!("secrets: {e:#}"))?;
        let name = branch_name(&r.id);
        for res in self.store.resources(&r.id)? {
            self.delete_branch(&r.id, &res, &name).await?;
            self.store.drop_resource(&r.id, res.kind, &res.parent)?;
        }
        let view = self.store.preview(&r.id)?;
        self.source.forget(&view).await;
        // What it cost this month stays counted against the project's budget.
        self.store.add_spend(&r.project_id, crate::store::month_start(self.store.now_ms()?), view.cost.month_to_date_usd)?;
        self.store.remove(&r.id)?;
        if let Some(c) = self.cluster.get() {
            c.retire(&preview_key(&r.id));
        }
        self.usage_read.lock().remove(&r.id);
        self.secrets_told.lock().remove(&r.id);
        info!(preview = %r.id, "previews: deleted");
        Ok(())
    }

    pub(super) async fn report(&self, row: Row) {
        let Ok(s) = self.store.settings(&row.project_id) else { return };
        if let Ok(p) = self.store.view(row) {
            self.source.report(&p, &s).await;
        }
    }

}
