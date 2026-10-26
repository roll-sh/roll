//! The executor: plans run as pull-based iterators over rows.

pub mod agg;
pub mod dml;
pub mod fasthash;
pub mod fastsort;
pub mod merge;
pub mod kernel;
pub mod need;
pub mod parscan;
pub mod partial;
pub mod nodes;
pub mod scan;
pub mod pghash;
pub mod pipe;
pub mod pgstat;
pub mod pgsort;
pub mod sizing;
pub mod trigger;
pub mod window;

use crate::catalog::Catalog;
use crate::catalog::names::CatalogNames;
use crate::error::{Error, Result, code};
use crate::session::state::SessionState;
use crate::sql::expr::{Expr, Subquery, SubqueryKind, array_op, eval};
use crate::sql::plan::Plan;
use crate::txn::Txn;
use crate::types::array::ArrayValue;
use crate::types::text::FormatCtx;
use crate::types::{DataType, Value};
use std::cell::{Cell, RefCell};
use fasthash::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

pub type Row = Vec<Value>;

/// A running plan node.
pub trait Node {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>>;

    /// A deferred DML node's AFTER triggers, once the whole statement's rows are done: the
    /// row events, then (`stmt`) the statement's.
    fn end(&mut self, _ctx: &ExecCtx, _stmt: bool) -> Result<()> {
        Ok(())
    }

    /// A DML node's target table.
    fn dml_table(&self) -> Option<Arc<crate::catalog::Table>> {
        None
    }

    /// Whether this DML node's statement triggers want the rows its inheritance children
    /// change (transition tables).
    fn captures(&self) -> bool {
        false
    }

    /// The rows a member of an inheritance DML kept for its parent's transition tables.
    fn take_captured(&mut self) -> Vec<trigger::AfterRow> {
        Vec::new()
    }

    /// Rows of the children for this (parent) DML node's transition tables.
    fn add_captured(&mut self, _rows: Vec<trigger::AfterRow>) {}

    /// Run this node's rows as a pipeline in a parallel scan's workers (`pipe`), when it
    /// can (a parallel scan below that hasn't started, through nodes that can be stages):
    /// the scan's workers run the hash join probes `above` (bottom first) after this
    /// node's own, and aggregate the rows coming out of the last as `spec` says
    /// (`partial`). Some(the level of this node's own rows): then `next_item` gives them.
    fn pipeline(
        &mut self,
        _above: Vec<Arc<pipe::Probe>>,
        _spec: Option<Arc<partial::Spec>>,
        _ctx: &ExecCtx,
    ) -> Result<Option<u8>> {
        Ok(None)
    }

    /// The next row, with the stages it has been through, or part (after `pipeline`).
    fn next_item(&mut self, ctx: &ExecCtx) -> Result<Option<pipe::Item>> {
        Ok(self.next(ctx)?.map(|r| pipe::Item::Row(r, pipe::ALL)))
    }
}

pub type BoxNode = Box<dyn Node>;

thread_local! {
    /// The notice lists of the statement contexts alive on this thread, outermost first.
    static LIVE_NOTICES: RefCell<Vec<std::rc::Weak<RefCell<Vec<Error>>>>> =
        const { RefCell::new(Vec::new()) };
    /// Where notices go out before the statement ends (a server connection's socket).
    static NOTICE_SINK: RefCell<Option<Box<dyn FnMut(&[Error])>>> = const { RefCell::new(None) };
}

/// A context's notice list, registered so a lock wait can send it.
fn live_notices() -> Rc<RefCell<Vec<Error>>> {
    let n = Rc::new(RefCell::new(Vec::new()));
    LIVE_NOTICES.with(|l| {
        let mut l = l.borrow_mut();
        l.retain(|w| w.strong_count() > 0);
        l.push(Rc::downgrade(&n));
    });
    n
}

/// Send the notices raised so far, as Postgres has by the time a statement waits (it
/// flushes every notice to the client): what a session blocked on a lock said is seen while
/// it waits.
pub fn flush_live_notices() {
    if !has_notice_sink() {
        return;
    }
    let pending: Vec<Error> = LIVE_NOTICES.with(|l| {
        let mut l = l.borrow_mut();
        l.retain(|w| w.strong_count() > 0);
        l.iter()
            .filter_map(|w| w.upgrade())
            .flat_map(|n| n.try_borrow_mut().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default())
            .collect()
    });
    if pending.is_empty() {
        return;
    }
    NOTICE_SINK.with(|s| {
        if let Ok(mut s) = s.try_borrow_mut()
            && let Some(f) = s.as_mut()
        {
            f(&pending);
        }
    });
}

/// Install (or remove) this thread's notice sink.
pub fn set_notice_sink(sink: Option<Box<dyn FnMut(&[Error])>>) {
    NOTICE_SINK.with(|s| *s.borrow_mut() = sink);
}

