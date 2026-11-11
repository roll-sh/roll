//! pgvector's ivfflat index access method, natively: builds, inserts, scans, VACUUM, the
//! rebuilds TRUNCATE and REINDEX make (and a rolled back TRUNCATE), and an index read back
//! from its store after the database is opened again.
//!
//! Results are compared with what an exact search (a sequential scan) gives: a scan probing
//! every list returns every row, nearest first; one probing fewer returns rows of its lists
//! only, still nearest first. (Equality with pgvector itself, order of ties included, is
//! checked against a reference server: see the verification notes of the port.)

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

/// Every probe: the exact answer (distances; ties may come in another order).
fn assert_exact(s: &mut Session, table: &str, op: &str, q: &str, k: usize) {
    exec(s, "SET ivfflat.probes = 32768");
    let a = knn(s, table, op, q, k, true);
    let b = knn(s, table, op, q, k, false);
    exec(s, "RESET ivfflat.probes");
    let da: Vec<f64> = a.iter().map(|x| x.1).collect();
    let db: Vec<f64> = b.iter().map(|x| x.1).collect();
    assert_eq!(da, db, "{table} {op} {q}");
}

/// Run `f` on a thread with a session's stack (as the server gives each connection).
fn on_thread(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            pg_engine::stack::set_limit(60 << 20);
            f()
        })
        .unwrap()
        .join()
        .unwrap()
}

fn session(dir: &std::path::Path) -> Session {
    let mut s = engine_session(open_store(dir), "b");
    exec(&mut s, "CREATE EXTENSION IF NOT EXISTS vector");
    s
}

#[test]
fn build_insert_scan() {
    on_thread(build_insert_scan_body);
}

fn build_insert_scan_body() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    exec(&mut s, "CREATE TABLE t (id int, v vector(8))");
    fill(&mut s, "t", 0, 3000, 8);
    exec(&mut s, "INSERT INTO t VALUES (-1, NULL)");
    exec(&mut s, "CREATE INDEX t_v ON t USING ivfflat (v vector_l2_ops) WITH (lists = 20)");
    let q = vec_lit(77_777, 8);
    assert!(uses_index(&mut s, "t", "<->", &q));
    // One probe: one list's rows, nearest first.
    let one_list = knn(&mut s, "t", "<->", &q, 3000, true);
    assert!(!one_list.is_empty() && one_list.len() < 3000 && sorted(&one_list));
    assert_exact(&mut s, "t", "<->", &q, 3000);
    // Rows inserted after the build are found.
    fill(&mut s, "t", 3000, 3500, 8);
    exec(&mut s, "SET ivfflat.probes = 20");
    assert_eq!(knn(&mut s, "t", "<->", &q, 10000, true).len(), 3500);
    exec(&mut s, "RESET ivfflat.probes");
    assert_exact(&mut s, "t", "<->", &q, 50);
    // Inner product and cosine (zero vectors aren't in a cosine index).
    exec(&mut s, "INSERT INTO t VALUES (-2, '[0,0,0,0,0,0,0,0]')");
    exec(&mut s, "CREATE INDEX t_ip ON t USING ivfflat (v vector_ip_ops) WITH (lists = 7)");
    assert_exact(&mut s, "t", "<#>", &q, 100);
    exec(&mut s, "DROP INDEX t_ip");
    exec(&mut s, "CREATE INDEX t_cos ON t USING ivfflat (v vector_cosine_ops) WITH (lists = 7)");
    exec(&mut s, "SET ivfflat.probes = 7");
    assert_eq!(knn(&mut s, "t", "<=>", &q, 10000, true).len(), 3500);
    // Iterative scans go on past the first lists.
    exec(&mut s, "SET ivfflat.probes = 1");
    let first = knn(&mut s, "t", "<=>", &q, 10000, true).len();
    exec(&mut s, "SET ivfflat.iterative_scan = relaxed_order");
    assert_eq!(knn(&mut s, "t", "<=>", &q, 10000, true).len(), 3500);
    exec(&mut s, "SET ivfflat.max_probes = 2");
    let two = knn(&mut s, "t", "<=>", &q, 10000, true).len();
    assert!(first < two && two < 3500);
    exec(&mut s, "RESET ivfflat.iterative_scan");
    exec(&mut s, "RESET ivfflat.max_probes");
    exec(&mut s, "RESET ivfflat.probes");
    // A NULL query returns the rows of a list.
    exec(&mut s, "SET enable_seqscan = off");
    let n: usize = one(&mut s, "SELECT count(*) FROM (SELECT id FROM t ORDER BY v <=> (SELECT NULL::vector)) x").parse().unwrap();
    assert!(n > 0 && n < 3500);
}

#[test]
fn errors_and_notices() {
    on_thread(errors_and_notices_body);
}

fn errors_and_notices_body() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    exec(&mut s, "CREATE TABLE t (id int, v vector(3), u vector)");
    assert_eq!(
        messages(&mut s, "CREATE INDEX ON t USING ivfflat (v vector_l2_ops)"),
        ["ivfflat index created with little data"]
    );
    assert_eq!(messages(&mut s, "CREATE INDEX ON t USING ivfflat (u vector_l2_ops)"), ["column does not have dimensions"]);
    exec(&mut s, "CREATE TABLE w (v vector(1))");
    assert_eq!(
        messages(&mut s, "CREATE INDEX ON w USING ivfflat (v vector_cosine_ops) WITH (lists = 1)"),
        ["dimensions must be greater than one for this opclass"]
    );
    exec(&mut s, "SET enable_seqscan = off");
    assert_eq!(
        messages(&mut s, "SELECT id FROM t ORDER BY v <-> '[1,2]' LIMIT 1"),
        ["expected 3 dimensions, not 2"]
    );
}

