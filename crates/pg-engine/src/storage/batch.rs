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
    /// Rows of column blocks (format 5, `sst::Block::cols`), not made: each refers to an empty
    /// buffer of `bufs`, so every accessor's bounds check fails for it and only then looks
    /// here (`lazy_row`); rows of other blocks pay nothing for them. The column blocks by
    /// their buffers' index; the runs of rows they hold (`col_runs`, for readers of
    /// columns: `cols::decode_cols`); by row of the batch (up to the last such row), its block
    /// row and its bytes once a reader wanted them (made alone).
    lazy: Vec<(u32, Arc<super::sst::Block>)>,
    cruns: Vec<ColRun>,
    lrows: Vec<(u32, OnceLock<Bytes>)>,
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
        if !self.lazy.is_empty() {
            self.lazy.clear();
            self.cruns.clear();
            self.lrows.clear();
        }
    }

    /// Row `i` of a column block: its bytes, made when first wanted.
    #[cold]
    #[inline(never)]
    fn lazy_row(&self, i: usize) -> &Bytes {
        let e = self.ents[i];
        let (_, blk) = self.lazy.iter().rev().find(|(b, _)| *b == e.buf).expect("a row of a column block");
        let (row, cell) = &self.lrows[i];
        cell.get_or_init(|| blk.row_at(*row as usize, e.len as usize))
    }

    /// The rows from column blocks, in order.
    pub fn col_runs(&self) -> &[ColRun] {
        &self.cruns
    }

    /// The column block of buffer `b`.
    pub fn col_block(&self, b: u32) -> Option<&Arc<super::sst::Block>> {
        self.lazy.iter().rev().find(|(x, _)| *x == b).map(|(_, blk)| blk)
    }

    /// The index rows of column block `blk` pushed next refer to (`push_col`, `note_rows`).
    pub fn buf_block(&mut self, blk: &Arc<super::sst::Block>) -> u32 {
        let last = self.bufs.len() as u32;
        if let Some((b, x)) = self.lazy.last()
            && *b + 1 == last
            && Arc::ptr_eq(x, blk)
        {
            return *b;
        }
        self.bufs.push(Bytes::new());
        self.lazy.push((last, blk.clone()));
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
        self.lrows.resize_with(at as usize, || (u32::MAX, OnceLock::new()));
        self.lrows.extend((0..n as u32).map(|j| (row + j, OnceLock::new())));
        if let Some(r) = self.cruns.last_mut()
            && r.buf == buf
            && r.at + r.n == at
            && r.row + r.n == row
        {
            r.n += n as u32;
            return;
        }
        self.cruns.push(ColRun { at, n: n as u32, buf, row });
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

    /// Row `i`'s key and value.
    pub fn get(&self, i: usize) -> (&[u8], &[u8]) {
        let e = self.ents[i];
        let k = &self.keys[self.key_start(i)..e.key_end as usize];
        let (o, l) = (e.off as usize, e.len as usize);
        let v = match e.buf {
            OWN => &self.own[o..o + l],
            b => match self.bufs[b as usize].get(o..o + l) {
                Some(v) => v,
                None => self.lazy_row(i),
            },
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
            b => match &self.bufs[b as usize] {
                buf if o + l <= buf.len() => buf.slice(o..o + l),
                _ => self.lazy_row(i).clone(),
            },
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
