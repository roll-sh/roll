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
