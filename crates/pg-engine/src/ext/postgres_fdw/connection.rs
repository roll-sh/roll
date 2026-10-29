//! postgres_fdw's connections (connection.c): one per user mapping, cached for the session,
//! with a remote transaction (and savepoints) following the local one.

use super::option;
use crate::catalog::Catalog;
use crate::catalog::userobj::{ForeignServer, UserMapping};
use crate::error::{Error, Result, code};
use crate::exec::ExecCtx;
use crate::remote::conn::{self, Conn, PgResult, Status, TxStatus};
use crate::remote::conninfo::ConnInfo;
use std::collections::BTreeMap;
use std::sync::Arc;

/// A cached connection (ConnCacheEntry), by user mapping oid.
pub struct Entry {
    pub conn: Option<Conn>,
    pub server: u32,
    /// The server and mapping it was made with (a change invalidates it).
    server_def: Arc<ForeignServer>,
    mapping_def: Arc<UserMapping>,
    /// 0: no remote transaction; 1: one; n: savepoints up to s<n>.
    pub xact_depth: usize,
    pub have_prep_stmt: bool,
    pub have_error: bool,
    pub changing_xact_state: bool,
    pub invalidated: bool,
    keep_connections: bool,
}

/// The session's postgres_fdw state.
#[derive(Default)]
pub struct Conns {
    pub entries: BTreeMap<u32, Entry>,
    /// A connection was used in the current transaction (xact_got_connection).
    pub xact_got_connection: bool,
    pub cursor_number: u32,
    pub prep_stmt_number: u32,
}

/// What a connection is for: its server, user mapping and the user it maps.
#[derive(Clone)]
pub struct Mapping {
    pub server: Arc<ForeignServer>,
    pub mapping: Arc<UserMapping>,
    /// The user the mapping is for (the checked-as user: PUBLIC mappings too).
    pub user: u32,
}

/// GetUserMapping: the mapping of `user` (or PUBLIC's) on `server`.
pub fn user_mapping(cat: &Catalog, user: u32, server: u32) -> Result<Mapping> {
    let srv = cat.servers.get(&server).cloned().ok_or_else(|| {
        Error::new(code::UNDEFINED_OBJECT, format!("foreign server with OID {server} does not exist"))
    })?;
    let Some(m) = crate::ext::dblink::user_mapping(cat, user, server) else {
        let name = if user == 0 { "public".to_string() } else { cat.roles.get(&user).map_or(user.to_string(), |r| r.name.clone()) };
        return Err(Error::new(
            code::UNDEFINED_OBJECT,
            format!("user mapping not found for user \"{name}\", server \"{}\"", srv.name),
        ));
    };
    Ok(Mapping { server: srv, mapping: m, user })
}

fn password_required(m: &UserMapping) -> bool {
    option::get(m.options.as_ref(), "password_required")
        .is_none_or(|v| crate::ext::dblink::def_boolean("password_required", v).unwrap_or(true))
}

fn server_bool(s: &ForeignServer, name: &str, default: bool) -> bool {
    option::get(s.options.as_ref(), name).map_or(default, |v| crate::ext::dblink::def_boolean(name, v).unwrap_or(default))
}

/// pgfdw_report_error: a remote error (or the connection's) as a local one, with the remote
/// SQL in its context.
pub fn report(res: &PgResult, conn: &Conn, sql: Option<&str>) -> Error {
    let f = res.error.clone().unwrap_or_default();
    let primary = match f.message() {
        Some(m) => m.to_string(),
        None => conn.error_message.trim_end_matches('\n').to_string(),
    };
    let primary = if primary.is_empty() { "could not obtain message string for remote error".into() } else { primary };
    let mut e = Error::new(crate::remote::sqlstate(f.sqlstate(), code::CONNECTION_FAILURE), primary);
    if let Some(d) = f.detail() {
        e = e.detail(d);
    }
    if let Some(h) = f.hint() {
        e = e.hint(h);
    }
    let mut ctx = Vec::new();
    if let Some(c) = f.context() {
        ctx.push(c.to_string());
    }
    if let Some(s) = sql {
        ctx.push(format!("remote SQL command: {s}"));
    }
    if !ctx.is_empty() {
        e = e.context(ctx.join("\n"));
    }
    e
}

/// do_sql_command: a command that must succeed.
pub fn command(conn: &mut Conn, sql: &str, ctx: &ExecCtx) -> Result<()> {
    let intr = crate::remote::interrupts(ctx);
    let res = conn.exec(sql, &intr)?;
    if res.status != Status::CommandOk {
        return Err(report(&res, conn, Some(sql)));
    }
    Ok(())
}

