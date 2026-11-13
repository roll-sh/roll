//! What the API asks of previews: create, delete, refresh, reset, sleep, wake, logs.

use super::*;

impl Engine {
    // ── what the API asks for ──

    /// A workspace project, or a 404.
    pub fn workspace_project(&self, id: &str) -> Result<roll_workspace::Project> {
        Ok(self.workspace.get(id)?)
    }

    /// A preview for `branch` of `repo` (or for change `change`), made by hand.
    pub async fn create(&self, project_id: &str, repo: &str, branch: Option<&str>, change: Option<&str>, by: &str) -> Result<(Preview, bool)> {
        let project = self.workspace.get(project_id)?;
        if !project.resources.iter().any(|r| r.kind == roll_workspace::Kind::Repo && r.r#ref == repo) {
            return Err(Error::NotFound(format!("{repo} isn't in project {}", project.name)));
        }
        let s = self.store.settings(project_id)?;
        if !s.enabled {
            return Err(Error::Conflict(format!("previews are off for project {}: turn them on in its preview rules", project.name)));
        }
        let busy = self.store.rows(Some(project_id))?.iter().filter(|r| r.desired == Desired::Active && !matches!(r.state, State::Queued | State::AwaitingApproval)).count();
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
