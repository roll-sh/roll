//! code-git's [`GitBackend`] over code-store: `git clone/fetch/push` against roll code.
//!
//! A push to the default branch is a land request: it fast-forwards when it can, otherwise the
//! lander rebases the pushed commits onto the head (merging files server-side) or rejects the
//! push with the conflicting paths. git itself refuses to push a branch that isn't a
//! fast-forward of what it last fetched, so agents land with `git push origin HEAD:refs/land/main`
//! instead: a ref that's never stored, so there's nothing to fetch first. A push to any other
//! branch updates that branch and opens or revises its change.

use crate::model::{Lane, LandPolicy, RepoRow};
use crate::service::{LandOutcome, short_ref};
use crate::{Caller, Code, Error, cs_id};
use async_trait::async_trait;
use code_git::{
    Access, Credentials, FetchPlan, FetchRequest, GitBackend, GitError, GitResult, Object, ObjectId, ReceiveOutcome,
    ReceiveRequest, Ref, RefList, RefStatus, RefUpdate,
};
use code_store::{ChangesetId, Expect, Kind, Oid};
use futures::StreamExt;
use serde_json::json;
use std::sync::Arc;

pub struct GitAdapter {
    code: Arc<Code>,
}

impl GitAdapter {
    pub fn new(code: Arc<Code>) -> Arc<GitAdapter> {
        Arc::new(GitAdapter { code })
    }

    /// `owner/name` (renames followed) to the repo.
    fn repo(&self, path: &str) -> GitResult<RepoRow> {
        let (owner, name) = path.split_once('/').ok_or_else(|| GitError::NotFound(path.into()))?;
        match self.code.find_repo(owner, name) {
            Ok(r) => Ok(r),
            Err(Error::Moved(to)) => {
                let (o, n) = to.split_once('/').unwrap_or((&to, ""));
                self.code.find_repo(o, n).map_err(|_| GitError::NotFound(path.into()))
            }
            Err(Error::NotFound(_)) => Err(GitError::NotFound(path.into())),
            Err(e) => Err(internal(e)),
        }
    }
}

fn internal(e: impl std::fmt::Display) -> GitError {
    GitError::Internal(anyhow::anyhow!("{e}"))
}

fn to_store(o: &ObjectId) -> Oid {
    Oid(o.0)
}

fn to_git(o: &Oid) -> ObjectId {
    ObjectId(o.0)
}

fn kind_to_store(k: code_git::Kind) -> Kind {
    match k {
        code_git::Kind::Commit => Kind::Commit,
        code_git::Kind::Tree => Kind::Tree,
        code_git::Kind::Blob => Kind::Blob,
        code_git::Kind::Tag => Kind::Tag,
    }
}

fn kind_to_git(k: Kind) -> code_git::Kind {
    match k {
        Kind::Commit => code_git::Kind::Commit,
        Kind::Tree => code_git::Kind::Tree,
        Kind::Blob => code_git::Kind::Blob,
        Kind::Tag => code_git::Kind::Tag,
    }
}

fn ok(name: &str) -> RefStatus {
    RefStatus { name: name.into(), result: Ok(()) }
}

fn rejected(name: &str, reason: impl Into<String>) -> RefStatus {
    RefStatus { name: name.into(), result: Err(reason.into()) }
}

#[async_trait]
impl GitBackend for GitAdapter {
    async fn authorize(&self, repo: &str, credentials: Option<&Credentials>, access: Access) -> GitResult<Option<String>> {
        let caller = self.code.caller(credentials.map(|c| c.password.as_str())).map_err(|_| GitError::Unauthorized)?;
        let r = self.repo(repo)?;
        if !Code::can_read(&caller, &r) {
            return Err(if caller == Caller::Anonymous { GitError::Unauthorized } else { GitError::NotFound(repo.into()) });
        }
        if access == Access::Write {
            match self.code.check_write(&caller, &r) {
                Ok(()) => {}
                Err(Error::Unauthorized) => return Err(GitError::Unauthorized),
                Err(e) => return Err(GitError::Forbidden(e.to_string())),
            }
        }
        // The pusher is the token's owner, or the username given with an admin token.
        Ok(match (&caller, credentials) {
            (Caller::Owner { name, .. }, _) => Some(name.clone()),
            (_, Some(c)) if !c.username.is_empty() && c.username != "x-access-token" => Some(c.username.clone()),
            _ => caller.identity(),
        })
    }

