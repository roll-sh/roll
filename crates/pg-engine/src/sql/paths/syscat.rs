//! System catalog scans planned as table scans: a stand-in table of a catalog's columns
//! and the indexes Postgres has on it (`catalog/sys_indexes.tsv`), sized from its rows, so
//! the path a query's quals and ORDER BY make cheapest (an index scan in order, an
//! index-only scan, a bitmap scan) is chosen as for a table. The catalog's rows are
//! generated whole; an ordered path's are sorted in the index's order.

use super::*;
use crate::catalog::system::SysTable;
use crate::catalog::{Column, IndexColumn, IndexKey};
use crate::error::Result;
use crate::exec::Row;
use std::collections::HashMap;

/// The stand-in table's marker (no table of the catalog's has it).
const STAND_IN: u32 = u32::MAX;

/// The stand-in table for catalog `t` (its oid is the catalog's).
pub fn sys_table(t: SysTable) -> Arc<Table> {
    thread_local! {
        static TABLES: std::cell::RefCell<HashMap<u32, Arc<Table>>> = std::cell::RefCell::new(HashMap::new());
    }
    if let Some(t) = TABLES.with(|m| m.borrow().get(&t.oid()).cloned()) {
        return t;
    }
    let columns = t
        .columns()
        .iter()
        .map(|(n, ty)| {
            let mut c = Column::new(n.clone(), ty.clone());
            // (Catalog key columns are NOT NULL.)
            c.not_null = true;
            c
        })
        .collect();
    let table = Arc::new(Table {
        oid: t.oid(),
        schema: t.schema(),
        name: t.name().to_string(),
        owner: crate::catalog::BOOTSTRAP_SUPERUSER_OID,
        columns,
        pkey: None,
        pkey_index: None,
        pkey_unkeyed: false,
        indexes: Vec::new(),
        constraints: Vec::new(),
        next_index_id: STAND_IN,
        rowid_seq: None,
        temporary: false,
        unlogged: false,
        comment: None,
        rowtype: 0,
        rowarray: 0,
        toast: 0,
        toast_relname: None,
        toast_indname: None,
        rls: false,
        rls_force: false,
        of_type: 0,
        inherits: Vec::new(),
        partkey: None,
        partbound: None,
        options: Vec::new(),
        replident: None,
        foreign: None,
        detach_pending: false,
    });
    TABLES.with(|m| m.borrow_mut().insert(t.oid(), table.clone()));
    table
}

/// The catalog a stand-in table is for.
pub(crate) fn stand_in_of(t: &Table) -> Option<SysTable> {
    if t.next_index_id != STAND_IN || t.oid >= crate::catalog::FIRST_USER_OID {
        return None;
    }
    SysTable::by_oid(t.oid)
}

/// Postgres's btree indexes on catalog `t`: (index, unique). (Read from the list once per
/// catalog: planning asks for each catalog column an estimate looks at.)
fn sys_indexes(t: SysTable) -> Vec<Arc<Index>> {
    static BY_TABLE: std::sync::OnceLock<parking_lot::Mutex<HashMap<u32, Vec<Arc<Index>>>>> =
        std::sync::OnceLock::new();
    let m = BY_TABLE.get_or_init(Default::default);
    if let Some(v) = m.lock().get(&t.oid()) {
        return v.clone();
    }
    let v = read_sys_indexes(t);
    m.lock().insert(t.oid(), v.clone());
    v
}

fn read_sys_indexes(t: SysTable) -> Vec<Arc<Index>> {
    static INDEXES: &str = include_str!("../../catalog/sys_indexes.tsv");
    let cols = t.columns();
    let mut out = Vec::new();
    for (n, line) in INDEXES.lines().filter(|l| !l.starts_with('#')).enumerate() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 3 || f[0] != t.name() {
            continue;
        }
        let keys: Option<Vec<usize>> = f[2]
            .split(',')
            .map(|k| cols.iter().position(|(c, _)| c == k))
            .collect();
        let Some(keys) = keys else { continue };
        out.push(Arc::new(Index {
            // (Apart from every real relation's.)
            oid: 0xF000_0000 + n as u32,
            name: f[1].to_string(),
            table: t.oid(),
            id: 0,
            columns: keys
                .iter()
                .map(|&k| IndexColumn {
                    key: IndexKey::Column(k),
                    desc: false,
                    nulls_first: false,
                    opclass: 0,
                    collation: None,
                    stattarget: None,
                    opclass_options: Vec::new(),
                })
                .collect(),
            include: Vec::new(),
            unique: f.get(3) == Some(&"t"),
            primary: false,
            nulls_not_distinct: false,
            predicate: None,
            method: "btree".into(),
            valid: true,
            options: Vec::new(),
            parent: 0,
            clustered: false,
            replident: false,
        }));
    }
    out
}

