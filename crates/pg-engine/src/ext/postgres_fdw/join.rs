//! Foreign Scans of joins (postgresGetForeignJoinPaths, foreign_join_ok,
//! estimate_path_cost_size): which joins of a server's tables run remotely as one query,
//! what each part of it sends, and what the remote join is estimated to cost.
//!
//! Relations are described over their query level's join row (the join search's group
//! row): each base relation's columns from its offset there, conditions as expressions over
//! that row.

use crate::catalog::{Catalog, Table};
use crate::sql::costs::CPU_TUPLE_COST;
use crate::sql::expr::Expr;
use crate::sql::plan::{JoinKind, ScanPlan};
use std::sync::Arc;

/// DEFAULT_FDW_STARTUP_COST / DEFAULT_FDW_TUPLE_COST.
const DEFAULT_STARTUP_COST: f64 = 100.0;
const DEFAULT_TUPLE_COST: f64 = 0.2;
/// DEFAULT_FDW_SORT_MULTIPLIER.
pub const SORT_MULTIPLIER: f64 = 1.05;

/// The options costing reads (apply_server_options, apply_table_options, merge_fdw_options).
#[derive(Clone, Debug)]
pub struct Opts {
    pub use_remote_estimate: bool,
    pub startup_cost: f64,
    pub tuple_cost: f64,
    pub fetch_size: usize,
    pub async_capable: bool,
}

/// A relation's options: its server's, then its table's (the table's win).
pub fn opts(cat: &Catalog, t: &Table) -> Opts {
    let f = t.foreign.as_ref();
    let srv = f.and_then(|f| cat.servers.get(&f.server));
    let lists = [srv.and_then(|s| s.options.as_ref()), f.and_then(|f| f.options.as_ref())];
    let mut o = Opts {
        use_remote_estimate: false,
        startup_cost: DEFAULT_STARTUP_COST,
        tuple_cost: DEFAULT_TUPLE_COST,
        fetch_size: 100,
        async_capable: false,
    };
    for l in lists {
        let get = |n: &str| super::option::get(l, n);
        if let Some(v) = get("use_remote_estimate").and_then(|v| crate::ext::dblink::def_boolean("use_remote_estimate", v).ok()) {
            o.use_remote_estimate = v;
        }
        if let Some(v) = get("fdw_startup_cost").and_then(|v| v.trim().parse().ok()) {
            o.startup_cost = v;
        }
        if let Some(v) = get("fdw_tuple_cost").and_then(|v| v.trim().parse().ok()) {
            o.tuple_cost = v;
        }
        if let Some(v) = get("fetch_size").and_then(|v| v.trim().parse().ok()) {
            o.fetch_size = v;
        }
        if let Some(v) = get("async_capable").and_then(|v| crate::ext::dblink::def_boolean("async_capable", v).ok()) {
            o.async_capable = v;
        }
    }
    o
}

/// merge_fdw_options: a join's (or upper relation's) options from its inputs'.
fn merge_opts(o: &Opts, i: Option<&Opts>) -> Opts {
    let mut m = o.clone();
    if let Some(i) = i {
        m.use_remote_estimate = o.use_remote_estimate || i.use_remote_estimate;
        m.fetch_size = o.fetch_size.max(i.fetch_size);
        m.async_capable = o.async_capable || i.async_capable;
    }
    m
}

/// Join row columns from here up are a joined relation's columns its own join doesn't
/// return (a FROM item that is a foreign join taken into a bigger one, `fdw_adopt`).
pub const HIDDEN_COLS: usize = 1 << 30;

/// A base relation of a foreign join: its scan and where its columns are in the join row.
#[derive(Clone, Debug)]
pub struct Base {
    pub scan: ScanPlan,
    /// Per column of the scan's row, the join row column it is.
    pub cols: Vec<Option<usize>>,
    /// Its relation's tuples (its restrictions' selectivity is its rows over them).
    pub tuples: f64,
}

