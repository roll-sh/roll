//! Reconciliation: which version of a cell, row marker or deletion wins, and how rows merge.
//!
//! Closely follows Apache Cassandra 5.0 (Apache-2.0): `Cells.reconcile` / `resolveRegular` /
//! `resolveCounter`, `Row.Merger`, and `BTreeRow.purge` / `AbstractCell.purge`.
//!
//! The highest timestamp always wins. Ties break by the dialect's rule ([`Reconcile`]).

use crate::compare::{ClusteringComparator, compare_paths};
use crate::model::*;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// Which product's tie-break rules decide between cells with equal timestamps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reconcile {
    /// ScyllaDB (the default dialect).
    #[default]
    Scylla,
    /// Apache Cassandra 5.0.
    Cassandra,
}

/// Reconcile two versions of the same cell (same row, column and path).
pub fn reconcile(mode: Reconcile, a: Cell, b: Cell) -> Cell {
    let a_counter = a.is_counter() && !a.is_tombstone();
    let b_counter = b.is_counter() && !b.is_tombstone();
    if a_counter || b_counter {
        return resolve_counter(a, b);
    }
    let a_wins = match mode {
        Reconcile::Cassandra => cassandra_regular(&a, &b) != Ordering::Less,
        Reconcile::Scylla => scylla_regular(&a, &b) != Ordering::Less,
    };
    if a_wins { a } else { b }
}

/// `Cells.resolveRegular` (Cassandra 5.0). `Greater`/`Equal` = `a` wins.
pub fn cassandra_regular(a: &Cell, b: &Cell) -> Ordering {
    if a.timestamp != b.timestamp {
        return a.timestamp.cmp(&b.timestamp);
    }
    let a_dead_or_expiring = a.local_deletion_time != NO_DELETION_TIME;
    let b_dead_or_expiring = b.local_deletion_time != NO_DELETION_TIME;
    if a_dead_or_expiring || b_dead_or_expiring {
        // Tombstones and expiring cells (future tombstones) beat live cells (CASSANDRA-14592).
        if a_dead_or_expiring != b_dead_or_expiring {
            return if a_dead_or_expiring { Ordering::Greater } else { Ordering::Less };
        }
        // Then pure tombstones beat expiring cells.
        let a_tomb = !a.is_expiring();
        let b_tomb = !b.is_expiring();
        if a_tomb != b_tomb {
            return if a_tomb { Ordering::Greater } else { Ordering::Less };
        }
        // Both tombstones or both expiring: the later deletion / expiration wins.
        if a.local_deletion_time != b.local_deletion_time {
            return a.local_deletion_time.cmp(&b.local_deletion_time);
        }
        // Same expiration: the lower TTL was written later.
        if !a_tomb && a.ttl != b.ttl {
            return b.ttl.cmp(&a.ttl);
        }
    }
    a.value.as_ref().cmp(b.value.as_ref())
}

/// ScyllaDB's documented order for equal timestamps (`docs/cql/dml.rst`, "Update ordering"):
/// tombstone over live cell, then the expiration, then the TTL, then the value.
///
/// The two details the documentation leaves open were checked against ScyllaDB 2026.3 and
/// Cassandra 5.0.9 by black-box writes (`USING TIMESTAMP` collisions, see
/// `tests::scylla_tiebreak_pinned_by_compat`): for equal expirations the lower TTL (the later
/// write) wins, and values compare as unsigned bytes, the greater winning. Whether an
/// already-expired cell counts as a tombstone isn't observable through reads (it loses or
/// wins only against cells that are dead either way), so it is evaluated without the clock,
/// as in Cassandra. The result equals [`cassandra_regular`]; they're kept apart so a dialect
/// difference found later changes one function.
pub fn scylla_regular(a: &Cell, b: &Cell) -> Ordering {
    if a.timestamp != b.timestamp {
        return a.timestamp.cmp(&b.timestamp);
    }
    let a_dead = a.is_tombstone();
    let b_dead = b.is_tombstone();
    if a_dead != b_dead {
        return if a_dead { Ordering::Greater } else { Ordering::Less };
    }
    if a_dead {
        // Both deleted: the later deletion wins.
        if a.local_deletion_time != b.local_deletion_time {
            return a.local_deletion_time.cmp(&b.local_deletion_time);
        }
    } else {
        if a.is_expiring() != b.is_expiring() {
            return if a.is_expiring() { Ordering::Greater } else { Ordering::Less };
        }
        if a.is_expiring() {
            if a.local_deletion_time != b.local_deletion_time {
                return a.local_deletion_time.cmp(&b.local_deletion_time);
            }
            if a.ttl != b.ttl {
                return b.ttl.cmp(&a.ttl);
            }
        }
    }
    a.value.as_ref().cmp(b.value.as_ref())
}

