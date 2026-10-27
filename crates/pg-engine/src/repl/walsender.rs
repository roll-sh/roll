//! The walsender side of a replication connection (walsender.c): replication commands, and
//! START_REPLICATION ... LOGICAL streaming the change stream in CopyBoth mode (XLogData,
//! keepalives, standby status updates confirming positions). Physical replication (WAL
//! shipping, base backups) exposes PostgreSQL's on-disk format, which the engine doesn't
//! have: those commands fail (HANDOFF "Logical replication").

use super::cmd::{Cmd, SlotKind};
use super::plugin::{Decoder, Setup};
use super::{Repl, Sender};
use crate::error::{Error, Result, code};
use crate::session::{Session, TxnStatus};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The connection, as the walsender writes and reads it.
pub trait Wire {
    /// Queue a message.
    fn put(&mut self, t: u8, body: &[u8]);
    fn flush(&mut self) -> std::io::Result<()>;
    /// A client message if one arrives within `wait` (None: none yet; an error at EOF).
    fn poll(&mut self, wait: Duration) -> std::io::Result<Option<(u8, Vec<u8>)>>;
}

/// What kind of walsender (the startup packet's `replication`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// `replication=true`: physical, no database.
    Physical,
    /// `replication=database`: logical, SQL too.
    Database,
}

/// The startup packet's `replication` value: None for a regular backend.
pub fn mode_of(v: &str) -> Option<Mode> {
    if v.eq_ignore_ascii_case("database") {
        return Some(Mode::Database);
    }
    match crate::types::text::parse_bool(v) {
        Ok(true) => Some(Mode::Physical),
        _ => None,
    }
}

/// InitPostgres's walsender check: the role must have REPLICATION.
pub fn check_role(cat: &crate::catalog::Catalog, user: &str) -> Result<()> {
    let ok = cat.role_by_name(user).is_none_or(|r| r.superuser || r.replication);
    if ok {
        return Ok(());
    }
    Err(Error::new(code::INSUFFICIENT_PRIVILEGE, "permission denied to start WAL sender")
        .detail("Only roles with the REPLICATION attribute may start a WAL sender process.")
        .fatal())
}

const TEXT: u32 = 25;
const INT4: u32 = 23;
const INT8: u32 = 20;

fn put_cstr(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(s.as_bytes());
    b.push(0);
}

/// A result set of text-format columns: RowDescription and DataRows.
fn result(w: &mut dyn Wire, cols: &[(&str, u32)], rows: &[Vec<Option<String>>]) {
    let mut b = Vec::new();
    b.extend_from_slice(&(cols.len() as i16).to_be_bytes());
    for (name, ty) in cols {
        put_cstr(&mut b, name);
        b.extend_from_slice(&0i32.to_be_bytes());
        b.extend_from_slice(&0i16.to_be_bytes());
        b.extend_from_slice(&(*ty as i32).to_be_bytes());
        let len: i16 = match *ty {
            INT4 => 4,
            INT8 => 8,
            _ => -1,
        };
        b.extend_from_slice(&len.to_be_bytes());
        b.extend_from_slice(&(-1i32).to_be_bytes());
        b.extend_from_slice(&0i16.to_be_bytes());
    }
    w.put(b'T', &b);
    for r in rows {
        let mut b = Vec::new();
        b.extend_from_slice(&(r.len() as i16).to_be_bytes());
        for v in r {
            match v {
                None => b.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(s) => {
                    b.extend_from_slice(&(s.len() as i32).to_be_bytes());
                    b.extend_from_slice(s.as_bytes());
                }
            }
        }
        w.put(b'D', &b);
    }
}

fn complete(w: &mut dyn Wire, tag: &str) {
    let mut b = Vec::new();
    put_cstr(&mut b, tag);
    w.put(b'C', &b);
}

fn repl(s: &Session) -> Result<Arc<Repl>> {
    s.state.db.repl().ok_or_else(|| Error::new(code::FEATURE_NOT_SUPPORTED, "replication is not available in this engine"))
}

