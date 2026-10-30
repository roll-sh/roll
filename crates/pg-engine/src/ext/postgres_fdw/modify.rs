//! Changes to foreign tables (postgresExecForeignInsert / Update / Delete): one remote
//! statement per row, by the row's remote ctid.

use super::connection;
use super::deparse::{column_name, relation_name};
use crate::catalog::{Table, quote_ident, trigger_type as tt};
use crate::error::Result;
use crate::exec::{ExecCtx, Row};
use crate::remote::conn::Status;
use crate::sql::expr::Expr;
use crate::sql::plan::BoundTrigger;
use crate::types::{DataType, Value};

thread_local! {
    /// The columns the running UPDATE assigns (its target list), set around each row's
    /// update.
    static SET_COLS: std::cell::RefCell<Option<Vec<usize>>> = const { std::cell::RefCell::new(None) };
    /// What the running INSERT / UPDATE / DELETE takes back of each remote row
    /// (deparseReturningList).
    static BACK: std::cell::RefCell<Option<Back>> = const { std::cell::RefCell::new(None) };
    /// The remote ctid of the row the last INSERT / UPDATE stored, when it came back.
    static LAST_CTID: std::cell::Cell<Option<(u32, u16)>> = const { std::cell::Cell::new(None) };
    /// Rows of batched INSERTs not sent yet (postgresExecForeignBatchInsert).
    static PENDING: std::cell::RefCell<Vec<Pending>> = const { std::cell::RefCell::new(Vec::new()) };
    /// The running INSERT routes rows into partitions while capturing transition tables.
    static ROUTED_CAPTURE: std::cell::RefCell<bool> = const { std::cell::RefCell::new(false) };
    /// The root's columns (by name) a routing INSERT's RETURNING list and check options
    /// read: a foreign partition's statement returns them too (ri_returningList mapped to
    /// the partition).
    static ROUTED_BACK: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
    /// The foreign partitions the running UPDATE changes, while it moves a row.
    static UPDATE_TARGETS: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// While the guard lives, rows moved by an UPDATE can't go to the foreign tables `oids`.
pub(crate) fn update_targets(oids: Vec<u32>) -> Option<crate::scoped::RefGuard<Vec<u32>>> {
    (!oids.is_empty()).then(|| crate::scoped::set_ref(&UPDATE_TARGETS, oids, "postgres_fdw::UPDATE_TARGETS"))
}

/// postgresBeginForeignInsert: a row moved into a foreign partition the UPDATE changes
/// would be changed again by it.
pub(crate) fn check_update_target(t: &Table) -> Result<()> {
    if UPDATE_TARGETS.with_borrow(|o| o.contains(&t.oid)) {
        return Err(crate::error::Error::new(
            crate::error::code::FEATURE_NOT_SUPPORTED,
            format!("cannot route tuples into foreign table to be updated \"{}\"", t.name),
        ));
    }
    Ok(())
}

/// While the guard lives, rows routed to foreign partitions bring back the columns of the
/// root table `t` that `b` names.
pub(crate) fn routed_back(t: &Table, b: Back) -> Option<crate::scoped::RefGuard<Option<Vec<String>>>> {
    (!b.cols.is_empty()).then(|| {
        let names = b.cols.iter().map(|&i| t.columns[i].name.clone()).collect();
        crate::scoped::set_ref(&ROUTED_BACK, Some(names), "postgres_fdw::ROUTED_BACK")
    })
}

/// A routed row's `b` with the partition `t`'s columns the routing statement reads
/// (`routed_back`).
pub(crate) fn with_routed(t: &Table, mut b: Back) -> Back {
    ROUTED_BACK.with_borrow(|names| {
        let Some(names) = names else { return };
        for (i, c) in t.columns.iter().enumerate() {
            if !c.dropped && names.contains(&c.name) && !b.cols.contains(&i) {
                b.cols.push(i);
            }
        }
        b.cols.sort_unstable();
    });
    b
}

/// While the guard lives, rows routed to partitions are captured for transition tables.
pub(crate) fn routed_capture(on: bool) -> Option<crate::scoped::RefGuard<bool>> {
    on.then(|| crate::scoped::set_ref(&ROUTED_CAPTURE, true, "postgres_fdw::ROUTED_CAPTURE"))
}

/// ExecInitRoutingInfo: a foreign partition can't give a routed row to transition tables.
pub(crate) fn check_routed_capture() -> Result<()> {
    if ROUTED_CAPTURE.with_borrow(|c| *c) {
        return Err(transition_error());
    }
    Ok(())
}

/// "cannot collect transition tuples from child foreign tables".
pub fn transition_error() -> crate::error::Error {
    crate::error::Error::new(
        crate::error::code::FEATURE_NOT_SUPPORTED,
        "cannot collect transition tuples from child foreign tables",
    )
}

/// The trigger types (tgtype) of a table's triggers (the trigger descriptor's flags).
pub fn tgtypes(trigs: &[BoundTrigger]) -> Vec<u16> {
    trigs.iter().map(|b| b.trig.tgtype).collect()
}

/// The trigger types of table `oid`'s triggers in the catalog.
pub fn catalog_tgtypes(cat: &crate::catalog::Catalog, oid: u32) -> Vec<u16> {
    cat.triggers.values().filter(|tg| tg.table == oid).map(|tg| tg.tgtype).collect()
}

/// The columns (and whether the ctid) a remote INSERT / UPDATE / DELETE returns
/// (deparseReturningList).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Back {
    pub cols: Vec<usize>,
    pub ctid: bool,
}

