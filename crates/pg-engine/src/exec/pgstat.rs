//! The cumulative statistics system (Postgres's pgstat): the counters of tables, indexes,
//! functions and the database that the pg_stat_* views show.
//!
//! As in Postgres, a backend counts into its own pending entries (`Pending`, in the
//! session), the transactional counts (rows inserted, updated, deleted, truncations) per
//! subtransaction level (`XactStats`, in the transaction), and flushes them into the
//! database's shared entries (`Shared`) when its transaction ends
//! (AtEOXact_PgStat + pgstat_report_stat). VACUUM and ANALYZE report into the shared
//! entries directly. Readers see the shared entries under stats_fetch_consistency: as they
//! are (none), each entry as first read in the transaction (cache), or a snapshot of all of
//! them taken at the first read (snapshot).

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A table's (or index's) counts a backend has not flushed yet (PgStat_TableCounts).
#[derive(Clone, Copy, Debug, Default)]
pub struct TabCounts {
    pub numscans: i64,
    pub tuples_returned: i64,
    pub tuples_fetched: i64,
    pub tuples_inserted: i64,
    pub tuples_updated: i64,
    pub tuples_deleted: i64,
    pub tuples_hot_updated: i64,
    pub tuples_newpage_updated: i64,
    pub truncdropped: bool,
    pub delta_live_tuples: i64,
    pub delta_dead_tuples: i64,
    pub changed_tuples: i64,
    pub blocks_fetched: i64,
    pub blocks_hit: i64,
}

/// A table's (or index's) shared entry (PgStat_StatTabEntry).
#[derive(Clone, Debug, Default)]
pub struct TabEntry {
    pub numscans: i64,
    pub lastscan: Option<i64>,
    pub tuples_returned: i64,
    pub tuples_fetched: i64,
    pub tuples_inserted: i64,
    pub tuples_updated: i64,
    pub tuples_deleted: i64,
    pub tuples_hot_updated: i64,
    pub tuples_newpage_updated: i64,
    pub live_tuples: i64,
    pub dead_tuples: i64,
    pub mod_since_analyze: i64,
    pub ins_since_vacuum: i64,
    pub blocks_fetched: i64,
    pub blocks_hit: i64,
    pub last_vacuum_time: Option<i64>,
    pub vacuum_count: i64,
    pub last_analyze_time: Option<i64>,
    pub analyze_count: i64,
    pub total_vacuum_time: f64,
    pub total_analyze_time: f64,
    /// What autovacuum's workers did (the auto columns of pg_stat_all_tables).
    pub last_autovacuum_time: Option<i64>,
    pub autovacuum_count: i64,
    pub total_autovacuum_time: f64,
    pub last_autoanalyze_time: Option<i64>,
    pub autoanalyze_count: i64,
    pub total_autoanalyze_time: f64,
}

/// A function's counts (PgStat_FunctionCounts): calls, and times in milliseconds.
#[derive(Clone, Copy, Debug, Default)]
pub struct FuncCounts {
    pub calls: i64,
    pub total_time: f64,
    pub self_time: f64,
}

/// A database's counters (PgStat_StatDBEntry, those kept here).
#[derive(Clone, Debug, Default)]
pub struct DbEntry {
    pub xact_commit: i64,
    pub xact_rollback: i64,
    pub blocks_fetched: i64,
    pub blocks_hit: i64,
    pub tuples_returned: i64,
    pub tuples_fetched: i64,
    pub tuples_inserted: i64,
    pub tuples_updated: i64,
    pub tuples_deleted: i64,
    pub sessions: i64,
    pub parallel_workers_to_launch: i64,
    pub parallel_workers_launched: i64,
    pub stat_reset: Option<i64>,
}

/// The cluster's fixed-numbered statistics (WAL, checkpointer, I/O, SLRU and their reset
/// times) and its WAL position: one per cluster (`Cluster`'s, set on every database it
/// opens), so every database's sessions read and count the same ones, as in PostgreSQL.
#[derive(Clone, Debug, Default)]
pub struct Fixed {
    /// The cluster's WAL positions (`repl::lsn`), when the engine has them: the position
    /// is theirs, persisted ahead and shared with the change stream.
    pub wal: Option<std::sync::Arc<crate::repl::lsn::WalPos>>,
    /// Reset times of the fixed-numbered statistics: archiver, bgwriter, checkpointer,
    /// io, recovery_prefetch, wal, and `slru:<name>`.
    pub resets: HashMap<String, i64>,
    pub checkpoints_requested: i64,
    pub checkpoints_performed: i64,
    pub wal_records: i64,
    pub wal_bytes: i64,
    /// pg_stat_io counts by (backend type, object, context, operation).
    pub io: super::fasthash::HashMap<IoKey, i64>,
    /// pg_stat_slru's blks_zeroed, by SLRU name (the notify queue's pages as NOTIFYs
    /// commit).
    pub slru_zeroed: HashMap<String, i64>,
    /// The WAL position the cluster started at (`wal_lsn`).
    pub lsn_base: u64,
    /// WAL bytes written since then (as pg_stat_wal's wal_bytes, but never reset).
    pub wal_written: u64,
    /// The last checkpoint: its record's and its redo's WAL positions and its time (None:
    /// the one the cluster started from).
    pub checkpoint: Option<(u64, u64, i64)>,
    /// CheckPointDistanceEstimate (bytes), for log_checkpoints' estimate.
    pub distance_estimate: f64,
    /// Backups in progress (pg_backup_start), by the backend running each.
    pub backups: HashMap<i32, Backup>,
}

/// A base backup a backend started (pg_backup_start), until it stops it.
#[derive(Clone, Debug)]
pub struct Backup {
    pub label: String,
    /// Its start (the checkpoint's redo) and checkpoint record's positions, and its time.
    pub start: u64,
    pub checkpoint: u64,
    pub time: i64,
}

/// wal_segment_size.
pub const WAL_SEGMENT_SIZE: u64 = 16 * 1024 * 1024;
/// A segment's long page header: its first record follows it.
const SEGMENT_HEADER: u64 = 0x28;

/// A cluster's `Fixed` statistics, shared by its databases.
pub type ClusterStats = std::sync::Arc<parking_lot::Mutex<Fixed>>;

/// The WAL position a new cluster's first record follows (initdb's checkpoint: the start of
/// the second segment, as pg_control_checkpoint reports it before any checkpoint here).
pub const INITDB_LSN: u64 = 16 * 1024 * 1024 + 0x28;

impl Fixed {
    /// A cluster's statistics, its WAL resuming after `lsn` (where it was at shutdown; 0:
    /// a new cluster): at the next segment, as after a restart's checkpoint.
    pub fn starting_at(lsn: u64) -> ClusterStats {
        const SEG: u64 = 16 * 1024 * 1024;
        let base = if lsn <= INITDB_LSN { INITDB_LSN } else { (lsn / SEG + 1) * SEG + 0x28 };
        std::sync::Arc::new(parking_lot::Mutex::new(Fixed { lsn_base: base, ..Fixed::default() }))
    }

    /// pg_current_wal_lsn: where the next WAL record goes.
    pub fn wal_lsn(&self) -> u64 {
        match &self.wal {
            Some(w) => w.current(),
            None => self.lsn_base.max(INITDB_LSN) + self.wal_written,
        }
    }

    /// pg_stat_wal counts `records` records of `bytes` bytes (the position moved as they
    /// were written: `advance`).
    pub fn add_wal(&mut self, records: i64, bytes: i64) {
        self.wal_records += records;
        self.wal_bytes += bytes;
    }

    /// A transaction's `bytes` of WAL are written: the position moves past them.
    pub fn advance(&mut self, bytes: i64) {
        match &self.wal {
            Some(w) => {
                if bytes > 0 {
                    w.skip(bytes as u64);
                }
            }
            None => self.wal_written += bytes.max(0) as u64,
        }
    }

