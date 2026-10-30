//! Catalog rows carried over from one catalog version to the next (round 19, agents 62, 76).
//!
//! A system catalog's generated rows (`system::rows_shared`) are a function of the catalog
//! (with the session's roles, path and settings, which key them). A DDL statement changes a
//! few objects; the next version's pg_class, pg_attribute, pg_attrdef, pg_index,
//! pg_constraint and pg_type differ from the last generated ones only in the rows of the
//! relations it touched. Those are found by comparing the two versions' structurally shared
//! maps (`PMap::changes`, by pointer), regenerated alone (`rows_generated_for` of one
//! relation) and spliced in among the earlier version's rows, which are shared by pointer
//! (`SysRows` holds `Arc`s): reading a whole catalog after DDL costs the rows that changed
//! and a pointer per row, not a generation of every relation. A version in which nothing
//! the catalog's rows read changed (COMMENT, ALTER ... SET of what no row shows) reuses the
//! earlier rows whole. When anything every row may read changed (schemas, roles,
//! privileges, types, collations, triggers, rules, functions, ...), the catalog is
//! generated whole. Debug builds compare every splice with the full generation.

use super::sysrows::{PartKey, RowChunk, fixed_key};
use super::system::SysTable;
use super::{Catalog, ConstraintKind, FIRST_USER_OID};
use crate::types::Value;
use crate::types::value::Row;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// The catalogs whose rows aren't spliced but are kept for a later version in which nothing
/// they read changed (`unchanged`): pg_depend and information_schema's constraint views.
pub fn kept(table: SysTable) -> bool {
    use SysTable::*;
    spliced(table)
        || matches!(
            table,
            PgDepend
                | IsTableConstraints
                | IsKeyColumnUsage
                | IsReferentialConstraints
                | IsCheckConstraints
                | IsConstraintColumnUsage
        )
}

/// Whether `b` stores as `a` does but for what no catalog `kept` shows: comments, pg_class's
/// heap order and statistics, frozen xids, counters.
pub fn unchanged(a: &Catalog, b: &Catalog) -> bool {
    use super::persist;
    const HEADER: [&str; 5] = ["version", "next_oid", "comment_tick", "class_tick", "shared_stamp"];
    const TAGS: [u8; 5] = [persist::COMMENTS_TAG, persist::CLASS_ORDER_TAG, 18, 38, 44];
    persist::header_changed(a, b).iter().all(|h| HEADER.contains(h))
        && persist::record_changes(a, b).is_ok_and(|c| c.iter().all(|(t, _, _)| TAGS.contains(t)))
}

/// The catalogs whose rows are spliced: rows belong to one relation each (`part_of`).
pub fn spliced(table: SysTable) -> bool {
    use SysTable::*;
    matches!(table, PgClass | PgAttribute | PgAttrdef | PgIndex | PgConstraint | PgType)
}

/// The column naming the object a row belongs to, whose rows lie together
/// (`SysRows::lookup` serves an equality on it from where they lie).
pub fn owner_col(table: SysTable) -> usize {
    match table {
        SysTable::PgAttrdef => 1,
        SysTable::PgConstraint => 8,
        _ => 0,
    }
}

/// Whether two maps of shared values hold the same entries (an object no copy changed is
/// the same `Arc`).
fn same_arcs<K: Ord, T>(a: &BTreeMap<K, Arc<T>>, b: &BTreeMap<K, Arc<T>>) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|((ka, va), (kb, vb))| ka == kb && Arc::ptr_eq(va, vb))
}

/// Whether two persistent maps hold equal entries (chunks they share aren't compared).
fn same_values<V: PartialEq>(a: &super::pmap::PMap<V>, b: &super::pmap::PMap<V>) -> bool {
    if a.ptr_eq(b) {
        return true;
    }
    let mut same = true;
    a.diff(b, |x, y| x == y, |_, _, _| same = false);
    same
}

