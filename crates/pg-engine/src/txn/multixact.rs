//! MultiXacts (multixact.c) and the xmax a row lock, an update or a delete leaves on the
//! version it acts on (heapam.c's compute_new_xmax_infomask).
//!
//! PostgreSQL keeps a row lock in the locked version's xmax: the locker's transaction id with
//! HEAP_XMAX_LOCK_ONLY and the lock's strength, or a MultiXactId once a second transaction
//! (or another subtransaction of the same one) locks or updates the version while a locker
//! is running. The xmax stays after the lockers end, until the version is locked, updated or
//! frozen again; `xmax`, pgrowlocks and pg_get_multixact_members read it. The engine's row
//! locks are its lock manager's (`txn::locks`); the heap model (`catalog::heapsim`) keeps the
//! xmax PostgreSQL would have, by these rules, and the cluster's multis are here.

use crate::txn::locks::RowLockMode;
use std::sync::Arc;

/// MultiXactStatus, in its numbering (members sort by it after the xid).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum MxStatus {
    ForKeyShare = 0,
    ForShare = 1,
    ForNoKeyUpdate = 2,
    ForUpdate = 3,
    NoKeyUpdate = 4,
    Update = 5,
}

impl MxStatus {
    /// ISUPDATE_from_mxstatus.
    pub fn is_update(self) -> bool {
        self >= MxStatus::NoKeyUpdate
    }

    /// TUPLOCK_from_mxstatus.
    pub fn mode(self) -> RowLockMode {
        match self {
            MxStatus::ForKeyShare => RowLockMode::KeyShare,
            MxStatus::ForShare => RowLockMode::Share,
            MxStatus::ForNoKeyUpdate | MxStatus::NoKeyUpdate => RowLockMode::NoKeyUpdate,
            MxStatus::ForUpdate | MxStatus::Update => RowLockMode::Update,
        }
    }

    /// get_mxact_status_for_lock (an update's lock is NoKeyUpdate or Update).
    pub fn of(mode: RowLockMode, is_update: bool) -> MxStatus {
        match (mode, is_update) {
            (RowLockMode::KeyShare, _) => MxStatus::ForKeyShare,
            (RowLockMode::Share, _) => MxStatus::ForShare,
            (RowLockMode::NoKeyUpdate, false) => MxStatus::ForNoKeyUpdate,
            (RowLockMode::Update, false) => MxStatus::ForUpdate,
            (RowLockMode::NoKeyUpdate, true) => MxStatus::NoKeyUpdate,
            (RowLockMode::Update, true) => MxStatus::Update,
        }
    }

    /// pg_get_multixact_members' mode (mxstatus_to_string).
    pub fn name(self) -> &'static str {
        match self {
            MxStatus::ForKeyShare => "keysh",
            MxStatus::ForShare => "sh",
            MxStatus::ForNoKeyUpdate => "fornokeyupd",
            MxStatus::ForUpdate => "forupd",
            MxStatus::NoKeyUpdate => "nokeyupd",
            MxStatus::Update => "upd",
        }
    }

    /// pgrowlocks' name for it.
    pub fn rowlocks_name(self) -> &'static str {
        match self {
            MxStatus::ForKeyShare => "For Key Share",
            MxStatus::ForShare => "For Share",
            MxStatus::ForNoKeyUpdate => "For No Key Update",
            MxStatus::ForUpdate => "For Update",
            MxStatus::NoKeyUpdate => "No Key Update",
            MxStatus::Update => "Update",
        }
    }

    pub fn from_u8(v: u8) -> Option<MxStatus> {
        Some(match v {
            0 => MxStatus::ForKeyShare,
            1 => MxStatus::ForShare,
            2 => MxStatus::ForNoKeyUpdate,
            3 => MxStatus::ForUpdate,
            4 => MxStatus::NoKeyUpdate,
            5 => MxStatus::Update,
            _ => return None,
        })
    }
}

