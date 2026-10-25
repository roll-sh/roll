//! Secondary indexes of the catalog's relation maps (`pmap::IndexedMap`), kept as tables,
//! views, indexes and sequences come and go: what every DDL statement (and the first
//! statements over a new catalog version) asked by scanning every relation.

use super::pmap::{Counts, MapIndex, PHash, PMap};
use super::{Index, Sequence, Table, ViewDef};
use crate::types::DataType;
use std::sync::Arc;

pub type TableMap = super::pmap::IndexedMap<Arc<Table>, TableIx>;
pub type ViewMap = super::pmap::IndexedMap<Arc<ViewDef>, ViewIx>;
pub type IndexMap = super::pmap::IndexedMap<Arc<Index>, IndexIx>;
pub type SequenceMap = super::pmap::IndexedMap<Arc<Sequence>, SeqIx>;

#[derive(Clone, Default)]
pub struct TableIx {
    /// The tables internal triggers are on (`exec::trigger::table_internal_triggers`: both
    /// ends of an enforced foreign key, a table with a deferrable key), by how many reasons.
    pub trigger_marks: Counts,
    /// (schema, constraint name) → how many of the schema's tables' constraints have it.
    pub con_names: PHash<(u32, String), u32>,
    /// A TOAST table, and its index, → its table.
    pub toast: PMap<u32>,
    /// Row types (true: their array types) → the table.
    pub rowtypes: PMap<(u32, bool)>,
    /// Composite types columns hold (directly, in arrays, ranges or other composites) →
    /// the tables with such columns.
    pub row_users: PHash<u32, Counts>,
    /// Tables with row-level security, by name.
    pub rls: PHash<String, Counts>,
    /// A constraint → its table (the first, by oid: `Deleted::owners`).
    pub con_owner: PMap<Counts>,
    /// A default's pg_attrdef oid → the tables with a column of it.
    pub def_owner: PMap<Counts>,
    /// A table → the tables whose foreign keys reference it.
    pub fk_refs: PMap<Counts>,
}

/// The composite types `t` holds, as `DataType::contains_row` finds them.
fn rows_in(t: &DataType, out: &mut Vec<u32>) {
    match t {
        DataType::Row(r) => {
            out.push(r.oid);
            for (_, f) in r.fields.iter() {
                rows_in(f, out);
            }
        }
        DataType::Array(e) => rows_in(e, out),
        DataType::Range(rt) | DataType::Multirange(rt) => rows_in(&rt.subtype, out),
        _ => {}
    }
}

