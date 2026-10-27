//! spi (`contrib/spi`): the trigger functions of autoinc, insert_username, moddatetime and
//! refint (check_primary_key, check_foreign_key: referential integrity checked and cascaded
//! through queries the trigger runs, as SPI runs them).

use super::native::{Native, TriggerData};
use crate::Result;
use crate::error::{Error, Frame, code};
use crate::exec::{ExecCtx, Row};
use crate::types::{DataType, Value};

pub(super) fn symbol(sym: &str) -> Option<Native> {
    fn not_trigger(prefix: &str) -> crate::Result<Value> {
        Err(crate::error::internal(format!("{prefix}not fired by trigger manager")))
    }
    Some(Native::scalar(match sym {
        "autoinc" => |_, _| not_trigger(""),
        "insert_username" => |_, _| not_trigger("insert_username: "),
        "moddatetime" => |_, _| not_trigger("moddatetime: "),
        "check_primary_key" => |_, _| not_trigger("check_primary_key: "),
        "check_foreign_key" => |_, _| not_trigger("check_foreign_key: "),
        _ => return None,
    }))
}

pub(super) fn trigger(sym: &str) -> Option<super::native::TriggerImpl> {
    Some(match sym {
        "autoinc" => autoinc,
        "insert_username" => insert_username,
        "moddatetime" => moddatetime,
        "check_primary_key" => check_primary_key,
        "check_foreign_key" => check_foreign_key,
        _ => return None,
    })
}

fn elog(m: impl Into<String>) -> Error {
    crate::error::internal(m)
}

fn action_error(m: String) -> Error {
    Error::new(code::TRIGGERED_ACTION_EXCEPTION, m)
}

/// SPI_fnumber: a column by its exact name (none for a dropped or system column).
fn fnumber(t: &TriggerData, name: &str) -> Option<usize> {
    t.table.column_index(name)
}

/// The row a BEFORE INSERT / UPDATE trigger goes on with (tg_trigtuple / tg_newtuple), or
/// the DELETE error. `who`: the messages' prefix.
fn before_row(t: &TriggerData, who: &str) -> Result<Row> {
    let p = |m: &str| if who.is_empty() { m.to_string() } else { format!("{who}: {m}") };
    if !t.row {
        return Err(elog(p("must be fired for row")));
    }
    if !t.before {
        return Err(elog(p("must be fired before event")));
    }
    if t.delete {
        return Err(elog(p("cannot process DELETE events")));
    }
    t.new.clone().ok_or_else(|| elog("trigger without a row"))
}

/// autoinc(col, seq [, col, seq ...]): a NULL or 0 column takes the sequence's next value
/// (once more if that is 0).
fn autoinc(t: TriggerData, ctx: &ExecCtx) -> Result<Option<Row>> {
    let mut row = before_row(&t, "")?;
    let relname = &t.table.name;
    if t.args.is_empty() || t.args.len() % 2 != 0 {
        return Err(elog(format!("autoinc ({relname}): even number gt 0 of arguments was expected")));
    }
    for pair in t.args.chunks(2) {
        let Some(at) = fnumber(&t, &pair[0]) else {
            return Err(action_error(format!("\"{relname}\" has no attribute \"{}\"", pair[0])));
        };
        if t.table.columns[at].ty != DataType::Int4 {
            return Err(action_error(format!(
                "attribute \"{}\" of \"{relname}\" must be type INT4",
                pair[0]
            )));
        }
        if !matches!(row[at], Value::Null | Value::Int4(0)) {
            continue;
        }
        let mut v = nextval(&pair[1], ctx)? as i32;
        if v == 0 {
            v = nextval(&pair[1], ctx)? as i32;
        }
        row[at] = Value::Int4(v);
    }
    Ok(Some(row))
}

/// nextval(text): the sequence named so (search path, quoting) advanced.
fn nextval(name: &str, ctx: &ExecCtx) -> Result<i64> {
    let def = crate::sql::func::registry()
        .funcs
        .get("nextval")
        .and_then(|v| v.first().cloned())
        .ok_or_else(|| elog("nextval"))?;
    let fc = crate::sql::func::FnCtx {
        exec: ctx,
        ret: &DataType::Int8,
        args: &[],
        coll: None,
    };
    let v = (def.imp)(&[Value::Text(name.into())], &fc)?;
    Ok(v.as_i64().unwrap_or(0))
}

/// insert_username(col): the column is set to the current user's name.
fn insert_username(t: TriggerData, ctx: &ExecCtx) -> Result<Option<Row>> {
    let mut row = before_row(&t, "insert_username")?;
    let relname = &t.table.name;
    let [col] = t.args else {
        return Err(elog(format!("insert_username ({relname}): one argument was expected")));
    };
    let Some(at) = fnumber(&t, col) else {
        return Err(action_error(format!("\"{relname}\" has no attribute \"{col}\"")));
    };
    if t.table.columns[at].ty != DataType::Text {
        return Err(action_error(format!(
            "attribute \"{col}\" of \"{relname}\" must be type TEXT"
        )));
    }
    row[at] = Value::Text(ctx.role_name(ctx.current_user()).into());
    Ok(Some(row))
}

