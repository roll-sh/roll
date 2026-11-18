//! What the API asks of previews: create, delete, refresh, reset, sleep, wake, logs.

use super::*;

/// How long a preview's metrics are kept before they're read again (ms).
const METRICS_FOR: i64 = 10_000;

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
        for project in projects {
            let rows = self.store.rows(Some(&project.id))?;
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
                        self.store.set_git_pull(&r.id, pl.number as i64, https_only(&pl.url), pl.fork, sha)?;
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
                    let s = self.store.settings(&project.id)?;
                    let skip = [Some(p.message.as_str()), pull_title.as_deref()].into_iter().flatten().any(|t| t.to_ascii_lowercase().contains("[skip preview]"));
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
                        self.store.set_git_pull(&row.id, pl.number as i64, https_only(&pl.url), pl.fork, sha)?;
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
        // roll apps holds a pv- environment's instance to Roll only (apps' #143), not its build:
        // the build sees the environment's variables on an open network, so a fork's head there
        // would send its branched data wherever it liked. Not released under Roll only.
        let row = self.store.row(id)?;
        if row.repo.starts_with("git:") && self.store.settings(&row.project_id)?.egress_for(row.trusted) == crate::settings::EgressRule::Roll {
            return Err(Error::Conflict(
                "this project's forks reach roll only (egress_forks), and roll apps builds a fork's head with its variables on an open network: it isn't approved to run there (open forks' egress to run it)".into(),
            ));
        }
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

    pub async fn logs(&self, id: &str, service: Option<&str>, lines: usize) -> Result<(String, Vec<String>)> {
        let services = self.store.services(id)?;
        let svc = match service {
            Some(name) => services.into_iter().find(|s| s.name == name).ok_or_else(|| Error::NotFound(format!("the preview has no service {name}")))?,
            None => services.into_iter().next().ok_or_else(|| Error::NotFound("the preview has no services".into()))?,
        };
        let got = self.apps.logs(id, &svc.name, lines).await;
        Ok((svc.name.clone(), self.masked(id, got).await?))
    }

    /// A service's last `lines` and then its output as it comes. Where the product can't follow,
    /// its tail is read every [`FOLLOW_POLL`] and what's new passed on. Ends when the receiver is
    /// dropped (or the service's output does).
    pub async fn follow(self: &Arc<Self>, id: &str, service: Option<&str>, lines: usize) -> Result<(String, tokio::sync::mpsc::Receiver<String>)> {
        let (name, _) = self.logs(id, service, 1).await?;
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let (me, id, svc) = (self.clone(), id.to_string(), name.clone());
        // Every line masked on its way out (`masks.rs`), with the masks read again as they age.
        if let Some(mut from) = self.apps.follow(&id, &name, lines).await {
            tokio::spawn(async move {
                while let Some(l) = from.recv().await {
                    let Ok(l) = me.masked(&id, vec![l]).await else { return };
                    if tx.send(l.into_iter().next().unwrap_or_default()).await.is_err() {
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
                        let _ = tx.send("… (lines skipped: more came between two reads than a read sees)".into()).await;
                        0
                    }
                };
                let Ok(shown) = me.masked(&id, now[from..].to_vec()).await else { return };
                for l in shown {
                    if tx.send(l).await.is_err() {
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
fn https_only(url: &str) -> &str {
    if url.starts_with("https://") && !url.chars().any(char::is_whitespace) { url } else { "" }
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

