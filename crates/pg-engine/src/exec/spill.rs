//! Spill files: rows a node keeps past work_mem, written to a temporary file of its own
//! (PostgreSQL's BufFile / logical tapes). A file is made in the database's spill directory
//! and unlinked at once, so it is gone when it is dropped (the statement ended, failed or was
//! cancelled) and when the process dies; `Store::open` empties the directory besides.
//! Rows go in as `storage::row` encodings with a length in front, appended in large writes and
//! read back sequentially a buffer at a time.
//!
//! What a file counts toward temp_file_limit, pg_stat_database's temp_files / temp_bytes,
//! pg_stat_statements' temp blocks and log_temp_files is the size of the file PostgreSQL
//! would have written (`TempFile::pg_size`, set by the node from PostgreSQL's model of its
//! tapes and batch files), not the engine's smaller one.

use crate::error::{Error, Result, code};
use crate::types::Value;
use std::cell::{Cell, RefCell};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Write buffer: rows are appended in writes of this size.
const WRITE_BUF: usize = 256 << 10;
/// Read buffer of each reader (a merge of N runs holds N of them).
const READ_BUF: usize = 64 << 10;

/// A session's temporary files (fd.c's temporary_files_size, and what pgstat and
/// log_temp_files are told as files go).
#[derive(Debug, Default)]
pub struct TempUsage {
    /// PostgreSQL's bytes of the files open now (what temp_file_limit caps).
    live: AtomicU64,
    /// Files removed and their bytes, not yet in the database's statistics.
    files: AtomicI64,
    bytes: AtomicI64,
    /// pg_stat_statements' temp blocks written and read, for the running statement.
    pub blks_written: AtomicI64,
    pub blks_read: AtomicI64,
    /// Names files (pgsql_tmp<pid>.<n>), and the engine's files that stand for none.
    counter: AtomicU64,
    raw: AtomicU64,
    /// Files removed that log_temp_files wants logged: (path, size). The server log
    /// (agent 128's) takes them after each statement (`take_logged`).
    logged: parking_lot::Mutex<Vec<(String, u64)>>,
}

impl TempUsage {
    /// Files removed and their bytes since the last call (pgstat_report_tempfile's).
    pub fn take_stats(&self) -> (i64, i64) {
        (self.files.swap(0, Ordering::Relaxed), self.bytes.swap(0, Ordering::Relaxed))
    }

    /// A temporary file other than a `TempFile` (the subscriber's changes file of a streamed
    /// transaction, which logs its own line) was removed: counted for pgstat.
    pub fn count_removed(&self, size: u64) {
        if size > 0 {
            self.files.fetch_add(1, Ordering::Relaxed);
            self.bytes.fetch_add(size as i64, Ordering::Relaxed);
        }
    }

    /// "temporary file: path ..., size ..." lines due (ReportTemporaryFileUsage's LOG).
    pub fn take_logged(&self) -> Vec<(String, u64)> {
        std::mem::take(&mut *self.logged.lock())
    }

    /// pg_stat_statements' (temp_blks_read, temp_blks_written) of the running statement.
    pub fn take_blocks(&self) -> (i64, i64) {
        (self.blks_read.swap(0, Ordering::Relaxed), self.blks_written.swap(0, Ordering::Relaxed))
    }
}

/// Buffered accounts outlive nodes that materialize their inputs. Only the executor
/// owner flushes them on successful shutdown; abort releases the written bytes as-is.
#[derive(Default)]
pub(crate) struct TempCleanup {
    active: Cell<bool>,
    pending: RefCell<Vec<(TempAcct, u64)>>,
    ctes: RefCell<Vec<std::rc::Weak<RefCell<super::nodes::CteState>>>>,
}

impl TempCleanup {
    /// Whether a scope is open (a statement running).
    pub(crate) fn active(&self) -> bool {
        self.active.get()
    }

    pub(crate) fn run<T>(self: &Rc<Self>, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.scoped(false, None, f)
    }

    /// SPI and SQL-function statements have ExecutorEnd boundaries of their own.
    pub(crate) fn statement<T>(self: &Rc<Self>, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.scoped(true, None, f)
    }