    /// One record of `bytes` bytes written now: where it starts (past a segment's header,
    /// at a segment's start) and ends.
    pub fn write_record(&mut self, bytes: u64) -> (u64, u64) {
        if self.wal_lsn().is_multiple_of(WAL_SEGMENT_SIZE) {
            self.advance(SEGMENT_HEADER as i64);
        }
        let at = self.wal_lsn();
        self.add_wal(1, bytes as i64);
        self.advance(bytes as i64);
        (at, at + bytes)
    }

    /// pg_switch_wal (RequestXLogSwitch): a switch record ends the segment, and the WAL
    /// goes on at the next one; its end (or, at a segment's start already, that position).
    pub fn switch(&mut self) -> u64 {
        let at = self.wal_lsn();
        if at.is_multiple_of(WAL_SEGMENT_SIZE) {
            return at;
        }
        let (_, end) = self.write_record(24);
        self.advance((end.next_multiple_of(WAL_SEGMENT_SIZE) - end) as i64);
        end
    }

    /// A checkpoint at `time`: its redo point, a running-transactions record, then its own
    /// record. Returns the redo and the checkpoint record's positions.
    pub fn checkpoint_at(&mut self, time: i64) -> (u64, u64) {
        let (redo, _) = self.write_record(0x58);
        let (ckpt, _) = self.write_record(122);
        self.checkpoint = Some((ckpt, redo, time));
        (redo, ckpt)
    }
}

/// The shared statistics of a database (and, through `fixed`, the cluster's fixed ones).
#[derive(Clone, Debug, Default)]
pub struct Shared {
    pub tables: HashMap<u32, TabEntry>,
    /// The catalog version whose dropped relations' and functions' entries were last
    /// removed (`end_xact`).
    pub pruned_at: u64,
    /// That catalog (the next prune looks at what changed since).
    pub pruned: Option<std::sync::Arc<crate::catalog::Catalog>>,
    pub funcs: HashMap<u32, FuncCounts>,
    pub db: DbEntry,
    /// Database 0: shared relations' counts.
    pub shared_db: DbEntry,
    /// The cluster's fixed-numbered statistics (locked after this database's).
    pub fixed: ClusterStats,
    /// Backends' counts kept unflushed when they went idle, by process id.
    pub unflushed: HashMap<i32, Unflushed>,
    /// When a reader flushed an idle backend's counts (its idle timeout), until the
    /// backend next reports.
    idle_flushed: HashMap<i32, Instant>,
    /// Relations rewritten (new files) since they were last read: a scan reads them in.
    pub cold: std::collections::HashSet<u32>,
    /// User tables whose modification counters (n_mod_since_analyze, n_ins_since_vacuum,
    /// n_dead_tup) moved since autovacuum last looked at the database.
    pub av_changed: std::collections::HashSet<u32>,
    /// The autovacuum launcher (when the database has one), and the time of the changes it
    /// has been told about (a visit is due then).
    pub av: Option<crate::autovacuum::Hook>,
    pub av_noted: Option<Instant>,
}

/// A pg_stat_io cell: (backend type, object, context, operation).
pub type IoKey = (&'static str, &'static str, &'static str, &'static str);

impl Shared {
    fn table(&mut self, oid: u32) -> &mut TabEntry {
        self.tables.entry(oid).or_default()
    }
}

/// A transactional count level of a table (PgStat_TableXactStatus).
#[derive(Clone, Copy, Debug, Default)]
struct Trans {
    nest: usize,
    inserted: i64,
    updated: i64,
    deleted: i64,
    truncdropped: bool,
    inserted_pre: i64,
    updated_pre: i64,
    deleted_pre: i64,
}

impl Trans {
    /// save_truncdrop_counters.
    fn save_truncdrop(&mut self, is_drop: bool) {
        if !self.truncdropped || is_drop {
            self.inserted_pre = self.inserted;
            self.updated_pre = self.updated;
            self.deleted_pre = self.deleted;
            self.truncdropped = true;
        }
    }

    /// restore_truncdrop_counters.
    fn restore_truncdrop(&mut self) {
        if self.truncdropped {
            self.inserted = self.inserted_pre;
            self.updated = self.updated_pre;
            self.deleted = self.deleted_pre;
        }
    }
}

/// A transaction's transactional counts, per table and subtransaction level, and the
/// counts its aborted subtransactions left (applied with the rest at its end).
#[derive(Clone, Debug, Default)]
pub struct XactStats {
    levels: HashMap<u32, Vec<Trans>>,
    done: HashMap<u32, TabCounts>,
    /// The WAL records its row changes logged.
    wal_records: i64,
}

impl XactStats {
    /// The table's count level for subtransaction nesting level `nest` (1: the top).
    fn level(&mut self, rel: u32, nest: usize) -> &mut Trans {
        let v = self.levels.entry(rel).or_default();
        if v.last().is_none_or(|t| t.nest < nest) {
            v.push(Trans {
                nest,
                ..Default::default()
            });
        }
        v.last_mut().unwrap()
    }

    pub fn count_insert(&mut self, rel: u32, nest: usize, n: i64) {
        self.level(rel, nest).inserted += n;
        self.wal_records += n;
    }

    pub fn count_update(&mut self, rel: u32, nest: usize) {
        self.level(rel, nest).updated += 1;
        self.wal_records += 1;
    }

    pub fn count_delete(&mut self, rel: u32, nest: usize) {
        self.level(rel, nest).deleted += 1;
        self.wal_records += 1;
    }

    /// pgstat_count_truncate.
    pub fn count_truncate(&mut self, rel: u32, nest: usize) {
        let t = self.level(rel, nest);
        t.save_truncdrop(false);
        t.inserted = 0;
        t.updated = 0;
        t.deleted = 0;
    }

    /// pgstat_drop_relation: the transaction's counts of a dropped table are forgotten
    /// (back if the drop rolls back).
    pub fn count_drop(&mut self, rel: u32, nest: usize) {
        let t = self.level(rel, nest);
        t.save_truncdrop(true);
        t.inserted = 0;
        t.updated = 0;
        t.deleted = 0;
    }

    /// The live (not yet ended) levels' counts of a table: (inserted, updated, deleted).
    pub fn live(&self, rel: u32) -> (i64, i64, i64) {
        self.levels.get(&rel).map_or((0, 0, 0), |v| {
            v.iter().fold((0, 0, 0), |a, t| {
                (a.0 + t.inserted, a.1 + t.updated, a.2 + t.deleted)
            })
        })
    }

    /// The counts aborted subtransactions left on a table.
    pub fn done(&self, rel: u32) -> Option<&TabCounts> {
        self.done.get(&rel)
    }

    /// The dead tuples aborted subtransactions left on a table.
    pub fn aborted_dead(&self, rel: u32) -> i64 {
        self.done.get(&rel).map_or(0, |c| c.delta_dead_tuples)
    }

    /// AtEOSubXact_PgStat_Relations: subtransaction level `nest` ends.
    pub fn end_subxact(&mut self, nest: usize, commit: bool) {
        for (rel, v) in self.levels.iter_mut() {
            let Some(top) = v.last() else { continue };
            if top.nest != nest {
                continue;
            }
            let mut t = v.pop().unwrap();
            if commit {
                match v.last_mut() {
                    Some(up) if up.nest == nest - 1 => {
                        if t.truncdropped {
                            up.save_truncdrop(false);
                            up.inserted = t.inserted;
                            up.updated = t.updated;
                            up.deleted = t.deleted;
                        } else {
                            up.inserted += t.inserted;
                            up.updated += t.updated;
                            up.deleted += t.deleted;
                        }
                    }
                    _ => {
                        t.nest = nest - 1;
                        v.push(t);
                    }
                }
            } else {
                t.restore_truncdrop();
                let d = self.done.entry(*rel).or_default();
                d.tuples_inserted += t.inserted;
                d.tuples_updated += t.updated;
                d.tuples_deleted += t.deleted;
                d.delta_dead_tuples += t.inserted + t.updated;
            }
        }
        self.levels.retain(|_, v| !v.is_empty());
    }

