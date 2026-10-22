//! The join search, as Postgres's planner does it (allpaths.c's standard_join_search,
//! joinrels.c, joinpath.c): every FROM item of a group of inner joins (or the two sides of
//! an outer join) gets its paths, parameterized index scans included; join relations are
//! built level by level, each with nested loop, hash and merge join paths kept by
//! add_path; the cheapest path of the whole becomes the plan (a projection restores the
//! column order the rest of the query expects).

use super::*;
use crate::sql::joins::{self, In, InKind, JoinGucs, Jt, QualCost, Semi};
use crate::sql::plan::{JoinMeta, JoinMethod};
use std::rc::Rc;

mod lift;

/// A pulled-up subquery's expression over the row it reads (`lift::subst`).
pub(super) fn lift_subst(e: &Expr, map: &[Expr]) -> Expr {
    lift::subst(e, map)
}
mod outerjoins;
mod partitionwise;
mod partmerge;
pub(super) use outerjoins::absorbable;
pub(super) use partitionwise::{PwJoin, whole_rows};

/// At most this many FROM items are reordered (join_collapse_limit).
const COLLAPSE_LIMIT: usize = 8;

/// A FROM item of the group: a slice of the group's row.
struct Leaf {
    plan: Plan,
    off: usize,
    ncols: usize,
    /// A table scan whose paths the planner chooses.
    scan: Option<(ScanPlan, Rel)>,
    rows: f64,
    startup: f64,
    total: f64,
    kind: InKind,
    /// Per column (leaf-local): width, the base column it is, a known distinct count.
    cols: Vec<(u32, Option<VarRef>, Option<f64>)>,
    /// An inheritance Append of table scans: its members (for parameterized paths).
    members: Option<Vec<Member>>,
    /// A LATERAL item: the values (over the group row) it reads as `Outer(1, k)`, and
    /// the items they come from.
    lateral: Option<(Vec<Expr>, u32)>,
    /// A table scan under the projection computing its virtual generated columns in their
    /// own slots (a nullable side's): the projection, put back over the scan chosen.
    proj: Option<Vec<Expr>>,
}

/// A FROM item that is a table scan computing virtual generated columns in their slots
/// (`binder::select::materialize_virtuals`): the scan and the projection.
fn virtual_proj(p: &Plan) -> Option<(&ScanPlan, &Vec<Expr>)> {
    let Plan::Project { input, exprs } = p else {
        return None;
    };
    let Plan::Scan(sp) = &**input else {
        return None;
    };
    let ok = exprs.iter().enumerate().all(|(k, e)| match e {
        Expr::Col(j, _) => *j == k,
        _ => sp.table.columns.get(k).is_some_and(|c| c.virtual_gen),
    });
    (ok && exprs.len() == crate::sql::binder::plan_width(input)).then_some((sp, exprs))
}

/// A member scan of an inheritance Append leaf.
struct Member {
    /// Its place among the Append's inputs.
    input: usize,
    scan: ScanPlan,
    rel: Rel,
    /// The member column of each of the leaf's (parent) columns.
    map: Vec<Option<usize>>,
    /// What each of the leaf's columns is over the member's row (a UNION ALL member's
    /// projection: an expression of its columns).
    exprs: Vec<Option<Expr>>,
}

/// A join clause of the group, over the group's row.
#[derive(Clone)]
struct Clause {
    expr: Expr,
    leaves: u32,
    /// `a = b` with the sides over different leaves: (a, b, a's leaves, b's leaves).
    eq: Option<(Expr, Expr, u32, u32)>,
    hashable: bool,
    mergeable: bool,
    cost: f64,
    /// An outer join's own clause (ON), not one pushed down to it.
    on: bool,
    /// An equality its equivalence class implies, not written in the query.
    derived: bool,
    /// The outer join this is the condition of (`outerjoins`), the items the clause needs
    /// joined before it is checked, and the items whose null-extended rows it must see.
    sj: Option<usize>,
    req: u32,
    nullable: u32,
    /// The selectivities of the restrictions extracted from this OR clause for single
    /// items (`or_restrictions`), which its own selectivity is divided by in turn so the
    /// join's size stays as it was.
    sel_div: Vec<f64>,
}

/// A pathkey of the group: an equivalence class and a direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GKey {
    ec: usize,
    desc: bool,
    nulls_first: bool,
}

#[derive(Clone)]
enum JNode {
    /// A scan leaf by one of its paths; parameterized: the outer expressions (over the
    /// group row) its `Outer(1, k)` values are.
    Scan {
        leaf: usize,
        path: Path,
        params: Vec<Expr>,
        /// The join clauses the scan enforces.
        enforced: Vec<usize>,
        rel: Rc<Rel>,
    },
    Opaque {
        leaf: usize,
        /// What the leaf's top node is (rescan costs).
        kind: InKind,
        /// A LATERAL item's values from the outer side (over the group row).
        params: Vec<Expr>,
    },
    /// An inheritance Append leaf as parameterized scans of its members (in `members`
    /// order).
    Append {
        leaf: usize,
        scans: Vec<(Path, Rc<Rel>)>,
        params: Vec<Expr>,
        enforced: Vec<usize>,
    },
    Material(Rc<JP>),
    /// A Memoize over a parameterized inner path: its cache keys (over the group row),
    /// mode, single-row-ness and rescan costs.
    Memoize {
        input: Rc<JP>,
        keys: Vec<Expr>,
        binary: bool,
        singlerow: bool,
        rescan: (f64, f64),
    },
    /// A semi join's right side made unique on its join columns (create_unique_path):
    /// hashed (HashAggregate) or sorted (Unique over a Sort).
    Unique {
        input: Rc<JP>,
        hashed: bool,
        cols: Vec<usize>,
    },
    Join {
        method: JoinMethod,
        jt: Jt,
        outer: Rc<JP>,
        inner: Rc<JP>,
        /// Clauses checked by the join itself (by index), and the hash / merge ones.
        quals: Vec<usize>,
        keys: Vec<usize>,
        /// Explicit sorts of the inputs (merge joins).
        outer_sort: Option<Vec<GKey>>,
        inner_sort: Option<Vec<GKey>>,
        material: bool,
        inner_unique: bool,
        semi: Option<Semi>,
        indexed: bool,
        scansel: Option<(f64, f64, f64, f64)>,
        rows: f64,
        batches: u32,
    },
}

/// A path of a group relation.
#[derive(Clone)]
struct JP {
    node: JNode,
    rows: f64,
    startup: f64,
    total: f64,
    width: u32,
    disabled: u32,
    keys: Vec<GKey>,
    /// Leaves a parameterized path needs values from.
    req: u32,
    set: u32,
}

impl JP {
    /// The path a Memoize caches (else this one).
    fn memoized(&self) -> &JP {
        match &self.node {
            JNode::Memoize { input, .. } => input,
            _ => self,
        }
    }

    fn input(&self) -> In {
        In {
            rows: self.rows,
            startup: self.startup,
            total: self.total,
            width: self.width,
            disabled: self.disabled,
            kind: match &self.node {
                JNode::Material(_) => InKind::Material,
                JNode::Memoize { rescan, .. } => InKind::Memoize(rescan.0, rescan.1),
                JNode::Opaque { kind, .. } => *kind,
                JNode::Append { .. } | JNode::Unique { .. } => InKind::Plain,
                JNode::Join {
                    method: JoinMethod::Hash,
                    batches,
                    ..
                } => InKind::HashJoin(*batches),
                JNode::Join { .. } => InKind::Join,
                // (Sequential and bitmap scans can't mark and restore; index scans and
                // sorts can.)
                JNode::Scan { path, .. } => match path.kind {
                    PathKind::Seq | PathKind::Bitmap(_) | PathKind::Tid(..) | PathKind::TidRange(_) => {
                        InKind::NoMark
                    }
                    _ => InKind::Plain,
                },
            },
        }
    }
}

/// A relation of the search: a set of leaves.
struct JRel {
    set: u32,
    rows: f64,
    width: u32,
    paths: Vec<JP>,
    /// Parameterized paths (base relations): the cheapest for each set of required leaves.
    param: Vec<JP>,
}

/// add_path for group paths (parameterized ones keep to their own required set).
/// Whether a FROM item's plan checks clause `q` (over its row) already: its scan's filter,
/// every member of its Append, through a member's projection of columns.
fn has_qual(p: &Plan, q: &Expr) -> bool {
    let in_parts = |f: &Expr, q: &Expr| {
        let mut parts = Vec::new();
        conjuncts(f.clone(), &mut parts);
        parts.iter().any(|x| {
            x.same_as(q)
                || matches!((x, q), (Expr::Call(_, xa, _), Expr::Call(_, qa, _))
                    if xa.len() == 2 && xa[0].same_as(&qa[1]) && xa[1].same_as(&qa[0]))
        })
    };
    match p {
        Plan::Scan(sp) => sp.filter.as_ref().is_some_and(|f| in_parts(f, q)),
        Plan::Filter { input, pred } => in_parts(pred, q) || has_qual(input, q),
        Plan::Append { inputs, inh: Some(_) } => !inputs.is_empty() && inputs.iter().all(|i| has_qual(i, q)),
        Plan::Project { input, exprs } if exprs.iter().all(|e| matches!(e, Expr::Col(..))) => {
            let mapped = q
                .clone()
                .transform(&mut |x| {
                    Ok(match x {
                        Expr::Col(i, t) => match exprs.get(i) {
                            Some(Expr::Col(j, _)) => Expr::Col(*j, t),
                            _ => Expr::Col(i, t),
                        },
                        x => x,
                    })
                })
                .unwrap();
            has_qual(input, &mapped)
        }
        _ => false,
    }
}

fn add_jpath(list: &mut Vec<JP>, new: JP, startup: bool) {
    let as_path = |p: &JP| Path {
        kind: PathKind::Seq,
        startup: p.startup,
        total: p.total,
        rows: p.rows,
        keys: p
            .keys
            .iter()
            .map(|k| PathKey {
                expr: Expr::Col(k.ec, DataType::Int4),
                desc: k.desc,
                nulls_first: k.nulls_first,
                cmp: 0,
            })
            .collect(),
        disabled: p.disabled,
    };
    let np = as_path(&new);
    let mut accept = true;
    let mut insert_at = 0;
    let mut i = 0;
    while i < list.len() {
        let old = &list[i];
        if old.req != new.req {
            i += 1;
            continue;
        }
        let op = as_path(old);
        let mut remove_old = false;
        let cc = fuzzy_cmp(&np, &op, FUZZ, startup);
        if cc != CostCmp::Different {
            let kc = cmp_keys(&np.keys, &op.keys);
            if kc != KeysCmp::Different {
                match cc {
                    CostCmp::Equal => match kc {
                        KeysCmp::Better1 => remove_old = new.rows <= old.rows,
                        KeysCmp::Better2 => accept = new.rows < old.rows,
                        _ => {
                            if new.rows < old.rows {
                                remove_old = true
                            } else if new.rows > old.rows {
                                accept = false
                            } else if fuzzy_cmp(&np, &op, 1.0000000001, startup) == CostCmp::Better1
                            {
                                remove_old = true
                            } else {
                                accept = false
                            }
                        }
                    },
                    CostCmp::Better1 => {
                        if kc != KeysCmp::Better2 && new.rows <= old.rows {
                            remove_old = true
                        }
                    }
                    CostCmp::Better2 => {
                        if kc != KeysCmp::Better1 && new.rows >= old.rows {
                            accept = false
                        }
                    }
                    CostCmp::Different => {}
                }
            }
        }
        if remove_old {
            list.remove(i);
        } else {
            if new.disabled > old.disabled
                || (new.disabled == old.disabled && new.total >= old.total)
            {
                insert_at = i + 1;
            }
            i += 1;
        }
        if !accept {
            return;
        }
    }
    list.insert(insert_at.min(list.len()), new);
}

fn cheaper(a: &JP, b: &JP, startup: bool) -> bool {
    if a.disabled != b.disabled {
        return a.disabled < b.disabled;
    }
    let (x, y, x2, y2) = if startup {
        (a.startup, b.startup, a.total, b.total)
    } else {
        (a.total, b.total, a.startup, b.startup)
    };
    if x != y {
        return x < y;
    }
    x2 < y2
}

/// set_cheapest: the cheapest unparameterized path by total (or startup) cost.
fn cheapest_of(list: &[JP], startup: bool) -> Option<&JP> {
    let mut best: Option<&JP> = None;
    for p in list.iter().filter(|p| p.req == 0) {
        if best.is_none_or(|b| cheaper(p, b, startup)) {
            best = Some(p);
        }
    }
    best
}

struct Search<'s, 'c, 'a> {
    pl: &'s Planner<'c, 'a>,
    g: JoinGucs,
    leaves: Vec<Leaf>,
    clauses: Vec<Clause>,
    /// The group column of each leaf column, and the leaf of each group column.
    leaf_of: Vec<usize>,
    /// The group's columns needed above it.
    needed: Vec<bool>,
    /// Equivalence class of each group column (a singleton unless equated).
    ec: Vec<usize>,
    /// The join type of a two-leaf outer / semi / anti group (Inner: freely reordered).
    kind: JoinKind,
    rels: Vec<JRel>,
    startup: bool,
    /// The ORDER BY above the group: its keys as far as they are group columns, whether
    /// that is all of them, the LIMIT, and the width the sort sees.
    order: Option<Order>,
    /// The group's outer joins, when it has them inside (`outerjoins`); then the joins'
    /// types come from them.
    sjs: Vec<outerjoins::Sj>,
    general: bool,
    /// The WHERE clauses the group took in (`plan_filtered_join`).
    extra: Vec<Expr>,
    /// The per-row cost of a filter left over the group's top join (`Planner::post_cost`).
    post: f64,
    /// The outer join the join being made implements.
    cur_sj: Option<usize>,
    /// Building a parameterized join path's plan: per enclosing nested loop, the values
    /// (over the group row) it passes in, which a scan inside reads as `Outer(1, k)`.
    param_stack: std::cell::RefCell<Vec<Vec<Expr>>>,
    /// A partitionwise child join's parent leaves' rows (`Planner::pw_loop_rows`).
    loop_rows: Vec<f64>,
}

/// The join clauses a parameterized path enforces (its scans', through its joins).
fn enforced_of(p: &JP) -> Vec<usize> {
    match &p.node {
        JNode::Scan { enforced, .. } | JNode::Append { enforced, .. } => enforced.clone(),
        JNode::Memoize { input, .. } | JNode::Material(input) => enforced_of(input),
        JNode::Join { outer, inner, quals, .. } => {
            let mut v = enforced_of(outer);
            v.extend(enforced_of(inner));
            // (A parameterized join checks the clauses joining it to its parameters.)
            if p.req != 0 {
                v.extend(quals.iter().copied());
            }
            v
        }
        _ => Vec::new(),
    }
}

/// An ORDER BY over a join group.
#[derive(Clone)]
pub(super) struct Order {
    /// (group column, descending, nulls first) per key, while they are plain columns.
    pub keys: Vec<(usize, bool, bool)>,
    pub complete: bool,
    pub limit: Option<(f64, f64)>,
    pub width: u32,
    /// The order of a grouping above (its query_pathkeys): useful to the joins' paths,
    /// but the group isn't sorted for it here.
    pub hint: bool,
}

/// The column a key expression is, through the binder's widening casts.
/// An expression a merge join can sort its side by: of that side's columns, nothing
/// volatile, no sub-select or aggregate.
fn merge_expr(e: &Expr) -> bool {
    let mut ok = !e.columns().is_empty() && !e.is_volatile();
    e.walk(&mut |x| ok &= !matches!(x, Expr::Subquery(_) | Expr::Agg(..) | Expr::Window(..) | Expr::Srf(..) | Expr::Outer(..)));
    ok
}

fn key_col(e: &Expr) -> Option<usize> {
    match key_operand(e) {
        Expr::Col(i, _) => Some(*i),
        // (A COLLATE is a RelabelType to Postgres: the column, as examine_variable sees it.)
        Expr::Collate(x, ..) => key_col(x),
        _ => None,
    }
}

fn mergeable(t: &DataType) -> bool {
    use DataType as T;
    !matches!(
        t,
        T::Json
            | T::Xml
            | T::Point
            | T::Lseg
            | T::Path
            | T::GBox
            | T::Polygon
            | T::Line
            | T::Circle
            | T::Xid
            | T::Cid
            | T::Aclitem
            | T::Unknown
    )
}

impl<'s, 'c, 'a> Search<'s, 'c, 'a> {
    fn leaves_of(&self, e: &Expr) -> u32 {
        let mut m = 0u32;
        for c in e.columns() {
            if let Some(&l) = self.leaf_of.get(c) {
                m |= 1 << l;
            }
        }
        m
    }

    /// The statistics variable behind a group column.
    fn var_of(&self, g: usize) -> (Option<VarRef>, Option<f64>, f64) {
        let l = self.leaf_of[g];
        let leaf = &self.leaves[l];
        let (_, v, d) = leaf.cols[g - leaf.off].clone();
        let v = v.map(|mut v| {
            if v.rows <= 0.0 {
                v.rows = leaf.rows;
            }
            v
        });
        (v, d, leaf.rows)
    }

    fn join_var(&self, e: &Expr) -> joins::JoinVar {
        match key_col(e) {
            Some(g) => {
                let (var, distinct, rows) = self.var_of(g);
                joins::JoinVar {
                    rel_rows: var.as_ref().map_or(rows, |v| v.rows),
                    var,
                    distinct,
                }
            }
            None => {
                let leaves = self.leaves_of(e);
                // examine_variable: an expression an index (or statistics object) of
                // the table has statistics for.
                if leaves.count_ones() == 1 {
                    let leaf = &self.leaves[leaves.trailing_zeros() as usize];
                    if let Some((_, r)) = &leaf.scan {
                        let local = key_operand(e).clone().shift_cols(-(leaf.off as isize));
                        if let Some((_, v)) = r.exprs.iter().find(|(x, _)| x.same_as(&local)) {
                            let mut v = v.clone();
                            v.rows = leaf.rows;
                            return joins::JoinVar {
                                rel_rows: leaf.rows,
                                var: Some(v),
                                distinct: None,
                            };
                        }
                    }
                }
                // An expression of one table's columns without statistics:
                // get_variable_numdistinct takes the table's tuples when fewer than the
                // default.
                let distinct = (leaves.count_ones() == 1)
                    .then(|| &self.leaves[leaves.trailing_zeros() as usize])
                    .and_then(|l| l.scan.as_ref())
                    .map(|(_, r)| r.size.tuples)
                    .filter(|t| *t < costs::DEFAULT_NUM_DISTINCT)
                    .map(costs::clamp_rows);
                joins::JoinVar {
                    var: None,
                    distinct,
                    rel_rows: 0.0,
                }
            }
        }
    }

    /// clause_selectivity of a join clause (JOIN_INNER semantics, or the semi one).
    fn clause_sel(&self, c: &Clause, semi_inner: Option<(u32, f64)>) -> f64 {
        let mut s = self.clause_sel_raw(c, semi_inner);
        // (consider_new_or_clause's adjustment of the cached selectivity.)
        for d in &c.sel_div {
            s = (s / d).min(1.0);
        }
        s
    }

    /// extract_restriction_or_clauses for item `l`: from each join OR clause that can be
    /// moved to it and has something to say about it alone in every arm, the OR of those
    /// parts (over the group row), with the clause's index.
    fn or_restrictions(&self, l: usize) -> Vec<(usize, Expr)> {
        let bit = 1u32 << l;
        let mut out = Vec::new();
        for (ci, c) in self.clauses.iter().enumerate() {
            if c.derived
                || c.leaves & bit == 0
                || c.leaves == bit
                || !matches!(c.expr, Expr::Or(_))
                || !self.movable_to(ci, l)
            {
                continue;
            }
            if let Some(q) = self.extract_or(&c.expr, bit) {
                out.push((ci, q));
            }
        }
        out
    }

    /// extract_or_clause: an OR's arms' parts that are restrictions of `bit` alone, still
    /// ORed (None: some arm has none).
    fn extract_or(&self, e: &Expr, bit: u32) -> Option<Expr> {
        let Expr::Or(arms) = e else {
            return None;
        };
        let mut out = Vec::new();
        for arm in arms {
            let parts: Vec<&Expr> = match arm {
                Expr::And(xs) => xs.iter().collect(),
                x => vec![x],
            };
            let mut subs = Vec::new();
            for p in parts {
                if matches!(p, Expr::Or(_)) {
                    if let Some(x) = self.extract_or(p, bit) {
                        subs.push(x);
                    }
                } else if self.leaves_of(p) == bit && !p.is_volatile() {
                    subs.push(p.clone());
                }
            }
            if subs.is_empty() {
                return None;
            }
            // (Kept flat: no OR right under an OR.)
            match if subs.len() == 1 { subs.pop().unwrap() } else { Expr::And(subs) } {
                Expr::Or(xs) => out.extend(xs),
                x => out.push(x),
            }
        }
        Some(Expr::Or(out))
    }

    fn clause_sel_raw(&self, c: &Clause, semi_inner: Option<(u32, f64)>) -> f64 {
        let cat = &self.pl.cat;
        let fmt = self.pl.ctx.fmt();
        if let Some((a, b, al, bl)) = &c.eq {
            let (ja, jb) = (self.join_var(a), self.join_var(b));
            if let Some((inner_set, inner_rows)) = semi_inner {
                // eqjoinsel_semi with the outer side first.
                let (o, i) = if *bl & inner_set != 0 && *al & inner_set == 0 {
                    (ja, jb)
                } else {
                    (jb, ja)
                };
                let s = joins::eqjoinsel_semi(cat, fmt, &o, &i, inner_rows);
                return s
                    .min(inner_rows * joins::eqjoinsel_inner(cat, fmt, &o, &i))
                    .clamp(0.0, 1.0);
            }
            return joins::eqjoinsel_inner(cat, fmt, &ja, &jb);
        }
        let vars = |i: usize| self.var_of(i).0;
        let est = Est {
            cat,
            fmt,
            vars: &vars,
            exprs: &[],
            endpoint: Some(&|v, min| self.pl.endpoint(v, min)),
            distinct: None,
            user: None,
        };
        est.clause(&c.expr)
    }

    /// The equivalence class a merge clause's side sorts by: its column's, or for an
    /// expression the class of the column a mergeable clause equates it with.
    fn side_ec(&self, e: &Expr) -> Option<usize> {
        if let Some(g) = key_col(e) {
            return Some(self.ec[g]);
        }
        let ec = self.clauses.iter().filter(|c| c.mergeable).find_map(|c| {
            let (a, b, _, _) = c.eq.as_ref()?;
            if a.same_as(e) {
                key_col(b).map(|g| self.ec[g])
            } else if b.same_as(e) {
                key_col(a).map(|g| self.ec[g])
            } else {
                None
            }
        })?;
        // (Not when a column of the expression's own items is in the class: an order by
        // that column isn't the expression's, no clause between them making it so.)
        let mine = self.leaves_of(e);
        (!(0..self.ec.len()).any(|h| self.ec[h] == ec && (1u32 << self.leaf_of[h]) & mine != 0)).then_some(ec)
    }

    /// An expression member of class `ec` computable over the columns `layout` has: the
    /// expression side of a mergeable clause whose column side is in the class.
    fn ec_expr_in(&self, ec: usize, layout: &[usize]) -> Option<Expr> {
        self.clauses.iter().filter(|c| c.mergeable).find_map(|c| {
            let (a, b, _, _) = c.eq.as_ref()?;
            for (x, y) in [(a, b), (b, a)] {
                if key_col(x).is_none()
                    && key_col(y).is_some_and(|g| self.ec[g] == ec)
                    && x.columns().iter().all(|g| layout.contains(g))
                {
                    return Some(x.clone());
                }
            }
            None
        })
    }

    /// The columns the equivalence class of a mergeable column equality clause has, in
    /// the order its clauses brought them in (None: not such a clause).
    fn clause_ec(&self, c: &Clause) -> Option<usize> {
        // (A semi join's own clause equates its sides as an inner join's does.)
        if !c.mergeable || (c.on && !(self.kind == JoinKind::Semi && !self.general)) {
            return None;
        }
        let (a, b, _, _) = c.eq.as_ref()?;
        let (x, y) = (key_col(a)?, key_col(b)?);
        (self.ec[x] == self.ec[y]).then_some(self.ec[x])
    }

