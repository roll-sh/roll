//! Serializable Snapshot Isolation, as predicate.c does it: every SERIALIZABLE transaction
//! that could take part in an anomaly is a `Sxact`; its reads leave SIRead predicate locks
//! (relation, page or tuple targets) and its writes are remembered; a read that meets a
//! concurrent transaction's write, or a write that meets a concurrent reader's lock, is a
//! rw-conflict (reader → writer). A transaction with a conflict in and a conflict out (a
//! "pivot") of the dangerous shape is canceled, at the points and with the reason codes
//! Postgres uses: on identification during the write or read, by dooming it, or at commit.
//!
//! The internals differ: the engine knows which transaction wrote which row directly, where
//! Postgres reads it from tuple headers; what conflicts with what, and when the errors
//! come, follows predicate.c (OnConflict_CheckForSerializationFailure,
//! PreCommit_CheckForSerializationFailure, ReleasePredicateLocks, the read-only
//! optimisations and DEFERRABLE's safe snapshot). Not modelled: summarizing old
//! transactions into the SLRU (the engine keeps them as long as they matter), parallel
//! workers sharing a transaction, two-phase commit.

use crate::error::{Error, Result, code};
use parking_lot::{Condvar, Mutex};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// A predicate lock target: (relation, page, tuple offset); page None: the relation,
/// tuple None: the page.
pub type Target = (u32, Option<u32>, Option<u16>);

#[derive(Default)]
struct Sxact {
    pid: i32,
    /// Its top-level transaction id once it has one (reason codes name it).
    xid: u64,
    /// Declared READ ONLY (topLevelIsDeclaredReadOnly).
    declared_ro: bool,
    /// SXACT_FLAG_READ_ONLY: declared, or committed without writing.
    read_only: bool,
    did_write: bool,
    committed: bool,
    /// Past PreCommit_CheckForSerializationFailure (SXACT_FLAG_PREPARED).
    prepared: bool,
    doomed: bool,
    rolled_back: bool,
    /// SXACT_FLAG_CONFLICT_OUT: committed with a conflict out to a committed transaction.
    conflict_out: bool,
    ro_safe: bool,
    ro_unsafe: bool,
    /// Waiting for a safe snapshot (SXACT_FLAG_DEFERRABLE_WAITING).
    deferrable_waiting: bool,
    last_commit_before_snapshot: u64,
    prepare_seq: u64,
    commit_seq: u64,
    earliest_out_conflict_commit: u64,
    /// The store timestamp of its snapshot, and of its commit (finishedBefore).
    snapshot_ts: u64,
    commit_ts: Option<u64>,
    /// Readers with a rw-conflict out to it, writers it has one out to.
    in_conflicts: Vec<u64>,
    out_conflicts: Vec<u64>,
    /// A read-only one: the writable transactions it might conflict with; a writable one:
    /// the read-only ones that might conflict with it.
    possible_unsafe: Vec<u64>,
    locks: Vec<Target>,
    /// Tuple locks on rows of tables without a modelled heap (no tuple ids): the row and
    /// the commit timestamp of the version read.
    row_versions: HashSet<(u32, Vec<u8>, u64)>,
    /// Rows it changed (relation, data key), relations it changed, and the index entries it
    /// added (index, entry key).
    writes: HashSet<(u32, Vec<u8>)>,
    rels: HashSet<u32>,
    entries: Vec<(u32, Vec<u8>)>,
    /// The index pages its writes went to (relation, page).
    pages: HashSet<(u32, u32)>,
}

#[derive(Default)]
struct State {
    sx: HashMap<u64, Sxact>,
    /// PredXact->LastSxactCommitSeqNo.
    last_commit_seq: u64,
}

/// The database's serializable transactions.
#[derive(Default)]
pub struct Ssi {
    st: Mutex<State>,
    cv: Condvar,
}

const MESSAGE: &str = "could not serialize access due to read/write dependencies among transactions";

