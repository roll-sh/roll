//! Foreign scans of base relations (postgresGetForeignPlan / BeginForeignScan /
//! IterateForeignScan): the remote SELECT, the conditions kept locally, and a cursor fetched
//! `fetch_size` rows at a time.

use super::connection;
use super::deparse::{Deparser, relation_name};
use super::option;
use crate::catalog::{Catalog, Table};
use crate::error::{Error, Result, code};
use crate::exec::{BoxNode, ExecCtx, Node, Row};
use crate::remote::conn::Status;
use crate::sql::expr::Expr;
use crate::sql::plan::{RowMark, ScanPlan};
use crate::types::Value;
use std::collections::VecDeque;
use std::sync::Arc;

/// Whether `t` is a foreign table of a postgres_fdw server.
pub fn is_postgres_fdw(cat: &Catalog, t: &Table) -> bool {
    let Some(f) = &t.foreign else { return false };
    let Some(w) = cat.servers.get(&f.server).and_then(|s| cat.fdws.get(&s.fdw)) else {
        return false;
    };
    cat.functions
        .get(&w.handler)
        .is_some_and(|h| h.probin.as_deref().is_some_and(|b| crate::ext::native::library(b) == "postgres_fdw"))
}

/// What a scan sends and what it checks locally.
pub struct Spec {
    pub sql: String,
    /// The table columns the remote SELECT returns, in its order.
    pub retrieved: Vec<usize>,
    /// The remote ctid is returned last (an UPDATE / DELETE target, a row mark).
    pub ctid: bool,
    /// Conditions checked locally (in evaluation order).
    pub local: Vec<Expr>,
    /// Conditions sent.
    pub remote: Vec<Expr>,
    /// The remote query's parameters (`$n`).
    pub params: Vec<Expr>,
}

fn conjuncts(e: &Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::And(xs) => xs.iter().for_each(|x| conjuncts(x, out)),
        e => out.push(e.clone()),
    }
}

/// The scan's remote query: the columns `need` reads (None: all) and those of the local
/// conditions, the shippable conditions in restriction order, and the locking clause.
pub fn spec(s: &ScanPlan, need: Option<&[bool]>, cat: &Catalog, fmt: &crate::types::text::FormatCtx) -> Spec {
    spec_with(s, need, cat, fmt, false)
}

/// `spec`, its parameters as placeholders when `explain` (a remote EXPLAIN's query).
pub fn spec_with(s: &ScanPlan, need: Option<&[bool]>, cat: &Catalog, fmt: &crate::types::text::FormatCtx, explain: bool) -> Spec {
    let t = &s.table;
    let mut d = Deparser::new(cat, t, fmt);
    d.placeholders = explain;
    let ctid_col = s.sys.then(|| t.columns.len() + usize::from(s.with_key));
    d.ctid_col = ctid_col;
    let mut filter = Vec::new();
    if let Some(f) = &s.filter {
        conjuncts(f, &mut filter);
    }
    // (The conditions in baserestrictinfo order, those the scan still checks: a clause a
    // join took over is out of its filter.)
    // (create_foreignscan_plan's order_qual_clauses, which postgresGetForeignPlan's
    // remote_exprs keep.)
    let mut order: Vec<Expr> = if s.written.is_empty() {
        filter.clone()
    } else {
        let mut w = Vec::new();
        s.written.iter().for_each(|x| conjuncts(x, &mut w));
        w.retain(|c| filter.iter().any(|f| f.same_as(c)));
        w.extend(filter.iter().filter(|f| !w.iter().any(|c| c.same_as(f))).cloned().collect::<Vec<_>>());
        w
    };
    crate::sql::binder::optimize::order_by_cost(&mut order);
    let remote: Vec<Expr> = order.into_iter().filter(|c| d.shippable(c)).collect();
    let local: Vec<Expr> = filter.into_iter().filter(|c| !remote.iter().any(|r| r.same_as(c))).collect();
    let n = t.columns.len();
    let wholerow = s.fdw.as_ref().is_some_and(|f| f.wholerow || f.copy_mark);
    let mut used: Vec<bool> = (0..n).map(|i| wholerow || need.is_none_or(|m| m.get(i).copied().unwrap_or(false))).collect();
    for c in &local {
        for col in c.columns() {
            if col < n {
                used[col] = true;
            }
        }
    }
    let retrieved: Vec<usize> = (0..n).filter(|i| used[*i] && !t.columns[*i].dropped).collect();
    let target = s.fdw.as_ref().is_some_and(|f| f.target);
    // (The ctid also when a condition checked here or the plan above reads it.)
    let ctid = target
        || (s.mark.is_some() && !s.fdw.as_ref().is_some_and(|f| f.copy_mark))
        || ctid_col.is_some_and(|c| {
            need.is_none_or(|m| m.get(c).copied().unwrap_or(false)) || local.iter().any(|x| x.columns().contains(&c))
        });
    let mut cols: Vec<String> = retrieved.iter().map(|i| d.column(*i)).collect();
    if ctid {
        cols.push("ctid".into());
    }
    let mut sql = format!(
        "SELECT {} FROM {}",
        if cols.is_empty() { "NULL".to_string() } else { cols.join(", ") },
        relation_name(cat, t)
    );
    if !remote.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&d.conditions(&remote));
    }
    if let Some(f) = &s.fdw {
        // appendOrderByClause / appendLimitClause.
        if !f.order.is_empty() {
            let keys: Vec<String> = f
                .order
                .iter()
                .map(|k| format!("{}{}", d.expr(&k.expr), d.order_suffix(k)))
                .collect();
            sql.push_str(&format!(" ORDER BY {}", keys.join(", ")));
        }
        if f.has_limit {
            if let Some(l) = &f.limit {
                sql.push_str(&format!(" LIMIT {}", d.expr(l)));
            }
            if let Some(o) = &f.offset {
                sql.push_str(&format!(" OFFSET {}", d.expr(o)));
            }
        }
    }
    match s.mark.as_ref().map(|m| m.0) {
        Some(RowMark::Update | RowMark::NoKeyUpdate) => sql.push_str(" FOR UPDATE"),
        Some(RowMark::Share | RowMark::KeyShare) => sql.push_str(" FOR SHARE"),
        None if target => sql.push_str(" FOR UPDATE"),
        None => {}
    }
    let params = d.params.borrow().clone();
    Spec { sql, retrieved, ctid, local, remote, params }
}

