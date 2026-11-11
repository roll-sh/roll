//! WAL, recovery and backup control functions (xlogfuncs.c, walsummaryfuncs.c) over the
//! cluster's WAL model (`pgstat::Fixed`): positions grow with the WAL the cluster writes,
//! and the server is a primary that is never in recovery.

use super::{FnCtx, Registry, SrfIter, text_arg};
use crate::error::{Error, Result, Severity, code};
use crate::exec::pgstat::{self, Backup, WAL_SEGMENT_SIZE};
use crate::types::Value;

fn lsn_arg(v: &Value) -> u64 {
    match v {
        Value::PgLsn(l) => *l,
        v => v.as_i64().unwrap_or(0) as u64,
    }
}

fn rows(v: Vec<Vec<Value>>) -> Result<SrfIter> {
    Ok(Box::new(v.into_iter().map(Ok)))
}

/// pg_current_wal_lsn / _insert_lsn / _flush_lsn: commits are flushed as they're written,
/// so the three are one position.
fn current_wal_lsn(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::PgLsn(pgstat::wal_lsn(&c.exec.sess.db)))
}

/// pg_wal_lsn_diff(a, b): a - b bytes, as numeric.
fn wal_lsn_diff(a: &[Value], _: &FnCtx) -> Result<Value> {
    let d = lsn_arg(&a[0]) as i128 - lsn_arg(&a[1]) as i128;
    Ok(Value::numeric(crate::types::numeric::Numeric::from_i128(d)))
}

/// What only a standby has: none here.
fn null(_: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Null)
}

/// The recovery control functions outside recovery.
fn not_in_recovery(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, "recovery is not in progress")
        .hint("Recovery control functions can only be executed during recovery."))
}

fn switch_wal(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::PgLsn(pgstat::fixed(&c.exec.sess.db).lock().switch()))
}

/// pg_create_restore_point(name): an XLOG_RESTORE_POINT record; its end.
fn create_restore_point(a: &[Value], c: &FnCtx) -> Result<Value> {
    let name = text_arg(&a[0]);
    if name.len() >= 64 {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "value too long for restore point (maximum 63 characters)",
        ));
    }
    Ok(Value::PgLsn(pgstat::fixed(&c.exec.sess.db).lock().write_record(98).1))
}

/// pg_log_standby_snapshot: a running-transactions record; its end.
fn log_standby_snapshot(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::PgLsn(pgstat::fixed(&c.exec.sess.db).lock().write_record(50).1))
}

/// pg_backup_start(label, fast): a WAL switch and a checkpoint; the backup starts at its
/// redo point.
fn backup_start(a: &[Value], c: &FnCtx) -> Result<Value> {
    let label = text_arg(&a[0]);
    let pid = c.exec.sess.pid;
    let fixed = pgstat::fixed(&c.exec.sess.db);
    let mut f = fixed.lock();
    if f.backups.contains_key(&pid) {
        return Err(Error::new(
            code::OBJECT_NOT_IN_PREREQUISITE_STATE,
            "a backup is already in progress in this session",
        ));
    }
    if label.len() > 1024 {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "backup label too long (max 1024 bytes)"));
    }
    f.switch();
    let time = crate::types::datetime::Clock::system().now;
    f.checkpoints_requested += 1;
    f.checkpoints_performed += 1;
    let (start, checkpoint) = f.checkpoint_at(time);
    f.backups.insert(pid, Backup { label: label.to_string(), start, checkpoint, time });
    Ok(Value::PgLsn(start))
}

/// The backup_label file of `b` (build_backup_content).
fn backup_label(b: &Backup) -> String {
    let secs = b.time.div_euclid(1_000_000) + 946_684_800;
    let time = chrono::DateTime::from_timestamp(secs, 0)
        .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_default();
    let seg = (b.start - 1) / WAL_SEGMENT_SIZE;
    let file = format!("{:08X}{:08X}{:08X}", 1, seg >> 8, seg & 0xff);
    let lsn = |l: u64| format!("{:X}/{:X}", l >> 32, l & 0xffff_ffff);
    format!(
        "START WAL LOCATION: {} (file {file})\nCHECKPOINT LOCATION: {}\nBACKUP METHOD: streamed\n\
         BACKUP FROM: primary\nSTART TIME: {time}\nLABEL: {}\nSTART TIMELINE: 1\n",
        lsn(b.start),
        lsn(b.checkpoint),
        b.label
    )
}