impl Back {
    /// deparseReturningList: every column for an AFTER ROW trigger of `event`, else the
    /// columns the expressions (the views' check options, the RETURNING list: over the
    /// table's row, its ctid at `ctid_col`) read.
    pub fn of(t: &Table, trigs: &[u16], event: u16, exprs: &[&Expr], ctid_col: Option<usize>) -> Back {
        let after_row = trigs.iter().any(|b| {
            let ty = *b;
            ty & tt::ROW != 0 && ty & (tt::BEFORE | tt::INSTEAD) == 0 && ty & event != 0
        });
        let mut used = vec![after_row; t.columns.len()];
        let mut ctid = false;
        for e in exprs {
            for c in e.columns() {
                if c < used.len() {
                    used[c] = true;
                } else if Some(c) == ctid_col {
                    ctid = true;
                }
            }
        }
        Back { cols: (0..t.columns.len()).filter(|i| used[*i] && !t.columns[*i].dropped).collect(), ctid }
    }

    /// RETURNING old.* / new.*'s columns too (pull_varattnos counts an old or new
    /// reference as the table's column).
    pub fn with_old_new(mut self, t: &Table, on: Option<&crate::sql::plan::OldNew>, exprs: &[&Expr]) -> Back {
        let Some(on) = on else {
            return self;
        };
        let n = t.columns.len();
        for c in exprs.iter().flat_map(|e| e.columns()) {
            let Some(k) = c.checked_sub(on.width) else {
                continue;
            };
            let col = if k < on.ncols { k } else { k.wrapping_sub(on.ncols + 1) };
            if col < n && !t.columns[col].dropped && !self.cols.contains(&col) {
                self.cols.push(col);
            }
        }
        self.cols.sort_unstable();
        self
    }

    pub fn is_empty(&self) -> bool {
        self.cols.is_empty() && !self.ctid
    }

    /// The statement's RETURNING clause (deparseTargetList, is_returning).
    pub fn sql(&self, t: &Table) -> String {
        let mut names: Vec<String> = self.cols.iter().map(|i| quote_ident(&column_name(t, *i))).collect();
        if self.ctid {
            names.push("ctid".into());
        }
        if names.is_empty() { String::new() } else { format!(" RETURNING {}", names.join(", ")) }
    }

    /// store_returning_result: the returned row as the table's (columns not returned are
    /// NULL), its ctid kept for `last_ctid`.
    fn row(&self, t: &Table, vals: &[Option<String>], ctx: &ExecCtx) -> Result<Row> {
        let mut out = vec![Value::Null; t.columns.len()];
        for (k, &i) in self.cols.iter().enumerate() {
            if let Some(Some(s)) = vals.get(k) {
                out[i] = crate::types::text::parse_value(s, &t.columns[i].ty, ctx.fmt())?;
            }
        }
        let ctid = vals.get(self.cols.len()).cloned().flatten().filter(|_| self.ctid);
        LAST_CTID.set(ctid.and_then(|c| match crate::types::text::parse_value(&c, &DataType::Tid, ctx.fmt()) {
            Ok(Value::Tid(b, o)) => Some((b, o)),
            _ => None,
        }));
        Ok(out)
    }
}

