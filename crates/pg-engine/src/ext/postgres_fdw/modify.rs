//! Changes to foreign tables (postgresExecForeignInsert / Update / Delete): one remote
//! statement per row, by the row's remote ctid.

use super::connection;
use super::deparse::{Deparser, column_name, relation_name};
use crate::catalog::{Table, quote_ident};
use crate::error::Result;
use crate::exec::{ExecCtx, Row};
use crate::remote::conn::Status;
use crate::types::Value;

thread_local! {
    /// The columns the running UPDATE assigns (its target list), set around each row's
    /// update.
    static SET_COLS: std::cell::RefCell<Option<Vec<usize>>> = const { std::cell::RefCell::new(None) };
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

/// INSERT of `row` into foreign table `t`.
pub fn insert(t: &Table, row: Row, ctx: &ExecCtx) -> Result<Row> {
    let cat = ctx.catalog();
    let (sql, cols) = insert_sql(&cat, t);
    run(t, &sql, &text_params(t, &row, &cols, ctx), ctx)?;
    Ok(row)
}

/// deparseUpdateSql over the assigned columns.
pub fn update_sql(cat: &crate::catalog::Catalog, t: &Table, cols: &[usize]) -> String {
    let sets: Vec<String> =
        cols.iter().enumerate().map(|(k, i)| format!("{} = ${}", quote_ident(&column_name(t, *i)), k + 2)).collect();
    format!("UPDATE {} SET {} WHERE ctid = $1", relation_name(cat, t), sets.join(", "))
}

/// UPDATE of the row at remote ctid `key` to `new`.
pub fn update(t: &Table, key: &[u8], old: &[Value], new: Row, ctx: &ExecCtx) -> Result<Row> {
    let cat = ctx.catalog();
    let cols: Vec<usize> = SET_COLS.with_borrow(|c| c.clone()).unwrap_or_else(|| {
        (0..t.columns.len()).filter(|i| !t.columns[*i].dropped && old.get(*i) != new.get(*i)).collect()
    });
    let mut cols: Vec<usize> = cols.into_iter().filter(|i| t.columns.get(*i).is_some_and(|c| c.generated.is_none())).collect();
    cols.sort_unstable();
    cols.dedup();
    if cols.is_empty() {
        return Ok(new);
    }
    let sql = update_sql(&cat, t, &cols);
    let mut params = vec![Some(String::from_utf8_lossy(key).into_owned())];
    params.extend(text_params(t, &new, &cols, ctx));
    run(t, &sql, &params, ctx)?;
    Ok(new)
}

/// DELETE of the row at remote ctid `key`.
pub fn delete(t: &Table, key: &[u8], ctx: &ExecCtx) -> Result<()> {
    let cat = ctx.catalog();
    let sql = format!("DELETE FROM {} WHERE ctid = $1", relation_name(cat.as_ref(), t));
    run(t, &sql, &[Some(String::from_utf8_lossy(key).into_owned())], ctx)?;
    Ok(())
}

#[allow(dead_code)]
fn deparser<'a>(cat: &'a crate::catalog::Catalog, t: &'a Table, ctx: &ExecCtx) -> Deparser<'a> {
    Deparser::new(cat, t, ctx.fmt())
}