/// MultiXactMember.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Member {
    pub xid: u64,
    pub status: MxStatus,
}

/// FirstMultiXactId.
pub const FIRST_MULTI: u32 = 1;

/// The cluster's MultiXactIds (MultiXactState and the offsets / members SLRUs): the next id
/// and member offset, and each multi's members.
pub struct MultiXacts {
    /// nextMXact.
    pub next: u32,
    /// nextOffset (offset 0 is never a multi's: the first one starts at 1).
    pub next_offset: u32,
    /// oldestMultiXactId: multis before it are gone.
    pub oldest: u32,
    /// Each multi's members, and the next transaction id when it was made (`oldest_needed`):
    /// those made or read since the database opened.
    members: std::collections::BTreeMap<u32, (u64, Arc<[Member]>)>,
    /// Reads a multi's record (`MULTI_PREFIX`) from the database's store.
    load: Option<Loader>,
}

/// Reads multi `m`'s stored record: its mark and members.
pub type Loader = Box<dyn Fn(u32) -> Option<(u64, Vec<Member>)> + Send + Sync>;

impl Default for MultiXacts {
    fn default() -> Self {
        MultiXacts {
            next: FIRST_MULTI,
            next_offset: 0,
            oldest: FIRST_MULTI,
            members: Default::default(),
            load: None,
        }
    }
}

impl MultiXacts {
    /// GetNewMultiXactId and RecordNewMultiXact: a new multi of `members` (sorted), made when
    /// `mark` was the next transaction id.
    pub fn create_marked(&mut self, members: &[Member], mark: u64) -> u32 {
        if self.next < FIRST_MULTI {
            self.next = FIRST_MULTI;
        }
        let multi = self.next;
        self.next = self.next.wrapping_add(1);
        let mut n = members.len() as u32;
        if self.next_offset == 0 {
            // (Member slot 0 is allocated too.)
            n += 1;
        }
        self.next_offset = self.next_offset.wrapping_add(n);
        self.members.insert(multi, (mark, members.into()));
        multi
    }

    /// `create_marked` with no mark.
    pub fn create(&mut self, members: &[Member]) -> u32 {
        self.create_marked(members, 0)
    }

    /// GetMultiXactIdMembers (None: not a multi this database has).
    pub fn members(&mut self, multi: u32) -> Option<Arc<[Member]>> {
        if let Some(m) = self.members.get(&multi) {
            return Some(m.1.clone());
        }
        let (mark, members) = (self.load.as_ref()?)(multi)?;
        let members: Arc<[Member]> = members.into();
        self.members.insert(multi, (mark, members.clone()));
        Some(members)
    }

    /// The state a database's store kept (`MULTI_PREFIX`): its last multi's id and the
    /// member offset after it (None: none made), and how to read the others.
    pub fn restored(last: Option<(u32, u32)>, load: Loader) -> MultiXacts {
        let mut m = MultiXacts { load: Some(load), ..Default::default() };
        if let Some((multi, next_offset)) = last {
            m.next = multi.wrapping_add(1).max(FIRST_MULTI);
            m.next_offset = next_offset;
        }
        m
    }

    /// GetOldestMultiXactId: the oldest multi a running transaction may still be a member of
    /// or have looked at, taken as the first one made after the oldest running transaction
    /// (`oldest_running`, None: none runs) got its id; the next one when none was.
    pub fn oldest_needed(&self, oldest_running: Option<u64>) -> u32 {
        let mut oldest = self.next;
        if let Some(r) = oldest_running {
            for (&m, (mark, _)) in self.members.iter().rev() {
                if *mark <= r {
                    break;
                }
                oldest = m;
            }
        }
        oldest
    }
}

