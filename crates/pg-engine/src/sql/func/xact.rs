//! Transaction ids and snapshots: pg_current_xact_id(), pg_current_snapshot(), xid8's
//! functions, and their txid_* forerunners (bigint and txid_snapshot).

use super::{FnCtx, Registry, SrfIter, text};
use crate::error::Result;
use crate::types::Value;
use crate::types::snapshot::Snapshot;
use std::sync::Arc;

fn rows(r: Vec<Vec<Value>>) -> Result<SrfIter> {
    Ok(Box::new(r.into_iter().map(Ok)))
}

fn xid_of(v: &Value) -> u64 {
    match v {
        Value::Xid8(x) => *x,
        v => v.as_i64().unwrap_or(0) as u64,
    }
}

fn snap(v: &Value) -> Result<&Snapshot> {
    match v {
        Value::Snapshot(s) => Ok(s),
        _ => Err(crate::error::internal("not a snapshot")),
    }
}

fn current_xact_id(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Xid8(c.exec.txn.borrow_mut().xid()))
}

fn current_xact_id_if_assigned(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(c.exec
        .txn
        .borrow()
        .xid_if_assigned()
        .map_or(Value::Null, Value::Xid8))
}

fn txid_current(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Int8(c.exec.txn.borrow_mut().xid() as i64))
}

fn txid_current_if_assigned(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(c.exec
        .txn
        .borrow()
        .xid_if_assigned()
        .map_or(Value::Null, |x| Value::Int8(x as i64)))
}

fn current_snapshot(_: &[Value], c: &FnCtx) -> Result<Value> {
    let txn = c.exec.txn.borrow();
    Ok(Value::Snapshot(Arc::new(
        txn.db.xids().snapshot(txn.xid_if_assigned()),
    )))
}

/// pg_export_snapshot(): the transaction's snapshot, for SET TRANSACTION SNAPSHOT elsewhere.
fn export_snapshot(_: &[Value], c: &FnCtx) -> Result<Value> {
    let id = c.exec.txn.borrow_mut().export_snapshot(c.exec.sess.pid)?;
    Ok(text(id))
}

fn snapshot_xmin(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Xid8(snap(&a[0])?.xmin))
}

fn snapshot_xmax(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Xid8(snap(&a[0])?.xmax))
}

fn txid_snapshot_xmin(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int8(snap(&a[0])?.xmin as i64))
}

fn txid_snapshot_xmax(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int8(snap(&a[0])?.xmax as i64))
}

fn snapshot_xip(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    rows(
        snap(&a[0])?
            .xip
            .iter()
            .map(|x| vec![Value::Xid8(*x)])
            .collect(),
    )
}

fn txid_snapshot_xip(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    rows(
        snap(&a[0])?
            .xip
            .iter()
            .map(|x| vec![Value::Int8(*x as i64)])
            .collect(),
    )
}

fn visible_in_snapshot(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(snap(&a[1])?.visible(xid_of(&a[0]))))
}

fn xact_status(a: &[Value], c: &FnCtx) -> Result<Value> {
    let x = xid_of(&a[0]);
    // Before the first normal id: bootstrap and frozen transactions, committed.
    if x < crate::txn::xids::FIRST_NORMAL_XID {
        return Ok(if x == 0 {
            Value::Null
        } else {
            text("committed")
        });
    }
    let xids = c.exec.txn.borrow().db.xids();
    // TransactionIdInRecentPast: older than the oldest status kept is unknown.
    if x < xids.oldest() {
        return Ok(Value::Null);
    }
    Ok(text(xids.status(x)?))
}

fn xid8_to_xid(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Oid(xid_of(&a[0]) as u32))
}

fn xid8_larger(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Xid8(xid_of(&a[0]).max(xid_of(&a[1]))))
}

fn xid8_smaller(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Xid8(xid_of(&a[0]).min(xid_of(&a[1]))))
}

/// age(xid): how many transactions back `xid` is from ours (or the next one, if we have
/// none); special xids are infinitely old.
fn xid_age(a: &[Value], c: &FnCtx) -> Result<Value> {
    let x = xid_of(&a[0]) as u32;
    if (x as u64) < crate::txn::xids::FIRST_NORMAL_XID {
        return Ok(Value::Int4(i32::MAX));
    }
    let txn = c.exec.txn.borrow();
    let now = txn.xid_if_assigned().unwrap_or_else(|| txn.db.xids().next()) as u32;
    Ok(Value::Int4(now.wrapping_sub(x) as i32))
}

