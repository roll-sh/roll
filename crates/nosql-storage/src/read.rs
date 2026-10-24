//! Read requests and their results: what to read (partition, clustering filter, columns,
//! limits, the time "now"), and turning merged data into what the engine returns.
//!
//! Merged rows go through the same steps as Cassandra's `ReadCommand.executeLocally`:
//! purgeable tombstones are dropped (`withoutPurgeableTombstones`), tombstones are counted
//! (`MetricRecording`, which fails the read past the threshold), then rows without live data
//! disappear and only live cells of the queried columns are kept (`filter`), under the limits
//! (`DataLimits.CQLLimits`).

use crate::cache::BlockCache;
use crate::clip::{Item, Slices};
use crate::compare::{ClusteringComparator, compare_paths};
use crate::merge::{MergedHeader, Merger, drive};
use crate::model::*;
use crate::reconcile::{GcPurger, Purger, purge_row};
use crate::sstable::cursor::KeyBound;
use crate::tombstones::{Marker, purge_marker};
use anyhow::Result;
use bytes::Bytes;
use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AO};

/// Which rows of a partition to read.
#[derive(Clone, Debug, PartialEq)]
pub enum ClusteringFilter {
    /// Ranges of clusterings (any order, may overlap).
    Slices(Vec<Slice>),
    /// Exact clusterings.
    Names(Vec<Clustering>),
}

impl ClusteringFilter {
    pub fn all() -> Self {
        ClusteringFilter::Slices(vec![Slice::ALL])
    }

    pub(crate) fn to_slices(&self, cmp: &dyn ClusteringComparator, reversed: bool) -> Slices {
        let slices = match self {
            ClusteringFilter::Slices(s) => s.clone(),
            ClusteringFilter::Names(n) => n.iter().cloned().map(Slice::prefix).collect(),
        };
        Slices::normalize(cmp, slices, reversed)
    }
}

/// Part of a complex column's cells.
#[derive(Clone, Debug, PartialEq)]
pub enum PathSelection {
    Element(Bytes),
    /// Paths in `[from, to]` (inclusive; `None` is unbounded).
    Slice { from: Option<Bytes>, to: Option<Bytes> },
}

/// Which columns to read and to return.
///
/// `fetched` decides row existence: a row with a live cell in any fetched column is returned,
/// even if none of the `queried` columns has a value (Cassandra's "fetch all, query a subset":
/// `SELECT a FROM t` returns rows where only `b` is set, with `a` null).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ColumnFilter {
    /// Sorted; `None` = every column.
    pub fetched: Option<Vec<ColumnId>>,
    /// Sorted; `None` = every fetched column.
    pub queried: Option<Vec<ColumnId>>,
    /// Restrict returned cells of complex columns.
    pub subselections: Vec<(ColumnId, PathSelection)>,
    /// Skip the static row.
    pub skip_static: bool,
}

impl ColumnFilter {
    pub fn all() -> Self {
        Self::default()
    }

    fn fetches(&self, c: ColumnId) -> bool {
        self.fetched.as_ref().is_none_or(|f| f.binary_search(&c).is_ok())
    }

    fn queries(&self, c: ColumnId) -> bool {
        self.queried.as_ref().is_none_or(|f| f.binary_search(&c).is_ok())
    }
}

/// CQL row limits (`DataLimits.CQLLimits`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub rows: u64,
    pub per_partition: u64,
    /// A partition with a live static row and no live rows counts as one row (when the query
    /// selects static columns or only partition-level data).
    pub count_static_only: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Self { rows: u64::MAX, per_partition: u64::MAX, count_static_only: true }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadOptions {
    /// "Now" in seconds: TTLs and purgeability are evaluated at this instant.
    pub now: i64,
    /// Fail with [`TombstoneOverwhelming`] after this many tombstones.
    pub tombstone_failure_threshold: Option<u64>,
    /// Return merged data as stored: tombstones included, nothing purged or filtered (beyond
    /// the column filter's `fetched`).
    pub raw: bool,
    /// ScyllaDB's `query_tombstone_page_limit`: end the read once it has seen this many
    /// tombstones (partition deletions, dead rows and cells, range tombstone bounds), at the
    /// first position after that a read can resume from ([`ReadMetrics::page_cut`]).
    pub tombstone_page_limit: Option<u64>,
}

