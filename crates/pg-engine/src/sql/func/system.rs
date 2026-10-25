//! System information and administration functions: version(), current_setting(), sequences,
//! advisory locks, pg_typeof, format_type, pg_get_* and size functions.

use super::{FnCtx, Registry, Ret, int_arg, text, text_arg};
use crate::catalog::names::parse_qualified;
use crate::catalog::tsearch::TsKind;
use crate::catalog::{Catalog, ConstraintKind, IndexKey, Relation, quote_ident};
use crate::error::{Error, Result, code};
use crate::exec::ExecCtx;
use crate::sql::binder::ast::parse_expr;
use crate::sql::ruleutils::{Stored, bind_stored, column_names, deparse, stored_expr};
use crate::txn::locks::{LockMode, WaitPolicy};
use crate::types::array::ArrayValue;
use crate::types::{DataType as T, Value};
use crate::{bail, err};
use std::sync::{Arc, OnceLock};

pub const VERSION: &str =
    "PostgreSQL 18.6 (roll) on x86_64-pc-linux-gnu, compiled by rustc, 64-bit";

fn version(_: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(text(VERSION))
}
fn current_database(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Text(c.exec.sess.database.as_str().into()))
}
fn current_user(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Text(c.exec.role_name(c.exec.current_user()).into()))
}
fn session_user(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Text(c.exec.role_name(c.exec.session_user()).into()))
}
fn current_schema(_: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    // The temporary schema counts where the path names `pg_temp` explicitly.
    let raw = c.exec.sess.settings.borrow().get_raw("search_path").unwrap_or_default();
    let explicit_temp = crate::session::settings::parse_search_path(&raw, "")
        .iter()
        .any(|s| s == "pg_temp");
    for s in &c.exec.search_path {
        if (explicit_temp || !s.starts_with("pg_temp")) && cat.schema_by_name(s).is_some() {
            return Ok(text(s.as_str()));
        }
        // A pending temporary schema named first is made by asking (AccessTempTableNamespace):
        // the transaction has used temporary objects.
        if explicit_temp
            && s.starts_with("pg_temp_")
            && c.exec.search_path.first() == Some(s)
        {
            c.exec.sess.temp_used.set(true);
            return Ok(text(s.as_str()));
        }
    }
    Ok(Value::Null)
}
fn current_schemas(a: &[Value], c: &FnCtx) -> Result<Value> {
    let implicit = a[0].as_bool().unwrap_or(false);
    let cat = c.exec.catalog();
    let mut out = Vec::new();
    // The session's temp schema and pg_catalog are searched first unless search_path names
    // them (fetch_search_path: the implicit ones only when asked for).
    let guc = c.exec.sess.settings.borrow().show("search_path").unwrap_or_default();
    let explicit_temp = guc
        .split(',')
        .any(|p| p.trim().trim_matches('"') == "pg_temp");
    let is_temp = |s: &str| s.starts_with("pg_temp_");
    if implicit {
        if !explicit_temp
            && let Some(t) = c
                .exec
                .search_path
                .iter()
                .find(|s| is_temp(s) && cat.schema_by_name(s).is_some())
        {
            out.push(text(t.as_str()));
        }
        if !c.exec.search_path.iter().any(|s| s == "pg_catalog") {
            out.push(text("pg_catalog"));
        }
    }
    for s in &c.exec.search_path {
        if is_temp(s) && !explicit_temp {
            continue;
        }
        if cat.schema_by_name(s).is_some() {
            out.push(text(s.as_str()));
        }
    }
    Ok(Value::Array(Arc::new(ArrayValue::from_vec(T::Name, out))))
}
fn current_setting(a: &[Value], c: &FnCtx) -> Result<Value> {
    let missing_ok = a.get(1).and_then(|v| v.as_bool()).unwrap_or(false);
    match c.exec.sess.settings.borrow().show(text_arg(&a[0])) {
        Ok(v) => Ok(text(v)),
        Err(_) if missing_ok => Ok(Value::Null),
        Err(e) => Err(e),
    }
}
fn set_config(a: &[Value], c: &FnCtx) -> Result<Value> {
    if a[0].is_null() {
        bail!(INVALID_PARAMETER_VALUE, "SET requires parameter name");
    }
    let name = text_arg(&a[0]);
    let local = a[2].as_bool().unwrap_or(false);
    let mut value = if a[1].is_null() {
        None
    } else {
        Some(text_arg(&a[1]).to_string())
    };
    if let Some(v) = &value
        && name.eq_ignore_ascii_case("default_text_search_config")
    {
        value = Some(crate::catalog::tsearch::check_ts_config_setting(
            &c.exec.catalog(),
            v,
            &c.exec.search_path,
        )?);
    }
    let mut s = c.exec.sess.settings.borrow_mut();
    s.set(name, value.as_deref(), local, true)?;
    Ok(text(s.show(name)?))
}
fn pg_settings_get_flags(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(
        match crate::session::settings::flags(text_arg(&a[0])) {
            Some(f) => Value::Array(Arc::new(ArrayValue::from_vec(
                T::Text,
                f.iter().map(|x| text(*x)).collect(),
            ))),
            None => Value::Null,
        },
    )
}
fn pg_backend_pid(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Int4(c.exec.sess.pid))
}
fn inet_server_addr(_: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Null)
}
fn inet_server_port(_: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Null)
}
fn pg_postmaster_start_time(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Timestamptz(c.exec.sess.cluster.start_time()))
}
fn pg_conf_load_time(a: &[Value], c: &FnCtx) -> Result<Value> {
    pg_postmaster_start_time(a, c)
}
fn pg_is_in_recovery(_: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(false))
}
fn pg_sleep(a: &[Value], c: &FnCtx) -> Result<Value> {
    crate::exec::flush_live_notices();
    let secs = a[0].as_f64().unwrap_or(0.0);
    let end = std::time::Instant::now()
        + std::time::Duration::from_secs_f64(if secs.is_nan() {
            0.0
        } else {
            secs.clamp(0.0, 1e7)
        });
    loop {
        let now = std::time::Instant::now();
        if now >= end {
            break;
        }
        c.exec.txn.borrow().check_cancel()?;
        if let Some(d) = c.exec.deadline
            && now > d
        {
            return Err(Error::new(
                code::QUERY_CANCELED,
                "canceling statement due to statement timeout",
            ));
        }
        std::thread::sleep((end - now).min(std::time::Duration::from_millis(20)));
    }
    Ok(Value::Null)
}
/// pg_sleep_for(interval): pg_sleep of the interval's seconds (its epoch).
fn pg_sleep_for(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Value::Interval(i) = &a[0] else {
        return Ok(Value::Null);
    };
    let secs = i.micros as f64 / 1e6
        + i.days as f64 * 86400.0
        + (i.months as f64 / 12.0) * 365.25 * 86400.0;
    pg_sleep(&[Value::Float8(secs)], c)
}
/// pg_sleep_until(timestamptz): pg_sleep until then.
fn pg_sleep_until(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Value::Timestamptz(t) = &a[0] else {
        return Ok(Value::Null);
    };
    let now = crate::types::datetime::Clock::system().now;
    pg_sleep(&[Value::Float8((t - now) as f64 / 1e6)], c)
}
/// Who may signal backend `pid` (pg_signal_backend): Ok(false) after a WARNING when it is no
/// backend; an error when the current role may not.
fn may_signal(pid: i32, c: &FnCtx, terminate: bool) -> Result<bool> {
    let cat = c.exec.catalog();
    let user = match c.exec.sess.cluster.activity().into_iter().find(|a| a.pid == pid) {
        Some(act) => cat.roles.values().find(|r| r.name == act.user).map(|r| r.oid),
        // (This session, when the cluster doesn't list it.)
        None if pid == c.exec.sess.pid => Some(c.exec.session_user()),
        // A launcher: as a superuser's backend.
        None if super::monitor::is_launcher_process(pid as i64) => None,
        None => {
            c.exec.notice(crate::session::warning(
                code::WARNING,
                format!("PID {pid} is not a PostgreSQL backend process"),
            ));
            return Ok(false);
        }
    };
    let me = c.exec.current_user();
    let owner = c.exec.db_owner();
    let target = user;
    let (verb, what) = if terminate {
        ("terminate process", "terminate processes")
    } else {
        ("cancel query", "cancel queries")
    };
    // A backend of a superuser (or of no role) takes a superuser.
    if target.is_none_or(|t| cat.is_superuser(t)) && !cat.is_superuser(me) {
        return Err(Error::new(
            code::INSUFFICIENT_PRIVILEGE,
            format!("permission denied to {verb}"),
        )
        .detail(format!(
            "Only roles with the SUPERUSER attribute may {what} of roles with the SUPERUSER attribute."
        )));
    }
    let target = target.unwrap_or(0);
    if !cat.has_privs_of_role(me, target, owner) && !cat.has_privs_of_role(me, 4200, owner) {
        let detail = if terminate {
            "Only roles with privileges of the role whose process is being terminated or with privileges of the \"pg_signal_backend\" role may terminate this process."
        } else {
            "Only roles with privileges of the role whose query is being canceled or with privileges of the \"pg_signal_backend\" role may cancel this query."
        };
        return Err(Error::new(
            code::INSUFFICIENT_PRIVILEGE,
            format!("permission denied to {verb}"),
        )
        .detail(detail));
    }
    Ok(true)
}
/// pg_blocking_pids(pid): the backends holding the heavyweight locks `pid` waits for.
fn pg_blocking_pids(a: &[Value], c: &FnCtx) -> Result<Value> {
    let pid = int_arg(&a[0]) as i32;
    let pids = c.exec.sess.db.locks.blocking_pids(pid);
    Ok(Value::Array(Arc::new(ArrayValue::from_vec(
        T::Int4,
        pids.into_iter().map(Value::Int4).collect(),
    ))))
}
/// pg_safe_snapshot_blocking_pids(pid): the backends whose serializable transactions a
/// SERIALIZABLE READ ONLY DEFERRABLE transaction's snapshot waits for.
fn pg_safe_snapshot_blocking_pids(a: &[Value], c: &FnCtx) -> Result<Value> {
    let pid = int_arg(&a[0]) as i32;
    let pids = c.exec.sess.db.ssi.safe_snapshot_blockers(pid);
    Ok(Value::Array(Arc::new(ArrayValue::from_vec(
        T::Int4,
        pids.into_iter().map(Value::Int4).collect(),
    ))))
}
/// pg_isolation_test_session_is_blocked(pid, interesting pids): whether `pid` waits for a
/// lock one of those backends holds.
fn pg_isolation_test_session_is_blocked(a: &[Value], c: &FnCtx) -> Result<Value> {
    let pid = int_arg(&a[0]) as i32;
    let Value::Array(arr) = &a[1] else {
        return Ok(Value::Bool(false));
    };
    if arr.items.iter().any(Value::is_null) {
        return Err(Error::new(code::INTERNAL_ERROR, "array must not contain nulls"));
    }
    let interesting: Vec<i32> = arr.items.iter().map(|v| int_arg(v) as i32).collect();
    let blocked = c
        .exec
        .sess
        .db
        .locks
        .blocking_pids(pid)
        .iter()
        .any(|p| interesting.contains(p));
    // (Or waiting for a safe snapshot, whoever it waits for.)
    Ok(Value::Bool(blocked || !c.exec.sess.db.ssi.safe_snapshot_blockers(pid).is_empty()))
}
fn pg_cancel_backend(a: &[Value], c: &FnCtx) -> Result<Value> {
    let pid = int_arg(&a[0]) as i32;
    if !may_signal(pid, c, false)? {
        return Ok(Value::Bool(false));
    }
    Ok(Value::Bool(c.exec.sess.cluster.cancel_backend(pid)))
}
fn pg_terminate_backend(a: &[Value], c: &FnCtx) -> Result<Value> {
    let pid = int_arg(&a[0]) as i32;
    if a.get(1).is_some_and(|t| int_arg(t) < 0) {
        bail!(NUMERIC_VALUE_OUT_OF_RANGE, "\"timeout\" must not be negative");
    }
    if !may_signal(pid, c, true)? {
        return Ok(Value::Bool(false));
    }
    // This session: it ends here, the statement with it.
    if pid == c.exec.sess.pid {
        return Err(Error::new(
            code::ADMIN_SHUTDOWN,
            "terminating connection due to administrator command",
        )
        .fatal());
    }
    Ok(Value::Bool(c.exec.sess.cluster.terminate_backend(pid)))
}

// ── sequences ──

fn seq_by_oid(cat: &Catalog, oid: u32) -> Result<Arc<crate::catalog::Sequence>> {
    if let Some(s) = cat.sequences.get(&oid) {
        return Ok(s.clone());
    }
    let what = cat
        .tables
        .get(&oid)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| oid.to_string());
    Err(Error::new(
        code::WRONG_OBJECT_TYPE,
        format!("\"{what}\" is not a sequence"),
    ))
}

fn seq_arg(v: &Value, c: &FnCtx) -> Result<Arc<crate::catalog::Sequence>> {
    let cat = c.exec.catalog();
    let oid = match v {
        Value::Oid(o) => *o,
        Value::Text(t) => {
            let parts = parse_qualified(t)?;
            let (schema, name) = match parts.as_slice() {
                [n] => (None, n.as_str()),
                [s, n] => (Some(s.as_str()), n.as_str()),
                _ => bail!(
                    INVALID_NAME,
                    "improper relation name (too many dotted names): {t}"
                ),
            };
            match cat.find(schema, name, &c.exec.search_path) {
                Some(r) => r.oid(),
                None => return Err(crate::catalog::no_such_relation(schema, name)),
            }
        }
        v => v.as_i64().unwrap_or(0) as u32,
    };
    seq_by_oid(&cat, oid)
}

/// nextval's lock on the sequence (lock_and_open_sequence: ROW EXCLUSIVE), and the
/// sequence as it is once that's held (an ALTER SEQUENCE we waited for changed it).
fn lock_sequence(
    seq: Arc<crate::catalog::Sequence>,
    c: &FnCtx,
) -> Result<Arc<crate::catalog::Sequence>> {
    let name = seq.name.clone();
    let mut txn = c.exec.txn.borrow_mut();
    txn.lock_relation(
        seq.oid,
        LockMode::RowExclusive,
        WaitPolicy::Block,
        &|| name.clone(),
    )?;
    let cat = txn.catalog();
    drop(txn);
    if cat.version != c.exec.catalog().version
        && let Ok(fresh) = seq_by_oid(&cat, seq.oid)
    {
        return Ok(fresh);
    }
    Ok(seq)
}

/// A sequence privilege check: any of `privs` will do.
fn seq_priv(seq: &crate::catalog::Sequence, c: &FnCtx, privs: u32) -> Result<()> {
    let cat = c.exec.catalog();
    let (held, _) = cat
        .rel_mask(seq.oid, c.exec.current_user(), c.exec.db_owner())
        .unwrap_or((0, 0));
    if held & privs == 0 {
        bail!(
            INSUFFICIENT_PRIVILEGE,
            "permission denied for sequence {}",
            seq.name
        );
    }
    Ok(())
}

/// pg_sequence_parameters: a sequence's options (pg_sequence).
fn pg_sequence_parameters(a: &[Value], c: &FnCtx) -> Result<super::SrfIter> {
    use crate::catalog::acl;
    let seq = seq_arg(&a[0], c)?;
    seq_priv(&seq, c, acl::SELECT | acl::UPDATE | acl::USAGE)?;
    let row = vec![
        Value::Int8(seq.start),
        Value::Int8(seq.min),
        Value::Int8(seq.max),
        Value::Int8(seq.increment),
        Value::Bool(seq.cycle),
        Value::Int8(seq.cache),
        Value::Oid(seq.ty.oid()),
    ];
    Ok(Box::new(std::iter::once(Ok(row))))
}

/// pg_get_sequence_data: its last value and is_called (NULLs without SELECT on it).
fn pg_get_sequence_data(a: &[Value], c: &FnCtx) -> Result<super::SrfIter> {
    use crate::catalog::acl;
    let seq = seq_arg(&a[0], c)?;
    let row = if seq_priv(&seq, c, acl::SELECT).is_ok() {
        let (last, called) = c.exec.sess.db.seq_state(&seq)?;
        vec![Value::Int8(last), Value::Bool(called)]
    } else {
        vec![Value::Null, Value::Null]
    };
    Ok(Box::new(std::iter::once(Ok(row))))
}

/// The sequence's next value for this session: from the values it cached (CACHE n),
/// else from the sequence.
pub(crate) fn next_value(seq: &crate::catalog::Sequence, c: &FnCtx) -> Result<i64> {
    next_value_in(seq, c.exec.sess)
}

pub(crate) fn next_value_in(
    seq: &crate::catalog::Sequence,
    sess: &crate::session::state::SessionState,
) -> Result<i64> {
    if seq.cache <= 1 {
        return sess.db.nextval(seq);
    }
    if let Some(v) = sess
        .seq_cache
        .borrow_mut()
        .get_mut(&seq.oid)
        .and_then(|q| q.pop_front())
    {
        return Ok(v);
    }
    let mut vals: std::collections::VecDeque<i64> = sess.db.nextval_batch(seq, seq.cache)?.into();
    let v = vals.pop_front().unwrap_or_default();
    sess.seq_cache.borrow_mut().insert(seq.oid, vals);
    Ok(v)
}

/// A temporary sequence: a read-only transaction may still advance it.
fn temp_sequence(seq: &crate::catalog::Sequence, c: &FnCtx) -> bool {
    c.exec.catalog().schema_name(seq.schema).starts_with("pg_temp")
}

fn nextval(a: &[Value], c: &FnCtx) -> Result<Value> {
    use crate::catalog::acl;
    let seq = seq_arg(&a[0], c)?;
    seq_priv(&seq, c, acl::USAGE | acl::UPDATE)?;
    if c.exec.txn.borrow().read_only && !temp_sequence(&seq, c) {
        bail!(
            READ_ONLY_SQL_TRANSACTION,
            "cannot execute nextval() in a read-only transaction"
        );
    }
    let seq = lock_sequence(seq, c)?;
    let v = next_value(&seq, c)?;
    c.exec.sess.currval.borrow_mut().insert(seq.oid, v);
    c.exec.sess.lastval.set(Some(v));
    c.exec.sess.lastval_seq.set(seq.oid);
    Ok(Value::Int8(v))
}
/// An identity column's next value (NextValueExpr): no privilege on the sequence is
/// needed.
fn identity_nextval(a: &[Value], c: &FnCtx) -> Result<Value> {
    let seq = seq_arg(&a[0], c)?;
    if c.exec.txn.borrow().read_only {
        bail!(
            READ_ONLY_SQL_TRANSACTION,
            "cannot execute nextval() in a read-only transaction"
        );
    }
    let seq = lock_sequence(seq, c)?;
    let v = next_value(&seq, c)?;
    c.exec.sess.currval.borrow_mut().insert(seq.oid, v);
    c.exec.sess.lastval.set(Some(v));
    c.exec.sess.lastval_seq.set(seq.oid);
    Ok(Value::Int8(v))
}

/// `nextval(...)` as an identity column's default calls it (see `identity_nextval`).
pub(crate) fn as_identity_nextval(def: &super::FuncDef) -> super::FuncDef {
    super::FuncDef {
        imp: identity_nextval,
        ..def.clone()
    }
}

fn currval(a: &[Value], c: &FnCtx) -> Result<Value> {
    use crate::catalog::acl;
    let seq = seq_arg(&a[0], c)?;
    seq_priv(&seq, c, acl::USAGE | acl::SELECT)?;
    match c.exec.sess.currval.borrow().get(&seq.oid) {
        Some(v) => Ok(Value::Int8(*v)),
        None => Err(Error::new(
            code::OBJECT_NOT_IN_PREREQUISITE_STATE,
            format!(
                "currval of sequence \"{}\" is not yet defined in this session",
                seq.name
            ),
        )),
    }
}
fn lastval(_: &[Value], c: &FnCtx) -> Result<Value> {
    // The sequence may have been dropped since; reading it takes SELECT or USAGE.
    let oid = c.exec.sess.lastval_seq.get();
    if oid != 0 && c.exec.sess.lastval.get().is_some() {
        let seq = c.exec.catalog().sequences.get(&oid).cloned();
        match seq {
            None => {
                return Err(Error::new(
                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "lastval is not yet defined in this session",
                ));
            }
            Some(seq) => {
                use crate::catalog::acl;
                seq_priv(&seq, c, acl::SELECT | acl::USAGE)?;
            }
        }
    }
    match c.exec.sess.lastval.get() {
        Some(v) => Ok(Value::Int8(v)),
        None => Err(Error::new(
            code::OBJECT_NOT_IN_PREREQUISITE_STATE,
            "lastval is not yet defined in this session",
        )),
    }
}
fn setval(a: &[Value], c: &FnCtx) -> Result<Value> {
    let seq = seq_arg(&a[0], c)?;
    seq_priv(&seq, c, crate::catalog::acl::UPDATE)?;
    if c.exec.txn.borrow().read_only && !temp_sequence(&seq, c) {
        bail!(
            READ_ONLY_SQL_TRANSACTION,
            "cannot execute setval() in a read-only transaction"
        );
    }
    let v = int_arg(&a[1]);
    let called = a.get(2).and_then(|x| x.as_bool()).unwrap_or(true);
    c.exec.sess.db.setval(&seq, v, called)?;
    c.exec.sess.seq_cache.borrow_mut().remove(&seq.oid);
    if called {
        c.exec.sess.currval.borrow_mut().insert(seq.oid, v);
        // (lastval() reports the sequence nextval() last used.)
        if c.exec.sess.lastval_seq.get() == seq.oid {
            c.exec.sess.lastval.set(Some(v));
        }
    }
    Ok(Value::Int8(v))
}
fn pg_get_serial_sequence(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    let parts = parse_qualified(text_arg(&a[0]))?;
    let (schema, name) = match parts.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => bail!(INVALID_NAME, "improper relation name"),
    };
    let t = match cat.find(schema, name, &c.exec.search_path) {
        Some(Relation::Table(t)) => t,
        // A view's or materialized view's columns own no sequence (get_attnum, then NULL).
        Some(Relation::View(v)) => {
            let col = text_arg(&a[1]);
            if !v.columns.iter().any(|(n, _)| n == col) {
                bail!(
                    UNDEFINED_COLUMN,
                    "column \"{col}\" of relation \"{}\" does not exist",
                    v.name
                );
            }
            return Ok(Value::Null);
        }
        _ => return Err(crate::catalog::no_such_relation(schema, name)),
    };
    let col = text_arg(&a[1]);
    let (idx, column) = t.column(col)?;
    // The first sequence pg_depend has the column owning (auto or internal): the oldest.
    let owned = cat
        .sequences
        .values()
        .filter(|s| s.owned_by == Some((t.oid, idx)))
        .min_by_key(|s| s.oid)
        .map(|s| s.oid);
    Ok(match owned.or(column.seq).and_then(|s| cat.sequences.get(&s)) {
        Some(s) => text(format!(
            "{}.{}",
            quote_ident(cat.schema_name(s.schema)),
            quote_ident(&s.name)
        )),
        None => Value::Null,
    })
}

// ── advisory locks ──

fn advisory_key(a: &[Value]) -> Vec<u8> {
    let mut k = vec![0, 0, 0, 0, 0, 0, 0, 3];
    if a.len() == 2 {
        k.push(2);
        k.extend_from_slice(&(int_arg(&a[0]) as i32).to_be_bytes());
        k.extend_from_slice(&(int_arg(&a[1]) as i32).to_be_bytes());
    } else {
        k.push(1);
        k.extend_from_slice(&int_arg(&a[0]).to_be_bytes());
    }
    k
}

