//! Durability of acknowledged writes on SQLite, and checkpoints off the writer's path.
//!
//! The database runs in WAL mode with `synchronous=NORMAL`: a commit appends to the WAL without
//! an fsync, so it survives the process crashing but not the machine. A write is only
//! acknowledged once [`WalSync::sync`] has fsynced the WAL past its commit. That's group commit:
//! one `fdatasync` covers every commit made before it started, from any writer, and it runs
//! outside the writer's lock, so other writes go on meanwhile.
//!
//! Why that's enough: WAL frames are appended in commit order, and fsyncing the file flushes
//! every frame written to it before the call, whichever connection wrote it. SQLite only
//! overwrites WAL frames after a checkpoint has copied them into the database file and synced
//! it (`synchronous=NORMAL` syncs the WAL before a checkpoint and the database after one), so a
//! commit's frames are always either in the WAL when it's synced or already durable in the
//! database.
//!
//! Checkpoints run on a background thread with its own connection ([`spawn_checkpointer`]), so
//! their fsyncs never happen inside a commit holding the writer's lock. The writer still
//! checkpoints by itself if the WAL grows past [`crate::sqlite::WRITER_CHECKPOINT_PAGES`], in
//! case the thread falls behind.

use parking_lot::Mutex;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Weak;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// How often the checkpointer looks for WAL frames to copy into the database.
const CHECKPOINT_EVERY: Duration = Duration::from_millis(50);

pub(crate) struct WalSync {
    path: PathBuf,
    file: Mutex<Option<File>>,
    /// Sync requests made so far; each caller takes one after its commit.
    requested: AtomicU64,
    /// Every request up to this one is durable.
    done: AtomicU64,
}

impl WalSync {
    pub(crate) fn new(db: &Path) -> WalSync {
        let mut path = db.as_os_str().to_owned();
        path.push("-wal");
        WalSync { path: path.into(), file: Mutex::new(None), requested: AtomicU64::new(0), done: AtomicU64::new(0) }
    }

    /// Make every commit that finished before this call durable. Callers that arrive while a
    /// sync runs share the next one.
    pub(crate) fn sync(&self) -> std::io::Result<()> {
        let ticket = self.requested.fetch_add(1, Ordering::SeqCst) + 1;
        let mut file = self.file.lock();
        if self.done.load(Ordering::SeqCst) >= ticket {
            return Ok(());
        }
        // Every request taken by now followed its commit, so this sync covers them all.
        let upto = self.requested.load(Ordering::SeqCst);
        let f = match &mut *file {
            Some(f) => f,
            None => file.insert(File::open(&self.path)?),
        };
        f.sync_data()?;
        self.done.fetch_max(upto, Ordering::SeqCst);
        Ok(())
    }

    /// Syncs that actually reached the disk so far (tests).
    pub(crate) fn synced(&self) -> u64 {
        self.done.load(Ordering::SeqCst)
    }
}

/// Copy WAL frames into the database on a thread of its own until the database is dropped.
pub(crate) fn spawn_checkpointer(db: &Path, owner: Weak<crate::sqlite::Sqlite>) -> crate::Result<()> {
    let conn = crate::sqlite::Sqlite::checkpoint_conn(db)?;
    let name = db.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    std::thread::Builder::new()
        .name("code-sql-checkpoint".into())
        .spawn(move || {
            loop {
                std::thread::sleep(CHECKPOINT_EVERY);
                let Some(alive) = owner.upgrade() else { return };
                drop(alive);
                // PASSIVE never waits for readers or the writer; whatever it can't copy now it
                // copies next time. It does nothing when there are no new frames.
                if let Err(e) = conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(())) {
                    tracing::warn!(error = %e, db = %name, "checkpointing");
                }
            }
        })
        .map_err(|e| crate::Error::Other(format!("starting the checkpointer: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_syncs_share_fsyncs_and_all_return_durable() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.db");
        std::fs::write(dir.path().join("x.db-wal"), b"frames").unwrap();
        let w = WalSync::new(&db);
        std::thread::scope(|s| {
            for _ in 0..16 {
                s.spawn(|| {
                    for _ in 0..50 {
                        w.sync().unwrap();
                    }
                });
            }
        });
        assert_eq!(w.synced(), 16 * 50, "every request is covered");
        // No WAL: the error comes back, nothing is claimed durable.
        let missing = WalSync::new(&dir.path().join("none.db"));
        assert!(missing.sync().is_err());
        assert_eq!(missing.synced(), 0);
    }
}
