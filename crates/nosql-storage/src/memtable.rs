//! Memtables: the in-memory, writable part of a table's LSM tree.
//!
//! Partitions are sharded by token range (contiguous, so a scan visits shards in order). Each
//! shard map is behind its own lock, taken only to find or insert a partition; each partition
//! has its own lock, held while one update is merged in or while a reader copies a batch of rows
//! out. No lock is held across an await, or for longer than one batch.
//!
//! Rows are kept in clustering order in short sorted chunks (inserts cost O(log n + chunk),
//! appends at the end are cheap). Range tombstones are kept disjoint ([`tombstones::insert`]).
//! Updates are merged on write (`Row.Merger` semantics), so a memtable holds one version per row.
//!
//! A memtable is frozen before it's flushed: writers check the flag after registering
//! themselves, the freezer waits for registered writers to leave (a Dekker-style handshake on
//! two atomics), so every write lands either in the frozen memtable before the flush reads it,
//! or in its successor.

use crate::clip::{Clipper, Item, Slices};
use crate::compare::{ClusteringComparator, Pos, compare_clustering, compare_clustering_bound, compare_pos};
use crate::memory::DbMemory;
use crate::model::*;
use crate::reconcile::{Reconcile, merge_rows};
use crate::sstable::Header;
use crate::sstable::cursor::KeyBound;
use crate::tombstones::{self, Marker, dir};
use parking_lot::RwLock;
use std::cmp::Ordering;
use std::collections::{BTreeMap, VecDeque};
use std::ops::Bound as RangeBound;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering as AO};

const SHARD_BITS: u32 = 4;
const CHUNK: usize = 64;
/// Rows copied out per partition lock acquisition.
const BATCH: usize = 128;

/// Rows in clustering order, in chunks of at most `2 * CHUNK`.
#[derive(Default)]
pub(crate) struct ChunkedRows {
    chunks: Vec<Vec<Row>>,
    len: usize,
}

impl ChunkedRows {
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Number of rows for which `before` holds (it must be monotone over the order).
    fn partition_point(&self, before: impl Fn(&Row) -> bool) -> usize {
        let ci = self.chunks.partition_point(|c| c.last().is_some_and(&before));
        let base: usize = self.chunks[..ci].iter().map(|c| c.len()).sum();
        base + self.chunks.get(ci).map_or(0, |c| c.partition_point(&before))
    }

    fn locate(&self, mut i: usize) -> Option<(usize, usize)> {
        for (ci, c) in self.chunks.iter().enumerate() {
            if i < c.len() {
                return Some((ci, i));
            }
            i -= c.len();
        }
        None
    }

    /// Rows from index `i` on.
    fn iter_from(&self, i: usize) -> impl Iterator<Item = &Row> {
        let (ci, ri) = self.locate(i).unwrap_or((self.chunks.len(), 0));
        self.chunks.get(ci).map(|c| &c[ri..]).unwrap_or(&[]).iter().chain(self.chunks.iter().skip(ci + 1).flatten())
    }

    /// Rows before index `i`, last first.
    fn iter_rev_before(&self, i: usize) -> impl Iterator<Item = &Row> {
        let (ci, ri) = match i.checked_sub(1).and_then(|j| self.locate(j)) {
            Some((ci, ri)) => (ci, ri + 1),
            None => (0, 0),
        };
        let head = self.chunks.get(ci).map(|c| &c[..ri]).unwrap_or(&[]);
        head.iter().rev().chain(self.chunks[..ci.min(self.chunks.len())].iter().rev().flat_map(|c| c.iter().rev()))
    }