fn advisory_what(a: &[Value]) -> String {
    format!(
        "advisory lock [{}]",
        a.iter()
            .map(|v| int_arg(v).to_string())
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn session_lock(a: &[Value], c: &FnCtx, shared: bool, policy: WaitPolicy) -> Result<bool> {
    let key = advisory_key(a);
    let mode = if shared {
        LockMode::Share
    } else {
        LockMode::Exclusive
    };
    let what = advisory_what(a);
    let owner = c.exec.sess.lock_owner;
    let got = {
        let t = c.exec.txn.borrow();
        c.exec.sess.db.locks.acquire(&key, owner, owner, mode, &t.waits(policy), &|| what.clone())?
    };
    if got {
        let mut k = key;
        k.push(shared as u8);
        *c.exec.sess.advisory.borrow_mut().entry(k).or_insert(0) += 1;
    }
    Ok(got)
}

fn xact_lock(a: &[Value], c: &FnCtx, shared: bool, policy: WaitPolicy) -> Result<bool> {
    let key = advisory_key(a);
    let mode = if shared {
        LockMode::Share
    } else {
        LockMode::Exclusive
    };
    let what = advisory_what(a);
    c.exec
        .txn
        .borrow_mut()
        .lock(&key, mode, policy, &|| what.clone())
}

fn pg_advisory_lock(a: &[Value], c: &FnCtx) -> Result<Value> {
    session_lock(a, c, false, WaitPolicy::Block)?;
    Ok(Value::Null)
}
fn pg_advisory_lock_shared(a: &[Value], c: &FnCtx) -> Result<Value> {
    session_lock(a, c, true, WaitPolicy::Block)?;
    Ok(Value::Null)
}
fn pg_try_advisory_lock(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(session_lock(
        a,
        c,
        false,
        WaitPolicy::SkipLocked,
    )?))
}
fn pg_try_advisory_lock_shared(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(session_lock(
        a,
        c,
        true,
        WaitPolicy::SkipLocked,
    )?))
}
fn pg_advisory_xact_lock(a: &[Value], c: &FnCtx) -> Result<Value> {
    xact_lock(a, c, false, WaitPolicy::Block)?;
    Ok(Value::Null)
}
fn pg_advisory_xact_lock_shared(a: &[Value], c: &FnCtx) -> Result<Value> {
    xact_lock(a, c, true, WaitPolicy::Block)?;
    Ok(Value::Null)
}
fn pg_try_advisory_xact_lock(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(xact_lock(a, c, false, WaitPolicy::SkipLocked)?))
}
fn pg_try_advisory_xact_lock_shared(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(xact_lock(a, c, true, WaitPolicy::SkipLocked)?))
}
fn pg_advisory_unlock(a: &[Value], c: &FnCtx) -> Result<Value> {
    advisory_unlock(a, c, false)
}
fn pg_advisory_unlock_shared(a: &[Value], c: &FnCtx) -> Result<Value> {
    advisory_unlock(a, c, true)
}
/// Release one hold of a session-level advisory lock in this mode.
fn advisory_unlock(a: &[Value], c: &FnCtx, shared: bool) -> Result<Value> {
    let key = advisory_key(a);
    let mut mk = key.clone();
    mk.push(shared as u8);
    let mut held = c.exec.sess.advisory.borrow_mut();
    match held.get_mut(&mk) {
        Some(n) => {
            *n -= 1;
            if *n == 0 {
                held.remove(&mk);
            }
            let mode = if shared { LockMode::Share } else { LockMode::Exclusive };
            c.exec.sess.db.locks.release(&key, c.exec.sess.lock_owner, mode);
            Ok(Value::Bool(true))
        }
        None => {
            c.exec.notice(Error {
                severity: crate::error::Severity::Warning,
                ..Error::new(
                    code::WARNING,
                    format!(
                        "you don't own a lock of type {}",
                        if shared { "ShareLock" } else { "ExclusiveLock" }
                    ),
                )
            });
            Ok(Value::Bool(false))
        }
    }
}

fn pg_advisory_unlock_all(_: &[Value], c: &FnCtx) -> Result<Value> {
    c.exec.sess.release_advisory();
    Ok(Value::Null)
}

// ── types and catalog helpers ──

fn pg_typeof(_: &[Value], c: &FnCtx) -> Result<Value> {
    if let Some(d) = c.args.first().and_then(expr_domain) {
        return Ok(Value::Oid(d));
    }
    Ok(Value::Oid(c.arg_type(0).oid()))
}

/// The domain an expression's value is of: a coercion to it, or a COALESCE / CASE /
/// GREATEST whose inputs all are of it (select_common_type keeps a domain they share).
fn expr_domain(e: &crate::sql::expr::Expr) -> Option<u32> {
    use crate::sql::expr::Expr;
    fn common<'e>(items: impl Iterator<Item = &'e Expr>) -> Option<u32> {
        let mut out = None;
        for x in items {
            let d = expr_domain(x)?;
            if out.is_some_and(|o| o != d) {
                return None;
            }
            out = Some(d);
        }
        out
    }
    match e {
        Expr::CoerceToDomain(_, d) => Some(d.oid),
        Expr::Coalesce(args, _) | Expr::MinMax(args, ..) => common(args.iter()),
        Expr::Case { whens, else_, .. } => {
            common(whens.iter().map(|(_, r)| r).chain(else_.as_deref()))
        }
        _ => None,
    }
}

/// The binder hands the argument over under the collation it derives (none: NULL).
fn pg_collation_for(_: &[Value], c: &FnCtx) -> Result<Value> {
    use crate::sql::expr::Expr;
    let (arg, coll) = match c.args.first() {
        Some(Expr::Collate(a, coll, _)) => (Some(&**a), Some(coll)),
        a => (a, None),
    };
    if !c.arg_type(0).collatable() && c.arg_type(0) != T::Unknown {
        let shown = match arg {
            Some(Expr::CoerceToDomain(_, d)) => {
                format_type_oid(d.oid, -1, &c.exec.catalog(), &c.exec.search_path)
            }
            _ => None,
        };
        bail!(
            DATATYPE_MISMATCH,
            "collations are not supported by type {}",
            shown.unwrap_or_else(|| c.arg_type(0).display())
        );
    }
    Ok(coll.map_or(Value::Null, |c| text(quote_ident(c))))
}

pub fn format_type_oid(
    oid: u32,
    typmod: i32,
    cat: &Catalog,
    search_path: &[String],
) -> Option<String> {
    if let Some((schema, name, array)) = cat.user_type_name(oid) {
        let n = cat.qualified_name(schema, name, search_path);
        return Some(if array { format!("{n}[]") } else { n });
    }
    let Some(t) = T::from_oid(oid) else {
        return crate::types::oid::other_pseudo_name(oid)
            .map(str::to_string)
            .or_else(|| crate::catalog::system::builtin_type_display(oid, search_path));
    };
    let t = if typmod >= 0 {
        crate::storage::row::with_typmod(t, typmod)
    } else {
        t
    };
    Some(t.display())
}

/// pg_basetype: a domain's base type (through domains over domains), any other type
/// itself; NULL for no type.
fn pg_basetype(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Some(oid) = a[0].as_i64().map(|o| o as u32) else {
        return Ok(Value::Null);
    };
    let cat = c.exec.catalog();
    if let Some(d) = cat.domains.get(&oid) {
        return Ok(Value::Oid(d.base.oid()));
    }
    Ok(match format_type_oid(oid, -1, &cat, &c.exec.search_path) {
        Some(_) => Value::Oid(oid),
        None => Value::Null,
    })
}

fn format_type(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Some(oid) = a[0].as_i64() else {
        return Ok(Value::Null);
    };
    // (format_type_extended: InvalidOid is "-"; an oid of no type "???".)
    if oid == 0 {
        return Ok(text("-"));
    }
    let typmod = a.get(1).and_then(|v| v.as_i64()).unwrap_or(-1) as i32;
    // An explicit typmod of -1 on bit / bpchar isn't BIT / CHARACTER (which mean length
    // 1): format_type shows the internal name so that it reads back the same.
    if typmod < 0 && a.get(1).is_some_and(|v| !v.is_null()) {
        match oid {
            1560 => return Ok(text("\"bit\"")),
            1561 => return Ok(text("\"bit\"[]")),
            1042 => return Ok(text("bpchar")),
            1014 => return Ok(text("bpchar[]")),
            _ => {}
        }
    }
    let cat = c.exec.catalog();
    Ok(text(
        format_type_oid(oid as u32, typmod, &cat, &c.exec.search_path)
            .unwrap_or_else(|| "???".into()),
    ))
}

fn to_regclass(a: &[Value], c: &FnCtx) -> Result<Value> {
    let names = c.exec.fmt().names.clone();
    Ok(match names.and_then(|n| n.rel_oid(text_arg(&a[0])).ok()) {
        Some(o) => Value::Oid(o),
        None => Value::Null,
    })
}
fn to_regtype(a: &[Value], c: &FnCtx) -> Result<Value> {
    let names = c.exec.fmt().names.clone();
    Ok(match names.and_then(|n| n.type_oid(text_arg(&a[0])).ok()) {
        Some(o) => Value::Oid(o),
        None => Value::Null,
    })
}
/// to_regtypemod: the typmod a type name string carries, or NULL if there's no such type.
fn to_regtypemod(a: &[Value], c: &FnCtx) -> Result<Value> {
    let tn = crate::sql::binder::parse_type_string(text_arg(&a[0]))?;
    let cat = c.exec.catalog();
    Ok(
        match crate::sql::binder::resolve_type_name(&cat, &c.exec.search_path, &tn) {
            Ok((ty, _)) => Value::Int4(ty.typmod()),
            Err(e) if e.code == code::UNDEFINED_OBJECT || e.code == code::INVALID_SCHEMA_NAME => {
                Value::Null
            }
            Err(e) => return Err(e),
        },
    )
}
fn to_regnamespace(a: &[Value], c: &FnCtx) -> Result<Value> {
    let names = c.exec.fmt().names.clone();
    Ok(match names.and_then(|n| n.nsp_oid(text_arg(&a[0])).ok()) {
        Some(o) => Value::Oid(o),
        None => Value::Null,
    })
}
/// to_regoper & co.: the type's input, or NULL where it would fail.
fn to_reg_input(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(
        crate::types::text::parse_value(text_arg(&a[0]), c.ret, c.exec.fmt())
            .unwrap_or(Value::Null),
    )
}

fn to_regrole(a: &[Value], c: &FnCtx) -> Result<Value> {
    let names = c.exec.fmt().names.clone();
    Ok(match names.and_then(|n| n.role_oid(text_arg(&a[0])).ok()) {
        Some(o) => Value::Oid(o),
        None => Value::Null,
    })
}

fn oid_arg(v: &Value) -> u32 {
    v.as_i64().unwrap_or(0) as u32
}

/// `pg_ts_*_is_visible`: NULL for an object that doesn't exist.
fn ts_visible(kind: TsKind, a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    let oid = oid_arg(&a[0]);
    if cat.ts_ident(kind, oid).is_none() {
        return Ok(Value::Null);
    }
    Ok(Value::Bool(cat.ts_visible(kind, oid, &c.exec.search_path)))
}

fn visible(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = oid_arg(&a[0]);
    let cat = c.exec.catalog();
    let path = cat.effective_path(&c.exec.search_path);
    let schema = cat
        .tables
        .get(&oid)
        .map(|t| (t.schema, t.name.clone()))
        .or_else(|| cat.views.get(&oid).map(|v| (v.schema, v.name.clone())))
        .or_else(|| cat.sequences.get(&oid).map(|s| (s.schema, s.name.clone())))
        .or_else(|| {
            cat.indexes
                .get(&oid)
                .and_then(|i| cat.index_table(i.table).map(|t| (t.schema, i.name.clone())))
        })
        .or_else(|| {
            cat.composites
                .values()
                .find(|ct| ct.relid == oid)
                .map(|ct| (ct.schema, ct.name.clone()))
        });
    if let Some(f) = cat.functions.get(&oid) {
        // Visible if no schema earlier in the path has a function with the same signature.
        let same = |g: &crate::catalog::Function| {
            g.name == f.name && g.args.iter().map(|a| &a.1).eq(f.args.iter().map(|a| &a.1))
        };
        let first = path
            .into_iter()
            .find(|p| cat.functions.values().any(|g| g.schema == *p && same(g)));
        return Ok(Value::Bool(first == Some(f.schema)));
    }
    let in_path = |s: u32, name: &str| {
        // RelationIsVisible: the first schema in the path with a relation of that name
        // (system relations and composite types share the namespace).
        let first = path.iter().copied().find(|p| {
            cat.relation_in(*p, name).is_some()
                || cat
                    .composites
                    .values()
                    .any(|ct| ct.schema == *p && ct.name == name)
                || Catalog::system_relation_in(*p, name)
        });
        first == Some(s)
    };
    Ok(match schema {
        Some((s, name)) => Value::Bool(in_path(s, &name)),
        // A system relation: by its schema too (information_schema's views are visible
        // only with it on the path).
        None => match crate::catalog::sys_rels::SYS_RELS.iter().find(|r| r.2 == oid) {
            Some(&(schema, name, ..)) => {
                let s = match schema {
                    "information_schema" => crate::catalog::INFORMATION_SCHEMA_OID,
                    _ => crate::catalog::PG_CATALOG_OID,
                };
                Value::Bool(in_path(s, name))
            }
            None => Value::Bool(crate::catalog::system::SysTable::by_oid(oid).is_some() || oid < 16384),
        },
    })
}

/// The first schema of the search path for which `has` holds.
fn first_schema(c: &FnCtx, cat: &Catalog, has: impl Fn(u32) -> bool) -> Option<u32> {
    cat.effective_path(&c.exec.search_path)
        .into_iter()
        .find(|s| has(*s))
}

/// Whether a built-in object of `table` (oid, name, namespace in its first three columns)
/// named `name` lives in schema `s`; `extra` checks the rest of its row.
fn builtin_named(
    table: crate::catalog::system::SysTable,
    s: u32,
    name: &str,
    extra: impl Fn(&[Value]) -> bool,
) -> bool {
    use crate::catalog::static_rows::{oid_at, rows, text_at};
    s == crate::catalog::PG_CATALOG_OID
        && rows(table)
            .iter()
            .any(|r| text_at(r, 1) == name && oid_at(r, 2) == s && extra(r))
}

/// pg_operator_is_visible (OperatorIsVisible): no operator with its name and argument
/// types earlier in the path.
fn operator_visible(a: &[Value], c: &FnCtx) -> Result<Value> {
    use crate::catalog::static_rows::oid_at;
    let oid = oid_arg(&a[0]);
    let cat = c.exec.catalog();
    let Some(o) = cat.operators.get(&oid).cloned() else {
        return visible(a, c);
    };
    let tyo = |t: Option<&crate::types::DataType>| {
        t.map_or(0, |t| crate::catalog::system::type_oid(&cat, t))
    };
    let (left, right) = (tyo(o.left.as_ref()), tyo(Some(&o.right)));
    let first = first_schema(c, &cat, |s| {
        cat.operators.values().any(|x| {
            x.schema == s
                && x.name == o.name
                && tyo(x.left.as_ref()) == left
                && tyo(Some(&x.right)) == right
        }) || builtin_named(crate::catalog::system::SysTable::PgOperator, s, &o.name, |r| {
            oid_at(r, 7) == left && oid_at(r, 8) == right
        })
    });
    Ok(Value::Bool(first == Some(o.schema)))
}

/// pg_opclass_is_visible / pg_opfamily_is_visible.
fn opclass_visible(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    Ok(match cat.opclass_info(oid_arg(&a[0])) {
        Some(x) => Value::Bool(cat.opclass_visible(&x, &c.exec.search_path)),
        None => Value::Null,
    })
}

fn opfamily_visible(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    Ok(match cat.opfamily_info(oid_arg(&a[0])) {
        Some(x) => Value::Bool(cat.opfamily_visible(&x, &c.exec.search_path)),
        None => Value::Null,
    })
}

/// pg_collation_is_visible: the first schema in the path with a collation of its name.
fn collation_visible(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = oid_arg(&a[0]);
    let cat = c.exec.catalog();
    let Some(x) = cat.collations.get(&oid).cloned() else {
        return visible(a, c);
    };
    let first = first_schema(c, &cat, |s| {
        cat.collations
            .values()
            .any(|y| y.namespace == s && y.name == x.name)
            || builtin_named(crate::catalog::system::SysTable::PgCollation, s, &x.name, |_| true)
    });
    Ok(Value::Bool(first == Some(x.namespace)))
}

/// pg_conversion_is_visible.
fn conversion_visible(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = oid_arg(&a[0]);
    let cat = c.exec.catalog();
    let all = cat.conversions_all();
    let Some(x) = all.iter().find(|x| x.oid == oid) else {
        return Ok(Value::Null);
    };
    let first = first_schema(c, &cat, |s| {
        all.iter().any(|y| y.schema == s && y.name == x.name)
    });
    Ok(Value::Bool(first == Some(x.schema)))
}

/// pg_statistics_obj_is_visible.
fn statistics_visible(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = oid_arg(&a[0]);
    let cat = c.exec.catalog();
    let Some(x) = cat.statistics.get(&oid).cloned() else {
        return Ok(Value::Null);
    };
    // (StatisticsObjIsVisible: the temporary schema isn't searched.)
    let first = cat.routine_path(&c.exec.search_path).into_iter().find(|s| {
        cat.statistics
            .values()
            .any(|y| y.schema == *s && y.name == x.name)
    });
    Ok(Value::Bool(first == Some(x.schema)))
}

/// pg_type_is_visible (TypeIsVisible): the first schema in the path with a type of its name.
fn type_visible(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = oid_arg(&a[0]);
    let cat = c.exec.catalog();
    let named = cat
        .user_type_name(oid)
        .map(|(s, n, array)| (s, if array { cat.array_typname(oid, n) } else { n.to_string() }))
        .or_else(|| cat.domains.get(&oid).map(|d| (d.schema, d.name.clone())))
        .or_else(|| {
            cat.base_types
                .values()
                .find(|b| b.oid() == oid)
                .map(|b| (b.ty.schema, b.ty.name.to_string()))
        });
    let Some((schema, name)) = named else {
        return Ok(Value::Bool(true));
    };
    // TypeIsVisible: the first schema in the path with a type of that typname (a
    // relation's row type and array types count).
    let first = first_schema(c, &cat, |s| {
        cat.type_named_in(s, &name)
            || cat.type_by_name(Some(s), &name, &[]).is_some()
            || builtin_named(crate::catalog::system::SysTable::PgType, s, &name, |_| true)
    });
    Ok(Value::Bool(first == Some(schema)))
}

fn obj_description(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = oid_arg(&a[0]);
    // Large objects keep their comments with their metadata.
    if a.get(1)
        .is_none_or(|v| matches!(v, Value::Text(t) if t.as_ref() == "pg_largeobject"))
        && let Some(m) =
            crate::largeobj::meta(&c.exec.txn.borrow(), oid, crate::largeobj::Snap::Txn)?
        && (m.comment.is_some() || a.len() > 1)
    {
        return Ok(m.comment.map_or(Value::Null, text));
    }
    let cat = c.exec.catalog();
    // With a catalog name, only the object of that class.
    let class = a.get(1).map(|v| catalog_class(text_arg(v)));
    let dbs: Vec<u32> = c
        .exec
        .sess
        .cluster
        .databases()
        .iter()
        .map(|d| d.0)
        .collect();
    if cat.shared_class_of(oid, &dbs).is_none()
        && let Some((_, _, t)) = cat
            .comments
            .iter()
            .find(|(o, s, t)| *o == oid && *s == 0 && !t.is_empty())
        && class.is_none_or(|k| {
            cat.class_of(oid)
                .unwrap_or(crate::catalog::depend::CONSTRAINT)
                == k
        })
    {
        return Ok(text(t.as_str()));
    }
    Ok(crate::catalog::system::builtin_description(&cat, oid, class, 0).map_or(Value::Null, text))
}

/// pg_extension.oid of the built-in plpgsql extension.
pub const PLPGSQL_EXT_OID: u32 = 13677;
fn col_description(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = oid_arg(&a[0]);
    let sub = int_arg(&a[1]) as i32;
    let cat = c.exec.catalog();
    match cat.comments.iter().find(|(o, s, _)| *o == oid && *s == sub) {
        Some((_, _, t)) if t.is_empty() => Ok(Value::Null),
        Some((_, _, t)) => Ok(text(t.as_str())),
        None => Ok(
            crate::catalog::system::builtin_description(&cat, oid, None, sub)
                .map_or(Value::Null, text),
        ),
    }
}
fn shobj_description(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = oid_arg(&a[0]);
    let class = catalog_class(text_arg(&a[1]));
    let cat = c.exec.catalog();
    let dbs: Vec<u32> = c
        .exec
        .sess
        .cluster
        .databases()
        .iter()
        .map(|d| d.0)
        .collect();
    if cat.shared_class_of(oid, &dbs) != Some(class) {
        return Ok(Value::Null);
    }
    match cat.comments.iter().find(|(o, s, _)| *o == oid && *s == 0) {
        Some((_, _, t)) if t.is_empty() => Ok(Value::Null),
        Some((_, _, t)) => Ok(text(t.as_str())),
        None => Ok(crate::catalog::system::builtin_shdescription(
            &cat,
            &c.exec.sess.cluster.databases(),
            oid,
            class,
        )
        .map_or(Value::Null, text)),
    }
}

/// The oid of system catalog `name` (0: there's none).
fn catalog_class(name: &str) -> u32 {
    crate::catalog::system::SysTable::lookup(Some("pg_catalog"), name, &[]).map_or(0, |t| t.oid())
}
fn pg_get_userbyid(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = oid_arg(&a[0]);
    let cat = c.exec.catalog();
    Ok(text(
        cat.roles
            .get(&oid)
            .map(|r| r.name.clone())
            .unwrap_or_else(|| format!("unknown (OID={oid})")),
    ))
}

/// One index key / predicate as Postgres prints it.
fn index_expr(
    ctx: &ExecCtx,
    t: &Arc<crate::catalog::Table>,
    e: &str,
    kind: Stored,
    pretty: bool,
) -> (String, bool) {
    let shown = parse_expr(e)
        .ok()
        .and_then(|n| bind_stored(ctx, t, &n, kind))
        .and_then(|x| {
            let s = deparse(&x, &column_names(t), ctx, pretty)?;
            Some((s, crate::sql::ruleutils::looks_like_function(&x)))
        });
    shown.unwrap_or_else(|| (e.to_string(), looks_like_function(e)))
}

/// An index's key expression as `pg_get_indexdef_columns` shows it (in a key's error
/// detail): pretty, parenthesized unless it looks like a function call.
pub fn index_key_text(ctx: &ExecCtx, t: &crate::catalog::Table, e: &str) -> String {
    match index_expr(ctx, &Arc::new(t.clone()), e, Stored::Plain, true) {
        (s, true) => s,
        (s, false) => format!("({s})"),
    }
}

/// CREATE INDEX statement text for an index (pg_get_indexdef). `pretty` prints the table
/// unqualified when it is visible, and fewer parentheses.
pub fn index_def(ctx: &ExecCtx, oid: u32, pretty: bool) -> Option<String> {
    let cat = ctx.catalog();
    let ix = cat.indexes.get(&oid)?;
    let t = cat.index_table(ix.table)?;
    let table = if pretty {
        cat.qualified_name(t.schema, &t.name, &ctx.search_path)
    } else {
        format!(
            "{}.{}",
            quote_ident(cat.schema_name(t.schema)),
            quote_ident(&t.name)
        )
    };
    let body = index_body(ctx, &t, ix, None, pretty);
    // A WITHOUT OVERLAPS constraint's index is unique for the catalogs (indisunique).
    let unique = ix.unique
        || t.constraints
            .iter()
            .any(|c| c.kind.index() == Some(ix.oid) && c.without_overlaps());
    // (A partitioned index is defined ON ONLY its table: RELKIND_PARTITIONED_INDEX.)
    Some(format!(
        "CREATE {}INDEX {} ON {}{table} {body}",
        if unique { "UNIQUE " } else { "" },
        quote_ident(&ix.name),
        if t.partkey.is_some() { "ONLY " } else { "" }
    ))
}

