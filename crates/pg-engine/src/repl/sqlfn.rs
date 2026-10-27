//! The SQL interface of replication slots and logical decoding (slotfuncs.c,
//! logicalfuncs.c): creating, dropping, copying and advancing slots, pg_replication_slots,
//! pg_logical_slot_{get,peek}[_binary]_changes, pg_logical_emit_message, the WAL positions.

use super::log::{self, Kind, Marker};
use super::plugin::{Decoder, Setup};
use super::slot::{SlotData, no_slot};
use super::{Repl, check_permissions};
use crate::error::{Error, Result, code};
use crate::sql::func::{FnCtx, Registry, SrfIter, text_arg};
use crate::txn::Database;
use crate::types::Value;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

fn repl(c: &FnCtx) -> Result<Arc<Repl>> {
    c.exec.sess.db.repl().ok_or_else(|| {
        Error::new(
            code::OBJECT_NOT_IN_PREREQUISITE_STATE,
            "replication slots can only be used if \"max_replication_slots\" > 0",
        )
    })
}

fn rows(v: Vec<Vec<Value>>) -> Result<SrfIter> {
    Ok(Box::new(v.into_iter().map(Ok)))
}

fn name(v: &Value) -> String {
    text_arg(v).to_string()
}

fn bool_arg(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Bool(true)))
}

fn lsn_or_null(l: u64) -> Value {
    if l == 0 { Value::Null } else { Value::PgLsn(l) }
}

/// What creating a logical slot found: its consistent point, and the store state that a
/// snapshot of it sees (exactly the commits before that point).
pub struct Created {
    pub lsn: u64,
    pub snapshot_ts: u64,
}