// ── Durable state ──
//
// A database's MultiXact state is kept in its own store, with the rows whose xmax names it,
// written by the transaction that made it with its other writes (so it is as durable as they
// are: its WAL record, replayed after a crash) and kept out of its write-conflict checks:
//
// - `MULTI_PREFIX ++ multi` → the multi's members, and the next transaction id and member
//   offset when it was made. A database opening reads the last one for its next multi and
//   offset, and members as they are asked for.
// - `XMAX_PREFIX ++ table ++ row key ++ TID` → the xmax of the version at that TID when it
//   outlives the transaction that wrote it (a lock's, or the lockers an update kept): what a
//   model laid out afresh from the rows gets back. An update, a delete or VACUUM's freezing of
//   the version deletes it.

/// Store keys of the multis' members.
pub const MULTI_PREFIX: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 0x26];
/// The end of the multis' keys.
pub const MULTI_END: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 0x27];
/// Store keys of versions' xmax records.
pub const XMAX_PREFIX: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 0x27];

/// Whether `k` is a MultiXact state key (no transaction's write of one conflicts with
/// another's).
pub fn is_state_key(k: &[u8]) -> bool {
    k.len() > 8 && k[..7] == [0; 7] && (k[7] == 0x26 || k[7] == 0x27)
}

pub fn multi_key(multi: u32) -> Vec<u8> {
    let mut k = MULTI_PREFIX.to_vec();
    k.extend_from_slice(&multi.to_be_bytes());
    k
}

/// A multi's record: the mark, the next member offset after it, then (xid, status) each.
pub fn encode_multi(mark: u64, next_offset: u32, members: &[Member]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + members.len() * 9);
    out.extend_from_slice(&mark.to_be_bytes());
    out.extend_from_slice(&next_offset.to_be_bytes());
    for m in members {
        out.extend_from_slice(&m.xid.to_be_bytes());
        out.push(m.status as u8);
    }
    out
}

pub fn decode_multi(b: &[u8]) -> Option<(u64, u32, Vec<Member>)> {
    let mark = u64::from_be_bytes(b.get(..8)?.try_into().ok()?);
    let next_offset = u32::from_be_bytes(b.get(8..12)?.try_into().ok()?);
    let rest = &b[12..];
    if !rest.len().is_multiple_of(9) {
        return None;
    }
    let members = rest
        .chunks(9)
        .map(|c| Some(Member { xid: u64::from_be_bytes(c[..8].try_into().ok()?), status: MxStatus::from_u8(c[8])? }))
        .collect::<Option<Vec<_>>>()?;
    Some((mark, next_offset, members))
}

/// The xmax record key of the version of `row` (a data key) of `table` at `tid`.
pub fn xmax_key(table: u32, row: &[u8], tid: (u32, u16)) -> Vec<u8> {
    let mut k = Vec::with_capacity(8 + 4 + row.len() + 6);
    k.extend_from_slice(XMAX_PREFIX);
    k.extend_from_slice(&table.to_be_bytes());
    k.extend_from_slice(row);
    k.extend_from_slice(&tid.0.to_be_bytes());
    k.extend_from_slice(&tid.1.to_be_bytes());
    k
}

/// The table's xmax records' key range.
pub fn xmax_range(table: u32) -> (Vec<u8>, Vec<u8>) {
    let mut a = XMAX_PREFIX.to_vec();
    a.extend_from_slice(&table.to_be_bytes());
    let mut b = XMAX_PREFIX.to_vec();
    b.extend_from_slice(&(table as u64 + 1).to_be_bytes()[4..]);
    if table == u32::MAX {
        b = vec![0, 0, 0, 0, 0, 0, 0, 0x28];
    }
    (a, b)
}

/// A record key's row key and TID (after the table).
pub fn split_xmax_key(k: &[u8]) -> Option<(&[u8], (u32, u16))> {
    let body = k.get(12..)?;
    let n = body.len().checked_sub(6)?;
    let tid = (u32::from_be_bytes(body[n..n + 4].try_into().ok()?), u16::from_be_bytes(body[n + 4..].try_into().ok()?));
    Some((&body[..n], tid))
}

