//! Filters run over stored rows (`exec::kernel::StoredCmps`: comparisons with constants
//! over the stored bytes) give what evaluating them gives: string comparisons under C,
//! bytewise and deterministic collations, bpchar's padding, NULLs, long values, numbers;
//! a table with values stored out of line reads them as before. Each filter is checked
//! against itself ANDed with a volatile call, which no kernel runs.

mod common;
use common::{engine_session, open_store};
use pg_engine::exec::kernel::STORED_CMPS_SCANS;
use std::sync::atomic::Ordering::Relaxed;

fn rows(s: &mut pg_engine::session::Session, sql: &str) -> String {
    let r = pg_engine::exec::parscan::without_serial_inline(|| s.query(sql))
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    format!("{:?}", r.last().unwrap().rows)
}

#[test]
fn stored_comparisons_match_evaluation() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path());
    let mut s = engine_session(store.clone(), "f");
    s.query("SET max_parallel_workers_per_gather = 0").unwrap();
    for t in ["w", "wbig"] {
        s.query(&format!("CREATE TABLE {t} (a int, e text, v varchar(12), c char(6), n name, m numeric)")).unwrap();
        s.query(&format!(
            "INSERT INTO {t} SELECT i, CASE WHEN i % 13 = 0 THEN NULL WHEN i % 7 = 0 THEN 'X' || (i % 10) \
             WHEN i % 11 = 0 THEN 'é' || (i % 10) ELSE 'x' || (i % 10) END, 'v' || (i % 5), \
             CASE WHEN i % 3 = 0 THEN 'c' || (i % 4) || ' ' ELSE 'c' || (i % 4) END, 'n' || (i % 6), (i % 9) / 4.0 \
             FROM generate_series(1, 3000) i"
        ))
        .unwrap();
        s.query(&format!(
            "INSERT INTO {t} VALUES (3001, repeat('x7', 100), 'v1 ', 'c1', 'n1', 1.25), (3003, 'x7 ', '', '', '', 0)"
        ))
        .unwrap();
    }
    // (Out of line: `wbig` is read as before, values put back.)
    s.query("INSERT INTO wbig VALUES (3002, repeat('y', 200000), 'v2', 'c2    ', 'n2', NULL)").unwrap();
    for q in [
        "CREATE COLLATION und_d (provider = icu, locale = 'und')",
        "CREATE COLLATION und_ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false)",
        "CREATE COLLATION c_utf8 (provider = builtin, locale = 'C.UTF-8')",
    ] {
        s.query(q).unwrap_or_else(|e| panic!("{q}: {e:?}"));
    }
    // Each filter, and whether it runs over stored bytes. A width-bearing varchar or bpchar
    // column's cast to its unbounded type leaves its bytes as they are and compiles away
    // (`kernel::compile`), so those comparisons run over stored bytes too.
    let filters = [
        ("a > 100", true),
        ("e = 'x7'", true),
        ("'x7' = e", true),
        ("e <> 'x7'", true),
        ("e < 'x5'", true),
        ("e >= 'é'", true),
        ("e > 'x7'", true),
        ("e <= 'x7 '", true),
        ("e = repeat('x7', 100)", true),
        ("e > repeat('x', 300)", true),
        ("v = 'v3'", true),
        ("v < 'v1 '", true),
        ("c = 'c1'", true),
        ("c = 'c1  '", true),
        ("c < 'c2'", true),
        ("c >= 'c3 '", true),
        ("c = ''", true),
        ("n = 'n4'", true),
        ("e = 'x7' AND a > 100", true),
        ("a < 2000 AND e > 'x2' AND m >= 1.25", true),
        ("e COLLATE \"C\" < 'x'", true),
        ("e COLLATE \"POSIX\" >= 'X5'", true),
        ("e COLLATE c_utf8 > 'x3'", true),
        ("e COLLATE und_d = 'x7'", true),
        ("e COLLATE und_d <> 'X7'", true),
        ("v COLLATE und_d = 'v3'", true),
        ("c COLLATE und_d = 'c1'", true),
        ("e COLLATE und_d < 'x7'", false),
        ("e COLLATE und_ci = 'X7'", false),
        ("e COLLATE und_ci <> 'x7'", false),
        ("c COLLATE und_ci = 'C1'", false),
        ("e IS NULL", false),
    ];
    let mut paths = Vec::new();
    for (f, fast) in filters {
        for t in ["w", "wbig"] {
            for q in ["SELECT count(*) FROM {t} WHERE {f}", "SELECT a FROM {t} WHERE {f} ORDER BY a"] {
                let q = q.replace("{t}", t);
                let before = STORED_CMPS_SCANS.load(Relaxed);
                let got = rows(&mut s, &q.replace("{f}", f));
                let made = STORED_CMPS_SCANS.load(Relaxed) > before;
                let want = rows(&mut s, &q.replace("{f}", &format!("({f}) AND random() >= 0")));
                assert_eq!(got, want, "{q} with {f}");
                if t == "w" && (!fast || q.contains("count")) && made != fast {
                    paths.push((f, fast, made));
                }
            }
        }
    }
    assert!(paths.is_empty(), "stored paths (filter, expected, actual): {paths:?}");
    assert_eq!(rows(&mut s, "SELECT count(*) FROM w WHERE c = 'c1'"), "[[Int8(751)]]");
    assert_eq!(
        rows(&mut s, "SELECT count(*) FROM w WHERE e COLLATE und_ci = 'X7'"),
        rows(&mut s, "SELECT count(*) FROM w WHERE lower(e) = 'x7'")
    );
    s.query("CREATE TABLE unbounded (c bpchar)").unwrap();
    s.query("INSERT INTO unbounded SELECT c FROM w").unwrap();
    let before = STORED_CMPS_SCANS.load(Relaxed);
    assert_eq!(rows(&mut s, "SELECT count(*) FROM unbounded WHERE c = 'c1  '"), "[[Int8(751)]]");
    assert!(STORED_CMPS_SCANS.load(Relaxed) > before, "unbounded bpchar compared as stored bytes");
    // Old rows lack the new column; updates and own writes must still be visible, and a
    // projection must decode columns beyond the ones read by the stored filter.
    s.query("ALTER TABLE w ADD COLUMN added text DEFAULT 'later'").unwrap();
    let mut old = pg_engine::session::Session::new(pg_engine::session::state::SessionState::new(
        s.state.db.clone(), s.state.cluster.clone(), "postgres", 4243,
    ));
    old.query("SET max_parallel_workers_per_gather = 0").unwrap();
    old.query("BEGIN ISOLATION LEVEL REPEATABLE READ").unwrap();
    let snapshot_query = "SELECT count(*), count(e), sum(a) FROM w WHERE e > 'x2'";
    let snapshot_rows = rows(&mut old, snapshot_query);
    s.query("DELETE FROM w WHERE a <= 900").unwrap();
    s.query("UPDATE w SET e = 'changed', added = 'updated' WHERE a % 17 = 0").unwrap();
    store.flush_memtables().unwrap();
    store.compact().unwrap();
    assert_eq!(rows(&mut old, snapshot_query), snapshot_rows, "pinned snapshot after flush and compaction");
    for phase in ["committed", "own writes", "rolled back"] {
        if phase == "own writes" {
            s.query("BEGIN").unwrap();
            s.query("DELETE FROM w WHERE a % 7 = 0").unwrap();
            s.query("UPDATE w SET e = 'changed', added = 'own' WHERE a % 11 = 0").unwrap();
            s.query("INSERT INTO w (a, e) VALUES (4000, 'changed')").unwrap();
        } else if phase == "rolled back" {
            s.query("ROLLBACK").unwrap();
        }
        for f in ["e = 'changed'", "added = 'later'", "e > 'x2' AND a < 2000", "e >= 'x2' AND e < 'x8'"] {
            for projection in ["count(*)", "a, e, c, m, added"] {
                let order = if projection == "count(*)" { "" } else { " ORDER BY a" };
                let got = rows(&mut s, &format!("SELECT {projection} FROM w WHERE {f}{order}"));
                let want = rows(&mut s, &format!("SELECT {projection} FROM w WHERE ({f}) AND random() >= 0{order}"));
                assert_eq!(got, want, "{phase}: {projection} WHERE {f}");
            }
        }
    }
    assert_eq!(rows(&mut old, snapshot_query), snapshot_rows, "pinned snapshot after rollback");
    old.query("COMMIT").unwrap();
    assert_eq!(rows(&mut old, snapshot_query), rows(&mut s, snapshot_query), "new snapshot");
    // Compare the actual out-of-line value, and project it through an unrelated filter.
    assert_eq!(rows(&mut s, "SELECT a FROM wbig WHERE e = repeat('y', 200000)"), "[[Int4(3002)]]");
    assert_eq!(rows(&mut s, "SELECT length(e) FROM wbig WHERE a = 3002"), "[[Int4(200000)]]");
}

