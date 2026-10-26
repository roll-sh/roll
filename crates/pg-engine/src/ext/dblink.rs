//! dblink (contrib/dblink): connections to other PostgreSQL databases from SQL, over the
//! engine's own client (`crate::remote`), with dblink.c's messages and rules. Outbound
//! connections go only where the operator allows (`remote::outbound`).

use super::native::Native;
use crate::catalog::Catalog;
use crate::error::{Error, Result, code};
use crate::remote::conn::{self, Conn, PgResult, Status, TxStatus};
use crate::remote::conninfo;
use crate::sql::func::{FnCtx, SrfIter};
use crate::types::{DataType, Value};
use std::collections::BTreeMap;

/// A dblink connection (remoteConn).
pub struct RemoteConn {
    conn: Conn,
    open_cursors: i32,
    new_xact_for_cursor: bool,
}

/// The session's dblink connections: the unnamed one and the named ones.
#[derive(Default)]
pub struct Conns {
    unnamed: Option<RemoteConn>,
    named: BTreeMap<String, RemoteConn>,
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    Some(match sym {
        "dblink_connect" => Native::scalar(|a, c| dblink_connect(a, c, false)),
        // dblink_connect_u: SECURITY DEFINER, running as its (superuser) owner.
        "dblink_connect#secdef" => Native::scalar(|a, c| dblink_connect(a, c, true)),
        "dblink_disconnect" => Native::scalar(dblink_disconnect),
        "dblink_open" => Native::scalar(dblink_open),
        "dblink_close" => Native::scalar(dblink_close),
        "dblink_exec" => Native::scalar(dblink_exec),
        "dblink_send_query" => Native::scalar(dblink_send_query),
        "dblink_is_busy" => Native::scalar(dblink_is_busy),
        "dblink_cancel_query" => Native::scalar(dblink_cancel_query),
        "dblink_error_message" => Native::scalar(dblink_error_message),
        "dblink_get_connections" => Native::scalar(dblink_get_connections),
        "dblink_build_sql_insert" => Native::scalar(|a, c| build_sql(a, c, 'i')),
        "dblink_build_sql_update" => Native::scalar(|a, c| build_sql(a, c, 'u')),
        "dblink_build_sql_delete" => Native::scalar(|a, c| build_sql(a, c, 'd')),
        "dblink_current_query" => Native::scalar(dblink_current_query),
        "dblink_fdw_validator" => Native::scalar(|_, _| Ok(Value::Null)),
        "dblink_fetch" => Native::set(dblink_fetch),
        "dblink_record" => Native::set(|a, c| dblink_record(a, c, false)),
        "dblink_get_result" => Native::set(|a, c| dblink_record(a, c, true)),
        "dblink_get_pkey" => Native::set(dblink_get_pkey),
        "dblink_get_notify" => Native::set(dblink_get_notify),
        _ => return None,
    })
}