/// Wait for every transaction running now (but `own`) to end: after this, every
/// transaction with a transaction id logs its changes.
fn wait_running(db: &Database, own: Option<u64>, cancel: &dyn Fn() -> Result<()>) -> Result<()> {
    let running = db.xids().snapshot(own).xip;
    loop {
        let now = db.xids().snapshot(own).xip;
        if !running.iter().any(|x| now.contains(x)) {
            return Ok(());
        }
        cancel()?;
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The consistent point: under the commit lock, the WAL position every later commit of the
/// database comes after, and the store state holding exactly the earlier ones.
fn consistent_point(db: &Database, r: &Repl) -> Created {
    let (lsn, ts) = db.store.with_commit_lock(|ts| (r.wal.current(), ts));
    while db.store.visible_ts() < ts {
        std::thread::sleep(Duration::from_millis(1));
    }
    Created { lsn, snapshot_ts: ts }
}

/// output_plugin_libraries allows `plugin` (StartupDecodingContext).
pub fn check_plugin_allowed(plugin: &str, settings: &crate::session::settings::Settings) -> Result<()> {
    let list = settings.get_str("output_plugin_libraries").unwrap_or("");
    let allowed = list.split(',').any(|p| p.trim().trim_matches('"') == plugin);
    if allowed {
        return Ok(());
    }
    Err(Error::new(code::INSUFFICIENT_PRIVILEGE, format!("library \"{plugin}\" may not be used as an output plugin"))
        .hint("If it is safe for all REPLICATION users to use this library as an output plugin, add it to \"output_plugin_libraries\" and reload the server configuration."))
}

/// create_logical_replication_slot: the slot made, its plugin checked, its consistent
/// point found (after the transactions running when it started have ended).
#[allow(clippy::too_many_arguments)]
pub fn create_logical(
    db: &Arc<Database>,
    r: &Repl,
    pid: i32,
    xid: Option<u64>,
    settings: &crate::session::settings::Settings,
    name: &str,
    plugin: &str,
    temporary: bool,
    two_phase: bool,
    failover: bool,
    cancel: &dyn Fn() -> Result<()>,
) -> Result<Created> {
    r.slots.create(
        SlotData {
            name: name.to_string(),
            plugin: Some(plugin.to_string()),
            database: db.oid,
            temporary,
            two_phase,
            failover,
            ..Default::default()
        },
        pid,
    )?;
    let made = (|| {
        if xid.is_some() {
            return Err(Error::new(
                code::ACTIVE_SQL_TRANSACTION,
                "cannot create logical replication slot in transaction that has performed writes",
            ));
        }
        check_plugin_allowed(plugin, settings)?;
        let mut st = super::plugin::Startup::default();
        super::plugin::load(plugin)?.startup(&[], true, &mut st)?;
        db.log.capture.store(true, Ordering::SeqCst);
        wait_running(db, xid, cancel)?;
        Ok(consistent_point(db, r))
    })();
    let made = match made {
        Ok(m) => m,
        Err(e) => {
            r.slots.abort_create(name);
            r.slots_changed(db.oid);
            return Err(e);
        }
    };
    let catalog_xmin = db.xids().snapshot(None).xmin;
    r.slots.finish_create(name, |d| {
        d.restart_lsn = made.lsn;
        d.confirmed_flush = made.lsn;
        d.catalog_xmin = catalog_xmin;
        if two_phase {
            d.two_phase_at = made.lsn;
        }
    })?;
    // What GC may remove starts here.
    db.log.gc_upto.fetch_max(0, Ordering::AcqRel);
    Ok(made)
}

fn pg_create_logical_replication_slot(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let r = repl(c)?;
    check_permissions(&c.exec.catalog(), c.exec.current_user())?;
    r.check_logical()?;
    let slot = name(&a[0]);
    let plugin = name(&a[1]);
    let temporary = bool_arg(a.get(2));
    let db = c.exec.sess.db.clone();
    let xid = c.exec.txn.borrow().xid_if_assigned();
    let cancel = || c.exec.txn.borrow().check_cancel();
    let settings = c.exec.sess.settings.borrow();
    let made = create_logical(
        &db,
        &r,
        c.exec.sess.pid,
        xid,
        &settings,
        &slot,
        &plugin,
        temporary,
        bool_arg(a.get(3)),
        bool_arg(a.get(4)),
        &cancel,
    )?;
    if !temporary {
        r.slots.release(&slot, c.exec.sess.pid);
    }
    rows(vec![vec![Value::Text(slot.into()), Value::PgLsn(made.lsn)]])
}

fn pg_create_physical_replication_slot(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let r = repl(c)?;
    check_permissions(&c.exec.catalog(), c.exec.current_user())?;
    r.check_slots()?;
    let slot = name(&a[0]);
    let reserve = bool_arg(a.get(1));
    let temporary = bool_arg(a.get(2));
    let pid = c.exec.sess.pid;
    r.slots.create(SlotData { name: slot.clone(), temporary, ..Default::default() }, pid)?;
    let lsn = if reserve { r.wal.current() } else { 0 };
    r.slots.finish_create(&slot, |d| d.restart_lsn = lsn)?;
    if !temporary {
        r.slots.release(&slot, pid);
    }
    rows(vec![vec![Value::Text(slot.into()), lsn_or_null(lsn)]])
}

fn pg_drop_replication_slot(a: &[Value], c: &FnCtx) -> Result<Value> {
    let r = repl(c)?;
    check_permissions(&c.exec.catalog(), c.exec.current_user())?;
    r.check_slots()?;
    let d = r.slots.drop_slot(&name(&a[0]), c.exec.sess.pid, false, &|| Ok(()))?;
    if d.plugin.is_some() {
        r.slots_changed(d.database);
    }
    Ok(Value::Null)
}

/// pg_get_replication_slots.
fn pg_get_replication_slots(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let Some(r) = c.exec.sess.db.repl() else { return rows(Vec::new()) };
    let keep = c.exec.sess.settings.borrow().get_str("max_slot_wal_keep_size").map(str::to_string);
    let mut out = Vec::new();
    for s in r.slots.list() {
        let d = &s.data;
        let logical = s.logical();
        let mut row = vec![Value::Null; 20];
        row[0] = Value::Text(d.name.as_str().into());
        row[1] = d.plugin.as_deref().map_or(Value::Null, |p| Value::Text(p.into()));
        row[2] = Value::Text(if logical { "logical" } else { "physical" }.into());
        row[3] = if logical { Value::Oid(d.database) } else { Value::Null };
        row[4] = Value::Bool(d.temporary);
        row[5] = Value::Bool(s.active_pid != 0);
        row[6] = if s.active_pid != 0 { Value::Int4(s.active_pid) } else { Value::Null };
        row[7] = if d.xmin != 0 { Value::Oid(d.xmin as u32) } else { Value::Null };
        row[8] = if d.catalog_xmin != 0 { Value::Oid(d.catalog_xmin as u32) } else { Value::Null };
        row[9] = lsn_or_null(d.restart_lsn);
        row[10] = if logical { lsn_or_null(d.confirmed_flush) } else { Value::Null };
        row[11] = if d.invalidated.is_some() {
            Value::Text("lost".into())
        } else if d.restart_lsn != 0 {
            Value::Text("reserved".into())
        } else {
            Value::Null
        };
        row[12] = match keep.as_deref() {
            Some("-1") | None => Value::Null,
            Some(_) if d.restart_lsn == 0 => Value::Null,
            Some(k) => Value::Int8(k.parse::<i64>().unwrap_or(0) << 20),
        };
        row[13] = Value::Bool(d.two_phase);
        row[14] = lsn_or_null(d.two_phase_at);
        row[15] = s.inactive_since.filter(|_| s.active_pid == 0).map_or(Value::Null, Value::Timestamptz);
        row[16] = if logical { Value::Bool(d.invalidated.is_some()) } else { Value::Null };
        row[17] = d.invalidated.as_deref().map_or(Value::Null, |i| Value::Text(i.into()));
        row[18] = Value::Bool(d.failover);
        row[19] = Value::Bool(false);
        out.push(row);
    }
    rows(out)
}

/// The slot's statistics (pg_stat_get_replication_slot): a slot that doesn't exist has
/// none (NULLs but its name; zeros for one never counted).
fn pg_stat_get_replication_slot(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let nm: String = text_arg(&a[0]).chars().take(63).collect();
    let st = c.exec.sess.db.repl().and_then(|r| r.slots.get(&nm)).map(|s| s.data.stats);
    let mut row = vec![Value::Text(nm.into())];
    match st {
        Some(s) => {
            for v in [s.spill_txns, s.spill_count, s.spill_bytes, s.stream_txns, s.stream_count, s.stream_bytes, s.total_txns, s.total_bytes] {
                row.push(Value::Int8(v));
            }
            row.push(s.reset.map_or(Value::Null, Value::Timestamptz));
        }
        // (A slot without statistics reads as zeros.)
        None => {
            row.extend(std::iter::repeat_n(Value::Int8(0), 8));
            row.push(Value::Null);
        }
    }
    rows(vec![row])
}

fn pg_stat_reset_replication_slot(a: &[Value], c: &FnCtx) -> Result<Value> {
    // No CheckSlotRequirements: a database without the engine's registry (embedded) has no slots.
    let r = c.exec.sess.db.repl();
    let now = log::now();
    match a.first().filter(|v| !v.is_null()) {
        Some(v) => {
            let nm = name(v);
            match r.as_ref().and_then(|r| r.slots.get(&nm)) {
                Some(s) if s.logical() => {}
                Some(_) => return Ok(Value::Null),
                None => {
                    return Err(Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!("replication slot \"{nm}\" does not exist"),
                    ));
                }
            }
            if let Some(r) = &r {
                r.slots.update(&nm, false, |d| d.stats = super::slot::SlotStats { reset: Some(now), ..Default::default() })?;
            }
        }
        None => {
            let Some(r) = r else { return Ok(Value::Null) };
            for s in r.slots.list().into_iter().filter(|s| s.logical()) {
                let _ = r.slots.update(&s.data.name, false, |d| {
                    d.stats = super::slot::SlotStats { reset: Some(now), ..Default::default() }
                });
            }
        }
    }
    Ok(Value::Null)
}

/// Options given as a text[] of name / value pairs.
fn options(v: &Value) -> Result<Vec<(String, Option<String>)>> {
    let Value::Array(arr) = v else {
        if v.is_null() {
            return Err(Error::new(code::NULL_VALUE_NOT_ALLOWED, "options array must not be null"));
        }
        return Ok(Vec::new());
    };
    if arr.dims.len() > 1 {
        return Err(Error::new(code::FEATURE_NOT_SUPPORTED, "array must be one-dimensional"));
    }
    if arr.items.iter().any(Value::is_null) {
        return Err(Error::new(code::FEATURE_NOT_SUPPORTED, "array must not contain nulls"));
    }
    if arr.items.len() % 2 != 0 {
        return Err(Error::new(code::FEATURE_NOT_SUPPORTED, "array must have even number of elements"));
    }
    Ok(arr
        .items
        .chunks(2)
        .map(|p| (text_arg(&p[0]).to_string(), Some(text_arg(&p[1]).to_string())))
        .collect())
}

pub fn work_mem(settings: &crate::session::settings::Settings) -> usize {
    settings
        .get_str("logical_decoding_work_mem")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(65536)
        * 1024
}

/// Fold a decoding run's statistics into the slot's.
pub fn add_stats(r: &Repl, slot: &str, s: &super::slot::SlotStats) {
    let _ = r.slots.update(slot, false, |d| {
        let t = &mut d.stats;
        t.spill_txns += s.spill_txns;
        t.spill_count += s.spill_count;
        t.spill_bytes += s.spill_bytes;
        t.stream_txns += s.stream_txns;
        t.stream_count += s.stream_count;
        t.stream_bytes += s.stream_bytes;
        t.total_txns += s.total_txns;
        t.total_bytes += s.total_bytes;
    });
}

/// pg_logical_slot_get_changes_guts.
fn get_changes(a: &[Value], c: &FnCtx, confirm: bool, binary: bool) -> Result<SrfIter> {
    let r = repl(c)?;
    check_permissions(&c.exec.catalog(), c.exec.current_user())?;
    r.check_logical()?;
    if a[0].is_null() {
        return Err(Error::new(code::NULL_VALUE_NOT_ALLOWED, "slot name must not be null"));
    }
    let slot = name(&a[0]);
    let upto_lsn = match &a[1] {
        Value::PgLsn(l) => *l,
        _ => 0,
    };
    let upto_n = a[2].as_i64().unwrap_or(0) as usize;
    let opts = match &a[3.min(a.len())..] {
        [] => Vec::new(),
        [v @ (Value::Array(_) | Value::Null)] => options(v)?,
        // (The VARIADIC arguments one by one.)
        rest => options(&Value::Array(Arc::new(crate::types::array::ArrayValue {
            elem: crate::types::DataType::Text,
            dims: vec![(rest.len(), 1)],
            items: rest.to_vec(),
        })))?,
    };
    let db = c.exec.sess.db.clone();
    let end_of_wal = db.log.safe_end(&r.wal);
    let pid = c.exec.sess.pid;
    let s = r.slots.acquire(&slot, pid, false, &|| Ok(()))?;
    let res = (|| {
        let Some(plugin) = s.data.plugin.clone() else {
            return Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, "cannot use physical replication slot for logical decoding"));
        };
        if s.data.database != db.oid {
            return Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("replication slot \"{slot}\" was not created in this database"),
            ));
        }
        if let Some(why) = &s.data.invalidated {
            return Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, format!("can no longer get changes from replication slot \"{slot}\""))
                .detail(format!("This replication slot has been invalidated due to \"{why}\".")));
        }
        let settings = c.exec.sess.settings.borrow();
        check_plugin_allowed(&plugin, &settings)?;
        let mut dec = Decoder::new(
            &db,
            &slot,
            Setup {
                plugin: &plugin,
                opts: &opts,
                is_init: false,
                slot_two_phase: s.data.two_phase,
                two_phase_at: s.data.two_phase_at,
                fmt: c.exec.fmt().clone(),
                work_mem: work_mem(&settings),
            },
        )?;
        drop(settings);
        if !binary && dec.binary {
            let f = if confirm { "pg_logical_slot_get_changes" } else { "pg_logical_slot_peek_changes" };
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("logical decoding output plugin \"{plugin}\" produces binary output, but function \"{f}(name,pg_lsn,integer,text[])\" expects textual data"),
            ));
        }
        let start = s.data.confirmed_flush;
        let mut out: Vec<Vec<Value>> = Vec::new();
        let mut end = start;
        let mut done = true;
        for m in super::decode::markers(&db.store, start, end_of_wal) {
            let m: Marker = m?;
            c.exec.txn.borrow().check_cancel()?;
            if upto_lsn != 0 && upto_lsn <= end {
                done = false;
                break;
            }
            dec.decode(&m, &mut |o| {
                let data = if binary {
                    Value::Bytea(o.data.into())
                } else {
                    Value::Text(String::from_utf8_lossy(&o.data).into_owned().into())
                };
                out.push(vec![Value::PgLsn(o.lsn), Value::Oid(o.xid as u32), data]);
                Ok(())
            })?;
            end = m.end_lsn;
            if upto_n != 0 && upto_n <= out.len() {
                done = false;
                break;
            }
        }
        if done {
            end = end.max(if upto_lsn != 0 { upto_lsn.min(end_of_wal) } else { end_of_wal });
        }
        if confirm && end > start {
            let catalog_xmin = db.xids().snapshot(None).xmin;
            r.slots.update(&slot, false, |d| {
                d.confirmed_flush = d.confirmed_flush.max(end);
                d.restart_lsn = d.restart_lsn.max(end);
                d.catalog_xmin = catalog_xmin;
            })?;
        }
        add_stats(&r, &slot, &dec.stats);
        Ok(out)
    })();
    r.slots.release(&slot, pid);
    let out = res?;
    if confirm {
        r.slots_changed(db.oid);
    }
    rows(out)
}

