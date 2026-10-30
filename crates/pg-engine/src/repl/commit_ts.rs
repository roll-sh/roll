//! track_commit_timestamp (commit_ts.c): each committed transaction's commit time and
//! replication origin, for pg_xact_commit_timestamp[_origin], pg_last_committed_xact and the
//! apply worker's origin-differs conflicts.
//!
//! PostgreSQL keeps them in an SLRU filled from the commit record. Here a transaction that
//! commits with an xid while the setting is on writes one small record per (sub)transaction id
//! into its own commit (`before_commit`): durable and atomic with it, no extra I/O or fsync,
//! and nothing at all while the setting is off (a server-start setting, as in PostgreSQL).

use crate::error::{Error, Result, code};
use crate::session::state::SessionState;
use crate::txn::Txn;
use parking_lot::Mutex;

/// Reserved keyspace (table oid 0) of the commit timestamps: `PREFIX xid`.
const PREFIX: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 0x32];

fn key(xid: u64) -> Vec<u8> {
    let mut k = PREFIX.to_vec();
    k.extend_from_slice(&xid.to_be_bytes());
    k
}

/// A commit's timestamp and origin (0: a local commit).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stamp {
    pub ts: i64,
    pub origin: u16,
}

impl Stamp {
    fn bytes(self) -> Vec<u8> {
        let mut b = self.ts.to_be_bytes().to_vec();
        b.extend_from_slice(&self.origin.to_be_bytes());
        b
    }

    fn parse(b: &[u8]) -> Option<Stamp> {
        Some(Stamp {
            ts: i64::from_be_bytes(b.get(..8)?.try_into().ok()?),
            origin: u16::from_be_bytes(b.get(8..10)?.try_into().ok()?),
        })
    }
}

/// The cluster's commit timestamp state (commitTsShared), present while the setting is on.
#[derive(Default)]
pub struct CommitTs {
    /// xidLastCommit with its data (pg_last_committed_xact).
    last: Mutex<(u64, Stamp)>,
}

/// error_commit_ts_disabled.
pub fn disabled() -> Error {
    Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, "could not get commit timestamp data")
        .hint("Make sure the configuration parameter \"track_commit_timestamp\" is set.")
}

impl CommitTs {
    /// pg_last_committed_xact's (xid, data); xid 0 before any commit.
    pub fn last(&self) -> (u64, Stamp) {
        *self.last.lock()
    }
}

/// Before a transaction commits (RecordTransactionCommit[Prepared]'s
/// TransactionTreeSetCommitTsData): with the setting on and an xid, its commit time (a
/// session origin's remote commit time when it set one) and origin go into the same commit,
/// for it and each committed subtransaction. What `after_commit` publishes.
pub fn before_commit(sess: &SessionState, txn: &mut Txn) -> Option<(u64, Stamp)> {
    if !sess.db.log.commit_ts.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let r = sess.db.repl()?;
    r.commit_ts.as_ref()?;
    let xid = txn.xid_if_assigned()?;
    let so = r.origins.session(sess.pid);
    let origin = so.map_or(0, |o| o.id);
    let ts = match so {
        Some(o) if o.xact_time != 0 => o.xact_time,
        _ => crate::types::datetime::Clock::system().now,
    };
    let stamp = Stamp { ts, origin };
    for x in std::iter::once(xid).chain(txn.committed_subxids()) {
        txn.put_log(key(x), stamp.bytes()).ok()?;
    }
    Some((xid, stamp))
}

/// After the commit: it is the latest (xidLastCommit).
pub fn after_commit(sess: &SessionState, (xid, stamp): (u64, Stamp)) {
    if let Some(c) = sess.db.repl().as_ref().and_then(|r| r.commit_ts.as_ref()) {
        let mut l = c.last.lock();
        if xid > l.0 {
            *l = (xid, stamp);
        }
    }
}

/// TransactionIdGetCommitTsData: the commit data of `xid` as the database's store has it
/// (None: not committed with the setting on, or still running). Errors as PostgreSQL's: an
/// invalid xid always, the setting off for normal ones.
pub fn get(db: &crate::txn::Database, xid: u64) -> Result<Option<Stamp>> {
    if xid == 0 {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, format!("cannot retrieve commit timestamp for transaction {xid}")));
    }
    if xid < crate::txn::xids::FIRST_NORMAL_XID {
        return Ok(None);
    }
    let r = db.repl();
    let Some(c) = r.as_ref().and_then(|r| r.commit_ts.as_ref()) else { return Err(disabled()) };
    let (lx, ls) = c.last();
    if lx == xid {
        return Ok(Some(ls));
    }
    Ok(db.store.get(&key(xid), db.store.visible_ts())?.and_then(|b| Stamp::parse(&b)))
}

/// `get` for the SQL functions: transaction ids are the cluster's, so one committed in
/// another (open) database is found there.
pub fn get_any(db: &crate::txn::Database, xid: u64) -> Result<Option<Stamp>> {
    if let Some(s) = get(db, xid)? {
        return Ok(Some(s));
    }
    let Some(r) = db.repl() else { return Ok(None) };
    for other in r.open_dbs() {
        if other.oid != db.oid
            && let Some(b) = other.store.get(&key(xid), other.store.visible_ts())?
        {
            return Ok(Stamp::parse(&b));
        }
    }
    Ok(None)
}
