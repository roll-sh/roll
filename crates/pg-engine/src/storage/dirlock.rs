//! One open store per directory in a process. A store its users dropped without `close`
//! stays open while anything still holds it, and its background flush or compaction goes on
//! writing the directory: a store opened there meanwhile reads a manifest the old one then
//! replaces, deletes the files the old one has just written (not in the manifest it read
//! yet), and both hand out the same file ids, two writers of one SST (a checksum error, a
//! missing file or lost rows at a later open). `Store::open` closes such a store first:
//! `close` waits for the work running and stops the rest, as when the engine closes an idle
//! database before opening it again.

use super::Store;
use crate::error::Result;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Weak};

static OPEN: LazyLock<Mutex<HashMap<PathBuf, Weak<Store>>>> = LazyLock::new(Default::default);

/// Close the store of this process that has `dir` open, if one does; the key to `register`
/// the new store under.
pub(super) fn take_over(dir: &Path) -> Result<PathBuf> {
    let key = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    // (Upgraded under the lock, closed and dropped outside it.)
    let open = OPEN.lock().get(&key).and_then(Weak::upgrade);
    if let Some(s) = open
        && !s.closed.load(Ordering::Acquire)
    {
        tracing::warn!(dir = %dir.display(), "a store left open on the directory is closed first");
        s.close()?;
    }
    Ok(key)
}

/// `store` has the directory `take_over` gave `key` for.
pub(super) fn register(key: PathBuf, store: &Arc<Store>) {
    let mut m = OPEN.lock();
    m.retain(|_, s| s.strong_count() > 0);
    m.insert(key, Arc::downgrade(store));
}
