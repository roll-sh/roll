//! Postgres's planner estimates, for EXPLAIN: relation sizes (estimate_rel_size), column
//! widths, restriction and join selectivities (selfuncs.c) and the cost formulas of
//! costsize.c with the default cost parameters.

use crate::catalog::{Catalog, ColStats, IndexKey, Table};
use crate::exec::ExecCtx;
use crate::sql::expr::Expr;
use crate::types::text::FormatCtx;
use crate::types::{DataType, Value};
use std::cmp::Ordering;
use std::sync::Arc;

pub const SEQ_PAGE_COST: f64 = 1.0;
pub const CPU_TUPLE_COST: f64 = 0.01;
pub const CPU_OPERATOR_COST: f64 = 0.0025;
const DEFAULT_EQ_SEL: f64 = 0.005;
const DEFAULT_INEQ_SEL: f64 = 0.3333333333333333;
const DEFAULT_RANGE_INEQ_SEL: f64 = 0.005;
const DEFAULT_MATCH_SEL: f64 = 0.005;
const DEFAULT_UNK_SEL: f64 = 0.005;
const DEFAULT_NOT_UNK_SEL: f64 = 1.0 - DEFAULT_UNK_SEL;

/// The fixed estimates of the restriction functions (pg_operator.oprrest) of geometric,
/// array, text search, range, network and jsonb operators, for an operator the estimates
/// here don't otherwise handle: geo_selfuncs.c's contsel, areasel and positionsel, and
/// the defaults arraycontsel, tsmatchsel, rangesel, networksel and matchingsel use without
/// statistics.
fn restrict_default(d: &crate::sql::func::FuncDef) -> Option<f64> {
    let row = crate::sql::func::oper_row(d.name, d);
    Some(match row.rest {
        // contsel, areasel, positionsel.
        1302 => 0.001,
        139 => 0.005,
        1300 => 0.1,
        // arraycontsel: && overlaps, else containment.
        3817 => {
            if d.name == "&&" {
                0.01
            } else {
                0.005
            }
        }
        // tsmatchsel.
        3686 => 0.005,
        // rangesel / multirangesel (default_range_selectivity).
        3169 | 4243 => match d.name {
            "&&" => 0.01,
            "@>" | "<@" => 0.005,
            "=" => 0.005,
            _ => DEFAULT_INEQ_SEL,
        },
        // networksel.
        3560 => {
            if d.name == "&&" {
                0.01
            } else {
                0.005
            }
        }
        // matchingsel.
        5040 => 0.010,
        _ => return None,
    })
}
pub const DEFAULT_NUM_DISTINCT: f64 = 200.0;
const BLCKSZ: f64 = 8192.0;
/// work_mem (4MB), in bytes.
pub const WORK_MEM: f64 = 4096.0 * 1024.0;

/// clamp_row_est.
pub fn clamp_rows(r: f64) -> f64 {
    if r.is_nan() || r <= 1.0 {
        1.0
    } else {
        r.round_ties_even().min(1e100)
    }
}

/// get_typavgwidth: a type's typical width.
pub fn type_width(ty: &DataType) -> u32 {
    let len = crate::catalog::relsize::typlen(ty);
    if len > 0 {
        return len as u32;
    }
    let max = match ty {
        DataType::Varchar(m) | DataType::Bpchar(m) if *m > 4 => Some((*m as i64 - 4) * 4 + 4),
        DataType::Numeric(m) if *m > 4 => {
            let precision = ((*m - 4) >> 16) & 0xffff;
            Some(8 + 2 * ((precision as i64 + 6) / 4))
        }
        _ => None,
    };
    match max {
        Some(m) if matches!(ty, DataType::Bpchar(_)) => m as u32,
        Some(m) if m <= 32 => m as u32,
        Some(m) if m < 1000 => (32 + (m - 32) / 2) as u32,
        Some(_) => 32 + (1000 - 32) / 2,
        None => 32,
    }
}

/// A column's statistics (pg_statistic, not inherited).
pub fn col_stats(cat: &Catalog, table: u32, col: usize) -> Option<&ColStats> {
    // An extended statistics object's expression (pg_statistic_ext_data.stxdexpr).
    if let Some(st) = cat.statistics.get(&table) {
        return st
            .data
            .iter()
            .find(|d| !d.inherit)?
            .exprs
            .as_ref()?
            .get(col)?
            .as_ref();
    }
    // (A partitioned table has only its inherited statistics: those, for its Append.)
    let all = cat.colstats.get(&table)?;
    all.iter()
        .find(|s| s.attnum as usize == col + 1 && !s.inherit)
        .or_else(|| all.iter().find(|s| s.attnum as usize == col + 1 && s.inherit))
}

/// The statistics of a variable (a column, or an index expression).
pub fn col_stats_of<'c>(cat: &'c Catalog, v: &VarRef) -> Option<&'c ColStats> {
    v.stats(cat)
}

/// A column's width: ANALYZE's average, else the type's.
pub fn col_width(cat: &Catalog, t: &Table, col: usize) -> u32 {
    match col_stats(cat, t.oid, col) {
        Some(s) if s.width > 0 => s.width as u32,
        _ => type_width(&t.columns[col].ty),
    }
}

/// A relation's size as the planner sees it: pages and tuples.
#[derive(Clone, Copy, Debug)]
pub struct RelSize {
    pub pages: f64,
    pub tuples: f64,
}

/// estimate_rel_size (table_block_relation_estimate_size): the current page count, with
/// the tuple density pg_class gives (or, never analyzed, the one the row width suggests).
pub fn rel_size(t: &Table, ctx: &ExecCtx) -> RelSize {
    if t.oid < crate::catalog::FIRST_USER_OID
        && t.next_index_id == u32::MAX
        && let Some(st) = crate::catalog::system::SysTable::by_oid(t.oid)
    {
        return crate::sql::paths::sys_size(st, ctx);
    }
    let cat = ctx.catalog();
    let mut curpages = crate::exec::sizing::heap_blocks(t, ctx)
        .ok()
        .flatten()
        .unwrap_or(0) as f64;
    let (relpages, reltuples) = cat
        .relstats
        .get(&t.oid)
        .map_or((0.0, -1.0), |s| (s.pages as f64, s.tuples));
    // (Not for a table with inheritance children: relhassubclass, which stays set after
    // the last child is dropped, until a VACUUM.)
    if curpages < 10.0
        && reltuples < 0.0
        && cat.children(t.oid).is_empty()
        && !cat.class_has_subclass.contains(&t.oid)
    {
        curpages = 10.0;
    }
    if curpages == 0.0 {
        return RelSize {
            pages: 0.0,
            tuples: 0.0,
        };
    }
    let density = if reltuples >= 0.0 && relpages > 0.0 {
        reltuples / relpages
    } else {
        let width: u32 = (0..t.columns.len())
            .filter(|&i| !t.columns[i].dropped)
            .map(|i| col_width(&cat, t, i))
            .sum();
        // MAXALIGN(SizeofHeapTupleHeader) + sizeof(ItemIdData); the page's usable bytes
        // to its fillfactor (integer division), at least one row on every page.
        (((BLCKSZ as u32 - 24) * fillfactor(t) / 100 / (width + 28)) as f64).max(1.0)
    };
    RelSize {
        pages: curpages,
        tuples: (density * curpages).round_ties_even(),
    }
}

/// estimate_rel_size's tuples for a table just given new, empty files (relpages 0,
/// reltuples -1): ten pages at the density its columns' widths give.
pub fn empty_rel_tuples(t: &Table, ctx: &ExecCtx) -> f64 {
    let cat = ctx.catalog();
    if !cat.children(t.oid).is_empty() {
        return 0.0;
    }
    let width: u32 = (0..t.columns.len())
        .filter(|&i| !t.columns[i].dropped)
        .map(|i| col_width(&cat, t, i))
        .sum();
    let density = (((BLCKSZ as u32 - 24) * fillfactor(t) / 100 / (width + 28)) as f64).max(1.0);
    (density * 10.0).round_ties_even()
}

/// The table's fillfactor (RelationGetFillFactor; 100 by default).
fn fillfactor(t: &Table) -> u32 {
    t.options
        .iter()
        .find_map(|o| {
            let (k, v) = o.split_once('=')?;
            (k.trim().eq_ignore_ascii_case("fillfactor")).then(|| v.trim().parse::<u32>().ok())?
        })
        .filter(|f| (10..=100).contains(f))
        .unwrap_or(100)
}

/// A column of a base relation, as selectivity estimation needs it.
#[derive(Clone)]
pub struct VarRef {
    pub table: Arc<Table>,
    pub col: usize,
    /// The relation's tuples (RelOptInfo.tuples).
    pub tuples: f64,
    /// An index expression: the index's oid, its column and the type (its statistics are
    /// the index column's).
    pub expr: Option<(u32, usize, DataType)>,
    /// The relation's rows after its restrictions (RelOptInfo.rows; 0: not known).
    pub rows: f64,
    /// An inheritance parent's column over its whole tree: its inherited statistics.
    pub inh: bool,
}

impl VarRef {
    pub fn column(table: Arc<Table>, col: usize, tuples: f64) -> VarRef {
        VarRef {
            table,
            col,
            tuples,
            expr: None,
            rows: 0.0,
            inh: false,
        }
    }

    /// What the statistics are of: (relation, column).
    fn key(&self) -> (u32, usize) {
        match &self.expr {
            Some((ix, c, _)) => (*ix, *c),
            None => (self.table.oid, self.col),
        }
    }

