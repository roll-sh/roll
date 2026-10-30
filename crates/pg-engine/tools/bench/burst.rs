//! Memory under concurrent load (BENCH.md "Memory under load"): many sessions running a mixed
//! workload at once (agent 111's mix: a PL/pgSQL loop, a SQL function over a FROM, an
//! ORM-style four-table join, psql's and information_schema's column queries, pgbench's
//! TPC-B), with the server's memory sampled through the burst and after it.
//! `burst-load` makes the tables (`--scale`, pgbench's); `burst` runs `--sessions` (a list,
//! e.g. `10,50,100,200`) for `--secs`, then reads the server's memory `--after` seconds
//! later with the sessions idle, and again after they close.
//! `--mix` names the operations run (default `pl,fn,orm,cat,tpcb`; also `sort`, `agg`);
//! `--reconnect N` opens a new connection every N operations (1: pgbench's `-C`).

use super::{Conn, Ctx, MemProbe, Res, mb, must, run_script, timed};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const LOAD: &str = "
DROP TABLE IF EXISTS pgbench_accounts, pgbench_branches, pgbench_tellers, pgbench_history,
  b_customer, b_orders, b_item, b_product CASCADE;
CREATE TABLE pgbench_branches (bid int PRIMARY KEY, bbalance int, filler char(88));
CREATE TABLE pgbench_tellers (tid int PRIMARY KEY, bid int, tbalance int, filler char(84));
CREATE TABLE pgbench_accounts (aid int PRIMARY KEY, bid int, abalance int, filler char(84));
CREATE TABLE pgbench_history (tid int, bid int, aid int, delta int, mtime timestamp, filler char(22));
INSERT INTO pgbench_branches SELECT g, 0, '' FROM generate_series(1, $S) g;
INSERT INTO pgbench_tellers SELECT g, (g - 1) / 10 + 1, 0, '' FROM generate_series(1, 10 * $S) g;
INSERT INTO pgbench_accounts SELECT g, (g - 1) / 100000 + 1, 0, '' FROM generate_series(1, 100000 * $S) g;
CREATE TABLE b_customer (id int PRIMARY KEY, name text, email text, phone text, city text, country text,
  created timestamptz, status int, score numeric);
CREATE TABLE b_orders (id int PRIMARY KEY, customer_id int NOT NULL, placed timestamptz, status text, total numeric,
  currency text, note text, shipped bool, ship_city text);
CREATE TABLE b_product (id int PRIMARY KEY, sku text, name text, descr text, price numeric, weight float8,
  category text, active bool, stock int);
CREATE TABLE b_item (id int PRIMARY KEY, order_id int NOT NULL, product_id int NOT NULL, qty int, price numeric,
  discount numeric, tax numeric, note text, line int);
INSERT INTO b_customer SELECT g, 'name ' || g, 'c' || g || '@example.com', '+1-555-' || g, 'city ' || g % 100,
  'country ' || g % 20, timestamptz '2025-01-01' + g * interval '1 min', g % 5, (g % 1000) / 10.0 FROM generate_series(1, 10000) g;
INSERT INTO b_orders SELECT g, (g * 7919) % 10000 + 1, timestamptz '2025-06-01' + g * interval '1 min', 'st' || g % 4,
  (g % 5000) / 3.0, 'EUR', 'order note ' || g, g % 2 = 0, 'city ' || g % 77 FROM generate_series(1, 50000) g;
INSERT INTO b_product SELECT g, 'SKU-' || g, 'product ' || g, repeat('d', 40 + g % 60), (g % 900) / 7.0, g / 13.0,
  'cat ' || g % 30, g % 9 <> 0, g % 500 FROM generate_series(1, 2000) g;
INSERT INTO b_item SELECT g, (g - 1) / 4 + 1, (g * 31) % 2000 + 1, g % 7 + 1, (g % 300) / 3.0, 0.05, 0.2,
  'item ' || g, (g - 1) % 4 + 1 FROM generate_series(1, 200000) g;
CREATE INDEX b_orders_customer ON b_orders (customer_id);
CREATE INDEX b_item_order ON b_item (order_id);
CREATE OR REPLACE FUNCTION burst_pl(start int) RETURNS int LANGUAGE plpgsql AS $$
DECLARE v int; s int := 0;
BEGIN
  FOR i IN start .. start + 9 LOOP
    SELECT abalance INTO v FROM pgbench_accounts WHERE aid = i;
    s := s + coalesce(v, 0);
  END LOOP;
  RETURN s;
END $$;
CREATE OR REPLACE FUNCTION burst_fa(a int) RETURNS int LANGUAGE sql STABLE AS
  'SELECT abalance + 1 FROM pgbench_accounts WHERE aid = a';
VACUUM ANALYZE pgbench_accounts;
VACUUM ANALYZE pgbench_branches;
VACUUM ANALYZE pgbench_tellers;
ANALYZE b_customer;
ANALYZE b_orders;
ANALYZE b_product;
ANALYZE b_item";

