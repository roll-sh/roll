//! The write observer of derived data: for every write to a table with views, index tables or a
//! CDC log, read the rows it touches (under per-partition locks held until the write is
//! applied), reconcile them with the update, and hand `existing` / `merged` rows to the view
//! generators and the CDC log. Cassandra does the same read-before-write in
//! `TableViews.pushViewReplicaUpdates`, under the partition lock of `Keyspace.applyInternal`.

use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use futures::future::BoxFuture;
use nosql_storage::manifest::TableId;
use nosql_storage::read::{ClusteringFilter, ColumnFilter, Limits, PartitionRead, ReadOptions};
use nosql_storage::reconcile::merge_rows;
use nosql_storage::{
    Clustering, DeletionTime, PartitionKey, PartitionUpdate, RangeTombstone, Reconcile, Row, Slice,
};
use parking_lot::Mutex;

use super::spec::{BaseSpecs, Specs};
use super::update::{BasePartition, Generator, deleted_row};
use crate::database::Database;
use crate::error::{Error, Result};
use crate::hooks::{Derived, WriteObserver};
use crate::schema::Table;

const STRIPES: usize = 256;

/// The writes to one base partition: its table, derived specs and updates.
type Group<'a> = (Arc<Table>, Arc<BaseSpecs>, Vec<&'a PartitionUpdate>);

/// Views, index tables and CDC logs, maintained on the write path.
pub struct Observer {
    specs: Mutex<Option<Arc<Specs>>>,
    locks: Vec<Arc<tokio::sync::Mutex<()>>>,
    /// Held shared by every write until it is applied; [`WriteObserver::barrier`] takes it
    /// exclusively.
    gate: Arc<tokio::sync::RwLock<()>>,
}

impl Default for Observer {
    fn default() -> Self {
        Observer {
            specs: Mutex::new(None),
            locks: (0..STRIPES)
                .map(|_| Arc::new(tokio::sync::Mutex::new(())))
                .collect(),
            gate: Arc::new(tokio::sync::RwLock::new(())),
        }
    }
}

/// The rows one write touches in one base partition.
pub struct Touched<'a> {
    pub base: &'a Arc<Table>,
    pub key: PartitionKey,
    /// The updates to this partition, in statement order.
    pub updates: Vec<&'a PartitionUpdate>,
    /// Live data before the write: the static row and rows by encoded clustering.
    pub existing_static: Option<Row>,
    pub existing: BTreeMap<Bytes, Row>,
    /// Reconciled rows after the write (tombstones included).
    pub merged_static: Option<Row>,
    pub merged: BTreeMap<Bytes, Row>,
    pub now: i64,
}

impl Observer {
    /// The specs of the current schema (rebuilt after a schema change; hidden index tables are
    /// attached on the way).
    pub async fn specs(&self, db: &Arc<Database>) -> Result<Arc<Specs>> {
        let schema = db.schema();
        if let Some(s) = self.specs.lock().as_ref()
            && s.version == schema.version
        {
            return Ok(s.clone());
        }
        let specs = Arc::new(Specs::build(db, &schema));
        for b in specs.by_base.values() {
            for v in &b.views {
                if v.hidden && db.store.get_table(v.view.table_id()).is_none() {
                    crate::storage::attach(
                        &db.store,
                        &v.view,
                        db.dialect(),
                        db.memtable_flush_bytes(),
                    )
                    .await?;
                }
            }
        }
        *self.specs.lock() = Some(specs.clone());
        Ok(specs)
    }

    /// Forgets the cached specs (after DDL that changes them without a new schema version).
    pub fn invalidate(&self) {
        *self.specs.lock() = None;
    }

    fn stripe(table: &[u8; 16], token: i64) -> usize {
        let mut h = token as u64 ^ 0xD6E8_FEB8_6659_FD93;
        for b in table {
            h = h.rotate_left(7) ^ *b as u64;
        }
        (h % STRIPES as u64) as usize
    }

