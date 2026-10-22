//! Owners, repos, writes and landing, changes and work items.

use crate::db::{NewChange, NewWork, now};
use crate::model::*;
use crate::{Caller, Code, Error, Result, cs_id, hash_token, names, new_id, new_token};
use base64::Engine;
use bytes::Bytes;
use code_store::{ChangesetId, Conflict, Edit, LandError, Landed, Mode, NewChangeset, RepoPath, Signature};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

/// One file in a write: text, base64, or a delete.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct FileWrite {
    pub content: Option<String>,
    pub content_b64: Option<String>,
    #[serde(default)]
    pub delete: bool,
    pub mode: Option<String>,
}

/// A changeset given by id, or written from files on a base.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct WriteFiles {
    pub changeset: Option<String>,
    pub base: Option<String>,
    pub message: Option<String>,
    /// `{name, email}` for the changeset's author. A plain string (a change's author) only names it.
    pub author: Option<Value>,
    pub files: Option<BTreeMap<String, FileWrite>>,
}

pub enum LandOutcome {
    Landed(Landed),
    Conflict(Conflict),
}

/// Largest file accepted through the JSON write API.
const MAX_WRITE_FILE: usize = 32 << 20;
/// Most files in one JSON write.
const MAX_WRITE_FILES: usize = 10_000;
const DEFAULT_LEASE_S: i64 = 900;

pub(crate) fn short_ref(name: &str) -> String {
    name.strip_prefix("refs/heads/").unwrap_or(name).to_string()
}

pub(crate) fn landed_json(l: &Landed) -> Value {
    json!({
        "status": "landed",
        "changeset": cs_id(&l.changeset),
        "git_commit": l.git_commit.hex(),
        "ref": short_ref(&l.ref_name),
        "generation": l.generation,
        "fast_forward": l.fast_forward,
        "merged_paths": l.merged_paths.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
    })
}

pub(crate) fn conflict_json(c: &Conflict, ref_name: &str) -> Value {
    let paths: Vec<&str> = c.paths.iter().map(|p| p.path.as_str()).collect();
    json!({
        "status": "conflict",
        "error": format!("conflict in {}", paths.join(", ")),
        "paths": paths,
        "conflicts": c.paths,
        "head": c.head.as_ref().map(cs_id).unwrap_or_default(),
        "ref": short_ref(ref_name),
    })
}

impl Code {
    // ── names and owners ──

    pub fn check_name(&self, name: &str) -> Result<Value> {
        let (available, reason, message) = match names::owner(name) {
            Err((e, msg)) => (false, Some(e.reason()), Some(msg)),
            Ok(n) => match self.db.owner(&n)? {
                Some(_) => (false, Some("taken"), Some(format!("{n} is taken"))),
                None => (true, None, None),
            },
        };
        Ok(json!({ "name": name.trim().to_ascii_lowercase(), "available": available, "reason": reason, "message": message }))
    }

    /// Is `name` free for a new repo of `owner`? A name renamed away from counts as free. Only
    /// callers who can read a private repo learn that it exists; nobody else can create repos
    /// under that owner anyway.
    pub fn check_repo_name(&self, caller: &Caller, owner: &str, name: &str) -> Result<Value> {
        let o = self.owner_or_404(owner)?;
        let (available, reason, message) = match names::repo(name) {
            Err(msg) => (false, Some("invalid"), Some(msg)),
            Ok(n) => match self.db.repo(&o.id, &n)?.filter(|r| Self::can_read(caller, r)) {
                Some(r) => (false, Some("taken"), Some(format!("{} already exists", r.full_name()))),
                None => (true, None, None),
            },
        };
        Ok(json!({ "name": name.trim(), "available": available, "reason": reason, "message": message }))
    }

    pub fn create_owner(&self, caller: &Caller, name: &str, kind: OwnerKind, display_name: Option<&str>) -> Result<Owner> {
        if *caller != Caller::Admin {
            return Err(Error::Forbidden("creating owners needs the admin token".into()));
        }
        let name = names::owner(name).map_err(|(_, msg)| Error::Unprocessable(msg))?;
        let o = Owner {
            id: new_id("own"),
            display_name: display_name.map(str::trim).filter(|d| !d.is_empty()).unwrap_or(&name).chars().take(100).collect(),
            name,
            kind,
            bio: None,
            url: None,
            created_at: now(),
            repos_count: 0,
        };
        if !self.db.insert_owner(&o)? {
            return Err(Error::Conflict(format!("{} is taken", o.name)));
        }
        Ok(o)
    }

    pub fn owner_or_404(&self, name: &str) -> Result<Owner> {
        self.db.owner(name)?.ok_or_else(|| Error::NotFound(format!("owner {name}")))
    }

    pub fn owns(caller: &Caller, owner_id: &str) -> bool {
        match caller {
            Caller::Admin => true,
            Caller::Owner { id, .. } => id == owner_id,
            Caller::App { .. } | Caller::Anonymous => false,
        }
    }

    /// Can `caller` manage the owner: its tokens, review apps, billing, providers, webhooks and
    /// repo settings? The owner (or admin), but not a token bound to an agent.
    pub fn manages(caller: &Caller, owner_id: &str) -> bool {
        Self::owns(caller, owner_id) && caller.agent().is_none()
    }

    /// [`Self::manages`] for a repo's owner, as an error.
    pub fn check_manage(&self, caller: &Caller, r: &RepoRow) -> Result<()> {
        self.check_write(caller, r)?;
        match caller.agent() {
            Some(a) => Err(Error::Forbidden(format!("agent tokens (agent:{a}) can't change {}'s settings", r.full_name()))),
            None => Ok(()),
        }
    }

    pub fn update_owner(&self, caller: &Caller, name: &str, patch: &Value) -> Result<Owner> {
        let o = self.owner_or_404(name)?;
        if !Self::manages(caller, &o.id) {
            return Err(Error::Forbidden(format!("not allowed to change {}", o.name)));
        }
        let s = |k: &str| patch.get(k).and_then(Value::as_str).map(|v| v.trim().chars().take(500).collect::<String>());
        let display = s("display_name").filter(|d| !d.is_empty());
        self.db.update_owner(&o.id, display.as_deref(), s("bio").as_deref(), s("url").as_deref())?;
        self.owner_or_404(name)
    }