const ORM: &str = "SELECT c.*, o.*, i.*, p.* FROM b_customer c JOIN b_orders o ON o.customer_id = c.id \
  JOIN b_item i ON i.order_id = o.id JOIN b_product p ON p.id = i.product_id WHERE c.id = ";

const CAT_D: &str = "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), \
  (SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid, true) FROM pg_catalog.pg_attrdef d \
   WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum AND a.atthasdef), a.attnotnull, \
  (SELECT c.collname FROM pg_catalog.pg_collation c, pg_catalog.pg_type t \
   WHERE c.oid = a.attcollation AND t.oid = a.atttypid AND a.attcollation <> t.typcollation) AS attcollation, \
  a.attidentity, a.attgenerated FROM pg_catalog.pg_attribute a \
  WHERE a.attrelid = 'b_orders'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum";

const CAT_IS: &str = "SELECT column_name, data_type, is_nullable, column_default FROM information_schema.columns \
  WHERE table_name = 'b_item' ORDER BY ordinal_position";

pub fn load(cx: &Ctx) {
    let scale: i64 = cx.opt("scale", 2);
    let mut c = cx.conn();
    let (_, secs) = timed(|| run_burst_load(c.as_mut(), scale));
    cx.report("burst-load", "load", &[("scale", scale.to_string()), ("secs", format!("{secs:.2}"))]);
}

fn run_burst_load(c: &mut dyn Conn, scale: i64) {
    // (Function bodies hold semicolons: those two statements go whole.)
    let sql = LOAD.replace("$S", &scale.to_string());
    let (head, rest) = sql.split_once("CREATE OR REPLACE FUNCTION burst_pl").unwrap();
    run_script(c, head);
    let (pl, tail) = rest.split_once("END $$;").unwrap();
    must(c.simple(&format!("CREATE OR REPLACE FUNCTION burst_pl{pl}END $$")), "burst_pl");
    let (fa, tail) = tail.split_once("aid = a';").unwrap();
    must(c.simple(&format!("{fa}aid = a'")), "burst_fa");
    run_script(c, tail);
}

