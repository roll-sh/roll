//! Cursors (portals): a query's executor and a position, as PostgreSQL's portals have. DECLARE
//! and PL/pgSQL OPEN only start the query: it runs as FETCH / MOVE pass over its rows, and
//! moving back over a row (SCROLL) evaluates its target list again, as PostgreSQL's executor
//! does. A query that writes runs to completion at the first FETCH, its rows then read from
//! a store; so do a holdable cursor's at COMMIT. They live in the session state so SQL
//! (DECLARE / FETCH / MOVE / CLOSE), PL/pgSQL (OPEN / FETCH / FOR over a cursor) and
//! `WHERE CURRENT OF` all see the same ones.

use crate::error::{Error, Result, code};
use crate::exec::{ExecCtx, Row};
use crate::sql::expr::Expr;
use crate::sql::plan::{OutCol, Plan};
use crate::types::{DataType, Value};
use pg_query::protobuf as pb;
use std::rc::Rc;
use std::sync::Arc;

/// FETCH ALL's count.
const FETCH_ALL: i64 = i64::MAX;

pub struct Cursor {
    pub cols: Vec<OutCol>,
    /// The rows the query produced so far: as returned, or (when its target list is
    /// evaluated each time a row is passed: `Lazy::project`) the rows under it.
    pub rows: Vec<Row>,
    /// A simply updatable cursor: its table and each row's storage key (`WHERE CURRENT OF`).
    pub keys: Option<(u32, Vec<Arc<[u8]>>)>,
    /// The executor's position: 0 = before the first row, k = on row k, rows.len()+1 = after
    /// the last (once the query is known to be done).
    pub pos: usize,
    /// The portal's row number (portalPos), and whether it is at the start / the end.
    portal_pos: u64,
    at_start: bool,
    at_end: bool,
    pub hold: bool,
    /// Read by a PL/pgSQL FOR loop: survives a COMMIT inside the loop.
    pub pinned: bool,
    pub scroll: bool,
    /// The statement's text and when it was opened (pg_cursors).
    pub text: String,
    pub created: i64,
    /// The query's executor, while it can still produce or re-evaluate rows (None: the
    /// rows are a store).
    pub lazy: Option<Lazy>,
    /// `rows` are under the target list, which is evaluated as rows are passed.
    projected: bool,
    /// The row the executor last returned.
    last: Option<Row>,
    /// Running the query failed: the portal can't run any more.
    pub failed: bool,
    /// With a re-evaluated qual under a LIMIT (`Lazy::qual`), the Limit node's state: the
    /// position of the row it is on (counting OFFSET's), and whether it has yet to start, has
    /// nothing to return, or stepped off the window's end, its start, or its input's end
    /// (ExecLimit's LIMIT_INITIAL, LIMIT_EMPTY, LIMIT_WINDOWEND, LIMIT_WINDOWSTART,
    /// LIMIT_SUBPLANEOF).
    lim_pos: u64,
    lim_state: LimState,
    /// The savepoint level it was declared at (0: the transaction itself): rolling back
    /// to a savepoint drops the cursors opened since (AtSubAbort_Portals).
    pub subxact: usize,
    /// The relations its query reads (its executor keeps them open while it runs).
    pub rels: Vec<u32>,
    /// Its query locks rows (FOR UPDATE / SHARE).
    pub row_marks: bool,
    /// Without row locks, when its plan as PostgreSQL builds it decides it (a sort, an
    /// Append: `explain::cursor_plan`): the tables `WHERE CURRENT OF` finds a row of, once
    /// per scan (search_plan_tree).
    pub simple: Option<Vec<u32>>,
}

#[derive(Clone, Copy, PartialEq)]
enum LimState {
    Initial,
    Empty,
    InWindow,
    WindowEnd,
    WindowStart,
    SubplanEof,
}

/// A cursor's query, run as its rows are fetched: opening only starts it, so what the query
/// does (functions it calls, errors it raises) happens at the FETCH that reaches the row.
pub struct Lazy {
    plan: Arc<Plan>,
    node: Option<crate::exec::BoxNode>,
    /// It has produced all its rows.
    done: bool,
    /// The target list, evaluated anew each time the executor passes a row (`plan` is what
    /// it is over).
    project: Option<Vec<Expr>>,
    /// The LIMIT count under `project`: backing up from the end of its window re-returns the
    /// row it last returned, as PostgreSQL's Limit node does.
    window: Option<Expr>,
    /// The WHERE qual of the scan under the target list, when it is volatile: evaluated anew
    /// each time the scan passes a row, back and forth (ExecScan), so the rows kept are the
    /// scan's; rows failing it are passed over.
    qual: Option<Expr>,
    /// The LIMIT over such a scan (its count and offset), applied as rows qualify.
    qual_limit: Option<(Option<Expr>, Option<Expr>)>,
    /// It writes: it runs to completion at the first FETCH or MOVE (PORTAL_ONE_MOD_WITH).
    fill: bool,
    /// Its parameters ($n).
    params: Vec<Value>,
    /// The PL/pgSQL variables it reads, as of OPEN.
    outer: Rc<Row>,
    /// Casts of its columns to the cursor's.
    coerce: Option<Vec<Expr>>,
    kept: crate::exec::Kept,
    /// The transaction as of OPEN: the query doesn't see later writes.
    snap: Rc<crate::txn::Snap>,
}

impl Lazy {
    /// Its plan sorts and it hasn't finished: a tuplesort is alive under the portal
    /// (pg_backend_memory_contexts shows its contexts).
    pub fn sorting(&self) -> bool {
        !self.done && format!("{:?}", self.plan).contains("Sort {")
    }

    /// `kept`: what the opening context had for the query (its ephemeral relations).
    pub fn new(
        ctx: &ExecCtx,
        plan: Arc<Plan>,
        outer: Rc<Row>,
        coerce: Option<Vec<Expr>>,
        kept: crate::exec::Kept,
    ) -> Lazy {
        Lazy {
            plan,
            node: None,
            done: false,
            project: None,
            window: None,
            qual: None,
            qual_limit: None,
            fill: false,
            params: Vec::new(),
            outer,
            coerce,
            kept,
            snap: ctx.cursor_snapshot(),
        }
    }

    /// With parameters ($n) for the query.
    pub fn with_params(mut self, params: Vec<Value>) -> Lazy {
        self.params = params;
        self
    }

    /// Evaluating the query's target list each time a row is passed (as the executor does
    /// scanning back and forth); a query that writes runs to completion at the first fetch
    /// instead.
    pub fn for_cursor(mut self) -> Lazy {
        if self.plan.writes() {
            self.fill = true;
            return self;
        }
        let (plan, exprs) = match split_target(&self.plan) {
            Some((plan, exprs)) => (plan, Some(exprs)),
            None => ((*self.plan).clone(), None),
        };
        if let Some((scan, qual, limit)) = split_qual(&plan, false) {
            self.qual = Some(qual);
            self.qual_limit = limit;
            self.project = exprs;
            self.plan = Arc::new(scan);
        } else if let Some(exprs) = exprs {
            if let Plan::Limit { limit, .. } = &plan {
                self.window = limit.clone();
            }
            self.project = Some(exprs);
            self.plan = Arc::new(plan);
        }
        self
    }

