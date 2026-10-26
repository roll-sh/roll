//! Storage paths for big tables, run at small sizes (`sizing::with_max_versions` makes a
//! few thousand rows "too big to model"): index-only scans answered from the index entries,
//! against the same queries answered from the rows.

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::exec::scan::ROWS_FROM_INDEX_ENTRIES;
use pg_engine::exec::sizing::with_max_versions;
use pg_engine::session::Session;
use std::sync::atomic::Ordering;
use std::time::Duration;

fn open(dir: &std::path::Path) -> std::sync::Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: dir.to_path_buf(),
        cache_bytes: 4 << 20,
        memtable_bytes: 256 << 10,
        sync: false,
        idle_close: Duration::from_secs(60),
        superuser: "postgres".into(),
        password: None,
        server_files: None,
        server_programs: false,
        shared_preload_libraries: Vec::new(),
        locale: Default::default(),
        outbound: Vec::new(),
        max_prepared_transactions: 0,
    })
    .unwrap()
}

fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    r.last()
        .unwrap()
        .rows
        .iter()
        .map(|row| format!("{row:?}"))
        .collect()
}

fn run(s: &mut Session, sql: &str) {
    for stmt in sql.split(";\n").map(str::trim).filter(|q| !q.is_empty()) {
        s.query(stmt).unwrap_or_else(|e| panic!("{stmt}: {e:?}"));
    }
}

/// Each query by index-only scan (checked in its plan) and by sequential scan: the same
/// rows.
fn same_both_ways(s: &mut Session, queries: &[&str]) {
    for q in queries {
        run(s, "SET enable_seqscan = off;\nSET enable_bitmapscan = off;\nSET enable_indexonlyscan = on");
        let plan = rows(s, &format!("EXPLAIN (COSTS OFF) {q}")).join("\n");
        assert!(plan.contains("Index Only Scan"), "{q}: {plan}");
        let made = ROWS_FROM_INDEX_ENTRIES.load(Ordering::Relaxed);
        let by_index = rows(s, q);
        assert!(ROWS_FROM_INDEX_ENTRIES.load(Ordering::Relaxed) > made, "{q}: rows were read");
        run(s, "RESET enable_seqscan;\nRESET enable_bitmapscan;\nSET enable_indexonlyscan = off;\nSET enable_indexscan = off");
        let by_rows = rows(s, q);
        run(s, "RESET enable_indexonlyscan;\nRESET enable_indexscan");
        assert_eq!(by_index, by_rows, "{q}");
    }
}

#[test]
fn index_only_scans_from_entries() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    with_max_versions(500, || {
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        let mut other = eng.connect(2, "postgres", "postgres", "t").unwrap();
        run(
            &mut s,
            "CREATE TABLE big (id int NOT NULL, a int, t text, d date, pad text);
             INSERT INTO big SELECT i, i % 97, 'k' || (i % 13), DATE '2020-01-01' + i % 400, repeat('x', 30) FROM generate_series(1, 3000) i;
             CREATE INDEX big_a_t ON big (a, t DESC NULLS LAST);
             CREATE INDEX big_d ON big (d);
             UPDATE big SET a = NULL WHERE id % 50 = 0;
             UPDATE big SET t = NULL WHERE id % 70 = 0;
             UPDATE big SET pad = 'y' WHERE id % 3 = 0;
             DELETE FROM big WHERE id % 11 = 0;
             VACUUM ANALYZE big",
        );
        let queries = [
            "SELECT count(*) FROM big",
            "SELECT a, t FROM big WHERE a BETWEEN 10 AND 20 ORDER BY a, t DESC",
            "SELECT a, count(*) FROM big WHERE a IS NULL OR a < 5 GROUP BY a ORDER BY a",
            "SELECT t FROM big WHERE a = 42 ORDER BY t DESC NULLS LAST",
            "SELECT d, count(*) FROM big WHERE d >= DATE '2020-06-01' GROUP BY d ORDER BY d",
        ];
        same_both_ways(&mut s, &queries);
        // Uncommitted writes: the writer's own show, another session's don't.
        run(
            &mut s,
            "BEGIN;
             INSERT INTO big SELECT i, 7, 'new', DATE '2021-01-01', '' FROM generate_series(5001, 5100) i;
             UPDATE big SET a = 8 WHERE a = 9;
             DELETE FROM big WHERE a = 10",
        );
        same_both_ways(&mut s, &queries);
        same_both_ways(&mut other, &queries);
        run(&mut s, "COMMIT");
        same_both_ways(&mut other, &queries);
        // A cursor's snapshot sees the rows as they were when it opened.
        run(&mut s, "BEGIN;\nSET LOCAL enable_seqscan = off;\nSET LOCAL enable_bitmapscan = off;\nDECLARE c CURSOR FOR SELECT a FROM big WHERE a BETWEEN 7 AND 8 ORDER BY a");
        let before = rows(&mut other, "SELECT a FROM big WHERE a BETWEEN 7 AND 8 ORDER BY a");
        run(&mut s, "DELETE FROM big WHERE a = 7");
        let all = rows(&mut s, "FETCH ALL FROM c");
        assert_eq!(all, before);
        run(&mut s, "ROLLBACK");
    });
}

