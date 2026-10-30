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