    /// The query's next rows, at most `n` (0: the rest), appended to `out` as they are made
    /// (what a protocol portal's Execute returns: PortalRunSelect); true once it has none
    /// left.
    pub fn next_rows(&mut self, ctx: &ExecCtx, n: usize, out: &mut Vec<Row>) -> Result<bool> {
        self.each_row(ctx, n, |r, _| {
            out.push(r);
            Ok(())
        })
    }

    /// `next_rows`, each row handed to `f` as the executor returns it (with the context the
    /// query runs in).
    pub fn each_row(
        &mut self,
        ctx: &ExecCtx,
        n: usize,
        mut f: impl FnMut(Row, &ExecCtx) -> Result<()>,
    ) -> Result<bool> {
        self.within(ctx, |ex, ctx| {
            let mut got = 0;
            while n == 0 || got < n {
                match ex.pull(ctx)? {
                    Some(r) => {
                        f(ex.coerce(r, ctx)?, ctx)?;
                        got += 1;
                    }
                    None => break,
                }
            }
            Ok(*ex.done)
        })
    }

    /// Run `f` in the query's context: its snapshot, parameters, variables and caches.
    fn within<T>(
        &mut self,
        ctx: &ExecCtx,
        f: impl FnOnce(&mut Exec<'_>, &ExecCtx) -> Result<T>,
    ) -> Result<T> {
        let Lazy {
            plan,
            node,
            done,
            project,
            window,
            qual,
            qual_limit,
            fill,
            params,
            outer,
            coerce,
            kept,
            snap,
        } = self;
        let child = ctx.child(params);
        child.restore(std::mem::take(kept));
        let mut ex = Exec {
            plan: &*plan,
            node,
            done,
            project: project.as_deref(),
            window: window.as_ref(),
            qual: qual.as_ref(),
            qual_limit: qual_limit.as_ref(),
            fill: *fill,
            coerce: coerce.as_deref(),
        };
        let r = child.with_snapshot(snap.clone(), || {
            child.with_outer(outer.clone(), || f(&mut ex, &child))
        });
        *kept = child.keep();
        ctx.notices
            .borrow_mut()
            .extend(child.notices.borrow_mut().drain(..));
        r
    }
}

/// The query's target list apart from what it is over (`Lazy::project`). Volatile target
/// entries the ORDER BY doesn't use are evaluated after the sort (and above a LIMIT's input),
/// as PostgreSQL's planner postpones them.
fn split_target(plan: &Plan) -> Option<(Plan, Vec<Expr>)> {
    fn go(p: &Plan) -> Option<(Plan, Vec<Expr>)> {
        match p {
            Plan::Project { input, exprs } if exprs.iter().any(|e| e.is_volatile()) => {
                Some(((**input).clone(), exprs.clone()))
            }
            Plan::Sort { input, keys } => {
                let Plan::Project { input, exprs } = &**input else {
                    return None;
                };
                let used = keys.iter().flat_map(|k| k.expr.columns());
                if !exprs.iter().any(|e| e.is_volatile())
                    || used.into_iter().any(|i| exprs.get(i).is_none_or(|e| e.is_volatile()))
                {
                    return None;
                }
                let keys = keys
                    .iter()
                    .map(|k| {
                        let mut k = k.clone();
                        k.expr = k
                            .expr
                            .transform(&mut |e| {
                                Ok(match e {
                                    Expr::Col(i, _) => exprs[i].clone(),
                                    e => e,
                                })
                            })
                            .ok()?;
                        Some(k)
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some((
                    Plan::Sort {
                        input: input.clone(),
                        keys,
                    },
                    exprs.clone(),
                ))
            }
            Plan::Limit {
                input,
                limit,
                offset,
                with_ties,
            } if with_ties.is_none() => {
                let (inner, exprs) = go(input)?;
                Some((
                    Plan::Limit {
                        input: Arc::new(inner),
                        limit: limit.clone(),
                        offset: offset.clone(),
                        with_ties: None,
                    },
                    exprs,
                ))
            }
            _ => None,
        }
    }
    match plan {
        Plan::Project { input, exprs } => Some(((**input).clone(), exprs.clone())),
        p => go(p),
    }
}

/// The scan under a cursor's target list and its volatile WHERE qual, which a scan node
/// evaluates as it passes rows in either direction (a sequential or index scan, a function
/// or VALUES scan); with the count and offset of a LIMIT over it.
#[allow(clippy::type_complexity)]
fn split_qual(
    plan: &Plan,
    under_limit: bool,
) -> Option<(Plan, Expr, Option<(Option<Expr>, Option<Expr>)>)> {
    let leaf = |p: &Plan| {
        matches!(p, Plan::FunctionScan { .. } | Plan::Values { .. })
            || matches!(p, Plan::Scan(s) if s.mark.is_none() && s.limit.is_none() && s.sample.is_none())
    };
    match plan {
        Plan::Scan(s)
            if s.filter.as_ref().is_some_and(|f| f.is_volatile())
                && s.mark.is_none()
                && s.sample.is_none()
                && (s.limit.is_none() || under_limit) =>
        {
            let mut s = s.clone();
            let qual = s.filter.take()?;
            s.limit = None;
            Some((Plan::Scan(s), qual, None))
        }
        Plan::Filter { input, pred } if pred.is_volatile() && leaf(input) => {
            Some(((**input).clone(), pred.clone(), None))
        }
        Plan::Limit {
            input,
            limit,
            offset,
            with_ties: None,
        } if !under_limit => {
            let (scan, qual, _) = split_qual(input, true)?;
            Some((scan, qual, Some((limit.clone(), offset.clone()))))
        }
        _ => None,
    }
}

/// A cursor's executor while it runs (see `Lazy::within`).
struct Exec<'l> {
    plan: &'l Arc<Plan>,
    node: &'l mut Option<crate::exec::BoxNode>,
    done: &'l mut bool,
    project: Option<&'l [Expr]>,
    window: Option<&'l Expr>,
    qual: Option<&'l Expr>,
    qual_limit: Option<&'l (Option<Expr>, Option<Expr>)>,
    fill: bool,
    coerce: Option<&'l [Expr]>,
}

impl Exec<'_> {
    /// The query's next row (under the target list, when that is evaluated apart); None:
    /// it is done.
    fn pull(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if *self.done {
            return Ok(None);
        }
        if self.node.is_none() {
            *self.node = Some(crate::exec::nodes::open(self.plan, ctx)?);
        }
        ctx.check()?;
        let node = self.node.as_mut().expect("opened");
        match node.next(ctx)? {
            Some(r) => Ok(Some(r)),
            None => {
                *self.done = true;
                *self.node = None;
                Ok(None)
            }
        }
    }

    /// Casts to the cursor's columns.
    fn coerce(&self, r: Row, ctx: &ExecCtx) -> Result<Row> {
        match self.coerce {
            Some(cs) => cs
                .iter()
                .map(|e| crate::sql::expr::eval(e, &r, ctx))
                .collect(),
            None => Ok(r),
        }
    }
}

/// FETCH / MOVE (`dest`: FETCH) on the cursor `name`, as PostgreSQL runs it: the rows
/// fetched, the cursor's columns, and how many rows it fetched or moved over. None: no such
/// cursor.
pub fn fetch(
    ctx: &ExecCtx,
    name: &str,
    dir: pb::FetchDirection,
    count: i64,
    dest: bool,
) -> Result<Option<(Vec<Row>, Vec<OutCol>, u64)>> {
    // The query may use cursors itself: it runs with this one taken out.
    let Some(mut c) = ctx.sess.cursors.borrow_mut().remove(name) else {
        return Ok(None);
    };
    let r = if c.failed {
        Err(Error::new(
            code::OBJECT_NOT_IN_PREREQUISITE_STATE,
            format!("portal \"{name}\" cannot be run"),
        ))
    } else {
        let r = c.with_executor(ctx, |c, mut ex, ctx| {
            // A query that writes runs to completion first.
            if let Some(ex) = ex.as_deref_mut().filter(|e| e.fill) {
                while let Some(r) = ex.pull(ctx)? {
                    let r = ex.coerce(r, ctx)?;
                    c.push_row(r);
                }
            }
            c.run_fetch(ex, ctx, dir, count, dest)
        });
        if r.is_err() {
            c.failed = true;
        }
        r
    };
    // A cursor stopped inside a temporary table's scan keeps its page pinned in the
    // session's local buffers (temp_buffers of them): with every one pinned, the next scan
    // finds none to use.
    let r = match r {
        Ok(x) if pins_local_buffer(&c, ctx) => {
            let pinned = ctx
                .sess
                .cursors
                .borrow()
                .values()
                .filter(|o| pins_local_buffer(o, ctx))
                .count();
            if pinned >= temp_buffers(ctx) {
                Err(Error::new(
                    "53000",
                    "no empty local buffer available",
                ))
            } else {
                Ok(x)
            }
        }
        r => r,
    };
    let cols = c.cols.clone();
    ctx.sess.cursors.borrow_mut().insert(name.to_string(), c);
    let (rows, n) = r?;
    Ok(Some((rows, cols, n)))
}

/// Whether the cursor's executor is stopped on a row of a temporary table's scan.
fn pins_local_buffer(c: &Cursor, ctx: &ExecCtx) -> bool {
    if c.pos == 0 || c.lazy.as_ref().is_none_or(|l| l.done) || c.rels.is_empty() {
        return false;
    }
    let cat = ctx.catalog();
    c.rels.iter().any(|r| {
        cat.tables
            .get(r)
            .is_some_and(|t| cat.schema_name(t.schema).starts_with("pg_temp"))
    })
}

/// temp_buffers, in buffers.
fn temp_buffers(ctx: &ExecCtx) -> usize {
    let raw = ctx.sess.settings.borrow().get_raw("temp_buffers");
    let raw = raw.as_deref().unwrap_or("1024").trim();
    let digits: String = raw.chars().take_while(|c| c.is_ascii_digit()).collect();
    let n: usize = digits.parse().unwrap_or(1024);
    match raw[digits.len()..].trim() {
        "kB" => n / 8,
        "MB" => n * 128,
        "GB" => n * 128 * 1024,
        _ => n,
    }
}

/// Whether `persist_for_commit` has a cursor to run into a store.
pub fn any_to_persist(sess: &crate::session::state::SessionState, commit: bool) -> bool {
    sess.cursors.borrow().values().any(|c| persisted(c, commit))
}

fn persisted(c: &Cursor, commit: bool) -> bool {
    (c.pinned || (commit && c.hold)) && c.lazy.is_some() && !c.failed
}

/// Before the transaction commits, holdable cursors (and those PL/pgSQL loops read, over a
/// COMMIT or ROLLBACK in a procedure) are run into a store (PersistHoldablePortal).
pub fn persist_for_commit(ctx: &ExecCtx, commit: bool) -> Result<()> {
    let names: Vec<String> = ctx
        .sess
        .cursors
        .borrow()
        .iter()
        .filter(|(_, c)| persisted(c, commit))
        .map(|(n, _)| n.clone())
        .collect();
    for name in names {
        let Some(mut c) = ctx.sess.cursors.borrow_mut().remove(&name) else {
            continue;
        };
        let r = c.persist(ctx);
        ctx.sess.cursors.borrow_mut().insert(name, c);
        r?;
    }
    Ok(())
}

fn backward_err() -> Error {
    Error::new(
        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
        "cursor can only scan forward",
    )
    .hint("Declare it with SCROLL option to enable backward scan.")
}

impl Cursor {
    /// Open over `rows`; with `key_table`, each row's last column is its storage key.
    pub fn new(
        cols: Vec<OutCol>,
        rows: Vec<Row>,
        key_table: Option<u32>,
        hold: bool,
        scroll: bool,
        text: String,
        created: i64,
    ) -> Cursor {
        let mut c = Cursor {
            cols,
            rows: Vec::new(),
            keys: key_table.map(|t| (t, Vec::new())),
            pos: 0,
            portal_pos: 0,
            at_start: true,
            at_end: false,
            hold,
            pinned: false,
            scroll,
            text,
            created,
            lazy: None,
            projected: false,
            last: None,
            failed: false,
            lim_pos: 0,
            lim_state: LimState::Initial,
            subxact: 0,
            rels: Vec::new(),
            row_marks: false,
            simple: None,
        };
        for r in rows {
            c.push_row(r);
        }
        c
    }

