//! Simple queries that differ only in their literals run with a plan kept for their template
//! (`session::litplan`): the same rows and errors as planning each alone, while the plan
//! fits (debug builds check every use against the query's own plan).

mod common;

use common::{engine_session, open_store};
use pg_engine::types::Value;

fn one(s: &mut pg_engine::session::Session, sql: &str) -> Option<Value> {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    r.last().and_then(|q| q.rows.first()).map(|row| row[0].clone())
}

#[test]
fn kept_plans_follow_values_sizes_and_ddl() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = engine_session(open_store(dir.path()), "kept");
    s.query("CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL DEFAULT 0, c int)").unwrap();
    s.query("INSERT INTO t SELECT g, 0, g % 7 FROM generate_series(1, 50) g").unwrap();
    for i in 1..=50 {
        s.query(&format!("UPDATE t SET v = v + {i} WHERE id = {i}")).unwrap();
        s.query(&format!("UPDATE t SET v = v + -{i} WHERE id = {}", 51 - i)).unwrap();
    }
    for i in 1..=50 {
        assert_eq!(one(&mut s, &format!("SELECT v FROM t WHERE id = {i}")), Some(Value::Int4(2 * i - 51)));
    }
    let used = s.kept_plan_uses();
    assert!(used > 100, "kept plans used {used} times");
    // A growing table: planned again as its size changes.
    for i in 51..=2000 {
        s.query(&format!("INSERT INTO t (id, c) VALUES ({i}, {})", i % 7)).unwrap();
    }
    assert_eq!(one(&mut s, "SELECT v FROM t WHERE id = 1999"), Some(Value::Int4(0)));
    // Errors as each query alone gives them.
    assert!(s.query("UPDATE t SET v = v + 2147483000 WHERE id = 1").is_ok());
    let e = s.query("UPDATE t SET v = v + 2147483001 WHERE id = 1").unwrap_err();
    assert!(e.message.contains("integer out of range"), "{e:?}");
    // DDL: no unique index any more.
    s.query("ALTER TABLE t DROP CONSTRAINT t_pkey").unwrap();
    assert_eq!(one(&mut s, "SELECT v FROM t WHERE id = 7"), Some(Value::Int4(-37)));
    assert_eq!(one(&mut s, "SELECT v FROM t WHERE id = 8"), Some(Value::Int4(-35)));
}

/// One extended-protocol round (Parse, Bind, Execute, Sync) of `sql` with text parameters
/// (None: null), named `name` ("": unnamed): the first column of its first row, or the tag.
fn ext(s: &mut pg_engine::session::Session, name: &str, sql: &str, params: &[Option<&str>]) -> Result<String, String> {
    use pg_engine::session::Outcome;
    let r = (|| {
        if name.is_empty() || !s.prepared.contains_key(name) {
            s.parse_message(name, sql, &[])?;
        }
        let vals = params.iter().map(|p| p.map(|v| v.as_bytes().to_vec())).collect();
        s.bind_message("", name, vals, &[], Vec::new())?;
        match s.execute_portal("", 0)?.0 {
            Outcome::Done(q) => Ok(match q.rows.first() {
                Some(row) => format!("{:?}", row[0]),
                None => q.tag,
            }),
            _ => Ok(String::new()),
        }
    })();
    let r = r.map_err(|e: pg_engine::Error| e.message);
    s.sync().unwrap();
    r
}

