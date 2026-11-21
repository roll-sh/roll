//! The SQLite backend: one writer connection, a pool of readers (WAL), and group-committed
//! fsyncs of the WAL after each write (`durable.rs`).

use crate::durable::WalSync;
use crate::{Error, Result, Row, RowRef, SqliteOptions, ValueRef};
use parking_lot::{Mutex, MutexGuard};
use rusqlite::Connection;
use rusqlite::types::{ToSqlOutput, ValueRef as SqRef};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// WAL pages (4 KiB) the writer lets build up before it checkpoints inside a commit. The
/// checkpointer thread normally copies them long before; this is the backstop.
pub const WRITER_CHECKPOINT_PAGES: u32 = 8192;

pub(crate) struct Sqlite {
    writer: Mutex<Connection>,
    readers: Vec<Mutex<Connection>>,
    pub(crate) wal: WalSync,
    /// In-process wake-ups ([`crate::Db::listen`], [`crate::Db::notify_after_commit`]).
    listeners: Mutex<std::collections::HashMap<String, tokio::sync::watch::Sender<u64>>>,
}

impl Sqlite {
    pub(crate) fn open(path: &Path, opts: &SqliteOptions) -> Result<Arc<Sqlite>> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Error::Other(format!("{}: {e}", dir.display())))?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "wal_autocheckpoint", WRITER_CHECKPOINT_PAGES)?;
        conn.execute_batch(&format!(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000; PRAGMA cache_size=-{};",
            opts.cache_kib
        ))?;
        let readers = (0..opts.readers)
            .map(|_| {
                let r = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
                r.execute_batch("PRAGMA busy_timeout=5000; PRAGMA cache_size=-16384;")?;
                Ok(Mutex::new(r))
            })
            .collect::<Result<_>>()?;
        let db = Arc::new(Sqlite { writer: Mutex::new(conn), readers, wal: WalSync::new(path), listeners: Mutex::new(Default::default()) });
        crate::durable::spawn_checkpointer(path, Arc::downgrade(&db))?;
        Ok(db)
    }

    /// A wake-up for `notify(channel)` on this handle (any clone of it, in this process).
    pub(crate) fn listen(&self, channel: &str) -> tokio::sync::watch::Receiver<u64> {
        self.listeners.lock().entry(channel.to_string()).or_insert_with(|| tokio::sync::watch::channel(0).0).subscribe()
    }

    pub(crate) fn notify(&self, channel: &str) {
        let mut l = self.listeners.lock();
        if let Some(tx) = l.get(channel) {
            if tx.is_closed() {
                l.remove(channel);
            } else {
                tx.send_modify(|n| *n += 1);
            }
        }
    }

    fn reader(&self) -> MutexGuard<'_, Connection> {
        for r in &self.readers {
            if let Some(g) = r.try_lock() {
                return g;
            }
        }
        if self.readers.is_empty() {
            return self.writer.lock();
        }
        // All busy: wait on this thread's own reader, so threads spread over them.
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        thread_local! { static SLOT: usize = NEXT.fetch_add(1, Ordering::Relaxed); }
        self.readers[SLOT.with(|s| *s) % self.readers.len()].lock()
    }

    pub(crate) fn read<T, E: From<Error>>(&self, f: impl FnOnce(&Connection) -> Result<T, E>) -> Result<T, E> {
        f(&self.reader())
    }

    /// `f` in one transaction on the writer with `secure_delete` on (what it frees is zeroed), then
    /// the WAL checkpointed into the file and truncated: nothing it overwrote stays readable in
    /// the database's free pages or its `-wal`.
    pub(crate) fn write_erasing<T, E: From<Error>>(&self, f: impl FnOnce(&Connection) -> Result<T, E>) -> Result<T, E> {
        let conn = self.writer.lock();
        conn.execute_batch("PRAGMA secure_delete = ON").map_err(Error::from)?;
        let out = (|| {
            conn.execute_batch("BEGIN IMMEDIATE").map_err(Error::from)?;
            let _guard = Rollback(&conn);
            let v = f(&conn)?;
            conn.execute_batch("COMMIT").map_err(Error::from)?;
            Ok::<T, E>(v)
        })();
        let _ = conn.execute_batch("PRAGMA secure_delete = OFF");
        let v = out?;
        empty_wal(&conn, std::time::Duration::from_secs(30)).map_err(E::from)?;
        Ok(v)
    }

    /// One transaction on the writer; then, if `sync`, wait for the WAL to be durable past it.
    pub(crate) fn write<T, E: From<Error>>(&self, sync: bool, f: impl FnOnce(&Connection) -> Result<T, E>) -> Result<T, E> {
        let out = {
            let conn = self.writer.lock();
            conn.execute_batch("BEGIN IMMEDIATE").map_err(Error::from)?;
            // Rolls back if `f` fails, panics or the commit fails, so the next writer doesn't
            // find a transaction open.
            let _guard = Rollback(&conn);
            let v = f(&conn)?;
            conn.execute_batch("COMMIT").map_err(Error::from)?;
            v
        };
        if sync {
            self.sync()?;
        }
        Ok(out)
    }

    pub(crate) fn sync(&self) -> Result<()> {
        self.wal.sync().map_err(|e| Error::Other(format!("syncing the WAL: {e}")))
    }

    pub(crate) fn checkpoint_conn(path: &Path) -> Result<Connection> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        conn.execute_batch("PRAGMA busy_timeout=5000;")?;
        Ok(conn)
    }
}