    /// Open over a query run as its rows are fetched (`key_table` as for `new`).
    pub fn new_lazy(
        cols: Vec<OutCol>,
        lazy: Lazy,
        key_table: Option<u32>,
        hold: bool,
        scroll: bool,
        text: String,
        created: i64,
    ) -> Cursor {
        let mut c = Cursor::new(cols, Vec::new(), key_table, hold, scroll, text, created);
        c.projected = lazy.project.is_some();
        c.lazy = Some(lazy);
        c
    }

    /// A row the query produced, after those it had: with its storage key last, unless the
    /// target list (which has the key) is evaluated as rows are passed.
    fn push_row(&mut self, mut r: Row) {
        let projected = self.projected;
        if let Some((_, keys)) = &mut self.keys {
            let key = if projected { None } else { r.pop() };
            keys.push(match key {
                Some(Value::Bytea(k)) => k,
                Some(Value::Array(a)) if a.elem == DataType::Bytea => multi_key(&a.items),
                Some(v) => {
                    r.push(v);
                    Arc::from(&[][..])
                }
                None => Arc::from(&[][..]),
            });
        }
        self.rows.push(r);
    }

    /// Run `f` with the query's executor (and in its context), if it has one.
    fn with_executor<T>(
        &mut self,
        ctx: &ExecCtx,
        f: impl FnOnce(&mut Cursor, Option<&mut Exec<'_>>, &ExecCtx) -> Result<T>,
    ) -> Result<T> {
        let Some(mut lazy) = self.lazy.take() else {
            return f(self, None, ctx);
        };
        let r = lazy.within(ctx, |ex, ctx| f(self, Some(ex), ctx));
        // Keep the executor while it can produce rows, or evaluates their target list or
        // qual.
        self.lazy = (self.projected || lazy.qual.is_some() || !lazy.done).then_some(lazy);
        r
    }

    /// The executor's row at its position, as returned: its target list evaluated again
    /// when that runs apart.
    fn visit(&mut self, ex: Option<&mut Exec<'_>>, ctx: &ExecCtx) -> Result<Row> {
        let i = self.pos - 1;
        let Some(ex) = ex.filter(|e| e.project.is_some() || e.qual.is_some()) else {
            return Ok(self.rows[i].clone());
        };
        let Some(project) = ex.project else {
            // The scan's rows as they are (a qual is re-evaluated apart).
            let r = ex.coerce(self.rows[i].clone(), ctx)?;
            self.last = Some(r.clone());
            return Ok(r);
        };
        let mut r = crate::exec::eval_all(project, &self.rows[i], ctx)?;
        if let Some((_, keys)) = &mut self.keys {
            match r.pop() {
                Some(Value::Bytea(k)) => keys[i] = k,
                Some(Value::Array(a)) if a.elem == DataType::Bytea => {
                    keys[i] = multi_key(&a.items)
                }
                _ => {}
            }
        }
        let r = ex.coerce(r, ctx)?;
        self.last = Some(r.clone());
        Ok(r)
    }

    /// Whether the executor stopped at the end of its LIMIT's window (not its input's end).
    fn at_window_end(&self, ex: &Exec<'_>, ctx: &ExecCtx) -> Result<bool> {
        let Some(e) = ex.window else {
            return Ok(false);
        };
        let n = self.rows.len();
        if n == 0 || self.pos != n + 1 || self.last.is_none() {
            return Ok(false);
        }
        Ok(match crate::sql::expr::eval(e, &[], ctx)? {
            Value::Int8(c) => c == n as i64,
            Value::Int4(c) => c as i64 == n as i64,
            _ => false,
        })
    }

    /// Move the executor one row over a scan whose qual is evaluated as it passes rows
    /// (`Lazy::qual`), under the LIMIT over it if any (ExecLimit's state machine).
    fn step_qual(
        &mut self,
        mut ex: Option<&mut Exec<'_>>,
        ctx: &ExecCtx,
        forward: bool,
    ) -> Result<Option<Row>> {
        let (qual, limit) = match ex.as_deref() {
            Some(e) => (e.qual.cloned(), e.qual_limit.cloned()),
            None => (None, None),
        };
        let qual = qual.expect("a qual");
        let Some((count, offset)) = limit else {
            return self.qual_move(ex, ctx, forward, &qual);
        };
        let value = |e: &Option<Expr>| -> Result<Option<i64>> {
            Ok(match e {
                Some(e) => match crate::sql::expr::eval(e, &[], ctx)? {
                    Value::Int8(n) => Some(n),
                    Value::Int4(n) => Some(n as i64),
                    _ => None,
                },
                None => None,
            })
        };
        let count = value(&count)?;
        let offset = value(&offset)?.unwrap_or(0).max(0) as u64;
        match (self.lim_state, forward) {
            (LimState::Initial, false) | (LimState::Empty, _) => Ok(None),
            (LimState::Initial, true) => {
                if count.is_some_and(|c| c <= 0) {
                    self.lim_state = LimState::Empty;
                    return Ok(None);
                }
                // The rows up to OFFSET pass, then the window's first.
                loop {
                    let Some(r) = self.qual_move(ex.as_deref_mut(), ctx, true, &qual)? else {
                        self.lim_state = LimState::Empty;
                        return Ok(None);
                    };
                    self.lim_pos += 1;
                    if self.lim_pos > offset {
                        self.lim_state = LimState::InWindow;
                        return Ok(Some(r));
                    }
                }
            }
            (LimState::InWindow, true) => {
                if count.is_some_and(|c| self.lim_pos - offset >= c.max(0) as u64) {
                    self.lim_state = LimState::WindowEnd;
                    return Ok(None);
                }
                match self.qual_move(ex, ctx, true, &qual)? {
                    Some(r) => {
                        self.lim_pos += 1;
                        Ok(Some(r))
                    }
                    None => {
                        self.lim_state = LimState::SubplanEof;
                        Ok(None)
                    }
                }
            }
            (LimState::InWindow, false) => {
                if self.lim_pos <= offset + 1 {
                    self.lim_state = LimState::WindowStart;
                    return Ok(None);
                }
                let r = self.qual_move(ex, ctx, false, &qual)?;
                self.lim_pos -= 1;
                Ok(r)
            }
            (LimState::SubplanEof, true)
            | (LimState::WindowEnd, true)
            | (LimState::WindowStart, false) => Ok(None),
            // Backing up from the input's end: the row before it again.
            (LimState::SubplanEof, false) => {
                let r = self.qual_move(ex, ctx, false, &qual)?;
                self.lim_state = LimState::InWindow;
                Ok(r)
            }
            // Stepping back off the window's end, or forward off its start: the row it was
            // on again, as it was.
            (LimState::WindowEnd, false) | (LimState::WindowStart, true) => {
                self.lim_state = LimState::InWindow;
                Ok(self.last.clone())
            }
        }
    }

    /// The scan's next row in a direction that passes its qual, as returned.
    fn qual_move(
        &mut self,
        mut ex: Option<&mut Exec<'_>>,
        ctx: &ExecCtx,
        forward: bool,
        qual: &Expr,
    ) -> Result<Option<Row>> {
        loop {
            let n = self.rows.len();
            if forward {
                if self.pos > n {
                    return Ok(None);
                }
                if self.pos == n {
                    let more = match ex.as_deref_mut() {
                        Some(e) => e.pull(ctx)?,
                        None => None,
                    };
                    let Some(r) = more else {
                        self.pos = n + 1;
                        return Ok(None);
                    };
                    self.push_row(r);
                }
                self.pos += 1;
            } else {
                if self.pos == 0 {
                    return Ok(None);
                }
                self.pos -= 1;
                if self.pos == 0 {
                    return Ok(None);
                }
            }
            if matches!(
                crate::sql::expr::eval(qual, &self.rows[self.pos - 1], ctx)?,
                Value::Bool(true)
            ) {
                break;
            }
        }
        self.visit(ex, ctx).map(Some)
    }

    /// Move the executor one row: the row it lands on, if any.
    fn step(
        &mut self,
        mut ex: Option<&mut Exec<'_>>,
        ctx: &ExecCtx,
        forward: bool,
    ) -> Result<Option<Row>> {
        if ex.as_deref().is_some_and(|e| e.qual.is_some()) {
            return self.step_qual(ex, ctx, forward);
        }
        let n = self.rows.len();
        if forward {
            if self.pos > n {
                return Ok(None);
            }
            if self.pos == n {
                let more = match ex.as_deref_mut() {
                    Some(e) => match e.pull(ctx)? {
                        Some(r) if e.project.is_none() => Some(e.coerce(r, ctx)?),
                        r => r,
                    },
                    None => None,
                };
                let Some(r) = more else {
                    self.pos = n + 1;
                    return Ok(None);
                };
                self.push_row(r);
            }
            self.pos += 1;
        } else {
            if self.pos == 0 {
                return Ok(None);
            }
            if let Some(e) = ex.as_deref()
                && self.at_window_end(e, ctx)?
            {
                self.pos -= 1;
                return Ok(self.last.clone());
            }
            self.pos -= 1;
            if self.pos == 0 {
                return Ok(None);
            }
        }
        self.visit(ex, ctx).map(Some)
    }

    /// PortalRunSelect: up to `count` rows in one direction (FETCH_ALL: all); returned when
    /// `dest`. With how many it processed.
    fn select(
        &mut self,
        mut ex: Option<&mut Exec<'_>>,
        ctx: &ExecCtx,
        forward: bool,
        count: i64,
        dest: bool,
    ) -> Result<(Vec<Row>, u64)> {
        let mut out = Vec::new();
        if !forward && !self.scroll {
            return Err(backward_err());
        }
        if (if forward { self.at_end } else { self.at_start }) || count <= 0 {
            return Ok((out, 0));
        }
        let mut n: u64 = 0;
        while count == FETCH_ALL || n < count as u64 {
            let Some(r) = self.step(ex.as_deref_mut(), ctx, forward)? else {
                break;
            };
            n += 1;
            if dest {
                out.push(r);
            }
        }
        let all = count == FETCH_ALL || n < count as u64;
        if forward {
            if n > 0 {
                self.at_start = false;
            }
            if all {
                self.at_end = true;
            }
            self.portal_pos += n;
        } else {
            if n > 0 && self.at_end {
                self.at_end = false;
                self.portal_pos += 1;
            }
            if all {
                self.at_start = true;
                self.portal_pos = 0;
            } else {
                self.portal_pos -= n;
            }
        }
        Ok((out, n))
    }

    /// DoPortalRewind: back before the first row (nothing is evaluated).
    fn rewind(&mut self) -> Result<()> {
        if self.at_start && !self.at_end {
            return Ok(());
        }
        if !self.scroll {
            return Err(backward_err());
        }
        self.pos = 0;
        self.at_start = true;
        self.at_end = false;
        self.portal_pos = 0;
        self.lim_pos = 0;
        self.lim_state = LimState::Initial;
        Ok(())
    }

    /// DoPortalRunFetch: FETCH / MOVE (`count` as FetchStmt's `how_many`), turned into
    /// executor runs the way PostgreSQL does, rows passed on the way evaluated too.
    fn run_fetch(
        &mut self,
        mut ex: Option<&mut Exec<'_>>,
        ctx: &ExecCtx,
        dir: pb::FetchDirection,
        count: i64,
        dest: bool,
    ) -> Result<(Vec<Row>, u64)> {
        use pb::FetchDirection as D;
        let (mut forward, mut count) = match dir {
            D::FetchBackward => (count < 0, count.saturating_abs()),
            D::FetchAbsolute => {
                if count > 0 {
                    if count - 1 <= (self.portal_pos / 2) as i64 {
                        self.rewind()?;
                        if count > 1 {
                            self.select(ex.as_deref_mut(), ctx, true, count - 1, false)?;
                        }
                    } else {
                        let mut pos = self.portal_pos as i64;
                        if self.at_end {
                            pos += 1;
                        }
                        if count <= pos {
                            self.select(ex.as_deref_mut(), ctx, false, pos - count + 1, false)?;
                        } else if count > pos + 1 {
                            self.select(ex.as_deref_mut(), ctx, true, count - pos - 1, false)?;
                        }
                    }
                    return self.select(ex, ctx, true, 1, dest);
                }
                if count < 0 {
                    self.select(ex.as_deref_mut(), ctx, true, FETCH_ALL, false)?;
                    if count < -1 {
                        self.select(ex.as_deref_mut(), ctx, false, -count - 1, false)?;
                    }
                    return self.select(ex, ctx, false, 1, dest);
                }
                self.rewind()?;
                return self.select(ex, ctx, true, 0, dest);
            }
            D::FetchRelative => {
                if count > 0 {
                    if count > 1 {
                        self.select(ex.as_deref_mut(), ctx, true, count - 1, false)?;
                    }
                    return self.select(ex, ctx, true, 1, dest);
                }
                if count < 0 {
                    if count < -1 {
                        self.select(ex.as_deref_mut(), ctx, false, -count - 1, false)?;
                    }
                    return self.select(ex, ctx, false, 1, dest);
                }
                (true, 0)
            }
            _ => (count >= 0, count.saturating_abs()),
        };
        // FETCH 0: the current row again, by backing up one and fetching it.
        if count == 0 {
            let on_row = !self.at_start && !self.at_end;
            if !dest {
                return Ok((Vec::new(), on_row as u64));
            }
            if on_row {
                self.select(ex.as_deref_mut(), ctx, false, 1, false)?;
                count = 1;
                forward = true;
            }
        }
        // MOVE BACKWARD ALL rewinds.
        if !forward && count == FETCH_ALL && !dest {
            let mut n = self.portal_pos;
            if n > 0 && !self.at_end {
                n -= 1;
            }
            self.rewind()?;
            return Ok((Vec::new(), n));
        }
        self.select(ex, ctx, forward, count, dest)
    }

    /// PersistHoldablePortal: run the query into a store, all of it again for a SCROLL
    /// cursor (its target list evaluated anew), else what is left; the position kept.
    fn persist(&mut self, ctx: &ExecCtx) -> Result<()> {
        if self.lazy.is_none() {
            return Ok(());
        }
        let (rows, keys) = self.with_executor(ctx, |c, mut ex, ctx| {
            if c.scroll {
                c.pos = 0;
                c.lim_pos = 0;
                c.lim_state = LimState::Initial;
            } else if c.at_end {
                c.pos = c.rows.len() + 1;
            }
            let mut rows = Vec::new();
            let mut keys = Vec::new();
            while let Some(r) = c.step(ex.as_deref_mut(), ctx, true)? {
                rows.push(r);
                if let Some((_, k)) = &c.keys {
                    keys.push(k[c.pos - 1].clone());
                }
            }
            Ok((rows, keys))
        })?;
        self.lazy = None;
        self.projected = false;
        self.last = None;
        self.rows = rows;
        if let Some((_, k)) = &mut self.keys {
            *k = keys;
        }
        self.pos = if self.at_end {
            self.rows.len() + 1
        } else if self.scroll {
            self.portal_pos as usize
        } else {
            0
        };
        Ok(())
    }

    /// The storage key of the current row in `table` (`WHERE CURRENT OF`).
    /// The storage key of the row the cursor is on, for `WHERE CURRENT OF` in a scan of
    /// `table` (execCurrentOf); None when that row is another member's of the inheritance
    /// tree the cursor reads.
    pub fn current_key(
        &self,
        name: &str,
        table: u32,
        table_name: &str,
        cat: &crate::catalog::Catalog,
    ) -> Result<Option<Arc<[u8]>>> {
        // A holdable cursor's rows are a store once its transaction has committed; a failed
        // portal has no executor either (execCurrentOf: no QueryDesc).
        if (self.hold && self.lazy.is_none()) || self.failed {
            return Err(Error::new(
                code::INVALID_CURSOR_STATE,
                format!("cursor \"{name}\" is held from a previous transaction"),
            ));
        }
        let not_simple = || {
            Error::new(
                code::INVALID_CURSOR_STATE,
                if self.row_marks {
                    format!(
                        "cursor \"{name}\" does not have a FOR UPDATE/SHARE reference to table \"{table_name}\""
                    )
                } else {
                    format!(
                        "cursor \"{name}\" is not a simply updatable scan of table \"{table_name}\""
                    )
                },
            )
        };
        let Some((t, keys)) = &self.keys else {
            return Err(not_simple());
        };
        if let Some(simple) = &self.simple
            && simple.iter().filter(|o| **o == table).count() != 1
        {
            return Err(not_simple());
        }
        // Several locking scans (`marked_keyed_plan`): the one of this table.
        let of_table = |k: &[u8]| {
            k.get(..4).is_some_and(|b| {
                let o = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                o == table || cat.inheritors(o).contains(&table) || cat.inheritors(table).contains(&o)
            })
        };
        if *t == 0 {
            let marked = |k: &[u8]| split_multi_key(k).into_iter().filter(|x| of_table(x)).count();
            let any = keys.iter().find(|k| k.starts_with(&MULTI_KEY));
            match any.map(|k| marked(k)) {
                Some(0) => return Err(not_simple()),
                None | Some(1) => {}
                Some(_) => {
                    return Err(Error::new(
                        code::INVALID_CURSOR_STATE,
                        format!(
                            "cursor \"{name}\" has multiple FOR UPDATE/SHARE references to table \"{table_name}\""
                        ),
                    ));
                }
            }
        // (A plan that decided it, `simple`, found the table's scan: an Append's member.)
        } else if self.simple.is_none() && *t != table && !cat.inheritors(*t).contains(&table) {
            return Err(not_simple());
        }
        if self.pos == 0 || self.pos > keys.len() {
            return Err(Error::new(
                code::INVALID_CURSOR_STATE,
                format!("cursor \"{name}\" is not positioned on a row"),
            ));
        }
        let key = keys[self.pos - 1].clone();
        let key: Arc<[u8]> = if key.starts_with(&MULTI_KEY) {
            match split_multi_key(&key).into_iter().find(|k| of_table(k)) {
                Some(k) => Arc::from(k),
                None => return Err(not_simple()),
            }
        } else {
            key
        };
        let of = key
            .get(..4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
        Ok((of.is_none_or(|o| o == table)).then_some(key))
    }
}

/// Whether a cursor over `plan` is scrollable when declared without [NO] SCROLL: PostgreSQL's
/// `ExecSupportsBackwardScan`, read off the plan PostgreSQL would build, and no row locks.
pub fn scrollable(plan: &Plan) -> bool {
    fn go(p: &Plan) -> bool {
        match p {
            // (A bitmap heap scan can't; an index scan as its access method says,
            // amcanbackward.)
            Plan::Scan(s) => {
                s.mark.is_none()
                    && match &s.access {
                        crate::sql::plan::Access::Bitmap(..) => false,
                        crate::sql::plan::Access::Index { .. } => s
                            .index
                            .as_ref()
                            .is_none_or(|(ix, _)| matches!(ix.method.as_str(), "btree" | "hash")),
                        _ => true,
                    }
            }
            Plan::SysScan { .. }
            | Plan::FunctionScan { .. }
            | Plan::PlCall { .. }
            | Plan::CteScan { .. }
            | Plan::Sort { .. } => true,
            // A one-row VALUES is a Result.
            Plan::Values { rows, .. } => rows.len() > 1,
            Plan::Filter { input, .. }
            | Plan::Project { input, .. }
            | Plan::Limit { input, .. }
            | Plan::With { body: input, .. }
            | Plan::Barrier { input, .. } | Plan::Context { input, .. } => go(input),
            Plan::Append { inputs, .. } => inputs.iter().all(|i| go(i)),
            _ => false,
        }
    }
    go(plan)
}

/// Whether what a cursor over `plan` can do (`scrollable`, `keyed_plan`) depends on choices
/// only the plan PostgreSQL builds shows (`explain::cursor_plan`): a sort it may replace
/// with an ordered path (an index, a join, an incremental sort) or an Append it may merge.
pub fn plan_decides(plan: &Plan) -> bool {
    match plan {
        Plan::Sort { .. } | Plan::Append { .. } => true,
        Plan::Filter { input, .. }
        | Plan::Project { input, .. }
        | Plan::Limit { input, .. }
        | Plan::With { body: input, .. }
        | Plan::Barrier { input, .. }
        | Plan::Context { input, .. } => plan_decides(input),
        _ => false,
    }
}

/// `plan` changed to also return each row's storage key as a trailing column, when it is a
/// simple scan of one table (what `WHERE CURRENT OF` can address): the table's oid too.
pub fn keyed_plan(plan: &Plan) -> Option<(Plan, u32)> {
    // `sh`: the scan's row has system columns, which the key inserted at this position
    // moves up one: expressions over that row are renumbered.
    fn fix(e: &Expr, sh: Option<usize>) -> Expr {
        match sh {
            Some(n) => e.clone().remap_cols(&|i| if i >= n { i + 1 } else { i }),
            None => e.clone(),
        }
    }
    fn go(p: &Plan) -> Option<(Plan, u32, usize, Option<usize>)> {
        Some(match p {
            Plan::Scan(s) if !s.with_key => {
                let mut s = s.clone();
                s.with_key = true;
                let key = s.table.columns.len();
                let sh = s.sys.then_some(key);
                if sh.is_some() {
                    s.filter = s.filter.as_ref().map(|f| fix(f, sh));
                    s.access = match &s.access {
                        crate::sql::plan::Access::Tid { quals, removed } => {
                            crate::sql::plan::Access::Tid {
                                quals: quals.iter().map(|q| fix(q, sh)).collect(),
                                removed: removed.iter().map(|q| fix(q, sh)).collect(),
                            }
                        }
                        crate::sql::plan::Access::TidRange(q) => {
                            crate::sql::plan::Access::TidRange(q.iter().map(|q| fix(q, sh)).collect())
                        }
                        crate::sql::plan::Access::Seq => crate::sql::plan::Access::Seq,
                        crate::sql::plan::Access::Unplanned => crate::sql::plan::Access::Unplanned,
                        // (Index conditions over system columns: not renumbered here.)
                        _ => return None,
                    };
                }
                let oid = s.table.oid;
                (Plan::Scan(s), oid, key, sh)
            }
            Plan::Filter { input, pred } => {
                let (i, oid, key, sh) = go(input)?;
                (
                    Plan::Filter {
                        input: Arc::new(i),
                        pred: fix(pred, sh),
                    },
                    oid,
                    key,
                    sh,
                )
            }
            Plan::Project { input, exprs } => {
                let (i, oid, key, sh) = go(input)?;
                let mut exprs: Vec<Expr> = exprs.iter().map(|e| fix(e, sh)).collect();
                exprs.push(Expr::Col(key, DataType::Bytea));
                let at = exprs.len() - 1;
                (
                    Plan::Project {
                        input: Arc::new(i),
                        exprs,
                    },
                    oid,
                    at,
                    None,
                )
            }
            Plan::Sort { input, keys } => {
                let (i, oid, key, sh) = go(input)?;
                (
                    Plan::Sort {
                        input: Arc::new(i),
                        keys: keys
                            .iter()
                            .map(|k| crate::sql::plan::SortKey {
                                expr: fix(&k.expr, sh),
                                ..k.clone()
                            })
                            .collect(),
                    },
                    oid,
                    key,
                    sh,
                )
            }
            Plan::Limit {
                input,
                limit,
                offset,
                with_ties,
            } => {
                let (i, oid, key, sh) = go(input)?;
                (
                    Plan::Limit {
                        input: Arc::new(i),
                        limit: limit.clone(),
                        offset: offset.clone(),
                        with_ties: with_ties.clone(),
                    },
                    oid,
                    key,
                    sh,
                )
            }
            // An inheritance tree's scans, or a UNION ALL's (search_plan_tree descends an
            // Append): each row's key names its own table.
            Plan::Append { inputs, inh, .. } => {
                let mut out = Vec::with_capacity(inputs.len());
                let mut first: Option<(u32, usize)> = None;
                for p in inputs {
                    let (i, oid, key, sh) = go(p)?;
                    if sh.is_some() || first.is_some_and(|(_, k)| k != key) {
                        return None;
                    }
                    first.get_or_insert((oid, key));
                    out.push(Arc::new(i));
                }
                let (oid, key) = first?;
                (
                    Plan::Append {
                        inputs: out,
                        inh: inh.clone(),
                        alias: None,
                        pw: None,
                    },
                    oid,
                    key,
                    None,
                )
            }
            // The level's rows locked at its top: its locked relations' keys are there
            // already (a row mark's, execCurrentOf), one or several (`marked_keyed_plan`).
            Plan::LockRows {
                input,
                marks,
                width,
            } => {
                let locked: Vec<&crate::sql::plan::LockMark> =
                    marks.iter().filter(|m| m.lock.is_some()).collect();
                let (key, oid) = match locked.as_slice() {
                    [] => return None,
                    [m] => (Expr::Col(m.col, DataType::Bytea), m.tables.first()?.oid),
                    _ => (
                        Expr::Array(
                            locked.iter().map(|m| Expr::Col(m.col, DataType::Bytea)).collect(),
                            DataType::Array(Box::new(DataType::Bytea)),
                        ),
                        0,
                    ),
                };
                let mut exprs: Vec<Expr> = (0..*width).map(|i| Expr::Col(i, DataType::Text)).collect();
                exprs.push(key);
                let mut keyed = Vec::with_capacity(marks.len());
                for m in marks.iter() {
                    keyed.push(crate::sql::plan::LockMark {
                        col: exprs.len(),
                        ..m.clone()
                    });
                    exprs.push(Expr::Col(m.col, DataType::Bytea));
                }
                (
                    Plan::LockRows {
                        input: Arc::new(Plan::Project {
                            input: input.clone(),
                            exprs,
                        }),
                        marks: Arc::new(keyed),
                        width: width + 1,
                    },
                    oid,
                    *width,
                    None,
                )
            }
            _ => return None,
        })
    }
    let Some((p, oid, key, _)) = go(plan) else {
        return marked_keyed_plan(plan);
    };
    // The key must come last: project it there when the scan's own row is the output.
    let width = crate::sql::binder::plan_width(&p);
    if key + 1 == width {
        return Some((p, oid));
    }
    let mut exprs: Vec<Expr> = (0..width)
        .filter(|i| *i != key)
        .map(|i| Expr::Col(i, DataType::Text))
        .collect();
    exprs.push(Expr::Col(key, DataType::Bytea));
    Some((
        Plan::Project {
            input: Arc::new(p),
            exprs,
        },
        oid,
    ))
}

/// A query over a join locking rows (FOR UPDATE / SHARE): `plan` changed to also return the
/// storage keys of the rows its locking scans read, as a trailing array (oid 0), or as
/// one key when there is one such scan. `WHERE CURRENT OF` then takes the row of the table
/// it names (execCurrentOf's row marks).
fn marked_keyed_plan(plan: &Plan) -> Option<(Plan, u32)> {
    use crate::sql::binder::plan_width;
    /// The plan with the keys appended; old output position -> new; the keys' positions
    /// and tables.
    type Keyed = (Plan, Vec<usize>, Vec<(usize, u32)>);
    fn go(p: &Plan) -> Option<Keyed> {
        let ident = |n: usize| (0..n).collect::<Vec<usize>>();
        Some(match p {
            Plan::Scan(s) if s.mark.is_some() && !s.with_key && !s.sys => {
                let mut s = s.clone();
                s.with_key = true;
                let n = s.table.columns.len();
                let oid = s.table.oid;
                (Plan::Scan(s), ident(n), vec![(n, oid)])
            }
            Plan::Scan(_) | Plan::Values { .. } | Plan::FunctionScan { .. } | Plan::One => {
                (p.clone(), ident(plan_width(p)), Vec::new())
            }
            Plan::Filter { input, pred } => {
                let (i, map, keys) = go(input)?;
                let pred = pred.clone().remap_cols(&|c| map.get(c).copied().unwrap_or(c));
                (
                    Plan::Filter {
                        input: Arc::new(i),
                        pred,
                    },
                    map,
                    keys,
                )
            }
            Plan::Project { input, exprs } => {
                let (i, map, keys) = go(input)?;
                let mut out: Vec<Expr> = exprs
                    .iter()
                    .map(|e| e.clone().remap_cols(&|c| map.get(c).copied().unwrap_or(c)))
                    .collect();
                let n = out.len();
                let mut nkeys = Vec::with_capacity(keys.len());
                for (k, oid) in keys {
                    nkeys.push((out.len(), oid));
                    out.push(Expr::Col(k, DataType::Bytea));
                }
                (
                    Plan::Project {
                        input: Arc::new(i),
                        exprs: out,
                    },
                    ident(n),
                    nkeys,
                )
            }
            Plan::Sort { input, keys: sk } => {
                let (i, map, keys) = go(input)?;
                let sk = sk
                    .iter()
                    .map(|k| crate::sql::plan::SortKey {
                        expr: k.expr.clone().remap_cols(&|c| map.get(c).copied().unwrap_or(c)),
                        ..k.clone()
                    })
                    .collect();
                (
                    Plan::Sort {
                        input: Arc::new(i),
                        keys: sk,
                    },
                    map,
                    keys,
                )
            }
            Plan::Limit {
                input,
                limit,
                offset,
                with_ties: None,
            } => {
                let (i, map, keys) = go(input)?;
                (
                    Plan::Limit {
                        input: Arc::new(i),
                        limit: limit.clone(),
                        offset: offset.clone(),
                        with_ties: None,
                    },
                    map,
                    keys,
                )
            }
            // An inheritance tree's scans: each keyed alike (a key names its own table).
            Plan::Append {
                inputs,
                inh: inh @ Some(_),
                ..
            } => {
                let mut out = Vec::with_capacity(inputs.len());
                let mut first: Option<(Vec<usize>, Vec<(usize, u32)>)> = None;
                for i in inputs {
                    let (p, map, keys) = go(i)?;
                    match &first {
                        Some((m, k))
                            if *m != map
                                || k.iter().map(|x| x.0).ne(keys.iter().map(|x| x.0)) =>
                        {
                            return None;
                        }
                        Some(_) => {}
                        None => first = Some((map, keys)),
                    }
                    out.push(Arc::new(p));
                }
                let (map, keys) = first?;
                (
                    Plan::Append {
                        inputs: out,
                        inh: inh.clone(),
                        alias: None,
                        pw: None,
                    },
                    map,
                    keys,
                )
            }
            Plan::Join {
                kind,
                left,
                right,
                cond,
                hash,
                params,
                nleft,
                nright,
                meta,
            } => {
                use crate::sql::plan::JoinKind;
                let (l, lmap, lkeys) = go(left)?;
                let (r, rmap, rkeys) = go(right)?;
                let semi = matches!(kind, JoinKind::Semi | JoinKind::Anti);
                if semi && !rkeys.is_empty() {
                    return None;
                }
                let wl = plan_width(&l);
                let wr = plan_width(&r);
                let (nl, nr) = (*nleft, *nright);
                let comb = |c: usize| {
                    if c < nl {
                        lmap.get(c).copied().unwrap_or(c)
                    } else {
                        wl + rmap.get(c - nl).copied().unwrap_or(c - nl)
                    }
                };
                let lm = |c: usize| lmap.get(c).copied().unwrap_or(c);
                let rm = |c: usize| rmap.get(c).copied().unwrap_or(c);
                let cond = cond.clone().map(|e| e.remap_cols(&comb));
                let hash = hash.clone().map(|(a, b)| {
                    (
                        a.into_iter().map(|e| e.remap_cols(&lm)).collect(),
                        b.into_iter().map(|e| e.remap_cols(&rm)).collect(),
                    )
                });
                let params = params.iter().map(|e| e.clone().remap_cols(&lm)).collect();
                let map: Vec<usize> = (0..nl + if semi { 0 } else { nr }).map(comb).collect();
                let mut keys = lkeys;
                keys.extend(rkeys.into_iter().map(|(k, o)| (wl + k, o)));
                (
                    Plan::Join {
                        kind: *kind,
                        left: Arc::new(l),
                        right: Arc::new(r),
                        cond,
                        hash,
                        params,
                        nleft: wl,
                        nright: wr,
                        meta: meta.clone(),
                    },
                    map,
                    keys,
                )
            }
            _ => return None,
        })
    }
    let (p, map, keys) = go(plan)?;
    if keys.is_empty() {
        return None;
    }
    let mut exprs: Vec<Expr> = map.iter().map(|&c| Expr::Col(c, DataType::Text)).collect();
    let oid = match keys.as_slice() {
        [(k, oid)] => {
            exprs.push(Expr::Col(*k, DataType::Bytea));
            *oid
        }
        _ => {
            let ty = DataType::Array(Box::new(DataType::Bytea));
            exprs.push(Expr::Array(
                keys.iter().map(|(k, _)| Expr::Col(*k, DataType::Bytea)).collect(),
                ty,
            ));
            0
        }
    };
    Some((
        Plan::Project {
            input: Arc::new(p),
            exprs,
        },
        oid,
    ))
}

/// A row's storage keys of several locking scans, as one key (`marked_keyed_plan`): a
/// marker no table's key starts with (oid 0xFFFFFFFF), then each key length-prefixed.
const MULTI_KEY: [u8; 4] = [0xFF; 4];

fn multi_key(items: &[Value]) -> Arc<[u8]> {
    let mut out = MULTI_KEY.to_vec();
    for v in items {
        let k: &[u8] = match v {
            Value::Bytea(k) => k,
            _ => &[],
        };
        out.extend_from_slice(&(k.len() as u32).to_be_bytes());
        out.extend_from_slice(k);
    }
    Arc::from(out)
}

fn split_multi_key(key: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut rest = &key[MULTI_KEY.len()..];
    while rest.len() >= 4 {
        let n = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        let Some(k) = rest.get(4..4 + n) else { break };
        out.push(k);
        rest = &rest[4 + n..];
    }
    out
}

/// DECLARE / FETCH / MOVE / CLOSE run through SPI (a SQL function's statement, PL/pgSQL's
/// EXECUTE): the cursors are the session's, as the top-level commands' are. None for other
/// statements.
pub fn spi_cursor_stmt(
    ctx: &ExecCtx,
    node: &pg_query::NodeEnum,
    sql: &str,
    params: &[(Value, DataType)],
) -> Option<Result<String>> {
    use pg_query::NodeEnum as N;
    const CURSOR_OPT_SCROLL: i32 = 0x0002;
    const CURSOR_OPT_NO_SCROLL: i32 = 0x0004;
    const CURSOR_OPT_HOLD: i32 = 0x0020;
    Some(match node {
        N::DeclareCursorStmt(s) => (|| {
            let hold = s.options & CURSOR_OPT_HOLD != 0;
            let q = s.query.as_deref().ok_or_else(|| crate::error::internal("DECLARE"))?;
            if !matches!(
                crate::sql::binder::ast::inner(q),
                Some(pg_query::NodeEnum::SelectStmt(_))
            ) {
                return Err(Error::new(
                    code::INVALID_CURSOR_DEFINITION,
                    "DECLARE CURSOR must be a SELECT statement",
                ));
            }
            let types = params.iter().map(|p| p.1.clone()).collect();
            let bq = crate::sql::binder::Binder::new(ctx, types).bind_statement(q)?;
            if bq.plan.writes() {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "DECLARE CURSOR must not contain data-modifying statements in WITH",
                ));
            }
            if hold && ctx.sess.security_restricted() {
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    "cannot create a cursor WITH HOLD within security-restricted operation",
                ));
            }
            if ctx.sess.cursors.borrow().contains_key(&s.portalname) {
                return Err(Error::new(
                    code::DUPLICATE_CURSOR,
                    format!("cursor \"{}\" already exists", s.portalname),
                ));
            }
            bq.check_perms(ctx)?;
            let keyed = keyed_plan(&bq.plan);
            let scroll = if s.options & (CURSOR_OPT_SCROLL | CURSOR_OPT_NO_SCROLL) == 0 {
                scrollable(&bq.plan)
            } else {
                s.options & CURSOR_OPT_SCROLL != 0
            };
            let plan = match &keyed {
                Some((plan, _)) => Arc::new(plan.clone()),
                None => bq.plan.clone(),
            };
            let lazy = Lazy::new(ctx, plan, Rc::new(Vec::new()), None, ctx.child(&[]).keep())
                .with_params(params.iter().map(|p| p.0.clone()).collect())
                .for_cursor();
            let mut c = Cursor::new_lazy(
                bq.cols.clone(),
                lazy,
                keyed.map(|k| k.1),
                hold,
                scroll,
                sql.to_string(),
                crate::session::Session::now(),
            );
            c.subxact = ctx.txn.borrow().subtxn_depth();
            c.rels = bq.perms.rels.iter().map(|r| r.oid).collect();
            c.row_marks = has_row_marks(&bq.plan);
            ctx.sess
                .cursors
                .borrow_mut()
                .insert(s.portalname.clone(), c);
            Ok("DECLARE CURSOR".to_string())
        })(),
        N::FetchStmt(s) => (|| {
            let dir = pb::FetchDirection::try_from(s.direction)
                .unwrap_or(pb::FetchDirection::FetchForward);
            let Some((out, _, n)) = fetch(ctx, &s.portalname, dir, s.how_many, !s.ismove)? else {
                return Err(Error::new(
                    code::INVALID_CURSOR_NAME,
                    format!("cursor \"{}\" does not exist", s.portalname),
                ));
            };
            Ok(if s.ismove {
                format!("MOVE {n}")
            } else {
                format!("FETCH {}", out.len())
            })
        })(),
        N::ClosePortalStmt(s) => {
            if s.portalname.is_empty() {
                ctx.sess.cursors.borrow_mut().clear();
                Ok("CLOSE CURSOR ALL".to_string())
            } else if ctx.sess.cursors.borrow_mut().remove(&s.portalname).is_none() {
                Err(Error::new(
                    code::INVALID_CURSOR_NAME,
                    format!("cursor \"{}\" does not exist", s.portalname),
                ))
            } else {
                Ok("CLOSE CURSOR".to_string())
            }
        }
        _ => return None,
    })
}

/// Whether a query locks the rows it reads (FOR UPDATE / SHARE): a LockRows node, or a
/// scan with a row mark.
pub fn has_row_marks(plan: &Plan) -> bool {
    match plan {
        Plan::LockRows { .. } => true,
        Plan::Scan(s) => s.mark.is_some(),
        Plan::Filter { input, .. }
        | Plan::Project { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. } => has_row_marks(input),
        Plan::Append { inputs, .. } => inputs.iter().any(|p| has_row_marks(p)),
        Plan::Join { left, right, .. } => has_row_marks(left) || has_row_marks(right),
        _ => false,
    }
}
