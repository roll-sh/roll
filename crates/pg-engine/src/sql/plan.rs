//! Query plans. The binder produces them directly (it also picks indexes and join methods), and
//! the executor runs them as pull-based iterators.

use super::expr::Expr;
use super::func::FuncDef;
use crate::catalog::system::SysTable;
use crate::catalog::{Index, Table};
use crate::exec::agg::AggDef;
use crate::types::DataType;
use std::sync::Arc;

/// A column of a plan's output, as RowDescription reports it.
#[derive(Clone, Debug)]
pub struct OutCol {
    pub name: String,
    pub ty: DataType,
    /// Source table oid and attribute number, when the column is a plain table column.
    pub table: u32,
    pub attnum: i16,
    /// An array of a domain: its array type, which clients see (a domain itself shows as
    /// its base type).
    pub domain_array: Option<u32>,
    /// The domain (or array-of-domain) type the value has, which a table made from it keeps.
    pub domain: Option<u32>,
    /// The collation the producing query's target derives (with its strength, for set
    /// operations): `Some(Coll::None)` for a collatable column with none. None: the source
    /// column's (`table`, `attnum`), else the type's.
    pub collation: Option<crate::sql::binder::Coll>,
}

impl OutCol {
    pub fn new(name: impl Into<String>, ty: DataType) -> Self {
        OutCol {
            name: name.into(),
            ty,
            table: 0,
            attnum: 0,
            domain_array: None,
            domain: None,
            collation: None,
        }
    }

    /// The type oid clients see.
    pub fn type_oid(&self) -> u32 {
        self.domain_array.unwrap_or_else(|| self.ty.oid())
    }
}

#[derive(Clone, Debug)]
pub struct SortKey {
    pub expr: Expr,
    pub desc: bool,
    pub nulls_first: bool,
    /// `ORDER BY x USING <op>`: a custom "less than".
    pub using: Option<Arc<FuncDef>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    /// EXISTS / IN: left rows with at least one match.
    Semi,
    /// NOT EXISTS: left rows without a match.
    Anti,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOpKind {
    Union,
    Intersect,
    Except,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowMark {
    Update,
    NoKeyUpdate,
    Share,
    KeyShare,
}

/// A row mark of a LockRows (PlanRowMark): one relation of the query level (a table, or an
/// inheritance tree's members), the position of its rows' storage key in the LockRows's
/// input (NULL: no row, the nullable side of an outer join), and the lock its rows take
/// (None: ROW_MARK_REFERENCE, the row only re-read when a locked row changed concurrently).
#[derive(Clone, Debug)]
pub struct LockMark {
    pub col: usize,
    pub tables: Vec<Arc<Table>>,
    pub lock: Option<(RowMark, Wait)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    Block,
    NoWait,
    SkipLocked,
}

/// How an index scan finds its range: equality on a prefix of the key columns, then an optional
/// range on the next column. `any` makes the last equality an `= ANY(array)` (several probes).
#[derive(Clone, Debug, Default)]
pub struct IndexBounds {
    pub eq: Vec<Expr>,
    pub any: Option<Expr>,
    pub lower: Option<(Expr, bool)>,
    pub upper: Option<(Expr, bool)>,
    /// The index's order when a column's made operator class orders it (its storage is in
    /// the values' own byte order): the keys an ordered scan sorts its rows by.
    pub order: Vec<SortKey>,
}

#[derive(Clone, Debug)]
pub struct ScanPlan {
    pub table: Arc<Table>,
    /// None: full scan of the primary data.
    pub index: Option<(Arc<Index>, IndexBounds)>,
    pub filter: Option<Expr>,
    /// Append the row's storage key as an extra trailing column (for UPDATE/DELETE/locking).
    pub with_key: bool,
    pub reverse: bool,
    pub mark: Option<(RowMark, Wait)>,
    /// Stop after this many rows (pushed-down LIMIT without ORDER BY).
    pub limit: Option<usize>,
    /// Append the system columns (ctid, xmin, cmin, xmax, cmax) after the key.
    pub sys: bool,
    /// The FROM-clause alias, when it differs from the table name (EXPLAIN shows it).
    pub alias: Option<String>,
    /// `WHERE CURRENT OF`: the cursor's name; the scan reads just its current row.
    pub current_of: Option<Expr>,
    /// TABLESAMPLE: the scan returns a sample of the rows.
    pub sample: Option<Box<TableSample>>,
    /// Constraint exclusion: the table's validated CHECK constraints and NOT NULL columns
    /// over its row; a filter contradicting them makes the scan empty.
    pub excl: Option<Arc<Vec<Expr>>>,
    /// Partition pruning: the partition bounds of the scanned table, from its own up to the
    /// partitioned table the query names.
    pub part: Option<Arc<Vec<PartLevel>>>,
    /// A partition named directly under constraint_exclusion = on: its partition
    /// constraint (these bounds) excludes it too (relation_excluded_by_constraints).
    pub part_excl: Option<Arc<Vec<PartLevel>>>,
    /// The access path the planner chose by cost (`sql::paths`).
    pub access: Access,
    /// FROM-clause column aliases (`t AS x(a, b)`): (column name, alias), which EXPLAIN
    /// shows the columns by.
    pub col_aliases: Option<Arc<Vec<(String, String)>>>,
    /// The restriction clauses in the order Postgres's baserestrictinfo holds them (as
    /// written, equivalence-class equalities after the others): an index's conditions on
    /// one column come in that order. (`filter` is in evaluation order.)
    pub written: Vec<Expr>,
    /// Restriction clauses that read a pulled-up subquery's PlaceHolderVar (a computed
    /// output of a subquery on an outer join's nullable side): never index quals.
    pub phv_quals: Vec<Expr>,
    /// The role privileges on the table are checked as (RTEPermissionInfo.checkAsUser: a
    /// view's owner for the tables the view reads); None: the current user.
    pub check_as: Option<u32>,
    /// The scan's place in the range table: a pulled-up subquery's relations after the
    /// query's own (by query level), then in the order bound; EXPLAIN's names of same-named
    /// relations of one level follow it (0: not known).
    pub rti: Rti,
    /// The orderings the planner found useful for the scan (the query's pathkeys, merge
    /// join columns), which its partial paths are built for too.
    pub useful: ScanUseful,
}

/// `ScanPlan::useful`: the pathkeys (as `sql::paths` builds them) and merge columns. Not
/// part of its Debug text.
#[derive(Clone, Default)]
pub struct ScanUseful(pub Option<Arc<(Vec<crate::sql::paths::PathKey>, Vec<Expr>)>>);

impl std::fmt::Debug for ScanUseful {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("_")
    }
}

/// A scan's place in the range table (`ScanPlan::rti`). Not part of its Debug text, by
/// which plans are compared (the same query bound twice is the same plan).
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rti(pub usize);

impl std::fmt::Debug for Rti {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("_")
    }
}

