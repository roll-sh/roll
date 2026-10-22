//! The metadata database: one SQLite connection behind a mutex. Every write transaction is
//! recorded (a SQLite session) so the log can ship the net changes; the log also replays them
//! and copies the database to a snapshot a few pages at a time while it keeps serving.

use crate::error::{Result, StoreError};
use crate::schema;
use rusqlite::backup::{Backup, StepResult};
use rusqlite::session::{ConflictAction, ConflictType, Session};
use rusqlite::{Connection, Transaction, TransactionBehavior};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

pub(crate) struct Db {
    inner: Mutex<Inner>,
}

/// The connection plus the change recorder and snapshot copy that borrow it. Fields drop in
/// order, so the borrowers go before what they point into.
struct Inner {
    backup: Option<Backup<'static, 'static>>,
    session: Option<Session<'static>>,
    snapshot: Option<Box<Connection>>,
    conn: Box<Connection>,
    /// The batch gathering writes: the next cut ships them.
    open_batch: u64,
    /// Written since the last cut.
    dirty: bool,
}

// The session and backup are only touched with the mutex held; SQLite is built thread-safe.
unsafe impl Send for Inner {}

impl Inner {
    /// The connection with a lifetime the borrowers can hold; it lives (boxed, unmoved) as
    /// long as `self`, and they are dropped first.
    fn conn_static(&self) -> &'static Connection {
        unsafe { &*(self.conn.as_ref() as *const Connection) }
    }

    fn new_session(&self) -> Result<Session<'static>> {
        let mut s = Session::new(self.conn_static())?;
        s.attach(None::<&str>)?;
        Ok(s)
    }

    /// Net changes since the last cut, as a patchset, and the batch they close.
    fn cut(&mut self) -> Result<(Vec<u8>, u64)> {
        let batch = self.open_batch;
        self.open_batch += 1;
        self.dirty = false;
        let Some(s) = self.session.as_mut() else { return Ok((Vec::new(), batch)) };
        if s.is_empty() {
            return Ok((Vec::new(), batch));
        }
        let mut out = Vec::new();
        s.patchset_strm(&mut out)?;
        self.session = None;
        self.session = Some(self.new_session()?);
        Ok((out, batch))
    }
}

impl Db {
    /// A working copy: durability comes from the log, so the file skips fsyncs (it's rebuilt
    /// from object storage on every start).
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = OFF; PRAGMA foreign_keys = OFF; PRAGMA busy_timeout = 5000;")?;
        Ok(Self { inner: Mutex::new(Inner { backup: None, session: None, snapshot: None, conn: Box::new(conn), open_batch: 1, dirty: false }) })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn schema_version(&self) -> Result<u32> {
        Ok(self.lock().conn.query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))?)
    }

    /// Bring the schema to `version` (not recorded: replay runs the same migrations).
    pub(crate) fn migrate_to(&self, version: u32) -> Result<()> {
        let inner = self.lock();
        schema::migrate(&inner.conn, version)
    }

    /// Start recording changes for [`Self::cut`].
    pub(crate) fn record_changes(&self) -> Result<()> {
        let mut inner = self.lock();
        if inner.session.is_none() {
            inner.session = Some(inner.new_session()?);
        }
        Ok(())
    }

    pub(crate) fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let inner = self.lock();
        f(&inner.conn)
    }

    /// Run `f` in one transaction. Returns its result and the batch that will carry the
    /// changes; an error rolls everything back.
    pub(crate) fn write<T>(&self, f: impl FnOnce(&Transaction) -> Result<T>) -> Result<(T, u64)> {
        let mut inner = self.lock();
        let tx = Transaction::new_unchecked(&inner.conn, TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        inner.dirty = true;
        Ok((out, inner.open_batch))
    }

    /// [`Self::read`], and the batch that makes everything it saw durable (the open one if it
    /// holds changes, else the last one cut).
    pub(crate) fn read_seen<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<(T, u64)> {
        let inner = self.lock();
        let out = f(&inner.conn)?;
        let batch = if inner.dirty { inner.open_batch } else { inner.open_batch - 1 };
        Ok((out, batch))
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.lock().dirty
    }

    pub(crate) fn cut(&self) -> Result<(Vec<u8>, u64)> {
        self.lock().cut()
    }

    /// Replay a patchset from [`Self::cut`] onto the state it was cut from (snapshots are cut
    /// at an exact entry, so every change applies). A row already holding the change's values
    /// is overwritten with them (idempotent); a change that can't apply at all (its row is
    /// missing, or it breaks a constraint once SQLite's deferred retry is done) aborts the
    /// replay, because skipping it would silently diverge from what was acknowledged.
    pub(crate) fn apply(&self, changes: &[u8]) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let inner = self.lock();
        let mut input: &[u8] = changes;
        inner.conn.apply_strm(&mut input, None::<fn(&str) -> bool>, |kind, _| match kind {
            ConflictType::SQLITE_CHANGESET_DATA | ConflictType::SQLITE_CHANGESET_CONFLICT => ConflictAction::SQLITE_CHANGESET_REPLACE,
            _ => ConflictAction::SQLITE_CHANGESET_ABORT,
        })?;
        Ok(())
    }

    /// Begin copying the database to `dst` (see [`Self::snapshot_step`]).
    pub(crate) fn snapshot_begin(&self, dst: &Path) -> Result<()> {
        let mut inner = self.lock();
        inner.backup = None;
        inner.snapshot = None;
        let _ = std::fs::remove_file(dst);
        let mut target = Box::new(Connection::open(dst)?);
        let target_ref: &'static mut Connection = unsafe { &mut *(target.as_mut() as *mut Connection) };
        let backup = Backup::new(inner.conn_static(), target_ref)?;
        inner.snapshot = Some(target);
        inner.backup = Some(backup);
        Ok(())
    }

    /// Copy up to `pages` more pages. Writes made meanwhile reach the copy too, so when it
    /// completes it's exactly the current state; at that instant the recorded changes are cut
    /// and returned: the snapshot is the state after them.
    pub(crate) fn snapshot_step(&self, pages: i32) -> Result<Option<(Vec<u8>, u64)>> {
        let mut inner = self.lock();
        let Some(backup) = inner.backup.as_ref() else { return Err(StoreError::internal("no snapshot in progress")) };
        match backup.step(pages)? {
            StepResult::Done => {
                inner.backup = None;
                inner.snapshot = None;
                Ok(Some(inner.cut()?))
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn snapshot_abort(&self) {
        let mut inner = self.lock();
        inner.backup = None;
        inner.snapshot = None;
    }

    pub(crate) fn size_bytes(&self) -> u64 {
        let inner = self.lock();
        let pages: i64 = inner.conn.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap_or(0);
        let size: i64 = inner.conn.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap_or(4096);
        (pages * size) as u64
    }
}

/// Open a database file read-only and check it's intact (snapshot verification).
pub(crate) fn check_file(path: &Path) -> Result<u32> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let ok: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if ok != "ok" {
        return Err(StoreError::internal(format!("snapshot fails its integrity check: {ok}")));
    }
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))?)
}