    pub fn create_token(&self, caller: &Caller, owner: &str, name: &str) -> Result<Value> {
        let o = self.owner_or_404(owner)?;
        if !Self::manages(caller, &o.id) {
            return Err(Error::Forbidden(format!("not allowed to create tokens for {}", o.name)));
        }
        let token = new_token();
        let name = if name.trim().is_empty() { "token" } else { name.trim() };
        let id = crate::new_id("tok");
        let prefix: String = token.chars().take(8).collect();
        self.db.insert_token(&hash_token(&token), &id, &prefix, &o.id, name)?;
        Ok(json!({ "token": token, "id": id, "prefix": prefix, "name": name, "owner": o.name, "created_at": now() }))
    }

    /// `GET /owners/{owner}/tokens`: the owner's tokens, never the secrets. Only the owner (or
    /// admin) sees them; anyone else gets 404, as for billing.
    pub fn list_tokens(&self, caller: &Caller, owner: &str) -> Result<Vec<Value>> {
        let o = self.owner_or_404(owner)?;
        if !Self::manages(caller, &o.id) {
            return Err(Error::NotFound(format!("owner {owner}")));
        }
        Ok(self.db.tokens(&o.id)?)
    }

    /// `DELETE /owners/{owner}/tokens/{id}`: revoke a token. It stops working at once: every
    /// request (API, git, MCP) looks the token up.
    pub fn revoke_token(&self, caller: &Caller, owner: &str, id: &str) -> Result<()> {
        let o = self.owner_or_404(owner)?;
        match caller {
            Caller::Anonymous => return Err(Error::Unauthorized),
            c if !Self::manages(c, &o.id) => return Err(Error::Forbidden(format!("not allowed to revoke tokens of {}", o.name))),
            _ => {}
        }
        if !self.db.delete_token(&o.id, id)? {
            return Err(Error::NotFound(format!("token {id}")));
        }
        Ok(())
    }

    // ── repos ──

    /// A repo by full name. A name that was renamed away answers [`Error::Moved`].
    pub fn find_repo(&self, owner: &str, name: &str) -> Result<RepoRow> {
        let name = name.strip_suffix(".git").unwrap_or(name);
        let not_found = || Error::NotFound(format!("repository {owner}/{name}"));
        let o = self.db.owner(owner)?.ok_or_else(not_found)?;
        if let Some(r) = self.db.repo(&o.id, name)? {
            return Ok(r);
        }
        if let Some(id) = self.db.redirect(&o.id, name)?
            && let Some(r) = self.db.repo_by_id(&id)?
        {
            return Err(Error::Moved(r.full_name()));
        }
        Err(not_found())
    }

    pub fn can_read(caller: &Caller, r: &RepoRow) -> bool {
        match caller {
            // A review app reads the repos it's installed on (private ones too).
            Caller::App { owner_id, repos, .. } if *owner_id == r.owner_id && (repos.is_empty() || repos.contains(&r.id)) => true,
            _ => r.visibility == Visibility::Public || Self::owns(caller, &r.owner_id),
        }
    }

    /// A repo the caller can read. Private repos are invisible (404) to everyone else.
    pub fn readable(&self, caller: &Caller, owner: &str, name: &str) -> Result<RepoRow> {
        let r = match self.find_repo(owner, name) {
            // Only say where a repo went to callers who can read it there.
            Err(Error::Moved(to)) => {
                let (o, n) = to.split_once('/').unwrap_or((&to, ""));
                return Err(match self.find_repo(o, n) {
                    Ok(target) if Self::can_read(caller, &target) => Error::Moved(to),
                    _ => Error::NotFound(format!("repository {owner}/{name}")),
                });
            }
            r => r?,
        };
        if !Self::can_read(caller, &r) {
            return Err(Error::NotFound(format!("repository {owner}/{name}")));
        }
        Ok(r)
    }

    pub fn writable(&self, caller: &Caller, owner: &str, name: &str) -> Result<RepoRow> {
        let r = self.readable(caller, owner, name)?;
        self.check_write(caller, &r)?;
        Ok(r)
    }

    pub fn check_write(&self, caller: &Caller, r: &RepoRow) -> Result<()> {
        match caller {
            _ if Self::owns(caller, &r.owner_id) => Ok(()),
            Caller::Anonymous => Err(Error::Unauthorized),
            _ => Err(Error::Forbidden(format!("no write access to {}", r.full_name()))),
        }
    }

    pub fn repo_by_id(&self, id: &str) -> Result<RepoRow> {
        self.db.repo_by_id(id)?.ok_or_else(|| Error::NotFound(format!("repository {id}")))
    }