/// A system catalog's scan as the planner chose it: `scan` reads a stand-in table of the
/// catalog's columns and Postgres's indexes on it (`sql::paths::sys_table`), its access path
/// planned as a table's; its rows come in that index's order when `ordered` (a sort above
/// relies on it).
#[derive(Clone, Debug)]
pub struct SysPath {
    pub scan: ScanPlan,
    pub ordered: bool,
}

/// A join method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinMethod {
    NestLoop,
    Hash,
    Merge,
}

/// The planner's decisions about a join (`sql::joins`).
#[derive(Clone, Debug, Default)]
pub struct JoinMeta {
    /// Columns of the join's row that equality clauses turned into constants read:
    /// Postgres still carries them up to the join (their attr_needed stays).
    pub kept: Vec<usize>,
    /// The method chosen by cost (None: not planned: hash when there are hash keys).
    pub method: Option<JoinMethod>,
    /// A nested loop's inner side is materialized.
    pub material: bool,
    /// The inner side has at most one match per outer row (Inner Unique).
    pub inner_unique: bool,
    /// The join relation's estimated rows (as the planner sized it).
    pub rows: Option<f64>,
    /// The semi-join factors costing used: (outer match fraction, matches per row).
    pub semi: Option<(f64, f64)>,
    /// A nested loop's inner side is an index scan using every join clause.
    pub indexed: bool,
    /// A merge join's mergejoinscansel: (outer start, outer end, inner start, inner end).
    pub scansel: Option<(f64, f64, f64, f64)>,
    /// A semi / anti join run the other way round (Hash Right Semi Join, Hash / Merge
    /// Right Anti Join): the right input is the outer one.
    pub swapped: bool,
    /// A partitionwise child join planned in the order a Merge Append above it merges by:
    /// its rows come out in that order.
    pub presorted: bool,
    /// The outer / inner input is a semi join's right side unique-ified
    /// (create_unique_path).
    pub unique_outer: bool,
    pub unique_inner: bool,
    /// A hash join's bucket count as ExecChooseHashTableSize picks it from the inner
    /// side's estimated rows and width (the executor's table follows it).
    pub hash_buckets: Option<u32>,
    /// A nested loop's parameterized inner side runs under a Memoize.
    pub memoize: Option<Memo>,
    /// The columns (of the join's row) that ON conditions pushed down into the nullable
    /// side read: the join's own clause still involves them (make_outerjoininfo).
    pub pushed: Vec<usize>,
    /// A merge join's key directions (descending, nulls first), per hash key: its inputs'
    /// order (empty: ascending).
    pub merge_dirs: Vec<(bool, bool)>,
    /// `FROM f(...)` of a PL function (the join of an empty row and its call): EXPLAIN
    /// shows it as the one Function Scan Postgres runs.
    pub from_call: Option<Arc<FromCall>>,
    /// Per hash key: the clause was written `right = left` (its original orientation).
    pub hash_swapped: Vec<bool>,
    /// Per hash key of an inner join the planner built: the join clause it came from (an
    /// equivalence class's, of the join relation) has the right side's operand first, the
    /// sides having been put the other way round.
    pub key_flipped: Vec<bool>,
    /// An outer join reduce_outer_joins made another kind: the kind it was written as
    /// (subquery pull-up, which comes first in Postgres, decides PlaceHolderVars by it).
    pub written_kind: Option<JoinKind>,
    /// A projection above this join computes PlaceHolderVars at it (a pulled-up empty
    /// relation's outputs, remove_useless_result_rtes): not a subquery's select list to
    /// pull up again.
    pub phv_eval: bool,
}

/// `p` with its join (if it is one) marked as computing PlaceHolderVars in the projection
/// above it (`JoinMeta::phv_eval`).
pub fn mark_phv_eval(p: &Plan) -> Plan {
    let mut p = p.clone();
    if let Plan::Join { meta, .. } = &mut p {
        meta.phv_eval = true;
    }
    p
}