#[test]
fn extended_custom_plans_run_the_generic_binding() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = engine_session(open_store(dir.path()), "kept");
    s.query("CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL DEFAULT 0, c int)").unwrap();
    s.query("CREATE INDEX ON t (c)").unwrap();
    s.query("INSERT INTO t SELECT g, 0, g % 7 FROM generate_series(1, 50) g").unwrap();
    let upd = "UPDATE t SET v = v + $1 WHERE id = $2";
    for i in 1..=50 {
        assert_eq!(ext(&mut s, "", upd, &[Some(&i.to_string()), Some(&i.to_string())]).unwrap(), "UPDATE 1");
    }
    let used = s.generic_custom_uses();
    assert!(used > 40, "custom plans as the generic binding: {used}");
    for i in [1, 7, 50] {
        let v = ext(&mut s, "", "SELECT v FROM t WHERE id = $1", &[Some(&i.to_string())]).unwrap();
        assert_eq!(v, format!("{:?}", Value::Int4(i)));
    }
    // A null value: its own custom plan (the comparison folds to null).
    assert_eq!(ext(&mut s, "", upd, &[Some("1"), None]).unwrap(), "UPDATE 0");
    // Errors as each custom plan gives them.
    assert!(ext(&mut s, "", upd, &[Some("2147483000"), Some("3")]).is_ok());
    let e = ext(&mut s, "", upd, &[Some("2147483000"), Some("3")]).unwrap_err();
    assert!(e.contains("integer out of range"), "{e}");
    // A column without a unique index or a most-common-values list: one estimate for every
    // value, so its plan is kept too (debug builds check each use against its own plan).
    let kept = s.kept_plan_uses();
    for c in 0..7 {
        let want = (1..=50).filter(|g| g % 7 == c).count() as i64;
        let n = ext(&mut s, "", "SELECT count(*) FROM t WHERE c = $1", &[Some(&c.to_string())]).unwrap();
        assert_eq!(n, format!("{:?}", Value::Int8(want)));
        assert_eq!(one(&mut s, &format!("SELECT count(*) FROM t WHERE c = {c}")), Some(Value::Int8(want)));
    }
    assert!(s.kept_plan_uses() > kept, "count(*) by a non-unique column kept no plan");
    // ANALYZE gives it one: planned with each value again.
    s.query("ANALYZE t").unwrap();
    for c in 0..7 {
        let want = (1..=50).filter(|g| g % 7 == c).count() as i64;
        assert_eq!(one(&mut s, &format!("SELECT count(*) FROM t WHERE c = {c}")), Some(Value::Int8(want)));
        let n = ext(&mut s, "", "SELECT count(*) FROM t WHERE c = $1", &[Some(&c.to_string())]).unwrap();
        assert_eq!(n, format!("{:?}", Value::Int8(want)));
    }
    // A named statement: five custom plans, then the generic one, as PostgreSQL counts them.
    for i in 1..=8 {
        ext(&mut s, "q", "SELECT v FROM t WHERE id = $1", &[Some(&i.to_string())]).unwrap();
    }
    let counts = s
        .query("SELECT generic_plans, custom_plans FROM pg_prepared_statements WHERE name = 'q'")
        .unwrap();
    let row = &counts.last().unwrap().rows[0];
    assert_eq!((row[0].clone(), row[1].clone()), (Value::Int8(3), Value::Int8(5)));
    // A growing table: kept data-modifying plans planned again (debug builds check each
    // against binding the statement again).
    for i in 51..=1500 {
        s.query(&format!("INSERT INTO t (id, c) VALUES ({i}, {})", i % 7)).unwrap();
        if i % 10 == 0 {
            s.query(&format!("UPDATE t SET v = v + 1 WHERE id = {}", i - 5)).unwrap();
            assert_eq!(ext(&mut s, "", upd, &[Some("1"), Some(&(i - 5).to_string())]).unwrap(), "UPDATE 1");
        }
    }
    assert_eq!(one(&mut s, "SELECT v FROM t WHERE id = 1495"), Some(Value::Int4(2)));
}