    /// AtEOXact_PgStat_Relations: the counts the transaction leaves, by table.
    pub fn end_xact(mut self, commit: bool) -> HashMap<u32, TabCounts> {
        // Subtransactions still open end with it.
        let depth = self
            .levels
            .values()
            .filter_map(|v| v.last().map(|t| t.nest))
            .max()
            .unwrap_or(1);
        for nest in (2..=depth).rev() {
            self.end_subxact(nest, commit);
        }
        let mut out = std::mem::take(&mut self.done);
        for (rel, v) in self.levels {
            for mut t in v {
                let c = out.entry(rel).or_default();
                if !commit {
                    t.restore_truncdrop();
                }
                c.tuples_inserted += t.inserted;
                c.tuples_updated += t.updated;
                c.tuples_deleted += t.deleted;
                if commit {
                    c.truncdropped = t.truncdropped;
                    if t.truncdropped {
                        c.delta_live_tuples = 0;
                        c.delta_dead_tuples = 0;
                    }
                    c.delta_live_tuples += t.inserted - t.deleted;
                    c.delta_dead_tuples += t.updated + t.deleted;
                    c.changed_tuples += t.inserted + t.updated + t.deleted;
                } else {
                    c.delta_dead_tuples += t.inserted + t.updated;
                }
            }
        }
        out
    }
}

/// How readers see the shared statistics (stats_fetch_consistency).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Consistency {
    None,
    #[default]
    Cache,
    Snapshot,
}

/// What a reader fetched (the snapshot or cache of stats_fetch_consistency).
#[derive(Clone, Debug, Default)]
struct Fetched {
    tables: HashMap<u32, Option<TabEntry>>,
    funcs: HashMap<u32, Option<FuncCounts>>,
    db: Option<DbEntry>,
    /// The SLRUs' counts, copied when first read.
    slru: Option<HashMap<String, i64>>,
}

/// A backend's pending (not yet flushed) counts, and its view of the shared ones.
#[derive(Debug, Default)]
pub struct Pending {
    /// The backend's own I/O counts (pg_stat_get_backend_io), never flushed away.
    pub io: super::fasthash::HashMap<IoKey, i64>,
    /// The latest I/O counted, still adding up (`count_io`): not yet in `io` or the
    /// shared counts.
    pub io_run: Option<(IoKey, i64)>,
    /// The backend's own WAL (pg_stat_get_backend_wal): records and bytes.
    pub wal: (i64, i64),
    pub tabs: HashMap<u32, TabCounts>,
    pub funcs: HashMap<u32, FuncCounts>,
    /// New sessions to count in the database's entry.
    pub sessions: i64,
    /// Parallel workers the backend's queries planned and launched
    /// (pgstat_update_parallel_workers_stats).
    pub parallel_workers: (i64, i64),
    /// Transactions ended since the last report.
    xact_commit: i64,
    xact_rollback: i64,
    /// WAL records, bytes and transactions writing them since the last report.
    wal_pending: (i64, i64, i64),
    /// SLRU pages zeroed since the last report, by SLRU.
    slru_zeroed: HashMap<String, i64>,
    /// pg_stat_force_next_flush was called.
    force_next: bool,
    /// pgstat_report_stat's last_flush and pending_since.
    last_flush: Option<Instant>,
    pending_since: Option<Instant>,
    /// Counts are kept unflushed in `Shared::unflushed`.
    parked: bool,
    /// The mode of the cache or snapshot held.
    mode: Option<Consistency>,
    /// A snapshot (snapshot mode, with its time) or the entries read (cache mode).
    snapshot: Option<(i64, Box<Shared>)>,
    cache: Fetched,
}

impl Pending {
    /// A new backend's: its session counts once flushed.
    pub fn new_session() -> Pending {
        Pending {
            sessions: 1,
            ..Default::default()
        }
    }

    pub fn tab(&mut self, rel: u32) -> &mut TabCounts {
        self.tabs.entry(rel).or_default()
    }

    /// pgstat_clear_snapshot.
    pub fn clear_snapshot(&mut self) {
        self.mode = None;
        self.snapshot = None;
        self.cache = Fetched::default();
    }

    /// The snapshot's time (snapshot mode), after a change of mode drops what was held.
    pub fn snapshot_timestamp(&mut self, mode: Consistency) -> Option<i64> {
        if self.mode.is_some_and(|m| m != mode) {
            self.clear_snapshot();
        }
        self.snapshot.as_ref().map(|(ts, _)| *ts)
    }
}

/// The shared catalogs, their TOAST tables and indexes (relisshared): their counts are
/// database 0's.
const SHARED_RELS: &[u32] = &[
    1260, 1261, 1262, 1213, 1214, 2396, 2964, 3592, 6000, 6100, 6243, 2671, 2672, 2676, 2677,
    2694, 2695, 2697, 2698, 2965, 2966, 2967, 3593, 4060, 4061, 4175, 4176, 4177, 4178, 4181,
    4182, 4183, 4184, 4185, 4186, 6001, 6002, 6114, 6115, 6244, 6245, 6246, 6247, 1232, 1233,
    2397, 3594, 4060,
];

/// Whether `rel` is a shared catalog (or its TOAST table or index).
pub fn is_shared(rel: u32) -> bool {
    SHARED_RELS.contains(&rel)
}

/// The current time as a timestamptz.
pub fn now() -> i64 {
    crate::types::datetime::Clock::system().now
}

use super::ExecCtx;

fn consistency(ctx: &ExecCtx) -> Consistency {
    match ctx
        .sess
        .settings
        .borrow()
        .get_raw("stats_fetch_consistency")
        .as_deref()
    {
        Some("none") => Consistency::None,
        Some("snapshot") => Consistency::Snapshot,
        _ => Consistency::Cache,
    }
}

/// Whether track_counts is on.
fn counting(ctx: &ExecCtx) -> bool {
    ctx.sess.settings.borrow().track_counts()
}

/// Run `f` on the shared entries as a reader sees them under stats_fetch_consistency
/// (building the snapshot or caching the entries `cache_key` names).
fn read<T>(ctx: &ExecCtx, f: impl Fn(&Shared) -> T, cache: impl FnOnce(&mut Fetched, &Shared)) -> T {
    let mode = consistency(ctx);
    let mut p = ctx.sess.pgstat.borrow_mut();
    if p.mode.is_some_and(|m| m != mode) {
        p.clear_snapshot();
    }
    match mode {
        Consistency::None => f(&shared(&ctx.sess.db)),
        Consistency::Snapshot => {
            if p.snapshot.is_none() {
                let mut s = shared(&ctx.sess.db).clone();
                // (The cluster's fixed statistics too, as they are now.)
                let copy = s.fixed.lock().clone();
                s.fixed = std::sync::Arc::new(parking_lot::Mutex::new(copy));
                p.snapshot = Some((now(), Box::new(s)));
                p.mode = Some(mode);
            }
            f(&p.snapshot.as_ref().unwrap().1)
        }
        Consistency::Cache => {
            p.mode = Some(mode);
            let shared = shared(&ctx.sess.db);
            cache(&mut p.cache, &shared);
            drop(shared);
            let mut view = Shared::default();
            for (k, v) in &p.cache.tables {
                if let Some(e) = v {
                    view.tables.insert(*k, e.clone());
                }
            }
            for (k, v) in &p.cache.funcs {
                if let Some(e) = v {
                    view.funcs.insert(*k, *e);
                }
            }
            if let Some(d) = &p.cache.db {
                view.db = d.clone();
            }
            f(&view)
        }
    }
}