/// connect_pg_server: the server's and mapping's libpq options, application_name, the
/// password checks, then configure_remote_session.
fn connect(ctx: &ExecCtx, m: &Mapping) -> Result<Conn> {
    let cat = ctx.catalog();
    let superuser = cat.is_superuser(m.user);
    let mut info = ConnInfo::default();
    for o in m.server.options.iter().flatten().chain(m.mapping.options.iter().flatten()) {
        let (k, v) = option::split(o);
        if option::is_libpq(k) {
            info.values.push((k.to_string(), v.to_string()));
        }
    }
    let app = ctx.sess.settings.borrow().get_str("postgres_fdw.application_name").map(str::to_string);
    if let Some(a) = app.filter(|a| !a.is_empty()) {
        info.values.push(("application_name".into(), a));
    }
    // (The last application_name given wins, its escapes replaced; empty ones are dropped.)
    if let Some(i) = info.values.iter().rposition(|(k, _)| k == "application_name") {
        let v = appname(&info.values[i].1, ctx);
        if v.is_empty() {
            info.values.remove(i);
        } else {
            info.values[i].1 = v;
        }
    }
    let mut merged = ConnInfo::default();
    for (k, v) in &info.values {
        merged.set(k, v);
    }
    merged.set("fallback_application_name", "postgres_fdw");
    let has_pw = merged.nonempty("password").is_some();
    // check_conn_params.
    if !superuser && !has_pw && password_required(&m.mapping) {
        return Err(Error::new(code::S_R_E_PROHIBITED_SQL_STATEMENT_ATTEMPTED, "password or GSSAPI delegated credentials required")
            .detail("Non-superusers must delegate GSSAPI credentials, provide a password, or enable SCRAM pass-through in user mapping."));
    }
    let mut c = conn::connect(&merged, &crate::remote::env(ctx.sess)).map_err(|msg| {
        Error::new(code::SQLCLIENT_UNABLE_TO_ESTABLISH_SQLCONNECTION, format!("could not connect to server \"{}\"", m.server.name))
            .detail(msg)
    })?;
    // pgfdw_security_check.
    if !superuser && password_required(&m.mapping) && !(c.used_password && has_pw) {
        return Err(Error::new(code::S_R_E_PROHIBITED_SQL_STATEMENT_ATTEMPTED, "password or GSSAPI delegated credentials required")
            .detail("Non-superuser cannot connect if the server does not request a password or use GSSAPI with delegated credentials.")
            .hint("Target server's authentication method must be changed or password_required=false set in the user mapping attributes."));
    }
    for sql in [
        "SET search_path = pg_catalog",
        "SET timezone = 'GMT'",
        "SET datestyle = ISO",
        "SET intervalstyle = postgres",
        "SET extra_float_digits = 3",
    ] {
        command(&mut c, sql, ctx)?;
    }
    Ok(c)
}

/// process_pgfdw_appname: %a application_name, %c session id, %C cluster_name, %d database,
/// %p pid, %u user, %% a percent sign.
fn appname(s: &str, ctx: &ExecCtx) -> String {
    let settings = ctx.sess.settings.borrow();
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(ch) = it.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match it.next() {
            None => break,
            Some('%') => out.push('%'),
            Some('a') => out.push_str(settings.get_str("application_name").unwrap_or("")),
            Some('c') => out.push_str(&format!("{:x}.{:x}", ctx.sess.backend_start / 1_000_000 + 946_684_800, ctx.sess.pid)),
            Some('C') => out.push_str(settings.get_str("cluster_name").unwrap_or("")),
            Some('d') => out.push_str(&ctx.sess.database),
            Some('p') => out.push_str(&ctx.sess.pid.to_string()),
            Some('u') => out.push_str(&ctx.sess.user),
            Some(_) => {}
        }
    }
    out
}

/// pgfdw_inval_callback, run when a connection is looked at: an entry whose server or user
/// mapping changed (or went) since it connected is closed, or marked invalid while its
/// remote transaction is open.
pub fn sweep(st: &mut Conns, cat: &Catalog) {
    for (key, e) in st.entries.iter_mut() {
        if e.conn.is_none() {
            continue;
        }
        let srv_changed = cat.servers.get(&e.server).is_none_or(|s| format!("{s:?}") != format!("{:?}", e.server_def));
        let map_changed = cat.user_mappings.get(key).is_none_or(|m| format!("{m:?}") != format!("{:?}", e.mapping_def));
        if srv_changed || map_changed {
            if e.xact_depth == 0 {
                e.conn = None;
            } else {
                e.invalidated = true;
            }
        }
    }
}

