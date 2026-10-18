//! Cursors (`cursor.c`): the user cursor list FT.AGGREGATE and FT.SEARCH WITHCURSOR reserve
//! into and FT.CURSOR reads, deletes and collects. A cursor holds its request's execution state (opaque
//! here), the index it was opened on (a weak reference: the spec's id, so a dropped and
//! recreated index is not the same one) and its idle deadline.
//!
//! Ids are the module's: `lrand48` seeded with the process id (`srand48(getpid())`), masked to
//! 31 bits, made odd, plus one (user cursors are even; odd ids belong to the coordinator's list,
//! which a standalone server never fills).
//!
//! The module reaps idle cursors with a timer armed for the earliest deadline, so a command
//! never sees an expired one; here every operation that can observe the list first drops the
//! expired cursors, which is what the timer would have done by then.

use bytes::Bytes;
use std::any::Any;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A cursor's execution state (the module's `AREQ *execState`), opaque to the list.
pub struct Exec(pub Box<dyn Any>);

// SAFETY: execution states live in the engine and are only touched with its lock held (the
// engine moves between threads behind a mutex, never shared); the `Rc`s they hold are never
// cloned outside it.
unsafe impl Send for Exec {}

impl std::fmt::Debug for Exec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Exec")
    }
}

/// `Cursor`.
#[derive(Debug)]
pub struct Cursor {
    pub id: u64,
    /// The spec the cursor was opened on (`spec_ref`).
    pub spec_id: u64,
    /// The index's name, to find the spec again.
    pub index: Bytes,
    /// `timeoutIntervalMs`.
    pub max_idle_ms: u32,
    /// When the paused cursor expires (`nextTimeoutNs`); `None` while it runs.
    deadline: Option<Instant>,
    pub exec: Option<Exec>,
}

/// `CursorList` (`g_CursorsList`).
#[derive(Debug, Default)]
pub struct Cursors {
    /// `lookup`: every cursor, idle or running.
    lookup: HashMap<u64, Cursor>,
    /// The `drand48` family's 48-bit state, seeded on first use.
    rand: Option<u64>,
}

impl Cursors {
    /// The idle-sweep timer's work, as of now (`Cursors_GCInternal` over expired idle ones).
    pub fn collect_expired(&mut self) {
        let now = Instant::now();
        self.lookup.retain(|_, c| c.deadline.is_none_or(|d| d > now));
    }

    /// `CursorList_Empty` (`Indexes_Free`: a flush, an RDB load): every cursor goes; the id
    /// sequence goes on.
    pub fn clear(&mut self) {
        self.lookup.clear();
    }

    /// `lrand48`.
    fn lrand48(&mut self) -> u64 {
        let x = self.rand.get_or_insert_with(|| ((std::process::id() as u64 & 0xffff_ffff) << 16) | 0x330e);
        *x = x.wrapping_mul(0x5_deec_e66d).wrapping_add(0xb) & ((1 << 48) - 1);
        *x >> 17
    }

    /// `CursorList_GenerateId`.
    fn generate_id(&mut self) -> u64 {
        loop {
            let id = ((self.lrand48() & 0x7fff_ffff) | 1) + 1;
            if !self.lookup.contains_key(&id) {
                return id;
            }
        }
    }

    /// The cursors opened on a spec (`spec->activeCursors`).
    pub fn index_total(&self, spec_id: u64) -> usize {
        self.lookup.values().filter(|c| c.spec_id == spec_id).count()
    }

    /// `Cursors_Reserve`: a running cursor on the spec, or the limit's error (`None`).
    pub fn reserve(&mut self, spec_id: u64, index: Bytes, max_idle_ms: u32, limit: i64) -> Option<u64> {
        self.collect_expired();
        if self.index_total(spec_id) as i64 >= limit {
            return None;
        }
        let id = self.generate_id();
        self.lookup.insert(id, Cursor { id, spec_id, index, max_idle_ms, deadline: None, exec: None });
        Some(id)
    }

    /// `Cursors_TakeForExecution`: an idle cursor, out of the list while it runs.
    pub fn take(&mut self, id: u64) -> Option<Cursor> {
        self.collect_expired();
        match self.lookup.get(&id) {
            Some(c) if c.deadline.is_some() => self.lookup.remove(&id),
            _ => None,
        }
    }

    /// The running cursor `id`, reserved but not yet paused (its first chunk).
    pub fn take_running(&mut self, id: u64) -> Option<Cursor> {
        self.lookup.remove(&id)
    }

    /// `Cursor_Pause`: idle again, until its interval passes.
    pub fn pause(&mut self, mut c: Cursor) {
        c.deadline = Some(Instant::now() + Duration::from_millis(c.max_idle_ms as u64));
        self.lookup.insert(c.id, c);
    }

    /// `Cursors_Purge`: whether the cursor existed.
    pub fn purge(&mut self, id: u64) -> bool {
        self.collect_expired();
        self.lookup.remove(&id).is_some()
    }

    /// `FT.CURSOR GC`: the timer leaves nothing expired to collect, and the coordinator's
    /// (empty) list always scans, so the reply is 0.
    pub fn gc(&mut self) -> i64 {
        self.collect_expired();
        0
    }

    /// `Cursors_RenderStats`: (global idle, global total).
    pub fn stats(&mut self) -> (usize, usize) {
        self.collect_expired();
        let idle = self.lookup.values().filter(|c| c.deadline.is_some()).count();
        (idle, self.lookup.len())
    }
}