/// A table's (or index's) shared entry as the reader sees it.
pub fn fetch_table(ctx: &ExecCtx, rel: u32) -> Option<TabEntry> {
    read(
        ctx,
        |s| s.tables.get(&rel).cloned(),
        |c, s| {
            c.tables
                .entry(rel)
                .or_insert_with(|| s.tables.get(&rel).cloned());
        },
    )
}

/// A function's shared entry as the reader sees it.
pub fn fetch_function(ctx: &ExecCtx, func: u32) -> Option<FuncCounts> {
    read(
        ctx,
        |s| s.funcs.get(&func).copied(),
        |c, s| {
            c.funcs.entry(func).or_insert_with(|| s.funcs.get(&func).copied());
        },
    )
}

/// The database's entry as the reader sees it (database 0: shared relations').
pub fn fetch_db(ctx: &ExecCtx, db: u32) -> Option<DbEntry> {
    let mine = ctx.sess.db.oid;
    if db != mine && db != 0 {
        // Another database's: as its entry is now (one without an entry counts nothing).
        return Some(ctx.sess.cluster.db_stats(db).unwrap_or_default());
    }
    if db == 0 {
        return Some(shared(&ctx.sess.db).shared_db.clone());
    }
    Some(read(
        ctx,
        |s| s.db.clone(),
        |c, s| {
            if c.db.is_none() {
                c.db = Some(s.db.clone());
            }
        },
    ))
}

/// pg_stat_slru's counts as the reader sees them: the fixed-numbered statistics are part
/// of a snapshot (snapshot mode), or copied once per transaction when first read (cache
/// mode: pgstat_snapshot_fixed), or read as they are (none).
pub fn fetch_slru(ctx: &ExecCtx) -> HashMap<String, i64> {
    let mode = consistency(ctx);
    if mode == Consistency::Cache {
        let mut p = ctx.sess.pgstat.borrow_mut();
        if p.mode.is_some_and(|m| m != mode) {
            p.clear_snapshot();
        }
        p.mode = Some(mode);
        if p.cache.slru.is_none() {
            p.cache.slru = Some(shared(&ctx.sess.db).fixed.lock().slru_zeroed.clone());
        }
        return p.cache.slru.clone().unwrap_or_default();
    }
    read(ctx, |s| s.fixed.lock().slru_zeroed.clone(), |_, _| {})
}

/// The fixed-numbered statistics and other cluster-wide values (no snapshot).
pub fn with_shared<T>(ctx: &ExecCtx, f: impl FnOnce(&mut Fixed) -> T) -> T {
    flush_io(ctx.sess);
    let fixed = shared(&ctx.sess.db).fixed.clone();
    f(&mut fixed.lock())
}

/// Database `db`'s own entry, with its idle backends' counts flushed.
pub fn db_entry(db: &crate::txn::Database) -> DbEntry {
    shared(db).db.clone()
}

/// The cluster's fixed statistics database `db` counts into.
pub fn fixed(db: &crate::txn::Database) -> ClusterStats {
    db.pgstat.lock().fixed.clone()
}

/// The cluster's WAL position now (pg_current_wal_lsn).
pub fn wal_lsn(db: &crate::txn::Database) -> u64 {
    fixed(db).lock().wal_lsn()
}

// ── counting ──

/// The relation gets its shared entry when the backend first counts for it
/// (pgstat_prep_pending_entry).
fn ensure(ctx: &ExecCtx, rel: u32) {
    if ctx.sess.pgstat.borrow().tabs.contains_key(&rel) {
        return;
    }
    ctx.sess.db.pgstat.lock().tables.entry(rel).or_default();
    ctx.sess.pgstat.borrow_mut().tab(rel);
}

/// A sequential scan of `rel` started (pgstat_count_heap_scan).
pub fn count_seqscan(ctx: &ExecCtx, rel: u32) {
    if counting(ctx) {
        ensure(ctx, rel);
        ctx.sess.pgstat.borrow_mut().tab(rel).numscans += 1;
    }
}

/// A query ran with parallel workers: those it meant to launch and those it did.
pub fn count_parallel_workers(ctx: &ExecCtx, to_launch: i64, launched: i64) {
    let mut p = ctx.sess.pgstat.borrow_mut();
    p.parallel_workers.0 += to_launch;
    p.parallel_workers.1 += launched;
}

/// An index scan of index `ix` started (pgstat_count_index_scan).
pub fn count_index_scan(ctx: &ExecCtx, ix: u32) {
    if counting(ctx) {
        ensure(ctx, ix);
        ctx.sess.pgstat.borrow_mut().tab(ix).numscans += 1;
    }
}

/// Whether a scan starting now counts (track_counts): the per-tuple counts below are
/// made only when it does.
pub fn scan_counts(ctx: &ExecCtx) -> bool {
    counting(ctx)
}

/// Tuples a scan of `rel` returned (pgstat_count_heap_getnext /
/// pgstat_count_index_tuples).
pub fn count_returned(ctx: &ExecCtx, rel: u32, n: i64) {
    if n > 0 {
        ctx.sess.pgstat.borrow_mut().tab(rel).tuples_returned += n;
    }
}

/// Heap tuples fetched through `rel` (an index, or a table by a bitmap scan)
/// (pgstat_count_heap_fetch).
pub fn count_fetched(ctx: &ExecCtx, rel: u32, n: i64) {
    if n > 0 {
        ctx.sess.pgstat.borrow_mut().tab(rel).tuples_fetched += n;
    }
}

/// Blocks of `rel` read (all hits here: everything is in memory).
pub fn count_blocks(ctx: &ExecCtx, rel: u32, n: i64) {
    if n > 0 {
        let mut p = ctx.sess.pgstat.borrow_mut();
        let t = p.tab(rel);
        t.blocks_fetched += n;
        t.blocks_hit += n;
    }
}

fn nest(ctx: &ExecCtx) -> usize {
    ctx.txn.borrow().savepoint_depth() + 1
}

pub fn count_insert(ctx: &ExecCtx, rel: u32, n: i64) {
    if counting(ctx) {
        ensure(ctx, rel);
        let nest = nest(ctx);
        ctx.txn.borrow_mut().pgstat.count_insert(rel, nest, n);
    }
}

/// An updated row (pgstat_count_heap_update): HOT and new-page updates aren't
/// transactional counts.
pub fn count_update(ctx: &ExecCtx, rel: u32, hot: bool, newpage: bool) {
    if counting(ctx) {
        ensure(ctx, rel);
        let nest = nest(ctx);
        ctx.txn.borrow_mut().pgstat.count_update(rel, nest);
        let mut p = ctx.sess.pgstat.borrow_mut();
        if hot {
            p.tab(rel).tuples_hot_updated += 1;
        } else if newpage {
            p.tab(rel).tuples_newpage_updated += 1;
        }
    }
}

pub fn count_delete(ctx: &ExecCtx, rel: u32) {
    if counting(ctx) {
        ensure(ctx, rel);
        let nest = nest(ctx);
        ctx.txn.borrow_mut().pgstat.count_delete(rel, nest);
    }
}

pub fn count_drop(ctx: &ExecCtx, rel: u32) {
    if counting(ctx) {
        let nest = nest(ctx);
        ctx.txn.borrow_mut().pgstat.count_drop(rel, nest);
    }
}

pub fn count_truncate(ctx: &ExecCtx, rel: u32) {
    if counting(ctx) {
        let nest = nest(ctx);
        ctx.txn.borrow_mut().pgstat.count_truncate(rel, nest);
    }
}

/// A function call ended (pgstat_end_function_usage), under track_functions: PL
/// functions unless it's `none`, SQL and C ones only with `all`.
pub fn count_function(ctx: &ExecCtx, func: u32, language: &str, ms: f64) {
    let track = ctx
        .sess
        .settings
        .borrow()
        .get_raw("track_functions")
        .unwrap_or_else(|| "none".into());
    let tracked = match track.as_str() {
        "all" => true,
        "pl" => !matches!(language, "sql" | "c" | "internal"),
        _ => false,
    };
    if !tracked {
        return;
    }
    ctx.sess.db.pgstat.lock().funcs.entry(func).or_default();
    let mut p = ctx.sess.pgstat.borrow_mut();
    let f = p.funcs.entry(func).or_default();
    f.calls += 1;
    f.total_time += ms;
    f.self_time += ms;
}