/// The user a scan connects as: the view owner it is checked as, else the current user.
pub fn mapping_for(t: &Table, check_as: Option<u32>, ctx: &ExecCtx) -> Result<connection::Mapping> {
    let f = t.foreign.as_ref().ok_or_else(|| crate::error::internal("not a foreign table"))?;
    let user = check_as.unwrap_or_else(|| ctx.current_user());
    connection::user_mapping(&ctx.catalog(), user, f.server)
}

/// A boolean option of table `t`, else its server's (the table's wins), else `default`.
pub fn bool_option(cat: &Catalog, t: &Table, name: &str, default: bool) -> bool {
    let f = t.foreign.as_ref();
    let srv = f.and_then(|f| cat.servers.get(&f.server));
    option::get(f.and_then(|f| f.options.as_ref()), name)
        .or_else(|| srv.and_then(|s| option::get(s.options.as_ref(), name)))
        .and_then(|v| crate::ext::dblink::def_boolean(name, v).ok())
        .unwrap_or(default)
}

/// postgresIsForeignPathAsyncCapable: the `async_capable` option.
pub fn async_capable(cat: &Catalog, t: &Table) -> bool {
    bool_option(cat, t, "async_capable", false)
}

/// ExecAppendAsyncBegin's order: an Append of several members, some of them async-capable
/// foreign scans (`async_capable`, enable_async_append on), returns its synchronous members'
/// rows while the async requests are in flight, then the async members' as they complete
/// (in order: one request at a time per connection).
/// The second value: the first async member's place, whose request goes out first.
pub fn async_order(inputs: Vec<Arc<crate::sql::plan::Plan>>, ctx: &ExecCtx) -> (Vec<Arc<crate::sql::plan::Plan>>, Option<usize>) {
    if inputs.len() < 2 || ctx.catalog().servers.is_empty() {
        return (inputs, None);
    }
    let on = ctx.sess.settings.borrow().get_str("enable_async_append").is_none_or(|v| v != "off" && v != "false");
    if !on {
        return (inputs, None);
    }
    let cat = ctx.catalog();
    let is_async = |p: &crate::sql::plan::Plan| {
        let mut q = p;
        while let crate::sql::plan::Plan::Project { input, .. } = q {
            q = input;
        }
        matches!(q, crate::sql::plan::Plan::Scan(s) if s.table.foreign.is_some() && !s.with_key && s.mark.is_none() && is_postgres_fdw(&cat, &s.table) && async_capable(&cat, &s.table))
    };
    let flags: Vec<bool> = inputs.iter().map(|p| is_async(p)).collect();
    if !flags.iter().any(|f| *f) {
        return (inputs, None);
    }
    // (A synchronous member that is a foreign scan too may share the connection: no request
    // goes out ahead of it.)
    let foreign_sync = inputs.iter().zip(&flags).any(|(p, f)| !*f && has_foreign_scan(p));
    if foreign_sync {
        let (asy, sync): (Vec<_>, Vec<_>) = inputs.into_iter().zip(flags).partition(|(_, f)| *f);
        return (sync.into_iter().chain(asy).map(|(p, _)| p).collect(), None);
    }
    if flags.iter().all(|f| *f) {
        return (inputs, Some(0));
    }
    let (asy, sync): (Vec<_>, Vec<_>) = inputs.into_iter().zip(flags).partition(|(_, f)| *f);
    let first = sync.len();
    (sync.into_iter().chain(asy).map(|(p, _)| p).collect(), Some(first))
}