/// A table a COPY makes too big to model stops being modelled when the load goes in, not at
/// the next restart: its index-only scans are answered from the entries straight away.
#[test]
fn copy_past_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    with_max_versions(500, || {
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        run(&mut s, "CREATE TABLE cp (id int NOT NULL, a int);\nCREATE INDEX cp_a ON cp (a)");
        let data: String = (1..=3000).map(|i| format!("{i}\t{}\n", i % 37)).collect();
        s.query_copy("COPY cp FROM STDIN", data.as_bytes()).unwrap();
        same_both_ways(
            &mut s,
            &["SELECT count(*) FROM cp", "SELECT a, count(*) FROM cp WHERE a < 9 GROUP BY a ORDER BY a"],
        );
    });
}

/// What bulk statements leave in the heap model when their rows past the first few are
/// modelled at the end (`sizing::with_defer_after`): the same TIDs and sizes as modelled row
/// by row, for COPY and INSERT … SELECT, a failed COPY, and loads past the model's size
/// (whose sizes are estimates either way).
#[test]
fn deferred_model_matches() {
    let script = |s: &mut Session| -> Vec<String> {
        run(s, "CREATE TABLE src (id int, a int, b text);\nCREATE TABLE dc (id int NOT NULL, a int, b text);\nCREATE INDEX dc_a ON dc (a);\nCREATE TABLE di (id int, a int, b text);\nCREATE UNIQUE INDEX di_id ON di (id);\nCREATE TABLE dbig (id int, a int)");
        run(s, "INSERT INTO src SELECT g, g % 37, repeat('x', g % 50) FROM generate_series(1, 3000) g");
        let data: String = (1..=3000).map(|i| format!("{i}\t{}\tv{}\n", i % 37, "y".repeat(i % 40))).collect();
        s.query_copy("COPY dc FROM STDIN", data.as_bytes()).unwrap();
        run(s, "INSERT INTO di SELECT * FROM src;\nDELETE FROM di WHERE id % 3 = 0;\nVACUUM di;\nINSERT INTO di SELECT id + 5000, a, b FROM src");
        // A COPY failing past its first rows, in a savepoint.
        run(s, "BEGIN;\nSAVEPOINT a");
        let dup: String = (1..=2500).map(|i| format!("{}\t1\tz\n", if i == 2400 { 7 } else { 10_000 + i })).collect();
        assert!(s.query_copy("COPY di FROM STDIN", dup.as_bytes()).is_err());
        run(s, "ROLLBACK TO a;\nCOMMIT");
        with_max_versions(2000, || {
            let big: String = (1..=5000).map(|i| format!("{i}\t{}\n", i % 11)).collect();
            s.query_copy("COPY dbig FROM STDIN", big.as_bytes()).unwrap();
            run(s, "INSERT INTO dbig SELECT g, g FROM generate_series(1, 3000) g");
        });
        run(s, "ANALYZE");
        let mut out = Vec::new();
        for q in [
            "SELECT ctid, id FROM dc ORDER BY id",
            "SELECT ctid, id FROM di ORDER BY id",
            "SELECT relname, relpages, reltuples FROM pg_class WHERE relname IN ('dc', 'dc_a', 'di', 'di_id', 'dbig') ORDER BY 1",
            "SELECT pg_relation_size('dc'), pg_relation_size('dc_a'), pg_relation_size('di'), pg_relation_size('di_id'), pg_relation_size('dbig')",
            "SELECT count(*), sum(id) FROM dbig",
        ] {
            out.extend(rows(s, q));
        }
        out
    };
    let mut results = Vec::new();
    for n in [usize::MAX, 0, 50] {
        let dir = tempfile::tempdir().unwrap();
        let eng = open(dir.path());
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        results.push(pg_engine::exec::sizing::with_defer_after(n, || script(&mut s)));
    }
    for (i, what) in [(1, "deferred from the first row"), (2, "deferred past 50 rows")] {
        let (a, b) = (&results[0], &results[i]);
        let first = a.iter().zip(b).position(|(x, y)| x != y);
        assert!(
            first.is_none() && a.len() == b.len(),
            "{what}: at {first:?} of {}/{}: {:?} against {:?}",
            a.len(),
            b.len(),
            first.map(|k| &a[k]),
            first.map(|k| &b[k])
        );
    }
}