    async fn refs(&self, repo: &str) -> GitResult<RefList> {
        let r = self.repo(repo)?;
        let refs = self
            .code
            .store
            .refs(&r.id)
            .map_err(internal)?
            .into_iter()
            .map(|i| match i.tag {
                Some(tag) => Ref { name: i.name, oid: to_git(&tag), peeled: Some(to_git(&i.commit)) },
                None => Ref { name: i.name, oid: to_git(&i.commit), peeled: None },
            })
            .collect();
        Ok(RefList { head: Some(code_store::full_ref(&r.default_branch)), refs })
    }

    async fn read_object(&self, repo: &str, oid: &ObjectId) -> GitResult<Option<Object>> {
        let r = self.repo(repo)?;
        let obj = self.code.store.git_object(&r.id, &to_store(oid)).await.map_err(internal)?;
        Ok(obj.map(|(kind, data)| Object { id: *oid, kind: kind_to_git(kind), data }))
    }

    async fn read_objects(&self, repo: &str, oids: &[ObjectId]) -> GitResult<Vec<Option<Object>>> {
        let r = self.repo(repo)?;
        let store = &self.code.store;
        let id = r.id.as_str();
        let reads = futures::stream::iter(oids.iter().copied().map(|oid| async move {
            let obj = store.git_object(id, &to_store(&oid)).await?;
            anyhow::Ok(obj.map(|(kind, data)| Object { id: oid, kind: kind_to_git(kind), data }))
        }))
        .buffered(32)
        .collect::<Vec<_>>()
        .await;
        reads.into_iter().map(|r| r.map_err(internal)).collect()
    }

    async fn has_object(&self, repo: &str, oid: &ObjectId) -> GitResult<bool> {
        let r = self.repo(repo)?;
        self.code.store.has_git_object(&r.id, &to_store(oid)).map_err(internal)
    }

    async fn plan_fetch(&self, repo: &str, req: &FetchRequest) -> GitResult<FetchPlan> {
        // Shallow and partial fetches take the generic walk; full and incremental fetches are
        // answered from changesets (generations + per-changeset file lists).
        if req.depth.is_some() || req.filter.is_some() || !req.client_shallow.is_empty() || req.deepen_relative {
            return code_git::walk::plan_fetch(self, repo, req).await;
        }
        let r = self.repo(repo)?;
        let wants: Vec<Oid> = req.wants.iter().map(to_store).collect();
        let haves: Vec<Oid> = req.haves.iter().map(to_store).collect();
        match self.code.store.objects_for_fetch(&r.id, &wants, &haves) {
            Ok(objects) => Ok(FetchPlan { objects: objects.iter().map(|(o, _)| to_git(o)).collect(), ..FetchPlan::default() }),
            // Wants that aren't commits or tags (a partial clone fetching blobs lazily).
            Err(e) if e.downcast_ref::<code_store::NotFound>().is_some() => code_git::walk::plan_fetch(self, repo, req).await,
            Err(e) => Err(internal(e)),
        }
    }

    async fn receive(&self, repo: &str, req: ReceiveRequest) -> GitResult<ReceiveOutcome> {
        let r = self.repo(repo)?;
        let mut out = ReceiveOutcome::default();
        if !req.objects.is_empty() {
            let objects = req.objects.into_iter().map(|o| (kind_to_store(o.kind), o.data)).collect();
            if let Err(e) = self.code.store.import_git(&r.id, objects).await {
                let reason = format!("unpack failed: {e}");
                out.refs = req.updates.iter().map(|u| rejected(&u.name, reason.clone())).collect();
                return Ok(out);
            }
        }
        if req.atomic && req.updates.len() > 1 {
            // Every update is checked before any is applied; a land can still conflict, so an
            // atomic push may land the default branch alone.
            for u in &req.updates {
                if let Err(reason) = self.precheck(&r, u) {
                    out.refs = req.updates.iter().map(|x| rejected(&x.name, if x.name == u.name { reason.clone() } else { "atomic push failed".into() })).collect();
                    return Ok(out);
                }
            }
        }
        for u in &req.updates {
            let result = self.apply(&r, u, req.pusher.as_deref(), &mut out.messages).await;
            self.code.emit(
                &r.id,
                "push",
                json!({
                    "ref": u.name,
                    "before": (!u.old.is_zero()).then(|| u.old.to_hex()),
                    "after": (!u.new.is_zero()).then(|| u.new.to_hex()),
                    "pusher": req.pusher,
                    "accepted": result.is_ok(),
                    "reason": result.as_ref().err(),
                }),
            );
            let status = match result {
                Ok(()) => ok(&u.name),
                Err(reason) => rejected(&u.name, reason),
            };
            out.refs.push(status);
        }
        Ok(out)
    }
}