    fn stats<'c>(&self, cat: &'c Catalog) -> Option<&'c ColStats> {
        let (rel, col) = self.key();
        if self.inh
            && self.expr.is_none()
            && let Some(s) = cat
                .colstats
                .get(&rel)
                .and_then(|all| all.iter().find(|s| s.attnum as usize == col + 1 && s.inherit))
        {
            return Some(s);
        }
        col_stats(cat, rel, col)
    }

    fn ty(&self) -> &DataType {
        match &self.expr {
            Some((_, _, t)) => t,
            None => &self.table.columns[self.col].ty,
        }
    }

    /// A unique index on just this column (no predicate).
    fn unique(&self, cat: &Catalog) -> bool {
        // (An index expression: unique when the index is, on that one expression
        // (examine_variable's isunique).)
        if let Some((ix, k, _)) = &self.expr {
            return *k == 0
                && cat.indexes.get(ix).is_some_and(|ix| {
                    ix.unique && ix.predicate.is_none() && ix.columns.len() == 1
                });
        }
        if crate::sql::paths::sys_unique_col(&self.table, self.col) {
            return true;
        }
        self.table
            .indexes
            .iter()
            .chain(&self.table.pkey_index)
            .filter_map(|o| cat.indexes.get(o))
            .any(|ix| {
                ix.unique
                    && ix.predicate.is_none()
                    && ix.columns.len() == 1
                    && matches!(ix.columns[0].key, IndexKey::Column(c) if c == self.col)
            })
    }

    /// get_variable_numdistinct: (estimate, is it the default).
    pub fn ndistinct(&self, cat: &Catalog) -> (f64, bool) {
        let s = self.stats(cat);
        let nullfrac = s.map_or(0.0, |s| s.nullfrac as f64);
        let mut d = match s {
            Some(s) => s.distinct as f64,
            None if matches!(self.ty(), DataType::Bool) => 2.0,
            None => 0.0,
        };
        if self.unique(cat) {
            d = -(1.0 - nullfrac);
        }
        if d > 0.0 {
            return (clamp_rows(d), false);
        }
        if self.tuples <= 0.0 {
            return (DEFAULT_NUM_DISTINCT, true);
        }
        if d < 0.0 {
            return (clamp_rows(-d * self.tuples), false);
        }
        if self.tuples < DEFAULT_NUM_DISTINCT {
            return (clamp_rows(self.tuples), false);
        }
        (DEFAULT_NUM_DISTINCT, true)
    }
}

/// A statistics slot's values (parsed) and numbers.
fn slot(
    s: &ColStats,
    kind: i16,
    fmt: &FormatCtx,
) -> Option<(Vec<Value>, Vec<f64>)> {
    let sl = s.slots.iter().find(|x| x.kind == kind)?;
    let numbers = sl
        .numbers
        .as_ref()
        .map(|n| n.iter().map(|x| *x as f64).collect())
        .unwrap_or_default();
    let values = match &sl.values {
        Some((ty, vals)) => slot_values(ty, vals, fmt)?,
        None => Vec::new(),
    };
    Some((values, numbers))
}

thread_local! {
    /// Statistics slots' values parsed lately (every estimate of a clause over a column
    /// parses its histogram or MCV list, which planning a custom plan per execution does
    /// again and again): by the settings' generation, the type and the texts.
    static SLOT_VALUES: std::cell::RefCell<std::collections::VecDeque<(u64, DataType, Vec<String>, std::rc::Rc<Vec<Value>>)>> =
        Default::default();
}

const SLOT_VALUES_CACHED: usize = 64;

/// A slot's values parsed as the column's type under `fmt` (None: one doesn't parse).
fn slot_values(ty: &DataType, vals: &[String], fmt: &FormatCtx) -> Option<Vec<Value>> {
    let parse = || {
        vals.iter()
            .map(|v| crate::types::text::parse_value(v, ty, fmt))
            .collect::<crate::error::Result<Vec<_>>>()
            .ok()
    };
    // (Types whose input reads nothing but the text and the settings: not enums, reg*
    // types, domains or composites, whose input reads the catalog.)
    use DataType as T;
    let plain = matches!(
        ty,
        T::Bool
            | T::Int2
            | T::Int4
            | T::Int8
            | T::Float4
            | T::Float8
            | T::Numeric(_)
            | T::Text
            | T::Varchar(_)
            | T::Bpchar(_)
            | T::Name
            | T::Date
            | T::Time(_)
            | T::Timestamp(_)
            | T::Timestamptz(_)
            | T::Interval(_)
            | T::Uuid
            | T::Oid
    );
    if fmt.settings == 0 || !plain {
        return parse();
    }
    let hit = SLOT_VALUES.with(|c| {
        c.borrow()
            .iter()
            .find(|(g, t, v, _)| *g == fmt.settings && t == ty && v.as_slice() == vals)
            .map(|e| e.3.clone())
    });
    if let Some(v) = hit {
        return Some(v.as_ref().clone());
    }
    let parsed = parse()?;
    SLOT_VALUES.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= SLOT_VALUES_CACHED {
            c.pop_front();
        }
        c.push_back((fmt.settings, ty.clone(), vals.to_vec(), std::rc::Rc::new(parsed.clone())));
    });
    Some(parsed)
}

/// Compare as the column's type does: bpchar ignores trailing spaces.
fn cmp_as(a: &Value, b: &Value, ty: &DataType) -> Ordering {
    if let (DataType::Bpchar(_), Value::Text(x), Value::Text(y)) = (ty, a, b) {
        return x.trim_end_matches(' ').cmp(y.trim_end_matches(' '));
    }
    cmp(a, b)
}

fn cmp(a: &Value, b: &Value) -> Ordering {
    crate::exec::agg::cmp_rows(std::slice::from_ref(a), std::slice::from_ref(b))
}

/// Estimation context: the catalog, and how column references resolve.
pub struct Est<'a> {
    pub cat: &'a Catalog,
    pub fmt: &'a FormatCtx,
    pub vars: &'a dyn Fn(usize) -> Option<VarRef>,
    /// Expressions with statistics of their own (index expressions).
    pub exprs: &'a [(Expr, VarRef)],
    /// get_actual_variable_range: a column's current minimum (true) or maximum, read from
    /// the data when an index has it as its first column.
    pub endpoint: Option<&'a dyn Fn(&VarRef, bool) -> Option<Value>>,
    /// A column without statistics of a small relation (a CTE's, VALUES'): its distinct
    /// count, the relation's tuples (get_variable_numdistinct).
    pub distinct: Option<&'a dyn Fn(usize) -> Option<f64>>,
    /// What clauses of user-defined operators need (None: they get no estimate of their
    /// own).
    pub user: Option<&'a UserOps<'a>>,
    /// The clauses are one relation's restrictions: `var = var` of it is no join clause
    /// (eqsel's get_restriction_variable fails: DEFAULT_EQ_SEL).
    pub same_rel: bool,
}

/// Estimating a clause of a user-defined operator (CREATE OPERATOR ... RESTRICT = eqsel,
/// scalarltsel, ...): selfuncs.c calls the operator's function on the statistics' values.
pub struct UserOps<'a> {
    pub ctx: &'a ExecCtx<'a>,
    /// statistic_proc_security_check's acl_ok: whether a column's statistics may reach a
    /// function that isn't leakproof (SELECT on it as the user the scan checks as, and no
    /// row-level security policies on its table).
    pub acl: &'a dyn Fn(&VarRef) -> bool,
    /// Expressions with statistics only leakproof functions may see (the index expressions
    /// of a table the user can't read whole).
    pub hidden: &'a [(Expr, VarRef)],
}

fn nullfrac_of(s: Option<&ColStats>) -> f64 {
    s.map_or(0.0, |s| s.nullfrac as f64)
}

/// The user-defined operator (pg_operator) a binary call shows as, with a function.
pub(crate) fn user_operator<'c>(
    cat: &'c Catalog,
    d: &crate::sql::func::FuncDef,
) -> Option<&'c crate::catalog::Operator> {
    if cat.operators.is_empty() || d.args.len() != 2 || crate::sql::func::oper_row(d.name, d).oid != 0 {
        return None;
    }
    let (schema, name) = match d.name.strip_prefix("OPERATOR(").and_then(|x| x.strip_suffix(')')) {
        Some(q) => {
            let (sc, n) = q.rsplit_once('.')?;
            (Some(sc), n)
        }
        None => (None, d.name),
    };
    cat.operators
        .values()
        .find(|o| {
            o.name == name
                && o.func != 0
                && o.left.as_ref().is_some_and(|l| l.same_base(&d.args[0]))
                && o.right.same_base(&d.args[1])
                && schema.is_none_or(|sc| cat.schema_name(o.schema) == sc)
        })
        .map(|o| &**o)
}

/// A comparison operator: (`<`-like?, includes equality?).
fn ineq(op: &str) -> Option<(bool, bool)> {
    Some(match op {
        "<" | "~<~" => (false, false),
        "<=" | "~<=~" => (false, true),
        ">" | "~>~" => (true, false),
        ">=" | "~>=~" => (true, true),
        _ => return None,
    })
}

/// The text_pattern_ops comparisons: the histogram (sorted by `<`) can't serve them.
fn pattern_cmp(op: &str) -> bool {
    matches!(op, "~<~" | "~<=~" | "~>~" | "~>=~")
}

fn commute(op: &str) -> &str {
    match op {
        "<" => ">",
        ">" => "<",
        "<=" => ">=",
        ">=" => "<=",
        o => o,
    }
}

/// An expression stripped of no-op relabelings.
pub(crate) fn strip(e: &Expr) -> &Expr {
    match e {
        Expr::Relabel(x, _) => strip(x),
        Expr::Cast(x, from, to, _) if binary_coercible(from, to) => strip(x),
        e => e,
    }
}

/// A cast to or from a string type that pg_cast has no function for: CoerceViaIO.
fn io_cast(from: &DataType, to: &DataType) -> bool {
    use DataType as T;
    let string = |t: &DataType| matches!(t, T::Text | T::Varchar(_) | T::Bpchar(_));
    let with_function = |t: &DataType| {
        matches!(
            t,
            T::Bool | T::Char | T::Name | T::Inet | T::Cidr | T::Xml | T::Regclass
        ) || string(t)
    };
    (string(to) && !with_function(from)) || (string(from) && !with_function(to))
}