/// pg_backup_stop(wait_for_archive): a backup-end record and a WAL switch; the record's end,
/// the backup_label and tablespace_map contents.
fn backup_stop(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let pid = c.exec.sess.pid;
    let fixed = pgstat::fixed(&c.exec.sess.db);
    let mut f = fixed.lock();
    let Some(b) = f.backups.remove(&pid) else {
        return Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, "backup is not in progress")
            .hint("Did you call pg_backup_start()?"));
    };
    let (_, end) = f.write_record(34);
    f.switch();
    drop(f);
    // (wait_for_archive defaults to true.)
    if !matches!(a.first(), Some(Value::Bool(false)))
        && c.exec.sess.settings.borrow().get_str("archive_mode") == Some("off")
    {
        let mut e = Error::new(
            code::SUCCESSFUL_COMPLETION,
            "WAL archiving is not enabled; you must ensure that all required WAL segments are copied through other means to complete the backup",
        );
        e.severity = Severity::Notice;
        c.exec.notice(e);
    }
    // tablespace_map: each user tablespace's oid and location.
    let cat = c.exec.catalog();
    let mut spcs: Vec<_> = cat.tablespaces.values().collect();
    spcs.sort_by_key(|t| t.oid);
    let map: String = spcs
        .iter()
        .map(|t| match t.location.as_str() {
            "" => format!("{0} pg_tblspc/{0}\n", t.oid),
            l => format!("{} {l}\n", t.oid),
        })
        .collect();
    rows(vec![vec![Value::PgLsn(end), Value::text(backup_label(&b)), Value::text(map)]])
}

/// The WAL resource managers (rmgrlist.h), all built in.
const RMGRS: [&str; 22] = [
    "XLOG",
    "Transaction",
    "Storage",
    "CLOG",
    "Database",
    "Tablespace",
    "MultiXact",
    "RelMap",
    "Standby",
    "Heap2",
    "Heap",
    "Btree",
    "Hash",
    "Gin",
    "Gist",
    "Sequence",
    "SPGist",
    "BRIN",
    "CommitTs",
    "ReplicationOrigin",
    "Generic",
    "LogicalMessage",
];

fn resource_managers(_: &[Value], _: &FnCtx) -> Result<SrfIter> {
    rows(
        RMGRS
            .iter()
            .enumerate()
            .map(|(i, n)| vec![Value::Int4(i as i32), Value::text(*n), Value::Bool(true)])
            .collect(),
    )
}

/// pg_get_wal_summarizer_state: summarize_wal is off, so nothing summarized and no
/// summarizer process.
fn summarizer_state(_: &[Value], _: &FnCtx) -> Result<SrfIter> {
    rows(vec![vec![Value::Int8(0), Value::PgLsn(0), Value::PgLsn(0), Value::Null]])
}

fn no_rows(_: &[Value], _: &FnCtx) -> Result<SrfIter> {
    rows(Vec::new())
}

/// pg_wal_summary_contents(tli, start, end): there are no summary files.
fn summary_contents(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    let tli = a[0].as_i64().unwrap_or(0);
    let (s, e) = (lsn_arg(&a[1]), lsn_arg(&a[2]));
    let half = |l: u64| format!("{:08X}{:08X}", l >> 32, l & 0xffff_ffff);
    Err(Error::new(
        code::UNDEFINED_FILE,
        format!(
            "could not open file \"pg_wal/summaries/{:08X}{}{}.summary\": No such file or directory",
            tli as u32,
            half(s),
            half(e)
        ),
    ))
}

/// pg_jit_available: the engine compiles no expressions to machine code.
fn jit_available(_: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(false))
}

pub fn register(r: &mut Registry) {
    for oid in [2849, 2852, 3330] {
        r.from_pg(oid, Some(current_wal_lsn), None);
    }
    r.from_pg(3165, Some(wal_lsn_diff), None);
    for oid in [3820, 3821, 3830] {
        r.from_pg(oid, Some(null), None);
    }
    for oid in [3073, 1137, 3071, 3072, 3436] {
        r.from_pg(oid, Some(not_in_recovery), None);
    }
    r.from_pg(2848, Some(switch_wal), None);
    r.from_pg(3098, Some(create_restore_point), None);
    r.from_pg(6305, Some(log_standby_snapshot), None);
    r.from_pg(2172, Some(backup_start), None);
    r.from_pg(2739, None, Some(backup_stop));
    r.from_pg(6224, None, Some(resource_managers));
    r.from_pg(6323, None, Some(summarizer_state));
    r.from_pg(6321, None, Some(no_rows));
    r.from_pg(6322, None, Some(summary_contents));
    r.from_pg(315, Some(jit_available), None);
}
