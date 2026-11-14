//! What the API asks of previews: create, delete, refresh, reset, sleep, wake, logs.

use super::*;

impl Engine {
    // ── what the API asks for ──

    /// The feedback left on `path` of a preview's pages (the latest 50, oldest first), for the
    /// button that lists it: roll previews' own proxy, and roll apps' for its `pv-` environments.
    /// Nothing for a branch's preview (no change to read from) or one that doesn't exist.
    pub async fn notes_of(&self, preview: &str, path: &str) -> anyhow::Result<Vec<crate::notes::Note>> {
        let mut here: Vec<crate::notes::Note> = self.open_notes(preview).await?.into_iter().filter(|n| n.path == path).collect();
        let skip = here.len().saturating_sub(50);
        here.drain(..skip);
        Ok(here)
    }

    /// The notes left on a preview's pages that nobody has resolved, every page's (the latest
    /// 500, oldest first): `GET …/notes`, and the page's button by path.
    pub async fn open_notes(&self, preview: &str) -> anyhow::Result<Vec<crate::notes::Note>> {
        let Ok(row) = self.store.row(preview) else { return Ok(Vec::new()) };
        let Some(change) = &row.change_id else { return Ok(Vec::new()) };
        let resolved = self.store.resolved_notes(preview)?;
        let mut here: Vec<crate::notes::Note> = self.source.feedback(change).await?.into_iter().filter(|n| !resolved.contains(&n.id)).collect();
        let skip = here.len().saturating_sub(500);
        here.drain(..skip);
        Ok(here)
    }

    /// `by` resolves a note on a change's preview page (Vercel Comments): it must be one of the
    /// change's feedback notes; the page's button stops listing it.
    pub async fn resolve_note(&self, id: &str, note: &str, by: &str) -> Result<()> {
        let r = self.store.row(id)?;
        let change = r.change_id.as_deref().ok_or_else(|| Error::BadRequest("only a change's preview has notes".into()))?;
        let notes = self.source.feedback(change).await.map_err(|e| Error::Internal(format!("{e:#}")))?;
        if !notes.iter().any(|n| n.id == note) {
            return Err(Error::NotFound(format!("no note {note} on this preview's change")));
        }
        self.store.resolve_note(id, note, by)?;
        self.store.log(id, &format!("{by} resolved a note on its page"))?;
        Ok(())
    }

    /// [`Engine::notes_of`] by the preview's environment name (`pv-<id>`, as roll apps knows it).
    pub async fn notes_for(&self, env: &str, path: &str) -> anyhow::Result<Vec<crate::notes::Note>> {
        match env.strip_prefix("pv-") {
            Some(id) if !id.is_empty() => self.notes_of(&format!("pv_{id}"), path).await,
            _ => Ok(Vec::new()),
        }
    }

    /// The ids of `owner`'s workspace projects (a signed-in person's).
    pub fn projects_of(&self, owner: &str) -> Result<std::collections::HashSet<String>> {
        Ok(self.workspace.list(roll_workspace::Scope::Owner(owner))?.into_iter().map(|p| p.id).collect())
    }

    /// A workspace project, or a 404.
    pub fn workspace_project(&self, id: &str) -> Result<roll_workspace::Project> {
        Ok(self.workspace.get(roll_workspace::Scope::All, id)?)
    }

