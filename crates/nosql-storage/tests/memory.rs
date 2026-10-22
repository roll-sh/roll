//! Memory against the real heap, measured by a counting allocator: the memtable budget must
//! account for what writes pin (or the budgets bound nothing), and an unload must give back
//! everything the database held.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use nosql_storage::db::DbConfig;
use nosql_storage::log::LogConfig;
use nosql_storage::manifest::TableId;
use nosql_storage::options::TableOptions;
use nosql_storage::store::{Store, StoreConfig};
use nosql_storage::{Cell, Clustering, LivenessInfo, PartitionKey, PartitionUpdate, Row};

struct Counting;

static LIVE: AtomicI64 = AtomicI64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size() as i64, Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as i64, Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        LIVE.fetch_add(new as i64 - l.size() as i64, Ordering::Relaxed);
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static A: Counting = Counting;

/// The counter is process-wide: one measurement at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn key(i: u64) -> PartitionKey {
    PartitionKey::new((i as i64).wrapping_mul(0x9E37_79B9_7F4A_7C15u64 as i64), Bytes::from(i.to_be_bytes().to_vec()))
}

/// Rows like the benchmark's batch table: `(pk bigint, ck int, payload text)`, 100-byte payloads.
fn update(i: u64, rows: u32) -> PartitionUpdate {
    let mut u = PartitionUpdate::new(key(i));
    for c in 0..rows {
        let ts = 1_000 + i as i64;
        u = u.with_row(Row::new(Clustering::new([c.to_be_bytes()])).with_liveness(LivenessInfo::live(ts)).with_cell(1, Cell::live(ts, Bytes::from(vec![b'x'; 100]))));
    }
    u
}

#[tokio::test(flavor = "current_thread")]
async fn memtable_accounting_covers_the_heap() {
    let _serial = SERIAL.lock().await;
    let cfg = StoreConfig {
        memtable_budget_bytes: 1 << 40,
        memtable_db_bytes: 1 << 40,
        maintenance_interval: Duration::from_secs(3600),
        manifest_interval: Duration::from_secs(3600),
        log: LogConfig { flush_interval: Duration::from_secs(3600), ..Default::default() },
        auto_compaction: false,
        ..Default::default()
    };
    let s = Store::start(nosql_storage::test_objects("memory-accounting"), cfg).await.unwrap();
    let db = s.open("d", DbConfig::default()).await.unwrap();
    let t = db.table(TableId(1), TableOptions { memtable_flush_bytes: u64::MAX, ..Default::default() }).await.unwrap();
    for (rows, n) in [(2u32, 20_000u64), (1, 20_000), (50, 400)] {
        s.shipper.flush().await.unwrap();
        let before = LIVE.load(Ordering::Relaxed);
        let accounted_before = db.memory().used();
        let base = n * 1_000 * rows as u64;
        for i in 0..n {
            t.apply(update(base + i, rows)).await.unwrap();
        }
        // The commit log buffer is accounted separately (`log_pending_bytes`).
        s.shipper.flush().await.unwrap();
        let heap = (LIVE.load(Ordering::Relaxed) - before) as f64;
        let accounted = (db.memory().used() - accounted_before) as f64;
        let ratio = accounted / heap;
        eprintln!("{rows} rows per partition: heap {heap} accounted {accounted} ratio {ratio:.2}");
        assert!((0.9..1.4).contains(&ratio), "{rows} rows per partition: accounted {accounted} for {heap} bytes of heap");
    }
    assert_eq!(db.memory().used(), t.memtable_bytes());
    // Flushed memtables give their bytes back.
    t.flush().await.unwrap();
    assert_eq!(db.memory().used(), 0);
    assert_eq!(db.memory().node().used(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn unload_gives_the_heap_back() {
    let _serial = SERIAL.lock().await;
    let tmp = std::env::temp_dir().join(format!("roll-nosql-heap-{}", std::process::id()));
    let cfg = StoreConfig {
        tmp_dir: Some(tmp.clone()),
        maintenance_interval: Duration::from_secs(3600),
        manifest_interval: Duration::from_secs(3600),
        ..Default::default()
    };
    let s = Store::start(nosql_storage::test_objects("memory-unload"), cfg).await.unwrap();
    // Warm up what the process keeps anyway (runtime, object storage, the first database).
    let db = s.open("warm", DbConfig::default()).await.unwrap();
    db.table(TableId(1), TableOptions::default()).await.unwrap().apply(update(0, 1)).await.unwrap();
    s.unload("warm").await.unwrap();
    drop(db);
    let baseline = LIVE.load(Ordering::Relaxed);
    let db = s.open("d", DbConfig::default()).await.unwrap();
    let t = db.table(TableId(1), TableOptions { block_size: 4 << 10, ..Default::default() }).await.unwrap();
    for round in 0..4u64 {
        for i in 0..5_000 {
            t.apply(update(round * 1_000_000 + i, 2)).await.unwrap();
        }
        t.flush().await.unwrap();
    }
    for i in 0..5_000 {
        t.apply(update(9_000_000 + i, 2)).await.unwrap();
    }
    // Reads fill the block cache; a compaction runs.
    for i in (0..5_000).step_by(7) {
        let r = nosql_storage::read::PartitionRead::new(key(i), 2_000_000);
        assert_eq!(t.read(r).partitions().await.unwrap().len(), 1);
    }
    t.compact_major().await.unwrap();
    let loaded = LIVE.load(Ordering::Relaxed) - baseline;
    assert!(loaded > 4 << 20, "the database held {loaded} bytes");
    s.unload("d").await.unwrap();
    drop((t, db));
    let left = LIVE.load(Ordering::Relaxed) - baseline;
    eprintln!("loaded {loaded} bytes, {left} left after the unload");
    assert!(left < 256 << 10, "{left} of {loaded} bytes still held after the unload");
    let _ = std::fs::remove_dir_all(tmp);
}