fn table_rows_used(t: &Table) -> Vec<u32> {
    let mut out = Vec::new();
    for c in t.columns.iter().filter(|c| !c.dropped) {
        rows_in(&c.ty, &mut out);
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn bump(m: &mut PHash<(u32, String), u32>, k: (u32, String), up: bool) {
    let n = m.get(&k).copied().unwrap_or(0);
    if up {
        m.insert(k, n + 1);
    } else if n > 1 {
        m.insert(k, n - 1);
    } else {
        m.remove(&k);
    }
}

fn set_member(m: &mut PHash<u32, Counts>, k: u32, member: u32, up: bool) {
    let mut c = m.get(&k).cloned().unwrap_or_default();
    if up {
        c.add(member);
    } else {
        c.remove(member);
    }
    if c.is_empty() {
        m.remove(&k);
    } else {
        m.insert(k, c);
    }
}

fn count_in(m: &mut PMap<Counts>, k: u32, member: u32, up: bool) {
    let mut c = m.get(&k).cloned().unwrap_or_default();
    if up {
        c.add(member);
    } else {
        c.remove(member);
    }
    if c.is_empty() {
        m.remove(&k);
    } else {
        m.insert(k, c);
    }
}

impl TableIx {
    fn apply(&mut self, t: &Table, up: bool) {
        let mut marks = Vec::new();
        crate::exec::trigger::table_internal_triggers(t, &mut marks);
        if t.constraints.iter().any(|c| c.kind.index().is_some() && c.deferrable) {
            marks.push(t.oid);
        }
        for m in marks {
            if up {
                self.trigger_marks.add(m);
            } else {
                self.trigger_marks.remove(m);
            }
        }
        for c in &t.constraints {
            bump(&mut self.con_names, (t.schema, c.name.clone()), up);
            count_in(&mut self.con_owner, c.oid, t.oid, up);
            if let super::ConstraintKind::ForeignKey { ref_table, .. } = &c.kind {
                count_in(&mut self.fk_refs, *ref_table, t.oid, up);
            }
        }
        for c in t.columns.iter().filter(|c| c.def_oid != 0) {
            count_in(&mut self.def_owner, c.def_oid, t.oid, up);
        }
        if t.toast != 0 {
            for o in [t.toast, t.toast + 1] {
                if up {
                    self.toast.insert(o, t.oid);
                } else if self.toast.get(&o) == Some(&t.oid) {
                    self.toast.remove(&o);
                }
            }
        }
        if t.rowtype != 0 {
            for (o, array) in [(t.rowtype, false), (t.array_type(), true)] {
                if up {
                    self.rowtypes.insert(o, (t.oid, array));
                } else {
                    self.rowtypes.remove(&o);
                }
            }
        }
        for r in table_rows_used(t) {
            set_member(&mut self.row_users, r, t.oid, up);
        }
        if t.rls {
            let mut c = self.rls.get(&t.name).cloned().unwrap_or_default();
            if up {
                c.add(t.oid);
            } else {
                c.remove(t.oid);
            }
            if c.is_empty() {
                self.rls.remove(&t.name);
            } else {
                self.rls.insert(t.name.clone(), c);
            }
        }
    }
}

impl MapIndex<Arc<Table>> for TableIx {
    fn add(&mut self, _: u32, t: &Arc<Table>) {
        self.apply(t, true);
    }
    fn remove(&mut self, _: u32, t: &Arc<Table>) {
        self.apply(t, false);
    }
}

#[derive(Clone, Default)]
pub struct ViewIx {
    /// A materialized view's TOAST table, and its index, → the view.
    pub toast: PMap<u32>,
    /// Row types (true: their array types) → the view.
    pub rowtypes: PMap<(u32, bool)>,
    /// Security barrier views, by name: how many.
    pub barriers: PHash<String, u32>,
}

impl ViewIx {
    fn apply(&mut self, v: &ViewDef, up: bool) {
        if v.option("security_barrier") {
            let n = self.barriers.get(&v.name).copied().unwrap_or(0);
            if up {
                self.barriers.insert(v.name.clone(), n + 1);
            } else if n > 1 {
                self.barriers.insert(v.name.clone(), n - 1);
            } else {
                self.barriers.remove(&v.name);
            }
        }
        if let Some(t) = v.storage.as_ref().filter(|t| t.toast != 0) {
            for o in [t.toast, t.toast + 1] {
                if up {
                    self.toast.insert(o, v.oid);
                } else if self.toast.get(&o) == Some(&v.oid) {
                    self.toast.remove(&o);
                }
            }
        }
        if v.rowtype != 0 {
            for (o, array) in [(v.rowtype, false), (v.array_type(), true)] {
                if up {
                    self.rowtypes.insert(o, (v.oid, array));
                } else {
                    self.rowtypes.remove(&o);
                }
            }
        }
    }
}

impl MapIndex<Arc<ViewDef>> for ViewIx {
    fn add(&mut self, _: u32, v: &Arc<ViewDef>) {
        self.apply(v, true);
    }
    fn remove(&mut self, _: u32, v: &Arc<ViewDef>) {
        self.apply(v, false);
    }
}

#[derive(Clone, Default)]
pub struct IndexIx {
    /// Tables with an index on an expression, by how many.
    pub expr_tables: Counts,
    /// Partitioned indexes with indexes attached to them, by how many.
    pub parents: Counts,
}

impl MapIndex<Arc<Index>> for IndexIx {
    fn add(&mut self, _: u32, ix: &Arc<Index>) {
        if ix.parent != 0 {
            self.parents.add(ix.parent);
        }
        if ix.columns.iter().any(|c| matches!(c.key, super::IndexKey::Expr(_))) {
            self.expr_tables.add(ix.table);
        }
    }
    fn remove(&mut self, _: u32, ix: &Arc<Index>) {
        if ix.parent != 0 {
            self.parents.remove(ix.parent);
        }
        if ix.columns.iter().any(|c| matches!(c.key, super::IndexKey::Expr(_))) {
            self.expr_tables.remove(ix.table);
        }
    }
}

#[derive(Clone, Default)]
pub struct SeqIx {
    /// A table → the sequences it owns (by a column), in oid order.
    pub owned: PMap<Arc<Vec<u32>>>,
}

impl MapIndex<Arc<Sequence>> for SeqIx {
    fn add(&mut self, oid: u32, s: &Arc<Sequence>) {
        if let Some((t, _)) = s.owned_by {
            let mut v: Vec<u32> = self.owned.get(&t).map(|v| (**v).clone()).unwrap_or_default();
            if let Err(at) = v.binary_search(&oid) {
                v.insert(at, oid);
            }
            self.owned.insert(t, Arc::new(v));
        }
    }
    fn remove(&mut self, oid: u32, s: &Arc<Sequence>) {
        if let Some((t, _)) = s.owned_by
            && let Some(v) = self.owned.get(&t)
        {
            let v: Vec<u32> = v.iter().copied().filter(|o| *o != oid).collect();
            if v.is_empty() {
                self.owned.remove(&t);
            } else {
                self.owned.insert(t, Arc::new(v));
            }
        }
    }
}
