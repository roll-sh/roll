//! Writes and reads back: INSERT / UPDATE / DELETE semantics, collections, counters, TTL,
//! batches, JSON.

use super::*;

#[tokio::test]
async fn smoke_insert_select() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.ok(KS).await;
        h.ok("CREATE TABLE ks.t (pk int, ck int, v text, PRIMARY KEY (pk, ck))")
            .await;
        h.ok("INSERT INTO ks.t (pk, ck, v) VALUES (1, 1, 'a')")
            .await;
        h.ok("INSERT INTO ks.t (pk, ck, v) VALUES (1, 2, 'b')")
            .await;
        assert_eq!(
            h.rows("SELECT * FROM ks.t WHERE pk = 1").await,
            table(&[&["1", "1", "a"], &["1", "2", "b"]])
        );
        assert_eq!(
            h.rows("SELECT v FROM ks.t WHERE pk = 1 AND ck = 2").await,
            table(&[&["b"]])
        );
    }
}

#[tokio::test]
async fn prepared_writes_with_positional_and_named_values() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (pk int, ck int, v text, PRIMARY KEY (pk, ck))"
        ))
        .await;
        let ins =
            h.s.prepare("INSERT INTO ks.t (pk, ck, v) VALUES (?, ?, ?)", None)
                .await
                .unwrap();
        assert_eq!(ins.pk_indexes, vec![0]);
        for i in 0..3 {
            h.s.execute(
                &ins.id,
                QueryParams::with_values(vec![int(1), int(i), text("x")]),
            )
            .await
            .unwrap();
        }
        let named =
            h.s.prepare("SELECT v FROM ks.t WHERE pk = :p AND ck = :c", None)
                .await
                .unwrap();
        let p = QueryParams {
            values: vec![int(2), int(1)],
            names: Some(vec!["c".into(), "p".into()]),
            ..Default::default()
        };
        let r = h.s.execute(&named.id, p).await.unwrap();
        assert_eq!(fmt_rows(&r), table(&[&["x"]]), "{d}");
        // An unset value leaves the column untouched.
        h.s.execute(
            &ins.id,
            QueryParams::with_values(vec![int(1), int(0), Param::Unset]),
        )
        .await
        .unwrap();
        assert_eq!(
            h.rows("SELECT v FROM ks.t WHERE pk = 1 AND ck = 0").await,
            table(&[&["x"]]),
            "{d}"
        );
        // A null value writes a tombstone.
        h.s.execute(
            &ins.id,
            QueryParams::with_values(vec![int(1), int(0), Param::Null]),
        )
        .await
        .unwrap();
        assert_eq!(
            h.rows("SELECT v FROM ks.t WHERE pk = 1 AND ck = 0").await,
            table(&[&["null"]]),
            "{d}"
        );
    }
}

#[tokio::test]
async fn repeated_named_markers_share_a_variable_in_scylla() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (pk int, ck int, v int, PRIMARY KEY (pk, ck))"
        ))
        .await;
        let p =
            h.s.prepare("SELECT * FROM ks.t WHERE pk = :x AND ck = :x", None)
                .await
                .unwrap();
        assert_eq!(p.bind.len(), if d.is_scylla() { 1 } else { 2 }, "{d}");
    }
}

#[tokio::test]
async fn batches_apply_atomically_with_one_timestamp() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (pk int, ck int, v int, PRIMARY KEY (pk, ck));\nCREATE TABLE ks.c (pk int PRIMARY KEY, n counter)"
        ))
        .await;
        h.ok("BEGIN BATCH INSERT INTO ks.t (pk, ck, v) VALUES (1, 1, 1); INSERT INTO ks.t (pk, ck, v) VALUES (2, 1, 2); APPLY BATCH").await;
        let w = h.rows("SELECT writetime(v) FROM ks.t").await;
        assert_eq!(w.len(), 2);
        assert_eq!(w[0], w[1], "{d}: one timestamp per batch");
        for _ in 0..2 {
            h.ok("BEGIN COUNTER BATCH UPDATE ks.c SET n = n + 1 WHERE pk = 1; UPDATE ks.c SET n = n + 3 WHERE pk = 2; APPLY BATCH").await;
        }
        assert_eq!(h.rows("SELECT pk, n FROM ks.c").await.len(), 2);
        assert_eq!(
            h.rows("SELECT n FROM ks.c WHERE pk = 2").await,
            table(&[&["6"]]),
            "{d}"
        );
        // Protocol batch mixing a prepared and a query statement.
        let p =
            h.s.prepare("INSERT INTO ks.t (pk, ck, v) VALUES (?, ?, ?)", None)
                .await
                .unwrap();
        let b = crate::Batch {
            kind: crate::BatchKind::Logged,
            statements: vec![
                crate::BatchStatement::Prepared {
                    id: p.id.clone(),
                    values: vec![int(3), int(1), int(3)],
                },
                crate::BatchStatement::Query {
                    text: "DELETE FROM ks.t WHERE pk = 1".into(),
                    values: vec![],
                },
            ],
            params: QueryParams::default(),
        };
        h.s.batch(b).await.unwrap();
        assert_eq!(
            h.rows("SELECT pk FROM ks.t WHERE pk IN (1, 2, 3)").await,
            table(&[&["2"], &["3"]]),
            "{d}"
        );
    }
}

