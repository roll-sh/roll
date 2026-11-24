//! Rows handed from the store to a scan a batch at a time, by reference (`Scan::fill_batch`,
//! `TxnScan::fill_batch`): the keys copied into one buffer, the values left in the blocks
//! (and memtable entries) holding them. A scan reading through a batch allocates nothing per
//! row: a block's bytes are referenced once per batch, not once per row, and the buffers are
//! reused from batch to batch.
use bytes::Bytes;
use std::sync::{Arc, OnceLock};

/// A source of rows a batch at a time (`TxnScan`, a store `Scan`): up to `max` more rows
/// appended to `b` per call; none appended means it is done.
pub trait FillRows: Send {
    fn fill(&mut self, b: &mut RowBatch, max: usize) -> crate::error::Result<()>;
}

impl FillRows for super::Scan {
    fn fill(&mut self, b: &mut RowBatch, max: usize) -> crate::error::Result<()> {
        self.fill_batch(b, max)
    }
}

/// A bulk read's source (`Store::bulk_range`): the blocks its fills read evict nothing from
/// the block cache (`storage::bulk_read`).
pub struct BulkFill<F>(pub F);

impl<F: FillRows> FillRows for BulkFill<F> {
    fn fill(&mut self, b: &mut RowBatch, max: usize) -> crate::error::Result<()> {
        super::bulk_read(|| self.0.fill(b, max))
    }
}

/// Where an entry's value is: in `bufs[buf]` at `off` (`buf == OWN`: in `own`).
#[derive(Clone, Copy)]
struct Ent {
    key_end: u32,
    buf: u32,
    off: u32,
    len: u32,
}

const OWN: u32 = u32::MAX;

/// A batch of (key, value) rows and a cursor over them (`pos`).
#[derive(Default)]
pub struct RowBatch {
    keys: Vec<u8>,
    ents: Vec<Ent>,
    bufs: Vec<Bytes>,
    /// Values copied (a derived index entry value; rows a reader resolved).
    own: Vec<u8>,
    pos: usize,
    /// Rows of column blocks (format 5, `sst::Block::cols`) not made, in a batch that takes
    /// them (`take_col_rows`); boxed, so a batch without them is as it was (made, cleared and
    /// dropped as before: many readers make a batch a call).
    col: Option<Box<ColRows>>,
    /// Fills may leave rows of column blocks unmade (`take_col_rows`).
    cols_ok: bool,
}

/// A batch's rows of column blocks: each refers to an empty buffer of `bufs`. The column
/// blocks by their buffers' index; the runs of rows they hold (`col_runs`, for readers of
/// columns: `cols::decode_cols`); by row of the batch (up to the last such row), its block row
/// and its bytes once a reader wanted them (`lazy_row`).
#[derive(Default)]
struct ColRows {
    blocks: Vec<(u32, Arc<super::sst::Block>)>,
    runs: Vec<ColRun>,
    rows: Vec<(u32, OnceLock<Bytes>)>,
}

/// Rows `at..at + n` of a batch are rows `row..row + n` of the column block of buffer `buf`.
#[derive(Clone, Copy, Debug)]
pub struct ColRun {
    pub at: u32,
    pub n: u32,
    pub buf: u32,
    pub row: u32,
}

impl RowBatch {
    /// Empty, the buffers kept.
    pub fn clear(&mut self) {
        self.keys.clear();
        self.ents.clear();
        self.bufs.clear();
        self.own.clear();
        self.pos = 0;
        if self.col.is_some() {
            self.clear_col();
        }
    }

    #[cold]
    #[inline(never)]
    fn clear_col(&mut self) {
        if let Some(c) = &mut self.col {
            c.blocks.clear();
            c.runs.clear();
            c.rows.clear();
        }
    }

    /// Row `i` of a column block: its bytes, made when first wanted.
    #[cold]
    #[inline(never)]
    fn lazy_row(&self, i: usize) -> &Bytes {
        let e = self.ents[i];
        let c = self.col.as_ref().expect("a row of a column block");
        let blk = self.col_block(e.buf).expect("a row of a column block");
        let (row, cell) = &c.rows[i];
        cell.get_or_init(|| blk.row_at(*row as usize, e.len as usize))
    }

