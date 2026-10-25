//! A session: one client's statements, transactions, prepared statements, portals and
//! cursors. The wire server drives it message by message; tests drive it directly.

pub mod copy;
pub mod current;
pub mod cursor;
pub(crate) mod explain_spi;
pub mod fastpath;
pub(crate) use utility::{bind_call, bind_call_typed};
pub mod ddl;
pub mod settings;
pub mod state;
mod pgss;
mod utility;
pub(crate) use utility::{LIST_QUOTE, split_guc_list};
pub(crate) use utility::set_value;
pub(crate) use utility::routine_set;

use crate::error::{Error, Result, code};
use crate::exec::{ExecCtx, Row, collect};
use crate::sql::binder::ast::inner;
use crate::sql::binder::{Binder, BoundQuery};
use crate::sql::plan::{OutCol, Plan};
use crate::txn::{Isolation, Txn};
use crate::types::datetime::Clock;
use crate::types::{DataType, Value};
use parking_lot::Mutex;
use pg_query::NodeEnum;
use pg_query::protobuf::{self as pb, Node};
use state::SessionState;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub use copy::{CopyIn, CopyOut};

/// Transaction status as ReadyForQuery reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnStatus {
    Idle,
    InBlock,
    Failed,
}

impl TxnStatus {
    pub fn byte(self) -> u8 {
        match self {
            TxnStatus::Idle => b'I',
            TxnStatus::InBlock => b'T',
            TxnStatus::Failed => b'E',
        }
    }
}

/// The result of one statement.
#[derive(Clone, Debug, Default)]
pub struct QueryResult {
    /// Some: the statement returns rows (and a RowDescription), possibly none.
    pub cols: Option<Vec<OutCol>>,
    pub rows: Vec<Row>,
    pub tag: String,
}

impl QueryResult {
    pub fn command(tag: impl Into<String>) -> Self {
        QueryResult {
            cols: None,
            rows: Vec::new(),
            tag: tag.into(),
        }
    }

    pub fn rows(cols: Vec<OutCol>, rows: Vec<Row>, tag: impl Into<String>) -> Self {
        QueryResult {
            cols: Some(cols),
            rows,
            tag: tag.into(),
        }
    }
}

/// What running a statement produced.
pub enum Outcome {
    Done(QueryResult),
    /// COPY ... FROM STDIN: the client sends data, then call `copy_in_finish`.
    CopyIn(CopyIn),
    /// COPY ... TO STDOUT: send the data, then the tag.
    CopyOut(CopyOut),
}

/// A parsed (and, for queries, bound) statement.
pub struct Prepared {
    pub sql: String,
    /// Where `sql` starts in the query string it came from, in bytes (a simple query's later
    /// statements): positions in the statement count from the start of that string.
    pub offset: usize,
    /// None: the empty query.
    pub stmt: Option<Node>,
    /// Declared or inferred parameter types.
    pub param_types: Vec<DataType>,
    /// Cached binding, valid for a catalog version, search_path, and the role and row_security
    /// setting (row-level security policies are bound into the plan).
    bound: Mutex<Option<(u64, Vec<String>, (u32, bool), Arc<BoundQuery>)>>,
    /// The plans it ran (plan cache choices, pg_prepared_statements).
    plans: Mutex<PlanUses>,
}

/// How often a prepared statement ran a generic plan and a custom one (bound with its
/// parameters' values as constants), and the result types of the last custom one.
#[derive(Default)]
struct PlanUses {
    generic: u64,
    custom: u64,
    cols: Option<Vec<DataType>>,
    /// The custom plans' costs, planning charged (total_custom_cost).
    custom_cost: f64,
    /// The generic plan's cost, and which binding it is of (generic_cost; None: not built).
    /// (A `Weak`, not an address: a later binding could take a dropped one's address.)
    generic_cost: Option<(std::sync::Weak<BoundQuery>, f64)>,
    /// The type oid a ParameterDescription gives a parameter in place of its value type's: a
    /// domain (named by the Parse message, or the type a cast gave it).
    param_oids: Vec<(usize, u32)>,
    /// The result row a Parse described (CompleteCachedPlan's fixed_result): a later binding
    /// (after DDL) must give the same names and types.
    fixed_result: Option<Vec<(String, u32, i32)>>,
}

fn result_row(cols: &[OutCol]) -> Vec<(String, u32, i32)> {
    cols.iter()
        .map(|c| (c.name.clone(), c.type_oid(), c.ty.typmod()))
        .collect()
}

impl Prepared {
    /// The parameters' type oids, as a ParameterDescription gives them.
    pub fn param_type_oids(&self) -> Vec<u32> {
        let over = &self.plans.lock().param_oids;
        (0..self.param_types.len())
            .map(|i| {
                over.iter()
                    .find(|(p, _)| *p == i)
                    .map_or_else(|| self.param_types[i].oid(), |(_, o)| *o)
            })
            .collect()
    }

    /// The statement's text placed where it is in its query string (blanks before it), so
    /// positions found in it count from the start of the string.
    pub fn placed_sql(&self) -> std::borrow::Cow<'_, str> {
        if self.offset == 0 {
            std::borrow::Cow::Borrowed(&self.sql)
        } else {
            std::borrow::Cow::Owned(format!("{}{}", " ".repeat(self.offset), self.sql))
        }
    }

    /// Whether the statement is a plain query the binder handles.
    pub fn is_query(&self) -> bool {
        matches!(
            self.stmt.as_ref().and_then(inner),
            Some(
                NodeEnum::SelectStmt(_)
                    | NodeEnum::InsertStmt(_)
                    | NodeEnum::UpdateStmt(_)
                    | NodeEnum::DeleteStmt(_)
                    | NodeEnum::MergeStmt(_)
            )
        )
    }
}

/// A bound portal (extended protocol), run on Execute; rows beyond `max_rows` wait here.
pub struct Portal {
    pub stmt: Arc<Prepared>,
    pub params: Vec<Value>,
    pub result_formats: Vec<i16>,
    pending: Option<VecDeque<Row>>,
    tag_verb: String,
    pub cols: Option<Vec<OutCol>>,
}

pub struct Session {
    pub state: SessionState,
    txn: Option<RefCell<Txn>>,
    status: TxnStatus,
    /// The open transaction was started implicitly (no BEGIN) and ends with the batch.
    implicit: bool,
    /// BEGIN / SET TRANSACTION options for a transaction not started yet.
    txn_opts: Option<(Isolation, bool)>,
    txn_time: i64,
    pub prepared: HashMap<String, Arc<Prepared>>,
    /// Named prepared statements: (statement text, prepare time, from SQL PREPARE).
    prepared_meta: HashMap<String, (String, i64, bool)>,
    pub portals: HashMap<String, Portal>,
    pub cancel: Arc<AtomicBool>,
    /// NOTICE / WARNING messages for the client.
    pub notices: Vec<Error>,
    /// LISTEN channels.
    /// Binding infers `$n` parameter types (Parse, PREPARE, EXPLAIN GENERIC_PLAN); otherwise a
    /// statement run without parameters can't reference any.
    infer_params: bool,
    /// Running a simple query of several statements (an implicit transaction block).
    multi_stmt: bool,
    /// An Execute completed since the last Sync (XACT_FLAGS_PIPELINING): what follows in
    /// the pipeline is in an implicit transaction block.
    pipelining: bool,
    /// The query string of the statements being run (pg_cursors / pg_prepared_statements
    /// show it whole).
    source: String,
    /// transaction_read_only as each open savepoint started (a subtransaction's change
    /// ends with it: CommitSubTransaction / AbortSubTransaction restore XactReadOnly).
    ro_stack: Vec<bool>,
    /// The rows a SELECT sent before it failed: Postgres streams a SELECT's rows as the
    /// executor makes them, so the client has these (and a chunked-mode psql, FETCH_COUNT,
    /// prints the full chunks) before the ErrorResponse.
    pub partial_rows: Option<QueryResult>,
    /// pg_stat_statements: the top-level statement running, and the parallel workers its
    /// plan used.
    pgss_tracked: Option<pgss::Tracked>,
    pgss_workers: i64,
    /// A COPY FROM STDIN waiting for its data, to be counted when it ends.
    pgss_copy: Option<pgss::Tracked>,
}