/// Whether what any relation's rows may read beyond its own entries is alike in both.
fn globals_same(a: &Catalog, b: &Catalog) -> bool {
    a.schemas == b.schemas
        && a.roles == b.roles
        && a.acls == b.acls
        && same_values(&a.rel_am, &b.rel_am)
        && same_values(&a.rel_tablespace, &b.rel_tablespace)
        && a.tablespaces == b.tablespaces
        && same_values(&a.filenodes, &b.filenodes)
        && a.class_has_rules == b.class_has_rules
        && a.class_has_subclass == b.class_has_subclass
        && a.class_has_triggers == b.class_has_triggers
        && a.had_triggers == b.had_triggers
        && a.colstats == b.colstats
        && a.array_names == b.array_names
        && a.composite_domains == b.composite_domains
        && a.multirange_schemas == b.multirange_schemas
        && same_arcs(&a.triggers, &b.triggers)
        && same_arcs(&a.rules, &b.rules)
        && same_arcs(&a.collations, &b.collations)
        && same_arcs(&a.composites, &b.composites)
        && same_arcs(&a.enums, &b.enums)
        && same_arcs(&a.domains, &b.domains)
        && same_arcs(&a.base_types, &b.base_types)
        && same_arcs(&a.ranges, &b.ranges)
        && same_arcs(&a.opclasses, &b.opclasses)
        && same_arcs(&a.opfamilies, &b.opfamilies)
        && same_arcs(&a.access_methods, &b.access_methods)
        && same_arcs(&a.statistics, &b.statistics)
        && same_arcs(&a.policies, &b.policies)
}

/// What stored expressions are shown through (pg_index's, pg_constraint's and pg_type's rows
/// deparse them): functions, operators and casts.
fn deparse_same(a: &Catalog, b: &Catalog) -> bool {
    (a.functions.ptr_eq(&b.functions) || a.functions.changes(&b.functions).is_empty())
        && same_arcs(&a.operators, &b.operators)
        && same_arcs(&a.casts, &b.casts)
}

/// The relations whose rows may differ between `a` and `b` (None: generate whole).
fn changed_relations(a: &Catalog, b: &Catalog) -> Option<BTreeSet<u32>> {
    // (Rows a superuser's UPDATE renumbered aren't their relation's parts: not spliced.)
    if !globals_same(a, b) || !a.rel_oid_edits.is_empty() || !b.rel_oid_edits.is_empty() {
        return None;
    }
    let mut out = BTreeSet::new();
    for (oid, x, y) in a.tables.changes(&b.tables) {
        out.insert(oid);
        for t in [x, y].into_iter().flatten() {
            if t.toast != 0 {
                out.extend([t.toast, t.toast + 1]);
            }
            // Its parents (relhassubclass), indexes (their kind, owner, columns' names),
            // and the tables its foreign keys reference (relhastriggers).
            out.extend(t.inherits.iter().copied());
            out.extend(t.indexes.iter().copied().chain(t.pkey_index));
            for c in &t.constraints {
                if let ConstraintKind::ForeignKey { ref_table, .. } = &c.kind {
                    out.insert(*ref_table);
                }
            }
        }
        // Its partitions (their bounds are shown by its key) and owned sequences (as
        // persistent as it is).
        for cat in [a, b] {
            out.extend(cat.children(oid).iter().copied());
            if let Some(s) = cat.sequences.ix().owned.get(&oid) {
                out.extend(s.iter().copied());
            }
        }
    }
    for (oid, x, y) in a.views.changes(&b.views) {
        out.insert(oid);
        for st in [x, y].into_iter().flatten().filter_map(|v| v.storage.as_ref()) {
            if st.toast != 0 {
                out.extend([st.toast, st.toast + 1]);
            }
            out.extend(st.indexes.iter().copied());
        }
    }
    for (oid, x, y) in a.indexes.changes(&b.indexes) {
        out.insert(oid);
        for ix in [x, y].into_iter().flatten() {
            out.insert(ix.table);
            if ix.parent != 0 {
                out.insert(ix.parent);
            }
        }
    }
    for (oid, _, _) in a.sequences.changes(&b.sequences) {
        out.insert(oid);
    }
    a.relstats.diff(&b.relstats, |x, y| x == y, |k, _, _| {
        out.insert(k);
    });
    a.frozen_xids.diff(&b.frozen_xids, |x, y| x == y, |k, _, _| {
        out.insert(k);
    });
    // (Postgres's own relations' rows aren't regenerated alone.)
    if out.first().is_some_and(|o| *o < FIRST_USER_OID) {
        return None;
    }
    Some(out)
}

