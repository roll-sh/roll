//! Access paths for table scans, chosen by cost the way Postgres's planner does
//! (allpaths.c's set_plain_rel_pathlist, indxpath.c, costsize.c, selfuncs.c's
//! btcostestimate): a sequential scan, index scans (plain or index-only, forward or
//! backward) and bitmap heap scans over BitmapAnd / BitmapOr trees, kept with add_path's
//! fuzzy dominance rules; an ORDER BY (and LIMIT) above a scan picks between sorting the
//! cheapest path and reading an index in that order.
//!
//! The choice is recorded in the scan (`ScanPlan::access`); EXPLAIN recomputes the same
//! estimates for it with `describe`.

use crate::catalog::{Index, Table};
use crate::exec::ExecCtx;
use crate::sql::binder::optimize::{conjuncts, probe_ok, pseudo_constant};
use crate::sql::costs::{self, CPU_OPERATOR_COST, CPU_TUPLE_COST, Est, RelSize, VarRef};
use crate::sql::expr::Expr;
use crate::sql::func::FuncDef;
use crate::sql::plan::{
    Access, BitmapQual, IndexBounds, IndexQual, JoinKind, Plan, ScanPlan, SortKey,
};
use crate::types::{DataType, Value};
use std::cmp::Ordering;
use std::sync::Arc;

pub(crate) mod analyzejoins;
mod reducejoins;
pub(crate) use reducejoins::strict_cols;
pub(crate) use joinpath::user_eq_props;
pub(crate) use joinpath::lateral_pullable;
mod pullup;
mod pathkeys;
pub use pathkeys::{column_classes, constant_columns};
mod joinpath;
pub mod parallel;
mod unused;
/// contain_volatile_functions over a plan (a sub-select counting by what it runs).
pub(crate) fn plan_volatile_deep(p: &Plan) -> bool {
    unused::plan_volatile(p)
}

/// contain_volatile_functions over an expression (a sub-select by what it runs).
pub(crate) fn expr_volatile_deep(e: &Expr) -> bool {
    unused::volatile(e)
}
mod mergeappend;
pub use mergeappend::{PartOrder, count_members, partition_order, plan_ordered_append};
mod rowmarks;
pub use rowmarks::level_scans;
pub(crate) use rowmarks::{OWN_LOCK, PUSHED_LEVEL};
mod sublinkcopy;
pub(crate) use sublinkcopy::make_copy as make_subquery_copy;
mod syscat;
pub use syscat::{sys_index_order, sys_size, sys_table};
pub(crate) use syscat::sys_unique_col;

pub const CPU_INDEX_TUPLE_COST: f64 = 0.005;
/// DEFAULT_PAGE_CPU_MULTIPLIER.
const PAGE_CPU_MULTIPLIER: f64 = 50.0;
/// STD_FUZZ_FACTOR.
const FUZZ: f64 = 1.01;
/// DEFAULT_RANGE_INEQ_SEL.
const RANGE_INEQ_SEL: f64 = 0.005;

/// The planner settings that matter here.
#[derive(Clone, Debug)]
pub struct Gucs {
    pub seqscan: bool,
    pub indexscan: bool,
    pub indexonlyscan: bool,
    pub bitmapscan: bool,
    pub tidscan: bool,
    pub sort: bool,
    pub hashagg: bool,
    pub incremental_sort: bool,
    /// enable_distinct_reordering.
    pub distinct_reordering: bool,
    pub random_page_cost: f64,
    pub seq_page_cost: f64,
    /// effective_cache_size, in pages.
    pub cache_pages: f64,
    /// work_mem, in kB.
    pub work_mem: f64,
    pub hash_mem_multiplier: f64,
}

impl Gucs {
    pub fn read(ctx: &ExecCtx) -> Gucs {
        let s = ctx.sess.settings.borrow();
        let on = |n: &str| s.get_str(n).is_none_or(|v| v != "off" && v != "false");
        let num = |n: &str, d: f64| {
            s.get_str(n)
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(d)
        };
        Gucs {
            seqscan: on("enable_seqscan"),
            indexscan: on("enable_indexscan"),
            indexonlyscan: on("enable_indexonlyscan"),
            bitmapscan: on("enable_bitmapscan"),
            tidscan: on("enable_tidscan"),
            sort: on("enable_sort"),
            hashagg: on("enable_hashagg"),
            incremental_sort: on("enable_incremental_sort"),
            distinct_reordering: on("enable_distinct_reordering"),
            random_page_cost: num("random_page_cost", 4.0),
            seq_page_cost: num("seq_page_cost", 1.0),
            cache_pages: num("effective_cache_size", 524288.0),
            work_mem: num("work_mem", 4096.0),
            hash_mem_multiplier: num("hash_mem_multiplier", 2.0),
        }
    }

    /// cost_agg's disk costs of a hashed aggregation expected to spill
    /// (hash_agg_entry_size, hash_agg_set_limits, hash_choose_num_partitions): the
    /// (startup, total) cost to add.
    pub fn hashagg_spill(
        &self,
        input_rows: f64,
        input_width: u32,
        groups: f64,
        num_trans: usize,
        trans_space: f64,
    ) -> (f64, f64) {
        const BLCKSZ: f64 = 8192.0;
        let maxalign = |x: f64| (x / 8.0).ceil() * 8.0;
        // TupleHashEntryData; the minimal tuple (its header MAXALIGNed); the per-group
        // states; the transition values' AllocSet chunks.
        let trans_chunk = if trans_space > 0.0 {
            8.0 + trans_space.log2().ceil().exp2()
        } else {
            0.0
        };
        let entry = 16.0 + maxalign(16.0 + input_width as f64) + 16.0 * num_trans as f64 + trans_chunk;
        let hash_mem = self.work_mem * self.hash_mem_multiplier * 1024.0;
        let (mem_limit, ngroups_limit, partitions) = if groups * entry <= hash_mem {
            (hash_mem, (hash_mem / entry).floor(), 0.0)
        } else {
            let partition_limit = (hash_mem * 0.25 - BLCKSZ) / BLCKSZ;
            let wanted = 1.5 * groups * entry;
            let mut d = 1.0 + wanted / hash_mem;
            if d > partition_limit {
                d = partition_limit;
            }
            d = d.clamp(4.0, 1024.0);
            let n = d.floor();
            // A power of two: 1 << ceil(log2(n)).
            let parts = n.log2().ceil().exp2();
            let partition_mem = BLCKSZ + BLCKSZ * parts;
            let limit = if hash_mem > 4.0 * partition_mem {
                hash_mem - partition_mem
            } else {
                (hash_mem * 0.75).floor()
            };
            let ng = if limit > entry { (limit / entry).floor() } else { 1.0 };
            (limit, ng, parts)
        };
        let nbatches = ((groups * entry) / mem_limit).max(groups / ngroups_limit).ceil().max(1.0);
        let partitions = partitions.max(2.0);
        let depth = (nbatches.ln() / partitions.ln()).ceil();
        if depth <= 0.0 || !depth.is_finite() {
            return (0.0, 0.0);
        }
        let pages = costs::relation_byte_size(input_rows, input_width) / BLCKSZ;
        let written = pages * depth * 2.0;
        let read = pages * depth * 2.0;
        let spill = depth * input_rows * 2.0 * costs::CPU_TUPLE_COST;
        (
            written * self.random_page_cost + spill,
            written * self.random_page_cost + read * self.seq_page_cost + spill,
        )
    }
}

// ── the relation and its indexes ──

/// Which index access method an index uses (what it can do).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Am {
    Btree,
    Hash,
    Gist,
    Spgist,
    Gin,
    Brin,
}

impl Am {
    /// amgettuple: plain and index-only scans (else bitmap scans only).
    fn gettuple(self) -> bool {
        !matches!(self, Am::Gin | Am::Brin)
    }

    /// amoptionalkey: a scan without a condition on the first column.
    fn optionalkey(self) -> bool {
        self != Am::Hash
    }

    /// amsearchnulls: IS NULL / IS NOT NULL as index conditions.
    fn searchnulls(self) -> bool {
        matches!(self, Am::Btree | Am::Gist | Am::Spgist | Am::Brin)
    }

    /// The operators an index column supports are its operator family's (GiST, SP-GiST,
    /// GIN, BRIN).
    fn by_family(self) -> bool {
        matches!(self, Am::Gist | Am::Spgist | Am::Gin | Am::Brin)
    }
}

impl IndexInfo {
    /// Column `col` uses a *_pattern_ops operator class (bytewise ~<~ and friends).
    fn pattern_ops(&self, col: usize) -> bool {
        self.ix
            .columns
            .get(col)
            .is_some_and(|c| matches!(c.opclass, 4217..=4219))
    }

    /// A partial index's predicate implies restriction clause `q` (it is left out of the
    /// index's indrestrictinfo).
    /// (Not for a target relation of UPDATE / DELETE / row locking: its quals are rechecked
    /// (check_index_predicates' is_target_rel); nor for an index without amoptionalkey
    /// (hash), whose first key's clauses could all go.)
    fn implies(&self, q: &Expr) -> bool {
        self.am != Am::Hash && self.implies_plan(q)
    }

    /// The same, as create_indexscan_plan leaves the implied quals out of the scan's filter.
    fn implies_plan(&self, q: &Expr) -> bool {
        !self.target && !self.pred.is_empty() && implied_by(std::slice::from_ref(q), &self.pred)
    }
}