impl ReadOptions {
    pub fn at(now: i64) -> Self {
        Self { now, tombstone_failure_threshold: None, raw: false, tombstone_page_limit: None }
    }
}

/// Where a read ended because of [`ReadOptions::tombstone_page_limit`] (positions in query
/// order).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageCut {
    pub key: PartitionKey,
    /// Resume after this clustering of `key`; `None` (and no `before`): after the whole
    /// partition.
    pub after: Option<Clustering>,
    /// The exact position, when it is just before this clustering (a range tombstone bound
    /// that excludes it): resume from it, included. `after` is then the last position
    /// before it a read can resume after.
    pub before: Option<Clustering>,
}

/// A read of one partition.
#[derive(Clone, Debug)]
pub struct PartitionRead {
    pub key: PartitionKey,
    pub clustering: ClusteringFilter,
    pub reversed: bool,
    pub columns: ColumnFilter,
    pub limits: Limits,
    pub options: ReadOptions,
}

impl PartitionRead {
    pub fn new(key: PartitionKey, now: i64) -> Self {
        Self { key, clustering: ClusteringFilter::all(), reversed: false, columns: ColumnFilter::all(), limits: Limits::default(), options: ReadOptions::at(now) }
    }
}

/// Where a paged scan continues.
#[derive(Clone, Debug, PartialEq)]
pub struct Resume {
    pub key: PartitionKey,
    /// Continue inside `key` after this clustering (in query order); `None` continues after
    /// the whole partition.
    pub after: Option<Clustering>,
    /// Rows of `key` already returned (for the per-partition limit).
    pub returned_in_partition: u64,
}

/// A read of a range of partitions in token order.
#[derive(Clone, Debug)]
pub struct RangeRead {
    pub start: KeyBound,
    pub end: KeyBound,
    pub resume: Option<Resume>,
    pub clustering: ClusteringFilter,
    pub reversed: bool,
    pub columns: ColumnFilter,
    pub limits: Limits,
    pub options: ReadOptions,
}

impl RangeRead {
    pub fn all(now: i64) -> Self {
        Self {
            start: KeyBound::Unbounded,
            end: KeyBound::Unbounded,
            resume: None,
            clustering: ClusteringFilter::all(),
            reversed: false,
            columns: ColumnFilter::all(),
            limits: Limits::default(),
            options: ReadOptions::at(now),
        }
    }
}

/// A read failed because it scanned too many tombstones (`TombstoneOverwhelmingException`).
#[derive(Debug, Clone, thiserror::Error)]
#[error("Scanned over {scanned} tombstones during query; query aborted")]
pub struct TombstoneOverwhelming {
    pub scanned: u64,
    pub key: PartitionKey,
    pub clustering: Option<Clustering>,
}

/// Counters of one read, readable while it streams and after.
#[derive(Debug, Default)]
pub struct ReadMetrics {
    pub tombstones: AtomicU64,
    pub live_rows: AtomicU64,
    pub partitions: AtomicU64,
    /// Sources (memtables, SSTables) consulted per partition, summed.
    pub sources: AtomicU64,
    /// SSTables skipped by partition-deletion timestamp.
    pub skipped_sources: AtomicU64,
    /// Set when the read ended at its tombstone page limit.
    pub page_cut: parking_lot::Mutex<Option<PageCut>>,
    /// Row positions, when enabled by [`ReadMetrics::trace_rows`] before the read starts.
    traced: std::sync::atomic::AtomicBool,
    pub trace: parking_lot::Mutex<ReadTrace>,
}

/// Where a read stands row by row, for Cassandra's tombstone warning (`ReadCommand`'s
/// `MetricRecording` counts what the command iterated before its limit stopped it, which a
/// reader that stops inside a chunk can't tell from the totals).
#[derive(Clone, Debug, Default)]
pub struct ReadTrace {
    /// The last partition the read opened.
    pub last_key: Option<PartitionKey>,
    /// Partitions opened with a live static row (Cassandra counts it as a live row).
    pub live_statics: u64,
    /// Live rows, live statics and tombstones counted before the last partition was opened,
    /// and the partition before it.
    pub opened: (u64, u64, u64),
    pub previous_key: Option<PartitionKey>,
    /// The same once its static row was read.
    pub after_static: (u64, u64, u64),
    /// One per row of the last chunk returned, once that row was read: live rows, live statics
    /// ([`Self::live_statics`]) and tombstones counted so far.
    pub marks: Vec<(u64, u64, u64)>,
}

