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
    match r {
        Ok(r) => Ok(r.rows.into_iter().next()),
        Err(e) => {
            connection::note_error(ctx, key);
            Err(e)
        }
    }
}

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
/// when the statement takes it back.
pub fn insert(t: &Table, row: Row, ctx: &ExecCtx) -> Result<Row> {
    let cat = ctx.catalog();
    let (mut sql, cols) = insert_sql(&cat, t);
    let back = back();
    if let Some(b) = &back {
        sql.push_str(&b.sql(t));
    }
    LAST_CTID.set(None);
    let vals = run(t, &sql, &text_params(t, &row, &cols, ctx), ctx)?;
    match (back, vals) {
        (Some(b), Some(vals)) => b.row(t, &vals, ctx),
        _ => Ok(row),
    }
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

/// deparseDeleteSql.
pub fn delete_sql(cat: &crate::catalog::Catalog, t: &Table) -> String {
    format!("DELETE FROM {} WHERE ctid = $1", relation_name(cat, t))
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
