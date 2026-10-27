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
        auth_files: Default::default(),
        settings: Vec::new(),
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

/// VACUUM, ANALYZE and CREATE INDEX measuring a table too big to model size most rows from
/// their stored bytes (`storage::row::stored_tuple_len`); debug builds check every such size
/// against the decoded row's. Here over each kind of value it reads or leaves to decoding:
/// nulls, booleans, numbers, dates, timestamps, ASCII and other text, varchar, char(n),
/// numeric, a virtual generated column and rows from before an added column.
#[test]
fn measured_sizes_from_stored_rows() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    with_max_versions(300, || {
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        run(&mut s, "CREATE TABLE m (i int, b bool, s smallint, l bigint, f real, d float8, dt date, ts timestamp, tz timestamptz, t text, v varchar(20), c char(5), n numeric, g int GENERATED ALWAYS AS (i * 2) VIRTUAL)");
        run(&mut s, "INSERT INTO m (i, b, s, l, f, d, dt, ts, tz, t, v, c, n) SELECT g, g % 2 = 0, g % 100, g * 1000, g / 3.0, g / 7.0, date '2000-01-01' + g, timestamp '2000-01-01' + g * interval '1 hour', timestamptz '2000-01-01 00:00+00' + g * interval '1 minute', CASE WHEN g % 5 = 0 THEN NULL WHEN g % 7 = 0 THEN 'é' || g ELSE repeat('x', g % 200) END, 'v' || g, 'c' || (g % 10), g / 3.0 FROM generate_series(1, 1500) g");
        run(&mut s, "ALTER TABLE m ADD COLUMN extra int DEFAULT 7");
        run(&mut s, "INSERT INTO m (i, t, extra) SELECT g, repeat('y', 300), g FROM generate_series(1501, 1700) g");
        run(&mut s, "CREATE INDEX m_i ON m (i);\nCREATE INDEX m_t ON m (t) INCLUDE (l);\nANALYZE m;\nVACUUM m");
        let pages = rows(&mut s, "SELECT relpages > 0, reltuples FROM pg_class WHERE relname = 'm'");
        assert_eq!(pages, ["[Bool(true), Float4(1700.0)]"]);
        // Only kinds sized from the stored bytes (and nulls), so every row goes that way.
        run(&mut s, "CREATE TABLE m2 (i int, b bool, s smallint, l bigint, f real, d float8, dt date, ts timestamp, tz timestamptz, t text, v varchar(20), g int GENERATED ALWAYS AS (i * 2) VIRTUAL)");
        run(&mut s, "INSERT INTO m2 SELECT g, g % 2 = 0, g % 100, g * 1000, g / 3.0, g / 7.0, date '2000-01-01' + g, timestamp '2000-01-01' + g * interval '1 hour', timestamptz '2000-01-01 00:00+00' + g * interval '1 minute', CASE WHEN g % 5 = 0 THEN NULL ELSE repeat('x', g % 300) END, CASE WHEN g % 3 = 0 THEN NULL ELSE 'v' || g END FROM generate_series(1, 1500) g");
        let sized = pg_engine::storage::row::STORED_SIZES.load(Ordering::Relaxed);
        run(&mut s, "CREATE INDEX m2_i ON m2 (i);\nCREATE UNIQUE INDEX m2_l ON m2 (l) INCLUDE (t);\nANALYZE m2;\nVACUUM m2");
        assert!(pg_engine::storage::row::STORED_SIZES.load(Ordering::Relaxed) >= sized + 4 * 1500, "sized from the stored bytes");
        let pages = rows(&mut s, "SELECT relpages > 0, reltuples FROM pg_class WHERE relname = 'm2'");
        assert_eq!(pages, ["[Bool(true), Float4(1500.0)]"]);
    });
}

