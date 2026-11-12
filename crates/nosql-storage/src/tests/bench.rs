//! Micro-benchmarks (`cargo test --release -p nosql-storage -- --ignored bench --nocapture`).
//! Sized to stay well under 500 MB RSS.

use super::*;
use crate::read::PartitionRead;
use std::time::Instant;

const N: u32 = 200_000;

fn wide_row(i: u32) -> PartitionUpdate {
    PartitionUpdate::new(key(i % 20_000)).with_row(
        Row::new(ck(i / 20_000))
            .with_liveness(LivenessInfo::live(i as i64))
            .with_cell(1, Cell::live(i as i64, Bytes::from(format!("value-{i:08}"))))
            .with_cell(2, Cell::live(i as i64, Bytes::from_static(b"0123456789abcdef"))),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn bench_writes_and_point_reads() {
    let env = env_with(crate::test_objects("bench"), TestClock::new(1_000).clock(), 64 << 20);
    let t = Table::standalone(TableId(1), "bench", env, TableOptions { memtable_flush_bytes: u64::MAX, ..Default::default() });
    // Writes, from 4 tasks.
    let start = Instant::now();
    let mut tasks = Vec::new();
    for w in 0..4u32 {
        let t = t.clone();
        tasks.push(tokio::spawn(async move {
            for i in (w..N).step_by(4) {
                t.apply(wide_row(i)).await.unwrap();
            }
        }));
    }
    for x in tasks {
        x.await.unwrap();
    }
    let el = start.elapsed();
    println!("writes (memtable, 4 writers): {:.0}/s, memtable {} MiB", N as f64 / el.as_secs_f64(), t.memtable_bytes() >> 20);
    // Point reads from the memtable.
    let reads = 50_000u32;
    let start = Instant::now();
    for i in 0..reads {
        let p = t.read(PartitionRead::new(key((i * 7919) % 20_000), 1)).partitions().await.unwrap();
        assert_eq!(p[0].rows.len(), 10);
    }
    println!("point reads (memtable, 10-row partitions): {:.0}/s", reads as f64 / start.elapsed().as_secs_f64());
    let start = Instant::now();
    t.flush().await.unwrap();
    println!("flush: {:.2} s, {} KiB stored", start.elapsed().as_secs_f64(), t.stats().space_used >> 10);
    // Point reads from the SSTable: first pass fetches blocks, second is cache-warm.
    for pass in ["cold", "warm"] {
        let start = Instant::now();
        for i in 0..reads {
            let p = t.read(PartitionRead::new(key((i * 7919) % 20_000), 1)).partitions().await.unwrap();
            assert_eq!(p[0].rows.len(), 10);
        }
        println!("point reads (sstable, {pass} cache): {:.0}/s", reads as f64 / start.elapsed().as_secs_f64());
    }
    // A single-row point read with a names filter.
    let start = Instant::now();
    for i in 0..reads {
        let mut r = PartitionRead::new(key((i * 7919) % 20_000), 1);
        r.clustering = crate::read::ClusteringFilter::Names(vec![ck(3)]);
        let p = t.read(r).partitions().await.unwrap();
        assert_eq!(p[0].rows.len(), 1);
    }
    println!("single-row reads (sstable, warm): {:.0}/s", reads as f64 / start.elapsed().as_secs_f64());
    let start = Instant::now();
    let n: usize = scan_all(&t, 1).await.iter().map(|p| p.rows.len()).sum();
    println!("full scan: {:.0} rows/s", n as f64 / start.elapsed().as_secs_f64());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn bench_logged_writes() {
    use crate::db::DbConfig;
    use crate::store::{Store, StoreConfig};
    let s = Store::start(crate::test_objects("bench-log"), StoreConfig { memtable_budget_bytes: 256 << 20, auto_compaction: false, ..Default::default() }).await.unwrap();
    let db = s.open("bench", DbConfig::default()).await.unwrap();
    let t = db.table(TableId(1), TableOptions { memtable_flush_bytes: u64::MAX, ..Default::default() }).await.unwrap();
    let start = Instant::now();
    let mut tasks = Vec::new();
    for w in 0..4u32 {
        let t = t.clone();
        tasks.push(tokio::spawn(async move {
            for i in (w..N).step_by(4) {
                t.apply(wide_row(i)).await.unwrap();
            }
        }));
    }
    for x in tasks {
        x.await.unwrap();
    }
    let el = start.elapsed();
    s.shipper.flush().await.unwrap();
    println!(
        "logged writes (async durability, 4 writers): {:.0}/s, {} segments, {} KiB shipped",
        N as f64 / el.as_secs_f64(),
        s.shipper.segments_put.load(std::sync::atomic::Ordering::Relaxed),
        s.shipper.bytes_put.load(std::sync::atomic::Ordering::Relaxed) >> 10
    );
}
