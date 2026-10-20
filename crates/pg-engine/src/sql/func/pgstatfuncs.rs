//! The cumulative statistics accessors (Postgres's pgstatfuncs.c): pg_stat_get_* of
//! tables and indexes, functions and the database, their pg_stat_get_xact_* forms (the
//! backend's counts not flushed yet), and the snapshot functions. See `exec::pgstat`.

use super::{FnCtx, Registry};
use crate::error::Result;
use crate::exec::pgstat::{self, TabCounts, TabEntry};

use crate::types::Value;

fn oid_arg(v: &Value) -> u32 {
    match v {
        Value::Oid(o) => *o,
        other => other.as_i64().unwrap_or(0) as u32,
    }
}

fn ts(t: Option<i64>) -> Value {
    t.map_or(Value::Null, Value::Timestamptz)
}

macro_rules! tab_counters {
    ($($name:ident = $oid:expr => |$e:ident| $body:expr;)*) => {
        $(fn $name(a: &[Value], c: &FnCtx) -> Result<Value> {
            Ok(Value::Int8(tab_entry(a, c).map_or(0, |$e| $body)))
        })*
        const TAB_COUNTERS: &[(u32, crate::sql::func::FnImpl)] = &[$(($oid, $name)),*];
    };
}

tab_counters! {
    numscans = 1928 => |e| e.numscans;
    tuples_returned = 1929 => |e| e.tuples_returned;
    tuples_fetched = 1930 => |e| e.tuples_fetched;
    tuples_inserted = 1931 => |e| e.tuples_inserted;
    tuples_updated = 1932 => |e| e.tuples_updated;
    tuples_deleted = 1933 => |e| e.tuples_deleted;
    blocks_fetched = 1934 => |e| e.blocks_fetched;
    blocks_hit = 1935 => |e| e.blocks_hit;
    tuples_hot_updated = 1972 => |e| e.tuples_hot_updated;
    tuples_newpage_updated = 6217 => |e| e.tuples_newpage_updated;
    live_tuples = 2878 => |e| e.live_tuples;
    dead_tuples = 2879 => |e| e.dead_tuples;
    mod_since_analyze = 3177 => |e| e.mod_since_analyze;
    ins_since_vacuum = 5053 => |e| e.ins_since_vacuum;
    vacuum_count = 3054 => |e| e.vacuum_count;
    autovacuum_count = 3055 => |_e| 0;
    analyze_count = 3056 => |e| e.analyze_count;
    autoanalyze_count = 3057 => |_e| 0;
}

macro_rules! tab_times {
    ($($name:ident = $oid:expr => |$e:ident| $body:expr;)*) => {
        $(fn $name(a: &[Value], c: &FnCtx) -> Result<Value> {
            Ok(tab_entry(a, c).map_or(Value::Null, |$e| ts($body)))
        })*
        const TAB_TIMES: &[(u32, crate::sql::func::FnImpl)] = &[$(($oid, $name)),*];
    };
}

tab_times! {
    lastscan = 6310 => |e| e.lastscan;
    last_vacuum_time = 2781 => |e| e.last_vacuum_time;
    last_autovacuum_time = 2782 => |_e| None;
    last_analyze_time = 2783 => |e| e.last_analyze_time;
    last_autoanalyze_time = 2784 => |_e| None;
}

macro_rules! tab_floats {
    ($($name:ident = $oid:expr => |$e:ident| $body:expr;)*) => {
        $(fn $name(a: &[Value], c: &FnCtx) -> Result<Value> {
            Ok(Value::Float8(tab_entry(a, c).map_or(0.0, |$e| $body)))
        })*
        const TAB_FLOATS: &[(u32, crate::sql::func::FnImpl)] = &[$(($oid, $name)),*];
    };
}

tab_floats! {
    total_vacuum_time = 6358 => |e| e.total_vacuum_time;
    total_autovacuum_time = 6359 => |_e| 0.0;
    total_analyze_time = 6360 => |e| e.total_analyze_time;
    total_autoanalyze_time = 6361 => |_e| 0.0;
}

