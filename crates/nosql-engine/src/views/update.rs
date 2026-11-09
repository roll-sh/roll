//! View updates from base updates: a port of Cassandra's `ViewUpdateGenerator` and
//! `TableViews.generateViewUpdates` (Apache-2.0), extended to ScyllaDB's index views (computed
//! `idx_token`, one view row per collection element).
//!
//! For every base row a write touches, the generator gets the row as it was (`existing`, live
//! data only) and as it will be (`merged`: existing reconciled with the update, tombstones
//! included) and decides whether the view entry is created, updated, moved (the view key
//! changed) or deleted. Old entries are removed with an *expired row marker* at the timestamp of
//! what removed them (`LivenessInfo.EXPIRED_LIVENESS_TTL`), so a later base write with a newer
//! timestamp revives the entry while an older one can't; view tables use strict liveness (a row
//! is live only through its marker).

use std::collections::BTreeMap;

use bytes::Bytes;
use nosql_storage::{
    Cell, Clustering, ColumnData, DeletionTime, LivenessInfo, PartitionKey, PartitionUpdate, Row,
    RowDeletion,
};
use nosql_types::{CqlType, Dialect};

use super::spec::{KeySource, ViewSpec};
use crate::error::Result;
use crate::index::TargetKind;
use crate::schema::{ColumnKind, Table};

/// `ViewUpdateGenerator.UpdateAction`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    None,
    NewEntry,
    DeleteOld,
    UpdateExisting,
    SwitchEntry,
}

/// The base partition a generator works on.
pub struct BasePartition<'a> {
    pub table: &'a Table,
    /// Partition key component values.
    pub key: &'a [Bytes],
    pub token: i64,
    pub now: i64,
    pub dialect: Dialect,
}

impl BasePartition<'_> {
    /// A live simple cell of base column `ci` (dropped incarnations ignored).
    fn cell<'r>(&self, row: &'r Row, ci: usize) -> Option<&'r Cell> {
        let col = &self.table.columns[ci];
        let cell = row.cell(col.id)?;
        let dropped = self.table.dropped_before(&col.name);
        (cell.is_live(self.now) && dropped.is_none_or(|d| cell.timestamp > d)).then_some(cell)
    }

    /// The value of base column `ci` in `row` (key columns from the key, others live cells).
    pub fn value(&self, row: &Row, ci: usize) -> Option<Bytes> {
        let col = &self.table.columns[ci];
        match col.kind {
            ColumnKind::PartitionKey => self.key.get(col.position as usize).cloned(),
            ColumnKind::Clustering => row.clustering.get(col.position as usize),
            _ => crate::storage::column_value(self.table, col, row, self.now, None),
        }
    }

    /// The live value of an Alternator key attribute in `row`: the `:attrs` element cell with
    /// its type tag removed (`None` when missing, of another type, or empty).
    fn extracted(&self, row: &Row, x: &super::spec::Extract) -> Option<Cell> {
        let col = &self.table.columns[x.column];
        let Some(ColumnData::Complex {
            deletion, cells, ..
        }) = row.column(col.id)
        else {
            return None;
        };
        let c = cells
            .iter()
            .find(|c| c.path.as_deref() == Some(&x.path[..]))?;
        if !c.is_live(self.now)
            || deletion.deletes(c.timestamp)
            || row.deletion.time.deletes(c.timestamp)
            || c.value.len() < 2
            || c.value[0] != x.tag
        {
            return None;
        }
        let mut cell = c.clone();
        cell.value = c.value.slice(1..);
        cell.path = None;
        Some(cell)
    }

    fn known(&self, id: u32) -> bool {
        self.table
            .columns
            .iter()
            .any(|c| c.id == id && !c.is_primary_key())
    }

    fn base_index(&self, id: u32) -> Option<usize> {
        self.table.columns.iter().position(|c| c.id == id)
    }
}

/// An element of an indexed collection: the indexed value, its cell path (for
/// `keys_for_values_idx`) and the cell it comes from.
struct Element {
    value: Bytes,
    path: Option<Bytes>,
    cell: Cell,
}

/// Builds the view updates of one base partition for one view.
pub struct Generator<'a> {
    spec: &'a ViewSpec,
    base: &'a BasePartition<'a>,
    updates: BTreeMap<PartitionKey, PartitionUpdate>,
}

impl<'a> Generator<'a> {
    pub fn new(spec: &'a ViewSpec, base: &'a BasePartition<'a>) -> Self {
        Generator {
            spec,
            base,
            updates: BTreeMap::new(),
        }
    }