fn targ(a: &[Value], i: usize) -> String {
    a.get(i).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn barg(a: &[Value], i: usize) -> bool {
    a.get(i).and_then(Value::as_bool).unwrap_or(true)
}

fn is_bool(c: &FnCtx, i: usize) -> bool {
    matches!(c.arg_type(i), DataType::Bool)
}

fn superuser(c: &FnCtx) -> bool {
    c.exec.catalog().is_superuser(c.exec.current_user())
}

fn text(s: impl Into<std::sync::Arc<str>>) -> Value {
    Value::text(s)
}

/// truncate_identifier, silently.
fn truncated(name: &str) -> String {
    crate::catalog::truncate_ident(name)
}

fn conn_not_avail(name: Option<&str>) -> Error {
    Error::new(
        code::CONNECTION_DOES_NOT_EXIST,
        match name {
            Some(n) => format!("connection \"{n}\" not available"),
            None => "connection not available".into(),
        },
    )
}

/// escape_param_str: `\` and `'` get a backslash.
fn escape_param(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch == '\\' || ch == '\'' {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// is_valid_dblink_option: what libpq accepts, without debug options, client_encoding and
/// the OAuth ones; `user` and secret options in user mappings only, the rest in servers.
pub fn valid_option(option: &str, context: u32) -> bool {
    let Some(o) = conninfo::option(option) else {
        return false;
    };
    if o.dispchar.contains('D') || option == "client_encoding" || option.starts_with("oauth_") {
        return false;
    }
    if option == "user" || o.dispchar.contains('*') {
        context == USER_MAPPING_RELATION
    } else {
        context == FOREIGN_SERVER_RELATION
    }
}

pub const FOREIGN_DATA_WRAPPER_RELATION: u32 = 2328;
pub const FOREIGN_SERVER_RELATION: u32 = 1417;
pub const USER_MAPPING_RELATION: u32 = 1418;

fn split_opt(o: &str) -> (&str, &str) {
    o.split_once('=').unwrap_or((o, ""))
}

/// GetUserMapping: the user's own mapping, else PUBLIC's.
pub fn user_mapping(cat: &Catalog, user: u32, server: u32) -> Option<std::sync::Arc<crate::catalog::userobj::UserMapping>> {
    let mine = cat.user_mappings.values().find(|m| m.server == server && m.user == user);
    mine.or_else(|| cat.user_mappings.values().find(|m| m.server == server && m.user == 0)).cloned()
}

/// get_connect_string: the connection string of foreign server `name` for the current user
/// (its wrapper's, its own and the user mapping's connection options), None when there's no
/// such server.
fn connect_string(c: &FnCtx, name: &str) -> Result<Option<String>> {
    let cat = c.exec.catalog();
    let name = truncated(name);
    let Some(srv) = cat.servers.values().find(|s| s.name == name).cloned() else {
        return Ok(None);
    };
    let user = c.exec.current_user();
    let Some(um) = user_mapping(&cat, user, srv.oid) else {
        return Err(Error::new(
            code::UNDEFINED_OBJECT,
            format!("user mapping not found for user \"{}\", server \"{}\"", c.exec.role_name(user), srv.name),
        ));
    };
    let (p, _) = cat.obj_mask(crate::catalog::acl::ObjKind::ForeignServer, srv.oid, user, c.exec.db_owner());
    if p & crate::catalog::acl::USAGE == 0 {
        return Err(Error::new(
            code::INSUFFICIENT_PRIVILEGE,
            format!("permission denied for foreign server {}", srv.name),
        ));
    }
    let mut buf = String::new();
    let fdw_opts = cat.fdws.get(&srv.fdw).and_then(|f| f.options.clone()).unwrap_or_default();
    for (opts, ctx) in [
        (fdw_opts, FOREIGN_DATA_WRAPPER_RELATION),
        (srv.options.clone().unwrap_or_default(), FOREIGN_SERVER_RELATION),
        (um.options.clone().unwrap_or_default(), USER_MAPPING_RELATION),
    ] {
        for o in &opts {
            let (k, v) = split_opt(o);
            if valid_option(k, ctx) {
                buf.push_str(&format!("{k}='{}' ", escape_param(v)));
            }
        }
    }
    Ok(Some(buf))
}

/// dblink_connstr_check: a non-superuser's connection string must give a password.
fn connstr_check(c: &FnCtx, connstr: &str) -> Result<()> {
    if superuser(c) || conninfo::parse(connstr).is_ok_and(|i| conninfo::has_password(&i)) {
        return Ok(());
    }
    Err(Error::new(code::S_R_E_PROHIBITED_SQL_STATEMENT_ATTEMPTED, "password or GSSAPI delegated credentials required")
        .detail("Non-superusers must provide a password in the connection string or send delegated GSSAPI credentials."))
}

/// dblink_security_check: a non-superuser's connection must have used the password given.
fn security_check(c: &FnCtx, conn: &Conn, connstr: &str) -> Result<()> {
    if superuser(c) || (conn.used_password && conninfo::parse(connstr).is_ok_and(|i| conninfo::has_password(&i))) {
        return Ok(());
    }
    Err(Error::new(code::S_R_E_PROHIBITED_SQL_STATEMENT_ATTEMPTED, "password or GSSAPI delegated credentials required")
        .detail("Non-superusers may only connect using credentials they provide, eg: password in connection string or delegated GSSAPI credentials")
        .hint("Ensure provided credentials match target server's authentication method."))
}

/// libpqsrv_connect, failing with dblink's "could not establish connection".
fn connect(c: &FnCtx, connstr: &str) -> Result<Conn> {
    let fail = |m: String| {
        Error::new(code::SQLCLIENT_UNABLE_TO_ESTABLISH_SQLCONNECTION, "could not establish connection").detail(m)
    };
    let info = conninfo::parse(connstr).map_err(fail)?;
    conn::connect(&info, &crate::remote::env(c.exec.sess)).map_err(fail)
}

/// dblink_get_conn: the named connection `s`, or a new one to the server or connection
/// string `s` (closed after the call). The name is None for a new one.
fn get_conn(c: &FnCtx, s: &str) -> Result<(Option<String>, Option<Conn>)> {
    if c.exec.sess.remote.borrow().dblink.named.contains_key(&truncated(s)) {
        return Ok((Some(s.to_string()), None));
    }
    let connstr = connect_string(c, s)?.unwrap_or_else(|| s.to_string());
    connstr_check(c, &connstr)?;
    let conn = connect(c, &connstr)?;
    security_check(c, &conn, &connstr)?;
    Ok((None, Some(conn)))
}

/// Run `f` on the named (or, `name` None, the unnamed) persistent connection.
fn with_conn<T>(c: &FnCtx, name: Option<&str>, f: impl FnOnce(&mut RemoteConn) -> Result<T>) -> Result<T> {
    let mut r = c.exec.sess.remote.borrow_mut();
    let rc = match name {
        Some(n) => r.dblink.named.get_mut(&truncated(n)),
        None => r.dblink.unnamed.as_mut(),
    };
    match rc {
        Some(rc) => f(rc),
        None => Err(conn_not_avail(name)),
    }
}

/// Where a call's query runs: a persistent connection, or one of its own.
enum Target {
    Persistent(Option<String>),
    Temporary(Box<Conn>),
}

fn target(c: &FnCtx, conname_or_str: Option<&str>) -> Result<(Target, Option<String>)> {
    match conname_or_str {
        None => Ok((Target::Persistent(None), None)),
        Some(s) => match get_conn(c, s)? {
            (Some(n), _) => Ok((Target::Persistent(Some(n.clone())), Some(n))),
            (None, Some(conn)) => Ok((Target::Temporary(Box::new(conn)), None)),
            (None, None) => Err(conn_not_avail(None)),
        },
    }
}

fn run_on<T>(c: &FnCtx, t: &mut Target, f: impl FnOnce(&mut Conn) -> Result<T>) -> Result<T> {
    match t {
        Target::Persistent(n) => with_conn(c, n.as_deref(), |rc| f(&mut rc.conn)),
        Target::Temporary(conn) => f(conn),
    }
}

/// dblink_res_error: the remote error as an ERROR (`fail`) or a NOTICE, with a context line
/// saying what dblink was doing on which connection.
fn res_error(c: &FnCtx, conn: &Conn, conname: Option<&str>, res: &PgResult, fail: bool, what: &str) -> Result<()> {
    let f = res.error.clone().unwrap_or_default();
    let sqlstate = crate::remote::sqlstate(f.sqlstate(), code::CONNECTION_FAILURE);
    let primary = match f.message() {
        Some(m) => m.to_string(),
        None => conn.error_message.trim_end_matches('\n').to_string(),
    };
    let primary = if primary.is_empty() { "could not obtain message string for remote error".to_string() } else { primary };
    let mut e = Error::new(sqlstate, primary);
    if let Some(d) = f.detail() {
        e = e.detail(d);
    }
    if let Some(h) = f.hint() {
        e = e.hint(h);
    }
    let here = match conname {
        Some(n) => format!("{what} on dblink connection named \"{n}\""),
        None => format!("{what} on unnamed dblink connection"),
    };
    e = e.context(match f.context() {
        Some(rc) => format!("{rc}\n{here}"),
        None => here,
    });
    if fail {
        return Err(e);
    }
    e.severity = crate::error::Severity::Notice;
    c.exec.notice(e);
    Ok(())
}

/// dblink_res_internalerror.
fn internal_error(conn: &Conn, what: &str) -> Error {
    crate::error::internal(format!("{what}: {}", conn.error_message.trim_end_matches('\n')))
}

fn dblink_connect(a: &[Value], c: &FnCtx, definer: bool) -> Result<Value> {
    let (name, conname_or_str) = if a.len() == 2 { (Some(targ(a, 0)), targ(a, 1)) } else { (None, targ(a, 0)) };
    let connstr = connect_string(c, &conname_or_str)?.unwrap_or(conname_or_str);
    if !definer {
        connstr_check(c, &connstr)?;
    }
    if let Some(n) = &name
        && c.exec.sess.remote.borrow().dblink.named.contains_key(&truncated(n))
    {
        return Err(Error::new(code::DUPLICATE_OBJECT, "duplicate connection name"));
    }
    let conn = connect(c, &connstr)?;
    if !definer {
        security_check(c, &conn, &connstr)?;
    }
    let rc = RemoteConn {
        conn,
        open_cursors: 0,
        new_xact_for_cursor: false,
    };
    let mut r = c.exec.sess.remote.borrow_mut();
    match name {
        Some(n) => {
            r.dblink.named.insert(truncated(&n), rc);
        }
        None => r.dblink.unnamed = Some(rc),
    }
    Ok(text("OK"))
}

fn dblink_disconnect(a: &[Value], c: &FnCtx) -> Result<Value> {
    let mut r = c.exec.sess.remote.borrow_mut();
    let gone = match a.first() {
        Some(_) => r.dblink.named.remove(&truncated(&targ(a, 0))),
        None => r.dblink.unnamed.take(),
    };
    match gone {
        Some(_) => Ok(text("OK")),
        None => Err(conn_not_avail(a.first().map(|_| targ(a, 0)).as_deref())),
    }
}

fn dblink_open(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (conname, curname, sql, fail) = match a.len() {
        2 => (None, targ(a, 0), targ(a, 1), true),
        3 if is_bool(c, 2) => (None, targ(a, 0), targ(a, 1), barg(a, 2)),
        3 => (Some(targ(a, 0)), targ(a, 1), targ(a, 2), true),
        _ => (Some(targ(a, 0)), targ(a, 1), targ(a, 2), barg(a, 3)),
    };
    let intr = crate::remote::interrupts(c.exec);
    let res = with_conn(c, conname.as_deref(), |rc| {
        if rc.conn.tx_status() == TxStatus::Idle {
            let r = rc.conn.exec("BEGIN", &intr)?;
            if r.status != Status::CommandOk {
                return Err(internal_error(&rc.conn, "begin error"));
            }
            rc.new_xact_for_cursor = true;
            rc.open_cursors = 0;
        }
        if rc.new_xact_for_cursor {
            rc.open_cursors += 1;
        }
        let r = rc.conn.exec(&format!("DECLARE {curname} CURSOR FOR {sql}"), &intr)?;
        Ok((r, rc.conn.error_message.clone()))
    })?;
    if res.0.status != Status::CommandOk {
        with_conn(c, conname.as_deref(), |rc| {
            res_error(c, &rc.conn, conname.as_deref(), &res.0, fail, &format!("while opening cursor \"{curname}\""))
        })?;
        return Ok(text("ERROR"));
    }
    Ok(text("OK"))
}

fn dblink_close(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (conname, curname, fail) = match a.len() {
        1 => (None, targ(a, 0), true),
        2 if is_bool(c, 1) => (None, targ(a, 0), barg(a, 1)),
        2 => (Some(targ(a, 0)), targ(a, 1), true),
        _ => (Some(targ(a, 0)), targ(a, 1), barg(a, 2)),
    };
    let intr = crate::remote::interrupts(c.exec);
    let res = with_conn(c, conname.as_deref(), |rc| rc.conn.exec(&format!("CLOSE {curname}"), &intr))?;
    if res.status != Status::CommandOk {
        with_conn(c, conname.as_deref(), |rc| {
            res_error(c, &rc.conn, conname.as_deref(), &res, fail, &format!("while closing cursor \"{curname}\""))
        })?;
        return Ok(text("ERROR"));
    }
    with_conn(c, conname.as_deref(), |rc| {
        if rc.new_xact_for_cursor {
            rc.open_cursors -= 1;
            if rc.open_cursors == 0 {
                rc.new_xact_for_cursor = false;
                let r = rc.conn.exec("COMMIT", &intr)?;
                if r.status != Status::CommandOk {
                    return Err(internal_error(&rc.conn, "commit error"));
                }
            }
        }
        Ok(())
    })?;
    Ok(text("OK"))
}

fn dblink_exec(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (conname_or_str, sql, fail) = match a.len() {
        3 => (Some(targ(a, 0)), targ(a, 1), barg(a, 2)),
        2 if is_bool(c, 1) => (None, targ(a, 0), barg(a, 1)),
        2 => (Some(targ(a, 0)), targ(a, 1), true),
        _ => (None, targ(a, 0), true),
    };
    let (mut t, conname) = target(c, conname_or_str.as_deref())?;
    let intr = crate::remote::interrupts(c.exec);
    run_on(c, &mut t, |conn| {
        let res = conn.exec(&sql, &intr)?;
        match res.status {
            Status::CommandOk => Ok(text(res.cmd_status.clone())),
            Status::TuplesOk => Err(Error::new(
                code::S_R_E_PROHIBITED_SQL_STATEMENT_ATTEMPTED,
                "statement returning results not allowed",
            )),
            _ => {
                res_error(c, conn, conname.as_deref(), &res, fail, "while executing command")?;
                Ok(text("ERROR"))
            }
        }
    })
}

/// The columns the call's FROM clause gives (its result row type).
fn result_types(c: &FnCtx) -> Vec<DataType> {
    match c.ret {
        DataType::Row(r) => r.fields.iter().map(|f| f.1.clone()).collect(),
        t => vec![t.clone()],
    }
}

fn rowtype_mismatch() -> Error {
    Error::new(
        code::DATATYPE_MISMATCH,
        "remote query result rowtype does not match the specified FROM clause rowtype",
    )
}

/// applyRemoteGucs: values are read under the remote session's DateStyle and IntervalStyle.
fn remote_format(c: &FnCtx, conn: &Conn) -> crate::types::text::FormatCtx {
    let mut fmt = c.exec.fmt().clone();
    let settings = c.exec.sess.settings.borrow();
    if let Some(ds) = conn.parameter("DateStyle")
        && settings.get_str("DateStyle") != Some(ds)
    {
        fmt.date_style = crate::types::datetime::DateStyle::parse(ds);
    }
    if let Some(is) = conn.parameter("IntervalStyle")
        && settings.get_str("IntervalStyle") != Some(is)
    {
        fmt.interval_style = match is {
            "postgres_verbose" => crate::types::text::IntervalStyle::PostgresVerbose,
            "sql_standard" => crate::types::text::IntervalStyle::SqlStandard,
            "iso_8601" => crate::types::text::IntervalStyle::Iso8601,
            _ => crate::types::text::IntervalStyle::Postgres,
        };
    }
    fmt
}

/// materializeResult: a result's rows as the FROM clause's row type (a command's status as
/// one text column).
fn materialize(c: &FnCtx, conn: &Conn, res: &PgResult) -> Result<SrfIter> {
    let types = result_types(c);
    let rows: Vec<Vec<Option<String>>> = if res.status == Status::CommandOk {
        if types.len() != 1 {
            return Err(rowtype_mismatch());
        }
        vec![vec![Some(res.cmd_status.clone())]]
    } else {
        if res.fields.len() != types.len() {
            return Err(rowtype_mismatch());
        }
        res.rows.clone()
    };
    let fmt = if res.status == Status::CommandOk { c.exec.fmt().clone() } else { remote_format(c, conn) };
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let row: Result<Vec<Value>> = r
            .iter()
            .zip(&types)
            .map(|(v, t)| match v {
                None => Ok(Value::Null),
                Some(s) => crate::types::text::parse_value(s, t, &fmt),
            })
            .collect();
        out.push(row?);
    }
    Ok(Box::new(out.into_iter().map(Ok)))
}

fn empty() -> SrfIter {
    Box::new(std::iter::empty())
}

fn dblink_record(a: &[Value], c: &FnCtx, is_async: bool) -> Result<SrfIter> {
    let intr = crate::remote::interrupts(c.exec);
    if is_async {
        let conname = targ(a, 0);
        let fail = a.len() < 2 || barg(a, 1);
        return with_conn(c, Some(&conname), |rc| {
            let Some(res) = rc.conn.get_result(&intr)? else {
                return Ok(empty());
            };
            if !res.ok() {
                res_error(c, &rc.conn, Some(&conname), &res, fail, "while executing query")?;
                return Ok(empty());
            }
            materialize(c, &rc.conn, &res)
        });
    }
    let (conname_or_str, sql, fail) = match a.len() {
        3 => (Some(targ(a, 0)), targ(a, 1), barg(a, 2)),
        2 if is_bool(c, 1) => (None, targ(a, 0), barg(a, 1)),
        2 => (Some(targ(a, 0)), targ(a, 1), true),
        _ => (None, targ(a, 0), true),
    };
    let (mut t, conname) = target(c, conname_or_str.as_deref())?;
    run_on(c, &mut t, |conn| {
        let res = conn.exec(&sql, &intr)?;
        if !res.ok() {
            res_error(c, conn, conname.as_deref(), &res, fail, "while executing query")?;
            return Ok(empty());
        }
        materialize(c, conn, &res)
    })
}

fn dblink_fetch(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let int = |i: usize| a.get(i).and_then(Value::as_i64).unwrap_or(0);
    let (conname, curname, howmany, fail) = match a.len() {
        4 => (Some(targ(a, 0)), targ(a, 1), int(2), barg(a, 3)),
        3 if is_bool(c, 2) => (None, targ(a, 0), int(1), barg(a, 2)),
        3 => (Some(targ(a, 0)), targ(a, 1), int(2), true),
        _ => (None, targ(a, 0), int(1), true),
    };
    let intr = crate::remote::interrupts(c.exec);
    with_conn(c, conname.as_deref(), |rc| {
        let res = rc.conn.exec(&format!("FETCH {howmany} FROM {curname}"), &intr)?;
        match res.status {
            Status::TuplesOk => materialize(c, &rc.conn, &res),
            Status::CommandOk => Err(Error::new(
                code::INVALID_CURSOR_NAME,
                format!("cursor \"{curname}\" does not exist"),
            )),
            _ => {
                res_error(c, &rc.conn, conname.as_deref(), &res, fail, &format!("while fetching from cursor \"{curname}\""))?;
                Ok(empty())
            }
        }
    })
}


fn dblink_send_query(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (conname, sql) = (targ(a, 0), targ(a, 1));
    with_conn(c, Some(&conname), |rc| {
        Ok(Value::Int4(match rc.conn.send_query(&sql) {
            Ok(()) => 1,
            Err(m) => {
                let mut e = Error::new(code::WARNING, format!("could not send query: {}", m.trim_end_matches('\n')));
                e.severity = crate::error::Severity::Notice;
                c.exec.notice(e);
                0
            }
        }))
    })
}

fn dblink_is_busy(a: &[Value], c: &FnCtx) -> Result<Value> {
    with_conn(c, Some(&targ(a, 0)), |rc| {
        rc.conn.consume_input();
        Ok(Value::Int4(i32::from(rc.conn.is_busy())))
    })
}

fn dblink_cancel_query(a: &[Value], c: &FnCtx) -> Result<Value> {
    with_conn(c, Some(&targ(a, 0)), |rc| {
        Ok(text(match rc.conn.cancel() {
            Ok(()) => "OK".to_string(),
            Err(m) => m,
        }))
    })
}

fn dblink_error_message(a: &[Value], c: &FnCtx) -> Result<Value> {
    with_conn(c, Some(&targ(a, 0)), |rc| {
        let m = rc.conn.error_message.trim_end_matches('\n');
        Ok(text(if m.is_empty() { "OK" } else { m }))
    })
}

fn dblink_get_connections(_: &[Value], c: &FnCtx) -> Result<Value> {
    let r = c.exec.sess.remote.borrow();
    if r.dblink.named.is_empty() {
        return Ok(Value::Null);
    }
    let items = r.dblink.named.keys().map(|k| text(k.as_str())).collect();
    Ok(Value::Array(std::sync::Arc::new(crate::types::array::ArrayValue::from_vec(DataType::Text, items))))
}

/// An alias of current_query() (which the engine reports as NULL).
fn dblink_current_query(_: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Null)
}

fn dblink_get_notify(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let name = a.first().map(|_| targ(a, 0));
    with_conn(c, name.as_deref(), |rc| {
        rc.conn.consume_input();
        let mut out = Vec::new();
        while let Some(n) = rc.conn.notifies() {
            out.push(vec![text(n.channel), Value::Int4(n.pid), text(n.payload)]);
            rc.conn.consume_input();
        }
        Ok(Box::new(out.into_iter().map(Ok)) as SrfIter)
    })
}

/// get_rel_from_relname: the table a (possibly qualified) name names, which the user may
/// SELECT from.
fn relation(c: &FnCtx, name: &str) -> Result<std::sync::Arc<crate::catalog::Table>> {
    let names = c.exec.fmt().names.clone().ok_or_else(|| crate::error::internal("no name resolver"))?;
    let oid = names.rel_oid(name)?;
    let cat = c.exec.catalog();
    let t = cat.table(oid).cloned().ok_or_else(|| crate::error::internal(format!("\"{name}\" is not a table")))?;
    let mask = cat.rel_mask(oid, c.exec.current_user(), c.exec.db_owner()).map_or(0, |m| m.0);
    if mask & crate::catalog::acl::SELECT == 0 {
        return Err(Error::new(code::INSUFFICIENT_PRIVILEGE, format!("permission denied for table {}", t.name)));
    }
    Ok(t)
}

fn dblink_get_pkey(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let t = relation(c, &targ(a, 0))?;
    let out: Vec<Vec<Value>> = t
        .pkey
        .clone()
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(i, col)| vec![Value::Int4(i as i32 + 1), text(t.columns[*col].name.as_str())])
        .collect();
    Ok(Box::new(out.into_iter().map(Ok)))
}

