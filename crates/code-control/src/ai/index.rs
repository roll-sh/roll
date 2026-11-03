//! Keeping each repo's codebase index at its default branch's head, and searching it.
//!
//! Landings on a default branch mark the repo dirty; one task brings dirty repos up to date,
//! one at a time, from the diff between the indexed head and the current one. A repo that was
//! never indexed is built on first use (a deep review, a search), file by file.

use crate::model::RepoRow;
use crate::{Code, Error, Result, cs_id};
use async_trait::async_trait;
use code_index::{Query, Update};
use code_store::{ChangesetId, FileStatus, Store};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// Files written per index transaction.
const BATCH: usize = 200;
/// Files a first build indexes at most (the rest of a huge repo isn't searchable).
const MAX_BUILD_FILES: usize = 200_000;


struct StoreSource {
    store: Arc<Store>,
    repo: String,
    at: ChangesetId,
}

#[async_trait]
impl code_index::Source for StoreSource {
    async fn read(&self, path: &str) -> Option<Vec<u8>> {
        self.store.read_file(&self.repo, &self.at, path).await.ok().flatten().map(|f| f.data.to_vec())
    }
}

impl Code {
    pub(crate) async fn run_indexer(self: Arc<Self>) {
        let mut landed = self.store.subscribe_all();
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                match landed.recv().await {
                    Ok(ev) => {
                        let Ok(r) = me.repo_by_id(&ev.repo) else { continue };
                        if ev.ref_name == code_store::full_ref(&r.default_branch) {
                            me.ai.index_dirty.lock().insert(ev.repo.clone());
                            me.ai.index_wake.notify_one();
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                }
            }
        });
        loop {
            self.ai.index_wake.notified().await;
            // Coalesce a burst of landings.
            tokio::time::sleep(Duration::from_millis(250)).await;
            let dirty: Vec<String> = std::mem::take(&mut *self.ai.index_dirty.lock()).into_iter().collect();
            for id in dirty {
                // A repo that moved away is indexed by its new owner.
                if !self.owns_repo(&id) {
                    continue;
                }
                let Ok(r) = self.repo_by_id(&id) else { continue };
                // Only repos that were indexed before follow along; others build on first use.
                let built = self.ai.index.as_ref().and_then(|i| i.repo(&r.id).ok()).and_then(|x| x.head().ok().flatten()).is_some();
                if built && let Err(e) = self.index_repo(&r).await {
                    tracing::warn!(repo = %r.full_name(), error = %e, "updating the codebase index");
                }
            }
        }
    }

    /// Bring a repo's index to its default head (building it the first time).
    pub async fn index_repo(&self, r: &RepoRow) -> anyhow::Result<()> {
        let Some(index) = &self.ai.index else { return Ok(()) };
        let idx = index.repo(&r.id)?;
        let Some(head) = self.head(r).map_err(|e| anyhow::anyhow!(e.to_string()))? else { return Ok(()) };
        let lock = self.ai.index_locks.lock().entry(r.id.clone()).or_default().clone();
        let _one = lock.lock().await;
        let target = cs_id(&head);
        let cur = idx.head()?;
        if cur.as_deref() == Some(target.as_str()) {
            return Ok(());
        }
        let from = cur.as_deref().and_then(|c| self.store.resolve(&r.id, c).ok().flatten());
        match from {
            Some(from) => {
                let diffs = self.store.compare(&r.id, &from, &head)?;
                let mut batch = Vec::new();
                for d in diffs {
                    let content = match d.status {
                        FileStatus::Deleted => None,
                        _ => self.store.read_file(&r.id, &head, d.path.as_str()).await?.map(|f| f.data.to_vec()),
                    };
                    batch.push(Update { path: d.path.to_string(), content });
                    if batch.len() >= BATCH {
                        idx.apply(None, &std::mem::take(&mut batch))?;
                    }
                }
                idx.apply(Some(&target), &batch)?;
            }
            None => {
                let _first = self.ai.first_builds.acquire().await?;
                idx.clear()?;
                let (store, repo) = (self.store.clone(), r.id.clone());
                let paths = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<String>> {
                    let mut out = Vec::new();
                    crate::reads::walk(&store, &repo, &head, MAX_BUILD_FILES * 2, |e| {
                        if e.kind == code_store::EntryKind::File && out.len() < MAX_BUILD_FILES && e.size.unwrap_or(0) <= code_index::MAX_FILE_BYTES as u64 {
                            out.push(e.path.clone());
                        }
                    })?;
                    Ok(out)
                })
                .await??;
                let mut batch = Vec::new();
                for p in paths {
                    let content = self.store.read_file(&r.id, &head, &p).await?.map(|f| f.data.to_vec());
                    batch.push(Update { path: p, content });
                    if batch.len() >= BATCH {
                        idx.apply(None, &std::mem::take(&mut batch))?;
                    }
                }
                idx.apply(Some(&target), &batch)?;
            }
        }
        Ok(())
    }

    /// Is the repo's index usable now? A repo indexed before is brought up to date in place
    /// (a diff since its indexed head). A repo never indexed builds in the background; this waits
    /// up to `wait` for it (small repos finish), and says `false` while a big one is still building.
    pub(crate) async fn index_ready(&self, r: &RepoRow, wait: Duration) -> anyhow::Result<bool> {
        let Some(index) = &self.ai.index else { return Ok(false) };
        if index.repo(&r.id)?.head()?.is_some() {
            self.index_repo(r).await?;
            return Ok(true);
        }
        let mut done = {
            let mut building = self.ai.building.lock();
            match building.get(&r.id) {
                Some(rx) => rx.clone(),
                None => {
                    let Some(me) = self.me.upgrade() else { return Ok(false) };
                    let (tx, rx) = tokio::sync::watch::channel(false);
                    building.insert(r.id.clone(), rx.clone());
                    let r2 = r.clone();
                    tokio::spawn(async move {
                        if let Err(e) = me.index_repo(&r2).await {
                            tracing::warn!(repo = %r2.full_name(), error = %e, "building the codebase index");
                        }
                        me.ai.building.lock().remove(&r2.id);
                        let _ = tx.send(true);
                    });
                    rx
                }
            }
        };
        // A zero wait doesn't wait at all (a timer of zero still waits for the timer driver).
        let finished = if wait.is_zero() { *done.borrow() } else { tokio::time::timeout(wait, done.wait_for(|d| *d)).await.is_ok() };
        Ok(finished && index.repo(&r.id)?.head()?.is_some())
    }

    /// `GET /repos/{o}/{r}/search?q=&regex=&case=&path=&limit=&rev=`: code search. The index
    /// follows the default branch; for any other revision (a change's head, a branch) the files
    /// that differ from the default head are searched directly at that revision and the index
    /// answers for the rest.
    #[allow(clippy::too_many_arguments)]
    pub async fn search_code(&self, r: &RepoRow, q: &str, regex: bool, case_sensitive: bool, path: Option<&str>, limit: usize, rev: Option<&str>) -> Result<Value> {
        let index = self.ai.index.as_ref().ok_or_else(|| Error::Unprocessable("code search is off on this server".into()))?;
        let q = q.trim();
        if q.is_empty() || q.len() > 500 {
            return Err(Error::Invalid("q is 1-500 characters".into()));
        }
        let at = match rev.map(str::trim).filter(|v| !v.is_empty()) {
            Some(v) => Some(self.resolve_rev(r, v)?),
            None => None,
        };
        let Some(head) = self.head(r)? else {
            if let Some(at) = at {
                // Nothing landed yet: search the revision's files directly.
                return self.search_overlay(r, q, regex, case_sensitive, path, limit, None, at).await;
            }
            return Ok(json!({ "matches": [], "files_scanned": 0, "truncated": false, "head": null, "rev": null, "overlay": null }));
        };
        if !self.index_ready(r, Duration::from_millis(self.config.search_index_wait_ms)).await.map_err(Error::Internal)? {
            return Err(Error::Indexing);
        }
        let at = at.filter(|a| *a != head);
        let Some(at) = at else {
            let idx = index.repo(&r.id).map_err(Error::Internal)?;
            let source = StoreSource { store: self.store.clone(), repo: r.id.clone(), at: head };
            let glob = path.map(str::to_string);
            let filter = move |p: &str| glob.as_deref().is_none_or(|g| code_review::glob::matches(g, p));
            let res = idx.search(&query(q, regex, case_sensitive, limit), &source, &filter).await.map_err(search_error)?;
            let mut v = serde_json::to_value(res).unwrap_or_default();
            v["rev"] = json!(cs_id(&head));
            v["overlay"] = Value::Null;
            return Ok(v);
        };
        self.search_overlay(r, q, regex, case_sensitive, path, limit, Some(head), at).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn search_overlay(
        &self,
        r: &RepoRow,
        q: &str,
        regex: bool,
        case_sensitive: bool,
        path: Option<&str>,
        limit: usize,
        base: Option<ChangesetId>,
        at: ChangesetId,
    ) -> Result<Value> {
        let glob = path.map(str::to_string);
        let glob_ok = move |p: &str| glob.as_deref().is_none_or(|g| code_review::glob::matches(g, p));
        let query = query(q, regex, case_sensitive, limit);
        let matcher = code_index::Matcher::new(&query).map_err(search_error)?;
        // Files that differ from the indexed head (all of them when nothing is indexed).
        let (differ, scan): (std::collections::HashSet<String>, Vec<String>) = match base {
            Some(base) => {
                let diffs = self.store.compare(&r.id, &base, &at)?;
                let differ = diffs.iter().map(|d| d.path.to_string()).collect();
                let scan = diffs.iter().filter(|d| d.status != FileStatus::Deleted).map(|d| d.path.to_string()).filter(|p| glob_ok(p)).collect();
                (differ, scan)
            }
            None => {
                let (store, repo) = (self.store.clone(), r.id.clone());
                let paths = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<String>> {
                    let mut out = Vec::new();
                    crate::reads::walk(&store, &repo, &at, MAX_BUILD_FILES, |e| {
                        if e.kind == code_store::EntryKind::File && e.size.unwrap_or(0) <= code_index::MAX_FILE_BYTES as u64 {
                            out.push(e.path.clone());
                        }
                    })?;
                    Ok(out)
                })
                .await
                .map_err(|e| Error::Internal(e.into()))??;
                (Default::default(), paths.into_iter().filter(|p| glob_ok(p)).collect())
            }
        };
        let source = StoreSource { store: self.store.clone(), repo: r.id.clone(), at };
        let mut out = code_index::SearchResult::default();
        let overlay_files = scan.len();
        matcher.scan(&scan, &source, limit, &mut out).await;
        if let (Some(base), false) = (base, out.truncated) {
            let index = self.ai.index.as_ref().ok_or_else(|| Error::Unprocessable("code search is off on this server".into()))?;
            let idx = index.repo(&r.id).map_err(Error::Internal)?;
            let base_source = StoreSource { store: self.store.clone(), repo: r.id.clone(), at: base };
            let rest = query_with_limit(&query, limit.saturating_sub(out.matches.len()).max(1));
            let filter = move |p: &str| glob_ok(p) && !differ.contains(p);
            let res = idx.search(&rest, &base_source, &filter).await.map_err(search_error)?;
            let room = limit.clamp(1, 1000).saturating_sub(out.matches.len());
            out.truncated |= res.truncated || res.matches.len() > room;
            out.files_scanned += res.files_scanned;
            out.matches.extend(res.matches.into_iter().take(room));
        }
        out.matches.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
        out.head = base.map(|b| cs_id(&b));
        let mut v = serde_json::to_value(out).unwrap_or_default();
        v["rev"] = json!(cs_id(&at));
        v["overlay"] = json!({ "base": base.map(|b| cs_id(&b)), "files": overlay_files });
        Ok(v)
    }
}

fn query(q: &str, regex: bool, case_sensitive: bool, limit: usize) -> Query {
    Query { text: q.to_string(), regex, case_sensitive, limit: limit.clamp(1, 500) }
}

fn query_with_limit(q: &Query, limit: usize) -> Query {
    Query { limit, ..q.clone() }
}

fn search_error(e: anyhow::Error) -> Error {
    if e.to_string().starts_with("bad regex") { Error::Invalid(e.to_string()) } else { Error::Internal(e) }
}