/// Counters: a tombstone always wins (CASSANDRA-7346); otherwise the highest clock (the single
/// writer's latest update), keeping the newest timestamp.
fn resolve_counter(a: Cell, b: Cell) -> Cell {
    if a.is_tombstone() || b.is_tombstone() {
        return if a.is_tombstone() { a } else { b };
    }
    let ts = a.timestamp.max(b.timestamp);
    let ka = a.counter_parts().map_or((i64::MIN, i64::MIN), |p| p);
    let kb = b.counter_parts().map_or((i64::MIN, i64::MIN), |p| p);
    let mut w = if ka >= kb { a } else { b };
    w.timestamp = ts;
    w
}

/// The newer of two row deletions / partition deletions.
pub fn max_deletion(a: DeletionTime, b: DeletionTime) -> DeletionTime {
    a.max(b)
}

/// Merge versions of one row (`Row.Merger.merge`). `active` is the deletion covering the row
/// (partition deletion or range tombstone); everything it deletes is dropped. Returns `None`
/// when nothing is left.
pub fn merge_rows(cmp: &dyn ClusteringComparator, mode: Reconcile, mut rows: Vec<Row>, active: DeletionTime) -> Option<Row> {
    if rows.is_empty() {
        return None;
    }
    if rows.len() == 1 && active.is_live() {
        return rows.pop();
    }
    let clustering = rows[0].clustering.clone();
    let mut liveness = LivenessInfo::EMPTY;
    let mut deletion = RowDeletion::LIVE;
    for r in &rows {
        if r.liveness.supersedes(&liveness) {
            liveness = r.liveness;
        }
        if r.deletion.supersedes(&deletion) {
            deletion = r.deletion;
        }
    }
    if deletion.is_shadowed_by(&liveness) {
        deletion = RowDeletion::LIVE;
    }
    let mut active = active;
    if deletion.time.supersedes(&active) {
        active = deletion.time;
    } else {
        deletion = RowDeletion::LIVE;
    }
    if active.deletes(liveness.timestamp) {
        liveness = LivenessInfo::EMPTY;
    }
    let columns = merge_columns(cmp, mode, rows.into_iter().map(|r| r.columns).collect(), active);
    let row = Row { clustering, liveness, deletion, columns };
    if row.is_empty() { None } else { Some(row) }
}

/// Merge column data of several versions of a row.
fn merge_columns(cmp: &dyn ClusteringComparator, mode: Reconcile, versions: Vec<Vec<ColumnData>>, active: DeletionTime) -> Vec<ColumnData> {
    let total: usize = versions.iter().map(|v| v.len()).sum();
    let mut all: Vec<ColumnData> = Vec::with_capacity(total);
    for v in versions {
        all.extend(v);
    }
    // Stable: versions of a column stay in input order (reconcile is commutative anyway).
    all.sort_by_key(|c| c.column());
    let mut out = Vec::with_capacity(all.len());
    let mut it = all.into_iter().peekable();
    while let Some(first) = it.next() {
        let column = first.column();
        let mut group = vec![first];
        while it.peek().is_some_and(|c| c.column() == column) {
            group.push(it.next().expect("peeked"));
        }
        if let Some(merged) = merge_column(cmp, mode, column, group, active) {
            out.push(merged);
        }
    }
    out
}