fn physical_unsupported(what: &str) -> Error {
    Error::new(code::FEATURE_NOT_SUPPORTED, format!("{what} is not supported by this server"))
        .detail("roll's engine has no PostgreSQL WAL or data files to ship: physical replication and base backups are not available.")
        .hint("Use logical replication (a replication=database connection, or CREATE SUBSCRIPTION).")
}

fn prevent_in_block(s: &Session, what: &str) -> Result<()> {
    if s.status() != TxnStatus::Idle {
        return Err(Error::new(code::ACTIVE_SQL_TRANSACTION, format!("{what} cannot run inside a transaction block")));
    }
    Ok(())
}

/// The snapshot a CREATE_REPLICATION_SLOT exported, which lives until the next command.
pub struct Exported {
    db_instance: u64,
    owner: u64,
}

impl Exported {
    pub fn release(self, s: &Session) {
        s.state.db.xids().unexport(self.db_instance, self.owner);
    }
}

/// exec_replication_command.
pub fn exec(w: &mut dyn Wire, s: &mut Session, mode: Mode, cmd: Cmd, exported: &mut Option<Exported>) -> Result<()> {
    if let Some(e) = exported.take() {
        e.release(s);
    }
    if s.status() == TxnStatus::Failed {
        return Err(Error::new(
            code::IN_FAILED_SQL_TRANSACTION,
            "current transaction is aborted, commands ignored until end of transaction block",
        ));
    }
    let r = repl(s)?;
    match cmd {
        Cmd::IdentifySystem => {
            let db = (mode == Mode::Database).then(|| s.state.db.name.clone());
            result(
                w,
                &[("systemid", TEXT), ("timeline", INT4), ("xlogpos", TEXT), ("dbname", TEXT)],
                &[vec![
                    Some("7000000000000000001".into()),
                    Some("1".into()),
                    Some(super::lsn::fmt(r.wal.current())),
                    db,
                ]],
            );
            complete(w, "IDENTIFY_SYSTEM");
        }
        Cmd::Show(name) => {
            let st = s.state.settings.borrow();
            if !st.exists(&name) {
                return Err(Error::new(code::UNDEFINED_OBJECT, format!("unrecognized configuration parameter \"{name}\"")));
            }
            let v = st.show_value(&name);
            drop(st);
            result(w, &[(&name, TEXT)], &[vec![Some(v)]]);
            complete(w, "SHOW");
        }
        Cmd::TimelineHistory(tli) => {
            prevent_in_block(s, "TIMELINE_HISTORY")?;
            return Err(Error::new(
                code::UNDEFINED_FILE,
                format!("could not open file \"pg_wal/{tli:08X}.history\": No such file or directory"),
            ));
        }
        Cmd::CreateSlot { name, temporary, kind, snapshot, two_phase, failover } => {
            create_slot(w, s, &r, mode, name, temporary, kind, snapshot, two_phase, failover, exported)?;
            complete(w, "CREATE_REPLICATION_SLOT");
        }
        Cmd::AlterSlot { name, failover, two_phase } => {
            let pid = s.state.pid;
            let sl = r.slots.acquire(&name, pid, false, &|| Ok(()))?;
            let res = (|| {
                if !sl.logical() {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "cannot use ALTER_REPLICATION_SLOT with a physical replication slot",
                    ));
                }
                if failover == Some(true) && sl.data.temporary {
                    return Err(Error::new(code::FEATURE_NOT_SUPPORTED, "cannot enable failover for a temporary replication slot"));
                }
                let at = r.wal.current();
                r.slots.update(&name, true, |d| {
                    if let Some(f) = failover {
                        d.failover = f;
                    }
                    if let Some(t) = two_phase {
                        if t && !d.two_phase {
                            d.two_phase_at = d.confirmed_flush.max(at.min(d.confirmed_flush));
                        }
                        d.two_phase = t;
                    }
                })
            })();
            r.slots.release(&name, pid);
            res?;
            complete(w, "ALTER_REPLICATION_SLOT");
        }
        Cmd::ReadSlot(name) => {
            let row = match r.slots.get(&name).filter(|s| !s.creating) {
                None => vec![None, None, None],
                Some(sl) if sl.logical() => {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "cannot use READ_REPLICATION_SLOT with a logical replication slot",
                    ));
                }
                Some(sl) => vec![
                    Some("physical".into()),
                    (sl.data.restart_lsn != 0).then(|| super::lsn::fmt(sl.data.restart_lsn)),
                    (sl.data.restart_lsn != 0).then(|| "1".into()),
                ],
            };
            result(w, &[("slot_type", TEXT), ("restart_lsn", TEXT), ("restart_tli", INT8)], &[row]);
            complete(w, "READ_REPLICATION_SLOT");
        }
        Cmd::DropSlot { name, wait } => {
            let cancel = s.cancel.clone();
            let d = r.slots.drop_slot(&name, s.state.pid, wait, &|| {
                if cancel.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    return Err(Error::new(code::QUERY_CANCELED, "canceling statement due to user request"));
                }
                Ok(())
            })?;
            if d.plugin.is_some() {
                r.slots_changed(d.database);
            }
            complete(w, "DROP_REPLICATION_SLOT");
        }
        Cmd::StartPhysical { slot, .. } => {
            prevent_in_block(s, "START_REPLICATION")?;
            if let Some(n) = &slot {
                match r.slots.get(n) {
                    None => return Err(super::slot::no_slot(n)),
                    Some(sl) if sl.logical() => {
                        return Err(Error::new(
                            code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                            "cannot use a logical replication slot for physical replication",
                        ));
                    }
                    _ => {}
                }
            }
            return Err(physical_unsupported("physical replication"));
        }
        Cmd::StartLogical { slot, lsn, options } => {
            prevent_in_block(s, "START_REPLICATION")?;
            if mode != Mode::Database {
                return Err(Error::new(
                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "logical decoding requires a database connection",
                ));
            }
            stream_logical(w, s, &r, &slot, lsn, &options)?;
            complete(w, "START_REPLICATION");
        }
        Cmd::BaseBackup => {
            prevent_in_block(s, "BASE_BACKUP")?;
            return Err(physical_unsupported("BASE_BACKUP"));
        }
        Cmd::UploadManifest => {
            prevent_in_block(s, "UPLOAD_MANIFEST")?;
            return Err(physical_unsupported("UPLOAD_MANIFEST"));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn create_slot(
    w: &mut dyn Wire,
    s: &mut Session,
    r: &Arc<Repl>,
    mode: Mode,
    name: String,
    temporary: bool,
    kind: SlotKind,
    snapshot: Option<String>,
    two_phase: bool,
    failover: bool,
    exported: &mut Option<Exported>,
) -> Result<()> {
    let pid = s.state.pid;
    let plugin = match kind {
        SlotKind::Physical { reserve_wal } => {
            r.check_slots()?;
            r.slots.create(super::slot::SlotData { name: name.clone(), temporary, ..Default::default() }, pid)?;
            let lsn = if reserve_wal { r.wal.current() } else { 0 };
            r.slots.finish_create(&name, |d| d.restart_lsn = lsn)?;
            if !temporary {
                r.slots.release(&name, pid);
            }
            let at = if lsn == 0 { "0/0".to_string() } else { super::lsn::fmt(lsn) };
            result(
                w,
                &[("slot_name", TEXT), ("consistent_point", TEXT), ("snapshot_name", TEXT), ("output_plugin", TEXT)],
                &[vec![Some(name), Some(at), None, None]],
            );
            return Ok(());
        }
        SlotKind::Logical { plugin } => plugin,
    };
    r.check_logical()?;
    if mode != Mode::Database {
        return Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, "logical decoding requires a database connection"));
    }
    let action = match snapshot.as_deref() {
        None | Some("export") => "export",
        Some("use") => "use",
        Some("nothing") => "nothing",
        Some(v) => {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                format!("unrecognized value for CREATE_REPLICATION_SLOT option \"snapshot\": \"{v}\""),
            ));
        }
    };
    let what = |a: &str| format!("CREATE_REPLICATION_SLOT ... (SNAPSHOT '{a}')");
    let in_block = s.status() != TxnStatus::Idle;
    let precheck = |s: &mut Session| -> Result<()> {
        match action {
            "export" if in_block => {
                Err(Error::new(code::INTERNAL_ERROR, format!("{} must not be called inside a transaction", what("export"))))
            }
            "use" => {
                if !in_block {
                    return Err(Error::new(code::INTERNAL_ERROR, format!("{} must be called inside a transaction", what("use"))));
                }
                let t = s.block_txn().expect("a transaction block");
                let t = t.borrow();
                if t.isolation != crate::txn::Isolation::RepeatableRead {
                    return Err(Error::new(
                        code::INTERNAL_ERROR,
                        format!("{} must be called in REPEATABLE READ isolation mode transaction", what("use")),
                    ));
                }
                if !t.read_only {
                    return Err(Error::new(code::INTERNAL_ERROR, format!("{} must be called in a read-only transaction", what("use"))));
                }
                if t.snapshot_taken() {
                    return Err(Error::new(code::INTERNAL_ERROR, format!("{} must be called before any query", what("use"))));
                }
                if t.in_savepoint() {
                    return Err(Error::new(code::INTERNAL_ERROR, format!("{} must not be called in a subtransaction", what("use"))));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    };
    precheck(s)?;
    let db = s.state.db.clone();
    let xid = s.block_txn().and_then(|t| t.borrow().xid_if_assigned());
    let cancel = s.cancel.clone();
    let made = {
        let settings = s.state.settings.borrow();
        super::sqlfn::create_logical(&db, r, pid, xid, &settings, &name, &plugin, temporary, two_phase, failover, &|| {
            if cancel.swap(false, std::sync::atomic::Ordering::Relaxed) {
                return Err(Error::new(code::QUERY_CANCELED, "canceling statement due to user request"));
            }
            Ok(())
        })?
    };
    let snap_name = match action {
        "export" => {
            let owner = (1u64 << 62) | pid as u64;
            let xids = db.xids();
            let n = xids.exported_by(db.instance, owner) + 1;
            let id = format!("{:08X}-{:08X}-{n}", pid as u32, owner as u32);
            xids.export(
                id.clone(),
                crate::txn::xids::Exported {
                    db: db.instance,
                    owner,
                    db_oid: db.oid,
                    ts: made.snapshot_ts,
                    serializable: false,
                    read_only: true,
                    pin: db.store.pin(made.snapshot_ts),
                },
            );
            *exported = Some(Exported { db_instance: db.instance, owner });
            Some(id)
        }
        "use" => {
            s.block_txn().expect("a transaction block").borrow_mut().use_snapshot(made.snapshot_ts);
            None
        }
        _ => None,
    };
    if !temporary {
        r.slots.release(&name, pid);
    }
    result(
        w,
        &[("slot_name", TEXT), ("consistent_point", TEXT), ("snapshot_name", TEXT), ("output_plugin", TEXT)],
        &[vec![Some(name), Some(super::lsn::fmt(made.lsn)), snap_name, Some(plugin)]],
    );
    Ok(())
}

fn xlog_data(lsn: u64, end: u64, data: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(25 + data.len());
    b.push(b'w');
    b.extend_from_slice(&lsn.to_be_bytes());
    b.extend_from_slice(&end.to_be_bytes());
    b.extend_from_slice(&super::log::now().to_be_bytes());
    b.extend_from_slice(data);
    b
}

fn keepalive(end: u64, reply: bool) -> Vec<u8> {
    let mut b = Vec::with_capacity(18);
    b.push(b'k');
    b.extend_from_slice(&end.to_be_bytes());
    b.extend_from_slice(&super::log::now().to_be_bytes());
    b.push(reply as u8);
    b
}

fn be64(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8).map_or(0, |x| u64::from_be_bytes(x.try_into().unwrap()))
}

/// StartLogicalReplication and WalSndLoop(XLogSendLogical).
fn stream_logical(w: &mut dyn Wire, s: &mut Session, r: &Arc<Repl>, slot: &str, start: u64, opts: &[(String, Option<String>)]) -> Result<()> {
    let pid = s.state.pid;
    let db = s.state.db.clone();
    let sl = r.slots.acquire(slot, pid, false, &|| Ok(()))?;
    let res = stream_acquired(w, s, r, &db, &sl, start, opts);
    r.slots.release(slot, pid);
    r.senders.lock().retain(|x| x.pid != pid);
    r.slots_changed(db.oid);
    res
}

fn stream_acquired(
    w: &mut dyn Wire,
    s: &mut Session,
    r: &Arc<Repl>,
    db: &Arc<crate::txn::Database>,
    sl: &super::slot::Slot,
    start: u64,
    opts: &[(String, Option<String>)],
) -> Result<()> {
    let name = sl.data.name.clone();
    let Some(plugin) = sl.data.plugin.clone() else {
        return Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, "cannot use physical replication slot for logical decoding"));
    };
    if sl.data.database != db.oid {
        return Err(Error::new(
            code::OBJECT_NOT_IN_PREREQUISITE_STATE,
            format!("replication slot \"{name}\" was not created in this database"),
        ));
    }
    if let Some(why) = &sl.data.invalidated {
        return Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, format!("can no longer get changes from replication slot \"{name}\""))
            .detail(format!("This replication slot has been invalidated due to \"{why}\".")));
    }
    let (fmt, work_mem, timeout) = {
        let st = s.state.settings.borrow();
        super::sqlfn::check_plugin_allowed(&plugin, &st)?;
        let fmt = st.format_ctx();
        let t = st.get_str("wal_sender_timeout").and_then(|v| v.parse::<u64>().ok()).unwrap_or(60000);
        (fmt, super::sqlfn::work_mem(&st), t)
    };
    // (A local handle: the decoder borrows it no longer than the row filter borrows `s`.)
    let db = &db.clone();
    let mut dec = Decoder::new(
        db,
        &name,
        Setup {
            plugin: &plugin,
            opts,
            is_init: false,
            slot_two_phase: sl.data.two_phase,
            two_phase_at: sl.data.two_phase_at,
            fmt,
            work_mem,
        },
    )?;
    let pid = s.state.pid;
    let cancel = s.cancel.clone();
    // Publications' row filters, run by this session as queries over the row.
    let mut eval = |rel: &super::log::RelDesc, qual: &str, row: &[crate::types::Value]| row_filter(s, rel, qual, row);
    dec.ctx.filter = Some(&mut eval);
    // CopyBothResponse.
    w.put(b'W', &[0, 0, 0]);
    w.flush()?;
    let mut pos = start.max(sl.data.confirmed_flush);
    let mut confirmed = sl.data.confirmed_flush;
    let mut saved = confirmed;
    let mut sent = confirmed;
    let mut last_save = Instant::now();
    let mut last_recv = Instant::now();
    let mut last_keepalive = Instant::now();
    let mut ping_sent = false;
    r.senders.lock().push(Sender { pid, state: "catchup", sent_lsn: sent, slot: name.clone(), ..Default::default() });
    let update = |f: &dyn Fn(&mut Sender)| {
        if let Some(x) = r.senders.lock().iter_mut().find(|x| x.pid == pid) {
            f(x);
        }
    };
    let half = Duration::from_millis(timeout / 2);
    loop {
        let seen = db.log.commit_count();
        let end = db.log.safe_end(&r.wal);
        if end > pos {
            for m in super::decode::markers(&db.store, pos, end) {
                let m = m?;
                let mut err = None;
                dec.decode(&m, &mut |o| {
                    w.put(b'd', &xlog_data(o.lsn, end, &o.data));
                    if let Err(e) = w.flush() {
                        err = Some(e);
                    }
                    Ok(())
                })?;
                if let Some(e) = err {
                    return Err(e.into());
                }
            }
            pos = end;
            if sent < end {
                sent = end;
                update(&|x: &mut Sender| x.sent_lsn = end);
            }
        }
        update(&|x: &mut Sender| x.state = "streaming");
        // The client's messages.
        let wait = if db.log.commit_count() != seen { Duration::ZERO } else { Duration::from_millis(20) };
        let mut wait = wait;
        loop {
            let Some((t, body)) = w.poll(wait)? else { break };
            wait = Duration::ZERO;
            last_recv = Instant::now();
            ping_sent = false;
            match t {
                b'd' if body.first() == Some(&b'r') => {
                    let (wr, fl, ap, reply) = (be64(&body, 1), be64(&body, 9), be64(&body, 17), body.get(33) == Some(&1));
                    update(&|x: &mut Sender| {
                        x.write_lsn = wr;
                        x.flush_lsn = fl;
                        x.replay_lsn = ap;
                        x.reply_time = Some(super::log::now());
                    });
                    if fl > confirmed {
                        confirmed = fl.min(sent.max(confirmed));
                    }
                    if reply {
                        w.put(b'd', &keepalive(sent, false));
                        w.flush()?;
                    }
                }
                b'd' => {}
                b'c' => {
                    w.put(b'c', &[]);
                    if confirmed > saved {
                        save(r, db, &name, confirmed);
                    }
                    add_stats(r, &name, &mut dec);
                    complete(w, "COPY 0");
                    return Ok(());
                }
                b'X' => {
                    if confirmed > saved {
                        save(r, db, &name, confirmed);
                    }
                    return Err(Error::new(code::CONNECTION_EXCEPTION, "client ended the connection").fatal());
                }
                t => {
                    return Err(Error::new(code::PROTOCOL_VIOLATION, format!("unexpected message type \"{}\"", t as char)).fatal());
                }
            }
        }
        if confirmed > saved && last_save.elapsed() >= Duration::from_secs(1) {
            save(r, db, &name, confirmed);
            add_stats(r, &name, &mut dec);
            saved = confirmed;
            last_save = Instant::now();
        }
        // Keepalives: a caught-up client learns where the WAL is, and one silent for half
        // of wal_sender_timeout is asked to reply.
        if timeout > 0 && last_recv.elapsed() >= half && !ping_sent {
            w.put(b'd', &keepalive(sent, true));
            w.flush()?;
            ping_sent = true;
            last_keepalive = Instant::now();
        } else if last_keepalive.elapsed() >= Duration::from_secs(10) {
            w.put(b'd', &keepalive(sent, false));
            w.flush()?;
            last_keepalive = Instant::now();
        }
        if timeout > 0 && last_recv.elapsed() >= Duration::from_millis(timeout) {
            return Err(Error::new(code::CONNECTION_EXCEPTION, "terminating walsender process due to replication timeout").fatal());
        }
        if cancel.swap(false, std::sync::atomic::Ordering::Relaxed) {
            return Err(Error::new(code::QUERY_CANCELED, "canceling statement due to user request"));
        }
        if db.log.commit_count() == seen {
            db.log.wait_commit(seen, Duration::from_millis(30));
        }
    }
}