impl Session {
    /// Login checks (CONNECT, connection limit, VALID UNTIL) and the role's/database's default
    /// settings. `others` is how many other sessions the role has open.
    pub fn login(&self, dbname: &str, others: usize) -> Result<()> {
        // Sessions that just ended may still be dropping their temporary objects.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.state.db.temp_cleanups.load(Ordering::SeqCst) > 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let cat = self.state.db.catalog();
        let Some(role) = cat.role_by_name(&self.state.user) else {
            return Ok(());
        };
        if !role.superuser {
            let db_owner = self.state.db_owner(&cat);
            let oid = self
                .state
                .cluster
                .databases()
                .into_iter()
                .find(|d| d.1 == dbname)
                .map(|d| d.0);
            if let Some(oid) = oid {
                let (p, _) = cat.obj_mask(
                    crate::catalog::acl::ObjKind::Database,
                    oid,
                    role.oid,
                    db_owner,
                );
                if p & crate::catalog::acl::CONNECT == 0 {
                    return Err(Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        format!("permission denied for database \"{dbname}\""),
                    )
                    .detail("User does not have CONNECT privilege.")
                    .fatal());
                }
            }
            if role.conn_limit >= 0 && others >= role.conn_limit as usize {
                return Err(Error::new(
                    code::TOO_MANY_CONNECTIONS,
                    format!("too many connections for role \"{}\"", role.name),
                )
                .fatal());
            }
        }
        // Most specific last: global, database, role, role in database.
        let mut settings = self.state.settings.borrow_mut();
        for (r, d) in [
            (0, None),
            (0, Some(dbname)),
            (role.oid, None),
            (role.oid, Some(dbname)),
        ] {
            if let Some(e) = cat
                .role_settings
                .iter()
                .find(|e| e.role == r && e.database.as_deref() == d)
            {
                for (k, v) in &e.settings {
                    let _ = settings.set_default(k, v);
                }
            }
        }
        Ok(())
    }

    pub fn new(state: SessionState) -> Session {
        state.db.sessions.fetch_add(1, Ordering::SeqCst);
        Session {
            state,
            txn: None,
            status: TxnStatus::Idle,
            implicit: false,
            txn_opts: None,
            txn_time: 0,
            prepared: HashMap::new(),
            prepared_meta: HashMap::new(),
            portals: HashMap::new(),
            cancel: Arc::new(AtomicBool::new(false)),
            notices: Vec::new(),
            infer_params: false,
            multi_stmt: false,
            pipelining: false,
            ro_stack: Vec::new(),
            source: String::new(),
            partial_rows: None,
            pgss_tracked: None,
            pgss_workers: 0,
            pgss_copy: None,
        }
    }

    pub fn status(&self) -> TxnStatus {
        self.status
    }

    /// Whether a CALL or DO now must run atomically (no COMMIT / ROLLBACK inside): in a
    /// transaction block, explicit or of a multi-statement query.
    fn atomic(&self) -> bool {
        self.status != TxnStatus::Idle || self.multi_stmt || self.pipelining
    }

    /// PreventInTransactionBlock: in an explicit block, the implicit block of a
    /// multi-statement query, or an implicit transaction that wrote.
    fn in_block(&self) -> bool {
        self.status != TxnStatus::Idle
            || self.multi_stmt
            || self.pipelining
            || self.implicit && self.txn.as_ref().is_some_and(|t| t.borrow().has_writes())
    }

    /// An Execute message completed (exec_execute_message's XACT_FLAGS_PIPELINING).
    pub fn note_executed(&mut self) {
        self.pipelining = true;
    }

    /// Statements of one simple-query message run in one implicit transaction block.
    pub fn set_batch_len(&mut self, n: usize) {
        self.multi_stmt = n > 1;
    }

    /// Deliver notifications to this session's client through `sink` while it is idle.
    pub fn attach_notifications(&self, sink: crate::notify::Sink) {
        self.state.db.notify.attach(self.state.pid, sink);
    }

    /// Notifications received and not yet delivered.
    pub fn take_notifications(&self) -> Vec<crate::notify::Notification> {
        self.state.db.notify.take(self.state.pid)
    }

    /// The client is waiting: deliver notifications directly (outside a transaction block).
    pub fn notifications_idle(&self) {
        if self.status == TxnStatus::Idle {
            self.state.db.notify.idle(self.state.pid);
        }
    }

    /// Handling a client message: hold notifications.
    pub fn notifications_busy(&self) {
        self.state.db.notify.busy(self.state.pid);
    }

    pub(crate) fn now() -> i64 {
        Clock::system().now
    }

    fn default_txn_opts(&self) -> (Isolation, bool) {
        default_txn_opts(&self.state)
    }

    /// Make sure a transaction is open (implicitly, outside BEGIN).
    fn ensure_txn(&mut self) {
        if self.txn.is_some() {
            return;
        }
        if self.status == TxnStatus::Idle {
            self.implicit = true;
            self.txn_time = Self::now();
        }
        let (iso, ro) = self
            .txn_opts
            .take()
            .unwrap_or_else(|| self.default_txn_opts());
        self.cancel.store(false, Ordering::Relaxed);
        let mut txn = self.state.db.begin(iso, ro, self.cancel.clone());
        txn.lock_peer = Some(self.state.lock_owner);
        self.state.temp_used.set(false);
        let lt = self.state.settings.borrow().lock_timeout();
        if lt > 0 {
            txn.lock_timeout = Some(std::time::Duration::from_millis(lt as u64));
        }
        txn_settings(&self.state, iso, ro);
        if self.status == TxnStatus::Idle {
            default_deferrable(&self.state);
        }
        self.txn = Some(RefCell::new(txn));
    }

    /// End the current transaction.
    fn finish_txn(&mut self, commit: bool) -> Result<()> {
        if commit
            && self.txn.as_ref().is_some_and(|t| {
                !t.borrow().deferred.is_empty() || !t.borrow().deferred_triggers.is_empty()
            })
            && let Err(e) = self.with_ctx(&[], |ctx| {
                crate::exec::trigger::fire_deferred(ctx, None)?;
                crate::exec::dml::check_deferred(ctx, None)
            })
        {
            let _ = self.finish_txn(false);
            return Err(e);
        }
        // ON COMMIT DELETE ROWS / DROP of temporary tables (PreCommit_on_commit_actions).
        if commit
            && self.txn.is_some()
            && !self.state.on_commit.borrow().is_empty()
            && let Err(e) = self.run_on_commit_actions()
        {
            let _ = self.finish_txn(false);
            return Err(e);
        }
        // Holdable cursors run into a store before the transaction commits (failing, it
        // rolls back).
        if commit
            && self.txn.is_some()
            && crate::session::cursor::any_to_persist(&self.state, true)
            && let Err(e) =
                self.with_ctx(&[], |ctx| crate::session::cursor::persist_for_commit(ctx, true))
        {
            let _ = self.finish_txn(false);
            return Err(e);
        }
        let txn = self.txn.take();
        self.implicit = false;
        self.status = TxnStatus::Idle;
        self.txn_opts = None;
        // Those the transaction declared go with it when it rolls back.
        self.state
            .cursors
            .borrow_mut()
            .retain(|_, c| c.hold && (commit || c.lazy.is_none()));
        let mut stats = crate::exec::pgstat::XactStats::default();
        let had_txn = txn.is_some();
        let r = match txn {
            Some(t) if commit => {
                let mut t = t.into_inner();
                let notes = std::mem::take(&mut t.notifies);
                stats = std::mem::take(&mut t.pgstat);
                let r = self.state.cluster.commit(t).map(|_| ());
                committed_notify(&self.state, r.is_ok(), &notes);
                r
            }
            Some(t) => {
                let mut t = t.into_inner();
                stats = std::mem::take(&mut t.pgstat);
                for (name, owner) in std::mem::take(&mut t.db_owner_undo).into_iter().rev() {
                    let _ = self.state.cluster.set_database_owner(&name, &owner);
                }
                t.rollback();
                committed_notify(&self.state, false, &[]);
                Ok(())
            }
            None => {
                committed_notify(&self.state, false, &[]);
                Ok(())
            }
        };
        if had_txn {
            crate::exec::pgstat::end_xact(
                &self.state,
                stats,
                commit && r.is_ok(),
                &self.state.db.catalog(),
            );
        }
        self.state
            .settings
            .borrow_mut()
            .end_transaction(commit && r.is_ok());
        self.state.db.touch();
        r
    }

    /// Empty or drop the temporary tables registered with ON COMMIT actions
    /// (PreCommit_on_commit_actions): all the DELETE ROWS ones at once, then the DROP ones,
    /// cascading and quietly.
    fn run_on_commit_actions(&mut self) -> Result<()> {
        let actions = self.state.on_commit.borrow().clone();
        let cat = self.txn.as_ref().unwrap().borrow().catalog();
        let name = |t: &crate::catalog::Table| {
            format!(
                "{}.{}",
                crate::catalog::quote_ident(cat.schema_name(t.schema)),
                crate::catalog::quote_ident(&t.name)
            )
        };
        let live: Vec<(u32, bool)> = actions
            .into_iter()
            .filter(|(oid, _)| cat.tables.contains_key(oid))
            .collect();
        // (Each table's own rows only; partitioned ones have none.)
        let truncate: Vec<String> = live
            .iter()
            .filter(|(_, drop)| !drop)
            .filter_map(|(oid, _)| cat.tables.get(oid))
            .filter(|t| t.partkey.is_none())
            .map(|t| format!("ONLY {}", name(t)))
            .collect();
        let drop: Vec<String> = live
            .iter()
            .filter(|(_, drop)| *drop)
            .filter_map(|(oid, _)| cat.tables.get(oid).map(|t| name(t)))
            .collect();
        *self.state.on_commit.borrow_mut() = live.into_iter().filter(|(_, d)| !d).collect();
        let mut stmts = Vec::new();
        if !truncate.is_empty() {
            stmts.push(format!("TRUNCATE TABLE {}", truncate.join(", ")));
        }
        if !drop.is_empty() {
            stmts.push(format!("DROP TABLE IF EXISTS {} CASCADE", drop.join(", ")));
        }
        for sql in stmts {
            let parsed = crate::sql::parse::parse(&sql)?;
            let Some(node) = parsed.first().and_then(|s| s.stmt.as_deref()).and_then(inner) else {
                continue;
            };
            let node = node.clone();
            // The server does this, whoever the current user is, without notices.
            self.state
                .definer
                .borrow_mut()
                .push(crate::catalog::BOOTSTRAP_SUPERUSER_OID);
            let before = self.notices.len();
            self.state.on_commit_truncating.set(true);
            let r = self.with_ctx(&[], |ctx| ddl::execute(ctx, &node, "").map(|_| ()));
            self.state.on_commit_truncating.set(false);
            self.notices.truncate(before);
            self.state.definer.borrow_mut().pop();
            r.map_err(|e| {
                if e.message == "cannot truncate a table referenced in a foreign key constraint" {
                    let detail = e
                        .detail
                        .as_deref()
                        .map(|d| d.trim_end_matches('.').to_string())
                        .unwrap_or_default();
                    Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "unsupported ON COMMIT and foreign key combination",
                    )
                    .detail(format!(
                        "{detail}, but they do not have the same ON COMMIT setting."
                    ))
                } else {
                    e
                }
            })?;
        }
        Ok(())
    }

    /// After an engine panic caught around this session's work (the server's `guard`): the
    /// statement failed as after an error, and the session goes on.
    pub fn recover_from_panic(&mut self) {
        self.state.clear_running();
        self.set_batch_len(0);
        self.on_error();
        crate::scoped::reset_statement_scratch();
    }

    /// After an error: an explicit block fails; an implicit transaction rolls back.
    pub(crate) fn on_error(&mut self) {
        let keep = self.state.commit_on_error.replace(false);
        if self.status == TxnStatus::InBlock || self.status == TxnStatus::Failed {
            self.status = TxnStatus::Failed;
            if let Some(t) = &self.txn
                && let Ok(mut t) = t.try_borrow_mut()
            {
                t.release_aborted_locks();
            }
        } else {
            let _ = self.finish_txn(keep);
        }
    }

    /// End of a simple-query batch or Sync: commit an implicit transaction.
    /// Login event triggers (EventTriggerOnLogin): run in a transaction of their own as the
    /// session starts, when the database has any.
    pub fn run_login_triggers(&mut self) -> Result<()> {
        if self.state.db.catalog().event_triggers.is_empty() {
            return Ok(());
        }
        self.ensure_txn();
        let r = self.with_ctx(&[], ddl::event_trigger::login);
        match r {
            Ok(()) => self.finish_txn(true),
            Err(e) => {
                let _ = self.finish_txn(false);
                Err(e)
            }
        }
    }

    pub fn end_batch(&mut self) -> Result<()> {
        if self.implicit && self.status == TxnStatus::Idle {
            self.finish_txn(true)?;
        }
        Ok(())
    }

    /// Run `f` with an execution context over the current transaction.
    fn with_ctx<T>(
        &mut self,
        params: &[Value],
        f: impl FnOnce(&ExecCtx) -> Result<T>,
    ) -> Result<T> {
        self.with_ctx_path(params, None, f)
    }

    /// `with_ctx`, given the session's search path when the caller has it at hand
    /// (`SessionState::search_path`, after `ensure_txn`).
    fn with_ctx_path<T>(
        &mut self,
        params: &[Value],
        search_path: Option<Vec<String>>,
        f: impl FnOnce(&ExecCtx) -> Result<T>,
    ) -> Result<T> {
        self.ensure_txn();
        self.sync_lex();
        crate::exec::pgstat::busy(&self.state);
        // (What a panic left half-set in an earlier statement goes.)
        crate::scoped::reset_statement_scratch();
        let txn = self.txn.as_ref().unwrap();
        crate::sql::binder::note_catalog(&txn.borrow().catalog());
        {
            let mut t = txn.borrow_mut();
            if t.isolation == crate::txn::Isolation::Serializable && t.read_only {
                t.deferrable = self.state.settings.borrow().bool("transaction_deferrable");
            }
            t.begin_statement(!self.state.no_snapshot.get())?;
        }
        let search_path = search_path.unwrap_or_else(|| self.state.search_path());
        let ctx = ExecCtx::with_search_path(
            txn,
            &self.state,
            params,
            self.txn_time,
            Self::now(),
            search_path,
        );
        let entered = current::enter(&ctx);
        let r = f(&ctx);
        drop(entered);
        ctx.take_input_notices();
        self.notices.extend(ctx.notices.borrow_mut().drain(..));
        r
    }

    // ── simple query ──

    /// Make the scanner read strings under this session's settings.
    pub(crate) fn sync_lex(&self) {
        crate::sql::scs::set_opts(self.state.settings.borrow().lex_opts());
    }

    /// Statement text as the client wrote it (its literals as sent, not made standard).
    /// The whole query string statement `p` came in (its own text when that's unknown).
    fn whole_query(&self, p: &Prepared) -> String {
        if self.source.get(p.offset..).is_some_and(|rest| rest.starts_with(p.sql.as_str())) {
            self.source.clone()
        } else {
            self.source_text(&p.sql, p.offset)
        }
    }

    fn source_text(&self, sql: &str, offset: usize) -> String {
        crate::sql::scs::source_of(sql, Some(offset)).unwrap_or_else(|| sql.to_string())
    }

    /// Scanner warnings from parsing a client's query.
    fn take_lex_warnings(&mut self) {
        let ws = crate::sql::scs::take_warnings();
        if !ws.is_empty() {
            let st = self.state.settings.borrow();
            self.notices
                .extend(ws.into_iter().filter(|w| settings::notice_wanted(&st, w.severity)));
        }
    }

    /// Parse a query string into statements.
    pub fn parse(&mut self, sql: &str) -> Result<Vec<Arc<Prepared>>> {
        // (`pg_temp` is this session's while it binds, on a thread another session may
        // have run on: an embedder's Parse / Describe before any execute.)
        crate::catalog::set_session_temp_schema(&self.state.temp_schema());
        self.sync_lex();
        let parsed = crate::sql::parse::parse_live(sql);
        self.take_lex_warnings();
        let (text, stmts) = parsed?;
        self.source = sql.to_string();
        crate::sql::scs::set_source(match &text {
            std::borrow::Cow::Owned(t) => Some((t.clone(), sql.to_string())),
            _ => None,
        });
        let sql = &*text;
        Ok(stmts
            .into_iter()
            .map(|s| {
                Arc::new(Prepared {
                    sql: crate::sql::parse::stmt_text(sql, &s).to_string(),
                    offset: crate::sql::parse::stmt_offset(sql, &s),
                    stmt: s.stmt.map(|n| {
                        crate::sql::binder::ast::select_into_as_ctas(&n).unwrap_or(*n)
                    }),
                    param_types: Vec::new(),
                    bound: Mutex::new(None),
                    plans: Default::default(),
                })
            })
            .collect())
    }

    /// Refresh what pg_prepared_statements / pg_cursors show.
    fn sync_lists(&self) {
        let mut pl = self.state.prepared_list.borrow_mut();
        pl.clear();
        for (name, p) in &self.prepared {
            if name.is_empty() {
                continue;
            }
            let (text, at, from_sql) = self
                .prepared_meta
                .get(name)
                .cloned()
                .unwrap_or_else(|| (p.sql.clone(), 0, true));
            let plans = p.plans.lock();
            let result = p
                .bound
                .lock()
                .as_ref()
                .map(|b| b.3.cols.iter().map(|c| c.ty.clone()).collect())
                .or_else(|| plans.cols.clone());
            pl.push((
                name.clone(),
                text,
                at,
                p.param_types.clone(),
                result,
                from_sql,
                plans.generic,
                plans.custom,
            ));
        }
        // (A stable order: by name.)
        pl.sort_by(|a, b| a.0.cmp(&b.0));
    }

    /// The catalog this session sees: its open transaction's, else the database's.
    fn visible_catalog(&self) -> Arc<crate::catalog::Catalog> {
        match &self.txn {
            Some(t) => t.borrow().catalog(),
            None => self.state.db.catalog(),
        }
    }

    /// How values are rendered as text for this session (settings plus catalog names).
    pub fn format_ctx(&self) -> crate::types::text::FormatCtx {
        let mut f = self.state.settings.borrow().format_ctx();
        let cat = self.visible_catalog();
        f.names = Some(Arc::new(crate::catalog::names::CatalogNames::new(
            cat,
            self.state.search_path(),
        )));
        f
    }

    /// Run one statement of a simple query.
    pub fn execute(&mut self, p: &Arc<Prepared>) -> Result<Outcome> {
        self.execute_with(p, &[], 0)
    }

    /// Convenience for tests and embedding: run a query string, stopping at the first error.
    pub fn query(&mut self, sql: &str) -> Result<Vec<QueryResult>> {
        self.query_copy(sql, &[])
    }

    /// `query`, with `copy_data` as the input of a COPY FROM STDIN.
    pub fn query_copy(&mut self, sql: &str, copy_data: &[u8]) -> Result<Vec<QueryResult>> {
        let stmts = match self.parse(sql) {
            Ok(s) => s,
            Err(e) => {
                self.on_error();
                return Err(e.in_chars(sql));
            }
        };
        let mut out = Vec::new();
        self.set_batch_len(stmts.len());
        let r = self.run_batch(&stmts, copy_data, &mut out, sql);
        self.set_batch_len(0);
        r?;
        Ok(out)
    }

    fn run_batch(
        &mut self,
        stmts: &[Arc<Prepared>],
        copy_data: &[u8],
        out: &mut Vec<QueryResult>,
        sql: &str,
    ) -> Result<()> {
        for s in stmts {
            match self.execute(s) {
                Ok(Outcome::Done(r)) => out.push(r),
                Ok(Outcome::CopyIn(c)) => match self.copy_in_finish(c, copy_data) {
                    Ok(r) => out.push(r),
                    Err(e) => {
                        let _ = self.end_batch();
                        return Err(e);
                    }
                },
                // (A row the query or its conversion failed on: the statement's error.)
                Ok(Outcome::CopyOut(c)) if c.error.is_some() => {
                    self.on_error();
                    let _ = self.end_batch();
                    return Err(c.error.unwrap().in_chars(sql));
                }
                Ok(Outcome::CopyOut(c)) => {
                    let mut r = QueryResult::command(c.tag.clone());
                    r.rows = c
                        .data
                        .iter()
                        .map(|d| vec![Value::Text(String::from_utf8_lossy(d).into())])
                        .collect();
                    out.push(r);
                    self.notices.extend(c.trailing);
                }
                Err(e) => {
                    let _ = self.end_batch();
                    return Err(e.in_chars(sql));
                }
            }
        }
        self.end_batch()
    }

    /// Run a statement with parameters; `max_rows` is ignored here (portals handle it).
    pub(crate) fn execute_with(
        &mut self,
        p: &Arc<Prepared>,
        params: &[Value],
        max_rows: usize,
    ) -> Result<Outcome> {
        let before = self.notices.len();
        crate::catalog::set_session_temp_schema(&self.state.temp_schema());
        // pg_stat_statements (when preloaded): what runs inside is one level down.
        let tracked = self.pgss_begin(p);
        let mine = tracked.is_some();
        if mine {
            self.pgss_tracked = tracked;
            self.pgss_workers = 0;
        }
        let counted = self.state.cluster.statements().is_some();
        if counted {
            self.state.pgss.borrow_mut().level += 1;
        }
        let r = self.execute_with_inner(p, params, max_rows);
        if counted {
            self.state.pgss.borrow_mut().level -= 1;
        }
        if mine && let Some(t) = self.pgss_tracked.take() {
            self.pgss_end(t, &r);
        }
        // Every message the statement raised goes through client_min_messages.
        if self.notices.len() > before {
            let st = self.state.settings.borrow();
            let mut i = 0;
            self.notices.retain(|n| {
                i += 1;
                i <= before || settings::notice_wanted(&st, n.severity)
            });
        }
        r
    }

    fn execute_with_inner(
        &mut self,
        p: &Arc<Prepared>,
        params: &[Value],
        _max_rows: usize,
    ) -> Result<Outcome> {
        crate::stack::mark();
        // (The caller holds `p`: its statement is borrowed, not copied.)
        let p = p.clone();
        let Some(stmt) = p.stmt.as_ref() else {
            return Ok(Outcome::Done(QueryResult::command("")));
        };
        let node = inner(stmt);
        // A failed transaction accepts only its end.
        if self.status == TxnStatus::Failed && !ends_failed_txn(node) {
            return Err(Error::new(
                code::IN_FAILED_SQL_TRANSACTION,
                "current transaction is aborted, commands ignored until end of transaction block",
            ));
        }
        self.state.db.touch();
        self.sync_lists();
        let r = self.dispatch(&p, stmt, node, params);
        self.state.no_snapshot.set(false);
        if r.is_err() {
            self.on_error();
        }
        r
    }

    fn dispatch(
        &mut self,
        p: &Arc<Prepared>,
        stmt: &Node,
        node: Option<&NodeEnum>,
        params: &[Value],
    ) -> Result<Outcome> {
        let Some(node) = node else {
            return Ok(Outcome::Done(QueryResult::command("")));
        };
        let done = |r: QueryResult| Ok(Outcome::Done(r));
        self.state.no_snapshot.set(matches!(
            node,
            NodeEnum::TransactionStmt(_)
                | NodeEnum::LockStmt(_)
                | NodeEnum::VariableSetStmt(_)
                | NodeEnum::VariableShowStmt(_)
                | NodeEnum::ConstraintsSetStmt(_)
                | NodeEnum::FetchStmt(_)
                | NodeEnum::ListenStmt(_)
                | NodeEnum::NotifyStmt(_)
                | NodeEnum::UnlistenStmt(_)
                | NodeEnum::CheckPointStmt(_)
        ));
        // DETACH PARTITION CONCURRENTLY commits on its own.
        if let NodeEnum::AlterTableStmt(a) = node
            && a.cmds.iter().any(|c| matches!(inner(c), Some(NodeEnum::AlterTableCmd(c)) if matches!(c.def.as_deref().and_then(inner), Some(NodeEnum::PartitionCmd(pc)) if pc.concurrent)))
            && self.in_block()
        {
            return Err(Error::new(code::ACTIVE_SQL_TRANSACTION, "ALTER TABLE ... DETACH CONCURRENTLY cannot run inside a transaction block"));
        }
        if let Some(cmd) = ddl::write_command(node) {
            self.ensure_txn();
            if self.txn.as_ref().is_some_and(|t| t.borrow().read_only) && !self.writes_temp_only(node)
            {
                return Err(Error::new(
                    code::READ_ONLY_SQL_TRANSACTION,
                    format!("cannot execute {cmd} in a read-only transaction"),
                ));
            }
        }
        match node {
            // SELECT ... INTO t is CREATE TABLE t AS SELECT ...
            NodeEnum::SelectStmt(sel) if crate::sql::binder::ast::select_into(sel).is_some() => {
                let ctas = crate::sql::binder::ast::select_into_as_ctas(stmt)
                    .and_then(|n| n.node)
                    .unwrap();
                let tag = self.with_ctx(params, |ctx| ddl::execute(ctx, &ctas, ""))?;
                done(QueryResult::command(tag))
            }
            // CREATE TABLE ... AS EXECUTE: the prepared query, over the EXECUTE's values.
            NodeEnum::CreateTableAsStmt(c)
                if matches!(
                    c.query.as_deref().and_then(inner),
                    Some(NodeEnum::ExecuteStmt(_))
                ) =>
            {
                let Some(NodeEnum::ExecuteStmt(es)) = c.query.as_deref().and_then(inner) else {
                    unreachable!()
                };
                let (prep, values) = self.prepared_args(es)?;
                if !prep.is_query()
                    || !matches!(prep.stmt.as_ref().and_then(inner), Some(NodeEnum::SelectStmt(_)))
                {
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        "prepared statement is not a SELECT",
                    ));
                }
                let mut ctas = (**c).clone();
                ctas.query = prep.stmt.clone().map(Box::new);
                let node = NodeEnum::CreateTableAsStmt(Box::new(ctas));
                let types = prep.param_types.clone();
                let tag = self.with_ctx(&values, |ctx| {
                    ddl::execute_with_params(ctx, &node, &types)
                })?;
                // The prepared query runs (with no rows WITH NO DATA): a SELECT's tag.
                let skip = c.into.as_ref().is_some_and(|i| i.skip_data);
                done(QueryResult::command(if skip && tag == "CREATE TABLE AS" {
                    "SELECT 0".to_string()
                } else {
                    tag
                }))
            }
            NodeEnum::UpdateStmt(u) if ddl::is_pg_class_update(u) => {
                let tag = self.with_ctx(params, |ctx| ddl::execute(ctx, node, ""))?;
                done(QueryResult::command(tag))
            }
            NodeEnum::DeleteStmt(d) if ddl::is_pg_depend_delete(d) => {
                let (cols, rows, tag) = self.with_ctx(params, |ctx| ddl::pg_depend_dml(ctx, node))?;
                done(if cols.is_empty() {
                    QueryResult::command(tag)
                } else {
                    QueryResult::rows(cols, rows, tag)
                })
            }
            NodeEnum::MergeStmt(m) if ddl::might_merge_pg_class(m) => {
                if let Some((cols, rows, tag)) =
                    self.with_ctx(params, |ctx| ddl::merge_pg_class(ctx, m))?
                {
                    return done(if cols.is_empty() {
                        QueryResult::command(tag)
                    } else {
                        QueryResult::rows(cols, rows, tag)
                    });
                }
                done(self.run_query(p, stmt, params)?)
            }
            NodeEnum::InsertStmt(i) if ddl::is_pg_depend_insert(i) => {
                let (_, _, tag) = self.with_ctx(params, |ctx| ddl::pg_depend_dml(ctx, node))?;
                done(QueryResult::command(tag))
            }
            NodeEnum::UpdateStmt(u) if ddl::is_pg_depend_update(u) => {
                let (_, _, tag) = self.with_ctx(params, |ctx| ddl::pg_depend_dml(ctx, node))?;
                done(QueryResult::command(tag))
            }
            NodeEnum::SelectStmt(_)
            | NodeEnum::InsertStmt(_)
            | NodeEnum::UpdateStmt(_)
            | NodeEnum::DeleteStmt(_)
            | NodeEnum::MergeStmt(_) => done(self.run_query(p, stmt, params)?),
            NodeEnum::TransactionStmt(t) => done(self.transaction_stmt(t)?),
            NodeEnum::ConstraintsSetStmt(s) => done(self.set_constraints(s)?),
            NodeEnum::VariableSetStmt(s) => done(self.set_stmt(s)?),
            NodeEnum::VariableShowStmt(s) => done(self.show_stmt(&s.name)?),
            NodeEnum::PrepareStmt(s) => {
                let text = self.whole_query(p);
                done(self.prepare_stmt(s, &text)?)
            }
            NodeEnum::ExecuteStmt(s) => self.execute_stmt(s),
            NodeEnum::DeallocateStmt(s) => done(self.deallocate_stmt(s)?),
            NodeEnum::ExplainStmt(s) => done(self.explain_stmt(s, params)?),
            NodeEnum::DeclareCursorStmt(s) => {
                let text = self.whole_query(p);
                done(self.declare_cursor(s, params, &text)?)
            }
            NodeEnum::FetchStmt(s) => done(self.fetch_stmt(s)?),
            NodeEnum::ClosePortalStmt(s) => done(self.close_cursor(s)?),
            NodeEnum::DiscardStmt(s) => done(self.discard_stmt(s)?),
            NodeEnum::LoadStmt(s) => done(self.load_stmt(s)?),
            NodeEnum::CopyStmt(s) => self.copy_stmt(s, params),
            NodeEnum::CheckPointStmt(_) => {
                self.state.db.store.flush_all()?;
                crate::exec::pgstat::report_checkpoint(&self.state.db);
                done(QueryResult::command("CHECKPOINT"))
            }
            NodeEnum::LockStmt(_) if self.status == TxnStatus::Idle && !self.implicit => {
                Err(Error::new(
                    code::NO_ACTIVE_SQL_TRANSACTION,
                    "LOCK TABLE can only be used in transaction blocks",
                ))
            }
            NodeEnum::ClusterStmt(c) => {
                // CLUSTER of every table, or of a partitioned one, commits as it goes.
                let partitioned = c.relation.as_ref().is_some_and(|rv| {
                    self.ensure_txn();
                    let cat = self.txn.as_ref().unwrap().borrow().catalog();
                    let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
                    matches!(cat.lookup(schema, &rv.relname, &self.state.search_path()), Ok(Some(crate::catalog::Relation::Table(t))) if t.partkey.is_some())
                });
                if (c.relation.is_none() || partitioned) && self.in_block() {
                    return Err(Error::new(
                        code::ACTIVE_SQL_TRANSACTION,
                        "CLUSTER cannot run inside a transaction block",
                    ));
                }
                let tag = self.with_ctx(params, |ctx| ddl::execute(ctx, node, &p.placed_sql()))?;
                done(QueryResult::command(tag))
            }
            NodeEnum::VacuumStmt(v) => {
                if v.is_vacuumcmd && self.in_block() {
                    return Err(Error::new(
                        code::ACTIVE_SQL_TRANSACTION,
                        "VACUUM cannot run inside a transaction block",
                    ));
                }
                // (Top level: VACUUM and ANALYZE outside a block take a transaction per
                // relation, their locks with it.)
                let in_block = self.in_block();
                let tag = ddl::run_top_level(in_block, || {
                    self.with_ctx(params, |ctx| ddl::execute(ctx, node, &p.placed_sql()))
                })?;
                done(QueryResult::command(tag))
            }
            NodeEnum::CreatedbStmt(_) | NodeEnum::DropdbStmt(_) => {
                if self.status != TxnStatus::Idle
                    || self.implicit && self.txn.as_ref().is_some_and(|t| t.borrow().has_writes())
                {
                    let what = if matches!(node, NodeEnum::CreatedbStmt(_)) {
                        "CREATE DATABASE"
                    } else {
                        "DROP DATABASE"
                    };
                    return Err(Error::new(
                        code::ACTIVE_SQL_TRANSACTION,
                        format!("{what} cannot run inside a transaction block"),
                    ));
                }
                done(self.database_stmt(node)?)
            }
            NodeEnum::DoStmt(d) => {
                let body = crate::plpgsql::do_body(d)?;
                let sql = p.placed_sql();
                let atomic = self.atomic();
                self.with_ctx(params, |ctx| {
                    crate::plpgsql::check_do(ctx, &body, &sql)?;
                    crate::plpgsql::run_do(ctx, &body, atomic)
                })?;
                done(QueryResult::command("DO"))
            }
            NodeEnum::CallStmt(c) => done(self.call_stmt(c, params, &p.param_types)?),
            _ => {
                let in_block = self.in_block();
                let tag = ddl::run_top_level(in_block, || {
                    self.with_ctx(params, |ctx| ddl::execute(ctx, node, &p.placed_sql()))
                })?;
                done(QueryResult::command(tag))
            }
        }
    }

    // ── queries ──

    /// Bind (or reuse the cached binding of) a query statement.
    fn bound(&mut self, p: &Prepared, stmt: &Node, params: &[Value]) -> Result<Arc<BoundQuery>> {
        self.ensure_txn();
        let version = self
            .txn
            .as_ref()
            .map(|t| t.borrow().catalog().version)
            .unwrap_or(0);
        let path = self.state.search_path();
        let who = {
            let cat = self.txn.as_ref().unwrap().borrow().catalog();
            (
                self.state.current_role(&cat),
                self.state
                    .settings
                    .borrow()
                    .get_str("row_security")
                    != Some("off"),
            )
        };
        if let Some((v, sp, w, b)) = &*p.bound.lock()
            && *v == version
            && *sp == path
            && *w == who
        {
            return Ok(b.clone());
        }
        let types = p.param_types.clone();
        let no_params = !self.infer_params && params.is_empty() && types.is_empty();
        let b = self.with_ctx_path(params, Some(path.clone()), |ctx| {
            let mut b = Binder::new(ctx, types);
            b.no_params = no_params;
            b.bind_statement(stmt)
        })?;
        if let Some(fixed) = &p.plans.lock().fixed_result
            && *fixed != result_row(&b.cols)
        {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "cached plan must not change result type",
            )
            .routine("RevalidateCachedQuery"));
        }
        let b = Arc::new(b);
        *p.bound.lock() = Some((version, path, who, b.clone()));
        Ok(b)
    }

    /// A statement being prepared (PREPARE, a Parse message) whose planned binding failed
    /// with `planned`: Postgres only analyzes it then, so what fails only when it's planned
    /// or starts (folding a constant call, a merge join without a collation, inlining a SQL
    /// function) waits for EXECUTE. The parameter types its analysis infers, or the error.
    pub(super) fn analyzed_params(
        &mut self,
        stmt: &Node,
        types: &[DataType],
        planned: Error,
    ) -> Result<Vec<DataType>> {
        let inlining = |e: &Error| {
            e.context
                .as_deref()
                .is_some_and(|c| c.lines().any(|l| l.ends_with("during inlining")))
        };
        let types = types.to_vec();
        let analyzed = self.with_ctx(&[], |ctx| {
            let mut b = Binder::new(ctx, types.clone());
            b.unplanned = true;
            b.bind_statement(stmt)
        });
        match analyzed {
            Ok(b) => Ok(b.params.clone()),
            Err(e) if inlining(&e) && inlining(&planned) => Ok(types),
            Err(_) => Err(planned),
        }
    }

    /// A custom plan: bound afresh with the parameters' values as constants.
    fn bound_custom(
        &mut self,
        p: &Prepared,
        stmt: &Node,
        params: &[Value],
    ) -> Result<Arc<BoundQuery>> {
        self.ensure_txn();
        let types = p.param_types.clone();
        let b = self.with_ctx(params, |ctx| {
            let mut b = Binder::new(ctx, types);
            b.param_consts = true;
            b.bind_statement(stmt)
        })?;
        if let Some(fixed) = &p.plans.lock().fixed_result
            && *fixed != result_row(&b.cols)
        {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "cached plan must not change result type",
            )
            .routine("RevalidateCachedQuery"));
        }
        Ok(Arc::new(b))
    }

    /// Whether running `p` with `params` takes a custom plan (choose_custom_plan): with
    /// parameter values, the first five times, unless plan_cache_mode decides (None: it's up
    /// to the costs).
    fn custom_plan(&self, p: &Prepared, params: &[Value]) -> Option<bool> {
        if params.is_empty() || p.param_types.is_empty() {
            return Some(false);
        }
        match self
            .state
            .settings
            .borrow()
            .get_str("plan_cache_mode")
        {
            Some("force_generic_plan") => Some(false),
            Some("force_custom_plan") => Some(true),
            _ if p.plans.lock().custom < 5 => Some(true),
            _ => None,
        }
    }

    /// The plan to run `p` with (GetCachedPlan): a custom or the generic one, counted. After
    /// five custom plans the generic one is used when it costs less than their average (whose
    /// costs include a charge for planning), its cost found when it is (re)built.
    fn planned(
        &mut self,
        p: &Prepared,
        stmt: &Node,
        params: &[Value],
    ) -> Result<(Arc<BoundQuery>, bool)> {
        let custom = match self.custom_plan(p, params) {
            Some(c) => c,
            None => {
                let bq = self.bound(p, stmt, params)?;
                let known = p
                    .plans
                    .lock()
                    .generic_cost
                    .as_ref()
                    .filter(|(g, _)| std::ptr::eq(g.as_ptr(), Arc::as_ptr(&bq)))
                    .map(|(_, c)| *c);
                let cost = match known {
                    Some(c) => c,
                    None => {
                        let c = self.plan_cost(&bq, params, false);
                        p.plans.lock().generic_cost = Some((Arc::downgrade(&bq), c));
                        c
                    }
                };
                let plans = p.plans.lock();
                let avg = plans.custom_cost / plans.custom as f64;
                drop(plans);
                if cost < avg {
                    p.plans.lock().generic += 1;
                    return Ok((bq, false));
                }
                true
            }
        };
        if custom {
            let bq = self.bound_custom(p, stmt, params)?;
            let cost = self.plan_cost(&bq, params, true);
            let mut plans = p.plans.lock();
            plans.custom += 1;
            plans.custom_cost += cost;
            plans.cols = Some(bq.cols.iter().map(|c| c.ty.clone()).collect());
            return Ok((bq, true));
        }
        let bq = self.bound(p, stmt, params)?;
        p.plans.lock().generic += 1;
        Ok((bq, false))
    }

    /// cached_plan_cost: the plan's total cost, as EXPLAIN shows it; a custom plan's with a
    /// charge for planning it (1000 * cpu_operator_cost per range table entry, plus one).
    fn plan_cost(&mut self, bq: &BoundQuery, params: &[Value], planning: bool) -> f64 {
        let Ok(mut o) = crate::sql::explain::options(&[]) else {
            return 0.0;
        };
        o.format = crate::sql::explain::Format::Json;
        o.costs = true;
        o.summary = false;
        let text = self
            .with_ctx(params, |ctx| {
                Ok(crate::sql::explain::explain(
                    &bq.plan,
                    bq.hidden_rtes,
                    &o,
                    ctx,
                    params,
                    None,
                    0.0,
                    0.0,
                ))
            })
            .unwrap_or_default();
        let total = (text.split("\"Total Cost\": ").nth(1))
            .and_then(|r| r.split([',', '\n', ' ']).next())
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);
        if planning {
            total + 2.5 * (range_entries(&bq.plan) + bq.hidden_rtes + 1) as f64
        } else {
            total
        }
    }

    fn run_query(&mut self, p: &Prepared, stmt: &Node, params: &[Value]) -> Result<QueryResult> {
        self.ensure_txn();
        let planning = std::time::Instant::now();
        let mut bq = self.planned(p, stmt, params)?.0;
        self.pgss_planned(planning.elapsed());
        // Its relations are locked before it runs; a wait for a lock may end with another
        // transaction's DDL committed, which the statement then sees (Postgres analyzes it
        // only once it holds them): it is bound again over the catalog it finds.
        let version = |s: &Self| s.txn.as_ref().map_or(0, |t| t.borrow().catalog().version);
        for _ in 0..8 {
            let before = version(self);
            self.with_ctx(params, |ctx| crate::sql::binder::perms::lock_named_relations(&bq.perms, ctx))?;
            if version(self) == before {
                break;
            }
            bq = self.planned(p, stmt, params)?.0;
        }
        self.pgss_measure(false);
        // A SELECT's rows go out as they're made (PORTAL_ONE_SELECT); a data-modifying
        // query's when it has finished.
        let streams = !bq.plan.writes() && matches!(inner(stmt), Some(NodeEnum::SelectStmt(_)));
        let mut made = Vec::new();
        // ExecutorRun began (past ExecutorStart's permission checks): the RowDescription went
        // out, so an error from here on follows it.
        let mut started = false;
        let mut parallel = false;
        let r = self.with_ctx(params, |ctx| {
            bq.check_perms(ctx)?;
            started = true;
            if streams && in_parallel_worker(ctx, &bq.plan) {
                crate::exec::pgstat::count_parallel_workers(ctx, 1, 1);
                parallel = true;
                let frame = crate::error::Frame::Text("parallel worker".into());
                // The worker's own locks (an advisory lock its functions take) go when it
                // ends: its transaction is its own.
                let mark = ctx.txn.borrow().lock_mark();
                let r = ctx.sess.framed(frame, |_| crate::exec::collect_into(&bq.plan, ctx, &mut made));
                ctx.txn.borrow_mut().release_locks_since(mark);
                r?;
                return Ok((std::mem::take(&mut made), ctx.processed.get()));
            }
            if streams {
                crate::exec::collect_into(&bq.plan, ctx, &mut made)?;
                return Ok((std::mem::take(&mut made), ctx.processed.get()));
            }
            let rows = collect(&bq.plan, ctx, None)?;
            Ok((rows, ctx.processed.get()))
        });
        let (rows, processed) = match r {
            Ok(r) => r,
            Err(e) => {
                if streams && started {
                    self.partial_rows = Some(QueryResult::rows(bq.cols.clone(), made, ""));
                }
                return Err(e);
            }
        };
        if parallel {
            self.pgss_workers += 1;
        }
        self.pgss_measure(true);
        let tag = query_tag(&bq.plan, stmt, rows.len(), processed);
        let returns = !bq.cols.is_empty() || matches!(inner(stmt), Some(NodeEnum::SelectStmt(_)));
        Ok(QueryResult {
            cols: if returns { Some(bq.cols.clone()) } else { None },
            rows,
            tag,
        })
    }

    // ── extended protocol ──

    /// Parse message: a named (or unnamed, "") prepared statement.
    pub fn parse_message(
        &mut self,
        name: &str,
        sql: &str,
        param_oids: &[u32],
    ) -> Result<Arc<Prepared>> {
        // (`pg_temp` is this session's while it binds, on a thread another session may
        // have run on: an embedder's Parse / Describe before any execute.)
        crate::catalog::set_session_temp_schema(&self.state.temp_schema());
        if !name.is_empty() && self.prepared.contains_key(name) {
            return Err(Error::new(
                code::DUPLICATE_PREPARED_STATEMENT,
                format!("prepared statement \"{name}\" already exists"),
            ));
        }
        let r = self.parse_message_inner(sql, param_oids);
        match r {
            Ok(p) => {
                self.prepared.insert(name.to_string(), p.clone());
                self.prepared_meta
                    .insert(name.to_string(), (sql.to_string(), Self::now(), false));
                Ok(p)
            }
            Err(e) => {
                self.on_error();
                Err(e)
            }
        }
    }

    fn parse_message_inner(&mut self, sql: &str, param_oids: &[u32]) -> Result<Arc<Prepared>> {
        self.sync_lex();
        let parsed = crate::sql::parse::parse_live(sql);
        self.take_lex_warnings();
        let (sql, stmts) = parsed?;
        let sql = &*sql;
        if stmts.len() > 1 {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                "cannot insert multiple commands into a prepared statement",
            ));
        }
        let mut types = Vec::new();
        for o in param_oids {
            types.push(if *o == 0 {
                DataType::Unknown
            } else {
                self.visible_catalog()
                    .any_data_type(*o)
                    .ok_or_else(|| {
                        Error::new(
                            code::UNDEFINED_OBJECT,
                            format!("type with OID {o} does not exist"),
                        )
                    })?
            });
        }
        let stmt = stmts.first().and_then(|s| {
            s.stmt.as_deref().map(|n| {
                crate::sql::binder::ast::select_into_as_ctas(n).unwrap_or_else(|| n.clone())
            })
        });
        let mut p = Prepared {
            sql: sql.to_string(),
            offset: 0,
            stmt,
            param_types: types,
            bound: Mutex::new(None),
            plans: Default::default(),
        };
        // exec_parse_message: a failed transaction parses only its end.
        if self.status == TxnStatus::Failed
            && p.stmt.is_some()
            && !ends_failed_txn(p.stmt.as_ref().and_then(inner))
        {
            return Err(Error::new(
                code::IN_FAILED_SQL_TRANSACTION,
                "current transaction is aborted, commands ignored until end of transaction block",
            ));
        }
        if p.is_query() {
            let stmt = p.stmt.clone().unwrap();
            self.infer_params = true;
            let b = self.bound(&p, &stmt, &[]);
            self.infer_params = false;
            p.param_types = match b {
                Ok(b) => {
                    let cat = self.visible_catalog();
                    let mut oids: Vec<(usize, u32)> = param_oids
                        .iter()
                        .enumerate()
                        .filter(|(_, o)| cat.domain_type(**o).is_some())
                        .map(|(i, o)| (i, *o))
                        .collect();
                    for d in &b.param_domains {
                        if !oids.iter().any(|(j, _)| *j == d.0) {
                            oids.push(*d);
                        }
                    }
                    let mut plans = p.plans.lock();
                    plans.param_oids = oids;
                    plans.fixed_result = Some(result_row(&b.cols));
                    drop(plans);
                    b.params.clone()
                }
                Err(e) => self.analyzed_params(&stmt, &p.param_types, e)?,
            };
            // The query as written types its parameters, before rules rewrite it (an INSERT
            // a DO INSTEAD NOTHING rule drops still has its $n).
            if p.param_types.len() < max_param(&stmt) {
                let types = p.param_types.clone();
                if let Ok(t) = self.with_ctx(&[], |ctx| {
                    let mut b = Binder::new(ctx, types);
                    b.unplanned = true;
                    b.rule_validate = true;
                    b.bind_statement(&stmt).map(|q| q.params)
                }) && t.len() > p.param_types.len()
                {
                    p.param_types = t;
                }
            }
            // exec_parse_message: a parameter the query never reads has no type.
            if stmt.node.is_some() {
                // (pg_query's node walker skips some clauses: read the whole tree.)
                let dump = format!("{stmt:?}");
                let used: std::collections::HashSet<i32> = dump
                    .split("ParamRef { number: ")
                    .skip(1)
                    .filter_map(|rest| {
                        rest.split(|c: char| !c.is_ascii_digit())
                            .next()
                            .and_then(|n| n.parse().ok())
                    })
                    .collect();
                for i in 0..p.param_types.len() {
                    if !used.contains(&(i as i32 + 1)) && param_oids.get(i).is_none_or(|o| *o == 0)
                    {
                        return Err(Error::new(
                            code::INDETERMINATE_DATATYPE,
                            format!("could not determine data type of parameter ${}", i + 1),
                        ));
                    }
                }
            }
        } else if let Some(NodeEnum::CallStmt(c)) = p.stmt.as_ref().and_then(inner).cloned() {
            // CALL: parameter types from the procedure's.
            while p.param_types.len() < max_param(p.stmt.as_ref().unwrap()) {
                p.param_types.push(DataType::Unknown);
            }
            let types = p.param_types.clone();
            let inferred = self.with_ctx(&[], |ctx| {
                let mut b = crate::sql::binder::Binder::new(ctx, types);
                let fc = c
                    .funccall
                    .as_deref()
                    .ok_or_else(|| Error::new(code::SYNTAX_ERROR, "syntax error in CALL"))?;
                let fname = crate::sql::binder::ast::names(&fc.funcname);
                let (schema, name) = match fname.as_slice() {
                    [s, n] => (Some(s.as_str()), n.as_str()),
                    _ => (None, fname.last().map(String::as_str).unwrap_or("")),
                };
                b.resolve_routine(schema, name, &fc.args, fc.func_variadic, fc.location, true)?;
                Ok(b.params)
            })?;
            p.param_types = inferred
                .into_iter()
                .map(|t| {
                    if matches!(t, DataType::Unknown) {
                        DataType::Text
                    } else {
                        t
                    }
                })
                .collect();
        } else if let Some(n) = p.stmt.as_ref() {
            // EXPLAIN of a query: its parameters typed as the query types them
            // (transformExplainStmt analyzes the query).
            if let Some(NodeEnum::ExplainStmt(ex)) = inner(n)
                && let Some(q) = ex.query.as_deref()
                && matches!(
                    inner(q),
                    Some(
                        NodeEnum::SelectStmt(_)
                            | NodeEnum::InsertStmt(_)
                            | NodeEnum::UpdateStmt(_)
                            | NodeEnum::DeleteStmt(_)
                            | NodeEnum::MergeStmt(_)
                    )
                )
            {
                let types = p.param_types.clone();
                if let Ok(t) = self.with_ctx(&[], |ctx| {
                    let mut b = Binder::new(ctx, types);
                    b.unplanned = true;
                    b.bind_statement(q).map(|bq| bq.params)
                }) {
                    p.param_types = t;
                }
            }
            // Utility statements: count $n so ParameterDescription is right.
            let max = max_param(n);
            while p.param_types.len() < max {
                p.param_types.push(DataType::Text);
            }
            for t in &mut p.param_types {
                if matches!(t, DataType::Unknown) {
                    *t = DataType::Text;
                }
            }
        }
        Ok(Arc::new(p))
    }

    /// Result columns of a statement, without running it.
    pub fn describe(&mut self, p: &Arc<Prepared>) -> Result<Option<Vec<OutCol>>> {
        // (`pg_temp` is this session's while it binds, on a thread another session may
        // have run on: an embedder's Parse / Describe before any execute.)
        crate::catalog::set_session_temp_schema(&self.state.temp_schema());
        let Some(stmt) = p.stmt.clone() else {
            return Ok(None);
        };
        let r = (|| -> Result<Option<Vec<OutCol>>> {
            match inner(&stmt) {
                Some(NodeEnum::SelectStmt(_)) => Ok(Some(self.bound(p, &stmt, &[])?.cols.clone())),
                Some(
                    NodeEnum::InsertStmt(_)
                    | NodeEnum::UpdateStmt(_)
                    | NodeEnum::DeleteStmt(_)
                    | NodeEnum::MergeStmt(_),
                ) => {
                    let b = self.bound(p, &stmt, &[])?;
                    Ok(if b.cols.is_empty() {
                        None
                    } else {
                        Some(b.cols.clone())
                    })
                }
                Some(NodeEnum::VariableShowStmt(s)) => Ok(Some(self.show_cols(&s.name))),
                Some(NodeEnum::ExplainStmt(_)) => {
                    Ok(Some(vec![OutCol::new("QUERY PLAN", DataType::Text)]))
                }
                Some(NodeEnum::CallStmt(c)) => {
                    // A procedure with output parameters returns them as one row.
                    let nulls: Vec<Value> = vec![Value::Null; p.param_types.len()];
                    let f = self
                        .with_ctx(&nulls, |ctx| {
                            utility::bind_call_typed(ctx, c, &p.param_types)
                        })?
                        .0;
                    Ok((!f.out_cols.is_empty()).then(|| {
                        f.out_cols
                            .iter()
                            .map(|(n, t)| OutCol::new(n.clone(), t.clone()))
                            .collect()
                    }))
                }
                Some(NodeEnum::FetchStmt(f)) => Ok(self
                    .state
                    .cursors
                    .borrow()
                    .get(&f.portalname)
                    .map(|c| c.cols.clone())),
                Some(NodeEnum::ExecuteStmt(e)) => match self.prepared.get(&e.name).cloned() {
                    Some(inner_p) => self.describe(&inner_p),
                    None => Ok(None),
                },
                _ => Ok(None),
            }
        })();
        if r.is_err() {
            self.on_error();
        }
        r
    }

    /// Bind message: create a portal.
    pub fn bind_message(
        &mut self,
        portal: &str,
        stmt: &str,
        params: Vec<Option<Vec<u8>>>,
        formats: &[i16],
        result_formats: Vec<i16>,
    ) -> Result<()> {
        let r = self.bind_inner(portal, stmt, params, formats, result_formats);
        if r.is_err() {
            self.on_error();
        }
        r
    }

    fn bind_inner(
        &mut self,
        portal: &str,
        stmt: &str,
        params: Vec<Option<Vec<u8>>>,
        formats: &[i16],
        result_formats: Vec<i16>,
    ) -> Result<()> {
        let Some(p) = self.prepared.get(stmt).cloned() else {
            return Err(Error::new(
                code::INVALID_SQL_STATEMENT_NAME,
                format!("prepared statement \"{stmt}\" does not exist"),
            ));
        };
        if self.status == TxnStatus::Failed && !ends_failed_txn(p.stmt.as_ref().and_then(inner)) {
            return Err(Error::new(
                code::IN_FAILED_SQL_TRANSACTION,
                "current transaction is aborted, commands ignored until end of transaction block",
            ));
        }
        if formats.len() > 1 && formats.len() != params.len() {
            return Err(Error::new(
                code::PROTOCOL_VIOLATION,
                format!(
                    "bind message has {} parameter formats but {} parameters",
                    formats.len(),
                    params.len()
                ),
            ));
        }
        if params.len() != p.param_types.len() {
            return Err(Error::new(
                code::PROTOCOL_VIOLATION,
                format!(
                    "bind message supplies {} parameters, but prepared statement \"{stmt}\" requires {}",
                    params.len(),
                    p.param_types.len()
                ),
            ));
        }
        // An object identifier type's input (regclassin, ...) looks its name up in the
        // transaction's catalog, through the search path; an enum's checks its labels.
        fn needs_names(t: &DataType) -> bool {
            use DataType as T;
            match t {
                T::Regclass
                | T::Regtype
                | T::Regproc
                | T::Regprocedure
                | T::Regnamespace
                | T::Regrole
                | T::Regconfig
                | T::Regdictionary
                | T::Regoper
                | T::Regoperator
                | T::Regcollation
                | T::Enum(..) => true,
                T::Array(e) => needs_names(e),
                // (A named composite's fields may be domains, which record_recv checks.)
                T::Row(r) => r.oid != 0 || r.fields.iter().any(|(_, t)| needs_names(t)),
                T::Range(r) | T::Multirange(r) => needs_names(&r.subtype),
                _ => false,
            }
        }
        let fmt = if p.param_types.iter().any(needs_names) {
            self.format_ctx()
        } else {
            self.state.settings.borrow().format_ctx()
        };
        let mut values = Vec::with_capacity(params.len());
        for (i, raw) in params.into_iter().enumerate() {
            let f = match formats.len() {
                0 => 0,
                1 => formats[0],
                _ => formats.get(i).copied().unwrap_or(0),
            };
            if !matches!(f, 0 | 1) {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("unsupported format code: {f}"),
                ));
            }
            let ty = &p.param_types[i];
            // bind_param_error_callback: an error names the parameter (a text one's value
            // as log_parameter_max_length_on_error allows).
            let param_err = |e: Error, text: Option<&str>| {
                let mut c = match portal {
                    "" => format!("unnamed portal parameter ${}", i + 1),
                    p => format!("portal \"{p}\" parameter ${}", i + 1),
                };
                if let Some(t) = text {
                    c.push_str(&format!(" = {}", param_log_string(t, &self.state)));
                }
                match e.context.as_deref().map(str::to_string) {
                    Some(old) => e.context(format!("{old}\n{c}")),
                    None => e.context(c),
                }
            };
            values.push(match raw {
                None => Value::Null,
                // Bytes the type's recv leaves over name the parameter; its own errors are its.
                Some(b) if f == 1 => crate::types::binary::decode(&b, ty, &fmt)
                    .map_err(|e| match e.code {
                        code::INVALID_BINARY_REPRESENTATION
                            if e.message.starts_with("incorrect binary data format") =>
                        {
                            Error::new(
                                code::INVALID_BINARY_REPRESENTATION,
                                format!("incorrect binary data format in bind parameter {}", i + 1),
                            )
                        }
                        _ => e,
                    })
                    .map_err(|e| param_err(e, None))?,
                // Text arrives in the client's encoding (pg_client_to_server, which checks it).
                Some(b) => {
                    let s = crate::types::mbconv::to_utf8(&b, fmt.client_encoding)
                        .map_err(|e| param_err(e, None))?;
                    crate::types::text::parse_value(&s, ty, &fmt)
                        .map_err(|e| param_err(e, Some(&s)))?
                }
            });
        }
        let cols = self.describe(&p)?;
        // PortalSetResultFormat
        if let Some(cols) = &cols
            && result_formats.len() > 1
            && result_formats.len() != cols.len()
        {
            return Err(Error::new(
                code::PROTOCOL_VIOLATION,
                format!(
                    "bind message has {} result formats but query has {} columns",
                    result_formats.len(),
                    cols.len()
                ),
            ));
        }
        if let Some(f) = result_formats.iter().find(|f| !matches!(f, 0 | 1)) {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("unsupported format code: {f}"),
            ));
        }
        self.portals.insert(
            portal.to_string(),
            Portal {
                stmt: p,
                params: values,
                result_formats,
                pending: None,
                tag_verb: String::new(),
                cols,
            },
        );
        Ok(())
    }

    /// Execute message. Returns the outcome and whether the portal was suspended (more rows).
    pub fn execute_portal(&mut self, portal: &str, max_rows: usize) -> Result<(Outcome, bool)> {
        if !self.portals.contains_key(portal) && self.state.cursors.borrow().contains_key(portal) {
            let r = self.execute_cursor_portal(portal, max_rows);
            if r.is_err() {
                self.on_error();
            }
            return r;
        }
        let Some(mut pt) = self.portals.remove(portal) else {
            let e = Error::new(
                code::INVALID_CURSOR_NAME,
                format!("portal \"{portal}\" does not exist"),
            );
            self.on_error();
            return Err(e);
        };
        if let Some(mut rest) = pt.pending.take() {
            let n = if max_rows == 0 {
                rest.len()
            } else {
                max_rows.min(rest.len())
            };
            let rows: Vec<Row> = rest.drain(..n).collect();
            // Suspended when the row count was reached, even by the last row: PortalRun
            // doesn't look ahead (the next Execute then completes with no rows).
            let more = !rest.is_empty() || (max_rows > 0 && rows.len() == max_rows);
            let tag = format!("{} {}", pt.tag_verb, rows.len());
            let cols = pt.cols.clone();
            // (A SELECT run to its end stays there: Execute again returns no rows.)
            if more || pt.tag_verb == "SELECT" {
                pt.pending = Some(rest);
            }
            self.portals.insert(portal.to_string(), pt);
            return Ok((Outcome::Done(QueryResult { cols, rows, tag }), more));
        }
        let stmt = pt.stmt.clone();
        let params = pt.params.clone();
        let out = self.execute_with(&stmt, &params, max_rows);
        let out = match out {
            Ok(o) => o,
            Err(e) => {
                self.portals.insert(portal.to_string(), pt);
                return Err(e);
            }
        };
        let r = match out {
            Outcome::Done(mut r) => {
                let mut more = false;
                if max_rows > 0
                    && (r.rows.len() > max_rows
                        || r.rows.len() == max_rows && r.cols.is_some() && r.tag.starts_with("SELECT "))
                {
                    let rest: VecDeque<Row> = r.rows.split_off(max_rows).into();
                    pt.tag_verb = r.tag.split(' ').next().unwrap_or("SELECT").to_string();
                    pt.pending = Some(rest);
                    more = true;
                } else if r.cols.is_some() && r.tag.starts_with("SELECT ") {
                    pt.tag_verb = "SELECT".into();
                    pt.pending = Some(VecDeque::new());
                }
                (Outcome::Done(r), more)
            }
            o => (o, false),
        };
        self.portals.insert(portal.to_string(), pt);
        Ok(r)
    }

    /// FETCH / MOVE on a portal the protocol bound (one namespace with cursors): forward
    /// only, as a portal made without SCROLL. None: no such portal.
    pub(super) fn fetch_protocol_portal(
        &mut self,
        s: &pb::FetchStmt,
    ) -> Result<Option<QueryResult>> {
        use pb::FetchDirection as D;
        if !self.portals.contains_key(&s.portalname) {
            return Ok(None);
        }
        let dir = D::try_from(s.direction).unwrap_or(D::FetchForward);
        let count = match dir {
            D::FetchForward if s.how_many >= 0 => s.how_many,
            D::FetchAbsolute | D::FetchRelative if s.how_many > 0 => {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "cursor can only scan forward",
                )
                .hint("Declare it with SCROLL option to enable backward scan."));
            }
            _ => {
                return Err(Error::new(
                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "cursor can only scan forward",
                )
                .hint("Declare it with SCROLL option to enable backward scan."));
            }
        };
        let not_started = self.portals[&s.portalname].pending.is_none();
        if not_started {
            // Run it, keeping every row in the portal for this FETCH and what follows.
            let (o, _) = self.execute_portal(&s.portalname, 0)?;
            let rows = match o {
                Outcome::Done(r) => r.rows,
                _ => Vec::new(),
            };
            let pt = self.portals.get_mut(&s.portalname).unwrap();
            pt.tag_verb = "SELECT".into();
            pt.pending = Some(rows.into());
        }
        let pt = self.portals.get_mut(&s.portalname).unwrap();
        let rest = pt.pending.get_or_insert_with(VecDeque::new);
        let n = (count.max(0) as u64).min(rest.len() as u64) as usize;
        let rows: Vec<Row> = rest.drain(..n).collect();
        let cols = pt.cols.clone().unwrap_or_default();
        Ok(Some(if s.ismove {
            QueryResult::command(format!("MOVE {}", rows.len()))
        } else {
            let tag = format!("FETCH {}", rows.len());
            QueryResult::rows(cols, rows, tag)
        }))
    }

    /// Execute on a cursor DECLARE made (cursors are portals, in one namespace with the
    /// protocol's): the next `max_rows` rows (all for 0), tagged as its SELECT; suspended
    /// unless it ran out of rows.
    fn execute_cursor_portal(&mut self, name: &str, max_rows: usize) -> Result<(Outcome, bool)> {
        if self.status == TxnStatus::Failed {
            return Err(Error::new(
                code::IN_FAILED_SQL_TRANSACTION,
                "current transaction is aborted, commands ignored until end of transaction block",
            ));
        }
        let count = if max_rows == 0 { i64::MAX } else { max_rows as i64 };
        let got = self.with_ctx(&[], |ctx| {
            cursor::fetch(ctx, name, pb::FetchDirection::FetchForward, count, true)
        })?;
        let Some((rows, cols, _)) = got else {
            return Err(Error::new(
                code::INVALID_CURSOR_NAME,
                format!("portal \"{name}\" does not exist"),
            ));
        };
        let more = max_rows > 0 && rows.len() == max_rows;
        let tag = format!("SELECT {}", rows.len());
        Ok((Outcome::Done(QueryResult::rows(cols, rows, tag)), more))
    }

    /// A portal's result columns for Describe: a bound one's, or a cursor's (in text).
    pub fn portal_cols(&self, name: &str) -> Option<(Option<Vec<OutCol>>, Vec<i16>)> {
        if let Some(pt) = self.portals.get(name) {
            return Some((pt.cols.clone(), pt.result_formats.clone()));
        }
        self.state
            .cursors
            .borrow()
            .get(name)
            .map(|c| (Some(c.cols.clone()), Vec::new()))
    }

    pub fn close_statement(&mut self, name: &str) {
        self.prepared.remove(name);
        self.prepared_meta.remove(name);
    }

    /// Close message on a portal: a bound one, or a cursor.
    pub fn close_portal(&mut self, name: &str) {
        if self.portals.remove(name).is_none() {
            self.state.cursors.borrow_mut().remove(name);
        }
    }

    /// Sync: end an implicit transaction; the unnamed portal goes away.
    pub fn sync(&mut self) -> Result<()> {
        self.pipelining = false;
        self.portals.retain(|n, _| !n.is_empty());
        let r = self.end_batch();
        if self.status == TxnStatus::Idle {
            self.portals.clear();
        }
        r
    }

    // ── transactions ──

    fn transaction_stmt(&mut self, t: &pb::TransactionStmt) -> Result<QueryResult> {
        use pb::TransactionStmtKind as K;
        let kind = K::try_from(t.kind).unwrap_or(K::TransStmtBegin);
        match kind {
            K::TransStmtBegin | K::TransStmtStart => {
                let tag = if kind == K::TransStmtStart {
                    "START TRANSACTION"
                } else {
                    "BEGIN"
                };
                if self.status != TxnStatus::Idle {
                    self.notices.push(warning(
                        code::ACTIVE_SQL_TRANSACTION,
                        "there is already a transaction in progress",
                    ));
                    return Ok(QueryResult::command(tag));
                }
                let (mut iso, mut ro) = self.default_txn_opts();
                apply_txn_options(&t.options, &mut iso, &mut ro)?;
                default_deferrable(&self.state);
                set_deferrable(&self.state, &t.options);
                if self.implicit {
                    // Earlier statements of this batch join the block.
                    self.implicit = false;
                } else {
                    self.txn_time = Self::now();
                    self.txn_opts = Some((iso, ro));
                }
                self.status = TxnStatus::InBlock;
                Ok(QueryResult::command(tag))
            }
            K::TransStmtCommit => {
                if t.chain {
                    return self.chain(true);
                }
                let failed = self.status == TxnStatus::Failed;
                // (An implicit transaction, of a multi-statement query too, isn't one.)
                if self.status == TxnStatus::Idle {
                    self.notices.push(warning(
                        code::NO_ACTIVE_SQL_TRANSACTION,
                        "there is no transaction in progress",
                    ));
                }
                self.finish_txn(!failed)?;
                Ok(QueryResult::command(if failed {
                    "ROLLBACK"
                } else {
                    "COMMIT"
                }))
            }
            K::TransStmtRollback => {
                if t.chain {
                    return self.chain(false);
                }
                if self.status == TxnStatus::Idle {
                    self.notices.push(warning(
                        code::NO_ACTIVE_SQL_TRANSACTION,
                        "there is no transaction in progress",
                    ));
                }
                self.finish_txn(false)?;
                Ok(QueryResult::command("ROLLBACK"))
            }
            K::TransStmtSavepoint => {
                self.require_block("SAVEPOINT")?;
                self.ensure_txn();
                let ro = self.txn.as_ref().unwrap().borrow().read_only;
                self.txn
                    .as_ref()
                    .unwrap()
                    .borrow_mut()
                    .savepoint(&t.savepoint_name);
                let depth = self.txn.as_ref().unwrap().borrow().subtxn_depth();
                self.ro_stack.truncate(depth.saturating_sub(1));
                self.ro_stack.push(ro);
                self.state.settings.borrow_mut().subxact_begin(depth);
                Ok(QueryResult::command("SAVEPOINT"))
            }
            K::TransStmtRelease => {
                self.require_block("RELEASE SAVEPOINT")?;
                self.ensure_txn();
                self.txn
                    .as_ref()
                    .unwrap()
                    .borrow_mut()
                    .release_savepoint(&t.savepoint_name)?;
                let depth = self.txn.as_ref().unwrap().borrow().subtxn_depth();
                self.state.settings.borrow_mut().subxact_commit(depth);
                self.restore_read_only(depth, depth);
                for c in self.state.cursors.borrow_mut().values_mut() {
                    c.subxact = c.subxact.min(depth);
                }
                Ok(QueryResult::command("RELEASE"))
            }
            K::TransStmtRollbackTo => {
                self.require_block("ROLLBACK TO SAVEPOINT")?;
                self.ensure_txn();
                self.txn
                    .as_ref()
                    .unwrap()
                    .borrow_mut()
                    .rollback_to(&t.savepoint_name)?;
                let depth = self.txn.as_ref().unwrap().borrow().subtxn_depth();
                self.state.settings.borrow_mut().subxact_abort(depth);
                self.restore_read_only(depth.saturating_sub(1), depth);
                // Cursors opened in the subtransactions rolled back are gone.
                self.state
                    .cursors
                    .borrow_mut()
                    .retain(|_, c| c.subxact < depth || c.hold && c.lazy.is_none());
                self.status = TxnStatus::InBlock;
                Ok(QueryResult::command("ROLLBACK"))
            }
            // PREPARE TRANSACTION ends the block; preparing it then fails here.
            K::TransStmtPrepare if self.status == TxnStatus::Failed => {
                self.finish_txn(false)?;
                Ok(QueryResult::command("ROLLBACK"))
            }
            K::TransStmtPrepare if self.status == TxnStatus::InBlock => {
                let temp = self.state.temp_used.get();
                self.finish_txn(false)?;
                Err(if temp {
                    Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "cannot PREPARE a transaction that has operated on temporary objects",
                    )
                } else {
                    Error::new(
                        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "prepared transactions are disabled",
                    )
                    .hint("Set \"max_prepared_transactions\" to a nonzero value.")
                })
            }
            // FinishPreparedTransaction: no transaction is ever prepared here, so LockGXact
            // finds none (it doesn't look at max_prepared_transactions).
            K::TransStmtCommitPrepared | K::TransStmtRollbackPrepared => {
                let what = if kind == K::TransStmtCommitPrepared {
                    "COMMIT PREPARED"
                } else {
                    "ROLLBACK PREPARED"
                };
                if self.status != TxnStatus::Idle {
                    return Err(Error::new(
                        code::ACTIVE_SQL_TRANSACTION,
                        format!("{what} cannot run inside a transaction block"),
                    ));
                }
                Err(Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("prepared transaction with identifier \"{}\" does not exist", t.gid),
                ))
            }
            K::TransStmtPrepare => {
                Err(Error::new(
                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "prepared transactions are disabled",
                )
                .hint("Set \"max_prepared_transactions\" to a nonzero value."))
            }
            _ => Err(crate::error::unsupported("this transaction command")),
        }
    }

    /// COMMIT AND CHAIN / ROLLBACK AND CHAIN: a new transaction with the same options.
    fn chain(&mut self, commit: bool) -> Result<QueryResult> {
        if self.status == TxnStatus::Idle {
            let what = if commit {
                "COMMIT AND CHAIN"
            } else {
                "ROLLBACK AND CHAIN"
            };
            return Err(Error::new(
                code::NO_ACTIVE_SQL_TRANSACTION,
                format!("{what} can only be used in transaction blocks"),
            ));
        }
        let opts = self.txn.as_ref().map(|t| {
            let t = t.borrow();
            (t.isolation, t.read_only)
        });
        let failed = self.status == TxnStatus::Failed;
        let deferrable = self.state.settings.borrow().get_raw("transaction_deferrable");
        self.finish_txn(commit && !failed)?;
        self.status = TxnStatus::InBlock;
        self.txn_opts = opts;
        if let Some(d) = deferrable {
            let mut st = self.state.settings.borrow_mut();
            let _ = st.set("transaction_deferrable", Some(&d), true, true);
            st.changed.retain(|n| n != "transaction_deferrable");
        }
        self.txn_time = Self::now();
        Ok(QueryResult::command(if commit && !failed {
            "COMMIT"
        } else {
            "ROLLBACK"
        }))
    }

    /// SET CONSTRAINTS { ALL | name, ... } { DEFERRED | IMMEDIATE }.
    fn set_constraints(&mut self, s: &pb::ConstraintsSetStmt) -> Result<QueryResult> {
        if self.status == TxnStatus::Idle {
            self.notices.push(warning(
                code::NO_ACTIVE_SQL_TRANSACTION,
                "SET CONSTRAINTS can only be used in transaction blocks",
            ));
        }
        let names: Vec<(Option<String>, String)> = s
            .constraints
            .iter()
            .filter_map(|n| match &n.node {
                Some(NodeEnum::RangeVar(rv)) => Some((
                    (!rv.schemaname.is_empty()).then(|| rv.schemaname.clone()),
                    rv.relname.clone(),
                )),
                _ => None,
            })
            .collect();
        let deferred = s.deferred;
        self.with_ctx(&[], |ctx| {
            let cat = ctx.catalog();
            let mut oids = std::collections::HashSet::new();
            for (schema, name) in &names {
                let schemas = match schema {
                    Some(s) => vec![cat.schema_oid(s)?],
                    None => cat.effective_path(&ctx.search_path),
                };
                let mut found = false;
                for sch in schemas {
                    for con in cat
                        .tables
                        .values()
                        .filter(|t| t.schema == sch)
                        .flat_map(|t| t.constraints.iter())
                        .filter(|c| &c.name == name)
                    {
                        if !con.deferral().0 {
                            return Err(Error::new(
                                code::WRONG_OBJECT_TYPE,
                                format!("constraint \"{name}\" is not deferrable"),
                            ));
                        }
                        oids.insert(con.oid);
                        found = true;
                    }
                    // Constraint triggers are constraints too.
                    for x in cat.triggers.values().filter(|x| {
                        x.constraint
                            && &x.name == name
                            && cat.tables.get(&x.table).is_some_and(|t| t.schema == sch)
                    }) {
                        if !x.deferrable {
                            return Err(Error::new(
                                code::WRONG_OBJECT_TYPE,
                                format!("constraint \"{name}\" is not deferrable"),
                            ));
                        }
                        oids.insert(x.oid);
                        found = true;
                    }
                    if found {
                        break;
                    }
                }
                if !found {
                    return Err(Error::new(
                        code::UNDEFINED_OBJECT,
                        format!("constraint \"{name}\" does not exist"),
                    ));
                }
            }
            // A partitioned table's constraint takes its partitions' along (conparentid).
            loop {
                let more: Vec<u32> = cat
                    .tables
                    .values()
                    .flat_map(|t| t.constraints.iter())
                    .filter(|c| c.parent != 0 && oids.contains(&c.parent) && !oids.contains(&c.oid))
                    .map(|c| c.oid)
                    .collect();
                if more.is_empty() {
                    break;
                }
                oids.extend(more);
            }
            {
                let mut txn = ctx.txn.borrow_mut();
                if names.is_empty() {
                    txn.all_deferred = Some(deferred);
                    txn.constraint_modes.clear();
                } else {
                    txn.constraint_modes
                        .extend(oids.iter().map(|o| (*o, deferred)));
                }
            }
            if !deferred {
                crate::exec::trigger::fire_deferred(ctx, (!names.is_empty()).then_some(&oids))?;
                crate::exec::dml::check_deferred(ctx, (!names.is_empty()).then_some(&oids))?;
            }
            Ok(())
        })?;
        Ok(QueryResult::command("SET CONSTRAINTS"))
    }

    /// After a subtransaction ends: transaction_read_only as savepoint `at` (0-based)
    /// found it, and the savepoints still open are `depth`.
    fn restore_read_only(&mut self, at: usize, depth: usize) {
        if let Some(&ro) = self.ro_stack.get(at)
            && let Some(t) = &self.txn
        {
            t.borrow_mut().read_only = ro;
            let _ = self.state.settings.borrow_mut().set(
                "transaction_read_only",
                Some(if ro { "on" } else { "off" }),
                true,
                true,
            );
        }
        self.ro_stack.truncate(depth);
    }

    /// INSERT / UPDATE / DELETE / MERGE of a temporary table: allowed in a read-only
    /// transaction (only writes to permanent relations are refused).
    fn writes_temp_only(&self, node: &NodeEnum) -> bool {
        let rv = match node {
            NodeEnum::InsertStmt(s) => s.relation.as_ref(),
            NodeEnum::UpdateStmt(s) => s.relation.as_ref(),
            NodeEnum::DeleteStmt(s) => s.relation.as_ref(),
            NodeEnum::MergeStmt(s) => s.relation.as_ref(),
            _ => None,
        };
        let Some(rv) = rv else {
            return false;
        };
        let has_with = match node {
            NodeEnum::InsertStmt(s) => s.with_clause.is_some(),
            NodeEnum::UpdateStmt(s) => s.with_clause.is_some(),
            NodeEnum::DeleteStmt(s) => s.with_clause.is_some(),
            NodeEnum::MergeStmt(s) => s.with_clause.is_some(),
            _ => true,
        };
        if has_with {
            return false;
        }
        let Some(t) = &self.txn else {
            return false;
        };
        let cat = t.borrow().catalog();
        let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
        matches!(
            cat.lookup(schema, &rv.relname, &self.state.search_path()),
            Ok(Some(crate::catalog::Relation::Table(t))) if cat.schema_name(t.schema).starts_with("pg_temp")
        )
    }

    fn require_block(&self, what: &str) -> Result<()> {
        if self.status == TxnStatus::Idle {
            return Err(Error::new(
                code::NO_ACTIVE_SQL_TRANSACTION,
                format!("{what} can only be used in transaction blocks"),
            ));
        }
        Ok(())
    }

    fn database_stmt(&mut self, node: &NodeEnum) -> Result<QueryResult> {
        let cluster = self.state.cluster.clone();
        match node {
            NodeEnum::CreatedbStmt(c) => {
                let cat = self.state.db.catalog();
                let me = self.state.current_role(&cat);
                if !cat
                    .roles
                    .get(&me)
                    .is_some_and(|r| r.create_db || r.superuser)
                {
                    return Err(Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        "permission denied to create database",
                    ));
                }
                let mut owner = cat
                    .roles
                    .get(&me)
                    .map(|r| r.name.clone())
                    .unwrap_or_else(|| self.state.user.clone());
                let mut template = None;
                for o in &c.options {
                    if let Some(NodeEnum::DefElem(d)) = inner(o) {
                        match d.defname.as_str() {
                            "owner" => {
                                owner = crate::sql::binder::ast::def_arg_str(d).unwrap_or(owner)
                            }
                            "template" => template = crate::sql::binder::ast::def_arg_str(d),
                            _ => {}
                        }
                    }
                }
                cluster.create_database(&c.dbname, &owner, template.as_deref())?;
                Ok(QueryResult::command("CREATE DATABASE"))
            }
            NodeEnum::DropdbStmt(d) => {
                let force = d.options.iter().any(
                    |o| matches!(inner(o), Some(NodeEnum::DefElem(e)) if e.defname == "force"),
                );
                let cat = self.state.db.catalog();
                let me = self.state.current_role(&cat);
                if let Some(owner) = cluster.database_owner(&d.dbname) {
                    let owner = cat.role_by_name(&owner).map_or(0, |r| r.oid);
                    if !cat.has_privs_of_role(me, owner, self.state.db_owner(&cat)) {
                        return Err(Error::new(
                            code::INSUFFICIENT_PRIVILEGE,
                            format!("must be owner of database {}", d.dbname),
                        ));
                    }
                }
                if !cluster.drop_database(&d.dbname, d.missing_ok, force, &self.state.database)? {
                    self.notices.push(notice(format!(
                        "database \"{}\" does not exist, skipping",
                        d.dbname
                    )));
                }
                Ok(QueryResult::command("DROP DATABASE"))
            }
            _ => unreachable!(),
        }
    }

    /// Changed reportable settings (for ParameterStatus), as (name, value).
    pub fn take_changed_settings(&mut self) -> Vec<(String, String)> {
        let mut s = self.state.settings.borrow_mut();
        let mut names = std::mem::take(&mut s.changed);
        names.sort();
        names.dedup();
        names
            .into_iter()
            .map(|n| (n.clone(), s.show_value(&n)))
            .collect()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.finish_txn(false);
        // RemoveTempRelationsCallback: the session's temporary objects go with it (done by
        // the server, whoever the current user is). A catalog change while another session
        // of the database runs could make its statement fail (a concurrent schema change),
        // so this waits for the database's last session, which drops every session's. The
        // cleanup is announced before the sessions are counted, and a session logging in
        // counts itself before it looks for cleanups (Session::new / login), so one of the
        // two always sees the other.
        let has_temp = |me: &Self| {
            let cat = me.state.db.catalog();
            cat.schemas
                .values()
                .filter(|s| s.name.starts_with("pg_temp_"))
                .any(|s| {
                    cat.tables.values().any(|t| t.schema == s.oid)
                        || cat.views.values().any(|v| v.schema == s.oid)
                        || cat.sequences.values().any(|q| q.schema == s.oid)
                })
        };
        if has_temp(self) {
            self.state.db.temp_cleanups.fetch_add(1, Ordering::SeqCst);
            if self.state.db.sessions.load(Ordering::SeqCst) == 1 {
                self.state
                    .definer
                    .borrow_mut()
                    .push(crate::catalog::BOOTSTRAP_SUPERUSER_OID);
                for _ in 0..20 {
                    let ok = self.drop_temp_objects(true).is_ok();
                    let _ = self.finish_txn(false);
                    if ok || !has_temp(self) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                self.state.definer.borrow_mut().pop();
            }
            self.state.db.temp_cleanups.fetch_sub(1, Ordering::SeqCst);
        }
        self.state.db.notify.detach(self.state.pid);
        self.state.db.sessions.fetch_sub(1, Ordering::Relaxed);
        self.state.db.touch();
    }
}