fn merge_column(cmp: &dyn ClusteringComparator, mode: Reconcile, column: ColumnId, group: Vec<ColumnData>, active: DeletionTime) -> Option<ColumnData> {
    // A column is simple or complex for all versions; a mismatch (schema bug) keeps the kind of
    // the first version.
    match &group[0] {
        ColumnData::Simple { .. } => {
            let mut merged: Option<Cell> = None;
            for d in group {
                let ColumnData::Simple { cell, .. } = d else { continue };
                if active.deletes(cell.timestamp) {
                    continue;
                }
                merged = Some(match merged {
                    None => cell,
                    Some(m) => reconcile(mode, m, cell),
                });
            }
            merged.map(|cell| ColumnData::Simple { column, cell })
        }
        ColumnData::Complex { .. } => {
            let mut deletion = DeletionTime::LIVE;
            let mut cells: Vec<Cell> = Vec::new();
            let single = group.len() == 1;
            for d in group {
                let ColumnData::Complex { deletion: d, cells: c, .. } = d else { continue };
                if d.supersedes(&deletion) {
                    deletion = d;
                }
                cells.extend(c);
            }
            let cell_active = if deletion.supersedes(&active) {
                deletion
            } else {
                deletion = DeletionTime::LIVE;
                active
            };
            if !single {
                cells.sort_by(|a, b| compare_paths(cmp, column, a.path.as_deref(), b.path.as_deref()));
            }
            let mut out: Vec<Cell> = Vec::with_capacity(cells.len());
            for c in cells {
                if cell_active.deletes(c.timestamp) {
                    continue;
                }
                match out.last_mut() {
                    Some(last) if compare_paths(cmp, column, last.path.as_deref(), c.path.as_deref()) == Ordering::Equal => {
                        let prev = std::mem::replace(last, Cell::tombstone(0, 0));
                        *last = reconcile(mode, prev, c);
                    }
                    _ => out.push(c),
                }
            }
            if out.is_empty() && deletion.is_live() {
                None
            } else {
                Some(ColumnData::Complex { column, deletion, cells: out })
            }
        }
    }
}

/// Decides whether a tombstone (`timestamp`, `local_deletion_time`) may be dropped
/// (`DeletionPurger`).
pub trait Purger {
    fn should_purge(&self, timestamp: i64, local_deletion_time: i64) -> bool;

    fn should_purge_deletion(&self, d: &DeletionTime) -> bool {
        !d.is_live() && self.should_purge(d.marked_at, d.local_deletion_time)
    }

    fn should_purge_liveness(&self, l: &LivenessInfo, now: i64) -> bool {
        !l.is_live(now) && self.should_purge(l.timestamp, l.local_expiration_time)
    }

    /// A row deletion: shadowable ones (view rows) only shadow row markers.
    fn should_purge_row_deletion(&self, d: &RowDeletion) -> bool {
        self.should_purge_deletion(&d.time)
    }

    /// A row marker that isn't live (an expired TTL, or a view's expired-liveness deletion).
    fn should_purge_marker(&self, l: &LivenessInfo, now: i64) -> bool {
        self.should_purge_liveness(l, now)
    }
}

/// Purge nothing.
pub struct NeverPurge;

impl Purger for NeverPurge {
    fn should_purge(&self, _: i64, _: i64) -> bool {
        false
    }
}

/// Purge tombstones older than `gc_before` (seconds) whose timestamp is below
/// `max_purgeable` (the oldest data that isn't part of the merge and could be shadowed).
/// Shadowable row deletions only shadow row markers, so they check `max_purgeable_shadowable`
/// (the oldest such row marker; ScyllaDB's fix for its issue #20424).
#[derive(Clone, Copy, Debug)]
pub struct GcPurger {
    pub gc_before: i64,
    pub max_purgeable: i64,
    pub max_purgeable_shadowable: i64,
    /// ScyllaDB dates an expired TTL'd row marker at its write (expiry - TTL), as it does
    /// expired cells; Cassandra at its expiry.
    pub marker_dated_at_write: bool,
}

impl Purger for GcPurger {
    fn should_purge(&self, timestamp: i64, local_deletion_time: i64) -> bool {
        local_deletion_time < self.gc_before && timestamp < self.max_purgeable
    }

