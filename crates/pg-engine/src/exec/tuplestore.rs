//! A tuplestore as tuplestore.c keeps one: rows in memory until PostgreSQL's count of them
//! (`workmem::StoreMem`) passes work_mem, then every row in a temporary file, read back in
//! order by each reader from its own place. A CTE's rows live in one (`nodes::CteState`).
//! What EXPLAIN ANALYZE's "Storage" line shows of it is `stats`.

use super::spill::{RowReader, TempAcct, TempCleanup, TempFile, put_row};
use super::workmem::{BLCKSZ, MINIMAL_TUPLE_DATA_OFFSET, StoreMem, minimal_tuple_len, work_mem};
use super::{ExecCtx, Row};
use crate::error::Result;
use crate::types::DataType;
use std::rc::Rc;

/// One reader's place: the next row's number, and (on disk) its offset in the file and the
/// reader over the bytes after it.
#[derive(Default)]
struct Place {
    row: usize,
    off: u64,
    rd: Option<RowReader>,
    /// PostgreSQL's bytes read back (its blocks count as temp blocks read).
    pg_off: u64,
}

/// The bytes a tuple of t_len `tlen` takes on tape (writetup_heap).
fn tape_len(tlen: i64, backward: bool) -> u64 {
    (tlen - MINIMAL_TUPLE_DATA_OFFSET + 4 + if backward { 4 } else { 0 }) as u64
}

pub struct Tuplestore {
    mem: StoreMem,
    types: Vec<DataType>,
    /// Trailing length words on tape (randomAccess / EXEC_FLAG_BACKWARD).
    backward: bool,
    /// The rows, while in memory.
    rows: Vec<Row>,
    /// Once on disk: the engine's file, PostgreSQL's (its accounts) and its bytes.
    file: Option<TempFile>,
    acct: Option<TempAcct>,
    cleanup: Rc<TempCleanup>,
    pg_bytes: u64,
    /// Rows stored.
    len: usize,
    /// Readers by slot (None: a closed reader's).
    places: Vec<Option<Place>>,
    buf: Vec<u8>,
}

impl Tuplestore {
    /// tuplestore_begin_heap(randomAccess = `backward`, work_mem) for rows of `types`.
    pub fn new(types: Vec<DataType>, backward: bool, ctx: &ExecCtx) -> Tuplestore {
        Tuplestore {
            mem: StoreMem::new(work_mem(ctx), backward),
            types,
            backward,
            rows: Vec::new(),
            file: None,
            acct: None,
            cleanup: ctx.temp_cleanup.clone(),
            pg_bytes: 0,
            len: 0,
            places: Vec::new(),
            buf: Vec::new(),
        }
    }

    /// A reader from the first row (tuplestore_alloc_read_pointer): its slot.
    pub fn reader(&mut self) -> usize {
        match self.places.iter().position(Option::is_none) {
            Some(i) => {
                self.places[i] = Some(Place::default());
                i
            }
            None => {
                self.places.push(Some(Place::default()));
                self.places.len() - 1
            }
        }
    }

    pub fn close_reader(&mut self, slot: usize) {
        if let Some(p) = self.places.get_mut(slot) {
            *p = None;
        }
    }

    /// Store `row` (tuplestore_puttuple); the reader `at`, if any, read it as it came (it is
    /// past it).
    pub fn put(&mut self, row: Row, at: Option<usize>, ctx: &ExecCtx) -> Result<()> {
        let tlen = minimal_tuple_len(&row, &self.types, ctx);
        let to_disk = self.mem.put(tlen);
        if self.file.is_none() && !to_disk {
            self.rows.push(row);
            self.len += 1;
            if let Some(Some(p)) = at.map(|s| &mut self.places[s]) {
                p.row = self.len;
            }
            return Ok(());
        }
        if to_disk {
            self.dump(ctx)?;
            self.mem.dumped(tlen);
        }
        let tape = tape_len(tlen, self.backward);
        self.write(&row, tape)?;
        self.len += 1;
        if let Some(Some(p)) = at.map(|s| &mut self.places[s]) {
            p.row = self.len;
            p.off = self.file.as_ref().map_or(0, TempFile::pos);
            p.rd = None;
            p.pg_off = self.pg_bytes;
        }
        Ok(())
    }

    /// One row to the file, and PostgreSQL's file grown by `tape` bytes (BufFile writes its
    /// buffer out a block at a time).
    fn write(&mut self, row: &[crate::types::Value], tape: u64) -> Result<()> {
        self.buf.clear();
        put_row(&mut self.buf, row);
        self.file.as_mut().expect("on disk").append(&self.buf)?;
        self.pg_bytes += tape;
        self.acct.as_ref().expect("on disk").pg_size(self.pg_bytes / BLCKSZ as u64 * BLCKSZ as u64)
    }

    /// The rows in memory go to a new file (dumptuples); each reader's place becomes an
    /// offset in it.
    fn dump(&mut self, ctx: &ExecCtx) -> Result<()> {
        self.file = Some(TempFile::create_raw(ctx)?);
        self.acct = Some(TempAcct::new(ctx));
        let rows = std::mem::take(&mut self.rows);
        for (i, r) in rows.iter().enumerate() {
            let pos = self.file.as_ref().unwrap().pos();
            for p in self.places.iter_mut().flatten().filter(|p| p.row == i) {
                (p.off, p.pg_off) = (pos, self.pg_bytes);
            }
            let tlen = minimal_tuple_len(r, &self.types, ctx);
            self.mem.dumped(tlen);
            self.write(r, tape_len(tlen, self.backward))?;
        }
        let end = self.file.as_ref().unwrap().pos();
        for p in self.places.iter_mut().flatten().filter(|p| p.row >= rows.len()) {
            (p.off, p.pg_off) = (end, self.pg_bytes);
        }
        Ok(())
    }

    /// Reader `slot`'s next row, if one is stored.
    pub fn read(&mut self, slot: usize, ctx: &ExecCtx) -> Result<Option<Row>> {
        let p = self.places[slot].as_mut().expect("an open reader");
        if p.row >= self.len {
            return Ok(None);
        }
        let Some(file) = self.file.as_mut() else {
            p.row += 1;
            return Ok(Some(self.rows[p.row - 1].clone()));
        };
        loop {
            if let Some(rd) = p.rd.as_mut()
                && let Some(r) = rd.read_row()?
            {
                p.row += 1;
                p.off = rd.offset();
                let blocks = p.pg_off / BLCKSZ as u64;
                p.pg_off += tape_len(minimal_tuple_len(&r, &self.types, ctx), self.backward);
                if let Some(a) = &self.acct {
                    a.pg_read((p.pg_off / BLCKSZ as u64 - blocks) * BLCKSZ as u64);
                }
                return Ok(Some(r));
            }
            p.rd = Some(file.reader(p.off, file.pos())?);
        }
    }

    /// tuplestore_get_stats: (on disk, maximum bytes).
    pub fn stats(&self) -> (bool, u64) {
        let (d, b) = self.mem.stats();
        (d, b as u64)
    }
}

impl Drop for Tuplestore {
    fn drop(&mut self) {
        if let Some(acct) = self.acct.take() {
            acct.close_later(self.pg_bytes, &self.cleanup);
        }
    }
}