    /// The equivalence class members (group columns) in the order their clauses list them.
    fn ec_members(&self) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        for c in self.clauses.iter().filter(|c| !c.derived) {
            if self.clause_ec(c).is_none() {
                continue;
            }
            let (a, b, _, _) = c.eq.as_ref().unwrap();
            for g in [key_col(a), key_col(b)].into_iter().flatten() {
                if !out.contains(&g) {
                    out.push(g);
                }
            }
        }
        out
    }

    /// Add the equalities between equivalence class members of different FROM items the
    /// query doesn't write.
    fn derive_equalities(&mut self) {
        let members = self.ec_members();
        let n = self.clauses.len();
        let mut add = Vec::new();
        for (i, &x) in members.iter().enumerate() {
            for &y in &members[i + 1..] {
                if self.ec[x] != self.ec[y] || self.leaf_of[x] == self.leaf_of[y] {
                    continue;
                }
                let exists = self.clauses[..n].iter().any(|c| {
                    c.eq.as_ref().is_some_and(|(a, b, _, _)| {
                        let (p, q) = (key_col(a), key_col(b));
                        (p == Some(x) && q == Some(y)) || (p == Some(y) && q == Some(x))
                    })
                });
                if exists {
                    continue;
                }
                // The class's operator, from one of its clauses over the same types.
                let (tx, ty) = (self.col_type(x), self.col_type(y));
                let Some(template) = self.clauses[..n].iter().find(|c| {
                    self.clause_ec(c) == Some(self.ec[x])
                        && c.eq.as_ref().is_some_and(|(a, b, _, _)| {
                            a.ty().without_typmod() == tx.without_typmod()
                                && b.ty().without_typmod() == ty.without_typmod()
                        })
                }) else {
                    continue;
                };
                let Expr::Call(def, _, rt) = &template.expr else {
                    continue;
                };
                let (ex, ey) = (Expr::Col(x, tx.clone()), Expr::Col(y, ty.clone()));
                let (lx, ly) = (1u32 << self.leaf_of[x], 1u32 << self.leaf_of[y]);
                let expr = Expr::Call(def.clone(), vec![ex.clone(), ey.clone()], rt.clone());
                add.push(Clause {
                    cost: costs::qual_cost(&expr),
                    expr,
                    leaves: lx | ly,
                    eq: Some((ex, ey, lx, ly)),
                    hashable: template.hashable,
                    mergeable: true,
                    on: false,
                    derived: true,
                    sj: None,
                    req: lx | ly,
                    nullable: 0,
                    sel_div: Vec::new(),
                });
            }
        }
        self.clauses.extend(add);
    }

    /// The orientation an equivalence class's join clauses get from their first
    /// derivation (create_join_clause reuses a clause either way round, never an original
    /// one): before the join search, check_index_predicates for each relation with a
    /// partial index (the other relations' member first), then the index paths of each
    /// relation with an index on a member (its member first), relations in order.
    fn orient_ec_clauses(&mut self) {
        let members = self.ec_members();
        if members.len() < 2 {
            return;
        }
        let mut oriented: Vec<(usize, usize)> = Vec::new();
        let known = |o: &[(usize, usize)], x: usize, y: usize| o.iter().any(|&(p, q)| (p == x && q == y) || (p == y && q == x));
        let leaf_cols = |s: &Self, l: usize, c: usize| -> Option<usize> {
            let g = s.leaves[l].off + c;
            members.contains(&g).then_some(g)
        };
        let nleaves = self.leaves.len();
        // (check_index_predicates: one clause per class, its first member in the other
        // relations and its first in this one.)
        for l in 0..nleaves {
            let Some((_, rel)) = &self.leaves[l].scan else { continue };
            if !rel.indexes.iter().any(|ii| !ii.pred.is_empty()) {
                continue;
            }
            let mut ecs: Vec<usize> = Vec::new();
            for &g in &members {
                if self.leaf_of[g] == l && !ecs.contains(&self.ec[g]) {
                    ecs.push(self.ec[g]);
                }
            }
            for e in ecs {
                let outer = members.iter().copied().find(|&g| self.ec[g] == e && self.leaf_of[g] != l);
                let inner = members.iter().copied().find(|&g| self.ec[g] == e && self.leaf_of[g] == l);
                if let (Some(x), Some(y)) = (outer, inner)
                    && !known(&oriented, x, y)
                {
                    oriented.push((x, y));
                }
            }
        }
        // (match_eclass_clauses_to_index: the indexed member with every other one.)
        for l in 0..nleaves {
            let Some((_, rel)) = &self.leaves[l].scan else { continue };
            let mut cols: Vec<usize> = Vec::new();
            for ii in &rel.indexes {
                if !matches!(ii.am, crate::sql::paths::Am::Btree | crate::sql::paths::Am::Hash) {
                    continue;
                }
                for k in &ii.keys {
                    if let Expr::Col(c, _) = k
                        && !cols.contains(c)
                    {
                        cols.push(*c);
                    }
                }
            }
            for c in cols {
                let Some(g) = leaf_cols(self, l, c) else { continue };
                for &h in &members {
                    if self.ec[h] == self.ec[g] && self.leaf_of[h] != l && !known(&oriented, g, h) {
                        oriented.push((g, h));
                    }
                }
            }
        }
        for c in self.clauses.iter_mut() {
            if !c.mergeable || c.on {
                continue;
            }
            let Some((a, b, _, _)) = c.eq.clone() else { continue };
            let (Some(x), Some(y)) = (key_col(&a), key_col(&b)) else { continue };
            if !oriented.contains(&(y, x)) || a.ty().without_typmod() != b.ty().without_typmod() {
                continue;
            }
            let Expr::Call(def, args, rt) = &c.expr else { continue };
            if args.len() != 2 {
                continue;
            }
            // (Only the clause as shown: the class's members keep the order their clauses
            // brought them in.)
            c.expr = Expr::Call(def.clone(), vec![args[1].clone(), args[0].clone()], rt.clone());
        }
    }

    /// The clauses joining `a` and `b` (in a set together, in neither alone); clauses of
    /// one leaf or none (an outer join's ON condition on its preserved side, a volatile
    /// one) belong to the last join. Of an equivalence class's equalities just one joins
    /// them: its first member of `a` with its first of `b`.
    fn crossing(&self, a: u32, b: u32) -> Vec<usize> {
        if self.general {
            return self.general_restrict(a, b);
        }
        let u = a | b;
        let full = (1u32 << self.leaves.len()) - 1;
        let all: Vec<usize> = (0..self.clauses.len())
            .filter(|&i| {
                let l = self.clauses[i].leaves;
                l & !u == 0
                    && ((l & !a != 0 && l & !b != 0) || (l.count_ones() <= 1 && u == full))
            })
            .collect();
        if !matches!(self.kind, JoinKind::Inner | JoinKind::Semi) && !self.clauses.iter().any(|c| c.derived) {
            return all;
        }
        let members = self.ec_members();
        let in_set = |g: usize, s: u32| (1u32 << self.leaf_of[g]) & s != 0;
        let mut chosen: Vec<(usize, usize)> = Vec::new(); // (ec, clause)
        let mut out = Vec::new();
        for &i in &all {
            let c = &self.clauses[i];
            let Some(e) = self.clause_ec(c) else {
                if !c.derived {
                    out.push(i);
                }
                continue;
            };
            if chosen.iter().any(|(x, _)| *x == e) {
                continue;
            }
            // generate_join_implied_equalities_normal: the first members on each side.
            let first = |s: u32| {
                members
                    .iter()
                    .copied()
                    .find(|&g| self.ec[g] == e && in_set(g, s))
            };
            let pick = match (first(a), first(b)) {
                (Some(x), Some(y)) => all.iter().copied().find(|&j| {
                    self.clauses[j].eq.as_ref().is_some_and(|(p, q, _, _)| {
                        let (p, q) = (key_col(p), key_col(q));
                        (p == Some(x) && q == Some(y)) || (p == Some(y) && q == Some(x))
                    })
                }),
                _ => None,
            };
            match pick {
                Some(j) => {
                    chosen.push((e, j));
                    out.push(j);
                }
                // generate_join_implied_equalities_broken: no equality between those
                // members (none written, none derivable): the class's own clauses here.
                None => {
                    chosen.push((e, i));
                    for &j in &all {
                        let cj = &self.clauses[j];
                        if !cj.derived && self.clause_ec(cj) == Some(e) && !out.contains(&j) {
                            out.push(j);
                        }
                    }
                }
            }
        }
        out.sort_unstable();
        out
    }

    fn jt(&self) -> Jt {
        match self.kind {
            JoinKind::Inner => Jt::Inner,
            JoinKind::Left => Jt::Left,
            JoinKind::Right => Jt::Right,
            JoinKind::Full => Jt::Full,
            JoinKind::Semi => Jt::Semi,
            JoinKind::Anti => Jt::Anti,
        }
    }

    /// The width of a relation's columns needed above it (build_joinrel_tlist).
    fn width_of(&self, set: u32) -> u32 {
        let mut w = 0;
        for (l, leaf) in self.leaves.iter().enumerate() {
            if set & (1 << l) == 0 {
                continue;
            }
            for k in 0..leaf.ncols {
                let g = leaf.off + k;
                if self.needed_above(g, set) {
                    w += leaf.cols[k].0;
                }
            }
        }
        w
    }

    /// Column `g` is needed above relation `set`: by the group's consumers or a clause
    /// reaching outside the set.
    fn needed_above(&self, g: usize, set: u32) -> bool {
        if self.needed.get(g).copied().unwrap_or(false) {
            return true;
        }
        self.clauses
            .iter()
            .any(|c| c.leaves & !set != 0 && c.expr.columns().contains(&g))
    }

    fn rel_index(&self, set: u32) -> Option<usize> {
        self.rels.iter().position(|r| r.set == set)
    }
}

// ── base relations ──

impl Search<'_, '_, '_> {
    /// A sorted subquery leaf's order (convert_subquery_pathkeys), as far as it is useful
    /// to a merge join or the query's order (truncate_useless_pathkeys).
    fn opaque_keys(&self, l: usize) -> Vec<GKey> {
        let leaf = &self.leaves[l];
        let mut p = &leaf.plan;
        // (Column positions of the leaf's row in the current node's row.)
        let mut map: Vec<Option<usize>> = (0..leaf.ncols).map(Some).collect();
        let mut limit_keys: Option<usize> = None;
        let mut keys = loop {
            match p {
                // (A subquery planned apart: its ORDER BY's keys at most.)
                Plan::Barrier { input, sorted, .. } => {
                    if let Some(n) = sorted {
                        limit_keys = Some(limit_keys.map_or(*n, |m| m.min(*n)));
                    }
                    p = input;
                }
                Plan::Limit { input, .. } | Plan::LockRows { input, .. } | Plan::Filter { input, .. } => p = input,
                Plan::Project { input, exprs } => {
                    map = map
                        .iter()
                        .map(|m| m.and_then(|i| match exprs.get(i) {
                            Some(Expr::Col(j, _)) => Some(*j),
                            _ => None,
                        }))
                        .collect();
                    p = input;
                }
                Plan::Sort { keys, .. } => break keys.clone(),
                // (A materialized CTE's scan keeps its query's order.)
                Plan::CteScan { name, .. } => break crate::sql::explain::cte_order(name),
                // (A subquery's ORDER BY an index scan gives.)
                Plan::Scan(sp) if matches!(sp.access, Access::Index { ordered: true, .. }) => {
                    break crate::sql::paths::index_order(self.pl.ctx, sp);
                }
                _ => return Vec::new(),
            }
        };
        if let Some(n) = limit_keys {
            keys.truncate(n);
        }
        let mut out: Vec<GKey> = Vec::new();
        for k in &keys {
            let Expr::Col(c, _) = &k.expr else { break };
            if k.using.is_some() {
                break;
            }
            let Some(local) = map.iter().position(|m| *m == Some(*c)) else {
                break;
            };
            let g = leaf.off + local;
            let gk = GKey {
                ec: self.ec[g],
                desc: k.desc,
                nulls_first: k.nulls_first,
            };
            if out.iter().any(|x| x.ec == gk.ec) {
                continue;
            }
            out.push(gk);
        }
        // truncate_useless_pathkeys: for the query's order, or a merge join on a clause.
        let q = self.query_keys();
        let ordering = out.iter().zip(&q).take_while(|(a, b)| a == b).count();
        let merging = out
            .iter()
            .take_while(|k| {
                self.clauses.iter().any(|c| {
                    (c.hashable || c.mergeable)
                        && c.eq.as_ref().is_some_and(|(a, b, _, _)| {
                            [a, b].iter().any(|e| key_col(e).is_some_and(|g| {
                                self.ec[g] == k.ec && (1u32 << self.leaf_of[g]) == 1u32 << l
                            }))
                        })
                })
            })
            .count();
        out.truncate(ordering.max(merging));
        out
    }

    /// The leaf's paths: its scan's (merge-useful orderings too) and parameterized index
    /// scans, or its one estimate.
    fn base_rel(&mut self, l: usize) {
        let set = 1u32 << l;
        let width = self.width_of(set);
        let leaf = &self.leaves[l];
        let mut rel = JRel {
            set,
            rows: leaf.rows,
            width,
            paths: Vec::new(),
            param: Vec::new(),
        };
        match &leaf.scan {
            None => {
                let (params, req) = leaf.lateral.clone().unwrap_or_default();
                let keys = self.opaque_keys(l);
                let jp = JP {
                    node: JNode::Opaque {
                        leaf: l,
                        kind: leaf.kind,
                        params,
                    },
                    rows: leaf.rows,
                    startup: leaf.startup,
                    total: leaf.total,
                    width,
                    disabled: 0,
                    keys,
                    req,
                    set,
                };
                // A LATERAL item has only its path parameterized by what it reads.
                if req != 0 {
                    rel.param.push(jp);
                } else {
                    rel.paths.push(jp);
                    rel.param = self.append_param_paths(l, width);
                }
            }
            Some((_, r)) => {
                let r = Rc::new(r.shallow_with_quals());
                // Merge-useful columns: those equated to another leaf's.
                let merge: Vec<Expr> = (0..leaf.ncols)
                    .filter(|k| {
                        let g = leaf.off + k;
                        self.ec_has_other(g, set)
                    })
                    .filter(|k| *k < r.table.columns.len())
                    .map(|k| Expr::Col(k, r.table.columns[k].ty.clone()))
                    .collect();
                // The ORDER BY's keys this leaf has a column for (a prefix).
                let mut keys = Vec::new();
                for q in self.query_keys() {
                    let col = (0..leaf.ncols.min(r.table.columns.len()))
                        .find(|k| self.ec[leaf.off + k] == q.ec);
                    match col {
                        Some(k) => keys.push(PathKey {
                            expr: Expr::Col(k, r.table.columns[k].ty.clone()),
                            desc: q.desc,
                            nulls_first: q.nulls_first,
                            cmp: 0,
                        }),
                        None => break,
                    }
                }
                let want = Want {
                    keys,
                    limit: None,
                    merge,
                };
                for p in self.pl.paths(&r, &want) {
                    let keys = self.gkeys(l, &p.keys);
                    rel.paths.push(JP {
                        rows: p.rows,
                        startup: p.startup,
                        total: p.total,
                        width,
                        disabled: p.disabled,
                        keys,
                        req: 0,
                        set,
                        node: JNode::Scan {
                            leaf: l,
                            path: p,
                            params: Vec::new(),
                            enforced: Vec::new(),
                            rel: r.clone(),
                        },
                    });
                }
                rel.param = self.param_paths(l, &r, width);
            }
        }
        self.rels.push(rel);
    }

    /// A column's equivalence class has a member in another leaf than `set`'s.
    fn ec_has_other(&self, g: usize, set: u32) -> bool {
        let e = self.ec[g];
        (0..self.ec.len()).any(|h| h != g && self.ec[h] == e && (1u32 << self.leaf_of[h]) & set == 0)
            // (An outer join's own clause: its sides are classes of their own.)
            || self.clauses.iter().any(|c| {
                c.mergeable
                    && c.eq.as_ref().is_some_and(|(a, b, al, bl)| {
                        (key_col(a).is_some_and(|x| self.ec[x] == e) && bl & set == 0)
                            || (key_col(b).is_some_and(|y| self.ec[y] == e) && al & set == 0)
                    })
            })
    }

    /// Path keys over leaf `l`'s table row as group keys.
    fn gkeys(&self, l: usize, keys: &[PathKey]) -> Vec<GKey> {
        let off = self.leaves[l].off;
        let mut out = Vec::new();
        for k in keys {
            // (A made operator family's order is none the group's keys stand for.)
            if k.cmp != 0 {
                break;
            }
            match &k.expr {
                Expr::Col(c, _) => out.push(GKey {
                    ec: self.ec[off + c],
                    desc: k.desc,
                    nulls_first: k.nulls_first,
                }),
                _ => break,
            }
        }
        out
    }

    /// Parameterized index paths of a scan leaf: for each other leaf (set) with join
    /// clauses an index can use, the cheapest path using them (get_join_index_paths).
    fn param_paths(&self, l: usize, r: &Rc<Rel>, width: u32) -> Vec<JP> {
        let leaf = &self.leaves[l];
        let set = 1u32 << l;
        // (local column or index expression, the outer expression over the group row,
        // its leaves, the `=`).
        let mut cands: Vec<(Expr, Expr, u32, usize)> = Vec::new();
        let leaves_of = |e: &Expr| -> u32 {
            e.columns()
                .into_iter()
                .fold(0u32, |m, g| m | (1u32 << self.leaf_of[g]))
        };
        for (ci, c) in self.clauses.iter().enumerate() {
            if !self.movable_to(ci, l) {
                continue;
            }
            // An equality, or (as match_clause_to_indexcol takes them too) a btree
            // comparison of this leaf's column with the other leaves' values.
            let sides = match (&c.eq, &c.expr) {
                (Some((a, b, al, bl)), _) => vec![(a.clone(), b.clone(), *al, *bl)],
                (None, Expr::Call(def, args, _))
                    if args.len() == 2 && matches!(def.name, "<" | "<=" | ">" | ">=") =>
                {
                    let (la, lb) = (leaves_of(&args[0]), leaves_of(&args[1]));
                    if la == 0 || lb == 0 || c.expr.is_volatile() {
                        continue;
                    }
                    vec![(args[0].clone(), args[1].clone(), la, lb)]
                }
                _ => continue,
            };
            for (a, b, al, bl) in sides {
                for (mine, other, ml, ol) in [(&a, &b, al, bl), (&b, &a, bl, al)] {
                    if ml != set || ol & set != 0 {
                        continue;
                    }
                    let local = match key_col(mine) {
                        // The row's ctid: a TID scan's condition (TidQualFromRestrictInfo
                        // with a parameter).
                        Some(g)
                            if g >= leaf.off + r.table.columns.len()
                                && g < leaf.off + leaf.ncols
                                && mine.ty() == DataType::Tid
                                && other.ty() == DataType::Tid =>
                        {
                            Expr::Col(g - leaf.off, DataType::Tid)
                        }
                        Some(g) => {
                            if g < leaf.off || g >= leaf.off + r.table.columns.len() {
                                continue;
                            }
                            Expr::Col(g - leaf.off, r.table.columns[g - leaf.off].ty.clone())
                        }
                        // An expression of this leaf's columns an index has as a key.
                        None => {
                            let cols = mine.columns();
                            if cols.is_empty()
                                || cols.iter().any(|&g| g < leaf.off || g >= leaf.off + r.table.columns.len())
                                || mine.is_volatile()
                            {
                                continue;
                            }
                            let local = mine.clone().shift_cols(-(leaf.off as isize));
                            if !r.indexes.iter().any(|ii| ii.keys.iter().any(|k| k.same_as(&local))) {
                                continue;
                            }
                            local
                        }
                    };
                    cands.push((local, other.clone(), ol, ci));
                }
            }
        }
        let tid_cand = cands.iter().any(|c| matches!(c.0, Expr::Col(_, DataType::Tid)) && c.0.columns()[0] >= r.table.columns.len());
        if cands.is_empty() || (r.indexes.is_empty() && !tid_cand) {
            // (Its OR join clauses may still parameterize an index scan or a BitmapOr.)
            return if r.indexes.is_empty() { Vec::new() } else { self.or_param_paths(l, r, width, set) };
        }
        // consider_index_join_outer_rels: each clause's set of outer leaves, and its union
        // with each set tried before (but not with one using its equivalence class
        // already), up to ten sets a clause.
        let mut reqs: Vec<u32> = Vec::new();
        let limit = 10 * cands.len();
        for c in &cands {
            if reqs.contains(&c.2) {
                continue;
            }
            let ec = self.clause_ec(&self.clauses[c.3]);
            for pos in 0..reqs.len() {
                let o = reqs[pos];
                if c.2 & !o == 0 || o & !c.2 == 0 {
                    continue;
                }
                if ec.is_some()
                    && cands
                        .iter()
                        .any(|d| d.2 & !o == 0 && self.clause_ec(&self.clauses[d.3]) == ec)
                {
                    continue;
                }
                if reqs.len() >= limit {
                    break;
                }
                if !reqs.contains(&(c.2 | o)) {
                    reqs.push(c.2 | o);
                }
            }
            if !reqs.contains(&c.2) {
                reqs.push(c.2);
            }
        }
        let mut out: Vec<JP> = Vec::new();
        let members = self.ec_members();
        for req in reqs {
            let used: Vec<&(Expr, Expr, u32, usize)> =
                cands.iter().filter(|c| c.2 & !req == 0).collect();
            // generate_join_implied_equalities: one clause per equivalence class, between its
            // first members on either side (the one the join's restriction list takes), when
            // that is a candidate here.
            let in_set = |g: usize, s: u32| (1u32 << self.leaf_of[g]) & s != 0;
            let pick = |e: usize| -> Option<usize> {
                let first = |s: u32| members.iter().copied().find(|&g| self.ec[g] == e && in_set(g, s));
                let (x, y) = (first(req)?, first(set)?);
                used.iter().map(|c| c.3).find(|&j| {
                    self.clauses[j].eq.as_ref().is_some_and(|(p, q, _, _)| {
                        let (p, q) = (key_col(p), key_col(q));
                        (p == Some(x) && q == Some(y)) || (p == Some(y) && q == Some(x))
                    })
                })
            };
            let used: Vec<&(Expr, Expr, u32, usize)> = used
                .iter()
                .copied()
                .filter(|c| match self.clause_ec(&self.clauses[c.3]) {
                    Some(e) => pick(e).is_none_or(|j| j == c.3),
                    None => true,
                })
                .collect();
            // The clauses with the outer value as a parameter.
            let Some(mut prel) = r.shallow_param() else {
                continue;
            };
            let mut params = Vec::new();
            let mut enforced = Vec::new();
            for (k, (col, other, _, ci)) in used.iter().enumerate() {
                let Expr::Call(def, args, ty) = &self.clauses[*ci].expr else {
                    continue;
                };
                let param = Expr::Outer(1, k, other.ty());
                let mine_first = args[0].clone().shift_cols(-(leaf.off as isize)).same_as(col)
                    || key_col(&args[0]).is_some_and(|g| matches!(col, Expr::Col(c, _) if g == leaf.off + c));
                let colx = col.clone();
                // (A clause of an equivalence class is generated outer member first
                // (generate_join_implied_equalities), unless the column is an index's key:
                // then index matching made it first, the column first
                // (generate_implied_equalities_for_column).)
                let index_key = r.indexes.iter().any(|ii| {
                    ii.keys.iter().any(|k| matches!((k, col), (Expr::Col(a, _), Expr::Col(b, _)) if a == b))
                });
                let symmetric = self.clause_ec(&self.clauses[*ci]).is_some()
                    && args.len() == 2
                    && args[0].ty() == args[1].ty()
                    && def.name == "=";
                // (A written clause between exactly these members is reused as written:
                // ec_search_clause_for_ems.)
                // (A semi join's clause isn't one of its classes' sources: derived afresh.)
                let semi = self.kind == JoinKind::Semi
                    || self.clauses[*ci].sj.is_some_and(|i| self.sjs[i].kind == JoinKind::Semi);
                let mine_first =
                    if symmetric && (self.clauses[*ci].derived || semi) { index_key } else { mine_first };
                let q = if mine_first {
                    Expr::Call(def.clone(), vec![colx, param], ty.clone())
                } else {
                    Expr::Call(def.clone(), vec![param, colx], ty.clone())
                };
                prel.quals.push(q);
                params.push(other.clone());
                enforced.push(*ci);
            }
            // get_baserel_parampathinfo: the other join clauses movable into the leaf with
            // these outer leaves are the path's too (its filter), their values of the
            // outer leaves parameters.
            // (An equality of an equivalence class no clause enforced so far has too:
            // generate_join_implied_equalities' one clause per class.)
            let mut ecs: Vec<usize> = enforced
                .iter()
                .filter_map(|&ci| self.clause_ec(&self.clauses[ci]))
                .collect();
            for (ci, c) in self.clauses.iter().enumerate() {
                let ec_taken = c.eq.is_some() && self.clause_ec(c).is_some_and(|e| ecs.contains(&e));
                if enforced.contains(&ci)
                    || ec_taken
                    || c.leaves & set == 0
                    || c.leaves == set
                    || c.leaves & !(set | req) != 0
                    || c.expr.is_volatile()
                    || !self.movable_to(ci, l)
                {
                    continue;
                }
                let mut plain = true;
                c.expr.walk(&mut |e| {
                    if matches!(e, Expr::Outer(..) | Expr::Subquery(_) | Expr::Param(..)) {
                        plain = false;
                    }
                });
                if !plain {
                    continue;
                }
                let Ok(q) = c.expr.clone().transform(&mut |e| {
                    Ok(match e {
                        Expr::Col(g, t) if self.leaf_of.get(g) == Some(&l) => Expr::Col(g - leaf.off, t),
                        Expr::Col(g, t) => {
                            let x = Expr::Col(g, t.clone());
                            let k = match params.iter().position(|p| p.same_as(&x)) {
                                Some(k) => k,
                                None => {
                                    params.push(x);
                                    params.len() - 1
                                }
                            };
                            Expr::Outer(1, k, t)
                        }
                        e => e,
                    })
                }) else {
                    continue;
                };
                if let Some(e) = self.clause_ec(c) {
                    ecs.push(e);
                }
                prel.quals.push(q);
                enforced.push(ci);
            }
            // ppi_clauses' order: the movable join clauses, then the equivalence classes'
            // (in the order the classes formed): the parameters (a Memoize's cache keys)
            // in the order the clauses read them.
            {
                let nrq = r.quals.len();
                let mut order: Vec<usize> = (0..enforced.len()).collect();
                order.sort_by_key(|&i| {
                    let c = &self.clauses[enforced[i]];
                    (c.eq.is_some(), self.clause_ec(c).unwrap_or(usize::MAX), enforced[i])
                });
                let tail: Vec<Expr> = prel.quals.drain(nrq..).collect();
                let mut quals: Vec<Expr> = order.iter().map(|&i| tail[i].clone()).collect();
                enforced = order.iter().map(|&i| enforced[i]).collect();
                let mut renum: Vec<usize> = Vec::new();
                for q in &quals {
                    q.walk(&mut |x| {
                        if let Expr::Outer(1, k, _) = x
                            && !renum.contains(k)
                        {
                            renum.push(*k);
                        }
                    });
                }
                for k in 0..params.len() {
                    if !renum.contains(&k) {
                        renum.push(k);
                    }
                }
                for q in &mut quals {
                    *q = std::mem::replace(q, Expr::Const(Value::Null, DataType::Unknown))
                        .transform(&mut |x| {
                            Ok(match x {
                                Expr::Outer(1, k, t) => {
                                    Expr::Outer(1, renum.iter().position(|&j| j == k).unwrap(), t)
                                }
                                x => x,
                            })
                        })
                        .unwrap();
                }
                params = renum.iter().map(|&k| params[k].clone()).collect();
                prel.quals.extend(quals);
            }
            let nrq = r.quals.len();
            prel.loop_count = (0..self.leaves.len())
                .filter(|j| req & (1 << j) != 0)
                .map(|j| self.leaf_loop_rows(j))
                .fold(f64::INFINITY, f64::min)
                .max(1.0);
            if !prel.loop_count.is_finite() {
                prel.loop_count = 1.0;
            }
            prel.rows = costs::clamp_rows(prel.size.tuples * self.pl.sel(&prel, &prel.quals));
            (prel.qual_startup, prel.qual_cost) = self.pl.quals_cost(&prel.quals);
            let prel = Rc::new(prel);
            let mut best: Vec<JP> = Vec::new();
            for (ix, ii) in prel.indexes.iter().enumerate() {
                if !ii.pred.is_empty() && !implied_by(&ii.pred, &prel.quals) {
                    continue;
                }
                let cs = match_quals(ii, &prel.quals, None, prel.booleq.as_ref(), self.pl);
                if !cs.iter().any(|c| c.qual >= nrq) {
                    continue;
                }
                let only = self.pl.g.indexonlyscan && self.pl.index_only(&prel, ii);
                let ip = self.pl.index_path(&prel, ix, cs, Vec::new(), false, only);
                let p = Path {
                    startup: ip.startup,
                    total: ip.total,
                    rows: prel.rows,
                    keys: Vec::new(),
                    disabled: ip.disabled,
                    kind: PathKind::Index(ip),
                };
                let jp = JP {
                    rows: prel.rows,
                    startup: p.startup,
                    total: p.total,
                    width,
                    disabled: p.disabled,
                    keys: Vec::new(),
                    req,
                    set,
                    node: JNode::Scan {
                        leaf: l,
                        path: p,
                        params: params.clone(),
                        enforced: enforced.clone(),
                        rel: prel.clone(),
                    },
                };
                add_jpath(&mut best, jp, false);
            }
            // create_index_paths' bitjoinpaths: the best AND of the bitmap index paths
            // with this parameterization and the restriction ones, under a bitmap heap
            // scan (when it uses a parameterized clause).
            let mut bits: Vec<BPath> = Vec::new();
            for (ix, ii) in prel.indexes.iter().enumerate() {
                if !ii.pred.is_empty() && !implied_by(&ii.pred, &prel.quals) {
                    continue;
                }
                let cs = match_quals(ii, &prel.quals, None, prel.booleq.as_ref(), self.pl);
                if cs.iter().any(|c| c.qual >= nrq) {
                    let ip = self.pl.index_path(&prel, ix, cs, Vec::new(), false, false);
                    if ip.index_sel < 1.0 {
                        bits.push(BPath::Index(ip));
                    }
                }
                // (The index on the restriction clauses alone.)
                let cs = match_quals(ii, &r.quals, None, r.booleq.as_ref(), self.pl);
                if !cs.is_empty() {
                    let ip = self.pl.index_path(r, ix, cs, Vec::new(), false, false);
                    if ip.index_sel < 1.0 {
                        bits.push(BPath::Index(ip));
                    }
                }
            }
            bits.extend(self.pl.bitmap_or_paths(r, &r.quals, &[]));
            if bits.iter().any(|b| b.uses_from(nrq)) {
                let b = self.pl.choose_bitmap_and(&prel, bits);
                if b.uses_from(nrq) {
                    let (st, tot, _, _) = self.pl.bitmap_heap(&prel, &b);
                    let p = Path {
                        kind: PathKind::Bitmap(b),
                        startup: st,
                        total: tot,
                        rows: prel.rows,
                        keys: Vec::new(),
                        disabled: (!self.pl.g.bitmapscan) as u32,
                    };
                    add_jpath(
                        &mut best,
                        JP {
                            rows: prel.rows,
                            startup: p.startup,
                            total: p.total,
                            width,
                            disabled: p.disabled,
                            keys: Vec::new(),
                            req,
                            set,
                            node: JNode::Scan {
                                leaf: l,
                                path: p,
                                params: params.clone(),
                                enforced: enforced.clone(),
                                rel: prel.clone(),
                            },
                        },
                        false,
                    );
                }
            }
            // create_tidscan_paths' parameterized ones: `ctid = outer value`.
            if tid_cand
                && let Some((quals, removed)) = tid_quals(&prel)
                && quals.iter().any(|q| q.has_outer_refs())
            {
                let (st, tot) = self.pl.tid_cost(&prel, &quals);
                let rows = (quals.len() as f64).max(1.0);
                let p = Path {
                    kind: PathKind::Tid(quals, removed),
                    startup: st,
                    total: tot,
                    rows,
                    keys: Vec::new(),
                    disabled: (!self.pl.g.tidscan) as u32,
                };
                add_jpath(
                    &mut best,
                    JP {
                        rows,
                        startup: p.startup,
                        total: p.total,
                        width,
                        disabled: p.disabled,
                        keys: Vec::new(),
                        req,
                        set,
                        node: JNode::Scan {
                            leaf: l,
                            path: p,
                            params: params.clone(),
                            enforced: enforced.clone(),
                            rel: prel.clone(),
                        },
                    },
                    false,
                );
            }
            if let Some(b) = cheapest_param(&best) {
                out.push(b.clone());
            }
        }
        out.extend(self.or_param_paths(l, r, width, set));
        out
    }

    /// generate_bitmap_or_paths over the join OR clauses movable to leaf `l`: a bitmap
    /// heap scan parameterized by the other leaves the clause reads, its arms' index
    /// conditions taking their values from them.
    fn or_param_paths(&self, l: usize, r: &Rc<Rel>, width: u32, set: u32) -> Vec<JP> {
        let leaf = &self.leaves[l];
        let mut out: Vec<JP> = Vec::new();
        for (ci, c) in self.clauses.iter().enumerate() {
            if !matches!(c.expr, Expr::Or(_))
                || c.leaves & set == 0
                || c.leaves == set
                || c.expr.is_volatile()
                || !self.movable_to(ci, l)
            {
                continue;
            }
            let mut plain = true;
            c.expr.walk(&mut |e| {
                if matches!(e, Expr::Outer(..) | Expr::Subquery(_) | Expr::Param(..)) {
                    plain = false;
                }
            });
            if !plain {
                continue;
            }
            // The leaf's columns as its own, the others' as parameters.
            let mut params: Vec<Expr> = Vec::new();
            let Ok(q) = c.expr.clone().transform(&mut |e| {
                Ok(match e {
                    Expr::Col(g, t) if self.leaf_of.get(g) == Some(&l) => {
                        Expr::Col(g - leaf.off, t)
                    }
                    Expr::Col(g, t) => {
                        let x = Expr::Col(g, t.clone());
                        let k = match params.iter().position(|p| p.same_as(&x)) {
                            Some(k) => k,
                            None => {
                                params.push(x);
                                params.len() - 1
                            }
                        };
                        Expr::Outer(1, k, t)
                    }
                    e => e,
                })
            }) else {
                continue;
            };
            let req = c.leaves & !set;
            let nrq = r.quals.len();
            let Some(mut prel) = r.shallow_param() else {
                continue;
            };
            prel.quals.push(q.clone());
            // get_baserel_parampathinfo: the other join clauses movable into the leaf with
            // these outer leaves (one per equivalence class) are the path's filter too.
            let mut enforced = vec![ci];
            let mut ecs: Vec<usize> = Vec::new();
            for (cj, d) in self.clauses.iter().enumerate() {
                if cj == ci
                    || d.leaves & set == 0
                    || d.leaves == set
                    || d.leaves & !(set | req) != 0
                    || d.expr.is_volatile()
                    || !self.movable_to(cj, l)
                {
                    continue;
                }
                if let Some(e) = self.clause_ec(d) {
                    if ecs.contains(&e) {
                        continue;
                    }
                    ecs.push(e);
                }
                let mut plain = true;
                d.expr.walk(&mut |e| {
                    if matches!(e, Expr::Outer(..) | Expr::Subquery(_) | Expr::Param(..)) {
                        plain = false;
                    }
                });
                if !plain {
                    continue;
                }
                let Ok(dq) = d.expr.clone().transform(&mut |e| {
                    Ok(match e {
                        Expr::Col(g, t) if self.leaf_of.get(g) == Some(&l) => Expr::Col(g - leaf.off, t),
                        Expr::Col(g, t) => {
                            let x = Expr::Col(g, t.clone());
                            let k = match params.iter().position(|p| p.same_as(&x)) {
                                Some(k) => k,
                                None => {
                                    params.push(x);
                                    params.len() - 1
                                }
                            };
                            Expr::Outer(1, k, t)
                        }
                        e => e,
                    })
                }) else {
                    continue;
                };
                prel.quals.push(dq);
                enforced.push(cj);
            }
            prel.loop_count = self.loop_count(req);
            prel.rows = costs::clamp_rows(prel.size.tuples * self.pl.sel(&prel, &prel.quals));
            (prel.qual_startup, prel.qual_cost) = self.pl.quals_cost(&prel.quals);
            let prel = Rc::new(prel);
            let mk = |path: Path| JP {
                rows: prel.rows,
                startup: path.startup,
                total: path.total,
                width,
                disabled: path.disabled,
                keys: Vec::new(),
                req,
                set,
                node: JNode::Scan {
                    leaf: l,
                    path,
                    params: params.clone(),
                    enforced: enforced.clone(),
                    rel: prel.clone(),
                },
            };
            let mut best: Vec<JP> = Vec::new();
            // The OR as one index condition (match_orclause_to_indexcol), else arm by arm
            // under a BitmapOr.
            let mut bits = Vec::new();
            for (ix, ii) in prel.indexes.iter().enumerate() {
                if !ii.pred.is_empty() && !implied_by(&ii.pred, &prel.quals) {
                    continue;
                }
                let cs = match_quals(ii, &prel.quals, None, prel.booleq.as_ref(), self.pl);
                if !cs.iter().any(|c| c.qual >= nrq) {
                    continue;
                }
                let only = self.pl.g.indexonlyscan && self.pl.index_only(&prel, ii);
                let ip = self.pl.index_path(&prel, ix, cs.clone(), Vec::new(), false, only);
                add_jpath(
                    &mut best,
                    mk(Path {
                        startup: ip.startup,
                        total: ip.total,
                        rows: prel.rows,
                        keys: Vec::new(),
                        disabled: ip.disabled,
                        kind: PathKind::Index(ip),
                    }),
                    false,
                );
                let ip = self.pl.index_path(&prel, ix, cs, Vec::new(), false, false);
                if ip.index_sel < 1.0 {
                    bits.push(BPath::Index(ip));
                }
            }
            if bits.is_empty() {
                bits = self.pl.bitmap_or_paths(&prel, std::slice::from_ref(&q), &r.quals);
            }
            if !bits.iter().any(|b| b.uses_from(nrq)) {
                if let Some(b) = cheapest_param(&best) {
                    out.push(b.clone());
                }
                continue;
            }
            for (ix, ii) in r.indexes.iter().enumerate() {
                if !ii.pred.is_empty() && !implied_by(&ii.pred, &r.quals) {
                    continue;
                }
                let cs = match_quals(ii, &r.quals, None, r.booleq.as_ref(), self.pl);
                if !cs.is_empty() {
                    let ip = self.pl.index_path(r, ix, cs, Vec::new(), false, false);
                    if ip.index_sel < 1.0 {
                        bits.push(BPath::Index(ip));
                    }
                }
            }
            let b = self.pl.choose_bitmap_and(&prel, bits);
            if b.uses_from(nrq) {
                let (st, tot, _, _) = self.pl.bitmap_heap(&prel, &b);
                add_jpath(
                    &mut best,
                    mk(Path {
                        kind: PathKind::Bitmap(b),
                        startup: st,
                        total: tot,
                        rows: prel.rows,
                        keys: Vec::new(),
                        disabled: (!self.pl.g.bitmapscan) as u32,
                    }),
                    false,
                );
            }
            let Some(jp) = cheapest_param(&best).cloned() else {
                continue;
            };
            // (One path per parameterization: the cheapest.)
            match out.iter().position(|o| o.req == req) {
                Some(i) if jp.total < out[i].total => out[i] = jp,
                Some(_) => {}
                None => out.push(jp),
            }
        }
        out
    }
}