pub fn encode_xmax(x: Xmax) -> Vec<u8> {
    let mut out = x.raw.to_be_bytes().to_vec();
    out.push(x.bits);
    out
}

pub fn decode_xmax(b: &[u8]) -> Option<Xmax> {
    Some(Xmax { raw: u64::from_be_bytes(b.get(..8)?.try_into().ok()?), bits: *b.get(8)? & XMAX_BITS })
}

/// A transaction id's state as the rules ask for it (TransactionIdIsInProgress,
/// TransactionIdDidCommit).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XState {
    Running,
    Committed,
    Aborted,
}

// A version's xmax bits (`heapsim::Tup::flags`, beside its HOT_UPDATED bit): t_infomask's
// HEAP_XMAX_* and t_infomask2's HEAP_KEYS_UPDATED.
/// HEAP_XMAX_LOCK_ONLY.
pub const LOCK_ONLY: u8 = 1 << 1;
/// HEAP_XMAX_KEYSHR_LOCK.
pub const KEYSHR: u8 = 1 << 2;
/// HEAP_XMAX_EXCL_LOCK.
pub const EXCL: u8 = 1 << 3;
/// HEAP_XMAX_SHR_LOCK (both).
pub const SHR: u8 = KEYSHR | EXCL;
/// HEAP_XMAX_IS_MULTI.
pub const IS_MULTI: u8 = 1 << 4;
/// HEAP_KEYS_UPDATED.
pub const KEYS_UPDATED: u8 = 1 << 5;
/// Every xmax bit.
pub const XMAX_BITS: u8 = LOCK_ONLY | SHR | IS_MULTI | KEYS_UPDATED;

/// A version's raw xmax and its bits (an xid, or a MultiXactId with `IS_MULTI`; 0: none).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Xmax {
    pub raw: u64,
    pub bits: u8,
}

impl Xmax {
    pub const NONE: Xmax = Xmax { raw: 0, bits: 0 };

    pub fn is_multi(self) -> bool {
        self.bits & IS_MULTI != 0
    }

    /// HEAP_XMAX_IS_LOCKED_ONLY.
    pub fn lock_only(self) -> bool {
        self.bits & LOCK_ONLY != 0
    }

    /// The status of a single xid's xmax (a lock's from its bits, an update's from
    /// HEAP_KEYS_UPDATED).
    pub fn single_status(self) -> MxStatus {
        if self.lock_only() {
            match self.bits & SHR {
                KEYSHR => MxStatus::ForKeyShare,
                SHR => MxStatus::ForShare,
                _ if self.bits & KEYS_UPDATED != 0 => MxStatus::ForUpdate,
                _ => MxStatus::ForNoKeyUpdate,
            }
        } else if self.bits & KEYS_UPDATED != 0 {
            MxStatus::Update
        } else {
            MxStatus::NoKeyUpdate
        }
    }

    /// The xmax of a single xid locking in `mode` (or updating, `is_update`).
    fn single(xid: u64, mode: RowLockMode, is_update: bool) -> Xmax {
        let bits = if is_update {
            if mode == RowLockMode::Update { KEYS_UPDATED } else { 0 }
        } else {
            LOCK_ONLY
                | match mode {
                    RowLockMode::KeyShare => KEYSHR,
                    RowLockMode::Share => SHR,
                    RowLockMode::NoKeyUpdate => EXCL,
                    RowLockMode::Update => EXCL | KEYS_UPDATED,
                }
        };
        Xmax { raw: xid, bits }
    }