/// The TOAST relation's RowExclusiveLock (toast_save_datum / toast_delete_datum) comes with
/// the same statements whether the table is modelled or too big to be: a delete or update
/// of a row with an out-of-line value takes it, one of a row without or keeping the value
/// doesn't.
#[test]
fn toast_locks_past_the_model() {
    let steps = [
        ("delete a small row", "DELETE FROM tt WHERE id = 1"),
        ("delete a row stored out of line", "DELETE FROM tt WHERE id = 1001"),
        ("update keeping the value", "UPDATE tt SET n = n + 1 WHERE id = 1002"),
        ("update replacing the value", "UPDATE tt SET v = 'short' WHERE id = 1003"),
        ("insert a value out of line", "INSERT INTO tt SELECT 2000, 0, string_agg(md5(g::text), '') FROM generate_series(1, 400) g"),
    ];
    let toast_locked = "SELECT count(*) FROM pg_locks WHERE pid = pg_backend_pid() AND mode = 'RowExclusiveLock' \
                        AND relation = (SELECT reltoastrelid FROM pg_class WHERE relname = 'tt')";
    let run_all = |big: bool| -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let eng = open(dir.path());
        let go = || {
            let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
            run(&mut s, "CREATE TABLE tt (id int PRIMARY KEY, n int, v text);\n\
                INSERT INTO tt SELECT g, 0, 'small ' || g FROM generate_series(1, 1000) g;\n\
                INSERT INTO tt SELECT 1000 + i, 0, string_agg(md5((i * 1000 + g)::text), '') FROM generate_series(1, 3) i, generate_series(1, 400) g GROUP BY i");
            steps
                .iter()
                .map(|(name, sql)| {
                    run(&mut s, &format!("BEGIN;\n{sql}"));
                    let n = rows(&mut s, toast_locked).join("");
                    run(&mut s, "ROLLBACK");
                    format!("{name}: {n}")
                })
                .collect()
        };
        if big { with_max_versions(500, go) } else { go() }
    };
    let modelled = run_all(false);
    assert_eq!(
        modelled,
        [
            "delete a small row: [Int8(0)]",
            "delete a row stored out of line: [Int8(1)]",
            "update keeping the value: [Int8(0)]",
            "update replacing the value: [Int8(1)]",
            "insert a value out of line: [Int8(1)]",
        ]
    );
    assert_eq!(run_all(true), modelled);
}

/// CREATE INDEX of a table too big to model measures the table and the index in its own
/// pass over the rows (`Ddl::measures_in_build`): the same pages and tuples as measuring
/// them apart (ANALYZE's pass, with nothing written between), for plain, expression,
/// partial, INCLUDE and unique indexes.
#[test]
fn index_build_measures_as_analyze() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    with_max_versions(400, || {
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        run(&mut s, "CREATE TABLE ib (id int NOT NULL, a int, t text, pad text);
             INSERT INTO ib SELECT g, g % 61, CASE WHEN g % 9 = 0 THEN NULL ELSE repeat('k', g % 40) END, repeat('p', g % 90) FROM generate_series(1, 4000) g");
        let indexes = [
            "CREATE INDEX ib_a ON ib (a)",
            "CREATE INDEX ib_t ON ib (t) INCLUDE (a)",
            "CREATE INDEX ib_e ON ib ((a * 2 + 1), lower(t))",
            "CREATE INDEX ib_p ON ib (id) WHERE a < 20",
            "CREATE UNIQUE INDEX ib_id ON ib (id DESC)",
        ];
        let stats = "SELECT relname, relpages, reltuples FROM pg_class WHERE relname LIKE 'ib%' ORDER BY relname";
        for ix in indexes {
            run(&mut s, ix);
            let built = rows(&mut s, stats);
            run(&mut s, "ANALYZE ib");
            assert_eq!(rows(&mut s, stats), built, "{ix}");
        }
        // A duplicate found by the build's pass still fails it.
        let e = s.query("CREATE UNIQUE INDEX ib_dup ON ib (a)").unwrap_err();
        assert!(format!("{e:?}").contains("is duplicated"), "{e:?}");
    });
}