fn binary_coercible(from: &DataType, to: &DataType) -> bool {
    use DataType as T;
    from == to
        // (Dropping a length modifier is a relabeling.)
        || (from.same_base(to) && to.typmod() < 0)
        || matches!(
            (from, to),
            (T::Varchar(_) | T::Text, T::Varchar(_) | T::Text) | (T::Name, T::Text)
        )
}

/// A constant (after folding): Some(None) for a non-constant one that is still a
/// pseudo-constant (a parameter).
fn constant(e: &Expr) -> Option<Option<Value>> {
    match strip(e) {
        Expr::Const(v, _) => Some(Some(v.clone())),
        Expr::Cast(x, _, _, _) => match strip(x) {
            Expr::Const(..) => Some(None),
            _ => None,
        },
        Expr::Param(..) | Expr::Outer(..) => Some(None),
        // Any other pseudo-constant: a value known only when run.
        e if crate::sql::binder::optimize::pseudo_constant(e) => Some(None),
        _ => None,
    }
}

impl Est<'_> {
    fn var(&self, e: &Expr) -> Option<VarRef> {
        match strip(e) {
            Expr::Col(i, _) => (self.vars)(*i),
            // (A COLLATE is a RelabelType once constants are folded, which examine_variable
            // looks through.)
            Expr::Collate(x, ..) => self.var(x),
            // (Postgres compares a bpchar through bpchar operators: no cast.)
            Expr::Cast(x, DataType::Bpchar(_), DataType::Text, false) => self.var(x),
            e => self
                .exprs
                .iter()
                .find(|(x, _)| x.same_as(e))
                .map(|(_, v)| v.clone()),
        }
    }

    /// clauselist_selectivity: the product, with `a > x AND a < y` pairs taken as a range.
    pub fn clauses(&self, list: &[Expr]) -> f64 {
        let mut s = 1.0;
        // (var key, lo sel, hi sel) of range pairs.
        let mut ranges: Vec<((u32, usize), Option<f64>, Option<f64>, VarRef)> = Vec::new();
        for e in list {
            if let Some((v, gt, sel)) = self.ineq_clause(e) {
                let key = v.key();
                match ranges.iter_mut().find(|r| r.0 == key) {
                    Some(r) => {
                        // addRangeClause: of two bounds on one side the tighter stays.
                        let slot = if gt { &mut r.1 } else { &mut r.2 };
                        match slot {
                            Some(x) => {
                                if *x > sel {
                                    *x = sel;
                                }
                            }
                            None => *slot = Some(sel),
                        }
                    }
                    None => ranges.push((key, gt.then_some(sel), (!gt).then_some(sel), v)),
                }
                continue;
            }
            s *= self.clause(e);
        }
        for (_, lo, hi, var) in ranges {
            let nullsel = match var.stats(self.cat) {
                Some(st) => st.nullfrac as f64,
                None => DEFAULT_UNK_SEL,
            };
            let _ = &nullsel;
            match (lo, hi) {
                (Some(lo), Some(hi)) => {
                    // (A default on either side means the estimator punted.)
                    let r = if lo == DEFAULT_INEQ_SEL || hi == DEFAULT_INEQ_SEL {
                        DEFAULT_RANGE_INEQ_SEL
                    } else {
                        // (Both bounds exclude the NULLs: add them back once.)
                        let r = hi + lo - 1.0 + nullsel;
                        if r <= 0.0 {
                            if r < -0.01 { DEFAULT_RANGE_INEQ_SEL } else { 1.0e-10 }
                        } else {
                            r
                        }
                    };
                    s *= r;
                }
                (Some(x), None) | (None, Some(x)) => s *= x,
                (None, None) => {}
            }
        }
        s
    }

    /// A `var < const`-like clause: (var, is it a lower bound (`>`), selectivity).
    fn ineq_clause(&self, e: &Expr) -> Option<(VarRef, bool, f64)> {
        let Expr::Call(d, args, _) = e else {
            return None;
        };
        if args.len() != 2 {
            return None;
        }
        let (var, c, op) = match (self.var(&args[0]), constant(&args[1])) {
            (Some(v), Some(c)) => (v, c, d.name),
            _ => match (constant(&args[0]), self.var(&args[1])) {
                (Some(c), Some(v)) => (v, c, commute(d.name)),
                _ => return None,
            },
        };
        let (gt, eq) = ineq(op)?;
        Some((
            var.clone(),
            gt,
            self.scalar_ineq(&var, c.as_ref(), gt, eq, !pattern_cmp(op)),
        ))
    }

    /// clause_selectivity.
    pub fn clause(&self, e: &Expr) -> f64 {
        match e {
            Expr::And(list) => self.clauses(list),
            Expr::Or(list) => {
                let mut s = 0.0;
                for x in list {
                    let s2 = self.clause(x);
                    s = s + s2 - s * s2;
                }
                s
            }
            Expr::Not(x) => 1.0 - self.clause(x),
            // rowcomparesel: the first pair's comparison.
            Expr::RowCompare(rc) => rc.cmps.first().map_or(DEFAULT_INEQ_SEL, |c| self.clause(c)),
            Expr::Const(Value::Bool(b), _) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            Expr::Const(Value::Null, _) => 0.0,
            Expr::IsNull(x, not) => {
                let Some(v) = self.var(x) else {
                    return if *not { DEFAULT_NOT_UNK_SEL } else { DEFAULT_UNK_SEL };
                };
                match v.stats(self.cat) {
                    Some(s) => {
                        let nf = s.nullfrac as f64;
                        if *not { 1.0 - nf } else { nf }
                    }
                    None if *not => DEFAULT_NOT_UNK_SEL,
                    None => DEFAULT_UNK_SEL,
                }
            }
            Expr::Col(..) | Expr::Relabel(..) if self.var(e).is_some() => {
                let v = self.var(e).unwrap();
                if v.stats(self.cat).is_some() {
                    self.eq_const(&v, Some(&Value::Bool(true)))
                } else {
                    0.5
                }
            }
            Expr::Call(d, args, _) if args.len() == 2 => {
                // (An equality under a nondeterministic collation: the most common values
                // compared by it, var_eq_const with the operator's collation.)
                if matches!(d.name, "=" | "<>")
                    && let Some(coll) = d.coll.as_ref().filter(|c| !c.deterministic)
                    && let Some((var, Some(c))) = match (self.var(&args[0]), constant(&args[1])) {
                        (Some(v), Some(c)) => Some((v, c)),
                        _ => match (constant(&args[0]), self.var(&args[1])) {
                            (Some(c), Some(v)) => Some((v, c)),
                            _ => None,
                        },
                    }
                {
                    let sel = self.eq_const_coll(&var, &c, coll);
                    return if d.name == "=" {
                        sel
                    } else {
                        let nf = var.stats(self.cat).map_or(0.0, |s| s.nullfrac as f64);
                        (1.0 - sel - nf).clamp(0.0, 1.0)
                    };
                }
                if let Some(o) = user_operator(self.cat, d)
                    && let Some(sel) = self.user_op(self.user, o, d, &args[0], &args[1])
                {
                    return sel;
                }
                match restrict_default(d) {
                    Some(sel) => sel,
                    None => self.binary_op(d.name, &args[0], &args[1]),
                }
            }
            Expr::Call(..) => DEFAULT_INEQ_SEL,
            // function_selectivity of a function called (not inlined): its default.
            Expr::Subquery(sq) if sq.call.is_some() => DEFAULT_INEQ_SEL,
            Expr::ArrayOp {
                left, right, op, all,
            } => {
                if let Some(s) = self.array_containment(left, right, op, *all) {
                    return s;
                }
                // scalararraysel: equality ANY of disjoint values sums, else combine.
                let items: Vec<Expr> = match strip(right) {
                    Expr::Array(items, _) => items.clone(),
                    Expr::Const(Value::Array(a), ty) => {
                        let et = ty.elem().cloned().unwrap_or(DataType::Unknown);
                        a.items.iter().map(|v| Expr::Const(v.clone(), et.clone())).collect()
                    }
                    _ => return if *all { DEFAULT_INEQ_SEL } else { 0.5 },
                };
                // (= ANY and <> ALL of distinct values: disjoint probabilities.)
                let disjoint = if *all { op.name == "<>" } else { op.name == "=" };
                let mut s = if *all { 1.0 } else { 0.0 };
                let mut sd = s;
                for item in &items {
                    let s2 = self.binary_op(op.name, left, item);
                    if *all {
                        s *= s2;
                        sd += s2 - 1.0;
                    } else {
                        s = s + s2 - s * s2;
                        sd += s2;
                    }
                }
                if disjoint && (0.0..=1.0).contains(&sd) { sd } else { s }
            }
            Expr::InList(x, list, not) => {
                // scalararraysel of `x = ANY (items)` (`x <> ALL (items)`).
                let (mut s, mut sd) = if *not { (1.0, 1.0) } else { (0.0, 0.0) };
                for (item, _) in list {
                    if *not {
                        let s2 = self.binary_op("<>", x, item);
                        s *= s2;
                        sd += s2 - 1.0;
                    } else {
                        let s2 = self.binary_op("=", x, item);
                        s = s + s2 - s * s2;
                        sd += s2;
                    }
                }
                if (0.0..=1.0).contains(&sd) { sd } else { s }
            }
            _ => 0.5,
        }
    }

    /// A user-defined operator's clause by its restriction estimator: eqsel / neqsel
    /// (var_eq_const) and scalarltsel / scalarlesel / scalargtsel / scalargesel
    /// (scalarineqsel), the most common values and the histogram's entries tested with the
    /// operator's function (a histogram not sorted by it: the fraction it accepts). Without
    /// acl_ok only a leakproof function sees them. None: another estimator.
    fn user_op(
        &self,
        u: Option<&UserOps<'_>>,
        o: &crate::catalog::Operator,
        d: &Arc<crate::sql::func::FuncDef>,
        l: &Expr,
        r: &Expr,
    ) -> Option<f64> {
        let rest = o.restrict.as_deref()?;
        // geo_selfuncs.c's fixed estimates (cube's and seg's operators name them).
        match rest {
            "contsel" => return Some(0.001),
            "areasel" => return Some(0.005),
            "positionsel" => return Some(0.1),
            _ => {}
        }
        let (neg, ineq) = match rest {
            "eqsel" => (false, false),
            "neqsel" => (true, false),
            "scalarltsel" | "scalarlesel" | "scalargtsel" | "scalargesel" => (false, true),
            _ => return None,
        };
        let default = match (ineq, neg) {
            (true, _) => DEFAULT_INEQ_SEL,
            (false, false) => DEFAULT_EQ_SEL,
            (false, true) => 1.0 - DEFAULT_EQ_SEL,
        };
        let var_of = |e: &Expr| {
            self.var(e).or_else(|| {
                let e = strip(e);
                u?.hidden.iter().find(|(x, _)| x.same_as(e)).map(|(_, v)| v.clone())
            })
        };
        let (var, c, varonleft) = match (var_of(l), constant(r)) {
            (Some(v), Some(c)) => (v, c, true),
            _ => match (constant(l), var_of(r)) {
                (Some(c), Some(v)) => (v, c, false),
                _ => return Some(default),
            },
        };
        // (scalarineqsel with the var on the right: the commutator's estimate, the default
        // without one.)
        if ineq && !varonleft && o.commutator == 0 {
            return Some(default);
        }
        // neqsel: the negator's eqsel, the default without one.
        if neg && o.negator == 0 {
            return Some(default);
        }
        let Some(c) = c else {
            // (A value known only when run: var_eq_non_const; scalarineqsel's default.)
            if ineq {
                return Some(default);
            }
            let stats = var.stats(self.cat);
            let sel = self.eq_const(&var, None);
            return Some(if neg { (1.0 - sel - nullfrac_of(stats)).clamp(0.0, 1.0) } else { sel });
        };
        if c.is_null() {
            return Some(0.0);
        }
        let leakproof = crate::sql::binder::optimize::leakproof_procs().contains(&o.func)
            || self.cat.functions.get(&o.func).is_some_and(|f| f.leakproof);
        let stats = var.stats(self.cat);
        // (Without the means to call the function: as if it may not see them.)
        let Some(u) = u else {
            return Some(if ineq {
                match stats {
                    Some(s) => (1.0 - s.nullfrac as f64) * 0.5,
                    None => DEFAULT_INEQ_SEL,
                }
            } else if var.unique(self.cat) && var.tuples >= 1.0 {
                let sel = 1.0 / var.tuples;
                if neg { 1.0 - sel - nullfrac_of(stats) } else { sel }
            } else {
                let sel = (1.0 / var.ndistinct(self.cat).0).clamp(0.0, 1.0);
                if neg { (1.0 - sel - nullfrac_of(stats)).clamp(0.0, 1.0) } else { sel }
            });
        };
        let allowed = stats.is_some() && (leakproof || (u.acl)(&var));
        let cty = if varonleft { d.args[1].clone() } else { d.args[0].clone() };
        let vty = if varonleft { d.args[0].clone() } else { d.args[1].clone() };
        let call = |x: &Value| -> Option<bool> {
            let (a, b) = (Expr::Const(x.clone(), vty.clone()), Expr::Const(c.clone(), cty.clone()));
            let args = if varonleft { vec![a, b] } else { vec![b, a] };
            match crate::sql::expr::eval(&Expr::Call(d.clone(), args, DataType::Bool), &[], u.ctx) {
                Ok(Value::Bool(b)) => Some(b),
                _ => None,
            }
        };
        let nullfrac = stats.map_or(0.0, |s| s.nullfrac as f64);
        if ineq {
            let Some(s) = stats else {
                return Some(DEFAULT_INEQ_SEL);
            };
            let (mut mcv_sel, mut sumcommon) = (0.0, 0.0);
            let mut hist = -1.0;
            if allowed {
                if let Some((vals, nums)) = slot(s, 1, self.fmt) {
                    for (x, n) in vals.iter().zip(&nums) {
                        if call(x) == Some(true) {
                            mcv_sel += n;
                        }
                        sumcommon += n;
                    }
                }
                if let Some((vals, _)) = slot(s, 2, self.fmt)
                    && vals.len() > 1
                {
                    let n = vals.len() as f64;
                    let hits = vals.iter().filter(|x| call(x) == Some(true)).count() as f64;
                    let cutoff = 0.01 / (n - 1.0);
                    hist = (hits / n).clamp(cutoff, 1.0 - cutoff);
                }
            }
            let mut sel = 1.0 - s.nullfrac as f64 - sumcommon;
            sel *= if hist >= 0.0 { hist } else { 0.5 };
            sel += mcv_sel;
            return Some(sel.clamp(0.0, 1.0));
        }
        // var_eq_const.
        let mut sel = if var.unique(self.cat) && var.tuples >= 1.0 {
            1.0 / var.tuples
        } else if allowed {
            let s = stats.unwrap();
            let mcv = slot(s, 1, self.fmt);
            let (vals, nums) = mcv.unwrap_or_default();
            let hit = vals.iter().position(|x| call(x).map(|b| b != neg) == Some(true));
            match hit {
                Some(i) => nums[i],
                None => {
                    let sumcommon: f64 = nums.iter().sum();
                    let mut sel = (1.0 - sumcommon - nullfrac).clamp(0.0, 1.0);
                    let other = var.ndistinct(self.cat).0 - nums.len() as f64;
                    if other > 1.0 {
                        sel /= other;
                    }
                    if let Some(last) = nums.last()
                        && sel > *last
                    {
                        sel = *last;
                    }
                    sel
                }
            }
        } else {
            1.0 / var.ndistinct(self.cat).0
        };
        sel = sel.clamp(0.0, 1.0);
        if neg {
            sel = 1.0 - sel - nullfrac;
        }
        Some(sel.clamp(0.0, 1.0))
    }

    /// scalararraysel_containment: `const = ANY (array column)` estimated as `column @>
    /// ARRAY[const]` from the column's most common elements (`<> ALL`: the complement).
    fn array_containment(
        &self,
        left: &Expr,
        right: &Expr,
        op: &crate::sql::func::FuncDef,
        all: bool,
    ) -> Option<f64> {
        let eq = match (op.name, all) {
            ("=", false) => true,
            ("<>", true) => false,
            _ => return None,
        };
        let Expr::Const(c, _) = strip(left) else {
            return None;
        };
        let v = self.var(right)?;
        if !matches!(v.ty(), DataType::Array(_)) {
            return None;
        }
        if c.is_null() {
            return Some(0.0);
        }
        // mcelem_array_contain_overlap_selec of one element.
        let contains = |vals: &[Value], nums: &[f64]| {
            let numbers = nums.len() == vals.len() + 3;
            let minfreq = if numbers { nums[vals.len()] } else { 2.0 * 0.005 };
            match vals.iter().position(|x| cmp(x, c) == Ordering::Equal) {
                Some(i) if numbers => nums[i],
                _ => (minfreq / 2.0).min(0.005),
            }
        };
        let mut selec = match v.stats(self.cat) {
            Some(st) => {
                let (vals, nums) = slot(st, 4, self.fmt).unwrap_or_default();
                contains(&vals, &nums) * (1.0 - st.nullfrac as f64)
            }
            None => contains(&[], &[]),
        };
        if !eq {
            selec = 1.0 - selec;
        }
        Some(selec)
    }

    fn binary_op(&self, op: &str, l: &Expr, r: &Expr) -> f64 {
        let mut varonleft = true;
        let (var, c, op) = match (self.var(l), constant(r)) {
            (Some(v), Some(c)) => (v, c, op),
            _ => match (constant(l), self.var(r)) {
                (Some(c), Some(v)) => {
                    varonleft = false;
                    (v, c, commute(op))
                }
                _ => {
                    // An expression without statistics against a NULL: the (strict)
                    // operator never holds.
                    let null = |x: &Expr| matches!(strip(x), Expr::Const(Value::Null, _));
                    let var_side = |x: &Expr| constant(x).is_none();
                    if ((null(r) && var_side(l)) || (null(l) && var_side(r)))
                        && (matches!(op, "=" | "<>") || ineq(op).is_some())
                    {
                        return 0.0;
                    }
                    // (A column of a small relation without statistics against a
                    // constant: one of its tuples' values, var_eq_const.)
                    let small = |x: &Expr| match strip(x) {
                        Expr::Col(i, _) => self.distinct.and_then(|d| d(*i)),
                        _ => None,
                    };
                    let nd = match (small(l), constant(r), constant(l), small(r)) {
                        (Some(d), Some(_), _, _) | (_, _, Some(_), Some(d)) => Some(d.max(1.0)),
                        _ => None,
                    };
                    if let Some(d) = nd {
                        match op {
                            "=" => return 1.0 / d,
                            "<>" => return 1.0 - 1.0 / d,
                            _ => {}
                        }
                    }
                    return match op {
                        "=" => {
                            // var = var of the same relation, or an expression.
                            let bare = |x: &Expr| matches!(strip(x), Expr::Col(..));
                            match (self.var(l), self.var(r)) {
                                (Some(a), Some(_)) if !self.same_rel => 1.0 / a.ndistinct(self.cat).0.max(1.0),
                                // (eqjoinsel_inner against another relation's value without
                                // statistics, a PlaceHolderVar: its distinct count the
                                // default, DEFAULT_NUM_DISTINCT.)
                                (Some(a), None) | (None, Some(a))
                                    if !self.same_rel && bare(l) && bare(r) =>
                                {
                                    let nf = nullfrac_of(a.stats(self.cat));
                                    (1.0 - nf) / a.ndistinct(self.cat).0.max(200.0)
                                }
                                _ => DEFAULT_EQ_SEL,
                            }
                        }
                        "<>" => 1.0 - DEFAULT_EQ_SEL,
                        // patternsel over an expression without statistics: a pattern
                        // without a fixed prefix by its heuristic selectivity.
                        "~~" | "~~*" | "~" | "~*" | "!~~" | "!~~*" | "!~" | "!~*"
                            if !strip(l).columns().is_empty()
                                && matches!(l.ty(), DataType::Text | DataType::Varchar(_) | DataType::Name | DataType::Bpchar(_))
                                && let Some(Some(Value::Text(t))) = constant(r) =>
                        {
                            let negate = op.starts_with('!');
                            let ptype = match op.trim_start_matches('!') {
                                "~~" => Ptype::Like,
                                "~~*" => Ptype::LikeIc,
                                "~" => Ptype::Regex,
                                _ => Ptype::RegexIc,
                            };
                            let (status, _, rest) = pattern_prefix(t.as_ref(), ptype);
                            let sel = if status == PrefixStatus::None {
                                rest.clamp(0.0001, 0.9999)
                            } else {
                                DEFAULT_MATCH_SEL
                            };
                            if negate { 1.0 - sel } else { sel }
                        }
                        "~~" | "~~*" | "~" | "~*" => DEFAULT_MATCH_SEL,
                        "!~~" | "!~~*" | "!~" | "!~*" => 1.0 - DEFAULT_MATCH_SEL,
                        o if ineq(o).is_some() => DEFAULT_INEQ_SEL,
                        _ => 0.5,
                    };
                }
            },
        };
        match op {
            "=" => self.eq_const(&var, c.as_ref()),
            "<>" => {
                let nf = var.stats(self.cat).map_or(0.0, |s| s.nullfrac as f64);
                if matches!(c, Some(Value::Null)) {
                    return 0.0;
                }
                (1.0 - self.eq_const(&var, c.as_ref()) - nf).clamp(0.0, 1.0)
            }
            "~~" | "~~*" | "~" | "~*" | "!~~" | "!~~*" | "!~" | "!~*" => {
                let negate = op.starts_with('!');
                let ptype = match op.trim_start_matches('!') {
                    "~~" => Ptype::Like,
                    "~~*" => Ptype::LikeIc,
                    "~" => Ptype::Regex,
                    _ => Ptype::RegexIc,
                };
                match (varonleft, &c) {
                    (true, Some(v)) => self.patternsel(&var, v, ptype, negate),
                    _ if negate => 1.0 - DEFAULT_MATCH_SEL,
                    _ => DEFAULT_MATCH_SEL,
                }
            }
            o => match ineq(o) {
                Some((gt, eq)) => self.scalar_ineq(&var, c.as_ref(), gt, eq, !pattern_cmp(o)),
                None => 0.5,
            },
        }
    }

    /// var_eq_const under a nondeterministic collation: the first most common value equal
    /// to the constant under it gives the frequency.
    fn eq_const_coll(&self, v: &VarRef, c: &Value, coll: &crate::types::collation::Collation) -> f64 {
        if c.is_null() {
            return 0.0;
        }
        let Some(s) = v.stats(self.cat) else {
            return self.eq_const(v, Some(c));
        };
        let nullfrac = s.nullfrac as f64;
        if let Some((vals, nums)) = slot(s, 1, self.fmt) {
            if let Some(i) = vals
                .iter()
                .position(|x| crate::types::collation::cmp_values(coll, x, c) == Ordering::Equal)
            {
                return nums.get(i).copied().unwrap_or(0.0);
            }
            let sumcommon: f64 = nums.iter().sum();
            let mut sel = (1.0 - sumcommon - nullfrac).clamp(0.0, 1.0);
            let other = v.ndistinct(self.cat).0 - nums.len() as f64;
            if other > 1.0 {
                sel /= other;
            }
            if let Some(last) = nums.last()
                && sel > *last
            {
                sel = *last;
            }
            return sel.clamp(0.0, 1.0);
        }
        self.eq_const(v, Some(c))
    }

    /// var_eq_const / var_eq_non_const.
    fn eq_const(&self, v: &VarRef, c: Option<&Value>) -> f64 {
        if matches!(c, Some(Value::Null)) {
            return 0.0;
        }
        if v.unique(self.cat) && v.tuples >= 1.0 {
            return 1.0 / v.tuples;
        }
        let Some(s) = v.stats(self.cat) else {
            return 1.0 / v.ndistinct(self.cat).0;
        };
        let nullfrac = s.nullfrac as f64;
        let mcv = slot(s, 1, self.fmt);
        let Some(c) = c else {
            let (nd, _) = v.ndistinct(self.cat);
            let mut sel = (1.0 - nullfrac) / nd;
            if let Some((_, nums)) = &mcv
                && let Some(first) = nums.first()
                && sel > *first
            {
                sel = *first;
            }
            return sel.clamp(0.0, 1.0);
        };
        if let Some((vals, nums)) = &mcv {
            if let Some(i) = vals
                .iter()
                .position(|x| cmp_as(x, c, v.ty()) == Ordering::Equal)
            {
                return nums.get(i).copied().unwrap_or(0.0);
            }
            let sumcommon: f64 = nums.iter().sum();
            let mut sel = (1.0 - sumcommon - nullfrac).clamp(0.0, 1.0);
            let other = v.ndistinct(self.cat).0 - nums.len() as f64;
            if other > 1.0 {
                sel /= other;
            }
            if let Some(last) = nums.last()
                && sel > *last
            {
                sel = *last;
            }
            return sel.clamp(0.0, 1.0);
        }
        let sel = (1.0 - nullfrac) / v.ndistinct(self.cat).0;
        sel.clamp(0.0, 1.0)
    }

    /// scalarineqsel.
    pub fn scalar_ineq(
        &self,
        v: &VarRef,
        c: Option<&Value>,
        gt: bool,
        eq: bool,
        hist_ok: bool,
    ) -> f64 {
        let (Some(s), Some(c)) = (v.stats(self.cat), c) else {
            return DEFAULT_INEQ_SEL;
        };
        if c.is_null() {
            return 0.0;
        }
        let holds = |x: &Value| {
            let o = cmp_as(x, c, v.ty());
            match (gt, eq) {
                (false, false) => o == Ordering::Less,
                (false, true) => o != Ordering::Greater,
                (true, false) => o == Ordering::Greater,
                (true, true) => o != Ordering::Less,
            }
        };
        let (mut mcv_sel, mut sumcommon) = (0.0, 0.0);
        if let Some((vals, nums)) = slot(s, 1, self.fmt) {
            for (x, n) in vals.iter().zip(&nums) {
                if holds(x) {
                    mcv_sel += n;
                }
                sumcommon += n;
            }
        }
        let hist = if hist_ok {
            self.hist_ineq(v, s, c, gt, eq)
        } else {
            // A histogram not sorted the operator's way: count the entries it accepts.
            match slot(s, 2, self.fmt) {
                Some((vals, _)) if vals.len() > 1 => {
                    let n = vals.len() as f64;
                    let hits = vals.iter().filter(|x| holds(x)).count() as f64;
                    let cutoff = 0.01 / (n - 1.0);
                    (hits / n).clamp(cutoff, 1.0 - cutoff)
                }
                _ => -1.0,
            }
        };
        let mut sel = 1.0 - s.nullfrac as f64 - sumcommon;
        sel *= if hist >= 0.0 { hist } else { 0.5 };
        sel += mcv_sel;
        sel.clamp(0.0, 1.0)
    }

    /// ineq_histogram_selectivity (-1: no histogram).
    fn hist_ineq(&self, v: &VarRef, s: &ColStats, c: &Value, gt: bool, eq: bool) -> f64 {
        let Some((mut vals, _)) = slot(s, 2, self.fmt) else {
            return -1.0;
        };
        let n = vals.len();
        if n < 2 {
            return -1.0;
        }
        // The histogram's end values give way to the column's actual ones.
        let end = |min: bool| self.endpoint.and_then(|f| f(v, min));
        let mut have_end = false;
        if n == 2
            && let (Some(a), Some(b)) = (end(true), end(false))
        {
            vals[0] = a;
            vals[1] = b;
            have_end = true;
        }
        let lt = |x: &Value| {
            let o = cmp_as(x, c, v.ty());
            let r = if gt {
                // the operator `x > c` (or >=)
                if eq { o != Ordering::Less } else { o == Ordering::Greater }
            } else if eq {
                o != Ordering::Greater
            } else {
                o == Ordering::Less
            };
            if gt { !r } else { r }
        };
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let probe = (lo + hi) / 2;
            if probe == 0 && n > 2 {
                have_end = false;
                if let Some(a) = end(true) {
                    vals[0] = a;
                    have_end = true;
                }
            } else if probe == n - 1 && n > 2 {
                have_end = false;
                if let Some(b) = end(false) {
                    vals[probe] = b;
                    have_end = true;
                }
            }
            if lt(&vals[probe]) {
                lo = probe + 1;
            } else {
                hi = probe;
            }
        }
        let histfrac = if lo == 0 {
            0.0
        } else if lo >= n {
            1.0
        } else {
            let i = lo;
            let mut eq_sel = 0.0;
            if i == 1 || gt == eq {
                let mut other = v.ndistinct(self.cat).0;
                if let Some((_, nums)) = slot(s, 1, self.fmt) {
                    other -= nums.len() as f64;
                }
                if other > 1.0 {
                    eq_sel = 1.0 / other;
                }
            }
            let binfrac = match scalars(c, &vals[i - 1], &vals[i], self.fmt) {
                Some((val, low, high)) => {
                    if high <= low {
                        0.5
                    } else if val <= low {
                        0.0
                    } else if val >= high {
                        1.0
                    } else {
                        let b = (val - low) / (high - low);
                        if b.is_nan() || !(0.0..=1.0).contains(&b) { 0.5 } else { b }
                    }
                }
                None => 0.5,
            };
            let mut h = ((i - 1) as f64 + binfrac) / (n - 1) as f64;
            if i == 1 {
                h += eq_sel * (1.0 - binfrac);
            }
            if gt == eq {
                h -= eq_sel;
            }
            h
        };
        let mut sel = if gt { 1.0 - histfrac } else { histfrac };
        // Unbelievably small or large: clamped, unless the ends are the actual ones.
        if have_end {
            sel = sel.clamp(0.0, 1.0);
        } else {
            let cutoff = 0.01 / (n - 1) as f64;
            if sel < cutoff {
                sel = cutoff;
            } else if sel > 1.0 - cutoff {
                sel = 1.0 - cutoff;
            }
        }
        sel
    }
}