#[test]
fn direct_projection_preserves_values_expressions_and_effects() {
    use pg_engine::types::Value;
    let dir = tempfile::tempdir().unwrap();
    let mut s = engine_session(open_store(dir.path()), "projection");
    s.query("SET max_parallel_workers_per_gather = 0").unwrap();
    let extra = (0..64).map(|i| format!(", pad{i} text")).collect::<String>();
    s.query(&format!("CREATE TABLE accounts (id int PRIMARY KEY, owner text, balance int, note text{extra})")).unwrap();
    s.query("INSERT INTO accounts (id, owner, balance, note) SELECT i, 'owner ' || i,
        CASE WHEN i % 7 = 0 THEN NULL ELSE i * 10 END,
        CASE WHEN i % 11 = 0 THEN NULL ELSE repeat(md5(i::text), 400) END
        FROM generate_series(1, 103) i").unwrap();
    let get = |s: &mut pg_engine::session::Session, sql: &str| {
        s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")).pop().unwrap().rows
    };
    let balance = |i: i32| if i % 7 == 0 { Value::Null } else { Value::Int4(i * 10) };
    // The measured primary-key range's exact [0, 2] projection, with a wide input row.
    s.query("PREPARE range_read(int) AS SELECT id, balance FROM accounts WHERE id BETWEEN $1 AND $1+99 ORDER BY id").unwrap();
    let want: Vec<_> = (2..=101).map(|i| vec![Value::Int4(i), balance(i)]).collect();
    assert_eq!(get(&mut s, "EXECUTE range_read(2)"), want);
    for (projection, expected) in [
        ("balance, id", (2..=101).map(|i| vec![balance(i), Value::Int4(i)]).collect::<Vec<_>>()),
        ("id, id, balance", (2..=101).map(|i| vec![Value::Int4(i), Value::Int4(i), balance(i)]).collect()),
        ("id, balance + id, balance", (2..=101).map(|i| vec![Value::Int4(i),
            if i % 7 == 0 { Value::Null } else { Value::Int4(i * 11) }, balance(i)]).collect()),
    ] {
        assert_eq!(get(&mut s, &format!("SELECT {projection} FROM accounts WHERE id BETWEEN 2 AND 101 ORDER BY id")), expected);
    }
    // TOAST is resolved by the scan: moving the value must keep the entire text alive.
    let text = get(&mut s, "SELECT id, note FROM accounts WHERE id BETWEEN 10 AND 12 ORDER BY id");
    for (i, row) in (10..=12).zip(&text) {
        let expected = if i == 11 { Value::Null } else {
            get(&mut s, &format!("SELECT repeat(md5('{i}'), 400)"))[0][0].clone()
        };
        assert_eq!(row, &vec![Value::Int4(i), expected]);
    }
    s.query("ALTER TABLE accounts ADD COLUMN added text DEFAULT 'later'").unwrap();
    assert_eq!(get(&mut s, "SELECT id, added FROM accounts WHERE id = 2"), vec![vec![Value::Int4(2), Value::text("later")]]);
    assert_eq!(get(&mut s, "SELECT FROM accounts WHERE id BETWEEN 2 AND 4"), vec![vec![], vec![], vec![]]);
    assert!(get(&mut s, "SELECT id, balance FROM accounts WHERE id < 0").is_empty());
    s.query("CREATE SEQUENCE projection_effect").unwrap();
    assert_eq!(get(&mut s, "SELECT id, nextval('projection_effect'), balance FROM accounts WHERE id BETWEEN 2 AND 4 ORDER BY id"),
        (2..=4).map(|i| vec![Value::Int4(i), Value::Int8((i - 1) as i64), balance(i)]).collect::<Vec<_>>());
    // Evaluated expressions run in order before columns move, including on error.
    s.query("ALTER SEQUENCE projection_effect RESTART WITH 1").unwrap();
    let error = s.query("SELECT id, nextval('projection_effect'), 1 / (id - id), nextval('projection_effect'), balance FROM accounts WHERE id = 2").unwrap_err();
    assert_eq!(error.code, pg_engine::error::code::DIVISION_BY_ZERO);
    assert_eq!(get(&mut s, "SELECT currval('projection_effect')"), vec![vec![Value::Int8(1)]]);
}