/// A function called in FROM whose body runs (not inlined), as EXPLAIN shows it.
#[derive(Clone, Debug)]
pub struct FromCall {
    pub def: Arc<FuncDef>,
    /// The call as the planner leaves it, when not the call itself: the constant it
    /// evaluates to, or a SQL function's expression put in its place.
    pub call: Option<Expr>,
    pub schema: String,
    pub alias: String,
    pub cols: Vec<String>,
    /// Its estimated rows (prorows; 1 when not SETOF) and cost (procost).
    pub rows: f64,
    pub cost: f64,
    pub width: u32,
}

/// A Memoize over a nested loop's inner side: its cache keys (over the outer row), whether
/// it compares them bit by bit (Cache Mode: binary), and whether each key has at most one
/// row (a unique inner side).
#[derive(Clone, Debug)]
pub struct Memo {
    pub keys: Vec<Expr>,
    pub binary: bool,
    pub singlerow: bool,
    /// cost_memoize_rescan's (startup, total).
    pub rescan: (f64, f64),
}

/// How a table scan reads its rows, as the planner chose it by cost.
#[derive(Clone, Debug, Default)]
pub enum Access {
    /// Not costed: the binder's own choice (`ScanPlan::index`, else a sequential scan).
    #[default]
    Unplanned,
    Seq,
    /// An index scan over `ScanPlan::index` (backward when `ScanPlan::reverse`); an
    /// index-only scan when `only`.
    Index {
        only: bool,
        quals: Vec<IndexQual>,
        /// A sort above relies on the scan's order.
        ordered: bool,
        /// A nested loop's parameterized inner side: the scans it is repeated for.
        loops: f64,
    },
    /// Bitmap index scans combined, then the heap read in physical order; the scans it
    /// is repeated for (a nested loop's parameterized inner side: 1 otherwise).
    Bitmap(Arc<BitmapQual>, f64),
    /// A TID scan: the rows at the TIDs `quals` give (alternatives, implicitly ORed:
    /// `ctid = x`, `ctid = ANY (array)`); `removed` are the restriction clauses they stand
    /// for exactly, not checked again. (Run as a scan in TID order checking the filter.)
    Tid { quals: Vec<Expr>, removed: Vec<Expr> },
    /// A TID range scan: the rows whose TIDs satisfy `quals` (ANDed `ctid < x` and the like).
    TidRange(Vec<Expr>),
}

/// An index condition: an index column compared with a value fixed for the scan.
#[derive(Clone, Debug)]
pub struct IndexQual {
    /// The index column (from 0).
    pub col: usize,
    /// The condition as the index applies it: the key on the left.
    pub expr: Expr,
    /// The scan's filter conjunct it came from.
    pub orig: Expr,
    /// Derived (a LIKE prefix range): the original conjunct is still checked.
    pub lossy: bool,
}

/// The bitmap a Bitmap Heap Scan reads.
#[derive(Clone, Debug)]
pub enum BitmapQual {
    Index {
        index: Arc<Index>,
        quals: Vec<IndexQual>,
        /// The index predicate's conditions the quals don't imply (a partial index).
        pred: Vec<Expr>,
        /// The key range the executor reads (the quals are checked on each row).
        bounds: Box<IndexBounds>,
    },
    And(Vec<BitmapQual>),
    Or(Vec<BitmapQual>),
}

/// One level of a partition tree as seen from a table in it: the partitioned table, its key
/// over the table's row, and the bound of the partition the table is in.
#[derive(Clone, Debug)]
pub struct PartLevel {
    pub parent: Arc<Table>,
    pub strategy: crate::catalog::PartStrategy,
    pub key: Vec<Expr>,
    pub types: Vec<crate::types::DataType>,
    pub bound: crate::catalog::PartBound,
    /// The other partitions' bounds (what a default partition doesn't hold).
    pub siblings: Vec<crate::catalog::PartBound>,
    /// The key's collations (`catalog::partition::key_collations`).
    pub colls: Vec<Option<Arc<crate::types::collation::Collation>>>,
    /// When a hash key column's operator class has a made support function: the key as
    /// clauses name it (before hashing), for pruning.
    pub raw: Option<Arc<RawPartKey>>,
}

/// A hash partition key before its made support functions hash it: the key columns, their
/// types, and for each hashed one its support function call over the value (`Col(0)`).
#[derive(Clone, Debug)]
pub struct RawPartKey {
    pub key: Vec<Expr>,
    pub types: Vec<crate::types::DataType>,
    pub hash: Vec<Option<Expr>>,
    /// The name of each key column's operator class's equality operator (strategy 1) when
    /// it is a made operator: clauses with it prune.
    pub eq_ops: Vec<Option<String>>,
}

/// Routing rows inserted into a partitioned table to its leaf partitions.
#[derive(Clone, Debug)]
pub struct PartRouter {
    pub table: Arc<Table>,
    pub strategy: crate::catalog::PartStrategy,
    /// The key over the routed (root-layout) row, and its display names.
    pub key: Vec<Expr>,
    pub key_names: Vec<String>,
    pub types: Vec<crate::types::DataType>,
    pub parts: Vec<(crate::catalog::PartBound, PartTarget)>,
    /// The key's collations (`catalog::partition::key_collations`).
    pub colls: Vec<Option<Arc<crate::types::collation::Collation>>>,
}

