//! pgvector's hnsw index access method, natively: builds, inserts, scans (plain and
//! iterative), VACUUM's removal and repair, the rebuilds TRUNCATE and REINDEX make (and a
//! rolled back TRUNCATE), an index read back from its store after the database is opened
//! again, and inserts from two sessions at once.
//!
//! Results are checked against an exact search (a sequential scan): recall, ordering, and
//! that no deleted row comes back. (Equality with pgvector itself, order of ties included,
//! is checked against a reference server: see the verification notes of the port.)

mod common;

use common::{Event, engine_query, engine_session, open_store};
use pg_engine::session::Session;

/// The rows of a statement's result (an error panics).
fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Option<String>>> {
    let mut out = Vec::new();
    for e in engine_query(s, sql) {
        match e {
            Event::Result { rows, .. } => out.extend(rows),
            Event::Error(f) => panic!("{sql}: {}", f.message),
            Event::Notice(_) => {}
        }
    }
    out
}

/// The notices and errors of a statement (their messages).
fn messages(s: &mut Session, sql: &str) -> Vec<String> {
    engine_query(s, sql)
        .into_iter()
        .filter_map(|e| match e {
            Event::Notice(f) | Event::Error(f) => Some(f.message),
            Event::Result { .. } => None,
        })
        .collect()
}

fn exec(s: &mut Session, sql: &str) {
    rows(s, sql);
}

fn one(s: &mut Session, sql: &str) -> String {
    rows(s, sql)[0][0].clone().unwrap_or_default()
}

/// A deterministic vector of `dim` elements for row `i`.
fn vec_lit(i: u64, dim: usize) -> String {
    let mut x = i.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let v: Vec<String> = (0..dim)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            format!("{:.3}", (x % 2000) as f64 / 1000.0 - 1.0)
        })
        .collect();
    format!("[{}]", v.join(","))
}

fn fill(s: &mut Session, table: &str, from: u64, to: u64, dim: usize) {
    let vals: Vec<String> = (from..to).map(|i| format!("({i},'{}')", vec_lit(i, dim))).collect();
    for chunk in vals.chunks(500) {
        exec(s, &format!("INSERT INTO {table} (id, v) VALUES {}", chunk.join(",")));
    }
}

/// (id, distance) of the rows `ORDER BY v <op> q LIMIT k`, through the index or not.
fn knn(s: &mut Session, table: &str, op: &str, q: &str, k: usize, index: bool) -> Vec<(i64, f64)> {
    exec(s, &format!("SET enable_indexscan = {index}"));
    exec(s, &format!("SET enable_seqscan = {}", !index));
    let r = rows(s, &format!("SELECT id, v {op} '{q}' FROM {table} ORDER BY v {op} '{q}' LIMIT {k}"));
    exec(s, "RESET enable_indexscan");
    exec(s, "RESET enable_seqscan");
    r.into_iter()
        .map(|r| (r[0].as_deref().unwrap().parse().unwrap(), r[1].as_deref().unwrap().parse().unwrap()))
        .collect()
}

fn uses_index(s: &mut Session, table: &str, op: &str, q: &str) -> bool {
    exec(s, "SET enable_seqscan = off");
    let plan = rows(s, &format!("EXPLAIN (COSTS OFF) SELECT id FROM {table} ORDER BY v {op} '{q}' LIMIT 5"));
    exec(s, "RESET enable_seqscan");
    plan.iter().any(|r| r[0].as_deref().unwrap_or("").contains("Index Scan"))
}

fn sorted(d: &[(i64, f64)]) -> bool {
    d.windows(2).all(|w| w[0].1 <= w[1].1)
}

/// The share of the exact k nearest that the index returns, over `n` queries.
fn recall(s: &mut Session, table: &str, op: &str, dim: usize, k: usize, n: u64) -> f64 {
    let mut hit = 0;
    for i in 0..n {
        let q = vec_lit(900_000 + i, dim);
        let a = knn(s, table, op, &q, k, true);
        let b = knn(s, table, op, &q, k, false);
        let want: std::collections::HashSet<i64> = b.iter().map(|x| x.0).collect();
        hit += a.iter().filter(|x| want.contains(&x.0)).count();
    }
    hit as f64 / (k as u64 * n) as f64
}