impl ReadMetrics {
    pub fn tombstones(&self) -> u64 {
        self.tombstones.load(AO::Relaxed)
    }
    pub fn live_rows(&self) -> u64 {
        self.live_rows.load(AO::Relaxed)
    }

    /// Where the read stopped at its tombstone page limit (`None`: it didn't).
    pub fn page_cut(&self) -> Option<PageCut> {
        self.page_cut.lock().clone()
    }

    /// Records [`ReadTrace`] from now on (call before the read is first polled).
    pub fn trace_rows(&self) {
        self.traced.store(true, AO::Relaxed);
    }

    fn traced(&self) -> bool {
        self.traced.load(AO::Relaxed)
    }
}

/// Part of one partition's result. A partition comes in one or more chunks (wide partitions
/// stream); the static row is in its first chunk.
#[derive(Clone, Debug, PartialEq)]
pub struct PartitionChunk {
    pub key: PartitionKey,
    pub partition_deletion: DeletionTime,
    pub static_row: Option<Row>,
    pub rows: Vec<Row>,
    /// Range tombstone markers (raw reads only).
    pub markers: Vec<Marker>,
    pub first: bool,
    pub last: bool,
}

/// Rows per chunk.
const CHUNK_ROWS: usize = 256;

/// One merged source set with its own slices (a scan with a resumed first partition has two).
pub(crate) struct Phase {
    pub merger: Merger,
    /// Rows already returned for the first partition of this phase (resume).
    pub returned_in_first: u64,
}

struct Current {
    header: MergedHeader,
    rows_in_partition: u64,
    sent_first: bool,
    static_row: Option<Row>,
    static_live: bool,
    /// The last position a read can resume after (tombstone page limit).
    last_resume: Option<Clustering>,
}

pub(crate) struct ReadState {
    phases: std::collections::VecDeque<Phase>,
    cache: Arc<BlockCache>,
    cmp: Arc<dyn ClusteringComparator>,
    columns: ColumnFilter,
    limits: Limits,
    opts: ReadOptions,
    strict: bool,
    purger: GcPurger,
    pub metrics: Arc<ReadMetrics>,
    rows_total: u64,
    current: Option<Current>,
    done: bool,
    first_of_phase: bool,
    /// Tombstones seen (for the tombstone page limit).
    page_tombstones: std::cell::Cell<u64>,
}