/// Queries whose literals have other lengths are one shape (`literals::Scan::shape`): one
/// plan kept for them all, and each query's errors at its own positions.
#[test]
fn kept_plans_serve_literals_of_any_length() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = engine_session(open_store(dir.path()), "shapes");
    s.query("CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL DEFAULT 0)").unwrap();
    s.query("INSERT INTO t SELECT g, g * 2 FROM generate_series(1, 20000) g").unwrap();
    // (Learned the second time the shape is seen.)
    assert_eq!(one(&mut s, "SELECT v FROM t WHERE id = 1"), Some(Value::Int4(2)));
    assert_eq!(one(&mut s, "SELECT v FROM t WHERE id = 22"), Some(Value::Int4(44)));
    let used = s.kept_plan_uses();
    for i in [333, 4444, 7, 19999, 55] {
        assert_eq!(one(&mut s, &format!("SELECT v FROM t WHERE id = {i}")), Some(Value::Int4(2 * i)));
        s.query(&format!("UPDATE t SET v = v + {} WHERE id = {i}", -i)).unwrap();
        s.query(&format!("UPDATE t SET v = v + {i} WHERE id = {i}")).unwrap();
    }
    // (Each UPDATE shape is learned the second time it is seen and kept from the third: 5 + 3 + 3.)
    assert!(s.kept_plan_uses() >= used + 11, "kept plans used {} times", s.kept_plan_uses() - used);
    for (id, want) in [(1, 38), (22222, 42)] {
        let q = format!("SELECT v FROM t WHERE id = {id} AND v = 'x'");
        let e = s.query(&q).unwrap_err();
        assert_eq!(e.position, Some(want), "{q}: {e:?}");
    }
}

#[test]
fn kept_plans_follow_size_changes_across_sessions() {
    use pg_engine::session::{Session, state::SessionState};

    let dir = tempfile::tempdir().unwrap();
    let mut a = engine_session(open_store(dir.path()), "shared");
    let mut b = Session::new(SessionState::new(a.state.db.clone(), a.state.cluster.clone(), "postgres", 4243));
    a.query("CREATE TABLE t (id int PRIMARY KEY, v int)").unwrap();
    a.query("INSERT INTO t SELECT g, g * 2 FROM generate_series(1, 50) g").unwrap();
    for s in [&mut a, &mut b] {
        assert_eq!(one(s, "SELECT v FROM t WHERE id = 1"), Some(Value::Int4(2)));
        assert_eq!(one(s, "SELECT v FROM t WHERE id = 22"), Some(Value::Int4(44)));
    }
    // Each growth invalidates both sessions' plans. The first replan can be shared with
    // the second session, whose literals have different lengths.
    for end in [5000, 10000] {
        let start = if end == 5000 { 51 } else { 5001 };
        a.query(&format!("INSERT INTO t SELECT g, g * 2 FROM generate_series({start}, {end}) g")).unwrap();
        assert_eq!(one(&mut a, &format!("SELECT v FROM t WHERE id = {end}")), Some(Value::Int4(end * 2)));
        assert_eq!(one(&mut b, "SELECT v FROM t WHERE id = 7"), Some(Value::Int4(14)));
    }
    b.query("SET enable_indexscan = off").unwrap();
    assert_eq!(one(&mut b, "SELECT v FROM t WHERE id = 9999"), Some(Value::Int4(19998)));
    b.query("RESET enable_indexscan").unwrap();
    b.query("CREATE TEMP TABLE t (id int PRIMARY KEY, v int)").unwrap();
    b.query("INSERT INTO t VALUES (7, 777)").unwrap();
    for _ in 0..3 {
        assert_eq!(one(&mut b, "SELECT v FROM t WHERE id = 7"), Some(Value::Int4(777)));
        assert_eq!(one(&mut a, "SELECT v FROM t WHERE id = 7"), Some(Value::Int4(14)));
    }
    a.query("ALTER TABLE t ALTER COLUMN v TYPE bigint").unwrap();
    assert_eq!(one(&mut a, "SELECT v FROM t WHERE id = 7"), Some(Value::Int8(14)));
    b.query("DROP TABLE t").unwrap();
    assert_eq!(one(&mut b, "SELECT v FROM t WHERE id = 7"), Some(Value::Int8(14)));
}