fn failure(reason: String) -> Error {
    Error::new(code::SERIALIZATION_FAILURE, MESSAGE)
        .detail(format!("Reason code: {reason}."))
        .hint("The transaction might succeed if retried.")
}

impl State {
    fn get(&self, id: u64) -> Option<&Sxact> {
        self.sx.get(&id)
    }

    fn active(&self) -> impl Iterator<Item = (&u64, &Sxact)> {
        self.sx.iter().filter(|(_, s)| s.commit_ts.is_none() && !s.rolled_back)
    }

    fn conflict_exists(&self, reader: u64, writer: u64) -> bool {
        self.get(reader).is_some_and(|r| r.out_conflicts.contains(&writer))
    }

    fn set_conflict(&mut self, reader: u64, writer: u64) {
        if let Some(r) = self.sx.get_mut(&reader)
            && !r.out_conflicts.contains(&writer)
        {
            r.out_conflicts.push(writer);
        }
        if let Some(w) = self.sx.get_mut(&writer)
            && !w.in_conflicts.contains(&reader)
        {
            w.in_conflicts.push(reader);
        }
    }

    fn release_conflict(&mut self, reader: u64, writer: u64) {
        if let Some(r) = self.sx.get_mut(&reader) {
            r.out_conflicts.retain(|x| *x != writer);
        }
        if let Some(w) = self.sx.get_mut(&writer) {
            w.in_conflicts.retain(|x| *x != reader);
        }
    }

    /// The possible-unsafe link between read-only `ro` and writable `rw`.
    fn release_unsafe(&mut self, ro: u64, rw: u64) {
        if let Some(r) = self.sx.get_mut(&ro) {
            r.possible_unsafe.retain(|x| *x != rw);
        }
        if let Some(w) = self.sx.get_mut(&rw) {
            w.possible_unsafe.retain(|x| *x != ro);
        }
    }

    /// FlagSxactUnsafe.
    fn flag_unsafe(&mut self, ro: u64) {
        let links = self.sx.get(&ro).map(|s| s.possible_unsafe.clone()).unwrap_or_default();
        for rw in links {
            self.release_unsafe(ro, rw);
        }
        if let Some(r) = self.sx.get_mut(&ro) {
            r.ro_unsafe = true;
        }
    }

    /// ReleaseOneSerializableXact: it goes, with its locks and conflicts.
    fn release_one(&mut self, id: u64) {
        let Some(s) = self.sx.remove(&id) else { return };
        for w in s.out_conflicts {
            if let Some(w) = self.sx.get_mut(&w) {
                w.in_conflicts.retain(|x| *x != id);
            }
        }
        for r in s.in_conflicts {
            if let Some(r) = self.sx.get_mut(&r) {
                r.out_conflicts.retain(|x| *x != id);
            }
        }
        for o in s.possible_unsafe {
            if let Some(o) = self.sx.get_mut(&o) {
                o.possible_unsafe.retain(|x| *x != id);
            }
        }
    }

    /// ClearOldPredicateLocks: committed transactions every active one's snapshot already
    /// sees finished can't conflict with anything any more.
    fn clear_old(&mut self) {
        let oldest = self.active().map(|(_, s)| s.snapshot_ts).min();
        let done: Vec<u64> = self
            .sx
            .iter()
            .filter(|(_, s)| s.commit_ts.is_some_and(|c| oldest.is_none_or(|o| c <= o)))
            .map(|(id, _)| *id)
            .collect();
        for id in done {
            self.release_one(id);
        }
    }