impl Base {
    /// A scan whose row is the join row's from `off`.
    pub fn at(scan: ScanPlan, off: usize, ncols: usize, tuples: f64) -> Base {
        Base { scan, cols: (0..ncols).map(|k| Some(off + k)).collect(), tuples }
    }

    /// The scan row column join row column `g` is.
    pub fn col(&self, g: usize) -> Option<usize> {
        self.cols.iter().position(|c| *c == Some(g))
    }
}

/// The type of join row column `g`.
pub fn col_type(bases: &[Base], g: usize) -> crate::types::DataType {
    bases
        .iter()
        .find_map(|b| b.col(g).and_then(|k| b.scan.table.columns.get(k)))
        .map_or(crate::types::DataType::Unknown, |c| c.ty.clone())
}

/// PgFdwRelationInfo of a relation that can run remotely.
#[derive(Clone, Debug)]
pub struct RelInfo {
    pub server: u32,
    /// The user the relation's permissions are checked as (None: the current user).
    pub user: Option<u32>,
    pub kind: Kind,
    /// Its base relations (indexes into the bases list).
    pub relids: u64,
    pub remote_conds: Vec<Expr>,
    pub local_conds: Vec<Expr>,
    /// Relations deparsed into a subquery below (lower_subquery_rels) and those a semi
    /// join hides in its EXISTS (hidden_subquery_rels).
    pub lower_subquery: u64,
    pub hidden_subquery: u64,
    /// Its subquery alias number (`sN`) when deparsed as one.
    pub relation_index: u32,
    /// The join row columns it returns (its reltarget), in order.
    pub reltarget: Vec<usize>,
    pub opts: Opts,
    pub rows: f64,
    pub width: u32,
    pub retrieved_rows: f64,
    pub rel_startup: f64,
    pub rel_total: f64,
    pub startup: f64,
    pub total: f64,
}

#[derive(Clone, Debug)]
pub enum Kind {
    Base(usize),
    Join {
        jt: JoinKind,
        outer: Arc<RelInfo>,
        inner: Arc<RelInfo>,
        joinclauses: Vec<Expr>,
        make_outer_subquery: bool,
        make_inner_subquery: bool,
    },
}

impl RelInfo {
    pub fn is_join(&self) -> bool {
        matches!(self.kind, Kind::Join { .. })
    }

    /// The same relation with its join row columns renumbered.
    pub fn map_cols(&self, map: &dyn Fn(usize) -> usize) -> RelInfo {
        let f = |v: &Vec<Expr>| v.iter().map(|e| e.clone().remap_cols(map)).collect::<Vec<_>>();
        let kind = match &self.kind {
            Kind::Base(k) => Kind::Base(*k),
            Kind::Join { jt, outer, inner, joinclauses, make_outer_subquery, make_inner_subquery } => Kind::Join {
                jt: *jt,
                outer: Arc::new(outer.map_cols(map)),
                inner: Arc::new(inner.map_cols(map)),
                joinclauses: f(joinclauses),
                make_outer_subquery: *make_outer_subquery,
                make_inner_subquery: *make_inner_subquery,
            },
        };
        RelInfo {
            kind,
            remote_conds: f(&self.remote_conds),
            local_conds: f(&self.local_conds),
            reltarget: self.reltarget.iter().map(|&g| map(g)).collect(),
            ..self.clone()
        }
    }

    /// The same relation with its base relations renumbered (`map` of each index).
    pub fn renumber(&self, map: &dyn Fn(usize) -> usize) -> RelInfo {
        let bits = |m: u64| (0..64).filter(|k| m >> k & 1 == 1).fold(0u64, |acc, k| acc | 1u64 << map(k));
        let kind = match &self.kind {
            Kind::Base(k) => Kind::Base(map(*k)),
            Kind::Join { jt, outer, inner, joinclauses, make_outer_subquery, make_inner_subquery } => Kind::Join {
                jt: *jt,
                outer: Arc::new(outer.renumber(map)),
                inner: Arc::new(inner.renumber(map)),
                joinclauses: joinclauses.clone(),
                make_outer_subquery: *make_outer_subquery,
                make_inner_subquery: *make_inner_subquery,
            },
        };
        RelInfo {
            kind,
            relids: bits(self.relids),
            lower_subquery: bits(self.lower_subquery),
            hidden_subquery: bits(self.hidden_subquery),
            ..self.clone()
        }
    }
}