/// I/O of the client backend (shared buffers are all hits; what the counts compare is
/// that they grow). Counts of one key in a row (a row-at-a-time insert's extends) add up
/// in the backend (`Pending::io_run`) and go into the shared and backend counts together:
/// when another key is counted, before anything reads or resets them (`flush_io`), and at
/// the transaction's end.
pub fn count_io(ctx: &ExecCtx, object: &'static str, context: &'static str, op: &'static str, n: i64) {
    if n <= 0 {
        return;
    }
    let key: IoKey = ("client backend", object, context, op);
    let prev = {
        let mut p = ctx.sess.pgstat.borrow_mut();
        match &mut p.io_run {
            Some((k, c)) if *k == key => {
                *c += n;
                return;
            }
            run => run.replace((key, n)),
        }
    };
    if let Some((k, c)) = prev {
        add_io(ctx.sess, k, c);
    }
}

fn add_io(sess: &crate::session::state::SessionState, key: IoKey, n: i64) {
    *sess.db.pgstat.lock().fixed.lock().io.entry(key).or_default() += n;
    *sess.pgstat.borrow_mut().io.entry(key).or_default() += n;
}

/// The backend's I/O counts still adding up (`count_io`), counted now.
pub fn flush_io(sess: &crate::session::state::SessionState) {
    let run = sess.pgstat.borrow_mut().io_run.take();
    if let Some((k, c)) = run {
        add_io(sess, k, c);
    }
}

/// The I/O a heap insert of `n` rows into `rel` does: it extends the relation (a
/// temporary one's local buffers, past temp_buffers, evicted and written out).
pub fn io_insert(ctx: &ExecCtx, t: &crate::catalog::Table, n: i64, context: &'static str) {
    let rel = t.oid;
    if !t.temporary {
        count_io(ctx, "relation", context, "extends", n);
        return;
    }
    count_io(ctx, "temp relation", "normal", "extends", n);
    let pages = super::sizing::modelled_blocks(rel, ctx).unwrap_or(0) as i64;
    let buffers = temp_buffers(ctx);
    if pages > buffers {
        count_io(ctx, "temp relation", "normal", "evictions", n);
        count_io(ctx, "temp relation", "normal", "writes", n);
    }
}

/// temp_buffers, in pages.
fn temp_buffers(ctx: &ExecCtx) -> i64 {
    let raw = ctx.sess.settings.borrow().get_raw("temp_buffers").unwrap_or_default();
    let n = raw.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let n: i64 = n.trim().parse().unwrap_or(1024);
    match raw.trim_start_matches(|c: char| c.is_ascii_digit()).trim() {
        "kB" => n / 8,
        "MB" => n * 128,
        "GB" => n * 131072,
        _ => n,
    }
}

/// The I/O of a sequential scan of `pages` pages of `rel`: hits, or reads of a relation
/// rewritten since it was last read (and of a temporary one's pages past temp_buffers).
pub fn io_scan(ctx: &ExecCtx, rel: u32, pages: i64) {
    let temp = ctx.catalog().tables.get(&rel).is_some_and(|t| t.temporary);
    let cold = ctx.sess.db.pgstat.lock().cold.remove(&rel);
    let object = if temp { "temp relation" } else { "relation" };
    if temp {
        let over = pages - temp_buffers(ctx);
        count_io(ctx, object, "normal", "reads", over.max(i64::from(cold)));
        count_io(ctx, object, "normal", "hits", pages - over.max(0));
    } else if cold {
        count_io(ctx, object, "normal", "reads", pages.max(1));
    } else {
        count_io(ctx, object, "normal", "hits", pages.max(1));
    }
}

/// A relation got new files (a rewrite): written out, and read in again by its next scan.
pub fn io_rewrite(ctx: &ExecCtx, rel: u32) {
    let temp = ctx.catalog().tables.get(&rel).is_some_and(|t| t.temporary);
    ctx.sess.db.pgstat.lock().cold.insert(rel);
    if temp {
        count_io(ctx, "temp relation", "normal", "writes", 1);
    } else {
        count_io(ctx, "relation", "normal", "writes", 1);
    }
}

/// pg_stat_reset_backend_stats of this backend.
pub fn reset_backend(ctx: &ExecCtx) {
    flush_io(ctx.sess);
    let mut p = ctx.sess.pgstat.borrow_mut();
    p.io.clear();
    p.wal = (0, 0);
}

// ── reports into the shared entries ──

/// pgstat_report_vacuum.
pub fn report_vacuum(ctx: &ExecCtx, rel: u32, live: i64, dead: i64, ms: f64) {
    if !counting(ctx) {
        return;
    }
    let mut s = ctx.sess.db.pgstat.lock();
    let t = s.table(rel);
    t.live_tuples = live;
    t.dead_tuples = dead;
    t.ins_since_vacuum = 0;
    if ctx.sess.autovacuum.get() {
        t.last_autovacuum_time = Some(now());
        t.autovacuum_count += 1;
        t.total_autovacuum_time += ms;
    } else {
        t.last_vacuum_time = Some(now());
        t.vacuum_count += 1;
        t.total_vacuum_time += ms;
    }
}

/// pgstat_report_analyze: the counts this transaction will report at its end are taken
/// off (a partitioned table's aren't counted).
pub fn report_analyze(
    ctx: &ExecCtx,
    rel: u32,
    mut live: i64,
    mut dead: i64,
    reset_counter: bool,
    partitioned: bool,
    ms: f64,
) {
    if !counting(ctx) {
        return;
    }
    if !partitioned {
        let txn = ctx.txn.borrow();
        let (ins, upd, del) = txn.pgstat.live(rel);
        live -= ins - del;
        dead -= upd + del;
        dead -= txn.pgstat.aborted_dead(rel);
        live = live.max(0);
        dead = dead.max(0);
    }
    let mut s = ctx.sess.db.pgstat.lock();
    let t = s.table(rel);
    t.live_tuples = live;
    t.dead_tuples = dead;
    if reset_counter {
        t.mod_since_analyze = 0;
    }
    if ctx.sess.autovacuum.get() {
        t.last_autoanalyze_time = Some(now());
        t.autoanalyze_count += 1;
        t.total_autoanalyze_time += ms;
    } else {
        t.last_analyze_time = Some(now());
        t.analyze_count += 1;
        t.total_analyze_time += ms;
    }
}