/// Bind makes the custom plan (GetCachedPlan) before it checks the result formats
/// (PortalSetResultFormat): a parameter that fails to fold is that error, not 08P01.
#[test]
fn bind_plans_before_checking_result_formats() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = engine_session(open_store(dir.path()), "bindfmt");
    let q = "SELECT $1::int8 + 1, $2::int2, $3::int8 * 2";
    let vals = |v: &str| vec![Some(b"1".to_vec()), Some(b"2".to_vec()), Some(v.as_bytes().to_vec())];
    s.parse_message("", q, &[]).unwrap();
    let e = s.bind_message("", "", vals("-9223372036854775808"), &[], vec![0, 0]).unwrap_err();
    assert_eq!(e.code, "22003", "{e:?}");
    s.sync().unwrap();
    s.parse_message("", q, &[]).unwrap();
    let e = s.bind_message("", "", vals("5"), &[], vec![0, 0]).unwrap_err();
    assert_eq!(e.code, "08P01", "{e:?}");
    s.sync().unwrap();
}

/// PL/pgSQL statements whose variables can't change their plan run the generic plan as
/// their first five custom plans (debug builds bind each custom plan too and assert it is
/// that plan); others, and variables that are also column names, bind each custom plan.
#[test]
fn plpgsql_custom_plans_as_the_generic_plan() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = engine_session(open_store(dir.path()), "plcustom");
    s.query("CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL DEFAULT 0, c int)").unwrap();
    s.query("INSERT INTO t SELECT g, 0, g % 3 FROM generate_series(1, 300) g").unwrap();
    s.query("ANALYZE t").unwrap();
    s.query(
        "CREATE FUNCTION bump(k int, d int) RETURNS int LANGUAGE plpgsql AS $$
         DECLARE r int; n bigint; c int := 1;
         BEGIN
           UPDATE t SET v = v + d WHERE id = k;
           SELECT v INTO r FROM t WHERE id = k;
           SELECT count(*) INTO n FROM t WHERE t.c = (k % 3);
           RETURN r + n::int;
         END $$",
    )
    .unwrap();
    for mode in ["auto", "force_custom_plan"] {
        s.query(&format!("SET plan_cache_mode = {mode}")).unwrap();
        for k in 1..=12 {
            let want = s.query(&format!("SELECT v + {k} FROM t WHERE id = {k}")).unwrap().last().unwrap().rows[0][0].clone();
            let n = (1..=300).filter(|g| g % 3 == k % 3).count() as i32;
            let Value::Int4(w) = want else { panic!() };
            assert_eq!(one(&mut s, &format!("SELECT bump({k}, {k})")), Some(Value::Int4(w + n)), "{mode} {k}");
        }
    }
    // A null value folds the comparison: planned with it.
    assert_eq!(one(&mut s, "SELECT bump(NULL, 1)"), Some(Value::Null));
}