    fn should_purge_marker(&self, l: &LivenessInfo, now: i64) -> bool {
        if l.is_empty() || l.is_live(now) {
            return false;
        }
        // A view's expired-liveness deletion only shadows row markers, as a shadowable one.
        if l.ttl == EXPIRED_LIVENESS_TTL {
            return l.local_expiration_time < self.gc_before
                && l.timestamp < self.max_purgeable_shadowable;
        }
        let dated = if self.marker_dated_at_write && l.ttl > 0 {
            l.local_expiration_time.saturating_sub(l.ttl as i64)
        } else {
            l.local_expiration_time
        };
        self.should_purge(l.timestamp, dated)
    }

    fn should_purge_row_deletion(&self, d: &RowDeletion) -> bool {
        if !d.shadowable {
            return self.should_purge_deletion(&d.time);
        }
        !d.time.is_live()
            && d.time.local_deletion_time < self.gc_before
            && d.time.marked_at < self.max_purgeable_shadowable
    }
}

/// `AbstractCell.purge`: drop purgeable tombstones, and turn expired cells into tombstones
/// (dated at the write: expiration - TTL), which may themselves be purgeable.
pub fn purge_cell(cell: Cell, p: &dyn Purger, now: i64) -> Option<Cell> {
    if cell.is_live(now) {
        return Some(cell);
    }
    if p.should_purge(cell.timestamp, cell.local_deletion_time) {
        return None;
    }
    if cell.is_expiring() {
        let t = Cell {
            timestamp: cell.timestamp,
            ttl: NO_TTL,
            flags: cell.flags,
            local_deletion_time: cell.local_deletion_time.saturating_sub(cell.ttl as i64),
            value: bytes::Bytes::new(),
            path: cell.path,
        };
        if p.should_purge(t.timestamp, t.local_deletion_time) {
            return None;
        }
        return Some(t);
    }
    Some(cell)
}