/// While the guard lives, the running statement's rows come back as `b` says.
pub(crate) fn with_back(b: Back) -> Option<crate::scoped::RefGuard<Option<Back>>> {
    (!b.is_empty()).then(|| crate::scoped::set_ref(&BACK, Some(b), "postgres_fdw::BACK"))
}

/// The remote ctid of the row the last INSERT / UPDATE of a foreign table stored, when
/// its statement returned it.
pub(crate) fn last_ctid() -> Option<Value> {
    LAST_CTID.get().map(|(b, o)| Value::Tid(b, o))
}

fn back() -> Option<Back> {
    BACK.with_borrow(|b| b.clone())
}

/// Whether the running statement has said what comes back (`with_back`).
pub(crate) fn has_back() -> bool {
    BACK.with_borrow(|b| b.is_some())
}

/// A foreign row's key for the statement (a scan's `with_key` value): the table's key
/// prefix, as a stored row's, so an inheritance UPDATE / DELETE finds its member, then the
/// remote ctid's text.
pub fn row_key(oid: u32, ctid: &str) -> Vec<u8> {
    let mut k = crate::storage::keys::index_prefix(oid, 0);
    k.extend_from_slice(ctid.as_bytes());
    k
}

/// The remote ctid of a foreign row's key (`row_key`).
fn key_ctid(key: &[u8]) -> String {
    String::from_utf8_lossy(key.get(8..).unwrap_or_default()).into_owned()
}

/// Run `f` with the UPDATE's assigned columns known (deparseUpdateSql's targetAttrs).
pub fn with_set_cols<T>(cols: Vec<usize>, f: impl FnOnce() -> T) -> T {
    let _g = crate::scoped::set_ref(&SET_COLS, Some(cols), "postgres_fdw::SET_COLS");
    f()
}

/// The scratch state outside any call (`scoped::unsettled_for_test`).
pub(crate) fn unsettled(out: &mut Vec<&'static str>) {
    if SET_COLS.with_borrow(|c| c.is_some()) {
        out.push("postgres_fdw::SET_COLS");
    }
    if BACK.with_borrow(|r| r.is_some()) {
        out.push("postgres_fdw::BACK");
    }
    if ROUTED_CAPTURE.with_borrow(|c| *c) {
        out.push("postgres_fdw::ROUTED_CAPTURE");
    }
    if ROUTED_BACK.with_borrow(|c| c.is_some()) {
        out.push("postgres_fdw::ROUTED_BACK");
    }
    if UPDATE_TARGETS.with_borrow(|c| !c.is_empty()) {
        out.push("postgres_fdw::UPDATE_TARGETS");
    }
}

fn text_params(t: &Table, row: &[Value], cols: &[usize], ctx: &ExecCtx) -> Vec<Option<String>> {
    let fmt = super::deparse::transmission_fmt(ctx.fmt());
    cols.iter()
        .map(|&i| {
            let v = row.get(i).cloned().unwrap_or(Value::Null);
            (!v.is_null()).then(|| crate::types::text::format_value(&v, &t.columns[i].ty, &fmt))
        })
        .collect()
}

/// Run a remote modification with parameters; its RETURNING row, if it has one.
fn run(t: &Table, sql: &str, params: &[Option<String>], ctx: &ExecCtx) -> Result<Option<Vec<Option<String>>>> {
    Ok(run_result(t, sql, params, ctx)?.rows.into_iter().next())
}

/// Run a remote modification with parameters: its result.
fn run_result(t: &Table, sql: &str, params: &[Option<String>], ctx: &ExecCtx) -> Result<crate::remote::conn::PgResult> {
    let m = super::scan::mapping_for(t, None, ctx)?;
    let key = connection::get_connection(ctx, &m, true)?;
    let intr = crate::remote::interrupts(ctx);
    let r = connection::with_conn(ctx, key, |c| {
        let r = c.exec_params(sql, None, params, &intr)?;
        if !matches!(r.status, Status::CommandOk | Status::TuplesOk) {
            return Err(connection::report(&r, c, Some(sql)));
        }
        Ok(r)
    });
    r.inspect_err(|_| connection::note_error(ctx, key))
}

/// deparseInsertSql's ON CONFLICT DO NOTHING (the only ON CONFLICT a foreign table takes:
/// it has no arbiter indexes).
pub const DO_NOTHING: &str = " ON CONFLICT DO NOTHING";