    /// The view updates (one per view partition).
    pub fn finish(self) -> Vec<PartitionUpdate> {
        self.updates.into_values().collect()
    }

    /// `addBaseTableUpdate`: a base row (or the static row, for a static column index) before and
    /// after the update.
    pub fn add(&mut self, existing: Option<&Row>, merged: &Row) -> Result<()> {
        if self.spec.elements.is_some() {
            return self.add_elements(existing, merged);
        }
        match self.action(existing, merged) {
            Action::None => Ok(()),
            Action::NewEntry => self.create(merged),
            Action::DeleteOld => match existing {
                Some(e) => self.delete_old(e, merged),
                None => Ok(()),
            },
            Action::UpdateExisting => match existing {
                Some(e) => self.update_entry(e, merged),
                None => self.create(merged),
            },
            Action::SwitchEntry => {
                self.create(merged)?;
                match existing {
                    Some(e) => self.delete_old(e, merged),
                    None => Ok(()),
                }
            }
        }
    }

    /// Every Alternator key attribute of `row` (`None` if one is missing).
    fn extracted_cells(&self, row: &Row) -> Option<Vec<Cell>> {
        self.spec
            .extracted
            .iter()
            .map(|x| self.base.extracted(row, x))
            .collect()
    }

    fn action(&self, existing: Option<&Row>, merged: &Row) -> Action {
        let b = self.base;
        if !self.spec.extracted.is_empty() {
            let before = existing.and_then(|e| self.extracted_cells(e));
            let after = self.extracted_cells(merged);
            return match (before, after) {
                (None, None) => Action::None,
                (None, Some(_)) => Action::NewEntry,
                (Some(_), None) => Action::DeleteOld,
                (Some(x), Some(y)) if x.iter().zip(&y).all(|(a, b)| a.value == b.value) => {
                    Action::UpdateExisting
                }
                (Some(_), Some(_)) => Action::SwitchEntry,
            };
        }
        let Some(ci) = self.spec.regular_in_pk else {
            let had = existing.is_some_and(|e| e.has_live_data(b.now, false));
            let has = merged.has_live_data(b.now, false);
            return match (had, has) {
                (true, true) => Action::UpdateExisting,
                (true, false) => Action::DeleteOld,
                (false, true) => Action::NewEntry,
                (false, false) => Action::None,
            };
        };
        let before = existing.and_then(|e| b.cell(e, ci));
        let after = b.cell(merged, ci);
        match (before, after) {
            (None, None) => Action::None,
            (None, Some(_)) => Action::NewEntry,
            (Some(_), None) => Action::DeleteOld,
            (Some(x), Some(y)) if x == y => Action::UpdateExisting,
            (Some(x), Some(y)) => {
                let ty = &b.table.columns[ci].ty;
                if nosql_types::compare(ty, &x.value, &y.value).is_eq() {
                    Action::UpdateExisting
                } else {
                    Action::SwitchEntry
                }
            }
        }
    }

    fn matches(&self, row: &Row) -> bool {
        let Some(f) = &self.spec.filter else {
            return true;
        };
        let b = self.base;
        f.matches(b.table, b.token, &|ci| b.value(row, ci))
    }

    /// The view key of a base row (`None` if a key column has no value).
    fn key_of(
        &self,
        row: &Row,
        element: Option<&Element>,
    ) -> Result<Option<(PartitionKey, Clustering)>> {
        let b = self.base;
        let view = &self.spec.view;
        let mut parts: Vec<Bytes> = Vec::with_capacity(self.spec.key.len());
        for k in &self.spec.key {
            let v = match k {
                KeySource::Base(ci) => b.value(row, *ci),
                KeySource::Token => Some(Bytes::copy_from_slice(&b.token.to_be_bytes())),
                KeySource::Element => element.map(|e| e.value.clone()),
                KeySource::Path => element.and_then(|e| e.path.clone()),
                KeySource::Extracted(i) => self
                    .spec
                    .extracted
                    .get(*i)
                    .and_then(|x| b.extracted(row, x))
                    .map(|c| c.value),
            };
            match v {
                Some(v) => parts.push(v),
                None => return Ok(None),
            }
        }
        let npk = view.partition_key.len();
        // A value that can't be a partition key (too long) has no entry; index reads with such a
        // value scan the base table instead. An empty one is a key.
        let Ok(pk) = crate::storage::view_partition_key(&parts[..npk], b.dialect) else {
            return Ok(None);
        };
        // Nor does a clustering value over the 64 KiB a key component can hold.
        if parts[npk..].iter().any(|p| p.len() > u16::MAX as usize) {
            return Ok(None);
        }
        Ok(Some((pk, Clustering::new(parts[npk..].iter()))))
    }