    /// FlagRWConflict, with OnConflict_CheckForSerializationFailure first: `me` is the
    /// transaction checking (the reader or the writer).
    fn flag_rw_conflict(&mut self, me: u64, reader: u64, writer: u64) -> Result<()> {
        let (Some(r), Some(w)) = (self.get(reader), self.get(writer)) else {
            return Ok(());
        };
        let mut failed = w.committed && w.conflict_out;
        if !failed {
            failed = w.out_conflicts.iter().filter_map(|t| self.get(*t)).any(|t2| {
                t2.prepared
                    && (!r.committed || t2.prepare_seq <= r.commit_seq)
                    && (!w.committed || t2.prepare_seq <= w.commit_seq)
                    && (!r.read_only || t2.prepare_seq <= r.last_commit_before_snapshot)
            });
        }
        if !failed && w.prepared && !r.read_only {
            failed = r.in_conflicts.iter().filter_map(|t| self.get(*t)).any(|t0| {
                !t0.doomed
                    && (!t0.committed || t0.commit_seq >= w.prepare_seq)
                    && (!t0.read_only || t0.last_commit_before_snapshot >= w.prepare_seq)
            });
        }
        if failed {
            if me == writer {
                return Err(failure("Canceled on identification as a pivot, during write".into()));
            }
            if w.prepared {
                return Err(failure(format!(
                    "Canceled on conflict out to pivot {}, during read",
                    w.xid
                )));
            }
            if let Some(w) = self.sx.get_mut(&writer) {
                w.doomed = true;
            }
        }
        self.set_conflict(reader, writer);
        Ok(())
    }

    /// CheckForSerializableConflictOut: `me` read a version `writer` made or replaced.
    fn conflict_out(&mut self, me: u64, writer: u64) -> Result<()> {
        let Some(m) = self.get(me) else { return Ok(()) };
        if m.doomed {
            return Err(failure(
                "Canceled on identification as a pivot, during conflict out checking".into(),
            ));
        }
        if writer == me {
            return Ok(());
        }
        let Some(w) = self.get(writer) else { return Ok(()) };
        if w.doomed {
            return Ok(());
        }
        if m.read_only
            && w.committed
            && (!w.conflict_out || m.last_commit_before_snapshot < w.earliest_out_conflict_commit)
        {
            return Ok(());
        }
        // XidIsConcurrent: it hadn't committed when our snapshot was taken.
        if w.commit_ts.is_some_and(|c| c <= m.snapshot_ts) {
            return Ok(());
        }
        if self.conflict_exists(me, writer) {
            return Ok(());
        }
        self.flag_rw_conflict(me, me, writer)
    }

    /// CheckTargetForConflictsIn for a row version without a tuple id.
    fn version_conflicts_in(&mut self, me: u64, v: (u32, Vec<u8>, u64)) -> Result<()> {
        let snap = self.get(me).map_or(0, |m| m.snapshot_ts);
        let mut readers: Vec<u64> = self
            .sx
            .iter()
            .filter(|(id, s)| {
                **id != me
                    && s.row_versions.contains(&v)
                    && !s.doomed
                    && s.commit_ts.is_none_or(|c| c > snap)
            })
            .map(|(id, _)| *id)
            .collect();
        readers.sort_unstable();
        for r in readers {
            if !self.conflict_exists(r, me) {
                self.flag_rw_conflict(me, r, me)?;
            }
        }
        Ok(())
    }

    /// CheckTargetForConflictsIn for one target `me` writes.
    fn conflicts_in(&mut self, me: u64, target: Target) -> Result<()> {
        let snap = self.get(me).map_or(0, |m| m.snapshot_ts);
        let mut readers: Vec<u64> = self
            .sx
            .iter()
            .filter(|(id, s)| {
                **id != me
                    && s.locks.contains(&target)
                    && !s.doomed
                    && s.commit_ts.is_none_or(|c| c > snap)
            })
            .map(|(id, _)| *id)
            .collect();
        readers.sort_unstable();
        for r in readers {
            if !self.conflict_exists(r, me) {
                self.flag_rw_conflict(me, r, me)?;
            }
        }
        Ok(())
    }
}