#[tokio::test]
async fn collections_and_static_rows() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (pk int, ck int, s int static, l list<int>, st set<text>, m map<text, int>, PRIMARY KEY (pk, ck))"
        ))
        .await;
        h.ok("UPDATE ks.t SET l = l + [1, 2], st = st + {'b', 'a'}, m['k'] = 1 WHERE pk = 1 AND ck = 1").await;
        h.ok("UPDATE ks.t SET l = [0] + l, l[1] = 9, st = st - {'b'}, m = m + {'j': 2} WHERE pk = 1 AND ck = 1").await;
        h.ok("DELETE m['k'] FROM ks.t WHERE pk = 1 AND ck = 1")
            .await;
        assert_eq!(
            h.rows("SELECT l, st, m FROM ks.t").await,
            table(&[&["[0, 1, 9]", "{'a'}", "{'j': 2}"]]),
            "{d}"
        );
        h.ok("UPDATE ks.t SET s = 5 WHERE pk = 1").await;
        h.ok("INSERT INTO ks.t (pk, ck) VALUES (1, 2)").await;
        assert_eq!(
            h.rows("SELECT ck, s FROM ks.t WHERE pk = 1").await,
            table(&[&["1", "5"], &["2", "5"]]),
            "{d}"
        );
        h.ok("DELETE FROM ks.t WHERE pk = 1 AND ck = 1").await;
        assert_eq!(
            h.rows("SELECT ck, s FROM ks.t WHERE pk = 1").await,
            table(&[&["2", "5"]]),
            "{d}"
        );
    }
}

/// Increments of one counter in one batch add up (`CountersTest.testCounterBatch`, both
/// references): later members see the earlier ones' increments.
#[tokio::test]
async fn counter_batch_increments_add_up() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.c (u int, url text, total counter, PRIMARY KEY (u, url));\n\
             CREATE TABLE ks.m (u int, url text, a counter, b counter, c counter, PRIMARY KEY (u, url));\n\
             CREATE TABLE ks.s (p int, c int, s counter static, n counter, PRIMARY KEY (p, c))"
        ))
        .await;
        let one = "UPDATE ks.c SET total = total + 1 WHERE u = 1 AND url = 'foo'";
        h.ok(&format!(
            "BEGIN UNLOGGED BATCH {one}; {one}; {one}; APPLY BATCH"
        ))
        .await;
        h.ok(&format!("BEGIN COUNTER BATCH {one}; {one}; APPLY BATCH"))
            .await;
        h.ok("BEGIN UNLOGGED BATCH UPDATE ks.c SET total = total + 1 WHERE u = 1 AND url = 'bar'; UPDATE ks.c SET total = total + 1 WHERE u = 1 AND url = 'baz'; APPLY BATCH").await;
        assert_eq!(
            h.rows("SELECT url, total FROM ks.c WHERE u = 1").await,
            table(&[&["bar", "1"], &["baz", "1"], &["foo", "5"]]),
            "{d}"
        );
        h.ok("BEGIN UNLOGGED BATCH UPDATE ks.m SET a = a + 1 WHERE u = 1 AND url = 'x'; UPDATE ks.m SET a = a + 1 WHERE u = 1 AND url = 'x'; UPDATE ks.m SET b = b + 1 WHERE u = 1 AND url = 'x'; APPLY BATCH").await;
        h.ok("BEGIN UNLOGGED BATCH UPDATE ks.m SET a = a + 1, b = b + 1 WHERE u = 1 AND url = 'y'; UPDATE ks.m SET a = a - 1, b = b - 1 WHERE u = 1 AND url = 'y'; UPDATE ks.m SET a = a + 5 WHERE u = 2 AND url = 'y'; APPLY BATCH").await;
        assert_eq!(
            h.rows("SELECT u, url, a, b, c FROM ks.m").await,
            table(&[
                &["1", "x", "2", "1", "null"],
                &["1", "y", "0", "0", "null"],
                &["2", "y", "5", "null", "null"]
            ]),
            "{d}"
        );
        h.ok("BEGIN COUNTER BATCH UPDATE ks.s SET s = s + 2 WHERE p = 1; UPDATE ks.s SET s = s + 3, n = n + 1 WHERE p = 1 AND c = 1; UPDATE ks.s SET n = n + 1 WHERE p = 1 AND c = 1; APPLY BATCH").await;
        assert_eq!(
            h.rows("SELECT p, c, s, n FROM ks.s").await,
            table(&[&["1", "1", "5", "2"]]),
            "{d}: static counters"
        );
    }
}