/// moddatetime(col): an UPDATE sets the column to the transaction's start ("now").
fn moddatetime(t: TriggerData, ctx: &ExecCtx) -> Result<Option<Row>> {
    if !t.row {
        return Err(elog("moddatetime: must be fired for row"));
    }
    if !t.before {
        return Err(elog("moddatetime: must be fired before event"));
    }
    if !t.update && !t.delete {
        return Err(elog("moddatetime: cannot process INSERT events"));
    }
    if t.delete {
        return Err(elog("moddatetime: cannot process DELETE events"));
    }
    let mut row = t.new.clone().ok_or_else(|| elog("trigger without a row"))?;
    let relname = &t.table.name;
    let [col] = t.args else {
        return Err(elog(format!("moddatetime ({relname}): A single argument was expected")));
    };
    let Some(at) = fnumber(&t, col) else {
        return Err(action_error(format!("\"{relname}\" has no attribute \"{col}\"")));
    };
    let now = ctx.txn_time();
    row[at] = match t.table.columns[at].ty {
        DataType::Timestamp(_) => Value::Timestamp(crate::sql::func::datetime::to_local(now, &ctx.fmt().tz)),
        DataType::Timestamptz(_) => Value::Timestamptz(now),
        _ => {
            return Err(action_error(format!(
                "attribute \"{col}\" of \"{relname}\" must be type TIMESTAMP or TIMESTAMPTZ"
            )));
        }
    };
    Ok(Some(row))
}

/// SPI_prepare(sql, types) + SPI_execp(values, tcount): what the statement processed (rows
/// a SELECT returned, rows DML changed). Errors in it carry its text as CONTEXT.
fn execp(ctx: &ExecCtx, sql: &str, vals: &[Value], types: &[DataType], tcount: Option<usize>) -> Result<u64> {
    let frame = Frame::Query {
        kind: "SQL statement",
        text: sql.to_string(),
        shift: 0,
    };
    ctx.sess.framed(frame, |_| {
        let stmts = crate::plpgsql::parse_spi(ctx, sql)?;
        let [st] = &stmts[..] else {
            return Err(Error::new(code::SYNTAX_ERROR, "cannot insert multiple commands into a prepared statement"));
        };
        let node = st.stmt.as_deref().ok_or_else(|| elog("empty query"))?;
        let select = matches!(node.node, Some(pg_query::NodeEnum::SelectStmt(_)));
        let child = ctx.child(vals);
        let r = (|| {
            let mut b = crate::sql::binder::Binder::new(&child, types.to_vec());
            let bq = b.bind_statement(node)?;
            crate::sql::binder::perms::check(&bq.perms, &child)?;
            let rows = crate::exec::collect(&bq.plan, &child, if select { tcount } else { None })?;
            Ok(if select { rows.len() as u64 } else { child.processed.get() })
        })();
        ctx.notices.borrow_mut().extend(child.notices.borrow_mut().drain(..));
        r
    })
}

/// The key columns' values and types (None: one is NULL, nothing to check).
fn key_values(t: &TriggerData, cols: &[String], row: &Row) -> Result<Option<(Vec<Value>, Vec<DataType>)>> {
    let (mut vals, mut types) = (Vec::new(), Vec::new());
    for c in cols {
        let Some(at) = fnumber(t, c) else {
            return Err(Error::new(
                code::UNDEFINED_COLUMN,
                format!("there is no attribute \"{c}\" in relation \"{}\"", t.table.name),
            ));
        };
        if row[at].is_null() {
            return Ok(None);
        }
        vals.push(row[at].clone());
        types.push(t.table.columns[at].ty.clone());
    }
    Ok(Some((vals, types)))
}

/// `col = $1 and col2 = $2 ` (refint's qualification).
fn qual(cols: &[String]) -> String {
    let parts: Vec<String> = cols.iter().enumerate().map(|(i, c)| format!("{c} = ${} ", i + 1)).collect();
    parts.join("and ")
}

/// check_primary_key(fkcol..., pktable, pkcol...): an AFTER INSERT / UPDATE trigger that
/// fails when the row's key has no row in the referenced table.
fn check_primary_key(t: TriggerData, ctx: &ExecCtx) -> Result<Option<Row>> {
    if !t.row {
        return Err(elog("check_primary_key: must be fired for row"));
    }
    if t.before {
        return Err(elog("check_primary_key: must be fired by AFTER trigger"));
    }
    if t.delete {
        return Err(elog("check_primary_key: cannot process DELETE events"));
    }
    let row = t.new.clone().ok_or_else(|| elog("trigger without a row"))?;
    if t.args.len() % 2 != 1 {
        return Err(elog("check_primary_key: odd number of arguments should be specified"));
    }
    let nkeys = t.args.len() / 2;
    let relname = &t.args[nkeys];
    let Some((vals, types)) = key_values(&t, &t.args[..nkeys], &row)? else {
        return Ok(Some(row));
    };
    let sql = format!("select 1 from {relname} where {}", qual(&t.args[nkeys + 1..]));
    if execp(ctx, &sql, &vals, &types, Some(1))? == 0 {
        return Err(action_error("tuple references non-existent key".into()).detail(format!(
            "Trigger \"{}\" found tuple referencing non-existent key in \"{relname}\".",
            t.name
        )));
    }
    Ok(Some(row))
}