#[derive(Clone, Debug)]
pub enum PartTarget {
    Leaf(usize),
    Sub(Box<PartRouter>),
}

/// A partitioned insert target: the routing tree and the leaf partitions.
#[derive(Clone, Debug)]
pub struct PartRouting {
    pub router: PartRouter,
    pub leaves: Vec<PartLeaf>,
    /// The root's own partition constraint, when it is a partition too.
    pub check: Option<Arc<Vec<PartLevel>>>,
}

#[derive(Clone, Debug)]
pub struct PartLeaf {
    pub rules: TableRules,
    /// The leaf's whole partition constraint (ON CONFLICT DO UPDATE keeps rows in place).
    pub check: Option<Arc<Vec<PartLevel>>>,
    /// The leaf's column for each root column.
    pub map: Vec<Option<usize>>,
}

/// `TABLESAMPLE method (percent) [REPEATABLE (seed)]`.
#[derive(Clone, Debug)]
pub struct TableSample {
    /// `bernoulli` (each row) or `system` (groups of rows).
    pub method: &'static str,
    /// The sample percentage (real).
    pub percent: Expr,
    /// The seed (double precision); none: a new random sample each scan.
    pub repeatable: Option<Expr>,
}

#[derive(Clone, Debug)]
pub struct AggCall {
    pub def: Arc<AggDef>,
    pub args: Vec<Expr>,
    pub distinct: bool,
    pub filter: Option<Expr>,
    pub order_by: Vec<SortKey>,
    /// Ordered-set aggregates (percentile_cont(0.5) WITHIN GROUP (ORDER BY x)): direct args.
    pub direct: Vec<Expr>,
    pub ty: DataType,
    /// Argument types (after coercion).
    pub arg_types: Vec<DataType>,
    /// `<` for min/max over types whose order isn't the storage order (enums).
    pub cmp: Option<Arc<FuncDef>>,
    /// Written as JSON_ARRAYAGG / JSON_OBJECTAGG (a view shows it so).
    pub json_ctor: bool,
    /// A user-defined aggregate (CREATE AGGREGATE).
    pub user: Option<Arc<UserAgg>>,
}

