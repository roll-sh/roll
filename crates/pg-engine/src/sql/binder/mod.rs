//! The binder: turns pg_query's parse tree into plans, resolving names, types, functions and
//! operators the way Postgres's parse analysis does, and choosing scans and join methods.
//!
//! Query levels: each subquery (and each derived table or LATERAL item in FROM) is a level.
//! A level reads values from enclosing levels through `params`: expressions evaluated in the
//! parent level and pushed as one outer row when the level runs, read inside as `Outer(1, i)`.

pub mod ast;
mod dml;
mod expr;
pub(crate) use expr::{INDETERMINATE_HASH, NONDETERMINISTIC_HASH, check_merge_keys, eq_under, hash_key, hash_key_operand, indeterminate_eq, is_indeterminate};
pub(crate) use expr::{check_sql_retval, sql_fn_frame};
pub use expr::{check_folds, is_whole_row, nullable_whole_row};
pub mod inherit;
pub(crate) mod optimize;
pub(crate) mod partition;
pub(crate) mod partprune;
pub mod perms;
pub(crate) mod refute;
mod recursion;
pub(crate) mod refs;
mod rls;
pub(crate) mod rules;
mod select;
mod sqljson;
mod sublink;
mod xml;
mod xmltable;
pub(crate) use select::{figure_colname, figure_index_colname};
mod types;
pub mod views;

pub use dml::{bound_indexes, partition_routing, table_rules};
pub use optimize::width as plan_width;
pub use types::{
    parse_type_string, resolve_type_name, resolve_type_name_shell, serial_type, type_from_names,
};

use crate::catalog::{Catalog, Table};
use crate::error::{Error, Result, code};
use crate::exec::ExecCtx;
use crate::sql::expr::Expr;
use crate::sql::plan::{AggCall, OutCol, Plan, SrfCall, WindowCall};
use crate::types::DataType;
use crate::types::Value;
use crate::types::cast::{CastContext, cast_context, cast_value, common_type};
use crate::types::text::parse_value_ex;
use pg_query::protobuf as pb;
use std::collections::HashMap;
use std::sync::Arc;

/// A bound statement that runs as a plan.
#[derive(Clone, Debug)]
pub struct BoundQuery {
    pub plan: Arc<Plan>,
    /// Result columns (empty for DML without RETURNING).
    pub cols: Vec<OutCol>,
    /// Types of `$n` parameters, as declared or inferred.
    pub params: Vec<DataType>,
    /// Privileges it needs (checked each time it runs).
    pub perms: Arc<perms::Perms>,
    /// FROM items with no scan of their own (subqueries, views): EXPLAIN counts them as range
    /// table entries.
    pub hidden_rtes: usize,
}

impl BoundQuery {
    /// Check the privileges the statement needs, as the current user.
    pub fn check_perms(&self, ctx: &ExecCtx) -> Result<()> {
        perms::check(&self.perms, ctx)
    }
}

/// A column visible to name resolution.
#[derive(Clone, Debug)]
pub struct ScopeCol {
    pub name: String,
    /// The column's value, as an expression over the level's current row.
    pub expr: Expr,
    /// Dropped columns: never visible.
    pub hidden: bool,
    /// Merged by JOIN USING / NATURAL: only reachable qualified, and not part of `*`.
    pub merged: bool,
    pub table: u32,
    pub attnum: i16,
    /// The collation a subquery / view / CTE column derives (`Coll::None`: none, for a
    /// collatable type). None: `table`'s column's, else the type's.
    pub collation: Option<Coll>,
    /// Where the column comes from under its own name (for rewriting stored definitions,
    /// `refs`): the location of the FROM item naming its relation, -1 the relation itself (a
    /// statement's target, a stored expression's table), None anything else.
    pub src: Option<i32>,
    /// A column of a function's named composite result: the column of the composite type's
    /// relation it is (relation, attnum), which a stored definition using it depends on
    /// (`process_function_rte_ref`).
    pub rowtype_col: Option<(u32, i16)>,
    /// The domain (or array-of-domain) type a subquery / view / CTE / function column is
    /// declared as (a base table's column has its own: `table`, `attnum`).
    pub domain: Option<u32>,
}

/// A relation (FROM item) visible to name resolution.
#[derive(Clone, Debug, Default)]
pub struct ScopeRel {
    /// Name used to qualify columns (the alias, or the table name).
    pub alias: String,
    /// Schema, when referenced by its real name (allows `schema.table.col`).
    pub schema: Option<String>,
    pub cols: Vec<ScopeCol>,
    /// The base table, for FOR UPDATE and row locking.
    pub table: Option<Arc<Table>>,
    /// Row position of the hidden storage key (when scanned with `with_key`).
    pub key_col: Option<usize>,
    /// A JOIN USING's merged columns: not addressable by name unless the join has an alias.
    pub anonymous: bool,
    /// Row position of the first system column (when the scan appends them).
    pub sys_col: Option<usize>,
    /// A system catalog's oid (its `tableoid`).
    pub sys_table: Option<u32>,
    /// RETURNING old / new: the row position of the flag saying the row exists (the whole
    /// row is NULL when it doesn't).
    pub exists_col: Option<usize>,
    /// A rule action's OLD / NEW: reachable only qualified, and not part of `*` (in the
    /// relation namespace only).
    pub qualified_only: bool,
    /// The location of the FROM item (a relation name) it was bound from.
    pub src: Option<i32>,
    /// The range-table name the deparser shows when it isn't `alias`: the relation's current
    /// name, under an alias the engine added when the relation was renamed (`ast::alias_name`).
    pub shown: Option<String>,
    /// The table or view whose row this is (its whole-row reference has its row type), when
    /// `table` doesn't say: a view, a rule's OLD / NEW.
    pub rel: Option<u32>,
    /// On the nullable side of an outer join: its whole-row reference is NULL for a
    /// null-extended row (a Var's nullingrels).
    pub nullable: bool,
    /// Visibility to LATERAL references (check_lateral_ref_ok): LAT_SIBLING (hidden from
    /// a FROM subquery not marked LATERAL), LAT_TARGET (a DML target, which FROM items
    /// can't reference), LAT_JOIN (the left side of a RIGHT / FULL join, which its right
    /// side can't).
    pub lateral: u8,
    /// A join's own range-table entry (a join alias): not searched for column hints.
    pub join_rte: bool,
    /// Inside a JOIN expression: its columns are reached through the join, which has no
    /// system columns.
    pub in_join: bool,
    /// A relation the planner never pulls up (a CTE read by a CTE Scan): its whole row is a
    /// Var to it (`r.*`), its IS [NOT] NULL test not expanded.
    pub whole_var: bool,
}

/// `ScopeCol::attnum` of a recursive self-reference's SEARCH / CYCLE column: `merged` (not
/// part of `*`), yet reachable unqualified too.
pub(crate) const AUX_ATTNUM: i16 = -1000;

pub(crate) const LAT_SIBLING: u8 = 1;
pub(crate) const LAT_TARGET: u8 = 2;
pub(crate) const LAT_JOIN: u8 = 3;

#[derive(Clone, Debug, Default)]
pub struct Scope {
    pub rels: Vec<ScopeRel>,
}

impl Scope {
    /// Shift every column expression (when this scope moves to the right of a join).
    pub fn shifted(mut self, delta: usize) -> Scope {
        for r in &mut self.rels {
            for c in &mut r.cols {
                c.expr = c.expr.clone().shift_cols(delta as isize);
            }
            if let Some(k) = &mut r.key_col {
                *k += delta;
            }
            if let Some(k) = &mut r.sys_col {
                *k += delta;
            }
            if let Some(k) = &mut r.exists_col {
                *k += delta;
            }
        }
        self
    }

    pub fn extend(&mut self, other: Scope) {
        self.rels.extend(other.rels);
    }

    /// Two FROM items can't use the same name.
    pub fn check_disjoint(&self, other: &Scope) -> Result<()> {
        for r in &other.rels {
            if r.anonymous || r.alias.is_empty() {
                continue;
            }
            if self.rels.iter().any(|x| {
                !x.anonymous
                    && x.alias == r.alias
                    && (x.schema.is_none() || r.schema.is_none() || x.schema == r.schema)
            }) {
                return Err(Error::new(
                    code::DUPLICATE_ALIAS,
                    format!("table name \"{}\" specified more than once", r.alias),
                ));
            }
        }
        Ok(())
    }
}

/// Where an expression appears: decides whether aggregates, window functions and
/// set-returning functions are allowed, and names the clause in errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clause {
    Select,
    Where,
    Having,
    GroupBy,
    OrderBy,
    JoinOn,
    FromFunction,
    Values,
    /// A single-row VALUES list (INSERT, MERGE INSERT): set-returning functions parse.
    ValuesSingle,
    Limit,
    Offset,
    Returning,
    Check,
    Default,
    IndexExpr,
    IndexPredicate,
    StatsExpr,
    Generated,
    PartitionKey,
    PartitionBound,
    UpdateSet,
    MergeWhen,
    /// A window's PARTITION BY / ORDER BY (set-returning functions run below it).
    Window,
    /// A window frame's offsets.
    WindowFrame,
    /// An aggregate's FILTER condition.
    Filter,
    /// COPY FROM's WHERE condition.
    CopyWhere,
    DistinctOn,
    Call,
    Policy,
    /// A FROM subquery being bound: its query level's clause (an aggregate of that level
    /// can't be there).
    FromSubselect,
    Other,
}