    async fn run(
        &self,
        db: &Arc<Database>,
        writes: &[(Arc<Table>, PartitionUpdate)],
    ) -> Result<Derived> {
        let mut out = Derived::default();
        if writes.is_empty() {
            return Ok(out);
        }
        out.guards
            .push(Box::new(self.gate.clone().read_owned().await));
        let specs = self.specs(db).await?;
        // Group the writes by base partition (statement order kept within one).
        let mut groups: BTreeMap<(u128, PartitionKey), Group<'_>> = BTreeMap::new();
        for (t, u) in writes {
            let Some(bs) = specs.get(&t.id) else {
                continue;
            };
            if bs.is_empty() {
                continue;
            }
            groups
                .entry((u128::from_be_bytes(t.id), u.key.clone()))
                .or_insert_with(|| (t.clone(), bs.clone(), Vec::new()))
                .2
                .push(u);
        }
        if groups.is_empty() {
            return Ok(out);
        }
        // Lock every touched partition, in ascending stripe order.
        let mut stripes: Vec<usize> = groups
            .iter()
            .map(|((id, k), _)| Self::stripe(&id.to_be_bytes(), k.token))
            .collect();
        stripes.sort_unstable();
        stripes.dedup();
        for s in stripes {
            let g = self.locks[s].clone().lock_owned().await;
            out.guards.push(Box::new(g));
        }
        let now = db.now_secs();
        for ((_, key), (base, bs, updates)) in &groups {
            let touched = touch(db, base, key.clone(), updates, now).await?;
            for spec in &bs.views {
                for u in generate(db, spec, &touched)? {
                    out.updates.push((spec.view.table_id(), u));
                }
            }
            if let Some(log) = &bs.cdc {
                out.updates
                    .extend(crate::cdc::log_updates(db, log, &touched)?);
            }
        }
        Ok(out)
    }
}

impl WriteObserver for Observer {
    fn on_write<'a>(
        &'a self,
        db: &'a Arc<Database>,
        writes: &'a [(Arc<Table>, PartitionUpdate)],
    ) -> BoxFuture<'a, Result<Derived>> {
        Box::pin(self.run(db, writes))
    }

    fn barrier<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            drop(self.gate.write().await);
        })
    }
}

pub fn reconcile_mode(db: &Database) -> Reconcile {
    if db.dialect().is_scylla() {
        Reconcile::Scylla
    } else {
        Reconcile::Cassandra
    }
}

/// The deletion covering `c` in `u` (partition deletion or range tombstones).
fn covering(base: &Table, u: &PartitionUpdate, c: &Clustering) -> DeletionTime {
    let cmp = &*base.comparator;
    let mut d = u.deletion;
    for rt in &u.range_tombstones {
        if nosql_storage::compare::slice_contains(cmp, &rt.start, &rt.end, c)
            && rt.deletion.supersedes(&d)
        {
            d = rt.deletion;
        }
    }
    d
}

/// The tombstones already stored in the partition a write touches: what shadows the written
/// rows besides the write's own deletions (a newer partition or range tombstone hides an older
/// insert, so its view entry must not appear).
struct Stored {
    partition: DeletionTime,
    /// Disjoint, sorted range tombstones (from the raw read's markers).
    ranges: Vec<RangeTombstone>,
    /// The rows as stored (row deletions and cell tombstones included), by encoded clustering.
    rows: BTreeMap<Bytes, Row>,
    static_row: Option<Row>,
}

impl Stored {
    /// Collects the tombstones and raw rows of a raw read, in clustering order.
    fn of(parts: Vec<nosql_storage::read::Partition>, key: &PartitionKey) -> Stored {
        let mut out = Stored {
            partition: DeletionTime::LIVE,
            ranges: Vec::new(),
            rows: BTreeMap::new(),
            static_row: None,
        };
        for p in parts.into_iter().filter(|p| p.key == *key) {
            out.partition = out.partition.max(p.partition_deletion);
            if p.static_row.is_some() {
                out.static_row = p.static_row;
            }
            for r in p.rows {
                out.rows.insert(r.clustering.encoded().clone(), r);
            }
            let mut open: Option<(nosql_storage::Bound, DeletionTime)> = None;
            for m in &p.markers {
                if let (Some(d), Some((start, _))) = (m.close, open.take()) {
                    out.ranges.push(RangeTombstone {
                        start,
                        end: m.bound.clone(),
                        deletion: d,
                    });
                }
                if let Some(d) = m.open {
                    open = Some((m.bound.clone(), d));
                }
            }
        }
        out
    }