/// A publication's row filter over a row (pgoutput_row_filter_exec_expr): the filter's SQL
/// run over the row's values, cast to the relation's column types. NULL is false.
fn row_filter(s: &mut Session, rel: &super::log::RelDesc, qual: &str, row: &[crate::types::Value]) -> Result<bool> {
    use crate::catalog::quote_ident;
    let fmt = s.format_ctx();
    let cols: Vec<String> = rel
        .cols
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.dropped)
        .map(|(i, c)| {
            let v = row.get(i).unwrap_or(&crate::types::Value::Null);
            let lit = if v.is_null() {
                "NULL".to_string()
            } else {
                crate::sql::func::string::quote_literal_str(&crate::types::text::format_value(v, &c.ty, &fmt))
            };
            format!("{lit}::{} AS {}", c.ty.sql_name(), quote_ident(&c.name))
        })
        .collect();
    let sql = format!("SELECT ({qual})::boolean FROM (SELECT {}) AS {}", cols.join(", "), quote_ident(&rel.name));
    let res = s.query(&sql)?;
    Ok(matches!(res.first().and_then(|r| r.rows.first()).and_then(|r| r.first()), Some(crate::types::Value::Bool(true))))
}

fn save(r: &Repl, db: &crate::txn::Database, slot: &str, confirmed: u64) {
    let catalog_xmin = db.xids().snapshot(None).xmin;
    let _ = r.slots.update(slot, false, |d| {
        d.confirmed_flush = d.confirmed_flush.max(confirmed);
        d.restart_lsn = d.restart_lsn.max(confirmed);
        d.catalog_xmin = catalog_xmin;
    });
    r.slots_changed(db.oid);
}

fn add_stats(r: &Repl, slot: &str, dec: &mut Decoder) {
    super::sqlfn::add_stats(r, slot, &dec.stats);
    dec.stats = Default::default();
}