impl Est<'_> {
    /// get_variable_range: a column's minimum and maximum (actual ones through an index,
    /// else the histogram's ends and the most common values).
    pub fn var_range(&self, v: &VarRef) -> Option<(Value, Value)> {
        if let Some(f) = self.endpoint
            && let (Some(a), Some(b)) = (f(v, true), f(v, false))
        {
            return Some((a, b));
        }
        let s = v.stats(self.cat)?;
        let mut range: Option<(Value, Value)> = None;
        if let Some((vals, _)) = slot(s, 2, self.fmt)
            && let (Some(a), Some(b)) = (vals.first(), vals.last())
        {
            range = Some((a.clone(), b.clone()));
        }
        if let Some((vals, _)) = slot(s, 1, self.fmt) {
            for x in vals {
                if x.is_null() {
                    continue;
                }
                range = Some(match range {
                    None => (x.clone(), x.clone()),
                    Some((lo, hi)) => (
                        if cmp_as(&x, &lo, v.ty()).is_lt() { x.clone() } else { lo },
                        if cmp_as(&x, &hi, v.ty()).is_gt() { x } else { hi },
                    ),
                });
            }
        }
        range
    }
}

/// A pattern-match operator's kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ptype {
    Like,
    LikeIc,
    Regex,
    RegexIc,
}