/// `USING method (keys) INCLUDE (...) NULLS NOT DISTINCT WITH (...) WHERE ...` of an index;
/// `excl`: an exclusion constraint's operators, shown as `key WITH op`.
fn index_body(
    ctx: &ExecCtx,
    t: &Arc<crate::catalog::Table>,
    ix: &crate::catalog::Index,
    excl: Option<&[String]>,
    pretty: bool,
) -> String {
    let cols: Vec<String> = ix
        .columns
        .iter()
        .enumerate()
        .map(|(n, c)| {
            let mut s = match &c.key {
                IndexKey::Column(i) => quote_ident(&t.columns[*i].name),
                IndexKey::Expr(e) => match index_expr(ctx, t, e, Stored::Plain, pretty) {
                    (e, true) => e,
                    (e, false) => format!("({e})"),
                },
            };
            if let Some(coll) = &c.collation {
                let natural = match &c.key {
                    IndexKey::Column(i) => crate::catalog::system::column_collation(&t.columns[*i]),
                    IndexKey::Expr(_) => 100,
                };
                // (A made collation is another collation, even one copying the column's.)
                let oid = crate::types::collation::def_by_name(&ctx.catalog(), coll).map_or(100, |d| d.oid);
                if oid != natural {
                    s.push_str(&format!(" COLLATE {}", quote_ident(coll)));
                }
            }
            // With options, the class shows even when it is the default.
            let opts = !c.opclass_options.is_empty();
            if let Some((name, default)) = crate::catalog::system::opclass_name(c.opclass)
                && (!default || opts)
            {
                s.push(' ');
                s.push_str(&quote_ident(name));
            } else if let Some(uc) = ctx.catalog().opclass_info(c.opclass)
                && (!uc.default || opts)
                && crate::catalog::system::opclass_name(c.opclass).is_none()
            {
                // A made class: qualified unless visible (get_opclass_name).
                let cat = ctx.catalog();
                s.push(' ');
                s.push_str(&if cat.opclass_visible(&uc, &ctx.search_path) {
                    quote_ident(&uc.name)
                } else {
                    format!(
                        "{}.{}",
                        quote_ident(cat.schema_name(uc.schema)),
                        quote_ident(&uc.name)
                    )
                });
            }
            if opts {
                // get_reloptions: values that read as identifiers stay bare.
                let o: Vec<String> = c
                    .opclass_options
                    .iter()
                    .map(|o| match o.split_once('=') {
                        Some((k, v)) if quote_ident(v) == v => format!("{k}={v}"),
                        Some((k, v)) => format!("{k}='{}'", v.replace('\'', "''")),
                        None => o.clone(),
                    })
                    .collect();
                s.push_str(&format!(" ({})", o.join(", ")));
            }
            if c.desc {
                s.push_str(" DESC");
                if !c.nulls_first {
                    s.push_str(" NULLS LAST");
                }
            } else if c.nulls_first {
                s.push_str(" NULLS FIRST");
            }
            if let Some(op) = excl.and_then(|o| o.get(n)) {
                s.push_str(&format!(" WITH {op}"));
            }
            s
        })
        .collect();
    let mut s = format!("USING {} ({})", ix.method, cols.join(", "));
    if !ix.include.is_empty() {
        s.push_str(&format!(
            " INCLUDE ({})",
            ix.include
                .iter()
                .map(|i| quote_ident(&t.columns[*i].name))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if ix.nulls_not_distinct {
        s.push_str(" NULLS NOT DISTINCT");
    }
    if !ix.options.is_empty() {
        let opts: Vec<String> = ix
            .options
            .iter()
            .map(|o| match o.split_once('=') {
                // flatten_reloptions: values that read as identifiers stay bare.
                Some((k, v)) if quote_ident(v) == v => format!("{k}={v}"),
                Some((k, v)) => format!("{k}='{}'", v.replace('\'', "''")),
                None => o.clone(),
            })
            .collect();
        s.push_str(&format!(" WITH ({})", opts.join(", ")));
    }
    if let Some(p) = &ix.predicate {
        let p = index_expr(ctx, t, p, Stored::Condition, pretty).0;
        // A constraint shows the predicate parenthesised once more.
        if excl.is_some() {
            s.push_str(&format!(" WHERE ({p})"));
        } else {
            s.push_str(&format!(" WHERE {p}"));
        }
    }
    s
}

fn pg_get_indexdef(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    let oid = oid_arg(&a[0]);
    let pretty = a.get(2).is_some_and(|v| matches!(v, Value::Bool(true)));
    let col = if a.len() >= 2 { int_arg(&a[1]).max(0) as usize } else { 0 };
    if !cat.indexes.contains_key(&oid) {
        return Ok(system_index_def(&cat, oid, col, pretty, &c.exec.search_path)
            .map(text)
            .unwrap_or(Value::Null));
    }
    if col > 0 {
        let Some(ix) = cat.indexes.get(&oid) else {
            return Ok(Value::Null);
        };
        let Some(t) = cat.index_table(ix.table) else {
            return Ok(Value::Null);
        };
        return Ok(match ix.columns.get(col - 1) {
            Some(ic) => text(match &ic.key {
                IndexKey::Column(i) => quote_ident(&t.columns[*i].name),
                IndexKey::Expr(e) => match index_expr(c.exec, &t, e, Stored::Plain, pretty) {
                    (e, true) => e,
                    (e, false) => format!("({e})"),
                },
            }),
            None => match ix.include.get(col - 1 - ix.columns.len()) {
                Some(i) => text(quote_ident(&t.columns[*i].name)),
                None => Value::Null,
            },
        });
    }
    Ok(index_def(c.exec, oid, pretty)
        .map(text)
        .unwrap_or(Value::Null))
}

/// pg_get_indexdef of a system catalog's index or a built-in TOAST table's (key column
/// `col` alone when > 0).
fn system_index_def(
    cat: &Catalog,
    oid: u32,
    col: usize,
    pretty: bool,
    search_path: &[String],
) -> Option<String> {
    use crate::catalog::static_rows::{oid_at, rows, text_at};
    let class = rows(crate::catalog::system::SysTable::PgClass);
    let (name, ns) = match cat.toast_owner(oid) {
        Some((t, true)) => (t.toast_relname(true), cat.toast_schema(t)),
        Some(_) => return None,
        None => {
            let r = class.iter().find(|r| oid_at(r, 0) == oid)?;
            (text_at(r, 1).to_string(), oid_at(r, 2))
        }
    };
    let (table, keys, unique) = if let Some(n) = name
        .strip_prefix("pg_toast_")
        .and_then(|x| x.strip_suffix("_index"))
    {
        (
            format!("pg_toast_{n}"),
            vec!["chunk_id".to_string(), "chunk_seq".to_string()],
            true,
        )
    } else {
        let line = include_str!("../../catalog/sys_indexes.tsv")
            .lines()
            .filter(|l| !l.starts_with('#'))
            .find(|l| l.split('\t').nth(1) == Some(name.as_str()))?;
        let f: Vec<&str> = line.split('\t').collect();
        (
            f[0].to_string(),
            f.get(2)?.split(',').map(str::to_string).collect(),
            f.get(3) == Some(&"t"),
        )
    };
    if col > 0 {
        return keys.get(col - 1).map(|k| quote_ident(k));
    }
    let table = if pretty {
        cat.qualified_name(ns, &table, search_path)
    } else {
        format!("{}.{}", quote_ident(cat.schema_name(ns)), quote_ident(&table))
    };
    Some(format!(
        "CREATE {}INDEX {} ON {table} USING btree ({})",
        if unique { "UNIQUE " } else { "" },
        quote_ident(&name),
        keys.iter().map(|k| quote_ident(k)).collect::<Vec<_>>().join(", ")
    ))
}

/// Constraint definition text (pg_get_constraintdef).
pub fn constraint_def(
    ctx: &ExecCtx,
    cat: &Catalog,
    table: &Arc<crate::catalog::Table>,
    con: &crate::catalog::Constraint,
    pretty: bool,
) -> String {
    let colnames = |cols: &[usize], t: &crate::catalog::Table| {
        cols.iter()
            .map(|i| quote_ident(&t.columns[*i].name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match &con.kind {
        ConstraintKind::PrimaryKey { index } | ConstraintKind::Unique { index } => {
            let ix = cat.indexes.get(index);
            let cols: Vec<usize> = ix
                .map(|ix| {
                    ix.columns
                        .iter()
                        .filter_map(|c| {
                            if let IndexKey::Column(i) = c.key {
                                Some(i)
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            let kind = if matches!(con.kind, ConstraintKind::PrimaryKey { .. }) {
                "PRIMARY KEY"
            } else {
                "UNIQUE"
            };
            let nnd = if ix.is_some_and(|i| i.nulls_not_distinct) {
                " NULLS NOT DISTINCT"
            } else {
                ""
            };
            let mut s = format!("{kind}{nnd} ({})", colnames(&cols, table));
            if let Some(ix) = ix
                && !ix.include.is_empty()
            {
                s.push_str(&format!(" INCLUDE ({})", colnames(&ix.include, table)));
            }
            match con.deferral() {
                (true, true) => s.push_str(" DEFERRABLE INITIALLY DEFERRED"),
                (true, false) => s.push_str(" DEFERRABLE"),
                _ => {}
            }
            s
        }
        ConstraintKind::Check { expr, no_inherit } => {
            let expr = stored_expr(ctx, table, expr, Stored::Condition, pretty)
                .unwrap_or_else(|| expr.clone());
            // Validity doesn't matter for a NOT ENFORCED constraint.
            let state = if !con.enforced {
                " NOT ENFORCED"
            } else if !con.validated {
                " NOT VALID"
            } else {
                ""
            };
            format!(
                "CHECK ({expr}){}{state}",
                if *no_inherit { " NO INHERIT" } else { "" }
            )
        }
        ConstraintKind::NotNull { column } => {
            format!(
                "NOT NULL {}{}{}",
                quote_ident(&table.columns[*column].name),
                if con.no_inherit { " NO INHERIT" } else { "" },
                if con.validated { "" } else { " NOT VALID" }
            )
        }
        ConstraintKind::Exclusion { index, .. } if con.without_overlaps() => {
            let ix = cat.indexes.get(index);
            let cols: Vec<usize> = ix
                .map(|ix| {
                    ix.columns
                        .iter()
                        .filter_map(|c| match c.key {
                            IndexKey::Column(i) => Some(i),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            let kind = if con.period == Some('p') {
                "PRIMARY KEY"
            } else {
                "UNIQUE"
            };
            let mut s = format!("{kind} ({} WITHOUT OVERLAPS)", colnames(&cols, table));
            if let Some(ix) = ix
                && !ix.include.is_empty()
            {
                s.push_str(&format!(" INCLUDE ({})", colnames(&ix.include, table)));
            }
            match con.deferral() {
                (true, true) => s.push_str(" DEFERRABLE INITIALLY DEFERRED"),
                (true, false) => s.push_str(" DEFERRABLE"),
                _ => {}
            }
            s
        }
        ConstraintKind::Exclusion { index, ops } => {
            let mut s = match cat.indexes.get(index) {
                Some(ix) => format!("EXCLUDE {}", index_body(ctx, table, ix, Some(ops), pretty)),
                None => "EXCLUDE".into(),
            };
            match con.deferral() {
                (true, true) => s.push_str(" DEFERRABLE INITIALLY DEFERRED"),
                (true, false) => s.push_str(" DEFERRABLE"),
                _ => {}
            }
            s
        }
        ConstraintKind::ForeignKey {
            columns,
            ref_table,
            ref_columns,
            on_delete,
            on_update,
            match_full,
            ..
        } => {
            let rt = cat.tables.get(ref_table);
            let rname = rt
                .map(|t| cat.qualified_name(t.schema, &t.name, &ctx.search_path))
                .unwrap_or_default();
            // PERIOD before the last column of each list.
            let cols = |cols: &[usize], t: &crate::catalog::Table| {
                let s = colnames(cols, t);
                match s.rfind(", ") {
                    Some(p) if con.is_period() => format!("{}, PERIOD {}", &s[..p], &s[p + 2..]),
                    _ => s,
                }
            };
            let mut s = format!(
                "FOREIGN KEY ({}) REFERENCES {}({})",
                cols(columns, table),
                rname,
                rt.map(|t| cols(ref_columns, t)).unwrap_or_default()
            );
            if *match_full {
                s.push_str(" MATCH FULL");
            }
            let action = |a: &crate::catalog::FkAction| match a {
                crate::catalog::FkAction::NoAction => None,
                crate::catalog::FkAction::Restrict => Some("RESTRICT"),
                crate::catalog::FkAction::Cascade => Some("CASCADE"),
                crate::catalog::FkAction::SetNull => Some("SET NULL"),
                crate::catalog::FkAction::SetDefault => Some("SET DEFAULT"),
            };
            if let Some(a) = action(on_update) {
                s.push_str(&format!(" ON UPDATE {a}"));
            }
            if let Some(a) = action(on_delete) {
                s.push_str(&format!(" ON DELETE {a}"));
                if !con.del_set_cols.is_empty() {
                    s.push_str(&format!(" ({})", colnames(&con.del_set_cols, table)));
                }
            }
            match con.deferral() {
                (true, true) => s.push_str(" DEFERRABLE INITIALLY DEFERRED"),
                (true, false) => s.push_str(" DEFERRABLE"),
                _ => {}
            }
            if !con.enforced {
                s.push_str(" NOT ENFORCED");
            } else if !con.validated {
                s.push_str(" NOT VALID");
            }
            s
        }
    }
}

fn pg_get_constraintdef(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    let oid = oid_arg(&a[0]);
    for t in cat.tables.values() {
        if let Some(con) = t.constraints.iter().find(|c| c.oid == oid) {
            let pretty = a.get(1).is_some_and(|v| matches!(v, Value::Bool(true)));
            return Ok(text(constraint_def(c.exec, &cat, t, con, pretty)));
        }
    }
    for d in cat.domains.values() {
        if d.not_null && oid == crate::catalog::system::domain_constraint_oid(d, 63) {
            return Ok(text("NOT NULL".to_string()));
        }
        for (i, (name, sql)) in d.checks.iter().enumerate() {
            if oid == crate::catalog::system::domain_constraint_oid(d, i) {
                let pretty = a.get(1).is_some_and(|v| matches!(v, Value::Bool(true)));
                let expr = crate::sql::ruleutils::domain_check(c.exec, &d.base, sql, pretty)
                    .unwrap_or_else(|| sql.clone());
                let state = if d.invalid.contains(name) {
                    " NOT VALID"
                } else {
                    ""
                };
                return Ok(text(format!("CHECK ({expr}){state}")));
            }
        }
    }
    Ok(Value::Null)
}
/// pg_get_ruledef(rule [, pretty]): a CREATE RULE rule, a view's `_RETURN` rule, or one of
/// Postgres's own (pg_settings').
fn pg_get_ruledef(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = oid_arg(&a[0]);
    let pretty = matches!(a.get(1), Some(Value::Bool(true)));
    let cat = c.exec.catalog();
    if let Some(r) = cat.rules.get(&oid) {
        return Ok(crate::sql::ruleutils::rule_def(c.exec, r, pretty)
            .map(text)
            .unwrap_or(Value::Null));
    }
    match oid {
        12108 => {
            return Ok(text(if pretty {
                "CREATE RULE pg_settings_u AS\n    ON UPDATE TO pg_settings\n   WHERE new.name = old.name DO  SELECT set_config(old.name, new.setting, false) AS set_config;"
            } else {
                "CREATE RULE pg_settings_u AS\n    ON UPDATE TO pg_catalog.pg_settings\n   WHERE (new.name = old.name) DO  SELECT set_config(old.name, new.setting, false) AS set_config;"
            }));
        }
        12109 => {
            return Ok(text(if pretty {
                "CREATE RULE pg_settings_n AS\n    ON UPDATE TO pg_settings DO INSTEAD NOTHING;"
            } else {
                "CREATE RULE pg_settings_n AS\n    ON UPDATE TO pg_catalog.pg_settings DO INSTEAD NOTHING;"
            }));
        }
        _ => {}
    }
    // A view's ON SELECT rule.
    let head = |schema: &str, name: &str| {
        let rel = if pretty {
            cat.schema_by_name(schema)
                .map(|s| cat.qualified_name(s.oid, name, &c.exec.search_path))
                .unwrap_or_else(|| crate::catalog::quote_ident(name))
        } else {
            format!(
                "{}.{}",
                crate::catalog::quote_ident(schema),
                crate::catalog::quote_ident(name)
            )
        };
        format!("CREATE RULE \"_RETURN\" AS\n    ON SELECT TO {rel} DO INSTEAD ")
    };
    if let Some(v) = cat.views.values().find(|v| v.rule_oid() == oid) {
        let def = crate::sql::ruleutils::view_def(c.exec, v, pretty)
            .unwrap_or_else(|| format!(" {};", v.query));
        return Ok(text(head(cat.schema_name(v.schema), &v.name) + &def));
    }
    Ok(Value::Null)
}

fn pg_get_viewdef(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    let oid = match &a[0] {
        // pg_get_viewdef_name: RangeVarGetRelid(.., missing_ok = false).
        Value::Text(t) => match c.exec.fmt().names.as_ref().map(|n| n.rel_oid(t)) {
            Some(r) => r?,
            None => return Ok(Value::Null),
        },
        v => oid_arg(v),
    };
    // pg_get_viewdef(view, pretty) or (view, wrap column): the latter is always pretty.
    let (pretty, wrap) = match a.get(1) {
        Some(Value::Bool(p)) => (*p, 0),
        Some(Value::Null) | None => (false, 0),
        Some(v) => (true, usize::try_from(int_arg(v)).unwrap_or(usize::MAX)),
    };
    if let Some(d) = crate::catalog::system::system_view_def(oid) {
        return Ok(text(if pretty { d.pretty.as_str() } else { d.def.as_str() }));
    }
    Ok(cat
        .views
        .get(&oid)
        .map(|v| {
            text(
                crate::sql::ruleutils::view_def_wrap(c.exec, v, pretty, wrap)
                    .unwrap_or_else(|| format!(" {};", v.query)),
            )
        })
        .unwrap_or(Value::Null))
}
fn pg_get_expr(a: &[Value], c: &FnCtx) -> Result<Value> {
    // Expressions are stored as SQL text; rebind it over the relation and print it like
    // Postgres prints its stored tree.
    let Value::Text(t) = &a[0] else {
        return Ok(Value::Null);
    };
    // Partition bounds are stored as they print.
    if t.starts_with("FOR VALUES") || &**t == "DEFAULT" {
        return Ok(Value::Text(t.clone()));
    }
    let pretty = a.get(2).is_some_and(|v| matches!(v, Value::Bool(true)));
    let cat = c.exec.catalog();
    let rel = cat.tables.get(&oid_arg(&a[1]));
    // A policy's expression (pg_policy shows it as its node tree): the policy's SQL.
    let t = match rel {
        Some(rel) if t.starts_with('{') => cat
            .policies
            .values()
            .filter(|p| p.table == rel.oid)
            .flat_map(|p| p.qual.iter().chain(p.with_check.iter()))
            .find(|q| {
                crate::sql::nodetree::policy_expr_tree(c.exec, rel, q).as_deref() == Some(&**t)
            })
            .map_or_else(|| t.clone(), |q| Arc::from(q.as_str())),
        _ => t.clone(),
    };
    let t = &t;
    // A column default's (adbin): its tree has the coercion to the column's type, which
    // shows in pretty mode (parentheses around its argument).
    if pretty && let Some(rel) = rel {
        for col in rel.columns.iter().filter(|c| !c.dropped) {
            let Some(d) = col.generated.as_deref().or(col.default.as_deref()) else {
                continue;
            };
            if stored_expr(c.exec, rel, d, Stored::Default(&col.ty), false).as_deref()
                == Some(&**t)
                && let Some(s) = stored_expr(c.exec, rel, d, Stored::Default(&col.ty), true)
            {
                return Ok(text(s));
            }
        }
    }
    let shown = rel.and_then(|rel| stored_expr(c.exec, rel, t, Stored::Plain, pretty));
    Ok(text(shown.unwrap_or_else(|| t.to_string())))
}
/// What a function printing deparsed SQL returns, as `ruleutils.c` prints string literals
/// under the session's `standard_conforming_strings`.
fn deparsed(r: Result<Value>) -> Result<Value> {
    let fix = |t: &Arc<str>| match crate::sql::scs::session_literals(t) {
        std::borrow::Cow::Owned(s) => Arc::from(s),
        std::borrow::Cow::Borrowed(_) => t.clone(),
    };
    Ok(match r? {
        Value::Text(t) => Value::Text(fix(&t)),
        Value::Array(mut a) => {
            for v in &mut Arc::make_mut(&mut a).items {
                if let Value::Text(t) = v {
                    *t = fix(t);
                }
            }
            Value::Array(a)
        }
        v => v,
    })
}

/// A built-in function or aggregate by oid.
fn builtin_fn(oid: u32) -> Option<Arc<super::FuncDef>> {
    super::registry()
        .funcs
        .values()
        .flatten()
        .find(|d| d.oid == oid)
        .cloned()
        .or_else(|| {
            crate::exec::agg::agg_registry()
                .by_name
                .values()
                .flatten()
                .find(|d| d.sig.oid == oid)
                .map(|d| d.sig.clone())
        })
}

/// A built-in's argument list: names where it has them, `VARIADIC` on the last.
fn builtin_arguments(d: &super::FuncDef) -> String {
    let n = d.args.len();
    d.args
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let variadic = d.variadic && i + 1 == n;
            let ty = match t {
                crate::types::DataType::Array(e)
                    if variadic && matches!(**e, crate::types::DataType::Any) =>
                {
                    e.display()
                }
                t => t.display(),
            };
            let name = d
                .arg_names
                .get(i)
                .filter(|s| !s.is_empty())
                .map(|s| format!("{s} "))
                .unwrap_or_default();
            format!("{}{name}{ty}", if variadic { "VARIADIC " } else { "" })
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `print_function_arguments`: modes, names, types and (optionally) defaults; TABLE
/// columns are part of the result instead.
fn function_arguments(
    cat: &Catalog,
    f: &crate::catalog::Function,
    defaults: bool,
    search_path: &[String],
) -> String {
    // An ordered-set aggregate's aggregated arguments follow ORDER BY (a merged VARIADIC
    // one is printed on both sides).
    let order_by = f
        .agg
        .as_deref()
        .filter(|a| a.kind != 'n')
        .map(|a| a.num_direct as usize);
    let mut params = f.all_params();
    if let Some(n) = order_by
        && n == params.len()
        && let Some(last) = params.last().cloned()
    {
        params.push(last);
    }
    let mut parts: Vec<String> = Vec::new();
    let mut input = 0;
    for (k, (name, ty, mode)) in params.into_iter().enumerate() {
        if order_by == Some(k) {
            let s = format!(
                "{}ORDER BY ",
                parts.join(", ") + if k > 0 { " " } else { "" }
            );
            parts = vec![s];
            let ty = cat.param_type_name(f, input.min(f.args.len().saturating_sub(1)), search_path);
            let prefix = if mode == 'v' { "VARIADIC " } else { "" };
            let item = match &name {
                Some(n) if !n.is_empty() => format!("{prefix}{} {ty}", quote_ident(n)),
                _ => format!("{prefix}{ty}"),
            };
            parts[0].push_str(&item);
            input += 1;
            continue;
        }
        let prefix = match mode {
            't' => continue,
            'i' if f.procedure => "IN ",
            'b' => "INOUT ",
            'o' => "OUT ",
            'v' => "VARIADIC ",
            _ => "",
        };
        let ty = if mode == 'o' {
            cat.format_type(&ty, None, search_path)
        } else {
            cat.param_type_name(f, input, search_path)
        };
        let mut s = match name {
            Some(n) if !n.is_empty() => format!("{prefix}{} {ty}", quote_ident(&n)),
            _ => format!("{prefix}{ty}"),
        };
        if mode != 'o' {
            if defaults && let Some(Some(d)) = f.defaults.get(input) {
                s.push_str(&format!(" DEFAULT {d}"));
            }
            input += 1;
        }
        parts.push(s);
    }
    parts.join(", ")
}

/// `print_function_rettype`.
fn function_result(cat: &Catalog, f: &crate::catalog::Function, search_path: &[String]) -> String {
    let cols = |c: &[(String, T)]| {
        c.iter()
            .map(|(n, t)| {
                format!(
                    "{} {}",
                    quote_ident(n),
                    cat.format_type(t, None, search_path)
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    // print_function_rettype: TABLE(...) for RETURNS TABLE, else the result type, which
    // is the OUT parameter's type when there is just one.
    let table = f.modes.is_empty() || f.modes.contains(&'t');
    let out_type = || match f.out_cols.as_slice() {
        [(_, t)] => cat.format_type(t, None, search_path),
        _ => "record".to_string(),
    };
    match (f.returns_set, f.out_cols.is_empty()) {
        (true, false) if table => format!("TABLE({})", cols(&f.out_cols)),
        (true, false) => format!("SETOF {}", out_type()),
        (true, true) => format!("SETOF {}", cat.ret_type_name(f, search_path)),
        (false, false) => out_type(),
        (false, true) => cat.ret_type_name(f, search_path),
    }
}

/// What pg_get_function_arguments, pg_get_function_identity_arguments and
/// pg_get_function_result show for built-in function `oid`, as Postgres has it.
fn builtin_proc_text(oid: u32, i: usize) -> Option<Value> {
    static TEXT: OnceLock<std::collections::HashMap<u32, [&'static str; 3]>> = OnceLock::new();
    let m = TEXT.get_or_init(|| {
        include_str!("data/proc_text.tsv")
            .lines()
            .filter_map(|l| {
                let f: Vec<&'static str> = l.split('\t').collect();
                Some((
                    f.first()?.parse().ok()?,
                    [*f.get(1)?, *f.get(2)?, *f.get(3)?],
                ))
            })
            .collect()
    });
    m.get(&oid).map(|t| text(t[i]))
}

/// pg_get_function_arg_default: the DEFAULT of parameter `n` (counting every parameter).
fn pg_get_function_arg_default(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (oid, n) = (oid_arg(&a[0]), a[1].as_i64().unwrap_or(0));
    let cat = c.exec.catalog();
    let Some(f) = cat.functions.get(&oid) else {
        static DEFAULTS: OnceLock<std::collections::HashMap<(u32, i64), &'static str>> =
            OnceLock::new();
        let m = DEFAULTS.get_or_init(|| {
            include_str!("data/arg_defaults.tsv")
                .lines()
                .filter_map(|l| {
                    let mut f = l.splitn(3, '\t');
                    Some((
                        (f.next()?.parse().ok()?, f.next()?.parse().ok()?),
                        f.next()?,
                    ))
                })
                .collect()
        });
        return Ok(m.get(&(oid, n)).map_or(Value::Null, |d| text(*d)));
    };
    let mut input = 0;
    for (i, (_, _, mode)) in f.all_params().into_iter().enumerate() {
        let is_input = !matches!(mode, 'o' | 't');
        if i as i64 + 1 == n {
            return Ok(match f.defaults.get(input) {
                Some(Some(d)) if is_input => text(d.as_str()),
                _ => Value::Null,
            });
        }
        if is_input {
            input += 1;
        }
    }
    Ok(Value::Null)
}

/// Whether schema `oid` is another session's temporary schema.
fn pg_is_other_temp_schema(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    let Some(s) = cat.schemas.get(&oid_arg(&a[0])) else {
        return Ok(Value::Bool(false));
    };
    let mine = c.exec.sess.temp_schema();
    let temp = s.name.starts_with("pg_temp_") || s.name.starts_with("pg_toast_temp_");
    Ok(Value::Bool(
        temp && s.name != mine && s.name != format!("pg_toast_temp_{}", c.exec.sess.pid),
    ))
}

fn pg_indexam_progress_phasename(_: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Null)
}

/// A range type's constructor (two of the range, three of the multirange): its arguments
/// and its result as pg_get_function_arguments / _result print them.
fn range_ctor_text(cat: &Catalog, oid: u32, search_path: &[String]) -> Option<(String, String)> {
    let r = cat.ranges.values().find(|r| r.fn_oids.iter().take(5).any(|o| *o == oid))?;
    let i = r.fn_oids.iter().position(|o| *o == oid)?;
    let rt = crate::types::DataType::Range(r.range_type());
    let st = cat.format_type(&r.subtype, r.subtype_domain, search_path);
    let range = cat.format_type(&rt, None, search_path);
    let multi = cat.qualified_name(cat.multirange_schema(r), &r.multi_name, search_path);
    Some(match i {
        0 => (format!("{st}, {st}"), range),
        1 => (format!("{st}, {st}, text"), range),
        2 => (String::new(), multi),
        3 => (range, multi),
        _ => (format!("VARIADIC {range}[]"), multi),
    })
}

fn pg_get_function_arguments(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = oid_arg(&a[0]);
    let cat = c.exec.catalog();
    if let Some((args, _)) = range_ctor_text(&cat, oid, &c.exec.search_path) {
        return Ok(text(args));
    }
    if let Some(f) = cat.functions.get(&oid) {
        return Ok(text(function_arguments(&cat, f, true, &c.exec.search_path)));
    }
    if let Some(v) = builtin_proc_text(oid, 0) {
        return Ok(v);
    }
    Ok(builtin_fn(oid)
        .map(|d| text(builtin_arguments(&d)))
        .unwrap_or(Value::Null))
}

fn pg_get_function_identity_arguments(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = oid_arg(&a[0]);
    let cat = c.exec.catalog();
    if let Some((args, _)) = range_ctor_text(&cat, oid, &c.exec.search_path) {
        return Ok(text(args));
    }
    if let Some(f) = cat.functions.get(&oid) {
        return Ok(text(function_arguments(
            &cat,
            f,
            false,
            &c.exec.search_path,
        )));
    }
    if let Some(v) = builtin_proc_text(oid, 1) {
        return Ok(v);
    }
    Ok(builtin_fn(oid)
        .map(|d| text(builtin_arguments(&d)))
        .unwrap_or(Value::Null))
}

fn pg_get_function_result(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = oid_arg(&a[0]);
    let cat = c.exec.catalog();
    if let Some((_, ret)) = range_ctor_text(&cat, oid, &c.exec.search_path) {
        return Ok(text(ret));
    }
    if let Some(f) = cat.functions.get(&oid) {
        // A procedure has no result.
        if f.procedure {
            return Ok(Value::Null);
        }
        return Ok(text(function_result(&cat, f, &c.exec.search_path)));
    }
    if let Some(v) = builtin_proc_text(oid, 2) {
        return Ok(v);
    }
    Ok(builtin_fn(oid)
        .map(|d| {
            let ret = match &d.ret {
                Ret::Fixed(t) => t.display(),
                Ret::Poly => "anyelement".into(),
                Ret::PolyArray => "anyarray".into(),
                Ret::Arg(i) => d.args.get(*i).map(|t| t.display()).unwrap_or_default(),
            };
            text(if d.srf.is_some() {
                format!("SETOF {ret}")
            } else {
                ret
            })
        })
        .unwrap_or(Value::Null))
}

fn object_addr(a: &[Value]) -> crate::catalog::depend::Addr {
    (
        oid_arg(&a[0]),
        oid_arg(&a[1]),
        a[2].as_i64().unwrap_or(0) as i32,
    )
}

fn opt_text(v: Option<String>) -> Value {
    v.map_or(Value::Null, text)
}

fn text_list(v: Option<Vec<String>>) -> Value {
    v.map_or(Value::Null, |v| {
        Value::Array(Arc::new(ArrayValue::from_vec(
            T::Text,
            v.into_iter().map(text).collect(),
        )))
    })
}

/// Whether large object `oid` exists (as the catalog snapshot sees it).
fn lo_exists(c: &FnCtx, oid: u32) -> Result<bool> {
    Ok(crate::largeobj::meta(&c.exec.txn.borrow(), oid, crate::largeobj::Snap::Latest)?.is_some())
}

/// `getObjectIdentityParts` of the object the arguments address.
fn identify(a: &[Value], c: &FnCtx) -> Result<crate::catalog::identify::Identity> {
    let (class, oid, sub) = object_addr(a);
    if class == crate::largeobj::CLASS_OID {
        let mut id = crate::catalog::identify::Identity {
            kind: "large object".into(),
            ..Default::default()
        };
        if lo_exists(c, oid)? {
            id.identity = Some(oid.to_string());
            id.names = Some(vec![oid.to_string()]);
            id.args = Some(Vec::new());
        }
        return Ok(id);
    }
    crate::catalog::identify::identify(&c.exec.catalog(), (class, oid, sub))
}

/// `pg_identify_object`: (type, schema, name, identity).
fn pg_identify_object(a: &[Value], c: &FnCtx) -> Result<Value> {
    let id = identify(a, c)?;
    Ok(Value::Record(Arc::new(vec![
        text(id.kind),
        opt_text(id.schema),
        opt_text(id.name),
        opt_text(id.identity),
    ])))
}

/// `pg_identify_object_as_address`: (type, object_names, object_args).
fn pg_identify_object_as_address(a: &[Value], c: &FnCtx) -> Result<Value> {
    let id = identify(a, c)?;
    Ok(Value::Record(Arc::new(vec![
        text(id.kind),
        text_list(id.names),
        text_list(id.args),
    ])))
}

/// `pg_get_object_address`: (classid, objid, objsubid).
fn pg_get_object_address(a: &[Value], c: &FnCtx) -> Result<Value> {
    let list = |v: &Value| -> Vec<String> {
        match v {
            Value::Array(arr) => arr.items.iter().map(|x| text_arg(x).to_string()).collect(),
            _ => Vec::new(),
        }
    };
    let kind = text_arg(&a[0]).to_string();
    let has_null = |v: &Value| matches!(v, Value::Array(arr) if arr.items.iter().any(Value::is_null));
    if has_null(&a[1]) || has_null(&a[2]) {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "name or argument lists may not contain nulls",
        ));
    }
    if list(&a[1]).is_empty()
        && !matches!(
            kind.as_str(),
            "type" | "domain" | "cast" | "transform" | "domain constraint" | "large object"
        )
        && crate::catalog::identify::known_object_type(&kind)
    {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "name list length must be at least 1",
        ));
    }
    if kind == "large object" {
        let names = list(&a[1]);
        if names.len() != 1 {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                "name list length must be exactly 1",
            ));
        }
        let oid = crate::types::text::parse_value(&names[0], &T::Oid, c.exec.fmt())?
            .as_i64()
            .unwrap_or(0) as u32;
        if !lo_exists(c, oid)? {
            return Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("large object {oid} does not exist"),
            ));
        }
        return Ok(Value::Record(Arc::new(vec![
            Value::Oid(crate::largeobj::CLASS_OID),
            Value::Oid(oid),
            Value::Int4(0),
        ])));
    }
    let (class, oid, sub) = crate::catalog::identify::object_address(
        &c.exec.catalog(),
        &c.exec.search_path,
        &kind,
        &list(&a[1]),
        &list(&a[2]),
    )?;
    Ok(Value::Record(Arc::new(vec![
        Value::Oid(class),
        Value::Oid(oid),
        Value::Int4(sub),
    ])))
}

/// `getObjectDescription`.
fn pg_describe_object(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (class, oid, sub) = (
        oid_arg(&a[0]),
        oid_arg(&a[1]),
        a[2].as_i64().unwrap_or(0) as i32,
    );
    if class == crate::largeobj::CLASS_OID && !lo_exists(c, oid)? {
        return Ok(Value::Null);
    }
    let cat = c.exec.catalog();
    Ok(
        crate::catalog::depend::describe(&cat, &c.exec.search_path, (class, oid, sub))
            .map(text)
            .unwrap_or(Value::Null),
    )
}

/// A SQL-standard body as stored (`RETURN …` / `BEGIN ATOMIC … END`), printed from its
/// statements; their text when they don't print.
fn sql_body(c: &FnCtx, f: &crate::catalog::Function) -> Option<String> {
    let kind = f.sql_body?;
    Some(
        crate::sql::ruleutils::function_body(c.exec, f).unwrap_or_else(|| {
            if kind == 'r' {
                format!(
                    "RETURN {}",
                    f.body.strip_prefix("SELECT ").unwrap_or(&f.body)
                )
            } else {
                let stmts: String = f
                    .body
                    .split(";\n")
                    .filter(|s| !s.is_empty())
                    .map(|s| format!(" {s};\n"))
                    .collect();
                format!("BEGIN ATOMIC\n{stmts}END")
            }
        }),
    )
}

fn pg_get_function_sqlbody(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    Ok(cat
        .functions
        .get(&oid_arg(&a[0]))
        .and_then(|f| sql_body(c, f))
        .map(text)
        .unwrap_or(Value::Null))
}

/// CREATE OR REPLACE FUNCTION text, laid out like Postgres's `pg_get_functiondef`.
/// pg_get_functiondef of the built-in functions (not aggregates), as Postgres prints them.
fn builtin_functiondef(oid: u32) -> Option<&'static str> {
    static DEFS: OnceLock<std::collections::HashMap<u32, String>> = OnceLock::new();
    DEFS.get_or_init(|| {
        let raw = zstd::decode_all(&include_bytes!("data/functiondefs.tsv.zst")[..])
            .expect("built-in function definitions");
        String::from_utf8(raw)
            .expect("built-in function definitions")
            .lines()
            .filter_map(|l| {
                let (oid, def) = l.split_once('\t')?;
                Some((oid.parse().ok()?, crate::catalog::system::copy_field(def)?))
            })
            .collect()
    })
    .get(&oid)
    .map(String::as_str)
}

fn pg_get_functiondef(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = oid_arg(&a[0]);
    let cat = c.exec.catalog();
    let Some(f) = cat.functions.get(&oid) else {
        if oid < crate::catalog::FIRST_USER_OID
            && let Some(def) = builtin_functiondef(oid)
        {
            return Ok(text(def));
        }
        // The rest of the built-ins are aggregates.
        if oid < crate::catalog::FIRST_USER_OID
            && let Some(sig) = crate::catalog::system::builtin_proc_sigs()
                .iter()
                .find(|s| s.oid == oid)
        {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("\"{}\" is an aggregate function", sig.name),
            ));
        }
        return Ok(Value::Null);
    };
    if f.agg.is_some() {
        return Err(Error::new(
            code::WRONG_OBJECT_TYPE,
            format!("\"{}\" is an aggregate function", f.name),
        ));
    }
    let mut s = format!(
        "CREATE OR REPLACE {} {}.{}({})\n",
        if f.procedure { "PROCEDURE" } else { "FUNCTION" },
        quote_ident(cat.schema_name(f.schema)),
        quote_ident(&f.name),
        function_arguments(&cat, f, true, &c.exec.search_path),
    );
    if !f.procedure {
        s.push_str(&format!(
            " RETURNS {}\n",
            function_result(&cat, f, &c.exec.search_path)
        ));
    }
    if !f.trftypes.is_empty() {
        let types: Vec<String> = f
            .trftypes
            .iter()
            .map(|t| format!("FOR TYPE {}", format_type_oid(*t, -1, &cat, &c.exec.search_path).unwrap_or_default()))
            .collect();
        s.push_str(&format!(" TRANSFORM {}\n", types.join(", ")));
    }
    s.push_str(&format!(" LANGUAGE {}\n", quote_ident(&f.language)));

    // As pg_get_functiondef orders them.
    let mut opts = String::new();
    if f.window {
        opts.push_str(" WINDOW");
    }
    match f.volatility {
        'i' => opts.push_str(" IMMUTABLE"),
        's' => opts.push_str(" STABLE"),
        _ => {}
    }
    match f.parallel {
        's' => opts.push_str(" PARALLEL SAFE"),
        'r' => opts.push_str(" PARALLEL RESTRICTED"),
        _ => {}
    }
    if f.strict {
        opts.push_str(" STRICT");
    }
    if f.security_definer {
        opts.push_str(" SECURITY DEFINER");
    }
    if f.leakproof {
        opts.push_str(" LEAKPROOF");
    }
    let default_cost = if matches!(f.language.as_str(), "internal" | "c") {
        1.0
    } else {
        100.0
    };
    if f.procost() != default_cost {
        opts.push_str(&format!(
            " COST {}",
            crate::types::text::format_float4(f.procost(), 0)
        ));
    }
    if f.prorows() > 0.0 && f.prorows() != 1000.0 {
        opts.push_str(&format!(
            " ROWS {}",
            crate::types::text::format_float4(f.prorows(), 0)
        ));
    }
    if f.support != 0 {
        // (Qualified when the search path wouldn't find it.)
        let name = match cat.functions.get(&f.support) {
            Some(g) => cat.function_name(g, &c.exec.search_path),
            None => crate::catalog::system::builtin_proc_sigs()
                .iter()
                .find(|p| p.oid == f.support)
                .map_or_else(String::new, |p| quote_ident(p.name)),
        };
        opts.push_str(&format!(" SUPPORT {name}"));
    }
    if !opts.is_empty() {
        s.push_str(&opts);
        s.push('\n');
    }
    for c in &f.config {
        let (name, value) = c.split_once('=').unwrap_or((c, ""));
        // List parameters show each element as a literal.
        let shown = if crate::session::LIST_QUOTE.contains(&name) {
            crate::session::split_guc_list(value)
                .iter()
                .map(|e| super::string::quote_literal_str(e))
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            super::string::quote_literal_str(value)
        };
        s.push_str(&format!(" SET {} TO {shown}\n", quote_ident(name)));
    }
    if let Some(body) = sql_body(c, f) {
        s.push_str(&body);
        s.push('\n');
        return Ok(text(s));
    }
    let mut dq = String::from(if f.procedure {
        "$procedure"
    } else {
        "$function"
    });
    while f.body.contains(&format!("{dq}$")) {
        dq.push('x');
    }
    dq.push('$');
    // A C function: its library, then its link symbol.
    let lib = f
        .probin
        .as_deref()
        .map(|l| format!("{}, ", super::string::quote_literal_str(l)))
        .unwrap_or_default();
    s.push_str(&format!("AS {lib}{dq}{}{dq}\n", f.body));
    Ok(text(s))
}
/// Events a relation supports: 1 << CMD_UPDATE | 1 << CMD_INSERT | 1 << CMD_DELETE.
fn relation_updatability(c: &FnCtx, oid: u32, triggers: bool) -> Result<(i32, Vec<bool>)> {
    let cat = c.exec.catalog();
    if let Some(t) = cat.tables.get(&oid) {
        // A foreign table asks its wrapper (GetFdwRoutineForRelation), which fails without
        // a handler.
        if t.is_foreign()
            && let Some(e) = cat.fdw_routine_error(t)
            && e.code == code::OBJECT_NOT_IN_PREREQUISITE_STATE
        {
            return Err(e);
        }
        return Ok((28, vec![true; t.columns.len()]));
    }
    Ok(match cat.views.get(&oid) {
        Some(v) => crate::sql::binder::views::view_updatability(c.exec, v, triggers),
        None => (0, Vec::new()),
    })
}
fn pg_relation_is_updatable(a: &[Value], c: &FnCtx) -> Result<Value> {
    let triggers = matches!(a.get(1), Some(Value::Bool(true)));
    Ok(Value::Int4(
        relation_updatability(c, oid_arg(&a[0]), triggers)?.0,
    ))
}
fn pg_column_is_updatable(a: &[Value], c: &FnCtx) -> Result<Value> {
    let triggers = matches!(a.get(2), Some(Value::Bool(true)));
    let (mask, cols) = relation_updatability(c, oid_arg(&a[0]), triggers)?;
    let attnum = int_arg(&a[1]);
    // System columns are never updatable; a table's columns are.
    let col = attnum > 0 && cols.get(attnum as usize - 1).copied().unwrap_or(false);
    Ok(Value::Bool(col && (mask == 28 || mask & 20 == 20)))
}
fn pg_get_triggerdef(a: &[Value], c: &FnCtx) -> Result<Value> {
    let pretty = a.get(1).is_some_and(|v| matches!(v, Value::Bool(true)));
    Ok(trigger_def(c.exec, oid_arg(&a[0]), pretty)
        .map(text)
        .unwrap_or(Value::Null))
}

/// The trigger's relation (a table, or a view's row shape).
pub fn trigger_relation(cat: &Catalog, rel: u32) -> Option<Arc<crate::catalog::Table>> {
    cat.tables
        .get(&rel)
        .cloned()
        .or_else(|| cat.views.get(&rel).map(|v| v.as_table()))
}

/// `EXECUTE FUNCTION f(args)` of a trigger.
pub fn trigger_action(ctx: &ExecCtx, x: &crate::catalog::Trigger) -> String {
    let cat = ctx.catalog();
    let fname = match cat.functions.get(&x.function) {
        Some(f) => {
            let first = cat.effective_path(&ctx.search_path).into_iter().find(|p| {
                cat.functions
                    .values()
                    .any(|g| g.schema == *p && g.name == f.name && g.args.is_empty())
            });
            if first == Some(f.schema) {
                quote_ident(&f.name)
            } else {
                format!(
                    "{}.{}",
                    quote_ident(cat.schema_name(f.schema)),
                    quote_ident(&f.name)
                )
            }
        }
        None => match crate::catalog::Function::builtin_trigger(x.function, "") {
            Some(f) => f.name.clone(),
            None if (1644..=1655).contains(&x.function) => {
                quote_ident(crate::exec::trigger::ri_fn::name(x.function))
            }
            None if x.function == 1250 => "unique_key_recheck".into(),
            None => format!("\"{}\"", x.function),
        },
    };
    let args: Vec<String> = x
        .args
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', "''")))
        .collect();
    format!("EXECUTE FUNCTION {fname}({})", args.join(", "))
}

/// CREATE TRIGGER statement text for a trigger (pg_get_triggerdef).
pub fn trigger_def(ctx: &ExecCtx, oid: u32, pretty: bool) -> Option<String> {
    use crate::catalog::trigger_type as tt;
    let cat = ctx.catalog();
    // A foreign key's internal trigger names the table at its other end.
    let (x, from) = match cat.triggers.get(&oid) {
        Some(x) => (x.clone(), cat.tables.get(&x.constr_rel).cloned()),
        None => {
            let r = crate::exec::trigger::ri_trigger(&cat, oid)?;
            (Arc::new(r.trig), cat.tables.get(&r.other).cloned())
        }
    };
    let x = &x;
    let t = trigger_relation(&cat, x.table)?;
    let mut s = format!(
        "CREATE {}TRIGGER {} ",
        if x.constraint { "CONSTRAINT " } else { "" },
        quote_ident(&x.name)
    );
    s.push_str(if x.tgtype & tt::BEFORE != 0 {
        "BEFORE"
    } else if x.tgtype & tt::INSTEAD != 0 {
        "INSTEAD OF"
    } else {
        "AFTER"
    });
    let mut events = Vec::new();
    for (bit, word) in [
        (tt::INSERT, "INSERT"),
        (tt::DELETE, "DELETE"),
        (tt::UPDATE, "UPDATE"),
        (tt::TRUNCATE, "TRUNCATE"),
    ] {
        if x.tgtype & bit == 0 {
            continue;
        }
        if bit == tt::UPDATE && !x.columns.is_empty() {
            let cols: Vec<String> = x
                .columns
                .iter()
                .map(|c| quote_ident(&t.columns[*c].name))
                .collect();
            events.push(format!("UPDATE OF {}", cols.join(", ")));
        } else {
            events.push(word.to_string());
        }
    }
    s.push(' ');
    s.push_str(&events.join(" OR "));
    let table = if pretty {
        cat.qualified_name(t.schema, &t.name, &ctx.search_path)
    } else {
        format!(
            "{}.{}",
            quote_ident(cat.schema_name(t.schema)),
            quote_ident(&t.name)
        )
    };
    s.push_str(&format!(" ON {table} "));
    if x.constraint {
        if let Some(f) = from {
            s.push_str(&format!(
                "FROM {} ",
                cat.qualified_name(f.schema, &f.name, &ctx.search_path)
            ));
        }
        s.push_str(if x.deferrable {
            "DEFERRABLE "
        } else {
            "NOT DEFERRABLE "
        });
        s.push_str(if x.initdeferred {
            "INITIALLY DEFERRED "
        } else {
            "INITIALLY IMMEDIATE "
        });
    }
    if x.old_table.is_some() || x.new_table.is_some() {
        s.push_str("REFERENCING ");
        if let Some(o) = &x.old_table {
            s.push_str(&format!("OLD TABLE AS {} ", quote_ident(o)));
        }
        if let Some(n) = &x.new_table {
            s.push_str(&format!("NEW TABLE AS {} ", quote_ident(n)));
        }
    }
    s.push_str(if x.tgtype & tt::ROW != 0 {
        "FOR EACH ROW "
    } else {
        "FOR EACH STATEMENT "
    });
    if let Some(w) = &x.when {
        let shown =
            crate::sql::ruleutils::trigger_when(ctx, &t, w, pretty).unwrap_or_else(|| w.clone());
        s.push_str(&format!("WHEN ({shown}) "));
    }
    s.push_str(&trigger_action(ctx, x));
    Some(s)
}
fn pg_get_statisticsobjdef(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(statistics_def(c.exec, oid_arg(&a[0]), false)
        .map(text)
        .unwrap_or(Value::Null))
}
fn pg_get_statisticsobjdef_columns(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(statistics_def(c.exec, oid_arg(&a[0]), true)
        .map(text)
        .unwrap_or(Value::Null))
}
fn pg_get_statisticsobjdef_expressions(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    let Some(st) = cat.statistics.get(&oid_arg(&a[0])).cloned() else {
        return Ok(Value::Null);
    };
    let Some(t) = cat.index_table(st.table) else {
        return Ok(Value::Null);
    };
    if st.exprs.is_empty() {
        return Ok(Value::Null);
    }
    let items = st
        .exprs
        .iter()
        .map(|e| text(index_expr(c.exec, &t, e, Stored::Plain, false).0))
        .collect();
    Ok(Value::Array(Arc::new(ArrayValue::from_vec(T::Text, items))))
}

/// A statistics object's expressions as `pg_get_statisticsobjdef_expressions` shows them
/// (None: it has none).
pub fn statistics_exprs(ctx: &ExecCtx, oid: u32) -> Option<Vec<String>> {
    let cat = ctx.catalog();
    let st = cat.statistics.get(&oid)?.clone();
    let t = cat.index_table(st.table)?;
    if st.exprs.is_empty() {
        return None;
    }
    Some(
        st.exprs
            .iter()
            .map(|e| index_expr(ctx, &t, e, Stored::Plain, false).0)
            .collect(),
    )
}

/// `pg_get_statisticsobjdef`: CREATE STATISTICS schema.name [(kinds)] ON … FROM table;
/// `columns_only` gives just the ON list.
pub fn statistics_def(ctx: &ExecCtx, oid: u32, columns_only: bool) -> Option<String> {
    let cat = ctx.catalog();
    let st = cat.statistics.get(&oid)?.clone();
    let t = cat.index_table(st.table)?;
    let mut items: Vec<String> = st
        .columns
        .iter()
        .map(|&i| quote_ident(&t.columns[i].name))
        .collect();
    for e in &st.exprs {
        let (shown, func) = index_expr(ctx, &t, e, Stored::Plain, true);
        items.push(if func { shown } else { format!("({shown})") });
    }
    if columns_only {
        return Some(items.join(", "));
    }
    let mut out = format!(
        "CREATE STATISTICS {}.{}",
        quote_ident(cat.schema_name(st.schema)),
        quote_ident(&st.name)
    );
    let kinds: Vec<&str> = st
        .stxkind()
        .iter()
        .filter_map(|k| match k {
            'd' => Some("ndistinct"),
            'f' => Some("dependencies"),
            'm' => Some("mcv"),
            _ => None,
        })
        .collect();
    if !kinds.is_empty() && kinds.len() < 3 {
        out.push_str(&format!(" ({})", kinds.join(", ")));
    }
    out.push_str(&format!(
        " ON {} FROM {}",
        items.join(", "),
        cat.qualified_name(t.schema, &t.name, &ctx.search_path)
    ));
    Some(out)
}
fn pg_encoding_to_char(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(text(
        crate::types::encnames::encoding_name(int_arg(&a[0])).unwrap_or(""),
    ))
}
fn pg_char_to_encoding(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int4(
        crate::types::encnames::encoding_id(text_arg(&a[0])).unwrap_or(-1),
    ))
}

/// The relation oid a size function was given (None: null).
fn size_arg(a: &[Value]) -> Option<u32> {
    match a.first()? {
        Value::Oid(o) => Some(*o),
        Value::Int4(o) => Some(*o as u32),
        Value::Int8(o) => Some(*o as u32),
        _ => None,
    }
}

fn size_value(n: Option<i64>) -> Value {
    n.map_or(Value::Null, Value::Int8)
}

/// pg_relation_size(rel [, fork]).
fn relation_size(a: &[Value], c: &FnCtx) -> Result<Value> {
    use crate::exec::sizing::{Fork, relation_size};
    let Some(oid) = size_arg(a) else {
        return Ok(Value::Null);
    };
    let fork = match a.get(1) {
        Some(Value::Text(f)) => Fork::parse(f)?,
        Some(Value::Null) => return Ok(Value::Null),
        _ => Fork::Main,
    };
    Ok(size_value(relation_size(oid, fork, c.exec)?))
}

fn table_size(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Some(oid) = size_arg(a) else {
        return Ok(Value::Null);
    };
    Ok(size_value(crate::exec::sizing::table_size(oid, c.exec)?))
}

fn indexes_size(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Some(oid) = size_arg(a) else {
        return Ok(Value::Null);
    };
    Ok(size_value(crate::exec::sizing::indexes_size(oid, c.exec)?))
}

fn total_relation_size(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Some(oid) = size_arg(a) else {
        return Ok(Value::Null);
    };
    Ok(size_value(crate::exec::sizing::total_size(oid, c.exec)?))
}
fn database_size(a: &[Value], c: &FnCtx) -> Result<Value> {
    let dbs = c.exec.sess.cluster.databases();
    let name = match &a[0] {
        Value::Text(t) => {
            if !dbs.iter().any(|d| *d.1 == **t) && **t != *c.exec.sess.database {
                bail!(UNDEFINED_DATABASE, "database \"{t}\" does not exist");
            }
            t.to_string()
        }
        v => match dbs.iter().find(|d| Some(d.0 as i64) == v.as_i64()) {
            Some(d) => d.1.clone(),
            None if dbs.is_empty() => c.exec.sess.database.clone(),
            None => bail!(
                UNDEFINED_OBJECT,
                "database with OID {} does not exist",
                v.as_i64().unwrap_or(0) as u32
            ),
        },
    };
    Ok(match c.exec.sess.cluster.database_size(&name) {
        Some(s) => Value::Int8(s),
        None => Value::Int8(c.exec.sess.db.store.size_bytes() as i64),
    })
}

/// pg_tablespace_size: pg_default holds every database here; the others (pg_global's
/// shared catalogs live with the cluster's registry, a tablespace made in place holds
/// nothing) are empty directories.
fn tablespace_size(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    let oid = match &a[0] {
        Value::Text(t) => match t.as_ref() {
            "pg_default" => 1663,
            "pg_global" => 1664,
            n => match cat.tablespaces.values().find(|x| x.name == n) {
                Some(x) => x.oid,
                None => bail!(UNDEFINED_OBJECT, "tablespace \"{n}\" does not exist"),
            },
        },
        v => v.as_i64().unwrap_or(0) as u32,
    };
    Ok(match oid {
        1663 => {
            // (A database the cluster can't size is this one's store: as pg_database_size.)
            let total: i64 = c
                .exec
                .sess
                .cluster
                .databases()
                .iter()
                .filter_map(|d| c.exec.sess.cluster.database_size(&d.1))
                .sum();
            Value::Int8(if total == 0 {
                c.exec.sess.db.store.size_bytes() as i64
            } else {
                total
            })
        }
        1664 => Value::Int8(0),
        o if cat.tablespaces.contains_key(&o) => Value::Int8(0),
        o => bail!(UNDEFINED_OBJECT, "tablespace with OID {o} does not exist"),
    })
}
/// pg_size_bytes: "1.5 GB" and the like, in bytes.
fn pg_size_bytes(a: &[Value], _: &FnCtx) -> Result<Value> {
    use crate::types::numeric::Numeric;
    let src = text_arg(&a[0]);
    let invalid = || err!(INVALID_PARAMETER_VALUE, "invalid size: \"{src}\"");
    let b = src.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let start = i;
    if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        i += 1;
    }
    let mut digits = false;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        digits = true;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            digits = true;
        }
    }
    if !digits {
        return Err(invalid());
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'-' || b[j] == b'+') {
            j += 1;
        }
        let d = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > d {
            i = j;
        }
    }
    let mut num = Numeric::parse_pg(&src[start..i])?;
    let unit = src[i..].trim_matches(|c: char| c.is_ascii_whitespace());
    if !unit.is_empty() {
        let bits = match unit.to_ascii_lowercase().as_str() {
            "bytes" | "b" => 0,
            "kb" => 10,
            "mb" => 20,
            "gb" => 30,
            "tb" => 40,
            "pb" => 50,
            _ => {
                return Err(invalid()
                    .detail(format!("Invalid size unit: \"{unit}\"."))
                    .hint("Valid units are \"bytes\", \"B\", \"kB\", \"MB\", \"GB\", \"TB\", and \"PB\"."));
            }
        };
        if bits > 0 {
            num = num.mul(&Numeric::from_i64(1i64 << bits));
        }
    }
    num.to_i64_round()
        .map(Value::Int8)
        .ok_or_else(|| err!(NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range"))
}
fn pg_size_pretty(a: &[Value], _: &FnCtx) -> Result<Value> {
    // size_pretty_units: (name, limit, half-rounded, unit bits).
    const UNITS: [(&str, i128, bool, u32); 6] = [
        ("bytes", 10 * 1024, false, 0),
        ("kB", 20 * 1024 - 1, true, 10),
        ("MB", 20 * 1024 - 1, true, 20),
        ("GB", 20 * 1024 - 1, true, 30),
        ("TB", 20 * 1024 - 1, true, 40),
        ("PB", 20 * 1024 - 1, true, 50),
    ];
    let half_rounded = |x: i128| (x + if x < 0 { -1 } else { 1 }) / 2;
    let shift = |i: usize| {
        UNITS[i + 1].3 - UNITS[i].3 - UNITS[i + 1].2 as u32 + UNITS[i].2 as u32
    };
    let mut size: i128 = match &a[0] {
        // A numeric below the bytes limit shows as it is (fraction and all).
        Value::Numeric(n) => {
            let f = n.to_f64();
            if f.abs() < UNITS[0].1 as f64 {
                return Ok(text(format!("{n} bytes")));
            }
            match n.to_string().split('.').next().and_then(|s| s.parse::<i128>().ok()) {
                Some(i) => i,
                None => f as i128,
            }
        }
        v => v.as_i64().unwrap_or(0) as i128,
    };
    for (i, &(name, limit, round, _)) in UNITS.iter().enumerate() {
        if i + 1 == UNITS.len() || size.abs() < limit {
            if round {
                size = half_rounded(size);
            }
            return Ok(text(format!("{size} {name}")));
        }
        size /= 1i128 << shift(i);
    }
    unreachable!()
}
fn pg_column_size(a: &[Value], c: &FnCtx) -> Result<Value> {
    // A table column's value (the binder adds the column, the table and its row): as
    // stored, compressed or out of line.
    if let Some((stored, ty)) = stored_value(a, c) {
        return Ok(Value::Int4(match stored {
            Some((n, _)) => n as i32,
            None => {
                let fmt = c.exec.fmt();
                let text = |v: &Value, ty: &T| crate::types::text::format_value(v, ty, fmt);
                crate::catalog::relsize::datum_width(&a[0], &ty, &text) as i32
            }
        }));
    }
    // Any other value: a fixed-length type's length, or a varlena with a 4-byte header.
    if let Some(e) = c.args.first() {
        let ty = e.ty();
        let fmt = c.exec.fmt();
        let text = |v: &Value, ty: &T| crate::types::text::format_value(v, ty, fmt);
        // A composite datum: a heap tuple header and the fields.
        if let Value::Record(vals) = &a[0] {
            let types: Option<Vec<T>> = match (e, &ty) {
                (crate::sql::expr::Expr::Row(items, _), _) => {
                    Some(items.iter().map(crate::sql::expr::Expr::ty).collect())
                }
                (_, T::Row(r)) => Some(r.fields.iter().map(|(_, t)| t.clone()).collect()),
                _ => None,
            };
            if let Some(types) = types.filter(|t| t.len() == vals.len()) {
                let n = crate::catalog::relsize::heap_tuple_len(&types, vals, &text);
                return Ok(Value::Int4(n as i32));
            }
        }
        if matches!(ty, T::Unknown | T::Cstring) {
            return Ok(Value::Int4(text(&a[0], &T::Text).len() as i32 + 1));
        }
        let len = crate::catalog::relsize::typlen(&ty);
        if len > 0 {
            return Ok(Value::Int4(len as i32));
        }
        if len == -1 {
            let w = crate::catalog::relsize::datum_width(&a[0], &ty, &text);
            // (datum_width counts a short header when the value fits one.)
            return Ok(Value::Int4(if w <= 127 { w + 3 } else { w } as i32));
        }
    }
    let mut out = Vec::new();
    crate::storage::row::encode_value(&mut out, &a[0]);
    Ok(Value::Int4(out.len() as i32))
}

/// The binder's (value, column, table oid, row) form: how the value is stored, and the
/// column's type.
#[allow(clippy::type_complexity)]
fn stored_value(a: &[Value], c: &FnCtx) -> Option<(Option<(usize, Option<char>)>, T)> {
    let [_, Value::Int4(col), Value::Oid(oid), Value::Record(row)] = a else {
        return None;
    };
    let cat = c.exec.catalog();
    let t = cat.table(*oid)?.clone();
    let col = *col as usize;
    let ty = t.columns.get(col)?.ty.clone();
    Some((crate::exec::sizing::stored_datum(&t, row, col, c.exec), ty))
}

fn pg_column_compression(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(match stored_value(a, c) {
        Some((Some((_, Some(m))), _)) => {
            Value::Text(Arc::from(if m == 'l' { "lz4" } else { "pglz" }))
        }
        // (Not a table's column the binder saw, e.g. a materialized view's: a datum read
        // from a relation is known by the method it is stored with.)
        None if a.len() == 1 => match crate::exec::sizing::read_compression(&a[0]) {
            Some(m) => Value::Text(Arc::from(if m == 'l' { "lz4" } else { "pglz" })),
            None => Value::Null,
        },
        _ => Value::Null,
    })
}
fn num_nulls(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int4(a.iter().filter(|v| v.is_null()).count() as i32))
}
fn num_nonnulls(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int4(a.iter().filter(|v| !v.is_null()).count() as i32))
}
fn pg_current_wal_lsn(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::PgLsn(c.exec.sess.db.store.visible_ts()))
}
fn pg_notify(a: &[Value], c: &FnCtx) -> Result<Value> {
    let s = |v: &Value| match v {
        Value::Null => String::new(),
        v => crate::types::text::format_value(v, &T::Text, c.exec.fmt()),
    };
    let (channel, payload) = (s(&a[0]), s(&a[1]));
    crate::notify::check(&channel, &payload)?;
    c.exec.txn.borrow_mut().notify(&channel, &payload);
    Ok(Value::Text(Arc::from("")))
}
fn pg_available_extensions(_: &[Value], _: &FnCtx) -> Result<super::SrfIter> {
    let rows: Vec<Vec<Value>> = crate::ext::available()
        .into_iter()
        .map(|(n, v, c)| {
            vec![
                Value::Text(Arc::from(n)),
                Value::Text(Arc::from(v)),
                Value::Text(Arc::from(c)),
            ]
        })
        .collect();
    Ok(Box::new(rows.into_iter().map(Ok)))
}
fn pg_notification_queue_usage(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Float8(c.exec.sess.db.notify.queue_usage()))
}
fn pg_listening_channels(_: &[Value], c: &FnCtx) -> Result<super::SrfIter> {
    let rows: Vec<Vec<Value>> = c
        .exec
        .sess
        .db
        .notify
        .channels(c.exec.sess.pid)
        .into_iter()
        .map(|ch| vec![Value::Text(Arc::from(ch))])
        .collect();
    Ok(Box::new(rows.into_iter().map(Ok)))
}
/// Whether a table or index is in a partition tree (partitioned, or a partition).
fn in_partition_tree(cat: &crate::catalog::Catalog, oid: u32) -> bool {
    cat.tables
        .get(&oid)
        .is_some_and(|t| t.partkey.is_some() || t.partbound.is_some())
        || cat.indexes.get(&oid).is_some_and(|ix| {
            ix.parent != 0 || cat.index_table(ix.table).is_some_and(|t| t.partkey.is_some())
        })
}

/// An index's ancestors in its partition tree (its parent's first).
fn index_ancestors(cat: &crate::catalog::Catalog, oid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut cur = cat.indexes.get(&oid).map_or(0, |ix| ix.parent);
    while cur != 0 && !out.contains(&cur) {
        out.push(cur);
        cur = cat.indexes.get(&cur).map_or(0, |ix| ix.parent);
    }
    out
}

fn pg_partition_ancestors(a: &[Value], c: &FnCtx) -> Result<super::SrfIter> {
    let cat = c.exec.catalog();
    let oid = oid_arg(&a[0]);
    if !in_partition_tree(&cat, oid) {
        return Ok(Box::new(std::iter::empty()));
    }
    let mut rows = vec![vec![Value::Oid(oid)]];
    let ancestors = if cat.indexes.contains_key(&oid) {
        index_ancestors(&cat, oid)
    } else {
        cat.partition_ancestors(oid)
    };
    rows.extend(ancestors.into_iter().map(|p| vec![Value::Oid(p)]));
    Ok(Box::new(rows.into_iter().map(Ok)))
}

fn pg_partition_root(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    let oid = oid_arg(&a[0]);
    if !in_partition_tree(&cat, oid) {
        return Ok(Value::Null);
    }
    let ancestors = if cat.indexes.contains_key(&oid) {
        index_ancestors(&cat, oid)
    } else {
        cat.partition_ancestors(oid)
    };
    Ok(Value::Oid(ancestors.last().copied().unwrap_or(oid)))
}

fn pg_partition_tree(a: &[Value], c: &FnCtx) -> Result<super::SrfIter> {
    let cat = c.exec.catalog();
    let oid = oid_arg(&a[0]);
    if !in_partition_tree(&cat, oid) {
        return Ok(Box::new(std::iter::empty()));
    }
    // find_all_inheritors order, each with its depth below the input.
    let mut rows = Vec::new();
    let mut level: std::collections::HashMap<u32, i32> = std::collections::HashMap::new();
    level.insert(oid, 0);
    if cat.indexes.contains_key(&oid) {
        let mut all = vec![oid];
        let mut i = 0;
        while i < all.len() {
            let mut kids: Vec<u32> = (cat.indexes.values())
                .filter(|x| x.parent == all[i])
                .map(|x| x.oid)
                .collect();
            kids.sort_unstable();
            all.extend(kids);
            i += 1;
        }
        for ix in all {
            let Some(x) = cat.indexes.get(&ix) else {
                continue;
            };
            let parent = (x.parent != 0).then_some(x.parent);
            let l = if ix == oid {
                0
            } else {
                parent.and_then(|p| level.get(&p)).map_or(0, |l| l + 1)
            };
            level.insert(ix, l);
            let leaf = cat.index_table(x.table).is_none_or(|t| t.partkey.is_none());
            rows.push(vec![
                Value::Oid(ix),
                parent.map_or(Value::Null, Value::Oid),
                Value::Bool(leaf),
                Value::Int4(l),
            ]);
        }
        return Ok(Box::new(rows.into_iter().map(Ok)));
    }
    for t in cat.inheritors(oid) {
        let Some(tab) = cat.tables.get(&t) else {
            continue;
        };
        // (get_partition_ancestors: none for a partition whose detach is pending.)
        let parent = tab
            .partbound
            .as_ref()
            .filter(|_| !tab.detach_pending)
            .and(tab.inherits.first().copied());
        let l = if t == oid {
            0
        } else {
            parent.and_then(|p| level.get(&p)).map_or(0, |l| l + 1)
        };
        level.insert(t, l);
        rows.push(vec![
            Value::Oid(t),
            parent.map_or(Value::Null, Value::Oid),
            Value::Bool(tab.partkey.is_none()),
            Value::Int4(l),
        ]);
    }
    Ok(Box::new(rows.into_iter().map(Ok)))
}

/// pg_get_partkeydef: `LIST (region)`, `RANGE ((a + 1))`, …
fn pg_get_partkeydef(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    let Some(t) = cat.tables.get(&oid_arg(&a[0])) else {
        return Ok(Value::Null);
    };
    let Some(key) = &t.partkey else {
        return Ok(Value::Null);
    };
    let cols: Vec<String> = key
        .cols
        .iter()
        .map(|k| {
            let mut s = match (k.col, &k.expr) {
                (Some(i), _) => quote_ident(&t.columns[i].name),
                (_, Some(e)) => {
                    let shown = stored_expr(c.exec, t, e, Stored::Plain, false)
                        .unwrap_or_else(|| e.clone());
                    let bound = crate::sql::binder::ast::parse_expr(e).ok().and_then(|n| {
                        crate::sql::ruleutils::bind_stored(c.exec, t, &n, Stored::Plain)
                    });
                    if bound
                        .as_ref()
                        .is_some_and(crate::sql::ruleutils::looks_like_function)
                    {
                        shown
                    } else {
                        format!("({shown})")
                    }
                }
                _ => String::new(),
            };
            // Shown when it isn't the key column's (expression's) own collation.
            let own = match k.col {
                Some(i) => t.columns[i].collation.as_deref().unwrap_or(match &t.columns[i].ty {
                    T::Name => "C",
                    _ => "default",
                }),
                None => "default",
            };
            if let Some(coll) = &k.collation
                && coll != own
            {
                s.push_str(&format!(" COLLATE {}", quote_ident(coll)));
            }
            if let Some(op) = &k.opclass {
                s.push_str(&format!(" {op}"));
            }
            s
        })
        .collect();
    Ok(text(format!(
        "{} ({})",
        key.strategy.name(),
        cols.join(", ")
    )))
}

fn pg_get_partition_constraintdef(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    let oid = oid_arg(&a[0]);
    let (Some(t), Some(sql)) = (
        cat.tables.get(&oid),
        crate::catalog::partition::constraint_sql(&cat, oid),
    ) else {
        return Ok(Value::Null);
    };
    Ok(text(
        stored_expr(c.exec, t, &sql, Stored::Plain, false).unwrap_or(sql),
    ))
}

/// satisfies_hash_partition(parent, modulus, remainder, key values...)
fn satisfies_hash_partition(a: &[Value], c: &FnCtx) -> Result<Value> {
    hash_partition_check(a, c, None)
}

/// satisfies_hash_partition called with VARIADIC: the array's elements (of type `elem`)
/// are the key values, all hashed with the first key column's support function.
pub(crate) fn satisfies_hash_partition_variadic(
    fixed: &[Value],
    items: Option<Vec<Value>>,
    elem: &T,
    c: &FnCtx,
) -> Result<Value> {
    let Some(items) = items else {
        // VARIADIC NULL
        if fixed[..3].iter().any(Value::is_null) {
            return Ok(Value::Bool(false));
        }
        return Ok(Value::Bool(false));
    };
    let mut a = fixed.to_vec();
    a.extend(items);
    hash_partition_check(&a, c, Some(elem))
}

fn hash_partition_check(a: &[Value], c: &FnCtx, variadic: Option<&T>) -> Result<Value> {
    if a[..3].iter().any(Value::is_null) {
        return Ok(Value::Bool(false));
    }
    let cat = c.exec.catalog();
    let parent = oid_arg(&a[0]);
    let name = cat
        .tables
        .get(&parent)
        .map(|t| t.name.clone())
        .or_else(|| cat.views.get(&parent).map(|v| v.name.clone()))
        .or_else(|| cat.sequences.get(&parent).map(|q| q.name.clone()))
        .or_else(|| cat.indexes.get(&parent).map(|i| i.name.clone()))
        .or_else(|| crate::catalog::system::SysTable::by_oid(parent).map(|t| t.name().to_string()));
    let Some(name) = name else {
        return Err(Error::new(
            code::UNDEFINED_TABLE,
            format!("could not open relation with OID {parent}"),
        ));
    };
    let Some(t) = cat
        .tables
        .get(&parent)
        .filter(|t| t.partkey.as_ref().is_some_and(|k| k.strategy == crate::catalog::PartStrategy::Hash))
        .cloned()
    else {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            format!("\"{name}\" is not a hash partitioned table"),
        ));
    };
    let modulus = a[1].as_i64().unwrap_or(0);
    let remainder = a[2].as_i64().unwrap_or(0);
    if modulus <= 0 {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "modulus for hash partition must be an integer value greater than zero",
        ));
    }
    if remainder < 0 {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "remainder for hash partition must be an integer value greater than or equal to zero",
        ));
    }
    if remainder >= modulus {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "remainder for hash partition must be less than modulus",
        ));
    }
    let key = t.partkey.clone().unwrap();
    let types = key.types();
    let vals = &a[3..];
    let mismatch = |n: usize| {
        Error::new(
            code::INVALID_PARAMETER_VALUE,
            format!(
                "number of partitioning columns ({}) does not match number of partition keys provided ({n})",
                key.cols.len()
            ),
        )
    };
    match variadic {
        None => {
            if vals.len() != key.cols.len() {
                return Err(mismatch(vals.len()));
            }
            for (j, ty) in types.iter().enumerate() {
                let at = c.arg_type(j + 3);
                if at.oid() != ty.oid() && !(at.is_string() && ty.is_string()) {
                    return Err(Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!(
                            "column {} of the partition key has type {}, but supplied value is of type {}",
                            j + 1,
                            ty.display(),
                            at.display()
                        ),
                    ));
                }
            }
        }
        Some(elem) => {
            for (j, ty) in types.iter().enumerate() {
                if ty.oid() != elem.oid() {
                    return Err(Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!(
                            "column {} of the partition key has type \"{}\", but supplied value is of type \"{}\"",
                            j + 1,
                            ty.display(),
                            elem.display()
                        ),
                    ));
                }
            }
        }
    }
    if variadic.is_some() && vals.len() != key.cols.len() {
        return Err(mismatch(vals.len()));
    }
    let _g = crate::catalog::partition::under(crate::catalog::partition::key_collations(&cat, &t));
    let mut h = 0u64;
    for (i, v) in vals.iter().enumerate() {
        if v.is_null() {
            continue;
        }
        // The variadic form hashes every value with the first column's function.
        let k = if variadic.is_some() { 0 } else { i };
        let x = match cat.hash_support_fn(&t, k) {
            Some(f) => {
                let rows = crate::plpgsql::call_function(
                    c.exec,
                    &f,
                    vec![v.clone(), Value::Int8(crate::types::hash::PARTITION_SEED as i64)],
                )?;
                rows.first()
                    .and_then(|r| r.first())
                    .and_then(Value::as_i64)
                    .unwrap_or(0) as u64
            }
            None => {
                let mut one = vec![Value::Null; i];
                one.push(v.clone());
                let mut tys = types.clone();
                tys.truncate(i + 1);
                while tys.len() < i + 1 {
                    tys.push(types[k].clone());
                }
                tys[i] = types[k].clone();
                // row_hash of a single key value is that value's hash combined into 0
                // (hash_combine64(0, x) = x + 0x49a0f4dd15e5a8e3).
                crate::catalog::partition::row_hash(&one, &tys)
                    .wrapping_sub(0x49a0_f4dd_15e5_a8e3)
            }
        };
        h = crate::types::hash::combine64(h, x);
    }
    Ok(Value::Bool(h % modulus as u64 == remainder as u64))
}

