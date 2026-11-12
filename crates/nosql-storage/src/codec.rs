//! Binary encoding of the data model, shared by commit log frames and SSTable blocks.
//!
//! Integers are LEB128 varints (signed ones zigzag-encoded). Timestamps in a row are deltas from
//! one base timestamp per row, so the common case (every cell written by the same statement)
//! costs one byte per cell. Decoding is zero-copy: values, paths and clusterings are slices of
//! the input `Bytes`. Every length and count read is checked against the bytes actually present,
//! so a corrupt or hostile input yields an error, never a panic or an oversized allocation.
//!
//! ```text
//! row      := flags:u8 [clustering] [base ts] [liveness] [deletion] ncols:uv column*
//! column   := id delta:uv (simple cell | complex [deletion] ncells:uv cell*)
//! cell     := flags:u8 [ts delta] [ttl:uv ldt] [ldt] [path:uv bytes] [value:uv bytes]
//! update   := token:i64be key:uv bytes flags:u8 [deletion] [static row] nrt:uv rt* nrows:uv row*
//! rt       := bound bound deletion          bound := after:u8 clustering
//! ```

use crate::model::*;
use anyhow::{Result, bail};
use bytes::{BufMut, Bytes};

pub fn put_uvarint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

#[inline]
pub fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
pub fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

pub fn put_varint(out: &mut Vec<u8>, v: i64) {
    put_uvarint(out, zigzag(v));
}

pub fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_uvarint(out, b.len() as u64);
    out.extend_from_slice(b);
}

/// A bounds-checked cursor over a byte slice.
#[derive(Clone)]
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    pub fn u8(&mut self) -> Result<u8> {
        let Some(&b) = self.data.get(self.pos) else { bail!("corrupt data: unexpected end") };
        self.pos += 1;
        Ok(b)
    }

    pub fn peek_u8(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            bail!("corrupt data: length {n} past the end");
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }

    pub fn uvarint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = self.u8()?;
            if shift == 63 && b > 1 {
                bail!("corrupt data: varint overflow");
            }
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
            if shift > 63 {
                bail!("corrupt data: varint overflow");
            }
        }
    }

    pub fn varint(&mut self) -> Result<i64> {
        Ok(unzigzag(self.uvarint()?))
    }

    /// A length that must fit in what's left.
    pub fn len(&mut self) -> Result<usize> {
        let n = self.uvarint()?;
        if n > self.remaining() as u64 {
            bail!("corrupt data: length {n} past the end");
        }
        Ok(n as usize)
    }

    /// A count of items of at least `min_size` bytes each, bounded by what's left.
    pub fn count(&mut self, min_size: usize) -> Result<usize> {
        let n = self.uvarint()?;
        if n.saturating_mul(min_size.max(1) as u64) > self.remaining() as u64 {
            bail!("corrupt data: count {n} past the end");
        }
        Ok(n as usize)
    }

    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.len()?;
        self.take(n)
    }

    pub fn i64_be(&mut self) -> Result<i64> {
        let s = self.take(8)?;
        Ok(i64::from_be_bytes(s.try_into().expect("8 bytes")))
    }

    pub fn u32_le(&mut self) -> Result<u32> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes(s.try_into().expect("4 bytes")))
    }

    pub fn u64_le(&mut self) -> Result<u64> {
        let s = self.take(8)?;
        Ok(u64::from_le_bytes(s.try_into().expect("8 bytes")))
    }
}

/// Zero-copy decoding: a [`Reader`] over a `Bytes` it can slice.
#[derive(Clone)]
pub struct BytesReader<'a> {
    src: &'a Bytes,
    pub r: Reader<'a>,
}

impl<'a> BytesReader<'a> {
    pub fn new(src: &'a Bytes) -> Self {
        Self { src, r: Reader::new(&src[..]) }
    }

    pub fn at(src: &'a Bytes, pos: usize) -> Result<Self> {
        if pos > src.len() {
            bail!("corrupt data: offset {pos} past the end");
        }
        let mut r = Reader::new(&src[..]);
        r.pos = pos;
        Ok(Self { src, r })
    }

    pub fn slice(&self, s: &[u8]) -> Bytes {
        if s.is_empty() { Bytes::new() } else { self.src.slice_ref(s) }
    }