impl Clause {
    pub fn name(self) -> &'static str {
        match self {
            Clause::Select => "SELECT",
            Clause::Where => "WHERE",
            Clause::Having => "HAVING",
            Clause::GroupBy => "GROUP BY",
            Clause::OrderBy => "ORDER BY",
            Clause::JoinOn => "JOIN conditions",
            Clause::FromFunction => "functions in FROM",
            Clause::Values | Clause::ValuesSingle => "VALUES",
            Clause::Limit => "LIMIT",
            Clause::Offset => "OFFSET",
            Clause::Returning => "RETURNING",
            Clause::Check => "check constraints",
            Clause::Default => "DEFAULT expressions",
            Clause::IndexExpr => "index expressions",
            Clause::StatsExpr => "statistics expressions",
            Clause::IndexPredicate => "index predicates",
            Clause::Generated => "column generation expressions",
            Clause::PartitionKey => "partition key expressions",
            Clause::PartitionBound => "partition bound",
            Clause::UpdateSet => "UPDATE",
            Clause::MergeWhen => "MERGE WHEN conditions",
            Clause::Window | Clause::WindowFrame => "window definitions",
            Clause::Filter => "FILTER",
            Clause::CopyWhere => "COPY FROM WHERE conditions",
            Clause::DistinctOn => "DISTINCT ON",
            Clause::Call => "CALL",
            Clause::Policy => "policy expressions",
            Clause::FromSubselect => "FROM clause of their own query level",
            Clause::Other => "this context",
        }
    }

    pub(crate) fn aggs_ok(self) -> bool {
        matches!(
            self,
            Clause::Select
                | Clause::Having
                | Clause::OrderBy
                | Clause::Window
                | Clause::WindowFrame
                | Clause::DistinctOn
        )
    }

    fn windows_ok(self) -> bool {
        matches!(self, Clause::Select | Clause::OrderBy | Clause::DistinctOn)
    }

    fn srfs_ok(self) -> bool {
        matches!(
            self,
            Clause::Select
                | Clause::FromFunction
                | Clause::OrderBy
                | Clause::GroupBy
                | Clause::Window
                | Clause::DistinctOn
                | Clause::ValuesSingle
        )
    }
}

/// What a SELECT collects while its expressions are bound.
#[derive(Default)]
pub(crate) struct SelectState {
    pub aggs: Vec<AggCall>,
    pub windows: Vec<WindowCall>,
    pub srfs: Vec<SrfCall>,
    pub named_windows: Vec<(String, pb::WindowDef)>,
    /// Deparse mode: each window function call as written (its name and OVER), by index.
    pub window_defs: Vec<(String, pb::WindowDef)>,
    pub in_agg: bool,
    pub in_window: bool,
    /// Inside CASE / COALESCE: set-returning functions are an error.
    pub srf_banned: Option<&'static str>,
    /// GROUP BY expressions (for GROUPING()).
    pub group_exprs: Vec<Expr>,
    /// Per GROUP BY expression: the select-list item it names (by alias or position),
    /// whose own grouping key it is (two items of equal value stay two keys).
    pub group_targets: Vec<Option<usize>>,
    pub grouping_sets: bool,
    /// The FROM clause reads the recursive CTE's work table (a recursive term).
    pub self_ref: bool,
    /// Where the first aggregate of this level is (for errors about it).
    pub agg_loc: Option<i32>,
    /// How many of `srfs` GROUP BY made (the select list's same calls are theirs).
    pub group_srfs: usize,
}

pub(crate) struct Level {
    pub scope: Scope,
    pub params: Vec<Expr>,
    pub st: SelectState,
    pub clause: Clause,
    /// Names from enclosing levels are not visible (view and function bodies).
    pub barrier: bool,
    /// A SQL function body: its arguments (name, type), read as this level's params.
    pub fn_args: Option<Vec<(Option<String>, DataType)>>,
    /// PL/pgSQL variables' collations, by argument index (None: their type's).
    pub fn_arg_colls: Vec<Option<String>>,
    pub fn_name: String,
    /// PL/pgSQL `label.variable` names: (label, variable, argument index).
    pub fn_quals: Vec<(String, String, usize)>,
    /// More names of PL/pgSQL variables (`ALIAS FOR`): (name, argument index).
    pub fn_aliases: Vec<(String, usize)>,
    /// PL/pgSQL variables (`fn_args`): how a name that is also a column resolves. `None`: a
    /// SQL function, whose columns win.
    pub var_conflict: Option<VarConflict>,
    /// A SQL function body the planner inlines over constant arguments: references to the
    /// arguments are those constants, folded like any (`substitute_actual_parameters`).
    pub inline_args: bool,
}

/// `plpgsql.variable_conflict` / `#variable_conflict`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VarConflict {
    Error,
    UseVariable,
    UseColumn,
}

impl VarConflict {
    pub fn parse(s: &str) -> Option<VarConflict> {
        match s {
            "error" => Some(VarConflict::Error),
            "use_variable" => Some(VarConflict::UseVariable),
            "use_column" => Some(VarConflict::UseColumn),
            _ => None,
        }
    }
}

impl Level {
    pub(crate) fn new() -> Level {
        Level {
            scope: Scope::default(),
            params: Vec::new(),
            st: SelectState::default(),
            clause: Clause::Other,
            barrier: false,
            fn_args: None,
            fn_arg_colls: Vec::new(),
            fn_name: String::new(),
            fn_quals: Vec::new(),
            fn_aliases: Vec::new(),
            var_conflict: None,
            inline_args: false,
        }
    }

    /// The argument (PL/pgSQL variable) `name` names, by its name or an alias of it.
    pub(crate) fn fn_arg_index(&self, name: &str) -> Option<usize> {
        let args = self.fn_args.as_ref()?;
        args.iter()
            .position(|(n, _)| n.as_deref() == Some(name))
            .or_else(|| {
                self.fn_aliases
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, i)| *i)
                    .filter(|i| *i < args.len())
            })
    }
}

/// A CTE visible by name.
#[derive(Clone, Debug)]
pub(crate) struct CteDef {
    pub name: String,
    pub id: usize,
    pub cols: Vec<OutCol>,
    /// Set while binding a recursive CTE's own recursive term: references read the work table.
    pub working: bool,
    /// Trailing columns added by SEARCH / CYCLE: in the recursive term they are only
    /// reachable qualified and not part of `*`.
    pub aux: usize,
    /// Level depth the CTE was defined at (references from deeper levels still just scan it).
    pub used: std::cell::Cell<bool>,
    /// INSERT / UPDATE / DELETE / MERGE without RETURNING: it can't be read.
    pub no_returning: bool,
    /// inline_cte: its query, put in place of each reference as a subquery.
    pub inline: Option<std::sync::Arc<Plan>>,
    /// The recursive term's CYCLE check over its work table's row (`NOT is_cycle`): a
    /// restriction of the work table's scan, as the rewritten WHERE clause is.
    pub work_filter: Option<Expr>,
}