/// pg_stat_get_progress_info: this backend's running command of the kind asked for (only
/// COPY is reported).
pub(super) fn pg_stat_get_progress_info(a: &[Value], c: &FnCtx) -> Result<super::SrfIter> {
    let kind = match &a[0] {
        Value::Text(t) => t.to_string(),
        _ => return Ok(Box::new(std::iter::empty())),
    };
    let sess = c.exec.sess;
    let Some(p) = sess.copy_progress.borrow().clone().filter(|_| kind.eq_ignore_ascii_case("COPY"))
    else {
        return Ok(Box::new(std::iter::empty()));
    };
    let mut params = [0i64; 20];
    params[0] = p.bytes_processed;
    params[1] = p.bytes_total;
    params[2] = p.tuples_processed;
    params[3] = p.tuples_excluded;
    params[4] = p.command;
    params[5] = p.kind;
    params[6] = p.tuples_skipped;
    let mut row = vec![
        Value::Int4(sess.pid),
        Value::Oid(sess.database_oid()),
        Value::Oid(p.relid),
    ];
    row.extend(params.iter().map(|v| Value::Int8(*v)));
    Ok(Box::new(std::iter::once(Ok(row))))
}

fn pg_get_keywords(_: &[Value], _: &FnCtx) -> Result<super::SrfIter> {
    // PostgreSQL 18.6's kwlist.h: word, category, whether it can be a bare column label.
    static KEYWORDS: &str = include_str!("keywords.tsv");
    let rows = KEYWORDS.lines().filter_map(|l| {
        let mut f = l.split('\t');
        let (word, cat, bare) = (f.next()?, f.next()?, f.next()? == "true");
        let catdesc = match cat {
            "U" => "unreserved",
            "C" => "unreserved (cannot be function or type name)",
            "T" => "reserved (can be function or type name)",
            _ => "reserved",
        };
        let baredesc = if bare { "can be bare label" } else { "requires AS" };
        Some(Ok(vec![
            Value::Text(word.into()),
            Value::Char(cat.as_bytes()[0]),
            Value::Bool(bare),
            Value::Text(catdesc.into()),
            Value::Text(baredesc.into()),
        ]))
    });
    Ok(Box::new(rows))
}

