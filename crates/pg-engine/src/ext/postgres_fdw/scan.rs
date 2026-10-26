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
    let t = &s.table;
    let d = Deparser::new(cat, t, fmt);
    let mut filter = Vec::new();
    if let Some(f) = &s.filter {
        conjuncts(f, &mut filter);
    }
    let order: Vec<Expr> = if s.written.is_empty() {
        filter.clone()
    } else {
        let mut w = Vec::new();
        s.written.iter().for_each(|x| conjuncts(x, &mut w));
        w
    };
    let remote: Vec<Expr> = order.into_iter().filter(|c| d.shippable(c)).collect();
    let local: Vec<Expr> = filter.into_iter().filter(|c| !remote.iter().any(|r| r.same_as(c))).collect();
    let n = t.columns.len();
    let mut used: Vec<bool> = (0..n).map(|i| need.is_none_or(|m| m.get(i).copied().unwrap_or(false))).collect();
    for c in &local {
        for col in c.columns() {
            if col < n {
                used[col] = true;
            }
        }
    }
    let retrieved: Vec<usize> = (0..n).filter(|i| used[*i] && !t.columns[*i].dropped).collect();
    let ctid = s.with_key || s.mark.is_some();
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
                .map(|k| {
                    format!(
                        "{} {} NULLS {}",
                        d.expr(&k.expr),
                        if k.desc { "DESC" } else { "ASC" },
                        if k.nulls_first { "FIRST" } else { "LAST" }
                    )
                })
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
        None if s.with_key => sql.push_str(" FOR UPDATE"),
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

/// fetch_size: the table's option, else the server's, else 100.
fn fetch_size(cat: &Catalog, t: &Table) -> usize {
    let f = t.foreign.as_ref();
    let srv = f.and_then(|f| cat.servers.get(&f.server));
    option::get(f.and_then(|f| f.options.as_ref()), "fetch_size")
        .or_else(|| srv.and_then(|s| option::get(s.options.as_ref(), "fetch_size")))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(100)
}

/// A running foreign scan: a remote cursor read in batches.
struct ScanNode {
    table: Arc<Table>,
    key: u32,
    cursor: String,
    fetch: usize,
    batch: VecDeque<Vec<Option<String>>>,
    eof: bool,
    retrieved: Vec<usize>,
    ctid: bool,
    with_key: bool,
    sys: bool,
    local: Vec<Expr>,
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
                .map_err(|e| e.context(format!("column \"{}\" of foreign table \"{}\"", col.name, t.name)))?;
        }
    }
    let ctid = if n.ctid { vals.get(n.retrieved.len()).cloned().flatten() } else { None };
    if n.with_key {
        row.push(Value::Bytea(Arc::from(ctid.clone().unwrap_or_default().into_bytes())));
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
    let cat = ctx.catalog();
    let spec = spec(s, need, &cat, ctx.fmt());
    let m = mapping_for(&s.table, s.check_as, ctx)?;
    let key = connection::get_connection(ctx, &m, false)?;
    let cursor = format!("c{}", connection::next_cursor(ctx));
    let values: Vec<Option<String>> = spec
        .params
        .iter()
        .map(|p| {
            let v = crate::sql::expr::eval(p, &[], ctx)?;
            let fmt = super::deparse::transmission_fmt(ctx.fmt());
            Ok((!v.is_null()).then(|| crate::types::text::format_value(&v, &p.ty(), &fmt)))
        })
        .collect::<Result<_>>()?;
    let sql = format!("DECLARE {cursor} CURSOR FOR\n{}", spec.sql);
    let intr = crate::remote::interrupts(ctx);
    let res = connection::with_conn(ctx, key, |c| {
        let r = if values.is_empty() { c.exec(&sql, &intr)? } else { c.exec_params(&sql, None, &values, &intr)? };
        if r.status != Status::CommandOk {
            return Err(connection::report(&r, c, Some(&sql)));
        }
        Ok(())
    });
    if let Err(e) = res {
        connection::note_error(ctx, key);
        return Err(e);
    }
    Ok(Box::new(ScanNode {
        table: s.table.clone(),
        key,
        cursor,
        fetch: fetch_size(&cat, &s.table),
        batch: VecDeque::new(),
        eof: false,
        retrieved: spec.retrieved,
        ctid: spec.ctid,
        with_key: s.with_key,
        sys: s.sys,
        local: spec.local,
    }))
}

impl ScanNode {
    /// fetch_more_data.
    fn fetch(&mut self, ctx: &ExecCtx) -> Result<()> {
        let sql = format!("FETCH {} FROM {}", self.fetch, self.cursor);
        let intr = crate::remote::interrupts(ctx);
        let r = connection::with_conn(ctx, self.key, |c| {
            let r = c.exec(&sql, &intr)?;
            if r.status != Status::TuplesOk {
                return Err(connection::report(&r, c, Some(&sql)));
            }
            Ok(r)
        });
        let r = match r {
            Ok(r) => r,
            Err(e) => {
                connection::note_error(ctx, self.key);
                return Err(e);
            }
        };
        self.eof = r.rows.len() < self.fetch;
        self.batch.extend(r.rows);
        if self.eof {
            let close = format!("CLOSE {}", self.cursor);
            connection::with_conn(ctx, self.key, |c| {
                let r = c.exec(&close, &intr)?;
                if r.status != Status::CommandOk {
                    return Err(connection::report(&r, c, Some(&close)));
                }
                Ok(())
            })?;
        }
        Ok(())
    }
}

impl Node for ScanNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        loop {
            if self.batch.is_empty() {
                if self.eof {
                    return Ok(None);
                }
                self.fetch(ctx)?;
                continue;
            }
            let vals = self.batch.pop_front().unwrap();
            let row = make_row(self, vals, ctx)?;
            let mut ok = true;
            for c in &self.local {
                if !matches!(crate::sql::expr::eval(c, &row, ctx)?, Value::Bool(true)) {
                    ok = false;
                    break;
                }
            }
            if ok {
                return Ok(Some(row));
            }
        }
    }
}

#[allow(dead_code)]
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
        key,
        cursor: String::new(),
        fetch: 0,
        batch: VecDeque::new(),
        eof: true,
        retrieved: cols,
        ctid: false,
        with_key: false,
        sys: false,
        local: Vec::new(),
    };
    let rows = res.rows.into_iter().map(|v| make_row(&node, v, ctx)).collect::<Result<Vec<_>>>()?;
    Ok((rows, pages))
}