    /// A preview for `branch` of `repo` (or for change `change`), made by hand.
    pub async fn create(&self, project_id: &str, repo: &str, branch: Option<&str>, change: Option<&str>, by: &str) -> Result<(Preview, bool)> {
        let project = self.workspace.get(roll_workspace::Scope::All, project_id)?;
        if !project.resources.iter().any(|r| r.kind == roll_workspace::Kind::Repo && r.r#ref == repo) {
            return Err(Error::NotFound(format!("{repo} isn't in project {}", project.name)));
        }
        let s = self.store.settings(project_id)?;
        if !s.enabled {
            return Err(Error::Conflict(format!("previews are off for project {}: turn them on in its preview rules", project.name)));
        }
        let busy = self.store.rows(Some(project_id))?.iter().filter(|r| r.desired == Desired::Active && !r.is_baseline() && !matches!(r.state, State::Queued | State::AwaitingApproval)).count();
        let n = match (change, branch) {
            (Some(id), _) => {
                let c = self.source.change(id).await.map_err(|e| Error::Internal(e.to_string()))?.ok_or_else(|| Error::NotFound(format!("no change {id}")))?;
                if c.repo != repo {
                    return Err(Error::BadRequest(format!("change {id} is in {}, not {repo}", c.repo)));
                }
                if !c.open {
                    return Err(Error::Conflict(format!("change {id} has landed or closed")));
                }
                if let Some(existing) = self.store.row_by_change(id)? {
                    return Ok((self.store.view(existing)?, false));
                }
                self.store.undismiss(id)?;
                New {
                    project_id: project_id.to_string(),
                    key: c.github_pr.map_or_else(|| format!("c{}", c.number), |n| format!("pr{n}")),
                    project_name: project.name.clone(),
                    repo: repo.to_string(),
                    change_id: Some(c.id.clone()),
                    branch: c.branch.clone(),
                    number: Some(c.number),
                    title: c.title.clone(),
                    github_pr: c.github_pr,
                    head: c.head.clone(),
                    trusted: c.trusted,
                    state: if c.may_run() { State::Creating } else { State::AwaitingApproval },
                    expires_at: None,
                    created_by: by.to_string(),
                }
            }
            (None, Some(b)) => {
                let head = self
                    .source
                    .head(repo, Some(b))
                    .await
                    .map_err(|e| Error::Internal(e.to_string()))?
                    .ok_or_else(|| Error::NotFound(format!("{repo} has no branch {b}")))?;
                if let Some(existing) = self.store.rows(Some(project_id))?.into_iter().find(|r| r.change_id.is_none() && r.repo == repo && r.branch.as_deref() == Some(b) && r.desired == Desired::Active) {
                    return Ok((self.store.view(existing)?, false));
                }
                let now = self.store.now_ms()?;
                New {
                    project_id: project_id.to_string(),
                    key: b.to_string(),
                    project_name: project.name.clone(),
                    repo: repo.to_string(),
                    change_id: None,
                    branch: Some(b.to_string()),
                    number: None,
                    title: format!("{repo}@{b}"),
                    github_pr: None,
                    head,
                    trusted: true,
                    state: State::Creating,
                    expires_at: Some(now + s.ttl_hours as i64 * 3_600_000),
                    created_by: by.to_string(),
                }
            }
            (None, None) => return Err(Error::BadRequest("say which branch (branch) or change (change) to preview".into())),
        };
        if busy >= s.max_previews as usize {
            return Err(Error::Conflict(format!("the project has {busy} previews, its max_previews: delete one first")));
        }
        let (row, made) = self.store.create(&n)?;
        self.kick();
        Ok((self.store.view(row)?, made))
    }

    /// Delete a preview. A change's preview stays deleted until the change's next push.
    pub fn delete(&self, id: &str) -> Result<Preview> {
        let row = self.store.row(id)?;
        if let Some(change) = &row.change_id {
            self.store.dismiss(change, &row.head)?;
        }
        let r = self.mark_deleted(id, "asked to")?;
        self.store.view(r)
    }

    /// Converge now (redeploy whatever isn't at the revision it should run).
    pub fn refresh(&self, id: &str) -> Result<Preview> {
        let r = self.store.update(id, |row| {
            row.next_at = 0;
            row.attempts = 0;
        })?;
        self.usage_read.lock().remove(id);
        self.kick();
        self.store.view(r)
    }

    /// A link to the preview for someone outside the team, open for `hours` (1–720), on its own
    /// token: the answer is the only time the link is shown. Only where the preview needs a
    /// link (`access: link`); a public one needs none.
    pub fn share(&self, id: &str, hours: u32, by: &str) -> Result<(crate::Share, String)> {
        if !(1..=KEEP_MAX_HOURS).contains(&hours) {
            return Err(Error::BadRequest(format!("hours: 1 to {KEEP_MAX_HOURS}")));
        }
        let r = self.store.row(id)?;
        if r.desired == Desired::Deleted {
            return Err(Error::Conflict("the preview is being deleted".into()));
        }
        if !self.store.access(id)?.is_some_and(|a| a.required) {
            return Err(Error::Conflict("the preview is public: anyone can open it without a link".into()));
        }
        let url = self.store.services(id)?.into_iter().next().map(|s| s.url).ok_or_else(|| Error::Conflict("the preview has no service to share yet".into()))?;
        let (share, token) = self.store.create_share(id, self.store.now_ms()? + hours as i64 * 3_600_000, by)?;
        self.store.log(id, &format!("shared for {hours} hours by {by} ({})", share.id))?;
        Ok((share, format!("{}/?roll_share={token}", url.trim_end_matches('/'))))
    }

