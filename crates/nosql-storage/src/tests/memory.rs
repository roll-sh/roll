//! Memory budgets ([`crate::memory`]): flushes under pressure across many databases, write
//! backpressure and its timeout, per-database caps and the fair share, cache budgets shared by
//! databases, the commit log backlog, and unloads releasing everything a database held.

use super::*;
use crate::cache::CacheConfig;
use crate::db::{Database, DbConfig};
use crate::log::LogConfig;
use crate::memory::Overloaded;
use crate::store::{Store, StoreConfig};
use std::sync::atomic::Ordering as AO;
use std::time::Duration;

const T1: TableId = TableId(0x51);
const NOW: i64 = 2_000_000;

fn config(budget: u64, db_cap: u64, timeout: Duration) -> StoreConfig {
    StoreConfig {
        node: "n1".into(),
        cache: CacheConfig { memory_bytes: 1 << 20, ..Default::default() },
        log: LogConfig { flush_interval: Duration::from_millis(20), ..Default::default() },
        memtable_budget_bytes: budget,
        memtable_db_bytes: db_cap,
        write_timeout: timeout,
        maintenance_interval: Duration::from_secs(3600),
        manifest_interval: Duration::from_secs(3600),
        auto_compaction: false,
        ..Default::default()
    }
}

async fn open(s: &Store, id: &str, cfg: DbConfig) -> Arc<Database> {
    let db = s.open(id, cfg).await.unwrap();
    db.table(T1, TableOptions::default()).await.unwrap();
    db
}

fn update(k: u32, c: u32, len: usize) -> PartitionUpdate {
    PartitionUpdate::new(key(k)).with_row(row(c, 1, &"v".repeat(len)))
}

async fn put(db: &Database, k: u32, c: u32, len: usize) -> anyhow::Result<()> {
    db.get_table(T1).unwrap().apply(update(k, c, len)).await
}

fn overloaded(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Overloaded>().is_some()
}

/// Writes until one fails; returns how many went in.
async fn fill(db: &Database, from: u32, len: usize) -> (u32, anyhow::Error) {
    for i in 0..100_000 {
        if let Err(e) = put(db, from + i, 0, len).await {
            return (i, e);
        }
    }
    panic!("writes never waited");
}

#[tokio::test]
async fn pressure_flushes_keep_many_databases_under_the_budget() {
    let budget = 256 << 10;
    let s = Store::start(crate::test_objects("mem-pressure"), config(budget, budget, Duration::from_secs(30))).await.unwrap();
    let mut dbs = Vec::new();
    for i in 0..8 {
        dbs.push(open(&s, &format!("d{i}"), DbConfig::default()).await);
    }
    let peak = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let writers = dbs.iter().map(|db| {
        let (db, peak, s) = (db.clone(), peak.clone(), s.clone());
        async move {
            for k in 0..300 {
                put(&db, k, 0, 200).await.unwrap();
                peak.fetch_max(s.memory_stats().memtable_bytes, AO::Relaxed);
            }
        }
    });
    futures::future::join_all(writers).await;
    // Past the budget only by its eighth and the writes admitted at the same moment.
    let peak = peak.load(AO::Relaxed);
    assert!(peak <= budget + budget / 8 + 8 * (4 << 10), "peak {peak}");
    let st = s.memory_stats();
    assert!(st.pressure_flushes > 0);
    assert_eq!(st.write_timeouts, 0);
    assert!(dbs.iter().filter(|d| !d.get_table(T1).unwrap().state().sstables.is_empty()).count() >= 4);
    // Nothing was lost on the way.
    for db in &dbs {
        let t = db.get_table(T1).unwrap();
        for k in [0, 150, 299] {
            assert!(read(&t, key(k), NOW).await.is_some(), "{} {k}", db.id);
        }
    }
    for i in 0..8 {
        s.unload(&format!("d{i}")).await.unwrap();
    }
    assert_eq!((s.memory_stats().memtable_bytes, s.memory_stats().databases_with_memtables), (0, 0));
}

