//! Row locks of tables too big to model (`EstHeap::locks`): the xmax PostgreSQL leaves in a
//! locked version (the locker's xid with HEAP_XMAX_LOCK_ONLY, or a MultiXactId), which the
//! heap model keeps in its tuples (`multixact`). Past the model there is no tuple: each row
//! whose latest version has such an xmax keeps it here, with that version's xmin, and in the
//! store as the modelled tables' xmax records are (`multixact::XMAX_PREFIX`, at `EST_AT`), so
//! a restart or a crash leaves it as it was. An update's new version carries the lockers still
//! running (heap_update); a delete or an update drops the old version's. Versions an update
//! or delete replaced keep no xmax of their updater here (a snapshot older than the update
//! reads 0), and a running updater's or deleter's isn't shown.

use super::{EstHeap, HashedMap, MxEnv, RelFiles, Tid, key_hash, vis_of};
use crate::error::Result;
use crate::exec::ExecCtx;
use crate::catalog::Table;
use crate::txn::Database;
use crate::txn::locks::RowLockMode;
use crate::txn::multixact::{self as mx, Xmax};

/// A row's lock-only xmax: the version's xmin (0: not known, any version's), the xmax, and
/// the TID its record is kept under (`mx::EST_AT`, or the model's when the table was).
#[derive(Clone, Copy, Debug)]
pub(super) struct EstLock {
    xmin: u64,
    x: Xmax,
    at: Tid,
}

pub(super) type Locks = HashedMap<EstLock>;

/// The record of `l`.
fn record(l: &EstLock) -> Vec<u8> {
    mx::encode_xmax(l.x)
}

/// The key of row `row`'s record of `l`.
fn record_key(table: u32, row: &[u8], l: &EstLock) -> Vec<u8> {
    if l.at == mx::EST_AT { mx::est_xmax_key(table, row, l.xmin) } else { mx::xmax_key(table, row, l.at) }
}

/// The locks table `oid`'s records keep, read when its estimate is made: records of the model's
/// (a version's TID, from before it outgrew the model) name any version of the row; at
/// `EST_AT`, the version of the xmin in the key. A row with records of several versions (a
/// locker that committed after an update) keeps the latest version's.
pub(super) fn load(db: &Database, oid: u32) -> Locks {
    let mut out = Locks::default();
    let (a, b) = mx::xmax_range(oid);
    for r in db.store.scan(&a, &b, db.store.visible_ts()) {
        let Ok((k, v)) = r else { break };
        let (Some((row, at)), Some(x)) = (mx::split_xmax_key(&k), mx::decode_xmax(&v)) else { continue };
        let (row, xmin) = if at == mx::EST_AT {
            match mx::split_est_row(row) {
                Some(r) => r,
                None => continue,
            }
        } else {
            (row, 0)
        };
        let l = EstLock { xmin, x, at };
        match out.entry(key_hash(row)) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                if at == mx::EST_AT && (e.get().at != mx::EST_AT || e.get().xmin < xmin) {
                    e.insert(l);
                }
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(l);
            }
        }
    }
    out
}

/// Table `oid`'s estimate `est` was made: its kept locks are read (`load`).
pub(super) fn loaded(db: &Database, oid: u32, est: &mut EstHeap) {
    let locks = load(db, oid);
    if !locks.is_empty() {
        db.relfiles.note_locks(oid);
    }
    est.locks = locks;
}

/// The xmax of row `key`'s version written by `xmin`, when the table is too big to model and
/// the version has a lock's.
pub fn xmax_of(table: u32, key: &[u8], xmin: u64, db: &Database) -> Option<Xmax> {
    if !db.relfiles.maybe_locks(table) {
        return None;
    }
    let rf = db.relfiles.of(table).read();
    let l = rf.estimated.get(&table)?.locks.get(&key_hash(key))?;
    (l.xmin == 0 || l.xmin == xmin).then_some(l.x)
}

/// heap_lock_tuple's mark on row `key` of `t`, too big to model, just locked in `mode`
/// (`sizing::row_locked`): its latest version gets the locker, or a MultiXactId with the
/// lockers still of interest. (A KEY SHARE lock past a running update doesn't go on to the
/// update's version: there is no chain past the model.)
pub(super) fn row_locked(t: &Table, key: &[u8], mode: RowLockMode, ctx: &ExecCtx) -> Result<()> {
    let db = &ctx.sess.db;
    if !db.relfiles.of(t.oid).read().estimated.contains_key(&t.oid) {
        return Ok(());
    }
    let Some((_, Some(bytes))) = ctx.txn.borrow().get_latest(key)? else {
        return Ok(());
    };
    let xmin = crate::exec::scan::row_xmin(&bytes)?;
    let xid = ctx.txn.borrow_mut().cur_xid();
    let v = vis_of(db.xids(), t, ctx);
    let h = key_hash(key);
    let mut rf = db.relfiles.of(t.oid).write();
    let Some(est) = rf.estimated.get_mut(&t.oid) else {
        return Ok(());
    };
    let found = est.locks.get(&h).copied();
    let old = found.filter(|l| l.xmin == 0 || l.xmin == xmin).map_or(Xmax::NONE, |l| l.x);
    let Ok(mut txn) = ctx.txn.try_borrow_mut() else {
        return Ok(());
    };
    let mut env = MxEnv { v: &v, txn: &mut txn };
    if mx::already_locked(old, mode, &mut env) {
        return Ok(());
    }
    let x = mx::new_xmax(old, xid, mode, false, &mut env);
    let l = EstLock { xmin, x, at: found.map_or(mx::EST_AT, |f| f.at) };
    est.locks.insert(h, l);
    db.relfiles.note_locks(t.oid);
    txn.put_state(record_key(t.oid, key, &l), Some(record(&l)))
}