    /// Only shutdown needs an added frame: ContextNode frames execution itself.
    pub(crate) fn statement_framed<T>(
        self: &Rc<Self>,
        ctx: &super::ExecCtx,
        frame: crate::error::Frame,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.scoped(true, Some((ctx, frame)), f)
    }

    fn scoped<T>(
        self: &Rc<Self>,
        statement: bool,
        frame: Option<(&super::ExecCtx, crate::error::Frame)>,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let outer = self.active.replace(true);
        let mut scope = TempScope {
            owner: self.clone(),
            outer,
            start: if outer { self.pending.borrow().len() } else { 0 },
            ctes: if outer { self.ctes.borrow().len() } else { 0 },
            success: false,
        };
        let out = f()?;
        if statement || !scope.outer {
            let finish = || self.finish_from(scope.start, scope.ctes);
            match frame {
                Some((ctx, frame)) => ctx.sess.framed(frame, |_| finish()),
                None => finish(),
            }?;
        }
        scope.success = true;
        Ok(out)
    }

    pub(crate) fn finish(&self) -> Result<()> {
        self.finish_from(0, 0)
    }

    fn finish_from(&self, start: usize, ctes: usize) -> Result<()> {
        self.release_ctes(ctes);
        let pending = self.pending.borrow_mut().split_off(start);
        for (acct, bytes) in pending {
            acct.pg_size(bytes)?;
        }
        Ok(())
    }

    pub(crate) fn abort(&self) {
        self.release_ctes(0);
        self.pending.borrow_mut().clear();
    }

    pub(crate) fn register_cte(&self, state: &Rc<RefCell<super::nodes::CteState>>) {
        self.ctes.borrow_mut().push(Rc::downgrade(state));
    }

    fn release_ctes(&self, start: usize) {
        let ctes = self.ctes.borrow_mut().split_off(start);
        for st in ctes.into_iter().filter_map(|s| s.upgrade()) {
            st.borrow_mut().release();
        }
    }

    /// A portal owns its resources across FETCH/Execute calls, even after EOF.
    pub(crate) fn portal() -> Rc<Self> {
        Rc::new(Self { active: Cell::new(true), ..Default::default() })
    }
}

struct TempScope {
    owner: Rc<TempCleanup>,
    outer: bool,
    start: usize,
    ctes: usize,
    success: bool,
}

impl Drop for TempScope {
    fn drop(&mut self) {
        if !self.success {
            self.owner.release_ctes(self.ctes);
            self.owner.pending.borrow_mut().truncate(self.start);
        }
        self.owner.active.set(self.outer);
    }
}

/// PostgreSQL's account of one temporary file it would have written: its size (what
/// temp_file_limit caps and pg_stat_statements' blocks count) and its report when it goes
/// (pg_stat_database's temp_files / temp_bytes, log_temp_files). A node whose engine file
/// stands for several of PostgreSQL's (a hash join's batch files) keeps one per file.
pub struct TempAcct {
    usage: Arc<TempUsage>,
    pg: Cell<u64>,
    limit_kb: i64,
    log_kb: i64,
    path: String,
}

impl TempAcct {
    /// A file PostgreSQL opens now (OpenTemporaryFile: the next pgsql_tmp<pid>.<n>).
    pub fn new(ctx: &super::ExecCtx) -> TempAcct {
        let usage = ctx.sess.temp.clone();
        let n = usage.counter.fetch_add(1, Ordering::Relaxed);
        let setting = |n: &str, d: i64| {
            ctx.sess.settings.borrow().get_raw(n).and_then(|v| v.parse::<i64>().ok()).unwrap_or(d)
        };
        TempAcct {
            usage,
            pg: Cell::new(0),
            limit_kb: setting("temp_file_limit", -1),
            log_kb: setting("log_temp_files", -1),
            path: format!("base/pgsql_tmp/pgsql_tmp{}.{n}", ctx.sess.pid),
        }
    }

