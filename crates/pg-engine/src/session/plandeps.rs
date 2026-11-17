//! What PostgreSQL's plan cache invalidates a prepared statement's plan by, for the
//! statements whose planning ran immutable PL/pgSQL calls (`Procs::calls`). The engine binds a
//! statement again after any catalog change; PostgreSQL replans only when an object its plan
//! depends on changed (plancache.c: PlanCacheRelCallback for the plan's relations, whose
//! relcache entries ALTER, CREATE / DROP INDEX, ANALYZE, VACUUM, GRANT and TRUNCATE reset;
//! PlanCacheObjectCallback for its functions; PlanCacheSysCallback, every plan, for schemas and
//! operators). When none did, the binding takes the values the earlier planning got
//! (`Binder::proc_memo`), so the calls (their NOTICEs) don't run again.

use crate::catalog::{Catalog, ColStats, Function, Index, Operator, Policy, RelStats, Schema, Sequence, Table, Trigger, ViewDef};
use crate::catalog::acl::ObjAcl;
use crate::sql::binder::BoundQuery;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The state of one relation the plan reads, as its relcache entry would see it.
struct Rel {
    table: Option<Arc<Table>>,
    view: Option<Arc<ViewDef>>,
    index: Option<Arc<Index>>,
    seq: Option<Arc<Sequence>>,
    stats: Option<RelStats>,
    filenode: Option<u32>,
    cols: Option<Vec<ColStats>>,
    acl: Vec<ObjAcl>,
    policies: Vec<Arc<Policy>>,
    triggers: Vec<Arc<Trigger>>,
}

fn same<T>(a: &Option<Arc<T>>, b: &Option<Arc<T>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

fn same_all<T>(a: &[Arc<T>], b: &[Arc<T>]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| Arc::ptr_eq(a, b))
}

impl Rel {
    fn of(cat: &Catalog, oid: u32) -> Rel {
        Rel {
            table: cat.tables.get(&oid).cloned(),
            view: cat.views.get(&oid).cloned(),
            index: cat.indexes.get(&oid).cloned(),
            seq: cat.sequences.get(&oid).cloned(),
            stats: cat.relstats.get(&oid).copied(),
            filenode: cat.filenodes.get(&oid).copied(),
            cols: cat.colstats.get(&oid).cloned(),
            acl: cat.acls.iter().filter(|a| a.oid == oid).cloned().collect(),
            policies: cat.policies.values().filter(|p| p.table == oid).cloned().collect(),
            triggers: cat.triggers.values().filter(|t| t.table == oid).cloned().collect(),
        }
    }

    fn same(&self, o: &Rel) -> bool {
        same(&self.table, &o.table)
            && same(&self.view, &o.view)
            && same(&self.index, &o.index)
            && same(&self.seq, &o.seq)
            && self.stats == o.stats
            && self.filenode == o.filenode
            && self.cols == o.cols
            && self.acl == o.acl
            && same_all(&self.policies, &o.policies)
            && same_all(&self.triggers, &o.triggers)
    }
}

/// A plan's invalidation items at one catalog version.
pub(super) struct Deps {
    rels: Vec<(u32, Rel)>,
    funcs: Vec<(u32, Option<Arc<Function>>)>,
    schemas: BTreeMap<u32, Schema>,
    operators: Vec<Arc<Operator>>,
}

impl Deps {
    /// `b`'s, in catalog `cat`: the relations it locks (its range table's, the views'
    /// included, and the indexes it scans), the functions it calls or planning ran.
    pub(super) fn of(cat: &Catalog, b: &BoundQuery) -> Deps {
        let mut rels: Vec<u32> = b.perms.locks.iter().map(|(o, _)| *o).chain(b.perms.rels.iter().map(|r| r.oid)).collect();
        rels.sort_unstable();
        rels.dedup();
        let mut funcs: Vec<u32> = b.perms.funcs.clone();
        funcs.extend(b.procs.calls.iter().flat_map(|c| c.calls.iter().map(|(f, ..)| *f)));
        funcs.sort_unstable();
        funcs.dedup();
        Deps {
            rels: rels.into_iter().map(|o| (o, Rel::of(cat, o))).collect(),
            funcs: funcs.into_iter().map(|f| (f, cat.functions.get(&f).cloned())).collect(),
            schemas: cat.schemas.clone(),
            operators: cat.operators.values().cloned().collect(),
        }
    }

    /// Whether a plan with these items is still valid in catalog `cat`.
    pub(super) fn hold(&self, cat: &Catalog) -> bool {
        self.schemas == cat.schemas
            && same_all(&self.operators, &cat.operators.values().cloned().collect::<Vec<_>>())
            && self.funcs.iter().all(|(f, d)| same(d, &cat.functions.get(f).cloned()))
            && self.rels.iter().all(|(o, r)| r.same(&Rel::of(cat, *o)))
    }
}