    fn at(&self, base: &Table, c: &Clustering) -> DeletionTime {
        let r = nosql_storage::tombstones::deletion_at(&*base.comparator, &self.ranges, c);
        self.partition.max(r)
    }
}

/// The deletion covering `c` after the whole write: the stored tombstones and those of every
/// update to the partition (the updates of a batch apply together, whatever their order).
fn covering_all(
    base: &Table,
    stored: &Stored,
    updates: &[&PartitionUpdate],
    c: &Clustering,
) -> DeletionTime {
    updates
        .iter()
        .fold(stored.at(base, c), |d, u| d.max(covering(base, u, c)))
}

/// Reads the rows `updates` touch and reconciles them with the updates.
pub async fn touch<'a>(
    db: &Arc<Database>,
    base: &'a Arc<Table>,
    key: PartitionKey,
    updates: &[&'a PartitionUpdate],
    now: i64,
) -> Result<Touched<'a>> {
    let cmp = base.comparator.clone();
    let mode = reconcile_mode(db);
    let deletes_all = updates.iter().any(|u| !u.deletion.is_live());
    let filter = if deletes_all {
        ClusteringFilter::all()
    } else {
        let mut slices = Vec::new();
        for u in updates {
            for r in &u.rows {
                slices.push(Slice::prefix(r.clustering.clone()));
            }
            for rt in &u.range_tombstones {
                slices.push(Slice::new(rt.start.clone(), rt.end.clone()));
            }
        }
        ClusteringFilter::Slices(slices)
    };
    let st = db.storage_table(base)?;
    let read = PartitionRead {
        key: key.clone(),
        clustering: filter,
        reversed: false,
        columns: ColumnFilter::all(),
        limits: Limits::default(),
        options: ReadOptions::at(now),
    };
    let raw = PartitionRead {
        options: ReadOptions {
            raw: true,
            ..ReadOptions::at(now)
        },
        ..read.clone()
    };
    let parts = st.read(read).partitions().await.map_err(Error::internal)?;
    let mut stored = Stored::of(
        st.read(raw).partitions().await.map_err(Error::internal)?,
        &key,
    );
    // A written row is merged with the row as stored (its tombstones shadow older cells of the
    // write), the first time the write touches it.
    let mut raw_rows = std::mem::take(&mut stored.rows);
    let mut raw_static = stored.static_row.take();
    let partition_deletion = updates
        .iter()
        .fold(stored.partition, |d, u| d.max(u.deletion));
    let mut existing: BTreeMap<Bytes, Row> = BTreeMap::new();
    let mut existing_static = None;
    for p in parts {
        if p.key != key {
            continue;
        }
        existing_static = p.static_row.filter(|r| !r.is_empty());
        for r in p.rows {
            existing.insert(r.clustering.encoded().clone(), r);
        }
    }
    // Merge the updates in order: each sees the previous ones.
    let mut merged: BTreeMap<Bytes, Row> = existing.clone();
    let mut merged_static = existing_static.clone();
    for u in updates {
        // Rows the update deletes without rewriting them.
        if !u.deletion.is_live() || !u.range_tombstones.is_empty() {
            let keys: Vec<Bytes> = merged.keys().cloned().collect();
            for k in keys {
                let Some(prev) = merged.remove(&k) else {
                    continue;
                };
                let c = prev.clustering.clone();
                let d = covering(base, u, &c);
                let m = if d.is_live() {
                    Some(prev)
                } else {
                    merge_rows(
                        &*cmp,
                        mode,
                        vec![prev, deleted_row(c, d)],
                        DeletionTime::LIVE,
                    )
                };
                if let Some(m) = m {
                    merged.insert(k, m);
                }
            }
        }
        if !u.deletion.is_live() {
            let del = deleted_row(Clustering::EMPTY, u.deletion);
            merged_static = match merged_static.take() {
                Some(prev) => merge_rows(&*cmp, mode, vec![prev, del], DeletionTime::LIVE),
                None => Some(del),
            };
        }
        if let Some(s) = &u.static_row {
            let mut s = s.clone();
            if partition_deletion.supersedes(&s.deletion.time) {
                s.deletion = nosql_storage::RowDeletion::regular(partition_deletion);
            }
            let prev = raw_static.take().or_else(|| merged_static.take());
            merged_static = match prev {
                Some(prev) => merge_rows(&*cmp, mode, vec![prev, s], DeletionTime::LIVE),
                None => Some(s),
            };
        }
        for r in &u.rows {
            let mut r = r.clone();
            let d = covering_all(base, &stored, updates, &r.clustering);
            if d.supersedes(&r.deletion.time) {
                r.deletion = nosql_storage::RowDeletion::regular(d);
            }
            let c = row_key(&merged, base, &r.clustering);
            let raw = raw_rows.remove(&row_key(&raw_rows, base, &r.clustering));
            let prev = match raw {
                Some(raw) => {
                    merged.remove(&c);
                    Some(raw)
                }
                None => merged.remove(&c),
            };
            let m = match prev {
                Some(prev) => merge_rows(&*cmp, mode, vec![prev, r], DeletionTime::LIVE),
                None => Some(r),
            };
            if let Some(m) = m {
                merged.insert(c, m);
            }
        }
    }
    Ok(Touched {
        base,
        key,
        updates: updates.to_vec(),
        existing_static,
        existing,
        merged_static,
        merged,
        now,
    })
}

