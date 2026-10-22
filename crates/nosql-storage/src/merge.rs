//! Merging memtables and SSTables: partitions by key, then each partition's rows and range
//! tombstone markers by clustering, reconciled (`UnfilteredRowIterators.merge` and
//! `UnfilteredPartitionIterators.merge` in Cassandra).
//!
//! The merge is a synchronous state machine: whenever a source needs blocks it doesn't have,
//! [`MStep::Pending`] reports every such source at once so the driver can fetch them
//! concurrently, then the merge resumes where it stopped. [`drive`] runs it on the async read
//! path; [`drive_blocking`] runs it inside compaction on the blocking pool.
//!
//! Sources whose newest write is no newer than the partition deletion found in the others are
//! dropped after the headers are read (Cassandra's timestamp-ordered skipping), and an
//! SSTable whose clustering range misses the requested slices is only asked for its header.

use crate::cache::BlockCache;
use crate::clip::{Item, Slices};
use crate::compare::{ClusteringComparator, compare_pos};
use crate::memtable::{MemCursor, MemPartition, Memtable};
use crate::model::*;
use crate::reconcile::{Reconcile, merge_rows};
use crate::sstable::Header;
use crate::sstable::cursor::{BlockNeed, KeyBound, SstCursor, Step};
use crate::tombstones::{Marker, MarkerMerger, dir};
use anyhow::Result;
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::sync::Arc;

pub enum MStep<T> {
    Ready(T),
    Pending(Vec<(usize, BlockNeed)>),
}

/// Partitions of one memtable: a single one (point reads) or a key range (scans).
pub struct MemSource {
    mt: Option<Arc<Memtable>>,
    range: Option<(KeyBound, KeyBound)>,
    queue: VecDeque<Arc<MemPartition>>,
    last: Option<PartitionKey>,
    exhausted: bool,
    cmp: Arc<dyn ClusteringComparator>,
    slices: Slices,
    cur: Option<MemCursor>,
    max_ts: i64,
}

impl MemSource {
    pub fn point(part: Arc<MemPartition>, max_ts: i64, cmp: Arc<dyn ClusteringComparator>, slices: Slices) -> Self {
        Self { mt: None, range: None, queue: VecDeque::from([part]), last: None, exhausted: true, cmp, slices, cur: None, max_ts }
    }

    pub fn scan(mt: Arc<Memtable>, start: KeyBound, end: KeyBound, slices: Slices) -> Self {
        let cmp = mt.comparator().clone();
        let max_ts = mt.max_timestamp();
        Self { mt: Some(mt), range: Some((start, end)), queue: VecDeque::new(), last: None, exhausted: false, cmp, slices, cur: None, max_ts }
    }

    fn next_header(&mut self) -> Option<Header> {
        self.cur = None;
        if self.queue.is_empty() && !self.exhausted {
            let (mt, (start, end)) = (self.mt.as_ref().expect("scan"), self.range.as_ref().expect("scan"));
            let batch = mt.range(start, end, self.last.as_ref(), 64);
            if batch.len() < 64 {
                self.exhausted = true;
            }
            self.queue.extend(batch);
        }
        let part = self.queue.pop_front()?;
        self.last = Some(part.key.clone());
        let cur = MemCursor::new(part, self.cmp.clone(), self.slices.clone());
        let h = cur.header();
        self.cur = Some(cur);
        Some(h)
    }
}

pub enum Source {
    Mem(MemSource),
    Sst(SstCursor),
}

impl Source {
    fn next_header(&mut self) -> Result<Step<Option<Header>>> {
        match self {
            Source::Mem(m) => Ok(Step::Ready(m.next_header())),
            Source::Sst(s) => s.next_header(),
        }
    }

    fn next_item(&mut self) -> Result<Step<Option<Item>>> {
        match self {
            Source::Mem(m) => Ok(Step::Ready(m.cur.as_mut().and_then(|c| c.next_item()))),
            Source::Sst(s) => s.next_item(),
        }
    }

    fn skip_partition(&mut self) {
        match self {
            Source::Mem(m) => m.cur = None,
            Source::Sst(s) => s.skip_partition(),
        }
    }

    fn max_timestamp(&self) -> i64 {
        match self {
            Source::Mem(m) => m.max_ts,
            Source::Sst(s) => s.sstable().stats().max_timestamp,
        }
    }

