//! Replication origins (origin.c): named origins of replicated changes, a session's origin
//! (pg_replication_origin_session_setup) stamped on the transactions it commits, and each
//! origin's progress. Origins are the cluster's, stored in `<data>/replorigins.json`; a
//! commit's progress also goes into its own database's store in the same commit (under
//! `log::PREFIX`), so an apply worker resumes exactly after what it committed.

use super::log::PREFIX;
use crate::error::{Error, Result, code};
use crate::sql::func::{FnCtx, Registry, SrfIter, text_arg};
use crate::types::Value;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Origin {
    pub id: u16,
    pub name: String,
    pub remote_lsn: u64,
    pub local_lsn: u64,
}

/// A backend's session origin (replorigin_session_*).
#[derive(Clone, Copy, Debug, Default)]
pub struct SessionOrigin {
    pub id: u16,
    /// pg_replication_origin_xact_setup's: the remote commit's position and time.
    pub xact_lsn: u64,
    pub xact_time: i64,
}

#[derive(Default)]
struct State {
    origins: Vec<Origin>,
    sessions: HashMap<i32, SessionOrigin>,
}

pub struct Origins {
    st: Mutex<State>,
    /// How many sessions have an origin set up (a commit of a database with none skips the lock).
    sessions_n: AtomicUsize,
    path: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Default)]
struct Stored {
    origins: Vec<Origin>,
}

/// The key of origin `id`'s progress in a database's store.
pub fn progress_key(id: u16) -> Vec<u8> {
    let mut k = PREFIX.to_vec();
    k.push(b'O');
    k.extend_from_slice(&id.to_be_bytes());
    k
}

/// pg_replication_origin_create's check of the name (IsReservedOriginName); internal callers
/// such as subscriptions use reserved `pg_<oid>` names.
fn check_reserved(name: &str) -> Result<()> {
    if name.eq_ignore_ascii_case("any") || name.eq_ignore_ascii_case("none") || name.starts_with("pg_") {
        return Err(Error::new(code::RESERVED_NAME, format!("replication origin name \"{name}\" is reserved"))
            .detail("Origin names \"any\", \"none\", and names starting with \"pg_\" are reserved."));
    }
    Ok(())
}

/// replorigin_create's own check: MAX_RONAME_LEN.
fn check_len(name: &str) -> Result<()> {
    if name.len() > 512 {
        return Err(Error::new(code::PROGRAM_LIMIT_EXCEEDED, "replication origin name is too long")
            .detail("Replication origin names must be no longer than 512 bytes."));
    }
    Ok(())
}

fn no_origin(name: &str) -> Error {
    Error::new(code::UNDEFINED_OBJECT, format!("replication origin \"{name}\" does not exist"))
}