/// The text values of a text[] argument.
fn text_items(v: &Value) -> Vec<Option<String>> {
    v.as_array()
        .map(|a| a.items.iter().map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// validate_pkattnums: 1-based logical column numbers to physical indexes.
fn pk_columns(t: &crate::catalog::Table, v: &Value, n: i64) -> Result<Vec<usize>> {
    let nums: Vec<i64> = v.as_array().map(|a| a.items.iter().filter_map(Value::as_i64).collect()).unwrap_or_default();
    let n = n.min(nums.len() as i64);
    if n <= 0 {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "number of key attributes must be > 0"));
    }
    let bad = |k: i64| Error::new(code::INVALID_PARAMETER_VALUE, format!("invalid attribute number {k}"));
    let mut out = Vec::new();
    for &k in &nums[..n as usize] {
        if k <= 0 || k > t.columns.len() as i64 {
            return Err(bad(k));
        }
        let phys = t.columns.iter().enumerate().filter(|(_, col)| !col.dropped).nth(k as usize - 1).map(|(i, _)| i);
        out.push(phys.ok_or_else(|| bad(k))?);
    }
    Ok(out)
}

/// dblink_build_sql_insert / _update / _delete: a statement for the row whose key columns
/// have the source values, with the target values in the key columns.
fn build_sql(a: &[Value], c: &FnCtx, kind: char) -> Result<Value> {
    let t = relation(c, &targ(a, 0))?;
    let keys = pk_columns(&t, &a[1], a[2].as_i64().unwrap_or(0))?;
    let (src, tgt) = match kind {
        'd' => (None, text_items(&a[3])),
        _ => (Some(text_items(&a[3])), text_items(&a[4])),
    };
    let mismatch = |w: &str| {
        Error::new(code::ARRAY_SUBSCRIPT_ERROR, format!("{w} key array length must match number of key attributes"))
    };
    if src.as_ref().is_some_and(|s| s.len() != keys.len()) {
        return Err(mismatch("source"));
    }
    if tgt.len() != keys.len() {
        return Err(mismatch("target"));
    }
    let names = c.exec.fmt().names.clone();
    let relname = names.and_then(|n| n.rel_name(t.oid)).unwrap_or_else(|| crate::catalog::quote_ident(&t.name));
    let q = |s: &str| crate::catalog::quote_ident(s);
    let lit = |v: &Option<String>| v.as_deref().map_or("NULL".to_string(), crate::sql::func::string::quote_literal_str);
    let cond = |vals: &[Option<String>]| -> String {
        keys.iter()
            .zip(vals)
            .map(|(k, v)| match v {
                Some(v) => format!("{} = {}", q(&t.columns[*k].name), crate::sql::func::string::quote_literal_str(v)),
                None => format!("{} IS NULL", q(&t.columns[*k].name)),
            })
            .collect::<Vec<_>>()
            .join(" AND ")
    };
    if kind == 'd' {
        return Ok(text(format!("DELETE FROM {relname} WHERE {}", cond(&tgt))));
    }
    // get_tuple_of_interest: the source row.
    let src = src.unwrap_or_default();
    let select = format!(
        "SELECT {} FROM {relname} WHERE {}",
        t.columns.iter().map(|col| if col.dropped { "NULL".to_string() } else { q(&col.name) }).collect::<Vec<_>>().join(", "),
        cond(&src)
    );
    let (rows, cols) = crate::plpgsql::spi_query(c.exec, &select)?;
    if rows.len() > 1 {
        return Err(Error::new(code::CARDINALITY_VIOLATION, "source criteria matched more than one record"));
    }
    let Some(row) = rows.first() else {
        return Err(Error::new(code::CARDINALITY_VIOLATION, "source row not found"));
    };
    let value = |i: usize| -> Option<String> {
        if let Some(p) = keys.iter().position(|k| *k == i) {
            return tgt[p].clone();
        }
        (!row[i].is_null()).then(|| crate::types::text::format_value(&row[i], &cols[i].ty, c.exec.fmt()))
    };
    let live: Vec<usize> = (0..t.columns.len()).filter(|i| !t.columns[*i].dropped).collect();
    let sql = if kind == 'i' {
        format!(
            "INSERT INTO {relname}({}) VALUES({})",
            live.iter().map(|i| q(&t.columns[*i].name)).collect::<Vec<_>>().join(","),
            live.iter().map(|i| lit(&value(*i))).collect::<Vec<_>>().join(",")
        )
    } else {
        format!(
            "UPDATE {relname} SET {} WHERE {}",
            live.iter().map(|i| format!("{} = {}", q(&t.columns[*i].name), lit(&value(*i)))).collect::<Vec<_>>().join(", "),
            cond(&tgt)
        )
    };
    Ok(text(sql))
}