pub struct Binder<'b, 'a> {
    pub ctx: &'b ExecCtx<'a>,
    pub cat: Arc<Catalog>,
    pub params: Vec<DataType>,
    pub(crate) levels: Vec<Level>,
    pub(crate) ctes: Vec<Vec<CteDef>>,
    /// The query level (index into `levels`) of each `ctes` scope's WITH (the last
    /// `ctes.len()` of them are `ctes`'s).
    pub(crate) cte_levels: Vec<usize>,
    /// Items of non-recursive WITH lists being analyzed that aren't visible yet (the one
    /// analyzed and those after it: `p_future_ctes`).
    pub(crate) future_ctes: Vec<String>,
    /// Levels that are SQL function bodies (`$n` reads their arguments).
    pub(crate) fn_levels: Vec<usize>,
    /// Source positions of string literals (keyed by their text's address), for input errors.
    pub(crate) lit_locs: HashMap<usize, i32>,
    /// Folded string constants whose collation isn't their type's default (keyed by their
    /// text's address; the value keeps it alive): Postgres's folded Const keeps the
    /// collation its expression derived.
    pub(crate) const_colls: HashMap<usize, (Value, Coll)>,
    /// Scalar subqueries (by id) whose column is a table column with a collation other
    /// than its type's default: the collation the subquery derives.
    pub(crate) sub_colls: HashMap<usize, Coll>,
    /// Deparse mode: the query each sublink (subquery id) runs, as written.
    pub(crate) sublink_nodes: HashMap<usize, pb::Node>,
    /// Deparse mode: the columns each NATURAL join (by its node's address) joins on.
    pub(crate) natural_using: HashMap<usize, Vec<String>>,
    /// Deparse mode: an aliased join's scope inside its alias, by `JoinExpr` address (its
    /// columns numbered from the join's first).
    pub(crate) join_inner: HashMap<usize, Scope>,
    /// The next simple SELECT is a set-operation leaf: unknown-typed literals stay unknown
    /// so the set operation resolves them.
    pub(crate) keep_unknown: bool,
    /// The statement mentions a system column, so table scans append them.
    pub(crate) want_sys: bool,
    /// Where the system columns of the table whose policies `rls_using` binds are, in the
    /// row its quals filter (None: not there).
    pub(crate) rls_sys: Option<usize>,
    /// Binding for display (`pg_get_expr` & co.): keep casts and calls as written instead of
    /// folding them, like the trees Postgres stores.
    pub deparse: bool,
    /// Privileges the statement needs.
    pub(crate) perms: perms::Perms,
    /// Role relation privileges are checked as (a view's owner inside its query).
    pub(crate) check_as: Option<u32>,
    /// Inside an inlined SECURITY DEFINER body: its owner (what current_user is there).
    pub(crate) definer: Option<u32>,
    /// Tables whose policies are being expanded (to catch recursion).
    pub(crate) rls_stack: Vec<u32>,
    /// Views whose queries are being expanded (a view reading itself is infinite recursion).
    pub(crate) view_stack: Vec<u32>,
    /// FROM-clause subqueries and views bound so far.
    pub(crate) hidden_rtes: usize,
    /// The last whole-row reference bound was of a relation whose whole row is a Var to the
    /// planner (`ScopeRel::whole_var`).
    pub(crate) whole_var_ref: bool,
    /// The statement runs without parameters: `$n` is an error.
    pub no_params: bool,
    /// Binding a query that isn't planned (CREATE VIEW, CREATE TABLE AS ... WITH NO
    /// DATA): what fails only when it's planned or starts (folding a constant call, a merge
    /// join without a collation) doesn't.
    pub unplanned: bool,
    /// Binding the arms of a UNION ALL under a constant LIMIT (the level they are at, the
    /// count and the offset): each arm is planned for the rows it reads
    /// (set_subquery_pathlist passes the tuple_fraction down).
    pub(crate) arm_limit: Option<(usize, f64, f64)>,
    /// Binding a set operation's tree: how many of its leaf queries are bound so far
    /// (their range table entries are `*SELECT* n`).
    pub(crate) setop_leaves: Option<usize>,
    /// Binding a set operation's tree: whether a leaf's columns were coerced to the tree's
    /// types (is_simple_union_all_recurse: such a UNION ALL is no appendrel).
    pub(crate) setop_coerced: bool,
    /// A FROM subquery being bound whose query level the query above plans (a Subquery
    /// Scan's, once it knows the columns read: remove_unused_subquery_outputs): the level
    /// it is at, whose planning waits; `deferred` once it did.
    pub(crate) defer_level: Option<usize>,
    pub(crate) deferred: bool,
    /// IN / ANY sub-selects as bound, before their level was planned (by subquery id):
    /// pull_up_sublinks pulls a join tree up from these (Postgres pulls sublinks up before
    /// planning anything); one that stays a SubPlan runs the plan made of it.
    pub(crate) unplanned_subs: std::collections::HashMap<usize, Arc<Plan>>,
    /// Binding the arms of a CASE: a constant call that fails stays a call until the CASE
    /// knows which arms are reachable (eval_const_expressions never simplifies the others).
    pub(crate) defer_fold: bool,
    /// Inside `bind_statement`: the first constant call that failed to fold (it stays a
    /// call, and the statement fails with its error once analysed).
    pub(crate) fold_err: Option<Option<crate::error::Error>>,
    /// Binding a statement of a SQL function run statement by statement (`call_sql`): its
    /// body is the statement itself.
    pub sql_fn_statement: bool,
    /// Binding a stored query (a view's, a materialized view's): a trigger's transition
    /// tables can't be read.
    pub persistent: bool,
    /// The locations of the source values `insert_row` coerces (a type mismatch points at
    /// its value: transformAssignedExpr).
    pub(crate) insert_locs: Vec<i32>,
    /// Binding a call the executor makes through fmgr (a partition key's support
    /// function): a SQL function's body is never inlined into it.
    pub(crate) fmgr_call: bool,
    /// A custom plan: `$n` is its value.
    pub param_consts: bool,
    /// Relations that exist but can't be referenced here (the other side of a MERGE NOT
    /// MATCHED clause), for errors.
    pub(crate) unreachable: Vec<ScopeRel>,
    /// Binding a MERGE (view column errors say "merge into").
    pub(crate) in_merge: bool,
    /// Binding INSERT ... ON CONFLICT DO UPDATE (view column errors say "insert into").
    pub(crate) in_conflict: bool,
    /// While binding MERGE ... RETURNING: the level and row position of merge_action().
    pub(crate) merge_action: Option<(usize, usize)>,
    /// Binding UPDATE / DELETE of a parent table that has children (or a partitioned
    /// table): the source reads the whole tree through one Append.
    pub(crate) dml_inh: bool,
    /// Where the first call of a user-defined function was bound (virtual generated columns
    /// can't use them).
    pub user_fn_loc: Option<i32>,
    /// The next SQL function body bound gets `Level::inline_args`.
    pub fold_args: bool,
    /// The statement whose WITH clause is bound next: how often it names each of its
    /// items (cterefcount), for inlining them.
    pub(crate) cte_refs: Option<std::collections::HashMap<String, usize>>,
    /// The privileges a SQL function run by the executor needs (`in_sql_fn`).
    pub fn_perms: Option<perms::Perms>,
    /// Collecting dependencies (pg_depend): (catalog, oid) of the user types values were
    /// coerced to and the user operators applied. Views referenced along the way don't
    /// contribute (nor their relations and functions).
    pub(crate) deps: Option<Vec<(u32, u32)>>,
    /// With `deps`: the columns of composite types' relations read through functions'
    /// results (`ScopeCol::rowtype_col`).
    pub(crate) rowtype_deps: Vec<(u32, i16)>,
    /// Binding a stored view's query: the function result columns it read when it was
    /// made (`ViewDef::rowtype_cols`).
    pub(crate) view_rowtypes: Option<Arc<Vec<(u32, i16, String, DataType)>>>,
    /// The view just bound: its columns that fail where they are used (by position).
    pub(crate) view_raised: Vec<(usize, Expr)>,
    /// Binding a rule action: the triggering statement's rows its OLD / NEW read (the
    /// `*RULE*` FROM item).
    pub(crate) rule_src: Option<rules::RuleSource>,
    /// (relation, event) of the rules being applied, outermost first (to catch recursion).
    pub(crate) rule_events: Vec<(u32, char)>,
    /// Binding an INSERT's rows for its relation's rules: the relation (a view too) is
    /// written as itself.
    pub(crate) rule_target: Option<u32>,
    /// CREATE RULE's check of an action: views are written as themselves, and no rules
    /// apply.
    pub(crate) rule_validate: bool,
    /// Binding the query of a view locked FOR UPDATE / SHARE: its relations need UPDATE.
    pub(crate) view_lock: bool,
    /// A FOR UPDATE / SHARE pushed into a FROM subquery or a view's query (applyLockingClause,
    /// pushedDown): the binder level of the query it locks every relation of.
    pub(crate) pushed_lock: Option<(usize, select::Locking)>,
    /// The events (trigger type bits) a write through an updatable view does: a view under
    /// it with INSTEAD OF triggers for them takes the write.
    pub(crate) view_events: u16,
    /// Recording what the names of a stored definition resolve to (`refs`).
    pub(crate) refs: Option<Box<refs::RefLog>>,
    next_id: usize,
}

/// Make what the binder reads of the catalog through thread-locals (made casts, built-in
/// operator families with made types) `cat`'s: each binder does, and so does each statement
/// and DDL command, whose lookups outside a binder (CREATE AGGREGATE's support functions,
/// PL/pgSQL assignment casts) would see the last binder's on the thread, which may be of a
/// catalog before a DROP CAST or a rollback.
pub(crate) fn note_catalog(cat: &crate::catalog::Catalog) {
    crate::types::cast::set_user_casts(cat.casts.values().map(|c| &**c));
    optimize::note_alias_families(cat);
}

/// The binder's thread-local flags not at rest (`scoped::unsettled_for_test`).
pub(crate) fn unsettled(out: &mut Vec<&'static str>) {
    expr::unsettled(out);
    sublink::unsettled(out);
}

impl<'b, 'a> Binder<'b, 'a> {
    pub fn new(ctx: &'b ExecCtx<'a>, params: Vec<DataType>) -> Self {
        let cat = ctx.catalog();
        note_catalog(&cat);
        partition::set_pruning(
            ctx.sess
                .settings
                .borrow()
                .get_raw("enable_partition_pruning")
                .is_none_or(|v| !matches!(v.as_str(), "off" | "false" | "0")),
        );
        Binder {
            cat,
            ctx,
            params,
            levels: vec![Level::new()],
            ctes: Vec::new(),
            cte_levels: Vec::new(),
            future_ctes: Vec::new(),
            fn_levels: Vec::new(),
            lit_locs: HashMap::new(),
            const_colls: HashMap::new(),
            sub_colls: HashMap::new(),
            sublink_nodes: HashMap::new(),
            natural_using: HashMap::new(),
            join_inner: HashMap::new(),
            keep_unknown: false,
            want_sys: false,
            rls_sys: None,
            deparse: false,
            perms: perms::Perms::default(),
            check_as: None,
            definer: None,
            rls_stack: Vec::new(),
            view_stack: Vec::new(),
            hidden_rtes: 0,
            whole_var_ref: false,
            no_params: false,
            unplanned: false,
            arm_limit: None,
            setop_leaves: None,
            setop_coerced: false,
            defer_level: None,
            deferred: false,
            unplanned_subs: Default::default(),
            defer_fold: false,
            fold_err: None,
            sql_fn_statement: false,
            persistent: false,
            insert_locs: Vec::new(),
            fmgr_call: false,
            param_consts: false,
            unreachable: Vec::new(),
            in_merge: false,
            in_conflict: false,
            merge_action: None,
            dml_inh: false,
            user_fn_loc: None,
            fold_args: false,
            cte_refs: None,
            fn_perms: None,
            deps: None,
            rowtype_deps: Vec::new(),
            view_rowtypes: None,
            view_raised: Vec::new(),
            rule_src: None,
            rule_events: Vec::new(),
            rule_target: None,
            rule_validate: false,
            view_lock: false,
            pushed_lock: None,
            view_events: 0,
            refs: None,
            next_id: 1,
        }
    }

    /// A new scan's place in the range table (`ScanPlan::rti`): a pulled-up subquery's
    /// relations after the query's own (by query level), then in the order bound.
    pub(crate) fn next_rti(&mut self) -> crate::sql::plan::Rti {
        let depth = self.levels.len();
        crate::sql::plan::Rti((depth << 40) | (self.fresh_id() & ((1 << 40) - 1)))
    }

    /// A query bound elsewhere (an inlined CTE's) read here as a subquery: its scans take
    /// places in the range table after this level's, keeping their order.
    pub(crate) fn restamp_rti(&mut self, p: &mut crate::sql::plan::Plan) {
        self.restamp_rti_below(p, 0)
    }