impl ReadState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        phases: Vec<Phase>,
        cache: Arc<BlockCache>,
        cmp: Arc<dyn ClusteringComparator>,
        columns: ColumnFilter,
        limits: Limits,
        opts: ReadOptions,
        strict: bool,
        gc_before: i64,
        metrics: Arc<ReadMetrics>,
    ) -> Self {
        Self {
            phases: phases.into(),
            cache,
            cmp,
            columns,
            limits,
            opts,
            strict,
            purger: GcPurger { gc_before, max_purgeable: i64::MAX, max_purgeable_shadowable: i64::MAX, marker_dated_at_write: false },
            metrics,
            rows_total: 0,
            current: None,
            done: false,
            first_of_phase: true,
            page_tombstones: std::cell::Cell::new(0),
        }
    }

    /// Whether the tombstone page limit was reached: the read ends at the next position it can
    /// resume from.
    fn cut_due(&self) -> bool {
        self.opts.tombstone_page_limit.is_some_and(|l| self.page_tombstones.get() >= l)
    }

    /// Ends the read at `cut`.
    fn cut(&mut self, cut: PageCut) {
        *self.metrics.page_cut.lock() = Some(cut);
        self.done = true;
    }

    fn count_tombstone(&self, key: &PartitionKey, clustering: Option<&Clustering>) -> Result<()> {
        self.page_tombstones.set(self.page_tombstones.get() + 1);
        let n = self.metrics.tombstones.fetch_add(1, AO::Relaxed) + 1;
        if let Some(t) = self.opts.tombstone_failure_threshold
            && n > t
        {
            return Err(TombstoneOverwhelming { scanned: n, key: key.clone(), clustering: clustering.cloned() }.into());
        }
        Ok(())
    }

    /// Purge, count tombstones, and reduce to live data. `None`: nothing to return.
    fn process_row(&self, key: &PartitionKey, row: Row) -> Result<Option<Row>> {
        let now = self.opts.now;
        let mut row = row;
        if self.columns.fetched.is_some() {
            row.columns.retain(|c| self.columns.fetches(c.column()));
        }
        if self.opts.raw {
            return Ok(if row.is_empty() { None } else { Some(row) });
        }
        let Some(row) = purge_row(row, &self.purger, now, self.strict) else { return Ok(None) };
        // MetricRecording.applyToRow.
        let mut dead_cells = false;
        for c in row.cells() {
            if !c.is_live(now) {
                self.count_tombstone(key, Some(&row.clustering))?;
                dead_cells = true;
            }
        }
        let live = row.has_live_data(now, self.strict);
        if !live {
            if !row.liveness.is_live(now) && now >= row.min_local_deletion_time() && !dead_cells {
                self.count_tombstone(key, Some(&row.clustering))?;
            }
            return Ok(None);
        }
        Ok(Some(self.live_view(row)))
    }

    /// Only live cells of queried columns (and selected paths).
    fn live_view(&self, mut row: Row) -> Row {
        let now = self.opts.now;
        let cmp = &*self.cmp;
        row.deletion = RowDeletion::LIVE;
        let mut out = Vec::with_capacity(row.columns.len());
        for c in row.columns {
            let column = c.column();
            if !self.columns.queries(column) {
                continue;
            }
            match c {
                ColumnData::Simple { column, cell } => {
                    if cell.is_live(now) {
                        out.push(ColumnData::Simple { column, cell });
                    }
                }
                ColumnData::Complex { column, cells, .. } => {
                    let sel: Vec<&PathSelection> = self.columns.subselections.iter().filter(|(c, _)| *c == column).map(|(_, s)| s).collect();
                    let cells: Vec<Cell> = cells
                        .into_iter()
                        .filter(|cell| cell.is_live(now))
                        .filter(|cell| sel.is_empty() || sel.iter().any(|s| path_selected(cmp, column, s, cell.path.as_deref())))
                        .collect();
                    if !cells.is_empty() {
                        out.push(ColumnData::Complex { column, deletion: DeletionTime::LIVE, cells });
                    }
                }
            }
        }
        row.columns = out;
        row
    }

    /// The next chunk of results, `None` at the end.
    pub(crate) async fn next_chunk(&mut self) -> Result<Option<PartitionChunk>> {
        loop {
            if self.done {
                return Ok(None);
            }
            if self.phases.is_empty() {
                self.done = true;
                return Ok(None);
            }
            if self.current.is_none() {
                if self.rows_total >= self.limits.rows {
                    self.done = true;
                    return Ok(None);
                }
                let phase = self.phases.front_mut().expect("phase");
                let header = drive(&mut phase.merger, &self.cache, true, |m| m.next_partition()).await?;
                let Some(header) = header else {
                    self.phases.pop_front();
                    self.first_of_phase = true;
                    continue;
                };
                let returned_in_first = self.phases.front().expect("phase").returned_in_first;
                let returned = if self.first_of_phase { returned_in_first } else { 0 };
                self.first_of_phase = false;
                self.metrics.partitions.fetch_add(1, AO::Relaxed);
                self.metrics.sources.fetch_add(header.sources as u64, AO::Relaxed);
                self.metrics.skipped_sources.fetch_add(header.skipped as u64, AO::Relaxed);
                if self.metrics.traced() {
                    let mut t = self.metrics.trace.lock();
                    t.opened = (self.metrics.live_rows(), t.live_statics, self.metrics.tombstones());
                    t.previous_key = t.last_key.take();
                }
                let static_raw = header.static_row.clone();
                let mut static_row = None;
                let mut static_live = false;
                if !self.columns.skip_static
                    && let Some(s) = static_raw
                {
                    static_row = self.process_row(&header.key, s)?;
                    static_live = static_row.is_some();
                }
                let mut header = header;
                if !self.opts.raw && self.purger.should_purge_deletion(&header.deletion) {
                    header.deletion = DeletionTime::LIVE;
                }
                if self.opts.tombstone_page_limit.is_some() && !header.deletion.is_live() {
                    self.page_tombstones.set(self.page_tombstones.get() + 1);
                }
                if self.metrics.traced() {
                    let mut t = self.metrics.trace.lock();
                    t.last_key = Some(header.key.clone());
                    t.live_statics += u64::from(static_live);
                    t.after_static = (self.metrics.live_rows(), t.live_statics, self.metrics.tombstones());
                }
                self.current = Some(Current { header, rows_in_partition: returned, sent_first: false, static_row, static_live, last_resume: None });
                if returned >= self.limits.per_partition {
                    self.phases.front_mut().expect("phase").merger.skip_partition();
                }
            }
            // Fill a chunk.
            let mut rows = Vec::new();
            let mut markers = Vec::new();
            let mut ended = false;
            let traced = self.metrics.traced();
            if traced {
                self.metrics.trace.lock().marks.clear();
            }
            {
                let cur = self.current.as_ref().expect("current");
                if cur.rows_in_partition >= self.limits.per_partition || self.rows_total >= self.limits.rows {
                    ended = true;
                }
            }
            while !ended && rows.len() < CHUNK_ROWS {
                let phase = self.phases.front_mut().expect("phase");
                let phase_reversed = phase.merger.reversed();
                let item = drive(&mut phase.merger, &self.cache, true, |m| m.next_item()).await?;
                let key = self.current.as_ref().expect("current").header.key.clone();
                // Where a read can resume after this item, or from (the exact position just
                // before a bound that excludes its prefix): tombstone page limit.
                let mut resume_after: Option<Clustering> = None;
                let mut resume_from: Option<Clustering> = None;
                match item {
                    None => ended = true,
                    Some(Item::Marker(m)) => {
                        // In query order, a bound after its prefix comes after it (forward) or
                        // before it (reversed).
                        if m.bound.after != phase_reversed {
                            resume_after = Some(m.bound.prefix.clone());
                        } else {
                            resume_from = Some(m.bound.prefix.clone());
                        }
                        if self.opts.raw {
                            markers.push(m);
                        } else if purge_marker(m, &self.purger).is_some() {
                            self.count_tombstone(&key, None)?;
                        }
                    }
                    Some(Item::Row(r)) => {
                        resume_after = Some(r.clustering.clone());
                        if let Some(r) = self.process_row(&key, r)? {
                            let live = !self.opts.raw || r.has_live_data(self.opts.now, self.strict);
                            rows.push(r);
                            if live {
                                let n = self.metrics.live_rows.fetch_add(1, AO::Relaxed) + 1;
                                if traced {
                                    let mut t = self.metrics.trace.lock();
                                    let mark = (n, t.live_statics, self.metrics.tombstones());
                                    t.marks.push(mark);
                                }
                                let cur = self.current.as_mut().expect("current");
                                cur.rows_in_partition += 1;
                                self.rows_total += 1;
                                if cur.rows_in_partition >= self.limits.per_partition || self.rows_total >= self.limits.rows {
                                    ended = true;
                                }
                            }
                        }
                    }
                }
                let due = !ended && self.cut_due();
                let cur = self.current.as_mut().expect("current");
                let cut = match (&resume_after, resume_from) {
                    (Some(a), _) if due => Some((Some(a.clone()), None)),
                    (None, Some(f)) if due && cur.last_resume.is_some() => Some((cur.last_resume.clone(), Some(f))),
                    _ => None,
                };
                if resume_after.is_some() {
                    cur.last_resume = resume_after;
                }
                if let Some((after, before)) = cut {
                    let cur = self.current.take().expect("current");
                    let first = !cur.sent_first;
                    self.cut(PageCut { key: cur.header.key.clone(), after, before });
                    return Ok(Some(PartitionChunk {
                        key: cur.header.key,
                        partition_deletion: cur.header.deletion,
                        static_row: if first { cur.static_row } else { None },
                        rows,
                        markers,
                        first,
                        last: false,
                    }));
                }
            }
            let cur = self.current.as_mut().expect("current");
            if ended {
                self.phases.front_mut().expect("phase").merger.skip_partition();
            }
            let first = !cur.sent_first;
            let has_content = !rows.is_empty() || !markers.is_empty() || (first && (cur.static_live || (self.opts.raw && cur.static_row.is_some())));
            if ended {
                let cur = self.current.take().expect("current");
                // A partition with only a live static row counts as one row.
                if !self.opts.raw && cur.rows_in_partition == 0 && cur.static_live && self.limits.count_static_only {
                    self.rows_total += 1;
                }
                let emit = has_content || (self.opts.raw && !cur.header.deletion.is_live());
                if self.cut_due() {
                    self.cut(PageCut { key: cur.header.key.clone(), after: None, before: None });
                }
                if !emit && !cur.sent_first {
                    continue;
                }
                return Ok(Some(PartitionChunk {
                    key: cur.header.key,
                    partition_deletion: cur.header.deletion,
                    static_row: if first { cur.static_row } else { None },
                    rows,
                    markers,
                    first,
                    last: true,
                }));
            }
            if !has_content {
                continue;
            }
            cur.sent_first = true;
            return Ok(Some(PartitionChunk {
                key: cur.header.key.clone(),
                partition_deletion: cur.header.deletion,
                static_row: if first { cur.static_row.take() } else { None },
                rows,
                markers,
                first,
                last: false,
            }));
        }
    }
}

