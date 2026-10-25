//! EXPLAIN (explain.c): our plan shown as the tree Postgres prints, in text, JSON, YAML or XML.
//!
//! The plan is converted into Postgres's node vocabulary first: projections and filters fold
//! into the node below them, hash joins get their `Hash` child, windows their `Sort`, and
//! subqueries become numbered SubPlan / InitPlan sections. ANALYZE reads the per-node
//! counters the executor keeps in `ExecCtx::instr`.

mod parallel;
mod pwagg;

use super::binder::ast::{const_str, inner, with_pos};
use super::expr::{Expr, Subquery, SubqueryKind};
use super::plan::{
    AggCall, ConflictAction, Frame, FrameBound, FrameExclude, FrameUnit, JoinKind, MergeCmd, Plan,
    ScanPlan, SetOpKind, SortKey, WinFunc, WinKind, WindowCall,
};
use super::costs;
use super::ruleutils::{SubplanLabel, deparse_explain};
use crate::catalog::quote_ident;
use crate::error::{Error, Result, code};
use crate::exec::{ExecCtx, Instr, NodeStats};
use crate::types::{DataType, Value};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    Text,
    Json,
    Yaml,
    Xml,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub analyze: bool,
    pub verbose: bool,
    pub costs: bool,
    pub buffers: bool,
    pub wal: bool,
    pub settings: bool,
    pub generic_plan: bool,
    /// Explaining a SCROLL cursor's query: a plan that can't run backwards gets a
    /// Materialize on top.
    pub scroll: bool,
    pub timing: bool,
    pub summary: bool,
    pub memory: bool,
    pub serialize: bool,
    /// SERIALIZE BINARY (else TEXT).
    pub serialize_binary: bool,
    pub format: Format,
}

/// A restriction or inner join clause as it comes out of its equivalence class: an `=`
/// with no collation (its sides' implicit collations conflict) has its non-constant
/// operands relabelled to none (canonicalize_ec_expression), which shows as a cast.
fn ec_relabel(e: &Expr) -> Expr {
    match e {
        Expr::And(list) => Expr::And(list.iter().map(ec_relabel).collect()),
        Expr::Call(d, args, ty)
            if d.name == "="
                && args.len() == 2
                && crate::sql::binder::is_indeterminate(d)
                && !args.iter().any(|a| matches!(a, Expr::Const(..))) =>
        {
            let args = args
                .iter()
                .map(|a| match a {
                    Expr::Cast(..) => a.clone(),
                    a => Expr::Collate(Box::new(a.clone()), String::new(), -1),
                })
                .collect();
            Expr::Call(d.clone(), args, ty.clone())
        }
        e => e.clone(),
    }
}

/// An inner (or semi) join clause `name = text` as it comes out of its equivalence class:
/// the comparison's collation is the `name` side's C (a non-default implicit collation beats
/// the default), so canonicalize_ec_expression relabels the other operand, a text-family
/// expression reading columns, to it, which shows as a cast. (An outer join's clause keeps
/// its form; a sub-select's value, a parameter, or a constant shows as it is.) A scan's own
/// restriction (`scan`, reading no other relation's value) is the clause as written: a class
/// of two members from one clause gives it back (generate_base_implied_equalities_const);
/// one the class derived comes relabeled already (`optimize::ec_constants`).
fn name_text_relabel(e: &Expr, scan: bool) -> Expr {
    match e {
        Expr::And(list) => Expr::And(list.iter().map(|x| name_text_relabel(x, scan)).collect()),
        Expr::Call(..) if scan && !e.has_outer_refs() => e.clone(),
        Expr::Call(d, args, ty) if d.name == "=" && args.len() == 2 => {
            let texty = |a: &Expr| {
                matches!(a.ty(), DataType::Text | DataType::Varchar(_))
                    && !matches!(a, Expr::Const(..) | Expr::Cast(..) | Expr::Collate(..))
                    && (!a.columns().is_empty() || a.has_outer_refs())
                    && {
                        let mut sub = false;
                        a.walk(&mut |x| sub |= matches!(x, Expr::Subquery(_) | Expr::Param(..)));
                        !sub
                    }
            };
            // (The name side another relation's column: a restriction against a
            // pseudoconstant, `text_col = CURRENT_USER`, is its class's one source clause,
            // which generate_base_implied_equalities_const uses as written.)
            let name = |a: &Expr| matches!(a.ty(), DataType::Name) && (!a.columns().is_empty() || a.has_outer_refs());
            let (a, b) = (&args[0], &args[1]);
            let args = if name(a) && texty(b) {
                vec![a.clone(), Expr::Collate(Box::new(b.clone()), String::new(), -1)]
            } else if name(b) && texty(a) {
                vec![Expr::Collate(Box::new(a.clone()), String::new(), -1), b.clone()]
            } else {
                return e.clone();
            };
            Expr::Call(d.clone(), args, ty.clone())
        }
        e => e.clone(),
    }
}

/// The parameters marked GUC_EXPLAIN, by name.
const EXPLAIN_GUCS: &[&str] = &[
    "cpu_index_tuple_cost",
    "cpu_operator_cost",
    "cpu_tuple_cost",
    "cursor_tuple_fraction",
    "debug_parallel_query",
    "effective_cache_size",
    "effective_io_concurrency",
    "enable_async_append",
    "enable_bitmapscan",
    "enable_distinct_reordering",
    "enable_gathermerge",
    "enable_group_by_reordering",
    "enable_hashagg",
    "enable_hashjoin",
    "enable_incremental_sort",
    "enable_indexonlyscan",
    "enable_indexscan",
    "enable_material",
    "enable_memoize",
    "enable_mergejoin",
    "enable_nestloop",
    "enable_parallel_append",
    "enable_parallel_hash",
    "enable_partition_pruning",
    "enable_partitionwise_aggregate",
    "enable_partitionwise_join",
    "enable_presorted_aggregate",
    "enable_self_join_elimination",
    "enable_seqscan",
    "enable_sort",
    "enable_tidscan",
    "from_collapse_limit",
    "geqo",
    "geqo_effort",
    "geqo_generations",
    "geqo_pool_size",
    "geqo_seed",
    "geqo_selection_bias",
    "geqo_threshold",
    "hash_mem_multiplier",
    "jit",
    "jit_above_cost",
    "jit_inline_above_cost",
    "jit_optimize_above_cost",
    "join_collapse_limit",
    "maintenance_io_concurrency",
    "max_parallel_workers",
    "max_parallel_workers_per_gather",
    "min_parallel_index_scan_size",
    "min_parallel_table_scan_size",
    "parallel_leader_participation",
    "parallel_setup_cost",
    "parallel_tuple_cost",
    "plan_cache_mode",
    "random_page_cost",
    "recursive_worktable_factor",
    "search_path",
    "seq_page_cost",
    "temp_buffers",
    "work_mem",
];

/// show_buffer_usage's counters in a structured format.
const BUFFER_FIELDS: [&str; 10] = [
    "Shared Hit Blocks",
    "Shared Read Blocks",
    "Shared Dirtied Blocks",
    "Shared Written Blocks",
    "Local Hit Blocks",
    "Local Read Blocks",
    "Local Dirtied Blocks",
    "Local Written Blocks",
    "Temp Read Blocks",
    "Temp Written Blocks",
];

/// `defGetBoolean`.
fn def_bool(d: &pb::DefElem) -> Result<bool> {
    let Some(arg) = d.arg.as_deref() else {
        return Ok(true);
    };
    let v = const_str(arg).unwrap_or_default().to_ascii_lowercase();
    match v.as_str() {
        "true" | "on" | "1" => Ok(true),
        "false" | "off" | "0" => Ok(false),
        _ => Err(Error::new(
            code::SYNTAX_ERROR,
            format!("{} requires a Boolean value", d.defname),
        )),
    }
}

/// `ParseExplainOptionList`.
pub fn options(list: &[pb::Node]) -> Result<Options> {
    let mut o = Options {
        analyze: false,
        verbose: false,
        costs: true,
        buffers: false,
        wal: false,
        settings: false,
        generic_plan: false,
        scroll: false,
        timing: false,
        summary: false,
        memory: false,
        serialize: false,
        serialize_binary: false,
        format: Format::Text,
    };
    let (mut timing, mut summary, mut buffers) = (None, None, None);
    let bad_value = |d: &pb::DefElem, v: &str| {
        with_pos(
            Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!(
                    "unrecognized value for EXPLAIN option \"{}\": \"{v}\"",
                    d.defname
                ),
            ),
            d.location,
        )
    };
    for n in list {
        let Some(NodeEnum::DefElem(d)) = inner(n) else {
            continue;
        };
        match d.defname.as_str() {
            "analyze" => o.analyze = def_bool(d)?,
            "verbose" => o.verbose = def_bool(d)?,
            "costs" => o.costs = def_bool(d)?,
            "buffers" => buffers = Some(def_bool(d)?),
            "wal" => o.wal = def_bool(d)?,
            "settings" => o.settings = def_bool(d)?,
            "generic_plan" => o.generic_plan = def_bool(d)?,
            "timing" => timing = Some(def_bool(d)?),
            "summary" => summary = Some(def_bool(d)?),
            "memory" => o.memory = def_bool(d)?,
            "serialize" => match d.arg.as_deref().and_then(const_str) {
                None => o.serialize = true,
                Some(v) => match v.as_str() {
                    "none" => o.serialize = false,
                    "text" | "binary" => {
                        o.serialize = true;
                        o.serialize_binary = v == "binary";
                    }
                    _ => return Err(bad_value(d, &v)),
                },
            },
            "format" => {
                let v = d.arg.as_deref().and_then(const_str).unwrap_or_default();
                o.format = match v.as_str() {
                    "text" => Format::Text,
                    "json" => Format::Json,
                    "yaml" => Format::Yaml,
                    "xml" => Format::Xml,
                    _ => return Err(bad_value(d, &v)),
                };
            }
            other => {
                return Err(with_pos(
                    Error::new(
                        code::SYNTAX_ERROR,
                        format!("unrecognized EXPLAIN option \"{other}\""),
                    ),
                    d.location,
                ));
            }
        }
    }
    let requires = |what: &str| {
        Error::new(
            code::INVALID_PARAMETER_VALUE,
            format!("EXPLAIN option {what} requires ANALYZE"),
        )
    };
    if o.wal && !o.analyze {
        return Err(requires("WAL"));
    }
    o.timing = timing.unwrap_or(o.analyze);
    o.buffers = buffers.unwrap_or(o.analyze);
    if o.timing && !o.analyze {
        return Err(requires("TIMING"));
    }
    if o.serialize && !o.analyze {
        return Err(requires("SERIALIZE"));
    }
    if o.generic_plan && o.analyze {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "EXPLAIN options ANALYZE and GENERIC_PLAN cannot be used together",
        ));
    }
    o.summary = summary.unwrap_or(o.analyze);
    Ok(o)
}

// ── the Postgres-shaped tree ──

/// A column as an expression would print it: bare, and qualified with its relation.
#[derive(Clone, Debug)]
struct Name {
    plain: String,
    full: String,
}

impl Name {
    fn col(rel: &str, col: &str) -> Name {
        Name {
            plain: quote_ident(col),
            full: format!("{}.{}", quote_ident(rel), quote_ident(col)),
        }
    }

    fn same(s: impl Into<String>) -> Name {
        let s = s.into();
        Name {
            plain: s.clone(),
            full: s,
        }
    }

    /// How a parent node refers to a value this node computes (ruleutils' `get_variable` on
    /// a non-Var target entry).
    fn wrapped(&self) -> Name {
        Name {
            plain: format!("({})", self.plain),
            full: format!("({})", self.full),
        }
    }
}

/// Where an expression is shown, which decides whether columns get relation prefixes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cx {
    /// A scan's own quals: prefixed only in VERBOSE.
    Scan,
    /// Join / sort / group / upper quals: prefixed when the statement has several relations.
    Upper,
    /// `Output:` lists.
    Output,
}

#[derive(Clone, Debug)]
enum Removed {
    /// Rows a scan's own filter rejected.
    Scan(usize),
    /// Rows the input node produced that this one didn't pass on.
    Diff(usize),
}

#[derive(Clone, Debug)]
enum Detail {
    Text(&'static str, String),
    List(&'static str, Vec<String>),
    /// "Rows Removed by …" under ANALYZE.
    Removed(&'static str, Vec<Removed>),
    SortInfo,
    /// An Incremental Sort's full-sort and presorted groups under ANALYZE.
    IncSortInfo,
    HashAggInfo,
    HashInfo,
    /// An index scan's descents (by its `ScanPlan`).
    IndexSearches(usize),
    /// An index-only scan's heap fetches (by its `ScanPlan`).
    HeapFetches(usize),
    /// A bitmap heap scan's pages read (by its `ScanPlan`).
    HeapBlocks(usize),
    /// A Memoize's cache hits, misses and memory.
    MemoizeInfo,
    /// A Gather's (or Gather Merge's) workers planned, and launched under ANALYZE.
    Workers(usize),
    /// A ModifyTable's result relations when there are several (an inheritance tree): text
    /// line and structured properties of each.
    Targets(Vec<(String, Vec<(&'static str, String, bool)>)>),
    /// A MERGE's tuples inserted, updated, deleted and skipped under ANALYZE (by its
    /// source's key).
    MergeTuples(usize),
    /// A sorted grouping (GroupAggregate, Group): the group keys its input is sorted on, in
    /// order (key, descending, NULLS FIRST; `sort_dir`). Not shown: what the executor
    /// emits its groups by. (Here rather than a field: `ExNode`s fill big stack frames.)
    GroupSort(Vec<(usize, bool, bool)>),
}

/// Set a node's `Detail::GroupSort` (none: empty).
fn put_group_sort(details: &mut Vec<Detail>, keys: Vec<(usize, bool, bool)>) {
    details.retain(|d| !matches!(d, Detail::GroupSort(_)));
    if !keys.is_empty() {
        details.push(Detail::GroupSort(keys));
    }
}

/// A node's `Detail::GroupSort` (empty: none).
fn group_sort_of(details: &[Detail]) -> Vec<(usize, bool, bool)> {
    details
        .iter()
        .find_map(|d| match d {
            Detail::GroupSort(k) => Some(k.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

impl Detail {
    fn analyze_info(&self) -> bool {
        matches!(
            self,
            Detail::SortInfo
                | Detail::IncSortInfo
                | Detail::HashAggInfo
                | Detail::HashInfo
                | Detail::IndexSearches(_)
                | Detail::HeapFetches(_)
                | Detail::HeapBlocks(_)
                | Detail::MemoizeInfo
        )
    }
}

/// An output column of a node: its estimated width, and the base relation column it is.
#[derive(Clone, Default)]
struct ColInfo {
    width: u32,
    var: Option<costs::VarRef>,
    /// A distinct count known otherwise (a VALUES list's columns are taken as unique).
    distinct: Option<f64>,
}

impl std::fmt::Debug for ColInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.width)
    }
}

#[derive(Clone, Debug, Default)]
struct ExNode {
    /// The resjunk GROUP BY keys a level's grouping returns (`group_tops`): the sorts and
    /// limits above it return them too (`carry_group_junk`).
    junk_keys: Vec<String>,
    /// A scan: the columns its join clauses read, as they came into its reltarget
    /// (`ScanPlan::reltarget`), named as its output names them.
    rel_order: Vec<String>,
    /// Output columns (empty: not tracked).
    cols: Vec<ColInfo>,
    /// The order rows come out in, as sort keys show (empty: none known).
    order: Vec<String>,
    /// Per `order` key, other names for it (an inner merge join's other side's key).
    order_eq: Vec<Vec<String>>,
    /// A sorted DISTINCT's Unique: the columns it is sorted on, in order.
    unique_keys: Option<Vec<usize>>,
    /// A full partitionwise aggregation: its HAVING is checked in each partition.
    pw_full: bool,
    /// A WindowAgg: the window calls it computes (their positions among the node's
    /// calls) and how each is shown.
    win_calls: Vec<(usize, String)>,
    /// An Append member's columns as a node above names them (a set operation's sort).
    out_names: Vec<String>,
    /// A nested loop whose inner side the executor reads once and keeps (no Materialize
    /// shown, not parameterized): Postgres rescans it for every outer row.
    inner_cached: bool,
    node_type: &'static str,
    strategy: Option<&'static str>,
    partial_mode: Option<&'static str>,
    operation: Option<&'static str>,
    relationship: Option<&'static str>,
    subplan_name: Option<String>,
    /// The text-format line (node name and target).
    title: String,
    /// Structured formats: the target and join properties, in order.
    target: Vec<(&'static str, String, bool)>,
    rows: f64,
    width: u32,
    startup_cost: f64,
    total_cost: f64,
    output: Vec<String>,
    details: Vec<Detail>,
    /// The plan node whose counters these are.
    key: Option<usize>,
    /// The planner had to use it although its enable_* setting is off.
    disabled: bool,
    initplans: Vec<ExNode>,
    /// A MIN / MAX InitPlan (the planner's `minmax_aggs`): the Result running it pays for
    /// it (create_minmaxagg_path), not the level's top node; `charged` once it has.
    minmax_init: bool,
    charged: bool,
    /// An EXISTS InitPlan: it reads its first row only (cost_subplan charges that).
    exists_init: bool,
    /// An Append member's place in its relids order: a Parallel Append's members costing
    /// the same keep it (append_startup_cost_compare's bms_compare).
    rank: usize,
    /// A UNION ALL arm kept a subquery: its plan (0: not one). Its relations are named as
    /// setrefs reaches it, in the order the final plan has the arms.
    arm: usize,
    children: Vec<ExNode>,
    subplans: Vec<ExNode>,
    /// A parallel-aware node (Parallel Seq Scan, Parallel Append, …).
    parallel_aware: bool,
    /// The workers a partial plan (or a Gather) has.
    workers: usize,
    /// A scan a worker could run (whether or not it gets workers itself).
    par_safe: bool,
    /// Another path to the same rows the node above may do better with (a Gather Merge
    /// beside a Sort, for a LIMIT).
    alt: Option<Box<ExNode>>,
    /// This node's output as a partial plan (rows spread over parallel workers), when
    /// one can make it: what a Gather above would run (`parallel`).
    partial: Option<Box<ExNode>>,
    /// The relation's other partial paths add_partial_path keeps beside `partial` for
    /// their order (a scan's parallel index scans): what a Gather Merge above may run as
    /// it is, or sorted incrementally (generate_useful_gather_paths).
    partials: Vec<ExNode>,
    /// A Sort's input as its relation gives it, with its partial plans (`partial`,
    /// `partials`), when it has some: a merge join's sorted inputs are one of the join
    /// relation's paths; its partial paths (hashing, a nested loop) read the inputs
    /// unsorted. With the input's own names (`Built::internal`).
    presort: Option<Box<(ExNode, Vec<Name>)>>,
    /// A Hash: the columns of its input's rows the table keeps (those the join returns or
    /// checks), whose sizes EXPLAIN ANALYZE's batch counts go by.
    hash_cols: Vec<usize>,
    /// A scan reading an index in the order the query's ORDER BY wants (the planner
    /// took it for the Sort): per wanted key, the column it reads. A plain Gather of its
    /// partial plan would lose that order (`settle` gathers it only in order).
    wanted_order: Vec<Option<usize>>,
    /// A node with a Gather in it: the cheapest plan of the same rows a worker could run
    /// (get_cheapest_parallel_safe_total_inner), for a Parallel Append to share out whole.
    safe_alt: Option<Box<ExNode>>,
    /// A Sort a LIMIT above bounds (ExecSetTupleBound): the rows it keeps.
    bound: Option<f64>,
    /// An Incremental Sort: the presorted groups its costs assume.
    presorted_groups: Option<f64>,
    /// Its target list is in the order built (a sort's input target): `tlist_layout`
    /// leaves it, and the nodes below it, as they are.
    fixed_tlist: bool,
    /// A scan under a pulled-up subquery's selection of its columns (a projection of
    /// plain columns, no node of Postgres's): the scan's physical target list, which it
    /// returns where its consumer takes any (`physical_input`).
    physical: Vec<String>,
    /// A join: the tuples its quals other than the hash / merge clauses are checked on
    /// (final_cost_nestloop's ntuples, final_cost_hashjoin's hashjointuples,
    /// final_cost_mergejoin's mergejointuples), what a filter over it costs per row of.
    qual_tuples: Option<f64>,
}

impl ExNode {
    fn new(node_type: &'static str) -> ExNode {
        ExNode {
            node_type,
            title: node_type.to_string(),
            ..ExNode::default()
        }
    }

    /// What an InitPlan costs the node that pays for it (SS_charge_for_initplans):
    /// all of it, or an EXISTS's first row.
    fn init_charge(&self) -> f64 {
        if self.exists_init {
            self.startup_cost + (self.total_cost - self.startup_cost) / costs::clamp_rows(self.rows)
        } else {
            self.total_cost
        }
    }

    fn child(mut self, rel: &'static str) -> ExNode {
        self.relationship = Some(rel);
        self
    }

    /// The node's partial plans: the cheapest and the ordered ones (`partials`).
    fn partials_mut(&mut self) -> impl Iterator<Item = &mut ExNode> {
        self.partial.as_deref_mut().into_iter().chain(self.partials.iter_mut())
    }

    fn is_scan(&self) -> bool {
        self.node_type.ends_with("Scan")
            && (self.children.is_empty() || self.node_type == "Bitmap Heap Scan")
    }

    fn is_join(&self) -> bool {
        matches!(self.node_type, "Hash Join" | "Nested Loop" | "Merge Join")
    }

    /// Add a detail before the ANALYZE-only ones.
    fn push_detail(&mut self, d: Detail) {
        let at = self
            .details
            .iter()
            .position(Detail::analyze_info)
            .unwrap_or(self.details.len());
        self.details.insert(at, d);
    }

    /// Add `label` (Filter / Join Filter), ANDed with an existing one, and its removed-rows
    /// counter.
    fn add_qual(
        &mut self,
        label: &'static str,
        removed_label: &'static str,
        text: String,
        removed: Option<Removed>,
    ) {
        if let Some(Detail::Text(_, t)) = self
            .details
            .iter_mut()
            .find(|d| matches!(d, Detail::Text(l, _) if *l == label))
        {
            *t = format!("({t} AND {text})");
        } else {
            self.push_detail(Detail::Text(label, text));
        }
        let Some(r) = removed else { return };
        match self
            .details
            .iter_mut()
            .find(|d| matches!(d, Detail::Removed(l, _) if *l == removed_label))
        {
            Some(Detail::Removed(_, list)) => list.push(r),
            _ => {
                let at = self
                    .details
                    .iter()
                    .position(|d| matches!(d, Detail::Text(l, _) if *l == label))
                    .map_or(self.details.len(), |i| i + 1);
                self.details
                    .insert(at, Detail::Removed(removed_label, vec![r]));
            }
        }
    }
}

/// A converted plan node and the names of its output columns: as it computes them, and as
/// the node above refers to them.
struct Built {
    node: ExNode,
    internal: Vec<Name>,
    exported: Vec<Name>,
}

impl Built {
    fn passthrough(node: ExNode, names: Vec<Name>) -> Built {
        Built {
            node,
            internal: names.clone(),
            exported: names,
        }
    }
}

struct Builder<'c, 'a> {
    ctx: &'c ExecCtx<'a>,
    o: &'c Options,
    /// More than one relation in the statement: upper expressions qualify columns.
    prefix: bool,
    /// Scan node address → its (unique) reference name.
    refnames: HashMap<usize, String>,
    labels: HashMap<usize, SubplanLabel>,
    plan_ids: usize,
    /// Column names of enclosing queries' rows (`Outer(1)` first), always qualified.
    outer: Vec<Vec<String>>,
    /// The query level (`depth`) each of `outer`'s rows was pushed at: a lateral join's
    /// row is its level's own, a subquery's parameters the enclosing level's.
    outer_depth: Vec<usize>,
    /// Query levels entered: subqueries planned apart (and SubPlans).
    depth: usize,
    /// SubPlans found in the expressions of the node being built.
    pending: Vec<ExNode>,
    /// InitPlans per query level.
    inits: Vec<Vec<ExNode>>,
    /// Parameter values: a custom plan shows them in place of `$n`.
    params: &'c [crate::types::Value],
    /// The CTEs' estimated rows and columns, by name.
    ctes: HashMap<String, (f64, Vec<ColInfo>)>,
    /// The output columns each plan node's consumers use (by node address).
    needed: HashMap<usize, Vec<bool>>,
    /// Scans of the other tables an UPDATE / DELETE reads: their ctid is a row mark.
    rowmarks: std::collections::HashSet<usize>,
    /// Those scans' row mark columns as their target lists show them (`ref.ctid`).
    rowmark_junk: HashMap<usize, Vec<((u8, super::plan::Rti), String)>>,
    /// The sources of no table's rows with a whole-row mark (ROW_MARK_COPY: a FROM
    /// subquery, a function, VALUES, a CTE of a locking level or a data-modifying
    /// statement), by address, and once built, their whole-row Var's width.
    copy_marks: HashMap<usize, Option<u32>>,
    /// The query levels' select-list projections whose constants no node above refers to
    /// (`plain_const_lists`).
    plain_consts: std::collections::HashSet<usize>,
    /// Correlated EXISTS sub-selects with a hashed alternative (AlternativeSubPlan), by
    /// subquery id: each place reading one picks the alternative for how often it runs
    /// there (fix_alternative_subplan), an inheritance tree's members each their own.
    alts: HashMap<usize, AltSubPlan>,
    /// Nested loop parameters holding sub-selects (a sub-select copy in an inner scan's
    /// clause, the value passed in): the outer frame they are (`outer`'s length once
    /// pushed), the parameters, and where the outer side's column names start in that
    /// frame. A place reading one shows the sub-select itself (`inline_sub_params`),
    /// registered there, as Postgres keeps the SubPlan in the inner clause and passes its
    /// arguments.
    sub_params: Vec<(usize, Vec<Expr>, usize)>,
    /// Registering sub-selects without attaching their SubPlans to the node being built:
    /// expressions the executor doesn't initialize under EXPLAIN without ANALYZE (an
    /// index-only scan's index conditions, a bitmap index scan's).
    detach: bool,
    /// SubPlans labeled under `detach`, attached by the first place that registers them.
    unattached: HashMap<usize, ExNode>,
    /// Correlated EXISTS sub-selects both of whose alternatives a build before showed:
    /// the hashed one's relations are named apart.
    fresh_hashed: std::rc::Rc<std::collections::HashSet<usize>>,
    /// A MERGE's empty inheritance target: its name (its dummy relation's columns are
    /// qualified by it).
    dummy_target: Option<String>,
    /// How often the expressions being shown run (setrefs' NUM_EXEC_QUAL /
    /// NUM_EXEC_TLIST): an EXISTS that can be hashed picks its plan by it.
    num_exec: Option<f64>,
    /// The statement's top plan node (its target list keeps resjunk columns).
    top: usize,
    /// The parallel query settings, when plans can have workers.
    par: Option<super::paths::parallel::ParCfg>,
    /// Converting the members of an inheritance tree's Append.
    member_scan: bool,
    /// An ORDER BY over the aggregation being converted: the group keys (by position)
    /// it sorts by, in its order.
    group_order: Option<Vec<(usize, String)>>,
    /// That ORDER BY is all group keys (`group_order` covers it).
    group_order_full: bool,
    /// Each group key that ORDER BY names, with its direction (transformGroupClause gives
    /// the GROUP BY item the first matching ORDER BY item's sort operator).
    group_named: Vec<(usize, String)>,
    /// Relations the planner keeps whole (a CTE Scan's): their reference names and column
    /// counts. A row of all their columns is their whole-row Var, `r.*`.
    whole_vars: HashMap<String, usize>,
    /// Columns of FROM subqueries proven empty before they were planned (their full names):
    /// a record's fields there are unknown, `fN` (get_name_for_var_field).
    unplanned_cols: std::collections::HashSet<String>,
    /// The FROM subquery (alias, column names) a filter being built is over.
    sub_alias: Option<Arc<(String, Vec<String>)>>,
    /// JSON_ARRAYAGG / JSON_OBJECTAGG results the projection above casts (RETURNING a
    /// type other than json): (grouping row column, the type).
    json_casts: Vec<(usize, DataType)>,
    /// The HAVING over the aggregation being converted (a partitionwise one checks it in
    /// each partition).
    having: Option<Expr>,
    /// The inheritance Appends grouped partitionwise (their parent's name goes unused).
    pw_aggs: Vec<usize>,
    /// Converting a partial partitionwise aggregation's partitions.
    pw_partial: bool,
    /// The method and orientation (outer side the right input) of the join being built
    /// for another partial path of it (`join`'s add_partial_path candidates).
    join_force: Option<(super::plan::JoinMethod, bool)>,
    /// The order the node above wants of a plan's rows (an ORDER BY's or a DISTINCT's
    /// keys over that plan's row, by its address): a join below it may gather a side's
    /// partial plan sorted in it (generate_useful_gather_paths' query_pathkeys).
    order_hint: Option<(usize, Vec<SortKey>)>,
    /// The aggregation node being converted's key.
    agg_key: Option<usize>,
    /// The input of the aggregation being converted.
    agg_input: Option<usize>,
    /// The DISTINCTs that are a join's input: a semi join's inner side made unique
    /// (create_unique_paths), over a projection to the compared expressions.
    unique_sides: std::collections::HashSet<usize>,
    /// A hashed DISTINCT's input projecting a subquery's plain columns: it labels the
    /// subquery's physical target list, no Subquery Scan of its own.
    plain_distinct: Option<usize>,
    /// The top nodes of the query levels converted (a sub-select's, a CTE's): their
    /// target lists keep the resjunk columns the level carries.
    level_tops: std::collections::HashSet<usize>,
    /// The groupings computing a query level's target list (its top, or below the top's
    /// sorts and limits, `grouping_top`): their target lists keep the GROUP BY keys the
    /// select list doesn't name, resjunk, as the level's does (a sub-select's too).
    group_tops: std::collections::HashSet<usize>,
    /// The FROM subqueries whose Subquery Scan shows (`shown_subqueries`), by address,
    /// with whether the node above wants just the columns it reads from it.
    shown: HashMap<usize, bool>,
    /// Projections of the query level being converted above the node being converted
    /// (its topmost one computes the select list: JSON_ARRAYAGG's constructor too).
    proj_depth: usize,
    /// Aggregations whose JSON_ARRAYAGG / JSON_OBJECTAGG calls a node further up
    /// computes the constructor of (by the Agg's address): the calls, their input's
    /// column names, and the group key count.
    json_aggs: HashMap<usize, (Vec<AggCall>, Vec<Name>, usize)>,
    /// The levels' final target lists over their groupings (`group_tlists`).
    gtl: std::rc::Rc<GroupTlists>,
    /// The resjunk group keys a final target list's node returned (by its address), for the
    /// nodes above it returning that list.
    carried: HashMap<usize, Vec<String>>,
    /// SubPlan numbers to give, by the order sub-selects are met (`renumbering`); none on
    /// the first build.
    renumber: Option<std::rc::Rc<Vec<usize>>>,
    /// The numbers taken so far, in the order they are met.
    events: Vec<PlanIdEvent>,
    /// The query levels being converted (an id each), innermost last.
    lvl_stack: Vec<usize>,
    lvl_next: usize,
    /// Each level met: the events taken while converting it (`LevelSpan`).
    spans: Vec<LevelSpan>,
    /// The sub-selects of the group keys of the aggregation being converted (by id).
    key_sub_ids: std::collections::HashSet<usize>,
    /// A group key that is a sub-select, by the names its aggregation's row shows it by.
    key_subs: HashMap<String, Subquery>,
    /// The copies of group keys' sub-selects GROUPING() arguments are, by the GROUPING
    /// call (its definition's address, unique per call) and argument.
    phantoms: HashMap<(usize, usize), SubplanLabel>,
}

/// A query level met while converting: its id, the level it is in, whether it is a FROM
/// subquery planned apart (a Subquery Scan's), and the events taken while converting it
/// (`start..end`).
#[derive(Clone, Copy)]
struct LevelSpan {
    level: usize,
    parent: usize,
    apart: bool,
    start: usize,
    end: usize,
}

/// A SubPlan number taken: the query level of the sub-select, whether it is a group key's
/// (an RTE_GROUP expression), and the first number taken for the sub-selects inside it.
#[derive(Clone, Copy)]
struct PlanIdEvent {
    level: usize,
    key: bool,
    start: usize,
    /// The sub-select's id and whether it is a join tree qual's (`Subquery::qual`).
    id: Option<usize>,
    qual: bool,
    /// A target list's (`Subquery::tlist`).
    tlist: bool,
    /// Taken right after the one before, for the same sub-select (a hashed alternative).
    follows: bool,
}

/// subquery_planner preprocesses a level's expressions in order: the target list (a
/// GROUPING() argument's sub-selects too: copies of their own), the join tree's quals,
/// HAVING, ..., the RTEs' expressions, the GROUP BY expressions last (Postgres 18's
/// RTE_GROUP); each sub-select is planned (its own sub-selects numbered) before it takes its
/// number (make_subplan / build_subplan). EXPLAIN meets the group keys' sub-selects first, in
/// the scan computing them: their numbers (each with the ones inside it) move after the
/// level's other own sub-selects. And it meets the quals' where the plan evaluates them (a
/// scan's filter before the join clause above it), Postgres in the join tree's order, which
/// is the order they were bound in: the quals' numbers go to them in that order. The
/// number each sub-select met in turn gets (or none when nothing moves).
/// Ids of the copies of group keys' sub-selects GROUPING() arguments are (below the
/// MERGE source copies' and the planner's own).
const GROUPING_COPY_BASE: usize = (1 << 39) + (1 << 37);

/// `merge_top`: the statement is a MERGE, whose actions' target lists subquery_planner
/// preprocesses after the quals (mergeActionList), its own level no target list first.
fn renumbering(events: &[PlanIdEvent], spans: &[LevelSpan], merge_top: bool) -> Option<Vec<usize>> {
    let n = events.len();
    let mut order: Vec<usize> = (0..n).collect();
    let mut levels: Vec<usize> = events.iter().filter(|e| e.key).map(|e| e.level).collect();
    levels.dedup();
    let mut moved = false;
    for l in levels {
        let mut block = vec![false; n];
        for (i, e) in events.iter().enumerate() {
            if e.key && e.level == l {
                for b in block.iter_mut().take(i + 1).skip(e.start) {
                    *b = true;
                }
            }
        }
        let Some(last_own) = (0..n).rev().find(|&i| events[i].level == l && !events[i].key && !block[i]) else {
            continue;
        };
        let Some(first_key) = (0..n).find(|&i| block[i]) else { continue };
        if first_key > last_own {
            continue;
        }
        let taken: Vec<usize> = order.iter().copied().filter(|&i| block[i]).collect();
        order.retain(|&i| !block[i]);
        let at = order.iter().position(|&i| i == last_own).map_or(order.len(), |p| p + 1);
        order.splice(at..at, taken);
        moved = true;
    }
    // (The quals': each with the sub-selects inside it, and its hashed alternative.)
    let mut levels: Vec<usize> = events.iter().filter(|e| e.qual && !e.key).map(|e| e.level).collect();
    levels.sort_unstable();
    levels.dedup();
    for l in levels {
        let mut unit: Vec<Option<usize>> = vec![None; n];
        let mut quals: Vec<usize> = Vec::new();
        for (i, e) in events.iter().enumerate() {
            if e.qual && !e.key && e.level == l && e.id.is_some() && unit[i].is_none() {
                quals.push(i);
                for u in unit.iter_mut().take(i + 1).skip(e.start) {
                    *u = Some(i);
                }
                let mut j = i + 1;
                while j < n && events[j].follows {
                    unit[j] = Some(i);
                    j += 1;
                }
            }
        }
        if quals.len() < 2 {
            continue;
        }
        // (Runs of one qual's numbers, in the current order; the others stay.)
        let mut segs: Vec<(Option<usize>, Vec<usize>)> = Vec::new();
        for &i in &order {
            match segs.last_mut() {
                Some((u, v)) if unit[i].is_some() && *u == unit[i] => v.push(i),
                _ => segs.push((unit[i], vec![i])),
            }
        }
        let mut in_order: Vec<usize> = segs.iter().filter_map(|s| s.0).collect();
        let mut by_id = in_order.clone();
        by_id.sort_by_key(|&q| events[q].id);
        // (A qual met in two places: left as it is.)
        let mut uniq = in_order.clone();
        uniq.sort_unstable();
        uniq.dedup();
        if uniq.len() != in_order.len() || by_id == in_order {
            continue;
        }
        let mut seg_of: HashMap<usize, Vec<usize>> = HashMap::new();
        for (u, v) in &segs {
            if let Some(q) = u {
                seg_of.insert(*q, v.clone());
            }
        }
        let mut next = by_id.into_iter();
        let mut rebuilt = Vec::with_capacity(n);
        for (u, v) in segs {
            match u {
                Some(_) => rebuilt.extend(next.next().and_then(|q| seg_of.get(&q).cloned()).unwrap_or_default()),
                None => rebuilt.extend(v),
            }
        }
        in_order.clear();
        if rebuilt.len() == order.len() {
            order = rebuilt;
            moved = true;
        }
    }
    // (The target list's, each with the sub-selects inside it and its hashed alternative:
    // before the level's other own ones, subquery_planner preprocessing the target list
    // first. EXPLAIN meets a scan's or a join's quals before the node computing the target
    // list above them.)
    let mut levels: Vec<usize> = events
        .iter()
        .filter(|e| e.tlist && !e.key && e.id.is_some() && !(merge_top && e.level == 1))
        .map(|e| e.level)
        .collect();
    levels.sort_unstable();
    levels.dedup();
    for l in levels {
        let mut unit: Vec<Option<usize>> = vec![None; n];
        for (i, e) in events.iter().enumerate() {
            if e.level == l && !e.key && e.id.is_some() && unit[i].is_none() {
                for u in unit.iter_mut().take(i + 1).skip(e.start) {
                    *u = Some(i);
                }
                let mut j = i + 1;
                while j < n && events[j].follows {
                    unit[j] = Some(i);
                    j += 1;
                }
            }
        }
        let mut segs: Vec<(Option<usize>, Vec<usize>)> = Vec::new();
        for &i in &order {
            match segs.last_mut() {
                Some((u, v)) if unit[i].is_some() && *u == unit[i] => v.push(i),
                _ => segs.push((unit[i], vec![i])),
            }
        }
        let tl = |u: &Option<usize>| u.is_some_and(|q| events[q].tlist);
        let Some(first_other) = segs.iter().position(|(u, _)| u.is_some() && !tl(u)) else {
            continue;
        };
        if !segs[first_other + 1..].iter().any(|(u, _)| tl(u)) {
            continue;
        }
        let (late, rest): (Vec<_>, Vec<_>) =
            segs.split_off(first_other).into_iter().partition(|(u, _)| tl(u));
        order = segs.into_iter().chain(late).chain(rest).flat_map(|(_, v)| v).collect();
        moved = true;
    }
    // (A level's FROM subqueries planned apart, each with everything inside it: after the
    // level's own expressions' sub-selects, query_planner planning them (set_subquery_pathlist)
    // once subquery_planner has preprocessed the level's expressions. Then the level's MIN /
    // MAX InitPlans, which create_minmaxagg_plan makes at createplan time.)
    let mut by_level: Vec<&LevelSpan> = spans.iter().filter(|sp| sp.end != usize::MAX).collect();
    by_level.sort_by_key(|sp| sp.level);
    for l in by_level {
        let blocks: Vec<&LevelSpan> = spans.iter().filter(|b| b.apart && b.parent == l.level && b.end != usize::MAX).collect();
        let in_block = |i: usize| blocks.iter().any(|b| b.start <= i && i < b.end);
        let mut minmax = vec![false; n];
        for (i, e) in events.iter().enumerate() {
            if e.level == l.level
                && !e.key
                && !in_block(i)
                && e.id.is_some_and(|id| id >= crate::sql::paths::PLANNER_SUBQUERY_BASE)
            {
                for m in minmax.iter_mut().take(i + 1).skip(e.start) {
                    *m = true;
                }
                let mut j = i + 1;
                while j < n && events[j].follows {
                    minmax[j] = true;
                    j += 1;
                }
            }
        }
        let own = |i: usize| l.start <= i && i < l.end && !in_block(i) && !minmax[i];
        let late = |i: usize| in_block(i) || minmax[i];
        let Some(last_own) = order.iter().rposition(|&i| own(i)) else { continue };
        if !order[..last_own].iter().any(|&i| late(i)) && !order[last_own..].iter().any(|&i| minmax[i]) {
            continue;
        }
        let blocks_taken: Vec<usize> = order.iter().copied().filter(|&i| in_block(i)).collect();
        let mm_taken: Vec<usize> = order.iter().copied().filter(|&i| minmax[i] && !in_block(i)).collect();
        let anchor = order[last_own];
        let mut rebuilt: Vec<usize> = order.iter().copied().filter(|&i| !late(i)).collect();
        let at = rebuilt.iter().position(|&i| i == anchor).map_or(rebuilt.len(), |p| p + 1);
        let tail: Vec<usize> = blocks_taken.into_iter().chain(mm_taken).collect();
        rebuilt.splice(at..at, tail);
        if rebuilt != order {
            order = rebuilt;
            moved = true;
        }
    }
    if !moved {
        return None;
    }
    let mut num = vec![0; n];
    for (pos, &i) in order.iter().enumerate() {
        num[i] = pos + 1;
    }
    Some(num)
}

/// The resjunk GROUP BY keys a level's grouping returns go up through the sorts and limits
/// over it to the level's top (a Var referencing a Var shows as it does). The keys passed up.
fn carry_group_junk(n: &mut ExNode) -> Vec<String> {
    match n.node_type {
        "Sort" | "Incremental Sort" | "Limit" => {
            let Some(c) = n.children.first_mut() else { return Vec::new() };
            let extra = carry_group_junk(c);
            for e in &extra {
                if !n.output.contains(e) {
                    n.output.push(e.clone());
                }
            }
            extra
        }
        _ => n.junk_keys.clone(),
    }
}

/// The node computing a query level's target list under its top: through the sorts and
/// limits above it and a projection only dropping the resjunk columns they read
/// (make_sort_input_target's Sort keeps the resjunk GROUP BY keys too).
fn grouping_top(mut p: &Plan) -> usize {
    loop {
        match p {
            Plan::Sort { input, .. } | Plan::Limit { input, .. } => p = input,
            Plan::Project { input, exprs }
                if exprs.len() < super::binder::plan_width(input)
                    && exprs.iter().enumerate().all(|(i, e)| matches!(e, Expr::Col(j, _) if *j == i)) =>
            {
                p = input
            }
            _ => return addr(p),
        }
    }
}

fn addr(p: &Plan) -> usize {
    p as *const Plan as usize
}

fn scan_addr(s: &ScanPlan) -> usize {
    s as *const ScanPlan as usize
}

/// Every expression a plan node evaluates itself (not its inputs').
pub(crate) fn node_exprs<'p>(p: &'p Plan, out: &mut Vec<&'p Expr>) {
    match p {
        Plan::Values { rows, .. } => out.extend(rows.iter().flatten()),
        Plan::Scan(s) => {
            out.extend(s.filter.as_ref());
            if let Some(ts) = &s.sample {
                out.push(&ts.percent);
                out.extend(ts.repeatable.as_ref());
            }
        }
        Plan::SysScan { filter, .. } => out.extend(filter.as_ref()),
        Plan::FunctionScan { calls, .. } => out.extend(calls.iter().flat_map(|c| &c.args)),
        Plan::PlCall { args, .. } => out.extend(args),
        Plan::Filter { pred, .. } => out.push(pred),
        Plan::Project { exprs, .. } => out.extend(exprs),
        Plan::ProjectSet { exprs, srfs, .. } => {
            out.extend(exprs);
            out.extend(srfs.iter().flat_map(|c| &c.args));
        }
        Plan::Agg { groups, aggs, .. } => {
            out.extend(groups);
            for a in aggs {
                out.extend(&a.args);
                out.extend(a.filter.as_ref());
            }
        }
        Plan::Sort { keys, .. } => out.extend(keys.iter().map(|k| &k.expr)),
        Plan::Window { calls, .. } => {
            for c in calls {
                out.extend(&c.args);
                if let WinFunc::Agg(a) = &c.func {
                    out.extend(a.filter.as_ref());
                }
                out.extend(&c.partition_by);
                out.extend(c.order_by.iter().map(|k| &k.expr));
            }
        }
        Plan::Limit { limit, offset, .. } => out.extend(limit.iter().chain(offset)),
        Plan::Join {
            cond, hash, params, ..
        } => {
            out.extend(cond.as_ref());
            if let Some((l, r)) = hash {
                out.extend(l.iter().chain(r));
            }
            out.extend(params);
        }
        Plan::Insert(p) => {
            if let Some(super::plan::OnConflict {
                action: ConflictAction::Update { sets, filter, .. },
                ..
            }) = &p.on_conflict
            {
                out.extend(sets.iter().map(|s| &s.1));
                out.extend(filter.as_ref());
            }
            out.extend(&p.returning);
        }
        Plan::Update(p) => {
            out.extend(p.sets.iter().map(|s| &s.1));
            out.extend(&p.returning);
        }
        Plan::Delete(p) => out.extend(&p.returning),
        Plan::Merge(p) => {
            for a in &p.actions {
                out.extend(a.cond.as_ref());
                match &a.cmd {
                    MergeCmd::Update { sets, .. } => out.extend(sets.iter().map(|s| &s.1)),
                    MergeCmd::Insert { values, .. } => out.extend(values),
                    _ => {}
                }
            }
            out.extend(&p.returning);
        }
        _ => {}
    }
}

/// `plan_inputs`, mutably (a WITH's CTEs too).
pub(crate) fn plan_inputs_mut(p: &mut Plan) -> Vec<&mut Arc<Plan>> {
    match p {
        Plan::Filter { input, .. }
        | Plan::Project { input, .. }
        | Plan::ProjectSet { input, .. }
        | Plan::Agg { input, .. }
        | Plan::Window { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input, .. }
        | Plan::LockRows { input, .. }
        | Plan::Barrier { input, .. }
        | Plan::Context { input, .. } => vec![input],
        Plan::SetOp { left, right, .. } | Plan::Join { left, right, .. } => vec![left, right],
        Plan::Append { inputs, .. } => inputs.iter_mut().collect(),
        Plan::Recursive {
            base, recursive, ..
        } => vec![base, recursive],
        Plan::With { ctes, body, .. } => {
            let mut v: Vec<&mut Arc<Plan>> = ctes.iter_mut().map(|(_, c)| c).collect();
            v.push(body);
            v
        }
        Plan::Insert(p) => vec![&mut p.source],
        Plan::Update(p) => vec![&mut p.source],
        Plan::Delete(p) => vec![&mut p.source],
        Plan::Merge(p) => vec![&mut p.source],
        _ => Vec::new(),
    }
}

pub(crate) fn plan_inputs(p: &Plan) -> Vec<&Arc<Plan>> {
    match p {
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
        Plan::Append { inputs, .. } => inputs.iter().collect(),
        Plan::Recursive {
            base, recursive, ..
        } => vec![base, recursive],
        Plan::With { body, .. } => vec![body],
        Plan::Insert(p) => vec![&p.source],
        Plan::Update(p) => vec![&p.source],
        Plan::Delete(p) => vec![&p.source],
        Plan::Merge(p) => vec![&p.source],
        _ => Vec::new(),
    }
}

/// The scans of tables other than the target that UPDATE / DELETE statements read (each
/// has a row mark: its ctid goes up the plan).
fn rowmark_scans(root: &Arc<Plan>) -> std::collections::HashSet<usize> {
    fn marks(p: &Plan, out: &mut std::collections::HashSet<usize>) {
        if let Plan::Scan(s) = p {
            if !s.with_key {
                out.insert(addr(p));
            }
            return;
        }
        // (A subquery planned apart has a whole-row mark of its own (ROW_MARK_COPY,
        // `dml_copy_marks`); its relations have none. Unless the target is in it: a view
        // written through.)
        if let Plan::Barrier { alias, .. } = p
            && !alias.is_empty()
            && target_scans(p).is_empty()
        {
            return;
        }
        for c in plan_inputs(p) {
            marks(c, out);
        }
    }
    fn go(p: &Plan, out: &mut std::collections::HashSet<usize>) {
        match p {
            Plan::Update(u) if !matches!(&*u.source, Plan::Scan(_)) => marks(&u.source, out),
            Plan::Delete(d) if !matches!(&*d.source, Plan::Scan(_)) => marks(&d.source, out),
            Plan::Merge(m) => marks(&m.source, out),
            // (A rule's action read as a CTE: a DELETE done as an UPDATE.)
            Plan::With { ctes, .. } => ctes.iter().for_each(|(_, c)| go(c, out)),
            // (A locking level's relations on a semi join's inner side: their ctids.)
            Plan::LockRows { input, .. } => {
                let (mut semi, mut copies) = (Vec::new(), Vec::new());
                lock_level_marks(input, false, &mut semi, &mut copies);
                out.extend(semi);
            }
            _ => {}
        }
        for c in plan_inputs(p) {
            go(c, out);
        }
    }
    let mut out = std::collections::HashSet::new();
    go(root, &mut out);
    out
}

/// The whole-row-marked sources of a plan (`Explainer::copy_marks`): a locking level's
/// (`lock_level_marks`), a data-modifying statement's other FROM items that are no table
/// (`dml_copy_marks`).
fn copy_mark_leaves(root: &Arc<Plan>) -> HashMap<usize, Option<u32>> {
    fn go(p: &Plan, out: &mut HashMap<usize, Option<u32>>) {
        let (mut semi, mut copies) = (Vec::new(), Vec::new());
        match p {
            Plan::LockRows { input, .. } => lock_level_marks(input, false, &mut semi, &mut copies),
            Plan::Update(u) => dml_copies(&u.source, &mut copies),
            Plan::Delete(d) => dml_copies(&d.source, &mut copies),
            Plan::Merge(m) => dml_copies(&m.source, &mut copies),
            _ => {}
        }
        out.extend(copies.into_iter().map(|(a, _)| (a, None)));
        for c in plan_inputs(p) {
            go(c, out);
        }
    }
    let mut out = HashMap::new();
    go(root, &mut out);
    out
}

/// An UPDATE / DELETE / MERGE's other FROM items that are no table: a CTE, a function,
/// VALUES, a FROM subquery (not one holding the target: a view written through).
fn dml_copies<'p>(source: &'p Plan, out: &mut Vec<(usize, &'p Plan)>) {
    let (mut semi, mut copies) = (Vec::new(), Vec::new());
    // (Its pulled-up sublinks' sources too: they are in its join tree,
    // get_relids_in_jointree.)
    level_marks(source, false, true, &mut semi, &mut copies);
    fn holds(p: &Plan, a: usize) -> bool {
        addr(p) == a || plan_inputs(p).into_iter().any(|c| holds(c, a))
    }
    let targets: Vec<usize> = target_scans(source).into_iter().map(addr).collect();
    out.extend(copies.into_iter().filter(|(_, p)| match p {
        Plan::Barrier { alias, .. } => !alias.is_empty() && !targets.iter().any(|&t| holds(p, t)),
        _ => true,
    }));
}

/// The widths of the whole-row marks at or under `p` (`Explainer::copy_marks`).
fn copy_widths_under(p: &Plan, marks: &HashMap<usize, Option<u32>>) -> u32 {
    if let Some(w) = marks.get(&addr(p)) {
        return w.unwrap_or(0);
    }
    plan_inputs(p).into_iter().map(|c| copy_widths_under(c, marks)).sum()
}

/// The row marks of a LockRows's query level that have no key column in the plan: the
/// table scans on a semi / anti join's inner side (their ctids, `semi`), and the sources of
/// no table's rows (functions, VALUES, CTEs: their whole rows, `copies`), by address.
pub(crate) fn lock_level_marks<'p>(p: &'p Plan, inner: bool, semi: &mut Vec<usize>, copies: &mut Vec<(usize, &'p Plan)>) {
    level_marks(p, inner, false, semi, copies)
}

/// `lock_level_marks`; `copy_inner`: a semi / anti join's inner sources of no table's
/// rows get their whole-row marks too (a data-modifying statement's).
fn level_marks<'p>(p: &'p Plan, inner: bool, copy_inner: bool, semi: &mut Vec<usize>, copies: &mut Vec<(usize, &'p Plan)>) {
    match p {
        Plan::Scan(_) => {
            if inner {
                semi.push(addr(p));
            }
        }
        Plan::FunctionScan { .. } | Plan::Values { .. } | Plan::CteScan { .. } | Plan::Barrier { .. } => {
            if !inner || copy_inner {
                copies.push((addr(p), p));
            }
        }
        Plan::Filter { input, .. }
        | Plan::Project { input, .. }
        | Plan::ProjectSet { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Context { input, .. }
        | Plan::With { body: input, .. } => level_marks(input, inner, copy_inner, semi, copies),
        Plan::Join { kind, left, right, .. } => {
            level_marks(left, inner, copy_inner, semi, copies);
            let semi_join = matches!(kind, JoinKind::Semi | JoinKind::Anti);
            level_marks(right, inner || semi_join, copy_inner, semi, copies);
        }
        // (A semi join's inner side unique-ified: its HashAggregate / Unique carries the
        // relations' ctids through, ungrouped.)
        Plan::Distinct { input, on: None, .. } => level_marks(input, true, copy_inner, semi, copies),
        _ => {}
    }
}

thread_local! {
    /// The RETURNING list a rule's action computes the statement's with (`rule_returning`),
    /// for the data-modifying node built next to show.
    static RULE_RETURNING: std::cell::RefCell<Option<Vec<Expr>>> = const { std::cell::RefCell::new(None) };
    /// The plan being explained is a rule's action (`rule_statements`).
    static RULE_ACTION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };

}

/// The statements a rule rewrote one into (QueryRewrite's list, each planned and explained
/// apart, ExplainSeparatePlans between them): an INSTEAD action the engine runs as a CTE under
/// a statement that returns nothing (`*RULE*`), and a qualified rule's statements run one
/// after another (an Append of data-modifying statements). Whether each is a rule's action.
/// None when the plan is one statement.
fn rule_statements(p: &Arc<Plan>) -> Option<Vec<(Arc<Plan>, bool)>> {
    fn instead(p: &Plan) -> Option<&Arc<Plan>> {
        if let Plan::With { ctes, body, names } = p
            && names.len() == 1
            && names[0] == super::binder::rules::RULE_RTE
            && let Plan::Filter { input, pred } = &**body
            && matches!(pred, Expr::Const(crate::types::Value::Bool(false), _))
            && matches!(&**input, Plan::CteScan { id, .. } if ctes.first().is_some_and(|c| c.0 == *id))
        {
            return Some(&ctes[0].1);
        }
        None
    }
    fn statement(p: &Plan) -> bool {
        matches!(p, Plan::Insert(_) | Plan::Update(_) | Plan::Delete(_) | Plan::Merge(_)) || instead(p).is_some()
    }
    fn several(p: &Plan) -> Option<&Vec<Arc<Plan>>> {
        match p {
            Plan::Append { inputs, inh: None, .. } if inputs.len() > 1 && inputs.iter().all(|i| statement(i)) => Some(inputs),
            _ => None,
        }
    }
    fn flat(p: &Arc<Plan>, action: bool, out: &mut Vec<(Arc<Plan>, bool)>) {
        if let Some(a) = instead(p) {
            flat(a, true, out);
        } else if let Some(all) = several(p) {
            for i in all {
                flat(i, action, out);
            }
        } else {
            out.push((p.clone(), action));
        }
    }
    if instead(p).is_none() && several(p).is_none() {
        return None;
    }
    let mut out = Vec::new();
    flat(p, false, &mut out);
    Some(out)
}

/// The RETURNING list a data-modifying node shows: `own`, or the one `rule_returning` left.
fn shown_returning(own: &[Expr]) -> Vec<Expr> {
    RULE_RETURNING.with(|r| r.borrow_mut().take()).unwrap_or_else(|| own.to_vec())
}

/// The sub-select copies a MERGE's source subquery output was replaced by
/// (`paths::sublinkcopy`, ids from SUBLINK_COPY_IDS on): their relations take range table
/// entries in the order the planner meets the copies (the join's, then each action's target
/// list, then its qual), which their ids follow; the names given them in plan order are
/// handed out again in that order.
fn copy_names_in_order(root: &Plan, out: &mut HashMap<usize, String>) {
    const FIRST: usize = (1 << 39) + (1 << 38);
    fn copies<'p>(p: &'p Plan, found: &mut Vec<(usize, &'p Plan)>) {
        let mut es = Vec::new();
        node_exprs(p, &mut es);
        let mut sq = Vec::new();
        for e in es {
            subqueries_in(e, &mut sq);
        }
        for s in sq {
            if (FIRST..1 << 40).contains(&s.id) && !found.iter().any(|f| f.0 == s.id) {
                found.push((s.id, &s.plan));
            }
            copies(&s.plan, found);
        }
        if let Plan::With { ctes, .. } = p {
            ctes.iter().for_each(|(_, c)| copies(c, found));
        }
        plan_inputs(p).into_iter().for_each(|c| copies(c, found));
    }
    fn scans<'p>(p: &'p Plan, found: &mut Vec<&'p Plan>) {
        if let Plan::Scan(_) = p {
            found.push(p);
        }
        plan_inputs(p).into_iter().for_each(|c| scans(c, found));
    }
    let mut found = Vec::new();
    copies(root, &mut found);
    if found.len() < 2 {
        return;
    }
    found.sort_by_key(|f| f.0);
    let mut by_base: Vec<(String, Vec<usize>)> = Vec::new();
    for (_, plan) in &found {
        let mut ss = Vec::new();
        scans(plan, &mut ss);
        for s in ss {
            let Plan::Scan(sp) = s else { continue };
            if !out.contains_key(&addr(s)) {
                continue;
            }
            let base = sp.alias.clone().unwrap_or_else(|| sp.table.name.clone());
            match by_base.iter_mut().find(|b| b.0 == base) {
                Some(b) => b.1.push(addr(s)),
                None => by_base.push((base, vec![addr(s)])),
            }
        }
    }
    // (A name's suffix: `r` 0, `r_1` 1, ….)
    let rank = |n: &str, base: &str| -> usize {
        n.strip_prefix(base)
            .and_then(|r| r.strip_prefix('_'))
            .and_then(|r| r.parse::<usize>().ok())
            .unwrap_or(0)
    };
    for (base, addrs) in by_base {
        let mut names: Vec<String> = addrs.iter().filter_map(|a| out.get(a).cloned()).collect();
        names.sort_by_key(|n| rank(n, &base));
        for (a, n) in addrs.into_iter().zip(names) {
            out.insert(a, n);
        }
    }
}

/// The data-modifying statement a rule's action is, computing the statement's RETURNING
/// (`rule_returning`), possibly read as a CTE (a DELETE done as an UPDATE): the plan's root.
fn rule_action_dml(root: &Plan) -> Option<&Plan> {
    // (A rule's action shown as a statement of its own, `rule_statements`.)
    if RULE_ACTION.get() && matches!(root, Plan::Update(_) | Plan::Delete(_)) {
        return Some(root);
    }
    let mut q = root;
    if let Plan::With { ctes, body, names } = q
        && names.len() == 1
        && names[0] == "*RULE*"
        && matches!(&**body, Plan::CteScan { id, .. } if ctes.first().is_some_and(|c| c.0 == *id))
    {
        q = &ctes[0].1;
    }
    let mut projected = false;
    while let Plan::Project { input, .. } = q {
        projected = true;
        q = input;
    }
    (projected && matches!(q, Plan::Update(_) | Plan::Delete(_))).then_some(q)
}

/// Projections over a data-modifying statement's RETURNING rows (a rule's action computing
/// the statement's RETURNING): the statement, and them composed into its RETURNING list.
fn rule_returning(p: &Plan) -> Option<(&Arc<Plan>, Vec<Expr>)> {
    let mut stack: Vec<&Vec<Expr>> = Vec::new();
    let mut q = p;
    let mut arc = None;
    while let Plan::Project { input, exprs } = q {
        stack.push(exprs);
        arc = Some(input);
        q = input;
    }
    let returning = match q {
        Plan::Update(u) => &u.returning,
        Plan::Delete(d) => &d.returning,
        Plan::Insert(i) => &i.returning,
        _ => return None,
    };
    if stack.is_empty() || returning.is_empty() {
        return None;
    }
    let mut list = returning.clone();
    for exprs in stack.into_iter().rev() {
        let below = list;
        list = exprs
            .iter()
            .map(|e| {
                e.clone().transform(&mut |x| {
                    Ok(match x {
                        Expr::Col(i, _) if i < below.len() => below[i].clone(),
                        x => x,
                    })
                })
            })
            .collect::<Result<Vec<Expr>>>()
            .ok()?;
    }
    Some((arc?, list))
}

/// The inheritance trees of a LockRows's query level whose members return their row marks:
/// (the parent's reference name, the member scans).
fn inherited_marks<'p>(p: &'p Plan, out: &mut Vec<(String, Vec<&'p Plan>)>) {
    match p {
        Plan::Append { inputs, inh: Some(parent), .. } => {
            let members: Vec<&Plan> = inputs
                .iter()
                .map(|i| match &**i {
                    Plan::Project { input, .. } => &**input,
                    x => x,
                })
                .filter(|m| matches!(m, Plan::Scan(s) if s.with_key))
                .collect();
            if !members.is_empty() {
                out.push((parent.clone(), members));
            }
        }
        Plan::Filter { input, .. }
        | Plan::Project { input, .. }
        | Plan::ProjectSet { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Context { input, .. }
        | Plan::With { body: input, .. } => inherited_marks(input, out),
        Plan::Join { kind, left, right, .. } => {
            inherited_marks(left, out);
            if !matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                inherited_marks(right, out);
            }
        }
        _ => {}
    }
}

/// `n`'s target lists with an inheritance tree's tableoid row mark: a member's scan after
/// its ctid, the nodes above at the end.
fn add_tableoid(n: &mut ExNode, parent: &str, members: &[String]) {
    for c in &mut n.children {
        add_tableoid(c, parent, members);
    }
    if n.is_scan() {
        for m in members {
            let ctid = format!("{m}.ctid");
            if let Some(i) = n.output.iter().position(|o| *o == ctid) {
                let t = format!("{m}.tableoid");
                if !n.output.contains(&t) {
                    n.output.insert(i + 1, t);
                }
            }
        }
    } else if n.output.contains(&format!("{parent}.ctid")) {
        let t = format!("{parent}.tableoid");
        if !n.output.contains(&t) {
            n.output.push(t);
        }
    }
}

thread_local! {
    /// How many LockRows the node being built is under: a scan there returning its row's
    /// key (a row mark's ctid) builds no physical target list.
    static UNDER_LOCK_ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many row-marked scans are under `p`.
fn rowmarks_under(p: &Plan, marks: &std::collections::HashSet<usize>) -> usize {
    (marks.contains(&addr(p)) as usize)
        + plan_inputs(p)
            .into_iter()
            .map(|c| rowmarks_under(c, marks))
            .sum::<usize>()
}

/// The output columns of every node its consumers use (PG's reltargets), by address.
fn needed_masks(root: &Arc<Plan>) -> HashMap<usize, Vec<bool>> {
    let mut out = needed_masks_of(root);
    // (The statement's top projection only dropping the resjunk sort columns of a scan
    // read in their order: the scan's target list keeps them.)
    let scan_proj = |mut q: &Plan| {
        let mut locked = false;
        while let Plan::Limit { input, .. } | Plan::LockRows { input, .. } = q {
            locked |= matches!(q, Plan::LockRows { .. });
            q = input;
        }
        // (Or a grouping read in the ORDER BY's order, whose target list computes them,
        // through the sort it needs no more.)
        let below = |p: &Plan| -> bool {
            let mut p = p;
            while let Plan::Sort { input, .. } | Plan::Project { input, .. } = p {
                p = input;
            }
            matches!(p, Plan::Agg { sets: None, .. })
        };
        // (Or the select list with its resjunk sort columns computed over a join and
        // sorted: the final target list, resjunk entries included, is the Sort's and the
        // top join's below it, apply_scanjoin_target_to_paths.)
        let over_join = |p: &Plan| -> bool {
            let Plan::Sort { input: p, .. } = p else { return false };
            let Plan::Project { input, .. } = &**p else { return false };
            let mut j: &Plan = input;
            while let Plan::Project { input, .. } = j {
                j = input;
            }
            matches!(j, Plan::Join { .. })
        };
        matches!(q, Plan::Project { input: pi, .. } if matches!(&**pi, Plan::Scan(_)) || below(pi))
            || !locked && over_join(q)
    };
    if let Plan::Project { input, exprs } = &**root
        && scan_proj(input)
        && exprs.len() < super::binder::plan_width(input)
        && exprs.iter().enumerate().all(|(i, e)| matches!(e, Expr::Col(j, _) if *j == i))
    {
        let w = super::binder::plan_width(input);
        let inner = needed_masks_of(input);
        out.extend(inner);
        out.insert(addr(input), vec![true; w]);
    }
    out
}

/// The columns the GROUPING() calls over a grouping read (`GroupTlists::grouping_keys`):
/// a key computed below is copied into the call, so the grouping's input returns the
/// columns the copy reads (make_group_input_target's pull_var_clause recursing into
/// GROUPING()'s arguments).
fn grouping_needs(root: &Plan, gtl: &GroupTlists, out: &mut HashMap<usize, Vec<bool>>) {
    fn go(p: &Plan, gtl: &GroupTlists, out: &mut HashMap<usize, Vec<bool>>) {
        if let Plan::Agg { input, groups, .. } = p
            && let Some(gk) = gtl.grouping_keys.get(&addr(p))
        {
            let mut chain: Vec<&Plan> = vec![&**input];
            let mut q: &Plan = input;
            while let Plan::Sort { input, .. } = q {
                q = input;
                chain.push(q);
            }
            if let Plan::Project { exprs, .. } = q {
                for &k in gk {
                    let Some(Expr::Col(j, _)) = groups.get(k) else { continue };
                    let Some(e) = exprs.get(*j) else { continue };
                    if matches!(e, Expr::Col(..)) {
                        continue;
                    }
                    for c in e.columns() {
                        if let Some(m) = exprs.iter().position(|x| matches!(x, Expr::Col(i, _) if *i == c)) {
                            for n in &chain {
                                let w = super::binder::plan_width(n);
                                let mask = out.entry(addr(n)).or_insert_with(|| vec![true; w]);
                                if let Some(x) = mask.get_mut(m) {
                                    *x = true;
                                }
                            }
                        }
                    }
                }
            }
        }
        let mut es = Vec::new();
        node_exprs(p, &mut es);
        let mut sq = Vec::new();
        for e in es {
            subqueries_in(e, &mut sq);
        }
        for s in sq {
            go(&s.plan, gtl, out);
        }
        if let Plan::With { ctes, .. } = p {
            ctes.iter().for_each(|(_, c)| go(c, gtl, out));
        }
        plan_inputs(p).into_iter().for_each(|c| go(c, gtl, out));
    }
    go(root, gtl, out);
}

fn needed_masks_of(root: &Plan) -> HashMap<usize, Vec<bool>> {
    // The FROM subqueries a locking level marks by their whole row (ROW_MARK_COPY, `s.*`):
    // the Var reads every output, so none is removed (remove_unused_subquery_outputs keeps
    // them all when attrs_used has the whole row).
    fn whole_marked(p: &Plan, out: &mut std::collections::HashSet<usize>) {
        let (mut semi, mut copies) = (Vec::new(), Vec::new());
        match p {
            Plan::LockRows { input, .. } => lock_level_marks(input, false, &mut semi, &mut copies),
            // (And a data-modifying statement's FROM subqueries: preprocess_targetlist adds
            // their whole rows to its target list.)
            Plan::Update(u) => dml_copies(&u.source, &mut copies),
            Plan::Delete(d) => dml_copies(&d.source, &mut copies),
            Plan::Merge(m) => dml_copies(&m.source, &mut copies),
            _ => {}
        }
        out.extend(copies.into_iter().filter(|(_, c)| matches!(c, Plan::Barrier { .. })).map(|(a, _)| a));
        plan_inputs(p).into_iter().for_each(|c| whole_marked(c, out));
    }
    let mut whole = std::collections::HashSet::new();
    whole_marked(root, &mut whole);
    fn go(p: &Plan, needed: Vec<bool>, whole: &std::collections::HashSet<usize>, out: &mut HashMap<usize, Vec<bool>>) {
        let needs = match p {
            // (A MERGE's source as its target list shows it: what the actions read, the
            // target row's ctid, and its tableoid for an inheritance tree.)
            Plan::Merge(m) => vec![merge_needs(m)],
            // (A FROM subquery planned apart computes its sort, group and DISTINCT keys
            // whether read or not: remove_unused_subquery_outputs keeps them; its plan
            // returns those its select list doesn't have as resjunk columns.)
            Plan::Barrier { input, secure: false, sorted, .. } => {
                let keys = level_keys(input, *sorted);
                // (Nor does it remove a volatile one.)
                let mut top: &Plan = input;
                while let Plan::Sort { input, .. }
                | Plan::Limit { input, .. }
                | Plan::Filter { input, .. }
                | Plan::With { body: input, .. } = top
                {
                    top = input;
                }
                // (An output nothing reads is a NULL in its target list, which it keeps:
                // remove_unused_subquery_outputs.)
                let volatile: Vec<bool> = match top {
                    Plan::Project { exprs, .. } => exprs
                        .iter()
                        .map(|e| expr_volatile_deep(e) || matches!(e, Expr::Const(crate::types::Value::Null, _)))
                        .collect(),
                    _ => Vec::new(),
                };
                let mask: Vec<bool> = needed
                    .iter()
                    .enumerate()
                    .map(|(i, n)| {
                        *n || keys.get(i).copied().unwrap_or(false)
                            || volatile.get(i).copied().unwrap_or(false)
                            || whole.contains(&addr(p))
                    })
                    .collect();
                if let Some((pin, _)) = barrier_junk(input, *sorted) {
                    let mut inner = super::paths::child_needs(input, &mask).pop().unwrap_or_default();
                    let pkeys = level_keys(pin, *sorted);
                    for (n, k) in inner.iter_mut().zip(pkeys) {
                        *n |= k;
                    }
                    out.insert(addr(p), needed.clone());
                    out.insert(addr(input), mask);
                    go(pin, inner, whole, out);
                    return;
                }
                vec![mask]
            }
            // (A nested loop's outer side that is a plain table scan computing the
            // PlaceHolderVars the inner side takes as parameters: its physical target list
            // (use_physical_tlist), which create_nestloop_plan's change_plan_targetlist
            // extends by them. Not an index-only scan's, nor one returning its row mark.)
            Plan::Join { left, params, .. } if !params.is_empty() => {
                let mut needs = super::paths::child_needs(p, &needed);
                if let Plan::Project { input, exprs } = &**left
                    && let Plan::Scan(sc) = &**input
                    && sc.mark.is_none()
                    && !matches!(sc.access, super::plan::Access::Index { only: true, .. })
                    && let Some(n) = needs.first_mut()
                {
                    let w = super::binder::plan_width(input);
                    if exprs.len() > w
                        && exprs[..w].iter().enumerate().all(|(i, e)| matches!(e, Expr::Col(j, _) if *j == i))
                        && exprs[w..].iter().any(|e| !matches!(e, Expr::Col(..)))
                    {
                        for x in n.iter_mut().take(w) {
                            *x = true;
                        }
                    }
                }
                needs
            }
            p => super::paths::child_needs(p, &needed),
        };
        // A LATERAL subquery's scan returns the columns its clauses over the outer side
        // read: join clauses, whose Vars stay in the relation's target list
        // (attr_needed) though the parameterized scan checks them.
        let mut needed = needed;
        // (Not an inheritance member's column mapping nothing above reads: a correlated
        // SubPlan's member scan returns nothing.)
        let member_map_unread = matches!(p, Plan::Project { exprs, .. }
            if exprs.iter().all(|e| matches!(e, Expr::Col(..))) && needed.iter().all(|n| !n));
        // (A FROM subquery's scan returning all its columns, then a nested loop's
        // PlaceHolderVar parameters: its physical target list, which
        // change_plan_targetlist extends.)
        if let Plan::Project { input, exprs } = p
            && matches!(&**input, Plan::Barrier { secure: false, alias, .. } if !alias.is_empty())
        {
            let w = super::binder::plan_width(input);
            if exprs.len() > w && exprs[..w].iter().enumerate().all(|(i, e)| matches!(e, Expr::Col(j, _) if *j == i)) {
                for n in needed.iter_mut().take(w) {
                    *n = true;
                }
            }
        }
        if let Plan::Project { input, exprs } = p
            && !member_map_unread
            && let Plan::Scan(sc) = &**input
            && let Some(f) = &sc.filter
        {
            let mut parts = Vec::new();
            conjuncts(f, &mut parts);
            for q in parts {
                if !q.has_outer_refs() {
                    continue;
                }
                for c in q.columns() {
                    if let Some(i) = exprs.iter().position(|e| matches!(e, Expr::Col(j, _) if *j == c))
                        && let Some(n) = needed.get_mut(i)
                    {
                        *n = true;
                    }
                }
            }
        }
        // (A scan's reltarget has its join clauses' columns, read above or not.)
        if let Plan::Scan(sc) = p {
            for &c in sc.reltarget.0.iter() {
                if let Some(n) = needed.get_mut(c) {
                    *n = true;
                }
            }
        }
        out.insert(addr(p), needed);
        let mut es = Vec::new();
        node_exprs(p, &mut es);
        let mut sq = Vec::new();
        for e in es {
            subqueries_in(e, &mut sq);
        }
        for s in sq {
            let w = super::binder::plan_width(&s.plan);
            go(&s.plan, vec![true; w], whole, out);
        }
        if let Plan::With { ctes, .. } = p {
            for (_, c) in ctes {
                let w = super::binder::plan_width(c);
                go(c, vec![true; w], whole, out);
            }
        }
        for (c, n) in plan_inputs(p).into_iter().zip(needs) {
            go(c, n, whole, out);
        }
    }
    let mut out = HashMap::new();
    let w = super::binder::plan_width(root);
    go(root, vec![true; w], &whole, &mut out);
    out
}

/// The output columns of a query level's plan that are its ORDER BY, GROUP BY or DISTINCT
/// keys (sort group references), through its top nodes; an ordered index scan's first
/// `sorted` keys stand for its ORDER BY.
fn level_keys(p: &Plan, sorted: Option<usize>) -> Vec<bool> {
    let w = super::binder::plan_width(p);
    let mut out = vec![false; w];
    let mark = |out: &mut Vec<bool>, e: &Expr| {
        if let Expr::Col(j, _) = e
            && let Some(o) = out.get_mut(*j)
        {
            *o = true;
        }
    };
    match p {
        Plan::Project { input, exprs } => {
            let inner = level_keys(input, sorted);
            // (A GiST / SP-GiST scan read in its ordering operator's order: the distance
            // the projection computes is the sort key.)
            let knn = matches!(&**input, Plan::Scan(s)
                if matches!(s.access, super::plan::Access::Index { ordered: true, .. })
                    && s.index.as_ref().is_some_and(|(ix, _)| matches!(ix.method.as_str(), "gist" | "spgist")));
            for (i, e) in exprs.iter().enumerate() {
                match e {
                    Expr::Col(j, _) if inner.get(*j).copied().unwrap_or(false) => out[i] = true,
                    Expr::Call(_, args, _) if knn && sorted.is_some() && args.len() == 2 && matches!(args[0], Expr::Col(..)) => {
                        out[i] = true
                    }
                    _ => {}
                }
            }
        }
        Plan::Sort { input, keys } => {
            out = level_keys(input, sorted);
            out.resize(w, false);
            for k in keys {
                mark(&mut out, &k.expr);
            }
        }
        Plan::Limit { input, .. } | Plan::LockRows { input, .. } | Plan::Filter { input, .. } | Plan::Context { input, .. } => {
            out = level_keys(input, sorted);
            out.resize(w, false);
        }
        Plan::Distinct { input, on, .. } => {
            out = level_keys(input, sorted);
            out.resize(w, false);
            match on {
                None => out.iter_mut().for_each(|o| *o = true),
                Some(on) => on.iter().for_each(|e| mark(&mut out, e)),
            }
        }
        Plan::Agg { groups, .. } => {
            for o in out.iter_mut().take(groups.len()) {
                *o = true;
            }
        }
        // (A window's PARTITION BY / ORDER BY keys: resjunk entries when the select list
        // doesn't have them.)
        Plan::Window { input, calls } => {
            out = level_keys(input, sorted);
            out.resize(w, false);
            for c in calls {
                c.partition_by.iter().for_each(|e| mark(&mut out, e));
                c.order_by.iter().for_each(|k| mark(&mut out, &k.expr));
            }
        }
        Plan::Scan(_) => {
            if let Some(n) = sorted {
                for c in super::paths::plan_order_cols(p).into_iter().take(n) {
                    if let Some(o) = out.get_mut(c) {
                        *o = true;
                    }
                }
            }
        }
        _ => {}
    }
    out
}

/// Where each query level's final target list is computed over its grouping: the GROUP BY
/// keys the select list doesn't name are resjunk entries of it (the parser adds them,
/// findTargetlistEntrySQL99), after the select list and the ORDER BY's resjunk entries, so
/// every node returning that target list (the aggregation, a ProjectSet or WindowAgg
/// computing it, the Sort, Limit, Unique above) returns them too; and the grouping's input
/// computes its keys in the target list's order (make_group_input_target).
#[derive(Default)]
struct GroupTlists {
    /// The node computing a level's final target list (a projection over the grouping, or a
    /// ProjectSet) → the group keys (positions in the grouping's row) it carries as resjunk.
    finals: HashMap<usize, Vec<usize>>,
    /// The nodes above such a node returning its target list → that node.
    chain: HashMap<usize, usize>,
    /// An aggregation → its keys in the order of its level's target list.
    order: HashMap<usize, Vec<usize>>,
    /// An aggregation → the keys its level's GROUPING() calls (target list, HAVING, the
    /// sub-selects' parameters there) test, in the order they come: its input returns what
    /// they read (make_group_input_target's pull_var_clause recurses into the arguments).
    grouping_keys: HashMap<usize, Vec<usize>>,
}

/// The group keys (positions) the GROUPING() calls in `e` test.
fn grouping_call_keys(e: &Expr, out: &mut Vec<usize>) {
    e.walk(&mut |x| {
        if let Expr::Call(d, args, _) = x
            && d.oid == 0
        {
            let keys: Vec<usize> = match d.name {
                "grouping" => args
                    .iter()
                    .skip(2)
                    .filter_map(|a| match a {
                        Expr::Const(v, _) => v.as_i64().map(|k| k as usize),
                        _ => None,
                    })
                    .collect(),
                "GROUPING" => args.iter().filter_map(|a| match a { Expr::Col(k, _) => Some(*k), _ => None }).collect(),
                _ => return,
            };
            for k in keys {
                if !out.contains(&k) {
                    out.push(k);
                }
            }
        }
    });
}

fn group_tlists(root: &Plan) -> GroupTlists {
    fn agg_below(p: &Plan) -> Option<(&Plan, usize, Option<&Expr>)> {
        match p {
            Plan::Agg { groups, .. } => Some((p, groups.len(), None)),
            // (A HAVING.)
            Plan::Filter { input, pred, .. } => match &**input {
                Plan::Agg { groups, .. } => Some((&**input, groups.len(), Some(pred))),
                _ => None,
            },
            _ => None,
        }
    }
    fn found(fin: usize, exprs: &[Expr], below: (&Plan, usize, Option<&Expr>), above: &[usize], out: &mut GroupTlists) {
        let (agg, ng, having) = below;
        let mut gk = Vec::new();
        exprs.iter().chain(having).for_each(|e| grouping_call_keys(e, &mut gk));
        out.grouping_keys.insert(addr(agg), gk);
        let mut order: Vec<usize> = Vec::new();
        for e in exprs {
            if let Expr::Col(j, _) = e
                && *j < ng
                && !order.contains(j)
            {
                order.push(*j);
            }
        }
        let junk: Vec<usize> = (0..ng).filter(|j| !order.contains(j)).collect();
        order.extend(&junk);
        out.order.insert(addr(agg), order);
        if !junk.is_empty() {
            out.finals.insert(fin, junk);
            for a in above {
                out.chain.insert(*a, fin);
            }
        }
    }
    // (From a level's top through the nodes returning its target list.)
    fn top(p: &Plan, above: &mut Vec<usize>, out: &mut GroupTlists) {
        match p {
            Plan::Context { input, .. } => top(input, above, out),
            Plan::Sort { input, .. } | Plan::Limit { input, .. } | Plan::LockRows { input, .. } | Plan::Distinct { input, .. } => {
                above.push(addr(p));
                top(input, above, out);
                above.pop();
            }
            // (The top projection dropping the resjunk columns: no node.)
            Plan::Project { input, exprs }
                if exprs.len() < super::binder::plan_width(input)
                    && exprs.iter().enumerate().all(|(i, e)| matches!(e, Expr::Col(j, _) if *j == i))
                    && matches!(&**input, Plan::Sort { .. } | Plan::Limit { .. } | Plan::LockRows { .. } | Plan::Distinct { .. }) =>
            {
                above.push(addr(p));
                top(input, above, out);
                above.pop();
            }
            Plan::Project { input, exprs } => {
                let agg = match &**input {
                    Plan::Window { input: wi, .. } => agg_below(wi),
                    _ => agg_below(input),
                };
                if let Some(b) = agg {
                    found(addr(p), exprs, b, above, out);
                }
            }
            Plan::ProjectSet { input, exprs, .. } => {
                if let Some(b) = agg_below(input) {
                    found(addr(p), exprs, b, above, out);
                }
            }
            _ => {}
        }
    }
    fn walk(p: &Plan, out: &mut GroupTlists) {
        let mut es = Vec::new();
        node_exprs(p, &mut es);
        let mut sq = Vec::new();
        for e in es {
            subqueries_in(e, &mut sq);
        }
        for s in sq {
            top(&s.plan, &mut Vec::new(), out);
            walk(&s.plan, out);
        }
        match p {
            Plan::With { ctes, .. } => ctes.iter().for_each(|(_, c)| {
                top(c, &mut Vec::new(), out);
                walk(c, out);
            }),
            Plan::Barrier { input, .. } => top(input, &mut Vec::new(), out),
            _ => {}
        }
        plan_inputs(p).into_iter().for_each(|c| walk(c, out));
    }
    let mut out = GroupTlists::default();
    top(root, &mut Vec::new(), &mut out);
    walk(root, &mut out);
    out
}

/// A FROM subquery's plan whose own top projection drops columns its upper nodes carry
/// (resjunk sort or group keys): that projection's input and the columns it keeps.
/// examine_simple_variable for a FROM subquery's output columns (`cols`, as its Subquery
/// Scan returns them; `input` the subquery's plan): one with set operations or grouping
/// sets says nothing of them, one with DISTINCT or GROUP BY only that its sole such key is
/// unique (as many values as the subquery's rows, get_variable_numdistinct's isunique);
/// any other passes its columns' statistics on.
fn subquery_var_stats(input: &Plan, cols: &mut [ColInfo], rows: f64) {
    // (Each output column's place in the current node's row, while it is a column.)
    let mut map: Vec<Option<usize>> = (0..cols.len()).map(Some).collect();
    let mut p = input;
    let unique: Option<usize> = loop {
        match p {
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
            Plan::Filter { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::LockRows { input, .. }
            | Plan::Window { input, .. } => p = input,
            Plan::SetOp { .. } | Plan::Agg { sets: Some(_), .. } => break None,
            Plan::Distinct { input, on, .. } => {
                break match on {
                    None => (super::binder::plan_width(input) == 1).then_some(0),
                    Some(es) if es.len() == 1 => match &es[0] {
                        Expr::Col(i, _) => Some(*i),
                        _ => None,
                    },
                    Some(_) => None,
                };
            }
            Plan::Agg { groups, sets: None, .. } if !groups.is_empty() => {
                break (groups.len() == 1).then_some(0);
            }
            _ => return,
        }
    };
    for (c, m) in cols.iter_mut().zip(&map) {
        c.var = None;
        c.distinct = match (unique, m) {
            (Some(u), Some(i)) if u == *i => Some(costs::clamp_rows(rows)),
            _ => None,
        };
    }
}

fn barrier_junk(input: &Plan, sorted: Option<usize>) -> Option<(&Arc<Plan>, &Vec<Expr>)> {
    match input {
        Plan::Project { input: pin, exprs }
            if exprs.len() < super::binder::plan_width(pin)
                && exprs.iter().all(|e| matches!(e, Expr::Col(..)))
                // (A sort, group or DISTINCT key its select list doesn't have.)
                && level_keys(pin, sorted)
                    .iter()
                    .enumerate()
                    .any(|(j, k)| *k && !exprs.iter().any(|e| matches!(e, Expr::Col(i, _) if *i == j))) =>
        {
            Some((pin, exprs))
        }
        _ => None,
    }
}

/// The FROM subqueries planned apart (non-secure `Barrier`s) whose Subquery Scan EXPLAIN
/// shows (setrefs' trivial_subqueryscan failing), each with whether the node above wants
/// an exact or small target list from it (CP_EXACT_TLIST / CP_SMALL_TLIST: a query level's
/// top, a Sort, a hash join's inner side, a Materialize or Memoize, an Append member; else
/// it returns its physical target list): a filter over it, a projection of it that isn't
/// all its columns in order, resjunk columns its plan carries, or that wish and columns
/// nothing above reads.
fn shown_subqueries(root: &Plan, needed: &HashMap<usize, Vec<bool>>) -> HashMap<usize, bool> {
    fn barrier(p: &Plan) -> bool {
        matches!(p, Plan::Barrier { secure: false, alias, .. } if !alias.is_empty())
    }
    // (A projection of distinct plain columns of a subquery: the subquery.)
    fn hashed_columns_of(p: &Plan) -> Option<&Plan> {
        let Plan::Project { input, exprs } = p else { return None };
        let plain = exprs
            .iter()
            .enumerate()
            .all(|(i, e)| matches!(e, Expr::Col(..)) && !exprs[..i].iter().any(|x| x.same_as(e)));
        (barrier(input) && plain).then_some(&**input)
    }
    // A query level's top: its projection dropping the sort columns it carries (resjunk
    // entries) isn't a node; the nodes under it that return their input's target list
    // (Sort, Unique, Limit, LockRows) share it, and apply_tlist_labeling marks those
    // columns resjunk in it, down to a subquery's scan whose own plan doesn't have them so.
    fn level(p: &Plan, demand: bool, needed: &HashMap<usize, Vec<bool>>, out: &mut HashMap<usize, bool>) {
        if let Plan::Project { input, exprs } = p
            && exprs.iter().all(|e| matches!(e, Expr::Col(..)))
            && exprs.len() < super::binder::plan_width(input)
            && matches!(&**input, Plan::Sort { .. } | Plan::Limit { .. } | Plan::Distinct { .. } | Plan::LockRows { .. })
        {
            let w = super::binder::plan_width(input);
            let junk: Vec<bool> = (0..w)
                .map(|i| !exprs.iter().any(|e| matches!(e, Expr::Col(j, _) if *j == i)))
                .collect();
            let mut q = &**input;
            while let Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::Distinct { input, .. }
            | Plan::LockRows { input, .. } = q
            {
                q = input;
            }
            if barrier(q) && super::binder::plan_width(q) == w && junk.iter().any(|j| *j) {
                out.insert(addr(q), true);
            }
        }
        go(p, demand, needed, out);
    }
    fn go(p: &Plan, demand: bool, needed: &HashMap<usize, Vec<bool>>, out: &mut HashMap<usize, bool>) {
        let mut es = Vec::new();
        node_exprs(p, &mut es);
        let mut sq = Vec::new();
        for e in es {
            subqueries_in(e, &mut sq);
        }
        for s in sq {
            level(&s.plan, true, needed, out);
        }
        match p {
            Plan::Barrier { input, cols, sorted, .. } if barrier(p) => {
                let partial = needed
                    .get(&addr(p))
                    .is_some_and(|m| m.iter().take(cols.len()).any(|x| !x));
                // (A physical target list has the subquery's resjunk columns too:
                // build_physical_tlist.)
                // (A subquery locking its rows returns their row marks too, resjunk.)
                // (Its LockRows is under its LIMIT, grouping_planner's.)
                let locked = {
                    let mut q: &Plan = input;
                    while let Plan::Project { input, .. } | Plan::Filter { input, .. } | Plan::Limit { input, .. } = q {
                        q = input;
                    }
                    matches!(q, Plan::Scan(sc) if sc.mark.is_some()) || matches!(q, Plan::LockRows { .. })
                };
                // (Its plan's row marks are resjunk entries of its target list, which the
                // scan's (physical or not) lacks: trivial_subqueryscan's lengths differ.)
                if (demand && (barrier_junk(input, *sorted).is_some() || partial)) || locked {
                    out.insert(addr(p), true);
                }
                match barrier_junk(input, *sorted) {
                    Some((pin, _)) => go(pin, true, needed, out),
                    None => level(input, true, needed, out),
                }
            }
            Plan::Filter { input, .. } if barrier(input) => {
                out.insert(addr(input), demand);
                go(input, demand, needed, out);
            }
            Plan::Project { input, exprs } if barrier(input) => {
                let w = super::binder::plan_width(input);
                let identity = exprs.len() == w
                    && exprs.iter().enumerate().all(|(i, e)| matches!(e, Expr::Col(j, _) if *j == i));
                // (All its columns, then values computed over them: its physical target
                // list with a nested loop's PlaceHolderVar parameters added
                // (create_nestloop_plan's change_plan_targetlist).)
                let extended = exprs.len() > w
                    && exprs[..w].iter().enumerate().all(|(i, e)| matches!(e, Expr::Col(j, _) if *j == i));
                if !identity {
                    out.insert(addr(input), !extended);
                }
                go(input, false, needed, out);
            }
            Plan::With { ctes, body, .. } => {
                for (_, c) in ctes {
                    level(c, true, needed, out);
                }
                go(body, demand, needed, out);
            }
            // (A hashed DISTINCT of plain columns labels its input's physical target list:
            // the subquery's scan is trivial then.)
            Plan::Distinct { input, on: None, .. } if hashed_columns_of(input).is_some() => {
                go(hashed_columns_of(input).expect("checked"), false, needed, out)
            }
            Plan::Sort { input, .. } => go(input, true, needed, out),
            Plan::Limit { input, .. } | Plan::LockRows { input, .. } | Plan::Context { input, .. } => {
                go(input, demand, needed, out)
            }
            Plan::Append { inputs, .. } => {
                for i in inputs {
                    go(i, true, needed, out);
                }
            }
            Plan::Join { left, right, params, meta, .. } => {
                go(left, false, needed, out);
                // (A merge join's materialized inner side is its own node, no path:
                // materialize_inner leaves the side's target list as it is.)
                let small = params.is_empty()
                    && !meta.swapped
                    && (meta.method == Some(super::plan::JoinMethod::Hash)
                        || meta.material && meta.method != Some(super::plan::JoinMethod::Merge)
                        || meta.memoize.is_some());
                go(right, small, needed, out);
            }
            p => {
                for c in plan_inputs(p) {
                    go(c, false, needed, out);
                }
            }
        }
    }
    let mut out = HashMap::new();
    level(root, true, needed, &mut out);
    // (A FROM subquery of a level locking rows returns its whole row, its row mark
    // (ROW_MARK_COPY): its Subquery Scan isn't trivial.)
    fn locked_levels(p: &Plan, out: &mut HashMap<usize, bool>) {
        if let Plan::LockRows { input, .. } = p {
            let (mut semi, mut copies) = (Vec::new(), Vec::new());
            lock_level_marks(input, false, &mut semi, &mut copies);
            for (a, q) in copies {
                if matches!(q, Plan::Barrier { secure: false, alias, .. } if !alias.is_empty()) {
                    out.insert(a, true);
                }
            }
        }
        if let Plan::With { ctes, .. } = p {
            ctes.iter().for_each(|(_, c)| locked_levels(c, out));
        }
        plan_inputs(p).into_iter().for_each(|c| locked_levels(c, out));
    }
    locked_levels(root, &mut out);
    // (So does a subquery an UPDATE / DELETE / MERGE joins its target with, a security
    // barrier view's too: preprocess_rowmarks gives every non-target relation of a
    // data-modifying statement its mark.)
    fn dml_levels(p: &Plan, out: &mut HashMap<usize, bool>) {
        let source = match p {
            Plan::Update(u) => Some((&u.source, u.rules.table.oid)),
            Plan::Delete(d) => Some((&d.source, d.table.oid)),
            Plan::Merge(m) => Some((&m.source, m.rules.table.oid)),
            _ => None,
        };
        if let Some((source, oid)) = source {
            fn holds(p: &Plan, a: usize) -> bool {
                addr(p) == a || plan_inputs(p).into_iter().any(|c| holds(c, a))
            }
            let target = target_scan(source, oid).map(addr);
            let (mut semi, mut copies) = (Vec::new(), Vec::new());
            level_marks(source, false, true, &mut semi, &mut copies);
            for (a, q) in copies {
                if matches!(q, Plan::Barrier { alias, .. } if !alias.is_empty())
                    && target.is_none_or(|t| !holds(q, t))
                {
                    out.insert(a, true);
                }
            }
        }
        if let Plan::With { ctes, .. } = p {
            ctes.iter().for_each(|(_, c)| dml_levels(c, out));
        }
        plan_inputs(p).into_iter().for_each(|c| dml_levels(c, out));
    }
    dml_levels(root, &mut out);
    out
}

/// The columns of a MERGE's source its actions, join condition and RETURNING read, the
/// target's key, and (an inheritance tree) its tableoid.
fn merge_needs(m: &super::plan::MergePlan) -> Vec<bool> {
    let w = super::binder::plan_width(&m.source);
    let mut n = vec![false; w];
    let mut mark = |e: &Expr| {
        for c in e.columns() {
            if let Some(x) = n.get_mut(c) {
                *x = true;
            }
        }
    };
    for a in &m.actions {
        if let Some(c) = &a.cond {
            mark(c);
        }
        match &a.cmd {
            MergeCmd::Update { sets, .. } => sets.iter().for_each(|(_, e)| mark(e)),
            MergeCmd::Insert { values, .. } => values.iter().for_each(&mut mark),
            _ => {}
        }
    }
    m.returning.iter().for_each(&mut mark);
    // (The target's own columns the actions read are the target tuple's, fetched by its
    // ctid (ExecMergeMatched): not the join's.)
    for c in m.target..m.target + m.rules.table.columns.len() {
        if let Some(x) = n.get_mut(c) {
            *x = false;
        }
    }
    let key = m.target + m.rules.table.columns.len();
    if let Some(x) = n.get_mut(key) {
        *x = true;
    }
    if let Some(f) = m.source_flag
        && let Some(x) = n.get_mut(f)
    {
        *x = true;
    }
    let tableoid = crate::exec::scan::SYS_COLS.iter().position(|(c, _)| *c == "tableoid");
    if m.sys
        && let Some(t) = tableoid
        && let Some(x) = n.get_mut(key + 1 + t)
    {
        *x = true;
    }
    n
}

/// The target lists under an UPDATE / DELETE / MERGE's top node (whose list is `top`, the
/// processed target list) as Postgres builds them: each row-marked relation's ctid goes up
/// from its scan; a scan returns what the top list reads of it first, in that order, then
/// what its join clauses read (its reltarget); a join returns its outer side's columns,
/// then its inner side's (build_joinrel_tlist); a node over one input keeps its order.
/// `last`: an inheritance target's row identity columns, which its scans return last.
/// Returns the row mark columns at or under `n`.
fn junk_layout(
    n: &mut ExNode,
    top: &[String],
    last: &[String],
    junk_of: &HashMap<usize, Vec<((u8, super::plan::Rti), String)>>,
    rtis: &HashMap<usize, super::plan::Rti>,
) -> Vec<String> {
    let mut junk = Vec::new();
    for c in &mut n.children {
        junk.extend(junk_layout(c, top, last, junk_of, rtis));
    }
    junk_layout_node(n, top, last, junk_of, rtis, junk)
}

/// The range table positions of a plan's scans, by plan address (`junk_layout`).
fn scan_rtis(p: &Plan) -> HashMap<usize, super::plan::Rti> {
    fn go(p: &Plan, out: &mut HashMap<usize, super::plan::Rti>) {
        if let Plan::Scan(s) = p {
            out.insert(addr(p), s.rti);
            return;
        }
        let r = leaf_rti(p);
        if r.0 != usize::MAX {
            out.insert(addr(p), r);
            return;
        }
        for c in plan_inputs(p) {
            go(c, out);
        }
    }
    let mut out = HashMap::new();
    go(p, &mut out);
    out
}

/// The range table position of the relation each output of the scans at or under `n`
/// comes from (`rtis`: the scans', by node key).
fn output_rtis(n: &ExNode, rtis: &HashMap<usize, super::plan::Rti>, out: &mut HashMap<String, super::plan::Rti>) {
    if (n.is_scan() || n.node_type == "Subquery Scan")
        && let Some(r) = n.key.and_then(|k| rtis.get(&k).copied())
    {
        for o in &n.output {
            out.entry(o.clone()).or_insert(r);
        }
        return;
    }
    for c in &n.children {
        output_rtis(c, rtis, out);
    }
}

fn junk_layout_node(
    n: &mut ExNode,
    top: &[String],
    last: &[String],
    junk_of: &HashMap<usize, Vec<((u8, super::plan::Rti), String)>>,
    rtis: &HashMap<usize, super::plan::Rti>,
    mut junk: Vec<String>,
) -> Vec<String> {
    if (n.is_scan() || n.node_type == "Subquery Scan")
        && let Some(js) = n.key.and_then(|k| junk_of.get(&k))
    {
        junk.extend(js.iter().map(|j| j.1.clone()));
    }
    if n.output.is_empty() {
        return junk;
    }
    // (A Hash or a Materialize returns its input's target list: set_dummy_tlist_references.)
    if matches!(n.node_type, "Hash" | "Materialize")
        && let [c] = &n.children[..]
        && c.is_join()
        && !c.output.is_empty()
    {
        n.output = c.output.clone();
        return junk;
    }
    for j in &junk {
        if !n.output.contains(j) {
            n.output.push(j.clone());
        }
    }
    // (A Subquery Scan returns its reltarget; an inheritance tree's Append its parent's.)
    let rank: Box<dyn Fn(&String) -> (usize, usize)> = if n.children.is_empty()
        || n.node_type == "Subquery Scan"
        || (n.node_type == "Append" && last.iter().any(|l| n.output.contains(l)))
    {
        // (An inheritance member's row identity comes after its reltarget's columns, in
        // `last`'s order.)
        Box::new(|o: &String| {
            if let Some(i) = last.iter().position(|l| l == o) {
                (0, usize::MAX - last.len() + i)
            } else {
                (0, top.iter().position(|t| mentions(t, o)).unwrap_or(usize::MAX - last.len() - 1))
            }
        })
    } else {
        let below: Vec<String> = n.children.iter().flat_map(|c| c.output.iter().cloned()).collect();
        // (A join's: build_joinrel_tlist from the input pairs the join search made it of
        // first, its relations' columns by their place in the range table (the join
        // search pairs them in that order), not the path's outer side first.)
        let mut by_rel = HashMap::new();
        if n.is_join() && !rtis.is_empty() {
            output_rtis(n, rtis, &mut by_rel);
        }
        Box::new(move |o: &String| {
            let at = below.iter().position(|t| t == o).unwrap_or(usize::MAX);
            match by_rel.get(o) {
                Some(r) => (r.0, at),
                None if by_rel.is_empty() => (0, at),
                None => (usize::MAX, at),
            }
        })
    };
    n.output.sort_by_key(|o| rank(o));
    junk
}

/// A node (a Sort, a Hash, a Materialize) over an inheritance tree's Append of `members`
/// returns what the members' scans do, under the tree's name `parent` (the Append's
/// target list is its first member's, set_dummy_tlist_references).
fn append_passthrough(n: &mut ExNode, parent: &str, members: &[String]) {
    for c in &mut n.children {
        append_passthrough(c, parent, members);
    }
    if n.is_join() || n.output.is_empty() || n.children.len() != 1 {
        return;
    }
    let c = &n.children[0];
    if c.node_type != "Append" {
        return;
    }
    let Some(first) = c.children.first().filter(|f| f.is_scan()) else { return };
    let renamed: Vec<String> = first
        .output
        .iter()
        .map(|o| {
            members
                .iter()
                .find_map(|m| o.strip_prefix(&format!("{m}.")).map(|col| format!("{parent}.{col}")))
                .unwrap_or_else(|| o.clone())
        })
        .collect();
    n.output = renamed;
}

/// A correlated EXISTS's two plans: the plain one and its hashed ANY form, with what each
/// costs (setrefs' choice), and whether each is shown yet.
#[derive(Clone)]
struct AltSubPlan {
    plain: (SubplanLabel, ExNode),
    hashed: (SubplanLabel, ExNode),
    per_plain: f64,
    startup_h: f64,
    per_h: f64,
    shown: [bool; 2],
}

impl AltSubPlan {
    /// The hashed one (1) or the plain one (0), for `n_exec` runs.
    fn pick(&self, n_exec: f64) -> usize {
        (self.startup_h + n_exec * self.per_h <= n_exec * self.per_plain) as usize
    }

    fn get(&self, i: usize) -> &(SubplanLabel, ExNode) {
        if i == 1 { &self.hashed } else { &self.plain }
    }
}

/// A scan's reltarget join clause columns (`ScanPlan::reltarget`).
fn scan_reltarget(p: &Plan) -> Vec<usize> {
    match p {
        Plan::Scan(s) => s.reltarget.0.to_vec(),
        _ => Vec::new(),
    }
}

/// Where `o` comes into the reltarget of the scan under `n` that returns it (`rel_order`).
fn rel_rank(n: &ExNode, o: &str) -> usize {
    if n.is_scan() {
        return n.rel_order.iter().position(|x| x == o).unwrap_or(usize::MAX);
    }
    for c in &n.children {
        if matches!(c.relationship, Some("Outer" | "Inner")) && c.output.iter().any(|x| x == o || format!("({x})") == o) {
            return rel_rank(c, o);
        }
    }
    usize::MAX
}

/// The target lists below a query level's top node (whose list is the level's target
/// list) in Postgres's order: a scan whose consumer wants a small target list (a Sort, a
/// Hash, a Materialize or Memoize) returns its reltarget, what the level's target list
/// reads of it first, in that order, then what its clauses read; a join its outer side's
/// columns, then its inner side's (build_joinrel_tlist); a node passing its input's rows on
/// keeps that order. (Other scans return their physical target list, in column order.)
fn tlist_layout(top: &mut ExNode) {
    tlist_layout_at(top, false)
}

/// `tlist_layout`. `locked`: the level's rows are locked at its top, a LockRows over the
/// node computing its target list (whose list it is then, the row marks' ctids last), and
/// every scan returns its reltarget, no physical target list (a row mark's ctid is a system
/// column: use_physical_tlist refuses).
fn tlist_layout_at(top: &mut ExNode, locked: bool) {
    if top.node_type == "LockRows" {
        if let Some(c) = top.children.first_mut() {
            tlist_layout_at(c, true);
        }
        return;
    }
    fn small(t: &str) -> bool {
        matches!(t, "Sort" | "Incremental Sort" | "Hash" | "Materialize" | "Memoize")
    }
    // (A relation's plain column, `ref.col`.)
    fn rel_of(o: &str) -> Option<&str> {
        if o.contains(['(', ' ', '\'', ':']) {
            return None;
        }
        o.rsplit_once('.').map(|(r, _)| r)
    }
    fn go(n: &mut ExNode, top: &[String], small_parent: bool, locked: bool) {
        if n.fixed_tlist
            || matches!(n.node_type, "Append" | "Merge Append" | "Subquery Scan" | "Gather" | "Gather Merge")
        {
            return;
        }
        if n.node_type == "LockRows" {
            if let Some(c) = n.children.first_mut() {
                tlist_layout_at(c, true);
            }
            return;
        }
        let s = small(n.node_type);
        for c in &mut n.children {
            if matches!(c.relationship, Some("Outer" | "Inner")) {
                go(c, top, s, locked);
            }
        }
        if n.output.len() < 2 {
            return;
        }
        let rank = |o: &String| top.iter().position(|t| mentions(t, o)).unwrap_or(usize::MAX);
        if n.is_scan() {
            // (Only a scan returning plain columns: one computing values returns them
            // as the level's target list has them.)
            // (Nor does one reading a system column: no physical target list.)
            let sys = n.output.iter().any(|o| {
                rel_of(o).is_some_and(|_| matches!(o.rsplit('.').next(), Some("tableoid" | "ctid" | "xmin" | "xmax" | "cmin" | "cmax")))
            });
            // (Nor one returning a whole-row Var: use_physical_tlist refuses.)
            let own = node_alias(n).map(|a| format!("{a}.*"));
            let whole = own.is_some_and(|w| n.output.contains(&w));
            if (small_parent || locked || sys || whole)
                && n.node_type != "Index Only Scan"
                && n.output.iter().all(|o| rel_of(o).is_some())
            {
                // (The target list's columns, then the join clauses' as they came in.)
                let order = std::mem::take(&mut n.rel_order);
                n.output.sort_by_key(|o| (rank(o), order.iter().position(|x| x == o).unwrap_or(usize::MAX)));
                n.rel_order = order;
            }
            return;
        }
        if n.is_join() {
            // (Each relation's columns in its reltarget's order; the relations, and the
            // values computed or referred to, where they are.)
            let mut groups: Vec<Option<String>> = Vec::new();
            let mut keys: Vec<(usize, (usize, usize))> = Vec::new();
            for (i, o) in n.output.iter().enumerate() {
                match rel_of(o) {
                    Some(r) => {
                        let g = match groups.iter().position(|x| x.as_deref() == Some(r)) {
                            Some(g) => g,
                            None => {
                                groups.push(Some(r.to_string()));
                                groups.len() - 1
                            }
                        };
                        keys.push((g, (rank(o), rel_rank(n, o))));
                    }
                    None => {
                        groups.push(None);
                        keys.push((groups.len() - 1, (i, 0)));
                    }
                }
            }
            // (Below the node computing the level's target list a join's list is its
            // relations' Vars, then the PlaceHolderVars it computes or passes up
            // (build_joinrel_tlist, then add_placeholders_to_joinrel).)
            // The joinrel's list is its first input's (as written: a Right join's inner side)
            // then its second's, each with the PlaceHolderVars it passes up where it has them,
            // a subquery relation's values among its columns; those of the second input below
            // a join of its own come after its Vars, and those evaluated here last.
            let final_tlist = n.output.len() == top.len()
                && n.output.iter().zip(top).all(|(o, t)| o == t || *t == format!("({o})"));
            if !final_tlist {
                let right = n.title.contains("Right");
                let kids: Vec<&ExNode> = n.children.iter().filter(|c| matches!(c.relationship, Some("Outer" | "Inner"))).collect();
                let provider = |o: &String| {
                    kids.iter().position(|c| c.output.iter().any(|co| co == o || format!("({co})") == *o))
                };
                let below_join = |c: &ExNode| {
                    let mut b = c;
                    while matches!(b.node_type, "Hash" | "Materialize" | "Memoize" | "Sort" | "Incremental Sort")
                        && let Some(x) = b.children.first()
                    {
                        b = x;
                    }
                    b.is_join()
                };
                // (A PlaceHolderVar a scan below computes is its base relation's, in its
                // reltarget after its Vars (add_placeholders_to_base_rels): where the
                // relation's columns are.)
                fn scan_computes(c: &ExNode, s: &str) -> bool {
                    (c.is_scan() && c.output.iter().any(|o| o == s))
                        || c.children.iter().any(|x| matches!(x.relationship, Some("Outer" | "Inner")) && scan_computes(x, s))
                }
                for (k, o) in keys.iter_mut().zip(&n.output) {
                    if rel_of(o).is_some() {
                        continue;
                    }
                    let moved = match provider(o) {
                        None => true,
                        Some(p) => {
                            let first = if right { kids.len().saturating_sub(1) } else { 0 };
                            let at_scan = o.strip_prefix('(').and_then(|x| x.strip_suffix(')')).is_some_and(|s| scan_computes(kids[p], s));
                            p != first && below_join(kids[p]) && !at_scan
                        }
                    };
                    if moved {
                        k.0 = usize::MAX;
                    }
                }
            }
            // (The node computing the level's target list returns it in its order:
            // apply_scanjoin_target_to_paths' final scan/join target, resjunk sort columns
            // last.)
            if final_tlist {
                return;
            }
            let mut items: Vec<((usize, (usize, usize)), String)> = keys.into_iter().zip(std::mem::take(&mut n.output)).collect();
            items.sort_by_key(|x| x.0);
            n.output = items.into_iter().map(|x| x.1).collect();
            return;
        }
        if matches!(n.node_type, "Sort" | "Incremental Sort" | "Hash" | "Materialize" | "Memoize" | "Limit" | "Unique") {
            let below: Vec<String> = n
                .children
                .iter()
                .filter(|c| matches!(c.relationship, Some("Outer" | "Inner")))
                .flat_map(|c| c.output.iter().cloned())
                .collect();
            // (Only when each is its input's: a value computed here or referred to keeps
            // the order as built.)
            if n.output.iter().all(|o| below.contains(o)) {
                n.output.sort_by_key(|o| below.iter().position(|t| t == o).unwrap_or(usize::MAX));
            }
        }
    }
    let t = top.output.clone();
    let s = small(top.node_type);
    if matches!(top.node_type, "ModifyTable" | "Append" | "Merge Append" | "Gather" | "Gather Merge") {
        return;
    }
    for c in &mut top.children {
        if matches!(c.relationship, Some("Outer" | "Inner")) {
            go(c, &t, s, locked);
        }
    }
}

/// Whether the expression text `t` reads the column `o` (`ref.col`).
/// The relations whose whole-row Vars (`r.*`) `s` reads.
fn whole_refs(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let b = s.as_bytes();
    for (i, _) in s.match_indices(".*") {
        // (Not a multiplication's `.*`: the star ends the reference.)
        if b.get(i + 2).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'"') {
            continue;
        }
        let start = if i > 0 && b[i - 1] == b'"' {
            match s[..i - 1].rfind('"') {
                Some(q) => q,
                None => continue,
            }
        } else {
            let mut k = i;
            while k > 0 && (b[k - 1].is_ascii_alphanumeric() || b[k - 1] == b'_' || b[k - 1] == b'$') {
                k -= 1;
            }
            k
        };
        if start == i || (start > 0 && matches!(b[start - 1], b'.' | b'"' | b'\'')) {
            continue;
        }
        let r = s[start..i].to_string();
        if !out.contains(&r) {
            out.push(r);
        }
    }
    out
}

/// The text of a node's own expressions: its target list and its details' (quals,
/// conditions, keys).
fn node_texts(n: &ExNode) -> Vec<&str> {
    // (Not an output that is a value an input computes, `(f(r.*))`: read from the input.)
    let from_input = |o: &str| {
        o.strip_prefix('(').and_then(|x| x.strip_suffix(')')).is_some_and(|x| {
            n.children
                .iter()
                .any(|c| matches!(c.relationship, Some("Outer" | "Inner")) && c.output.iter().any(|co| co == x))
        })
    };
    let mut out: Vec<&str> = n.output.iter().map(String::as_str).filter(|o| !from_input(o)).collect();
    for d in &n.details {
        match d {
            Detail::Text(_, s) => out.push(s),
            Detail::List(_, v) => out.extend(v.iter().map(String::as_str)),
            _ => {}
        }
    }
    out
}

/// The reference name of the relation a node scans (its `Alias`), a scan or a Subquery
/// Scan's), quoted as EXPLAIN qualifies columns with it.
fn node_alias(n: &ExNode) -> Option<String> {
    if !n.node_type.ends_with("Scan") {
        return None;
    }
    n.target.iter().find(|t| t.0 == "Alias").map(|t| quote_ident(&t.1))
}

/// The relations scanned in `n`'s join tree (through its Outer / Inner children).
fn tree_rels(n: &ExNode, out: &mut Vec<String>) {
    if let Some(a) = node_alias(n) {
        out.push(a);
    }
    for c in &n.children {
        if matches!(c.relationship, Some("Outer" | "Inner")) {
            tree_rels(c, out);
        }
    }
}

/// Whole-row Vars passed up as one value: Postgres's reltargets carry `r.*` (attribute 0)
/// wherever a node above reads it (build_base_rel_tlists, a join's target list, a nested
/// loop's parameter for its inner side: find_lateral_references), and a scan returning
/// one has no physical target list (use_physical_tlist). The executor passes the columns
/// and builds the row where it is read; EXPLAIN shows `r.*` returned from r's scan up to
/// the node reading it (`i.f1, i.*` under a nested loop whose inner side reads `i.*`).
/// `added`: the relations whose scans had no `r.*` before (a row mark's), collected.
fn whole_row_outputs(n: &mut ExNode, added: &mut Vec<String>) {
    let kids: Vec<usize> = (0..n.children.len())
        .filter(|&k| matches!(n.children[k].relationship, Some("Outer" | "Inner")))
        .collect();
    if !kids.is_empty() && !n.is_scan() {
        let rels: Vec<Vec<String>> = kids
            .iter()
            .map(|&k| {
                let mut v = Vec::new();
                tree_rels(&n.children[k], &mut v);
                v
            })
            .collect();
        // What this node reads, and (a nested loop's parameters) what each side reads of
        // the other's relations.
        let mut wants: Vec<(usize, String)> = Vec::new();
        for r in node_texts(n).into_iter().flat_map(whole_refs) {
            if let Some(i) = rels.iter().position(|v| v.contains(&r)) {
                wants.push((i, r));
            }
        }
        if n.is_join() {
            for (i, &k) in kids.iter().enumerate() {
                let mut used = Vec::new();
                subtree_whole_refs(&n.children[k], &mut used);
                for r in used {
                    if rels[i].contains(&r) {
                        continue;
                    }
                    if let Some(j) = rels.iter().position(|v| v.contains(&r)) {
                        wants.push((j, r));
                    }
                }
            }
        }
        for (i, r) in wants {
            let c = &mut n.children[kids[i]];
            if can_return_whole(c, &r) && return_whole(c, &r) && !added.contains(&r) {
                added.push(r);
            }
        }
    }
    for c in &mut n.children {
        whole_row_outputs(c, added);
    }
}

/// The columns of a relation returning its whole row (`r.*`) that nothing reads but that
/// whole row (the executor's row of it): no reltarget has them, they go from the target
/// lists. `top`: the level's top node, whose target list is the level's.
/// `rels`: those `whole_row_outputs` gave a whole row. (Not under a data-modifying node,
/// whose actions read columns EXPLAIN doesn't show.)
fn drop_whole_only_columns(top: &mut ExNode, rels: Vec<String>) {
    if top.operation.is_some() || top.node_type == "ModifyTable" {
        return;
    }
    // Every reading of a column but passing it up as it is.
    fn reads(n: &ExNode, top: bool, col: &str) -> bool {
        let in_outputs = n.output.iter().any(|t| (top || t != col) && mentions(t, col));
        let in_details = n.details.iter().any(|d| match d {
            Detail::Text(_, s) => mentions(s, col),
            Detail::List(_, v) => v.iter().any(|s| mentions(s, col)),
            _ => false,
        });
        let own = in_outputs || in_details;
        own || n.children.iter().chain(&n.subplans).any(|c| reads(c, false, col))
    }
    fn drop(n: &mut ExNode, col: &str) {
        n.output.retain(|o| o != col);
        for c in &mut n.children {
            if matches!(c.relationship, Some("Outer" | "Inner")) {
                drop(c, col);
            }
        }
    }
    // (A subquery's columns are passed up by the nodes over its Subquery Scan, not only
    // returned by it.)
    fn columns(n: &ExNode, r: &str, sub: bool, out: &mut Vec<String>) {
        if sub || n.is_scan() && node_alias(n).as_deref() == Some(r) {
            let prefix = format!("{r}.");
            for o in &n.output {
                if o.starts_with(&prefix) && !o.ends_with(".*") && !out.contains(o) {
                    out.push(o.clone());
                }
            }
        }
        for c in &n.children {
            if matches!(c.relationship, Some("Outer" | "Inner")) {
                columns(c, r, sub, out);
            }
        }
    }
    // A whole-row-marked subquery (ROW_MARK_COPY: its Subquery Scan returns `s.*`) passes
    // up only what is read too: the Sort or Materialize over it lists `s.x, s.*`.
    fn marked(n: &ExNode, out: &mut Vec<String>) {
        if n.node_type == "Subquery Scan"
            && let Some(a) = node_alias(n)
            && n.output.iter().any(|o| *o == format!("{a}.*"))
            && !out.contains(&a)
        {
            out.push(a);
        }
        for c in &n.children {
            if matches!(c.relationship, Some("Outer" | "Inner")) {
                marked(c, out);
            }
        }
    }
    let mut subs = Vec::new();
    marked(top, &mut subs);
    let rels: Vec<(String, bool)> = rels
        .into_iter()
        .map(|r| (r, false))
        .chain(subs.into_iter().map(|r| (r, true)))
        .collect();
    for (r, sub) in rels {
        let mut cols = Vec::new();
        columns(top, &r, sub, &mut cols);
        for c in cols {
            if !reads(top, true, &c) {
                drop(top, &c);
            }
        }
    }
}

/// The whole-row Vars read anywhere in `n`'s join tree.
fn subtree_whole_refs(n: &ExNode, out: &mut Vec<String>) {
    for r in node_texts(n).into_iter().flat_map(whole_refs) {
        if !out.contains(&r) {
            out.push(r);
        }
    }
    for c in &n.children {
        if matches!(c.relationship, Some("Outer" | "Inner")) {
            subtree_whole_refs(c, out);
        }
    }
}

/// Whether `r.*` can come up from `n`: through joins and nodes passing their input's
/// rows on, from r's own scan.
fn can_return_whole(n: &ExNode, r: &str) -> bool {
    if node_alias(n).as_deref() == Some(r) {
        return n.is_scan() || n.node_type == "Subquery Scan";
    }
    let through = n.is_join() || matches!(n.node_type, "Hash" | "Materialize" | "Memoize" | "Sort" | "Incremental Sort");
    through
        && n.children.iter().any(|c| {
            matches!(c.relationship, Some("Outer" | "Inner")) && {
                let mut v = Vec::new();
                tree_rels(c, &mut v);
                v.iter().any(|x| x == r)
            } && can_return_whole(c, r)
        })
}

/// `n` returns `r.*` (checked by `can_return_whole`), and so does each node below it on
/// the way to r's scan.
/// Whether r's scan had no `r.*` of its own before (a row mark's).
fn return_whole(n: &mut ExNode, r: &str) -> bool {
    let var = format!("{r}.*");
    let added = !n.output.contains(&var);
    if added {
        n.output.push(var.clone());
    }
    if node_alias(n).as_deref() == Some(r) {
        return added;
    }
    for c in &mut n.children {
        if matches!(c.relationship, Some("Outer" | "Inner")) && can_return_whole(c, r) {
            return return_whole(c, r);
        }
    }
    false
}

fn mentions(t: &str, o: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '.' || c == '"';
    t.match_indices(o).any(|(i, _)| {
        !t[..i].chars().next_back().is_some_and(ident) && !t[i + o.len()..].chars().next().is_some_and(ident)
    })
}

/// The row mark columns of the scans at or under `n`, in range table order.
/// The processed target list of an UPDATE / DELETE of an inheritance tree over a join, after
/// its new values (`top`): the row marks' junk (preprocess_targetlist: each mark's ctid or
/// whole row, in mark order), what RETURNING reads of the other relations, then what
/// expand_inherited_rtentry appends as it expands each parent in range table order: a row
/// mark's tableoid, the target's row identity (its `tableoid`, then `ident`; a target that
/// is no inheritance parent has it before the row marks). Returns that last part, which each
/// scan returns last.
fn expanded_tlist(
    top: &mut Vec<String>,
    n: &ExNode,
    junk_of: &HashMap<usize, Vec<((u8, super::plan::Rti), String)>>,
    returning: Vec<String>,
    tableoid: Option<&str>,
    ident: &str,
) -> Vec<String> {
    let keyed = junk_in_keyed(n, junk_of);
    for (k, j) in &keyed {
        if k.0 == 0 && !top.contains(j) {
            top.push(j.clone());
        }
    }
    for v in returning {
        if !top.contains(&v) {
            top.push(v);
        }
    }
    let mut last = Vec::new();
    for (k, j) in &keyed {
        if k.0 != 0 && !last.contains(j) {
            last.push(j.clone());
            if Some(j.as_str()) == tableoid {
                last.push(ident.to_string());
            }
        }
    }
    if let Some(t) = tableoid
        && !last.iter().any(|l| l == t)
    {
        last.push(t.to_string());
        last.push(ident.to_string());
    }
    top.extend(last.iter().cloned());
    last
}

/// The names a node computing a data-modifying statement's target list (a join) gives its
/// own outputs: a projection merged into it computes its expressions there, as they read
/// (`internal`), not as a node above would refer to them.
fn computed_names(b: &Built) -> &[Name] {
    if b.internal.len() == b.exported.len() && b.node.is_join() {
        &b.internal
    } else {
        &b.exported
    }
}

fn junk_in(n: &ExNode, junk_of: &HashMap<usize, Vec<((u8, super::plan::Rti), String)>>) -> Vec<String> {
    junk_in_keyed(n, junk_of).into_iter().map(|j| j.1).collect()
}

/// A source of no table's rows' place in the range table (a FROM subquery's, a function's,
/// VALUES', a CTE's: their whole-row marks' order); not known: after every relation's.
fn leaf_rti(p: &Plan) -> super::plan::Rti {
    match p {
        Plan::Barrier { rti, .. } | Plan::FunctionScan { rti, .. } | Plan::Values { rti, .. } | Plan::CteScan { rti, .. }
            if rti.0 != 0 =>
        {
            *rti
        }
        _ => super::plan::Rti(usize::MAX),
    }
}

/// `junk_in`, each column with its place: (0 a row mark's, 1 an inheritance parent's
/// tableoid; the range table index).
fn junk_in_keyed(
    n: &ExNode,
    junk_of: &HashMap<usize, Vec<((u8, super::plan::Rti), String)>>,
) -> Vec<((u8, super::plan::Rti), String)> {
    fn go(
        n: &ExNode,
        junk_of: &HashMap<usize, Vec<((u8, super::plan::Rti), String)>>,
        out: &mut Vec<((u8, super::plan::Rti), String)>,
    ) {
        for c in &n.children {
            go(c, junk_of, out);
        }
        if (n.is_scan() || n.node_type == "Subquery Scan")
            && let Some(js) = n.key.and_then(|k| junk_of.get(&k))
        {
            out.extend(js.iter().cloned());
        }
    }
    let mut out = Vec::new();
    go(n, junk_of, &mut out);
    out.sort_by_key(|j| j.0);
    out
}

/// A MERGE's source target lists: a relation's tableoid before its ctid (the junk
/// columns' order).
fn tableoid_first(n: &mut ExNode) {
    // (Columns, then the junk ones: tableoid, ctid.)
    let rank = |o: &String| {
        if o.ends_with(".tableoid") {
            1
        } else if o.ends_with(".ctid") {
            2
        } else {
            0
        }
    };
    n.output.sort_by_key(rank);
    for k in &mut n.children {
        tableoid_first(k);
    }
}

/// The inheritance trees a plan reads whose parent takes no name of its own (an Append of
/// one member, dropped): each a range table entry all the same.
fn inh_parents(p: &Plan) -> usize {
    // (Its members are range table entries too, the ones pruning removes as well.)
    let own = match p {
        Plan::Append { inh: Some(_), inputs, .. } => inputs.len(),
        _ => 0,
    };
    own + plan_inputs(p).into_iter().map(|c| inh_parents(c)).sum::<usize>()
}

/// The earliest range table place of the relations a plan scans (its FROM-list order).
fn min_rti(p: &Plan) -> Option<super::plan::Rti> {
    let own = match p {
        Plan::Scan(s) if s.rti.0 > 0 => Some(s.rti),
        _ => None,
    };
    plan_inputs(p).into_iter().filter_map(|c| min_rti(c)).chain(own).min()
}

/// The ids of the sub-selects a plan's expressions run (not those inside them).
fn plan_subquery_ids(p: &Plan) -> Vec<usize> {
    let mut exprs = Vec::new();
    node_exprs(p, &mut exprs);
    let mut sq = Vec::new();
    for e in exprs {
        subqueries_in(e, &mut sq);
    }
    let mut ids: Vec<usize> = sq.iter().map(|s| s.id).collect();
    for c in plan_inputs(p) {
        ids.extend(plan_subquery_ids(c));
    }
    ids
}

fn subqueries_in<'p>(e: &'p Expr, out: &mut Vec<&'p Subquery>) {
    e.walk(&mut |x| {
        // (A function call run as a subquery shows as the call: no plan of its own.)
        if let Expr::Subquery(s) = x
            && s.call.is_none()
        {
            out.push(s);
        }
    });
}

/// `set_rtable_names`: each scan's reference name, made unique with `_N` suffixes. The main
/// query's relations are named first, then CTEs and subqueries in turn.
/// Each scan's reference name; and the FROM-less query levels, which Postgres gives an
/// RTE_RESULT range table entry each (replace_empty_jointree).
fn assign_refnames(root: &Plan) -> (HashMap<usize, String>, usize) {
    assign_refnames_in(root, None)
}

/// The same, leaving out the members of inheritance Appends that pruning when the executor
/// starts removes (`ctx`): EXPLAIN names only the relations the plan still scans.
fn assign_refnames_in(root: &Plan, ctx: Option<&ExecCtx>) -> (HashMap<usize, String>, usize) {
    assign_refnames_skip(root, ctx, &[], &[], None)
}

/// The same, the inheritance Appends at `skip` (grouped partitionwise) taking no name:
/// their members are the parent's scans then.
/// `orders`: UNION ALL arms kept subqueries, in the order the final plan has them (a
/// Parallel Append's, by cost): named in that order.
fn assign_refnames_skip(
    root: &Plan,
    ctx: Option<&ExecCtx>,
    skip: &[usize],
    orders: &[Vec<usize>],
    ranks: Option<&HashMap<usize, usize>>,
) -> (HashMap<usize, String>, usize) {
    let mut out = HashMap::new();
    let mut results = 0;
    let mut used: HashMap<String, usize> = HashMap::new();
    // A partitioned table joined partitionwise has no Append of its own, but its name is
    // still taken (its partitions are `name_1`, `name_2`, …).
    let mut parents = Vec::new();
    partitionwise_parents(root, true, &mut parents, skip);
    for p in parents {
        used.insert(p, 0);
    }
    let returning_firsts = dml_returning_firsts(root);
    // (An UPDATE / DELETE's target is the range table's first entry: named first.)
    // (Not an inheritance tree's: its Append takes the parent's name.)
    // (Nor a rule action's: the statement's relations come first, below.)
    let target = match root {
        _ if RULE_ACTION.get() => None,
        Plan::Update(u) if find_inh_append(&u.source).is_none() => target_scan(&u.source, u.rules.table.oid),
        Plan::Delete(d) if find_inh_append(&d.source).is_none() => target_scan(&d.source, d.table.oid),
        _ => None,
    };
    if let Some(Plan::Scan(s)) = target {
        let base = s.alias.clone().unwrap_or_else(|| s.table.name.clone());
        if !used.contains_key(&base) {
            used.insert(base.clone(), 0);
            out.insert(addr(target.expect("checked")), base);
        }
    }
    // (A statement a rule's action computes the RETURNING of: rewriteRuleAction puts the
    // statement's range table before the action's, so the statement's own relations, bound
    // first, are named before the action's target and FROM items.)
    if let Some(dml) = rule_action_dml(root) {
        fn scans<'p>(p: &'p Plan, out: &mut Vec<(&'p Plan, usize)>) {
            match p {
                Plan::Scan(s) => out.push((p, s.rti.0)),
                // (A one-member inheritance Append is its member.)
                Plan::Append { inputs, inh: Some(_), .. } if inputs.len() == 1 => scans(&inputs[0], out),
                Plan::Barrier { .. } | Plan::Append { .. } | Plan::With { .. } => {}
                p => plan_inputs(p).into_iter().for_each(|c| scans(c, out)),
            }
        }
        // (An inheritance tree's parent is the nominal relation, the first of the action's
        // entries, named first; its member scans come after the statement's relations, as
        // expand_inherited_rtentry appends them.)
        let (source, parent) = match dml {
            Plan::Update(u) => (
                Some(&u.source),
                (u.inh && !multi_target_append(&u.source))
                    .then(|| dml_tree_name(&u.source, &u.rules.table, u.alias.as_deref())),
            ),
            Plan::Delete(d) => (
                Some(&d.source),
                (d.inh && !multi_target_append(&d.source))
                    .then(|| dml_tree_name(&d.source, &d.table, d.alias.as_deref())),
            ),
            _ => (None, None),
        };
        if let Some(p) = parent
            && !used.contains_key(&p)
        {
            used.insert(p.clone(), 0);
            out.insert(addr(dml), p);
        }
        let mut found = Vec::new();
        if let Some(s) = source {
            scans(s, &mut found);
        }
        if found.iter().all(|(_, r)| *r > 0) {
            found.sort_by_key(|(_, r)| *r);
            for (p, _) in found {
                let Plan::Scan(s) = p else { continue };
                let base = s.alias.clone().unwrap_or_else(|| s.table.name.clone());
                let name = match used.get_mut(&base) {
                    Some(n) => loop {
                        *n += 1;
                        let cand = crate::catalog::suffixed_name(&base, &format!("_{n}"));
                        if !out.values().any(|v| *v == cand) {
                            break cand;
                        }
                    },
                    None => base,
                };
                used.entry(name.clone()).or_insert(0);
                out.insert(addr(p), name);
            }
        }
    }
    let shown = shown_subqueries(root, &needed_masks_of(root));
    let mut union_flat: Vec<usize> = Vec::new();
    union_inh_appends(root, &mut union_flat);
    let mut queue: Vec<&Plan> = vec![root];
    let mut qi = 0;
    // (A partition grouped partitionwise that is itself partitioned: its Append, though
    // not in the plan, takes a name before its leaves.)
    let mut mid_first: HashMap<usize, Vec<String>> = HashMap::new();
    let mut fake = usize::MAX;
    let mut inline_barriers: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut appendrel_nested: std::collections::HashSet<usize> = std::collections::HashSet::new();
    // (The SubPlans a build showed, by their numbers: setrefs flattens their range tables
    // after the main query's, in plan_id order, so their relations are named in that order
    // once the levels' own are, a nested one before the one it is in.)
    let mut ranked: Vec<(usize, &Plan)> = Vec::new();
    if let Some(r) = ranks {
        fn collect<'p>(p: &'p Plan, r: &HashMap<usize, usize>, out: &mut Vec<(usize, &'p Plan)>) {
            let mut es = Vec::new();
            node_exprs(p, &mut es);
            match p {
                Plan::Insert(i) => es.extend(i.rules.view_checks.iter().map(|(_, e)| e)),
                Plan::Update(u) => es.extend(u.rules.view_checks.iter().map(|(_, e)| e)),
                _ => {}
            }
            let mut sq = Vec::new();
            for e in es {
                subqueries_in(e, &mut sq);
            }
            for s in sq {
                if let Some(&k) = r.get(&s.id) {
                    match out.iter_mut().find(|(_, q)| std::ptr::eq(*q, &*s.plan)) {
                        Some(x) => x.0 = x.0.min(k),
                        None => out.push((k, &*s.plan)),
                    }
                }
                collect(&s.plan, r, out);
            }
            if let Plan::With { ctes, .. } = p {
                ctes.iter().for_each(|(_, c)| collect(c, r, out));
            }
            plan_inputs(p).into_iter().for_each(|c| collect(c, r, out));
        }
        collect(root, r, &mut ranked);
        ranked.sort_by_key(|x| std::cmp::Reverse(x.0));
    }
    loop {
    if qi >= queue.len() {
        match ranked.pop() {
            Some((_, p)) => {
                queue.push(p);
                continue;
            }
            None => break,
        }
    }
    {
        let mut stack = vec![queue[qi]];
        qi += 1;
        let mut later: Vec<&Plan> = Vec::new();
        // (Subquery levels: named before the level's SubPlans and CTEs, as setrefs reaches
        // them in the plan, the subplans after the whole plan.)
        let mut later_sub: Vec<&Plan> = Vec::new();
        while let Some(p) = stack.pop() {
            // (A UNION ALL's arm flattened into its Append: no level of it named, its
            // sub-partitioned members' neither.)
            if let Plan::Append { inh: Some(n), inputs, .. } = p
                && skip.contains(&addr(p))
                && !union_flat.contains(&addr(p))
            {
                let members: Vec<&Arc<Plan>> = inputs
                    .iter()
                    .filter(|m| !matches!(&***m, Plan::Empty { .. }))
                    .collect();
                if let Some((_, groups)) = pwagg::own_levels(&members) {
                    for g in groups.iter().filter(|g| g.len() > 1 && !skip.contains(&addr(members[g[0]]))) {
                        mid_first.entry(addr(members[g[0]])).or_default().push(n.clone());
                    }
                }
            }
            // (So does a sub-partitioned table an ordered Append reads in its own Append or
            // Merge Append: `ordered_append`.)
            if let Plan::Sort { input, keys } = p
                && merge_appended(input)
            {
                let exprs: Vec<Expr> = keys.iter().map(|k| k.expr.clone()).collect();
                let dirs: Vec<(bool, bool)> = keys.iter().map(|k| (k.desc, k.nulls_first)).collect();
                let mut a = &**input;
                while let Plan::Project { input, .. } = a {
                    a = input;
                }
                if let Plan::Append { inh: Some(n), inputs, .. } = a
                    && let Some(order) = crate::sql::paths::partition_order(input, &exprs, &dirs)
                {
                    // (A member run-time pruning removes as the executor starts isn't
                    // named: its level's name goes on its first member kept.)
                    fn mark(
                        o: &[crate::sql::paths::PartOrder],
                        members: &[&Arc<Plan>],
                        kept: &[bool],
                        n: &str,
                        out: &mut HashMap<usize, Vec<String>>,
                    ) -> usize {
                        use crate::sql::paths::PartOrder;
                        let on = |i: usize| kept.get(i).copied().unwrap_or(true);
                        let mut low = usize::MAX;
                        for m in o {
                            let first = match m {
                                PartOrder::Leaf(i) => {
                                    if on(*i) {
                                        *i
                                    } else {
                                        usize::MAX
                                    }
                                }
                                PartOrder::Merge(v) => {
                                    let f = v.iter().copied().filter(|i| on(*i)).min().unwrap_or(usize::MAX);
                                    if let Some(x) = members.get(f) {
                                        out.entry(addr(x)).or_default().push(n.to_string());
                                    }
                                    f
                                }
                                PartOrder::Append(sub) => {
                                    // (Its name before its own sub-partitions'.)
                                    let mut inner = HashMap::new();
                                    let f = mark(sub, members, kept, n, &mut inner);
                                    if let Some(x) = members.get(f) {
                                        out.entry(addr(x)).or_default().push(n.to_string());
                                    }
                                    for (k, v) in inner {
                                        out.entry(k).or_default().extend(v);
                                    }
                                    f
                                }
                            };
                            low = low.min(first);
                        }
                        low
                    }
                    let members: Vec<&Arc<Plan>> =
                        inputs.iter().filter(|m| !matches!(&***m, Plan::Empty { .. })).collect();
                    // (A single partition read: its plan replaces the Append, named as
                    // the parent's.)
                    let kept: Vec<bool> = match ctx {
                        Some(ctx) => {
                            let k = crate::exec::nodes::runtime_pruned(
                                inputs,
                                crate::sql::binder::partition::Known::Start,
                                ctx,
                            );
                            members.iter().map(|m| k.iter().any(|x| Arc::ptr_eq(x, m))).collect()
                        }
                        None => vec![true; members.len()],
                    };
                    if crate::sql::paths::count_members(&order) == members.len() {
                        match order.as_slice() {
                            [crate::sql::paths::PartOrder::Merge(_)] => {}
                            [crate::sql::paths::PartOrder::Append(sub)] => {
                                mark(sub, &members, &kept, n, &mut mid_first);
                            }
                            o => {
                                mark(o, &members, &kept, n, &mut mid_first);
                            }
                        }
                    }
                }
            }
            for base in mid_first.get(&addr(p)).cloned().unwrap_or_default() {
                let name = if used.contains_key(&base) {
                    let n = used.get_mut(&base).unwrap();
                    loop {
                        *n += 1;
                        let cand = crate::catalog::suffixed_name(&base, &format!("_{n}"));
                        if !out.values().any(|v| *v == cand) {
                            break cand;
                        }
                    }
                } else {
                    base
                };
                used.entry(name.clone()).or_insert(0);
                out.insert(fake, name);
                fake -= 1;
            }
            let base = match p {
                Plan::Scan(s) => Some(s.alias.clone().unwrap_or_else(|| s.table.name.clone())),
                Plan::SysScan { table, alias, .. } => Some(alias.clone().unwrap_or_else(|| table.name().to_string())),
                Plan::Values { rows, .. } if rows.len() > 1 => Some("*VALUES*".to_string()),
                Plan::FunctionScan { alias, .. }
                | Plan::CteScan { alias, .. }
                | Plan::WorkScan { alias, .. } => Some(alias.clone()),
                Plan::Join { meta, .. } if meta.from_call.is_some() => {
                    meta.from_call.as_ref().map(|f| f.alias.clone())
                }
                // A MERGE into an inheritance tree pruned to one member: the parent (its
                // result relation) still takes its name first.
                Plan::Merge(m) if !m.members.is_empty() && !multi_inh_append(&m.source) => {
                    let t = &m.rules.table;
                    std::iter::once(t)
                        .chain(m.members.iter().map(|x| &x.rules.table))
                        .find_map(|x| match target_scan(&m.source, x.oid) {
                            Some(Plan::Scan(s)) => Some(s.alias.clone().unwrap_or_else(|| t.name.clone())),
                            _ => None,
                        })
                }
                // So does an UPDATE / DELETE of an inheritance tree that reads one member (or
                // none).
                Plan::Update(u) if u.inh && !multi_target_append(&u.source) => {
                    Some(dml_tree_name(&u.source, &u.rules.table, u.alias.as_deref()))
                }
                Plan::Delete(d) if d.inh && !multi_target_append(&d.source) => {
                    Some(dml_tree_name(&d.source, &d.table, d.alias.as_deref()))
                }
                Plan::EnrScan { name, alias, .. } => {
                    Some(alias.clone().unwrap_or_else(|| name.clone()))
                }
                Plan::PlCall { func, .. } => Some(func.name.clone()),
                Plan::One => {
                    results += 1;
                    None
                }
                // (A relation proven empty is still in the range table, unnamed.)
                Plan::Empty { info: Some(d), .. } => {
                    let mut rels: Vec<&String> = d.rels.iter().flatten().collect();
                    rels.sort();
                    rels.dedup();
                    // (A FROM subquery never planned counts as the relations its query reads,
                    // itself being counted with the FROM subqueries.)
                    results += rels
                        .iter()
                        .map(|r| d.unplanned.iter().find(|u| u.0 == **r).map_or(1, |u| u.1))
                        .sum::<usize>();
                    None
                }
                Plan::Insert(i) => Some(i.alias.clone().unwrap_or_else(|| i.rules.table.name.clone())),
                // A UNION ALL appendrel's parent entry (the Append's apprelids): named before
                // its members.
                Plan::Append {
                    inh: None,
                    alias: Some(a),
                    inputs,
                    ..
                } if inputs.len() > 1 => Some(a.clone()),
                // The parent's own name; its member scans are `name_1`, `name_2`, …. (A single
                // member's Append is dropped, and it takes the name itself.)
                Plan::Append {
                    inh: Some(n),
                    inputs,
                    ..
                } if !skip.contains(&addr(p))
                    && (inputs.len() > 1
                    || matches!(
                        inputs.first().map(|i| &**i),
                        Some(Plan::Update(_) | Plan::Delete(_))
                    )) =>
                {
                    Some(n.clone())
                }
                _ => None,
            };
            if let Some(base) = base
                && !out.contains_key(&addr(p))
            {
                let name = if used.contains_key(&base) {
                    let n = used.get_mut(&base).unwrap();
                    loop {
                        *n += 1;
                        let cand = crate::catalog::suffixed_name(&base, &format!("_{n}"));
                        if !out.values().any(|v| *v == cand) {
                            break cand;
                        }
                    }
                } else {
                    base
                };
                used.entry(name.clone()).or_insert(0);
                out.insert(addr(p), name);
            }
            let mut es = Vec::new();
            node_exprs(p, &mut es);
            // (WITH CHECK OPTION's sub-selects are the ModifyTable's.)
            match p {
                Plan::Insert(i) => es.extend(i.rules.view_checks.iter().map(|(_, e)| e)),
                Plan::Update(u) => es.extend(u.rules.view_checks.iter().map(|(_, e)| e)),
                _ => {}
            }
            let mut sq = Vec::new();
            for e in es {
                subqueries_in(e, &mut sq);
            }
            // (A subquery used twice is one plan: named once.)
            for s in sq {
                if !later.iter().any(|x| std::ptr::eq(*x, &*s.plan))
                    && !queue.iter().any(|x| std::ptr::eq(*x, &*s.plan))
                    && !ranked.iter().any(|x| std::ptr::eq(x.1, &*s.plan))
                    && !ranks.is_some_and(|r| r.contains_key(&s.id))
                {
                    later.push(&*s.plan);
                }
            }
            if let Plan::With { ctes, .. } = p {
                later.extend(ctes.iter().map(|(_, c)| &**c));
            }
            let mut inputs = plan_inputs(p);
            if let Plan::Append { inh: None, .. } = p {
                for order in orders {
                    let at: Vec<usize> = (0..inputs.len()).filter(|&k| order.contains(&addr(inputs[k]))).collect();
                    if at.len() == order.len() {
                        let moved: Vec<&Arc<Plan>> = order
                            .iter()
                            .filter_map(|o| inputs.iter().find(|i| addr(i) == *o).copied())
                            .collect();
                        for (k, m) in at.into_iter().zip(moved) {
                            inputs[k] = m;
                        }
                    }
                }
            }
            if matches!(p, Plan::Join { meta, .. } if meta.from_call.is_some()) {
                inputs.clear();
            }
            // (An INTERSECT with the input of fewer groups first: its relations are named
            // in that order, as the plan has them.)
            if let (Some(ctx), Plan::SetOp { kind: SetOpKind::Intersect, left, right, .. }) = (ctx, p)
                && setop_shape(left, right, true, ctx).0
            {
                inputs.reverse();
            }
            // (A subquery a filter keeps apart, its Subquery Scan shown: its alias is a
            // name of this level, its own relations named after this level's.)
            // (So does any FROM subquery whose Subquery Scan shows.)
            let sub_shown: Option<&Plan> = match p {
                Plan::Filter { input, .. } if matches!(&**input, Plan::Barrier { secure: true, .. }) => Some(&**input),
                Plan::Filter { input, .. } | Plan::Project { input, .. } if shown.contains_key(&addr(input)) => {
                    Some(&**input)
                }
                Plan::Barrier { .. } if shown.contains_key(&addr(p)) => Some(p),
                _ => None,
            };
            if let Some(input) = sub_shown
                && let Plan::Barrier { input: bi, alias, .. } = input
                && !out.contains_key(&addr(input))
            {
                let name = if used.contains_key(alias) {
                    let n = used.get_mut(alias).unwrap();
                    loop {
                        *n += 1;
                        let cand = crate::catalog::suffixed_name(alias, &format!("_{n}"));
                        if !out.values().any(|v| *v == cand) {
                            break cand;
                        }
                    }
                } else {
                    alias.clone()
                };
                used.entry(name.clone()).or_insert(0);
                out.insert(addr(input), name);
                inputs.clear();
                later_sub.push(&**bi);
            } else if sub_shown.is_none()
                && let Plan::Barrier { input: bi, .. } = p
                && !inline_barriers.contains(&addr(p))
            {
                // (A subquery setrefs removes as trivial: its relations are named when
                // setrefs reaches it, after the level's own, in plan order.)
                inputs.clear();
                later_sub.push(&**bi);
            }
            // (A set operation's arms are all subqueries of a level with no relations of
            // its own: named in plan order as they are.)
            // (Not an appendrel's: its arms pulled up are the level's relations, and the
            // ones planned apart are subqueries named after them, as setrefs reaches them.
            // Nor a nested UNION ALL's in it, which is flattened into it.)
            if let Plan::Append { inh: None, alias: Some(_), inputs, .. } = p {
                fn nested(inputs: &[Arc<Plan>], out: &mut std::collections::HashSet<usize>) {
                    for i in inputs {
                        let mut x: &Plan = i;
                        while let Plan::Project { input, .. } = x {
                            x = input;
                        }
                        if let Plan::Append { inh: None, inputs, .. } = x {
                            out.insert(addr(x));
                            nested(inputs, out);
                        }
                    }
                }
                nested(inputs, &mut appendrel_nested);
            }
            if let Plan::SetOp { .. } | Plan::Append { inh: None, alias: None, .. } = p
                && !appendrel_nested.contains(&addr(p))
            {
                for c in plan_inputs(p) {
                    let mut a: &Plan = c;
                    loop {
                        match a {
                            Plan::Barrier { input, .. } => {
                                inline_barriers.insert(addr(a));
                                a = input;
                            }
                            Plan::Project { input, .. }
                            | Plan::Filter { input, .. }
                            | Plan::Context { input, .. }
                            | Plan::Sort { input, .. }
                            | Plan::Distinct { input, .. } => a = input,
                            _ => break,
                        }
                    }
                }
            }
            if let (Some(ctx), Plan::Append { inputs: all, inh, .. }) = (ctx, p)
                && !matches!(all.first().map(|i| &**i), Some(Plan::Update(_) | Plan::Delete(_)))
                && (inh.is_some() || !all.iter().any(|i| i.writes()))
            {
                let kept = crate::exec::nodes::runtime_pruned(
                    all,
                    crate::sql::binder::partition::Known::Start,
                    ctx,
                );
                // (An UPDATE / DELETE with RETURNING names its first result relation
                // however it is pruned: RETURNING's columns are that relation's.)
                inputs.retain(|i| {
                    kept.iter().any(|k| Arc::ptr_eq(k, i)) || returning_firsts.contains(&addr(i))
                });
            }
            for c in inputs.into_iter().rev() {
                stack.push(c);
            }
        }
        queue.extend(later_sub);
        queue.extend(later);
    }
    }
    copy_names_in_order(root, &mut out);
    // Partitions joined partitionwise are numbered as their parent expands them (bound
    // order), not as the child joins (in merged-bounds order) list them.
    let mut scans = Vec::new();
    partitionwise_scans(root, &mut scans);
    let mut groups: Vec<(String, Vec<(Vec<usize>, usize)>)> = Vec::new();
    for p in scans {
        let Plan::Scan(s) = p else { continue };
        let Some(levels) = &s.part else { continue };
        if !out.contains_key(&addr(p)) {
            continue;
        }
        let base = s.alias.clone().unwrap_or_else(|| s.table.name.clone());
        let mut path: Vec<usize> = levels
            .iter()
            .map(|lv| {
                let mut bounds: Vec<&crate::catalog::PartBound> = lv.siblings.iter().collect();
                bounds.push(&lv.bound);
                let _g = crate::catalog::partition::under(lv.colls.clone());
                bounds.sort_by(|a, b| crate::catalog::partition::bound_order(a, b));
                bounds.iter().position(|b| **b == lv.bound).unwrap_or(0)
            })
            .collect();
        path.reverse();
        match groups.iter_mut().find(|(b, _)| *b == base) {
            Some((_, v)) => v.push((path, addr(p))),
            None => groups.push((base, vec![(path, addr(p))])),
        }
    }
    for (_, mut v) in groups {
        if v.len() < 2 {
            continue;
        }
        let num = |n: &str| n.rsplit('_').next().and_then(|x| x.parse::<usize>().ok()).unwrap_or(0);
        let mut names: Vec<String> = v.iter().map(|(_, a)| out[a].clone()).collect();
        names.sort_by_key(|n| num(n));
        v.sort_by(|a, b| a.0.cmp(&b.0));
        for ((_, a), n) in v.into_iter().zip(names) {
            out.insert(a, n);
        }
    }
    // The relations of one query level that share a name (a view read twice and pulled
    // up) take their numbers in range-table order, not in the order the joins put them.
    let mut levels: Vec<Vec<(&Plan, LevelKey)>> = vec![Vec::new()];
    level_scans(root, 0, &mut levels);
    let target = target.map(addr);
    for level in levels {
        let mut groups: Vec<(String, Vec<(LevelKey, usize)>)> = Vec::new();
        for (p, key) in level {
            let Plan::Scan(s) = p else { continue };
            if s.rti.0 == 0 || Some(addr(p)) == target || !out.contains_key(&addr(p)) {
                continue;
            }
            let base = s.alias.clone().unwrap_or_else(|| s.table.name.clone());
            match groups.iter_mut().find(|(b, _)| *b == base) {
                Some((_, v)) => v.push((key, addr(p))),
                None => groups.push((base, vec![(key, addr(p))])),
            }
        }
        for (base, mut v) in groups {
            if v.len() < 2 {
                continue;
            }
            let num = |n: &str| {
                n.strip_prefix(base.as_str())
                    .and_then(|x| x.strip_prefix('_'))
                    .and_then(|x| x.parse::<usize>().ok())
                    .unwrap_or(0)
            };
            let mut names: Vec<String> = v.iter().map(|(_, a)| out[a].clone()).collect();
            names.sort_by_key(|n| num(n));
            v.sort_by_key(|x| x.0);
            for ((_, a), n) in v.into_iter().zip(names) {
                out.insert(a, n);
            }
        }
    }
    inherited_parents_in_order(root, &mut out);
    (out, results)
}

/// One range table entry of a query level (`inherited_parents_in_order`): a reference name,
/// its place, the node named after it (None: a partitioned table joined partitionwise, no
/// Append of its own), and its children (inheritance members) in the order they expand.
struct LevelRte {
    base: String,
    rti: crate::sql::plan::Rti,
    node: Option<usize>,
    kids: Vec<(Vec<usize>, usize)>,
}

/// Relations of one query level that share a name with two or more inheritance parents
/// among them (a pulled-up sub-select reusing an alias): Postgres names the level's range
/// table entries in order (parents and plain relations), then the parents' children as
/// expand_inherited_rtentry appends them, parent by parent, each in bound order.
fn inherited_parents_in_order(root: &Plan, out: &mut HashMap<usize, String>) {
    fn bound_path(s: &crate::sql::plan::ScanPlan) -> Vec<usize> {
        let Some(levels) = &s.part else { return Vec::new() };
        let mut path: Vec<usize> = levels
            .iter()
            .map(|lv| {
                let mut bounds: Vec<&crate::catalog::PartBound> = lv.siblings.iter().collect();
                bounds.push(&lv.bound);
                let _g = crate::catalog::partition::under(lv.colls.clone());
                bounds.sort_by(|a, b| crate::catalog::partition::bound_order(a, b));
                bounds.iter().position(|b| **b == lv.bound).unwrap_or(0)
            })
            .collect();
        path.reverse();
        path
    }
    fn base_of(s: &crate::sql::plan::ScanPlan) -> String {
        s.alias.clone().unwrap_or_else(|| s.table.name.clone())
    }
    // (`bad`: a shape this doesn't follow (a sub-partitioned member, a UNION ALL): the
    // level is left as named.)
    fn walk(p: &Plan, cur: usize, levels: &mut Vec<(Vec<LevelRte>, bool)>) {
        match p {
            Plan::Scan(s) if s.rti.0 != 0 => {
                let base = base_of(s);
                if s.part.is_none() {
                    levels[cur].0.push(LevelRte { base, rti: s.rti, node: Some(addr(p)), kids: Vec::new() });
                    return;
                }
                // (A partition joined partitionwise: a child of its parent's entry.)
                let path = bound_path(s);
                match levels[cur].0.iter_mut().find(|u| u.rti == s.rti && u.node.is_none()) {
                    Some(u) => u.kids.push((path, addr(p))),
                    None => levels[cur].0.push(LevelRte { base, rti: s.rti, node: None, kids: vec![(path, addr(p))] }),
                }
            }
            Plan::Append { inh: Some(_), inputs, .. } => {
                let mut kids = Vec::new();
                let mut first: Option<(&crate::sql::plan::ScanPlan, usize)> = None;
                for (k, m) in inputs.iter().enumerate() {
                    let mut x: &Plan = m;
                    while let Plan::Project { input, .. } | Plan::Filter { input, .. } = x {
                        x = input;
                    }
                    match x {
                        Plan::Scan(s) => {
                            first.get_or_insert((s, addr(x)));
                            kids.push((vec![k], addr(x)));
                        }
                        Plan::Empty { .. } => {}
                        _ => levels[cur].1 = true,
                    }
                }
                let Some((s, a)) = first else { return };
                if inputs.len() < 2 {
                    // (A single member's Append is dropped: the member takes the name.)
                    levels[cur].0.push(LevelRte { base: base_of(s), rti: s.rti, node: Some(a), kids: Vec::new() });
                    return;
                }
                levels[cur].0.push(LevelRte { base: base_of(s), rti: s.rti, node: Some(addr(p)), kids });
            }
            Plan::Barrier { input, .. } => {
                levels.push((Vec::new(), false));
                let n = levels.len() - 1;
                walk(input, n, levels);
            }
            Plan::SetOp { .. } | Plan::Append { inh: None, alias: Some(_), .. } => levels[cur].1 = true,
            _ => {
                for c in plan_inputs(p) {
                    walk(c, cur, levels);
                }
            }
        }
    }
    let mut levels: Vec<(Vec<LevelRte>, bool)> = vec![(Vec::new(), false)];
    walk(root, 0, &mut levels);
    for (units, bad) in levels {
        if bad {
            continue;
        }
        let mut bases: Vec<&str> = Vec::new();
        for u in &units {
            if !bases.contains(&u.base.as_str()) {
                bases.push(&u.base);
            }
        }
        for base in bases {
            let mut group: Vec<&LevelRte> = units.iter().filter(|u| u.base == base).collect();
            // (Two entries of one place: a sub-partitioned tree, left as named.)
            let mut rtis: Vec<crate::sql::plan::Rti> = group.iter().map(|u| u.rti).collect();
            rtis.sort();
            rtis.dedup();
            if rtis.len() != group.len()
                || group.iter().filter(|u| u.node.is_none() || !u.kids.is_empty()).count() < 2
                || group.iter().flat_map(|u| u.node.iter().chain(u.kids.iter().map(|k| &k.1))).any(|a| !out.contains_key(a))
            {
                continue;
            }
            let num = |n: &str| -> usize {
                n.strip_prefix(base)
                    .and_then(|x| x.strip_prefix('_'))
                    .and_then(|x| x.parse::<usize>().ok())
                    .unwrap_or(0)
            };
            let mut names: Vec<String> = group
                .iter()
                .flat_map(|u| u.node.iter().chain(u.kids.iter().map(|k| &k.1)))
                .map(|a| out[a].clone())
                .collect();
            // (Partitioned tables joined partitionwise take names too, the first ones free.)
            let need = group.len() + group.iter().map(|u| u.kids.len()).sum::<usize>();
            let mut k = 0;
            while names.len() < need {
                let cand = if k == 0 { base.to_string() } else { crate::catalog::suffixed_name(base, &format!("_{k}")) };
                k += 1;
                if !names.contains(&cand) && !out.values().any(|v| *v == cand) {
                    names.push(cand);
                }
            }
            names.sort_by_key(|n| num(n));
            group.sort_by_key(|u| u.rti);
            let mut it = names.into_iter();
            for u in &group {
                let n = it.next().expect("a name per entry");
                if let Some(a) = u.node {
                    out.insert(a, n);
                }
            }
            for u in &group {
                let mut kids = u.kids.clone();
                kids.sort();
                for (_, a) in kids {
                    out.insert(a, it.next().expect("a name per child"));
                }
            }
        }
    }
}

/// The UNION ALL arms kept subqueries of each Append of the final plan, in its order.
fn arm_orders(n: &ExNode, out: &mut Vec<Vec<usize>>) {
    let arms: Vec<usize> = n.children.iter().map(|c| c.arm).filter(|a| *a != 0).collect();
    if n.node_type == "Append" && arms.len() > 1 {
        out.push(arms);
    }
    for c in n.children.iter().chain(&n.initplans).chain(&n.subplans) {
        arm_orders(c, out);
    }
}

/// The collation a grouping key of an aggregation over `input` groups (and sorts) under,
/// when it isn't the database's default: its implicit COLLATE, or its column's.
fn group_key_collation(input: &Plan, g: &Expr) -> Option<String> {
    match g {
        Expr::Collate(_, n, -1) => Some(n.clone()),
        Expr::Col(j, _) => crate::sql::binder::optimize::output_collation(input, *j),
        _ => None,
    }
}

/// A DISTINCT ON / grouping key that is a column (or one carrying its implicit collation)
/// an equality with a constant fixes.
pub(crate) fn key_col_fixed(e: &Expr, fixed: &[bool]) -> bool {
    implicit_key_col(e).is_some_and(|j| fixed.get(j).copied().unwrap_or(false))
}

/// A grouping key that is a column, or a column carrying its nondeterministic collation
/// (the binder's implicit COLLATE, no location).
/// The group keys whose pathkeys are redundant (make_pathkeys_for_sortclauses_extended
/// with remove_redundant, out of processed_groupClause): fixed to a constant by an equality,
/// or a pseudoconstant expression (no Vars, nothing volatile: a constant, an InitPlan's
/// output), its equivalence class holding a constant.
fn fixed_group_keys(input: &Plan, groups: &[Expr]) -> Vec<bool> {
    let fixed = super::paths::constant_columns(input);
    // (A one-row VALUES list is pulled up: its expressions are the keys.)
    let computed: Option<&Vec<Expr>> = match input {
        Plan::Project { exprs, .. } => Some(exprs),
        Plan::Values { rows, .. } if rows.len() == 1 => rows.first(),
        _ => None,
    };
    let pseudo = |e: &Expr| crate::sql::binder::optimize::pseudo_constant(e);
    groups
        .iter()
        .map(|g| match implicit_key_col(g) {
            Some(j) => {
                fixed.get(j).copied().unwrap_or(false)
                    || computed.and_then(|es| es.get(j)).is_some_and(|e| !matches!(e, Expr::Col(..)) && pseudo(e))
            }
            None => pseudo(g),
        })
        .collect()
}

fn implicit_key_col(g: &Expr) -> Option<usize> {
    match g {
        Expr::Col(j, _) => Some(*j),
        Expr::Collate(x, _, -1) => match &**x {
            Expr::Col(j, _) => Some(*j),
            _ => None,
        },
        _ => None,
    }
}

/// The table scans of each query level (a level's own, not inside a subquery level or an
/// inheritance tree).
/// A scan's place among its level's (`level_scans`): its range table place, a pulled-up
/// UNION ALL's members after it in their order (the UNION ALL placed by its first
/// relation's place, each arm's relations by their places).
type LevelKey = (crate::sql::plan::Rti, usize, crate::sql::plan::Rti);

fn level_scans<'p>(p: &'p Plan, cur: usize, levels: &mut Vec<Vec<(&'p Plan, LevelKey)>>) {
    fn min_rti(p: &Plan) -> Option<crate::sql::plan::Rti> {
        match p {
            Plan::Scan(s) if s.rti.0 != 0 => Some(s.rti),
            _ => plan_inputs(p).into_iter().filter_map(|c| min_rti(c)).min(),
        }
    }
    fn union_all(p: &Plan) -> bool {
        matches!(
            p,
            Plan::SetOp {
                kind: SetOpKind::Union,
                all: true,
                ..
            } | Plan::Append { inh: None, .. }
        )
    }
    // `arm`: within a UNION ALL, (its place, the arm's number); `next`: the next arm's.
    fn walk<'p>(
        p: &'p Plan,
        cur: usize,
        arm: Option<(crate::sql::plan::Rti, usize)>,
        next: &mut usize,
        levels: &mut Vec<Vec<(&'p Plan, LevelKey)>>,
    ) {
        match p {
            Plan::Scan(s) if s.part.is_none() => {
                let key = match arm {
                    Some((g, a)) => (g, a, s.rti),
                    None => (s.rti, 0, s.rti),
                };
                levels[cur].push((p, key));
            }
            Plan::Append { inh: Some(_), .. } => {}
            Plan::Barrier { input, .. } => {
                levels.push(Vec::new());
                let n = levels.len() - 1;
                walk(input, n, None, &mut 0, levels);
            }
            // (A UNION ALL's arms are pulled up into the query's append relation: their
            // range table entries come after its other ones, arm by arm.)
            _ if union_all(p) => {
                let (g, mut own) = match arm {
                    Some((g, _)) => (g, None),
                    None => (min_rti(p).unwrap_or_default(), Some(0)),
                };
                let counter = own.as_mut().unwrap_or(next);
                for c in plan_inputs(p) {
                    if union_all(c) {
                        walk(c, cur, Some((g, 0)), counter, levels);
                    } else {
                        *counter += 1;
                        let a = *counter;
                        walk(c, cur, Some((g, a)), &mut 0, levels);
                    }
                }
            }
            // (Other set operations' arms are subqueries: levels of their own.)
            Plan::SetOp { .. } => {
                for c in plan_inputs(p) {
                    levels.push(Vec::new());
                    let n = levels.len() - 1;
                    walk(c, n, None, &mut 0, levels);
                }
            }
            _ => {
                for c in plan_inputs(p) {
                    walk(c, cur, arm, next, levels);
                }
            }
        }
    }
    walk(p, cur, None, &mut 0, levels);
}

/// The reference names of the partitioned tables whose partitions the plan scans outside
/// an inheritance Append of theirs (partitionwise joins), in plan order.
fn partitionwise_parents(p: &Plan, under: bool, out: &mut Vec<String>, skip: &[usize]) {
    match p {
        Plan::Scan(s) if !under && s.part.is_some() => {
            let base = s.alias.clone().unwrap_or_else(|| s.table.name.clone());
            if !out.contains(&base) {
                out.push(base);
            }
        }
        Plan::Append {
            inputs,
            inh: Some(_),
            ..
        } => inputs.iter().for_each(|i| partitionwise_parents(i, true, out, skip)),
        // (A partitionwise join of one partition pair has no Append: its partitions keep
        // their parents' names.)
        // (One grouped partitionwise: its parents go unnamed.)
        Plan::Append { inh: None, .. } if skip.contains(&addr(p)) => {}
        Plan::Append { inputs, inh: None, .. } if inputs.len() > 1 => inputs
            .iter()
            .for_each(|i| partitionwise_parents(i, false, out, skip)),
        _ => plan_inputs(p)
            .into_iter()
            .for_each(|i| partitionwise_parents(i, under, out, skip)),
    }
}

/// `text` with the column qualifier `from` (`rel.`) replaced by `to` wherever it starts a
/// name (not inside another identifier or a quoted string).
fn replace_qualifier(text: &str, from: &str, to: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let b = text.as_bytes();
    let mut quoted = false;
    while i < text.len() {
        if b[i] == b'\'' {
            quoted = !quoted;
        }
        let boundary = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_' || b[i - 1] == b'"');
        if !quoted && boundary && text[i..].starts_with(from) {
            out.push_str(to);
            i += from.len();
            continue;
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// The partition scans of a partitionwise join's child (outside any inheritance Append):
/// their own reference names, and their parent's.
fn partitionwise_scans<'p>(p: &'p Plan, out: &mut Vec<&'p Plan>) {
    match p {
        Plan::Scan(s) if s.part.is_some() => out.push(p),
        Plan::Append { inh: Some(_), .. } => {}
        _ => plan_inputs(p)
            .into_iter()
            .for_each(|i| partitionwise_scans(i, out)),
    }
}

/// The partition scans a partitionwise join's first child join reads, as a node above
/// the join's Append names them: ruleutils descends to an Append's first child (a
/// partition's own partitions' Append too), and a Var whose topmost parent is among the
/// Appends' apprelids prints as that parent's (get_variable's appendparents).
fn pw_named_scans<'p>(p: &'p Plan, out: &mut Vec<&'p Plan>) {
    match p {
        Plan::Scan(s) if s.part.is_some() => out.push(p),
        Plan::Append { inh: Some(_), inputs, .. } => {
            if let Some(first) = inputs.first() {
                pw_named_scans(first, out);
            }
        }
        _ => plan_inputs(p)
            .into_iter()
            .for_each(|i| pw_named_scans(i, out)),
    }
}

fn type_width(t: &DataType) -> u32 {
    costs::type_width(t)
}

fn conjuncts(e: &Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::And(list) => list.iter().for_each(|x| conjuncts(x, out)),
        e => out.push(e.clone()),
    }
}

/// The binder's widening casts for cross-type comparisons, which Postgres doesn't show.
/// Through a relabelling to a binary-compatible type (varchar to text): the column.
fn strip_relabel(e: &Expr) -> &Expr {
    match e {
        Expr::Relabel(x, _) | Expr::Collate(x, ..) => strip_relabel(x),
        // (A hash key under a nondeterministic collation: its column's statistics.)
        Expr::Call(d, a, _) if d.name == crate::sql::binder::NONDETERMINISTIC_HASH => strip_relabel(&a[0]),
        Expr::Cast(x, from, to, false)
            if matches!(
                (from, to),
                (DataType::Varchar(_) | DataType::Name | DataType::Text, DataType::Text | DataType::Varchar(_))
            ) =>
        {
            strip_relabel(x)
        }
        e => e,
    }
}

fn strip_widening(e: &Expr) -> &Expr {
    match e {
        Expr::Cast(inner, from, to, false)
            if matches!(
                (from, to),
                (DataType::Int2, DataType::Int4 | DataType::Int8)
                    | (DataType::Int4, DataType::Int8)
                    | (DataType::Float4, DataType::Float8)
            ) =>
        {
            inner
        }
        e => e,
    }
}

/// A conjunct an index scan uses as its bound: the key column, and whether the column is on
/// the right (so it prints commuted).
fn index_qual(e: &Expr, keys: &[usize]) -> Option<(usize, bool)> {
    let key = |x: &Expr| match strip_widening(x) {
        Expr::Col(k, _) if keys.contains(k) => Some(*k),
        _ => None,
    };
    match e {
        Expr::Call(def, args, _)
            if args.len() == 2 && matches!(def.name, "=" | "<" | "<=" | ">" | ">=") =>
        {
            if let Some(k) = key(&args[0])
                && args[1].columns().is_empty()
            {
                return Some((k, false));
            }
            if let Some(k) = key(&args[1])
                && args[0].columns().is_empty()
            {
                return Some((k, true));
            }
            None
        }
        Expr::ArrayOp {
            left,
            right,
            all: false,
            ..
        } => key(left)
            .filter(|_| right.columns().is_empty())
            .map(|k| (k, false)),
        Expr::InList(x, list, false) => key(x)
            .filter(|_| list.iter().all(|(v, _)| v.columns().is_empty()))
            .map(|k| (k, false)),
        _ => None,
    }
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

fn and_text(parts: Vec<String>) -> Option<String> {
    match parts.len() {
        0 => None,
        1 => parts.into_iter().next(),
        _ => Some(format!("({})", parts.join(" AND "))),
    }
}

/// A sort key as an Incremental Sort's presorted keys show it: without its direction
/// (show_sort_group_keys lists the bare expressions).
/// A text value computed from a `name` column (through casts and calls) takes that
/// column's collation, "C", implicitly (a non-default implicit collation beats the
/// default, parse_collate.c): a sort on it is under "C", which isn't the text type's own.
fn name_derived_c(e: &Expr) -> bool {
    fn text(t: &DataType) -> bool {
        matches!(t, DataType::Text | DataType::Varchar(_) | DataType::Bpchar(_))
    }
    fn from_name(e: &Expr) -> bool {
        match e {
            Expr::Col(_, DataType::Name) => true,
            Expr::Cast(x, _, to, _) => text(to) && from_name(x),
            Expr::Call(_, args, ty) => text(ty) && args.iter().any(from_name),
            Expr::Relabel(x, _) => from_name(x),
            _ => false,
        }
    }
    !matches!(e, Expr::Col(..)) && text(&e.ty()) && from_name(e)
}

/// A sort key's text without the collation a `name`-derived value shows (`name_derived_c`).
fn uncollated(k: &str) -> String {
    k.strip_suffix(" COLLATE \"C\"").unwrap_or(k).to_string()
}

fn presorted_name(k: &str) -> String {
    let mut s = k;
    for sfx in [" NULLS FIRST", " NULLS LAST"] {
        if let Some(x) = s.strip_suffix(sfx) {
            s = x;
            break;
        }
    }
    s.strip_suffix(" DESC").unwrap_or(s).to_string()
}

fn sort_suffix(k: &SortKey) -> String {
    let mut s = String::new();
    // A made operator class's ordering operator (its comparison function's stand-in).
    if let Some(op) = k
        .using
        .as_ref()
        .and_then(|u| u.coll.as_ref())
        .and_then(|c| crate::catalog::partition::comparator_op(c))
    {
        let _ = write!(s, " USING {op}");
        return s;
    }
    // A key sorted under its (non-byte-order) collation shows it (not the database default,
    // its `<` / `>` the type's own: show_sortorder_options).
    let default_coll = k
        .using
        .as_ref()
        .and_then(|op| op.coll.as_ref())
        .is_some_and(|c| c.oid == crate::types::collation::DEFAULT_OID);
    if let Some(coll) = k
        .using
        .as_ref()
        .and_then(|op| op.coll.as_ref())
        .filter(|_| !default_coll)
    {
        let _ = write!(s, " COLLATE {}", crate::catalog::quote_ident(&coll.name));
        if k.desc {
            s.push_str(" DESC");
        }
    // (An enum's own `<` / `>`, which the engine carries to compare its labels: its default
    // btree class's, so no USING.)
    } else if let Some(op) = k.using.as_ref().filter(|op| {
        !(matches!(op.name, "<" | ">")
            && (default_coll || matches!(k.expr.ty(), crate::types::DataType::Enum(..))))
    }) {
        let _ = write!(s, " USING {}", op.name);
    } else if k.desc {
        s.push_str(" DESC");
    }
    if k.nulls_first && !k.desc {
        s.push_str(" NULLS FIRST");
    } else if !k.nulls_first && k.desc {
        s.push_str(" NULLS LAST");
    }
    s
}

fn win_name(k: WinKind) -> &'static str {
    match k {
        WinKind::RowNumber => "row_number",
        WinKind::Rank => "rank",
        WinKind::DenseRank => "dense_rank",
        WinKind::PercentRank => "percent_rank",
        WinKind::CumeDist => "cume_dist",
        WinKind::Ntile => "ntile",
        WinKind::Lag => "lag",
        WinKind::Lead => "lead",
        WinKind::FirstValue => "first_value",
        WinKind::LastValue => "last_value",
        WinKind::NthValue => "nth_value",
    }
}

fn join_type(k: JoinKind) -> &'static str {
    match k {
        JoinKind::Inner => "Inner",
        JoinKind::Left => "Left",
        JoinKind::Right => "Right",
        JoinKind::Full => "Full",
        JoinKind::Semi => "Semi",
        JoinKind::Anti => "Anti",
    }
}

impl Builder<'_, '_> {
    /// The grouping keys that group nothing more (remove_useless_groupby_columns:
    /// functionally dependent on a table's unique key among the keys; a key an equality
    /// fixes; one equal to an earlier one): out of the shown keys, the sort and the hash.
    fn redundant_keys(&self, input: &Plan, groups: &[Expr], sets: Option<&[Vec<usize>]>, cols: &[ColInfo]) -> Vec<bool> {
        let var = |k: usize| match &groups[k] {
            Expr::Col(j, _) => cols
                .get(*j)
                .and_then(|c| c.var.as_ref())
                .filter(|v| v.expr.is_none() && (!v.inh || v.table.partkey.is_some())),
            _ => None,
        };
        let common = |k: usize| sets.is_none_or(|ss| ss.iter().all(|s| s.contains(&k)));
        let mut out = vec![false; groups.len()];
        if groups.len() >= 2 {
            let cat = self.ctx.catalog();
            let mut done: Vec<u32> = Vec::new();
            for i in 0..groups.len() {
                let Some(v) = var(i).filter(|_| common(i)) else { continue };
                let t = &v.table;
                if done.contains(&t.oid) {
                    continue;
                }
                done.push(t.oid);
                // (The table's grouped columns.)
                let grouped: Vec<usize> = (0..groups.len())
                    .filter(|&k| common(k))
                    .filter_map(|k| var(k).filter(|w| w.table.oid == t.oid).map(|w| w.col))
                    .collect();
                // (The primary key first, then the indexes.)
                let pk_deferrable = t.constraints.iter().any(|c| {
                    matches!(c.kind, crate::catalog::ConstraintKind::PrimaryKey { .. }) && c.deferrable
                });
                let mut best: Option<Vec<usize>> = t
                    .pkey
                    .clone()
                    .filter(|pk| !pk_deferrable && pk.iter().all(|k| grouped.contains(k)));
                for ix in cat.table_indexes(t) {
                    // (Of the grouping's own equality: default operator classes.)
                    let cols: Option<Vec<usize>> = ix
                        .columns
                        .iter()
                        .map(|c| match c.key {
                            crate::catalog::IndexKey::Column(k) if c.opclass == 0 => Some(k),
                            _ => None,
                        })
                        .collect();
                    let Some(cols) = cols else { continue };
                    let deferrable = t
                        .constraints
                        .iter()
                        .any(|c| c.kind.index() == Some(ix.oid) && c.deferrable);
                    let not_null = ix.nulls_not_distinct
                        || cols.iter().all(|&k| t.columns.get(k).is_some_and(|c| c.not_null));
                    if !ix.unique
                        || !ix.valid
                        || deferrable
                        || ix.predicate.is_some()
                        || !not_null
                        || !cols.iter().all(|k| grouped.contains(k))
                    {
                        continue;
                    }
                    if best.as_ref().is_none_or(|b| cols.len() < b.len()) {
                        best = Some(cols);
                    }
                }
                if let Some(b) = best {
                    for (k, o) in out.iter_mut().enumerate() {
                        if common(k) && var(k).is_some_and(|w| w.table.oid == t.oid && !b.contains(&w.col)) {
                            *o = true;
                        }
                    }
                }
            }
        }
        // (A key fixed to a constant by an equality: a redundant pathkey, out of
        // processed_groupClause.)
        if sets.is_none() {
            let fixed = fixed_group_keys(input, groups);
            if !fixed.iter().all(|f| *f) {
                for (k, f) in fixed.iter().enumerate() {
                    if *f {
                        out[k] = true;
                    }
                }
            }
        }
        if sets.is_none() {
            let classes = super::paths::column_classes(input);
            let mut seen: Vec<usize> = Vec::new();
            for (k, g) in groups.iter().enumerate() {
                if out[k] {
                    continue;
                }
                if let Expr::Col(j, _) = g
                    && let Some((r, false)) = classes.get(*j).copied()
                {
                    if seen.contains(&r) {
                        out[k] = true;
                    } else {
                        seen.push(r);
                    }
                }
            }
        }
        out
    }

    fn full(&self, cx: Cx) -> bool {
        match cx {
            Cx::Scan => self.o.verbose,
            Cx::Upper => self.prefix || self.o.verbose,
            Cx::Output => self.prefix,
        }
    }

    fn pick(&self, n: &Name, cx: Cx) -> String {
        if self.full(cx) {
            n.full.clone()
        } else {
            n.plain.clone()
        }
    }

    /// `e` over columns `cols`, deparsed bare and qualified.
    fn name(&mut self, e: &Expr, cols: &[Name], aggs: &[AggCall]) -> Name {
        if let Some(x) = self.inline_sub_params(e) {
            return self.name(&x, cols, aggs);
        }
        // (A record column of a FROM subquery never planned: its fields `f1`, `f2`, ….)
        if !self.unplanned_cols.is_empty() {
            let unknown = |x: &Expr| {
                matches!(x, Expr::Field(b, ..) if matches!(&**b, Expr::Col(j, DataType::Row(rt))
                    if rt.oid == 0 && cols.get(*j).is_some_and(|n| self.unplanned_cols.contains(&n.full))
                        && rt.fields.iter().enumerate().any(|(k, f)| f.0 != format!("f{}", k + 1))))
            };
            let mut any = false;
            e.walk(&mut |x| any |= unknown(x));
            if any {
                let e2 = e
                    .clone()
                    .transform(&mut |x| {
                        Ok(match x {
                            Expr::Field(b, k, t) if unknown(&Expr::Field(b.clone(), k, t.clone())) => {
                                let Expr::Col(j, DataType::Row(rt)) = *b else { unreachable!() };
                                let fields = rt.fields.iter().enumerate().map(|(k, f)| (format!("f{}", k + 1), f.1.clone())).collect();
                                let ty = DataType::Row(Arc::new(crate::types::RowType { fields, ..(*rt).clone() }));
                                Expr::Field(Box::new(Expr::Col(j, ty)), k, t)
                            }
                            x => x,
                        })
                    })
                    .unwrap_or_else(|_| e.clone());
                return self.name(&e2, cols, aggs);
            }
        }
        // A relation's whole row (`ROW(...)` of all its columns, of its row type): the
        // whole-row Var `rel.*`, which ruleutils always qualifies.
        let whole_vars = &self.whole_vars;
        let whole_cat = self.ctx.catalog();
        let whole_row_of = |x: &Expr| -> Option<String> {
            // (Null-extended above an outer join: the same Var.)
            let x = crate::sql::binder::nullable_whole_row(x).unwrap_or(x);
            let Expr::Row(items, DataType::Row(rt)) = x else { return None };
            // (A relation of a named row type; a VALUES list's or a subquery's whole row is
            // a ROW() of its columns by then, but not a CTE Scan's.)
            if items.is_empty() || rt.fields.len() != items.len() || !crate::sql::binder::is_whole_row(x) {
                return None;
            }
            let mut qual: Option<String> = None;
            for (item, (fname, _)) in items.iter().zip(&rt.fields) {
                let Expr::Col(j, _) = item else { return None };
                let n = cols.get(*j)?;
                let q = n.full.strip_suffix(&format!(".{}", quote_ident(fname)))?;
                if n.plain != quote_ident(fname) || qual.as_deref().is_some_and(|p| p != q) {
                    return None;
                }
                qual = Some(q.to_string());
            }
            if rt.oid == 0 && qual.as_ref().is_none_or(|q| whole_vars.get(q) != Some(&items.len())) {
                return None;
            }
            // (A view's row, or a composite type's, is no relation scanned: the view's
            // subquery is pulled up, and its whole-row Var becomes a ROW() of the subquery's
            // outputs (pullup_replace_vars), typed as the view's row.)
            if rt.oid != 0
                && (whole_cat.composites.contains_key(&rt.oid)
                    || whole_cat.views.values().any(|v| v.rowtype == rt.oid && !v.materialized))
            {
                return None;
            }
            qual
        };
        let mut whole = false;
        e.walk(&mut |x| whole |= whole_row_of(x).is_some());
        if whole {
            // (Top down: a null-extended row is replaced whole.)
            fn top_down(e: Expr, f: &mut dyn FnMut(&Expr) -> Option<Expr>) -> Expr {
                if let Some(r) = f(&e) {
                    return r;
                }
                e.map_children(&mut |c| Ok(top_down(c, f))).expect("top_down")
            }
            // (A partitioned table's whole row is its partition's converted to the parent's
            // row type, a ConvertRowtypeExpr: `(t.*)::parent`.)
            let converted = |ty: &DataType| -> Option<String> {
                let DataType::Row(rt) = ty else { return None };
                let cat = self.ctx.catalog();
                cat.tables
                    .values()
                    .any(|t| t.rowtype == rt.oid && rt.oid != 0 && t.partkey.is_some())
                    .then(|| cat.format_type(ty, None, &self.ctx.search_path))
            };
            let mut more: Vec<Name> = cols.to_vec();
            let e = top_down(e.clone(), &mut |x| {
                // (Cast itself, a converted row is no simple operand: get_coercion_expr's
                // get_rule_expr_paren puts it in parentheses of its own,
                // `(((t1.*)::prt1))::text`.)
                if let Expr::Cast(inner, from, to, f) = x
                    && let Some(q) = whole_row_of(inner)
                    && let Some(tn) = converted(&inner.ty())
                {
                    more.push(Name::same(format!("(({q}.*)::{tn})")));
                    let col = Expr::Col(more.len() - 1, inner.ty());
                    return Some(Expr::Cast(Box::new(col), from.clone(), to.clone(), *f));
                }
                let q = whole_row_of(x)?;
                more.push(Name::same(match converted(&x.ty()) {
                    Some(tn) => format!("({q}.*)::{tn}"),
                    None => format!("{q}.*"),
                }));
                Some(Expr::Col(more.len() - 1, x.ty()))
            });
            return self.name(&e, &more, aggs);
        }
        // A column a stored view reads that has been dropped since: by the name the view
        // knows it by (its RTE's column names), qualified as the relation's others are.
        let is_dropped = |x: &Expr| {
            matches!(x, Expr::Call(d, args, _) if d.oid == 0
                && d.name == "raise_error"
                && args.len() == 4)
        };
        let mut dropped = false;
        e.walk(&mut |x| dropped |= is_dropped(x));
        if dropped {
            let mut more: Vec<Name> = cols.to_vec();
            let e = e
                .clone()
                .transform(&mut |x| {
                    if !is_dropped(&x) {
                        return Ok(x);
                    }
                    let Expr::Call(_, args, ty) = &x else { unreachable!() };
                    let Expr::Const(Value::Text(shown), _) = &args[3] else {
                        return Ok(x);
                    };
                    more.push(split_qualified(shown));
                    Ok(Expr::Col(more.len() - 1, ty.clone()))
                })
                .unwrap_or_else(|_| e.clone());
            return self.name(&e, &more, aggs);
        }
        // GROUPING(keys): a test of the grouping set mask column, shown by the keys.
        // (Without grouping sets, GROUPING over the keys themselves: always 0.)
        let is_grouping = |x: &Expr| {
            matches!(x, Expr::Call(d, args, _) if d.oid == 0 && d.name == "grouping"
                && matches!(args.first(), Some(Expr::Col(m, _)) if cols.get(*m).is_some_and(|n| n.plain == "GROUPING")))
                || matches!(x, Expr::Call(d, args, _) if d.oid == 0 && d.name == "GROUPING"
                    && args.iter().all(|a| matches!(a, Expr::Col(k, _) if *k < cols.len())))
        };
        // (A GROUPING() argument that is a sub-select group key: a copy of its own, each
        // call's, planned and numbered with the target list's expressions; an InitPlan of
        // the level when uncorrelated, else a SubPlan nothing runs.)
        let mut copies: HashMap<(usize, usize), String> = HashMap::new();
        if !self.key_subs.is_empty() {
            let mut calls: Vec<(usize, Vec<usize>)> = Vec::new();
            e.walk(&mut |x| {
                if is_grouping(x)
                    && let Expr::Call(d, args, _) = x
                    && d.name == "GROUPING"
                {
                    let keys = args.iter().filter_map(|a| match a { Expr::Col(k, _) => Some(*k), _ => None }).collect();
                    calls.push((Arc::as_ptr(d) as usize, keys));
                } else if is_grouping(x)
                    && let Expr::Call(d, args, _) = x
                {
                    let keys = args[2..].iter().filter_map(|a| match a { Expr::Const(v, _) => v.as_i64().map(|k| k as usize), _ => None }).collect();
                    calls.push((Arc::as_ptr(d) as usize, keys));
                }
            });
            for (ptr, keys) in calls {
                for (ai, k) in keys.into_iter().enumerate() {
                    let Some(n) = cols.get(k) else { continue };
                    let Some(s) = self.key_subs.get(&n.full).or_else(|| self.key_subs.get(&n.plain)).cloned() else {
                        continue;
                    };
                    let label = match self.phantoms.get(&(ptr, ai)) {
                        Some(l) => l.clone(),
                        None => {
                            let mut copy = s;
                            copy.id = GROUPING_COPY_BASE + self.phantoms.len();
                            copy.qual = false;
                            // (Numbered with the target list the GROUPING() call is in.)
                            copy.tlist = true;
                            let before = self.pending.len();
                            self.subquery(&copy, &[]);
                            let Some(l) = self.labels.get(&copy.id).cloned() else { continue };
                            if !l.init {
                                self.pending.truncate(before);
                            }
                            self.phantoms.insert((ptr, ai), l.clone());
                            l
                        }
                    };
                    let shown = if label.init { format!("({}).col1", label.name) } else { format!("({})", label.name) };
                    copies.insert((ptr, ai), shown);
                }
            }
        }
        let mut found = false;
        e.walk(&mut |x| found |= is_grouping(x));
        let (grouped, ext);
        let (e, cols) = if found {
            let mut more: Vec<Name> = cols.to_vec();
            grouped = e
                .clone()
                .transform(&mut |x| {
                    if !is_grouping(&x) {
                        return Ok(x);
                    }
                    let Expr::Call(d, args, _) = &x else { unreachable!() };
                    let ptr = Arc::as_ptr(d) as usize;
                    let keys: Vec<&Name> = if d.name == "GROUPING" {
                        args.iter()
                            .filter_map(|a| match a {
                                Expr::Col(k, _) => cols.get(*k),
                                _ => None,
                            })
                            .collect()
                    } else {
                        args[2..]
                            .iter()
                            .filter_map(|a| match a {
                                Expr::Const(v, _) => v.as_i64().and_then(|k| cols.get(k as usize)),
                                _ => None,
                            })
                            .collect()
                    };
                    let list = |f: &dyn Fn(&Name) -> &str| {
                        keys.iter()
                            .enumerate()
                            .map(|(ai, k)| copies.get(&(ptr, ai)).map_or_else(|| f(k).to_string(), |c| c.clone()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    more.push(Name {
                        plain: format!("GROUPING({})", list(&|n| &n.plain)),
                        full: format!("GROUPING({})", list(&|n| &n.full)),
                    });
                    Ok(Expr::Col(more.len() - 1, DataType::Int4))
                })
                .unwrap_or_else(|_| e.clone());
            ext = more;
            (&grouped, &ext[..])
        } else {
            (e, cols)
        };
        let shown;
        let e = if has_calls(e) {
            shown = show_calls(e.clone());
            &shown
        } else {
            e
        };
        let bound;
        let e = if self.params.is_empty() {
            e
        } else {
            let params = self.params;
            let ctx = self.ctx;
            bound = e
                .clone()
                .transform(&mut |x| {
                    Ok(match x {
                        Expr::Param(i, t) if i < params.len() => Expr::Const(params[i].clone(), t),
                        // (An array of the values: eval_const_expressions folds it.)
                        Expr::Array(items, t)
                            if items
                                .iter()
                                .all(|i| i.columns().is_empty() && !i.has_outer_refs() && i.is_immutable())
                                && items.iter().any(|i| !matches!(i, Expr::Const(..))) =>
                        {
                            let arr = Expr::Array(items, t.clone());
                            match crate::sql::expr::eval(&arr, &[], ctx) {
                                Ok(v) => Expr::Const(v, t),
                                Err(_) => arr,
                            }
                        }
                        x => x,
                    })
                })
                .unwrap_or_else(|_| e.clone());
            &bound
        };
        self.register(e, cols);
        // (Function calls run as subqueries in the aggregates' arguments: shown as the
        // calls.)
        let shown_aggs: Vec<AggCall>;
        let aggs = if aggs
            .iter()
            .any(|a| a.args.iter().chain(a.filter.as_ref()).any(has_calls))
        {
            shown_aggs = aggs
                .iter()
                .map(|a| {
                    let mut a = a.clone();
                    a.args = a
                        .args
                        .into_iter()
                        .map(|x| if has_calls(&x) { show_calls(x) } else { x })
                        .collect();
                    a.filter = a.filter.map(|x| if has_calls(&x) { show_calls(x) } else { x });
                    a
                })
                .collect();
            &shown_aggs[..]
        } else {
            aggs
        };
        let plain: Vec<String> = cols.iter().map(|c| c.plain.clone()).collect();
        let full: Vec<String> = cols.iter().map(|c| c.full.clone()).collect();
        let d = |cs: &[String]| {
            deparse_explain(e, cs, &self.outer, aggs, &self.labels, self.ctx)
                .unwrap_or_else(|| format!("{e:?}"))
        };
        Name {
            plain: d(&plain),
            full: d(&full),
        }
    }

    fn show(&mut self, e: &Expr, cols: &[Name], cx: Cx) -> String {
        // A scan's restriction comes from its equivalence class.
        let relabeled;
        let e = if cx == Cx::Scan {
            relabeled = ec_relabel(e);
            &relabeled
        } else {
            e
        };
        let relabeled2;
        let e = if cx != Cx::Output && !OUTER_JOIN_SIDE.with(|o| o.get()) {
            relabeled2 = name_text_relabel(e, cx == Cx::Scan);
            &relabeled2
        } else {
            e
        };
        let n = self.name(e, cols, &[]);
        self.pick(&n, cx)
    }

    /// Number and convert the subqueries in `e` (whose columns are `cols`).
    fn register(&mut self, e: &Expr, cols: &[Name]) {
        let mut sq = Vec::new();
        subqueries_in(e, &mut sq);
        for s in sq {
            if !self.labels.contains_key(&s.id) {
                let before = self.pending.len();
                self.subquery(s, cols);
                if self.detach {
                    // (Labeled, its SubPlan kept for a place the executor initializes.)
                    if let Some(alt) = self.alts.get_mut(&s.id) {
                        alt.shown = [false; 2];
                        self.pending.truncate(before);
                    } else if self.pending.len() > before
                        && let Some(n) = self.pending.pop()
                    {
                        self.unattached.insert(s.id, n);
                    }
                }
            } else if let Some(alt) = self.alts.get_mut(&s.id) {
                // (Another place reading it: its own choice.)
                let i = alt.pick(self.num_exec.unwrap_or(0.0));
                let (label, node) = alt.get(i).clone();
                if !alt.shown[i] && !self.detach {
                    alt.shown[i] = true;
                    self.pending.push(node);
                }
                self.labels.insert(s.id, label);
            } else if !self.detach
                && let Some(n) = self.unattached.remove(&s.id)
            {
                self.pending.push(n);
            }
        }
    }

    /// The SubPlans shown, by sub-select id: their numbers (a correlated EXISTS's plain
    /// one's when shown, else its hashed one's).
    fn sub_ranks(&self) -> HashMap<usize, usize> {
        let num = |l: &SubplanLabel| {
            l.name
                .rsplit(' ')
                .next()
                .and_then(|n| n.parse::<usize>().ok())
        };
        let mut out = HashMap::new();
        for (id, l) in &self.labels {
            if self.unattached.contains_key(id) {
                continue;
            }
            let n = match self.alts.get(id) {
                Some(a) if a.shown[0] => num(&a.plain.0),
                Some(a) if a.shown[1] => num(&a.hashed.0),
                Some(_) => None,
                None => num(l),
            };
            if let Some(n) = n {
                out.insert(*id, n);
            }
        }
        out
    }

    /// `e` with each nested loop parameter that holds a sub-select (`sub_params`) read as
    /// that expression over the outer side's columns; None when it reads none.
    fn inline_sub_params(&self, e: &Expr) -> Option<Expr> {
        if self.sub_params.is_empty() || !e.has_outer_refs() {
            return None;
        }
        let m = self.outer.len();
        let mut any = false;
        let out = e
            .clone()
            .transform(&mut |x| {
                Ok(match x {
                    Expr::Outer(d, k, t) => {
                        let frame = self.sub_params.iter().find(|(pos, ..)| m + 1 >= *pos && d == m + 1 - pos);
                        match frame {
                            Some((_, params, base)) if params.get(k).is_some_and(has_subselect) => {
                                any = true;
                                params[k]
                                    .clone()
                                    .transform(&mut |y| {
                                        Ok(match y {
                                            Expr::Col(i, t) => Expr::Outer(d, base + i, t),
                                            Expr::Outer(n, i, t) => Expr::Outer(n + d, i, t),
                                            y => y,
                                        })
                                    })
                                    .unwrap_or(Expr::Outer(d, k, t))
                            }
                            _ => Expr::Outer(d, k, t),
                        }
                    }
                    x => x,
                })
            })
            .ok()?;
        any.then_some(out)
    }

    /// The next SubPlan number, for the sub-select `sub` (`start`: the first number taken
    /// for the sub-selects inside it).
    fn next_plan_id(&mut self, sub: Option<&Subquery>, start: usize, follows: bool) -> usize {
        self.plan_ids += 1;
        let met = self.plan_ids;
        self.events.push(PlanIdEvent {
            level: self.lvl_stack.last().copied().unwrap_or(0),
            key: sub.is_some_and(|s| self.key_sub_ids.contains(&s.id)),
            start,
            id: sub.map(|s| s.id),
            qual: sub.is_some_and(|s| s.qual),
            tlist: sub.is_some_and(|s| s.tlist),
            follows,
        });
        match &self.renumber {
            Some(r) => r.get(met - 1).copied().unwrap_or(met),
            None => met,
        }
    }

    fn push_level(&mut self, apart: bool) {
        self.lvl_next += 1;
        self.spans.push(LevelSpan {
            level: self.lvl_next,
            parent: self.lvl_stack.last().copied().unwrap_or(0),
            apart,
            start: self.events.len(),
            end: usize::MAX,
        });
        self.lvl_stack.push(self.lvl_next);
    }

    fn pop_level(&mut self) {
        if let Some(l) = self.lvl_stack.pop()
            && let Some(sp) = self.spans.iter_mut().rev().find(|sp| sp.level == l)
        {
            sp.end = self.events.len();
        }
    }

    /// `Plan::Barrier::copies`: each sub-select numbered where the level makes it, an
    /// uncorrelated one an InitPlan of the level (SS_attach_initplans attaches them all), a
    /// correlated one attached nowhere, as nothing references it.
    fn unused_copies(&mut self, copies: &[Expr]) {
        for e in copies {
            let mut subs: Vec<&Subquery> = Vec::new();
            e.walk(&mut |x| {
                if let Expr::Subquery(s) = x
                    && s.call.is_none()
                {
                    subs.push(s);
                }
            });
            for s in subs {
                if self.labels.contains_key(&s.id) {
                    continue;
                }
                if s.params.is_empty() {
                    let saved = std::mem::take(&mut self.pending);
                    self.subquery(s, &[]);
                    self.pending = saved;
                } else {
                    let start = self.events.len();
                    self.next_plan_id(Some(s), start, false);
                }
            }
        }
    }

    fn subquery(&mut self, s: &Subquery, cols: &[Name]) {
        let block_start = self.events.len();
        let params: Vec<String> = s
            .params
            .iter()
            .map(|p| self.name(p, cols, &[]).full)
            .collect();
        let saved = std::mem::take(&mut self.pending);
        // (Without params it reads no values of its own: as the executor runs it, the
        // enclosing query's are its `Outer(1, …)` — a MIN / MAX InitPlan in a SubPlan.)
        let framed = !s.params.is_empty();
        // (Correlated: reading values of the enclosing level — its columns, or a
        // lateral join's row of that same level — not only those of levels further out.)
        let level_param = s.params.iter().any(|p| {
            let mut own = false;
            p.walk(&mut |x| match x {
                Expr::Col(..) => own = true,
                Expr::Outer(d, ..) => own |= self.outer_depth.get(d - 1).is_none_or(|&l| l == self.depth),
                _ => {}
            });
            own
        });
        if framed {
            self.outer.insert(0, params);
            self.outer_depth.insert(0, self.depth);
        }
        self.depth += 1;
        // (A user-defined operator's comparison is the test expression's: the subquery
        // returns its column.)
        let test = match (&*s.plan, s.test_op) {
            (Plan::Project { input, exprs }, Some(_)) if exprs.len() == 1 => Some((input.clone(), exprs[0].clone())),
            _ => None,
        };
        // (A MIN / MAX InitPlan the planner made: its path's quals are its level's, whose
        // InitPlans they read are that level's, attached to its top by SS_attach_initplans.)
        let minmax = s.id >= crate::sql::paths::PLANNER_SUBQUERY_BASE && s.params.is_empty();
        let mut node = match &test {
            Some((input, _)) => self.level_in(input, minmax),
            None => self.level_in(&s.plan, minmax),
        };
        if s.kind == SubqueryKind::Exists {
            // simplify_EXISTS_query: the subquery returns no columns.
            node.output.clear();
            node.width = 0;
        }
        self.depth -= 1;
        if framed {
            self.outer.remove(0);
            self.outer_depth.remove(0);
        }
        self.pending = saved;
        let id = self.next_plan_id(Some(s), block_start, false);
        let correlated = match s.test_op {
            Some(n) => n > 0,
            None => level_param,
        };
        let init = !correlated && !matches!(s.kind, SubqueryKind::Any | SubqueryKind::All);
        let hashed = !correlated
            && s.kind == SubqueryKind::Any
            && match &test {
                // (Hashable when each comparison is an `=` of the outer values with the
                // subquery's column, in that order: Postgres can't hash one the other way
                // round.)
                Some((_, t)) => {
                    let mut parts = Vec::new();
                    conjuncts(t, &mut parts);
                    !parts.is_empty()
                        && parts.iter().all(|e| match e {
                            Expr::Call(d, args, _) if d.name == "=" && args.len() == 2 => {
                                let (a, b) = (&args[0], &args[1]);
                                a.columns().is_empty()
                                    && a.has_outer_refs()
                                    && !b.has_outer_refs()
                                    && b.columns() == vec![0]
                                    && self.ctx.catalog().type_hashable(&a.ty())
                            }
                            _ => false,
                        })
                }
                // (hash_ok_operator: each `=` hashes for its type; an anonymous record's
                // equality doesn't, bug #17363.)
                None => {
                    let cat = self.ctx.catalog();
                    s.ops.iter().all(|o| o.name == "=")
                        && s.left.iter().all(|l| cat.type_hashable(&l.ty()))
                }
            };
        if !correlated
            && !init
            && !hashed
            && !matches!(
                node.node_type,
                "Materialize" | "Function Scan" | "CTE Scan" | "WorkTable Scan" | "Sort"
            )
        {
            // Rescanned for every outer row, so the planner keeps its output.
            let mut m = ExNode {
                key: node.key,
                rows: node.rows,
                width: node.width,
                startup_cost: node.startup_cost,
                total_cost: node.total_cost + node.rows * 0.0125,
                ..ExNode::new("Materialize")
            };
            m.initplans = std::mem::take(&mut node.initplans);
            if self.o.verbose {
                // Its target list refers to its input's: a value the input computes (not
                // one of its columns) parenthesized, as get_variable shows it.
                m.output = node.output.iter().map(|o| refer_to(o)).collect();
            }
            m.children.push(node.child("Outer"));
            node = m;
        }
        // A correlated EXISTS that can become a hashed ANY gets both plans; setrefs keeps
        // the one cheaper for the times it runs (fix_alternative_subplan), the hashed one
        // numbered after the plain one.
        let exists_any = None;
        if s.kind == SubqueryKind::Exists
            && correlated
            && let Some((mut hplan, keys)) = exists_hashed_from(&s.plan, true)
        {
            // (Planned for itself: its scan reads the whole table.)
            crate::sql::paths::plan_query(self.ctx, &mut hplan);
            // (Where how often it runs isn't known, the plain one.)
            let n_exec = self.num_exec.unwrap_or(0.0);
            let per_plain = (node.total_cost - node.startup_cost) / costs::clamp_rows(node.rows)
                + node.startup_cost;
            let hplan = Arc::new(hplan);
            // Its scan is the plain plan's under the same name, unless both are shown: its
            // relation is a range table entry of its own then, named after the plain one's
            // (`fresh_hashed`, from a build before).
            if let (Plan::Project { input, .. }, Some(orig)) = (&*hplan, scan_below(&s.plan))
                && let Some(name) = self.refnames.get(&addr(orig)).cloned()
            {
                let name = match orig {
                    Plan::Scan(sc) if self.fresh_hashed.contains(&s.id) => {
                        let base = sc.alias.clone().unwrap_or_else(|| sc.table.name.clone());
                        let next = self
                            .refnames
                            .values()
                            .filter_map(|n| {
                                if *n == base {
                                    return Some(0);
                                }
                                n.strip_prefix(base.as_str())?.strip_prefix('_')?.parse::<usize>().ok()
                            })
                            .max()
                            .map_or(0, |m| m + 1);
                        if next == 0 { base } else { crate::catalog::suffixed_name(&base, &format!("_{next}")) }
                    }
                    _ => name,
                };
                self.refnames.insert(addr(input), name);
            }
            // (An inheritance tree's copy is range table entries of its own: the parent's
            // next number, then its members'.)
            if let Plan::Append { inputs, inh: Some(base), .. } = &*hplan {
                let next = self
                    .refnames
                    .values()
                    .filter_map(|n| n.strip_prefix(base.as_str())?.strip_prefix('_')?.parse::<usize>().ok())
                    .max()
                    .unwrap_or(0)
                    + 1;
                for (k, m) in inputs.iter().enumerate() {
                    let mut q: &Plan = m;
                    while let Plan::Project { input, .. } = q {
                        q = input;
                    }
                    self.refnames.insert(addr(q), format!("{base}_{}", next + 1 + k));
                }
            }
            let saved = std::mem::take(&mut self.pending);
            let hnode = self.level(&hplan);
            self.pending = saved;
            let startup_h = hnode.total_cost + costs::CPU_OPERATOR_COST * hnode.rows;
            let per_h = costs::CPU_OPERATOR_COST * keys.len() as f64;
            // (The hashed alternative's number is taken whichever runs.)
            let hashed_id = self.next_plan_id(None, self.events.len(), true);
            let left: Vec<Expr> = keys
                .iter()
                .map(|(o, _)| {
                    o.clone()
                        .transform(&mut |x| {
                            Ok(match x {
                                Expr::Outer(1, i, t) => {
                                    s.params.get(i).cloned().unwrap_or(Expr::Outer(1, i, t))
                                }
                                x => x,
                            })
                        })
                        .unwrap_or_else(|_| o.clone())
                })
                .collect();
            let label = |id: usize, any: Option<(Vec<Expr>, Vec<Arc<crate::sql::func::FuncDef>>)>| SubplanLabel {
                name: format!("SubPlan {id}"),
                init: false,
                hashed: any.is_some(),
                exists_any: any,
            };
            let mut pnode = node.clone();
            pnode.relationship = Some("SubPlan");
            pnode.subplan_name = Some(format!("SubPlan {id}"));
            let mut hnode = hnode;
            hnode.relationship = Some("SubPlan");
            hnode.subplan_name = Some(format!("SubPlan {hashed_id}"));
            let mut alt = AltSubPlan {
                plain: (label(id, None), pnode),
                hashed: (label(hashed_id, Some((left, keys.iter().map(|k| k.1.clone()).collect()))), hnode),
                per_plain,
                startup_h,
                per_h,
                shown: [false; 2],
            };
            let i = alt.pick(n_exec);
            alt.shown[i] = true;
            let (l, n) = alt.get(i).clone();
            self.alts.insert(s.id, alt);
            self.labels.insert(s.id, l);
            self.pending.push(n);
            return;
        }
        let name = format!("{} {id}", if init { "InitPlan" } else { "SubPlan" });
        self.labels.insert(
            s.id,
            SubplanLabel {
                name: name.clone(),
                init,
                hashed,
                exists_any,
            },
        );
        node.relationship = Some(if init { "InitPlan" } else { "SubPlan" });
        node.subplan_name = Some(name);
        node.minmax_init = init && s.id >= crate::sql::paths::PLANNER_SUBQUERY_BASE;
        node.exists_init = init && s.kind == SubqueryKind::Exists;
        if init {
            self.inits.last_mut().expect("query level").push(node);
        } else {
            self.pending.push(node);
        }
    }

    /// A query level (the statement, a CTE or a subquery): its root carries its InitPlans.
    fn level(&mut self, p: &Arc<Plan>) -> ExNode {
        self.level_in(p, false)
    }

    /// `level`; `shared`: its InitPlans are the enclosing level's.
    fn level_in(&mut self, p: &Arc<Plan>, shared: bool) -> ExNode {
        if !shared {
            self.inits.push(Vec::new());
        }
        let saved_depth = std::mem::take(&mut self.proj_depth);
        self.level_tops.insert(addr(p));
        self.group_tops.insert(grouping_top(p));
        self.push_level(false);
        let mut node = self.node(p).node;
        self.pop_level();
        self.proj_depth = saved_depth;
        self.settle(&mut node);
        if self.o.verbose {
            carry_group_junk(&mut node);
            let mut whole = Vec::new();
            whole_row_outputs(&mut node, &mut whole);
            drop_whole_only_columns(&mut node, whole);
            tlist_layout(&mut node);
            // (A LockRows at the level's top returns its rows' marks too, resjunk, and so
            // do the Limits over it, which return their input's target list.)
            lock_junk(&mut node);
        }
        if shared {
            return node;
        }
        let mut inits = self.inits.pop().unwrap_or_default();
        // (SS_attach_initplans: in the order they were made, once renumbered.)
        if self.renumber.is_some() {
            inits.sort_by_key(|i| {
                i.subplan_name
                    .as_deref()
                    .and_then(|n| n.strip_prefix("InitPlan "))
                    .and_then(|n| n.parse::<usize>().ok())
                    .unwrap_or(0)
            });
        }
        // SS_charge_for_initplans: the level's top node pays for its InitPlans.
        let cost: f64 = inits.iter().filter(|i| !i.charged).map(|i| i.init_charge()).sum();
        node.startup_cost += cost;
        node.total_cost += cost;
        inits.append(&mut node.initplans);
        node.initplans = inits;
        node
    }

    /// materialize_finished_plan: a Materialize over the finished plan.
    fn materialize_top(&self, root: ExNode) -> ExNode {
        let mut m = ExNode::new("Materialize");
        m.rows = root.rows;
        m.width = root.width;
        m.cols = root.cols.clone();
        (m.startup_cost, m.total_cost) =
            costs::material_cost(root.startup_cost, root.total_cost, root.rows, root.width);
        if self.o.verbose {
            m.output = root.output.iter().map(|o| refer_to(o)).collect();
        }
        m.key = root.key;
        // (The statement's InitPlans hang off its top node.)
        let mut root = root;
        m.initplans = std::mem::take(&mut root.initplans);
        m.children.push(root.child("Outer"));
        m
    }

    /// debug_parallel_query = on: a parallel-safe SELECT runs under a single-copy Gather
    /// of one worker (standard_planner), its InitPlans going with it.
    fn single_copy(&self, plan: &Arc<Plan>, root: ExNode) -> ExNode {
        let on = self
            .ctx
            .sess
            .settings
            .borrow()
            .get_raw("debug_parallel_query")
            .is_some_and(|v| matches!(v.as_str(), "on" | "true" | "1"));
        let Some(cfg) = self.par.as_ref() else {
            return root;
        };
        if !on
            || cfg.max_workers == 0
            || parallel::has_gather(&root)
            || matches!(root.node_type, "ModifyTable" | "LockRows")
            || plan.writes()
            || super::paths::parallel::plan_hazard(plan, &self.ctx.catalog()) != 's'
        {
            return root;
        }
        let mut g = ExNode::new("Gather");
        g.rows = root.rows;
        g.width = root.width;
        g.cols = root.cols.clone();
        g.startup_cost = root.startup_cost + cfg.setup_cost;
        g.total_cost = root.total_cost + cfg.setup_cost + cfg.tuple_cost * root.rows;
        g.workers = 1;
        g.details.push(Detail::Workers(1));
        g.details.push(Detail::Text("Single Copy", "true".into()));
        if self.o.verbose {
            g.output = root.output.iter().map(|o| refer_to(o)).collect();
        }
        g.key = root.key;
        let mut child = root;
        g.initplans = std::mem::take(&mut child.initplans);
        g.children.push(child.child("Outer"));
        g
    }

    /// " on <relation> <alias>" and the matching structured properties.
    fn target(
        &self,
        n: &mut ExNode,
        tag: Option<&'static str>,
        object: Option<&str>,
        schema: Option<&str>,
        refname: &str,
    ) {
        n.title.push_str(" on");
        // get_namespace_name_or_temp: the session's temporary schema is `pg_temp`.
        let schema = schema.map(|s| if s.starts_with("pg_temp_") { "pg_temp" } else { s });
        match (object, schema.filter(|_| self.o.verbose)) {
            (Some(o), Some(s)) => {
                let _ = write!(n.title, " {}.{}", quote_ident(s), quote_ident(o));
            }
            (Some(o), None) => {
                let _ = write!(n.title, " {}", quote_ident(o));
            }
            _ => {}
        }
        if object != Some(refname) {
            let _ = write!(n.title, " {}", quote_ident(refname));
        }
        if let (Some(tag), Some(o)) = (tag, object) {
            n.target.push((tag, o.to_string(), false));
        }
        if let Some(s) = schema.filter(|_| self.o.verbose) {
            n.target.push(("Schema", s.to_string(), false));
        }
        n.target.push(("Alias", refname.to_string(), false));
    }

    fn refname(&self, p: &Plan) -> String {
        self.refnames.get(&addr(p)).cloned().unwrap_or_default()
    }

    /// A catalog's scan by the path the planner chose (`SysPath`): as the scan of its
    /// stand-in table shows.
    fn sys_path_scan(
        &mut self,
        p: &Arc<Plan>,
        _table: crate::catalog::system::SysTable,
        sp: &super::plan::SysPath,
    ) -> Built {
        let tmp = Arc::new(Plan::Scan(Box::new(sp.scan.clone())));
        let refname = self.refname(p);
        self.refnames.insert(addr(&tmp), refname);
        let Plan::Scan(s) = &*tmp else { unreachable!() };
        let mut b = self.scan(&tmp, s);
        self.refnames.remove(&addr(&tmp));
        b.node.key = Some(addr(p));
        b
    }

    fn node(&mut self, p: &Arc<Plan>) -> Built {
        if let Plan::Context { input, .. } = &**p {
            return self.node(input);
        }
        // (A LATERAL subquery's copies of this level's PlaceHolderVars' sub-selects, made by
        // extract_lateral_references and never run: SubPlans of this level, before the
        // subquery's own; an uncorrelated one an InitPlan of it, a correlated one shown
        // nowhere, a number all the same.)
        if let Plan::Barrier { copies, .. } = &**p
            && !copies.is_empty()
        {
            self.unused_copies(copies);
        }
        let inits_before = self.inits.last().map_or(0, |v| v.len());
        let apart = matches!(&**p, Plan::Barrier { .. });
        self.depth += apart as usize;
        if apart {
            self.push_level(true);
        }
        let mut b = self.convert(p);
        if apart {
            self.pop_level();
        }
        self.depth -= apart as usize;
        // (The MIN / MAX InitPlans the node's expressions run: their costs are its own.)
        if let Some(level) = self.inits.last_mut() {
            for i in level.iter_mut().skip(inits_before) {
                if i.minmax_init && !i.charged {
                    i.charged = true;
                    b.node.startup_cost += i.total_cost;
                    b.node.total_cost += i.total_cost;
                }
            }
        }
        // (A subquery planned apart: its InitPlans are its plan's top node's, and it
        // pays for them.)
        if matches!(&**p, Plan::Barrier { .. })
            && let Some(level) = self.inits.last_mut()
            && level.len() > inits_before
        {
            let mut moved: Vec<ExNode> = level.drain(inits_before..).collect();
            let cost: f64 = moved.iter().filter(|i| !i.charged).map(|i| i.init_charge()).sum();
            for i in &mut moved {
                i.charged = true;
            }
            b.node.startup_cost += cost;
            b.node.total_cost += cost;
            let target = if b.node.node_type == "Subquery Scan" && !b.node.children.is_empty() {
                let c = &mut b.node.children[0];
                c.startup_cost += cost;
                c.total_cost += cost;
                c
            } else {
                &mut b.node
            };
            moved.append(&mut target.initplans);
            target.initplans = moved;
        }
        b.node.subplans.append(&mut self.pending);
        if b.node.key.is_none() {
            b.node.key = Some(addr(p));
        }
        // (The ordered partial paths go through projections of a scan only.)
        if !matches!(&**p, Plan::Scan(_) | Plan::Project { .. }) || b.node.partial.is_none() {
            b.node.partials.clear();
        }
        // (Its partial form counts as it: the same node runs.)
        let k = b.node.key;
        for pt in b.node.partials_mut() {
            if pt.key.is_none() {
                pt.key = k;
            }
        }
        // (A whole-row-marked source (ROW_MARK_COPY): set_rel_width counts its whole-row Var,
        // a tuple header's 24 bytes plus the widths it knows of its columns (a subquery's
        // plain column outputs, set_subquery_size_estimates; the columns read), on top of
        // the columns read.)
        if self.copy_marks.get(&addr(p)) == Some(&None)
            && matches!(
                b.node.node_type,
                "Subquery Scan" | "Values Scan" | "Function Scan" | "CTE Scan" | "Table Function Scan"
            )
        {
            let needed = self.needed.get(&addr(p));
            let read = |i: usize| needed.is_none_or(|m| m.get(i).copied().unwrap_or(true));
            let sub = b.node.node_type == "Subquery Scan";
            let (reads, known) = if b.node.cols.len() >= b.exported.len() {
                let cols = &b.node.cols[..b.exported.len()];
                (
                    cols.iter().enumerate().filter(|(i, _)| read(*i)).map(|(_, c)| c.width).sum::<u32>(),
                    cols.iter()
                        .enumerate()
                        .filter(|(i, c)| read(*i) || (sub && c.var.is_some()))
                        .map(|(_, c)| c.width)
                        .sum::<u32>(),
                )
            } else {
                (b.node.width, b.node.width)
            };
            let whole = 24 + known;
            self.copy_marks.insert(addr(p), Some(whole));
            b.node.width = reads + whole;
        }
        // (A join whose columns nothing above reads is 0 wide; so is a scan of them.)
        let unread_scan = b.node.is_scan() && self.needed.get(&addr(p)).is_some_and(|m| m.iter().all(|x| !x));
        if b.node.width == 0 && !b.node.is_join() && !unread_scan {
            b.node.width = b.exported.len() as u32 * 4;
        }
        if self.o.verbose && b.node.output.is_empty() && b.node.node_type != "ModifyTable" {
            // A join returns just the columns needed above it, and so does an index-only
            // scan (no physical target list: its row is the index's).
            // (So does a dummy relation's Result.)
            let dummy = b.node.node_type == "Result" && b.node.children.is_empty() && matches!(&**p, Plan::Empty { .. });
            // (Or a member's column mapping over its scan.)
            let member_addr = match &**p {
                Plan::Project { input, .. } if matches!(&**input, Plan::Scan(_)) => addr(input),
                _ => addr(p),
            };
            let member = b.node.is_scan() && INH_SCANS.with(|m| m.borrow().contains(&member_addr));
            // (A Sort over a join returns what the join does: the columns needed above.)
            let sorted_join = matches!(b.node.node_type, "Sort" | "Incremental Sort")
                && b.node.children.first().is_some_and(|c| c.is_join());
            // (A ProjectSet under an aggregation returns the group input target: what the
            // aggregation reads, and its calls.)
            let srf_target = b.node.node_type == "ProjectSet" && self.agg_input == Some(addr(p));
            // (A row-marked scan returns its ctid: no physical target list either.)
            // (Nor does a scan whose system columns are read, an UPDATE / DELETE target's
            // ctid above a join: use_physical_tlist refuses.)
            let sys_read = |s: &ScanPlan| {
                self.needed
                    .get(&addr(p))
                    .is_some_and(|m| m.iter().skip(s.table.columns.len()).any(|x| *x))
            };
            let marked = b.node.is_scan()
                && (self.rowmarks.contains(&addr(p))
                    || matches!(&**p, Plan::Scan(s) if s.with_key && (UNDER_LOCK_ROWS.with(|u| u.get() > 0) || sys_read(s))));
            // (Nor one returning the system columns it reads.)
            let sys_cols = b.node.is_scan() && matches!(&**p, Plan::Scan(s) if s.sys && sys_read(s));
            // (An inheritance tree's Append returns its appendrel's reltarget: the parent's
            // columns needed above, set_append_rel_size's translation of them.)
            let appendrel = b.node.node_type == "Append" && matches!(&**p, Plan::Append { inh: Some(_), .. });
            // (So does a Sort over it.)
            let sorted_append = matches!(b.node.node_type, "Sort" | "Incremental Sort")
                && matches!(&**p, Plan::Sort { input, .. } if matches!(&**input, Plan::Append { inh: Some(_), .. }))
                && b.node.children.first().is_some_and(|c| c.node_type == "Append" && c.children.iter().all(ExNode::is_scan));
            let mut mask = (b.node.is_join() || appendrel || sorted_append || b.node.node_type == "Index Only Scan" || b.node.node_type == "WindowAgg" || dummy || member || sorted_join || srf_target || marked || sys_cols)
                .then(|| self.needed.get(&addr(p)).cloned())
                .flatten();
            if srf_target
                && let (Some(m), Plan::ProjectSet { input, exprs, .. }) = (&mut mask, &**p)
            {
                let iw = crate::sql::binder::plan_width(input);
                for (i, e) in exprs.iter().enumerate() {
                    if (e.columns().iter().any(|&c| c >= iw) || !matches!(e, Expr::Col(..)))
                        && let Some(x) = m.get_mut(i)
                    {
                        *x = true;
                    }
                }
            }
            // (A join's in its relation's target list order, JoinMeta::tlist.)
            let order: Vec<usize> = match &**p {
                Plan::Join { meta, .. } if b.node.is_join() && meta.tlist.len() == b.internal.len() => meta.tlist.clone(),
                _ => (0..b.internal.len()).collect(),
            };
            b.node.output = order
                .into_iter()
                .filter(|i| mask.as_ref().is_none_or(|m| m.get(*i).copied().unwrap_or(true)))
                .map(|i| self.pick(if appendrel { b.exported.get(i).unwrap_or(&b.internal[i]) } else { &b.internal[i] }, Cx::Output))
                .collect();
            let out = b.node.output.clone();
            for pt in b.node.partials_mut() {
                if pt.output.is_empty() {
                    pt.output = out.clone();
                }
            }
        }
        // (A node above the one computing its level's target list returns that list: the
        // resjunk group keys too.)
        if self.o.verbose
            && let Some(fin) = self.gtl.chain.get(&addr(p))
            && let Some(carried) = self.carried.get(fin)
            && !b.node.output.is_empty()
        {
            for o in carried {
                let o = refer_to(o);
                if !b.node.output.contains(&o) {
                    b.node.output.push(o);
                }
            }
        }
        // (A dummy relation's Result: its Vars, then the PlaceHolderVars it evaluates, as
        // its reltarget lists them.)
        if self.o.verbose
            && let Plan::Project { input, exprs } = &**p
            && matches!(&**input, Plan::Empty { .. })
            && b.node.node_type == "Result"
        {
            let mask = self.needed.get(&addr(p));
            let shown: Vec<&Expr> = exprs
                .iter()
                .enumerate()
                .filter(|(i, _)| b.node.output.len() == exprs.len() || mask.is_none_or(|m| m.get(*i).copied().unwrap_or(true)))
                .map(|(_, e)| e)
                .collect();
            let (vars, rest): (Vec<_>, Vec<_>) = if shown.len() == b.node.output.len() {
                b.node.output.drain(..).zip(shown).partition(|(_, e)| matches!(e, Expr::Col(..)))
            } else {
                (Vec::new(), b.node.output.drain(..).map(|o| (o, &exprs[0])).collect())
            };
            b.node.output = vars.into_iter().chain(rest).map(|(o, _)| o).collect();
        }
        b
    }

    fn result(&self, rows: f64) -> ExNode {
        ExNode {
            rows,
            total_cost: 0.01,
            ..ExNode::new("Result")
        }
    }

    fn convert(&mut self, p: &Arc<Plan>) -> Built {
        let key = addr(p);
        match &**p {
            Plan::Context { input, .. } => self.convert(input),
            // A subquery Postgres plans as a level of its own: its Subquery Scan shows
            // unless trivial (setrefs' trivial_subqueryscan): it returns columns its plan
            // computes but doesn't return (resjunk sort or group keys), or the node above
            // wants fewer columns than the subquery has (no physical target list then).
            // (A security barrier's too when a data-modifying statement's row mark keeps it.)
            Plan::Barrier { input, alias, cols, secure, sorted, path, .. }
                if !alias.is_empty() && (!*secure || self.shown.get(&key) == Some(&true)) =>
            {
                let junk = barrier_junk(input, *sorted);
                let Some(&demand) = self.shown.get(&key) else {
                    // (Its plan returns its resjunk columns as well, past the ones the
                    // query above reads.)
                    if let Some((pin, exprs)) = junk {
                        let mut c = self.node(pin);
                        if let Some(sp) = path {
                            let map: Vec<usize> =
                                exprs.iter().map(|e| if let Expr::Col(j, _) = e { *j } else { usize::MAX }).collect();
                            self.take_sub_path(&mut c, &map, sp);
                        }
                        let pick = |v: &Vec<Name>| -> Vec<Name> {
                            exprs
                                .iter()
                                .map(|e| match e {
                                    Expr::Col(j, _) => v.get(*j).cloned().unwrap_or_else(|| Name::same("?")),
                                    _ => Name::same("?"),
                                })
                                .collect()
                        };
                        let (internal, exported) = (pick(&c.internal), pick(&c.exported));
                        let mut node = c.node;
                        if node.cols.len() == c.exported.len() {
                            node.cols = exprs
                                .iter()
                                .map(|e| match e {
                                    Expr::Col(j, _) => node.cols.get(*j).cloned().unwrap_or_default(),
                                    _ => ColInfo::default(),
                                })
                                .collect();
                            subquery_var_stats(input, &mut node.cols, node.rows);
                        }
                        return Built { node, internal, exported };
                    }
                    // (Its scan trivial, the order it offers is still the subquery's: its
                    // ORDER BY's keys, truncate_useless_pathkeys.)
                    let mut b = self.convert(input);
                    if let Some(sp) = path {
                        let map: Vec<usize> = (0..b.exported.len()).collect();
                        self.take_sub_path(&mut b, &map, sp);
                    }
                    if let Some(n) = sorted {
                        b.node.order.truncate(*n);
                    }
                    if b.node.cols.len() == cols.len() {
                        subquery_var_stats(input, &mut b.node.cols, b.node.rows);
                    }
                    return b;
                };
                let needed = self.needed.get(&key).cloned();
                // (Its reference name: the alias made unique, `*SELECT* 1_1` under a UNION
                // ALL whose parent entry is `*SELECT* 1`.)
                let alias = &self.refnames.get(&key).cloned().unwrap_or_else(|| alias.clone());
                let mut c = match junk {
                    Some((pin, _)) => self.node(pin),
                    None => self.node(input),
                };
                // (Its level's LockRows returns its rows' marks, and so do the Limits over it.)
                if self.o.verbose {
                    lock_junk(&mut c.node);
                }
                let map: Vec<usize> = match junk {
                    Some((_, exprs)) => exprs
                        .iter()
                        .map(|e| match e {
                            Expr::Col(j, _) => *j,
                            _ => 0,
                        })
                        .collect(),
                    None => (0..c.exported.len()).collect(),
                };
                if let Some(sp) = path {
                    self.take_sub_path(&mut c, &map, sp);
                }
                let names: Vec<Name> = cols.iter().map(|col| Name::col(alias, col)).collect();
                let order = self.subquery_order(&c, &map, &names, *sorted, needed.as_ref());
                // (The subquery's other path, in another order: its scan over that one, for
                // the node above to choose from.)
                let alt_order = c.node.alt.as_ref().map(|a| {
                    let tmp = Built {
                        node: (**a).clone(),
                        internal: Vec::new(),
                        exported: c.exported.clone(),
                    };
                    self.subquery_order(&tmp, &map, &names, *sorted, needed.as_ref())
                });
                let alt = c.node.alt.take();
                let child_cols = c.node.cols.clone();
                let info = Arc::new((alias.clone(), cols.clone()));
                let saved = self.sub_alias.replace(info);
                let mut b = self.subquery_scan(c, key);
                self.sub_alias = saved;
                // (Its whole row is a Var of its own above: `s.*`.)
                self.whole_vars.insert(quote_ident(alias), cols.len());
                if let (Some(a), Some(ao)) = (alt, alt_order) {
                    let mut n = b.node.clone();
                    n.startup_cost = a.startup_cost;
                    n.total_cost = a.total_cost + a.rows * costs::CPU_TUPLE_COST;
                    n.order = ao;
                    n.children = vec![a.child("Subquery")];
                    b.node.alt = Some(Box::new(n));
                }
                // (Its columns: the subquery's outputs, not the row marks its plan returns
                // after them.)
                if child_cols.len() >= map.len() {
                    b.node.cols = map
                        .iter()
                        .take(names.len())
                        .map(|&j| child_cols.get(j).cloned().unwrap_or_default())
                        .collect();
                    subquery_var_stats(input, &mut b.node.cols, b.node.rows);
                    if let Some(a) = &mut b.node.alt {
                        a.cols = b.node.cols.clone();
                    }
                }
                b.node.order = order;
                // (Over a LockRows, whose target list has the row marks too: its own
                // reltarget's columns.)
                if b.node.children.first().is_some_and(|c| c.node_type == "LockRows") && b.node.cols.len() == names.len() {
                    b.node.width = b
                        .node
                        .cols
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| needed.as_ref().is_none_or(|m| m.get(*i).copied().unwrap_or(true)))
                        .map(|(_, c)| c.width)
                        .sum();
                }
                if self.o.verbose {
                    b.node.output = names
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| !demand || needed.as_ref().is_none_or(|m| m.get(*i).copied().unwrap_or(true)))
                        .map(|(_, n)| self.pick(n, Cx::Output))
                        .collect();
                }
                b.internal = names.clone();
                b.exported = names;
                b
            }
            // (Without a filter above, its Subquery Scan is trivial.)
            Plan::Barrier { input, .. } => self.convert(input),
            Plan::One => Built::passthrough(self.result(1.0), Vec::new()),
            Plan::Empty { ncols, cols, info } => {
                let mut n = ExNode {
                    total_cost: 0.0,
                    rows: 0.0,
                    ..self.result(0.0)
                };
                n.details
                    .push(Detail::Text("One-Time Filter", "false".into()));
                // (Qualified by the relation each was read from, which has a name only when
                // the plan scans no relation at all: set_rtable_names names just the
                // relations scanned, or every one when none is. With several, the range
                // table has several entries: qualified.)
                let named = self.refnames.is_empty();
                // (So has a MERGE's empty partitioned target: the statement names it.)
                let target = self.dummy_target.clone();
                let rel = |i: usize| {
                    info.as_ref()
                        .and_then(|d| d.rels.get(i).cloned().flatten())
                        .filter(|r| named || target.as_deref() == Some(r.as_str()))
                };
                if let Some(d) = info
                    && let Some(c) = cols
                {
                    for (x, r) in c.iter().zip(&d.rels) {
                        if let Some(r) = r
                            && d.unplanned.iter().any(|u| u.0 == *r)
                        {
                            self.unplanned_cols.insert(format!("{}.{}", quote_ident(r), x));
                        }
                    }
                }
                let names = match cols {
                    Some(c) if c.len() == *ncols => {
                        let rels: Vec<Option<String>> = (0..c.len()).map(rel).collect();
                        let several = rels.iter().flatten().any(|r| rels.iter().flatten().any(|q| q != r));
                        c.iter()
                            .zip(rels)
                            .map(|(x, r)| match r {
                                Some(r) if several => Name::same(format!("{}.{}", quote_ident(&r), x)),
                                Some(r) => Name {
                                    plain: x.clone(),
                                    full: format!("{}.{}", quote_ident(&r), x),
                                },
                                None => Name::same(x.clone()),
                            })
                            .collect()
                    }
                    _ => vec![Name::same("NULL"); *ncols],
                };
                Built::passthrough(n, names)
            }
            Plan::Values { rows, .. } if rows.len() == 1 => {
                let internal: Vec<Name> = rows[0].iter().map(|e| self.name(e, &[], &[])).collect();
                let exported = internal.iter().map(Name::wrapped).collect();
                let mut n = self.result(1.0);
                n.width = rows[0].iter().map(|e| type_width(&e.ty())).sum();
                Built {
                    node: n,
                    internal,
                    exported,
                }
            }
            Plan::Values { rows, ncols, .. } => {
                let refname = self.refname(p);
                let mut n = ExNode::new("Values Scan");
                self.target(&mut n, None, None, None, &refname);
                // (The SubPlans its lists compute are its.)
                for e in rows.iter().flatten() {
                    self.register(e, &[]);
                }
                n.rows = rows.len() as f64;
                n.total_cost = rows.len() as f64 * 0.0125;
                n.cols = rows[0]
                    .iter()
                    .map(|e| ColInfo {
                        width: type_width(&e.ty()),
                        var: None,
                        distinct: Some(rows.len() as f64),
                    })
                    .collect();
                n.width = n.cols.iter().map(|c| c.width).sum();
                let names = (1..=*ncols)
                    .map(|i| Name::col(&refname, &format!("column{i}")))
                    .collect();
                Built::passthrough(n, names)
            }
            Plan::Scan(s) => self.scan(p, s),
            Plan::SysScan { table, filter, path, .. } => {
                if let Some(sp) = path {
                    return self.sys_path_scan(p, *table, sp);
                }
                let refname = self.refname(p);
                let mut n = ExNode::new("Seq Scan");
                let schema = if table.schema() == crate::catalog::PG_CATALOG_OID {
                    "pg_catalog"
                } else {
                    "information_schema"
                };
                self.target(
                    &mut n,
                    Some("Relation Name"),
                    Some(table.name()),
                    Some(schema),
                    &refname,
                );
                let cols = table.columns();
                let names: Vec<Name> = cols.iter().map(|(c, _)| Name::col(&refname, c)).collect();
                n.width = cols.iter().map(|(_, t)| type_width(t)).sum();
                // cost_seqscan over the catalog's rows (its reltuples) and the pages they
                // fill.
                let tuples = sys_tuples(*table, self.ctx);
                let pages = ((tuples * (n.width as f64 + 28.0)) / 8192.0).ceil().max(1.0);
                n.rows = tuples;
                n.total_cost = pages + tuples * costs::CPU_TUPLE_COST;
                if let Some(f) = filter {
                    // A catalog index its conditions can use (as the planner would pick).
                    match sys_index_scan(table.name(), f, &cols) {
                        Some((index, conds, rest)) => {
                            n.node_type = "Index Scan";
                            n.title = format!("Index Scan using {index} on {}", n.title.trim_start_matches("Seq Scan on "));
                            n.target.insert(0, ("Index Name", index.clone(), false));
                            let texts: Vec<String> = conds
                                .iter()
                                .map(|c| match c {
                                    SysCond::Expr(e) => self.show(e, &names, Cx::Scan),
                                    SysCond::Bound(col, op, v) => format!(
                                        "({} {op} {})",
                                        self.pick(&names[*col], Cx::Scan),
                                        quote_literal(v) + "::text"
                                    ),
                                })
                                .collect();
                            n.details
                                .push(Detail::Text("Index Cond", and_text(texts).unwrap_or_default()));
                            if !rest.is_empty() {
                                let texts = rest
                                    .iter()
                                    .map(|e| self.show(e, &names, Cx::Scan))
                                    .collect();
                                n.details
                                    .push(Detail::Text("Filter", and_text(texts).unwrap_or_default()));
                            }
                            n.rows = 1.0;
                        }
                        None => {
                            let t = self.show(f, &names, Cx::Scan);
                            n.details.push(Detail::Text("Filter", t));
                            let cat = self.ctx.catalog();
                            let vars = |_: usize| None;
                            let est = costs::Est {
                                cat: &cat,
                                fmt: self.ctx.fmt(),
                                vars: &vars,
                                exprs: &[],
                                endpoint: None,
                                distinct: None,
                                user: None,
                                same_rel: false,
                            };
                            let mut parts = Vec::new();
                            conjuncts(f, &mut parts);
                            n.total_cost += tuples * parts.iter().map(costs::qual_cost).sum::<f64>();
                            n.rows = costs::clamp_rows(tuples * est.clauses(&parts));
                        }
                    }
                }
                Built::passthrough(n, names)
            }
            Plan::FunctionScan {
                calls, alias, cols, ..
            } if calls.len() == 1 && crate::sql::func::xmltable::is_xmltable(&calls[0].func) => {
                let refname = self
                    .refnames
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| alias.clone());
                let mut n = ExNode::new("Table Function Scan");
                n.title = if refname == "xmltable" {
                    "Table Function Scan on \"xmltable\"".into()
                } else {
                    format!(
                        "Table Function Scan on \"xmltable\" {}",
                        quote_ident(&refname)
                    )
                };
                n.target
                    .push(("Table Function Name", "xmltable".into(), false));
                n.target.push(("Alias", refname.clone(), false));
                let names: Vec<Name> = cols.iter().map(|c| Name::col(&refname, c)).collect();
                if self.o.verbose {
                    let mut show = |e: &Expr| Some(self.show(e, &[], Cx::Scan));
                    let t = crate::sql::func::xmltable::xml_table_text(&calls[0].args, &mut show)
                        .unwrap_or_default();
                    n.details.push(Detail::Text("Table Function Call", t));
                }
                n.rows = 100.0;
                n.total_cost = 1.0;
                n.width = calls.iter().map(|c| type_width(&c.ty)).sum();
                Built::passthrough(n, names)
            }
            Plan::FunctionScan {
                calls, alias, cols, ..
            } if calls.len() == 1
                && calls[0].func.name == "JSON_TABLE"
                && calls[0].func.oid == 0 =>
            {
                let refname = self
                    .refnames
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| alias.clone());
                let mut n = ExNode::new("Table Function Scan");
                n.title = if refname == "json_table" {
                    "Table Function Scan on \"json_table\"".into()
                } else {
                    format!(
                        "Table Function Scan on \"json_table\" {}",
                        quote_ident(&refname)
                    )
                };
                n.target
                    .push(("Table Function Name", "json_table".into(), false));
                n.target.push(("Alias", refname.clone(), false));
                let names: Vec<Name> = cols.iter().map(|c| Name::col(&refname, c)).collect();
                if self.o.verbose {
                    let mut show = |e: &Expr| Some(self.show(e, &[], Cx::Scan));
                    let t = crate::sql::ruleutils::json_table_text(
                        &calls[0],
                        &mut show,
                        &|t| t.display(),
                        None,
                        true,
                    )
                    .unwrap_or_default();
                    n.details.push(Detail::Text("Table Function Call", t));
                }
                // (cost_tablefuncscan: the TableFunc's expressions once — a JSON_TABLE's
                // row pattern and each column's path, ordinality columns none — then a
                // tuple's CPU cost for each of its 100 rows.)
                fn path_cols(p: &crate::sql::func::sqljson::PathSpec) -> (usize, u32) {
                    let mut ops = p.columns.iter().filter(|c| c.query.is_some()).count();
                    let mut width: u32 = p.columns.iter().map(|c| type_width(&c.ty)).sum();
                    for n in &p.nested {
                        let (o, w) = path_cols(n);
                        ops += o;
                        width += w;
                    }
                    (ops, width)
                }
                let (ops, width) = match calls[0].args.get(1) {
                    Some(Expr::Const(Value::Text(spec), _)) => {
                        serde_json::from_str::<crate::sql::func::sqljson::TableSpec>(spec)
                            .map_or((1, 32), |t| path_cols(&t.root))
                    }
                    _ => (1, 32),
                };
                n.rows = 100.0;
                n.startup_cost = costs::CPU_OPERATOR_COST * (ops + 1) as f64;
                n.total_cost = n.startup_cost + costs::CPU_TUPLE_COST * 100.0;
                n.width = width;
                Built::passthrough(n, names)
            }
            Plan::FunctionScan {
                calls, alias, cols, ..
            } => {
                let refname = self
                    .refnames
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| alias.clone());
                let mut n = ExNode::new("Function Scan");
                let object = (calls.len() == 1).then(|| calls[0].func.name);
                // (A CREATE FUNCTION function: its own schema.)
                let schema = {
                    let cat = self.ctx.catalog();
                    calls
                        .first()
                        .filter(|c| c.func.oid != 0)
                        .and_then(|c| cat.functions.get(&c.func.oid))
                        .map_or("pg_catalog".to_string(), |f| cat.schema_name(f.schema).to_string())
                };
                self.target(
                    &mut n,
                    object.map(|_| "Function Name"),
                    object,
                    Some(&schema),
                    &refname,
                );
                let names: Vec<Name> = cols.iter().map(|c| Name::col(&refname, c)).collect();
                // (A row of several columns: its whole row is the function RTE's Var.)
                if cols.len() > 1 {
                    self.whole_vars.insert(quote_ident(&refname), cols.len());
                }
                // (ROWS FROM's calls in one list.)
                if self.o.verbose && !calls.is_empty() {
                    let t: Vec<String> = calls
                        .iter()
                        .map(|c| {
                            let call = Expr::Call(c.func.clone(), c.args.clone(), c.ty.clone());
                            self.show(&call, &[], Cx::Scan)
                        })
                        .collect();
                    n.details.push(Detail::Text("Function Call", t.join(", ")));
                }
                // cost_functionscan: the calls once, then a tuple per row. (The calls'
                // evaluation, their arguments' included, cost_qual_eval_node over
                // rte->functions: a LATERAL call's `t.x::integer % 10` too.)
                n.rows = calls
                    .iter()
                    .map(|c| costs::function_rows(c.func.name, &c.args, c.func.srf.is_some()))
                    .fold(1.0, f64::max);
                n.startup_cost = calls
                    .iter()
                    .map(|c| costs::CPU_OPERATOR_COST + c.args.iter().map(costs::qual_cost).sum::<f64>())
                    .sum();
                n.total_cost = n.startup_cost + n.rows * costs::CPU_TUPLE_COST;
                n.width = calls.iter().map(|c| type_width(&c.ty)).sum();
                // (Only the columns used above: its reltarget.)
                if calls.len() == cols.len()
                    && let Some(m) = self.needed.get(&key)
                    && m.len() == calls.len()
                {
                    n.width = calls
                        .iter()
                        .zip(m)
                        .filter(|(_, x)| **x)
                        .map(|(c, _)| type_width(&c.ty))
                        .sum();
                }
                // (A worker can run it when its calls are parallel safe:
                // set_rel_consider_parallel.)
                n.par_safe = {
                    let cat = self.ctx.catalog();
                    calls.iter().all(|c| {
                        let call = Expr::Call(c.func.clone(), c.args.clone(), c.ty.clone());
                        super::paths::parallel::hazard(&call, &cat) == 's'
                    })
                };
                // get_variable_numdistinct without statistics: a relation of fewer tuples
                // than the default has that many values.
                if calls.len() == cols.len() {
                    let distinct = (n.rows < costs::DEFAULT_NUM_DISTINCT).then_some(costs::clamp_rows(n.rows));
                    n.cols = calls
                        .iter()
                        .map(|c| ColInfo {
                            width: type_width(&c.ty),
                            var: None,
                            distinct,
                        })
                        .collect();
                }
                Built::passthrough(n, names)
            }
            Plan::PlCall { func, ncols, .. } => {
                let refname = self.refname(p);
                let mut n = ExNode::new("Function Scan");
                let schema = self.ctx.catalog().schema_name(func.schema).to_string();
                self.target(
                    &mut n,
                    Some("Function Name"),
                    Some(&func.name),
                    Some(&schema),
                    &refname,
                );
                let names = if func.out_cols.is_empty() {
                    vec![Name::col(&refname, &refname); *ncols]
                } else {
                    func.out_cols
                        .iter()
                        .map(|(c, _)| Name::col(&refname, c))
                        .collect()
                };
                n.rows = 1000.0;
                n.total_cost = 10.25;
                Built::passthrough(n, names)
            }
            Plan::Filter { input, pred } => {
                // (A HAVING: a partitionwise aggregation checks it in each partition.)
                let saved = self.having.take();
                if matches!(&**input, Plan::Agg { .. }) {
                    self.having = Some(pred.clone());
                }
                let inits_before = self.inits.last().map_or(0, |v| v.len());
                // (Windows of a FROM subquery under it: the filter decides their scan.)
                let mut c = {
                    let over = subquery_of(input).is_some();
                    let _g = crate::scoped::set(&FILTER_OVER_WINDOW, over, "explain::FILTER_OVER_WINDOW");
                    self.node(input)
                };
                let inits_mid = self.inits.last().map_or(0, |v| v.len());
                self.having = saved;
                if c.node.pw_full {
                    return c;
                }
                // (A filter over a subquery's windows: a Subquery Scan by its alias.)
                let saved = std::mem::replace(&mut self.sub_alias, subquery_of(input));
                // (A clause on a monotonic window function: the WindowAgg's run condition.)
                let out = if let Some(rest) = self.run_conditions(input, pred, &mut c) {
                    match rest {
                        Some(r) => self.fold_filter(c, &r, key, false),
                        // (Every clause a run condition: the subquery's scan stays when it
                        // projects.)
                        None if self.sub_alias.is_some() && !self.trivial_subquery(input, key) => {
                            self.subquery_scan(c, key)
                        }
                        None => c,
                    }
                } else {
                    // (Over a set operation: its Subquery Scan checks it.)
                    let setop = matches!(subquery_input(input), Plan::SetOp { .. } | Plan::Barrier { .. });
                    // (A HAVING over MIN / MAX aggregates made InitPlans: it reads their
                    // Params, as preprocess_minmax_aggregates replaced the Aggrefs, so it
                    // is the Result's constant qual.)
                    let minmax_having = match &**input {
                        Plan::Project { input: one, exprs }
                            if matches!(&**one, Plan::One)
                                && !exprs.is_empty()
                                && exprs.iter().all(|e| {
                                    matches!(e, Expr::Subquery(s) if s.id >= crate::sql::paths::PLANNER_SUBQUERY_BASE && s.params.is_empty())
                                        || matches!(e, Expr::Const(..))
                                }) =>
                        {
                            pred.clone()
                                .transform(&mut |x| {
                                    Ok(match x {
                                        Expr::Col(i, t) => exprs.get(i).cloned().unwrap_or(Expr::Col(i, t)),
                                        x => x,
                                    })
                                })
                                .ok()
                        }
                        _ => None,
                    };
                    self.fold_filter(c, minmax_having.as_ref().unwrap_or(pred), key, setop)
                };
                // (The subquery's InitPlans are its top node's: under the Subquery Scan.)
                let mut out = out;
                if self.sub_alias.is_some()
                    && out.node.node_type == "Subquery Scan"
                    && let Some(level) = self.inits.last_mut()
                    && inits_mid > inits_before
                    && level.len() >= inits_mid
                    && let Some(child) = out.node.children.first_mut()
                {
                    let moved: Vec<ExNode> = level.drain(inits_before..inits_mid).collect();
                    child.initplans.extend(moved);
                }
                self.sub_alias = saved;
                out
            }
            // The statement's RETURNING a rule's action computes (rewriteRuleAction puts it
            // in the action's own RETURNING list): over the action's.
            Plan::Project { .. } if rule_returning(p).is_some() => {
                let (dml, list) = rule_returning(p).expect("checked");
                let _g = crate::scoped::set_ref(&RULE_RETURNING, Some(list), "explain::RULE_RETURNING");
                self.node(dml)
            }
            Plan::Project { input, exprs } => {
                let under_filter = FILTER_OVER_WINDOW.with(|f| f.replace(false));
                // (The target list over windows: the input columns it reads, in order.)
                let tl: Vec<usize> = match &**input {
                    Plan::Window { input: wi, .. } => {
                        let iw = super::binder::plan_width(wi);
                        exprs
                            .iter()
                            .filter_map(|e| match e {
                                Expr::Col(j, _) if *j < iw => Some(*j),
                                _ => None,
                            })
                            .collect()
                    }
                    _ => Vec::new(),
                };
                WINDOW_TLIST.with(|w| *w.borrow_mut() = tl);
                if WINDOW_TLIST.with_borrow(|w| !w.is_empty()) {
                    crate::scoped::checkpoint("explain::WINDOW_TLIST");
                }
                // (JSON_ARRAYAGG / JSON_OBJECTAGG ... RETURNING text: the level's top
                // projection shows the constructor, its json cast merged; below.)
                let casts: Vec<(usize, DataType)> = Vec::new();
                let stripped;
                let exprs = if casts.is_empty() {
                    exprs
                } else {
                    stripped = exprs
                        .iter()
                        .map(|e| {
                            e.clone()
                                .transform(&mut |x| {
                                    Ok(match x {
                                        Expr::Cast(x, DataType::Json | DataType::Jsonb, to, _)
                                            if matches!(&*x, Expr::Col(k, _) if casts.iter().any(|c| c.0 == *k)) =>
                                        {
                                            let Expr::Col(k, _) = *x else { unreachable!() };
                                            Expr::Col(k, to)
                                        }
                                        x => x,
                                    })
                                })
                                .unwrap_or_else(|_| e.clone())
                        })
                        .collect::<Vec<_>>();
                    &stripped
                };
                let saved_casts = std::mem::replace(&mut self.json_casts, casts);
                let top_proj = self.proj_depth == 0;
                // (A top projection only dropping the resjunk columns the level carries
                // isn't one: the one below computes the select list.)
                let junk_only = exprs.len() < super::binder::plan_width(input)
                    && exprs.iter().enumerate().all(|(i, e)| matches!(e, Expr::Col(j, _) if *j == i));
                if !junk_only {
                    self.proj_depth += 1;
                }
                // (The order wanted of its rows, over its input's.)
                let saved_hint = self.order_hint.take();
                if let Some((a, hk)) = &saved_hint
                    && *a == addr(p)
                {
                    let through: Vec<SortKey> = hk
                        .iter()
                        .map(|k| SortKey {
                            expr: k
                                .expr
                                .clone()
                                .transform(&mut |x| {
                                    Ok(match x {
                                        Expr::Col(i, t) => exprs.get(i).cloned().unwrap_or(Expr::Col(i, t)),
                                        x => x,
                                    })
                                })
                                .unwrap_or_else(|_| k.expr.clone()),
                            ..k.clone()
                        })
                        .collect();
                    self.order_hint = Some((addr(input), through));
                }
                let mut c = self.node(input);
                self.order_hint = saved_hint;
                // (A scan returning its physical target list under a selection of its
                // columns.)
                if self.o.verbose
                    && matches!(&**input, Plan::Scan(_))
                    && matches!(c.node.node_type, "Seq Scan" | "Index Scan" | "Bitmap Heap Scan")
                    && c.node.output.len() == c.internal.len()
                    && exprs.iter().all(|e| matches!(e, Expr::Col(..)))
                {
                    c.node.physical = c.node.output.clone();
                }
                if !junk_only {
                    self.proj_depth -= 1;
                }
                self.json_casts = saved_casts;
                // A partial input stays partial through a projection of what workers can
                // compute; else the Gather (or the serial input) computes it.
                let cat = self.ctx.catalog();
                let keep_partial = c.node.partial.is_some()
                    && exprs.iter().all(|e| {
                        matches!(e, Expr::Col(..))
                            || super::paths::parallel::hazard(e, &cat) == 's'
                    });
                drop(cat);
                if !keep_partial {
                    self.settle(&mut c.node);
                }
                // The statement's own target list keeps the sort columns it carries as
                // resjunk entries: the projection that drops them isn't a plan node.
                // (So does a ProjectSet's: it carries them up from below.)
                if (self.o.verbose || c.node.node_type == "ProjectSet")
                    && (addr(p) == self.top || self.level_tops.contains(&addr(p)))
                    && (matches!(c.node.node_type, "Sort" | "Incremental Sort" | "Limit" | "LockRows" | "ProjectSet")
                        || c.node.node_type == "Result" && matches!(&**input, Plan::Project { .. })
                        // (A scan read in the ORDER BY's order computing its keys.)
                        || matches!(&**input, Plan::Project { input: pi, .. } if matches!(&**pi, Plan::Scan(_))))
                    && exprs.len() < c.internal.len()
                    && exprs
                        .iter()
                        .enumerate()
                        .all(|(i, e)| matches!(e, Expr::Col(j, _) if *j == i))
                {
                    return c;
                }
                // A ProjectSet can't project: a Result over it computes what isn't its own
                // output (is_projection_capable_path).
                // (Nor can a Sort: values computed after it, make_sort_input_target's
                // postponed ones.)
                // (Nor an Append: a projection computing values over it.)
                // (A UNION ALL pulled up as an append relation too: what the query
                // computes of its columns is a Result's.)
                // (Also one over a parameterized Append a one-row join passes its values to:
                // a nested loop's PlaceHolderVar parameters, change_plan_targetlist's
                // inject_projection_plan.)
                let param_append = matches!(&**input, Plan::Join { kind: JoinKind::Inner, left, right, params, nleft: 0, cond: None, hash: None, .. }
                    if matches!(&**left, Plan::One) && !params.is_empty() && matches!(&**right, Plan::Append { .. }));
                let append_above = (self.o.verbose && matches!(&**input, Plan::Append { inh: Some(_), .. })
                    || matches!(&**input, Plan::Append { inh: None, .. })
                    || param_append)
                    && c.node.node_type == "Append"
                    && exprs.iter().any(|e| !matches!(e, Expr::Col(..)));
                // A set operation's FROM subquery (not pulled up) that the query reads
                // other than as it is: its Subquery Scan computes that
                // (trivial_subqueryscan).
                // (One shown already, the Barrier's own, takes the projection.)
                let barrier_sorted = match &**input {
                    Plan::Barrier { secure: false, sorted, .. } => *sorted,
                    _ => None,
                };
                let barrier_info = match &**input {
                    Plan::Barrier { alias, cols, secure: false, .. }
                        if !alias.is_empty()
                            && c.node.node_type != "Subquery Scan"
                            && self.plain_distinct != Some(key) =>
                    {
                        Some(Arc::new((alias.clone(), cols.clone())))
                    }
                    _ => None,
                };
                let setop_info = match subquery_input(input) {
                    Plan::SetOp { sub: Some(info), .. } => Some(info.clone()),
                    _ => None,
                };
                if let Some(info) = barrier_info.or(setop_info)
                    && info.1.len() == c.exported.len()
                    && !(exprs.len() == c.exported.len()
                        && exprs.iter().enumerate().all(|(i, e)| matches!(e, Expr::Col(j, _) if *j == i)))
                {
                    let names: Vec<Name> = info.1.iter().map(|col| Name::col(&info.0, col)).collect();
                    let map: Vec<usize> = (0..c.exported.len()).collect();
                    let bneeded = self.needed.get(&addr(input)).cloned();
                    let order = self.subquery_order(&c, &map, &names, barrier_sorted, bneeded.as_ref());
                    let saved = self.sub_alias.replace(info.clone());
                    c = self.subquery_scan(c, addr(p));
                    self.sub_alias = saved;
                    // (Its whole row is a Var of its own above: `s.*`.)
                    self.whole_vars.insert(quote_ident(&info.0), info.1.len());
                    c.node.order = order;
                    c.internal = names.clone();
                    c.exported = names;
                }
                let sort_above = matches!(&**input, Plan::Sort { .. })
                    && matches!(c.node.node_type, "Sort" | "Incremental Sort")
                    && exprs.iter().any(|e| {
                        let mut postponed = e.is_volatile();
                        e.walk(&mut |x| postponed |= matches!(x, Expr::Subquery(s) if s.call.is_none()));
                        postponed
                    });
                // (Over a Gather Merge of the sorted partial plan, values computed after the
                // sort that workers can compute are: apply_projection_to_path pushes the
                // projection under it, a Result over the Sort.)
                let into_gather = matches!(&**input, Plan::Sort { .. })
                    && c.node.node_type == "Gather Merge"
                    && c.node
                        .children
                        .first()
                        .is_some_and(|ch| matches!(ch.node_type, "Sort" | "Incremental Sort"))
                    && exprs.iter().any(|e| {
                        let mut postponed = e.is_volatile();
                        e.walk(&mut |x| postponed |= matches!(x, Expr::Subquery(s) if s.call.is_none()));
                        postponed
                    })
                    && {
                        let cat = self.ctx.catalog();
                        exprs.iter().all(|e| super::paths::parallel::hazard(e, &cat) == 's')
                    };
                // (inject_projection_plan copies the subplan's costs and width.)
                let injected = (append_above && param_append).then_some((c.node.startup_cost, c.node.total_cost, c.node.width));
                if sort_above
                    || append_above
                    || matches!(&**input, Plan::ProjectSet { .. })
                        && !(exprs.len() == c.internal.len()
                            && exprs.iter().enumerate().all(|(i, e)| matches!(e, Expr::Col(j, _) if *j == i)))
                {
                    let child = std::mem::take(&mut c.node);
                    let mut r = ExNode::new("Result");
                    r.rows = child.rows;
                    r.width = child.width;
                    r.cols = child.cols.clone();
                    r.startup_cost = child.startup_cost;
                    r.total_cost = child.total_cost;
                    r.key = child.key;
                    r.children.push(child.child("Outer"));
                    c.node = r;
                    // (It reads its input's output as a parent does.)
                    c.internal = c.exported.clone();
                }
                // (What a projection over an inheritance Append computes is over the
                // parent's columns, the appendrel's Vars.)
                if matches!(&**input, Plan::Append { inh: Some(_), .. }) && c.node.node_type == "Append" {
                    c.internal = c.exported.clone();
                }
                // (JSON_ARRAYAGG / JSON_OBJECTAGG an aggregation further down computes as
                // its aggregate: the constructor shows here, over it, its cast merged.)
                let mut jcols: Vec<Name> = c.internal.clone();
                let jexprs: Vec<Expr> = if self.json_aggs.is_empty() || !top_proj {
                    exprs.to_vec()
                } else {
                    let ctor = |b: &mut Self, k: usize, to: Option<&DataType>| -> Option<Name> {
                        let (key, i) = json_agg_col(input, k)?;
                        let (calls, names, _) = b.json_aggs.get(&key)?.clone();
                        let a = calls.get(i)?.clone();
                        if !a.json_ctor {
                            return None;
                        }
                        let agg = Expr::Agg(0, a.ty.clone());
                        let e = match to {
                            Some(t) => Expr::Cast(Box::new(agg), a.ty.clone(), t.clone(), true),
                            None => agg,
                        };
                        Some(b.name(&e, &names, std::slice::from_ref(&a)))
                    };
                    let mut extra: Vec<(usize, DataType, usize)> = Vec::new();
                    let mut out = Vec::new();
                    for e in exprs.iter() {
                        let e2 = e
                            .clone()
                            .transform(&mut |x| {
                                Ok(match x {
                                    Expr::Cast(inner, DataType::Json | DataType::Jsonb, to, ex)
                                        if matches!(&*inner, Expr::Col(..)) =>
                                    {
                                        let Expr::Col(k, t) = *inner else { unreachable!() };
                                        let at = extra.iter().find(|(kk, tt, _)| *kk == k && *tt == to).map(|x| x.2);
                                        match at {
                                            Some(m) => Expr::Col(m, to),
                                            None => match ctor(self, k, Some(&to)) {
                                                Some(n) => {
                                                    let m = jcols.len();
                                                    jcols.push(n);
                                                    extra.push((k, to.clone(), m));
                                                    Expr::Col(m, to)
                                                }
                                                None => Expr::Cast(Box::new(Expr::Col(k, t)), DataType::Json, to, ex),
                                            },
                                        }
                                    }
                                    x => x,
                                })
                            })
                            .unwrap_or_else(|_| e.clone());
                        out.push(e2);
                    }
                    for (k, jc) in jcols.iter_mut().enumerate().take(c.internal.len()) {
                        if let Some(n) = ctor(self, k, None) {
                            *jc = n;
                        }
                    }
                    out
                };
                let saved = self.num_exec.replace(c.node.rows);
                // JSON_ARRAYAGG / JSON_OBJECTAGG as a window function: the projection
                // shows the constructor (`JSON_ARRAYAGG(…) OVER w1`), its cast merged.
                let win = match &**input {
                    Plan::Window { input: wi, calls } => Some((super::binder::plan_width(wi), calls)),
                    _ => None,
                };
                let internal: Vec<Name> = jexprs
                    .iter()
                    .map(|e| {
                        if let Some((iw, calls)) = win {
                            let (k, to) = match e {
                                Expr::Col(k, _) => (Some(*k), None),
                                Expr::Cast(x, DataType::Json | DataType::Jsonb, to, _) => match &**x {
                                    Expr::Col(k, _) => (Some(*k), Some(to.clone())),
                                    _ => (None, None),
                                },
                                _ => (None, None),
                            };
                            if let Some(k) = k.filter(|&k| k >= iw)
                                && let Some(call) = calls.get(k - iw)
                                && let WinFunc::Agg(a) = &call.func
                                && a.json_ctor
                                && let Some(over) = c.internal.get(k).and_then(|n| {
                                    let t = n.full.trim_end_matches(')');
                                    t.rfind(" OVER ").map(|p| {
                                        t[p..].split_whitespace().take(2).collect::<Vec<_>>().join(" ")
                                    })
                                })
                            {
                                let mut a = a.clone();
                                a.args = call.args.clone();
                                let agg = Expr::Agg(0, a.ty.clone());
                                let e = match to {
                                    Some(to) => Expr::Cast(Box::new(agg), a.ty.clone(), to, true),
                                    None => agg,
                                };
                                let n = self.name(&e, &c.internal, std::slice::from_ref(&a));
                                return Name {
                                    plain: format!("{} {over}", n.plain),
                                    full: format!("{} {over}", n.full),
                                };
                            }
                        }
                        self.name(e, &jcols, &[])
                    })
                    .collect();
                self.num_exec = saved;
                let no_phvs = self.plain_consts.contains(&addr(p));
                let exported = exprs
                    .iter()
                    .zip(&internal)
                    .map(|(e, n)| match e {
                        Expr::Col(j, _) => c.exported.get(*j).cloned().unwrap_or_else(|| n.clone()),
                        // (A constant isn't referenced from above: fix_upper_expr leaves it,
                        // search_indexed_tlist_for_non_var. A subquery's unread output NULLed
                        // in its select list is one; others may stand for PlaceHolderVars,
                        // referenced, unless none can be here.)
                        Expr::Const(Value::Null, _) if UNREAD_NULLS.with(|u| u.borrow().contains(&addr(p))) => n.clone(),
                        Expr::Const(..) if no_phvs => n.clone(),
                        _ => n.wrapped(),
                    })
                    .collect();
                if c.node.node_type == "Aggregate" || c.node.is_join() || c.node.node_type.ends_with(" Scan") {
                    // (ExecInitAgg initializes the projection before the aggregates'
                    // arguments: its SubPlans come first. So do a scan's and a join's
                    // ExecInit functions, before their quals.)
                    let mut tl = std::mem::take(&mut self.pending);
                    tl.append(&mut c.node.subplans);
                    c.node.subplans = tl;
                } else {
                    c.node.subplans.append(&mut self.pending);
                }
                // The target list's widths, and its evaluation per row (cost_qual_eval).
                let cols: Vec<ColInfo> = exprs
                    .iter()
                    .map(|e| match e {
                        Expr::Col(j, _) if *j < c.node.cols.len() => c.node.cols[*j].clone(),
                        e => ColInfo {
                            width: type_width(&e.ty()),
                            var: None,
                            // (An expression of columns of known distinct counts, none with
                            // statistics: as many values as their combinations.)
                            distinct: {
                                let used = e.columns();
                                let ds: Option<Vec<f64>> = used
                                    .iter()
                                    .map(|j| {
                                        c.node.cols.get(*j).filter(|x| x.var.is_none()).and_then(|x| x.distinct)
                                    })
                                    .collect();
                                // (A constant: one value, as estimate_num_groups counts it.)
                                if matches!(e, Expr::Const(..)) {
                                    Some(1.0)
                                } else {
                                    ds.filter(|d| !d.is_empty() && !e.is_volatile())
                                        .map(|d| d.iter().product::<f64>().min(costs::clamp_rows(c.node.rows)))
                                }
                            },
                        },
                    })
                    .collect();
                // (Only the columns read above count.)
                let mask = self.needed.get(&addr(p));
                // (Over a grouping, its target list holds the group keys the select list
                // doesn't, as resjunk entries.)
                let agg_groups = match &**input {
                    Plan::Agg { groups, sets: None, .. } => Some(groups.len()),
                    Plan::Filter { input: fi, .. } => match &**fi {
                        Plan::Agg { groups, sets: None, .. } => Some(groups.len()),
                        _ => None,
                    },
                    _ => None,
                };
                let junk_keys: u32 = match agg_groups {
                    Some(ng) if c.node.node_type == "Aggregate" => (0..ng)
                        .filter(|j| !exprs.iter().any(|e| matches!(e, Expr::Col(k, _) if k == j)))
                        .filter_map(|j| c.node.cols.get(j).map(|x| x.width))
                        .sum(),
                    // (A level's LockRows returns its row marks too, resjunk entries of
                    // the level's target list.)
                    _ if junk_only => match &**input {
                        Plan::LockRows { marks, .. } => 6 * marks.len() as u32,
                        _ => 0,
                    },
                    _ => 0,
                };
                c.node.width = cols
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| mask.is_none_or(|m| m.get(*i).copied().unwrap_or(true)))
                    .map(|(_, c)| c.width)
                    .sum::<u32>()
                    + junk_keys;
                let w = c.node.width;
                for pt in c.node.partials_mut() {
                    pt.width = w;
                    pt.cols = cols.clone();
                }
                c.node.cols = cols;
                // (cost_qual_eval: a called function's procost too.)
                let per: f64 = exprs.iter().map(costs::qual_cost).sum();
                c.node.total_cost += c.node.rows * per;
                for pt in c.node.partials_mut() {
                    pt.total_cost += pt.rows * per;
                }
                // Sub-selects in the target list (cost_subplan), per row.
                let (sp_startup, sp_per) = subplan_costs(exprs, self.ctx);
                c.node.startup_cost += sp_startup;
                c.node.total_cost += sp_startup + c.node.rows * sp_per;
                if let Some((st, tot, w)) = injected {
                    (c.node.startup_cost, c.node.total_cost, c.node.width) = (st, tot, w);
                }
                if self.o.verbose {
                    // (The Result over a Sort computes the whole target list, its resjunk
                    // sort columns too.)
                    let mask = if sort_above { None } else { self.needed.get(&addr(p)).cloned() };
                    c.node.output = internal
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| {
                            mask.as_ref().is_none_or(|m| m.get(*i).copied().unwrap_or(true))
                        })
                        .map(|(_, n)| self.pick(n, Cx::Output))
                        .collect();
                    // (A LockRows computing it returns its rows' marks too, resjunk, as do
                    // the Limits over it.)
                    lock_junk(&mut c.node);
                    let out = c.node.output.clone();
                    for pt in c.node.partials_mut() {
                        pt.output = out.clone();
                    }
                    // A level's target list carries the GROUP BY keys its select list
                    // doesn't name as resjunk entries (in their order), up to the level's
                    // top (`group_tlists`).
                    if let Some(junk) = self.gtl.finals.get(&addr(p)).cloned() {
                        let mut carried = Vec::new();
                        for j in junk {
                            if let Some(n) = c.internal.get(j) {
                                let shown = self.pick(n, Cx::Output);
                                if !c.node.output.contains(&shown) {
                                    c.node.output.push(shown.clone());
                                    c.node.junk_keys.push(shown.clone());
                                }
                                carried.push(shown);
                            }
                        }
                        self.carried.insert(addr(p), carried);
                    }
                    // The statement's target list carries its windows' PARTITION BY / ORDER
                    // BY columns as resjunk entries.
                    if addr(p) == self.top
                        && let Plan::Window { calls, .. } = &**input
                    {
                        for call in calls {
                            for e in call
                                .partition_by
                                .iter()
                                .chain(call.order_by.iter().map(|k| &k.expr))
                            {
                                if let Expr::Col(j, _) = e
                                    && !exprs.iter().any(|x| matches!(x, Expr::Col(i, _) if i == j))
                                    && let Some(n) = c.internal.get(*j)
                                {
                                    let shown = self.pick(n, Cx::Output);
                                    if !c.node.output.contains(&shown) {
                                        c.node.output.push(shown);
                                    }
                                } else if !matches!(e, Expr::Col(..))
                                    && !crate::sql::binder::optimize::pseudo_constant(e)
                                {
                                    // (A computed key: a reference to the input's column.)
                                    let n = self.name(e, &c.internal, &[]).wrapped();
                                    let shown = self.pick(&n, Cx::Output);
                                    if !c.node.output.contains(&shown) {
                                        c.node.output.push(shown);
                                    }
                                }
                            }
                        }
                    }
                }
                if into_gather {
                    let gm = &mut c.node;
                    let child = gm.children.remove(0);
                    let mut r = ExNode::new("Result");
                    r.rows = child.rows;
                    r.width = gm.width;
                    r.cols = gm.cols.clone();
                    r.startup_cost = child.startup_cost;
                    r.total_cost = child.total_cost + child.rows * (costs::CPU_TUPLE_COST + per);
                    r.key = child.key;
                    r.workers = child.workers;
                    r.output = std::mem::take(&mut gm.output);
                    gm.output = r.output.iter().map(|o| refer_to(o)).collect();
                    r.children.push(child.child("Outer"));
                    gm.children.insert(0, r.child("Outer"));
                }
                let built = Built {
                    node: c.node,
                    internal,
                    exported,
                };
                // A FROM subquery of windows nothing filters: its Subquery Scan stays
                // unless trivial (its sort / partition keys are resjunk columns).
                if let Plan::Window { calls, .. } = &**input
                    && let Some(info) = calls.first().and_then(|c| c.subquery.clone())
                    && !under_filter
                    && !self.trivial_subquery(p, addr(p))
                {
                    let saved = self.sub_alias.replace(info);
                    let b = self.subquery_scan(built, addr(p));
                    self.sub_alias = saved;
                    return b;
                }
                built
            }
            Plan::ProjectSet { input, exprs, srfs } => {
                let mut c = self.node(input);
                // A Values Scan under it returns exactly the columns it reads (CP_EXACT_TLIST).
                if self.o.verbose
                    && c.node.node_type == "Values Scan"
                    && let Some(mask) = self.needed.get(&addr(input))
                    && mask.iter().any(|m| !m)
                {
                    c.node.output = c
                        .internal
                        .iter()
                        .zip(mask)
                        .filter(|(_, m)| **m)
                        .map(|(x, _)| self.pick(x, Cx::Output))
                        .collect();
                }
                if matches!(&**input, Plan::Sort { .. }) {
                    self.sort_input_target(&mut c, exprs, srfs);
                }
                let mut n = ExNode::new("ProjectSet");
                // create_set_projection_path: the most rows a call gives per input row
                // (its support function's, else prorows); cpu_tuple_cost and the target
                // list's cost per input row, half a cpu_tuple_cost per added row.
                let per_row = srfs
                    .iter()
                    .map(|s| costs::function_rows(s.func.name, &s.args, s.func.srf.is_some()))
                    .fold(1.0f64, f64::max);
                let calls: Vec<Expr> = exprs
                    .iter()
                    .map(|e| {
                        e.clone()
                            .transform(&mut |x| {
                                Ok(match x {
                                    Expr::Srf(i, t) => match srfs.get(i) {
                                        Some(s) => Expr::Call(s.func.clone(), s.args.clone(), t),
                                        None => Expr::Srf(i, t),
                                    },
                                    x => x,
                                })
                            })
                            .unwrap_or_else(|_| e.clone())
                    })
                    .collect();
                let per_tuple: f64 = calls.iter().map(costs::qual_cost).sum();
                let in_rows = c.node.rows;
                n.rows = costs::clamp_rows(in_rows * per_row);
                n.startup_cost = c.node.startup_cost;
                n.total_cost = c.node.total_cost
                    + (costs::CPU_TUPLE_COST + per_tuple) * in_rows
                    + (n.rows - in_rows) * costs::CPU_TUPLE_COST / 2.0;
                // The expressions read the input row, then each call's value.
                let mut row = c.exported.clone();
                for s in srfs {
                    // (A plain expression run in step with the calls: `const_srf`.)
                    let call = match &s.args[..] {
                        [a] if s.func.oid == 0 && s.func.name == "?column?" => a.clone(),
                        _ => Expr::Call(s.func.clone(), s.args.clone(), s.ty.clone()),
                    };
                    row.push(self.name(&call, &c.exported, &[]));
                }
                let internal: Vec<Name> = exprs
                    .iter()
                    .map(|e| {
                        let e = e
                            .clone()
                            .transform(&mut |x| {
                                Ok(match x {
                                    Expr::Srf(i, t) => match srfs.get(i) {
                                        Some(s) => Expr::Call(s.func.clone(), s.args.clone(), t),
                                        None => Expr::Srf(i, t),
                                    },
                                    x => x,
                                })
                            })
                            .unwrap_or_else(|_| e.clone());
                        self.name(&e, &row, &[])
                    })
                    .collect();
                let exported = exprs
                    .iter()
                    .zip(&internal)
                    .map(|(e, nm)| match e {
                        Expr::Col(j, _) if *j < c.exported.len() => c.exported[*j].clone(),
                        _ => nm.wrapped(),
                    })
                    .collect();
                n.width = exprs.iter().map(|e| type_width(&e.ty())).sum();
                // Computing its level's target list over a grouping (split_pathtarget_at_srfs):
                // the grouping returns what the expressions read of it, in the order they
                // read it (its keys, aggregates and GROUPING() calls, not the grouping
                // sets' mask), then the resjunk group keys; this node returns those too.
                let over_agg = match &**input {
                    Plan::Agg { .. } => true,
                    Plan::Filter { input: fi, .. } => matches!(&**fi, Plan::Agg { .. }),
                    _ => false,
                };
                if self.o.verbose && over_agg && matches!(c.node.node_type, "Aggregate" | "Group") {
                    let junk = self.gtl.finals.get(&addr(p)).cloned().unwrap_or_default();
                    let mut items: Vec<String> = Vec::new();
                    let add = |s: String, items: &mut Vec<String>| {
                        if !items.contains(&s) {
                            items.push(s);
                        }
                    };
                    // (A column past the input's is a call's value: what its arguments read.)
                    let iw = c.exported.len();
                    let read: Vec<Expr> = exprs
                        .iter()
                        .map(|e| {
                            e.clone()
                                .transform(&mut |x| {
                                    Ok(match x {
                                        Expr::Col(j, t) if j >= iw => match srfs.get(j - iw) {
                                            Some(s) => Expr::Call(s.func.clone(), s.args.clone(), t),
                                            None => Expr::Col(j, t),
                                        },
                                        x => x,
                                    })
                                })
                                .unwrap_or_else(|_| e.clone())
                        })
                        .collect();
                    for e in &read {
                        let mut found: Vec<Expr> = Vec::new();
                        let mut skip: Vec<*const Expr> = Vec::new();
                        e.walk(&mut |x| {
                            if skip.contains(&(x as *const Expr)) {
                                return;
                            }
                            match x {
                                Expr::Call(d, args, _) if d.oid == 0 && d.name == "grouping" => {
                                    skip.extend(args.iter().map(|a| a as *const Expr));
                                    found.push(x.clone());
                                }
                                Expr::Col(..) => found.push(x.clone()),
                                _ => {}
                            }
                        });
                        for f in found {
                            let s = match &f {
                                Expr::Col(j, _) => match c.internal.get(*j) {
                                    Some(nm) => self.pick(nm, Cx::Output),
                                    None => continue,
                                },
                                _ => {
                                    let nm = self.name(&f, &c.internal, &[]);
                                    self.pick(&nm, Cx::Output)
                                }
                            };
                            add(s, &mut items);
                        }
                    }
                    let mut carried = Vec::new();
                    for j in junk {
                        if let Some(nm) = c.internal.get(j) {
                            let s = self.pick(nm, Cx::Output);
                            add(s.clone(), &mut items);
                            if let Some(e) = c.exported.get(j) {
                                carried.push(self.pick(e, Cx::Output));
                            }
                        }
                    }
                    c.node.output = items;
                    if !carried.is_empty() {
                        n.output = internal.iter().map(|nm| self.pick(nm, Cx::Output)).collect();
                        for s in &carried {
                            if !n.output.contains(s) {
                                n.output.push(s.clone());
                            }
                        }
                        self.carried.insert(addr(p), n.output[internal.len()..].to_vec());
                    }
                }
                n.children.push(c.node.child("Outer"));
                Built {
                    node: n,
                    internal,
                    exported,
                }
            }
            Plan::Agg {
                input,
                groups,
                sets,
                aggs,
            } => {
                self.agg_key = Some(key);
                // (MIN / MAX as InitPlans: the planner's `minmax_aggs`.)
                self.agg(input, groups, sets.as_deref(), aggs)
            }
            Plan::Window { input, calls } => self.window(key, input, calls),
            Plan::Sort { input, keys } if matches!(
                &**input,
                Plan::SetOp {
                    kind: SetOpKind::Union,
                    all: false,
                    ..
                }
            ) && keys.iter().all(|k| matches!(k.expr, Expr::Col(..))) =>
            {
                self.sorted_union(input, keys)
            }
            Plan::Sort { input, keys } => {
                // (A Merge Append's member sorts fully: make_sort_from_pathkeys.)
                let member_sort = MERGE_MEMBERS.with(|m| m.get());
                // (The ORDER BY over windows: its items are the first sort group references
                // the windows are ordered by.)
                let win_keys: Vec<String> = {
                    // (Through a DISTINCT of the columns: its sort comes in that order.)
                    let below = match &**input {
                        Plan::Distinct { input: di, on: None, .. } => &**di,
                        p => p,
                    };
                    let (exprs, win): (Option<&Vec<Expr>>, &Plan) = match below {
                        Plan::Project { input: pin, exprs } => (Some(exprs), pin),
                        p => (None, p),
                    };
                    match win {
                        Plan::Window { input: wi, .. } => {
                            let iw = super::binder::plan_width(wi);
                            keys.iter()
                                .filter_map(|k| {
                                    let e = match (&k.expr, exprs) {
                                        (Expr::Col(i, _), Some(ex)) => ex.get(*i)?.clone(),
                                        (e, _) => e.clone(),
                                    };
                                    (e.columns().iter().all(|&c| c < iw)).then(|| format!("{e:?}"))
                                })
                                .collect()
                        }
                        _ => Vec::new(),
                    }
                };
                WINDOW_ORDER.with(|w| *w.borrow_mut() = win_keys);
                // An aggregation below may group in this order (its keys by position).
                self.group_order = sort_group_order(input, keys);
                self.group_named = sort_group_named(input, keys);
                self.group_order_full = self.group_order.as_ref().is_some_and(|g| g.len() == keys.len());
                let saved_hint = self.order_hint.replace((addr(input), keys.clone()));
                let mut c = {
                    let merged = merge_appended(input);
                    let _g = crate::scoped::set(&MERGE_MEMBERS, merged, "explain::MERGE_MEMBERS.set");
                    if merged {
                        crate::scoped::checkpoint("explain::MERGE_MEMBERS");
                    }
                    self.node(input)
                };
                self.order_hint = saved_hint;
                // A key computed over an inheritance tree's Append (a merge clause's
                // expression, `t1::text`): the Append can't project, so a Result computes it
                // under the Sort (prepare_sort_from_pathkeys / inject_projection_plan, with
                // the subplan's costs).
                if matches!(&**input, Plan::Append { inh: Some(_), .. })
                    && c.node.node_type == "Append"
                    && keys.iter().any(|k| !matches!(k.expr, Expr::Col(..)))
                {
                    let app = std::mem::replace(&mut c.node, ExNode::new("Result"));
                    c.node.rows = app.rows;
                    c.node.width = app.width;
                    c.node.startup_cost = app.startup_cost;
                    c.node.total_cost = app.total_cost;
                    c.node.cols = app.cols.clone();
                    c.node.output = app.output.clone();
                    c.node.children.push(app.child("Outer"));
                }
                // (VERBOSE: a projection over an inheritance tree merged in order is its
                // members' to compute, each over its own columns; a Result above returns
                // the query's columns when those aren't the members'.)
                let mut result_out: Option<Vec<String>> = None;
                if self.o.verbose
                    && c.node.node_type == "Result"
                    && merge_appended(input)
                    && let Plan::Project { input: pin, exprs } = &**input
                    && matches!(&**pin, Plan::Append { inh: Some(_), .. })
                    && c.node.children.len() == 1
                    && c.node.children[0].node_type == "Append"
                    && c.node.output.len() == exprs.len()
                    && let Some(parent) = self.refnames.get(&addr(pin)).map(|n| format!("{}.", quote_ident(n)))
                {
                    let extra: Vec<String> = c
                        .node
                        .output
                        .iter()
                        .zip(exprs)
                        .filter(|(_, e)| !matches!(e, Expr::Col(..)))
                        .map(|(o, _)| o.clone())
                        .collect();
                    fn add(n: &mut ExNode, extra: &[String], parent: &str) {
                        if n.node_type == "Append" {
                            n.children.iter_mut().for_each(|k| add(k, extra, parent));
                            return;
                        }
                        let Some(alias) = member_alias(n) else { return };
                        let mine: Vec<String> = extra
                            .iter()
                            .map(|x| replace_qualifier(x, parent, &format!("{}.", quote_ident(&alias))))
                            .collect();
                        if n.node_type == "Sort" {
                            // (It returns what its input does.)
                            if let Some(k) = n.children.first_mut() {
                                if !k.output.is_empty() {
                                    n.output = k.output.clone();
                                }
                                k.output.extend(mine.iter().cloned());
                            }
                            n.output.extend(mine.iter().map(|x| format!("({x})")));
                        } else {
                            n.output.extend(mine);
                        }
                    }
                    let read: std::collections::BTreeSet<usize> = exprs.iter().flat_map(|e| e.columns()).collect();
                    let cols: Vec<usize> = exprs
                        .iter()
                        .filter_map(|e| match e {
                            Expr::Col(i, _) => Some(*i),
                            _ => None,
                        })
                        .collect();
                    let computed_last = exprs
                        .iter()
                        .skip_while(|e| matches!(e, Expr::Col(..)))
                        .all(|e| !matches!(e, Expr::Col(..)));
                    if !(computed_last && cols == read.iter().copied().collect::<Vec<_>>()) {
                        result_out = Some(
                            c.node
                                .output
                                .iter()
                                .zip(exprs)
                                .map(|(o, e)| if matches!(e, Expr::Col(..)) { o.clone() } else { format!("({o})") })
                                .collect(),
                        );
                    }
                    let mut app = c.node.children.pop().expect("one child");
                    app.relationship = None;
                    add(&mut app, &extra, &parent);
                    c.node = app;
                }
                self.group_order = None;
                self.group_order_full = false;
                self.group_named.clear();
                // (A scan sorted as is returns the columns read above and the keys: the
                // sort's input target.)
                if self.o.verbose
                    && matches!(&**input, Plan::Scan(_))
                    && c.node.is_scan()
                    && let Some(m) = self.needed.get(&addr(input))
                    && c.node.output.len() == m.len()
                    && m.iter().any(|x| !x)
                {
                    c.node.output = c.node.output.iter().zip(m).filter(|(_, k)| **k).map(|(o, _)| o.clone()).collect();
                    let out = c.node.output.clone();
                    for pt in c.node.partials_mut() {
                        pt.output = out.clone();
                    }
                }
                let part = c.node.partial.clone();
                let others = c.node.partials.clone();
                let rel_rows = c.node.rows;
                let presort = part.is_some().then(|| {
                    let mut u = c.node.clone();
                    u.presort = None;
                    Box::new((u, c.internal.clone()))
                });
                self.settle(&mut c.node);
                // (A member's sort key expressions its scan computes: references to them.)
                let list: Vec<String> = keys
                    .iter()
                    .map(|k| {
                        // (A constant the member returns stays a constant: no reference to
                        // it, set_dummy_tlist_references.)
                        let constant = match (&k.expr, &**input) {
                            (Expr::Col(i, _), Plan::Project { exprs: pe, .. }) if member_sort => {
                                pe.get(*i).filter(|e| matches!(e, Expr::Const(..))).cloned()
                            }
                            _ => None,
                        };
                        if let Some(cst) = constant {
                            return self.show(&cst, &[], Cx::Upper) + &sort_suffix(k);
                        }
                        // (A key under an explicit COLLATE: the sort's collation, not a value
                        // computed below it.)
                        if let (Expr::Col(i, _), Plan::Project { exprs: pe, .. }) = (&k.expr, &**input)
                            && let Some(Expr::Collate(inner, coll, loc)) = pe.get(*i)
                            && *loc >= 0
                            && matches!(&**inner, Expr::Col(..))
                            && k.using.is_none()
                            && let Some(name) = c.internal.get(*i)
                        {
                            return format!("{} COLLATE {}", self.pick(name, Cx::Upper), quote_ident(coll)) + &sort_suffix(k);
                        }
                        // (A member's sort over a scan computing a UNION ALL arm's values:
                        // the scan computes the key too, over its own columns.)
                        let t = if member_sort && c.node.is_scan() && !matches!(k.expr, Expr::Col(..)) {
                            self.show(&k.expr, &c.internal, Cx::Upper)
                        } else {
                            self.show(&k.expr, &c.exported, Cx::Upper)
                        };
                        // (Or over a join, sorting it for a merge join on an expression the
                        // join's target list computes: a reference to it.)
                        let t = if !matches!(k.expr, Expr::Col(..) | Expr::Const(..))
                            && ((member_sort && c.node.is_scan()) || !member_sort)
                        {
                            format!("({t})")
                        } else {
                            t
                        };
                        // (A column of a collation other than its type's default shows it
                        // (show_sortorder_options), byte-ordered ones ("C", "POSIX") too.)
                        let declared = match &k.expr {
                            Expr::Col(j, ty) if k.using.is_none() => c
                                .node
                                .cols
                                .get(*j)
                                .and_then(|ci| ci.var.as_ref())
                                .filter(|v| v.expr.is_none())
                                .and_then(|v| v.table.columns.get(v.col).and_then(|col| col.collation.clone()))
                                .filter(|n| n != "default" && !(*ty == DataType::Name && n == "C")),
                            _ => None,
                        };
                        // (So does a value computed from a `name` column, "C" derived.)
                        let declared = declared.or_else(|| match (&k.expr, &**input) {
                            (Expr::Col(j, _), Plan::Project { exprs: pe, .. })
                                if k.using.is_none() && pe.get(*j).is_some_and(name_derived_c) =>
                            {
                                Some("C".to_string())
                            }
                            _ => None,
                        });
                        // (Or a grouping's key under a written COLLATE: the sort's collation,
                        // show_sortorder_options.)
                        let declared = declared.or_else(|| match (&k.expr, &**input) {
                            (Expr::Col(i, _), Plan::Agg { groups, sets: None, .. }) if k.using.is_none() => {
                                match groups.get(*i) {
                                    Some(Expr::Collate(x, coll, loc)) if *loc >= 0 && matches!(&**x, Expr::Col(..)) => {
                                        Some(coll.clone())
                                    }
                                    _ => None,
                                }
                            }
                            _ => None,
                        });
                        let t = match declared {
                            Some(n) => format!("{t} COLLATE {}", quote_ident(&n)),
                            None => t,
                        };
                        t + &sort_suffix(k)
                    })
                    .collect();
                // A projection over a UNION ALL merged in order: the sort expressions are
                // the members' to compute (prepare_sort_from_pathkeys adds them to each
                // member's target list), a Result above projecting the query's columns from
                // the Merge Append's unless they are its own (bug #18652).
                if c.node.node_type == "Result"
                    && merge_appended(input)
                    && let Plan::Project { input: pin, exprs } = &**input
                    && matches!(
                        &**pin,
                        Plan::Append { inh: None, .. }
                            | Plan::SetOp {
                                kind: SetOpKind::Union,
                                all: true,
                                ..
                            }
                    )
                    && c.node.children.len() == 1
                    && c.node.children[0].node_type == "Append"
                    && keys.iter().all(|k| matches!(k.expr, Expr::Col(..)))
                {
                    let width = super::binder::plan_width(pin);
                    let key_exprs: Vec<Expr> = keys
                        .iter()
                        .filter_map(|k| match &k.expr {
                            Expr::Col(i, _) => exprs.get(*i).cloned(),
                            _ => None,
                        })
                        .collect();
                    // (Only computed keys: a column the members already return.)
                    let junk: Vec<Expr> = key_exprs.iter().filter(|e| !matches!(e, Expr::Col(..))).cloned().collect();
                    let mut app = c.node.children.pop().expect("one child");
                    let mut kids = Vec::new();
                    fn flat(n: ExNode, out: &mut Vec<ExNode>) {
                        if n.node_type == "Append" {
                            n.children.into_iter().for_each(|k| flat(k, out));
                        } else {
                            out.push(n.child("Member"));
                        }
                    }
                    for k in std::mem::take(&mut app.children) {
                        flat(k, &mut kids);
                    }
                    // Each member's scan computes them over its own outputs.
                    let mut first: Option<(Vec<String>, Vec<String>)> = None;
                    if self.o.verbose {
                        for m in kids.iter_mut() {
                            let scan_out = if m.node_type == "Sort" {
                                m.children.first().map(|k| k.output.clone()).unwrap_or_default()
                            } else {
                                m.output.clone()
                            };
                            if scan_out.len() != width {
                                continue;
                            }
                            let names: Vec<Name> = scan_out.iter().map(|o| Name::same(o.clone())).collect();
                            let texts: Vec<String> = junk.iter().map(|e| self.show(e, &names, Cx::Output)).collect();
                            if m.node_type == "Sort" {
                                if let Some(k) = m.children.first_mut() {
                                    k.output.extend(texts.iter().cloned());
                                }
                                m.output.extend(texts.iter().map(|t| format!("({t})")));
                            } else {
                                m.output.extend(texts.iter().cloned());
                            }
                            if first.is_none() {
                                first = Some((scan_out, texts));
                            }
                        }
                    }
                    let n = kids.len() as f64;
                    let cmp = 2.0 * costs::CPU_OPERATOR_COST;
                    let logn = n.log2();
                    let mut ma = ExNode::new("Merge Append");
                    ma.details.push(Detail::List("Sort Key", list.clone()));
                    ma.rows = kids.iter().map(|k| k.rows).sum();
                    ma.width = app.width;
                    ma.cols = app.cols.clone();
                    ma.key = app.key;
                    ma.startup_cost = kids.iter().map(|k| k.startup_cost).sum::<f64>() + cmp * n * logn;
                    ma.total_cost = ma.startup_cost
                        + kids.iter().map(|k| k.total_cost - k.startup_cost).sum::<f64>()
                        + ma.rows * cmp * logn
                        + costs::CPU_TUPLE_COST * 0.5 * ma.rows;
                    ma.order = list;
                    ma.children = kids;
                    // The Merge Append returns the members' columns, then the sort
                    // expressions: the query's own columns when they are just those.
                    let own: Vec<Expr> = (0..width)
                        .map(|j| Expr::Col(j, exprs.get(j).map_or(DataType::Unknown, |e| e.ty())))
                        .chain(junk.iter().cloned())
                        .collect();
                    let same = exprs.len() == own.len()
                        && exprs.iter().zip(&own).all(|(a, b)| match (a, b) {
                            (Expr::Col(x, _), Expr::Col(y, _)) => x == y,
                            (a, b) => a.same_as(b),
                        });
                    if same {
                        ma.output = c.node.output.clone();
                        return Built::passthrough(ma, c.exported);
                    }
                    let mut r = result_over(ma, Some(Vec::new()));
                    r.output = match &first {
                        Some((scan_out, texts)) => {
                            let names: Vec<Name> = scan_out.iter().map(|o| Name::same(o.clone())).collect();
                            exprs
                                .iter()
                                .map(|e| match junk.iter().position(|j| j.same_as(e)) {
                                    Some(i) => format!("({})", texts[i]),
                                    None => self.show(e, &names, Cx::Output),
                                })
                                .collect()
                        }
                        None => Vec::new(),
                    };
                    r.total_cost = r.children[0].total_cost;
                    return Built::passthrough(r, c.exported);
                }
                // An append relation the planner merges in order (`merge_append`): its
                // members (nested Appends flattened) each in the order, merged.
                if c.node.node_type == "Append" && merge_appended(input) {
                    fn flatten(mut n: ExNode, out: &mut Vec<ExNode>) {
                        if n.node_type == "Append" {
                            n.children.into_iter().for_each(|k| flatten(k, out));
                        } else {
                            // (A member's Sort returns what its input does.)
                            if n.node_type == "Sort"
                                && let Some(k) = n.children.first()
                                && !k.output.is_empty()
                                && n.output.len() > k.output.len()
                            {
                                n.output = k.output.clone();
                            }
                            out.push(n.child("Member"));
                        }
                    }
                    let mut kids = Vec::new();
                    for k in std::mem::take(&mut c.node.children) {
                        flatten(k, &mut kids);
                    }
                    // Partitions read in their order: a plain Append (sub-partitioned
                    // ones in theirs, or merged).
                    let exprs: Vec<Expr> = keys.iter().map(|k| k.expr.clone()).collect();
                    let dirs: Vec<(bool, bool)> = keys.iter().map(|k| (k.desc, k.nulls_first)).collect();
                    // (Members run-time pruning removed as the executor starts: their
                    // places left empty, each level counting its own.)
                    let kept = startup_kept(input, self.ctx);
                    if let Some(order) = crate::sql::paths::partition_order(input, &exprs, &dirs)
                        && let Some(mask) = kept.filter(|m| {
                            m.len() == crate::sql::paths::count_members(&order) && m.iter().filter(|k| **k).count() == kids.len()
                        })
                    {
                        let mut it = kids.into_iter();
                        let mut slots: Vec<Option<ExNode>> = mask.iter().map(|k| if *k { it.next() } else { None }).collect();
                        let mut a = ordered_append(&order, &mut slots, &list);
                        a.width = c.node.width;
                        a.cols = c.node.cols.clone();
                        a.key = c.node.key;
                        a.output = c.node.output.clone();
                        if a.node_type == "Append" {
                            a.order = list;
                        }
                        return Built::passthrough(result_over(a, result_out), c.exported);
                    }
                    let n = kids.len() as f64;
                    let cmp = 2.0 * costs::CPU_OPERATOR_COST;
                    let logn = n.log2();
                    let mut ma = ExNode::new("Merge Append");
                    ma.details.push(Detail::List("Sort Key", list.clone()));
                    // (The members run-time pruning removed as the executor starts.)
                    ma.details.extend(
                        c.node.details.iter().filter(|d| matches!(d, Detail::Text("Subplans Removed", _))).cloned(),
                    );
                    ma.rows = kids.iter().map(|k| k.rows).sum();
                    ma.width = c.node.width;
                    ma.cols = c.node.cols.clone();
                    ma.key = c.node.key;
                    ma.output = c.node.output.clone();
                    ma.startup_cost = kids.iter().map(|k| k.startup_cost).sum::<f64>() + cmp * n * logn;
                    ma.total_cost = ma.startup_cost
                        + kids.iter().map(|k| k.total_cost - k.startup_cost).sum::<f64>()
                        + ma.rows * cmp * logn
                        + costs::CPU_TUPLE_COST * 0.5 * ma.rows;
                    ma.order = list;
                    ma.children = kids;
                    return Built::passthrough(result_over(ma, result_out), c.exported);
                }
                // (A merge join's inner input sorts in full: an incremental sort can't mark and
                // restore, initial_cost_mergejoin.)
                let inc_ok = !member_sort
                    && MERGE_INNER.with(|m| m.get()) != key
                    && crate::sql::paths::Gucs::read(self.ctx).incremental_sort;
                let key_cols: Vec<Option<usize>> = keys
                    .iter()
                    .map(|k| match &k.expr {
                        Expr::Col(j, _) => Some(*j),
                        _ => None,
                    })
                    .collect();
                let sorted_on = |x: &ExNode| x.order.len() >= list.len() && x.order[..list.len()] == list[..];
                // The input's other paths (its `alt` chain): one already in this order when
                // it costs less than sorting this one; the others in it, or in a leading
                // part of it sorted incrementally, for the node above to weigh
                // (create_ordered_paths: a LIMIT may take one).
                let mut alts: Vec<ExNode> = Vec::new();
                let mut next = c.node.alt.take();
                while let Some(mut a) = next {
                    next = a.alt.take();
                    alts.push(*a);
                }
                if let Some(i) = alts.iter().position(&sorted_on) {
                    let mut sn = ExNode::new("Sort");
                    self.sort_costs(&mut sn, &c.node);
                    let alt = &alts[i];
                    if parallel::disabled_nodes(alt) <= parallel::disabled_nodes(&c.node)
                        && alt.total_cost <= sn.total_cost
                    {
                        c.node = alts.remove(i);
                    }
                }
                let mut unsorted_alt: Option<ExNode> = alts.iter().find(|a| !sorted_on(a)).cloned();
                let mut extra: Vec<ExNode> = Vec::new();
                for a in alts {
                    if sorted_on(&a) {
                        extra.push(a);
                    } else if inc_ok && list.iter().zip(&a.order).take_while(|(x, y)| x == y).count() > 0 {
                        extra.push(self.sorted_partial(&a, &list, &key_cols, true, a.key));
                    }
                }
                // (The plan chosen and those others: the cheaper in front, the rest its
                // alternatives.)
                let with_alts = |mut n: ExNode, extra: Vec<ExNode>| -> ExNode {
                    let mut rest: Vec<ExNode> = Vec::new();
                    let mut next = n.alt.take();
                    while let Some(mut a) = next {
                        next = a.alt.take();
                        rest.push(*a);
                    }
                    for x in extra {
                        if parallel::fuzzy_better(&x, &n) {
                            rest.push(std::mem::replace(&mut n, x));
                        } else {
                            rest.push(x);
                        }
                    }
                    let mut chain: Option<Box<ExNode>> = None;
                    for mut r in rest.into_iter().rev() {
                        r.alt = chain;
                        chain = Some(Box::new(r));
                    }
                    n.alt = chain;
                    n
                };
                let mut common = list
                    .iter()
                    .zip(&c.node.order)
                    .enumerate()
                    .take_while(|(oi, (a, b))| {
                        a == b || c.node.order_eq.get(*oi).is_some_and(|e| e.contains(a))
                    })
                    .count();
                // Already in that order: no sort (unless the input's other path sorted costs
                // less: a hashed grouping of few groups); partly: an incremental one.
                if common == list.len() && !list.is_empty() {
                    let swap = unsorted_alt.as_ref().filter(|a| a.node_type == "Aggregate" && c.node.node_type == "Aggregate").is_some_and(|alt| {
                        let mut sn = ExNode::new("Sort");
                        self.sort_costs(&mut sn, alt);
                        sn.disabled = !crate::sql::paths::Gucs::read(self.ctx).sort;
                        let d_alt = parallel::disabled_nodes(alt) + sn.disabled as usize;
                        let d_cur = parallel::disabled_nodes(&c.node);
                        d_alt < d_cur || (d_alt == d_cur && sn.total_cost * 1.01 < c.node.total_cost)
                    });
                    if !swap {
                        c.node = with_alts(c.node, extra);
                        return c;
                    }
                    c.node = unsorted_alt.take().unwrap();
                    common = 0;
                }
                if common > 0 && inc_ok {
                    let mut n = ExNode::new("Incremental Sort");
                    n.details.push(Detail::List("Sort Key", list.clone()));
                    n.details.push(Detail::List(
                        "Presorted Key",
                        list[..common].iter().map(|k| presorted_name(k)).collect(),
                    ));
                    n.details.push(Detail::IncSortInfo);
                    let pre: Vec<ColInfo> = keys[..common]
                        .iter()
                        .map(|k| match &k.expr {
                            Expr::Col(j, _) => c.node.cols.get(*j).cloned().unwrap_or_default(),
                            _ => ColInfo::default(),
                        })
                        .collect();
                    let groups = if pre.iter().any(|x| x.var.is_none()) {
                        c.node.rows.clamp(1.0, costs::DEFAULT_NUM_DISTINCT)
                    } else {
                        self.num_groups(&pre, c.node.rows)
                    };
                    (n.startup_cost, n.total_cost) = costs::incremental_sort_cost(
                        c.node.startup_cost,
                        c.node.total_cost,
                        c.node.rows,
                        c.node.width,
                        groups,
                        None,
                    );
                    n.rows = c.node.rows;
                    n.width = c.node.width;
                    n.cols = c.node.cols.clone();
                    n.order = list;
                    n.children.push(c.node.child("Outer"));
                    return Built::passthrough(with_alts(n, extra), c.exported);
                }
                let mut n = ExNode::new("Sort");
                n.details.push(Detail::List("Sort Key", list.clone()));
                n.details.push(Detail::SortInfo);
                self.sort_costs(&mut n, &c.node);
                // (Under enable_sort = off, the sort the plan needs anyway.)
                n.disabled = !crate::sql::paths::Gucs::read(self.ctx).sort;
                n.order = list.clone();
                if self.o.verbose && matches!(&**input, Plan::Scan(_)) && c.node.is_scan() {
                    n.output = c.node.output.clone();
                }
                // (A key the Sort's input computes for it, a merge join's expression: in that
                // input's target list, the Sort's referring to it.)
                if self.o.verbose && !member_sort {
                    for k in keys.iter().filter(|k| !matches!(k.expr, Expr::Col(..) | Expr::Const(..))) {
                        let t = self.show(&k.expr, &c.exported, Cx::Upper);
                        if !c.node.output.contains(&t) {
                            c.node.output.push(t.clone());
                        }
                        let r = format!("({t})");
                        if !n.output.contains(&r) {
                            n.output.push(r);
                        }
                    }
                }
                n.children.push(c.node.child("Outer"));
                // Or each worker sorts its share (or reads it in order), merged by a Gather
                // Merge: over a relation's partial plans sorted for the query's order
                // (generate_useful_gather_paths, the relation's rows).
                if let Some(pt) = part {
                    let rel_input = |p: &Plan| matches!(p, Plan::Scan(_) | Plan::Join { .. });
                    let rows = (rel_input(input)
                        || matches!(&**input, Plan::Project { input: pi, .. } if rel_input(pi)))
                    .then_some(rel_rows);
                    let mut gms = self.useful_gathers(&pt, &others, &list, &key_cols, rows, Some(key), !member_sort);
                    // (The cheapest in total; the cheapest to start beside it.)
                    let pick = |v: &[ExNode], by_startup: bool| {
                        (0..v.len()).reduce(|a, b| {
                            let better = if by_startup {
                                (parallel::disabled_nodes(&v[b]), v[b].startup_cost)
                                    < (parallel::disabled_nodes(&v[a]), v[a].startup_cost)
                            } else {
                                parallel::cheaper(&v[b], &v[a])
                            };
                            if better { b } else { a }
                        })
                    };
                    if let Some(i) = pick(&gms, false) {
                        let mut gm = gms.remove(i);
                        if let Some(j) = pick(&gms, true)
                            && gms[j].startup_cost < gm.startup_cost
                        {
                            extra.push(gms.remove(j));
                        }
                        if parallel::par_beats(&gm, &n) {
                            gm.alt = Some(Box::new(n));
                            return Built::passthrough(with_alts(gm, extra), c.exported);
                        }
                        n.alt = Some(Box::new(gm));
                    }
                }
                let mut n = with_alts(n, extra);
                n.presort = presort;
                Built::passthrough(n, c.exported)
            }
            // limit_needed: no count and a constant zero (or NULL) offset make no Limit
            // node (OFFSET 0 only keeps the subquery from being flattened).
            Plan::Limit {
                input,
                limit: None,
                offset: None | Some(Expr::Const(crate::types::Value::Int8(0) | crate::types::Value::Null, _)),
                with_ties: None,
            } => self.node(input),
            Plan::Limit {
                input,
                limit,
                offset,
                ..
            } => {
                let mut c = self.node(input);
                self.settle(&mut c.node);
                // preprocess_limit: a constant count (or offset); -1 when not constant.
                let est = |e: &Option<Expr>| match e {
                    None => 0.0,
                    Some(Expr::Const(crate::types::Value::Int8(v), _)) => (*v as f64).max(1.0),
                    Some(Expr::Const(crate::types::Value::Int4(v), _)) => (*v as f64).max(1.0),
                    Some(Expr::Const(crate::types::Value::Null, _)) => 0.0,
                    Some(_) => -1.0,
                };
                let (count, off) = (est(limit), est(offset));
                // The input's other path (a Gather Merge of sorted partial plans, say):
                // the Limit over the cheaper of them.
                let mut next = c.node.alt.take();
                let mut forms = vec![c.node];
                while let Some(mut a) = next {
                    next = a.alt.take();
                    forms.push(*a);
                }
                // (A Gather Merge of sorted partial plans: also as create_ordered_paths
                // costs it for the rows the LIMIT reads.)
                if count > 0.0 && off >= 0.0 {
                    // (add_path: fuzzily as costly as the unbounded one, the one returning
                    // fewer rows stays.)
                    let bounded: Vec<ExNode> = forms
                        .iter()
                        .filter_map(|f| {
                            let b = self.bounded_gather_merge(f, count + off)?;
                            (f.total_cost > b.total_cost * 1.01 || f.startup_cost > b.startup_cost * 1.01)
                                .then_some(b)
                        })
                        .collect();
                    forms.extend(bounded);
                }
                let mut n: Option<ExNode> = None;
                for f in forms {
                    let m = limit_over(f, count, off);
                    if n.as_ref().is_none_or(|n| parallel::cheaper(&m, n)) {
                        n = Some(m);
                    }
                }
                let mut n = n.expect("a plan");
                // set_dummy_tlist_references keeps Consts as Consts: the Limit (and the
                // sorts and LockRows of its level under it, which copy their input's target
                // list too) shows a constant of its level's select list itself, not as a
                // reference to it (`1`, not `(1)`); a node above refers to it as before.
                // (Its own output only: what it returns is referred to as before, a Hash
                // over it showing `(1)`.)
                let consts = level_select_consts(input);
                if self.o.verbose && consts.iter().any(Option::is_some) && consts.len() == c.exported.len() {
                    let mut own = c.exported.clone();
                    let mut swaps = Vec::new();
                    for (j, k) in consts.iter().enumerate() {
                        if let Some(k) = k {
                            let bare = self.name(k, &[], &[]);
                            swaps.push((self.pick(&own[j], Cx::Output), self.pick(&bare, Cx::Output)));
                            own[j] = bare;
                        }
                    }
                    n.output = own.iter().map(|x| self.pick(x, Cx::Output)).collect();
                    let mut t = n.children.first_mut();
                    while let Some(ch) = t {
                        if !matches!(ch.node_type, "Sort" | "Incremental Sort" | "LockRows") {
                            break;
                        }
                        for o in ch.output.iter_mut() {
                            if let Some((_, b)) = swaps.iter().find(|(w, _)| w == o) {
                                *o = b.clone();
                            }
                        }
                        t = ch.children.first_mut();
                    }
                }
                Built::passthrough(n, c.exported)
            }
            Plan::Distinct { input, on, hashed: by_hash, .. } => {
                // (Grouped on keys by hashing, the other columns passed through: a unique
                // path's HashAggregate as the join search chose it.)
                let hashed_on = *by_hash && on.is_some();
                // (Hashed: sorted, the Sort's input is the projection.)
                let plain = on.is_none()
                    && crate::sql::paths::Gucs::read(self.ctx).hashagg
                    && matches!(&**input, Plan::Project { input: b, exprs }
                        if matches!(&**b, Plan::Barrier { secure: false, alias, .. } if !alias.is_empty())
                            && exprs.iter().enumerate().all(|(i, e)| {
                                matches!(e, Expr::Col(..)) && !exprs[..i].iter().any(|x| x.same_as(e))
                            }));
                let saved_plain = std::mem::replace(&mut self.plain_distinct, plain.then(|| addr(input)));
                // (Its keys are the query's order below: distinct_pathkeys.)
                let dkeys: Vec<SortKey> = (0..super::binder::plan_width(input))
                    .map(|i| SortKey {
                        expr: Expr::Col(i, DataType::Unknown),
                        desc: false,
                        nulls_first: false,
                        using: None,
                    })
                    .collect();
                let saved_hint = std::mem::replace(&mut self.order_hint, on.is_none().then(|| (addr(input), dkeys)));
                let mut c = self.node(input);
                self.order_hint = saved_hint;
                self.plain_distinct = saved_plain;
                // (Its partial plan: a DISTINCT in the workers first, create_partial_distinct_paths.)
                let part = c.node.partial.clone();
                // (The relation's other partial paths, kept for their order.)
                let part_others = c.node.partials.clone();
                self.settle(&mut c.node);
                let in_alt = c.node.alt.take();
                // A hashed DISTINCT reads a plain scan's physical target list (its select
                // list of columns is the aggregate's).
                // (A semi join's side made unique reads just the compared values.)
                let semi_unique = self.unique_sides.contains(&key);
                // (Sorted instead, the Sort's input is its exact target list.)
                let exact_out = c.node.output.clone();
                let mut physical = false;
                if self.o.verbose && on.is_none() && semi_unique && c.node.is_scan() {
                    c.node.output = c.internal.iter().map(|n| self.pick(n, Cx::Output)).collect();
                    // (Hashed, of plain columns: create_unique_plan keeps the subplan's
                    // target list, its physical one where the join reads it as it is.)
                    if let Plan::Project { input: pin, exprs } = &**input
                        && exprs.iter().all(|e| matches!(e, Expr::Col(..)))
                        && let Plan::Scan(sc) = &**pin
                        && matches!(c.node.node_type, "Seq Scan" | "Index Scan" | "Bitmap Heap Scan")
                        && sc.mark.is_none()
                        && !sc.with_key
                    {
                        let names = self.table_names(&sc.table, &self.refname(pin));
                        c.node.physical = sc
                            .table
                            .columns
                            .iter()
                            .zip(&names)
                            .filter(|(col, _)| !col.dropped)
                            .map(|(_, n)| self.pick(n, Cx::Output))
                            .collect();
                    }
                }
                if self.o.verbose
                    && on.is_none()
                    && !semi_unique
                    && let Plan::Project { input: pin, exprs } = &**input
                    && exprs.iter().all(|e| matches!(e, Expr::Col(..)))
                    // (A column listed twice: the select list itself.)
                    && exprs
                        .iter()
                        .enumerate()
                        .all(|(i, e)| !exprs[..i].iter().any(|x| x.same_as(e)))
                    && let Plan::Scan(sc) = &**pin
                    && c.node.is_scan()
                {
                    let names = self.table_names(&sc.table, &self.refname(pin));
                    physical = true;
                    c.node.output = sc
                        .table
                        .columns
                        .iter()
                        .zip(&names)
                        .filter(|(col, _)| !col.dropped)
                        .map(|(_, n)| self.pick(n, Cx::Output))
                        .collect();
                }
                let mut n = if hashed_on {
                    let mut n = ExNode {
                        strategy: Some("Hashed"),
                        partial_mode: Some("Simple"),
                        ..ExNode::new("Aggregate")
                    };
                    n.title = "HashAggregate".into();
                    let keys = on
                        .iter()
                        .flatten()
                        .filter_map(|e| match e {
                            Expr::Col(j, _) => c.exported.get(*j).map(|k| self.pick(k, Cx::Upper)),
                            _ => None,
                        })
                        .collect();
                    n.details.push(Detail::List("Group Key", keys));
                    n.details.push(Detail::HashAggInfo);
                    n.disabled = !crate::sql::paths::Gucs::read(self.ctx).hashagg;
                    n
                } else if on.is_some() {
                    ExNode::new("Unique")
                } else {
                    let mut n = ExNode {
                        strategy: Some("Hashed"),
                        partial_mode: Some("Simple"),
                        ..ExNode::new("Aggregate")
                    };
                    n.title = "HashAggregate".into();
                    // Constant columns distinguish nothing: Postgres drops them as keys.
                    // (An InitPlan's value is a Param, a key all the same.)
                    let constant = |i: usize| match &**input {
                        Plan::Project { exprs, .. } => exprs.get(i).is_some_and(|e| {
                            let mut sub = false;
                            e.walk(&mut |x| sub |= matches!(x, Expr::Subquery(_)));
                            !sub && crate::sql::binder::optimize::pseudo_constant(e)
                        }),
                        _ => false,
                    };
                    // (A hashed grouping shows a computed key as the expression itself: a
                    // window function's result too.)
                    let windowed = |p: &Plan, j: usize| {
                        matches!(p, Plan::Window { input: wi, .. } if j >= super::binder::plan_width(wi))
                    };
                    let computed = |i: usize| match &**input {
                        Plan::Project { exprs, input: pin } => exprs.get(i).is_some_and(|e| match e {
                            Expr::Col(j, _) => windowed(pin, *j),
                            _ => true,
                        }),
                        p => windowed(p, i),
                    };
                    // (A key listed twice is one key: remove_redundant pathkeys.)
                    let dup = |i: usize| match &**input {
                        Plan::Project { exprs, .. } => exprs
                            .get(i)
                            .is_some_and(|e| exprs[..i].iter().any(|x| x.same_as(e))),
                        _ => false,
                    };
                    let keys = c
                        .exported
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| !constant(*i) && !dup(*i))
                        .map(|(i, k)| {
                            let s = self.pick(k, Cx::Upper);
                            // (An InitPlan's value is a Param: shown bare.)
                            let param = s.starts_with("((InitPlan ") && s.ends_with(".col1)");
                            match s.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
                                Some(inner) if computed(i) || param => inner.to_string(),
                                _ => s,
                            }
                        })
                        .collect();
                    n.details.push(Detail::List("Group Key", keys));
                    n.details.push(Detail::HashAggInfo);
                    n
                };
                let cols = if c.node.cols.len() == c.exported.len() {
                    c.node.cols.clone()
                } else {
                    vec![ColInfo::default(); c.exported.len()]
                };
                let keys: Vec<ColInfo> = match on {
                    Some(on) => on
                        .iter()
                        .map(|e| match e {
                            Expr::Col(j, _) => cols.get(*j).cloned().unwrap_or_default(),
                            _ => ColInfo::default(),
                        })
                        .collect(),
                    None => cols
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| !param_free_constant(input, *i))
                        .map(|(_, c)| c.clone())
                        .collect(),
                };
                n.rows = self.num_groups(&keys, c.node.rows);
                // (estimate_num_groups: a volatile key with no columns makes every row a
                // group.)
                if let Plan::Project { exprs, .. } = &**input
                    && exprs.iter().any(|e| e.is_volatile() && e.columns().is_empty())
                    && on.is_none()
                {
                    n.rows = c.node.rows;
                }
                n.width = c.node.width;
                n.cols = cols;
                let key_ops = costs::CPU_OPERATOR_COST * keys.len() as f64;
                if on.is_some() && !hashed_on {
                    // cost_unique-like: one comparison per input row and key.
                    n.startup_cost = c.node.startup_cost;
                    n.total_cost = c.node.total_cost + c.node.rows * key_ops;
                } else {
                    // A hashed aggregation with no aggregates.
                    n.startup_cost = c.node.total_cost + c.node.rows * key_ops;
                    n.total_cost = n.startup_cost + n.rows * costs::CPU_TUPLE_COST;
                    let (s, t) = crate::sql::paths::Gucs::read(self.ctx).hashagg_spill(
                        c.node.rows,
                        c.node.width,
                        n.rows,
                        0,
                        0.0,
                    );
                    n.startup_cost += s;
                    n.total_cost += t;
                    // create_agg_path: its target's expressions (the computed keys) per
                    // group.
                    if let Plan::Project { exprs, .. } = &**input {
                        let per: f64 = exprs
                            .iter()
                            .filter(|e| !matches!(e, Expr::Col(..)))
                            .map(costs::qual_cost)
                            .sum();
                        n.total_cost += per * n.rows;
                    }
                }
                // Every key fixed by an equality with a constant (its distinct pathkeys all
                // redundant): the first row is the one distinct row (a Limit 1).
                // (So with DISTINCT ON keys the input fixes.)
                let on_fixed = !hashed_on && on.as_ref().is_some_and(|on| {
                    let fixed = super::paths::constant_columns(input);
                    !on.is_empty()
                        && on.iter().all(|e| crate::sql::explain::key_col_fixed(e, &fixed))
                });
                if (on.is_none() && distinct_keys_fixed(input)) || on_fixed {
                    // (Gathered: the workers stop at their first row too.)
                    let mut node = c.node;
                    if node.node_type == "Gather"
                        && node.children.len() == 1
                        && part.is_some()
                    {
                        let child = node.children.pop().expect("one child");
                        let mut l = limit_over(child, 1.0, 0.0);
                        l.relationship = Some("Outer");
                        node.children.push(l);
                    }
                    let l = limit_over(node, 1.0, 0.0);
                    return Built::passthrough(l, c.exported);
                }
                // Or (create_distinct_paths) a Unique over the input sorted on the keys, in
                // the order the input already has as far as it goes.
                // (The DISTINCT's keys as written, distinct_pathkeys: column, name.)
                let mut distinct_named: Vec<(usize, String)> = Vec::new();
                let sorted = if on.is_none() && !keys.is_empty() {
                    let named: Vec<(usize, String)> = c
                        .exported
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| !param_free_constant(input, *i))
                        .map(|(i, k)| (i, self.pick(k, Cx::Upper)))
                        .fold(Vec::new(), |mut v: Vec<(usize, String)>, (i, s)| {
                            if !v.iter().any(|x| x.1 == s) {
                                v.push((i, s));
                            }
                            v
                        });
                    let names: Vec<String> = named.iter().map(|x| x.1.clone()).collect();
                    distinct_named = named.clone();
                    let mut ord: Vec<String> = Vec::new();
                    // (In either direction: a backward scan's order serves as well.)
                    // (A semi join's side made unique is sorted whole, its input's order
                    // unused: create_unique_path costs cost_sort without pathkeys and
                    // create_unique_plan sorts.)
                    for o in c.node.order.iter().filter(|_| !semi_unique) {
                        let base = presorted_name(o);
                        if names.contains(&base) && !ord.iter().any(|k| presorted_name(k) == base) {
                            ord.push(o.clone());
                        } else {
                            break;
                        }
                    }
                    // An append relation whose members all come in an order the keys
                    // lead: merged in it (generate_orderedappend_paths).
                    let mut merged: Option<ExNode> = None;
                    if ord.is_empty()
                        && !semi_unique
                        && c.node.node_type == "Append"
                        && c.node.children.len() > 1
                        && let Some((keys, kids)) = self.merged_append_keys(&c.node, &named)
                    {
                        let mut ma = ExNode::new("Merge Append");
                        let list: Vec<String> = keys.iter().map(|&i| c.exported.get(i).map_or(String::new(), |n| self.pick(n, Cx::Upper))).collect();
                        let nk = kids.len() as f64;
                        let cmp = 2.0 * costs::CPU_OPERATOR_COST;
                        let logn = nk.log2();
                        ma.details.push(Detail::List("Sort Key", list.clone()));
                        ma.rows = c.node.rows;
                        ma.width = c.node.width;
                        ma.cols = c.node.cols.clone();
                        ma.key = c.node.key;
                        ma.output = c.node.output.clone();
                        ma.startup_cost = kids.iter().map(|k| k.startup_cost).sum::<f64>() + cmp * nk * logn;
                        ma.total_cost = ma.startup_cost
                            + kids.iter().map(|k| k.total_cost - k.startup_cost).sum::<f64>()
                            + ma.rows * cmp * logn
                            + costs::CPU_TUPLE_COST * 0.5 * ma.rows;
                        ma.children = kids;
                        ma.order = list.clone();
                        ord = list;
                        merged = Some(ma);
                    }
                    let common = ord.len();
                    let rest: Vec<String> = names
                        .iter()
                        .filter(|k| !ord.iter().any(|o| presorted_name(o) == **k))
                        .cloned()
                        .collect();
                    ord.extend(rest);
                    let g = crate::sql::paths::Gucs::read(self.ctx);
                    let merged_none = merged.is_none();
                    let mut input_node = merged.unwrap_or_else(|| c.node.clone());
                    if physical {
                        input_node.output = exact_out.clone();
                    }
                    if common < ord.len() {
                        let incremental = common > 0 && g.incremental_sort;
                        let mut sn = ExNode::new(if incremental { "Incremental Sort" } else { "Sort" });
                        // (A value computed from a `name` column sorts under "C".)
                        let shown: Vec<String> = ord
                            .iter()
                            .map(|k| {
                                let derived = named
                                    .iter()
                                    .find(|x| x.1 == presorted_name(k))
                                    .is_some_and(|x| matches!(&**input, Plan::Project { exprs, .. } if exprs.get(x.0).is_some_and(name_derived_c)));
                                if derived {
                                    let base = presorted_name(k);
                                    format!("{base} COLLATE \"C\"{}", &k[base.len()..])
                                } else {
                                    k.clone()
                                }
                            })
                            .collect();
                        sn.details.push(Detail::List("Sort Key", shown));
                        if incremental {
                            sn.details.push(Detail::List(
                                "Presorted Key",
                                ord[..common].iter().map(|k| presorted_name(k)).collect(),
                            ));
                            let pre_cols: Vec<ColInfo> = ord[..common]
                                .iter()
                                .filter_map(|k| named.iter().find(|x| x.1 == *k).map(|x| x.0))
                                .filter_map(|i| input_node.cols.get(i).cloned())
                                .collect();
                            let ng = if pre_cols.len() < common || pre_cols.iter().any(|x| x.var.is_none()) {
                                input_node.rows.clamp(1.0, costs::DEFAULT_NUM_DISTINCT)
                            } else {
                                self.num_groups(&pre_cols, input_node.rows)
                            };
                            (sn.startup_cost, sn.total_cost) = costs::incremental_sort_cost(
                                input_node.startup_cost,
                                input_node.total_cost,
                                input_node.rows,
                                input_node.width,
                                ng,
                                None,
                            );
                            sn.rows = input_node.rows;
                            sn.width = input_node.width;
                            sn.cols = input_node.cols.clone();
                        } else {
                            sn.details.push(Detail::SortInfo);
                            self.sort_costs(&mut sn, &input_node);
                            sn.disabled = !g.sort;
                        }
                        sn.key = input_node.key;
                        if self.o.verbose {
                            sn.output = input_node.output.clone();
                            // (Its input's target list computed there: the Sort refers to
                            // each value, a computed one parenthesized, `((a + 1))`.)
                            let computed: Vec<String> = c.internal.iter().map(|n| self.pick(n, Cx::Output)).collect();
                            // (Then the resjunk group keys its input carries, references too.)
                            if !physical
                                && merged_none
                                && sn.output.len() >= computed.len()
                                && sn.output[..computed.len()] == computed[..]
                                && c.exported.len() == computed.len()
                            {
                                let rest: Vec<String> = sn.output[computed.len()..].iter().map(|o| refer_to(o)).collect();
                                sn.output = c.exported.iter().map(|n| self.pick(n, Cx::Output)).collect();
                                sn.output.extend(rest);
                            }
                        }
                        sn.order = ord.clone();
                        sn.children.push(input_node.child("Outer"));
                        input_node = sn;
                    }
                    let mut u = ExNode::new("Unique");
                    // (Its input already in order: its rows as they come.)
                    u.unique_keys = Some(if common == ord.len() && ord.iter().any(|k| *k != presorted_name(k)) {
                        Vec::new()
                    } else {
                        ord.iter()
                            .filter_map(|k| named.iter().find(|x| x.1 == presorted_name(k)).map(|x| x.0))
                            .collect()
                    });
                    u.rows = n.rows;
                    u.width = n.width;
                    u.cols = n.cols.clone();
                    u.startup_cost = input_node.startup_cost;
                    u.total_cost = input_node.total_cost + input_node.rows * key_ops;
                    u.order = ord;
                    u.children.push(input_node.child("Outer"));
                    // (Or the input's other path, already in an order of the keys: a nested
                    // loop over a Gather Merge, say.)
                    if let Some(a) = in_alt.as_deref().filter(|_| !semi_unique)
                        && a.order.len() >= names.len()
                        && names.iter().all(|nm| a.order[..names.len()].iter().any(|o| uncollated(&presorted_name(o)) == *nm))
                    {
                        let aord: Vec<String> = a.order[..names.len()].to_vec();
                        let mut ua = ExNode::new("Unique");
                        ua.unique_keys = Some(
                            aord.iter()
                                .filter_map(|k| named.iter().find(|x| x.1 == uncollated(&presorted_name(k))).map(|x| x.0))
                                .collect(),
                        );
                        ua.rows = n.rows;
                        ua.width = n.width;
                        ua.cols = n.cols.clone();
                        ua.startup_cost = a.startup_cost;
                        ua.total_cost = a.total_cost + a.rows * key_ops;
                        ua.order = aord;
                        ua.children.push(a.clone().child("Outer"));
                        if parallel::cheaper(&ua, &u) {
                            u = ua;
                        }
                    }
                    n.disabled = !g.hashagg;
                    Some(u)
                } else {
                    None
                };
                n.children.push(c.node.child("Outer"));
                // The rows made distinct in the workers, then again over what they send.
                if on.is_none()
                    && !semi_unique
                    && let Some(u) = sorted.as_ref()
                    && let Some(pt) = part.as_deref()
                    && self.par.is_some()
                    && !keys.is_empty()
                    && let Some(best) = self.parallel_distinct(&n, u, pt, &part_others, &distinct_named, &keys, key_ops, match &**input {
                        // (create_agg_path: the computed keys per group.)
                        Plan::Project { exprs, .. } => exprs
                            .iter()
                            .filter(|e| !matches!(e, Expr::Col(..)))
                            .map(costs::qual_cost)
                            .sum(),
                        _ => 0.0,
                    }, {
                        // (A volatile key of no columns: every row a group.)
                        matches!(&**input, Plan::Project { exprs, .. }
                            if exprs.iter().any(|e| e.is_volatile() && e.columns().is_empty()))
                    })
                {
                    return Built::passthrough(best, c.exported);
                }
                if let Some(u) = sorted {
                    // (create_distinct_paths / create_unique_path: no hashed form when a
                    // key's type doesn't hash, grouping_is_hashable.)
                    let can_hash = {
                        let cat = self.ctx.catalog();
                        crate::sql::paths::analyzejoins::col_types(input)
                            .iter()
                            .all(|t| *t == DataType::Unknown || cat.type_hashable(t))
                    };
                    // (create_unique_path: hashed when it is enabled where the sort isn't,
                    // or cheaper in total.)
                    let sort_wins = if semi_unique {
                        let (dh, ds) = (parallel::disabled_nodes(&n), parallel::disabled_nodes(&u));
                        !(dh < ds || (dh == ds && n.total_cost < u.total_cost))
                    } else {
                        !parallel::unsorted_kept(&n, &u) || parallel::cheaper(&u, &n)
                    };
                    if !can_hash {
                        n = u;
                    } else if sort_wins {
                        let hashed = std::mem::replace(&mut n, u);
                        n.alt = Some(Box::new(hashed));
                    } else {
                        n.alt = Some(Box::new(u));
                    }
                }
                Built::passthrough(n, c.exported)
            }
            Plan::SetOp {
                kind: SetOpKind::Union,
                all,
                ..
            } => {
                let mut arms = Vec::new();
                union_arms(p, *all, &mut arms);
                let app = self.append(&arms);
                if *all {
                    return app;
                }
                let mut n = ExNode {
                    strategy: Some("Hashed"),
                    partial_mode: Some("Simple"),
                    ..ExNode::new("Aggregate")
                };
                n.title = "HashAggregate".into();
                let keys = app
                    .exported
                    .iter()
                    .map(|k| self.pick(k, Cx::Upper))
                    .collect();
                n.details.push(Detail::List("Group Key", keys));
                n.details.push(Detail::HashAggInfo);
                // No statistics for a union's output: every row may be distinct.
                let ncols = app.exported.len() as f64;
                n.rows = app.node.rows;
                n.width = app.node.width;
                n.cols = app.node.cols.clone();
                n.startup_cost =
                    app.node.total_cost + app.node.rows * costs::CPU_OPERATOR_COST * ncols;
                n.total_cost = n.startup_cost + n.rows * costs::CPU_TUPLE_COST;
                let g = crate::sql::paths::Gucs::read(self.ctx);
                let (s, t) = g.hashagg_spill(app.node.rows, app.node.width, n.rows, 0, 0.0);
                n.startup_cost += s;
                n.total_cost += t;
                n.disabled = !g.hashagg;
                // generate_union_paths: hashed when the columns' types hash; else, or when
                // cheaper, sorted: a Unique over the Append sorted, or over a Merge Append
                // of the arms each in order.
                let types = crate::sql::paths::analyzejoins::col_types(arms[0]);
                let can_hash = all_hashable(self.ctx, &types);
                let can_sort = types.iter().all(setop_sortable);
                let names: Vec<String> = app.exported.iter().map(|k| self.pick(k, Cx::Upper)).collect();
                let mut best: Option<ExNode> = None;
                if can_sort {
                    // (No columns: no sort.)
                    let sn = if names.is_empty() {
                        app.node.clone()
                    } else {
                        let mut sn = ExNode::new("Sort");
                        sn.details.push(Detail::List("Sort Key", names.clone()));
                        sn.details.push(Detail::SortInfo);
                        self.sort_costs(&mut sn, &app.node);
                        sn.disabled = !g.sort;
                        sn.key = app.node.key;
                        if self.o.verbose {
                            sn.output = app.exported.iter().map(|k| self.pick(k, Cx::Output)).collect();
                        }
                        sn.order = names.clone();
                        sn.children.push(app.node.clone().child("Outer"));
                        sn
                    };
                    let mut u = ExNode::new("Unique");
                    u.unique_keys = Some((0..app.exported.len()).collect());
                    u.rows = n.rows;
                    u.width = app.node.width;
                    u.cols = app.node.cols.clone();
                    u.startup_cost = sn.startup_cost;
                    u.total_cost = sn.total_cost + costs::CPU_OPERATOR_COST * ncols * sn.rows;
                    u.order = names.clone();
                    u.children.push(sn.child("Outer"));
                    best = Some(u);
                    // The Merge Append: each arm in the keys' order already, or sorted.
                    let kids_in: Vec<ExNode> = app.node.children.to_vec();
                    if kids_in.len() > 1 && app.node.node_type == "Append" && !names.is_empty() {
                        let mut kids = Vec::new();
                        let (mut startup, mut total, mut rows) = (0.0, 0.0, 0.0);
                        for k in kids_in {
                            let kn: Vec<String> = k.out_names.clone();
                            let presorted = !kn.is_empty() && k.order.len() >= kn.len() && k.order[..kn.len()] == kn[..];
                            let kid = if presorted {
                                k
                            } else {
                                let mut ks = ExNode::new("Sort");
                                ks.details.push(Detail::List("Sort Key", kn.clone()));
                                ks.details.push(Detail::SortInfo);
                                self.sort_costs(&mut ks, &k);
                                ks.disabled = !g.sort;
                                ks.key = k.key;
                                if self.o.verbose {
                                    ks.output = k.output.clone();
                                }
                                ks.order = kn;
                                ks.children.push(k.child("Outer"));
                                ks
                            };
                            startup += kid.startup_cost;
                            total += kid.total_cost;
                            rows += kid.rows;
                            kids.push(kid);
                        }
                        let nn = kids.len() as f64;
                        let cmp = 2.0 * costs::CPU_OPERATOR_COST;
                        let logn = nn.log2();
                        let mut ma = ExNode::new("Merge Append");
                        ma.details.push(Detail::List("Sort Key", names.clone()));
                        ma.rows = rows;
                        ma.width = app.node.width;
                        ma.cols = app.node.cols.clone();
                        ma.startup_cost = startup + cmp * nn * logn;
                        ma.total_cost = cmp * nn * logn
                            + total
                            + rows * cmp * logn
                            + costs::CPU_TUPLE_COST * 0.5 * rows;
                        ma.order = names.clone();
                        for k in kids {
                            ma.children.push(k.child("Member"));
                        }
                        let mut u = ExNode::new("Unique");
                        u.unique_keys = Some((0..app.exported.len()).collect());
                        u.rows = n.rows;
                        u.width = app.node.width;
                        u.cols = app.node.cols.clone();
                        u.startup_cost = ma.startup_cost;
                        u.total_cost = ma.total_cost + costs::CPU_OPERATOR_COST * ncols * ma.rows;
                        u.order = names.clone();
                        u.children.push(ma.child("Outer"));
                        // (Both in the columns' order: add_path keeps the one that
                        // dominates, fuzzily.)
                        if best.as_ref().is_none_or(|b| parallel::dominates(&u, b)) {
                            best = Some(u);
                        }
                    }
                }
                n.children.push(app.node.child("Outer"));
                match best {
                    Some(u) if !can_hash || parallel::cheaper(&u, &n) => {
                        let hashed = std::mem::replace(&mut n, u);
                        if can_hash {
                            n.alt = Some(Box::new(hashed));
                        }
                    }
                    Some(u) => n.alt = Some(Box::new(u)),
                    None => {}
                }
                Built::passthrough(n, app.exported)
            }
            Plan::SetOp {
                kind,
                all,
                left,
                right,
                ..
            } => {
                let mut l = self.node(left);
                let mut r = self.node(right);
                let cmd = match (kind, all) {
                    (SetOpKind::Intersect, false) => "Intersect",
                    (SetOpKind::Intersect, true) => "Intersect All",
                    (_, false) => "Except",
                    (_, true) => "Except All",
                };
                let mut n = ExNode {
                    strategy: Some("Hashed"),
                    ..ExNode::new("SetOp")
                };
                n.title = format!("HashSetOp {cmd}");
                n.target.push(("Command", cmd.into(), false));
                // generate_nonunion_paths: the inputs' group counts; INTERSECT puts the
                // input with fewer groups on the left.
                let lgroups = self.num_groups(&l.node.cols, l.node.rows);
                let rgroups = self.num_groups(&r.node.cols, r.node.rows);
                let exported = l.exported.clone();
                let mut groups = lgroups;
                if *kind == SetOpKind::Intersect && lgroups > rgroups {
                    std::mem::swap(&mut l, &mut r);
                    groups = rgroups;
                }
                n.rows = match (kind, all) {
                    (_, false) => groups,
                    (SetOpKind::Intersect, true) => l.node.rows.min(r.node.rows),
                    _ => l.node.rows,
                };
                // The output columns' types' widths (generate_setop_tlist).
                n.width = l
                    .node
                    .cols
                    .iter()
                    .map(|c| match &c.var {
                        Some(v) if v.expr.is_none() && v.col < v.table.columns.len() => {
                            type_width(&v.table.columns[v.col].ty)
                        }
                        _ => c.width,
                    })
                    .sum::<u32>()
                    .max(if l.node.cols.is_empty() { l.node.width } else { 0 });
                let ncols = l.exported.len().max(1) as f64;
                n.startup_cost = l.node.total_cost
                    + r.node.total_cost
                    + costs::CPU_OPERATOR_COST * (l.node.rows + r.node.rows) * ncols;
                n.total_cost = n.startup_cost + n.rows * costs::CPU_OPERATOR_COST;
                // generate_nonunion_paths: hashed (enable_hashagg, the table fitting in
                // hash_mem), or both inputs sorted (create_setop_path).
                let g = crate::sql::paths::Gucs::read(self.ctx);
                let types = crate::sql::paths::analyzejoins::col_types(left);
                let can_hash = all_hashable(self.ctx, &types);
                let can_sort = types.iter().all(setop_sortable);
                let entry = ((n.width as f64 / 8.0).ceil() * 8.0) + 16.0;
                n.disabled = !g.hashagg || entry * groups > g.work_mem * g.hash_mem_multiplier * 1024.0;
                let sorted_in = |b: &mut Self, c: &Built| -> ExNode {
                    let names: Vec<String> = c.exported.iter().map(|k| b.pick(k, Cx::Upper)).collect();
                    if c.node.order.len() >= names.len() && c.node.order[..names.len()] == names[..] {
                        return c.node.clone();
                    }
                    let mut sn = ExNode::new("Sort");
                    sn.details.push(Detail::List("Sort Key", names.clone()));
                    sn.details.push(Detail::SortInfo);
                    b.sort_costs(&mut sn, &c.node);
                    sn.disabled = !g.sort;
                    sn.key = c.node.key;
                    if b.o.verbose {
                        sn.output = c.node.output.clone();
                    }
                    sn.order = names;
                    sn.children.push(c.node.clone().child("Outer"));
                    sn
                };
                let sl = sorted_in(self, &l);
                let sr = sorted_in(self, &r);
                let mut so = n.clone();
                so.strategy = Some("Sorted");
                so.title = format!("SetOp {cmd}");
                so.disabled = false;
                so.startup_cost = sl.startup_cost + sr.startup_cost;
                so.total_cost = sl.total_cost
                    + sr.total_cost
                    + costs::CPU_OPERATOR_COST * (sl.rows + sr.rows) * ncols
                    + costs::CPU_OPERATOR_COST * n.rows;
                so.order = sl.order.clone();
                so.unique_keys = Some((0..l.exported.len()).collect());
                so.children.push(sl.child("Outer"));
                so.children.push(sr.child("Inner"));
                n.children.push(l.node.child("Outer"));
                n.children.push(r.node.child("Inner"));
                if can_sort && (!can_hash || parallel::cheaper(&so, &n)) {
                    let hashed = std::mem::replace(&mut n, so);
                    if can_hash {
                        n.alt = Some(Box::new(hashed));
                    }
                } else if can_sort {
                    n.alt = Some(Box::new(so));
                }
                Built::passthrough(n, exported)
            }
            // UPDATE / DELETE of an inheritance tree its source reads members of.
            Plan::Update(up)
                if up.inh
                    && up.members.iter().map(|m| &m.rules.table).chain([&up.rules.table])
                        .any(|t| target_scan(&up.source, t.oid).is_some()) =>
            {
                let members: Vec<Arc<crate::catalog::Table>> =
                    up.members.iter().map(|m| m.rules.table.clone()).collect();
                let returning = shown_returning(&up.returning);
                self.inherited_dml(key, "Update", &up.rules.table, &members, &up.sets, &returning, up.old_new.as_ref(), up.sys, &up.source)
            }
            Plan::Delete(dp)
                if dp.inh
                    && dp.members.iter().map(|m| &m.table).chain([&dp.table])
                        .any(|t| target_scan(&dp.source, t.oid).is_some()) =>
            {
                let members: Vec<Arc<crate::catalog::Table>> =
                    dp.members.iter().map(|m| m.table.clone()).collect();
                let returning = shown_returning(&dp.returning);
                self.inherited_dml(key, "Delete", &dp.table, &members, &[], &returning, dp.old_new.as_ref(), dp.sys, &dp.source)
            }
            // A single member needs no Append (Postgres removes it).
            Plan::Append {
                inputs,
                inh: Some(_),
                ..
            } if inputs.len() == 1 => {
                let mut c = self.node(&inputs[0]);
                // (Its columns are still the partitioned table's: its inherited statistics.)
                if let Some(parent) = pwagg::member_scan_of(&inputs[0])
                    .and_then(|s| s.part.as_ref())
                    .and_then(|l| l.last())
                    .map(|l| l.parent.clone())
                {
                    for (k, col) in c.node.cols.iter_mut().enumerate() {
                        if parent.columns.get(k).is_none_or(|pc| pc.dropped) {
                            continue;
                        }
                        if let Some(v) = col.var.as_mut()
                            && v.expr.is_none()
                            && !v.inh
                            && v.table.columns.get(v.col).map(|c| &c.name) == parent.columns.get(k).map(|c| &c.name)
                        {
                            v.table = parent.clone();
                            v.col = k;
                            v.inh = true;
                        }
                    }
                }
                c
            }
            Plan::Append { inputs, inh, pw, .. } => {
                // (Below a partitionwise join's Append: its child joins.)
                let _pw = pw.map(|_| crate::scoped::set(&UNDER_PW, true, "explain::UNDER_PW.set"));
                let mut direct = Vec::new();
                for i in inputs.iter() {
                    let mut x = &**i;
                    while let Plan::Sort { input, .. } = x {
                        x = input;
                    }
                    // (A member's column mapping over its scan: the scan is the member.)
                    if let Plan::Project { input, exprs } = x
                        && matches!(&**input, Plan::Scan(_))
                        && exprs.iter().all(|e| matches!(e, Expr::Col(..)))
                    {
                        x = input;
                    }
                    // (A UNION ALL's arm of one table without quals of its own is an
                    // appendrel member too, is_safe_append_member: no physical target
                    // list.)
                    if matches!(x, Plan::Scan(s) if inh.is_some() || s.filter.is_none()) {
                        direct.push(addr(x));
                    }
                }
                INH_SCANS.with(|m| m.borrow_mut().extend(direct.iter().copied()));
                crate::scoped::checkpoint("explain::INH_SCANS");
                // Partitions pruned when the executor starts aren't shown (a UNION ALL's
                // too).
                let kept = if inh.is_some() || !inputs.iter().any(|i| i.writes()) {
                    crate::exec::nodes::runtime_pruned(
                        inputs,
                        crate::sql::binder::partition::Known::Start,
                        self.ctx,
                    )
                } else {
                    inputs.clone()
                };
                let arms: Vec<&Arc<Plan>> = kept.iter().collect();
                // (Every member pruned as the executor starts: the plan is still the one
                // made for them all, its parallel form too, without members to show.)
                let all_pruned = arms.is_empty() && inputs.len() > 1 && inh.is_some();
                let mut b = if all_pruned {
                    let all: Vec<&Arc<Plan>> = inputs.iter().collect();
                    let mut b = self.append_of(&all, true);
                    b.node.children.clear();
                    if let Some(pt) = &mut b.node.partial {
                        pt.children.clear();
                    }
                    b
                } else {
                    // (flatten_simple_union_all: a query level's own UNION ALL takes its
                    // leftmost arm's entry as the appendrel's parent, the arm a copy of it
                    // after the others.)
                    let leftmost_last = matches!(&**p, Plan::Append { alias: Some(a), .. } if a == "*SELECT* 1")
                        && arms.first().is_some_and(|a| Arc::ptr_eq(a, &inputs[0]));
                    let mut b = self.append_of_ranked(&arms, inh.is_some(), leftmost_last);
                    // (The plan was made, and costed, for every member: pruning removes
                    // them only as the executor starts.)
                    if inh.is_some() && arms.len() < inputs.len() {
                        for i in inputs.iter().filter(|i| !kept.iter().any(|k| Arc::ptr_eq(k, i))) {
                            let e = estimate(i, self.ctx);
                            b.node.rows += e.rows;
                            b.node.total_cost += e.total + e.rows * costs::CPU_TUPLE_COST * 0.5;
                        }
                    }
                    b
                };
                INH_SCANS.with(|m| {
                    let mut m = m.borrow_mut();
                    for d in &direct {
                        m.remove(d);
                    }
                });
                // Above a partitionwise join's Append its columns are the partitioned
                // tables' (the parents' names), not the first child join's partitions'.
                if inh.is_none() && let Some(first) = arms.first() {
                    let mut map: Vec<(String, String)> = Vec::new();
                    let mut scans = Vec::new();
                    pw_named_scans(first, &mut scans);
                    for sp in scans {
                        let Plan::Scan(sc) = sp else { continue };
                        if let Some(name) = self.refnames.get(&addr(sp)) {
                            let parent = sc.alias.clone().unwrap_or_else(|| sc.table.name.clone());
                            map.push((format!("{}.", quote_ident(name)), format!("{}.", quote_ident(&parent))));
                        }
                    }
                    for n in b.exported.iter_mut() {
                        for (c, par) in &map {
                            n.full = replace_qualifier(&n.full, c, par);
                        }
                    }
                }
                // Above an inheritance Append its columns are the parent's (the appendrel's
                // Vars), not its first member's. (A partition's own partitions joined as one
                // in a partitionwise child join, numbered as that partition: its first
                // member's, as Postgres shows them.)
                let deep_member = {
                    let mut first = arms.first().map(|a| &***a);
                    while let Some(Plan::Project { input, .. } | Plan::Sort { input, .. }) = first {
                        first = Some(&**input);
                    }
                    matches!(first, Some(Plan::Scan(sc)) if sc.part.as_ref().is_some_and(|l| l.len() >= 2))
                };
                let sub_partition = deep_member
                    && UNDER_PW.get()
                    && inh.as_ref().is_some_and(|base| {
                        self.refnames.get(&key).is_some_and(|r| {
                            r.strip_prefix(&format!("{}_", base)).is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
                        })
                    });
                if inh.is_some()
                    && !sub_partition
                    && let Some(parent) = self.refnames.get(&key)
                {
                    let parent = quote_ident(parent);
                    // (Expressions the members compute: over the parent's columns too.)
                    let mut first = arms.first().map(|a| &***a);
                    while let Some(Plan::Project { input, .. } | Plan::Sort { input, .. }) = first {
                        first = Some(&**input);
                    }
                    let member = first
                        .filter(|f| matches!(f, Plan::Scan(_)))
                        .and_then(|f| self.refnames.get(&addr(f)))
                        .map(|m| format!("{}.", quote_ident(m)))
                        .filter(|m| *m != format!("{parent}."));
                    for n in b.exported.iter_mut() {
                        if n.full != n.plain && n.full.ends_with(&format!(".{}", n.plain))
                            // (Every member pruned: none named, the columns still the parent's.)
                            || all_pruned && n.full == n.plain
                        {
                            n.full = format!("{parent}.{}", n.plain);
                        } else if let Some(m) = &member
                            && n.full != n.plain
                        {
                            n.full = replace_qualifier(&n.full, m, &format!("{parent}."));
                        }
                    }
                }
                // (Run-time pruning's exec steps: the partition keys' clauses over a nested
                // loop's values, fixed for one run (register_partpruneinfo) and initialized
                // by the Append before its members: a SubPlan there is the Append's when no
                // member's shows it.)
                if inh.is_some() && kept.len() > 1 && partition_pruning_on(self.ctx) {
                    let vals = exec_prune_values(&kept);
                    if vals.iter().any(|v| self.inline_sub_params(v).is_some_and(|x| has_subselect(&x))) {
                        let saved = self.num_exec.replace(1.0);
                        for v in &vals {
                            let _ = self.name(v, &[], &[]);
                        }
                        self.num_exec = saved;
                    }
                }
                if kept.len() < inputs.len() {
                    let k = inputs.len() - kept.len();
                    add_removed(&mut b.node.details, k);
                    if let Some(pt) = &mut b.node.partial {
                        add_removed(&mut pt.details, k);
                    }
                }
                // (Its members' rows locked: one LockRows above the Append.)
                if inh.is_some()
                    && !b.node.children.is_empty()
                    && b.node.children.iter().all(|c| c.node_type == "LockRows" && c.children.len() == 1)
                {
                    let kids = std::mem::take(&mut b.node.children);
                    b.node.children = kids
                        .into_iter()
                        .map(|mut k| {
                            let rel = k.relationship;
                            let mut inner = k.children.pop().expect("one child");
                            inner.relationship = rel;
                            inner
                        })
                        .collect();
                    b.node.partial = None;
                    b.node.key.get_or_insert(key);
                    let mut l = lock_rows(std::mem::take(&mut b.node));
                    l.key = Some(key);
                    b.node = l;
                }
                b
            }
            // (An inlined set-returning SQL function's FROM item: its query, the empty
            // row it is joined to gone.)
            Plan::Join {
                kind: JoinKind::Inner,
                left,
                right,
                cond: None,
                hash: None,
                params,
                nleft: 0,
                meta,
                ..
            } if meta.from_call.is_none() && matches!(&**left, Plan::One) => {
                // (Its arguments read from the items to its left: shown as they are, in
                // place of the parameters, as substitute_actual_srf_parameters puts them.)
                if params.is_empty() {
                    return self.node(right);
                }
                let names: Vec<String> = params.iter().map(|e| self.name(e, &[], &[]).full).collect();
                self.outer.insert(0, names);
                self.outer_depth.insert(0, self.depth);
                let b = self.node(right);
                self.outer.remove(0);
                self.outer_depth.remove(0);
                b
            }
            Plan::Join { params, meta, .. } if meta.from_call.is_some() => {
                let fc = meta.from_call.as_ref().expect("checked");
                let refname = self
                    .refnames
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| fc.alias.clone());
                let mut n = ExNode::new("Function Scan");
                // (A call the planner replaced is no function's: only the alias shows.)
                match &fc.call {
                    None => self.target(
                        &mut n,
                        Some("Function Name"),
                        Some(fc.def.name),
                        Some(&fc.schema),
                        &refname,
                    ),
                    Some(_) => self.target(&mut n, None, None, None, &refname),
                }
                let names: Vec<Name> = fc.cols.iter().map(|c| Name::col(&refname, c)).collect();
                if self.o.verbose {
                    let ty = match &fc.def.ret {
                        crate::sql::func::Ret::Fixed(t) => t.clone(),
                        _ => DataType::Unknown,
                    };
                    let call = fc
                        .call
                        .clone()
                        .unwrap_or_else(|| Expr::Call(fc.def.clone(), params.clone(), ty));
                    let t = self.show(&call, &[], Cx::Scan);
                    n.details.push(Detail::Text("Function Call", t));
                }
                // cost_functionscan: the call once, then a tuple per row.
                n.rows = fc.rows;
                n.startup_cost = if fc.call.is_some() { 0.0 } else { fc.cost * costs::CPU_OPERATOR_COST };
                n.total_cost = n.startup_cost + n.rows * costs::CPU_TUPLE_COST;
                n.width = fc.width;
                Built::passthrough(n, names)
            }
            Plan::Join {
                kind,
                left,
                right,
                cond,
                hash,
                params,
                meta,
                ..
            } => {
                let saved = OUTER_JOIN_SIDE.with(|o| o.get());
                let b = self.join(
                    key,
                    *kind,
                    left,
                    right,
                    cond.as_ref(),
                    hash.as_ref(),
                    params,
                    meta,
                );
                OUTER_JOIN_SIDE.with(|o| o.set(saved));
                b
            }
            // A statement a rule made of another command (a DELETE done as an UPDATE) that
            // returns the statement's RETURNING rows: Postgres runs it as the query (its count
            // isn't the statement's: the engine reads it as a CTE for that).
            Plan::With { ctes, body, names }
                if names.len() == 1
                    && names[0] == "*RULE*"
                    && matches!(&**body, Plan::CteScan { id, .. } if ctes.first().is_some_and(|c| c.0 == *id)) =>
            {
                self.node(&ctes[0].1)
            }
            Plan::With { ctes, body, names } => {
                for ((_, cte), name) in ctes.iter().zip(names) {
                    let mut node = self.level(cte);
                    let cols = if node.cols.is_empty() {
                        vec![]
                    } else {
                        node.cols
                            .iter()
                            .map(|c| ColInfo {
                                // (A column of a relation: its statistics, as
                                // examine_simple_variable finds them through the CTE.)
                                var: c.var.clone().filter(|v| v.expr.is_none()),
                                distinct: c.distinct.or(
                                    (node.rows < costs::DEFAULT_NUM_DISTINCT).then(|| costs::clamp_rows(node.rows)),
                                ),
                                ..c.clone()
                            })
                            .collect()
                    };
                    cte_push(name.clone(), node.rows, cols.clone(), plan_output_order(cte, self.ctx));
                    self.ctes.insert(name.clone(), (node.rows, cols));
                    self.next_plan_id(None, self.events.len(), false);
                    node.relationship = Some("InitPlan");
                    node.subplan_name = Some(format!("CTE {}", quote_ident(name)));
                    self.inits.last_mut().expect("query level").push(node);
                }
                // (Estimates made apart from this builder see them too.)
                let b = self.node(body);
                pop_cte_estimates(ctes.len());
                b
            }
            Plan::CteScan {
                name, alias, cols, ..
            }
            | Plan::WorkScan {
                name, alias, cols, ..
            } => {
                let refname = self
                    .refnames
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| alias.clone());
                let mut n = ExNode::new(if matches!(&**p, Plan::CteScan { .. }) {
                    "CTE Scan"
                } else {
                    "WorkTable Scan"
                });
                self.target(&mut n, Some("CTE Name"), Some(name), None, &refname);
                // cost_ctescan: two tuple costs per row (the tuplestore, the scan).
                let work_key = match &**p {
                    Plan::WorkScan { id, .. } => Some(format!("\u{1}{id}")),
                    _ => None,
                };
                let known = match &work_key {
                    Some(k) => CTE_ESTIMATES.with(|m| {
                        m.borrow().iter().rev().find(|x| x.0 == *k).map(|x| (x.1, x.2.clone()))
                    }),
                    None => self.ctes.get(name).cloned().or_else(|| {
                        CTE_ESTIMATES.with(|m| {
                            m.borrow().iter().rev().find(|x| x.0 == *name).map(|x| (x.1, x.2.clone()))
                        })
                    }),
                };
                match known.as_ref() {
                    Some((rows, ccols)) => {
                        n.rows = *rows;
                        if ccols.len() == cols.len() {
                            n.cols = ccols.clone();
                            // (The width of the columns read above: the relation's target.)
                            let needed = self.needed.get(&key).cloned();
                            n.width = ccols
                                .iter()
                                .enumerate()
                                .filter(|(i, _)| needed.as_ref().is_none_or(|m| m.get(*i).copied().unwrap_or(true)))
                                .map(|(_, c)| c.width)
                                .sum();
                        } else {
                            n.width = 4 * cols.len() as u32;
                        }
                    }
                    _ => {
                        n.rows = 1000.0;
                        n.width = 4 * cols.len() as u32;
                    }
                }
                n.total_cost = n.rows * 2.0 * costs::CPU_TUPLE_COST;
                let names: Vec<Name> = cols.iter().map(|c| Name::col(&refname, c)).collect();
                self.whole_vars.insert(quote_ident(&refname), cols.len());
                if matches!(&**p, Plan::CteScan { .. }) {
                    n.order = cte_order(name)
                        .iter()
                        .map(|k| self.show(&k.expr, &names, Cx::Upper) + &sort_suffix(k))
                        .collect();
                }
                Built::passthrough(n, names)
            }
            Plan::Recursive {
                id, base, recursive, ..
            } => {
                // (Each term is a query level of its own: its InitPlans, its CTEs', are its
                // top node's.)
                let term = |this: &mut Self, p: &Arc<Plan>| {
                    let before = this.inits.last().map_or(0, |v| v.len());
                    let mut b = this.node(p);
                    if let Some(level) = this.inits.last_mut()
                        && level.len() > before
                    {
                        let mut moved: Vec<ExNode> = level.drain(before..).collect();
                        for i in &mut moved {
                            if !i.charged {
                                i.charged = true;
                                b.node.startup_cost += i.init_charge();
                                b.node.total_cost += i.init_charge();
                            }
                        }
                        moved.append(&mut b.node.initplans);
                        b.node.initplans = moved;
                    }
                    b
                };
                let b = term(self, base);
                // (The worktable: ten times the non-recursive term, set_cte_size_estimates.)
                let wcols: Vec<ColInfo> = b
                    .node
                    .cols
                    .iter()
                    .map(|c| ColInfo {
                        width: c.width,
                        var: None,
                        distinct: (b.node.rows * 10.0 < costs::DEFAULT_NUM_DISTINCT)
                            .then(|| costs::clamp_rows(b.node.rows * 10.0)),
                    })
                    .collect();
                cte_push(format!("\u{1}{id}"), b.node.rows * 10.0, wcols, Vec::new());
                let r = term(self, recursive);
                pop_cte_estimates(1);
                // cost_recursive_union: about ten iterations of the recursive term, and a
                // tuple cost per row for the tuplestores.
                let mut n = ExNode::new("Recursive Union");
                n.rows = b.node.rows + r.node.rows * 10.0;
                n.width = b.node.width.max(r.node.width);
                n.startup_cost = b.node.startup_cost;
                n.total_cost = b.node.total_cost + r.node.total_cost * 10.0 + costs::CPU_TUPLE_COST * n.rows;
                n.cols = b.node.cols.iter().map(|c| ColInfo { width: c.width, var: None, distinct: None }).collect();
                n.children.push(b.node.child("Outer"));
                n.children.push(r.node.child("Inner"));
                Built::passthrough(n, b.exported)
            }
            Plan::EnrScan {
                name, alias, cols, ..
            } => {
                let refname = self
                    .refnames
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| alias.clone().unwrap_or_else(|| name.clone()));
                // PG names no target for a Named Tuplestore Scan; its size is the
                // tuplestore's actual row count (set_namedtuplestore_size_estimates).
                let mut n = ExNode::new("Named Tuplestore Scan");
                let enr = self
                    .ctx
                    .enrs
                    .borrow()
                    .iter()
                    .rev()
                    .find(|e| e.name == *name)
                    .cloned();
                let tuples = enr.as_ref().map_or(1000.0, |e| e.rows.len() as f64);
                n.rows = tuples.max(1.0);
                n.total_cost = tuples * costs::CPU_TUPLE_COST;
                if let Some(e) = &enr {
                    n.width = e.cols.iter().map(|(_, t)| costs::type_width(t)).sum();
                }
                let names = cols.iter().map(|c| Name::col(&refname, c)).collect();
                Built::passthrough(n, names)
            }
            Plan::LockRows { input, marks, width } => {
                // The level's other row marks, which the engine needs no column for: a semi
                // join's inner relation's ctid (ROW_MARK_REFERENCE), a function's, VALUES'
                // or a CTE's whole row (ROW_MARK_COPY), resjunk up to the LockRows.
                let mut semi = Vec::new();
                let mut copies = Vec::new();
                if self.o.verbose {
                    lock_level_marks(input, false, &mut semi, &mut copies);
                    self.rowmarks.extend(semi.iter().copied());
                }
                let mut c = {
                    let n = UNDER_LOCK_ROWS.get() + 1;
                    let _g = crate::scoped::set(&UNDER_LOCK_ROWS, n, "explain::UNDER_LOCK_ROWS");
                    self.node(input)
                };
                // An inheritance tree's row mark has its tableoid too (expand_inherited_rtentry
                // adds it after the target list's other junk), from each member's scan up.
                let mut inh: Vec<(String, Vec<String>)> = Vec::new();
                if self.o.verbose {
                    let mut trees = Vec::new();
                    inherited_marks(input, &mut trees);
                    for (parent, members) in trees {
                        let members: Vec<String> =
                            members.into_iter().map(|m| quote_ident(&self.refname(m))).collect();
                        add_tableoid(&mut c.node, &quote_ident(&parent), &members);
                        inh.push((quote_ident(&parent), members));
                    }
                }
                if self.o.verbose && (!semi.is_empty() || !copies.is_empty()) {
                    // (preprocess_rowmarks: the locked relations' marks in the order the
                    // locking clauses name them, then the other relations' ctids and whole
                    // rows in range table order; the inheritance parents' tableoids after
                    // all.)
                    for (a, p) in copies {
                        let name = format!("{}.*", quote_ident(&self.refname(p)));
                        self.rowmark_junk.insert(a, vec![((0, leaf_rti(p)), name)]);
                    }
                    let junk = junk_in_keyed(&c.node, &self.rowmark_junk);
                    let mut top = c.node.output.clone();
                    if top.len() == *width + marks.len() && marks.iter().all(|m| m.col < top.len()) {
                        // (The level's columns, then its marks' keys as `rowmarks::hoist`
                        // put them: the locked ones first.)
                        let mut locked = Vec::new();
                        let mut others = Vec::new();
                        for m in marks.iter() {
                            match m.lock {
                                Some(_) => locked.push((m.col, top[m.col].clone())),
                                None => others.push((m.rti, top[m.col].clone())),
                            }
                        }
                        locked.sort_by_key(|l| l.0);
                        let mut after = Vec::new();
                        for ((kind, rti), j) in junk {
                            if top.contains(&j) {
                                continue;
                            }
                            if kind == 0 {
                                others.push((rti, j));
                            } else {
                                after.push(j);
                            }
                        }
                        others.sort_by_key(|o| o.0);
                        top.truncate(*width);
                        top.extend(locked.into_iter().map(|l| l.1));
                        top.extend(others.into_iter().map(|o| o.1));
                        top.extend(after);
                    } else {
                        top.extend(junk.into_iter().map(|j| j.1).filter(|j| !c.node.output.contains(j)));
                    }
                    let tableoids: Vec<String> = inh.iter().map(|(p, _)| format!("{p}.tableoid")).collect();
                    top.sort_by_key(|o| tableoids.iter().position(|t| t == o).map_or(0, |i| i + 1));
                    // (A member's scan returns its parent's reltarget, the tableoid last.)
                    let mut ranks = Vec::new();
                    for t in &top {
                        ranks.push(t.clone());
                        for (parent, members) in &inh {
                            if let Some(col) = t.strip_prefix(&format!("{parent}."))
                                && !tableoids.contains(t)
                            {
                                ranks.extend(members.iter().map(|m| format!("{m}.{col}")));
                            }
                        }
                    }
                    let mut last = Vec::new();
                    for (parent, members) in &inh {
                        last.extend(members.iter().map(|m| format!("{m}.tableoid")));
                        last.push(format!("{parent}.tableoid"));
                    }
                    for ch in &mut c.node.children {
                        junk_layout(ch, &ranks, &last, &self.rowmark_junk, &HashMap::new());
                    }
                    c.node.output = top;
                }
                Built::passthrough(lock_rows(c.node), c.exported)
            }
            Plan::Insert(ip) => {
                let returning = shown_returning(&ip.returning);
                let mut c = self.node(&ip.source);
                let refname = self.refname(p);
                let t = &ip.rules.table;
                // A FROM subquery not pulled up whose row isn't the new row as it is (columns
                // left to their defaults, or in another order): the Subquery Scan computes
                // the new row (trivial_subqueryscan).
                if let Plan::Barrier { alias, cols, secure: false, .. } = &*ip.source
                    && !alias.is_empty()
                    && c.node.node_type != "Subquery Scan"
                    && ip.route.is_none()
                    && ip.copy.is_none()
                    && cols.len() == c.exported.len()
                    && cols.len() == ip.target.len()
                    && {
                        let live: Vec<usize> = t.columns.iter().enumerate().filter(|(_, c)| !c.dropped).map(|(i, _)| i).collect();
                        ip.target != live
                    }
                {
                    let names: Vec<Name> = cols.iter().map(|col| Name::col(alias, col)).collect();
                    let saved = self.sub_alias.replace(Arc::new((alias.clone(), cols.clone())));
                    c = self.subquery_scan(c, addr(&ip.source));
                    self.sub_alias = saved;
                    c.internal = names.clone();
                    c.exported = names;
                }
                // VERBOSE: the source computes the whole new row, defaults included.
                if self.o.verbose
                    && ip.route.is_none()
                    && ip.copy.is_none()
                    && matches!(c.node.node_type, "Result" | "Values Scan" | "Subquery Scan")
                    && c.internal.len() == ip.target.len()
                {
                    let mut out = Vec::new();
                    for (i, col) in t.columns.iter().enumerate() {
                        if col.dropped {
                            continue;
                        }
                        out.push(match ip.target.iter().position(|&x| x == i) {
                            Some(k) => self.pick(&c.internal[k], Cx::Output),
                            None => match ip.rules.defaults.get(i).cloned().flatten() {
                                Some(d) => self.show(&d, &[], Cx::Output),
                                None => self.show(&Expr::Const(crate::types::Value::Null, col.ty.clone()), &[], Cx::Output),
                            },
                        });
                    }
                    c.node.output = out;
                }
                let mut n = self.modify("Insert", t, &refname, &c.node);
                self.check_option_subplans(&mut n, &ip.rules.view_checks, t, &refname);
                // (RETURNING's sub-selects show without VERBOSE too: its list is shown to
                // find them.)
                if !returning.is_empty() {
                    let names = self.table_names(t, &refname);
                    // (The source is a range table entry of its own: columns qualified.)
                    let names: Vec<Name> = names.into_iter().map(|n| Name::same(n.full)).collect();
                    let names = old_new_names(names, ip.old_new.as_ref(), t);
                    let out = returning
                        .iter()
                        .map(|e| self.show(&old_new_rows(e, ip.old_new.as_ref()), &names, Cx::Output))
                        .collect();
                    if self.o.verbose {
                        n.output = out;
                    }
                }
                if let Some(oc) = &ip.on_conflict {
                    let names = self.table_names(t, &refname);
                    match &oc.action {
                        ConflictAction::Nothing => n
                            .details
                            .push(Detail::Text("Conflict Resolution", "NOTHING".into())),
                        ConflictAction::Update { .. } => n
                            .details
                            .push(Detail::Text("Conflict Resolution", "UPDATE".into())),
                    }
                    if !oc.arbiters.is_empty() {
                        let cat = self.ctx.catalog();
                        let ixs = oc
                            .arbiters
                            .iter()
                            .filter_map(|o| cat.index(*o).map(|i| i.name.clone()))
                            .collect();
                        n.details
                            .push(Detail::List("Conflict Arbiter Indexes", ixs));
                    }
                    if let ConflictAction::Update {
                        filter: Some(f), ..
                    } = &oc.action
                    {
                        let mut both = names.clone();
                        both.extend(t.columns.iter().map(|c| Name::col("excluded", &c.name)));
                        let text = self.show(f, &both, Cx::Upper);
                        n.details.push(Detail::Text("Conflict Filter", text));
                    }
                }
                self.returning(&mut n, &returning, t, &c.node);
                n.children.push(c.node.child("Outer"));
                Built::passthrough(n, Vec::new())
            }
            Plan::Update(up) => {
                let returning = shown_returning(&up.returning);
                // (Over a join the target's scan returns what is read of it and its ctid:
                // no physical target list.)
                let joined = !matches!(&*up.source, Plan::Scan(_));
                UNDER_LOCK_ROWS.with(|u| u.set(u.get() + joined as usize));
                self.dml_copy_marks(&up.source);
                let mut c = self.node(&up.source);
                UNDER_LOCK_ROWS.with(|u| u.set(u.get() - joined as usize));
                let t = &up.rules.table;
                // (No row of it read: the name the statement gives it.)
                let refname = target_scan(&up.source, t.oid)
                    .map(|s| self.refname(s))
                    .or_else(|| self.refnames.get(&key).cloned())
                    .unwrap_or_else(|| up.alias.clone().unwrap_or_else(|| t.name.clone()));
                // (The node computing the new values pays for their sub-selects, per row:
                // its target list's cost_subplan, as a SELECT list's.)
                if c.node.is_scan() || c.node.is_join() {
                    let sets: Vec<Expr> = up.sets.iter().map(|(_, e)| e.clone()).collect();
                    let (sp_startup, sp_per) = subplan_costs(&sets, self.ctx);
                    c.node.startup_cost += sp_startup;
                    c.node.total_cost += sp_startup + c.node.rows * sp_per;
                    // (And for their operators, cost_qual_eval.)
                    let per: f64 = sets.iter().map(costs::qual_cost).sum();
                    c.node.total_cost += c.node.rows * per;
                }
                // A join computes the final target list (apply_scanjoin_target_to_paths): the
                // new values (a column's width, else its type's), the target row's identity
                // (its ctid; an inheritance tree's tableoid too), the other relations' row
                // marks (ctids, whole rows) and what RETURNING reads of them.
                if c.node.is_join() && up.route.is_none() && up.members.is_empty() {
                    let cat = self.ctx.catalog();
                    let ncols = t.columns.len();
                    let col_w = |i: usize, ty: &DataType| -> u32 {
                        if i < ncols {
                            costs::col_width(&cat, t, i)
                        } else {
                            c.node.cols.get(i).map_or_else(|| type_width(ty), |x| x.width)
                        }
                    };
                    let mut w: u32 = up
                        .sets
                        .iter()
                        .map(|(_, e)| match e {
                            Expr::Col(i, ty) => col_w(*i, ty),
                            e => type_width(&e.ty()),
                        })
                        .sum();
                    let inherited = up.inh || cat.class_has_subclass.contains(&t.oid);
                    w += 6 + if inherited { 4 } else { 0 };
                    w += 6 * rowmarks_under(&up.source, &self.rowmarks) as u32
                        + copy_widths_under(&up.source, &self.copy_marks);
                    let mut seen = Vec::new();
                    for e in &returning {
                        e.walk(&mut |x| {
                            if let Expr::Col(i, ty) = x
                                && *i > ncols
                                && !seen.contains(i)
                            {
                                seen.push(*i);
                                w += col_w(*i, ty);
                            }
                        });
                    }
                    c.node.width = w;
                }
                // A scan computes the new values, then the row's ctid.
                if c.node.is_scan() {
                    let cat = self.ctx.catalog();
                    let w: u32 = up
                        .sets
                        .iter()
                        .map(|(_, e)| match e {
                            Expr::Col(i, _) if *i < t.columns.len() => {
                                costs::col_width(&cat, t, *i)
                            }
                            e => type_width(&e.ty()),
                        })
                        .sum();
                    c.node.width = w + 6;
                    // VERBOSE: the new values (in column order), then the row's ctid.
                    if self.o.verbose && up.route.is_none() {
                        let mut sets: Vec<&(usize, Expr)> = up.sets.iter().collect();
                        sets.sort_by_key(|x| x.0);
                        let mut out: Vec<String> = sets
                            .iter()
                            .map(|(_, e)| self.set_value(e, &c.exported))
                            .collect();
                        out.extend(self.rescans(&sets));
                        if let Some(k) = c.exported.get(t.columns.len()) {
                            out.push(self.pick(k, Cx::Output));
                        }
                        c.node.output = out;
                    }
                } else if self.o.verbose
                    && up.route.is_none()
                    && !up.inh
                    && c.node.node_type == "Result"
                    && c.node.children.len() == 1
                    && c.node.children[0].is_scan()
                {
                    // A Result (a one-time filter) over the scan computes them; the scan
                    // returns the columns they read, then its ctid.
                    let mut sets: Vec<&(usize, Expr)> = up.sets.iter().collect();
                    sets.sort_by_key(|x| x.0);
                    let mut out: Vec<String> =
                        sets.iter().map(|(_, e)| self.set_value(e, &c.exported)).collect();
                    out.extend(self.rescans(&sets));
                    let mut read: Vec<usize> = sets
                        .iter()
                        .flat_map(|(_, e)| e.columns())
                        .filter(|&i| i < t.columns.len())
                        .collect();
                    read.sort_unstable();
                    read.dedup();
                    let mut below: Vec<String> = read
                        .iter()
                        .filter_map(|&i| c.exported.get(i).map(|k| self.pick(k, Cx::Output)))
                        .collect();
                    if let Some(k) = c.exported.get(t.columns.len()) {
                        let k = self.pick(k, Cx::Output);
                        out.push(k.clone());
                        below.push(k);
                    }
                    c.node.output = out;
                    c.node.children[0].output = below;
                    c.node.subplans.append(&mut self.pending);
                } else if self.o.verbose
                    && up.route.is_none()
                    && up.members.is_empty()
                    && self.ctx.catalog().children(t.oid).is_empty()
                    && c.node.is_join()
                    && let Some(k) = c.exported.get(t.columns.len())
                {
                    // A join computes them: the new values (in column order), the target
                    // row's ctid, then the other relations' row marks.
                    // (An inheritance parent without members left, relhassubclass still set:
                    // its row identity is a tableoid and the ctid, after the other row
                    // marks, as expand_inherited_rtentry adds them; each scan returns it last.)
                    let parent = up.inh || self.ctx.catalog().class_has_subclass.contains(&t.oid);
                    let last: Vec<String>;
                    let mut sets: Vec<&(usize, Expr)> = up.sets.iter().collect();
                    sets.sort_by_key(|x| x.0);
                    // (The join computes them: a pulled-up subquery's output they read is
                    // its expression there, pullup_replace_vars.)
                    let own = computed_names(&c);
                    let mut top: Vec<String> =
                        sets.iter().map(|(_, e)| self.set_value(e, own)).collect();
                    top.extend(self.rescans(&sets));
                    // (A view's row identity, for its INSTEAD OF triggers: the whole row.)
                    let ident = if up.rules.view {
                        let cols: Vec<String> = c.exported[..t.columns.len()]
                            .iter()
                            .map(|n| self.pick(n, Cx::Output))
                            .collect();
                        format!("ROW({})", cols.join(", "))
                    } else {
                        self.pick(k, Cx::Output)
                    };
                    // (Then what RETURNING reads of the other relations: resjunk entries
                    // preprocess_targetlist adds for its Vars.)
                    let ncols = t.columns.len();
                    let limit = up.old_new.as_ref().map_or(c.exported.len(), |o| o.width);
                    let ret_vars = self.returning_vars(&returning, &c.exported, ncols + 1, limit);
                    if parent && let Some(ts) = target_scan(&up.source, t.oid) {
                        let Plan::Scan(s) = ts else { unreachable!() };
                        let tableoid = format!("{}.tableoid", quote_ident(&self.refname(ts)));
                        self.rowmark_junk.insert(addr(ts), vec![((1, s.rti), tableoid.clone())]);
                        last = expanded_tlist(&mut top, &c.node, &self.rowmark_junk, ret_vars, Some(&tableoid), &ident);
                    } else {
                        top.push(ident);
                        last = expanded_tlist(&mut top, &c.node, &self.rowmark_junk, ret_vars, None, "");
                    }
                    for ch in &mut c.node.children {
                        junk_layout(ch, &top, &last, &self.rowmark_junk, &scan_rtis(&up.source));
                    }
                    c.node.output = top;
                }
                // The subqueries the new values run show under the node computing them.
                for (_, e) in &up.sets {
                    self.register(e, &c.exported);
                }
                c.node.subplans.append(&mut self.pending);
                // An inheritance parent all of whose members are excluded: the new values
                // over its row, no tableoid or ctid.
                let inherited = up.inh || !self.ctx.catalog().children(t.oid).is_empty();
                if inherited && c.node.node_type == "Result" && c.node.children.is_empty() {
                    c.node.width = up.sets.iter().map(|(_, e)| type_width(&e.ty())).sum::<u32>() + 6;
                }
                if self.o.verbose
                    && inherited
                    && c.node.node_type == "Result"
                    && c.node.children.is_empty()
                {
                    let names: Vec<Name> = self
                        .table_names(t, &refname)
                        .into_iter()
                        .map(|n| Name::same(n.full))
                        .collect();
                    let mut sets: Vec<&(usize, Expr)> = up.sets.iter().collect();
                    sets.sort_by_key(|x| x.0);
                    let mut out: Vec<String> = sets.iter().map(|(_, e)| self.set_value(e, &names)).collect();
                    out.push("NULL::oid".into());
                    out.push("NULL::tid".into());
                    c.node.output = out;
                }
                let mut n = self.modify("Update", t, &refname, &c.node);
                self.check_option_subplans(&mut n, &up.rules.view_checks, t, &refname);
                self.returning(&mut n, &returning, t, &c.node);
                if !returning.is_empty() {
                    let mut names = self.table_names(t, &refname);
                    if inherited {
                        names = names.into_iter().map(|n| Name::same(n.full)).collect();
                    }
                    // (The new row, then the source row's own from its key on: its key slot,
                    // the other relations' columns.)
                    names.extend(c.exported.iter().skip(t.columns.len()).cloned());
                    let names = old_new_names(names, up.old_new.as_ref(), t);
                    let cx = if up.old_new.is_some() { Cx::Upper } else { Cx::Output };
                    let out = returning
                        .iter()
                        .map(|e| self.show(&old_new_rows(e, up.old_new.as_ref()), &names, cx))
                        .collect();
                    if self.o.verbose {
                        n.output = out;
                    }
                }
                n.children.push(c.node.child("Outer"));
                Built::passthrough(n, Vec::new())
            }
            Plan::Delete(dp) => {
                let returning = shown_returning(&dp.returning);
                let joined = !matches!(&*dp.source, Plan::Scan(_));
                UNDER_LOCK_ROWS.with(|u| u.set(u.get() + joined as usize));
                self.dml_copy_marks(&dp.source);
                let mut c = self.node(&dp.source);
                UNDER_LOCK_ROWS.with(|u| u.set(u.get() - joined as usize));
                let t = &dp.table;
                let refname = target_scan(&dp.source, t.oid)
                    .map(|s| self.refname(s))
                    .or_else(|| self.refnames.get(&key).cloned())
                    .unwrap_or_else(|| dp.alias.clone().unwrap_or_else(|| t.name.clone()));
                // VERBOSE: a scan returns just the row's ctid.
                if self.o.verbose
                    && c.node.is_scan()
                    && let Some(k) = c.exported.get(t.columns.len())
                {
                    c.node.output = vec![self.pick(k, Cx::Output)];
                } else if self.o.verbose
                    && !dp.inh
                    && c.node.is_join()
                    && let Some(k) = c.exported.get(t.columns.len())
                {
                    // A join returns the target row's ctid, then the other relations' row
                    // marks, then what RETURNING reads of them.
                    let mut top = vec![self.pick(k, Cx::Output)];
                    let skip = t.columns.len() + 1 + if dp.sys { crate::exec::scan::NUM_SYS_COLS } else { 0 };
                    let limit = dp.old_new.as_ref().map_or(c.exported.len(), |o| o.width);
                    let ret_vars = self.returning_vars(&returning, &c.exported, skip, limit);
                    let last = expanded_tlist(&mut top, &c.node, &self.rowmark_junk, ret_vars, None, "");
                    for ch in &mut c.node.children {
                        junk_layout(ch, &top, &last, &self.rowmark_junk, &scan_rtis(&dp.source));
                    }
                    c.node.output = top;
                }
                // An inheritance tree none of whose members is read: no tableoid or ctid,
                // RETURNING over the table's row.
                let none_read = (dp.inh || !self.ctx.catalog().children(t.oid).is_empty())
                    && c.node.node_type == "Result"
                    && c.node.children.is_empty();
                if self.o.verbose && none_read {
                    c.node.output = vec!["NULL::oid".into(), "NULL::tid".into()];
                }
                let mut n = self.modify("Delete", t, &refname, &c.node);
                self.returning(&mut n, &returning, t, &c.node);
                if !returning.is_empty() {
                    let exported = if none_read {
                        let mut names: Vec<Name> = t
                            .columns
                            .iter()
                            .map(|col| Name::same(Name::col(&refname, &col.name).full))
                            .collect();
                        names.push(Name::same(format!("{}.ctid", quote_ident(&refname))));
                        if dp.sys {
                            for (sc, _) in crate::exec::scan::SYS_COLS.iter() {
                                names.push(Name::same(format!("{}.{sc}", quote_ident(&refname))));
                            }
                        }
                        names
                    } else {
                        c.exported.clone()
                    };
                    let names = old_new_names(exported, dp.old_new.as_ref(), t);
                    let cx = if dp.old_new.is_some() { Cx::Upper } else { Cx::Output };
                    let out = returning
                        .iter()
                        .map(|e| self.show(&old_new_rows(e, dp.old_new.as_ref()), &names, cx))
                        .collect();
                    if self.o.verbose {
                        n.output = out;
                    }
                }
                n.children.push(c.node.child("Outer"));
                Built::passthrough(n, Vec::new())
            }
            Plan::Merge(mp) => {
                // (The target's scan returns its ctid: no physical target list.)
                let joined = !matches!(&*mp.source, Plan::Scan(_));
                UNDER_LOCK_ROWS.with(|u| u.set(u.get() + joined as usize));
                self.dml_copy_marks(&mp.source);
                let mut c = self.node(&mp.source);
                UNDER_LOCK_ROWS.with(|u| u.set(u.get() - joined as usize));
                if self.o.verbose && !mp.members.is_empty() {
                    tableoid_first(&mut c.node);
                }
                if self.o.verbose && c.node.is_join() {
                    // The processed target list: the target row's identity (first for a
                    // plain table, last for an inheritance tree's members), the source
                    // columns the actions read, the other relations' row marks.
                    let mut targets: Vec<String> = std::iter::once(mp.rules.table.oid)
                        .chain(mp.members.iter().map(|m| m.rules.table.oid))
                        .filter_map(|o| target_scan(&mp.source, o))
                        .map(|s| format!("{}.", quote_ident(&self.refname(s))))
                        .collect();
                    // (An empty inheritance tree: its dummy relation, named as the target;
                    // its row identity a ctid, last.)
                    let dummy = targets.is_empty();
                    if dummy {
                        targets.push(format!(
                            "{}.",
                            quote_ident(mp.alias.as_deref().unwrap_or(&mp.rules.table.name))
                        ));
                    }
                    let (mut ident, mut top) = (Vec::new(), Vec::new());
                    for o in &c.node.output {
                        if !targets.iter().any(|t| o.starts_with(t.as_str())) {
                            top.push(o.clone());
                        } else if o.ends_with(".ctid") || (o.ends_with(".tableoid") && !dummy) {
                            if !ident.contains(o) {
                                ident.push(o.clone());
                            }
                        }
                    }
                    // (preprocess_targetlist adds the Vars of each action's qual, then of its
                    // target list (in column order, as the rewriter leaves it), in the
                    // actions' order.)
                    let mut read: Vec<usize> = Vec::new();
                    let mut add = |e: &Expr| {
                        e.walk(&mut |x| {
                            if let Expr::Col(i, _) = x
                                && !read.contains(i)
                            {
                                read.push(*i);
                            }
                        })
                    };
                    for a in &mp.actions {
                        if let Some(q) = &a.cond {
                            add(q);
                        }
                        match &a.cmd {
                            MergeCmd::Update { sets, .. } => {
                                let mut sets: Vec<&(usize, Expr)> = sets.iter().collect();
                                sets.sort_by_key(|s| s.0);
                                sets.into_iter().for_each(|(_, e)| add(e));
                            }
                            MergeCmd::Insert { target, values, .. } => {
                                let mut vals: Vec<(usize, &Expr)> = target.iter().copied().zip(values).collect();
                                vals.sort_by_key(|v| v.0);
                                vals.into_iter().for_each(|(_, e)| add(e));
                            }
                            _ => {}
                        }
                    }
                    let read: Vec<String> =
                        read.iter().filter_map(|&i| c.exported.get(i)).map(|n| self.pick(n, Cx::Output)).collect();
                    top.sort_by_key(|o| read.iter().position(|r| r == o).unwrap_or(read.len()));
                    top.extend(junk_in(&c.node, &self.rowmark_junk));
                    let last = if mp.members.is_empty() && !dummy {
                        ident.append(&mut top);
                        top = ident;
                        Vec::new()
                    } else {
                        top.extend(ident.iter().cloned());
                        ident
                    };
                    for ch in &mut c.node.children {
                        junk_layout(ch, &top, &last, &self.rowmark_junk, &scan_rtis(&mp.source));
                    }
                    // (The dummy relation returns what is read of it and its ctid, no other
                    // system column.)
                    if dummy {
                        fn trim(n: &mut ExNode, t: &str) {
                            let mut seen = Vec::new();
                            n.output.retain(|o| {
                                let sys = o.strip_prefix(t).is_some_and(|c| {
                                    matches!(c, "xmin" | "cmin" | "xmax" | "cmax" | "tableoid")
                                });
                                let keep = !sys && !seen.contains(o);
                                seen.push(o.clone());
                                keep
                            });
                            for c in &mut n.children {
                                trim(c, t);
                            }
                        }
                        for ch in &mut c.node.children {
                            trim(ch, &targets[0]);
                        }
                    }
                    c.node.output = top;
                }
                let t = &mp.rules.table;
                let refname = target_scan(&mp.source, t.oid)
                    .map(|s| self.refname(s))
                    .unwrap_or_else(|| mp.alias.clone().unwrap_or_else(|| t.name.clone()));
                let mut n = if mp.members.is_empty() {
                    self.modify("Merge", t, &refname, &c.node)
                } else {
                    // An inheritance tree: the parent by its own name, then each member.
                    let app = if multi_inh_append(&mp.source) {
                        find_inh_append(&mp.source).map(|a| self.refname(a))
                    } else {
                        self.refnames.get(&key).cloned()
                    }
                    .unwrap_or_else(|| t.name.clone());
                    let mut n = self.modify("Merge", t, &app, &c.node);
                    let mut targets = Vec::new();
                    // (A partitioned parent holds no rows: not a target. Members pruned away
                    // aren't either, as the executor starts too.)
                    let own = (t.partkey.is_none()).then_some(t);
                    let kept: Option<Vec<Arc<Plan>>> = match target_append(&mp.source) {
                        Some(Plan::Append { inputs, .. }) => Some(crate::exec::nodes::runtime_pruned(
                            inputs,
                            crate::sql::binder::partition::Known::Start,
                            self.ctx,
                        )),
                        _ => None,
                    };
                    let live = |oid: u32| match (&kept, target_scan(&mp.source, oid)) {
                        (_, None) => false,
                        (None, Some(_)) => true,
                        (Some(k), Some(s)) => k.iter().any(|m| {
                            let mut x: &Plan = m;
                            loop {
                                if std::ptr::eq(x, s) {
                                    return true;
                                }
                                match x {
                                    Plan::Project { input, .. } | Plan::Filter { input, .. } => x = input,
                                    _ => return false,
                                }
                            }
                        }),
                    };
                    for m in own
                        .into_iter()
                        .chain(mp.members.iter().map(|m| &m.rules.table))
                        .filter(|m| live(m.oid))
                    {
                        let r = target_scan(&mp.source, m.oid)
                            .map(|s| self.refname(s))
                            .unwrap_or_else(|| m.name.clone());
                        let schema = self.ctx.catalog().schema_name(m.schema).to_string();
                        let mut tmp = ExNode::default();
                        self.target(
                            &mut tmp,
                            Some("Relation Name"),
                            Some(&m.name),
                            Some(&schema),
                            &r,
                        );
                        targets.push((format!("Merge{}", tmp.title), tmp.target));
                    }
                    n.details.push(Detail::Targets(targets));
                    n
                };
                // The sub-selects the actions run are the ModifyTable's SubPlans: numbered as
                // the planner preprocesses each action (its target list, then its qual), shown
                // the quals' first.
                {
                    let mut quals = Vec::new();
                    let mut tlists = Vec::new();
                    let sub = |e: &Expr| {
                        let mut sq = Vec::new();
                        subqueries_in(e, &mut sq);
                        !sq.is_empty()
                    };
                    for a in &mp.actions {
                        let start = self.pending.len();
                        let exprs: Vec<&Expr> = match &a.cmd {
                            MergeCmd::Update { sets, .. } => sets.iter().map(|(_, e)| e).collect(),
                            MergeCmd::Insert { values, .. } => values.iter().collect(),
                            _ => Vec::new(),
                        };
                        for e in exprs.into_iter().filter(|e| sub(e)) {
                            let _ = self.show(e, &c.exported, Cx::Upper);
                        }
                        tlists.extend(self.pending.drain(start..));
                        if let Some(q) = a.cond.as_ref().filter(|q| sub(q)) {
                            let start = self.pending.len();
                            let _ = self.show(q, &c.exported, Cx::Upper);
                            quals.extend(self.pending.drain(start..));
                        }
                    }
                    n.subplans.extend(quals);
                    n.subplans.extend(tlists);
                }
                n.details.push(Detail::MergeTuples(addr(&mp.source)));
                n.children.push(c.node.child("Outer"));
                Built::passthrough(n, Vec::new())
            }
        }
    }

    /// WITH CHECK OPTION's sub-selects over the new row: SubPlans of the ModifyTable.
    fn check_option_subplans(
        &mut self,
        n: &mut ExNode,
        checks: &[(String, Expr)],
        t: &crate::catalog::Table,
        refname: &str,
    ) {
        let names = self.table_names(t, refname);
        for (_, e) in checks {
            let mut sq = Vec::new();
            subqueries_in(e, &mut sq);
            if !sq.is_empty() {
                let _ = self.show(e, &names, Cx::Upper);
            }
        }
        n.subplans.append(&mut self.pending);
    }

    fn table_names(&self, t: &crate::catalog::Table, refname: &str) -> Vec<Name> {
        t.columns
            .iter()
            .map(|c| Name::col(refname, &c.name))
            .collect()
    }

    fn modify(
        &self,
        op: &'static str,
        t: &crate::catalog::Table,
        refname: &str,
        child: &ExNode,
    ) -> ExNode {
        let mut n = ExNode {
            operation: Some(op),
            ..ExNode::new("ModifyTable")
        };
        n.title = op.to_string();
        // A view written through its INSTEAD OF triggers whose query is a Subquery Scan of
        // its own: that scan has the view's name, the result relation the next one.
        fn view_scan(n: &ExNode, name: &str) -> bool {
            (n.node_type == "Subquery Scan" && n.target.iter().any(|(t, v, _)| *t == "Alias" && v == name))
                || (n.node_type != "ModifyTable" && n.children.iter().any(|c| view_scan(c, name)))
        }
        let suffixed;
        let refname = if self.ctx.catalog().views.contains_key(&t.oid) && view_scan(child, &t.name) {
            suffixed = crate::catalog::suffixed_name(&t.name, "_1");
            suffixed.as_str()
        } else {
            refname
        };
        let schema = self.ctx.catalog().schema_name(t.schema).to_string();
        self.target(
            &mut n,
            Some("Relation Name"),
            Some(&t.name),
            Some(&schema),
            refname,
        );
        n.startup_cost = child.startup_cost;
        n.total_cost = child.total_cost;
        n.width = 0;
        n
    }

    /// A ModifyTable with RETURNING returns the rows (its width the list's).
    fn returning(
        &self,
        n: &mut ExNode,
        list: &[Expr],
        t: &crate::catalog::Table,
        child: &ExNode,
    ) {
        if list.is_empty() {
            return;
        }
        let cat = self.ctx.catalog();
        let _ = (cat, t);
        // (Postgres gives it the subplan's width, not the list's.)
        n.rows = child.rows;
        n.width = child.width;
    }

    /// The MULTIEXPR SubPlans of multiple-column assignments among `sets` (each once, in
    /// order), as the target list that computes them runs them: `(rescan SubPlan n)` (an
    /// InitPlan's, a null placeholder).
    fn rescans(&self, sets: &[&(usize, Expr)]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: Vec<usize> = Vec::new();
        for (_, e) in sets {
            e.walk(&mut |x| {
                if let Some((sq, _)) = crate::sql::expr::multi_assign_field(x)
                    && !seen.contains(&sq.id)
                    && let Some(l) = self.labels.get(&sq.id)
                {
                    seen.push(sq.id);
                    // (An InitPlan's: a null placeholder.)
                    out.push(if l.init {
                        "NULL::record".to_string()
                    } else {
                        format!("(rescan {})", l.name)
                    });
                }
            });
        }
        out
    }

    /// An UPDATE's new value as its subplan computes it (field assignments: the new
    /// values, a FieldStore shown as them).
    fn set_value(&mut self, e: &Expr, names: &[Name]) -> String {
        fn values(e: &Expr) -> Option<Vec<&Expr>> {
            match e {
                Expr::Assign {
                    base,
                    subs,
                    field: Some(_),
                    value,
                    ..
                } if subs.is_empty() => {
                    let mut v = match &**base {
                        Expr::Col(..) => Vec::new(),
                        b => values(b)?,
                    };
                    v.push(value);
                    Some(v)
                }
                Expr::CoerceToDomain(x, _) => values(x),
                _ => None,
            }
        }
        match values(e) {
            Some(v) if v.len() == 1 => self.show(v[0], names, Cx::Output),
            Some(v) => {
                let parts: Vec<String> = v.iter().map(|x| self.show(x, names, Cx::Output)).collect();
                format!("ROW({})", parts.join(", "))
            }
            None => self.show(e, names, Cx::Output),
        }
    }

    /// What an inheritance UPDATE / DELETE's member scan named `r` returns (VERBOSE): an
    /// UPDATE's new values (computed there for a partitioned table, else the columns they
    /// read, in column order), then the row's tableoid and ctid.
    fn member_dml_output(
        &mut self,
        op: &str,
        pt: &crate::catalog::Table,
        sorted: &[&(usize, Expr)],
        r: &str,
    ) -> Vec<String> {
        let names: Vec<Name> = pt
            .columns
            .iter()
            .map(|c| Name::same(Name::col(r, &c.name).full))
            .collect();
        let mut out: Vec<String> = Vec::new();
        if op == "Update" {
            if pt.partkey.is_some() {
                for (_, e) in sorted {
                    out.push(self.set_value(e, &names));
                }
                out.extend(self.rescans(sorted));
            } else {
                let mut cols: Vec<usize> = Vec::new();
                for (_, e) in sorted {
                    cols.extend(e.columns().into_iter().filter(|&c| c < pt.columns.len()));
                }
                cols.sort_unstable();
                cols.dedup();
                out.extend(cols.iter().map(|&c| names[c].full.clone()));
            }
        }
        out.push(format!("{}.tableoid", quote_ident(r)));
        out.push(format!("{}.ctid", quote_ident(r)));
        out
    }

    /// UPDATE / DELETE of an inheritance tree: one ModifyTable naming each result relation
    /// (the members its source reads, in order), over the source: the members' Append, or a
    /// join over it.
    #[allow(clippy::too_many_arguments)]
    /// VERBOSE: an UPDATE / DELETE / MERGE's other relations that are no table (a CTE, a
    /// function, VALUES) have a ROW_MARK_COPY row mark (preprocess_rowmarks): their whole
    /// row goes up the plan as junk, for EvalPlanQual.
    fn dml_copy_marks(&mut self, source: &Arc<Plan>) {
        if !self.o.verbose {
            return;
        }
        // (A FROM subquery's mark is its whole row too, which its Subquery Scan returns
        // (`shown_subqueries`); not a subquery holding the target, a view written through.)
        let mut copies = Vec::new();
        dml_copies(source, &mut copies);
        for (a, p) in copies {
            let name = format!("{}.*", quote_ident(&self.refname(p)));
            self.rowmark_junk.insert(a, vec![((0, leaf_rti(p)), name)]);
        }
    }

    /// The columns of other relations a data-modifying statement's RETURNING reads (those in
    /// `from..until` of its source's row), in the order it reads them.
    fn returning_vars(&self, returning: &[Expr], exported: &[Name], from: usize, until: usize) -> Vec<String> {
        let mut out = Vec::new();
        for e in returning {
            for col in e.columns() {
                if (from..until).contains(&col)
                    && let Some(n) = exported.get(col)
                {
                    let s = self.pick(n, Cx::Output);
                    if !out.contains(&s) {
                        out.push(s);
                    }
                }
            }
        }
        out
    }

    fn inherited_dml(
        &mut self,
        key: usize,
        op: &'static str,
        pt: &Arc<crate::catalog::Table>,
        members: &[Arc<crate::catalog::Table>],
        sets: &[(usize, Expr)],
        returning: &[Expr],
        old_new: Option<&super::plan::OldNew>,
        sys: bool,
        source: &Arc<Plan>,
    ) -> Built {
        // The result relations: the members the plan reads (a partitioned parent holds no
        // rows), those pruning leaves when the executor starts shown.
        let kept: Option<Vec<Arc<Plan>>> = match target_append(source) {
            Some(Plan::Append { inputs, .. }) => Some(crate::exec::nodes::runtime_pruned(
                inputs,
                crate::sql::binder::partition::Known::Start,
                self.ctx,
            )),
            _ => None,
        };
        let mut all: Vec<(Arc<crate::catalog::Table>, String)> = Vec::new();
        let mut rels: Vec<(Arc<crate::catalog::Table>, String)> = Vec::new();
        let inputs: &[Arc<Plan>] = match target_append(source) {
            Some(Plan::Append { inputs, .. }) => inputs,
            _ => &[],
        };
        for m in std::iter::once(pt)
            .filter(|t| t.partkey.is_none())
            .chain(members.iter())
        {
            if let Some(s) = target_scan(source, m.oid) {
                // (A member pruning removed has no name of its own: its parent's.)
                let name = self
                    .refnames
                    .get(&addr(s))
                    .cloned()
                    .unwrap_or_else(|| dml_tree_name(source, pt, None));
                let r = (m.clone(), name);
                // (Postgres keeps the result relations of a partitioning level the
                // pruning has no steps for, their parent's pruning removing them or not.)
                let input = inputs.iter().find(|i| target_scan(i, m.oid).is_some());
                if kept
                    .as_ref()
                    .is_none_or(|k| k.iter().any(|i| target_scan(i, m.oid).is_some()))
                    || input.is_some_and(|i| !level_prunable(i))
                {
                    rels.push(r.clone());
                }
                all.push(r);
            }
        }
        let mut targets = Vec::new();
        for (t, r) in &rels {
            let schema = self.ctx.catalog().schema_name(t.schema).to_string();
            let mut tmp = ExNode::default();
            self.target(&mut tmp, Some("Relation Name"), Some(&t.name), Some(&schema), r);
            targets.push((format!("{op}{}", tmp.title), tmp.target));
        }
        let refname = if multi_target_append(source) {
            target_append(source).map(|a| self.refname(a))
        } else {
            self.refnames.get(&key).cloned()
        }
        .unwrap_or_else(|| pt.name.clone());
        let joined = has_join(source);
        UNDER_LOCK_ROWS.with(|u| u.set(u.get() + joined as usize));
        self.dml_copy_marks(source);
        let mut app = self.node(source);
        UNDER_LOCK_ROWS.with(|u| u.set(u.get() - joined as usize));
        // VERBOSE, one member read over a join: the join computes the new values, then the
        // other relations' row marks, then the member's row identity (tableoid, ctid), which
        // each scan returns last.
        if self.o.verbose
            && joined
            && all.len() == 1
            && app.node.is_join()
            && let Some(ts) = target_scan(source, all[0].0.oid)
            && let Plan::Scan(s) = ts
            && let Some(k) = app.exported.get(pt.columns.len())
        {
            let tref = quote_ident(&self.refname(ts));
            let tableoid = format!("{tref}.tableoid");
            let ident = self.pick(k, Cx::Output);
            self.rowmark_junk.insert(addr(ts), vec![((1, s.rti), tableoid.clone())]);
            let mut sorted: Vec<&(usize, Expr)> = sets.iter().collect();
            sorted.sort_by_key(|x| x.0);
            let own = computed_names(&app);
            let mut top: Vec<String> = sorted.iter().map(|(_, e)| self.set_value(e, own)).collect();
            top.extend(self.rescans(&sorted));
            let skip = pt.columns.len() + 1 + if sys { crate::exec::scan::NUM_SYS_COLS } else { 0 };
            let limit = old_new.map_or(app.exported.len(), |o| o.width);
            let ret_vars = self.returning_vars(returning, &app.exported, skip, limit);
            let last = expanded_tlist(&mut top, &app.node, &self.rowmark_junk, ret_vars, Some(&tableoid), &ident);
            for ch in &mut app.node.children {
                junk_layout(ch, &top, &last, &self.rowmark_junk, &scan_rtis(source));
            }
            app.node.output = top;
        }
        let partitioned = pt.partkey.is_some();
        // (One member: no Append, the plan's own.)
        let single = all.len() == 1;
        // The new values (in column order) over the members' rows, named `r`.
        let qualified = |r: &str| -> Vec<Name> {
            pt.columns
                .iter()
                .map(|c| Name::same(Name::col(r, &c.name).full))
                .collect()
        };
        let mut sorted: Vec<&(usize, Expr)> = sets.iter().collect();
        sorted.sort_by_key(|x| x.0);
        // UPDATE of an inheritance tree computes the new values above its members' scans
        // (the Append can't project) unless they are columns the scans return; a
        // partitioned table's members compute them.
        let above_scans = op == "Update"
            && !partitioned
            && !joined
            && sets.iter().any(|(_, e)| !matches!(e, Expr::Col(..)));
        let above = if single { all[0].1.clone() } else { refname.clone() };
        // VERBOSE: each member's scan returns the new values (computed there for a
        // partitioned table, else the columns they read), then its tableoid and ctid.
        let result_out = if self.o.verbose && !joined {
            if single {
                if app.node.is_scan() {
                    app.node.output = self.member_dml_output(op, pt, &sorted, &all[0].1);
                    app.node.subplans.append(&mut self.pending);
                }
            } else if let Some(kept) = &kept {
                // (The members the executor starts with: those pruning left.)
                let kept: Vec<&Arc<Plan>> =
                    kept.iter().filter(|m| !matches!(&***m, Plan::Empty { .. })).collect();
                if kept.len() == app.node.children.len() {
                    for (m, c) in kept.iter().zip(app.node.children.iter_mut()) {
                        let r = rels
                            .iter()
                            .find(|(t, _)| target_scan(m, t.oid).is_some())
                            .map(|(_, r)| r.clone());
                        if let Some(r) = r
                            && c.is_scan()
                        {
                            c.output = self.member_dml_output(op, pt, &sorted, &r);
                            // (A subquery the new values read shows under the first member
                            // that computes them.)
                            c.subplans.append(&mut self.pending);
                        }
                    }
                }
            }
            // Above them, the new values over the parent's row (its one member's, when the
            // Append is gone).
            above_scans.then(|| {
                let pnames = qualified(&above);
                let mut out: Vec<String> =
                    sorted.iter().map(|(_, e)| self.set_value(e, &pnames)).collect();
                out.extend(self.rescans(&sorted));
                out.push(format!("{}.tableoid", quote_ident(&above)));
                out.push(format!("{}.ctid", quote_ident(&above)));
                out
            })
        } else {
            None
        };
        // The subqueries the new values run show under the node computing them: the Result
        // above the members, the join, or the first member's scan.
        if op == "Update" {
            let names: Vec<Name> = if joined {
                app.exported.clone()
            } else if above_scans {
                qualified(&above)
            } else {
                qualified(&all[0].1)
            };
            for (_, e) in &sorted {
                self.register(e, &names);
            }
            if !above_scans {
                let at = if joined || single {
                    Some(&mut app.node)
                } else {
                    app.node.children.first_mut()
                };
                if let Some(at) = at {
                    at.subplans.append(&mut self.pending);
                }
            }
        }
        // VERBOSE, several members read over a join: the processed target list
        // (preprocess_targetlist): the new values, the other relations' row marks, then the
        // tree's row identity (add_row_identity_columns, after them: its tableoid and ctid),
        // which each member's scan returns last.
        if self.o.verbose
            && joined
            && all.len() > 1
            && app.node.is_join()
            && let Some(ctid) = app.exported.get(pt.columns.len()).map(|k| self.pick(k, Cx::Output))
            && let Some(parent) = ctid.strip_suffix(".ctid").map(str::to_string)
        {
            let own = computed_names(&app);
            let mut top: Vec<String> = sorted.iter().map(|(_, e)| self.set_value(e, own)).collect();
            top.extend(self.rescans(&sorted));
            top.extend(junk_in(&app.node, &self.rowmark_junk));
            let members: Vec<String> = all.iter().map(|(_, r)| quote_ident(r)).collect();
            add_tableoid(&mut app.node, &parent, &members);
            top.push(format!("{parent}.tableoid"));
            top.push(ctid);
            let last: Vec<String> = members
                .iter()
                .flat_map(|m| [format!("{m}.tableoid"), format!("{m}.ctid")])
                .collect();
            for ch in &mut app.node.children {
                junk_layout(ch, &top, &last, &self.rowmark_junk, &scan_rtis(source));
                append_passthrough(ch, &parent, &members);
            }
            app.node.output = top;
        }
        let child = if above_scans {
            let mut r = self.result(app.node.rows);
            r.width = app.node.width;
            r.startup_cost = app.node.startup_cost;
            r.total_cost = app.node.total_cost;
            // (It runs as its input does.)
            r.key = app.node.key;
            if let Some(o) = result_out {
                r.output = o;
            }
            r.subplans.append(&mut self.pending);
            r.children.push(app.node.child("Outer"));
            r
        } else {
            app.node
        };
        let mut n = self.modify(op, pt, &refname, &child);
        self.returning(&mut n, returning, pt, &child);
        // RETURNING over the first result relation's row (as the plan has it), then its
        // key, system columns and the other relations' columns.
        if !returning.is_empty() {
            let r = all[0].1.clone();
            let mut names = qualified(&r);
            names.push(Name::same(format!("{}.ctid", quote_ident(&r))));
            let mut skip = pt.columns.len() + 1;
            if sys {
                for (sc, _) in crate::exec::scan::SYS_COLS.iter() {
                    names.push(Name::same(format!("{}.{sc}", quote_ident(&r))));
                }
                skip += crate::exec::scan::NUM_SYS_COLS;
            }
            names.extend(app.exported.iter().skip(skip).cloned());
            let names = old_new_names(names, old_new, pt);
            let cx = if old_new.is_some() { Cx::Upper } else { Cx::Output };
            let out = returning
                .iter()
                .map(|e| self.show(&old_new_rows(e, old_new), &names, cx))
                .collect();
            if self.o.verbose {
                n.output = out;
            }
        }
        n.details.push(Detail::Targets(targets));
        n.children.push(child.child("Outer"));
        Built::passthrough(n, Vec::new())
    }

    /// A UNION under an ORDER BY: hashed then sorted, or (when cheaper) each arm sorted,
    /// merged and made unique (generate_union_paths).
    fn sorted_union(&mut self, input: &Arc<Plan>, keys: &[SortKey]) -> Built {
        let mut arms = Vec::new();
        union_arms(input, false, &mut arms);
        let built: Vec<Built> = arms.iter().map(|a| self.node(a)).collect();
        let exported = built.first().map(|b| b.exported.clone()).unwrap_or_default();
        let ncols = exported.len();
        // Hashed: an Append, a HashAggregate, a Sort.
        let mut app = ExNode::new("Append");
        for (i, b) in built.iter().enumerate() {
            if i == 0 {
                app.startup_cost = b.node.startup_cost;
                app.cols = b.node.cols.iter().map(|x| ColInfo { var: None, ..x.clone() }).collect();
            }
            app.rows += b.node.rows;
            app.total_cost += b.node.total_cost;
            app.width = app.width.max(b.node.width);
            app.children.push(b.node.clone().child("Member"));
        }
        app.total_cost += app.rows * costs::CPU_TUPLE_COST * 0.5;
        let mut agg = ExNode {
            strategy: Some("Hashed"),
            partial_mode: Some("Simple"),
            ..ExNode::new("Aggregate")
        };
        agg.title = "HashAggregate".into();
        agg.key = Some(addr(input));
        let gkeys = exported.iter().map(|k| self.pick(k, Cx::Upper)).collect();
        agg.details.push(Detail::List("Group Key", gkeys));
        agg.details.push(Detail::HashAggInfo);
        agg.rows = app.rows;
        agg.width = app.width;
        agg.startup_cost = app.total_cost + app.rows * costs::CPU_OPERATOR_COST * ncols as f64;
        agg.total_cost = agg.startup_cost + agg.rows * costs::CPU_TUPLE_COST;
        let list: Vec<String> = keys
            .iter()
            .map(|k| self.show(&k.expr, &exported, Cx::Upper) + &sort_suffix(k))
            .collect();
        let mut hs = ExNode::new("Sort");
        self.sort_costs(&mut hs, &agg);
        let g = crate::sql::paths::Gucs::read(self.ctx);
        let types = crate::sql::paths::analyzejoins::col_types(arms[0]);
        let can_hash = all_hashable(self.ctx, &types);
        agg.disabled = !g.hashagg;
        agg.children.push(app.clone().child("Outer"));
        hs.details.push(Detail::List("Sort Key", list.clone()));
        hs.details.push(Detail::SortInfo);
        hs.order = list.clone();
        hs.children.push(agg.child("Outer"));
        let mut best = hs;
        // Or a Unique over the Append sorted on the columns (then sorted for the ORDER BY
        // when that isn't its order).
        if types.iter().all(setop_sortable) && ncols > 0 {
            let names: Vec<String> = exported.iter().map(|k| self.pick(k, Cx::Upper)).collect();
            let mut us = ExNode::new("Sort");
            us.details.push(Detail::List("Sort Key", names.clone()));
            us.details.push(Detail::SortInfo);
            self.sort_costs(&mut us, &app);
            us.disabled = !g.sort;
            if self.o.verbose {
                us.output = exported.iter().map(|k| self.pick(k, Cx::Output)).collect();
            }
            us.order = names.clone();
            us.children.push(app.clone().child("Outer"));
            let mut u = ExNode::new("Unique");
            u.unique_keys = Some((0..ncols).collect());
            u.rows = app.rows;
            u.width = app.width;
            u.cols = app.cols.clone();
            u.startup_cost = us.startup_cost;
            u.total_cost = us.total_cost + costs::CPU_OPERATOR_COST * ncols as f64 * us.rows;
            u.order = names.clone();
            u.children.push(us.child("Outer"));
            let top = if names.len() >= list.len() && names[..list.len()] == list[..] {
                u
            } else {
                let mut ts = ExNode::new("Sort");
                ts.details.push(Detail::List("Sort Key", list.clone()));
                ts.details.push(Detail::SortInfo);
                self.sort_costs(&mut ts, &u);
                ts.order = list.clone();
                ts.children.push(u.child("Outer"));
                ts
            };
            if !can_hash || parallel::cheaper(&top, &best) {
                best = top;
            }
        }
        // Sorted: each arm sorted, merged, made unique: in the ORDER BY's order when that
        // covers every column, else in the columns' (then sorted for the ORDER BY).
        let order: Vec<usize> = keys
            .iter()
            .filter_map(|k| match k.expr {
                Expr::Col(i, _) => Some(i),
                _ => None,
            })
            .collect();
        let covers = (0..ncols).all(|i| order.contains(&i));
        // (EXPLAIN ANALYZE shows the plan that ran.)
        if (covers || types.iter().all(setop_sortable)) && ncols > 0 && built.len() > 1 && !self.o.analyze {
            let mut kids = Vec::new();
            let (mut startup, mut run, mut rows) = (0.0, 0.0, 0.0);
            let mut arm_list = Vec::new();
            for (i, c) in built.iter().enumerate() {
                let (alist, cols): (Vec<String>, Vec<Option<usize>>) = if covers {
                    keys.iter()
                        .map(|k| {
                            let col = match k.expr {
                                Expr::Col(j, _) => Some(j),
                                _ => None,
                            };
                            (self.show(&k.expr, &c.exported, Cx::Upper) + &sort_suffix(k), col)
                        })
                        .unzip()
                } else {
                    c.exported.iter().enumerate().map(|(j, x)| (self.pick(x, Cx::Upper), Some(j))).unzip()
                };
                if i == 0 {
                    arm_list = alist.clone();
                }
                let mut sn = ExNode::new("Sort");
                sn.details.push(Detail::List("Sort Key", alist.clone()));
                sn.details.push(Detail::SortInfo);
                self.sort_costs(&mut sn, &c.node);
                sn.key = c.node.key;
                if self.o.verbose {
                    sn.output = c.node.output.clone();
                }
                sn.order = alist.clone();
                sn.children.push(c.node.clone().child("Outer"));
                // (Or the arm's partial plans sorted in the workers and merged, when that
                // is its cheapest path in the order: generate_useful_gather_paths.)
                if let Some(pt) = &c.node.partial {
                    for gm in self.useful_gathers(pt, &c.node.partials, &alist, &cols, Some(c.node.rows), c.node.key, true) {
                        if parallel::cheaper(&gm, &sn) {
                            sn = gm;
                        }
                    }
                }
                startup += sn.startup_cost;
                run += sn.total_cost - sn.startup_cost;
                rows += sn.rows;
                kids.push(sn);
            }
            // cost_merge_append, and a Unique comparing every column.
            let n = kids.len() as f64;
            let cmp = 2.0 * costs::CPU_OPERATOR_COST;
            let logn = n.log2();
            let ma_startup = startup + cmp * n * logn;
            let ma_total =
                ma_startup + run + rows * cmp * logn + costs::CPU_TUPLE_COST * 0.5 * rows;
            let u_total = ma_total + costs::CPU_OPERATOR_COST * ncols as f64 * rows;
            {
                let width = kids.iter().map(|k| k.width).max().unwrap_or(0);
                let mut ma = ExNode::new("Merge Append");
                let names: Vec<String> = if covers {
                    list.clone()
                } else {
                    exported.iter().map(|k| self.pick(k, Cx::Upper)).collect()
                };
                ma.details.push(Detail::List("Sort Key", arm_list));
                ma.rows = rows;
                ma.width = width;
                ma.cols = app.cols.clone();
                ma.startup_cost = ma_startup;
                ma.total_cost = ma_total;
                for k in kids {
                    ma.children.push(k.child("Member"));
                }
                let mut u = ExNode::new("Unique");
                u.rows = rows;
                u.width = width;
                u.cols = app.cols.clone();
                u.startup_cost = ma_startup;
                u.total_cost = u_total;
                u.order = names.clone();
                u.unique_keys = Some((0..ncols).collect());
                u.children.push(ma.child("Outer"));
                let u = if covers {
                    u
                } else {
                    let common = list.iter().zip(&names).take_while(|(a, b)| a == b).count();
                    if common == list.len() {
                        u
                    } else if common > 0 && g.incremental_sort {
                        let mut is = ExNode::new("Incremental Sort");
                        is.details.push(Detail::List("Sort Key", list.clone()));
                        is.details.push(Detail::List(
                            "Presorted Key",
                            list[..common].iter().map(|k| presorted_name(k)).collect(),
                        ));
                        is.details.push(Detail::IncSortInfo);
                        let groups = u.rows.clamp(1.0, costs::DEFAULT_NUM_DISTINCT);
                        (is.startup_cost, is.total_cost) =
                            costs::incremental_sort_cost(u.startup_cost, u.total_cost, u.rows, u.width, groups, None);
                        is.rows = u.rows;
                        is.width = u.width;
                        is.cols = u.cols.clone();
                        is.order = list.clone();
                        is.children.push(u.child("Outer"));
                        is
                    } else {
                        let mut ts = ExNode::new("Sort");
                        ts.details.push(Detail::List("Sort Key", list.clone()));
                        ts.details.push(Detail::SortInfo);
                        self.sort_costs(&mut ts, &u);
                        ts.order = list.clone();
                        ts.children.push(u.child("Outer"));
                        ts
                    }
                };
                // (In the same order: add_path keeps the one that dominates, fuzzily.)
                if parallel::dominates(&u, &best) {
                    return Built::passthrough(u, exported);
                }
            }
        }
        Built::passthrough(best, exported)
    }

    /// estimate_num_groups: the product of the columns' distinct counts (a default for an
    /// expression), scaled down when the input is a filtered relation, at most the input.
    fn num_groups(&self, cols: &[ColInfo], in_rows: f64) -> f64 {
        if cols.is_empty() {
            return 1.0;
        }
        let cat = self.ctx.catalog();
        let mut total = 1.0;
        let mut vars = Vec::new();
        // (The relation's rows, as its scan estimated them: what its restrictions leave of
        // the values, not the rows of the input grouped.)
        let mut rel_rows = None;
        for c in cols {
            match &c.var {
                Some(v) => {
                    if rel_rows.is_none() && v.rows > 0.0 && v.expr.is_none() {
                        rel_rows = Some(v.rows);
                    }
                    super::extstat::add_unique(&mut vars, super::extstat::GroupVar::of(&cat, v))
                }
                None => total *= c.distinct.unwrap_or(costs::DEFAULT_NUM_DISTINCT),
            }
        }
        // (estimate_num_groups scales a relation's distinct values to its own rows, rel->rows,
        // however many rows a join above makes of them.)
        let rel_rows = rel_rows.unwrap_or(in_rows);
        super::extstat::num_groups(self.ctx, vars, total, in_rows, Some(rel_rows))
    }

    /// estimate_num_groups over GROUP BY expressions of the input's columns `cols`: a
    /// boolean counts 2, an expression with statistics (an index's or a statistics
    /// object's) its distinct count, any other the columns it uses.
    /// create_groupingsets_path's costs of grouping sets `sets` over `input`: whether
    /// the sorted form (each rollup's sets over the input sorted for it) costs less than
    /// the hashed (or mixed) one, which must also fit in hash_mem.
    fn sets_sorted_cheaper(&self, input: &ExNode, groups: &[Expr], sets: &[Vec<usize>], aggs: &[AggCall]) -> bool {
        let in_rows = input.rows;
        let g = crate::sql::paths::Gucs::read(self.ctx);
        let trans = costs::CPU_OPERATOR_COST
            * aggs
                .iter()
                .map(|a| 1 + a.args.iter().map(costs::ops).sum::<usize>())
                .sum::<usize>() as f64;
        let set_groups = |s: &Vec<usize>| -> f64 {
            if s.is_empty() {
                return 1.0;
            }
            let exprs: Vec<Expr> = s.iter().map(|&i| groups[i].clone()).collect();
            self.group_num_groups(&exprs, &input.cols, in_rows)
        };
        let op = costs::CPU_OPERATOR_COST;
        let tuple = costs::CPU_TUPLE_COST;
        // Sorted: the first rollup over the input sorted for it, each later one sorting
        // the input again.
        let chains = crate::exec::agg::rollup_chains(sets);
        let mut sorted = 0.0;
        for (ci, (chain, cols)) in chains.iter().enumerate() {
            let ng: f64 = chain.iter().map(|&si| set_groups(&sets[si])).sum();
            let ncols = cols.len() as f64;
            let base = if ci == 0 {
                if cols.is_empty() {
                    input.total_cost
                } else {
                    costs::sort_cost(input.total_cost, in_rows, input.width, None).1
                }
            } else {
                costs::sort_cost(0.0, in_rows, input.width, None).1
            };
            sorted += base + op * ncols * in_rows + trans * in_rows + ng * tuple;
        }
        // Hashed: every non-empty set its own hash table (the empty ones a sorted rollup
        // after them); not when the tables don't fit in hash_mem.
        let hashed_sets: Vec<&Vec<usize>> = sets.iter().filter(|s| !s.is_empty()).collect();
        if hashed_sets.is_empty() {
            return true;
        }
        let empties = sets.len() - hashed_sets.len();
        let total_groups: f64 = hashed_sets.iter().map(|s| set_groups(s)).sum();
        let entry = 16.0 + ((16.0 + input.width as f64) / 8.0).ceil() * 8.0 + 16.0 * aggs.len() as f64;
        if entry * total_groups > g.work_mem * g.hash_mem_multiplier * 1024.0 {
            return true;
        }
        // (enable_sort off: the sorts disable the sorted form.)
        if !g.sort && (chains.len() > 1 || chains.first().is_some_and(|c| !c.1.is_empty())) {
            return false;
        }
        let mut hashed = input.total_cost;
        for s in &hashed_sets {
            let ng = set_groups(s);
            hashed += op * s.len() as f64 * in_rows + trans * in_rows + ng * tuple;
        }
        if empties > 0 {
            hashed += trans * in_rows + empties as f64 * tuple;
        }
        sorted < hashed
    }

    /// create_partial_distinct_paths: the DISTINCT (serial: hashed `n`, sorted `u`) done in
    /// the workers over the input's partial plans (`pt` the cheapest, `others` the ones
    /// kept for their order), then over what a Gather (Merge) of them sends
    /// (create_final_distinct_paths), when such a plan survives add_path and is the
    /// cheapest; the other plans that survive go beside it (`alt`), for a LIMIT above.
    /// `needed`: the DISTINCT's keys (distinct_pathkeys: column, name); each sorted path
    /// is also tried in the order of a path's own leading keys among them
    /// (get_useful_pathkeys_for_distinct).
    #[allow(clippy::too_many_arguments)]
    fn parallel_distinct(
        &self,
        n: &ExNode,
        u: &ExNode,
        pt: &ExNode,
        others: &[ExNode],
        needed: &[(usize, String)],
        keys: &[ColInfo],
        key_ops: f64,
        per_group: f64,
        every_row: bool,
    ) -> Option<ExNode> {
        let g = crate::sql::paths::Gucs::read(self.ctx);
        let need_names: Vec<String> = needed.iter().map(|x| x.1.clone()).collect();
        let col_of = |name: &str| needed.iter().find(|x| x.1 == name).map(|x| x.0);
        // get_useful_pathkeys_for_distinct: the keys as they are, and reordered to lead
        // with the path's own leading keys among them.
        let useful = |order: &[String]| -> Vec<Vec<String>> {
            let mut out = vec![need_names.clone()];
            if !g.distinct_reordering {
                return out;
            }
            let mut lead: Vec<String> = Vec::new();
            for o in order {
                if !need_names.contains(o) || lead.contains(o) {
                    break;
                }
                lead.push(o.clone());
            }
            if lead.is_empty() {
                return out;
            }
            for k in &need_names {
                if !lead.contains(k) {
                    lead.push(k.clone());
                }
            }
            if lead != need_names {
                out.push(lead);
            }
            out
        };
        let pd_rows = if every_row { pt.rows } else { self.num_groups(keys, pt.rows) };
        let unique_over = |child: ExNode, rows: f64, ord: &[String]| -> ExNode {
            let mut x = ExNode::new("Unique");
            x.unique_keys = Some(ord.iter().filter_map(|k| col_of(k)).collect());
            x.rows = rows;
            x.width = n.width;
            x.cols = n.cols.clone();
            x.startup_cost = child.startup_cost;
            x.total_cost = child.total_cost + child.rows * key_ops;
            x.order = ord.to_vec();
            x.workers = child.workers;
            x.output = child.output.clone();
            x.children.push(child.child("Outer"));
            x
        };
        let hash_over = |child: ExNode, rows: f64| -> ExNode {
            let mut x = n.clone();
            x.children.clear();
            x.alt = None;
            x.partial = None;
            x.rows = rows;
            x.startup_cost = child.total_cost + child.rows * key_ops;
            x.total_cost = x.startup_cost + rows * (costs::CPU_TUPLE_COST + per_group);
            let (ss, st) = g.hashagg_spill(child.rows, child.width, rows, 0, 0.0);
            x.startup_cost += ss;
            x.total_cost += st;
            x.disabled = !g.hashagg;
            x.order.clear();
            x.workers = child.workers;
            x.output = child.output.clone();
            x.children.push(child.child("Outer"));
            x
        };
        // make_ordered_path: `p` already in `ord`, or sorted on it: incrementally when it
        // has a leading part of it, else fully but only the cheapest path (`cheapest`).
        let ordered = |p: &ExNode, ord: &[String], cheapest: bool| -> Option<ExNode> {
            let common = ord.iter().zip(&p.order).take_while(|(a, b)| a == b).count();
            if common == ord.len() {
                return Some(p.clone());
            }
            if !cheapest && (common == 0 || !g.incremental_sort) {
                return None;
            }
            let cols: Vec<Option<usize>> = ord.iter().map(|k| col_of(k)).collect();
            let mut sn = self.sorted_partial(p, ord, &cols, g.incremental_sort, p.key);
            sn.output = p.output.clone();
            Some(sn)
        };
        let same = |a: &ExNode, b: &ExNode| {
            a.title == b.title && a.total_cost == b.total_cost && a.startup_cost == b.startup_cost && a.order == b.order
        };
        // The partial DISTINCTs (add_partial_path): sorted ones over each partial plan, and
        // the hashed one over the cheapest.
        let mut partials: Vec<ExNode> = Vec::new();
        for (i, p) in std::iter::once(pt).chain(others).enumerate() {
            for ord in useful(&p.order) {
                if let Some(sp) = ordered(p, &ord, i == 0) {
                    parallel::add_partial_path(&mut partials, unique_over(sp, pd_rows, &ord));
                }
            }
        }
        parallel::add_partial_path(&mut partials, hash_over(pt.clone(), pd_rows));
        let cheapest_partial = partials
            .iter()
            .fold(None::<&ExNode>, |b, x| match b {
                Some(b) if !parallel::cheaper(x, b) => Some(b),
                _ => Some(x),
            })?
            .clone();
        // generate_useful_gather_paths: a Gather of the cheapest, a Gather Merge of each
        // sorted one, and for the query's order (the keys) one over the cheapest sorted
        // and over each other one sorted incrementally.
        let mut gathered = vec![self.gather_of(&cheapest_partial)];
        for p in &partials {
            if !p.order.is_empty() {
                gathered.push(self.gather_merge_of(p));
            }
        }
        if self.par.as_ref().is_some_and(|c| c.gather_merge) {
            for p in &partials {
                let common = need_names.iter().zip(&p.order).take_while(|(a, b)| a == b).count();
                if common == need_names.len() {
                    continue;
                }
                if let Some(sp) = ordered(p, &need_names, same(p, &cheapest_partial)) {
                    gathered.push(self.gather_merge_of(&sp));
                }
            }
        }
        let gathered = parallel::add_paths(gathered);
        let cheapest_total = gathered
            .iter()
            .fold(None::<&ExNode>, |b, x| match b {
                Some(b) if !parallel::cheaper(x, b) => Some(b),
                _ => Some(x),
            })
            .cloned()?;
        // create_final_distinct_paths over them: each in a useful order of its own (the
        // cheapest sorted when it isn't), and the cheapest hashed.
        let mut finals: Vec<ExNode> = Vec::new();
        for x in &gathered {
            for ord in useful(&x.order) {
                if let Some(sp) = ordered(x, &ord, same(x, &cheapest_total)) {
                    finals.push(unique_over(sp, n.rows, &ord));
                }
            }
        }
        finals.push(hash_over(cheapest_total, n.rows));
        let mut all = vec![n.clone(), u.clone()];
        all.extend(finals);
        let mut kept = parallel::add_paths(all);
        let best_at = kept.iter().enumerate().fold(None::<usize>, |b, (i, x)| match b {
            Some(b) if !parallel::cheaper(x, &kept[b]) => Some(b),
            _ => Some(i),
        })?;
        // (Only a plan of the workers' DISTINCT: the serial ones go as before.)
        if same(n, &kept[best_at]) || same(u, &kept[best_at]) {
            return None;
        }
        let mut best = kept.remove(best_at);
        // (The others beside it, in the order add_path kept them.)
        let mut tail: Option<Box<ExNode>> = None;
        for mut x in kept.into_iter().rev() {
            x.alt = tail;
            tail = Some(Box::new(x));
        }
        best.alt = tail;
        Some(best)
    }

    /// consider_groupingsets_paths over sorted input: the rollups after the first (whose
    /// largest set isn't empty) to hash, the most whose tables fit in hash_mem together
    /// (DiscreteKnapsack, each rollup worth one sort saved).
    fn knapsack_rollups(
        &self,
        input: &ExNode,
        groups: &[Expr],
        sets: &[Vec<usize>],
        aggs: &[AggCall],
        chains: &[(Vec<usize>, Vec<usize>)],
        unhashable: &[usize],
    ) -> Vec<usize> {
        if chains.len() < 2 {
            return Vec::new();
        }
        let g = crate::sql::paths::Gucs::read(self.ctx);
        let avail = g.work_mem * g.hash_mem_multiplier * 1024.0;
        let entry = 16.0 + ((16.0 + input.width as f64) / 8.0).ceil() * 8.0 + 16.0 * aggs.len() as f64;
        let set_groups = |s: &Vec<usize>| -> f64 {
            let exprs: Vec<Expr> = s.iter().map(|&i| groups[i].clone()).collect();
            self.group_num_groups(&exprs, &input.cols, input.rows)
        };
        let nrollups = chains.len();
        let scale = (avail / (20.0 * nrollups as f64)).max(1.0);
        let capacity = (avail / scale).floor() as usize;
        let mut items: Vec<usize> = Vec::new();
        let mut weights: Vec<usize> = Vec::new();
        for (ci, (chain, _)) in chains.iter().enumerate().skip(1) {
            if chain.first().is_none_or(|&si| sets[si].is_empty() || sets[si].iter().any(|k| unhashable.contains(k))) {
                continue;
            }
            let groups: f64 = chain.iter().map(|&si| set_groups(&sets[si])).sum();
            let sz = entry * groups;
            weights.push((sz / scale).floor().min(capacity as f64 + 1.0) as usize);
            items.push(ci);
        }
        if items.is_empty() {
            return Vec::new();
        }
        // DiscreteKnapsack, every item of value 1.
        let mut values = vec![0.0f64; capacity + 1];
        let mut picked: Vec<Vec<usize>> = vec![Vec::new(); capacity + 1];
        for (i, &w) in weights.iter().enumerate() {
            if w > capacity {
                continue;
            }
            for j in (w..=capacity).rev() {
                let ow = j - w;
                if values[j] <= values[ow] + 1.0 {
                    if j != ow {
                        picked[j] = picked[ow].clone();
                    }
                    if !picked[j].contains(&i) {
                        picked[j].push(i);
                    }
                    values[j] = values[ow] + 1.0;
                }
            }
        }
        picked[capacity].iter().map(|&i| items[i]).collect()
    }

    fn group_num_groups(&self, groups: &[Expr], cols: &[ColInfo], in_rows: f64) -> f64 {
        use super::extstat::{GroupVar, add_unique};
        if groups.is_empty() {
            return 1.0;
        }
        let cat = self.ctx.catalog();
        let mut total = 1.0;
        let mut vars = Vec::new();
        let mut defaults: Vec<usize> = Vec::new();
        for g in groups {
            if g.ty() == DataType::Bool {
                total *= 2.0;
                continue;
            }
            if let Expr::Col(j, _) = g {
                match cols.get(*j).and_then(|c| c.var.as_ref()) {
                    Some(v) => add_unique(&mut vars, GroupVar::of(&cat, v)),
                    None => {
                        total *= cols.get(*j).and_then(|c| c.distinct).unwrap_or(costs::DEFAULT_NUM_DISTINCT)
                    }
                }
                continue;
            }
            let used = g.columns();
            if used.is_empty() {
                if g.is_volatile() {
                    return costs::clamp_rows(in_rows);
                }
                continue;
            }
            // The expression over one table's columns: its own statistics, if any.
            let base: Vec<Option<&costs::VarRef>> = used
                .iter()
                .map(|j| cols.get(*j).and_then(|c| c.var.as_ref()).filter(|v| v.expr.is_none()))
                .collect();
            if let Some(Some(first)) = base.first()
                && base.iter().all(|v| v.is_some_and(|v| v.table.oid == first.table.oid))
            {
                let over = g.clone().remap_cols(&|j| {
                    cols.get(j).and_then(|c| c.var.as_ref()).map_or(j, |v| v.col)
                });
                if let Some(v) = super::extstat::expr_var(self.ctx, &first.table, first.tuples, &over) {
                    let mut gv = GroupVar::of(&cat, &v);
                    gv.expr = Some(over);
                    add_unique(&mut vars, gv);
                    continue;
                }
            }
            for j in used {
                match cols.get(j).and_then(|c| c.var.as_ref()) {
                    Some(v) => add_unique(&mut vars, GroupVar::of(&cat, v)),
                    None if !defaults.contains(&j) => {
                        defaults.push(j);
                        total *= cols.get(j).and_then(|c| c.distinct).unwrap_or(costs::DEFAULT_NUM_DISTINCT);
                    }
                    None => {}
                }
            }
        }
        super::extstat::num_groups(self.ctx, vars, total, in_rows, Some(in_rows))
    }

    /// make_sort_input_target for a ProjectSet over a Sort: the sort's input target is the
    /// final target's columns that aren't postponed (no set-returning call, nothing volatile
    /// or expensive: a sub-select), then the columns the postponed ones read; a Result over
    /// the sort (under a Gather Merge: apply_projection_to_path) returns them in the
    /// ProjectSet's input order (split_pathtarget_at_srfs: as its expressions read them).
    fn sort_input_target(&self, c: &mut Built, exprs: &[Expr], srfs: &[super::plan::SrfCall]) {
        let iw = c.internal.len();
        let cols_of = |e: &Expr, out: &mut Vec<usize>| {
            let add = |j: usize, out: &mut Vec<usize>| {
                if j < iw && !out.contains(&j) {
                    out.push(j);
                }
            };
            e.walk(&mut |x| match x {
                Expr::Col(j, _) if *j < iw => add(*j, out),
                Expr::Col(j, _) | Expr::Srf(j, _) => {
                    let i = if matches!(x, Expr::Srf(..)) { *j } else { j - iw };
                    if let Some(s) = srfs.get(i) {
                        for a in &s.args {
                            a.walk(&mut |y| {
                                if let Expr::Col(k, _) = y {
                                    add(*k, out);
                                }
                            });
                        }
                    }
                }
                _ => {}
            });
        };
        let postponed = |e: &Expr| {
            let mut p = e.is_volatile();
            e.walk(&mut |x| {
                p |= matches!(x, Expr::Srf(..))
                    || matches!(x, Expr::Col(j, _) if *j >= iw)
                    || matches!(x, Expr::Subquery(s) if s.call.is_none())
            });
            p
        };
        let mut level = Vec::new();
        for e in exprs {
            cols_of(e, &mut level);
        }
        let mut sort_in = Vec::new();
        let mut any_postponed = false;
        for e in exprs {
            if postponed(e) {
                any_postponed = true;
                continue;
            }
            match e {
                Expr::Col(j, _) if *j < iw => {
                    if !sort_in.contains(j) {
                        sort_in.push(*j);
                    }
                }
                _ => return,
            }
        }
        if !any_postponed {
            return;
        }
        for e in exprs.iter().filter(|e| postponed(e)) {
            cols_of(e, &mut sort_in);
        }
        if sort_in == level || sort_in.len() != level.len() {
            return;
        }
        let names = |order: &[usize]| -> Vec<String> {
            order.iter().map(|&j| self.pick(&c.internal[j], Cx::Output)).collect()
        };
        let (level_out, sort_out) = (names(&level), names(&sort_in));
        let verbose = self.o.verbose;
        let wrap = |sort: ExNode| -> ExNode {
            let mut sort = sort;
            let mut r = ExNode::new("Result");
            r.rows = sort.rows;
            r.width = sort.width;
            r.cols = sort.cols.clone();
            r.startup_cost = sort.startup_cost;
            r.total_cost = sort.total_cost + costs::CPU_TUPLE_COST * sort.rows;
            r.key = sort.key;
            r.workers = sort.workers;
            if verbose {
                r.output = level_out.clone();
                sort.output = sort_out.clone();
                sort.fixed_tlist = true;
                if let Some(k) = sort.children.first_mut()
                    && k.is_scan()
                {
                    k.output = sort_out.clone();
                }
            }
            r.children.push(sort.child("Outer"));
            r
        };
        match c.node.node_type {
            "Sort" | "Incremental Sort" => {
                let sort = std::mem::take(&mut c.node);
                c.node = wrap(sort);
            }
            "Gather Merge"
                if c.node
                    .children
                    .first()
                    .is_some_and(|k| matches!(k.node_type, "Sort" | "Incremental Sort")) =>
            {
                let sort = c.node.children.remove(0);
                let r = wrap(sort);
                let extra = r.total_cost - r.children[0].total_cost;
                c.node.total_cost += extra;
                c.node.children.insert(0, r.child("Outer"));
            }
            _ => {}
        }
    }

    fn sort_costs(&self, n: &mut ExNode, c: &ExNode) {
        n.rows = c.rows;
        n.width = c.width;
        n.cols = c.cols.clone();
        (n.startup_cost, n.total_cost) = costs::sort_cost(c.total_cost, c.rows, c.width, None);
    }

    fn append(&mut self, arms: &[&Arc<Plan>]) -> Built {
        self.append_of(arms, false)
    }

    fn append_of(&mut self, arms: &[&Arc<Plan>], inh: bool) -> Built {
        self.append_of_ranked(arms, inh, false)
    }

    /// `leftmost_last`: the first arm's range table entry comes after the others'
    /// (flatten_simple_union_all).
    fn append_of_ranked(&mut self, arms: &[&Arc<Plan>], inh: bool, leftmost_last: bool) -> Built {
        let mut n = ExNode::new("Append");
        // (A partitionwise join's child joins: each one's column statistics, for the
        // partitioned tables' columns above.)
        let mut arm_vars: Vec<Vec<Option<costs::VarRef>>> = Vec::new();
        let mut names = None;
        let mut members = Vec::new();
        let mut member_tuples: Vec<f64> = Vec::new();
        for (i, a) in arms.iter().enumerate() {
            // (A UNION ALL arm reading one table is an appendrel member too.)
            let one_table = match &***a {
                Plan::Scan(_) => true,
                Plan::Project { input, .. } => matches!(&**input, Plan::Scan(_)),
                _ => false,
            };
            let saved = std::mem::replace(&mut self.member_scan, inh || one_table);
            // (A UNION ALL arm with quals stays a subquery, is_safe_append_member: a query
            // level of its own, its InitPlans at its top.)
            let quals = match &***a {
                Plan::Scan(s) => s.filter.is_some(),
                Plan::Project { input, .. } => !matches!(&**input, Plan::Scan(s) if s.filter.is_none()),
                _ => true,
            };
            let own_level = !inh && quals;
            if own_level {
                self.inits.push(Vec::new());
            }
            let mut c = self.node(a);
            if own_level {
                let mut inits = std::mem::take(&mut c.node.initplans);
                inits.extend(self.inits.pop().unwrap_or_default());
                // (Only its own: one another arm reads too is the parent level's, a qual
                // pushed down into every arm.)
                let others: std::collections::HashSet<String> = arms
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .flat_map(|(_, x)| plan_subquery_ids(x))
                    .filter_map(|id| self.labels.get(&id).map(|l| l.name.clone()))
                    .collect();
                let (shared, mut inits): (Vec<ExNode>, Vec<ExNode>) = inits
                    .into_iter()
                    .partition(|x| x.subplan_name.as_ref().is_some_and(|n| others.contains(n)));
                if !shared.is_empty() {
                    match self.inits.last_mut() {
                        Some(level) => level.extend(shared),
                        None => c.node.initplans.extend(shared),
                    }
                }
                if !inits.is_empty() {
                    // SS_charge_for_initplans: its paths pay for them; one of them not
                    // parallel safe (a Gather in it) leaves none of its paths parallel safe
                    // (a Gather below them stays), else its partial plan keeps them too.
                    let unsafe_inits = inits.iter().any(parallel::has_gather);
                    if unsafe_inits {
                        self.settle(&mut c.node);
                    }
                    let cost: f64 = inits.iter().filter(|i| !i.charged).map(|i| i.total_cost).sum();
                    c.node.startup_cost += cost;
                    c.node.total_cost += cost;
                    // (Its partial plan pays for all of them, the ones the level's top
                    // node already paid for too: every path of the level does.)
                    let pcost: f64 = inits.iter().map(|i| i.init_charge()).sum();
                    for i in &mut inits {
                        i.charged = true;
                    }
                    if !unsafe_inits && let Some(pt) = c.node.partial.as_deref_mut() {
                        pt.startup_cost += pcost;
                        pt.total_cost += pcost;
                        let mut own = inits.clone();
                        own.append(&mut pt.initplans);
                        pt.initplans = own;
                    }
                    inits.append(&mut c.node.initplans);
                    c.node.initplans = inits;
                    if unsafe_inits {
                        c.node.par_safe = false;
                    }
                }
            }
            self.member_scan = saved;
            // (Its relids: a partition's in bound order; a UNION ALL arm kept a subquery
            // its own entry's, the leftmost's copy after the others; one pulled up the
            // relations it brings, after all the arms' entries, in the arms' order.)
            c.node.rank = if inh || own_level {
                if leftmost_last && i == 0 { arms.len() } else { i }
            } else {
                2 * arms.len() + 1 + i
            };
            if own_level {
                c.node.arm = addr(a);
            }
            if let Some(pt) = c.node.partial.as_deref_mut() {
                pt.rank = c.node.rank;
                pt.arm = c.node.arm;
            }
            members.push(c.node.clone());
            c.node.partial = None;
            c.node.out_names = c.exported.iter().map(|k| self.pick(k, Cx::Upper)).collect();
            if i == 0 {
                n.startup_cost = c.node.startup_cost;
                n.cols = c.node.cols.iter().map(|x| ColInfo { var: None, ..x.clone() }).collect();
            }
            if !inh {
                arm_vars.push(c.node.cols.iter().map(|x| x.var.clone()).collect());
            }
            n.rows += c.node.rows;
            n.width = n.width.max(c.node.width);
            member_tuples.push(c.node.cols.iter().find_map(|x| x.var.as_ref().map(|v| v.tuples)).unwrap_or(c.node.rows));
            names.get_or_insert(c.exported);
            // (A UNION ALL arm's own Append is flattened into this one:
            // accumulate_append_subpath.)
            // (Not a leaf query planned apart, a subquery: its Append is a member.)
            let apart = {
                let mut x = &***a;
                while let Plan::Project { input, .. } = x {
                    x = input;
                }
                matches!(x, Plan::Barrier { secure: false, .. })
            };
            if !inh && !apart && c.node.node_type == "Append" && c.node.partial.is_none() {
                // (Its members pruned as the executor starts count as this one's.)
                let removed: usize = c
                    .node
                    .details
                    .iter()
                    .filter_map(|d| match d {
                        Detail::Text("Subplans Removed", k) => k.parse::<usize>().ok(),
                        _ => None,
                    })
                    .sum();
                if removed > 0 {
                    add_removed(&mut n.details, removed);
                }
                n.total_cost += c.node.children.iter().map(|k| k.total_cost).sum::<f64>();
                n.children.extend(c.node.children.into_iter().map(|k| k.child("Member")));
                continue;
            }
            n.total_cost += c.node.total_cost;
            n.children.push(c.node.child("Member"));
        }
        // cost_append: half a tuple cost per row passed up.
        n.total_cost += n.rows * costs::CPU_TUPLE_COST * 0.5;
        // A partitioned table's columns: the parent's (its inherited statistics), over
        // all the members' tuples.
        // (A plain inheritance parent is its own first member, the others its
        // descendants.)
        let first = arms.first().and_then(|a| pwagg::member_scan_of(a));
        let plain_parent = first.filter(|s| s.part.is_none()).and_then(|s| {
            arms.iter()
                .skip(1)
                .filter_map(|a| pwagg::member_scan_of(a))
                .any(|m| m.table.inherits.contains(&s.table.oid))
                .then(|| s.table.clone())
        });
        // (The Append of a sub-partitioned partition's own members: that partition's.)
        let common = first.and_then(|s| s.part.as_ref()).and_then(|l| {
            let others: Vec<Vec<u32>> = arms
                .iter()
                .skip(1)
                .filter_map(|a| pwagg::member_scan_of(a))
                .map(|m| m.part.as_ref().map_or_else(Vec::new, |l| l.iter().map(|x| x.parent.oid).collect()))
                .collect();
            l.iter()
                .map(|x| x.parent.clone())
                .find(|p| others.iter().all(|o| o.contains(&p.oid)))
        });
        let top = first.and_then(|s| s.part.as_ref()).and_then(|l| l.last()).map(|l| l.parent.clone());
        if inh
            && let Some(parent) = common.clone().or(top.clone()).or(plain_parent)
        {
            let tuples: f64 = member_tuples.iter().sum();
            // (The Append's columns are the top parent's: the same names in this one.)
            let layout = top.unwrap_or_else(|| parent.clone());
            for (k, c) in n.cols.iter_mut().enumerate() {
                let Some(lc) = layout.columns.get(k).filter(|c| !c.dropped) else { continue };
                let Some(pk) = parent.columns.iter().position(|pc| !pc.dropped && pc.name == lc.name) else {
                    continue;
                };
                c.var = Some(costs::VarRef {
                    table: parent.clone(),
                    col: pk,
                    tuples,
                    expr: None,
                    rows: n.rows,
                    inh: true,
                });
            }
        }
        // A partitionwise join's columns: its partitioned tables' (their inherited
        // statistics), over all the child joins' partitions.
        // (Inner child joins only: a column an outer join nulls isn't its table's as such.)
        let child_join = |a: &Arc<Plan>| {
            let mut x: &Plan = a;
            while let Plan::Project { input, .. } | Plan::Filter { input, .. } = x {
                x = input;
            }
            matches!(x, Plan::Join { kind: JoinKind::Inner, .. })
        };
        if !inh && arms.len() > 1 && arms.iter().all(|a| child_join(a)) && arm_vars.len() == arms.len() {
            let cat = self.ctx.catalog();
            for (k, c) in n.cols.iter_mut().enumerate() {
                let Some(Some(v)) = arm_vars[0].get(k) else { continue };
                if v.expr.is_some() {
                    continue;
                }
                let Some(pcol) = v.table.columns.get(v.col) else { continue };
                // (The root of the partition tree the column's table is in.)
                let mut root = v.table.clone();
                while let Some(&po) = root.inherits.first()
                    && let Some(pt) = cat.table(po)
                    && pt.partkey.is_some()
                {
                    root = pt.clone();
                }
                if Arc::ptr_eq(&root, &v.table) {
                    continue;
                }
                let Some(rk) = root.columns.iter().position(|rc| !rc.dropped && rc.name == pcol.name) else {
                    continue;
                };
                let tuples: f64 = arm_vars
                    .iter()
                    .map(|vs| vs.get(k).and_then(|x| x.as_ref()).map_or(0.0, |x| x.tuples))
                    .sum();
                c.var = Some(costs::VarRef {
                    table: root,
                    col: rk,
                    tuples,
                    expr: None,
                    rows: 0.0,
                    inh: true,
                });
            }
        }
        // (A UNION ALL's arms in parallel too, an Append of their partial plans.)
        n.par_safe = !members.is_empty() && members.iter().all(|m| m.par_safe);
        n.partial = self.partial_append(&n, &members).map(Box::new);
        Built::passthrough(n, names.unwrap_or_default())
    }

    fn scan(&mut self, p: &Arc<Plan>, s: &ScanPlan) -> Built {
        let refname = self.refname(p);
        let t = &s.table;
        let mut names = self.table_names(t, &refname);
        // FROM-clause column aliases name the columns.
        if let Some(al) = &s.col_aliases {
            for (c, n) in t.columns.iter().zip(names.iter_mut()) {
                if let Some((_, a)) = al.iter().find(|(o, _)| *o == c.name) {
                    *n = Name::col(&refname, a);
                }
            }
        }
        let cat = self.ctx.catalog();
        let size = costs::rel_size(t, self.ctx);
        let mut cols: Vec<ColInfo> = (0..t.columns.len())
            .map(|i| ColInfo {
                width: if t.columns[i].dropped {
                    0
                } else {
                    costs::col_width(&cat, t, i)
                },
                var: Some(costs::VarRef {
                    table: t.clone(),
                    col: i,
                    tuples: size.tuples,
                    expr: None,
                    rows: 0.0,
                    inh: false,
                }),
                distinct: None,
            })
            .collect();
        let mut width: u32 = cols.iter().map(|c| c.width).sum();
        if s.with_key {
            names.push(Name::col(&refname, "ctid"));
            width += 6;
            cols.push(ColInfo {
                width: 6,
                var: None,
                distinct: None,
            });
        }
        if s.sys {
            names.extend(
                ["ctid", "xmin", "cmin", "xmax", "cmax", "tableoid"]
                    .map(|c| Name::col(&refname, c)),
            );
        }
        // Only the columns used above (the scan's reltarget), and a row mark's ctid.
        if let Some(m) = self.needed.get(&addr(p)) {
            width = cols
                .iter()
                .enumerate()
                .filter(|(i, _)| m.get(*i).copied().unwrap_or(false))
                .map(|(_, c)| c.width)
                .sum();
        }
        if self.rowmarks.contains(&addr(p)) {
            width += 6;
            // (Its ctid; an inheritance parent's (relhassubclass) tableoid too, after every
            // relation's ctid: expand_inherited_rtentry adds it.)
            let mut junk = vec![((0, s.rti), format!("{}.ctid", quote_ident(&refname)))];
            if cat.class_has_subclass.contains(&t.oid) {
                junk.push(((1, s.rti), format!("{}.tableoid", quote_ident(&refname))));
            }
            self.rowmark_junk.insert(addr(p), junk);
        }
        let mut parts = Vec::new();
        if let Some(f) = &s.filter {
            conjuncts(f, &mut parts);
        }
        if s.current_of.is_none()
            && let Some(d) = super::paths::describe(self.ctx, s)
        {
            // (A volatile clause of no columns is the relation's, as distribute_qual_to_rels
            // puts it: not a gating qual.)
            // (Nor is a clause over a PlaceHolderVar, or a part of one: pull_varnos counts
            // the PlaceHolderVar's ph_eval_at, so distribute_qual_to_rels makes it a
            // restriction of the relation computing it, `Filter: (1 = 1)`.)
            let in_phv = |e: &Expr| {
                s.phv_quals.iter().any(|q| {
                    let mut sub = Vec::new();
                    conjuncts(q, &mut sub);
                    sub.iter().any(|x| x.same_as(e))
                })
            };
            let gating: Vec<Expr> = parts
                .iter()
                .filter(|e| e.columns().is_empty() && !super::paths::expr_volatile_deep(e) && !in_phv(e))
                .cloned()
                .collect();
            let mut n = self.planned_scan(p, s, d, &names);
            n.rel_order = s.reltarget.0.iter().filter_map(|&c| names.get(c)).map(|nm| self.pick(nm, Cx::Output)).collect();
            n.width = width;
            for c in &mut cols {
                if let Some(v) = &mut c.var {
                    v.rows = n.rows;
                }
            }
            n.cols = cols;
            n.key = Some(addr(p));
            n.subplans.append(&mut self.pending);
            if gating.is_empty() && s.mark.is_none() {
                match self.partial_scan(p, s, &names, &n) {
                    Some(pn) if pn.node_type.is_empty() => n.par_safe = true,
                    Some(pn) => {
                        n.par_safe = true;
                        n.partial = Some(Box::new(pn));
                        let others = self.ordered_partial_scans(p, s, &names, &n);
                        let cheapest = n.partial.as_deref().map(|c| (c.title.clone(), c.order.clone()));
                        n.partials = others
                            .into_iter()
                            .filter(|o| cheapest.as_ref() != Some(&(o.title.clone(), o.order.clone())))
                            .collect();
                        self.ordered_scan_gathers(s, &mut n);
                    }
                    // (A parameterized scan: a worker of a partial nested loop can run it.)
                    None => n.par_safe = self.param_scan_safe(s),
                }
            }
            if s.mark.is_some() {
                n = self.lock_scan(n, p, s, &names);
            }
            if !gating.is_empty() {
                n = self.gate(n, &gating, &names);
            }
            return Built::passthrough(n, names);
        }
        let mut n;
        let mut index_conds = Vec::new();
        let mut rest = Vec::new();
        let mut gating = Vec::new();
        let base = 1000.0_f64;
        match &s.index {
            _ if s.current_of.is_some() => {
                n = ExNode::new("Tid Scan");
                n.rows = 1.0;
                n.total_cost = 4.01;
                let cursor = match &s.current_of {
                    Some(Expr::Const(v, _)) => crate::types::text::format_value(
                        v,
                        &crate::types::DataType::Text,
                        self.ctx.fmt(),
                    ),
                    _ => "$0".into(),
                };
                n.details
                    .push(Detail::Text("TID Cond", format!("CURRENT OF {cursor}")));
                // (The scan's other quals, row security's say, are its filter.)
                rest.extend(parts);
            }
            Some((ix, b)) => {
                let usable = (b.eq.len() + 1).min(ix.columns.len());
                let keys: Vec<usize> = ix.columns[..usable]
                    .iter()
                    .filter_map(|c| match c.key {
                        crate::catalog::IndexKey::Column(k) => Some(k),
                        _ => None,
                    })
                    .collect();
                for e in parts {
                    match index_qual(&e, &keys) {
                        Some(q) => index_conds.push((e, q)),
                        None if e.columns().is_empty() && !super::paths::expr_volatile_deep(&e) => gating.push(e),
                        None => rest.push(e),
                    }
                }
                index_conds.sort_by_key(|(_, (k, _))| keys.iter().position(|x| x == k));
                n = ExNode::new("Index Scan");
                n.title = format!(
                    "Index Scan{} using {}",
                    if s.reverse { " Backward" } else { "" },
                    quote_ident(&ix.name)
                );
                n.target.push((
                    "Scan Direction",
                    if s.reverse { "Backward" } else { "Forward" }.into(),
                    false,
                ));
                n.target.push(("Index Name", ix.name.clone(), false));
                let unique = (ix.unique || ix.primary) && b.eq.len() == ix.columns.len();
                n.rows = if unique { 1.0 } else { base * 0.05 };
                n.startup_cost = 0.15;
                n.total_cost = if unique { 8.17 } else { 4.0 + n.rows * 0.5 };
            }
            None => {
                for e in parts {
                    if e.columns().is_empty() && !super::paths::expr_volatile_deep(&e) {
                        gating.push(e)
                    } else {
                        rest.push(e)
                    }
                }
                n = ExNode::new(if s.sample.is_some() {
                    "Sample Scan"
                } else {
                    "Seq Scan"
                });
                // cost_seqscan.
                n.rows = size.tuples;
                n.total_cost =
                    size.pages * costs::SEQ_PAGE_COST + size.tuples * costs::CPU_TUPLE_COST;
            }
        }
        if let Some(ts) = &s.sample {
            let mut text = format!(
                "{} ({})",
                ts.method,
                self.show(&ts.percent, &names, Cx::Scan)
            );
            if let Some(r) = &ts.repeatable {
                text.push_str(&format!(" REPEATABLE ({})", self.show(r, &names, Cx::Scan)));
            }
            n.details.push(Detail::Text("Sampling", text));
        }
        let schema = self.ctx.catalog().schema_name(t.schema).to_string();
        self.target(
            &mut n,
            Some("Relation Name"),
            Some(&t.name),
            Some(&schema),
            &refname,
        );
        if !index_conds.is_empty() {
            let mut texts = Vec::new();
            for (e, (k, commuted)) in &index_conds {
                let text = match e {
                    Expr::Call(def, args, _) if *commuted => {
                        let col = self.pick(&names[*k], Cx::Scan);
                        let other = self.show(strip_widening(&args[0]), &names, Cx::Scan);
                        format!("({col} {} {other})", commute(def.name))
                    }
                    e => self.show(e, &names, Cx::Scan),
                };
                texts.push(text);
            }
            n.details.push(Detail::Text(
                "Index Cond",
                and_text(texts).unwrap_or_default(),
            ));
        }
        if s.index.is_none() && s.current_of.is_none() {
            let vars = |i: usize| cols.get(i).and_then(|c| c.var.clone());
            let est = costs::Est {
                cat: &cat,
                fmt: self.ctx.fmt(),
                vars: &vars,
                exprs: &[],
                endpoint: None,
                distinct: None,
                user: None,
                same_rel: true,
            };
            let qc: f64 = rest.iter().map(costs::qual_cost).sum();
            n.total_cost += size.tuples * qc;
            n.rows = costs::clamp_rows(size.tuples * est.clauses(&rest));
        } else if let (Some((ix, b)), None) = (&s.index, &s.current_of) {
            let vars = |i: usize| cols.get(i).and_then(|c| c.var.clone());
            let est = costs::Est {
                cat: &cat,
                fmt: self.ctx.fmt(),
                vars: &vars,
                exprs: &[],
                endpoint: None,
                distinct: None,
                user: None,
                same_rel: true,
            };
            let conds: Vec<Expr> = index_conds.iter().map(|(e, _)| e.clone()).collect();
            let index_sel = est.clauses(&conds);
            let mut all = conds.clone();
            all.extend(rest.iter().cloned());
            let first_col = ix.columns.first().and_then(|c| match c.key {
                crate::catalog::IndexKey::Column(k) => Some(k),
                _ => None,
            });
            let mut correlation = first_col.map_or(0.0, |k| costs::correlation(&cat, t.oid, k));
            if ix.columns.len() > 1 {
                correlation *= 0.75;
            }
            let db = &self.ctx.sess.db;
            let scan = costs::IndexScan {
                heap: size,
                index_pages: crate::exec::sizing::btree_blocks(ix.oid, db).unwrap_or(1) as f64,
                tree_height: crate::exec::sizing::btree_height(ix.oid, db).unwrap_or(0),
                index_sel,
                boundary_sel: index_sel,
                unique_eq: (ix.unique || ix.primary)
                    && b.eq.len() == ix.columns.len()
                    && index_conds.len() == ix.columns.len(),
                nquals: conds.len(),
                correlation,
                filter_cost: rest.iter().map(costs::qual_cost).sum(),
            };
            (n.startup_cost, n.total_cost) = costs::index_scan_cost(&scan);
            n.rows = costs::clamp_rows(size.tuples * est.clauses(&all));
        }
        if !rest.is_empty() {
            let saved = self.num_exec.replace(n.rows * 2.0);
            let texts = rest
                .iter()
                .map(|e| self.show(e, &names, Cx::Scan))
                .collect();
            self.num_exec = saved;
            n.details
                .push(Detail::Text("Filter", and_text(texts).unwrap_or_default()));
            n.details.push(Detail::Removed(
                "Rows Removed by Filter",
                vec![Removed::Scan(scan_addr(s))],
            ));
        }
        if s.index.is_some() {
            n.details.push(Detail::IndexSearches(scan_addr(s)));
        }
        n.width = width;
        for c in &mut cols {
            if let Some(v) = &mut c.var {
                v.rows = n.rows;
            }
        }
        n.cols = cols;
        n.key = Some(addr(p));
        n.subplans.append(&mut self.pending);
        if s.mark.is_some() {
            n = self.lock_scan(n, p, s, &names);
        }
        if !gating.is_empty() {
            n = self.gate(n, &gating, &names);
        }
        Built::passthrough(n, names)
    }

    /// An index condition as EXPLAIN shows it: the key on the left, without the binder's
    /// widening casts.
    fn index_cond(&mut self, e: &Expr, names: &[Name], plain: &[usize]) -> String {
        // (The index key's binary-compatible relabeling is gone: the operand is the index
        // column, fix_indexqual_operand.)
        let strip = |x: &Expr| match x {
            Expr::Cast(inner, DataType::Varchar(_), DataType::Text, _)
                if matches!(**inner, Expr::Col(c, _) if plain.contains(&c)) =>
            {
                (**inner).clone()
            }
            x => x.clone(),
        };
        let e = match e {
            Expr::Call(d, args, ty) if args.len() == 2 && matches!(args[0], Expr::Cast(..)) => {
                Expr::Call(d.clone(), vec![strip(&args[0]), args[1].clone()], ty.clone())
            }
            e => e.clone(),
        };
        self.show(&e, names, Cx::Scan)
    }

    /// An index-only scan's expression key columns (its conditions read them as index
    /// columns: `INDEX_VAR`s, which ruleutils parenthesizes when they aren't Vars).
    fn ios_expr_keys(&self, s: &ScanPlan) -> Vec<Expr> {
        let (super::plan::Access::Index { only: true, .. }, Some((ix, _))) = (&s.access, &s.index)
        else {
            return Vec::new();
        };
        let Ok(bound) = crate::sql::binder::bound_indexes(&s.table, self.ctx) else {
            return Vec::new();
        };
        bound
            .iter()
            .find(|b| b.index.oid == ix.oid)
            .map(|b| b.keys.iter().filter(|k| !matches!(k, Expr::Col(..))).cloned().collect())
            .unwrap_or_default()
    }

    /// `text` (condition `e` shown) with the index expression columns it reads in parentheses.
    fn paren_index_exprs(&mut self, mut text: String, e: &Expr, keys: &[Expr], names: &[Name]) -> String {
        for k in keys {
            let mut found = false;
            e.walk(&mut |x| found |= x.same_as(k));
            if found {
                let shown = self.show(k, names, Cx::Scan);
                text = text.replace(&shown, &format!("({shown})"));
            }
        }
        text
    }

    /// A scan the planner chose by cost: its node (and bitmap children) with the planner's
    /// estimates.
    fn planned_scan(
        &mut self,
        p: &Arc<Plan>,
        s: &ScanPlan,
        d: super::paths::ScanDesc,
        names: &[Name],
    ) -> ExNode {
        use super::plan::Access;
        let refname = self.refname(p);
        let t = &s.table;
        let mut n = match &s.access {
            Access::Index { only, .. } => {
                let ix_name = s.index.as_ref().map(|i| i.0.name.clone()).unwrap_or_default();
                let ty = if *only { "Index Only Scan" } else { "Index Scan" };
                let mut n = ExNode::new(ty);
                n.title = format!(
                    "{ty}{} using {}",
                    if s.reverse { " Backward" } else { "" },
                    quote_ident(&ix_name)
                );
                n.target.push((
                    "Scan Direction",
                    if s.reverse { "Backward" } else { "Forward" }.into(),
                    false,
                ));
                n.target.push(("Index Name", ix_name, false));
                n
            }
            Access::Bitmap(..) => ExNode::new("Bitmap Heap Scan"),
            Access::Tid { .. } => ExNode::new("Tid Scan"),
            Access::TidRange(_) => ExNode::new("Tid Range Scan"),
            _ => ExNode::new("Seq Scan"),
        };
        let schema = self.ctx.catalog().schema_name(t.schema).to_string();
        self.target(
            &mut n,
            Some("Relation Name"),
            Some(&t.name),
            Some(&schema),
            &refname,
        );
        n.startup_cost = d.startup;
        n.total_cost = d.total;
        n.rows = d.rows;
        n.disabled = d.disabled;
        if !d.index_conds.is_empty() {
            // (An index-only scan shows its conditions over the index's columns, an index
            // scan its original ones.)
            let only = matches!(s.access, Access::Index { only: true, .. });
            let plain: Vec<usize> = s
                .index
                .as_ref()
                .filter(|_| only)
                .map(|(ix, _)| {
                    ix.columns
                        .iter()
                        .filter_map(|c| match c.key {
                            crate::catalog::IndexKey::Column(i) => Some(i),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            let keys = self.ios_expr_keys(s);
            // (setrefs: an index scan shows indexqualorig, fixed for NUM_EXEC_QUAL and
            // initialized; an index-only scan its indexqual, fixed for one run and not
            // initialized under EXPLAIN alone, while its recheckqual, the same quals as
            // written, is fixed for NUM_EXEC_QUAL and initialized: its SubPlans are the ones
            // attached.)
            let saved = (self.num_exec, self.detach);
            if only && d.index_conds.iter().any(|e| has_subselect(&self.inline_sub_params(e).unwrap_or_else(|| e.clone()))) {
                self.num_exec = Some(n.rows * 2.0);
                for e in &d.index_conds {
                    let _ = self.show(e, names, Cx::Scan);
                }
            }
            self.num_exec = Some(if only { 1.0 } else { n.rows * 2.0 });
            self.detach = only && !self.o.analyze;
            let texts = d
                .index_conds
                .iter()
                .map(|e| {
                    let text = self.index_cond(e, names, &plain);
                    self.paren_index_exprs(text, e, &keys, names)
                })
                .collect();
            (self.num_exec, self.detach) = saved;
            n.details
                .push(Detail::Text("Index Cond", and_text(texts).unwrap_or_default()));
            n.details
                .push(Detail::Removed("Rows Removed by Index Recheck", Vec::new()));
        }
        if !d.tid_conds.is_empty() {
            // (A TID scan's quals are alternatives, shown ORed; a range scan's are ANDed.)
            let text = match &s.access {
                Access::Tid { .. } if d.tid_conds.len() > 1 => {
                    self.show(&Expr::Or(d.tid_conds.clone()), names, Cx::Scan)
                }
                _ => {
                    let texts = d
                        .tid_conds
                        .iter()
                        .map(|e| self.show(e, names, Cx::Scan))
                        .collect();
                    and_text(texts).unwrap_or_default()
                }
            };
            n.details.push(Detail::Text("TID Cond", text));
        }
        if !d.order_by.is_empty() {
            let keys = self.ios_expr_keys(s);
            let texts = d
                .order_by
                .iter()
                .map(|e| {
                    let text = self.index_cond(e, names, &[]);
                    self.paren_index_exprs(text, e, &keys, names)
                })
                .collect();
            n.details
                .push(Detail::Text("Order By", and_text(texts).unwrap_or_default()));
        }
        if !d.recheck.is_empty() {
            // (bitmapqualorig: fixed for NUM_EXEC_QUAL, initialized.)
            let saved = self.num_exec.replace(n.rows * 2.0);
            let texts = d
                .recheck
                .iter()
                .map(|e| self.show(e, names, Cx::Scan))
                .collect();
            self.num_exec = saved;
            n.details.push(Detail::Text(
                "Recheck Cond",
                and_text(texts).unwrap_or_default(),
            ));
            n.details
                .push(Detail::Removed("Rows Removed by Index Recheck", Vec::new()));
        }
        if !d.filter.is_empty() {
            let saved = self.num_exec.replace(n.rows * 2.0);
            let texts = d
                .filter
                .iter()
                .map(|e| self.show(e, names, Cx::Scan))
                .collect();
            self.num_exec = saved;
            n.details
                .push(Detail::Text("Filter", and_text(texts).unwrap_or_default()));
            n.details.push(Detail::Removed(
                "Rows Removed by Filter",
                vec![Removed::Scan(scan_addr(s))],
            ));
        }
        match &s.access {
            Access::Index { only, .. } => {
                if *only {
                    n.details.push(Detail::HeapFetches(scan_addr(s)));
                }
                n.details.push(Detail::IndexSearches(scan_addr(s)));
                // (A GiST or SP-GiST scan's ordering operators compute their values: as an
                // upper node's sort key over them shows, parenthesized.)
                let knn = s
                    .index
                    .as_ref()
                    .is_some_and(|(ix, _)| matches!(ix.method.as_str(), "gist" | "spgist"));
                n.order = super::paths::index_order(self.ctx, s)
                    .iter()
                    .map(|k| {
                        let shown = self.show(&k.expr, names, Cx::Upper);
                        let shown = if knn && !matches!(k.expr, Expr::Col(..)) {
                            format!("({shown})")
                        } else {
                            shown
                        };
                        shown + &sort_suffix(k)
                    })
                    .collect();
            }
            Access::Bitmap(..) => {
                n.details.push(Detail::HeapBlocks(scan_addr(s)));
                if let Some(b) = &d.bitmap {
                    let child = self.bitmap_node(b, names);
                    n.children.push(child.child("Outer"));
                }
            }
            _ => {}
        }
        n
    }

    fn bitmap_node(&mut self, b: &super::paths::BitmapNode, names: &[Name]) -> ExNode {
        use super::paths::BitmapNode;
        match b {
            BitmapNode::Index {
                name,
                quals,
                total,
                rows,
                key,
            } => {
                let mut n = ExNode::new("Bitmap Index Scan");
                n.title = format!("Bitmap Index Scan on {}", quote_ident(name));
                n.target.push(("Index Name", name.clone(), false));
                n.total_cost = *total;
                n.rows = *rows;
                n.key = Some(*key);
                if !quals.is_empty() {
                    // (indexqualorig, fixed for NUM_EXEC_QUAL, is shown; the executor
                    // initializes only the runtime keys, indexqual fixed for one run, and
                    // those not under EXPLAIN alone.)
                    let saved = (self.num_exec, self.detach);
                    self.num_exec = Some(*rows * 2.0);
                    self.detach = true;
                    let texts = quals.iter().map(|e| self.index_cond(e, names, &[])).collect();
                    if self.o.analyze {
                        self.num_exec = Some(1.0);
                        self.detach = false;
                        let before = self.pending.len();
                        for e in quals {
                            let _ = self.index_cond(e, names, &[]);
                        }
                        n.subplans.extend(self.pending.drain(before..));
                    }
                    (self.num_exec, self.detach) = saved;
                    n.details
                        .push(Detail::Text("Index Cond", and_text(texts).unwrap_or_default()));
                }
                n.details.push(Detail::IndexSearches(*key));
                n.width = 0;
                n
            }
            BitmapNode::And(list, cost, rows, key) | BitmapNode::Or(list, cost, rows, key) => {
                let ty = if matches!(b, BitmapNode::And(..)) {
                    "BitmapAnd"
                } else {
                    "BitmapOr"
                };
                let mut n = ExNode::new(ty);
                n.startup_cost = *cost;
                n.total_cost = *cost;
                n.rows = *rows;
                n.key = Some(*key);
                for c in list {
                    let c = self.bitmap_node(c, names);
                    n.children.push(c.child("Member"));
                }
                n
            }
        }
    }

    /// A locked scan under its LockRows: VERBOSE, the scan returns the columns read and its
    /// row mark (ctid), and so does the LockRows.
    fn lock_scan(&self, mut n: ExNode, p: &Arc<Plan>, s: &ScanPlan, names: &[Name]) -> ExNode {
        if self.o.verbose && n.output.is_empty() {
            let mask = self.needed.get(&addr(p));
            let mut out: Vec<String> = names
                .iter()
                .take(s.table.columns.len())
                .enumerate()
                .filter(|(i, _)| mask.is_none_or(|m| m.get(*i).copied().unwrap_or(false)))
                .map(|(_, x)| self.pick(x, Cx::Output))
                .collect();
            out.push(self.pick(&Name::col(&self.refname(p), "ctid"), Cx::Output));
            n.output = out;
        }
        lock_rows(n)
    }

    /// A Result node that runs `child` only when the variable-free quals hold.
    fn gate(&mut self, mut child: ExNode, quals: &[Expr], names: &[Name]) -> ExNode {
        use crate::types::Value;
        // (A scan under it returns its physical target list: the Result projects.)
        if self.o.verbose
            && child.output.is_empty()
            && child.is_scan()
            && child.node_type != "Index Only Scan"
        {
            child.output = names.iter().map(|n| self.pick(n, Cx::Output)).collect();
        }
        if quals
            .iter()
            .any(|q| matches!(q, Expr::Const(Value::Bool(false) | Value::Null, _)))
        {
            // Never true: the planner drops the input altogether.
            let mut n = ExNode {
                key: child.key,
                ..self.result(0.0)
            };
            n.details
                .push(Detail::Text("One-Time Filter", "false".into()));
            return n;
        }
        let texts = quals
            .iter()
            .map(|e| self.show(e, names, Cx::Upper))
            .collect();
        let mut n = ExNode {
            key: child.key,
            rows: child.rows,
            width: child.width,
            startup_cost: child.startup_cost,
            total_cost: child.total_cost,
            ..ExNode::new("Result")
        };
        n.details.push(Detail::Text(
            "One-Time Filter",
            and_text(texts).unwrap_or_default(),
        ));
        n.subplans.append(&mut self.pending);
        n.children.push(child.child("Outer"));
        n
    }

    /// find_window_run_conditions: the clauses of `pred` (over a projection of window
    /// functions, `input`) comparing a monotonically increasing window function of the
    /// top WindowAgg with a constant become its Run Condition. Some(remaining clauses)
    /// when any did (`wf < c` and `wf <= c` go; `wf = c` stays, its run condition
    /// `wf <= c`).
    /// trivial_subqueryscan: the subquery's windows return just what the query reads of
    /// it, in its order (no sort or partition key the subquery's columns leave out, no
    /// column unused).
    fn trivial_subquery(&self, input: &Arc<Plan>, key: usize) -> bool {
        let ident: Vec<Expr>;
        let (calls, exprs) = match &**input {
            Plan::Project { input: pin, exprs } => match &**pin {
                Plan::Window { calls, .. } => (calls, exprs),
                _ => return true,
            },
            Plan::Window { input: win_in, calls } => {
                ident = (0..super::binder::plan_width(win_in))
                    .map(|i| Expr::Col(i, DataType::Unknown))
                    .collect();
                (calls, &ident)
            }
            _ => return true,
        };
        if self.needed.get(&key).is_some_and(|n| n.iter().any(|x| !x)) {
            return false;
        }
        calls.iter().all(|c| {
            c.partition_by
                .iter()
                .chain(c.order_by.iter().map(|k| &k.expr))
                .all(|e| {
                    crate::sql::binder::optimize::pseudo_constant(e)
                        || exprs.iter().any(|x| {
                            x.same_as(e) || matches!((x, e), (Expr::Col(a, _), Expr::Col(b, _)) if a == b)
                        })
                })
        })
    }

    /// The order a subquery's rows come in (`c`'s, its plan's), by the subquery's columns
    /// `names` (its column `i` is `c`'s column `map[i]`): convert_subquery_pathkeys, as
    /// far as the keys are columns it returns, at most its ORDER BY's `sorted` keys.
    /// A FROM subquery read by one of its sorted paths (`Plan::Barrier`'s `path`): of its
    /// node and the paths kept beside it (`alt`), the one the query above took (the
    /// cheapest in the keys' order, or the cheapest to start); the others go. `map`: the
    /// node's column of each of the subquery's.
    fn take_sub_path(&self, c: &mut Built, map: &[usize], path: &super::plan::SubPath) {
        let want: Option<Vec<String>> = path
            .keys
            .iter()
            .map(|k| match &k.expr {
                Expr::Col(j, _) => map
                    .get(*j)
                    .and_then(|m| c.exported.get(*m))
                    .map(|n| self.pick(n, Cx::Upper) + &sort_suffix(k)),
                _ => None,
            })
            .collect();
        let mut forms = vec![std::mem::take(&mut c.node)];
        let mut next = forms[0].alt.take();
        while let Some(mut a) = next {
            next = a.alt.take();
            forms.push(*a);
        }
        let fits = |x: &ExNode| want.as_ref().is_some_and(|w| x.order.len() >= w.len() && x.order[..w.len()] == w[..]);
        let cost = |x: &ExNode| (parallel::disabled_nodes(x), if path.fast { x.startup_cost } else { x.total_cost });
        let pick = (0..forms.len())
            .filter(|&i| fits(&forms[i]))
            .min_by(|&a, &b| cost(&forms[a]).partial_cmp(&cost(&forms[b])).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(0);
        c.node = forms.swap_remove(pick);
    }

    fn subquery_order(
        &self,
        c: &Built,
        map: &[usize],
        names: &[Name],
        sorted: Option<usize>,
        needed: Option<&Vec<bool>>,
    ) -> Vec<String> {
        let below: Vec<String> = map
            .iter()
            .map(|&j| c.exported.get(j).map_or(String::new(), |n| self.pick(n, Cx::Upper)))
            .collect();
        let mut order = Vec::new();
        for k in &c.node.order {
            let hit = below.iter().enumerate().find_map(|(i, b)| {
                if b.is_empty() {
                    return None;
                }
                if k == b {
                    Some((i, ""))
                } else {
                    k.strip_prefix(b.as_str()).filter(|r| r.starts_with(' ')).map(|r| (i, r))
                }
            });
            // (A column the query above doesn't read: no pathkey of its.)
            match hit {
                Some((i, rest)) if i < names.len() && needed.is_none_or(|m| m.get(i).copied().unwrap_or(true)) => {
                    order.push(format!("{}{rest}", self.pick(&names[i], Cx::Upper)))
                }
                _ => break,
            }
        }
        if let Some(n) = sorted {
            order.truncate(n);
        }
        order
    }

    /// An Append's members each in an order whose leading keys are its columns (the same
    /// ones, in the same directions, for every member) among `named` (the keys wanted, by
    /// column): those columns, as far as all members share them.
    fn merged_append_keys(&self, append: &ExNode, named: &[(usize, String)]) -> Option<(Vec<usize>, Vec<ExNode>)> {
        let keys_of = |k: &ExNode, names: &[String]| -> Vec<(usize, String)> {
            let mut mine = Vec::new();
            for o in &k.order {
                let base = presorted_name(o);
                let suffix = o[base.len()..].to_string();
                match names.iter().position(|n| *n == base) {
                    Some(i) if named.iter().any(|x| x.0 == i) && !mine.iter().any(|m: &(usize, String)| m.0 == i) => {
                        mine.push((i, suffix))
                    }
                    _ => break,
                }
            }
            mine
        };
        let mut common: Option<Vec<(usize, String)>> = None;
        let mut kids = Vec::new();
        for k in &append.children {
            if k.out_names.is_empty() {
                return None;
            }
            // (A member's other path when it has the order and this one doesn't.)
            let mut chosen = k.clone();
            let mut mine = keys_of(k, &k.out_names);
            if mine.is_empty()
                && let Some(a) = &k.alt
            {
                let am = keys_of(a, &k.out_names);
                if !am.is_empty() {
                    let mut a = (**a).clone();
                    a.out_names = k.out_names.clone();
                    a.relationship = k.relationship;
                    chosen = a;
                    mine = am;
                }
            }
            chosen.alt = None;
            kids.push(chosen);
            common = Some(match common {
                None => mine,
                Some(c) => c.into_iter().zip(mine).take_while(|(a, b)| a == b).map(|(a, _)| a).collect(),
            });
        }
        let keys: Vec<usize> = common?.into_iter().take_while(|(_, s)| s.is_empty()).map(|(i, _)| i).collect();
        (!keys.is_empty()).then_some((keys, kids))
    }

    /// A Subquery Scan (without quals) over `c`, by the subquery's alias.
    fn subquery_scan(&mut self, mut c: Built, key: usize) -> Built {
        self.settle(&mut c.node);
        let mut n = ExNode::new("Subquery Scan");
        let alias = self.sub_alias.as_ref().map_or("unnamed_subquery".to_string(), |i| i.0.clone());
        self.target(&mut n, None, None, None, &alias);
        n.key = Some(key);
        n.rows = c.node.rows;
        n.width = c.node.width;
        n.cols = c.node.cols.clone();
        n.startup_cost = c.node.startup_cost;
        n.total_cost = c.node.total_cost + c.node.rows * costs::CPU_TUPLE_COST;
        let child = std::mem::take(&mut c.node);
        n.children.push(child.child("Subquery"));
        c.node = n;
        c
    }

    fn run_conditions(&mut self, input: &Arc<Plan>, pred: &Expr, c: &mut Built) -> Option<Option<Expr>> {
        // (Over the windows' projection, or the windows themselves when it is theirs.)
        let ident: Vec<Expr>;
        let (win_in, calls, exprs) = match &**input {
            Plan::Project { input: pin, exprs } => match &**pin {
                Plan::Window { input: win_in, calls } => (win_in, calls, exprs),
                _ => return None,
            },
            Plan::Window { input: win_in, calls } => {
                let w = super::binder::plan_width(input);
                ident = (0..w).map(|i| Expr::Col(i, DataType::Unknown)).collect();
                (win_in, calls, &ident)
            }
            _ => return None,
        };
        if c.node.node_type != "WindowAgg" {
            return None;
        }
        let iw = super::binder::plan_width(win_in);
        let mut parts = Vec::new();
        conjuncts(pred, &mut parts);
        let mut conds = Vec::new();
        let mut rest = Vec::new();
        // A lower WindowAgg's calls: run conditions there (by call), the quals kept as the
        // top one's Filter (its rows pass through: find_window_run_conditions).
        let mut lower: Vec<(usize, String)> = Vec::new();
        let mut top_filter: Vec<String> = Vec::new();
        for q in parts {
            let mut taken = false;
            if let Expr::Call(def, args, _) = &q
                && args.len() == 2
                && matches!(def.name, "<" | "<=" | ">" | ">=" | "=")
            {
                let wcol = |e: &Expr| match e {
                    Expr::Col(i, _) => match exprs.get(*i) {
                        Some(Expr::Col(k, _)) if *k >= iw => Some(*k - iw),
                        _ => None,
                    },
                    _ => None,
                };
                let konst = |e: &Expr| e.columns().is_empty() && super::binder::optimize::pseudo_constant(e);
                // (wf on the left: the operator as written; else as if flipped.)
                let found = match (wcol(&args[0]), wcol(&args[1])) {
                    (Some(k), None) if konst(&args[1]) => Some((k, def.name, &args[1], true)),
                    (None, Some(k)) if konst(&args[0]) => {
                        let flipped = match def.name {
                            "<" => ">",
                            "<=" => ">=",
                            ">" => "<",
                            ">=" => "<=",
                            o => o,
                        };
                        Some((k, flipped, &args[0], false))
                    }
                    _ => None,
                };
                let at_top = found
                    .as_ref()
                    .is_some_and(|f| c.node.win_calls.iter().any(|(ci, _)| *ci == f.0));
                let shown_at = |k: usize| -> Option<String> {
                    let mut n = &c.node;
                    loop {
                        if let Some((_, s)) = n.win_calls.iter().find(|(ci, _)| *ci == k) {
                            return Some(s.clone());
                        }
                        if !matches!(n.node_type, "WindowAgg" | "Sort" | "Incremental Sort") || n.children.len() != 1 {
                            return None;
                        }
                        n = &n.children[0];
                    }
                };
                if let Some((k, op, val, wf_left)) = found
                    && let Some(call) = calls.get(k)
                    && let Some(shown) = shown_at(k)
                {
                    // The function's monotonicity (its support function): 1 increasing,
                    // 2 decreasing, 3 both.
                    let mono = match &call.func {
                        WinFunc::Builtin(
                            WinKind::RowNumber
                            | WinKind::Rank
                            | WinKind::DenseRank
                            | WinKind::PercentRank
                            | WinKind::CumeDist
                            | WinKind::Ntile,
                        ) => 1,
                        // int8inc_support: an EXCLUDE spoils it, but for COUNT(*) EXCLUDE
                        // CURRENT ROW; without ORDER BY in RANGE mode all rows are peers;
                        // else the frame starting (ending) at the partition's edge.
                        WinFunc::Agg(a)
                            if a.def.sig.name == "count"
                                && (call.frame.exclude == FrameExclude::NoOthers
                                    || (call.frame.exclude == FrameExclude::CurrentRow
                                        && call.args.is_empty()
                                        && a.filter.is_none())) =>
                        {
                            if call.order_by.is_empty() && call.frame.unit == FrameUnit::Range {
                                3
                            } else {
                                (matches!(call.frame.start, FrameBound::UnboundedPreceding) as u8)
                                    | ((matches!(call.frame.end, FrameBound::UnboundedFollowing)
                                        as u8)
                                        << 1)
                            }
                        }
                        _ => 0,
                    };
                    // (Not over a volatile function.)
                    let mono = if call.args.iter().any(|a| a.is_volatile()) { 0 } else { mono };
                    let v = self.show(val, &[], Cx::Upper);
                    let orig = if wf_left {
                        format!("({shown} {} {v})", def.name)
                    } else {
                        format!("({v} {} {shown})", def.name)
                    };
                    let inc = mono & 1 != 0;
                    let dec = mono & 2 != 0;
                    if !at_top && mono != 0 {
                        let usable = (inc && matches!(op, "<" | "<=")) || (dec && matches!(op, ">" | ">=")) || (mono == 3 && op == "=");
                        let cond = if usable {
                            Some(orig)
                        } else if op == "=" && (inc || dec) {
                            let rop = match (inc, wf_left) {
                                (true, true) | (false, false) => "<=",
                                _ => ">=",
                            };
                            Some(if wf_left {
                                format!("({shown} {rop} {v})")
                            } else {
                                format!("({v} {rop} {shown})")
                            })
                        } else {
                            None
                        };
                        if let Some(cond) = cond {
                            lower.push((k, cond));
                            top_filter.push(if wf_left {
                                format!("(({shown}) {} {v})", def.name)
                            } else {
                                format!("({v} {} ({shown}))", def.name)
                            });
                            taken = true;
                        }
                    } else if (inc && matches!(op, "<" | "<=")) || (dec && matches!(op, ">" | ">=")) || (mono == 3 && op == "=") {
                        conds.push(orig);
                        taken = true;
                    } else if op == "=" && (inc || dec) {
                        let rop = if inc { "<=" } else { ">=" };
                        let rop = if wf_left {
                            rop
                        } else if inc {
                            ">="
                        } else {
                            "<="
                        };
                        conds.push(if wf_left {
                            format!("({shown} {rop} {v})")
                        } else {
                            format!("({v} {rop} {shown})")
                        });
                    }
                }
            }
            if !taken {
                rest.push(q);
            }
        }
        if conds.is_empty() && lower.is_empty() {
            return None;
        }
        fn add_run_condition(n: &mut ExNode, mut conds: Vec<String>) {
            if conds.is_empty() {
                return;
            }
            let text = if conds.len() == 1 {
                conds.pop().unwrap()
            } else {
                and_text(conds).unwrap_or_default()
            };
            let pos = n
                .details
                .iter()
                .position(|d| matches!(d, Detail::Text("Window", _)))
                .map_or(n.details.len(), |p| p + 1);
            n.details.insert(pos, Detail::Text("Run Condition", text));
        }
        add_run_condition(&mut c.node, conds);
        // (Each lower WindowAgg: its own calls' conditions.)
        {
            let mut n = &mut c.node;
            loop {
                if !matches!(n.node_type, "WindowAgg" | "Sort" | "Incremental Sort") || n.children.len() != 1 {
                    break;
                }
                n = &mut n.children[0];
                if n.node_type == "WindowAgg" {
                    let mine: Vec<String> = lower
                        .iter()
                        .filter(|(k, _)| n.win_calls.iter().any(|(ci, _)| ci == k))
                        .map(|(_, t)| t.clone())
                        .collect();
                    add_run_condition(n, mine);
                }
            }
        }
        if !top_filter.is_empty() {
            let pos = c
                .node
                .details
                .iter()
                .rposition(|d| matches!(d, Detail::Text("Run Condition" | "Window", _)))
                .map_or(c.node.details.len(), |p| p + 1);
            let text = if top_filter.len() == 1 {
                top_filter.pop().unwrap()
            } else {
                and_text(top_filter).unwrap_or_default()
            };
            c.node.details.insert(pos, Detail::Text("Filter", text));
        }
        Some(match rest.len() {
            0 => None,
            1 => rest.pop(),
            _ => Some(Expr::And(rest)),
        })
    }

    /// Whether `e` reads a value of a LATERAL join of the current query level (a Var of
    /// another relation, not an enclosing query's parameter).
    fn reads_level_var(&self, e: &Expr) -> bool {
        let mut found = false;
        e.walk(&mut |x| {
            if let Expr::Outer(d, ..) = x
                && *d >= 1
                && self.outer_depth.get(d - 1) == Some(&self.depth)
            {
                found = true;
            }
        });
        found
    }

    fn fold_filter(&mut self, mut c: Built, pred: &Expr, key: usize, setop: bool) -> Built {
        // (A qual workers can't run keeps the node out of parallel plans; a subquery's
        // Subquery Scan checking it, planned apart, may gather its own plan below it.)
        if super::paths::parallel::hazard(pred, &self.ctx.catalog()) != 's' {
            if setop {
                self.settle(&mut c.node);
            }
            c.node.partial = None;
            c.node.par_safe = false;
        }
        self.settle(&mut c.node);
        let input_key = c.node.key.unwrap_or(0);
        let mut parts = Vec::new();
        conjuncts(pred, &mut parts);
        // (An aggregation's HAVING clauses are all its quals: none gates it.)
        let having = c.node.node_type == "Aggregate" && !setop;
        // (A clause reading a LATERAL value of this level, a Var of another relation of
        // it, isn't pseudoconstant: the node's own qual.)
        // (A FROM-less relation's Result (an RTE_RESULT) gates on all of them.)
        let result_rel = c.node.node_type == "Result" && c.node.children.is_empty();
        let (gating, quals): (Vec<Expr>, Vec<Expr>) = parts.into_iter().partition(|e| {
            !having
                && e.columns().is_empty()
                && !super::paths::expr_volatile_deep(e)
                && (result_rel || !self.reads_level_var(e))
        });
        let node = &c.node;
        let scan_like = node.is_scan() || (node.node_type == "Result" && node.children.is_empty());
        // (A subquery's scan shown already: its own quals.)
        let subscan = setop
            && node.node_type == "Subquery Scan"
            && !node.details.iter().any(|d| matches!(d, Detail::Text("Filter", _)));
        if !quals.is_empty() {
            if !setop
                && (scan_like
                    || node.node_type == "Aggregate"
                    || node.is_join()
                    || node.node_type == "Result")
            {
                let cx = if scan_like { Cx::Scan } else { Cx::Upper };
                // (NUM_EXEC_QUAL: twice the rows the node puts out.)
                let out_rows = {
                    let cat = self.ctx.catalog();
                    let cols = c.node.cols.clone();
                    let vars = |i: usize| cols.get(i).and_then(|c| c.var.clone());
                    let nd = |i: usize| cols.get(i).filter(|c| c.var.is_none()).and_then(|c| c.distinct);
                    let est = costs::Est {
                        cat: &cat,
                        fmt: self.ctx.fmt(),
                        vars: &vars,
                        exprs: &[],
                        endpoint: None,
                        distinct: scan_like.then_some(&nd as &dyn Fn(usize) -> Option<f64>),
                        user: None,
                        same_rel: scan_like,
                    };
                    costs::clamp_rows(c.node.rows * est.clauses(&quals))
                };
                let saved = self.num_exec.replace(out_rows * 2.0);
                let texts = quals
                    .iter()
                    .map(|e| self.show(e, &c.internal, cx))
                    .collect();
                self.num_exec = saved;
                let text = and_text(texts).unwrap_or_default();
                let inner_join = c.node.is_join()
                    && c.node
                        .target
                        .iter()
                        .any(|t| t.0 == "Join Type" && t.1 == "Inner");
                let (label, removed) = if inner_join {
                    ("Join Filter", "Rows Removed by Join Filter")
                } else {
                    ("Filter", "Rows Removed by Filter")
                };
                // (A gathered grouping's serial form checks its HAVING too.)
                if let Some(sa) = &mut c.node.safe_alt {
                    sa.add_qual(label, removed, text.clone(), Some(Removed::Diff(input_key)));
                    sa.key = Some(key);
                }
                c.node
                    .add_qual(label, removed, text, Some(Removed::Diff(input_key)));
                c.node.key = Some(key);
                let cat = self.ctx.catalog();
                let cols = c.node.cols.clone();
                let vars = |i: usize| cols.get(i).and_then(|c| c.var.clone());
                let nd = |i: usize| cols.get(i).filter(|c| c.var.is_none()).and_then(|c| c.distinct);
                let est = costs::Est {
                    cat: &cat,
                    fmt: self.ctx.fmt(),
                    vars: &vars,
                    exprs: &[],
                    endpoint: None,
                    distinct: scan_like.then_some(&nd as &dyn Fn(usize) -> Option<f64>),
                    user: None,
                    same_rel: scan_like,
                };
                let qc: f64 = quals.iter().map(costs::qual_cost).sum();
                let sel = est.clauses(&quals);
                // (A join checks them with its other quals, on the tuples those are checked
                // on: final_cost_*join's qp_qual_cost.)
                let tuples = c.node.qual_tuples.filter(|_| c.node.is_join()).unwrap_or(c.node.rows);
                c.node.total_cost += tuples * qc;
                c.node.rows = costs::clamp_rows(c.node.rows * sel);
                if let Some(sa) = &mut c.node.safe_alt {
                    sa.total_cost += sa.rows * qc;
                    sa.rows = costs::clamp_rows(sa.rows * sel);
                }
                // (A relation's distinct counts are no more than its rows.)
                if scan_like {
                    let rows = c.node.rows;
                    for col in &mut c.node.cols {
                        if let Some(d) = &mut col.distinct {
                            *d = d.min(rows);
                        }
                    }
                }
                c.node.subplans.append(&mut self.pending);
            } else {
                // A filter over a node that can't check quals: a subquery scan (named by
                // its alias when known).
                // (Its baserestrictinfo: the equivalence classes' equalities with constants
                // come after the other clauses.)
                let ec_const = |e: &Expr| match e {
                    Expr::Call(d, a, _) if d.name == "=" && a.len() == 2 => {
                        let col = |x: &Expr| matches!(strip_relabel(x), Expr::Col(..));
                        let konst = |x: &Expr| x.columns().is_empty() && !super::paths::expr_volatile_deep(x);
                        (col(&a[0]) && konst(&a[1])) || (konst(&a[0]) && col(&a[1]))
                    }
                    _ => false,
                };
                let quals: Vec<Expr> = quals
                    .iter()
                    .filter(|e| !ec_const(e))
                    .chain(quals.iter().filter(|e| ec_const(e)))
                    .cloned()
                    .collect();
                let sub = self.sub_alias.clone();
                let names: Vec<Name> = match &sub {
                    Some(info) if info.1.len() == c.exported.len() => {
                        info.1.iter().map(|col| Name::col(&info.0, col)).collect()
                    }
                    _ => c.exported.clone(),
                };
                let texts = quals
                    .iter()
                    .map(|e| self.show(e, &names, Cx::Upper))
                    .collect();
                // (Estimated over the subquery's outputs, examine_variable looking into
                // it; cost_subqueryscan's per-tuple qual cost. A LATERAL value of this level
                // is another relation's Var there, `level_vars_hidden`.)
                let est_quals: Vec<Expr> = {
                    let (od, depth) = (&self.outer_depth, self.depth);
                    let level = |d: usize| d >= 1 && od.get(d - 1) == Some(&depth);
                    quals.iter().map(|e| costs::level_vars_hidden(e, &level)).collect()
                };
                let (sel, qc) = {
                    let cat = self.ctx.catalog();
                    let cols = c.node.cols.clone();
                    let vars = |i: usize| cols.get(i).and_then(|c| c.var.clone());
                    let est = costs::Est {
                        cat: &cat,
                        fmt: self.ctx.fmt(),
                        vars: &vars,
                        exprs: &[],
                        endpoint: None,
                        distinct: None,
                        user: None,
                        same_rel: true,
                    };
                    (est.clauses(&est_quals), quals.iter().map(costs::qual_cost).sum::<f64>())
                };
                if subscan {
                    // (Its scan computes it: the quals go on it.)
                    let n = &mut c.node;
                    let below = n.children.first().map_or(n.rows, |k| k.rows);
                    let below_key = n.children.first().and_then(|k| k.key).unwrap_or(input_key);
                    n.details
                        .push(Detail::Text("Filter", and_text(texts).unwrap_or_default()));
                    n.details.push(Detail::Removed(
                        "Rows Removed by Filter",
                        vec![Removed::Diff(below_key)],
                    ));
                    n.key = Some(key);
                    n.rows = costs::clamp_rows(below * sel);
                    n.total_cost += below * qc;
                    n.subplans.append(&mut self.pending);
                } else {
                let mut n = ExNode::new("Subquery Scan");
                let alias = sub.as_ref().map_or("unnamed_subquery", |i| i.0.as_str());
                self.target(&mut n, None, None, None, alias);
                n.details
                    .push(Detail::Text("Filter", and_text(texts).unwrap_or_default()));
                n.details.push(Detail::Removed(
                    "Rows Removed by Filter",
                    vec![Removed::Diff(input_key)],
                ));
                n.key = Some(key);
                n.rows = costs::clamp_rows(c.node.rows * sel);
                n.width = c.node.width;
                n.startup_cost = c.node.startup_cost;
                n.total_cost = c.node.total_cost + c.node.rows * (costs::CPU_TUPLE_COST + qc);
                n.subplans.append(&mut self.pending);
                let child = std::mem::take(&mut c.node);
                n.children.push(child.child("Subquery"));
                c.node = n;
                }
                // (Above it, its columns are the subquery's.)
                if setop && sub.is_some() {
                    c.internal = names.clone();
                    c.exported = names;
                }
            }
        } else {
            c.node.key = Some(key);
        }
        if !gating.is_empty() {
            if c.node.node_type == "Result" && c.node.children.is_empty() && quals.is_empty() {
                let texts = gating
                    .iter()
                    .map(|e| self.show(e, &c.internal, Cx::Upper))
                    .collect();
                c.node.add_qual(
                    "One-Time Filter",
                    "",
                    and_text(texts).unwrap_or_default(),
                    None,
                );
                c.node.subplans.append(&mut self.pending);
            } else if quals.is_empty()
                && c.node.node_type == "Subquery Scan"
                && c.node.children.len() == 1
                && !matches!(
                    c.node.children[0].node_type,
                    "Append" | "Merge Append" | "SetOp" | "HashSetOp" | "Unique" | "Recursive Union"
                )
                && !c.node.details.iter().any(|d| matches!(d, Detail::Text("Filter", _)))
            {
                // (A subquery's Subquery Scan kept (a row mark): the pseudoconstant quals go
                // into it, set_subquery_pathlist, as a Result over its plan; their InitPlans
                // are this level's, where the sub-selects became Params.)
                let inner = c.node.children.pop().expect("checked");
                let g = self.gate(inner, &gating, &[]);
                c.node.children.push(g.child("Subquery"));
            } else {
                let child = std::mem::take(&mut c.node);
                let names = c.internal.clone();
                c.node = self.gate(child, &gating, &names);
                c.node.key = Some(key);
            }
        }
        c
    }

    fn agg(
        &mut self,
        input: &Arc<Plan>,
        groups: &[Expr],
        sets: Option<&[Vec<usize>]>,
        aggs: &[AggCall],
    ) -> Built {
        let having = self.having.take();
        let my_key = self.agg_key.take();
        // (GROUPING() makes the level an aggregating one: an Agg node, no Group.)
        let no_aggs = aggs.is_empty()
            && my_key.and_then(|k| self.gtl.grouping_keys.get(&k)).is_none_or(|g| g.is_empty());
        // (An ORDER BY above: the GROUP BY's keys in its order first, preprocess_groupclause.)
        let hint = self.group_order.take();
        let hint_full = self.group_order_full;
        let named = std::mem::take(&mut self.group_named);
        let saved_input = self.agg_input.replace(addr(input));
        // (The group keys' sub-selects: RTE_GROUP expressions, numbered after the level's
        // others; a key that is one sub-select, what a GROUPING() argument copies.)
        let key_exprs: Vec<Expr> = {
            let mut src: &Plan = input;
            while let Plan::Sort { input, .. } = src {
                src = input;
            }
            groups
                .iter()
                .map(|g| match (implicit_key_col(g), src) {
                    (Some(j), Plan::Project { exprs, .. }) => exprs.get(j).unwrap_or(g).clone(),
                    (Some(j), Plan::Values { rows, .. }) if rows.len() == 1 => rows[0].get(j).unwrap_or(g).clone(),
                    _ => g.clone(),
                })
                .collect()
        };
        let mut key_ids = std::collections::HashSet::new();
        for e in &key_exprs {
            let mut sq = Vec::new();
            subqueries_in(e, &mut sq);
            key_ids.extend(sq.iter().map(|s| s.id));
        }
        let saved_keys = std::mem::replace(&mut self.key_sub_ids, key_ids);
        let mut c = self.node(input);
        self.agg_input = saved_input;
        // (A projection computing a key: its target list is the grouping's input target,
        // the keys first in the level's target list's order, make_group_input_target.)
        if self.o.verbose
            && let Plan::Project { exprs: pexprs, .. } = &**input
            && (c.node.is_scan() || c.node.node_type == "Result")
            && groups
                .iter()
                .any(|g| implicit_key_col(g).and_then(|j| pexprs.get(j)).is_some_and(|e| !matches!(e, Expr::Col(..) | Expr::Const(..))))
        {
            let order: Vec<usize> = my_key
                .and_then(|k| self.gtl.order.get(&k))
                .filter(|o| o.len() == groups.len())
                .cloned()
                .unwrap_or_else(|| (0..groups.len()).collect());
            let mut front: Vec<String> = Vec::new();
            for k in order {
                if let Some(j) = implicit_key_col(&groups[k])
                    && let Some(n) = c.internal.get(j)
                {
                    let t = self.pick(n, Cx::Output);
                    if c.node.output.contains(&t) && !front.contains(&t) {
                        front.push(t);
                    }
                }
            }
            let rest: Vec<String> = c.node.output.iter().filter(|o| !front.contains(o)).cloned().collect();
            c.node.output = front.into_iter().chain(rest).collect();
        }
        let mut part_in = c.node.partial.take();
        let mut part_others = std::mem::take(&mut c.node.partials);
        // A scan feeding the aggregation returns only the columns it uses.
        if c.node.is_scan() && c.node.cols.len() == c.exported.len() {
            let mut used = std::collections::BTreeSet::new();
            let mut add = |e: &Expr| used.extend(e.columns());
            groups.iter().for_each(&mut add);
            for a in aggs {
                a.args.iter().chain(&a.direct).chain(&a.filter).for_each(&mut add);
                a.order_by.iter().for_each(|k| add(&k.expr));
            }
            c.node.width = used
                .iter()
                .filter_map(|i| c.node.cols.get(*i))
                .map(|c| c.width)
                .sum();
            if let Some(pi) = &mut part_in {
                pi.width = c.node.width;
            }
            for po in &mut part_others {
                po.width = c.node.width;
            }
        }
        // The group expressions, computed below the aggregation for each row (the input's
        // target list): in the workers too.
        let gexpr: f64 = groups
            .iter()
            .filter(|g| !matches!(g, Expr::Col(..)))
            .map(costs::qual_cost)
            .sum();
        if gexpr > 0.0 {
            c.node.total_cost += c.node.rows * gexpr;
            for pi in part_in.iter_mut().map(|b| &mut **b).chain(part_others.iter_mut()) {
                pi.total_cost += pi.rows * gexpr;
            }
        }
        // (A complete grouping may read the relation's partial plans gathered in order.)
        let rel_rows = matches!(&**input, Plan::Scan(_) | Plan::Join { .. }).then_some(c.node.rows);
        let gather_in: Option<(ExNode, Vec<ExNode>, Option<f64>)> =
            part_in.as_ref().map(|pi| ((**pi).clone(), part_others, rel_rows));
        // The input run serially, or gathered from workers.
        if let Some(pi) = &part_in {
            c.node.partial = Some(pi.clone());
            self.settle(&mut c.node);
        }
        // (Over a Result computing a row of values, a pulled-up VALUES list's: a key
        // computed there is over its values, as they are.)
        let values_row = c.node.node_type == "Result" && c.node.children.is_empty() && c.internal.len() == c.exported.len();
        // (A written COLLATE over a column of another collation is a RelabelType to
        // Postgres (eval_const_expressions), which a key shows as a cast:
        // `(c)::text`.)
        let relabel_key = |b: &mut Self, g: &Expr| -> Option<Name> {
            let Expr::Collate(x, coll, loc) = g else { return None };
            let Expr::Col(j, _) = &**x else { return None };
            if *loc < 0 {
                return None;
            }
            let own = c
                .node
                .cols
                .get(*j)
                .and_then(|ci| ci.var.as_ref())
                .filter(|v| v.expr.is_none())
                .and_then(|v| v.table.columns.get(v.col))
                .map(|col| col.collation.clone().unwrap_or_else(|| "default".into()))?;
            if own == *coll {
                return None;
            }
            let n = b.name(x, &c.exported, &[]);
            let ty = x.ty().typname();
            Some(Name {
                plain: format!("({})::{ty}", n.plain),
                full: format!("({})::{ty}", n.full),
            })
        };
        let mut relabeled: Vec<bool> = Vec::new();
        let gnames: Vec<Name> = groups
            .iter()
            .map(|g| {
                let r = relabel_key(self, g);
                relabeled.push(r.is_some());
                if let Some(n) = r {
                    n
                } else if values_row && !matches!(g, Expr::Col(..)) && !g.columns().is_empty() {
                    self.name(g, &c.internal, &[])
                } else {
                    self.name(g, &c.exported, &[])
                }
            })
            .collect();
        let ng = groups.len();
        // (The sub-selects of the keys: the input computing the keys runs them,
        // make_group_input_target.)
        if !self.pending.is_empty() && !self.key_sub_ids.is_empty() {
            let target = if c.node.is_scan() || c.node.node_type == "Result" {
                Some(&mut c.node)
            } else {
                None
            };
            if let Some(t) = target {
                t.subplans.append(&mut self.pending);
            }
        }
        // make_group_input_target: a GROUP BY expression is computed below the
        // aggregation (its input returns it, before the columns the aggregates read); the
        // nodes above it, a Sort between, refer to it.
        // (A column under COLLATE is the column: the planner strips the CollateExpr.)
        let plain_col = |g: &Expr| {
            let mut g = g;
            while let Expr::Collate(x, ..) | Expr::Relabel(x, _) = g {
                g = x;
            }
            matches!(g, Expr::Col(..) | Expr::Const(..))
        };
        // (With grouping sets a constant key is computed below too: the sets null it.)
        let computed: Vec<bool> = groups
            .iter()
            .zip(&relabeled)
            .map(|(g, &r)| {
                (!plain_col(g) && !g.columns().is_empty())
                    || (sets.is_some() && matches!(g, Expr::Const(..)))
                    || r
            })
            .collect();
        let gref: Vec<Name> = gnames
            .iter()
            .zip(&computed)
            .map(|(g, &k)| if k { g.wrapped() } else { g.clone() })
            .collect();
        for ((e, g), r) in key_exprs.iter().zip(&gnames).zip(&gref) {
            if let Expr::Subquery(s) = e
                && s.call.is_none()
            {
                for n in [g, r] {
                    self.key_subs.insert(n.full.clone(), (**s).clone());
                    self.key_subs.insert(n.plain.clone(), (**s).clone());
                }
            }
        }
        // (A Sort over the input returns references to what the input computes.)
        let mut computed_shown: Vec<(String, String)> = gnames
            .iter()
            .zip(&gref)
            .zip(&computed)
            .filter(|(_, k)| **k)
            .map(|((g, r), _)| (self.pick(g, Cx::Output), self.pick(r, Cx::Output)))
            .collect();
        // (So are the keys a projection below computes: the input's values.)
        if let Plan::Project { exprs: pexprs, .. } = &**input {
            for g in groups {
                if let Some(j) = implicit_key_col(g)
                    && pexprs.get(j).is_some_and(|e| !matches!(e, Expr::Col(..) | Expr::Const(..)))
                    && let (Some(i), Some(e)) = (c.internal.get(j), c.exported.get(j))
                    && i.full != e.full
                {
                    let pair = (self.pick(i, Cx::Output), self.pick(e, Cx::Output));
                    if !computed_shown.contains(&pair) {
                        computed_shown.push(pair);
                    }
                }
            }
        }
        let sort_out = |o: &String| -> String {
            match computed_shown.iter().find(|(g, _)| g == o) {
                Some((_, r)) => r.clone(),
                None => refer_to(o),
            }
        };
        // (A scan computing them returns that target list, no physical one: the group
        // keys, then the columns the aggregates read.)
        if self.o.verbose && computed.iter().any(|k| *k) && (c.node.is_scan() || values_row) {
            let mut out: Vec<String> = Vec::new();
            // (A relabelled column shows bare at the target list's top: an entry of its own.)
            let mut seen: Vec<String> = Vec::new();
            // (In the order of the level's target list: make_group_input_target. A
            // grouping-set query's pass-through values, keys in no set, after the keys, each
            // an entry of its own.)
            let pass: Vec<bool> = (0..groups.len())
                .map(|i| sets.is_some_and(|ss| !ss.iter().any(|s| s.contains(&i))))
                .collect();
            let nkeys = pass.iter().filter(|p| !**p).count();
            let mut order: Vec<usize> = my_key
                .and_then(|k| self.gtl.order.get(&k))
                .filter(|o| o.len() == nkeys || o.len() == groups.len())
                .map(|o| o.iter().copied().filter(|&i| !pass.get(i).copied().unwrap_or(true)).collect())
                .unwrap_or_else(|| (0..groups.len()).filter(|&i| !pass[i]).collect());
            order.extend((0..groups.len()).filter(|&i| pass[i]));
            for (g, n, &r, &own) in order.iter().map(|&i| (&groups[i], &gnames[i], &relabeled[i], &pass[i])) {
                if r && let Expr::Collate(x, ..) = g && let Expr::Col(j, _) = &**x {
                    if let Some(n) = c.exported.get(*j) {
                        out.push(self.pick(n, Cx::Output));
                    }
                    continue;
                }
                // (A column the scan computes, a virtual generated one's: its expression.)
                let n = match g {
                    Expr::Col(j, _) => c.internal.get(*j).unwrap_or(n),
                    _ => n,
                };
                let t = self.pick(n, Cx::Output);
                if own || !seen.contains(&t) {
                    seen.push(t.clone());
                    out.push(t);
                }
            }
            let mut cols: Vec<usize> = Vec::new();
            // (What the GROUPING() arguments read: copies of the keys they test.)
            if let Some(gk) = my_key.and_then(|k| self.gtl.grouping_keys.get(&k)) {
                for &k in gk {
                    if let Some(g) = groups.get(k)
                        && !matches!(g, Expr::Col(..))
                    {
                        cols.extend(g.columns());
                    }
                }
            }
            for a in aggs {
                for e in a.args.iter().chain(&a.direct).chain(&a.filter).chain(a.order_by.iter().map(|k| &k.expr)) {
                    cols.extend(e.columns());
                }
            }
            for j in cols {
                if let Some(n) = if values_row { c.internal.get(j) } else { c.exported.get(j) } {
                    let t = self.pick(n, Cx::Output);
                    if !seen.contains(&t) {
                        seen.push(t.clone());
                        out.push(t);
                    }
                }
            }
            c.node.output = out;
        }
        let casts = std::mem::take(&mut self.json_casts);
        // (The sub-selects the aggregates' arguments read: InitPlans of the level.)
        for a in aggs {
            for e in a.args.iter().chain(&a.direct).chain(&a.filter) {
                self.register(e, &c.exported);
            }
        }
        // JSON_ARRAYAGG / JSON_OBJECTAGG under another node computing the select list
        // (a WindowAgg, a Result over a Sort): the aggregation computes the aggregate
        // itself (json_agg_strict, …), that node the constructor over it.
        let plain_aggs: Vec<AggCall>;
        // (The level's top node itself, no projection above: it computes them.)
        let aggs: &[AggCall] = if aggs.iter().any(|a| a.json_ctor) && self.proj_depth > 0 {
            if let Some(k) = my_key {
                self.json_aggs.insert(k, (aggs.to_vec(), c.exported.clone(), ng));
            }
            plain_aggs = aggs
                .iter()
                .map(|a| {
                    let mut a = a.clone();
                    a.json_ctor = false;
                    a
                })
                .collect();
            &plain_aggs
        } else {
            aggs
        };
        let anames: Vec<Name> = (0..aggs.len())
            .map(|i| {
                let a = Expr::Agg(i, aggs[i].ty.clone());
                match casts.iter().find(|c| c.0 == ng + i) {
                    Some((_, to)) => self.name(
                        &Expr::Cast(Box::new(a), aggs[i].ty.clone(), to.clone(), true),
                        &c.exported,
                        aggs,
                    ),
                    None => self.name(&a, &c.exported, aggs),
                }
            })
            .collect();
        let mut internal = gref.clone();
        internal.extend(anames.iter().cloned());
        let mut exported: Vec<Name> = groups
            .iter()
            .zip(&gnames)
            .map(|(g, n)| match implicit_key_col(g) {
                // (A key carrying its own nondeterministic collation is the column.)
                Some(j) => c.exported.get(j).cloned().unwrap_or_else(|| n.clone()),
                None => n.wrapped(),
            })
            .collect();
        exported.extend(anames.iter().map(Name::wrapped));
        if sets.is_some() {
            internal.push(Name::same("GROUPING"));
            exported.push(Name::same("GROUPING"));
        }
        let mut n = ExNode {
            partial_mode: Some("Simple"),
            ..ExNode::new("Aggregate")
        };
        // Keys functionally dependent on other keys group nothing more
        // (remove_useless_groupby_columns: a table's columns beyond those of its unique,
        // not-null (or nulls-not-distinct), immediate, non-partial index of the fewest
        // columns all grouped; with grouping sets, the columns grouped along for being
        // selected). Then a key equal to an earlier one (processed_groupClause).
        let redundant = self.redundant_keys(input, groups, sets, &c.node.cols);
        // (A key its input computes shows as that input's expression: show_agg_keys
        // deparses the input's target list entry.)
        let graw: Vec<Name> = groups
            .iter()
            .zip(&gnames)
            .map(|(g, n)| match g {
                Expr::Col(j, _) => match (c.internal.get(*j), c.exported.get(*j)) {
                    (Some(i), Some(e)) if c.node.node_type == "ProjectSet" && e.full == format!("({})", i.full) => i.clone(),
                    _ => n.clone(),
                },
                _ => n.clone(),
            })
            .collect();
        let keys = |b: &Self, idx: &[usize]| {
            idx.iter()
                .filter(|i| !redundant.get(**i).copied().unwrap_or(false))
                .filter_map(|i| graw.get(*i))
                .map(|g| b.pick(g, Cx::Upper))
                .collect::<Vec<_>>()
        };
        // (A hashed grouping set's keys read the input directly: as it computes them.)
        let ghash: Vec<Name> = groups
            .iter()
            .zip(&graw)
            .map(|(g, n)| match g {
                Expr::Col(j, _) => match (c.internal.get(*j), c.exported.get(*j)) {
                    (Some(i), Some(e)) if e.full == format!("({})", i.full) && n.full == e.full => i.clone(),
                    _ => n.clone(),
                },
                _ => n.clone(),
            })
            .collect();
        let hash_keys = |b: &Self, idx: &[usize]| {
            idx.iter()
                .filter(|i| !redundant.get(**i).copied().unwrap_or(false))
                .filter_map(|i| ghash.get(*i))
                .map(|g| b.pick(g, Cx::Upper))
                .collect::<Vec<_>>()
        };
        // (Over a sort, the keys its input computes are references.)
        let keys_ref = |b: &Self, idx: &[usize]| {
            idx.iter()
                .filter(|i| !redundant.get(**i).copied().unwrap_or(false))
                .filter_map(|i| gref.get(*i))
                .map(|g| b.pick(g, Cx::Upper))
                .collect::<Vec<_>>()
        };
        // Presorted aggregates (adjust_group_pathkeys_for_groupagg): the input sorted, after
        // the group keys, for the DISTINCT / ORDER BY order that suits the most aggregates;
        // the grouping is by sorting then.
        let ordered = aggs.iter().any(|a| a.distinct || !a.order_by.is_empty());
        // (The group pathkeys: the keys the ORDER BY starts with first, each key in the
        // direction the ORDER BY gives it.)
        let presort_order: Vec<(usize, String)> = {
            let mut o: Vec<(usize, String)> = Vec::new();
            for (i, sfx) in hint.iter().flatten() {
                if *i < groups.len() && !o.iter().any(|x| x.0 == *i) {
                    o.push((*i, sfx.clone()));
                }
            }
            for i in 0..groups.len() {
                if !o.iter().any(|x| x.0 == i) {
                    o.push((i, named.iter().find(|n| n.0 == i).map_or_else(String::new, |n| n.1.clone())));
                }
            }
            o
        };
        let base_idx: Vec<usize> = presort_order.iter().map(|o| o.0).collect();
        let mut presort_idx: Vec<usize> = base_idx.clone();
        // (Keys by position, in the group pathkeys' directions.)
        let sfx_of = |i: usize| presort_order.iter().find(|o| o.0 == i).map_or_else(String::new, |o| o.1.clone());
        let keys_sfx = |b: &Self, idx: &[usize]| -> Vec<String> {
            idx.iter()
                .filter(|i| !redundant.get(**i).copied().unwrap_or(false))
                .filter_map(|&i| gref.get(i).map(|g| b.pick(g, Cx::Upper) + &sfx_of(i)))
                .collect()
        };
        let presort: Option<Vec<String>> = if !ordered || sets.is_some() {
            None
        } else {
            let enabled = self
                .ctx
                .sess
                .settings
                .borrow()
                .get_raw("enable_presorted_aggregate")
                .is_none_or(|v| v != "off" && v != "false");
            let group_keys: Vec<String> = keys_sfx(self, &base_idx);
            let mut best: Vec<String> = Vec::new();
            if enabled {
                // (Each distinct aggregate's pathkeys; none for ones that can't presort.)
                let mut seen: Vec<String> = Vec::new();
                let mut unprocessed: Vec<Vec<String>> = Vec::new();
                for a in aggs {
                    if !(a.distinct || !a.order_by.is_empty())
                        || !matches!(a.def.kind, crate::exec::agg::AggKind::Normal)
                    {
                        continue;
                    }
                    let id = format!("{a:?}");
                    if seen.contains(&id) {
                        continue;
                    }
                    seen.push(id);
                    // (With a FILTER, only plain columns and constants as arguments.)
                    if a.filter.is_some()
                        && !a.args.iter().all(|e| {
                            let e = match e {
                                Expr::Relabel(x, _) | Expr::Cast(x, _, _, false) => &**x,
                                e => e,
                            };
                            matches!(e, Expr::Col(..) | Expr::Const(..))
                        })
                    {
                        continue;
                    }
                    let sort: Vec<(&Expr, String)> = if a.distinct {
                        a.args.iter().map(|e| (e, String::new())).collect()
                    } else {
                        a.order_by.iter().map(|k| (&k.expr, sort_suffix(k))).collect()
                    };
                    if sort.iter().any(|(e, _)| e.is_volatile()) {
                        continue;
                    }
                    let mut pk: Vec<String> = Vec::new();
                    for (e, sfx) in sort {
                        if e.columns().is_empty() && !e.has_outer_refs() {
                            continue;
                        }
                        let k = self.show(e, &c.exported, Cx::Upper) + &sfx;
                        if !pk.contains(&k) {
                            pk.push(k);
                        }
                    }
                    unprocessed.push(pk);
                }
                // (With the group keys first.)
                let with_groups = |pk: &Vec<String>| -> Vec<String> {
                    let mut out = group_keys.clone();
                    for k in pk {
                        if !out.contains(k) {
                            out.push(k.clone());
                        }
                    }
                    out
                };
                let mut best_n = 0;
                while unprocessed.len() > best_n {
                    let mut cur: Option<Vec<String>> = None;
                    let mut taken: Vec<usize> = Vec::new();
                    for (i, pk) in unprocessed.iter().enumerate() {
                        let pk = with_groups(pk);
                        match &cur {
                            None => {
                                cur = Some(pk);
                                taken.push(i);
                            }
                            Some(c) => {
                                if c.len() <= pk.len() && pk[..c.len()] == c[..] {
                                    cur = Some(pk);
                                    taken.push(i);
                                } else if pk.len() < c.len() && c[..pk.len()] == pk[..] {
                                    taken.push(i);
                                }
                            }
                        }
                    }
                    let n_taken = taken.len();
                    for i in taken.into_iter().rev() {
                        unprocessed.remove(i);
                    }
                    if n_taken > best_n {
                        best_n = n_taken;
                        best = cur.unwrap_or_default();
                    }
                }
            }
            let best = if best.is_empty() { group_keys.clone() } else { best };
            // get_useful_group_keys_orderings: the group keys also in the input's order as
            // far as it goes (the rest in their order), the aggregates' after them; the
            // cheaper to sort.
            let g = crate::sql::paths::Gucs::read(self.ctx);
            let names: Vec<String> = (0..groups.len()).map(|i| keys_sfx(self, &[i]).pop().unwrap_or_default()).collect();
            let mut pre: Vec<usize> = Vec::new();
            for (oi, o) in c.node.order.iter().enumerate() {
                let eqs = c.node.order_eq.get(oi).cloned().unwrap_or_default();
                let found = base_idx
                    .iter()
                    .copied()
                    .find(|&i| !names[i].is_empty() && (names[i] == *o || eqs.contains(&names[i])));
                match found {
                    Some(i) if !pre.contains(&i) => pre.push(i),
                    _ => break,
                }
            }
            let suffix: Vec<String> = best[group_keys.len().min(best.len())..].to_vec();
            let keys_for = |idx: &[usize]| -> Vec<String> {
                let mut out = keys_sfx(self, idx);
                for k in &suffix {
                    if !out.contains(k) {
                        out.push(k.clone());
                    }
                }
                out
            };
            let presorted = |keys: &[String]| -> usize {
                keys.iter()
                    .zip(&c.node.order)
                    .enumerate()
                    .take_while(|(oi, (a, b))| a == b || c.node.order_eq.get(*oi).is_some_and(|e| e.contains(a)))
                    .count()
            };
            let sort_cost = |keys: &[String]| -> f64 {
                let common = presorted(keys);
                if common == keys.len() {
                    0.0
                } else if common > 0 && g.incremental_sort {
                    costs::incremental_sort_cost(
                        c.node.startup_cost,
                        c.node.total_cost,
                        c.node.rows,
                        c.node.width,
                        c.node.rows.clamp(1.0, costs::DEFAULT_NUM_DISTINCT),
                        None,
                    )
                    .1
                } else {
                    costs::sort_cost(c.node.total_cost, c.node.rows, c.node.width, None).1
                }
            };
            let mut chosen = best.clone();
            if !pre.is_empty()
                && pre[..] != base_idx[..pre.len()]
                && (g.incremental_sort || pre.len() == groups.len())
            {
                let mut idx = pre.clone();
                idx.extend(base_idx.iter().copied().filter(|i| !pre.contains(i)));
                let alt = keys_for(&idx);
                if sort_cost(&alt) < sort_cost(&best) {
                    presort_idx = idx;
                    chosen = alt;
                }
            }
            Some(chosen)
        };
        if let Some(sort_keys) = presort {
            // (The input's order, as far as it goes: sorted incrementally after it.)
            let common = sort_keys
                .iter()
                .zip(&c.node.order)
                .enumerate()
                .take_while(|(oi, (a, b))| *a == *b || c.node.order_eq.get(*oi).is_some_and(|e| e.contains(a)))
                .count();
            let incremental = common > 0 && crate::sql::paths::Gucs::read(self.ctx).incremental_sort;
            if !sort_keys.is_empty() && common < sort_keys.len() {
                let mut sn = ExNode::new(if incremental { "Incremental Sort" } else { "Sort" });
                sn.details.push(Detail::List("Sort Key", sort_keys.clone()));
                if incremental {
                    sn.details.push(Detail::List(
                        "Presorted Key",
                        sort_keys[..common].iter().map(|k| presorted_name(k)).collect(),
                    ));
                    let pre_cols: Vec<ColInfo> = presort_idx[..common.min(presort_idx.len())]
                        .iter()
                        .map(|i| match &groups[*i] {
                            Expr::Col(j, _) => c.node.cols.get(*j).cloned().unwrap_or_default(),
                            _ => ColInfo::default(),
                        })
                        .collect();
                    let ng = if pre_cols.len() < common || pre_cols.iter().any(|x| x.var.is_none()) {
                        c.node.rows.clamp(1.0, costs::DEFAULT_NUM_DISTINCT)
                    } else {
                        self.num_groups(&pre_cols, c.node.rows)
                    };
                    (sn.startup_cost, sn.total_cost) = costs::incremental_sort_cost(
                        c.node.startup_cost,
                        c.node.total_cost,
                        c.node.rows,
                        c.node.width,
                        ng,
                        None,
                    );
                    sn.rows = c.node.rows;
                    sn.width = c.node.width;
                    sn.cols = c.node.cols.clone();
                } else {
                    sn.details.push(Detail::SortInfo);
                    self.sort_costs(&mut sn, &c.node);
                }
                sn.key = c.node.key;
                sn.order = sort_keys.clone();
                if self.o.verbose {
                    sn.output = c.node.output.iter().map(&sort_out).collect();
                }
                let child = std::mem::take(&mut c.node);
                sn.children.push(child.child("Outer"));
                c.node = sn;
            }
            if !groups.is_empty() {
                n.strategy = Some("Sorted");
                n.title = "GroupAggregate".into();
                let keys = presort_idx
                    .iter()
                    .filter(|i| !redundant.get(**i).copied().unwrap_or(false))
                    .map(|&i| {
                        let (d, nf) = sort_dir(&sfx_of(i));
                        (i, d, nf)
                    })
                    .collect();
                put_group_sort(&mut n.details, keys);
                n.details.push(Detail::List("Group Key", keys_ref(self, &presort_idx)));
                // (Its groups come out in the keys' order.)
                n.order = keys_sfx(self, &presort_idx);
                n.rows = (c.node.rows * 0.2).clamp(1.0, 200.0);
            }
        }
        // Grouping sets with ordered aggregates can't hash: a GroupAggregate whose
        // rollups after the first sort the input again.
        // (So with enable_hashagg off.)
        let no_hash = !crate::sql::paths::Gucs::read(self.ctx).hashagg;
        // (Or when sorting costs less: consider_groupingsets_paths.)
        // (Keys of a type without a btree operator class are only hashed, and ones
        // without a hash operator class only sorted: their sets must be.)
        let (unsortable, unhashable): (Vec<usize>, Vec<usize>) = {
            let cat = self.ctx.catalog();
            let ty = |i: usize| groups[i].ty();
            (
                (0..groups.len()).filter(|&i| cat.default_opclass(403, &ty(i)) == 0).collect(),
                (0..groups.len()).filter(|&i| !cat.type_hashable(&ty(i))).collect(),
            )
        };
        let must_sort = sets.is_some_and(|ss| ss.iter().any(|s| s.iter().any(|k| unhashable.contains(k))));
        let must_hash: Vec<usize> = match sets {
            Some(ss) => (0..ss.len()).filter(|&i| ss[i].iter().any(|k| unsortable.contains(k))).collect(),
            None => Vec::new(),
        };
        // (Input sorted for the first rollup already, its keys all fixed by equalities with
        // constants (the group pathkeys redundant): consider_groupingsets_paths is offered
        // sorted input, and tries only the sorted and the mixed plans.)
        let first_fixed = sets.is_some_and(|sets| {
            let fixed = super::paths::constant_columns(input);
            crate::exec::agg::rollup_chains(sets).first().is_some_and(|(_, cols)| {
                !cols.is_empty()
                    && cols.iter().all(|i| matches!(groups.get(*i), Some(Expr::Col(j, _)) if fixed.get(*j).copied().unwrap_or(false)))
            })
        });
        let sorted_cheaper = match sets {
            Some(_) if must_sort || !must_hash.is_empty() => true,
            Some(_) if !ordered && !no_hash && first_fixed => true,
            Some(sets) if !ordered && !no_hash => self.sets_sorted_cheaper(&c.node, groups, sets, aggs),
            _ => false,
        };
        if let (Some(sets), true) = (sets, ordered || no_hash || sorted_cheaper) {
            // (The sets that sort make the rollups.)
            let sortable: Vec<usize> = (0..sets.len()).filter(|i| !must_hash.contains(i)).collect();
            let sub: Vec<Vec<usize>> = sortable.iter().map(|&i| sets[i].clone()).collect();
            let all_chains: Vec<(Vec<usize>, Vec<usize>)> = if must_hash.is_empty() {
                crate::exec::agg::rollup_chains(sets)
            } else {
                crate::exec::agg::rollup_chains(&sub)
                    .into_iter()
                    .map(|(chain, cols)| (chain.into_iter().map(|i| sortable[i]).collect(), cols))
                    .collect()
            };
            // (One rollup: its columns follow the ORDER BY as far as they can,
            // reorder_grouping_sets.)
            let mut all_chains = all_chains;
            if let [(chain, cols)] = &mut all_chains[..]
                && let Some(h) = hint.as_ref().filter(|h| !h.is_empty())
            {
                let mut sort: Vec<usize> = h.iter().map(|(i, _)| *i).collect();
                let mut prev: Vec<usize> = Vec::new();
                for &si in chain.iter().rev() {
                    let mut new: Vec<usize> = Vec::new();
                    for &k in &sets[si] {
                        if !prev.contains(&k) && !new.contains(&k) {
                            new.push(k);
                        }
                    }
                    while sort.len() > prev.len() && !new.is_empty() {
                        let r = sort[prev.len()];
                        if let Some(p) = new.iter().position(|&k| k == r) {
                            prev.push(r);
                            new.remove(p);
                        } else {
                            sort.clear();
                            break;
                        }
                    }
                    prev.extend(new);
                }
                *cols = prev;
            }
            // (GROUP BY (): empty sets only, a plain aggregation.)
            let plain = sets.iter().all(|s| s.is_empty());
            n.strategy = Some(if plain { "Plain" } else { "Sorted" });
            n.title = if plain { "Aggregate" } else { "GroupAggregate" }.into();
            // Later rollups whose hash tables fit in hash_mem hashed instead of sorted for
            // (the knapsack of consider_groupingsets_paths): a MixedAggregate, their sets'
            // hash keys first (last rollup's first).
            let hashed = if ordered || no_hash {
                Vec::new()
            } else {
                self.knapsack_rollups(&c.node, groups, sets, aggs, &all_chains, &unhashable)
            };
            let mut chains = Vec::new();
            let mut hash_sets: Vec<(usize, Vec<usize>)> =
                must_hash.iter().map(|&i| (i, sets[i].clone())).collect();
            for (ci, (chain, cols)) in all_chains.into_iter().enumerate() {
                if hashed.contains(&ci) {
                    for &si in &chain {
                        hash_sets.push((si, cols.iter().copied().filter(|c| sets[si].contains(c)).collect()));
                    }
                } else {
                    chains.push((chain, cols));
                }
            }
            if !hash_sets.is_empty() {
                n.strategy = Some("Mixed");
                n.title = "MixedAggregate".into();
                for (_, cols) in hash_sets.iter().rev() {
                    n.details.push(Detail::List("Hash Key", hash_keys(self, cols)));
                }
            }
            // (Keys the input fixes to constants need no sort.)
            let fixed = super::paths::constant_columns(input);
            let is_fixed = |i: &usize| matches!(groups.get(*i), Some(Expr::Col(j, _)) if fixed.get(*j).copied().unwrap_or(false));
            // (Nor does input already in that order: an index scan's.)
            let presorted = chains.first().is_some_and(|(_, cols)| {
                let mut sk: Vec<String> = Vec::new();
                for k in keys(self, cols) {
                    if !sk.contains(&k) {
                        sk.push(k);
                    }
                }
                !sk.is_empty() && c.node.order.len() >= sk.len() && c.node.order[..sk.len()] == sk[..]
            });
            let sort_below = chains
                .first()
                .is_some_and(|(_, cols)| !cols.is_empty() && !cols.iter().all(is_fixed) && !presorted);
            // (Over a Sort, the keys the input computes are references to its values.)
            let skeys = |b: &Self, idx: &[usize]| if sort_below { keys_ref(b, idx) } else { keys(b, idx) };
            for (ci, (chain, cols)) in chains.iter().enumerate() {
                if ci > 0 {
                    n.details.push(Detail::List("Sort Key", skeys(self, cols)));
                }
                for &si in chain {
                    // (A set's keys in its rollup's order.)
                    let k = if sets[si].is_empty() {
                        vec!["()".to_string()]
                    } else {
                        let ordered: Vec<usize> = cols.iter().copied().filter(|c| sets[si].contains(c)).collect();
                        skeys(self, &ordered)
                    };
                    n.details
                        .push(Detail::List(if ci > 0 { "  Group Key" } else { "Group Key" }, k));
                }
            }
            // (One sorted rollup puts its groups out in its order, as far as keys of
            // every set go: the others go null.)
            if hash_sets.is_empty()
                && let [(chain, cols)] = &chains[..]
                && !plain
            {
                let lead: Vec<usize> =
                    cols.iter().copied().take_while(|k| chain.iter().all(|&si| sets[si].contains(k))).collect();
                n.order = skeys(self, &lead);
            }
            if sort_below && let Some((_, cols)) = chains.first() {
                let mut sn = ExNode::new("Sort");
                // (A key listed twice is one pathkey.)
                let mut sk: Vec<String> = Vec::new();
                for k in keys_ref(self, cols) {
                    if !sk.contains(&k) {
                        sk.push(k);
                    }
                }
                sn.details.push(Detail::List("Sort Key", sk));
                sn.details.push(Detail::SortInfo);
                self.sort_costs(&mut sn, &c.node);
                sn.key = c.node.key;
                if self.o.verbose {
                    sn.output = c.node.output.iter().map(sort_out).collect();
                }
                let child = std::mem::take(&mut c.node);
                sn.children.push(child.child("Outer"));
                c.node = sn;
            }
            n.rows = (c.node.rows * 0.2).clamp(1.0, 200.0) * sets.len() as f64;
        }
        match sets {
            _ if n.strategy.is_some() => {}
            None if groups.is_empty() => {
                n.strategy = Some("Plain");
                n.rows = 1.0;
            }
            // Every key fixed by an equality with a constant (its group pathkeys all
            // redundant): a GroupAggregate of one group, no keys to compare.
            None if fixed_group_keys(input, groups).iter().all(|f| *f) =>
            {
                n.strategy = Some("Sorted");
                n.title = if no_aggs { "Group" } else { "GroupAggregate" }.into();
                if no_aggs {
                    n.node_type = "Group";
                    n.strategy = None;
                    n.partial_mode = None;
                }
                n.rows = 1.0;
            }
            None => {
                n.strategy = Some("Hashed");
                n.title = "HashAggregate".into();
                let mut all: Vec<usize> = Vec::new();
                for (i, _) in hint.iter().flatten() {
                    if *i < groups.len() && !all.contains(i) {
                        all.push(*i);
                    }
                }
                all.extend((0..groups.len()).filter(|i| !all.contains(i)).collect::<Vec<_>>());
                all.retain(|i| !redundant.get(*i).copied().unwrap_or(false));
                // (A hashed grouping shows a key its input computes as the expression.)
                let computed = |i: usize| match (groups.get(i), &**input) {
                    (Some(Expr::Col(j, _)), Plan::Project { exprs, .. }) => {
                        exprs.get(*j).is_some_and(|e| !matches!(e, Expr::Col(..) | Expr::Const(..)))
                    }
                    _ => false,
                };
                let shown: Vec<String> = all
                    .iter()
                    .zip(keys(self, &all))
                    .map(|(&i, k)| match k.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
                        Some(inner) if computed(i) => inner.to_string(),
                        _ => k,
                    })
                    .collect();
                n.details.push(Detail::List("Group Key", shown));
                n.details.push(Detail::HashAggInfo);
                n.rows = (c.node.rows * 0.2).clamp(1.0, 200.0);
            }
            Some(sets) => {
                let mixed = sets.iter().any(|s| s.is_empty());
                n.strategy = Some(if mixed { "Mixed" } else { "Hashed" });
                n.title = if mixed {
                    "MixedAggregate"
                } else {
                    "HashAggregate"
                }
                .into();
                for (_, cols) in crate::exec::agg::hashed_sets_order(sets) {
                    n.details.push(Detail::List("Hash Key", hash_keys(self, &cols)));
                }
                for _ in sets.iter().filter(|s| s.is_empty()) {
                    n.details.push(Detail::List("Group Key", vec!["()".into()]));
                }
                n.details.push(Detail::HashAggInfo);
                n.rows = (c.node.rows * 0.2).clamp(1.0, 200.0) * sets.len() as f64;
            }
        }
        let gcols: Vec<ColInfo> = groups
            .iter()
            .map(|g| match g {
                Expr::Col(j, _) if *j < c.node.cols.len() => c.node.cols[*j].clone(),
                g => ColInfo {
                    width: type_width(&g.ty()),
                    var: None,
                    distinct: None,
                },
            })
            .collect();
        // cost_agg, with estimate_num_groups for the group count.
        let in_rows = c.node.rows;
        // (Of the keys that group anything: processed_groupClause. None when an equality
        // fixes them all.)
        let kept: Vec<Expr> = if sets.is_some() {
            groups.to_vec()
        } else if fixed_group_keys(input, groups).iter().all(|f| *f) {
            Vec::new()
        } else {
            groups
                .iter()
                .enumerate()
                .filter(|(i, _)| !redundant.get(*i).copied().unwrap_or(false))
                .map(|(_, g)| g.clone())
                .collect()
        };
        let ngroups = self.group_num_groups(&kept, &c.node.cols, in_rows);
        let trans = costs::CPU_OPERATOR_COST
            * aggs
                .iter()
                .map(|a| 1 + a.args.iter().map(costs::ops).sum::<usize>())
                .sum::<usize>() as f64;
        let group_ops = costs::CPU_OPERATOR_COST * kept.len() as f64;
        match n.strategy {
            Some("Plain") => {
                n.rows = 1.0;
                n.startup_cost = c.node.total_cost + in_rows * trans;
                n.total_cost = n.startup_cost + costs::CPU_TUPLE_COST;
            }
            Some("Hashed") if sets.is_none() => {
                n.rows = ngroups;
                n.startup_cost = c.node.total_cost + in_rows * (trans + group_ops);
                n.total_cost = n.startup_cost + ngroups * costs::CPU_TUPLE_COST;
                // (Spilling to disk when the groups don't fit in hash_mem.)
                let space: f64 = aggs
                    .iter()
                    .map(|a| crate::sql::paths::parallel::agg_trans_space(a.def.sig.oid))
                    .sum();
                let (s, t) = crate::sql::paths::Gucs::read(self.ctx).hashagg_spill(
                    in_rows,
                    c.node.width,
                    ngroups,
                    aggs.len(),
                    space,
                );
                n.startup_cost += s;
                n.total_cost += t;
            }
            Some("Sorted") => {
                n.rows = ngroups;
                n.startup_cost = c.node.startup_cost;
                n.total_cost = c.node.total_cost
                    + in_rows * (trans + group_ops)
                    + ngroups * costs::CPU_TUPLE_COST;
            }
            _ => {
                n.startup_cost = c.node.total_cost + in_rows * (trans + group_ops);
                n.total_cost = n.startup_cost + n.rows * costs::CPU_TUPLE_COST;
            }
        }
        // (create_agg_path: the grouping target's group expressions, per group.)
        n.total_cost += n.rows * gexpr;
        let mut cols = gcols;
        // (A single grouping key is unique in the output: examine_variable's isunique, as
        // many values as groups.)
        if sets.is_none() && groups.len() == 1 && let Some(c) = cols.first_mut() {
            c.distinct = Some(costs::clamp_rows(n.rows));
        }
        cols.extend(aggs.iter().map(|a| ColInfo {
            width: type_width(&a.ty),
            var: None,
            distinct: None,
        }));
        n.width = cols.iter().map(|c| c.width).sum();
        n.cols = cols;
        n.subplans.append(&mut self.pending);
        // Grouping by hashing, or by sorting (add_paths_to_grouping_rel): the cheaper,
        // enable_hashagg / enable_sort counting first. The sorted one orders the keys as
        // the input already is (get_useful_group_keys_orderings), or as an ORDER BY above
        // wants, and keeps its order for that.
        let sorted_alt = if n.strategy == Some("Hashed") && sets.is_none() && !groups.is_empty() {
            let g = crate::sql::paths::Gucs::read(self.ctx);
            let names_all: Vec<String> = gref.iter().map(|g| self.pick(g, Cx::Upper)).collect();
            // (Orders of the keys: per key its position and direction, as a sort key's
            // suffix.)
            // (A key sorted under a collation other than the default shows it.)
            // (A key ORDER BY names sorts in its direction.)
            let coll_sfx: Vec<String> = groups
                .iter()
                .enumerate()
                .map(|(i, g)| match named.iter().find(|n| n.0 == i) {
                    Some((_, sfx)) => sfx.clone(),
                    None => match group_key_collation(input, g) {
                        Some(n) => format!(" COLLATE {}", crate::catalog::quote_ident(&n)),
                        None => String::new(),
                    },
                })
                .collect();
            // (root->group_pathkeys: the GROUP BY with the ORDER BY's keys first,
            // preprocess_groupclause.)
            let base: Vec<(usize, String)> = match hint.clone() {
                Some(hint) => {
                    let mut ord: Vec<(usize, String)> = Vec::new();
                    for (i, sfx) in hint {
                        if i < groups.len() && !ord.iter().any(|o| o.0 == i) {
                            ord.push((i, sfx));
                        }
                    }
                    ord.extend(
                        (0..groups.len())
                            .filter(|i| !ord.iter().any(|o| o.0 == *i))
                            .map(|i| (i, coll_sfx[i].clone()))
                            .collect::<Vec<_>>(),
                    );
                    ord
                }
                None => (0..groups.len()).map(|i| (i, coll_sfx[i].clone())).collect(),
            };
            let mut orders: Vec<Vec<(usize, String)>> = vec![base.clone()];
            // The input's order, as far as group keys go.
            let mut pre: Vec<(usize, String)> = Vec::new();
            for (oi, o) in c.node.order.iter().enumerate() {
                let eqs = c.node.order_eq.get(oi).cloned().unwrap_or_default();
                let found = names_all.iter().enumerate().find_map(|(i, k)| {
                    std::iter::once(o).chain(eqs.iter()).find_map(|o| {
                        let rest = o.strip_prefix(k.as_str())?;
                        (rest.is_empty() || rest.starts_with(' ')).then(|| (i, rest.to_string()))
                    })
                });
                match found {
                    Some((i, sfx)) if !pre.iter().any(|p| p.0 == i) => pre.push((i, sfx)),
                    _ => break,
                }
            }
            // (group_keys_reorder_by_pathkeys: the rest in the group pathkeys' order; with
            // incremental sort off only when the input's order covers them all.)
            if !pre.is_empty() && (g.incremental_sort || pre.len() == groups.len()) {
                let mut ord = pre.clone();
                ord.extend(base.iter().filter(|b| !pre.iter().any(|p| p.0 == b.0)).cloned());
                if !orders.contains(&ord) {
                    orders.push(ord);
                }
            }
            // (An ORDER BY of just the group keys: an ordering other than its own needs the
            // grouping's output sorted again, create_ordered_paths.)
            let final_sort = |ga: &ExNode, ord: &[(usize, String)]| -> f64 {
                if !hint_full || ord == &base[..] {
                    return 0.0;
                }
                let common = ord.iter().zip(&base).take_while(|(a, b)| a == b).count();
                if common > 0 && g.incremental_sort {
                    costs::incremental_sort_cost(
                        ga.startup_cost,
                        ga.total_cost,
                        ga.rows,
                        ga.width,
                        ga.rows.clamp(1.0, costs::DEFAULT_NUM_DISTINCT),
                        None,
                    )
                    .1 - ga.total_cost
                } else {
                    costs::sort_cost(ga.total_cost, ga.rows, ga.width, None).1 - ga.total_cost
                }
            };
            let mut best: Option<(ExNode, f64)> = None;
            let mut best_start: Option<ExNode> = None;
            // The input paths: the cheapest (sorted as needed), and the relation's partial
            // plans gathered in the query's group order (generate_useful_gather_paths;
            // sorted further only incrementally, make_ordered_path).
            let mut gathered: Vec<ExNode> = Vec::new();
            for (oi, ord) in orders.into_iter().enumerate() {
                let full_ord = ord.clone();
                let ord: Vec<(usize, String)> =
                    ord.into_iter().filter(|(i, _)| !redundant.get(*i).copied().unwrap_or(false)).collect();
                let group_names: Vec<String> = ord.iter().map(|(i, _)| names_all[*i].clone()).collect();
                let keys_ord: Vec<String> = ord.iter().map(|(i, sfx)| names_all[*i].clone() + sfx).collect();
                let group_sort: Vec<(usize, bool, bool)> = ord
                    .iter()
                    .map(|(i, sfx)| {
                        let (d, nf) = sort_dir(sfx);
                        (*i, d, nf)
                    })
                    .collect();
                let ord: Vec<usize> = ord.iter().map(|o| o.0).collect();
                if oi == 0
                    && let Some((pi, others, rel_rows)) = &gather_in
                {
                    let cols: Vec<Option<usize>> = ord
                        .iter()
                        .map(|&i| match &groups[i] {
                            Expr::Col(j, _) => Some(*j),
                            _ => None,
                        })
                        .collect();
                    gathered = self.useful_gathers(pi, others, &keys_ord, &cols, *rel_rows, c.node.key, true);
                }
                let cands: Vec<(ExNode, bool)> = std::iter::once((c.node.clone(), true))
                    .chain(gathered.iter().map(|x| (x.clone(), false)))
                    .collect();
                for (cand, cheapest) in cands {
                let common = keys_ord
                    .iter()
                    .zip(&cand.order)
                    .enumerate()
                    .take_while(|(oi, (a, b))| {
                        a == b || cand.order_eq.get(*oi).is_some_and(|e| e.contains(a))
                    })
                    .count();
                if !cheapest && common < keys_ord.len() && (common == 0 || !g.incremental_sort) {
                    continue;
                }
                let mut input = cand;
                if common < keys_ord.len() {
                    let incremental = common > 0 && g.incremental_sort;
                    let mut sn = ExNode::new(if incremental { "Incremental Sort" } else { "Sort" });
                    sn.details.push(Detail::List("Sort Key", keys_ord.clone()));
                    if incremental {
                        sn.details
                            .push(Detail::List("Presorted Key", group_names[..common].to_vec()));
                        let pre_cols: Vec<ColInfo> = ord[..common]
                            .iter()
                            .map(|&i| match &groups[i] {
                                Expr::Col(j, _) => input.cols.get(*j).cloned().unwrap_or_default(),
                                _ => ColInfo::default(),
                            })
                            .collect();
                        let ng = if pre_cols.iter().any(|x| x.var.is_none()) {
                            input.rows.clamp(1.0, costs::DEFAULT_NUM_DISTINCT)
                        } else {
                            self.num_groups(&pre_cols, input.rows)
                        };
                        (sn.startup_cost, sn.total_cost) = costs::incremental_sort_cost(
                            input.startup_cost,
                            input.total_cost,
                            input.rows,
                            input.width,
                            ng,
                            None,
                        );
                        sn.rows = input.rows;
                        sn.width = input.width;
                        sn.cols = input.cols.clone();
                    } else {
                        sn.details.push(Detail::SortInfo);
                        self.sort_costs(&mut sn, &input);
                        sn.disabled = !g.sort;
                    }
                    sn.key = input.key;
                    sn.order = keys_ord.clone();
                    if self.o.verbose {
                        sn.output = input.output.iter().map(&sort_out).collect();
                    }
                    sn.children.push(input.child("Outer"));
                    input = sn;
                }
                let mut ga = n.clone();
                ga.strategy = Some("Sorted");
                put_group_sort(&mut ga.details, group_sort.clone());
                ga.details.retain(|d| !matches!(d, Detail::HashAggInfo));
                for d in &mut ga.details {
                    if let Detail::List("Group Key", k) = d {
                        *k = group_names.clone();
                    }
                }
                ga.startup_cost = input.startup_cost;
                // (Over this input path's rows: a Gather Merge returns the relation's.)
                let in_rows = if cheapest { in_rows } else { input.rows };
                if no_aggs {
                    // cost_group.
                    ga.title = "Group".into();
                    ga.node_type = "Group";
                    ga.strategy = None;
                    ga.partial_mode = None;
                    ga.total_cost = input.total_cost + in_rows * group_ops;
                } else {
                    ga.title = "GroupAggregate".into();
                    ga.total_cost = input.total_cost
                        + in_rows * (trans + group_ops)
                        + ngroups * costs::CPU_TUPLE_COST;
                }
                ga.total_cost += ngroups * gexpr;
                ga.disabled = false;
                ga.order = keys_ord.clone();
                ga.children.push(input.child("Outer"));
                let start = (parallel::disabled_nodes(&ga), ga.startup_cost);
                if best_start
                    .as_ref()
                    .is_none_or(|b| start < (parallel::disabled_nodes(b), b.startup_cost))
                {
                    best_start = Some(ga.clone());
                }
                let extra = final_sort(&ga, &full_ord);
                let better = match &best {
                    None => true,
                    Some((b, bx)) => {
                        let (da, db) = (parallel::disabled_nodes(&ga), parallel::disabled_nodes(b));
                        if da != db {
                            da < db
                        } else {
                            let mut x = ga.clone();
                            x.total_cost += extra;
                            let mut y = b.clone();
                            y.total_cost += bx;
                            parallel::cheaper(&x, &y)
                        }
                    }
                };
                if better {
                    best = Some((ga, extra));
                }
                }
            }
            n.disabled = !g.hashagg;
            let mut best = best.map(|b| b.0);
            // (add_path keeps the one cheapest to start beside it: a LIMIT may take that.)
            if let (Some(b), Some(s)) = (&mut best, best_start)
                && s.startup_cost < b.startup_cost
                && parallel::disabled_nodes(&s) <= parallel::disabled_nodes(b)
            {
                b.alt = Some(Box::new(s));
            }
            best
        } else {
            None
        };
        // (An index-only scan the aggregation reads as it comes returns its physical
        // target list, the index's columns in its order: use_physical_tlist under
        // CP_LABEL_TLIST, the group keys being plain columns.)
        let ios_out: Option<Vec<String>> = match &**input {
            Plan::Scan(sc) if self.o.verbose => sc.index.as_ref().and_then(|(ix, _)| {
                ix.columns
                    .iter()
                    .map(|col| match col.key {
                        crate::catalog::IndexKey::Column(k) => Some(k),
                        _ => None,
                    })
                    .chain(ix.include.iter().map(|&k| Some(k)))
                    .map(|k| k.and_then(|k| c.exported.get(k)).map(|nm| self.pick(nm, Cx::Output)))
                    .collect()
            }),
            _ => None,
        };
        let physical_ios = |n: &mut ExNode| {
            if let (Some(out), Some(k)) = (&ios_out, n.children.first_mut())
                && k.node_type == "Index Only Scan"
            {
                k.output = out.clone();
            }
        };
        n.children.push(c.node.child("Outer"));
        physical_ios(&mut n);
        let mut sorted_alt = sorted_alt;
        if let Some(ga) = &mut sorted_alt {
            physical_ios(ga);
        }
        if let Some(ga) = sorted_alt {
            // (grouping_is_hashable: a key of a type whose `=` doesn't hash, money or seg,
            // leaves only the sorted grouping.)
            let can_hash = {
                let cat = self.ctx.catalog();
                groups
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !redundant.get(*i).copied().unwrap_or(false))
                    .all(|(_, g)| cat.type_hashable(&g.ty()))
            };
            // (add_path: the sorted grouping, its pathkeys better, removes the hashed one
            // unless that is fuzzily cheaper in total, or as cheap but cheaper to start;
            // both kept, the cheaper in total is the relation's cheapest path.)
            let removes = {
                const FUZZ: f64 = 1.01;
                let (ds, dh) = (parallel::disabled_nodes(&ga), parallel::disabled_nodes(&n));
                if ds != dh {
                    ds < dh
                } else if ga.total_cost > n.total_cost * FUZZ {
                    false
                } else if n.total_cost > ga.total_cost * FUZZ {
                    true
                } else {
                    ga.startup_cost <= n.startup_cost * FUZZ
                }
            };
            if !can_hash {
                n = ga;
            } else if removes || parallel::cheaper(&ga, &n) {
                let mut hashed = std::mem::replace(&mut n, ga);
                hashed.alt = n.alt.take();
                n.alt = Some(Box::new(hashed));
            } else {
                n.alt = Some(Box::new(ga));
            }
        }
        // Or partly in parallel workers (its partial forms count as it).
        if n.key.is_none() {
            n.key = my_key;
        }
        if sets.is_none()
            && let Some(pi) = part_in
            && let Some(mut f) = self.parallel_agg(
                &n,
                *pi,
                gather_in.as_ref().map_or(&[][..], |g| &g.1[..]),
                groups,
                aggs,
                &gnames,
                &anames,
            )
        {
            let mut serial = std::mem::take(&mut n);
            // (A serial grouping in order stays beside an unordered parallel one, and the
            // one cheapest to start beside that: the grouping relation keeps them, for the
            // ORDER BY or LIMIT above.)
            let mut forms: Vec<ExNode> = Vec::new();
            let mut next = serial.alt.take();
            while let Some(mut a) = next {
                next = a.alt.take();
                forms.push(*a);
            }
            if f.order.is_empty() {
                let sorted: Vec<&ExNode> =
                    std::iter::once(&serial).chain(forms.iter()).filter(|x| !x.order.is_empty()).collect();
                let total = sorted.iter().copied().reduce(|b, x| if parallel::cheaper(x, b) { x } else { b });
                let start = sorted.iter().copied().reduce(|b, x| {
                    if (parallel::disabled_nodes(x), x.startup_cost) < (parallel::disabled_nodes(b), b.startup_cost) {
                        x
                    } else {
                        b
                    }
                });
                if let Some(t) = total {
                    let mut t = t.clone();
                    t.alt = start
                        .filter(|s| s.startup_cost < t.startup_cost)
                        .map(|s| Box::new(s.clone()));
                    f.alt = Some(Box::new(t));
                }
            }
            if !parallel::has_gather(&serial) {
                f.safe_alt = Some(Box::new(serial));
            }
            n = f;
        }
        // Or partition by partition.
        if sets.is_none() {
            // (As the nodes above the partitions' groupings refer to the keys: a key
            // computed below them wrapped, as the serial grouping's sort keys are.)
            let names: Vec<String> = gref.iter().map(|g| self.pick(g, Cx::Upper)).collect();
            self.group_order = hint.clone();
            let pw = self.partitionwise_agg(input, groups, aggs, &n, having.as_ref(), &names, hint_full);
            self.group_order = None;
            if let Some(pw) = pw
            {
                n = pw;
            }
        }
        self.key_sub_ids = saved_keys;
        Built {
            node: n,
            internal,
            exported,
        }
    }

    fn frame_text(&mut self, calls: &[&WindowCall], f: &Frame) -> String {
        let default = f.unit == FrameUnit::Range
            && matches!(f.start, FrameBound::UnboundedPreceding)
            && matches!(f.end, FrameBound::CurrentRow)
            && f.exclude == FrameExclude::NoOthers
            && !f.between;
        // Ranking functions don't depend on the frame, so Postgres runs them in ROWS mode.
        let rows_ok = calls.iter().all(|c| {
            matches!(
                c.func,
                WinFunc::Builtin(
                    WinKind::RowNumber
                        | WinKind::Rank
                        | WinKind::DenseRank
                        | WinKind::PercentRank
                        | WinKind::CumeDist
                        | WinKind::Ntile
                )
            )
        });
        if default && rows_ok {
            return "ROWS UNBOUNDED PRECEDING".into();
        }
        if default {
            return String::new();
        }
        let mut bound = |b: &FrameBound| match b {
            FrameBound::UnboundedPreceding => "UNBOUNDED PRECEDING".to_string(),
            FrameBound::Preceding(e) => format!("{} PRECEDING", self.show(e, &[], Cx::Upper)),
            FrameBound::CurrentRow => "CURRENT ROW".to_string(),
            FrameBound::Following(e) => format!("{} FOLLOWING", self.show(e, &[], Cx::Upper)),
            FrameBound::UnboundedFollowing => "UNBOUNDED FOLLOWING".to_string(),
        };
        let unit = match f.unit {
            FrameUnit::Rows => "ROWS",
            FrameUnit::Range => "RANGE",
            FrameUnit::Groups => "GROUPS",
        };
        let mut s = if matches!(f.end, FrameBound::CurrentRow) && !f.between {
            format!("{unit} {}", bound(&f.start))
        } else {
            let a = bound(&f.start);
            let b = bound(&f.end);
            format!("{unit} BETWEEN {a} AND {b}")
        };
        match f.exclude {
            FrameExclude::NoOthers => {}
            FrameExclude::CurrentRow => s.push_str(" EXCLUDE CURRENT ROW"),
            FrameExclude::Group => s.push_str(" EXCLUDE GROUP"),
            FrameExclude::Ties => s.push_str(" EXCLUDE TIES"),
        }
        s
    }

    fn window(&mut self, key: usize, input: &Arc<Plan>, calls: &[WindowCall]) -> Built {
        // (A JSON_ARRAYAGG / JSON_OBJECTAGG window function computes as its aggregate;
        // the projection above shows the constructor.)
        let owned: Vec<WindowCall> = calls
            .iter()
            .map(|c| {
                let mut c = c.clone();
                if let WinFunc::Agg(a) = &mut c.func {
                    a.json_ctor = false;
                }
                c
            })
            .collect();
        let calls = &owned[..];
        // (Taken before the input is converted: its own sorts set their own.)
        let order_refs: Vec<String> = WINDOW_ORDER.with(|w| std::mem::take(&mut *w.borrow_mut()));
        let tlist_cols: Vec<usize> = WINDOW_TLIST.with(|w| std::mem::take(&mut *w.borrow_mut()));
        let input_sorted = matches!(&**input, Plan::Sort { .. });
        let classes = super::paths::column_classes(input);
        let mut c = self.node(input);
        self.settle(&mut c.node);
        // Calls sharing a window definition run in one WindowAgg.
        struct Win<'w> {
            keys: Vec<String>,
            /// The keys the Sort below needs: pseudo-constant ones order nothing.
            sort: Vec<String>,
            spec: String,
            calls: Vec<&'w WindowCall>,
        }
        let mut wins: Vec<Win> = Vec::new();
        let mut call_win = Vec::new();
        // optimize_window_clauses: a window clause whose functions all ignore the frame
        // (row_number, rank, …) gets ROWS UNBOUNDED PRECEDING, and then joins an
        // identical clause.
        let clause_of = |call: &WindowCall| format!("{:?}{:?}{:?}", call.partition_by, call.order_by, call.frame);
        let frameless = |call: &WindowCall| {
            matches!(
                call.func,
                WinFunc::Builtin(
                    WinKind::RowNumber
                        | WinKind::Rank
                        | WinKind::DenseRank
                        | WinKind::PercentRank
                        | WinKind::CumeDist
                        | WinKind::Ntile
                )
            )
        };
        let frame_of = |call: &WindowCall| -> Frame {
            let k = clause_of(call);
            if calls.iter().filter(|c| clause_of(c) == k).all(&frameless) {
                Frame {
                    unit: FrameUnit::Rows,
                    start: FrameBound::UnboundedPreceding,
                    end: FrameBound::CurrentRow,
                    exclude: FrameExclude::NoOthers,
                    between: false,
                }
            } else {
                call.frame.clone()
            }
        };
        let mut win_frames: Vec<Frame> = Vec::new();
        for call in calls {
            // Keys are the input's columns: an expression shows parenthesized, as a reference
            // to a computed column does. Constant partition keys are dropped.
            let mut key = |e: &Expr| {
                let shown = self.show(e, &c.exported, Cx::Upper);
                if matches!(e, Expr::Col(..) | Expr::Outer(..)) {
                    shown
                } else {
                    format!("({shown})")
                }
            };
            // (A key a WHERE equality makes constant is redundant too: its pathkey goes.)
            let part_exprs: Vec<&Expr> = call
                .partition_by
                .iter()
                .filter(|e| !crate::sql::binder::optimize::pseudo_constant(e))
                .filter(|e| !matches!(e, Expr::Col(j, _) if classes.get(*j).is_some_and(|c| c.1)))
                .collect();
            // (A key carrying its column's collation is the column; its sort shows the
            // collation.)
            let part: Vec<String> = part_exprs
                .iter()
                .map(|e| match e {
                    Expr::Collate(x, _, -1) if matches!(&**x, Expr::Col(..)) => key(x),
                    e => key(e),
                })
                .collect();
            let part_sort: Vec<String> = part_exprs
                .iter()
                .zip(&part)
                .map(|(e, p)| match e {
                    Expr::Collate(x, n, -1) if matches!(&**x, Expr::Col(..)) => {
                        format!("{p} COLLATE {}", quote_ident(n))
                    }
                    _ => p.clone(),
                })
                .collect();
            let order: Vec<String> = call
                .order_by
                .iter()
                .map(|k| key(&k.expr) + &sort_suffix(k))
                .collect();
            let mut keys = part_sort.clone();
            keys.extend(order.iter().cloned());
            let exprs = part_exprs
                .into_iter()
                .chain(call.order_by.iter().map(|k| &k.expr));
            // (Keys an equivalence class makes redundant: a constant one, one equal to
            // an earlier key.)
            let mut seen: Vec<usize> = Vec::new();
            let sort = keys
                .iter()
                .zip(exprs)
                .filter(|(_, e)| !crate::sql::binder::optimize::pseudo_constant(e))
                .filter(|(_, e)| match e {
                    Expr::Col(j, _) => match classes.get(*j) {
                        Some(&(r, konst)) => {
                            if konst || seen.contains(&r) {
                                false
                            } else {
                                seen.push(r);
                                true
                            }
                        }
                        None => true,
                    },
                    _ => true,
                })
                .map(|(k, _)| k.clone())
                .collect();
            let frame = frame_of(call);
            let raw = format!("{part:?}{order:?}{frame:?}");
            let i = match wins.iter().position(|w| w.spec == raw && w.keys == keys) {
                Some(i) => i,
                None => {
                    win_frames.push(frame);
                    wins.push(Win {
                        keys,
                        sort,
                        spec: raw,
                        calls: Vec::new(),
                    });
                    wins.len() - 1
                }
            };
            wins[i].calls.push(call);
            call_win.push((i, part, order));
        }
        // select_active_windows: the windows ordered by their sort clauses (higher sort
        // group references first; one that is a prefix of another after it), so windows
        // needing the same order are adjacent and the stronger order is sorted for first.
        // References go to ORDER BY items, then each window's ORDER BY and PARTITION BY.
        let mut refs: Vec<String> = order_refs;
        let mut ref_of = |e: &Expr| {
            let k = format!("{e:?}");
            match refs.iter().position(|r| *r == k) {
                Some(i) => i,
                None => {
                    refs.push(k);
                    refs.len() - 1
                }
            }
        };
        let mut unique_orders: Vec<Vec<(usize, bool, bool)>> = Vec::new();
        for w in &wins {
            let call = w.calls[0];
            for k in &call.order_by {
                ref_of(&k.expr);
            }
            for e in &call.partition_by {
                ref_of(e);
            }
            let mut u: Vec<(usize, bool, bool)> = Vec::new();
            for e in &call.partition_by {
                let x = (ref_of(e), false, false);
                if !u.contains(&x) {
                    u.push(x);
                }
            }
            for k in &call.order_by {
                let x = (ref_of(&k.expr), k.desc, k.nulls_first);
                if !u.contains(&x) {
                    u.push(x);
                }
            }
            unique_orders.push(u);
        }
        let mut sequence: Vec<usize> = (0..wins.len()).collect();
        sequence.sort_by(|&a, &b| {
            use std::cmp::Ordering;
            for (x, y) in unique_orders[a].iter().zip(&unique_orders[b]) {
                let o = y
                    .0
                    .cmp(&x.0)
                    .then(x.1.cmp(&y.1).reverse())
                    .then(x.2.cmp(&y.2).reverse());
                if o != Ordering::Equal {
                    return o;
                }
            }
            unique_orders[b].len().cmp(&unique_orders[a].len())
        });
        // Windows are named by their WINDOW clause name, else w1, w2, … in the order
        // they are computed, skipping names in use (name_active_windows).
        let given: Vec<Option<String>> =
            wins.iter().map(|w| w.calls.iter().find_map(|c| c.name.clone())).collect();
        let mut name_of = vec![String::new(); wins.len()];
        let mut next = 1;
        for &wi in &sequence {
            name_of[wi] = match &given[wi] {
                Some(n) => quote_ident(n),
                None => loop {
                    let cand = format!("w{next}");
                    next += 1;
                    if !given.iter().any(|g| g.as_deref() == Some(cand.as_str())) {
                        break cand;
                    }
                },
            };
        }
        let mut specs = Vec::new();
        for (i, w) in wins.iter().enumerate() {
            let (_, part, order) = call_win.iter().find(|(j, ..)| *j == i).expect("window");
            let frame = self.frame_text(&w.calls, &win_frames[i]);
            let mut parts = Vec::new();
            if !part.is_empty() {
                parts.push(format!("PARTITION BY {}", part.join(", ")));
            }
            // An ordering a GiST or SP-GiST scan's ordering operators give the window
            // computes it there: its keys show as the expressions themselves.
            let knn_sorted = !w.sort.is_empty()
                && c.node.title.starts_with("Index")
                && c.node.order.len() >= w.sort.len()
                && c.node.order[..w.sort.len()] == w.sort[..];
            let order: Vec<String> = if knn_sorted {
                order
                    .iter()
                    .map(|k| match k.strip_prefix('(') {
                        Some(r) if k.starts_with("((") => match r.rfind(')') {
                            Some(e) => format!("{}{}", &r[..e], &r[e + 1..]),
                            None => k.clone(),
                        },
                        _ => k.clone(),
                    })
                    .collect()
            } else {
                order.clone()
            };
            // (show_window_keys: no sort order shown, WindowAgg carrying only equality
            // operators.)
            if !order.is_empty() {
                let bare: Vec<String> = order
                    .iter()
                    .map(|k| {
                        let k = match k.find(" USING ") {
                            Some(p) => &k[..p],
                            None => k.as_str(),
                        };
                        presorted_name(k)
                    })
                    .collect();
                parts.push(format!("ORDER BY {}", bare.join(", ")));
            }
            if !frame.is_empty() {
                parts.push(frame);
            }
            specs.push(format!("{} AS ({})", name_of[i], parts.join(" ")));
        }
        let mut cnames = Vec::new();
        for (call, (i, ..)) in calls.iter().zip(&call_win) {
            let f = match &call.func {
                WinFunc::Builtin(k) => {
                    // (An InitPlan's value the input passes on: the Param itself, bare —
                    // its subquery was pulled up.)
                    let bare = |s: String| match s.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
                        Some(x) if x.starts_with("(InitPlan ") && x.ends_with(".col1") => x.to_string(),
                        _ => s,
                    };
                    let args: Vec<Name> = call
                        .args
                        .iter()
                        .map(|a| {
                            let n = self.name(a, &c.exported, &[]);
                            Name {
                                plain: bare(n.plain),
                                full: bare(n.full),
                            }
                        })
                        .collect();
                    let list = |full: bool| {
                        args.iter()
                            .map(|n| if full { n.full.clone() } else { n.plain.clone() })
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    Name {
                        plain: format!("{}({})", win_name(*k), list(false)),
                        full: format!("{}({})", win_name(*k), list(true)),
                    }
                }
                WinFunc::Agg(a) => {
                    // (Sub-selects in its arguments are the node's: InitPlans, SubPlans.)
                    for x in a.args.iter().chain(a.filter.as_ref()) {
                        self.register(x, &c.exported);
                    }
                    self.name(&Expr::Agg(0, a.ty.clone()), &c.exported, std::slice::from_ref(a))
                }
            };
            cnames.push(Name {
                plain: format!("{} OVER {}", f.plain, name_of[*i]),
                full: format!("{} OVER {}", f.full, name_of[*i]),
            });
        }
        // (The top WindowAgg computes its own calls; those of the ones below it it reads.)
        let last_win = sequence.last().copied();
        let mut internal = c.exported.clone();
        internal.extend(cnames.iter().zip(&call_win).map(|(n, (i, ..))| {
            if Some(*i) == last_win {
                n.clone()
            } else {
                n.wrapped()
            }
        }));
        let mut exported = c.exported.clone();
        exported.extend(cnames.iter().map(Name::wrapped));
        let width = c.node.width + calls.iter().map(|x| type_width(&x.ty)).sum::<u32>();
        let mut cur = c.node;
        // A scan sorted for the windows returns just the columns needed (CP_SMALL_TLIST):
        // those used above, then the windows' keys.
        if self.o.verbose && cur.is_scan() && wins.iter().any(|w| !w.sort.is_empty()) {
            let ninput = c.internal.len();
            let mut order: Vec<usize> = Vec::new();
            // (make_window_input_target: the windows' plain keys first — in the target
            // list's order, then the resjunk ones — then the other columns read.)
            let mut keys: Vec<usize> = Vec::new();
            for call in calls {
                for e in call.partition_by.iter().chain(call.order_by.iter().map(|k| &k.expr)) {
                    if let Expr::Col(col, _) = e
                        && *col < ninput
                        && !keys.contains(col)
                    {
                        keys.push(*col);
                    }
                }
            }
            for col in &tlist_cols {
                if keys.contains(col) && !order.contains(col) {
                    order.push(*col);
                }
            }
            for col in keys {
                if !order.contains(&col) {
                    order.push(col);
                }
            }
            if let Some(need) = self.needed.get(&key) {
                for i in (0..ninput).filter(|&i| need.get(i).copied().unwrap_or(true)) {
                    if !order.contains(&i) {
                        order.push(i);
                    }
                }
            }
            for call in calls {
                for e in call
                    .partition_by
                    .iter()
                    .chain(call.order_by.iter().map(|k| &k.expr))
                    .chain(call.args.iter())
                {
                    for col in e.columns() {
                        if col < ninput && !order.contains(&col) {
                            order.push(col);
                        }
                    }
                }
            }
            if !order.is_empty() && order.len() < ninput {
                cur.output = order
                    .iter()
                    .map(|&i| self.pick(&c.internal[i], Cx::Output))
                    .collect();
            }
        }
        let _ = input_sorted;
        let mut sorted: Vec<String> = cur.order.clone();
        let mut specs: Vec<Option<String>> = specs.into_iter().map(Some).collect();
        // VERBOSE: the windows' computed PARTITION BY / ORDER BY keys are columns the input
        // computes first (make_window_input_target), passed up as references.
        let mut junk: Vec<(String, String)> = Vec::new();
        if self.o.verbose {
            let mut seen: Vec<&Expr> = Vec::new();
            for call in calls {
                for e in call.partition_by.iter().chain(call.order_by.iter().map(|k| &k.expr)) {
                    if matches!(e, Expr::Col(..))
                        || crate::sql::binder::optimize::pseudo_constant(e)
                        || seen.iter().any(|x| x.same_as(e))
                    {
                        continue;
                    }
                    seen.push(e);
                    let n = self.name(e, &c.exported, &[]);
                    junk.push((self.pick(&n, Cx::Output), self.pick(&n.wrapped(), Cx::Output)));
                }
            }
            if !junk.is_empty() {
                let rest = if cur.output.is_empty() {
                    c.exported.iter().map(|n| self.pick(n, Cx::Output)).collect()
                } else {
                    cur.output.clone()
                };
                cur.output = junk.iter().map(|j| j.0.clone()).chain(rest).collect();
            }
        }
        // (A reference to a computed key: wrapped.)
        let refer = |out: &[String]| -> Vec<String> {
            out.iter()
                .map(|o| junk.iter().find(|j| j.0 == *o).map_or_else(|| o.clone(), |j| j.1.clone()))
                .collect()
        };
        // VERBOSE: a WindowAgg below another returns its input's columns, then the calls
        // computed so far.
        let input_out: Vec<String> = if cur.output.is_empty() {
            c.exported.iter().map(|n| self.pick(n, Cx::Output)).collect()
        } else {
            refer(&cur.output)
        };
        let mut done_out: Vec<String> = Vec::new();
        for wi in sequence {
            let w = &wins[wi];
            let spec = specs[wi].take().unwrap_or_default();
            let mut need: Vec<String> = Vec::new();
            for k in &w.sort {
                if !need.contains(k) {
                    need.push(k.clone());
                }
            }
            let common = need.iter().zip(&sorted).take_while(|(a, b)| a == b).count();
            if !need.is_empty() && common < need.len() {
                let mut s = if common > 0 {
                    let mut s = ExNode::new("Incremental Sort");
                    s.details.push(Detail::List("Sort Key", need.clone()));
                    s.details.push(Detail::List(
                        "Presorted Key",
                        need[..common].iter().map(|k| presorted_name(k)).collect(),
                    ));
                    let groups = cur.rows.clamp(1.0, costs::DEFAULT_NUM_DISTINCT);
                    (s.startup_cost, s.total_cost) = costs::incremental_sort_cost(
                        cur.startup_cost,
                        cur.total_cost,
                        cur.rows,
                        cur.width,
                        groups,
                        None,
                    );
                    s.rows = cur.rows;
                    s.width = cur.width;
                    s.cols = cur.cols.clone();
                    s
                } else {
                    let mut s = ExNode::new("Sort");
                    s.details.push(Detail::List("Sort Key", need.clone()));
                    s.details.push(Detail::SortInfo);
                    self.sort_costs(&mut s, &cur);
                    s
                };
                s.key = cur.key;
                if self.o.verbose {
                    // (Over a WindowAgg: references to what it computed.)
                    s.output = if cur.node_type == "WindowAgg" {
                        input_out.iter().chain(&done_out).cloned().collect()
                    } else {
                        refer(&cur.output)
                    };
                }
                s.children.push(cur.child("Outer"));
                cur = s;
                sorted = need;
            }
            let mut n = ExNode::new("WindowAgg");
            n.details.push(Detail::Text("Window", spec));
            n.win_calls = call_win
                .iter()
                .enumerate()
                .filter(|(_, (j, ..))| *j == wi)
                .map(|(ci, _)| (ci, self.pick(&cnames[ci], Cx::Upper)))
                .collect();
            n.rows = cur.rows;
            n.width = width;
            // cost_windowagg: each function per row, a comparison per key, a tuple each.
            let rows = cur.rows;
            let fcost: f64 = w
                .calls
                .iter()
                .map(|x| {
                    costs::CPU_OPERATOR_COST
                        + x.args.iter().map(costs::qual_cost).sum::<f64>()
                })
                .sum();
            let first = w.calls[0];
            let nkeys = first.partition_by.len() + first.order_by.len();
            n.startup_cost = cur.startup_cost;
            n.total_cost = cur.total_cost
                + fcost * rows
                + costs::CPU_OPERATOR_COST * nkeys as f64 * rows
                + costs::CPU_TUPLE_COST * rows;
            // Rows read before the first is returned (get_windowclause_startup_tuples).
            let col_of = |e: &Expr| match e {
                Expr::Col(j, _) => cur.cols.get(*j).cloned().unwrap_or_default(),
                _ => ColInfo::default(),
            };
            let part_rows = if first.partition_by.is_empty() {
                rows
            } else {
                let cols: Vec<ColInfo> = first.partition_by.iter().map(col_of).collect();
                rows / self.num_groups(&cols, rows)
            };
            let peers = if first.order_by.is_empty() {
                1.0
            } else {
                let cols: Vec<ColInfo> = first.order_by.iter().map(|k| col_of(&k.expr)).collect();
                part_rows / self.num_groups(&cols, part_rows)
            };
            // (The clause's frame as optimize_window_clauses left it.)
            let f = &win_frames[wi];
            let mut ret = match (&f.end, f.unit) {
                (FrameBound::UnboundedFollowing, _) => part_rows,
                (FrameBound::CurrentRow, FrameUnit::Rows) => 1.0,
                (FrameBound::CurrentRow, _) if first.order_by.is_empty() => part_rows,
                (FrameBound::CurrentRow, _) => peers,
                (FrameBound::Preceding(_), _) => 1.0,
                (FrameBound::Following(e), unit) => {
                    let off = match e {
                        Expr::Const(crate::types::Value::Int2(v), _) => *v as f64,
                        Expr::Const(crate::types::Value::Int4(v), _) => *v as f64,
                        Expr::Const(crate::types::Value::Int8(v), _) => *v as f64,
                        Expr::Const(crate::types::Value::Null, _) => 1.0,
                        _ => part_rows / peers * 0.3333333333333333,
                    };
                    if unit == FrameUnit::Rows {
                        off + 1.0
                    } else {
                        peers * (off + 1.0)
                    }
                }
                _ => 1.0,
            };
            ret = if nkeys > 0 {
                (ret + 1.0).min(part_rows)
            } else {
                ret.min(part_rows)
            };
            let startup_rows = costs::clamp_rows(ret);
            if startup_rows > 1.0 && rows > 0.0 {
                n.startup_cost +=
                    (n.total_cost - n.startup_cost) / rows * (startup_rows - 1.0);
            }
            n.cols = cur.cols.clone();
            n.cols.extend(w.calls.iter().map(|x| ColInfo {
                width: type_width(&x.ty),
                var: None,
                distinct: None,
            }));
            n.key = cur.key;
            if self.o.verbose && Some(wi) != last_win {
                let own: Vec<String> = cnames
                    .iter()
                    .zip(&call_win)
                    .filter(|(_, (j, ..))| *j == wi)
                    .map(|(nm, _)| self.pick(nm, Cx::Output))
                    .collect();
                n.output = input_out.iter().chain(&done_out).chain(&own).cloned().collect();
                done_out.extend(own.iter().map(|o| format!("({o})")));
            }
            n.children.push(cur.child("Outer"));
            cur = n;
        }
        cur.order = sorted;
        cur.key = None;
        cur.subplans.append(&mut self.pending);
        Built {
            node: cur,
            internal,
            exported,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn join(
        &mut self,
        at: usize,
        kind: JoinKind,
        left: &Arc<Plan>,
        right: &Arc<Plan>,
        cond: Option<&Expr>,
        hash: Option<&(Vec<Expr>, Vec<Expr>)>,
        params: &[Expr],
        meta: &super::plan::JoinMeta,
    ) -> Built {
        for side in [left, right] {
            if matches!(&**side, Plan::Distinct { .. }) {
                self.unique_sides.insert(addr(side));
            }
        }
        // (An outer join's nullable sides and clauses: `OUTER_JOIN_SIDE`; the caller restores.)
        // (An inner join's own clauses come out of classes again, under an outer join too.)
        let outer = !matches!(kind, JoinKind::Inner | JoinKind::Semi);
        OUTER_JOIN_SIDE.with(|o| o.set(matches!(kind, JoinKind::Right | JoinKind::Full)));
        let mut l = self.node(left);
        // (Its PlaceHolderVar parameters added to the outer plan's target list don't count in
        // its width: change_plan_targetlist keeps the plan's, the path's reltarget width.)
        if let Some(w) = meta.phv_outer_width {
            l.node.width = w;
        }
        if outer {
            OUTER_JOIN_SIDE.with(|o| o.set(true));
        }
        let lateral = !params.is_empty();
        // A scan whose row the right side reads whole, and nothing of it else: its target
        // list is that whole-row Var (use_physical_tlist doesn't build one then).
        if self.o.verbose
            && lateral
            && let Plan::Scan(sc) = &**left
            && cond.is_none()
            && hash.is_none()
            && self.needed.get(&at).is_some_and(|m| m.iter().take(l.internal.len()).all(|x| !x))
        {
            let ncols = sc.table.columns.len();
            let whole = |e: &Expr| {
                matches!(e, Expr::Row(items, _) if items.len() == ncols
                    && crate::sql::binder::is_whole_row(e)
                    && items.iter().enumerate().all(|(i, x)| matches!(x, Expr::Col(j, _) if *j == i)))
            };
            if params.iter().any(whole)
                && params.iter().all(|e| whole(e) || e.columns().is_empty())
                && l.node.node_type == "Seq Scan"
            {
                l.node.output = vec![format!("{}.*", quote_ident(&self.refname(left)))];
            }
        }
        // (Parameters holding sub-selects: shown where the inner side reads them, each
        // place registering its SubPlan; here only labeled.)
        let sub_frame = lateral && params.iter().any(has_subselect);
        if lateral {
            let saved = self.detach;
            self.detach |= sub_frame;
            let mut names: Vec<String> = params
                .iter()
                .map(|e| self.name(e, &l.exported, &[]).full)
                .collect();
            self.detach = saved;
            if sub_frame {
                let base = names.len();
                names.extend(l.exported.iter().map(|n| n.full.clone()));
                self.sub_params.push((self.outer.len() + 1, params.to_vec(), base));
            }
            self.outer.insert(0, names);
            self.outer_depth.insert(0, self.depth);
        }
        let saved_inner = MERGE_INNER.with(|m| {
            m.replace(if meta.method == Some(crate::sql::plan::JoinMethod::Merge) { addr(right) } else { 0 })
        });
        let r = self.node(right);
        MERGE_INNER.with(|m| m.set(saved_inner));
        if lateral {
            self.outer.remove(0);
            self.outer_depth.remove(0);
        }
        if sub_frame {
            self.sub_params.pop();
        }
        let copy = |b: &Built| Built {
            node: b.node.clone(),
            internal: b.internal.clone(),
            exported: b.exported.clone(),
        };
        let parts = l.node.partial.is_some() || r.node.partial.is_some();
        // (A merge join's inputs sorted for it: the join's other partial paths read its
        // sides as their relations give them, with their partial plans.)
        let unsorted = |b: &Built| -> Built {
            match &b.node.presort {
                Some(p) if meta.method == Some(super::plan::JoinMethod::Merge) && b.node.node_type == "Sort" => Built {
                    node: p.0.clone(),
                    internal: p.1.clone(),
                    exported: b.exported.clone(),
                },
                _ => copy(b),
            }
        };
        let (lu, ru) = (unsorted(&l), unsorted(&r));
        let unsorted_parts = lu.node.partial.is_some() || ru.node.partial.is_some();
        let (mut lg, mut rg) = (copy(&l), copy(&r));
        let mut lateral_gathered = false;
        if parts && !lateral {
            // (A join's inputs gathered as their relations' cheapest paths: in no order.)
            lg.node.wanted_order.clear();
            rg.node.wanted_order.clear();
            self.settle(&mut lg.node);
            self.settle(&mut rg.node);
        } else if lateral && l.node.partial.is_some() {
            // (A LATERAL join's outer relation has its Gather path too
            // (generate_useful_gather_paths at the base relation): a serial nested loop may
            // read it. The inner side, parameterized by the outer, has no partial paths to
            // gather.)
            lg.node.wanted_order.clear();
            self.settle(&mut lg.node);
            lateral_gathered = parallel::has_gather(&lg.node) && !parallel::has_gather(&l.node);
        }
        // (A partial nested loop takes the inner side's cheapest path, which a Gather
        // can't run under.)
        let lkey = l.node.key;
        // (An input read in the order the query wants: the join keeps that order.)
        let ordered_input = !l.node.wanted_order.is_empty() || !r.node.wanted_order.is_empty();
        let (lgath, rgath) = (parallel::has_gather(&lg.node), parallel::has_gather(&rg.node));
        let nl_inner_gathered = |p: &ExNode| -> bool {
            if !p.title.ends_with("Nested Loop") && !p.title.contains("Nested Loop ") {
                return false;
            }
            let inner_key = p.children.get(1).and_then(|c| {
                if matches!(c.node_type, "Materialize" | "Memoize") {
                    c.children.first().and_then(|x| x.key)
                } else {
                    c.key
                }
            });
            if inner_key.is_some() && inner_key == lkey { lgath } else { rgath }
        };
        // (A Hash Right Semi Join has no partial form: the semi join the other way round
        // may have one.)
        let unswapped = (meta.swapped && kind == JoinKind::Semi && parts && !lateral)
            .then(|| (copy(&l), copy(&r)));
        // (The join's other partial paths: by hashing or a nested loop, either side
        // outside for an inner join, add_paths_to_joinrel's partial candidates.)
        let others_in = (unsorted_parts
            && !lateral
            && self.par.is_some()
            && meta.from_call.is_none()
            && matches!(
                kind,
                JoinKind::Inner | JoinKind::Left | JoinKind::Right | JoinKind::Semi | JoinKind::Anti
            ))
        .then_some((lu, ru));
        // (A nested loop keeps its outer side's order: that side's partial plan sorted in
        // the query's order under a Gather Merge, generate_useful_gather_paths at its
        // relation, makes a path in that order; either side outside for an inner join.)
        let ordered_outer = 'oo: {
            if !(matches!(meta.method, Some(super::plan::JoinMethod::NestLoop) | None)
                && !lateral
                && !meta.swapped
                && meta.memoize.is_none()
                && matches!(kind, JoinKind::Inner | JoinKind::Left)
                && self.par.as_ref().is_some_and(|c| c.gather_merge))
            {
                break 'oo None;
            }
            // (The keys: the order the node above wants, as far as this side computes them
            // early (relation_can_be_sorted_early: no volatile key), else the orderings
            // the planner found useful for its scan.)
            let hint = self.order_hint.as_ref().filter(|(a, _)| *a == at).map(|(_, k)| k.clone());
            let side = |this: &mut Self, p: &Arc<Plan>, b: &Built, off: usize| -> Option<(Vec<String>, Vec<Option<usize>>)> {
                let Plan::Scan(s) = &**p else { return None };
                b.node.partial.as_ref()?;
                let w = b.exported.len();
                let mut list = Vec::new();
                let mut cols = Vec::new();
                if let Some(hk) = &hint {
                    for k in hk {
                        let cs = k.expr.columns();
                        if cs.is_empty() || !cs.iter().all(|&c| c >= off && c < off + w) || k.expr.is_volatile() {
                            break;
                        }
                        let e = k.expr.clone().shift_cols(-(off as isize));
                        let t = match &e {
                            Expr::Col(j, _) => this.pick(b.exported.get(*j)?, Cx::Upper),
                            _ => {
                                let t = format!("({})", this.show(&e, &b.exported, Cx::Upper));
                                if name_derived_c(&e) { t + " COLLATE \"C\"" } else { t }
                            }
                        };
                        list.push(t + &sort_suffix(k));
                        cols.push(match &e {
                            Expr::Col(j, _) => Some(*j),
                            _ => None,
                        });
                    }
                } else {
                    let u = s.useful.0.as_ref()?;
                    for k in &u.0 {
                        let Expr::Col(j, _) = &k.expr else { break };
                        let name = b.exported.get(*j)?;
                        let sk = SortKey {
                            expr: k.expr.clone(),
                            desc: k.desc,
                            nulls_first: k.nulls_first,
                            using: None,
                        };
                        list.push(this.pick(name, Cx::Upper) + &sort_suffix(&sk));
                        cols.push(Some(*j));
                    }
                }
                (!list.is_empty()).then_some((list, cols))
            };
            let nl = l.exported.len();
            if let Some((list, cols)) = side(self, left, &l, 0) {
                break 'oo Some((false, copy(&l), copy(&r), list, cols));
            }
            if kind == JoinKind::Inner
                && let Some((list, cols)) = side(self, right, &r, nl)
            {
                break 'oo Some((true, copy(&l), copy(&r), list, cols));
            }
            None
        };
        let mut b1 = self.join_built(at, kind, left, right, l, r, cond, hash, params, meta);
        if let Some((sw, mut lo, mut ro, list, cols)) = ordered_outer {
            let outer = if sw { &mut ro } else { &mut lo };
            if let Some(pt) = outer.node.partial.take() {
                let others = std::mem::take(&mut outer.node.partials);
                let gm = self
                    .useful_gathers(&pt, &others, &list, &cols, Some(outer.node.rows), outer.node.key, true)
                    .into_iter()
                    .reduce(|a, b| if parallel::cheaper(&b, &a) { b } else { a });
                if let Some(gm) = gm {
                    outer.node = gm;
                    let mut m2 = meta.clone();
                    m2.method = None;
                    m2.material = false;
                    m2.indexed = false;
                    m2.scansel = None;
                    m2.unique_outer = false;
                    m2.unique_inner = false;
                    m2.hash_buckets = None;
                    let saved = std::mem::take(&mut self.pending);
                    self.join_force = Some((super::plan::JoinMethod::NestLoop, sw));
                    let mut bg = self.join_built(at, kind, left, right, lo, ro, cond, hash, params, &m2);
                    self.join_force = None;
                    self.pending = saved;
                    bg.node.partial = None;
                    if bg.node.subplans.is_empty() {
                        bg.node.subplans = b1.node.subplans.clone();
                    }
                    if !bg.node.order.is_empty() && b1.node.alt.is_none() {
                        b1.node.alt = Some(Box::new(bg.node));
                    }
                }
            }
        }
        if let Some((l3, r3)) = unswapped
            && b1.node.partial.is_none()
        {
            let mut m3 = meta.clone();
            m3.swapped = false;
            let saved = std::mem::take(&mut self.pending);
            let b3 = self.join_built(at, kind, left, right, l3, r3, cond, hash, params, &m3);
            self.pending = saved;
            b1.node.partial = b3.node.partial;
        }
        if let Some((la, ra)) = others_in {
            use super::plan::JoinMethod;
            let mut m2 = meta.clone();
            m2.method = None;
            m2.material = false;
            m2.memoize = None;
            m2.indexed = false;
            m2.scansel = None;
            m2.swapped = false;
            m2.unique_outer = false;
            m2.unique_inner = false;
            m2.hash_buckets = None;
            // (In the order Postgres builds them, a tie keeping the first: the side earlier in
            // the FROM list outside first (make_join_rel's rel1), nested loops before
            // hashing; the serial join's own partial path with its sides as they are. An
            // outer join as a LEFT join first, then the other way round as a RIGHT one
            // (populate_joinrel_with_paths): a RIGHT join's sides swapped first.)
            let first_sw = match (min_rti(left), min_rti(right)) {
                _ if kind == JoinKind::Right => true,
                (Some(a), Some(b)) => b < a && kind == JoinKind::Inner,
                _ => false,
            };
            let own = b1.node.partial.take().filter(|p| !nl_inner_gathered(p));
            let mut own = Some(own);
            let mut best: Option<Box<ExNode>> = None;
            let consider = |p: Option<Box<ExNode>>, best: &mut Option<Box<ExNode>>| {
                if let Some(p) = p
                    && !nl_inner_gathered(&p)
                    && best.as_ref().is_none_or(|x| parallel::partial_better(&p, x))
                {
                    *best = Some(p);
                }
            };
            for sw in [first_sw, !first_sw] {
                if !sw {
                    consider(own.take().flatten(), &mut best);
                }
                for m in [JoinMethod::NestLoop, JoinMethod::Hash] {
                    // (An outer join either way round by hashing (as a RIGHT join only with a
                    // Parallel Hash), by a nested loop only as a LEFT join.)
                    let turnable = match kind {
                        JoinKind::Inner | JoinKind::Right => true,
                        JoinKind::Left => m == JoinMethod::Hash,
                        _ => false,
                    };
                    let nl_right = m == JoinMethod::NestLoop && kind == JoinKind::Right && !sw;
                    if (m == JoinMethod::Hash && hash.is_none()) || (sw && !turnable) || nl_right {
                        continue;
                    }
                    self.join_force = Some((m, sw));
                    let saved = std::mem::take(&mut self.pending);
                    let b = self.join_built(at, kind, left, right, copy(&la), copy(&ra), cond, hash, params, &m2);
                    self.pending = saved;
                    self.join_force = None;
                    consider(b.node.partial, &mut best);
                }
            }
            b1.node.partial = best;
        }
        // Or over its inputs gathered from workers (their Gather paths, when cheaper).
        if parts && (!lateral || lateral_gathered) {
            if b1.node.partial.as_deref().is_some_and(nl_inner_gathered) {
                b1.node.partial = None;
            }
            if lgath || rgath {
                let mut b2 = self.join_built(at, kind, left, right, lg, rg, cond, hash, params, meta);
                if b2.node.subplans.is_empty() {
                    b2.node.subplans = b1.node.subplans.clone();
                }
                // (A side whose Gather beat its serial path at its relation leaves the
                // join only that path to read, unless the serial join is clearly cheaper:
                // add_path dropped the serial input. A merge join's ordered inputs stay.)
                let (d1, d2) = (parallel::disabled_nodes(&b1.node), parallel::disabled_nodes(&b2.node));
                let b1_clearly = d1 < d2 || (d1 == d2 && b2.node.total_cost > b1.node.total_cost * 1.01);
                if parallel::par_beats(&b2.node, &b1.node)
                    || (b1.node.node_type != "Merge Join" && !ordered_input && !b1_clearly)
                {
                    b2.node.partial = b1.node.partial;
                    if b2.node.alt.is_none() {
                        b2.node.alt = b1.node.alt;
                    }
                    return b2;
                }
            }
        }
        b1
    }

    /// A join over inputs `l` and `r` already converted (`join`).
    #[allow(clippy::too_many_arguments)]
    fn join_built(
        &mut self,
        at: usize,
        kind: JoinKind,
        left: &Arc<Plan>,
        right: &Arc<Plan>,
        mut l: Built,
        mut r: Built,
        cond: Option<&Expr>,
        hash: Option<&(Vec<Expr>, Vec<Expr>)>,
        params: &[Expr],
        meta: &super::plan::JoinMeta,
    ) -> Built {
        use super::joins::{self, In, InKind, Jt, QualCost};
        use super::plan::JoinMethod;
        let lateral = !params.is_empty();
        // The inputs' partial plans: a partial join runs its outer side's in each worker.
        let lpart = l.node.partial.take();
        let rpart = r.node.partial.take();
        let mut pcost: Option<joins::Cost> = None;
        let mut parallel_hash = false;
        let cat = self.ctx.catalog();
        let g = joins::JoinGucs::read(self.ctx);
        let method = meta.method.unwrap_or(if hash.is_some() && !lateral {
            JoinMethod::Hash
        } else {
            JoinMethod::NestLoop
        });
        // (A full join is never a nested loop: with no merge clauses, as ON false, a merge
        // join with none.)
        let method = if kind == JoinKind::Full && method == JoinMethod::NestLoop {
            JoinMethod::Merge
        } else {
            method
        };
        // (Building another partial path of the join: its method and orientation.)
        let force = self.join_force.take();
        let method = force.map_or(method, |f| f.0);
        let pad = |cols: &[ColInfo], n: usize, w: u32| -> Vec<ColInfo> {
            if cols.len() == n {
                cols.to_vec()
            } else {
                vec![
                    ColInfo {
                        width: w / n.max(1) as u32,
                        var: None,
                        distinct: None,
                    };
                    n
                ]
            }
        };
        let lcols = pad(&l.node.cols, l.exported.len(), l.node.width);
        let rcols = pad(&r.node.cols, r.exported.len(), r.node.width);
        let mut cols = lcols.clone();
        cols.extend(rcols.iter().cloned());
        let mut all = l.exported.clone();
        all.extend(r.exported.iter().cloned());
        // The equality pairs and the other quals.
        let jv = |e: &Expr, cs: &[ColInfo], rows: f64| -> joins::JoinVar {
            let c = match strip_relabel(strip_widening(e)) {
                Expr::Col(i, _) => cs.get(*i).cloned(),
                _ => None,
            };
            // (An expression of one table's columns without statistics of its own:
            // get_variable_numdistinct takes the table's tuples when fewer than the default,
            // as the join search's `join_var` does.)
            let expr_distinct = || -> Option<f64> {
                let vars: Vec<costs::VarRef> = e.columns().iter().map(|&k| cs.get(k).and_then(|c| c.var.clone())).collect::<Option<_>>()?;
                let first = vars.first()?;
                vars.iter()
                    .all(|v| v.table.oid == first.table.oid)
                    .then_some(first.tuples)
                    .filter(|t| *t > 0.0 && *t < costs::DEFAULT_NUM_DISTINCT)
                    .map(costs::clamp_rows)
            };
            joins::JoinVar {
                var: c.as_ref().and_then(|c| c.var.clone()),
                distinct: match &c {
                    Some(c) => c.distinct,
                    None => expr_distinct(),
                },
                rel_rows: c
                    .as_ref()
                    .and_then(|c| c.var.as_ref())
                    .map_or(rows, |v| if v.rows > 0.0 { v.rows } else { rows }),
            }
        };
        let (lrows, rrows) = (l.node.rows, r.node.rows);
        let keys: Vec<(Expr, Expr)> = hash
            .map(|(lk, rk)| lk.iter().cloned().zip(rk.iter().cloned()).collect())
            .unwrap_or_default();
        let pairs: Vec<(joins::JoinVar, joins::JoinVar)> = keys
            .iter()
            .map(|(a, b)| (jv(a, &lcols, lrows), jv(b, &rcols, rrows)))
            .collect();
        let mut others = Vec::new();
        if let Some(c) = cond {
            conjuncts(c, &mut others);
        }
        let vars = |i: usize| cols.get(i).and_then(|c| c.var.clone());
        let est = costs::Est {
            cat: &cat,
            fmt: self.ctx.fmt(),
            vars: &vars,
            exprs: &[],
            endpoint: None,
            distinct: None,
            user: None,
            same_rel: false,
        };
        // (A pseudoconstant qual gates the join: 1.0, clause_selectivity_ext.)
        let filtering: Vec<Expr> = others.iter().filter(|q| !super::binder::optimize::pseudo_gate(q)).cloned().collect();
        let other_sel = est.clauses(&filtering);
        let fmt = self.ctx.fmt();
        let inner_sel: f64 = pairs
            .iter()
            .map(|(a, b)| joins::eqjoinsel_inner(&cat, fmt, a, b))
            .product();
        let semi_sel = |pairs: &[(joins::JoinVar, joins::JoinVar)], inner_rows: f64| -> f64 {
            pairs
                .iter()
                .map(|(a, b)| {
                    let s = joins::eqjoinsel_semi(&cat, fmt, a, b, inner_rows);
                    s.min(inner_rows * joins::eqjoinsel_inner(&cat, fmt, a, b))
                        .clamp(0.0, 1.0)
                })
                .product()
        };
        let jt = match kind {
            JoinKind::Inner => Jt::Inner,
            JoinKind::Left => Jt::Left,
            JoinKind::Right => Jt::Right,
            JoinKind::Full => Jt::Full,
            JoinKind::Semi => Jt::Semi,
            JoinKind::Anti => Jt::Anti,
        };
        let jsel = match jt {
            Jt::Semi | Jt::Anti => semi_sel(&pairs, rrows) * other_sel,
            _ => inner_sel * other_sel,
        };
        // A semi / anti join run the other way round: its right input is the outer one.
        let right_variant = meta.swapped && matches!(jt, Jt::Semi | Jt::Anti);
        let cjt = match (right_variant, jt) {
            (true, Jt::Semi) => Jt::RightSemi,
            (true, Jt::Anti) => Jt::RightAnti,
            _ => jt,
        };
        let mut n = ExNode::new(match method {
            JoinMethod::Hash => "Hash Join",
            JoinMethod::Merge => "Merge Join",
            JoinMethod::NestLoop => "Nested Loop",
        });
        let moved = (lateral && matches!(jt, Jt::Inner | Jt::Left))
            .then(|| lateral_moved(right, params, &lcols, self.ctx))
            .flatten();
        n.rows = meta.rows.unwrap_or_else(|| match moved {
            // (The join relation's size from the LATERAL item's own rows and the join
            // clauses its parameterized path checks: set_joinrel_size_estimates.)
            Some((rel_rows, sel)) => joins::join_rows(jt, lrows, rel_rows, jsel * sel, 1.0),
            None => joins::join_rows(jt, lrows, rrows, jsel, 1.0),
        });
        let planned = meta.method.is_some();
        n.cols = if matches!(kind, JoinKind::Semi | JoinKind::Anti) {
            lcols.clone()
        } else {
            cols.clone()
        };
        let mask = self.needed.get(&at).cloned();
        n.width = n
            .cols
            .iter()
            .enumerate()
            .filter(|(i, _)| mask.as_ref().is_none_or(|m| m.get(*i).copied().unwrap_or(false)))
            .map(|(_, c)| c.width)
            .sum::<u32>()
            + 6 * (rowmarks_under(left, &self.rowmarks) + rowmarks_under(right, &self.rowmarks))
                as u32
            + copy_widths_under(left, &self.copy_marks)
            + copy_widths_under(right, &self.copy_marks)
            + if matches!(kind, JoinKind::Semi) { meta.semi_carried } else { 0 };
        // The quals' evaluation costs.
        let key_cost: f64 = keys
            .iter()
            .map(|(a, b)| {
                costs::CPU_OPERATOR_COST
                    + costs::qual_cost(strip_widening(a))
                    + costs::qual_cost(strip_widening(b))
            })
            .sum();
        let other_cost: f64 = others.iter().map(costs::qual_cost).sum();
        // (cost_qual_eval: a SubPlan in them costs its runs too, cost_subplan.)
        let key_exprs: Vec<Expr> = keys.iter().flat_map(|(a, b)| [a.clone(), b.clone()]).collect();
        let (key_sub_startup, key_sub) = subplan_costs(&key_exprs, self.ctx);
        let (other_sub_startup, other_sub) = subplan_costs(&others, self.ctx);
        let (key_cost, other_cost) = (key_cost + key_sub, other_cost + other_sub);
        let input = |x: &ExNode| In {
            rows: x.rows,
            startup: x.startup_cost,
            total: x.total_cost,
            width: x.width,
            disabled: 0,
            kind: match x.node_type {
                "Function Scan" => InKind::FunctionScan,
                "CTE Scan" => InKind::CteScan,
                "WorkTable Scan" => InKind::WorkTableScan,
                "Materialize" => InKind::Material,
                "Sort" => InKind::Sort,
                "Nested Loop" | "Merge Join" => InKind::Join,
                // (A parameterized hash join under a nested loop: a single batch keeps its
                // table for a rescan, cost_rescan.)
                "Hash Join" => InKind::HashJoin(
                    x.children
                        .iter()
                        .find(|c| c.node_type == "Hash")
                        .map_or(1, |h| joins::hash_table_size(h.rows, h.width, &g).1),
                ),
                "Seq Scan" | "Bitmap Heap Scan" | "Values Scan" | "Subquery Scan" | "Append" => {
                    InKind::NoMark
                }
                _ => InKind::Plain,
            },
        };
        // The semi/anti factors (compute_semi_anti_join_factors), for an orientation.
        let factors = |outer_rows: f64, inner_rows: f64, swapped: bool| -> joins::Semi {
            if let Some((f, c)) = meta.semi {
                return joins::Semi {
                    outer_match_frac: f,
                    match_count: c,
                };
            }
            let ps: Vec<(joins::JoinVar, joins::JoinVar)> = if swapped {
                pairs.iter().map(|(a, b)| (b.clone(), a.clone())).collect()
            } else {
                pairs.clone()
            };
            // (eqjoinsel answers for the join's own type: an inner join's selectivity
            // stands in for an inner-unique one's.)
            let j = if matches!(jt, Jt::Semi | Jt::Anti) {
                semi_sel(&ps, inner_rows) * other_sel
            } else {
                inner_sel * other_sel
            };
            let nsel = inner_sel * other_sel;
            let _ = outer_rows;
            let avg = if j > 0.0 {
                (nsel * inner_rows / j).max(1.0)
            } else {
                1.0
            };
            joins::Semi {
                outer_match_frac: j,
                match_count: avg,
            }
        };
        // A side's rows are unique on its join keys.
        let unique_on = |p: &Arc<Plan>, ks: Vec<&Expr>| unique_for(&cat, p, &ks);
        let lunique = unique_on(left, keys.iter().map(|k| &k.0).collect());
        let runique = if planned {
            meta.inner_unique
        } else {
            unique_on(right, keys.iter().map(|k| &k.1).collect())
        };
        let mut swapped = false;
        let mut nl_outer: Option<ExNode> = None;
        let mut inner_unique;
        let mut material = false;
        let mut cost;
        match method {
            JoinMethod::Hash => {
                let hash_cost = |outer: &ExNode, inner: &ExNode, sw: bool, uniq: bool| {
                    // (A Parallel Hash, over the inner side's partial plan: the shared
                    // table sized for all the participants' rows, initial_cost_hashjoin.)
                    let parallel = (inner.workers > 0 && !parallel::has_gather(inner))
                        .then(|| self.par.as_ref().map(|c| (inner.rows * c.divisor(inner.workers), outer.workers)))
                        .flatten();
                    let (nb, nbatch) = match parallel {
                        Some((rows, w)) => joins::parallel_hash_table_size(rows, inner.width, &g, w),
                        None => joins::hash_table_size(inner.rows, inner.width, &g),
                    };
                    let vb = nb as f64 * nbatch as f64;
                    let mut bucket = (1.0_f64, 1.0_f64);
                    for (a, b) in &pairs {
                        let iv = if sw { a } else { b };
                        let (mcv, bs) =
                            joins::bucket_stats(&cat, iv.var.as_ref(), iv.rel_rows, vb);
                        let bs = match (&iv.var, iv.distinct.filter(|d| *d >= 1.0)) {
                            // (A unique column (a single grouping key): as many values as
                            // rows.)
                            (Some(_), Some(d)) => if d > vb { 1.0 / vb } else { 1.0 / d },
                            (None, _) => 0.1_f64.max(1.0 / vb),
                            _ => bs,
                        };
                        let mcv = if iv.var.is_some() && iv.distinct.is_some() { 0.0 } else { mcv };
                        bucket.0 = bucket.0.min(bs);
                        bucket.1 = bucket.1.min(mcv);
                    }
                    let info = joins::HashInfo {
                        nclauses: pairs.len(),
                        hash_quals: QualCost {
                            startup: key_sub_startup,
                            per_tuple: key_cost,
                        },
                        other_quals: QualCost {
                            startup: other_sub_startup,
                            per_tuple: other_cost,
                        },
                        bucket: (!meta.unique_inner).then_some(bucket),
                        hash_tuples: costs::clamp_rows(inner_sel * outer.rows * inner.rows),
                        parallel,
                    };
                    let sf = (matches!(jt, Jt::Semi | Jt::Anti) || uniq)
                        .then(|| factors(outer.rows, inner.rows, sw));
                    joins::hashjoin(cjt, &input(outer), &input(inner), uniq, sf, &info, &g).0
                };
                inner_unique = runique;
                if right_variant {
                    cost = hash_cost(&r.node, &l.node, true, runique);
                    swapped = true;
                } else {
                    cost = hash_cost(&l.node, &r.node, false, runique);
                }
                if kind == JoinKind::Inner && meta.method.is_none() {
                    let c2 = hash_cost(&r.node, &l.node, true, lunique);
                    if fuzzy_better(&c2, &cost) {
                        cost = c2;
                        swapped = true;
                        inner_unique = lunique;
                    }
                }
                // (A partial path of another orientation, `join_force`: an outer join's the
                // other way round.)
                if let Some((_, sw)) = force
                    && matches!(kind, JoinKind::Inner | JoinKind::Left | JoinKind::Right)
                    && sw != swapped
                {
                    swapped = sw;
                    (cost, inner_unique) = if sw {
                        (hash_cost(&r.node, &l.node, true, lunique), lunique)
                    } else {
                        (hash_cost(&l.node, &r.node, false, runique), runique)
                    };
                }
                let (po, inn, pin) = if swapped {
                    (&rpart, &l.node, &lpart)
                } else {
                    (&lpart, &r.node, &rpart)
                };
                // (hash_inner_and_outer: no partial right semi join; a full, right or
                // right anti one only with a Parallel Hash, no one worker having all the
                // match bits otherwise.)
                let jt_now = match (swapped, cjt) {
                    (true, Jt::Semi) => Jt::RightSemi,
                    (true, Jt::Anti) => Jt::RightAnti,
                    (true, Jt::Left) => Jt::Right,
                    (true, Jt::Right) => Jt::Left,
                    (_, j) => j,
                };
                if let Some(po) = po.as_ref().filter(|_| jt_now != Jt::RightSemi) {
                    if !matches!(jt_now, Jt::Full | Jt::Right | Jt::RightAnti) {
                        pcost = Some(hash_cost(po, inn, swapped, inner_unique));
                    }
                    // Or with a Parallel Hash built by all the workers from the inner
                    // side's partial plan.
                    if let Some(pi) = pin
                        && self.par.as_ref().is_some_and(|c| c.hash)
                    {
                        let c2 = hash_cost(po, pi, swapped, inner_unique);
                        if pcost.as_ref().is_none_or(|c| c2.total < c.total) {
                            pcost = Some(c2);
                            parallel_hash = true;
                        }
                    }
                }
            }
            JoinMethod::Merge => {
                let msel: f64 = inner_sel;
                let info = joins::MergeInfo {
                    merge_quals: QualCost {
                        startup: key_sub_startup,
                        per_tuple: key_cost,
                    },
                    other_quals: QualCost {
                        startup: other_sub_startup,
                        per_tuple: other_cost,
                    },
                    scansel: meta.scansel.unwrap_or((0.0, 1.0, 0.0, 1.0)),
                    merge_tuples: costs::clamp_rows(msel * lrows * rrows),
                    all_merge: others.is_empty(),
                    outer_sort: None,
                    inner_sort: false,
                    outer_unique: meta.unique_outer,
                    // (Not planned as a merge join: decided here.)
                    material: (meta.method == Some(JoinMethod::Merge)).then_some(meta.material),
                };
                inner_unique = runique;
                let (o, i) = if right_variant {
                    swapped = true;
                    (&r.node, &l.node)
                } else {
                    (&l.node, &r.node)
                };
                let (c, m, _, _) = joins::mergejoin(cjt, &input(o), &input(i), runique, &info, &g);
                cost = c;
                material = m;
                let po = if right_variant { &rpart } else { &lpart };
                if let Some(po) = po {
                    let mut info = info;
                    info.merge_tuples = costs::clamp_rows(msel * po.rows * i.rows);
                    info.material = Some(m);
                    pcost = Some(joins::mergejoin(cjt, &input(po), &input(i), runique, &info, &g).0);
                }
            }
            JoinMethod::NestLoop => {
                let quals = QualCost {
                    startup: key_sub_startup + other_sub_startup,
                    per_tuple: key_cost + other_cost,
                };
                let nl_cost = |outer: &ExNode, inner: &ExNode, sw: bool, uniq: bool, mat: bool| {
                    let mut i = input(inner);
                    if meta.indexed {
                        i.kind = InKind::IndexedJoin;
                    }
                    if let Some(m) = &meta.memoize {
                        i.startup += costs::CPU_TUPLE_COST;
                        i.total += costs::CPU_TUPLE_COST;
                        i.kind = InKind::Memoize(m.rescan.0, m.rescan.1);
                    }
                    if mat {
                        i = joins::material(&i, &g);
                    }
                    let sf = (matches!(jt, Jt::Semi | Jt::Anti) || uniq)
                        .then(|| factors(outer.rows, inner.rows, sw));
                    (
                        joins::nestloop(jt, &input(outer), &i, uniq, sf, n.rows, quals, &g),
                        i,
                    )
                };
                // The inner side materialized when that comes out cheaper (unless it keeps
                // its rows anyway); an inner join puts either side outside.
                let best = |outer: &ExNode, inner: &ExNode, sw: bool, uniq: bool| {
                    let plain = nl_cost(outer, inner, sw, uniq, false);
                    if meta.method.is_some() {
                        if meta.material {
                            return (nl_cost(outer, inner, sw, uniq, true).0, true);
                        }
                        return (plain.0, false);
                    }
                    if lateral || !g.material || input(inner).kind.materializes() {
                        return (plain.0, false);
                    }
                    let m = nl_cost(outer, inner, sw, uniq, true);
                    if fuzzy_better(&m.0, &plain.0) || (m.0.total < plain.0.total && !fuzzy_better(&plain.0, &m.0)) {
                        (m.0, true)
                    } else {
                        (plain.0, false)
                    }
                };
                inner_unique = runique;
                let (c1, m1) = best(&l.node, &r.node, false, runique);
                cost = c1;
                material = m1;
                if kind == JoinKind::Inner && !lateral && meta.method.is_none() {
                    let (c2, m2) = best(&r.node, &l.node, true, lunique);
                    if fuzzy_better(&c2, &cost) {
                        cost = c2;
                        material = m2;
                        swapped = true;
                        inner_unique = lunique;
                    }
                }
                // (A partial path of another orientation, `join_force`: a RIGHT join's as a
                // LEFT one.)
                if let Some((_, sw)) = force
                    && (kind == JoinKind::Inner || (kind == JoinKind::Right && sw))
                    && !lateral
                    && sw != swapped
                {
                    swapped = sw;
                    let ((c, m), u) = if sw {
                        (best(&r.node, &l.node, true, lunique), lunique)
                    } else {
                        (best(&l.node, &r.node, false, runique), runique)
                    };
                    (cost, material, inner_unique) = (c, m, u);
                }
                let (po, inn) = if swapped { (&rpart, &l.node) } else { (&lpart, &r.node) };
                // (A parameterized inner side too, when a worker can run it: the outer row
                // is the worker's own, consider_parallel_nestloop.)
                if let Some(po) = po
                    && (!lateral || parallel::safe_tree(inn))
                {
                    // (try_partial_nestloop_path: the inner side as it is, or materialized.)
                    let plain = nl_cost(po, inn, swapped, inner_unique, false).0;
                    let mat = (force.is_some() && g.material && !input(inn).kind.materializes())
                        .then(|| nl_cost(po, inn, swapped, inner_unique, true).0);
                    pcost = Some(match mat {
                        Some(m) if m.total < plain.total => {
                            material = true;
                            m
                        }
                        Some(_) => {
                            material = false;
                            plain
                        }
                        None => nl_cost(po, inn, swapped, inner_unique, material).0,
                    });
                    // (consider_parallel_nestloop: every partial path of the outer side;
                    // fuzzily as cheap, one in order is kept, add_partial_path.)
                    let others = if swapped { &r.node.partials } else { &l.node.partials };
                    for op in others.iter().filter(|o| !o.order.is_empty()) {
                        let c = nl_cost(op, inn, swapped, inner_unique, material).0;
                        if pcost.as_ref().is_some_and(|p| c.total <= p.total * 1.01) {
                            pcost = Some(c);
                            nl_outer = Some(op.clone());
                        }
                    }
                }
            }
        }
        if swapped {
            std::mem::swap(&mut l, &mut r);
        }
        n.startup_cost = cost.startup;
        n.total_cost = cost.total;
        n.qual_tuples = match (method, jt) {
            (_, Jt::Semi | Jt::Anti) => None,
            (JoinMethod::NestLoop, _) => Some(lrows * rrows),
            _ => Some(costs::clamp_rows(inner_sel * lrows * rrows)),
        };
        // (initial_cost_nestloop / _mergejoin / _hashjoin count a disabled method itself,
        // FULL JOIN or not.)
        n.disabled = match method {
            JoinMethod::NestLoop => !g.nestloop,
            JoinMethod::Merge => !g.mergejoin,
            JoinMethod::Hash => !g.hashjoin,
        };
        // (An outer join turned round: LEFT for RIGHT and back.)
        let shown_kind = match (swapped, kind) {
            (true, JoinKind::Left) => JoinKind::Right,
            (true, JoinKind::Right) => JoinKind::Left,
            (_, k) => k,
        };
        let jname = if right_variant {
            format!("Right {}", join_type(kind))
        } else {
            join_type(shown_kind).to_string()
        };
        n.title = match (method, shown_kind) {
            (JoinMethod::Hash, _) if right_variant => format!("Hash {jname} Join"),
            (JoinMethod::Merge, _) if right_variant => format!("Merge {jname} Join"),
            (JoinMethod::Hash, JoinKind::Inner) => "Hash Join".into(),
            (JoinMethod::Hash, k) => format!("Hash {} Join", join_type(k)),
            (JoinMethod::Merge, JoinKind::Inner) => "Merge Join".into(),
            (JoinMethod::Merge, k) => format!("Merge {} Join", join_type(k)),
            (JoinMethod::NestLoop, JoinKind::Inner) => "Nested Loop".into(),
            (JoinMethod::NestLoop, k) => format!("Nested Loop {} Join", join_type(k)),
        };
        n.target.push(("Join Type", jname, false));
        n.target.push((
            "Inner Unique",
            if inner_unique { "true" } else { "false" }.into(),
            true,
        ));
        // Conditions, written outer side first.
        // (set_join_references: the join's clauses fixed for NUM_EXEC_QUAL.)
        let saved_num_exec = self.num_exec.replace(n.rows * 2.0);
        let mut keys_text = Vec::new();
        let mut merge_names: Vec<(String, String)> = Vec::new();
        // An inner join's equality comes from its equivalence class: operands whose
        // collation isn't the class's (none, when the sides' implicit collations conflict)
        // are relabelled (canonicalize_ec_expression), which shows as a cast.
        // (A key column's own collation, from its relation's column.)
        let own_coll = |e: &Expr, cols: &[ColInfo]| -> Option<String> {
            let Expr::Col(j, _) = e else { return None };
            cols.get(*j)
                .and_then(|c| c.var.as_ref())
                .filter(|v| v.expr.is_none())
                .and_then(|v| v.table.columns.get(v.col))
                .and_then(|c| c.collation.clone())
        };
        let relabel = |k: &Expr, t: String, cols: &[ColInfo]| match k {
            Expr::Call(d, a, _)
                if kind == JoinKind::Inner && d.name == crate::sql::binder::INDETERMINATE_HASH =>
            {
                format!("({t})::{}", a[0].ty().typname())
            }
            // (Under a nondeterministic collation: an explicit COLLATE is a relabelling,
            // and an inner or semi join's clause comes from its equivalence class, a member
            // not of the class's collation relabelled.)
            Expr::Call(d, a, _)
                if d.name == crate::sql::binder::NONDETERMINISTIC_HASH
                    && (matches!(a[0], Expr::Collate(..))
                        || (matches!(kind, JoinKind::Inner | JoinKind::Semi | JoinKind::Anti)
                            && own_coll(&a[0], cols).as_deref() != d.coll.as_ref().map(|c| c.name.as_str()))) =>
            {
                let t = t.strip_suffix(&format!(" COLLATE {}", d.coll.as_ref().map_or("", |c| c.name.as_str()))).unwrap_or(&t).to_string();
                format!("({t})::{}", a[0].ty().typname())
            }
            _ => t,
        };
        let mut hash_subplans: Vec<ExNode> = Vec::new();
        for (a, b) in &keys {
            let (outer_names, inner_names) = (&l.exported, &r.exported);
            let (oa, ib) = if swapped { (b, a) } else { (a, b) };
            // (An operator's argument shows its implicit relabelling, as varchar to text.)
            let (outer_cols, inner_cols) = (&l.node.cols, &r.node.cols);
            let side = |this: &mut Self, k: &Expr, names: &[Name], cols: &[ColInfo]| match strip_widening(k) {
                Expr::Cast(inner, from, to, false) if from != to && !from.same_base(to) => {
                    format!("({})::{}", this.show(inner, names, Cx::Upper), to.typname())
                }
                k2 => relabel(k, this.show(k2, names, Cx::Upper), cols),
            };
            // (A merge key computed by the Sort's input: a reference to that value.)
            let sorted_expr = |k: &Expr, child: &ExNode| {
                method == JoinMethod::Merge
                    && child.node_type == "Sort"
                    && !matches!(strip_widening(k), Expr::Col(..) | Expr::Const(..))
                    && !matches!(strip_widening(k), Expr::Cast(x, ..) if matches!(**x, Expr::Col(..)))
            };
            // (A written COLLATE over a column of another collation is a RelabelType, and so
            // is, in an inner join's clause from its equivalence class, the other side's
            // column when its collation isn't the class's: `(c)::text`.)
            let explicit = |k: &Expr| match strip_widening(k) {
                Expr::Collate(x, coll, loc) if *loc >= 0 && matches!(**x, Expr::Col(..)) => Some(((**x).clone(), coll.clone())),
                _ => None,
            };
            let pair_coll = explicit(oa).or_else(|| explicit(ib)).map(|x| x.1);
            let relabeled = |this: &mut Self, k: &Expr, names: &[Name], cols: &[ColInfo]| -> Option<String> {
                let (x, coll) = match explicit(k) {
                    Some(x) => x,
                    None if matches!(kind, JoinKind::Inner) && matches!(strip_widening(k), Expr::Col(..)) => {
                        (strip_widening(k).clone(), pair_coll.clone()?)
                    }
                    None => return None,
                };
                if own_coll(&x, cols).unwrap_or_else(|| "default".into()) == coll {
                    return None;
                }
                Some(format!("({})::{}", this.show(&x, names, Cx::Upper), x.ty().typname()))
            };
            let a = match relabeled(self, oa, outer_names, outer_cols) {
                Some(t) => t,
                None => side(self, oa, outer_names, outer_cols),
            };
            let a = if sorted_expr(oa, &l.node) { format!("({a})") } else { a };
            let before = self.pending.len();
            let b = match relabeled(self, ib, inner_names, inner_cols) {
                Some(t) => t,
                None => side(self, ib, inner_names, inner_cols),
            };
            let b = if sorted_expr(ib, &r.node) { format!("({b})") } else { b };
            // (An inner or semi join's `name = text` key pair comes out of its equivalence
            // class, whose collation is the name side's C: canonicalize_ec_expression relabels
            // the text column to it, a cast (`name_text_relabel`); an outer join's keeps its
            // form.)
            let (a, b) = {
                let (ka, kb) = (crate::sql::binder::hash_key_operand(oa).0, crate::sql::binder::hash_key_operand(ib).0);
                let text_col = |k: &Expr| matches!(k, Expr::Col(..)) && matches!(k.ty(), DataType::Text);
                let name = |k: &Expr| matches!(k.ty(), DataType::Name);
                if !matches!(kind, JoinKind::Inner | JoinKind::Semi) || OUTER_JOIN_SIDE.with(|o| o.get()) {
                    (a, b)
                } else if name(&ka) && text_col(&kb) {
                    (a, format!("({b})::text"))
                } else if name(&kb) && text_col(&ka) {
                    (format!("({a})::text"), b)
                } else {
                    (a, b)
                }
            };
            // (The inner keys are the Hash's to compute: their SubPlans are its.)
            if method == JoinMethod::Hash && self.pending.len() > before {
                hash_subplans.extend(self.pending.drain(before..));
            }
            merge_names.push((a.clone(), b.clone()));
            // (The built-in `=`, qualified when a user `=` of the same types comes first on
            // the search path: generate_operator_name.)
            let eq = {
                let (ta, tb) = (
                    crate::sql::binder::hash_key_operand(oa).0.ty(),
                    crate::sql::binder::hash_key_operand(ib).0.ty(),
                );
                let cat = self.ctx.catalog();
                let path = cat.routine_path(&self.ctx.search_path);
                let pc = path.iter().position(|&s| s == crate::catalog::PG_CATALOG_OID);
                let hidden = pc.is_some_and(|pc| {
                    cat.operators.values().any(|o| {
                        o.name == "="
                            && path.iter().position(|&s| s == o.schema).is_some_and(|i| i < pc)
                            && o.left.as_ref().is_some_and(|t| t.same_base(&ta))
                            && o.right.same_base(&tb)
                    })
                });
                if hidden { "OPERATOR(pg_catalog.=)" } else { "=" }
            };
            keys_text.push(format!("({a} {eq} {b})"));
        }
        let mut join_filter = Vec::new();
        match method {
            JoinMethod::Hash => n
                .details
                .push(Detail::Text("Hash Cond", and_text(keys_text).unwrap_or_default())),
            // (A merge join with no merge clauses, as FULL JOIN ON false: none shown.)
            JoinMethod::Merge => {
                if let Some(t) = and_text(keys_text) {
                    n.details.push(Detail::Text("Merge Cond", t));
                }
                // A CTE or WorkTable Scan sorted for the merge returns just the columns used
                // above, then the merge keys' (CP_SMALL_TLIST, in reltarget order).
                if self.o.verbose {
                    let need = self.needed.get(&at).cloned();
                    let lw = l.exported.len();
                    for (side, off) in [(&mut l, 0usize), (&mut r, lw)] {
                        if side.node.node_type != "Sort"
                            || side.node.children.len() != 1
                            || !matches!(side.node.children[0].node_type, "CTE Scan" | "WorkTable Scan")
                        {
                            continue;
                        }
                        let width = side.internal.len();
                        let mut order: Vec<usize> = Vec::new();
                        if let Some(need) = &need {
                            for i in 0..width {
                                if need.get(off + i).copied().unwrap_or(true) && !order.contains(&i) {
                                    order.push(i);
                                }
                            }
                        }
                        for (a, b) in &keys {
                            let k = if off == 0 { a } else { b };
                            for c in k.columns() {
                                if c < width && !order.contains(&c) {
                                    order.push(c);
                                }
                            }
                        }
                        if order.len() < width {
                            let out: Vec<String> = order.iter().map(|&i| self.pick(&side.internal[i], Cx::Output)).collect();
                            side.node.output = out.clone();
                            side.node.children[0].output = out;
                        }
                    }
                }
            }
            JoinMethod::NestLoop => join_filter = keys_text,
        }
        // An inner join's pseudoconstant quals (no column of this query level) gate it
        // (create_gating_plan): a Result with a One-Time Filter above.
        let mut gating = Vec::new();
        for p in &others {
            // (An InitPlan's output is no volatile function: contain_volatile_functions
            // doesn't look into a SubPlan.)
            if kind == JoinKind::Inner
                && p.columns().is_empty()
                && crate::sql::binder::optimize::pseudo_constant(p)
                && !matches!(p, Expr::Const(..))
            {
                gating.push(p.clone());
                continue;
            }
            let p = if kind == JoinKind::Inner {
                ec_relabel(p)
            } else {
                p.clone()
            };
            join_filter.push(self.show(&p, &all, Cx::Upper));
        }
        self.num_exec = saved_num_exec;
        if let Some(t) = and_text(join_filter) {
            // (Under ANALYZE, the pairs a nested loop's filter rejected.)
            let removed = (method == JoinMethod::NestLoop).then_some(Removed::Scan(at));
            n.add_qual("Join Filter", "Rows Removed by Join Filter", t, removed);
        }
        if material {
            let mut m = joins::material(&input(&r.node), &g);
            // (A merge join's own Materialize is labelled as its input plus
            // cpu_operator_cost a row, create_mergejoin_plan.)
            if method == JoinMethod::Merge {
                m.startup = r.node.startup_cost;
                m.total = r.node.total_cost + costs::CPU_OPERATOR_COST * r.node.rows;
            }
            let mut mn = ExNode {
                key: r.node.key,
                rows: r.node.rows,
                width: r.node.width,
                cols: r.node.cols.clone(),
                startup_cost: m.startup,
                total_cost: m.total,
                ..ExNode::new("Materialize")
            };
            // (Its target list refers to its input's: a value the input computes shown
            // parenthesized.)
            // A scan under it returns just the columns used above and by the join's quals
            // (CP_SMALL_TLIST), possibly none.
            let mut small: Option<Vec<usize>> = None;
            if self.o.verbose && r.node.is_scan() {
                let nleft = l.exported.len();
                let width = r.internal.len();
                let mut order: Vec<usize> = Vec::new();
                let add = |i: usize, order: &mut Vec<usize>| {
                    if i < width && !order.contains(&i) {
                        order.push(i);
                    }
                };
                if !matches!(kind, JoinKind::Semi | JoinKind::Anti)
                    && let Some(need) = self.needed.get(&at)
                {
                    for i in 0..width {
                        if need.get(nleft + i).copied().unwrap_or(true) {
                            add(i, &mut order);
                        }
                    }
                }
                for q in &others {
                    for c in q.columns() {
                        if c >= nleft {
                            add(c - nleft, &mut order);
                        }
                    }
                }
                // (And the columns of an equivalence class the join's clause was implied
                // by: Postgres still carries them up to the join.)
                for &c in &meta.kept {
                    if c >= nleft {
                        add(c - nleft, &mut order);
                    }
                }
                // (And its reltarget's join clause columns, whether read here or not.)
                for c in scan_reltarget(if swapped { left } else { right }) {
                    add(c, &mut order);
                }
                order.sort_unstable();
                if order.len() < width {
                    r.node.output = order.iter().map(|&i| self.pick(&r.internal[i], Cx::Output)).collect();
                    small = Some(order);
                }
            }
            r.internal = r.exported.clone();
            if self.o.verbose {
                mn.output = match &small {
                    Some(order) => order.iter().map(|&i| self.pick(&r.internal[i], Cx::Output)).collect(),
                    // (Over a Gather: what it returns, its exact target list.)
                    None if r.node.node_type == "Gather" => r.node.output.iter().map(|o| refer_to(o)).collect(),
                    // (Over a join: its target list, the columns needed above it, possibly
                    // none.)
                    None if r.node.is_join() => r.node.output.iter().map(|o| refer_to(o)).collect(),
                    // (Over an inheritance tree's Append: its appendrel's reltarget.)
                    None if r.node.node_type == "Append"
                        && r.node.children.iter().all(ExNode::is_scan)
                        && !r.node.output.is_empty() =>
                    {
                        r.node.output.clone()
                    }
                    None => r.internal.iter().map(|x| self.pick(x, Cx::Output)).collect(),
                };
            }
            let child = std::mem::take(&mut r.node);
            mn.children.push(child.child("Outer"));
            r.node = mn;
        }
        if let Some(m) = meta.memoize.as_ref().filter(|_| method == JoinMethod::NestLoop) {
            let keys: Vec<String> = m
                .keys
                .iter()
                .map(|k| self.show(k, &l.exported, Cx::Upper))
                .collect();
            let mut mn = ExNode {
                key: Some(at + 1),
                rows: r.node.rows,
                width: r.node.width,
                cols: r.node.cols.clone(),
                startup_cost: r.node.startup_cost + costs::CPU_TUPLE_COST,
                total_cost: r.node.total_cost + costs::CPU_TUPLE_COST,
                ..ExNode::new("Memoize")
            };
            mn.details.push(Detail::Text("Cache Key", keys.join(", ")));
            mn.details.push(Detail::Text(
                "Cache Mode",
                if m.binary { "binary" } else { "logical" }.into(),
            ));
            mn.details.push(Detail::MemoizeInfo);
            if self.o.verbose {
                mn.output = r
                    .internal
                    .iter()
                    .map(|x| self.pick(x, Cx::Output))
                    .collect();
            }
            let child = std::mem::take(&mut r.node);
            mn.children.push(child.child("Outer"));
            r.node = mn;
        }
        n.subplans.append(&mut self.pending);
        n.inner_cached = method == JoinMethod::NestLoop
            && !lateral
            && !material
            && meta.memoize.is_none()
            && !matches!(kind, JoinKind::Right | JoinKind::Full);
        // The rows' order: a nested loop's or merge join's outer side's (an inner merge
        // join's keys are equal on both sides: either names it).
        if method != JoinMethod::Hash && !matches!(kind, JoinKind::Full) && (kind != JoinKind::Right || swapped) {
            n.order = l.node.order.clone();
            n.order_eq = l.node.order_eq.clone();
            n.order_eq.resize(n.order.len(), Vec::new());
            if method == JoinMethod::Merge && kind == JoinKind::Inner {
                for (i, o) in n.order.iter().enumerate() {
                    for (a, b) in &merge_names {
                        if let Some(rest) = o.strip_prefix(a.as_str())
                            && (rest.is_empty() || rest.starts_with(' '))
                        {
                            n.order_eq[i].push(format!("{b}{rest}"));
                        }
                    }
                }
            }
        }
        // (The outer side's scan reading a system column has no physical target list
        // either, use_physical_tlist refusing: its reltarget, the columns used above, then
        // the keys' and the quals'.)
        if self.o.verbose
            && method == JoinMethod::Hash
            && l.node.is_scan()
            && l.node.output.iter().any(|o| {
                !o.contains(['(', ' ', '\'', ':'])
                    && matches!(o.rsplit('.').next(), Some("tableoid" | "ctid" | "xmin" | "xmax" | "cmin" | "cmax"))
            })
        {
            let nleft = if swapped { r.exported.len() } else { l.exported.len() };
            let outer_off = if swapped { nleft } else { 0 };
            let width = l.internal.len();
            let mut order: Vec<usize> = Vec::new();
            let add = |i: usize, order: &mut Vec<usize>| {
                if i < width && !order.contains(&i) {
                    order.push(i);
                }
            };
            if let Some(need) = self.needed.get(&at) {
                for i in 0..width {
                    if need.get(outer_off + i).copied().unwrap_or(true) {
                        add(i, &mut order);
                    }
                }
            }
            for (a, b) in &keys {
                let ok = if swapped { b } else { a };
                for c in ok.columns() {
                    add(c, &mut order);
                }
            }
            for q in &others {
                for c in q.columns() {
                    if c >= outer_off && c < outer_off + width {
                        add(c - outer_off, &mut order);
                    }
                }
            }
            if !order.is_empty() && order.len() < width {
                l.node.output = order.iter().map(|&i| self.pick(&l.internal[i], Cx::Output)).collect();
            }
        }
        // A scan the join reads as it is (no Sort, Hash or Materialize between them: flags
        // 0 in create_nestloop_plan / create_mergejoin_plan / create_hashjoin_plan's outer
        // side) under a pulled-up subquery's selection of its columns returns its physical
        // target list (use_physical_tlist: the subquery is no node of Postgres's).
        if self.o.verbose {
            let hashed = method == JoinMethod::Hash;
            for (i, side) in [&mut l.node, &mut r.node].into_iter().enumerate() {
                if hashed && i == 1 {
                    continue;
                }
                if !side.physical.is_empty() && side.is_scan() {
                    side.output = side.physical.clone();
                }
                // (A semi join's side hashed unique: its scan's, the HashAggregate passing
                // the flags on.)
                if side.node_type == "Aggregate"
                    && side.strategy == Some("Hashed")
                    && let [c] = &mut side.children[..]
                    && !c.physical.is_empty()
                    && c.is_scan()
                {
                    c.output = c.physical.clone();
                }
            }
        }
        n.children.push(l.node.child("Outer"));
        if method == JoinMethod::Hash {
            let mut h = ExNode::new("Hash");
            h.subplans = std::mem::take(&mut hash_subplans);
            h.key = r.node.key;
            h.rows = r.node.rows;
            h.width = r.node.width;
            h.startup_cost = r.node.total_cost;
            h.total_cost = r.node.total_cost;
            // (The inner columns the table keeps: those used above, the keys' and the
            // quals'.)
            if self.o.analyze {
                let nleft = if swapped { r.exported.len() } else { l.exported.len() };
                let inner_off = if swapped { 0 } else { nleft };
                let width = r.exported.len();
                let mut cols: Vec<usize> = Vec::new();
                let need = self.needed.get(&at).filter(|_| !matches!(kind, JoinKind::Semi | JoinKind::Anti));
                for i in 0..width {
                    if need.is_some_and(|m| m.get(inner_off + i).copied().unwrap_or(true)) {
                        cols.push(i);
                    }
                }
                for (a, b) in &keys {
                    for c in (if swapped { a } else { b }).columns() {
                        if c < width && !cols.contains(&c) {
                            cols.push(c);
                        }
                    }
                }
                for q in &others {
                    for c in q.columns() {
                        if c >= inner_off && (swapped || c >= nleft) && c - inner_off < width && !cols.contains(&(c - inner_off)) {
                            cols.push(c - inner_off);
                        }
                    }
                }
                h.hash_cols = cols;
            }
            if self.o.verbose {
                // (A Hash returns its input's values: a computed one by reference.)
                // (An inheritance tree's Append returns the parent's columns.)
                let src = if r.exported.len() == r.internal.len()
                    && (r.node.is_scan() || r.node.node_type == "Append" && r.node.children.iter().all(ExNode::is_scan))
                {
                    &r.exported
                } else {
                    &r.internal
                };
                h.output = src.iter().map(|x| self.pick(x, Cx::Output)).collect();
                // (Over a Gather: what it returns, its exact target list.)
                if r.node.node_type == "Gather" && !r.node.output.is_empty() {
                    h.output = r.node.output.iter().map(|o| refer_to(o)).collect();
                }
                // (Of the parent's columns those of its appendrel's reltarget: the node's
                // own masked list.)
                if r.node.node_type == "Append" && r.node.children.iter().all(ExNode::is_scan) && !r.node.output.is_empty() {
                    h.output = r.node.output.clone();
                }
                // (A Subquery Scan returns what the join reads of it.)
                if r.node.node_type == "Subquery Scan" && !r.node.output.is_empty() {
                    h.output = r.node.output.clone();
                }
                // (So does a join: its exact target list, build_path_tlist.)
                if r.node.is_join() && !r.node.output.is_empty() {
                    h.output = r.node.output.iter().map(|o| refer_to(o)).collect();
                }
                // The hashed side's scan returns just the columns the join needs
                // (CP_SMALL_TLIST): those used above, then the keys' and quals'.
                if r.node.is_scan() {
                    let nleft = if swapped { r.exported.len() } else { l.exported.len() };
                    let inner_off = if swapped { 0 } else { nleft };
                    let width = r.internal.len();
                    let mut order: Vec<usize> = Vec::new();
                    let add = |i: usize, order: &mut Vec<usize>| {
                        if i < width && !order.contains(&i) {
                            order.push(i);
                        }
                    };
                    if let Some(need) = self.needed.get(&at).filter(|_| {
                        !matches!(kind, JoinKind::Semi | JoinKind::Anti)
                    }) {
                        for i in 0..width {
                            if need.get(inner_off + i).copied().unwrap_or(true) {
                                add(i, &mut order);
                            }
                        }
                    }
                    for (a, b) in &keys {
                        let ik = if swapped { a } else { b };
                        for c in ik.columns() {
                            add(c, &mut order);
                        }
                    }
                    for q in &others {
                        for c in q.columns() {
                            if c >= inner_off && (swapped || c >= nleft) {
                                add(c - inner_off, &mut order);
                            }
                        }
                    }
                    // (And its reltarget's join clause columns, whether read here or not.)
                    for c in scan_reltarget(if swapped { left } else { right }) {
                        add(c, &mut order);
                    }
                    // (A pulled-up subquery's PlaceHolderVars come after the relation's
                    // Vars: add_placeholders_to_base_rels adds them last.)
                    if let Plan::Project { input: pin, exprs } = &**(if swapped { left } else { right })
                        && matches!(&**pin, Plan::Scan(_))
                    {
                        order.sort_by_key(|&i| exprs.get(i).is_some_and(|e| !matches!(e, Expr::Col(..))));
                    }
                    if !order.is_empty() && order.len() < width {
                        let small: Vec<String> = order
                            .iter()
                            .map(|&i| self.pick(&r.internal[i], Cx::Output))
                            .collect();
                        // (The Hash returns its input's values: a computed one by
                        // reference.)
                        h.output = order
                            .iter()
                            .map(|&i| self.pick(r.exported.get(i).unwrap_or(&r.internal[i]), Cx::Output))
                            .collect();
                        r.node.output = small;
                    }
                }
            }
            h.details.push(Detail::HashInfo);
            h.children.push(r.node.child("Outer"));
            n.children.push(h.child("Inner"));
        } else {
            n.children.push(r.node.child("Inner"));
        }
        let internal = if matches!(kind, JoinKind::Semi | JoinKind::Anti) {
            if swapped { r.exported.clone() } else { l.exported.clone() }
        } else {
            all.clone()
        };
        // The join over the outer side's partial plan, the inner side whole in each worker
        // (it must be something a worker can run).
        let (outer_part, inner_part) = if swapped { (rpart, lpart) } else { (lpart, rpart) };
        let outer_part = nl_outer.map(Box::new).or(outer_part);
        n.par_safe = n.children.iter().all(parallel::safe_tree);
        if let (Some(c), Some(po)) = (pcost, outer_part)
            && gating.is_empty()
            && (parallel_hash
                || matches!(shown_kind, JoinKind::Inner | JoinKind::Left | JoinKind::Semi | JoinKind::Anti))
            && (parallel_hash || n.children.get(1).is_some_and(parallel::safe_tree))
            && let Some(cfg) = self.par.clone()
        {
            let mut pn = n.clone();
            pn.startup_cost = c.startup;
            pn.total_cost = c.total;
            pn.rows = costs::clamp_rows(n.rows / cfg.divisor(po.workers));
            pn.workers = po.workers;
            if self.o.verbose {
                pn.output = Vec::new();
            }
            pn.children[0] = po.child("Outer");
            if parallel_hash
                && let Some(pi) = inner_part
                && let Some(h) = pn.children.get_mut(1)
            {
                pn.title = format!("Parallel {}", pn.title);
                pn.parallel_aware = true;
                h.title = "Parallel Hash".into();
                h.parallel_aware = true;
                h.rows = pi.rows;
                h.startup_cost = pi.total_cost;
                h.total_cost = pi.total_cost;
                let mut pi = *pi;
                if self.o.verbose {
                    pi.output = h.children.first().map(|c| c.output.clone()).unwrap_or_default();
                }
                h.children = vec![pi.child("Outer")];
            }
            n.partial = Some(Box::new(pn));
        }
        let n = if gating.is_empty() {
            n
        } else {
            // (The join returns the columns needed above; the Result computes the rest.)
            let mut n = n;
            if self.o.verbose
                && n.output.is_empty()
                && let Some(m) = self.needed.get(&at)
            {
                n.output = all
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| m.get(*i).copied().unwrap_or(true))
                    .map(|(_, x)| self.pick(x, Cx::Output))
                    .collect();
            }
            self.gate(n, &gating, &all)
        };
        Built::passthrough(n, internal)
    }
}

/// add_path's fuzzy comparison: `a` clearly cheaper than `b` (a total within 1% lets the
/// startup decide).
/// `e` calls a user-defined function run as a subquery.
fn has_calls(e: &Expr) -> bool {
    let mut any = false;
    e.walk(&mut |x| {
        if matches!(x, Expr::Subquery(s) if s.call.is_some()) {
            any = true;
        }
    });
    any
}

/// User-defined function calls run as subqueries as the planner leaves them: the call, or
/// the body inlined in its place (not SubPlans). A STRICT call's NULL check goes with it.
fn show_calls(e: Expr) -> Expr {
    match e {
        Expr::Case {
            operand: None,
            whens,
            else_: Some(el),
            ty,
        } if whens.len() == 1
            && matches!(whens[0].1, Expr::Const(Value::Null, _))
            && matches!(&*el, Expr::Subquery(s) if s.call.as_ref().is_some_and(|c| {
                let nulls: Vec<Expr> = s.params[..c.nargs.min(s.params.len())]
                    .iter()
                    .map(|p| Expr::IsNull(Box::new(p.clone()), false))
                    .collect();
                format!("{:?}", whens[0].0) == format!("{:?}", Expr::Or(nulls))
            })) =>
        {
            let _ = ty;
            show_calls(*el)
        }
        Expr::Subquery(s) if s.call.is_some() => {
            let c = s.call.clone().unwrap();
            let args: Vec<Expr> = s
                .params
                .iter()
                .take(c.nargs)
                .cloned()
                .map(show_calls)
                .collect();
            if c.inline
                && let Some(body) = inlined_row(&s.plan).and_then(|mut r| {
                    (r.len() == 1).then(|| r.remove(0))
                })
                && let Ok(body) = body.transform(&mut |x| {
                    Ok(match x {
                        Expr::Outer(1, i, t) => args.get(i).cloned().unwrap_or(Expr::Outer(1, i, t)),
                        x => x,
                    })
                })
            {
                return show_calls(body);
            }
            Expr::Call(c.def.clone(), args, s.ty.clone())
        }
        e => e
            .map_children(&mut |c| Ok(show_calls(c)))
            .expect("show_calls"),
    }
}

/// The row an inlined function body computes (over its arguments, `Outer(1, i)`): a
/// projection of no input.
fn inlined_row(p: &Plan) -> Option<Vec<Expr>> {
    match p {
        Plan::One => Some(Vec::new()),
        Plan::Context { input, .. } => inlined_row(input),
        Plan::Limit {
            input,
            limit: Some(Expr::Const(Value::Int8(1), _)),
            offset: None,
            with_ties: None,
        } => inlined_row(input),
        Plan::Project { input, exprs } => {
            let base = inlined_row(input)?;
            exprs
                .iter()
                .map(|e| {
                    e.clone()
                        .transform(&mut |x| match x {
                            Expr::Col(i, _) => base.get(i).cloned().ok_or_else(|| {
                                crate::error::internal("inlined_row")
                            }),
                            x => Ok(x),
                        })
                        .ok()
                })
                .collect()
        }
        _ => None,
    }
}

fn fuzzy_better(a: &super::joins::Cost, b: &super::joins::Cost) -> bool {
    const FUZZ: f64 = 1.01;
    if a.disabled != b.disabled {
        return a.disabled < b.disabled;
    }
    if b.total > a.total * FUZZ {
        return true;
    }
    if a.total > b.total * FUZZ {
        return false;
    }
    if b.startup > a.startup * FUZZ {
        return true;
    }
    if a.startup > b.startup * FUZZ {
        return false;
    }
    a.total < b.total
}

/// innerrel_is_unique (rel_is_distinct_for): `p` reads one table (through projections and
/// filters) with a unique index, no predicate, every key column of which `keys` equate.
fn unique_for(cat: &crate::catalog::Catalog, p: &Plan, keys: &[&Expr]) -> bool {
    match p {
        Plan::Project { input, exprs } => {
            let mapped: Vec<Expr> = keys
                .iter()
                .filter_map(|k| match strip_widening(k) {
                    Expr::Col(i, _) => exprs.get(*i).cloned(),
                    _ => None,
                })
                .collect();
            let refs: Vec<&Expr> = mapped.iter().collect();
            unique_for(cat, input, &refs)
        }
        Plan::Filter { input, .. } | Plan::Barrier { input, .. } | Plan::Context { input, .. } => unique_for(cat, input, keys),
        // A partitioned table's members: its own unique indexes (which hold the partition
        // key, so they are unique across the partitions too), over its columns.
        Plan::Append { inputs, inh: Some(_), .. } => {
            let mut m: &Plan = match inputs.first() {
                Some(m) => m,
                None => return false,
            };
            while let Plan::Project { input, .. } | Plan::Filter { input, .. } = m {
                m = input;
            }
            let Plan::Scan(ms) = m else { return false };
            let Some(parent) = ms.table.inherits.first().and_then(|o| cat.table(*o)) else {
                return false;
            };
            if parent.partkey.is_none() {
                return false;
            }
            let cols: Vec<usize> = keys
                .iter()
                .filter_map(|k| match strip_widening(k) {
                    Expr::Col(i, _) => Some(*i),
                    _ => None,
                })
                .collect();
            cat.table_indexes(parent)
                .iter()
                .chain(parent.pkey_index.and_then(|o| cat.index(o)))
                .any(|ix| {
                    (ix.unique || ix.primary)
                        && ix.predicate.is_none()
                        && ix.columns.iter().all(|c| {
                            matches!(c.key, crate::catalog::IndexKey::Column(k) if cols.contains(&k))
                        })
                })
        }
        Plan::Scan(s) => {
            let cols: Vec<usize> = keys
                .iter()
                .filter_map(|k| match strip_widening(k) {
                    Expr::Col(i, _) => Some(*i),
                    _ => None,
                })
                .collect();
            let t = &s.table;
            cat.table_indexes(t)
                .iter()
                .chain(t.pkey_index.and_then(|o| cat.index(o)))
                .any(|ix| {
                    (ix.unique || ix.primary)
                        && ix.valid
                        && ix.predicate.is_none()
                        && ix.columns.iter().all(|c| {
                            matches!(c.key, crate::catalog::IndexKey::Column(k) if cols.contains(&k))
                        })
                })
        }
        _ => false,
    }
}

fn lock_rows(child: ExNode) -> ExNode {
    let mut n = ExNode {
        key: child.key,
        rows: child.rows,
        width: child.width,
        startup_cost: child.startup_cost,
        total_cost: child.total_cost + child.rows * 0.01,
        ..ExNode::new("LockRows")
    };
    n.output = child.output.clone();
    // (Its rows are its input's, in their order: create_lockrows_path keeps the
    // subpath's pathkeys, and its columns' statistics are theirs.)
    n.cols = child.cols.clone();
    n.order = child.order.clone();
    n.children.push(child.child("Outer"));
    n
}

fn union_arms<'p>(p: &'p Arc<Plan>, all: bool, out: &mut Vec<&'p Arc<Plan>>) {
    match &**p {
        Plan::SetOp {
            kind: SetOpKind::Union,
            all: a,
            left,
            right,
            ..
        } if *a == all => {
            union_arms(left, all, out);
            union_arms(right, all, out);
        }
        Plan::Append { inputs, inh: None, .. } if all => {
            inputs.iter().for_each(|i| union_arms(i, all, out))
        }
        _ => out.push(p),
    }
}

/// A system catalog's rows now (its reltuples), cached for the catalog version.
pub(crate) fn sys_tuples(table: crate::catalog::system::SysTable, ctx: &ExecCtx) -> f64 {
    // (By the catalog's address, with a `Weak` to it: that keeps the allocation, so a later
    // catalog can't take the address and find this one's count.)
    type SysRows = HashMap<(usize, u32), (std::sync::Weak<crate::catalog::Catalog>, f64)>;
    thread_local! {
        static SYS_ROWS: std::cell::RefCell<SysRows> = std::cell::RefCell::new(HashMap::new());
    }
    let cat = ctx.catalog();
    let key = (Arc::as_ptr(&cat) as usize, table.oid());
    if let Some(n) = SYS_ROWS.with(|m| m.borrow().get(&key).map(|e| e.1)) {
        return n;
    }
    let n = if crate::catalog::system::rows_cacheable(table) {
        // (Counted without generating the rows, unless they are already: `row_count`.)
        match crate::catalog::system::rows_cached(table, ctx) {
            Some(r) => Ok(r.rows.len()),
            None => match crate::catalog::system::row_count(table, ctx) {
                Some(n) => Ok(n),
                None => crate::catalog::system::rows_shared(table, ctx).map(|r| r.rows.len()),
            },
        }
        .map_or(1000.0, |n| n as f64)
    } else {
        // (Counted without generating the rows where it can be: `row_count`.)
        match crate::catalog::system::row_count(table, ctx) {
            Some(n) => n as f64,
            None => crate::catalog::system::rows(table, ctx).map_or(1000.0, |r| r.len() as f64),
        }
    }
    .max(1.0);
    SYS_ROWS.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() > 256 {
            m.clear();
        }
        m.insert(key, (Arc::downgrade(&cat), n));
    });
    n
}

/// The planner planning a WITH's body: its CTEs' estimated rows and columns, for the CTE
/// Scans it estimates.
pub(crate) fn push_cte_estimates(ctes: &[(usize, Arc<Plan>)], names: &[String], ctx: &ExecCtx) -> usize {
    let mut n = 0;
    for ((_, cte), name) in ctes.iter().zip(names) {
        let e = estimate(cte, ctx);
        let cols: Vec<ColInfo> = e
            .cols
            .iter()
            .map(|(w, v, d)| ColInfo {
                width: *w,
                var: v.clone().filter(|v| v.expr.is_none()),
                // (get_variable_numdistinct without statistics: a small relation's rows.)
                distinct: d.or((e.rows < costs::DEFAULT_NUM_DISTINCT).then(|| costs::clamp_rows(e.rows))),
            })
            .collect();
        cte_push(name.clone(), e.rows, cols, plan_output_order(cte, ctx));
        n += 1;
    }
    n
}

/// The planner planning a recursive union's recursive term: its worktable's estimate
/// (set_cte_size_estimates of a self-reference: ten times the non-recursive term's rows).
pub(crate) fn push_work_estimate(id: usize, base: &Arc<Plan>, ctx: &ExecCtx) {
    let e = estimate(base, ctx);
    let cols: Vec<ColInfo> = e
        .cols
        .iter()
        // (No statistics through a self-reference: examine_variable doesn't look into it;
        // get_variable_numdistinct takes a small relation's tuples.)
        .map(|(w, _, _)| ColInfo {
            width: *w,
            var: None,
            distinct: (e.rows * 10.0 < costs::DEFAULT_NUM_DISTINCT).then(|| costs::clamp_rows(e.rows * 10.0)),
        })
        .collect();
    cte_push(format!("\u{1}{id}"), e.rows * 10.0, cols, Vec::new());
}

pub(crate) fn pop_cte_estimates(n: usize) {
    CTE_ESTIMATES.with(|m| {
        let mut m = m.borrow_mut();
        let keep = m.len().saturating_sub(n);
        m.truncate(keep);
    });
    CTE_ORDERS.with(|m| {
        let mut m = m.borrow_mut();
        let keep = m.len().saturating_sub(n);
        m.truncate(keep);
    });
}

/// Per-statement scratch back at rest (`scoped::reset_statement_scratch`).
pub(crate) fn reset_scratch() {
    FILTER_OVER_WINDOW.set(false);
    MERGE_MEMBERS.set(false);
    UNDER_PW.set(false);
    UNDER_LOCK_ROWS.set(0);
    RULE_RETURNING.with_borrow_mut(|r| *r = None);
    WINDOW_TLIST.with_borrow_mut(|w| w.clear());
    WINDOW_ORDER.with_borrow_mut(|w| w.clear());
    INH_SCANS.with_borrow_mut(|m| m.clear());
    CTE_ESTIMATES.with_borrow_mut(|m| m.clear());
    CTE_ORDERS.with_borrow_mut(|m| m.clear());
}

/// The flags and scratch not at rest (`scoped::unsettled_for_test`).
pub(crate) fn unsettled(out: &mut Vec<&'static str>) {
    for (name, set) in [
        ("explain::RULE_RETURNING", RULE_RETURNING.with_borrow(|r| r.is_some())),
        ("explain::UNDER_LOCK_ROWS", UNDER_LOCK_ROWS.get() != 0),
        ("explain::WINDOW_TLIST", WINDOW_TLIST.with_borrow(|w| !w.is_empty())),
        ("explain::CTE_ESTIMATES", CTE_ESTIMATES.with_borrow(|m| !m.is_empty())),
        ("explain::CTE_ORDERS", CTE_ORDERS.with_borrow(|m| !m.is_empty())),
        ("explain::FILTER_OVER_WINDOW", FILTER_OVER_WINDOW.get()),
        ("explain::MERGE_MEMBERS", MERGE_MEMBERS.get()),
        ("explain::UNDER_PW", UNDER_PW.get()),
        ("explain::INH_SCANS", INH_SCANS.with_borrow(|m| !m.is_empty())),
    ] {
        if set {
            out.push(name);
        }
    }
}

/// CTE estimates pushed (`push_cte_estimates`), popped when dropped.
pub(crate) struct CteEstimatesPushed(pub usize);

impl Drop for CteEstimatesPushed {
    fn drop(&mut self) {
        pop_cte_estimates(self.0);
    }
}

/// A CTE's estimate and output order, for the CTE Scans of the query being planned.
fn cte_push(name: String, rows: f64, cols: Vec<ColInfo>, order: Vec<SortKey>) {
    CTE_ORDERS.with(|m| m.borrow_mut().push((name.clone(), order)));
    CTE_ESTIMATES.with(|m| m.borrow_mut().push((name, rows, cols)));
}

/// The order a materialized CTE's (or any query's) rows come out in: its top Sort's keys or
/// an ordered index scan's, over its output columns, through projections and a LIMIT.
fn plan_output_order(p: &Plan, ctx: &ExecCtx) -> Vec<SortKey> {
    let w = super::binder::plan_width(p);
    let mut map: Vec<Option<usize>> = (0..w).map(Some).collect();
    let mut cur = p;
    let keys = loop {
        match cur {
            Plan::Barrier { input, .. } | Plan::Limit { input, .. } | Plan::LockRows { input, .. } | Plan::Context { input, .. } => {
                cur = input
            }
            Plan::Project { input, exprs } => {
                map = map
                    .iter()
                    .map(|m| m.and_then(|i| match exprs.get(i) {
                        Some(Expr::Col(j, _)) => Some(*j),
                        _ => None,
                    }))
                    .collect();
                cur = input;
            }
            Plan::Sort { keys, .. } => break keys.clone(),
            Plan::Scan(sp) if matches!(sp.access, crate::sql::plan::Access::Index { ordered: true, .. }) => {
                break super::paths::index_order(ctx, sp);
            }
            _ => return Vec::new(),
        }
    };
    let mut out = Vec::new();
    for k in keys {
        let Expr::Col(c, _) = &k.expr else { break };
        let Some(o) = map.iter().position(|m| *m == Some(*c)) else { break };
        let ty = k.expr.ty();
        out.push(SortKey { expr: Expr::Col(o, ty), ..k });
    }
    out
}

/// A CTE Scan's order (`plan_output_order` of its query), by the CTE's name.
pub(crate) fn cte_order(name: &str) -> Vec<SortKey> {
    CTE_ORDERS.with(|m| m.borrow().iter().rev().find(|x| x.0 == name).map(|x| x.1.clone()).unwrap_or_default())
}

thread_local! {
    /// The target list over the windows being converted: the input columns it reads.
    static WINDOW_TLIST: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
    /// The ORDER BY over the windows being converted, as expressions over their input.
    static WINDOW_ORDER: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    /// The CTEs of the WITH bodies being planned (`push_cte_estimates`).
    static CTE_ESTIMATES: std::cell::RefCell<Vec<(String, f64, Vec<ColInfo>)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Beside each CTE_ESTIMATES entry: the CTE's output order (its query's pathkeys, which
    /// a CTE Scan keeps).
    static CTE_ORDERS: std::cell::RefCell<Vec<(String, Vec<SortKey>)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Converting a filter's input that is a FROM subquery's windows.
    static FILTER_OVER_WINDOW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Converting the members of a Merge Append (`merge_appended`).
    static MERGE_MEMBERS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Explaining a partitionwise join's child joins (a sub-partitioned partition's own
    /// Append among them is named as that partition).
    static UNDER_PW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// A merge join's inner input (its address): sorted in full, an incremental sort can't
    /// mark and restore (initial_cost_mergejoin).
    static MERGE_INNER: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Explaining an outer (or anti) join's nullable side or its own clauses: they don't
    /// come out of equivalence classes (`name_text_relabel`).
    static OUTER_JOIN_SIDE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The scans that are an inheritance Append's members as they are (the parent's own
    /// rows): an appendrel member's target list is what is needed above, no physical one.
    static INH_SCANS: std::cell::RefCell<std::collections::HashSet<usize>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

/// An Append over partitions read in their order (`paths::partition_order`), of the
/// members `slots` (taken); `list`: the sort keys, as the parent's names show them.
fn ordered_append(order: &[crate::sql::paths::PartOrder], slots: &mut [Option<ExNode>], list: &[String]) -> ExNode {
    use crate::sql::paths::PartOrder;
    let mut a = ExNode::new("Append");
    // (Members pruned as the executor starts: empty places.)
    let mut removed = 0;
    for m in order {
        let k = match m {
            PartOrder::Leaf(i) => match slots[*i].take() {
                Some(k) => k,
                None => {
                    removed += 1;
                    continue;
                }
            },
            PartOrder::Append(sub) => ordered_append(sub, slots, list),
            PartOrder::Merge(v) => {
                let first_pruned = v.first().is_some_and(|i| slots[*i].is_none());
                let kids: Vec<ExNode> = v.iter().filter_map(|i| slots[*i].take()).collect();
                let gone = v.len() - kids.len();
                let n = kids.len() as f64;
                let cmp = 2.0 * costs::CPU_OPERATOR_COST;
                let logn = n.log2();
                let mut ma = ExNode::new("Merge Append");
                // (Its keys over its first member's row.)
                let alias = kids.first().and_then(member_alias);
                let parent = kids.first().and_then(member_parent_alias);
                let keys: Vec<String> = match (alias, parent) {
                    // (Its first member pruned: Postgres resolves the keys through that
                    // member, which has no name then.)
                    (Some(_), Some(_)) if first_pruned => list.iter().map(|k| unqualified(k)).collect(),
                    (Some(al), Some(pa)) => list.iter().map(|k| k.replace(&format!("{pa}."), &format!("{al}."))).collect(),
                    _ => list.to_vec(),
                };
                ma.details.push(Detail::List("Sort Key", keys.clone()));
                if gone > 0 {
                    ma.details.push(Detail::Text("Subplans Removed", gone.to_string()));
                }
                ma.rows = kids.iter().map(|k| k.rows).sum();
                ma.width = kids.iter().map(|k| k.width).max().unwrap_or(0);
                ma.startup_cost = kids.iter().map(|k| k.startup_cost).sum::<f64>() + cmp * n * logn;
                ma.total_cost = ma.startup_cost
                    + kids.iter().map(|k| k.total_cost - k.startup_cost).sum::<f64>()
                    + ma.rows * cmp * logn
                    + costs::CPU_TUPLE_COST * 0.5 * ma.rows;
                ma.order = keys;
                ma.children = kids;
                ma
            }
        };
        a.children.push(k.child("Member"));
    }
    if removed > 0 {
        add_removed(&mut a.details, removed);
    }
    // (One member: no Append.)
    if a.children.len() == 1 && removed == 0 {
        let mut k = a.children.pop().expect("one member");
        k.relationship = None;
        return k;
    }
    a.rows = a.children.iter().map(|k| k.rows).sum();
    a.width = a.children.iter().map(|k| k.width).max().unwrap_or(0);
    a.startup_cost = a.children.first().map_or(0.0, |k| k.startup_cost);
    a.total_cost = a.children.iter().map(|k| k.total_cost).sum::<f64>() + costs::CPU_TUPLE_COST * 0.5 * a.rows;
    a.order = list.to_vec();
    a
}

/// A plain column reference `rel.col` without its qualifier.
fn unqualified(k: &str) -> String {
    match k.split_once('.') {
        Some((q, c)) if !q.contains(['(', ' ', ')']) && !c.contains(['(', ' ', '.']) => c.to_string(),
        _ => k.to_string(),
    }
}

/// Of the partitions an ordered Append under `p` (through projections) reads, in the
/// order of its members: whether run-time pruning keeps each as the executor starts.
fn startup_kept(p: &Plan, ctx: &ExecCtx) -> Option<Vec<bool>> {
    let mut x = p;
    while let Plan::Project { input, .. } = x {
        x = input;
    }
    let Plan::Append { inputs, inh: Some(_), .. } = x else { return None };
    let kept = crate::exec::nodes::runtime_pruned(inputs, crate::sql::binder::partition::Known::Start, ctx);
    Some(
        inputs
            .iter()
            .filter(|i| !matches!(&***i, Plan::Empty { .. }))
            .map(|i| kept.iter().any(|k| Arc::ptr_eq(k, i)))
            .collect(),
    )
}

/// `k` more members pruned as the executor starts, on an Append's details (first).
fn add_removed(details: &mut Vec<Detail>, k: usize) {
    if let Some(Detail::Text("Subplans Removed", n)) =
        details.iter_mut().find(|d| matches!(d, Detail::Text("Subplans Removed", _)))
    {
        *n = (n.parse::<usize>().unwrap_or(0) + k).to_string();
        return;
    }
    details.insert(0, Detail::Text("Subplans Removed", k.to_string()));
}

/// A Result over `n` returning `output` (None: `n` as it is).
fn result_over(n: ExNode, output: Option<Vec<String>>) -> ExNode {
    let Some(output) = output else { return n };
    let mut r = ExNode::new("Result");
    r.rows = n.rows;
    r.width = n.width;
    r.cols = n.cols.clone();
    r.startup_cost = n.startup_cost;
    r.total_cost = n.total_cost + costs::CPU_TUPLE_COST * n.rows;
    r.key = n.key;
    r.order = n.order.clone();
    r.output = output;
    r.children.push(n.child("Outer"));
    r
}

/// The alias a member scan (or the scan a member sorts) shows.
fn member_alias(n: &ExNode) -> Option<String> {
    if let Some((_, a, _)) = n.target.iter().find(|t| t.0 == "Alias") {
        return Some(a.clone());
    }
    n.children.first().and_then(member_alias)
}

/// The parent's name of a member's alias (`t_3`: `t`).
fn member_parent_alias(n: &ExNode) -> Option<String> {
    let a = member_alias(n)?;
    let (p, num) = a.rsplit_once('_')?;
    (!num.is_empty() && num.bytes().all(|b| b.is_ascii_digit())).then(|| p.to_string())
}

/// A Sort's input that is an append relation whose members each come in the Sort's
/// order (sorted, or read from an index in order): the planner's `merge_append`, a Merge
/// Append.
fn merge_appended(p: &Plan) -> bool {
    fn rel(p: &Plan, leaves: &mut usize) -> bool {
        match p {
            Plan::Project { input, .. } => rel(input, leaves),
            Plan::Append { inputs, .. } => inputs
                .iter()
                .filter(|i| !matches!(&***i, Plan::Empty { .. }))
                .all(|i| member(i, leaves)),
            Plan::SetOp {
                kind: SetOpKind::Union,
                all: true,
                left,
                right,
                ..
            } => member(left, leaves) && member(right, leaves),
            _ => false,
        }
    }
    fn member(p: &Plan, leaves: &mut usize) -> bool {
        match p {
            Plan::Sort { .. } => {
                *leaves += 1;
                true
            }
            Plan::Scan(s) if matches!(s.access, super::plan::Access::Index { ordered: true, .. }) => {
                *leaves += 1;
                true
            }
            // (A partitionwise child join planned in the merge's order.)
            Plan::Join { meta, .. } if meta.presorted => {
                *leaves += 1;
                true
            }
            Plan::Project { input, .. } if !matches!(&**input, Plan::Append { .. } | Plan::SetOp { .. }) => {
                member(input, leaves)
            }
            Plan::Project { .. } | Plan::Append { .. } | Plan::SetOp { .. } => rel(p, leaves),
            // (A leaf query planned apart: its own sort.)
            Plan::Barrier { input, secure: false, .. } => member(input, leaves),
            _ => false,
        }
    }
    let mut leaves = 0;
    rel(p, &mut leaves) && leaves > 1
}

/// The inheritance Appends that are UNION ALL arms (through projections): the planner
/// flattens them into the UNION ALL's Append.
fn union_inh_appends(p: &Plan, out: &mut Vec<usize>) {
    if matches!(
        p,
        Plan::SetOp {
            kind: SetOpKind::Union,
            all: true,
            ..
        } | Plan::Append { inh: None, .. }
    ) {
        let arc = Arc::new(p.clone());
        let mut arms = Vec::new();
        union_arms(&arc, true, &mut arms);
        for a in arms {
            let mut x = &**a;
            while let Plan::Project { input, .. } = x {
                x = input;
            }
            if let Plan::Append { inh: Some(_), inputs, .. } = x
                && inputs.iter().filter(|i| !matches!(&***i, Plan::Empty { .. })).count() > 1
            {
                // (By the address in `p`'s tree: the arms are shared, not copied.)
                out.push(addr(x));
            }
        }
    }
    for c in plan_inputs(p) {
        union_inh_appends(c, out);
    }
}

/// Column `i` of DISTINCT's input is a constant (no key): not an InitPlan's value, a
/// Param that is a key all the same.
fn param_free_constant(input: &Plan, i: usize) -> bool {
    match input {
        Plan::Project { exprs, .. } => exprs.get(i).is_some_and(|e| {
            let mut sub = false;
            e.walk(&mut |x| sub |= matches!(x, Expr::Subquery(_)));
            !sub && crate::sql::binder::optimize::pseudo_constant(e)
        }),
        _ => false,
    }
}

/// The scan of an UPDATE / DELETE target within its source plan.
fn find_inh_append(p: &Plan) -> Option<&Plan> {
    if let Plan::Append { inh: Some(_), .. } = p {
        return Some(p);
    }
    plan_inputs(p).into_iter().find_map(|c| find_inh_append(c))
}

/// The FROM subquery a plan's windows belong to (below its projection, sort and limit).
/// The plan of a FROM subquery under its projection, sort and limit.
/// The aggregation (its address) and aggregate (its place among the Agg's calls) an
/// input column of a node is, through sorts, limits, a HAVING, windows and projections of
/// columns.
fn json_agg_col(p: &Plan, k: usize) -> Option<(usize, usize)> {
    match p {
        Plan::Agg { groups, .. } => k.checked_sub(groups.len()).map(|i| (addr(p), i)),
        Plan::Filter { input, .. } | Plan::Sort { input, .. } | Plan::Limit { input, .. } => json_agg_col(input, k),
        Plan::Window { input, .. } if k < super::binder::plan_width(input) => json_agg_col(input, k),
        Plan::Project { input, exprs } => match exprs.get(k) {
            Some(Expr::Col(j, _)) => json_agg_col(input, *j),
            _ => None,
        },
        _ => None,
    }
}

fn subquery_input(p: &Plan) -> &Plan {
    match p {
        Plan::Project { input, .. } | Plan::Sort { input, .. } | Plan::Limit { input, .. } => {
            subquery_input(input)
        }
        p => p,
    }
}

fn subquery_of(p: &Plan) -> Option<Arc<(String, Vec<String>)>> {
    match p {
        Plan::Project { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Filter { input, .. }
        | Plan::Distinct { input, .. } => subquery_of(input),
        Plan::Window { calls, .. } => calls.first().and_then(|c| c.subquery.clone()),
        Plan::SetOp { sub, .. } => sub.clone(),
        Plan::Barrier { alias, cols, .. } => Some(Arc::new((alias.clone(), cols.clone()))),
        _ => None,
    }
}

/// The plan has an inheritance Append of several members (one of a single member is
/// dropped: its member takes the name).
fn multi_inh_append(p: &Plan) -> bool {
    matches!(find_inh_append(p), Some(Plan::Append { inputs, .. })
        if inputs.iter().filter(|m| !matches!(&***m, Plan::Empty { .. })).count() > 1)
}

/// The Append of an UPDATE / DELETE target's members within its source (theirs are the
/// scans with a row key).
fn target_append(p: &Plan) -> Option<&Plan> {
    fn keyed(p: &Plan) -> bool {
        match p {
            Plan::Scan(s) => s.with_key,
            Plan::Filter { input, .. } | Plan::Project { input, .. } => keyed(input),
            _ => false,
        }
    }
    if let Plan::Append { inh: Some(_), inputs, .. } = p
        && inputs.iter().any(|i| keyed(i))
    {
        return Some(p);
    }
    plan_inputs(p).into_iter().find_map(|c| target_append(c))
}

/// The target's Append has several members (one of a single member is dropped: its member
/// takes the name).
fn multi_target_append(p: &Plan) -> bool {
    matches!(target_append(p), Some(Plan::Append { inputs, .. })
        if inputs.iter().filter(|m| !matches!(&***m, Plan::Empty { .. })).count() > 1)
}

/// Whether pruning when the executor starts can remove a member of an inheritance UPDATE /
/// DELETE's Append from its result relations: the partitioning level the member is a
/// partition of has steps over values known then (the clauses on its key compare it with
/// parameters or stable expressions).
fn level_prunable(member: &Plan) -> bool {
    fn scan(p: &Plan, quals: &mut Vec<Expr>) -> Option<ScanPlan> {
        match p {
            Plan::Scan(s) => {
                if let Some(f) = &s.filter {
                    conjuncts(f, quals);
                }
                Some((**s).clone())
            }
            Plan::Project { input, .. } => scan(input, quals),
            Plan::Filter { input, pred } => {
                conjuncts(pred, quals);
                scan(input, quals)
            }
            _ => None,
        }
    }
    let mut quals = Vec::new();
    let Some(s) = scan(member, &mut quals) else {
        return true;
    };
    let Some(level) = s.part.as_ref().and_then(|l| l.first()) else {
        return true;
    };
    let keys: Vec<usize> = level.key.iter().flat_map(|k| k.columns()).collect();
    quals.iter().any(|q| {
        q.columns().iter().any(|c| keys.contains(c))
            && crate::sql::binder::partition::with_known(
                std::slice::from_ref(q),
                crate::sql::binder::partition::Known::Start,
                &mut |_| Ok(crate::types::Value::Null),
            )
            .ok()
            .flatten()
            .is_some()
    })
}

/// The first member of the Append of each inheritance UPDATE / DELETE with RETURNING in a
/// plan (its first result relation, whose columns RETURNING shows), by address.
fn dml_returning_firsts(root: &Plan) -> std::collections::HashSet<usize> {
    fn go(p: &Plan, out: &mut std::collections::HashSet<usize>) {
        let first = match p {
            Plan::Update(u) if u.inh && !u.returning.is_empty() => target_append(&u.source),
            Plan::Delete(d) if d.inh && !d.returning.is_empty() => target_append(&d.source),
            _ => None,
        };
        if let Some(Plan::Append { inputs, .. }) = first
            && let Some(m) = inputs.iter().find(|m| !matches!(&***m, Plan::Empty { .. }))
        {
            out.insert(addr(m));
        }
        for c in plan_inputs(p) {
            go(c, out);
        }
        let mut es = Vec::new();
        node_exprs(p, &mut es);
        let mut sq = Vec::new();
        for e in es {
            subqueries_in(e, &mut sq);
        }
        for s in sq {
            go(&s.plan, out);
        }
        if let Plan::With { ctes, .. } = p {
            for (_, c) in ctes {
                go(c, out);
            }
        }
    }
    let mut out = std::collections::HashSet::new();
    go(root, &mut out);
    out
}

/// The reference name of the inheritance tree an UPDATE / DELETE changes (its members'
/// Append's; when its source reads none, the name the statement gives the table).
fn dml_tree_name(source: &Plan, t: &crate::catalog::Table, alias: Option<&str>) -> String {
    match target_append(source) {
        Some(Plan::Append { inh: Some(n), .. }) => n.clone(),
        _ => alias.unwrap_or(&t.name).to_string(),
    }
}

/// Whether a plan joins relations (not just reads one).
fn has_join(p: &Plan) -> bool {
    match p {
        Plan::Join { .. } => true,
        p => plan_inputs(p).into_iter().any(|c| has_join(c)),
    }
}

/// Every scan of a data-modifying statement's target (an inheritance tree's members too).
fn target_scans(p: &Plan) -> Vec<&Plan> {
    fn go<'p>(p: &'p Plan, out: &mut Vec<&'p Plan>) {
        match p {
            Plan::Scan(s) if s.with_key => out.push(p),
            _ => plan_inputs(p).into_iter().for_each(|c| go(c, out)),
        }
    }
    let mut out = Vec::new();
    go(p, &mut out);
    out
}

fn target_scan(p: &Plan, oid: u32) -> Option<&Plan> {
    if let Plan::Scan(s) = p
        && s.with_key
        && s.table.oid == oid
    {
        return Some(p);
    }
    plan_inputs(p).into_iter().find_map(|c| target_scan(c, oid))
}

// ── output ──

/// explain.c's structured-output writer (`ExplainOpenGroup`, `ExplainProperty`, …).
struct Writer {
    format: Format,
    buf: String,
    indent: usize,
    /// Per open group: whether it has members yet (JSON) / whether the next line continues the
    /// current one (YAML).
    stack: Vec<bool>,
}

fn json_escape(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn xml_escape(s: &str, out: &mut String) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#x0d;"),
            c => out.push(c),
        }
    }
}

fn xml_tag(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

impl Writer {
    fn new(format: Format) -> Writer {
        let mut w = Writer {
            format,
            buf: String::new(),
            indent: 0,
            stack: Vec::new(),
        };
        match format {
            Format::Xml => {
                w.buf
                    .push_str("<explain xmlns=\"http://www.postgresql.org/2009/explain\">\n");
                w.indent += 1;
            }
            Format::Json => {
                w.buf.push('[');
                w.stack.push(false);
                w.indent += 1;
            }
            Format::Yaml => w.stack.push(false),
            Format::Text => {}
        }
        w
    }

    fn finish(mut self) -> String {
        match self.format {
            Format::Xml => {
                self.indent -= 1;
                self.buf.push_str("</explain>");
            }
            Format::Json => {
                self.indent -= 1;
                self.buf.push_str("\n]");
            }
            _ => {}
        }
        self.buf
    }

    fn spaces(&mut self, n: usize) {
        self.buf.extend(std::iter::repeat_n(' ', n));
    }

    /// show_buffer_usage in a structured format (none used), with the I/O times
    /// track_io_timing adds.
    fn buffer_props(&mut self, io_timing: bool) {
        for k in BUFFER_FIELDS {
            self.prop(k, "0", true);
        }
        if io_timing {
            for k in [
                "Shared I/O Read Time",
                "Shared I/O Write Time",
                "Local I/O Read Time",
                "Local I/O Write Time",
                "Temp I/O Read Time",
                "Temp I/O Write Time",
            ] {
                self.prop(k, "0.000", true);
            }
        }
    }

    fn json_line_ending(&mut self) {
        if let Some(top) = self.stack.last_mut() {
            if *top {
                self.buf.push(',');
            } else {
                *top = true;
            }
        }
        self.buf.push('\n');
    }

    fn yaml_line_starting(&mut self) {
        if let Some(top) = self.stack.last_mut()
            && !*top
        {
            *top = true;
            return;
        }
        self.buf.push('\n');
        self.spaces(self.indent * 2);
    }

    fn open(&mut self, objtype: &str, label: Option<&str>, object: bool) {
        match self.format {
            Format::Xml => {
                self.spaces(self.indent * 2);
                let _ = writeln!(self.buf, "<{}>", xml_tag(objtype));
                self.indent += 1;
            }
            Format::Json => {
                self.json_line_ending();
                self.spaces(self.indent * 2);
                if let Some(l) = label {
                    json_escape(l, &mut self.buf);
                    self.buf.push_str(": ");
                }
                self.buf.push(if object { '{' } else { '[' });
                self.stack.push(false);
                self.indent += 1;
            }
            Format::Yaml => {
                self.yaml_line_starting();
                match label {
                    Some(l) => {
                        let _ = write!(self.buf, "{l}: ");
                        self.stack.push(true);
                    }
                    None => {
                        self.buf.push_str("- ");
                        self.stack.push(false);
                    }
                }
                self.indent += 1;
            }
            Format::Text => {}
        }
    }

    fn close(&mut self, objtype: &str, object: bool) {
        match self.format {
            Format::Xml => {
                self.indent -= 1;
                self.spaces(self.indent * 2);
                let _ = writeln!(self.buf, "</{}>", xml_tag(objtype));
            }
            Format::Json => {
                self.indent -= 1;
                self.buf.push('\n');
                self.spaces(self.indent * 2);
                self.buf.push(if object { '}' } else { ']' });
                self.stack.pop();
            }
            Format::Yaml => {
                self.indent -= 1;
                self.stack.pop();
            }
            Format::Text => {}
        }
    }

    fn prop(&mut self, name: &str, value: &str, numeric: bool) {
        match self.format {
            Format::Xml => {
                self.spaces(self.indent * 2);
                let tag = xml_tag(name);
                let _ = write!(self.buf, "<{tag}>");
                xml_escape(value, &mut self.buf);
                let _ = writeln!(self.buf, "</{tag}>");
            }
            Format::Json => {
                self.json_line_ending();
                self.spaces(self.indent * 2);
                json_escape(name, &mut self.buf);
                self.buf.push_str(": ");
                if numeric {
                    self.buf.push_str(value)
                } else {
                    json_escape(value, &mut self.buf)
                }
            }
            Format::Yaml => {
                self.yaml_line_starting();
                let _ = write!(self.buf, "{name}: ");
                if numeric {
                    self.buf.push_str(value)
                } else {
                    json_escape(value, &mut self.buf)
                }
            }
            Format::Text => {
                self.spaces(self.indent * 2);
                let _ = writeln!(self.buf, "{name}: {value}");
            }
        }
    }

    fn list(&mut self, name: &str, items: &[String]) {
        match self.format {
            Format::Xml => {
                self.spaces(self.indent * 2);
                let tag = xml_tag(name);
                let _ = writeln!(self.buf, "<{tag}>");
                for i in items {
                    self.spaces(self.indent * 2 + 2);
                    self.buf.push_str("<Item>");
                    xml_escape(i, &mut self.buf);
                    self.buf.push_str("</Item>\n");
                }
                self.spaces(self.indent * 2);
                let _ = writeln!(self.buf, "</{tag}>");
            }
            Format::Json => {
                self.json_line_ending();
                self.spaces(self.indent * 2);
                json_escape(name, &mut self.buf);
                self.buf.push_str(": [");
                for (k, i) in items.iter().enumerate() {
                    if k > 0 {
                        self.buf.push_str(", ");
                    }
                    json_escape(i, &mut self.buf);
                }
                self.buf.push(']');
            }
            Format::Yaml => {
                self.yaml_line_starting();
                let _ = write!(self.buf, "{name}: ");
                for i in items {
                    self.buf.push('\n');
                    self.spaces(self.indent * 2 + 2);
                    self.buf.push_str("- ");
                    json_escape(i, &mut self.buf);
                }
            }
            Format::Text => {
                self.spaces(self.indent * 2);
                let _ = writeln!(self.buf, "{name}: {}", items.join(", "));
            }
        }
    }
}

struct Render<'r> {
    o: &'r Options,
    instr: Option<&'r Instr>,
    w: Writer,
    /// Under a Gather: the processes each loop of the nodes below runs in (its workers,
    /// and the leader when it participates).
    procs: u64,
    leader: bool,
    /// Inside a nested loop's inner side the executor ran once for all the outer rows:
    /// the scans Postgres makes of it.
    rescans: u64,
    /// A nested loop's inner Materialize about to be shown: the outer rows, each a rescan
    /// of it (its input read once).
    mat_loops: Option<u64>,
    /// The hash join settings (a Hash's batches).
    jg: super::joins::JoinGucs,
    /// track_io_timing: buffer usage comes with its I/O times.
    io_timing: bool,
}

impl Render<'_> {
    fn stats(&self, key: Option<usize>) -> Option<NodeStats> {
        let mut s = self.instr?.get(key?)?;
        s.loops *= self.procs * self.rescans;
        s.rows *= self.rescans;
        s.removed *= self.rescans;
        s.searches *= self.rescans;
        s.fetches *= self.rescans;
        s.pages *= self.rescans;
        (s.loops > 0).then_some(s)
    }

    /// The node's own actual rows and loops: an inner Materialize is rescanned for each
    /// outer row (its input, sharing its counters, read once).
    fn header_stats(&mut self, n: &ExNode) -> Option<NodeStats> {
        let mat = if n.node_type == "Materialize" { self.mat_loops.take() } else { None };
        // (A sub-partitioned table's Append or Merge Append among an ordered Append's
        // members, which the executor runs as its members: what they did.)
        if n.key.is_none() && matches!(n.node_type, "Append" | "Merge Append") {
            let kids: Vec<NodeStats> = n.children.iter().filter_map(|c| self.stats(c.key)).collect();
            let loops = kids.iter().map(|k| k.loops).max()?;
            return Some(NodeStats {
                loops,
                rows: kids.iter().map(|k| k.rows).sum(),
                ..Default::default()
            });
        }
        let s = self.stats(n.key)?;
        match mat {
            Some(0) => None,
            Some(r) if r > s.loops => Some(NodeStats {
                rows: s.rows / s.loops.max(1) * r,
                loops: r,
                ..s
            }),
            _ => Some(s),
        }
    }

    /// Render a node's children, the inner side of a nested loop the executor read once
    /// as the outer rows' many scans of it.
    fn children_of(&mut self, n: &ExNode, f: &mut dyn FnMut(&mut Self, &ExNode)) {
        let saved = self.rescans;
        for (i, c) in n.initplans.iter().chain(&n.children).chain(&n.subplans).enumerate() {
            let inner = n.inner_cached && i == n.initplans.len() + 1;
            if inner {
                let outer_rows = n.children.first().and_then(|o| self.stats(o.key)).map_or(0, |s| s.rows);
                self.rescans = saved * outer_rows;
            }
            if n.node_type == "Nested Loop" && i == n.initplans.len() + 1 && c.node_type == "Materialize" {
                let outer_rows = n.children.first().and_then(|o| self.stats(o.key)).map_or(0, |s| s.rows);
                self.mat_loops = Some(outer_rows);
            }
            f(self, c);
            self.mat_loops = None;
            self.rescans = saved;
        }
    }

    /// The process count for a node's children: a Gather's spread them.
    fn enter(&mut self, n: &ExNode) -> u64 {
        let saved = self.procs;
        if matches!(n.node_type, "Gather" | "Gather Merge") && n.workers > 0 {
            self.procs *= n.workers as u64 + u64::from(self.leader);
        }
        saved
    }

    fn removed(&self, n: &ExNode, list: &[Removed]) -> f64 {
        let Some(st) = self.stats(n.key) else {
            return 0.0;
        };
        let total: u64 = list
            .iter()
            .map(|r| match r {
                Removed::Scan(k) => self
                    .instr
                    .and_then(|i| i.get(*k))
                    .map_or(0, |s| s.removed * self.rescans),
                Removed::Diff(k) => self
                    .instr
                    .and_then(|i| i.get(*k))
                    .map_or(0, |s| (s.rows * self.rescans).saturating_sub(st.rows)),
            })
            .sum();
        (total as f64 / st.loops as f64).round()
    }

    /// Memory a node's rows take, in kB, with Postgres's small-set minimums.
    fn memory_kb(&self, n: &ExNode, min: u64) -> u64 {
        let rows = self.stats(n.key).map_or(0, |s| s.rows / s.loops.max(1));
        min.max(rows * (n.width as u64 + 40) / 1024 + 1)
    }

    fn node(&mut self, n: &ExNode) {
        if self.w.format == Format::Text {
            self.text_node(n);
        } else {
            self.structured_node(n);
        }
    }

    fn text_node(&mut self, n: &ExNode) {
        let save = self.w.indent;
        if let Some(name) = &n.subplan_name {
            self.w.spaces(self.w.indent * 2);
            self.w.buf.push_str(name);
            self.w.buf.push('\n');
            self.w.indent += 1;
        }
        if self.w.indent > 0 {
            self.w.spaces(self.w.indent * 2);
            self.w.buf.push_str("->  ");
            self.w.indent += 2;
        }
        self.w.buf.push_str(&n.title);
        if self.o.costs {
            let _ = write!(
                self.w.buf,
                "  (cost={:.2}..{:.2} rows={:.0} width={})",
                n.startup_cost, n.total_cost, n.rows, n.width
            );
        }
        if self.o.analyze {
            match self.header_stats(n) {
                Some(s) => {
                    let loops = s.loops as f64;
                    let rows = s.rows as f64 / loops;
                    if self.o.timing {
                        let _ = write!(
                            self.w.buf,
                            " (actual time={:.3}..{:.3} rows={rows:.2} loops={})",
                            s.startup * 1000.0 / loops,
                            s.total * 1000.0 / loops,
                            s.loops
                        );
                    } else {
                        let _ = write!(self.w.buf, " (actual rows={rows:.2} loops={})", s.loops);
                    }
                }
                None => self.w.buf.push_str(" (never executed)"),
            }
        }
        self.w.buf.push('\n');
        self.w.indent += 1;
        if n.disabled {
            self.w.prop("Disabled", "true", true);
        }
        self.details(n);
        let procs = self.enter(n);
        self.children_of(n, &mut |r, c| r.text_node(c));
        self.procs = procs;
        self.w.indent = save;
    }

    fn details(&mut self, n: &ExNode) {
        // (An Append's, Merge Append's or Recursive Union's target list is a dummy.)
        if !n.output.is_empty()
            && !matches!(n.node_type, "Append" | "Merge Append" | "Recursive Union")
        {
            self.w.list("Output", &n.output);
        }
        let executed = self.o.analyze && self.stats(n.key).is_some();
        let text = self.w.format == Format::Text;
        // (Text format says a join's inner side is unique only in verbose mode, and only
        // when it is.)
        if text
            && self.o.verbose
            && n.target.iter().any(|(k, v, _)| *k == "Inner Unique" && v == "true")
        {
            self.w.prop("Inner Unique", "true", true);
        } else if !text && let Some((_, v, _)) = n.target.iter().find(|t| t.0 == "Inner Unique") {
            self.w.prop("Inner Unique", v, true);
        }
        for d in &n.details {
            match d {
                Detail::Text(k, v) => self.w.prop(k, v, false),
                Detail::Workers(w) => {
                    self.w.prop("Workers Planned", &w.to_string(), true);
                    if executed {
                        self.w.prop("Workers Launched", &w.to_string(), true);
                    }
                }
                Detail::List(k, v) => self.w.list(k, v),
                Detail::Removed(k, list) => {
                    if executed {
                        let r = self.removed(n, list);
                        if r > 0.0 || !text {
                            self.w.prop(k, &format!("{r:.0}"), true);
                        }
                    }
                }
                Detail::SortInfo if executed => {
                    // (Past work_mem: an external merge, its runs on disk.)
                    let rows = self.stats(n.key).map_or(0, |s| s.rows / s.loops.max(1));
                    let spill = rows * (n.width as u64 + 40) > (self.jg.work_mem * 1024.0) as u64;
                    // (A bounded sort past twice its bound: the top-N heap.)
                    let input_rows = n
                        .children
                        .first()
                        .and_then(|k| self.stats(k.key))
                        .map_or(0, |s| s.rows / s.loops.max(1));
                    let (method, kb, space) = if n.bound.is_some_and(|b| input_rows as f64 > 2.0 * b) {
                        ("top-N heapsort", self.memory_kb(n, 25), "Memory")
                    } else if spill {
                        ("external merge", (rows * (n.width as u64 + 8)).div_ceil(1024).max(1), "Disk")
                    } else {
                        ("quicksort", self.memory_kb(n, 25), "Memory")
                    };
                    if text {
                        self.w
                            .prop("Sort Method", &format!("{method}  {space}: {kb}kB"), true);
                        // (A sort in parallel workers: each worker's own, show_sort_info's
                        // shared_info.)
                        if n.workers > 0 {
                            for i in 0..n.workers {
                                self.w.prop(
                                    &format!("Worker {i}:  Sort Method"),
                                    &format!("{method}  {space}: {kb}kB"),
                                    true,
                                );
                            }
                        }
                    } else {
                        self.w.prop("Sort Method", method, false);
                        self.w.prop("Sort Space Used", &kb.to_string(), true);
                        self.w.prop("Sort Space Type", space, false);
                    }
                }
                Detail::IncSortInfo if executed => {
                    // show_incremental_sort_info: the full sorts, then those of presorted
                    // prefix groups.
                    let st = self.stats(n.key).unwrap_or_default().incr;
                    let width = n.width as u64;
                    for (g, label) in [(0, "Full-sort"), (1, "Pre-sorted")] {
                        if st.groups[g] == 0 || st.groups[0] == 0 {
                            continue;
                        }
                        let methods: Vec<String> = [(1u8, "top-N heapsort"), (2, "quicksort")]
                            .iter()
                            .filter(|(b, _)| st.methods[g] & b != 0)
                            .map(|(_, m)| m.to_string())
                            .collect();
                        let kb = |kept: u64, sorts: u64| (kept * (width + 40)).div_ceil(1024) + 25 * sorts;
                        let peak = kb(st.kept_max[g], 1);
                        let avg = kb(st.kept_total[g], st.groups[g]) / st.groups[g];
                        let name = format!("{label} Groups");
                        if text {
                            let s = if methods.len() > 1 { "s" } else { "" };
                            self.w.prop(
                                &name,
                                &format!(
                                    "{}  Sort Method{s}: {}  Average Memory: {avg}kB  Peak Memory: {peak}kB",
                                    st.groups[g],
                                    methods.join(", ")
                                ),
                                true,
                            );
                        } else {
                            self.w.open("Incremental Sort Groups", Some(&name), true);
                            self.w.prop("Group Count", &st.groups[g].to_string(), true);
                            self.w.list("Sort Methods Used", &methods);
                            self.w.open("Sort Space", Some("Sort Space Memory"), true);
                            self.w.prop("Average Sort Space Used", &avg.to_string(), true);
                            self.w.prop("Peak Sort Space Used", &peak.to_string(), true);
                            self.w.close("Sort Space", true);
                            self.w.close("Incremental Sort Groups", true);
                        }
                    }
                }
                Detail::MemoizeInfo if executed => {
                    let st = self.stats(n.key).unwrap_or_default();
                    if st.cache_misses > 0 {
                        let kb = st.cache_bytes.div_ceil(1024);
                        if text {
                            self.w.prop(
                                "Hits",
                                &format!(
                                    "{}  Misses: {}  Evictions: {}  Overflows: 0  Memory Usage: {kb}kB",
                                    st.cache_hits, st.cache_misses, st.cache_evictions
                                ),
                                true,
                            );
                        } else {
                            self.w.prop("Cache Hits", &st.cache_hits.to_string(), true);
                            self.w.prop("Cache Misses", &st.cache_misses.to_string(), true);
                            self.w.prop("Cache Evictions", &st.cache_evictions.to_string(), true);
                            self.w.prop("Cache Overflows", "0", true);
                            self.w.prop("Peak Memory Usage", &kb.to_string(), true);
                        }
                    }
                }
                Detail::HashAggInfo => {
                    if !text && self.o.costs {
                        self.w.prop("Planned Partitions", "0", true);
                    }
                    if executed {
                        let kb = self.memory_kb(n, 32);
                        if text {
                            self.w
                                .prop("Batches", &format!("1  Memory Usage: {kb}kB"), true);
                        } else {
                            self.w.prop("HashAgg Batches", "1", true);
                            self.w.prop("Peak Memory Usage", &kb.to_string(), true);
                            self.w.prop("Disk Usage", "0", true);
                        }
                    }
                }
                Detail::HashInfo if executed => {
                    let st = self.stats(n.key).unwrap_or_default();
                    // (Per loop of the join, all the processes' rows together.)
                    let raw = self.instr.and_then(|i| n.key.and_then(|k| i.get(k))).unwrap_or_default();
                    let loops = raw.loops.max(1);
                    let rows = raw.rows / loops;
                    let _ = &st;
                    // ExecChooseHashTableSize over the estimate (a Parallel Hash's: all the
                    // processes' rows, in their combined memory), then the batches the
                    // rows it got needed (ExecHashIncreaseNumBatches: doubling while the
                    // first batch's share is over the memory, until a split moves none).
                    // (Its space_allowed: hash_mem, doubled with each halving of the batches
                    // ExecChooseHashTableSize makes.)
                    let (nb0, nbatch0, space) = if n.parallel_aware {
                        // (The plan's rows_total: the partial rows times the parallel
                        // divisor (get_parallel_divisor), as the planner costed it; the
                        // combined hash_mem of the participants when the table fits it.)
                        let w = n.children.first().map_or(0, |c| c.workers);
                        let wf = w as f64;
                        let lead = if self.leader { (1.0 - 0.3 * wf).max(0.0) } else { 0.0 };
                        let est = n.rows * (wf + lead).max(1.0);
                        super::joins::parallel_hash_table_space(est, n.width, &self.jg, w)
                    } else {
                        super::joins::hash_table_space(n.rows, n.width, &self.jg)
                    };
                    // (The rows' size in the table: as the executor hashed them, else the
                    // planner's width.)
                    // (HJTUPLE_OVERHEAD, then the MinimalTuple: its header and the values,
                    // MAXALIGNed.)
                    let tupsize = if raw.hash_rows > 0 && !n.hash_cols.is_empty() {
                        let data: f64 = n
                            .hash_cols
                            .iter()
                            .map(|&c| raw.hash_col_bytes.get(c).copied().unwrap_or(0) as f64 / raw.hash_rows as f64)
                            .sum();
                        16.0 + ((16.0 + data) / 8.0).ceil() * 8.0
                    } else {
                        (32 + (n.width as u64).div_ceil(8) * 8) as f64
                    };
                    let max_group = (st.hash_max_group / loops).max(1) as f64;
                    let mut nbatch = nbatch0 as u64;
                    let mut in_mem = (rows as f64 / nbatch as f64).max(max_group.min(rows as f64));
                    let mut space = space;
                    // (A Parallel Hash's participants: the Gather's workers and the leader.)
                    let participants = n.children.first().map_or(0, |c| c.workers) as u64 + 1;
                    let mut regrown_buckets: Option<u64> = None;
                    // (A Parallel Hash lets each participant allocate one chunk whatever its
                    // size: a batch of no more tuples than participants never grows.)
                    while in_mem * tupsize + nb0 as f64 * 8.0 > space
                        && !(n.parallel_aware && nbatch > 1 && in_mem <= participants as f64)
                    {
                        // ExecParallelHashIncreaseNumBatches: from one batch (the combined
                        // hash_mem wasn't enough) to two per participant, with one
                        // participant's hash_mem and a bucket array for the tuples expected
                        // per batch; else doubling.
                        let next = if n.parallel_aware && nbatch == 1 {
                            space = (self.jg.work_mem * self.jg.hash_mem_multiplier * 1024.0).floor();
                            let next = (participants * 2).next_power_of_two();
                            let dbuckets = (rows as f64 * 2.0 / next as f64).ceil() as u64;
                            regrown_buckets = Some(dbuckets.max(1024).next_power_of_two());
                            next
                        } else {
                            nbatch * 2
                        };
                        let now = (rows as f64 / next as f64).max(max_group.min(rows as f64));
                        nbatch = next;
                        // (A split that moves no tuple out of the batch disables growth.)
                        if now >= in_mem {
                            break;
                        }
                        in_mem = now;
                    }
                    let nb = if nbatch == 1 {
                        (nb0 as u64).max(rows.next_power_of_two())
                    } else {
                        regrown_buckets.unwrap_or(nb0 as u64)
                    };
                    let kb = self.memory_kb(n, 9);
                    if text {
                        let v = if nb != nb0 as u64 || nbatch != nbatch0 as u64 {
                            format!(
                                "{nb} (originally {nb0})  Batches: {nbatch} (originally {nbatch0})  Memory Usage: {kb}kB"
                            )
                        } else {
                            format!("{nb}  Batches: {nbatch}  Memory Usage: {kb}kB")
                        };
                        self.w.prop("Buckets", &v, true);
                    } else {
                        for (k, v) in [
                            ("Hash Buckets", nb),
                            ("Original Hash Buckets", nb0 as u64),
                            ("Hash Batches", nbatch),
                            ("Original Hash Batches", nbatch0 as u64),
                            ("Peak Memory Usage", kb),
                        ] {
                            self.w.prop(k, &v.to_string(), true);
                        }
                    }
                }
                Detail::IndexSearches(k) if self.o.analyze => {
                    let v = self.instr.and_then(|i| i.get(*k)).map_or(0, |s| s.searches);
                    self.w.prop("Index Searches", &v.to_string(), true);
                }
                Detail::HeapFetches(k) if self.o.analyze => {
                    let v = self.instr.and_then(|i| i.get(*k)).map_or(0, |s| s.fetches);
                    let loops = self.stats(n.key).map_or(1, |s| s.loops.max(1));
                    let v = (v as f64 / loops as f64).round();
                    self.w.prop("Heap Fetches", &format!("{v:.0}"), true);
                }
                Detail::MergeTuples(k) if self.o.analyze => {
                    let s = self.instr.and_then(|i| i.get(*k)).unwrap_or_default();
                    let [ins, upd, del] = s.merged;
                    let skipped = s.rows.saturating_sub(ins + upd + del);
                    if text {
                        if s.rows > 0 {
                            let mut line = String::new();
                            for (k, v) in [
                                ("inserted", ins),
                                ("updated", upd),
                                ("deleted", del),
                                ("skipped", skipped),
                            ] {
                                if v > 0 {
                                    let _ = write!(line, " {k}={v}");
                                }
                            }
                            self.w.spaces(self.w.indent * 2);
                            let _ = writeln!(self.w.buf, "Tuples:{line}");
                        }
                    } else {
                        for (k, v) in [
                            ("Tuples Inserted", ins),
                            ("Tuples Updated", upd),
                            ("Tuples Deleted", del),
                            ("Tuples Skipped", skipped),
                        ] {
                            self.w.prop(k, &format!("{v:.0}"), true);
                        }
                    }
                }
                Detail::HeapBlocks(k) if self.o.analyze => {
                    let v = self.instr.and_then(|i| i.get(*k)).map_or(0, |s| s.pages);
                    if text {
                        if v > 0 {
                            self.w.prop("Heap Blocks", &format!("exact={v}"), true);
                        }
                    } else {
                        self.w.prop("Exact Heap Blocks", &v.to_string(), true);
                        self.w.prop("Lossy Heap Blocks", "0", true);
                    }
                }
                Detail::Targets(list) if text => {
                    for (title, _) in list {
                        self.w.spaces(self.w.indent * 2);
                        self.w.buf.push_str(title);
                        self.w.buf.push('\n');
                    }
                }
                Detail::Targets(list) => {
                    self.w.open("Target Tables", Some("Target Tables"), false);
                    for (_, props) in list {
                        self.w.open("Target Table", None, true);
                        for (k, v, numeric) in props {
                            self.w.prop(k, v, *numeric);
                        }
                        self.w.close("Target Table", true);
                    }
                    self.w.close("Target Tables", false);
                }
                Detail::SortInfo
                | Detail::IncSortInfo
                | Detail::HashInfo
                | Detail::IndexSearches(_)
                | Detail::HeapFetches(_)
                | Detail::HeapBlocks(_)
                | Detail::MergeTuples(_)
                | Detail::GroupSort(_)
                | Detail::MemoizeInfo => {}
            }
        }
        // A tuplestore's storage (show_windowagg_info / show_material_info): in memory,
        // or on disk past work_mem.
        if executed && matches!(n.node_type, "WindowAgg" | "Materialize") {
            let rows = self.stats(n.key).map_or(0, |s| s.rows / s.loops.max(1));
            let bytes = rows * (n.width as u64 + 40);
            let disk = bytes > (self.jg.work_mem * 1024.0) as u64;
            let kb = if disk {
                (rows * (n.width as u64 + 8)).div_ceil(1024).max(1)
            } else {
                17.max(bytes / 1024 + 9)
            };
            let kind = if disk { "Disk" } else { "Memory" };
            if text {
                self.w.prop("Storage", &format!("{kind}  Maximum Storage: {kb}kB"), true);
            } else {
                self.w.prop("Storage", kind, false);
                self.w.prop("Maximum Storage", &kb.to_string(), true);
            }
        }
        if self.o.buffers && !text {
            self.w.buffer_props(self.io_timing);
        }
    }

    fn structured_node(&mut self, n: &ExNode) {
        self.w.open(
            "Plan",
            if n.relationship.is_none() {
                Some("Plan")
            } else {
                None
            },
            true,
        );
        self.w.prop("Node Type", n.node_type, false);
        if let Some(s) = n.strategy {
            self.w.prop("Strategy", s, false);
        }
        if let Some(s) = n.partial_mode {
            self.w.prop("Partial Mode", s, false);
        }
        if let Some(s) = n.operation {
            self.w.prop("Operation", s, false);
        }
        if let Some(s) = n.relationship {
            self.w.prop("Parent Relationship", s, false);
        }
        if let Some(s) = &n.subplan_name {
            self.w.prop("Subplan Name", s, false);
        }
        self.w.prop(
            "Parallel Aware",
            if n.parallel_aware { "true" } else { "false" },
            true,
        );
        self.w.prop("Async Capable", "false", true);
        // ("Inner Unique" comes after the target list: see `details`.)
        for (k, v, numeric) in n.target.iter().filter(|t| t.0 != "Inner Unique") {
            self.w.prop(k, v, *numeric);
        }
        if self.o.costs {
            self.w
                .prop("Startup Cost", &format!("{:.2}", n.startup_cost), true);
            self.w
                .prop("Total Cost", &format!("{:.2}", n.total_cost), true);
            self.w.prop("Plan Rows", &format!("{:.0}", n.rows), true);
            self.w.prop("Plan Width", &n.width.to_string(), true);
        }
        if self.o.analyze {
            let s = self.header_stats(n).unwrap_or_default();
            let loops = s.loops.max(1) as f64;
            if self.o.timing {
                self.w.prop(
                    "Actual Startup Time",
                    &format!("{:.3}", s.startup * 1000.0 / loops),
                    true,
                );
                self.w.prop(
                    "Actual Total Time",
                    &format!("{:.3}", s.total * 1000.0 / loops),
                    true,
                );
            }
            self.w.prop(
                "Actual Rows",
                &format!("{:.2}", s.rows as f64 / loops),
                true,
            );
            self.w.prop("Actual Loops", &s.loops.to_string(), true);
        }
        self.w
            .prop("Disabled", if n.disabled { "true" } else { "false" }, true);
        self.details(n);
        let kids: Vec<&ExNode> = n
            .initplans
            .iter()
            .chain(&n.children)
            .chain(&n.subplans)
            .collect();
        if !kids.is_empty() {
            self.w.open("Plans", Some("Plans"), false);
            let procs = self.enter(n);
            drop(kids);
            self.children_of(n, &mut |r, c| r.structured_node(c));
            self.procs = procs;
            self.w.close("Plans", false);
        }
        self.w.close("Plan", true);
    }
}

/// EXPLAIN's options for the planner's estimates: costs only.
fn estimate_options() -> Options {
    Options {
        analyze: false,
        verbose: false,
        costs: true,
        buffers: false,
        wal: false,
        settings: false,
        generic_plan: false,
        scroll: false,
        timing: false,
        summary: false,
        memory: false,
        serialize: false,
        serialize_binary: false,
        format: Format::Text,
    }
}

/// A builder making `plan`'s estimates.
fn estimate_builder<'c, 'a>(plan: &Arc<Plan>, ctx: &'c ExecCtx<'a>, o: &'c Options) -> Builder<'c, 'a> {
    let (refnames, _) = assign_refnames(plan);
    Builder {
        ctx,
        o,
        prefix: true,
        refnames,
        labels: HashMap::new(),
        plan_ids: 0,
        outer: Vec::new(),
        outer_depth: Vec::new(),
        depth: 0,
        pending: Vec::new(),
        inits: Vec::new(),
        params: &[],
        ctes: HashMap::new(),
        needed: needed_masks(plan),
        num_exec: None,
        top: 0,
        par: super::paths::parallel::ParCfg::read(ctx).enabled(),
        member_scan: false,
        group_order: None,
        group_order_full: false,
        group_named: Vec::new(),
        whole_vars: HashMap::new(),
        unplanned_cols: Default::default(),
        sub_alias: None,
        json_casts: Vec::new(),
        proj_depth: 0,
        level_tops: std::collections::HashSet::new(),
        group_tops: std::collections::HashSet::new(),
        json_aggs: HashMap::new(),
        gtl: Default::default(),
        carried: HashMap::new(),
        renumber: None,
        events: Vec::new(),
        lvl_stack: Vec::new(),
        lvl_next: 0,
        spans: Vec::new(),
        key_sub_ids: Default::default(),
        key_subs: HashMap::new(),
        phantoms: HashMap::new(),
        having: None,
        pw_aggs: Vec::new(),
        pw_partial: false,
        join_force: None,
        order_hint: None,
        agg_key: None,
        agg_input: None,
        unique_sides: Default::default(),
        plain_distinct: None,
        shown: shown_subqueries(plan, &needed_masks(plan)),
        rowmarks: Default::default(),
        rowmark_junk: Default::default(),
        copy_marks: Default::default(),
        plain_consts: Default::default(),
        alts: Default::default(),
        sub_params: Vec::new(),
        detach: false,
        unattached: HashMap::new(),
        fresh_hashed: Default::default(),
        dummy_target: None,
    }
}

/// A sort key's direction from the suffix EXPLAIN shows after it (" DESC", " NULLS
/// FIRST", …): (descending, NULLS FIRST).
fn sort_dir(sfx: &str) -> (bool, bool) {
    let desc = sfx.contains(" DESC");
    let nulls_first = if sfx.contains("NULLS FIRST") {
        true
    } else if sfx.contains("NULLS LAST") {
        false
    } else {
        desc
    };
    (desc, nulls_first)
}

/// A plan's estimates as EXPLAIN makes them (the planner sizes FROM items it can't plan
/// itself this way).
pub struct PlanEstimate {
    pub rows: f64,
    pub startup: f64,
    pub total: f64,
    pub width: u32,
    /// The top node's first child's row width (a hashed aggregation's input width).
    pub input_width: u32,
    /// The top node's first child's type ("Sort", …; "": none).
    pub input_type: &'static str,
    /// A sorted grouping on top: the group keys its input is sorted on (`ExNode::group_sort`).
    pub group_sort: Option<Vec<(usize, bool, bool)>>,
    /// The top node's type.
    pub node_type: &'static str,
    /// An Incremental Sort on top: its presorted keys.
    pub presorted: usize,
    /// Per output column: its width, the base column it is, a known distinct count.
    pub cols: Vec<(u32, Option<costs::VarRef>, Option<f64>)>,
    /// Its disabled nodes (add_path compares them before costs).
    pub disabled: usize,
}

/// cost_subplan for the sub-selects in `exprs`: (startup, per evaluation). A hashed one
/// builds its table once; a correlated one runs each time (an EXISTS to its first row).
pub(crate) fn subplan_costs(exprs: &[Expr], ctx: &ExecCtx) -> (f64, f64) {
    use crate::sql::expr::SubqueryKind as K;
    let (mut startup, mut per) = (0.0, 0.0);
    for x in exprs {
        x.walk(&mut |e| {
            let Expr::Subquery(s) = e else { return };
            // (A function call is costed as one: its procost.)
            if s.call.is_some() {
                return;
            }
            let correlated = !s.params.is_empty();
            let hashed = !correlated && s.kind == K::Any && s.ops.iter().all(|o| o.name == "=");
            let ops = s.ops.len() as f64 * costs::CPU_OPERATOR_COST;
            if hashed {
                let est = estimate_subquery(&s.plan, ctx);
                startup += costs::CPU_OPERATOR_COST.mul_add(est.rows, est.total);
                per += ops;
            } else if correlated {
                let est = estimate_subquery(&s.plan, ctx);
                let run = est.total - est.startup;
                per += ops
                    + match s.kind {
                        K::Exists => run / costs::clamp_rows(est.rows),
                        K::Any | K::All => 0.5 * run + 0.5 * est.rows * costs::CPU_OPERATOR_COST,
                        _ => run,
                    }
                    + est.startup;
            }
        });
    }
    (startup, per)
}

/// An index condition of a system catalog scan: a clause as it is, or a pattern's prefix
/// bound on a column (`col op 'value'::text`).
enum SysCond {
    Expr(Expr),
    Bound(usize, &'static str, String),
}

fn quote_literal(v: &str) -> String {
    format!("'{}'", v.replace('\'', "''"))
}

/// The catalog index a system catalog scan's quals can use (the most leading key columns
/// matched; a prefix pattern bounds its column, lossily): its name, the index
/// conditions, and the quals left to filter.
fn sys_index_scan(
    table: &str,
    filter: &Expr,
    cols: &[(String, DataType)],
) -> Option<(String, Vec<SysCond>, Vec<Expr>)> {
    static INDEXES: &str = include_str!("../catalog/sys_indexes.tsv");
    let mut quals = Vec::new();
    conjuncts(filter, &mut quals);
    let col_of = |e: &Expr| -> Option<usize> {
        match e {
            Expr::Col(i, _) => Some(*i),
            Expr::Relabel(x, _) | Expr::Cast(x, _, _, false) => match &**x {
                Expr::Col(i, _) => Some(*i),
                _ => None,
            },
            _ => None,
        }
    };
    let constant = |e: &Expr| match e {
        Expr::Const(..) => true,
        Expr::Cast(x, ..) | Expr::Relabel(x, _) => matches!(&**x, Expr::Const(..)),
        _ => false,
    };
    // (conditions, quals used whole, quals kept as filters too, matched columns)
    let mut best: Option<(String, Vec<SysCond>, Vec<usize>, Vec<usize>, usize)> = None;
    for line in INDEXES.lines().filter(|l| !l.starts_with('#')) {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 3 || f[0] != table {
            continue;
        }
        let mut conds = Vec::new();
        let (mut used, mut lossy) = (Vec::new(), Vec::new());
        let mut matched = 0;
        for key in f[2].split(',') {
            let Some(col) = cols.iter().position(|(n, _)| n == key) else {
                break;
            };
            let mut eq = false;
            let mut any = false;
            for (qi, q) in quals.iter().enumerate() {
                if used.contains(&qi) || lossy.contains(&qi) {
                    continue;
                }
                let Expr::Call(d, args, _) = q else { continue };
                if args.len() != 2 {
                    continue;
                }
                let on_col = col_of(&args[0]) == Some(col) && constant(&args[1])
                    || col_of(&args[1]) == Some(col) && constant(&args[0]);
                match d.name {
                    "=" if on_col => {
                        conds.push(SysCond::Expr(q.clone()));
                        used.push(qi);
                        eq = true;
                        any = true;
                    }
                    "<" | "<=" | ">" | ">=" if on_col => {
                        conds.push(SysCond::Expr(q.clone()));
                        used.push(qi);
                        any = true;
                    }
                    "~" | "~~" if col_of(&args[0]) == Some(col) => {
                        // (A byte-ordered collation on the pattern: the catalog's "C" index
                        // still serves it, match_pattern_prefix.)
                        let mut pe = &args[1];
                        while let Expr::Collate(x, c, _) = pe
                            && matches!(c.as_str(), "C" | "POSIX" | "default")
                        {
                            pe = x;
                        }
                        let Expr::Const(crate::types::Value::Text(patt), _) = pe else {
                            continue;
                        };
                        let ptype = if d.name == "~" {
                            costs::Ptype::Regex
                        } else {
                            costs::Ptype::Like
                        };
                        let (status, prefix, _) = costs::pattern_prefix(patt, ptype);
                        match status {
                            costs::PrefixStatus::None => continue,
                            costs::PrefixStatus::Exact => {
                                conds.push(SysCond::Bound(col, "=", prefix));
                                eq = true;
                            }
                            costs::PrefixStatus::Partial => {
                                conds.push(SysCond::Bound(col, ">=", prefix.clone()));
                                if let Some(g) = costs::greater_string(&prefix) {
                                    conds.push(SysCond::Bound(col, "<", g));
                                }
                            }
                        }
                        lossy.push(qi);
                        any = true;
                    }
                    _ => {}
                }
            }
            if !any {
                break;
            }
            matched += 1;
            if !eq {
                break;
            }
        }
        if matched > 0 && best.as_ref().is_none_or(|b| matched > b.4) {
            best = Some((f[1].to_string(), conds, used, lossy, matched));
        }
    }
    let (index, conds, used, _, _) = best?;
    let rest = quals
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !used.contains(i))
        .map(|(_, q)| q)
        .collect();
    Some((index, conds, rest))
}

/// convert_EXISTS_to_ANY on a planned correlated EXISTS over one table: its correlated
/// hashable equalities `inner = outer` leave the scan's quals and their inner sides become
/// the hashed subplan's columns. Returns that plan and, per column, the outer side (over
/// `Outer(1, _)`) and the operator.
pub(crate) fn exists_hashed(plan: &Plan) -> Option<(Plan, Vec<(Expr, Arc<crate::sql::func::FuncDef>)>)> {
    exists_hashed_from(plan, false)
}

/// `exists_hashed`, also for a subquery whose scan reads an index by its correlated
/// values (`any_access`): the hashed plan's scan is then left for the planner.
pub(crate) fn exists_hashed_from(
    plan: &Plan,
    any_access: bool,
) -> Option<(Plan, Vec<(Expr, Arc<crate::sql::func::FuncDef>)>)> {
    use super::plan::Access;
    let outer = |e: &Expr| {
        let mut found = false;
        e.walk(&mut |x| {
            if matches!(x, Expr::Outer(1, ..)) {
                found = true;
            }
        });
        found
    };
    let mut p = plan;
    loop {
        match p {
            Plan::Project { input, exprs } if !exprs.iter().any(&outer) => p = input,
            Plan::Sort { input, .. } => p = input,
            // (simplify_EXISTS_query: a positive constant LIMIT, or LIMIT NULL, goes; LIMIT
            // 0 keeps the subplan.)
            Plan::Limit {
                input,
                limit: Some(Expr::Const(v, _)),
                offset: None,
                with_ties: None,
            } if match v {
                crate::types::Value::Null => true,
                crate::types::Value::Int8(n) => *n > 0,
                crate::types::Value::Int4(n) => *n > 0,
                _ => false,
            } =>
            {
                p = input
            }
            _ => break,
        }
    }
    let mut preds: Vec<Expr> = Vec::new();
    while let Plan::Filter { input, pred } = p {
        conjuncts(pred, &mut preds);
        p = input;
    }
    // An inheritance tree: each member's the same way, the same correlated keys.
    if let Plan::Append { inputs, inh: inh @ Some(_), .. } = p
        && preds.is_empty()
        && !inputs.is_empty()
    {
        let mut members = Vec::new();
        let mut keys0: Option<Vec<(Expr, Arc<crate::sql::func::FuncDef>)>> = None;
        for m in inputs {
            let (hp, keys) = exists_hashed_from(m, any_access)?;
            match &keys0 {
                None => keys0 = Some(keys),
                Some(k0) => {
                    if k0.len() != keys.len() || k0.iter().zip(&keys).any(|(a, b)| !a.0.same_as(&b.0) || a.1.name != b.1.name) {
                        return None;
                    }
                }
            }
            members.push(Arc::new(hp));
        }
        return Some((Plan::Append { inputs: members, inh: inh.clone(), alias: None, pw: None }, keys0?));
    }
    let Plan::Scan(sc) = p else {
        return None;
    };
    if (!any_access && (sc.index.is_some() || !matches!(sc.access, Access::Unplanned | Access::Seq)))
        || sc.sample.is_some()
        || sc.mark.is_some()
        || sc.current_of.is_some()
    {
        return None;
    }
    if let Some(f) = &sc.filter {
        conjuncts(f, &mut preds);
    }
    let mut keys: Vec<(Expr, Arc<crate::sql::func::FuncDef>)> = Vec::new();
    let mut inner: Vec<Expr> = Vec::new();
    let mut rest: Vec<Expr> = Vec::new();
    for q in preds {
        let mut sub = false;
        q.walk(&mut |x| sub |= matches!(x, Expr::Subquery(_)));
        if sub || q.is_volatile() {
            return None;
        }
        if !outer(&q) {
            rest.push(q);
            continue;
        }
        let Expr::Call(d, args, _) = &q else {
            return None;
        };
        if d.name != "=" || args.len() != 2 || !hashable_key(&args[0].ty()) {
            return None;
        }
        let (lo, ro) = (outer(&args[0]), outer(&args[1]));
        let (o, i) = if lo && !ro && args[0].columns().is_empty() {
            (&args[0], &args[1])
        } else if ro && !lo && args[1].columns().is_empty() {
            (&args[1], &args[0])
        } else {
            return None;
        };
        keys.push((o.clone(), d.clone()));
        inner.push(i.clone());
    }
    if keys.is_empty() {
        return None;
    }
    let mut scan = (**sc).clone();
    scan.filter = match rest.len() {
        0 => None,
        1 => rest.pop(),
        _ => Some(Expr::And(rest)),
    };
    if !matches!(scan.access, Access::Unplanned | Access::Seq) {
        scan.access = Access::Unplanned;
        scan.index = None;
        scan.reverse = false;
    }
    Some((
        Plan::Project {
            input: Arc::new(Plan::Scan(Box::new(scan))),
            exprs: inner,
        },
        keys,
    ))
}

/// A correlated scalar subquery `SELECT exprs FROM t WHERE t.k = outer AND …` over a
/// sequential scan: the scan with its own quals, projecting the key columns then the
/// select list; the correlated keys; their number.
pub(crate) fn scalar_hashed(plan: &Plan) -> Option<(Plan, Vec<(Expr, Arc<crate::sql::func::FuncDef>)>, usize)> {
    let Plan::Project { input, exprs } = plan else {
        return None;
    };
    // (Plain columns only: computing anything for rows no outer row reaches could fail
    // where the per-row subquery wouldn't.)
    if !exprs.iter().all(|e| matches!(e, Expr::Col(..) | Expr::Const(..))) {
        return None;
    }
    // Only filters and the scan below (no sort, limit or aggregate).
    let mut p: &Plan = input;
    while let Plan::Filter { input, .. } = p {
        p = input;
    }
    if !matches!(p, Plan::Scan(_)) {
        return None;
    }
    let (keyed, keys) = exists_hashed(input)?;
    let Plan::Project { input: scan, exprs: key_exprs } = keyed else {
        return None;
    };
    // The select list over the scan's row: through the filters, the same row.
    let n = key_exprs.len();
    let mut all = key_exprs;
    all.extend(exprs.iter().cloned());
    Some((Plan::Project { input: scan, exprs: all }, keys, n))
}

/// The scan under an EXISTS subquery's projections, sorts, limits and filters.
fn scan_below(p: &Plan) -> Option<&Plan> {
    match p {
        Plan::Project { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Filter { input, .. } => scan_below(input),
        Plan::Scan(_) => Some(p),
        _ => None,
    }
}

/// Whether a set operation's columns of `types` hash (`Catalog::type_hashable`; a column
/// whose type the plan doesn't say is taken to).
fn all_hashable(ctx: &ExecCtx, types: &[DataType]) -> bool {
    let cat = ctx.catalog();
    types.iter().all(|t| *t == DataType::Unknown || cat.type_hashable(t))
}

/// A set operation's column type whose values sort (a btree operator class).
fn setop_sortable(t: &DataType) -> bool {
    match t {
        DataType::Array(e) => setop_sortable(e),
        DataType::Row(rt) => rt.fields.iter().all(|(_, f)| setop_sortable(f)),
        t => {
            let name = t.display();
            let base = name.split('(').next().unwrap_or("").trim().to_string();
            !matches!(
                base.as_str(),
                "json" | "xml" | "point" | "lseg" | "path" | "box" | "polygon" | "line" | "circle"
                    | "xid" | "cid" | "aclitem"
            )
        }
    }
}

/// A type whose `=` hashes (it has a hash operator class).
fn hashable_key(t: &DataType) -> bool {
    // An extension's native type: when its script gives it a hash operator class.
    if let DataType::Base(b) = t
        && b.ext != 0
    {
        return crate::ext::types::get(b.ext).is_some_and(|x| x.hash);
    }
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
            | DataType::Money
    )
}

/// Whether running `p` calls a volatile function (or a subquery).
pub fn plan_volatile(p: &Plan) -> bool {
    plan_volatile_by(p, &|e| e.is_volatile())
}

/// `plan_volatile` with its sub-selects counted by their plans' functions
/// (`expr_volatile_deep`).
pub fn plan_volatile_deep(p: &Plan) -> bool {
    plan_volatile_by(p, &expr_volatile_deep)
}

/// contain_volatile_functions over a planned expression: a SubPlan counts by its
/// plan's functions, not as volatile itself.
pub fn expr_volatile_deep(e: &Expr) -> bool {
    let mut v = false;
    e.walk(&mut |x| match x {
        Expr::Call(d, ..) if d.volatility == 'v' => v = true,
        Expr::Subquery(s) => match s.func {
            Some(c) => v |= c == 'v',
            None => v |= plan_volatile_by(&s.plan, &expr_volatile_deep),
        },
        _ => {}
    });
    v
}

fn plan_volatile_by(p: &Plan, vol: &dyn Fn(&Expr) -> bool) -> bool {
    let mut es = Vec::new();
    node_exprs(p, &mut es);
    if es.iter().any(|e| vol(e)) {
        return true;
    }
    let srf_volatile = |calls: &[super::plan::SrfCall]| {
        calls
            .iter()
            .any(|c| c.func.volatility == 'v' || c.body.as_deref().is_some_and(|b| plan_volatile_by(b, vol)))
    };
    match p {
        Plan::FunctionScan { calls, .. } | Plan::ProjectSet { srfs: calls, .. }
            if srf_volatile(calls) =>
        {
            return true;
        }
        Plan::PlCall { func, .. } if func.volatility == 'v' => return true,
        Plan::Insert(_) | Plan::Update(_) | Plan::Delete(_) | Plan::Merge(_) => return true,
        _ => {}
    }
    plan_inputs(p).iter().any(|c| plan_volatile_by(c, vol))
}

/// A hashed INTERSECT / EXCEPT as the planner arranges it: whether the inputs swap
/// places (INTERSECT puts the one with fewer groups first) and the groups its table is
/// sized for.
pub fn setop_shape(left: &Arc<Plan>, right: &Arc<Plan>, intersect: bool, ctx: &ExecCtx) -> (bool, f64) {
    let o = Options {
        analyze: false,
        verbose: false,
        costs: true,
        buffers: false,
        wal: false,
        settings: false,
        generic_plan: false,
        scroll: false,
        timing: false,
        summary: false,
        memory: false,
        serialize: false,
        serialize_binary: false,
        format: Format::Text,
    };
    let groups = |p: &Arc<Plan>| {
        let (refnames, _) = assign_refnames(p);
        let mut b = Builder {
            ctx,
            o: &o,
            prefix: true,
            refnames,
            labels: HashMap::new(),
            plan_ids: 0,
            outer: Vec::new(),
            outer_depth: Vec::new(),
            depth: 0,
            pending: Vec::new(),
            inits: Vec::new(),
            params: &[],
            ctes: HashMap::new(),
            needed: needed_masks(p),
            num_exec: None,
            top: 0,
            par: super::paths::parallel::ParCfg::read(ctx).enabled(),
            member_scan: false,
            group_order: None,
        group_order_full: false,
        group_named: Vec::new(),
        whole_vars: HashMap::new(),
        unplanned_cols: Default::default(),
        sub_alias: None,
        json_casts: Vec::new(),
        proj_depth: 0,
        level_tops: std::collections::HashSet::new(),
        group_tops: std::collections::HashSet::new(),
        json_aggs: HashMap::new(),
        gtl: Default::default(),
        carried: HashMap::new(),
        renumber: None,
        events: Vec::new(),
        lvl_stack: Vec::new(),
        lvl_next: 0,
        spans: Vec::new(),
        key_sub_ids: Default::default(),
        key_subs: HashMap::new(),
        phantoms: HashMap::new(),
        having: None,
        pw_aggs: Vec::new(),
        pw_partial: false,
        join_force: None,
        order_hint: None,
        agg_key: None,
        agg_input: None,
        unique_sides: Default::default(),
        plain_distinct: None,
        shown: shown_subqueries(p, &needed_masks(p)),
            rowmarks: Default::default(),
            rowmark_junk: Default::default(),
            copy_marks: Default::default(),
            plain_consts: Default::default(),
        alts: Default::default(),
        sub_params: Vec::new(),
        detach: false,
        unattached: HashMap::new(),
        fresh_hashed: Default::default(),
        dummy_target: None,
        };
        let n = b.level(p);
        b.num_groups(&n.cols, n.rows)
    };
    let lg = groups(left);
    if !intersect {
        return (false, lg);
    }
    let rg = groups(right);
    if lg > rg { (true, rg) } else { (false, lg) }
}

/// estimate_num_groups for each grouping set of an aggregation over `input` (the
/// planner sizes each set's hash table by it).
pub fn set_group_estimates(
    input: &Arc<Plan>,
    groups: &[Expr],
    sets: &[Vec<usize>],
    ctx: &ExecCtx,
) -> Vec<f64> {
    let o = Options {
        analyze: false,
        verbose: false,
        costs: true,
        buffers: false,
        wal: false,
        settings: false,
        generic_plan: false,
        scroll: false,
        timing: false,
        summary: false,
        memory: false,
        serialize: false,
        serialize_binary: false,
        format: Format::Text,
    };
    let (refnames, _) = assign_refnames(input);
    let mut b = Builder {
        ctx,
        o: &o,
        prefix: true,
        refnames,
        labels: HashMap::new(),
        plan_ids: 0,
        outer: Vec::new(),
        outer_depth: Vec::new(),
        depth: 0,
        pending: Vec::new(),
        inits: Vec::new(),
        params: &[],
        ctes: HashMap::new(),
        needed: needed_masks(input),
        num_exec: None,
        top: 0,
        par: super::paths::parallel::ParCfg::read(ctx).enabled(),
        member_scan: false,
        group_order: None,
        group_order_full: false,
        group_named: Vec::new(),
        whole_vars: HashMap::new(),
        unplanned_cols: Default::default(),
        sub_alias: None,
        json_casts: Vec::new(),
        proj_depth: 0,
        level_tops: std::collections::HashSet::new(),
        group_tops: std::collections::HashSet::new(),
        json_aggs: HashMap::new(),
        gtl: Default::default(),
        carried: HashMap::new(),
        renumber: None,
        events: Vec::new(),
        lvl_stack: Vec::new(),
        lvl_next: 0,
        spans: Vec::new(),
        key_sub_ids: Default::default(),
        key_subs: HashMap::new(),
        phantoms: HashMap::new(),
        having: None,
        pw_aggs: Vec::new(),
        pw_partial: false,
        join_force: None,
        order_hint: None,
        agg_key: None,
        agg_input: None,
        unique_sides: Default::default(),
        plain_distinct: None,
        shown: shown_subqueries(input, &needed_masks(input)),
        rowmarks: Default::default(),
        rowmark_junk: Default::default(),
        copy_marks: Default::default(),
        plain_consts: Default::default(),
        alts: Default::default(),
        sub_params: Vec::new(),
        detach: false,
        unattached: HashMap::new(),
        fresh_hashed: Default::default(),
        dummy_target: None,
    };
    let n = b.level(input);
    let gcols: Vec<ColInfo> = groups
        .iter()
        .map(|g| match g {
            Expr::Col(j, _) if *j < n.cols.len() => n.cols[*j].clone(),
            g => ColInfo {
                width: type_width(&g.ty()),
                var: None,
                distinct: None,
            },
        })
        .collect();
    sets.iter()
        .map(|set| {
            let cols: Vec<ColInfo> = set.iter().filter_map(|i| gcols.get(*i).cloned()).collect();
            b.num_groups(&cols, n.rows)
        })
        .collect()
}

/// The grouping keys of an aggregation over `input` that group nothing more (EXPLAIN's
/// `redundant_keys`): the executor's hash tables leave them out, as Postgres's do.
pub fn agg_redundant_keys(
    input: &Arc<Plan>,
    groups: &[Expr],
    sets: Option<&[Vec<usize>]>,
    ctx: &ExecCtx,
) -> Vec<bool> {
    let o = Options {
        analyze: false,
        verbose: false,
        costs: true,
        buffers: false,
        wal: false,
        settings: false,
        generic_plan: false,
        scroll: false,
        timing: false,
        summary: false,
        memory: false,
        serialize: false,
        serialize_binary: false,
        format: Format::Text,
    };
    let (refnames, _) = assign_refnames(input);
    let mut b = Builder {
        ctx,
        o: &o,
        prefix: true,
        refnames,
        labels: HashMap::new(),
        plan_ids: 0,
        outer: Vec::new(),
        outer_depth: Vec::new(),
        depth: 0,
        pending: Vec::new(),
        inits: Vec::new(),
        params: &[],
        ctes: HashMap::new(),
        needed: needed_masks(input),
        num_exec: None,
        top: 0,
        par: super::paths::parallel::ParCfg::read(ctx).enabled(),
        member_scan: false,
        group_order: None,
        group_order_full: false,
        group_named: Vec::new(),
        whole_vars: HashMap::new(),
        unplanned_cols: Default::default(),
        sub_alias: None,
        json_casts: Vec::new(),
        proj_depth: 0,
        level_tops: std::collections::HashSet::new(),
        group_tops: std::collections::HashSet::new(),
        json_aggs: HashMap::new(),
        gtl: Default::default(),
        carried: HashMap::new(),
        renumber: None,
        events: Vec::new(),
        lvl_stack: Vec::new(),
        lvl_next: 0,
        spans: Vec::new(),
        key_sub_ids: Default::default(),
        key_subs: HashMap::new(),
        phantoms: HashMap::new(),
        having: None,
        pw_aggs: Vec::new(),
        pw_partial: false,
        join_force: None,
        order_hint: None,
        agg_key: None,
        agg_input: None,
        unique_sides: Default::default(),
        plain_distinct: None,
        shown: shown_subqueries(input, &needed_masks(input)),
        rowmarks: Default::default(),
        rowmark_junk: Default::default(),
        copy_marks: Default::default(),
        plain_consts: Default::default(),
        alts: Default::default(),
        sub_params: Vec::new(),
        detach: false,
        unattached: HashMap::new(),
        fresh_hashed: Default::default(),
        dummy_target: None,
    };
    let n = b.level(input);
    b.redundant_keys(input, groups, sets, &n.cols)
}

/// EXPLAIN's estimates of `plan` (no output).
/// A grouping-sets aggregation (`plan`) EXPLAIN shows sorted (a GroupAggregate whose
/// rollups sort the input), so its groups come out in that order.
pub fn sets_sorted(plan: &Arc<Plan>, ctx: &ExecCtx) -> bool {
    let o = Options {
        analyze: false,
        verbose: false,
        costs: true,
        buffers: false,
        wal: false,
        settings: false,
        generic_plan: false,
        scroll: false,
        timing: false,
        summary: false,
        memory: false,
        serialize: false,
        serialize_binary: false,
        format: Format::Text,
    };
    let (refnames, _) = assign_refnames(plan);
    let mut b = Builder {
        ctx,
        o: &o,
        prefix: true,
        refnames,
        labels: HashMap::new(),
        plan_ids: 0,
        outer: Vec::new(),
        outer_depth: Vec::new(),
        depth: 0,
        pending: Vec::new(),
        inits: Vec::new(),
        params: &[],
        ctes: HashMap::new(),
        needed: needed_masks(plan),
        num_exec: None,
        top: 0,
        par: super::paths::parallel::ParCfg::read(ctx).enabled(),
        member_scan: false,
        group_order: None,
        group_order_full: false,
        group_named: Vec::new(),
        whole_vars: HashMap::new(),
        unplanned_cols: Default::default(),
        sub_alias: None,
        json_casts: Vec::new(),
        proj_depth: 0,
        level_tops: std::collections::HashSet::new(),
        group_tops: std::collections::HashSet::new(),
        json_aggs: HashMap::new(),
        gtl: Default::default(),
        carried: HashMap::new(),
        renumber: None,
        events: Vec::new(),
        lvl_stack: Vec::new(),
        lvl_next: 0,
        spans: Vec::new(),
        key_sub_ids: Default::default(),
        key_subs: HashMap::new(),
        phantoms: HashMap::new(),
        having: None,
        pw_aggs: Vec::new(),
        pw_partial: false,
        join_force: None,
        order_hint: None,
        agg_key: None,
        agg_input: None,
        unique_sides: Default::default(),
        plain_distinct: None,
        shown: shown_subqueries(plan, &needed_masks(plan)),
        rowmarks: Default::default(),
        rowmark_junk: Default::default(),
        copy_marks: Default::default(),
        plain_consts: Default::default(),
        alts: Default::default(),
        sub_params: Vec::new(),
        detach: false,
        unattached: HashMap::new(),
        fresh_hashed: Default::default(),
        dummy_target: None,
    };
    let n = b.level(plan);
    fn find(n: &ExNode) -> Option<bool> {
        if n.node_type == "Aggregate" {
            return Some(n.strategy == Some("Sorted"));
        }
        n.children.iter().find_map(find)
    }
    find(&n).unwrap_or(false)
}

/// A DISTINCT (`plan`) EXPLAIN shows as a Unique over its input sorted (rather than a
/// HashAggregate): the columns its rows come out sorted on.
pub fn distinct_order(plan: &Arc<Plan>, ctx: &ExecCtx) -> Option<Vec<usize>> {
    level_node(plan, ctx).unique_keys
}

/// An aggregation (`plan`) EXPLAIN shows split in two, a Partial Aggregate in the workers
/// under a Finalize Aggregate (create_partial_grouping_paths): its aggregates' states are
/// combined (the combine function runs) before their results are made.
pub fn agg_split(plan: &Arc<Plan>, ctx: &ExecCtx) -> bool {
    fn find(n: &ExNode) -> Option<bool> {
        if n.node_type == "Aggregate" {
            return Some(n.partial_mode == Some("Finalize"));
        }
        n.children.iter().find_map(find)
    }
    find(&level_node(plan, ctx)).unwrap_or(false)
}

/// The EXPLAIN node of `plan` (a query level's top), with costs.
fn level_node(plan: &Arc<Plan>, ctx: &ExecCtx) -> ExNode {
    let o = Options {
        analyze: false,
        verbose: false,
        costs: true,
        buffers: false,
        wal: false,
        settings: false,
        generic_plan: false,
        scroll: false,
        timing: false,
        summary: false,
        memory: false,
        serialize: false,
        serialize_binary: false,
        format: Format::Text,
    };
    let (refnames, _) = assign_refnames(plan);
    let mut b = Builder {
        ctx,
        o: &o,
        prefix: true,
        refnames,
        labels: HashMap::new(),
        plan_ids: 0,
        outer: Vec::new(),
        outer_depth: Vec::new(),
        depth: 0,
        pending: Vec::new(),
        inits: Vec::new(),
        params: &[],
        ctes: HashMap::new(),
        needed: needed_masks(plan),
        num_exec: None,
        top: 0,
        par: super::paths::parallel::ParCfg::read(ctx).enabled(),
        member_scan: false,
        group_order: None,
        group_order_full: false,
        group_named: Vec::new(),
        whole_vars: HashMap::new(),
        unplanned_cols: Default::default(),
        sub_alias: None,
        json_casts: Vec::new(),
        proj_depth: 0,
        level_tops: std::collections::HashSet::new(),
        group_tops: std::collections::HashSet::new(),
        json_aggs: HashMap::new(),
        gtl: Default::default(),
        carried: HashMap::new(),
        renumber: None,
        events: Vec::new(),
        lvl_stack: Vec::new(),
        lvl_next: 0,
        spans: Vec::new(),
        key_sub_ids: Default::default(),
        key_subs: HashMap::new(),
        phantoms: HashMap::new(),
        having: None,
        pw_aggs: Vec::new(),
        pw_partial: false,
        join_force: None,
        order_hint: None,
        agg_key: None,
        agg_input: None,
        unique_sides: Default::default(),
        plain_distinct: None,
        shown: shown_subqueries(plan, &needed_masks(plan)),
        rowmarks: Default::default(),
        rowmark_junk: Default::default(),
        copy_marks: Default::default(),
        plain_consts: Default::default(),
        alts: Default::default(),
        sub_params: Vec::new(),
        detach: false,
        unattached: HashMap::new(),
        fresh_hashed: Default::default(),
        dummy_target: None,
    };
    b.level(plan)
}

pub fn estimate(plan: &Arc<Plan>, ctx: &ExecCtx) -> PlanEstimate {
    estimate_in(plan, ctx, false, true)
}

/// `estimate` of a sub-select's plan (a level of its own): every `Outer(d, ..)` in it is a
/// value of an enclosing level, so a sub-select of it reading only those is an InitPlan its
/// top node pays for once (SS_charge_for_initplans), not a SubPlan run per row.
pub fn estimate_subquery(plan: &Arc<Plan>, ctx: &ExecCtx) -> PlanEstimate {
    estimate_in(plan, ctx, true, true)
}

/// `estimate` of the plan Postgres would make without parallel workers (its serial plan).
pub fn estimate_serial(plan: &Arc<Plan>, ctx: &ExecCtx) -> PlanEstimate {
    estimate_in(plan, ctx, false, false)
}

fn estimate_in(plan: &Arc<Plan>, ctx: &ExecCtx, sublevel: bool, parallel: bool) -> PlanEstimate {
    let o = estimate_options();
    let mut b = estimate_builder(plan, ctx, &o);
    if !parallel {
        b.par = None;
    }
    if sublevel {
        b.outer_depth = vec![usize::MAX; 16];
    }
    let n = b.level(plan);
    let width_of = super::binder::plan_width(plan);
    let cols = if n.cols.len() == width_of {
        n.cols
            .iter()
            .map(|c| (c.width, c.var.clone(), c.distinct))
            .collect()
    } else {
        vec![(n.width / width_of.max(1) as u32, None, None); width_of]
    };
    let presorted = n
        .details
        .iter()
        .find_map(|d| match d {
            Detail::List("Presorted Key", v) => Some(v.len()),
            _ => None,
        })
        .unwrap_or(0);
    PlanEstimate {
        rows: n.rows,
        startup: n.startup_cost,
        total: n.total_cost,
        width: n.width,
        input_width: n.children.first().map_or(0, |c| c.width),
        input_type: n.children.first().map_or("", |c| c.node_type),
        group_sort: (n.strategy == Some("Sorted") || n.node_type == "Group").then(|| group_sort_of(&n.details)),
        node_type: n.node_type,
        presorted,
        disabled: parallel::disabled_nodes(&n),
        cols,
    }
}

/// A LATERAL join's right side with the join clauses its parameterized path checks
/// moved into it (a Filter whose every clause reads the join's values, `Outer(1, k)`):
/// the item's own rows (rel->rows) and those clauses' selectivity where the join relation
/// is sized (set_joinrel_size_estimates). A subquery's Subquery Scan estimated them as
/// restrictions over two relations' Vars (cost_subqueryscan, `costs::level_vars_hidden`),
/// the RestrictInfos' cached norm_selec the join takes too; another item's parameterized
/// path took them with the left side a constant, caching nothing: their join selectivity.
fn lateral_moved(right: &Plan, params: &[Expr], lcols: &[ColInfo], ctx: &ExecCtx) -> Option<(f64, f64)> {
    let Plan::Filter { input, pred } = right else {
        return None;
    };
    let mut parts = Vec::new();
    conjuncts(pred, &mut parts);
    let reads = |e: &Expr| {
        let mut f = false;
        e.walk(&mut |x| f |= matches!(x, Expr::Outer(1, ..)));
        f
    };
    if parts.is_empty() || !parts.iter().all(reads) {
        return None;
    }
    let inner = estimate(input, ctx);
    let cat = ctx.catalog();
    let subquery = matches!(&**input, Plan::Barrier { .. });
    let nl = lcols.len();
    let mut cols: Vec<Option<costs::VarRef>> = if subquery { Vec::new() } else { lcols.iter().map(|c| c.var.clone()).collect() };
    cols.extend(inner.cols.iter().map(|c| c.1.clone()));
    let vars = |i: usize| cols.get(i).cloned().flatten();
    let est = costs::Est {
        cat: &cat,
        fmt: ctx.fmt(),
        vars: &vars,
        exprs: &[],
        endpoint: None,
        distinct: None,
        user: None,
        same_rel: subquery,
    };
    let mut sel = 1.0;
    for p in &parts {
        if subquery {
            sel *= est.clause(&costs::level_vars_hidden(p, &|d| d == 1));
            continue;
        }
        // (Over the join's row: the values as the left side's expressions.)
        let joined = p
            .clone()
            .transform(&mut |x| {
                Ok(match x {
                    Expr::Outer(1, k, t) => params.get(k).cloned().unwrap_or(Expr::Outer(1, k, t)),
                    Expr::Col(c, t) => Expr::Col(c + nl, t),
                    x => x,
                })
            })
            .expect("a moved clause over the join's row");
        let jv = |e: &Expr| -> Option<super::joins::JoinVar> {
            let Expr::Col(i, _) = strip_relabel(strip_widening(e)) else { return None };
            let (var, distinct) = if *i < nl {
                (lcols[*i].var.clone(), lcols[*i].distinct)
            } else {
                let c = inner.cols.get(*i - nl)?;
                (c.1.clone(), c.2)
            };
            Some(super::joins::JoinVar {
                rel_rows: var.as_ref().map_or(0.0, |v| v.rows),
                var,
                distinct,
            })
        };
        sel *= match &joined {
            Expr::Call(d, a, _) if d.name == "=" && a.len() == 2 => match (jv(&a[0]), jv(&a[1])) {
                (Some(x), Some(y)) => super::joins::eqjoinsel_inner(&cat, ctx.fmt(), &x, &y),
                _ => est.clause(&joined),
            },
            e => est.clause(e),
        };
    }
    Some((inner.rows, sel))
}

/// One of a FROM subquery's final paths, as the join search weighs it.
pub struct SubPathCost {
    pub startup: f64,
    pub total: f64,
    pub disabled: usize,
    /// Its order over the subquery's output columns: (column, descending, nulls first).
    pub order: Vec<(usize, bool, bool)>,
}

/// A FROM subquery's (`plan`, its `Barrier`) final paths, as set_subquery_pathlist makes
/// a Subquery Scan path of each: its cheapest first, then the others its grouping or
/// DISTINCT keeps beside it (add_path, `alt`), each with the cost of its scan.
pub fn subquery_paths(plan: &Arc<Plan>, ctx: &ExecCtx) -> Vec<SubPathCost> {
    let Plan::Barrier { input, sorted, path: None, .. } = &**plan else {
        return Vec::new();
    };
    if barrier_junk(input, *sorted).is_some() {
        return Vec::new();
    }
    let o = estimate_options();
    let top = estimate_builder(plan, ctx, &o).level(plan);
    let mut b = estimate_builder(plan, ctx, &o);
    b.inits.push(Vec::new());
    let c = b.node(input);
    let names: Vec<String> = c.exported.iter().map(|n| b.pick(n, Cx::Upper)).collect();
    let col_of = |o: &str| -> Option<(usize, bool, bool)> {
        names.iter().enumerate().find_map(|(j, nm)| {
            let rest = if o == nm { "" } else { o.strip_prefix(nm.as_str()).filter(|r| r.starts_with(' '))? };
            let (desc, nulls_first) = match rest {
                "" => (false, false),
                " DESC" => (true, true),
                " NULLS FIRST" => (false, true),
                " DESC NULLS LAST" => (true, false),
                _ => return None,
            };
            Some((j, desc, nulls_first))
        })
    };
    let mut forms = vec![c.node];
    let mut next = forms[0].alt.take();
    while let Some(mut a) = next {
        next = a.alt.take();
        forms.push(*a);
    }
    // (The Subquery Scan over each: what it costs over the cheapest.)
    let (ds, dt) = (top.startup_cost - forms[0].startup_cost, top.total_cost - forms[0].total_cost);
    forms
        .iter()
        .map(|x| {
            let mut order: Vec<(usize, bool, bool)> = Vec::new();
            for k in &x.order {
                match col_of(k) {
                    Some(c) if !order.iter().any(|o| o.0 == c.0) => order.push(c),
                    _ => break,
                }
            }
            SubPathCost {
                startup: x.startup_cost + ds,
                total: x.total_cost + dt,
                disabled: parallel::disabled_nodes(x),
                order,
            }
        })
        .collect()
}

/// The EXPLAIN output of `plan`: text lines, or one JSON / YAML / XML document. Planning
/// and execution times are in seconds.
/// enable_partition_pruning.
fn partition_pruning_on(ctx: &ExecCtx) -> bool {
    ctx.sess
        .settings
        .borrow()
        .get_raw("enable_partition_pruning")
        .is_none_or(|v| !matches!(v.as_str(), "off" | "false" | "0"))
}

/// The values an inheritance Append's run-time pruning compares its partition keys with
/// each time it is rescanned (its exec pruning steps' expressions): its members' clauses
/// on their partition keys reading an outer row's values, from the first member with any.
fn exec_prune_values(members: &[Arc<Plan>]) -> Vec<Expr> {
    for m in members {
        let mut q: &Plan = m;
        while let Plan::Project { input, exprs } = q
            && exprs.iter().all(|e| matches!(e, Expr::Col(..)))
        {
            q = input;
        }
        let Plan::Scan(sc) = q else { continue };
        let Some(levels) = &sc.part else { continue };
        let mut quals = Vec::new();
        if let Some(f) = &sc.filter {
            crate::sql::binder::optimize::conjuncts(f.clone(), &mut quals);
        }
        if let super::plan::Access::Index { quals: iq, .. } = &sc.access {
            for x in iq {
                if !quals.iter().any(|y| y.same_as(&x.orig)) {
                    quals.push(x.orig.clone());
                }
            }
        }
        let key = |e: &Expr| levels.iter().any(|l| l.key.iter().any(|k| k.same_as(strip_widening(e))));
        let mut out = Vec::new();
        for qual in &quals {
            let Expr::Call(_, args, _) = qual else { continue };
            if args.len() != 2 {
                continue;
            }
            for (a, b) in [(0, 1), (1, 0)] {
                if key(&args[a]) && args[b].columns().is_empty() && args[b].has_outer_refs() {
                    out.push(args[b].clone());
                }
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    Vec::new()
}

/// Whether `e` holds a sub-select (not a function call run as one).
fn has_subselect(e: &Expr) -> bool {
    let mut any = false;
    e.walk(&mut |x| any |= matches!(x, Expr::Subquery(s) if s.call.is_none()));
    any
}

/// The sub-selects of a projection's entries, by id.
fn entry_subquery_ids(e: &Expr) -> Vec<usize> {
    let mut out = Vec::new();
    e.walk(&mut |x| {
        // (Not a MIN / MAX InitPlan the planner made: its value is a parameter the
        // query shares.)
        if let Expr::Subquery(s) = x
            && s.func.is_none()
            && s.id < crate::sql::paths::PLANNER_SUBQUERY_BASE
        {
            out.push(s.id);
        }
    });
    out
}

/// pullup_replace_vars puts a copy of a pulled-up subquery's output expression in each place
/// that reads it: a sub-select two entries of a projection read is two SubPlans (InitPlans)
/// there, each run where it is read. The plan with each such later copy given an id of its
/// own (and its plan nodes: the executor runs and caches each apart), when there is one.
/// Applied to every query level as it is planned (`paths::plan_query`).
pub(crate) fn split_shared_subqueries(p: &Arc<Plan>) -> Option<Arc<Plan>> {
    fn shared(exprs: &[Expr]) -> bool {
        let mut seen = std::collections::HashSet::new();
        exprs.iter().any(|e| {
            let ids = entry_subquery_ids(e);
            let dup = ids.iter().any(|i| seen.contains(i));
            seen.extend(ids);
            dup
        })
    }
    // (A projection over one computing a sub-select it reads twice: the two composed, as
    // the pulled-up subquery's output is put in place of each reference.)
    fn reread(outer: &[Expr], input: &Plan) -> bool {
        let Plan::Project { exprs: inner, .. } = input else { return false };
        let mut count = vec![0usize; inner.len()];
        for e in outer {
            for c in e.columns() {
                if let Some(n) = count.get_mut(c) {
                    *n += 1;
                }
            }
        }
        inner.iter().zip(&count).any(|(e, n)| *n > 1 && !entry_subquery_ids(e).is_empty())
            && inner.iter().all(|e| {
                let mut plain = true;
                e.walk(&mut |x| plain &= !matches!(x, Expr::Agg(..) | Expr::Window(..) | Expr::Srf(..) | Expr::Grouping(..)));
                plain && !expr_volatile_deep(e)
            })
    }
    fn any_shared(p: &Plan) -> bool {
        matches!(p, Plan::Project { exprs, input } if shared(exprs) || reread(exprs, input))
            || plan_inputs(p).iter().any(|c| any_shared(c))
    }
    fn go(p: &mut Arc<Plan>) {
        if !any_shared(p) {
            return;
        }
        let p = Arc::make_mut(p);
        if let Plan::Project { exprs, input } = p
            && reread(exprs, input)
            && let Plan::Project { input: below, exprs: inner } = &**input
        {
            let composed: Vec<Expr> = exprs
                .iter()
                .map(|e| {
                    e.clone()
                        .transform(&mut |x| {
                            Ok(match x {
                                Expr::Col(i, t) => inner.get(i).cloned().unwrap_or(Expr::Col(i, t)),
                                x => x,
                            })
                        })
                        .unwrap_or_else(|_| e.clone())
                })
                .collect();
            let below = below.clone();
            *exprs = composed;
            *input = below;
        }
        if let Plan::Project { exprs, .. } = p {
            let mut seen = std::collections::HashSet::new();
            for e in exprs.iter_mut() {
                let ids = entry_subquery_ids(e);
                if ids.iter().any(|i| seen.contains(i)) {
                    let copy = e
                        .clone()
                        .transform(&mut |x| {
                            Ok(match x {
                                Expr::Subquery(mut s) if s.func.is_none() && seen.contains(&s.id) => {
                                    // (Its relations are range table entries of their own, its
                                    // own sub-selects copies too.)
                                    crate::sql::paths::make_subquery_copy(&mut s);
                                    Expr::Subquery(s)
                                }
                                x => x,
                            })
                        })
                        .ok();
                    if let Some(c) = copy {
                        *e = c;
                    }
                }
                seen.extend(ids);
            }
        }
        for c in plan_inputs_mut(p) {
            go(c);
        }
    }
    if !any_shared(p) {
        return None;
    }
    let mut out = p.clone();
    go(&mut out);
    Some(out)
}

thread_local! {
    /// The select-list projections of the subqueries planned apart in the plan being
    /// explained (remove_unused_subquery_outputs' NULLs are constants there).
    static UNREAD_NULLS: std::cell::RefCell<std::collections::HashSet<usize>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

/// The statement's select-list projection when only nodes with set_dummy_tlist_references'
/// target lists read it (a Sort, a Limit, a LockRows) and nothing below it makes a pulled-up
/// subquery's outputs PlaceHolderVars: its constants are plain Consts, which
/// search_indexed_tlist_for_non_var never replaces with references. (A subquery's planned
/// apart are read through its scan's Vars, or its plan's target list once that scan is
/// removed: referenced.)
fn plain_const_lists(root: &Plan) -> std::collections::HashSet<usize> {
    fn top(p: &Plan, out: &mut std::collections::HashSet<usize>) {
        match p {
            Plan::Sort { input, .. } | Plan::Limit { input, .. } | Plan::LockRows { input, .. } | Plan::Context { input, .. } => {
                top(input, out)
            }
            Plan::Project { input, .. } if !may_hold_phvs(input) => {
                out.insert(addr(p));
            }
            _ => {}
        }
    }
    let mut out = std::collections::HashSet::new();
    top(root, &mut out);
    out
}

/// Whether the query level under `p` has what makes a pulled-up subquery's outputs
/// PlaceHolderVars (pull_up_simple_subquery: an outer join over it, an appendrel member, a
/// grouping set): not looking into a subquery planned apart.
fn may_hold_phvs(p: &Plan) -> bool {
    match p {
        Plan::Join { kind, .. } if !matches!(kind, JoinKind::Inner | JoinKind::Semi) => true,
        Plan::Append { .. } | Plan::SetOp { .. } | Plan::Recursive { .. } => true,
        Plan::Agg { sets: Some(_), .. } => true,
        Plan::Barrier { .. } | Plan::CteScan { .. } => false,
        p => plan_inputs(p).into_iter().any(|c| may_hold_phvs(c)),
    }
}

/// A Limit's input down to its level's select list (through the level's ORDER BY sort and
/// LockRows): the constants that select list computes, per column.
fn level_select_consts(p: &Plan) -> Vec<Option<Expr>> {
    match p {
        Plan::Sort { input, .. } | Plan::LockRows { input, .. } | Plan::Context { input, .. } => level_select_consts(input),
        Plan::Project { exprs, .. } => exprs
            .iter()
            .map(|e| matches!(e, Expr::Const(..)).then(|| e.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

/// The subquery levels' select-list projections under `p` (`UNREAD_NULLS`).
fn select_lists(p: &Plan, out: &mut std::collections::HashSet<usize>) {
    if let Plan::Barrier { input, secure: false, .. } = p {
        let mut q: &Plan = input;
        while let Plan::Limit { input, .. } | Plan::Sort { input, .. } | Plan::LockRows { input, .. } = q {
            q = input;
        }
        if matches!(q, Plan::Project { .. }) {
            out.insert(addr(q));
        }
    }
    let mut es = Vec::new();
    node_exprs(p, &mut es);
    let mut sq = Vec::new();
    for e in es {
        subqueries_in(e, &mut sq);
    }
    for s in sq {
        select_lists(&s.plan, out);
    }
    for c in plan_inputs(p) {
        select_lists(c, out);
    }
}

/// `rel.col` as a name shown bare (`col`) or qualified; a name without a qualification as it
/// is.
fn split_qualified(s: &str) -> Name {
    let mut quoted = false;
    let mut dot = None;
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '.' if !quoted => dot = Some(i),
            _ => {}
        }
    }
    match dot {
        Some(i) => Name {
            plain: s[i + 1..].to_string(),
            full: s.to_string(),
        },
        None => Name::same(s),
    }
}

/// Debugging aid: the plan tree EXPLAIN gets, on stderr, when the engine runs with
/// `ROLL_DUMP_PLAN` set (node variants, widths, projections, scans, sort keys, sub-selects).
pub fn dump_plan(p: &Plan, depth: usize) {
    let dbg = format!("{p:?}");
    let name: String = dbg.chars().take_while(|c| c.is_alphanumeric()).collect();
    let extra = match p {
        Plan::Project { exprs, .. } => exprs
            .iter()
            .map(|e| match e {
                Expr::Col(i, _) => format!("#{i}"),
                Expr::Const(..) => "K".into(),
                Expr::Cast(x, from, to, ex) => format!("CAST({x:?} {from:?} -> {to:?} {ex})").chars().take(600).collect(),
                e => format!("{e:?}").chars().take(300).collect(),
            })
            .collect::<Vec<_>>()
            .join(","),
        Plan::Scan(s) => format!("{} {:?} filter={:?}", s.table.name, s.access, s.filter).chars().take(400).collect(),
        Plan::Filter { pred, .. } => format!("{pred:?}"),
        Plan::Append { inh, alias, .. } => format!("inh={inh:?} alias={alias:?}"),
        Plan::Join { kind, cond, hash, params, nleft, meta, .. } => format!(
            "{kind:?} nleft={nleft} cond={cond:?} hash={hash:?} params={params:?} bound={} lateral_held={} ec_written={:?}",
            meta.bound, meta.lateral_held, meta.ec_written
        )
        .chars()
        .take(600)
        .collect(),
        Plan::Barrier { alias, .. } => format!("alias={alias:?}"),
        Plan::Sort { keys, .. } => keys
            .iter()
            .map(|k| format!("{:?} using={:?}", k.expr, k.using.as_ref().map(|u| (u.name, u.coll.as_ref().map(|c| c.name.clone())))).chars().take(400).collect::<String>())
            .collect::<Vec<_>>()
            .join(" | "),
        _ => String::new(),
    };
    eprintln!("{}{} w={} {}", "  ".repeat(depth), name, super::binder::plan_width(p), extra);
    let mut es = Vec::new();
    node_exprs(p, &mut es);
    let mut sq = Vec::new();
    for e in es {
        subqueries_in(e, &mut sq);
    }
    for s in sq {
        eprintln!("{}[subquery {}]", "  ".repeat(depth + 1), s.id);
        dump_plan(&s.plan, depth + 2);
    }
    if let Plan::With { ctes, names, .. } = p {
        for ((_, c), n) in ctes.iter().zip(names) {
            eprintln!("{}[CTE {n}]", "  ".repeat(depth + 1));
            dump_plan(c, depth + 2);
        }
    }
    let dml_source = match p {
        Plan::Insert(x) => Some(&x.source),
        Plan::Update(x) => Some(&x.source),
        Plan::Delete(x) => Some(&x.source),
        Plan::Merge(x) => Some(&x.source),
        _ => None,
    };
    if let Some(s) = dml_source
        && !plan_inputs(p).iter().any(|c| Arc::ptr_eq(c, s))
    {
        dump_plan(s, depth + 1);
    }
    for c in plan_inputs(p) {
        dump_plan(c, depth + 1);
    }
}

/// A MERGE whose target is an empty inheritance tree (no scan of it): the target is a
/// range table entry all the same.
fn dummy_merge_target(p: &Plan) -> usize {
    match p {
        Plan::Merge(m) => {
            let oids = std::iter::once(m.rules.table.oid).chain(m.members.iter().map(|x| x.rules.table.oid));
            oids.into_iter().all(|o| target_scan(&m.source, o).is_none()) as usize
        }
        _ => 0,
    }
}

pub fn explain(
    plan: &Arc<Plan>,
    hidden_rtes: usize,
    o: &Options,
    ctx: &ExecCtx,
    params: &[crate::types::Value],
    instr: Option<&Instr>,
    planning: f64,
    execution: f64,
) -> String {
    // A statement a rule rewrote into several: each one's plan, a blank line between.
    if instr.is_none()
        && o.format == Format::Text
        && let Some(list) = rule_statements(plan)
    {
        let mut out = String::new();
        for (i, (s, action)) in list.iter().enumerate() {
            if i > 0 {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
            }
            let _g = crate::scoped::set(&RULE_ACTION, *action, "explain::RULE_ACTION");
            out.push_str(&explain(s, hidden_rtes, o, ctx, params, None, planning, execution));
        }
        return out;
    }
    // (The planner made the copies already; a plan built otherwise gets them here.)
    let split = if instr.is_none() { split_shared_subqueries(plan) } else { None };
    let plan = split.as_ref().unwrap_or(plan);
    // (Built again when it groups partitionwise: the parents' names go unused then.)
    // (An inheritance tree that is a UNION ALL's arm is flattened into its Append: the
    // parent's name goes unused too.)
    if std::env::var_os("ROLL_DUMP_PLAN").is_some() {
        dump_plan(plan, 0);
    }
    UNREAD_NULLS.with(|u| {
        let mut set = std::collections::HashSet::new();
        select_lists(plan, &mut set);
        *u.borrow_mut() = set;
    });
    let mut skip: Vec<usize> = Vec::new();
    union_inh_appends(plan, &mut skip);
    let mut pw_named = false;
    let mut orders: Vec<Vec<usize>> = Vec::new();
    let mut ordered = false;
    let gtl = std::rc::Rc::new(group_tlists(plan));
    let mut renumber: Option<std::rc::Rc<Vec<usize>>> = None;
    let mut fresh_hashed: Option<std::rc::Rc<std::collections::HashSet<usize>>> = None;
    let mut sub_ranks: Option<std::rc::Rc<HashMap<usize, usize>>> = None;
    let root = loop {
    let (refnames, results) = assign_refnames_skip(plan, Some(ctx), &skip, &orders, sub_ranks.as_deref());
    // ON CONFLICT DO UPDATE's EXCLUDED is a range table entry of its own.
    // (Under a rule's actions too: a CTE of them, a frame.)
    fn excluded_rte(p: &Plan) -> bool {
        match p {
            Plan::Insert(i) => i
                .on_conflict
                .as_ref()
                .is_some_and(|oc| matches!(oc.action, ConflictAction::Update { .. })),
            Plan::Context { input, .. } | Plan::Project { input, .. } => excluded_rte(input),
            Plan::With { body, ctes, .. } => excluded_rte(body) || ctes.iter().any(|(_, c)| excluded_rte(c)),
            _ => false,
        }
    }
    let excluded = excluded_rte(plan) as usize;
    let names_before = refnames.clone();
    let mut b = Builder {
        ctx,
        o,
        // (An inheritance tree's parent is a range table entry besides its members.)
        prefix: refnames.len() + hidden_rtes + results + excluded + inh_parents(plan) + dummy_merge_target(plan) > 1,
        refnames,
        labels: HashMap::new(),
        plan_ids: 0,
        outer: Vec::new(),
        outer_depth: Vec::new(),
        depth: 0,
        pending: Vec::new(),
        inits: Vec::new(),
        params,
        ctes: HashMap::new(),
        needed: {
            let mut n = needed_masks(plan);
            grouping_needs(plan, &gtl, &mut n);
            n
        },
        num_exec: None,
        top: 0,
        // (standard_planner: a parallel unsafe function anywhere in the query means no
        // parallel plan at all.)
        par: super::paths::parallel::ParCfg::read(ctx)
            .enabled()
            .filter(|_| super::paths::parallel::plan_hazard(plan, &ctx.catalog()) != 'u'),
        member_scan: false,
        group_order: None,
        group_order_full: false,
        group_named: Vec::new(),
        whole_vars: HashMap::new(),
        unplanned_cols: Default::default(),
        sub_alias: None,
        json_casts: Vec::new(),
        proj_depth: 0,
        level_tops: std::collections::HashSet::new(),
        group_tops: std::collections::HashSet::new(),
        json_aggs: HashMap::new(),
        gtl: gtl.clone(),
        carried: HashMap::new(),
        renumber: renumber.clone(),
        events: Vec::new(),
        lvl_stack: Vec::new(),
        lvl_next: 0,
        spans: Vec::new(),
        key_sub_ids: Default::default(),
        key_subs: HashMap::new(),
        phantoms: HashMap::new(),
        having: None,
        pw_aggs: Vec::new(),
        pw_partial: false,
        join_force: None,
        order_hint: None,
        agg_key: None,
        agg_input: None,
        unique_sides: Default::default(),
        plain_distinct: None,
        shown: shown_subqueries(plan, &needed_masks(plan)),
        rowmarks: rowmark_scans(plan),
        rowmark_junk: Default::default(),
        copy_marks: copy_mark_leaves(plan),
        plain_consts: plain_const_lists(plan),
        alts: Default::default(),
        sub_params: Vec::new(),
        detach: false,
        unattached: HashMap::new(),
        fresh_hashed: fresh_hashed.clone().unwrap_or_default(),
        dummy_target: None,
    };
    b.top = addr(plan);
    b.group_tops.insert(grouping_top(plan));
    if let Plan::Merge(m) = &**plan
        && dummy_merge_target(plan) == 1
    {
        b.dummy_target = Some(m.alias.clone().unwrap_or_else(|| m.rules.table.name.clone()));
    }
    let root = b.level(plan);
    if !pw_named && !b.pw_aggs.is_empty() {
        pw_named = true;
        skip.extend(b.pw_aggs.iter().copied());
        continue;
    }
    // (Again when a Parallel Append runs a UNION ALL's arms kept subqueries in another
    // order than the query lists them: setrefs names their relations in the plan's.)
    if !ordered {
        ordered = true;
        let mut o2 = Vec::new();
        arm_orders(&root, &mut o2);
        if !o2.is_empty() && assign_refnames_skip(plan, Some(ctx), &skip, &o2, sub_ranks.as_deref()).0 != b.refnames {
            orders = o2;
            continue;
        }
    }
    // (Again with the SubPlans numbered in the order Postgres plans them.)
    if renumber.is_none()
        && let Some(r) = renumbering(&b.events, &b.spans, matches!(&**plan, Plan::Merge(_)))
    {
        renumber = Some(std::rc::Rc::new(r));
        continue;
    }
    // (Again with the SubPlans' relations named in their numbers' order.)
    if sub_ranks.is_none() {
        let r = b.sub_ranks();
        let again = !r.is_empty() && assign_refnames_skip(plan, Some(ctx), &skip, &orders, Some(&r)).0 != names_before;
        sub_ranks = Some(std::rc::Rc::new(r));
        if again {
            continue;
        }
    }
    // (Again with the hashed alternatives shown beside their plain ones named apart.)
    if fresh_hashed.is_none() {
        let both: std::collections::HashSet<usize> =
            b.alts.iter().filter(|(_, a)| a.shown == [true, true]).map(|(id, _)| *id).collect();
        let again = !both.is_empty();
        fresh_hashed = Some(std::rc::Rc::new(both));
        if again {
            continue;
        }
    }
    let root = b.single_copy(plan, root);
    break if o.scroll && !backward_ok(&root) { b.materialize_top(root) } else { root };
    };
    let mut r = Render {
        o,
        instr,
        w: Writer::new(o.format),
        procs: 1,
        rescans: 1,
        mat_loops: None,
        leader: crate::sql::paths::parallel::ParCfg::read(ctx).leader,
        jg: super::joins::JoinGucs::read(ctx),
        io_timing: ctx.sess.settings.borrow().get_raw("track_io_timing").is_some_and(|v| v == "on" || v == "true"),
    };
    r.w.open("Query", None, true);
    r.node(&root);
    let text = o.format == Format::Text;
    // SETTINGS: the planner parameters not at their built-in values
    // (ExplainPrintSettings).
    if o.settings {
        let changed: Vec<(String, String)> = ctx
            .sess
            .settings
            .borrow()
            .all()
            .into_iter()
            .filter(|r| EXPLAIN_GUCS.contains(&r.name.as_str()) && r.setting != r.boot_val)
            .map(|r| (r.name, r.shown))
            .collect();
        if text {
            if !changed.is_empty() {
                let v: Vec<String> = changed.iter().map(|(n, v)| format!("{n} = {}", quote_literal(v))).collect();
                r.w.prop("Settings", &v.join(", "), false);
            }
        } else {
            r.w.open("Settings", Some("Settings"), true);
            for (n, v) in &changed {
                r.w.prop(n, v, false);
            }
            r.w.close("Settings", true);
        }
    }
    // compute_query_id = on: the statement's query identifier (VERBOSE).
    if o.verbose
        && ctx
            .sess
            .settings
            .borrow()
            .get_raw("compute_query_id")
            .is_some_and(|v| {
                v.eq_ignore_ascii_case("on")
                    || (v.eq_ignore_ascii_case("auto") && ctx.sess.cluster.statements().is_some())
            })
    {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        format!("{plan:?}").hash(&mut h);
        let id = (h.finish() as i64).max(1);
        r.w.prop("Query Identifier", &id.to_string(), true);
    }
    // Planning's buffer usage (in a structured format, always shown) and memory.
    if o.buffers && !text || o.memory {
        if text {
            r.w.buf.push_str("Planning:\n");
            r.w.indent += 1;
        } else {
            r.w.open("Planning", Some("Planning"), true);
        }
        if o.buffers && !text {
            r.w.buffer_props(r.io_timing);
        }
        if o.memory {
            if text {
                r.w.prop("Memory", "used=9kB  allocated=16kB", false);
            } else {
                r.w.prop("Memory Used", "9200", true);
                r.w.prop("Memory Allocated", "16384", true);
            }
        }
        if text {
            r.w.indent -= 1;
        } else {
            r.w.close("Planning", true);
        }
    }
    if o.summary {
        let v = format!("{:.3}", planning * 1000.0);
        r.w.prop(
            "Planning Time",
            &if text { format!("{v} ms") } else { v },
            true,
        );
    }
    if o.analyze && !text {
        r.w.open("Triggers", Some("Triggers"), false);
        r.w.close("Triggers", false);
    }
    // SERIALIZE: the output converted as it would be sent.
    if o.serialize {
        let (rows, width) = (instr.and_then(|i| root.key.and_then(|k| i.get(k))).map_or(0, |s| s.rows), root.width as u64);
        let kb = (rows * (width + 8)).div_ceil(1024);
        let format = if o.serialize_binary { "binary" } else { "text" };
        let time = format!("{:.3}", execution * 1000.0 * 0.01);
        if text {
            let v = if o.timing {
                format!("time={time} ms  output={kb}kB  format={format}")
            } else {
                format!("output={kb}kB  format={format}")
            };
            r.w.prop("Serialization", &v, false);
        } else {
            r.w.open("Serialization", Some("Serialization"), true);
            if o.timing {
                r.w.prop("Time", &time, true);
            }
            r.w.prop("Output Volume", &kb.to_string(), true);
            r.w.prop("Format", format, false);
            if o.buffers {
                r.w.buffer_props(r.io_timing);
            }
            r.w.close("Serialization", true);
        }
    }
    if o.summary && o.analyze {
        let v = format!("{:.3}", execution * 1000.0);
        r.w.prop(
            "Execution Time",
            &if text { format!("{v} ms") } else { v },
            true,
        );
    }
    r.w.close("Query", true);
    r.w.finish()
}

/// The group keys (positions) of an aggregation below a Sort (through a projection of
/// columns) the sort keys are, in their order.
fn sort_group_order(input: &Plan, keys: &[SortKey]) -> Option<Vec<(usize, String)>> {
    // (Through projections, a HAVING, and a FROM subquery's scan the order passes.)
    let mut cols: Vec<Option<usize>> = keys
        .iter()
        .map(|k| match &k.expr {
            Expr::Col(i, _) if k.using.is_none() => Some(*i),
            // (Sorted under a collation: a key grouped under that collation.)
            Expr::Col(i, _) if k.using.as_ref().is_some_and(|u| u.coll.is_some() && matches!(u.name, "<" | ">")) => Some(*i),
            _ => None,
        })
        .collect();
    let mut cur = input;
    let groups = loop {
        match cur {
            Plan::Project { input, exprs } => {
                cols = cols
                    .into_iter()
                    .map(|c| c.and_then(|i| match exprs.get(i) {
                        Some(Expr::Col(j, _)) => Some(*j),
                        _ => None,
                    }))
                    .collect();
                cur = input;
            }
            Plan::Filter { input, .. } if matches!(&**input, Plan::Agg { .. }) => cur = input,
            Plan::Barrier { input, secure: false, .. } => cur = input,
            Plan::Agg { groups, input, .. } => break (groups, input),
            _ => return None,
        }
    };
    let (groups, agg_input) = groups;
    let mut out = Vec::new();
    for (k, c) in keys.iter().zip(cols) {
        let coll = k.using.as_ref().and_then(|u| u.coll.as_ref()).map(|c| c.name.as_str());
        match c {
            Some(j)
                if j < groups.len()
                    && coll.is_none_or(|n| group_key_collation(agg_input, &groups[j]).as_deref() == Some(n)) =>
            {
                out.push((j, sort_suffix(k)))
            }
            _ => break,
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Every group key the ORDER BY over a grouping names (by position), with the first
/// naming item's direction: `sort_group_order`'s keys, wherever they are in the list.
fn sort_group_named(input: &Plan, keys: &[SortKey]) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    for k in keys {
        // (Each key on its own: its position among the group keys, if it is one.)
        if let Some(v) = sort_group_order(input, std::slice::from_ref(k))
            && let Some((j, sfx)) = v.into_iter().next()
            && !out.iter().any(|o| o.0 == j)
        {
            out.push((j, sfx));
        }
    }
    out
}

/// A DISTINCT's input (through a projection) whose every output column that isn't a
/// constant is a scan column its filter equates with a constant.
pub(crate) fn distinct_keys_fixed(input: &Plan) -> bool {
    let (exprs, scan) = match input {
        Plan::Project { input, exprs } => match &**input {
            Plan::Scan(s) => (exprs.clone(), s),
            _ => return false,
        },
        _ => return false,
    };
    let mut quals = Vec::new();
    if let Some(f) = &scan.filter {
        conjuncts(f, &mut quals);
    }
    let fixed = |c: usize| {
        quals.iter().any(|q| match q {
            Expr::Call(d, args, _) if d.name == "=" && args.len() == 2 => {
                let is_col = |e: &Expr| matches!(strip_relabel(e), Expr::Col(i, _) if *i == c);
                let is_const = |e: &Expr| matches!(strip_relabel(e), Expr::Const(..));
                (is_col(&args[0]) && is_const(&args[1])) || (is_col(&args[1]) && is_const(&args[0]))
            }
            _ => false,
        })
    };
    let mut any = false;
    for e in &exprs {
        match e {
            Expr::Col(c, _) => {
                if !fixed(*c) {
                    return false;
                }
                any = true;
            }
            e if crate::sql::binder::optimize::pseudo_constant(e) => {}
            _ => return false,
        }
    }
    any
}

/// A Limit over `c` (adjust_limit_rows_costs): `count` / `off` are the constant LIMIT and
/// OFFSET (0: none, -1: not constant). A Sort under a constant LIMIT keeps only the rows it
/// needs.
/// A LockRows's rows' marks (their ctids), resjunk in its output, and in the output of the
/// Limits over it, which return their input's target list: the marks it added.
fn lock_junk(node: &mut ExNode) -> Vec<String> {
    let junk: Vec<String> = match node.node_type {
        "LockRows" => node
            .children
            .first()
            .map(|c| c.output.iter().filter(|o| o.ends_with("ctid")).cloned().collect())
            .unwrap_or_default(),
        "Limit" if node.children.len() == 1 => lock_junk(&mut node.children[0]),
        _ => Vec::new(),
    };
    for j in &junk {
        if !node.output.contains(j) {
            node.output.push(j.clone());
        }
    }
    junk
}

fn limit_over(mut c: ExNode, count: f64, off: f64) -> ExNode {
    let mut n = ExNode::new("Limit");
    // ExecSetTupleBound: the sort below (through a subquery's scan without quals) keeps
    // only the rows the LIMIT reads, in a bounded heap.
    if count > 0.0 && off >= 0.0 {
        let mut t = &mut c;
        loop {
            match t.node_type {
                "Sort" => {
                    t.bound = Some(count + off);
                    break;
                }
                "Subquery Scan"
                    if !t.details.iter().any(|d| matches!(d, Detail::Text("Filter", _))) && t.children.len() == 1 =>
                {
                    t = &mut t.children[0];
                }
                _ => break,
            }
        }
    }
    if c.node_type == "Sort"
        && count > 0.0
        && off >= 0.0
        && let Some(input) = c.children.first()
    {
        let (st, tot) = costs::sort_cost(input.total_cost, input.rows, input.width, Some(count + off));
        c.startup_cost = st;
        c.total_cost = tot;
    }
    let in_rows = c.rows;
    let run = c.total_cost - c.startup_cost;
    n.rows = in_rows;
    n.startup_cost = c.startup_cost;
    n.total_cost = c.total_cost;
    if off != 0.0 {
        let o = if off > 0.0 { off } else { costs::clamp_rows(in_rows * 0.1) }.min(n.rows);
        if in_rows > 0.0 {
            n.startup_cost += run * o / in_rows;
        }
        n.rows = (n.rows - o).max(1.0);
    }
    if count != 0.0 {
        let k = if count > 0.0 { count } else { costs::clamp_rows(in_rows * 0.1) }.min(n.rows);
        if in_rows > 0.0 {
            n.total_cost = n.startup_cost + run * k / in_rows;
        }
        n.rows = k.max(1.0);
    }
    n.width = c.width;
    n.cols = c.cols.clone();
    n.children.push(c.child("Outer"));
    n
}

/// How a node above refers to an output its input shows as `o`: as is when that is a
/// column (or already a reference to a value computed further down), parenthesized when
/// it is a value the input computes.
fn refer_to(o: &str) -> String {
    // (A whole-row Var, `r.*`, is a column too.)
    let o_col = o.strip_suffix(".*").unwrap_or(o);
    let column = !o_col.is_empty()
        && o_col.split('.').all(|p| {
            !p.is_empty()
                && (p.starts_with('"') && p.ends_with('"')
                    || p.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$'))
                && !p.chars().next().is_some_and(|c| c.is_ascii_digit())
        });
    if column || (o.starts_with('(') && o.ends_with(')')) || quoted_constant(o) {
        o.to_string()
    } else {
        format!("({o})")
    }
}

/// `'text'::type`: a constant, which setrefs matches to no subplan output (it stays).
fn quoted_constant(o: &str) -> bool {
    let Some(body) = o.strip_prefix('\'') else {
        return false;
    };
    let mut chars = body.char_indices().peekable();
    let mut end = None;
    while let Some((i, c)) = chars.next() {
        if c == '\'' {
            if chars.peek().is_some_and(|(_, n)| *n == '\'') {
                chars.next();
                continue;
            }
            end = Some(i + 1);
            break;
        }
    }
    let Some(end) = end else { return false };
    body[end..].strip_prefix("::").is_some_and(|t| {
        !t.is_empty() && t.chars().all(|c| c.is_alphanumeric() || " _.\"[](),".contains(c))
    })
}

/// ExecSupportsBackwardScan over the explained plan.
fn backward_ok(n: &ExNode) -> bool {
    match n.node_type {
        "Result" | "Limit" | "LockRows" | "Subquery Scan" => n
            .children
            .iter()
            .find(|c| matches!(c.relationship, Some("Outer" | "Subquery")))
            .is_some_and(backward_ok),
        "Append" => n.children.iter().filter(|c| c.relationship == Some("Member")).all(backward_ok),
        "Seq Scan" | "Tid Scan" | "Tid Range Scan" | "Function Scan" | "Values Scan" | "CTE Scan"
        | "Materialize" | "Sort" | "Index Scan" | "Index Only Scan" => true,
        _ => false,
    }
}

/// RETURNING's row with OLD and NEW (`exec::dml::with_old_new`): `base` padded to its
/// width, then the old row's columns (and system columns), whether it exists, the new
/// row's likewise.
fn old_new_names(mut base: Vec<Name>, on: Option<&super::plan::OldNew>, t: &crate::catalog::Table) -> Vec<Name> {
    let Some(on) = on else { return base };
    base.resize(on.width, Name::same("?"));
    // (A rule action's own OLD / NEW rows, which the statement's RETURNING reads as its
    // old and new: rules.rs's internal names.)
    let shown = |a: &'static str| match a {
        "*OLD ROW*" => "old",
        "*NEW ROW*" => "new",
        a => a,
    };
    for side in [shown(on.old_alias), shown(on.new_alias)] {
        let mut cols: Vec<Name> = t
            .columns
            .iter()
            .map(|c| Name::same(format!("{side}.{}", quote_ident(&c.name))))
            .collect();
        if on.sys {
            for sc in ["ctid", "xmin", "cmin", "xmax", "cmax", "tableoid"] {
                cols.push(Name::same(format!("{side}.{sc}")));
            }
        }
        cols.resize(on.ncols, Name::same("?"));
        base.extend(cols);
        base.push(Name::same(format!("{side}.*")));
    }
    base
}

/// RETURNING's whole OLD / NEW row (NULL when there is none): shown as `old.*`.
fn old_new_rows(e: &Expr, on: Option<&super::plan::OldNew>) -> Expr {
    let Some(on) = on else { return e.clone() };
    let flags = [on.width + on.ncols, on.width + 2 * on.ncols + 1];
    e.clone()
        .transform(&mut |x| {
            Ok(match x {
                Expr::Case {
                    operand: None,
                    whens,
                    else_,
                    ty,
                } if whens.len() == 1
                    && matches!(&whens[0].0, Expr::Col(f, _) if flags.contains(f))
                    && else_
                        .as_deref()
                        .is_none_or(|el| matches!(el, Expr::Const(crate::types::Value::Null, _))) =>
                {
                    let _ = ty;
                    whens.into_iter().next().expect("one").1
                }
                x => x,
            })
        })
        .unwrap_or_else(|_| e.clone())
}