    fn submit(&mut self, key: PartitionKey, row: Row) {
        if row.is_empty() {
            return;
        }
        self.updates
            .entry(key.clone())
            .or_insert_with(|| PartitionUpdate::new(key))
            .rows
            .push(row);
    }

    /// `createEntry`.
    fn create(&mut self, merged: &Row) -> Result<()> {
        if !self.matches(merged) {
            return Ok(());
        }
        let Some((key, clustering)) = self.key_of(merged, None)? else {
            return Ok(());
        };
        let mut row = Row::new(clustering)
            .with_liveness(self.liveness_for_entry(merged))
            .with_deletion(merged.deletion);
        for data in &merged.columns {
            if let Some(vid) = self.view_column(data.column()) {
                row.columns.push(with_column(data, vid));
            }
        }
        self.submit(key, row);
        Ok(())
    }

    /// `updateEntry`.
    fn update_entry(&mut self, existing: &Row, merged: &Row) -> Result<()> {
        if !self.matches(existing) {
            return self.create(merged);
        }
        if !self.matches(merged) {
            return self.delete_old_internal(existing, merged);
        }
        let Some((key, clustering)) = self.key_of(merged, None)? else {
            return Ok(());
        };
        let mut row = Row::new(clustering)
            .with_liveness(self.liveness_for_entry(merged))
            .with_deletion(merged.deletion);
        self.add_different_cells(&mut row, existing, merged);
        self.submit(key, row);
        Ok(())
    }

    /// `deleteOldEntry`.
    fn delete_old(&mut self, existing: &Row, merged: &Row) -> Result<()> {
        if !self.matches(existing) {
            return Ok(());
        }
        self.delete_old_internal(existing, merged)
    }

    /// `deleteOldEntryInternal`: an expired marker at the deletion's timestamp (so the entry can
    /// coexist with a row tombstone), the base row deletion, and the changed cells.
    fn delete_old_internal(&mut self, existing: &Row, merged: &Row) -> Result<()> {
        let Some((key, clustering)) = self.key_of(existing, None)? else {
            return Ok(());
        };
        let ts = self.timestamp_for_deletion(existing, merged);
        let mut row = Row::new(clustering);
        if ts > merged.deletion.time.marked_at {
            row.liveness = LivenessInfo::expired(ts, self.base.now);
        }
        row.deletion = merged.deletion;
        self.add_different_cells(&mut row, existing, merged);
        self.submit(key, row);
        Ok(())
    }

    fn view_column(&self, base_id: u32) -> Option<u32> {
        let bi = self.base.base_index(base_id)?;
        self.spec
            .copied
            .iter()
            .find(|(_, b)| *b == bi)
            .map(|(v, _)| *v)
    }

    /// `addDifferentCells`: the cells of `merged` the update changed.
    fn add_different_cells(&self, row: &mut Row, existing: &Row, merged: &Row) {
        for data in &merged.columns {
            let Some(vid) = self.view_column(data.column()) else {
                continue;
            };
            match (data, existing.column(data.column())) {
                (_, None) => row.columns.push(with_column(data, vid)),
                (d, Some(e)) if d == e => {}
                (
                    ColumnData::Complex {
                        deletion, cells, ..
                    },
                    Some(ColumnData::Complex {
                        deletion: ed,
                        cells: ec,
                        ..
                    }),
                ) => {
                    let del = if deletion.supersedes(ed) {
                        *deletion
                    } else {
                        DeletionTime::LIVE
                    };
                    let changed: Vec<Cell> =
                        cells.iter().filter(|c| !ec.contains(c)).cloned().collect();
                    if !del.is_live() || !changed.is_empty() {
                        row.columns.push(ColumnData::complex(vid, del, changed));
                    }
                }
                (d, Some(_)) => row.columns.push(with_column(d, vid)),
            }
        }
    }

