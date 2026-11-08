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

/// With workers off, an aggregation over a sequential scan of a table too big to model runs
/// inline (the workers' code on the session's thread, chunks cut by `scan::keyed_cuts`): the
/// rows the serial path gives, in a transaction that deleted and inserted rows again too
/// (agent 102 found 1,984 rows for 1,982 before 80b46ae1 fixed the cuts).
fn same_inline(s: &mut Session, q: &str) {
    run(s, "SET max_parallel_workers_per_gather = 0;\nSET enable_indexscan = off;\nSET enable_indexonlyscan = off;\nSET enable_bitmapscan = off");
    let serial = pg_engine::exec::parscan::without_serial_inline(|| rows(s, q));
    let inlined = pg_engine::exec::parscan::inline_started();
    let inline = rows(s, q);
    assert!(pg_engine::exec::parscan::inline_started() > inlined, "{q}: inline");
    run(s, "RESET max_parallel_workers_per_gather;\nRESET enable_indexscan;\nRESET enable_indexonlyscan;\nRESET enable_bitmapscan");
    assert_eq!(serial, inline, "{q}");
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

/// COPY through the transaction's sink (`Txn::open_sink`): rows and entries written as
/// sorted runs and linked at commit, into a table with an index; a cursor declared before
/// doesn't see them, statements after do; a failed COPY leaves nothing; writes over the
/// linked keys later in the transaction; a restart.
#[test]
fn copy_through_the_sink() {
    let dir = tempfile::tempdir().unwrap();
    // (A buffer that holds the load's first batch until its TIDs are stamped, as the usual
    // 64 MB one does: stamped after a spill, those rows are rewritten, and the spilled run
    // with them, whose entries span the index's range, keeps the sink's runs from linking.)
    pg_engine::storage::set_default_txn_spill_bytes(1 << 20);
    let eng = open(dir.path());
    pg_engine::storage::set_default_txn_spill_bytes(0);
    let queries = ["SELECT count(*), sum(a) FROM cs", "SELECT a, count(*) FROM cs WHERE a < 9 GROUP BY a ORDER BY a"];
    let data = |from: i32, to: i32| -> String { (from..=to).map(|i| format!("{i}\t{}\tt{i}\n", i % 37)).collect() };
    let before = with_max_versions(500, || {
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        run(&mut s, "CREATE TABLE cs (id int NOT NULL, a int, t text);\nCREATE INDEX cs_a ON cs (a)");
        let linked = pg_engine::storage::LINKED_RUNS.load(Ordering::Relaxed);
        let sunk = pg_engine::txn::SINK_RUNS.load(Ordering::Relaxed);
        s.query_copy("COPY cs FROM STDIN", data(1, 20000).as_bytes()).unwrap();
        assert!(pg_engine::txn::SINK_RUNS.load(Ordering::Relaxed) > sunk, "runs from the sink");
        assert!(pg_engine::storage::LINKED_RUNS.load(Ordering::Relaxed) > linked, "runs linked");
        same_both_ways(&mut s, &queries);
        run(&mut s, "BEGIN;\nDECLARE c CURSOR FOR SELECT count(*) FROM cs");
        s.query_copy("COPY cs FROM STDIN", data(20001, 30000).as_bytes()).unwrap();
        assert_eq!(rows(&mut s, "FETCH c"), ["[Int8(20000)]"]);
        assert_eq!(rows(&mut s, "SELECT count(*) FROM cs WHERE id > 20000"), ["[Int8(10000)]"]);
        run(&mut s, "UPDATE cs SET a = a + 100 WHERE id % 1000 = 7;\nDELETE FROM cs WHERE id % 1000 = 8;\nCOMMIT");
        same_both_ways(&mut s, &queries);
        // (VACUUM's measure finds the TOAST table's oldest xmin too: no value is out of line,
        // so its relfrozenxid passes the table's, held back by the rows' xmins.)
        run(&mut s, "VACUUM cs");
        let q = "SELECT c.relfrozenxid::text::bigint < t.relfrozenxid::text::bigint FROM pg_class c JOIN pg_class t ON t.oid = c.reltoastrelid WHERE c.relname = 'cs'";
        assert_eq!(rows(&mut s, q), ["[Bool(true)]"]);
        let bad = format!("{}x\t1\tbad\n", data(30001, 39000));
        assert!(s.query_copy("COPY cs FROM STDIN", bad.as_bytes()).is_err());
        assert_eq!(rows(&mut s, "SELECT count(*), max(id) FROM cs"), ["[Int8(29970), Int4(30000)]"]);
        let r = (rows(&mut s, queries[0]), rows(&mut s, queries[1]));
        eng.shutdown();
        r
    });
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    assert_eq!((rows(&mut s, queries[0]), rows(&mut s, queries[1])), before);
}

/// Parallel btree builds (`session::ddl::parbuild`) give what the serial build gives: the
/// same entries (index-only scans against the rows), pg_class numbers, and unique errors.
#[test]
fn parallel_index_builds() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    with_max_versions(500, || {
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        run(&mut s, "CREATE TABLE pb (id int NOT NULL, a int, t text);
             INSERT INTO pb SELECT g, CASE WHEN g % 97 = 0 THEN NULL ELSE g % 211 END, 'v' || (g % 50) FROM generate_series(1, 6000) g;
             VACUUM pb;
             SET min_parallel_table_scan_size = 0");
        let mut stats = Vec::new();
        for workers in [0, 2] {
            run(&mut s, &format!("SET max_parallel_maintenance_workers = {workers}"));
            let before = pg_engine::exec::parscan::index_builds();
            run(&mut s, "CREATE INDEX pb_a ON pb (a) INCLUDE (t);\nCREATE UNIQUE INDEX pb_id ON pb (id);\nCREATE INDEX pb_t ON pb (t, a);\nVACUUM pb");
            assert_eq!(pg_engine::exec::parscan::index_builds() - before, if workers > 0 { 3 } else { 0 });
            // (Each query through the indexes and through the rows: the same answers.)
            for q in ["SELECT a, count(*) FROM pb WHERE a < 20 GROUP BY a ORDER BY a", "SELECT count(*) FROM pb WHERE id > 100", "SELECT t, a FROM pb WHERE t < 'v2' ORDER BY t, a LIMIT 30"] {
                run(&mut s, "SET enable_seqscan = off;\nSET enable_bitmapscan = off");
                let plan = rows(&mut s, &format!("EXPLAIN (COSTS OFF) {q}")).join("\n");
                assert!(plan.contains("Index"), "{q}: {plan}");
                let by_index = rows(&mut s, q);
                run(&mut s, "RESET enable_seqscan;\nSET enable_indexscan = off;\nSET enable_indexonlyscan = off");
                assert_eq!(by_index, rows(&mut s, q), "{q}");
                run(&mut s, "RESET enable_bitmapscan;\nRESET enable_indexscan;\nRESET enable_indexonlyscan");
            }
            stats.push(rows(&mut s, "SELECT relname, relpages, reltuples FROM pg_class WHERE relname LIKE 'pb\\_%' ORDER BY relname"));
            let mut errs = Vec::new();
            for sql in ["CREATE UNIQUE INDEX pb_bad ON pb (a)", "CREATE UNIQUE INDEX pb_bad ON pb (t)", "CREATE UNIQUE INDEX pb_bad ON pb (a) NULLS NOT DISTINCT"] {
                let e = s.query(sql).unwrap_err();
                errs.push((e.code, e.message.clone(), e.detail.clone()));
            }
            run(&mut s, "UPDATE pb SET a = -id WHERE a IS NULL OR id > 211;\nCREATE UNIQUE INDEX pb_ok ON pb (a) NULLS NOT DISTINCT");
            stats.push(errs.iter().map(|e| format!("{e:?}")).collect());
            run(&mut s, "DROP INDEX pb_a, pb_id, pb_t, pb_ok;\nUPDATE pb SET a = CASE WHEN id % 97 = 0 THEN NULL ELSE id % 211 END;\nVACUUM pb");
        }
        assert_eq!(stats[0], stats[2]);
        assert_eq!(stats[1], stats[3]);
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
        // (VACUUM measures the rows; the builds take the heap's estimated pages and size only
        // the rows at the table's start, and ANALYZE of a table too big to model samples them.)
        assert!(pg_engine::storage::row::STORED_SIZES.load(Ordering::Relaxed) >= sized + 1500, "sized from the stored bytes");
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

/// BRIN and GIN builds of a table too big to model measure the index as their access method
/// lays it out (`am_measure`): PostgreSQL 18.6's relpages (and pg_relation_size) for the same
/// table, after the build and after a VACUUM (whose kept measure the next build reads); built
/// in parallel too (`parbuild::measure_am_parallel`; PostgreSQL's parallel builds of this table,
/// with min_parallel_table_scan_size 0, give the same pages), first and after the VACUUM.
#[test]
fn brin_gin_builds_measure_as_postgres() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    with_max_versions(400, || {
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        run(&mut s, "CREATE TABLE am (id int NOT NULL, ts timestamptz, t text, tags int[], doc tsvector, j jsonb);
             INSERT INTO am SELECT g, timestamptz '2026-01-01' + g * interval '1 min', CASE WHEN g % 11 = 0 THEN NULL ELSE md5(g::text) END,
               ARRAY[g % 50, g % 300, 1000 + g % 3], to_tsvector('simple', 'w' || (g % 700) || ' x' || (g % 9) || ' common'),
               jsonb_build_object('u', g % 400, 'k', 'k' || (g % 5)) FROM generate_series(1, 4000) g");
        let build = "CREATE INDEX am_b ON am USING brin (ts) WITH (pages_per_range = 1);
             CREATE INDEX am_bt ON am USING brin (t, id) WITH (pages_per_range = 2);
             CREATE INDEX am_g ON am USING gin (tags);
             CREATE INDEX am_d ON am USING gin (doc);
             CREATE INDEX am_j ON am USING gin (j);
             CREATE INDEX am_jp ON am USING gin (j jsonb_path_ops)";
        let stats = "SELECT string_agg(relname || ' ' || relpages, ', ' ORDER BY relname) FROM pg_class WHERE relname LIKE 'am%' AND relpages = pg_relation_size(oid) / 8192";
        let want = [r#"[Text("am 91, am_b 3, am_bt 3, am_d 11, am_g 8, am_j 10, am_jp 8")]"#];
        let drop = "DROP INDEX am_b, am_bt, am_g, am_d, am_j, am_jp";
        // (Workers for a table this small, two as PostgreSQL's 256 MB allows; am_bt's text
        // summaries are measured serially.)
        let parallel = |s: &mut Session| {
            run(s, "SET min_parallel_table_scan_size = 0;\nSET maintenance_work_mem = '256MB'");
            let before = pg_engine::exec::parscan::index_builds();
            run(s, build);
            assert!(pg_engine::exec::parscan::index_builds() - before >= 5, "parallel builds");
            assert_eq!(rows(s, stats), want);
            run(s, &format!("{drop};\nRESET min_parallel_table_scan_size;\nRESET maintenance_work_mem"));
        };
        parallel(&mut s);
        run(&mut s, build);
        assert_eq!(rows(&mut s, stats), want);
        run(&mut s, &format!("{drop};\nVACUUM am"));
        run(&mut s, build);
        assert_eq!(rows(&mut s, stats), want);
        run(&mut s, drop);
        parallel(&mut s);
    });
}

/// A DELETE's index entries go in together at its end (`Txn::defer_entry_delete`): within
/// its transaction, index scans, index-only scans from the entries, unique keys inserted
/// again and savepoints see what deleting them one at a time gives; so do DELETEs of rows
/// their transaction wrote itself.
#[test]
fn deferred_entry_deletes() {
    let dir = tempfile::tempdir().unwrap();
    // (A 1 MB buffer: a DELETE of more than 16 kB of keys writes them as runs.)
    pg_engine::storage::set_default_txn_spill_bytes(1 << 20);
    let eng = open(dir.path());
    pg_engine::storage::set_default_txn_spill_bytes(0);
    let runs = || pg_engine::txn::DELETE_RUNS.load(Ordering::Relaxed);
    with_max_versions(500, || {
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        run(&mut s, "CREATE TABLE dd (id int PRIMARY KEY, a int, t text);
             CREATE INDEX dd_a ON dd (a);
             INSERT INTO dd SELECT g, g % 50, 'v' || g FROM generate_series(1, 3000) g;
             VACUUM ANALYZE dd");
        let queries = ["SELECT count(*) FROM dd", "SELECT a, count(*) FROM dd WHERE a < 10 GROUP BY a ORDER BY a"];
        run(&mut s, "BEGIN;\nDELETE FROM dd WHERE a % 3 = 0;\nINSERT INTO dd VALUES (3, 3, 'again'), (6, 6, 'again')");
        // (By rows, the count is a parallel scan's: its chunks' bounds once let rows the
        // transaction deleted come back, `scan::keyed_cuts`.)
        let started = pg_engine::exec::parscan::started();
        same_both_ways(&mut s, &queries);
        assert!(pg_engine::exec::parscan::started() > started, "a parallel scan");
        same_inline(&mut s, queries[1]);
        run(&mut s, "SET enable_seqscan = off;\nSET enable_bitmapscan = off");
        assert_eq!(rows(&mut s, "SELECT id, t FROM dd WHERE a = 6 ORDER BY id"), ["[Int4(6), Text(\"again\")]"]);
        run(&mut s, "SAVEPOINT p;\nDELETE FROM dd WHERE a = 1;\nROLLBACK TO p;\nDELETE FROM dd WHERE a = 2");
        assert_eq!(rows(&mut s, "SELECT count(*) FROM dd WHERE a IN (1, 2)"), ["[Int8(60)]"]);
        run(&mut s, "RESET enable_seqscan;\nRESET enable_bitmapscan;\nCOMMIT");
        same_both_ways(&mut s, &queries);
        run(&mut s, "BEGIN;\nINSERT INTO dd SELECT g, g % 50, 'n' FROM generate_series(10001, 12000) g;\nDELETE FROM dd WHERE id > 10000 AND a < 25;\nCOMMIT");
        same_both_ways(&mut s, &queries);
        same_inline(&mut s, queries[1]);
        let e = s.query("INSERT INTO dd VALUES (10030, 0, 'dup')").unwrap_err();
        assert!(format!("{e:?}").contains("duplicate key"), "{e:?}");
        run(&mut s, "INSERT INTO dd VALUES (10001, 0, 'ok')");
        run(&mut s, "UPDATE dd SET t = (SELECT string_agg(md5(g::text), '') FROM generate_series(1, 400) g) WHERE id = 7");
        // A cursor declared just before a DELETE whose rows and entries go in as runs reads
        // what its snapshot saw, through the index too; the statements after see them gone.
        run(&mut s, "SET enable_seqscan = off;\nSET enable_bitmapscan = off;\nBEGIN");
        let want = rows(&mut s, "SELECT count(*), sum(id) FROM dd WHERE a = 7");
        let want2 = rows(&mut s, "SELECT id, md5(t) FROM dd WHERE a = 7 ORDER BY id");
        assert!(want2.len() > 10);
        run(&mut s, "DECLARE c CURSOR FOR SELECT count(*), sum(id) FROM dd WHERE a = 7;\nDECLARE c2 CURSOR FOR SELECT id, md5(t) FROM dd WHERE a = 7 ORDER BY id");
        let before = runs();
        run(&mut s, "DELETE FROM dd WHERE a >= 0");
        assert!(runs() > before, "the deletes went in as runs");
        assert_eq!(rows(&mut s, "FETCH ALL FROM c"), want);
        assert_eq!(rows(&mut s, "FETCH ALL FROM c2"), want2);
        assert_eq!(rows(&mut s, "SELECT count(*) FROM dd WHERE a = 7"), ["[Int8(0)]"]);
        run(&mut s, "COMMIT;\nRESET enable_seqscan;\nRESET enable_bitmapscan");
        assert_eq!(rows(&mut s, "SELECT count(*) FROM dd"), ["[Int8(0)]"]);
    });
}

/// VACUUM of a big table nothing changed since its last measure takes that measure again
/// (`txn::unchanged`) instead of reading the rows: VACUUM's own, or a COPY's into a table its
/// transaction TRUNCATEd (measured as it went in). The pg_class numbers are a full read's: a
/// table loaded the same way without the TRUNCATE is read, and gives the same.
#[test]
fn vacuum_skips_unchanged_tables() {
    use pg_engine::txn::unchanged::skips as skipped;
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    with_max_versions(500, || {
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        let data: String = (1..=6000).map(|i| format!("{i}\t{}\n", "x".repeat(i % 70))).collect();
        run(&mut s, "CREATE TABLE vu_a (id int, t text);\nCREATE TABLE vu_b (id int, t text)");
        let load = |s: &mut Session, t: &str, truncate: bool| -> String {
            run(s, if truncate { "BEGIN;\nTRUNCATE vu_a" } else { "BEGIN" });
            let x = rows(s, "SELECT pg_current_xact_id()::text")[0].clone();
            s.query_copy(&format!("COPY {t} FROM STDIN"), data.as_bytes()).unwrap();
            run(s, "COMMIT");
            x.trim_start_matches("[Text(\"").trim_end_matches("\")]").to_string()
        };
        let (xa, xb) = (load(&mut s, "vu_a", true), load(&mut s, "vu_b", false));
        let stats = |s: &mut Session, t: &str, x: &str| {
            rows(s, &format!("SELECT relpages, reltuples, relallvisible, relfrozenxid::text = '{x}' FROM pg_class WHERE relname = '{t}'"))
        };
        let skips = |n| assert_eq!(skipped(), n);
        let base = skipped();
        run(&mut s, "VACUUM vu_a");
        skips(base + 1);
        run(&mut s, "VACUUM vu_b");
        skips(base + 1);
        let a = stats(&mut s, "vu_a", &xa);
        assert_eq!(a, stats(&mut s, "vu_b", &xb));
        assert!(a[0].ends_with("Bool(true)]"), "{a:?}");
        // Again: both skip, with the same numbers; an index changes no row.
        run(&mut s, "CREATE INDEX vu_b_id ON vu_b (id);\nVACUUM vu_a;\nVACUUM vu_b");
        skips(base + 3);
        assert_eq!(stats(&mut s, "vu_a", &xa), a);
        assert_eq!(stats(&mut s, "vu_b", &xb), a);
        // A change to the rows: read again, then kept again.
        run(&mut s, "UPDATE vu_b SET t = t || 'y' WHERE id % 7 = 0;\nDELETE FROM vu_a WHERE id % 5 = 0;\nVACUUM vu_a;\nVACUUM vu_b");
        skips(base + 3);
        let (a2, b2) = (stats(&mut s, "vu_a", &xa), stats(&mut s, "vu_b", &xb));
        run(&mut s, "VACUUM vu_a;\nVACUUM vu_b");
        skips(base + 5);
        assert_eq!((stats(&mut s, "vu_a", &xa), stats(&mut s, "vu_b", &xb)), (a2, b2));
        // A second COPY in the TRUNCATE's transaction, or a write after the load in it: read.
        run(&mut s, "BEGIN;\nTRUNCATE vu_a");
        s.query_copy("COPY vu_a FROM STDIN", data.as_bytes()).unwrap();
        run(&mut s, "INSERT INTO vu_a VALUES (0, 'z');\nCOMMIT;\nVACUUM vu_a");
        skips(base + 5);
        assert_eq!(rows(&mut s, "SELECT reltuples FROM pg_class WHERE relname = 'vu_a'"), ["[Float4(6001.0)]"]);
    });
}

/// VACUUM of a modelled table nothing changed in since the last reads no row for its frozen
/// xid: it takes the xmins that read kept (`txn::unchanged`), and gives the same relfrozenxid.
#[test]
fn vacuum_keeps_xmins_of_unchanged_tables() {
    use pg_engine::txn::unchanged::XMIN_SKIPS;
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    run(&mut s, "CREATE TABLE vx (id int, t text);\nINSERT INTO vx SELECT g, 'v' || g FROM generate_series(1, 3000) g");
    let q = "SELECT relfrozenxid::text, (SELECT relfrozenxid::text FROM pg_class WHERE oid = c.reltoastrelid) FROM pg_class c WHERE relname = 'vx'";
    let n = || XMIN_SKIPS.load(Ordering::Relaxed);
    // (Other tests' VACUUMs count too: only increases are checked.)
    run(&mut s, "VACUUM vx");
    let first = rows(&mut s, q);
    let base = n();
    run(&mut s, "VACUUM vx");
    assert!(n() > base);
    assert_eq!(rows(&mut s, q), first);
    run(&mut s, "UPDATE vx SET t = t || 'x' WHERE id % 10 = 0;\nVACUUM vx");
    let third = rows(&mut s, q);
    let base = n();
    run(&mut s, "VACUUM vx");
    assert!(n() > base);
    assert_eq!(rows(&mut s, q), third);
}

/// A modelled table keeps its pages across a restart (`heapsim::restored`: each row back at
/// the TID it was stored with): the planner's page counts and pg_relation_size are what they
/// were, with rows deleted from the middle of it.
#[test]
fn heap_pages_kept_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let queries = [
        "SELECT pg_relation_size('hp')",
        "EXPLAIN SELECT * FROM hp WHERE a > 3",
        "SELECT relpages FROM pg_class WHERE relname = 'hp'",
    ];
    let before: Vec<Vec<String>> = {
        let eng = open(dir.path());
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        run(&mut s, "CREATE TABLE hp (id int, a int, t text);
             INSERT INTO hp SELECT g, g % 7, repeat('x', 40 + g % 60) FROM generate_series(1, 3000) g;
             DELETE FROM hp WHERE id BETWEEN 500 AND 1500 AND id % 3 <> 0;
             UPDATE hp SET t = t || 'y' WHERE id % 11 = 0;
             VACUUM hp");
        let r = queries.iter().map(|q| rows(&mut s, q)).collect();
        drop(s);
        eng.shutdown();
        r
    };
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    for (q, b) in queries.iter().zip(&before) {
        assert_eq!(&rows(&mut s, q), b, "{q}");
    }
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

#[test]
fn estimate_rebuilt_with_higher_model_limit() {
    let dir = tempfile::tempdir().unwrap();
    let before = {
        let eng = open(dir.path());
        let r = with_max_versions(500, || {
            let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
            run(&mut s, "CREATE TABLE est_upgrade (id int PRIMARY KEY, k int);
                 INSERT INTO est_upgrade SELECT g, g % 17 FROM generate_series(1, 1200) g;
                 CREATE INDEX est_upgrade_k ON est_upgrade (k);
                 DELETE FROM est_upgrade WHERE id % 5 = 0");
            let result = rows(&mut s, "SELECT id, k FROM est_upgrade ORDER BY id");
            let oid = s.query("SELECT 'est_upgrade'::regclass::oid").unwrap()[0].rows[0][0].clone();
            let pg_engine::types::Value::Oid(oid) = oid else { panic!("expected oid") };
            rows(&mut s, "SELECT pg_relation_size('est_upgrade')");
            assert!(pg_engine::exec::sizing::is_estimated(oid, &s.state.db));
            result
        });
        eng.shutdown();
        r
    };
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    rows(&mut s, "SELECT pg_relation_size('est_upgrade')");
    let oid = s.query("SELECT 'est_upgrade'::regclass::oid").unwrap()[0].rows[0][0].clone();
    let pg_engine::types::Value::Oid(oid) = oid else { panic!("expected oid") };
    assert!(!pg_engine::exec::sizing::is_estimated(oid, &s.state.db));
    assert_eq!(rows(&mut s, "SELECT id, k FROM est_upgrade ORDER BY id"), before);
    for q in ["SELECT id FROM est_upgrade ORDER BY id", "SELECT k FROM est_upgrade WHERE k BETWEEN 3 AND 8 ORDER BY k"] {
        run(&mut s, "SET enable_seqscan = off;\nSET enable_bitmapscan = off");
        let indexed = rows(&mut s, q);
        run(&mut s, "RESET enable_seqscan;\nRESET enable_bitmapscan;\nSET enable_indexscan = off;\nSET enable_indexonlyscan = off");
        assert_eq!(rows(&mut s, q), indexed, "{q}");
        run(&mut s, "RESET enable_indexscan;\nRESET enable_indexonlyscan");
    }
}

/// ANALYZE of a table too big to model, with more pages than its sample takes: only the
/// sampled pages' rows are read (`Ddl::rowid_blocks`), its tuples estimated from them as
/// PostgreSQL estimates them, near the count; its pages are the estimate it had; the
/// statistics come from the sample.
#[test]
fn analyze_samples_pages_of_big_tables() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    with_max_versions(500, || {
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        run(&mut s, "CREATE TABLE sa (id int, a int, t text);
             INSERT INTO sa SELECT g, g % 10, repeat('x', 100) FROM generate_series(1, 40000) g;
             DELETE FROM sa WHERE id % 4 = 0;
             SET default_statistics_target = 1;
             ANALYZE sa");
        let r = rows(&mut s, "SELECT relpages, reltuples FROM pg_class WHERE relname = 'sa'");
        let v: Vec<f64> = r[0]
            .trim_matches(|c| c == '[' || c == ']')
            .split(", ")
            .map(|x| x.trim_end_matches(')').rsplit('(').next().unwrap().parse().unwrap())
            .collect();
        assert!(v[0] > 300.0, "{r:?}");
        assert!((v[1] - 30000.0).abs() < 3000.0, "{r:?}");
        assert_eq!(rows(&mut s, "SELECT n_distinct FROM pg_stats WHERE tablename = 'sa' AND attname = 'a'"), ["[Float4(10.0)]"]);
    });
}

/// INCLUDE size checks on the measured serial callback, with its actual unmodelled
/// heap state asserted. Error sizes are the PG18.6 a163_review_sizes fixture's.
#[test]
fn measured_serial_include_size() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    with_max_versions(500, || {
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        s.query("SET max_parallel_maintenance_workers=0").unwrap();
        s.query("CREATE FUNCTION mi_text(n int) RETURNS text LANGUAGE sql IMMUTABLE AS $$ SELECT left(string_agg(md5(g::text), ''), n) FROM generate_series(1, (n+31)/32) g $$").unwrap();
        for (len, expected) in [(2800, "index row size 2816 exceeds btree version 4 maximum 2704"), (8300, "index row requires 8320 bytes, maximum size is 8191")] {
            s.query("CREATE TABLE mi (k int, payload text)").unwrap();
            s.query(&format!("INSERT INTO mi SELECT g, CASE WHEN g=6000 THEN mi_text({len}) ELSE 'small' END FROM generate_series(1,6000) g")).unwrap();
            // The bulk insert already leaves this heap unmodelled. VACUUM of the
            // 8300-byte raw row hits a separately recorded HeapPacker overflow before
            // CREATE INDEX; it is not needed to enter the measured build callback.
            let result = s.query("SELECT oid FROM pg_class WHERE relname='mi'").unwrap();
            let pg_engine::types::Value::Oid(oid) = result[0].rows[0][0] else { panic!("table oid") };
            assert!(pg_engine::exec::sizing::is_estimated(oid, &s.state.db));
            assert!(pg_engine::exec::sizing::heap_writes(oid, &s.state.db).is_none());
            let e = s.query("CREATE INDEX mi_i ON mi(k) INCLUDE(payload)").unwrap_err();
            assert_eq!(e.code, "54000");
            assert!(e.message.starts_with(expected), "{e:?}");
            eprintln!("measured serial INCLUDE: estimated=true heap_model=false workers=0: {}", e.message);
            s.query("DROP TABLE mi").unwrap();
        }
    });
}

/// Hash, GIN and BRIN indexes of a table too big to model have the relpages and
/// pg_relation_size PostgreSQL 18.6's build gives them (its numbers below), as those of a
/// modelled table do: a build's pass over the rows lays them out (hash: `sizing::BigIndex`;
/// GIN and BRIN: `am_measure`), and a modelled table's relpages are its models' sizes.
#[test]
fn hash_gin_brin_sizes_past_the_model() {
    let want = [
        "[Text(\"bx_b\"), Int4(3), Int8(3)]",
        "[Text(\"bx_g\"), Int4(44), Int8(44)]",
        "[Text(\"bx_h\"), Int4(136), Int8(136)]",
        "[Text(\"bx_hid\"), Int4(130), Int8(130)]",
    ];
    let sizes = "SELECT c.relname::text, c.relpages, pg_relation_size(c.oid) / 8192 FROM pg_class c WHERE relname LIKE 'bx_%' ORDER BY 1";
    for big in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let eng = open(dir.path());
        let go = || {
            let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
            run(&mut s, "CREATE TABLE bx (id int, a int, tags int[], doc text);
                 INSERT INTO bx SELECT g, g % 997, ARRAY[g % 50, g % 7, g % 1000], 'w' || (g % 300) || ' x' || (g % 11) FROM generate_series(1, 30000) g;
                 CREATE INDEX bx_h ON bx USING hash (a);
                 CREATE INDEX bx_hid ON bx USING hash (id);
                 CREATE INDEX bx_g ON bx USING gin (tags);
                 CREATE INDEX bx_b ON bx USING brin (id)");
            assert_eq!(rows(&mut s, sizes), want, "built, too big to model: {big}");
            run(&mut s, "VACUUM bx");
            assert_eq!(rows(&mut s, sizes), want, "vacuumed, too big to model: {big}");
        };
        if big { with_max_versions(5000, go) } else { go() }
    }
}