fn pg_logical_slot_get_changes(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    get_changes(a, c, true, false)
}
fn pg_logical_slot_peek_changes(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    get_changes(a, c, false, false)
}
fn pg_logical_slot_get_binary_changes(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    get_changes(a, c, true, true)
}
fn pg_logical_slot_peek_binary_changes(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    get_changes(a, c, false, true)
}

/// pg_replication_slot_advance.
fn pg_replication_slot_advance(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let r = repl(c)?;
    let target = match &a[1] {
        Value::PgLsn(l) => *l,
        _ => 0,
    };
    check_permissions(&c.exec.catalog(), c.exec.current_user())?;
    if target == 0 {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "invalid target WAL LSN"));
    }
    let slot = name(&a[0]);
    let pid = c.exec.sess.pid;
    let db = c.exec.sess.db.clone();
    let s = r.slots.acquire(&slot, pid, false, &|| Ok(()))?;
    let res = (|| {
        if s.data.restart_lsn == 0 {
            return Err(Error::new(code::FEATURE_NOT_SUPPORTED, format!("replication slot \"{slot}\" cannot be advanced"))
                .detail("This slot has never previously reserved WAL, or it has been invalidated."));
        }
        let logical = s.logical();
        let moveto = target.min(if logical { db.log.safe_end(&r.wal) } else { r.wal.current() });
        let min = if logical { s.data.confirmed_flush } else { s.data.restart_lsn };
        if moveto < min {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("cannot advance replication slot to {}, minimum is {}", super::lsn::fmt(moveto), super::lsn::fmt(min)),
            ));
        }
        if logical && s.data.database != db.oid {
            return Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("replication slot \"{slot}\" was not created in this database"),
            ));
        }
        let end = r.slots.update(&slot, true, |d| {
            if logical {
                d.confirmed_flush = d.confirmed_flush.max(moveto);
            }
            d.restart_lsn = d.restart_lsn.max(moveto);
            if logical { d.confirmed_flush } else { d.restart_lsn }
        })?;
        Ok((end, logical))
    })();
    r.slots.release(&slot, pid);
    let (end, logical) = res?;
    if logical {
        r.slots_changed(db.oid);
    }
    rows(vec![vec![Value::Text(slot.into()), Value::PgLsn(end)]])
}