impl Ssi {
    /// GetSerializableTransactionSnapshotInt: transaction `id` of backend `pid` took its
    /// snapshot. False: it needn't be tracked (read only, with no concurrent writable
    /// serializable transaction).
    pub fn register(&self, id: u64, pid: i32, read_only: bool, snapshot_ts: u64) -> bool {
        let mut st = self.st.lock();
        let writable: Vec<u64> = st
            .active()
            .filter(|(_, s)| !s.declared_ro && !s.doomed && !s.prepared)
            .map(|(i, _)| *i)
            .collect();
        if read_only && writable.is_empty() {
            return false;
        }
        let s = Sxact {
            pid,
            declared_ro: read_only,
            read_only,
            last_commit_before_snapshot: st.last_commit_seq,
            snapshot_ts,
            possible_unsafe: if read_only { writable.clone() } else { Vec::new() },
            ..Default::default()
        };
        st.sx.insert(id, s);
        if read_only {
            for w in writable {
                if let Some(w) = st.sx.get_mut(&w) {
                    w.possible_unsafe.push(id);
                }
            }
        }
        true
    }

    /// GetSafeSnapshot (SERIALIZABLE READ ONLY DEFERRABLE): wait until the writable
    /// transactions concurrent with this snapshot have finished. Ok(true): the snapshot is
    /// safe (and needs no tracking); Ok(false): it isn't, take another.
    pub fn wait_safe(&self, id: u64, cancel: &AtomicBool) -> Result<bool> {
        let mut st = self.st.lock();
        if let Some(s) = st.sx.get_mut(&id) {
            s.deferrable_waiting = true;
        }
        loop {
            let Some(s) = st.get(id) else { return Ok(true) };
            if s.ro_unsafe {
                st.release_one(id);
                return Ok(false);
            }
            if s.possible_unsafe.is_empty() {
                st.release_one(id);
                return Ok(true);
            }
            if cancel.load(Ordering::Relaxed) {
                st.release_one(id);
                return Err(Error::new(
                    code::QUERY_CANCELED,
                    "canceling statement due to user request",
                ));
            }
            self.cv.wait_for(&mut st, Duration::from_millis(50));
        }
    }

    /// GetSafeSnapshotBlockingPids: the backends whose transactions backend `pid`'s
    /// deferrable snapshot waits for.
    pub fn safe_snapshot_blockers(&self, pid: i32) -> Vec<i32> {
        let st = self.st.lock();
        let Some(s) = st.sx.values().find(|s| s.pid == pid && s.deferrable_waiting)
        else {
            return Vec::new();
        };
        s.possible_unsafe.iter().filter_map(|w| st.get(*w)).map(|w| w.pid).collect()
    }

    /// Whether `id` is tracked.
    pub fn tracked(&self, id: u64) -> bool {
        self.st.lock().sx.contains_key(&id)
    }

    pub fn set_xid(&self, id: u64, xid: u64) {
        if let Some(s) = self.st.lock().sx.get_mut(&id) {
            s.xid = xid;
        }
    }

    /// SerializationNeededForRead: false once it no longer takes part (its read-only
    /// snapshot turned out safe: it drops out, ReleasePredicateLocks).
    pub fn needed_for_read(&self, id: u64) -> bool {
        let mut st = self.st.lock();
        match st.get(id) {
            None => false,
            Some(s) if s.ro_safe => {
                st.release_one(id);
                false
            }
            Some(_) => true,
        }
    }

    /// Its SIRead locks are now these.
    pub fn set_locks(&self, id: u64, locks: &[Target]) {
        if let Some(s) = self.st.lock().sx.get_mut(&id) {
            s.locks.clear();
            s.locks.extend_from_slice(locks);
        }
    }

    /// The tracked transactions that changed row `key` of `rel`: what reading its version
    /// conflicts out to.
    pub fn read_row(&self, id: u64, rel: u32, key: &[u8]) -> Result<()> {
        let mut st = self.st.lock();
        let k = (rel, key.to_vec());
        let mut writers: Vec<u64> =
            st.sx.iter().filter(|(w, s)| **w != id && s.writes.contains(&k)).map(|(w, _)| *w).collect();
        writers.sort_unstable();
        for w in writers {
            st.conflict_out(id, w)?;
        }
        Ok(())
    }