/// Row `key`'s latest version, too big to model, goes (a delete, or an update: `new`, the new
/// version's key, written by `xid`): its xmax record goes, and an update's new version gets the
/// lockers still running (heap_update's xmax for it). Takes the shard's lock only when some
/// table of the shard's group has locks.
pub(super) fn replaced(t: &Table, key: &[u8], new: Option<&[u8]>, ctx: &ExecCtx) -> Result<()> {
    let db = &ctx.sess.db;
    if !db.relfiles.maybe_locks(t.oid) {
        return Ok(());
    }
    let gone = {
        let mut rf = db.relfiles.of(t.oid).write();
        let RelFiles { estimated, .. } = &mut *rf;
        match estimated.get_mut(&t.oid) {
            Some(est) if !est.locks.is_empty() => est.locks.remove(&key_hash(key)),
            _ => None,
        }
    };
    let Some(old) = gone else {
        return Ok(());
    };
    ctx.txn.borrow_mut().put_state(record_key(t.oid, key, &old), None)?;
    let Some(new_key) = new else {
        return Ok(());
    };
    let xid = ctx.txn.borrow_mut().cur_xid();
    let v = vis_of(db.xids(), t, ctx);
    let carried = {
        let mut txn = ctx.txn.borrow_mut();
        mx::carried_xmax(old.x, &mut MxEnv { v: &v, txn: &mut txn })
    };
    if carried.raw == 0 {
        return Ok(());
    }
    let l = EstLock { xmin: xid, x: carried, at: mx::EST_AT };
    if let Some(est) = db.relfiles.of(t.oid).write().estimated.get_mut(&t.oid) {
        est.locks.insert(key_hash(new_key), l);
    }
    ctx.txn.borrow_mut().put_state(record_key(t.oid, new_key, &l), Some(record(&l)))
}

/// VACUUM of `t`, too big to model: each kept xmax as heap_prepare_freeze_tuple leaves it,
/// where the freezing is required or the VACUUM freezes every page it reads (FREEZE, or an
/// aggressive one); the oldest MultiXactId left (NewRelminMxid's hold), if any.
pub(super) fn vacuumed(t: &Table, freeze: bool, aggressive: bool, ctx: &ExecCtx) -> Result<Option<u32>> {
    let db = &ctx.sess.db;
    if !db.relfiles.maybe_locks(t.oid) {
        return Ok(None);
    }
    let v = super::vis_for(db.xids(), t.oid, ctx);
    let cut = super::freeze_cutoffs(&v, freeze, ctx);
    let aggressive = aggressive || freeze;
    let mut changed: HashedMap<EstLock> = HashedMap::default();
    let mut min: Option<u32> = None;
    {
        let mut rf = db.relfiles.of(t.oid).write();
        let Some(est) = rf.estimated.get_mut(&t.oid) else {
            return Ok(None);
        };
        let Ok(mut txn) = ctx.txn.try_borrow_mut() else {
            return Ok(None);
        };
        est.locks.retain(|&h, l| {
            let f = mx::freeze_xmax(l.x, &cut, &mut MxEnv { v: &v, txn: &mut txn });
            if (f.required || aggressive) && f.xmax != l.x {
                l.x = f.xmax;
                changed.insert(h, *l);
            }
            if l.x.is_multi() && min.is_none_or(|m| mx::precedes(l.x.raw as u32, m)) {
                min = Some(l.x.raw as u32);
            }
            l.x.raw != 0
        });
    }
    if changed.is_empty() {
        return Ok(min);
    }
    // (The records follow: found by their keys' hashes in the table's record range.)
    let (a, b) = mx::xmax_range(t.oid);
    let mut txn = ctx.txn.borrow_mut();
    let keys: Vec<Vec<u8>> = db.store.scan(&a, &b, db.store.visible_ts()).filter_map(|r| r.ok()).map(|(k, _)| k.to_vec()).collect();
    for k in keys {
        let Some((row, at)) = mx::split_xmax_key(&k) else { continue };
        let row = if at == mx::EST_AT { mx::split_est_row(row).map_or(row, |r| r.0) } else { row };
        if let Some(l) = changed.get(&key_hash(row)).filter(|l| l.at == at && record_key(t.oid, row, l) == k) {
            let rec = (l.x.raw != 0).then(|| record(l));
            txn.put_state(k, rec)?;
        }
    }
    Ok(min)
}

/// The oldest MultiXactId a kept xmax of `t` names, past the model.
pub fn min_multi(table: u32, db: &Database) -> Option<u32> {
    if !db.relfiles.maybe_locks(table) {
        return None;
    }
    let rf = db.relfiles.of(table).read();
    let est = rf.estimated.get(&table)?;
    est.locks
        .values()
        .filter(|l| l.x.is_multi())
        .map(|l| l.x.raw as u32)
        .reduce(|a, b| if mx::precedes(a, b) { a } else { b })
}