/// get_remote_estimate: EXPLAIN the relation's remote query on its server and take the
/// estimates of its top node, then the local conditions' share and the transfer costs.
pub fn remote_estimate(
    ctx: &crate::exec::ExecCtx,
    r: &mut RelInfo,
    bases: &[Base],
    tlist: &[usize],
    local_sel: f64,
    local_cost: (f64, f64),
) {
    let cat = ctx.catalog();
    let fmt = ctx.fmt().clone();
    let types = |g: usize| col_type(bases, g);
    let mut q = super::joinsql::Sql::new(&cat, &fmt, bases);
    q.placeholders = true;
    let cols: Vec<Expr> = tlist.iter().map(|&g| Expr::Col(g, types(g))).collect();
    let sql = format!("EXPLAIN {}", q.select(r, Some(&cols), &r.remote_conds, &super::joinsql::Tail::default()));
    let first = bases.iter().enumerate().find(|(k, _)| r.relids >> k & 1 == 1).map(|(_, b)| b);
    let Some(first) = first else { return };
    let Some(est) = explain_estimate(ctx, &first.scan, &sql) else { return };
    let (rows, width, mut startup, mut total) = est;
    let retrieved = rows;
    let rows = crate::sql::costs::clamp_rows(rows * local_sel);
    startup += local_cost.0;
    total += local_cost.1 * retrieved;
    finish(r, rows, width, retrieved, startup, total);
}

/// The first line of a remote EXPLAIN: `(cost=s..t rows=r width=w)`.
pub fn explain_estimate(ctx: &crate::exec::ExecCtx, scan: &ScanPlan, sql: &str) -> Option<(f64, u32, f64, f64)> {
    let m = super::scan::mapping_for(&scan.table, scan.check_as, ctx).ok()?;
    let key = super::connection::get_connection(ctx, &m, false).ok()?;
    let intr = crate::remote::interrupts(ctx);
    let res = super::connection::with_conn(ctx, key, |c| {
        let r = c.exec(sql, &intr)?;
        if r.status != crate::remote::conn::Status::TuplesOk {
            return Err(super::connection::report(&r, c, Some(sql)));
        }
        Ok(r)
    })
    .ok()?;
    let line = res.rows.first()?.first()?.clone()?;
    let p = line.find("(cost=")?;
    let rest = &line[p + 6..];
    let (s, rest) = rest.split_once("..")?;
    let (t, rest) = rest.split_once(" rows=")?;
    let (rows, rest) = rest.split_once(" width=")?;
    let w = rest.trim_end_matches(')');
    Some((rows.parse().ok()?, w.parse().ok()?, s.parse().ok()?, t.parse().ok()?))
}

/// What the planner knows of a join being considered.
pub struct JoinIn<'a> {
    pub jt: JoinKind,
    pub outer: &'a RelInfo,
    pub inner: &'a RelInfo,
    /// Its restriction clauses, each with whether it is pushed down to the join from above
    /// (RINFO_IS_PUSHED_DOWN) rather than an outer join's own clause.
    pub restrict: Vec<(Expr, bool)>,
    /// Whether an expression can run remotely (is_foreign_expr over the join).
    pub ship: &'a dyn Fn(&Expr) -> bool,
    /// The join row columns its reltarget has, in order.
    pub reltarget: Vec<usize>,
    pub relation_index: u32,
}