/// pgfdw_reject_incomplete_xact_state_change.
fn reject_incomplete(e: &Entry, cat: &Catalog) -> Result<()> {
    if e.conn.is_some() && e.changing_xact_state {
        let name = cat.servers.get(&e.server).map_or(String::new(), |s| s.name.clone());
        return Err(Error::new(
            code::CONNECTION_EXCEPTION,
            format!("connection to server \"{name}\" cannot be used due to abort cleanup failure"),
        ));
    }
    Ok(())
}

/// The local transaction's nesting level (GetCurrentTransactionNestLevel).
pub fn nest_level(ctx: &ExecCtx) -> usize {
    ctx.txn.try_borrow().map_or(1, |t| t.savepoint_depth() + 1)
}

/// Whether the connection has neither closed nor sent anything unasked (a terminated
/// backend's FATAL): what is there to read is read without waiting.
fn alive(conn: &mut Conn) -> bool {
    !conn.bad && (conn.is_busy() || conn.consume_input()) && !conn.bad
}

/// begin_remote_xact: a remote transaction (REPEATABLE READ, or SERIALIZABLE when the local
/// one is), and savepoints up to the local nesting level.
fn begin_remote_xact(e: &mut Entry, ctx: &ExecCtx) -> Result<()> {
    let cur = nest_level(ctx);
    let conn = e.conn.as_mut().expect("connection");
    if e.xact_depth == 0 {
        let serializable = ctx
            .txn
            .try_borrow()
            .is_ok_and(|t| t.isolation == crate::txn::Isolation::Serializable);
        let sql = if serializable {
            "START TRANSACTION ISOLATION LEVEL SERIALIZABLE"
        } else {
            "START TRANSACTION ISOLATION LEVEL REPEATABLE READ"
        };
        // (Sent with the first command that needs it, a round trip fewer, when the
        // connection still looks alive: a broken one fails here, so that GetConnection
        // connects again.)
        if alive(conn) {
            conn.defer(sql.to_string());
        } else {
            e.changing_xact_state = true;
            command(conn, sql, ctx)?;
            e.changing_xact_state = false;
        }
        e.xact_depth = 1;
    }
    while e.xact_depth < cur {
        let sql = format!("SAVEPOINT s{}", e.xact_depth + 1);
        if alive(conn) {
            conn.defer(sql);
        } else {
            e.changing_xact_state = true;
            command(conn, &sql, ctx)?;
            e.changing_xact_state = false;
        }
        e.xact_depth += 1;
    }
    Ok(())
}

/// GetConnection: the cached connection of mapping `m` (made if needed), in a remote
/// transaction at the local nesting level. Returns its key (the user mapping's oid).
pub fn get_connection(ctx: &ExecCtx, m: &Mapping, will_prep_stmt: bool) -> Result<u32> {
    let key = m.mapping.oid;
    let cat = ctx.catalog();
    let mut r = ctx.sess.remote.borrow_mut();
    let st = &mut r.fdw;
    st.xact_got_connection = true;
    sweep(st, &cat);
    let e = st.entries.entry(key).or_insert_with(|| Entry {
        conn: None,
        server: m.server.oid,
        server_def: m.server.clone(),
        mapping_def: m.mapping.clone(),
        xact_depth: 0,
        have_prep_stmt: false,
        have_error: false,
        changing_xact_state: false,
        invalidated: false,
        keep_connections: true,
    });
    reject_incomplete(e, &cat)?;
    if e.conn.is_some() && e.invalidated && e.xact_depth == 0 {
        e.conn = None;
    }
    if e.conn.is_none() {
        *e = Entry {
            conn: Some(connect(ctx, m)?),
            server: m.server.oid,
            server_def: m.server.clone(),
            mapping_def: m.mapping.clone(),
            xact_depth: 0,
            have_prep_stmt: false,
            have_error: false,
            changing_xact_state: false,
            invalidated: false,
            keep_connections: server_bool(&m.server, "keep_connections", true),
        };
    }
    if let Err(err) = begin_remote_xact(e, ctx) {
        // A connection that broke while idle is made again, once.
        let broken = e.conn.as_ref().is_some_and(|c| c.bad);
        if err.code != code::CONNECTION_FAILURE || !broken || e.xact_depth > 0 {
            return Err(err);
        }
        e.changing_xact_state = false;
        e.conn = Some(connect(ctx, m)?);
        begin_remote_xact(e, ctx)?;
    }
    e.have_prep_stmt |= will_prep_stmt;
    Ok(key)
}

