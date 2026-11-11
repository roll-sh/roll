//! Heap and btree models kept across a clean restart (`sizing::kept`): a table's first
//! statement after one loads them instead of reading its rows, and everything the models
//! decide (ctids of new versions, page counts, plans, VACUUM) comes out as if the database
//! had never restarted, as PostgreSQL's files keep their pages (heap and index pages, line
//! pointers, free space and visibility maps).

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::session::Session;
use std::path::Path;
use std::time::Duration;

fn open(dir: &Path) -> std::sync::Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: dir.to_path_buf(),
        sync: false,
        idle_close: Duration::from_secs(600),
        superuser: "postgres".into(),
        max_prepared_transactions: 2,
        ..Default::default()
    })
    .unwrap()
}

fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    r.last().unwrap().rows.iter().map(|row| format!("{row:?}")).collect()
}

#[test]
fn vacuum_freeze_keeps_page_state_across_restart() {
    for (frozen, clean) in [(false, true), (true, true), (false, false)] {
        let dir = tempfile::tempdir().unwrap();
        {
            let eng = open(dir.path());
            let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
            exec(&mut s, "CREATE TABLE vf (id int); INSERT INTO vf SELECT generate_series(1, 7458)");
            s.query(if frozen { "VACUUM (FREEZE, DISABLE_PAGE_SKIPPING) vf" } else { "VACUUM vf" }).unwrap();
            s.query("INSERT INTO vf VALUES (7459)").unwrap();
            s.query("VACUUM vf").unwrap();
            assert_eq!(rows(&mut s, "SELECT reltuples FROM pg_class WHERE relname = 'vf'"), ["[Float4(7458.0)]"]);
            drop(s);
            if clean {
                eng.shutdown();
            }
        }
        let eng = open(dir.path());
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        s.query("VACUUM (FREEZE) vf").unwrap();
        let expected = if frozen { "[Float4(7458.0)]" } else { "[Float4(7459.0)]" };
        assert_eq!(rows(&mut s, "SELECT reltuples FROM pg_class WHERE relname = 'vf'"), [expected], "frozen before restart: {frozen}, clean: {clean}");
    }
}

#[test]
fn nulls_not_distinct_cleanup_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let eng = open(dir.path());
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        exec(&mut s, "CREATE TABLE nd (n int, k int, v int); CREATE UNIQUE INDEX nd_i ON nd(n,k) INCLUDE(v) NULLS NOT DISTINCT; INSERT INTO nd SELECT NULL,g,0 FROM generate_series(1,1500)g");
        drop(s);
        eng.shutdown();
    }
    for _ in 0..2 {
        let eng = open(dir.path());
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        for _ in 0..10 {
            exec(&mut s, "DELETE FROM nd; INSERT INTO nd SELECT NULL,g,1 FROM generate_series(1,1500)g");
        }
        assert_eq!(rows(&mut s, "SELECT pg_relation_size('nd_i') = 65536, count(*) = 1500 FROM nd"), ["[Bool(true), Bool(true)]"]);
        drop(s);
        eng.shutdown();
    }
}

/// Each statement on its own (VACUUM can't run in the implicit block of several).
fn exec(s: &mut Session, sql: &str) {
    for q in sql.split(';') {
        s.query(q).unwrap_or_else(|e| panic!("{q}: {e:?}"));
    }
}

const SETUP: &str = "CREATE TABLE km (id int PRIMARY KEY, a int, t text, big text);
    CREATE INDEX km_a ON km (a, id);
    CREATE INDEX km_part ON km (t) WHERE a % 3 = 0;
    CREATE TABLE km2 (x int, y text);
    CREATE INDEX km2_x ON km2 (x);
    INSERT INTO km SELECT g, g % 17, repeat('t', g % 50), CASE WHEN g % 97 = 0 THEN repeat(md5(g::text), 400) END
      FROM generate_series(1, 4000) g;
    INSERT INTO km2 SELECT g, 'y' || g FROM generate_series(1, 3000) g;
    DELETE FROM km WHERE id BETWEEN 1000 AND 1600 AND id % 4 <> 0;
    UPDATE km SET t = t || 'u' WHERE id % 13 = 0;
    UPDATE km SET a = a + 1 WHERE id % 29 = 0;
    UPDATE km2 SET y = y || '!' WHERE x % 7 = 0;
    VACUUM km2;
    UPDATE km SET big = repeat(md5(id::text), 300) WHERE id % 211 = 0";