/// Whether stand-in table `t`'s column `col` is the one key of a unique index of its
/// catalog's (examine_variable's isunique).
pub(crate) fn sys_unique_col(t: &Table, col: usize) -> bool {
    stand_in_of(t).is_some_and(|st| {
        sys_indexes(st).iter().any(|ix| {
            ix.unique && ix.columns.len() == 1 && matches!(ix.columns[0].key, IndexKey::Column(c) if c == col)
        })
    })
}

/// The catalog's size: its rows, and the heap pages they'd fill.
pub fn sys_size(t: SysTable, ctx: &ExecCtx) -> RelSize {
    let tuples = crate::sql::explain::sys_tuples(t, ctx);
    let width: u32 = t.columns().iter().map(|(_, ty)| costs::type_width(ty)).sum();
    RelSize {
        pages: ((tuples * (width as f64 + 28.0)) / 8192.0).ceil().max(1.0),
        tuples,
    }
}

/// The index paths' view of the catalog's indexes (all its rows visible: index-only scans
/// read no heap).
pub(super) fn sys_index_infos(t: SysTable, tuples: f64) -> Vec<IndexInfo> {
    let table = sys_table(t);
    let mut out: Vec<IndexInfo> = sys_indexes(t)
        .into_iter()
        .map(|ix| {
            let keys: Vec<Expr> = ix
                .columns
                .iter()
                .filter_map(|c| match c.key {
                    IndexKey::Column(k) => Some(Expr::Col(k, table.columns[k].ty.clone())),
                    _ => None,
                })
                .collect();
            // (Leaf pages at 90% full: the keys and their tuple headers.)
            let kw: u32 = keys.iter().map(|k| costs::type_width(&k.ty())).sum();
            let pages = ((tuples * (kw as f64 + 16.0)) / (8192.0 * 0.9)).ceil().max(1.0) + 1.0;
            let height = if pages <= 2.0 { 0 } else { 1 };
            let n = keys.len();
            IndexInfo {
                canreturn: keys
                    .iter()
                    .filter_map(|k| match k {
                        Expr::Col(c, _) => Some(*c),
                        _ => None,
                    })
                    .collect(),
                ix,
                am: Am::Btree,
                keys,
                pred: Vec::new(),
                target: false,
                pages,
                tuples,
                height,
                order: vec![None; n],
                colls: vec![None; n],
                kcolls: Vec::new(),
                fam_ops: Vec::new(),
                user_ops: Vec::new(),
            }
        })
        .collect();
    // (get_relation_info's list: descending OIDs.)
    out.sort_by_key(|a| std::cmp::Reverse(a.ix.oid));
    out
}

/// A system catalog's rows in the order of the index `s` reads (backward when it scans so),
/// and the position each came from.
pub fn sys_index_order(rows: Vec<Row>, s: &ScanPlan, ctx: &ExecCtx) -> Result<(Vec<Row>, Vec<usize>)> {
    let Some((ix, _)) = &s.index else {
        let n = rows.len();
        return Ok((rows, (0..n).collect()));
    };
    let keys: Vec<usize> = ix
        .columns
        .iter()
        .filter_map(|c| match c.key {
            IndexKey::Column(k) => Some(k),
            _ => None,
        })
        .collect();
    let sk = SortKey {
        expr: Expr::Col(0, DataType::Unknown),
        desc: false,
        nulls_first: false,
        using: None,
    };
    let mut idx: Vec<usize> = (0..rows.len()).collect();
    let mut err = None;
    idx.sort_by(|&a, &b| {
        for &k in &keys {
            match crate::exec::nodes::compare_values(&rows[a][k], &rows[b][k], &sk, ctx) {
                Ok(Ordering::Equal) => {}
                Ok(o) => return o,
                Err(e) => {
                    err.get_or_insert(e);
                    return Ordering::Equal;
                }
            }
        }
        Ordering::Equal
    });
    if let Some(e) = err {
        return Err(e);
    }
    if s.reverse {
        idx.reverse();
    }
    let mut slots: Vec<Option<Row>> = rows.into_iter().map(Some).collect();
    let out = idx.iter().map(|&i| slots[i].take().expect("each once")).collect();
    Ok((out, idx))
}

/// The SysScan under a Sort's projections, its keys over its row.
fn sys_target(p: &mut Plan, keys: Vec<Expr>) -> Option<(&mut Plan, Vec<Expr>)> {
    match p {
        Plan::Project { input, exprs } => {
            let keys = keys
                .into_iter()
                .map(|k| {
                    k.transform(&mut |e| {
                        Ok(match e {
                            Expr::Col(i, t) => exprs.get(i).cloned().unwrap_or(Expr::Col(i, t)),
                            e => e,
                        })
                    })
                    .unwrap()
                })
                .collect();
            sys_target(take(input), keys)
        }
        Plan::SysScan { .. } => Some((p, keys)),
        _ => None,
    }
}

impl Planner<'_, '_> {
    /// The relation path generation sees for a catalog scan.
    pub(super) fn sys_rel(&self, t: SysTable, filter: Option<&Expr>, needed: &[bool]) -> Rel {
        let table = sys_table(t);
        let mut s = stand_in_scan(table, filter.cloned());
        s.written = written_order(filter);
        self.rel(&s, needed).expect("a stand-in relation")
    }