/// A bound frozen set or map is stored sorted and without duplicates, at every nesting level
/// (`Sets.Value.fromSerialized` / `Maps.Value.fromSerialized`), so it matches the literal
/// (`test_frozen_collection.py::test_wrong_set_order*`).
#[tokio::test]
async fn bound_frozen_collections_are_normalised() {
    let ints = |vs: &[i32]| {
        nosql_types::pack_collection(
            vs.iter()
                .map(|v| v.to_be_bytes())
                .collect::<Vec<_>>()
                .iter()
                .map(|b| &b[..]),
        )
    };
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (k frozen<set<int>> PRIMARY KEY, v int);\n\
             CREATE TABLE ks.n (p int, c frozen<list<frozen<set<int>>>>, PRIMARY KEY (p, c));\n\
             CREATE TABLE ks.m (p int PRIMARY KEY, m frozen<map<int, frozen<set<int>>>>)"
        ))
        .await;
        let ins =
            h.s.prepare("INSERT INTO ks.t (k, v) VALUES (?, 1)", None)
                .await
                .unwrap();
        h.s.execute(
            &ins.id,
            QueryParams::with_values(vec![Param::new(ints(&[3, 1, 2, 1]))]),
        )
        .await
        .unwrap();
        assert_eq!(
            h.rows("SELECT v FROM ks.t WHERE k = {1, 2, 3}").await,
            table(&[&["1"]]),
            "{d}"
        );
        assert_eq!(
            h.rows("SELECT k FROM ks.t").await,
            table(&[&["{1, 2, 3}"]]),
            "{d}"
        );
        let inner = [ints(&[2, 1]), ints(&[5, 4, 4])];
        let nested = nosql_types::pack_collection(inner.iter().map(|b| &b[..]));
        let ins =
            h.s.prepare("INSERT INTO ks.n (p, c) VALUES (1, ?)", None)
                .await
                .unwrap();
        h.s.execute(&ins.id, QueryParams::with_values(vec![Param::new(nested)]))
            .await
            .unwrap();
        assert_eq!(
            h.rows("SELECT p FROM ks.n WHERE p = 1 AND c = [{1, 2}, {4, 5}]")
                .await,
            table(&[&["1"]]),
            "{d}: nested"
        );
        let (k2, k1) = (2i32.to_be_bytes(), 1i32.to_be_bytes());
        let (s2, s1) = (ints(&[9, 8]), ints(&[7]));
        let map = nosql_types::pack_map([(&k2[..], &s2[..]), (&k1[..], &s1[..])]);
        let ins =
            h.s.prepare("INSERT INTO ks.m (p, m) VALUES (1, ?)", None)
                .await
                .unwrap();
        h.s.execute(&ins.id, QueryParams::with_values(vec![Param::new(map)]))
            .await
            .unwrap();
        assert_eq!(
            h.rows("SELECT m FROM ks.m").await,
            table(&[&["{1: {7}, 2: {8, 9}}"]]),
            "{d}: map keys and values"
        );
    }
}

/// A tuple literal is a user type value, field by field: in ScyllaDB for any user type, in
/// Cassandra for a frozen one (a non-frozen one fails casting the value, a server error).
/// Probed on both references.
#[tokio::test]
async fn tuple_literals_as_user_type_values() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TYPE ks.u (v int, w text);\nCREATE TABLE ks.t (a frozen<u> PRIMARY KEY, b u)"
        ))
        .await;
        h.ok("INSERT INTO ks.t (a) VALUES ((7))").await;
        assert_eq!(
            h.rows("SELECT a FROM ks.t").await,
            vec![vec!["{v: 7, w: None}".to_string()]],
            "{d}"
        );
        let r = h.run("INSERT INTO ks.t (a, b) VALUES ((8, 'x'), (9, 'y'))").await;
        match d {
            Dialect::Scylla => assert!(r.is_ok(), "{r:?}"),
            Dialect::Cassandra => {
                let e = r.unwrap_err();
                assert_eq!(e.code, ErrorCode::Server);
                assert!(e.message.starts_with("java.lang.ClassCastException: class org.apache.cassandra.cql3.Tuples$Value cannot be cast"));
            }
        }
        let e = h.err("INSERT INTO ks.t (a) VALUES ((1, 'a', 3))").await;
        assert_eq!(
            e.message,
            "Invalid tuple literal for a: too many elements. Type frozen<u> expects 2 but got 3",
            "{d}"
        );
    }
}
