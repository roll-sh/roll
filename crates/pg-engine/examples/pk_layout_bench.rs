//! Compare the two layouts of a primary-keyed table: the default (rows under a hidden row
//! id in heap order, the primary key an index of its own) and `roll_pk_clustered` (rows
//! keyed by the primary key). Run with
//! `cargo run --release -p pg-engine --example pk_layout_bench [bulk rows] [ops]`.
#[path = "../tests/common/mod.rs"]
mod common;
use pg_engine::session::Session;
use std::time::Instant;

fn run(s: &mut Session, sql: &str) {
    s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

/// A key in 1..=n, spread (a fixed permutation-ish walk).
fn key(i: usize, n: usize) -> usize {
    (i.wrapping_mul(7919) % n) + 1
}

fn bench(clustered: bool, bulk: usize, ops: usize) -> Vec<(&'static str, f64)> {
    let dir = tempfile::tempdir().unwrap();
    let mut s = common::engine_session(common::open_store(dir.path()), "bench");
    let with = if clustered { " WITH (roll_pk_clustered = true)" } else { "" };
    run(&mut s, &format!("CREATE TABLE t (id int PRIMARY KEY, v text, n int){with}"));
    let mut out = Vec::new();
    let mut time = |name: &'static str, n: usize, f: &mut dyn FnMut()| {
        let t = Instant::now();
        f();
        let us = t.elapsed().as_secs_f64() * 1e6 / n as f64;
        out.push((name, us));
    };
    time("bulk insert (us/row)", bulk, &mut || {
        run(
            &mut s,
            &format!("INSERT INTO t SELECT g, 'value ' || g, g FROM generate_series(1, {bulk}) g"),
        )
    });
    // (BULK_ONLY: stop here, for profiling.)
    if std::env::var_os("BULK_ONLY").is_some() {
        drop(time);
        return out;
    }
    let simple_only = std::env::var_os("SIMPLE_ONLY").is_some();
    time("single insert (us/stmt)", ops, &mut || {
        for i in 0..ops {
            if simple_only {
                break;
            }
            let id = bulk + 1 + i;
            run(&mut s, &format!("INSERT INTO t VALUES ({id}, 'single {id}', {id})"));
        }
    });
    let n = bulk + ops;
    // Per-statement overhead: a constant query and a point lookup sent as simple queries.
    time("SELECT 1 (us/stmt)", ops, &mut || {
        for _ in 0..ops {
            run(&mut s, "SELECT 1");
        }
    });
    time("simple point select (us/stmt)", ops, &mut || {
        for i in 0..ops {
            run(&mut s, &format!("SELECT v FROM t WHERE id = {}", key(i, n)));
        }
    });
    // (SIMPLE_ONLY: stop here, for profiling per-statement overhead.)
    if simple_only {
        drop(time);
        return out;
    }
    run(&mut s, "PREPARE sel(int) AS SELECT v FROM t WHERE id = $1");
    run(&mut s, "PREPARE upd(int) AS UPDATE t SET n = n + 1 WHERE id = $1");
    run(&mut s, "PREPARE del(int) AS DELETE FROM t WHERE id = $1");
    time("point lookup (us/stmt)", ops, &mut || {
        for i in 0..ops {
            run(&mut s, &format!("EXECUTE sel({})", key(i, n)));
        }
    });
    time("update by pk (us/stmt)", ops, &mut || {
        for i in 0..ops {
            run(&mut s, &format!("EXECUTE upd({})", key(i + 1, n)));
        }
    });
    time("delete by pk (us/stmt)", ops, &mut || {
        for i in 0..ops {
            run(&mut s, &format!("EXECUTE del({})", key(i * 3 + 2, n)));
        }
    });
    let rows = n;
    time("seq scan count(*) (us/row)", rows * 5, &mut || {
        for _ in 0..5 {
            run(&mut s, "SELECT count(*) FROM t");
        }
    });
    out
}

fn main() {
    let mut args = std::env::args().skip(1);
    let bulk: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(50_000);
    let ops: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(5_000);
    let rounds: usize = std::env::var("ROUNDS").ok().and_then(|r| r.parse().ok()).unwrap_or(3);
    let mut best: Vec<(&str, f64, f64)> = Vec::new();
    // (ONLY=default or ONLY=clustered: run one layout, for profiling.)
    let only = std::env::var("ONLY").unwrap_or_default();
    for _ in 0..rounds {
        let d = if only == "clustered" { Vec::new() } else { bench(false, bulk, ops) };
        let c = if only == "default" { Vec::new() } else { bench(true, bulk, ops) };
        if d.is_empty() || c.is_empty() {
            for (name, v) in d.iter().chain(&c) {
                println!("{name:<28} {v:>12.2}");
            }
            continue;
        }
        for ((name, dv), (_, cv)) in d.into_iter().zip(c) {
            match best.iter_mut().find(|b| b.0 == name) {
                Some(b) => {
                    b.1 = b.1.min(dv);
                    b.2 = b.2.min(cv);
                }
                None => best.push((name, dv, cv)),
            }
        }
    }
    println!("{bulk} rows bulk, {ops} ops, best of {rounds}");
    println!("{:<28} {:>12} {:>12} {:>9}", "", "default", "clustered", "overhead");
    for (name, d, c) in best {
        println!("{name:<28} {d:>12.2} {c:>12.2} {:>8.0}%", (d / c - 1.0) * 100.0);
    }
}