thread_local! {
    /// A statement is running whose notices go out as they are raised (`streaming_notices`).
    static STREAMING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` (a server connection's statement) with each notice it raises sent to the client
/// at once, through the notice sink, as Postgres sends and flushes every one
/// (send_message_to_frontend). Outside it notices are collected and go out after the
/// statement (an embedder's session, a protocol message that isn't a statement).
pub fn streaming_notices<T>(f: impl FnOnce() -> T) -> T {
    struct Back(bool);
    impl Drop for Back {
        fn drop(&mut self) {
            STREAMING.with(|s| s.set(self.0));
        }
    }
    let _back = Back(STREAMING.with(|s| s.replace(true)));
    f()
}

/// Whether notices go to the client as they are raised now.
pub fn notices_stream() -> bool {
    STREAMING.with(|s| s.get()) && has_notice_sink()
}

/// Send a notice to the client now, when notices stream (false: collect it).
pub fn send_notice_now(e: &Error) -> bool {
    if !STREAMING.with(|s| s.get()) {
        return false;
    }
    NOTICE_SINK.with(|s| match s.try_borrow_mut() {
        Ok(mut s) => match s.as_mut() {
            Some(f) => {
                f(std::slice::from_ref(e));
                true
            }
            None => false,
        },
        Err(_) => false,
    })
}

/// Whether notices can go out early on this thread.
pub fn has_notice_sink() -> bool {
    NOTICE_SINK.with(|s| s.try_borrow().is_ok_and(|s| s.is_some()))
}

/// Everything a statement's execution needs.
pub struct ExecCtx<'a> {
    pub txn: &'a RefCell<Txn>,
    pub sess: &'a SessionState,
    fmt: FormatCtx,
    pub params: &'a [Value],
    pub search_path: Vec<String>,
    /// The role whose USAGE privileges `search_path` was filtered by.
    path_role: u32,
    outer: RefCell<Vec<Rc<Row>>>,
    case_vals: RefCell<Vec<Value>>,
    subq_cache: RefCell<HashMap<usize, Rc<Vec<Row>>>>,
    pub ctes: RefCell<HashMap<usize, Rc<RefCell<nodes::CteState>>>>,
    pub work: RefCell<HashMap<usize, Rc<Vec<Row>>>>,
    /// Shared with child contexts: a COMMIT inside a procedure starts a new transaction time.
    txn_time: Rc<Cell<i64>>,
    stmt_time: i64,
    /// Rows processed by the statement (the command tag's count).
    pub processed: Cell<u64>,
    pub deadline: Option<Instant>,
    /// Notices raised and not yet sent (shared with `LIVE_NOTICES`, which a lock wait
    /// sends them from).
    pub notices: Rc<RefCell<Vec<Error>>>,
    ticks: Cell<u32>,
    /// Ephemeral named relations: a trigger's transition tables.
    pub enrs: RefCell<Vec<Rc<Enr>>>,
    /// EXPLAIN ANALYZE: per-node counters (nested statements aren't instrumented).
    pub instr: RefCell<Option<Rc<Instr>>>,
    /// EXPLAIN ANALYZE of a utility statement (CREATE TABLE AS): the plan it ran, with its
    /// hidden range-table count.
    pub explained: RefCell<Option<(Arc<Plan>, usize)>>,
    /// Keys of rows an UPDATE moved to another partition: the statement doesn't see them
    /// again when it scans that partition.
    pub moved: RefCell<std::collections::HashSet<Vec<u8>>>,
    /// Set while an inheritance DML Append opens a member: its AFTER triggers wait for the
    /// Append to end them.
    pub defer_after: Cell<bool>,
    /// Set while a DML Append opens a member whose parent keeps transition tables.
    pub capture: Cell<bool>,
    /// The statement's queue of AFTER trigger events (`trigger::fire_after`), and whether it
    /// is being run.
    pub after_queue: RefCell<std::collections::VecDeque<trigger::Pending>>,
    pub after_running: Cell<bool>,
    /// (table, event)s whose BEFORE STATEMENT triggers fired in this statement: referential
    /// actions on the table don't fire them again.
    pub stmt_fired: RefCell<std::collections::HashSet<(u32, u16)>>,
    /// The statement's transition table data (AfterTriggersTableData), shared by its
    /// commands and their referential actions until a trigger reads one.
    pub trans_data: RefCell<Vec<Rc<trigger::TransData>>>,
    /// The running statement's snapshot (`statement`), or a cursor's (as of its OPEN): its
    /// table scans read through it. None: no statement runs in this context, reads see the
    /// latest (utility commands, which see each step's writes as after a
    /// CommandCounterIncrement). Not passed to child contexts.
    snap: RefCell<Option<Rc<crate::txn::Snap>>>,
    /// Inside a non-volatile function: the snapshot of the query calling it, which the
    /// function's statements use instead of their own (passed to child contexts).
    stable_snap: RefCell<Option<Rc<crate::txn::Snap>>>,
    /// Set while a LIMIT opens its input: the bound a sort right below it gets.
    pub sort_bound: Cell<Option<usize>>,
    /// Set right before opening a plan node (`nodes::open_need`), taken as it opens: the
    /// columns of its rows anything above reads (None: all). A scan leaves the others
    /// undecoded (NULL).
    pub need: Cell<Option<Rc<[bool]>>>,
    /// Planner estimates the executor sizes hash tables by, per plan node.
    pub estimates: RefCell<HashMap<usize, f64>>,
    /// The planner's view of aggregation and grouping nodes (`pghash::agg_estimate`).
    pub agg_estimates: RefCell<HashMap<usize, Rc<pghash::AggEstimate>>>,
    /// Correlated EXISTS subqueries run as hashed lookups (by subquery id; None: not
    /// convertible).
    exists_hash: RefCell<HashMap<usize, Option<Rc<ExistsHash>>>>,
    /// Correlated scalar subqueries over a sequentially scanned table, run as hashed
    /// lookups of its rows by the correlated columns.
    scalar_hash: RefCell<HashMap<usize, Option<Rc<ScalarHash>>>>,
    /// Columns of system views (by oid) the statements checked in this context read
    /// (their permission checks note them): a view column Postgres computes with a function
    /// that can fail (pg_relation_is_updatable) fails only when read.
    pub sys_cols: RefCell<HashMap<u32, std::collections::BTreeSet<i16>>>,
    /// Inheritance children the statement planned that were dropped while it waited for
    /// their locks (find_inheritance_children leaves them out): scanned as empty.
    pub gone_rels: RefCell<std::collections::HashSet<u32>>,
    /// EvalPlanQual: while a LockRows rechecks a row a concurrent update changed, the test
    /// row each of its level's scans returns instead of reading its table (by the scan's
    /// address; None: no row).
    pub epq: RefCell<Option<Rc<EpqRows>>>,
    /// Catalogs a statement scans again and again (`nodes::sys_rescan`): each one's rows,
    /// generated once, by catalog oid.
    pub sys_rescans: RefCell<HashMap<u32, Rc<nodes::SysRescan>>>,
    /// How many times the statement generated one relation's rows of a catalog for a rescan
    /// (`nodes::sys_rescan`), by catalog oid: past a few, the whole catalog is generated.
    pub sys_rel_scans: RefCell<HashMap<u32, u32>>,
    /// FROM subqueries whose join trees hold semi / anti joins, as their levels had them
    /// before planning (`paths::BoundTree`): the query above pulls such a subquery up from
    /// that (pull_up_subqueries comes before any planning), not from the planned tree,
    /// whose semi joins may be unique-ified already.
    pub bound_trees: RefCell<Vec<crate::sql::paths::BoundTree>>,
}

/// Drop from `path` the schemas the current role may not use: name lookups skip them
/// (recomputeNamespacePath's ACL_USAGE check); temporary schemas stay.
fn usable_path(catalog: &Catalog, sess: &SessionState, path: &mut Vec<String>) -> u32 {
    let me = sess.current_role(catalog);
    if catalog.is_superuser(me) {
        return me;
    }
    let db_owner = sess.db_owner(catalog);
    path.retain(|s| match catalog.schema_by_name(s) {
        Some(sc) => {
            sc.name.starts_with("pg_temp")
                || catalog
                    .obj_mask(crate::catalog::acl::ObjKind::Schema, sc.oid, me, db_owner)
                    .0
                    & crate::catalog::acl::USAGE
                    != 0
        }
        None => true,
    });
    me
}

/// EvalPlanQual's test rows (`ExecCtx::epq`): per scan, its row's key and stored data.
pub type EpqRows = HashMap<usize, Option<(Vec<u8>, bytes::Bytes)>>;

/// A correlated scalar subquery's rows by the values of the columns its parameters
/// equal (read once instead of a scan per outer row).
pub struct ScalarHash {
    params: Vec<usize>,
    rows: HashMap<Vec<Value>, Vec<Row>>,
}

/// A correlated EXISTS as a hash table of its correlated columns' values
/// (convert_EXISTS_to_ANY): the subquery's parameters each key column compares with.
pub struct ExistsHash {
    params: Vec<usize>,
    keys: std::collections::HashSet<Vec<Value>>,
}

/// EXPLAIN ANALYZE's counters for one plan node.
#[derive(Clone, Copy, Debug, Default)]
pub struct NodeStats {
    pub loops: u64,
    pub rows: u64,
    /// Rows a scan read but its filter rejected.
    pub removed: u64,
    /// Index descents of an index scan.
    pub searches: u64,
    /// Heap fetches of an index-only scan.
    pub fetches: u64,
    /// Heap pages a bitmap heap scan read.
    pub pages: u64,
    /// A Memoize's cache hits and misses, and the bytes its cache holds.
    pub cache_hits: u64,
    pub cache_misses: u64,
    /// (Its peak.)
    pub cache_bytes: u64,
    pub cache_evictions: u64,
    /// Seconds until the first row, and in total, summed over loops.
    pub startup: f64,
    pub total: f64,
    /// A hash join's inner side: the most rows sharing one key (what no batching splits).
    pub hash_max_group: u64,
    /// And the rows it hashed, with each column's bytes over them (the first 32 columns'):
    /// what the values the table keeps take, which decides a batch's size.
    pub hash_rows: u64,
    pub hash_col_bytes: [u64; 32],
    /// A MERGE's source (counted under its key): rows inserted, updated and deleted.
    pub merged: [u64; 3],
    /// An Incremental Sort's sorts.
    pub incr: pgsort::IncrStats,
}

/// Counters per plan node, keyed by the node's address (`Arc::as_ptr`; a scan's filter by its
/// `ScanPlan`).
#[derive(Debug, Default)]
pub struct Instr {
    pub nodes: RefCell<HashMap<usize, NodeStats>>,
}

impl Instr {
    pub fn get(&self, key: usize) -> Option<NodeStats> {
        self.nodes.borrow().get(&key).copied()
    }

    fn update(&self, key: usize, f: impl FnOnce(&mut NodeStats)) {
        f(self.nodes.borrow_mut().entry(key).or_default())
    }
}

/// A node under EXPLAIN ANALYZE: counts its loops and rows and times its `next` calls.
pub struct InstrNode {
    inner: BoxNode,
    key: usize,
    instr: Rc<Instr>,
    /// This loop: rows so far and seconds spent.
    rows: u64,
    spent: f64,
}

impl InstrNode {
    pub fn wrap(inner: BoxNode, key: usize, instr: Rc<Instr>) -> BoxNode {
        instr.update(key, |s| s.loops += 1);
        Box::new(InstrNode {
            inner,
            key,
            instr,
            rows: 0,
            spent: 0.0,
        })
    }
}

impl Node for InstrNode {
    fn end(&mut self, ctx: &ExecCtx, stmt: bool) -> Result<()> {
        self.inner.end(ctx, stmt)
    }

    fn dml_table(&self) -> Option<Arc<crate::catalog::Table>> {
        self.inner.dml_table()
    }

    fn captures(&self) -> bool {
        self.inner.captures()
    }

    fn take_captured(&mut self) -> Vec<trigger::AfterRow> {
        self.inner.take_captured()
    }

    fn add_captured(&mut self, rows: Vec<trigger::AfterRow>) {
        self.inner.add_captured(rows)
    }

    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        let start = Instant::now();
        let r = self.inner.next(ctx)?;
        let t = start.elapsed().as_secs_f64();
        self.spent += t;
        let first = self.rows == 0 && r.is_some();
        if r.is_some() {
            self.rows += 1;
        }
        let (spent, has) = (self.spent, r.is_some());
        self.instr.update(self.key, |s| {
            s.total += t;
            if first {
                s.startup += spent;
            }
            s.rows += has as u64;
        });
        Ok(r)
    }
}

/// What a plan running in a context keeps there between rows, for a cursor's query run a
/// few rows at a time: CTE and work tables, cached subquery results, ephemeral relations.
#[derive(Default)]
pub struct Kept {
    subq_cache: HashMap<usize, Rc<Vec<Row>>>,
    ctes: HashMap<usize, Rc<RefCell<nodes::CteState>>>,
    work: HashMap<usize, Rc<Vec<Row>>>,
    enrs: Vec<Rc<Enr>>,
}

/// A named set of rows visible to queries like a table (REFERENCING OLD / NEW TABLE).
#[derive(Debug)]
pub struct Enr {
    pub name: String,
    pub cols: Vec<(String, crate::types::DataType)>,
    pub rows: Rc<Vec<Row>>,
}

/// Whether `p` (run `depth` correlated subqueries deep) reads a value of a query level
/// enclosing it (`Expr::Outer` past its own levels).
pub(crate) fn reads_enclosing(p: &crate::sql::plan::Plan, depth: usize) -> bool {
    fn expr(e: &crate::sql::expr::Expr, depth: usize) -> bool {
        let mut found = false;
        e.walk(&mut |x| match x {
            crate::sql::expr::Expr::Outer(l, ..) if *l > depth => found = true,
            crate::sql::expr::Expr::Subquery(s) => {
                let d = if s.params.is_empty() { depth } else { depth + 1 };
                found |= reads_enclosing(&s.plan, d);
            }
            _ => {}
        });
        found
    }
    let mut es = Vec::new();
    crate::sql::explain::node_exprs(p, &mut es);
    // (A scan's index conditions and bounds too.)
    if let crate::sql::plan::Plan::Scan(sc) = p {
        if let Some((_, b)) = &sc.index {
            es.extend(b.eq.iter().chain(&b.any));
            es.extend(b.lower.iter().chain(&b.upper).map(|x| &x.0));
        }
        if let crate::sql::plan::Access::Index { quals, .. } = &sc.access {
            es.extend(quals.iter().map(|q| &q.expr));
        }
        if matches!(sc.access, crate::sql::plan::Access::Bitmap(..) | crate::sql::plan::Access::Tid { .. } | crate::sql::plan::Access::TidRange(_)) {
            // (Conservatively: their conditions came from the filter or a join.)
            return true;
        }
    }
    es.iter().any(|e| expr(e, depth))
        || crate::sql::explain::plan_inputs(p).into_iter().any(|c| reads_enclosing(c, depth))
}

impl<'a> ExecCtx<'a> {
    pub fn new(
        txn: &'a RefCell<Txn>,
        sess: &'a SessionState,
        params: &'a [Value],
        txn_time: i64,
        stmt_time: i64,
    ) -> ExecCtx<'a> {
        Self::with_search_path(txn, sess, params, txn_time, stmt_time, sess.search_path())
    }

    /// `new`, given the session's search path (`SessionState::search_path`).
    pub fn with_search_path(
        txn: &'a RefCell<Txn>,
        sess: &'a SessionState,
        params: &'a [Value],
        txn_time: i64,
        stmt_time: i64,
        mut search_path: Vec<String>,
    ) -> ExecCtx<'a> {
        let settings = sess.settings.borrow();
        let mut fmt = settings.format_ctx();
        drop(settings);
        let catalog = txn.borrow().catalog();
        let path_role = usable_path(&catalog, sess, &mut search_path);
        let settings = sess.settings.borrow();
        let timeout = settings.statement_timeout();
        let deadline = (timeout > 0)
            .then(|| Instant::now() + std::time::Duration::from_millis(timeout as u64));
        // The lock waits of the statement: lock_timeout, deadlock_timeout, statement_timeout.
        if let Ok(mut t) = txn.try_borrow_mut() {
            let lt = settings.lock_timeout();
            t.lock_timeout = (lt > 0).then(|| std::time::Duration::from_millis(lt as u64));
            t.deadlock_timeout =
                std::time::Duration::from_millis(settings.int("deadlock_timeout").max(1) as u64);
            t.deadline = deadline;
        }
        crate::stack::set_max_depth_kb(match settings.max_stack_depth() {
            0 => 2048,
            kb => kb,
        });
        drop(settings);
        fmt.clock = crate::types::datetime::Clock { now: txn_time };
        fmt.names = Some(Arc::new(CatalogNames::new(catalog, search_path.clone())));
        ExecCtx {
            txn,
            sess,
            fmt,
            params,
            search_path,
            path_role,
            outer: RefCell::new(Vec::new()),
            case_vals: RefCell::new(Vec::new()),
            subq_cache: RefCell::new(HashMap::default()),
            ctes: RefCell::new(HashMap::default()),
            work: RefCell::new(HashMap::default()),
            txn_time: Rc::new(Cell::new(txn_time)),
            stmt_time,
            processed: Cell::new(0),
            deadline,
            notices: live_notices(),
            ticks: Cell::new(0),
            enrs: RefCell::new(Vec::new()),
            instr: RefCell::new(None),
            explained: RefCell::new(None),
            moved: RefCell::default(),
            defer_after: Cell::new(false),
            capture: Cell::new(false),
            after_queue: RefCell::default(),
            after_running: Cell::new(false),
            stmt_fired: RefCell::default(),
            trans_data: RefCell::default(),
            snap: RefCell::new(None),
            stable_snap: RefCell::new(None),
            sort_bound: Cell::new(None),
            need: Cell::new(None),
            estimates: RefCell::default(),
            agg_estimates: RefCell::default(),
            exists_hash: RefCell::default(),
            scalar_hash: RefCell::default(),
            sys_cols: RefCell::default(),
            gone_rels: RefCell::default(),
            epq: RefCell::new(None),
            sys_rescans: RefCell::default(),
            sys_rel_scans: Default::default(),
            bound_trees: RefCell::default(),
        }
    }

    /// The search path a statement starting now resolves names through: the session's
    /// `search_path` setting as it is now (a function's SET clause, `SET` or `set_config`
    /// earlier in the routine), less the schemas the current role may not use. PL/pgSQL
    /// and SQL functions bind each statement under it, as plancache revalidates a cached
    /// plan whose search_path no longer matches (`SearchPathMatchesCurrentEnvironment`).
    pub fn session_path(&self) -> Vec<String> {
        self.session_path_role().0
    }

    /// `session_path`, and the role whose privileges it was filtered by.
    fn session_path_role(&self) -> (Vec<String>, u32) {
        let mut path = self.sess.search_path();
        let role = usable_path(&self.catalog(), self.sess, &mut path);
        (path, role)
    }

    /// Whether the session's settings or current role may have changed what
    /// `session_path` gives since this context's path was made (a statement starting now
    /// may resolve names otherwise; `path_stale` says whether it does).
    pub fn path_unsure(&self) -> bool {
        self.sess.settings.borrow().generation() != self.fmt.settings
            || self.current_user() != self.path_role
    }

    /// The session's search path (`session_path`) is no longer this context's: the
    /// settings changed since it was made (a SET, `set_config`, a function's SET clause), or
    /// the current role did (a SECURITY DEFINER function: schemas are filtered by USAGE).
    pub fn path_stale(&self) -> bool {
        self.path_unsure() && self.session_path() != self.search_path
    }

    /// This context's names (reg* input, constants) resolve through an older catalog than
    /// the transaction's: DDL ran since it was made (an earlier statement of a routine).
    pub fn names_stale(&self) -> bool {
        self.fmt
            .names
            .as_ref()
            .and_then(|n| n.catalog_version())
            .is_some_and(|v| v < self.txn.borrow().catalog().version)
    }

    /// A child context (as `child`, with this one's parameters and snapshot) whose names and
    /// formatting follow the session as it is now (`session_path`, the settings' current
    /// values): what a routine's next statements run in once a statement changed them.
    pub fn resynced(&self) -> ExecCtx<'_> {
        let c = self.refreshed();
        *c.snap.borrow_mut() = self.snap.borrow().clone();
        c
    }

    pub fn fmt(&self) -> &FormatCtx {
        &self.fmt
    }

    /// Take what a running plan keeps in this context (see `Kept`).
    pub fn keep(&self) -> Kept {
        Kept {
            subq_cache: std::mem::take(&mut *self.subq_cache.borrow_mut()),
            ctes: std::mem::take(&mut *self.ctes.borrow_mut()),
            work: std::mem::take(&mut *self.work.borrow_mut()),
            enrs: self.enrs.borrow().clone(),
        }
    }

    /// Give a running plan back what it kept (see `Kept`).
    pub fn restore(&self, k: Kept) {
        *self.subq_cache.borrow_mut() = k.subq_cache;
        *self.ctes.borrow_mut() = k.ctes;
        *self.work.borrow_mut() = k.work;
        *self.enrs.borrow_mut() = k.enrs;
    }

    /// A context for running a nested statement (PL/pgSQL): same transaction, clock and
    /// deadline, but its own parameters, caches, row count and notices, and fresh names.
    pub fn child<'b>(&'b self, params: &'b [Value]) -> ExecCtx<'b> {
        let mut fmt = self.fmt.clone();
        fmt.names = Some(Arc::new(CatalogNames::new(
            self.catalog(),
            self.search_path.clone(),
        )));
        ExecCtx {
            txn: self.txn,
            sess: self.sess,
            fmt,
            params,
            search_path: self.search_path.clone(),
            path_role: self.path_role,
            outer: RefCell::new(Vec::new()),
            case_vals: RefCell::new(Vec::new()),
            subq_cache: RefCell::new(HashMap::default()),
            ctes: RefCell::new(HashMap::default()),
            work: RefCell::new(HashMap::default()),
            txn_time: self.txn_time.clone(),
            stmt_time: self.stmt_time,
            processed: Cell::new(0),
            deadline: self.deadline,
            notices: live_notices(),
            ticks: Cell::new(0),
            enrs: RefCell::new(self.enrs.borrow().clone()),
            instr: RefCell::new(None),
            explained: RefCell::new(None),
            moved: RefCell::default(),
            defer_after: Cell::new(false),
            capture: Cell::new(false),
            after_queue: RefCell::default(),
            after_running: Cell::new(false),
            stmt_fired: RefCell::default(),
            trans_data: RefCell::default(),
            snap: RefCell::new(None),
            stable_snap: RefCell::new(self.stable_snap.borrow().clone()),
            sort_bound: Cell::new(None),
            need: Cell::new(None),
            estimates: RefCell::default(),
            agg_estimates: RefCell::default(),
            exists_hash: RefCell::default(),
            scalar_hash: RefCell::default(),
            sys_cols: RefCell::default(),
            gone_rels: RefCell::default(),
            epq: RefCell::new(None),
            sys_rescans: RefCell::default(),
            sys_rel_scans: Default::default(),
            bound_trees: RefCell::default(),
        }
    }

    /// A child context (as `child`) resolving names (regclass input, …) through `cat`: the
    /// catalog as a DDL statement has changed it so far.
    pub fn child_over(&self, cat: Arc<Catalog>) -> ExecCtx<'_> {
        let mut c = self.child(self.params);
        c.fmt.names = Some(Arc::new(CatalogNames::new(cat, self.search_path.clone())));
        c
    }

    /// A child context (as `child`) that resolves names through `search_path`.
    pub fn child_with_path<'b>(&'b self, params: &'b [Value], search_path: Vec<String>) -> ExecCtx<'b> {
        let mut c = self.child(params);
        c.fmt.names = Some(Arc::new(CatalogNames::new(
            self.catalog(),
            search_path.clone(),
        )));
        c.search_path = search_path;
        c
    }

    /// A child context (as `child`) seeing the session's settings as they are now: a
    /// function's SET clauses (search_path, DateStyle, …) apply inside it.
    pub fn refreshed(&self) -> ExecCtx<'_> {
        let mut c = self.child(self.params);
        let mut fmt = self.sess.settings.borrow().format_ctx();
        fmt.clock = crate::types::datetime::Clock {
            now: self.fmt.clock.now,
        };
        (c.search_path, c.path_role) = self.session_path_role();
        fmt.names = Some(Arc::new(CatalogNames::new(
            self.catalog(),
            c.search_path.clone(),
        )));
        c.fmt = fmt;
        c
    }

    /// The catalog as this transaction sees it (including its own DDL).
    pub fn catalog(&self) -> Arc<Catalog> {
        self.txn.borrow().catalog()
    }

    /// current_user's oid.
    pub fn current_user(&self) -> u32 {
        self.sess.current_role(&self.catalog())
    }

    /// session_user's oid.
    pub fn session_user(&self) -> u32 {
        self.sess.session_role(&self.catalog())
    }

    pub fn db_owner(&self) -> u32 {
        self.sess.db_owner(&self.catalog())
    }

    /// Name of a role (its oid as text if it no longer exists).
    pub fn role_name(&self, oid: u32) -> String {
        self.catalog()
            .roles
            .get(&oid)
            .map_or_else(|| oid.to_string(), |r| r.name.clone())
    }

    /// Refresh name resolution after DDL inside the statement (e.g. a function creating a table).
    pub fn refresh_names(&mut self) {
        let c = self.catalog();
        self.fmt.names = Some(Arc::new(CatalogNames::new(c, self.search_path.clone())));
    }

    pub fn txn_time(&self) -> i64 {
        self.txn_time.get()
    }

    pub fn set_txn_time(&self, t: i64) {
        self.txn_time.set(t);
    }

    pub fn stmt_time(&self) -> i64 {
        self.stmt_time
    }

    pub fn random(&self) -> f64 {
        self.sess.random()
    }

    pub fn random_normal(&self) -> f64 {
        self.sess.random_normal()
    }

    pub fn random_i64_range(&self, rmin: i64, rmax: i64) -> i64 {
        self.sess.random_i64_range(rmin, rmax)
    }

    pub fn random_u64_range(&self, rmin: u64, rmax: u64) -> u64 {
        self.sess.random_u64_range(rmin, rmax)
    }

    pub fn set_seed(&self, seed: f64) {
        self.sess.set_seed(seed)
    }

    pub fn random_bytes(&self, out: &mut [u8]) {
        self.sess.random_bytes(out)
    }

    /// Report a notice; it gets the CONTEXT lines of what is running.
    pub fn notice(&self, e: Error) {
        self.take_input_notices();
        if !self.notice_wanted(e.severity) {
            return;
        }
        let e = e.in_frames(&self.sess.frames.borrow());
        self.raise_notice(e);
    }

    /// A notice goes to the client as it is raised when the session streams them (after
    /// what this context collected before, so they keep their order); else it is collected.
    fn raise_notice(&self, e: Error) {
        if !self.notices.borrow().is_empty() && self.sess.notice_hold.get() == 0 {
            let queued = std::mem::take(&mut *self.notices.borrow_mut());
            let mut it = queued.into_iter();
            for q in it.by_ref() {
                if !self.sess.stream_notice(&q) {
                    let mut n = self.notices.borrow_mut();
                    n.push(q);
                    n.extend(it);
                    n.push(e);
                    return;
                }
            }
        }
        if !self.sess.stream_notice(&e) {
            self.notices.borrow_mut().push(e);
        }
    }

    /// Whether a message of this severity reaches the client (client_min_messages; INFO
    /// always does).
    fn notice_wanted(&self, sev: crate::error::Severity) -> bool {
        use crate::error::Severity as S;
        let rank = match sev {
            S::Info => return true,
            S::Debug => 4,
            S::Log => 5,
            S::Notice => 6,
            S::Warning => 7,
            S::Error | S::Fatal => 8,
        };
        let st = self.sess.settings.borrow();
        let min_rank = match st.get_str("client_min_messages") {
            Some("debug5") => 0,
            Some("debug4") => 1,
            Some("debug3") => 2,
            Some("debug2") => 3,
            Some("debug1" | "debug") => 4,
            Some("log") => 5,
            Some("warning") => 7,
            Some("error") => 8,
            _ => 6,
        };
        rank >= min_rank
    }

    /// Move the NOTICEs input functions queued (they run without a context) into ours.
    pub fn take_input_notices(&self) {
        let lexed = crate::sql::scs::take_warnings();
        let lexed = lexed.into_iter().filter(|e| self.notice_wanted(e.severity));
        for e in crate::types::tsearch::take_notices().into_iter().chain(lexed) {
            let e = e.in_frames(&self.sess.frames.borrow());
            self.raise_notice(e);
        }
    }

    /// Cancellation and statement_timeout, checked every so often by long-running nodes.
    /// EXPLAIN ANALYZE: a scan's filter rejected a row.
    pub fn filtered(&self, key: usize) {
        if let Some(i) = &*self.instr.borrow() {
            i.update(key, |s| s.removed += 1);
        }
    }

    /// EXPLAIN ANALYZE: an index scan descends its index `n` times.
    pub fn searched(&self, key: usize, n: usize) {
        if let Some(i) = &*self.instr.borrow() {
            i.update(key, |s| s.searches += n as u64);
        }
    }

    /// EXPLAIN ANALYZE: update the counters of node `key` (one not run as a node itself,
    /// like a bitmap index scan).
    pub fn count(&self, key: usize, f: impl FnOnce(&mut NodeStats)) {
        if let Some(i) = &*self.instr.borrow() {
            i.update(key, f);
        }
    }

    /// Whether EXPLAIN ANALYZE is counting.
    pub fn instrumented(&self) -> bool {
        self.instr.borrow().is_some()
    }

    pub fn check(&self) -> Result<()> {
        let t = self.ticks.get().wrapping_add(1);
        self.ticks.set(t);
        if !t.is_multiple_of(256) {
            return Ok(());
        }
        self.txn.borrow().check_cancel()?;
        if let Some(d) = self.deadline
            && Instant::now() > d
        {
            return Err(Error::new(
                code::QUERY_CANCELED,
                "canceling statement due to statement timeout",
            ));
        }
        Ok(())
    }

    pub fn param(&self, i: usize) -> Result<Value> {
        self.params.get(i).cloned().ok_or_else(|| {
            Error::new(
                code::UNDEFINED_PARAMETER,
                format!("there is no parameter ${}", i + 1),
            )
        })
    }

    pub fn outer_value(&self, level: usize, i: usize) -> Result<Value> {
        let o = self.outer.borrow();
        let idx = o
            .len()
            .checked_sub(level)
            .ok_or_else(|| crate::error::internal("outer reference without an outer row"))?;
        Ok(o[idx].get(i).cloned().unwrap_or(Value::Null))
    }

    /// Run `f` with `row` as the innermost outer row (for subqueries and LATERAL).
    pub fn with_outer<T>(&self, row: Rc<Row>, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.outer.borrow_mut().push(row);
        let r = f();
        self.outer.borrow_mut().pop();
        r
    }

    pub fn with_case_value<T>(&self, v: Value, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.case_vals.borrow_mut().push(v);
        let r = f();
        self.case_vals.borrow_mut().pop();
        r
    }

    pub fn case_value(&self) -> Result<Value> {
        Ok(self
            .case_vals
            .borrow()
            .last()
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Cast errors keep their own message (Postgres reports the input function's error).
    pub fn cast_error(&self, e: Error, _from: &DataType, _to: &DataType) -> Error {
        e
    }

    /// Rows of a subquery: cached for uncorrelated ones.
    fn subquery_rows(
        &self,
        s: &Subquery,
        row: &[Value],
        limit: Option<usize>,
    ) -> Result<Rc<Vec<Row>>> {
        crate::stack::check()?;
        // An initplan reading a CTE, inside a correlated subquery: the CTE may read the
        // outer row, so it is redone with it (its chgParam).
        fn reads_cte(p: &crate::sql::plan::Plan) -> bool {
            use crate::sql::plan::Plan;
            match p {
                Plan::CteScan { .. } => true,
                Plan::With { ctes, body, .. } => {
                    ctes.iter().any(|(_, p)| reads_cte(p)) || reads_cte(body)
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
                | Plan::Barrier { input, .. } | Plan::Context { input, .. } => reads_cte(input),
                Plan::SetOp { left, right, .. } | Plan::Join { left, right, .. } => {
                    reads_cte(left) || reads_cte(right)
                }
                Plan::Append { inputs, .. } => inputs.iter().any(|p| reads_cte(p)),
                _ => false,
            }
        }
        if s.params.is_empty() && !self.outer.borrow().is_empty() && reads_cte(&s.plan) {
            return Ok(Rc::new(collect(&s.plan, self, limit)?));
        }
        // An initplan reading the enclosing subquery's values (a MIN / MAX over an index
        // inside a correlated subquery): redone for each of them (its chgParam).
        if s.params.is_empty() && !self.outer.borrow().is_empty() && reads_enclosing(&s.plan, 0) {
            return Ok(Rc::new(collect(&s.plan, self, limit)?));
        }
        if s.params.is_empty() {
            if let Some(r) = self.subq_cache.borrow().get(&s.id) {
                return Ok(r.clone());
            }
            let rows = Rc::new(collect(&s.plan, self, None)?);
            self.subq_cache.borrow_mut().insert(s.id, rows.clone());
            return Ok(rows);
        }
        let mut params = Vec::with_capacity(s.params.len());
        for p in &s.params {
            params.push(eval(p, row, self)?);
        }
        let rows = self.with_outer(Rc::new(params), || collect(&s.plan, self, limit))?;
        Ok(Rc::new(rows))
    }

    /// A correlated EXISTS whose correlation is equalities of plain parameters with
    /// columns of types `=` compares exactly: its column values, read once.
    fn exists_hashed(&self, s: &Subquery) -> Result<Option<Rc<ExistsHash>>> {
        if let Some(h) = self.exists_hash.borrow().get(&s.id) {
            return Ok(h.clone());
        }
        let mut made = None;
        if let Some((plan, keys)) = crate::sql::explain::exists_hashed(&s.plan) {
            let inner: Vec<DataType> = match &plan {
                crate::sql::plan::Plan::Project { exprs, .. } => exprs.iter().map(|e| e.ty()).collect(),
                _ => Vec::new(),
            };
            let params: Option<Vec<usize>> = keys
                .iter()
                .zip(&inner)
                .map(|((o, op), it)| match o {
                    crate::sql::expr::Expr::Outer(1, i, t)
                        if op.name == "="
                            && t.without_typmod() == it.without_typmod()
                            && crate::sql::binder::optimize::hashable_type(it)
                            && op.coll.as_ref().is_none_or(|c| c.deterministic) =>
                    {
                        Some(*i)
                    }
                    _ => None,
                })
                .collect();
            if let Some(params) = params.filter(|p| p.len() == keys.len()) {
                let rows = collect(&Arc::new(plan), self, None)?;
                let keys = rows
                    .into_iter()
                    .filter(|r| !r.iter().any(Value::is_null))
                    .collect();
                made = Some(Rc::new(ExistsHash { params, keys }));
            }
        }
        self.exists_hash.borrow_mut().insert(s.id, made.clone());
        Ok(made)
    }

    /// A correlated scalar subquery as a hash of its rows (see `ScalarHash`).
    fn scalar_hashed(&self, s: &Subquery) -> Result<Option<Rc<ScalarHash>>> {
        if let Some(h) = self.scalar_hash.borrow().get(&s.id) {
            return Ok(h.clone());
        }
        let mut made = None;
        if let Some((plan, keys, nkeys)) = crate::sql::explain::scalar_hashed(&s.plan) {
            let inner: Vec<DataType> = match &plan {
                crate::sql::plan::Plan::Project { exprs, .. } => exprs.iter().map(|e| e.ty()).collect(),
                _ => Vec::new(),
            };
            let params: Option<Vec<usize>> = keys
                .iter()
                .zip(&inner)
                .map(|((o, op), it)| match o {
                    crate::sql::expr::Expr::Outer(1, i, t)
                        if op.name == "="
                            && t.without_typmod() == it.without_typmod()
                            && crate::sql::binder::optimize::hashable_type(it)
                            && op.coll.as_ref().is_none_or(|c| c.deterministic) =>
                    {
                        Some(*i)
                    }
                    _ => None,
                })
                .collect();
            if let Some(params) = params.filter(|p| p.len() == nkeys) {
                let mut rows: HashMap<Vec<Value>, Vec<Row>> = HashMap::default();
                for r in collect(&Arc::new(plan), self, None)? {
                    if r[..nkeys].iter().any(Value::is_null) {
                        continue;
                    }
                    let (k, v) = r.split_at(nkeys);
                    rows.entry(k.to_vec()).or_default().push(v.to_vec());
                }
                made = Some(Rc::new(ScalarHash { params, rows }));
            }
        }
        self.scalar_hash.borrow_mut().insert(s.id, made.clone());
        Ok(made)
    }

    pub fn eval_subquery(&self, s: &Subquery, row: &[Value]) -> Result<Value> {
        crate::stack::check()?;
        match s.kind {
            SubqueryKind::Scalar => {
                let hashed = if !s.params.is_empty() && !self.instrumented() {
                    self.scalar_hashed(s)?
                } else {
                    None
                };
                let rows = match hashed {
                    Some(h) => {
                        let mut key = Vec::with_capacity(h.params.len());
                        let mut null = false;
                        for &i in &h.params {
                            let v = eval(&s.params[i], row, self)?;
                            null |= v.is_null();
                            key.push(v);
                        }
                        Rc::new(if null {
                            Vec::new()
                        } else {
                            h.rows.get(&key).cloned().unwrap_or_default()
                        })
                    }
                    None => {
                        let started = std::time::Instant::now();
                        let rows = self.subquery_rows(s, row, Some(2))?;
                        // pg_stat_statements: a SQL function's statement ran, one level down.
                        if let Some((text, vars)) = s.call.as_ref().and_then(|c| c.pgss.as_ref()) {
                            crate::ext::pg_stat_statements::nested(self.sess, text, vars, rows.len() as u64, started.elapsed());
                        }
                        rows
                    }
                };
                if rows.len() > 1 {
                    return Err(Error::new(
                        code::CARDINALITY_VIOLATION,
                        "more than one row returned by a subquery used as an expression",
                    ));
                }
                Ok(match rows.first() {
                    None => Value::Null,
                    Some(r) if r.len() == 1 => r[0].clone(),
                    Some(r) => Value::Record(Arc::new(r.clone())),
                })
            }
            SubqueryKind::Exists => {
                if !s.params.is_empty()
                    && !self.instrumented()
                    && let Some(h) = self.exists_hashed(s)?
                {
                    let mut key = Vec::with_capacity(h.params.len());
                    for &i in &h.params {
                        let v = eval(&s.params[i], row, self)?;
                        if v.is_null() {
                            return Ok(Value::Bool(false));
                        }
                        key.push(v);
                    }
                    return Ok(Value::Bool(h.keys.contains(&key)));
                }
                let rows = self.subquery_rows(s, row, Some(1))?;
                Ok(Value::Bool(!rows.is_empty()))
            }
            SubqueryKind::Array => {
                let rows = self.subquery_rows(s, row, None)?;
                let elem = s.ty.elem().cloned().unwrap_or(DataType::Text);
                let items: Vec<Value> = rows
                    .iter()
                    .map(|r| r.first().cloned().unwrap_or(Value::Null))
                    .collect();
                if items.iter().any(|v| matches!(v, Value::Array(_)))
                    && !elem.is_array()
                    && !matches!(elem, DataType::Int2Vector | DataType::OidVector)
                {
                    return crate::sql::expr::build_array(elem, items);
                }
                Ok(Value::Array(Arc::new(ArrayValue::from_vec(elem, items))))
            }
            SubqueryKind::Any | SubqueryKind::All | SubqueryKind::RowCompare => {
                let all = s.kind == SubqueryKind::All;
                // A row comparison's one row (NULL without one), compared as by ANY.
                let one = if s.kind == SubqueryKind::RowCompare {
                    let rows = self.subquery_rows(s, row, Some(2))?;
                    if rows.len() > 1 {
                        return Err(Error::new(
                            code::CARDINALITY_VIOLATION,
                            "more than one row returned by a subquery used as an expression",
                        ));
                    }
                    if rows.is_empty() {
                        return Ok(Value::Null);
                    }
                    Some(rows)
                } else {
                    None
                };
                let mut left = Vec::with_capacity(s.left.len());
                for l in &s.left {
                    left.push(eval(l, row, self)?);
                }
                let rows = match one {
                    Some(rows) => rows,
                    None => self.subquery_rows(s, row, None)?,
                };
                if s.left.len() == 1 {
                    let items: Vec<Value> = rows.iter().map(|r| r[0].clone()).collect();
                    return array_op(&left[0], &items, &s.ops[0], &s.left[0].ty(), all, self);
                }
                // Row comparison: (a, b) IN (SELECT x, y ...); `<>` is true where any column
                // differs, an ordering operator compares the rows column by column
                // (RowCompareExpr).
                let ordering = matches!(s.ops[0].name, "<" | "<=" | ">" | ">=");
                let differs = s.ops[0].name == "<>";
                let mut saw_null = false;
                for r in rows.iter() {
                    if ordering || differs {
                        let res = self.row_compare(s, &left, r, ordering)?;
                        match (res, all) {
                            (Some(true), false) => return Ok(Value::Bool(true)),
                            (Some(false), true) => return Ok(Value::Bool(false)),
                            (None, _) => saw_null = true,
                            _ => {}
                        }
                        continue;
                    }
                    let mut res = Some(true);
                    for (i, op) in s.ops.iter().enumerate() {
                        let (x, y) = (&left[i], &r[i]);
                        if x.is_null() || y.is_null() {
                            res = res.and(None);
                            if res.is_none() {
                                continue;
                            }
                        }
                        match crate::sql::func::call2(
                            op,
                            x.clone(),
                            y.clone(),
                            &s.left[i].ty(),
                            self,
                        )? {
                            Value::Bool(true) => {}
                            Value::Bool(false) => {
                                res = Some(false);
                                break;
                            }
                            _ => res = None,
                        }
                    }
                    match (res, all) {
                        (Some(true), false) => return Ok(Value::Bool(true)),
                        (Some(false), true) => return Ok(Value::Bool(false)),
                        (None, _) => saw_null = true,
                        _ => {}
                    }
                }
                Ok(if saw_null {
                    Value::Null
                } else {
                    Value::Bool(all)
                })
            }
        }
    }
}

/// Run a plan to completion (as a statement of its own when none runs in `ctx`).
pub fn collect(plan: &Arc<Plan>, ctx: &ExecCtx, limit: Option<usize>) -> Result<Vec<Row>> {
    ctx.statement(|| {
        let mut node = nodes::open(plan, ctx)?;
        let mut out = Vec::new();
        while let Some(r) = node.next(ctx)? {
            out.push(r);
            if limit.is_some_and(|l| out.len() >= l) {
                // A data-modifying query runs to completion all the same.
                if plan.writes() {
                    while node.next(ctx)?.is_some() {}
                }
                break;
            }
        }
        Ok(out)
    })
}

/// Run a query, handing each row to `f` as the executor returns it (a DestReceiver's
/// receiveSlot): the rows before an error have been handed over; an error of `f` ends it.
pub fn run_each(plan: &Arc<Plan>, ctx: &ExecCtx, mut f: impl FnMut(Row) -> Result<()>) -> Result<()> {
    ctx.statement(|| {
        let mut node = nodes::open(plan, ctx)?;
        while let Some(r) = node.next(ctx)? {
            f(r)?;
        }
        Ok(())
    })
}

impl ExecCtx<'_> {
    /// The snapshot table scans read through (None: the latest).
    pub fn snapshot(&self) -> Option<Rc<crate::txn::Snap>> {
        self.snap.borrow().clone()
    }

    /// The command id of the statement running in this context (its snapshot's), which
    /// its writes carry.
    pub fn command_id(&self) -> Option<u32> {
        self.snap.borrow().as_ref().map(|s| s.cid)
    }

    /// Run `f` as a statement (ExecutorStart .. ExecutorEnd): after a
    /// CommandCounterIncrement, with a snapshot of its own (or, in a non-volatile function,
    /// its caller's), so it sees what earlier commands wrote and nothing it writes itself.
    /// Within a statement already running in this context, `f` just runs.
    pub fn statement<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        if self.snap.borrow().is_some() {
            return f();
        }
        self.new_statement(f)
    }

    /// `statement`, even when one runs in this context already (a volatile SQL function's
    /// statements, run in its caller's context).
    pub fn new_statement<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let stable = self.stable_snap.borrow().clone();
        let snap = match stable {
            Some(s) => s,
            None => {
                // (A caller still reading the transaction: run as before, on the latest.)
                let Ok(mut txn) = self.txn.try_borrow_mut() else {
                    return f();
                };
                let cid = txn.next_cid();
                Rc::new(txn.snap_for(cid))
            }
        };
        self.with_snapshot(snap, f)
    }

    /// Run `f` reading through `snap`, its writes stamped with its command id.
    pub fn with_snapshot<T>(
        &self,
        snap: Rc<crate::txn::Snap>,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let cid = snap.cid;
        let Ok(outer_cid) = self.txn.try_borrow_mut().map(|mut t| t.set_write_cid(cid)) else {
            return f();
        };
        let outer = self.snap.replace(Some(snap));
        // A query of its own: its statement triggers and AFTER trigger events are its own.
        let fired = self.stmt_fired.take();
        let trans = self.trans_data.take();
        let queue = self.after_queue.take();
        let running = self.after_running.replace(false);
        let r = f();
        *self.stmt_fired.borrow_mut() = fired;
        *self.trans_data.borrow_mut() = trans;
        *self.after_queue.borrow_mut() = queue;
        self.after_running.set(running);
        if let Ok(mut t) = self.txn.try_borrow_mut() {
            t.set_write_cid(outer_cid);
        }
        *self.snap.borrow_mut() = outer;
        r
    }

    /// A snapshot for a new command (CommandCounterIncrement first).
    pub fn fresh_snapshot(&self) -> Rc<crate::txn::Snap> {
        let mut txn = self.txn.borrow_mut();
        let cid = txn.next_cid();
        Rc::new(txn.snap_for(cid))
    }

    /// A cursor's snapshot, taken at its OPEN for a command of its own (in a non-volatile
    /// function: the calling query's).
    pub fn cursor_snapshot(&self) -> Rc<crate::txn::Snap> {
        if let Some(s) = self.stable_snap.borrow().clone() {
            return s;
        }
        let mut txn = self.txn.borrow_mut();
        let cid = txn.next_cid();
        Rc::new(txn.snap_for(cid))
    }

    /// CommandCounterIncrement between the queries of a statement the rule system made
    /// several of: the next one sees what the earlier ones wrote.
    pub fn next_command(&self) {
        if self.snap.borrow().is_none() || self.stable_snap.borrow().is_some() {
            return;
        }
        let Ok(mut txn) = self.txn.try_borrow_mut() else {
            return;
        };
        let cid = txn.next_cid();
        txn.set_write_cid(cid);
        let snap = Rc::new(txn.snap_for(cid));
        drop(txn);
        *self.snap.borrow_mut() = Some(snap);
        self.stmt_fired.borrow_mut().clear();
        self.trans_data.borrow_mut().clear();
    }

    /// Run `f` (a function's body) with its statements using the calling query's snapshot
    /// (`stable`: a STABLE or IMMUTABLE function) or each their own (VOLATILE).
    pub fn in_function<T>(&self, stable: bool, f: impl FnOnce() -> T) -> T {
        let snap = if stable {
            self.snapshot().or_else(|| self.stable_snap.borrow().clone())
        } else {
            None
        };
        let outer = self.stable_snap.replace(snap);
        let r = f();
        *self.stable_snap.borrow_mut() = outer;
        r
    }
}

/// Evaluate a list of expressions over a row.
pub fn eval_all(exprs: &[Expr], row: &[Value], ctx: &ExecCtx) -> Result<Row> {
    let mut out = Vec::with_capacity(exprs.len());
    for e in exprs {
        out.push(eval(e, row, ctx)?);
    }
    Ok(out)
}

impl ExecCtx<'_> {
    /// `left op right` over rows: an ordering operator decides at the first column whose
    /// values aren't equal (NULL if a NULL comes first); `<>` is true if any column differs.
    fn row_compare(
        &self,
        s: &crate::sql::expr::Subquery,
        left: &[Value],
        right: &[Value],
        ordering: bool,
    ) -> Result<Option<bool>> {
        let n = s.ops.len();
        let call = |i: usize, x: &Value, y: &Value| -> Result<Option<bool>> {
            Ok(
                match crate::sql::func::call2(&s.ops[i], x.clone(), y.clone(), &s.left[i].ty(), self)? {
                    Value::Bool(b) => Some(b),
                    _ => None,
                },
            )
        };
        if !ordering {
            // `<>`: any column differing; NULL if none does but a NULL leaves it open.
            let mut unknown = false;
            for i in 0..n {
                match call(i, &left[i], &right[i])? {
                    Some(true) => return Ok(Some(true)),
                    Some(false) => {}
                    None => unknown = true,
                }
            }
            return Ok(if unknown { None } else { Some(false) });
        }
        for i in 0..n {
            let (x, y) = (&left[i], &right[i]);
            if x.is_null() || y.is_null() {
                return Ok(None);
            }
            if i + 1 == n {
                return call(i, x, y);
            }
            let (xy, yx) = (call(i, x, y)?, call(i, y, x)?);
            match (xy, yx) {
                (Some(a), Some(b)) if a == b => continue,
                (Some(a), Some(_)) => return Ok(Some(a)),
                _ => return Ok(None),
            }
        }
        Ok(None)
    }
}