/// A checkpoint was requested and done (CHECKPOINT): it writes and syncs buffers and logs
/// a checkpoint record. `log_checkpoints` (the session's value): LogCheckpointStart /
/// LogCheckpointEnd's lines, from the checkpointer.
pub fn report_checkpoint(
    db: &crate::txn::Database,
    log_checkpoints: bool,
    flush: impl FnOnce() -> crate::error::Result<()>,
) -> crate::error::Result<()> {
    let started = Instant::now();
    let logging = log_checkpoints && crate::log::active();
    if logging {
        checkpointer_log("checkpoint starting: immediate force wait".into());
    }
    flush()?;
    let fixed = db.pgstat.lock().fixed.clone();
    let mut s = fixed.lock();
    s.checkpoints_requested += 1;
    let prev_redo = s.checkpoint.map(|(_, redo, _)| redo);
    let (redo, ckpt) = s.checkpoint_at(now());
    s.checkpoints_performed += 1;
    for op in ["writes", "fsyncs"] {
        *s.io
            .entry(("checkpointer", "relation", "normal", op))
            .or_default() += 1;
    }
    *s.io
        .entry(("checkpointer", "wal", "normal", "writes"))
        .or_default() += 1;
    // UpdateCheckPointDistanceEstimate.
    let distance = prev_redo.map_or(0, |p| redo.saturating_sub(p));
    s.distance_estimate = if s.distance_estimate < distance as f64 {
        distance as f64
    } else {
        0.9 * s.distance_estimate + 0.1 * distance as f64
    };
    let estimate = s.distance_estimate;
    drop(s);
    if logging {
        // (Buffers: the pages the WAL since the last checkpoint stands for, out of
        // shared_buffers' 16384; the engine has no shared buffers of its own to write.)
        let bufs = (distance / 8192).min(16384);
        let ms = started.elapsed().as_millis() as u64;
        let secs = |ms: u64| format!("{}.{:03}", ms / 1000, ms % 1000);
        let lsn = |p: u64| format!("{:X}/{:X}", p >> 32, p & 0xffff_ffff);
        let files = u64::from(bufs > 0);
        checkpointer_log(format!(
            "checkpoint complete: wrote {bufs} buffers ({:.1}%), wrote 0 SLRU buffers; 0 WAL file(s) added, \
             0 removed, 0 recycled; write={} s, sync=0.000 s, total={} s; sync files={files}, \
             longest=0.000 s, average=0.000 s; distance={} kB, estimate={} kB; lsn={}, redo lsn={}",
            bufs as f64 * 100.0 / 16384.0,
            secs(ms),
            secs(ms),
            distance / 1024,
            (estimate / 1024.0) as u64,
            lsn(ckpt),
            lsn(redo),
        ));
    }
    Ok(())
}

/// A LOG line of the checkpointer (its pid in pg_stat_activity).
fn checkpointer_log(text: String) {
    static LINES: std::sync::OnceLock<std::sync::Arc<crate::log::Lines>> = std::sync::OnceLock::new();
    let f = crate::log::Format::from_files(None);
    crate::log::write(&f, &crate::log::Who::aux(30, "checkpointer", &LINES), &crate::log::Message::log(&text));
}


/// pg_stat_reset: the database's counters and every table's and function's entry
/// (pgstat_reset_counters: the entries stay, zeroed).
pub fn reset_db(ctx: &ExecCtx) {
    let mut s = ctx.sess.db.pgstat.lock();
    for (rel, e) in s.tables.iter_mut() {
        if !SHARED_RELS.contains(rel) {
            *e = TabEntry::default();
        }
    }
    for e in s.funcs.values_mut() {
        *e = FuncCounts::default();
    }
    s.db = DbEntry {
        stat_reset: Some(now()),
        ..Default::default()
    };
}

/// pg_stat_reset_single_table_counters: a shared relation's resets database 0's time.
/// (pgstat_reset: an existing entry stays, zeroed.)
pub fn reset_table(ctx: &ExecCtx, rel: u32, shared: bool) {
    let mut s = ctx.sess.db.pgstat.lock();
    if let Some(e) = s.tables.get_mut(&rel) {
        *e = TabEntry::default();
    }
    let t = now();
    if shared {
        s.shared_db.stat_reset = Some(t);
    } else {
        s.db.stat_reset = Some(t);
    }
}

/// pg_stat_reset_single_function_counters: an existing entry stays, zeroed (its calls
/// read 0, not NULL).
pub fn reset_function(ctx: &ExecCtx, func: u32) {
    let mut s = ctx.sess.db.pgstat.lock();
    if let Some(e) = s.funcs.get_mut(&func) {
        *e = FuncCounts::default();
    }
    s.db.stat_reset = Some(now());
}

/// pg_stat_reset_shared / pg_stat_reset_slru: `target`'s reset time is now.
pub fn reset_shared(ctx: &ExecCtx, target: &str) {
    flush_io(ctx.sess);
    let fixed = fixed(&ctx.sess.db);
    let mut s = fixed.lock();
    let t = now();
    if target == "io" {
        s.io.clear();
    }
    if let Some(name) = target.strip_prefix("slru:") {
        s.slru_zeroed.remove(name);
    }
    if target == "wal" {
        s.wal_bytes = 0;
        s.wal_records = 0;
    }
    if target == "checkpointer" {
        s.checkpoints_requested = 0;
        s.checkpoints_performed = 0;
    }
    s.resets.insert(target.to_string(), t);
}

/// When the fixed-numbered statistics were last reset: at first, when the statistics
/// started (the server's start).
pub fn reset_time(ctx: &ExecCtx, target: &str) -> Option<i64> {
    let start = ctx.sess.cluster.start_time();
    Some(*fixed(&ctx.sess.db).lock().resets.entry(target.to_string()).or_insert(start))
}

// ── the end of a transaction ──

/// pgstat_report_stat's intervals: a backend going idle flushes its pending counts only if
/// its last flush was at least PGSTAT_MIN_INTERVAL ago (unless forced); counts it keeps
/// go after PGSTAT_IDLE_INTERVAL of idleness, or at the first idle point once they have
/// waited PGSTAT_MAX_INTERVAL.
const MIN_INTERVAL: Duration = Duration::from_millis(1000);
const IDLE_INTERVAL: Duration = Duration::from_millis(10000);
const MAX_INTERVAL: Duration = Duration::from_millis(60000);

/// A backend's counts that went idle unflushed (in `Shared::unflushed`, by its process
/// id): flushed by the backend at its next idle point that may flush, or by any reader of
/// the statistics once the backend has been idle for PGSTAT_IDLE_INTERVAL (the backend's
/// idle-stats-update timeout).
#[derive(Clone, Debug, Default)]
pub struct Unflushed {
    pub tabs: HashMap<u32, TabCounts>,
    pub funcs: HashMap<u32, FuncCounts>,
    sessions: i64,
    parallel_workers: (i64, i64),
    xact_commit: i64,
    xact_rollback: i64,
    /// WAL records and bytes, and the transactions that wrote them (one WAL write and
    /// fsync each).
    wal: (i64, i64, i64),
    slru_zeroed: HashMap<String, i64>,
    /// When the idle timeout fires (None: the backend is busy).
    due: Option<Instant>,
}

/// Add pending table counts `c` to `t` (a later transaction's after an earlier's: a
/// truncation in the later one starts the live / dead deltas over).
pub fn add_pending(t: &mut TabCounts, c: &TabCounts) {
    t.numscans += c.numscans;
    t.tuples_returned += c.tuples_returned;
    t.tuples_fetched += c.tuples_fetched;
    t.tuples_inserted += c.tuples_inserted;
    t.tuples_updated += c.tuples_updated;
    t.tuples_deleted += c.tuples_deleted;
    t.tuples_hot_updated += c.tuples_hot_updated;
    t.tuples_newpage_updated += c.tuples_newpage_updated;
    if c.truncdropped {
        t.truncdropped = true;
        t.delta_live_tuples = 0;
        t.delta_dead_tuples = 0;
    }
    t.delta_live_tuples += c.delta_live_tuples;
    t.delta_dead_tuples += c.delta_dead_tuples;
    t.changed_tuples += c.changed_tuples;
    t.blocks_fetched += c.blocks_fetched;
    t.blocks_hit += c.blocks_hit;
}

