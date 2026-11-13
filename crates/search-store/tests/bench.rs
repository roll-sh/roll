//! WAL append throughput. Ignored by default; run with
//! `cargo test -p search-store --release --test bench -- --ignored --nocapture`.
//! Uses a directory on the target disk (not tmpfs) so fsync costs are real; set
//! `ROLL_BENCH_DIR` to measure elsewhere.

mod common;

use common::*;
use search_store::{Store, WalConfig, test_objects};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn bench_dir(name: &str) -> PathBuf {
    let base = std::env::var("ROLL_BENCH_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(env!("CARGO_TARGET_TMPDIR")));
    let dir = base.join(format!("search-store-bench-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `tasks` writers, each sending requests of `batch` ops of `size` bytes and waiting for the
/// acknowledgement of each request, for `dur`. Returns (ops/s, MB/s, commits).
async fn run(store: &Store, tasks: usize, batch: usize, size: usize, dur: Duration) -> (f64, f64, u64) {
    static RUN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let run = RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut shards = Vec::new();
    for t in 0..tasks {
        let (w, _, _) = recover(store, &format!("bench{}", t % 16), &shard(&format!("r{run}idx{t}"), 0)).await;
        shards.push(w);
    }
    let before = store.wal().stats();
    let start = Instant::now();
    let deadline = start + dur;
    let mut handles = Vec::new();
    for w in shards {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            let op = vec![0x5au8; size];
            let mut seq = 0i64;
            while Instant::now() < deadline {
                let mut lsn = 0;
                for _ in 0..batch {
                    lsn = w.append(seq, &op).unwrap();
                    seq += 1;
                }
                store.wal().wait_ack(lsn).await.unwrap();
            }
            seq as u64
        }));
    }
    let mut ops = 0;
    for h in handles {
        ops += h.await.unwrap();
    }
    let secs = start.elapsed().as_secs_f64();
    let after = store.wal().stats();
    let commits = after.commits - before.commits;
    println!("        avg fsync {:.2} ms", (after.fsync_nanos - before.fsync_nanos) as f64 / commits.max(1) as f64 / 1e6);
    (ops as f64 / secs, (ops * size as u64) as f64 / secs / 1e6, commits)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn wal_append_throughput() {
    let dur = Duration::from_millis(std::env::var("ROLL_BENCH_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(2000));
    for (label, flush_delay) in [("5ms window", Duration::from_millis(5)), ("eager", Duration::ZERO)] {
        let objects = test_objects("bench");
        let cfg = search_store::StoreConfig {
            node: "bench".into(),
            wal: WalConfig { flush_delay, ..WalConfig::default() },
            ..search_store::StoreConfig::default()
        };
        let store = Store::open(objects, bench_dir(label.split(' ').next().unwrap()), cfg).await.unwrap();
        println!("\n== group commit: {label} (fsync on, NVMe) ==");
        println!("{:>6} {:>6} {:>6} {:>12} {:>9} {:>10} {:>9}", "tasks", "batch", "bytes", "ops/s", "MB/s", "commits", "ops/fsync");
        for &(tasks, batch, size) in &[
            (1, 1, 256),
            (4, 1, 256),
            (16, 1, 256),
            (64, 1, 256),
            (256, 1, 256),
            (1, 100, 256),
            (16, 100, 256),
            (64, 100, 1024),
            (16, 1000, 1024),
        ] {
            let (ops, mb, commits) = run(&store, tasks, batch, size, dur).await;
            let secs = dur.as_secs_f64();
            println!("{tasks:>6} {batch:>6} {size:>6} {ops:>12.0} {mb:>9.1} {commits:>10} {:>9.1}", ops * secs / commits.max(1) as f64);
        }
        store.close().await.unwrap();
    }
}

/// Raw append cost without waiting for durability (the in-memory hot path). The WAL goes to
/// the temp dir without fsync so the disk doesn't throttle appends through backpressure.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn wal_append_hot_path() {
    let objects = test_objects("bench-hot");
    let cfg = search_store::StoreConfig {
        node: "bench".into(),
        wal: WalConfig { fsync: false, max_pending_bytes: 256 << 20, ..WalConfig::default() },
        ..search_store::StoreConfig::default()
    };
    let store = Store::open(objects, temp_dir("bench-hot"), cfg).await.unwrap();
    // `batch` 1 is `append`, larger ones `append_many` (a bulk request's ops for one shard).
    for (threads, batch) in [(1usize, 1usize), (4, 1), (8, 1), (1, 64), (4, 64), (8, 64)] {
        let mut ws = Vec::new();
        for t in 0..threads {
            ws.push(recover(&store, "hot", &shard(&format!("h{t}b{batch}"), 0)).await.0);
        }
        let n = 2_000_000 / threads / batch * batch;
        let start = Instant::now();
        let hs: Vec<_> = ws
            .into_iter()
            .map(|w| {
                std::thread::spawn(move || {
                    let op = vec![1u8; 200];
                    let mut ops: Vec<(i64, &[u8])> = (0..batch).map(|_| (0, &op[..])).collect();
                    let mut lsn = 0;
                    for i in (0..n).step_by(batch) {
                        if batch == 1 {
                            lsn = w.append(i as i64, &op).unwrap();
                        } else {
                            for (j, o) in ops.iter_mut().enumerate() {
                                o.0 = (i + j) as i64;
                            }
                            lsn = w.append_many(&ops).unwrap();
                        }
                    }
                    lsn
                })
            })
            .collect();
        let mut last = 0;
        for h in hs {
            last = last.max(h.join().unwrap());
        }
        let el = start.elapsed();
        store.wal().durable(last).await.unwrap();
        let total = n * threads;
        print!("batch {batch:>3}, ");
        println!("{threads} threads: {:.2} M appends/s ({:.0} ns/append), {:.0} MB/s", total as f64 / el.as_secs_f64() / 1e6, el.as_nanos() as f64 / total as f64, (total * 200) as f64 / el.as_secs_f64() / 1e6);
    }
    store.close().await.unwrap();
}