    /// PostgreSQL's file has grown to `bytes` (FileWrite past its end): an error past
    /// temp_file_limit, and pg_stat_statements' blocks written.
    pub fn pg_size(&self, bytes: u64) -> Result<()> {
        let old = self.pg.get();
        if bytes <= old {
            return Ok(());
        }
        let total = self.usage.live.load(Ordering::Relaxed) + bytes - old;
        if self.limit_kb >= 0 && total > self.limit_kb as u64 * 1024 {
            return Err(Error::new(
                code::CONFIGURATION_LIMIT_EXCEEDED,
                format!("temporary file size exceeds \"temp_file_limit\" ({}kB)", self.limit_kb),
            ));
        }
        self.usage.live.fetch_add(bytes - old, Ordering::Relaxed);
        let blocks = bytes.div_ceil(8192) as i64 - old.div_ceil(8192) as i64;
        self.usage.blks_written.fetch_add(blocks, Ordering::Relaxed);
        self.pg.set(bytes);
        Ok(())
    }

    /// Transfer an unwritten tail to the executor owner without writing or raising an
    /// error here. The owner explicitly chooses successful shutdown or abort.
    pub(crate) fn close_later(self, bytes: u64, owner: &TempCleanup) {
        if bytes > self.pg.get() {
            owner.pending.borrow_mut().push((self, bytes));
        }
    }

    /// PostgreSQL read `bytes` of it back (temp blocks read).
    pub fn pg_read(&self, bytes: u64) {
        self.usage.blks_read.fetch_add(bytes.div_ceil(8192) as i64, Ordering::Relaxed);
    }
}

impl Drop for TempAcct {
    fn drop(&mut self) {
        // FileClose of a temporary file: ReportTemporaryFileUsage.
        let size = self.pg.get();
        self.usage.live.fetch_sub(size, Ordering::Relaxed);
        if size > 0 {
            self.usage.files.fetch_add(1, Ordering::Relaxed);
            self.usage.bytes.fetch_add(size as i64, Ordering::Relaxed);
        }
        if self.log_kb >= 0 && (size / 1024) as i64 >= self.log_kb {
            self.usage.logged.lock().push((std::mem::take(&mut self.path), size));
        }
    }
}

/// A temporary file of rows.
pub struct TempFile {
    file: Rc<File>,
    /// Bytes on disk (`buf` not yet).
    flushed: u64,
    buf: Vec<u8>,
    /// The file of PostgreSQL's it stands for (None: its node keeps the accounts,
    /// `TempAcct`).
    acct: Option<TempAcct>,
}

impl TempFile {
    pub fn create(ctx: &super::ExecCtx) -> Result<TempFile> {
        let mut f = Self::create_raw(ctx)?;
        f.acct = Some(TempAcct::new(ctx));
        Ok(f)
    }

    /// A file that stands for none of PostgreSQL's by itself.
    pub fn create_raw(ctx: &super::ExecCtx) -> Result<TempFile> {
        let dir = ctx.sess.db.store.spill_dir().join("tmp");
        let io = |e: std::io::Error| Error::new(code::INTERNAL_ERROR, format!("could not create temporary file: {e}"));
        std::fs::create_dir_all(&dir).map_err(io)?;
        let n = ctx.sess.temp.raw.fetch_add(1, Ordering::Relaxed);
        let p = dir.join(format!("e{}.{n}.{}", ctx.sess.pid, std::process::id()));
        let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&p).map_err(io)?;
        let _ = std::fs::remove_file(&p);
        Ok(TempFile { file: Rc::new(file), flushed: 0, buf: Vec::new(), acct: None })
    }

    /// Where the next row goes.
    pub fn pos(&self) -> u64 {
        self.flushed + self.buf.len() as u64
    }

    pub fn write_row(&mut self, row: &[Value]) -> Result<()> {
        put_row(&mut self.buf, row);
        if self.buf.len() >= WRITE_BUF {
            self.flush()?;
        }
        Ok(())
    }

    /// Rows encoded elsewhere (`put_row`): where they went.
    pub fn append(&mut self, rows: &[u8]) -> Result<(u64, u64)> {
        let start = self.pos();
        self.buf.extend_from_slice(rows);
        if self.buf.len() >= WRITE_BUF {
            self.flush()?;
        }
        Ok((start, self.pos()))
    }

    pub fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        self.file
            .write_all_at(&self.buf, self.flushed)
            .map_err(|e| Error::new(code::DISK_FULL, format!("could not write to temporary file: {e}")))?;
        self.flushed += self.buf.len() as u64;
        self.buf.clear();
        Ok(())
    }

    /// The rows written between `start` and `end` (positions), read back in order.
    pub fn reader(&mut self, start: u64, end: u64) -> Result<RowReader> {
        if end > self.flushed {
            self.flush()?;
        }
        Ok(RowReader { file: self.file.clone(), pos: start, end, buf: Vec::new(), at: 0 })
    }

    /// PostgreSQL's file has grown to `bytes` (`TempAcct::pg_size`).
    pub fn pg_size(&self, bytes: u64) -> Result<()> {
        self.acct.as_ref().map_or(Ok(()), |a| a.pg_size(bytes))
    }

    /// PostgreSQL read `bytes` of it back.
    pub fn pg_read(&self, bytes: u64) {
        if let Some(a) = &self.acct {
            a.pg_read(bytes);
        }
    }
}