/// A user-defined aggregate as a call runs it: its transition and final functions.
#[derive(Debug)]
pub struct UserAgg {
    /// The next state, over `[state, args…]`.
    pub trans: Expr,
    /// The transition function is strict: rows with a NULL argument are skipped, and without
    /// an initial value the first row's value is the state.
    pub trans_strict: bool,
    /// The result, over `[state]`; none: the state is the result.
    pub final_: Option<Expr>,
    pub final_strict: bool,
    /// The initial state (INITCOND); none: NULL.
    pub init: Option<Expr>,
    /// The moving-aggregate implementation (MSFUNC / MINVFUNC / MFINALFUNC / MINITCOND), for
    /// window frames whose head moves.
    pub moving: Option<Box<UserAgg>>,
    /// In a moving implementation: the inverse transition, over `[state, args…]`, and
    /// whether it is strict.
    pub inverse: Option<(Expr, bool)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameUnit {
    Rows,
    Range,
    Groups,
}

#[derive(Clone, Debug)]
pub enum FrameBound {
    UnboundedPreceding,
    Preceding(Expr),
    CurrentRow,
    Following(Expr),
    UnboundedFollowing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameExclude {
    NoOthers,
    CurrentRow,
    Group,
    Ties,
}

#[derive(Clone, Debug)]
pub struct Frame {
    pub unit: FrameUnit,
    pub start: FrameBound,
    pub end: FrameBound,
    pub exclude: FrameExclude,
    /// Written with BETWEEN (FRAMEOPTION_BETWEEN): shown so.
    pub between: bool,
}

impl Default for Frame {
    fn default() -> Self {
        Frame {
            unit: FrameUnit::Range,
            start: FrameBound::UnboundedPreceding,
            end: FrameBound::CurrentRow,
            exclude: FrameExclude::NoOthers,
            between: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WinKind {
    RowNumber,
    Rank,
    DenseRank,
    PercentRank,
    CumeDist,
    Ntile,
    Lag,
    Lead,
    FirstValue,
    LastValue,
    NthValue,
}

// One per window call in a plan; boxing the aggregate buys nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum WinFunc {
    Builtin(WinKind),
    Agg(AggCall),
}

#[derive(Clone, Debug)]
pub struct WindowCall {
    pub func: WinFunc,
    pub args: Vec<Expr>,
    pub partition_by: Vec<Expr>,
    pub order_by: Vec<SortKey>,
    pub frame: Frame,
    pub ty: DataType,
    /// lag/lead/first_value ... IGNORE NULLS
    pub ignore_nulls: bool,
    /// The FROM subquery whose window this is: its alias and column names (EXPLAIN's
    /// Subquery Scan over it).
    pub subquery: Option<Arc<(String, Vec<String>)>>,
    /// The named window it is computed over (`OVER w`): EXPLAIN shows the window by it.
    pub name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SrfCall {
    pub func: Arc<FuncDef>,
    pub args: Vec<Expr>,
    pub ty: DataType,
    /// Number of output columns (1 unless the function returns several).
    pub ncols: usize,
    /// A CREATE FUNCTION function's body: it runs with the arguments as the outer row.
    pub body: Option<Arc<Plan>>,
}

#[derive(Clone, Debug)]
pub enum ConflictAction {
    Nothing,
    /// SET columns from expressions over (existing row ++ excluded row), with optional WHERE.
    Update {
        sets: Vec<(usize, Expr)>,
        filter: Option<Expr>,
        rls_existing: Vec<RlsCheck>,
        rls_new: Vec<RlsCheck>,
    },
}

#[derive(Clone, Debug)]
pub struct OnConflict {
    /// Indexes that arbitrate conflicts (empty = any unique index).
    pub arbiters: Vec<u32>,
    pub action: ConflictAction,
}

/// A row-level security WITH CHECK option.
#[derive(Clone, Debug)]
pub struct RlsCheck {
    /// The restrictive policy it comes from (permissive ones are combined).
    pub policy: Option<String>,
    pub table: String,
    pub expr: Expr,
    /// ON CONFLICT DO UPDATE's check of the existing row ("(USING expression)").
    pub conflict: bool,
}

/// Bound per-table write rules: defaults, generated columns, checks, NOT NULLs.
#[derive(Clone, Debug)]
pub struct TableRules {
    pub table: Arc<Table>,
    /// Default per column (None = NULL).
    pub defaults: Vec<Option<Expr>>,
    /// Generated (stored) column expressions over the row.
    pub generated: Vec<Option<Expr>>,
    /// CHECK constraints: (name, expr over the row).
    pub checks: Vec<(String, Expr)>,
    /// Indexes with bound key expressions / predicates.
    pub indexes: Vec<BoundIndex>,
    /// Domain checks per column: (domain name, constraint name, expr over the value as Col(0)).
    pub domain_checks: Vec<Option<Arc<crate::sql::expr::DomainCoerce>>>,
    /// Triggers on the table, in firing (name) order.
    pub triggers: Vec<BoundTrigger>,
    /// Row-level security checks of written rows.
    pub rls_checks: Vec<RlsCheck>,
    /// The target is a view: INSTEAD OF row triggers stand in for the write.
    pub view: bool,
    /// WITH CHECK OPTION conditions of the updatable views written through, innermost first.
    pub view_checks: Vec<(String, Expr)>,
    /// A partition: its partition constraint (every level up to the root), checked on
    /// direct writes.
    pub part_check: Option<Arc<Vec<PartLevel>>>,
}

#[derive(Clone, Debug)]
pub struct BoundTrigger {
    pub trig: Arc<crate::catalog::Trigger>,
    pub func: Arc<crate::catalog::Function>,
    /// WHEN over (OLD columns ++ NEW columns).
    pub when: Option<Expr>,
    /// A foreign key's internal trigger: (the constraint, the referencing table that has it).
    pub ri: Option<(u32, u32)>,
}

#[derive(Clone, Debug)]
pub struct BoundIndex {
    pub index: Arc<Index>,
    /// Key expressions over the table row (plain columns become Col(i)).
    pub keys: Vec<Expr>,
    pub predicate: Option<Expr>,
    /// The primary key, unique or exclusion constraint the index enforces.
    pub constraint: Option<Arc<IndexConstraint>>,
}

#[derive(Clone, Debug)]
pub struct IndexConstraint {
    pub oid: u32,
    pub name: String,
    /// (DEFERRABLE, INITIALLY DEFERRED).
    pub deferral: (bool, bool),
    /// EXCLUDE: per key column, `new op existing` over the new key values followed by the
    /// existing row's.
    pub exclusion: Option<Vec<Expr>>,
    /// The first exclusion operator is `=`: only rows sharing the first key value can conflict.
    pub leading_eq: bool,
    /// A primary key or unique constraint WITHOUT OVERLAPS: its last key can't be empty.
    pub without_overlaps: bool,
}

/// RETURNING old / new: the row RETURNING sees is padded to `width`, then the old row and the
/// new row follow, each `ncols` values (the columns, then the system columns) and a flag
/// saying it exists.
#[derive(Clone, Copy, Debug)]
pub struct OldNew {
    pub width: usize,
    pub ncols: usize,
    /// The target's rows carry system columns (after their key, for UPDATE and DELETE).
    pub sys: bool,
    /// What RETURNING calls the old and new rows (`WITH (OLD AS o, NEW AS n)`).
    pub old_alias: &'static str,
    pub new_alias: &'static str,
}

#[derive(Clone, Debug)]
pub struct InsertPlan {
    pub rules: TableRules,
    /// Rows from here have one value per `target` column.
    pub source: Arc<Plan>,
    pub target: Vec<usize>,
    pub on_conflict: Option<OnConflict>,
    /// Over the new row.
    pub returning: Vec<Expr>,
    pub old_new: Option<OldNew>,
    /// OVERRIDING SYSTEM VALUE
    pub overriding_system: bool,
    pub overriding_user: bool,
    /// RETURNING sees system columns after the row.
    pub sys: bool,
    /// The target is partitioned: rows go to its partitions.
    pub route: Option<Arc<PartRouting>>,
    /// COPY FROM: each source row's input line, for errors.
    pub copy: Option<Arc<CopyLines>>,
    /// `INSERT INTO t AS alias` (EXPLAIN shows it).
    pub alias: Option<String>,
}

/// COPY FROM's input: the table and each row's line number and text
/// (`CopyFromErrorCallback`).
#[derive(Debug)]
pub struct CopyLines {
    pub table: String,
    pub lines: Vec<(usize, String)>,
    /// Rows are buffered (no BEFORE ROW triggers, no volatile defaults): index
    /// insertion happens later, when the line's text is gone.
    pub multi: bool,
    /// ON_ERROR IGNORE's notice of the rows skipped, raised after the rows are in and
    /// before the AFTER STATEMENT triggers fire.
    pub skipped_notice: Option<String>,
}

#[derive(Clone, Debug)]
pub struct UpdatePlan {
    pub rules: TableRules,
    /// Rows: the target table's columns, its storage key, then any FROM-clause columns.
    pub source: Arc<Plan>,
    /// (column, expression over the source row)
    pub sets: Vec<(usize, Expr)>,
    /// Over (new row ++ source row's extra columns).
    pub returning: Vec<Expr>,
    pub old_new: Option<OldNew>,
    /// Filter to re-check on a concurrently updated row (READ COMMITTED), over the table row.
    pub recheck: Option<Expr>,
    /// The source scan appends system columns after the key.
    pub sys: bool,
    /// A partitioned table (or an inheritance tree with partitioned members) updated: rows
    /// that leave their partition move through this routing (the table named by the
    /// statement).
    pub route: Option<Arc<PartRouting>>,
    /// The table named heads an inheritance tree (or is partitioned): the source reads every
    /// member through one Append, each row over the named table's columns and telling which
    /// table it is in by its tableoid (row identity: the key and tableoid), and each row is
    /// changed in its own table by that table's rules (`members`; `rules` for the named
    /// table's own rows).
    pub inh: bool,
    /// The named table's descendants holding rows (result relations), in expansion order.
    pub members: Vec<UpdateMember>,
    /// The target's alias (EXPLAIN names the target by it when the plan reads no row of it).
    pub alias: Option<String>,
}

/// An inheritance child (or leaf partition) an UPDATE of its ancestor changes.
#[derive(Clone, Debug)]
pub struct UpdateMember {
    /// Its own rules (row triggers only; the parent's row-level security and check options
    /// over its columns).
    pub rules: TableRules,
    /// (its column, the new value over the source row)
    pub sets: Vec<(usize, Expr)>,
    /// The named table's column → the member's.
    pub map: Vec<Option<usize>>,
}

/// An inheritance child (or leaf partition) a DELETE of its ancestor deletes from.
#[derive(Clone, Debug)]
pub struct DeleteMember {
    pub table: Arc<Table>,
    /// Its row triggers.
    pub triggers: Vec<BoundTrigger>,
    pub indexes: Vec<BoundIndex>,
    /// The named table's column → the member's.
    pub map: Vec<Option<usize>>,
}

/// A member's row over its ancestor's columns (`map`: the ancestor's column → the member's).
pub fn row_as_parent(map: &[Option<usize>], row: &[crate::types::Value]) -> Vec<crate::types::Value> {
    map.iter()
        .map(|m| {
            m.and_then(|j| row.get(j).cloned())
                .unwrap_or(crate::types::Value::Null)
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct DeletePlan {
    pub table: Arc<Table>,
    pub triggers: Vec<BoundTrigger>,
    pub source: Arc<Plan>,
    pub returning: Vec<Expr>,
    pub old_new: Option<OldNew>,
    pub recheck: Option<Expr>,
    pub indexes: Vec<BoundIndex>,
    /// The target is a view with INSTEAD OF DELETE triggers.
    pub view: bool,
    /// The target's rows carry system columns (after their key).
    pub sys: bool,
    /// As `UpdatePlan::inh`.
    pub inh: bool,
    pub members: Vec<DeleteMember>,
    /// As `UpdatePlan::alias`.
    pub alias: Option<String>,
}

/// Which rows a MERGE WHEN clause applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeWhen {
    Matched,
    NotMatchedBySource,
    NotMatchedByTarget,
}

#[derive(Clone, Debug)]
pub enum MergeCmd {
    /// (column, expression over the joined row); row-level security checks of the existing
    /// row and of the new one.
    Update {
        sets: Vec<(usize, Expr)>,
        rls_existing: Vec<RlsCheck>,
        rls_new: Vec<RlsCheck>,
    },
    Delete {
        rls_existing: Vec<RlsCheck>,
    },
    /// One value per `target` column, over the joined row.
    Insert {
        target: Vec<usize>,
        values: Vec<Expr>,
        rls_new: Vec<RlsCheck>,
    },
    Nothing,
}

#[derive(Clone, Debug)]
pub struct MergeAction {
    pub when: MergeWhen,
    /// Over the joined row.
    pub cond: Option<Expr>,
    pub cmd: MergeCmd,
}

#[derive(Clone, Debug)]
pub struct MergePlan {
    pub rules: TableRules,
    /// The source joined with the target. Rows: the source's columns, then the target
    /// table's columns from `target`, its storage key (NULL without a target row) and any
    /// system columns.
    pub source: Arc<Plan>,
    pub target: usize,
    /// A column that is TRUE when the row has a source row (for NOT MATCHED BY SOURCE).
    pub source_flag: Option<usize>,
    pub actions: Vec<MergeAction>,
    /// The join condition, re-checked against a concurrently updated target row.
    pub join_cond: Option<Expr>,
    /// Over the joined row with the target's columns replaced by the new (or deleted) row,
    /// then merge_action().
    pub returning: Vec<Expr>,
    pub old_new: Option<OldNew>,
    pub sys: bool,
    /// The target's inheritance children (matched rows say which table they are in by
    /// their tableoid system column).
    pub members: Vec<MergeMember>,
    /// A partitioned target: where new rows (and rows leaving their partition) go.
    pub route: Option<Arc<PartRouting>>,
    /// As `UpdatePlan::alias`.
    pub alias: Option<String>,
}

/// A child table of a MERGE target: its own rules, and each action's UPDATE / DELETE over
/// its row (columns mapped from the parent's).
#[derive(Clone, Debug)]
pub struct MergeMember {
    pub rules: TableRules,
    pub cmds: Vec<MergeCmd>,
    /// Parent column → the child's column.
    pub map: Vec<Option<usize>>,
    /// UPDATE columns, as the child's.
    pub update_cols: Vec<usize>,
}

impl MergePlan {
    /// Statement-level events, in BEFORE trigger order.
    pub fn events(&self) -> Vec<u16> {
        use crate::catalog::trigger_type as tt;
        let has = |f: fn(&MergeCmd) -> bool| self.actions.iter().any(|a| f(&a.cmd));
        let mut ev = Vec::new();
        if has(|c| matches!(c, MergeCmd::Insert { .. })) {
            ev.push(tt::INSERT);
        }
        if has(|c| matches!(c, MergeCmd::Update { .. })) {
            ev.push(tt::UPDATE);
        }
        if has(|c| matches!(c, MergeCmd::Delete { .. })) {
            ev.push(tt::DELETE);
        }
        ev
    }

    /// Columns any UPDATE action sets (for UPDATE OF triggers).
    pub fn update_cols(&self) -> Vec<usize> {
        let mut cols = Vec::new();
        for a in &self.actions {
            if let MergeCmd::Update { sets, .. } = &a.cmd {
                for (c, _) in sets {
                    if !cols.contains(c) {
                        cols.push(*c);
                    }
                }
            }
        }
        cols
    }
}

/// A relation proven empty (a dummy rel), as the planner left it.
#[derive(Clone, Debug, Default)]
pub struct DummyInfo {
    /// Per column, the relation it was read from (EXPLAIN qualifies its name with it).
    pub rels: Vec<Option<String>>,
    /// The equivalence classes its quals made before it was found empty (per column: its
    /// class's representative column, and whether the class holds a constant), empty when
    /// unknown: group and sort keys they make redundant stay so.
    pub ec: Vec<(usize, bool)>,
}

#[derive(Clone, Debug)]
pub enum Plan {
    /// One row with no columns (SELECT without FROM).
    One,
    /// No rows.
    Empty {
        ncols: usize,
        /// A relation proven empty (a dummy rel): its columns' names, as an expression
        /// above shows them.
        cols: Option<Arc<Vec<String>>>,
        /// What else is known of the relation found empty.
        info: Option<Arc<DummyInfo>>,
    },
    Values {
        rows: Vec<Vec<Expr>>,
        ncols: usize,
    },
    Scan(Box<ScanPlan>),
    /// A pg_catalog / information_schema virtual table.
    SysScan {
        table: SysTable,
        filter: Option<Expr>,
        /// The access path the planner chose for it, when not a sequential scan (or
        /// when its order matters).
        path: Option<Arc<SysPath>>,
        /// The FROM-clause alias, when it has one (EXPLAIN's reference name).
        alias: Option<String>,
    },
    /// ROWS FROM (f(...), g(...)) [WITH ORDINALITY]
    FunctionScan {
        calls: Vec<SrfCall>,
        ordinality: bool,
        alias: String,
        cols: Vec<String>,
    },
    /// A PL/pgSQL function call; `args` are evaluated against the outer row.
    PlCall {
        func: Arc<crate::catalog::Function>,
        args: Vec<Expr>,
        ncols: usize,
        /// The call's input collation (None: the default, or none determined), which the
        /// function's collatable variables take.
        coll: Option<String>,
    },
    /// A security_barrier view's query (or a subquery planned apart): the statement's
    /// quals that may leak its rows' values stay above it; EXPLAIN shows a Subquery Scan
    /// on `alias` (columns `cols`) when one does.
    Barrier {
        input: Arc<Plan>,
        alias: String,
        cols: Vec<String>,
        /// A security barrier's (leaky clauses stay above); else a subquery Postgres
        /// doesn't pull up (grouping sets, set-returning or volatile outputs), which takes
        /// the clauses safe to push down into it.
        secure: bool,
        /// The subquery's ORDER BY keys, when it has one: its rows come in that order, as
        /// many of its plan's sort keys as the query above may use (its useful pathkeys).
        sorted: Option<usize>,
    },
    /// `input` runs inside `frame` (a SQL function's statement), which errors and notices
    /// name in their CONTEXT, once it has the privileges `perms`.
    Context {
        input: Arc<Plan>,
        frame: crate::error::Frame,
        perms: Option<Arc<crate::sql::binder::perms::Perms>>,
        /// A VOLATILE function's statement: each run is a command of its own, with its own
        /// snapshot (a non-volatile one reads the calling query's).
        volatile: bool,
    },
    Filter {
        input: Arc<Plan>,
        pred: Expr,
    },
    Project {
        input: Arc<Plan>,
        exprs: Vec<Expr>,
    },
    /// Select list with set-returning functions: `exprs` reference SRF results as Expr::Srf(i).
    ProjectSet {
        input: Arc<Plan>,
        exprs: Vec<Expr>,
        srfs: Vec<SrfCall>,
    },
    /// Output: group keys, then aggregate results. `sets` lists grouping sets as indexes into
    /// `groups` (None = plain GROUP BY). With grouping sets a trailing int column holds the
    /// GROUPING() bitmask.
    Agg {
        input: Arc<Plan>,
        groups: Vec<Expr>,
        sets: Option<Vec<Vec<usize>>>,
        aggs: Vec<AggCall>,
    },
    /// Output: input columns, then one column per window call.
    Window {
        input: Arc<Plan>,
        calls: Vec<WindowCall>,
    },
    Sort {
        input: Arc<Plan>,
        keys: Vec<SortKey>,
    },
    Limit {
        input: Arc<Plan>,
        limit: Option<Expr>,
        offset: Option<Expr>,
        with_ties: Option<Vec<SortKey>>,
    },
    /// DISTINCT (all columns) or DISTINCT ON (exprs; input sorted by them first).
    Distinct {
        input: Arc<Plan>,
        on: Option<Vec<Expr>>,
        /// DISTINCT ON: how many of its sorted input's leading keys the query's ORDER BY
        /// asks for (the rest order the rows of a group, or only group them).
        order: usize,
    },
    SetOp {
        kind: SetOpKind,
        all: bool,
        left: Arc<Plan>,
        right: Arc<Plan>,
        /// Per output column, the nondeterministic collation it compares under (empty when
        /// none does): rows it finds equal are duplicates.
        nondet: Vec<Option<String>>,
        /// The FROM subquery it is (its alias and column names), for EXPLAIN's Subquery
        /// Scan.
        sub: Option<Arc<(String, Vec<String>)>>,
    },
    /// Each input in turn. `inh`: the scans of an inheritance tree, under the parent's
    /// reference name (each input reads one table, projected to the parent's columns).
    Append {
        inputs: Vec<Arc<Plan>>,
        inh: Option<String>,
    },
    /// Output: left columns, then right columns (Semi/Anti: left only).
    /// `hash`: equi-join keys (left exprs over left row, right exprs over right row).
    /// `params`: for LATERAL, expressions over the left row the right side reads as Outer(1, i).
    Join {
        kind: JoinKind,
        left: Arc<Plan>,
        right: Arc<Plan>,
        cond: Option<Expr>,
        hash: Option<(Vec<Expr>, Vec<Expr>)>,
        params: Vec<Expr>,
        nleft: usize,
        nright: usize,
        /// What the planner decided about it.
        meta: Box<JoinMeta>,
    },
    /// Materialise CTE `id` (run at most once), then run `body`.
    With {
        ctes: Vec<(usize, Arc<Plan>)>,
        body: Arc<Plan>,
        names: Vec<String>,
    },
    CteScan {
        id: usize,
        ncols: usize,
        name: String,
        alias: String,
        cols: Vec<String>,
    },
    /// WITH RECURSIVE: base, then iterate `recursive` (reading WorkScan{id}) until no new rows.
    Recursive {
        id: usize,
        base: Arc<Plan>,
        recursive: Arc<Plan>,
        all: bool,
        ncols: usize,
    },
    WorkScan {
        id: usize,
        ncols: usize,
        name: String,
        alias: String,
        cols: Vec<String>,
    },
    /// A trigger's transition table, by name.
    EnrScan {
        name: String,
        ncols: usize,
        /// The FROM alias, if any (EXPLAIN's reference name).
        alias: Option<String>,
        /// Column names (after any column aliases).
        cols: Vec<String>,
    },
    /// SELECT ... FOR UPDATE / SHARE: the query level's rows locked once joined and filtered,
    /// at the level's top (Postgres's LockRows over its row marks). `input`'s row is the
    /// level's `width` columns, then the storage key of each relation's row (`marks`), which
    /// isn't returned.
    LockRows {
        input: Arc<Plan>,
        marks: Arc<Vec<LockMark>>,
        width: usize,
    },
    Insert(Box<InsertPlan>),
    Update(Box<UpdatePlan>),
    Delete(Box<DeletePlan>),
    Merge(Box<MergePlan>),
}

impl Plan {
    pub fn arc(self) -> Arc<Plan> {
        Arc::new(self)
    }

    /// Whether running this plan writes.
    pub fn writes(&self) -> bool {
        match self {
            Plan::Insert(_) | Plan::Update(_) | Plan::Delete(_) | Plan::Merge(_) => true,
            Plan::With { ctes, body, .. } => ctes.iter().any(|(_, p)| p.writes()) || body.writes(),
            Plan::Filter { input, .. }
            | Plan::Project { input, .. }
            | Plan::ProjectSet { input, .. }
            | Plan::Agg { input, .. }
            | Plan::Window { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::Distinct { input, .. }
            | Plan::LockRows { input, .. }
            | Plan::Barrier { input, .. } | Plan::Context { input, .. } => input.writes(),
            Plan::SetOp { left, right, .. } | Plan::Join { left, right, .. } => {
                left.writes() || right.writes()
            }
            Plan::Append { inputs, .. } => inputs.iter().any(|p| p.writes()),
            Plan::Recursive {
                base, recursive, ..
            } => base.writes() || recursive.writes(),
            _ => false,
        }
    }
}
