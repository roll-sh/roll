//! pgvector's approximate indexes against a reference, result for result: the same data, the
//! same index parameters and the same queries must return the same rows in the same order, in
//! one session per side (both draw index-build randomness from the session's
//! pg_global_prng_state, seeded with 42 at each build). Also prints recall against exact search.
//!
//! The reference must be PostgreSQL 18.6 with pgvector 0.8.7 built with `-DIVFFLAT_BENCH
//! -DHNSW_MEMORY` (its builds then seed 42 first; `tests/contrib/STATUS.md` has the image):
//!
//!     PGVECTOR_REF=127.0.0.1:51405 cargo test -p pg-engine --test pgvector_equality -- \
//!       --ignored --nocapture
//!
//! `PGVECTOR_ROWS` scales the tables (default 1: a few thousand rows each);
//! `PGVECTOR_SELF=1` runs the reference against itself (a second session, a second database):
//! what it doesn't repeat isn't the engine's to match; `PGVECTOR_CASES=a,b` picks cases.

mod common;

use common::pgclient::PgClient;
use common::{Event, engine_query, engine_session, open_store};
use pg_engine::session::Session;

/// One side: the engine in-process, or the reference over the wire.
enum Side {
    Engine(Box<Session>),
    Ref(PgClient),
}

impl Side {
    fn run(&mut self, sql: &str) -> Vec<Vec<Option<String>>> {
        let events = match self {
            Side::Engine(s) => engine_query(s, sql),
            Side::Ref(c) => c.simple_query(sql).expect("reference query"),
        };
        let mut rows = Vec::new();
        for e in events {
            match e {
                Event::Error(f) => panic!("{sql}: {f:?}"),
                Event::Result { rows: r, .. } => rows.extend(r),
                Event::Notice(_) => {}
            }
        }
        rows
    }

    fn ids(&mut self, sql: &str) -> Vec<String> {
        self.run(sql).into_iter().map(|r| r[0].clone().unwrap_or_default()).collect()
    }
}

/// xorshift64*: the test's data, the same on both sides (sent as text, which both parse to the
/// same floats).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Clustered values in [-1, 1) with 4 decimals (ties happen).
    fn coord(&mut self, center: f64) -> f64 {
        let u = (self.next() >> 11) as f64 / (1u64 << 53) as f64;
        ((center + (u - 0.5) * 0.6) * 1e4).round() / 1e4
    }
}

fn vec_text(rng: &mut Rng, dim: usize, cluster: u64) -> String {
    let items: Vec<String> = (0..dim)
        .map(|j| {
            let c = (((cluster * 31 + j as u64 * 17) % 7) as f64 - 3.0) / 4.0;
            format!("{}", rng.coord(c))
        })
        .collect();
    format!("[{}]", items.join(","))
}

fn bits_text(rng: &mut Rng, n: usize) -> String {
    (0..n).map(|_| if rng.next() >> 63 == 1 { '1' } else { '0' }).collect()
}

/// A scenario: a table of one indexed column, an index, statements run alike on both sides,
/// and the queries compared (each `{q}` replaced by a query value).
struct Case {
    name: &'static str,
    ty: &'static str,
    index: &'static str,
    ops: &'static [&'static str],
    settings: &'static [&'static str],
    /// Compared after the build only. hnsw breaks exact distance ties by comparing the C
    /// pointers of the elements, so with ties (jaccard over random bits) an insert's neighbor
    /// choice depends on where the backend's allocations landed, which differs with its
    /// session's history (the reference agrees with itself only for the same statements in
    /// the same order).
    build_only: bool,
}

fn value(rng: &mut Rng, ty: &str, i: u64) -> String {
    match ty {
        t if t.starts_with("bit(") => format!("'{}'", bits_text(rng, t[4..t.len() - 1].parse().unwrap())),
        t if t.starts_with("sparsevec(") => {
            let dim: usize = t[10..t.len() - 1].parse().unwrap();
            let mut items = Vec::new();
            for j in 1..=dim {
                if rng.next().is_multiple_of(3) {
                    items.push(format!("{j}:{}", rng.coord(0.2)));
                }
            }
            format!("'{{{}}}/{dim}'", items.join(","))
        }
        t => {
            let dim: usize = t[t.find('(').unwrap() + 1..t.len() - 1].parse().unwrap();
            format!("'{}'", vec_text(rng, dim, i % 9))
        }
    }
}