/// mxid_age(xid): no multixacts are made here, so the next one is still the first.
fn mxid_age(a: &[Value], _: &FnCtx) -> Result<Value> {
    let x = xid_of(&a[0]) as u32;
    if x == 0 {
        return Ok(Value::Int4(i32::MAX));
    }
    Ok(Value::Int4(1u32.wrapping_sub(x) as i32))
}

/// pg_xact_commit_timestamp(xid): NULL when there is no data for it.
fn xact_commit_timestamp(a: &[Value], c: &FnCtx) -> Result<Value> {
    let db = c.exec.txn.borrow().db.clone();
    Ok(match crate::repl::commit_ts::get_any(&db, xid_of(&a[0]))? {
        Some(s) => Value::Timestamptz(s.ts),
        None => Value::Null,
    })
}

/// pg_xact_commit_timestamp_origin(xid): (timestamp, roident), both NULL without data.
fn xact_commit_timestamp_origin(a: &[Value], c: &FnCtx) -> Result<Value> {
    let db = c.exec.txn.borrow().db.clone();
    let v = match crate::repl::commit_ts::get_any(&db, xid_of(&a[0]))? {
        Some(s) => vec![Value::Timestamptz(s.ts), Value::Oid(s.origin as u32)],
        None => vec![Value::Null, Value::Null],
    };
    Ok(Value::Record(Arc::new(v)))
}

/// pg_last_committed_xact(): (xid, timestamp, roident) of the latest commit, NULLs before one.
fn last_committed_xact(_: &[Value], c: &FnCtx) -> Result<Value> {
    let repl = c.exec.txn.borrow().db.repl();
    let Some(ct) = repl.as_ref().and_then(|r| r.commit_ts.as_ref()) else {
        return Err(crate::repl::commit_ts::disabled());
    };
    let (xid, s) = ct.last();
    let v = if xid < crate::txn::xids::FIRST_NORMAL_XID {
        vec![Value::Null, Value::Null, Value::Null]
    } else {
        vec![Value::Oid(xid as u32), Value::Timestamptz(s.ts), Value::Oid(s.origin as u32)]
    };
    Ok(Value::Record(Arc::new(v)))
}

pub fn register(r: &mut Registry) {
    r.from_pg(5059, Some(current_xact_id), None);
    r.from_pg(5060, Some(current_xact_id_if_assigned), None);
    r.from_pg(2943, Some(txid_current), None);
    r.from_pg(3348, Some(txid_current_if_assigned), None);
    r.from_pg(3809, Some(export_snapshot), None);
    r.from_pg(5061, Some(current_snapshot), None);
    r.from_pg(2944, Some(current_snapshot), None);
    r.from_pg(5062, Some(snapshot_xmin), None);
    r.from_pg(5063, Some(snapshot_xmax), None);
    r.from_pg(2945, Some(txid_snapshot_xmin), None);
    r.from_pg(2946, Some(txid_snapshot_xmax), None);
    r.from_pg(5064, None, Some(snapshot_xip));
    r.from_pg(2947, None, Some(txid_snapshot_xip));
    r.from_pg(5065, Some(visible_in_snapshot), None);
    r.from_pg(2948, Some(visible_in_snapshot), None);
    r.from_pg(5066, Some(xact_status), None);
    r.from_pg(3360, Some(xact_status), None);
    r.from_pg(5071, Some(xid8_to_xid), None);
    r.from_pg(1181, Some(xid_age), None);
    r.from_pg(3939, Some(mxid_age), None);
    r.from_pg(5097, Some(xid8_larger), None);
    r.from_pg(5098, Some(xid8_smaller), None);
    // (Their records' fields are known, so `(f(x)).roident` resolves as in PostgreSQL.)
    let row = |f: &[(&str, crate::types::DataType)]| {
        super::Ret::Fixed(crate::types::DataType::row(f.iter().map(|(n, t)| (n.to_string(), t.clone())).collect()))
    };
    use crate::types::DataType::{Oid, Timestamptz};
    r.from_pg(3581, Some(xact_commit_timestamp), None);
    r.from_pg(6168, Some(xact_commit_timestamp_origin), None).ret = row(&[("timestamp", Timestamptz(-1)), ("roident", Oid)]);
    r.from_pg(3583, Some(last_committed_xact), None).ret =
        row(&[("xid", crate::types::DataType::Xid), ("timestamp", Timestamptz(-1)), ("roident", Oid)]);
}