fn tab_entry(a: &[Value], c: &FnCtx) -> Option<TabEntry> {
    pgstat::fetch_table(c.exec, oid_arg(&a[0]))
}

/// A pg_stat_get_xact_* count: the backend's pending count, and with `live` (1, 2, 3)
/// the live subtransactions' inserted / updated / deleted.
fn xact_count(a: &[Value], c: &FnCtx, get: fn(&TabCounts) -> i64, live: u8) -> Result<Value> {
    let rel = oid_arg(&a[0]);
    let mut counts = c
        .exec
        .sess
        .pgstat
        .borrow()
        .tabs
        .get(&rel)
        .copied()
        .unwrap_or_default();
    let txn = c.exec.txn.borrow();
    // Aborted subtransactions' counts are already the backend's.
    if let Some(d) = txn.pgstat.done(rel) {
        counts.tuples_inserted += d.tuples_inserted;
        counts.tuples_updated += d.tuples_updated;
        counts.tuples_deleted += d.tuples_deleted;
    }
    let (ins, upd, del) = txn.pgstat.live(rel);
    let n = get(&counts)
        + match live {
            1 => ins,
            2 => upd,
            3 => del,
            _ => 0,
        };
    Ok(Value::Int8(n))
}

macro_rules! xact_counters {
    ($($name:ident = $oid:expr => |$c:ident| $body:expr, $live:expr;)*) => {
        $(fn $name(a: &[Value], c: &FnCtx) -> Result<Value> {
            xact_count(a, c, |$c| $body, $live)
        })*
        const XACT_COUNTERS: &[(u32, crate::sql::func::FnImpl)] = &[$(($oid, $name)),*];
    };
}

xact_counters! {
    xact_numscans = 3037 => |c| c.numscans, 0;
    xact_tuples_returned = 3038 => |c| c.tuples_returned, 0;
    xact_tuples_fetched = 3039 => |c| c.tuples_fetched, 0;
    xact_tuples_inserted = 3040 => |c| c.tuples_inserted, 1;
    xact_tuples_updated = 3041 => |c| c.tuples_updated, 2;
    xact_tuples_deleted = 3042 => |c| c.tuples_deleted, 3;
    xact_tuples_hot_updated = 3043 => |c| c.tuples_hot_updated, 0;
    xact_tuples_newpage_updated = 6218 => |c| c.tuples_newpage_updated, 0;
    xact_blocks_fetched = 3044 => |c| c.blocks_fetched, 0;
    xact_blocks_hit = 3045 => |c| c.blocks_hit, 0;
}

fn function_calls(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(pgstat::fetch_function(c.exec, oid_arg(&a[0])).map_or(Value::Null, |f| Value::Int8(f.calls)))
}

fn function_total_time(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(pgstat::fetch_function(c.exec, oid_arg(&a[0]))
        .map_or(Value::Null, |f| Value::Float8(f.total_time)))
}

fn function_self_time(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(pgstat::fetch_function(c.exec, oid_arg(&a[0]))
        .map_or(Value::Null, |f| Value::Float8(f.self_time)))
}

fn pending_function(c: &FnCtx, f: u32) -> Option<pgstat::FuncCounts> {
    c.exec.sess.pgstat.borrow().funcs.get(&f).copied()
}

fn xact_function_calls(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(pending_function(c, oid_arg(&a[0])).map_or(Value::Null, |f| Value::Int8(f.calls)))
}

fn xact_function_total_time(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(pending_function(c, oid_arg(&a[0])).map_or(Value::Null, |f| Value::Float8(f.total_time)))
}

fn xact_function_self_time(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(pending_function(c, oid_arg(&a[0])).map_or(Value::Null, |f| Value::Float8(f.self_time)))
}

