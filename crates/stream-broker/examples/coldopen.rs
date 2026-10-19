//! How long a closed partition takes to open and serve its first read, by size and by how it was
//! closed (cleanly, or by a crash, which makes the newest segments get validated).
//!
//! ```sh
//! cargo run --release -p stream-broker --example coldopen
//! ```

use bytes::Bytes;
use std::time::Instant;
use stream_log::batch::{self, BatchOptions, NewRecord};
use stream_log::{AppendOptions, Log, LogConfig, ReadPlan, Recovered, read_local};

fn fill(dir: &std::path::Path, mb: usize, config: &LogConfig) {
    let log = Log::open(dir, config.clone(), Recovered::default(), 0).unwrap();
    // 16 KB batches of 200 B records, like a typical Kafka producer.
    let recs: Vec<NewRecord> = (0..80).map(|_| NewRecord { value: Some(Bytes::from(vec![b'x'; 190])), ..Default::default() }).collect();
    let b = batch::build(&recs, BatchOptions::default(), 1);
    let n = mb * 1_000_000 / b.len();
    for _ in 0..n {
        log.append(std::slice::from_ref(&b), AppendOptions::at(1)).unwrap();
    }
    log.flush().unwrap();
}

fn first_read(log: &Log) -> usize {
    let r = log.read(0, 1 << 20, false).unwrap();
    match r.plan {
        ReadPlan::Ready(b) => b.len(),
        ReadPlan::Local(l) => read_local(&l, r.offset, r.upper, r.max_bytes, 1 << 16).unwrap().len(),
        ReadPlan::Remote(_) => unreachable!(),
    }
}

fn main() {
    let config = LogConfig::default();
    for mb in [1usize, 64, 256, 1024] {
        let dir = std::env::temp_dir().join(format!("roll-coldopen-{mb}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        fill(&dir, mb, &config);
        let mut row = format!("{mb:>5} MB:");
        for clean in [true, false] {
            let mut best = f64::MAX;
            for _ in 0..5 {
                if clean {
                    Log::open(&dir, config.clone(), Recovered::default(), 0).unwrap().mark_clean().unwrap();
                }
                let t = Instant::now();
                let log = Log::open(&dir, config.clone(), Recovered::default(), 0).unwrap();
                first_read(&log);
                best = best.min(t.elapsed().as_secs_f64() * 1e3);
            }
            row += &format!("  {} {best:.2} ms", if clean { "after a clean close" } else { "after a crash" });
        }
        println!("{row}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