fn ends_failed_txn(n: Option<&NodeEnum>) -> bool {
    use pb::TransactionStmtKind as K;
    match n {
        Some(NodeEnum::TransactionStmt(t)) => matches!(
            K::try_from(t.kind),
            Ok(K::TransStmtCommit
                | K::TransStmtRollback
                | K::TransStmtRollbackTo
                | K::TransStmtRollbackPrepared)
        ),
        _ => false,
    }
}

fn apply_txn_options(opts: &[Node], iso: &mut Isolation, ro: &mut bool) -> Result<()> {
    for o in opts {
        let Some(NodeEnum::DefElem(d)) = inner(o) else {
            continue;
        };
        let v = crate::sql::binder::ast::def_arg_str(d).unwrap_or_default();
        match d.defname.as_str() {
            "transaction_isolation" => {
                *iso = Isolation::parse(&v).ok_or_else(|| {
                    Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!("invalid isolation level \"{v}\""),
                    )
                })?
            }
            "transaction_read_only" => {
                *ro = crate::sql::binder::ast::def_arg_int(d)
                    .map(|i| i != 0)
                    .unwrap_or(v == "true")
            }
            _ => {}
        }
    }
    Ok(())
}

/// DEFERRABLE / NOT DEFERRABLE among BEGIN / SET TRANSACTION options: transaction_deferrable.
/// BuildParamLogString: a parameter's text, quoted, cut to log_parameter_max_length_on_error
/// bytes (whole characters; -1: all of it) with "..." after a cut.
fn param_log_string(v: &str, state: &SessionState) -> String {
    let max = state.settings.borrow().int("log_parameter_max_length_on_error");
    let mut cut = v.len();
    if max >= 0 && (max as usize) < v.len() {
        cut = max as usize;
        while !v.is_char_boundary(cut) {
            cut -= 1;
        }
    }
    let mut out = format!("'{}", v[..cut].replace('\'', "''"));
    if cut < v.len() {
        out.push_str("...");
    }
    out.push('\'');
    out
}