    /// The rows from column blocks, in order.
    pub fn col_runs(&self) -> &[ColRun] {
        self.col.as_ref().map_or(&[], |c| &c.runs)
    }

    /// The column block of buffer `b`.
    pub fn col_block(&self, b: u32) -> Option<&Arc<super::sst::Block>> {
        self.col.as_ref()?.blocks.iter().rev().find(|(x, _)| *x == b).map(|(_, blk)| blk)
    }

    /// The index rows of column block `blk` pushed next refer to (`push_col`, `note_rows`).
    pub fn buf_block(&mut self, blk: &Arc<super::sst::Block>) -> u32 {
        let last = self.bufs.len() as u32;
        let c = self.col.get_or_insert_with(Default::default);
        if let Some((b, x)) = c.blocks.last()
            && *b + 1 == last
            && Arc::ptr_eq(x, blk)
        {
            return *b;
        }
        c.blocks.push((last, blk.clone()));
        self.bufs.push(Bytes::new());
        last
    }

    /// A row of a column block (`buf_block`), `len` bytes, its block row `row`.
    pub fn push_col(&mut self, key: &[u8], buf: u32, len: usize, row: u32) {
        self.push_ent(key, buf, 0, len);
        self.note_rows(1, row);
    }

    /// The last `n` rows pushed are rows `row..row + n` of their column block's buffer.
    pub fn note_rows(&mut self, n: usize, row: u32) {
        let at = (self.ents.len() - n) as u32;
        let buf = self.ents[at as usize].buf;
        let c = self.col.as_mut().expect("rows of a column block (buf_block)");
        c.rows.resize_with(at as usize, || (u32::MAX, OnceLock::new()));
        c.rows.extend((0..n as u32).map(|j| (row + j, OnceLock::new())));
        if let Some(r) = c.runs.last_mut()
            && r.buf == buf
            && r.at + r.n == at
            && r.row + r.n == row
        {
            r.n += n as u32;
            return;
        }
        c.runs.push(ColRun { at, n: n as u32, buf, row });
    }

    /// Rows in the batch (read or not).
    pub fn len(&self) -> usize {
        self.ents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ents.is_empty()
    }

    /// Rows not yet passed (`advance`).
    pub fn remaining(&self) -> usize {
        self.ents.len() - self.pos
    }

    /// The cursor: the index of the row `peek` shows.
    pub fn pos(&self) -> usize {
        self.pos
    }

    #[inline]
    fn key_start(&self, i: usize) -> usize {
        if i == 0 { 0 } else { self.ents[i - 1].key_end as usize }
    }

    /// Whether some row is a column block's (`lazy`): only in a batch that takes them
    /// (`take_col_rows`), until `make_rows`.
    #[inline(always)]
    pub fn has_col_rows(&self) -> bool {
        self.col.as_ref().is_some_and(|c| !c.blocks.is_empty())
    }

    /// Whether a fill may leave rows of column blocks unmade in it (`take_col_rows`).
    #[inline(always)]
    pub fn takes_col_rows(&self) -> bool {
        self.cols_ok
    }

    /// Let fills leave rows of column blocks unmade (format 5): the reader takes their columns
    /// (`cols::decode_cols`) and calls `make_rows` before it reads one of them by index (`get`,
    /// `value_in`, `take`, `peek`: they read rows made, as before format 5). Kept by `clear`.
    pub fn take_col_rows(&mut self, on: bool) {
        self.cols_ok = on;
    }

