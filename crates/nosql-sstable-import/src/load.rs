//! Converting a backup's partitions into roll SSTables of a target table.
//!
//! **Regular tables**: each source SSTable becomes roll SSTables one-to-one, streamed in its
//! own (token) order: one row in memory at a time, plus the read-ahead window and one
//! decompressed chunk. Replicas and overlapping SSTables need no merge here: they are
//! reconciled by roll's reads and compactions exactly as Cassandra's own would.
//!
//! **Counter tables**: a counter's replicas must merge per counter id (their contexts), which
//! roll's `(clock, total)` cells can't express afterwards, so all sources of a counter table are
//! merged here, partition by partition (a k-way merge on token order; one partition from each
//! source in memory, bounded by [`LoadOptions::partition_budget`]).
//!
//! Cells of columns the target dropped are skipped when written at or before the drop;
//! columns the target doesn't know are skipped with a warning.

use crate::counter::Context;
use crate::rows::{Item, PartitionHeader};
use crate::sstable::{OpenSst, SstFiles, Store, open};
use crate::stats::HeaderColumn;
use anyhow::{Context as _, Result, bail};
use bytes::Bytes;
use nosql_storage::ingest::IngestWriter;
use nosql_storage::sstable::SsTable;
use nosql_storage::table::Table;
use nosql_storage::tombstones::Marker;
use nosql_storage::{Bound, Cell, Clustering, ColumnData, DeletionTime, PartitionKey, PartitionUpdate, RangeTombstone, Row};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A column of the target table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetColumn {
    pub id: u32,
    pub is_static: bool,
    pub complex: bool,
    pub counter: bool,
    /// The column's type structure as [`crate::types::SType::shape`] renders it (no freezing,
    /// UDTs by lowercase name), checked against the SSTable's when given.
    pub shape: Option<String>,
    /// Cells written at or before this (microseconds) belong to a dropped incarnation.
    pub dropped_at: Option<i64>,
}

/// The token of a serialised partition key.
pub type TokenFn = Arc<dyn Fn(&[u8]) -> i64 + Send + Sync>;

/// Where converted partitions go.
pub struct Target {
    pub keyspace: String,
    pub table: String,
    pub storage: Arc<Table>,
    /// Columns by name (bytes), including dropped ones the table remembers.
    pub columns: HashMap<Vec<u8>, TargetColumn>,
    pub clustering_columns: usize,
    pub counter: bool,
    /// The token of a serialised partition key (Murmur3, as the target's dialect computes it).
    pub token: TokenFn,
}

#[derive(Clone, Debug)]
pub struct LoadOptions {
    /// Read-ahead of each Data.db (bytes).
    pub window: usize,
    /// Output SSTables are cut past this many (compressed) bytes.
    pub max_output_bytes: u64,
    /// … or this many partitions (sizes their bloom filters).
    pub max_output_partitions: u64,
    /// Most bytes of one counter partition held while merging its replicas.
    pub partition_budget: u64,
    /// Read-ahead budget shared by the sources of a counter table merge.
    pub merge_window_budget: usize,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self { window: 4 << 20, max_output_bytes: 1 << 30, max_output_partitions: 4_000_000, partition_budget: 64 << 20, merge_window_budget: 32 << 20 }
    }
}

/// What a conversion did.
#[derive(Clone, Debug, Default)]
pub struct Loaded {
    pub sstables: Vec<Arc<SsTable>>,
    pub partitions: u64,
    pub rows: u64,
    pub warnings: Vec<String>,
}

/// Progress counters shared with the caller.
#[derive(Debug, Default)]
pub struct Progress {
    pub rows: AtomicU64,
    pub partitions: AtomicU64,
    pub bytes: AtomicU64,
}

/// Maps one source SSTable's header columns to the target.
struct ColumnMap {
    statics: Vec<Option<TargetColumn>>,
    regulars: Vec<Option<TargetColumn>>,
}

fn norm_cql(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace() && *c != '"').collect::<String>().to_ascii_lowercase().replace("varchar", "text")
}

impl ColumnMap {
    fn new(target: &Target, sst: &OpenSst, warnings: &mut Vec<String>) -> ColumnMap {
        let map = |cols: &[HeaderColumn], is_static: bool, warnings: &mut Vec<String>| -> Vec<Option<TargetColumn>> {
            cols.iter()
                .map(|c| {
                    let name = c.name_str();
                    let Some(t) = target.columns.get(&c.name[..]) else {
                        if !warnings.iter().any(|w| w.contains(&format!("column {name} "))) {
                            warnings.push(format!("{}.{}: column {name} of the backup isn't in the table; its cells were skipped", target.keyspace, target.table));
                        }
                        return None;
                    };
                    let kind_ok = t.is_static == is_static && t.complex == c.complex && t.counter == c.ty.is_counter();
                    let type_ok = t.shape.as_ref().is_none_or(|sh| norm_cql(sh) == norm_cql(&c.ty.shape()));
                    if !kind_ok || !type_ok {
                        warnings.push(format!(
                            "{}.{}: column {name} is {} in the backup but {} in the table; its cells were skipped",
                            target.keyspace,
                            target.table,
                            c.ty.cql(),
                            t.shape.clone().unwrap_or_else(|| "different".into())
                        ));
                        return None;
                    }
                    Some(t.clone())
                })
                .collect()
        };
        let h = &sst.stats.header;
        ColumnMap { statics: map(&h.statics, true, warnings), regulars: map(&h.regulars, false, warnings) }
    }

