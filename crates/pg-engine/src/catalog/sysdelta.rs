//! Catalog rows carried over from one catalog version to the next (round 19, agent 62).
//!
//! A system catalog's generated rows (`system::rows_shared`) are a function of the catalog
//! (with the session's roles, path and settings, which key them). A DDL statement changes a
//! few objects; the next version's pg_class, pg_attribute, pg_attrdef and pg_index differ
//! from the last generated ones only in the rows of the relations it touched. Those are
//! found by comparing the two versions' structurally shared maps (`PMap::changes`, by
//! pointer), regenerated alone (`rows_generated_for` of one relation) and spliced into the
//! earlier rows in place, so reading a whole catalog after DDL costs the rows that changed
//! plus a copy, not a generation of every relation. When anything every row may read
//! changed (schemas, roles, privileges, types, collations, triggers, rules, ...), the
//! catalog is generated whole as before. Debug builds compare every splice with the full
//! generation.

use super::system::SysTable;
use super::{Catalog, ConstraintKind, FIRST_USER_OID};
use crate::types::Value;
use crate::types::value::Row;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// The catalogs whose rows are spliced: rows belong to one relation each (`owner`).
/// (pg_index isn't: an index's row reads more than its own and its table's entries.)
pub fn spliced(table: SysTable) -> bool {
    matches!(table, SysTable::PgClass | SysTable::PgAttribute | SysTable::PgAttrdef)
}

/// The column naming the relation a row belongs to.
fn owner_col(table: SysTable) -> usize {
    match table {
        SysTable::PgAttrdef => 1,
        _ => 0,
    }
}

/// Whether two maps of shared values hold the same entries (an object no copy changed is
/// the same `Arc`).
fn same_arcs<K: Ord, T>(a: &BTreeMap<K, Arc<T>>, b: &BTreeMap<K, Arc<T>>) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|((ka, va), (kb, vb))| ka == kb && Arc::ptr_eq(va, vb))
}

/// Whether what any relation's rows may read beyond its own entries is alike in both.
fn globals_same(a: &Catalog, b: &Catalog) -> bool {
    a.schemas == b.schemas
        && a.roles == b.roles
        && a.acls == b.acls
        && a.rel_am == b.rel_am
        && a.rel_tablespace == b.rel_tablespace
        && a.tablespaces == b.tablespaces
        && a.filenodes == b.filenodes
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

/// The relations whose rows may differ between `a` and `b` (None: generate whole).
pub fn changed_rels(a: &Catalog, b: &Catalog) -> Option<BTreeSet<u32>> {
    if !globals_same(a, b) {
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

/// Where relation `oid`'s rows of `table` lie in the full generation's order (None: it has
/// none there).
fn position(table: SysTable, cat: &Catalog, oid: u32) -> Option<(u8, u64)> {
    match table {
        SysTable::PgAttrdef => Some(if cat.tables.contains_key(&oid) {
            (1, oid as u64)
        } else {
            (2, cat.views.get(&oid)?.oid as u64)
        }),
        _ => super::system::class_position(cat, oid),
    }
}

/// The rows of `table` for the statement's catalog, from `old` (the rows generated for an
/// earlier version) with the rows of the relations in `changed` regenerated (None: not a
/// catalog spliced, or rows it can't place).
pub fn splice(
    table: SysTable,
    ctx: &crate::exec::ExecCtx,
    old: &[Row],
    changed: &BTreeSet<u32>,
) -> crate::error::Result<Option<Vec<Row>>> {
    if !spliced(table) {
        return Ok(None);
    }
    let cat = ctx.catalog();
    let k = owner_col(table);
    let owner = |r: &Row| match r.get(k) {
        Some(Value::Oid(o)) => *o,
        _ => 0,
    };
    // The changed relations, in the order their rows go.
    let mut fresh: Vec<((u8, u64), u32)> = (changed.iter())
        .filter_map(|&o| Some((position(table, &cat, o)?, o)))
        .collect();
    fresh.sort_unstable();
    let mut fresh = fresh.into_iter().peekable();
    let mut out: Vec<Row> = Vec::with_capacity(old.len() + changed.len());
    let regen = |o: u32, out: &mut Vec<Row>| -> crate::error::Result<()> {
        let rows = super::system::rows_generated_for(table, ctx, Some(o))?;
        out.extend(rows.into_iter().filter(|r| owner(r) == o));
        Ok(())
    };
    let mut i = 0;
    while i < old.len() {
        let o = owner(&old[i]);
        let mut j = i + 1;
        while j < old.len() && owner(&old[j]) == o {
            j += 1;
        }
        if o < FIRST_USER_OID {
            // (Postgres's own relations' rows come first.)
            out.extend_from_slice(&old[i..j]);
        } else if !changed.contains(&o) {
            let Some(at) = position(table, &cat, o) else {
                return Ok(None);
            };
            while let Some(&(p, f)) = fresh.peek()
                && p < at
            {
                regen(f, &mut out)?;
                fresh.next();
            }
            out.extend_from_slice(&old[i..j]);
        }
        i = j;
    }
    for (_, f) in fresh {
        regen(f, &mut out)?;
    }
    Ok(Some(out))
}