    /// GetMultiXactIdHintBits: a multi's bits from its members.
    pub fn of_multi(multi: u32, members: &[Member]) -> Xmax {
        let mut bits = IS_MULTI;
        let mut strongest = RowLockMode::KeyShare;
        let mut has_update = false;
        for m in members {
            strongest = strongest.max(m.status.mode());
            match m.status {
                MxStatus::ForUpdate => bits |= KEYS_UPDATED,
                MxStatus::NoKeyUpdate => has_update = true,
                MxStatus::Update => {
                    bits |= KEYS_UPDATED;
                    has_update = true;
                }
                _ => {}
            }
        }
        bits |= match strongest {
            RowLockMode::Update | RowLockMode::NoKeyUpdate => EXCL,
            RowLockMode::Share => SHR,
            RowLockMode::KeyShare => KEYSHR,
        };
        if !has_update {
            bits |= LOCK_ONLY;
        }
        Xmax { raw: multi as u64, bits }
    }
}

/// What the rules read: transaction states, the current transaction's ids, and multis (made
/// with the transaction's cache of them).
pub trait Env {
    fn state(&mut self, xid: u64) -> XState;
    /// TransactionIdIsCurrentTransactionId.
    fn is_current(&mut self, xid: u64) -> bool;
    /// GetMultiXactIdMembers (empty: none known).
    fn members(&mut self, multi: u32) -> Arc<[Member]>;
    /// MultiXactIdCreateFromMembers (`members` sorted).
    fn create(&mut self, members: &[Member]) -> u32;
}

/// MultiXactIdGetUpdateXid: a multi's updating member (0: none).
pub fn update_xid(members: &[Member]) -> u64 {
    members.iter().find(|m| m.status.is_update()).map_or(0, |m| m.xid)
}

fn create_of(mut members: Vec<Member>, env: &mut dyn Env) -> Xmax {
    members.sort_unstable();
    let multi = env.create(&members);
    Xmax::of_multi(multi, &members)
}

/// MultiXactIdExpand: `multi`'s members still of interest (running, or a committed update)
/// and `xid` in `status`; `multi` itself when `xid` is a member in that status already.
fn expand(multi: u32, members: &[Member], xid: u64, status: MxStatus, env: &mut dyn Env) -> Xmax {
    if members.iter().any(|m| m.xid == xid && m.status == status) {
        return Xmax::of_multi(multi, members);
    }
    let mut keep: Vec<Member> = Vec::with_capacity(members.len() + 1);
    for m in members {
        let st = env.state(m.xid);
        if st == XState::Running || (m.status.is_update() && st == XState::Committed) {
            keep.push(*m);
        }
    }
    keep.push(Member { xid, status });
    create_of(keep, env)
}

/// heap_lock_tuple's check before it changes anything: whether the current transaction (or
/// a subtransaction of it) already holds a lock on the version at least as strong as `mode`.
pub fn already_locked(old: Xmax, mode: RowLockMode, env: &mut dyn Env) -> bool {
    if old.raw == 0 {
        return false;
    }
    if old.is_multi() {
        let members = env.members(old.raw as u32);
        return members
            .iter()
            .any(|m| m.status.mode() >= mode && env.is_current(m.xid) && env.state(m.xid) == XState::Running);
    }
    // (Our update makes a later version the one to lock: not this one.)
    if !old.lock_only() || !env.is_current(old.raw) || env.state(old.raw) != XState::Running {
        return false;
    }
    old.single_status().mode() >= mode
}

/// compute_new_xmax_infomask: the xmax a version with xmax `old` gets when `xid` locks it in
/// `mode` (`is_update`: updates or deletes it, its lock mode then NoKeyUpdate or Update).
pub fn new_xmax(old: Xmax, xid: u64, mut mode: RowLockMode, is_update: bool, env: &mut dyn Env) -> Xmax {
    if old.raw != 0 {
        if old.is_multi() {
            let multi = old.raw as u32;
            let members = env.members(multi);
            // (MultiXactIdIsRunning: a member in progress.)
            let running = members.iter().any(|m| env.state(m.xid) == XState::Running);
            let gone = !running && {
                let u = update_xid(&members);
                old.lock_only() || u == 0 || env.state(u) != XState::Committed
            };
            if !gone {
                return expand(multi, &members, xid, MxStatus::of(mode, is_update), env);
            }
        } else {
            let x = old.raw;
            match env.state(x) {
                XState::Running => {
                    let old_status = old.single_status();
                    if x != xid {
                        return create_of(
                            vec![Member { xid: x, status: old_status }, Member { xid, status: MxStatus::of(mode, is_update) }],
                            env,
                        );
                    }
                    // Our own lock: the stronger of both.
                    mode = mode.max(old_status.mode());
                }
                XState::Committed if !old.lock_only() => {
                    // A committed update stays the updater.
                    return create_of(
                        vec![
                            Member { xid: x, status: old.single_status() },
                            Member { xid, status: MxStatus::of(mode, is_update) },
                        ],
                        env,
                    );
                }
                _ => {}
            }
        }
    }
    Xmax::single(xid, mode, is_update)
}