fn session(dir: &std::path::Path) -> Session {
    let mut s = engine_session(open_store(dir), "b");
    exec(&mut s, "CREATE EXTENSION IF NOT EXISTS vector");
    s
}

#[test]
fn build_insert_scan() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    exec(&mut s, "CREATE TABLE t (id int, v vector(8))");
    fill(&mut s, "t", 0, 2000, 8);
    exec(&mut s, "INSERT INTO t VALUES (-1, NULL)");
    exec(&mut s, "CREATE INDEX t_v ON t USING hnsw (v vector_l2_ops) WITH (m = 8, ef_construction = 32)");
    let q = vec_lit(77_777, 8);
    assert!(uses_index(&mut s, "t", "<->", &q));
    // ef_search candidates, nearest first.
    let r = knn(&mut s, "t", "<->", &q, 1000, true);
    assert_eq!(r.len(), 40);
    assert!(sorted(&r));
    exec(&mut s, "SET hnsw.ef_search = 100");
    assert!(recall(&mut s, "t", "<->", 8, 10, 20) >= 0.95);
    // Inserts after the build are found.
    fill(&mut s, "t", 2000, 2500, 8);
    let q = vec_lit(2100, 8);
    assert_eq!(knn(&mut s, "t", "<->", &q, 1, true)[0].0, 2100);
    assert!(recall(&mut s, "t", "<->", 8, 10, 20) >= 0.95);
    // Other operators.
    for (ops, op) in [("vector_ip_ops", "<#>"), ("vector_cosine_ops", "<=>"), ("vector_l1_ops", "<+>")] {
        exec(&mut s, "DROP INDEX t_v");
        exec(&mut s, &format!("CREATE INDEX t_v ON t USING hnsw (v {ops})"));
        assert!(uses_index(&mut s, "t", op, &q));
        assert!(recall(&mut s, "t", op, 8, 10, 10) >= 0.9, "{ops}");
    }
    // The relation's size: a meta page and the element pages.
    assert!(one(&mut s, "SELECT pg_relation_size('t_v')").parse::<i64>().unwrap() >= 3 * 8192);
}

#[test]
fn iterative_scans() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    exec(&mut s, "CREATE TABLE t (id int, v vector(4))");
    fill(&mut s, "t", 0, 3000, 4);
    exec(&mut s, "CREATE INDEX ON t USING hnsw (v vector_l2_ops)");
    exec(&mut s, "SET enable_seqscan = off");
    exec(&mut s, "SET hnsw.ef_search = 10");
    let q = vec_lit(5, 4);
    let filtered = |s: &mut Session| rows(s, &format!("SELECT id FROM t WHERE id % 100 = 0 ORDER BY v <-> '{q}' LIMIT 20")).len();
    // Without iterative scans a filter leaves what is among ef_search candidates.
    assert!(filtered(&mut s) < 20);
    exec(&mut s, "SET hnsw.iterative_scan = relaxed_order");
    assert_eq!(filtered(&mut s), 20);
    exec(&mut s, "SET hnsw.iterative_scan = strict_order");
    let r = rows(&mut s, &format!("SELECT v <-> '{q}' FROM t ORDER BY v <-> '{q}' LIMIT 500"));
    let d: Vec<f64> = r.iter().map(|r| r[0].as_deref().unwrap().parse().unwrap()).collect();
    assert_eq!(d.len(), 500);
    assert!(d.windows(2).all(|w| w[0] <= w[1]));
    // max_scan_tuples bounds the search.
    exec(&mut s, "SET hnsw.max_scan_tuples = 50");
    assert!(rows(&mut s, &format!("SELECT id FROM t WHERE id % 1000 = 1 ORDER BY v <-> '{q}' LIMIT 3")).len() <= 3);
}

