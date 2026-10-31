//! Table synchronization (tablesync.c): the leader starts a table synchronization worker per
//! table not synchronized yet (at most max_sync_workers_per_subscription) and keeps applying
//! meanwhile. A worker copies its table under a permanent slot's snapshot
//! (`pg_<subid>_sync_<relid>_<sysid>`), commits the copy with state 'f' (FINISHEDCOPY) and
//! its synchronization origin at the slot's consistent point, then waits for the leader
//! (SYNCWAIT, in memory). The leader answers at its next transaction boundary (CATCHUP with
//! its own position) and waits; the worker streams its slot, applying its table's changes,
//! until it reaches that position, records 's' (SYNCDONE) there and drops its slot. The
//! leader applies the table's changes of transactions committing after that point, and the
//! table is 'r' once it has passed it. A worker that fails after the copy resumes from its
//! origin; one that fails during it drops its slot and copies again.

use super::{Severity, Worker, Workers, lsn, stop_check, sync_main, walrcv};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A table synchronization worker the leader started.
struct Run {
    relid: u32,
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<bool>,
}

/// The leader's table synchronization workers, stopped and waited for when it ends.
#[derive(Default)]
pub(super) struct Syncs {
    runs: Vec<Run>,
    /// When each table's last worker started (last_start_times: one again only after
    /// wal_retrieve_retry_interval).
    started: HashMap<u32, Instant>,
}

impl Syncs {
    pub(super) fn stop_all(&mut self) {
        for r in &self.runs {
            r.stop.store(true, Ordering::SeqCst);
        }
        for r in self.runs.drain(..) {
            let _ = r.thread.join();
        }
    }
}

impl Drop for Syncs {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// ReplicationSlotNameForTablesync (the subscriber's system identifier).
pub(super) fn slot_name(subid: u32, relid: u32) -> String {
    format!("pg_{subid}_sync_{relid}_{}", crate::repl::SYSTEM_IDENTIFIER)
}

impl Workers {
    /// A table synchronization worker's in-memory state (relstate, relstate_lsn): 'w'
    /// SYNCWAIT, 'c' CATCHUP.
    fn relstate(&self, subid: u32, relid: u32) -> Option<(char, u64)> {
        self.relstates.lock().get(&(subid, relid)).copied()
    }

    /// logicalrep_worker_stop for table `relid`'s synchronization worker (a refresh removed
    /// the table): it stops, and this waits for it (at most 30 s).
    pub fn stop_sync(&self, subid: u32, relid: u32) {
        let Some(f) = self.sync_stops.lock().get(&(subid, relid)).cloned() else { return };
        f.store(true, Ordering::SeqCst);
        let until = Instant::now() + Duration::from_secs(30);
        while self.sync_stops.lock().contains_key(&(subid, relid)) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn set_relstate(&self, subid: u32, relid: u32, s: Option<(char, u64)>) {
        let mut m = self.relstates.lock();
        match s {
            Some(s) => m.insert((subid, relid), s),
            None => m.remove(&(subid, relid)),
        };
    }
}

impl Worker<'_> {
    /// process_syncing_tables_for_apply at `current` (the end of the last transaction
    /// applied, or what was received when idle): tables synchronized at or before it become
    /// ready; a worker waiting for the leader is told to catch up to it, and waited for; a
    /// table without a worker gets one.
    pub(super) fn sync_for_apply(&mut self, current: u64) -> Result<(), String> {
        let subid = self.sub.oid;
        let mut i = 0;
        while i < self.syncs.runs.len() {
            if self.syncs.runs[i].thread.is_finished() {
                let r = self.syncs.runs.swap_remove(i);
                let _ = r.thread.join();
                self.workers.set_relstate(subid, r.relid, None);
            } else {
                i += 1;
            }
        }
        if self.syncs.runs.is_empty() && self.states.values().all(|(s, _)| *s == 'r') {
            return Ok(());
        }
        // (A worker of a table a refresh removed stops.)
        for r in &self.syncs.runs {
            if !self.states.contains_key(&r.relid) {
                r.stop.store(true, Ordering::SeqCst);
            }
        }
        self.ready_tables(current, true)?;
        let mut todo: Vec<u32> = self.states.iter().filter(|(_, (s, _))| matches!(s, 'i' | 'd' | 'f')).map(|(r, _)| *r).collect();
        todo.sort_unstable();
        let stopping = self.stop.load(Ordering::SeqCst);
        for relid in todo {
            if self.syncs.runs.iter().any(|r| r.relid == relid) {
                if let Some(('w', l)) = self.workers.relstate(subid, relid) {
                    self.workers.set_relstate(subid, relid, Some(('c', l.max(current))));
                    self.wait_syncdone(relid);
                }
                continue;
            }
            let max = self.setting_int("max_sync_workers_per_subscription").max(0) as usize;
            if stopping || self.syncs.runs.len() >= max {
                continue;
            }
            if self.syncs.started.get(&relid).is_some_and(|t| t.elapsed() < super::RETRY_INTERVAL) {
                continue;
            }
            if self.workers.slots_used() >= self.setting_int("max_logical_replication_workers").max(0) as usize {
                self.log(
                    Severity::Warning,
                    "out of logical replication worker slots\nHINT:  You might need to increase \"max_logical_replication_workers\".",
                );
                break;
            }
            let stop = Arc::new(AtomicBool::new(false));
            let (engine, workers, sub, st) = (Arc::downgrade(self.e), self.workers.clone(), self.sub.clone(), stop.clone());
            // (Registered before it starts: it removes itself as it ends.)
            self.workers.sync_stops.lock().insert((subid, relid), stop.clone());
            let thread = std::thread::Builder::new()
                .name(format!("roll-sync-{relid}"))
                .spawn(move || sync_main(engine, workers, sub, relid, st))
                .map_err(|e| {
                    self.workers.sync_stops.lock().remove(&(subid, relid));
                    format!("could not start table synchronization worker: {e}")
                })?;
            self.syncs.started.insert(relid, Instant::now());
            self.syncs.runs.push(Run { relid, stop, thread });
        }
        Ok(())
    }

