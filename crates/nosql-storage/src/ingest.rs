//! Bulk loading: SSTables written outside the write path and swapped into tables atomically
//! (imports of Cassandra / ScyllaDB backups).
//!
//! 1. [`Table::ingest_writer`] streams partitions, in token order, into an SSTable in a scratch
//!    file (only the block being filled, the block index and the bloom filter are in memory).
//!    [`IngestWriter::write_update`] takes a whole [`PartitionUpdate`] instead and normalises it
//!    first (rows merged, range tombstones made disjoint markers), for sources that need merging.
//! 2. [`Table::ingest_upload`] uploads it under the table's key prefix.
//! 3. [`Database::stage_ingest`] records the uploaded SSTables in a named snapshot, so garbage
//!    collection keeps them while a long import is still running.
//! 4. [`Database::ingest`] swaps every table's new SSTables in at once and records one manifest
//!    version, so a branch can be taken right after. With `replace`, a table's previous contents
//!    (SSTables and memtables) go, as with `TRUNCATE`: a refreshed mirror drops rows the source
//!    deleted.

use crate::clip::{Item, Slices};
use crate::db::Database;
use crate::manifest::{self, Manifest, SstRef, TableEntry, TableId};
use crate::memtable::{MemCursor, Memtable};
use crate::model::{DeletionTime, PartitionKey, PartitionUpdate, Row};
use crate::sstable::SsTable;
use crate::sstable::writer::{SstWriter, WriterOptions};
use crate::table::{Sink, Table, TableState, sst_ref};
use crate::tombstones::Marker;
use anyhow::{Result, bail};
use bytes::Bytes;
use std::sync::Arc;

/// An SSTable being bulk-written into a scratch file.
pub struct IngestWriter {
    w: SstWriter<Sink>,
    table: Arc<Table>,
}

impl IngestWriter {
    pub fn begin_partition(&mut self, key: &PartitionKey, deletion: DeletionTime, static_row: Option<Row>) -> Result<()> {
        let static_row = static_row.and_then(|r| self.table.normalize_row(r));
        self.w.begin_partition(key, deletion, static_row)
    }

    /// Rows and markers of the open partition, in clustering order. Rows are normalised as on
    /// the write path (columns sorted by id, complex cells by path, duplicates reconciled, data
    /// the row's own deletions shadow dropped).
    pub fn add_row(&mut self, row: Row) -> Result<()> {
        match self.table.normalize_row(row) {
            Some(r) => self.w.add_row(&r),
            None => Ok(()),
        }
    }

    pub fn add_marker(&mut self, m: &Marker) -> Result<()> {
        self.w.add_marker(m)
    }

    pub fn end_partition(&mut self) -> Result<()> {
        self.w.end_partition()
    }

    /// Write a whole partition given as an update in any order: rows are normalised and merged
    /// and range tombstones turned into disjoint markers, as the write path does.
    pub fn write_update(&mut self, mut u: PartitionUpdate) -> Result<()> {
        self.table.normalize(&mut u);
        let opts = self.table.options();
        let mt = Memtable::new(opts.comparator.clone(), opts.reconcile);
        if mt.apply(u, 1).is_err() {
            bail!("ingest: the scratch memtable refused a partition");
        }
        for p in mt.all() {
            let h = p.header();
            self.w.begin_partition(&h.key, h.deletion, h.static_row)?;
            let mut c = MemCursor::new(p, opts.comparator.clone(), Slices::all(false));
            while let Some(item) = c.next_item() {
                match item {
                    Item::Row(r) => self.w.add_row(&r)?,
                    Item::Marker(m) => self.w.add_marker(&m)?,
                }
            }
            self.w.end_partition()?;
        }
        Ok(())
    }

    /// Give up on this SSTable and delete its scratch file.
    pub fn discard(self) {
        self.w.abandon().discard();
    }

    /// Bytes written so far (compressed).
    pub fn bytes_written(&self) -> u64 {
        self.w.bytes_written()
    }

    pub fn partitions(&self) -> u64 {
        self.w.partitions()
    }

    pub fn is_empty(&self) -> bool {
        self.w.is_empty()
    }
}

