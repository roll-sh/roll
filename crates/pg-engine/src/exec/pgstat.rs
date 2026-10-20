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

/// The shared statistics of a database (and, kept with it, the cluster's fixed ones).
#[derive(Clone, Debug, Default)]
pub struct Shared {
    pub tables: HashMap<u32, TabEntry>,
    pub funcs: HashMap<u32, FuncCounts>,
    pub db: DbEntry,
    /// Database 0: shared relations' counts.
    pub shared_db: DbEntry,
    /// Reset times of the fixed-numbered statistics: archiver, bgwriter, checkpointer,
    /// io, recovery_prefetch, wal, and `slru:<name>`.
    pub resets: HashMap<String, i64>,
    pub checkpoints_requested: i64,
    pub checkpoints_performed: i64,
    pub wal_records: i64,
    pub wal_bytes: i64,
    /// pg_stat_io counts by (backend type, object, context, operation).
    pub io: HashMap<IoKey, i64>,
    /// Relations rewritten (new files) since they were last read: a scan reads them in.
    pub cold: std::collections::HashSet<u32>,
}

/// A pg_stat_io cell: (backend type, object, context, operation).
pub type IoKey = (String, String, String, &'static str);

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
}

/// A backend's pending (not yet flushed) counts, and its view of the shared ones.
#[derive(Debug, Default)]
pub struct Pending {
    /// The backend's own I/O counts (pg_stat_get_backend_io), never flushed away.
    pub io: HashMap<IoKey, i64>,
    /// The backend's own WAL (pg_stat_get_backend_wal): records and bytes.
    pub wal: (i64, i64),
    pub tabs: HashMap<u32, TabCounts>,
    pub funcs: HashMap<u32, FuncCounts>,
    /// New sessions to count in the database's entry.
    pub sessions: i64,
    /// Parallel workers the backend's queries planned and launched
    /// (pgstat_update_parallel_workers_stats).
    pub parallel_workers: (i64, i64),
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
    ctx.sess
        .settings
        .borrow()
        .get_raw("track_counts")
        .is_none_or(|v| v != "off")
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
        Consistency::None => f(&ctx.sess.db.pgstat.lock()),
        Consistency::Snapshot => {
            if p.snapshot.is_none() {
                let s = ctx.sess.db.pgstat.lock().clone();
                p.snapshot = Some((now(), Box::new(s)));
                p.mode = Some(mode);
            }
            f(&p.snapshot.as_ref().unwrap().1)
        }
        Consistency::Cache => {
            p.mode = Some(mode);
            let shared = ctx.sess.db.pgstat.lock();
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
        return None;
    }
    if db == 0 {
        return Some(ctx.sess.db.pgstat.lock().shared_db.clone());
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

/// The fixed-numbered statistics and other cluster-wide values (no snapshot).
pub fn with_shared<T>(ctx: &ExecCtx, f: impl FnOnce(&mut Shared) -> T) -> T {
    f(&mut ctx.sess.db.pgstat.lock())
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
/// that they grow).
pub fn count_io(ctx: &ExecCtx, object: &str, context: &str, op: &'static str, n: i64) {
    if n <= 0 {
        return;
    }
    let key: IoKey = ("client backend".into(), object.into(), context.into(), op);
    *ctx.sess.db.pgstat.lock().io.entry(key.clone()).or_default() += n;
    *ctx.sess.pgstat.borrow_mut().io.entry(key).or_default() += n;
}

/// The I/O a heap insert of `n` rows into `rel` does: it extends the relation (a
/// temporary one's local buffers, past temp_buffers, evicted and written out).
pub fn io_insert(ctx: &ExecCtx, rel: u32, n: i64, context: &str) {
    let temp = ctx.catalog().tables.get(&rel).is_some_and(|t| t.temporary);
    if !temp {
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
    t.last_vacuum_time = Some(now());
    t.vacuum_count += 1;
    t.total_vacuum_time += ms;
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
    t.last_analyze_time = Some(now());
    t.analyze_count += 1;
    t.total_analyze_time += ms;
}

/// A checkpoint was requested and done (CHECKPOINT): it writes and syncs buffers and logs
/// a checkpoint record.
pub fn report_checkpoint(db: &crate::txn::Database) {
    let mut s = db.pgstat.lock();
    s.checkpoints_requested += 1;
    s.wal_records += 1;
    s.wal_bytes += 122;
    s.checkpoints_performed += 1;
    for op in ["writes", "fsyncs"] {
        *s.io
            .entry(("checkpointer".into(), "relation".into(), "normal".into(), op))
            .or_default() += 1;
    }
    *s.io
        .entry(("checkpointer".into(), "wal".into(), "normal".into(), "writes"))
        .or_default() += 1;
}


/// pg_stat_reset: the database's counters and every table's and function's entry.
pub fn reset_db(ctx: &ExecCtx) {
    let mut s = ctx.sess.db.pgstat.lock();
    s.tables.clear();
    s.funcs.clear();
    s.db = DbEntry {
        stat_reset: Some(now()),
        ..Default::default()
    };
}

/// pg_stat_reset_single_table_counters: a shared relation's resets database 0's time.
pub fn reset_table(ctx: &ExecCtx, rel: u32, shared: bool) {
    let mut s = ctx.sess.db.pgstat.lock();
    s.tables.remove(&rel);
    let t = now();
    if shared {
        s.shared_db.stat_reset = Some(t);
    } else {
        s.db.stat_reset = Some(t);
    }
}

pub fn reset_function(ctx: &ExecCtx, func: u32) {
    let mut s = ctx.sess.db.pgstat.lock();
    s.funcs.remove(&func);
    s.db.stat_reset = Some(now());
}

/// pg_stat_reset_shared / pg_stat_reset_slru: `target`'s reset time is now.
pub fn reset_shared(ctx: &ExecCtx, target: &str) {
    let mut s = ctx.sess.db.pgstat.lock();
    let t = now();
    if target == "io" {
        s.io.clear();
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
    static START: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    let start = *START.get_or_init(now);
    Some(
        *ctx.sess
            .db
            .pgstat
            .lock()
            .resets
            .entry(target.to_string())
            .or_insert(start),
    )
}

// ── the end of a transaction ──

/// AtEOXact_PgStat and the flush that follows when the backend goes idle: the
/// transaction's counts join the backend's, all of which go into the shared entries; the
/// entries of tables and functions no longer in the catalog go.
pub fn end_xact(
    sess: &crate::session::state::SessionState,
    xact: XactStats,
    commit: bool,
    cat: &crate::catalog::Catalog,
) {
    let wal = xact.wal_records;
    let counts = xact.end_xact(commit);
    let mut p = sess.pgstat.borrow_mut();
    for (rel, c) in counts {
        let t = p.tab(rel);
        t.tuples_inserted += c.tuples_inserted;
        t.tuples_updated += c.tuples_updated;
        t.tuples_deleted += c.tuples_deleted;
        if c.truncdropped {
            t.truncdropped = true;
            t.delta_live_tuples = 0;
            t.delta_dead_tuples = 0;
        }
        t.delta_live_tuples += c.delta_live_tuples;
        t.delta_dead_tuples += c.delta_dead_tuples;
        t.changed_tuples += c.changed_tuples;
    }
    let ts = now();
    let mut s = sess.db.pgstat.lock();
    for (rel, c) in p.tabs.drain() {
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
        let db = if shared { &mut s.shared_db } else { &mut s.db };
        db.tuples_returned += c.tuples_returned;
        db.tuples_fetched += c.tuples_fetched;
        db.tuples_inserted += c.tuples_inserted;
        db.tuples_updated += c.tuples_updated;
        db.tuples_deleted += c.tuples_deleted;
        db.blocks_fetched += c.blocks_fetched;
        db.blocks_hit += c.blocks_hit;
    }
    for (f, c) in p.funcs.drain() {
        let e = s.funcs.entry(f).or_default();
        e.calls += c.calls;
        e.total_time += c.total_time;
        e.self_time += c.self_time;
    }
    if commit {
        s.db.xact_commit += 1;
    } else {
        s.db.xact_rollback += 1;
    }
    // (A heap insert's record, about 60 bytes, and the commit record.)
    if wal > 0 {
        s.wal_records += wal + 1;
        s.wal_bytes += wal * 59 + 34;
        p.wal.0 += wal + 1;
        p.wal.1 += wal * 59 + 34;
        for op in ["writes", "fsyncs"] {
            *s.io
                .entry(("client backend".into(), "wal".into(), "normal".into(), op))
                .or_default() += 1;
        }
    }
    s.db.sessions += std::mem::take(&mut p.sessions);
    let (to_launch, launched) = std::mem::take(&mut p.parallel_workers);
    s.db.parallel_workers_to_launch += to_launch;
    s.db.parallel_workers_launched += launched;
    // Dropped objects' statistics go with them.
    s.tables.retain(|rel, _| {
        *rel < crate::catalog::FIRST_USER_OID
            || cat.tables.contains_key(rel)
            || cat.indexes.contains_key(rel)
            || cat.views.contains_key(rel)
            || cat.toast_owner(*rel).is_some()
    });
    s.funcs
        .retain(|f, _| *f < crate::catalog::FIRST_USER_OID || cat.functions.contains_key(f));
    drop(s);
    p.clear_snapshot();
}