/// StartTransaction: XactDeferrable = DefaultXactDeferrable (transaction_deferrable starts
/// as default_transaction_deferrable; it's off unless that is on).
fn default_deferrable(state: &SessionState) {
    let mut st = state.settings.borrow_mut();
    if st.bool("default_transaction_deferrable") && !st.bool("transaction_deferrable") {
        let _ = st.set("transaction_deferrable", Some("on"), true, true);
        st.changed.retain(|n| n != "transaction_deferrable");
    }
}

pub(crate) fn set_deferrable(state: &SessionState, opts: &[Node]) {
    for o in opts {
        let Some(NodeEnum::DefElem(d)) = inner(o) else {
            continue;
        };
        if d.defname == "transaction_deferrable" {
            let on = crate::sql::binder::ast::def_arg_int(d).map_or_else(
                || crate::sql::binder::ast::def_arg_str(d).as_deref() == Some("true"),
                |i| i != 0,
            );
            let mut st = state.settings.borrow_mut();
            let _ = st.set(
                "transaction_deferrable",
                Some(if on { "on" } else { "off" }),
                true,
                true,
            );
            st.changed.retain(|n| n != "transaction_deferrable");
        }
    }
}

fn default_txn_opts(state: &SessionState) -> (Isolation, bool) {
    state.settings.borrow().default_txn()
}