/// What a pattern says about the start of the strings it matches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PrefixStatus {
    None,
    Partial,
    Exact,
}

const FIXED_CHAR_SEL: f64 = 0.20;
const CHAR_RANGE_SEL: f64 = 0.25;
const ANY_CHAR_SEL: f64 = 0.9;
const FULL_WILDCARD_SEL: f64 = 5.0;
const PARTIAL_WILDCARD_SEL: f64 = 2.0;

/// like_selectivity.
fn like_selectivity(patt: &[u8]) -> f64 {
    let mut sel = 1.0;
    let mut pos = 0;
    while pos < patt.len() && (patt[pos] == b'%' || patt[pos] == b'_') {
        pos += 1;
    }
    while pos < patt.len() {
        match patt[pos] {
            b'%' => sel *= FULL_WILDCARD_SEL,
            b'_' => sel *= ANY_CHAR_SEL,
            b'\\' => {
                pos += 1;
                if pos >= patt.len() {
                    break;
                }
                sel *= FIXED_CHAR_SEL;
            }
            _ => sel *= FIXED_CHAR_SEL,
        }
        pos += 1;
    }
    sel.min(1.0)
}

/// regex_selectivity_sub.
fn regex_selectivity_sub(patt: &[u8]) -> f64 {
    let mut sel = 1.0;
    let mut depth = 0;
    let mut paren_pos = 0;
    let mut pos = 0;
    while pos < patt.len() {
        let ch = patt[pos];
        if ch == b'(' {
            if depth == 0 {
                paren_pos = pos;
            }
            depth += 1;
        } else if ch == b')' && depth > 0 {
            depth -= 1;
            if depth == 0 {
                sel *= regex_selectivity_sub(&patt[paren_pos + 1..pos]);
            }
        } else if ch == b'|' && depth == 0 {
            sel += regex_selectivity_sub(&patt[pos + 1..]);
            break;
        } else if ch == b'[' {
            let mut neg = false;
            pos += 1;
            if patt.get(pos) == Some(&b'^') {
                neg = true;
                pos += 1;
            }
            if patt.get(pos) == Some(&b']') {
                pos += 1;
            }
            while pos < patt.len() && patt[pos] != b']' {
                pos += 1;
            }
            if depth == 0 {
                sel *= if neg { 1.0 - CHAR_RANGE_SEL } else { CHAR_RANGE_SEL };
            }
        } else if ch == b'.' {
            if depth == 0 {
                sel *= ANY_CHAR_SEL;
            }
        } else if matches!(ch, b'*' | b'?' | b'+') {
            if depth == 0 {
                sel *= PARTIAL_WILDCARD_SEL;
            }
        } else if ch == b'{' {
            while pos < patt.len() && patt[pos] != b'}' {
                pos += 1;
            }
            if depth == 0 {
                sel *= PARTIAL_WILDCARD_SEL;
            }
        } else if ch == b'\\' {
            pos += 1;
            if pos >= patt.len() {
                break;
            }
            if depth == 0 {
                sel *= FIXED_CHAR_SEL;
            }
        } else if depth == 0 {
            sel *= FIXED_CHAR_SEL;
        }
        pos += 1;
    }
    sel.min(1.0)
}