    /// A sequential scan of `rel` meets every tuple there: it conflicts out to every
    /// concurrent transaction that changed the relation.
    pub fn read_relation(&self, id: u64, rel: u32) -> Result<()> {
        let mut st = self.st.lock();
        let mut writers: Vec<u64> =
            st.sx.iter().filter(|(w, s)| **w != id && s.rels.contains(&rel)).map(|(w, _)| *w).collect();
        writers.sort_unstable();
        for w in writers {
            st.conflict_out(id, w)?;
        }
        Ok(())
    }

    /// An index scan over these entry ranges of `index` meets the entries concurrent
    /// transactions added there (and their heap tuples).
    pub fn read_entries(&self, id: u64, index: u32, ranges: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
        let mut st = self.st.lock();
        let mut writers: Vec<u64> = st
            .sx
            .iter()
            .filter(|(w, s)| {
                **w != id
                    && s.entries.iter().any(|(ix, e)| {
                        *ix == index && ranges.iter().any(|(lo, hi)| e >= lo && e < hi)
                    })
            })
            .map(|(w, _)| *w)
            .collect();
        writers.sort_unstable();
        for w in writers {
            st.conflict_out(id, w)?;
        }
        Ok(())
    }

    /// A tuple lock on the version (committed at `ts`) of row `key` of `rel`, for a table
    /// whose tuples have no modelled ids.
    pub fn lock_row_version(&self, id: u64, rel: u32, key: &[u8], ts: u64) {
        if let Some(s) = self.st.lock().sx.get_mut(&id) {
            s.row_versions.insert((rel, key.to_vec(), ts));
        }
    }

    /// CheckForSerializableConflictIn: `id` writes `targets` (its tuple, page and relation,
    /// or an index page and the index); `row` names the row it changes, if any, and
    /// `version` the version (committed at that timestamp) it replaces when that has no
    /// tuple id.
    pub fn write(
        &self,
        id: u64,
        row: Option<(u32, &[u8])>,
        targets: &[Target],
        version: Option<u64>,
    ) -> Result<()> {
        let mut st = self.st.lock();
        let Some(m) = st.sx.get_mut(&id) else { return Ok(()) };
        if m.doomed {
            return Err(failure(
                "Canceled on identification as a pivot, during conflict in checking".into(),
            ));
        }
        m.did_write = true;
        if let Some((rel, key)) = row {
            m.writes.insert((rel, key.to_vec()));
            m.rels.insert(rel);
            if let Some(ts) = version {
                st.version_conflicts_in(id, (rel, key.to_vec(), ts))?;
            }
        }
        for t in targets {
            if let (rel, Some(page), None) = *t
                && let Some(m) = st.sx.get_mut(&id)
            {
                m.pages.insert((rel, page));
            }
            st.conflicts_in(id, *t)?;
        }
        Ok(())
    }

    /// A scan read these pages of `index` (one whose entries the engine doesn't keep as
    /// keys, GiST): it meets what concurrent transactions wrote there.
    pub fn read_pages(&self, id: u64, index: u32, pages: &[u32]) -> Result<()> {
        let mut st = self.st.lock();
        let mut writers: Vec<u64> = st
            .sx
            .iter()
            .filter(|(w, s)| **w != id && pages.iter().any(|p| s.pages.contains(&(index, *p))))
            .map(|(w, _)| *w)
            .collect();
        writers.sort_unstable();
        for w in writers {
            st.conflict_out(id, w)?;
        }
        Ok(())
    }

    /// An index entry `id` added (what index scans of concurrent readers meet).
    pub fn wrote_entry(&self, id: u64, index: u32, entry: &[u8]) {
        if let Some(m) = self.st.lock().sx.get_mut(&id) {
            m.entries.push((index, entry.to_vec()));
        }
    }