/// The system catalogs' foreign-key relationships (`system_fk_info.h`, generated from the
/// catalog headers' BKI_LOOKUP markings): referencing catalog and columns, referenced catalog
/// and columns, whether the last referencing column is an array, and whether zero is allowed.
#[rustfmt::skip]
static CATALOG_FKS: &[(u32, &[&str], u32, &[&str], bool, bool)] = &[
    (1255, &["pronamespace"], 2615, &["oid"], false, false), // pg_proc => pg_namespace
    (1255, &["proowner"], 1260, &["oid"], false, false), // pg_proc => pg_authid
    (1255, &["prolang"], 2612, &["oid"], false, false), // pg_proc => pg_language
    (1255, &["provariadic"], 1247, &["oid"], false, true), // pg_proc => pg_type
    (1255, &["prosupport"], 1255, &["oid"], false, true), // pg_proc => pg_proc
    (1255, &["prorettype"], 1247, &["oid"], false, false), // pg_proc => pg_type
    (1255, &["proargtypes"], 1247, &["oid"], true, false), // pg_proc => pg_type
    (1255, &["proallargtypes"], 1247, &["oid"], true, false), // pg_proc => pg_type
    (1255, &["protrftypes"], 1247, &["oid"], true, false), // pg_proc => pg_type
    (1247, &["typnamespace"], 2615, &["oid"], false, false), // pg_type => pg_namespace
    (1247, &["typowner"], 1260, &["oid"], false, false), // pg_type => pg_authid
    (1247, &["typrelid"], 1259, &["oid"], false, true), // pg_type => pg_class
    (1247, &["typsubscript"], 1255, &["oid"], false, true), // pg_type => pg_proc
    (1247, &["typelem"], 1247, &["oid"], false, true), // pg_type => pg_type
    (1247, &["typarray"], 1247, &["oid"], false, true), // pg_type => pg_type
    (1247, &["typinput"], 1255, &["oid"], false, false), // pg_type => pg_proc
    (1247, &["typoutput"], 1255, &["oid"], false, false), // pg_type => pg_proc
    (1247, &["typreceive"], 1255, &["oid"], false, true), // pg_type => pg_proc
    (1247, &["typsend"], 1255, &["oid"], false, true), // pg_type => pg_proc
    (1247, &["typmodin"], 1255, &["oid"], false, true), // pg_type => pg_proc
    (1247, &["typmodout"], 1255, &["oid"], false, true), // pg_type => pg_proc
    (1247, &["typanalyze"], 1255, &["oid"], false, true), // pg_type => pg_proc
    (1247, &["typbasetype"], 1247, &["oid"], false, true), // pg_type => pg_type
    (1247, &["typcollation"], 3456, &["oid"], false, true), // pg_type => pg_collation
    (1249, &["attrelid"], 1259, &["oid"], false, false), // pg_attribute => pg_class
    (1249, &["atttypid"], 1247, &["oid"], false, true), // pg_attribute => pg_type
    (1249, &["attcollation"], 3456, &["oid"], false, true), // pg_attribute => pg_collation
    (1259, &["relnamespace"], 2615, &["oid"], false, false), // pg_class => pg_namespace
    (1259, &["reltype"], 1247, &["oid"], false, true), // pg_class => pg_type
    (1259, &["reloftype"], 1247, &["oid"], false, true), // pg_class => pg_type
    (1259, &["relowner"], 1260, &["oid"], false, false), // pg_class => pg_authid
    (1259, &["relam"], 2601, &["oid"], false, true), // pg_class => pg_am
    (1259, &["reltablespace"], 1213, &["oid"], false, true), // pg_class => pg_tablespace
    (1259, &["reltoastrelid"], 1259, &["oid"], false, true), // pg_class => pg_class
    (1259, &["relrewrite"], 1259, &["oid"], false, true), // pg_class => pg_class
    (2604, &["adrelid"], 1259, &["oid"], false, false), // pg_attrdef => pg_class
    (2604, &["adrelid", "adnum"], 1249, &["attrelid", "attnum"], false, false), // pg_attrdef => pg_attribute
    (2606, &["connamespace"], 2615, &["oid"], false, false), // pg_constraint => pg_namespace
    (2606, &["conrelid"], 1259, &["oid"], false, true), // pg_constraint => pg_class
    (2606, &["contypid"], 1247, &["oid"], false, true), // pg_constraint => pg_type
    (2606, &["conindid"], 1259, &["oid"], false, true), // pg_constraint => pg_class
    (2606, &["conparentid"], 2606, &["oid"], false, true), // pg_constraint => pg_constraint
    (2606, &["confrelid"], 1259, &["oid"], false, true), // pg_constraint => pg_class
    (2606, &["conpfeqop"], 2617, &["oid"], true, false), // pg_constraint => pg_operator
    (2606, &["conppeqop"], 2617, &["oid"], true, false), // pg_constraint => pg_operator
    (2606, &["conffeqop"], 2617, &["oid"], true, false), // pg_constraint => pg_operator
    (2606, &["conexclop"], 2617, &["oid"], true, false), // pg_constraint => pg_operator
    (2606, &["conrelid", "conkey"], 1249, &["attrelid", "attnum"], true, true), // pg_constraint => pg_attribute
    (2606, &["confrelid", "confkey"], 1249, &["attrelid", "attnum"], true, false), // pg_constraint => pg_attribute
    (2611, &["inhrelid"], 1259, &["oid"], false, false), // pg_inherits => pg_class
    (2611, &["inhparent"], 1259, &["oid"], false, false), // pg_inherits => pg_class
    (2610, &["indexrelid"], 1259, &["oid"], false, false), // pg_index => pg_class
    (2610, &["indrelid"], 1259, &["oid"], false, false), // pg_index => pg_class
    (2610, &["indcollation"], 3456, &["oid"], true, true), // pg_index => pg_collation
    (2610, &["indclass"], 2616, &["oid"], true, false), // pg_index => pg_opclass
    (2610, &["indrelid", "indkey"], 1249, &["attrelid", "attnum"], true, true), // pg_index => pg_attribute
    (2617, &["oprnamespace"], 2615, &["oid"], false, false), // pg_operator => pg_namespace
    (2617, &["oprowner"], 1260, &["oid"], false, false), // pg_operator => pg_authid
    (2617, &["oprleft"], 1247, &["oid"], false, true), // pg_operator => pg_type
    (2617, &["oprright"], 1247, &["oid"], false, false), // pg_operator => pg_type
    (2617, &["oprresult"], 1247, &["oid"], false, true), // pg_operator => pg_type
    (2617, &["oprcom"], 2617, &["oid"], false, true), // pg_operator => pg_operator
    (2617, &["oprnegate"], 2617, &["oid"], false, true), // pg_operator => pg_operator
    (2617, &["oprcode"], 1255, &["oid"], false, true), // pg_operator => pg_proc
    (2617, &["oprrest"], 1255, &["oid"], false, true), // pg_operator => pg_proc
    (2617, &["oprjoin"], 1255, &["oid"], false, true), // pg_operator => pg_proc
    (2753, &["opfmethod"], 2601, &["oid"], false, false), // pg_opfamily => pg_am
    (2753, &["opfnamespace"], 2615, &["oid"], false, false), // pg_opfamily => pg_namespace
    (2753, &["opfowner"], 1260, &["oid"], false, false), // pg_opfamily => pg_authid
    (2616, &["opcmethod"], 2601, &["oid"], false, false), // pg_opclass => pg_am
    (2616, &["opcnamespace"], 2615, &["oid"], false, false), // pg_opclass => pg_namespace
    (2616, &["opcowner"], 1260, &["oid"], false, false), // pg_opclass => pg_authid
    (2616, &["opcfamily"], 2753, &["oid"], false, false), // pg_opclass => pg_opfamily
    (2616, &["opcintype"], 1247, &["oid"], false, false), // pg_opclass => pg_type
    (2616, &["opckeytype"], 1247, &["oid"], false, true), // pg_opclass => pg_type
    (2601, &["amhandler"], 1255, &["oid"], false, false), // pg_am => pg_proc
    (2602, &["amopfamily"], 2753, &["oid"], false, false), // pg_amop => pg_opfamily
    (2602, &["amoplefttype"], 1247, &["oid"], false, false), // pg_amop => pg_type
    (2602, &["amoprighttype"], 1247, &["oid"], false, false), // pg_amop => pg_type
    (2602, &["amopopr"], 2617, &["oid"], false, false), // pg_amop => pg_operator
    (2602, &["amopmethod"], 2601, &["oid"], false, false), // pg_amop => pg_am
    (2602, &["amopsortfamily"], 2753, &["oid"], false, true), // pg_amop => pg_opfamily
    (2603, &["amprocfamily"], 2753, &["oid"], false, false), // pg_amproc => pg_opfamily
    (2603, &["amproclefttype"], 1247, &["oid"], false, false), // pg_amproc => pg_type
    (2603, &["amprocrighttype"], 1247, &["oid"], false, false), // pg_amproc => pg_type
    (2603, &["amproc"], 1255, &["oid"], false, false), // pg_amproc => pg_proc
    (2612, &["lanowner"], 1260, &["oid"], false, false), // pg_language => pg_authid
    (2612, &["lanplcallfoid"], 1255, &["oid"], false, true), // pg_language => pg_proc
    (2612, &["laninline"], 1255, &["oid"], false, true), // pg_language => pg_proc
    (2612, &["lanvalidator"], 1255, &["oid"], false, true), // pg_language => pg_proc
    (2995, &["lomowner"], 1260, &["oid"], false, false), // pg_largeobject_metadata => pg_authid
    (2613, &["loid"], 2995, &["oid"], false, false), // pg_largeobject => pg_largeobject_metadata
    (2600, &["aggfnoid"], 1255, &["oid"], false, false), // pg_aggregate => pg_proc
    (2600, &["aggtransfn"], 1255, &["oid"], false, false), // pg_aggregate => pg_proc
    (2600, &["aggfinalfn"], 1255, &["oid"], false, true), // pg_aggregate => pg_proc
    (2600, &["aggcombinefn"], 1255, &["oid"], false, true), // pg_aggregate => pg_proc
    (2600, &["aggserialfn"], 1255, &["oid"], false, true), // pg_aggregate => pg_proc
    (2600, &["aggdeserialfn"], 1255, &["oid"], false, true), // pg_aggregate => pg_proc
    (2600, &["aggmtransfn"], 1255, &["oid"], false, true), // pg_aggregate => pg_proc
    (2600, &["aggminvtransfn"], 1255, &["oid"], false, true), // pg_aggregate => pg_proc
    (2600, &["aggmfinalfn"], 1255, &["oid"], false, true), // pg_aggregate => pg_proc
    (2600, &["aggsortop"], 2617, &["oid"], false, true), // pg_aggregate => pg_operator
    (2600, &["aggtranstype"], 1247, &["oid"], false, false), // pg_aggregate => pg_type
    (2600, &["aggmtranstype"], 1247, &["oid"], false, true), // pg_aggregate => pg_type
    (2619, &["starelid"], 1259, &["oid"], false, false), // pg_statistic => pg_class
    (2619, &["staop1"], 2617, &["oid"], false, true), // pg_statistic => pg_operator
    (2619, &["staop2"], 2617, &["oid"], false, true), // pg_statistic => pg_operator
    (2619, &["staop3"], 2617, &["oid"], false, true), // pg_statistic => pg_operator
    (2619, &["staop4"], 2617, &["oid"], false, true), // pg_statistic => pg_operator
    (2619, &["staop5"], 2617, &["oid"], false, true), // pg_statistic => pg_operator
    (2619, &["stacoll1"], 3456, &["oid"], false, true), // pg_statistic => pg_collation
    (2619, &["stacoll2"], 3456, &["oid"], false, true), // pg_statistic => pg_collation
    (2619, &["stacoll3"], 3456, &["oid"], false, true), // pg_statistic => pg_collation
    (2619, &["stacoll4"], 3456, &["oid"], false, true), // pg_statistic => pg_collation
    (2619, &["stacoll5"], 3456, &["oid"], false, true), // pg_statistic => pg_collation
    (2619, &["starelid", "staattnum"], 1249, &["attrelid", "attnum"], false, false), // pg_statistic => pg_attribute
    (3381, &["stxrelid"], 1259, &["oid"], false, false), // pg_statistic_ext => pg_class
    (3381, &["stxnamespace"], 2615, &["oid"], false, false), // pg_statistic_ext => pg_namespace
    (3381, &["stxowner"], 1260, &["oid"], false, false), // pg_statistic_ext => pg_authid
    (3381, &["stxrelid", "stxkeys"], 1249, &["attrelid", "attnum"], true, false), // pg_statistic_ext => pg_attribute
    (3429, &["stxoid"], 3381, &["oid"], false, false), // pg_statistic_ext_data => pg_statistic_ext
    (2618, &["ev_class"], 1259, &["oid"], false, false), // pg_rewrite => pg_class
    (2620, &["tgrelid"], 1259, &["oid"], false, false), // pg_trigger => pg_class
    (2620, &["tgparentid"], 2620, &["oid"], false, true), // pg_trigger => pg_trigger
    (2620, &["tgfoid"], 1255, &["oid"], false, false), // pg_trigger => pg_proc
    (2620, &["tgconstrrelid"], 1259, &["oid"], false, true), // pg_trigger => pg_class
    (2620, &["tgconstrindid"], 1259, &["oid"], false, true), // pg_trigger => pg_class
    (2620, &["tgconstraint"], 2606, &["oid"], false, true), // pg_trigger => pg_constraint
    (2620, &["tgrelid", "tgattr"], 1249, &["attrelid", "attnum"], true, false), // pg_trigger => pg_attribute
    (3466, &["evtowner"], 1260, &["oid"], false, false), // pg_event_trigger => pg_authid
    (3466, &["evtfoid"], 1255, &["oid"], false, false), // pg_event_trigger => pg_proc
    (2609, &["classoid"], 1259, &["oid"], false, false), // pg_description => pg_class
    (2605, &["castsource"], 1247, &["oid"], false, false), // pg_cast => pg_type
    (2605, &["casttarget"], 1247, &["oid"], false, false), // pg_cast => pg_type
    (2605, &["castfunc"], 1255, &["oid"], false, true), // pg_cast => pg_proc
    (3501, &["enumtypid"], 1247, &["oid"], false, false), // pg_enum => pg_type
    (2615, &["nspowner"], 1260, &["oid"], false, false), // pg_namespace => pg_authid
    (2607, &["connamespace"], 2615, &["oid"], false, false), // pg_conversion => pg_namespace
    (2607, &["conowner"], 1260, &["oid"], false, false), // pg_conversion => pg_authid
    (2607, &["conproc"], 1255, &["oid"], false, false), // pg_conversion => pg_proc
    (2608, &["classid"], 1259, &["oid"], false, false), // pg_depend => pg_class
    (2608, &["refclassid"], 1259, &["oid"], false, false), // pg_depend => pg_class
    (1262, &["datdba"], 1260, &["oid"], false, false), // pg_database => pg_authid
    (1262, &["dattablespace"], 1213, &["oid"], false, false), // pg_database => pg_tablespace
    (2964, &["setdatabase"], 1262, &["oid"], false, true), // pg_db_role_setting => pg_database
    (2964, &["setrole"], 1260, &["oid"], false, true), // pg_db_role_setting => pg_authid
    (1213, &["spcowner"], 1260, &["oid"], false, false), // pg_tablespace => pg_authid
    (1261, &["roleid"], 1260, &["oid"], false, false), // pg_auth_members => pg_authid
    (1261, &["member"], 1260, &["oid"], false, false), // pg_auth_members => pg_authid
    (1261, &["grantor"], 1260, &["oid"], false, false), // pg_auth_members => pg_authid
    (1214, &["dbid"], 1262, &["oid"], false, true), // pg_shdepend => pg_database
    (1214, &["classid"], 1259, &["oid"], false, false), // pg_shdepend => pg_class
    (1214, &["refclassid"], 1259, &["oid"], false, false), // pg_shdepend => pg_class
    (2396, &["classoid"], 1259, &["oid"], false, false), // pg_shdescription => pg_class
    (3602, &["cfgnamespace"], 2615, &["oid"], false, false), // pg_ts_config => pg_namespace
    (3602, &["cfgowner"], 1260, &["oid"], false, false), // pg_ts_config => pg_authid
    (3602, &["cfgparser"], 3601, &["oid"], false, false), // pg_ts_config => pg_ts_parser
    (3603, &["mapcfg"], 3602, &["oid"], false, false), // pg_ts_config_map => pg_ts_config
    (3603, &["mapdict"], 3600, &["oid"], false, false), // pg_ts_config_map => pg_ts_dict
    (3600, &["dictnamespace"], 2615, &["oid"], false, false), // pg_ts_dict => pg_namespace
    (3600, &["dictowner"], 1260, &["oid"], false, false), // pg_ts_dict => pg_authid
    (3600, &["dicttemplate"], 3764, &["oid"], false, false), // pg_ts_dict => pg_ts_template
    (3601, &["prsnamespace"], 2615, &["oid"], false, false), // pg_ts_parser => pg_namespace
    (3601, &["prsstart"], 1255, &["oid"], false, false), // pg_ts_parser => pg_proc
    (3601, &["prstoken"], 1255, &["oid"], false, false), // pg_ts_parser => pg_proc
    (3601, &["prsend"], 1255, &["oid"], false, false), // pg_ts_parser => pg_proc
    (3601, &["prsheadline"], 1255, &["oid"], false, true), // pg_ts_parser => pg_proc
    (3601, &["prslextype"], 1255, &["oid"], false, false), // pg_ts_parser => pg_proc
    (3764, &["tmplnamespace"], 2615, &["oid"], false, false), // pg_ts_template => pg_namespace
    (3764, &["tmplinit"], 1255, &["oid"], false, true), // pg_ts_template => pg_proc
    (3764, &["tmpllexize"], 1255, &["oid"], false, false), // pg_ts_template => pg_proc
    (3079, &["extowner"], 1260, &["oid"], false, false), // pg_extension => pg_authid
    (3079, &["extnamespace"], 2615, &["oid"], false, false), // pg_extension => pg_namespace
    (3079, &["extconfig"], 1259, &["oid"], true, false), // pg_extension => pg_class
    (2328, &["fdwowner"], 1260, &["oid"], false, false), // pg_foreign_data_wrapper => pg_authid
    (2328, &["fdwhandler"], 1255, &["oid"], false, true), // pg_foreign_data_wrapper => pg_proc
    (2328, &["fdwvalidator"], 1255, &["oid"], false, true), // pg_foreign_data_wrapper => pg_proc
    (1417, &["srvowner"], 1260, &["oid"], false, false), // pg_foreign_server => pg_authid
    (1417, &["srvfdw"], 2328, &["oid"], false, false), // pg_foreign_server => pg_foreign_data_wrapper
    (1418, &["umuser"], 1260, &["oid"], false, true), // pg_user_mapping => pg_authid
    (1418, &["umserver"], 1417, &["oid"], false, false), // pg_user_mapping => pg_foreign_server
    (3118, &["ftrelid"], 1259, &["oid"], false, false), // pg_foreign_table => pg_class
    (3118, &["ftserver"], 1417, &["oid"], false, false), // pg_foreign_table => pg_foreign_server
    (3256, &["polrelid"], 1259, &["oid"], false, false), // pg_policy => pg_class
    (3256, &["polroles"], 1260, &["oid"], true, true), // pg_policy => pg_authid
    (826, &["defaclrole"], 1260, &["oid"], false, false), // pg_default_acl => pg_authid
    (826, &["defaclnamespace"], 2615, &["oid"], false, true), // pg_default_acl => pg_namespace
    (3394, &["classoid"], 1259, &["oid"], false, false), // pg_init_privs => pg_class
    (3596, &["classoid"], 1259, &["oid"], false, false), // pg_seclabel => pg_class
    (3592, &["classoid"], 1259, &["oid"], false, false), // pg_shseclabel => pg_class
    (3456, &["collnamespace"], 2615, &["oid"], false, false), // pg_collation => pg_namespace
    (3456, &["collowner"], 1260, &["oid"], false, false), // pg_collation => pg_authid
    (3350, &["partrelid"], 1259, &["oid"], false, false), // pg_partitioned_table => pg_class
    (3350, &["partdefid"], 1259, &["oid"], false, true), // pg_partitioned_table => pg_class
    (3350, &["partclass"], 2616, &["oid"], true, false), // pg_partitioned_table => pg_opclass
    (3350, &["partcollation"], 3456, &["oid"], true, true), // pg_partitioned_table => pg_collation
    (3350, &["partrelid", "partattrs"], 1249, &["attrelid", "attnum"], true, true), // pg_partitioned_table => pg_attribute
    (3541, &["rngtypid"], 1247, &["oid"], false, false), // pg_range => pg_type
    (3541, &["rngsubtype"], 1247, &["oid"], false, false), // pg_range => pg_type
    (3541, &["rngmultitypid"], 1247, &["oid"], false, false), // pg_range => pg_type
    (3541, &["rngcollation"], 3456, &["oid"], false, true), // pg_range => pg_collation
    (3541, &["rngsubopc"], 2616, &["oid"], false, false), // pg_range => pg_opclass
    (3541, &["rngcanonical"], 1255, &["oid"], false, true), // pg_range => pg_proc
    (3541, &["rngsubdiff"], 1255, &["oid"], false, true), // pg_range => pg_proc
    (3576, &["trftype"], 1247, &["oid"], false, false), // pg_transform => pg_type
    (3576, &["trflang"], 2612, &["oid"], false, false), // pg_transform => pg_language
    (3576, &["trffromsql"], 1255, &["oid"], false, true), // pg_transform => pg_proc
    (3576, &["trftosql"], 1255, &["oid"], false, true), // pg_transform => pg_proc
    (2224, &["seqrelid"], 1259, &["oid"], false, false), // pg_sequence => pg_class
    (2224, &["seqtypid"], 1247, &["oid"], false, false), // pg_sequence => pg_type
    (6104, &["pubowner"], 1260, &["oid"], false, false), // pg_publication => pg_authid
    (6237, &["pnpubid"], 6104, &["oid"], false, false), // pg_publication_namespace => pg_publication
    (6237, &["pnnspid"], 2615, &["oid"], false, false), // pg_publication_namespace => pg_namespace
    (6106, &["prpubid"], 6104, &["oid"], false, false), // pg_publication_rel => pg_publication
    (6106, &["prrelid"], 1259, &["oid"], false, false), // pg_publication_rel => pg_class
    (6100, &["subdbid"], 1262, &["oid"], false, false), // pg_subscription => pg_database
    (6100, &["subowner"], 1260, &["oid"], false, false), // pg_subscription => pg_authid
    (6102, &["srsubid"], 6100, &["oid"], false, false), // pg_subscription_rel => pg_subscription
    (6102, &["srrelid"], 1259, &["oid"], false, false), // pg_subscription_rel => pg_class
];