    pub async fn create_repo(self: &Arc<Self>, caller: &Caller, input: &Value) -> Result<RepoRow> {
        let s = |k: &str| input.get(k).and_then(Value::as_str);
        let owner_name = s("owner").ok_or_else(|| Error::Invalid("owner is required".into()))?;
        let owner = self.owner_or_404(owner_name)?;
        if !Self::owns(caller, &owner.id) {
            return Err(if *caller == Caller::Anonymous { Error::Unauthorized } else { Error::Forbidden(format!("not allowed to create repositories for {}", owner.name)) });
        }
        let name = names::repo(s("name").unwrap_or("")).map_err(Error::Unprocessable)?;
        let mut default_branch = names::branch(s("default_branch").unwrap_or("main")).map_err(Error::Invalid)?;
        let mut visibility = match s("visibility") {
            None | Some("public") => Visibility::Public,
            Some("private") => Visibility::Private,
            Some(v) => return Err(Error::Invalid(format!("visibility must be public or private, not {v}"))),
        };
        let github = match input.get("source") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s == "native" => None,
            Some(Value::Object(o)) if o.contains_key("github") => {
                // Any installation's repositories could be imported this way (see
                // `api::github_installations`).
                if *caller != Caller::Admin {
                    return Err(Error::Forbidden("connecting a GitHub repository needs the admin token".into()));
                }
                let spec = crate::github::spec(&o["github"])?;
                Some(crate::github::prepare(self, &spec).await?)
            }
            Some(_) => return Err(Error::Invalid("source must be \"native\" or {github: {…}}".into())),
        };
        if let Some(g) = &github {
            // GitHub decides the default branch; privacy follows GitHub unless asked otherwise.
            default_branch = g.default_branch.clone();
            if g.private && s("visibility").is_none() {
                visibility = Visibility::Private;
            }
        }
        let source = if github.is_some() { "github" } else { "native" };
        let t = now();
        let row = RepoRow {
            id: new_id("repo"),
            owner_id: owner.id.clone(),
            owner: owner.name.clone(),
            name,
            description: s("description").map(|d| d.trim().chars().take(350).collect::<String>()).filter(|d| !d.is_empty()),
            visibility,
            source: source.into(),
            default_branch,
            land_policy: LandPolicy::PostMerge,
            required_checks: Vec::new(),
            github: github.as_ref().map(|g| serde_json::to_value(&g.link).unwrap_or_default()),
            import: github.as_ref().map(|_| json!({ "state": "importing", "commits": 0, "total": null, "error": null })),
            created_at: t,
            updated_at: t,
        };
        self.store.create_repo(&row.id)?;
        if !self.db.insert_repo(&row)? {
            self.store.delete_repo(&row.id).await?;
            return Err(Error::Conflict(format!("{} already exists", row.full_name())));
        }
        if github.is_some() {
            crate::github::connected(self, &row.id);
        }
        Ok(row)
    }

    pub async fn update_repo(&self, caller: &Caller, owner: &str, name: &str, patch: &Value) -> Result<RepoRow> {
        let mut r = self.writable(caller, owner, name)?;
        self.check_manage(caller, &r)?;
        let s = |k: &str| patch.get(k).and_then(Value::as_str);
        if let Some(n) = s("name") {
            let n = names::repo(n).map_err(Error::Unprocessable)?;
            if n != r.name {
                if !self.db.rename_repo(&r, &n)? {
                    return Err(Error::Conflict(format!("{}/{n} already exists", r.owner)));
                }
                r.name = n;
            }
        }
        if let Some(d) = patch.get("description") {
            r.description = d.as_str().map(|d| d.trim().chars().take(350).collect::<String>()).filter(|d| !d.is_empty());
        }
        if let Some(v) = s("visibility") {
            r.visibility = match v {
                "public" => Visibility::Public,
                "private" => Visibility::Private,
                _ => return Err(Error::Invalid("visibility must be public or private".into())),
            };
        }
        if let Some(p) = s("land_policy") {
            r.land_policy = LandPolicy::parse(p).ok_or_else(|| Error::Invalid("land_policy must be post_merge or queue".into()))?;
        }
        if let Some(c) = patch.get("required_checks") {
            let list: Vec<String> = serde_json::from_value(c.clone()).map_err(|_| Error::Invalid("required_checks must be a list of names".into()))?;
            r.required_checks = list.into_iter().map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect();
            r.required_checks.dedup();
        }
        if let Some(b) = s("default_branch") {
            r.default_branch = names::branch(b).map_err(Error::Invalid)?;
        }
        self.db.update_repo_settings(&r)?;
        self.repo_by_id(&r.id)
    }

    pub async fn delete_repo(&self, caller: &Caller, owner: &str, name: &str) -> Result<()> {
        let r = self.writable(caller, owner, name)?;
        self.check_manage(caller, &r)?;
        crate::github::forget(self, &r.id);
        if let Some(i) = &self.ai.index {
            i.remove(&r.id);
        }
        // Previews hold containers and data branches; nothing else would ever stop them.
        let backend = self.previews.read().clone();
        for c in self.db.changes(&r.id, None, usize::MAX)?.into_iter().filter(|c| c.preview.is_some()) {
            if let Some(b) = &backend
                && let Err(e) = b.delete(&r, &c).await
            {
                tracing::warn!(change = %c.id, error = %e, "removing a deleted repo's preview");
            }
            let _ = tokio::fs::remove_dir_all(self.config.home.join("code").join("previews").join(&c.id)).await;
        }
        self.db.delete_repo(&r.id)?;
        // Its queue worker notices the repo is gone and stops.
        self.unsettle(&r.id);
        self.drop_worktrees(&r.id);
        self.forget_repo_hooks(&r.id)?;
        self.store.delete_repo(&r.id).await?;
        Ok(())
    }

    /// The default branch's head, if anything has landed.
    pub fn head(&self, r: &RepoRow) -> Result<Option<ChangesetId>> {
        Ok(self.store.get_ref(&r.id, &code_store::full_ref(&r.default_branch))?)
    }

    pub fn clone_url(&self, r: &RepoRow) -> String {
        format!("{}/{}.git", self.config.git_url.trim_end_matches('/'), r.full_name())
    }

    pub fn web_url(&self, r: &RepoRow) -> String {
        format!("{}/{}", self.config.web_url.trim_end_matches('/'), r.full_name())
    }

    pub async fn repo_view(&self, r: &RepoRow) -> Result<Repo> {
        let head = self.head(r)?;
        let (head_git, generation) = match &head {
            Some(h) => (
                Some(self.store.derive_git(&r.id, h)?.0.hex()),
                self.store.meta(&r.id, h)?.map(|m| m.generation).unwrap_or(0),
            ),
            None => (None, 0),
        };
        let (languages, readme, contributors) = match &head {
            Some(h) => (
                (*self.languages_at(r, h).await?).clone(),
                self.readme_path(r, h)?,
                self.contributors_at(r, h).await?,
            ),
            None => (BTreeMap::new(), None, 0),
        };
        let mut stats = self.repo_stats(r)?;
        stats.contributors = contributors;
        Ok(Repo {
            id: r.id.clone(),
            owner: r.owner.clone(),
            name: r.name.clone(),
            full_name: r.full_name(),
            description: r.description.clone(),
            visibility: r.visibility,
            source: r.source.clone(),
            default_branch: r.default_branch.clone(),
            head: head.as_ref().map(cs_id),
            head_git,
            generation,
            created_at: r.created_at,
            updated_at: r.updated_at,
            land_policy: r.land_policy,
            required_checks: r.required_checks.clone(),
            clone_url: self.clone_url(r),
            web_url: self.web_url(r),
            github: r.github.clone(),
            import: r.import.clone(),
            languages,
            readme,
            stats,
        })
    }

    fn rate(&self, repo_id: &str) -> Result<(u64, f64)> {
        let now_s = now();
        let minutes_today = (now_s % 86_400) / 60 + 1;
        let per_minute = self.store.land_rate(repo_id, minutes_today)?;
        let today: u64 = per_minute.iter().map(|(_, c)| c).sum();
        // The last 60 seconds, from the current (partial) minute and the share of the previous one.
        let last2 = self.store.land_rate(repo_id, 2)?;
        let f = (now_s % 60) as f64 / 60.0;
        let (prev, cur) = (last2.first().map(|x| x.1).unwrap_or(0) as f64, last2.last().map(|x| x.1).unwrap_or(0) as f64);
        Ok((today, (cur + prev * (1.0 - f)) / 60.0))
    }

    pub fn repo_stats(&self, r: &RepoRow) -> Result<RepoStats> {
        let s = self.store.stats(&r.id)?;
        let (merges_today, merges_per_s) = self.rate(&r.id)?;
        Ok(RepoStats {
            merges_today,
            merges_per_s,
            // Waiting for the lander, plus waiting in the merge queue.
            queue_depth: self.store.queue_depth(&r.id) as u64 + self.db.queue_depth(&r.id)?,
            changesets: s.changesets,
            open_changes: self.db.count_changes(&r.id, &["open", "queued", "conflict"])?,
            open_work: self.db.count_work(&r.id, &["open", "claimed", "review"])?,
            contributors: 0,
            merges_daily: self.store.land_daily(&r.id, 30)?.into_iter().map(|(_, c)| c).collect(),
            queue_batch: self.queue_batch_len(&r.id),
        })
    }

    pub fn stats(&self) -> Result<Stats> {
        let mut out = Stats::default();
        for r in self.db.repos(None)? {
            let (today, rate) = self.rate(&r.id)?;
            out.merges_today += today;
            out.merges_per_s += rate;
            out.queue_depth += self.store.queue_depth(&r.id) as u64 + self.db.queue_depth(&r.id)?;
        }
        Ok(out)
    }

    // ── writing and landing ──

    fn signature(&self, caller: &Caller, author: Option<&Value>) -> Signature {
        let (name, email) = match author {
            Some(Value::String(n)) => (Some(n.as_str()), None),
            Some(v) => (v.get("name").and_then(Value::as_str), v.get("email").and_then(Value::as_str)),
            None => (None, None),
        };
        match name.map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => {
                let email = email.map(str::to_string).unwrap_or_else(|| format!("{}@users.roll.sh", n.replace(' ', "-")));
                Signature::now(n, &email)
            }
            _ => match caller.identity() {
                Some(name) if name != "admin" => Signature::now(&name, &format!("{name}@users.roll.sh")),
                _ => Signature::now("roll", "code@roll.sh"),
            },
        }
    }

    /// The changeset a write names, or a new one written from files.
    pub async fn write(&self, r: &RepoRow, caller: &Caller, w: &WriteFiles) -> Result<ChangesetId> {
        if let Some(id) = &w.changeset {
            return Ok(self.store.resolve_or_404(&r.id, id)?);
        }
        let files = w.files.as_ref().ok_or_else(|| Error::Invalid("give a changeset, or files to write".into()))?;
        if files.is_empty() {
            return Err(Error::Invalid("files is empty".into()));
        }
        if files.len() > MAX_WRITE_FILES {
            return Err(Error::Invalid(format!("at most {MAX_WRITE_FILES} files per write")));
        }
        let message = w.message.as_deref().map(str::trim).filter(|m| !m.is_empty()).ok_or_else(|| Error::Invalid("message is required".into()))?;
        let base = match w.base.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
            Some(b) => match self.store.resolve(&r.id, b)? {
                Some(cs) => Some(cs),
                // An empty repo: writing on the default branch creates its first changeset.
                None if b == r.default_branch && self.head(r)?.is_none() => None,
                None => return Err(Error::NotFound(format!("revision {b}"))),
            },
            None => self.head(r)?,
        };
        let mut edits = Vec::with_capacity(files.len());
        for (path, f) in files {
            let path = RepoPath::new(path.as_str()).map_err(|e| Error::Invalid(e.to_string()))?;
            let edit = if f.delete {
                Edit::Delete
            } else {
                let content = match (&f.content, &f.content_b64) {
                    (Some(c), None) => Bytes::from(c.clone()),
                    (None, Some(b)) => Bytes::from(
                        base64::engine::general_purpose::STANDARD
                            .decode(b.trim())
                            .map_err(|_| Error::Invalid(format!("{path}: content_b64 isn't base64")))?,
                    ),
                    _ => return Err(Error::Invalid(format!("{path}: give one of content, content_b64 or delete"))),
                };
                if content.len() > MAX_WRITE_FILE {
                    return Err(Error::Invalid(format!("{path}: larger than {} MiB", MAX_WRITE_FILE >> 20)));
                }
                let mode = match f.mode.as_deref() {
                    None | Some("100644") => Mode::Regular,
                    Some("100755") => Mode::Executable,
                    Some("120000") => Mode::Symlink,
                    Some(m) => return Err(Error::Invalid(format!("{path}: mode must be 100644, 100755 or 120000, not {m}"))),
                };
                Edit::Write { content, mode }
            };
            edits.push((path, edit));
        }
        let author = self.signature(caller, w.author.as_ref());
        let id = self
            .store
            .create_changeset(
                &r.id,
                NewChangeset { base, other_parents: Vec::new(), author, committer: None, message: message.to_string(), edits },
            )
            .await?;
        Ok(id)
    }

    /// Land `cs` on `ref_name` (a branch; default: the repo's default branch).
    pub async fn land(&self, r: &RepoRow, ref_name: Option<&str>, cs: ChangesetId, change: Option<&str>) -> Result<LandOutcome> {
        let branch = match ref_name.map(str::trim).filter(|s| !s.is_empty()) {
            Some(b) => names::branch(b).map_err(Error::Invalid)?,
            None => r.default_branch.clone(),
        };
        if let Some(c) = change {
            self.landing_changes.put((r.id.clone(), cs), c.to_string());
        }
        match self.store.land(&r.id, &branch, cs).await {
            Ok(l) => {
                self.db.touch_repo(&r.id)?;
                Ok(LandOutcome::Landed(l))
            }
            Err(LandError::Conflict(c)) => Ok(LandOutcome::Conflict(c)),
            Err(LandError::Other(e)) => Err(e.into()),
        }
    }

    /// `POST /land`: write (or name) a changeset and land it; a conflict is [`Error::LandConflict`].
    /// On a queue repo, landing on the default branch queues it instead (`status: "queued"`),
    /// unless an admin asks to `bypass` the queue.
    pub async fn land_api(&self, r: &RepoRow, caller: &Caller, w: &WriteFiles, ref_name: Option<&str>, bypass: bool) -> Result<Value> {
        let cs = self.write(r, caller, w).await?;
        let branch = ref_name.map(str::to_string).unwrap_or_else(|| r.default_branch.clone());
        if r.land_policy == LandPolicy::Queue && branch == r.default_branch {
            if !bypass {
                return self.land_via_queue(r, caller, cs, Lane::Normal, None).await;
            }
            self.audit_bypass(r, caller, None, &cs, &branch)?;
        } else if branch == r.default_branch {
            // A repo whose rules require reviews takes changes, not direct landings.
            let blockers = self.direct_land_blockers(r, &cs).await?;
            if !blockers.is_empty() {
                if !bypass {
                    let (_, blockers) = self.needs_review_change(r, caller, &cs, blockers).await?;
                    return Err(Error::Blocked(blockers));
                }
                self.audit_review_bypass(r, caller, None, &cs)?;
            }
        }
        match self.land(r, Some(&branch), cs, None).await? {
            LandOutcome::Landed(l) => Ok(landed_json(&l)),
            LandOutcome::Conflict(c) => Err(Error::LandConflict(conflict_json(&c, &branch))),
        }
    }

    // ── changes ──

    /// A change as the API shows it: its repo's name, its place in the queue and what blocks
    /// it (from the repo's settings as last read).
    pub fn change_view(&self, mut c: Change) -> Result<Change> {
        let Some(r) = self.db.repo_by_id(&c.repo_id)? else { return Ok(c) };
        c.repo = r.full_name();
        c.queue = match self.db.entry_for_change(&c.id)? {
            Some(e) => Some(self.queue_info(&e)?),
            None => None,
        };
        c.blockers = self.blockers(&r, &c, &self.settings_cached(&r.id));
        c.review = self.review_summary(&r, &c);
        Ok(c)
    }

    pub fn change_or_404(&self, id: &str) -> Result<Change> {
        let c = self.db.change(id)?.ok_or_else(|| Error::NotFound(format!("change {id}")))?;
        self.change_view(c)
    }

    /// A change and its repo, if the caller can read the repo.
    pub fn readable_change(&self, caller: &Caller, id: &str) -> Result<(RepoRow, Change)> {
        let c = self.change_or_404(id)?;
        let r = self.repo_by_id(&c.repo_id)?;
        if !Self::can_read(caller, &r) {
            return Err(Error::NotFound(format!("change {id}")));
        }
        Ok((r, c))
    }

    pub fn writable_change(&self, caller: &Caller, id: &str) -> Result<(RepoRow, Change)> {
        let (r, c) = self.readable_change(caller, id)?;
        self.check_write(caller, &r)?;
        Ok((r, c))
    }

    pub(crate) fn emit_change(&self, c: &Change) {
        self.emit(&c.repo_id, "change", serde_json::to_value(c).unwrap_or_default());
    }

    fn emit_work(&self, w: &WorkItem) {
        self.emit(&w.repo_id, "work", serde_json::to_value(w).unwrap_or_default());
    }

    /// Where a change's head left the default branch: its merge base, so a branch with several
    /// commits (or a revision stacked on the last one, like an applied suggestion) keeps the base
    /// it diverged from instead of its previous head. With no common history (or a head already
    /// on the default branch), the head's first parent.
    fn base_of(&self, r: &RepoRow, cs: &ChangesetId) -> Result<String> {
        let m = self.store.meta(&r.id, cs)?.ok_or_else(|| Error::NotFound(format!("changeset {cs:?}")))?;
        let parent = m.cs.parents.first().map(cs_id).unwrap_or_default();
        Ok(match self.merge_base(r, cs)? {
            Some(mb) if mb != *cs => cs_id(&mb),
            _ => parent,
        })
    }

    pub async fn create_change(&self, r: &RepoRow, caller: &Caller, input: &Value, branch: Option<&str>) -> Result<Change> {
        self.create_change_as(r, caller, input, branch, true).await
    }

    /// [`create_change`](Self::create_change), for a change that may come from someone without
    /// write access (`trusted: false`, a pull request from a fork).
    pub async fn create_change_as(&self, r: &RepoRow, caller: &Caller, input: &Value, branch: Option<&str>, trusted: bool) -> Result<Change> {
        self.create_change_with(r, caller, input, branch, trusted, true).await
    }

    /// [`create_change_as`](Self::create_change_as); with `start_work` false, a linked work item
    /// keeps its state instead of moving to `review` (roll's suggested reverts: the item stays open
    /// for an agent, and never flickers through `review` on the way).
    pub(crate) async fn create_change_with(
        &self,
        r: &RepoRow,
        caller: &Caller,
        input: &Value,
        branch: Option<&str>,
        trusted: bool,
        start_work: bool,
    ) -> Result<Change> {
        let w: WriteFiles = serde_json::from_value(input.clone()).map_err(|e| Error::Invalid(e.to_string()))?;
        let s = |k: &str| input.get(k).and_then(Value::as_str).map(str::trim);
        let title = s("title").filter(|t| !t.is_empty()).map(str::to_string).or_else(|| w.message.as_deref().map(|m| m.lines().next().unwrap_or("").trim().to_string())).filter(|t| !t.is_empty()).ok_or_else(|| Error::Invalid("title is required".into()))?;
        let work = match s("work_item").filter(|w| !w.is_empty()) {
            Some(id) => {
                let wi = self.db.work(id)?.filter(|wi| wi.repo_id == r.id).ok_or_else(|| Error::NotFound(format!("work item {id}")))?;
                Some(wi)
            }
            None => None,
        };
        let cs = self.write(r, caller, &w).await?;
        let author = s("author")
            .or_else(|| input.get("author").and_then(|a| a.get("name")).and_then(Value::as_str).map(str::trim))
            .filter(|a| !a.is_empty())
            .map(str::to_string)
            // A change is by whoever wrote its head changeset, not by the token that sent it: a
            // pushed branch has its commit's author, an API write the author it gave (or "roll").
            .or_else(|| self.store.info(&r.id, &cs).ok().flatten().map(|i| i.author.name()).filter(|n| !n.is_empty()))
            .or_else(|| caller.identity())
            .unwrap_or_else(|| "anonymous".into());
        let id = new_id("chg");
        let base = self.base_of(r, &cs)?;
        let draft = match input.get("draft") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(Error::Invalid("draft must be true or false".into())),
        };
        self.db.insert_change(
            &id,
            &r.id,
            &NewChange {
                title: &title.chars().take(300).collect::<String>(),
                body: s("body").unwrap_or(""),
                author: &author,
                base: &base,
                head: &cs_id(&cs),
                work_item: work.as_ref().map(|w| w.id.as_str()),
                branch,
                trusted,
                draft,
            },
        )?;
        if let Some(mut wi) = work
            && start_work
            && matches!(wi.state, WorkState::Open | WorkState::Claimed)
        {
            wi.state = WorkState::Review;
            wi.lease_until = None;
            self.db.update_work(&wi)?;
            if let Some(wi) = self.db.work(&wi.id)? {
                self.emit_work(&self.work_view(wi)?);
            }
        }
        let c = self.change_or_404(&id)?;
        self.emit_change(&c);
        self.review_change_event(r, &c, "change.opened");
        Ok(c)
    }

    pub async fn revise_change(&self, r: &RepoRow, caller: &Caller, c: &Change, input: &Value) -> Result<Change> {
        if matches!(c.state, ChangeState::Landed | ChangeState::Closed) {
            return Err(Error::Conflict(format!("change {} is {}", c.number, c.state.as_str())));
        }
        let w: WriteFiles = serde_json::from_value(input.clone()).map_err(|e| Error::Invalid(e.to_string()))?;
        let cs = self.write(r, caller, &w).await?;
        self.db.revise_change(&c.id, &self.base_of(r, &cs)?, &cs_id(&cs))?;
        // A queued change stays queued, but whatever batch it was in tested the old head.
        self.queued_change_moved(r, c, true)?;
        let c = self.change_or_404(&c.id)?;
        // Review threads follow their lines to the new revision (or become outdated).
        if let Err(e) = self.carry_threads(r, &c).await {
            tracing::warn!(change = %c.id, error = %e, "carrying review threads to a new revision");
        }
        let c = self.change_or_404(&c.id)?;
        self.emit_change(&c);
        self.review_change_event(r, &c, "change.revised");
        Ok(c)
    }

    /// `PATCH /changes/{id}`: title, body, draft. Marking a queued change draft dequeues it.
    pub fn update_change(&self, r: &RepoRow, c: &Change, patch: &Value) -> Result<Change> {
        let title = match patch.get("title") {
            None | Some(Value::Null) => None,
            Some(Value::String(t)) if !t.trim().is_empty() => Some(t.trim().chars().take(300).collect::<String>()),
            Some(_) => return Err(Error::Invalid("title must be a non-empty string".into())),
        };
        let body = match patch.get("body") {
            None | Some(Value::Null) => None,
            Some(Value::String(b)) => Some(b.clone()),
            Some(_) => return Err(Error::Invalid("body must be a string".into())),
        };
        let draft = match patch.get("draft") {
            None | Some(Value::Null) => None,
            Some(Value::Bool(d)) => Some(*d),
            Some(_) => return Err(Error::Invalid("draft must be true or false".into())),
        };
        self.db.update_change_meta(&c.id, title.as_deref(), body.as_deref(), draft)?;
        if draft == Some(true) && c.state == ChangeState::Queued {
            self.queued_change_moved(r, c, false)?;
            self.db.set_change_state(&c.id, ChangeState::Open, None, None)?;
        }
        let ready = draft == Some(false) && c.draft;
        let c = self.change_or_404(&c.id)?;
        self.emit_change(&c);
        if ready {
            // Marked ready: roll reviews it now if it skips drafts.
            self.ai_on_change(r, &c);
        }
        Ok(c)
    }

    /// A writer vouches for an untrusted change's current head: its checks and preview run, and
    /// it can be queued. A new revision needs approving again.
    pub fn approve_run(&self, r: &RepoRow, caller: &Caller, c: &Change) -> Result<Change> {
        // Letting a fork's code run with the repo's data is the owner's call, not an agent's.
        self.check_manage(caller, r)?;
        self.db.approve_run(&c.id, &c.head)?;
        self.db.audit(&r.id, &caller.identity().unwrap_or_default(), "approve_run", &json!({ "change": c.id, "head": c.head }))?;
        let c = self.change_or_404(&c.id)?;
        self.emit_change(&c);
        Ok(c)
    }

    pub fn close_change(&self, c: &Change) -> Result<Change> {
        if c.state == ChangeState::Landed {
            return Err(Error::Conflict(format!("change {} already landed", c.number)));
        }
        if let Some(r) = self.db.repo_by_id(&c.repo_id)? {
            self.queued_change_moved(&r, c, false)?;
        }
        self.db.set_change_state(&c.id, ChangeState::Closed, None, None)?;
        let c = self.change_or_404(&c.id)?;
        self.emit_change(&c);
        Ok(c)
    }

    /// Land a change on the default branch. Returns the land body; a conflict is
    /// [`Error::LandConflict`] (and the change records it). On a queue repo the change is
    /// queued instead (`status: "queued"`), unless an admin asks to `bypass` the queue.
    pub async fn land_change(&self, r: &RepoRow, caller: &Caller, c: &Change, bypass: bool, lane: Lane) -> Result<Value> {
        match c.state {
            ChangeState::Landed => return Err(Error::Conflict(format!("change {} already landed", c.number))),
            ChangeState::Closed => return Err(Error::Conflict(format!("change {} is closed", c.number))),
            _ => {}
        }
        if r.land_policy == LandPolicy::Queue && !bypass {
            return self.enqueue(r, c, lane, caller).await;
        }
        let head = self.store.resolve_or_404(&r.id, &c.head)?;
        if r.land_policy == LandPolicy::Queue {
            self.audit_bypass(r, caller, Some(&c.id), &head, &r.default_branch)?;
            // It may have been waiting in the queue.
            self.db.remove_entry(&c.id)?;
        } else {
            // A writer landing it directly vouches for it (untrusted) and doesn't wait for checks.
            // Review rules still apply, unless an admin bypasses them (audited).
            let settings = self.settings(r).await;
            self.prepare_review(r, c).await;
            let blockers: Vec<Blocker> =
                self.blockers(r, c, &settings).into_iter().filter(|b| b.kind != "untrusted" && b.kind != "checks").collect();
            let (review, other): (Vec<Blocker>, Vec<Blocker>) = blockers.into_iter().partition(|b| crate::review::REVIEW_BLOCKERS.contains(&b.kind.as_str()));
            if !other.is_empty() || (!review.is_empty() && !bypass) {
                return Err(Error::Blocked(other.into_iter().chain(review).collect()));
            }
            if !review.is_empty() {
                self.audit_review_bypass(r, caller, Some(&c.id), &head)?;
            }
        }
        self.db.set_change_state(&c.id, ChangeState::Queued, None, None)?;
        self.emit_change(&self.change_or_404(&c.id)?);
        match self.land(r, None, head, Some(&c.id)).await {
            Ok(LandOutcome::Landed(l)) => {
                self.mark_landed(c, &l)?;
                Ok(landed_json(&l))
            }
            Ok(LandOutcome::Conflict(k)) => {
                let info = ConflictInfo {
                    paths: k.paths.iter().map(|p| p.path.as_str().to_string()).collect(),
                    head: k.head.as_ref().map(cs_id).unwrap_or_default(),
                };
                self.db.set_change_state(&c.id, ChangeState::Conflict, None, Some(&info))?;
                self.emit_change(&self.change_or_404(&c.id)?);
                Err(Error::LandConflict(conflict_json(&k, &r.default_branch)))
            }
            Err(e) => {
                self.db.set_change_state(&c.id, ChangeState::Open, None, None)?;
                Err(e)
            }
        }
    }

    pub(crate) fn mark_landed(&self, c: &Change, l: &Landed) -> Result<()> {
        self.mark_landed_as(c, &cs_id(&l.changeset))
    }

    /// The change landed as changeset `landed_as`: its work item is done.
    pub(crate) fn mark_landed_as(&self, c: &Change, landed_as: &str) -> Result<()> {
        self.db.set_change_state(&c.id, ChangeState::Landed, Some(landed_as), None)?;
        let c = self.change_or_404(&c.id)?;
        self.emit_change(&c);
        if let Some(wid) = &c.work_item
            && let Some(mut wi) = self.db.work(wid)?
            && !matches!(wi.state, WorkState::Done | WorkState::Closed)
        {
            wi.state = WorkState::Done;
            wi.lease_until = None;
            self.db.update_work(&wi)?;
            if let Some(wi) = self.db.work(wid)? {
                self.emit_work(&self.work_view(wi)?);
            }
        }
        Ok(())
    }

    pub fn report_check(&self, c: &Change, input: &Value) -> Result<Change> {
        let s = |k: &str| input.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
        let name = s("name").ok_or_else(|| Error::Invalid("name is required".into()))?;
        if name.len() > 100 {
            return Err(Error::Invalid("check names are at most 100 characters".into()));
        }
        let status = s("status")
            .and_then(CheckStatus::parse)
            .ok_or_else(|| Error::Invalid("status is one of queued, running, success, failure, cancelled, skipped".into()))?;
        let t = now();
        let check = Check {
            name: name.into(),
            status,
            source: s("source").filter(|v| ["runner", "external", "github"].contains(v)).unwrap_or("external").into(),
            url: s("url").map(Into::into),
            summary: s("summary").map(|v| v.chars().take(4000).collect()),
            started_at: (status != CheckStatus::Queued).then_some(t),
            finished_at: status.finished().then_some(t),
        };
        // `head`: the changeset the check ran on. Without it the check counts for whatever the
        // change is now, which is fine for a status that doesn't outlive a revision.
        let head = s("head").map(|h| self.store.resolve(&c.repo_id, h).ok().flatten().map(|cs| cs_id(&cs)).unwrap_or_else(|| h.to_string()));
        if !self.db.upsert_check(&c.id, head.as_deref(), &check)? {
            return Err(Error::Conflict(format!("change {} moved past {}; report the check for its current head", c.number, head.unwrap_or_default())));
        }
        let check = self.db.checks(&c.id)?.into_iter().find(|k| k.name == name).unwrap_or(check);
        self.emit(&c.repo_id, "check", json!({ "change": c.id, "check": check }));
        let c = self.change_or_404(&c.id)?;
        self.emit_change(&c);
        Ok(c)
    }

    pub async fn create_preview(&self, r: &RepoRow, c: &Change) -> Result<Change> {
        let Some(backend) = self.previews.read().clone() else {
            return Err(Error::Unprocessable("previews aren't available on this server".into()));
        };
        let head = self.store.resolve_or_404(&r.id, &c.head)?;
        // A pooled work tree, leased only while the backend reads it.
        let tree = self.checkout(&r.id, &head).await?;
        let preview = backend.create(r, c, tree.path()).await?;
        drop(tree);
        self.db.set_change_preview(&c.id, Some(&preview))?;
        self.emit(&r.id, "preview", json!({ "change": c.id, "preview": preview }));
        let c = self.change_or_404(&c.id)?;
        self.emit_change(&c);
        Ok(c)
    }

    pub async fn delete_preview(&self, r: &RepoRow, c: &Change) -> Result<Change> {
        let backend = self.previews.read().clone();
        if c.preview.is_some()
            && let Some(backend) = backend
        {
            backend.delete(r, c).await?;
        }
        self.db.set_change_preview(&c.id, None)?;
        self.emit(&r.id, "preview", json!({ "change": c.id, "preview": null }));
        let c = self.change_or_404(&c.id)?;
        self.emit_change(&c);
        Ok(c)
    }

    // ── work items ──

    pub fn work_view(&self, mut w: WorkItem) -> Result<WorkItem> {
        w.repo = self.db.repo_by_id(&w.repo_id)?.map(|r| r.full_name()).unwrap_or_default();
        Ok(w)
    }

    pub fn work_or_404(&self, id: &str) -> Result<WorkItem> {
        let w = self.db.work(id)?.ok_or_else(|| Error::NotFound(format!("work item {id}")))?;
        self.work_view(w)
    }

    pub fn readable_work(&self, caller: &Caller, id: &str) -> Result<(RepoRow, WorkItem)> {
        let w = self.work_or_404(id)?;
        let r = self.repo_by_id(&w.repo_id)?;
        if !Self::can_read(caller, &r) {
            return Err(Error::NotFound(format!("work item {id}")));
        }
        Ok((r, w))
    }

    pub fn writable_work(&self, caller: &Caller, id: &str) -> Result<(RepoRow, WorkItem)> {
        let (r, w) = self.readable_work(caller, id)?;
        self.check_write(caller, &r)?;
        Ok((r, w))
    }

    fn labels(v: Option<&Value>) -> Result<Vec<String>> {
        let Some(v) = v.filter(|v| !v.is_null()) else { return Ok(Vec::new()) };
        let list: Vec<String> = serde_json::from_value(v.clone()).map_err(|_| Error::Invalid("labels must be a list of strings".into()))?;
        let mut out: Vec<String> = Vec::new();
        for l in list {
            let l: String = l.trim().chars().take(50).collect();
            if !l.is_empty() && !out.iter().any(|o| o.eq_ignore_ascii_case(&l)) {
                out.push(l);
            }
        }
        Ok(out)
    }

    fn priority(v: Option<&Value>) -> Result<Option<i64>> {
        match v {
            None | Some(Value::Null) => Ok(None),
            Some(p) => match p.as_i64() {
                Some(p @ 0..=3) => Ok(Some(p)),
                _ => Err(Error::Invalid("priority is 0 (P0, most urgent) to 3".into())),
            },
        }
    }

    pub fn create_work(&self, r: &RepoRow, input: &Value) -> Result<WorkItem> {
        let title = input.get("title").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).ok_or_else(|| Error::Invalid("title is required".into()))?;
        let labels = Self::labels(input.get("labels"))?;
        let id = new_id("wi");
        self.db.insert_work(
            &id,
            &r.id,
            &NewWork {
                title: &title.chars().take(300).collect::<String>(),
                body: input.get("body").and_then(Value::as_str).unwrap_or(""),
                labels: &labels,
                priority: Self::priority(input.get("priority"))?.unwrap_or(2),
                source: "native",
                github_url: None,
                state: WorkState::Open,
            },
        )?;
        let w = self.work_or_404(&id)?;
        self.emit_work(&w);
        Ok(w)
    }

    pub fn claim(&self, r: &RepoRow, input: &Value) -> Result<Option<WorkItem>> {
        let agent = input.get("agent").and_then(Value::as_str).map(str::trim).filter(|a| !a.is_empty()).ok_or_else(|| Error::Invalid("agent is required".into()))?;
        let labels = Self::labels(input.get("labels"))?;
        let lease = input.get("lease_s").and_then(Value::as_i64).unwrap_or(DEFAULT_LEASE_S).clamp(1, 86_400);
        let Some(id) = self.db.claim(&r.id, &agent.chars().take(100).collect::<String>(), &labels, lease)? else { return Ok(None) };
        let w = self.work_or_404(&id)?;
        self.emit_work(&w);
        Ok(Some(w))
    }

    fn check_agent(w: &WorkItem, input: &Value) -> Result<()> {
        if let Some(agent) = input.get("agent").and_then(Value::as_str).filter(|a| !a.is_empty())
            && w.claimed_by.as_deref().is_some_and(|c| c != agent)
        {
            return Err(Error::Conflict(format!("work item {} is claimed by {}", w.number, w.claimed_by.as_deref().unwrap_or(""))));
        }
        Ok(())
    }

    pub fn heartbeat(&self, w: &WorkItem, input: &Value) -> Result<WorkItem> {
        if w.state != WorkState::Claimed {
            return Err(Error::Conflict(format!("work item {} isn't claimed (it's {})", w.number, w.state.as_str())));
        }
        Self::check_agent(w, input)?;
        // An explicit lease_s becomes the lease's length; otherwise renew by the claimed length.
        let explicit = input.get("lease_s").and_then(Value::as_i64).map(|s| s.clamp(1, 86_400));
        let lease = match explicit {
            Some(s) => s,
            None => self.db.lease_len(&w.id)?.unwrap_or(DEFAULT_LEASE_S),
        };
        let mut w = w.clone();
        w.lease_until = Some(now() + lease);
        self.db.update_work(&w)?;
        if let Some(s) = explicit {
            self.db.set_lease_len(&w.id, s)?;
        }
        let w = self.work_or_404(&w.id)?;
        self.emit_work(&w);
        Ok(w)
    }

    pub fn release(&self, w: &WorkItem, input: &Value) -> Result<WorkItem> {
        Self::check_agent(w, input)?;
        let mut w = w.clone();
        if matches!(w.state, WorkState::Claimed | WorkState::Review) {
            w.state = WorkState::Open;
        }
        w.claimed_by = None;
        w.lease_until = None;
        self.db.update_work(&w)?;
        let w = self.work_or_404(&w.id)?;
        self.emit_work(&w);
        Ok(w)
    }

    pub fn complete(&self, w: &WorkItem, input: &Value) -> Result<WorkItem> {
        Self::check_agent(w, input)?;
        let mut w = w.clone();
        w.state = WorkState::Done;
        w.lease_until = None;
        self.db.update_work(&w)?;
        let w = self.work_or_404(&w.id)?;
        self.emit_work(&w);
        Ok(w)
    }

    pub fn update_work(&self, w: &WorkItem, patch: &Value) -> Result<WorkItem> {
        let mut w = w.clone();
        if let Some(t) = patch.get("title").and_then(Value::as_str).map(str::trim) {
            if t.is_empty() {
                return Err(Error::Invalid("title can't be empty".into()));
            }
            w.title = t.chars().take(300).collect();
        }
        if let Some(b) = patch.get("body").and_then(Value::as_str) {
            w.body = b.into();
        }
        if patch.get("labels").is_some() {
            w.labels = Self::labels(patch.get("labels"))?;
        }
        if let Some(p) = Self::priority(patch.get("priority"))? {
            w.priority = p;
        }
        if let Some(s) = patch.get("state").and_then(Value::as_str) {
            w.state = WorkState::parse(s).ok_or_else(|| Error::Invalid("state is one of open, claimed, review, done, closed".into()))?;
            if w.state == WorkState::Claimed && w.claimed_by.is_none() {
                return Err(Error::Invalid("claim work with POST …/work/claim".into()));
            }
            if w.state != WorkState::Claimed {
                w.lease_until = None;
            }
            if w.state == WorkState::Open {
                w.claimed_by = None;
            }
        }
        self.db.update_work(&w)?;
        let w = self.work_or_404(&w.id)?;
        self.emit_work(&w);
        Ok(w)
    }
}