/// foreign_join_ok: the join's description when it can run remotely (its costs unset).
pub fn join_ok(j: &JoinIn) -> Option<RelInfo> {
    let (o, i) = (j.outer, j.inner);
    if !matches!(j.jt, JoinKind::Inner | JoinKind::Left | JoinKind::Full | JoinKind::Semi) {
        return None;
    }
    if o.server != i.server || o.user != i.user || !o.local_conds.is_empty() || !i.local_conds.is_empty() {
        return None;
    }
    let outer_join = matches!(j.jt, JoinKind::Left | JoinKind::Full);
    let mut joinclauses = Vec::new();
    let mut remote = Vec::new();
    let mut local = Vec::new();
    for (c, pushed) in &j.restrict {
        let shippable = (j.ship)(c);
        if outer_join && !pushed {
            if !shippable {
                return None;
            }
            joinclauses.push(c.clone());
        } else if shippable {
            remote.push(c.clone());
        } else {
            local.push(c.clone());
        }
    }
    let mut lower = o.lower_subquery | i.lower_subquery;
    let mut hidden = o.hidden_subquery | i.hidden_subquery;
    let (mut mo, mut mi) = (false, false);
    match j.jt {
        JoinKind::Inner => {
            remote.extend(i.remote_conds.iter().cloned());
            remote.extend(o.remote_conds.iter().cloned());
        }
        JoinKind::Left => {
            if i.hidden_subquery == 0 {
                joinclauses.extend(i.remote_conds.iter().cloned());
            }
            if o.hidden_subquery == 0 {
                remote.extend(o.remote_conds.iter().cloned());
            }
        }
        JoinKind::Semi => {
            joinclauses.extend(i.remote_conds.iter().cloned());
            joinclauses.append(&mut remote);
            remote = o.remote_conds.clone();
            hidden |= i.relids;
        }
        _ => {
            if !o.remote_conds.is_empty() {
                mo = true;
                lower |= o.relids;
            }
            if !i.remote_conds.is_empty() {
                mi = true;
                lower |= i.relids;
            }
        }
    }
    if j.jt == JoinKind::Inner {
        joinclauses = std::mem::take(&mut remote);
    } else if outer_join {
        if o.hidden_subquery != 0 {
            mo = true;
            lower |= o.relids;
        }
        if i.hidden_subquery != 0 {
            mi = true;
            lower |= i.relids;
        }
    }
    Some(RelInfo {
        server: o.server,
        user: o.user,
        kind: Kind::Join {
            jt: j.jt,
            outer: Arc::new(o.clone()),
            inner: Arc::new(i.clone()),
            joinclauses,
            make_outer_subquery: mo,
            make_inner_subquery: mi,
        },
        relids: o.relids | i.relids,
        remote_conds: remote,
        local_conds: local,
        lower_subquery: lower,
        hidden_subquery: hidden,
        relation_index: j.relation_index,
        reltarget: j.reltarget.clone(),
        opts: merge_opts(&o.opts, Some(&i.opts)),
        rows: 0.0,
        width: 0,
        retrieved_rows: -1.0,
        rel_startup: -1.0,
        rel_total: -1.0,
        startup: 0.0,
        total: 0.0,
    })
}

/// What estimate_path_cost_size reads of the planner for a join (no remote estimate):
/// the core code's size of the join, the selectivity of its join clauses (at its join
/// type) and of its local conditions, and the per-tuple and startup costs of the clause
/// lists.
pub struct JoinCostIn {
    pub rows: f64,
    pub width: u32,
    pub joinclause_sel: f64,
    pub local_sel: f64,
    pub join_cost: (f64, f64),
    pub remote_cost: (f64, f64),
    pub local_cost: (f64, f64),
}