    /// The row with target column ids; counter contexts kept raw when `raw_counters`.
    fn row(&self, mut row: Row, is_static: bool, raw_counters: bool) -> Result<Row> {
        let cols = if is_static { &self.statics } else { &self.regulars };
        let mut out = Vec::with_capacity(row.columns.len());
        for c in row.columns.drain(..) {
            let Some(Some(t)) = cols.get(c.column() as usize) else { continue };
            let keep = |cell: &Cell| t.dropped_at.is_none_or(|d| cell.timestamp > d);
            match c {
                ColumnData::Simple { cell, .. } => {
                    if !keep(&cell) {
                        continue;
                    }
                    let cell = if t.counter && !raw_counters { counter_cell(cell)? } else { cell };
                    out.push(ColumnData::Simple { column: t.id, cell });
                }
                ColumnData::Complex { mut deletion, cells, .. } => {
                    if t.dropped_at.is_some_and(|d| deletion.marked_at <= d) {
                        deletion = DeletionTime::LIVE;
                    }
                    let cells: Vec<Cell> = cells.into_iter().filter(keep).collect();
                    if cells.is_empty() && deletion.is_live() {
                        continue;
                    }
                    out.push(ColumnData::Complex { column: t.id, deletion, cells });
                }
            }
        }
        row.columns = out;
        Ok(row)
    }
}

/// A counter cell in roll's model.
fn counter_cell(c: Cell) -> Result<Cell> {
    if c.is_tombstone() {
        return Ok(Cell::counter_tombstone(c.timestamp, c.local_deletion_time));
    }
    let ctx = Context::parse(&c.value)?;
    Ok(Cell::counter(c.timestamp, ctx.clock(), ctx.total()))
}

struct Output<'a> {
    target: &'a Target,
    opts: &'a LoadOptions,
    rt: &'a tokio::runtime::Handle,
    writer: Option<IngestWriter>,
    done: Vec<Arc<SsTable>>,
    expected: u64,
}

impl Output<'_> {
    fn writer(&mut self) -> Result<&mut IngestWriter> {
        if self.writer.is_none() {
            self.writer = Some(self.target.storage.ingest_writer(self.expected.clamp(1, self.opts.max_output_partitions))?);
        }
        Ok(self.writer.as_mut().expect("writer"))
    }

    /// After each partition: cut the output when it's big enough.
    fn maybe_cut(&mut self) -> Result<()> {
        if let Some(w) = &self.writer
            && (w.bytes_written() >= self.opts.max_output_bytes || w.partitions() >= self.opts.max_output_partitions)
        {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if let Some(w) = self.writer.take()
            && let Some(s) = self.rt.block_on(self.target.storage.ingest_upload(w))?
        {
            self.done.push(s);
        }
        Ok(())
    }
}

fn key(target: &Target, raw: &Bytes) -> PartitionKey {
    PartitionKey::new((target.token)(raw), raw.clone())
}

/// Convert one SSTable of a regular (non-counter) table. Blocking: run it on a blocking
/// thread; uploads go through `rt`.
pub fn convert_sstable(store: &dyn Store, files: &SstFiles, target: &Target, opts: &LoadOptions, rt: &tokio::runtime::Handle, progress: &Progress) -> Result<Loaded> {
    let mut sst = open(store, files, opts.window)?;
    check_clustering(target, &sst)?;
    let mut loaded = Loaded::default();
    let map = ColumnMap::new(target, &sst, &mut loaded.warnings);
    let mut out = Output { target, opts, rt, writer: None, done: vec![], expected: sst.stats.partitions };
    let name = files.data_key().to_string();
    let mut last: Option<PartitionKey> = None;
    let mut read_before = 0u64;
    let result = (|| -> Result<()> {
        while let Some(h) = sst.scanner.next_partition()? {
            let k = key(target, &h.key);
            if last.as_ref().is_some_and(|l| &k <= l) {
                bail!("partitions out of token order (is the partitioner Murmur3?)");
            }
            let static_row = h.static_row.map(|r| map.row(r, true, false)).transpose()?;
            let w = out.writer()?;
            w.begin_partition(&k, h.deletion, static_row)?;
            while let Some(item) = sst.scanner.next_item()? {
                match item {
                    Item::Row(r) => {
                        w.add_row(map.row(r, false, false)?)?;
                        loaded.rows += 1;
                        progress.rows.fetch_add(1, Ordering::Relaxed);
                    }
                    Item::Marker(m) => w.add_marker(&m)?,
                }
            }
            w.end_partition()?;
            loaded.partitions += 1;
            progress.partitions.fetch_add(1, Ordering::Relaxed);
            let pos = sst.scanner.input().pos();
            progress.bytes.fetch_add(pos - read_before, Ordering::Relaxed);
            read_before = pos;
            last = Some(k);
            out.maybe_cut()?;
        }
        sst.scanner.input().finish()
    })();
    if let Err(e) = result {
        if let Some(w) = out.writer.take() {
            w.discard();
        }
        return Err(e.context(format!("reading {name}")));
    }
    out.flush()?;
    loaded.sstables = out.done;
    Ok(loaded)
}

