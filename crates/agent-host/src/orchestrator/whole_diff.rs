//! A thread's whole diff stat ([`Thread::diff`]): its base checkpoint to its files now, the
//! numbers of its "Whole thread" diff, kept on the record so the list, the board and the cards
//! count every change, not only the last run's.
//!
//! A run's end sets it from the checkpoint it just took, and a rollback from the checkpoint it
//! went back to (both tree to tree). Between runs a terminal or `fs.write` can change files, so
//! they [`Host::touch_diff`]: the thread is counted again once it has been quiet for [`QUIET`]
//! (never later than [`LATEST`] after the first change), from the working tree through a private
//! index like a checkpoint's, never the user's. A whole-thread `diff.get` stores what it counted
//! too, and threads a daemon before this one never counted are counted once after it starts.

use super::{Host, Rt};
use crate::git;
use agent_proto::*;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Weak;
use std::time::Duration;
use tokio::time::Instant;

/// How long a thread's files stay untouched before they're counted again.
const QUIET: Duration = Duration::from_millis(800);
/// The longest a refresh waits while the files keep changing (a long build in a terminal).
const LATEST: Duration = Duration::from_secs(10);

/// Threads waiting to be counted again: when the first and the newest change came.
#[derive(Default)]
pub(crate) struct Pending(std::sync::Mutex<HashMap<ThreadId, (Instant, Instant)>>);

impl Host {
    /// The commit a thread's whole diff starts from: its base checkpoint, else (no run yet) the
    /// checkout's HEAD, as the "Whole thread" diff does.
    pub(crate) async fn diff_base(&self, thread: &ThreadId, cwd: &Path) -> anyhow::Result<Option<String>> {
        match self.checkpoints_of(thread)?.into_iter().find(|c| c.ordinal == 0) {
            Some(base) => Ok(Some(base.commit)),
            None => git::head(cwd).await,
        }
    }

    /// Sets the record's whole stat from `base` to `to` (commits or trees); the caller stores
    /// the thread. Leaves it as it was when git fails.
    pub(crate) async fn count_whole(&self, rt: &mut Rt, to: &str) {
        let cwd = self.git_dir(&rt.thread);
        let counted = match self.diff_base(&rt.thread.id, &cwd).await {
            Ok(Some(base)) => git::stat(&cwd, &base, to).await,
            Ok(None) => return,
            Err(e) => Err(e),
        };
        match counted {
            Ok(stat) => rt.thread.diff = Some(stat),
            Err(e) => tracing::warn!("counting the changes of {}: {e:#}", rt.thread.id),
        }
    }

    /// The thread's files may have changed outside a run: count them again once it goes quiet.
    pub(crate) fn touch_diff(&self, thread: &ThreadId) {
        let at = Instant::now();
        {
            let mut pending = self.whole_diff.0.lock().unwrap();
            if let Some((_, last)) = pending.get_mut(thread) {
                *last = at;
                return;
            }
            pending.insert(thread.clone(), (at, at));
        }
        let (me, id) = (self.me.clone(), thread.clone());
        tokio::spawn(async move {
            loop {
                let due = {
                    let Some(host) = me.upgrade() else { return };
                    let mut pending = host.whole_diff.0.lock().unwrap();
                    let Some(&(first, last)) = pending.get(&id) else { return };
                    let due = (last + QUIET).min(first + LATEST);
                    if due <= Instant::now() {
                        pending.remove(&id);
                        None
                    } else {
                        Some(due)
                    }
                };
                match due {
                    Some(due) => tokio::time::sleep_until(due).await,
                    None => break,
                }
            }
            if let Some(host) = me.upgrade() {
                host.refresh_diff(&id).await;
            }
        });
    }

    /// Counts the thread's working tree against its base and stores the record if the numbers
    /// moved. A running thread is left to its run's end; the lock isn't held while git reads the
    /// files.
    pub(crate) async fn refresh_diff(&self, id: &ThreadId) {
        let (cwd, to) = {
            let Ok(rt) = self.lock(id).await else { return };
            if !rt.git || rt.active.is_some() || rt.thread.archived_at.is_some() {
                return;
            }
            // A cleaned-up worktree's files are its newest checkpoint (none yet: nothing to count).
            let to = match &rt.thread.worktree {
                Some(w) if w.removed_at.is_some() => match &rt.private.head {
                    Some(head) => Some(head.clone()),
                    None => return,
                },
                _ => None,
            };
            (self.git_dir(&rt.thread), to)
        };
        let counted = async {
            let Some(base) = self.diff_base(id, &cwd).await? else { return Ok(None) };
            let to = match to {
                Some(head) => head,
                None => git::working_tree(&cwd).await?,
            };
            anyhow::Ok(Some(git::stat(&cwd, &base, &to).await?))
        };
        match counted.await {
            Ok(Some(stat)) => self.keep_diff(id, stat).await,
            Ok(None) => {}
            Err(e) => tracing::debug!("counting the changes of {id}: {e:#}"),
        }
    }

    /// Stores `stat` as the thread's whole stat if it changed and no run started meanwhile (a
    /// run's end counts it then).
    pub(crate) async fn keep_diff(&self, id: &ThreadId, stat: DiffStat) {
        let Ok(mut rt) = self.lock(id).await else { return };
        if rt.active.is_some() || rt.thread.diff == Some(stat) {
            return;
        }
        rt.thread.diff = Some(stat);
        if let Err(e) = self.put(vec![Entity::Thread(rt.thread.clone())]) {
            tracing::warn!("storing the changes of {id}: {e:#}");
        }
    }
}

/// Counts, one at a time in the background, the threads with runs that no daemon has counted
/// yet (records from before [`Thread::diff`]).
pub(crate) fn spawn_counting(me: Weak<Host>) {
    tokio::spawn(async move {
        let uncounted: Vec<ThreadId> = {
            let Some(host) = me.upgrade() else { return };
            match host.store.list(EntityKind::Thread) {
                Ok(threads) => threads
                    .into_iter()
                    .filter_map(|e| match e {
                        Entity::Thread(t) if t.diff.is_none() && t.run_count > 0 && t.archived_at.is_none() => Some(t.id),
                        _ => None,
                    })
                    .collect(),
                Err(e) => return tracing::warn!("listing threads to count: {e:#}"),
            }
        };
        for id in uncounted {
            let Some(host) = me.upgrade() else { return };
            host.refresh_diff(&id).await;
        }
    });
}