/// copy_replication_slot.
fn copy_slot(a: &[Value], c: &FnCtx, logical: bool) -> Result<SrfIter> {
    let r = repl(c)?;
    check_permissions(&c.exec.catalog(), c.exec.current_user())?;
    if logical {
        r.check_logical()?;
    } else {
        r.check_slots()?;
    }
    let src = name(&a[0]);
    let dst = name(&a[1]);
    let Some(s) = r.slots.get(&src).filter(|s| !s.creating) else { return Err(no_slot(&src)) };
    // (copy_replication_slot's two messages are the other way round: a logical source asked
    // for as physical is "cannot copy physical replication slot ... as a logical".)
    if logical != s.logical() {
        let msg = if s.logical() {
            format!("cannot copy physical replication slot \"{src}\" as a logical replication slot")
        } else {
            format!("cannot copy logical replication slot \"{src}\" as a physical replication slot")
        };
        return Err(Error::new(code::FEATURE_NOT_SUPPORTED, msg));
    }
    if s.data.restart_lsn == 0 {
        return Err(Error::new(code::FEATURE_NOT_SUPPORTED, "cannot copy a replication slot that doesn't reserve WAL"));
    }
    let temporary = match a.get(2) {
        Some(Value::Bool(b)) => *b,
        _ => s.data.temporary,
    };
    let plugin = if logical {
        match a.get(3) {
            Some(v) if !v.is_null() => Some(name(v)),
            _ => s.data.plugin.clone(),
        }
    } else {
        None
    };
    let pid = c.exec.sess.pid;
    let db = c.exec.sess.db.clone();
    if logical && s.data.database != db.oid {
        return Err(Error::new(
            code::OBJECT_NOT_IN_PREREQUISITE_STATE,
            format!("replication slot \"{src}\" was not created in this database"),
        ));
    }
    if let Some(p) = &plugin {
        super::plugin::load(p)?;
    }
    r.slots.create(
        SlotData {
            name: dst.clone(),
            plugin: plugin.clone(),
            database: if logical { s.data.database } else { 0 },
            temporary,
            two_phase: s.data.two_phase && logical,
            failover: false,
            ..Default::default()
        },
        pid,
    )?;
    let src_data = s.data.clone();
    r.slots.finish_create(&dst, |d| {
        d.restart_lsn = src_data.restart_lsn;
        d.confirmed_flush = src_data.confirmed_flush;
        d.catalog_xmin = src_data.catalog_xmin;
        d.two_phase_at = src_data.two_phase_at;
    })?;
    if !temporary {
        r.slots.release(&dst, pid);
    }
    let lsn = if logical { src_data.confirmed_flush } else { src_data.restart_lsn };
    rows(vec![vec![Value::Text(dst.into()), lsn_or_null(lsn)]])
}