fn check_clustering(target: &Target, sst: &OpenSst) -> Result<()> {
    let n = sst.stats.header.clustering.len();
    if n != target.clustering_columns {
        bail!("{}: {n} clustering columns in the backup, {} in {}.{}", sst.files.data_key(), target.clustering_columns, target.keyspace, target.table);
    }
    Ok(())
}

/// One source of a counter table merge.
struct MergeSource {
    sst: OpenSst,
    map: ColumnMap,
    next: Option<(PartitionKey, PartitionHeader)>,
}

/// Convert every SSTable of a counter table, merging replicas' counter contexts. Blocking.
pub fn convert_counters(store: &dyn Store, sources: &[SstFiles], target: &Target, opts: &LoadOptions, rt: &tokio::runtime::Handle, progress: &Progress) -> Result<Loaded> {
    let mut loaded = Loaded::default();
    let window = (opts.merge_window_budget / sources.len().max(1)).clamp(64 << 10, opts.window);
    let mut srcs = Vec::with_capacity(sources.len());
    let mut expected = 0;
    for f in sources {
        let mut sst = open(store, f, window)?;
        check_clustering(target, &sst)?;
        let map = ColumnMap::new(target, &sst, &mut loaded.warnings);
        expected = expected.max(sst.stats.partitions);
        let next = sst.scanner.next_partition().with_context(|| format!("reading {}", f.data_key()))?.map(|h| (key(target, &h.key), h));
        srcs.push(MergeSource { sst, map, next });
    }
    let mut out = Output { target, opts, rt, writer: None, done: vec![], expected };
    let r = merge_all(&mut srcs, &mut out, target, opts, progress, &mut loaded);
    if let Err(e) = r {
        if let Some(w) = out.writer.take() {
            w.discard();
        }
        return Err(e);
    }
    for s in &mut srcs {
        s.sst.scanner.input().finish()?;
    }
    out.flush()?;
    loaded.sstables = out.done;
    Ok(loaded)
}

fn merge_all(srcs: &mut [MergeSource], out: &mut Output, target: &Target, opts: &LoadOptions, progress: &Progress, loaded: &mut Loaded) -> Result<()> {
    loop {
        let Some(min) = srcs.iter().filter_map(|s| s.next.as_ref().map(|n| &n.0)).min().cloned() else { break };
        let mut update = PartitionUpdate::new(min.clone());
        let mut rows: BTreeMap<Bytes, Vec<Row>> = BTreeMap::new();
        let mut statics: Vec<Row> = Vec::new();
        let mut held = 0u64;
        for s in srcs.iter_mut() {
            if s.next.as_ref().is_none_or(|n| n.0 != min) {
                continue;
            }
            let (_, h) = s.next.take().expect("next");
            update.deletion = update.deletion.max(h.deletion);
            if let Some(st) = h.static_row {
                statics.push(s.map.row(st, true, true)?);
            }
            let mut open: Option<(Bound, DeletionTime)> = None;
            while let Some(item) = s.sst.scanner.next_item()? {
                match item {
                    Item::Row(r) => {
                        held += 64 + r.columns.len() as u64 * 64 + r.clustering.encoded().len() as u64;
                        let r = s.map.row(r, false, true)?;
                        rows.entry(r.clustering.encoded().clone()).or_default().push(r);
                        loaded.rows += 1;
                        progress.rows.fetch_add(1, Ordering::Relaxed);
                    }
                    Item::Marker(m) => marker_to_rts(m, &mut open, &mut update.range_tombstones),
                }
                if held > opts.partition_budget {
                    bail!("a partition of counter table {}.{} is too large to merge in memory ({} bytes)", target.keyspace, target.table, opts.partition_budget);
                }
            }
            s.next = s.sst.scanner.next_partition().with_context(|| format!("reading {}", s.sst.files.data_key()))?.map(|h| (key(target, &h.key), h));
            if s.next.as_ref().is_some_and(|n| n.0 <= min) {
                bail!("{}: partitions out of token order", s.sst.files.data_key());
            }
        }
        if !statics.is_empty() {
            update.static_row = Some(merge_counter_rows(statics, Clustering::EMPTY)?);
        }
        for (_, group) in rows {
            let c = group[0].clustering.clone();
            update.rows.push(merge_counter_rows(group, c)?);
        }
        out.writer()?.write_update(update)?;
        loaded.partitions += 1;
        progress.partitions.fetch_add(1, Ordering::Relaxed);
        out.maybe_cut()?;
    }
    Ok(())
}

