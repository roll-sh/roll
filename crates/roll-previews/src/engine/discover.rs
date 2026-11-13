//! Which previews should exist: a project's open changes under its rules, TTLs, slots.

use super::*;

impl Engine {
    /// The previews a project should have, from its repos' open changes and its rules.
    /// Once a month per level: tell the project's repos its previews passed 80% (then 100%) of
    /// `max_usd_month`.
    async fn budget_alert(&self, project: &roll_workspace::Project, repos: &[String], s: &Settings, spent: f64, month: i64) {
        if s.max_usd_month <= 0.0 {
            return;
        }
        let Some(&level) = BUDGET_LEVELS.iter().rev().find(|&&l| spent >= s.max_usd_month * l as f64 / 100.0) else { return };
        match self.store.alerted(&project.id, month, s.max_usd_month) {
            Ok(Some(sent)) if sent >= level as i64 => return,
            Ok(_) => {}
            // Unknown: never risk an alert every pass; the next pass reads it again.
            Err(e) => return warn!(project = %project.id, error = %e, "previews: reading budget alerts"),
        }
        let alert = crate::ports::BudgetAlert {
            project: project.id.clone(),
            project_name: project.name.clone(),
            percent: level,
            spent_usd_month: (spent * 100.0).round() / 100.0,
            max_usd_month: s.max_usd_month,
        };
        if let Err(e) = self.source.budget(repos, &alert).await {
            return warn!(project = %project.id, error = %e, "previews: sending a budget alert (tried again next pass)");
        }
        let msg = format!("the project's previews passed {level}% of the month's budget (${:.2} of ${:.2})", alert.spent_usd_month, s.max_usd_month);
        info!(project = %project.id, "{msg}");
        for r in self.store.rows(Some(&project.id)).unwrap_or_default().iter().filter(|r| r.desired == Desired::Active) {
            let _ = self.store.log(&r.id, &msg);
        }
        if let Err(e) = self.store.set_alerted(&project.id, month, s.max_usd_month, level as i64) {
            warn!(project = %project.id, error = %e, "previews: recording a budget alert");
        }
    }