    /// `computeLivenessInfoForEntry`.
    fn liveness_for_entry(&self, row: &Row) -> LivenessInfo {
        let b = self.base;
        let base_liveness = row.liveness;
        if !self.spec.extracted.is_empty() {
            // The entry lives as long as its key attributes (the newest one decides).
            return match self.extracted_cells(row) {
                Some(cells) => cells
                    .iter()
                    .max_by_key(|c| c.timestamp)
                    .map_or(base_liveness, cell_liveness),
                None => base_liveness,
            };
        }
        let Some(ci) = self.spec.regular_in_pk else {
            if self.spec.include_all {
                return base_liveness;
            }
            let mut timestamp = base_liveness.timestamp;
            let mut non_expiring = false;
            let mut biggest: Option<&Cell> = None;
            for data in &row.columns {
                if !b.known(data.column()) || self.selected(data.column()) {
                    continue;
                }
                let cells: &[Cell] = match data {
                    ColumnData::Simple { cell, .. } => std::slice::from_ref(cell),
                    ColumnData::Complex { cells, .. } => cells,
                };
                for c in cells {
                    if !c.is_live(b.now) {
                        continue;
                    }
                    timestamp = timestamp.max(c.timestamp);
                    if !c.is_expiring() {
                        non_expiring = true;
                    } else if biggest.is_none_or(|x| c.local_deletion_time > x.local_deletion_time)
                    {
                        biggest = Some(c);
                    }
                }
            }
            if base_liveness.is_live(b.now) && !base_liveness.is_expiring() {
                return LivenessInfo::live(timestamp);
            }
            if non_expiring {
                return LivenessInfo::live(timestamp);
            }
            let Some(big) = biggest else {
                return base_liveness;
            };
            if big.local_deletion_time > base_liveness.local_expiration_time
                || !base_liveness.is_live(b.now)
            {
                return LivenessInfo::expiring(timestamp, big.ttl, big.local_deletion_time);
            }
            return base_liveness;
        };
        match b.cell(row, ci) {
            Some(c) => cell_liveness(c),
            None => base_liveness,
        }
    }

    fn selected(&self, base_id: u32) -> bool {
        self.base
            .base_index(base_id)
            .is_some_and(|bi| self.spec.selected[bi])
    }

    /// `computeTimestampForEntryDeletion`.
    fn timestamp_for_deletion(&self, existing: &Row, merged: &Row) -> i64 {
        let b = self.base;
        let deletion = merged.deletion.time;
        if !self.spec.extracted.is_empty() {
            let before = self
                .extracted_cells(existing)
                .and_then(|cells| cells.iter().map(|c| c.timestamp).max())
                .unwrap_or(i64::MIN);
            // What removed an attribute: a newer element cell or the map's deletion.
            let mut ts = if deletion.deletes(before) {
                deletion.marked_at
            } else {
                before
            };
            for x in &self.spec.extracted {
                let col = &b.table.columns[x.column];
                if let Some(ColumnData::Complex {
                    deletion: cd,
                    cells,
                    ..
                }) = merged.column(col.id)
                {
                    if cd.deletes(before) {
                        ts = ts.max(cd.marked_at);
                    }
                    if let Some(c) = cells
                        .iter()
                        .find(|c| c.path.as_deref() == Some(&x.path[..]))
                    {
                        ts = ts.max(c.timestamp);
                    }
                }
            }
            return ts;
        }
        if let Some(ci) = self.spec.regular_in_pk {
            let col = &b.table.columns[ci];
            let before = existing
                .cell(col.id)
                .map(|c| c.timestamp)
                .unwrap_or(i64::MIN);
            return if deletion.deletes(before) {
                deletion.marked_at
            } else {
                before
            };
        }
        let mut ts = deletion.marked_at.max(existing.liveness.timestamp);
        if self.spec.include_all {
            return ts;
        }
        for data in &existing.columns {
            if self.selected(data.column()) {
                continue;
            }
            ts = ts.max(column_max_timestamp(data));
        }
        // Cassandra's existing row has the tombstones of earlier deletions; ours is live data,
        // so the tombstones the update writes stand in for them.
        for data in &merged.columns {
            if !self.selected(data.column()) && b.known(data.column()) {
                ts = ts.max(column_max_timestamp(data));
            }
        }
        ts
    }

    // ---- collection indexes ------------------------------------------------------------------