/// Flush a backend's unflushed counts into the shared entries (pgstat_flush_pending_entries
/// and the fixed-numbered statistics' flushes); entries of objects no longer in `cat` stay
/// gone.
fn flush(s: &mut Shared, u: Unflushed, cat: &crate::catalog::Catalog) {
    let ts = now();
    for (rel, c) in u.tabs {
        if rel >= crate::catalog::FIRST_USER_OID && !in_catalog(cat, rel) {
            continue;
        }
        let shared = SHARED_RELS.contains(&rel);
        let e = s.tables.entry(rel).or_default();
        e.numscans += c.numscans;
        if c.numscans > 0 && e.lastscan.is_none_or(|l| ts > l) {
            e.lastscan = Some(ts);
        }
        e.tuples_returned += c.tuples_returned;
        e.tuples_fetched += c.tuples_fetched;
        e.tuples_inserted += c.tuples_inserted;
        e.tuples_updated += c.tuples_updated;
        e.tuples_deleted += c.tuples_deleted;
        e.tuples_hot_updated += c.tuples_hot_updated;
        e.tuples_newpage_updated += c.tuples_newpage_updated;
        if c.truncdropped {
            e.live_tuples = 0;
            e.dead_tuples = 0;
            e.ins_since_vacuum = 0;
        }
        e.live_tuples += c.delta_live_tuples;
        e.dead_tuples += c.delta_dead_tuples;
        e.mod_since_analyze += c.changed_tuples;
        e.ins_since_vacuum += c.tuples_inserted;
        e.blocks_fetched += c.blocks_fetched;
        e.blocks_hit += c.blocks_hit;
        e.live_tuples = e.live_tuples.max(0);
        e.dead_tuples = e.dead_tuples.max(0);
        if rel >= crate::catalog::FIRST_USER_OID && modifies(&c) {
            s.av_changed.insert(rel);
        }
        let db = if shared { &mut s.shared_db } else { &mut s.db };
        db.tuples_returned += c.tuples_returned;
        db.tuples_fetched += c.tuples_fetched;
        db.tuples_inserted += c.tuples_inserted;
        db.tuples_updated += c.tuples_updated;
        db.tuples_deleted += c.tuples_deleted;
        db.blocks_fetched += c.blocks_fetched;
        db.blocks_hit += c.blocks_hit;
    }
    for (f, c) in u.funcs {
        if f >= crate::catalog::FIRST_USER_OID && !cat.functions.contains_key(&f) {
            continue;
        }
        let e = s.funcs.entry(f).or_default();
        e.calls += c.calls;
        e.total_time += c.total_time;
        e.self_time += c.self_time;
    }
    s.db.xact_commit += u.xact_commit;
    s.db.xact_rollback += u.xact_rollback;
    let (records, bytes, xacts) = u.wal;
    let mut f = s.fixed.lock();
    if xacts > 0 {
        f.add_wal(records, bytes);
        for op in ["writes", "fsyncs"] {
            *f.io
                .entry(("client backend", "wal", "normal", op))
                .or_default() += xacts;
        }
    }
    for (name, n) in u.slru_zeroed {
        *f.slru_zeroed.entry(name).or_default() += n;
    }
    drop(f);
    s.db.sessions += u.sessions;
    s.db.parallel_workers_to_launch += u.parallel_workers.0;
    s.db.parallel_workers_launched += u.parallel_workers.1;
    if !s.av_changed.is_empty() {
        note_autovacuum(s, Instant::now());
    }
}

/// Whether counts `c` move what autovacuum's thresholds read.
fn modifies(c: &TabCounts) -> bool {
    c.changed_tuples != 0 || c.tuples_inserted != 0 || c.delta_dead_tuples != 0 || c.truncdropped
}

/// Tell the database's autovacuum launcher (once until it visits, unless the changes come
/// sooner than it was told) that tables changed, or will have once counts kept unflushed
/// until `at` are flushed. (Counts kept for an idle timeout, then flushed by the backend's
/// next statement, are due now: told only once, the launcher waited for the timeout.)
fn note_autovacuum(s: &mut Shared, at: Instant) {
    if s.av_noted.is_none_or(|n| at < n)
        && let Some(h) = &s.av
        && h.note(at)
    {
        s.av_noted = Some(at);
    }
}

/// An autovacuum worker's look at the database (do_autovacuum's pgstat fetch): idle
/// backends' due counts flushed, the tables changed since the last look taken, and the
/// launcher told again about counts still kept unflushed (their idle timeout comes later).
pub fn take_autovacuum_changes(db: &crate::txn::Database) -> (Vec<u32>, HashMap<u32, TabEntry>) {
    let mut s = shared(db);
    s.av_noted = None;
    let changed: Vec<u32> = std::mem::take(&mut s.av_changed).into_iter().collect();
    let entries = changed.iter().filter_map(|r| s.tables.get(r).map(|e| (*r, e.clone()))).collect();
    let later = s
        .unflushed
        .values()
        .filter(|u| u.tabs.iter().any(|(r, c)| *r >= crate::catalog::FIRST_USER_OID && modifies(c)))
        .filter_map(|u| u.due)
        .min();
    if let Some(at) = later {
        note_autovacuum(&mut s, at);
    }
    (changed, entries)
}

/// A table autovacuum wanted to process but didn't (its lock was taken, its work was
/// canceled): looked at again on the next visit.
pub fn autovacuum_retry(db: &crate::txn::Database, rel: u32) {
    let mut s = db.pgstat.lock();
    s.av_changed.insert(rel);
    note_autovacuum(&mut s, Instant::now());
}

/// Whether relation `rel` (a table, index, view or TOAST table) is in the catalog.
fn in_catalog(cat: &crate::catalog::Catalog, rel: u32) -> bool {
    cat.tables.contains_key(&rel)
        || cat.indexes.contains_key(&rel)
        || cat.views.contains_key(&rel)
        || cat.toast_owner(rel).is_some()
}

/// The entries of the relations and functions `cur` dropped since `old` (the catalog the
/// entries were last pruned for) gone: those its maps' changes name.
fn prune_since(s: &mut Shared, old: &crate::catalog::Catalog, cur: &crate::catalog::Catalog) {
    let mut gone: Vec<u32> = Vec::new();
    for (oid, x, _) in old.tables.changes(&cur.tables) {
        gone.push(oid);
        if let Some(t) = x.filter(|t| t.toast != 0) {
            gone.extend([t.toast, t.toast + 1]);
        }
    }
    for (oid, x, _) in old.views.changes(&cur.views) {
        gone.push(oid);
        if let Some(t) = x.and_then(|v| v.storage.as_ref()).filter(|t| t.toast != 0) {
            gone.extend([t.toast, t.toast + 1]);
        }
    }
    gone.extend(old.indexes.changes(&cur.indexes).into_iter().map(|c| c.0));
    for rel in gone {
        if !in_catalog(cur, rel) {
            s.tables.remove(&rel);
        }
    }
    for (f, _, now) in old.functions.changes(&cur.functions) {
        if now.is_none() {
            s.funcs.remove(&f);
        }
    }
}

/// Flush the counts of backends idle for PGSTAT_IDLE_INTERVAL (their idle timeouts have
/// fired): what readers of the shared statistics do first.
fn flush_idle(s: &mut Shared, db: &crate::txn::Database) {
    if s.unflushed.is_empty() {
        return;
    }
    let now = Instant::now();
    let due: Vec<i32> = s
        .unflushed
        .iter()
        .filter(|(_, u)| u.due.is_some_and(|d| d <= now))
        .map(|(pid, _)| *pid)
        .collect();
    if due.is_empty() {
        return;
    }
    let cat = db.catalog();
    for pid in due {
        if let Some(u) = s.unflushed.remove(&pid) {
            flush(s, u, &cat);
            s.idle_flushed.insert(pid, now);
        }
    }
}

/// The shared statistics, locked, with the idle backends' counts flushed into them.
fn shared(db: &crate::txn::Database) -> parking_lot::MutexGuard<'_, Shared> {
    let mut s = db.pgstat.lock();
    flush_idle(&mut s, db);
    s
}

/// The backend's counts kept unflushed for table `rel` (pg_stat_get_xact_*'s pending
/// entry holds them too).
pub fn unflushed_table(sess: &crate::session::state::SessionState, rel: u32) -> Option<TabCounts> {
    if !sess.pgstat.borrow().parked {
        return None;
    }
    sess.db.pgstat.lock().unflushed.get(&sess.pid).and_then(|u| u.tabs.get(&rel).copied())
}