#[test]
fn vacuum_repairs_and_reuses() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    exec(&mut s, "CREATE TABLE t (id int, v vector(8))");
    fill(&mut s, "t", 0, 3000, 8);
    exec(&mut s, "CREATE INDEX t_v ON t USING hnsw (v vector_l2_ops) WITH (m = 8, ef_construction = 32)");
    exec(&mut s, "DELETE FROM t WHERE id % 2 = 0");
    exec(&mut s, "VACUUM t");
    exec(&mut s, "SET hnsw.ef_search = 100");
    for i in 0..20 {
        let q = vec_lit(i * 2, 8);
        let r = knn(&mut s, "t", "<->", &q, 10, true);
        assert_eq!(r.len(), 10);
        assert!(r.iter().all(|x| x.0 % 2 == 1));
    }
    assert!(recall(&mut s, "t", "<->", 8, 10, 20) >= 0.9);
    // New elements take the deleted ones' space.
    let size = one(&mut s, "SELECT pg_relation_size('t_v')");
    fill(&mut s, "t", 10_000, 11_000, 8);
    assert_eq!(one(&mut s, "SELECT pg_relation_size('t_v')"), size);
    assert_eq!(knn(&mut s, "t", "<->", &vec_lit(10_500, 8), 1, true)[0].0, 10_500);
    assert!(recall(&mut s, "t", "<->", 8, 10, 20) >= 0.9);
}

#[test]
fn truncate_reindex_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let q = vec_lit(123, 8);
    let before;
    {
        let mut s = session(dir.path());
        exec(&mut s, "CREATE TABLE t (id int, v vector(8))");
        fill(&mut s, "t", 0, 1000, 8);
        exec(&mut s, "CREATE INDEX t_v ON t USING hnsw (v vector_l2_ops)");
        before = knn(&mut s, "t", "<->", &q, 10, true);
        // A rolled back TRUNCATE leaves the index as it was.
        exec(&mut s, "BEGIN");
        exec(&mut s, "TRUNCATE t");
        assert!(knn(&mut s, "t", "<->", &q, 10, true).is_empty());
        exec(&mut s, "ROLLBACK");
        assert_eq!(knn(&mut s, "t", "<->", &q, 10, true), before);
        exec(&mut s, "REINDEX INDEX t_v");
        assert_eq!(knn(&mut s, "t", "<->", &q, 10, true).len(), 10);
        exec(&mut s, "TRUNCATE t");
        assert!(knn(&mut s, "t", "<->", &q, 10, true).is_empty());
        fill(&mut s, "t", 0, 1000, 8);
    }
    // The store opened again: the index read back from its records.
    let mut s = session(dir.path());
    let after = knn(&mut s, "t", "<->", &q, 10, true);
    assert_eq!(after.len(), 10);
    assert!(sorted(&after));
    assert_eq!(after[0].0, 123);
    fill(&mut s, "t", 1000, 1100, 8);
    assert_eq!(knn(&mut s, "t", "<->", &vec_lit(1050, 8), 1, true)[0].0, 1050);
}