fn load(sides: &mut [Side], ty: &str, from: u64, to: u64, rng: &mut Rng) {
    let mut sql = String::new();
    for i in from..=to {
        if sql.is_empty() {
            sql.push_str("INSERT INTO t (id, v) VALUES ");
        } else {
            sql.push(',');
        }
        // (A few NULLs and duplicates.)
        let v = if i % 97 == 0 { "NULL".to_string() } else { value(rng, ty, i) };
        sql.push_str(&format!("({i}, {v})"));
        if (i - from) % 400 == 399 || i == to {
            for s in sides.iter_mut() {
                s.run(&sql);
            }
            sql.clear();
        }
    }
}

fn check(sides: &mut [Side], case: &Case, stage: &str, queries: &[String], recall: bool) -> usize {
    let mut same = 0;
    let mut hits = (0usize, 0usize);
    for q in queries {
        for op in case.ops {
            let sql = format!("SELECT id FROM t ORDER BY v {op} {q} LIMIT 10");
            let a = sides[0].ids(&sql);
            let b = sides[1].ids(&sql);
            assert_eq!(a, b, "{} {stage}: {sql}", case.name);
            same += 1;
            if recall {
                let exact = sides[1].ids(&format!("SET enable_indexscan = off; {sql}; "));
                sides[1].run("SET enable_indexscan = on");
                hits.0 += a.iter().filter(|x| exact.contains(x)).count();
                hits.1 += exact.len();
            }
        }
    }
    if recall && hits.1 > 0 {
        eprintln!("{} {stage}: recall@10 {:.3}", case.name, hits.0 as f64 / hits.1 as f64);
    }
    same
}