/// regex_selectivity.
fn regex_selectivity(patt: &[u8], fixed_prefix_len: usize) -> f64 {
    let n = patt.len();
    let mut sel = if n > 0 && patt[n - 1] == b'$' && (n == 1 || patt[n - 2] != b'\\') {
        regex_selectivity_sub(&patt[..n - 1])
    } else {
        regex_selectivity_sub(patt) * FULL_WILDCARD_SEL
    };
    if fixed_prefix_len > 0 {
        let p = FIXED_CHAR_SEL.powi(fixed_prefix_len as i32);
        if p > 0.0 {
            sel /= p;
        }
    }
    sel.clamp(0.0, 1.0)
}

/// pattern_fixed_prefix: (status, prefix, the rest's selectivity).
pub fn pattern_prefix(patt: &str, ptype: Ptype) -> (PrefixStatus, String, f64) {
    match ptype {
        Ptype::Like | Ptype::LikeIc => {
            let ci = ptype == Ptype::LikeIc;
            let b = patt.as_bytes();
            let mut out: Vec<u8> = Vec::new();
            let mut pos = 0;
            while pos < b.len() {
                if b[pos] == b'%' || b[pos] == b'_' {
                    break;
                }
                if b[pos] == b'\\' {
                    pos += 1;
                    if pos >= b.len() {
                        break;
                    }
                }
                // (A letter varies in case: it is sort of a wildcard.)
                if ci && b[pos].is_ascii_alphabetic() {
                    break;
                }
                // Non-ASCII may vary too (the multibyte case of pattern_char_isalpha).
                if ci && b[pos] >= 0x80 {
                    break;
                }
                out.push(b[pos]);
                pos += 1;
            }
            let rest = like_selectivity(&b[pos.min(b.len())..]);
            let prefix = String::from_utf8_lossy(&out).into_owned();
            let status = if pos >= b.len() {
                PrefixStatus::Exact
            } else if !out.is_empty() {
                PrefixStatus::Partial
            } else {
                PrefixStatus::None
            };
            (status, prefix, rest)
        }
        Ptype::Regex | Ptype::RegexIc => {
            use crate::types::regex as rx;
            let ci = ptype == Ptype::RegexIc;
            let mut cflags = rx::REG_ADVANCED | rx::REG_NOSUB;
            if ci {
                cflags |= rx::REG_ICASE;
            }
            let found = rx::compile(patt, cflags, rx::DEFAULT_STRATEGY)
                .map(|re| re.fixed_prefix())
                .unwrap_or(rx::Prefix::None);
            let text = |cs: &[u32]| cs.iter().filter_map(|c| char::from_u32(*c)).collect::<String>();
            match found {
                rx::Prefix::None => (
                    PrefixStatus::None,
                    String::new(),
                    regex_selectivity(patt.as_bytes(), 0),
                ),
                rx::Prefix::Exact(cs) => (PrefixStatus::Exact, text(&cs), 1.0),
                rx::Prefix::Prefix(cs) => {
                    let p = text(&cs);
                    let rest = regex_selectivity(patt.as_bytes(), p.len());
                    (PrefixStatus::Partial, p, rest)
                }
            }
        }
    }
}

/// make_greater_string (C collation, UTF-8): the smallest-ish string greater than every
/// string starting with `s`.
pub fn greater_string(s: &str) -> Option<String> {
    let mut chars: Vec<char> = s.chars().collect();
    while let Some(last) = chars.pop() {
        let mut c = last as u32;
        loop {
            // pg_utf8_increment: the next code point of the same encoded length.
            let len = last.len_utf8();
            c += 1;
            let Some(n) = char::from_u32(c) else {
                if c > 0x10FFFF {
                    break;
                }
                continue;
            };
            if n.len_utf8() != len || (len == 1 && c >= 0x80) {
                break;
            }
            let mut out: String = chars.iter().collect();
            out.push(n);
            if out.as_str() > s {
                return Some(out);
            }
        }
    }
    None
}