    pub(super) async fn discover(&self, project_id: &str, s: &Settings) -> anyhow::Result<()> {
        let project = match self.workspace.get(project_id) {
            Ok(p) => p,
            Err(roll_workspace::Error::NotFound(_)) => {
                // The project is gone: so are its previews (their branches go with them).
                for r in self.store.rows(Some(project_id))? {
                    self.mark_deleted(&r.id, "the project was deleted")?;
                }
                self.store.delete_settings(project_id)?;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        let repos: Vec<String> = project.resources.iter().filter(|r| r.kind == roll_workspace::Kind::Repo).map(|r| r.r#ref.clone()).collect();
        let now = self.store.now_ms()?;
        let mut open: HashMap<String, ChangeInfo> = HashMap::new();
        let mut heads: HashMap<String, Option<String>> = HashMap::new();
        for repo in &repos {
            // A repo gone from roll code (the workspace still names it) has no changes; it must
            // not stop the project's other previews from being followed and deleted.
            let Some(head) = self.source.head(repo, None).await? else {
                heads.insert(repo.clone(), None);
                continue;
            };
            for c in self.source.open_changes(repo).await? {
                open.insert(c.id.clone(), c);
            }
            heads.insert(repo.clone(), Some(head));
        }
        let existing = self.store.rows(Some(project_id))?;
        // Changes that match the rules and have no preview yet.
        for c in open.values() {
            if !self.wanted(s, c).await? {
                continue;
            }
            if existing.iter().any(|r| r.change_id.as_deref() == Some(&c.id)) || self.store.dismissed(&c.id, &c.head)? {
                continue;
            }
            let key = match c.github_pr {
                Some(n) => format!("pr{n}"),
                None => format!("c{}", c.number),
            };
            let state = if c.may_run() { State::Creating } else { State::AwaitingApproval };
            let (r, made) = self.store.create(&New {
                project_id: project_id.to_string(),
                key,
                project_name: project.name.clone(),
                repo: c.repo.clone(),
                change_id: Some(c.id.clone()),
                branch: c.branch.clone(),
                number: Some(c.number),
                title: c.title.clone(),
                github_pr: c.github_pr,
                head: c.head.clone(),
                trusted: c.trusted,
                state,
                expires_at: None,
                created_by: "roll".into(),
            })?;
            if made {
                info!(preview = %r.id, change = %c.id, "previews: new preview");
            }
        }
        // What changed for the previews this project has.
        for r in self.store.rows(Some(project_id))? {
            if r.desired == Desired::Deleted {
                continue;
            }
            let ttl_ms = s.ttl_hours as i64 * 3_600_000;
            if let Some(change) = &r.change_id {
                let wanted = match open.get(change) {
                    Some(c) => self.wanted(s, c).await?,
                    None => false,
                };
                match open.get(change) {
                    Some(c) if wanted => {
                        if c.head != r.head || c.title != r.title || c.trusted != r.trusted || c.github_pr != r.github_pr {
                            let moved = c.head != r.head;
                            // A push that touches only ignored paths (since what the services
                            // run) keeps them running; the preview stands for the new head.
                            let keep = moved && matches!(r.state, State::Ready | State::Sleeping) && self.only_ignored(s, &r.repo, r.build(), &c.head).await?;
                            let build = r.build().to_string();
                            let (head, title, trusted, pr) = (c.head.clone(), c.title.clone(), c.trusted, c.github_pr);
                            self.store.update(&r.id, |row| {
                                row.head = head.clone();
                                row.title = title.clone();
                                row.trusted = trusted;
                                row.github_pr = pr;
                                if moved {
                                    row.last_active_at = now;
                                    row.next_at = 0;
                                    row.attempts = 0;
                                    if keep {
                                        row.runs = Some(build.clone());
                                    } else {
                                        row.runs = None;
                                        if matches!(row.state, State::Ready | State::Sleeping | State::Failed) {
                                            row.state = State::Updating;
                                        }
                                    }
                                }
                            })?;
                            if moved && keep {
                                self.store.log(&r.id, &format!("pushed: {} (only ignored paths: the services keep running {})", short(&c.head), short(&build)))?;
                            } else if moved {
                                self.store.log(&r.id, &format!("pushed: {}", short(&c.head)))?;
                            }
                        }
                        // Services kept on an earlier head redeploy once the path filters no
                        // longer leave out what changed since (they were edited or removed).
                        if c.head == r.head && let Some(runs) = &r.runs && !self.only_ignored(s, &r.repo, runs, &r.head).await? {
                            self.store.update(&r.id, |row| {
                                row.runs = None;
                                row.next_at = 0;
                                if matches!(row.state, State::Ready | State::Sleeping | State::Failed) {
                                    row.state = State::Updating;
                                }
                            })?;
                            self.store.log(&r.id, "the path filters changed: redeploying at the change's head")?;
                        }
                        // An approval (or its loss on a new head) is a change of state.
                        if c.may_run() == (r.state == State::AwaitingApproval) {
                            self.store.update(&r.id, |row| row.next_at = 0)?;
                        }
                    }
                    Some(_) => {
                        self.mark_deleted(&r.id, "the change no longer matches the project's preview rules")?;
                        continue;
                    }
                    None => {
                        let why = match self.source.change(change).await? {
                            Some(c) if c.open => None,
                            Some(_) => Some("the change landed or closed"),
                            None => Some("the change was deleted"),
                        };
                        if let Some(why) = why {
                            self.store.undismiss(change)?;
                            self.mark_deleted(&r.id, why)?;
                            continue;
                        }
                    }
                }
            } else if let Some(branch) = &r.branch {
                // A branch preview follows its branch until the branch goes.
                match self.source.head(&r.repo, Some(branch)).await? {
                    None => {
                        self.mark_deleted(&r.id, "the branch was deleted")?;
                        continue;
                    }
                    Some(h) if h != r.head => {
                        self.store.update(&r.id, |row| {
                            row.head = h.clone();
                            row.last_active_at = now;
                            row.next_at = 0;
                            if matches!(row.state, State::Ready | State::Sleeping | State::Failed) {
                                row.state = State::Updating;
                            }
                        })?;
                        self.store.log(&r.id, &format!("pushed: {}", short(&h)))?;
                    }
                    Some(_) => {}
                }
            }
            let kept = self.store.kept_until(&r.id)?.is_some_and(|t| t > now);
            if !kept && (r.expires_at.is_some_and(|t| t <= now) || r.last_active_at + ttl_ms <= now) {
                // An open change gets a preview again on its next push, not before.
                if let Some(change) = &r.change_id {
                    self.store.dismiss(change, &r.head)?;
                }
                self.mark_deleted(&r.id, &format!("unused for {} hours (ttl)", s.ttl_hours))?;
                continue;
            }
            // Unused for a while, nothing running: let the branches drop what they keep warm.
            if r.last_active_at + IDLE_AFTER_MS <= now && matches!(r.state, State::Ready | State::Sleeping) && !self.store.idled(&r.id)? {
                let running = self.store.services(&r.id)?.iter().any(|x| x.state == ServiceState::Running);
                if !running {
                    for res in self.store.resources(&r.id)? {
                        let (Some(branch), Ok(b)) = (&res.r#ref, self.brancher(res.kind)) else { continue };
                        if let Err(e) = b.idle(&res.parent, branch).await {
                            warn!(preview = %r.id, kind = res.kind.as_str(), error = %e, "previews: idling a branch");
                        }
                    }
                    self.store.set_idled(&r.id, true)?;
                    self.store.log(&r.id, "idle for an hour: its branches went cold")?;
                }
            }
            // A service from another repo of the project follows that repo's branch of the same name,
            // else its default branch.
            let mut stale = false;
            for svc in self.store.services(&r.id)? {
                if svc.repo == r.repo || !heads.contains_key(&svc.repo) {
                    continue;
                }
                let want = self.head_for(&svc.repo, r.branch.as_deref(), heads.get(&svc.repo).cloned()).await?;
                if want.is_some() && want.as_deref() != svc.revision.as_deref() {
                    stale = true;
                    break;
                }
            }
            // A reset that's due, or the usage reading that's due, makes it converge too.
            let reset_due = s.reset_every_hours > 0 && r.last_reset_at.unwrap_or(r.created_at) + s.reset_every_hours as i64 * 3_600_000 <= now;
            let usage_due = self.usage_read.lock().get(&r.id).is_none_or(|t| *t + self.cfg.usage_every.as_millis() as i64 <= now);
            if (stale || reset_due || usage_due) && r.next_at == SETTLED {
                self.store.update(&r.id, |row| row.next_at = 0)?;
            }
        }
        // Slots: the oldest queued previews start as others go.
        let rows = self.store.rows(Some(project_id))?;
        // A fork waiting for approval holds no slot: spam PRs can't block the project's previews.
        let busy = rows.iter().filter(|r| r.desired == Desired::Active && !matches!(r.state, State::Queued | State::AwaitingApproval)).count();
        let mut free = (s.max_previews as usize).saturating_sub(busy);
        // The month's budget: once spent, no new preview starts this month.
        let month = crate::store::month_start(self.store.now_ms()?);
        let spent: f64 = self.store.spent(project_id, month)? + rows.iter().filter_map(|r| self.store.view(r.clone()).ok()).map(|p| p.cost.month_to_date_usd).sum::<f64>();
        let over_budget = s.max_usd_month > 0.0 && spent >= s.max_usd_month;
        self.budget_alert(&project, &repos, s, spent, month).await;
        if over_budget {
            free = 0;
        }
        let mut queued: Vec<&Row> = rows.iter().filter(|r| r.desired == Desired::Active && r.state == State::Queued).collect();
        queued.sort_by_key(|r| r.created_at);
        for r in queued {
            if free == 0 {
                break;
            }
            free -= 1;
            self.store.update(&r.id, |row| {
                row.state = State::Creating;
                row.next_at = 0;
            })?;
        }
        // New previews beyond the limit wait.
        let rows = self.store.rows(Some(project_id))?;
        let mut active: Vec<&Row> = rows.iter().filter(|r| r.desired == Desired::Active && !matches!(r.state, State::Queued | State::AwaitingApproval)).collect();
        active.sort_by_key(|r| r.created_at);
        let limit = if over_budget { 0 } else { s.max_previews as usize };
        for r in active.iter().skip(limit) {
            if r.state == State::Creating && self.store.resources(&r.id)?.iter().all(|x| x.r#ref.is_none()) {
                self.store.update(&r.id, |row| row.state = State::Queued)?;
                let why = if over_budget {
                    format!("waiting: the project's previews have cost ${spent:.2} this month, its max_usd_month")
                } else {
                    format!("waiting: the project has {} previews (max_previews)", s.max_previews)
                };
                self.store.log(&r.id, &why)?;
            }
        }
        Ok(())
    }

    pub fn mark_deleted(&self, id: &str, why: &str) -> Result<Row> {
        let r = self.store.update(id, |row| {
            row.desired = Desired::Deleted;
            row.next_at = 0;
            row.attempts = 0;
        })?;
        self.store.log(id, &format!("deleting: {why}"))?;
        self.kick();
        Ok(r)
    }

}