    pub fn bytes(&mut self) -> Result<Bytes> {
        let s = self.r.bytes()?;
        Ok(self.slice(s))
    }

    pub fn clustering(&mut self) -> Result<Clustering> {
        let start = self.r.pos;
        let n = self.r.count(1)?;
        for _ in 0..n {
            let len = self.r.len()?;
            self.r.skip(len)?;
        }
        let s = &self.src[start..self.r.pos];
        Ok(Clustering::from_trusted(self.slice(s)))
    }

    pub fn bound(&mut self) -> Result<Bound> {
        let after = match self.r.u8()? {
            0 => false,
            1 => true,
            b => bail!("corrupt data: bound side {b}"),
        };
        Ok(Bound { prefix: self.clustering()?, after })
    }

    pub fn deletion(&mut self) -> Result<DeletionTime> {
        let marked_at = self.r.varint()?;
        let local_deletion_time = self.r.varint()?;
        Ok(DeletionTime { marked_at, local_deletion_time })
    }
}

pub fn put_clustering(out: &mut Vec<u8>, c: &Clustering) {
    out.extend_from_slice(c.encoded());
}

pub fn put_bound(out: &mut Vec<u8>, b: &Bound) {
    out.push(b.after as u8);
    put_clustering(out, &b.prefix);
}

pub fn put_deletion(out: &mut Vec<u8>, d: &DeletionTime) {
    put_varint(out, d.marked_at);
    put_varint(out, d.local_deletion_time);
}

const ROW_LIVENESS: u8 = 1;
const ROW_LIVENESS_TTL: u8 = 2;
const ROW_DELETION: u8 = 4;
const ROW_SHADOWABLE: u8 = 8;
const ROW_HAS_BASE: u8 = 16;

const CELL_TOMBSTONE: u8 = 1;
const CELL_EXPIRING: u8 = 2;
const CELL_IS_COUNTER: u8 = 4;
const CELL_HAS_PATH: u8 = 8;
const CELL_SAME_TS: u8 = 16;
const CELL_EMPTY: u8 = 32;

const COL_SIMPLE: u8 = 0;
const COL_COMPLEX: u8 = 1;
const COL_COMPLEX_DELETED: u8 = 2;

fn row_base(row: &Row) -> Option<i64> {
    if !row.liveness.is_empty() {
        return Some(row.liveness.timestamp);
    }
    if let Some(c) = row.cells().next() {
        return Some(c.timestamp);
    }
    if !row.deletion.is_live() {
        return Some(row.deletion.time.marked_at);
    }
    row.columns.iter().find_map(|c| match c {
        ColumnData::Complex { deletion, .. } if !deletion.is_live() => Some(deletion.marked_at),
        _ => None,
    })
}

/// Encode a row (`with_clustering = false` for static rows).
pub fn put_row(out: &mut Vec<u8>, row: &Row, with_clustering: bool) {
    let base = row_base(row);
    let mut flags = 0u8;
    if !row.liveness.is_empty() {
        flags |= ROW_LIVENESS;
        if row.liveness.is_expiring() {
            flags |= ROW_LIVENESS_TTL;
        }
    }
    if !row.deletion.is_live() {
        flags |= ROW_DELETION;
        if row.deletion.shadowable {
            flags |= ROW_SHADOWABLE;
        }
    }
    if base.is_some() {
        flags |= ROW_HAS_BASE;
    }
    out.push(flags);
    if with_clustering {
        put_clustering(out, &row.clustering);
    }
    let base = base.unwrap_or(0);
    if flags & ROW_HAS_BASE != 0 {
        put_varint(out, base);
    }
    if flags & ROW_LIVENESS != 0 {
        put_varint(out, row.liveness.timestamp.wrapping_sub(base));
        if flags & ROW_LIVENESS_TTL != 0 {
            put_uvarint(out, row.liveness.ttl as u32 as u64);
            put_varint(out, row.liveness.local_expiration_time);
        }
    }
    if flags & ROW_DELETION != 0 {
        put_varint(out, row.deletion.time.marked_at.wrapping_sub(base));
        put_varint(out, row.deletion.time.local_deletion_time);
    }
    put_uvarint(out, row.columns.len() as u64);
    let mut prev: i64 = -1;
    for c in &row.columns {
        let id = c.column() as i64;
        put_uvarint(out, (id - prev - 1).max(0) as u64);
        prev = id;
        match c {
            ColumnData::Simple { cell, .. } => {
                out.push(COL_SIMPLE);
                put_cell(out, cell, base);
            }
            ColumnData::Complex { deletion, cells, .. } => {
                if deletion.is_live() {
                    out.push(COL_COMPLEX);
                } else {
                    out.push(COL_COMPLEX_DELETED);
                    put_varint(out, deletion.marked_at.wrapping_sub(base));
                    put_varint(out, deletion.local_deletion_time);
                }
                put_uvarint(out, cells.len() as u64);
                for cell in cells {
                    put_cell(out, cell, base);
                }
            }
        }
    }
}