fn pg_get_catalog_foreign_keys(_: &[Value], _: &FnCtx) -> Result<super::SrfIter> {
    let cols = |c: &[&str]| {
        Value::Array(Arc::new(ArrayValue::from_vec(
            T::Text,
            c.iter().map(|n| text((*n).to_string())).collect(),
        )))
    };
    Ok(Box::new(CATALOG_FKS.iter().map(
        move |&(fk, fkcols, pk, pkcols, is_array, is_opt)| {
            Ok(vec![
                Value::Oid(fk),
                cols(fkcols),
                Value::Oid(pk),
                cols(pkcols),
                Value::Bool(is_array),
                Value::Bool(is_opt),
            ])
        },
    )))
}
fn pg_relation_filenode(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    Ok(cat
        .storage_location(oid_arg(&a[0]))
        .map_or(Value::Null, |(n, ..)| Value::Oid(n)))
}
fn pg_relation_filepath(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    let Some((node, shared, backend)) = cat.storage_location(oid_arg(&a[0])) else {
        return Ok(Value::Null);
    };
    Ok(text(if shared {
        format!("global/{node}")
    } else {
        let db = c.exec.sess.database_oid();
        match backend {
            Some(b) => format!("base/{db}/t{b}_{node}"),
            None => format!("base/{db}/{node}"),
        }
    }))
}
/// pg_filenode_relation (RelidByRelfilenumber): the relation of this database (or a shared
/// catalog, for pg_global) whose file is `filenode` in tablespace `spc` (0: the database's);
/// temporary relations aren't found.
fn pg_filenode_relation(a: &[Value], c: &FnCtx) -> Result<Value> {
    let spc = oid_arg(&a[0]);
    let node = oid_arg(&a[1]);
    if node == 0 || !matches!(spc, 0 | 1663 | 1664) {
        return Ok(Value::Null);
    }
    let shared = spc == 1664;
    let cat = c.exec.catalog();
    let sys = crate::catalog::static_rows::rows(crate::catalog::system::SysTable::PgClass);
    let found = sys
        .iter()
        .map(|r| crate::catalog::static_rows::oid_at(r, 0))
        .chain(cat.storage_oids())
        .find(|o| {
            cat.storage_location(*o)
                .is_some_and(|(n, sh, b)| n == node && sh == shared && b.is_none())
        });
    Ok(found.map_or(Value::Null, Value::Oid))
}
/// pg_tablespace_location: a tablespace's directory; one made in place lies in
/// `pg_tblspc/<oid>`, the built-in ones in the data directory ('').
fn pg_tablespace_location(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = oid_arg(&a[0]);
    let cat = c.exec.catalog();
    Ok(text(match cat.tablespaces.get(&oid) {
        Some(t) if t.location.is_empty() => format!("pg_tblspc/{oid}"),
        Some(t) => t.location.clone(),
        None => String::new(),
    }))
}
/// Run `value`'s input as type `type_name` (parseTypeString + InputFunctionCallSafe): Ok(None)
/// when it is valid, Ok(Some(error)) for a soft input error, Err for what Postgres can't
/// trap (a bad type name, an error in a domain's CHECK expression).
fn input_soft_error(value: &str, type_name: &str, c: &FnCtx) -> Result<Option<Error>> {
    use pg_query::NodeEnum;
    use pg_query::protobuf as pb;
    let mut tn = crate::sql::binder::parse_type_string(type_name)?;
    // The type must exist (a hard error otherwise).
    let cat = c.exec.catalog();
    let (ty, _) = crate::sql::binder::resolve_type_name(&cat, &c.exec.search_path, &tn)
        .map_err(|mut e| {
            e.position = None;
            e
        })?;
    // A length (char(n), varchar(n), bit(n)) is checked as input does, not as an explicit
    // cast (which truncates): read without it, then apply it.
    let elem = match &ty {
        T::Array(e) => &**e,
        t => t,
    };
    let length = matches!(elem, T::Varchar(_) | T::Bpchar(_) | T::Bit(_) | T::Varbit(_))
        && !tn.typmods.is_empty();
    if length {
        // The unbounded type (a bare `char` / `bit` would mean length 1).
        let base = match elem {
            T::Bpchar(_) => "bpchar",
            T::Varchar(_) => "varchar",
            T::Bit(_) => "bit",
            _ => "varbit",
        };
        tn.typmods.clear();
        tn.names = vec![pb::Node {
            node: Some(NodeEnum::String(pb::String {
                sval: base.to_string(),
            })),
        }];
    }
    // value::type, as its input function (and a domain's constraints) runs it.
    let cast = pb::TypeCast {
        arg: Some(Box::new(pb::Node {
            node: Some(NodeEnum::AConst(pb::AConst {
                isnull: false,
                val: Some(pb::a_const::Val::Sval(pb::String {
                    sval: value.to_string(),
                })),
                location: -1,
            })),
        })),
        type_name: Some(tn),
        location: -1,
    };
    let q = pb::SelectStmt {
        target_list: vec![pb::Node {
            node: Some(NodeEnum::ResTarget(Box::new(pb::ResTarget {
                name: String::new(),
                indirection: Vec::new(),
                val: Some(Box::new(pb::Node {
                    node: Some(NodeEnum::TypeCast(Box::new(cast))),
                })),
                location: -1,
            }))),
        }],
        ..Default::default()
    };
    let node = pb::Node {
        node: Some(NodeEnum::SelectStmt(Box::new(q))),
    };
    let child = c.exec.child(&[]);
    let r = (|| -> Result<()> {
        let mut b = crate::sql::binder::Binder::new(&child, Vec::new());
        let bq = b.bind_statement(&node)?;
        let rows = crate::exec::collect(&bq.plan, &child, None)?;
        if length && let Some(v) = rows.into_iter().next().and_then(|r| r.into_iter().next()) {
            match (v, &ty) {
                (Value::Array(a), T::Array(e)) => {
                    for x in a.items.iter().filter(|x| !x.is_null()) {
                        crate::types::cast::apply_typmod(x.clone(), e, false)?;
                    }
                }
                (v, t) if !v.is_null() => {
                    crate::types::cast::apply_typmod(v, t, false)?;
                }
                _ => {}
            }
        }
        Ok(())
    })();
    match r {
        Ok(()) => Ok(None),
        Err(e) => {
            // Input functions report their errors softly; the rest still throw.
            let reg = matches!(
                ty,
                T::Regtype
                    | T::Regclass
                    | T::Regproc
                    | T::Regprocedure
                    | T::Regoper
                    | T::Regoperator
                    | T::Regnamespace
                    | T::Regrole
                    | T::Regcollation
            );
            let soft = match e.code {
                code::DIVISION_BY_ZERO => false,
                code::SYNTAX_ERROR => !reg,
                c if matches!(ty, T::Regtype) => c == code::UNDEFINED_OBJECT,
                c => {
                    c.starts_with("22")
                        || matches!(
                            c,
                            code::CHECK_VIOLATION
                                | code::NOT_NULL_VIOLATION
                                | code::UNDEFINED_TABLE
                                | code::UNDEFINED_OBJECT
                                | code::UNDEFINED_FUNCTION
                                | code::AMBIGUOUS_FUNCTION
                                | code::INVALID_SCHEMA_NAME
                        )
                }
            };
            if soft { Ok(Some(e)) } else { Err(e) }
        }
    }
}
fn pg_input_is_valid(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(
        input_soft_error(text_arg(&a[0]), text_arg(&a[1]), c)?.is_none(),
    ))
}
/// pg_input_error_info(value, type): (message, detail, hint, sql_error_code) of the soft
/// error the input would raise; all NULL when it is valid.
fn pg_input_error_info(a: &[Value], c: &FnCtx) -> Result<Value> {
    let e = input_soft_error(text_arg(&a[0]), text_arg(&a[1]), c)?;
    let opt = |s: Option<&str>| s.map_or(Value::Null, text);
    Ok(Value::Record(Arc::new(match e {
        None => vec![Value::Null; 4],
        Some(e) => vec![
            text(e.message.as_str()),
            opt(e.detail.as_deref()),
            opt(e.hint.as_deref()),
            text(e.code),
        ],
    })))
}
fn gen_random_bytes(a: &[Value], c: &FnCtx) -> Result<Value> {
    let n = int_arg(&a[0]);
    if !(1..=1024).contains(&n) {
        bail!(INVALID_PARAMETER_VALUE, "Length not in range");
    }
    let mut b = vec![0u8; n as usize];
    c.exec.random_bytes(&mut b);
    Ok(Value::bytea(b))
}
fn current_query(_: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Null)
}
/// check_safe_enum_use: a value this (uncommitted) transaction added to an enum type that
/// existed before it can't be used yet. Values of a type created in the transaction, and
/// renamed values, are fine.
pub fn check_safe_enum(oid: u32, label: &str, type_name: &str) -> Result<()> {
    let unsafe_use = crate::session::current::with_current(|c| {
        let Ok(t) = c.txn.try_borrow() else {
            return false;
        };
        let Some(mine) = t.changed_catalog() else {
            return false;
        };
        let committed = t.db.catalog();
        let (Some(now), Some(before)) = (mine.enums.get(&oid), committed.enums.get(&oid)) else {
            return false;
        };
        let Some(i) = now.labels.iter().position(|l| l == label) else {
            return false;
        };
        match now.label_oids.get(i) {
            Some(lo) if !before.label_oids.is_empty() => !before.label_oids.contains(lo),
            _ => !before.labels.iter().any(|l| l == label),
        }
    });
    if unsafe_use == Some(true) {
        return Err(Error::new(
            code::UNSAFE_NEW_ENUM_VALUE_USAGE,
            format!("unsafe use of new value \"{label}\" of enum type {type_name}"),
        )
        .hint("New enum values must be committed before they can be used."));
    }
    Ok(())
}