macro_rules! db_counters {
    ($($name:ident = $oid:expr => |$d:ident| $body:expr;)*) => {
        $(fn $name(a: &[Value], c: &FnCtx) -> Result<Value> {
            Ok(pgstat::fetch_db(c.exec, oid_arg(&a[0])).map_or(Value::Null, |$d| Value::Int8($body)))
        })*
        const DB_COUNTERS: &[(u32, crate::sql::func::FnImpl)] = &[$(($oid, $name)),*];
    };
}

db_counters! {
    db_xact_commit = 1942 => |d| d.xact_commit;
    db_xact_rollback = 1943 => |d| d.xact_rollback;
    db_blocks_fetched = 1944 => |d| d.blocks_fetched;
    db_blocks_hit = 1945 => |d| d.blocks_hit;
    db_tuples_returned = 2758 => |d| d.tuples_returned;
    db_tuples_fetched = 2759 => |d| d.tuples_fetched;
    db_tuples_inserted = 2760 => |d| d.tuples_inserted;
    db_tuples_updated = 2761 => |d| d.tuples_updated;
    db_tuples_deleted = 2762 => |d| d.tuples_deleted;
    db_sessions = 6188 => |d| d.sessions;
    db_parallel_workers_to_launch = 6355 => |d| d.parallel_workers_to_launch;
    db_parallel_workers_launched = 6356 => |d| d.parallel_workers_launched;
}

fn db_stat_reset_time(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(pgstat::fetch_db(c.exec, oid_arg(&a[0])).map_or(Value::Null, |d| ts(d.stat_reset)))
}

fn snapshot_timestamp(_: &[Value], c: &FnCtx) -> Result<Value> {
    let mode = match c
        .exec
        .sess
        .settings
        .borrow()
        .get_raw("stats_fetch_consistency")
        .as_deref()
    {
        Some("none") => pgstat::Consistency::None,
        Some("snapshot") => pgstat::Consistency::Snapshot,
        _ => pgstat::Consistency::Cache,
    };
    Ok(ts(c.exec.sess.pgstat.borrow_mut().snapshot_timestamp(mode)))
}

fn clear_snapshot(_: &[Value], c: &FnCtx) -> Result<Value> {
    c.exec.sess.pgstat.borrow_mut().clear_snapshot();
    Ok(Value::Null)
}

fn checkpointer_num_requested(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Int8(pgstat::with_shared(c.exec, |s| s.checkpoints_requested)))
}

fn checkpointer_num_performed(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Int8(pgstat::with_shared(c.exec, |s| s.checkpoints_performed)))
}

fn checkpointer_reset_time(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(ts(pgstat::reset_time(c.exec, "checkpointer")))
}

fn bgwriter_reset_time(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(ts(pgstat::reset_time(c.exec, "bgwriter")))
}

pub fn register(r: &mut Registry) {
    for (oid, f) in TAB_COUNTERS.iter().chain(TAB_TIMES).chain(TAB_FLOATS).chain(XACT_COUNTERS) {
        r.from_pg(*oid, Some(*f), None);
    }
    r.from_pg(2978, Some(function_calls), None);
    r.from_pg(2979, Some(function_total_time), None);
    r.from_pg(2980, Some(function_self_time), None);
    r.from_pg(3046, Some(xact_function_calls), None);
    r.from_pg(3047, Some(xact_function_total_time), None);
    r.from_pg(3048, Some(xact_function_self_time), None);
    for (oid, f) in DB_COUNTERS {
        r.from_pg(*oid, Some(*f), None);
    }
    r.from_pg(3074, Some(db_stat_reset_time), None);
    r.from_pg(3788, Some(snapshot_timestamp), None);
    r.from_pg(2770, Some(checkpointer_num_requested), None);
    r.from_pg(6377, Some(checkpointer_num_performed), None);
    r.from_pg(6314, Some(checkpointer_reset_time), None);
    r.from_pg(3075, Some(bgwriter_reset_time), None);
    r.from_pg(2230, Some(clear_snapshot), None);
}