/// The objects whose rows of `table` may differ between `a` (the rows' version) and `b`
/// (None: generate whole; empty: the rows are `a`'s).
pub fn changed_rels(table: SysTable, a: &Catalog, b: &Catalog) -> Option<BTreeSet<u32>> {
    let rels = changed_relations(a, b)?;
    if rels.is_empty() || matches!(table, SysTable::PgClass | SysTable::PgAttribute | SysTable::PgAttrdef) {
        return Some(rels);
    }
    if !deparse_same(a, b) {
        return None;
    }
    match table {
        // (Rows by index: those of the changed tables' indexes are in the set already.)
        SysTable::PgIndex => Some(rels),
        SysTable::PgConstraint => {
            // A constraint trigger's row shows its table's schema: kept in place, so its
            // table mustn't change.
            if b.triggers.values().any(|x| x.con_oid != 0 && rels.contains(&x.table)) {
                return None;
            }
            // A foreign key's row shows the referenced table's key index.
            let mut out = rels.clone();
            for cat in [a, b] {
                for r in &rels {
                    if let Some(f) = cat.tables.ix().fk_refs.get(r) {
                        out.extend(f.keys());
                    }
                }
            }
            Some(out)
        }
        SysTable::PgType => Some(rels),
        _ => None,
    }
}

/// Where a row lies: among the rows of an object (`Of`), or among those every version in
/// which `changed_rels` splices has alike, in a section of the full generation's order.
#[derive(Clone, Copy, PartialEq)]
enum Part {
    Of(u32),
    Fixed(u8),
}

impl Part {
    fn key(self) -> PartKey {
        match self {
            Part::Of(o) => o as PartKey,
            Part::Fixed(s) => fixed_key(s),
        }
    }
}

/// The column `SysRows` serves equalities on from where its objects' rows lie: every row
/// whose value there is a user object's is in that object's run.
pub fn run_owner(table: SysTable) -> Option<usize> {
    use SysTable::*;
    matches!(table, PgClass | PgAttribute | PgAttrdef | PgIndex).then(|| owner_col(table))
}

/// The part row `r` of `table` belongs to in `cat` (`chunked`'s).
pub fn part_key(table: SysTable, cat: &Catalog, r: &Row) -> PartKey {
    part_of(table, cat, r).key()
}

/// `rows` (the full generation of `table` for `cat`) in chunks along their parts.
pub fn chunked(table: SysTable, cat: &Catalog, rows: Vec<Row>) -> Vec<Arc<RowChunk>> {
    super::sysrows::chunk(rows, |r| part_of(table, cat, r).key())
}

/// Whether `chunks`' runs are the parts of their rows in `cat` (debug builds check every
/// splice's).
pub fn parts_hold(table: SysTable, cat: &Catalog, chunks: &[Arc<RowChunk>]) -> bool {
    chunks.iter().all(|c| {
        let mut a = 0;
        c.parts.iter().all(|&(p, end)| {
            let ok = a < end && c.rows(a as usize, end as usize).all(|r| part_of(table, cat, &r).key() == p);
            a = end;
            ok
        }) && a as usize == c.len()
    })
}

fn oid_col(r: &Row, i: usize) -> u32 {
    match r.get(i) {
        Some(Value::Oid(o)) => *o,
        _ => 0,
    }
}