/// Custom plans made from the Parse's analysis with the values put in
/// (`Analyzed::bind_values`), as PostgreSQL plans its cached query tree: the range read
/// whose estimates follow its values, quals ordered by their folded cost, and what binding
/// the values would fold otherwise (an overflow, a null, two constants for one key) bound
/// afresh. Debug builds check each against binding the values.
#[test]
fn custom_plans_from_the_analysis() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = engine_session(open_store(dir.path()), "values");
    s.query("CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL, w int)").unwrap();
    s.query("INSERT INTO t SELECT g, g % 13, g % 5 FROM generate_series(1, 2000) g").unwrap();
    s.query("ANALYZE t").unwrap();
    let sum = |lo: i64, hi: i64| (lo..=hi.min(2000)).map(|g| g % 13).sum::<i64>();
    let used = s.analysis_custom_uses();
    let range = "SELECT sum(v) FROM t WHERE id BETWEEN $1 AND $2 + 99";
    for lo in [1, 17, 400, 1950] {
        let got = ext(&mut s, "", range, &[Some(&lo.to_string()), Some(&lo.to_string())]).unwrap();
        assert_eq!(got, format!("{:?}", Value::Int8(sum(lo, lo + 99))));
    }
    assert!(s.analysis_custom_uses() >= used + 3, "{}", s.analysis_custom_uses() - used);
    // Written with the folded bound first: the quals ordered by their custom costs.
    let rev = "SELECT count(*) FROM t WHERE id <= $1 + 9 AND id >= $1 AND w <> $2";
    for lo in [3, 900] {
        let want = (lo..=lo + 9).filter(|g| g % 5 != 1).count() as i64;
        let got = ext(&mut s, "", rev, &[Some(&lo.to_string()), Some("1")]).unwrap();
        assert_eq!(got, format!("{:?}", Value::Int8(want)));
    }
    // An overflow folding the bound: the custom plan's error.
    let e = ext(&mut s, "", range, &[Some("1"), Some("2147483600")]).unwrap_err();
    assert!(e.contains("integer out of range"), "{e}");
    // A null: the comparison folds away.
    assert_eq!(ext(&mut s, "", range, &[None, Some("5")]).unwrap(), format!("{:?}", Value::Null));
    // Two constants for one key: no rows (const_conflict).
    let two = "SELECT count(*) FROM t WHERE id = $1 AND id = $2";
    assert_eq!(ext(&mut s, "", two, &[Some("4"), Some("5")]).unwrap(), format!("{:?}", Value::Int8(0)));
    assert_eq!(ext(&mut s, "", two, &[Some("4"), Some("4")]).unwrap(), format!("{:?}", Value::Int8(1)));
    // The value in the select list too, and a named statement's plans counted as
    // PostgreSQL counts them.
    let named = "SELECT $1 + max(v) FROM t WHERE id BETWEEN $2 AND $2 + 30";
    for i in 1..=8 {
        let got = ext(&mut s, "r", named, &[Some("1000"), Some(&(i * 100).to_string())]).unwrap();
        assert_eq!(got, format!("{:?}", Value::Int4(1012)));
    }
    let counts = s
        .query("SELECT generic_plans + custom_plans, custom_plans >= 5 FROM pg_prepared_statements WHERE name = 'r'")
        .unwrap();
    let row = &counts.last().unwrap().rows[0];
    assert_eq!((row[0].clone(), row[1].clone()), (Value::Int8(8), Value::Bool(true)));
    // DDL since: the next Parse's analysis.
    s.query("CREATE INDEX ON t (w)").unwrap();
    let got = ext(&mut s, "", range, &[Some("101"), Some("101")]).unwrap();
    assert_eq!(got, format!("{:?}", Value::Int8(sum(101, 200))));
    // Simple queries: their template's analysis with each query's literals put in; a
    // LIMIT's literal stays a constant, a negative one is its value.
    let used = s.analysis_query_uses();
    for lo in [7, 70, 700, 1500, -40] {
        let q = format!("SELECT sum(v) FROM t WHERE id BETWEEN {lo} AND {lo} + 99");
        assert_eq!(one(&mut s, &q), Some(Value::Int8(sum(lo.max(1), lo + 99))));
        let q = format!("SELECT id FROM t WHERE id > {lo} AND w = 2 ORDER BY id LIMIT 3");
        let want = (lo.max(0) + 1..).find(|g| g % 5 == 2).unwrap() as i32;
        assert_eq!(one(&mut s, &q), Some(Value::Int4(want)));
    }
    assert!(s.analysis_query_uses() >= used + 4, "{}", s.analysis_query_uses() - used);
    // An overflow folding a bound: the query's own error.
    let e = s.query("SELECT sum(v) FROM t WHERE id BETWEEN 1 AND 2147483600 + 99").unwrap_err();
    assert!(e.message.contains("integer out of range"), "{}", e.message);
}


#[test]
fn generic_cost_uses_choice_context_after_parse() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = engine_session(open_store(dir.path()), "choice_context");
    s.query("CREATE TABLE t (a int, b text); INSERT INTO t SELECT g, g::text FROM generate_series(1,100) g; ANALYZE t").unwrap();
    s.query("SET seq_page_cost = 500").unwrap();
    s.parse_message("choice_context", "SELECT b FROM t WHERE a = $1", &[]).unwrap();
    s.sync().unwrap();
    s.query("RESET seq_page_cost").unwrap();
    for _ in 0..6 {
        assert_eq!(ext(&mut s, "choice_context", "SELECT b FROM t WHERE a = $1", &[Some("1")]).unwrap(), format!("{:?}", Value::Text("1".into())));
    }
    let counts = s.query("SELECT generic_plans, custom_plans FROM pg_prepared_statements WHERE name = 'choice_context'").unwrap();
    let row = &counts.last().unwrap().rows[0];
    println!("Parse seq_page_cost=500; six Binds after RESET: {row:?}");
    assert_eq!((row[0].clone(), row[1].clone()), (Value::Int8(1), Value::Int8(5)));
}

