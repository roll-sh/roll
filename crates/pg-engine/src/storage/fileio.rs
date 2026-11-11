//! What the stores write to their files and sync, counted for the process: a checkpoint
//! reports its part of it (log_checkpoints' "wrote N buffers", "N WAL file(s) added, M
//! removed", the sync times and files) as the difference across its flush.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

static SST_BYTES: AtomicU64 = AtomicU64::new(0);
static SYNCS: AtomicU64 = AtomicU64::new(0);
static SYNC_US: AtomicU64 = AtomicU64::new(0);
static LONGEST_US: AtomicU64 = AtomicU64::new(0);
static WAL_ADDED: AtomicU64 = AtomicU64::new(0);
static WAL_REMOVED: AtomicU64 = AtomicU64::new(0);

/// The counts at one moment, or between two.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileIo {
    /// Bytes of table files (SSTs) written.
    pub sst_bytes: u64,
    /// Data files synced (SSTs, manifests and their directories), the time it took and
    /// the longest one (microseconds).
    pub syncs: u64,
    pub sync_us: u64,
    pub longest_us: u64,
    /// Write-ahead log files made and removed.
    pub wal_added: u64,
    pub wal_removed: u64,
}

impl FileIo {
    /// The counts now, the longest sync counting from here.
    pub fn begin() -> FileIo {
        LONGEST_US.store(0, Relaxed);
        FileIo::now()
    }

    fn now() -> FileIo {
        FileIo {
            sst_bytes: SST_BYTES.load(Relaxed),
            syncs: SYNCS.load(Relaxed),
            sync_us: SYNC_US.load(Relaxed),
            longest_us: LONGEST_US.load(Relaxed),
            wal_added: WAL_ADDED.load(Relaxed),
            wal_removed: WAL_REMOVED.load(Relaxed),
        }
    }

    /// What happened since `begin` returned `self`.
    pub fn end(self) -> FileIo {
        let n = FileIo::now();
        FileIo {
            sst_bytes: n.sst_bytes - self.sst_bytes,
            syncs: n.syncs - self.syncs,
            sync_us: n.sync_us - self.sync_us,
            longest_us: n.longest_us,
            wal_added: n.wal_added - self.wal_added,
            wal_removed: n.wal_removed - self.wal_removed,
        }
    }
}

/// A table file of `bytes` bytes was written.
pub fn wrote_sst(bytes: u64) {
    SST_BYTES.fetch_add(bytes, Relaxed);
}

/// Sync a data file (`f`: its sync_all), counted.
pub fn sync(f: impl FnOnce() -> std::io::Result<()>) -> std::io::Result<()> {
    let t = Instant::now();
    let r = f();
    let us = t.elapsed().as_micros() as u64;
    SYNCS.fetch_add(1, Relaxed);
    SYNC_US.fetch_add(us, Relaxed);
    LONGEST_US.fetch_max(us, Relaxed);
    r
}

pub fn wal_added() {
    WAL_ADDED.fetch_add(1, Relaxed);
}

pub fn wal_removed() {
    WAL_REMOVED.fetch_add(1, Relaxed);
}