/// A DELETE's index entries go in together at its end (`Txn::defer_entry_delete`): within
/// its transaction, index scans, index-only scans from the entries, unique keys inserted
/// again and savepoints see what deleting them one at a time gives; so do DELETEs of rows
/// their transaction wrote itself.
#[test]
fn deferred_entry_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    with_max_versions(500, || {
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        // (Parallel workers don't run under `with_max_versions`: a seq scan of this "big"
        // table in them counted 42 deleted rows, deferred or not.)
        run(&mut s, "SET max_parallel_workers_per_gather = 0");
        run(&mut s, "CREATE TABLE dd (id int PRIMARY KEY, a int, t text);
             CREATE INDEX dd_a ON dd (a);
             INSERT INTO dd SELECT g, g % 50, 'v' || g FROM generate_series(1, 3000) g;
             VACUUM ANALYZE dd");
        let queries = ["SELECT count(*) FROM dd", "SELECT a, count(*) FROM dd WHERE a < 10 GROUP BY a ORDER BY a"];
        run(&mut s, "BEGIN;\nDELETE FROM dd WHERE a % 3 = 0;\nINSERT INTO dd VALUES (3, 3, 'again'), (6, 6, 'again')");
        same_both_ways(&mut s, &queries);
        run(&mut s, "SET enable_seqscan = off;\nSET enable_bitmapscan = off");
        assert_eq!(rows(&mut s, "SELECT id, t FROM dd WHERE a = 6 ORDER BY id"), ["[Int4(6), Text(\"again\")]"]);
        run(&mut s, "SAVEPOINT p;\nDELETE FROM dd WHERE a = 1;\nROLLBACK TO p;\nDELETE FROM dd WHERE a = 2");
        assert_eq!(rows(&mut s, "SELECT count(*) FROM dd WHERE a IN (1, 2)"), ["[Int8(60)]"]);
        run(&mut s, "RESET enable_seqscan;\nRESET enable_bitmapscan;\nCOMMIT");
        same_both_ways(&mut s, &queries);
        run(&mut s, "BEGIN;\nINSERT INTO dd SELECT g, g % 50, 'n' FROM generate_series(10001, 12000) g;\nDELETE FROM dd WHERE id > 10000 AND a < 25;\nCOMMIT");
        same_both_ways(&mut s, &queries);
        let e = s.query("INSERT INTO dd VALUES (10030, 0, 'dup')").unwrap_err();
        assert!(format!("{e:?}").contains("duplicate key"), "{e:?}");
        run(&mut s, "INSERT INTO dd VALUES (10001, 0, 'ok')");
    });
}

/// A table too big to model that was never measured keeps its estimated size across a
/// restart (`sizing::persist_estimates`): its first plan after one needn't read it whole.
#[test]
fn estimate_kept_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let explain = "EXPLAIN SELECT * FROM est WHERE a = 1";
    let (before, size) = {
        let eng = open(dir.path());
        let r = with_max_versions(500, || {
            let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
            run(&mut s, "CREATE TABLE est (id int, a int, t text)");
            let data: String = (1..=3000).map(|i| format!("{i}\t{}\tsome text {i}\n", i % 37)).collect();
            s.query_copy("COPY est FROM STDIN", data.as_bytes()).unwrap();
            run(&mut s, "INSERT INTO est SELECT g, 1, 'more' FROM generate_series(1, 200) g");
            (rows(&mut s, explain), rows(&mut s, "SELECT pg_relation_size('est')"))
        });
        eng.shutdown();
        r
    };
    let eng = open(dir.path());
    with_max_versions(500, || {
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        assert_eq!(rows(&mut s, explain), before);
        assert_eq!(rows(&mut s, "SELECT pg_relation_size('est')"), size);
    });
}