fn has_foreign_scan(p: &crate::sql::plan::Plan) -> bool {
    match p {
        crate::sql::plan::Plan::Scan(s) => s.table.foreign.is_some(),
        p => crate::sql::explain::plan_inputs(p).into_iter().any(|c| has_foreign_scan(c)),
    }
}

/// fetch_size: the table's option, else the server's, else 100.
fn fetch_size(cat: &Catalog, t: &Table) -> usize {
    let f = t.foreign.as_ref();
    let srv = f.and_then(|f| cat.servers.get(&f.server));
    option::get(f.and_then(|f| f.options.as_ref()), "fetch_size")
        .or_else(|| srv.and_then(|s| option::get(s.options.as_ref(), "fetch_size")))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(100)
}

/// A remote query read through a cursor, `fetch` rows at a time (create_cursor,
/// fetch_more_data, close_cursor).
pub struct Cursor {
    pub key: u32,
    name: String,
    fetch: usize,
    batch: VecDeque<Vec<Option<String>>>,
    eof: bool,
    /// The remote query and its parameters (`$n`), declared as a cursor at the first row.
    pub sql: String,
    params: Vec<Expr>,
    /// A FETCH sent and not read yet (fetch_more_data_begin).
    inflight: bool,
}

impl Cursor {
    pub fn new(key: u32, fetch: usize, sql: String, params: Vec<Expr>) -> Cursor {
        Cursor { key, name: String::new(), fetch, batch: VecDeque::new(), eof: false, sql, params, inflight: false }
    }

    /// fetch_more_data_begin: declare the cursor and send its first FETCH without waiting.
    pub fn prefetch(&mut self, ctx: &ExecCtx) -> Result<()> {
        if self.inflight || self.eof || !self.batch.is_empty() {
            return Ok(());
        }
        let sql = self.fetch_sql(ctx)?;
        let _ = connection::take_pending_result(self.key);
        connection::with_conn_raw(ctx, self.key, |c| {
            c.send_query(&sql).map_err(|m| Error::new(code::CONNECTION_FAILURE, m))
        })?;
        self.inflight = true;
        Ok(())
    }

    /// The next remote row.
    pub fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Vec<Option<String>>>> {
        loop {
            if let Some(r) = self.batch.pop_front() {
                return Ok(Some(r));
            }
            if self.eof {
                return Ok(None);
            }
            self.fetch_more(ctx)?;
        }
    }
}

/// A running foreign scan: a remote cursor read in batches.
struct ScanNode {
    table: Arc<Table>,
    cur: Cursor,
    retrieved: Vec<usize>,
    ctid: bool,
    with_key: bool,
    sys: bool,
    local: Vec<Expr>,
    /// The FROM item's alias and column aliases (conversion errors name them).
    alias: Option<String>,
    col_aliases: Option<Arc<Vec<(String, String)>>>,
    /// `FdwPushdown::member`: each row returned is kept whole for the DML node.
    member: bool,
}