fn pg_copy_logical_replication_slot(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    copy_slot(a, c, true)
}
fn pg_copy_physical_replication_slot(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    copy_slot(a, c, false)
}

/// pg_logical_emit_message: transactional, in the transaction's change stream; otherwise
/// its own record at once.
fn pg_logical_emit_message(a: &[Value], c: &FnCtx) -> Result<Value> {
    let transactional = bool_arg(a.first());
    let prefix = text_arg(&a[1]).to_string();
    let content: Vec<u8> = match &a[2] {
        Value::Bytea(b) => b.to_vec(),
        v => text_arg(v).as_bytes().to_vec(),
    };
    let db = c.exec.sess.db.clone();
    let Some(r) = db.repl() else { return Ok(Value::PgLsn(db.store.visible_ts())) };
    if r.wal_level < super::WalLevel::Logical {
        // (Logged, and decoded by no one.)
        return Ok(Value::PgLsn(r.wal.alloc(content.len() as u64 + 32) + content.len() as u64 + 32));
    }
    if transactional {
        c.exec.txn.borrow_mut().xid();
        log::message(&prefix, &content, c.exec)?;
        return Ok(Value::PgLsn(r.wal.current()));
    }
    if !db.log.capture.load(Ordering::SeqCst) {
        return Ok(Value::PgLsn(r.wal.alloc(content.len() as u64 + 32) + content.len() as u64 + 32));
    }
    let mut m = Marker::new(Kind::Message, c.exec.txn.borrow().xid_if_assigned().unwrap_or(0), log::now());
    m.message = Some(log::message_body(&prefix, &content));
    let sealed = std::cell::Cell::new(0);
    let res = db.store.commit_grouped(Vec::new(), |_, w| {
        let (rec, lsn) = log::seal(&db.log, &r.wal, m, 0);
        sealed.set(lsn);
        w.push(rec);
        Ok(())
    }, |_| {}, |_| {});
    if sealed.get() != 0 {
        log::committed(&db.log, sealed.get());
    }
    res?;
    Ok(Value::PgLsn(r.wal.current()))
}