    /// A Sort over a catalog's scan (through projections): read an index in order when
    /// that is cheapest (the sort goes), or the catalog by a bitmap scan. Whether the scan
    /// was planned here.
    pub(super) fn sys_ordered_scan(&self, p: &mut Plan, needed: &[bool], lim: Option<(f64, f64)>) -> bool {
        let Plan::Sort { input, keys } = p else {
            return false;
        };
        if keys.iter().any(|k| k.using.is_some()) {
            return false;
        }
        let key_exprs: Vec<Expr> = keys.iter().map(|k| k.expr.clone()).collect();
        let dirs: Vec<(bool, bool)> = keys.iter().map(|k| (k.desc, k.nulls_first)).collect();
        let scan_needed = scan_needs(input, needed);
        let Some((scan, mapped)) = sys_target(take(input), key_exprs) else {
            return false;
        };
        let Plan::SysScan { table, filter, path, .. } = scan else {
            return false;
        };
        if path.is_some() || table.is_view() || sys_indexes(*table).is_empty() {
            return false;
        }
        let rel = self.sys_rel(*table, filter.as_ref(), &scan_needed);
        let width: u32 = table
            .columns()
            .iter()
            .enumerate()
            .filter(|(i, _)| scan_needed.get(*i).copied().unwrap_or(true))
            .map(|(_, (_, ty))| costs::type_width(ty))
            .sum();
        let want = Want {
            merge: Vec::new(),
            keys: mapped
                .into_iter()
                .zip(dirs)
                // (A key an equality fixes is no pathkey.)
                .filter(|(expr, _)| !const_equated(&rel, expr))
                .map(|(expr, (desc, nulls_first))| PathKey {
                    expr,
                    desc,
                    nulls_first,
                    cmp: 0,
                    coll: 0,
                })
                .collect(),
            limit: lim,
        };
        let chosen = self.choose(&rel, &want, width);
        let (ordered, base) = match &chosen.kind {
            PathKind::Sort(inner) => (false, (**inner).clone()),
            _ => (true, chosen.clone()),
        };
        // (A sequential scan sorted: as the catalog is scanned already.)
        if !ordered && matches!(base.kind, PathKind::Seq) && self.g.seqscan {
            return false;
        }
        let mut s = stand_in_scan(rel.table.clone(), filter.clone());
        s.written = rel.written.as_ref().clone();
        apply(&rel, &mut s, &base);
        if let Access::Index { ordered: o, .. } = &mut s.access {
            *o = ordered;
        }
        *path = Some(Arc::new(crate::sql::plan::SysPath { scan: s, ordered }));
        if ordered {
            let Plan::Sort { input, .. } = std::mem::replace(p, Plan::One) else {
                unreachable!()
            };
            *p = Arc::try_unwrap(input).unwrap_or_else(|a| (*a).clone());
        }
        true
    }
}

/// A catalog's scan as a join group's relation (its paths chosen as a table's, a
/// parameterized index scan among them): the stand-in table's scan with the catalog scan's
/// filter. None for a view, or a catalog without indexes.
pub(super) fn sys_leaf_scan(t: SysTable, filter: Option<&Expr>) -> Option<ScanPlan> {
    if t.is_view() || sys_indexes(t).is_empty() {
        return None;
    }
    let mut s = stand_in_scan(sys_table(t), filter.cloned());
    s.written = written_order(filter);
    Some(s)
}

/// A stand-in table's scan with the catalog scan's filter.
fn stand_in_scan(table: Arc<Table>, filter: Option<Expr>) -> ScanPlan {
    ScanPlan {
        table,
        index: None,
        filter,
        with_key: false,
        reverse: false,
        mark: None,
        lock_order: 0,
        limit: None,
        sys: false,
        alias: None,
        current_of: None,
        sample: None,
        excl: None,
        part_excl: None,
        part: None,
        access: Default::default(),
        col_aliases: None,
        written: Vec::new(),
        phv_quals: Vec::new(),
        phv_proj: false,
        check_as: None,
        rti: Default::default(),
        useful: Default::default(),
        params: Default::default(),
        reltarget: Default::default(),
        fdw: None,
        rtindex: Default::default(),
    }
}

/// The filter's conjuncts as baserestrictinfo holds them: equalities with a constant
/// (equivalence classes') after the others.
fn written_order(filter: Option<&Expr>) -> Vec<Expr> {
    let mut parts = Vec::new();
    if let Some(f) = filter {
        conjuncts(f.clone(), &mut parts);
    }
    let ec = |e: &Expr| {
        matches!(e, Expr::Call(d, a, _) if d.name == "=" && a.len() == 2
            && (matches!(a[0], Expr::Col(..)) && a[1].columns().is_empty()
                || matches!(a[1], Expr::Col(..)) && a[0].columns().is_empty()))
    };
    let (mut first, rest): (Vec<Expr>, Vec<Expr>) = parts.into_iter().partition(|e| !ec(e));
    first.extend(rest);
    first
}