    /// PreCommit_CheckForSerializationFailure.
    pub fn pre_commit(&self, id: u64) -> Result<()> {
        let mut st = self.st.lock();
        let Some(m) = st.get(id) else { return Ok(()) };
        if m.doomed {
            return Err(failure(
                "Canceled on identification as a pivot, during commit attempt".into(),
            ));
        }
        let mut doom = Vec::new();
        for near in &m.in_conflicts {
            let Some(n) = st.get(*near) else { continue };
            if n.committed || n.doomed {
                continue;
            }
            let dangerous = n.in_conflicts.iter().any(|far| {
                *far == id
                    || st
                        .get(*far)
                        .is_some_and(|f| !f.committed && !f.read_only && !f.doomed)
            });
            if dangerous {
                if n.prepared {
                    return Err(failure(
                        "Canceled on commit attempt with conflict in from prepared pivot".into(),
                    ));
                }
                doom.push(*near);
            }
        }
        for d in doom {
            if let Some(n) = st.sx.get_mut(&d) {
                n.doomed = true;
            }
        }
        st.last_commit_seq += 1;
        let seq = st.last_commit_seq;
        let m = st.sx.get_mut(&id).unwrap();
        m.prepare_seq = seq;
        m.prepared = true;
        Ok(())
    }

    /// ReleasePredicateLocks: `id` committed (at store time `ts`) or rolled back.
    pub fn finish(&self, id: u64, commit: bool, ts: u64) {
        let mut st = self.st.lock();
        let Some(m) = st.sx.get(&id) else { return };
        let declared_ro = m.declared_ro;
        let did_write = m.did_write;
        st.last_commit_seq += commit as u64;
        let seq = st.last_commit_seq;
        {
            let m = st.sx.get_mut(&id).unwrap();
            if commit {
                m.committed = true;
                m.commit_seq = seq;
                m.commit_ts = Some(ts);
                if !m.did_write {
                    m.read_only = true;
                }
            } else {
                m.doomed = true;
                m.rolled_back = true;
                m.prepared = false;
            }
        }
        if declared_ro {
            let links = st.sx[&id].possible_unsafe.clone();
            for rw in links {
                st.release_unsafe(id, rw);
            }
        }
        // Conflicts out to committed transactions make it "conflict out"; those that can
        // no longer matter go.
        let read_only = st.sx[&id].read_only;
        let outs = st.sx[&id].out_conflicts.clone();
        for w in outs {
            let Some(ws) = st.get(w) else { continue };
            let (w_committed, w_prepare, w_lcbs) =
                (ws.committed, ws.prepare_seq, ws.last_commit_before_snapshot);
            if commit && !read_only && w_committed {
                let m = st.sx.get_mut(&id).unwrap();
                if !m.conflict_out || w_prepare < m.earliest_out_conflict_commit {
                    m.earliest_out_conflict_commit = w_prepare;
                }
                m.conflict_out = true;
            }
            if !commit || w_committed || w_lcbs >= st.last_commit_seq {
                st.release_conflict(id, w);
            }
        }
        let ins = st.sx[&id].in_conflicts.clone();
        for r in ins {
            let release = st.get(r).is_none_or(|rs| rs.committed || rs.read_only);
            if !commit || release {
                st.release_conflict(r, id);
            }
        }
        if !declared_ro {
            let ros = st.sx[&id].possible_unsafe.clone();
            let m = &st.sx[&id];
            let (conflict_out, earliest) = (m.conflict_out, m.earliest_out_conflict_commit);
            for ro in ros {
                let lcbs = st.get(ro).map_or(0, |r| r.last_commit_before_snapshot);
                if commit && did_write && conflict_out && earliest <= lcbs {
                    st.flag_unsafe(ro);
                } else {
                    st.release_unsafe(ro, id);
                    if let Some(r) = st.sx.get_mut(&ro)
                        && r.possible_unsafe.is_empty()
                    {
                        r.ro_safe = true;
                    }
                }
            }
        }
        if !commit {
            st.release_one(id);
        }
        st.clear_old();
        drop(st);
        self.cv.notify_all();
    }
}