/// pg_sync_replication_slots: only a standby synchronizes slots, and there are none.
fn pg_sync_replication_slots(_: &[Value], c: &FnCtx) -> Result<Value> {
    check_permissions(&c.exec.catalog(), c.exec.current_user())?;
    Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, "replication slots can only be synchronized to a standby server"))
}

/// pg_get_replica_identity_index(regclass): RelationGetReplicaIndex.
fn pg_get_replica_identity_index(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = a[0].as_i64().unwrap_or(0) as u32;
    let cat = c.exec.catalog();
    let Some(t) = cat.tables.get(&oid) else { return Ok(Value::Null) };
    let ix = match t.replident {
        None => t.pkey_index.filter(|_| cat.identity_key(t).is_some()),
        Some('i') => t.indexes.iter().chain(&t.pkey_index).copied().find(|i| cat.indexes.get(i).is_some_and(|x| x.replident)),
        _ => None,
    };
    Ok(ix.map_or(Value::Null, Value::Oid))
}

/// pg_stat_get_wal_senders: the walsenders streaming now.
fn pg_stat_get_wal_senders(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let Some(r) = c.exec.sess.db.repl() else { return rows(Vec::new()) };
    let senders = r.senders.lock().clone();
    rows(
        senders
            .into_iter()
            .map(|s| {
                vec![
                    Value::Int4(s.pid),
                    Value::Text(s.state.into()),
                    lsn_or_null(s.sent_lsn),
                    lsn_or_null(s.write_lsn),
                    lsn_or_null(s.flush_lsn),
                    lsn_or_null(s.replay_lsn),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Int4(0),
                    Value::Text("async".into()),
                    s.reply_time.map_or(Value::Null, Value::Timestamptz),
                ]
            })
            .collect(),
    )
}

