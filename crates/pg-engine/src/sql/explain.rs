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
    /// A node with a Gather in it: the cheapest plan of the same rows a worker could run
    /// (get_cheapest_parallel_safe_total_inner), for a Parallel Append to share out whole.
    safe_alt: Option<Box<ExNode>>,
    /// A Sort a LIMIT above bounds (ExecSetTupleBound): the rows it keeps.
    bound: Option<f64>,
}

impl ExNode {
    fn new(node_type: &'static str) -> ExNode {
        ExNode {
            node_type,
            title: node_type.to_string(),
            ..ExNode::default()
        }
    }

    fn child(mut self, rel: &'static str) -> ExNode {
        self.relationship = Some(rel);
        self
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
    /// Correlated EXISTS sub-selects with a hashed alternative (AlternativeSubPlan), by
    /// subquery id: each place reading one picks the alternative for how often it runs
    /// there (fix_alternative_subplan), an inheritance tree's members each their own.
    alts: HashMap<usize, AltSubPlan>,
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
        for c in plan_inputs(p) {
            marks(c, out);
        }
    }
    fn go(p: &Plan, out: &mut std::collections::HashSet<usize>) {
        match p {
            Plan::Update(u) if !matches!(&*u.source, Plan::Scan(_)) => marks(&u.source, out),
            Plan::Delete(d) if !matches!(&*d.source, Plan::Scan(_)) => marks(&d.source, out),
            Plan::Merge(m) => marks(&m.source, out),
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
    needed_masks_of(root)
}

fn needed_masks_of(root: &Plan) -> HashMap<usize, Vec<bool>> {
    fn go(p: &Plan, needed: Vec<bool>, out: &mut HashMap<usize, Vec<bool>>) {
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
                    .map(|(i, n)| *n || keys.get(i).copied().unwrap_or(false) || volatile.get(i).copied().unwrap_or(false))
                    .collect();
                if let Some((pin, _)) = barrier_junk(input, *sorted) {
                    let mut inner = super::paths::child_needs(input, &mask).pop().unwrap_or_default();
                    let pkeys = level_keys(pin, *sorted);
                    for (n, k) in inner.iter_mut().zip(pkeys) {
                        *n |= k;
                    }
                    out.insert(addr(p), needed.clone());
                    out.insert(addr(input), mask);
                    go(pin, inner, out);
                    return;
                }
                vec![mask]
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
        out.insert(addr(p), needed);
        let mut es = Vec::new();
        node_exprs(p, &mut es);
        let mut sq = Vec::new();
        for e in es {
            subqueries_in(e, &mut sq);
        }
        for s in sq {
            let w = super::binder::plan_width(&s.plan);
            go(&s.plan, vec![true; w], out);
        }
        if let Plan::With { ctes, .. } = p {
            for (_, c) in ctes {
                let w = super::binder::plan_width(c);
                go(c, vec![true; w], out);
            }
        }
        for (c, n) in plan_inputs(p).into_iter().zip(needs) {
            go(c, n, out);
        }
    }
    let mut out = HashMap::new();
    let w = super::binder::plan_width(root);
    go(root, vec![true; w], &mut out);
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

/// A FROM subquery's plan whose own top projection drops columns its upper nodes carry
/// (resjunk sort or group keys): that projection's input and the columns it keeps.
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
                if demand && (barrier_junk(input, *sorted).is_some() || partial) {
                    out.insert(addr(p), demand);
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
                if !identity {
                    out.insert(addr(input), true);
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
) -> Vec<String> {
    let mut junk = Vec::new();
    for c in &mut n.children {
        junk.extend(junk_layout(c, top, last, junk_of));
    }
    if n.is_scan()
        && let Some(js) = n.key.and_then(|k| junk_of.get(&k))
    {
        junk.extend(js.iter().map(|j| j.1.clone()));
    }
    if n.output.is_empty() {
        return junk;
    }
    for j in &junk {
        if !n.output.contains(j) {
            n.output.push(j.clone());
        }
    }
    let rank: Box<dyn Fn(&String) -> usize> = if n.children.is_empty() {
        // (An inheritance member's row identity comes after its reltarget's columns.)
        Box::new(|o: &String| {
            if last.contains(o) {
                usize::MAX
            } else {
                top.iter().position(|t| mentions(t, o)).unwrap_or(usize::MAX - 1)
            }
        })
    } else {
        let below: Vec<String> = n.children.iter().flat_map(|c| c.output.iter().cloned()).collect();
        Box::new(move |o: &String| below.iter().position(|t| t == o).unwrap_or(usize::MAX))
    };
    n.output.sort_by_key(|o| rank(o));
    junk
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

/// The target lists below a query level's top node (whose list is the level's target
/// list) in Postgres's order: a scan whose consumer wants a small target list (a Sort, a
/// Hash, a Materialize or Memoize) returns its reltarget, what the level's target list
/// reads of it first, in that order, then what its clauses read; a join its outer side's
/// columns, then its inner side's (build_joinrel_tlist); a node passing its input's rows on
/// keeps that order. (Other scans return their physical target list, in column order.)
fn tlist_layout(top: &mut ExNode) {
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
    fn go(n: &mut ExNode, top: &[String], small_parent: bool) {
        if matches!(n.node_type, "Append" | "Merge Append" | "Subquery Scan" | "Gather" | "Gather Merge") {
            return;
        }
        let s = small(n.node_type);
        for c in &mut n.children {
            if matches!(c.relationship, Some("Outer" | "Inner")) {
                go(c, top, s);
            }
        }
        if n.output.len() < 2 {
            return;
        }
        let rank = |o: &String| top.iter().position(|t| mentions(t, o)).unwrap_or(usize::MAX);
        if n.is_scan() {
            // (Only a scan returning plain columns: one computing values returns them
            // as the level's target list has them.)
            if small_parent && n.node_type != "Index Only Scan" && n.output.iter().all(|o| rel_of(o).is_some()) {
                n.output.sort_by_key(rank);
            }
            return;
        }
        if n.is_join() {
            // (Each relation's columns in its reltarget's order; the relations, and the
            // values computed or referred to, where they are.)
            let mut groups: Vec<Option<String>> = Vec::new();
            let mut keys: Vec<(usize, usize)> = Vec::new();
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
                        keys.push((g, rank(o)));
                    }
                    None => {
                        groups.push(None);
                        keys.push((groups.len() - 1, i));
                    }
                }
            }
            let mut items: Vec<((usize, usize), String)> = keys.into_iter().zip(std::mem::take(&mut n.output)).collect();
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
            go(c, &t, s);
        }
    }
}

/// Whether the expression text `t` reads the column `o` (`ref.col`).
fn mentions(t: &str, o: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '.' || c == '"';
    t.match_indices(o).any(|(i, _)| {
        !t[..i].chars().next_back().is_some_and(ident) && !t[i + o.len()..].chars().next().is_some_and(ident)
    })
}

/// The row mark columns of the scans at or under `n`, in range table order.
fn junk_in(n: &ExNode, junk_of: &HashMap<usize, Vec<((u8, super::plan::Rti), String)>>) -> Vec<String> {
    fn go(
        n: &ExNode,
        junk_of: &HashMap<usize, Vec<((u8, super::plan::Rti), String)>>,
        out: &mut Vec<((u8, super::plan::Rti), String)>,
    ) {
        for c in &n.children {
            go(c, junk_of, out);
        }
        if n.is_scan()
            && let Some(js) = n.key.and_then(|k| junk_of.get(&k))
        {
            out.extend(js.iter().cloned());
        }
    }
    let mut out = Vec::new();
    go(n, junk_of, &mut out);
    out.sort_by_key(|j| j.0);
    out.into_iter().map(|j| j.1).collect()
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
    let own = match p {
        Plan::Append { inh: Some(_), inputs } if inputs.len() == 1 => 1,
        _ => 0,
    };
    own + plan_inputs(p).into_iter().map(|c| inh_parents(c)).sum::<usize>()
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
    assign_refnames_skip(root, ctx, &[])
}

/// The same, the inheritance Appends at `skip` (grouped partitionwise) taking no name:
/// their members are the parent's scans then.
fn assign_refnames_skip(
    root: &Plan,
    ctx: Option<&ExecCtx>,
    skip: &[usize],
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
    let target = match root {
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
    let shown = shown_subqueries(root, &needed_masks_of(root));
    let mut queue: Vec<&Plan> = vec![root];
    let mut qi = 0;
    // (A partition grouped partitionwise that is itself partitioned: its Append, though
    // not in the plan, takes a name before its leaves.)
    let mut mid_first: HashMap<usize, Vec<String>> = HashMap::new();
    let mut fake = usize::MAX;
    let mut inline_barriers: std::collections::HashSet<usize> = std::collections::HashSet::new();
    while qi < queue.len() {
        let mut stack = vec![queue[qi]];
        qi += 1;
        let mut later: Vec<&Plan> = Vec::new();
        // (Subquery levels: named before the level's SubPlans and CTEs, as setrefs reaches
        // them in the plan, the subplans after the whole plan.)
        let mut later_sub: Vec<&Plan> = Vec::new();
        while let Some(p) = stack.pop() {
            if let Plan::Append { inh: Some(n), inputs } = p
                && skip.contains(&addr(p))
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
                if let Plan::Append { inh: Some(n), inputs } = a
                    && let Some(order) = crate::sql::paths::partition_order(input, &exprs, &dirs)
                {
                    fn mark(
                        o: &[crate::sql::paths::PartOrder],
                        members: &[&Arc<Plan>],
                        n: &str,
                        out: &mut HashMap<usize, Vec<String>>,
                    ) -> usize {
                        use crate::sql::paths::PartOrder;
                        let mut low = usize::MAX;
                        for m in o {
                            let first = match m {
                                PartOrder::Leaf(i) => *i,
                                PartOrder::Merge(v) => {
                                    let f = v.iter().copied().min().unwrap_or(usize::MAX);
                                    if let Some(x) = members.get(f) {
                                        out.entry(addr(x)).or_default().push(n.to_string());
                                    }
                                    f
                                }
                                PartOrder::Append(sub) => {
                                    // (Its name before its own sub-partitions'.)
                                    let mut inner = HashMap::new();
                                    let f = mark(sub, members, n, &mut inner);
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
                    if crate::sql::paths::count_members(&order) == members.len() {
                        match order.as_slice() {
                            [crate::sql::paths::PartOrder::Merge(_)] => {}
                            [crate::sql::paths::PartOrder::Append(sub)] => {
                                mark(sub, &members, n, &mut mid_first);
                            }
                            o => {
                                mark(o, &members, n, &mut mid_first);
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
                    results += rels.len();
                    None
                }
                Plan::Insert(i) => Some(i.alias.clone().unwrap_or_else(|| i.rules.table.name.clone())),
                // The parent's own name; its member scans are `name_1`, `name_2`, …. (A single
                // member's Append is dropped, and it takes the name itself.)
                Plan::Append {
                    inh: Some(n),
                    inputs,
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
                {
                    later.push(&*s.plan);
                }
            }
            if let Plan::With { ctes, .. } = p {
                later.extend(ctes.iter().map(|(_, c)| &**c));
            }
            let mut inputs = plan_inputs(p);
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
            if let Plan::SetOp { .. } | Plan::Append { inh: None, .. } = p {
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
            if let (Some(ctx), Plan::Append { inputs: all, inh: Some(_) }) = (ctx, p)
                && !matches!(all.first().map(|i| &**i), Some(Plan::Update(_) | Plan::Delete(_)))
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
    let mut levels: Vec<Vec<&Plan>> = vec![Vec::new()];
    level_scans(root, 0, &mut levels);
    let target = target.map(addr);
    for level in levels {
        let mut groups: Vec<(String, Vec<(crate::sql::plan::Rti, usize)>)> = Vec::new();
        for p in level {
            let Plan::Scan(s) = p else { continue };
            if s.rti.0 == 0 || Some(addr(p)) == target || !out.contains_key(&addr(p)) {
                continue;
            }
            let base = s.alias.clone().unwrap_or_else(|| s.table.name.clone());
            match groups.iter_mut().find(|(b, _)| *b == base) {
                Some((_, v)) => v.push((s.rti, addr(p))),
                None => groups.push((base, vec![(s.rti, addr(p))])),
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
    (out, results)
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
fn level_scans<'p>(p: &'p Plan, cur: usize, levels: &mut Vec<Vec<&'p Plan>>) {
    match p {
        Plan::Scan(s) if s.part.is_none() => levels[cur].push(p),
        Plan::Append { inh: Some(_), .. } => {}
        Plan::Barrier { input, .. } => {
            levels.push(Vec::new());
            let n = levels.len() - 1;
            level_scans(input, n, levels);
        }
        // (A set operation's arms are subqueries: levels of their own.)
        Plan::SetOp { .. } | Plan::Append { inh: None, .. } => {
            for c in plan_inputs(p) {
                levels.push(Vec::new());
                let n = levels.len() - 1;
                level_scans(c, n, levels);
            }
        }
        _ => {
            for c in plan_inputs(p) {
                level_scans(c, cur, levels);
            }
        }
    }
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
        } => inputs.iter().for_each(|i| partitionwise_parents(i, true, out, skip)),
        // (A partitionwise join of one partition pair has no Append: its partitions keep
        // their parents' names.)
        // (One grouped partitionwise: its parents go unnamed.)
        Plan::Append { inh: None, .. } if skip.contains(&addr(p)) => {}
        Plan::Append { inputs, inh: None } if inputs.len() > 1 => inputs
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
    // A key sorted under its (non-byte-order) collation shows it.
    if let Some(coll) = k.using.as_ref().and_then(|op| op.coll.as_ref()) {
        let _ = write!(s, " COLLATE {}", crate::catalog::quote_ident(&coll.name));
        if k.desc {
            s.push_str(" DESC");
        }
    } else if let Some(op) = &k.using {
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
            let fixed = super::paths::constant_columns(input);
            let is_fixed = |g: &Expr| implicit_key_col(g).is_some_and(|j| fixed.get(j).copied().unwrap_or(false));
            if !groups.iter().all(is_fixed) {
                for (k, g) in groups.iter().enumerate() {
                    if is_fixed(g) {
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
        // GROUPING(keys): a test of the grouping set mask column, shown by the keys.
        let is_grouping = |x: &Expr| {
            matches!(x, Expr::Call(d, args, _) if d.oid == 0 && d.name == "grouping"
                && matches!(args.first(), Some(Expr::Col(m, _)) if cols.get(*m).is_some_and(|n| n.plain == "GROUPING")))
        };
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
                    let Expr::Call(_, args, _) = &x else { unreachable!() };
                    let keys: Vec<&Name> = args[2..]
                        .iter()
                        .filter_map(|a| match a {
                            Expr::Const(v, _) => v.as_i64().and_then(|k| cols.get(k as usize)),
                            _ => None,
                        })
                        .collect();
                    let list = |f: &dyn Fn(&Name) -> &str| keys.iter().map(|k| f(k)).collect::<Vec<_>>().join(", ");
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
        let n = self.name(e, cols, &[]);
        self.pick(&n, cx)
    }

    /// Number and convert the subqueries in `e` (whose columns are `cols`).
    fn register(&mut self, e: &Expr, cols: &[Name]) {
        let mut sq = Vec::new();
        subqueries_in(e, &mut sq);
        for s in sq {
            if !self.labels.contains_key(&s.id) {
                self.subquery(s, cols);
            } else if let Some(alt) = self.alts.get_mut(&s.id) {
                // (Another place reading it: its own choice.)
                let i = alt.pick(self.num_exec.unwrap_or(0.0));
                let (label, node) = alt.get(i).clone();
                if !alt.shown[i] {
                    alt.shown[i] = true;
                    self.pending.push(node);
                }
                self.labels.insert(s.id, label);
            }
        }
    }

    fn subquery(&mut self, s: &Subquery, cols: &[Name]) {
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
        let mut node = match &test {
            Some((input, _)) => self.level(input),
            None => self.level(&s.plan),
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
        self.plan_ids += 1;
        let id = self.plan_ids;
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
                                    && hashable_key(&a.ty())
                            }
                            _ => false,
                        })
                }
                // (An anonymous record's equality isn't hashable: bug #17363.)
                None => {
                    s.ops.iter().all(|o| o.name == "=")
                        && s.left.iter().all(|l| match l.ty() {
                            DataType::Record => false,
                            DataType::Row(r) => r.oid != 0,
                            _ => true,
                        })
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
            // Its scan is the plain plan's under the same name.
            if let (Plan::Project { input, .. }, Some(orig)) = (&*hplan, scan_below(&s.plan))
                && let Some(name) = self.refnames.get(&addr(orig)).cloned()
            {
                self.refnames.insert(addr(input), name);
            }
            // (An inheritance tree's copy is range table entries of its own: the parent's
            // next number, then its members'.)
            if let Plan::Append { inputs, inh: Some(base) } = &*hplan {
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
            self.plan_ids += 1;
            let hashed_id = self.plan_ids;
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
        if init {
            self.inits.last_mut().expect("query level").push(node);
        } else {
            self.pending.push(node);
        }
    }

    /// A query level (the statement, a CTE or a subquery): its root carries its InitPlans.
    fn level(&mut self, p: &Arc<Plan>) -> ExNode {
        self.inits.push(Vec::new());
        let saved_depth = std::mem::take(&mut self.proj_depth);
        self.level_tops.insert(addr(p));
        let mut node = self.node(p).node;
        self.proj_depth = saved_depth;
        self.settle(&mut node);
        if self.o.verbose {
            tlist_layout(&mut node);
        }
        let mut inits = self.inits.pop().unwrap_or_default();
        // SS_charge_for_initplans: the level's top node pays for its InitPlans.
        let cost: f64 = inits.iter().filter(|i| !i.charged).map(|i| i.total_cost).sum();
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
        let inits_before = self.inits.last().map_or(0, |v| v.len());
        let apart = matches!(&**p, Plan::Barrier { .. });
        self.depth += apart as usize;
        let mut b = self.convert(p);
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
            let cost: f64 = moved.iter().filter(|i| !i.charged).map(|i| i.total_cost).sum();
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
        // (Its partial form counts as it: the same node runs.)
        let k = b.node.key;
        if let Some(pt) = &mut b.node.partial
            && pt.key.is_none()
        {
            pt.key = k;
        }
        // (A join whose columns nothing above reads is 0 wide.)
        if b.node.width == 0 && !b.node.is_join() {
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
            let marked = b.node.is_scan() && self.rowmarks.contains(&addr(p));
            let mut mask = (b.node.is_join() || b.node.node_type == "Index Only Scan" || dummy || member || sorted_join || srf_target || marked)
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
            b.node.output = b
                .internal
                .iter()
                .enumerate()
                .filter(|(i, _)| mask.as_ref().is_none_or(|m| m.get(*i).copied().unwrap_or(true)))
                .map(|(_, n)| self.pick(n, Cx::Output))
                .collect();
            if let Some(pt) = &mut b.node.partial
                && pt.output.is_empty()
            {
                pt.output = b.node.output.clone();
            }
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
            Plan::Barrier { input, alias, cols, secure: false, sorted } if !alias.is_empty() => {
                let junk = barrier_junk(input, *sorted);
                let Some(&demand) = self.shown.get(&key) else {
                    // (Its plan returns its resjunk columns as well, past the ones the
                    // query above reads.)
                    if let Some((pin, exprs)) = junk {
                        let c = self.node(pin);
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
                        }
                        return Built { node, internal, exported };
                    }
                    // (Its scan trivial, the order it offers is still the subquery's: its
                    // ORDER BY's keys, truncate_useless_pathkeys.)
                    let mut b = self.convert(input);
                    if let Some(n) = sorted {
                        b.node.order.truncate(*n);
                    }
                    return b;
                };
                let needed = self.needed.get(&key).cloned();
                let c = match junk {
                    Some((pin, _)) => self.node(pin),
                    None => self.node(input),
                };
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
                let mut c = c;
                let alt = c.node.alt.take();
                let child_cols = c.node.cols.clone();
                let info = Arc::new((alias.clone(), cols.clone()));
                let saved = self.sub_alias.replace(info);
                let mut b = self.subquery_scan(c, key);
                self.sub_alias = saved;
                if let (Some(a), Some(ao)) = (alt, alt_order) {
                    let mut n = b.node.clone();
                    n.startup_cost = a.startup_cost;
                    n.total_cost = a.total_cost + a.rows * costs::CPU_TUPLE_COST;
                    n.order = ao;
                    n.children = vec![a.child("Subquery")];
                    b.node.alt = Some(Box::new(n));
                }
                if child_cols.len() >= map.len() {
                    b.node.cols = map.iter().map(|&j| child_cols.get(j).cloned().unwrap_or_default()).collect();
                    if let Some(a) = &mut b.node.alt {
                        a.cols = b.node.cols.clone();
                    }
                }
                b.node.order = order;
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
                // relations scanned, or every one when none is.)
                let named = self.refnames.is_empty();
                let rel = |i: usize| {
                    info.as_ref()
                        .filter(|_| named)
                        .and_then(|d| d.rels.get(i).cloned().flatten())
                };
                let names = match cols {
                    Some(c) if c.len() == *ncols => c
                        .iter()
                        .enumerate()
                        .map(|(i, x)| match rel(i) {
                            Some(r) => Name {
                                plain: x.clone(),
                                full: format!("{}.{}", quote_ident(&r), x),
                            },
                            None => Name::same(x.clone()),
                        })
                        .collect(),
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
            Plan::Values { rows, ncols } => {
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
                n.rows = 100.0;
                n.total_cost = 3.0;
                n.width = calls.iter().map(|c| type_width(&c.ty)).sum();
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
                if self.o.verbose {
                    for c in calls {
                        let call = Expr::Call(c.func.clone(), c.args.clone(), c.ty.clone());
                        let t = self.show(&call, &[], Cx::Scan);
                        n.details.push(Detail::Text("Function Call", t));
                    }
                }
                // cost_functionscan: the calls once, then a tuple per row.
                n.rows = calls
                    .iter()
                    .map(|c| costs::function_rows(c.func.name, &c.args, c.func.srf.is_some()))
                    .fold(1.0, f64::max);
                n.startup_cost = costs::CPU_OPERATOR_COST * calls.len() as f64;
                n.total_cost = n.startup_cost + n.rows * costs::CPU_TUPLE_COST;
                n.width = calls.iter().map(|c| type_width(&c.ty)).sum();
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
                let saved_fw = FILTER_OVER_WINDOW.with(|f| f.replace(subquery_of(input).is_some()));
                let mut c = self.node(input);
                FILTER_OVER_WINDOW.with(|f| f.set(saved_fw));
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
                    self.fold_filter(c, pred, key, setop)
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
                let mut c = self.node(input);
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
                if self.o.verbose
                    && (addr(p) == self.top || self.level_tops.contains(&addr(p)))
                    && (matches!(c.node.node_type, "Sort" | "Incremental Sort" | "Limit")
                        || c.node.node_type == "Result" && matches!(&**input, Plan::Project { .. }))
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
                let append_above = (self.o.verbose && matches!(&**input, Plan::Append { inh: Some(_), .. })
                    || matches!(&**input, Plan::Append { inh: None, .. }))
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
                let exported = exprs
                    .iter()
                    .zip(&internal)
                    .map(|(e, n)| match e {
                        Expr::Col(j, _) => c.exported.get(*j).cloned().unwrap_or_else(|| n.clone()),
                        _ => n.wrapped(),
                    })
                    .collect();
                if c.node.node_type == "Aggregate" {
                    // (ExecInitAgg initializes the projection before the aggregates'
                    // arguments: its SubPlans come first.)
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
                    _ => 0,
                };
                c.node.width = cols
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| mask.is_none_or(|m| m.get(*i).copied().unwrap_or(true)))
                    .map(|(_, c)| c.width)
                    .sum::<u32>()
                    + junk_keys;
                if let Some(pt) = &mut c.node.partial {
                    pt.width = c.node.width;
                    pt.cols = cols.clone();
                }
                c.node.cols = cols;
                // (cost_qual_eval: a called function's procost too.)
                let per: f64 = exprs.iter().map(costs::qual_cost).sum();
                c.node.total_cost += c.node.rows * per;
                if let Some(pt) = &mut c.node.partial {
                    pt.total_cost += pt.rows * per;
                }
                // Sub-selects in the target list (cost_subplan), per row.
                let (sp_startup, sp_per) = subplan_costs(exprs, self.ctx);
                c.node.startup_cost += sp_startup;
                c.node.total_cost += sp_startup + c.node.rows * sp_per;
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
                    if let Some(pt) = &mut c.node.partial {
                        pt.output = c.node.output.clone();
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
                let mut n = ExNode::new("ProjectSet");
                n.rows = c.node.rows * 10.0;
                n.total_cost = c.node.total_cost + n.rows * 0.02;
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
                self.group_order_full = self.group_order.as_ref().is_some_and(|g| g.len() == keys.len());
                let saved_members = MERGE_MEMBERS.with(|m| m.replace(merge_appended(input)));
                let mut c = self.node(input);
                MERGE_MEMBERS.with(|m| m.set(saved_members));
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
                    if let Some(pt) = &mut c.node.partial {
                        pt.output = c.node.output.clone();
                    }
                }
                let part = c.node.partial.clone();
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
                        let t = self.show(&k.expr, &c.exported, Cx::Upper);
                        let t = if member_sort && !matches!(k.expr, Expr::Col(..)) && c.node.is_scan() {
                            format!("({t})")
                        } else {
                            t
                        };
                        t + &sort_suffix(k)
                    })
                    .collect();
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
                    if let Some(order) = crate::sql::paths::partition_order(input, &exprs, &dirs)
                        && crate::sql::paths::count_members(&order) == kids.len()
                    {
                        let mut slots: Vec<Option<ExNode>> = kids.into_iter().map(Some).collect();
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
                // The input's other path, when it comes in this order and costs less than
                // sorting this one.
                if let Some(alt) = c.node.alt.take() {
                    let sorted = alt.order.len() >= list.len() && alt.order[..list.len()] == list[..];
                    if sorted {
                        let mut sn = ExNode::new("Sort");
                        self.sort_costs(&mut sn, &c.node);
                        if parallel::disabled_nodes(&alt) <= parallel::disabled_nodes(&c.node)
                            && alt.total_cost <= sn.total_cost
                        {
                            c.node = *alt;
                        }
                    }
                }
                let common = list
                    .iter()
                    .zip(&c.node.order)
                    .enumerate()
                    .take_while(|(oi, (a, b))| {
                        a == b || c.node.order_eq.get(*oi).is_some_and(|e| e.contains(a))
                    })
                    .count();
                // Already in that order: no sort; partly: an incremental one.
                if common == list.len() && !list.is_empty() {
                    return c;
                }
                if common > 0 && !member_sort && crate::sql::paths::Gucs::read(self.ctx).incremental_sort {
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
                    return Built::passthrough(n, c.exported);
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
                n.children.push(c.node.child("Outer"));
                // Or each worker sorts its share, merged by a Gather Merge.
                if let Some(pt) = part
                    && self.par.as_ref().is_some_and(|p| p.gather_merge)
                {
                    let mut sn = ExNode::new("Sort");
                    sn.details.push(Detail::List("Sort Key", list.clone()));
                    sn.details.push(Detail::SortInfo);
                    self.sort_costs(&mut sn, &pt);
                    sn.order = list;
                    sn.workers = pt.workers;
                    // (The sort that ran: its counters.)
                    sn.key = Some(key);
                    sn.disabled = !crate::sql::paths::Gucs::read(self.ctx).sort;
                    if self.o.verbose {
                        sn.output = pt.output.clone();
                    }
                    sn.children.push(pt.child("Outer"));
                    // (Over a relation's partial plan sorted for the query's order:
                    // generate_useful_gather_paths, the relation's rows.)
                    let mut gm = if matches!(&**input, Plan::Scan(_) | Plan::Join { .. }) {
                        self.gather_merge_rows(&sn, n.rows)
                    } else {
                        self.gather_merge_of(&sn)
                    };
                    if parallel::par_beats(&gm, &n) {
                        gm.alt = Some(Box::new(n));
                        return Built::passthrough(gm, c.exported);
                    }
                    n.alt = Some(Box::new(gm));
                }
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
                let alt = c.node.alt.take();
                let mut n = limit_over(c.node, count, off);
                if let Some(a) = alt {
                    let m = limit_over(*a, count, off);
                    if parallel::cheaper(&m, &n) {
                        n = m;
                    }
                }
                Built::passthrough(n, c.exported)
            }
            Plan::Distinct { input, on, .. } => {
                // (Hashed: sorted, the Sort's input is the projection.)
                let plain = on.is_none()
                    && crate::sql::paths::Gucs::read(self.ctx).hashagg
                    && matches!(&**input, Plan::Project { input: b, exprs }
                        if matches!(&**b, Plan::Barrier { secure: false, alias, .. } if !alias.is_empty())
                            && exprs.iter().enumerate().all(|(i, e)| {
                                matches!(e, Expr::Col(..)) && !exprs[..i].iter().any(|x| x.same_as(e))
                            }));
                let saved_plain = std::mem::replace(&mut self.plain_distinct, plain.then(|| addr(input)));
                let mut c = self.node(input);
                self.plain_distinct = saved_plain;
                // (Its partial plan: a DISTINCT in the workers first, create_partial_distinct_paths.)
                let part = c.node.partial.clone();
                self.settle(&mut c.node);
                // A hashed DISTINCT reads a plain scan's physical target list (its select
                // list of columns is the aggregate's).
                // (A semi join's side made unique reads just the compared values.)
                let semi_unique = self.unique_sides.contains(&key);
                // (Sorted instead, the Sort's input is its exact target list.)
                let exact_out = c.node.output.clone();
                let mut physical = false;
                if self.o.verbose && on.is_none() && semi_unique && c.node.is_scan() {
                    c.node.output = c.internal.iter().map(|n| self.pick(n, Cx::Output)).collect();
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
                let mut n = if on.is_some() {
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
                if on.is_some() {
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
                let on_fixed = on.as_ref().is_some_and(|on| {
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
                    let mut ord: Vec<String> = Vec::new();
                    // (In either direction: a backward scan's order serves as well.)
                    for o in &c.node.order {
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
                    let mut input_node = merged.unwrap_or_else(|| c.node.clone());
                    if physical {
                        input_node.output = exact_out.clone();
                    }
                    if common < ord.len() {
                        let incremental = common > 0 && g.incremental_sort;
                        let mut sn = ExNode::new(if incremental { "Incremental Sort" } else { "Sort" });
                        sn.details.push(Detail::List("Sort Key", ord.clone()));
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
                    n.disabled = !g.hashagg;
                    Some(u)
                } else {
                    None
                };
                n.children.push(c.node.child("Outer"));
                // The rows made distinct in the workers, then again over what they send.
                if on.is_none()
                    && let Some(u) = sorted.as_ref()
                    && let Some(pt) = part.as_deref()
                    && self.par.is_some()
                    && !keys.is_empty()
                    && let Some(best) = self.parallel_distinct(&n, u, pt, &keys, key_ops, match &**input {
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
                    if !parallel::unsorted_kept(&n, &u) || parallel::cheaper(&u, &n) {
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
                let can_hash = types.iter().all(setop_hashable);
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
                let can_hash = types.iter().all(setop_hashable);
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
                self.inherited_dml(key, "Update", &up.rules.table, &members, &up.sets, &up.returning, up.old_new.as_ref(), up.sys, &up.source)
            }
            Plan::Delete(dp)
                if dp.inh
                    && dp.members.iter().map(|m| &m.table).chain([&dp.table])
                        .any(|t| target_scan(&dp.source, t.oid).is_some()) =>
            {
                let members: Vec<Arc<crate::catalog::Table>> =
                    dp.members.iter().map(|m| m.table.clone()).collect();
                self.inherited_dml(key, "Delete", &dp.table, &members, &[], &dp.returning, dp.old_new.as_ref(), dp.sys, &dp.source)
            }
            // A single member needs no Append (Postgres removes it).
            Plan::Append {
                inputs,
                inh: Some(_),
            } if inputs.len() == 1 => self.node(&inputs[0]),
            Plan::Append { inputs, inh } => {
                let mut direct = Vec::new();
                if inh.is_some() {
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
                        if matches!(x, Plan::Scan(_)) {
                            direct.push(addr(x));
                        }
                    }
                    INH_SCANS.with(|m| m.borrow_mut().extend(direct.iter().copied()));
                }
                // Partitions pruned when the executor starts aren't shown.
                let kept = if inh.is_some() {
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
                let mut b = if arms.is_empty() && inputs.len() > 1 && inh.is_some() {
                    let all: Vec<&Arc<Plan>> = inputs.iter().collect();
                    let mut b = self.append_of(&all, true);
                    b.node.children.clear();
                    if let Some(pt) = &mut b.node.partial {
                        pt.children.clear();
                    }
                    b
                } else {
                    self.append_of(&arms, inh.is_some())
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
                    partitionwise_scans(first, &mut scans);
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
                        if n.full != n.plain && n.full.ends_with(&format!(".{}", n.plain)) {
                            n.full = format!("{parent}.{}", n.plain);
                        } else if let Some(m) = &member
                            && n.full != n.plain
                        {
                            n.full = replace_qualifier(&n.full, m, &format!("{parent}."));
                        }
                    }
                }
                if kept.len() < inputs.len() {
                    let d = Detail::Text("Subplans Removed", (inputs.len() - kept.len()).to_string());
                    b.node.details.insert(0, d.clone());
                    if let Some(pt) = &mut b.node.partial {
                        pt.details.insert(0, d);
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
            } => self.join(
                key,
                *kind,
                left,
                right,
                cond.as_ref(),
                hash.as_ref(),
                params,
                meta,
            ),
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
                    self.plan_ids += 1;
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
                                b.node.startup_cost += i.total_cost;
                                b.node.total_cost += i.total_cost;
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
            Plan::LockRows { input, .. } => {
                let c = self.node(input);
                Built::passthrough(lock_rows(c.node), c.exported)
            }
            Plan::Insert(ip) => {
                let mut c = self.node(&ip.source);
                let refname = self.refname(p);
                let t = &ip.rules.table;
                // VERBOSE: the source computes the whole new row, defaults included.
                if self.o.verbose
                    && ip.route.is_none()
                    && ip.copy.is_none()
                    && matches!(c.node.node_type, "Result" | "Values Scan")
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
                if self.o.verbose && !ip.returning.is_empty() {
                    let names = self.table_names(t, &refname);
                    // (The source is a range table entry of its own: columns qualified.)
                    let names: Vec<Name> = names.into_iter().map(|n| Name::same(n.full)).collect();
                    let names = old_new_names(names, ip.old_new.as_ref(), t);
                    n.output = ip
                        .returning
                        .iter()
                        .map(|e| self.show(&old_new_rows(e, ip.old_new.as_ref()), &names, Cx::Output))
                        .collect();
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
                self.returning(&mut n, &ip.returning, t, &c.node);
                n.children.push(c.node.child("Outer"));
                Built::passthrough(n, Vec::new())
            }
            Plan::Update(up) => {
                let mut c = self.node(&up.source);
                let t = &up.rules.table;
                // (No row of it read: the name the statement gives it.)
                let refname = target_scan(&up.source, t.oid)
                    .map(|s| self.refname(s))
                    .or_else(|| self.refnames.get(&key).cloned())
                    .unwrap_or_else(|| up.alias.clone().unwrap_or_else(|| t.name.clone()));
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
                    let ops: usize = up.sets.iter().map(|(_, e)| costs::ops(e)).sum();
                    c.node.total_cost += c.node.rows * costs::CPU_OPERATOR_COST * ops as f64;
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
                    && !up.inh
                    && self.ctx.catalog().children(t.oid).is_empty()
                    && c.node.is_join()
                    && let Some(k) = c.exported.get(t.columns.len())
                {
                    // A join computes them: the new values (in column order), the target
                    // row's ctid, then the other relations' row marks.
                    let mut sets: Vec<&(usize, Expr)> = up.sets.iter().collect();
                    sets.sort_by_key(|x| x.0);
                    let mut top: Vec<String> =
                        sets.iter().map(|(_, e)| self.set_value(e, &c.exported)).collect();
                    top.extend(self.rescans(&sets));
                    // (A view's row identity, for its INSTEAD OF triggers: the whole row.)
                    top.push(if up.rules.view {
                        let cols: Vec<String> = c.exported[..t.columns.len()]
                            .iter()
                            .map(|n| self.pick(n, Cx::Output))
                            .collect();
                        format!("ROW({})", cols.join(", "))
                    } else {
                        self.pick(k, Cx::Output)
                    });
                    top.extend(junk_in(&c.node, &self.rowmark_junk));
                    for ch in &mut c.node.children {
                        junk_layout(ch, &top, &[], &self.rowmark_junk);
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
                self.returning(&mut n, &up.returning, t, &c.node);
                if self.o.verbose && !up.returning.is_empty() {
                    let mut names = self.table_names(t, &refname);
                    if inherited {
                        names = names.into_iter().map(|n| Name::same(n.full)).collect();
                    }
                    names.extend(c.exported.iter().skip(t.columns.len() + 1).cloned());
                    let names = old_new_names(names, up.old_new.as_ref(), t);
                    let cx = if up.old_new.is_some() { Cx::Upper } else { Cx::Output };
                    n.output = up
                        .returning
                        .iter()
                        .map(|e| self.show(&old_new_rows(e, up.old_new.as_ref()), &names, cx))
                        .collect();
                }
                n.children.push(c.node.child("Outer"));
                Built::passthrough(n, Vec::new())
            }
            Plan::Delete(dp) => {
                let mut c = self.node(&dp.source);
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
                    && self.ctx.catalog().children(t.oid).is_empty()
                    && c.node.is_join()
                    && let Some(k) = c.exported.get(t.columns.len())
                {
                    // A join returns the target row's ctid, then the other relations' row
                    // marks.
                    let mut top = vec![self.pick(k, Cx::Output)];
                    top.extend(junk_in(&c.node, &self.rowmark_junk));
                    for ch in &mut c.node.children {
                        junk_layout(ch, &top, &[], &self.rowmark_junk);
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
                self.returning(&mut n, &dp.returning, t, &c.node);
                if self.o.verbose && !dp.returning.is_empty() {
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
                    n.output = dp
                        .returning
                        .iter()
                        .map(|e| self.show(&old_new_rows(e, dp.old_new.as_ref()), &names, cx))
                        .collect();
                }
                n.children.push(c.node.child("Outer"));
                Built::passthrough(n, Vec::new())
            }
            Plan::Merge(mp) => {
                let mut c = self.node(&mp.source);
                if self.o.verbose && !mp.members.is_empty() {
                    tableoid_first(&mut c.node);
                }
                if self.o.verbose && c.node.is_join() {
                    // The processed target list: the target row's identity (first for a
                    // plain table, last for an inheritance tree's members), the source
                    // columns the actions read, the other relations' row marks.
                    let targets: Vec<String> = std::iter::once(mp.rules.table.oid)
                        .chain(mp.members.iter().map(|m| m.rules.table.oid))
                        .filter_map(|o| target_scan(&mp.source, o))
                        .map(|s| format!("{}.", quote_ident(&self.refname(s))))
                        .collect();
                    let (mut ident, mut top) = (Vec::new(), Vec::new());
                    for o in &c.node.output {
                        if !targets.iter().any(|t| o.starts_with(t.as_str())) {
                            top.push(o.clone());
                        } else if o.ends_with(".ctid") || o.ends_with(".tableoid") {
                            ident.push(o.clone());
                        }
                    }
                    top.extend(junk_in(&c.node, &self.rowmark_junk));
                    let last = if mp.members.is_empty() {
                        ident.append(&mut top);
                        top = ident;
                        Vec::new()
                    } else {
                        top.extend(ident.iter().cloned());
                        ident
                    };
                    for ch in &mut c.node.children {
                        junk_layout(ch, &top, &last, &self.rowmark_junk);
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
                    // aren't either.)
                    let own = (t.partkey.is_none()).then_some(t);
                    for m in own
                        .into_iter()
                        .chain(mp.members.iter().map(|m| &m.rules.table))
                        .filter(|m| target_scan(&mp.source, m.oid).is_some())
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
        let mut app = self.node(source);
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
        if self.o.verbose && !returning.is_empty() {
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
            n.output = returning
                .iter()
                .map(|e| self.show(&old_new_rows(e, old_new), &names, cx))
                .collect();
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
        let can_hash = types.iter().all(setop_hashable);
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
        // Sorted: each arm sorted, merged, made unique (when the sort covers every column).
        let order: Vec<usize> = keys
            .iter()
            .filter_map(|k| match k.expr {
                Expr::Col(i, _) => Some(i),
                _ => None,
            })
            .collect();
        // (EXPLAIN ANALYZE shows the plan that ran.)
        if (0..ncols).all(|i| order.contains(&i)) && built.len() > 1 && !self.o.analyze {
            let mut kids = Vec::new();
            let (mut startup, mut run, mut rows) = (0.0, 0.0, 0.0);
            let mut arm_list = Vec::new();
            for (i, c) in built.iter().enumerate() {
                let alist: Vec<String> = keys
                    .iter()
                    .map(|k| self.show(&k.expr, &c.exported, Cx::Upper) + &sort_suffix(k))
                    .collect();
                if i == 0 {
                    arm_list = alist.clone();
                }
                let mut sn = ExNode::new("Sort");
                sn.details.push(Detail::List("Sort Key", alist));
                sn.details.push(Detail::SortInfo);
                self.sort_costs(&mut sn, &c.node);
                sn.key = c.node.key;
                if self.o.verbose {
                    sn.output = c.node.output.clone();
                }
                startup += sn.startup_cost;
                run += sn.total_cost - sn.startup_cost;
                rows += sn.rows;
                sn.children.push(c.node.clone().child("Outer"));
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
                ma.details.push(Detail::List("Sort Key", arm_list));
                ma.rows = rows;
                ma.width = width;
                ma.startup_cost = ma_startup;
                ma.total_cost = ma_total;
                for k in kids {
                    ma.children.push(k.child("Member"));
                }
                let mut u = ExNode::new("Unique");
                u.rows = rows;
                u.width = width;
                u.startup_cost = ma_startup;
                u.total_cost = u_total;
                u.order = list;
                u.unique_keys = Some((0..ncols).collect());
                u.children.push(ma.child("Outer"));
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
        for c in cols {
            match &c.var {
                Some(v) => super::extstat::add_unique(&mut vars, super::extstat::GroupVar::of(&cat, v)),
                None => total *= c.distinct.unwrap_or(costs::DEFAULT_NUM_DISTINCT),
            }
        }
        super::extstat::num_groups(self.ctx, vars, total, in_rows, Some(in_rows))
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
    /// the workers over partial plan `pt`, then over what a Gather (Merge) of them sends,
    /// when such a plan survives add_path and is the cheapest.
    fn parallel_distinct(
        &self,
        n: &ExNode,
        u: &ExNode,
        pt: &ExNode,
        keys: &[ColInfo],
        key_ops: f64,
        per_group: f64,
        every_row: bool,
    ) -> Option<ExNode> {
        let g = crate::sql::paths::Gucs::read(self.ctx);
        let ord = u.order.clone();
        let pd_rows = if every_row { pt.rows } else { self.num_groups(keys, pt.rows) };
        let unique_over = |child: ExNode, rows: f64| -> ExNode {
            let mut x = ExNode::new("Unique");
            x.unique_keys = u.unique_keys.clone();
            x.rows = rows;
            x.width = n.width;
            x.cols = n.cols.clone();
            x.startup_cost = child.startup_cost;
            x.total_cost = child.total_cost + child.rows * key_ops;
            x.order = ord.clone();
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
        let sort_of = |child: ExNode| -> ExNode {
            let mut sn = ExNode::new("Sort");
            sn.details.push(Detail::List("Sort Key", ord.clone()));
            sn.details.push(Detail::SortInfo);
            self.sort_costs(&mut sn, &child);
            sn.disabled = !g.sort;
            sn.order = ord.clone();
            sn.workers = child.workers;
            sn.key = child.key;
            sn.output = child.output.clone();
            sn.children.push(child.child("Outer"));
            sn
        };
        // The partial DISTINCTs: hashed, and sorted (with its sort).
        let p_hash = hash_over(pt.clone(), pd_rows);
        let p_sorted = unique_over(sort_of(pt.clone()), pd_rows);
        let cheapest_partial = if parallel::cheaper(&p_sorted, &p_hash) { &p_sorted } else { &p_hash };
        // generate_useful_gather_paths: a Gather of the cheapest, Gather Merges of the
        // sorted ones (and of the cheapest sorted).
        let mut gathered = vec![self.gather_of(cheapest_partial), self.gather_merge_of(&p_sorted)];
        if std::ptr::eq(cheapest_partial, &p_hash) {
            gathered.push(self.gather_merge_of(&sort_of(p_hash.clone())));
        }
        let gathered = pwagg::add_path_survivors(gathered, &ord);
        let cheapest_total = gathered
            .iter()
            .fold(None::<&ExNode>, |b, x| match b {
                Some(b) if !parallel::cheaper(x, b) => Some(b),
                _ => Some(x),
            })
            .cloned()?;
        // create_final_distinct_paths over them.
        let sorted_by = |x: &ExNode| x.order.len() >= ord.len() && x.order[..ord.len()] == ord[..];
        let mut finals: Vec<ExNode> = Vec::new();
        for x in &gathered {
            if sorted_by(x) {
                finals.push(unique_over(x.clone(), n.rows));
            }
        }
        if !sorted_by(&cheapest_total) {
            finals.push(unique_over(sort_of(cheapest_total.clone()), n.rows));
        }
        finals.push(hash_over(cheapest_total, n.rows));
        let mut all = vec![n.clone(), u.clone()];
        all.extend(finals);
        let kept = pwagg::add_path_survivors(all, &ord);
        let best = kept.into_iter().fold(None::<ExNode>, |b, x| match b {
            Some(b) if !parallel::cheaper(&x, &b) => Some(b),
            _ => Some(x),
        })?;
        // (Only a plan of the workers' DISTINCT: the serial ones go as before.)
        let serial = |s: &ExNode| s.title == best.title && s.total_cost == best.total_cost && s.startup_cost == best.startup_cost;
        (!serial(n) && !serial(u)).then_some(best)
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
        let mut n = ExNode::new("Append");
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
            let mut c = self.node(a);
            self.member_scan = saved;
            members.push(c.node.clone());
            c.node.partial = None;
            c.node.out_names = c.exported.iter().map(|k| self.pick(k, Cx::Upper)).collect();
            if i == 0 {
                n.startup_cost = c.node.startup_cost;
                n.cols = c.node.cols.iter().map(|x| ColInfo { var: None, ..x.clone() }).collect();
            }
            n.rows += c.node.rows;
            n.width = n.width.max(c.node.width);
            member_tuples.push(c.node.cols.iter().find_map(|x| x.var.as_ref().map(|v| v.tuples)).unwrap_or(c.node.rows));
            names.get_or_insert(c.exported);
            // (A UNION ALL arm's own Append is flattened into this one:
            // accumulate_append_subpath.)
            if !inh && c.node.node_type == "Append" && c.node.partial.is_none() {
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
            let gating: Vec<Expr> = parts
                .iter()
                .filter(|e| e.columns().is_empty() && !super::paths::expr_volatile_deep(e))
                .cloned()
                .collect();
            let mut n = self.planned_scan(p, s, d, &names);
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
                    }
                    None => {}
                }
            }
            if s.mark.is_some() {
                n = lock_rows(n);
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
            n = lock_rows(n);
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
            Access::Bitmap(_) => ExNode::new("Bitmap Heap Scan"),
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
            let texts = d
                .index_conds
                .iter()
                .map(|e| self.index_cond(e, names, &plain))
                .collect();
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
            let texts = d
                .order_by
                .iter()
                .map(|e| self.index_cond(e, names, &[]))
                .collect();
            n.details
                .push(Detail::Text("Order By", and_text(texts).unwrap_or_default()));
        }
        if !d.recheck.is_empty() {
            let texts = d
                .recheck
                .iter()
                .map(|e| self.show(e, names, Cx::Scan))
                .collect();
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
            Access::Bitmap(_) => {
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
                    let texts = quals.iter().map(|e| self.index_cond(e, names, &[])).collect();
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

    fn fold_filter(&mut self, mut c: Built, pred: &Expr, key: usize, setop: bool) -> Built {
        // (A qual workers can't run keeps the node out of parallel plans.)
        if super::paths::parallel::hazard(pred, &self.ctx.catalog()) != 's' {
            c.node.partial = None;
            c.node.par_safe = false;
        }
        self.settle(&mut c.node);
        let input_key = c.node.key.unwrap_or(0);
        let mut parts = Vec::new();
        conjuncts(pred, &mut parts);
        // (An aggregation's HAVING clauses are all its quals: none gates it.)
        let having = c.node.node_type == "Aggregate" && !setop;
        let (gating, quals): (Vec<Expr>, Vec<Expr>) = parts
            .into_iter()
            .partition(|e| !having && e.columns().is_empty() && !super::paths::expr_volatile_deep(e));
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
                };
                let qc: f64 = quals.iter().map(costs::qual_cost).sum();
                let sel = est.clauses(&quals);
                c.node.total_cost += c.node.rows * qc;
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
                // it; cost_subqueryscan's per-tuple qual cost.)
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
                    };
                    (est.clauses(&quals), quals.iter().map(costs::qual_cost).sum::<f64>())
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
        // (An ORDER BY above: the GROUP BY's keys in its order first, preprocess_groupclause.)
        let hint = self.group_order.take();
        let hint_full = self.group_order_full;
        let saved_input = self.agg_input.replace(addr(input));
        let mut c = self.node(input);
        self.agg_input = saved_input;
        let mut part_in = c.node.partial.take();
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
            if let Some(pi) = &mut part_in {
                pi.total_cost += pi.rows * gexpr;
            }
        }
        // The input run serially, or gathered from workers.
        if let Some(pi) = &part_in {
            c.node.partial = Some(pi.clone());
            self.settle(&mut c.node);
        }
        let gnames: Vec<Name> = groups
            .iter()
            .map(|g| self.name(g, &c.exported, &[]))
            .collect();
        let ng = groups.len();
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
            .map(|g| {
                (!plain_col(g) && !g.columns().is_empty() && sets.is_none())
                    || (sets.is_some() && matches!(g, Expr::Const(..)))
            })
            .collect();
        let gref: Vec<Name> = gnames
            .iter()
            .zip(&computed)
            .map(|(g, &k)| if k { g.wrapped() } else { g.clone() })
            .collect();
        // (A Sort over the input returns references to what the input computes.)
        let computed_shown: Vec<(String, String)> = gnames
            .iter()
            .zip(&gref)
            .zip(&computed)
            .filter(|(_, k)| **k)
            .map(|((g, r), _)| (self.pick(g, Cx::Output), self.pick(r, Cx::Output)))
            .collect();
        let sort_out = |o: &String| -> String {
            match computed_shown.iter().find(|(g, _)| g == o) {
                Some((_, r)) => r.clone(),
                None => refer_to(o),
            }
        };
        // (A scan computing them returns that target list, no physical one: the group
        // keys, then the columns the aggregates read.)
        if self.o.verbose && computed.iter().any(|k| *k) && c.node.is_scan() {
            let mut out: Vec<String> = Vec::new();
            for g in &gnames {
                let t = self.pick(g, Cx::Output);
                if !out.contains(&t) {
                    out.push(t);
                }
            }
            let mut cols: Vec<usize> = Vec::new();
            for a in aggs {
                for e in a.args.iter().chain(&a.direct).chain(&a.filter).chain(a.order_by.iter().map(|k| &k.expr)) {
                    cols.extend(e.columns());
                }
            }
            for j in cols {
                if let Some(n) = c.exported.get(j) {
                    let t = self.pick(n, Cx::Output);
                    if !out.contains(&t) {
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
            let group_keys = keys_ref(self, &(0..groups.len()).collect::<Vec<_>>());
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
            Some(if best.is_empty() { group_keys } else { best })
        };
        if let Some(sort_keys) = presort {
            if !sort_keys.is_empty() {
                let mut sn = ExNode::new("Sort");
                sn.details.push(Detail::List("Sort Key", sort_keys));
                sn.details.push(Detail::SortInfo);
                self.sort_costs(&mut sn, &c.node);
                sn.key = c.node.key;
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
                let all: Vec<usize> = (0..groups.len()).collect();
                n.details.push(Detail::List("Group Key", keys_ref(self, &all)));
                // (Its groups come out in the keys' order.)
                n.order = keys_ref(self, &all);
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
                (0..groups.len()).filter(|&i| !hashable_key(&ty(i))).collect(),
            )
        };
        let must_sort = sets.is_some_and(|ss| ss.iter().any(|s| s.iter().any(|k| unhashable.contains(k))));
        let must_hash: Vec<usize> = match sets {
            Some(ss) => (0..ss.len()).filter(|&i| ss[i].iter().any(|k| unsortable.contains(k))).collect(),
            None => Vec::new(),
        };
        let sorted_cheaper = match sets {
            Some(_) if must_sort || !must_hash.is_empty() => true,
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
            None if {
                let fixed = super::paths::constant_columns(input);
                groups.iter().all(|g| implicit_key_col(g).is_some_and(|j| fixed.get(j).copied().unwrap_or(false)))
            } =>
            {
                n.strategy = Some("Sorted");
                n.title = if aggs.is_empty() { "Group" } else { "GroupAggregate" }.into();
                if aggs.is_empty() {
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
        } else if {
            let fixed = super::paths::constant_columns(input);
            groups.iter().all(|g| implicit_key_col(g).is_some_and(|j| fixed.get(j).copied().unwrap_or(false)))
        } {
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
            let coll_sfx: Vec<String> = groups
                .iter()
                .map(|g| match group_key_collation(input, g) {
                    Some(n) => format!(" COLLATE {}", crate::catalog::quote_ident(&n)),
                    None => String::new(),
                })
                .collect();
            let mut orders: Vec<Vec<(usize, String)>> =
                vec![(0..groups.len()).map(|i| (i, coll_sfx[i].clone())).collect()];
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
            if !pre.is_empty() {
                let mut ord = pre.clone();
                ord.extend((0..groups.len()).filter(|i| !pre.iter().any(|p| p.0 == *i)).map(|i| (i, coll_sfx[i].clone())));
                if !orders.contains(&ord) {
                    orders.push(ord);
                }
            }
            if let Some(hint) = hint.clone() {
                let mut ord: Vec<(usize, String)> = Vec::new();
                for (i, sfx) in hint {
                    if i < groups.len() && !ord.iter().any(|o| o.0 == i) {
                        ord.push((i, sfx));
                    }
                }
                let rest: Vec<(usize, String)> = (0..groups.len())
                    .filter(|i| !ord.iter().any(|o| o.0 == *i))
                    .map(|i| (i, coll_sfx[i].clone()))
                    .collect();
                ord.extend(rest);
                // (First: at equal cost, the order the ORDER BY wants.)
                if ord.len() == groups.len() {
                    orders.retain(|o| *o != ord);
                    orders.insert(0, ord);
                }
            }
            let mut best: Option<ExNode> = None;
            for ord in orders {
                let ord: Vec<(usize, String)> =
                    ord.into_iter().filter(|(i, _)| !redundant.get(*i).copied().unwrap_or(false)).collect();
                let group_names: Vec<String> = ord.iter().map(|(i, _)| names_all[*i].clone()).collect();
                let keys_ord: Vec<String> = ord.iter().map(|(i, sfx)| names_all[*i].clone() + sfx).collect();
                let ord: Vec<usize> = ord.iter().map(|o| o.0).collect();
                let common = keys_ord
                    .iter()
                    .zip(&c.node.order)
                    .enumerate()
                    .take_while(|(oi, (a, b))| {
                        a == b || c.node.order_eq.get(*oi).is_some_and(|e| e.contains(a))
                    })
                    .count();
                let mut input = c.node.clone();
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
                ga.details.retain(|d| !matches!(d, Detail::HashAggInfo));
                for d in &mut ga.details {
                    if let Detail::List("Group Key", k) = d {
                        *k = group_names.clone();
                    }
                }
                ga.startup_cost = input.startup_cost;
                if aggs.is_empty() {
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
                ga.order = keys_ord;
                ga.children.push(input.child("Outer"));
                if best.as_ref().is_none_or(|b| parallel::cheaper(&ga, b)) {
                    best = Some(ga);
                }
            }
            n.disabled = !g.hashagg;
            best
        } else {
            None
        };
        n.children.push(c.node.child("Outer"));
        if let Some(ga) = sorted_alt {
            if parallel::fuzzy_better(&ga, &n) {
                let hashed = std::mem::replace(&mut n, ga);
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
            && let Some(mut f) = self.parallel_agg(&n, *pi, groups, aggs, &gnames, &anames)
        {
            let mut serial = std::mem::take(&mut n);
            serial.alt = None;
            if !parallel::has_gather(&serial) {
                f.safe_alt = Some(Box::new(serial));
            }
            n = f;
        }
        // Or partition by partition.
        if sets.is_none() {
            let names: Vec<String> = gnames.iter().map(|g| self.pick(g, Cx::Upper)).collect();
            self.group_order = hint.clone();
            let pw = self.partitionwise_agg(input, groups, aggs, &n, having.as_ref(), &names, hint_full);
            self.group_order = None;
            if let Some(pw) = pw
            {
                n = pw;
            }
        }
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
        let mut l = self.node(left);
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
        if lateral {
            let names: Vec<String> = params
                .iter()
                .map(|e| self.name(e, &l.exported, &[]).full)
                .collect();
            self.outer.insert(0, names);
            self.outer_depth.insert(0, self.depth);
        }
        let r = self.node(right);
        if lateral {
            self.outer.remove(0);
            self.outer_depth.remove(0);
        }
        let copy = |b: &Built| Built {
            node: b.node.clone(),
            internal: b.internal.clone(),
            exported: b.exported.clone(),
        };
        let parts = l.node.partial.is_some() || r.node.partial.is_some();
        let (lg, rg) = (copy(&l), copy(&r));
        // (A Hash Right Semi Join has no partial form: the semi join the other way round
        // may have one.)
        let unswapped = (meta.swapped && kind == JoinKind::Semi && parts && !lateral)
            .then(|| (copy(&l), copy(&r)));
        let mut b1 = self.join_built(at, kind, left, right, l, r, cond, hash, params, meta);
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
        // Or over its inputs gathered from workers (their Gather paths, when cheaper).
        if parts && !lateral {
            let (mut lg, mut rg) = (lg, rg);
            self.settle(&mut lg.node);
            self.settle(&mut rg.node);
            // (A partial nested loop takes the inner side's cheapest path, which a Gather
            // can't run under.)
            if b1.node.node_type == "Nested Loop" {
                let inner_key = b1.node.children.get(1).and_then(|c| {
                    if matches!(c.node_type, "Materialize" | "Memoize") {
                        c.children.first().and_then(|x| x.key)
                    } else {
                        c.key
                    }
                });
                let inner_gathered = if inner_key.is_some() && inner_key == lg.node.key {
                    parallel::has_gather(&lg.node)
                } else {
                    parallel::has_gather(&rg.node)
                };
                if inner_gathered {
                    b1.node.partial = None;
                }
            }
            if parallel::has_gather(&lg.node) || parallel::has_gather(&rg.node) {
                let mut b2 = self.join_built(at, kind, left, right, lg, rg, cond, hash, params, meta);
                if b2.node.subplans.is_empty() {
                    b2.node.subplans = b1.node.subplans.clone();
                }
                if parallel::par_beats(&b2.node, &b1.node) {
                    b2.node.partial = b1.node.partial;
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
            joins::JoinVar {
                var: c.as_ref().and_then(|c| c.var.clone()),
                distinct: c.as_ref().and_then(|c| c.distinct),
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
        };
        let other_sel = est.clauses(&others);
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
        n.rows = meta
            .rows
            .unwrap_or_else(|| joins::join_rows(jt, lrows, rrows, jsel, 1.0));
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
                as u32;
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
        let mut inner_unique;
        let mut material = false;
        let mut cost;
        match method {
            JoinMethod::Hash => {
                let hash_cost = |outer: &ExNode, inner: &ExNode, sw: bool, uniq: bool| {
                    let (nb, nbatch) = joins::hash_table_size(inner.rows, inner.width, &g);
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
                            startup: 0.0,
                            per_tuple: key_cost,
                        },
                        other_quals: QualCost {
                            startup: 0.0,
                            per_tuple: other_cost,
                        },
                        bucket: (!meta.unique_inner).then_some(bucket),
                        hash_tuples: costs::clamp_rows(inner_sel * outer.rows * inner.rows),
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
                        startup: 0.0,
                        per_tuple: key_cost,
                    },
                    other_quals: QualCost {
                        startup: 0.0,
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
                    startup: 0.0,
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
                let (po, inn) = if swapped { (&rpart, &l.node) } else { (&lpart, &r.node) };
                if let Some(po) = po
                    && !lateral
                {
                    pcost = Some(nl_cost(po, inn, swapped, inner_unique, material).0);
                }
            }
        }
        if swapped {
            std::mem::swap(&mut l, &mut r);
        }
        n.startup_cost = cost.startup;
        n.total_cost = cost.total;
        let jname = if right_variant {
            format!("Right {}", join_type(kind))
        } else {
            join_type(kind).to_string()
        };
        n.title = match (method, kind) {
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
            let a = side(self, oa, outer_names, outer_cols);
            let before = self.pending.len();
            let b = side(self, ib, inner_names, inner_cols);
            // (The inner keys are the Hash's to compute: their SubPlans are its.)
            if method == JoinMethod::Hash && self.pending.len() > before {
                hash_subplans.extend(self.pending.drain(before..));
            }
            merge_names.push((a.clone(), b.clone()));
            keys_text.push(format!("({a} = {b})"));
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
            if kind == JoinKind::Inner
                && p.columns().is_empty()
                && !p.is_volatile()
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
        if let Some(t) = and_text(join_filter) {
            n.details.push(Detail::Text("Join Filter", t));
        }
        if material {
            let m = joins::material(&input(&r.node), &g);
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
        n.children.push(l.node.child("Outer"));
        if method == JoinMethod::Hash {
            let mut h = ExNode::new("Hash");
            h.subplans = std::mem::take(&mut hash_subplans);
            h.key = r.node.key;
            h.rows = r.node.rows;
            h.width = r.node.width;
            h.startup_cost = r.node.total_cost;
            h.total_cost = r.node.total_cost;
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
        n.par_safe = n.children.iter().all(parallel::safe_tree);
        if let (Some(c), Some(po)) = (pcost, outer_part)
            && gating.is_empty()
            && (parallel_hash
                || matches!(kind, JoinKind::Inner | JoinKind::Left | JoinKind::Semi | JoinKind::Anti))
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
        Plan::Append { inputs, inh: Some(_) } => {
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
        Plan::Append { inputs, inh: None } if all => {
            inputs.iter().for_each(|i| union_arms(i, all, out))
        }
        _ => out.push(p),
    }
}

/// A system catalog's rows now (its reltuples), cached for the catalog version.
pub(crate) fn sys_tuples(table: crate::catalog::system::SysTable, ctx: &ExecCtx) -> f64 {
    thread_local! {
        static SYS_ROWS: std::cell::RefCell<HashMap<(usize, u32), f64>> = std::cell::RefCell::new(HashMap::new());
    }
    let key = (Arc::as_ptr(&ctx.catalog()) as usize, table.oid());
    if let Some(n) = SYS_ROWS.with(|m| m.borrow().get(&key).copied()) {
        return n;
    }
    let n = crate::catalog::system::rows(table, ctx).map_or(1000.0, |r| r.len() as f64).max(1.0);
    SYS_ROWS.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() > 256 {
            m.clear();
        }
        m.insert(key, n);
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
    for m in order {
        let k = match m {
            PartOrder::Leaf(i) => slots[*i].take().unwrap_or_default(),
            PartOrder::Append(sub) => ordered_append(sub, slots, list),
            PartOrder::Merge(v) => {
                let kids: Vec<ExNode> = v.iter().map(|i| slots[*i].take().unwrap_or_default()).collect();
                let n = kids.len() as f64;
                let cmp = 2.0 * costs::CPU_OPERATOR_COST;
                let logn = n.log2();
                let mut ma = ExNode::new("Merge Append");
                // (Its keys over its first member's row.)
                let alias = kids.first().and_then(member_alias);
                let parent = kids.first().and_then(member_parent_alias);
                let keys: Vec<String> = match (alias, parent) {
                    (Some(al), Some(pa)) => list.iter().map(|k| k.replace(&format!("{pa}."), &format!("{al}."))).collect(),
                    _ => list.to_vec(),
                };
                ma.details.push(Detail::List("Sort Key", keys.clone()));
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
    // (One member: no Append.)
    if a.children.len() == 1 {
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
            if let Plan::Append { inh: Some(_), inputs } = x
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
    if let Plan::Append { inh: Some(_), inputs } = p
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
            f(self, c);
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
            match self.stats(n.key) {
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
                                    "{}  Misses: {}  Evictions: 0  Overflows: 0  Memory Usage: {kb}kB",
                                    st.cache_hits, st.cache_misses
                                ),
                                true,
                            );
                        } else {
                            self.w.prop("Cache Hits", &st.cache_hits.to_string(), true);
                            self.w.prop("Cache Misses", &st.cache_misses.to_string(), true);
                            self.w.prop("Cache Evictions", "0", true);
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
                    let mut g = self.jg;
                    let mut est = n.rows;
                    if n.parallel_aware {
                        let w = n.children.first().map_or(0, |c| c.workers) as f64;
                        let procs = w + if self.leader { 1.0 } else { 0.0 };
                        est *= procs.max(1.0);
                        g.work_mem *= w + 1.0;
                    }
                    let (mut nb0, mut nbatch0) = super::joins::hash_table_size(est, n.width, &g);
                    // (Not fitting the combined memory, it plans with its own.)
                    if n.parallel_aware && nbatch0 > 1 {
                        g = self.jg;
                        (nb0, nbatch0) = super::joins::hash_table_size(est, n.width, &g);
                    }
                    let tupsize = (32 + (n.width as u64).div_ceil(8) * 8) as f64;
                    let space = (g.work_mem * g.hash_mem_multiplier * 1024.0).floor();
                    let max_group = (st.hash_max_group / loops).max(1) as f64;
                    let mut nbatch = nbatch0 as u64;
                    let mut in_mem = (rows as f64 / nbatch as f64).max(max_group.min(rows as f64));
                    let mut grew_stuck = false;
                    while in_mem * tupsize + nb0 as f64 * 8.0 > space {
                        let next = nbatch * 2;
                        let now = (rows as f64 / next as f64).max(max_group.min(rows as f64));
                        nbatch = next;
                        if now >= in_mem {
                            grew_stuck = true;
                            break;
                        }
                        in_mem = now;
                    }
                    // (A Parallel Hash splits its first batch once more before it gives up.)
                    if grew_stuck && n.parallel_aware {
                        nbatch *= 2;
                    }
                    let nb = if nbatch == 1 {
                        (nb0 as u64).max(rows.next_power_of_two())
                    } else {
                        nb0 as u64
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
            let s = self.stats(n.key).unwrap_or_default();
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

/// A plan's estimates as EXPLAIN makes them (the planner sizes FROM items it can't plan
/// itself this way).
pub struct PlanEstimate {
    pub rows: f64,
    pub startup: f64,
    pub total: f64,
    pub width: u32,
    /// The top node's type.
    pub node_type: &'static str,
    /// An Incremental Sort on top: its presorted keys.
    pub presorted: usize,
    /// Per output column: its width, the base column it is, a known distinct count.
    pub cols: Vec<(u32, Option<costs::VarRef>, Option<f64>)>,
}

/// cost_subplan for the sub-selects in `exprs`: (startup, per evaluation). A hashed one
/// builds its table once; a correlated one runs each time (an EXISTS to its first row).
fn subplan_costs(exprs: &[Expr], ctx: &ExecCtx) -> (f64, f64) {
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
                let est = estimate(&s.plan, ctx);
                startup += costs::CPU_OPERATOR_COST.mul_add(est.rows, est.total);
                per += ops;
            } else if correlated {
                let est = estimate(&s.plan, ctx);
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
                        let Expr::Const(crate::types::Value::Text(patt), _) = &args[1] else {
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
    if let Plan::Append { inputs, inh: inh @ Some(_) } = p
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
        return Some((Plan::Append { inputs: members, inh: inh.clone() }, keys0?));
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

/// A set operation's column type whose values hash: an array of such, a named composite
/// of such (an anonymous record's fields the type cache doesn't look into).
fn setop_hashable(t: &DataType) -> bool {
    match t {
        DataType::Array(e) => setop_hashable(e),
        DataType::Row(rt) => rt.oid != 0 && rt.fields.iter().all(|(_, f)| setop_hashable(f)),
        t => hashable_key(t),
    }
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
        sub_alias: None,
        json_casts: Vec::new(),
        proj_depth: 0,
        level_tops: std::collections::HashSet::new(),
        json_aggs: HashMap::new(),
        having: None,
        pw_aggs: Vec::new(),
        pw_partial: false,
        agg_key: None,
        agg_input: None,
        unique_sides: Default::default(),
        plain_distinct: None,
        shown: shown_subqueries(p, &needed_masks(p)),
            rowmarks: Default::default(),
            rowmark_junk: Default::default(),
        alts: Default::default(),
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
        sub_alias: None,
        json_casts: Vec::new(),
        proj_depth: 0,
        level_tops: std::collections::HashSet::new(),
        json_aggs: HashMap::new(),
        having: None,
        pw_aggs: Vec::new(),
        pw_partial: false,
        agg_key: None,
        agg_input: None,
        unique_sides: Default::default(),
        plain_distinct: None,
        shown: shown_subqueries(input, &needed_masks(input)),
        rowmarks: Default::default(),
        rowmark_junk: Default::default(),
        alts: Default::default(),
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
        sub_alias: None,
        json_casts: Vec::new(),
        proj_depth: 0,
        level_tops: std::collections::HashSet::new(),
        json_aggs: HashMap::new(),
        having: None,
        pw_aggs: Vec::new(),
        pw_partial: false,
        agg_key: None,
        agg_input: None,
        unique_sides: Default::default(),
        plain_distinct: None,
        shown: shown_subqueries(input, &needed_masks(input)),
        rowmarks: Default::default(),
        rowmark_junk: Default::default(),
        alts: Default::default(),
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
        sub_alias: None,
        json_casts: Vec::new(),
        proj_depth: 0,
        level_tops: std::collections::HashSet::new(),
        json_aggs: HashMap::new(),
        having: None,
        pw_aggs: Vec::new(),
        pw_partial: false,
        agg_key: None,
        agg_input: None,
        unique_sides: Default::default(),
        plain_distinct: None,
        shown: shown_subqueries(plan, &needed_masks(plan)),
        rowmarks: Default::default(),
        rowmark_junk: Default::default(),
        alts: Default::default(),
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
        sub_alias: None,
        json_casts: Vec::new(),
        proj_depth: 0,
        level_tops: std::collections::HashSet::new(),
        json_aggs: HashMap::new(),
        having: None,
        pw_aggs: Vec::new(),
        pw_partial: false,
        agg_key: None,
        agg_input: None,
        unique_sides: Default::default(),
        plain_distinct: None,
        shown: shown_subqueries(plan, &needed_masks(plan)),
        rowmarks: Default::default(),
        rowmark_junk: Default::default(),
        alts: Default::default(),
    };
    let n = b.level(plan);
    n.unique_keys
}

pub fn estimate(plan: &Arc<Plan>, ctx: &ExecCtx) -> PlanEstimate {
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
        sub_alias: None,
        json_casts: Vec::new(),
        proj_depth: 0,
        level_tops: std::collections::HashSet::new(),
        json_aggs: HashMap::new(),
        having: None,
        pw_aggs: Vec::new(),
        pw_partial: false,
        agg_key: None,
        agg_input: None,
        unique_sides: Default::default(),
        plain_distinct: None,
        shown: shown_subqueries(plan, &needed_masks(plan)),
        rowmarks: Default::default(),
        rowmark_junk: Default::default(),
        alts: Default::default(),
    };
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
        node_type: n.node_type,
        presorted,
        cols,
    }
}

/// The EXPLAIN output of `plan`: text lines, or one JSON / YAML / XML document. Planning
/// and execution times are in seconds.
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
/// there. The plan with each such later copy given an id of its own (EXPLAIN without
/// ANALYZE: the executor shares one), when there is one.
fn split_shared_subqueries(p: &Arc<Plan>) -> Option<Arc<Plan>> {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1 << 39);
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
    fn deep_copy(p: &mut Arc<Plan>) {
        let n = Arc::make_mut(p);
        for c in plan_inputs_mut(n) {
            deep_copy(c);
        }
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
                                    s.id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    // (Its relations are range table entries of their own.)
                                    deep_copy(&mut s.plan);
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
    let split = if instr.is_none() { split_shared_subqueries(plan) } else { None };
    let plan = split.as_ref().unwrap_or(plan);
    // (Built again when it groups partitionwise: the parents' names go unused then.)
    // (An inheritance tree that is a UNION ALL's arm is flattened into its Append: the
    // parent's name goes unused too.)
    let mut skip: Vec<usize> = Vec::new();
    union_inh_appends(plan, &mut skip);
    let mut pw_named = false;
    let root = loop {
    let (refnames, results) = assign_refnames_skip(plan, Some(ctx), &skip);
    // ON CONFLICT DO UPDATE's EXCLUDED is a range table entry of its own.
    let excluded = match &**plan {
        Plan::Insert(i) => i
            .on_conflict
            .as_ref()
            .is_some_and(|oc| matches!(oc.action, ConflictAction::Update { .. })) as usize,
        _ => 0,
    };
    let mut b = Builder {
        ctx,
        o,
        // (An inheritance tree's parent is a range table entry besides its members.)
        prefix: refnames.len() + hidden_rtes + results + excluded + inh_parents(plan) > 1,
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
        needed: needed_masks(plan),
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
        sub_alias: None,
        json_casts: Vec::new(),
        proj_depth: 0,
        level_tops: std::collections::HashSet::new(),
        json_aggs: HashMap::new(),
        having: None,
        pw_aggs: Vec::new(),
        pw_partial: false,
        agg_key: None,
        agg_input: None,
        unique_sides: Default::default(),
        plain_distinct: None,
        shown: shown_subqueries(plan, &needed_masks(plan)),
        rowmarks: rowmark_scans(plan),
        rowmark_junk: Default::default(),
        alts: Default::default(),
    };
    b.top = addr(plan);
    let root = b.level(plan);
    if !pw_named && !b.pw_aggs.is_empty() {
        pw_named = true;
        skip.extend(b.pw_aggs.iter().copied());
        continue;
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
            .is_some_and(|v| v.eq_ignore_ascii_case("on"))
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
    let column = !o.is_empty()
        && o.split('.').all(|p| {
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
    for side in [on.old_alias, on.new_alias] {
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