impl Search<'_, '_, '_> {
    /// The join clauses `col = <other leaves>` a leaf's column could be an index condition
    /// of: (leaf column, the outer expression, its leaves, the clause).
    /// The join clauses `mine = other` that could parameterize leaf `l`: `mine` over the
    /// leaf's own columns (leaf-local; a column, or an expression of them an index may
    /// have), `other` over the group row, the leaves it reads, the clause.
    fn param_cands(&self, l: usize, ncols: usize) -> Vec<(Expr, Expr, u32, usize)> {
        let leaf = &self.leaves[l];
        let set = 1u32 << l;
        let mut cands = Vec::new();
        for (ci, c) in self.clauses.iter().enumerate() {
            if !self.movable_to(ci, l) {
                continue;
            }
            // (An equality, or a btree comparison of the leaf's column with the other
            // leaves' values, as match_clause_to_indexcol takes it.)
            let (a, b, al, bl) = match (&c.eq, &c.expr) {
                (Some((a, b, al, bl)), _) => (a.clone(), b.clone(), *al, *bl),
                (None, Expr::Call(def, args, _))
                    if args.len() == 2 && matches!(def.name, "<" | "<=" | ">" | ">=") && !c.expr.is_volatile() =>
                {
                    let (la, lb) = (self.leaves_of(&args[0]), self.leaves_of(&args[1]));
                    if la == 0 || lb == 0 {
                        continue;
                    }
                    (args[0].clone(), args[1].clone(), la, lb)
                }
                _ => continue,
            };
            for (mine, other, ml, ol) in [(&a, &b, &al, &bl), (&b, &a, &bl, &al)] {
                if *ml != set || *ol & set != 0 {
                    continue;
                }
                let local = match key_col(mine) {
                    Some(g) if g >= leaf.off && g < leaf.off + ncols => {
                        Expr::Col(g - leaf.off, key_operand(mine).ty())
                    }
                    Some(_) => continue,
                    None => {
                        let cols = mine.columns();
                        if cols.is_empty()
                            || mine.is_volatile()
                            || cols.iter().any(|&g| g < leaf.off || g >= leaf.off + ncols)
                        {
                            continue;
                        }
                        mine.clone().shift_cols(-(leaf.off as isize))
                    }
                };
                cands.push((local, other.clone(), *ol, ci));
            }
        }
        cands
    }

    /// A leaf's rows as an outer relation of a parameterized path: in a partitionwise
    /// child join, its parent relation's.
    fn leaf_loop_rows(&self, j: usize) -> f64 {
        match self.loop_rows.get(j) {
            Some(&r) if self.loop_rows.len() == self.leaves.len() => r,
            _ => self.leaves[j].rows,
        }
    }

    /// The fewest rows of the leaves in `req` (get_loop_count).
    fn loop_count(&self, req: u32) -> f64 {
        let n = (0..self.leaves.len())
            .filter(|j| req & (1 << j) != 0)
            .map(|j| self.leaf_loop_rows(j))
            .fold(f64::INFINITY, f64::min)
            .max(1.0);
        if n.is_finite() { n } else { 1.0 }
    }

    /// Parameterized paths of an inheritance Append leaf (add_paths_to_append_rel): for
    /// each set of outer leaves some member has a parameterized index scan for, an Append
    /// of every member's cheapest path with that parameterization (an index scan, or its
    /// sequential scan filtering on the join clauses).
    fn append_param_paths(&self, l: usize, width: u32) -> Vec<JP> {
        let leaf = &self.leaves[l];
        let Some(members) = &leaf.members else {
            return Vec::new();
        };
        let set = 1u32 << l;
        let cands = self.param_cands(l, leaf.ncols);
        if cands.is_empty() {
            return Vec::new();
        }
        let mut reqs: Vec<u32> = Vec::new();
        for c in &cands {
            if !reqs.contains(&c.2) {
                reqs.push(c.2);
            }
        }
        let mut out = Vec::new();
        'req: for req in reqs {
            let used: Vec<&(Expr, Expr, u32, usize)> =
                cands.iter().filter(|c| c.2 & !req == 0).collect();
            let params: Vec<Expr> = used.iter().map(|c| c.1.clone()).collect();
            let enforced: Vec<usize> = used.iter().map(|c| c.3).collect();
            let mut scans = Vec::new();
            let mut any_index = false;
            for m in members {
                let Some(mut prel) = m.rel.shallow_param() else {
                    continue 'req;
                };
                let nrq = prel.quals.len();
                for (k, (mine, other, _, ci)) in used.iter().enumerate() {
                    // The leaf's value as the member has it (its columns: columns, or a
                    // UNION ALL member's expressions of its columns).
                    let mut missing = false;
                    let colx = match mine {
                        Expr::Col(pcol, _) => match m.exprs.get(*pcol) {
                            Some(Some(x)) => x.clone(),
                            _ => continue 'req,
                        },
                        e => e
                            .clone()
                            .transform(&mut |x| {
                                Ok(match x {
                                    Expr::Col(c, t) => match m.exprs.get(c) {
                                        Some(Some(x)) => x.clone(),
                                        _ => {
                                            missing = true;
                                            Expr::Col(c, t)
                                        }
                                    },
                                    x => x,
                                })
                            })
                            .unwrap(),
                    };
                    if missing {
                        continue 'req;
                    }
                    let pcol = mine.columns().first().copied().unwrap_or(0);
                    let Expr::Call(def, args, ty) = &self.clauses[*ci].expr else {
                        continue 'req;
                    };
                    let param = Expr::Outer(1, k, other.ty());
                    // (An inner join's clause from its equivalence class: the outer
                    // side's member first; an outer join's as written.)
                    let written = match mine {
                        Expr::Col(..) => key_col(&args[0]).is_some_and(|g| g == leaf.off + pcol),
                        _ => {
                            let c = args[0].columns();
                            !c.is_empty() && c.iter().all(|&g| g >= leaf.off && g < leaf.off + leaf.ncols)
                        }
                    };
                    let mine_first = if self.kind == JoinKind::Inner
                        && !self.clauses[*ci].on
                        && args[0].ty().without_typmod() == args[1].ty().without_typmod()
                        && self.clauses[*ci].eq.is_some()
                    {
                        false
                    } else {
                        written
                    };
                    prel.quals.push(if mine_first {
                        Expr::Call(def.clone(), vec![colx, param], ty.clone())
                    } else {
                        Expr::Call(def.clone(), vec![param, colx], ty.clone())
                    });
                }
                prel.loop_count = self.loop_count(req);
                prel.rows = costs::clamp_rows(prel.size.tuples * self.pl.sel(&prel, &prel.quals));
                (prel.qual_startup, prel.qual_cost) = self.pl.quals_cost(&prel.quals);
                let prel = Rc::new(prel);
                // The member's sequential scan, reparameterized: the join clauses filter.
                let seq_total = prel.qual_startup
                    + self.pl.g.seq_page_cost * prel.size.pages
                    + (CPU_TUPLE_COST + prel.qual_cost) * prel.size.tuples;
                let mut best = Path {
                    kind: PathKind::Seq,
                    startup: prel.qual_startup,
                    total: seq_total,
                    rows: prel.rows,
                    keys: Vec::new(),
                    disabled: (!self.pl.g.seqscan) as u32,
                };
                let mut indexed = false;
                for (ix, ii) in prel.indexes.iter().enumerate() {
                    if !ii.pred.is_empty() && !implied_by(&ii.pred, &prel.quals) {
                        continue;
                    }
                    let cs = match_quals(ii, &prel.quals, None, prel.booleq.as_ref(), self.pl);
                    if !cs.iter().any(|c| c.qual >= nrq) {
                        continue;
                    }
                    let only = self.pl.g.indexonlyscan && self.pl.index_only(&prel, ii);
                    let ip = self.pl.index_path(&prel, ix, cs, Vec::new(), false, only);
                    let better = if ip.disabled != best.disabled {
                        ip.disabled < best.disabled
                    } else {
                        ip.total < best.total || (ip.total == best.total && ip.startup < best.startup)
                    };
                    if better {
                        indexed = true;
                        best = Path {
                            startup: ip.startup,
                            total: ip.total,
                            rows: prel.rows,
                            keys: Vec::new(),
                            disabled: ip.disabled,
                            kind: PathKind::Index(ip),
                        };
                    }
                }
                any_index |= indexed;
                scans.push((best, prel));
            }
            if !any_index || scans.is_empty() {
                continue;
            }
            // cost_append (one member: the member's own costs).
            let startup = scans[0].0.startup;
            let rows: f64 = scans.iter().map(|(p, _)| p.rows).sum();
            let mut total: f64 = scans.iter().map(|(p, _)| p.total).sum();
            let disabled: u32 = scans.iter().map(|(p, _)| p.disabled).sum();
            if scans.len() > 1 {
                total += CPU_TUPLE_COST * 0.5 * rows;
            }
            out.push(JP {
                rows,
                startup,
                total,
                width,
                disabled,
                keys: Vec::new(),
                req,
                set,
                node: JNode::Append {
                    leaf: l,
                    scans,
                    params,
                    enforced,
                },
            });
        }
        out
    }
}

fn cheapest_param(list: &[JP]) -> Option<&JP> {
    let mut best: Option<&JP> = None;
    for p in list {
        if best.is_none_or(|b| cheaper(p, b, false)) {
            best = Some(p);
        }
    }
    best
}

/// query_is_distinct_for: a query level's plan returns no two rows equal in `cols` (its
/// output columns): grouped by columns among them, DISTINCT on them, or one row.
fn distinct_for(p: &Plan, cols: &[usize]) -> bool {
    match p {
        Plan::Sort { input, .. } | Plan::Limit { input, .. } | Plan::Filter { input, .. } => distinct_for(input, cols),
        Plan::Project { input, exprs } => {
            let mut inner = Vec::new();
            for &c in cols {
                if let Some(Expr::Col(j, _)) = exprs.get(c) {
                    inner.push(*j);
                }
            }
            match &**input {
                Plan::Agg { groups, sets: None, .. } => {
                    (0..groups.len()).all(|g| inner.contains(&g))
                }
                _ => distinct_for(input, &inner),
            }
        }
        Plan::Agg { groups, sets: None, .. } => (0..groups.len()).all(|g| cols.contains(&g)),
        Plan::Distinct { input, on: None, .. } => {
            (0..crate::sql::binder::plan_width(input)).all(|g| cols.contains(&g))
        }
        _ => false,
    }
}

impl Rel {
    /// A copy to add parameterized quals to.
    fn shallow_with_quals(&self) -> Rel {
        let mut r = self.shallow();
        r.quals = self.quals.clone();
        r
    }

    /// The same, for a nested loop's parameterized inner side: it runs with the outer row
    /// as a frame of its own (`Outer(1, k)`), so the values of enclosing query levels its
    /// quals read are a level further out. None when a sub-select in them reads those.
    fn shallow_param(&self) -> Option<Rel> {
        let mut r = self.shallow();
        r.quals = self.quals.iter().map(deepen_outer).collect::<Option<_>>()?;
        Some(r)
    }
}

/// `e` read one frame further in (see `Rel::shallow_param`); None when a sub-select in it
/// reads an enclosing query level's values itself.
pub(super) fn deepen_outer(e: &Expr) -> Option<Expr> {
    let mut ok = true;
    e.walk(&mut |x| {
        if let Expr::Subquery(s) = x {
            ok &= !crate::exec::reads_enclosing(&s.plan, usize::from(!s.params.is_empty()));
        }
    });
    if !ok {
        return None;
    }
    e.clone()
        .transform(&mut |x| {
            Ok(match x {
                Expr::Outer(d, k, t) => Expr::Outer(d + 1, k, t),
                x => x,
            })
        })
        .ok()
}

// ── join relations ──

/// What add_paths_to_joinrel works with for one (outer, inner) pair.
/// A semi join done as an inner join with its right side unique-ified, on the inner or
/// the outer side (JOIN_UNIQUE_INNER / JOIN_UNIQUE_OUTER).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Uniq {
    No,
    Inner,
    Outer,
}

struct Extra {
    jt: Jt,
    uniq: Uniq,
    restrict: Vec<usize>,
    inner_unique: bool,
    semi: Option<Semi>,
    rows: f64,
    width: u32,
    set: u32,
}