    /// wait_for_relation_state_change(SYNCDONE): until the worker recorded it or ended.
    fn wait_syncdone(&mut self, relid: u32) {
        loop {
            self.load_states();
            if !matches!(self.states.get(&relid), Some(('i' | 'd' | 'f', _))) || self.stop.load(Ordering::SeqCst) {
                return;
            }
            if self.syncs.runs.iter().find(|r| r.relid == relid).is_none_or(|r| r.thread.is_finished()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A table synchronization worker's run (LogicalRepSyncTableStart, then the catch-up of
    /// process_syncing_tables_for_sync): table `relid` synchronized, state 's'.
    pub(super) fn sync_table(&mut self, relid: u32) -> Result<(), String> {
        let cat = self.sess.state.db.catalog();
        let Some(t) = cat.tables.get(&relid) else { return Ok(()) };
        let (nsp, rel) = (cat.schema_name(t.schema).to_string(), t.name.clone());
        let local_cols: Vec<String> = t.columns.iter().filter(|c| !c.dropped).map(|c| c.name.clone()).collect();
        drop(cat);
        self.log(Severity::Log, &format!(
            "logical replication table synchronization worker for subscription \"{}\", table \"{rel}\" has started",
            self.sub.name
        ));
        let subid = self.sub.oid;
        let origin = format!("pg_{subid}_{relid}");
        let state = self.states.get(&relid).map_or('i', |s| s.0);
        // (The copy commits under the table's synchronization origin, as a tablesync worker's
        // does: its rows' commit origin isn't the subscription's; the catch-up's either.)
        if state == 'f' && self.repl.origins.by_name(&origin).is_none() {
            return Err(format!("replication origin \"{origin}\" does not exist"));
        }
        self.use_origin(&origin)?;
        self.origin = self.repl.origins.by_name(&origin).map(|o| o.id).ok_or("no synchronization origin")?;
        let start = if state == 'f' {
            // The copy was done (its worker failed later): the catch-up again, from the origin.
            self.load_progress();
            self.progress()
        } else {
            self.copy_table(relid, &nsp, &rel, &local_cols, state == 'd')?
        };
        // SYNCWAIT until the leader says how far to catch up (CATCHUP).
        self.workers.set_relstate(subid, relid, Some(('w', start)));
        let target = loop {
            if let Some(('c', l)) = self.workers.relstate(subid, relid) {
                break l;
            }
            stop_check(self.stop).map_err(|e| e.message.to_string())?;
            std::thread::sleep(Duration::from_millis(10));
        };
        let done = if start >= target {
            start
        } else {
            self.catchup = Some(target);
            self.stream()?;
            self.synced_at.ok_or("terminating logical replication worker due to administrator command")?
        };
        // SYNCDONE there; then the slot and the origin go.
        self.query("BEGIN")?;
        self.set_states(&[(relid, 's', done)])?;
        self.query("COMMIT")?;
        let slot = slot_name(subid, relid);
        let mut c = walrcv::connect(&self.sub.conninfo, &slot, &self.env).map_err(|m| {
            format!("table synchronization worker for subscription \"{}\" could not connect to the publisher: {m}", self.sub.name)
        })?;
        self.drop_sync_slot(&mut c, &slot, false)?;
        let _ = self.repl.origins.session_reset(self.pid);
        let _ = self.repl.origins.drop_origin(&origin, true);
        tracing::debug!(relid, "table synchronized at {}", lsn(done));
        self.log(Severity::Log, &format!(
            "logical replication table synchronization worker for subscription \"{}\", table \"{rel}\" has finished",
            self.sub.name,
        ));
        Ok(())
    }

    /// ReplicationSlotDropAtPubNode: a NOTICE once dropped; a missing slot with `missing_ok`
    /// a LOG line, else an error.
    pub(super) fn drop_sync_slot(&self, c: &mut crate::remote::conn::Conn, slot: &str, missing_ok: bool) -> Result<(), String> {
        let stop = self.stop;
        let intr = move || stop_check(stop);
        match walrcv::drop_slot(c, slot, &intr) {
            Ok(()) => {
                self.log(Severity::Notice, &format!("dropped replication slot \"{slot}\" on publisher"));
                Ok(())
            }
            Err(walrcv::Fail::Remote { msg, sqlstate }) if missing_ok && sqlstate.as_deref() == Some("42704") => {
                self.log(Severity::Log, &format!("could not drop replication slot \"{slot}\" on publisher: {msg}"));
                Ok(())
            }
            Err(e) => Err(format!("could not drop replication slot \"{slot}\" on publisher: {}", e.msg())),
        }
    }
}
