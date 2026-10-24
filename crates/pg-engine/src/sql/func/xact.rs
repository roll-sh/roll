//! Transaction ids and snapshots: pg_current_xact_id(), pg_current_snapshot(), xid8's
//! functions, and their txid_* forerunners (bigint and txid_snapshot).

use super::{FnCtx, Registry, SrfIter, text};
use crate::error::{Error, Result, code};
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

/// track_commit_timestamp is off.
fn commit_ts_disabled(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(Error::new(
        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
        "could not get commit timestamp data",
    )
    .hint("Make sure the configuration parameter \"track_commit_timestamp\" is set."))
}

pub fn register(r: &mut Registry) {
    r.from_pg(5059, Some(current_xact_id), None);
    r.from_pg(5060, Some(current_xact_id_if_assigned), None);
    r.from_pg(2943, Some(txid_current), None);
    r.from_pg(3348, Some(txid_current_if_assigned), None);
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
    for oid in [3581, 6168, 3583] {
        r.from_pg(oid, Some(commit_ts_disabled), None);
    }
}