/// check_foreign_key(nrefs, action, pkcol..., fktable, fkcol..., ...): an AFTER DELETE /
/// UPDATE trigger that restricts, cascades or sets to NULL the referencing rows of each
/// table.
fn check_foreign_key(t: TriggerData, ctx: &ExecCtx) -> Result<Option<Row>> {
    if !t.row {
        return Err(elog("check_foreign_key: must be fired for row"));
    }
    if !t.update && !t.delete {
        return Err(elog("check_foreign_key: cannot process INSERT events"));
    }
    if t.before {
        return Err(elog("check_foreign_key: must be fired by AFTER trigger"));
    }
    let old = t.old.clone().ok_or_else(|| elog("trigger without a row"))?;
    let new = if t.update { t.new.clone() } else { None };
    let nargs = t.args.len();
    if nargs < 5 {
        return Err(elog(format!("check_foreign_key: too short {nargs} (< 5) list of arguments")));
    }
    let nrefs = t.args[0].trim().parse::<i32>().map_err(|_| {
        Error::new(
            code::INVALID_TEXT_REPRESENTATION,
            format!("invalid input syntax for type integer: \"{}\"", t.args[0]),
        )
    })?;
    if nrefs < 1 {
        return Err(elog(format!("check_foreign_key: {nrefs} (< 1) number of references specified")));
    }
    let action = t.args[1].chars().next().map(|c| c.to_ascii_lowercase()).unwrap_or(' ');
    if !matches!(action, 'r' | 'c' | 's') {
        return Err(elog(format!("check_foreign_key: invalid action {}", t.args[1])));
    }
    let nrefs = nrefs as usize;
    let args = &t.args[2..];
    let n = args.len();
    let nkeys = n.saturating_sub(nrefs) / (nrefs + 1);
    if nkeys == 0 || n != nrefs + nkeys * (nrefs + 1) {
        return Err(elog(format!(
            "check_foreign_key: invalid number of arguments {nargs} for {nrefs} references"
        )));
    }
    let keys = &args[..nkeys];
    let ret = Some(new.clone().unwrap_or_else(|| old.clone()));
    let Some((vals, types)) = key_values(&t, keys, &old)? else {
        return Ok(ret);
    };
    // (SPI_getvalue: the key's text in the old and the new row.)
    let text = |r: &Row, k: &str| -> Option<String> {
        let at = fnumber(&t, k)?;
        (!r[at].is_null()).then(|| crate::types::text::format_value(&r[at], &t.table.columns[at].ty, ctx.fmt()))
    };
    let equal = new.as_ref().is_some_and(|nw| keys.iter().all(|k| text(&old, k) == text(nw, k)));
    let mut plans = Vec::with_capacity(nrefs);
    for r in args[nkeys..].chunks(nkeys + 1) {
        let (relname, fkcols) = (&r[0], &r[1..]);
        let head = match action {
            'r' => format!("select 1 from {relname} where "),
            'c' => match &new {
                Some(nw) => {
                    let sets: Vec<String> = keys
                        .iter()
                        .zip(fkcols)
                        .map(|(k, f)| {
                            let v = text(nw, k).map_or("NULL".to_string(), |v| quote_literal(&v));
                            format!(" {f} = {v} ")
                        })
                        .collect();
                    format!("update {relname} set {} where ", sets.join(", "))
                }
                None => format!("delete from {relname} where "),
            },
            _ => {
                let sets: Vec<String> = fkcols.iter().map(|f| format!("{f} = null")).collect();
                format!("update {relname} set {} where ", sets.join(", "))
            }
        };
        plans.push((relname.clone(), format!("{head}{}", qual(fkcols))));
    }
    if equal {
        return Ok(ret);
    }
    for (relname, sql) in plans {
        let processed = execp(ctx, &sql, &vals, &types, (action == 'r').then_some(1))?;
        if action == 'r' {
            if processed > 0 {
                return Err(action_error(format!("\"{}\": tuple is referenced in \"{relname}\"", t.name)));
            }
        } else {
            let operation = match (action, t.update) {
                ('c', true) => "updated",
                ('c', false) => "deleted",
                _ => "set to null",
            };
            let mut e = Error::new(
                code::SUCCESSFUL_COMPLETION,
                format!("{}: {processed} tuple(s) of {relname} are {operation}", t.name),
            );
            e.severity = crate::error::Severity::Notice;
            ctx.notice(e);
        }
    }
    Ok(ret)
}

/// quote_literal_cstr: `'...'` with quotes doubled, as `E'...'` when there's a backslash.
fn quote_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 3);
    if s.contains('\\') {
        out.push('E');
    }
    out.push('\'');
    for c in s.chars() {
        if c == '\'' || c == '\\' {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}