fn path_selected(cmp: &dyn ClusteringComparator, column: ColumnId, sel: &PathSelection, path: Option<&[u8]>) -> bool {
    match sel {
        PathSelection::Element(e) => compare_paths(cmp, column, path, Some(e)) == Ordering::Equal,
        PathSelection::Slice { from, to } => {
            from.as_ref().is_none_or(|f| compare_paths(cmp, column, path, Some(f)) != Ordering::Less)
                && to.as_ref().is_none_or(|t| compare_paths(cmp, column, path, Some(t)) != Ordering::Greater)
        }
    }
}

/// A whole partition's result (chunks joined).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Partition {
    pub key: PartitionKey,
    pub partition_deletion: DeletionTime,
    pub static_row: Option<Row>,
    pub rows: Vec<Row>,
    pub markers: Vec<Marker>,
}

/// Stream of a read's chunks, with its metrics.
pub struct ReadStream {
    pub metrics: Arc<ReadMetrics>,
    inner: futures::stream::BoxStream<'static, Result<PartitionChunk>>,
}

impl ReadStream {
    pub(crate) fn new(state: ReadState) -> Self {
        let metrics = state.metrics.clone();
        let inner = futures::stream::unfold(Some(state), |st| async move {
            let mut st = st?;
            match st.next_chunk().await {
                Ok(Some(c)) => Some((Ok(c), Some(st))),
                Ok(None) => None,
                Err(e) => Some((Err(e), None)),
            }
        });
        Self { metrics, inner: Box::pin(inner) }
    }