/// One operation of the mix, on a connection whose prepared statements are `ids`.
fn op(c: &mut dyn Conn, ids: &[usize], kind: &str, rng: &mut super::tpch::Rng, accounts: i64, branches: i64) -> Res<()> {
    let aid = rng.range(1, accounts - 10);
    match kind {
        "pl" => c.exec(ids[0], &[aid]).map(drop),
        "fn" => c
            .simple(&format!("SELECT sum(burst_fa(g)) FROM generate_series({aid}, {} ) g", aid + 99))
            .map(drop),
        "orm" => c.simple(&format!("{ORM}{}", rng.range(1, 10000))).map(drop),
        "cat" => {
            c.simple(CAT_D)?;
            c.simple(CAT_IS).map(drop)
        }
        "tpcb" => {
            let (tid, bid, delta) = (rng.range(1, branches * 10), rng.range(1, branches), rng.range(-5000, 5000));
            c.simple("BEGIN")?;
            let r = (|| {
                c.exec(ids[1], &[delta, aid])?;
                c.exec(ids[2], &[aid])?;
                c.exec(ids[3], &[delta, tid])?;
                c.exec(ids[4], &[delta, bid])?;
                c.exec(ids[5], &[tid, bid, aid, delta])
            })();
            c.simple(if r.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
            r.map(drop)
        }
        // A sort of ~20,000 rows (about 2 MB, under the default work_mem) and a grouping of
        // every account: per-statement working memory.
        "sort" => c
            .simple(&format!(
                "SELECT aid, abalance FROM pgbench_accounts WHERE aid BETWEEN {aid} AND {} ORDER BY abalance DESC, filler, aid LIMIT 5",
                aid + 20000
            ))
            .map(drop),
        "agg" => c
            .simple("SELECT aid % 50000, count(*), sum(abalance) FROM pgbench_accounts GROUP BY 1 ORDER BY 3 DESC LIMIT 3")
            .map(drop),
        _ => Err(format!("unknown operation {kind}")),
    }
}

fn prep(c: &mut dyn Conn) -> Vec<usize> {
    vec![
        must(c.prepare("SELECT burst_pl($1)", 1), "prep pl"),
        must(c.prepare("UPDATE pgbench_accounts SET abalance = abalance + $1 WHERE aid = $2", 2), "prep 1"),
        must(c.prepare("SELECT abalance FROM pgbench_accounts WHERE aid = $1", 1), "prep 2"),
        must(c.prepare("UPDATE pgbench_tellers SET tbalance = tbalance + $1 WHERE tid = $2", 2), "prep 3"),
        must(c.prepare("UPDATE pgbench_branches SET bbalance = bbalance + $1 WHERE bid = $2", 2), "prep 4"),
        must(
            c.prepare("INSERT INTO pgbench_history (tid, bid, aid, delta, mtime) VALUES ($1, $2, $3, $4, CURRENT_TIMESTAMP)", 4),
            "prep 5",
        ),
    ]
}

pub fn run(cx: &Ctx) {
    let list: String = cx.opt("sessions", "10,50,100,200".to_string());
    let secs: f64 = cx.opt("secs", 20.0);
    let after: u64 = cx.opt("after", 60);
    let scale: i64 = cx.opt("scale", 2);
    let mix: String = cx.opt("mix", "pl,fn,orm,cat,tpcb".to_string());
    let kinds: Vec<&str> = mix.split(',').collect();
    for n in list.split(',').filter_map(|s| s.parse::<usize>().ok()) {
        burst(cx, n, secs, after, scale, &kinds);
    }
}

/// Samples the server's memory every 50 ms into `peak`, with a line per second on stderr.
fn sampler(p: &MemProbe, stop: &AtomicBool, peak: &AtomicU64) {
    let t0 = Instant::now();
    let mut last = u64::MAX;
    while !stop.load(Ordering::Relaxed) {
        let b = p.bytes();
        peak.fetch_max(b, Ordering::Relaxed);
        let s = t0.elapsed().as_secs();
        if s != last {
            last = s;
            eprintln!("burst t={s}s mem_mb={}", mb(b));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn burst(cx: &Ctx, n: usize, secs: f64, after: u64, scale: i64, kinds: &[&str]) {
    let reconnect: usize = cx.opt("reconnect", 0);
    let before = cx.mem.bytes();
    let bar = || std::sync::Barrier::new(n + 1);
    let (ready, go, done, close) = (bar(), bar(), bar(), bar());
    let stop = AtomicBool::new(false);
    let ops = AtomicU64::new(0);
    let errors = AtomicU64::new(0);
    let first_err = std::sync::Mutex::new(String::new());
    let (sstop, peak) = (AtomicBool::new(false), AtomicU64::new(0));
    let mut kv = Vec::new();
    std::thread::scope(|sc| {
        for c in 0..n {
            let (ready, go, done, close, stop, ops, errors, first_err) =
                (&ready, &go, &done, &close, &stop, &ops, &errors, &first_err);
            sc.spawn(move || {
                let mut conn = cx.conn();
                let mut ids = prep(conn.as_mut());
                let mut rng = super::tpch::Rng::new(125, c as u64 + 1);
                ready.wait();
                go.wait();
                let mut i = c;
                while !stop.load(Ordering::Relaxed) {
                    let k = kinds[i % kinds.len()];
                    i += 1;
                    if reconnect > 0 && i % reconnect == 0 {
                        conn = cx.conn();
                        ids = prep(conn.as_mut());
                    }
                    match op(conn.as_mut(), &ids, k, &mut rng, 100_000 * scale, scale) {
                        Ok(()) => drop(ops.fetch_add(1, Ordering::Relaxed)),
                        Err(e) => {
                            errors.fetch_add(1, Ordering::Relaxed);
                            let mut f = first_err.lock().unwrap();
                            if f.is_empty() {
                                *f = format!("{k}: {e}");
                            }
                        }
                    }
                }
                done.wait();
                // Idle and connected until the memory after the burst has been read.
                close.wait();
                drop(conn);
            });
        }
        ready.wait();
        let connected = cx.mem.bytes();
        let cpu0 = cx.mem.cpu_secs();
        sc.spawn(|| sampler(&cx.mem, &sstop, &peak));
        go.wait();
        let t0 = Instant::now();
        std::thread::sleep(Duration::from_secs_f64(secs));
        stop.store(true, Ordering::Relaxed);
        done.wait();
        let elapsed = t0.elapsed().as_secs_f64();
        let cpu = cx.mem.cpu_secs() - cpu0;
        let end = cx.mem.bytes();
        std::thread::sleep(Duration::from_secs(after));
        let idle = cx.mem.bytes();
        close.wait();
        sstop.store(true, Ordering::Relaxed);
        let nops = ops.load(Ordering::Relaxed).max(1);
        kv = vec![
            ("sessions", n.to_string()),
            ("ops_s", format!("{:.0}", nops as f64 / elapsed)),
            ("cpu_us_per_op", format!("{:.0}", cpu * 1e6 / nops as f64)),
            ("before_mb", mb(before)),
            ("connected_mb", mb(connected)),
            ("peak_mb", mb(peak.load(Ordering::Relaxed))),
            ("end_mb", mb(end)),
            ("after_mb", mb(idle)),
            ("after_s", after.to_string()),
            ("errors", errors.load(Ordering::Relaxed).to_string()),
        ];
    });
    std::thread::sleep(Duration::from_secs(5));
    kv.push(("closed_5s_mb", mb(cx.mem.bytes())));
    let f = first_err.into_inner().unwrap();
    if !f.is_empty() {
        kv.push(("first_error", f.replace(['\t', '\n'], " ")));
    }
    cx.report("burst", &kinds.join("+"), &kv);
}