pub fn register(r: &mut Registry) {
    super::origin::register(r);
    r.from_pg(3099, None, Some(pg_stat_get_wal_senders));
    r.from_pg(6120, Some(pg_get_replica_identity_index), None);
    r.from_pg(6344, Some(pg_sync_replication_slots), None);
    r.from_pg(3786, None, Some(pg_create_logical_replication_slot));
    r.from_pg(3779, None, Some(pg_create_physical_replication_slot));
    r.from_pg(3780, Some(pg_drop_replication_slot), None);
    r.from_pg(3781, None, Some(pg_get_replication_slots));
    r.from_pg(3782, None, Some(pg_logical_slot_get_changes)).variadic();
    r.from_pg(3783, None, Some(pg_logical_slot_get_binary_changes)).variadic();
    r.from_pg(3784, None, Some(pg_logical_slot_peek_changes)).variadic();
    r.from_pg(3785, None, Some(pg_logical_slot_peek_binary_changes)).variadic();
    r.from_pg(3878, None, Some(pg_replication_slot_advance));
    r.from_pg(4220, None, Some(pg_copy_physical_replication_slot));
    r.from_pg(4221, None, Some(pg_copy_physical_replication_slot));
    r.from_pg(4222, None, Some(pg_copy_logical_replication_slot));
    r.from_pg(4223, None, Some(pg_copy_logical_replication_slot));
    r.from_pg(4224, None, Some(pg_copy_logical_replication_slot));
    r.from_pg(3577, Some(pg_logical_emit_message), None);
    r.from_pg(3578, Some(pg_logical_emit_message), None);
    r.from_pg(6169, None, Some(pg_stat_get_replication_slot));
    r.from_pg(6170, Some(pg_stat_reset_replication_slot), None);
}