/// Where row `r` of `table` (generated for `cat`) lies.
fn part_of(table: SysTable, cat: &Catalog, r: &Row) -> Part {
    match table {
        SysTable::PgConstraint => {
            let rel = oid_col(r, 8);
            if rel < FIRST_USER_OID || r.get(3) == Some(&Value::Char(b't')) {
                // (Postgres's own, then after the tables' those of triggers and domains.)
                Part::Fixed(if oid_col(r, 0) < FIRST_USER_OID { 0 } else { 9 })
            } else {
                Part::Of(rel)
            }
        }
        SysTable::PgType => {
            let oid = oid_col(r, 0);
            if oid < FIRST_USER_OID {
                return Part::Fixed(0);
            }
            if let Some(&(t, _)) = cat.tables.ix().rowtypes.get(&oid) {
                return Part::Of(t);
            }
            if let Some(&(v, _)) = cat.views.ix().rowtypes.get(&oid) {
                return Part::Of(v);
            }
            // Enums and domains (and their arrays) come before the relations' row types,
            // composites, ranges and base types after.
            let elem = match oid_col(r, 13) {
                0 => oid,
                e => e,
            };
            Part::Fixed(if cat.enums.contains_key(&elem) || cat.domains.contains_key(&elem) { 1 } else { 9 })
        }
        _ => {
            let o = oid_col(r, owner_col(table));
            if o < FIRST_USER_OID { Part::Fixed(0) } else { Part::Of(o) }
        }
    }
}

/// Where relation (or index) `oid`'s rows of `table` lie in the full generation's order
/// for `cat` (None: it has none there).
fn position(table: SysTable, cat: &Catalog, oid: u32) -> Option<(u8, u64)> {
    match table {
        SysTable::PgAttrdef => Some(if cat.tables.contains_key(&oid) {
            (2, oid as u64)
        } else {
            (3, cat.views.get(&oid)?.oid as u64)
        }),
        SysTable::PgIndex => {
            if cat.indexes.contains_key(&oid) {
                return Some((2, oid as u64));
            }
            // (TOAST tables' indexes follow, in `Catalog::toasted`'s order.)
            match cat.toast_owner(oid) {
                Some((t, true)) => {
                    let view = !cat.tables.contains_key(&t.oid);
                    Some((3, ((view as u64) << 40) | t.oid as u64))
                }
                _ => None,
            }
        }
        SysTable::PgConstraint => cat.tables.contains_key(&oid).then_some((2, oid as u64)),
        SysTable::PgType => {
            if cat.tables.get(&oid).is_some_and(|t| t.rowtype != 0) {
                Some((2, oid as u64))
            } else if cat.views.get(&oid).is_some_and(|v| v.rowtype != 0) {
                Some((3, oid as u64))
            } else {
                None
            }
        }
        _ => super::system::class_position(cat, oid).map(|(s, k)| (s + 1, k)),
    }
}

/// Object `o`'s rows of `table` for the statement's catalog, generated alone.
fn regen(table: SysTable, ctx: &crate::exec::ExecCtx, cat: &Catalog, o: u32) -> crate::error::Result<Vec<Row>> {
    let only = match table {
        SysTable::PgIndex => match (cat.indexes.get(&o), cat.toast_owner(o)) {
            (Some(ix), _) => ix.table,
            (None, Some((t, true))) => t.toast,
            _ => return Ok(Vec::new()),
        },
        _ => o,
    };
    let rows = super::system::rows_generated_for(table, ctx, Some(only))?;
    Ok(rows.into_iter().filter(|r| part_of(table, cat, r) == Part::Of(o)).collect())
}