#[derive(Clone)]
struct IndexInfo {
    ix: Arc<Index>,
    am: Am,
    /// Key column expressions over the table row.
    keys: Vec<Expr>,
    /// Columns an index-only scan can return.
    canreturn: Vec<usize>,
    pred: Vec<Expr>,
    /// The relation is a result relation or row-marked (its quals all stay).
    target: bool,
    pages: f64,
    tuples: f64,
    /// btree height (-1: unknown).
    height: i32,
    /// Per key column with a made btree operator class ordering it: its comparison
    /// function, and its family's operators by strategy.
    order: Vec<Option<(u32, Vec<(String, &'static str)>)>>,
    /// Per key column of a GiST, SP-GiST, GIN or BRIN index: its operator family's
    /// operators (pg_amop: oid, purpose 's' search / 'o' ordering, sort family).
    fam_ops: Vec<Vec<(u32, char, u32)>>,
    /// The user-defined operators among `fam_ops` (an extension's operator class's), by
    /// their function: how a call of one is known as the operator.
    user_ops: Vec<UserOp>,
    /// Per key column: its collation (the index's COLLATE, else its column's; None: the
    /// default).
    colls: Vec<Option<String>>,
}

/// A user-defined operator of an index's operator family.
#[derive(Clone, Debug)]
struct UserOp {
    oid: u32,
    name: String,
    func: u32,
    /// Its commutator: the operator and a call target for it.
    com: Option<(u32, Arc<FuncDef>)>,
}

impl IndexInfo {
    /// The pg_operator oid of a binary operator call (0: none known): a built-in operator,
    /// or one of the family's user-defined operators (its function, by name as shown).
    fn op_oid(&self, def: &FuncDef) -> u32 {
        match call_operator(def) {
            0 => self.user_op(def).map_or(0, |u| u.oid),
            o => o,
        }
    }

    fn user_op(&self, def: &FuncDef) -> Option<&UserOp> {
        let shown = def.name.strip_prefix("OPERATOR(").and_then(|n| n.strip_suffix(')'));
        let shown = shown.map_or(def.name, |n| n.rsplit('.').next().unwrap_or(n));
        self.user_ops.iter().find(|u| u.func == def.oid && u.name == shown)
    }

    /// The commutator of an operator call: its oid and a call target for it.
    fn commutator(&self, def: &FuncDef) -> Option<(u32, Arc<FuncDef>)> {
        if let Some(u) = self.user_op(def) {
            return u.com.clone();
        }
        let com = crate::sql::func::oper_row(def.name, def).com;
        let info = crate::sql::func::operator_by_oid(com)?;
        let cdef = crate::sql::func::resolve_binop(info.name, info.left.as_ref()?, &info.right).ok()?.def;
        Some((com, cdef))
    }
}

/// The user-defined operators of an index's operator families (`fam_ops`).
fn user_ops(cat: &crate::catalog::Catalog, fam_ops: &[Vec<(u32, char, u32)>]) -> Vec<UserOp> {
    let target = |oid: u32| -> Option<Arc<FuncDef>> {
        let o = cat.operators.get(&oid)?;
        let f = cat.functions.get(&o.func)?;
        let mut d = crate::sql::binder::Binder::native_def(f)?;
        d.name = crate::sql::func::intern(&o.name);
        Some(Arc::new(d))
    };
    fam_ops
        .iter()
        .flatten()
        .filter_map(|(oid, _, _)| {
            let o = cat.operators.get(oid)?;
            let com = (o.commutator != 0).then(|| target(o.commutator).map(|d| (o.commutator, d))).flatten();
            Some(UserOp { oid: o.oid, name: o.name.clone(), func: o.func, com })
        })
        .collect()
}

/// A base relation as path generation sees it.
struct Rel {
    table: Arc<Table>,
    size: RelSize,
    allvisfrac: f64,
    /// Restriction clauses (conjuncts over the table row, gating ones left out).
    quals: Vec<Expr>,
    rows: f64,
    /// Every restriction clause's evaluation cost, per tuple, and once (a hashed
    /// SubPlan's table).
    qual_cost: f64,
    qual_startup: f64,
    /// Table columns needed above the scan; `other` when it also needs columns no index
    /// has (the row's key, system columns).
    needed: Vec<bool>,
    other: bool,
    indexes: Vec<IndexInfo>,
    /// The scans a parameterized path is repeated for (1: not parameterized).
    loop_count: f64,
    /// Index expressions, with their statistics.
    exprs: Vec<(Expr, VarRef)>,
    /// `=` for booleans (a boolean index matches `col` as `col = true`).
    booleq: Option<Arc<FuncDef>>,
    /// The restriction clauses in baserestrictinfo order (`ScanPlan::written`).
    written: Arc<Vec<Expr>>,
    /// all_rows_selectable, for functions that aren't leakproof: the whole table (its index
    /// expressions' statistics), and each column.
    acl: Arc<(bool, Vec<bool>)>,
    /// Index expressions with statistics only leakproof functions may see.
    hidden: Arc<Vec<(Expr, VarRef)>>,
    /// A parameterized path's parameters (`Outer(1, k)`): the outer expressions the nested
    /// loop passes in. Postgres passes only the outer Vars (nestloop params) and the scan
    /// evaluates the rest of a clause, so costs see the clause as written.
    params: Arc<Vec<Expr>>,
}

impl Rel {
    /// `e` with a parameterized path's parameters replaced by the outer expressions they
    /// stand for: the clause Postgres costs (cost_qual_eval of a ppi_clause, an index
    /// qual's other operand in index_other_operands_eval_cost), where only the outer
    /// relation's Vars are parameters (Vars and Params cost nothing, the rest of the
    /// expression is evaluated by the scan).
    fn unparam(&self, e: &Expr) -> Expr {
        if self.params.is_empty() {
            return e.clone();
        }
        e.clone()
            .transform(&mut |x| {
                Ok(match x {
                    Expr::Outer(1, k, t) => self.params.get(k).cloned().unwrap_or(Expr::Outer(1, k, t)),
                    x => x,
                })
            })
            .unwrap_or_else(|_| e.clone())
    }

    fn vars(&self) -> impl Fn(usize) -> Option<VarRef> + '_ {
        move |i| {
            (i < self.table.columns.len()).then(|| VarRef {
                table: self.table.clone(),
                col: i,
                tuples: self.size.tuples,
                expr: None,
                rows: 0.0,
                inh: false,
            })
        }
    }
}

/// Are there statistics objects on the table?
fn cat_has_statistics(cat: &crate::catalog::Catalog, table: u32) -> bool {
    cat.statistics.values().any(|s| s.table == table)
}

/// The relation's size as the planner sees it, and its all-visible fraction.
fn rel_size(t: &Table, ctx: &ExecCtx) -> (RelSize, f64) {
    // (A catalog's stand-in: all its rows visible.)
    if let Some(st) = syscat::stand_in_of(t) {
        return (syscat::sys_size(st, ctx), 1.0);
    }
    let size = costs::rel_size(t, ctx);
    let allvis = ctx
        .catalog()
        .relstats
        .get(&t.oid)
        .map_or(0.0, |s| s.allvisible as f64);
    let frac = if allvis <= 0.0 || size.pages <= 0.0 {
        0.0
    } else if allvis >= size.pages {
        1.0
    } else {
        allvis / size.pages
    };
    (size, frac)
}

fn index_infos(ctx: &ExecCtx, t: &Arc<Table>, rel_tuples: f64) -> Vec<IndexInfo> {
    if let Some(st) = syscat::stand_in_of(t) {
        return syscat::sys_index_infos(st, rel_tuples);
    }
    let Ok(bound) = crate::sql::binder::bound_indexes(t, ctx) else {
        return Vec::new();
    };
    let db = &ctx.sess.db;
    let cat = ctx.catalog();
    let mut out = Vec::new();
    for b in bound {
        let ix = b.index.clone();
        if !ix.valid {
            continue;
        }
        let am = match ix.method.as_str() {
            "btree" => Am::Btree,
            "hash" => Am::Hash,
            "gist" => Am::Gist,
            "spgist" => Am::Spgist,
            "gin" => Am::Gin,
            "brin" => Am::Brin,
            _ => continue,
        };
        let pages = crate::exec::sizing::fork_blocks(ix.oid, crate::exec::sizing::Fork::Main, ctx)
            .ok()
            .flatten()
            .unwrap_or(1) as f64;
        let mut pred = Vec::new();
        if let Some(p) = b.predicate.clone() {
            conjuncts(p, &mut pred);
        }
        let tuples = if pred.is_empty() {
            rel_tuples
        } else {
            // estimate_rel_size of the index: its density, the metapage discounted.
            let (rp, rt) = cat
                .relstats
                .get(&ix.oid)
                .map_or((0.0, -1.0), |s| (s.pages as f64, s.tuples));
            let (mut cur, mut relpages) = (pages, rp);
            if cur == 0.0 {
                0.0
            } else {
                if relpages > 0.0 {
                    cur -= 1.0;
                    relpages -= 1.0;
                }
                let density = if rt >= 0.0 && relpages > 0.0 {
                    rt / relpages
                } else {
                    let width: u32 = b.keys.iter().map(|k| costs::type_width(&k.ty())).sum();
                    ((8192 - 24) / (width + 24 + 4)) as f64
                };
                (density * cur).round_ties_even().min(rel_tuples)
            }
        };
        let height = match am {
            Am::Btree => crate::exec::sizing::btree_height(ix.oid, db).map_or(0, |h| h as i32),
            _ => -1,
        };
        // The key columns' operator classes (by_family AMs).
        let classes: Vec<Option<crate::catalog::userobj::ClassInfo>> = if am.by_family() {
            let method = crate::catalog::system::access_method_oid(&ix.method);
            ix.columns
                .iter()
                .zip(&b.keys)
                .map(|(c, k)| {
                    let oc = if c.opclass != 0 {
                        c.opclass
                    } else {
                        cat.default_opclass(method, &k.ty())
                    };
                    cat.opclass_info(oc)
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut canreturn = Vec::new();
        match am {
            Am::Btree => {
                for k in &b.keys {
                    if let Expr::Col(c, _) = k {
                        canreturn.push(*c);
                    }
                }
                canreturn.extend(ix.include.iter().copied());
            }
            // gistcanreturn / spgcanreturn: a column whose operator class can give its
            // values back (a GiST fetch function or no compress function; SP-GiST's
            // config canReturnData), and INCLUDE columns.
            Am::Gist | Am::Spgist => {
                let procs = if am == Am::Gist { cat.opmembers_all(true) } else { Vec::new() };
                let has = |c: &crate::catalog::userobj::ClassInfo, n: i16| {
                    procs.iter().any(|m| {
                        m.family == c.family && m.left == c.intype && m.right == c.intype && m.number == n
                    })
                };
                for (k, c) in b.keys.iter().zip(&classes) {
                    let Some(c) = c else { continue };
                    let ok = match am {
                        Am::Gist => has(c, 9) || !has(c, 3),
                        _ => c.oid != 10087,
                    };
                    if let (true, Expr::Col(col, _)) = (ok, k) {
                        canreturn.push(*col);
                    }
                }
                canreturn.extend(ix.include.iter().copied());
            }
            _ => {}
        }
        let fam_ops: Vec<Vec<(u32, char, u32)>> = if am.by_family() {
            let members = cat.opmembers_all(false);
            classes
                .iter()
                .map(|c| {
                    c.as_ref().map_or_else(Vec::new, |c| {
                        members
                            .iter()
                            .filter(|m| m.family == c.family)
                            .map(|m| (m.object, m.purpose, m.sortfamily))
                            .collect()
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        let order = ix
            .columns
            .iter()
            .map(|c| if am == Am::Btree { user_order(&cat, c.opclass) } else { None })
            .collect();
        let colls = ix
            .columns
            .iter()
            .zip(&b.keys)
            .map(|(c, k)| {
                c.collation.clone().or_else(|| match k {
                    Expr::Col(j, _) => t.columns.get(*j).and_then(|col| col.collation.clone()),
                    _ => None,
                })
            })
            .collect();
        out.push(IndexInfo {
            ix,
            am,
            keys: b.keys.clone(),
            canreturn,
            pred,
            target: false,
            pages,
            tuples,
            height,
            order,
            user_ops: user_ops(&cat, &fam_ops),
            fam_ops,
            colls,
        });
    }
    // get_relation_info builds its list with lcons over ascending OIDs.
    out.sort_by_key(|a| std::cmp::Reverse(a.ix.oid));
    out
}

/// A made btree operator class ordering by a made comparison function: the function,
/// and the family's operators for its input type by strategy (None: the type's own order).
fn user_order(
    cat: &crate::catalog::Catalog,
    opclass: u32,
) -> Option<(u32, Vec<(String, &'static str)>)> {
    let oc = cat.opclasses.values().find(|o| o.oid == opclass && o.method == 403)?;
    let members = cat.opmembers_all(false);
    let cmp = cat.opmembers_all(true).into_iter().find(|m| {
        m.family == oc.family && m.number == 1 && m.left == oc.intype && m.right == oc.intype
    })?;
    if !cat.functions.contains_key(&cmp.object) {
        return None;
    }
    let ops = members
        .iter()
        .filter(|m| m.family == oc.family && m.left == oc.intype && m.right == oc.intype)
        .filter_map(|m| {
            let name = cat.operators.get(&m.object)?.name.clone();
            let s = match m.number {
                1 => "<",
                2 => "<=",
                3 => "=",
                4 => ">=",
                5 => ">",
                _ => return None,
            };
            Some((name, s))
        })
        .collect();
    Some((cmp.object, ops))
}

// ── clause matching ──

/// A btree operator's strategy: (is it `<`-like, `>`-like, `=`).
fn btree_op(op: &str) -> bool {
    matches!(op, "=" | "<" | "<=" | ">" | ">=")
}

fn commute(op: &'static str) -> &'static str {
    match op {
        "<" => ">",
        ">" => "<",
        "<=" => ">=",
        ">=" => "<=",
        o => o,
    }
}

/// A type's btree operator family, for cross-type comparisons.
fn family(t: &DataType) -> DataType {
    use DataType as T;
    match t {
        T::Int2 | T::Int4 | T::Int8 => T::Int8,
        T::Float4 | T::Float8 => T::Float8,
        T::Text | T::Varchar(_) | T::Name => T::Text,
        T::Date | T::Timestamp(_) | T::Timestamptz(_) => T::Timestamptz(-1),
        T::Oid | T::Regclass | T::Regtype | T::Regproc | T::Regprocedure => T::Oid,
        // (cidr is binary-coercible to inet: network_ops covers both.)
        T::Inet | T::Cidr => T::Inet,
        t => crate::sql::binder::optimize::alias_family(t).unwrap_or_else(|| t.without_typmod()),
    }
}

/// A cast pg_cast does without a function (method 'b') between integer and oid types.
fn binary_relabel(from: &DataType, to: &DataType) -> bool {
    use DataType as T;
    let oidish = |t: &DataType| matches!(t, T::Oid | T::Regclass | T::Regtype | T::Regproc | T::Regprocedure);
    (matches!(from, T::Int4) && oidish(to)) || (oidish(from) && matches!(to, T::Int4)) || (oidish(from) && oidish(to))
}

/// The index key an operand is, looking through relabelings and the binder's implicit
/// casts to a type of the same operator family (Postgres uses a cross-type operator).
fn key_operand(e: &Expr) -> &Expr {
    match e {
        Expr::Relabel(x, _) => key_operand(x),
        Expr::Cast(x, from, to, false) if family(from) == family(to) => key_operand(x),
        // A cast to the same type without a typmod (`char(16)::bpchar`) is a relabeling.
        Expr::Cast(x, from, to, true) if from.same_base(to) && to.typmod() < 0 => key_operand(x),
        // Binary-coercible casts there and back (`(x::oid)::integer`): RelabelTypes, which
        // eval_const_expressions collapses into one over the column.
        Expr::Cast(inner, from, to, _)
            if let Expr::Cast(x, f0, t0, _) = &**inner
                && t0 == from
                && family(f0) == family(to)
                && binary_relabel(f0, t0) =>
        {
            key_operand(x)
        }
        e => e,
    }
}

fn matches_key(e: &Expr, key: &Expr) -> bool {
    let e = key_operand(e);
    match (e, key) {
        (Expr::Col(a, _), Expr::Col(b, _)) => a == b,
        _ => e.same_as(key),
    }
}

/// A value fixed for the whole scan (an index condition's comparison value).
fn scan_const(e: &Expr) -> bool {
    pseudo_constant(e) && !matches!(e, Expr::Default(_))
}

#[derive(Clone, Debug, PartialEq)]
enum Kind {
    Op(&'static str),
    Saop(&'static str),
    /// IS NULL (true) / IS NOT NULL.
    Null(bool),
}

#[derive(Clone, Debug)]
struct IClause {
    col: usize,
    /// The restriction clause it came from (position in `Rel::quals`).
    qual: usize,
    /// The index qual: the key on the left.
    expr: Expr,
    kind: Kind,
    /// The value compared with (an array for `= ANY`).
    value: Option<Expr>,
    /// The restriction clause (or OR arm) it came from.
    orig: Expr,
    lossy: bool,
}

fn commuted_call(def: &Arc<FuncDef>, l: Expr, r: Expr, ty: DataType) -> Expr {
    let name = commute(def.name);
    let def = if name == def.name {
        def.clone()
    } else {
        let mut d = (**def).clone();
        d.name = name;
        d.args.reverse();
        Arc::new(d)
    };
    Expr::Call(def, vec![l, r], ty)
}

/// The pg_operator oid of a binary operator call (0: not a built-in operator).
fn call_operator(def: &FuncDef) -> u32 {
    crate::sql::func::oper_row(def.name, def).oid
}

/// match_opclause_to_indexcol for an index whose column supports its operator family's
/// operators (GiST, SP-GiST, GIN, BRIN): `key op value`, `value op key` when the
/// commutator is in the family, and IS [NOT] NULL where the AM searches for nulls.
fn family_clause(ii: &IndexInfo, col: usize, qn: usize, q: &Expr) -> Option<IClause> {
    let key = &ii.keys[col];
    let ops = ii.fam_ops.get(col)?;
    let searchable = |oid: u32| oid != 0 && ops.iter().any(|(o, p, _)| *o == oid && *p == 's');
    let mk = |expr: Expr, kind: Kind, value: Option<Expr>| IClause {
        col,
        qual: qn,
        expr,
        kind,
        value,
        orig: q.clone(),
        lossy: false,
    };
    match q {
        // text_starts_with_support: starts_with(key, prefix) as `key ^@ prefix` where the
        // family has ^@ (the function is still checked on each row).
        Expr::Call(def, args, ty) if def.name == "starts_with" && args.len() == 2 => {
            if !matches_key(&args[0], key) || !scan_const(&args[1]) {
                return None;
            }
            let op = crate::sql::func::resolve_binop("^@", &DataType::Text, &DataType::Text)
                .ok()?
                .def;
            if !searchable(call_operator(&op)) {
                return None;
            }
            let e = Expr::Call(op.clone(), vec![args[0].clone(), args[1].clone()], ty.clone());
            let mut c = mk(e, Kind::Op(op.name), Some(args[1].clone()));
            c.lossy = true;
            Some(c)
        }
        Expr::Call(def, args, ty) if args.len() == 2 => {
            let (a, b) = (&args[0], &args[1]);
            let oid = ii.op_oid(def);
            if matches_key(a, key) && scan_const(b) && searchable(oid) {
                return Some(mk(q.clone(), Kind::Op(def.name), Some(b.clone())));
            }
            if matches_key(b, key) && scan_const(a) {
                let (com, cdef) = ii.commutator(def)?;
                if searchable(com) {
                    let e = Expr::Call(cdef.clone(), vec![b.clone(), a.clone()], ty.clone());
                    return Some(mk(e, Kind::Op(cdef.name), Some(a.clone())));
                }
            }
            None
        }
        Expr::IsNull(x, not) if ii.am.searchnulls() && matches_key(x, key) => {
            Some(mk(q.clone(), Kind::Null(!*not), None))
        }
        _ => None,
    }
}

/// match_clause_to_indexcol: whether restriction clause `q` can be an index qual on
/// column `col` of `ii`.
fn match_clause(
    ii: &IndexInfo,
    col: usize,
    qn: usize,
    q: &Expr,
    booleq: Option<&Arc<FuncDef>>,
) -> Option<IClause> {
    let key = &ii.keys[col];
    // match_boolean_index_clause: `k`, `NOT k`, `k IS TRUE` / `IS FALSE` as `k = bool`, for
    // a btree column or a family with boolean `=` (btree_gin's bool_ops).
    let boolean = match ii.am {
        Am::Btree => true,
        am if am.by_family() => ii.fam_ops.get(col).is_some_and(|ops| ops.iter().any(|(o, p, _)| *o == 91 && *p == 's')),
        _ => false,
    };
    if let (Some(eq), true, DataType::Bool) = (booleq, boolean, key.ty()) {
        let val = match q {
            e if matches_key(e, key) => Some(true),
            Expr::Not(x) if matches_key(x, key) => Some(false),
            Expr::BoolTest(x, crate::sql::expr::BoolTest::IsTrue) if matches_key(x, key) => {
                Some(true)
            }
            Expr::BoolTest(x, crate::sql::expr::BoolTest::IsFalse) if matches_key(x, key) => {
                Some(false)
            }
            _ => None,
        };
        if let Some(v) = val {
            let value = Expr::Const(Value::Bool(v), DataType::Bool);
            return Some(IClause {
                col,
                qual: qn,
                expr: Expr::Call(eq.clone(), vec![key.clone(), value.clone()], DataType::Bool),
                kind: Kind::Op("="),
                value: Some(value),
                orig: q.clone(),
                lossy: false,
            });
        }
    }
    if ii.am.by_family() {
        return family_clause(ii, col, qn, q);
    }
    // A column a made operator class orders: its family's operators (the index's byte
    // order isn't theirs: no key range, the scan checks them on each row).
    if let Some(Some((_, ops))) = ii.order.get(col) {
        return match q {
            Expr::Call(def, args, _) if args.len() == 2 && matches_key(&args[0], key) => {
                let (_, s) = ops.iter().find(|(n, _)| n == def.name)?;
                Some(IClause {
                    col,
                    qual: qn,
                    expr: q.clone(),
                    kind: Kind::Op(s),
                    value: Some(args[1].clone()),
                    orig: q.clone(),
                    lossy: false,
                })
            }
            Expr::IsNull(x, not) if matches_key(x, key) => Some(IClause {
                col,
                qual: qn,
                expr: q.clone(),
                kind: Kind::Null(!*not),
                value: None,
                orig: q.clone(),
                lossy: false,
            }),
            _ => None,
        };
    }
    let kty = family(&key.ty());
    let pattern_ops = ii.pattern_ops(col);
    let ops_ok = |op: &str| match ii.am {
        Am::Btree if pattern_ops => matches!(op, "=" | "~<~" | "~<=~" | "~>=~" | "~>~"),
        Am::Btree => btree_op(op),
        _ => op == "=",
    };
    // The strategy a pattern comparison stands for.
    let strat = |op: &'static str| match op {
        "~<~" => "<",
        "~<=~" => "<=",
        "~>=~" => ">=",
        "~>~" => ">",
        o => o,
    };
    let mk = |expr: Expr, kind: Kind, value: Option<Expr>| IClause {
        col,
        qual: qn,
        expr,
        kind,
        value,
        orig: q.clone(),
        lossy: false,
    };
    match q {
        Expr::Call(def, args, ty)
            if args.len() == 2 && ops_ok(def.name) && (byte_order_op(def) || own_collation_eq(ii, col, def)) =>
        {
            let (a, b) = (&args[0], &args[1]);
            if matches_key(a, key) && scan_const(b) && family(&b.ty()) == kty {
                return Some(mk(q.clone(), Kind::Op(strat(def.name)), Some(b.clone())));
            }
            if matches_key(b, key) && scan_const(a) && family(&a.ty()) == kty && !pattern_ops {
                let e = commuted_call(def, b.clone(), a.clone(), ty.clone());
                return Some(mk(e, Kind::Op(commute(def.name)), Some(a.clone())));
            }
            None
        }
        Expr::ArrayOp {
            left,
            right,
            op,
            all: false,
        } if ii.am == Am::Btree && btree_op(op.name) && byte_order_op(op) => {
            let et = right.ty().elem().cloned()?;
            if matches_key(left, key) && scan_const(right) && family(&et) == kty {
                return Some(mk(q.clone(), Kind::Saop(op.name), Some((**right).clone())));
            }
            None
        }
        Expr::IsNull(x, not) if ii.am == Am::Btree && matches_key(x, key) => {
            Some(mk(q.clone(), Kind::Null(!*not), None))
        }
        Expr::Or(arms) if ii.am == Am::Btree => or_to_saop(ii, col, qn, arms),
        Expr::RowCompare(rc) if ii.am == Am::Btree && !pattern_ops => rowcompare_clause(ii, col, qn, q, rc),
        _ => None,
    }
}

/// match_rowcompare_to_indexcol / expand_indexqual_rowcompare: a row comparison whose
/// first pair compares key column `col` with a value fixed for the scan, the index's key
/// columns (any of them) on that side of the pairs after it as far as they go; the
/// comparison cut to those pairs is lossy (`<` becoming `<=`). The scan reads from (or to)
/// the first key's value.
fn rowcompare_clause(
    ii: &IndexInfo,
    col: usize,
    qn: usize,
    q: &Expr,
    rc: &crate::sql::expr::RowCompare,
) -> Option<IClause> {
    let (ls, rs) = rc.sides();
    let key = &ii.keys[col];
    let var_left = if matches_key(ls[0], key) && scan_const(rs[0]) {
        true
    } else if matches_key(rs[0], key) && scan_const(ls[0]) {
        false
    } else {
        return None;
    };
    let ok_op = |c: &Expr| matches!(c, Expr::Call(d, a, _) if a.len() == 2 && byte_order_op(d) && matches!(d.name, "<" | "<=" | ">" | ">="));
    if !rc.cmps.iter().all(ok_op) {
        return None;
    }
    let (vars, consts) = if var_left { (&ls, &rs) } else { (&rs, &ls) };
    if family(&vars[0].ty()) != family(&consts[0].ty()) {
        return None;
    }
    let mut matching = 1;
    while matching < rc.cmps.len() {
        let (v, c) = (vars[matching], consts[matching]);
        let hit = ii
            .keys
            .iter()
            .enumerate()
            .any(|(j, k)| matches_key(v, k) && ii.order.get(j).is_none_or(Option::is_none) && family(&k.ty()) == family(&c.ty()));
        if !hit || !scan_const(c) {
            break;
        }
        matching += 1;
    }
    // (With the key on the right: the pairs commuted.)
    let orient = |c: &Expr| -> Expr {
        match c {
            Expr::Call(d, a, t) if !var_left => commuted_call(d, a[1].clone(), a[0].clone(), t.clone()),
            c => c.clone(),
        }
    };
    let op = if var_left { rc.op } else { commute(rc.op) };
    let lossy = matching < rc.cmps.len();
    // (Cut short, `<` / `>` take in the equal prefix: `<=` / `>=`.)
    let op = match (lossy, op) {
        (true, "<") => "<=",
        (true, ">") => ">=",
        (_, o) => o,
    };
    let cmps: Vec<Expr> = rc.cmps[..matching]
        .iter()
        .map(|c| {
            let c = orient(c);
            match c {
                Expr::Call(d, a, t) if d.name != op => {
                    let r = crate::sql::func::resolve_binop(op, &a[0].ty(), &a[1].ty()).ok();
                    match r {
                        Some(r) => {
                            let mut nd = (*r.def).clone();
                            nd.coll = d.coll.clone();
                            Expr::Call(Arc::new(nd), a, t)
                        }
                        None => Expr::Call(d, a, t),
                    }
                }
                c => c,
            }
        })
        .collect();
    let eqs: Vec<Expr> = rc.eqs[..matching].iter().map(orient).collect();
    let expr = if matching == 1 {
        cmps[0].clone()
    } else {
        Expr::RowCompare(Box::new(crate::sql::expr::RowCompare { op, cmps, eqs }))
    };
    // (The key range: from the first key's value, inclusive.)
    let bound = match op {
        "<" | "<=" => "<=",
        _ => ">=",
    };
    Some(IClause {
        col,
        qual: qn,
        expr,
        kind: Kind::Op(bound),
        value: Some(consts[0].clone()),
        orig: q.clone(),
        lossy,
    })
}

/// A LATERAL join's inner side that is a relation: a scan (under projections and filters),
/// or an inheritance Append of them.
fn lateral_rel(p: &Plan) -> bool {
    match p {
        Plan::Scan(_) => true,
        Plan::Project { input, .. } | Plan::Filter { input, .. } => lateral_rel(input),
        Plan::Append { inputs, inh: Some(_), .. } => inputs.iter().all(|i| matches!(&**i, Plan::Empty { .. }) || lateral_rel(i)),
        _ => false,
    }
}

/// A partitionwise join Append's id (`Plan::Append`'s `pw`), unique in the process.
fn next_pw_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Planner<'_, '_> {
    /// match_pattern_prefix (like_support.c): a LIKE / regular expression with a fixed
    /// prefix as a btree range (or `=` for an exact pattern), both lossy.
    fn pattern_clause(&self, ii: &IndexInfo, col: usize, qn: usize, q: &Expr) -> Vec<IClause> {
        let Expr::Call(def, args, _) = q else {
            return Vec::new();
        };
        if ii.am != Am::Btree || args.len() != 2 {
            return Vec::new();
        }
        let ptype = match def.name {
            "~~" => costs::Ptype::Like,
            "~~*" => costs::Ptype::LikeIc,
            "~" => costs::Ptype::Regex,
            "~*" => costs::Ptype::RegexIc,
            _ => return Vec::new(),
        };
        let key = &ii.keys[col];
        // (A text key indexed as bpchar (bpchar_ops): only a bpchar pattern operator, over
        // the key relabelled as bpchar, matches its family.)
        let bpchar_class = bpchar_class(ii, col);
        if bpchar_class {
            match &args[0] {
                Expr::Cast(x, DataType::Text | DataType::Varchar(_), DataType::Bpchar(_), _) if matches_key(x, key) => {}
                _ => return Vec::new(),
            }
        } else if !matches_key(&args[0], key)
            || !matches!(
                key.ty(),
                DataType::Text | DataType::Varchar(_) | DataType::Name | DataType::Bpchar(_)
            )
        {
            return Vec::new();
        }
        // (A COLLATE on the pattern is its collation, which the call carries.)
        let mut pe = &args[1];
        while let Expr::Collate(x, ..) = pe {
            pe = x;
        }
        let Expr::Const(Value::Text(patt), _) = pe else {
            return Vec::new();
        };
        let (status, prefix, _) = costs::pattern_prefix(patt, ptype);
        // (Under a nondeterministic collation only an exact match, and only with an index
        // of that collation: match_pattern_prefix.)
        if let Some(c) = def.coll.as_ref().filter(|c| !c.deterministic)
            && (status != costs::PrefixStatus::Exact
                || ii.colls.get(col).cloned().flatten().as_deref() != Some(c.name.as_str()))
        {
            return Vec::new();
        }
        let pattern_ops = ii.pattern_ops(col);
        let leftop = match &args[0] {
            // (A bpchar key is compared as bpchar.)
            Expr::Cast(x, DataType::Bpchar(_), DataType::Text, false) => (**x).clone(),
            e => e.clone(),
        };
        let cty = match key.ty() {
            DataType::Bpchar(_) => DataType::Bpchar(-1),
            _ if bpchar_class => DataType::Bpchar(-1),
            _ => DataType::Text,
        };
        let text = |s: &str| Expr::Const(Value::Text(s.into()), cty.clone());
        let mut b = crate::sql::binder::Binder::new(self.ctx, Vec::new());
        // (The index condition compares under the index's collation, the database default
        // unless the column has another.)
        let key_coll = crate::types::collation::by_name(
            &self.ctx.catalog(),
            ii.colls.get(col).cloned().flatten().as_deref().unwrap_or("default"),
        );
        let index_coll = key_coll.clone().filter(|c| !c.bytewise());
        // (A range needs an index in byte order, a "C" collation: match_pattern_prefix.)
        let c_order = key_coll.is_none_or(|c| c.collate_is_c);
        let mut make = |op: &'static str, v: Expr| -> Option<IClause> {
            let Expr::Call(d, _, ty) = b.builtin_binop(op, leftop.clone(), v.clone()).ok()? else {
                return None;
            };
            let d = match &index_coll {
                Some(c) if !pattern_ops => Arc::new(FuncDef { coll: Some(c.clone()), ..(*d).clone() }),
                _ => d,
            };
            let e = Expr::Call(d, vec![leftop.clone(), v.clone()], ty);
            Some(IClause {
                col,
                qual: qn,
                expr: e,
                kind: Kind::Op(match op {
                    "~>=~" => ">=",
                    "~<~" => "<",
                    o => o,
                }),
                value: Some(v),
                orig: q.clone(),
                lossy: true,
            })
        };
        let mut out = Vec::new();
        match status {
            costs::PrefixStatus::None => {}
            costs::PrefixStatus::Exact => out.extend(make("=", text(&prefix))),
            costs::PrefixStatus::Partial if !pattern_ops && !c_order => {}
            costs::PrefixStatus::Partial => {
                let (ge, lt) = if pattern_ops {
                    ("~>=~", "~<~")
                } else {
                    (">=", "<")
                };
                out.extend(make(ge, text(&prefix)));
                if let Some(g) = costs::greater_string(&prefix) {
                    out.extend(make(lt, text(&g)));
                }
            }
        }
        out
    }
}

impl Planner<'_, '_> {
    /// match_network_subset (network.c's planner support): `k << c` / `k <<= c` (or
    /// `c >> k`, `c >>= k`) on a btree inet key as the range `k > network(c)` (`>=` for the
    /// `=` forms) and `k <= broadcast(c)` with a full mask; lossy (the clause stays a filter).
    fn network_clause(&self, ii: &IndexInfo, col: usize, qn: usize, q: &Expr) -> Vec<IClause> {
        let Expr::Call(def, args, _) = q else {
            return Vec::new();
        };
        if ii.am != Am::Btree || args.len() != 2 {
            return Vec::new();
        }
        let (keyop, c, is_eq) = match def.name {
            "<<" => (&args[0], &args[1], false),
            "<<=" => (&args[0], &args[1], true),
            ">>" => (&args[1], &args[0], false),
            ">>=" => (&args[1], &args[0], true),
            _ => return Vec::new(),
        };
        let key = &ii.keys[col];
        let kty = key.ty();
        if !matches_key(keyop, key) || !matches!(kty, DataType::Inet | DataType::Cidr) {
            return Vec::new();
        }
        let Expr::Const(Value::Inet(addr, bits), _) = c else {
            return Vec::new();
        };
        let first = crate::types::text::inet_network(addr, *bits);
        let last: Vec<u8> = addr
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let keep = (*bits as usize).saturating_sub(i * 8).min(8);
                if keep == 8 { *b } else { b | (0xffu8 >> keep) }
            })
            .collect();
        let full = (addr.len() * 8) as u8;
        let konst = |a: Vec<u8>, m: u8| Expr::Const(Value::Inet(a.into(), m), kty.clone());
        let mut b = crate::sql::binder::Binder::new(self.ctx, Vec::new());
        let mut make = |op: &'static str, v: Expr| -> Option<IClause> {
            let Expr::Call(d, _, ty) = b.builtin_binop(op, keyop.clone(), v.clone()).ok()? else {
                return None;
            };
            Some(IClause {
                col,
                qual: qn,
                expr: Expr::Call(d, vec![keyop.clone(), v.clone()], ty),
                kind: Kind::Op(op),
                value: Some(v),
                orig: q.clone(),
                lossy: true,
            })
        };
        let mut out = Vec::new();
        out.extend(make(if is_eq { ">=" } else { ">" }, konst(first, *bits)));
        out.extend(make("<=", konst(last, full)));
        if out.len() != 2 {
            return Vec::new();
        }
        out
    }
}

/// match_orclause_to_indexcol: `k op c1 OR k op c2 …` as `k op ANY (array)`.
fn or_to_saop(ii: &IndexInfo, col: usize, qn: usize, arms: &[Expr]) -> Option<IClause> {
    let key = &ii.keys[col];
    let mut first: Option<(Arc<FuncDef>, &'static str, DataType, Expr)> = None;
    let mut vals = Vec::new();
    for a in arms {
        let Expr::Call(def, args, _) = a else {
            return None;
        };
        if args.len() != 2 || !btree_op(def.name) {
            return None;
        }
        let (k, v, op) = if matches_key(&args[0], key) && !args[1].is_volatile_call() {
            (&args[0], &args[1], def.name)
        } else if matches_key(&args[1], key) && !args[0].is_volatile_call() {
            (&args[1], &args[0], commute(def.name))
        } else {
            return None;
        };
        if !v.columns().is_empty() {
            return None;
        }
        let vt = v.ty();
        match &first {
            None => {
                if family(&vt) != family(&key.ty()) || matches!(vt, DataType::Record) {
                    return None;
                }
                first = Some((def.clone(), op, vt, k.clone()));
            }
            Some((_, o, t, _)) => {
                if *o != op || *t != vt {
                    return None;
                }
            }
        }
        vals.push(v.clone());
    }
    let (def, op, vt, k) = first?;
    let arr_ty = DataType::Array(Box::new(vt.clone()));
    let all_const = vals.iter().all(|v| matches!(v, Expr::Const(..)));
    let right = if all_const {
        let items = vals
            .iter()
            .map(|v| match v {
                Expr::Const(x, _) => x.clone(),
                _ => Value::Null,
            })
            .collect();
        Expr::Const(
            Value::Array(Arc::new(crate::types::array::ArrayValue::from_vec(
                vt.clone(),
                items,
            ))),
            arr_ty,
        )
    } else {
        Expr::Array(vals, arr_ty)
    };
    let def = if def.name == op {
        def
    } else {
        let mut d = (*def).clone();
        d.name = op;
        Arc::new(d)
    };
    let expr = Expr::ArrayOp {
        left: Box::new(k),
        right: Box::new(right.clone()),
        op: def,
        all: false,
    };
    Some(IClause {
        col,
        qual: qn,
        expr,
        kind: Kind::Saop(op),
        value: Some(right),
        orig: Expr::Or(arms.to_vec()),
        lossy: false,
    })
}

trait VolatileCall {
    fn is_volatile_call(&self) -> bool;
}

impl VolatileCall for Expr {
    /// contain_volatile_functions (sub-selects aren't volatile as such).
    fn is_volatile_call(&self) -> bool {
        let mut v = false;
        self.walk(&mut |e| {
            if let Expr::Call(d, ..) = e
                && d.volatility == 'v'
            {
                v = true
            }
        });
        v
    }
}

/// A column of a non-bpchar key indexed with a bpchar operator class (bpchar_ops).
fn bpchar_class(ii: &IndexInfo, col: usize) -> bool {
    !matches!(ii.keys.get(col).map(|k| k.ty()), Some(DataType::Bpchar(_)))
        && ii
            .ix
            .columns
            .get(col)
            .filter(|c| c.opclass != 0)
            .and_then(|c| crate::catalog::system::opclass_intype(c.opclass))
            == Some(1042)
}

/// A clause comparing a value relabelled as bpchar.
fn bpchar_operand(q: &Expr) -> bool {
    match q {
        Expr::Call(_, args, _) => args.first().is_some_and(|a| {
            matches!(a, Expr::Cast(_, DataType::Text | DataType::Varchar(_), DataType::Bpchar(_), _))
        }),
        _ => false,
    }
}

/// The index clauses of `quals` for each index column, in index column order.
fn match_quals(
    ii: &IndexInfo,
    quals: &[Expr],
    only: Option<&[usize]>,
    booleq: Option<&Arc<FuncDef>>,
    pl: &Planner,
) -> Vec<IClause> {
    let mut out = Vec::new();
    for col in 0..ii.keys.len() {
        for (qn, q) in quals.iter().enumerate() {
            if only.is_some_and(|o| !o.contains(&qn)) {
                continue;
            }
            // (A partial index's indrestrictinfo leaves out what its predicate implies.)
            if ii.implies(q) {
                continue;
            }
            // (Nor is a clause over a PlaceHolderVar an index qual.)
            if pl.phv_quals.borrow().iter().any(|(o, x)| *o == ii.ix.table && x.same_as(q)) {
                continue;
            }
            // (A text key indexed as bpchar: only bpchar operators, over the key relabelled
            // as bpchar, are its operator family's.)
            if bpchar_class(ii, col) && !bpchar_operand(q) {
                continue;
            }
            // A clause matches the first column it can (match_clause_to_index).
            if out.iter().any(|c: &IClause| c.qual == qn) {
                continue;
            }
            if let Some(c) = match_clause(ii, col, qn, q, booleq) {
                out.push(c);
            } else {
                out.extend(pl.pattern_clause(ii, col, qn, q));
                out.extend(pl.network_clause(ii, col, qn, q));
            }
        }
    }
    out.sort_by_key(|c| c.col);
    out
}

// ── predicates (predtest.c, the simple cases) ──

/// predicate_implied_by: the clauses (ANDed) imply the predicate (ANDed).
fn implied_by(pred: &[Expr], quals: &[Expr]) -> bool {
    let one = |l: &[Expr]| {
        if l.len() == 1 {
            l[0].clone()
        } else {
            Expr::And(l.to_vec())
        }
    };
    if pred.is_empty() {
        return true;
    }
    implies_tree(&one(quals), &one(pred))
}

enum PredClass {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Atom,
}

/// predicate_classify: AND / OR structure, a constant `= ANY` / `= ALL` array as one.
fn classify(e: &Expr) -> PredClass {
    match e {
        Expr::And(l) => PredClass::And(l.clone()),
        Expr::Or(l) => PredClass::Or(l.clone()),
        Expr::ArrayOp {
            left,
            right,
            op,
            all,
        } => {
            let items: Option<Vec<Expr>> = match &**right {
                Expr::Const(Value::Array(a), t) if a.items.len() <= 100 => {
                    let et = t.elem().cloned().unwrap_or(DataType::Unknown);
                    Some(
                        a.items
                            .iter()
                            .map(|v| Expr::Const(v.clone(), et.clone()))
                            .collect(),
                    )
                }
                Expr::Array(items, _) => Some(items.clone()),
                _ => None,
            };
            match items {
                Some(items) => {
                    let list = items
                        .into_iter()
                        .map(|v| {
                            Expr::Call(op.clone(), vec![(**left).clone(), v], DataType::Bool)
                        })
                        .collect();
                    if *all {
                        PredClass::And(list)
                    } else {
                        PredClass::Or(list)
                    }
                }
                None => PredClass::Atom,
            }
        }
        _ => PredClass::Atom,
    }
}

/// predicate_implied_by: clause `c` (an AND of conditions) implies predicate `p`.
pub(crate) fn predicate_implied_by(p: &Expr, c: &Expr) -> bool {
    implies_tree(c, p)
}

/// predicate_implied_by_recurse.
fn implies_tree(c: &Expr, p: &Expr) -> bool {
    use PredClass::*;
    let pc = classify(p);
    match (classify(c), pc) {
        (And(_), And(ps)) => ps.iter().all(|x| implies_tree(c, x)),
        (And(cs), Or(ps)) => {
            ps.iter().any(|x| implies_tree(c, x)) || cs.iter().any(|x| implies_tree(x, p))
        }
        (And(cs), Atom) => cs.iter().any(|x| implies_tree(x, p)),
        (Or(cs), Or(ps)) => cs.iter().all(|ci| ps.iter().any(|x| implies_tree(ci, x))),
        (Or(cs), _) => cs.iter().all(|x| implies_tree(x, p)),
        (Atom, And(ps)) => ps.iter().all(|x| implies_tree(c, x)),
        (Atom, Or(ps)) => ps.iter().any(|x| implies_tree(c, x)),
        (Atom, Atom) => implies(c, p),
    }
}

/// A clause `q` implies predicate `p`.
fn implies(q: &Expr, p: &Expr) -> bool {
    if q.same_as(p) {
        return true;
    }
    // A boolean index's `x = true` / `x = false` against `x` / `NOT x`.
    if let Expr::Call(d, args, _) = q
        && d.name == "="
        && args.len() == 2
        && let Expr::Const(Value::Bool(b), _) = &args[1]
    {
        match p {
            Expr::Not(x) if !*b && x.same_as(&args[0]) => return true,
            x if *b && x.same_as(&args[0]) => return true,
            _ => {}
        }
    }
    // `x IS NOT NULL` follows from a strict comparison of x.
    if let Expr::IsNull(x, true) = p {
        if let Expr::Call(d, args, _) = q
            && d.strict
            && args.iter().any(|a| key_operand(a).same_as(key_operand(x)))
        {
            return true;
        }
        return false;
    }
    let (Some((px, pop, pv)), Some((qx, qop, qv))) = (cmp_parts(p), cmp_parts(q)) else {
        return false;
    };
    if !key_operand(&px).same_as(key_operand(&qx)) {
        return false;
    }
    let (Expr::Const(pv, _), Expr::Const(qv, _)) = (&pv, &qv) else {
        return false;
    };
    if pv.is_null() || qv.is_null() {
        return false;
    }
    let o = crate::exec::agg::cmp_rows(std::slice::from_ref(qv), std::slice::from_ref(pv));
    use Ordering::*;
    // q: x qop qv; p: x pop pv. Does every x satisfying q satisfy p?
    match (qop, pop) {
        ("=", "=") => o == Equal,
        ("=", "<") => o == Less,
        ("=", "<=") => o != Greater,
        ("=", ">") => o == Greater,
        ("=", ">=") => o != Less,
        ("=", "<>") => o != Equal,
        ("<", "<") | ("<=", "<=") | ("<", "<=") => o != Greater,
        ("<=", "<") => o == Less,
        (">", ">") | (">=", ">=") | (">", ">=") => o != Less,
        (">=", ">") => o == Greater,
        ("<", "<>") => o != Greater,
        ("<=", "<>") => o == Less,
        (">", "<>") => o != Less,
        (">=", "<>") => o == Greater,
        _ => false,
    }
}

/// `x op const` (commuted when the constant is on the left).
fn cmp_parts(e: &Expr) -> Option<(Expr, &'static str, Expr)> {
    let Expr::Call(d, args, _) = e else {
        return None;
    };
    if args.len() != 2 || !matches!(d.name, "=" | "<" | "<=" | ">" | ">=" | "<>") {
        return None;
    }
    let isc = |x: &Expr| matches!(x, Expr::Const(..));
    if isc(&args[1]) && !isc(&args[0]) {
        Some((args[0].clone(), d.name, args[1].clone()))
    } else if isc(&args[0]) && !isc(&args[1]) {
        Some((args[1].clone(), commute(d.name), args[0].clone()))
    } else {
        None
    }
}

// ── paths ──

/// A pathkey: an expression over the table row and its direction.
#[derive(Clone, Debug)]
pub struct PathKey {
    pub expr: Expr,
    pub desc: bool,
    pub nulls_first: bool,
    /// The comparison function of the made operator family ordering it (0: the type's
    /// own order).
    pub cmp: u32,
}

impl PathKey {
    fn same(&self, o: &PathKey) -> bool {
        self.desc == o.desc
            && self.nulls_first == o.nulls_first
            && self.cmp == o.cmp
            && same_key(&self.expr, &o.expr)
    }
}

fn same_key(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::Col(x, _), Expr::Col(y, _)) => x == y,
        _ => a.same_as(b),
    }
}

#[derive(Clone, Debug)]
struct IPath {
    ix: usize,
    clauses: Vec<IClause>,
    backward: bool,
    keys: Vec<PathKey>,
    only: bool,
    startup: f64,
    total: f64,
    index_total: f64,
    index_sel: f64,
    rows: f64,
    disabled: u32,
    /// For a parallel scan of it (cost_index's partial path): the CPU cost per heap
    /// tuple fetched times those (divided among the workers), the heap pages it fetches
    /// in random order (-1: index-only), and the index pages.
    cpu_run: f64,
    rand_pages: f64,
    index_pages: f64,
    /// ORDER BY expressions the scan's ordering operators compute (EXPLAIN's Order By).
    orderbys: Vec<Expr>,
}

#[derive(Clone, Debug)]
enum BPath {
    Index(IPath),
    And(Vec<BPath>, f64, f64),
    Or(Vec<BPath>, f64, f64),
}

impl BPath {
    /// Whether it uses a restriction clause at or past position `from` (a parameterized
    /// one, put after the others), or an index condition taking a parameter's value.
    fn uses_from(&self, from: usize) -> bool {
        match self {
            BPath::Index(p) => p.clauses.iter().any(|c| {
                (c.qual >= from && c.qual != usize::MAX)
                    || c.value.as_ref().is_some_and(|v| {
                        let mut outer = false;
                        v.walk(&mut |e| outer |= matches!(e, Expr::Outer(1, ..)));
                        outer
                    })
            }),
            BPath::And(l, ..) | BPath::Or(l, ..) => l.iter().any(|b| b.uses_from(from)),
        }
    }

    /// get_indexpath_pages: the pages of its indexes.
    fn index_pages(&self, rel: &Rel) -> f64 {
        match self {
            BPath::Index(p) => rel.indexes[p.ix].pages,
            BPath::And(l, ..) | BPath::Or(l, ..) => l.iter().map(|b| b.index_pages(rel)).sum(),
        }
    }

    /// cost_bitmap_tree_node: (cost, selectivity).
    fn cost_sel(&self) -> (f64, f64) {
        match self {
            BPath::Index(p) => ((0.1 * CPU_OPERATOR_COST).mul_add(p.rows, p.index_total), p.index_sel),
            BPath::And(_, c, s) | BPath::Or(_, c, s) => (*c, *s),
        }
    }

    /// The restriction clauses (and index predicates) it uses.
    fn quals(&self, rel: &Rel, out: &mut Vec<Expr>, preds: &mut Vec<Expr>) {
        match self {
            BPath::Index(p) => {
                let _ = rel;
                for c in &p.clauses {
                    out.push(c.orig.clone());
                }
                preds.extend(rel.indexes[p.ix].pred.iter().cloned());
            }
            BPath::And(l, ..) | BPath::Or(l, ..) => l.iter().for_each(|x| x.quals(rel, out, preds)),
        }
    }
}

#[derive(Clone, Debug)]
enum PathKind {
    Seq,
    Index(IPath),
    Bitmap(BPath),
    /// A TID scan: its TID quals (ORed) and the clauses they replace.
    Tid(Vec<Expr>, Vec<Expr>),
    /// A TID range scan over its (ANDed) range quals.
    TidRange(Vec<Expr>),
    /// A sort over another path.
    Sort(Box<Path>),
}

#[derive(Clone, Debug)]
struct Path {
    kind: PathKind,
    startup: f64,
    total: f64,
    rows: f64,
    keys: Vec<PathKey>,
    disabled: u32,
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum CostCmp {
    Better1,
    Better2,
    Equal,
    Different,
}

fn fuzzy_cmp(a: &Path, b: &Path, fuzz: f64, startup: bool) -> CostCmp {
    if a.disabled != b.disabled {
        return if a.disabled < b.disabled {
            CostCmp::Better1
        } else {
            CostCmp::Better2
        };
    }
    if a.total > b.total * fuzz {
        if startup && b.startup > a.startup * fuzz {
            return CostCmp::Different;
        }
        return CostCmp::Better2;
    }
    if b.total > a.total * fuzz {
        if startup && a.startup > b.startup * fuzz {
            return CostCmp::Different;
        }
        return CostCmp::Better1;
    }
    if a.startup > b.startup * fuzz {
        return CostCmp::Better2;
    }
    if b.startup > a.startup * fuzz {
        return CostCmp::Better1;
    }
    CostCmp::Equal
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum KeysCmp {
    Equal,
    Better1,
    Better2,
    Different,
}

fn cmp_keys(a: &[PathKey], b: &[PathKey]) -> KeysCmp {
    let n = a.len().min(b.len());
    if !(0..n).all(|i| a[i].same(&b[i])) {
        return KeysCmp::Different;
    }
    match a.len().cmp(&b.len()) {
        Ordering::Equal => KeysCmp::Equal,
        Ordering::Greater => KeysCmp::Better1,
        Ordering::Less => KeysCmp::Better2,
    }
}

/// add_path (unparameterized paths).
fn add_path(list: &mut Vec<Path>, new: Path, startup: bool) {
    let mut accept = true;
    let mut insert_at = 0;
    let mut i = 0;
    while i < list.len() {
        let old = &list[i];
        let mut remove_old = false;
        let cc = fuzzy_cmp(&new, old, FUZZ, startup);
        if cc != CostCmp::Different {
            let kc = cmp_keys(&new.keys, &old.keys);
            if kc != KeysCmp::Different {
                match cc {
                    CostCmp::Equal => match kc {
                        KeysCmp::Better1 => {
                            if new.rows <= old.rows {
                                remove_old = true
                            }
                        }
                        KeysCmp::Better2 => {
                            if new.rows >= old.rows {
                                accept = false
                            }
                        }
                        _ => {
                            if new.rows < old.rows {
                                remove_old = true
                            } else if new.rows > old.rows {
                                accept = false
                            } else if fuzzy_cmp(&new, old, 1.0000000001, startup)
                                == CostCmp::Better1
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

/// compare_path_costs on total cost (then startup).
fn cheaper_total(a: &Path, b: &Path) -> bool {
    if a.disabled != b.disabled {
        return a.disabled < b.disabled;
    }
    if a.total != b.total {
        return a.total < b.total;
    }
    a.startup < b.startup
}

fn cheapest(list: &[Path]) -> Option<&Path> {
    let mut best: Option<&Path> = None;
    for p in list {
        if best.is_none_or(|b| cheaper_total(p, b)) {
            best = Some(p);
        }
    }
    best
}

/// What the relation's scan is asked for: an order (the query's pathkeys) and a LIMIT.
#[derive(Clone, Debug, Default)]
struct Want {
    keys: Vec<PathKey>,
    /// LIMIT count + offset estimates (preprocess_limit; -1: not constant, 0: none).
    limit: Option<(f64, f64)>,
    /// Columns (over the table row) a merge join could use an ordering by.
    merge: Vec<Expr>,
}

impl Want {
    /// The planner cares about fast-start plans (tuple_fraction > 0).
    fn startup(&self) -> bool {
        matches!(self.limit, Some((c, _)) if c != 0.0)
    }
}

/// A planner flag set for a scope, its value put back as the scope ends.
struct FlagScope<'a>(&'a std::cell::Cell<bool>, bool);

impl<'a> FlagScope<'a> {
    fn set(c: &'a std::cell::Cell<bool>, v: bool) -> Self {
        let old = c.replace(v);
        FlagScope(c, old)
    }
}

impl Drop for FlagScope<'_> {
    fn drop(&mut self) {
        self.0.set(self.1);
    }
}

struct Planner<'c, 'a> {
    ctx: &'c ExecCtx<'a>,
    cat: Arc<crate::catalog::Catalog>,
    g: Gucs,
    /// Columns' actual (min, max), read once (get_actual_variable_range).
    ends: std::cell::RefCell<std::collections::HashMap<(u32, usize), Option<(Value, Value)>>>,
    /// Relations join removal and self-join elimination took out of the plan (they stay
    /// in the range table: EXPLAIN still qualifies column names).
    removed: std::cell::Cell<usize>,
    /// Row types of the whole-row references in the plan (`joinpath::whole_rows`).
    whole_rows: std::cell::RefCell<Vec<u32>>,
    /// The range table indexes of the scans a row mark will read the key of (a
    /// data-modifying statement's other relations, a locking level's unlocked ones:
    /// ROW_MARK_REFERENCE): their ctid is needed, no index-only scan (`rowmarks::referenced`).
    ref_marks: std::cell::RefCell<std::collections::HashSet<usize>>,
    /// The range table indexes of the level's sources of no table's rows with a whole-row
    /// mark (ROW_MARK_COPY, `rowmarks::copied`): their whole row's width is in the paths'.
    copy_marks: std::cell::RefCell<std::collections::HashSet<usize>>,
    /// Partitionwise joins made, by their first child's address: as FROM items of joins
    /// above (`joinpath::PwJoin`).
    /// Partitionwise joins' Appends (by their `pw` id) and how they are partitioned.
    pw_joins: std::cell::RefCell<Vec<(u64, joinpath::PwJoin)>>,
    /// WHERE clauses a join group takes in (`plan_filtered_join`), for the next group.
    absorbed: std::cell::RefCell<Vec<Expr>>,
    /// The per-row cost of a filter over an outer join that its group didn't take in: the
    /// join's restriction list has it in Postgres (joinrestrictinfo), for the next group.
    post_cost: std::cell::Cell<f64>,
    /// The ORDER BY over the grouping below: per key, (group key, descending, nulls
    /// first), for the order of the GROUP BY (`joinpath::grouped_join`).
    group_dirs: std::cell::RefCell<Option<GroupDirs>>,
    /// Planning a join group under a grouping that may go partitionwise.
    pw_agg_join: std::cell::Cell<bool>,
    /// Planning a join group's alternative with some of its items joined partitionwise
    /// first (`joinpath::partitionwise`'s `pw_subset`): no nested alternatives.
    pw_alt: std::cell::Cell<bool>,
    /// Planning what is below a join group or inside one (its items, a partitionwise
    /// join's child joins): a join group there isn't its query level's topmost scan/join
    /// relation.
    join_nested: std::cell::Cell<bool>,
    /// Planning a MERGE's source join: its target table (the first relation of Postgres's
    /// range table, so the first one the join search tries).
    merge_target: std::cell::Cell<Option<u32>>,
    /// Restriction clauses reading a PlaceHolderVar, by table (`ScanPlan::phv_quals`):
    /// never index quals.
    phv_quals: std::cell::RefCell<Vec<(u32, Expr)>>,
    /// Planning a join group whose subqueries' outputs were lifted (`joinpath::lift`).
    /// (Its address, only while that group is planned, when no other plan can have it: a
    /// group inside it is lifted for itself.)
    lifting: std::cell::Cell<usize>,
    /// This planning's id, unique in the process (`JoinMeta::lifted_by`).
    id: u64,
    /// The projections computing lifted subquery outputs over join groups, by their
    /// inputs (a Weak keeps an allocation's address from going to another plan).
    lifted: std::cell::RefCell<Vec<std::sync::Weak<Plan>>>,
    /// Planning a partitionwise join's child join: the parent group's leaves' rows, which
    /// its parameterized paths' loop counts come from (a child relation's are made
    /// parameterized by the parent relations, then reparameterized by child).
    pw_loop_rows: std::cell::RefCell<Vec<f64>>,
    /// Planning a partitionwise join's child join: the sets of the parent group's leaves
    /// whose join relations are partitioned (`Planner::pw_partitioned_sets`); the child
    /// search makes join relations of those only (a child join relation exists only for a
    /// partitioned parent one, try_partitionwise_join), and the parent's leaf count.
    pw_rels: std::cell::RefCell<Option<(usize, Vec<u32>)>>,
    /// Walking a LATERAL join's inner relation: the outer side's rows (its scans' loops).
    lateral_loops: std::cell::Cell<Option<f64>>,
    /// Planning a partitionwise join's child joins under a LIMIT (no ORDER BY): its count
    /// and offset, each child's cheapest path for that fraction (get_cheapest_fractional_path).
    pw_limit: std::cell::Cell<Option<(f64, f64)>>,
    /// Planning a partitionwise join's child joins under an ORDER BY: the order, each child
    /// planned for it too (a Merge Append of them, generate_orderedappend_paths).
    pw_order: std::cell::RefCell<Option<joinpath::Order>>,
}

impl<'c, 'a> Planner<'c, 'a> {
    fn new(ctx: &'c ExecCtx<'a>) -> Self {
        Planner {
            ctx,
            cat: ctx.catalog(),
            g: Gucs::read(ctx),
            ends: Default::default(),
            removed: Default::default(),
            whole_rows: Default::default(),
            ref_marks: Default::default(),
            copy_marks: Default::default(),
            pw_joins: Default::default(),
            absorbed: Default::default(),
            post_cost: Default::default(),
            group_dirs: Default::default(),
            pw_agg_join: Default::default(),
            pw_alt: Default::default(),
            join_nested: Default::default(),
            merge_target: Default::default(),
            phv_quals: Default::default(),
            lifting: Default::default(),
            id: PLANNER_IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            lifted: Default::default(),
            pw_loop_rows: Default::default(),
            pw_rels: Default::default(),
            lateral_loops: Default::default(),
            pw_limit: Default::default(),
            pw_order: Default::default(),
        }
    }

    /// get_actual_variable_range: the column's current min or max, when a btree index
    /// (default operator class, no predicate) has it as its first column.
    fn endpoint(&self, v: &VarRef, min: bool) -> Option<Value> {
        if v.expr.is_some() {
            return None;
        }
        let key = (v.table.oid, v.col);
        let cached = self.ends.borrow().get(&key).cloned();
        let r = match cached {
            Some(r) => r,
            None => {
                let t = &v.table;
                let indexed = self
                    .cat
                    .table_indexes(t)
                    .iter()
                    .chain(t.pkey_index.and_then(|p| self.cat.index(p)))
                    .any(|ix| {
                        ix.valid
                            && ix.method == "btree"
                            && ix.predicate.is_none()
                            && ix.columns.first().is_some_and(|c| {
                                matches!(c.key, crate::catalog::IndexKey::Column(k) if k == v.col)
                                    && !matches!(c.opclass, 4217..=4219)
                            })
                    });
                let r = if indexed {
                    crate::exec::scan::column_range(t, v.col, self.ctx)
                        .ok()
                        .flatten()
                } else {
                    None
                };
                self.ends.borrow_mut().insert(key, r.clone());
                r
            }
        };
        r.map(|(a, b)| if min { a } else { b })
    }
}

/// IndexClause selectivity input: the qual as the index applies it.
fn qual_exprs(cs: &[IClause]) -> Vec<Expr> {
    cs.iter().map(|c| c.expr.clone()).collect()
}

/// estimate_array_length.
fn array_len(e: &Expr) -> f64 {
    match e {
        Expr::Const(Value::Array(a), _) => a.items.len() as f64,
        Expr::Const(Value::Null, _) => 0.0,
        Expr::Array(items, _) => items.len() as f64,
        Expr::Relabel(x, _) => array_len(x),
        _ => 10.0,
    }
}

/// What amcostestimate reports.
struct AmCost {
    startup: f64,
    total: f64,
    sel: f64,
    correlation: f64,
    /// The index pages a scan reads (genericcostestimate's numIndexPages).
    pages: f64,
}

impl Planner<'_, '_> {
    fn sel(&self, rel: &Rel, quals: &[Expr]) -> f64 {
        // Conditions on the row's ctid have estimates of their own (ctid_sel); range pairs
        // combine as clauselist_selectivity's do.
        if quals.iter().any(|q| ctid_sel(rel, q).is_some()) {
            let (mut eq, mut lo, mut hi) = (1.0, None::<f64>, None::<f64>);
            let mut rest = Vec::new();
            for q in quals {
                match ctid_sel(rel, q) {
                    Some((s, Some(true))) => lo = Some(lo.map_or(s, |l: f64| l.min(s))),
                    Some((s, Some(false))) => hi = Some(hi.map_or(s, |h: f64| h.min(s))),
                    Some((s, None)) => eq *= s,
                    None => rest.push(q.clone()),
                }
            }
            let range = match (lo, hi) {
                (Some(l), Some(h)) => {
                    let s2 = h + l - 1.0;
                    if s2 > 0.0 {
                        s2
                    } else if s2 < -0.01 {
                        RANGE_INEQ_SEL
                    } else {
                        1.0e-10
                    }
                }
                (Some(x), None) | (None, Some(x)) => x,
                (None, None) => 1.0,
            };
            let rest_sel = if rest.is_empty() { 1.0 } else { self.sel(rel, &rest) };
            return eq * range * rest_sel;
        }
        let vars = rel.vars();
        let est = Est {
            cat: &self.cat,
            fmt: self.ctx.fmt(),
            vars: &vars,
            exprs: &rel.exprs,
            endpoint: Some(&|v, min| self.endpoint(v, min)),
            distinct: None,
            user: Some(&costs::UserOps {
                ctx: self.ctx,
                acl: &|v: &VarRef| {
                    if v.expr.is_some() {
                        rel.acl.0
                    } else {
                        rel.acl.1.get(v.col).copied().unwrap_or(true)
                    }
                },
                hidden: &rel.hidden,
            }),
            same_rel: true,
        };
        // (With the table's extended statistics where they cover the clauses.)
        crate::sql::extstat::ExtSel::new(self.ctx, &est, &rel.table).clauses(quals, true)
    }

    /// index_pages_fetched (Mackert and Lohman).
    fn pages_fetched(&self, tuples: f64, pages: f64, index_pages: f64, table_pages: f64) -> f64 {
        let t = if pages > 1.0 { pages } else { 1.0 };
        let total = (table_pages + index_pages).max(1.0);
        let b = self.g.cache_pages * t / total;
        let b = if b <= 1.0 { 1.0 } else { b.ceil() };
        if t <= b {
            let p = (2.0 * t * tuples) / (2.0 * t + tuples);
            if p >= t { t } else { p.ceil() }
        } else {
            let lim = (2.0 * t * b) / (2.0 * t - b);
            let p = if tuples <= lim {
                (2.0 * t * tuples) / (2.0 * t + tuples)
            } else {
                b + (tuples - lim) * (t - b) / t
            };
            p.ceil()
        }
    }

    /// The statistics-based correlation of an index's first column (btcost_correlation).
    fn correlation(&self, rel: &Rel, ii: &IndexInfo) -> f64 {
        let Some(Expr::Col(c, _)) = ii.keys.first() else {
            return 0.0;
        };
        let mut corr = costs::correlation(&self.cat, rel.table.oid, *c);
        if ii.ix.columns.first().is_some_and(|c| c.desc) {
            corr = -corr;
        }
        if ii.keys.len() > 1 { corr * 0.75 } else { corr }
    }

    /// genericcostestimate, given btcostestimate's index tuple and descent estimates.
    fn generic(
        &self,
        rel: &Rel,
        ii: &IndexInfo,
        quals: &[IClause],
        num_index_tuples: f64,
        num_sa_scans: f64,
        loop_count: f64,
        orderbys: usize,
    ) -> (AmCost, f64) {
        let mut selq: Vec<Expr> = Vec::new();
        let qexprs = qual_exprs(quals);
        for p in &ii.pred {
            if !implied_by(std::slice::from_ref(p), &qexprs) {
                selq.push(p.clone());
            }
        }
        selq.extend(qexprs.iter().cloned());
        let mut num_sa_scans = num_sa_scans;
        if num_sa_scans < 1.0 {
            num_sa_scans = 1.0;
            for q in quals {
                if let (Kind::Saop(_), Some(v)) = (&q.kind, &q.value) {
                    let n = array_len(v);
                    if n > 1.0 {
                        num_sa_scans *= n;
                    }
                }
            }
        }
        let sel = self.sel(rel, &selq);
        let mut tuples = num_index_tuples;
        if tuples <= 0.0 {
            tuples = (sel * rel.size.tuples / num_sa_scans).round_ties_even();
        }
        if tuples > ii.tuples {
            tuples = ii.tuples;
        }
        if tuples < 1.0 {
            tuples = 1.0;
        }
        let num_pages = if ii.pages > 1.0 && ii.tuples > 1.0 {
            (tuples * ii.pages / ii.tuples).ceil()
        } else {
            1.0
        };
        let num_scans = num_sa_scans * loop_count;
        let mut total = if num_scans > 1.0 {
            let pf = self.pages_fetched(num_pages * num_scans, ii.pages, ii.pages, rel.size.pages);
            pf * self.g.random_page_cost / loop_count
        } else {
            num_pages * self.g.random_page_cost
        };
        let arg_cost = self.other_operands_cost(rel, quals);
        // (The reference build fuses multiply-adds, as GCC does on aarch64.)
        let per_tuple =
            CPU_OPERATOR_COST.mul_add((quals.len() + orderbys) as f64, CPU_INDEX_TUPLE_COST);
        let startup = arg_cost;
        total += arg_cost;
        total = (tuples * num_sa_scans).mul_add(per_tuple, total);
        (
            AmCost {
                startup,
                total,
                sel,
                correlation: 0.0,
                pages: num_pages,
            },
            num_sa_scans,
        )
    }

    /// btcostestimate.
    fn btcost(&self, rel: &Rel, ii: &IndexInfo, quals: &[IClause], loop_count: f64) -> AmCost {
        let nkey = ii.keys.len();
        let mut bound: Vec<Expr> = Vec::new();
        let mut skip: Vec<Expr> = Vec::new();
        let mut indexcol = 0usize;
        let mut eq_here = false;
        let mut found_array = false;
        let mut found_null = false;
        let mut num_sa_scans = 1.0;
        let mut correlation = None;
        'outer: for q in quals {
            if indexcol < q.col {
                let prev = num_sa_scans;
                if eq_here {
                    indexcol += 1;
                    skip.clear();
                }
                eq_here = false;
                while indexcol < q.col {
                    found_array = true;
                    let (mut nd, default) = match &ii.keys[indexcol] {
                        Expr::Col(c, _) => VarRef {
                            table: rel.table.clone(),
                            col: *c,
                            tuples: rel.size.tuples,
                            expr: None,
                            rows: 0.0,
                            inh: false,
                        }
                        .ndistinct(&self.cat),
                        _ => (costs::DEFAULT_NUM_DISTINCT, true),
                    };
                    if indexcol == 0 {
                        correlation = Some(self.correlation(rel, ii));
                    }
                    if default {
                        num_sa_scans = prev;
                        break;
                    }
                    if !skip.is_empty() {
                        let mut sq = ii.pred.clone();
                        sq.extend(skip.iter().cloned());
                        let frac = self.sel(rel, &sq);
                        if frac < RANGE_INEQ_SEL {
                            num_sa_scans = prev;
                            break;
                        }
                        nd = (nd * frac).round_ties_even().max(1.0);
                    }
                    if skip.is_empty() {
                        nd += 1.0;
                    }
                    num_sa_scans *= nd;
                    if ii.pages < num_sa_scans {
                        num_sa_scans = prev;
                        break;
                    }
                    indexcol += 1;
                    skip.clear();
                }
                if indexcol != q.col {
                    break 'outer;
                }
            }
            match &q.kind {
                Kind::Op(op) => {
                    if *op == "=" {
                        eq_here = true;
                    }
                }
                Kind::Saop(op) => {
                    found_array = true;
                    if let Some(v) = &q.value {
                        let n = array_len(v);
                        if n > 1.0 {
                            num_sa_scans *= n;
                        }
                    }
                    if *op == "=" {
                        eq_here = true;
                    }
                }
                Kind::Null(is_null) => {
                    if *is_null {
                        found_null = true;
                        eq_here = true;
                    }
                }
            }
            bound.push(q.expr.clone());
            if !eq_here && indexcol + 1 < nkey {
                skip.push(q.expr.clone());
            }
        }
        let num_index_tuples = if (ii.ix.unique || ii.ix.primary)
            && indexcol + 1 == nkey
            && eq_here
            && !found_array
            && !found_null
        {
            1.0
        } else {
            let mut sq: Vec<Expr> = Vec::new();
            for p in &ii.pred {
                if !implied_by(std::slice::from_ref(p), &bound) {
                    sq.push(p.clone());
                }
            }
            sq.extend(bound.iter().cloned());
            let s = self.sel(rel, &sq);
            let t = s * rel.size.tuples;
            num_sa_scans = num_sa_scans.min((ii.pages * 0.3333333).ceil()).max(1.0);
            (t / num_sa_scans).round_ties_even()
        };
        let (mut c, num_sa_scans) =
            self.generic(rel, ii, quals, num_index_tuples, num_sa_scans, loop_count, 0);
        if ii.tuples > 1.0 {
            let d = (ii.tuples.ln() / 2f64.ln()).ceil() * CPU_OPERATOR_COST;
            c.startup += d;
            c.total = num_sa_scans.mul_add(d, c.total);
        }
        let d = (ii.height as f64 + 1.0) * PAGE_CPU_MULTIPLIER * CPU_OPERATOR_COST;
        c.startup += d;
        c.total = num_sa_scans.mul_add(d, c.total);
        c.correlation = correlation.unwrap_or_else(|| self.correlation(rel, ii));
        c
    }

    /// gistcostestimate / spgcostestimate: genericcostestimate, plus the descent's
    /// comparisons and pages (a fanout of 100 assumed for the tree's height).
    fn gistcost(
        &self,
        rel: &Rel,
        ii: &IndexInfo,
        quals: &[IClause],
        loop_count: f64,
        orderbys: usize,
    ) -> AmCost {
        let (mut c, num_sa_scans) = self.generic(rel, ii, quals, 0.0, 0.0, loop_count, orderbys);
        let height = if ii.pages > 1.0 {
            (ii.pages.ln() / 100f64.ln()) as i32
        } else {
            0
        };
        if ii.tuples > 1.0 {
            let descent = ii.tuples.ln().ceil() * CPU_OPERATOR_COST;
            c.startup += descent;
            c.total += num_sa_scans * descent;
        }
        let descent = (height + 1) as f64 * PAGE_CPU_MULTIPLIER * CPU_OPERATOR_COST;
        c.startup += descent;
        c.total += num_sa_scans * descent;
        c
    }

    /// brincostestimate: the ranges the conditions' selectivity touches given the best
    /// correlation of a column they use; the revmap read sequentially, the rest of the
    /// index at random.
    fn brincost(&self, rel: &Rel, ii: &IndexInfo, quals: &[IClause], loop_count: f64) -> AmCost {
        let ppr = crate::exec::sizing::brin_pages_per_range(&ii.ix) as f64;
        let index_ranges = (rel.size.pages / ppr).ceil().max(1.0);
        // REVMAP_PAGE_MAXITEMS: the revmap's pages are those after the metapage up to
        // the last one the ranges need.
        let revmap_pages = (index_ranges / 1360.0).floor() + 1.0;
        let mut corr: f64 = 0.0;
        for q in quals {
            let c = match ii.keys.get(q.col) {
                Some(Expr::Col(c, _)) => costs::correlation(&self.cat, rel.table.oid, *c),
                _ => costs::correlation(&self.cat, ii.ix.oid, q.col),
            };
            corr = corr.max(c.abs());
        }
        let qual_sel = self.sel(rel, &qual_exprs(quals));
        let minimal = (index_ranges * qual_sel).ceil();
        let estimated = if corr < 1.0e-10 {
            index_ranges
        } else {
            (minimal / corr).min(index_ranges)
        };
        let sel = (estimated / index_ranges).clamp(0.0, 1.0);
        let arg_cost = self.other_operands_cost(rel, quals);
        let startup = self.g.seq_page_cost * revmap_pages * loop_count + arg_cost;
        let mut total =
            startup + self.g.random_page_cost * (ii.pages - revmap_pages) * loop_count;
        total += 0.1 * CPU_OPERATOR_COST * estimated * ppr;
        AmCost {
            startup,
            total,
            sel,
            correlation: corr,
            pages: ii.pages,
        }
    }

    fn amcost(
        &self,
        rel: &Rel,
        ii: &IndexInfo,
        quals: &[IClause],
        loop_count: f64,
        orderbys: usize,
    ) -> AmCost {
        match ii.am {
            Am::Btree => self.btcost(rel, ii, quals, loop_count),
            Am::Hash | Am::Gin => self.generic(rel, ii, quals, 0.0, 0.0, loop_count, 0).0,
            Am::Gist | Am::Spgist => self.gistcost(rel, ii, quals, loop_count, orderbys),
            Am::Brin => self.brincost(rel, ii, quals, loop_count),
        }
    }

    /// cost_index.
    fn index_path(
        &self,
        rel: &Rel,
        ix: usize,
        clauses: Vec<IClause>,
        keys: Vec<PathKey>,
        backward: bool,
        only: bool,
    ) -> IPath {
        self.index_path_ob(rel, ix, clauses, keys, Vec::new(), backward, only)
    }

    /// cost_index of a scan that also orders its rows by `orderbys` (ordering operators
    /// over index columns: a GiST or SP-GiST nearest-neighbour search).
    #[allow(clippy::too_many_arguments)]
    fn index_path_ob(
        &self,
        rel: &Rel,
        ix: usize,
        clauses: Vec<IClause>,
        keys: Vec<PathKey>,
        orderbys: Vec<Expr>,
        backward: bool,
        only: bool,
    ) -> IPath {
        let ii = &rel.indexes[ix];
        // (One column's conditions in baserestrictinfo order; a parameterized path's join
        // clauses before them, as get_join_index_paths collects them.)
        let mut clauses = clauses;
        if !rel.written.is_empty() {
            let rank = |c: &IClause| rel.written.iter().position(|w| w.same_as(&c.orig));
            let ranks: Vec<(usize, usize, usize)> = clauses
                .iter()
                .enumerate()
                .map(|(i, c)| match rank(c) {
                    Some(r) => (c.col, 1, r),
                    None if c.orig.has_outer_refs() => (c.col, 0, i),
                    None => (c.col, 1, usize::MAX / 2 + i),
                })
                .collect();
            let mut idx: Vec<usize> = (0..clauses.len()).collect();
            idx.sort_by_key(|&i| ranks[i]);
            let mut old: Vec<Option<IClause>> = clauses.into_iter().map(Some).collect();
            clauses = idx.into_iter().filter_map(|i| old[i].take()).collect();
        }
        let loop_count = rel.loop_count;
        let am = self.amcost(rel, ii, &clauses, loop_count, orderbys.len());
        let mut startup = am.startup;
        let mut run = am.total - am.startup;
        let tuples_fetched = costs::clamp_rows(am.sel * rel.size.tuples);
        let pages = rel.size.pages;
        let rpc = self.g.random_page_cost;
        let spc = self.g.seq_page_cost;
        let vis = |pf: f64| {
            if only {
                (pf * (1.0 - rel.allvisfrac)).ceil()
            } else {
                pf
            }
        };
        let mut rand_pages = -1.0;
        let (max_io, min_io) = if loop_count > 1.0 {
            // Repeated scans: the pages all of them fetch, pro-rated.
            let pf = vis(self.pages_fetched(tuples_fetched * loop_count, pages, ii.pages, pages));
            let max_io = (pf * rpc) / loop_count;
            let pf = (am.sel * pages).ceil();
            let pf = vis(self.pages_fetched(pf * loop_count, pages, ii.pages, pages));
            (max_io, (pf * rpc) / loop_count)
        } else {
            let pf = vis(self.pages_fetched(tuples_fetched, pages, ii.pages, pages));
            if !only {
                rand_pages = pf;
            }
            let max_io = pf * rpc;
            let pf = vis((am.sel * pages).ceil());
            let min_io = if pf > 0.0 {
                if pf > 1.0 {
                    (pf - 1.0).mul_add(spc, rpc)
                } else {
                    rpc
                }
            } else {
                0.0
            };
            (max_io, min_io)
        };
        let csq = am.correlation * am.correlation;
        run += csq.mul_add(min_io - max_io, max_io);
        // The quals the index doesn't enforce (nor its predicate).
        let qpquals: Vec<Expr> = rel
            .quals
            .iter()
            .enumerate()
            .filter(|(n, q)| {
                !clauses.iter().any(|c| c.qual == *n && !c.lossy) && !ii.implies(q)
            })
            .map(|(_, q)| q.clone())
            .collect();
        let (qs, qp) = self.rel_quals_cost(rel, &qpquals);
        startup += qs;
        let cpu_run = (CPU_TUPLE_COST + qp) * tuples_fetched;
        run += cpu_run;
        IPath {
            orderbys,
            cpu_run,
            rand_pages,
            index_pages: am.pages,
            ix,
            clauses,
            backward,
            keys,
            only,
            startup,
            total: startup + run,
            index_total: am.total,
            index_sel: am.sel,
            rows: rel.rows,
            disabled: (!self.g.indexscan) as u32,
        }
    }

    /// compute_bitmap_pages + cost_bitmap_heap_scan: (startup, total, pages, tuples).
    fn bitmap_heap(&self, rel: &Rel, b: &BPath) -> (f64, f64, f64, f64) {
        let (index_cost, sel) = b.cost_sel();
        let mut tuples = costs::clamp_rows(sel * rel.size.tuples);
        let t = if rel.size.pages > 1.0 { rel.size.pages } else { 1.0 };
        let mut pages = (2.0 * t * tuples) / 2.0f64.mul_add(t, tuples);
        let heap_pages = pages.min(rel.size.pages);
        // tbm_calculate_entries: 64 bytes per page entry.
        let maxentries = (self.g.work_mem * 1024.0 / 64.0).floor().max(16.0);
        // Repeated scans (a nested loop's parameterized inner side): the pages all of them
        // fetch, pro-rated (the bitmap still holds one scan's).
        if rel.loop_count > 1.0 {
            pages = self.pages_fetched(tuples * rel.loop_count, rel.size.pages, b.index_pages(rel), rel.size.pages)
                / rel.loop_count;
        }
        if pages >= t {
            pages = t;
        } else {
            pages = pages.ceil();
        }
        if maxentries < heap_pages {
            let lossy = (heap_pages - maxentries / 2.0).max(0.0);
            let exact = heap_pages - lossy;
            if lossy > 0.0 {
                tuples = costs::clamp_rows(
                    sel * (exact / heap_pages) * rel.size.tuples
                        + (lossy / heap_pages) * rel.size.tuples,
                );
            }
        }
        let rpc = self.g.random_page_cost;
        let spc = self.g.seq_page_cost;
        let cost_per_page = if pages >= 2.0 {
            (-(rpc - spc)).mul_add((pages / t).sqrt(), rpc)
        } else {
            rpc
        };
        let startup = index_cost + rel.qual_startup;
        let run = pages * cost_per_page + (CPU_TUPLE_COST + rel.qual_cost) * tuples;
        (startup, startup + run, pages, tuples)
    }

    fn bitmap_cost(&self, rel: &Rel, b: &BPath) -> f64 {
        self.bitmap_heap(rel, b).1
    }

    fn bitmap_and(&self, paths: Vec<BPath>) -> BPath {
        let mut cost = 0.0;
        let mut sel = 1.0;
        for (i, p) in paths.iter().enumerate() {
            let (c, s) = p.cost_sel();
            sel *= s;
            cost += c;
            if i > 0 {
                cost = 100f64.mul_add(CPU_OPERATOR_COST, cost);
            }
        }
        BPath::And(paths, cost, sel)
    }

    fn bitmap_or(&self, paths: Vec<BPath>) -> BPath {
        let mut cost = 0.0;
        let mut sel = 0.0;
        for (i, p) in paths.iter().enumerate() {
            let (c, s) = p.cost_sel();
            sel += s;
            cost += c;
            if i > 0 && !matches!(p, BPath::Index(_)) {
                cost = 100f64.mul_add(CPU_OPERATOR_COST, cost);
            }
        }
        BPath::Or(paths, cost, sel.min(1.0))
    }

    /// choose_bitmap_and.
    fn choose_bitmap_and(&self, rel: &Rel, paths: Vec<BPath>) -> BPath {
        if paths.len() == 1 {
            return paths.into_iter().next().unwrap();
        }
        // (path, clause ids, quals, preds)
        let mut seen: Vec<Expr> = Vec::new();
        let mut infos: Vec<(BPath, Vec<usize>, Vec<Expr>, Vec<Expr>)> = Vec::new();
        for p in paths {
            let (mut q, mut pr) = (Vec::new(), Vec::new());
            p.quals(rel, &mut q, &mut pr);
            let mut ids: Vec<usize> = Vec::new();
            for e in q.iter().chain(&pr) {
                let id = match seen.iter().position(|s| s.same_as(e)) {
                    Some(i) => i,
                    None => {
                        seen.push(e.clone());
                        seen.len() - 1
                    }
                };
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            ids.sort();
            match infos.iter().position(|x| x.1 == ids) {
                Some(i) => {
                    if p.cost_sel().0 < infos[i].0.cost_sel().0 {
                        infos[i] = (p, ids, q, pr);
                    }
                }
                None => infos.push((p, ids, q, pr)),
            }
        }
        if infos.len() == 1 {
            return infos.pop().unwrap().0;
        }
        // path_usage_comparator (qsort; ties keep no particular order, as there).
        infos.sort_by(|a, b| {
            let (ac, asel) = a.0.cost_sel();
            let (bc, bsel) = b.0.cost_sel();
            ac.partial_cmp(&bc)
                .unwrap_or(Ordering::Equal)
                .then(asel.partial_cmp(&bsel).unwrap_or(Ordering::Equal))
        });
        let mut best: Option<(Vec<BPath>, f64)> = None;
        for i in 0..infos.len() {
            let mut group = vec![infos[i].0.clone()];
            let mut cost = self.bitmap_cost(rel, &infos[i].0);
            let mut qualsofar: Vec<Expr> = infos[i].2.clone();
            qualsofar.extend(infos[i].3.iter().cloned());
            let mut ids = infos[i].1.clone();
            for info in infos.iter().skip(i + 1) {
                if info.1.iter().any(|x| ids.contains(x)) {
                    continue;
                }
                if info
                    .3
                    .iter()
                    .any(|p| implied_by(std::slice::from_ref(p), &qualsofar))
                {
                    continue;
                }
                let mut trial = group.clone();
                trial.push(info.0.clone());
                let and = self.bitmap_and(trial.clone());
                let c = self.bitmap_cost(rel, &and);
                if c < cost {
                    cost = c;
                    group = trial;
                    qualsofar.extend(info.2.iter().cloned());
                    qualsofar.extend(info.3.iter().cloned());
                    ids.extend(info.1.iter().copied());
                }
            }
            if i == 0 || best.as_ref().is_some_and(|b| cost < b.1) {
                best = Some((group, cost));
            }
        }
        let (group, _) = best.unwrap();
        if group.len() == 1 {
            group.into_iter().next().unwrap()
        } else {
            self.bitmap_and(group)
        }
    }

    /// build_paths_for_OR: bitmap index paths for one arm of an OR (`cur` must be used).
    fn paths_for_or(&self, rel: &Rel, cur: &[Expr], other: &[Expr]) -> Vec<BPath> {
        let mut out = Vec::new();
        let mut all: Vec<Expr> = cur.to_vec();
        all.extend(other.iter().cloned());
        for (ix, ii) in rel.indexes.iter().enumerate() {
            let mut useful_pred = false;
            if !ii.pred.is_empty() {
                if implied_by(&ii.pred, &rel.quals) {
                    // usable; not useful for the arm
                } else {
                    if !implied_by(&ii.pred, &all) {
                        continue;
                    }
                    if !implied_by(&ii.pred, other) {
                        useful_pred = true;
                    }
                }
            }
            // The arm's clauses, then the others, matched to the index (as a rel of quals
            // `all`: positions in `all`).
            let ncur = cur.len();
            let tmp = Rel {
                quals: all.clone(),
                ..rel.shallow()
            };
            let cs = match_quals(ii, &tmp.quals, None, rel.booleq.as_ref(), self);
            if !cs.iter().any(|c| c.qual < ncur) && !useful_pred {
                continue;
            }
            if cs.is_empty() && !ii.am.optionalkey() {
                continue;
            }
            let p = self.index_path(&tmp, ix, cs, Vec::new(), false, false);
            out.push(BPath::Index(self.rebase(rel, &all, p)));
        }
        out
    }

    /// An index path costed over quals `all`, its clause positions made `rel`'s.
    fn rebase(&self, rel: &Rel, all: &[Expr], mut p: IPath) -> IPath {
        for c in &mut p.clauses {
            let q = &all[c.qual];
            c.qual = rel
                .quals
                .iter()
                .position(|x| x.same_as(q))
                .unwrap_or(usize::MAX);
        }
        p.rows = rel.rows;
        p
    }


    /// generate_bitmap_or_paths.
    fn bitmap_or_paths(&self, rel: &Rel, clauses: &[Expr], other: &[Expr]) -> Vec<BPath> {
        let mut out = Vec::new();
        let mut all: Vec<Expr> = clauses.to_vec();
        all.extend(other.iter().cloned());
        for c in clauses {
            let Expr::Or(arms) = c else { continue };
            let mut pathlist = Vec::new();
            let mut ok = true;
            for arm in group_or_args(rel, arms) {
                let indlist = match &arm {
                    OrArg::One(Expr::And(list)) => {
                        let mut v = self.paths_for_or(rel, list, &all);
                        v.extend(self.bitmap_or_paths(rel, list, &all));
                        v
                    }
                    OrArg::One(e) => self.paths_for_or(rel, std::slice::from_ref(e), &all),
                    OrArg::Group(list) => {
                        let others: Vec<Expr> =
                            all.iter().filter(|x| !x.same_as(c)).cloned().collect();
                        let v = self.or_group(rel, list, &others);
                        if v.is_empty() {
                            ok = false;
                            break;
                        }
                        pathlist.extend(v);
                        continue;
                    }
                };
                if indlist.is_empty() {
                    ok = false;
                    break;
                }
                pathlist.push(self.choose_bitmap_and(rel, indlist));
            }
            if ok && !pathlist.is_empty() {
                out.push(self.bitmap_or(pathlist));
            }
        }
        out
    }

    /// make_bitmap_paths_for_or_group: the group as one `= ANY`, or arm by arm.
    fn or_group(&self, rel: &Rel, args: &[Expr], other: &[Expr]) -> Vec<BPath> {
        let whole = Expr::Or(args.to_vec());
        let mut joint = Vec::new();
        let mut jointcost = 0.0;
        let ind = self.paths_for_or(rel, std::slice::from_ref(&whole), other);
        if !ind.is_empty() {
            let b = self.choose_bitmap_and(rel, ind);
            jointcost = b.cost_sel().0;
            joint.push(b);
        }
        if !joint.is_empty() && other.is_empty() {
            return joint;
        }
        let mut split = Vec::new();
        let mut splitcost = 0.0;
        for a in args {
            let ind = self.paths_for_or(rel, std::slice::from_ref(a), other);
            if ind.is_empty() {
                split.clear();
                break;
            }
            let b = self.choose_bitmap_and(rel, ind);
            splitcost += b.cost_sel().0;
            split.push(b);
        }
        if split.is_empty() {
            joint
        } else if joint.is_empty() || splitcost <= jointcost {
            split
        } else {
            joint
        }
    }
}

enum OrArg {
    One(Expr),
    Group(Vec<Expr>),
}

/// group_similar_or_args: OR arms `key op const` on the same index column with the same
/// operator gather into one group (which may match the index as `= ANY`).
fn group_or_args(rel: &Rel, arms: &[Expr]) -> Vec<OrArg> {
    // (index, column, operator, type) per arm.
    let mut sig: Vec<Option<(usize, usize, &'static str, DataType)>> = Vec::new();
    for a in arms {
        let mut s = None;
        if let Expr::Call(d, args, _) = a
            && args.len() == 2
        {
            let (k, v, op) = if args[1].columns().is_empty() && !args[1].is_volatile_call() {
                (&args[0], &args[1], d.name)
            } else if args[0].columns().is_empty() && !args[0].is_volatile_call() {
                (&args[1], &args[0], commute(d.name))
            } else {
                (&args[0], &args[0], "")
            };
            if !op.is_empty() && !k.columns().is_empty() {
                'ix: for (n, ii) in rel.indexes.iter().enumerate() {
                    if ii.am != Am::Btree {
                        continue;
                    }
                    for (col, key) in ii.keys.iter().enumerate() {
                        if matches_key(k, key) {
                            s = Some((n, col, op, v.ty()));
                            break 'ix;
                        }
                    }
                }
            }
        }
        sig.push(s);
    }
    if sig.iter().all(Option::is_none) {
        return arms.iter().cloned().map(OrArg::One).collect();
    }
    let mut out = Vec::new();
    let mut used = vec![false; arms.len()];
    for i in 0..arms.len() {
        if used[i] {
            continue;
        }
        used[i] = true;
        let mut group = vec![arms[i].clone()];
        if let Some(s) = &sig[i] {
            for j in i + 1..arms.len() {
                if !used[j] && sig[j].as_ref() == Some(s) {
                    used[j] = true;
                    group.push(arms[j].clone());
                }
            }
        }
        out.push(if group.len() == 1 {
            OrArg::One(group.pop().unwrap())
        } else {
            OrArg::Group(group)
        });
    }
    out
}

impl Rel {
    /// The same relation (cheap parts), for re-matching clauses.
    fn shallow(&self) -> Rel {
        Rel {
            table: self.table.clone(),
            size: self.size,
            allvisfrac: self.allvisfrac,
            quals: Vec::new(),
            rows: self.rows,
            qual_cost: self.qual_cost,
            qual_startup: self.qual_startup,
            needed: self.needed.clone(),
            other: self.other,
            indexes: self.indexes.clone(),
            loop_count: self.loop_count,
            exprs: self.exprs.clone(),
            booleq: self.booleq.clone(),
            written: self.written.clone(),
            acl: self.acl.clone(),
            hidden: self.hidden.clone(),
            params: self.params.clone(),
        }
    }
}

/// The value a clause on column `col` equates the key to, if it is `key = const`.
fn const_equated(rel: &Rel, key: &Expr) -> bool {
    eq_const(rel, key) || bool_const(rel, key)
}

/// An equality of `key` with a constant among the relation's clauses: an equivalence class
/// with a constant, making `key` redundant as a sort key.
fn eq_const(rel: &Rel, key: &Expr) -> bool {
    rel.quals.iter().any(|q| match q {
        Expr::Call(d, args, _) if d.name == "=" && args.len() == 2 => {
            (matches_key(&args[0], key) && scan_const(&args[1]))
                || (matches_key(&args[1], key) && scan_const(&args[0]))
        }
        _ => false,
    })
}

/// A boolean `key` the relation's clauses fix (no equivalence class).
fn bool_const(rel: &Rel, key: &Expr) -> bool {
    let boolean = key.ty() == DataType::Bool;
    rel.quals.iter().any(|q| match q {
        Expr::Call(d, args, _) if d.name == "=" && args.len() == 2 => false,
        // indexcol_is_bool_constant_for_query: `col`, `NOT col`, `col IS TRUE`, `col IS
        // FALSE` on a boolean column.
        Expr::Not(x) | Expr::BoolTest(x, crate::sql::expr::BoolTest::IsTrue | crate::sql::expr::BoolTest::IsFalse)
            if boolean =>
        {
            matches_key(x, key)
        }
        q if boolean => matches_key(q, key),
        _ => false,
    })
}

/// build_index_pathkeys cut to the part useful for ordering the query (`want`).
fn index_pathkeys(rel: &Rel, ii: &IndexInfo, backward: bool, w: &Want) -> Vec<PathKey> {
    let want = &w.keys;
    if (want.is_empty() && w.merge.is_empty()) || ii.am != Am::Btree {
        return Vec::new();
    }
    let mut keys = Vec::new();
    for (i, k) in ii.keys.iter().enumerate() {
        if eq_const(rel, k) {
            continue;
        }
        let c = &ii.ix.columns[i];
        let pk = PathKey {
            expr: k.clone(),
            desc: c.desc != backward,
            nulls_first: c.nulls_first != backward,
            cmp: ii.order.get(i).and_then(|o| o.as_ref()).map_or(0, |o| o.0),
        };
        // Only keys the query sorts or merges by have an equivalence class to be a
        // pathkey of (a boolean one fixed by a clause has none: past it).
        if !want.iter().any(|x| same_key(&x.expr, k)) && !w.merge.iter().any(|m| same_key(m, k))
        {
            if bool_const(rel, k) {
                continue;
            }
            break;
        }
        keys.push(pk);
    }
    // truncate_useless_pathkeys: useful for the ORDER BY, or for merging (in the right
    // direction: the ORDER BY's for that column, else ascending).
    let ordering = keys
        .iter()
        .zip(want)
        .take_while(|(a, b)| a.same(b))
        .count();
    let merging = keys
        .iter()
        .take_while(|k| {
            let right_desc = want
                .iter()
                .find(|x| same_key(&x.expr, &k.expr))
                .is_some_and(|x| x.desc);
            k.cmp == 0 && k.desc == right_desc && w.merge.iter().any(|m| same_key(m, &k.expr))
        })
        .count();
    keys.truncate(ordering.max(merging));
    keys
}

/// match_pathkeys_to_index for an index with ordering operators (GiST, SP-GiST): the
/// leading ORDER BY keys (ascending, nulls last) that are `key op value` (or commuted)
/// with `op` an ordering operator of the column's family, and those expressions as the
/// index computes them (the key on the left).
fn order_by_ops(ii: &IndexInfo, w: &Want) -> (Vec<PathKey>, Vec<Expr>) {
    let (mut keys, mut exprs) = (Vec::new(), Vec::new());
    if !matches!(ii.am, Am::Gist | Am::Spgist) {
        return (keys, exprs);
    }
    'keys: for pk in &w.keys {
        if pk.desc || pk.nulls_first {
            break;
        }
        let Expr::Call(def, args, ty) = &pk.expr else {
            break;
        };
        if args.len() != 2 {
            break;
        }
        let fixed = |e: &Expr| e.columns().is_empty() && !e.is_volatile();
        for (col, key) in ii.keys.iter().enumerate() {
            let ops = &ii.fam_ops[col];
            let ordering = |oid: u32| {
                oid != 0 && ops.iter().any(|(o, p, sf)| *o == oid && *p == 'o' && *sf != 0)
            };
            if matches_key(&args[0], key) && fixed(&args[1]) && ordering(ii.op_oid(def)) {
                keys.push(pk.clone());
                exprs.push(pk.expr.clone());
                continue 'keys;
            }
            if matches_key(&args[1], key)
                && fixed(&args[0])
                && let Some((com, cdef)) = ii.commutator(def)
                && ordering(com)
            {
                keys.push(pk.clone());
                exprs.push(Expr::Call(
                    cdef,
                    vec![args[1].clone(), args[0].clone()],
                    ty.clone(),
                ));
                continue 'keys;
            }
        }
        break;
    }
    (keys, exprs)
}

/// The rows sorted by `keys` that are presorted by `have`: (all, count of leading ones).
fn presorted(want: &[PathKey], have: &[PathKey]) -> (bool, usize) {
    let n = want.iter().zip(have).take_while(|(a, b)| a.same(b)).count();
    (n == want.len(), n)
}

impl Planner<'_, '_> {
    /// cost_qual_eval over quals: (startup, per tuple), sub-selects included
    /// (cost_subplan): a hashed one builds its table once; a correlated one runs per row.
    /// quals_cost of a relation's clauses as Postgres sees them: a parameterized path's
    /// with the outer expressions in place of its parameters (get_restriction_qual_cost
    /// over the ppi_clauses, `Rel::unparam`).
    pub(super) fn rel_quals_cost(&self, rel: &Rel, quals: &[Expr]) -> (f64, f64) {
        if rel.params.is_empty() {
            return self.quals_cost(quals);
        }
        let quals: Vec<Expr> = quals.iter().map(|q| rel.unparam(q)).collect();
        self.quals_cost(&quals)
    }

    /// index_other_operands_eval_cost: evaluating the index conditions' other operands
    /// (once a scan, startup and per-tuple cost alike), a parameterized path's as the
    /// scan computes them from the outer Vars.
    fn other_operands_cost(&self, rel: &Rel, quals: &[IClause]) -> f64 {
        quals
            .iter()
            .filter_map(|q| q.value.as_ref())
            .map(|v| {
                let (s, p) = self.rel_quals_cost(rel, std::slice::from_ref(v));
                s + p
            })
            .sum()
    }

    pub(super) fn quals_cost(&self, quals: &[Expr]) -> (f64, f64) {
        let (mut startup, mut per_tuple) = (0.0, 0.0);
        for q in quals {
            per_tuple += costs::qual_cost(q);
            q.walk(&mut |e| {
                let Expr::Subquery(s) = e else { return };
                // (A function call is costed as one: its procost.)
                if s.call.is_some() {
                    return;
                }
                use crate::sql::expr::SubqueryKind as K;
                // (Correlated: its parameters read the scanned row. One reading only values
                // of enclosing query levels is an InitPlan of this level (SS_process_sublinks:
                // no Var of it), its Param free to read, charged once where it is attached.)
                let correlated = s.params.iter().any(|p| !p.columns().is_empty());
                let hashed =
                    !correlated && s.kind == K::Any && s.ops.iter().all(|o| o.name == "=");
                let ops = s.ops.len() as f64 * CPU_OPERATOR_COST;
                if hashed {
                    let est = crate::sql::explain::estimate_subquery(&s.plan, self.ctx);
                    startup += CPU_OPERATOR_COST.mul_add(est.rows, est.total);
                    per_tuple += ops;
                } else if correlated {
                    let est = crate::sql::explain::estimate_subquery(&s.plan, self.ctx);
                    let run = est.total - est.startup;
                    per_tuple += ops
                        + match s.kind {
                            K::Exists => run / costs::clamp_rows(est.rows),
                            K::Any | K::All => {
                                0.5 * run + 0.5 * est.rows * CPU_OPERATOR_COST
                            }
                            _ => run,
                        }
                        + est.startup;
                }
            });
        }
        (startup, per_tuple)
    }

    /// set_plain_rel_pathlist: the relation's paths after add_path.
    fn paths(&self, rel: &Rel, want: &Want) -> Vec<Path> {
        let startup = want.startup();
        let mut list = Vec::new();
        // cost_seqscan.
        let seq_total = rel.qual_startup
            + self.g.seq_page_cost * rel.size.pages
            + (CPU_TUPLE_COST + rel.qual_cost) * rel.size.tuples;
        add_path(
            &mut list,
            Path {
                kind: PathKind::Seq,
                startup: rel.qual_startup,
                total: seq_total,
                rows: rel.rows,
                keys: Vec::new(),
                disabled: (!self.g.seqscan) as u32,
            },
            startup,
        );
        // create_tidscan_paths.
        if let Some((quals, removed)) = tid_quals(rel) {
            let (st, tot) = self.tid_cost(rel, &quals);
            add_path(
                &mut list,
                Path {
                    kind: PathKind::Tid(quals, removed),
                    startup: st,
                    total: tot,
                    rows: rel.rows,
                    keys: Vec::new(),
                    disabled: (!self.g.tidscan) as u32,
                },
                startup,
            );
        }
        let range = tid_range_quals(rel);
        if !range.is_empty() {
            let (st, tot) = self.tid_range_cost(rel, &range);
            add_path(
                &mut list,
                Path {
                    kind: PathKind::TidRange(range),
                    startup: st,
                    total: tot,
                    rows: rel.rows,
                    keys: Vec::new(),
                    disabled: (!self.g.tidscan) as u32,
                },
                startup,
            );
        }
        // create_index_paths.
        let mut bitindex: Vec<BPath> = Vec::new();
        for (ix, ii) in rel.indexes.iter().enumerate() {
            let pred_ok = !ii.pred.is_empty() && implied_by(&ii.pred, &rel.quals);
            if !ii.pred.is_empty() && !pred_ok {
                continue;
            }
            let cs = match_quals(ii, &rel.quals, None, rel.booleq.as_ref(), self);
            if cs.is_empty() && !ii.am.optionalkey() {
                continue;
            }
            let fwd_keys = index_pathkeys(rel, ii, false, want);
            let only = self.g.indexonlyscan && self.index_only(rel, ii);
            let mut made = Vec::new();
            // match_pathkeys_to_index: ordering operators for (a prefix of) the ORDER BY.
            let (ob_keys, orderbys) = order_by_ops(ii, want);
            if !orderbys.is_empty() {
                made.push(self.index_path_ob(rel, ix, cs.clone(), ob_keys, orderbys, false, only));
            } else if !cs.is_empty() || !fwd_keys.is_empty() || pred_ok || only {
                made.push(self.index_path(rel, ix, cs.clone(), fwd_keys, false, only));
            }
            let back_keys = index_pathkeys(rel, ii, true, want);
            if !back_keys.is_empty() {
                made.push(self.index_path(rel, ix, cs.clone(), back_keys, true, only));
            }
            let gettuple = ii.am.gettuple();
            for p in made {
                if p.keys.is_empty() || p.index_sel < 1.0 {
                    bitindex.push(BPath::Index(p.clone()));
                }
                // (A GIN or BRIN index is read through a bitmap only.)
                if !gettuple {
                    continue;
                }
                let mut path = Path {
                    startup: p.startup,
                    total: p.total,
                    rows: p.rows,
                    keys: p.keys.clone(),
                    disabled: p.disabled,
                    kind: PathKind::Seq,
                };
                if p.only && !self.g.indexonlyscan {
                    path.disabled = 1;
                }
                path.kind = PathKind::Index(p);
                add_path(&mut list, path, startup);
            }
        }
        bitindex.extend(self.bitmap_or_paths(rel, &rel.quals, &[]));
        if !bitindex.is_empty() {
            let b = self.choose_bitmap_and(rel, bitindex);
            let (st, tot, _, _) = self.bitmap_heap(rel, &b);
            add_path(
                &mut list,
                Path {
                    kind: PathKind::Bitmap(b),
                    startup: st,
                    total: tot,
                    rows: rel.rows,
                    keys: Vec::new(),
                    disabled: (!self.g.bitmapscan) as u32,
                },
                startup,
            );
        }
        list
    }

    /// check_index_only.
    fn index_only(&self, rel: &Rel, ii: &IndexInfo) -> bool {
        if rel.other || ii.canreturn.is_empty() {
            return false;
        }
        let mut need = rel.needed.clone();
        for q in rel.quals.iter().filter(|q| !ii.implies(q)) {
            for c in q.columns() {
                if c < need.len() {
                    need[c] = true;
                } else {
                    return false;
                }
            }
        }
        need.iter()
            .enumerate()
            .all(|(c, n)| !*n || ii.canreturn.contains(&c))
    }

    /// The groups of rows equal on `exprs` (estimate_num_groups for one relation).
    fn num_groups(&self, rel: &Rel, exprs: &[&Expr], rows: f64) -> f64 {
        let mut nd = 1.0;
        let mut maxnd: f64 = 0.0;
        let mut n = 0;
        for e in exprs {
            match e {
                Expr::Col(c, _) => {
                    let d = VarRef {
                        table: rel.table.clone(),
                        col: *c,
                        tuples: rel.size.tuples,
                        expr: None,
                        rows: 0.0,
                        inh: false,
                    }
                    .ndistinct(&self.cat)
                    .0;
                    nd *= d;
                    maxnd = maxnd.max(d);
                    n += 1;
                }
                _ => return rows.clamp(1.0, costs::DEFAULT_NUM_DISTINCT),
            }
        }
        let tuples = rel.size.tuples;
        let mut clamp = tuples;
        if n > 1 {
            clamp *= 0.1;
            if clamp < maxnd {
                clamp = maxnd.min(tuples);
            }
        }
        if nd > clamp && clamp > 0.0 {
            nd = clamp;
        }
        // (Dell'Era's estimate of the distinct values a sample of the rows holds.)
        if tuples > 0.0 && nd > 0.0 && rows < tuples {
            nd *= 1.0 - ((tuples - rows) / tuples).powf(tuples / nd);
        }
        costs::clamp_rows(nd).ceil().min(rows).max(1.0)
    }

    /// create_ordered_paths over the relation's paths, then the LIMIT: the path to run.
    fn choose(&self, rel: &Rel, want: &Want, width: u32) -> Path {
        let list = self.paths(rel, want);
        let cheapest = cheapest(&list).cloned().expect("a path");
        let startup = want.startup();
        let mut out: Vec<Path> = Vec::new();
        if want.keys.is_empty() {
            out = list.clone();
        } else {
            let limit_tuples = match want.limit {
                Some((c, o)) if c > 0.0 && o >= 0.0 => Some(c + o),
                _ => None,
            };
            for p in &list {
                let (sorted, pre) = presorted(&want.keys, &p.keys);
                let is_cheapest = std::ptr::eq(p, cheapest_ref(&list));
                let path = if sorted {
                    p.clone()
                } else {
                    if !is_cheapest && (pre == 0 || !self.g.incremental_sort) {
                        continue;
                    }
                    let (st, tot) = if pre == 0 || !self.g.incremental_sort {
                        costs::sort_cost(p.total, p.rows, width, limit_tuples)
                    } else {
                        let exprs: Vec<&Expr> = want.keys[..pre].iter().map(|k| &k.expr).collect();
                        let groups = self.num_groups(rel, &exprs, p.rows);
                        costs::incremental_sort_cost(
                            p.startup,
                            p.total,
                            p.rows,
                            width,
                            groups,
                            limit_tuples,
                        )
                    };
                    let mut disabled = p.disabled;
                    if !self.g.sort {
                        disabled += 1;
                    }
                    Path {
                        kind: PathKind::Sort(Box::new(p.clone())),
                        startup: st,
                        total: tot,
                        rows: p.rows,
                        keys: want.keys.clone(),
                        disabled,
                    }
                };
                add_path(&mut out, path, startup);
            }
        }
        let _ = cheapest;
        let Some((count, offset)) = want.limit else {
            return cheapest_owned(&out);
        };
        // The Limit over each (adjust_limit_rows_costs), then get_cheapest_fractional_path:
        // a constant LIMIT compares total costs, another one a tenth of the rows.
        let fraction = if count < 0.0 || (count > 0.0 && offset < 0.0) {
            0.1
        } else {
            1.0
        };
        let mut best: Option<(Path, f64)> = None;
        for p in &out {
            let (st, tot) = limit_costs(p, count, offset);
            let cost = st + fraction * (tot - st);
            let tie = st;
            let better = match &best {
                None => true,
                Some((bp, bcost)) => {
                    if p.disabled != bp.disabled {
                        p.disabled < bp.disabled
                    } else if cost != *bcost {
                        cost < *bcost
                    } else {
                        let (bst, _) = limit_costs(bp, count, offset);
                        tie < bst
                    }
                }
            };
            if better {
                best = Some((p.clone(), cost));
            }
        }
        best.map(|b| b.0).unwrap_or_else(|| cheapest_owned(&out))
    }
}

fn cheapest_ref(list: &[Path]) -> &Path {
    cheapest(list).expect("a path")
}

fn cheapest_owned(list: &[Path]) -> Path {
    cheapest(list).cloned().expect("a path")
}

/// adjust_limit_rows_costs: (startup, total) of a LIMIT over `p`.
fn limit_costs(p: &Path, count: f64, offset: f64) -> (f64, f64) {
    let rows_in = p.rows;
    let run = p.total - p.startup;
    let (mut st, mut tot, mut rows) = (p.startup, p.total, rows_in);
    if offset != 0.0 {
        let o = if offset > 0.0 {
            offset
        } else {
            costs::clamp_rows(rows_in * 0.1)
        }
        .min(rows);
        if rows_in > 0.0 {
            st += run * o / rows_in;
        }
        rows = (rows - o).max(1.0);
    }
    if count != 0.0 {
        let k = if count > 0.0 {
            count
        } else {
            costs::clamp_rows(rows_in * 0.1)
        }
        .min(rows);
        if rows_in > 0.0 {
            tot = st + run * k / rows_in;
        }
    }
    (st, tot)
}

// ── building the relation from a scan ──

impl Planner<'_, '_> {
    fn rel(&self, s: &ScanPlan, needed: &[bool]) -> Option<Rel> {
        let t = s.table.clone();
        let (size, allvisfrac) = rel_size(&t, self.ctx);
        let mut quals = Vec::new();
        if let Some(f) = &s.filter {
            let mut parts = Vec::new();
            conjuncts(f.clone(), &mut parts);
            // (A clause over a PlaceHolderVar whose expression is an AND: one clause.)
            for q in s.phv_quals.iter().filter(|q| matches!(q, Expr::And(_))) {
                let mut sub = Vec::new();
                conjuncts(q.clone(), &mut sub);
                if let Some(first) = parts.iter().position(|p| sub.first().is_some_and(|x| x.same_as(p)))
                    && sub.iter().all(|x| parts.iter().any(|p| p.same_as(x)))
                {
                    parts.retain(|p| !sub.iter().any(|x| x.same_as(p)));
                    parts.insert(first.min(parts.len()), q.clone());
                }
            }
            // (A clause over a PlaceHolderVar is the relation's, whatever it reads: pull_varnos
            // counts the PlaceHolderVar's ph_eval_at.)
            quals = parts
                .into_iter()
                .filter(|e| {
                    !e.columns().is_empty() || expr_volatile_deep(e) || s.phv_quals.iter().any(|q| q.same_as(e))
                })
                .collect();
        }
        let ncols = t.columns.len();
        let mut need = vec![false; ncols];
        let mut other =
            s.with_key || s.mark.is_some() || (s.rti.0 != 0 && self.ref_marks.borrow().contains(&s.rti.0));
        for (i, n) in needed.iter().enumerate() {
            if *n {
                if i < ncols {
                    need[i] = true;
                } else {
                    other = true;
                }
            }
        }
        // (check_index_only reads the reltarget: a join clause's columns stay there though
        // the clause went, an equivalence class with a constant or self-join elimination
        // replacing it.)
        for &c in s.reltarget.0.iter() {
            if c < ncols {
                need[c] = true;
            }
        }
        let mut indexes = index_infos(self.ctx, &t, size.tuples);
        if s.with_key || s.mark.is_some() {
            indexes.iter_mut().for_each(|ii| ii.target = true);
        }
        let mut exprs = Vec::new();
        let mut booleq = None;
        // examine_variable: an index expression's statistics only for a user who may read
        // the whole table (or the partitioned table the query names).
        // (As the user the scan checks as: a view's owner.)
        let me = s.check_as.unwrap_or_else(|| self.ctx.current_user());
        let named = s
            .part
            .as_ref()
            .and_then(|p| p.last())
            .map(|l| l.parent.clone());
        let expr_stats_ok = {
            let cat = &self.cat;
            let dbo = self.ctx.db_owner();
            let can = |oid: u32| {
                cat.rel_mask(oid, me, dbo)
                    .is_none_or(|(held, _)| held & crate::catalog::acl::SELECT != 0)
            };
            can(t.oid) || named.as_ref().is_some_and(|p| can(p.oid))
        };
        let acl = {
            let cat = &self.cat;
            let dbo = self.ctx.db_owner();
            // Permissions and policies are the table's the query names.
            let top = named.clone().unwrap_or_else(|| t.clone());
            let rls = top.rls
                && !cat.is_superuser(me)
                && !cat.roles.get(&me).is_some_and(|r| r.bypass_rls)
                && (top.rls_force || !cat.has_privs_of_role(me, top.owner, dbo));
            let table = !rls
                && cat
                    .rel_mask(top.oid, me, dbo)
                    .is_none_or(|(held, _)| held & crate::catalog::acl::SELECT != 0);
            let cols = t
                .columns
                .iter()
                .map(|c| {
                    table
                        || (!rls
                            && top.columns.iter().position(|x| x.name == c.name).is_some_and(|i| {
                                cat.col_mask(top.oid, i as i32 + 1, me, dbo).0
                                    & crate::catalog::acl::SELECT
                                    != 0
                            }))
                })
                .collect();
            Arc::new((table, cols))
        };
        let mut hidden = Vec::new();
        for ii in &indexes {
            for (k, key) in ii.keys.iter().enumerate() {
                if !matches!(key, Expr::Col(..)) && !expr_stats_ok {
                    hidden.push((
                        key.clone(),
                        VarRef {
                            table: t.clone(),
                            col: 0,
                            tuples: size.tuples,
                            expr: Some((ii.ix.oid, k, key.ty())),
                            rows: 0.0,
                            inh: false,
                        },
                    ));
                }
                if !matches!(key, Expr::Col(..)) && expr_stats_ok {
                    exprs.push((
                        key.clone(),
                        VarRef {
                            table: t.clone(),
                            col: 0,
                            tuples: size.tuples,
                            expr: Some((ii.ix.oid, k, key.ty())),
                            rows: 0.0,
                            inh: false,
                        },
                    ));
                }
                if booleq.is_none() && key.ty() == DataType::Bool && (ii.am == Am::Btree || ii.am.by_family()) {
                    let mut b = crate::sql::binder::Binder::new(self.ctx, Vec::new());
                    // (Over two non-constants: `x = true` folds to x.)
                    if let Ok(Expr::Call(d, ..)) = b.builtin_binop("=", key.clone(), key.clone()) {
                        booleq = Some(d);
                    }
                }
            }
        }
        // Statistics objects' expressions (examine_variable looks after the indexes').
        if cat_has_statistics(&self.cat, t.oid) {
            for info in crate::sql::extstat::statlist(self.ctx, &t, false) {
                if !info.kinds.contains(&'e') {
                    continue;
                }
                for (k, x) in info.exprs.iter().enumerate() {
                    if costs::col_stats(&self.cat, info.oid, k).is_some() {
                        exprs.push((
                            x.clone(),
                            VarRef {
                                table: t.clone(),
                                col: 0,
                                tuples: size.tuples,
                                expr: Some((info.oid, k, x.ty())),
                                rows: 0.0,
                                inh: false,
                            },
                        ));
                    }
                }
            }
        }
        let mut rel = Rel {
            table: t,
            size,
            allvisfrac,
            quals,
            rows: 0.0,
            qual_cost: 0.0,
            qual_startup: 0.0,
            needed: need,
            other,
            indexes,
            loop_count: 1.0,
            exprs,
            booleq,
            written: Arc::new(s.written.clone()),
            acl,
            hidden: Arc::new(hidden),
            params: s.params.0.clone(),
        };
        {
            let mut pq = self.phv_quals.borrow_mut();
            for q in &s.phv_quals {
                if !pq.iter().any(|(o, x)| *o == rel.table.oid && x.same_as(q)) {
                    pq.push((rel.table.oid, q.clone()));
                }
            }
        }
        rel.rows = costs::clamp_rows(size.tuples * self.sel(&rel, &rel.quals));
        (rel.qual_startup, rel.qual_cost) = self.rel_quals_cost(&rel, &rel.quals);
        Some(rel)
    }
}

/// The key range an index scan reads for `quals` (the rest is checked on each row).
fn bounds(ii: &IndexInfo, quals: &[IClause]) -> IndexBounds {
    let mut b = IndexBounds::default();
    // GiST, SP-GiST, GIN and BRIN scans read the heap, checking their conditions on each
    // row.
    if ii.am.by_family() {
        return b;
    }
    let usable = |v: &Expr| {
        v.columns().is_empty() && !v.is_volatile() && !matches!(v, Expr::Default(_))
    };
    // An index a made operator class orders: the keys it is in order of.
    if ii.order.iter().any(Option::is_some) {
        for (i, (k, c)) in ii.keys.iter().zip(&ii.ix.columns).enumerate() {
            let using = ii.order[i].as_ref().and_then(|(f, _)| {
                let lt = crate::sql::func::resolve_binop("<", &k.ty(), &k.ty())
                    .or_else(|_| crate::sql::func::resolve_binop("<", &DataType::Int4, &DataType::Int4))
                    .ok()?
                    .def;
                Some(Arc::new(FuncDef {
                    coll: crate::catalog::partition::comparator(*f),
                    ..(*lt).clone()
                }))
            });
            b.order.push(SortKey {
                expr: k.clone(),
                desc: c.desc,
                nulls_first: c.nulls_first,
                using,
            });
        }
    }
    for (col, key) in ii.keys.iter().enumerate() {
        // (Its byte order isn't the made class's: no range from here on.)
        if ii.order.get(col).is_some_and(Option::is_some) {
            break;
        }
        let kt = key.ty();
        let cs: Vec<&IClause> = quals.iter().filter(|c| c.col == col).collect();
        // (An equality the index's byte order can't seek, under a nondeterministic
        // collation: no range from here on.)
        if cs.iter().any(|c| matches!(&c.expr, Expr::Call(d, ..) if !byte_order_op(d))) {
            break;
        }
        if let Some(v) = cs.iter().find_map(|c| match (&c.kind, &c.value) {
            (Kind::Op("="), Some(v)) if usable(v) && probe_ok(&kt, &v.ty()) => Some(v.clone()),
            _ => None,
        }) {
            b.eq.push(v);
            continue;
        }
        if let Some(v) = cs.iter().find_map(|c| match (&c.kind, &c.value) {
            (Kind::Saop("="), Some(v))
                if usable(v) && v.ty().elem().is_some_and(|e| probe_ok(&kt, e)) =>
            {
                Some(v.clone())
            }
            _ => None,
        }) {
            b.any = Some(v);
            break;
        }
        for c in &cs {
            let (Kind::Op(op), Some(v)) = (&c.kind, &c.value) else {
                continue;
            };
            if !usable(v) || !probe_ok(&kt, &v.ty()) {
                continue;
            }
            match *op {
                ">" => b.lower = Some((v.clone(), false)),
                ">=" => b.lower = Some((v.clone(), true)),
                "<" => b.upper = Some((v.clone(), false)),
                "<=" => b.upper = Some((v.clone(), true)),
                _ => {}
            }
        }
        break;
    }
    b
}

fn index_quals(cs: &[IClause]) -> Vec<IndexQual> {
    cs.iter()
        .map(|c| IndexQual {
            col: c.col,
            expr: c.expr.clone(),
            orig: c.orig.clone(),
            lossy: c.lossy,
        })
        .collect()
}

fn to_bitmap(rel: &Rel, b: &BPath) -> BitmapQual {
    match b {
        BPath::Index(p) => {
            let ii = &rel.indexes[p.ix];
            let origs: Vec<Expr> = p.clauses.iter().map(|c| c.orig.clone()).collect();
            BitmapQual::Index {
                index: ii.ix.clone(),
                quals: index_quals(&p.clauses),
                pred: ii
                    .pred
                    .iter()
                    .filter(|x| !implied_by(std::slice::from_ref(x), &origs))
                    .cloned()
                    .collect(),
                bounds: Box::new(bounds(ii, &p.clauses)),
            }
        }
        BPath::And(l, ..) => BitmapQual::And(l.iter().map(|x| to_bitmap(rel, x)).collect()),
        BPath::Or(l, ..) => BitmapQual::Or(l.iter().map(|x| to_bitmap(rel, x)).collect()),
    }
}

/// Record path `p` in the scan.
fn apply(rel: &Rel, s: &mut ScanPlan, p: &Path) {
    s.params = crate::sql::plan::ScanParams(rel.params.clone());
    match &p.kind {
        PathKind::Seq => {
            s.index = None;
            s.reverse = false;
            s.access = Access::Seq;
        }
        PathKind::Index(ip) => {
            let ii = &rel.indexes[ip.ix];
            let mut b = bounds(ii, &ip.clauses);
            if !ip.orderbys.is_empty() {
                b.order = ip
                    .orderbys
                    .iter()
                    .map(|e| SortKey {
                        expr: e.clone(),
                        desc: false,
                        nulls_first: false,
                        using: None,
                    })
                    .collect();
            }
            s.index = Some((ii.ix.clone(), b));
            s.reverse = ip.backward;
            s.access = Access::Index {
                only: ip.only,
                quals: index_quals(&ip.clauses),
                ordered: false,
                loops: rel.loop_count,
            };
        }
        PathKind::Bitmap(b) => {
            s.index = None;
            s.reverse = false;
            s.access = Access::Bitmap(Arc::new(to_bitmap(rel, b)), rel.loop_count);
        }
        PathKind::Tid(quals, removed) => {
            s.index = None;
            s.reverse = false;
            s.access = Access::Tid {
                quals: quals.clone(),
                removed: removed.clone(),
            };
        }
        PathKind::TidRange(quals) => {
            s.index = None;
            s.reverse = false;
            s.access = Access::TidRange(quals.clone());
        }
        PathKind::Sort(inner) => apply(rel, s, inner),
    }
}

// ── TID scans (tidpath.c) ──

/// The scan's own ctid column (a system column after the table's).
fn is_ctid(rel: &Rel, e: &Expr) -> bool {
    matches!(e, Expr::Col(i, DataType::Tid) if *i >= rel.table.columns.len())
}

/// A value fixed for the scan: no column of the row, nothing volatile.
fn tid_pseudo_constant(e: &Expr) -> bool {
    e.columns().is_empty() && !e.is_volatile_call()
}

/// IsTidEqualClause / IsTidEqualAnyClause: `ctid = x`, `x = ctid`, `ctid = ANY (x)`.
fn tid_equal_clause(rel: &Rel, e: &Expr) -> bool {
    match e {
        Expr::Call(d, args, _) if d.name == "=" && args.len() == 2 => {
            (is_ctid(rel, &args[0]) && tid_pseudo_constant(&args[1]) && args[1].ty() == DataType::Tid)
                || (is_ctid(rel, &args[1])
                    && tid_pseudo_constant(&args[0])
                    && args[0].ty() == DataType::Tid)
        }
        Expr::ArrayOp {
            left,
            right,
            op,
            all: false,
        } => op.name == "=" && is_ctid(rel, left) && tid_pseudo_constant(right),
        _ => false,
    }
}

/// TidQualFromRestrictInfoList: the first clause usable as TID quals: an equality, or an
/// OR each of whose arms is one (or ANDs one with other conditions). Returns the quals
/// (ORed) and the clause, when they stand for it exactly.
fn tid_quals_from(rel: &Rel, clauses: &[Expr]) -> Option<(Vec<Expr>, bool)> {
    for c in clauses {
        if tid_equal_clause(rel, c) {
            return Some((vec![c.clone()], true));
        }
        if let Expr::Or(arms) = c {
            let mut out = Vec::new();
            let mut exact = true;
            for a in arms {
                if tid_equal_clause(rel, a) {
                    out.push(a.clone());
                    continue;
                }
                let sub = match a {
                    Expr::And(items) => tid_quals_from(rel, items),
                    _ => None,
                };
                match sub {
                    Some((q, _)) => {
                        exact = false;
                        out.extend(q);
                    }
                    None => {
                        out.clear();
                        break;
                    }
                }
            }
            if !out.is_empty() {
                return Some((out, exact));
            }
        }
    }
    None
}

fn tid_quals(rel: &Rel) -> Option<(Vec<Expr>, Vec<Expr>)> {
    let (quals, exact) = tid_quals_from(rel, &rel.quals)?;
    // (create_tidscan_plan drops the clause the quals came from when they are all of it.)
    let removed = if quals.len() == 1 {
        quals.clone()
    } else if exact {
        rel.quals
            .iter()
            .find(|q| matches!(q, Expr::Or(arms) if arms.len() == quals.len() && arms.iter().zip(&quals).all(|(a, b)| a.same_as(b))))
            .cloned()
            .into_iter()
            .collect()
    } else {
        Vec::new()
    };
    Some((quals, removed))
}

/// TidRangeQualFromRestrictInfoList: every `ctid < x` (`<=`, `>`, `>=`, either way round).
fn tid_range_quals(rel: &Rel) -> Vec<Expr> {
    rel.quals
        .iter()
        .filter(|q| match q {
            Expr::Call(d, args, _) if matches!(d.name, "<" | "<=" | ">" | ">=") && args.len() == 2 => {
                (is_ctid(rel, &args[0]) && tid_pseudo_constant(&args[1]) && args[1].ty() == DataType::Tid)
                    || (is_ctid(rel, &args[1])
                        && tid_pseudo_constant(&args[0])
                        && args[0].ty() == DataType::Tid)
            }
            _ => false,
        })
        .cloned()
        .collect()
}

/// The selectivity of a condition on the row's ctid, and for an inequality whether it is a
/// lower bound (`>`, `>=`): equality is one row in the table's (unique, never NULL); an
/// inequality against a constant goes by the block it names (scalarineqsel's ctid case).
fn ctid_sel(rel: &Rel, q: &Expr) -> Option<(f64, Option<bool>)> {
    let tuples = rel.size.tuples.max(1.0);
    match q {
        Expr::ArrayOp {
            left,
            right,
            op,
            all: false,
        } if op.name == "=" && is_ctid(rel, left) => {
            Some(((array_len(right) / tuples).min(1.0), None))
        }
        Expr::Call(d, args, _) if args.len() == 2 => {
            let (op, other) = if is_ctid(rel, &args[0]) {
                (d.name, &args[1])
            } else if is_ctid(rel, &args[1]) {
                (commute(d.name), &args[0])
            } else {
                return None;
            };
            if other.ty() != DataType::Tid {
                return None;
            }
            match op {
                "=" => Some((1.0 / tuples, None)),
                "<" | "<=" | ">" | ">=" => {
                    let isgt = matches!(op, ">" | ">=");
                    let iseq = matches!(op, "<=" | ">=");
                    let Expr::Const(Value::Tid(blk, off), _) = other else {
                        // (Not a constant: the default.)
                        return Some((1.0 / 3.0, Some(isgt)));
                    };
                    let pages = rel.size.pages;
                    if pages <= 0.0 {
                        return Some((1.0, Some(isgt)));
                    }
                    let mut block = *blk as f64;
                    let mut density = rel.size.tuples / (pages - 0.5);
                    if block >= pages - 1.0 {
                        density *= 0.5;
                    }
                    if density > 0.0 {
                        block += (*off as f64 / density).min(1.0);
                    }
                    let mut selec = block / (pages - 0.5);
                    if iseq == isgt && rel.size.tuples >= 1.0 {
                        selec -= 1.0 / rel.size.tuples;
                    }
                    if isgt {
                        selec = 1.0 - selec;
                    }
                    Some((selec.clamp(0.0, 1.0), Some(isgt)))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

impl Planner<'_, '_> {
    /// cost_tidscan: a random page per TID, the TID quals evaluated once.
    fn tid_cost(&self, rel: &Rel, quals: &[Expr]) -> (f64, f64) {
        let ntuples: f64 = quals
            .iter()
            .map(|q| match q {
                Expr::ArrayOp { right, .. } => array_len(right),
                _ => 1.0,
            })
            .sum();
        let (_, tid_per) = self.quals_cost(quals);
        let startup = rel.qual_startup + tid_per;
        let run = self.g.random_page_cost * ntuples
            + (CPU_TUPLE_COST + rel.qual_cost - tid_per) * ntuples;
        (startup, startup + run)
    }

    /// cost_tidrangescan: one random page, then sequential ones for the range's share.
    fn tid_range_cost(&self, rel: &Rel, quals: &[Expr]) -> (f64, f64) {
        let sel = self.sel(rel, quals);
        let pages = (sel * rel.size.pages).ceil().max(1.0);
        let ntuples = sel * rel.size.tuples;
        let (_, tid_per) = self.quals_cost(quals);
        let startup = rel.qual_startup + tid_per;
        let run = self.g.random_page_cost
            + self.g.seq_page_cost * (pages - 1.0)
            + (CPU_TUPLE_COST + rel.qual_cost - tid_per) * ntuples;
        (startup, startup + run)
    }
}

/// Whether a scan's access path is the planner's to choose.
fn plannable(s: &ScanPlan) -> bool {
    matches!(s.access, Access::Unplanned)
        && s.sample.is_none()
        && s.current_of.is_none()
        && s.table.partkey.is_none()
}

// ── EXPLAIN ──

/// A bitmap tree node as EXPLAIN shows it.
pub enum BitmapNode {
    Index {
        name: String,
        quals: Vec<Expr>,
        total: f64,
        rows: f64,
        /// The BitmapQual's address (its counters).
        key: usize,
    },
    /// Members, cost, rows and the BitmapQual's address.
    And(Vec<BitmapNode>, f64, f64, usize),
    Or(Vec<BitmapNode>, f64, f64, usize),
}

/// A planned scan's estimates and the conditions each part checks.
pub struct ScanDesc {
    pub startup: f64,
    pub total: f64,
    pub rows: f64,
    /// Index Cond (index scans).
    pub index_conds: Vec<Expr>,
    /// Recheck Cond (bitmap heap scans).
    pub recheck: Vec<Expr>,
    /// The quals checked on each row read (Filter).
    pub filter: Vec<Expr>,
    pub bitmap: Option<BitmapNode>,
    pub disabled: bool,
    /// Order By (an index scan's ordering operators).
    pub order_by: Vec<Expr>,
    /// TID Cond (TID scans: alternatives; TID range scans: conjuncts).
    pub tid_conds: Vec<Expr>,
}

fn iclauses(ii: &IndexInfo, quals: &[IndexQual], rel: &Rel) -> Vec<IClause> {
    let _ = ii;
    quals
        .iter()
        .map(|q| {
            let (kind, value) = match &q.expr {
                Expr::Call(d, args, _) if args.len() == 2 => (Kind::Op(d.name), Some(args[1].clone())),
                Expr::ArrayOp { op, right, .. } => (Kind::Saop(op.name), Some((**right).clone())),
                Expr::IsNull(_, not) => (Kind::Null(!*not), None),
                _ => (Kind::Op("="), None),
            };
            IClause {
                col: q.col,
                qual: rel
                    .quals
                    .iter()
                    .position(|x| x.same_as(&q.orig))
                    .unwrap_or(usize::MAX),
                expr: q.expr.clone(),
                kind,
                value,
                orig: q.orig.clone(),
                lossy: q.lossy,
            }
        })
        .collect()
}

impl Planner<'_, '_> {
    fn bpath(&self, rel: &Rel, b: &BitmapQual) -> Option<BPath> {
        Some(match b {
            BitmapQual::Index { index, quals, .. } => {
                let ix = rel.indexes.iter().position(|i| i.ix.oid == index.oid)?;
                let cs = iclauses(&rel.indexes[ix], quals, rel);
                BPath::Index(self.index_path(rel, ix, cs, Vec::new(), false, false))
            }
            BitmapQual::And(l) => {
                self.bitmap_and(l.iter().map(|x| self.bpath(rel, x)).collect::<Option<_>>()?)
            }
            BitmapQual::Or(l) => {
                self.bitmap_or(l.iter().map(|x| self.bpath(rel, x)).collect::<Option<_>>()?)
            }
        })
    }

    fn bitmap_node(&self, rel: &Rel, q: &BitmapQual, b: &BPath) -> BitmapNode {
        match (q, b) {
            (BitmapQual::Index { index, quals, .. }, BPath::Index(p)) => BitmapNode::Index {
                name: index.name.clone(),
                quals: quals.iter().map(|x| x.expr.clone()).collect(),
                total: p.index_total,
                rows: costs::clamp_rows(p.index_sel * rel.size.tuples),
                key: q as *const BitmapQual as usize,
            },
            (BitmapQual::And(ql), BPath::And(bl, c, s)) => BitmapNode::And(
                ql.iter()
                    .zip(bl)
                    .map(|(x, y)| self.bitmap_node(rel, x, y))
                    .collect(),
                *c,
                costs::clamp_rows(s * rel.size.tuples),
                q as *const BitmapQual as usize,
            ),
            (BitmapQual::Or(ql), BPath::Or(bl, c, s)) => BitmapNode::Or(
                ql.iter()
                    .zip(bl)
                    .map(|(x, y)| self.bitmap_node(rel, x, y))
                    .collect(),
                *c,
                costs::clamp_rows(s * rel.size.tuples),
                q as *const BitmapQual as usize,
            ),
            _ => BitmapNode::And(Vec::new(), 0.0, 0.0, 0),
        }
    }
}

/// The original quals a bitmap tree checks, as one condition per top-level AND member.
fn bitmap_orig(q: &BitmapQual) -> Vec<Expr> {
    match q {
        BitmapQual::Index { quals, pred, .. } => {
            let mut out: Vec<Expr> = Vec::new();
            for x in quals {
                if !out.iter().any(|o| o.same_as(&x.orig)) {
                    out.push(x.orig.clone());
                }
            }
            out.extend(pred.iter().cloned());
            out
        }
        BitmapQual::And(l) => {
            let mut out: Vec<Expr> = Vec::new();
            for x in l {
                for e in bitmap_orig(x) {
                    if !out.iter().any(|o| o.same_as(&e)) {
                        out.push(e);
                    }
                }
            }
            out
        }
        BitmapQual::Or(l) => {
            let arms: Vec<Expr> = l
                .iter()
                .map(|x| {
                    let v = bitmap_orig(x);
                    if v.len() == 1 {
                        v.into_iter().next().unwrap()
                    } else {
                        Expr::And(v)
                    }
                })
                .collect();
            vec![Expr::Or(arms)]
        }
    }
}

/// The clauses of a bitmap tree's leaves that its index conditions were made from
/// without loss (all of a BitmapAnd's; an Or's are its own).
fn bitmap_exact_orig(q: &BitmapQual, out: &mut Vec<Expr>) {
    match q {
        // (Not a row comparison commuted: predtest can't prove that one.)
        BitmapQual::Index { quals, .. } => out.extend(
            quals
                .iter()
                .filter(|x| !x.lossy && !(matches!(x.orig, Expr::RowCompare(_)) && !x.orig.same_as(&x.expr)))
                .map(|x| x.orig.clone()),
        ),
        BitmapQual::And(l) => {
            for x in l {
                bitmap_exact_orig(x, out);
            }
        }
        BitmapQual::Or(_) => {}
    }
}

/// The index conditions a bitmap tree applies (create_bitmap_subplan's indexqual).
fn bitmap_indexquals(q: &BitmapQual) -> Vec<Expr> {
    match q {
        BitmapQual::Index { quals, pred, .. } => {
            let mut out: Vec<Expr> = quals.iter().map(|x| x.expr.clone()).collect();
            out.extend(pred.iter().cloned());
            out
        }
        BitmapQual::And(l) => l.iter().flat_map(bitmap_indexquals).collect(),
        BitmapQual::Or(l) => {
            let arms: Vec<Expr> = l
                .iter()
                .map(|x| {
                    let v = bitmap_indexquals(x);
                    if v.len() == 1 {
                        v.into_iter().next().unwrap()
                    } else {
                        Expr::And(v)
                    }
                })
                .collect();
            vec![Expr::Or(arms)]
        }
    }
}

/// EXPLAIN's view of a planned scan (None: not planned by cost).
pub fn describe(ctx: &ExecCtx, s: &ScanPlan) -> Option<ScanDesc> {
    describe_with(ctx, s, None)
}

/// `describe`, of the scan as a partial path of `par` = (workers, parallel divisor): its
/// CPU costs and rows spread over the processes.
pub fn describe_with(ctx: &ExecCtx, s: &ScanPlan, par: Option<(usize, f64)>) -> Option<ScanDesc> {
    if matches!(s.access, Access::Unplanned) {
        return None;
    }
    let p = Planner::new(ctx);
    let rel = p.rel(s, &[])?;
    let div = par.map_or(1.0, |(_, d)| d);
    let prows = |r: f64| if par.is_some() { costs::clamp_rows(r / div) } else { r };
    match &s.access {
        Access::Unplanned => None,
        Access::Seq => Some(ScanDesc {
            startup: rel.qual_startup,
            total: rel.qual_startup
                + p.g.seq_page_cost * rel.size.pages
                + (CPU_TUPLE_COST + rel.qual_cost) * rel.size.tuples / div,
            rows: prows(rel.rows),
            index_conds: Vec::new(),
            recheck: Vec::new(),
            filter: rel.quals.clone(),
            bitmap: None,
            disabled: !p.g.seqscan,
            order_by: Vec::new(),
            tid_conds: Vec::new(),
        }),
        Access::Index {
            only, quals, loops, ..
        } => {
            let (ix, _) = s.index.as_ref()?;
            let mut rel = rel;
            rel.loop_count = *loops;
            let n = rel.indexes.iter().position(|i| i.ix.oid == ix.oid)?;
            let cs = iclauses(&rel.indexes[n], quals, &rel);
            let order_by: Vec<Expr> = match &s.index {
                Some((ix, b)) if matches!(ix.method.as_str(), "gist" | "spgist") => {
                    b.order.iter().map(|k| k.expr.clone()).collect()
                }
                _ => Vec::new(),
            };
            let ip = p.index_path_ob(
                &rel,
                n,
                cs.clone(),
                Vec::new(),
                order_by.clone(),
                s.reverse,
                *only,
            );
            let ii = &rel.indexes[n];
            let conds: Vec<Expr> = quals.iter().map(|q| q.expr.clone()).collect();
            let filter = rel
                .quals
                .iter()
                .filter(|q| {
                    !quals.iter().any(|x| !x.lossy && x.orig.same_as(q))
                        && !ii.implies_plan(q)
                        && !(q.is_immutable() && implied_by(std::slice::from_ref(q), &conds))
                })
                .cloned()
                .collect();
            Some(ScanDesc {
                startup: ip.startup,
                total: ip.total - (ip.cpu_run - ip.cpu_run / div),
                rows: prows(rel.rows),
                index_conds: quals.iter().map(|q| q.expr.clone()).collect(),
                recheck: Vec::new(),
                filter,
                bitmap: None,
                disabled: !p.g.indexscan || (*only && !p.g.indexonlyscan),
                order_by,
                tid_conds: Vec::new(),
            })
        }
        Access::Tid { quals, removed } => {
            let (startup, total) = p.tid_cost(&rel, quals);
            let filter = rel
                .quals
                .iter()
                .filter(|q| !removed.iter().any(|r| r.same_as(q)))
                .cloned()
                .collect();
            Some(ScanDesc {
                startup,
                total,
                rows: prows(rel.rows),
                index_conds: Vec::new(),
                recheck: Vec::new(),
                filter,
                bitmap: None,
                disabled: !p.g.tidscan,
                order_by: Vec::new(),
                tid_conds: quals.clone(),
            })
        }
        Access::TidRange(quals) => {
            let (startup, total) = p.tid_range_cost(&rel, quals);
            let filter = rel
                .quals
                .iter()
                .filter(|q| !quals.iter().any(|r| r.same_as(q)))
                .cloned()
                .collect();
            Some(ScanDesc {
                startup,
                total,
                rows: prows(rel.rows),
                index_conds: Vec::new(),
                recheck: Vec::new(),
                filter,
                bitmap: None,
                disabled: !p.g.tidscan,
                order_by: Vec::new(),
                tid_conds: quals.clone(),
            })
        }
        Access::Bitmap(bq, loops) => {
            let mut rel = rel;
            rel.loop_count = *loops;
            let b = p.bpath(&rel, bq)?;
            let (st, tot, _, tuples) = p.bitmap_heap(&rel, &b);
            let cpu = (CPU_TUPLE_COST + rel.qual_cost) * tuples;
            let tot = tot - (cpu - cpu / div);
            // create_bitmap_scan_plan: the quals the index conditions don't imply are
            // checked as a filter, and aren't rechecked as well.
            let orig = bitmap_orig(bq);
            let iq = bitmap_indexquals(bq);
            // (A clause an index condition was made from exactly, an OR as `= ANY` with
            // its sides commuted say, is implied by it: predtest proves that much.)
            let mut exact: Vec<Expr> = Vec::new();
            bitmap_exact_orig(bq, &mut exact);
            let filter: Vec<Expr> = rel
                .quals
                .iter()
                .filter(|q| {
                    !iq.iter().any(|x| x.same_as(q))
                        && !exact.iter().any(|x| x.same_as(q))
                        && !(q.is_immutable() && implied_by(std::slice::from_ref(q), &iq))
                })
                .cloned()
                .collect();
            let recheck = orig
                .into_iter()
                .filter(|x| !filter.iter().any(|f| f.same_as(x)))
                .collect();
            Some(ScanDesc {
                startup: st,
                total: tot,
                rows: prows(rel.rows),
                index_conds: Vec::new(),
                recheck,
                filter,
                bitmap: Some(p.bitmap_node(&rel, bq, &b)),
                disabled: !p.g.bitmapscan,
                order_by: Vec::new(),
                tid_conds: Vec::new(),
            })
        }
    }
}

// ── a query level ──

/// preprocess_limit's estimate of a LIMIT / OFFSET expression (0: none, -1: not constant).
fn limit_est(e: &Option<Expr>, count: bool) -> f64 {
    match e {
        None => 0.0,
        Some(Expr::Const(Value::Null, _)) => 0.0,
        Some(Expr::Const(Value::Int8(v), _)) => {
            if count {
                (*v as f64).max(1.0)
            } else {
                (*v).max(0) as f64
            }
        }
        Some(Expr::Const(Value::Int4(v), _)) => {
            if count {
                (*v as f64).max(1.0)
            } else {
                (*v).max(0) as f64
            }
        }
        Some(_) => -1.0,
    }
}

fn mark(needed: &mut [bool], e: &Expr) {
    for c in e.columns() {
        if let Some(n) = needed.get_mut(c) {
            *n = true;
        }
    }
}

fn take(p: &mut Arc<Plan>) -> &mut Plan {
    Arc::make_mut(p)
}

/// A scan reached from a Sort through projections and filters: the sort keys made
/// expressions over its row, and the sort's input width.
fn sort_target(p: &mut Plan, keys: Vec<Expr>) -> Option<(&mut ScanPlan, Vec<Expr>)> {
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
            sort_target(take(input), keys)
        }
        Plan::Scan(s) => Some((s, keys)),
        _ => None,
    }
}

/// The ORDER BY keys over a grouping (through projections) that are its GROUP BY keys, a
/// prefix: (group key, descending, nulls first).
/// The ORDER BY's directions on a grouping's keys: (group key, descending, nulls first)
/// for the keys it starts with (preprocess_groupclause puts them first), and for every
/// key it names (the GROUP BY item takes the first matching ORDER BY item's sort operator,
/// as transformGroupClause gives it).
pub(crate) type GroupDirs = (Vec<(usize, bool, bool)>, Vec<(usize, bool, bool)>);

fn sort_group_dirs(input: &Plan, keys: &[SortKey]) -> Option<GroupDirs> {
    let mut exprs: Vec<Expr> = keys.iter().map(|k| k.expr.clone()).collect();
    let mut cur = input;
    let groups = loop {
        match cur {
            Plan::Project { input, exprs: px } => {
                exprs = exprs
                    .into_iter()
                    .map(|e| match e {
                        Expr::Col(i, t) => px.get(i).cloned().unwrap_or(Expr::Col(i, t)),
                        e => e,
                    })
                    .collect();
                cur = input;
            }
            Plan::Agg { groups, sets: None, .. } => break groups,
            // (Through a HAVING, and a FROM subquery planned apart.)
            Plan::Filter { input, .. } if matches!(&**input, Plan::Agg { .. }) => cur = input,
            Plan::Barrier { input, secure: false, .. } => cur = input,
            _ => return None,
        }
    };
    let mut out = Vec::new();
    let mut all: Vec<(usize, bool, bool)> = Vec::new();
    let mut prefix = true;
    for (e, k) in exprs.iter().zip(keys) {
        match e {
            Expr::Col(i, _) if *i < groups.len() && k.using.is_none() => {
                if prefix {
                    out.push((*i, k.desc, k.nulls_first));
                }
                if !all.iter().any(|a| a.0 == *i) {
                    all.push((*i, k.desc, k.nulls_first));
                }
            }
            Expr::Col(i, _) if *i < groups.len() => {
                prefix = false;
                // (Another operator: the GROUP BY keeps its own.)
                if !all.iter().any(|a| a.0 == *i) {
                    all.push((*i, false, false));
                }
            }
            _ => prefix = false,
        }
    }
    Some((out, all))
}

/// The columns each input of `p` must produce, given those `p`'s consumers need (PG's
/// reltargets / attr_needed): in the order of the plan's inputs.
pub fn child_needs(p: &Plan, needed: &[bool]) -> Vec<Vec<bool>> {
    let w = crate::sql::binder::plan_width(p);
    let pass = |extra: &[&Expr]| {
        let mut n = vec![false; w];
        for (i, x) in needed.iter().enumerate() {
            if i < w && *x {
                n[i] = true;
            }
        }
        for e in extra {
            mark(&mut n, e);
        }
        n
    };
    let pw = |p: &Plan| crate::sql::binder::plan_width(p);
    match p {
        Plan::Limit { .. } | Plan::Context { .. } | Plan::Barrier { .. } => vec![pass(&[])],
        Plan::LockRows { input, marks, .. } => {
            let mut n = pass(&[]);
            n.resize(pw(input), false);
            for m in marks.iter() {
                if let Some(x) = n.get_mut(m.col) {
                    *x = true;
                }
            }
            vec![n]
        }
        Plan::Filter { pred, .. } => vec![pass(&[pred])],
        Plan::Sort { keys, .. } => {
            let mut n = pass(&[]);
            keys.iter().for_each(|k| mark(&mut n, &k.expr));
            vec![n]
        }
        Plan::Project { input, exprs } => {
            let mut n = vec![false; pw(input)];
            for (i, e) in exprs.iter().enumerate() {
                if needed.get(i).copied().unwrap_or(true) {
                    mark(&mut n, e);
                }
            }
            vec![n]
        }
        Plan::ProjectSet { input, exprs, srfs } => {
            let mut n = vec![false; pw(input)];
            // (What the consumers read of its target list, and the calls' arguments.)
            exprs
                .iter()
                .enumerate()
                .filter(|(i, _)| needed.get(*i).copied().unwrap_or(true))
                .for_each(|(_, e)| mark(&mut n, e));
            for s in srfs {
                s.args.iter().for_each(|a| mark(&mut n, a));
            }
            vec![n]
        }
        Plan::Distinct { input, .. } => vec![vec![true; pw(input)]],
        Plan::SetOp { left, right, .. } => vec![vec![true; pw(left)], vec![true; pw(right)]],
        Plan::Agg {
            input,
            groups,
            aggs,
            ..
        } => {
            let mut n = vec![false; pw(input)];
            groups.iter().for_each(|g| mark(&mut n, g));
            for a in aggs {
                a.args
                    .iter()
                    .chain(&a.direct)
                    .chain(&a.filter)
                    .for_each(|e| mark(&mut n, e));
                a.order_by.iter().for_each(|k| mark(&mut n, &k.expr));
            }
            vec![n]
        }
        Plan::Window { input, calls } => {
            let iw = pw(input);
            let mut n = vec![false; iw];
            for (i, x) in needed.iter().enumerate() {
                if i < iw && *x {
                    n[i] = true;
                }
            }
            for c in calls {
                c.args
                    .iter()
                    .chain(&c.partition_by)
                    .for_each(|e| mark(&mut n, e));
                c.order_by.iter().for_each(|k| mark(&mut n, &k.expr));
                if let crate::sql::plan::WinFunc::Agg(a) = &c.func {
                    a.filter.iter().for_each(|e| mark(&mut n, e));
                }
            }
            vec![n]
        }
        Plan::Join {
            kind,
            left,
            right,
            cond,
            hash,
            params,
            nleft,
            meta,
            ..
        } => {
            let nl = *nleft;
            let (lw, rw) = (pw(left), pw(right));
            let mut ln = vec![false; lw];
            let mut rn = vec![false; rw];
            for (i, x) in needed.iter().enumerate() {
                if *x {
                    if i < nl {
                        if i < lw {
                            ln[i] = true;
                        }
                    } else if !matches!(kind, JoinKind::Semi | JoinKind::Anti) && i - nl < rw {
                        rn[i - nl] = true;
                    }
                }
            }
            let mut both = |e: &Expr| {
                for c in e.columns() {
                    if c < nl {
                        if c < lw {
                            ln[c] = true;
                        }
                    } else if c - nl < rw {
                        rn[c - nl] = true;
                    }
                }
            };
            if let Some(c) = cond {
                both(c);
            }
            if let Some((lk, rk)) = hash {
                lk.iter().for_each(|e| mark(&mut ln, e));
                rk.iter().for_each(|e| mark(&mut rn, e));
            }
            params.iter().for_each(|e| mark(&mut ln, e));
            for &c in &meta.kept {
                if c < nl {
                    if c < lw {
                        ln[c] = true;
                    }
                } else if c - nl < rw {
                    rn[c - nl] = true;
                }
            }
            vec![ln, rn]
        }
        Plan::Append { inputs, .. } => inputs
            .iter()
            .map(|i| {
                let iw = pw(i);
                if iw == w {
                    needed.to_vec()
                } else {
                    vec![true; iw]
                }
            })
            .collect(),
        Plan::Recursive {
            base, recursive, ..
        } => vec![vec![true; pw(base)], vec![true; pw(recursive)]],
        Plan::With { .. } => vec![pass(&[])],
        Plan::Insert(i) => vec![vec![true; pw(&i.source)]],
        Plan::Update(u) => {
            let ncols = u.rules.table.columns.len();
            let mut n = vec![false; pw(&u.source)];
            for (_, e) in &u.sets {
                mark(&mut n, e);
            }
            // A view's INSTEAD OF trigger gets the whole old row from the scan.
            if u.rules.view {
                for x in n.iter_mut().take(ncols) {
                    *x = true;
                }
            }
            // The row's key; RETURNING reads the new row.
            if let Some(x) = n.get_mut(ncols) {
                *x = true;
            }
            for e in &u.returning {
                for c in e.columns() {
                    if c > ncols
                        && let Some(x) = n.get_mut(c)
                    {
                        *x = true;
                    }
                }
            }
            vec![n]
        }
        Plan::Delete(d) => {
            let ncols = d.table.columns.len();
            let mut n = vec![false; pw(&d.source)];
            if let Some(x) = n.get_mut(ncols) {
                *x = true;
            }
            // (As for UPDATE: the view's INSTEAD OF trigger takes the whole row.)
            if d.view {
                for x in n.iter_mut().take(ncols) {
                    *x = true;
                }
            }
            vec![n]
        }
        // A MERGE reads the source columns its actions, join condition and RETURNING read,
        // and all of the target's (its actions read the target row from the join); a computed
        // source column nothing reads (a sub-select its readers took copies of) goes.
        Plan::Merge(m) => {
            let w = pw(&m.source);
            let mut n = vec![false; w];
            let ncols = m.rules.table.columns.len();
            let sys = if m.sys { crate::exec::scan::NUM_SYS_COLS } else { 0 };
            for x in n.iter_mut().skip(m.target).take(ncols + 1 + sys) {
                *x = true;
            }
            if let Some(x) = m.source_flag.and_then(|f| n.get_mut(f)) {
                *x = true;
            }
            for a in &m.actions {
                if let Some(c) = &a.cond {
                    mark(&mut n, c);
                }
                match &a.cmd {
                    crate::sql::plan::MergeCmd::Update { sets, .. } => sets.iter().for_each(|(_, e)| mark(&mut n, e)),
                    crate::sql::plan::MergeCmd::Insert { values, .. } => values.iter().for_each(|e| mark(&mut n, e)),
                    _ => {}
                }
            }
            if let Some(c) = &m.join_cond {
                mark(&mut n, c);
            }
            for e in &m.returning {
                for c in e.columns() {
                    if let Some(x) = n.get_mut(c) {
                        *x = true;
                    }
                }
            }
            // (Members' own target lists: theirs over the same row; row-level security
            // checks over it.)
            let rls = m.actions.iter().any(|a| match &a.cmd {
                crate::sql::plan::MergeCmd::Update { rls_existing, rls_new, .. } => !rls_existing.is_empty() || !rls_new.is_empty(),
                crate::sql::plan::MergeCmd::Delete { rls_existing } => !rls_existing.is_empty(),
                crate::sql::plan::MergeCmd::Insert { rls_new, .. } => !rls_new.is_empty(),
                crate::sql::plan::MergeCmd::Nothing => false,
            });
            if !m.members.is_empty() || m.route.is_some() || rls || !m.rules.rls_checks.is_empty() {
                n.iter_mut().for_each(|x| *x = true);
            }
            vec![n]
        }
        _ => Vec::new(),
    }
}

impl Planner<'_, '_> {
    /// get_memoize_path for a LATERAL nested loop: its inner side cached on the outer
    /// values it reads (compared bit by bit), when that makes the loop cheaper.
    fn lateral_memoize(&self, p: &mut Plan) {
        let Plan::Join {
            kind,
            left,
            right,
            cond,
            hash: None,
            params,
            meta,
            ..
        } = p
        else {
            return;
        };
        let g = crate::sql::joins::JoinGucs::read(self.ctx);
        if params.is_empty()
            || !g.memoize
            || meta.memoize.is_some()
            || !matches!(kind, JoinKind::Inner | JoinKind::Left)
            || crate::sql::explain::plan_volatile(right)
            || params.iter().any(|e| e.is_volatile() || !joinpath::memo_hashable(&e.ty()))
            || joinpath::lateral_join_leaf(right)
        {
            return;
        }
        let l = crate::sql::explain::estimate(left, self.ctx);
        if l.rows < 2.0 {
            return;
        }
        let r = crate::sql::explain::estimate(right, self.ctx);
        let keys = lateral_memo_keys(right, params);
        let mut cols: Vec<usize> = Vec::new();
        let mut width = 0.0;
        for k in &keys {
            width += match k {
                Expr::Col(j, _) => l.cols.get(*j).map_or(4, |c| c.0) as f64,
                k => costs::type_width(&k.ty()) as f64,
            };
            for c in k.columns() {
                if !cols.contains(&c) {
                    cols.push(c);
                }
            }
        }
        let vars: Vec<_> = cols
            .iter()
            .map(|&c| match l.cols.get(c) {
                Some((_, v, d)) => (v.clone(), *d, l.rows),
                None => (None, None, l.rows),
            })
            .collect();
        let calls = costs::clamp_rows(l.rows);
        let nd = crate::sql::joins::memo_ndistinct(&self.cat, &vars, calls);
        use crate::sql::joins::{In, InKind, Jt, QualCost};
        let inner = In {
            rows: r.rows,
            startup: r.startup,
            total: r.total,
            width: r.width,
            disabled: 0,
            kind: InKind::Plain,
        };
        let outer = In {
            rows: l.rows,
            startup: l.startup,
            total: l.total,
            width: l.width,
            disabled: 0,
            kind: InKind::Plain,
        };
        let rescan = crate::sql::joins::memoize_rescan(&inner, calls, nd, width, &g);
        let memo = In {
            startup: inner.startup + CPU_TUPLE_COST,
            total: inner.total + CPU_TUPLE_COST,
            kind: InKind::Memoize(rescan.0, rescan.1),
            ..inner
        };
        let jt = if *kind == JoinKind::Left { Jt::Left } else { Jt::Inner };
        let quals = QualCost {
            startup: 0.0,
            per_tuple: cond.as_ref().map_or(0.0, costs::qual_cost),
        };
        let plain = crate::sql::joins::nestloop(jt, &outer, &inner, false, None, 0.0, quals, &g);
        let cached = crate::sql::joins::nestloop(jt, &outer, &memo, false, None, 0.0, quals, &g);
        if cached.total < plain.total
            || (cached.total == plain.total && cached.startup < plain.startup)
        {
            meta.memoize = Some(crate::sql::plan::Memo {
                keys,
                binary: true,
                singlerow: false,
                rescan,
            });
        }
    }

    fn walk(&self, p: &mut Plan, needed: &[bool], lim: Option<(f64, f64)>) {
        // (A filter's cost is for the join right under it only.)
        if !matches!(p, Plan::Join { .. }) {
            self.post_cost.set(0.0);
        }
        // A WHERE over a UNION ALL (an append relation): each arm takes the clauses.
        if self.push_into_union_all(p) {
            return self.walk(p, needed, lim);
        }
        // ORDER BY keys the equivalence classes below make redundant go (all of them: the
        // sort goes).
        if matches!(p, Plan::Sort { .. }) {
            self.redundant_sort_keys(p);
            if !matches!(p, Plan::Sort { .. }) {
                return self.walk(p, needed, lim);
            }
        }
        // A group of joins: their order, methods and access paths together (under a
        // LIMIT, for the fraction it reads).
        if matches!(p, Plan::Join { .. }) && self.plan_joins_limited(p, needed, lim) {
            return;
        }
        // (A LATERAL subquery left to be pulled up that no group took in: planned apart.)
        if matches!(p, Plan::Join { .. }) {
            self.settle_laterals(p);
        }
        if matches!(p, Plan::Filter { .. }) && self.plan_filtered_join(p, needed) {
            return;
        }
        // (A filter over an outer join that stays above it: its cost is the join's.)
        if let Plan::Filter { input, pred } = &*p
            && matches!(&**input, Plan::Join { params, kind, .. } if params.is_empty() && *kind != JoinKind::Inner)
        {
            self.post_cost.set(costs::qual_cost(pred));
        }
        // A Sort over a scan may read an index in order instead.
        if matches!(p, Plan::Sort { .. }) {
            let n = child_needs(p, needed).pop().unwrap_or_default();
            if self.merge_append(p, &n, lim)
                || self.ordered_scan(p, &n, lim)
                || self.sys_ordered_scan(p, &n, lim)
                || self.ordered_join(p, &n, lim)
            {
                return;
            }
            if let Plan::Sort { input, keys } = &*p
                && let Some(d) = sort_group_dirs(input, keys)
            {
                self.group_dirs.replace(Some(d));
            }
        }
        // A grouping (or DISTINCT) over a join group: the joins planned for the GROUP BY's
        // order.
        if matches!(p, Plan::Agg { .. } | Plan::Distinct { on: None, .. }) {
            let dirs = self.group_dirs.take().unwrap_or_default();
            // (Partitionwise aggregation needs the join partitionwise: Postgres keeps
            // that path for it and costs them together.)
            let pw = matches!(p, Plan::Agg { groups, sets: None, .. } if !groups.is_empty())
                && self
                    .ctx
                    .sess
                    .settings
                    .borrow()
                    .get_raw("enable_partitionwise_aggregate")
                    .is_some_and(|v| v == "on" || v == "true");
            self.pw_agg_join.set(pw);
            let done = self.grouped_join(p, needed, &dirs);
            self.pw_agg_join.set(false);
            if done {
                return;
            }
        }
        if self.reorder_distinct_on(p, needed, lim) || self.ordered_distinct(p, needed, lim) {
            return;
        }
        // A grouping or DISTINCT over a scan: index orders on its keys are useful
        // (pathkeys_useful_for_grouping / _distinct), though not required.
        let keys: Option<Vec<Expr>> = match &*p {
            Plan::Agg {
                groups, sets: None, ..
            } if !groups.is_empty() => Some(groups.clone()),
            // (Grouping sets: the first rollup's keys, which its sorted input wants.)
            Plan::Agg {
                groups,
                sets: Some(sets),
                ..
            } if !groups.is_empty() => crate::exec::agg::rollup_chains(sets)
                .first()
                .map(|(_, cols)| cols.iter().filter_map(|&c| groups.get(c).cloned()).collect::<Vec<_>>())
                .filter(|k| !k.is_empty()),
            Plan::Distinct { input, on: None, .. } => {
                let w = crate::sql::binder::plan_width(input);
                Some((0..w).map(|i| Expr::Col(i, DataType::Unknown)).collect())
            }
            _ => None,
        };
        if let Some(keys) = keys {
            let n = child_needs(p, needed).pop().unwrap_or_default();
            let distinct = matches!(p, Plan::Distinct { .. });
            if let Plan::Agg { input, .. } | Plan::Distinct { input, .. } = p
                && self.hinted_scan(take(input), &n, keys, distinct)
            {
                return;
            }
        }
        // A window ordered by a distance (`key <-> value`): its ordering as the query's
        // pathkeys, which a GiST or SP-GiST scan's ordering operators may give.
        if let Plan::Window { calls, .. } = p
            && let Some(first) = calls.first()
            && calls.iter().all(|c| {
                c.partition_by.len() == first.partition_by.len()
                    && c.order_by.len() == first.order_by.len()
                    && c.partition_by.iter().zip(&first.partition_by).all(|(a, b)| a.same_as(b))
                    && c.order_by.iter().zip(&first.order_by).all(|(a, b)| {
                        a.expr.same_as(&b.expr) && a.desc == b.desc && a.nulls_first == b.nulls_first
                    })
            })
            && first.partition_by.is_empty()
            && first
                .order_by
                .first()
                .is_some_and(|k| matches!(&k.expr, Expr::Call(_, a, _) if a.len() == 2))
        {
            let n = child_needs(p, needed).pop().unwrap_or_default();
            let Plan::Window { input, calls } = p else { unreachable!() };
            let mut tmp = Plan::Sort {
                input: input.clone(),
                keys: calls[0].order_by.clone(),
            };
            if self.ordered_scan(&mut tmp, &n, None) && !matches!(tmp, Plan::Sort { .. }) {
                *input = Arc::new(tmp);
                return;
            }
            let _ = calls;
        }
        // A window over an inheritance tree: its members read in the window's order (an
        // ordered Append, or a Merge Append) when that costs less than sorting them all
        // (generate_orderedappend_paths for the window's pathkeys).
        if let Plan::Window { input, calls } = p
            && let Some(first) = calls.first()
            && calls.iter().all(|c| {
                c.partition_by.len() == first.partition_by.len()
                    && c.order_by.len() == first.order_by.len()
                    && c.partition_by.iter().zip(&first.partition_by).all(|(a, b)| a.same_as(b))
                    && c.order_by.iter().zip(&first.order_by).all(|(a, b)| {
                        a.expr.same_as(&b.expr) && a.desc == b.desc && a.nulls_first == b.nulls_first
                    })
            })
            && !(first.partition_by.is_empty() && first.order_by.is_empty())
            && first.order_by.iter().all(|k| k.using.is_none())
            && {
                let mut q: &Plan = input;
                while let Plan::Project { input, .. } = q {
                    q = input;
                }
                matches!(q, Plan::Append { inh: Some(_), .. })
            }
        {
            let keys: Vec<SortKey> = first
                .partition_by
                .iter()
                .map(|e| SortKey {
                    expr: e.clone(),
                    desc: false,
                    nulls_first: false,
                    using: None,
                })
                .chain(first.order_by.iter().cloned())
                .fold(Vec::new(), |mut v: Vec<SortKey>, k| {
                    // (A key already sorted on: redundant, as a pathkey.)
                    if !v.iter().any(|x| x.expr.same_as(&k.expr)) {
                        v.push(k);
                    }
                    v
                });
            let n = child_needs(p, needed).pop().unwrap_or_default();
            let Plan::Window { input, .. } = p else { unreachable!() };
            let mut tmp = Plan::Sort {
                input: input.clone(),
                keys,
            };
            // (The Sort stays: the members come in its order, EXPLAIN shows no sort.)
            if self.merge_append(&mut tmp, &n, None) {
                *input = Arc::new(tmp);
                return;
            }
        }
        // A window over a join: the join planned for the window's order (query_pathkeys
        // are the window's), presorted paths competing with sorting the cheapest; under a
        // LIMIT, for the rows read before its first output when its frame doesn't need them
        // all (get_windowclause_startup_tuples).
        if let Plan::Window { input, calls } = p
            && let Some(first) = calls.first()
            && calls.iter().all(|c| {
                c.partition_by.len() == first.partition_by.len()
                    && c.order_by.len() == first.order_by.len()
                    && c.partition_by.iter().zip(&first.partition_by).all(|(a, b)| a.same_as(b))
                    && c.order_by.iter().zip(&first.order_by).all(|(a, b)| {
                        a.expr.same_as(&b.expr) && a.desc == b.desc && a.nulls_first == b.nulls_first
                    })
            })
            && !(first.partition_by.is_empty() && first.order_by.is_empty())
            && first.order_by.iter().all(|k| k.using.is_none())
            && {
                let mut q: &Plan = input;
                while let Plan::Project { input, .. } = q {
                    q = input;
                }
                matches!(q, Plan::Join { .. })
            }
        {
            let all_rows = calls.iter().any(|c| {
                matches!(
                    c.frame.end,
                    crate::sql::plan::FrameBound::UnboundedFollowing | crate::sql::plan::FrameBound::Following(_)
                )
            });
            let keys: Vec<SortKey> = first
                .partition_by
                .iter()
                .map(|e| SortKey {
                    expr: e.clone(),
                    desc: false,
                    nulls_first: false,
                    using: None,
                })
                .chain(first.order_by.iter().cloned())
                .collect();
            let n = child_needs(p, needed).pop().unwrap_or_default();
            let Plan::Window { input, .. } = p else { unreachable!() };
            let mut tmp = Plan::Sort {
                input: input.clone(),
                keys,
            };
            if self.ordered_join(&mut tmp, &n, if all_rows { None } else { lim }) {
                // (The window sorts its input unless it comes in order.)
                *input = match tmp {
                    Plan::Sort { input, .. } => input,
                    other => Arc::new(other),
                };
                return;
            }
        }
        if let Plan::Scan(s) = p {
            if plannable(s) {
                let want = Want {
                    keys: Vec::new(),
                    limit: lim,
                    merge: Vec::new(),
                };
                self.plan_scan(s, needed, &want, 0);
            }
            return;
        }
        // A FROM subquery planned apart: its own query level, planned for the columns read
        // of it (remove_unused_subquery_outputs) and the LIMIT above it, if nothing else
        // is (set_subquery_pathlist's tuple_fraction).
        if let Plan::Barrier { input, secure: false, .. } = p {
            let dirs = self.group_dirs.take();
            let removed = plan_level_ordered(self.ctx, take(input), needed, lim, dirs);
            self.removed.set(self.removed.get() + removed);
            return;
        }
        // A LATERAL join of two partitioned items the join search doesn't take: joined
        // partition by partition when it can be.
        if matches!(p, Plan::Join { params, .. } if !params.is_empty()) && self.lateral_partitionwise(p) {
            return self.walk(p, needed, lim);
        }
        let needs = child_needs(p, needed);
        // A LIMIT reaches the scan through projections (and a CTE's body).
        let lim_in = match p {
            Plan::Limit { limit, offset, .. } => {
                Some((limit_est(limit, true), limit_est(offset, false)))
            }
            Plan::Project { .. } | Plan::With { .. } => lim,
            // (A UNION ALL's arms are planned for the fraction the LIMIT reads:
            // set_subquery_pathlist passes the tuple_fraction down.)
            Plan::Append { inh: None, .. }
            | Plan::SetOp {
                kind: crate::sql::plan::SetOpKind::Union,
                all: true,
                ..
            } => lim,
            // Every DISTINCT key fixed by an equality with a constant: its first row is
            // all it returns (a Limit 1, as EXPLAIN shows it).
            Plan::Distinct { input, on, .. } => {
                let fixed = match on {
                    Some(on) => {
                        let fixed = constant_columns(input);
                        !on.is_empty()
                            && on.iter().all(|e| crate::sql::explain::key_col_fixed(e, &fixed))
                    }
                    None => crate::sql::explain::distinct_keys_fixed(input),
                };
                fixed.then_some((1.0, 0.0))
            }
            _ => None,
        };
        let mut needs = needs.into_iter();
        let mut visit = |c: &mut Arc<Plan>, lim: Option<(f64, f64)>| {
            let n = needs.next().unwrap_or_else(|| vec![true; crate::sql::binder::plan_width(c)]);
            self.walk(take(c), &n, lim);
        };
        match p {
            Plan::With { ctes, body, names } => {
                let nested = self.join_nested.replace(false);
                for (_, c) in ctes.iter_mut() {
                    let cw = crate::sql::binder::plan_width(c);
                    self.walk(take(c), &vec![true; cw], None);
                }
                self.join_nested.set(nested);
                // (Its CTE Scans' sizes: their queries' estimates.)
                let _pushed = crate::sql::explain::CteEstimatesPushed(crate::sql::explain::push_cte_estimates(
                    ctes, names, self.ctx,
                ));
                crate::scoped::checkpoint("explain::CTE_ESTIMATES.plan");
                visit(body, lim_in);
            }
            Plan::Filter { input, .. }
            | Plan::Project { input, .. }
            | Plan::ProjectSet { input, .. }
            | Plan::Agg { input, .. }
            | Plan::Window { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::Distinct { input, .. }
            | Plan::LockRows { input, .. }
            | Plan::Context { input, .. } => visit(input, lim_in),
            // (A query level of its own: its joins its topmost scan/join relation.)
            Plan::Barrier { input, .. } => {
                let nested = self.join_nested.replace(false);
                visit(input, lim_in);
                self.join_nested.set(nested);
            }
            Plan::SetOp {
                kind, left, right, ..
            } => {
                let nested = self.join_nested.replace(false);
                visit(left, lim_in);
                visit(right, lim_in);
                self.join_nested.set(nested);
                // generate_nonunion_paths: INTERSECT puts the input with fewer groups
                // first.
                if *kind == crate::sql::plan::SetOpKind::Intersect
                    && crate::sql::explain::setop_shape(left, right, true, self.ctx).0
                {
                    std::mem::swap(left, right);
                }
            }
            Plan::Join { left, right, params, .. } => {
                let nested = self.join_nested.replace(true);
                visit(left, None);
                // (A LATERAL right side: clauses reading its values stay out of its subquery
                // levels.)
                let lateral = !params.is_empty();
                if lateral {
                    crate::sql::binder::optimize::lateral_push(params);
                }
                // A LATERAL side that is a relation (a pulled-up subquery's scan, or an
                // appendrel of them): its paths are parameterized by the outer side,
                // costed for that side's rows as their loops (get_loop_count).
                let loops = (lateral && lateral_rel(right))
                    .then(|| crate::sql::explain::estimate(left, self.ctx).rows.max(1.0));
                let saved = self.lateral_loops.replace(loops);
                visit(right, None);
                self.lateral_loops.set(saved);
                if lateral {
                    crate::sql::binder::optimize::lateral_pop();
                }
                self.join_nested.set(nested);
                self.lateral_memoize(p);
            }
            Plan::Append { inputs, inh, .. } => {
                // (A UNION ALL's arms: subqueries, each a query level.)
                let nested = if inh.is_none() { self.join_nested.replace(false) } else { self.join_nested.get() };
                for i in inputs.iter_mut() {
                    visit(i, lim_in);
                }
                self.join_nested.set(nested);
            }
            Plan::Recursive {
                id, base, recursive, ..
            } => {
                let _level = FlagScope::set(&self.join_nested, false);
                visit(base, None);
                // (The recursive term's WorkTable Scan: ten times the non-recursive term.)
                crate::sql::explain::push_work_estimate(*id, base, self.ctx);
                let _pushed = crate::sql::explain::CteEstimatesPushed(1);
                visit(recursive, None);
            }
            Plan::Insert(i) => visit(&mut i.source, None),
            Plan::Update(u) => visit(&mut u.source, None),
            Plan::Delete(d) => visit(&mut d.source, None),
            Plan::Merge(m) => {
                self.merge_target.set(Some(m.rules.table.oid));
                visit(&mut m.source, None);
                self.merge_target.set(None);
            }
            _ => {}
        }
        // A WHERE clause over a join made partitionwise: each child join's own.
        if let Plan::Filter { input, pred } = p
            && let Plan::Append { inputs, inh: None, pw: Some(id), .. } = &**input
            && joinpath::absorbable(pred)
            && self.pw_joins.borrow().iter().any(|(a, _)| a == id)
        {
            let id = *id;
            let inputs: Vec<Arc<Plan>> = inputs
                .iter()
                .map(|c| {
                    Arc::new(Plan::Filter {
                        input: c.clone(),
                        pred: pred.clone(),
                    })
                })
                .collect();
            *p = Plan::Append { inputs, inh: None, alias: None, pw: Some(id) };
        }
    }

    /// pull_up_simple_union_all / set_append_rel_size: a filter over a UNION ALL of arms
    /// goes into each arm, its columns replaced by what the arm computes; an arm the
    /// clauses (simplified with its constants) reject drops out. Whether `p` changed.
    fn push_into_union_all(&self, p: &mut Plan) -> bool {
        let Plan::Filter { input, pred } = &*p else {
            return false;
        };
        let is_setop = |x: &Plan| matches!(x, Plan::SetOp { .. } | Plan::Append { inh: None, .. });
        if !is_setop(input) || expr_volatile_deep(pred) {
            return false;
        }
        // (An uncorrelated sub-select is an InitPlan's parameter and a statement's parameter
        // a value: set_append_rel_size gives every child the clauses as they are.)
        let mut unsafe_pred = false;
        pred.walk(&mut |x| {
            unsafe_pred |= matches!(x, Expr::Subquery(s) if !s.params.is_empty())
                || matches!(x, Expr::Agg(..) | Expr::Window(..) | Expr::Srf(..))
                || matches!(x, Expr::Outer(..));
        });
        if unsafe_pred {
            return false;
        }
        // qual_is_pushdown_safe: an operator of another btree family than the grouping's
        // equality (record_image_ops, text_pattern_ops) can tell apart rows a grouping set
        // operation merged.
        let mut foreign_op = false;
        pred.walk(&mut |x| {
            if let Expr::Call(d, args, _) = x
                && args.len() == 2
            {
                foreign_op |= matches!(
                    d.name,
                    "*=" | "*<>" | "*<" | "*<=" | "*>" | "*>=" | "~<~" | "~<=~" | "~>=~" | "~>~"
                );
            }
        });
        fn grouping_setop(p: &Plan) -> bool {
            match p {
                Plan::SetOp {
                    kind, all, left, right, ..
                } => {
                    !(*kind == crate::sql::plan::SetOpKind::Union && *all)
                        || grouping_setop(left)
                        || grouping_setop(right)
                }
                Plan::Append { inh: None, inputs, .. } => inputs.iter().any(|i| grouping_setop(i)),
                Plan::Distinct { .. } | Plan::Agg { .. } => true,
                Plan::Project { input, .. } | Plan::Filter { input, .. } => grouping_setop(input),
                _ => false,
            }
        }
        if foreign_op && grouping_setop(input) {
            return false;
        }
        // (An Append that isn't a UNION ALL's: a partitionwise join's, a rule's commands.)
        if let Plan::Append { pw: Some(id), .. } = &**input
            && self.pw_joins.borrow().iter().any(|(a, _)| a == id)
        {
            return false;
        }
        let union_all = matches!(
            &**input,
            Plan::SetOp {
                kind: crate::sql::plan::SetOpKind::Union,
                all: true,
                ..
            } | Plan::Append { inh: None, .. }
        );
        let cols = pred.columns();
        let Some(new) = self.push_qual_setop(input, pred, &cols, union_all) else {
            return false;
        };
        *p = Arc::try_unwrap(new).unwrap_or_else(|a| (*a).clone());
        true
    }

    /// subquery_push_qual / recurse_push_qual: `pred` (over the set operation's row, reading
    /// columns `cols`) into each arm of the set operation at `p`. In a UNION ALL (`drop`)
    /// an arm it rejects outright drops out. None when an arm can't take it.
    fn push_qual_setop(&self, p: &Arc<Plan>, pred: &Expr, cols: &[usize], drop: bool) -> Option<Arc<Plan>> {
        use crate::sql::plan::SetOpKind;
        match &**p {
            Plan::SetOp {
                kind,
                all,
                left,
                right,
                nondet,
                sub,
            } => {
                // (Rows a nondeterministic collation groups as equal: a qual under the
                // grouping could tell them apart.)
                if !(*kind == SetOpKind::Union && *all) && nondet.iter().any(Option::is_some) {
                    return None;
                }
                // (A UNION ALL inside the dropping one keeps dropping.)
                let d = drop && *kind == SetOpKind::Union && *all;
                let l = self.push_qual_setop(left, pred, cols, d)?;
                let r = self.push_qual_setop(right, pred, cols, d)?;
                if d {
                    let mut inputs = Vec::new();
                    for x in [l, r] {
                        match &*x {
                            Plan::Empty { .. } => {}
                            Plan::Append { inputs: xs, inh: None, .. } => inputs.extend(xs.iter().cloned()),
                            _ => inputs.push(x),
                        }
                    }
                    let ncols = crate::sql::binder::plan_width(p);
                    return Some(Arc::new(match inputs.len() {
                        0 => Plan::Empty { ncols, cols: None, info: None },
                        1 => Arc::try_unwrap(inputs.pop().unwrap()).unwrap_or_else(|a| (*a).clone()),
                        _ => Plan::Append { inputs, inh: None, alias: None, pw: None },
                    }));
                }
                Some(Arc::new(Plan::SetOp {
                    kind: *kind,
                    all: *all,
                    left: l,
                    right: r,
                    nondet: nondet.clone(),
                    sub: sub.clone(),
                }))
            }
            Plan::Append { inputs, inh: None, alias, .. } => {
                let mut out = Vec::new();
                for i in inputs {
                    let x = self.push_qual_setop(i, pred, cols, drop)?;
                    match &*x {
                        Plan::Empty { .. } if drop => {}
                        Plan::Append { inputs: xs, inh: None, .. } if drop => out.extend(xs.iter().cloned()),
                        _ => out.push(x),
                    }
                }
                let ncols = crate::sql::binder::plan_width(p);
                Some(Arc::new(match out.len() {
                    0 => Plan::Empty { ncols, cols: None, info: None },
                    1 if drop => Arc::try_unwrap(out.pop().unwrap()).unwrap_or_else(|a| (*a).clone()),
                    _ => Plan::Append { inputs: out, inh: None, alias: alias.clone(), pw: None },
                }))
            }
            // pull_up_simple_values: a one-row VALUES arm is its expressions over an empty
            // row (a Result), the clauses over them its gating quals.
            Plan::Values { .. } | Plan::Project { .. }
                if values_row(p).is_some_and(|r| r.iter().all(|e| e.columns().is_empty() && !e.is_volatile())) =>
            {
                let exprs = values_row(p).unwrap();
                self.push_qual_setop(&Arc::new(Plan::Project { input: Arc::new(Plan::One), exprs }), pred, cols, drop)
            }
            arm => {
                let (exprs, below): (Option<&Vec<Expr>>, &Plan) = match arm {
                    Plan::Project { input, exprs } => (Some(exprs), input),
                    a => (None, a),
                };
                // (Arms a filter can't go under: a LIMIT, windows, row locks, a writing
                // statement; a column read that is set-returning, volatile or a sub-select.)
                if matches!(
                    below,
                    Plan::Limit { .. } | Plan::ProjectSet { .. } | Plan::Window { .. } | Plan::LockRows { .. }
                ) || below.writes()
                {
                    return None;
                }
                if let Some(ex) = exprs
                    && cols.iter().any(|&c| {
                        ex.get(c).is_none_or(|e| {
                            let mut bad = e.is_volatile();
                            e.walk(&mut |x| bad |= matches!(x, Expr::Srf(..) | Expr::Subquery(_)));
                            bad
                        })
                    })
                {
                    return None;
                }
                let q = match exprs {
                    Some(ex) => pred
                        .clone()
                        .transform(&mut |e| {
                            Ok(match e {
                                Expr::Col(i, t) => ex.get(i).cloned().unwrap_or(Expr::Col(i, t)),
                                e => e,
                            })
                        })
                        .unwrap(),
                    None => pred.clone(),
                };
                let q = self.simplify_consts(q);
                if drop && crate::sql::binder::refute::always_false(&q) {
                    let ncols = crate::sql::binder::plan_width(p);
                    return Some(Arc::new(Plan::Empty { ncols, cols: None, info: None }));
                }
                if matches!(q, Expr::Const(Value::Bool(true), _)) {
                    return Some(p.clone());
                }
                // (Under a DISTINCT, a qual with a collation of its own may tell apart rows
                // the grouping finds equal.)
                let mut collated = false;
                q.walk(&mut |x| collated |= matches!(x, Expr::Collate(..)));
                if collated && matches!(below, Plan::Distinct { .. }) {
                    return None;
                }
                let filtered = |x: Plan| -> Plan {
                    match x {
                        // (DISTINCT commutes with a filter on its columns.)
                        Plan::Distinct { input, on: None, .. } => Plan::Distinct {
                            input: Arc::new(crate::sql::binder::optimize::add_filter(
                                &self.cat,
                                Arc::try_unwrap(input).unwrap_or_else(|a| (*a).clone()),
                                q.clone(),
                            )),
                            on: None,
                            order: 0,
                            hashed: false,
                        },
                        x => crate::sql::binder::optimize::add_filter(&self.cat, x, q.clone()),
                    }
                };
                Some(Arc::new(match arm {
                    Plan::Project { input, exprs } => Plan::Project {
                        input: Arc::new(filtered((**input).clone())),
                        exprs: exprs.clone(),
                    },
                    a => filtered(a.clone()),
                }))
            }
        }
    }

    /// Clauses with their constant parts evaluated (eval_const_expressions): AND / OR
    /// arms that are constants decide or drop out.
    fn simplify_consts(&self, e: Expr) -> Expr {
        let t = Expr::Const(Value::Bool(true), DataType::Bool);
        let f = Expr::Const(Value::Bool(false), DataType::Bool);
        match e {
            Expr::And(items) => {
                let mut keep = Vec::new();
                for i in items {
                    match self.simplify_consts(i) {
                        Expr::Const(Value::Bool(true), _) => {}
                        Expr::Const(Value::Bool(false), _) => return f,
                        Expr::And(xs) => keep.extend(xs),
                        x => keep.push(x),
                    }
                }
                match keep.len() {
                    0 => t,
                    1 => keep.pop().unwrap(),
                    _ => Expr::And(keep),
                }
            }
            Expr::Or(items) => {
                let mut keep = Vec::new();
                for i in items {
                    match self.simplify_consts(i) {
                        Expr::Const(Value::Bool(true), _) => return t,
                        Expr::Const(Value::Bool(false), _) => {}
                        x => keep.push(x),
                    }
                }
                match keep.len() {
                    0 => f,
                    1 => keep.pop().unwrap(),
                    _ => Expr::Or(keep),
                }
            }
            e if e.columns().is_empty() && !e.is_volatile() && !matches!(e, Expr::Const(..)) => {
                let mut plain = true;
                e.walk(&mut |x| {
                    plain &= !matches!(x, Expr::Subquery(_) | Expr::Param(..) | Expr::Outer(..))
                });
                if plain && let Ok(v) = crate::sql::expr::eval(&e, &[], self.ctx) {
                    let ty = e.ty();
                    return Expr::Const(v, ty);
                }
                e
            }
            e => e,
        }
    }

    /// A FROM subquery (`barrier`) grouping or DISTINCT over a table scan: the subquery
    /// with its scan read by each other index path whose order leads with a grouping key.
    /// add_paths_to_grouping_rel and create_final_distinct_paths group every path of
    /// their input relation in its useful orders, not only the cheapest one; the query
    /// above may take such a sorted grouping (set_subquery_pathlist).
    pub(super) fn grouping_variants(&self, barrier: &Plan) -> Vec<Plan> {
        fn grouping(p: &mut Plan) -> Option<&mut Plan> {
            if matches!(p, Plan::Agg { groups, sets: None, .. } if !groups.is_empty())
                || matches!(p, Plan::Distinct { on: None, .. })
            {
                return Some(p);
            }
            match p {
                Plan::Barrier { input, .. } | Plan::Project { input, .. } | Plan::Filter { input, .. } => {
                    grouping(take(input))
                }
                _ => None,
            }
        }
        fn scan_of(p: &mut Plan) -> Option<(&mut ScanPlan, Vec<Expr>, Vec<bool>)> {
            let (keys, input) = match p {
                Plan::Agg { groups, input, .. } => (groups.clone(), input),
                Plan::Distinct { input, .. } => {
                    let w = crate::sql::binder::plan_width(input);
                    ((0..w).map(|i| Expr::Col(i, DataType::Unknown)).collect(), input)
                }
                _ => return None,
            };
            let w = crate::sql::binder::plan_width(input);
            let needed = scan_needs(input, &vec![true; w]);
            let (s, mapped) = sort_target(take(input), keys)?;
            Some((s, mapped, needed))
        }
        let mut base = barrier.clone();
        let Some((s, mapped, needed)) = grouping(&mut base).and_then(scan_of) else {
            return Vec::new();
        };
        // (Its scan planned already, by the cheapest grouping: hinted_scan.)
        if !matches!(s.access, Access::Seq | Access::Index { .. } | Access::Bitmap(..))
            || s.sample.is_some()
            || s.current_of.is_some()
            || s.table.partkey.is_some()
        {
            return Vec::new();
        }
        let merge: Vec<Expr> = mapped
            .into_iter()
            .filter_map(|e| match e {
                Expr::Col(i, _) if i < s.table.columns.len() => Some(Expr::Col(i, s.table.columns[i].ty.clone())),
                _ => None,
            })
            .collect();
        if merge.is_empty() {
            return Vec::new();
        }
        let want = Want {
            keys: Vec::new(),
            limit: None,
            merge,
        };
        let Some(rel) = self.rel(s, &needed) else {
            return Vec::new();
        };
        let current = (s.access.clone(), s.index.as_ref().map(|(ix, _)| ix.oid));
        let mut out = Vec::new();
        for p in self.paths(&rel, &want) {
            let leads = matches!(p.kind, PathKind::Index(_))
                && p.keys
                    .first()
                    .is_some_and(|k| !k.desc && !k.nulls_first && want.merge.iter().any(|m| same_key(m, &k.expr)));
            if !leads {
                continue;
            }
            let mut v = barrier.clone();
            let Some((vs, _, _)) = grouping(&mut v).and_then(scan_of) else { continue };
            apply(&rel, vs, &p);
            // (Not the path the subquery reads already.)
            if format!("{:?}", (&vs.access, vs.index.as_ref().map(|(ix, _)| ix.oid))) == format!("{current:?}") {
                continue;
            }
            out.push(v);
        }
        out
    }

    /// A scan (through projections) whose order on `keys` (over its consumer's row) would
    /// be useful above: its index paths in that order considered, the cheapest taken.
    fn hinted_scan(&self, p: &mut Plan, needed: &[bool], keys: Vec<Expr>, distinct: bool) -> bool {
        let scan_needed = scan_needs(p, needed);
        let Some((s, mapped)) = sort_target(p, keys) else {
            return false;
        };
        if !plannable(s) {
            return false;
        }
        let merge: Vec<Expr> = mapped
            .into_iter()
            .filter_map(|e| match e {
                Expr::Col(i, _) if i < s.table.columns.len() => {
                    Some(Expr::Col(i, s.table.columns[i].ty.clone()))
                }
                _ => None,
            })
            .collect();
        let merge: Vec<Expr> = merge.into_iter().fold(Vec::new(), |mut v, e| {
            if !v.iter().any(|x| same_key(x, &e)) {
                v.push(e);
            }
            v
        });
        if merge.is_empty() {
            return false;
        }
        let want = Want {
            keys: Vec::new(),
            limit: None,
            merge,
        };
        let Some(rel) = self.rel(s, &scan_needed) else {
            return false;
        };
        if rel.indexes.is_empty() {
            self.plan_scan(s, &scan_needed, &want, 0);
            return true;
        }
        // The grouping over each path (create_final_distinct_paths / add_paths_to_grouping_rel,
        // as far as they differ): the cheapest path hashed or sorted, or a path already in
        // the keys' order grouped as it comes.
        let list = self.paths(&rel, &want);
        let Some(c) = cheapest(&list).cloned() else {
            return false;
        };
        let width: u32 = (0..s.table.columns.len())
            .filter(|&i| scan_needed.get(i).copied().unwrap_or(false) && !s.table.columns[i].dropped)
            .map(|i| costs::col_width(&self.cat, &s.table, i))
            .sum();
        let key_refs: Vec<&Expr> = want.merge.iter().collect();
        let groups = self.num_groups(&rel, &key_refs, c.rows);
        // (A hashed DISTINCT returns its groups at cpu_tuple_cost each, a Unique not; an
        // aggregation's do either way.)
        let hashed = (
            !self.g.hashagg as u32,
            c.total + if distinct { groups * costs::CPU_TUPLE_COST } else { 0.0 },
        );
        let sorted = (
            c.disabled + !self.g.sort as u32,
            costs::sort_cost(c.total, c.rows, width, None).1,
        );
        let other = if hashed.0 < sorted.0 || (hashed.0 == sorted.0 && hashed.1 <= sorted.1) {
            hashed
        } else {
            sorted
        };
        let mut best = c.clone();
        let mut best_cost = (c.disabled + other.0, other.1);
        for p in &list {
            let n = want.merge.len();
            let covers = p.keys.len() >= n
                && p.keys[..n].iter().all(|k| {
                    !k.desc && !k.nulls_first && want.merge.iter().any(|m| same_key(m, &k.expr))
                });
            if !covers {
                continue;
            }
            let cost = (p.disabled, p.total);
            if cost.0 < best_cost.0 || (cost.0 == best_cost.0 && cost.1 < best_cost.1) {
                best = p.clone();
                best_cost = cost;
            }
        }
        apply(&rel, s, &best);
        // (Its partial paths are built for the keys' orders too: pathkeys_useful_for_grouping.)
        s.useful = crate::sql::plan::ScanUseful(Some(Arc::new((Vec::new(), want.merge.clone()))));
        true
    }

    /// Plan a scan for `want`; whether the path chosen delivers its order.
    fn plan_scan(&self, s: &mut ScanPlan, needed: &[bool], want: &Want, width: u32) -> bool {
        // (Its partial paths are built for the same orderings.)
        s.useful = crate::sql::plan::ScanUseful(
            (!want.keys.is_empty() || !want.merge.is_empty())
                .then(|| Arc::new((want.keys.clone(), want.merge.clone()))),
        );
        let Some(mut rel) = self.rel(s, needed) else {
            return false;
        };
        // (A LATERAL relation's scan reading the outer side's values: once per outer row.)
        if let Some(loops) = self.lateral_loops.get()
            && rel.quals.iter().any(|q| {
                let mut outer = false;
                q.walk(&mut |x| outer |= matches!(x, Expr::Outer(1, ..)));
                outer
            })
        {
            rel.loop_count = loops;
        }
        if rel.indexes.is_empty()
            && want.keys.is_empty()
            && tid_quals(&rel).is_none()
            && tid_range_quals(&rel).is_empty()
        {
            s.access = Access::Seq;
            s.index = None;
            return false;
        }
        let path = self.choose(&rel, want, width);
        apply(&rel, s, &path);
        !want.keys.is_empty() && !matches!(path.kind, PathKind::Sort(..))
    }

    /// transformDistinctClause: an ORDER BY of DISTINCT's columns leads its sort (with its
    /// directions), the other columns after it; an index read in that order makes the
    /// distinct rows come out in the ORDER BY's order (no sort above). Whether `p` was
    /// planned so.
    fn ordered_distinct(&self, p: &mut Plan, needed: &[bool], lim: Option<(f64, f64)>) -> bool {
        let Plan::Sort { input, keys } = &*p else {
            return false;
        };
        let Plan::Distinct { input: x, on: None, .. } = &**input else {
            return false;
        };
        let w = crate::sql::binder::plan_width(x);
        let mut cols = Vec::new();
        for k in keys {
            match &k.expr {
                Expr::Col(j, _) if k.using.is_none() && *j < w && !cols.contains(j) => cols.push(*j),
                _ => return false,
            }
        }
        // (Only a descending order is new: an ascending one the distinct step reads
        // already.)
        if !keys.iter().any(|k| k.desc) {
            return false;
        }
        let types = crate::sql::paths::analyzejoins::col_types(x);
        let mut k2 = keys.clone();
        for j in 0..w {
            if !cols.contains(&j) {
                k2.push(SortKey {
                    expr: Expr::Col(j, types.get(j).cloned().unwrap_or(DataType::Unknown)),
                    desc: false,
                    nulls_first: false,
                    using: None,
                });
            }
        }
        let mut tmp = Plan::Sort {
            input: x.clone(),
            keys: k2,
        };
        let n = vec![true; w];
        let _ = needed;
        if !(self.ordered_scan(&mut tmp, &n, lim) && !matches!(tmp, Plan::Sort { .. })) {
            return false;
        }
        let d = Arc::new(Plan::Distinct {
            input: Arc::new(tmp),
            on: None,
            order: 0,
            hashed: false,
        });
        // (Only when the distinct step reads its input in that order, keeping it: a Unique
        // over it rather than hashing.)
        if crate::sql::explain::distinct_order(&d, self.ctx).is_none_or(|k| !k.is_empty()) {
            return false;
        }
        *p = Arc::try_unwrap(d).unwrap_or_else(|a| (*a).clone());
        true
    }

    /// get_useful_pathkeys_for_distinct: DISTINCT ON's keys reordered to an order an
    /// index of the scan below reads (the keys ordering the rows of a group after them),
    /// when that index path is taken; the ORDER BY then sorts the distinct rows. Whether
    /// `p` was planned so.
    fn reorder_distinct_on(&self, p: &mut Plan, needed: &[bool], lim: Option<(f64, f64)>) -> bool {
        let Plan::Distinct {
            input,
            on: Some(on),
            order,
            hashed: false,
        } = &*p
        else {
            return false;
        };
        let Plan::Sort { input: sin, keys } = &**input else {
            return false;
        };
        let nd = on.len();
        if !(2..=4).contains(&nd) || keys.len() < nd || keys.iter().any(|k| k.using.is_some()) {
            return false;
        }
        let (order, on) = (*order, on.clone());
        // (The keys the distinct step needs: its own, or all of them when the ORDER BY
        // has more.)
        let needed_keys: Vec<SortKey> = if order > nd { keys.clone() } else { keys[..nd].to_vec() };
        let _ = lim;
        // The original order planned as it is: an index already reading it needs nothing.
        let mut orig = (**input).clone();
        let inner_needs = child_needs(input, &child_needs(p, needed).pop().unwrap_or_default())
            .pop()
            .unwrap_or_default();
        if self.ordered_scan(&mut orig, &inner_needs, None) && !matches!(orig, Plan::Sort { .. }) {
            return false;
        }
        let perms = permutations(nd);
        // A subquery below already sorted (its own ORDER BY): the permutation whose keys
        // its order gives the most of, when that's more than the keys as written.
        let have = plan_order(self.ctx, sin);
        if !have.is_empty() {
            let prefix = |k2: &[SortKey]| {
                k2.iter()
                    .zip(&have)
                    .take_while(|(k, (j, desc, nf))| {
                        matches!(&k.expr, Expr::Col(c, _) if c == j) && k.desc == *desc && k.nulls_first == *nf
                    })
                    .count()
            };
            let perm_keys = |perm: &[usize]| {
                let mut k2: Vec<SortKey> = perm.iter().map(|&i| needed_keys[i].clone()).collect();
                k2.extend(needed_keys[nd..].iter().cloned());
                k2
            };
            let base = prefix(&perm_keys(&perms[0]));
            let best = perms
                .iter()
                .skip(1)
                .map(|perm| (prefix(&perm_keys(perm)), perm))
                .filter(|(n, _)| *n > base)
                .fold(None::<(usize, &Vec<usize>)>, |b, x| match b {
                    Some(b) if b.0 >= x.0 => Some(b),
                    _ => Some(x),
                });
            if let Some((_, perm)) = best {
                let k2 = perm_keys(perm);
                let wanted: Vec<SortKey> = keys[..order.min(keys.len())].to_vec();
                let same = |a: &SortKey, b: &SortKey| {
                    a.expr.same_as(&b.expr) && a.desc == b.desc && a.nulls_first == b.nulls_first
                };
                let satisfied = wanted.iter().zip(&k2).all(|(a, b)| same(a, b));
                let distinct = Plan::Distinct {
                    input: Arc::new(Plan::Sort {
                        input: sin.clone(),
                        keys: k2,
                    }),
                    on: Some(on),
                    order: 0,
                    hashed: false,
                };
                *p = if satisfied || wanted.is_empty() {
                    distinct
                } else {
                    Plan::Sort {
                        input: Arc::new(distinct),
                        keys: wanted,
                    }
                };
                // (Its input planned as any: the keys now in the order it has.)
                self.walk(p, needed, lim);
                return true;
            }
            return false;
        }
        for perm in perms.iter().skip(1) {
            let mut k2: Vec<SortKey> = perm.iter().map(|&i| needed_keys[i].clone()).collect();
            k2.extend(needed_keys[nd..].iter().cloned());
            let mut tmp = Plan::Sort {
                input: sin.clone(),
                keys: k2.clone(),
            };
            if !(self.ordered_scan(&mut tmp, &inner_needs, None) && !matches!(tmp, Plan::Sort { .. })) {
                continue;
            }
            // The ORDER BY over the distinct rows, unless their order has it.
            let wanted: Vec<SortKey> = keys[..order.min(keys.len())].to_vec();
            let same = |a: &SortKey, b: &SortKey| {
                a.expr.same_as(&b.expr) && a.desc == b.desc && a.nulls_first == b.nulls_first
            };
            let satisfied = wanted.iter().zip(&k2).all(|(a, b)| same(a, b));
            let distinct = Plan::Distinct {
                input: Arc::new(tmp),
                on: Some(on),
                order: 0,
                hashed: false,
            };
            *p = if satisfied || wanted.is_empty() {
                distinct
            } else {
                Plan::Sort {
                    input: Arc::new(distinct),
                    keys: wanted,
                }
            };
            return true;
        }
        false
    }

    /// A Sort over a scan (through projections): sort the cheapest path or read an index
    /// in order. Whether the scan was planned here (the Sort removed when it reads in
    /// order).
    fn ordered_scan(&self, p: &mut Plan, needed: &[bool], lim: Option<(f64, f64)>) -> bool {
        let Plan::Sort { input, keys } = p else {
            return false;
        };
        // A USING key: the order of a made operator family (by its comparison function),
        // else none an index gives.
        let mut cmps = Vec::new();
        for k in keys.iter() {
            cmps.push(match &k.using {
                None => 0,
                Some(u) => match u
                    .coll
                    .as_deref()
                    .and_then(crate::catalog::partition::comparator_fn)
                {
                    Some(f) if u.name == "<" => f,
                    _ => return false,
                },
            });
        }
        let key_exprs: Vec<Expr> = keys.iter().map(|k| k.expr.clone()).collect();
        let dirs: Vec<(bool, bool)> = keys.iter().map(|k| (k.desc, k.nulls_first)).collect();
        // The sort input's width, as EXPLAIN shows it.
        let cat = self.cat.clone();
        let width = sort_width(&cat, input);
        let inner = take(input);
        // needed columns of the scan, through the projections.
        let scan_needed = scan_needs(inner, needed);
        let Some((s, mapped)) = sort_target(inner, key_exprs) else {
            return false;
        };
        if !plannable(s) {
            return false;
        }
        let want = Want {
            merge: Vec::new(),
            keys: mapped
                .into_iter()
                .zip(dirs)
                .zip(cmps)
                .map(|((expr, (desc, nulls_first)), cmp)| PathKey {
                    expr,
                    desc,
                    nulls_first,
                    cmp,
                })
                .collect(),
            limit: lim,
        };
        let sorted = self.plan_scan(s, &scan_needed, &want, width);
        if sorted {
            if let Access::Index { ordered, .. } = &mut s.access {
                *ordered = true;
            }
            let Plan::Sort { input, .. } = std::mem::replace(p, Plan::One) else {
                unreachable!()
            };
            *p = Arc::try_unwrap(input).unwrap_or_else(|a| (*a).clone());
        }
        true
    }
}

/// The order `p`'s rows come in, by its output columns (column, descending, nulls
/// first), as far as it is known to be plain columns: a sort's, an index scan's read in
/// order, through projections; a subquery's at most its ORDER BY's keys.
pub(crate) fn plan_order(ctx: &ExecCtx, p: &Plan) -> Vec<(usize, bool, bool)> {
    match p {
        Plan::Barrier { input, sorted, .. } => {
            let mut o = plan_order(ctx, input);
            if let Some(n) = sorted {
                o.truncate(*n);
            }
            o
        }
        Plan::Project { input, exprs } => {
            let mut out = Vec::new();
            for (j, d, n) in plan_order(ctx, input) {
                match exprs.iter().position(|e| matches!(e, Expr::Col(c, _) if *c == j)) {
                    Some(i) => out.push((i, d, n)),
                    None => break,
                }
            }
            out
        }
        Plan::Sort { keys, .. } => keys
            .iter()
            .map_while(|k| match &k.expr {
                Expr::Col(j, _) if k.using.is_none() => Some((*j, k.desc, k.nulls_first)),
                _ => None,
            })
            .collect(),
        Plan::Limit { input, .. }
        | Plan::Filter { input, .. }
        | Plan::LockRows { input, .. }
        | Plan::Context { input, .. }
        | Plan::Distinct { input, on: Some(_), hashed: false, .. } => plan_order(ctx, input),
        Plan::Scan(s) if matches!(s.access, Access::Index { ordered: true, .. }) => index_order(ctx, s)
            .iter()
            .map_while(|k| match &k.expr {
                Expr::Col(j, _) if k.using.is_none() => Some((*j, k.desc, k.nulls_first)),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The columns an ordered index scan's rows come in the order of (`plan_order`'s, without
/// the directions), for a scan read in order; none otherwise.
pub(crate) fn plan_order_cols(p: &Plan) -> Vec<usize> {
    match p {
        Plan::Scan(s) if matches!(s.access, Access::Index { ordered: true, .. }) => match &s.index {
            Some((ix, _)) => ix
                .columns
                .iter()
                .map_while(|c| match &c.key {
                    crate::catalog::IndexKey::Column(j) => Some(*j),
                    _ => None,
                })
                .collect(),
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// The permutations of 0..n, the identity first.
fn permutations(n: usize) -> Vec<Vec<usize>> {
    fn go(prefix: &mut Vec<usize>, n: usize, out: &mut Vec<Vec<usize>>) {
        if prefix.len() == n {
            out.push(prefix.clone());
            return;
        }
        for i in 0..n {
            if !prefix.contains(&i) {
                prefix.push(i);
                go(prefix, n, out);
                prefix.pop();
            }
        }
    }
    let mut out = Vec::new();
    go(&mut Vec::new(), n, &mut out);
    out
}

/// The columns of the scan under projections a Sort's input needs.
fn scan_needs(p: &Plan, needed: &[bool]) -> Vec<bool> {
    match p {
        Plan::Project { input, exprs } => {
            let iw = crate::sql::binder::plan_width(input);
            let mut n = vec![false; iw];
            for (i, e) in exprs.iter().enumerate() {
                if needed.get(i).copied().unwrap_or(true) {
                    mark(&mut n, e);
                }
            }
            scan_needs(input, &n)
        }
        _ => needed.to_vec(),
    }
}

/// The width EXPLAIN gives a Sort's input: its projection's, else the table row's.
fn sort_width(cat: &crate::catalog::Catalog, p: &Plan) -> u32 {
    match p {
        Plan::Project { input, exprs } => {
            let scan = match &**input {
                Plan::Scan(s) => Some(&s.table),
                _ => None,
            };
            exprs
                .iter()
                .map(|e| match (e, scan) {
                    (Expr::Col(i, _), Some(t)) if *i < t.columns.len() => {
                        costs::col_width(cat, t, *i)
                    }
                    (e, _) => costs::type_width(&e.ty()),
                })
                .sum()
        }
        Plan::Scan(s) => (0..s.table.columns.len())
            .filter(|i| !s.table.columns[*i].dropped)
            .map(|i| costs::col_width(cat, &s.table, i))
            .sum(),
        _ => 0,
    }
}

/// Choose the access path of each table scan in a query level's plan by cost (and
/// whether a sort above one reads an index in order instead). The number of relations
/// join removal took out (range table entries the plan no longer shows).
pub fn plan_query(ctx: &ExecCtx, plan: &mut Plan) -> usize {
    plan_query_limited(ctx, plan, None)
}

/// `plan_query` for a LIMIT above it (a UNION ALL arm's: `lim`, its count and offset).
pub fn plan_query_limited(ctx: &ExecCtx, plan: &mut Plan, lim: Option<(f64, f64)>) -> usize {
    let w = crate::sql::binder::plan_width(plan);
    plan_level(ctx, plan, &vec![true; w], lim)
}

/// A locked relation of the level on the nullable side of an outer join, as planning
/// (reduce_outer_joins, then make_outerjoininfo) finds it: its lock's strength.
pub fn nullable_side_lock(ctx: &ExecCtx, plan: &Plan) -> Option<crate::sql::plan::RowMark> {
    rowmarks::nullable_mark(plan)?;
    let mut p = plan.clone();
    let planner = Planner::new(ctx);
    planner.pull_up_consts(&mut p);
    planner.reduce_outer_joins(&mut p);
    rowmarks::nullable_mark(&p)
}

/// A query level's plan, the columns of its output its consumers read being `needed`.
fn plan_level(ctx: &ExecCtx, plan: &mut Plan, needed: &[bool], lim: Option<(f64, f64)>) -> usize {
    plan_level_ordered(ctx, plan, needed, lim, None)
}

/// `plan_level`, a grouping at its top wanting the order a sort above it reads its groups
/// in (`group_dirs`).
fn plan_level_ordered(
    ctx: &ExecCtx,
    plan: &mut Plan,
    needed: &[bool],
    lim: Option<(f64, f64)>,
    dirs: Option<GroupDirs>,
) -> usize {
    let p = Planner::new(ctx);
    p.group_dirs.replace(dirs);
    sublinkcopy::copy_into_merge(plan);
    p.ref_marks.replace(rowmarks::referenced(plan));
    p.copy_marks.replace(rowmarks::copied(plan));
    p.pull_up_consts(plan);
    p.reduce_outer_joins(plan);
    p.remove_result_semijoins(plan);
    p.remove_useless_joins(plan, needed);
    p.reduce_dummies(plan);
    joinpath::whole_rows(plan, &mut p.whole_rows.borrow_mut());
    p.null_unused_level_outputs(plan, needed);
    p.drop_unused_outputs(plan, needed);
    let bound = bound_join_tree(plan);
    p.walk(plan, needed, lim);
    if !p.lifted.borrow().is_empty() {
        p.lifted_into_aggs(plan);
    }
    p.push_pw_targets(plan);
    p.minmax_aggs(plan);
    rowmarks::hoist(plan);
    crate::sql::binder::optimize::subplan_classes(plan);
    // A sub-select output a projection reads twice: a copy per place, each run apart
    // (pullup_replace_vars, make_subplan per copy).
    let whole = Arc::new(std::mem::replace(plan, Plan::One));
    if let Some(split) = crate::sql::explain::split_shared_subqueries(&whole) {
        *plan = Arc::try_unwrap(split).unwrap_or_else(|a| (*a).clone());
    } else {
        *plan = Arc::try_unwrap(whole).unwrap_or_else(|a| (*a).clone());
    }
    if let Some(b) = bound {
        note_bound_join_tree(ctx, plan, b);
    }
    p.removed.get()
}

/// The range table indexes of a plan's scans (sorted; None when one isn't known) and its
/// width.
pub(crate) fn join_tree_key(p: &Plan) -> Option<(Vec<usize>, usize)> {
    fn go(p: &Plan, out: &mut Vec<usize>) -> bool {
        if let Plan::Scan(sp) = p {
            if sp.rti.0 == 0 {
                return false;
            }
            out.push(sp.rti.0);
        }
        crate::sql::explain::plan_inputs(p).into_iter().all(|c| go(c, out))
    }
    let mut rtis = Vec::new();
    if !go(p, &mut rtis) || rtis.is_empty() {
        return None;
    }
    rtis.sort_unstable();
    Some((rtis, crate::sql::binder::plan_width(p)))
}

/// A FROM subquery's join tree as its level had it before planning, kept for the query
/// above (`ExecCtx::bound_trees`): pull_up_simple_subquery pulls the subquery's join tree up
/// before anything is planned. Used for trees with semi / anti joins (pull_up_sublinks'
/// EXISTS / IN), and for any tree a LATERAL item of the query above reads the outputs of
/// (the planned tree's parameterized nested loops would be LATERAL items of the query's
/// join problem, not its relations).
/// The planned tree that took its place is its top join marked `JoinMeta::bound` (`id`),
/// under the projections `glue` (the planned row put back in the tree's order), if any.
#[derive(Clone, Debug)]
pub struct BoundTree {
    pub id: u64,
    pub tree: Arc<Plan>,
    pub glue: Vec<Vec<Expr>>,
    /// The relations (`join_tree_key`) both trees have.
    pub key: (Vec<usize>, usize),
    /// The tree has semi / anti joins; else it is pulled up as bound only under a LATERAL
    /// item reading it (`lift`'s `Cx::unbind`).
    pub semi: bool,
}

static BOUND_IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A level's plan before planning: the projections of its select list over a join tree.
/// The projections, the tree, and whether it has a semi or anti join among its inner and
/// outer joins.
type LevelTree = (Vec<Vec<Expr>>, Plan, bool);

fn bound_join_tree(p: &Plan) -> Option<LevelTree> {
    fn has_semi(p: &Plan) -> bool {
        match p {
            Plan::Join { kind, left, right, params, meta, .. } if params.is_empty() && meta.from_call.is_none() => {
                matches!(kind, JoinKind::Semi | JoinKind::Anti) || has_semi(left) || has_semi(right)
            }
            Plan::Filter { input, .. } => matches!(&**input, Plan::Join { .. }) && has_semi(input),
            _ => false,
        }
    }
    let mut projs = Vec::new();
    let mut cur = p;
    while let Plan::Project { input, exprs } = cur {
        projs.push(exprs.clone());
        cur = input;
    }
    let j = match cur {
        Plan::Filter { input, .. } => &**input,
        j => j,
    };
    (!projs.is_empty() && matches!(j, Plan::Join { .. })).then(|| (projs, cur.clone(), has_semi(j)))
}

/// Keep a level's join tree as it was before planning (`bound_join_tree`) for the query
/// above, marking the planned tree that took its place: under the same projections, a
/// join (perhaps under a projection) of the same relations, of the same width.
fn note_bound_join_tree(ctx: &ExecCtx, plan: &mut Plan, (projs, tree, semi): LevelTree) {
    let mut cur = plan;
    for exprs in &projs {
        match cur {
            Plan::Project { input, exprs: e2 } if e2.len() == exprs.len() && e2.iter().zip(exprs.iter()).all(|(a, b)| a.same_as(b)) => {
                cur = Arc::make_mut(input);
            }
            _ => return,
        }
    }
    let Some(key) = join_tree_key(&tree) else { return };
    if join_tree_key(cur).as_ref() != Some(&key) {
        return;
    }
    // (The planned row put back in the tree's order: projections down to the planned join,
    // a removed join's columns constants.)
    let mut glue = Vec::new();
    while let Plan::Project { input, exprs } = cur {
        glue.push(exprs.clone());
        cur = Arc::make_mut(input);
    }
    let Plan::Join { meta, .. } = cur else { return };
    let id = BOUND_IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    meta.bound = id;
    meta.bound_pushed.clear();
    ctx.bound_trees.borrow_mut().push(BoundTree {
        id,
        tree: Arc::new(tree),
        glue,
        key,
        semi,
    });
}

/// Subqueries the planner makes (MIN / MAX InitPlans): ids apart from the binder's.
/// `Planner::id`s.
static PLANNER_IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

static PLANNER_SUBQUERY_IDS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(PLANNER_SUBQUERY_BASE);

/// The first id of a subquery the planner makes.
pub const PLANNER_SUBQUERY_BASE: usize = 1 << 40;

impl Planner<'_, '_> {
    /// apply_scanjoin_target_to_paths: the target list over a join made partitionwise is
    /// each child join's (no Result over the Append). Done once the level is planned: a
    /// partitionwise join is found as a FROM item of a join above by its row.
    fn push_pw_targets(&self, p: &mut Plan) {
        for c in crate::sql::explain::plan_inputs_mut(p) {
            self.push_pw_targets(take(c));
        }
        if let Plan::Project { input, exprs } = p
            && let Plan::Append { inputs, inh: None, pw: Some(id), .. } = &**input
            && exprs.iter().any(|e| !matches!(e, Expr::Col(..)))
            && exprs.iter().all(|e| {
                let mut ok = !e.is_volatile();
                e.walk(&mut |x| {
                    ok &= !matches!(x, Expr::Subquery(_) | Expr::Agg(..) | Expr::Window(..) | Expr::Srf(..) | Expr::Param(..))
                });
                ok
            })
            && self.pw_joins.borrow().iter().any(|(a, _)| a == id)
        {
            let id = *id;
            let inputs: Vec<Arc<Plan>> = inputs
                .iter()
                .map(|c| {
                    Arc::new(Plan::Project {
                        input: c.clone(),
                        exprs: exprs.clone(),
                    })
                })
                .collect();
            *p = Plan::Append { inputs, inh: None, alias: None, pw: Some(id) };
        }
    }

    /// preprocess_minmax_aggregates: an aggregation of only MIN / MAX over one table (or
    /// inheritance tree) with no grouping, as InitPlans each reading the first row of the
    /// table in that order (`IS NOT NULL` and a LIMIT 1 over the ordered scan), when that
    /// costs less.
    fn minmax_aggs(&self, p: &mut Plan) {
        {
            let kids: Vec<&mut Arc<Plan>> = match p {
                Plan::Filter { input, .. }
                | Plan::Project { input, .. }
                | Plan::ProjectSet { input, .. }
                | Plan::Agg { input, .. }
                | Plan::Window { input, .. }
                | Plan::Sort { input, .. }
                | Plan::Limit { input, .. }
                | Plan::Distinct { input, .. }
                | Plan::LockRows { input, .. }
                | Plan::Barrier { input, .. } | Plan::Context { input, .. } => vec![input],
                Plan::SetOp { left, right, .. } | Plan::Join { left, right, .. } => vec![left, right],
                Plan::Append { inputs, .. } => inputs.iter_mut().collect(),
                Plan::With { ctes, body, .. } => {
                    let mut v: Vec<&mut Arc<Plan>> = ctes.iter_mut().map(|(_, c)| c).collect();
                    v.push(body);
                    v
                }
                _ => Vec::new(),
            };
            for k in kids {
                self.minmax_aggs(Arc::make_mut(k));
            }
        }
        let Plan::Agg {
            input,
            groups,
            sets,
            aggs,
        } = &*p
        else {
            return;
        };
        // (GROUP BY (): one empty grouping set, no grouping at all; its GROUPING()
        // bitmask column is 0.)
        let empty_set = matches!(sets, Some(s) if s.len() == 1 && s[0].is_empty());
        if !groups.is_empty() || (sets.is_some() && !empty_set) || aggs.is_empty() {
            return;
        }
        // One table: its scan, or an inheritance tree's Append of its members' scans.
        let ok_input = match &**input {
            // (A partition named directly is a table of its own.)
            Plan::Scan(s) => {
                s.sample.is_none()
                    && s.mark.is_none()
                    && s.current_of.is_none()
                    && !s.with_key
            }
            Plan::Append { inh: Some(_), inputs, .. } => inputs.iter().all(|m| match &**m {
                Plan::Scan(s) => s.mark.is_none() && !s.with_key,
                Plan::Project { input, .. } => matches!(&**input, Plan::Scan(s) if s.mark.is_none() && !s.with_key),
                _ => false,
            }),
            // (A UNION ALL of single tables: an appendrel all the same.)
            Plan::Append { inh: None, inputs, .. } => inputs.iter().all(|m| match &**m {
                Plan::Scan(s) => s.mark.is_none() && !s.with_key && s.part.is_none(),
                Plan::Project { input, exprs } => {
                    matches!(&**input, Plan::Scan(s) if s.mark.is_none() && !s.with_key && s.part.is_none())
                        && exprs.iter().all(|e| !e.is_volatile() && !matches!(e, Expr::Subquery(_)))
                }
                _ => false,
            }),
            _ => false,
        };
        // (An input reading an enclosing query's values is fine: the executor redoes such
        // an InitPlan for each of them.)
        if !ok_input || aggs.iter().any(|a| a.args.iter().any(|x| x.has_outer_refs())) {
            return;
        }
        let mut cands: Vec<(Arc<Plan>, DataType)> = Vec::new();
        let mut which: Vec<usize> = Vec::new();
        let mut seen: Vec<(&str, &Expr)> = Vec::new();
        for a in aggs {
            let name = a.def.sig.name;
            if !matches!(name, "min" | "max")
                || a.args.len() != 1
                || a.distinct
                || a.filter.is_some()
                || !a.order_by.is_empty()
                || a.user.is_some()
            {
                return;
            }
            let arg = &a.args[0];
            if let Some(i) = seen.iter().position(|(n, e)| *n == name && e.same_as(arg)) {
                which.push(i);
                continue;
            }
            let mut plain = !arg.is_volatile();
            arg.walk(&mut |x| plain &= !matches!(x, Expr::Subquery(_)));
            let ty = arg.ty();
            if !plain || crate::sql::func::resolve_binop("<", &ty, &ty).is_err() {
                return;
            }
            let desc = name == "max";
            let notnull = Expr::IsNull(Box::new(arg.clone()), true);
            // (build_minmax_path: the IS NOT NULL test first among the restrictions.)
            fn first_written(p: &mut Plan, q: &Expr) {
                match p {
                    Plan::Scan(sc) => {
                        if let Some(i) = sc.written.iter().position(|w| w.same_as(q)) {
                            let w = sc.written.remove(i);
                            sc.written.insert(0, w);
                        }
                    }
                    Plan::Project { input, .. } | Plan::Filter { input, .. } => {
                        let q = q.clone();
                        first_written(Arc::make_mut(input), &q)
                    }
                    Plan::Append { inputs, .. } => {
                        for i in inputs {
                            first_written(Arc::make_mut(i), q);
                        }
                    }
                    _ => {}
                }
            }
            fn unplanned(p: &mut Plan) {
                match p {
                    Plan::Scan(sc) => {
                        sc.access = Default::default();
                        sc.index = None;
                        sc.reverse = false;
                        sc.limit = None;
                    }
                    Plan::Project { input, .. } | Plan::Filter { input, .. } => unplanned(Arc::make_mut(input)),
                    Plan::Append { inputs, .. } => {
                        for i in inputs {
                            unplanned(Arc::make_mut(i));
                        }
                    }
                    _ => {}
                }
            }
            let mut src = (**input).clone();
            unplanned(&mut src);
            // (A UNION ALL's members each take the test, as an appendrel's children do.)
            let mut src = match src {
                Plan::Append { inh: None, inputs, alias, .. } => Plan::Append {
                    inputs: inputs
                        .into_iter()
                        .map(|m| {
                            let m = Arc::try_unwrap(m).unwrap_or_else(|a| (*a).clone());
                            Arc::new(crate::sql::binder::optimize::add_filter(&self.cat, m, notnull.clone()))
                        })
                        .collect(),
                    inh: None,
                    alias,
                    pw: None,
                },
                src => crate::sql::binder::optimize::add_filter(&self.cat, src, notnull.clone()),
            };
            first_written(&mut src, &notnull);
            let proj = Plan::Project {
                input: Arc::new(src),
                exprs: vec![arg.clone()],
            };
            let sort = Plan::Sort {
                input: Arc::new(proj),
                keys: vec![crate::sql::plan::SortKey {
                    expr: Expr::Col(0, ty.clone()),
                    desc,
                    nulls_first: desc,
                    // The aggregate's own "<" (enums, non-bytewise collations): indexes keep
                    // such keys in storage order, so the scan's order isn't the aggregate's.
                    using: a.cmp.clone(),
                }],
            };
            let mut lim = Plan::Limit {
                input: Arc::new(sort),
                limit: Some(Expr::Const(Value::Int8(1), DataType::Int8)),
                offset: None,
                with_ties: None,
            };
            plan_query(self.ctx, &mut lim);
            which.push(cands.len());
            seen.push((name, arg));
            cands.push((Arc::new(lim), a.ty.clone()));
        }
        // (Disabled nodes first: a disabled sequential scan loses to the index paths.)
        let agg = crate::sql::explain::estimate(&Arc::new(p.clone()), self.ctx);
        let ests: Vec<_> = cands.iter().map(|c| crate::sql::explain::estimate(&c.0, self.ctx)).collect();
        let cost: f64 = ests.iter().map(|e| e.total).sum::<f64>() + CPU_TUPLE_COST;
        let disabled: usize = ests.iter().map(|e| e.disabled).sum();
        if disabled > agg.disabled || (disabled == agg.disabled && cost >= agg.total) {
            return;
        }
        let subs: Vec<Expr> = cands
            .into_iter()
            .map(|(c, ty)| {
                Expr::Subquery(Box::new(crate::sql::expr::Subquery {
                    id: PLANNER_SUBQUERY_IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                    kind: crate::sql::expr::SubqueryKind::Scalar,
                    plan: c,
                    params: Vec::new(),
                    left: Vec::new(),
                    ops: Vec::new(),
                    ty,
                    func: None,
                    call: None,
                    test_op: None,
                    names: Default::default(),
                    qual: false,
                    tlist: false,
                }))
            })
            .collect();
        let mut exprs: Vec<Expr> = which.iter().map(|&i| subs[i].clone()).collect();
        if empty_set {
            exprs.push(Expr::Const(Value::Int8(0), DataType::Int8));
        }
        *p = Plan::Project {
            input: Arc::new(Plan::One),
            exprs,
        };
    }
}

/// The order an index scan returns rows in: its key columns in the scan's direction,
/// those a qual fixes left out.
pub fn index_order(ctx: &ExecCtx, s: &ScanPlan) -> Vec<SortKey> {
    let Some((ix, _)) = &s.index else {
        return Vec::new();
    };
    let Ok(bound) = crate::sql::binder::bound_indexes(&s.table, ctx) else {
        return Vec::new();
    };
    let Some(b) = bound.iter().find(|b| b.index.oid == ix.oid) else {
        return Vec::new();
    };
    if matches!(ix.method.as_str(), "gist" | "spgist") {
        return s.index.as_ref().map_or_else(Vec::new, |(_, b)| b.order.clone());
    }
    if ix.method != "btree" {
        return Vec::new();
    }
    let mut quals = Vec::new();
    if let Some(f) = &s.filter {
        conjuncts(f.clone(), &mut quals);
    }
    let rel = Rel {
        table: s.table.clone(),
        size: RelSize {
            pages: 0.0,
            tuples: 0.0,
        },
        allvisfrac: 0.0,
        quals,
        rows: 0.0,
        qual_cost: 0.0,
        qual_startup: 0.0,
        needed: Vec::new(),
        other: false,
        indexes: Vec::new(),
        loop_count: 1.0,
        exprs: Vec::new(),
        booleq: None,
        written: Default::default(),
        acl: Default::default(),
        hidden: Default::default(),
        params: Default::default(),
    };
    let mut out = Vec::new();
    for (i, k) in b.keys.iter().enumerate() {
        if const_equated(&rel, k) {
            continue;
        }
        let c = &ix.columns[i];
        out.push(SortKey {
            expr: k.clone(),
            desc: c.desc != s.reverse,
            nulls_first: c.nulls_first != s.reverse,
            using: None,
        });
    }
    out
}

/// An equality under the index column's own (nondeterministic) collation: an index
/// condition as Postgres has it, though the index's byte order gives no range for it (the
/// scan checks it on each row).
fn own_collation_eq(ii: &IndexInfo, col: usize, def: &FuncDef) -> bool {
    def.name == "="
        && ii.am == Am::Btree
        && def.coll.as_ref().is_some_and(|c| !c.deterministic && ii.colls.get(col).cloned().flatten().as_deref() == Some(c.name.as_str()))
}

/// Whether a comparison agrees with the indexes' byte order: one under a collation that
/// sorts otherwise (ICU) only as equality, and only when that is byte equality
/// (deterministic).
fn byte_order_op(def: &FuncDef) -> bool {
    match &def.coll {
        None => true,
        Some(c) => c.deterministic && (c.bytewise() || def.name == "="),
    }
}

/// The one row of a one-row VALUES list (through a projection of it), as the arm's output
/// expressions.
fn values_row(p: &Plan) -> Option<Vec<Expr>> {
    match p {
        Plan::Values { rows, .. } if rows.len() == 1 => Some(rows[0].clone()),
        Plan::Project { input, exprs } => {
            let row = match &**input {
                Plan::Values { rows, .. } if rows.len() == 1 => &rows[0],
                _ => return None,
            };
            exprs
                .iter()
                .map(|e| {
                    e.clone().transform(&mut |x| {
                        Ok(match x {
                            Expr::Col(i, t) => row.get(i).cloned().unwrap_or(Expr::Col(i, t)),
                            x => x,
                        })
                    })
                })
                .collect::<crate::error::Result<Vec<_>>>()
                .ok()
        }
        _ => None,
    }
}

/// A LATERAL item's Memoize cache keys: the values it reads (`params`), those of the join
/// clauses pushed into a subquery's scan first, then its own LATERAL references
/// (paraminfo_get_equal_hashops), each once.
pub(crate) fn lateral_memo_keys(right: &Plan, params: &[Expr]) -> Vec<Expr> {
    let mut first: Vec<usize> = Vec::new();
    // (A pulled-up scan's filter reading them: the join clauses its parameterized path
    // checks, ppi_clauses; a TABLESAMPLE's arguments are lateral_vars.)
    let quals = match right {
        Plan::Filter { input, pred } if matches!(&**input, Plan::Barrier { .. }) => Some(pred),
        Plan::Scan(s) => s.filter.as_ref(),
        _ => None,
    };
    if let Some(pred) = quals {
        pred.walk(&mut |x| {
            if let Expr::Outer(1, i, _) = x
                && !first.contains(i)
            {
                first.push(*i);
            }
        });
    }
    let ordered = first
        .iter()
        .filter_map(|&i| params.get(i))
        .chain(params.iter().enumerate().filter(|(i, _)| !first.contains(i)).map(|(_, e)| e));
    let mut keys: Vec<Expr> = Vec::new();
    for e in ordered {
        if !keys.iter().any(|k| k.same_as(e)) {
            keys.push(e.clone());
        }
    }
    keys
}