/// currtid2(relname, tid) (currtid_byrelname): the latest version of the row at `tid` in a
/// relation with storage, or through a view's `ctid` column. Rows don't move here once
/// written, so a valid tid is its own latest version.
fn currtid2(a: &[Value], c: &FnCtx) -> Result<Value> {
    let name = text_arg(&a[0]);
    let Value::Tid(blk, off) = a[1] else {
        return Ok(Value::Null);
    };
    let parts = parse_qualified(name)?;
    let cat = c.exec.catalog();
    let (schema, rel) = match parts.as_slice() {
        [r] => (None, r.as_str()),
        [s, r] => (Some(s.as_str()), r.as_str()),
        _ => bail!(
            SYNTAX_ERROR,
            "improper relation name (too many dotted names): {name}"
        ),
    };
    let Some(r) = cat.find(schema, rel, &c.exec.search_path) else {
        bail!(UNDEFINED_TABLE, "relation \"{name}\" does not exist");
    };
    currtid_rel(&cat, &r, (blk, off), c)
}

/// currtid_internal.
fn currtid_rel(
    cat: &Catalog,
    r: &Relation,
    (blk, off): (u32, u16),
    c: &FnCtx,
) -> Result<Value> {
    let qualified = |schema: u32, name: &str| format!("{}.{name}", cat.schema_name(schema));
    match r {
        Relation::Index(ix) => {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("cannot open relation \"{}\"", ix.name),
            )
            .detail("This operation is not supported for indexes."));
        }
        Relation::View(v) if !v.materialized => return currtid_for_view(cat, v, (blk, off), c),
        Relation::Table(t) if t.partkey.is_some() || t.foreign.is_some() => {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!(
                    "cannot look at latest visible tid for relation \"{}\"",
                    qualified(t.schema, &t.name)
                ),
            ));
        }
        _ => {}
    }
    let name = match r {
        Relation::Table(t) => t.name.clone(),
        Relation::View(v) => v.name.clone(),
        Relation::Sequence(sq) => sq.name.clone(),
        Relation::Index(ix) => ix.name.clone(),
    };
    let blocks = crate::exec::sizing::fork_blocks(r.oid(), crate::exec::sizing::Fork::Main, c.exec)?;
    // heapam_tuple_tid_valid: a valid item pointer in a block the relation has.
    if off == 0 || blocks.is_some_and(|n| blk as u64 >= n) {
        bail!(
            INVALID_PARAMETER_VALUE,
            "tid ({blk}, {off}) is not valid for relation \"{name}\""
        );
    }
    Ok(Value::Tid(blk, off))
}