/// A new transaction's characteristics, as `transaction_isolation` / `transaction_read_only`.
fn txn_settings(state: &SessionState, iso: Isolation, ro: bool) {
    let mut s = state.settings.borrow_mut();
    s.start_transaction(iso.name(), ro);
    s.changed
        .retain(|n| n != "transaction_isolation" && n != "transaction_read_only");
}

/// COMMIT / ROLLBACK [AND CHAIN] inside a procedure or DO block run non-atomically: end the
/// transaction and carry on in a new one (with the same characteristics when chained).
pub(crate) fn end_txn_in_routine(ctx: &ExecCtx, commit: bool, chain: bool) -> Result<()> {
    if ctx.txn.borrow().in_savepoint() {
        let what = if commit { "commit" } else { "roll back" };
        return Err(Error::new(
            code::INVALID_TRANSACTION_TERMINATION,
            format!("cannot {what} while a subtransaction is active"),
        ));
    }
    if commit
        && (!ctx.txn.borrow().deferred.is_empty() || !ctx.txn.borrow().deferred_triggers.is_empty())
    {
        crate::exec::trigger::fire_deferred(ctx, None)?;
        crate::exec::dml::check_deferred(ctx, None)?;
    }
    // A loop's cursor (and, committing, a holdable one) outlives the transaction: its query
    // runs to the end in it.
    crate::session::cursor::persist_for_commit(ctx, commit)?;
    let (iso, ro) = {
        let t = ctx.txn.borrow();
        if chain {
            (t.isolation, t.read_only)
        } else {
            default_txn_opts(ctx.sess)
        }
    };
    let next = {
        let t = ctx.txn.borrow();
        let mut next = ctx.sess.db.begin(iso, ro, t.cancel.clone());
        next.lock_peer = Some(ctx.sess.lock_owner);
        next.lock_timeout = t.lock_timeout;
        next.seq_last = t.seq_last.clone();
        next
    };
    let mut old = std::mem::replace(&mut *ctx.txn.borrow_mut(), next);
    let stats = std::mem::take(&mut old.pgstat);
    let r = if commit {
        let notes = std::mem::take(&mut old.notifies);
        let r = ctx.sess.cluster.commit(old).map(|_| ());
        committed_notify(ctx.sess, r.is_ok(), &notes);
        r
    } else {
        for (name, owner) in std::mem::take(&mut old.db_owner_undo).into_iter().rev() {
            let _ = ctx.sess.cluster.set_database_owner(&name, &owner);
        }
        old.rollback();
        committed_notify(ctx.sess, false, &[]);
        Ok(())
    };
    crate::exec::pgstat::end_xact(ctx.sess, stats, commit && r.is_ok(), &ctx.sess.db.catalog());
    ctx.sess
        .settings
        .borrow_mut()
        .end_transaction(commit && r.is_ok());
    txn_settings(ctx.sess, iso, ro);
    // Cursors end with the transaction, except holdable ones and those a loop is reading.
    ctx.sess
        .cursors
        .borrow_mut()
        .retain(|_, c| c.pinned || (c.hold && (commit || c.lazy.is_none())));
    ctx.sess.db.touch();
    ctx.set_txn_time(Session::now());
    ctx.txn.borrow_mut().begin_statement(true)?;
    r
}

