//! Drops stream their deletes (`Txn::delete_range`): DROP INDEX and TRUNCATE (DROP TABLE's)
//! delete every record of what they drop, a chunk at a time, past the chunk size, inside and
//! outside transaction blocks, rolled back or not, and with the transaction's own writes in
//! the range.

mod common;
use common::{engine_session, open_store};

fn one(s: &mut pg_engine::session::Session, sql: &str) -> String {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    format!("{:?}", r.last().unwrap().rows)
}

/// Keys the store holds at its latest commit.
fn keys(store: &pg_engine::storage::Store) -> usize {
    store.scan_raw(&[], &[], store.visible_ts()).count()
}

#[test]
fn drops_delete_every_record() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path());
    let mut s = engine_session(store.clone(), "d");
    s.query("CREATE TABLE t (a int PRIMARY KEY, b int, c text)").unwrap();
    s.query("INSERT INTO t SELECT i, i % 100, 'x' || i FROM generate_series(1, 12000) i").unwrap();
    s.query("CREATE INDEX t_b ON t (b)").unwrap();
    s.query("CREATE INDEX t_c ON t (c)").unwrap();
    let before = keys(&store);

    // Rolled back: nothing goes.
    s.query("BEGIN").unwrap();
    s.query("DROP INDEX t_b").unwrap();
    s.query("ROLLBACK").unwrap();
    assert_eq!(one(&mut s, "SELECT count(*) FROM t WHERE b = 7"), "[[Int8(120)]]");
    assert_eq!(keys(&store), before);

    // Its 12,000 entries go (three chunks), and only they.
    s.query("DROP INDEX t_b").unwrap();
    let after = keys(&store);
    assert!(before - after >= 12_000 && before - after < 12_100, "{before} -> {after}");
    assert_eq!(one(&mut s, "SELECT count(*) FROM t WHERE c = 'x77'"), "[[Int8(1)]]");

    // TRUNCATE with the transaction's own rows in the range, then a savepoint rolled back.
    s.query("BEGIN").unwrap();
    s.query("INSERT INTO t SELECT i, 1, 'y' FROM generate_series(20001, 20500) i").unwrap();
    s.query("TRUNCATE t").unwrap();
    assert_eq!(one(&mut s, "SELECT count(*) FROM t"), "[[Int8(0)]]");
    s.query("INSERT INTO t VALUES (1, 1, 'z')").unwrap();
    s.query("SAVEPOINT p").unwrap();
    s.query("TRUNCATE t").unwrap();
    s.query("ROLLBACK TO p").unwrap();
    assert_eq!(one(&mut s, "SELECT a, c FROM t"), "[[Int4(1), Text(\"z\")]]");
    s.query("COMMIT").unwrap();
    assert_eq!(one(&mut s, "SELECT count(*), min(c) FROM t WHERE c = 'z'"), "[[Int8(1), Text(\"z\")]]");

    // DROP TABLE: every record of it.
    s.query("INSERT INTO t SELECT i, i, 'w' FROM generate_series(2, 9000) i").unwrap();
    let full = keys(&store);
    s.query("DROP TABLE t").unwrap();
    assert!(full - keys(&store) >= 9000 * 3, "{full} -> {}", keys(&store));
}