#[test]
#[ignore = "needs a pgvector reference built with -DIVFFLAT_BENCH -DHNSW_MEMORY (PGVECTOR_REF)"]
fn pgvector_equality() {
    // SAFETY: the test binary's only test, before any engine thread starts.
    unsafe { std::env::set_var("ROLL_VECTOR_BUILD_SEED", "42") };
    let addr = std::env::var("PGVECTOR_REF").unwrap_or_else(|_| "127.0.0.1:51405".into());
    let scale: u64 = std::env::var("PGVECTOR_ROWS").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    let cases = [
        Case { name: "ivfflat l2", ty: "vector(16)", index: "ivfflat (v vector_l2_ops) WITH (lists = 20)", ops: &["<->"], settings: &["SET ivfflat.probes = 3"], build_only: false },
        Case { name: "ivfflat cosine", ty: "vector(12)", index: "ivfflat (v vector_cosine_ops) WITH (lists = 10)", ops: &["<=>"], settings: &["SET ivfflat.probes = 2"], build_only: false },
        Case { name: "ivfflat ip halfvec", ty: "halfvec(10)", index: "ivfflat (v halfvec_ip_ops) WITH (lists = 10)", ops: &["<#>"], settings: &["SET ivfflat.probes = 2", "SET ivfflat.iterative_scan = relaxed_order"], build_only: false },
        Case { name: "ivfflat hamming", ty: "bit(48)", index: "ivfflat (v bit_hamming_ops) WITH (lists = 8)", ops: &["<~>"], settings: &[], build_only: false },
        Case { name: "hnsw l2", ty: "vector(16)", index: "hnsw (v vector_l2_ops) WITH (m = 8, ef_construction = 32)", ops: &["<->"], settings: &["SET hnsw.ef_search = 20"], build_only: false },
        Case { name: "hnsw cosine", ty: "vector(12)", index: "hnsw (v vector_cosine_ops)", ops: &["<=>"], settings: &["SET hnsw.ef_search = 30", "SET hnsw.iterative_scan = strict_order"], build_only: false },
        Case { name: "hnsw l1 halfvec", ty: "halfvec(10)", index: "hnsw (v halfvec_l1_ops) WITH (m = 6)", ops: &["<+>"], settings: &[], build_only: false },
        Case { name: "hnsw ip", ty: "vector(8)", index: "hnsw (v vector_ip_ops) WITH (m = 4, ef_construction = 16)", ops: &["<#>"], settings: &["SET hnsw.iterative_scan = relaxed_order"], build_only: false },
        Case { name: "hnsw sparsevec l2", ty: "sparsevec(30)", index: "hnsw (v sparsevec_l2_ops)", ops: &["<->"], settings: &[], build_only: false },
        Case { name: "hnsw jaccard", ty: "bit(64)", index: "hnsw (v bit_jaccard_ops) WITH (m = 8)", ops: &["<%>"], settings: &[], build_only: true },
    ];
    let mut total = 0;
    for case in &cases {
        let db = format!("a70_eq_{}", case.name.replace(' ', "_"));
        let dir = tempfile::tempdir().unwrap();
        let selfcheck = std::env::var("PGVECTOR_SELF").is_ok_and(|v| v == "1");
        if let Ok(only) = std::env::var("PGVECTOR_CASES")
            && !only.split(',').any(|c| c.trim() == case.name)
        {
            continue;
        }
        let mut admin = PgClient::connect(&addr, "postgres", "pw", "postgres").expect("connect to the reference");
        admin.simple_query(&format!("DROP DATABASE IF EXISTS {db}")).unwrap();
        admin.simple_query(&format!("CREATE DATABASE {db}")).unwrap();
        let reference = PgClient::connect(&addr, "postgres", "pw", &db).unwrap();
        let first = if selfcheck {
            admin.simple_query(&format!("DROP DATABASE IF EXISTS {db}_2")).unwrap();
            admin.simple_query(&format!("CREATE DATABASE {db}_2")).unwrap();
            Side::Ref(PgClient::connect(&addr, "postgres", "pw", &format!("{db}_2")).unwrap())
        } else {
            Side::Engine(Box::new(engine_session(open_store(dir.path()), &db)))
        };
        let mut sides = [first, Side::Ref(reference)];
        let setup = format!(
            "CREATE EXTENSION vector; SET max_parallel_maintenance_workers = 0; SET client_min_messages = warning; \
             CREATE TABLE t (id int PRIMARY KEY, v {}) WITH (autovacuum_enabled = off)",
            case.ty
        );
        for s in sides.iter_mut() {
            s.run(&setup);
            for st in case.settings {
                s.run(st);
            }
        }
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ case.name.len() as u64);
        let n = 3000 * scale;
        load(&mut sides, case.ty, 1, n, &mut rng);
        for s in sides.iter_mut() {
            s.run(&format!("CREATE INDEX t_v ON t USING {}", case.index));
        }
        let queries: Vec<String> = (0..12).map(|i| value(&mut rng, case.ty, 1000 + i)).collect();
        total += check(&mut sides, case, "build", &queries, true);
        if case.build_only {
            drop(sides);
            admin.simple_query(&format!("DROP DATABASE IF EXISTS {db}")).unwrap();
            admin.simple_query(&format!("DROP DATABASE IF EXISTS {db}_2")).unwrap();
            eprintln!("{}: identical (build)", case.name);
            continue;
        }
        load(&mut sides, case.ty, n + 1, n + n / 3, &mut rng);
        total += check(&mut sides, case, "insert", &queries, false);
        for s in sides.iter_mut() {
            s.run("DELETE FROM t WHERE id % 5 = 1");
            s.run("VACUUM t");
        }
        total += check(&mut sides, case, "vacuum", &queries, false);
        load(&mut sides, case.ty, n + n / 3 + 1, n + n / 2, &mut rng);
        total += check(&mut sides, case, "reinsert", &queries, true);
        // Filtered (iterative scans continue past the first candidates).
        for q in queries.iter().take(4) {
            let sql = format!("SELECT id FROM t WHERE id % 7 = 3 ORDER BY v {} {q} LIMIT 10", case.ops[0]);
            let a = sides[0].ids(&sql);
            let b = sides[1].ids(&sql);
            assert_eq!(a, b, "{} filtered: {sql}", case.name);
            total += 1;
        }
        drop(sides);
        admin.simple_query(&format!("DROP DATABASE IF EXISTS {db}")).unwrap();
        admin.simple_query(&format!("DROP DATABASE IF EXISTS {db}_2")).unwrap();
        eprintln!("{}: identical", case.name);
    }
    eprintln!("pgvector_equality: {total} queries, every one identical");
}