impl Est<'_> {
    /// patternsel_common: LIKE / ILIKE / regular expression match selectivity.
    pub fn patternsel(&self, v: &VarRef, patt: &Value, ptype: Ptype, negate: bool) -> f64 {
        let result = if negate {
            1.0 - DEFAULT_MATCH_SEL
        } else {
            DEFAULT_MATCH_SEL
        };
        let p = match patt {
            Value::Null => return 0.0,
            Value::Text(t) => t.to_string(),
            v => match v.as_str() {
                Some(t) => t.to_string(),
                None => return result,
            },
        };
        if !matches!(
            v.ty(),
            DataType::Text | DataType::Varchar(_) | DataType::Name | DataType::Bpchar(_)
        ) {
            return result;
        }
        let s = v.stats(self.cat);
        let nullfrac = s.map_or(0.0, |s| s.nullfrac as f64);
        let (status, prefix, rest) = pattern_prefix(&p, ptype);
        let matches = |x: &Value| -> bool {
            let Some(t) = x.as_str() else {
                return false;
            };
            match ptype {
                Ptype::Like | Ptype::LikeIc => {
                    crate::sql::func::string::like_match(t, &p, Some('\\'), ptype == Ptype::LikeIc)
                        .unwrap_or(false)
                }
                Ptype::Regex | Ptype::RegexIc => {
                    use crate::types::regex as rx;
                    let mut cflags = rx::REG_ADVANCED;
                    if ptype == Ptype::RegexIc {
                        cflags |= rx::REG_ICASE;
                    }
                    rx::compile_and_match(&p, t, cflags, rx::DEFAULT_STRATEGY).unwrap_or(false)
                }
            }
        };
        let mut result = if status == PrefixStatus::Exact {
            self.eq_const(v, Some(&Value::Text(prefix.as_str().into())))
        } else {
            // histogram_selectivity (at least 10 entries, the first and last skipped).
            let (mut selec, hist_size) = match s.and_then(|s| slot(s, 2, self.fmt)) {
                Some((vals, _)) => {
                    let n = vals.len();
                    if n >= 10 {
                        let hits = vals[1..n - 1].iter().filter(|x| matches(x)).count();
                        (hits as f64 / (n - 2) as f64, n)
                    } else {
                        (-1.0, n)
                    }
                }
                None => (-1.0, 0),
            };
            if hist_size < 100 {
                let prefixsel = if status == PrefixStatus::Partial {
                    self.prefix_selectivity(v, &prefix)
                } else {
                    1.0
                };
                let heursel = prefixsel * rest;
                if selec < 0.0 {
                    selec = heursel;
                } else {
                    let w = hist_size as f64 / 100.0;
                    selec = selec.mul_add(w, heursel * (1.0 - w));
                }
            }
            selec = selec.clamp(0.0001, 0.9999);
            let (mut mcv, mut sumcommon) = (0.0, 0.0);
            if let Some((vals, nums)) = s.and_then(|s| slot(s, 1, self.fmt)) {
                for (x, f) in vals.iter().zip(&nums) {
                    if matches(x) {
                        mcv += f;
                    }
                    sumcommon += f;
                }
            }
            selec.mul_add(1.0 - nullfrac - sumcommon, mcv)
        };
        if negate {
            result = 1.0 - result - nullfrac;
        }
        result.clamp(0.0, 1.0)
    }

    /// prefix_selectivity: `x >= prefix AND x < greater(prefix)` over the histogram, at
    /// least `x = prefix`.
    fn prefix_selectivity(&self, v: &VarRef, prefix: &str) -> f64 {
        let Some(s) = v.stats(self.cat) else {
            return DEFAULT_MATCH_SEL;
        };
        let pv = Value::Text(prefix.into());
        let mut sel = self.hist_ineq(v, s, &pv, true, true);
        if sel < 0.0 {
            return DEFAULT_MATCH_SEL;
        }
        if let Some(g) = greater_string(prefix) {
            let top = self.hist_ineq(v, s, &Value::Text(g.as_str().into()), false, false);
            sel = top + sel - 1.0;
        }
        let eq = self.eq_const(v, Some(&pv));
        sel.max(eq)
    }
}

/// convert_to_scalar: a value and a histogram bin's bounds on one numeric scale.
fn scalars(v: &Value, lo: &Value, hi: &Value, fmt: &FormatCtx) -> Option<(f64, f64, f64)> {
    let num = |x: &Value| -> Option<f64> {
        Some(match x {
            Value::Int2(i) => *i as f64,
            Value::Int4(i) => *i as f64,
            Value::Int8(i) => *i as f64,
            Value::Float4(f) => *f as f64,
            Value::Float8(f) => *f,
            Value::Bool(b) => *b as u8 as f64,
            Value::Numeric(_) => crate::types::text::format_value(x, &DataType::NUMERIC, fmt)
                .parse()
                .ok()?,
            Value::Date(d) => *d as f64,
            Value::Timestamp(t) | Value::Timestamptz(t) | Value::Time(t) => *t as f64,
            _ => return None,
        })
    };
    if let (Some(a), Some(b), Some(c)) = (num(v), num(lo), num(hi)) {
        return Some((a, b, c));
    }
    let (Value::Text(a), Value::Text(b), Value::Text(c)) = (v, lo, hi) else {
        return None;
    };
    Some(strings(a.as_bytes(), b.as_bytes(), c.as_bytes()))
}

/// convert_string_to_scalar (C collation).
fn strings(value: &[u8], lo: &[u8], hi: &[u8]) -> (f64, f64, f64) {
    let (mut rlo, mut rhi) = (hi.first().copied().unwrap_or(0), hi.first().copied().unwrap_or(0));
    for &c in lo.iter().chain(hi) {
        rlo = rlo.min(c);
        rhi = rhi.max(c);
    }
    if rlo <= b'Z' && rhi >= b'A' {
        rlo = rlo.min(b'A');
        rhi = rhi.max(b'Z');
    }
    if rlo <= b'z' && rhi >= b'a' {
        rlo = rlo.min(b'a');
        rhi = rhi.max(b'z');
    }
    if rlo <= b'9' && rhi >= b'0' {
        rlo = rlo.min(b'0');
        rhi = rhi.max(b'9');
    }
    if (rhi as i32) - (rlo as i32) < 9 {
        rlo = b' ';
        rhi = 127;
    }
    let mut k = 0;
    while k < lo.len() && k < hi.len() && k < value.len() && lo[k] == hi[k] && lo[k] == value[k] {
        k += 1;
    }
    let one = |s: &[u8]| {
        let s = &s[k.min(s.len())..];
        let base = (rhi as f64) - (rlo as f64) + 1.0;
        let (mut num, mut denom) = (0.0, base);
        for &ch in s.iter().take(12) {
            let ch = if ch < rlo {
                rlo as f64 - 1.0
            } else if ch > rhi {
                rhi as f64 + 1.0
            } else {
                ch as f64
            };
            num += (ch - rlo as f64) / denom;
            denom *= base;
        }
        num
    };
    (one(value), one(lo), one(hi))
}

/// The operators and functions an expression runs per evaluation (cost_qual_eval: each is
/// cpu_operator_cost times its procost, 1 for built-ins).
pub fn ops(e: &Expr) -> usize {
    let mut n = 0;
    e.walk(&mut |x| match x {
        Expr::Call(..) => n += 1,
        Expr::Cast(_, from, to, _) if !binary_coercible(from, to) => n += 1,
        Expr::InList(_, list, _) => n += list.len(),
        _ => {}
    });
    n
}

/// cost_qual_eval's operator count with `x op ANY (array)` charged for half the array.
pub fn qual_cost(e: &Expr) -> f64 {
    let mut n = 0.0;
    e.walk(&mut |x| match x {
        Expr::Call(..) => n += 1.0,
        // (A row comparison checks every pair, once: its `=`s aren't calls of their own.)
        Expr::RowCompare(rc) => n -= rc.eqs.len() as f64,
        // A user-defined function called (not inlined): its procost.
        Expr::Subquery(s) if s.call.as_ref().is_some_and(|c| !c.inline) => {
            n += s.call.as_ref().map_or(0.0, |c| c.cost)
        }
        // An I/O conversion (no pg_cast function) runs an output and an input function.
        Expr::Cast(_, from, to, _) if io_cast(from, to) => n += 2.0,
        Expr::Cast(_, from, to, _) if !binary_coercible(from, to) => n += 1.0,
        Expr::InList(_, list, _) => n += list.len() as f64,
        // (A domain with constraints checked: cost_qual_eval_walker's one operator. One
        // without any is a RelabelType after eval_const_expressions.)
        Expr::CoerceToDomain(_, d)
            if d.tag || d.array || d.not_null || !d.checks.is_empty() || d.base.is_some() =>
        {
            n += 1.0
        }
        Expr::ArrayOp { right, .. } => {
            let len = match strip(right) {
                Expr::Array(items, _) => items.len(),
                Expr::Const(Value::Array(a), _) => a.items.len(),
                _ => 10,
            };
            n += len as f64 * 0.5;
        }
        _ => {}
    });
    n * CPU_OPERATOR_COST
}

/// eqjoinsel without MCV matching: (1 - nullfrac) products over the larger ndistinct.
/// A side's column may instead come with a known distinct count (a VALUES list's).
pub fn eq_join_sel(
    cat: &Catalog,
    a: (Option<&VarRef>, Option<f64>),
    b: (Option<&VarRef>, Option<f64>),
) -> f64 {
    let nd = |(v, known): (Option<&VarRef>, Option<f64>)| match v {
        Some(v) => {
            let nf = v.stats(cat).map_or(0.0, |s| s.nullfrac as f64);
            (v.ndistinct(cat).0, nf)
        }
        None => (known.unwrap_or(DEFAULT_NUM_DISTINCT), 0.0),
    };
    let (nd1, nf1) = nd(a);
    let (nd2, nf2) = nd(b);
    let mut sel = (1.0 - nf1) * (1.0 - nf2);
    sel /= nd1.max(nd2);
    sel.clamp(0.0, 1.0)
}