    pub fn supply(&mut self, first: usize, blocks: Vec<bytes::Bytes>) {
        if let Source::Sst(s) = self {
            s.supply(first, blocks);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HState {
    Need,
    Have,
    Done,
}

/// The merged header of one partition.
#[derive(Clone, Debug)]
pub struct MergedHeader {
    pub key: PartitionKey,
    pub deletion: DeletionTime,
    /// Merged under the partition deletion; not purged.
    pub static_row: Option<Row>,
    /// Sources holding the partition, and how many were skipped by timestamp.
    pub sources: usize,
    pub skipped: usize,
}

/// The row-level merge of one partition: a binary heap of the participating sources, keyed
/// by their next item's position.
struct RowMerge {
    parts: Vec<usize>,
    heads: Vec<Option<Item>>,
    heap: Vec<usize>,
    /// Participants whose next item must be fetched before the next pop.
    need: Vec<usize>,
    markers: MarkerMerger,
}

impl RowMerge {
    fn new(parts: Vec<usize>, deletion: DeletionTime) -> Self {
        let n = parts.len();
        Self { heads: vec![None; n], heap: Vec::with_capacity(n), need: (0..n).collect(), markers: MarkerMerger::new(n, deletion), parts }
    }

    fn less(&self, cmp: &dyn ClusteringComparator, reversed: bool, a: usize, b: usize) -> bool {
        let (x, y) = (self.heads[a].as_ref().expect("head"), self.heads[b].as_ref().expect("head"));
        match dir(compare_pos(cmp, x.pos(), y.pos()), reversed) {
            Ordering::Less => true,
            Ordering::Equal => a < b,
            Ordering::Greater => false,
        }
    }

    fn push(&mut self, cmp: &dyn ClusteringComparator, reversed: bool, j: usize) {
        self.heap.push(j);
        let mut i = self.heap.len() - 1;
        while i > 0 {
            let p = (i - 1) / 2;
            if !self.less(cmp, reversed, self.heap[i], self.heap[p]) {
                break;
            }
            self.heap.swap(i, p);
            i = p;
        }
    }

    fn pop(&mut self, cmp: &dyn ClusteringComparator, reversed: bool) -> Option<usize> {
        if self.heap.is_empty() {
            return None;
        }
        let top = self.heap.swap_remove(0);
        let mut i = 0;
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut m = i;
            if l < self.heap.len() && self.less(cmp, reversed, self.heap[l], self.heap[m]) {
                m = l;
            }
            if r < self.heap.len() && self.less(cmp, reversed, self.heap[r], self.heap[m]) {
                m = r;
            }
            if m == i {
                break;
            }
            self.heap.swap(i, m);
            i = m;
        }
        Some(top)
    }
}

pub struct Merger {
    cmp: Arc<dyn ClusteringComparator>,
    mode: Reconcile,
    reversed: bool,
    pub sources: Vec<Source>,
    heads: Vec<Option<Header>>,
    hstate: Vec<HState>,
    row: Option<RowMerge>,
}

impl Merger {
    pub fn new(cmp: Arc<dyn ClusteringComparator>, mode: Reconcile, reversed: bool, sources: Vec<Source>) -> Self {
        let n = sources.len();
        Self { cmp, mode, reversed, sources, heads: vec![None; n], hstate: vec![HState::Need; n], row: None }
    }

    /// Whether items come in reverse clustering order.
    pub fn reversed(&self) -> bool {
        self.reversed
    }

    /// The next partition (abandoning the rest of the current one).
    pub fn next_partition(&mut self) -> Result<MStep<Option<MergedHeader>>> {
        if let Some(rm) = self.row.take() {
            for p in rm.parts {
                self.sources[p].skip_partition();
            }
        }
        let mut pending = Vec::new();
        for i in 0..self.sources.len() {
            if self.hstate[i] != HState::Need {
                continue;
            }
            match self.sources[i].next_header()? {
                Step::Ready(Some(h)) => {
                    self.heads[i] = Some(h);
                    self.hstate[i] = HState::Have;
                }
                Step::Ready(None) => self.hstate[i] = HState::Done,
                Step::Pending(n) => pending.push((i, n)),
            }
        }
        if !pending.is_empty() {
            return Ok(MStep::Pending(pending));
        }
        let Some(min) = self.heads.iter().flatten().map(|h| &h.key).min().cloned() else { return Ok(MStep::Ready(None)) };
        let mut parts = Vec::new();
        let mut deletion = DeletionTime::LIVE;
        let mut statics = Vec::new();
        for i in 0..self.sources.len() {
            if self.heads[i].as_ref().is_some_and(|h| h.key == min) {
                let h = self.heads[i].take().expect("head");
                self.hstate[i] = HState::Need;
                deletion = deletion.max(h.deletion);
                if let Some(s) = h.static_row {
                    statics.push(s);
                }
                parts.push(i);
            }
        }
        let static_row = merge_rows(&*self.cmp, self.mode, statics, deletion);
        let sources = parts.len();
        let mut kept = Vec::with_capacity(parts.len());
        for i in parts {
            if !deletion.is_live() && self.sources[i].max_timestamp() <= deletion.marked_at {
                self.sources[i].skip_partition();
            } else {
                kept.push(i);
            }
        }
        let skipped = sources - kept.len();
        self.row = Some(RowMerge::new(kept, deletion));
        Ok(MStep::Ready(Some(MergedHeader { key: min, deletion, static_row, sources, skipped })))
    }

    /// Abandon the rest of the current partition.
    pub fn skip_partition(&mut self) {
        if let Some(rm) = self.row.take() {
            for p in rm.parts {
                self.sources[p].skip_partition();
            }
        }
    }

    /// The deletion that applies at the current position of the current partition.
    pub fn active_deletion(&self) -> DeletionTime {
        self.row.as_ref().map_or(DeletionTime::LIVE, |r| r.markers.active())
    }

    /// The current partition's next merged row or marker.
    pub fn next_item(&mut self) -> Result<MStep<Option<Item>>> {
        let cmp = &*self.cmp;
        let rev = self.reversed;
        let Some(rm) = self.row.as_mut() else { return Ok(MStep::Ready(None)) };
        loop {
            let mut pending = Vec::new();
            let need = std::mem::take(&mut rm.need);
            for j in need {
                match self.sources[rm.parts[j]].next_item()? {
                    Step::Ready(Some(it)) => {
                        rm.heads[j] = Some(it);
                        rm.push(cmp, rev, j);
                    }
                    Step::Ready(None) => {}
                    Step::Pending(n) => {
                        pending.push((rm.parts[j], n));
                        rm.need.push(j);
                    }
                }
            }
            if !pending.is_empty() {
                return Ok(MStep::Pending(pending));
            }
            let Some(m) = rm.pop(cmp, rev) else {
                self.row = None;
                return Ok(MStep::Ready(None));
            };
            let mut group = vec![m];
            while let Some(&top) = rm.heap.first() {
                let same = compare_pos(cmp, rm.heads[top].as_ref().expect("head").pos(), rm.heads[m].as_ref().expect("head").pos()) == Ordering::Equal;
                if !same {
                    break;
                }
                group.push(rm.pop(cmp, rev).expect("top"));
            }
            rm.need.extend(group.iter().copied());
            if matches!(rm.heads[m], Some(Item::Row(_))) {
                let rows: Vec<Row> = group
                    .iter()
                    .filter_map(|&j| match rm.heads[j].take() {
                        Some(Item::Row(r)) => Some(r),
                        _ => None,
                    })
                    .collect();
                if let Some(r) = merge_rows(cmp, self.mode, rows, rm.markers.active()) {
                    return Ok(MStep::Ready(Some(Item::Row(r))));
                }
            } else {
                let ms: Vec<(usize, Marker)> = group
                    .iter()
                    .filter_map(|&j| match rm.heads[j].take() {
                        Some(Item::Marker(mk)) => Some((j, mk)),
                        _ => None,
                    })
                    .collect();
                let bound = ms[0].1.bound.clone();
                let refs: Vec<(usize, &Marker)> = ms.iter().map(|(j, m)| (*j, m)).collect();
                if let Some(out) = rm.markers.merge(&bound, &refs) {
                    return Ok(MStep::Ready(Some(Item::Marker(out))));
                }
            }
        }
    }
}

/// Run a merge step to completion on the async read path, fetching blocks through the cache.
pub async fn drive<T>(m: &mut Merger, cache: &BlockCache, fill: bool, mut f: impl FnMut(&mut Merger) -> Result<MStep<T>>) -> Result<T> {
    loop {
        match f(m)? {
            MStep::Ready(t) => return Ok(t),
            MStep::Pending(needs) => {
                let fetched = futures::future::try_join_all(needs.iter().map(|(_, n)| cache.fetch(n, fill))).await?;
                for ((i, n), blocks) in needs.into_iter().zip(fetched) {
                    m.sources[i].supply(n.blocks.start, blocks);
                }
            }
        }
    }
}

/// [`drive`] from a blocking thread (compaction).
pub fn drive_blocking<T>(m: &mut Merger, cache: &BlockCache, rt: &tokio::runtime::Handle, f: impl FnMut(&mut Merger) -> Result<MStep<T>>) -> Result<T> {
    rt.block_on(drive(m, cache, false, f))
}