    /// Every row of a column block made, each run's into one buffer (`has_col_rows` false
    /// after): a fill's for a batch that doesn't take them, a reader's that does before it
    /// reads rows by index.
    #[cold]
    #[inline(never)]
    pub fn make_rows(&mut self) {
        let runs = std::mem::take(&mut self.col.as_mut().expect("rows of column blocks").runs);
        for run in runs {
            let blk = self.col_block(run.buf).expect("a row of a column block").clone();
            let (a, n) = (run.at as usize, run.n as usize);
            let mut buf = Vec::with_capacity(self.ents[a..a + n].iter().map(|e| e.len as usize).sum());
            let mut at = Vec::with_capacity(n);
            for j in 0..n {
                at.push(buf.len());
                blk.row_into(run.row as usize + j, &mut buf);
            }
            let b = self.bufs.len() as u32;
            self.bufs.push(Bytes::from(buf));
            for (j, o) in at.into_iter().enumerate() {
                let e = &mut self.ents[a + j];
                e.buf = b;
                e.off = o as u32;
            }
        }
        self.clear_col();
    }

    /// Row `i`'s key and value.
    pub fn get(&self, i: usize) -> (&[u8], &[u8]) {
        let e = self.ents[i];
        let k = &self.keys[self.key_start(i)..e.key_end as usize];
        let (o, l) = (e.off as usize, e.len as usize);
        let v = match e.buf {
            OWN => &self.own[o..o + l],
            b => &self.bufs[b as usize][o..o + l],
        };
        (k, v)
    }

    /// Row `i`'s value with the buffer holding it and where in it (None: a copied value, in
    /// the batch's own buffer, valid until it is cleared).
    #[inline]
    pub fn value_in(&self, i: usize) -> (&[u8], Option<(&Bytes, usize)>) {
        let e = self.ents[i];
        let (o, l) = (e.off as usize, e.len as usize);
        match e.buf {
            OWN => (&self.own[o..o + l], None),
            b => {
                let buf = &self.bufs[b as usize];
                (&buf[o..o + l], Some((buf, o)))
            }
        }
    }

    /// `value_in` of a batch that may hold rows of column blocks (`cols::decode_cols`' general
    /// rows of one): such a row made alone, kept for the batch (`lazy_row`).
    #[cold]
    #[inline(never)]
    pub fn value_in_any(&self, i: usize) -> (&[u8], Option<(&Bytes, usize)>) {
        let e = self.ents[i];
        let (o, l) = (e.off as usize, e.len as usize);
        match e.buf {
            OWN => (&self.own[o..o + l], None),
            b => {
                let buf = &self.bufs[b as usize];
                match buf.get(o..o + l) {
                    Some(v) => (v, Some((buf, o))),
                    None => {
                        let r = self.lazy_row(i);
                        (r, Some((r, 0)))
                    }
                }
            }
        }
    }

    /// Row `i`'s value as `Bytes`: a slice of the buffer holding it (a copy of a copied one).
    pub fn value_bytes(&self, i: usize) -> Bytes {
        match self.value_in(i) {
            (v, Some((buf, off))) => buf.slice(off..off + v.len()),
            (v, None) => Bytes::copy_from_slice(v),
        }
    }

    /// The last row's key.
    pub fn last_key(&self) -> Option<&[u8]> {
        let n = self.ents.len();
        (n > 0).then(|| &self.keys[self.key_start(n - 1)..])
    }

    /// The row at the cursor (None: none left).
    pub fn peek(&self) -> Option<(&[u8], &[u8])> {
        (self.pos < self.ents.len()).then(|| self.get(self.pos))
    }

    /// Room for `rows` more rows of `key_bytes` of keys and `own_bytes` of copied values.
    pub fn reserve(&mut self, rows: usize, key_bytes: usize, own_bytes: usize) {
        self.ents.reserve(rows);
        self.keys.reserve(key_bytes);
        self.own.reserve(own_bytes);
    }

    /// The cursor on row `i`.
    pub fn set_pos(&mut self, i: usize) {
        self.pos = i;
    }

    /// The cursor on past its row.
    pub fn advance(&mut self) {
        self.pos += 1;
    }

    /// The row at the cursor, owned, and the cursor past it (the value shares its buffer).
    pub fn take(&mut self) -> Option<(Vec<u8>, Bytes)> {
        let i = self.pos;
        let e = *self.ents.get(i)?;
        self.pos += 1;
        let k = self.keys[self.key_start(i)..e.key_end as usize].to_vec();
        let (o, l) = (e.off as usize, e.len as usize);
        let v = match e.buf {
            OWN => Bytes::copy_from_slice(&self.own[o..o + l]),
            b => self.bufs[b as usize].slice(o..o + l),
        };
        Some((k, v))
    }