/// Pair a source's markers into range tombstones.
fn marker_to_rts(m: Marker, open: &mut Option<(Bound, DeletionTime)>, out: &mut Vec<RangeTombstone>) {
    if let Some(_close) = m.close
        && let Some((start, d)) = open.take()
    {
        out.push(RangeTombstone::new(start, m.bound.clone(), d));
    }
    if let Some(d) = m.open {
        *open = Some((m.bound, d));
    }
}

/// Merge replicas' copies of one counter row: liveness and deletion as reconciliation does,
/// counter cells by context (a counter tombstone wins).
fn merge_counter_rows(rows: Vec<Row>, clustering: Clustering) -> Result<Row> {
    let mut out = Row::new(clustering);
    let mut cells: BTreeMap<u32, Vec<Cell>> = BTreeMap::new();
    for r in rows {
        if r.liveness.supersedes(&out.liveness) {
            out.liveness = r.liveness;
        }
        if r.deletion.supersedes(&out.deletion) {
            out.deletion = r.deletion;
        }
        for c in r.columns {
            if let ColumnData::Simple { column, cell } = c {
                cells.entry(column).or_default().push(cell);
            }
        }
    }
    for (column, cs) in cells {
        let tomb = cs.iter().filter(|c| c.is_tombstone()).max_by_key(|c| (c.timestamp, c.local_deletion_time)).cloned();
        let cell = match tomb {
            Some(t) => Cell::counter_tombstone(t.timestamp, t.local_deletion_time),
            None => {
                let mut ctx = Context::default();
                let mut ts = i64::MIN;
                for c in &cs {
                    ctx = ctx.merge(&Context::parse(&c.value)?);
                    ts = ts.max(c.timestamp);
                }
                Cell::counter(ts, ctx.clock(), ctx.total())
            }
        };
        out.columns.push(ColumnData::Simple { column, cell });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::counter::tests::{encode, g};

    #[test]
    fn counter_rows_merge_by_context() {
        let row = |shards: &[crate::counter::Shard], ts: i64| Row::new(Clustering::EMPTY).with_cell(1, Cell::live(ts, Bytes::from(encode(shards))));
        let m = merge_counter_rows(vec![row(&[g(1, 5, 10), g(2, 3, 4)], 10), row(&[g(2, 4, 6)], 20)], Clustering::EMPTY).unwrap();
        let c = m.cell(1).unwrap();
        assert_eq!(c.counter_parts(), Some((9, 16)));
        assert_eq!(c.timestamp, 20);
        let dead = Row::new(Clustering::EMPTY).with_cell(1, Cell::tombstone(5, 100));
        let m = merge_counter_rows(vec![row(&[g(1, 9, 9)], 50), dead], Clustering::EMPTY).unwrap();
        assert!(m.cell(1).unwrap().is_tombstone() && m.cell(1).unwrap().is_counter(), "a counter tombstone wins");
    }

    #[test]
    fn markers_pair_into_range_tombstones() {
        let b = |after| Bound { prefix: Clustering::new([b"a"]), after };
        let d1 = DeletionTime::new(1, 1);
        let d2 = DeletionTime::new(2, 2);
        let mut open = None;
        let mut out = vec![];
        marker_to_rts(Marker::open(b(false), d1), &mut open, &mut out);
        marker_to_rts(Marker { bound: b(true), close: Some(d1), open: Some(d2) }, &mut open, &mut out);
        marker_to_rts(Marker::close(Bound::TOP, d2), &mut open, &mut out);
        assert_eq!(out, vec![RangeTombstone::new(b(false), b(true), d1), RangeTombstone::new(b(true), Bound::TOP, d2)]);
        assert!(open.is_none());
    }

    #[test]
    fn types_compare_loosely() {
        assert_eq!(norm_cql("map<text, int>"), norm_cql("map<varchar,int>"));
        assert_eq!(norm_cql("frozen<\"Addr\">"), norm_cql("frozen<Addr>"));
    }
}