#[test]
fn concurrent_inserts() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = engine_session(open_store(dir.path()), "b");
    exec(&mut s, "CREATE EXTENSION IF NOT EXISTS vector");
    exec(&mut s, "CREATE TABLE t (id int, v vector(8))");
    fill(&mut s, "t", 0, 200, 8);
    exec(&mut s, "CREATE INDEX t_v ON t USING hnsw (v vector_l2_ops)");
    let db = s.state.db.clone();
    let threads: Vec<_> = (0..2)
        .map(|k| {
            let db = db.clone();
            std::thread::spawn(move || {
                let mut s = session_of(db, 4243 + k);
                for i in 0..300u64 {
                    let id = 1000 + k as u64 * 1000 + i;
                    exec(&mut s, &format!("INSERT INTO t VALUES ({id}, '{}')", vec_lit(id, 8)));
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let count = |s: &mut Session| {
        exec(s, "SET hnsw.iterative_scan = relaxed_order");
        exec(s, "SET enable_seqscan = off");
        one(s, &format!("SELECT count(*) FROM (SELECT id FROM t ORDER BY v <-> '{}' LIMIT 10000) x", vec_lit(1, 8)))
    };
    assert_eq!(count(&mut s), "800");
    for id in [1000u64, 1299, 2000, 2299] {
        assert_eq!(knn(&mut s, "t", "<->", &vec_lit(id, 8), 1, true)[0].0, id as i64);
    }
    // What the sessions wrote reads back as the same index.
    drop(s);
    let mut s = session(dir.path());
    assert_eq!(count(&mut s), "800");
}

/// Another session of database `db` (as a server's connections share it).
fn session_of(db: std::sync::Arc<pg_engine::txn::Database>, pid: i32) -> Session {
    use pg_engine::session::state::{SessionState, SingleCluster};
    let locale = common::createdb_locale(common::GOLDEN_CREATEDB);
    let cluster = std::sync::Arc::new(SingleCluster { name: "b".into(), started: 0, locale, conf: Default::default() });
    Session::new(SessionState::new(db, cluster, "postgres", pid))
}

#[test]
fn updates_under_a_normalizing_class() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    exec(&mut s, "CREATE TABLE t (id int, v vector(4))");
    fill(&mut s, "t", 0, 300, 4);
    exec(&mut s, "CREATE INDEX ON t USING hnsw (v vector_cosine_ops)");
    // The entries hold normalized values; an updated row is found by its new value, once.
    exec(&mut s, "UPDATE t SET v = '[1,2,3,4]' WHERE id = 7");
    exec(&mut s, "UPDATE t SET v = '[2,4,6,8.5]' WHERE id = 8");
    assert_eq!(knn(&mut s, "t", "<=>", "[1,2,3,4]", 1, true)[0].0, 7);
    exec(&mut s, "SET hnsw.iterative_scan = relaxed_order");
    exec(&mut s, "SET enable_seqscan = off");
    let ids = rows(&mut s, "SELECT id FROM t ORDER BY v <=> '[1,0,0,0]' LIMIT 1000");
    assert_eq!(ids.len(), 300);
    let distinct: std::collections::HashSet<_> = ids.iter().map(|r| r[0].clone()).collect();
    assert_eq!(distinct.len(), 300);
    exec(&mut s, "RESET hnsw.iterative_scan");
    exec(&mut s, "VACUUM t");
    assert_eq!(knn(&mut s, "t", "<=>", "[2,4,6,8.5]", 1, true)[0].0, 8);
}

#[test]
fn errors() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    exec(&mut s, "CREATE TABLE t (v vector(3), u vector, w vector(2001), h halfvec(4001), b bit(64), vb varbit(8))");
    let err = |s: &mut Session, sql: &str| messages(s, sql).join("\n");
    assert_eq!(err(&mut s, "CREATE INDEX ON t USING hnsw (v vector_l2_ops) WITH (m = 16, ef_construction = 31)"), "ef_construction must be greater than or equal to 2 * m");
    assert_eq!(err(&mut s, "CREATE INDEX ON t USING hnsw (u vector_l2_ops)"), "column does not have dimensions");
    assert_eq!(err(&mut s, "CREATE INDEX ON t USING hnsw (w vector_l2_ops)"), "column cannot have more than 2000 dimensions for hnsw index");
    assert_eq!(err(&mut s, "CREATE INDEX ON t USING hnsw (h halfvec_l2_ops)"), "column cannot have more than 4000 dimensions for hnsw index");
    exec(&mut s, "CREATE INDEX ON t USING hnsw (b bit_hamming_ops)");
    exec(&mut s, "CREATE INDEX t_v ON t USING hnsw (v vector_l2_ops)");
    exec(&mut s, "INSERT INTO t (v) VALUES ('[1,2,3]')");
    // A low maintenance_work_mem: the graph goes to disk early, with a notice.
    exec(&mut s, "CREATE TABLE big (id int, v vector(8))");
    fill(&mut s, "big", 0, 4000, 8);
    exec(&mut s, "SET maintenance_work_mem = '1MB'");
    let n = messages(&mut s, "CREATE INDEX ON big USING hnsw (v vector_l2_ops)");
    assert!(n.iter().any(|m| m.starts_with("hnsw graph no longer fits into maintenance_work_mem after")), "{n:?}");
    exec(&mut s, "SET hnsw.ef_search = 100");
    assert!(recall(&mut s, "big", "<->", 8, 10, 10) >= 0.9);
}