impl Table {
    /// A writer for an SSTable to bulk-load into this table (`expected_partitions` sizes its
    /// bloom filter). The data goes to a scratch file in the node's temp directory.
    pub fn ingest_writer(self: &Arc<Self>, expected_partitions: u64) -> Result<IngestWriter> {
        let opts = self.options();
        let wopts = WriterOptions { block_size: opts.block_size, codec: opts.codec, bloom_fp_chance: opts.bloom_filter_fp_chance, expected_partitions: expected_partitions.max(1) };
        let dir = self.env.tmp_dir.clone().unwrap_or_else(std::env::temp_dir);
        Ok(IngestWriter { w: SstWriter::new(Sink::new(Some(&dir))?, opts.comparator.clone(), wopts), table: self.clone() })
    }

    /// Finish and upload a bulk-written SSTable (`None` when it holds nothing). It is not part
    /// of the table until [`Database::ingest`].
    pub async fn ingest_upload(&self, w: IngestWriter) -> Result<Option<Arc<SsTable>>> {
        if w.w.is_empty() {
            w.w.finish()?.0.discard();
            return Ok(None);
        }
        let (sink, index) = tokio::task::spawn_blocking(move || -> Result<_> {
            let (sink, index) = w.w.finish()?;
            Ok((sink.done()?, index))
        })
        .await??;
        if index.blocks.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.upload(sink, index, 0, rand::random::<u64>()).await?))
    }
}

/// The SSTables to swap into one table.
#[derive(Clone, Debug)]
pub struct Ingested {
    pub table: TableId,
    pub sstables: Vec<Arc<SsTable>>,
    /// Drop the table's previous contents (SSTables and memtables).
    pub replace: bool,
}

impl Database {
    /// Record uploaded-but-not-yet-ingested SSTables in the snapshot `name`, so garbage
    /// collection keeps them (call again as more are uploaded; [`Database::clear_snapshot`]
    /// when done).
    pub async fn stage_ingest(&self, name: &str, staged: &[(TableId, Vec<Arc<SsTable>>)]) -> Result<()> {
        let mut m = Manifest::new(&self.id);
        for (id, ssts) in staged {
            let e: &mut TableEntry = m.tables.entry(*id).or_default();
            e.sstables.extend(ssts.iter().map(|s| sst_ref(s)));
        }
        let raw = Bytes::from(serde_json::to_vec(&m)?);
        self.env.objects.put(&manifest::snapshot_key(&self.id, name), raw).await
    }

    /// Swap bulk-written SSTables into their tables and record one manifest version.
    pub async fn ingest(&self, batches: Vec<Ingested>) -> Result<()> {
        let mut tables = Vec::with_capacity(batches.len());
        for b in &batches {
            let Some(t) = self.get_table(b.table) else { bail!("table {} is not attached", b.table) };
            tables.push(t);
        }
        // Replacing tables wait for their flushes and compactions, as TRUNCATE does.
        let mut guards = Vec::new();
        for (t, b) in tables.iter().zip(&batches) {
            if b.replace {
                guards.push((t.flush_lock.lock().await, t.compaction_lock.lock().await));
            }
        }
        let mut changes: Vec<(TableId, Vec<SstRef>, bool)> = Vec::with_capacity(batches.len());
        for (t, b) in tables.iter().zip(&batches) {
            let old = {
                let mut st = t.state.write();
                let old = (**st).clone();
                let next = if b.replace {
                    let opts = t.options();
                    TableState { active: Arc::new(Memtable::new(opts.comparator.clone(), opts.reconcile)), frozen: Vec::new(), sstables: b.sstables.clone() }
                } else {
                    let mut next = old.clone();
                    next.sstables.extend(b.sstables.iter().cloned());
                    next
                };
                *st = Arc::new(next);
                old
            };
            if b.replace {
                for m in std::iter::once(&old.active).chain(old.frozen.iter()) {
                    m.freeze();
                }
                for s in &old.sstables {
                    self.env.cache.forget(s.uid);
                }
            }
            changes.push((t.id, t.state().sstables.iter().map(|s| sst_ref(s)).collect(), b.replace));
        }
        drop(guards);
        self.record_ingest(changes).await?;
        for t in &tables {
            t.schedule_compaction();
        }
        Ok(())
    }
}