impl Search<'_, '_, '_> {
    /// add_paths_to_joinrel's param_source_rels: the relations a path of the join of `set`
    /// may be parameterized by, those outside the minimal right side of an outer join whose
    /// right side the join is in.
    fn param_source_rels(&self, set: u32) -> u32 {
        let all = if self.leaves.len() >= 32 { u32::MAX } else { (1u32 << self.leaves.len()) - 1 };
        let mut src = 0;
        for sj in &self.sjs {
            if set & sj.min_r != 0 && set & sj.min_l == 0 {
                src |= all & !sj.min_r;
            }
        }
        src
    }

    /// LATERAL items can only be the inside of a nested loop whose outer side has every
    /// item they read (join_is_legal's LATERAL checks; parameterized join relations aren't
    /// made, so the item itself is that inner side).
    fn lateral_ok(&self, a: u32, b: u32) -> bool {
        for (l, leaf) in self.leaves.iter().enumerate() {
            let Some((_, deps)) = &leaf.lateral else { continue };
            let bit = 1u32 << l;
            for (mine, other) in [(a, b), (b, a)] {
                if mine & bit != 0 && deps & !mine != 0 && (mine != bit || deps & !other != 0) {
                    return false;
                }
            }
        }
        true
    }

    /// Whether `a` and `b` are an item that reads LATERAL values and items it reads them
    /// from (have_join_order_restriction's direct lateral references).
    fn lateral_pair(&self, a: u32, b: u32) -> bool {
        self.leaves.iter().enumerate().any(|(l, leaf)| {
            leaf.lateral.as_ref().is_some_and(|(_, deps)| {
                let bit = 1u32 << l;
                (a & bit != 0 && deps & b != 0) || (b & bit != 0 && deps & a != 0)
            })
        })
    }

    /// make_join_rel.
    fn make_join_rel(&mut self, r1: usize, r2: usize) {
        let (mut r1, mut r2) = (r1, r2);
        let mut unique_only = false;
        let jt = if self.general {
            let Some((sj, rev, u)) = self.join_is_legal_u(self.rels[r1].set, self.rels[r2].set) else {
                return;
            };
            unique_only = u;
            if rev {
                std::mem::swap(&mut r1, &mut r2);
            }
            self.cur_sj = sj;
            match sj.map(|t| self.sjs[t].kind) {
                None => Jt::Inner,
                Some(JoinKind::Semi) => Jt::Semi,
                Some(JoinKind::Anti) => Jt::Anti,
                Some(_) => Jt::Left,
            }
        } else {
            self.jt()
        };
        let (s1, s2) = (self.rels[r1].set, self.rels[r2].set);
        if !self.lateral_ok(s1, s2) {
            return;
        }
        let set = s1 | s2;
        let restrict = self.crossing(s1, s2);
        let ri = match self.rel_index(set) {
            Some(i) => i,
            None => {
                // build_join_rel: the size from this first pair.
                let rows = self.join_size(jt, r1, r2, &restrict);
                self.rels.push(JRel {
                    set,
                    rows,
                    width: self.width_of(set),
                    paths: Vec::new(),
                    param: Vec::new(),
                });
                self.rels.len() - 1
            }
        };
        match jt {
            Jt::Inner => {
                self.add_paths(ri, r1, r2, Jt::Inner, &restrict);
                self.add_paths(ri, r2, r1, Jt::Inner, &restrict);
            }
            Jt::Left => {
                self.add_paths(ri, r1, r2, Jt::Left, &restrict);
                self.add_paths(ri, r2, r1, Jt::Right, &restrict);
            }
            // (A RIGHT JOIN is a LEFT one with its sides swapped to the planner: its paths
            // that way round first, as populate_joinrel_with_paths makes them.)
            Jt::Right => {
                self.add_paths(ri, r2, r1, Jt::Left, &restrict);
                self.add_paths(ri, r1, r2, Jt::Right, &restrict);
            }
            Jt::Full => {
                self.add_paths(ri, r1, r2, Jt::Full, &restrict);
                self.add_paths(ri, r2, r1, Jt::Full, &restrict);
            }
            Jt::Semi => {
                if !unique_only {
                    self.add_paths(ri, r1, r2, Jt::Semi, &restrict);
                    self.add_paths(ri, r2, r1, Jt::RightSemi, &restrict);
                }
                // The right side unique-ified, then an inner join either way round (on all
                // its semi join clauses when the left side isn't all of the semi join's).
                let uclauses = if unique_only { self.semi_rhs_clauses(self.rels[r2].set) } else { restrict.clone() };
                if let Some(u) = self.unique_rel(r2, &uclauses) {
                    self.rels.push(u);
                    let ui = self.rels.len() - 1;
                    self.add_paths_u(ri, r1, ui, Jt::Inner, Uniq::Inner, &restrict);
                    self.add_paths_u(ri, ui, r1, Jt::Inner, Uniq::Outer, &restrict);
                    self.rels.pop();
                }
            }
            Jt::Anti => {
                self.add_paths(ri, r1, r2, Jt::Anti, &restrict);
                self.add_paths(ri, r2, r1, Jt::RightAnti, &restrict);
            }
            Jt::RightSemi | Jt::RightAnti => {}
        }
    }

    /// The semi join's right side (one leaf) made unique on the columns its equality
    /// clauses compare (create_unique_path): None when the clauses aren't all such
    /// equalities, or every such column is equated to a constant.
    fn unique_rel(&self, r: usize, restrict: &[usize]) -> Option<JRel> {
        let rset = self.rels[r].set;
        if rset.count_ones() != 1 || restrict.is_empty() {
            return None;
        }
        let l = rset.trailing_zeros() as usize;
        let mut cols = Vec::new();
        let (mut can_btree, mut can_hash) = (true, true);
        for &c in restrict {
            let cl = &self.clauses[c];
            let (a, b, al, bl) = cl.eq.as_ref()?;
            let rhs = if *bl == rset {
                b
            } else if *al == rset {
                a
            } else {
                return None;
            };
            let g = key_col(rhs)?;
            can_btree &= cl.mergeable;
            can_hash &= cl.hashable;
            // (A column equated to a constant needs no unique-ifying.)
            let constant = match &self.leaves[l].scan {
                Some((_, rel)) => rel.quals.iter().any(|q| const_equality(q, g - self.leaves[l].off)),
                None => false,
            };
            if !constant && !cols.contains(&g) {
                cols.push(g);
            }
        }
        if cols.is_empty() || !(can_btree || can_hash) {
            return None;
        }
        let rel = &self.rels[r];
        let input = cheapest_of(&rel.paths, false)?.clone();
        let rows_in = rel.rows;
        let groups = match &self.leaves[l].scan {
            Some((_, lr)) => {
                let exprs: Vec<Expr> = cols
                    .iter()
                    .map(|&g| Expr::Col(g - self.leaves[l].off, DataType::Unknown))
                    .collect();
                let refs: Vec<&Expr> = exprs.iter().collect();
                self.pl.num_groups(lr, &refs, rows_in)
            }
            // (Columns with statistics of their own, as a CTE's of a table: their distinct
            // counts; else estimate_num_groups' default.)
            None => {
                let leaf = &self.leaves[l];
                let mut prod = 1.0;
                let mut known = true;
                for &g in cols.iter() {
                    match leaf.cols.get(g - leaf.off) {
                        Some((_, Some(v), _)) => prod *= v.ndistinct(&self.pl.cat).0,
                        Some((_, None, Some(d))) => prod *= *d,
                        _ => known = false,
                    }
                }
                if known && !cols.is_empty() {
                    costs::clamp_rows(prod.min(rows_in))
                } else {
                    costs::clamp_rows(rows_in.min(costs::DEFAULT_NUM_DISTINCT))
                }
            }
        };
        let n = cols.len() as f64;
        let width = input.width;
        // Sort + Unique.
        let sort = can_btree.then(|| {
            let (st, tot) = costs::sort_cost(input.total, rows_in, width, None);
            (
                input.disabled + (!self.pl.g.sort) as u32,
                st,
                CPU_OPERATOR_COST.mul_add(rows_in * n, tot),
            )
        });
        // HashAggregate, when the table fits in hash_mem.
        let hash_mem = self.g.work_mem * self.g.hash_mem_multiplier * 1024.0;
        let hash = (can_hash && (width as f64 + 64.0) * groups <= hash_mem).then(|| {
            let st = (CPU_OPERATOR_COST * n).mul_add(rows_in, input.total);
            (
                input.disabled + (!self.pl.g.hashagg) as u32,
                st,
                CPU_TUPLE_COST.mul_add(groups, st),
            )
        });
        let (hashed, (disabled, startup, total)) = match (sort, hash) {
            (Some(s), Some(h)) => {
                if h.0 < s.0 || (h.0 == s.0 && h.2 < s.2) {
                    (true, h)
                } else {
                    (false, s)
                }
            }
            (Some(s), None) => (false, s),
            (None, Some(h)) => (true, h),
            (None, None) => return None,
        };
        Some(JRel {
            set: rset,
            rows: rel.rows,
            width: rel.width,
            paths: vec![JP {
                rows: groups,
                startup,
                total,
                width,
                disabled,
                keys: Vec::new(),
                req: 0,
                set: rset,
                node: JNode::Unique {
                    input: Rc::new(input),
                    hashed,
                    cols,
                },
            }],
            param: Vec::new(),
        })
    }

    /// get_foreign_key_join_selectivity: foreign keys from a table on one side to one on
    /// the other whose every column pair a join clause equates. Each referencing row
    /// matches one referenced row: the selectivity is 1 / the referenced table's tuples
    /// (for a semi / anti join with the referenced table inside: its restrictions'
    /// selectivity), instead of the matching clauses'. Returns it and those clauses.
    fn fk_selectivity(&self, jt: Jt, oset: u32, iset: u32, restrict: &[usize]) -> (f64, Vec<usize>) {
        let cat = &self.pl.cat;
        let mut sel = 1.0;
        let mut removed: Vec<usize> = Vec::new();
        let semi = matches!(jt, Jt::Semi | Jt::Anti | Jt::RightSemi | Jt::RightAnti);
        let constant = |g: usize| {
            let l = self.leaf_of[g];
            let leaf = &self.leaves[l];
            match &leaf.scan {
                Some((_, rel)) => rel.quals.iter().any(|q| const_equality(q, g - leaf.off)),
                None => false,
            }
        };
        for (cl, cleaf) in self.leaves.iter().enumerate() {
            let Some((cs, _)) = &cleaf.scan else { continue };
            if !cat.children(cs.table.oid).is_empty() {
                continue;
            }
            for con in &cs.table.constraints {
                let crate::catalog::ConstraintKind::ForeignKey {
                    columns,
                    ref_table,
                    ref_columns,
                    ..
                } = &con.kind
                else {
                    continue;
                };
                if !con.enforced || !cat.children(*ref_table).is_empty() {
                    continue;
                }
                for (rl, rleaf) in self.leaves.iter().enumerate() {
                    let Some((rs, rrel)) = &rleaf.scan else { continue };
                    if rl == cl || rs.table.oid != *ref_table {
                        continue;
                    }
                    let (cb, rb) = (1u32 << cl, 1u32 << rl);
                    let ref_outer = if cb & oset != 0 && rb & iset != 0 {
                        false
                    } else if rb & oset != 0 && cb & iset != 0 {
                        true
                    } else {
                        continue;
                    };
                    if semi && (ref_outer || iset.count_ones() != 1) {
                        continue;
                    }
                    let mut mine: Vec<usize> = Vec::new();
                    let mut ok = true;
                    for (&cc, &rc) in columns.iter().zip(ref_columns) {
                        let (gc, gr) = (cleaf.off + cc, rleaf.off + rc);
                        if constant(gc) || constant(gr) {
                            ok = false;
                            break;
                        }
                        let matching: Vec<usize> = restrict
                            .iter()
                            .copied()
                            .filter(|c| !removed.contains(c) && !mine.contains(c))
                            .filter(|&c| {
                                let Some((a, b, _, _)) = &self.clauses[c].eq else {
                                    return false;
                                };
                                let (Expr::Col(x, _), Expr::Col(y, _)) = (a, b) else {
                                    return false;
                                };
                                let pair = (*x == gc && *y == gr) || (*x == gr && *y == gc);
                                // (A clause of the pair's equivalence class matches too.)
                                let same_ec = self.ec[gc] == self.ec[gr]
                                    && self.ec[*x] == self.ec[gc]
                                    && self.ec[*y] == self.ec[gc];
                                pair || same_ec
                            })
                            .collect();
                        if matching.is_empty() {
                            ok = false;
                            break;
                        }
                        mine.extend(matching);
                    }
                    if !ok {
                        continue;
                    }
                    removed.extend(mine);
                    let tuples = rrel.size.tuples.max(1.0);
                    sel *= if semi { rleaf.rows / tuples } else { 1.0 / tuples };
                }
            }
        }
        (sel.clamp(0.0, 1.0), removed)
    }

    /// calc_joinrel_size_estimate.
    fn join_size(&self, jt: Jt, outer: usize, inner: usize, restrict: &[usize]) -> f64 {
        let (o, i) = (&self.rels[outer], &self.rels[inner]);
        let semi = matches!(jt, Jt::Semi | Jt::Anti).then_some((i.set, i.rows));
        let (fksel, fk_clauses) = self.fk_selectivity(jt, o.set, i.set, restrict);
        let mut jsel = fksel;
        let mut psel = 1.0;
        for &c in restrict {
            if fk_clauses.contains(&c) {
                continue;
            }
            let s = self.clause_sel(&self.clauses[c], semi);
            if matches!(jt, Jt::Inner | Jt::Semi) || self.clauses[c].on {
                jsel *= s;
            } else {
                psel *= s;
            }
        }
        joins::join_rows(jt, o.rows, i.rows, jsel, psel)
    }

    /// innerrel_is_unique: the inner relation is one table with a unique index whose every
    /// column a clause equates to the outer side.
    fn inner_unique(&self, inner: u32, restrict: &[usize]) -> bool {
        if inner.count_ones() != 1 {
            return false;
        }
        let l = inner.trailing_zeros() as usize;
        let leaf = &self.leaves[l];
        // A subquery distinct on the columns the join equates (rel_is_distinct_for's
        // query_is_distinct_for: its GROUP BY keys, DISTINCT outputs, or a single row).
        if leaf.scan.is_none()
            && let Plan::Barrier { input, .. } = &leaf.plan
        {
            let mut cols = Vec::new();
            for &c in restrict {
                // (Equal under a nondeterministic collation isn't the grouping's equal.)
                if matches!(&self.clauses[c].expr, Expr::Call(d, ..)
                    if d.coll.as_ref().is_some_and(|c| !c.deterministic))
                {
                    continue;
                }
                if let Some((a, b, al, bl)) = &self.clauses[c].eq {
                    for (x, xl, yl) in [(a, al, bl), (b, bl, al)] {
                        if *xl == inner
                            && *yl & inner == 0
                            && let Some(g) = key_col(x)
                        {
                            cols.push(g - leaf.off);
                        }
                    }
                }
            }
            return distinct_for(input, &cols);
        }
        // (A pulled-up subquery's scan computing its PlaceHolderVars: through the
        // projection's columns.)
        let (t, proj): (&std::sync::Arc<crate::catalog::Table>, Option<&Vec<Expr>>) = match (&leaf.scan, &leaf.plan) {
            (Some((s, _)), _) => (&s.table, None),
            (None, Plan::Project { input, exprs }) => match &**input {
                Plan::Scan(s) => (&s.table, Some(exprs)),
                _ => return false,
            },
            // A partitioned table's members: its own unique indexes, which hold the
            // partition key (unique across the partitions), over its columns.
            (None, Plan::Append { inputs, inh: Some(_) }) => {
                let mut m: &Plan = match inputs.first() {
                    Some(m) => m,
                    None => return false,
                };
                while let Plan::Project { input, .. } | Plan::Filter { input, .. } = m {
                    m = input;
                }
                let Plan::Scan(ms) = m else { return false };
                match ms.table.inherits.first().and_then(|o| self.pl.cat.table(*o)) {
                    Some(p) if p.partkey.is_some() => (p, None),
                    _ => return false,
                }
            }
            _ => return false,
        };
        let mut cols = Vec::new();
        for &c in restrict {
            // (Equal under a nondeterministic collation the index's column doesn't have:
            // not the index's notion of equal, IndexCollMatchesExprColl.)
            let nondet = match &self.clauses[c].expr {
                Expr::Call(d, ..) => d.coll.as_ref().filter(|c| !c.deterministic).map(|c| c.name.clone()),
                _ => None,
            };
            if let Some((a, b, al, bl)) = &self.clauses[c].eq {
                for (x, xl, yl) in [(a, al, bl), (b, bl, al)] {
                    if *xl == inner
                        && *yl & inner == 0
                        && let Some(g) = key_col(x)
                    {
                        let col = match proj {
                            None => g - leaf.off,
                            Some(ex) => match ex.get(g - leaf.off) {
                                Some(Expr::Col(j, _)) => *j,
                                _ => continue,
                            },
                        };
                        if let Some(n) = &nondet
                            && t.columns.get(col).and_then(|c| c.collation.as_ref()) != Some(n)
                        {
                            continue;
                        }
                        cols.push(col);
                    }
                }
            }
        }
        let cat = &self.pl.cat;
        let partitioned = t.partkey.is_some();
        cat.table_indexes(t)
            .iter()
            .chain(t.pkey_index.and_then(|o| cat.index(o)))
            .any(|ix| {
                (ix.unique || ix.primary)
                    && (ix.valid || partitioned)
                    && ix.predicate.is_none()
                    && ix.columns.iter().all(|c| {
                        matches!(c.key, crate::catalog::IndexKey::Column(k) if cols.contains(&k))
                    })
            })
    }

    /// compute_semi_anti_join_factors. The match fraction is a semi join's selectivity
    /// when the join belongs to the group's semi / anti join (the right leaf its inner
    /// side, whichever way round this path has it).
    fn semi_factors(&self, jt: Jt, inner: usize, restrict: &[usize]) -> Semi {
        let i = &self.rels[inner];
        let semi = if self.general {
            self.cur_sj
                .filter(|&t| matches!(self.sjs[t].kind, JoinKind::Semi | JoinKind::Anti))
                .map(|t| {
                    let rs = self.sjs[t].syn_r;
                    match self.rels.iter().find(|r| r.set == rs) {
                        Some(r) => (r.set, r.rows),
                        None => (i.set, i.rows),
                    }
                })
        } else {
            matches!(self.kind, JoinKind::Semi | JoinKind::Anti).then(|| {
                let r = self.rels.iter().find(|r| r.set == 2).expect("the right leaf");
                (r.set, r.rows)
            })
        };
        let quals: Vec<usize> = restrict
            .iter()
            .copied()
            .filter(|&c| {
                !matches!(jt, Jt::Anti | Jt::RightAnti | Jt::Left | Jt::Right | Jt::Full)
                    || self.clauses[c].on
            })
            .collect();
        let mut jsel = 1.0;
        let mut nsel = 1.0;
        for &c in &quals {
            jsel *= self.clause_sel(&self.clauses[c], semi);
            nsel *= self.clause_sel(&self.clauses[c], None);
        }
        let avg = if jsel > 0.0 {
            (nsel * i.rows / jsel).max(1.0)
        } else {
            1.0
        };
        Semi {
            outer_match_frac: jsel,
            match_count: avg,
        }
    }

    /// add_paths_to_joinrel.
    fn add_paths(&mut self, ri: usize, outer: usize, inner: usize, jt: Jt, restrict: &[usize]) {
        self.add_paths_u(ri, outer, inner, jt, Uniq::No, restrict)
    }

    fn add_paths_u(
        &mut self,
        ri: usize,
        outer: usize,
        inner: usize,
        jt: Jt,
        uniq: Uniq,
        restrict: &[usize],
    ) {
        let (oset, iset) = (self.rels[outer].set, self.rels[inner].set);
        let inner_unique = match (jt, uniq) {
            (Jt::Semi | Jt::Anti, _) => false,
            (_, Uniq::Inner) => true,
            _ => self.inner_unique(iset, restrict),
        };
        let semi = (matches!(jt, Jt::Semi | Jt::Anti) || inner_unique)
            .then(|| self.semi_factors(jt, inner, restrict));
        let ex = Extra {
            jt,
            uniq,
            restrict: restrict.to_vec(),
            inner_unique,
            semi,
            rows: self.rels[ri].rows,
            width: self.rels[ri].width,
            set: oset | iset,
        };
        // The clauses usable to merge or hash: equalities with one side in each input.
        let sides: Vec<(usize, bool)> = restrict
            .iter()
            .filter_map(|&c| {
                let cl = &self.clauses[c];
                if jt.is_outer() && !cl.on {
                    return None;
                }
                let (_, _, al, bl) = cl.eq.as_ref()?;
                if *al & !oset == 0 && *bl & !iset == 0 {
                    Some((c, true))
                } else if *al & !iset == 0 && *bl & !oset == 0 {
                    Some((c, false))
                } else {
                    None
                }
            })
            .collect();
        // (A semi or anti join merges on columns only: its unique-ified side, which
        // Postgres would prefer, groups by columns here.)
        let semi_like = matches!(jt, Jt::Semi | Jt::Anti | Jt::RightSemi | Jt::RightAnti);
        let merge: Vec<(usize, bool)> = sides
            .iter()
            .copied()
            .filter(|(c, _)| {
                let cl = &self.clauses[*c];
                cl.mergeable
                    && (!semi_like
                        || cl.eq.as_ref().is_some_and(|(a, b, _, _)| key_col(a).is_some() && key_col(b).is_some()))
            })
            .collect();
        let hash: Vec<(usize, bool)> = sides
            .iter()
            .copied()
            .filter(|(c, _)| self.clauses[*c].hashable)
            .collect();
        // (Right semi joins hash only; right and full joins merge on every clause.)
        // (select_mergejoin_clauses: constant clauses the executor checks itself, as for
        // FULL JOIN ON false.)
        let merge_ok = jt != Jt::RightSemi
            && !(matches!(jt, Jt::Full | Jt::Right | Jt::RightAnti)
                && merge.len()
                    != restrict
                        .iter()
                        .filter(|&&c| self.clauses[c].on && !matches!(self.clauses[c].expr, Expr::Const(..)))
                        .count());
        if merge_ok && (self.g.mergejoin || jt == Jt::Full) && !merge.is_empty() {
            self.sort_inner_and_outer(ri, outer, inner, &merge, &ex);
        }
        if merge_ok {
            self.match_unsorted_outer(ri, outer, inner, &merge, &ex);
        }
        if (self.g.hashjoin || jt == Jt::Full) && !hash.is_empty() {
            self.hash_inner_and_outer(ri, outer, inner, &hash, &ex);
        }
    }
}

impl Jt {
    fn is_outer(self) -> bool {
        matches!(self, Jt::Left | Jt::Right | Jt::Full | Jt::Anti | Jt::RightAnti)
    }
}

/// `col = constant` on column `col` of a scan's row.
fn const_equality(q: &Expr, col: usize) -> bool {
    let Expr::Call(d, args, _) = q else {
        return false;
    };
    if d.name != "=" || args.len() != 2 {
        return false;
    }
    let is_col = |e: &Expr| key_col(e) == Some(col);
    (is_col(&args[0]) && crate::sql::binder::optimize::pseudo_constant(&args[1]))
        || (is_col(&args[1]) && crate::sql::binder::optimize::pseudo_constant(&args[0]))
}

// ── the three join methods ──

