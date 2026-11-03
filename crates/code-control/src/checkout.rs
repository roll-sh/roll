//! Work trees for the checks runner and the merge queue: a few persistent checkouts per repo,
//! each moved to the changeset a job needs by applying the diff from where it was
//! ([`code_store::Store::checkout_delta`]) instead of writing the whole tree every time.
//!
//! A job leases a tree for as long as it runs (runners copy the tree into their containers, so
//! nothing writes to it). When every tree of a repo is busy, the job gets a throwaway full
//! checkout. Any trouble moving a tree (or a tree that was never written) means a fresh
//! materialize. Trees live under `<home>/code/worktrees/<repo>/<n>`. Leases are in-memory
//! locks, so none outlive the process; what a tree holds isn't recorded either, so on startup
//! [`Code::clean_worktrees`] removes every tree and throwaway checkout a previous process left
//! (including a crashed one's), and the first job of each tree checks out from scratch.

use crate::Code;
use code_store::ChangesetId;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

/// Persistent trees per repo.
pub const TREES_PER_REPO: usize = 4;

/// What a tree holds: the changeset it was checked out at, if it's known to be intact.
type State = Option<ChangesetId>;
/// A repo's trees: where each is, and its lease.
type Trees = Vec<(PathBuf, Arc<AsyncMutex<State>>)>;

#[derive(Default)]
pub struct Worktrees {
    repos: Mutex<HashMap<String, Trees>>,
    /// Checkouts by kind, for tests and logs: (delta, full, throwaway).
    pub(crate) counts: [AtomicU64; 3],
}

/// A leased work tree at the requested changeset. Dropping it returns the tree (or removes a
/// throwaway one).
pub struct Checkout {
    dir: PathBuf,
    _lease: Option<OwnedMutexGuard<State>>,
    throwaway: bool,
}

impl Checkout {
    pub fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        if self.throwaway {
            let dir = self.dir.clone();
            match tokio::runtime::Handle::try_current() {
                Ok(h) => {
                    h.spawn_blocking(move || std::fs::remove_dir_all(dir));
                }
                Err(_) => {
                    let _ = std::fs::remove_dir_all(dir);
                }
            }
        }
    }
}

impl Code {
    fn worktrees_dir(&self) -> PathBuf {
        self.config.home.join("code").join("worktrees")
    }

    /// A work tree of `repo_id` at `cs`, leased until the [`Checkout`] is dropped.
    pub async fn checkout(&self, repo_id: &str, cs: &ChangesetId) -> anyhow::Result<Checkout> {
        let leased = {
            let mut repos = self.worktrees.repos.lock();
            let trees = repos.entry(repo_id.to_string()).or_default();
            let free = trees.iter().find_map(|(dir, m)| m.clone().try_lock_owned().ok().map(|g| (dir.clone(), g, false)));
            match free {
                Some(f) => Some(f),
                None if trees.len() < TREES_PER_REPO => {
                    let dir = self.worktrees_dir().join(repo_id).join(trees.len().to_string());
                    let m = Arc::new(AsyncMutex::new(None));
                    let g = m.clone().try_lock_owned().expect("a new lock is free");
                    trees.push((dir.clone(), m));
                    // New to this process: whatever is on disk is unknown.
                    Some((dir, g, true))
                }
                None => None,
            }
        };
        let Some((dir, mut lease, fresh)) = leased else {
            // Every tree is busy: a throwaway full checkout.
            let dir = self.worktrees_dir().join(format!("{repo_id}-tmp-{}", &crate::new_id("t")[2..]));
            let _ = tokio::fs::remove_dir_all(&dir).await;
            self.store.materialize(repo_id, cs, &dir).await?;
            self.worktrees.counts[2].fetch_add(1, Ordering::Relaxed);
            return Ok(Checkout { dir, _lease: None, throwaway: true });
        };
        let moved = match (*lease, fresh) {
            (Some(at), false) if at == *cs => true,
            (Some(at), false) => match self.store.checkout_delta(repo_id, &at, cs, &dir).await {
                Ok(_) => {
                    self.worktrees.counts[0].fetch_add(1, Ordering::Relaxed);
                    true
                }
                Err(e) => {
                    tracing::debug!(repo = repo_id, error = %e, "moving a work tree failed; checking out from scratch");
                    false
                }
            },
            _ => false,
        };
        if !moved {
            *lease = None;
            let _ = tokio::fs::remove_dir_all(&dir).await;
            self.store.materialize(repo_id, cs, &dir).await?;
            self.worktrees.counts[1].fetch_add(1, Ordering::Relaxed);
        }
        *lease = Some(*cs);
        Ok(Checkout { dir, _lease: Some(lease), throwaway: false })
    }

    /// Forget a repo's work trees (it was deleted): they're removed once jobs using them finish.
    pub(crate) fn drop_worktrees(&self, repo_id: &str) {
        let trees = self.worktrees.repos.lock().remove(repo_id).unwrap_or_default();
        let dir = self.worktrees_dir().join(repo_id);
        let remove = async move {
            for (_, m) in trees {
                let _g = m.lock().await;
            }
            let _ = tokio::fs::remove_dir_all(dir).await;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn(remove);
            }
            Err(_) => drop(remove),
        }
    }

    /// Remove the work trees, throwaway checkouts and per-change preview trees a previous
    /// process left. The directories are renamed out of the way at once (so new checkouts start
    /// clean) and deleted on a background thread. Returns how many were set aside.
    pub(crate) fn clean_worktrees(&self) -> usize {
        let code_dir = self.config.home.join("code");
        let mut stale = Vec::new();
        for name in ["worktrees", "previews"] {
            let dir = code_dir.join(name);
            if dir.exists() {
                let aside = code_dir.join(format!("{name}.old-{}", &crate::new_id("t")[2..]));
                match std::fs::rename(&dir, &aside) {
                    Ok(()) => stale.push(aside),
                    Err(e) => tracing::warn!(dir = %dir.display(), error = %e, "setting stale work trees aside"),
                }
            }
        }
        // And whatever an earlier cleanup didn't finish.
        if let Ok(rd) = std::fs::read_dir(&code_dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if (n.starts_with("worktrees.old-") || n.starts_with("previews.old-")) && !stale.contains(&e.path()) {
                    stale.push(e.path());
                }
            }
        }
        let n = stale.len();
        if n > 0 {
            std::thread::spawn(move || {
                for d in stale {
                    let _ = std::fs::remove_dir_all(d);
                }
            });
        }
        n
    }

    /// `(delta, full, throwaway)` checkouts so far.
    pub fn checkout_counts(&self) -> (u64, u64, u64) {
        let c = &self.worktrees.counts;
        (c[0].load(Ordering::Relaxed), c[1].load(Ordering::Relaxed), c[2].load(Ordering::Relaxed))
    }
}