/// The chunks of `table`'s rows for the statement's catalog, from `old` (the rows generated
/// for `held`, an earlier version) with the rows of the objects in `changed` regenerated
/// (None: not a catalog spliced, or rows it can't place).
///
/// The earlier chunks hold runs of one part each, in the full generation's order. Where a
/// changed object's rows were and go is found by binary searches over the runs' positions
/// (in `held`: an unchanged object's is the same in both), so the catalog is asked about the
/// changed objects and the runs a search visits; only the chunks holding them are made
/// again, every other one is the earlier version's, by pointer.
pub fn splice(
    table: SysTable,
    ctx: &crate::exec::ExecCtx,
    held: &Catalog,
    old: &super::sysrows::SysRows,
    changed: &BTreeSet<u32>,
) -> crate::error::Result<Option<Vec<Arc<RowChunk>>>> {
    let chunks = old.chunks();
    if !spliced(table) || chunks.is_empty() {
        return Ok(None);
    }
    let cat = ctx.catalog();
    let pos = |p: PartKey| match p >> 32 {
        0 => position(table, held, p as u32),
        _ => Some(((p & 0xff) as u8, 0)),
    };
    // (chunk, run): the first run whose position is past `p` (a run of chunk `c`, or its
    // end); Err: a run that can't be placed.
    let locate = |p: (u8, u64)| -> Result<(usize, usize), ()> {
        let mut bad = false;
        let mut le = |k: PartKey| match pos(k) {
            Some(q) => q <= p,
            None => {
                bad = true;
                false
            }
        };
        let c1 = chunks.partition_point(|c| le(c.parts[0].0));
        if c1 == 0 {
            return if bad { Err(()) } else { Ok((0, 0)) };
        }
        let r = chunks[c1 - 1].parts.partition_point(|x| le(x.0));
        if bad { Err(()) } else { Ok((c1 - 1, r)) }
    };
    // Per chunk to make again: (run, position, object) of the rows going in.
    let mut edits: BTreeMap<usize, Vec<(usize, (u8, u64), u32)>> = BTreeMap::new();
    for &o in changed {
        // Where its earlier rows are (when it had any).
        if let Some(p) = position(table, held, o) {
            let Ok((c, r)) = locate(p) else { return Ok(None) };
            if r > 0 && chunks[c].parts[r - 1].0 == o as PartKey {
                edits.entry(c).or_default();
            }
        }
        if let Some(p) = position(table, &cat, o) {
            let Ok((c, r)) = locate(p) else { return Ok(None) };
            edits.entry(c).or_default().push((r, p, o));
        }
    }
    let mut out: Vec<Arc<RowChunk>> = Vec::with_capacity(chunks.len() + 1);
    let mut next = 0;
    for (c, mut ins) in edits {
        out.extend(chunks[next..c].iter().cloned());
        next = c + 1;
        ins.sort_unstable();
        let mut ins = ins.into_iter().peekable();
        let old = &chunks[c];
        let mut rows: Vec<Row> = Vec::with_capacity(old.len() + 8);
        let mut parts: Vec<(PartKey, u32)> = Vec::with_capacity(old.parts.len() + 2);
        let fresh = |o: u32, rows: &mut Vec<Row>, parts: &mut Vec<(PartKey, u32)>| -> crate::error::Result<()> {
            let new = regen(table, ctx, &cat, o)?;
            if !new.is_empty() {
                rows.extend(new);
                parts.push((o as PartKey, rows.len() as u32));
            }
            Ok(())
        };
        let mut a = 0;
        for (k, &(p, end)) in old.parts.iter().enumerate() {
            while let Some(&(r, _, o)) = ins.peek()
                && r <= k
            {
                fresh(o, &mut rows, &mut parts)?;
                ins.next();
            }
            if p >> 32 != 0 || !changed.contains(&(p as u32)) {
                rows.extend(old.rows(a as usize, end as usize));
                parts.push((p, rows.len() as u32));
            }
            a = end;
        }
        for (_, _, o) in ins {
            fresh(o, &mut rows, &mut parts)?;
        }
        if !rows.is_empty() {
            out.extend(super::sysrows::SysRows::rechunk(rows, parts));
        }
    }
    out.extend(chunks[next..].iter().cloned());
    Ok(Some(out))
}