/// What the models decide, read before and after more writes.
const CHECKS: &[&str] = &[
    "SELECT ctid, id FROM km ORDER BY id",
    "SELECT ctid, x FROM km2 ORDER BY x",
    "SELECT pg_relation_size('km'), pg_relation_size('km_pkey'), pg_relation_size('km_a'), pg_relation_size('km_part'), pg_relation_size('km2'), pg_relation_size('km2_x')",
    "SELECT pg_relation_size(reltoastrelid) FROM pg_class WHERE relname = 'km'",
    "EXPLAIN SELECT * FROM km WHERE a = 3",
    "EXPLAIN SELECT * FROM km2 WHERE x < 100",
];

const MORE: &str = "INSERT INTO km SELECT g, g % 5, 'new', NULL FROM generate_series(5001, 5400) g;
    UPDATE km SET a = a + 100 WHERE id % 31 = 0;
    UPDATE km SET t = 'hot' WHERE id % 37 = 0;
    DELETE FROM km WHERE id % 41 = 0;
    INSERT INTO km2 SELECT g, 'z' FROM generate_series(4000, 4300) g;
    UPDATE km2 SET y = 'w' WHERE x % 11 = 0";

/// Runs SETUP in `dir`, then closes it cleanly (`clean`) or as a crash would leave it.
fn prepare(dir: &Path, clean: bool) {
    let eng = open(dir);
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    exec(&mut s, SETUP);
    for q in CHECKS {
        rows(&mut s, q);
    }
    drop(s);
    if clean {
        eng.shutdown();
    }
}

fn after_restart(dir: &Path, clean: bool) -> Vec<Vec<String>> {
    let eng = open(dir);
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    let out = steps(&mut s);
    drop(s);
    if clean {
        eng.shutdown();
    }
    out
}

/// The checks, then more writes, VACUUM, VACUUM FULL, with the checks after each.
fn steps(s: &mut Session) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = CHECKS.iter().map(|q| rows(s, q)).collect();
    exec(s, MORE);
    out.extend(CHECKS.iter().map(|q| rows(s, q)));
    exec(s, "VACUUM km; VACUUM km2");
    out.extend(CHECKS.iter().map(|q| rows(s, q)));
    s.query("INSERT INTO km SELECT g, 1, 'again', NULL FROM generate_series(6001, 6200) g").unwrap();
    out.extend(CHECKS.iter().map(|q| rows(s, q)));
    s.query("VACUUM FULL km").unwrap();
    out.extend(CHECKS.iter().map(|q| rows(s, q)));
    out
}

fn reopened(dir: &Path) -> Vec<Vec<String>> {
    let eng = open(dir);
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    CHECKS.iter().map(|q| rows(&mut s, q)).collect()
}

/// A clean restart changes nothing the models decide: the same steps on an engine that
/// never restarted give the same results, before the next restart and after it.
#[test]
fn kept_models_match_models_never_restarted() {
    let kept = tempfile::tempdir().unwrap();
    let never = tempfile::tempdir().unwrap();
    prepare(kept.path(), true);
    let a = after_restart(kept.path(), true);
    let (b, last) = {
        let eng = open(never.path());
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        exec(&mut s, SETUP);
        for q in CHECKS {
            rows(&mut s, q);
        }
        let b = steps(&mut s);
        let last: Vec<Vec<String>> = CHECKS.iter().map(|q| rows(&mut s, q)).collect();
        (b, last)
    };
    assert_eq!(a.len(), b.len());
    for (i, (x, y)) in a.iter().zip(&b).enumerate() {
        let d = x.iter().zip(y).find(|(p, q)| p != q);
        assert_eq!(x, y, "step {} {}: first difference {d:?}", i / CHECKS.len(), CHECKS[i % CHECKS.len()]);
    }
    // Once more: the records the first restart's close kept (after VACUUM FULL, from models
    // that were kept themselves).
    assert_eq!(reopened(kept.path()), last);
}