/// A transaction ended: when it committed, apply its LISTEN / UNLISTENs and send its NOTIFYs.
fn committed_notify(state: &SessionState, committed: bool, notes: &[(String, String)]) {
    let listens = std::mem::take(&mut *state.listen_actions.borrow_mut());
    if !committed {
        return;
    }
    let hub = &state.db.notify;
    for (listen, ch) in listens {
        match (listen, ch) {
            (true, Some(ch)) => hub.listen(state.pid, &ch),
            (_, ch) => hub.unlisten(state.pid, ch.as_deref()),
        }
    }
    let zeroed = hub.queue_entries(notes);
    crate::exec::pgstat::count_slru_zeroed(state, "notify", zeroed);
    hub.notify(state.pid, notes);
}

pub(crate) fn warning(c: &'static str, msg: impl Into<String>) -> Error {
    let mut e = Error::new(c, msg);
    e.severity = crate::error::Severity::Warning;
    e
}

pub(crate) fn notice(msg: impl Into<String>) -> Error {
    let mut e = Error::new(code::SUCCESSFUL_COMPLETION, msg);
    e.severity = crate::error::Severity::Notice;
    e
}

/// The command tag of a finished query.
fn query_tag(plan: &Plan, stmt: &Node, nrows: usize, processed: u64) -> String {
    match inner(stmt) {
        Some(NodeEnum::InsertStmt(_)) => format!("INSERT 0 {processed}"),
        Some(NodeEnum::UpdateStmt(_)) => format!("UPDATE {processed}"),
        Some(NodeEnum::DeleteStmt(_)) => format!("DELETE {processed}"),
        Some(NodeEnum::MergeStmt(_)) => format!("MERGE {processed}"),
        _ => {
            let _ = plan;
            format!("SELECT {nrows}")
        }
    }
}

