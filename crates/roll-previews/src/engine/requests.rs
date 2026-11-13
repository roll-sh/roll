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
            let mut seen: Vec<String> = Vec::new();
            loop {
                let now = me.apps.logs(&id, &svc, lines).await;
                for l in &now[overlap(&seen, &now)..] {
                    if tx.send(l.clone()).await.is_err() {
                        return;
                    }
                }
                seen = now;
                tokio::select! {
                    _ = tokio::time::sleep(FOLLOW_POLL) => {}
                    _ = tx.closed() => return,
                }
            }
        });
        Ok((name, rx))
    }
}

/// How often a service that can't be followed has its tail read again.
pub const FOLLOW_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// How many of `now`'s first lines `before` already ended with: the longest suffix of `before`
/// that's a prefix of `now`.
pub fn overlap(before: &[String], now: &[String]) -> usize {
    (0..=before.len().min(now.len())).rev().find(|&k| before[before.len() - k..] == now[..k]).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    /// A followed log that's polled passes each line on once: what's new after the lines it
    /// already saw, everything when the tail moved past them.
    #[test]
    fn a_polled_log_passes_on_only_new_lines() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(super::overlap(&[], &v(&["a", "b"])), 0);
        assert_eq!(super::overlap(&v(&["a", "b"]), &v(&["a", "b"])), 2, "nothing new");
        assert_eq!(super::overlap(&v(&["a", "b", "c"]), &v(&["b", "c", "d"])), 2, "the tail scrolled by one");
        assert_eq!(super::overlap(&v(&["x", "x"]), &v(&["x", "x", "x"])), 2, "repeated lines aren't swallowed");
        assert_eq!(super::overlap(&v(&["a"]), &v(&["q", "r"])), 0, "moved past everything seen");
    }
}