    /// End a share link at once.
    pub fn unshare(&self, id: &str, share: &str) -> Result<()> {
        self.store.revoke_share(id, share)?;
        self.store.log(id, &format!("share link {share} revoked"))?;
        Ok(())
    }

    /// Keep the preview past its TTL for `hours` from now (at most [`KEEP_MAX_HOURS`]); `0` lets
    /// its rules decide again. A landed or closed change, or a deleted project, still ends it.
    pub fn keep(&self, id: &str, hours: u32) -> Result<Preview> {
        if hours > KEEP_MAX_HOURS {
            return Err(Error::BadRequest(format!("hours: 0 to {KEEP_MAX_HOURS}")));
        }
        let r = self.store.row(id)?;
        if r.desired == Desired::Deleted {
            return Err(Error::Conflict("the preview is being deleted".into()));
        }
        let until = (hours > 0).then(|| self.store.now_ms().map(|now| now + hours as i64 * 3_600_000)).transpose()?;
        self.store.set_kept_until(id, until)?;
        self.store.log(id, &match hours {
            0 => "no longer kept: its rules decide when it goes".to_string(),
            h => format!("kept for {h} hours past its TTL"),
        })?;
        self.store.view(self.store.row(id)?)
    }

    /// `by` accepts how the change's pages look at its latest screenshot (`github.visual_review`):
    /// `page` only, or all of them; a new head needs its own. The change and its pull request
    /// say so at once.
    /// `person`: not a token's; only a person (or the operator) accepts a required one.
    pub async fn accept_visual(&self, id: &str, by: &str, person: bool, seen: Option<&str>, page: Option<&str>) -> Result<Preview> {
        let r = self.store.row(id)?;
        if r.change_id.is_none() {
            return Err(Error::BadRequest("only a change's preview has a visual review".into()));
        }
        if !person && self.store.settings(&r.project_id)?.visual_review == crate::settings::VisualReviewRule::Required {
            return Err(Error::Forbidden("a required visual review is accepted by a person, in roll, not by a token".into()));
        }
        // Not by the change's own author, as approvals aren't (and as GitHub's button isn't).
        if let Some(change) = &r.change_id
            && let Ok(Some(c)) = self.source.change(change).await
            && (by.eq_ignore_ascii_case(&c.author) || by.eq_ignore_ascii_case(c.author.trim_start_matches('@')))
        {
            return Err(Error::Forbidden("the change's own author can't accept its visual change: someone else looks at it".into()));
        }
        let head = self.store.accept_visual(id, by, seen, page)?.ok_or_else(|| Error::Conflict("the preview has no screenshot to accept yet".into()))?;
        let what = match page {
            None => "its pages look".to_string(),
            Some("") => "its first page looks".to_string(),
            Some(p) => format!("{p} looks"),
        };
        self.store.log(id, &format!("{by} accepted how {what} at {}", short(&head)))?;
        self.report(r.clone()).await;
        self.store.view(self.store.row(id)?)
    }

    /// Each data branch's connection, as the preview's services get it (Neon's connection
    /// strings), so a reviewer queries what the preview sees: the branch's own credentials (a
    /// Postgres branch's role), never the parent's or a superuser's. Read from the products each
    /// time, as converges do; nothing is stored.
    pub async fn connections(&self, id: &str) -> Result<Vec<crate::Connection>> {
        let r = self.store.row(id)?;
        if r.desired == Desired::Deleted {
            return Err(Error::Conflict("the preview is being deleted".into()));
        }
        let vars = self.branch_vars(id).await.map_err(|e| Error::Internal(format!("{e:#}")))?;
        Ok(vars.into_iter().map(|(kind, parent, vars)| crate::Connection { kind, parent, vars: vars.into_iter().collect() }).collect())
    }