/// The key of the row with clustering `c` in a touched map: its own encoding, or that of a row
/// whose clustering compares equal (`1.5` and `1.50` are one decimal row; the row keeps the
/// bytes it was written with).
pub fn row_key(rows: &BTreeMap<Bytes, Row>, base: &Table, c: &Clustering) -> Bytes {
    let own = c.encoded();
    if rows.contains_key(own) {
        return own.clone();
    }
    let cmp = &*base.comparator;
    rows.iter()
        .find(|(_, r)| nosql_storage::compare::compare_clustering(cmp, &r.clustering, c).is_eq())
        .map_or_else(|| own.clone(), |(k, _)| k.clone())
}

impl Touched<'_> {
    /// The row before the write that `c` names.
    pub fn existing_row(&self, c: &Clustering) -> Option<&Row> {
        self.existing.get(&row_key(&self.existing, self.base, c))
    }

    /// The row after the write that `c` names.
    pub fn merged_row(&self, c: &Clustering) -> Option<&Row> {
        self.merged.get(&row_key(&self.merged, self.base, c))
    }
}

/// The view updates of one view for one touched base partition.
pub fn generate(
    db: &Database,
    spec: &super::ViewSpec,
    t: &Touched,
) -> Result<Vec<PartitionUpdate>> {
    let key_parts: Vec<Bytes> = crate::storage::key_components(t.base, &t.key.key)
        .into_iter()
        .map(Option::unwrap_or_default)
        .collect();
    let bp = BasePartition {
        table: t.base,
        key: &key_parts,
        token: t.key.token,
        now: t.now,
        dialect: db.dialect(),
    };
    let mut g = Generator::new(spec, &bp);
    if spec.static_target {
        if let Some(m) = &t.merged_static
            && t.merged_static != t.existing_static
        {
            g.add(t.existing_static.as_ref(), m)?;
        }
    } else {
        for (c, m) in &t.merged {
            let e = t.existing.get(c);
            if e == Some(m) {
                continue;
            }
            g.add(e, m)?;
        }
    }
    Ok(g.finish())
}

/// The (table id, update) pairs of a view's updates.
pub fn tagged(
    spec: &super::ViewSpec,
    ups: Vec<PartitionUpdate>,
) -> Vec<(TableId, PartitionUpdate)> {
    let id = spec.view.table_id();
    ups.into_iter().map(|u| (id, u)).collect()
}
