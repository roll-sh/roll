//! Memory under concurrent load (BENCH.md "Memory under load"), with a counting allocator:
//!
//! - the parse tree cache holds about the bytes it counts against its bound (it once counted
//!   a tenth of them: "32 MB" held 300 MB);
//! - many sessions starting a mixed workload at once on a database just opened (the
//!   tables' size models not yet built) stay within a bound of heap over what they held
//!   connected: each session used to build its own copy of a table's model, 100-400 MB
//!   per session at once.
//!
//! `cargo test -p pg-engine --test memory_load -- --nocapture` prints the numbers;
//! `MEMLOAD_SESSIONS=200 MEMLOAD_SECS=10` makes a bigger burst.

mod common;

use common::pgclient::PgClient;
use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::server::{AuthMethod, ServerOptions};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Barrier, Mutex};
use std::time::Duration;

struct Counting;

static CUR: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// What this thread allocated less what it freed (the engine's other threads don't
    /// count).
    static MINE: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

fn mine(d: isize) {
    let _ = MINE.try_with(|m| m.set(m.get() + d));
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let n = CUR.fetch_add(l.size(), Relaxed) + l.size();
            PEAK.fetch_max(n, Relaxed);
            mine(l.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        CUR.fetch_sub(l.size(), Relaxed);
        mine(-(l.size() as isize));
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            mine(new as isize - l.size() as isize);
            if new > l.size() {
                let n = CUR.fetch_add(new - l.size(), Relaxed) + new - l.size();
                PEAK.fetch_max(n, Relaxed);
            } else {
                CUR.fetch_sub(l.size() - new, Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static A: Counting = Counting;

const MB: f64 = 1048576.0;

/// One test at a time: both read the process's allocations.
static SERIAL: Mutex<()> = Mutex::new(());

fn parse_cache_bytes() -> usize {
    pg_engine::lru::footprints().iter().find(|(n, _)| *n == "parse trees").map_or(0, |(_, f)| f.bytes)
}

#[test]
fn parse_cache_holds_what_it_counts() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // Emptied, filled below its bound, then emptied again: what that frees on this thread
    // is what the cache held.
    pg_engine::lru::set_plan_cache_bytes(0);
    pg_engine::lru::set_plan_cache_bytes(pg_engine::lru::PLAN_CACHE_BYTES);
    for i in 0..600 {
        let texts = [
            format!("{ORM}{i}"),
            format!("SELECT sum(burst_fa(g)) FROM generate_series({i}, {}) g", i + 99),
            CAT_D.replace("'b_orders'", &format!("'t{i}'")),
        ];
        for t in &texts {
            pg_engine::sql::parse::parse(t).unwrap();
        }
    }
    let counted = parse_cache_bytes() as f64;
    let before = MINE.with(|m| m.get());
    pg_engine::lru::set_plan_cache_bytes(0);
    let held = (before - MINE.with(|m| m.get())) as f64;
    pg_engine::lru::set_plan_cache_bytes(pg_engine::lru::PLAN_CACHE_BYTES);
    eprintln!("parse cache: {:.1} MB held, {:.1} MB counted", held / MB, counted / MB);
    assert!(counted > (1 << 20) as f64, "nothing cached");
    let r = held / counted;
    assert!((0.7..=1.3).contains(&r), "the parse cache holds {r:.2}x the bytes it counts");
}

const SETUP: &str = "
CREATE TABLE pgbench_branches (bid int PRIMARY KEY, bbalance int, filler char(88));
CREATE TABLE pgbench_tellers (tid int PRIMARY KEY, bid int, tbalance int, filler char(84));
CREATE TABLE pgbench_accounts (aid int PRIMARY KEY, bid int, abalance int, filler char(84));
CREATE TABLE pgbench_history (tid int, bid int, aid int, delta int, mtime timestamp, filler char(22));
INSERT INTO pgbench_branches SELECT g, 0, '' FROM generate_series(1, 2) g;
INSERT INTO pgbench_tellers SELECT g, (g - 1) / 10 + 1, 0, '' FROM generate_series(1, 20) g;
INSERT INTO pgbench_accounts SELECT g, (g - 1) / 20000 + 1, 0, '' FROM generate_series(1, 40000) g;
CREATE TABLE b_customer (id int PRIMARY KEY, name text, city text);
CREATE TABLE b_orders (id int PRIMARY KEY, customer_id int NOT NULL, placed timestamptz, note text);
CREATE TABLE b_item (id int PRIMARY KEY, order_id int NOT NULL, qty int, price numeric);
INSERT INTO b_customer SELECT g, 'name ' || g, 'city ' || g % 100 FROM generate_series(1, 2000) g;
INSERT INTO b_orders SELECT g, (g * 7919) % 2000 + 1, timestamptz '2025-06-01' + g * interval '1 min', 'note ' || g
  FROM generate_series(1, 10000) g;
INSERT INTO b_item SELECT g, (g - 1) / 4 + 1, g % 7 + 1, (g % 300) / 3.0 FROM generate_series(1, 40000) g;
CREATE INDEX b_orders_customer ON b_orders (customer_id);
CREATE INDEX b_item_order ON b_item (order_id);
CREATE FUNCTION burst_pl(start int) RETURNS int LANGUAGE plpgsql AS $$
DECLARE v int; s int := 0;
BEGIN
  FOR i IN start .. start + 9 LOOP
    SELECT abalance INTO v FROM pgbench_accounts WHERE aid = i;
    s := s + coalesce(v, 0);
  END LOOP;
  RETURN s;
END $$;
CREATE FUNCTION burst_fa(a int) RETURNS int LANGUAGE sql STABLE AS
  'SELECT abalance + 1 FROM pgbench_accounts WHERE aid = a';
ANALYZE";

const ORM: &str = "SELECT c.*, o.*, i.* FROM b_customer c JOIN b_orders o ON o.customer_id = c.id \
  JOIN b_item i ON i.order_id = o.id WHERE c.id = ";

const CAT_D: &str = "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), \
  (SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid, true) FROM pg_catalog.pg_attrdef d \
   WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum AND a.atthasdef), a.attnotnull \
  FROM pg_catalog.pg_attribute a WHERE a.attrelid = 'b_orders'::regclass AND a.attnum > 0 \
  AND NOT a.attisdropped ORDER BY a.attnum";

fn options(dir: &std::path::Path) -> EngineOptions {
    EngineOptions {
        data_dir: dir.to_path_buf(),
        cache_bytes: 16 << 20,
        sync: false,
        ..EngineOptions::default()
    }
}

fn op(c: &mut PgClient, k: usize, i: u64) {
    let aid = (i * 7919) % 39_000 + 1;
    let sql = match k % 5 {
        0 => format!("SELECT burst_pl({aid})"),
        1 => format!("SELECT sum(burst_fa(g)) FROM generate_series({aid}, {}) g", aid + 99),
        2 => format!("{ORM}{}", i % 2000 + 1),
        3 => CAT_D.to_string(),
        _ => format!(
            "BEGIN; UPDATE pgbench_accounts SET abalance = abalance + 1 WHERE aid = {aid}; \
             SELECT abalance FROM pgbench_accounts WHERE aid = {aid}; \
             UPDATE pgbench_tellers SET tbalance = tbalance + 1 WHERE tid = {}; \
             UPDATE pgbench_branches SET bbalance = bbalance + 1 WHERE bid = {}; \
             INSERT INTO pgbench_history VALUES (1, 1, {aid}, 1, now(), ''); COMMIT",
            i % 20 + 1,
            i % 2 + 1
        ),
    };
    c.simple_query(&sql).unwrap();
}

#[test]
fn many_sessions_stay_bounded() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let n: usize = std::env::var("MEMLOAD_SESSIONS").ok().and_then(|v| v.parse().ok()).unwrap_or(100);
    let secs: f64 = std::env::var("MEMLOAD_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(3.0);
    let dir = tempfile::tempdir().unwrap();
    // (A database closes 0.3 s after its last session: the burst opens it again, when no
    // table has a size model yet.)
    let engine = Engine::open(EngineOptions { idle_close: Duration::from_millis(300), ..options(dir.path()) }).unwrap();
    let addr = pg_engine::server::spawn(engine.clone(), "127.0.0.1:0", ServerOptions { auth: AuthMethod::Trust })
        .unwrap()
        .to_string();
    let mut c = PgClient::connect(&addr, "postgres", "", "postgres").unwrap();
    c.simple_query("CREATE DATABASE memload").unwrap();
    let mut c = PgClient::connect(&addr, "postgres", "", "memload").unwrap();
    c.simple_query(SETUP).unwrap();
    drop(c);
    std::thread::sleep(Duration::from_millis(1500));
    let ready = Barrier::new(n + 1);
    let go = Barrier::new(n + 1);
    let stop = AtomicBool::new(false);
    let ops = AtomicUsize::new(0);
    let (connected, peak) = std::thread::scope(|sc| {
        for s in 0..n {
            let (addr, ready, go, stop, ops) = (&addr, &ready, &go, &stop, &ops);
            std::thread::Builder::new()
                .stack_size(1 << 20)
                .spawn_scoped(sc, move || {
                    let mut c = PgClient::connect(addr, "postgres", "", "memload").unwrap();
                    ready.wait();
                    go.wait();
                    let mut i = s as u64 * 1_000_003;
                    while !stop.load(Relaxed) {
                        op(&mut c, i as usize + s, i);
                        i += 1;
                        ops.fetch_add(1, Relaxed);
                    }
                })
                .unwrap();
        }
        ready.wait();
        let connected = CUR.load(Relaxed);
        PEAK.store(connected, Relaxed);
        go.wait();
        std::thread::sleep(Duration::from_secs_f64(secs));
        stop.store(true, Relaxed);
        (connected, PEAK.load(Relaxed))
    });
    let over = (peak - connected) as f64 / MB;
    eprintln!(
        "{n} sessions, {} operations: heap {:.1} MB connected, peak +{over:.1} MB",
        ops.load(Relaxed),
        connected as f64 / MB
    );
    // Measured at 100 sessions (round 22, release build): +93 MB; with every session building
    // its own models of the tables, +640 MB (and 120 operations instead of 3,570).
    let bound = 40.0 + 0.8 * n as f64;
    assert!(over <= bound, "{n} sessions peaked at +{over:.1} MB of heap (bound {bound:.0} MB)");
}