    /// Set (`Some`) or remove (`None`) the preview's own variables, over the project's `env`
    /// (Vercel's branch-scoped variables, Railway's per-environment ones). Its services redeploy
    /// with them. roll's own (`ROLL_*`, `PORT`) and its branches' are refused: those always win.
    pub async fn set_vars(&self, id: &str, changes: BTreeMap<String, Option<String>>) -> Result<BTreeMap<String, String>> {
        let r = self.store.row(id)?;
        if r.desired == Desired::Deleted {
            return Err(Error::Conflict("the preview is being deleted".into()));
        }
        // Best effort: a preview not branched yet has no names to clash with.
        let theirs: Vec<String> = self.resource_env(id).await.map(|v| v.into_iter().map(|(k, _)| k).collect()).unwrap_or_default();
        for (k, v) in &changes {
            let well_formed = !k.is_empty() && k.len() <= 128 && !k.starts_with(|c: char| c.is_ascii_digit()) && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
            if !well_formed {
                return Err(Error::BadRequest(format!("{k:?} isn't a variable name (letters, digits and _)")));
            }
            if k.starts_with("ROLL_") || k == "PORT" || theirs.contains(k) {
                return Err(Error::BadRequest(format!("{k} is set by roll or the preview's branches")));
            }
            if v.as_ref().is_some_and(|v| v.len() > MAX_VAR_BYTES) {
                return Err(Error::BadRequest(format!("{k}: at most {MAX_VAR_BYTES} bytes")));
            }
        }
        let vars = self.store.update_vars(id, |vars| {
            for (k, v) in &changes {
                match v {
                    Some(v) => vars.insert(k.clone(), v.clone()),
                    None => vars.remove(k),
                };
            }
            if vars.len() > MAX_VARS {
                return Err(Error::BadRequest(format!("at most {MAX_VARS} variables per preview")));
            }
            Ok(())
        })?;
        let names: Vec<&str> = vars.keys().map(String::as_str).collect();
        self.store.log(id, &if names.is_empty() { "its own variables removed".to_string() } else { format!("its own variables: {}", names.join(", ")) })?;
        self.store.update(id, |row| row.next_at = 0)?;
        self.kick();
        Ok(vars)
    }

    /// Re-branch `which` (`"<kind>:<parent>"`, or every resource when empty) from the parents.
    pub fn reset(&self, id: &str, which: Vec<String>) -> Result<Preview> {
        let resources = self.store.resources(id)?;
        for w in &which {
            if !resources.iter().any(|r| format!("{}:{}", r.kind.as_str(), r.parent) == *w) {
                return Err(Error::BadRequest(format!("the preview has no resource {w}")));
            }
        }
        let which = if which.is_empty() { vec!["*".to_string()] } else { which };
        let r = self.store.update(id, |row| {
            if row.desired == Desired::Active {
                row.reset_requested = Some(which.clone());
                row.next_at = 0;
                row.attempts = 0;
                if row.state == State::Sleeping && row.error.is_some() {
                    row.state = State::Updating;
                }
            }
        })?;
        self.kick();
        self.store.view(r)
    }

    pub async fn sleep(&self, id: &str) -> Result<Preview> {
        let r = self.store.row(id)?;
        for svc in self.store.services(id)? {
            self.apps.sleep(id, &svc.name).await;
            self.store.set_service(id, &svc.name, ServiceState::Sleeping, None, None)?;
        }
        let r = self.store.update(&r.id, |row| {
            row.asleep = true;
            if row.state == State::Ready {
                row.state = State::Sleeping;
            }
        })?;
        self.store.log(id, "put to sleep")?;
        self.report(r).await;
        self.store.preview(id)
    }

    pub async fn wake(&self, id: &str) -> Result<Preview> {
        let r = self.store.row(id)?;
        if r.desired == Desired::Deleted || matches!(r.state, State::AwaitingApproval | State::Queued | State::Deleting) {
            return Err(Error::Conflict(format!("the preview is {}: it can't be woken", r.state.as_str())));
        }
        self.store.update(id, |row| {
            row.asleep = false;
            row.last_active_at = row.last_active_at.max(0);
        })?;
        self.store.touch(id)?;
        for svc in self.store.services(id)? {
            if let Err(e) = self.apps.wake(id, &svc.name).await {
                return Err(Error::Conflict(format!("{} didn't start: {e:#}", svc.name)));
            }
        }
        self.store.preview(id)
    }