/// A row as a temporary file holds it: its length, then its encoding.
pub fn put_row(buf: &mut Vec<u8>, row: &[Value]) {
    let start = buf.len();
    buf.extend_from_slice(&[0; 4]);
    crate::storage::row::encode_row_into(buf, row);
    let n = (buf.len() - start - 4) as u32;
    buf[start..start + 4].copy_from_slice(&n.to_le_bytes());
}

/// A row with two words in front (a hash join's batch files: its hash value and its
/// MinimalTuple's length).
pub fn put_tagged_row(buf: &mut Vec<u8>, tag: [u32; 2], row: &[Value]) {
    buf.extend_from_slice(&tag[0].to_le_bytes());
    buf.extend_from_slice(&tag[1].to_le_bytes());
    put_row(buf, row);
}

/// Rows of a temporary file, read in order.
pub struct RowReader {
    file: Rc<File>,
    pos: u64,
    end: u64,
    buf: Vec<u8>,
    at: usize,
}

impl RowReader {
    /// The file offset of the next row.
    pub fn offset(&self) -> u64 {
        self.pos - (self.buf.len() - self.at) as u64
    }

    /// A row `put_tagged_row` wrote.
    pub fn read_tagged_row(&mut self) -> Result<Option<([u32; 2], Vec<Value>)>> {
        let Some(t) = self.take(8)? else { return Ok(None) };
        let tag = [u32::from_le_bytes(t[..4].try_into().unwrap()), u32::from_le_bytes(t[4..].try_into().unwrap())];
        let row = self.read_row()?.ok_or_else(|| Error::new(code::INTERNAL_ERROR, "temporary file ended early"))?;
        Ok(Some((tag, row)))
    }
    pub fn read_row(&mut self) -> Result<Option<Vec<Value>>> {
        let Some(len) = self.take(4)?.map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize) else {
            return Ok(None);
        };
        let b = self.take(len)?.ok_or_else(|| Error::new(code::INTERNAL_ERROR, "temporary file ended early"))?;
        crate::storage::row::decode_row(b).map(Some)
    }

    /// The next `n` bytes, refilling the buffer as needed.
    fn take(&mut self, n: usize) -> Result<Option<&[u8]>> {
        if self.at + n > self.buf.len() {
            let left = self.buf.len() - self.at;
            let remaining = (self.end - self.pos) as usize;
            if left + remaining < n || (left == 0 && remaining == 0) {
                return Ok(None);
            }
            let want = (n - left).max(READ_BUF).min(remaining);
            self.buf.drain(..self.at);
            self.at = 0;
            let old = self.buf.len();
            self.buf.resize(old + want, 0);
            self.file
                .read_exact_at(&mut self.buf[old..], self.pos)
                .map_err(|e| Error::new(code::INTERNAL_ERROR, format!("could not read from temporary file: {e}")))?;
            self.pos += want as u64;
        }
        let s = &self.buf[self.at..self.at + n];
        self.at += n;
        Ok(Some(s))
    }
}