struct Rollback<'a>(&'a Connection);

impl Drop for Rollback<'_> {
    fn drop(&mut self) {
        if !self.0.is_autocommit() {
            let _ = self.0.execute_batch("ROLLBACK");
        }
    }
}

struct P<'a>(ValueRef<'a>);

impl rusqlite::ToSql for P<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(match self.0 {
            ValueRef::Null => SqRef::Null,
            ValueRef::Int(i) => SqRef::Integer(i),
            ValueRef::Real(r) => SqRef::Real(r),
            ValueRef::Text(s) => SqRef::Text(s.as_bytes()),
            ValueRef::Blob(b) => SqRef::Blob(b),
        }))
    }
}

fn bind<'a>(values: &'a [ValueRef<'a>]) -> impl rusqlite::Params + 'a {
    rusqlite::params_from_iter(values.iter().map(|v| P(*v)))
}

pub(crate) fn execute(c: &Connection, sql: &str, values: &[ValueRef<'_>]) -> Result<usize> {
    let mut st = c.prepare_cached(sql)?;
    Ok(st.execute(bind(values))?)
}

pub(crate) fn query_row<T>(c: &Connection, sql: &str, values: &[ValueRef<'_>], f: &mut dyn FnMut(&Row<'_>) -> Result<T>) -> Result<T> {
    let mut st = c.prepare_cached(sql)?;
    let mut rows = st.query(bind(values))?;
    match rows.next()? {
        Some(r) => f(&Row(RowRef::Sqlite(r))),
        None => Err(Error::NoRows),
    }
}

pub(crate) fn query<T>(c: &Connection, sql: &str, values: &[ValueRef<'_>], f: &mut dyn FnMut(&Row<'_>) -> Result<T>) -> Result<Vec<T>> {
    let mut st = c.prepare_cached(sql)?;
    let mut rows = st.query(bind(values))?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        out.push(f(&Row(RowRef::Sqlite(r)))?);
    }
    Ok(out)
}

pub(crate) fn column<T>(r: &rusqlite::Row<'_>, i: usize, f: impl FnOnce(ValueRef<'_>) -> Result<T>) -> Result<T> {
    let v = match r.get_ref(i)? {
        SqRef::Null => ValueRef::Null,
        SqRef::Integer(i) => ValueRef::Int(i),
        SqRef::Real(r) => ValueRef::Real(r),
        SqRef::Text(t) => ValueRef::Text(std::str::from_utf8(t).map_err(|e| Error::Conversion(format!("text isn't UTF-8: {e}")))?),
        SqRef::Blob(b) => ValueRef::Blob(b),
    };
    f(v)
}

/// Copy the whole WAL into the database file and truncate it, so what an erasing write removed
/// is in neither. A TRUNCATE checkpoint answers busy, rather than failing, when it can't finish:
/// the background checkpointer holds the checkpoint lock, or a reader is on an older snapshot
/// past the busy timeout. Tried again until it's done, for at most `within`.
fn empty_wal(conn: &Connection, within: std::time::Duration) -> Result<()> {
    let until = std::time::Instant::now() + within;
    loop {
        let (busy, log, copied): (i64, i64, i64) = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        if busy == 0 && log == copied {
            return Ok(());
        }
        if std::time::Instant::now() >= until {
            return Err(Error::Other(format!("the WAL couldn't be emptied after an erasing write ({copied} of {log} frames copied): a reader held an older snapshot")));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(test)]
mod erasing_tests {
    /// A checkpoint that can't finish at once (a reader on an older snapshot) is tried again:
    /// the WAL ends empty, not with the erased rows still in it.
    #[test]
    fn the_wal_is_emptied_even_when_a_checkpoint_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.db");
        let w = rusqlite::Connection::open(&path).unwrap();
        w.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=0; CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('old');").unwrap();
        let r = rusqlite::Connection::open(&path).unwrap();
        r.execute_batch("BEGIN").unwrap();
        let _: String = r.query_row("SELECT v FROM t", [], |x| x.get(0)).unwrap();
        w.execute_batch("UPDATE t SET v = 'secret'").unwrap();
        let reader = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            r.execute_batch("COMMIT").unwrap();
        });
        super::empty_wal(&w, std::time::Duration::from_secs(10)).unwrap();
        reader.join().unwrap();
        assert_eq!(std::fs::metadata(dir.path().join("e.db-wal")).map(|m| m.len()).unwrap_or(0), 0, "the WAL is empty");
    }
}