/// estimate_path_cost_size for a bare join relation, locally.
pub fn estimate_join(r: &mut RelInfo, c: &JoinCostIn) {
    let Kind::Join { outer, inner, .. } = &r.kind else { return };
    let rows = c.rows;
    let nrows = inner.rows * outer.rows;
    let retrieved = crate::sql::costs::clamp_rows(rows / c.local_sel).min(nrows);
    let startup = inner.rel_startup + outer.rel_startup + c.join_cost.0 + c.remote_cost.0 + c.local_cost.0;
    let mut run = inner.rel_total - inner.rel_startup + outer.rel_total - outer.rel_startup;
    run += nrows * c.join_cost.1;
    run += crate::sql::costs::clamp_rows(nrows * c.joinclause_sel) * c.remote_cost.1;
    run += c.local_cost.1 * retrieved;
    finish(r, rows, c.width, retrieved, startup, startup + run);
}

/// The cached bare-relation estimates, then the transfer costs on top
/// (estimate_path_cost_size's tail for a path without pathkeys or extra steps).
pub fn finish(r: &mut RelInfo, rows: f64, width: u32, retrieved: f64, startup: f64, total: f64) {
    r.rows = rows;
    r.width = width;
    r.retrieved_rows = retrieved;
    r.rel_startup = startup;
    r.rel_total = total;
    r.startup = startup + r.opts.startup_cost;
    r.total = total + r.opts.startup_cost + (r.opts.tuple_cost + CPU_TUPLE_COST) * retrieved;
}

/// What estimate_path_cost_size reads of a base relation (set_baserel_size_estimates' rows,
/// its pages and tuples, baserestrictcost, the local conditions' selectivity).
pub struct BaseIn<'a> {
    pub table: &'a Table,
    pub base: usize,
    pub rtindex: u32,
    pub user: Option<u32>,
    pub remote_conds: Vec<Expr>,
    pub local_conds: Vec<Expr>,
    pub reltarget: Vec<usize>,
    pub rows: f64,
    pub tuples: f64,
    pub pages: f64,
    pub width: u32,
    pub qual: (f64, f64),
    pub local_sel: f64,
    pub seq_page_cost: f64,
}

/// A lone base relation's description (an aggregation's input), without estimates.
pub fn plain_base(cat: &Catalog, s: &ScanPlan, remote_conds: Vec<Expr>) -> RelInfo {
    RelInfo {
        server: s.table.foreign.as_ref().map_or(0, |f| f.server),
        user: s.check_as,
        kind: Kind::Base(0),
        relids: 1,
        remote_conds,
        local_conds: Vec::new(),
        lower_subquery: 0,
        hidden_subquery: 0,
        relation_index: s.rtindex.0,
        reltarget: Vec::new(),
        opts: opts(cat, &s.table),
        rows: 0.0,
        width: 0,
        retrieved_rows: -1.0,
        rel_startup: -1.0,
        rel_total: -1.0,
        startup: 0.0,
        total: 0.0,
    }
}

/// A base relation's description and its estimate without remote help.
pub fn base_info(cat: &Catalog, b: &BaseIn) -> Option<RelInfo> {
    let f = b.table.foreign.as_ref()?;
    let mut r = RelInfo {
        server: f.server,
        user: b.user,
        kind: Kind::Base(b.base),
        relids: 1u64 << b.base,
        remote_conds: b.remote_conds.clone(),
        local_conds: b.local_conds.clone(),
        lower_subquery: 0,
        hidden_subquery: 0,
        relation_index: b.rtindex,
        reltarget: b.reltarget.clone(),
        opts: opts(cat, b.table),
        rows: 0.0,
        width: 0,
        retrieved_rows: -1.0,
        rel_startup: -1.0,
        rel_total: -1.0,
        startup: 0.0,
        total: 0.0,
    };
    let retrieved = crate::sql::costs::clamp_rows(b.rows / b.local_sel).min(b.tuples);
    let startup = b.qual.0;
    let run = b.seq_page_cost * b.pages + (CPU_TUPLE_COST + b.qual.1) * b.tuples;
    finish(&mut r, b.rows, b.width, retrieved, startup, startup + run);
    Some(r)
}