/// make_tuple_from_result_row: a remote row as the table's row (columns not retrieved are
/// NULL), its ctid as the storage key, then the system columns.
fn make_row(n: &ScanNode, vals: Vec<Option<String>>, ctx: &ExecCtx) -> Result<Row> {
    let t = &n.table;
    let mut row = vec![Value::Null; t.columns.len()];
    for (k, &i) in n.retrieved.iter().enumerate() {
        if let Some(Some(s)) = vals.get(k) {
            let col = &t.columns[i];
            row[i] = crate::types::text::parse_value(s, &col.ty, ctx.fmt())
                .map_err(|e| {
                    // (conversion_error_callback: the names the query knows them by.)
                    let cname = n
                        .col_aliases
                        .as_ref()
                        .and_then(|a| a.iter().find(|(c, _)| *c == col.name).map(|(_, x)| x.clone()))
                        .unwrap_or_else(|| col.name.clone());
                    let rname = n.alias.clone().unwrap_or_else(|| t.name.clone());
                    e.context(format!("column \"{cname}\" of foreign table \"{rname}\""))
                })?;
        }
    }
    let ctid = if n.ctid { vals.get(n.retrieved.len()).cloned().flatten() } else { None };
    if n.with_key {
        row.push(Value::Bytea(Arc::from(super::modify::row_key(t.oid, ctid.as_deref().unwrap_or("")))));
    }
    if n.sys {
        let tid = ctid
            .as_deref()
            .and_then(|c| crate::types::text::parse_value(c, &crate::types::DataType::Tid, ctx.fmt()).ok())
            .unwrap_or(Value::Tid(u32::MAX, 0));
        row.extend([tid, Value::Oid(0), Value::Oid(0), Value::Oid(0), Value::Oid(0), Value::Oid(t.oid)]);
    }
    Ok(row)
}

/// Open a foreign scan (BeginForeignScan + create_cursor).
pub fn open(s: &ScanPlan, need: Option<&[bool]>, ctx: &ExecCtx) -> Result<BoxNode> {
    if s.current_of.is_some() {
        return Err(unsupported("WHERE CURRENT OF is not supported for this table type"));
    }
    let cat = ctx.catalog();
    let spec = spec(s, need, &cat, ctx.fmt());
    let m = mapping_for(&s.table, s.check_as, ctx)?;
    let key = connection::get_connection(ctx, &m, false)?;
    Ok(Box::new(ScanNode {
        table: s.table.clone(),
        cur: Cursor::new(key, fetch_size(&cat, &s.table), spec.sql, spec.params),
        retrieved: spec.retrieved,
        ctid: spec.ctid,
        with_key: s.with_key,
        sys: s.sys,
        local: spec.local,
        alias: s.alias.clone(),
        col_aliases: s.col_aliases.clone(),
        member: s.fdw.as_ref().is_some_and(|f| f.member) && s.with_key,
    }))
}

impl Cursor {
    /// create_cursor, at the first row asked for (when outer references have values).
    /// The DECLARE, sent with the first FETCH in one message (a round trip saved).
    fn declare_sql(&mut self, ctx: &ExecCtx) -> Result<String> {
        let fmt = super::deparse::transmission_fmt(ctx.fmt());
        let values: Vec<Option<String>> = self
            .params
            .iter()
            .map(|p| {
                let v = crate::sql::expr::eval(p, &[], ctx)?;
                Ok((!v.is_null()).then(|| crate::types::text::format_value(&v, &p.ty(), &fmt)))
            })
            .collect::<Result<_>>()?;
        self.name = format!("c{}", connection::next_cursor(ctx));
        // (The parameters' values go in as literals of the parameters' types: `$n::type`
        // becomes `'value'::type`, highest numbers first.)
        let mut text = self.sql.clone();
        for (i, v) in values.iter().enumerate().rev() {
            let lit = v.as_deref().map_or("NULL".to_string(), super::deparse::string_literal);
            text = text.replace(&format!("${}::", i + 1), &format!("{lit}::"));
        }
        Ok(format!("DECLARE {} CURSOR FOR\n{text}", self.name))
    }

    /// The next FETCH, after the DECLARE when the cursor isn't declared yet.
    fn fetch_sql(&mut self, ctx: &ExecCtx) -> Result<String> {
        let declare = if self.name.is_empty() { Some(self.declare_sql(ctx)?) } else { None };
        let fetch = format!("FETCH {} FROM {}", self.fetch, self.name);
        Ok(match declare {
            Some(d) => format!("{d};\n{fetch}"),
            None => fetch,
        })
    }
}