/// cost_sort: (startup, total) over an input (total cost, rows, width), `limit` bounding
/// the output.
pub fn sort_cost(input_total: f64, rows: f64, width: u32, limit: Option<f64>) -> (f64, f64) {
    let tuples = rows.max(2.0);
    let comparison = 2.0 * CPU_OPERATOR_COST;
    let input_bytes = relation_byte_size(tuples, width);
    let (output, output_bytes) = match limit {
        Some(l) if l > 0.0 && l < tuples => (l, relation_byte_size(l, width)),
        _ => (tuples, input_bytes),
    };
    let mut startup = input_total;
    if output_bytes > WORK_MEM {
        let npages = (input_bytes / BLCKSZ).ceil();
        let nruns = input_bytes / WORK_MEM;
        let mergeorder = 6.0_f64.max((WORK_MEM / (BLCKSZ * 3.0 + 1024.0 * 8.0)).floor());
        let log_runs = if nruns > mergeorder {
            (nruns.ln() / mergeorder.ln()).ceil()
        } else {
            1.0
        };
        let npageaccesses = 2.0 * npages * log_runs;
        startup += comparison * tuples * tuples.log2();
        startup += npageaccesses * (SEQ_PAGE_COST * 0.75 + 4.0 * 0.25);
    } else if tuples > 2.0 * output || input_bytes > WORK_MEM {
        // A bounded heap sort.
        startup += comparison * tuples * (2.0 * output).log2();
    } else {
        startup += comparison * tuples * tuples.log2();
    }
    (startup, startup + CPU_OPERATOR_COST * tuples)
}

/// relation_byte_size: rows of `width` with their tuple headers.
pub fn relation_byte_size(rows: f64, width: u32) -> f64 {
    let w = (width as usize).div_ceil(8) * 8 + 24;
    rows * w as f64
}

/// cost_material: (startup, total) over an input (startup, total, rows, width).
pub fn material_cost(startup: f64, total: f64, rows: f64, width: u32) -> (f64, f64) {
    let mut run = total - startup + 2.0 * CPU_OPERATOR_COST * rows;
    let bytes = relation_byte_size(rows, width);
    if bytes > WORK_MEM {
        run += SEQ_PAGE_COST * (bytes / BLCKSZ).ceil();
    }
    (startup, startup + run)
}

/// The rows a set-returning function in FROM gives: generate_series and unnest over
/// constants say exactly (their support functions), others their prorows.
pub fn function_rows(name: &str, args: &[Expr], srf: bool) -> f64 {
    let c = |e: &Expr| match strip(e) {
        Expr::Const(v, _) => Some(v.clone()),
        _ => None,
    };
    let int = |v: &Value| match v {
        Value::Int2(i) => Some(*i as f64),
        Value::Int4(i) => Some(*i as f64),
        Value::Int8(i) => Some(*i as f64),
        _ => None,
    };
    match name {
        "generate_series" if (2..=3).contains(&args.len()) => {
            let vals: Option<Vec<f64>> = args.iter().map(|a| c(a).as_ref().and_then(int)).collect();
            if let Some(v) = vals {
                let step = v.get(2).copied().unwrap_or(1.0);
                if step != 0.0 {
                    return ((v[1] - v[0]) / step + 1.0).floor().max(0.0);
                }
            }
            // generate_series_numeric_support: finite constants, a non-zero step.
            let num = |v: &Value| match v {
                Value::Numeric(n) => Some(n.to_f64()),
                _ => None,
            };
            let vals: Option<Vec<f64>> = args.iter().map(|a| c(a).as_ref().and_then(num)).collect();
            if let Some(v) = vals {
                let step = v.get(2).copied().unwrap_or(1.0);
                if step != 0.0 && v.iter().all(|x| x.is_finite()) {
                    let res = ((v[1] - v[0]) / step).trunc();
                    return if res < 0.0 { 0.0 } else { res + 1.0 };
                }
            }
            timestamp_series_rows(args).unwrap_or(1000.0)
        }
        // (With a time zone.)
        "generate_series" if args.len() == 4 => timestamp_series_rows(&args[..3]).unwrap_or(1000.0),
        // (array_unnest_support: estimate_array_length, whose default guess is 10.)
        "unnest" if args.len() == 1 => match (c(&args[0]), strip(&args[0])) {
            (Some(Value::Array(a)), _) => a.items.len() as f64,
            (Some(Value::Null), _) => 0.0,
            (_, Expr::Array(items, _)) => items.len() as f64,
            _ => 10.0,
        },
        _ if srf => 1000.0,
        _ => 1.0,
    }
}

/// generate_series_timestamp_support: constant finite bounds and step, the step taken as
/// microseconds (a month as 30 days).
fn timestamp_series_rows(args: &[Expr]) -> Option<f64> {
    let ts = |e: &Expr| match strip(e) {
        Expr::Const(Value::Timestamp(t) | Value::Timestamptz(t), _)
            if *t != i64::MIN && *t != i64::MAX =>
        {
            Some(*t as f64)
        }
        _ => None,
    };
    let (start, finish) = (ts(args.first()?)?, ts(args.get(1)?)?);
    let Expr::Const(Value::Interval(iv), _) = strip(args.get(2)?) else {
        return None;
    };
    let step = iv.micros as f64
        + iv.days as f64 * crate::types::value::USECS_PER_DAY as f64
        + iv.months as f64 * 30.0 * crate::types::value::USECS_PER_DAY as f64;
    if step == 0.0 || !step.is_finite() {
        return None;
    }
    Some(((finish - start + step) / step).floor().max(0.0))
}

const RANDOM_PAGE_COST: f64 = 4.0;
const CPU_INDEX_TUPLE_COST: f64 = 0.005;
/// effective_cache_size (4GB), in pages.
const EFFECTIVE_CACHE_SIZE: f64 = 524288.0;

/// index_pages_fetched (Mackert and Lohman), one table and its index in the query.
fn index_pages_fetched(tuples: f64, pages: f64, index_pages: f64) -> f64 {
    let t = if pages > 1.0 { pages } else { 1.0 };
    let total = (pages + index_pages).max(1.0);
    let b = (EFFECTIVE_CACHE_SIZE * t / total).max(1.0).ceil();
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

/// What an index scan's estimate needs.
pub struct IndexScan {
    pub heap: RelSize,
    pub index_pages: f64,
    pub tree_height: u32,
    /// The index quals' selectivity, and the leading (boundary) ones'.
    pub index_sel: f64,
    pub boundary_sel: f64,
    /// A unique index with every key column compared for equality.
    pub unique_eq: bool,
    pub nquals: usize,
    pub correlation: f64,
    /// The other quals' cost per row.
    pub filter_cost: f64,
}

/// btcostestimate + cost_index (one scan, no index-only): (startup, total).
pub fn index_scan_cost(s: &IndexScan) -> (f64, f64) {
    let tuples = s.heap.tuples;
    let mut n_index_tuples = if s.unique_eq { 1.0 } else { s.boundary_sel * tuples };
    n_index_tuples = n_index_tuples.round_ties_even();
    if n_index_tuples < 1.0 {
        n_index_tuples = 1.0;
    }
    let index_pages = if s.index_pages > 1.0 && tuples > 1.0 {
        (n_index_tuples * s.index_pages / tuples).ceil()
    } else {
        1.0
    };
    let mut index_startup = 0.0;
    let mut index_total = index_pages * RANDOM_PAGE_COST
        + n_index_tuples * (CPU_INDEX_TUPLE_COST + CPU_OPERATOR_COST * s.nquals as f64);
    if tuples > 1.0 {
        let descent = tuples.log2().ceil() * CPU_OPERATOR_COST;
        index_startup += descent;
        index_total += descent;
    }
    let descent = (s.tree_height as f64 + 1.0) * 50.0 * CPU_OPERATOR_COST;
    index_startup += descent;
    index_total += descent;
    // The heap.
    let fetched = clamp_rows(s.index_sel * tuples);
    let pages = s.heap.pages;
    let max_io = index_pages_fetched(fetched, pages, s.index_pages) * RANDOM_PAGE_COST;
    let pf = index_pages_fetched((s.index_sel * pages).ceil(), pages, s.index_pages);
    let min_io = if pf > 0.0 {
        RANDOM_PAGE_COST + (pf - 1.0) * SEQ_PAGE_COST
    } else {
        0.0
    };
    let csq = s.correlation * s.correlation;
    let run = index_total - index_startup
        + max_io
        + csq * (min_io - max_io)
        + (CPU_TUPLE_COST + s.filter_cost) * fetched;
    (index_startup, index_startup + run)
}

/// The correlation ANALYZE found for a column (0 without one).
pub fn correlation(cat: &Catalog, table: u32, col: usize) -> f64 {
    col_stats(cat, table, col)
        .and_then(|s| s.slots.iter().find(|x| x.kind == 3))
        .and_then(|x| x.numbers.as_ref()?.first().copied())
        .map_or(0.0, |c| c as f64)
}

/// cost_incremental_sort: (startup, total) sorting groups of `groups` presorted runs.
pub fn incremental_sort_cost(
    input_startup: f64,
    input_total: f64,
    rows: f64,
    width: u32,
    groups: f64,
    limit: Option<f64>,
) -> (f64, f64) {
    let tuples = rows.max(2.0);
    let groups = groups.clamp(1.0, tuples);
    let group_tuples = tuples / groups;
    let group_input_run = (input_total - input_startup) / groups;
    let (gs, gt) = sort_cost(0.0, group_tuples, width, limit);
    let group_run = gt - gs;
    let startup = gs + input_startup + group_input_run;
    let mut run = group_run + (group_run + gs) * (groups - 1.0) + group_input_run * (groups - 1.0);
    run += CPU_TUPLE_COST * tuples;
    run += 2.0 * CPU_TUPLE_COST * groups;
    (startup, startup + run)
}