/// deparseInsertSql: every (non-generated) column.
pub fn insert_sql(cat: &crate::catalog::Catalog, t: &Table) -> (String, Vec<usize>) {
    let cols: Vec<usize> = (0..t.columns.len()).filter(|i| !t.columns[*i].dropped).collect();
    let names: Vec<String> = cols.iter().map(|i| quote_ident(&column_name(t, *i))).collect();
    let mut pindex = 0;
    let values: Vec<String> = cols
        .iter()
        .map(|i| {
            if t.columns[*i].generated.is_some() {
                "DEFAULT".to_string()
            } else {
                pindex += 1;
                format!("${pindex}")
            }
        })
        .collect();
    let mut sql = format!("INSERT INTO {}", relation_name(cat, t));
    if cols.is_empty() {
        sql.push_str(" DEFAULT VALUES");
    } else {
        sql.push_str(&format!("({}) VALUES ({})", names.join(", "), values.join(", ")));
    }
    let params: Vec<usize> = cols.into_iter().filter(|i| t.columns[*i].generated.is_none()).collect();
    (sql, params)
}

/// INSERT of `row` into foreign table `t`: the stored row, as the remote server returned it
/// when the statement takes it back; None when ON CONFLICT DO NOTHING (`do_nothing`) kept
/// it out.
pub fn insert(t: &Table, row: Row, do_nothing: bool, ctx: &ExecCtx) -> Result<Option<Row>> {
    let cat = ctx.catalog();
    let (mut sql, cols) = insert_sql(&cat, t);
    let back = back();
    // (Batched: nothing comes back, no row trigger of the table, batch_size over 1.)
    // (The running statement's batch: at most as many rows as 65535 parameters take.)
    // (Not under ON CONFLICT DO NOTHING: the statement counts the rows the server took.)
    let batch = batch_size(&cat, t, false, false, &[]).min(65535 / cols.len().max(1)).max(1);
    if batch > 1 && !do_nothing && back.as_ref().is_none_or(|b| b.cols.is_empty() && !b.ctid) && !row_insert_triggers(&cat, t) {
        let params = text_params(t, &row, &cols, ctx);
        let full = PENDING.with_borrow_mut(|ps| {
            let i = match ps.iter().position(|p| p.table.oid == t.oid) {
                Some(i) => i,
                None => {
                    ps.push(Pending { table: t.clone(), rows: Vec::new() });
                    ps.len() - 1
                }
            };
            ps[i].rows.push(params);
            (ps[i].rows.len() >= batch).then(|| ps.remove(i))
        });
        if let Some(p) = full {
            send_batch(p, ctx)?;
        }
        LAST_CTID.set(None);
        return Ok(Some(row));
    }
    if do_nothing {
        sql.push_str(DO_NOTHING);
    }
    if let Some(b) = &back {
        sql.push_str(&b.sql(t));
    }
    LAST_CTID.set(None);
    let r = run_result(t, &sql, &text_params(t, &row, &cols, ctx), ctx)?;
    // (store_returning_result; "INSERT 0 0": the row conflicted.)
    if do_nothing && r.rows.is_empty() && r.cmd_status.ends_with(" 0") {
        return Ok(None);
    }
    match (back, r.rows.into_iter().next()) {
        (Some(b), Some(vals)) => b.row(t, &vals, ctx).map(Some),
        _ => Ok(Some(row)),
    }
}

/// Rows waiting to go to one table in one INSERT.
struct Pending {
    table: Table,
    rows: Vec<Vec<Option<String>>>,
}

/// BEFORE / AFTER ROW INSERT triggers on the foreign table turn batching off.
fn row_insert_triggers(cat: &crate::catalog::Catalog, t: &Table) -> bool {
    cat.triggers.values().any(|tg| tg.table == t.oid && tg.tgtype & tt::ROW != 0 && tg.tgtype & tt::INSERT != 0)
}