impl GitAdapter {
    /// A push to a queue repo's default branch: it becomes a change in the merge queue. The ref
    /// isn't updated (the queue lands it once it's tested), so git reports the push as rejected,
    /// with where it went.
    async fn enqueue_push(&self, r: &RepoRow, cs: ChangesetId, pusher: Option<&str>, messages: &mut Vec<String>) -> Result<(), String> {
        let queued = self.code.land_via_queue(r, &Caller::Admin, cs, Lane::Normal, pusher).await.map_err(|e| match e {
            Error::Blocked(b) => format!("can't be queued: {}", b.iter().map(|b| b.message.as_str()).collect::<Vec<_>>().join("; ")),
            e => e.to_string(),
        })?;
        let number = queued["change"]["number"].as_u64().unwrap_or(0);
        let position = queued["position"].as_u64().unwrap_or(0);
        messages.push(format!("{} goes through the merge queue: it lands once its checks pass", r.default_branch));
        messages.push(format!("queued as change #{number} (position {position})"));
        messages.push(format!("watch it: roll code queue {}  ·  {}/changes/{number}", r.full_name(), self.code.web_url(r)));
        Err(format!("queued as change #{number} (position {position})"))
    }

    /// A push to the default branch of a repo whose rules require reviews becomes a change
    /// instead (the ref isn't updated), like a queue repo's.
    async fn review_gate(&self, r: &RepoRow, cs: &ChangesetId, messages: &mut Vec<String>) -> Result<(), String> {
        let blockers = self.code.direct_land_blockers(r, cs).await.map_err(|e| e.to_string())?;
        if blockers.is_empty() {
            return Ok(());
        }
        let (c, blockers) = self.code.needs_review_change(r, &Caller::Admin, cs, blockers).await.map_err(|e| e.to_string())?;
        messages.push(format!("{} needs review before anything lands on it", r.default_branch));
        for b in blockers.iter().skip(1) {
            messages.push(format!("  {}", b.message));
        }
        messages.push(format!("opened change #{}: {}/changes/{}", c.number, self.code.web_url(r), c.number));
        Err(format!("review required: opened as change #{}", c.number))
    }

    fn changeset_of(&self, r: &RepoRow, oid: &ObjectId) -> Result<Option<ChangesetId>, String> {
        self.code.store.changeset_of_git(&r.id, &to_store(oid)).map_err(|e| e.to_string())
    }

    fn precheck(&self, r: &RepoRow, u: &RefUpdate) -> Result<(), String> {
        let default = code_store::full_ref(&r.default_branch);
        if u.name == default && u.is_delete() {
            return Err("the default branch can't be deleted".into());
        }
        if let Some(target) = u.name.strip_prefix("refs/land/") {
            if u.is_delete() {
                return Err("refs/land/<branch> lands a push; there's nothing to delete".into());
            }
            crate::names::branch(target)?;
        } else if !u.name.starts_with("refs/heads/") && !u.name.starts_with("refs/tags/") {
            return Err("only branches, tags and refs/land/<branch> can be pushed".into());
        }
        if !u.is_delete() && !u.name.starts_with("refs/tags/") && self.changeset_of(r, &u.new)?.is_none() {
            return Err("missing objects".into());
        }
        Ok(())
    }