impl Search<'_, '_, '_> {
    /// build_join_pathkeys: the outer path's order, as far as it is still useful (to the
    /// rest of the search or the query).
    fn join_pathkeys(&self, jt: Jt, set: u32, outer_set: u32, outer_keys: &[GKey]) -> Vec<GKey> {
        if matches!(jt, Jt::Full | Jt::Right) {
            return Vec::new();
        }
        // (An outer side sorted by an expression has the class of the column a clause
        // equates it with; past an outer join's null-extended rows that is no order of
        // the column's: only classes with members on the outer side go on.)
        let outer_keys = if jt != Jt::Left {
            outer_keys
        } else {
            let n = outer_keys
                .iter()
                .take_while(|k| (0..self.ec.len()).any(|g| self.ec[g] == k.ec && (1u32 << self.leaf_of[g]) & outer_set != 0))
                .count();
            &outer_keys[..n]
        };
        // truncate_useless_pathkeys: as far as the ORDER BY or a later merge can use them.
        let q = self.query_keys();
        let ordering = outer_keys
            .iter()
            .zip(&q)
            .take_while(|(a, b)| a == b)
            .count();
        let mut merging = 0;
        for k in outer_keys {
            // (pathkeys_useful_for_merging: a class with members still to join, or a
            // mergeable clause between the rel and the rest on that class, as an outer
            // join's clause is.)
            let useful = (0..self.ec.len())
                .any(|g| self.ec[g] == k.ec && (1u32 << self.leaf_of[g]) & set == 0)
                || self.clauses.iter().any(|c| {
                    c.mergeable
                        && c.leaves & set != 0
                        && c.leaves & !set != 0
                        && c.eq.as_ref().is_some_and(|(a, b, _, _)| {
                            self.side_ec(a) == Some(k.ec) || self.side_ec(b) == Some(k.ec)
                        })
                });
            let right_desc = q.iter().find(|x| x.ec == k.ec).is_some_and(|x| x.desc);
            if !useful || k.desc != right_desc {
                break;
            }
            merging += 1;
        }
        outer_keys[..ordering.max(merging)].to_vec()
    }

    fn sort_inner_and_outer(
        &mut self,
        ri: usize,
        outer: usize,
        inner: usize,
        merge: &[(usize, bool)],
        ex: &Extra,
    ) {
        let (Some(op), Some(ip)) = (
            cheapest_of(&self.rels[outer].paths, false).cloned(),
            cheapest_of(&self.rels[inner].paths, false).cloned(),
        ) else {
            return;
        };
        let all = self.outer_keys_for_merge(merge, ex.set);
        for f in 0..all.len() {
            let mut okeys = all.clone();
            if f > 0 {
                let k = okeys.remove(f);
                okeys.insert(0, k);
            }
            let clauses = self.clauses_for_outer_keys(&okeys, merge);
            let ikeys = self.inner_keys_for(&clauses, &okeys);
            let pk = self.join_pathkeys(ex.jt, ex.set, op.set, &okeys);
            self.try_merge(ri, &op, &ip, pk, &clauses, Some(okeys), Some(ikeys), ex);
        }
    }

    /// find_mergeclauses_for_outer_pathkeys: the clauses in the order of the outer keys.
    fn clauses_for_outer_keys(&self, okeys: &[GKey], merge: &[(usize, bool)]) -> Vec<(usize, bool)> {
        let mut out = Vec::new();
        for k in okeys {
            let mut found = false;
            for &(c, oa) in merge {
                let (a, b, _, _) = self.clauses[c].eq.as_ref().unwrap();
                let oe = if oa { a } else { b };
                if self.side_ec(oe) == Some(k.ec) && !out.contains(&(c, oa)) {
                    out.push((c, oa));
                    found = true;
                }
            }
            if !found {
                break;
            }
        }
        out
    }

    /// make_inner_pathkeys_for_merge: per clause its inner side's class, in the direction
    /// of the outer key it matches (a class already listed is redundant).
    fn inner_keys_for(&self, clauses: &[(usize, bool)], okeys: &[GKey]) -> Vec<GKey> {
        let mut out: Vec<GKey> = Vec::new();
        for &(c, oa) in clauses {
            let (a, b, _, _) = self.clauses[c].eq.as_ref().unwrap();
            let (oe, ie) = if oa { (a, b) } else { (b, a) };
            let (Some(oec), Some(iec)) = (self.side_ec(oe), self.side_ec(ie)) else {
                continue;
            };
            let ok = okeys.iter().find(|k| k.ec == oec);
            let k = GKey {
                ec: iec,
                desc: ok.is_some_and(|k| k.desc),
                nulls_first: ok.is_some_and(|k| k.nulls_first),
            };
            if !out.iter().any(|x| x.ec == k.ec) {
                out.push(k);
            }
        }
        out
    }

    /// select_outer_pathkeys_for_merge: the merge clauses' outer classes, the query's
    /// order first when it is all of them (or they are a prefix of it), then the rest by
    /// how many later joins they may still serve, ascending.
    fn outer_keys_for_merge(&self, merge: &[(usize, bool)], set: u32) -> Vec<GKey> {
        if merge.is_empty() {
            return Vec::new();
        }
        let mut ecs: Vec<(usize, i64)> = Vec::new();
        for &(c, oa) in merge {
            let (a, b, _, _) = self.clauses[c].eq.as_ref().unwrap();
            let oe = if oa { a } else { b };
            let Some(ec) = self.side_ec(oe) else { continue };
            if ecs.iter().any(|(e, _)| *e == ec) {
                continue;
            }
            let score = (0..self.ec.len())
                .filter(|&h| self.ec[h] == ec && (1u32 << self.leaf_of[h]) & set == 0)
                .count() as i64;
            ecs.push((ec, score));
        }
        if ecs.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<GKey> = Vec::new();
        let q = self.query_keys();
        if !q.is_empty() {
            let matches = q.iter().take_while(|k| ecs.iter().any(|(e, _)| *e == k.ec)).count();
            if matches == q.len() {
                for k in &q {
                    if let Some(x) = ecs.iter_mut().find(|(e, _)| *e == k.ec) {
                        x.1 = -1;
                    }
                }
                out = q;
            } else if matches == merge.len() {
                return q[..matches].to_vec();
            }
        }
        loop {
            let mut best = 0;
            for j in 1..ecs.len() {
                if ecs[j].1 > ecs[best].1 {
                    best = j;
                }
            }
            if ecs[best].1 < 0 {
                break;
            }
            ecs[best].1 = -1;
            out.push(GKey {
                ec: ecs[best].0,
                desc: false,
                nulls_first: false,
            });
        }
        out
    }

    fn match_unsorted_outer(
        &mut self,
        ri: usize,
        outer: usize,
        inner: usize,
        merge: &[(usize, bool)],
        ex: &Extra,
    ) {
        let (nest_ok, useall) = match ex.jt {
            Jt::Inner | Jt::Left | Jt::Semi | Jt::Anti => (true, false),
            Jt::Right | Jt::Full | Jt::RightAnti => (false, true),
            Jt::RightSemi => return,
        };
        let inner_total = cheapest_of(&self.rels[inner].paths, false).cloned();
        let matpath = match &inner_total {
            Some(p)
                if nest_ok
                    && ex.uniq != Uniq::Inner
                    && self.g.material
                    && !p.input().kind.materializes() =>
            {
                let m = joins::material(&p.input(), &self.g);
                Some(JP {
                    node: JNode::Material(Rc::new(p.clone())),
                    rows: m.rows,
                    startup: m.startup,
                    total: m.total,
                    width: p.width,
                    disabled: m.disabled,
                    keys: p.keys.clone(),
                    req: 0,
                    set: p.set,
                })
            }
            _ => None,
        };
        let oset = self.rels[outer].set;
        let outer_paths: Vec<JP> = self.rels[outer].paths.clone();
        let inner_params: Vec<JP> = inner_total
            .iter()
            .cloned()
            .chain(
                self.rels[inner]
                    .param
                    .iter()
                    // (Or parameterized by the outer side and others: a parameterized
                    // join path's inner side.)
                    .filter(|p| {
                        (p.req & !oset == 0 || (ex.jt == Jt::Inner && p.req & oset != 0))
                            && ex.uniq != Uniq::Inner
                    })
                    .cloned(),
            )
            .collect();
        // (In a group with outer joins, parameterized outer paths too: the join path is
        // parameterized by what they read, when an outer join above allows it.)
        if self.general && matches!(ex.jt, Jt::Inner | Jt::Left) {
            let iset = self.rels[inner].set;
            let pouter: Vec<JP> = self.rels[outer]
                .param
                .iter()
                .filter(|p| p.req & iset == 0)
                .cloned()
                .collect();
            for op in &pouter {
                for ip in &inner_params {
                    self.try_nestloop(ri, op, ip, Vec::new(), ex);
                }
                if let Some(m) = &matpath {
                    self.try_nestloop(ri, op, m, Vec::new(), ex);
                }
            }
        }
        for op in outer_paths.iter().filter(|p| p.req == 0) {
            let pk = self.join_pathkeys(ex.jt, ex.set, op.set, &op.keys);
            if nest_ok {
                for ip in &inner_params {
                    self.try_nestloop(ri, op, ip, pk.clone(), ex);
                    // A Memoize over a parameterized inner path may make it cheaper.
                    if let Some(m) = self.memoize_path(outer, op, ip, ex) {
                        self.try_nestloop(ri, op, &m, pk.clone(), ex);
                    }
                }
                if let Some(m) = &matpath {
                    self.try_nestloop(ri, op, m, pk.clone(), ex);
                }
            }
            // (A unique-ified outer side: nested loops only.)
            if ex.uniq == Uniq::Outer {
                continue;
            }
            let Some(it) = &inner_total else { continue };
            // generate_mergejoin_paths: a presorted outer side.
            let clauses = self.clauses_for_outer_keys(&op.keys, merge);
            // (A FULL JOIN on constants only: a merge join with no merge clauses.)
            let keyless = ex.jt == Jt::Full && merge.is_empty();
            if (clauses.is_empty() && !keyless) || (useall && clauses.len() != merge.len()) {
                continue;
            }
            let ikeys = self.inner_keys_for(&clauses, &op.keys);
            self.try_merge(ri, op, it, pk.clone(), &clauses, None, Some(ikeys.clone()), ex);
            // (A unique-ified inner side is that one path, sorted.)
            if ex.uniq == Uniq::Inner {
                continue;
            }
            // Inner paths already in (a prefix of) that order.
            let mut cheapest_total: Option<JP> = it
                .keys
                .starts_with(&ikeys)
                .then(|| it.clone());
            let mut cheapest_startup = cheapest_total.clone();
            let inner_paths: Vec<JP> = self.rels[inner].paths.clone();
            for n in (1..=ikeys.len()).rev() {
                let trial = &ikeys[..n];
                let mut newclauses = None;
                let cand = inner_paths
                    .iter()
                    .filter(|p| p.req == 0 && p.keys.starts_with(trial))
                    .fold(None::<&JP>, |b, p| match b {
                        Some(b) if !cheaper(p, b, false) => Some(b),
                        _ => Some(p),
                    });
                if let Some(c) = cand
                    && cheapest_total.as_ref().is_none_or(|b| cheaper(c, b, false))
                {
                    let cl: Vec<(usize, bool)> = clauses[..n.min(clauses.len())].to_vec();
                    newclauses = Some(cl.clone());
                    self.try_merge(ri, op, c, pk.clone(), &cl, None, None, ex);
                    cheapest_total = Some(c.clone());
                }
                let cand = inner_paths
                    .iter()
                    .filter(|p| p.req == 0 && p.keys.starts_with(trial))
                    .fold(None::<&JP>, |b, p| match b {
                        Some(b) if !cheaper(p, b, true) => Some(b),
                        _ => Some(p),
                    });
                if let Some(c) = cand
                    && cheapest_startup.as_ref().is_none_or(|b| cheaper(c, b, true))
                {
                    let same = cheapest_total
                        .as_ref()
                        .is_some_and(|t| t.total == c.total && t.startup == c.startup);
                    if !same {
                        let cl = newclauses
                            .clone()
                            .unwrap_or_else(|| clauses[..n.min(clauses.len())].to_vec());
                        self.try_merge(ri, op, c, pk.clone(), &cl, None, None, ex);
                    }
                    cheapest_startup = Some(c.clone());
                }
                if useall {
                    break;
                }
            }
        }
    }

    fn hash_inner_and_outer(
        &mut self,
        ri: usize,
        outer: usize,
        inner: usize,
        hash: &[(usize, bool)],
        ex: &Extra,
    ) {
        let (Some(ot), Some(os), Some(it)) = (
            cheapest_of(&self.rels[outer].paths, false).cloned(),
            cheapest_of(&self.rels[outer].paths, true).cloned(),
            cheapest_of(&self.rels[inner].paths, false).cloned(),
        ) else {
            return;
        };
        self.try_hash(ri, &os, &it, hash, ex);
        let same = os.total == ot.total && os.startup == ot.startup;
        if !same {
            self.try_hash(ri, &ot, &it, hash, ex);
        }
    }

    /// get_memoize_path: a Memoize over parameterized inner path `ip` (cached on the
    /// outer values its join clauses read), when its parameters can be hashed.
    fn memoize_path(&self, outer: usize, op: &JP, ip: &JP, ex: &Extra) -> Option<JP> {
        if !self.g.memoize || self.rels[outer].rows < 2.0 || ip.req == 0 {
            return None;
        }
        // A LATERAL item: cached by the values it reads, compared as bytes.
        if let JNode::Opaque { params, leaf, .. } = &ip.node
            && !params.is_empty()
        {
            if params.iter().any(|e| e.is_volatile() || !memo_hashable(&e.ty())) {
                return None;
            }
            if lateral_join_leaf(&self.leaves[*leaf].plan) {
                return None;
            }
            let mut keys: Vec<Expr> = Vec::new();
            for e in params {
                if !keys.iter().any(|k| k.same_as(e)) {
                    keys.push(e.clone());
                }
            }
            let sub = ip.input();
            let calls = costs::clamp_rows(op.rows);
            let (ndistinct, key_width) = self.memo_groups(&keys, calls);
            let rescan = joins::memoize_rescan(&sub, calls, ndistinct, key_width, &self.g);
            return Some(JP {
                node: JNode::Memoize {
                    input: Rc::new(ip.clone()),
                    keys,
                    binary: true,
                    singlerow: false,
                    rescan,
                },
                rows: ip.rows,
                startup: ip.startup + CPU_TUPLE_COST,
                total: ip.total + CPU_TUPLE_COST,
                width: ip.width,
                disabled: ip.disabled,
                keys: ip.keys.clone(),
                req: ip.req,
                set: ip.set,
            });
        }
        // (A parameterized scan, or an append relation's members scanned so.)
        let (JNode::Scan { enforced, .. } | JNode::Append { enforced, .. }) = &ip.node else {
            return None;
        };
        if enforced.is_empty() {
            return None;
        }
        if !ex.inner_unique && matches!(ex.jt, Jt::Semi | Jt::Anti) {
            return None;
        }
        // A unique inner side caches single rows: only if every join clause is a
        // parameter of the inner path.
        if ex.inner_unique && ex.restrict.iter().any(|c| !enforced.contains(c)) {
            return None;
        }
        let leaves_of = |e: &Expr| -> u32 {
            e.columns()
                .into_iter()
                .fold(0u32, |m, g| m | (1u32 << self.leaf_of[g]))
        };
        let mut keys: Vec<Expr> = Vec::new();
        let mut binary = false;
        for &c in enforced {
            let cl = &self.clauses[c];
            if cl.expr.is_volatile() {
                return None;
            }
            // paraminfo_get_equal_hashops: the clause's outer side, hashable.
            let outer_side = match &cl.eq {
                Some((a, b, al, _)) => {
                    if *al & op.set != 0 && *al & !op.set == 0 {
                        a.clone()
                    } else {
                        b.clone()
                    }
                }
                None => match &cl.expr {
                    Expr::Call(_, args, _) if args.len() == 2 => {
                        let (la, lb) = (leaves_of(&args[0]), leaves_of(&args[1]));
                        if la != 0 && la & !op.set == 0 && lb & op.set == 0 {
                            args[0].clone()
                        } else if lb != 0 && lb & !op.set == 0 && la & op.set == 0 {
                            args[1].clone()
                        } else {
                            return None;
                        }
                    }
                    _ => return None,
                },
            };
            if !memo_hashable(&outer_side.ty()) {
                return None;
            }
            let hj = pg_hashjoinable(&outer_side.ty());
            if !keys.iter().any(|k| k.same_as(&outer_side)) {
                keys.push(outer_side);
            }
            // A join operator that doesn't hash may tell apart what hashing can't.
            if !(cl.eq.is_some() && hj) {
                binary = true;
            }
        }
        let sub = ip.input();
        let calls = costs::clamp_rows(op.rows);
        let (ndistinct, key_width) = self.memo_groups(&keys, calls);
        let rescan = joins::memoize_rescan(&sub, calls, ndistinct, key_width, &self.g);
        Some(JP {
            node: JNode::Memoize {
                input: Rc::new(ip.clone()),
                keys,
                binary,
                singlerow: ex.inner_unique,
                rescan,
            },
            rows: ip.rows,
            startup: ip.startup + CPU_TUPLE_COST,
            total: ip.total + CPU_TUPLE_COST,
            width: ip.width,
            disabled: ip.disabled,
            keys: ip.keys.clone(),
            req: ip.req,
            set: ip.set,
        })
    }

    /// estimate_num_groups over a Memoize's keys for `rows` calls (None: it used a
    /// default), and the keys' widths.
    fn memo_groups(&self, keys: &[Expr], rows: f64) -> (Option<f64>, f64) {
        let mut width = 0.0;
        // The group variables: a column, or the columns an expression reads.
        let mut vars: Vec<usize> = Vec::new();
        for k in keys {
            match key_col(k) {
                Some(g) => {
                    let l = self.leaf_of[g];
                    width += self.leaves[l].cols[g - self.leaves[l].off].0 as f64;
                    if !vars.contains(&g) {
                        vars.push(g);
                    }
                }
                None => {
                    width += costs::type_width(&k.ty()) as f64;
                    for g in k.columns() {
                        if !vars.contains(&g) {
                            vars.push(g);
                        }
                    }
                }
            }
        }
        let vs: Vec<_> = vars.into_iter().map(|g| self.var_of(g)).collect();
        (joins::memo_ndistinct(&self.pl.cat, &vs, rows), width)
    }

    fn try_nestloop(&mut self, ri: usize, op: &JP, ip: &JP, keys: Vec<GKey>, ex: &Extra) {
        // An inner (or outer) path parameterized by more than the join: a join path itself
        // parameterized by the rest. Either an inner join's nested loop with the outer side
        // plain (allow_star_schema_join), or, in a group with outer joins, one parameterized
        // by relations an outer join above has on its left (param_source_rels).
        let req_all = (ip.req | op.req) & !ex.set;
        let param_join = req_all != 0;
        let star = ex.jt == Jt::Inner
            && op.req == 0
            && !self.general
            && matches!(ip.node, JNode::Scan { .. } | JNode::Join { .. });
        let sourced = self.general
            && matches!(ex.jt, Jt::Inner | Jt::Left)
            && op.req & ip.set == 0
            && req_all & self.param_source_rels(ex.set) != 0;
        if param_join && !(star || sourced) {
            return;
        }
        if !param_join && op.req != 0 {
            return;
        }
        let mut enforced: Vec<usize> = enforced_of(ip.memoized());
        enforced.extend(enforced_of(op));
        let mut quals: Vec<usize> = ex
            .restrict
            .iter()
            .copied()
            .filter(|c| !enforced.contains(c))
            .collect();
        // get_joinrel_parampathinfo's equivalence classes: a class's clause between the
        // parameters and the join that the inner path enforces is replaced by one between
        // the parameters and the outer side's member (else nothing would equate those).
        if param_join {
            let members = self.ec_members();
            let in_set = |g: usize, s: u32| (1u32 << self.leaf_of[g]) & s != 0;
            let mut ecs: Vec<usize> = Vec::new();
            for &g in &members {
                if !ecs.contains(&self.ec[g]) {
                    ecs.push(self.ec[g]);
                }
            }
            for e in ecs {
                let first = |s: u32| members.iter().copied().find(|&g| self.ec[g] == e && in_set(g, s));
                let (Some(x), Some(y)) = (first(req_all), first(ex.set)) else { continue };
                if !in_set(y, ip.set) {
                    continue;
                }
                let Some(z) = first(op.set) else { continue };
                let found = (0..self.clauses.len()).find(|&j| {
                    self.clauses[j].eq.as_ref().is_some_and(|(p, q, _, _)| {
                        let (p, q) = (key_col(p), key_col(q));
                        (p == Some(x) && q == Some(z)) || (p == Some(z) && q == Some(x))
                    })
                });
                if let Some(j) = found
                    && !quals.contains(&j)
                    && !enforced.contains(&j)
                {
                    quals.push(j);
                }
            }
        }
        // (A join path parameterized by an outer join's left side checks the join clauses
        // movable into it from there: get_joinrel_parampathinfo.)
        if param_join && !star {
            for (ci, c) in self.clauses.iter().enumerate() {
                if quals.contains(&ci)
                    || enforced.contains(&ci)
                    || c.leaves & !(ex.set | req_all) != 0
                    || c.leaves & req_all == 0
                    || c.leaves & ex.set == 0
                    || c.expr.is_volatile()
                    || (0..self.leaves.len())
                        .filter(|l| c.leaves & ex.set & (1u32 << l) != 0)
                        .any(|l| !self.movable_to(ci, l))
                {
                    continue;
                }
                quals.push(ci);
            }
        }
        let full = (1u32 << self.leaves.len()) - 1;
        let post = if ex.set == full { self.post } else { 0.0 };
        let qc: f64 = quals.iter().map(|&c| self.clauses[c].cost).sum::<f64>() + post;
        let indexed = ip.req != 0
            && quals.is_empty()
            && !enforced.is_empty()
            && matches!(&ip.node, JNode::Scan { path, .. } if matches!(path.kind, PathKind::Index(_)));
        let mut inner = ip.input();
        if indexed {
            inner.kind = InKind::IndexedJoin;
        }
        // (A parameterized join's rows: the outer side's times the inner's, as the
        // parameters restrict it, times the join's other clauses.)
        let rows = if param_join {
            let sel: f64 = quals.iter().map(|&q| self.clause_sel(&self.clauses[q], None)).product();
            costs::clamp_rows(op.rows * ip.rows * sel)
        } else {
            ex.rows
        };
        let c = joins::nestloop(
            ex.jt,
            &op.input(),
            &inner,
            ex.inner_unique,
            ex.semi,
            rows,
            QualCost {
                startup: 0.0,
                per_tuple: qc,
            },
            &self.g,
        );
        if param_join {
            let req = req_all;
            let jp = JP {
                node: JNode::Join {
                    method: JoinMethod::NestLoop,
                    jt: ex.jt,
                    outer: Rc::new(op.clone()),
                    inner: Rc::new(ip.clone()),
                    quals,
                    keys: Vec::new(),
                    outer_sort: None,
                    inner_sort: None,
                    material: false,
                    inner_unique: ex.inner_unique,
                    semi: ex.semi,
                    indexed,
                    scansel: None,
                    rows,
                    batches: 1,
                },
                rows,
                startup: c.startup,
                total: c.total,
                width: ex.width,
                disabled: c.disabled,
                keys: Vec::new(),
                req,
                set: ex.set,
            };
            let params = &mut self.rels[ri].param;
            match params.iter().position(|o| o.req == req) {
                Some(i) if jp.disabled < params[i].disabled
                    || (jp.disabled == params[i].disabled && jp.total < params[i].total) =>
                {
                    params[i] = jp
                }
                Some(_) => {}
                None => params.push(jp),
            }
            return;
        }
        let jp = JP {
            node: JNode::Join {
                method: JoinMethod::NestLoop,
                jt: ex.jt,
                outer: Rc::new(op.clone()),
                inner: Rc::new(ip.clone()),
                quals,
                keys: Vec::new(),
                outer_sort: None,
                inner_sort: None,
                material: matches!(ip.node, JNode::Material(_)),
                inner_unique: ex.inner_unique,
                semi: ex.semi,
                indexed,
                scansel: None,
                rows: ex.rows,
                batches: 1,
            },
            rows: ex.rows,
            startup: c.startup,
            total: c.total,
            width: ex.width,
            disabled: c.disabled,
            keys,
            req: 0,
            set: ex.set,
        };
        let startup = self.startup;
        add_jpath(&mut self.rels[ri].paths, jp, startup);
    }

    fn try_hash(&mut self, ri: usize, op: &JP, ip: &JP, hash: &[(usize, bool)], ex: &Extra) {
        let keys: Vec<usize> = hash.iter().map(|h| h.0).collect();
        let quals: Vec<usize> = ex
            .restrict
            .iter()
            .copied()
            .filter(|c| !keys.contains(c))
            .collect();
        let (nb, nbatch) = joins::hash_table_size(ip.rows, ip.width, &self.g);
        let vb = nb as f64 * nbatch as f64;
        let mut bucket = (1.0_f64, 1.0_f64);
        let mut inner_sel = 1.0;
        for &(c, _) in hash {
            inner_sel *= self.clause_sel(&self.clauses[c], None);
        }
        // estimate_multivariate_bucketsize: the inner keys of one relation with
        // statistics objects first, by their n-distinct coefficients.
        let inner_var = |c: usize, outer_is_a: bool| {
            let (a, b, _, _) = self.clauses[c].eq.as_ref().unwrap();
            self.join_var(if outer_is_a { b } else { a })
        };
        let mut others: Vec<(usize, bool)> = Vec::new();
        if hash.len() > 1 {
            let cat = &self.pl.cat;
            let mut groups: Vec<(u32, Vec<(usize, bool)>, Vec<crate::sql::extstat::GroupVar>)> =
                Vec::new();
            for &(c, o) in hash {
                let jv = inner_var(c, o);
                match jv.var {
                    Some(v) if cat.statistics.values().any(|s| s.table == v.table.oid) => {
                        let gv = crate::sql::extstat::GroupVar::of(cat, &v);
                        match groups.iter_mut().find(|g| g.0 == v.table.oid) {
                            Some(g) => {
                                g.1.push((c, o));
                                g.2.push(gv);
                            }
                            None => groups.push((v.table.oid, vec![(c, o)], vec![gv])),
                        }
                    }
                    _ => others.push((c, o)),
                }
            }
            let mut nd: f64 = 1.0;
            for (_, cs, vars) in groups {
                let (d, left) = crate::sql::extstat::bucket_ndistinct(self.pl.ctx, vars);
                nd = nd.max(d);
                others.extend(left.into_iter().map(|i| cs[i]));
            }
            bucket.0 = 1.0 / nd;
        } else {
            others = hash.to_vec();
        }
        for &(c, outer_is_a) in &others {
            let jv = inner_var(c, outer_is_a);
            let (mcv, bs) = if let (Some(_), Some(d)) = (&jv.var, jv.distinct.filter(|d| *d >= 1.0)) {
                // (A unique column (a single grouping key): as many values as rows.)
                (0.0, if d > vb { 1.0 / vb } else { 1.0 / d })
            } else if jv.var.is_none() {
                // (A known distinct count, as of a unique ctid: estimate_hash_bucket_stats
                // without statistics.)
                match jv.distinct {
                    Some(d) if d >= 1.0 => (0.0, if d > vb { 1.0 / vb } else { 1.0 / d }),
                    _ => (0.0, 0.1_f64.max(1.0 / vb)),
                }
            } else {
                joins::bucket_stats(&self.pl.cat, jv.var.as_ref(), jv.rel_rows, vb)
            };
            bucket.0 = bucket.0.min(bs);
            bucket.1 = bucket.1.min(mcv);
        }
        let info = joins::HashInfo {
            nclauses: keys.len(),
            hash_quals: QualCost {
                startup: 0.0,
                per_tuple: keys.iter().map(|&c| self.clauses[c].cost).sum(),
            },
            other_quals: QualCost {
                startup: 0.0,
                per_tuple: quals.iter().map(|&c| self.clauses[c].cost).sum::<f64>()
                    + if ex.set == (1u32 << self.leaves.len()) - 1 { self.post } else { 0.0 },
            },
            // (A unique-ified inner side: one row per bucket.)
            bucket: (!matches!(ip.node, JNode::Unique { .. })).then_some(bucket),
            hash_tuples: costs::clamp_rows(inner_sel * op.rows * ip.rows),
        };
        let (c, batches) = joins::hashjoin(
            ex.jt,
            &op.input(),
            &ip.input(),
            ex.inner_unique,
            ex.semi,
            &info,
            &self.g,
        );
        let jp = JP {
            node: JNode::Join {
                method: JoinMethod::Hash,
                jt: ex.jt,
                outer: Rc::new(op.clone()),
                inner: Rc::new(ip.clone()),
                quals,
                keys: hash.iter().map(|h| h.0).collect(),
                outer_sort: None,
                inner_sort: None,
                material: false,
                inner_unique: ex.inner_unique,
                semi: ex.semi,
                indexed: false,
                scansel: None,
                rows: ex.rows,
                batches,
            },
            rows: ex.rows,
            startup: c.startup,
            total: c.total,
            width: ex.width,
            disabled: c.disabled,
            keys: Vec::new(),
            req: 0,
            set: ex.set,
        };
        let startup = self.startup;
        add_jpath(&mut self.rels[ri].paths, jp, startup);
    }

    /// mergejoinscansel of a merge clause: (outer start, outer end, inner start, inner end).
    fn scansel(&self, c: usize, outer_is_a: bool) -> (f64, f64, f64, f64) {
        let (a, b, _, _) = self.clauses[c].eq.as_ref().unwrap();
        let (oe, ie) = if outer_is_a { (a, b) } else { (b, a) };
        let (Some(og), Some(ig)) = (key_col(oe), key_col(ie)) else {
            return (0.0, 1.0, 0.0, 1.0);
        };
        let (Some(ov), Some(iv)) = (self.var_of(og).0, self.var_of(ig).0) else {
            return (0.0, 1.0, 0.0, 1.0);
        };
        let vars = |_: usize| None;
        let est = Est {
            cat: &self.pl.cat,
            fmt: self.pl.ctx.fmt(),
            vars: &vars,
            exprs: &[],
            endpoint: Some(&|v, min| self.pl.endpoint(v, min)),
            distinct: None,
            user: None,
        };
        let (Some((omin, omax)), Some((imin, imax))) = (est.var_range(&ov), est.var_range(&iv))
        else {
            return (0.0, 1.0, 0.0, 1.0);
        };
        const DEF: f64 = 0.3333333333333333;
        let (mut os, mut oe_, mut is, mut ie_) = (0.0, 1.0, 0.0, 1.0);
        let s = est.scalar_ineq(&ov, Some(&imax), false, true, true);
        if s != DEF {
            oe_ = s;
        }
        let s = est.scalar_ineq(&iv, Some(&omax), false, true, true);
        if s != DEF {
            ie_ = s;
        }
        if oe_ > ie_ {
            oe_ = 1.0;
        } else if oe_ < ie_ {
            ie_ = 1.0;
        } else {
            oe_ = 1.0;
            ie_ = 1.0;
        }
        let s = est.scalar_ineq(&ov, Some(&imin), false, false, true);
        if s != DEF {
            os = s;
        }
        let s = est.scalar_ineq(&iv, Some(&omin), false, false, true);
        if s != DEF {
            is = s;
        }
        if os < is {
            os = 0.0;
        } else if os > is {
            is = 0.0;
        } else {
            os = 0.0;
            is = 0.0;
        }
        if os >= oe_ {
            os = 0.0;
            oe_ = 1.0;
        }
        if is >= ie_ {
            is = 0.0;
            ie_ = 1.0;
        }
        (os, oe_, is, ie_)
    }

    #[allow(clippy::too_many_arguments)]
    fn try_merge(
        &mut self,
        ri: usize,
        op: &JP,
        ip: &JP,
        keys: Vec<GKey>,
        clauses: &[(usize, bool)],
        outer_sort: Option<Vec<GKey>>,
        inner_sort: Option<Vec<GKey>>,
        ex: &Extra,
    ) {
        if clauses.is_empty() {
            return;
        }
        // Skip a sort the input's order already gives.
        let mut presorted = 0;
        let outer_sort = outer_sort.filter(|k| {
            presorted = k.iter().zip(&op.keys).take_while(|(a, b)| a == b).count();
            presorted < k.len()
        });
        let inner_sort = inner_sort.filter(|k| !ip.keys.starts_with(k));
        let ckeys: Vec<usize> = clauses.iter().map(|c| c.0).collect();
        let quals: Vec<usize> = ex
            .restrict
            .iter()
            .copied()
            .filter(|c| !ckeys.contains(c))
            .collect();
        let mut msel = 1.0;
        for &c in &ckeys {
            msel *= self.clause_sel(&self.clauses[c], None);
        }
        let scansel = match clauses.first() {
            Some(&(c, oa)) => self.scansel(c, oa),
            None => (0.0, 1.0, 0.0, 1.0),
        };
        let groups = if presorted > 0 {
            // (the presorted keys' distinct count, as estimate_num_groups would)
            costs::clamp_rows(op.rows.min(costs::DEFAULT_NUM_DISTINCT))
        } else {
            1.0
        };
        let info = joins::MergeInfo {
            merge_quals: QualCost {
                startup: 0.0,
                per_tuple: ckeys.iter().map(|&c| self.clauses[c].cost).sum(),
            },
            other_quals: QualCost {
                startup: 0.0,
                per_tuple: quals.iter().map(|&c| self.clauses[c].cost).sum::<f64>()
                    + if ex.set == (1u32 << self.leaves.len()) - 1 { self.post } else { 0.0 },
            },
            scansel,
            merge_tuples: costs::clamp_rows(msel * op.rows * ip.rows),
            all_merge: quals.is_empty(),
            outer_sort: outer_sort.as_ref().map(|_| joins::SortIn {
                presorted,
                groups,
            }),
            inner_sort: inner_sort.is_some(),
            outer_unique: matches!(op.node, JNode::Unique { .. }),
            material: None,
        };
        let (c, material, _, _) =
            joins::mergejoin(ex.jt, &op.input(), &ip.input(), ex.inner_unique, &info, &self.g);
        let jp = JP {
            node: JNode::Join {
                method: JoinMethod::Merge,
                jt: ex.jt,
                outer: Rc::new(op.clone()),
                inner: Rc::new(ip.clone()),
                quals,
                keys: ckeys,
                outer_sort,
                inner_sort,
                material,
                inner_unique: ex.inner_unique,
                semi: ex.semi,
                indexed: false,
                scansel: Some(scansel),
                rows: ex.rows,
                batches: 1,
            },
            rows: ex.rows,
            startup: c.startup,
            total: c.total,
            width: ex.width,
            disabled: c.disabled,
            keys,
            req: 0,
            set: ex.set,
        };
        let startup = self.startup;
        add_jpath(&mut self.rels[ri].paths, jp, startup);
    }
}

// ── the search ──