fn put_cell(out: &mut Vec<u8>, c: &Cell, base: i64) {
    let mut flags = 0u8;
    if c.is_tombstone() {
        flags |= CELL_TOMBSTONE;
    } else if c.is_expiring() {
        flags |= CELL_EXPIRING;
    }
    if c.is_counter() {
        flags |= CELL_IS_COUNTER;
    }
    if c.path.is_some() {
        flags |= CELL_HAS_PATH;
    }
    if c.timestamp == base {
        flags |= CELL_SAME_TS;
    }
    if c.value.is_empty() {
        flags |= CELL_EMPTY;
    }
    out.push(flags);
    if flags & CELL_SAME_TS == 0 {
        put_varint(out, c.timestamp.wrapping_sub(base));
    }
    if flags & CELL_EXPIRING != 0 {
        put_uvarint(out, c.ttl as u32 as u64);
        put_varint(out, c.local_deletion_time);
    } else if flags & CELL_TOMBSTONE != 0 {
        put_varint(out, c.local_deletion_time);
    }
    if let Some(p) = &c.path {
        put_bytes(out, p);
    }
    if flags & CELL_EMPTY == 0 {
        put_bytes(out, &c.value);
    }
}

impl BytesReader<'_> {
    pub fn row(&mut self, with_clustering: bool) -> Result<Row> {
        let flags = self.r.u8()?;
        if flags & !(ROW_LIVENESS | ROW_LIVENESS_TTL | ROW_DELETION | ROW_SHADOWABLE | ROW_HAS_BASE) != 0 {
            bail!("corrupt data: row flags {flags:#x}");
        }
        let clustering = if with_clustering { self.clustering()? } else { Clustering::EMPTY };
        let base = if flags & ROW_HAS_BASE != 0 { self.r.varint()? } else { 0 };
        let mut liveness = LivenessInfo::EMPTY;
        if flags & ROW_LIVENESS != 0 {
            liveness.timestamp = self.r.varint()?.wrapping_add(base);
            if flags & ROW_LIVENESS_TTL != 0 {
                liveness.ttl = self.r.uvarint()? as u32 as i32;
                liveness.local_expiration_time = self.r.varint()?;
                if liveness.ttl == NO_TTL {
                    bail!("corrupt data: expiring liveness without a ttl");
                }
            }
        }
        let mut deletion = RowDeletion::LIVE;
        if flags & ROW_DELETION != 0 {
            deletion.time.marked_at = self.r.varint()?.wrapping_add(base);
            deletion.time.local_deletion_time = self.r.varint()?;
            deletion.shadowable = flags & ROW_SHADOWABLE != 0;
        }
        let n = self.r.count(2)?;
        let mut columns = Vec::with_capacity(n);
        let mut prev: i64 = -1;
        for _ in 0..n {
            let delta = self.r.uvarint()?;
            let id = prev.checked_add(1).and_then(|p| p.checked_add(delta as i64)).filter(|id| *id <= u32::MAX as i64);
            let Some(id) = id else { bail!("corrupt data: column id overflow") };
            prev = id;
            let column = id as ColumnId;
            match self.r.u8()? {
                COL_SIMPLE => columns.push(ColumnData::Simple { column, cell: self.cell(base)? }),
                kind @ (COL_COMPLEX | COL_COMPLEX_DELETED) => {
                    let deletion = if kind == COL_COMPLEX_DELETED {
                        DeletionTime { marked_at: self.r.varint()?.wrapping_add(base), local_deletion_time: self.r.varint()? }
                    } else {
                        DeletionTime::LIVE
                    };
                    let m = self.r.count(1)?;
                    let mut cells = Vec::with_capacity(m);
                    for _ in 0..m {
                        cells.push(self.cell(base)?);
                    }
                    columns.push(ColumnData::Complex { column, deletion, cells });
                }
                k => bail!("corrupt data: column kind {k}"),
            }
        }
        Ok(Row { clustering, liveness, deletion, columns })
    }

    fn cell(&mut self, base: i64) -> Result<Cell> {
        let flags = self.r.u8()?;
        if flags & !(CELL_TOMBSTONE | CELL_EXPIRING | CELL_IS_COUNTER | CELL_HAS_PATH | CELL_SAME_TS | CELL_EMPTY) != 0
            || flags & (CELL_TOMBSTONE | CELL_EXPIRING) == (CELL_TOMBSTONE | CELL_EXPIRING)
        {
            bail!("corrupt data: cell flags {flags:#x}");
        }
        let timestamp = if flags & CELL_SAME_TS != 0 { base } else { self.r.varint()?.wrapping_add(base) };
        let (mut ttl, mut local_deletion_time) = (NO_TTL, NO_DELETION_TIME);
        if flags & CELL_EXPIRING != 0 {
            ttl = self.r.uvarint()? as u32 as i32;
            local_deletion_time = self.r.varint()?;
            if ttl == NO_TTL || local_deletion_time == NO_DELETION_TIME {
                bail!("corrupt data: expiring cell without a ttl");
            }
        } else if flags & CELL_TOMBSTONE != 0 {
            local_deletion_time = self.r.varint()?;
            if local_deletion_time == NO_DELETION_TIME {
                bail!("corrupt data: tombstone without a deletion time");
            }
        }
        let path = if flags & CELL_HAS_PATH != 0 { Some(self.bytes()?) } else { None };
        let value = if flags & CELL_EMPTY != 0 { Bytes::new() } else { self.bytes()? };
        let flags = if flags & CELL_IS_COUNTER != 0 { CELL_COUNTER } else { 0 };
        Ok(Cell { timestamp, ttl, flags, local_deletion_time, value, path })
    }

    pub fn range_tombstone(&mut self) -> Result<RangeTombstone> {
        let start = self.bound()?;
        let end = self.bound()?;
        let deletion = self.deletion()?;
        Ok(RangeTombstone { start, end, deletion })
    }

    pub fn update(&mut self) -> Result<PartitionUpdate> {
        let token = self.r.i64_be()?;
        let key = self.bytes()?;
        let flags = self.r.u8()?;
        if flags & !3 != 0 {
            bail!("corrupt data: update flags {flags:#x}");
        }
        let deletion = if flags & 1 != 0 { self.deletion()? } else { DeletionTime::LIVE };
        let static_row = if flags & 2 != 0 { Some(self.row(false)?) } else { None };
        let n = self.r.count(6)?;
        let mut range_tombstones = Vec::with_capacity(n);
        for _ in 0..n {
            range_tombstones.push(self.range_tombstone()?);
        }
        let n = self.r.count(3)?;
        let mut rows = Vec::with_capacity(n);
        for _ in 0..n {
            rows.push(self.row(true)?);
        }
        Ok(PartitionUpdate { key: PartitionKey { token, key }, deletion, static_row, rows, range_tombstones })
    }
}