impl Origins {
    pub fn open(dir: Option<PathBuf>) -> Origins {
        let path = dir.map(|d| d.join("replorigins.json"));
        let origins = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<Stored>(&b).ok())
            .map(|s| s.origins)
            .unwrap_or_default();
        Origins { st: Mutex::new(State { origins, sessions: HashMap::new() }), sessions_n: AtomicUsize::new(0), path }
    }

    fn save(&self, st: &State) {
        let Some(p) = &self.path else { return };
        let tmp = p.with_extension("json.tmp");
        if let Ok(b) = serde_json::to_vec(&Stored { origins: st.origins.clone() })
            && std::fs::write(&tmp, b).is_ok()
        {
            let _ = std::fs::rename(&tmp, p);
        }
    }

    pub fn list(&self) -> Vec<Origin> {
        self.st.lock().origins.clone()
    }

    pub fn by_name(&self, name: &str) -> Option<Origin> {
        self.st.lock().origins.iter().find(|o| o.name == name).cloned()
    }

    pub fn name_of(&self, id: u16) -> Option<String> {
        self.st.lock().origins.iter().find(|o| o.id == id).map(|o| o.name.clone())
    }

    /// replorigin_create.
    pub fn create(&self, name: &str) -> Result<u16> {
        check_len(name)?;
        let mut st = self.st.lock();
        if st.origins.iter().any(|o| o.name == name) {
            return Err(Error::new(
                "23505",
                "duplicate key value violates unique constraint \"pg_replication_origin_roname_index\"",
            )
            .detail(format!("Key (roname)=({name}) already exists.")));
        }
        let Some(id) = (1..=u16::MAX).find(|i| !st.origins.iter().any(|o| o.id == *i)) else {
            return Err(Error::new(code::PROGRAM_LIMIT_EXCEEDED, "could not find free replication origin ID"));
        };
        st.origins.push(Origin { id, name: name.to_string(), remote_lsn: 0, local_lsn: 0 });
        self.save(&st);
        Ok(id)
    }

    /// replorigin_drop_by_name.
    pub fn drop_origin(&self, name: &str, missing_ok: bool) -> Result<()> {
        let mut st = self.st.lock();
        let Some(o) = st.origins.iter().find(|o| o.name == name).cloned() else {
            return if missing_ok { Ok(()) } else { Err(no_origin(name)) };
        };
        if let Some((pid, _)) = st.sessions.iter().find(|(_, s)| s.id == o.id) {
            return Err(Error::new(
                code::OBJECT_IN_USE,
                format!("could not drop replication origin with ID {}, in use by PID {pid}", o.id),
            ));
        }
        st.origins.retain(|x| x.id != o.id);
        self.save(&st);
        Ok(())
    }

    /// Record progress: the remote commit `remote` is applied locally at `local`.
    pub fn advance(&self, id: u16, remote: u64, local: u64, go_backward: bool) {
        let mut st = self.st.lock();
        if let Some(o) = st.origins.iter_mut().find(|o| o.id == id) {
            if go_backward || remote > o.remote_lsn {
                o.remote_lsn = remote;
            }
            if local > o.local_lsn {
                o.local_lsn = local;
            }
        }
        self.save(&st);
    }

    pub fn session(&self, pid: i32) -> Option<SessionOrigin> {
        if self.sessions_n.load(Ordering::Acquire) == 0 {
            return None;
        }
        self.st.lock().sessions.get(&pid).copied()
    }

    /// replorigin_session_setup.
    pub fn session_setup(&self, pid: i32, name: &str, acquired_by: i32) -> Result<()> {
        let mut st = self.st.lock();
        if st.sessions.contains_key(&pid) {
            return Err(Error::new(code::OBJECT_IN_USE, "cannot setup replication origin when one is already setup"));
        }
        let Some(o) = st.origins.iter().find(|o| o.name == name).cloned() else { return Err(no_origin(name)) };
        if let Some((other, _)) = st.sessions.iter().find(|(p, s)| s.id == o.id && **p != acquired_by) {
            return Err(Error::new(
                code::OBJECT_IN_USE,
                format!("replication origin with ID {} is already active for PID {other}", o.id),
            ));
        }
        st.sessions.insert(pid, SessionOrigin { id: o.id, ..Default::default() });
        self.sessions_n.store(st.sessions.len(), Ordering::Release);
        Ok(())
    }

    pub fn session_reset(&self, pid: i32) -> Result<()> {
        let mut st = self.st.lock();
        let removed = st.sessions.remove(&pid);
        self.sessions_n.store(st.sessions.len(), Ordering::Release);
        if removed.is_none() {
            return Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, "no replication origin is configured"));
        }
        Ok(())
    }

    pub fn xact_setup(&self, pid: i32, lsn: u64, time: i64) -> Result<()> {
        let mut st = self.st.lock();
        let Some(s) = st.sessions.get_mut(&pid) else {
            return Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, "no replication origin is configured"));
        };
        s.xact_lsn = lsn;
        s.xact_time = time;
        Ok(())
    }

    pub fn xact_reset(&self, pid: i32) {
        if let Some(s) = self.st.lock().sessions.get_mut(&pid) {
            s.xact_lsn = 0;
            s.xact_time = 0;
        }
    }

    /// A backend ended: its session origin is released.
    pub fn backend_exit(&self, pid: i32) {
        if self.sessions_n.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut st = self.st.lock();
        st.sessions.remove(&pid);
        self.sessions_n.store(st.sessions.len(), Ordering::Release);
    }
}

/// Before a session's transaction commits: its origin goes on its commit record, and the
/// origin's progress into the same commit.
pub fn before_commit(sess: &crate::session::state::SessionState, txn: &mut crate::txn::Txn) -> Option<(u16, u64)> {
    let r = sess.db.repl()?;
    let so = r.origins.session(sess.pid)?;
    txn.xid_if_assigned()?;
    if let Some(l) = txn.log.as_mut() {
        l.origin = so.id;
        l.origin_lsn = so.xact_lsn;
    }
    if so.xact_lsn != 0 {
        let _ = txn.put_log(progress_key(so.id), so.xact_lsn.to_be_bytes().to_vec());
    }
    Some((so.id, so.xact_lsn))
}

/// After it committed: the origin's progress in memory (replorigin_session_advance: the local
/// position moves with every commit, the remote one only with a session origin position).
pub fn after_commit(sess: &crate::session::state::SessionState, (id, lsn): (u16, u64)) {
    if let Some(r) = sess.db.repl() {
        r.origins.advance(id, lsn, r.wal.current(), false);
    }
}