    pub async fn logs(&self, id: &str, service: Option<&str>, lines: usize) -> Result<(String, Vec<String>)> {
        let services = self.store.services(id)?;
        let svc = match service {
            Some(name) => services.into_iter().find(|s| s.name == name).ok_or_else(|| Error::NotFound(format!("the preview has no service {name}")))?,
            None => services.into_iter().next().ok_or_else(|| Error::NotFound("the preview has no services".into()))?,
        };
        Ok((svc.name.clone(), self.apps.logs(id, &svc.name, lines).await))
    }

    /// A service's last `lines` and then its output as it comes. Where the product can't follow,
    /// its tail is read every [`FOLLOW_POLL`] and what's new passed on. Ends when the receiver is
    /// dropped (or the service's output does).
    pub async fn follow(self: &Arc<Self>, id: &str, service: Option<&str>, lines: usize) -> Result<(String, tokio::sync::mpsc::Receiver<String>)> {
        let (name, _) = self.logs(id, service, 1).await?;
        if let Some(rx) = self.apps.follow(id, &name, lines).await {
            return Ok((name, rx));
        }
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let (me, id, svc) = (self.clone(), id.to_string(), name.clone());
        tokio::spawn(async move {
            // The first read is what was asked for; later ones read wide, so a burst between two
            // reads isn't lost.
            let (mut seen, mut first): (Vec<String>, bool) = (Vec::new(), true);
            loop {
                let now = me.apps.logs(&id, &svc, if first { lines } else { FOLLOW_WINDOW }).await;
                let from = match resume_at(&seen, &now) {
                    Some(i) => i,
                    None => {
                        let _ = tx.send("… (lines skipped: more came between two reads than a read sees)".into()).await;
                        0
                    }
                };
                for l in &now[from..] {
                    if tx.send(l.clone()).await.is_err() {
                        return;
                    }
                }
                if !now.is_empty() {
                    seen = now;
                }
                first = false;
                tokio::select! {
                    _ = tokio::time::sleep(FOLLOW_POLL) => {}
                    _ = tx.closed() => return,
                }
            }
        });
        Ok((name, rx))
    }
}

/// The longest a preview can be kept past its TTL in one go (30 days).
pub const KEEP_MAX_HOURS: u32 = 720;
/// A preview's own variables: how many, and how long each value.
pub const MAX_VARS: usize = 100;
pub const MAX_VAR_BYTES: usize = 32 * 1024;

/// How many lines each later read of a polled log asks for.
pub const FOLLOW_WINDOW: usize = 2000;

/// How often a service that can't be followed has its tail read again.
pub const FOLLOW_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// Where in `now` (a later, possibly wider read of the same log) the lines after `before` start:
/// just past the first place `before`'s last lines (up to 20) appear in it. `None`: they don't,
/// so lines were missed. The first match, so repeated lines are passed on again rather than
/// silently dropped.
pub fn resume_at(before: &[String], now: &[String]) -> Option<usize> {
    if before.is_empty() {
        return Some(0);
    }
    let k = before.len().min(20);
    let tail = &before[before.len() - k..];
    (k..=now.len()).find(|&p| &now[p - k..p] == tail)
}

#[cfg(test)]
mod tests {
    /// A followed log that's polled passes each line on once: what's new after the lines it
    /// already saw, everything when the tail moved past them.
    #[test]
    fn a_polled_log_passes_on_only_new_lines() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(super::resume_at(&[], &v(&["a", "b"])), Some(0));
        assert_eq!(super::resume_at(&v(&["a", "b"]), &v(&["a", "b"])), Some(2), "nothing new");
        assert_eq!(super::resume_at(&v(&["b", "c"]), &v(&["z", "a", "b", "c", "d"])), Some(4), "a wider read: only d is new");
        assert_eq!(super::resume_at(&v(&["x", "x"]), &v(&["x", "x", "x"])), Some(2), "repeated lines aren't swallowed");
        assert_eq!(super::resume_at(&v(&["a"]), &v(&["q", "r"])), None, "moved past everything seen: a gap");
    }
}