#[tokio::test]
async fn writes_wait_for_flushes_and_time_out_when_none_can_run() {
    let s = Store::start(crate::test_objects("mem-backpressure"), config(1 << 20, 64 << 10, Duration::from_millis(300))).await.unwrap();
    let db = open(&s, "d", DbConfig::default()).await;
    // No flush can run: the memtable can't be freed.
    let stalled = s.env.flushes.clone().acquire_many_owned(s.config().flush_concurrency as u32).await.unwrap();
    let started = std::time::Instant::now();
    let (n, e) = fill(&db, 0, 500).await;
    assert!(overloaded(&e), "{e:#}");
    assert!(e.downcast_ref::<Overloaded>().unwrap().waited_ms >= 300);
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert!(n > 10);
    // The cap held (the last admitted write may cross it).
    assert!(db.memory().used() <= (64 << 10) + (2 << 10), "{}", db.memory().used());
    let st = s.memory_stats();
    assert!(st.blocked_writes >= 1 && st.write_timeouts >= 1);
    assert_eq!(st.blocked_writes_now, 0);
    // A waiting write goes in as soon as flushes run again.
    let w = tokio::spawn({
        let db = db.clone();
        async move { put(&db, 999_999, 0, 500).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(s.memory_stats().blocked_writes_now, 1);
    drop(stalled);
    w.await.unwrap().unwrap();
    assert!(read(&db.get_table(T1).unwrap(), key(999_999), NOW).await.is_some());
    // Batches wait the same way.
    let stalled = s.env.flushes.clone().acquire_many_owned(s.config().flush_concurrency as u32).await.unwrap();
    let e = loop {
        match db.apply_batch(vec![(T1, update(5, 1, 500)), (T1, update(6, 1, 500))]).await {
            Ok(()) => continue,
            Err(e) => break e,
        }
    };
    assert!(overloaded(&e), "{e:#}");
    drop(stalled);
}

#[tokio::test]
async fn one_database_cannot_take_the_others_memory() {
    let budget = 128 << 10;
    let s = Store::start(crate::test_objects("mem-fair"), config(budget, budget, Duration::from_millis(200))).await.unwrap();
    let big = open(&s, "big", DbConfig::default()).await;
    let small = open(&s, "small", DbConfig::default()).await;
    let capped = open(&s, "capped", DbConfig { memtable_space_bytes: Some(16 << 10), ..Default::default() }).await;
    let _stalled = s.env.flushes.clone().acquire_many_owned(s.config().flush_concurrency as u32).await.unwrap();
    // A database's own cap (its plan) stops it long before the node's budget does.
    assert_eq!(capped.memory().cap(), 16 << 10);
    let (n, e) = fill(&capped, 0, 100).await;
    assert!(overloaded(&e) && n > 3);
    assert!(capped.memory().used() >= 16 << 10 && capped.memory().used() <= (16 << 10) + (1 << 10));
    // The big tenant fills the node's budget, then waits.
    let (_, e) = fill(&big, 0, 500).await;
    assert!(overloaded(&e));
    assert!(s.memory_stats().memtable_bytes >= budget);
    // The others still write: they hold less than their share.
    for k in 0..5 {
        put(&small, k, 0, 500).await.unwrap();
    }
    assert!(s.memory_stats().memtable_bytes > budget);
    assert!(overloaded(&put(&big, 10_000_000, 0, 10).await.unwrap_err()));
    // Up to the node's budget plus an eighth, or their share.
    let (_, e) = fill(&small, 100, 500).await;
    assert!(overloaded(&e));
    assert!(s.memory_stats().memtable_bytes <= budget + budget / 8 + (2 << 10));
    assert_eq!(s.memory_stats().databases_with_memtables, 3);
}

#[tokio::test]
async fn caps_change_live_and_never_exceed_the_budget() {
    let s = Store::start(crate::test_objects("mem-caps"), config(1 << 20, 256 << 10, Duration::from_secs(1))).await.unwrap();
    let db = open(&s, "d", DbConfig::default()).await;
    assert_eq!(db.memory().cap(), 256 << 10);
    db.set_config(DbConfig { memtable_space_bytes: Some(32 << 10), ..Default::default() });
    assert_eq!(db.memory().cap(), 32 << 10);
    db.set_config(DbConfig { memtable_space_bytes: Some(1 << 30), ..Default::default() });
    assert_eq!(db.memory().cap(), 1 << 20, "never above the node's budget");
    db.set_config(DbConfig::default());
    assert_eq!(db.memory().cap(), 256 << 10);
    let st = db.memory_stats();
    assert_eq!(st.memtable_cap_bytes, 256 << 10);
    assert_eq!(s.memory_stats().memtable_db_bytes, 256 << 10);
}

#[tokio::test]
async fn caches_hold_their_budgets_across_databases() {
    let disk = std::env::temp_dir().join(format!("roll-nosql-cache-{}", rand::random::<u64>()));
    let mut cfg = config(64 << 20, 64 << 20, Duration::from_secs(5));
    cfg.cache = CacheConfig { memory_bytes: 64 << 10, disk_dir: Some(disk.clone()), disk_bytes: 128 << 10, ..Default::default() };
    let s = Store::start(crate::test_objects("mem-cache"), cfg).await.unwrap();
    let mut dbs = Vec::new();
    for id in ["a", "b"] {
        let db = s.open(id, DbConfig::default()).await.unwrap();
        db.table(T1, TableOptions { block_size: 2 << 10, ..Default::default() }).await.unwrap();
        for k in 0..300 {
            put(&db, k, 0, 200).await.unwrap();
        }
        db.flush().await.unwrap();
        dbs.push(db);
    }
    for db in &dbs {
        let t = db.get_table(T1).unwrap();
        for k in 0..300 {
            assert!(read(&t, key(k), NOW).await.is_some());
        }
    }
    let st = s.memory_stats();
    assert!(st.cache_memory_bytes > 0 && st.cache_memory_bytes <= 64 << 10, "{st:?}");
    assert!(st.cache_disk_bytes > 0 && st.cache_disk_bytes <= 128 << 10, "{st:?}");
    assert_eq!((st.cache_memory_budget_bytes, st.cache_disk_budget_bytes), (64 << 10, 128 << 10));
    // Shared: what each database holds adds up to the totals.
    let held: u64 = dbs.iter().map(|d| d.memory_stats().cache_bytes).sum();
    assert_eq!(held, st.cache_memory_bytes + st.cache_disk_bytes);
    assert!(dbs[1].memory_stats().cache_bytes > 0);
    // An unload drops its blocks from both tiers.
    let b = dbs[1].memory_stats().cache_bytes;
    s.unload("b").await.unwrap();
    let after = s.memory_stats();
    assert_eq!(after.cache_memory_bytes + after.cache_disk_bytes, held - b);
    let _ = std::fs::remove_dir_all(disk);
}

static RELEASES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[tokio::test]
async fn unload_stops_compactions_drops_caches_and_trims_the_allocator() {
    crate::memory::set_allocator_release(|| {
        RELEASES.fetch_add(1, AO::Relaxed);
    });
    let tmp = std::env::temp_dir().join(format!("roll-nosql-unload-{}", rand::random::<u64>()));
    let mut cfg = config(64 << 20, 64 << 20, Duration::from_secs(5));
    cfg.tmp_dir = Some(tmp.clone());
    let s = Store::start(crate::test_objects("mem-unload"), cfg).await.unwrap();
    let db = s.open("d", DbConfig::default()).await.unwrap();
    let t = db.table(T1, TableOptions { block_size: 1 << 10, ..Default::default() }).await.unwrap();
    for round in 0..6u32 {
        for k in 0..400 {
            put(&db, k, round, 300).await.unwrap();
        }
        t.flush().await.unwrap();
    }
    for k in 0..50 {
        read(&t, key(k), NOW).await.unwrap();
    }
    put(&db, 7, 99, 300).await.unwrap();
    assert!(db.memory().used() > 0 && db.memory_stats().cache_bytes > 0);
    let before = RELEASES.load(AO::Relaxed);
    // A major compaction is running (or about to) when the database unloads.
    let compaction = tokio::spawn({
        let t = t.clone();
        async move { t.compact_major().await }
    });
    tokio::task::yield_now().await;
    let started = std::time::Instant::now();
    assert!(s.unload("d").await.unwrap());
    let unload = started.elapsed();
    compaction.await.unwrap().unwrap();
    assert!(unload < Duration::from_secs(5), "{unload:?}");
    // Nothing of it is left: memtables, cached blocks, scratch files.
    let st = s.memory_stats();
    assert_eq!((st.memtable_bytes, st.databases_with_memtables, st.cache_memory_bytes, st.sstable_index_bytes), (0, 0, 0, 0), "{st:?}");
    assert_eq!(db.memory().used(), 0);
    assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0, "scratch files removed");
    assert!(RELEASES.load(AO::Relaxed) > before, "freed memory handed back to the OS");
    // The data is all there after the unload.
    let db = s.open("d", DbConfig::default()).await.unwrap();
    let t = db.table(T1, TableOptions::default()).await.unwrap();
    assert_eq!(read(&t, key(7), NOW).await.unwrap().rows.len(), 7);
    let _ = std::fs::remove_dir_all(tmp);
}

#[tokio::test]
async fn compaction_reads_ahead_within_its_budget() {
    use crate::table::compaction_readahead;
    assert_eq!(compaction_readahead(16 << 20, 4, 64 << 10), 64);
    assert_eq!(compaction_readahead(16 << 20, 32, 64 << 10), 8);
    assert_eq!(compaction_readahead(16 << 20, 1000, 64 << 10), 1);
    assert_eq!(compaction_readahead(1 << 40, 1, 64 << 10), 64);
    assert_eq!(compaction_readahead(0, 0, 0), 1);
}

#[tokio::test]
async fn a_commit_log_backlog_makes_writes_wait() {
    let mut cfg = config(64 << 20, 64 << 20, Duration::from_millis(200));
    cfg.log = LogConfig { flush_interval: Duration::from_millis(20), max_pending_bytes: 8 << 10, ..Default::default() };
    let s = Store::start(crate::test_objects("mem-log"), cfg).await.unwrap();
    let db = open(&s, "d", DbConfig::default()).await;
    // Object storage is down: frames pile up until writes wait, then time out.
    s.shipper.fail.store(true, AO::SeqCst);
    let (n, e) = fill(&db, 0, 500).await;
    assert!(overloaded(&e), "{e:#}");
    assert!(n >= 8);
    assert!(s.memory_stats().log_pending_bytes >= 8 << 10);
    assert_eq!(s.memory_stats().log_pending_limit_bytes, 8 << 10);
    // It is back: the backlog ships (after the upload's retry backoff) and writes go on.
    s.shipper.fail.store(false, AO::SeqCst);
    let mut ok = false;
    for i in 0..50 {
        if put(&db, 1_000_000 + i, 0, 10).await.is_ok() {
            ok = true;
            break;
        }
    }
    assert!(ok);
}