/// rebuildInsertSql: one INSERT of the batch's rows, `VALUES ($1, $2), ($3, $4)`.
fn send_batch(p: Pending, ctx: &ExecCtx) -> Result<()> {
    let cat = ctx.catalog();
    let t = &p.table;
    let (sql, _) = insert_sql(&cat, t);
    let Some(at) = sql.find(" VALUES (") else {
        for r in &p.rows {
            run(t, &sql, r, ctx)?;
        }
        return Ok(());
    };
    let (head, first) = sql.split_at(at + " VALUES ".len());
    let mut n = 0;
    let mut groups = Vec::new();
    for _ in &p.rows {
        let g = first
            .trim_start_matches('(')
            .trim_end_matches(')')
            .split(", ")
            .map(|v| {
                if v.starts_with('$') {
                    n += 1;
                    format!("${n}")
                } else {
                    v.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        groups.push(format!("({g})"));
    }
    let sql = format!("{head}{}", groups.join(", "));
    let params: Vec<Option<String>> = p.rows.into_iter().flatten().collect();
    run(t, &sql, &params, ctx).map(|_| ())
}

/// ExecPendingInserts: send the batched rows not sent yet (the end of a data-modifying
/// node).
pub fn flush_pending(ctx: &ExecCtx) -> Result<()> {
    let ps = PENDING.with_borrow_mut(std::mem::take);
    for p in ps {
        send_batch(p, ctx)?;
    }
    Ok(())
}

/// Rows of a statement that failed: never sent.
pub fn discard_pending() {
    PENDING.with_borrow_mut(|p| p.clear());
}

/// The columns a remote UPDATE sets (postgresPlanForeignModify): every column under a BEFORE
/// ROW UPDATE trigger (it may change any), else get_rel_all_updated_cols, the assigned
/// columns and the generated columns reading them; in column order.
pub fn target_cols(t: &Table, sets: impl Iterator<Item = usize>, generated: &[Option<Expr>], trigs: &[u16]) -> Vec<usize> {
    let before_row = trigs.iter().any(|b| {
        let ty = *b;
        ty & tt::ROW != 0 && ty & tt::BEFORE != 0 && ty & tt::UPDATE != 0
    });
    if before_row {
        return (0..t.columns.len()).filter(|i| !t.columns[*i].dropped).collect();
    }
    let mut cols: Vec<usize> = sets.filter(|i| t.columns.get(*i).is_some_and(|c| c.generated.is_none())).collect();
    for (i, g) in generated.iter().enumerate() {
        if let Some(e) = g
            && t.columns.get(i).is_some_and(|c| !c.dropped)
            && e.columns().iter().any(|c| cols.contains(c))
        {
            cols.push(i);
        }
    }
    cols.sort_unstable();
    cols.dedup();
    cols
}

/// deparseUpdateSql over the target columns (generated ones set to DEFAULT).
pub fn update_sql(cat: &crate::catalog::Catalog, t: &Table, cols: &[usize]) -> String {
    let mut pindex = 1;
    let sets: Vec<String> = cols
        .iter()
        .map(|i| {
            let name = quote_ident(&column_name(t, *i));
            if t.columns[*i].generated.is_some() {
                format!("{name} = DEFAULT")
            } else {
                pindex += 1;
                format!("{name} = ${pindex}")
            }
        })
        .collect();
    format!("UPDATE {} SET {} WHERE ctid = $1", relation_name(cat, t), sets.join(", "))
}

/// A foreign inheritance member's own columns that the named table lacks, read by the
/// row's remote ctid into `row` (PostgreSQL's member scan returns the member's whole row
/// for its row triggers and for an UPDATE sending every column; the engine's scan returns
/// the named table's columns). A row gone meanwhile leaves them NULL.
pub fn fill_member_row(t: &Table, row: &mut [Value], missing: &[usize], key: &[u8], ctx: &ExecCtx) -> Result<()> {
    let cat = ctx.catalog();
    let names: Vec<String> = missing.iter().map(|i| quote_ident(&column_name(t, *i))).collect();
    let sql = format!("SELECT {} FROM {} WHERE ctid = $1", names.join(", "), relation_name(&cat, t));
    if let Some(vals) = run(t, &sql, &[Some(key_ctid(key))], ctx)? {
        for (k, &i) in missing.iter().enumerate() {
            if let (Some(Some(s)), Some(slot)) = (vals.get(k), row.get_mut(i)) {
                *slot = crate::types::text::parse_value(s, &t.columns[i].ty, ctx.fmt())?;
            }
        }
    }
    Ok(())
}

/// UPDATE of the row at remote ctid `key` to `new`: the stored row, as the remote server
/// returned it when the statement takes it back.
pub fn update(t: &Table, key: &[u8], old: &[Value], new: Row, ctx: &ExecCtx) -> Result<Row> {
    let cat = ctx.catalog();
    let cols: Vec<usize> = SET_COLS.with_borrow(|c| c.clone()).unwrap_or_else(|| {
        (0..t.columns.len()).filter(|i| !t.columns[*i].dropped && old.get(*i) != new.get(*i)).collect()
    });
    LAST_CTID.set(None);
    if cols.is_empty() {
        return Ok(new);
    }
    let back = back();
    let mut sql = update_sql(&cat, t, &cols);
    if let Some(b) = &back {
        sql.push_str(&b.sql(t));
    }
    let mut params = vec![Some(key_ctid(key))];
    let values: Vec<usize> = cols.iter().copied().filter(|i| t.columns[*i].generated.is_none()).collect();
    params.extend(text_params(t, &new, &values, ctx));
    match (back, run(t, &sql, &params, ctx)?) {
        (Some(b), Some(vals)) => b.row(t, &vals, ctx),
        _ => Ok(new),
    }
}

/// postgresGetForeignModifyBatchSize: the `batch_size` option (the table's, else its
/// server's, else 1), 1 with RETURNING, a view's check option or a BEFORE / AFTER ROW INSERT
/// trigger.
pub fn batch_size(cat: &crate::catalog::Catalog, t: &Table, returning: bool, wco: bool, trigs: &[u16]) -> usize {
    let row_insert = trigs.iter().any(|ty| ty & tt::ROW != 0 && ty & tt::INSERT != 0 && ty & tt::INSTEAD == 0);
    if returning || wco || row_insert {
        return 1;
    }
    let f = t.foreign.as_ref();
    let srv = f.and_then(|f| cat.servers.get(&f.server));
    let size = super::option::get(f.and_then(|f| f.options.as_ref()), "batch_size")
        .or_else(|| srv.and_then(|s| super::option::get(s.options.as_ref(), "batch_size")))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(1);
    // (EXPLAIN shows the option as it is: the 65535-parameter cap is a running statement's.)
    size
}

/// deparseDeleteSql.
pub fn delete_sql(cat: &crate::catalog::Catalog, t: &Table) -> String {
    format!("DELETE FROM {} WHERE ctid = $1", relation_name(cat, t))
}

/// postgresExecForeignTruncate: one server's tables truncated by one remote TRUNCATE, each
/// table (or its server) allowing truncates (`truncatable`, default on).
pub fn truncate(rels: &[std::sync::Arc<Table>], cascade: bool, restart: bool, ctx: &ExecCtx) -> Result<()> {
    let cat = ctx.catalog();
    let Some(first) = rels.first() else { return Ok(()) };
    for t in rels {
        if !super::scan::bool_option(&cat, t, "truncatable", true) {
            return Err(crate::error::Error::new(
                crate::error::code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("foreign table \"{}\" does not allow truncates", t.name),
            ));
        }
    }
    // (The connection is the first table's user mapping's, for the current user.)
    let m = super::scan::mapping_for(first, None, ctx)?;
    let key = connection::get_connection(ctx, &m, false)?;
    let names: Vec<String> = rels.iter().map(|t| relation_name(&cat, t)).collect();
    let sql = format!(
        "TRUNCATE {} {} IDENTITY {}",
        names.join(", "),
        if restart { "RESTART" } else { "CONTINUE" },
        if cascade { "CASCADE" } else { "RESTRICT" }
    );
    let intr = crate::remote::interrupts(ctx);
    let r = connection::with_conn(ctx, key, |c| {
        let r = c.exec(&sql, &intr)?;
        if r.status != Status::CommandOk {
            return Err(connection::report(&r, c, Some(&sql)));
        }
        Ok(())
    });
    if r.is_err() {
        connection::note_error(ctx, key);
    }
    r
}

/// DELETE of the row at remote ctid `key`: the deleted row as the remote server returned
/// it, when the statement takes it back.
pub fn delete(t: &Table, key: &[u8], ctx: &ExecCtx) -> Result<Option<Row>> {
    let cat = ctx.catalog();
    let back = back();
    let mut sql = delete_sql(&cat, t);
    if let Some(b) = &back {
        sql.push_str(&b.sql(t));
    }
    match (back, run(t, &sql, &[Some(key_ctid(key))], ctx)?) {
        (Some(b), Some(vals)) => Ok(Some(b.row(t, &vals, ctx)?)),
        _ => Ok(None),
    }
}
