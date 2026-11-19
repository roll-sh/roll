//! Bringing one preview to its desired state, and deleting one.

use super::*;

/// How long one resource's columns are read for mask suggestions before it's named in `unread`.
pub const SUGGEST_READ_FOR: std::time::Duration = std::time::Duration::from_secs(10);

impl Engine {
    pub(super) async fn converge_one(&self, r: Row) {
        let id = r.id.clone();
        // While it converges its `next_at` is a lease, not 0: a request meanwhile (a variable
        // removed, a push, a keep) sets it to 0, which is a change, so it bumps the generation and
        // this converge doesn't settle over it. A crash mid-converge is retried when it ends.
        let lease = self.store.now_ms().unwrap_or_default() + CONVERGE_LEASE_MS;
        if self.store.update(&id, |row| row.next_at = lease).is_err() {
            return;
        }
        let deleting = r.desired == Desired::Deleted;
        let result = if deleting { self.teardown(&r).await } else { self.converge(&r).await };
        Counters::add(&self.counters.converges);
        match result {
            Ok(()) => {}
            Err(e) => {
                Counters::add(&self.counters.converge_failures);
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

    /// Refuses masks `new` adds or changes over `old` that a product says can't work (a NoSQL
    /// key column, a string mask on a search field that isn't text). A product that can't be asked (not on this node, unreachable) doesn't hold
    /// the settings up: its own read policy still applies.
    pub(crate) async fn check_masks(&self, old: &Settings, new: &Settings) -> crate::Result<()> {
        let mut problems = Vec::new();
        for (key, columns) in &new.masking {
            let Some((kind, parent)) = key.strip_prefix("nosql:").map(|p| (Kind::Nosql, p)).or_else(|| key.strip_prefix("search:").map(|p| (Kind::Search, p))) else { continue };
            let added: std::collections::BTreeMap<String, String> =
                columns.iter().filter(|(c, f)| old.masking.get(key).and_then(|o| o.get(*c)) != Some(f)).map(|(c, f)| (c.clone(), f.clone())).collect();
            if added.is_empty() {
                continue;
            }
            let Ok(b) = self.brancher(kind) else { continue };
            match b.mask_problems(parent, &added).await {
                Ok(p) => problems.extend(p.into_iter().map(|p| format!("masking: {key}: {p}"))),
                Err(e) => tracing::warn!(error = %e, "checking {} masks against {parent}'s schema", kind.as_str()),
            }
        }
        if problems.is_empty() { Ok(()) } else { Err(crate::Error::BadRequest(problems.join("; "))) }
    }

    /// Columns of the project's data that look like people's data and aren't masked yet
    /// (`GET …/mask-suggestions`): of every Postgres, NoSQL and search resource on its canvas that
    /// the project stands for (`Workspace::trusts`) and `may_read` allows (a scoped token's grant
    /// for that resource), whatever its rule now (the dialog shows them under those on `branch`,
    /// as the rule being edited says), at most [`crate::mask::SUGGEST_COLUMNS_MAX`] columns read
    /// each. A resource that isn't read is named in `unread`, with why: it sleeps (reading it
    /// would wake it), it didn't answer within [`SUGGEST_READ_FOR`], it couldn't be read, or it
    /// has more columns than are read.
    pub async fn mask_suggestions(&self, project_id: &str, may_read: impl Fn(Kind, &str) -> bool) -> crate::Result<serde_json::Value> {
        let max = crate::mask::SUGGEST_COLUMNS_MAX;
        let project = self.workspace_project(project_id)?;
        let s = self.store.settings(project_id)?;
        let resources: Vec<(Kind, String)> = project
            .resources
            .iter()
            .filter(|r| self.workspace.trusts(&project, r.kind, &r.r#ref))
            .filter_map(|r| Kind::of_workspace(r.kind).map(|k| (k, r.r#ref.clone())))
            .filter(|(k, parent)| matches!(k, Kind::Postgres | Kind::Nosql | Kind::Search) && may_read(*k, parent))
            .collect();
        // `None`: not read in time.
        let read = futures::future::join_all(resources.iter().map(|(k, parent)| async move {
            let b = match self.brancher(*k) {
                Ok(b) => b,
                Err(e) => return Some(Err(e)),
            };
            tokio::time::timeout(SUGGEST_READ_FOR, b.columns(parent, max + 1)).await.ok()
        }))
        .await;
        let (mut suggestions, mut unread) = (Vec::new(), Vec::new());
        for ((kind, parent), columns) in resources.iter().zip(read) {
            let key = format!("{}:{parent}", kind.as_str());
            let why = match columns {
                Some(Ok(c)) => Ok(c),
                Some(Err(e)) if e.is::<crate::ports::Asleep>() => Err("asleep: open it to read its columns".to_string()),
                Some(Err(e)) => {
                    warn!(resource = %key, error = %format!("{e:#}"), "previews: reading columns for mask suggestions");
                    Err("its columns couldn't be read".into())
                }
                None => Err(format!("its columns weren't read within {} s", SUGGEST_READ_FOR.as_secs())),
            };
            let mut columns = match why {
                Ok(c) => c,
                Err(why) => {
                    unread.push(serde_json::json!({ "resource": key, "why": why }));
                    continue;
                }
            };
            if columns.len() > max {
                columns.truncate(max);
                unread.push(serde_json::json!({ "resource": key, "why": format!("only its first {max} columns were read") }));
            }
            let masked = s.masking.get(&key);
            for c in &columns {
                // Already masked, or a path the settings wouldn't take as a mask.
                let unnameable = c.path.len() > 200 || (*kind == Kind::Search && c.path.contains(['*', ',', '/', ' ', '\\']));
                if unnameable || masked.is_some_and(|m| m.contains_key(&c.path)) {
                    continue;
                }
                if let Some(x) = crate::mask::suggest(*kind, c) {
                    suggestions.push(serde_json::json!({ "resource": key, "path": c.path, "rule": x.rule, "why": x.why }));
                }
            }
        }
        Ok(serde_json::json!({ "suggestions": suggestions, "unread": unread }))
    }

    /// A head nobody approved yet: nothing of it runs (its services sleep) until a writer does.
    async fn await_approval(&self, r: &Row) -> anyhow::Result<()> {
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
        Ok(())
    }

    /// One data resource of a preview brought to what its rules want (branched, reset, masked,
    /// given credentials): `(whether it was reset, a problem to say)`. The resources of a preview
    /// are independent, so a converge runs them together.
    async fn converge_resource(&self, r: &Row, s: &Settings, name: &str, res: ResourceRow, reset_due: bool, asked: &[String]) -> anyhow::Result<(bool, Option<String>)> {
        let mut reset = false;
        let key = format!("{}:{}", res.kind.as_str(), res.parent);
        let b = match self.brancher(res.kind) {
            Ok(b) => b.clone(),
            Err(e) => {
                let msg = e.to_string();
                self.store.set_resource(&r.id, res.kind, &res.parent, |x| {
                    x.state = ResourceState::Failed;
                    x.error = Some(msg.clone());
                })?;
                return Ok((false, Some(msg)));
            }
        };
        match &res.r#ref {
            None => {
                // As of `data_at` where the product keeps history (structure-only needs none).
                let at = s.data_at.as_deref().filter(|_| res.mode == Mode::Branch).and_then(|a| crate::settings::data_at_secs(a, self.store.now_ms().unwrap_or(0) / 1000));
                let past = match at {
                    Some(at) => b.branch_at(&res.parent, name, res.mode, at).await.map_err(|e| anyhow::anyhow!("branching {key} as of {at}: {e:#}"))?,
                    None => None,
                };
                if at.is_some() && past.is_none() {
                    self.store.log(&r.id, &format!("{key} keeps no history: branched as of now, not data_at"))?;
                }
                let made = match past {
                    Some(m) => m,
                    None => b.branch(&res.parent, name, res.mode).await.map_err(|e| anyhow::anyhow!("branching {key}: {e:#}"))?,
                };
                // Recorded at once (a crash mustn't lose it), Ready only once it's masked.
                self.store.set_resource(&r.id, res.kind, &res.parent, |x| {
                    x.r#ref = Some(made.r#ref.clone());
                    x.kept = made.kept.clone();
                    x.state = ResourceState::Pending;
                    x.error = None;
                })?;
                self.store.log(&r.id, &format!("branched {key} as {}", made.r#ref))?;
                self.store.set_seed_needed(&r.id, true)?;
                self.mask(&r.id, &b, &key, &made.r#ref, s).await?;
                self.store.set_resource(&r.id, res.kind, &res.parent, |x| x.state = ResourceState::Ready)?;
            }
            Some(branch) if reset_due || asked.iter().any(|a| a == "*" || *a == key) => {
                self.store.set_resource(&r.id, res.kind, &res.parent, |x| x.state = ResourceState::Resetting)?;
                let at = s.data_at.as_deref().filter(|_| res.mode == Mode::Branch).and_then(|a| crate::settings::data_at_secs(a, self.store.now_ms().unwrap_or(0) / 1000));
                let past = match at {
                    Some(at) => b.reset_at(&res.parent, branch, res.kept.as_deref(), name, res.mode, at).await.map_err(|e| anyhow::anyhow!("resetting {key} as of {at}: {e:#}"))?,
                    None => None,
                };
                let fresh = match past {
                    Some(f) => f,
                    None => b.reset(&res.parent, branch, res.kept.as_deref(), name, res.mode).await.map_err(|e| anyhow::anyhow!("resetting {key}: {e:#}"))?,
                };
                self.store.set_resource(&r.id, res.kind, &res.parent, |x| {
                    x.r#ref = Some(fresh.r#ref.clone());
                    x.kept = fresh.kept.clone();
                    x.state = ResourceState::Resetting;
                    x.error = None;
                    x.bytes = 0;
                })?;
                self.store.log(&r.id, &format!("reset {key} from its parent"))?;
                self.store.set_seed_needed(&r.id, true)?;
                self.mask(&r.id, &b, &key, &fresh.r#ref, s).await?;
                self.store.set_resource(&r.id, res.kind, &res.parent, |x| x.state = ResourceState::Ready)?;
                reset = true;
            }
            // Made (or reset) but not finished: masks again, then Ready.
            Some(branch) if res.state != ResourceState::Ready => {
                self.mask(&r.id, &b, &key, branch, s).await?;
                self.store.set_resource(&r.id, res.kind, &res.parent, |x| {
                    x.state = ResourceState::Ready;
                    x.error = None;
                })?;
            }
            // Recorded without credentials (from before its product handed any out): get them,
            // so its services never need the product's own.
            Some(branch) if res.kept.is_none() => {
                if let Some(kept) = b.credentials(&res.parent, branch, name).await.map_err(|e| anyhow::anyhow!("credentials for {key}: {e:#}"))? {
                    self.store.set_resource(&r.id, res.kind, &res.parent, |x| x.kept = Some(kept.clone()))?;
                    self.store.log(&r.id, &format!("{key} has credentials of its own now"))?;
                }
            }
            Some(_) => {}
        }
        Ok((reset, None))
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
                return self.await_approval(r).await;
            }
        } else if !self.git_head_may_run(r)? {
            return self.await_approval(r).await;
        }
        if r.state == State::Queued {
            self.store.update(&r.id, |row| row.next_at = if row.generation == r.generation { SETTLED } else { 0 })?;
            return Ok(());
        }
        if r.state == State::AwaitingApproval {
            self.store.update(&r.id, |row| row.state = State::Creating)?;
            self.store.log(&r.id, "approved: starting")?;
        }
        // A head that may run: where the project's `teardown` runs when the preview goes.
        if self.store.ran_head(&r.id)?.as_deref() != Some(r.build()) {
            self.store.set_ran_head(&r.id, Some(r.build()))?;
        }
        let project = self.workspace.get(roll_workspace::Scope::All, &r.project_id)?;
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
        let reset_of = |x: &ResourceRow| x.r#ref.is_some() && (reset_due || asked.iter().any(|a| a == "*" || *a == format!("{}:{}", x.kind.as_str(), x.parent)));
        // A masked Postgres resource is reset, or masked with changed rules, by branching it again
        // (a new endpoint, masked before any service is pointed at it), never rewritten in place:
        // anything still connected would read the unmasked rows until the mask commits, and the
        // preview's code may have left triggers or functions in it that a superuser's rewrite runs.
        let mut remask = false;
        for res in self.store.resources(&r.id)? {
            let masked = mask_digest(&s, &res) != mask_digest(&Settings::default(), &res);
            let rules_moved = res.r#ref.is_some() && res.state == ResourceState::Ready && self.store.mask_digest(&r.id, res.kind, &res.parent)?.unwrap_or_default() != mask_digest(&s, &res);
            if res.kind == Kind::Postgres && ((masked && reset_of(&res)) || rules_moved) {
                self.delete_branch(&r.id, &res, &name).await?;
                self.store.drop_resource(&r.id, res.kind, &res.parent)?;
                self.store.log(&r.id, &format!("postgres:{} is masked: branched again", res.parent))?;
                reset_any = true;
                remask = true;
            } else if rules_moved {
                // A read policy (NoSQL): changed in place, nothing serving meanwhile.
                self.store.set_resource(&r.id, res.kind, &res.parent, |x| x.state = ResourceState::Pending)?;
                self.store.log(&r.id, &format!("{}:{}: the masking rules changed: masking again", res.kind.as_str(), res.parent))?;
                remask = true;
            }
        }
        self.store.want_resources(&r.id, &wanted)?;
        // Nothing serves data while it's replaced (and until its seed and masks have run again).
        let resetting = remask || self.store.resources(&r.id)?.iter().any(reset_of);
        if resetting {
            for svc in self.store.services(&r.id)? {
                self.apps.sleep(&r.id, &svc.name).await;
                self.store.set_service(&r.id, &svc.name, ServiceState::Sleeping, None, None)?;
            }
        }
        // Every resource at once (a Postgres branch doesn't wait for the bucket's): each records its
        // own progress as it goes, and the first error is the converge's once all are done.
        let results = futures::future::join_all(self.store.resources(&r.id)?.into_iter().map(|res| self.converge_resource(r, &s, &name, res, reset_due, &asked))).await;
        let mut first_error = None;
        for result in results {
            match result {
                Ok((reset, problem)) => {
                    reset_any |= reset;
                    problems.extend(problem);
                }
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
        if let Some(e) = first_error {
            return Err(e);
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
            // Nothing keeps running with what it may no longer have.
            for svc in self.store.services(&r.id)? {
                self.apps.sleep(&r.id, &svc.name).await;
            }
            anyhow::bail!("this preview may not inherit the production environment {:?}", s.secrets.base);
        }
        let mut vars = self.preview_vars(r, &s).await?;
        let branches = self.branch_vars(&r.id).await?;
        // A link of the base to a parent the preview branched, under a name of its own
        // (`STAGING_PGPASS`): set from the branch as the environment's own key, so whoever
        // resolves it (roll apps too, which reads the environment itself) gets the branch's value,
        // never the parent's. Read from the base's list (links, no values): nothing resolved.
        // Unreadable: the converge fails (tried again) rather than let roll apps resolve a
        // renamed link to the parent.
        let links = self.secrets.links(&r.project_id, &s.secrets.base).await.map_err(|e| anyhow::anyhow!("secrets: reading the base's links: {e:#}"))?;
        for (key, kind, parent, field) in links {
            if vars.iter().any(|(k, _)| *k == key) {
                continue;
            }
            if let Some((_, _, own)) = branches.iter().find(|(k, p, _)| *k == kind && *p == parent) {
                vars.push((key, crate::names::follow(kind, &field, own.clone())));
            }
        }
        // Link access hands the backend the link's token; team access has it let in people
        // signed in to roll instead. The bypass goes with either.
        let access = self.store.access(&r.id)?;
        let access_token = access.as_ref().and_then(|a| a.link_token().map(str::to_string));
        let signed_in = access.as_ref().is_some_and(|a| a.mode == crate::settings::Access::Team);
        let bypass = access.and_then(|a| a.bypass).filter(|_| access_token.is_some() || signed_in);
        // Every converge (at least every few minutes, for usage) pushes its expiry out: only a
        // preview nobody converges any more lets it lapse.
        let trusted = s.secrets.allow_production && r.trusted;
        let expires_at = self.store.now_ms()? + SECRETS_ENV_MARGIN_MS;
        // What it branched: roll secrets resolves nothing of those parents' (`followed`).
        let branched: Vec<(Kind, String)> = self.store.resources(&r.id)?.into_iter().map(|x| (x.kind, x.parent)).collect();
        self.secrets.ensure(&r.project_id, &env_name(&r.id), &s.secrets.base, &vars, trusted, expires_at, &branched).await.map_err(|e| anyhow::anyhow!("secrets: {e:#}"))?;
        // Its logs' masks follow what it's given now: these values at once (roll apps' instances
        // get them without a start here), roll secrets' read again at the next logs read.
        let own: Vec<(String, String)> = project_env(&s, r.branch.as_deref()).into_iter().chain(self.store.vars(&r.id)?).collect();
        self.add_masks(&r.id, super::masks::secret_values(&[], &[], &own, &vars));
        self.stale_masks(&r.id);

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
            && self.store.released(&r.id)?.as_deref() != Some(r.build())
        {
            let started = std::time::Instant::now();
            self.run_job(r, &s, &vars, "release", release).await?;
            let ms = started.elapsed().as_millis() as i64;
            self.store.set_released(&r.id, r.build(), ms)?;
            self.store.log(&r.id, &format!("released in {:.1} s", ms as f64 / 1000.0))?;
        }
        // 3d. What that did to the schema, once per head (best effort: a reviewer's aid). Only
        // after a seed or release could have changed it: reading the parent's starts its compute.
        if s.seed.is_none() && s.release.is_none() {
            self.store.clear_schema_changes(&r.id)?;
        } else if self.store.schema_head(&r.id)?.as_deref() != Some(r.build()) {
            self.read_schema(r).await;
        }

        // 4. Services: the change's repo at its head, every other repo at its default head.
        // From roll apps' catalog when the project has services there, else the repos'
        // `roll.toml`.
        // Only repos the project stands for (`Workspace::trusts`): they run with its secrets.
        let repos: Vec<String> = project.resources.iter().filter(|x| x.kind == roll_workspace::Kind::Repo && self.workspace.trusts(&project, x.kind, &x.r#ref)).map(|x| x.r#ref.clone()).collect();
        // (spec, repo, revision, whether roll apps runs it)
        let mut wanted: Vec<(ServiceSpec, String, String, bool)> = Vec::new();
        let head_of = |repo: String| async move {
            if repo == r.repo {
                Ok::<_, anyhow::Error>(Some(r.build().to_string()))
            } else if repo.starts_with("git:") {
                // Another Git URL service of the project: its own (production's) build.
                Ok(Some(String::new()))
            } else {
                self.head_for(&repo, r.branch.as_deref(), None).await
            }
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
                    let spec = ServiceSpec { name: c.name.clone(), image: String::new(), run: String::new(), port: 0, memory_mb: c.memory_mb, cpus: 0.0, env: Vec::new(), health: None, states: Vec::new(), pages: Vec::new(), hide: Vec::new() };
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
        let mut still_deploying = false;
        let mut checks: Vec<(String, String, String)> = Vec::new();
        let mut deployed: Vec<String> = Vec::new();
        // Rebuilds a deploy hook asked for: each one is a deploy of every service, built afresh.
        let rebuilds = self.store.rebuilds(&r.id)?;
        let rebuilt = if rebuilds > 0 { format!("#r{rebuilds}") } else { String::new() };

        // (what's sent, its fingerprint, the revision, whether it's the change's own repo)
        let mut to_deploy: Vec<(Target, String, String, bool)> = Vec::new();

        // The project's own preview domain, when it has one (`Store::set_project_domain`).
        let domain = self.store.project_domain(&r.project_id)?;
        for (spec, repo, rev, on_apps) in wanted {
            keep.push(spec.name.clone());
            if let Some(path) = &spec.health {
                checks.push((spec.name.clone(), rev.clone(), path.clone()));
            }
            // What it costs is what it runs with: a backend's preview size when it has one.
            let memory_mb = if on_apps { self.apps.memory_mb(&r.id, &spec.name).await.unwrap_or(spec.memory_mb) } else { spec.memory_mb };
            let svc = self.store.want_service(&r.id, &spec.name, &repo, &rev, memory_mb, &|h| self.cfg.url_in(domain.as_ref(), h))?;
            // A redeploy when the revision moved, or what it's deployed with did (its variables:
            // a reset's new URLs, changed overrides; on roll apps its access): the runtime
            // restarts it on its next request, roll apps redeploys it.
            // Access and the bypass only matter to a backend that checks access itself (roll
            // apps): the runtime's gate reads them on each request, so turning a project public or
            // private, or a rotation, restarts nothing there (nor cuts a screenshot short).
            // The feedback button: the runtime's proxy adds it per request; roll apps at deploy.
            let feedback = s.feedback && r.change_id.is_some() && access_token.is_some();
            // Egress too: a service moves onto (or off) the preview's own network by restarting.
            let fingerprint = config_fingerprint(&vars, (access_token.is_some() || signed_in) && on_apps, bypass.as_deref().filter(|_| on_apps), trusted, feedback && on_apps)
                + if signed_in && on_apps { "s" } else { "" }
                + &if s.egress_for(r.trusted) == crate::settings::EgressRule::Roll { format!("e{}", s.egress_hosts.join(",")) } else { String::new() }
                + &rebuilt;
            let was = self.store.service_config(&r.id, &spec.name)?;
            let config_moved = was.as_deref() != Some(fingerprint.as_str());
            // Moved only by a rebuild (or with one): the same head built again, nothing cached.
            let fresh = rebuilds > 0 && was.as_deref().and_then(|w| w.rsplit_once("#r")).is_none_or(|(_, n)| n != rebuilds.to_string());
            // The head it ran stays reachable at a URL of its own (Vercel's per-push URLs): the
            // change's own services on the runtime, a trusted change's.
            if !on_apps
                && repo == r.repo
                && r.trusted
                && let Some(old) = svc.deployed.as_deref().filter(|d| *d != rev && !d.is_empty())
            {
                for gone in self.store.pin(&r.id, &spec.name, old, &|h| self.cfg.url_in(domain.as_ref(), h))? {
                    self.apps.remove(&r.id, &gone).await.map_err(|e| anyhow::anyhow!("removing {gone}: {e:#}"))?;
                }
            }
            // A deploy the backend is still on is asked about again (until it's live or fails).
            let in_flight = self.deploying_since.lock().contains_key(&(r.id.clone(), spec.name.clone()));
            if svc.deployed.as_deref() != Some(&rev) || reset_any || config_moved || in_flight {
                // A Git URL service's revision is its git commit already.
                let git = if repo.is_empty() || rev.is_empty() {
                    None
                } else if repo.starts_with("git:") {
                    Some(rev.clone())
                } else {
                    self.source.git_commit(&repo, &rev).await?
                };
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
                    signed_in,
                    bypass_sha256: bypass.clone(),
                    feedback,
                    egress: s.egress_for(r.trusted),
                    egress_hosts: s.egress_hosts.clone(),
                    rebuild: fresh.then_some(rebuilds),
                };
                self.store.set_service(&r.id, &spec.name, ServiceState::Deploying, None, None)?;
                to_deploy.push((t, fingerprint, rev.clone(), repo == r.repo));
            }
        }
        // Every deploy at once (on roll apps each waits for its rollout), their outcomes in order.
        let mut failed: Option<String> = None;
        let outcomes = futures::future::join_all(to_deploy.iter().map(|(t, ..)| self.apps.deploy(t))).await;
        for ((t, fingerprint, rev, own), outcome) in to_deploy.into_iter().zip(outcomes) {
            let spec = &t.spec;
            match outcome {
                Ok(url) => {
                    self.deploying_since.lock().remove(&(r.id.clone(), spec.name.clone()));
                    if let Some(url) = url {
                        self.store.set_service_url(&r.id, &spec.name, &url)?;
                    }
                    let state = self.apps.status(&r.id, &spec.name).unwrap_or(ServiceState::Sleeping);
                    self.store.set_service(&r.id, &spec.name, state, Some(&rev), None)?;
                    self.store.set_service_config(&r.id, &spec.name, &fingerprint)?;
                    // A new deploy is checked again, even at the same revision.
                    self.store.set_healthy(&r.id, &spec.name, "")?;
                    self.store.log(&r.id, &format!("deployed {} at {}", spec.name, short(&rev)))?;
                    if own {
                        deployed.push(spec.name.clone());
                    }
                }
                // Accepted, not live yet: no slot waits on a build; come back and ask again.
                Err(e) if e.downcast_ref::<crate::ports::StillDeploying>().is_some() => {
                    let now = self.store.now_ms()?;
                    let since = *self.deploying_since.lock().entry((r.id.clone(), spec.name.clone())).or_insert(now);
                    if now - since > DEPLOY_WAIT_MS {
                        self.deploying_since.lock().remove(&(r.id.clone(), spec.name.clone()));
                        let msg = format!("{e:#}, for over {} minutes", DEPLOY_WAIT_MS / 60_000);
                        self.store.set_service(&r.id, &spec.name, ServiceState::Failed, None, Some(&msg))?;
                        failed.get_or_insert(format!("deploying {}: {msg}", spec.name));
                        continue;
                    }
                    self.store.set_service(&r.id, &spec.name, ServiceState::Deploying, None, None)?;
                    // The backend has what this deploy sent (its variables): a change made
                    // meanwhile, even one back to how it was before, is sent again.
                    self.store.set_service_config(&r.id, &spec.name, &fingerprint)?;
                    still_deploying = true;
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    self.store.set_service(&r.id, &spec.name, ServiceState::Failed, None, Some(&msg))?;
                    failed.get_or_insert(format!("deploying {}: {msg}", spec.name));
                }
            }
        }
        // Every outcome is kept first: a failed deploy doesn't lose the others' (which would be
        // deployed again for nothing).
        if let Some(e) = failed {
            anyhow::bail!(e);
        }
        if still_deploying {
            let again = self.store.now_ms()? + DEPLOY_POLL_MS;
            self.store.update(&r.id, |row| row.next_at = if row.generation == r.generation { again } else { 0 })?;
            return Ok(());
        }
        for svc in self.store.services(&r.id)? {
            if !keep.contains(&svc.name) {
                self.apps.remove(&r.id, &svc.name).await.map_err(|e| anyhow::anyhow!("removing {}: {e:#}", svc.name))?;
                self.store.drop_service(&r.id, &svc.name)?;
                self.store.log(&r.id, &format!("removed service {}", svc.name))?;
            }
        }
        // 4b. Health: a service with a `health` path counts once it answers 2xx/3xx there, asked
        // once per revision (Render's and Railway's checks). Anything else fails the preview, and
        // the retry asks again.
        // Asked all at once (a probe wakes a sleeping service: a cold start each), answered in order.
        let mut asked = Vec::new();
        for (name, rev, path) in checks {
            if self.store.healthy(&r.id, &name)?.as_deref() != Some(rev.as_str()) {
                asked.push((name, rev, path));
            }
        }
        let answers = futures::future::join_all(asked.iter().map(|(name, _, path)| self.apps.probe(&r.id, name, path))).await;
        for ((name, rev, path), answer) in asked.into_iter().zip(answers) {
            let answer = match answer {
                Ok(a) => a,
                // Its start was overtaken (slept, redeployed, pushed): converge again soon.
                Err(e) if e.downcast_ref::<crate::ports::StillDeploying>().is_some() => {
                    let now = self.store.now_ms()?;
                    let key = (r.id.clone(), format!("{name} health"));
                    let since = *self.deploying_since.lock().entry(key.clone()).or_insert(now);
                    if now - since > DEPLOY_WAIT_MS {
                        self.deploying_since.lock().remove(&key);
                        anyhow::bail!("{name} didn't answer {path} for over {} minutes ({e:#})", DEPLOY_WAIT_MS / 60_000);
                    }
                    let again = now + DEPLOY_POLL_MS;
                    self.store.update(&r.id, |row| row.next_at = if row.generation == r.generation { again } else { 0 })?;
                    return Ok(());
                }
                Err(e) => anyhow::bail!("{name} didn't answer {path}: {e:#}"),
            };
            self.deploying_since.lock().remove(&(r.id.clone(), format!("{name} health")));
            match answer {
                Some((status, _)) if (200..400).contains(&status) => {
                    self.store.set_healthy(&r.id, &name, &rev)?;
                    self.store.log(&r.id, &format!("{name} is healthy ({path}: {status})"))?;
                }
                // Its answer is its code's words, and becomes the preview's error (the pull
                // request's comment): masked as its logs are.
                Some((status, said)) => {
                    // Masked whole, then cut: a value the cut splits isn't left half shown.
                    let said: String = code_preview::runner::mask(&said, &self.masks(&r.id).await?).chars().take(200).collect();
                    anyhow::bail!("{name} answered {status} on {path}: {said}")
                }
                None => {}
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
        let ready = row.state == State::Ready;
        self.report(row).await;
        // 5b. Warm: the change's own services just deployed are woken after the pass, so the first
        // visit after a push doesn't wait for a cold start.
        if ready && s.warm && r.change_id.is_some() && !r.asleep {
            self.warm_wanted.lock().extend(deployed.into_iter().map(|svc| (r.id.clone(), svc)));
        }
        // 6. A screenshot of the first service, once per head (best effort; it wakes the service):
        // taken after the pass, never in a converge slot.
        if ready && s.github.screenshot && self.store.screenshot_head(&r.id)?.as_deref() != Some(r.build()) {
            self.shots_wanted.lock().insert(r.id.clone());
        }
        Ok(())
    }

    /// The project's column masks for a fresh branch, if it has any for this resource.
    /// The first service's screenshot at `r`'s head: `true` when one was kept. Not taken (a
    /// backend that can't, a failure) is remembered for the head, and the last one taken stays.
    pub(super) async fn take_screenshot(&self, r: &Row) -> bool {
        let Ok(Some(first)) = self.store.services(&r.id).map(|v| v.into_iter().next()) else { return false };
        let (mut entries, mut hide, widths, openapi, endpoints, mut states, browsers) = self
            .store
            .settings(&r.project_id)
            .map(|s| (s.github.pages, s.github.hide, s.github.widths, s.github.openapi, s.github.endpoints, s.github.states, s.github.browsers))
            .unwrap_or_default();
        // What the first service's own repo says its screenshots take, at the revision it runs
        // (versioned with the code it names), joined with the project's (a state name the
        // project has wins): a service on roll apps from its `[app.previews.screenshots]` (roll
        // apps describes it), else its `[preview]`.
        let on_apps = self.apps.catalog(&r.project_id).await.ok().flatten().and_then(|c| c.into_iter().find(|s| s.name == first.name));
        let from_repo = match (first.revision.as_deref(), on_apps) {
            (Some(rev), Some(svc)) => match svc.repo.as_deref() {
                Some(repo) => match self.source.screenshots(repo, rev, &svc.root).await {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = self.store.log(&r.id, &format!("its roll.toml's [app.previews.screenshots] wasn't read: {e:#}"));
                        None
                    }
                },
                None => None,
            },
            (Some(rev), None) => self
                .source
                .services(&first.repo, rev)
                .await
                .ok()
                .and_then(|specs| specs.into_iter().find(|s| s.name == first.name))
                .map(|s| code_preview::config::Screenshots { pages: s.pages, hide: s.hide, states: s.states }),
            (None, _) => None,
        };
        if let Some(more) = from_repo {
            for st in more.states {
                if !states.iter().any(|s| s.name == st.name) {
                    states.push(st);
                }
            }
            for p in more.pages {
                if !entries.contains(&p) {
                    entries.push(p);
                }
            }
            for h in more.hide {
                if !hide.contains(&h) {
                    hide.push(h);
                }
            }
        }
        // The first page at the project's other widths: entries of their own (`@768`).
        let width_keys: Vec<String> = widths.iter().map(|w| format!("@{w}")).collect();
        // The first service's own pages go in its run; another service's in a run of its own.
        let mine = |svc: Option<&str>| svc.is_none_or(|s| s == first.name);
        let mut first_paths: Vec<String> = width_keys.clone();
        first_paths.extend(entries.iter().filter_map(|e| crate::settings::page_entry(e)).filter(|(s, p)| mine(*s) && *p != "/").map(|(_, p)| p.to_string()));
        // The project's page states (in the first service's run) and other browsers (a run of
        // their own), each a page of its own.
        let own_keys: Vec<String> = states.iter().map(|s| format!("state:{}", s.name)).chain(browsers.iter().map(|b| format!("browser:{b}"))).collect();
        first_paths.extend(own_keys.iter().filter(|k| k.starts_with("state:")).cloned());
        // The open notes' elements not cropped yet, on the first service's pages (a note's link
        // is the preview's first URL and its path): what each note is about, as a picture.
        let crops = self.crops_wanted(r).await;
        let opts = crate::ports::ShotOptions { pages: first_paths, hide: hide.clone(), openapi, endpoints, states, browsers, crops };
        let started = std::time::Instant::now();
        // The page's words are its code's: what it was given is masked before they're kept (and
        // reach the pull request's comment); a run whose masks can't be read fails as a run.
        let taken = match self.apps.screenshot(&r.id, &first.name, &opts).await {
            Ok(Some(mut shot)) => self.mask_shot(&r.id, &mut shot).await.map(|()| Some(shot)).map_err(|e| anyhow::anyhow!("reading what its page mustn't show: {e}")),
            other => other,
        };
        match taken {
            Ok(Some(shot)) => {
                // Kept under the entry that asked for each (`/pricing`, or `web:/pricing`).
                let mut pages: Vec<crate::ports::PageShot> = shot
                    .pages
                    .iter()
                    .filter_map(|p| {
                        if width_keys.contains(&p.path) || own_keys.contains(&p.path) {
                            return Some(p.clone());
                        }
                        let entry = entries.iter().find(|e| crate::settings::page_entry(e).is_some_and(|(s, path)| mine(s) && path == p.path))?;
                        Some(crate::ports::PageShot { path: entry.clone(), ..p.clone() })
                    })
                    .collect();
                // `<first>:/` is the screenshot itself (which service is first isn't the person's to know).
                if let Some(e) = entries.iter().find(|e| crate::settings::page_entry(e) == Some((Some(first.name.as_str()), "/"))) {
                    let errors = shot.health.as_ref().map_or(0, |h| h.errors + h.failed);
                    let report = crate::PageReport { a11y: shot.a11y.clone(), links: shot.links.clone(), text: shot.text.clone(), unstable: shot.health.as_ref().is_some_and(|h| h.unstable), step: None };
                    pages.push(crate::ports::PageShot { path: e.clone(), png: shot.png.clone(), errors, report });
                }
                pages.extend(self.other_services_pages(r, &first.name, &entries, &hide).await);
                // Keeping it compares PNGs (with the push before, every page): CPU work, off the
                // runtime's workers.
                // Another browser's page only when this run took it: a failed browsers run leaves
                // no older picture behind to be compared and reviewed as this head's.
                let took = |k: &String| !k.starts_with("browser:") || pages.iter().any(|p| p.path == *k);
                let wanted: Vec<String> = entries.iter().cloned().chain(width_keys.iter().cloned()).chain(own_keys.iter().filter(|k| took(k)).cloned()).collect();
                for (note, png) in &shot.crops {
                    if let Err(e) = self.store.set_note_crop(&r.id, note, png) {
                        warn!(preview = %r.id, error = %e, "previews: keeping a note's crop");
                    }
                }
                let (store, id, head) = (self.store.clone(), r.id.clone(), r.build().to_string());
                // One transaction: a comparison never sees the picture without its checks and pages.
                let kept = tokio::task::spawn_blocking(move || store.keep_shot(&id, &head, &shot, &pages, &wanted).is_ok())
                .await
                .unwrap_or(false);
                if kept && r.change_id.is_some() {
                    self.compare_with_default_branch(r).await;
                    let threshold = self.store.settings(&r.project_id).map_or(0.0, |s| s.github.visual_threshold);
                    if self.store.carry_acceptance(&r.id, threshold).unwrap_or(false) {
                        let _ = self.store.log(&r.id, "its pages look exactly as when they were accepted: the acceptance carries over");
                    }
                } else if kept {
                    // A branch's preview may be a baseline: its screenshots are kept by head.
                    if let Err(e) = self.store.keep_base_history(&r.id, r.build()) {
                        warn!(preview = %r.id, error = %e, "previews: keeping the screenshot's history");
                    }
                    self.compare_waiting(r).await;
                }
                if kept {
                    Counters::add(&self.counters.screenshots);
                    self.shots_failed.lock().retain(|(p, _)| *p != r.id);
                    let _ = self.store.log(&r.id, &format!("took a screenshot of {} in {:.1} s", first.name, started.elapsed().as_secs_f64()));
                }
                kept
            }
            Ok(None) => {
                let _ = self.store.set_screenshot_tried(&r.id, r.build());
                false
            }
            Err(e) => {
                Counters::add(&self.counters.screenshot_failures);
                let first_try = self.shots_failed.lock().insert((r.id.clone(), r.build().to_string()));
                let again = if first_try { " (trying again)" } else { "" };
                let _ = self.store.log(&r.id, &format!("couldn't take a screenshot of {}{again}: {e:#}", first.name));
                if first_try {
                    self.shots_wanted.lock().insert(r.id.clone());
                } else {
                    let _ = self.store.set_screenshot_tried(&r.id, r.build());
                }
                false
            }
        }
    }

    /// The open notes of a change's preview without a crop, whose page and selector pass roll's
    /// checks (they come from the preview's code), at most [`crate::store::CROPS_PER_RUN`].
    async fn crops_wanted(&self, r: &Row) -> Vec<crate::ports::CropAsk> {
        if r.change_id.is_none() {
            return Vec::new();
        }
        let notes = match self.open_notes(&r.id).await {
            Ok(n) => n,
            Err(e) => {
                warn!(preview = %r.id, error = %e, "previews: reading the notes to crop");
                return Vec::new();
            }
        };
        notes
            .into_iter()
            .filter(|n| !n.crop && crate::settings::page_path_ok(&n.path) && crate::settings::selector_ok(&n.selector))
            .take(crate::store::CROPS_PER_RUN)
            .map(|n| crate::ports::CropAsk { note: n.id, path: n.path, selector: n.selector })
            .collect()
    }

    /// The pages `github.pages` names on the preview's other services (`admin:/`, `admin:/users`),
    /// a screenshot run per service in that service's own network. Best effort: a service that
    /// can't be taken is logged and its pages wait for the next head.
    async fn other_services_pages(&self, r: &Row, first: &str, entries: &[String], hide: &[String]) -> Vec<crate::ports::PageShot> {
        let mut by_service: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (svc, path) in entries.iter().filter_map(|e| crate::settings::page_entry(e)) {
            if let Some(svc) = svc.filter(|s| *s != first) {
                by_service.entry(svc).or_default().push(path);
            }
        }
        let services = self.store.services(&r.id).unwrap_or_default();
        let mut out = Vec::new();
        for (svc, paths) in by_service {
            let Some(service) = services.iter().find(|s| s.name == svc) else { continue };
            // Only the pages a push can change are taken again (Chromatic's TurboSnap): a service
            // that runs what it ran last time (its revision, what it's deployed with, the same
            // data) keeps its pages, checks and all, without a browser run.
            let key = service.deployed.as_ref().map(|rev| format!("{rev}:{}:{}", self.store.service_config(&r.id, svc).ok().flatten().unwrap_or_default(), r.last_reset_at.unwrap_or(0)));
            if let Some(key) = &key
                && self.store.service_shot_key(&r.id, svc).ok().flatten().as_deref() == Some(key.as_str())
                && let Some(kept) = paths.iter().map(|p| self.store.kept_page(&r.id, &format!("{svc}:{p}")).ok().flatten()).collect::<Option<Vec<_>>>()
            {
                let _ = self.store.log(&r.id, &format!("kept {svc}'s pages: it runs what it ran at the push before"));
                out.extend(kept);
                continue;
            }
            let opts = crate::ports::ShotOptions { pages: paths.iter().filter(|p| **p != "/").map(|p| p.to_string()).collect(), hide: hide.to_vec(), ..Default::default() };
            match self.apps.screenshot(&r.id, svc, &opts).await {
                Ok(Some(mut shot)) => {
                    if let Err(e) = self.mask_shot(&r.id, &mut shot).await {
                        let _ = self.store.log(&r.id, &format!("{svc}'s pages weren't kept: {e}"));
                        continue;
                    }
                    if let Some(key) = &key {
                        let _ = self.store.set_service_shot_key(&r.id, svc, key);
                    }
                    if paths.contains(&"/") {
                        let errors = shot.health.as_ref().map_or(0, |h| h.errors + h.failed);
                        let report = crate::PageReport { a11y: shot.a11y.clone(), links: shot.links.clone(), text: shot.text.clone(), unstable: shot.health.as_ref().is_some_and(|h| h.unstable), step: None };
                        out.push(crate::ports::PageShot { path: format!("{svc}:/"), png: shot.png.clone(), errors, report });
                    }
                    out.extend(shot.pages.into_iter().map(|p| crate::ports::PageShot { path: format!("{svc}:{}", p.path), ..p }));
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = self.store.log(&r.id, &format!("couldn't take {svc}'s pages: {e:#}"));
                }
            }
        }
        out
    }

    /// A change's screenshot against the project's preview of its repo's default branch, when it
    /// has one with a screenshot (best effort).
    async fn compare_with_default_branch(&self, r: &Row) {
        let Ok(Some(default)) = self.source.default_branch(&r.repo).await else { return };
        let Ok(rows) = self.store.rows(Some(&r.project_id)) else { return };
        let rows: Vec<Row> = rows.into_iter().filter(|x| x.change_id.is_none() && x.desired == Desired::Active).collect();
        // The repo's own default branch, else the project's baseline (of another of its repos).
        let own = rows.iter().find(|x| x.repo == r.repo && x.branch.as_deref() == Some(default.as_str()));
        let Some(base) = own.or_else(|| rows.iter().find(|x| x.is_baseline())) else { return };
        let Some(branch) = base.branch.as_deref() else { return };
        // Where the change left the branch (Chromatic's ancestor build), when roll code knows.
        let at = self.source.merge_base(&r.repo, r.build()).await.ok().flatten();
        // Decoding and comparing PNGs: off the runtime's workers.
        let (store, id, base_id, branch) = (self.store.clone(), r.id.clone(), base.id.clone(), branch.to_string());
        match tokio::task::spawn_blocking(move || store.compare_with_base(&id, &base_id, &branch, at.as_deref())).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(preview = %r.id, error = %e, "previews: comparing with the default branch's"),
            Err(e) => warn!(preview = %r.id, error = %e, "previews: comparing with the default branch's"),
        }
    }

    /// After branch preview `b`'s screenshot: the project's changes not compared with the default
    /// branch yet (their screenshots came first, as a new baseline's does) are now, and say so.
    async fn compare_waiting(&self, b: &Row) {
        let Ok(rows) = self.store.rows(Some(&b.project_id)) else { return };
        // Every change again, not only those not compared yet: a change's screenshot run may
        // have compared it while this baseline's was half kept (its first page in, its other
        // pages not yet: screenshots run side by side), leaving pages uncompared for good. At the
        // same merge base the answer is the same, so only a change whose comparison moved is
        // reported.
        let compared = |id: &str| {
            self.store.row(id).and_then(|r| self.store.view(r)).ok().map(|p| {
                let pages: Vec<(String, Option<f64>)> = p.pages.iter().map(|x| (x.path.clone(), x.base_percent)).collect();
                serde_json::to_string(&(&p.base_change, pages)).unwrap_or_default()
            })
        };
        for r in rows.into_iter().filter(|x| x.change_id.is_some() && x.desired == Desired::Active) {
            let before = compared(&r.id);
            self.compare_with_default_branch(&r).await;
            if compared(&r.id) != before {
                self.report(r).await;
            }
        }
    }

    /// Run the project's seed or release to its end on the preview's tree: its secrets
    /// environment (as services get it: untrusted unless the project allows production and the
    /// change is trusted), the branches and overrides on top.
    /// What the preview's secrets environment and jobs get: the project's `env`, the preview's own
    /// variables, the mail catcher's settings, and the branches' URLs over them all.
    async fn preview_vars(&self, r: &Row, s: &Settings) -> anyhow::Result<Vec<(String, String)>> {
        let urls = self.resource_env(&r.id).await?;
        let mut vars: Vec<(String, String)> = project_env(s, r.branch.as_deref());
        for (k, v) in self.store.vars(&r.id)? {
            set(&mut vars, &k, v);
        }
        // A deploy elsewhere (roll apps) never emails real people either.
        if s.mail == crate::settings::MailRule::Catch {
            self.catch_mail_vars(&r.id, &mut vars)?;
        }
        for (k, v) in urls {
            set(&mut vars, &k, v);
        }
        Ok(vars)
    }

    async fn run_job(&self, r: &Row, s: &Settings, vars: &[(String, String)], what: &str, job: &crate::settings::Seed) -> anyhow::Result<()> {
        let untrusted = !(s.secrets.allow_production && r.trusted);
        // Its dynamic credentials last as long as the job does.
        let (mut env, leases, plain) = self.resolve_secrets(r, untrusted, true, what).await?;
        // Its output becomes the preview's error (a teardown's, the pull request's comment): what
        // it was given is masked there, as checks' output is.
        let own: Vec<(String, String)> = project_env(s, r.branch.as_deref()).into_iter().chain(self.store.vars(&r.id)?).collect();
        let hide = super::masks::secret_values(&env, &plain, &own, vars);
        self.add_masks(&r.id, hide.clone());
        for (k, v) in vars {
            set(&mut env, k, v.clone());
        }
        set(&mut env, "ROLL_PREVIEW", r.id.clone());
        set(&mut env, "ROLL_REPO", r.repo.clone());
        set(&mut env, "ROLL_CHANGESET", r.build().to_string());
        let run = crate::ports::SeedJob {
            preview: r.id.clone(),
            repo: r.repo.clone(),
            revision: r.build().to_string(),
            seed: job.clone(),
            env,
            egress: s.egress_for(r.trusted),
            egress_hosts: s.egress_hosts.clone(),
        };
        self.store.log(&r.id, &format!("{what}: {}", job.run.lines().next().unwrap_or("")))?;
        let ran = self.apps.seed(&run).await;
        for l in &leases {
            if let Err(e) = self.secrets.revoke(&r.project_id, l).await {
                warn!(preview = %r.id, lease = %l, error = %e, "previews: revoking a job's dynamic credential (it expires by itself)");
            }
        }
        let (code, tail) = ran.map_err(|e| anyhow::anyhow!("{what}: {}", code_preview::runner::mask(&format!("{e:#}"), &hide)))?;
        self.drain_job(&r.project_id, &r.id, what, &code_preview::runner::mask(&tail, &hide)).await;
        if code != 0 {
            anyhow::bail!("the {what} command failed with exit code {code}\n\n{}", code_preview::runner::mask(tail.trim_end(), &hide));
        }
        Ok(())
    }

    /// The Postgres branches' schema, and the search branches' mappings, against their parents'
    /// (`schema::diff`), kept for `head`. A failure is logged and read again on the next converge.
    async fn read_schema(&self, r: &Row) {
        let read = async {
            let mut all: Option<Vec<String>> = None;
            // Every line, uncapped, for the risks: the shown lines stop at 200.
            let mut every: Vec<String> = Vec::new();
            let resources = self.store.resources(&r.id)?;
            let has_schema = |k: Kind| matches!(k, Kind::Postgres | Kind::Search);
            let several = resources.iter().filter(|x| has_schema(x.kind)).count() > 1;
            for res in resources.iter().filter(|x| has_schema(x.kind)) {
                let Some(branch) = &res.r#ref else { continue };
                let Some((parent, mine)) = self.brancher(res.kind)?.schema(&res.parent, branch).await? else { continue };
                let lines = crate::schema::diff_all(&parent, &mine);
                let header = format!("# {}:{}", res.kind.as_str(), res.parent);
                if several && !lines.is_empty() {
                    every.push(header.clone());
                }
                every.extend(lines.iter().cloned());
                let out = all.get_or_insert_with(Vec::new);
                if several && !lines.is_empty() {
                    out.push(header);
                }
                out.extend(crate::schema::cap(lines));
            }
            anyhow::Ok((all.map(crate::schema::cap), crate::schema::risks(&every)))
        };
        match read.await {
            Ok((changes, risks)) => {
                if let Err(e) = self.store.set_schema_changes(&r.id, r.build(), changes.as_deref(), &risks) {
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
        let (kind, parent) = key.split_once(':').unwrap_or((key, ""));
        let kind = Kind::parse(kind)?;
        let digest = mask_digest_of(s.masking.get(key));
        if let Some(cols) = s.masking.get(key).filter(|c| !c.is_empty()) {
            let kept = self.store.resources(id)?.into_iter().find(|x| x.kind == kind && x.parent == parent).and_then(|x| x.kept);
            b.mask(parent, branch, kept.as_deref(), cols).await.map_err(|e| anyhow::anyhow!("masking {key}: {e:#}"))?;
            self.store.log(id, &format!("masked {} column(s) of {key}", cols.len()))?;
        }
        self.store.set_mask_digest(id, kind, parent, &digest)?;
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
            // With the names a roll secrets link to the parent would hand out, so the parent's
            // values under them don't survive the secrets layer (`names::with_standard`).
            for (k, v) in suffixed(crate::names::with_standard(kind, vars), suffix.as_deref()) {
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
        // The project's teardown, once, at a head the preview ran, while what it made is still
        // there. A failure is logged and the preview goes regardless (Heroku's `pr-predestroy`).
        if let Some(head) = self.store.ran_head(&r.id)? {
            let s = self.store.settings(&r.project_id)?;
            if let Some(job) = &s.teardown {
                let at = Row { runs: Some(head), ..r.clone() };
                let ran = async {
                    let vars = self.preview_vars(&at, &s).await?;
                    self.run_job(&at, &s, &vars, "teardown", job).await
                };
                if let Err(e) = ran.await {
                    let e = format!("{e:#}");
                    warn!(preview = %r.id, error = %e, "previews: the teardown failed");
                    self.store.log(&r.id, &e)?;
                    self.store.set_teardown_error(&r.id, &e)?;
                }
            }
            self.store.set_ran_head(&r.id, None)?;
        }
        for p in self.store.pinned(&r.id)? {
            let name = format!("{}@{}", p.service, p.revision);
            self.apps.remove(&r.id, &name).await.map_err(|e| anyhow::anyhow!("removing {name}: {e:#}"))?;
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
        // The change hears it went, and how its teardown failed if it did.
        let teardown = self.store.teardown_error(&r.id)?;
        self.source.forget(&view, teardown.as_deref()).await;
        // A Git URL service's pull request too, unless another project's preview of it speaks
        // now (it carries the comment on; saying this one went could land after it).
        let hands_over = matches!(self.refresh_voice(r), Ok((_, false)));
        if !hands_over && let Ok(s) = self.store.settings(&r.project_id) {
            self.report_on_git(&view, &s, Some(teardown.as_deref())).await;
        }
        // What it cost this month stays counted against the project's budget.
        self.store.add_spend(&r.project_id, crate::store::month_start(self.store.now_ms()?), view.cost.month_to_date_usd)?;
        self.store.remove(&r.id)?;
        if let Some(c) = self.cluster.get() {
            c.retire(&preview_key(&r.id));
        }
        self.usage_read.lock().remove(&r.id);
        self.secrets_told.lock().remove(&r.id);
        self.egress_told.lock().remove(&r.id);
        self.git_reporting.lock().remove(&r.id);
        self.forget_masks(&r.id);
        info!(preview = %r.id, "previews: deleted");
        Ok(())
    }

    pub(super) async fn report(&self, row: Row) {
        let Ok(s) = self.store.settings(&row.project_id) else { return };
        // Who speaks for a Git URL service's branch as of now: another project's preview of it
        // stays on its own sheet (`report_on_git`).
        let _ = self.refresh_voice(&row);
        if let Ok(mut p) = self.store.view(row) {
            // The open notes, newest first (a few), for the pull request's comment.
            if p.change.is_some() {
                let mut notes = self.open_notes(&p.id).await.unwrap_or_default();
                notes.reverse();
                notes.truncate(10);
                p.notes = notes.into_iter().map(|n| {
                    let link = if n.crop { self.store.note_crop_link(&p.id, &n.id).ok().flatten() } else { None };
                    (n, link)
                }).collect();
            }
            self.source.report(&p, &s).await;
            self.report_on_git(&p, &s, None).await;
        }
    }

    /// A Git URL service's preview on its repository (Vercel's status and comment): the
    /// `roll/preview` status on its head and, for a pull request's, the comment edited in place,
    /// each posted only when it changed (what was posted is kept, so a restart posts nothing new).
    /// `deleted`: the preview went (its comment says so). Best effort: a failure is logged and
    /// tried at the next report.
    pub(super) async fn report_on_git(&self, p: &Preview, s: &crate::Settings, deleted: Option<Option<&str>>) {
        let (Some(gr), Some(service)) = (self.git_report.get(), p.repo.strip_prefix("git:")) else { return };
        // Another project speaks for the service: its own preview posts (and says it went).
        if p.reported_by.is_some() {
            return;
        }
        // One report at a time per preview (what's posted, and the comment's id, read and kept
        // in turn).
        let turn = self.git_reporting.lock().entry(p.id.clone()).or_default().clone();
        let _turn = turn.lock().await;
        // Only a project that stands for the service speaks for it on its repository (anyone may
        // put any service on their own canvas: `Workspace::trusts`).
        let stands = self.workspace.get(roll_workspace::Scope::All, &p.project).is_ok_and(|project| self.workspace.trusts(&project, roll_workspace::Kind::Service, service));
        if !stands {
            return;
        }
        let Some(mut body) = self.source.git_report_body(p, s, deleted) else { return };
        // A fork's pull request waits for a writer in roll's sheet (not roll code's
        // `approve-run`), and nothing of it is linked: it hasn't run.
        if deleted.is_none() && p.state == State::AwaitingApproval {
            body.description = "Waiting for a writer to approve running this fork (Approve and run, in roll)".into();
            body.target_url = None;
        }
        let Ok((last_status, comment_id, last_hash)) = self.store.git_report(&p.id) else { return };
        // The project's own switches, as for a roll code change's (`github.status`, `.comment`).
        if let Some(state) = body.state.as_ref().filter(|_| s.github.status) {
            let key = format!("{}:{state}:{}", p.head, body.description);
            if last_status.as_deref() != Some(key.as_str()) {
                match gr.status(service, &p.head, state, &body.description, body.target_url.as_deref()).await {
                    Ok(true) => {
                        let _ = self.store.set_git_report(&p.id, Some(&key), None);
                    }
                    Ok(false) => return,
                    Err(e) => tracing::warn!(preview = %p.id, error = %e, "previews: a Git URL preview's status"),
                }
            }
        }
        let Ok(Some(pull)) = self.store.git_pull(&p.id) else { return };
        if !s.github.comment {
            return;
        }
        // Its repository, as roll apps named it in the pull request's page: the comment's number
        // and id are that repository's, whatever the service is connected to later.
        let Some(repo) = pull_repo(&pull.url) else { return };
        let hash = format!("{:016x}", body.comment.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, x| (h ^ x as u64).wrapping_mul(0x0100_0000_01b3)));
        if last_hash.as_deref() == Some(hash.as_str()) {
            return;
        }
        // The pull request's comment, whichever project's preview posted it: one per pull request.
        let comment_id = comment_id.or_else(|| self.store.pull_comment(service, &repo, pull.number).ok().flatten());
        match gr.comment(service, &repo, pull.number as u64, &body.comment, comment_id.map(|i| i as u64)).await {
            Ok(Some(id)) => {
                let _ = self.store.set_git_report(&p.id, None, Some((id as i64, &hash)));
                let _ = self.store.set_pull_comment(service, &repo, pull.number, id as i64);
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(preview = %p.id, error = %e, "previews: a Git URL preview's pull request comment"),
        }
    }

}

impl Engine {
    /// The project roll code's `POST …/preview` makes a change's preview in, as `(id, name)`: of
    /// the projects with previews on that stand for `kind r#ref` (`Workspace::trusts`), the one
    /// that has had it on its canvas longest, then by id.
    pub fn speaker(&self, kind: roll_workspace::Kind, r#ref: &str) -> Option<(String, String)> {
        self.workspace
            .projects_of(kind.as_str(), r#ref)
            .ok()?
            .iter()
            .filter_map(|id| self.workspace.get(roll_workspace::Scope::All, id).ok())
            .filter(|p| self.workspace.trusts(p, kind, r#ref) && self.store.settings(&p.id).is_ok_and(|s| s.enabled))
            .filter_map(|p| p.resources.iter().find(|x| x.kind == kind && x.r#ref == r#ref).map(|x| (x.added_at, p.id.clone(), p.name.clone())))
            .min()
            .map(|(_, id, name)| (id, name))
    }

    /// Keeps `row`'s [`crate::Preview::reported_by`] up to date: `(changed, it speaks)`. With no
    /// speaker at all (its project's previews just turned off) it speaks for itself. Only a Git
    /// URL service's previews can be quiet: each project with the service on its canvas has its
    /// own preview of a branch, where a change has one preview (made by its repo's speaker).
    /// An error (the workspace or the store unread) changes nothing: the voice stored stays.
    pub(super) fn refresh_voice(&self, row: &Row) -> anyhow::Result<(bool, bool)> {
        let Some(svc) = row.repo.strip_prefix("git:") else { return Ok((false, true)) };
        // Of the projects with an active preview of this branch (each stands for the service, or
        // its preview would be gone), the one that has had the service on its canvas longest:
        // a project whose rules skip the branch has no preview, so it never silences one that does.
        let mut speaker: Option<(i64, String, String)> = None;
        let mut operators = false;
        for r in self.store.branch_rows(&row.repo, row.branch.as_deref())? {
            let p = self.workspace.get(roll_workspace::Scope::All, &r.project_id)?;
            if r.id == row.id {
                operators = p.owner.is_empty();
            }
            let Some(added) = p.resources.iter().find(|x| x.kind == roll_workspace::Kind::Service && x.r#ref == svc).map(|x| x.added_at) else { continue };
            let key = (added, p.id.clone(), p.name.clone());
            if speaker.as_ref().is_none_or(|s| key < *s) {
                speaker = Some(key);
            }
        }
        // Named only on the operator's projects, which see every project: a person's project
        // never learns another owner's project's name (they're 404s to them).
        let by = speaker.filter(|(_, id, _)| *id != row.project_id).map(|(_, _, name)| if operators { name } else { "another project".into() });
        Ok((self.store.set_reported_by(&row.id, by.as_deref())?, by.is_none()))
    }
}

/// `owner/name` of a pull request's page (`https://<host>/<owner>/<name>/pull/<n>`).
fn pull_repo(url: &str) -> Option<String> {
    let path = url.split_once("://")?.1.split_once('/')?.1;
    let mut parts = path.split('/');
    let (owner, name, kind) = (parts.next()?, parts.next()?, parts.next()?);
    (kind == "pull" && !owner.is_empty() && !name.is_empty()).then(|| format!("{owner}/{name}"))
}