    /// Insert `row`, or merge it into the row with the same clustering. Returns whether a row
    /// was added.
    fn upsert(&mut self, cmp: &dyn ClusteringComparator, row: Row, merge: impl FnOnce(Row, Row) -> Option<Row>) -> bool {
        if self.chunks.is_empty() {
            self.chunks.push(vec![row]);
            self.len = 1;
            return true;
        }
        // Fast path: appending at the end.
        let last_chunk = self.chunks.len() - 1;
        let ci = match self.chunks[last_chunk].last().map(|r| compare_clustering(cmp, &r.clustering, &row.clustering)) {
            Some(Ordering::Less) => last_chunk,
            _ => {
                let ci = self.chunks.partition_point(|c| c.last().is_some_and(|r| compare_clustering(cmp, &r.clustering, &row.clustering) == Ordering::Less));
                ci.min(last_chunk)
            }
        };
        let chunk = &mut self.chunks[ci];
        match chunk.binary_search_by(|r| compare_clustering(cmp, &r.clustering, &row.clustering)) {
            Ok(i) => {
                let old = std::mem::take(&mut chunk[i]);
                match merge(old, row) {
                    Some(m) => chunk[i] = m,
                    None => {
                        chunk.remove(i);
                        self.len -= 1;
                        if chunk.is_empty() {
                            self.chunks.remove(ci);
                        }
                    }
                }
                false
            }
            Err(i) => {
                chunk.insert(i, row);
                self.len += 1;
                if chunk.len() > 2 * CHUNK {
                    let tail = chunk.split_off(CHUNK);
                    self.chunks.insert(ci + 1, tail);
                }
                true
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct PartData {
    pub(crate) deletion: DeletionTime,
    pub(crate) static_row: Option<Row>,
    /// Disjoint, in clustering order.
    pub(crate) rts: Vec<RangeTombstone>,
    pub(crate) rows: ChunkedRows,
}

pub struct MemPartition {
    pub key: PartitionKey,
    pub(crate) data: RwLock<PartData>,
}

impl MemPartition {
    pub fn header(&self) -> Header {
        let d = self.data.read();
        Header { key: self.key.clone(), deletion: d.deletion, static_row: d.static_row.clone() }
    }

    pub fn rows(&self) -> usize {
        self.data.read().rows.len()
    }

    /// The oldest live data of the partition (ScyllaDB's memtable `min_live_timestamp`) and the
    /// oldest live row marker (`min_live_row_marker_timestamp`), `i64::MAX` when there is none:
    /// what a tombstone purged by compaction could have shadowed. Tombstones don't count, a
    /// backdated deletion doesn't hold back purging.
    pub fn min_live_timestamps(&self, now: i64) -> (i64, i64) {
        let d = self.data.read();
        let (mut live, mut marker) = (i64::MAX, i64::MAX);
        let mut row = |r: &Row| {
            if r.liveness.is_live(now) {
                live = live.min(r.liveness.timestamp);
                marker = marker.min(r.liveness.timestamp);
            }
            for c in &r.columns {
                let cells: &[Cell] = match c {
                    ColumnData::Simple { cell, .. } => std::slice::from_ref(cell),
                    ColumnData::Complex { cells, .. } => cells,
                };
                for cell in cells.iter().filter(|c| c.is_live(now)) {
                    live = live.min(cell.timestamp);
                }
            }
        };
        if let Some(st) = &d.static_row {
            row(st);
        }
        for r in d.rows.iter_from(0) {
            row(r);
        }
        (live, marker)
    }
}

type Shard = RwLock<BTreeMap<PartitionKey, Arc<MemPartition>>>;

pub struct Memtable {
    cmp: Arc<dyn ClusteringComparator>,
    mode: Reconcile,
    shards: Box<[Shard]>,
    bytes: AtomicU64,
    partitions: AtomicU64,
    rows: AtomicU64,
    min_ts: AtomicI64,
    max_ts: AtomicI64,
    min_lsn: AtomicU64,
    max_lsn: AtomicU64,
    writers: AtomicUsize,
    frozen: AtomicBool,
    pub(crate) created: std::time::Instant,
    /// The database's memtable memory, charged per write and given back on drop.
    account: Option<Arc<DbMemory>>,
}

/// Heap a partition costs a memtable beyond its rows: the shared partition, its map entry, its
/// row chunk list.
const PARTITION_OVERHEAD: u64 = (std::mem::size_of::<MemPartition>() + 16 + 80 + 48) as u64;

/// Approximate heap cost of an update in a memtable: its data (the encoded frame its cells
/// reference, with the frame's shared header) and the structures holding its rows — a row slot
/// in a chunk with growth slack, column and cell slots. `memory_accounting_covers_the_heap`
/// (tests/memory.rs) checks it against a counting allocator.
fn update_size(u: &PartitionUpdate) -> u64 {
    const ROW: u64 = 2 * std::mem::size_of::<Row>() as u64;
    const COLUMN: u64 = std::mem::size_of::<ColumnData>() as u64;
    const CELL: u64 = std::mem::size_of::<Cell>() as u64;
    fn cell_data(c: &Cell) -> u64 {
        12 + c.value.len() as u64 + c.path.as_ref().map_or(0, |p| p.len() as u64)
    }
    fn row_size(r: &Row) -> u64 {
        let mut s = ROW + 12 + r.clustering.encoded().len() as u64;
        for c in &r.columns {
            s += COLUMN
                + match c {
                    ColumnData::Simple { cell, .. } => cell_data(cell),
                    ColumnData::Complex { cells, .. } => cells.iter().map(|c| CELL + cell_data(c)).sum::<u64>(),
                };
        }
        s
    }
    let mut s = 48 + u.key.key.len() as u64;
    s += u.static_row.as_ref().map_or(0, row_size);
    s += u.rows.iter().map(row_size).sum::<u64>();
    s += u.range_tombstones.iter().map(|rt| 2 * std::mem::size_of::<RangeTombstone>() as u64 + rt.start.prefix.encoded().len() as u64 + rt.end.prefix.encoded().len() as u64).sum::<u64>();
    s
}

impl Memtable {
    pub fn new(cmp: Arc<dyn ClusteringComparator>, mode: Reconcile) -> Self {
        Self {
            cmp,
            mode,
            shards: (0..1 << SHARD_BITS).map(|_| RwLock::new(BTreeMap::new())).collect(),
            bytes: AtomicU64::new(0),
            partitions: AtomicU64::new(0),
            rows: AtomicU64::new(0),
            min_ts: AtomicI64::new(i64::MAX),
            max_ts: AtomicI64::new(i64::MIN),
            min_lsn: AtomicU64::new(u64::MAX),
            max_lsn: AtomicU64::new(0),
            writers: AtomicUsize::new(0),
            frozen: AtomicBool::new(false),
            created: std::time::Instant::now(),
            account: None,
        }
    }

    /// A memtable whose bytes count against `account` (`None`: unaccounted scratch memtables).
    pub fn accounted(cmp: Arc<dyn ClusteringComparator>, mode: Reconcile, account: Option<Arc<DbMemory>>) -> Self {
        let mut m = Self::new(cmp, mode);
        m.account = account;
        m
    }

    pub fn comparator(&self) -> &Arc<dyn ClusteringComparator> {
        &self.cmp
    }

    fn shard_of(token: i64) -> usize {
        (((token as u64) ^ (1 << 63)) >> (64 - SHARD_BITS)) as usize
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(AO::Relaxed)
    }

    pub fn partitions(&self) -> u64 {
        self.partitions.load(AO::Relaxed)
    }

    pub fn rows(&self) -> u64 {
        self.rows.load(AO::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.partitions() == 0
    }

    /// Smallest write timestamp (`i64::MAX` when empty).
    pub fn min_timestamp(&self) -> i64 {
        self.min_ts.load(AO::Relaxed)
    }

    pub fn max_timestamp(&self) -> i64 {
        self.max_ts.load(AO::Relaxed)
    }

    /// Smallest commit log position applied (`u64::MAX` when none).
    pub fn min_lsn(&self) -> u64 {
        self.min_lsn.load(AO::SeqCst)
    }

    pub fn max_lsn(&self) -> u64 {
        self.max_lsn.load(AO::SeqCst)
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen.load(AO::SeqCst)
    }

    /// Stop accepting writes and wait for the ones in progress.
    pub fn freeze(&self) {
        self.frozen.store(true, AO::SeqCst);
        let mut spins = 0u32;
        while self.writers.load(AO::SeqCst) != 0 {
            spins += 1;
            if spins < 64 {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
    }

    /// Merge an update in. `lsn` is its commit log position (0 when not logged). A frozen
    /// memtable hands the update back.
    #[allow(clippy::result_large_err)]
    pub fn apply(&self, mut u: PartitionUpdate, lsn: u64) -> Result<(), PartitionUpdate> {
        self.writers.fetch_add(1, AO::SeqCst);
        if self.frozen.load(AO::SeqCst) {
            self.writers.fetch_sub(1, AO::SeqCst);
            return Err(u);
        }
        if lsn != 0 {
            self.min_lsn.fetch_min(lsn, AO::SeqCst);
            self.max_lsn.fetch_max(lsn, AO::SeqCst);
        }
        let mut size = update_size(&u);
        let ts = u.max_timestamp();
        self.max_ts.fetch_max(ts, AO::Relaxed);
        self.min_ts.fetch_min(min_timestamp(&u), AO::Relaxed);
        let part = {
            let shard = &self.shards[Self::shard_of(u.key.token)];
            let found = shard.read().get(&u.key).cloned();
            match found {
                Some(p) => p,
                None => {
                    let mut w = shard.write();
                    w.entry(u.key.clone())
                        .or_insert_with(|| {
                            self.partitions.fetch_add(1, AO::Relaxed);
                            size += PARTITION_OVERHEAD;
                            Arc::new(MemPartition { key: u.key.clone(), data: RwLock::new(PartData::default()) })
                        })
                        .clone()
                }
            }
        };
        let cmp = &*self.cmp;
        let mode = self.mode;
        let mut added = 0;
        {
            let mut d = part.data.write();
            d.deletion = d.deletion.max(u.deletion);
            if let Some(mut s) = u.static_row.take() {
                s.clustering = Clustering::EMPTY;
                d.static_row = match d.static_row.take() {
                    None => Some(s),
                    Some(old) => merge_rows(cmp, mode, vec![old, s], DeletionTime::LIVE),
                };
            }
            for rt in u.range_tombstones.drain(..) {
                tombstones::insert(cmp, &mut d.rts, rt);
            }
            for r in u.rows.drain(..) {
                if d.rows.upsert(cmp, r, |old, new| merge_rows(cmp, mode, vec![old, new], DeletionTime::LIVE)) {
                    added += 1;
                }
            }
        }
        self.rows.fetch_add(added, AO::Relaxed);
        self.bytes.fetch_add(size, AO::Relaxed);
        if let Some(a) = &self.account {
            a.charge(size);
        }
        self.writers.fetch_sub(1, AO::SeqCst);
        Ok(())
    }

    pub fn partition(&self, key: &PartitionKey) -> Option<Arc<MemPartition>> {
        self.shards[Self::shard_of(key.token)].read().get(key).cloned()
    }

    /// Up to `limit` partitions in `(start, end)` order, after `after` if given.
    pub fn range(&self, start: &KeyBound, end: &KeyBound, after: Option<&PartitionKey>, limit: usize) -> Vec<Arc<MemPartition>> {
        let mut out = Vec::new();
        let lo = match after {
            Some(k) => RangeBound::Excluded(k.clone()),
            None => match start.search_key() {
                Some(k) => RangeBound::Included(k),
                None => RangeBound::Unbounded,
            },
        };
        let first_shard = match &lo {
            RangeBound::Included(k) | RangeBound::Excluded(k) => Self::shard_of(k.token),
            RangeBound::Unbounded => 0,
        };
        for shard in &self.shards[first_shard..] {
            let map = shard.read();
            for (k, p) in map.range((lo.clone(), RangeBound::Unbounded)) {
                if !end.admits_to(k) {
                    return out;
                }
                if !start.admits_from(k) {
                    continue;
                }
                out.push(p.clone());
                if out.len() >= limit {
                    return out;
                }
            }
        }
        out
    }

    /// Every partition in order (for flushing a frozen memtable).
    pub fn all(&self) -> Vec<Arc<MemPartition>> {
        let mut out = Vec::with_capacity(self.partitions() as usize);
        for shard in self.shards.iter() {
            out.extend(shard.read().values().cloned());
        }
        out
    }
}

impl Drop for Memtable {
    fn drop(&mut self) {
        if let Some(a) = &self.account {
            a.release(self.bytes());
        }
    }
}

fn min_timestamp(u: &PartitionUpdate) -> i64 {
    let mut ts = if u.deletion.is_live() { i64::MAX } else { u.deletion.marked_at };
    let row_min = |r: &Row| {
        let mut m = i64::MAX;
        if !r.liveness.is_empty() {
            m = m.min(r.liveness.timestamp);
        }
        if !r.deletion.is_live() {
            m = m.min(r.deletion.time.marked_at);
        }
        for c in &r.columns {
            match c {
                ColumnData::Simple { cell, .. } => m = m.min(cell.timestamp),
                ColumnData::Complex { deletion, cells, .. } => {
                    if !deletion.is_live() {
                        m = m.min(deletion.marked_at);
                    }
                    for c in cells {
                        m = m.min(c.timestamp);
                    }
                }
            }
        }
        m
    };
    if let Some(s) = &u.static_row {
        ts = ts.min(row_min(s));
    }
    for r in &u.rows {
        ts = ts.min(row_min(r));
    }
    for rt in &u.range_tombstones {
        ts = ts.min(rt.deletion.marked_at);
    }
    ts
}

/// One memtable partition's items through a [`Clipper`], copied out in batches.
pub struct MemCursor {
    part: Arc<MemPartition>,
    cmp: Arc<dyn ClusteringComparator>,
    clip: Clipper,
    markers: VecDeque<Marker>,
    rows: VecDeque<Row>,
    last: Option<Clustering>,
    rows_done: bool,
    finished: bool,
}

impl MemCursor {
    pub fn new(part: Arc<MemPartition>, cmp: Arc<dyn ClusteringComparator>, slices: Slices) -> Self {
        let rev = slices.reversed;
        let markers = {
            let d = part.data.read();
            let mut m = tombstones::to_markers(&*cmp, &d.rts);
            if rev {
                m.reverse();
                m = m.into_iter().map(Marker::reversed).collect();
            }
            m.into()
        };
        Self { part, cmp, clip: Clipper::new(slices), markers, rows: VecDeque::new(), last: None, rows_done: false, finished: false }
    }

    pub fn header(&self) -> Header {
        self.part.header()
    }

    fn refill(&mut self) {
        let cmp = &*self.cmp;
        let rev = self.clip.slices.reversed;
        let d = self.part.data.read();
        let rows = &d.rows;
        // Where to continue: after the last row, or at the slice being looked for.
        let target = self.clip.seek_target().cloned();
        let p = match (&self.last, &target) {
            (Some(last), None) => {
                if rev {
                    rows.partition_point(|r| compare_clustering(cmp, &r.clustering, last) == Ordering::Less)
                } else {
                    rows.partition_point(|r| compare_clustering(cmp, &r.clustering, last) != Ordering::Greater)
                }
            }
            (last, Some(t)) => {
                let pt = rows.partition_point(|r| compare_clustering_bound(cmp, &r.clustering, t) == Ordering::Less);
                match last {
                    None => pt,
                    Some(last) => {
                        let pl = if rev {
                            rows.partition_point(|r| compare_clustering(cmp, &r.clustering, last) == Ordering::Less)
                        } else {
                            rows.partition_point(|r| compare_clustering(cmp, &r.clustering, last) != Ordering::Greater)
                        };
                        if rev { pt.min(pl) } else { pt.max(pl) }
                    }
                }
            }
            (None, None) => {
                if rev {
                    rows.len()
                } else {
                    0
                }
            }
        };
        let left = if rev { p } else { rows.len() - p.min(rows.len()) };
        if rev {
            self.rows.extend(rows.iter_rev_before(p).take(BATCH).cloned());
        } else {
            self.rows.extend(rows.iter_from(p).take(BATCH).cloned());
        }
        if left <= BATCH {
            self.rows_done = true;
        }
        if let Some(r) = self.rows.back() {
            self.last = Some(r.clustering.clone());
        }
    }

    /// The partition's next item, or `None` at its end.
    pub fn next_item(&mut self) -> Option<Item> {
        let rev = self.clip.slices.reversed;
        loop {
            if let Some(i) = self.clip.pop() {
                return Some(i);
            }
            if self.finished || self.clip.done() {
                return None;
            }
            if self.rows.is_empty() && !self.rows_done {
                self.refill();
            }
            let cmp = &*self.cmp;
            let take_marker = match (self.markers.front(), self.rows.front()) {
                (None, None) => {
                    self.clip.finish();
                    self.finished = true;
                    continue;
                }
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (Some(m), Some(r)) => dir(compare_pos(cmp, Pos::Bound(&m.bound), Pos::Row(&r.clustering)), rev) == Ordering::Less,
            };
            let item = if take_marker {
                Item::Marker(self.markers.pop_front().expect("front"))
            } else {
                Item::Row(self.rows.pop_front().expect("front"))
            };
            self.clip.push(cmp, item);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::BytesComparator;
    use bytes::Bytes;

    fn ck(i: u32) -> Clustering {
        Clustering::new([i.to_be_bytes()])
    }

    fn mt() -> Memtable {
        Memtable::new(Arc::new(BytesComparator), Reconcile::Cassandra)
    }

    fn key(t: i64) -> PartitionKey {
        PartitionKey::new(t, Bytes::from(t.to_be_bytes().to_vec()))
    }

    fn rows_of(m: &Memtable, k: &PartitionKey, slices: Slices) -> Vec<Item> {
        let p = m.partition(k).unwrap();
        let mut c = MemCursor::new(p, Arc::new(BytesComparator), slices);
        std::iter::from_fn(|| c.next_item()).collect()
    }

    #[test]
    fn upsert_keeps_order_and_merges() {
        let m = mt();
        let k = key(1);
        // Insert in a scrambled order, enough rows to split chunks.
        for i in (0..1000u32).map(|i| (i * 7919) % 1000) {
            let u = PartitionUpdate::new(k.clone()).with_row(Row::new(ck(i)).with_cell(1, Cell::live(i as i64, Bytes::from_static(b"a"))));
            m.apply(u, 1).unwrap();
        }
        // Overwrite one with a newer cell.
        m.apply(PartitionUpdate::new(k.clone()).with_row(Row::new(ck(5)).with_cell(1, Cell::live(1000, Bytes::from_static(b"b")))), 2).unwrap();
        assert_eq!(m.rows(), 1000);
        let items = rows_of(&m, &k, Slices::all(false));
        assert_eq!(items.len(), 1000);
        let cs: Vec<Clustering> = items.iter().map(|i| match i {
            Item::Row(r) => r.clustering.clone(),
            _ => unreachable!(),
        }).collect();
        assert!(cs.windows(2).all(|w| w[0].encoded() < w[1].encoded()));
        let Item::Row(r5) = &items[5] else { unreachable!() };
        assert_eq!(r5.cell(1).unwrap().value, "b");
        let rev = rows_of(&m, &k, Slices::all(true));
        assert_eq!(rev.len(), 1000);
        assert_eq!(rev[0], items[999]);
        assert_eq!((m.min_lsn(), m.max_lsn()), (1, 2));
        assert_eq!((m.min_timestamp(), m.max_timestamp()), (0, 1000));
    }

    #[test]
    fn slices_and_range_tombstones() {
        let cmp = BytesComparator;
        let m = mt();
        let k = key(1);
        let mut u = PartitionUpdate::new(k.clone());
        for i in 0..20 {
            u.rows.push(Row::new(ck(i)).with_liveness(LivenessInfo::live(1)));
        }
        u.range_tombstones.push(RangeTombstone::new(Bound::inclusive_start(ck(5)), Bound::exclusive_end(ck(8)), DeletionTime::new(2, 1)));
        m.apply(u, 1).unwrap();
        let slices = Slices::normalize(&cmp, vec![Slice::new(Bound::inclusive_start(ck(6)), Bound::inclusive_end(ck(9)))], false);
        let items = rows_of(&m, &k, slices);
        assert_eq!(items.len(), 6, "open, 6, 7, close, 8, 9: {items:?}");
        assert!(matches!(&items[0], Item::Marker(mk) if mk.open == Some(DeletionTime::new(2, 1)) && mk.bound == Bound::inclusive_start(ck(6))));
        assert!(matches!(&items[3], Item::Marker(mk) if mk.close.is_some()));
        let slices = Slices::normalize(&cmp, vec![Slice::new(Bound::inclusive_start(ck(6)), Bound::inclusive_end(ck(9)))], true);
        let items = rows_of(&m, &k, slices);
        assert_eq!(items.len(), 6, "{items:?}");
        assert!(matches!(&items[0], Item::Row(r) if r.clustering == ck(9)));
        assert!(matches!(&items[2], Item::Marker(mk) if mk.open.is_some() && mk.bound == Bound::exclusive_end(ck(8))));
    }

    #[test]
    fn freeze_rejects_writes() {
        let m = mt();
        m.freeze();
        assert!(m.apply(PartitionUpdate::new(key(1)), 1).is_err());
    }

    #[test]
    fn range_scans_in_token_order() {
        let m = mt();
        for t in [i64::MIN, -5, 0, 7, i64::MAX] {
            m.apply(PartitionUpdate::new(key(t)).with_deletion(DeletionTime::new(1, 1)), 1).unwrap();
        }
        let all: Vec<i64> = m.range(&KeyBound::Unbounded, &KeyBound::Unbounded, None, 100).iter().map(|p| p.key.token).collect();
        assert_eq!(all, vec![i64::MIN, -5, 0, 7, i64::MAX]);
        let some: Vec<i64> = m
            .range(&KeyBound::Token { token: -5, after: true }, &KeyBound::Token { token: 7, after: true }, None, 100)
            .iter()
            .map(|p| p.key.token)
            .collect();
        assert_eq!(some, vec![0, 7]);
        let after: Vec<i64> = m.range(&KeyBound::Unbounded, &KeyBound::Unbounded, Some(&key(0)), 1).iter().map(|p| p.key.token).collect();
        assert_eq!(after, vec![7]);
        assert_eq!(m.all().len(), 5);
    }
}
