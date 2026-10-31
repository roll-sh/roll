//! Logical replication: the client-visible parts of PostgreSQL's logical decoding and
//! replication over the engine's own commit path (HANDOFF "Logical replication").
//!
//! - `lsn`: the cluster's WAL positions (pg_lsn values clients see).
//! - `slot`: replication slots, persistent and temporary.
//! - `log`: the change stream's capture: change records written by transactions while a
//!   database has logical slots, and a commit record per transaction.
//! - `decode`: reading committed transactions back in commit order; `plugin` drives an
//!   output plugin over them as logical.c / reorderbuffer.c drive one (streaming large
//!   transactions, two-phase commit, origins).
//! - `test_decoding`, `pgoutput`: the output plugins.
//! - `sqlfn`: the SQL interface (slot functions, pg_logical_slot_get_changes, ...).
//! - `walsender`: the replication protocol (`replication=database` connections).

pub mod apply;
mod apply_rows;
mod apply_stream;
pub mod cmd;
pub mod commit_ts;
pub mod decode;
pub mod log;
pub mod lsn;
pub mod origin;
pub mod pgoutput;
pub mod plugin;
pub mod slot;
pub mod sqlfn;
pub mod test_decoding;
pub mod walrcv;
pub mod walsender;

use crate::error::{Error, Result, code};
use crate::txn::Database;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};

/// The cluster's system identifier (IDENTIFY_SYSTEM's systemid; part of a table
/// synchronization slot's name, ReplicationSlotNameForTablesync).
pub const SYSTEM_IDENTIFIER: &str = "7000000000000000001";

/// wal_level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WalLevel {
    Minimal,
    Replica,
    Logical,
}

impl WalLevel {
    pub fn parse(s: &str) -> Option<WalLevel> {
        Some(match s {
            "minimal" => WalLevel::Minimal,
            "replica" | "archive" | "hot_standby" => WalLevel::Replica,
            "logical" => WalLevel::Logical,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            WalLevel::Minimal => "minimal",
            WalLevel::Replica => "replica",
            WalLevel::Logical => "logical",
        }
    }
}

/// A walsender, as pg_stat_replication shows it.
#[derive(Clone, Debug, Default)]
pub struct Sender {
    pub pid: i32,
    pub state: &'static str,
    pub sent_lsn: u64,
    pub write_lsn: u64,
    pub flush_lsn: u64,
    pub replay_lsn: u64,
    /// When the last standby status update came (timestamptz microseconds).
    pub reply_time: Option<i64>,
    pub slot: String,
}

/// The cluster's replication state.
pub struct Repl {
    pub wal: Arc<lsn::WalPos>,
    pub slots: slot::Slots,
    pub wal_level: WalLevel,
    pub max_wal_senders: usize,
    pub origins: origin::Origins,
    /// track_commit_timestamp = on (a server-start setting): the commit timestamps' state.
    pub commit_ts: Option<commit_ts::CommitTs>,
    /// The open databases, by oid (to switch their change capture on and off).
    dbs: Mutex<HashMap<u32, Weak<Database>>>,
    pub senders: Mutex<Vec<Sender>>,
}

impl Repl {
    pub fn new(wal: lsn::WalPos, slots: slot::Slots, origins: origin::Origins, wal_level: WalLevel, max_wal_senders: usize) -> Repl {
        Repl {
            wal: Arc::new(wal),
            slots,
            wal_level,
            max_wal_senders,
            origins,
            commit_ts: None,
            dbs: Mutex::new(HashMap::new()),
            senders: Mutex::new(Vec::new()),
        }
    }

    pub fn with_commit_ts(mut self, on: bool) -> Repl {
        self.commit_ts = on.then(Default::default);
        self
    }

    /// A database opened: it logs changes while it has logical slots.
    pub fn attach(&self, db: &Arc<Database>) {
        self.dbs.lock().insert(db.oid, Arc::downgrade(db));
        db.log.commit_ts.store(self.commit_ts.is_some(), Ordering::Relaxed);
        let on = !self.slots.logical_in(db.oid).is_empty();
        db.log.capture.store(on, Ordering::SeqCst);
        if !on {
            // What a crash or the last slot's drop left behind.
            if let Ok(w) = log::gc_writes(&db.store, u64::MAX)
                && !w.is_empty()
            {
                let _ = db.store.commit(w, |_| Ok(()));
            }
        }
    }

    pub fn detach(&self, db: &Database) {
        let mut d = self.dbs.lock();
        if d.get(&db.oid).is_some_and(|w| w.upgrade().is_none_or(|x| std::ptr::eq(&*x, db))) {
            d.remove(&db.oid);
        }
    }

    pub fn open_db(&self, oid: u32) -> Option<Arc<Database>> {
        self.dbs.lock().get(&oid).and_then(Weak::upgrade)
    }

    pub fn open_dbs(&self) -> Vec<Arc<Database>> {
        self.dbs.lock().values().filter_map(Weak::upgrade).collect()
    }

    /// Database `oid`'s slots changed: capture follows, and what no slot needs any more is
    /// removed.
    pub fn slots_changed(&self, oid: u32) {
        let Some(db) = self.open_db(oid) else { return };
        let slots = self.slots.logical_in(oid);
        if slots.is_empty() {
            db.log.capture.store(false, Ordering::SeqCst);
        }
        db.log.wake();
        let upto = if slots.is_empty() {
            u64::MAX
        } else if slots.iter().any(|s| s.creating) {
            return;
        } else {
            slots.iter().map(|s| s.data.confirmed_flush).min().unwrap_or(0)
        };
        self.gc(&db, upto);
    }

    /// Remove the change stream's records ending at or before `upto` (what every slot of
    /// the database confirmed), unless done already.
    pub fn gc(&self, db: &Database, upto: u64) {
        if upto <= db.log.gc_upto.load(Ordering::Acquire) {
            return;
        }
        match log::gc_writes(&db.store, upto) {
            Ok(w) => {
                if !w.is_empty() && db.store.commit(w, |_| Ok(())).is_err() {
                    return;
                }
                db.log.gc_upto.fetch_max(upto, Ordering::AcqRel);
            }
            Err(e) => tracing::warn!(error = %e, "removing confirmed change stream records failed"),
        }
    }

    /// CheckLogicalDecodingRequirements.
    pub fn check_logical(&self) -> Result<()> {
        self.check_slots()?;
        if self.wal_level < WalLevel::Logical {
            return Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "logical decoding requires \"wal_level\" >= \"logical\"",
            ));
        }
        Ok(())
    }

    /// CheckSlotRequirements.
    pub fn check_slots(&self) -> Result<()> {
        if self.slots.max == 0 {
            return Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "replication slots can only be used if \"max_replication_slots\" > 0",
            ));
        }
        if self.wal_level < WalLevel::Replica {
            return Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "replication slots can only be used if \"wal_level\" >= \"replica\"",
            ));
        }
        Ok(())
    }
}

/// CheckSlotPermissions: the REPLICATION attribute (or superuser).
pub fn check_permissions(cat: &crate::catalog::Catalog, role: u32) -> Result<()> {
    let ok = cat.roles.get(&role).is_some_and(|r| r.superuser || r.replication);
    if ok {
        return Ok(());
    }
    Err(Error::new(code::INSUFFICIENT_PRIVILEGE, "permission denied to use replication slots")
        .detail("Only roles with the REPLICATION attribute may use replication slots."))
}