/// Run `f` on the cached connection `key`.
pub fn with_conn<T>(ctx: &ExecCtx, key: u32, f: impl FnOnce(&mut Conn) -> Result<T>) -> Result<T> {
    with_conn_raw(ctx, key, |c| {
        // (process_pending_request: a prefetched FETCH not read yet is read now, its rows
        // kept for its scan, before the next command.)
        let mut last = None;
        while c.is_busy() {
            match c.get_result(&|| Ok(()))? {
                Some(r) => last = Some(r),
                None => break,
            }
        }
        if let Some(r) = last {
            PENDING_RESULT.with_borrow_mut(|m| m.insert(key, r));
        }
        f(c)
    })
}

thread_local! {
    /// Results of prefetched FETCHes read for another command (`with_conn`), by connection.
    static PENDING_RESULT: std::cell::RefCell<std::collections::HashMap<u32, crate::remote::conn::PgResult>> = Default::default();
}

/// The prefetched FETCH's result another command read first, if one did.
pub fn take_pending_result(key: u32) -> Option<crate::remote::conn::PgResult> {
    PENDING_RESULT.with_borrow_mut(|m| m.remove(&key))
}

/// `with_conn`, the connection as it is (a query sent and not read yet: `Cursor::prefetch`).
pub fn with_conn_raw<T>(ctx: &ExecCtx, key: u32, f: impl FnOnce(&mut Conn) -> Result<T>) -> Result<T> {
    let mut r = ctx.sess.remote.borrow_mut();
    match r.fdw.entries.get_mut(&key).and_then(|e| e.conn.as_mut()) {
        Some(c) => f(c),
        None => Err(Error::new(code::CONNECTION_FAILURE, "connection to server was lost")),
    }
}

/// GetCursorNumber / GetPrepStmtNumber.
pub fn next_cursor(ctx: &ExecCtx) -> u32 {
    let mut r = ctx.sess.remote.borrow_mut();
    r.fdw.cursor_number += 1;
    r.fdw.cursor_number
}

pub fn next_prep_stmt(ctx: &ExecCtx) -> u32 {
    let mut r = ctx.sess.remote.borrow_mut();
    r.fdw.prep_stmt_number += 1;
    r.fdw.prep_stmt_number
}

/// Mark that a remote statement failed on `key` (have_error: prepared statements are
/// deallocated when the transaction ends).
pub fn note_error(ctx: &ExecCtx, key: u32) {
    if let Some(e) = ctx.sess.remote.borrow_mut().fdw.entries.get_mut(&key) {
        e.have_error = true;
    }
}

fn no_interrupts() -> Result<()> {
    Ok(())
}

/// A transaction-control command at transaction end (no interrupts are taken then).
fn end_command(conn: &mut Conn, sql: &str) -> Result<()> {
    let res = conn.exec(sql, &no_interrupts)?;
    if res.status != Status::CommandOk {
        return Err(report(&res, conn, Some(sql)));
    }
    Ok(())
}

/// pgfdw_reset_xact_state: after the top-level transaction a connection not idle (or
/// invalidated, or of a server with keep_connections off) is closed.
fn reset_xact_state(e: &mut Entry, toplevel: bool) {
    if toplevel {
        e.xact_depth = 0;
        let idle = e.conn.as_ref().is_some_and(|c| !c.bad && c.tx_status() == TxStatus::Idle);
        if !idle || e.changing_xact_state || e.invalidated || !e.keep_connections {
            e.conn = None;
        }
    } else {
        e.xact_depth = e.xact_depth.saturating_sub(1);
    }
}