impl Search<'_, '_, '_> {
    /// standard_join_search's levels (join_search_one_level).
    fn run(&mut self) -> Option<(JP, bool)> {
        let n = self.leaves.len();
        for l in 0..n {
            self.base_rel(l);
        }
        // Rels of each level, in creation order.
        let mut levels: Vec<Vec<usize>> = vec![Vec::new(); n + 1];
        levels[1] = (0..n).collect();
        let has_clause = |s: &Search, a: u32, b: u32| {
            if s.general {
                s.relevant_clause(a, b) || s.order_restriction(a, b)
            } else {
                // have_relevant_joinclause: a clause over both, even one that needs more
                // items before it can be checked (a parameterized path's).
                !s.crossing(a, b).is_empty()
                    || s.clauses.iter().any(|c| c.leaves & a != 0 && c.leaves & b != 0)
            }
        };
        for level in 2..=n {
            let before = self.rels.len();
            let prev = levels[level - 1].clone();
            for (pi, &r) in prev.iter().enumerate() {
                let rs = self.rels[r].set;
                let has_join = if self.general {
                    self.clauses.iter().any(|c| c.req & rs != 0 && c.req & !rs != 0)
                        || self.has_join_restriction(rs)
                } else {
                    self.clauses.iter().any(|c| c.leaves & rs != 0 && c.leaves & !rs != 0)
                };
                let first = if level == 2 { pi + 1 } else { 0 };
                for &b in levels[1].iter().skip(first) {
                    let bs = self.rels[b].set;
                    if rs & bs != 0 {
                        continue;
                    }
                    if has_join {
                        if has_clause(self, rs, bs) {
                            self.make_join_rel(r, b);
                        }
                    } else {
                        self.make_join_rel(r, b);
                    }
                }
            }
            // Bushy joins.
            let mut k = 2;
            while k <= level - k {
                let other = level - k;
                let ks = levels[k].clone();
                for (ai, &a) in ks.iter().enumerate() {
                    let as_ = self.rels[a].set;
                    let joinable = if self.general {
                        self.clauses.iter().any(|c| c.req & as_ != 0 && c.req & !as_ != 0)
                            || self.has_join_restriction(as_)
                    } else {
                        self.clauses.iter().any(|c| c.leaves & as_ != 0 && c.leaves & !as_ != 0)
                    };
                    if !joinable {
                        continue;
                    }
                    let first = if k == other { ai + 1 } else { 0 };
                    let os = levels[other].clone();
                    for &b in os.iter().skip(first) {
                        let bs = self.rels[b].set;
                        if as_ & bs == 0 && has_clause(self, as_, bs) {
                            self.make_join_rel(a, b);
                        }
                    }
                }
                k += 1;
            }
            if self.rels.len() == before {
                // Cartesian products, as a last resort.
                for &r in &prev {
                    let rs = self.rels[r].set;
                    for &b in &levels[1] {
                        let bs = self.rels[b].set;
                        if rs & bs == 0 {
                            self.make_join_rel(r, b);
                        }
                    }
                }
            }
            levels[level] = (before..self.rels.len()).collect();
        }
        let full = (1u32 << n) - 1;
        let top = self.rel_index(full)?;
        self.final_path(top)
    }

    /// The query's order as group keys.
    fn query_keys(&self) -> Vec<GKey> {
        self.order
            .as_ref()
            .map(|o| {
                o.keys
                    .iter()
                    .map(|&(g, desc, nulls_first)| GKey {
                        ec: self.ec[g],
                        desc,
                        nulls_first,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// create_ordered_paths (and the LIMIT's fractional choice) over the top relation:
    /// the path, and whether it delivers the ORDER BY.
    fn final_path(&self, top: usize) -> Option<(JP, bool)> {
        let rel = &self.rels[top];
        let cheapest = cheapest_of(&rel.paths, false)?.clone();
        let Some(o) = self.order.as_ref().filter(|o| !o.hint) else {
            return Some((cheapest, false));
        };
        let want = self.query_keys();
        let limit_tuples = match o.limit {
            Some((c, off)) if c > 0.0 && off >= 0.0 => Some(c + off),
            _ => None,
        };
        // (path, sorted already)
        let mut out: Vec<(JP, bool)> = Vec::new();
        for p in rel.paths.iter().filter(|p| p.req == 0) {
            let pre = want.iter().zip(&p.keys).take_while(|(a, b)| a == b).count();
            let sorted = o.complete && pre == want.len();
            let is_cheapest = p.total == cheapest.total && p.startup == cheapest.startup;
            let cand = if sorted {
                p.clone()
            } else {
                if !is_cheapest && (pre == 0 || !self.g.incremental_sort) {
                    continue;
                }
                let (st, tot) = if pre == 0 || !self.g.incremental_sort {
                    costs::sort_cost(p.total, p.rows, o.width, limit_tuples)
                } else {
                    costs::incremental_sort_cost(
                        p.startup,
                        p.total,
                        p.rows,
                        o.width,
                        p.rows.clamp(1.0, costs::DEFAULT_NUM_DISTINCT),
                        limit_tuples,
                    )
                };
                let mut q = p.clone();
                q.startup = st;
                q.total = tot;
                q.keys = want.clone();
                q
            };
            // add_path among the ordered candidates.
            let mut list: Vec<JP> = out.iter().map(|x| x.0.clone()).collect();
            let before = list.len();
            add_jpath(&mut list, cand.clone(), self.startup);
            let kept: Vec<(JP, bool)> = list
                .into_iter()
                .map(|x| {
                    let s = out
                        .iter()
                        .find(|(y, _)| y.total == x.total && y.startup == x.startup)
                        .map_or(sorted, |(_, s)| *s);
                    (x, s)
                })
                .collect();
            let _ = before;
            out = kept;
        }
        let Some((count, offset)) = o.limit else {
            let mut best: Option<&(JP, bool)> = None;
            for x in &out {
                if best.is_none_or(|b| cheaper(&x.0, &b.0, false)) {
                    best = Some(x);
                }
            }
            return best.cloned().or(Some((cheapest, false)));
        };
        let fraction = if count < 0.0 || (count > 0.0 && offset < 0.0) {
            0.1
        } else {
            1.0
        };
        let mut best: Option<((JP, bool), f64, f64)> = None;
        for x in &out {
            let p = Path {
                kind: PathKind::Seq,
                startup: x.0.startup,
                total: x.0.total,
                rows: x.0.rows,
                keys: Vec::new(),
                disabled: x.0.disabled,
            };
            let (st, tot) = limit_costs(&p, count, offset);
            let cost = st + fraction * (tot - st);
            let better = match &best {
                None => true,
                Some((b, bc, bst)) => {
                    if x.0.disabled != b.0.disabled {
                        x.0.disabled < b.0.disabled
                    } else if cost != *bc {
                        cost < *bc
                    } else {
                        st < *bst
                    }
                }
            };
            if better {
                best = Some((x.clone(), cost, st));
            }
        }
        best.map(|b| b.0).or(Some((cheapest, false)))
    }

    fn col_width(&self, g: usize) -> u32 {
        let l = self.leaf_of[g];
        let leaf = &self.leaves[l];
        leaf.cols.get(g - leaf.off).map_or(0, |c| c.0)
    }
}

// ── building the plan ──

impl Search<'_, '_, '_> {
    /// The plan of a path, and where each group column is in its rows.
    /// The values from outside a parameterized path its scans read (over the group row).
    fn outer_params(&self, p: &JP) -> Vec<Expr> {
        fn walk(s: &Search, n: &JP, set: u32, out: &mut Vec<Expr>) {
            match &n.node {
                JNode::Scan { params, .. }
                | JNode::Append { params, .. }
                | JNode::Opaque { params, .. } => {
                    // (Their columns from outside: a parameter may mix them with ones
                    // inside.)
                    for e in params {
                        e.walk(&mut |x| {
                            if let Expr::Col(g, _) = x
                                && s.leaf_of.get(*g).is_some_and(|&l| (1u32 << l) & set == 0)
                                && !out.iter().any(|y| y.same_as(x))
                            {
                                out.push(x.clone());
                            }
                        });
                    }
                }
                JNode::Memoize { input, .. } | JNode::Material(input) => walk(s, input, set, out),
                JNode::Join { outer, inner, quals, .. } => {
                    walk(s, outer, set, out);
                    walk(s, inner, set, out);
                    // (A parameterized join's clauses over relations outside it.)
                    if n.req != 0 {
                        for &q in quals {
                            s.clauses[q].expr.walk(&mut |x| {
                                if let Expr::Col(g, _) = x
                                    && s.leaf_of.get(*g).is_some_and(|&l| (1u32 << l) & set == 0)
                                    && !out.iter().any(|y| y.same_as(x))
                                {
                                    out.push(x.clone());
                                }
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        walk(self, p, p.set, &mut out);
        out
    }

    /// A parameter (over the group row) as a nested loop over `layout` evaluates it: its
    /// outer row's column, or a value an enclosing nested loop passed in.
    fn param_ref(&self, e: &Expr, layout: &[usize]) -> Expr {
        let pos = |g: usize| layout.iter().position(|&x| x == g);
        if e.columns().iter().all(|&g| pos(g).is_some()) {
            return e.clone().remap_cols(&|g| pos(g).unwrap_or(g));
        }
        let stack = self.param_stack.borrow();
        let top = stack.last();
        e.clone()
            .transform(&mut |x| {
                Ok(match x {
                    Expr::Col(g, t) => match pos(g) {
                        Some(p) => Expr::Col(p, t),
                        None => match top.and_then(|ps| {
                            ps.iter().position(|y| matches!(y, Expr::Col(h, _) if *h == g))
                        }) {
                            Some(j) => Expr::Outer(1, j, t),
                            None => Expr::Col(g, t),
                        },
                    },
                    x => x,
                })
            })
            .unwrap_or_else(|_| e.clone())
    }

    fn build(&self, p: &JP) -> (Plan, Vec<usize>) {
        match &p.node {
            JNode::Opaque { leaf, .. } => {
                let l = &self.leaves[*leaf];
                (l.plan.clone(), (l.off..l.off + l.ncols).collect())
            }
            JNode::Scan {
                leaf,
                path,
                params,
                rel,
                enforced,
            } => {
                let l = &self.leaves[*leaf];
                let (s, _) = l.scan.as_ref().expect("a scan leaf");
                let mut s = s.clone();
                if !params.is_empty() {
                    // The parameterized clauses (one per clause enforced) join the scan's
                    // own filter.
                    let extra: Vec<Expr> =
                        rel.quals[rel.quals.len() - enforced.len()..].to_vec();
                    let mut all = Vec::new();
                    if let Some(f) = s.filter.take() {
                        conjuncts(f, &mut all);
                    }
                    for q in &mut all {
                        *q = deepen_outer(q).expect("checked by shallow_param");
                    }
                    all.extend(extra);
                    s.filter = Some(if all.len() == 1 {
                        all.pop().unwrap()
                    } else {
                        Expr::And(all)
                    });
                }
                apply(rel, &mut s, path);
                let scan = Plan::Scan(Box::new(s));
                let plan = match &l.proj {
                    Some(exprs) => Plan::Project {
                        input: Arc::new(scan),
                        exprs: exprs.clone(),
                    },
                    None => scan,
                };
                (plan, (l.off..l.off + l.ncols).collect())
            }
            JNode::Append {
                leaf,
                scans,
                params,
                ..
            } => {
                let l = &self.leaves[*leaf];
                let (Plan::Append { inputs, inh }, Some(members)) = (&l.plan, &l.members) else {
                    unreachable!("an Append leaf")
                };
                let mut inputs = inputs.clone();
                for (m, (path, rel)) in members.iter().zip(scans) {
                    let mut s = m.scan.clone();
                    let extra: Vec<Expr> = rel.quals[rel.quals.len() - params.len()..].to_vec();
                    let mut all = Vec::new();
                    if let Some(f) = s.filter.take() {
                        conjuncts(f, &mut all);
                    }
                    for q in &mut all {
                        *q = deepen_outer(q).expect("checked by shallow_param");
                    }
                    all.extend(extra);
                    s.filter = Some(if all.len() == 1 {
                        all.pop().unwrap()
                    } else {
                        Expr::And(all)
                    });
                    s.access = Access::Unplanned;
                    apply(rel, &mut s, path);
                    let scan = Plan::Scan(Box::new(s));
                    inputs[m.input] = Arc::new(match &*inputs[m.input] {
                        Plan::Project { exprs, .. } => Plan::Project {
                            exprs: exprs.clone(),
                            input: Arc::new(scan),
                        },
                        _ => scan,
                    });
                }
                (
                    Plan::Append {
                        inputs,
                        inh: inh.clone(),
                    },
                    (l.off..l.off + l.ncols).collect(),
                )
            }
            JNode::Material(inner) | JNode::Memoize { input: inner, .. } => self.build(inner),
            JNode::Unique {
                input,
                hashed,
                cols,
            } => {
                let (p, layout) = self.build(input);
                let pos = |g: usize| layout.iter().position(|&x| x == g).unwrap_or(g);
                let exprs: Vec<Expr> = cols
                    .iter()
                    .map(|&g| Expr::Col(pos(g), self.col_type(g)))
                    .collect();
                let proj = Plan::Project {
                    input: Arc::new(p),
                    exprs,
                };
                let plan = if *hashed {
                    Plan::Distinct {
                        input: Arc::new(proj),
                        on: None,
                        order: 0,
                    }
                } else {
                    let keys: Vec<SortKey> = cols
                        .iter()
                        .enumerate()
                        .map(|(i, &g)| SortKey {
                            expr: Expr::Col(i, self.col_type(g)),
                            desc: false,
                            nulls_first: false,
                            using: None,
                        })
                        .collect();
                    let on = keys.iter().map(|k| k.expr.clone()).collect();
                    Plan::Distinct {
                        input: Arc::new(Plan::Sort {
                            input: Arc::new(proj),
                            keys,
                        }),
                        on: Some(on),
                        order: 0,
                    }
                };
                (plan, cols.clone())
            }
            JNode::Join {
                method,
                jt,
                outer,
                inner,
                quals,
                keys,
                outer_sort,
                inner_sort,
                material,
                inner_unique,
                semi,
                indexed,
                scansel,
                rows,
                ..
            } => {
                // A right semi / anti join: the plan's left input (whose rows come out) is
                // the path's inner side.
                let swapped = matches!(jt, Jt::RightSemi | Jt::RightAnti);
                let (lnode, rnode) = if swapped { (inner, outer) } else { (outer, inner) };
                let (lsort, rsort) = if swapped {
                    (inner_sort, outer_sort)
                } else {
                    (outer_sort, inner_sort)
                };
                let (mut lp, ll) = self.build(lnode);
                // A parameterized join inside: the values from outside it it reads come in
                // as this nested loop's parameters.
                let join_params: Option<Vec<Expr>> = (*method == JoinMethod::NestLoop
                    && !swapped
                    && inner.req != 0
                    && matches!(inner.memoized().node, JNode::Join { .. }))
                .then(|| self.outer_params(inner.memoized()));
                if let Some(ps) = &join_params {
                    self.param_stack.borrow_mut().push(ps.clone());
                }
                let (mut rp, rl) = self.build(rnode);
                if join_params.is_some() {
                    self.param_stack.borrow_mut().pop();
                }
                let pos = |layout: &[usize], g: usize| layout.iter().position(|&x| x == g);
                // (A parameterized join's clauses read the values from outside it as the
                // enclosing nested loop's parameters.)
                let remap = |e: &Expr, layout: &[usize]| -> Expr { self.param_ref(e, layout) };
                let sorted = |plan: Plan, layout: &[usize], ks: &Option<Vec<GKey>>| -> Plan {
                    let Some(ks) = ks else { return plan };
                    let keys = ks
                        .iter()
                        .filter_map(|k| {
                            let Some(g) = (0..self.ec.len()).find(|&g| self.ec[g] == k.ec && pos(layout, g).is_some()) else {
                                // (Sorted by an expression of the class: a merge clause's.)
                                let e = self.ec_expr_in(k.ec, layout)?;
                                return Some(SortKey {
                                    expr: e.remap_cols(&|g| pos(layout, g).unwrap_or(g)),
                                    desc: k.desc,
                                    nulls_first: k.nulls_first,
                                    using: None,
                                });
                            };
                            let ty = self.col_type(g);
                            Some(SortKey {
                                expr: Expr::Col(pos(layout, g)?, ty),
                                desc: k.desc,
                                nulls_first: k.nulls_first,
                                using: None,
                            })
                        })
                        .collect();
                    Plan::Sort {
                        input: Arc::new(plan),
                        keys,
                    }
                };
                lp = sorted(lp, &ll, lsort);
                rp = sorted(rp, &rl, rsort);
                let mut both = ll.clone();
                both.extend(rl.iter().copied());
                let nleft = ll.len();
                let nright = rl.len();
                // At an outer (or semi / anti) join, the clauses pushed down to it are
                // checked on its result (Postgres's "other quals": the join's Filter).
                let post_join = |c: usize| {
                    self.general && *jt != Jt::Inner && self.clauses[c].sj.is_none()
                };
                let post: Vec<usize> = quals.iter().copied().filter(|&c| post_join(c)).collect();
                let mut cond: Vec<Expr> = quals
                    .iter()
                    .filter(|&&c| !post_join(c))
                    .map(|&c| remap(&self.clauses[c].expr, &both))
                    .collect();
                let mut hash = None;
                let mut merge_dirs = Vec::new();
                let mut key_flipped = Vec::new();
                // A LATERAL inner side reads the outer row anyway: the join clauses it can
                // check itself go into it (its parameterized path's clauses), as a filter
                // over it.
                let mut lateral_params: Option<Vec<Expr>> = None;
                let mut moved_clauses: Vec<usize> = Vec::new();
                if *method == JoinMethod::NestLoop
                    && !swapped
                    && let JNode::Opaque {
                        leaf,
                        params: lparams,
                        ..
                    } = &inner.node
                    && !lparams.is_empty()
                {
                    let lf = &self.leaves[*leaf];
                    let src = lf.lateral.as_ref().map_or(0, |l| l.1);
                    let movable: Vec<usize> = quals
                        .iter()
                        .chain(keys.iter())
                        .copied()
                        .filter(|&c| {
                            !post_join(c)
                                && self.clauses[c].leaves & inner.set != 0
                                && self.clauses[c].leaves & !(inner.set | src) == 0
                        })
                        .collect();
                    if !movable.is_empty() {
                        let mut ps = lparams.clone();
                        let mut moved = Vec::new();
                        for &c in &movable {
                            let e = self.clauses[c].expr.clone().transform(&mut |x| {
                                Ok(match x {
                                    Expr::Col(g, t) if g >= lf.off && g < lf.off + lf.ncols => {
                                        Expr::Col(g - lf.off, t)
                                    }
                                    Expr::Col(g, t) => {
                                        let k = match ps.iter().position(
                                            |p| matches!(p, Expr::Col(h, _) if *h == g),
                                        ) {
                                            Some(k) => k,
                                            None => {
                                                ps.push(Expr::Col(g, t.clone()));
                                                ps.len() - 1
                                            }
                                        };
                                        Expr::Outer(1, k, t)
                                    }
                                    x => x,
                                })
                            });
                            if let Ok(e) = e {
                                moved.push(e);
                            }
                        }
                        if moved.len() == movable.len() {
                            let mut drop = Vec::new();
                            for &c in &movable {
                                drop.push(remap(&self.clauses[c].expr, &both));
                            }
                            cond.retain(|e| !drop.iter().any(|d| d.same_as(e)));
                            let r = std::mem::replace(&mut rp, Plan::One);
                            rp = crate::sql::binder::optimize::add_filter(
                                &self.pl.cat,
                                r,
                                Expr::And(moved),
                            );
                            lateral_params = Some(ps);
                            moved_clauses = movable;
                        }
                    }
                }
                if *method == JoinMethod::NestLoop {
                    let ks: Vec<Expr> = keys
                        .iter()
                        .filter(|&&c| !moved_clauses.contains(&c))
                        .map(|&c| remap(&self.clauses[c].expr, &both))
                        .collect();
                    // (build_joinrel_restrictlist: an inner join's equivalence class clauses
                    // come first; then order_qual_clauses, cheapest first, stable.)
                    if *jt == Jt::Inner {
                        let rest = std::mem::take(&mut cond);
                        cond = ks;
                        cond.extend(rest);
                    } else {
                        cond.extend(ks);
                    }
                    cond.sort_by(|a, b| costs::qual_cost(a).total_cmp(&costs::qual_cost(b)));
                } else {
                    let (mut lk, mut rk) = (Vec::new(), Vec::new());
                    for &c in keys {
                        let (a, b, al, _) = self.clauses[c].eq.as_ref().unwrap();
                        let a_outer = *al & outer.set != 0 && *al & !outer.set == 0;
                        let (oe, ie) = if a_outer { (a, b) } else { (b, a) };
                        // (The clause as the join's restriction list has it, its first
                        // operand on the right: a nested loop planned over this join
                        // again shows it so.)
                        key_flipped.push(a_outer == swapped);
                        if *method == JoinMethod::Merge {
                            let ok = self.side_ec(oe).and_then(|ec| {
                                outer_sort
                                    .as_deref()
                                    .unwrap_or(&outer.keys)
                                    .iter()
                                    .find(|k| k.ec == ec)
                            });
                            merge_dirs.push(ok.map_or((false, false), |k| (k.desc, k.nulls_first)));
                        }
                        let (le, re) = if swapped { (ie, oe) } else { (oe, ie) };
                        // Keys without a collation fail as they're hashed.
                        let eq = &self.clauses[c].expr;
                        lk.push(crate::sql::binder::hash_key(eq, remap(le, &ll)));
                        rk.push(crate::sql::binder::hash_key(eq, remap(re, &rl)));
                    }
                    if !lk.is_empty() {
                        hash = Some((lk, rk));
                    }
                }
                // The columns of the clauses a parameterized inner scan enforces stay in
                // the join's row (their attr_needed).
                let mut kept = Vec::new();
                // So do an equivalence class's columns from a side that hasn't all its
                // FROM items yet: they are needed up to the join of them all (its
                // ec_relids).
                if self.clauses.iter().any(|c| c.derived) {
                    let members = self.ec_members();
                    let ec_leaves = |e: usize| {
                        members
                            .iter()
                            .filter(|&&g| self.ec[g] == e)
                            .fold(0u32, |m, &g| m | (1u32 << self.leaf_of[g]))
                    };
                    for &g in &members {
                        for side in [outer.set, inner.set] {
                            let lg = 1u32 << self.leaf_of[g];
                            if lg & side != 0
                                && ec_leaves(self.ec[g]) & !side != 0
                                && let Some(i) = pos(&both, g)
                                && !kept.contains(&i)
                            {
                                kept.push(i);
                            }
                        }
                    }
                }
                if let JNode::Scan { enforced, .. } | JNode::Append { enforced, .. } =
                    &inner.memoized().node
                {
                    for &ci in enforced {
                        for g in self.clauses[ci].expr.columns() {
                            if let Some(i) = pos(&both, g)
                                && !kept.contains(&i)
                            {
                                kept.push(i);
                            }
                        }
                    }
                }
                // A parameterized inner scan: the outer values it reads.
                let params = match &inner.memoized().node {
                    _ if lateral_params.is_some() => lateral_params
                        .as_ref()
                        .unwrap()
                        .iter()
                        .map(|e| remap(e, &ll))
                        .collect(),
                    _ if join_params.is_some() => join_params
                        .as_ref()
                        .unwrap()
                        .iter()
                        .map(|e| self.param_ref(e, &ll))
                        .collect(),
                    JNode::Scan { params, .. }
                    | JNode::Append { params, .. }
                    | JNode::Opaque { params, .. } => {
                        params.iter().map(|e| self.param_ref(e, &ll)).collect()
                    }
                    _ => Vec::new(),
                };
                let kind = match jt {
                    Jt::Inner => JoinKind::Inner,
                    Jt::Left => JoinKind::Left,
                    Jt::Right => JoinKind::Right,
                    Jt::Full => JoinKind::Full,
                    Jt::Semi | Jt::RightSemi => JoinKind::Semi,
                    Jt::Anti | Jt::RightAnti => JoinKind::Anti,
                };
                let plan = Plan::Join {
                    kind,
                    left: Arc::new(lp),
                    right: Arc::new(rp),
                    cond: match cond.len() {
                        0 => None,
                        1 => cond.pop(),
                        _ => Some(Expr::And(cond)),
                    },
                    hash,
                    params,
                    nleft,
                    nright,
                    meta: Box::new(JoinMeta {
                        pushed: Vec::new(),
                        merge_dirs,
                        kept,
                        method: Some(*method),
                        material: *material,
                        inner_unique: *inner_unique,
                        rows: Some(*rows),
                        semi: semi.map(|s| (s.outer_match_frac, s.match_count)),
                        indexed: *indexed,
                        scansel: *scansel,
                        swapped,
                        unique_outer: matches!(outer.node, JNode::Unique { .. }),
                        unique_inner: matches!(inner.node, JNode::Unique { .. }),
                        hash_buckets: (*method == JoinMethod::Hash)
                            .then(|| joins::hash_table_size(inner.rows, inner.width, &self.g).0),
                        memoize: match &inner.node {
                            JNode::Memoize {
                                keys,
                                binary,
                                singlerow,
                                rescan,
                                ..
                            } => Some(crate::sql::plan::Memo {
                                keys: keys.iter().map(|e| remap(e, &ll)).collect(),
                                binary: *binary,
                                singlerow: *singlerow,
                                rescan: *rescan,
                            }),
                            _ => None,
                        },
                        from_call: None,
                        hash_swapped: Vec::new(),
                        key_flipped: if kind == JoinKind::Inner { key_flipped } else { Vec::new() },
                        written_kind: None,
                        phv_eval: false,
                        presorted: false,
                    }),
                };
                let layout = if matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                    ll
                } else {
                    both
                };
                let plan = if post.is_empty() {
                    plan
                } else {
                    let mut f: Vec<Expr> = post
                        .iter()
                        .map(|&c| remap(&self.clauses[c].expr, &layout))
                        .collect();
                    Plan::Filter {
                        input: Arc::new(plan),
                        pred: if f.len() == 1 { f.pop().unwrap() } else { Expr::And(f) },
                    }
                };
                (plan, layout)
            }
        }
    }

    fn col_type(&self, g: usize) -> DataType {
        let l = self.leaf_of[g];
        let leaf = &self.leaves[l];
        match &leaf.scan {
            Some((s, _)) if g - leaf.off < s.table.columns.len() => {
                s.table.columns[g - leaf.off].ty.clone()
            }
            // (A leaf planned already: the column's type as the clauses read it.)
            _ => {
                let mut found = None;
                for c in &self.clauses {
                    c.expr.walk(&mut |x| {
                        if let Expr::Col(i, t) = x
                            && *i == g
                            && found.is_none()
                        {
                            found = Some(t.clone());
                        }
                    });
                    if found.is_some() {
                        break;
                    }
                }
                found.unwrap_or(DataType::Unknown)
            }
        }
    }
}

/// Whether `=` on the type is hash-joinable in Postgres (oprcanhash).
fn pg_hashjoinable(t: &DataType) -> bool {
    match t {
        DataType::Array(e) => pg_hashjoinable(e),
        DataType::Money => false,
        t => memo_hashable(t),
    }
}

/// Whether a Memoize can hash a cache key of this type (its type has a hash function and
/// an equality operator).
pub(super) fn memo_hashable(t: &DataType) -> bool {
    !matches!(
        t,
        DataType::Json
            | DataType::Xml
            | DataType::Point
            | DataType::Lseg
            | DataType::Path
            | DataType::GBox
            | DataType::Polygon
            | DataType::Line
            | DataType::Circle
            | DataType::TsVector
            | DataType::TsQuery
            | DataType::Bit(_)
            | DataType::Varbit(_)
            | DataType::Unknown
    )
}

// ── entry ──

/// The leaves and clauses of the join group at `p`: inner joins (no LATERAL) gathered
/// into one, or an outer / semi / anti join's two sides.
fn gather(
    p: &Plan,
    off: usize,
    leaves: &mut Vec<(Plan, usize)>,
    clauses: &mut Vec<(Expr, bool)>,
    top: bool,
) {
    if let Plan::Join {
        kind,
        left,
        right,
        cond,
        hash,
        params,
        nleft,
        meta,
        ..
    } = p
        && params.is_empty()
        && (*kind == JoinKind::Inner || top)
        // (A function called in FROM: one relation, its Function Scan.)
        && meta.from_call.is_none()
    {
        let inner_only = *kind == JoinKind::Inner;
        if inner_only {
            gather(left, off, leaves, clauses, false);
            gather(right, off + nleft, leaves, clauses, false);
        } else {
            leaves.push(((**left).clone(), off));
            leaves.push(((**right).clone(), off + nleft));
        }
        if let Some(c) = cond {
            let mut parts = Vec::new();
            conjuncts(c.clone(), &mut parts);
            for e in parts {
                clauses.push((e.shift_cols(off as isize), !inner_only));
            }
        }
        if let Some((lk, rk)) = hash {
            // reconsider_outer_join_clauses: an outer join's clauses with the outer side's
            // value on the left come back first, those with it on the right after them.
            let mut order: Vec<usize> = (0..lk.len()).collect();
            let swapped = |i: usize| meta.hash_swapped.get(i) == Some(&true);
            match kind {
                JoinKind::Left => order.sort_by_key(|&i| swapped(i)),
                JoinKind::Right => order.sort_by_key(|&i| !swapped(i)),
                _ => {}
            }
            for i in order {
                let (a, b) = (&lk[i], &rk[i]);
                // A key wrapped to fail hashing (no collation) is an `=` that fails comparing;
                // one hashing a nondeterministic collation's sort keys, an `=` under it.
                let (a, bad, coll) = crate::sql::binder::hash_key_operand(a);
                let (b, _, _) = crate::sql::binder::hash_key_operand(b);
                let a = a.shift_cols(off as isize);
                let b = b.shift_cols((off + nleft) as isize);
                // (An outer join's clause as written; an inner or semi join's comes from
                // its equivalence class, outer side first: as a planned one had it.)
                let (a, b) = if !inner_only
                    && matches!(kind, JoinKind::Left | JoinKind::Right | JoinKind::Full | JoinKind::Anti)
                    && meta.hash_swapped.get(i) == Some(&true)
                    || inner_only && meta.key_flipped.get(i) == Some(&true)
                {
                    (b, a)
                } else {
                    (a, b)
                };
                let ty = a.ty();
                // Rebuild `a = b` with the `=` the binder found for it.
                let eq = if ty.without_typmod() != b.ty().without_typmod() {
                    crate::sql::func::resolve_binop("=", &ty, &b.ty()).ok().map(|r| r.def)
                } else {
                    equality_for(&ty).or_else(|| crate::sql::func::resolve_binop("=", &ty, &ty).ok().map(|r| r.def))
                };
                if let Some(eq) = eq {
                    let e = crate::sql::binder::eq_under(Expr::Call(eq, vec![a, b], DataType::Bool), coll);
                    let e = if bad {
                        crate::sql::binder::indeterminate_eq(e)
                    } else {
                        e
                    };
                    clauses.push((e, !inner_only));
                }
            }
        }
        return;
    }
    // A pulled-up subquery's WHERE over its inner joins (`lift`): its clauses are the
    // tree's.
    if !top
        && let Plan::Filter { input, pred } = p
        && matches!(&**input, Plan::Join { kind: JoinKind::Inner, params, meta, .. } if params.is_empty() && meta.from_call.is_none())
        && outerjoins::absorbable(pred)
    {
        gather(input, off, leaves, clauses, false);
        let mut parts = Vec::new();
        conjuncts(pred.clone(), &mut parts);
        for e in parts {
            clauses.push((e.shift_cols(off as isize), false));
        }
        return;
    }
    leaves.push((p.clone(), off));
}

thread_local! {
    /// `=` operators met in join clauses, by type (to rebuild hash keys as clauses).
    static EQ_OPS: std::cell::RefCell<Vec<(DataType, Arc<FuncDef>)>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn equality_for(t: &DataType) -> Option<Arc<FuncDef>> {
    EQ_OPS.with(|m| {
        m.borrow()
            .iter()
            .find(|(x, _)| x.without_typmod() == t.without_typmod())
            .map(|(_, d)| d.clone())
    })
}

fn remember_equalities(p: &Plan, ctx: &ExecCtx) {
    // The hash keys' types: an `=` for each, as the binder would resolve it.
    let mut types = Vec::new();
    // (Through every node: a filter or projection between two joins too, whose lower
    // join's keys are rebuilt as clauses all the same.)
    fn walk_types(p: &Plan, out: &mut Vec<DataType>) {
        if let Plan::Join { hash: Some((lk, _)), .. } = p {
            out.extend(lk.iter().map(|e| e.ty()));
        }
        for c in crate::sql::explain::plan_inputs(p) {
            walk_types(c, out);
        }
    }
    walk_types(p, &mut types);
    for t in types {
        if equality_for(&t).is_some() {
            continue;
        }
        let mut b = crate::sql::binder::Binder::new(ctx, Vec::new());
        let (x, y) = (Expr::Col(0, t.clone()), Expr::Col(1, t.clone()));
        if let Ok(Expr::Call(d, ..)) = b.binop("=", x, y, -1) {
            EQ_OPS.with(|m| m.borrow_mut().push((t, d)));
        }
    }
}

impl Planner<'_, '_> {
    /// Plan the join group at `p` (whose consumers need `needed`): its order, methods and
    /// the leaves' paths. False when it isn't one the search takes on.
    pub(super) fn plan_joins(&self, p: &mut Plan, needed: &[bool]) -> bool {
        self.plan_joins_ordered(p, needed, None).is_some()
    }

    /// `plan_joins` under a LIMIT (no order): the cheapest path for the rows it reads.
    pub(super) fn plan_joins_limited(&self, p: &mut Plan, needed: &[bool], lim: Option<(f64, f64)>) -> bool {
        match lim {
            Some((c, off)) if c > 0.0 && off >= 0.0 => {
                let order = Order {
                    keys: Vec::new(),
                    complete: true,
                    limit: Some((c, off)),
                    width: 0,
                    hint: false,
                };
                self.plan_joins_ordered(p, needed, Some(&order)).is_some()
            }
            _ => self.plan_joins(p, needed),
        }
    }

    /// A WHERE filter over a join tree with outer joins inside: its clauses join the
    /// group, checked as low as the outer joins allow. False (nothing done) otherwise.
    pub(super) fn plan_filtered_join(&self, p: &mut Plan, needed: &[bool]) -> bool {
        let Plan::Filter { input, pred } = p else {
            return false;
        };
        if !matches!(&**input, Plan::Join { params, kind, .. } if params.is_empty() && *kind != JoinKind::Full) {
            return false;
        }
        let mut parts = Vec::new();
        conjuncts(pred.clone(), &mut parts);
        let (absorb, keep): (Vec<Expr>, Vec<Expr>) = parts.into_iter().partition(|e| {
            !e.is_volatile()
                && !e.columns().is_empty()
                && !crate::sql::explain::plan_volatile(&Plan::Filter {
                    input: Arc::new(Plan::One),
                    pred: e.clone(),
                })
                && {
                    let mut sub = false;
                    e.walk(&mut |x| sub |= matches!(x, Expr::Subquery(_)));
                    !sub
                }
        });
        if absorb.is_empty() {
            return false;
        }
        let Some(g) = outerjoins::gather_group(input, &[]) else {
            return false;
        };
        if g.sjs.is_empty() {
            return false;
        }
        let w = crate::sql::binder::plan_width(input);
        let mut need = needed.to_vec();
        need.resize(w, false);
        for e in &keep {
            mark(&mut need, e);
        }
        let mut join = (**input).clone();
        self.absorbed.replace(absorb);
        let done = self.plan_joins_with(&mut join, &need, None, &[]).is_some();
        self.absorbed.take();
        if !done {
            return false;
        }
        *p = match keep.len() {
            0 => join,
            1 => Plan::Filter {
                input: Arc::new(join),
                pred: keep.into_iter().next().unwrap(),
            },
            _ => Plan::Filter {
                input: Arc::new(join),
                pred: Expr::And(keep),
            },
        };
        true
    }

    /// A Sort over a join group (through projections): plan the group for that order.
    /// Whether the group was planned here (the Sort removed when its rows come in order).
    pub(super) fn ordered_join(
        &self,
        p: &mut Plan,
        needed: &[bool],
        lim: Option<(f64, f64)>,
    ) -> bool {
        let Plan::Sort { input, keys } = p else {
            return false;
        };
        if keys.iter().any(|k| k.using.is_some()) {
            return false;
        }
        // Down the projections to the join, the keys and the needed columns mapped along.
        let mut key_exprs: Vec<Expr> = keys.iter().map(|k| k.expr.clone()).collect();
        let dirs: Vec<(bool, bool)> = keys.iter().map(|k| (k.desc, k.nulls_first)).collect();
        let mut need = needed.to_vec();
        let mut proj_width: Option<Vec<Expr>> = None;
        let mut cur = take(input);
        loop {
            match cur {
                Plan::Project { input, exprs } => {
                    key_exprs = key_exprs
                        .into_iter()
                        .map(|k| {
                            k.transform(&mut |e| {
                                Ok(match e {
                                    Expr::Col(i, t) => {
                                        exprs.get(i).cloned().unwrap_or(Expr::Col(i, t))
                                    }
                                    e => e,
                                })
                            })
                            .unwrap()
                        })
                        .collect();
                    if proj_width.is_none() {
                        proj_width = Some(exprs.clone());
                    }
                    let iw = crate::sql::binder::plan_width(input);
                    let mut n = vec![false; iw];
                    for (i, e) in exprs.iter().enumerate() {
                        if need.get(i).copied().unwrap_or(true) {
                            mark(&mut n, e);
                        }
                    }
                    need = n;
                    cur = take(input);
                }
                Plan::Join { .. } => break,
                _ => return false,
            }
        }
        let mut okeys = Vec::new();
        let mut complete = true;
        for (e, (desc, nf)) in key_exprs.iter().zip(&dirs) {
            match e {
                Expr::Col(g, _) => okeys.push((*g, *desc, *nf)),
                _ => {
                    complete = false;
                    break;
                }
            }
        }
        // The sort input's width: its projection's (column widths are the leaves').
        let order = Order {
            keys: okeys,
            complete,
            limit: lim,
            width: 0,
            hint: false,
        };
        let w_exprs = proj_width.unwrap_or_default();
        let Some(sorted) = self.plan_joins_with(cur, &need, Some(&order), &w_exprs) else {
            return false;
        };
        if sorted {
            let Plan::Sort { input, .. } = std::mem::replace(p, Plan::One) else {
                unreachable!()
            };
            *p = Arc::try_unwrap(input).unwrap_or_else(|a| (*a).clone());
        }
        true
    }

    /// A grouping over a join group (through projections): the group planned with the
    /// GROUP BY keys as its query order (standard_qp_callback's group_pathkeys), the
    /// ORDER BY's directions on the keys it starts with (`dirs`: per group key).
    pub(super) fn grouped_join(&self, p: &mut Plan, needed: &[bool], dirs: &[(usize, bool, bool)]) -> bool {
        let mut need = super::child_needs(p, needed).pop().unwrap_or_default();
        let Plan::Agg {
            input,
            groups,
            sets: None,
            ..
        } = p
        else {
            return false;
        };
        if groups.is_empty() {
            return false;
        }
        // The GROUP BY's order: the ORDER BY's keys first (preprocess_groupclause).
        let mut order: Vec<(usize, bool, bool)> = Vec::new();
        for &(i, d, nf) in dirs {
            if i < groups.len() && !order.iter().any(|o| o.0 == i) {
                order.push((i, d, nf));
            }
        }
        for i in 0..groups.len() {
            if !order.iter().any(|o| o.0 == i) {
                order.push((i, false, false));
            }
        }
        let mut key_exprs: Vec<Expr> = order.iter().map(|o| groups[o.0].clone()).collect();
        let mut proj_width: Option<Vec<Expr>> = None;
        let mut cur = take(input);
        loop {
            match cur {
                Plan::Project { input, exprs } => {
                    key_exprs = key_exprs
                        .into_iter()
                        .map(|k| {
                            k.transform(&mut |e| {
                                Ok(match e {
                                    Expr::Col(i, t) => {
                                        exprs.get(i).cloned().unwrap_or(Expr::Col(i, t))
                                    }
                                    e => e,
                                })
                            })
                            .unwrap()
                        })
                        .collect();
                    if proj_width.is_none() {
                        proj_width = Some(exprs.clone());
                    }
                    let iw = crate::sql::binder::plan_width(input);
                    let mut n = vec![false; iw];
                    for (i, e) in exprs.iter().enumerate() {
                        if need.get(i).copied().unwrap_or(true) {
                            mark(&mut n, e);
                        }
                    }
                    need = n;
                    cur = take(input);
                }
                Plan::Join { .. } => break,
                _ => return false,
            }
        }
        let mut okeys = Vec::new();
        let mut complete = true;
        for (e, o) in key_exprs.iter().zip(&order) {
            match key_operand(e) {
                Expr::Col(g, _) => okeys.push((*g, o.1, o.2)),
                _ => {
                    complete = false;
                    break;
                }
            }
        }
        if okeys.is_empty() {
            return false;
        }
        let order = Order {
            keys: okeys,
            complete,
            limit: None,
            width: 0,
            hint: true,
        };
        let w_exprs = proj_width.unwrap_or_default();
        self.plan_joins_with(cur, &need, Some(&order), &w_exprs).is_some()
    }

    /// The member scans of an inheritance Append (each a table scan, maybe under the
    /// projection to the parent's columns); None when one is something else.
    fn members(&self, inputs: &[Arc<Plan>], lneed: &[bool]) -> Option<Vec<Member>> {
        let mut out = Vec::new();
        for (i, inp) in inputs.iter().enumerate() {
            let (sp, proj) = match &**inp {
                Plan::Empty { .. } => continue,
                Plan::Scan(sp) => (sp, None),
                Plan::Project { input, exprs } => match &**input {
                    Plan::Scan(sp) => (sp, Some(exprs)),
                    _ => return None,
                },
                _ => return None,
            };
            if sp.sample.is_some() || sp.current_of.is_some() || sp.table.partkey.is_some() {
                return None;
            }
            let map: Vec<Option<usize>> = (0..lneed.len())
                .map(|k| match proj {
                    None => Some(k),
                    Some(ex) => match ex.get(k) {
                        Some(Expr::Col(j, _)) => Some(*j),
                        _ => None,
                    },
                })
                .collect();
            let exprs: Vec<Option<Expr>> = (0..lneed.len())
                .map(|k| match proj {
                    None => sp.table.columns.get(k).map(|c| Expr::Col(k, c.ty.clone())),
                    Some(ex) => ex.get(k).filter(|e| {
                        let mut plain = !e.is_volatile() && !e.has_outer_refs();
                        e.walk(&mut |x| {
                            plain &= !matches!(x, Expr::Subquery(_) | Expr::Param(..))
                        });
                        plain
                    }).cloned(),
                })
                .collect();
            let mut scan = (**sp).clone();
            scan.access = Access::Unplanned;
            let sw = crate::sql::binder::plan_width(&Plan::Scan(Box::new(scan.clone())));
            let mut cneed = vec![false; sw];
            match proj {
                None => {
                    for (k, n) in lneed.iter().enumerate() {
                        if *n && k < sw {
                            cneed[k] = true;
                        }
                    }
                }
                Some(ex) => {
                    for (k, e) in ex.iter().enumerate() {
                        if lneed.get(k).copied().unwrap_or(true) {
                            mark(&mut cneed, e);
                        }
                    }
                }
            }
            let rel = self.rel(&scan, &cneed)?;
            out.push(Member {
                input: i,
                scan,
                rel,
                map,
                exprs,
            });
        }
        (!out.is_empty()).then_some(out)
    }

    /// `plan_joins_with` over the lifted group, its columns put back by the lifted map.
    fn plan_lifted(
        &self,
        p: &mut Plan,
        l: lift::Lifted,
        needed: &[bool],
        order: Option<&Order>,
        sort_exprs: &[Expr],
    ) -> Option<bool> {
        let lift::Lifted { plan: mut lp, map, results } = l;
        // (Clauses the lifting put over a scan or an inheritance tree: its quals, each
        // member's, to be pruned with.)
        fn sink(cat: &crate::catalog::Catalog, p: &mut Plan) {
            match p {
                Plan::Join { left, right, .. } => {
                    sink(cat, Arc::make_mut(left));
                    sink(cat, Arc::make_mut(right));
                }
                Plan::Filter { input, .. }
                    if matches!(&**input, Plan::Scan(_) | Plan::Append { inh: Some(_), .. }) =>
                {
                    let Plan::Filter { input, pred } = std::mem::replace(p, Plan::One) else {
                        unreachable!()
                    };
                    let i = Arc::try_unwrap(input).unwrap_or_else(|a| (*a).clone());
                    *p = crate::sql::binder::optimize::add_filter(cat, i, pred);
                }
                _ => {}
            }
        }
        sink(&self.cat, &mut lp);
        let lw = crate::sql::binder::plan_width(&lp);
        let mut need = vec![false; lw];
        for (i, e) in map.iter().enumerate() {
            if needed.get(i).copied().unwrap_or(true) {
                mark(&mut need, e);
            }
        }
        // The order's keys while they stay columns.
        let order2 = order.map(|o| {
            let mut keys = Vec::new();
            let mut complete = o.complete;
            for &(g, d, nf) in &o.keys {
                match map.get(g) {
                    Some(Expr::Col(j, _)) => keys.push((*j, d, nf)),
                    _ => {
                        complete = false;
                        break;
                    }
                }
            }
            Order {
                keys,
                complete,
                limit: o.limit,
                width: o.width,
                hint: o.hint,
            }
        });
        let order2 = order2.filter(|o| !o.keys.is_empty() || o.complete);
        let sort2: Vec<Expr> = sort_exprs.iter().map(|e| lift::subst(e, &map)).collect();
        // (WHERE clauses the group takes in: over the lifted row too.)
        let extra = self.absorbed.take();
        self.absorbed
            .replace(extra.iter().map(|e| lift::subst(e, &map)).collect());
        // (An empty relation removed, one leaf left: planned as it is.)
        if !matches!(lp, Plan::Join { .. }) {
            self.absorbed.take();
            if !extra.is_empty() {
                lp = Plan::Filter {
                    input: Arc::new(lp),
                    pred: Expr::And(extra.iter().map(|e| lift::subst(e, &map)).collect()),
                };
            }
            self.walk(&mut lp, &need, None);
            *p = Plan::Project {
                input: Arc::new(lp),
                exprs: map,
            };
            self.lifted.borrow_mut().push(p as *const Plan as usize);
            return Some(false);
        }
        let orig_lp = matches!(&*p, Plan::Join { params, .. } if !params.is_empty()).then(|| lp.clone());
        let saved = self.lifting.replace(&lp as *const Plan as usize);
        let r = self.plan_joins_with(&mut lp, &need, order2.as_ref(), &sort2);
        self.lifting.set(saved);
        let Some(sorted) = r else {
            // (Empty relations gone, a LATERAL join of what remains: the tree as it
            // became, its sides planned.)
            if results
                && let Plan::Join { left, right, params, .. } = &mut lp
                && !params.is_empty()
            {
                self.absorbed.take();
                let nl = crate::sql::binder::plan_width(left);
                let w = crate::sql::binder::plan_width(right);
                let lneed: Vec<bool> = (0..nl).map(|i| need.get(i).copied().unwrap_or(true) || params.iter().any(|e| e.columns().contains(&i))).collect();
                let rneed: Vec<bool> = (0..w).map(|i| need.get(nl + i).copied().unwrap_or(true)).collect();
                self.walk(take(left), &lneed, None);
                self.walk(take(right), &rneed, None);
                let input = Arc::new(lp);
                self.lifted.borrow_mut().push(Arc::as_ptr(&input) as usize);
                *p = Plan::Project { input, exprs: map };
                return Some(false);
            }
            self.absorbed.replace(extra);
            let r = self.plan_joins_with_unlifted(p, needed, order, sort_exprs);
            // A LATERAL join the search doesn't take on: its lifted form all the same
            // (its scan returns the relation's columns, then its PlaceHolderVars).
            if r.is_none()
                && matches!(&*p, Plan::Join { params, .. } if !params.is_empty())
                && let Some(orig) = orig_lp
            {
                let input = Arc::new(orig);
                // (Not lifted again when the walk comes to it.)
                self.lifted.borrow_mut().push(Arc::as_ptr(&input) as usize);
                *p = Plan::Project { input, exprs: map };
            }
            return r;
        };
        let sorted = sorted && order2.as_ref().is_some_and(|o| o.complete) == order.is_some_and(|o| o.complete);
        let computed = map.iter().any(|e| !matches!(e, Expr::Col(..)));
        *p = match lp {
            Plan::Project { input, exprs } => Plan::Project {
                input,
                exprs: map.iter().map(|e| lift::subst(e, &exprs)).collect(),
            },
            lp => Plan::Project {
                input: Arc::new(lp),
                exprs: map,
            },
        };
        if computed {
            self.lifted.borrow_mut().push(p as *const Plan as usize);
        }
        Some(sorted)
    }

    /// make_group_input_target: below a grouping, the joins return the columns its
    /// aggregates' arguments read (the arguments are the Agg's to compute); a lifted
    /// expression it groups by stays computed below.
    pub(super) fn lifted_into_aggs(&self, p: &mut Plan) {
        for c in crate::sql::explain::plan_inputs_mut(p) {
            self.lifted_into_aggs(Arc::make_mut(c));
        }
        let Plan::Agg { input, groups, aggs, .. } = p else {
            return;
        };
        if !self.lifted.borrow().contains(&(Arc::as_ptr(input) as usize)) {
            return;
        }
        let Plan::Project { input: inner, exprs } = &**input else {
            return;
        };
        let mut grouped: Vec<usize> = Vec::new();
        for g in groups.iter() {
            grouped.extend(g.columns());
        }
        let mut exprs2 = exprs.clone();
        let mut repl: Vec<Option<Expr>> = vec![None; exprs.len()];
        for (i, e) in exprs.iter().enumerate() {
            if matches!(e, Expr::Col(..)) || grouped.contains(&i) {
                continue;
            }
            // The expression over the projection's row: its input columns, added when
            // the projection doesn't return them.
            let over = e
                .clone()
                .transform(&mut |x| {
                    Ok(match x {
                        Expr::Col(k, t) => {
                            let at = match exprs2.iter().position(|y| matches!(y, Expr::Col(j, _) if *j == k)) {
                                Some(at) => at,
                                None => {
                                    exprs2.push(Expr::Col(k, t.clone()));
                                    exprs2.len() - 1
                                }
                            };
                            Expr::Col(at, t)
                        }
                        x => x,
                    })
                })
                .unwrap();
            repl[i] = Some(over);
        }
        if repl.iter().all(Option::is_none) {
            return;
        }
        let sub = |e: &Expr| -> Expr {
            e.clone()
                .transform(&mut |x| {
                    Ok(match x {
                        Expr::Col(i, t) => match repl.get(i).cloned().flatten() {
                            Some(r) => r,
                            None => Expr::Col(i, t),
                        },
                        x => x,
                    })
                })
                .unwrap()
        };
        for a in aggs.iter_mut() {
            a.args = a.args.iter().map(&sub).collect();
            a.filter = a.filter.as_ref().map(sub);
            a.direct = a.direct.iter().map(&sub).collect();
            for o in a.order_by.iter_mut() {
                o.expr = sub(&o.expr);
            }
        }
        // (What only the aggregates read isn't computed below any more.)
        let mut read = vec![false; exprs2.len()];
        for g in groups.iter() {
            mark(&mut read, g);
        }
        for a in aggs.iter() {
            for e in a.args.iter().chain(&a.direct).chain(&a.filter).chain(a.order_by.iter().map(|o| &o.expr)) {
                mark(&mut read, e);
            }
        }
        for (i, r) in repl.iter().enumerate() {
            if r.is_some() && !read[i] {
                exprs2[i] = Expr::Const(Value::Null, exprs2[i].ty());
            }
        }
        *input = Arc::new(Plan::Project {
            input: inner.clone(),
            exprs: exprs2,
        });
    }

    /// `plan_joins_with` without lifting.
    fn plan_joins_with_unlifted(
        &self,
        p: &mut Plan,
        needed: &[bool],
        order: Option<&Order>,
        sort_exprs: &[Expr],
    ) -> Option<bool> {
        let saved = self.lifting.replace(p as *const Plan as usize);
        let r = self.plan_joins_with(p, needed, order, sort_exprs);
        self.lifting.set(saved);
        r
    }

    fn plan_joins_ordered(&self, p: &mut Plan, needed: &[bool], order: Option<&Order>) -> Option<bool> {
        self.plan_joins_with(p, needed, order, &[])
    }

    /// Plan the join group at `p`; with an ORDER BY over it, whether the rows come out in
    /// that order (None: not a group the search takes on).
    fn plan_joins_with(
        &self,
        p: &mut Plan,
        needed: &[bool],
        order: Option<&Order>,
        sort_exprs: &[Expr],
    ) -> Option<bool> {
        let Plan::Join { params, .. } = &*p else {
            return None;
        };
        // A subquery's computed outputs that need no PlaceHolderVar are evaluated above
        // the joins: the group planned over its tables (`lift`).
        if self.lifting.get() != p as *const Plan as usize
            && !self.lifted.borrow().contains(&(p as *const Plan as usize))
            && let Some(l) = lift::lift_group(p)
        {
            return self.plan_lifted(p, l, needed, order, sort_exprs);
        }
        // (A LATERAL join at the top: only as part of a group with outer joins.)
        let lateral_top = !params.is_empty();
        let mut kind = match &*p {
            Plan::Join { kind, .. } => *kind,
            _ => unreachable!(),
        };
        remember_equalities(p, self.ctx);
        let mut raw_leaves = Vec::new();
        let mut raw_clauses = Vec::new();
        // A tree with outer joins inside is one group with the joins' constraints
        // (`outerjoins`); one outer join of two items, or inner joins alone, the plain way.
        let extra = self.absorbed.take();
        let mut laterals: Vec<(usize, Vec<Expr>)> = Vec::new();
        let (mut sjs, mut clause_meta, general) = match outerjoins::gather_group(p, &extra) {
            // (Or LATERAL items among more than two inner-joined ones: the join order
            // the references allow.)
            Some(g)
                if !g.sjs.is_empty() && !(g.sjs.len() == 1 && g.leaves.len() == 2)
                    || g.sjs.is_empty() && !g.laterals.is_empty() && g.leaves.len() > 2 =>
            {
                raw_leaves = g.leaves;
                raw_clauses = g.clauses;
                laterals = g.laterals;
                kind = JoinKind::Inner;
                (g.sjs, g.meta, true)
            }
            _ => {
                if !extra.is_empty() || lateral_top {
                    return None;
                }
                gather(p, 0, &mut raw_leaves, &mut raw_clauses, true);
                // (MERGE: the target is the first relation, the source the second; equal
                // inner or full join paths keep the target outside.)
                // (Its RIGHT JOIN is the LEFT JOIN the other way round: the preserved
                // target first.)
                if matches!(kind, JoinKind::Inner | JoinKind::Full | JoinKind::Right)
                    && let Some(t) = self.merge_target.take()
                    && let [_, (Plan::Scan(ts), _)] = &raw_leaves[..]
                    && ts.table.oid == t
                {
                    raw_leaves.swap(0, 1);
                    if kind == JoinKind::Right {
                        kind = JoinKind::Left;
                    }
                    // (An inner join's equalities come from equivalence classes, built
                    // for the relations in that order: the target's operand first.)
                    if kind == JoinKind::Inner {
                        let (toff, tw) = (raw_leaves[0].1, crate::sql::binder::plan_width(&raw_leaves[0].0));
                        let on_target = |e: &Expr| {
                            let c = e.columns();
                            !c.is_empty() && c.iter().all(|&i| i >= toff && i < toff + tw)
                        };
                        for (c, _) in raw_clauses.iter_mut() {
                            if let Expr::Call(d, args, ty) = c
                                && d.name == "="
                                && args.len() == 2
                                && args[0].ty().without_typmod() == args[1].ty().without_typmod()
                                && !on_target(&args[0])
                                && on_target(&args[1])
                                && !args[0].columns().is_empty()
                            {
                                *c = Expr::Call(d.clone(), vec![args[1].clone(), args[0].clone()], ty.clone());
                            }
                        }
                    }
                }
                (Vec::new(), Vec::new(), false)
            }
        };
        if raw_leaves.len() < 2 || raw_leaves.len() > COLLAPSE_LIMIT {
            return None;
        }
        // The group's output width, and its row's (a semi / anti join's right side
        // included).
        let width = crate::sql::binder::plan_width(p);
        let gw = raw_leaves
            .iter()
            .map(|(plan, off)| off + crate::sql::binder::plan_width(plan))
            .max()
            .unwrap_or(width)
            .max(width);
        // Self-join elimination: a leaf merged into another reads its columns
        // (`alias`).
        let mut alias: Vec<usize> = (0..gw).collect();
        if laterals.is_empty() {
            let mut sides: Vec<(u32, u32)> = sjs.iter().map(|s| (s.syn_l, s.syn_r)).collect();
            let merges = self.self_joins(
                &mut kind,
                &mut raw_leaves,
                &mut raw_clauses,
                &mut alias,
                &mut sides,
                &mut clause_meta,
            );
            for (r, k) in merges {
                let remap = |m: u32| {
                    let m = if m >> r & 1 == 1 { (m | 1 << k) & !(1 << r) } else { m };
                    (m & ((1u32 << r) - 1)) | ((m >> (r + 1)) << r)
                };
                for sj in sjs.iter_mut() {
                    sj.syn_l = remap(sj.syn_l);
                    sj.syn_r = remap(sj.syn_r);
                    sj.min_l = remap(sj.min_l);
                    sj.min_r = remap(sj.min_r);
                }
            }
        }
        let mut needed: Vec<bool> = needed.to_vec();
        needed.resize(gw, false);
        // (The values LATERAL items read stay in the rows up to their joins.)
        for (_, ps) in &laterals {
            for e in ps {
                for g in e.columns() {
                    if g < gw {
                        needed[g] = true;
                    }
                }
            }
        }
        if let Plan::Join { meta, .. } = &*p {
            for &c in &meta.kept {
                if c < width {
                    needed[c] = true;
                }
            }
        }
        for g in 0..gw {
            if alias[g] != g && needed[g] {
                needed[g] = false;
                needed[alias[g]] = true;
            }
        }
        if raw_leaves.len() == 1 {
            return self.single_leaf(p, raw_leaves, raw_clauses, &needed, &alias, width);
        }
        let needed = &needed[..];
        let mut leaf_of = vec![0usize; gw];
        let mut leaves = Vec::new();
        for (i, (plan, off)) in raw_leaves.iter().enumerate() {
            let n = crate::sql::binder::plan_width(plan);
            for x in leaf_of.iter_mut().take((*off + n).min(gw)).skip(*off) {
                *x = i;
            }
            leaves.push((plan.clone(), *off, n));
        }
        for g in 0..gw {
            if alias[g] != g {
                leaf_of[g] = leaf_of[alias[g]];
            }
        }
        let mut s = Search {
            pl: self,
            g: JoinGucs::read(self.ctx),
            leaves: Vec::new(),
            clauses: Vec::new(),
            leaf_of,
            needed: needed.to_vec(),
            ec: (0..gw).collect(),
            kind,
            rels: Vec::new(),
            startup: order.is_some_and(|o| matches!(o.limit, Some((c, _)) if c != 0.0)),
            order: order.map(|o| Order {
                keys: o.keys.clone(),
                complete: o.complete,
                limit: o.limit,
                width: o.width,
                hint: o.hint,
            }),
            sjs,
            general,
            extra: if general { extra } else { Vec::new() },
            post: self.post_cost.take(),
            cur_sj: None,
            param_stack: Default::default(),
            loop_rows: self.pw_loop_rows.take(),
        };
        s.needed.resize(gw, false);
        for (ci, (e, on)) in raw_clauses.into_iter().enumerate() {
            let leaves_mask = s.leaves_of(&e);
            let eq = match &e {
                // (Under a nondeterministic collation equal strings needn't hash or sort
                // alike: a hash key of their sort keys, no merge key.)
                // (A SubPlan in it is no volatile function: a hash key it can be.)
                Expr::Call(d, args, _)
                    if d.name == "="
                        && args.len() == 2
                        && !crate::sql::explain::expr_volatile_deep(&e) =>
                {
                    let (al, bl) = (s.leaves_of(&args[0]), s.leaves_of(&args[1]));
                    (al != 0 && bl != 0 && al & bl == 0)
                        .then(|| (args[0].clone(), args[1].clone(), al, bl))
                }
                _ => None,
            };
            // (Under a nondeterministic collation equal strings needn't be equal bytes.)
            let nondet = matches!(&e, Expr::Call(d, ..)
                if d.coll.as_ref().is_some_and(|c| !c.deterministic));
            let (hashable, mergeable_) = match &eq {
                Some((a, b, _, _)) if nondet => {
                    let (ta, tb) = (a.ty(), b.ty());
                    (ta.without_typmod() == tb.without_typmod() && crate::sql::binder::optimize::hashable_type(&ta), false)
                }
                Some((a, b, _, _)) => {
                    let (ta, tb) = (a.ty(), b.ty());
                    // (integer_ops: int2, int4 and int8 hash and sort alike.)
                    let int = |t: &DataType| matches!(t, DataType::Int2 | DataType::Int4 | DataType::Int8);
                    let same = ta.without_typmod() == tb.without_typmod() || (int(&ta) && int(&tb));
                    // (Merging needs the sides' orders: plain columns here, or a column and
                    // an expression of the other side's columns, sorted by it: a pathkey of
                    // the column's class.)
                    (
                        same && crate::sql::binder::optimize::hashable_type(&ta),
                        same && mergeable(&ta)
                            && ((key_col(a).is_some() && key_col(b).is_some())
                                || (key_col(a).is_some() && merge_expr(b))
                                || (key_col(b).is_some() && merge_expr(a))),
                    )
                }
                None => (false, false),
            };
            let cost = costs::qual_cost(&e);
            s.clauses.push(Clause {
                expr: e,
                leaves: leaves_mask,
                eq,
                hashable,
                mergeable: mergeable_,
                cost,
                on,
                derived: false,
                sj: None,
                req: leaves_mask,
                nullable: 0,
                sel_div: Vec::new(),
            });
            if let Some(&(sj, req, nullable)) = clause_meta.get(ci) {
                let c = s.clauses.last_mut().unwrap();
                c.sj = sj;
                c.req = req;
                c.nullable = nullable;
            }
        }
        // Equivalence classes of equated columns (with outer joins inside, of the inner
        // joins' clauses only; an outer join's own clause equates nothing: each side is
        // a class of its own).
        let outer_join = !matches!(s.kind, JoinKind::Inner | JoinKind::Semi);
        for c in &s.clauses {
            if !c.mergeable
                || (s.general && (c.sj.is_some() || c.req != c.leaves))
                || (!s.general && outer_join && c.on)
            {
                continue;
            }
            let Some((a, b, _, _)) = &c.eq else { continue };
            if let (Some(x), Some(y)) = (key_col(a), key_col(b)) {
                let (ex, ey) = (s.ec[x], s.ec[y]);
                if ex != ey {
                    for e in s.ec.iter_mut() {
                        if *e == ey {
                            *e = ex;
                        }
                    }
                }
            }
        }
        // A FROM item listing one of its values twice (a sub-select's `x, x`): one member
        // of the class those columns are in (one Var in Postgres).
        let mut same_value: Vec<(usize, usize)> = Vec::new();
        for (plan, off, _) in &leaves {
            if let Plan::Project { exprs, .. } = plan {
                for i in 0..exprs.len() {
                    for j in i + 1..exprs.len() {
                        if matches!(exprs[i], Expr::Col(..))
                            && exprs[i].same_as(&exprs[j])
                            && off + j < s.ec.len()
                            && s.ec[off + i] != s.ec[off + j]
                            && (s.ec.iter().filter(|&&e| e == s.ec[off + i]).count() > 1
                                || s.ec.iter().filter(|&&e| e == s.ec[off + j]).count() > 1)
                        {
                            let (ex, ey) = (s.ec[off + i], s.ec[off + j]);
                            for e in s.ec.iter_mut() {
                                if *e == ey {
                                    *e = ex;
                                }
                            }
                            same_value.push((off + i, off + j));
                        }
                    }
                }
            }
        }
        // generate_base_implied_equalities: members of a class within one FROM item are
        // equal there, a restriction of it (each to the one before); the joins need just
        // one equality per class (`crossing`).
        let mut leaves = leaves;
        if matches!(s.kind, JoinKind::Inner | JoinKind::Semi) && !s.general {
            let col_ty = |g: usize| {
                s.clauses.iter().find_map(|c| {
                    let (a, b, _, _) = c.eq.as_ref()?;
                    [a, b].into_iter().find(|x| key_col(x) == Some(g)).map(|x| x.ty())
                })
            };
            let mut prev: Vec<(usize, usize, usize)> = Vec::new();
            for g in s.ec_members() {
                let (l, e) = (s.leaf_of[g], s.ec[g]);
                let Some(slot) = prev.iter().position(|(pe, pl, _)| *pe == e && *pl == l) else {
                    prev.push((e, l, g));
                    continue;
                };
                let pg = prev[slot].2;
                prev[slot].2 = g;
                if same_value.contains(&(pg, g)) || same_value.contains(&(g, pg)) {
                    continue;
                }
                let (Some(tp), Some(tg)) = (col_ty(pg), col_ty(g)) else { continue };
                let template = s.clauses.iter().find(|c| {
                    s.clause_ec(c) == Some(e)
                        && c.eq.as_ref().is_some_and(|(a, b, _, _)| {
                            a.ty().without_typmod() == tp.without_typmod()
                                && b.ty().without_typmod() == tg.without_typmod()
                        })
                });
                let Some(Clause {
                    expr: Expr::Call(def, _, rt),
                    ..
                }) = template
                else {
                    continue;
                };
                let off = leaves[l].1;
                let q = Expr::Call(
                    def.clone(),
                    vec![Expr::Col(pg - off, tp), Expr::Col(g - off, tg)],
                    rt.clone(),
                );
                // (Unless the item has it already: a partition join's partitions, their
                // members too.)
                let has = has_qual(&leaves[l].0, &q);
                if !has {
                    let plan = std::mem::replace(&mut leaves[l].0, Plan::One);
                    leaves[l].0 = crate::sql::binder::optimize::add_filter(&self.cat, plan, q);
                }
            }
        }
        // Each leaf: planned for itself, then sized.
        for (i, (mut plan, off, n)) in leaves.into_iter().enumerate() {
            let set = 1u32 << i;
            let mut lneed = vec![false; n];
            for (k, x) in lneed.iter_mut().enumerate() {
                *x = s.needed_above(off + k, set);
            }
            // extract_restriction_or_clauses: a join OR clause whose every arm says
            // something about this item alone restricts it too, when that rejects enough
            // of its rows (consider_new_or_clause); the clause's selectivity makes up for
            // it.
            if (s.general || s.kind == JoinKind::Inner)
                && matches!(&plan, Plan::Scan(sp) if plannable(sp))
            {
                for (ci, q) in s.or_restrictions(i) {
                    let local = q.shift_cols(-(off as isize));
                    let Plan::Scan(sp) = &plan else { break };
                    let Some(r) = self.rel(sp, &lneed) else { break };
                    let or_sel = self.sel(&r, std::slice::from_ref(&local));
                    if or_sel > 0.9 {
                        continue;
                    }
                    plan = crate::sql::binder::optimize::add_filter(&self.cat, plan, local);
                    if or_sel > 0.0 {
                        s.clauses[ci].sel_div.push(or_sel);
                    }
                }
            }
            // A pulled-up subquery's scan, planned for itself before: the join's paths are
            // chosen again (parameterized ones too), as for any base relation.
            if let Plan::Scan(sp) = &mut plan
                && matches!(sp.access, Access::Seq | Access::Bitmap(..) | Access::Index { ordered: false, .. })
                && sp.sample.is_none()
                && sp.current_of.is_none()
                && sp.table.partkey.is_none()
            {
                sp.access = Access::Unplanned;
                sp.index = None;
                sp.reverse = false;
            }
            let mut proj = None;
            let scan = match &plan {
                Plan::Scan(sp) if plannable(sp) => {
                    let sp = (**sp).clone();
                    self.rel(&sp, &lneed).map(|r| (sp, r))
                }
                p => match virtual_proj(p) {
                    Some((sp, exprs)) if plannable(sp) => {
                        let sp = sp.clone();
                        let r = self.rel(&sp, &lneed).map(|r| (sp, r));
                        if r.is_some() {
                            proj = Some(exprs.clone());
                        }
                        r
                    }
                    _ => None,
                },
            };
            if scan.is_none() {
                self.walk(&mut plan, &lneed, None);
            }
            let members = match (&scan, &plan) {
                (None, Plan::Append { inputs, .. }) => self.members(inputs, &lneed),
                _ => None,
            };
            let est = match &scan {
                Some(_) => crate::sql::explain::PlanEstimate {
                    rows: 0.0,
                    startup: 0.0,
                    total: 0.0,
                    width: 0,
                    node_type: "Seq Scan",
                    presorted: 0,
                    disabled: 0,
                    cols: Vec::new(),
                },
                None => crate::sql::explain::estimate(&Arc::new(plan.clone()), self.ctx),
            };
            let (rows, cols) = match &scan {
                Some((sp, r)) => {
                    let cols = (0..n)
                        .map(|k| {
                            if k < sp.table.columns.len() {
                                (
                                    if sp.table.columns[k].dropped {
                                        0
                                    } else {
                                        costs::col_width(&self.cat, &sp.table, k)
                                    },
                                    Some(VarRef {
                                        table: sp.table.clone(),
                                        col: k,
                                        tuples: r.size.tuples,
                                        expr: None,
                                        rows: r.rows,
                                        inh: false,
                                    }),
                                    None,
                                )
                            } else {
                                // The row's ctid (the storage key, or the first system
                                // column) is unique: as many values as tuples.
                                let ncols = sp.table.columns.len();
                                let ctid = (sp.with_key && k == ncols) || (sp.sys && k == ncols + sp.with_key as usize);
                                // (tableoid: one value per table, get_variable_numdistinct.)
                                let tableoid = sp.sys && k == ncols + sp.with_key as usize + 5;
                                let d = if ctid {
                                    Some(r.size.tuples.max(1.0))
                                } else {
                                    tableoid.then_some(1.0)
                                };
                                (6, None, d)
                            }
                        })
                        .collect();
                    (r.rows, cols)
                }
                None => (est.rows, est.cols.clone()),
            };
            // A partitioned table's Append: its columns are the parent's (whose unique
            // indexes the planner knows), over the members' tuples.
            let mut cols = cols;
            // (A system catalog's columns: its stand-in table's, whose unique indexes are
            // the catalog's.)
            if let Plan::SysScan { table, .. } = &plan {
                let t = super::sys_table(*table);
                let tuples = crate::sql::explain::sys_tuples(*table, self.ctx);
                for (k, c) in cols.iter_mut().enumerate() {
                    if c.1.is_none() && k < t.columns.len() {
                        c.1 = Some(VarRef {
                            table: t.clone(),
                            col: k,
                            tuples,
                            expr: None,
                            rows,
                            inh: false,
                        });
                    }
                }
            }
            // (An inheritance tree's tableoid: one value per table all the same.)
            if let Some(ms) = &members {
                let sp = &ms[0].scan;
                let mk = sp.table.columns.len() + sp.with_key as usize + 5;
                if sp.sys
                    && let Some(k) = ms[0].map.iter().position(|m| *m == Some(mk))
                    && let Some(c) = cols.get_mut(k)
                    && c.1.is_none()
                {
                    c.2 = Some(1.0);
                }
            }
            if let Some(ms) = &members
                && let Some(parent) = ms[0].scan.part.as_ref().and_then(|p| p.last())
            {
                let parent = &parent.parent;
                let tuples: f64 = ms.iter().map(|m| m.rel.size.tuples).sum();
                for (k, c) in cols.iter_mut().enumerate() {
                    if k < parent.columns.len() && !parent.columns[k].dropped {
                        c.1 = Some(VarRef {
                            table: parent.clone(),
                            col: k,
                            tuples,
                            expr: None,
                            rows,
                            inh: true,
                        });
                    }
                }
            }
            let kind = match est.node_type {
                "Function Scan" => InKind::FunctionScan,
                "CTE Scan" => InKind::CteScan,
                "WorkTable Scan" => InKind::WorkTableScan,
                "Materialize" => InKind::Material,
                "Sort" => InKind::Sort,
                // (A subquery's scan, VALUES, an Append of several: no mark/restore.)
                _ => InKind::NoMark,
            };
            s.leaves.push(Leaf {
                plan,
                off,
                ncols: n,
                scan,
                rows,
                startup: est.startup,
                total: est.total,
                kind,
                cols,
                members,
                lateral: laterals
                    .iter()
                    .find(|(li, _)| *li == i)
                    .map(|(_, ps)| (ps.clone(), ps.iter().fold(0u32, |m, e| m | s.leaves_of(e)))),
                proj,
            });
        }
        // generate_join_implied_equalities: in a group of inner joins, each pair of an
        // equivalence class's columns from different FROM items can be joined on.
        if s.kind == JoinKind::Inner && !s.general && s.leaves.len() >= 3 {
            s.derive_equalities();
        }
        if s.kind == JoinKind::Inner && !s.general {
            s.orient_ec_clauses();
        }
        // reduce_unique_semijoins: a semi join whose right side has at most one match
        // per row is an inner join.
        if s.kind == JoinKind::Semi && s.leaves.len() == 2 {
            let all: Vec<usize> = (0..s.clauses.len()).collect();
            if s.inner_unique(2, &all) {
                s.kind = JoinKind::Inner;
            }
        }
        if s.order.is_some() {
            // The width the sort sees: its projection's, else the whole row's.
            let w: u32 = if sort_exprs.is_empty() {
                (0..width).map(|g| s.col_width(g)).sum()
            } else {
                sort_exprs
                    .iter()
                    .map(|e| match e {
                        Expr::Col(g, _) if *g < width => s.col_width(*g),
                        e => costs::type_width(&e.ty()),
                    })
                    .sum()
            };
            if let Some(o) = &mut s.order {
                o.width = w;
            }
        }
        let (best, sorted) = s.run()?;
        // Partitionwise join, when it is cheaper. (Under a LIMIT without an order: its
        // child joins planned for the rows it reads, the two compared for them.)
        let lim = s.order.as_ref().filter(|o| o.keys.is_empty() && o.complete && !o.hint).and_then(|o| match o.limit {
            Some((c, off)) if c > 0.0 && off >= 0.0 => Some((c, off)),
            _ => None,
        });
        let saved_lim = self.pw_limit.replace(lim);
        let ord = s.order.as_ref().filter(|o| !o.keys.is_empty() && o.complete && !o.hint).cloned();
        let saved_ord = self.pw_order.replace(ord.clone());
        let pwj = if alias.iter().enumerate().all(|(i, &g)| i == g) {
            self.partitionwise(p, &s, &needed[..width.min(needed.len())])
        } else {
            None
        };
        self.pw_limit.set(saved_lim);
        self.pw_order.replace(saved_ord);
        // (A Merge Append of child joins in the ORDER BY's order, when it reads the rows
        // the query wants cheaper than the group's best path: generate_orderedappend_paths.)
        if let Some((_, _, _, pw_join, Some(ordered))) = &pwj
            && let Some(o) = &ord
            && ordered.len() > 1
        {
            let ests: Vec<_> = ordered.iter().map(|c| crate::sql::explain::estimate(&Arc::new(c.clone()), self.ctx)).collect();
            let n = ests.len() as f64;
            let cmp = 2.0 * CPU_OPERATOR_COST;
            let logn = n.log2();
            let rows: f64 = ests.iter().map(|e| e.rows).sum();
            let st = ests.iter().map(|e| e.startup).sum::<f64>() + cmp * n * logn;
            let tot = st + ests.iter().map(|e| e.total - e.startup).sum::<f64>() + rows * cmp * logn + CPU_TUPLE_COST * 0.5 * rows;
            let frac = |st: f64, tot: f64, rows: f64| match o.limit {
                Some((c, off)) if c > 0.0 && off >= 0.0 => {
                    let k = (c + off).min(rows.max(1.0));
                    st + (tot - st) * k / rows.max(1.0)
                }
                _ => tot,
            };
            if frac(st, tot, rows) < frac(best.startup, best.total, best.rows) {
                let mut inputs = Vec::new();
                for c in ordered.iter() {
                    let mut c = c.clone();
                    let mut x = &mut c;
                    loop {
                        match x {
                            Plan::Project { input, .. } | Plan::Filter { input, .. } => x = Arc::make_mut(input),
                            Plan::Join { meta, .. } => {
                                meta.presorted = true;
                                break;
                            }
                            _ => break,
                        }
                    }
                    inputs.push(Arc::new(c));
                }
                let keys = o
                    .keys
                    .iter()
                    .map(|&(g, desc, nulls_first)| SortKey {
                        expr: Expr::Col(g, s.col_type(g)),
                        desc,
                        nulls_first,
                        using: None,
                    })
                    .collect();
                self.pw_joins.borrow_mut().push((Arc::as_ptr(&inputs[0]) as usize, pw_join.clone()));
                *p = Plan::Sort {
                    input: Arc::new(Plan::Append { inputs, inh: None }),
                    keys,
                };
                return Some(true);
            }
        }
        let pwj = pwj.map(|(a, b, c, d, _)| (a, b, c, d));
        if let Some((children, pw_total, pw_rows, pw_join)) = pwj {
            let pw = match &s.order {
                Some(o) if sorted => {
                    let limit = match o.limit {
                        Some((c, off)) if c > 0.0 && off >= 0.0 => Some(c + off),
                        _ => None,
                    };
                    costs::sort_cost(pw_total, pw_rows, o.width, limit).1
                }
                _ => pw_total,
            };
            let frac = |st: f64, tot: f64, rows: f64, (c, off): (f64, f64)| {
                let k = (c + off).min(rows.max(1.0));
                st + (tot - st) * k / rows.max(1.0)
            };
            let cheaper = match lim {
                Some(l) => {
                    let st = children
                        .first()
                        .map_or(0.0, |c| crate::sql::explain::estimate(&Arc::new(c.clone()), self.ctx).startup);
                    frac(st, pw_total, pw_rows, l) < frac(best.startup, best.total, best.rows, l)
                }
                None => pw < best.total,
            };
            if cheaper || self.pw_agg_join.get() {
                // (One child join needs no Append: Postgres removes it.)
                *p = if children.len() == 1 {
                    children.into_iter().next().unwrap()
                } else {
                    let inputs: Vec<Arc<Plan>> = children.into_iter().map(Arc::new).collect();
                    // (A join above may join this one partitionwise in turn.)
                    self.pw_joins
                        .borrow_mut()
                        .push((Arc::as_ptr(&inputs[0]) as usize, pw_join));
                    Plan::Append { inputs, inh: None }
                };
                return Some(false);
            }
        }
        let (mut plan, layout) = s.build(&best);
        // The columns of equalities turned into constants stay needed up to the join.
        let kept: Vec<usize> = match &*p {
            Plan::Join { meta, .. } => meta
                .kept
                .iter()
                .filter_map(|c| layout.iter().position(|g| g == c))
                .collect(),
            _ => Vec::new(),
        };
        if let Plan::Join { meta, .. } = &mut plan {
            for k in kept {
                if !meta.kept.contains(&k) {
                    meta.kept.push(k);
                }
            }
        }
        // Back to the group's column order.
        let w = layout.len();
        let identity = layout.iter().enumerate().all(|(i, &g)| i == g)
            && w == width
            && alias.iter().enumerate().all(|(i, &g)| i == g);
        let out = if identity {
            plan
        } else {
            let exprs = (0..width)
                .map(|g| {
                    let g = alias[g];
                    let ty = match &s.leaves[s.leaf_of[g]].scan {
                        Some((sp, _)) if g - s.leaves[s.leaf_of[g]].off < sp.table.columns.len() => {
                            sp.table.columns[g - s.leaves[s.leaf_of[g]].off].ty.clone()
                        }
                        _ => DataType::Unknown,
                    };
                    match layout.iter().position(|&x| x == g) {
                        Some(i) => Expr::Col(i, ty),
                        None => Expr::Const(Value::Null, ty),
                    }
                })
                .collect();
            Plan::Project {
                input: Arc::new(plan),
                exprs,
            }
        };
        *p = out;
        Some(sorted)
    }
}

/// A LATERAL item that is a join Postgres would have pulled up into the query: a join
/// relation has no lateral_vars to cache it by (get_memoize_path).
pub(super) fn lateral_join_leaf(p: &Plan) -> bool {
    let mut q: &Plan = p;
    while let Plan::Project { input, exprs } = q
        && !exprs.iter().any(Expr::is_volatile)
    {
        q = input;
    }
    while let Plan::Filter { input, .. } = q {
        q = input;
    }
    matches!(q, Plan::Join { .. })
}