    /// Collect whole partitions.
    pub async fn partitions(mut self) -> Result<Vec<Partition>> {
        use futures::StreamExt;
        let mut out: Vec<Partition> = Vec::new();
        while let Some(c) = self.inner.next().await {
            let c = c?;
            if c.first || out.last().is_none_or(|p| p.key != c.key) {
                out.push(Partition { key: c.key.clone(), partition_deletion: c.partition_deletion, static_row: c.static_row, rows: Vec::new(), markers: Vec::new() });
            }
            let p = out.last_mut().expect("pushed");
            p.rows.extend(c.rows);
            p.markers.extend(c.markers);
        }
        Ok(out)
    }
}

impl futures::Stream for ReadStream {
    type Item = Result<PartitionChunk>;

    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

/// Slices restricted to after `after` in query order.
pub(crate) fn slices_after(cmp: &dyn ClusteringComparator, slices: &Slices, after: &Clustering) -> Slices {
    use crate::compare::compare_bounds;
    let mut out = Vec::new();
    for s in &slices.slices {
        let mut s = s.clone();
        if slices.reversed {
            let b = Bound::exclusive_end(after.clone());
            if compare_bounds(cmp, &b, &s.end) == Ordering::Less {
                s.end = b;
            }
        } else {
            let b = Bound::exclusive_start(after.clone());
            if compare_bounds(cmp, &b, &s.start) == Ordering::Greater {
                s.start = b;
            }
        }
        out.push(s);
    }
    Slices::normalize(cmp, out, slices.reversed)
}