/// Highest `$n` in a statement.
fn max_param(n: &Node) -> usize {
    let mut max = 0;
    if let Some(ne) = n.node.as_ref() {
        for (node, _, _, _) in ne.nodes() {
            if let pg_query::NodeRef::ParamRef(p) = node {
                max = max.max(p.number.max(0) as usize);
            }
        }
    }
    max
}

/// About how many range table entries the planned statement has (relations, functions and
/// VALUES lists scanned, a DML statement's target), as the plan cache's planning charge
/// counts them; FROM items without a scan of their own are counted apart.
fn range_entries(plan: &Plan) -> usize {
    match plan {
        Plan::Scan(_)
        | Plan::SysScan { .. }
        | Plan::FunctionScan { .. }
        | Plan::CteScan { .. }
        | Plan::WorkScan { .. }
        | Plan::EnrScan { .. } => 1,
        Plan::Values { rows, .. } => (rows.len() > 1) as usize,
        Plan::Insert(i) => 1 + range_entries(&i.source),
        Plan::Update(u) => range_entries(&u.source),
        Plan::Delete(d) => range_entries(&d.source),
        Plan::Merge(m) => 1 + range_entries(&m.source),
        Plan::Filter { input, .. }
        | Plan::Project { input, .. }
        | Plan::ProjectSet { input, .. }
        | Plan::Agg { input, .. }
        | Plan::Window { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input, .. }
        | Plan::LockRows { input, .. }
        | Plan::Barrier { input, .. } | Plan::Context { input, .. } => range_entries(input),
        Plan::SetOp { left, right, .. } | Plan::Join { left, right, .. } => {
            range_entries(left) + range_entries(right)
        }
        Plan::Append { inputs, .. } => inputs.iter().map(|p| range_entries(p)).sum(),
        Plan::With { ctes, body, .. } => {
            ctes.iter().map(|(_, p)| range_entries(p)).sum::<usize>() + range_entries(body)
        }
        Plan::Recursive {
            base, recursive, ..
        } => range_entries(base) + range_entries(recursive),
        _ => 0,
    }
}

/// debug_parallel_query = on: a parallel-safe SELECT runs in the one worker of a single-copy
/// Gather (standard_planner), when a worker can be launched; the leader relays the worker's
/// errors and notices with the CONTEXT line "parallel worker" (HandleParallelMessage; not
/// in `regress` mode), and pg_stat_database counts the worker.
fn in_parallel_worker(ctx: &crate::exec::ExecCtx, plan: &crate::sql::plan::Plan) -> bool {
    {
        let s = ctx.sess.settings.borrow();
        let on = s
            .get_raw("debug_parallel_query")
            .is_some_and(|v| matches!(v.as_str(), "on" | "true" | "yes" | "1"));
        let workers = s
            .get_raw("max_parallel_workers")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(8);
        if !on || workers <= 0 {
            return false;
        }
    }
    crate::sql::paths::parallel::ParCfg::read(ctx).max_workers > 0
        && !plan.writes()
        && crate::sql::paths::parallel::plan_hazard(plan, &ctx.catalog()) == 's'
}
