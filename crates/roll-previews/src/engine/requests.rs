//! What the API asks of previews: create, delete, refresh, reset, sleep, wake, logs.

use super::*;

/// How long a preview's metrics are kept before they're read again (ms).
const METRICS_FOR: i64 = 10_000;
/// How long a preview's errors are kept before they're read again (ms).
pub(super) const ERRORS_FOR: i64 = 10_000;

/// The span of minutes a preview's errors are read over: 5–1,440, rounded up to a 15-minute step
/// (the dashboard's 60, 360 and 1,440 stay as they are), so asking for every minute in between
/// can't step past [`ERRORS_FOR`]'s cache.
pub fn error_span(minutes: i64) -> i64 {
    (minutes.clamp(5, 1440) + 14) / 15 * 15
}

/// The least time between two rebuilds by one deploy hook (ms).
pub const HOOK_EVERY: i64 = 60_000;

/// `text` with each of `words` (a preview's own names) as `*`.
pub fn without_words(text: &str, words: &[String]) -> String {
    words.iter().fold(text.to_string(), |t, w| t.replace(w.as_str(), "*"))
}

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
        let cropped = self.store.cropped_notes(preview)?;
        for n in &mut here {
            n.crop = cropped.contains(&n.id);
        }
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
        // Only a repo the project stands for (`Workspace::trusts`), as for the previews it makes
        // itself: anyone may put any repo on their own canvas, and a preview builds and runs it.
        if !project.resources.iter().any(|r| r.kind == roll_workspace::Kind::Repo && r.r#ref == repo) || !self.workspace.trusts(&project, roll_workspace::Kind::Repo, repo) {
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
                // A change has one preview: this project's is answered, another's never (it's
                // another project's, perhaps another owner's).
                if let Some(existing) = self.store.row_by_change(id)? {
                    if existing.project_id != project_id {
                        return Err(Error::Conflict(format!("change {id} already has a preview in another project")));
                    }
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

    /// A push to a branch of a Git URL service on roll apps ([`crate::ports::GitPush`]): in each
    /// project whose canvas holds the service, the branch's preview is made (its rules allowing:
    /// previews on, the branch matching `branches`, no `[skip preview]`, room under
    /// `max_previews`), moved to the pushed sha, or deleted with its branch. The service's
    /// previews are the repo `git:<service>` with the branch: the same push twice changes
    /// nothing. Only the repository's writers move its branches, so these previews are trusted.
    ///
    /// A pull request ([`crate::ports::GitPull`]) names the preview by its title and links it.
    /// A same-repo one is its branch's preview; a fork's (`pull/<n>`) is untrusted (secrets
    /// resolve untrusted) and runs only at the head a writer approved ([`Engine::approve`]);
    /// a close deletes it. A row is never made trusted again by a later push.
    pub async fn git_push(&self, p: &crate::ports::GitPush) -> Result<()> {
        let repo = format!("git:{}", p.service);
        let fork = p.pull.as_ref().is_some_and(|pl| pl.fork);
        // The pull request's title as plain words: no control characters, at most 200 characters.
        // Invisible formatting (bidi overrides, zero-width marks) too: the title sits by Approve.
        let invisible = |c: char| c.is_control() || matches!(c, '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}');
        let pull_title = p.pull.as_ref().map(|pl| pl.title.chars().map(|c| if invisible(c) { ' ' } else { c }).take(200).collect::<String>().trim().to_string());
        let projects: Vec<roll_workspace::Project> = self
            .workspace
            .list(roll_workspace::Scope::All)?
            .into_iter()
            // Anyone may put any service on their own canvas: only a project that stands for it
            // (`Workspace::trusts`) gets its pushes (its branches, titles and commits).
            .filter(|pr| pr.resources.iter().any(|r| r.kind == roll_workspace::Kind::Service && r.r#ref == p.service) && self.workspace.trusts(pr, roll_workspace::Kind::Service, &p.service))
            .collect();
        // What the pull request's title says (`[preview]` asks, `[skip preview]`/`[no preview]`
        // skips), and whether its opener is a dependency bot the project skips (`skip_authors`).
        let says = pull_title.as_deref().and_then(title_says);
        for project in projects {
            let rows = self.store.rows(Some(&project.id))?;
            let s = self.store.settings(&project.id)?;
            let bot = p.pull.as_ref().and_then(|pl| pl.author.as_deref()).is_some_and(|a| crate::glob::any(&s.skip_authors, a)) && says != Some(true);
            // A bot's own branch push (its pusher a `skip_authors` glob matches), before any pull
            // request: no preview to make and take back. A preview someone else's push made stays.
            let bot_push = p.pull.is_none() && p.sha.is_some() && p.pusher.as_deref().is_some_and(|a| crate::glob::any(&s.skip_authors, a));
            // A fork's pull request's preview is its number's (never a writer's branch that happens
            // to be named `pull/7`); a branch's is the branch's that no fork's pull request holds.
            let fork_of = |r: &Row| self.store.git_pull(&r.id).ok().flatten().filter(|g| g.fork).map(|g| g.number);
            let existing = rows
                .iter()
                .find(|r| {
                    r.change_id.is_none()
                        && r.repo == repo
                        && r.desired == Desired::Active
                        && match p.pull.as_ref().filter(|pl| pl.fork) {
                            Some(pl) => fork_of(r) == Some(pl.number as i64),
                            None => r.branch.as_deref() == Some(p.branch.as_str()) && fork_of(r).is_none(),
                        }
                })
                .cloned();
            // A bot's branch is pushed before its pull request opens: that push made a preview the
            // pull event takes back, and later pushes to the branch make none until it closes.
            if p.sha.is_none() || says == Some(true) {
                self.store.set_git_skip(&project.id, &repo, &p.branch, false)?;
            } else if bot_push && existing.is_none() {
                self.store.set_git_skip(&project.id, &repo, &p.branch, true)?;
                continue;
            } else if bot {
                self.store.set_git_skip(&project.id, &repo, &p.branch, true)?;
                if let Some(r) = &existing {
                    self.mark_deleted(&r.id, "its pull request's opener is in skip_authors")?;
                }
                continue;
            } else if self.store.git_skipped(&project.id, &repo, &p.branch)? {
                // A later push, or a pull event that names no opener (GitLab's when someone else
                // acted: unknown, not a person), keeps what the opening event decided.
                continue;
            }
            match (&p.sha, existing) {
                (None, Some(r)) => {
                    let why = match &p.pull {
                        Some(pl) => format!("its pull request #{} closed", pl.number),
                        None => format!("its branch {} was deleted", p.branch),
                    };
                    self.mark_deleted(&r.id, &why)?;
                }
                (None, None) => {}
                (Some(sha), Some(r)) => {
                    if let Some(pl) = &p.pull {
                        self.store.set_git_pull(&r.id, pl.number as i64, kept_url(&pl.url), pl.fork, sha)?;
                    }
                    let title = pull_title.clone().filter(|t| !t.is_empty() && *t != r.title);
                    if r.head != *sha || title.is_some() || (fork && r.trusted) {
                        let now = self.store.now_ms()?;
                        self.store.update(&r.id, |x| {
                            if let Some(t) = &title {
                                x.title = t.clone();
                            }
                            x.trusted = x.trusted && !fork;
                            if x.head != *sha {
                                x.head = sha.clone();
                                x.last_active_at = now;
                                x.next_at = 0;
                                if matches!(x.state, State::Ready | State::Sleeping | State::Failed) {
                                    x.state = State::Updating;
                                }
                            }
                        })?;
                        if r.head != *sha {
                            self.store.log(&r.id, &format!("{} pushed: {}", p.branch, short(sha)))?;
                        }
                    }
                }
                (Some(sha), None) => {
                    let skip = [Some(p.message.as_str()), pull_title.as_deref()].into_iter().flatten().any(|t| title_says(t) == Some(false));
                    if !s.enabled || skip || !crate::glob::any(&s.branches, &p.branch) {
                        continue;
                    }
                    // A fork's pull request waiting for approval holds a place too: anyone can open
                    // them, and none may grow the list past `max_previews`.
                    let busy = rows.iter().filter(|r| r.desired == Desired::Active && !r.is_baseline() && !(r.state == State::Queued || (r.state == State::AwaitingApproval && !fork))).count();
                    if busy >= s.max_previews as usize {
                        continue;
                    }
                    let now = self.store.now_ms()?;
                    let name = p.url.rsplit('/').next().unwrap_or(&p.service).trim_end_matches(".git");
                    let n = New {
                        project_id: project.id.clone(),
                        key: match &p.pull {
                            Some(pl) if pl.fork => format!("pr{}", pl.number),
                            _ => p.branch.clone(),
                        },
                        project_name: project.name.clone(),
                        repo: repo.clone(),
                        change_id: None,
                        branch: Some(p.branch.clone()),
                        number: None,
                        title: pull_title.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| format!("{name}@{}", p.branch)),
                        github_pr: None,
                        head: sha.clone(),
                        trusted: !fork,
                        state: if fork { State::AwaitingApproval } else { State::Creating },
                        expires_at: Some(now + s.ttl_hours as i64 * 3_600_000),
                        created_by: format!("git:{}", p.service),
                    };
                    let (row, _) = self.store.create(&n)?;
                    if let Some(pl) = &p.pull {
                        self.store.set_git_pull(&row.id, pl.number as i64, kept_url(&pl.url), pl.fork, sha)?;
                    }
                    if fork {
                        self.store.log(&row.id, &format!("pull request #{} is from a fork: it runs once a writer approves {}", p.pull.as_ref().map_or(0, |pl| pl.number), short(sha)))?;
                    }
                }
            }
        }
        self.kick();
        Ok(())
    }

    /// Whether a preview with no roll code change may run its head: a Git URL service's fork's
    /// pull request only at the head a writer approved; everything else (trusted) always.
    pub(super) fn git_head_may_run(&self, r: &Row) -> Result<bool> {
        if r.trusted {
            return Ok(true);
        }
        Ok(self.store.git_pull(&r.id)?.and_then(|g| g.approved_head).as_deref() == Some(r.head.as_str()))
    }

    /// `POST …/approve {head}`: a writer lets this head of a fork's pull request run (roll
    /// code's `approve-run` for a Git URL service's preview). Only the head the preview is at; the
    /// next push waits again.
    pub fn approve(&self, id: &str, head: &str, by: &str) -> Result<Preview> {
        // Roll only holds on roll apps for the instance (apps' #143, its gateway isolated from the
        // host's services since #240) and the build: it gets none of its secrets (#222; rolld's
        // adapter deploys nothing to a closed environment that doesn't say so) on a network of
        // its own, out only to public hosts through its proxy (#238). So a fork's head is
        // released under it.
        self.store.approve_git_pull(id, head, by)?;
        self.store.log(id, &format!("{} approved by {by}", short(head)))?;
        let r = self.store.update(id, |row| {
            row.next_at = 0;
            row.attempts = 0;
        })?;
        self.kick();
        self.store.view(r)
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

    /// A deploy hook for `project`'s previews of `branches` (globs; `["*"]`: every one): its path,
    /// shown once (`/v1/previews/hooks/rlb_…`).
    pub fn create_hook(&self, project: &str, name: &str, branches: &[String]) -> Result<(crate::store::Hook, String)> {
        self.workspace_project(project)?;
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
            return Err(Error::BadRequest("a hook's name is 1–64 printable characters".into()));
        }
        if branches.is_empty() || branches.len() > crate::settings::MAX_BRANCH_GLOBS || branches.iter().any(|b| b.is_empty() || b.len() > 200) {
            return Err(Error::BadRequest(format!("branches: 1–{} globs of 1–200 characters", crate::settings::MAX_BRANCH_GLOBS)));
        }
        let (hook, secret) = self.store.create_hook(project, name, branches)?;
        Ok((hook, format!("/v1/previews/hooks/{secret}")))
    }

    /// `POST /v1/previews/hooks/{secret}` (a headless CMS's publish webhook): every active preview
    /// of the hook's project whose branch a glob matches is built again at its head, with nothing
    /// cached. At most once a [`HOOK_EVERY`] (a CMS that saves often makes one rebuild, not one a
    /// save: `coalesced`). Anything but a live hook is the same 404. `from`: the caller's address,
    /// for each preview's activity.
    pub fn run_hook(&self, secret: &str, from: Option<&str>) -> Result<(Vec<String>, bool)> {
        let Some(hook) = self.store.hook_by_secret(secret)? else { return Err(Error::NotFound("no such hook".into())) };
        if !self.store.fire_hook(&hook.id, self.store.now_ms()?, HOOK_EVERY)? {
            return Ok((Vec::new(), true));
        }
        let mut rebuilt = Vec::new();
        for r in self.store.rows(Some(&hook.project))? {
            if r.desired != Desired::Active || !crate::glob::any(&hook.branches, r.branch.as_deref().unwrap_or("*")) {
                continue;
            }
            self.store.add_rebuild(&r.id)?;
            let by = from.map(|a| format!(" (from {a})")).unwrap_or_default();
            self.store.log(&r.id, &format!("built again by the deploy hook {:?}{by}", hook.name))?;
            rebuilt.push(r.id);
        }
        self.kick();
        Ok((rebuilt, false))
    }

    /// A hook secret found where it shouldn't be (roll-access's leak scanning): the hook stops
    /// working at once. Says which it was, or `None` when no live hook has it.
    pub fn revoke_leaked_hook(&self, secret: &str) -> Option<String> {
        let hook = self.store.hook_by_secret(secret).ok()??;
        self.store.delete_hook(&hook.project, &hook.id).ok()?;
        tracing::warn!(project = %hook.project, hook = %hook.id, "previews: a leaked deploy hook was revoked");
        Some(format!("previews' deploy hook {:?} of project {}", hook.name, hook.project))
    }

    /// A link to the preview for someone outside the team, open for `hours` (1–720), on its own
    /// token: the answer is the only time the link is shown. Only where the preview isn't public
    /// (`access: link` or `team`); a public one needs none.
    pub fn share(&self, id: &str, hours: u32, by: &str) -> Result<(crate::Share, String)> {
        if !(1..=KEEP_MAX_HOURS).contains(&hours) {
            return Err(Error::BadRequest(format!("hours: 1 to {KEEP_MAX_HOURS}")));
        }
        let r = self.store.row(id)?;
        if r.desired == Desired::Deleted {
            return Err(Error::Conflict("the preview is being deleted".into()));
        }
        if !self.store.access(id)?.is_some_and(|a| a.required()) {
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
    /// `behind`: the signed-in person a token acts for, checked against the author as `by` is.
    pub async fn accept_visual(&self, id: &str, by: &str, behind: Option<&str>, person: bool, seen: Option<&str>, page: Option<&str>) -> Result<Preview> {
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
            && [Some(by), behind].into_iter().flatten().any(|who| who.eq_ignore_ascii_case(&c.author) || who.eq_ignore_ascii_case(c.author.trim_start_matches('@')))
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

    /// Someone accepts a change's regressions against main's preview at its head (`seen`: the head
    /// they looked at; a newer one is a 409), as a visual change is accepted: not by its own author,
    /// and a required rule's only by a person.
    pub async fn accept_regressions(&self, id: &str, by: &str, behind: Option<&str>, person: bool, seen: Option<&str>) -> Result<Preview> {
        let r = self.store.row(id)?;
        if r.change_id.is_none() {
            return Err(Error::BadRequest("only a change's preview has regressions against main".into()));
        }
        if !person && self.store.settings(&r.project_id)?.regressions == crate::settings::RegressionsRule::Required {
            return Err(Error::Forbidden("required regressions are accepted by a person, in roll, not by a token".into()));
        }
        // Its author, read for sure (an error refuses, it doesn't pass), and known.
        let change = r.change_id.as_deref().unwrap_or_default();
        let c = self.source.change(change).await.map_err(|e| Error::Internal(format!("reading the change's author: {e}")))?.ok_or_else(|| Error::NotFound(format!("no change {change}")))?;
        if c.author.is_empty() || c.author == "github" {
            return Err(Error::Conflict("the change's author isn't known, so nobody can be told apart from them".into()));
        }
        if [Some(by), behind].into_iter().flatten().any(|who| who.eq_ignore_ascii_case(&c.author) || who.eq_ignore_ascii_case(c.author.trim_start_matches('@'))) {
            return Err(Error::Forbidden("the change's own author can't accept its regressions: someone else looks at them".into()));
        }
        if seen.is_some_and(|s| s != r.head) {
            return Err(Error::Conflict(format!("a newer push came in (at {}): look at it first", short(&r.head))));
        }
        // Something compared at this head to accept: what it is goes with the acceptance.
        let view = self.store.view(r.clone())?;
        let what = crate::RegressionsAccepted::of(view.slower_routes.as_ref(), view.new_errors.as_ref());
        if !view.regressions_compared || what.is_empty() {
            return Err(Error::Conflict("nothing to accept: it has no regressions against main's preview at this push (or isn't compared yet)".into()));
        }
        let at = self.store.now_ms()?;
        self.store.set_runtime_fact(id, "regressions_accepted", Some(&crate::RegressionsAccepted { head: r.head.clone(), by: by.to_string(), at, person, what, errors_total: view.new_errors.as_ref().map_or(0, |n| n.total) }))?;
        self.store.log(id, &format!("{by} accepted its regressions against main at {}", short(&r.head)))?;
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
        // Only a branch that's ready and masked with today's rules: one still being masked (or
        // whose masking failed, or whose rules changed since) holds the parent's values.
        let s = self.store.settings(&r.project_id)?;
        let mut out = Vec::new();
        for res in self.store.resources(id)? {
            let (Some(branch), Ok(b)) = (&res.r#ref, self.brancher(res.kind)) else { continue };
            if res.state != ResourceState::Ready || self.store.mask_digest(id, res.kind, &res.parent)?.unwrap_or_default() != mask_digest(&s, &res) {
                continue;
            }
            let vars = b.connect(&res.parent, branch, res.kept.as_deref()).await.map_err(|e| Error::Internal(format!("connecting to the {} branch: {e:#}", res.kind.as_str())))?;
            out.push(crate::Connection { kind: res.kind, parent: res.parent.clone(), vars: vars.into_iter().collect() });
        }
        Ok(out)
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
            // A person's own reference into roll secrets waits for its maker-checked binding
            // (roll secrets' `bind_ref`); the project's rules (`env`, `env_for`) take one now.
            if v.as_deref().is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("roll://")) {
                return Err(Error::BadRequest(format!("{k}: a preview's own variable can't be a roll:// reference yet; put it in the project's env rules")));
            }
        }
        // Masked from now on, before its services (or roll apps) are given it.
        self.add_masks(id, masks::secret_values(&[], &[], &changes.values().flatten().map(|v| (String::new(), v.clone())).collect::<Vec<_>>(), &[]));
        self.stale_masks(id);
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

    /// Its latest requests through roll, newest first (on its holder, which serves them).
    /// Its paths masked as its logs are: a path is its code's to choose (`/x/<secret>`).
    pub async fn requests(&self, id: &str) -> Result<Vec<crate::RequestLine>> {
        self.store.row(id)?;
        let mut lines = self.apps.requests(id);
        if lines.is_empty() {
            return Ok(lines);
        }
        let paths = self.masked(id, lines.iter().map(|l| l.path.clone()).collect()).await?;
        for (l, p) in lines.iter_mut().zip(paths) {
            l.path = p;
        }
        Ok(lines)
    }

    /// What its services use right now (on its holder, which runs them): the running ones. Read
    /// at most once per [`METRICS_FOR`] (each read is a `docker stats` or a roll apps call per
    /// service), whoever asks.
    pub async fn metrics(&self, id: &str) -> Result<Vec<crate::ServiceMetrics>> {
        self.store.row(id)?;
        let now = self.store.now_ms()?;
        if let Some((at, m)) = self.metrics_read.lock().get(id)
            && now - at < METRICS_FOR
        {
            return Ok(m.clone());
        }
        let services = self.store.services(id)?;
        let got = futures::future::join_all(services.iter().map(|s| self.apps.metrics(id, &s.name))).await;
        let m: Vec<crate::ServiceMetrics> = services
            .into_iter()
            .zip(got)
            .filter_map(|(s, m)| m.map(|(cpu, memory_bytes)| crate::ServiceMetrics { service: s.name, cpu, memory_bytes, memory_mb: s.memory_mb }))
            .collect();
        let mut read = self.metrics_read.lock();
        read.retain(|_, (at, _)| now - *at < METRICS_FOR);
        read.insert(id.to_string(), (now, m.clone()));
        Ok(m)
    }

    /// Its services' errors in the last `minutes` ([`error_span`]), grouped, newest first (on its
    /// holder, which runs them). Titles and samples masked as its logs are, a value a cut split
    /// too ([`crate::mask_cut`]): its code can print its secrets. Its values are read only when
    /// there's an error. A fingerprint is keyed ([`Engine::error_fingerprint`]). Read at most once
    /// per [`ERRORS_FOR`] for a span, whoever asks. (roll apps answers its 50 most frequent
    /// groups a service; a `roll.toml` service's newest 50.)
    pub async fn errors(&self, id: &str, minutes: i64) -> Result<Vec<crate::ErrorGroup>> {
        self.store.row(id)?;
        let now = self.store.now_ms()?;
        let key = (id.to_string(), error_span(minutes));
        if let Some((at, g)) = self.errors_read.lock().get(&key)
            && now - at < ERRORS_FOR
        {
            return Ok(g.clone());
        }
        let masks = super::ErrorMasks::new(self, id);
        let services = self.store.services(id)?;
        let got = futures::future::join_all(services.iter().map(|s| self.apps.errors(id, &s.name, key.1, Some(&masks)))).await;
        let mut groups = Vec::new();
        for (s, gs) in services.iter().zip(got) {
            groups.extend(gs?.into_iter().map(|g| crate::ErrorGroup { service: s.name.clone(), ..g }));
        }
        groups.sort_by(|a, b| b.last_seen.cmp(&a.last_seen).then(b.count.cmp(&a.count)));
        if !groups.is_empty() {
            let values = masks.values().await?;
            for g in &mut groups {
                g.title = crate::mask_cut(&g.title, values);
                g.sample = crate::mask_cut(&g.sample, values);
                g.fingerprint = self.error_fingerprint(id, &g.service, &g.fingerprint);
            }
            // A group whose newest sample names a trace the preview keeps opens it.
            // (Only when a sample names something like one: reading them asks roll apps too.)
            let named = groups.iter().any(|g| super::traces::trace_ids_in(&g.sample).next().is_some());
            let kept: std::collections::HashSet<String> = if named { self.trace_rows(id, super::traces::KEEP_MS / 60_000).await?.into_iter().map(|t| t.trace).collect() } else { Default::default() };
            if !kept.is_empty() {
                for g in &mut groups {
                    g.trace = super::traces::trace_ids_in(&g.sample).find(|t| kept.contains(t));
                }
            }
        }
        let mut read = self.errors_read.lock();
        read.retain(|_, (at, _)| now - *at < ERRORS_FOR);
        read.insert(key, (now, groups.clone()));
        Ok(groups)
    }

    /// The previews this node holds that run (a change's or a branch's): their error groups of
    /// the last hour that main's preview doesn't have (by service and fingerprint, unkeyed), kept
    /// on the record (the verdict strip, the change page, a count in the pull request's comment),
    /// the most frequent first. Main's are read once a round, and only where they can be: main's
    /// preview running (`Ready`) on this node; otherwise nothing is said rather than everything.
    /// A preview's values are read to mask titles only when its set moved; a change reports it
    /// again.
    pub async fn read_new_errors(&self) {
        use futures::StreamExt;
        let Ok(rows) = self.store.rows(None) else { return };
        let mut bases: HashMap<(String, String), Option<crate::store::Row>> = HashMap::new();
        let mut mains: HashMap<String, Option<Vec<crate::ErrorGroup>>> = HashMap::new();
        let mut work = Vec::new();
        for r in rows.into_iter().filter(|r| r.desired == Desired::Active && r.state == crate::State::Ready && (r.change_id.is_some() || r.branch.is_some())) {
            if !self.holds(&r.id).await {
                continue;
            }
            let key = (r.project_id.clone(), r.repo.clone());
            if !bases.contains_key(&key) {
                let base = self.main_of(&r.project_id, &r.repo).await.ok().flatten();
                bases.insert(key.clone(), base);
            }
            let main = match bases[&key].clone().filter(|b| b.id != r.id) {
                Some(b) => {
                    if !mains.contains_key(&b.id) {
                        // Asleep, its logs are still there to read (a frozen or stopped container's).
                        let read = if matches!(b.state, crate::State::Ready | crate::State::Sleeping) && self.holds(&b.id).await { self.raw_errors(&b.id).await.ok() } else { None };
                        mains.insert(b.id.clone(), read);
                    }
                    mains[&b.id].clone().map(|groups| (b, groups))
                }
                None => None,
            };
            work.push((r, main));
        }
        let kept: std::collections::HashSet<String> = work.iter().map(|(r, _)| r.id.clone()).collect();
        futures::stream::iter(work).for_each_concurrent(4, |(r, main)| self.new_errors_of(r, main)).await;
        self.new_errors_seen.lock().retain(|id, _| kept.contains(id));
    }

    /// One preview's new errors against main's groups (`None`: nothing of main's to compare with).
    async fn new_errors_of(&self, r: crate::store::Row, main: Option<(crate::store::Row, Vec<crate::ErrorGroup>)>) {
        let kept = match main {
            None => {
                self.new_errors_seen.lock().remove(&r.id);
                None
            }
            Some((base, main)) => {
                let Ok(mine) = self.raw_errors(&r.id).await else { return };
                // Compared at this head (what a required rule waits for); the change's says so.
                if self.store.set_runtime_fact(&r.id, "compared_errors", Some(&r.head)).unwrap_or(false) {
                    self.report(r.clone()).await;
                }
                let theirs: std::collections::HashSet<(&str, &str)> = main.iter().map(|g| (g.service.as_str(), g.fingerprint.as_str())).collect();
                // The same error says each preview's own names (its id, its branches, its hosts):
                // with those taken out, a title main's has too isn't new either.
                let (my_words, main_words) = (self.own_words(&r.id), self.own_words(&base.id));
                let their_titles: std::collections::HashSet<(&str, String)> = main.iter().map(|g| (g.service.as_str(), without_words(&g.title, &main_words))).collect();
                let mut new: Vec<&crate::ErrorGroup> = mine
                    .iter()
                    .filter(|g| !theirs.contains(&(g.service.as_str(), g.fingerprint.as_str())) && !their_titles.contains(&(g.service.as_str(), without_words(&g.title, &my_words))))
                    .collect();
                new.sort_by(|a, b| b.count.cmp(&a.count).then(b.last_seen.cmp(&a.last_seen)));
                let total = new.len();
                new.truncate(crate::NewErrors::MAX);
                let seen: Vec<String> = new.iter().map(|g| format!("{}\0{}", g.service, g.fingerprint)).chain([total.to_string(), r.head.clone()]).collect();
                if self.new_errors_seen.lock().get(&r.id) == Some(&seen) {
                    return;
                }
                let kept = if total == 0 {
                    None
                } else {
                    let Ok(values) = super::ErrorMasks::new(self, &r.id).values().await.map(<[String]>::to_vec) else { return };
                    // A title was cut (at 300) before it was masked: shown only to 120, short of
                    // where a cut could have split a value.
                    let errors = new.iter().map(|g| crate::NewError { service: g.service.clone(), title: crate::mask_cut(&g.title, &values).chars().take(120).collect() }).collect();
                    Some(crate::NewErrors { branch: base.branch.clone().unwrap_or_else(|| "main".into()), total, errors, head: r.head.clone() })
                };
                self.new_errors_seen.lock().insert(r.id.clone(), seen);
                kept
            }
        };
        if self.store.set_runtime_fact(&r.id, "new_errors", kept.as_ref()).unwrap_or(false) {
            self.report(r).await;
        }
    }

    /// What differs between two previews' output for the same error: a preview's id and slug, its
    /// branches' refs and its services' hosts (each of 4 characters or more), longest first.
    fn own_words(&self, id: &str) -> Vec<String> {
        let mut words = vec![id.to_string()];
        if let Ok(r) = self.store.row(id) {
            words.push(r.slug);
        }
        words.extend(self.store.resources(id).unwrap_or_default().into_iter().filter_map(|x| x.r#ref));
        words.extend(self.store.services(id).unwrap_or_default().into_iter().map(|s| s.host));
        words.retain(|w| w.len() >= 4);
        words.sort_by_key(|w| std::cmp::Reverse(w.len()));
        words.dedup();
        words
    }

    /// Its services' error groups of the last hour as the backends group them: fingerprints
    /// unkeyed (to compare with another preview's), titles unmasked (never shown as they are).
    async fn raw_errors(&self, id: &str) -> Result<Vec<crate::ErrorGroup>> {
        let mut groups = Vec::new();
        for s in self.store.services(id)? {
            groups.extend(self.apps.errors(id, &s.name, 60, None).await?.into_iter().map(|g| crate::ErrorGroup { service: s.name.clone(), ..g }));
        }
        Ok(groups)
    }

    /// How many errors its services wrote in the last `minutes` ([`error_span`]): a list's rows.
    /// Counted from the groups [`Engine::errors`] lists, but nothing they say leaves, so nothing is
    /// masked and no values are read. Kept as long.
    pub async fn error_count(&self, id: &str, minutes: i64) -> Result<u64> {
        self.store.row(id)?;
        let now = self.store.now_ms()?;
        let key = (id.to_string(), error_span(minutes));
        if let Some((at, n)) = self.error_counts_read.lock().get(&key)
            && now - at < ERRORS_FOR
        {
            return Ok(*n);
        }
        if let Some((at, g)) = self.errors_read.lock().get(&key)
            && now - at < ERRORS_FOR
        {
            return Ok(g.iter().map(|g| g.count).sum());
        }
        let services = self.store.services(id)?;
        let got = futures::future::join_all(services.iter().map(|s| self.apps.errors(id, &s.name, key.1, None))).await;
        let mut n = 0;
        for gs in got {
            n += gs?.iter().map(|g| g.count).sum::<u64>();
        }
        let mut read = self.error_counts_read.lock();
        read.retain(|_, (at, _)| now - *at < ERRORS_FOR);
        read.insert(key, (now, n));
        Ok(n)
    }

    /// A group's fingerprint as answered: its preview, service and backend fingerprint under this
    /// process's random key, so it names the group without being a hash anyone could match
    /// against guesses of what the error said.
    fn error_fingerprint(&self, id: &str, service: &str, fingerprint: &str) -> String {
        use hmac::Mac;
        let mut m = hmac::Hmac::<sha2::Sha256>::new_from_slice(&self.errors_key).expect("any key length");
        for part in [id, service, fingerprint] {
            m.update(part.as_bytes());
            m.update(&[0]);
        }
        hex::encode(&m.finalize().into_bytes()[..8])
    }

    pub async fn logs(&self, id: &str, service: Option<&str>, lines: usize) -> Result<(String, Vec<String>)> {
        let services = self.store.services(id)?;
        let svc = match service {
            Some(name) => services.into_iter().find(|s| s.name == name).ok_or_else(|| Error::NotFound(format!("the preview has no service {name}")))?,
            None => services.into_iter().next().ok_or_else(|| Error::NotFound("the preview has no services".into()))?,
        };
        let got = self.apps.logs(id, &svc.name, lines).await;
        Ok((svc.name.clone(), self.masked(id, got).await?))
    }

    /// A service's last `lines` and then its output as it comes, each line with whether it came on
    /// stderr. Where the product can't follow, its tail is read every [`FOLLOW_POLL`] and what's
    /// new passed on (as stdout: a tail doesn't say). Ends when the receiver is dropped (or the
    /// service's output does).
    pub async fn follow(self: &Arc<Self>, id: &str, service: Option<&str>, lines: usize) -> Result<(String, tokio::sync::mpsc::Receiver<(bool, String)>)> {
        let (name, _) = self.logs(id, service, 1).await?;
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let (me, id, svc) = (self.clone(), id.to_string(), name.clone());
        // Every line masked on its way out (`masks.rs`), with the masks read again as they age.
        if let Some(mut from) = self.apps.follow(&id, &name, lines).await {
            tokio::spawn(async move {
                while let Some((stderr, l)) = from.recv().await {
                    let Ok(l) = me.masked(&id, vec![l]).await else { return };
                    if tx.send((stderr, l.into_iter().next().unwrap_or_default())).await.is_err() {
                        return;
                    }
                }
            });
            return Ok((name, rx));
        }
        tokio::spawn(async move {
            // The first read is what was asked for; later ones read wide, so a burst between two
            // reads isn't lost.
            let (mut seen, mut first): (Vec<String>, bool) = (Vec::new(), true);
            loop {
                let now = me.apps.logs(&id, &svc, if first { lines } else { FOLLOW_WINDOW }).await;
                let from = match resume_at(&seen, &now) {
                    Some(i) => i,
                    None => {
                        let _ = tx.send((false, "… (lines skipped: more came between two reads than a read sees)".into())).await;
                        0
                    }
                };
                let Ok(shown) = me.masked(&id, now[from..].to_vec()).await else { return };
                for l in shown {
                    if tx.send((false, l)).await.is_err() {
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

/// A pull request's link as the sheet may show it: `https://` only (`""` otherwise), so a link
/// can't run script.
/// A pull request's page as roll apps built it (from the service's source and the number), kept
/// to tell its repository (`pull_repo`): whole when it's one printable URL of at most 500
/// bytes. Shown as a link only when `https://` (the preview's view).
fn kept_url(url: &str) -> &str {
    if url.len() <= 500 && url.contains("://") && !url.chars().any(|c| c.is_whitespace() || c.is_control()) { url } else { "" }
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