    /// The live elements of the indexed collection in `row`.
    fn elements(&self, row: &Row, ci: usize, kind: TargetKind) -> Vec<Element> {
        let b = self.base;
        let col = &b.table.columns[ci];
        let dropped = b.table.dropped_before(&col.name);
        let Some(ColumnData::Complex {
            deletion, cells, ..
        }) = row.column(col.id)
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for c in cells {
            if !c.is_live(b.now)
                || deletion.deletes(c.timestamp)
                || row.deletion.time.deletes(c.timestamp)
                || dropped.is_some_and(|d| c.timestamp <= d)
            {
                continue;
            }
            let path = c.path.clone().unwrap_or_default();
            let (value, p) = match (&col.ty, kind) {
                (CqlType::Set { .. }, _) | (CqlType::Map { .. }, TargetKind::Keys) => (path, None),
                (CqlType::Map { .. }, TargetKind::Entries) => (
                    nosql_types::pack_fields([Some(&path[..]), Some(&c.value[..])]),
                    None,
                ),
                _ => (c.value.clone(), Some(path)),
            };
            out.push(Element {
                value,
                path: p,
                cell: c.clone(),
            });
        }
        out
    }

    /// One view row per collection element: new elements get an entry with the element cell's
    /// liveness, removed ones an expired marker at the timestamp of what removed them.
    fn add_elements(&mut self, existing: Option<&Row>, merged: &Row) -> Result<()> {
        let Some((ci, kind)) = self.spec.elements else {
            return Ok(());
        };
        let col_id = self.base.table.columns[ci].id;
        let before = existing
            .map(|e| self.elements(e, ci, kind))
            .unwrap_or_default();
        let after = self.elements(merged, ci, kind);
        let mut old: BTreeMap<(PartitionKey, Bytes), (Clustering, Cell)> = BTreeMap::new();
        if let Some(e) = existing {
            for el in &before {
                if let Some((pk, c)) = self.key_of(e, Some(el))? {
                    old.insert((pk, c.encoded().clone()), (c, el.cell.clone()));
                }
            }
        }
        let mut new: BTreeMap<(PartitionKey, Bytes), (Clustering, Cell)> = BTreeMap::new();
        for el in &after {
            if let Some((pk, c)) = self.key_of(merged, Some(el))? {
                new.insert((pk, c.encoded().clone()), (c, el.cell.clone()));
            }
        }
        for (k, (c, cell)) in &new {
            if old.get(k).is_some_and(|(_, o)| o == cell) {
                continue;
            }
            let row = Row::new(c.clone()).with_liveness(cell_liveness(cell));
            self.submit(k.0.clone(), row);
        }
        for (k, (c, cell)) in &old {
            if new.contains_key(k) {
                continue;
            }
            // What removed it: a row / partition / range deletion, the collection's deletion, or
            // a tombstone (or newer cell) at its path.
            let mut ts = cell.timestamp;
            if merged.deletion.time.deletes(cell.timestamp) {
                ts = ts.max(merged.deletion.time.marked_at);
            }
            if let Some(ColumnData::Complex {
                deletion, cells, ..
            }) = merged.column(col_id)
            {
                if deletion.deletes(cell.timestamp) {
                    ts = ts.max(deletion.marked_at);
                }
                if let Some(c) = cells.iter().find(|c| c.path == cell.path) {
                    ts = ts.max(c.timestamp);
                }
            }
            let row = Row::new(c.clone())
                .with_liveness(LivenessInfo::expired(ts, self.base.now))
                .with_deletion(merged.deletion);
            self.submit(k.0.clone(), row);
        }
        Ok(())
    }
}

/// The liveness of an entry that lives as long as `cell`.
fn cell_liveness(c: &Cell) -> LivenessInfo {
    if c.is_expiring() {
        LivenessInfo::expiring(c.timestamp, c.ttl, c.local_deletion_time)
    } else {
        LivenessInfo::live(c.timestamp)
    }
}

fn column_max_timestamp(d: &ColumnData) -> i64 {
    match d {
        ColumnData::Simple { cell, .. } => cell.timestamp,
        ColumnData::Complex {
            deletion, cells, ..
        } => cells
            .iter()
            .map(|c| c.timestamp)
            .fold(deletion.marked_at, i64::max),
    }
}

/// `data` re-targeted to the view column `id`.
fn with_column(data: &ColumnData, id: u32) -> ColumnData {
    match data {
        ColumnData::Simple { cell, .. } => ColumnData::simple(id, cell.clone()),
        ColumnData::Complex {
            deletion, cells, ..
        } => ColumnData::complex(id, *deletion, cells.clone()),
    }
}

/// A deleted empty row (`BTreeRow.emptyDeletedRow`): what a partition or range deletion does to
/// a row.
pub fn deleted_row(clustering: Clustering, deletion: DeletionTime) -> Row {
    Row::new(clustering).with_deletion(RowDeletion::regular(deletion))
}