/// The backend's counts kept unflushed for function `func`.
pub fn unflushed_function(sess: &crate::session::state::SessionState, func: u32) -> Option<FuncCounts> {
    if !sess.pgstat.borrow().parked {
        return None;
    }
    sess.db.pgstat.lock().unflushed.get(&sess.pid).and_then(|u| u.funcs.get(&func).copied())
}

/// pg_stat_force_next_flush: the backend's next report flushes whatever the interval.
pub fn force_next_flush(sess: &crate::session::state::SessionState) {
    sess.pgstat.borrow_mut().force_next = true;
}

/// SLRU `name` zeroed `n` new pages (counted by the backend, flushed with the rest).
pub fn count_slru_zeroed(sess: &crate::session::state::SessionState, name: &str, n: i64) {
    if n > 0 {
        *sess.pgstat.borrow_mut().slru_zeroed.entry(name.to_string()).or_default() += n;
    }
}

/// A statement starts: a backend that kept counts unflushed isn't idle any more (its
/// idle-stats-update timeout is disabled until it is idle again).
pub fn busy(sess: &crate::session::state::SessionState) {
    if !sess.pgstat.borrow().parked {
        return;
    }
    if let Some(u) = sess.db.pgstat.lock().unflushed.get_mut(&sess.pid) {
        u.due = None;
    }
}

/// The backend ends: what it kept goes now (pgstat_report_stat(true) at exit).
pub fn session_ends(sess: &crate::session::state::SessionState) {
    flush_io(sess);
    report(sess, true, &sess.db.catalog());
}

/// AtEOXact_PgStat and the report that follows when the backend goes idle: the
/// transaction's counts join the backend's pending counts, which pgstat_report_stat
/// flushes into the shared entries unless the last flush was too recent (then they wait
/// for the next idle point, or the idle timeout). The entries of tables and functions no
/// longer in the catalog go at once (AtEOXact_PgStat_DroppedStats).
pub fn end_xact(
    sess: &crate::session::state::SessionState,
    xact: XactStats,
    commit: bool,
    cat: &crate::catalog::Catalog,
) {
    end_xact_counts(sess, xact, commit, cat, true)
}

/// A prepared transaction finished by COMMIT / ROLLBACK PREPARED in this backend
/// (pgstat_twophase_postcommit / postabort): its counts join this backend's pending ones,
/// without a transaction count of its own (the statement's transaction has one).
pub fn end_prepared(
    sess: &crate::session::state::SessionState,
    xact: XactStats,
    commit: bool,
    cat: &crate::catalog::Catalog,
) {
    end_xact_counts(sess, xact, commit, cat, false)
}

fn end_xact_counts(
    sess: &crate::session::state::SessionState,
    xact: XactStats,
    commit: bool,
    cat: &crate::catalog::Catalog,
    count_xact: bool,
) {
    flush_io(sess);
    let wal = xact.wal_records;
    let counts = xact.end_xact(commit);
    {
        let mut p = sess.pgstat.borrow_mut();
        for (rel, c) in counts {
            add_pending(p.tab(rel), &c);
        }
        match (count_xact, commit) {
            (false, _) => {}
            (true, true) => p.xact_commit += 1,
            (true, false) => p.xact_rollback += 1,
        }
        // (A heap insert's record, about 60 bytes, and the commit record.)
        if wal > 0 {
            p.wal.0 += wal + 1;
            p.wal.1 += wal * 59 + 34;
            p.wal_pending.0 += wal + 1;
            p.wal_pending.1 += wal * 59 + 34;
            p.wal_pending.2 += 1;
            // (Its records are in the WAL now; the statistics count them when flushed.)
            fixed(&sess.db).lock().advance(wal * 59 + 34);
        }
        let mut s = sess.db.pgstat.lock();
        // Dropped objects' entries go, once per catalog version, by a transaction that sees
        // the database's current catalog. (Every transaction's end visited every table's
        // entry under the lock all sessions share; and one over an older catalog removed
        // the entries of tables made since.) Since the version pruned last, only what the
        // catalog's maps changed is looked at: each DDL commit of `pg_restore -j` visited
        // every table's entry.
        let cur = sess.db.catalog();
        if s.pruned_at != cat.version && cur.version == cat.version {
            match s.pruned.take() {
                Some(old) => prune_since(&mut s, &old, &cur),
                None => {
                    s.tables.retain(|rel, _| *rel < crate::catalog::FIRST_USER_OID || in_catalog(cat, *rel));
                    s.funcs
                        .retain(|f, _| *f < crate::catalog::FIRST_USER_OID || cat.functions.contains_key(f));
                }
            }
            s.pruned_at = cat.version;
            s.pruned = Some(cur);
        }
        drop(s);
        p.clear_snapshot();
    }
    report(sess, false, cat);
}

/// pgstat_report_stat: move the backend's pending counts to its unflushed ones, and flush
/// those when `force`d (pg_stat_force_next_flush, the backend's end), when they have
/// waited PGSTAT_MAX_INTERVAL, or when the last flush was PGSTAT_MIN_INTERVAL ago or more.
fn report(sess: &crate::session::state::SessionState, mut force: bool, cat: &crate::catalog::Catalog) {
    let mut p = sess.pgstat.borrow_mut();
    // (The forced flush is absorbed even with nothing to flush.)
    if std::mem::take(&mut p.force_next) {
        force = true;
    }
    if !p.parked
        && p.tabs.is_empty()
        && p.funcs.is_empty()
        && p.slru_zeroed.is_empty()
        && p.wal_pending.2 == 0
        && p.sessions == 0
        && p.parallel_workers == (0, 0)
    {
        return;
    }
    let mut s = sess.db.pgstat.lock();
    // (An idle timeout flushed what was kept: that was the last flush.)
    if let Some(t) = s.idle_flushed.remove(&sess.pid) {
        p.last_flush = Some(t);
        p.pending_since = None;
    }
    let mut u = s.unflushed.remove(&sess.pid).unwrap_or_default();
    for (rel, c) in p.tabs.drain() {
        add_pending(u.tabs.entry(rel).or_default(), &c);
    }
    for (f, c) in p.funcs.drain() {
        let e = u.funcs.entry(f).or_default();
        e.calls += c.calls;
        e.total_time += c.total_time;
        e.self_time += c.self_time;
    }
    for (name, n) in p.slru_zeroed.drain() {
        *u.slru_zeroed.entry(name).or_default() += n;
    }
    u.sessions += std::mem::take(&mut p.sessions);
    let (a, b) = std::mem::take(&mut p.parallel_workers);
    u.parallel_workers.0 += a;
    u.parallel_workers.1 += b;
    u.xact_commit += std::mem::take(&mut p.xact_commit);
    u.xact_rollback += std::mem::take(&mut p.xact_rollback);
    let (r, by, x) = std::mem::take(&mut p.wal_pending);
    u.wal.0 += r;
    u.wal.1 += by;
    u.wal.2 += x;
    let now = Instant::now();
    // Unless forced, or kept PGSTAT_MAX_INTERVAL already: not again within
    // PGSTAT_MIN_INTERVAL of the last flush.
    let overdue = p.pending_since.is_some_and(|t| now.duration_since(t) >= MAX_INTERVAL);
    if !force && !overdue && p.last_flush.is_some_and(|t| now.duration_since(t) < MIN_INTERVAL) {
        // (The idle timeout runs from the first count kept, as PostgreSQL's
        // PGSTAT_IDLE_INTERVAL - (now - pending_since).)
        let due = *p.pending_since.get_or_insert(now) + IDLE_INTERVAL;
        u.due = Some(due);
        if u.tabs.iter().any(|(r, c)| *r >= crate::catalog::FIRST_USER_OID && modifies(c)) {
            note_autovacuum(&mut s, due);
        }
        s.unflushed.insert(sess.pid, u);
        p.parked = true;
        return;
    }
    flush(&mut s, u, cat);
    p.parked = false;
    p.last_flush = Some(now);
    p.pending_since = None;
}