    fn push_ent(&mut self, key: &[u8], buf: u32, off: usize, len: usize) {
        self.keys.extend_from_slice(key);
        self.ents.push(Ent { key_end: self.keys.len() as u32, buf, off: off as u32, len: len as u32 });
    }

    /// A row whose value is `buf[off..off + len]` (`buf` referenced once while the rows
    /// before share it).
    pub fn push_in(&mut self, key: &[u8], buf: &Bytes, off: usize, len: usize) {
        let same = self.bufs.last().is_some_and(|b| b.as_ptr() == buf.as_ptr() && b.len() == buf.len());
        if !same {
            self.bufs.push(buf.clone());
        }
        self.push_ent(key, self.bufs.len() as u32 - 1, off, len);
    }

    /// The index rows pushed next refer to `buf` by (`push_shared`): the last row's when it
    /// is the same buffer.
    pub fn buf_index(&mut self, buf: &Bytes) -> u32 {
        let same = self.bufs.last().is_some_and(|b| b.as_ptr() == buf.as_ptr() && b.len() == buf.len());
        if !same {
            self.bufs.push(buf.clone());
        }
        self.bufs.len() as u32 - 1
    }

    /// A row (not the first) whose key is the last row's first `shared` bytes and then
    /// `suffix`, its value `bufs[buf][off..off + len]` (`buf_index`).
    #[inline(always)]
    pub fn push_shared(&mut self, shared: usize, suffix: &[u8], buf: u32, off: usize, len: usize) {
        let prev = self.key_start(self.ents.len() - 1);
        self.keys.extend_from_within(prev..prev + shared);
        self.keys.extend_from_slice(suffix);
        self.ents.push(Ent { key_end: self.keys.len() as u32, buf, off: off as u32, len: len as u32 });
    }

    /// A row whose key is `key`'s first `n` bytes, its value `bufs[buf][off..off + len]`
    /// (`buf_index`): the key's bytes moved as one fixed-size copy, then cut back.
    #[inline(always)]
    pub fn push_key<const K: usize>(&mut self, key: &[u8; K], n: usize, buf: u32, off: usize, len: usize) {
        let start = self.keys.len();
        self.keys.extend_from_slice(key);
        self.keys.truncate(start + n);
        self.ents.push(Ent { key_end: (start + n) as u32, buf, off: off as u32, len: len as u32 });
    }

    /// A row whose value is `v`.
    pub fn push(&mut self, key: &[u8], v: Bytes) {
        let len = v.len();
        self.bufs.push(v);
        self.push_ent(key, self.bufs.len() as u32 - 1, 0, len);
    }

    /// A row whose value is copied from `v`.
    pub fn push_copy(&mut self, key: &[u8], v: &[u8]) {
        let off = self.own.len();
        self.own.extend_from_slice(v);
        self.push_ent(key, OWN, off, v.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_by_reference_and_owned() {
        let block = Bytes::from_static(b"aaaabbbbcc");
        let mut b = RowBatch::default();
        b.push_in(b"k1", &block, 0, 4);
        b.push_in(b"k2", &block, 4, 4);
        b.push(b"k3", Bytes::from_static(b"zz"));
        b.push_copy(b"k4", b"yy");
        assert_eq!(b.bufs.len(), 2, "one reference per block");
        assert_eq!(b.peek(), Some((&b"k1"[..], &b"aaaa"[..])));
        b.advance();
        assert_eq!(b.take(), Some((b"k2".to_vec(), Bytes::from_static(b"bbbb"))));
        assert_eq!(b.get(2), (&b"k3"[..], &b"zz"[..]));
        assert_eq!(b.get(3), (&b"k4"[..], &b"yy"[..]));
        assert_eq!(b.remaining(), 2);
        b.clear();
        assert!(b.peek().is_none() && b.is_empty());
    }
}