/// heap_update's xmax for the new version: the old version's, when lockers of it are still
/// running (they lock the new version too: our own lock, or KEY SHARE lockers an update
/// leaving the key alone didn't wait for), else none.
pub fn carried_xmax(old: Xmax, env: &mut dyn Env) -> Xmax {
    if old.raw == 0 {
        return Xmax::NONE;
    }
    if old.is_multi() {
        let multi = old.raw as u32;
        let members = env.members(multi);
        if members.iter().any(|m| env.state(m.xid) == XState::Running) {
            return Xmax::of_multi(multi, &members);
        }
        return Xmax::NONE;
    }
    if old.lock_only() && env.state(old.raw) == XState::Running {
        return Xmax { raw: old.raw, bits: LOCK_ONLY | KEYSHR };
    }
    Xmax::NONE
}

/// The updating transaction a version's xmax names (HeapTupleHeaderGetUpdateXid; 0: none,
/// a lock only).
pub fn updater(x: Xmax, members: impl FnOnce(u32) -> Option<Arc<[Member]>>) -> u64 {
    if x.raw == 0 || x.lock_only() {
        return 0;
    }
    if x.is_multi() {
        return members(x.raw as u32).map_or(0, |m| update_xid(&m));
    }
    x.raw
}

/// VACUUM's cutoffs, as freezing reads them (VacuumCutoffs).
#[derive(Clone, Copy, Debug)]
pub struct Cutoffs {
    /// OldestXmin: transactions before it are settled for everyone.
    pub oldest_xmin: u64,
    /// FreezeLimit: xids before it must be frozen.
    pub freeze_limit: u64,
    /// OldestMxact: the oldest multi a running transaction may still need.
    pub oldest_mxact: u32,
    /// MultiXactCutoff: multis before it must be frozen.
    pub multi_cutoff: u32,
}

/// MultiXactIdPrecedes.
pub fn precedes(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

/// What freezing does to a version's xmax (heap_prepare_freeze_tuple's xmax half, with
/// FreezeMultiXactId and heap_tuple_should_freeze).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frozen {
    /// The xmax the version gets if its page is frozen.
    pub xmax: Xmax,
    /// Whether it makes the page's freezing mandatory (freeze_required).
    pub required: bool,
}