    async fn apply(&self, r: &RepoRow, u: &RefUpdate, pusher: Option<&str>, messages: &mut Vec<String>) -> Result<(), String> {
        self.precheck(r, u)?;
        let default = code_store::full_ref(&r.default_branch);
        let store = &self.code.store;
        let target = if u.name == default { Some(r.default_branch.clone()) } else { u.name.strip_prefix("refs/land/").map(str::to_string) };
        if let Some(target) = target {
            let cs = self.changeset_of(r, &u.new)?.ok_or("missing objects")?;
            if r.land_policy == LandPolicy::Queue && target == r.default_branch {
                return self.enqueue_push(r, cs, pusher, messages).await;
            }
            if target == r.default_branch {
                self.review_gate(r, &cs, messages).await?;
            }
            return match self.code.land(r, Some(&target), cs, None).await.map_err(|e| e.to_string())? {
                LandOutcome::Landed(l) => {
                    if l.already {
                        messages.push(format!("{} is already on {target}", &u.new.to_hex()[..12]));
                    } else if !l.fast_forward {
                        messages.push(format!(
                            "landed {} as {} on {target} (rebased onto newer changes; run `git pull --rebase`)",
                            if l.count == 1 { "1 commit".to_string() } else { format!("{} commits", l.count) },
                            &l.git_commit.hex()[..12],
                        ));
                        if !l.merged_paths.is_empty() {
                            messages.push(format!(
                                "merged server-side: {}",
                                l.merged_paths.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(", ")
                            ));
                        }
                    }
                    Ok(())
                }
                LandOutcome::Conflict(c) => {
                    messages.push(format!("conflict landing on {target}: {} changed since your base", if c.paths.len() == 1 { "a file".to_string() } else { format!("{} files", c.paths.len()) }));
                    for p in &c.paths {
                        let kind = serde_json::to_value(p.kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
                        messages.push(format!("  {} ({})", p.path.as_str(), kind.replace('_', "/")));
                    }
                    messages.push(format!("fetch, rebase onto {target}, resolve and push again"));
                    Err(format!("conflict: {}", c.paths.iter().map(|p| p.path.as_str()).collect::<Vec<_>>().join(", ")))
                }
            };
        }
        let expect = if u.is_create() {
            Expect::Absent
        } else {
            match self.changeset_of(r, &u.old)? {
                Some(cs) => Expect::Is(cs),
                None => return Err("stale info: fetch first".into()),
            }
        };
        if u.name.starts_with("refs/tags/") {
            let result = if u.is_delete() {
                store.update_ref(&r.id, &u.name, expect, None).await
            } else if let Some(cs) = self.changeset_of(r, &u.new)? {
                store.update_ref(&r.id, &u.name, expect, Some(cs)).await
            } else {
                store.update_tag(&r.id, &u.name, expect, to_store(&u.new)).await
            };
            return result.map_err(|e| e.to_string());
        }
        // Any other branch: update it, then open or revise its change.
        let branch = short_ref(&u.name);
        if u.is_delete() {
            store.update_ref(&r.id, &u.name, expect, None).await.map_err(|e| e.to_string())?;
            if let Ok(Some(c)) = self.code.db.change_by_branch(&r.id, &branch) {
                let _ = self.code.close_change(&c);
            }
            return Ok(());
        }
        let cs = self.changeset_of(r, &u.new)?.ok_or("missing objects")?;
        store.update_ref(&r.id, &u.name, expect, Some(cs)).await.map_err(|e| e.to_string())?;
        let head = cs_id(&cs);
        let existing = self.code.db.change_by_branch(&r.id, &branch).map_err(|e| e.to_string())?;
        let change = match existing {
            Some(c) if c.head == head => c,
            Some(c) => self
                .code
                .revise_change(r, &Caller::Admin, &c, &json!({ "changeset": head }))
                .await
                .map_err(|e| e.to_string())?,
            None => {
                let info = store.info(&r.id, &cs).map_err(|e| e.to_string())?.ok_or("missing objects")?;
                let input = json!({
                    "title": info.summary,
                    "body": info.message.split_once('\n').map(|(_, b)| b.trim()).unwrap_or(""),
                    "changeset": head,
                });
                self.code.create_change(r, &Caller::Admin, &input, Some(&branch)).await.map_err(|e| e.to_string())?
            }
        };
        messages.push(format!("change #{} ({}): {}/changes/{}", change.number, change.state.as_str(), self.code.web_url(r), change.number));
        Ok(())
    }
}