/// Rechosen custom scans return every ordered row and value, including after parameter,
/// data, snapshot and planning-environment changes. Counters only establish reuse coverage.
#[test]
fn custom_plans_choose_the_scan_again() {
    use pg_engine::session::{Outcome, QueryResult, Session, state::SessionState};
    use std::collections::BTreeMap;

    let dir = tempfile::tempdir().unwrap();
    let mut s = engine_session(open_store(dir.path()), "rechoose");
    s.query("CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL)").unwrap();
    s.query("INSERT INTO t SELECT g, g % 13 FROM generate_series(1, 5000) g").unwrap();
    s.query("ANALYZE t").unwrap();
    let mut data: BTreeMap<i32, i32> = (1..=5000).map(|id| (id, id % 13)).collect();
    let range = "SELECT id, v FROM t WHERE id BETWEEN $1 AND $2 ORDER BY id";
    let check = |q: QueryResult, data: &BTreeMap<i32, i32>, lo: Option<i32>, hi: Option<i32>| {
        let want: Vec<Vec<Value>> = data.iter().filter(|&(&id, _)| {
            lo.is_some_and(|lo| id >= lo) && hi.is_some_and(|hi| id <= hi)
        }).map(|(&id, &v)| vec![Value::Int4(id), Value::Int4(v)]).collect();
        assert_eq!(q.rows, want, "complete ordered result for {lo:?}..={hi:?}");
        assert_eq!(q.tag, format!("SELECT {}", want.len()));
    };
    // Parse once; every later call binds new values to the same named statement.
    s.parse_message("rr", range, &[]).unwrap();
    s.sync().unwrap();
    let run = |s: &mut Session, data: &BTreeMap<i32, i32>, lo: Option<i32>, hi: Option<i32>| {
        let params = [lo, hi].into_iter().map(|v| v.map(|v| v.to_string().into_bytes())).collect();
        s.bind_message("", "rr", params, &[], Vec::new()).unwrap();
        let Outcome::Done(q) = s.execute_portal("", 0).unwrap().0 else { panic!("expected SELECT") };
        s.sync().unwrap();
        check(q, data, lo, hi);
    };
    let used = s.rechosen_custom_uses();
    for (lo, hi) in [(1, 100), (17, 116), (400, 499), (4950, 5049), (30, 129)] {
        run(&mut s, &data, Some(lo), Some(hi));
    }
    assert!(s.rechosen_custom_uses() >= used + 3, "{}", s.rechosen_custom_uses() - used);
    s.query("SET plan_cache_mode = force_custom_plan").unwrap();
    // A whole-table path, clipped/negative bounds, reversed bounds, then nonempty again.
    for (lo, hi) in [(1, 5000), (2, 101), (-50, 3), (100, 99), (2500, 2600)] {
        run(&mut s, &data, Some(lo), Some(hi));
    }
    let counts = s.query("SELECT generic_plans, custom_plans FROM pg_prepared_statements WHERE name = 'rr'").unwrap();
    assert_eq!(counts.last().unwrap().rows[0], vec![Value::Int8(0), Value::Int8(10)]);
    for (lo, hi) in [(None, Some(100)), (Some(1), None), (None, None), (Some(5001), Some(5100)), (Some(45), Some(145))] {
        run(&mut s, &data, lo, hi);
    }
    s.query("PREPARE rx (int, int) AS SELECT id, v FROM t WHERE id BETWEEN $1 AND $2 ORDER BY id").unwrap();
    for (lo, hi) in [(5, 104), (50, 149), (500, 599), (100, 99), (4998, 5100), (1, 5000)] {
        let q = s.query(&format!("EXECUTE rx({lo}, {hi})")).unwrap().pop().unwrap();
        check(q, &data, Some(lo), Some(hi));
    }
    let plan = s.query("EXPLAIN (COSTS OFF) EXECUTE rx(7, 106)").unwrap();
    let text: Vec<String> = plan.last().unwrap().rows.iter().map(|r| format!("{:?}", r[0])).collect();
    assert!(text.iter().any(|l| l.contains("(id >= 7) AND (id <= 106)")), "{text:?}");
    run(&mut s, &data, Some(7), Some(106));
    // Size, statistics, DDL and settings invalidate retained planning assumptions.
    s.query("INSERT INTO t SELECT g, g % 13 FROM generate_series(5001, 9000) g").unwrap();
    data.extend((5001..=9000).map(|id| (id, id % 13)));
    run(&mut s, &data, Some(8990), Some(9089));
    for sql in ["ANALYZE t", "CREATE INDEX ON t (v)", "SET enable_indexscan = off", "RESET enable_indexscan"] {
        s.query(sql).unwrap();
        run(&mut s, &data, Some(3), Some(102));
        run(&mut s, &data, Some(4), Some(103));
    }
    let q = s.query("EXECUTE rx(8990, 9089)").unwrap().pop().unwrap();
    check(q, &data, Some(8990), Some(9089));

    let mut writer = Session::new(SessionState::new(s.state.db.clone(), s.state.cluster.clone(), "postgres", 4243));
    for level in ["READ COMMITTED", "REPEATABLE READ"] {
        let used = s.rechosen_custom_uses();
        s.query(&format!("BEGIN ISOLATION LEVEL {level}")).unwrap();
        run(&mut s, &data, Some(40), Some(45)); // establish the transaction snapshot
        run(&mut s, &data, Some(9001), Some(9003)); // empty before the writer's insert
        writer.query("BEGIN; UPDATE t SET v = -4200 WHERE id = 42; DELETE FROM t WHERE id = 43; INSERT INTO t VALUES (9002, -9002)").unwrap();
        // Neither isolation level sees another transaction's uncommitted changes.
        run(&mut s, &data, Some(40), Some(45));
        run(&mut s, &data, Some(9001), Some(9003));
        writer.query("COMMIT").unwrap();
        let mut committed = data.clone();
        committed.insert(42, -4200);
        committed.remove(&43);
        committed.insert(9002, -9002);
        let visible = if level == "READ COMMITTED" { &committed } else { &data };
        run(&mut s, visible, Some(40), Some(45));
        run(&mut s, visible, Some(9001), Some(9003));
        s.query("COMMIT").unwrap();
        run(&mut s, &committed, Some(40), Some(45));
        run(&mut s, &committed, Some(9001), Some(9003));
        // Own writes are visible to reuse, and rollback restores complete results.
        s.query("BEGIN; UPDATE t SET v = -4400 WHERE id = 44; DELETE FROM t WHERE id = 9002").unwrap();
        let mut own = committed.clone();
        own.insert(44, -4400);
        own.remove(&9002);
        run(&mut s, &own, Some(40), Some(45));
        run(&mut s, &own, Some(9001), Some(9003));
        s.query("ROLLBACK").unwrap();
        run(&mut s, &committed, Some(40), Some(45));
        run(&mut s, &committed, Some(9001), Some(9003));
        writer.query("UPDATE t SET v = 3 WHERE id = 42; INSERT INTO t VALUES (43, 4); DELETE FROM t WHERE id = 9002").unwrap();
        run(&mut s, &data, Some(40), Some(45));
        run(&mut s, &data, Some(9001), Some(9003));
        assert!(s.rechosen_custom_uses() > used, "no custom-plan reuse during {level}");
    }
    s.query("ALTER TABLE t DROP CONSTRAINT t_pkey").unwrap();
    run(&mut s, &data, Some(3), Some(102));
    run(&mut s, &data, Some(4), Some(103));
}