/// Freezing `x` with cutoffs `c`.
pub fn freeze_xmax(x: Xmax, c: &Cutoffs, env: &mut dyn Env) -> Frozen {
    let keep = Frozen { xmax: x, required: false };
    if x.raw == 0 {
        return keep;
    }
    if !x.is_multi() {
        // (A lock's, or an aborted update's: a committed one before OldestXmin is pruned.)
        let gone = x.raw < c.oldest_xmin;
        return Frozen {
            xmax: if gone { Xmax::NONE } else { x },
            required: x.raw < c.freeze_limit,
        };
    }
    let multi = x.raw as u32;
    if precedes(multi, c.oldest_mxact) {
        // No member can be running: a lock goes, an update stays as its xid alone.
        let members = env.members(multi);
        let upd = if x.lock_only() { 0 } else { update_xid(&members) };
        let xmax = if upd == 0 || upd < c.oldest_xmin {
            Xmax::NONE
        } else {
            Xmax { raw: upd, bits: x.bits & KEYS_UPDATED }
        };
        return Frozen { xmax, required: true };
    }
    let members = env.members(multi);
    if members.is_empty() {
        return Frozen { xmax: Xmax::NONE, required: true };
    }
    let need = members.iter().any(|m| m.xid < c.freeze_limit) || precedes(multi, c.multi_cutoff);
    if !need {
        return keep;
    }
    let mut keep_members: Vec<Member> = Vec::new();
    let (mut lockers, mut upd, mut upd_status) = (false, 0u64, MxStatus::NoKeyUpdate);
    for m in members.iter() {
        let st = env.state(m.xid);
        if !m.status.is_update() {
            if st == XState::Running {
                keep_members.push(*m);
                lockers = true;
            }
        } else if st != XState::Aborted {
            upd = m.xid;
            upd_status = m.status;
            keep_members.push(*m);
        }
    }
    let xmax = if keep_members.is_empty() {
        Xmax::NONE
    } else if upd != 0 && !lockers {
        Xmax { raw: upd, bits: if upd_status == MxStatus::Update { KEYS_UPDATED } else { 0 } }
    } else {
        create_of(keep_members, env)
    };
    Frozen { xmax, required: true }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct T {
        states: HashMap<u64, XState>,
        current: Vec<u64>,
        multis: MultiXacts,
    }

    impl Env for T {
        fn state(&mut self, xid: u64) -> XState {
            self.states.get(&xid).copied().unwrap_or(XState::Committed)
        }
        fn is_current(&mut self, xid: u64) -> bool {
            self.current.contains(&xid)
        }
        fn members(&mut self, multi: u32) -> Arc<[Member]> {
            self.multis.members(multi).unwrap_or_else(|| Arc::from([]))
        }
        fn create(&mut self, members: &[Member]) -> u32 {
            self.multis.create(members)
        }
    }

    // The values are a PostgreSQL 18.6's (HANDOFF "MultiXact").
    #[test]
    fn lockers_make_multis_as_postgres_does() {
        let mut e = T::default();
        for x in [766, 767, 768] {
            e.states.insert(x, XState::Running);
        }
        let a = new_xmax(Xmax::NONE, 766, RowLockMode::KeyShare, false, &mut e);
        assert_eq!(a, Xmax { raw: 766, bits: LOCK_ONLY | KEYSHR });
        let b = new_xmax(a, 767, RowLockMode::Share, false, &mut e);
        assert_eq!(b, Xmax { raw: 1, bits: IS_MULTI | LOCK_ONLY | SHR });
        let c = new_xmax(b, 768, RowLockMode::KeyShare, false, &mut e);
        assert_eq!(c.raw, 2);
        assert_eq!(e.members(2).len(), 3);
        assert_eq!((e.multis.next, e.multis.next_offset), (3, 6));
        for x in [766, 767, 768] {
            e.states.insert(x, XState::Committed);
        }
        let d = new_xmax(c, 769, RowLockMode::Update, false, &mut e);
        assert_eq!(d, Xmax { raw: 769, bits: LOCK_ONLY | EXCL | KEYS_UPDATED });
    }

    #[test]
    fn own_lock_then_update() {
        let mut e = T::default();
        e.states.insert(775, XState::Running);
        e.current.push(775);
        let l = new_xmax(Xmax::NONE, 775, RowLockMode::Update, false, &mut e);
        assert!(already_locked(l, RowLockMode::KeyShare, &mut e));
        let u = new_xmax(l, 775, RowLockMode::NoKeyUpdate, true, &mut e);
        assert_eq!(u, Xmax { raw: 775, bits: KEYS_UPDATED });
        assert_eq!(carried_xmax(l, &mut e), Xmax { raw: 775, bits: LOCK_ONLY | KEYSHR });
    }
}