/// dblink_fdw_validator: option names only (values are checked when connecting).
pub fn validate(v: &super::native::FdwValidate) -> Result<()> {
    for o in v.options {
        let (k, val) = split_opt(o);
        let scram = k == "use_scram_passthrough"
            && matches!(v.catalog, FOREIGN_SERVER_RELATION | USER_MAPPING_RELATION);
        if !scram && !valid_option(k, v.catalog) {
            let valid: Vec<&str> =
                conninfo::OPTIONS.iter().map(|o| o.keyword).filter(|o| valid_option(o, v.catalog)).collect();
            let e = Error::new(code::FDW_OPTION_NAME_NOT_FOUND, format!("invalid option \"{k}\""));
            return Err(if valid.is_empty() {
                e.hint("There are no valid options in this context.")
            } else {
                match crate::session::ddl::closest_match(k, valid.iter().copied()) {
                    Some(m) => e.hint(format!("Perhaps you meant the option \"{m}\".")),
                    None => e,
                }
            });
        }
        if scram {
            def_boolean(k, val)?;
        }
    }
    Ok(())
}

/// defGetBoolean of a string option.
pub fn def_boolean(name: &str, v: &str) -> Result<bool> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "on" | "1" => Ok(true),
        "false" | "off" | "0" => Ok(false),
        _ => Err(Error::new(code::SYNTAX_ERROR, format!("{name} requires a Boolean value"))),
    }
}