// ── SQL functions ──

fn origins(c: &FnCtx) -> Result<std::sync::Arc<super::Repl>> {
    c.exec.sess.db.repl().ok_or_else(|| Error::new(code::FEATURE_NOT_SUPPORTED, "replication origins are not available"))
}

fn lsn(v: &Value) -> u64 {
    match v {
        Value::PgLsn(l) => *l,
        _ => 0,
    }
}

fn create(a: &[Value], c: &FnCtx) -> Result<Value> {
    check_reserved(text_arg(&a[0]))?;
    // (replorigin_create's own check, before anything else can fail.)
    check_len(text_arg(&a[0]))?;
    Ok(Value::Oid(origins(c)?.origins.create(text_arg(&a[0]))? as u32))
}
fn drop_fn(a: &[Value], c: &FnCtx) -> Result<Value> {
    origins(c)?.origins.drop_origin(text_arg(&a[0]), false)?;
    Ok(Value::Null)
}
fn oid(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(origins(c)?.origins.by_name(text_arg(&a[0])).map_or(Value::Null, |o| Value::Oid(o.id as u32)))
}
fn session_setup(a: &[Value], c: &FnCtx) -> Result<Value> {
    let pid = a.get(1).and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    origins(c)?.origins.session_setup(c.exec.sess.pid, text_arg(&a[0]), pid)?;
    Ok(Value::Null)
}
fn session_reset(_: &[Value], c: &FnCtx) -> Result<Value> {
    origins(c)?.origins.session_reset(c.exec.sess.pid)?;
    Ok(Value::Null)
}
fn session_is_setup(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(origins(c)?.origins.session(c.exec.sess.pid).is_some()))
}
fn session_progress(_: &[Value], c: &FnCtx) -> Result<Value> {
    let r = origins(c)?;
    let Some(s) = r.origins.session(c.exec.sess.pid) else {
        return Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, "no replication origin is configured"));
    };
    let o = r.origins.list().into_iter().find(|o| o.id == s.id);
    Ok(o.filter(|o| o.remote_lsn != 0).map_or(Value::Null, |o| Value::PgLsn(o.remote_lsn)))
}
fn xact_setup(a: &[Value], c: &FnCtx) -> Result<Value> {
    let t = match a.get(1) {
        Some(Value::Timestamptz(t)) => *t,
        _ => 0,
    };
    origins(c)?.origins.xact_setup(c.exec.sess.pid, lsn(&a[0]), t)?;
    Ok(Value::Null)
}
fn xact_reset(_: &[Value], c: &FnCtx) -> Result<Value> {
    origins(c)?.origins.xact_reset(c.exec.sess.pid);
    Ok(Value::Null)
}
fn advance(a: &[Value], c: &FnCtx) -> Result<Value> {
    let r = origins(c)?;
    let name = text_arg(&a[0]);
    let Some(o) = r.origins.by_name(name) else { return Err(no_origin(name)) };
    r.origins.advance(o.id, lsn(&a[1]), 0, true);
    Ok(Value::Null)
}
fn progress(a: &[Value], c: &FnCtx) -> Result<Value> {
    let r = origins(c)?;
    let name = text_arg(&a[0]);
    let Some(o) = r.origins.by_name(name) else { return Err(no_origin(name)) };
    Ok(if o.remote_lsn == 0 { Value::Null } else { Value::PgLsn(o.remote_lsn) })
}
fn status(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let rows: Vec<Vec<Value>> = match c.exec.sess.db.repl() {
        None => Vec::new(),
        Some(r) => r
            .origins
            .list()
            .into_iter()
            .map(|o| {
                vec![
                    Value::Oid(o.id as u32),
                    Value::Text(o.name.into()),
                    Value::PgLsn(o.remote_lsn),
                    Value::PgLsn(o.local_lsn),
                ]
            })
            .collect(),
    };
    Ok(Box::new(rows.into_iter().map(Ok)))
}

pub fn register(r: &mut Registry) {
    r.from_pg(6003, Some(create), None);
    r.from_pg(6004, Some(drop_fn), None);
    r.from_pg(6005, Some(oid), None);
    r.from_pg(6006, Some(session_setup), None);
    r.from_pg(6007, Some(session_reset), None);
    r.from_pg(6008, Some(session_is_setup), None);
    r.from_pg(6009, Some(session_progress), None);
    r.from_pg(6010, Some(xact_setup), None);
    r.from_pg(6011, Some(xact_reset), None);
    r.from_pg(6012, Some(advance), None);
    r.from_pg(6013, Some(progress), None);
    r.from_pg(6014, None, Some(status));
}