pub fn put_range_tombstone(out: &mut Vec<u8>, rt: &RangeTombstone) {
    put_bound(out, &rt.start);
    put_bound(out, &rt.end);
    put_deletion(out, &rt.deletion);
}

pub fn put_update(out: &mut Vec<u8>, u: &PartitionUpdate) {
    out.put_i64(u.key.token);
    put_bytes(out, &u.key.key);
    let flags = (!u.deletion.is_live()) as u8 | ((u.static_row.is_some() as u8) << 1);
    out.push(flags);
    if !u.deletion.is_live() {
        put_deletion(out, &u.deletion);
    }
    if let Some(s) = &u.static_row {
        put_row(out, s, false);
    }
    put_uvarint(out, u.range_tombstones.len() as u64);
    for rt in &u.range_tombstones {
        put_range_tombstone(out, rt);
    }
    put_uvarint(out, u.rows.len() as u64);
    for r in &u.rows {
        put_row(out, r, true);
    }
}

/// An update encoded into an allocation of exactly its size: memtable cells reference these
/// bytes for as long as they live, so growth slack would be pinned with them.
pub fn encode_update(u: &PartitionUpdate) -> Bytes {
    thread_local! {
        static SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    SCRATCH.with(|s| {
        let mut s = s.borrow_mut();
        s.clear();
        put_update(&mut s, u);
        let out = Bytes::copy_from_slice(&s);
        if s.capacity() > 1 << 20 {
            *s = Vec::new();
        }
        out
    })
}

pub fn decode_update(src: &Bytes) -> Result<PartitionUpdate> {
    let mut r = BytesReader::new(src);
    let u = r.update()?;
    if !r.r.is_empty() {
        bail!("corrupt data: trailing bytes after an update");
    }
    Ok(u)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample_update() -> PartitionUpdate {
        let ck = |s: &str| Clustering::new([s.as_bytes()]);
        PartitionUpdate::new(PartitionKey::new(-42, Bytes::from_static(b"pk")))
            .with_deletion(DeletionTime::new(5, 100))
            .with_static(Row::new(Clustering::EMPTY).with_cell(0, Cell::live(9, Bytes::from_static(b"s"))))
            .with_range_tombstone(RangeTombstone::new(Bound::inclusive_start(ck("a")), Bound::exclusive_end(ck("c")), DeletionTime::new(7, 101)))
            .with_row(
                Row::new(ck("b"))
                    .with_liveness(LivenessInfo::expiring(10, 60, 160))
                    .with_deletion(RowDeletion::shadowable(DeletionTime::new(8, 99)))
                    .with_cell(1, Cell::live(10, Bytes::from_static(b"v1")))
                    .with_cell(3, Cell::expiring(11, 5, 105, Bytes::from_static(b"")))
                    .with_cell(4, Cell::tombstone(i64::MIN + 1, 99))
                    .with_column(ColumnData::complex(
                        7,
                        DeletionTime::new(9, 98),
                        vec![Cell::live(10, Bytes::from_static(b"e")).with_path(Bytes::from_static(b"k1")), Cell::tombstone(12, 97).with_path(Bytes::new())],
                    ))
                    .with_cell(9, Cell::counter(10, 3, 77)),
            )
            .with_row(Row::new(ck("z")).with_cell(u32::MAX, Cell::live(i64::MAX, Bytes::from_static(b"x"))))
    }

    #[test]
    fn varints() {
        for v in [0i64, 1, -1, 63, -64, i64::MAX, i64::MIN, 1 << 40] {
            let mut out = Vec::new();
            put_varint(&mut out, v);
            assert_eq!(Reader::new(&out).varint().unwrap(), v);
        }
        assert!(Reader::new(&[0xff; 11]).uvarint().is_err());
        assert!(Reader::new(&[0x80]).uvarint().is_err());
    }

    #[test]
    fn update_round_trip() {
        let u = sample_update();
        let mut out = Vec::new();
        put_update(&mut out, &u);
        let b = Bytes::from(out);
        assert_eq!(decode_update(&b).unwrap(), u);
    }

    #[test]
    fn truncated_and_corrupt_updates_fail_cleanly() {
        let u = sample_update();
        let mut out = Vec::new();
        put_update(&mut out, &u);
        for n in 0..out.len() {
            let b = Bytes::copy_from_slice(&out[..n]);
            assert!(decode_update(&b).is_err(), "prefix of {n} bytes decoded");
        }
        // Flip every byte: must never panic.
        for i in 0..out.len() {
            let mut c = out.clone();
            c[i] ^= 0xff;
            let _ = decode_update(&Bytes::from(c));
        }
        // A huge count must not allocate.
        let mut bad = Vec::new();
        bad.put_i64(1);
        put_bytes(&mut bad, b"k");
        bad.push(0);
        put_uvarint(&mut bad, u64::MAX);
        assert!(decode_update(&Bytes::from(bad)).is_err());
    }
}