/// currtid_for_view: through the view's `ctid` column, when its query takes it straight
/// from a table's ctid.
fn currtid_for_view(
    cat: &Catalog,
    v: &crate::catalog::ViewDef,
    tid: (u32, u16),
    c: &FnCtx,
) -> Result<Value> {
    use pg_query::NodeEnum as N;
    let Some(idx) = v.columns.iter().position(|(n, _)| n == "ctid") else {
        bail!(FEATURE_NOT_SUPPORTED, "currtid cannot handle views with no CTID");
    };
    if v.columns[idx].1 != T::Tid {
        bail!(FEATURE_NOT_SUPPORTED, "ctid isn't of type TID");
    }
    let cannot = || crate::error::internal("currtid cannot handle this view");
    let stmts = crate::sql::parse::parse(&v.query)?;
    let Some(N::SelectStmt(sel)) = stmts
        .first()
        .and_then(|s| s.stmt.as_ref())
        .and_then(|n| n.node.as_ref())
    else {
        return Err(cannot());
    };
    let col = sel.target_list.get(idx).and_then(|t| match t.node.as_ref() {
        Some(N::ResTarget(rt)) => match rt.val.as_ref().and_then(|v| v.node.as_ref()) {
            Some(N::ColumnRef(cr)) => Some(
                cr.fields
                    .iter()
                    .filter_map(|f| match f.node.as_ref() {
                        Some(N::String(s)) => Some(s.sval.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        },
        _ => None,
    });
    let Some(col) = col.filter(|c| c.last().is_some_and(|l| l == "ctid")) else {
        return Err(cannot());
    };
    let tables: Vec<&pg_query::protobuf::RangeVar> = sel
        .from_clause
        .iter()
        .filter_map(|f| match f.node.as_ref() {
            Some(N::RangeVar(rv)) => Some(rv),
            _ => None,
        })
        .collect();
    let rv = match col.as_slice() {
        [_] if tables.len() == 1 => tables[0],
        [q, _] => tables
            .iter()
            .copied()
            .find(|rv| rv.alias.as_ref().map_or(&rv.relname, |a| &a.aliasname) == q)
            .ok_or_else(cannot)?,
        _ => return Err(cannot()),
    };
    let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
    let r = cat
        .find(schema, &rv.relname, &c.exec.search_path)
        .ok_or_else(cannot)?;
    currtid_rel(cat, &r, tid, c)
}

fn enum_range(a: &[Value], c: &FnCtx) -> Result<Value> {
    let ty = c.arg_type(0);
    let T::Enum(oid, ..) = &ty else {
        bail!(WRONG_OBJECT_TYPE, "enum_range requires an enum")
    };
    let cat = c.exec.catalog();
    let Some(e) = cat.enums.get(oid) else {
        return Ok(Value::Null);
    };
    let labels = &e.labels;
    let (lo, hi) = if a.len() == 2 {
        let pos = |v: &Value| v.as_str().and_then(|s| labels.iter().position(|l| l == s));
        (
            pos(&a[0]).unwrap_or(0),
            pos(&a[1]).unwrap_or(labels.len().saturating_sub(1)),
        )
    } else {
        (0, labels.len().saturating_sub(1))
    };
    let items = if lo <= hi && !labels.is_empty() {
        let name = ty.display();
        for l in &labels[lo..=hi] {
            check_safe_enum(*oid, l, &name)?;
        }
        labels[lo..=hi].iter().map(|l| text(l.as_str())).collect()
    } else {
        Vec::new()
    };
    Ok(Value::Array(Arc::new(ArrayValue::from_vec(
        ty.clone(),
        items,
    ))))
}
fn enum_first(_: &[Value], c: &FnCtx) -> Result<Value> {
    let ty = c.arg_type(0);
    let T::Enum(oid, ..) = &ty else {
        return Ok(Value::Null);
    };
    let label = c.exec.catalog().enums.get(oid).and_then(|e| e.labels.first().cloned());
    if let Some(l) = &label {
        check_safe_enum(*oid, l, &ty.display())?;
    }
    Ok(label.map(text).unwrap_or(Value::Null))
}
fn enum_last(_: &[Value], c: &FnCtx) -> Result<Value> {
    let ty = c.arg_type(0);
    let T::Enum(oid, ..) = &ty else {
        return Ok(Value::Null);
    };
    let label = c.exec.catalog().enums.get(oid).and_then(|e| e.labels.last().cloned());
    if let Some(l) = &label {
        check_safe_enum(*oid, l, &ty.display())?;
    }
    Ok(label.map(text).unwrap_or(Value::Null))
}

/// Ordering of two enum labels by declaration order.
fn enum_cmp(a: &[Value], c: &FnCtx) -> Result<std::cmp::Ordering> {
    let ty = c.arg_type(0);
    let pos = |v: &Value| -> Option<usize> {
        let T::Enum(oid, ..) = &ty else { return None };
        let cat = c.exec.catalog();
        cat.enums
            .get(oid)
            .and_then(|e| e.labels.iter().position(|l| Some(l.as_str()) == v.as_str()))
    };
    match (pos(&a[0]), pos(&a[1])) {
        (Some(x), Some(y)) => Ok(x.cmp(&y)),
        _ => Ok(a[0].cmp_total(&a[1])),
    }
}

macro_rules! enum_ops {
    ($($name:ident => $test:expr),* $(,)?) => {
        $(fn $name(a: &[Value], c: &FnCtx) -> Result<Value> {
            let o = enum_cmp(a, c)?;
            Ok(Value::Bool($test(o)))
        })*
    };
}
use std::cmp::Ordering;
enum_ops! {
    enum_eq => |o| o == Ordering::Equal,
    enum_ne => |o| o != Ordering::Equal,
    enum_lt => |o| o == Ordering::Less,
    enum_le => |o| o != Ordering::Greater,
    enum_gt => |o| o == Ordering::Greater,
    enum_ge => |o| o != Ordering::Less,
}

pub fn register(r: &mut Registry) {
    use T::*;
    r.func("version", vec![], Text, version).stable();
    r.func("current_database", vec![], Name, current_database)
        .stable();
    for n in ["current_user", "user", "current_role", "getpgusername"] {
        r.func(n, vec![], Name, current_user).stable();
    }
    r.func("session_user", vec![], Name, session_user).stable();
    r.func("system_user", vec![], Text, |_, c| {
        Ok(c.exec.sess.system_user.clone().map_or(Value::Null, text))
    })
    .stable();
    r.func("current_catalog", vec![], Name, current_database)
        .stable();
    r.func("current_schema", vec![], Name, current_schema)
        .stable();
    r.func(
        "current_schemas",
        vec![Bool],
        T::array_of(Name),
        current_schemas,
    )
    .stable();
    r.func("current_setting", vec![Text], Text, current_setting)
        .stable();
    r.func("current_setting", vec![Text, Bool], Text, current_setting)
        .stable();
    r.func(
        "pg_settings_get_flags",
        vec![Text],
        T::array_of(Text),
        pg_settings_get_flags,
    )
    .stable();
    r.func("set_config", vec![Text, Text, Bool], Text, set_config)
        .volatile()
        .non_strict();
    r.from_pg(6315, Some(pg_basetype), None);
    r.from_pg(3078, None, Some(pg_sequence_parameters));
    r.from_pg(6427, None, Some(pg_get_sequence_data));
    r.func("pg_backend_pid", vec![], Int4, pg_backend_pid)
        .stable();
    r.func("inet_server_addr", vec![], Inet, inet_server_addr)
        .stable();
    r.func("inet_client_addr", vec![], Inet, inet_server_addr)
        .stable();
    r.func("inet_server_port", vec![], Int4, inet_server_port)
        .stable();
    r.func("inet_client_port", vec![], Int4, inet_server_port)
        .stable();
    r.func(
        "pg_postmaster_start_time",
        vec![],
        Timestamptz(-1),
        pg_postmaster_start_time,
    )
    .stable();
    r.func(
        "pg_conf_load_time",
        vec![],
        Timestamptz(-1),
        pg_conf_load_time,
    )
    .stable();
    r.func("pg_is_in_recovery", vec![], Bool, pg_is_in_recovery)
        .volatile();
    r.func("pg_sleep", vec![Float8], Void, pg_sleep).volatile();
    r.func("pg_sleep_for", vec![Interval(-1)], Void, pg_sleep_for).volatile();
    r.func("pg_sleep_until", vec![Timestamptz(-1)], Void, pg_sleep_until).volatile();
    r.func("pg_cancel_backend", vec![Int4], Bool, pg_cancel_backend)
        .volatile();
    r.func("pg_blocking_pids", vec![Int4], T::array_of(Int4), pg_blocking_pids)
        .volatile();
    r.func(
        "pg_safe_snapshot_blocking_pids",
        vec![Int4],
        T::array_of(Int4),
        pg_safe_snapshot_blocking_pids,
    )
    .volatile();
    r.func(
        "pg_isolation_test_session_is_blocked",
        vec![Int4, T::array_of(Int4)],
        Bool,
        pg_isolation_test_session_is_blocked,
    )
    .volatile();
    r.func(
        "pg_terminate_backend",
        vec![Int4],
        Bool,
        pg_terminate_backend,
    )
    .volatile();
    r.func(
        "pg_terminate_backend",
        vec![Int4, Int8],
        Bool,
        pg_terminate_backend,
    )
    .volatile();
    r.func("nextval", vec![Regclass], Int8, nextval).volatile();
    r.func("currval", vec![Regclass], Int8, currval).volatile();
    r.func("lastval", vec![], Int8, lastval).volatile();
    r.func("setval", vec![Regclass, Int8], Int8, setval)
        .volatile();
    r.func("setval", vec![Regclass, Int8, Bool], Int8, setval)
        .volatile();
    r.func(
        "pg_get_serial_sequence",
        vec![Text, Text],
        Text,
        pg_get_serial_sequence,
    )
    .stable();
    for (name, f) in [
        ("pg_advisory_lock", pg_advisory_lock as super::FnImpl),
        ("pg_advisory_lock_shared", pg_advisory_lock_shared),
        ("pg_advisory_xact_lock", pg_advisory_xact_lock),
        ("pg_advisory_xact_lock_shared", pg_advisory_xact_lock_shared),
    ] {
        r.func(name, vec![Int8], Void, f).volatile();
        r.func(name, vec![Int4, Int4], Void, f).volatile();
    }
    for (name, f) in [
        (
            "pg_try_advisory_lock",
            pg_try_advisory_lock as super::FnImpl,
        ),
        ("pg_try_advisory_lock_shared", pg_try_advisory_lock_shared),
        ("pg_try_advisory_xact_lock", pg_try_advisory_xact_lock),
        (
            "pg_try_advisory_xact_lock_shared",
            pg_try_advisory_xact_lock_shared,
        ),
        ("pg_advisory_unlock", pg_advisory_unlock),
        ("pg_advisory_unlock_shared", pg_advisory_unlock_shared),
    ] {
        r.func(name, vec![Int8], Bool, f).volatile();
        r.func(name, vec![Int4, Int4], Bool, f).volatile();
    }
    r.func(
        "pg_advisory_unlock_all",
        vec![],
        Void,
        pg_advisory_unlock_all,
    )
    .volatile();
    r.func("pg_typeof", vec![Any], Regtype, pg_typeof)
        .stable()
        .non_strict();
    r.func("pg_collation_for", vec![Any], Text, pg_collation_for)
        .stable()
        .non_strict();
    r.func("format_type", vec![Oid, Int4], Text, format_type)
        .stable()
        .non_strict();
    r.func(
        "pg_describe_object",
        vec![Oid, Oid, Int4],
        Text,
        pg_describe_object,
    )
    .stable();
    r.func("to_regclass", vec![Text], Regclass, to_regclass)
        .stable();
    r.func("to_regtypemod", vec![Text], Int4, to_regtypemod)
        .stable();
    r.func("to_regtype", vec![Text], Regtype, to_regtype)
        .stable();
    r.func("to_regnamespace", vec![Text], Regnamespace, to_regnamespace)
        .stable();
    r.func("to_regproc", vec![Text], Regproc, to_reg_input)
        .stable();
    r.func("to_regprocedure", vec![Text], Regprocedure, to_reg_input)
        .stable();
    r.func("to_regrole", vec![Text], Regrole, to_regrole)
        .stable();
    r.func("to_regoper", vec![Text], T::Regoper, to_reg_input)
        .stable();
    r.func("to_regoperator", vec![Text], T::Regoperator, to_reg_input)
        .stable();
    r.func("to_regcollation", vec![Text], T::Regcollation, to_reg_input)
        .stable();
    for n in ["pg_table_is_visible", "pg_function_is_visible"] {
        r.func(n, vec![Oid], Bool, visible).stable();
    }
    r.func("pg_operator_is_visible", vec![Oid], Bool, operator_visible)
        .stable();
    r.func("pg_opclass_is_visible", vec![Oid], Bool, opclass_visible)
        .stable();
    r.func("pg_opfamily_is_visible", vec![Oid], Bool, opfamily_visible)
        .stable();
    r.func("pg_collation_is_visible", vec![Oid], Bool, collation_visible)
        .stable();
    r.func("pg_conversion_is_visible", vec![Oid], Bool, conversion_visible)
        .stable();
    r.func(
        "pg_statistics_obj_is_visible",
        vec![Oid],
        Bool,
        statistics_visible,
    )
    .stable();
    r.func(
        "pg_collation_actual_version",
        vec![Oid],
        Text,
        collation_actual_version,
    )
    .volatile();
    r.func(
        "pg_database_collation_actual_version",
        vec![Oid],
        Text,
        database_collation_actual_version,
    )
    .volatile();
    r.func("pg_ts_config_is_visible", vec![Oid], Bool, |a, c| {
        ts_visible(TsKind::Config, a, c)
    })
    .stable();
    r.func("pg_ts_dict_is_visible", vec![Oid], Bool, |a, c| {
        ts_visible(TsKind::Dict, a, c)
    })
    .stable();
    r.func("pg_ts_parser_is_visible", vec![Oid], Bool, |a, c| {
        ts_visible(TsKind::Parser, a, c)
    })
    .stable();
    r.func("pg_ts_template_is_visible", vec![Oid], Bool, |a, c| {
        ts_visible(TsKind::Template, a, c)
    })
    .stable();
    r.func("pg_type_is_visible", vec![Oid], Bool, type_visible)
        .stable();
    r.func("obj_description", vec![Oid, Name], Text, obj_description)
        .stable();
    r.func("obj_description", vec![Oid], Text, obj_description)
        .stable();
    r.func("col_description", vec![Oid, Int4], Text, col_description)
        .stable();
    r.func(
        "shobj_description",
        vec![Oid, Name],
        Text,
        shobj_description,
    )
    .stable();
    r.func("pg_get_userbyid", vec![Oid], Name, pg_get_userbyid)
        .stable();
    r.func("pg_get_indexdef", vec![Oid], Text, |a, c| deparsed(pg_get_indexdef(a, c)))
        .stable();
    r.func(
        "pg_get_indexdef",
        vec![Oid, Int4, Bool],
        Text,
        |a, c| deparsed(pg_get_indexdef(a, c)),
    )
    .stable();
    r.func(
        "pg_get_constraintdef",
        vec![Oid],
        Text,
        |a, c| deparsed(pg_get_constraintdef(a, c)),
    )
    .stable();
    r.func(
        "pg_get_constraintdef",
        vec![Oid, Bool],
        Text,
        |a, c| deparsed(pg_get_constraintdef(a, c)),
    )
    .stable();
    r.func("pg_get_ruledef", vec![Oid], Text, |a, c| deparsed(pg_get_ruledef(a, c)))
        .stable();
    r.func("pg_get_ruledef", vec![Oid, Bool], Text, |a, c| {
        deparsed(pg_get_ruledef(a, c))
    })
    .stable();
    r.srf_cols(
        "pg_available_extensions",
        vec![],
        vec![("name", Name), ("default_version", Text), ("comment", Text)],
        pg_available_extensions,
    );
    r.func("pg_get_viewdef", vec![Oid], Text, |a, c| deparsed(pg_get_viewdef(a, c)))
        .stable();
    r.func("pg_get_viewdef", vec![Oid, Bool], Text, |a, c| deparsed(pg_get_viewdef(a, c)))
        .stable();
    r.func("pg_get_viewdef", vec![Text], Text, |a, c| deparsed(pg_get_viewdef(a, c)))
        .stable();
    r.func("pg_get_viewdef", vec![Text, Bool], Text, |a, c| deparsed(pg_get_viewdef(a, c)))
        .stable();
    r.func("pg_get_viewdef", vec![Oid, Int4], Text, |a, c| deparsed(pg_get_viewdef(a, c)))
        .stable();
    r.func("pg_get_expr", vec![PgNodeTree, Oid], Text, |a, c| deparsed(pg_get_expr(a, c)))
        .stable();
    r.func(
        "pg_get_expr",
        vec![PgNodeTree, Oid, Bool],
        Text,
        |a, c| deparsed(pg_get_expr(a, c)),
    )
    .stable();
    r.func(
        "pg_get_function_identity_arguments",
        vec![Oid],
        Text,
        |a, c| deparsed(pg_get_function_identity_arguments(a, c)),
    )
    .stable();
    r.func(
        "pg_get_function_arguments",
        vec![Oid],
        Text,
        |a, c| deparsed(pg_get_function_arguments(a, c)),
    )
    .stable();
    r.from_pg(3808, Some(pg_get_function_arg_default), None);
    r.from_pg(2855, Some(pg_is_other_temp_schema), None);
    r.from_pg(676, Some(pg_indexam_progress_phasename), None);
    r.from_pg(1294, Some(currtid2), None);
    r.func(
        "pg_get_function_result",
        vec![Oid],
        Text,
        |a, c| deparsed(pg_get_function_result(a, c)),
    )
    .stable();
    r.func("pg_get_functiondef", vec![Oid], Text, |a, c| deparsed(pg_get_functiondef(a, c)))
        .stable();
    r.func(
        "pg_get_function_sqlbody",
        vec![Oid],
        Text,
        |a, c| deparsed(pg_get_function_sqlbody(a, c)),
    )
    .stable();
    let row = |cols: &[(&str, T)]| {
        T::row(
            cols.iter()
                .map(|(n, t)| (n.to_string(), t.clone()))
                .collect(),
        )
    };
    r.func(
        "pg_input_error_info",
        vec![Text, Text],
        row(&[
            ("message", Text),
            ("detail", Text),
            ("hint", Text),
            ("sql_error_code", Text),
        ]),
        pg_input_error_info,
    )
    .stable();
    r.func(
        "pg_identify_object",
        vec![Oid, Oid, Int4],
        row(&[
            ("type", Text),
            ("schema", Text),
            ("name", Text),
            ("identity", Text),
        ]),
        pg_identify_object,
    )
    .stable();
    r.func(
        "pg_identify_object_as_address",
        vec![Oid, Oid, Int4],
        row(&[
            ("type", Text),
            ("object_names", T::array_of(Text)),
            ("object_args", T::array_of(Text)),
        ]),
        pg_identify_object_as_address,
    )
    .stable();
    r.func(
        "pg_get_object_address",
        vec![Text, T::array_of(Text), T::array_of(Text)],
        row(&[("classid", Oid), ("objid", Oid), ("objsubid", Int4)]),
        pg_get_object_address,
    )
    .stable();
    r.func("pg_get_triggerdef", vec![Oid], Text, |a, c| deparsed(pg_get_triggerdef(a, c)))
        .stable();
    r.func(
        "pg_relation_is_updatable",
        vec![Oid, Bool],
        Int4,
        pg_relation_is_updatable,
    )
    .stable();
    r.func(
        "pg_column_is_updatable",
        vec![Oid, Int2, Bool],
        Bool,
        pg_column_is_updatable,
    )
    .stable();
    r.func(
        "pg_get_triggerdef",
        vec![Oid, Bool],
        Text,
        |a, c| deparsed(pg_get_triggerdef(a, c)),
    )
    .stable();
    r.func(
        "pg_get_statisticsobjdef",
        vec![Oid],
        Text,
        |a, c| deparsed(pg_get_statisticsobjdef(a, c)),
    )
    .stable();
    r.func(
        "pg_get_statisticsobjdef_columns",
        vec![Oid],
        Text,
        pg_get_statisticsobjdef_columns,
    )
    .stable();
    r.func(
        "pg_get_statisticsobjdef_expressions",
        vec![Oid],
        T::Array(Box::new(Text)),
        |a, c| deparsed(pg_get_statisticsobjdef_expressions(a, c)),
    )
    .stable();
    r.func("pg_encoding_to_char", vec![Int4], Name, pg_encoding_to_char)
        .stable();
    r.func("pg_char_to_encoding", vec![Name], Int4, pg_char_to_encoding)
        .stable();
    let sizes: [(&'static str, super::FnImpl); 4] = [
        ("pg_relation_size", relation_size),
        ("pg_total_relation_size", total_relation_size),
        ("pg_table_size", table_size),
        ("pg_indexes_size", indexes_size),
    ];
    for (n, f) in sizes {
        r.func(n, vec![Regclass], Int8, f).volatile();
    }
    r.func("pg_relation_size", vec![Regclass, Text], Int8, relation_size)
        .volatile();
    r.func("pg_database_size", vec![Name], Int8, database_size)
        .volatile();
    r.func("pg_database_size", vec![Oid], Int8, database_size)
        .volatile();
    r.func("pg_tablespace_size", vec![Name], Int8, tablespace_size)
        .volatile();
    r.func("pg_tablespace_size", vec![Oid], Int8, tablespace_size)
        .volatile();
    r.func("pg_size_bytes", vec![Text], Int8, pg_size_bytes);
    r.func("pg_size_pretty", vec![Int8], Text, pg_size_pretty);
    r.func("pg_size_pretty", vec![Numeric(-1)], Text, pg_size_pretty);
    r.func("pg_column_size", vec![Any], Int4, pg_column_size)
        .stable();
    r.func("pg_column_compression", vec![Any], Text, pg_column_compression)
        .stable();
    r.func("num_nulls", vec![T::array_of(Any)], Int4, num_nulls)
        .variadic()
        .non_strict();
    r.func("num_nonnulls", vec![T::array_of(Any)], Int4, num_nonnulls)
        .variadic()
        .non_strict();
    r.func("pg_current_wal_lsn", vec![], PgLsn, pg_current_wal_lsn)
        .volatile();
    r.func("pg_notify", vec![Text, Text], Void, pg_notify)
        .volatile()
        .non_strict();
    r.srf("pg_listening_channels", vec![], Text, pg_listening_channels);
    // Event trigger functions' views of the command being run.
    r.srf_cols(
        "pg_event_trigger_dropped_objects",
        vec![],
        vec![
            ("classid", Oid),
            ("objid", Oid),
            ("objsubid", Int4),
            ("original", Bool),
            ("normal", Bool),
            ("is_temporary", Bool),
            ("object_type", Text),
            ("schema_name", Text),
            ("object_name", Text),
            ("object_identity", Text),
            ("address_names", T::array_of(Text)),
            ("address_args", T::array_of(Text)),
        ],
        |_, _| {
            let rows = crate::session::ddl::event_trigger::dropped_objects()?;
            Ok(Box::new(rows.into_iter().map(Ok)))
        },
    );
    r.srf_cols(
        "pg_event_trigger_ddl_commands",
        vec![],
        vec![
            ("classid", Oid),
            ("objid", Oid),
            ("objsubid", Int4),
            ("command_tag", Text),
            ("object_type", Text),
            ("schema_name", Text),
            ("object_identity", Text),
            ("in_extension", Bool),
            (
                "command",
                crate::types::BaseType::other_pseudo(32).unwrap_or(Text),
            ),
        ],
        |_, _| {
            let rows = crate::session::ddl::event_trigger::ddl_commands()?;
            Ok(Box::new(rows.into_iter().map(Ok)))
        },
    );
    r.func("pg_event_trigger_table_rewrite_oid", vec![], Oid, |_, _| {
        crate::session::ddl::event_trigger::table_rewrite_info("pg_event_trigger_table_rewrite_oid()")
            .map(|r| Value::Oid(r.0))
    });
    r.func("pg_event_trigger_table_rewrite_reason", vec![], Int4, |_, _| {
        crate::session::ddl::event_trigger::table_rewrite_info(
            "pg_event_trigger_table_rewrite_reason()",
        )
        .map(|r| Value::Int4(r.1))
    });
    r.func(
        "pg_notification_queue_usage",
        vec![],
        Float8,
        pg_notification_queue_usage,
    )
    .volatile();
    r.srf(
        "pg_partition_ancestors",
        vec![Regclass],
        Regclass,
        pg_partition_ancestors,
    )
    .out_name("relid");
    r.srf_cols(
        "pg_partition_tree",
        vec![Regclass],
        vec![
            ("relid", Regclass),
            ("parentrelid", Regclass),
            ("isleaf", Bool),
            ("level", Int4),
        ],
        pg_partition_tree,
    );
    r.func(
        "pg_partition_root",
        vec![Regclass],
        Regclass,
        pg_partition_root,
    );
    r.func("pg_get_partkeydef", vec![Oid], Text, |a, c| deparsed(pg_get_partkeydef(a, c)))
        .stable();
    r.func(
        "pg_get_partition_constraintdef",
        vec![Oid],
        Text,
        |a, c| deparsed(pg_get_partition_constraintdef(a, c)),
    )
    .stable();
    // amvalidate: the access method's check of an operator class (Postgres's own pass).
    r.func("amvalidate", vec![Oid], Bool, |a, c| {
        let oid = oid_arg(&a[0]);
        if c.exec.catalog().opclass_info(oid).is_none() {
            return Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("cache lookup failed for operator class {oid}"),
            ));
        }
        Ok(Value::Bool(super::amvalidate::amvalidate(oid, c)?))
    })
    .stable();
    r.func(
        "satisfies_hash_partition",
        vec![Oid, Int4, Int4, Any],
        Bool,
        satisfies_hash_partition,
    )
    .variadic()
    .non_strict();
    // Permanent user tables can be published (`is_publishable_class`).
    r.func(
        "pg_relation_is_publishable",
        vec![Regclass],
        Bool,
        |a, c| {
            let cat = c.exec.catalog();
            Ok(Value::Bool(
                cat.tables
                    .get(&oid_arg(&a[0]))
                    .is_some_and(|t| crate::catalog::pubsub::publishable(t)),
            ))
        },
    );
    r.srf_cols(
        "pg_get_keywords",
        vec![],
        vec![
            ("word", Text),
            ("catcode", Char),
            ("barelabel", Bool),
            ("catdesc", Text),
            ("baredesc", Text),
        ],
        pg_get_keywords,
    );
    r.srf_cols(
        "pg_get_catalog_foreign_keys",
        vec![],
        vec![
            ("fktable", Regclass),
            ("fkcols", T::Array(Box::new(Text))),
            ("pktable", Regclass),
            ("pkcols", T::Array(Box::new(Text))),
            ("is_array", Bool),
            ("is_opt", Bool),
        ],
        pg_get_catalog_foreign_keys,
    )
    .stable();
    r.func(
        "pg_relation_filenode",
        vec![Regclass],
        Oid,
        pg_relation_filenode,
    )
    .stable();
    r.func("pg_my_temp_schema", vec![], Oid, |_, c| {
        let name = c.exec.sess.temp_schema();
        let cat = c.exec.catalog();
        Ok(Value::Oid(cat.schema_by_name(&name).map_or(0, |s| s.oid)))
    })
    .stable();
    r.func(
        "pg_relation_filepath",
        vec![Regclass],
        Text,
        pg_relation_filepath,
    )
    .stable();
    r.func(
        "pg_filenode_relation",
        vec![Oid, Oid],
        Regclass,
        pg_filenode_relation,
    )
    .stable();
    r.func(
        "pg_tablespace_location",
        vec![Oid],
        Text,
        pg_tablespace_location,
    )
    .stable();
    r.func(
        "pg_input_is_valid",
        vec![Text, Text],
        Bool,
        pg_input_is_valid,
    )
    .stable();
    r.func("gen_random_bytes", vec![Int4], Bytea, gen_random_bytes)
        .volatile();
    r.func("current_query", vec![], Text, current_query)
        .volatile();
    r.func_ret("enum_range", vec![AnyEnum], Ret::PolyArray, enum_range)
        .stable()
        .non_strict();
    r.func_ret(
        "enum_range",
        vec![AnyEnum, AnyEnum],
        Ret::PolyArray,
        enum_range,
    )
    .stable()
    .non_strict();
    r.func_ret("enum_first", vec![AnyEnum], Ret::Poly, enum_first)
        .stable()
        .non_strict();
    r.func_ret("enum_last", vec![AnyEnum], Ret::Poly, enum_last)
        .stable()
        .non_strict();
    for (op, f) in [
        ("=", enum_eq as super::FnImpl),
        ("<>", enum_ne),
        ("<", enum_lt),
        ("<=", enum_le),
        (">", enum_gt),
        (">=", enum_ge),
    ] {
        r.binop(op, AnyEnum, AnyEnum, Bool, f);
    }
    let _ = err!(INTERNAL_ERROR, "");
}

/// pg_get_indexdef prints function-call expressions without extra parentheses.
fn looks_like_function(sql: &str) -> bool {
    use pg_query::NodeEnum as N;
    match crate::sql::binder::ast::parse_expr(sql) {
        Ok(n) => matches!(
            n.node,
            Some(N::FuncCall(_) | N::CoalesceExpr(_) | N::MinMaxExpr(_) | N::SqlvalueFunction(_))
        ),
        Err(_) => false,
    }
}

/// pg_collation_actual_version(oid): what the provider reports for the collation's locale.
fn collation_actual_version(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = match &a[0] {
        Value::Oid(o) => *o,
        v => v.as_i64().unwrap_or(0) as u32,
    };
    let (provider, locale) = if oid == crate::types::collation::DEFAULT_OID {
        // The database's own locale.
        let l = crate::types::collation::db_locale(&c.exec.catalog());
        (l.provider, l.version_locale().to_string())
    } else {
        let cat = c.exec.catalog();
        let d = crate::types::collation::def_by_oid(&cat, oid).ok_or_else(|| {
            crate::error::Error::new(
                crate::error::code::UNDEFINED_OBJECT,
                format!("collation with OID {oid} does not exist"),
            )
        })?;
        (d.provider, d.locale_str().to_string())
    };
    Ok(crate::types::collation::actual_version(provider, &locale)?
        .map_or(Value::Null, text))
}

/// pg_database_collation_actual_version(oid): what the provider reports for the database's
/// locale.
fn database_collation_actual_version(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = match &a[0] {
        Value::Oid(o) => *o,
        v => v.as_i64().unwrap_or(0) as u32,
    };
    let cluster = &c.exec.sess.cluster;
    let l = cluster
        .databases()
        .into_iter()
        .find(|d| d.0 == oid)
        .and_then(|d| cluster.database_locale(&d.1))
        .ok_or_else(|| {
            crate::error::Error::new(
                crate::error::code::UNDEFINED_OBJECT,
                format!("database with OID {oid} does not exist"),
            )
        })?;
    Ok(l.actual_version()?.map_or(Value::Null, text))
}