impl Cursor {
    /// fetch_more_data.
    fn fetch_more(&mut self, ctx: &ExecCtx) -> Result<()> {
        let sql = self.fetch_sql(ctx)?;
        let query = &self.sql;
        let intr = crate::remote::interrupts(ctx);
        let inflight = std::mem::take(&mut self.inflight);
        let stashed = if inflight { connection::take_pending_result(self.key) } else { None };
        let run = |c: &mut crate::remote::conn::Conn| {
            // (A command sent in between read this one's results first.)
            if let Some(r) = stashed {
                return Ok(r);
            }
            if inflight && !c.is_busy() {
                return c.exec(&sql, &intr);
            }
            let r = if inflight { collect(c, &intr)? } else { c.exec(&sql, &intr)? };
            if r.status != Status::TuplesOk {
                return Err(connection::report(&r, c, Some(query)));
            }
            Ok(r)
        };
        // (Its own FETCH in flight: the connection as it is; else another scan's prefetch is
        // read first.)
        let r = if inflight { connection::with_conn_raw(ctx, self.key, run) } else { connection::with_conn(ctx, self.key, run) };
        let r = match r {
            Ok(r) => r,
            Err(e) => {
                connection::note_error(ctx, self.key);
                return Err(e);
            }
        };
        self.eof = r.rows.len() < self.fetch;
        self.batch.extend(r.rows);
        // (close_cursor, sent with the connection's next command; the transaction's end
        // closes it when that is the next.)
        if self.eof {
            let close = format!("CLOSE {}", self.name);
            connection::with_conn(ctx, self.key, |c| {
                c.defer(close);
                Ok(())
            })?;
        }
        Ok(())
    }
}

/// The results of the query sent (`prefetch`): its last one, as `exec` gives.
fn collect(c: &mut crate::remote::conn::Conn, intr: crate::remote::conn::Interrupt) -> Result<crate::remote::conn::PgResult> {
    let mut last = None;
    while let Some(r) = c.get_result(intr)? {
        last = Some(r);
    }
    last.ok_or_else(|| crate::error::internal("no result of the prefetched FETCH"))
}

impl Node for ScanNode {
    fn prefetch(&mut self, ctx: &ExecCtx) -> Result<()> {
        self.cur.prefetch(ctx)
    }

    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        loop {
            let Some(vals) = self.cur.next(ctx)? else { return Ok(None) };
            let row = make_row(self, vals, ctx)?;
            let mut ok = true;
            for c in &self.local {
                if !matches!(crate::sql::expr::eval(c, &row, ctx)?, Value::Bool(true)) {
                    ok = false;
                    break;
                }
            }
            if ok {
                if self.member
                    && let Some(Value::Bytea(k)) = row.get(self.table.columns.len())
                {
                    super::modify::keep_member_row(k.to_vec(), row[..self.table.columns.len()].to_vec());
                }
                return Ok(Some(row));
            }
        }
    }
}

fn unsupported(what: &str) -> Error {
    Error::new(code::FEATURE_NOT_SUPPORTED, what.to_string())
}

/// postgresAcquireSampleRowsFunc / postgresAnalyzeForeignTable: every remote row (as the
/// table's rows) and the remote relation's size in pages.
pub fn remote_rows(t: &Table, ctx: &ExecCtx) -> Result<(Vec<Row>, i32)> {
    let cat = ctx.catalog();
    let m = mapping_for(t, None, ctx)?;
    let key = connection::get_connection(ctx, &m, false)?;
    let d = Deparser::new(&cat, t, ctx.fmt());
    let cols: Vec<usize> = (0..t.columns.len()).filter(|i| !t.columns[*i].dropped).collect();
    let rel = relation_name(&cat, t);
    let list: Vec<String> = cols.iter().map(|i| d.column(*i)).collect();
    let sql = format!("SELECT {} FROM {rel}", if list.is_empty() { "NULL".to_string() } else { list.join(", ") });
    let size_sql = format!(
        "SELECT pg_catalog.pg_relation_size({}::pg_catalog.regclass) / 8192",
        super::deparse::string_literal(&rel)
    );
    let intr = crate::remote::interrupts(ctx);
    let (res, pages) = connection::with_conn(ctx, key, |c| {
        let p = c.exec(&size_sql, &intr)?;
        if p.status != Status::TuplesOk {
            return Err(connection::report(&p, c, Some(&size_sql)));
        }
        let r = c.exec(&sql, &intr)?;
        if r.status != Status::TuplesOk {
            return Err(connection::report(&r, c, Some(&sql)));
        }
        let pages = p.rows.first().and_then(|r| r.first().cloned().flatten()).and_then(|v| v.parse().ok()).unwrap_or(0);
        Ok((r, pages))
    })?;
    let node = ScanNode {
        table: Arc::new(t.clone()),
        cur: Cursor::new(key, 0, String::new(), Vec::new()),
        retrieved: cols,
        ctid: false,
        with_key: false,
        sys: false,
        local: Vec::new(),
        alias: None,
        col_aliases: None,
        member: false,
    };
    let rows = res.rows.into_iter().map(|v| make_row(&node, v, ctx)).collect::<Result<Vec<_>>>()?;
    Ok((rows, pages))
}