fn size(s: &mut Session, index: &str) -> i64 {
    one(s, &format!("SELECT pg_relation_size('{index}') / 8192")).parse().unwrap()
}

#[test]
fn vacuum_frees_space_for_inserts() {
    on_thread(vacuum_frees_space_for_inserts_body);
}

fn vacuum_frees_space_for_inserts_body() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    exec(&mut s, "CREATE TABLE t (id int, v vector(16))");
    fill(&mut s, "t", 0, 4000, 16);
    exec(&mut s, "CREATE INDEX t_v ON t USING ivfflat (v vector_l2_ops) WITH (lists = 4)");
    // A meta page, a list page and each list's pages (16-dim tuples: 80 bytes, 8192-byte
    // pages: at most 97 a page).
    let built = size(&mut s, "t_v");
    assert!(built >= 2 + 4000 / 97, "{built}");
    exec(&mut s, "DELETE FROM t WHERE id % 2 = 0");
    exec(&mut s, "VACUUM t");
    assert_eq!(size(&mut s, "t_v"), built);
    let q = vec_lit(5, 16);
    exec(&mut s, "SET ivfflat.probes = 4");
    let left = knn(&mut s, "t", "<->", &q, 10000, true);
    assert_eq!(left.len(), 2000);
    assert!(left.iter().all(|r| r.0 % 2 == 1) && sorted(&left));
    // Inserts go to the freed space: no new pages.
    fill(&mut s, "t", 10000, 11000, 16);
    assert_eq!(size(&mut s, "t_v"), built);
    assert_eq!(knn(&mut s, "t", "<->", &q, 10000, true).len(), 3000);
    // Past it, lists grow.
    fill(&mut s, "t", 20000, 22000, 16);
    assert!(size(&mut s, "t_v") > built);
    assert_exact(&mut s, "t", "<->", &q, 200);
}

#[test]
fn truncate_reindex_and_rollback() {
    on_thread(truncate_reindex_and_rollback_body);
}

fn truncate_reindex_and_rollback_body() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    exec(&mut s, "CREATE TABLE t (id int, v vector(4))");
    fill(&mut s, "t", 0, 500, 4);
    exec(&mut s, "CREATE INDEX t_v ON t USING ivfflat (v vector_l2_ops) WITH (lists = 3)");
    let q = vec_lit(9, 4);
    exec(&mut s, "SET ivfflat.probes = 3");
    assert_eq!(knn(&mut s, "t", "<->", &q, 1000, true).len(), 500);
    // A rolled back TRUNCATE leaves the index as it was.
    exec(&mut s, "BEGIN");
    assert_eq!(messages(&mut s, "TRUNCATE t"), ["ivfflat index created with little data"]);
    assert_eq!(knn(&mut s, "t", "<->", &q, 1000, true).len(), 0);
    exec(&mut s, "ROLLBACK");
    assert_eq!(knn(&mut s, "t", "<->", &q, 1000, true).len(), 500);
    fill(&mut s, "t", 500, 600, 4);
    assert_eq!(knn(&mut s, "t", "<->", &q, 1000, true).len(), 600);
    // REINDEX builds it again over the rows.
    exec(&mut s, "REINDEX INDEX t_v");
    assert_eq!(knn(&mut s, "t", "<->", &q, 1000, true).len(), 600);
    assert_exact(&mut s, "t", "<->", &q, 600);
    // A committed TRUNCATE empties it; later rows go in.
    exec(&mut s, "SET ivfflat.probes = 3");
    exec(&mut s, "TRUNCATE t");
    assert_eq!(knn(&mut s, "t", "<->", &q, 1000, true).len(), 0);
    fill(&mut s, "t", 0, 50, 4);
    assert_eq!(knn(&mut s, "t", "<->", &q, 1000, true).len(), 50);
}

#[test]
fn read_back_after_reopen() {
    on_thread(read_back_after_reopen_body);
}

fn read_back_after_reopen_body() {
    let dir = tempfile::tempdir().unwrap();
    let q = vec_lit(3, 12);
    let (before, built) = {
        let mut s = session(dir.path());
        exec(&mut s, "CREATE TABLE t (id int, v vector(12))");
        exec(&mut s, "CREATE TABLE b (id int, v bit(20))");
        fill(&mut s, "t", 0, 1500, 12);
        exec(&mut s, "INSERT INTO b SELECT i, (i * 7919 % 1048576)::bit(20) FROM generate_series(1, 800) i");
        exec(&mut s, "CREATE INDEX t_v ON t USING ivfflat (v vector_cosine_ops) WITH (lists = 6)");
        exec(&mut s, "CREATE INDEX b_v ON b USING ivfflat (v bit_hamming_ops) WITH (lists = 4)");
        fill(&mut s, "t", 1500, 1700, 12);
        exec(&mut s, "DELETE FROM t WHERE id < 100");
        exec(&mut s, "VACUUM t");
        exec(&mut s, "SET ivfflat.probes = 2");
        let mut r = knn(&mut s, "t", "<=>", &q, 2000, true);
        r.extend(knn(&mut s, "b", "<~>", "10101010101010101010", 2000, true));
        (r, size(&mut s, "t_v"))
    };
    let mut s = session(dir.path());
    exec(&mut s, "SET ivfflat.probes = 2");
    let mut after = knn(&mut s, "t", "<=>", &q, 2000, true);
    after.extend(knn(&mut s, "b", "<~>", "10101010101010101010", 2000, true));
    assert_eq!(before, after);
    assert_eq!(size(&mut s, "t_v"), built);
    // Bit strings: ties of the hamming distance come in the index's order.
    assert!(sorted(&after[after.len() - 10..]));
}