/// `BTreeRow.purge`.
pub fn purge_row(mut row: Row, p: &dyn Purger, now: i64, strict_liveness: bool) -> Option<Row> {
    if p.should_purge_marker(&row.liveness, now) {
        row.liveness = LivenessInfo::EMPTY;
    }
    if p.should_purge_row_deletion(&row.deletion) {
        row.deletion = RowDeletion::LIVE;
    }
    if strict_liveness && row.deletion.is_live() && row.liveness.is_empty() {
        return None;
    }
    let mut cols = Vec::with_capacity(row.columns.len());
    for c in row.columns {
        match c {
            ColumnData::Simple { column, cell } => {
                if let Some(cell) = purge_cell(cell, p, now) {
                    cols.push(ColumnData::Simple { column, cell });
                }
            }
            ColumnData::Complex { column, deletion, cells } => {
                let deletion = if p.should_purge_deletion(&deletion) { DeletionTime::LIVE } else { deletion };
                let cells: Vec<Cell> = cells.into_iter().filter_map(|c| purge_cell(c, p, now)).collect();
                if !cells.is_empty() || !deletion.is_live() {
                    cols.push(ColumnData::Complex { column, deletion, cells });
                }
            }
        }
    }
    row.columns = cols;
    if row.is_empty() { None } else { Some(row) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::BytesComparator;
    use bytes::Bytes;

    fn v(s: &'static str) -> Bytes {
        Bytes::from_static(s.as_bytes())
    }

    fn both(a: &Cell, b: &Cell) -> [Cell; 2] {
        let c = reconcile(Reconcile::Cassandra, a.clone(), b.clone());
        // Commutative.
        assert_eq!(c, reconcile(Reconcile::Cassandra, b.clone(), a.clone()));
        let s = reconcile(Reconcile::Scylla, a.clone(), b.clone());
        assert_eq!(s, reconcile(Reconcile::Scylla, b.clone(), a.clone()));
        [c, s]
    }

    #[test]
    fn higher_timestamp_wins() {
        let a = Cell::live(2, v("a"));
        let b = Cell::tombstone(1, 100);
        assert_eq!(both(&a, &b), [a.clone(), a.clone()]);
        let c = Cell::expiring(3, 10, 50, v("c"));
        assert_eq!(both(&a, &c), [c.clone(), c.clone()]);
    }

    #[test]
    fn tie_tombstone_beats_live() {
        let a = Cell::live(5, v("zzz"));
        let t = Cell::tombstone(5, 1);
        assert_eq!(both(&a, &t), [t.clone(), t.clone()]);
    }

    #[test]
    fn tie_tombstone_beats_expiring() {
        let e = Cell::expiring(5, 10, 1000, v("zzz"));
        let t = Cell::tombstone(5, 1);
        assert_eq!(both(&e, &t), [t.clone(), t.clone()]);
    }

    #[test]
    fn tie_expiring_beats_regular() {
        let a = Cell::live(5, v("zzz"));
        let e = Cell::expiring(5, 10, 1000, v("a"));
        assert_eq!(both(&a, &e), [e.clone(), e.clone()]);
    }

    #[test]
    fn tie_later_deletion_wins() {
        let t1 = Cell::tombstone(5, 10);
        let t2 = Cell::tombstone(5, 11);
        assert_eq!(both(&t1, &t2), [t2.clone(), t2.clone()]);
    }

    #[test]
    fn tie_later_expiry_wins_even_with_smaller_value() {
        let e1 = Cell::expiring(5, 10, 100, v("z"));
        let e2 = Cell::expiring(5, 10, 101, v("a"));
        assert_eq!(both(&e1, &e2), [e2.clone(), e2.clone()]);
    }

    #[test]
    fn tie_same_expiry_lower_ttl_wins() {
        let e1 = Cell::expiring(5, 20, 100, v("z"));
        let e2 = Cell::expiring(5, 10, 100, v("a"));
        assert_eq!(both(&e1, &e2), [e2.clone(), e2.clone()]);
    }

    #[test]
    fn tie_values_compare_unsigned() {
        let a = Cell::live(5, Bytes::from_static(&[0x7f]));
        let b = Cell::live(5, Bytes::from_static(&[0x80]));
        assert_eq!(both(&a, &b), [b.clone(), b.clone()]);
        let e = Cell::live(5, Bytes::new());
        assert_eq!(both(&a, &e), [a.clone(), a.clone()]);
        let x = Cell::expiring(5, 10, 100, v("b"));
        let y = Cell::expiring(5, 10, 100, v("a"));
        assert_eq!(both(&x, &y), [x.clone(), x.clone()]);
    }

    /// Observed on `scylladb/scylla:2026.3` (and `cassandra:5.0.9`): writes at `USING TIMESTAMP
    /// 20` with TTLs 1000 vs 2000 keep the later expiry whatever the values; with expirations
    /// made equal (TTL 1003 written 3 s before TTL 1000) the lower TTL wins; equal everything
    /// else, '0' beats '' and 'b' beats 'a'; a same-timestamp `DELETE` beats an `INSERT`.
    #[test]
    fn scylla_tiebreak_pinned_by_compat() {
        // An expired cell vs a live one at the same timestamp: decided without the clock.
        let expired = Cell::expiring(5, 1, 2, v("a"));
        let live = Cell::live(5, v("z"));
        assert_eq!(reconcile(Reconcile::Scylla, expired.clone(), live.clone()), expired);
        // Same expiration, different TTLs: the lower TTL.
        let e1 = Cell::expiring(5, 20, 100, v("z"));
        let e2 = Cell::expiring(5, 10, 100, v("a"));
        assert_eq!(reconcile(Reconcile::Scylla, e1, e2.clone()), e2);
    }

    #[test]
    fn counters() {
        let a = Cell::counter(5, 1, 10);
        let b = Cell::counter(4, 2, 12);
        let r = reconcile(Reconcile::Cassandra, a.clone(), b.clone());
        assert_eq!(r.counter_parts(), Some((2, 12)));
        assert_eq!(r.timestamp, 5, "keeps the newest timestamp");
        let t = Cell::counter_tombstone(1, 9);
        assert_eq!(reconcile(Reconcile::Scylla, a.clone(), t.clone()), t, "tombstone always wins");
        assert_eq!(reconcile(Reconcile::Scylla, t.clone(), a), t);
    }

    fn ck(s: &str) -> Clustering {
        Clustering::new([s.as_bytes()])
    }

    #[test]
    fn row_merge_shadowing() {
        let cmp = BytesComparator;
        let r1 = Row::new(ck("a")).with_liveness(LivenessInfo::live(10)).with_cell(1, Cell::live(10, v("x"))).with_cell(2, Cell::live(12, v("y")));
        let r2 = Row::new(ck("a")).with_deletion(RowDeletion::regular(DeletionTime::new(11, 100)));
        let m = merge_rows(&cmp, Reconcile::Cassandra, vec![r1.clone(), r2.clone()], DeletionTime::LIVE).unwrap();
        assert_eq!(m.liveness, LivenessInfo::EMPTY);
        assert_eq!(m.deletion, RowDeletion::regular(DeletionTime::new(11, 100)));
        assert_eq!(m.columns, vec![ColumnData::simple(2, Cell::live(12, v("y")))]);
        // An active deletion newer than the row deletion replaces it.
        let m = merge_rows(&cmp, Reconcile::Cassandra, vec![r1.clone(), r2.clone()], DeletionTime::new(20, 50));
        assert_eq!(m, None);
    }

    #[test]
    fn shadowable_deletion_lifted_by_newer_marker() {
        let cmp = BytesComparator;
        let r1 = Row::new(ck("a")).with_deletion(RowDeletion::shadowable(DeletionTime::new(10, 100))).with_cell(1, Cell::live(9, v("old")));
        let r2 = Row::new(ck("a")).with_liveness(LivenessInfo::live(11)).with_cell(1, Cell::live(8, v("older")));
        let m = merge_rows(&cmp, Reconcile::Cassandra, vec![r1, r2], DeletionTime::LIVE).unwrap();
        assert!(m.deletion.is_live());
        assert_eq!(m.liveness, LivenessInfo::live(11));
        assert_eq!(m.columns, vec![ColumnData::simple(1, Cell::live(9, v("old")))]);
        // A regular deletion is not lifted.
        let r1 = Row::new(ck("a")).with_deletion(RowDeletion::regular(DeletionTime::new(10, 100))).with_cell(1, Cell::live(9, v("old")));
        let r2 = Row::new(ck("a")).with_liveness(LivenessInfo::live(11));
        let m = merge_rows(&cmp, Reconcile::Cassandra, vec![r1, r2], DeletionTime::LIVE).unwrap();
        assert!(!m.deletion.is_live());
        assert!(m.columns.is_empty());
        assert_eq!(m.liveness, LivenessInfo::live(11));
    }

    #[test]
    fn complex_merge() {
        let cmp = BytesComparator;
        let p = |s: &'static str| Bytes::from_static(s.as_bytes());
        let r1 = Row::new(ck("a")).with_column(ColumnData::complex(
            3,
            DeletionTime::LIVE,
            vec![Cell::live(5, v("1")).with_path(p("a")), Cell::live(5, v("2")).with_path(p("c"))],
        ));
        let r2 = Row::new(ck("a")).with_column(ColumnData::complex(
            3,
            DeletionTime::new(5, 10),
            vec![Cell::live(6, v("3")).with_path(p("b")), Cell::live(6, v("4")).with_path(p("c"))],
        ));
        let m = merge_rows(&cmp, Reconcile::Scylla, vec![r1, r2], DeletionTime::LIVE).unwrap();
        assert_eq!(
            m.columns,
            vec![ColumnData::complex(3, DeletionTime::new(5, 10), vec![Cell::live(6, v("3")).with_path(p("b")), Cell::live(6, v("4")).with_path(p("c"))])]
        );
        // A complex deletion older than the active deletion is dropped.
        let r = Row::new(ck("a")).with_column(ColumnData::complex(3, DeletionTime::new(5, 10), vec![Cell::live(9, v("x")).with_path(p("a"))]));
        let r2 = Row::new(ck("a"));
        let m = merge_rows(&cmp, Reconcile::Scylla, vec![r, r2], DeletionTime::new(6, 1)).unwrap();
        assert_eq!(m.columns, vec![ColumnData::complex(3, DeletionTime::LIVE, vec![Cell::live(9, v("x")).with_path(p("a"))])]);
    }

    #[test]
    fn purge_expired_cells() {
        let p = GcPurger { gc_before: 100, max_purgeable: i64::MAX, max_purgeable_shadowable: i64::MAX, marker_dated_at_write: false };
        // Expired, but written recently: becomes a tombstone dated at the write.
        let c = Cell::expiring(5, 50, 160, v("x"));
        let t = purge_cell(c, &p, 200).unwrap();
        assert!(t.is_tombstone());
        assert_eq!(t.local_deletion_time, 110);
        // Written before gc_before: gone.
        let c = Cell::expiring(5, 50, 120, v("x"));
        assert_eq!(purge_cell(c, &p, 200), None);
        // Not expired: untouched.
        let c = Cell::expiring(5, 50, 300, v("x"));
        assert_eq!(purge_cell(c.clone(), &p, 200), Some(c));
        // Overlap protection.
        let p = GcPurger { gc_before: 100, max_purgeable: 5, max_purgeable_shadowable: 5, marker_dated_at_write: false };
        let t = Cell::tombstone(5, 10);
        assert_eq!(purge_cell(t.clone(), &p, 200), Some(t));
    }

    #[test]
    fn shadowable_deletions_check_row_markers() {
        let p = GcPurger { gc_before: 100, max_purgeable: 3, max_purgeable_shadowable: 10, marker_dated_at_write: false };
        let r = Row::new(ck("a")).with_deletion(RowDeletion::shadowable(DeletionTime::new(5, 50)));
        assert_eq!(purge_row(r, &p, 200, false), None);
        let r = Row::new(ck("a")).with_deletion(RowDeletion::regular(DeletionTime::new(5, 50)));
        assert!(purge_row(r, &p, 200, false).is_some());
        let p = GcPurger { gc_before: 100, max_purgeable: 10, max_purgeable_shadowable: 3, marker_dated_at_write: false };
        let r = Row::new(ck("a")).with_deletion(RowDeletion::shadowable(DeletionTime::new(5, 50)));
        assert!(purge_row(r, &p, 200, false).is_some());
    }

    #[test]
    fn expired_markers() {
        // A TTL'd marker that expired at 105 was written at 95: ScyllaDB purges it at
        // gc_before 100, Cassandra waits for its expiry.
        let r = || Row::new(ck("a")).with_liveness(LivenessInfo::expiring(5, 10, 105));
        let scylla = GcPurger { gc_before: 100, max_purgeable: 10, max_purgeable_shadowable: 10, marker_dated_at_write: true };
        assert_eq!(purge_row(r(), &scylla, 200, false), None);
        let cassandra = GcPurger { marker_dated_at_write: false, ..scylla };
        assert!(purge_row(r(), &cassandra, 200, false).is_some());
        // A view's expired-liveness deletion waits for older row markers only.
        let v = || Row::new(ck("a")).with_liveness(LivenessInfo::expired(5, 50)).with_cell(1, Cell::live(4, v("x")));
        let p = GcPurger { gc_before: 100, max_purgeable: 4, max_purgeable_shadowable: 10, marker_dated_at_write: true };
        assert_eq!(purge_row(v(), &p, 200, true), None);
        let p = GcPurger { max_purgeable_shadowable: 5, ..p };
        assert!(purge_row(v(), &p, 200, true).is_some());
    }

    #[test]
    fn purge_rows() {
        let p = GcPurger { gc_before: 100, max_purgeable: i64::MAX, max_purgeable_shadowable: i64::MAX, marker_dated_at_write: false };
        let r = Row::new(ck("a")).with_deletion(RowDeletion::regular(DeletionTime::new(5, 50))).with_cell(1, Cell::tombstone(6, 60));
        assert_eq!(purge_row(r, &p, 200, false), None);
        let r = Row::new(ck("a")).with_liveness(LivenessInfo::expiring(5, 10, 90)).with_cell(1, Cell::live(6, v("x")));
        let pr = purge_row(r, &p, 200, false).unwrap();
        assert!(pr.liveness.is_empty());
        assert_eq!(purge_row(pr, &p, 200, true), None, "strict liveness drops rows without a marker");
    }
}