/// After a crash nothing kept is used: the models are laid out from the rows, every row at
/// its stored TID, and what they decide about the rows' places is the same.
#[test]
fn models_after_a_crash_keep_the_rows_in_place() {
    let crashed = tempfile::tempdir().unwrap();
    let kept = tempfile::tempdir().unwrap();
    prepare(crashed.path(), false);
    prepare(kept.path(), true);
    let a = after_restart(crashed.path(), false);
    let b = after_restart(kept.path(), true);
    // (The rows' ctids and the heaps' sizes; index pages are those of a fresh build.)
    for (i, (x, y)) in a.iter().zip(&b).enumerate().take(CHECKS.len()) {
        if i < 2 {
            assert_eq!(x, y, "{}", CHECKS[i]);
        }
    }
}

/// A kept record isn't trusted after a crash, nor once its table changed: DDL that changes
/// what the rows' layout depends on, TRUNCATE, a write after a crash.
#[test]
fn kept_models_not_used_when_stale() {
    let dir = tempfile::tempdir().unwrap();
    prepare(dir.path(), true);
    let expect = {
        let eng = open(dir.path());
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        // Written, then a crash (no clean close): the next open lays km out from its rows.
        s.query("INSERT INTO km VALUES (9001, 1, 'x', NULL); UPDATE km SET a = 0 WHERE id = 2").unwrap();
        let r = rows(&mut s, "SELECT ctid, id FROM km ORDER BY id");
        drop(s);
        r
    };
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    assert_eq!(rows(&mut s, "SELECT ctid, id FROM km ORDER BY id"), expect);
    s.query("INSERT INTO km VALUES (9002, 1, 'x', NULL)").unwrap();
    let ctids: Vec<String> = rows(&mut s, "SELECT ctid FROM km");
    let distinct: std::collections::HashSet<&String> = ctids.iter().collect();
    assert_eq!(ctids.len(), distinct.len());
    s.query("ALTER TABLE km2 ADD COLUMN z text DEFAULT repeat('z', 30)").unwrap();
    let size = rows(&mut s, "SELECT pg_relation_size('km2')");
    drop(s);
    eng.shutdown();
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    assert_eq!(rows(&mut s, "SELECT pg_relation_size('km2')"), size);
    s.query("TRUNCATE km2; INSERT INTO km2 VALUES (1, 'a')").unwrap();
    assert_eq!(rows(&mut s, "SELECT ctid FROM km2"), ["[Tid(0, 1)]"]);
}

/// A kept table whose pages were all visible is planned from its record's sizes while its
/// models are still being laid out (`kept::sizes`): the plans, costs and sizes its first
/// statements see are those of its models once laid out, and of before the restart.
#[test]
fn kept_sizes_plan_as_the_models_do() {
    let dir = tempfile::tempdir().unwrap();
    let checks = [
        "EXPLAIN SELECT pad FROM ks WHERE id = 12345",
        "EXPLAIN SELECT * FROM ks WHERE a = 7",
        "SELECT pg_relation_size('ks'), pg_relation_size('ks_pkey'), pg_relation_size('ks_a')",
        "SELECT pad FROM ks WHERE id = 12345",
    ];
    let before: Vec<Vec<String>> = {
        let eng = open(dir.path());
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        exec(&mut s, "CREATE TABLE ks (id int PRIMARY KEY, a int, pad text); CREATE INDEX ks_a ON ks (a);
            INSERT INTO ks SELECT g, g % 101, repeat('p', 60) FROM generate_series(1, 150000) g;
            DELETE FROM ks WHERE id % 10 = 0; VACUUM ks");
        let out = checks.iter().map(|q| rows(&mut s, q)).collect();
        drop(s);
        eng.shutdown();
        out
    };
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    let first: Vec<Vec<String>> = checks.iter().map(|q| rows(&mut s, q)).collect();
    // (A ctid lays the models out, if the background builder hasn't yet.)
    rows(&mut s, "SELECT ctid FROM ks WHERE id = 1");
    let laid: Vec<Vec<String>> = checks.iter().map(|q| rows(&mut s, q)).collect();
    assert_eq!(first, before);
    assert_eq!(laid, before);
}
