//! Per-row cost of scans, in process, on the calling thread's CPU time: the store's iterator
//! alone (`store`), and sequential scans through SQL (`sql`), over data fresh (memtables and
//! L0 files) or compacted into L1. See BENCH.md (round 22, agent 117). Run with
//! `cargo run --release -p pg-engine --example scan_bench -- [store|sql] [rows]`.
#[path = "../tests/common/mod.rs"]
mod common;
use bytes::Bytes;
use pg_engine::storage::cache::BlockCache;
use pg_engine::storage::{Store, StoreOptions};
use std::sync::Arc;

// The allocator the roll-engine server has.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// This thread's CPU time in ns.
fn cpu_ns() -> u64 {
    let s = std::fs::read_to_string("/proc/thread-self/schedstat").unwrap_or_default();
    s.split_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// The process's CPU time in ns (user + system, in clock ticks: threads that ended
/// included).
fn process_cpu_ns() -> u64 {
    let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    // (Fields after the command's closing parenthesis; utime and stime are the 14th and 15th.)
    let rest = s.rsplit_once(')').map_or("", |r| r.1);
    let f: Vec<u64> = rest.split_whitespace().filter_map(|v| v.parse().ok()).collect();
    f.get(11).zip(f.get(12)).map_or(0, |(u, s)| (u + s) * 10_000_000)
}

/// The median over `reps` runs of `f`'s CPU ns per row (`rows` per run) on this thread, and
/// the process's over all runs.
fn per_row2(reps: usize, rows: usize, f: &mut dyn FnMut() -> usize) -> (f64, f64) {
    let p = process_cpu_ns();
    let mut v: Vec<f64> = (0..reps)
        .map(|_| {
            let t = cpu_ns();
            let n = f();
            assert_eq!(n, rows, "rows scanned");
            (cpu_ns() - t) as f64 / rows as f64
        })
        .collect();
    let all = (process_cpu_ns() - p) as f64 / (rows * reps) as f64;
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (v[v.len() / 2], all)
}

fn per_row(reps: usize, rows: usize, f: &mut dyn FnMut() -> usize) -> f64 {
    per_row2(reps, rows, f).0
}

/// Runs per measurement (`SCAN_REPS`, default 7).
fn reps() -> usize {
    std::env::var("SCAN_REPS").ok().and_then(|r| r.parse().ok()).unwrap_or(7)
}

/// Whether to measure data in `state` (`SCAN_STATE`: fresh or compacted; default both).
fn wanted(state: &str) -> bool {
    std::env::var("SCAN_STATE").ok().filter(|s| !s.is_empty()).is_none_or(|s| s == state)
}

fn open(dir: &std::path::Path) -> Arc<Store> {
    let opts = StoreOptions {
        memtable_bytes: 32 << 20,
        sync: false,
        background: false,
        cache: BlockCache::new(1 << 30),
        ..StoreOptions::default()
    };
    Store::open(dir, opts).expect("open store")
}

/// Rows of a table: an 8-byte prefix and an 8-byte row id, `width` value bytes.
fn load(s: &Arc<Store>, rows: usize, width: usize, from: usize) {
    let mut batch = Vec::new();
    for i in from..from + rows {
        let mut k = vec![0, 0, 0x40, 0x09, 0, 0, 0, 1];
        k.extend_from_slice(&(i as u64).to_be_bytes());
        let mut v = vec![0xFE, 5, 3];
        v.extend((0..width).map(|j| ((i * 31 + j * 7) % 251) as u8));
        batch.push((k, Some(Bytes::from(v))));
        if batch.len() == 20_000 {
            s.commit(std::mem::take(&mut batch), |_| Ok(())).unwrap();
        }
    }
    if !batch.is_empty() {
        s.commit(batch, |_| Ok(())).unwrap();
    }
}

fn store_bench(rows: usize) {
    for width in [16usize, 150] {
        let dir = tempfile::tempdir().unwrap();
        let s = open(dir.path());
        // Fresh: a third in L1, a third in an L0 file, a third in the memtable.
        load(&s, rows / 3, width, 0);
        s.flush_memtables().unwrap();
        s.compact().unwrap();
        load(&s, rows / 3, width, rows / 3);
        s.flush_memtables().unwrap();
        load(&s, rows - 2 * (rows / 3), width, 2 * (rows / 3));
        let (lo, hi) = (vec![0u8], vec![0xFFu8; 4]);
        let mut scan = || {
            let ts = s.visible_ts();
            s.scan_raw(&lo, &hi, ts).map(|r| r.unwrap().1.len()).filter(|&l| l > 0).count()
        };
        if wanted("fresh") {
            scan();
            println!("store width {width:3} fresh     {:7.1} ns/row", per_row(reps(), rows, &mut scan));
        }
        s.flush_memtables().unwrap();
        s.compact().unwrap();
        if wanted("compacted") {
            scan();
            println!("store width {width:3} compacted {:7.1} ns/row", per_row(reps(), rows, &mut scan));
        }
    }
}

fn sql_bench(rows: usize) {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let mut s = common::engine_session(store.clone(), "bench");
    let run = |s: &mut pg_engine::session::Session, q: &str| s.query(q).unwrap_or_else(|e| panic!("{q}: {e:?}"));
    // (`SCAN_QUERY`: only that query, and only its table made; `SCAN_WORKERS`: only that
    // setting; `SCAN_REPS=0`: none run, for counting the setup's instructions apart.)
    let only = std::env::var("SCAN_QUERY").ok().filter(|q| !q.is_empty());
    let made = |t: &str| only.as_ref().is_none_or(|o| o.contains(&format!("FROM {t}")));
    if made("n") {
        run(&mut s, "CREATE TABLE n (a int, b int, c int)");
        run(&mut s, &format!("INSERT INTO n SELECT i, i % 100, i % 7 FROM generate_series(1, {rows}) i"));
    }
    if made("w") {
        run(
        &mut s,
        &format!(
            "CREATE TABLE w AS SELECT i AS id, i % 1000 AS a, i::bigint * 7 AS b, (i % 10000) / 100.0 AS c, \
             'name-' || i AS d, 'x' || (i % 50) AS e, date '2020-01-01' + (i % 1000) AS f, i / 3.0::float8 AS g, \
             i % 13 AS h, repeat('z', 40 + i % 20) AS i FROM generate_series(1, {}) i",
            rows / 2
        ),
        );
    }
    // (Modelled, under 8 MB: the inline scan's size.)
    if made("m") {
        run(&mut s, "CREATE TABLE m (a int, b int, c int)");
        run(&mut s, "INSERT INTO m SELECT i, i % 100, i % 7 FROM generate_series(1, 150000) i");
    }
    let queries: &[(&str, usize)] = &[
        ("SELECT count(*) FROM n", rows),
        ("SELECT sum(b) FROM n", rows),
        ("SELECT count(*) FROM n WHERE b = 7", rows),
        ("SELECT count(*) FROM w", rows / 2),
        ("SELECT sum(h) FROM w", rows / 2),
        ("SELECT count(*) FROM w WHERE e = 'x7'", rows / 2),
        ("SELECT count(*) FROM m WHERE b = 7", 150_000),
        ("SELECT sum(b) FROM m", 150_000),
    ];
    for state in ["fresh", "compacted"] {
        if state == "compacted" {
            store.flush_memtables().unwrap();
            store.compact().unwrap();
        }
        if !wanted(state) {
            continue;
        }
        // Serial, then as planned (workers, or the inline scan): this thread's CPU per row,
        // and the process's.
        let wset = std::env::var("SCAN_WORKERS").ok().and_then(|w| w.parse::<usize>().ok());
        for workers in [0, 2] {
            if wset.is_some_and(|w| w != workers) {
                continue;
            }
            run(&mut s, &format!("SET max_parallel_workers_per_gather = {workers}"));
            for (q, n) in queries {
                if only.as_ref().is_some_and(|o| *q != o.as_str()) || reps() == 0 {
                    continue;
                }
                let mut f = || {
                    run(&mut s, q);
                    *n
                };
                f();
                let (me, all) = per_row2(reps(), *n, &mut f);
                println!("sql {state:9} w{workers} {me:7.1} ns/row (all threads {all:7.1})  {q}");
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let what = args.get(1).map_or("store", String::as_str);
    let rows = args.get(2).and_then(|r| r.parse().ok()).unwrap_or(1_000_000);
    match what {
        "store" => store_bench(rows),
        "sql" => sql_bench(rows),
        _ => {
            store_bench(rows);
            sql_bench(rows);
        }
    }
}