/// pgfdw_abort_cleanup: the query running is cancelled, the remote (sub)transaction rolled
/// back. A failure leaves `changing_xact_state` set (the connection is then unusable).
fn abort_cleanup(e: &mut Entry, toplevel: bool, level: usize) {
    if e.changing_xact_state {
        return;
    }
    e.changing_xact_state = true;
    e.have_error = true;
    let Some(conn) = e.conn.as_mut() else { return };
    if conn.tx_status() == TxStatus::Active {
        conn.cancel_and_drain();
        if conn.tx_status() == TxStatus::Active || conn.bad {
            return;
        }
    }
    let sql = if toplevel {
        "ABORT TRANSACTION".to_string()
    } else {
        format!("ROLLBACK TO SAVEPOINT s{level}; RELEASE SAVEPOINT s{level}")
    };
    if conn.exec(&sql, &no_interrupts).map(|r| r.status != Status::CommandOk).unwrap_or(true) {
        return;
    }
    if toplevel {
        if e.have_prep_stmt && e.have_error && conn.exec("DEALLOCATE ALL", &no_interrupts).map(|r| !r.ok()).unwrap_or(true) {
            return;
        }
        e.have_prep_stmt = false;
        e.have_error = false;
    }
    e.changing_xact_state = false;
}

/// XACT_EVENT_PRE_COMMIT: every remote transaction commits (a failure fails the local
/// commit).
pub fn pre_commit(sess: &crate::session::state::SessionState) -> Result<()> {
    let mut r = sess.remote.borrow_mut();
    let st = &mut r.fdw;
    if !st.xact_got_connection {
        return Ok(());
    }
    let cat = sess.db.catalog();
    for e in st.entries.values_mut() {
        if e.conn.is_none() {
            continue;
        }
        if e.xact_depth > 0 {
            reject_incomplete(e, &cat)?;
            e.changing_xact_state = true;
            end_command(e.conn.as_mut().unwrap(), "COMMIT TRANSACTION")?;
            e.changing_xact_state = false;
            if e.have_prep_stmt && e.have_error {
                let _ = e.conn.as_mut().unwrap().exec("DEALLOCATE ALL", &no_interrupts);
            }
            e.have_prep_stmt = false;
            e.have_error = false;
        }
        reset_xact_state(e, true);
    }
    st.xact_got_connection = false;
    st.cursor_number = 0;
    Ok(())
}

/// XACT_EVENT_PRE_PREPARE.
pub fn pre_prepare(sess: &crate::session::state::SessionState) -> Result<()> {
    let r = sess.remote.borrow();
    if r.fdw.xact_got_connection && r.fdw.entries.values().any(|e| e.conn.is_some() && e.xact_depth > 0) {
        return Err(Error::new(
            code::FEATURE_NOT_SUPPORTED,
            "cannot PREPARE a transaction that has operated on postgres_fdw foreign tables",
        ));
    }
    Ok(())
}

/// XACT_EVENT_ABORT: every remote transaction rolls back.
pub fn abort(sess: &crate::session::state::SessionState) {
    super::modify::discard_pending();
    let mut r = sess.remote.borrow_mut();
    let st = &mut r.fdw;
    if !st.xact_got_connection {
        return;
    }
    for e in st.entries.values_mut() {
        if e.conn.is_none() {
            continue;
        }
        if e.xact_depth > 0 {
            abort_cleanup(e, true, 0);
        }
        reset_xact_state(e, true);
    }
    st.xact_got_connection = false;
    st.cursor_number = 0;
}

/// pgfdw_subxact_callback for the subtransaction at `level` ending (commit: RELEASE, else
/// ROLLBACK TO and RELEASE of its remote savepoint).
pub fn subxact_end(sess: &crate::session::state::SessionState, level: usize, commit: bool) -> Result<()> {
    let mut r = sess.remote.borrow_mut();
    let st = &mut r.fdw;
    if !st.xact_got_connection {
        return Ok(());
    }
    let cat = sess.db.catalog();
    for e in st.entries.values_mut() {
        if e.conn.is_none() || e.xact_depth < level {
            continue;
        }
        if e.xact_depth > level {
            return Err(crate::error::internal(format!(
                "missed cleaning up remote subtransaction at level {}",
                e.xact_depth
            )));
        }
        if commit {
            reject_incomplete(e, &cat)?;
            e.changing_xact_state = true;
            end_command(e.conn.as_mut().unwrap(), &format!("RELEASE SAVEPOINT s{level}"))?;
            e.changing_xact_state = false;
        } else {
            abort_cleanup(e, false, level);
        }
        reset_xact_state(e, false);
    }
    Ok(())
}

/// Subtransactions `from` down to `to` (nesting levels, innermost first) end.
pub fn subxacts_end(sess: &crate::session::state::SessionState, from: usize, to: usize, commit: bool) -> Result<()> {
    let mut level = from;
    while level >= to && level > 1 {
        subxact_end(sess, level, commit)?;
        level -= 1;
    }
    Ok(())
}