    /// `restamp_rti` for the scans at query levels `min_depth` and deeper only.
    pub(crate) fn restamp_rti_below(&mut self, p: &mut crate::sql::plan::Plan, min_depth: usize) {
        fn scans<'p>(p: &'p mut crate::sql::plan::Plan, out: &mut Vec<&'p mut crate::sql::plan::ScanPlan>) {
            if let crate::sql::plan::Plan::Scan(s) = p {
                out.push(s);
                return;
            }
            for c in crate::sql::explain::plan_inputs_mut(p) {
                scans(std::sync::Arc::make_mut(c), out);
            }
        }
        let mut all = Vec::new();
        scans(p, &mut all);
        all.retain(|s| s.rti.0 >> 40 >= min_depth);
        all.sort_by_key(|s| s.rti);
        let depth = self.levels.len() + 1;
        for s in all {
            s.rti = crate::sql::plan::Rti((depth << 40) | (self.fresh_id() & ((1 << 40) - 1)));
        }
    }

    pub fn fresh_id(&mut self) -> usize {
        // Unique per process run (not per binder), so cached subquery results never collide
        // across statements sharing an ExecCtx: a SQL function's statements, each bound by a
        // binder of its own, run in their caller's context, whose sub-selects and CTEs are
        // cached by id. (Wrapping well below the ids EXPLAIN and the planner hand out.)
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(2);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % (1 << 38);
        self.next_id = id.max(2);
        self.next_id
    }

    pub(crate) fn level(&mut self) -> &mut Level {
        self.levels.last_mut().unwrap()
    }

    pub(crate) fn scope(&self) -> &Scope {
        &self.levels.last().unwrap().scope
    }

    pub(crate) fn clause(&self) -> Clause {
        self.levels.last().unwrap().clause
    }

    /// A sub-select bound now is in a WHERE or JOIN ... ON clause (`Subquery::qual`).
    pub(crate) fn binding_qual(&self) -> bool {
        matches!(self.clause(), Clause::Where | Clause::JoinOn)
    }

    /// Binding the target list (EXPRKIND_TARGET: the select list and its resjunk entries,
    /// an UPDATE's SET list, a one-row INSERT's values, RETURNING).
    pub(crate) fn binding_tlist(&self) -> bool {
        matches!(
            self.clause(),
            Clause::Select
                | Clause::OrderBy
                | Clause::DistinctOn
                | Clause::Window
                | Clause::Returning
                | Clause::UpdateSet
                | Clause::ValuesSingle
        )
    }

    /// Run `f` with `clause` as the current clause.
    pub(crate) fn in_clause<T>(
        &mut self,
        clause: Clause,
        f: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let prev = std::mem::replace(&mut self.level().clause, clause);
        let r = f(self);
        self.level().clause = prev;
        r
    }

    /// Bind inside a new query level. Returns the result and the level's params.
    pub(crate) fn in_level<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<(T, Vec<Expr>)> {
        self.levels.push(Level::new());
        // A subquery is planned on its own: its constant calls fold (and fail) whatever
        // CASE arm it sits in.
        let defer = std::mem::take(&mut self.defer_fold);
        let r = f(self);
        self.defer_fold = defer;
        let lvl = self.levels.pop().unwrap();
        Ok((r?, lvl.params))
    }

    /// Bind with a fresh FROM scope and SELECT state at the same level (set-operation
    /// branches, which run within the same outer row).
    pub(crate) fn in_sibling<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let scope = std::mem::take(&mut self.level().scope);
        let st = std::mem::take(&mut self.level().st);
        let r = f(self);
        let lvl = self.level();
        lvl.scope = scope;
        lvl.st = st;
        r
    }

    /// Make an expression from level `from` usable in the current level, adding params to
    /// every level in between.
    pub(crate) fn lift(&mut self, from: usize, mut e: Expr) -> Expr {
        let cur = self.levels.len() - 1;
        for l in from + 1..=cur {
            let ty = e.ty();
            let params = &mut self.levels[l].params;
            let idx = match params.iter().position(|p| p.same_as(&e)) {
                Some(i) => i,
                None => {
                    params.push(e);
                    params.len() - 1
                }
            };
            e = Expr::Outer(1, idx, ty);
        }
        e
    }

    // ── statements ──

    /// Bind a statement that runs as a plan: SELECT / VALUES / INSERT / UPDATE / DELETE.
    pub fn bind_statement(&mut self, n: &pb::Node) -> Result<BoundQuery> {
        // Postgres folds constant calls when it plans, after parse analysis: a call that
        // fails to fold fails only once the whole statement has been analysed.
        let prev = self.fold_err.replace(None);
        let r = self.bind_statement_inner(n);
        let pending = std::mem::replace(&mut self.fold_err, prev).flatten();
        match (r, pending) {
            (Ok(_), Some(e)) => Err(e),
            (r, _) => r,
        }
    }

    fn bind_statement_inner(&mut self, n: &pb::Node) -> Result<BoundQuery> {
        crate::stack::check()?;
        let _rls = optimize::begin_statement_rls();
        // (Row-level security applying to a table the statement names: its own quals
        // sit at a higher security level.)
        if let Some(ne) = n.node.as_ref() {
            // (A security barrier view too.)
            let applies = ne.nodes().into_iter().any(|(x, ..)| match x {
                pg_query::NodeRef::RangeVar(rv) => {
                    self.cat.tables.values().any(|t| {
                        t.rls && t.name == rv.relname && self.rls_enabled(t).unwrap_or(false)
                    }) || self.cat.views.values().any(|v| {
                        v.name == rv.relname && v.option("security_barrier")
                    })
                }
                _ => false,
            });
            if applies {
                optimize::mark_statement_rls();
            }
        }
        self.bind_statement_body(n)
    }

    fn bind_statement_body(&mut self, n: &pb::Node) -> Result<BoundQuery> {
        self.want_sys |= mentions_sys_column(n);
        // A relation with rules: the statements they make of it (planned already).
        if let Some((plan, cols)) = self.bind_rewritten(n)? {
            let params = self
                .params
                .iter()
                .map(|t| {
                    if matches!(t, DataType::Unknown) {
                        DataType::Text
                    } else {
                        t.clone()
                    }
                })
                .collect();
            return Ok(BoundQuery {
                plan: Arc::new(plan),
                cols,
                params,
                perms: Arc::new(self.perms.clone()),
                hidden_rtes: self.hidden_rtes,
            });
        }
        let (plan, cols) = match ast::inner(n) {
            Some(pg_query::NodeEnum::SelectStmt(s)) => self.bind_select_stmt(s)?,
            Some(pg_query::NodeEnum::InsertStmt(s)) => self.bind_insert(s)?,
            Some(pg_query::NodeEnum::UpdateStmt(s)) => self.bind_update(s)?,
            Some(pg_query::NodeEnum::DeleteStmt(s)) => self.bind_delete(s)?,
            Some(pg_query::NodeEnum::MergeStmt(s)) => self.bind_merge(s)?,
            _ => return Err(crate::error::internal("not a query statement")),
        };
        // Access paths of a data-modifying statement's scans (SELECTs plan their own).
        let mut plan = plan;
        if !self.deparse && !matches!(ast::inner(n), Some(pg_query::NodeEnum::SelectStmt(_))) {
            self.hidden_rtes += crate::sql::paths::plan_query(self.ctx, &mut plan);
            if !self.unplanned {
                check_merge_keys(self.ctx, &mut plan)?;
            }
        }
        let params = self
            .params
            .iter()
            .map(|t| {
                if matches!(t, DataType::Unknown) {
                    DataType::Text
                } else {
                    t.clone()
                }
            })
            .collect();
        Ok(BoundQuery {
            plan: Arc::new(plan),
            cols,
            params,
            perms: Arc::new(self.perms.clone()),
            hidden_rtes: self.hidden_rtes,
        })
    }

    /// Bind a scalar expression with no FROM (defaults, CALL arguments).
    pub fn bind_scalar(&mut self, n: &pb::Node, clause: Clause) -> Result<Expr> {
        self.in_clause(clause, |b| b.bind_expr(n))
    }

    /// A table's virtual generated columns read as their expressions over its row (as the
    /// Postgres rewriter expands them), so a scan of a parent computes them its own way.
    pub(crate) fn virtual_columns(&mut self, t: &Arc<Table>, rel: &mut ScopeRel) -> Result<()> {
        if self.deparse {
            return Ok(());
        }
        for (i, c) in t.columns.iter().enumerate() {
            if let (true, false, Some(sql)) = (c.virtual_gen, c.dropped, &c.generated) {
                let n = ast::parse_expr(sql)?;
                let e = self.bind_over_table(t, &n, Clause::Generated)?;
                let e = self.coerce_assign(e, &c.ty, &c.name)?;
                // The column reads with its own collation, whatever the expression derives.
                rel.cols[i].expr = match &c.collation {
                    Some(coll) if c.ty.collatable() => {
                        Expr::Collate(Box::new(e), coll.clone(), IMPLICIT_COLLATE)
                    }
                    _ => e,
                };
            }
        }
        Ok(())
    }

    /// Bind an expression over a table's row (CHECK constraints, index expressions,
    /// generated columns).
    pub fn bind_over_table(
        &mut self,
        t: &Arc<Table>,
        n: &pb::Node,
        clause: Clause,
    ) -> Result<Expr> {
        let mut rel = table_rel(t, &t.name, None);
        // A policy may read the row's system columns (`USING (ctid IN ...)`).
        if clause == Clause::Policy {
            rel.sys_col = Some(t.columns.len());
        }
        let saved = std::mem::replace(&mut self.level().scope, Scope { rels: vec![rel] });
        let r = self.in_clause(clause, |b| b.bind_expr(n));
        self.level().scope = saved;
        r
    }

    // ── coercion ──

    /// Convert `e` to `to` if a cast is allowed in `cc`; `err` builds the error otherwise.
    pub fn coerce_with(
        &mut self,
        e: Expr,
        to: &DataType,
        cc: CastContext,
        err: &dyn Fn(&DataType) -> Error,
    ) -> Result<Expr> {
        if matches!(to, DataType::Any | DataType::Unknown) || to.is_polymorphic() {
            return Ok(e);
        }
        let from = e.ty();
        if let Some(deps) = &mut self.deps
            && from != *to
        {
            let oid = crate::catalog::system::type_oid(&self.cat, to);
            if self.cat.user_type_name(oid).is_some() && !deps.contains(&(1247, oid)) {
                deps.push((1247, oid));
            }
        }
        // A reg* constant (a literal: what else is cast at run time) depends on the object it
        // names (find_expr_references).
        if self.deps.is_some()
            && matches!(from, DataType::Unknown)
            && let Expr::Const(v, _) = &e
            && !v.is_null()
            && let Some(class) = reg_class_of(to)
            && let Ok(Value::Oid(oid)) = cast_value(v.clone(), &from, to, self.ctx.fmt(), true)
            && if class == crate::catalog::depend::TYPE {
                self.cat.user_type_name(oid).is_some()
            } else {
                self.cat.class_of(oid) == Some(class)
            }
            && let Some(deps) = &mut self.deps
            && !deps.contains(&(class, oid))
        {
            deps.push((class, oid));
        }
        // Like coerce_to_target_type: the coercion goes under the COLLATE, which stays only on
        // a collatable result.
        if let Expr::Collate(..) = e
            && from != *to
        {
            let (mut inner, mut top) = (e, None);
            while let Expr::Collate(i, c, l) = inner {
                top.get_or_insert((c, l));
                inner = *i;
            }
            let r = self.coerce_with(inner, to, cc, err)?;
            return Ok(match top {
                Some((c, l)) if to.collatable() => Expr::Collate(Box::new(r), c, l),
                _ => r,
            });
        }
        match e {
            Expr::Param(i, DataType::Unknown) => {
                let base = to.without_typmod();
                if let Some(p) = self.params.get_mut(i) {
                    *p = base.clone();
                }
                let p = Expr::Param(i, base.clone());
                return if to.typmod() >= 0 {
                    Ok(Expr::Cast(
                        Box::new(p),
                        base,
                        to.clone(),
                        cc == CastContext::Explicit,
                    ))
                } else {
                    Ok(p)
                };
            }
            Expr::Const(v, DataType::Unknown) => {
                let v = match v {
                    Value::Null => Value::Null,
                    Value::Text(s) => {
                        // Like Postgres: input without the typmod (errors point at the
                        // literal), then a separate length coercion (errors don't).
                        let loc = self
                            .lit_locs
                            .get(&(s.as_ptr() as usize))
                            .copied()
                            .unwrap_or(-1);
                        let explicit = cc == CastContext::Explicit;
                        let base = match to {
                            // (An array's element modifier is a length coercion too.)
                            DataType::Array(e)
                                if e.typmod() >= 0 && !matches!(**e, DataType::Interval(_)) =>
                            {
                                DataType::array_of(e.without_typmod())
                            }
                            _ if to.typmod() < 0 || matches!(to, DataType::Interval(_)) => {
                                to.clone()
                            }
                            _ => to.without_typmod(),
                        };
                        let v = parse_value_ex(&s, &base, self.ctx.fmt(), explicit)
                            .map_err(|e| ast::with_pos(e, loc))?;
                        // A range over a domain: its bounds went through the domain's input.
                        if let (DataType::Range(rt), Value::Range(r)) = (&base, &v)
                            && let Some(d) = rt.subtype_domain
                            && let Some(dc) = self.domain_coerce(d)?
                        {
                            for b in [&r.lower, &r.upper].into_iter().flatten() {
                                dc.check(b, self.ctx).map_err(|e| ast::with_pos(e, loc))?;
                            }
                        }
                        // A reg* literal of a stored definition names its object by oid.
                        if let Value::Oid(o) = &v
                            && loc >= 0
                            && let Some(class) = match base {
                                DataType::Regclass => Some(crate::catalog::depend::REL),
                                DataType::Regproc | DataType::Regprocedure => {
                                    Some(crate::catalog::depend::PROC)
                                }
                                DataType::Regtype => Some(crate::catalog::depend::TYPE),
                                _ => None,
                            }
                            && let Some(log) = self.ref_log()
                        {
                            log.literals.push((loc, class, *o));
                        }
                        // The input function's NOTICEs point at the literal too.
                        for n in crate::types::tsearch::take_notices() {
                            self.ctx.notice(ast::with_pos(n, loc));
                        }
                        if base != *to {
                            return self.fold_cast(
                                Expr::Const(v, base.clone()),
                                base,
                                to.clone(),
                                explicit,
                            );
                        }
                        v
                    }
                    v => v,
                };
                return Ok(Expr::Const(v, to.clone()));
            }
            _ => {}
        }
        // A domain value passed as its base type: Postgres relabels it, and deparsing shows that.
        if self.deparse
            && from.same_base(to)
            && !matches!(e, Expr::Relabel(..))
            && self.domain_of(&e).is_some()
        {
            return Ok(Expr::Relabel(Box::new(e), to.clone()));
        }
        if from.same_base(to)
            && !matches!((&from, to), (DataType::Row(_), _) | (DataType::Record, _))
        {
            if to.typmod() >= 0 && from.typmod() != to.typmod()
                || matches!(to, DataType::Bpchar(_)) && from.typmod() != to.typmod()
            {
                return self.fold_cast(e, from, to.clone(), cc == CastContext::Explicit);
            }
            // An explicit cast dropping the typmod (`numeric(18,3)::numeric`) still marks the
            // new typmod (coerce_type_typmod's RelabelType).
            if cc == CastContext::Explicit
                && from.typmod() >= 0
                && to.typmod() < 0
                && !matches!(e, Expr::Const(..))
            {
                return self.fold_cast(e, from, to.clone(), true);
            }
            return Ok(e);
        }
        if let (DataType::Enum(a, ..), DataType::Enum(b, ..)) = (&from, to)
            && a == b
        {
            return Ok(e);
        }
        if matches!(from, DataType::Unknown) {
            // An unknown-typed non-constant (e.g. an untyped subquery column): text input.
            return self.fold_cast(e, DataType::Text, to.clone(), cc == CastContext::Explicit);
        }
        if matches!(to, DataType::Record) && matches!(from, DataType::Row(_) | DataType::Record) {
            return Ok(e);
        }
        if let (DataType::Row(ff), DataType::Row(tf)) = (&from, to) {
            if ff == tf || tf.oid == 0 {
                return Ok(e);
            }
            // A table's row as an ancestor's (ConvertRowtypeExpr): its fields by name. A
            // whole-row reference keeps the conversion (shown `(c.*)::parent`).
            if ff.oid != 0
                && ff.oid != tf.oid
                && tf.fields.iter().all(|(n, _)| ff.fields.iter().any(|(m, _)| m == n))
                && crate::sql::func::can_coerce(&from, to)
            {
                return Ok(match e {
                    e if is_whole_row(&e) => Expr::Cast(
                        Box::new(e),
                        from.clone(),
                        to.clone(),
                        cc == CastContext::Explicit,
                    ),
                    Expr::Row(items, _) if items.len() == ff.fields.len() => {
                        let mut out = Vec::with_capacity(tf.fields.len());
                        for (name, _) in &tf.fields {
                            let i = ff.fields.iter().position(|(m, _)| m == name).unwrap_or(0);
                            out.push(items[i].clone());
                        }
                        Expr::Row(out, to.clone())
                    }
                    // A constant converts now.
                    Expr::Const(v, _) if !self.deparse => {
                        let v = cast_value(v, &from, to, self.ctx.fmt(), true)?;
                        Expr::Const(v, to.clone())
                    }
                    // Successive conversions are one (the planner removes the redundant
                    // ConvertRowtypeExprs).
                    Expr::Cast(inner, f0, DataType::Row(mid), x)
                        if mid.oid != 0
                            && matches!(&f0, DataType::Row(r0) if r0.oid != 0)
                            && !self.deparse =>
                    {
                        Expr::Cast(inner, f0, to.clone(), x || cc == CastContext::Explicit)
                    }
                    e => Expr::Cast(
                        Box::new(e),
                        from.clone(),
                        to.clone(),
                        cc == CastContext::Explicit,
                    ),
                });
            }
            // ROW(...) to a named composite: field by field.
            if let Expr::Row(items, _) = e {
                let cannot = |more: &str| {
                    Error::new(
                        code::CANNOT_COERCE,
                        format!("cannot cast type record to {}", to.display()),
                    )
                    .detail(format!("Input has too {more} columns."))
                };
                let n = items.len();
                let mut items = items.into_iter();
                let mut out = Vec::with_capacity(tf.len());
                for (_, ft) in tf.iter() {
                    let item = items.next().ok_or_else(|| cannot("few"))?;
                    out.push(self.coerce_with(item, ft, cc, err)?);
                }
                if n > tf.len() {
                    return Err(cannot("many"));
                }
                // Constants make a constant of the type (eval_const_expressions).
                if !self.deparse
                    && let Some(vals) = out
                        .iter()
                        .map(|x| match x {
                            Expr::Const(v, _) => Some(v.clone()),
                            _ => None,
                        })
                        .collect::<Option<Vec<Value>>>()
                {
                    return Ok(Expr::Const(
                        Value::Record(std::sync::Arc::new(vals)),
                        to.clone(),
                    ));
                }
                // Postgres's tree keeps an explicit cast of a ROW() (`ROW(...)::t`).
                if cc == CastContext::Explicit {
                    let anon = DataType::row(
                        out.iter()
                            .enumerate()
                            .map(|(i, e)| (format!("f{}", i + 1), e.ty()))
                            .collect(),
                    );
                    return Ok(Expr::Cast(
                        Box::new(Expr::Row(out, anon.clone())),
                        anon,
                        to.clone(),
                        true,
                    ));
                }
                return Ok(Expr::Row(out, to.clone()));
            }
            return Ok(Expr::Cast(
                Box::new(e),
                from,
                to.clone(),
                cc == CastContext::Explicit,
            ));
        }
        if matches!(from, DataType::Record) && matches!(to, DataType::Row(r) if r.oid != 0) {
            return Ok(Expr::Cast(
                Box::new(e),
                from,
                to.clone(),
                cc == CastContext::Explicit,
            ));
        }
        let (fo, toid) = (
            crate::catalog::system::type_oid(&self.cat, &from),
            crate::catalog::system::type_oid(&self.cat, to),
        );
        if let Some(c) = self.cat.user_cast(fo, toid).cloned() {
            let ctx = match c.context {
                'i' => CastContext::Implicit,
                'a' => CastContext::Assignment,
                _ => CastContext::Explicit,
            };
            if ctx > cc {
                return Err(err(&from));
            }
            return self.user_cast_expr(e, &from, to, &c, cc == CastContext::Explicit);
        }
        match cast_context(&from, to) {
            Some(c) if c <= cc => self.fold_cast(e, from, to.clone(), cc == CastContext::Explicit),
            _ => Err(err(&from)),
        }
    }

    /// A conversion by a made cast: its function, the same bits (WITHOUT FUNCTION) or I/O.
    fn user_cast_expr(
        &mut self,
        e: Expr,
        from: &DataType,
        to: &DataType,
        c: &crate::catalog::userobj::Cast,
        explicit: bool,
    ) -> Result<Expr> {
        match c.method {
            'f' => {
                let Some(f) = self.cat.functions.get(&c.func).cloned() else {
                    return Err(crate::error::unsupported(format!(
                        "a cast by built-in function {}",
                        c.func
                    )));
                };
                let arg_ty = f.args.first().map(|a| a.1.clone()).unwrap_or(from.clone());
                let arg = if arg_ty.oid() == from.oid() {
                    e
                } else {
                    binary_coercion(e, &arg_ty)
                };
                let mut args = vec![arg];
                if f.args.len() > 1 {
                    args.push(Expr::Const(Value::Int4(to.typmod()), DataType::Int4));
                }
                if f.args.len() > 2 {
                    args.push(Expr::Const(Value::Bool(explicit), DataType::Bool));
                }
                let ret = f.ret.clone();
                let call = self.user_call(f, args, -1)?;
                Ok(if ret.oid() == to.oid() {
                    call
                } else {
                    binary_coercion(call, to)
                })
            }
            'b' => Ok(binary_coercion_as(e, to, explicit, !self.deparse)),
            _ => self.fold_cast(e, from.clone(), to.clone(), explicit),
        }
    }

    /// The (non-array) domain a value has: a domain-typed column, or a cast to a domain.
    fn domain_of(&self, e: &Expr) -> Option<u32> {
        let (level, col) = match e {
            Expr::CoerceToDomain(_, d) => return (!d.array).then_some(d.oid),
            Expr::Col(i, _) => (self.levels.len().checked_sub(1)?, *i),
            Expr::Outer(up, i, _) => (self.levels.len().checked_sub(1 + *up)?, *i),
            _ => return None,
        };
        let sc = self
            .levels
            .get(level)?
            .scope
            .rels
            .iter()
            .flat_map(|r| &r.cols)
            .find(|c| matches!(c.expr, Expr::Col(j, _) if j == col))?;
        let d = self
            .cat
            .tables
            .get(&sc.table)?
            .columns
            .get(usize::try_from(sc.attnum).ok()?.checked_sub(1)?)?
            .domain?;
        self.cat
            .domain_type(d)
            .filter(|(_, array)| !array)
            .map(|_| d)
    }

    /// The domain or array-of-domain type an expression is declared as (`exprType` when
    /// that is one): a cast to it, a column declared as it (of a table, or of a subquery,
    /// view, CTE or function whose output keeps it), a function declared to return it, an
    /// element or slice of an array of it, or a COALESCE / CASE / GREATEST / LEAST whose
    /// inputs all have it (select_common_type keeps a type they share).
    pub(crate) fn declared_domain(&self, e: &Expr) -> Option<u32> {
        self.declared_domain_at(e, self.levels.len())
    }

    fn declared_domain_at(&self, e: &Expr, depth: usize) -> Option<u32> {
        let common = |items: &mut dyn Iterator<Item = &Expr>| -> Option<u32> {
            let mut out = None;
            for x in items {
                let d = self.declared_domain_at(x, depth)?;
                if out.is_some_and(|o| o != d) {
                    return None;
                }
                out = Some(d);
            }
            out
        };
        let (level, col) = match e {
            Expr::CoerceToDomain(_, d) => return Some(d.oid),
            Expr::Col(i, _) => (depth.checked_sub(1)?, *i),
            Expr::Outer(up, i, _) => (depth.checked_sub(1 + *up)?, *i),
            Expr::Call(f, _, _) if f.oid >= crate::catalog::FIRST_USER_OID => {
                return self.cat.functions.get(&f.oid)?.ret_domain;
            }
            Expr::Call(f, args, _)
                if matches!(f.ret, crate::sql::func::Ret::Poly | crate::sql::func::Ret::PolyArray) =>
            {
                return self.poly_result_domain(f, args, depth);
            }
            // A polymorphic aggregate (array_agg).
            Expr::Agg(i, _) => {
                let a = self.levels.get(depth.checked_sub(1)?)?.st.aggs.get(*i)?;
                if a.user.is_some() {
                    return None;
                }
                let sig = &a.def.sig;
                return match &sig.ret {
                    crate::sql::func::Ret::Poly | crate::sql::func::Ret::PolyArray => {
                        self.poly_result_domain(sig, &a.args, depth)
                    }
                    // anyarray in, anyarray out: the argument's (flattened) array type.
                    crate::sql::func::Ret::Fixed(DataType::AnyArray) => {
                        let d = self.declared_domain_at(a.args.first()?, depth)?;
                        match self.cat.domain_type(d)? {
                            (_, true) => Some(d),
                            (x, false) => self.cat.domain_array_base(x),
                        }
                    }
                    _ => None,
                };
            }
            Expr::Coalesce(args, _) | Expr::MinMax(args, ..) => {
                return common(&mut args.iter());
            }
            // A call of a user function (its body run as a subquery).
            Expr::Subquery(sq) if sq.call.is_some() => {
                let f = sq.call.as_ref()?.def.oid;
                return self.cat.functions.get(&f)?.ret_domain;
            }
            // A STRICT one: NULL for NULL arguments.
            Expr::Case { whens, else_: Some(e), .. }
                if whens.len() == 1
                    && matches!(&whens[0], (Expr::Or(_), Expr::Const(Value::Null, _)))
                    && matches!(&**e, Expr::Subquery(sq) if sq.call.is_some()) =>
            {
                return self.declared_domain_at(e, depth);
            }
            Expr::Case { whens, else_, .. } => {
                // (A missing ELSE is a NULL of unknown type: no common domain.)
                let else_ = else_.as_deref()?;
                return common(&mut whens.iter().map(|(_, r)| r).chain([else_]));
            }
            // ARRAY[...] of a domain: its array type; of an array of one (a sub-array): that.
            Expr::Array(items, _) => {
                let d = common(&mut items.iter())?;
                return match self.cat.domain_type(d)? {
                    (_, true) => Some(d),
                    (x, false) => Some(x.array_oid).filter(|&a| a != 0),
                };
            }
            Expr::Subscript(base, subs, _) => {
                let d = self.declared_domain_at(base, depth)?;
                let slice = subs.iter().any(|s| s.2);
                return self.subscript_domain(d, slice);
            }
            _ => return None,
        };
        let cols = self
            .levels
            .get(level)?
            .scope
            .rels
            .iter()
            .flat_map(|r| &r.cols)
            .filter(|c| matches!(c.expr, Expr::Col(j, _) if j == col));
        for sc in cols {
            if let Some(d) = sc.domain {
                return Some(d);
            }
            if sc.table != 0 && sc.attnum > 0 {
                return self
                    .cat
                    .tables
                    .get(&sc.table)?
                    .columns
                    .get(usize::try_from(sc.attnum).ok()? - 1)?
                    .domain;
            }
        }
        None
    }

    /// The domain (or array-of-domain) type a polymorphic built-in's result is declared as
    /// (enforce_generic_type_consistency): an `anyelement` / `anycompatible` argument's
    /// type as it is, an `anyarray` argument's element type once a domain over an array is
    /// flattened to that array.
    fn poly_result_domain(
        &self,
        f: &crate::sql::func::FuncDef,
        args: &[Expr],
        depth: usize,
    ) -> Option<u32> {
        // (Some(None): a type that's no domain.)
        let mut elem: Option<Option<u32>> = None;
        let mut mixed = false;
        for (t, a) in f.args.iter().zip(args) {
            let d = self.declared_domain_at(a, depth);
            let e = match t {
                DataType::AnyElement
                | DataType::AnyNonArray
                | DataType::AnyCompatible
                | DataType::AnyCompatibleNonArray => d,
                DataType::AnyArray | DataType::AnyCompatibleArray => {
                    let arr = d.and_then(|d| match self.cat.domain_type(d)? {
                        (_, true) => Some(d),
                        (x, false) => self.cat.domain_array_base(x),
                    });
                    arr.and_then(|a| self.cat.domain_type(a).map(|(x, _)| x.oid))
                }
                _ => continue,
            };
            match elem {
                None => elem = Some(e),
                Some(x) if x != e => mixed = true,
                _ => {}
            }
        }
        let e = elem.flatten().filter(|_| !mixed)?;
        match f.ret {
            crate::sql::func::Ret::Poly => Some(e),
            _ => match self.cat.domain_type(e)? {
                (x, false) => Some(x.array_oid).filter(|&a| a != 0),
                (_, true) => None,
            },
        }
    }

    /// The domain (or array-of-domain) type all of `items` are declared as.
    pub(crate) fn common_domain(&self, items: &[Expr]) -> Option<u32> {
        let mut out = None;
        for x in items {
            let d = self.declared_domain(x)?;
            if out.is_some_and(|o| o != d) {
                return None;
            }
            out = Some(d);
        }
        out
    }

    /// The domain type an element (or a slice) of a value of domain or array-of-domain type
    /// `d` has (transformContainerType: a domain over an array is subscripted as its base
    /// array type).
    pub(crate) fn subscript_domain(&self, d: u32, slice: bool) -> Option<u32> {
        let (dom, array) = self.cat.domain_type(d)?;
        if array {
            return Some(if slice { d } else { dom.oid });
        }
        // A domain over an array of a domain: the base array type's element domain.
        let base = self.cat.domain_array_base(dom)?;
        if slice {
            Some(base)
        } else {
            self.cat.domain_type(base).filter(|(_, a)| *a).map(|(x, _)| x.oid)
        }
    }

    /// `assign_collations_walker`: the collation `e` derives (explicit from a COLLATE, else
    /// implicit from its inputs or its type).
    /// The declared collation of column `attnum` of table (or system catalog) `table`,
    /// when not its type's.
    pub(crate) fn declared_collation(&self, table: u32, attnum: i16) -> Option<String> {
        let n = usize::try_from(attnum).ok()?.checked_sub(1)?;
        match self.cat.tables.get(&table) {
            Some(t) => t.columns.get(n)?.collation.clone(),
            None if table < crate::catalog::FIRST_USER_OID => {
                crate::catalog::system::sys_column_collation(table, n + 1).map(str::to_string)
            }
            None => None,
        }
    }

    /// The collation a query's output column derives from its target `e` (None: not
    /// collatable).
    pub(crate) fn output_collation(&self, e: &Expr) -> Result<Option<Coll>> {
        // An unknown-typed literal becomes text (under COLLATE too).
        if matches!(e.ty(), DataType::Unknown) {
            return Ok(Some(match self.collation_of(e)? {
                Coll::None => Coll::Implicit("default".into()),
                c => c,
            }));
        }
        if !e.ty().collatable() {
            return Ok(None);
        }
        Ok(Some(self.collation_of(e)?))
    }

    pub(crate) fn collation_of(&self, e: &Expr) -> Result<Coll> {
        self.collation_at(e, self.levels.len())
    }

    /// `collation_of` with only the first `depth` levels (an outer reference's value is the
    /// level's param, an expression one level out).
    fn collation_at(&self, e: &Expr, depth: usize) -> Result<Coll> {
        let col = match e {
            Expr::Collate(_, c, IMPLICIT_COLLATE) => return Ok(Coll::Implicit(c.clone())),
            Expr::Collate(_, c, l) => return Ok(Coll::Explicit(c.clone(), *l)),
            // A domain declared with a collation gives its values that collation.
            Expr::CoerceToDomain(_, d)
                if let Some(c) = self.cat.domains.get(&d.oid).and_then(|x| x.collation.clone()) =>
            {
                return Ok(Coll::Implicit(c));
            }
            Expr::Const(Value::Text(t), _) => {
                if let Some((_, c)) = self.const_colls.get(&(t.as_ptr() as usize)) {
                    return Ok(c.clone());
                }
                None
            }
            Expr::Subquery(s) if s.kind == crate::sql::expr::SubqueryKind::Scalar => {
                if let Some(c) = self.sub_colls.get(&s.id) {
                    return Ok(c.clone());
                }
                None
            }
            Expr::Col(i, _) => depth.checked_sub(1).map(|l| (l, *i)),
            // An aggregate's / window function's result: what its arguments derive.
            Expr::Agg(i, ty) | Expr::Window(i, ty) if ty.collatable() => {
                let st = depth
                    .checked_sub(1)
                    .and_then(|l| self.levels.get(l))
                    .map(|l| &l.st);
                let args = match e {
                    Expr::Agg(..) => st.and_then(|st| st.aggs.get(*i)).map(|a| a.args.clone()),
                    _ => st.and_then(|st| st.windows.get(*i)).map(|w| w.args.clone()),
                };
                let mut merged = Coll::None;
                for a in args.unwrap_or_default() {
                    merged = merged.merge(self.collation_at(&a, depth)?)?;
                }
                return Ok(match merged {
                    Coll::None => Coll::Implicit(ty.default_collation().unwrap_or_default().to_string()),
                    m => m,
                });
            }
            // A PL/pgSQL variable: its collation (the function's input collation, or its
            // declared one).
            Expr::Outer(1, i, ty)
                if ty.collatable()
                    && let Some(c) = depth
                        .checked_sub(1)
                        .and_then(|l| self.levels.get(l))
                        .filter(|l| l.fn_args.is_some())
                        .and_then(|l| l.fn_arg_colls.get(*i))
                        .and_then(Option::clone) =>
            {
                return Ok(Coll::Implicit(c));
            }
            Expr::Outer(1, i, _)
                if let Some(p) = depth
                    .checked_sub(1)
                    .and_then(|l| self.levels.get(l))
                    .and_then(|l| l.params.get(*i)) =>
            {
                return self.collation_at(p, depth - 1);
            }
            Expr::Outer(up, i, _) => depth.checked_sub(1 + *up).map(|l| (l, *i)),
            _ => None,
        };
        let ty = e.ty();
        if !ty.collatable() && col.is_some() {
            return Ok(Coll::None);
        }
        if let Some((level, i)) = col {
            let sc = self.levels.get(level).and_then(|l| {
                l.scope
                    .rels
                    .iter()
                    .flat_map(|r| &r.cols)
                    .find(|c| matches!(c.expr, Expr::Col(j, _) if j == i))
            });
            // A subquery's, view's or CTE's column: what its query derived.
            if let Some(c) = sc.and_then(|sc| sc.collation.clone()) {
                return Ok(c);
            }
            let declared = sc.and_then(|sc| self.declared_collation(sc.table, sc.attnum));
            return Ok(Coll::Implicit(declared.unwrap_or_else(|| {
                ty.default_collation().unwrap_or_default().to_string()
            })));
        }
        // A ROW's fields are independent (their explicit collations may differ); a
        // composite has no collation (assign_collations_walker's RowExpr case).
        if let Expr::Row(items, _) = e {
            for x in items {
                self.collation_at(x, depth)?;
            }
            return Ok(Coll::None);
        }
        // So are an ANY / ALL / EXISTS sublink's comparisons, one per column; its result
        // is a boolean.
        if let Expr::Subquery(s) = e
            && matches!(
                s.kind,
                crate::sql::expr::SubqueryKind::Any
                    | crate::sql::expr::SubqueryKind::All
                    | crate::sql::expr::SubqueryKind::RowCompare
                    | crate::sql::expr::SubqueryKind::Exists
            )
        {
            for x in &s.left {
                self.collation_at(x, depth)?;
            }
            return Ok(Coll::None);
        }
        let mut merged = Coll::None;
        let mut err = None;
        let _ = e.clone().map_children(&mut |c| {
            if err.is_none() {
                match self.collation_at(&c, depth) {
                    Ok(k) => match std::mem::replace(&mut merged, Coll::None).merge(k) {
                        Ok(m) => merged = m,
                        Err(e) => err = Some(e),
                    },
                    Err(e) => err = Some(e),
                }
            }
            Ok(c)
        });
        if let Some(e) = err {
            return Err(e);
        }
        if !ty.collatable() {
            return Ok(Coll::None);
        }
        // A FULL JOIN USING column whose sides' collations conflict has none (its merged
        // COALESCE derives no collation, without an error).
        if matches!(merged, Coll::Conflict(..))
            && matches!(e, Expr::Coalesce(..))
            && self.is_merged_using(e, depth)
        {
            return Ok(Coll::None);
        }
        Ok(match merged {
            Coll::None => Coll::Implicit(ty.default_collation().unwrap_or_default().to_string()),
            m => m,
        })
    }

    /// Whether `e` is a FULL JOIN USING column of a level within `depth`.
    fn is_merged_using(&self, e: &Expr, depth: usize) -> bool {
        let text = format!("{e:?}");
        self.levels.iter().take(depth).any(|l| {
            l.scope
                .rels
                .iter()
                .filter(|r| r.anonymous && r.alias.is_empty())
                .flat_map(|r| &r.cols)
                .any(|c| matches!(c.expr, Expr::Coalesce(..)) && format!("{:?}", c.expr) == text)
        })
    }

    fn fold_cast(&mut self, e: Expr, from: DataType, to: DataType, explicit: bool) -> Result<Expr> {
        // The cast's function (an array cast's: its elements') must be executable.
        if !self.deparse {
            let (f, t) = match (&from, &to) {
                (DataType::Array(a), DataType::Array(b)) => (a.oid(), b.oid()),
                _ => (from.oid(), to.oid()),
            };
            if f != t
                && let Some(('f', func)) = crate::catalog::system::builtin_cast_method(f, t)
                && func != 0
            {
                self.note_builtin_call(func);
            }
        }
        if let Expr::Const(v, _) = &e
            && !self.deparse
        {
            let v = match cast_value(v.clone(), &from, &to, self.ctx.fmt(), explicit) {
                Ok(v) => v,
                // Not simplified yet (a default being cooked, a CASE arm that may be
                // unreachable): a literal still takes its type now, the rest (a length
                // coercion, a cast function) stays a call that fails when it runs.
                Err(err) if self.unplanned || self.defer_fold => {
                    if !matches!(from, DataType::Unknown) {
                        return Ok(Expr::Cast(Box::new(e), from, to, explicit));
                    }
                    let base = to.without_typmod();
                    if base == to {
                        return Err(err);
                    }
                    let v = cast_value(v.clone(), &from, &base, self.ctx.fmt(), explicit)?;
                    return Ok(Expr::Cast(
                        Box::new(Expr::Const(v, base.clone())),
                        base,
                        to,
                        explicit,
                    ));
                }
                Err(err) => return Err(err),
            };
            let folded = Expr::Const(v, to);
            self.note_const_coll(&e, &folded);
            return Ok(folded);
        }
        Ok(Expr::Cast(Box::new(e), from, to, explicit))
    }

    /// `folded`, the constant `e` folds into, keeps the collation `e` derives when that
    /// isn't its type's default (a name's C through a cast to text, say).
    pub(crate) fn note_const_coll(&mut self, e: &Expr, folded: &Expr) {
        let Expr::Const(v @ Value::Text(t), ty) = folded else {
            return;
        };
        if !ty.collatable() {
            return;
        }
        let Ok(c) = self.collation_of(e) else {
            return;
        };
        let default = ty.default_collation().unwrap_or_default();
        if matches!(&c, Coll::None) || matches!(&c, Coll::Implicit(n) if n == default) {
            return;
        }
        self.const_colls
            .insert(t.as_ptr() as usize, (v.clone(), c));
    }

    /// Explicit cast (`::`, CAST).
    pub fn cast_explicit(&mut self, e: Expr, to: &DataType) -> Result<Expr> {
        self.coerce_with(e, to, CastContext::Explicit, &|from| {
            Error::new(
                code::CANNOT_COERCE,
                format!("cannot cast type {} to {}", from.display(), to.display()),
            )
        })
    }

    /// Implicit coercion for a function / operator argument.
    pub fn coerce_implicit(&mut self, e: Expr, to: &DataType) -> Result<Expr> {
        self.coerce_with(e, to, CastContext::Implicit, &|from| {
            Error::new(
                code::DATATYPE_MISMATCH,
                format!("cannot cast type {} to {}", from.display(), to.display()),
            )
        })
    }

    /// Assignment coercion (INSERT / UPDATE targets).
    pub fn coerce_assign(&mut self, e: Expr, to: &DataType, column: &str) -> Result<Expr> {
        self.coerce_assign_to(e, to, None, column)
    }

    /// Assignment coercion to a column declared as domain (or array-of-domain) type
    /// `to_domain` (of base type `to`), which a type mismatch names.
    pub fn coerce_assign_to(
        &mut self,
        e: Expr,
        to: &DataType,
        to_domain: Option<u32>,
        column: &str,
    ) -> Result<Expr> {
        let col = column.to_string();
        let shown = |b: &Self, t: &DataType, d: Option<u32>| match d {
            Some(d) => b.cat.format_type(t, Some(d), &b.ctx.search_path),
            None => t.display(),
        };
        let to_shown = shown(self, to, to_domain);
        let from_domain = self.declared_domain(&e);
        let from_shown = shown(self, &e.ty(), from_domain);
        let from_ty = e.ty();
        self.coerce_with(e, to, CastContext::Assignment, &|from| {
            Error::new(
                code::DATATYPE_MISMATCH,
                format!(
                    "column \"{col}\" is of type {to_shown} but expression is of type {}",
                    if *from == from_ty {
                        from_shown.clone()
                    } else {
                        from.display()
                    }
                ),
            )
            .hint("You will need to rewrite or cast the expression.")
        })
    }

    /// `WHERE` / `HAVING` / `ON` / `CASE WHEN` arguments must be boolean.
    /// coerce_bool, with the error at `n`'s position.
    pub fn coerce_bool_at(&mut self, e: Expr, what: &str, n: &pb::Node) -> Result<Expr> {
        self.coerce_bool(e, what)
            .map_err(|err| ast::with_pos(err, ast::location(n)))
    }

    pub fn coerce_bool(&mut self, e: Expr, what: &str) -> Result<Expr> {
        let what = what.to_string();
        self.coerce_with(e, &DataType::Bool, CastContext::Implicit, &|from| {
            Error::new(
                code::DATATYPE_MISMATCH,
                format!(
                    "argument of {what} must be type boolean, not type {}",
                    from.display()
                ),
            )
        })
    }

    /// Resolve a common type for `exprs` (CASE, COALESCE, UNION, ARRAY[], VALUES) and coerce.
    pub fn unify(&mut self, exprs: Vec<Expr>, context: &str) -> Result<(Vec<Expr>, DataType)> {
        let types: Vec<DataType> = exprs.iter().map(|e| e.ty()).collect();
        let ty = unify_types(&types, context)?;
        let ctx = context.to_string();
        let out = exprs
            .into_iter()
            .map(|e| {
                let t2 = ty.clone();
                let ctx = ctx.clone();
                self.coerce_with(e, &ty, CastContext::Implicit, &move |from| {
                    Error::new(
                        code::DATATYPE_MISMATCH,
                        format!(
                            "{ctx} could not convert type {} to {}",
                            from.display(),
                            t2.display()
                        ),
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((out, ty))
    }
}

/// The names a binary-coercible conversion's call goes by (implicit / written as a cast).
pub(crate) const BINARY_COERCION: &str = "binary coercion";
pub(crate) const BINARY_COERCION_EXPLICIT: &str = "binary coercion (explicit)";

/// A binary-coercible conversion of `e` to `to` (RelabelType): the value's bits read as
/// the target type's.
pub(crate) fn binary_coercion(e: Expr, to: &DataType) -> Expr {
    binary_coercion_as(e, to, false, false)
}

/// `binary_coercion`, written as a cast when `explicit`; a constant is folded (the
/// planner's eval_const_expressions).
pub(crate) fn binary_coercion_as(e: Expr, to: &DataType, explicit: bool, fold: bool) -> Expr {
    if fold && let Expr::Const(v, _) = e {
        return Expr::Const(crate::types::cast::binary_reinterpret(v, to), to.clone());
    }
    let mut out = binary_coercion_call(e, to);
    if explicit && let Expr::Call(def, _, _) = &mut out {
        let mut d = (**def).clone();
        d.name = BINARY_COERCION_EXPLICIT;
        *def = Arc::new(d);
    }
    out
}

fn binary_coercion_call(e: Expr, to: &DataType) -> Expr {
    fn imp(a: &[Value], c: &crate::sql::func::FnCtx) -> Result<Value> {
        Ok(crate::types::cast::binary_reinterpret(a[0].clone(), c.ret))
    }
    let from = e.ty();
    let def = crate::sql::func::FuncDef {
        oid: 0,
        name: BINARY_COERCION,
        args: vec![from],
        variadic: false,
        spread: false,
        ret: crate::sql::func::Ret::Fixed(to.clone()),
        strict: true,
        volatility: 'i',
        imp,
        min_args: 1,
        srf: None,
        out_cols: Vec::new(),
        arg_names: Vec::new(),
        defaults: Vec::new(),
        coll: None,
    };
    Expr::Call(Arc::new(def), vec![e], to.clone())
}

/// common_type with enums and rows (which `common_type` doesn't know).
pub fn unify_types(types: &[DataType], context: &str) -> Result<DataType> {
    let known: Vec<&DataType> = types
        .iter()
        .filter(|t| !matches!(t, DataType::Unknown))
        .collect();
    match known.first() {
        Some(DataType::Enum(oid, ..))
            if known
                .iter()
                .all(|t| matches!(t, DataType::Enum(o, ..) if o == oid)) =>
        {
            Ok(known[0].clone())
        }
        Some(first @ (DataType::Row(_) | DataType::Record))
            if known
                .iter()
                .all(|t| matches!(t, DataType::Row(_) | DataType::Record)) =>
        {
            Ok((*first).clone())
        }
        _ => common_type(types, context),
    }
}

/// Whether a statement references ctid / xmin / xmax / cmin / cmax anywhere.
fn mentions_sys_column(n: &pb::Node) -> bool {
    let Some(ne) = n.node.as_ref() else {
        return false;
    };
    // pg_query's walker doesn't descend into RETURNING lists (of the statement, or of a
    // data-modifying WITH query in it).
    let returning_sys = |list: &Option<pb::ReturningClause>| {
        crate::sql::binder::ast::returning(list)
            .iter()
            .any(mentions_sys_column)
    };
    let own = match ast::inner(n) {
        Some(pg_query::NodeEnum::InsertStmt(s)) => returning_sys(&s.returning_clause),
        Some(pg_query::NodeEnum::UpdateStmt(s)) => returning_sys(&s.returning_clause),
        Some(pg_query::NodeEnum::DeleteStmt(s)) => returning_sys(&s.returning_clause),
        Some(pg_query::NodeEnum::MergeStmt(s)) => returning_sys(&s.returning_clause),
        _ => false,
    };
    own || ne.nodes().into_iter().any(|(node, _, _, _)| match node {
        // (pg_query's walker doesn't descend into COLLATE either: look at its argument.)
        pg_query::NodeRef::CollateClause(c) => c.arg.as_deref().is_some_and(mentions_sys_column),
        pg_query::NodeRef::ColumnRef(c) => match c.fields.last().and_then(ast::inner) {
            Some(pg_query::NodeEnum::String(s)) => {
                crate::exec::scan::SYS_COLS.iter().any(|(n, _)| *n == s.sval)
            }
            _ => false,
        },
        pg_query::NodeRef::InsertStmt(s) => returning_sys(&s.returning_clause),
        pg_query::NodeRef::UpdateStmt(s) => returning_sys(&s.returning_clause),
        pg_query::NodeRef::DeleteStmt(s) => returning_sys(&s.returning_clause),
        pg_query::NodeRef::MergeStmt(s) => returning_sys(&s.returning_clause),
        _ => false,
    })
}

/// Scope entry for a table, with columns at row positions 0.. (the table's physical layout).
pub fn table_rel(t: &Arc<Table>, alias: &str, schema: Option<String>) -> ScopeRel {
    let cols = t
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| ScopeCol {
            name: c.name.clone(),
            expr: Expr::Col(i, c.ty.clone()),
            hidden: c.dropped,
            merged: false,
            table: t.oid,
            attnum: i as i16 + 1,
            collation: None,
            src: Some(-1),
            rowtype_col: None,
            domain: c.domain,
        })
        .collect();
    ScopeRel {
        alias: alias.to_string(),
        schema,
        cols,
        table: Some(t.clone()),
        key_col: None,
        anonymous: false,
        sys_col: None,
        sys_table: None,
        exists_col: None,
        qualified_only: false,
        src: None,
        shown: None,
        rel: None,
        nullable: false,
        lateral: 0,
        join_rte: false,
        in_join: false,
        whole_var: false,
    }
}

/// Scope entry for a subquery / function result with the given output columns at `offset`.
pub fn derived_rel(alias: &str, cols: &[OutCol], offset: usize) -> ScopeRel {
    ScopeRel {
        alias: alias.to_string(),
        schema: None,
        cols: cols
            .iter()
            .enumerate()
            .map(|(i, c)| ScopeCol {
                name: c.name.clone(),
                expr: Expr::Col(offset + i, c.ty.clone()),
                hidden: false,
                merged: false,
                table: c.table,
                attnum: c.attnum,
                // Outside the query that made it, a column's collation is implicit.
                collation: c.collation.clone().map(Coll::implicit),
                src: None,
                rowtype_col: None,
                domain: c.domain,
            })
            .collect(),
        table: None,
        key_col: None,
        anonymous: false,
        sys_col: None,
        sys_table: None,
        exists_col: None,
        qualified_only: false,
        src: None,
        shown: None,
        rel: None,
        nullable: false,
        lateral: 0,
        join_rte: false,
        in_join: false,
        whole_var: false,
    }
}

/// A map from lowercase names to positions, for checking duplicates.
pub fn check_unique_names<'n>(
    names: impl Iterator<Item = &'n str>,
    err: impl Fn(&str) -> Error,
) -> Result<()> {
    let mut seen = HashMap::new();
    for n in names {
        if seen.insert(n, ()).is_some() {
            return Err(err(n));
        }
    }
    Ok(())
}

/// The catalog a reg* type's values name objects of (none for regrole: roles are shared).
fn reg_class_of(t: &DataType) -> Option<u32> {
    use crate::catalog::depend as d;
    Some(match t {
        DataType::Regclass => d::REL,
        DataType::Regtype => d::TYPE,
        DataType::Regproc | DataType::Regprocedure => d::PROC,
        DataType::Regoper | DataType::Regoperator => d::OPERATOR,
        DataType::Regnamespace => d::NAMESPACE,
        DataType::Regconfig => d::TSCONFIG,
        DataType::Regdictionary => d::TSDICT,
        DataType::Regcollation => d::COLLATION,
        _ => return None,
    })
}

/// The location of a COLLATE the binder adds to carry a collation of implicit strength (a
/// virtual generated column's own).
pub(crate) const IMPLICIT_COLLATE: i32 = -2;

/// A derived collation (parse_collate.c's strength and collation).
#[derive(Clone, Debug, PartialEq)]
pub enum Coll {
    None,
    Implicit(String),
    /// Implicit collations that disagree (the first two).
    Conflict(String, String),
    /// From a COLLATE clause (and where it is).
    Explicit(String, i32),
}

impl Coll {
    /// The collation as a column of a query's output carries it: implicit, and none when
    /// the inputs conflicted.
    pub(crate) fn implicit(self) -> Coll {
        match self {
            Coll::Explicit(c, _) => Coll::Implicit(c),
            Coll::Conflict(..) => Coll::None,
            c => c,
        }
    }

    pub(crate) fn merge(self, other: Coll) -> Result<Coll> {
        Ok(match (self, other) {
            (Coll::Explicit(a, _), Coll::Explicit(b, at)) if a != b => {
                return Err(ast::with_pos(
                    Error::new(
                        code::COLLATION_MISMATCH,
                        format!(
                            "collation mismatch between explicit collations \"{a}\" and \"{b}\""
                        ),
                    ),
                    at,
                ));
            }
            (a @ Coll::Explicit(..), _) | (_, a @ Coll::Explicit(..)) => a,
            (c @ Coll::Conflict(..), _) | (_, c @ Coll::Conflict(..)) => c,
            // A non-default implicit collation beats the default one.
            (Coll::Implicit(a), Coll::Implicit(b)) if a == "default" => Coll::Implicit(b),
            (Coll::Implicit(a), Coll::Implicit(b)) if a != b && b != "default" => Coll::Conflict(a, b),
            (Coll::None, b) => b,
            (a, _) => a,
        })
    }
}
