//! Derived data behaviour the captures can't show: index and view maintenance across writes
//! (batches, LWT), paging through index results with prepared statements, backfill of flushed
//! data, persistence across a reopen, and Cassandra materialized views (captured live in
//! `captured_views`).

use std::sync::Arc;

use nosql_storage::store::Store;

use super::*;

/// A database on `store` with Cassandra's `materialized_views_enabled` on.
async fn open(store: Arc<Store>, d: Dialect) -> H {
    let handle = store
        .open("db", nosql_storage::db::DbConfig::default())
        .await
        .unwrap();
    let mut cfg = DbConfig::new("db", d);
    cfg.settings.materialized_views_enabled = true;
    let db = Database::open(cfg, handle).await.unwrap();
    let s = db.connect(Default::default());
    H { db, s, store }
}

async fn fresh(d: Dialect) -> H {
    let store = Store::start(nosql_storage::test_objects("engine"), small_store_config())
        .await
        .unwrap();
    open(store, d).await
}

/// Rows of `cql`, sorted (order-insensitive checks).
async fn sorted(h: &mut H, cql: &str) -> Vec<Vec<String>> {
    let mut r = h.rows(cql).await;
    r.sort();
    r
}

#[tokio::test]
async fn index_follows_every_write_kind() {
    for d in BOTH {
        let mut h = fresh(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (pk int, ck int, v int, PRIMARY KEY (pk, ck));\n\
             INSERT INTO ks.t (pk, ck, v) VALUES (1, 1, 10);\n\
             INSERT INTO ks.t (pk, ck, v) VALUES (2, 1, 10);\n\
             INSERT INTO ks.t (pk, ck, v) VALUES (3, 1, 20);\n\
             CREATE INDEX ON ks.t (v);\n\
             INSERT INTO ks.t (pk, ck, v) VALUES (4, 5, 10)"
        ))
        .await;
        let q10 = "SELECT pk, ck FROM ks.t WHERE v = 10";
        assert_eq!(
            sorted(&mut h, q10).await,
            table(&[&["1", "1"], &["2", "1"], &["4", "5"]]),
            "{d}: backfill and insert"
        );
        h.ok("UPDATE ks.t SET v = 20 WHERE pk = 1 AND ck = 1").await;
        assert_eq!(
            sorted(&mut h, q10).await,
            table(&[&["2", "1"], &["4", "5"]]),
            "{d}"
        );
        assert_eq!(
            sorted(&mut h, "SELECT pk FROM ks.t WHERE v = 20").await,
            table(&[&["1"], &["3"]]),
            "{d}: update"
        );
        // Deletes of a cell, a row, a range and a partition.
        h.ok("DELETE v FROM ks.t WHERE pk = 2 AND ck = 1").await;
        h.ok("DELETE FROM ks.t WHERE pk = 3 AND ck = 1").await;
        h.ok("DELETE FROM ks.t WHERE pk = 4 AND ck >= 0").await;
        assert_eq!(h.rows(q10).await, Vec::<Vec<String>>::new(), "{d}: deletes");
        assert_eq!(
            h.rows("SELECT pk FROM ks.t WHERE v = 20").await,
            table(&[&["1"]]),
            "{d}"
        );
        h.ok("DELETE FROM ks.t WHERE pk = 1").await;
        assert_eq!(
            h.rows("SELECT pk FROM ks.t WHERE v = 20").await.len(),
            0,
            "{d}"
        );
        // Batches, conditional writes, TTL.
        h.ok("BEGIN BATCH INSERT INTO ks.t (pk, ck, v) VALUES (5, 1, 30); INSERT INTO ks.t (pk, ck, v) VALUES (6, 1, 30); APPLY BATCH")
            .await;
        h.ok("INSERT INTO ks.t (pk, ck, v) VALUES (7, 1, 30) IF NOT EXISTS")
            .await;
        h.ok("UPDATE ks.t SET v = 40 WHERE pk = 5 AND ck = 1 IF v = 30")
            .await;
        assert_eq!(
            sorted(&mut h, "SELECT pk FROM ks.t WHERE v = 30").await,
            table(&[&["6"], &["7"]]),
            "{d}: batch and LWT"
        );
        assert_eq!(
            h.rows("SELECT pk FROM ks.t WHERE v = 40").await,
            table(&[&["5"]]),
            "{d}"
        );
        h.ok("TRUNCATE ks.t").await;
        assert_eq!(
            h.rows("SELECT pk FROM ks.t WHERE v = 30").await.len(),
            0,
            "{d}: truncate"
        );
    }
}

#[tokio::test]
async fn collection_and_static_indexes_follow_writes() {
    for d in BOTH {
        let mut h = fresh(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (pk int, ck int, s int static, l list<int>, m map<text, int>, PRIMARY KEY (pk, ck));\n\
             CREATE INDEX ON ks.t (l);\nCREATE INDEX ON ks.t (KEYS(m));\nCREATE INDEX ON ks.t (s);\n\
             INSERT INTO ks.t (pk, ck, s, l, m) VALUES (1, 1, 7, [1, 2], {{'a': 1}});\n\
             INSERT INTO ks.t (pk, ck, l) VALUES (1, 2, [2])"
        ))
        .await;
        assert_eq!(
            h.rows("SELECT pk, ck FROM ks.t WHERE l CONTAINS 2").await,
            table(&[&["1", "1"], &["1", "2"]]),
            "{d}"
        );
        h.ok("UPDATE ks.t SET l = l - [2] WHERE pk = 1 AND ck = 1")
            .await;
        h.ok("UPDATE ks.t SET m['b'] = 2 WHERE pk = 1 AND ck = 2")
            .await;
        assert_eq!(
            h.rows("SELECT pk, ck FROM ks.t WHERE l CONTAINS 2").await,
            table(&[&["1", "2"]]),
            "{d}: element removal"
        );
        assert_eq!(
            h.rows("SELECT pk, ck FROM ks.t WHERE m CONTAINS KEY 'b'")
                .await,
            table(&[&["1", "2"]]),
            "{d}: element addition"
        );
        assert_eq!(
            h.rows("SELECT pk, ck, s FROM ks.t WHERE s = 7").await,
            table(&[&["1", "1", "7"], &["1", "2", "7"]]),
            "{d}: static"
        );
        h.ok("UPDATE ks.t SET s = 8 WHERE pk = 1").await;
        assert_eq!(
            h.rows("SELECT pk FROM ks.t WHERE s = 7").await.len(),
            0,
            "{d}"
        );
        assert_eq!(
            h.rows("SELECT pk FROM ks.t WHERE s = 8").await.len(),
            2,
            "{d}"
        );
    }
}

#[tokio::test]
async fn index_pages_continue_with_prepared_statements() {
    for d in BOTH {
        let mut h = fresh(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (pk int, ck int, v int, PRIMARY KEY (pk, ck));\nCREATE INDEX ON ks.t (v)"
        ))
        .await;
        for pk in 0..12 {
            for ck in 0..3 {
                h.ok(&format!(
                    "INSERT INTO ks.t (pk, ck, v) VALUES ({pk}, {ck}, {})",
                    pk % 2
                ))
                .await;
            }
        }
        let want = h
            .rows(
                "SELECT pk, ck FROM ks.t WHERE v = ?"
                    .replace('?', "1")
                    .as_str(),
            )
            .await;
        assert_eq!(want.len(), 18, "{d}");
        let p =
            h.s.prepare("SELECT pk, ck FROM ks.t WHERE v = ?", None)
                .await
                .unwrap();
        let mut got = Vec::new();
        let mut state = None;
        for _ in 0..20 {
            let r =
                h.s.execute(
                    &p.id,
                    QueryParams {
                        values: vec![int(1)],
                        page_size: Some(4),
                        paging_state: state.take(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            got.extend(fmt_rows(&r));
            state = r.rows().and_then(|r| r.paging_state.clone());
            if state.is_none() {
                break;
            }
        }
        assert_eq!(got, want, "{d}: pages add up to the unpaged result");
    }
}

#[tokio::test]
async fn backfill_reads_flushed_data() {
    for d in BOTH {
        let mut h = fresh(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (pk int, ck int, v int, PRIMARY KEY (pk, ck));\n\
             INSERT INTO ks.t (pk, ck, v) VALUES (1, 1, 5);\nINSERT INTO ks.t (pk, ck, v) VALUES (2, 2, 5)"
        ))
        .await;
        h.db.flush().await.unwrap();
        h.ok("INSERT INTO ks.t (pk, ck, v) VALUES (3, 3, 5)").await;
        h.ok("CREATE INDEX ON ks.t (v)").await;
        h.ok("CREATE MATERIALIZED VIEW ks.mv AS SELECT * FROM ks.t WHERE v IS NOT NULL AND pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (v, pk, ck)")
            .await;
        assert_eq!(
            sorted(&mut h, "SELECT pk FROM ks.t WHERE v = 5").await,
            table(&[&["1"], &["2"], &["3"]]),
            "{d}: index"
        );
        assert_eq!(
            h.rows("SELECT pk, ck FROM ks.mv WHERE v = 5").await,
            table(&[&["1", "1"], &["2", "2"], &["3", "3"]]),
            "{d}: view"
        );
    }
}

#[tokio::test]
async fn materialized_views_follow_writes() {
    for d in BOTH {
        let mut h = fresh(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (pk int, ck int, v int, w int, PRIMARY KEY (pk, ck));\n\
             CREATE MATERIALIZED VIEW ks.mv AS SELECT * FROM ks.t WHERE v IS NOT NULL AND pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (v, pk, ck);\n\
             INSERT INTO ks.t (pk, ck, v, w) VALUES (1, 1, 10, 100);\n\
             INSERT INTO ks.t (pk, ck, v, w) VALUES (1, 2, 10, 200)"
        ))
        .await;
        let q = "SELECT v, pk, ck, w FROM ks.mv";
        assert_eq!(
            h.rows(q).await,
            table(&[&["10", "1", "1", "100"], &["10", "1", "2", "200"]]),
            "{d}"
        );
        // Moving a row to another view key, a regular column, a delete, TTL.
        h.ok("UPDATE ks.t SET v = 20 WHERE pk = 1 AND ck = 1").await;
        h.ok("UPDATE ks.t SET w = 201 WHERE pk = 1 AND ck = 2")
            .await;
        assert_eq!(
            h.rows(q).await,
            table(&[&["10", "1", "2", "201"], &["20", "1", "1", "100"]]),
            "{d}: update"
        );
        h.ok("DELETE v FROM ks.t WHERE pk = 1 AND ck = 2").await;
        assert_eq!(
            h.rows(q).await,
            table(&[&["20", "1", "1", "100"]]),
            "{d}: key column deleted"
        );
        h.ok("DELETE FROM ks.t WHERE pk = 1").await;
        assert_eq!(h.rows(q).await.len(), 0, "{d}: partition delete");
        // Writes to the view itself are refused.
        assert_eq!(
            code(
                &h.err("INSERT INTO ks.mv (v, pk, ck) VALUES (1, 1, 1)")
                    .await
            ),
            ErrorCode::Invalid,
            "{d}"
        );
    }
}

#[tokio::test]
async fn derived_data_survives_a_reopen() {
    for d in BOTH {
        let mut h = fresh(d).await;
        let cdc = if d.is_scylla() {
            " WITH cdc = {'enabled': true}"
        } else {
            ""
        };
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (pk int, ck int, v int, l list<int>, PRIMARY KEY (pk, ck)){cdc};\n\
             CREATE INDEX ON ks.t (v);\nCREATE INDEX ON ks.t (l);\n\
             CREATE MATERIALIZED VIEW ks.mv AS SELECT * FROM ks.t WHERE v IS NOT NULL AND pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (v, pk, ck);\n\
             INSERT INTO ks.t (pk, ck, v, l) VALUES (1, 1, 10, [1])"
        ))
        .await;
        let schema_q = [
            "SELECT * FROM system_schema.indexes WHERE keyspace_name = 'ks'",
            "SELECT * FROM system_schema.views WHERE keyspace_name = 'ks'",
            "SELECT * FROM system_schema.tables WHERE keyspace_name = 'ks'",
        ];
        let mut before = Vec::new();
        for q in schema_q {
            before.push(h.rows(q).await);
        }
        if d.is_scylla() {
            before.push(
                h.rows("SELECT * FROM system_schema.computed_columns WHERE keyspace_name = 'ks'")
                    .await,
            );
        }
        h.db.flush().await.unwrap();
        let store = h.store.clone();
        drop(h);
        let mut h = open(store, d).await;
        let mut after = Vec::new();
        for q in schema_q {
            after.push(h.rows(q).await);
        }
        if d.is_scylla() {
            after.push(
                h.rows("SELECT * FROM system_schema.computed_columns WHERE keyspace_name = 'ks'")
                    .await,
            );
        }
        assert_eq!(after, before, "{d}: schema");
        // Maintenance goes on after the reload.
        h.ok("INSERT INTO ks.t (pk, ck, v, l) VALUES (2, 1, 10, [1])")
            .await;
        h.ok("UPDATE ks.t SET v = 11 WHERE pk = 1 AND ck = 1").await;
        assert_eq!(
            h.rows("SELECT pk FROM ks.t WHERE v = 10").await,
            table(&[&["2"]]),
            "{d}"
        );
        assert_eq!(
            sorted(&mut h, "SELECT pk FROM ks.t WHERE l CONTAINS 1").await,
            table(&[&["1"], &["2"]]),
            "{d}"
        );
        assert_eq!(
            h.rows("SELECT v, pk FROM ks.mv").await,
            table(&[&["10", "2"], &["11", "1"]]),
            "{d}"
        );
        if d.is_scylla() {
            let ops = sorted(
                &mut h,
                "SELECT \"cdc$operation\", pk, v FROM ks.t_scylla_cdc_log",
            )
            .await;
            assert_eq!(
                ops,
                table(&[&["1", "1", "11"], &["2", "1", "10"], &["2", "2", "10"]]),
                "{d}: CDC log"
            );
        }
    }
}

/// The references let a dropped id be used again: an index re-created under its old name (its
/// index table's id comes from the name in Cassandra), `WITH ID` of a dropped view or table.
/// Each new incarnation starts empty, and keeps its storage across a reopen.
#[tokio::test]
async fn dropped_ids_are_reused_on_fresh_storage() {
    const ID: &str = "5f2c3f50-ae3c-11ee-8f37-0242ac110002";
    const TID: &str = "6f2c3f50-ae3c-11ee-8f37-0242ac110002";
    for d in BOTH {
        let mut h = fresh(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (p int PRIMARY KEY, v int);\n\
             INSERT INTO ks.t (p, v) VALUES (1, 10);\n\
             CREATE INDEX ON ks.t (v);\nDROP INDEX ks.t_v_idx;\n\
             CREATE INDEX ON ks.t (v)"
        ))
        .await;
        // The failed CREATE used to leave the index half there.
        h.ok("CREATE INDEX IF NOT EXISTS ON ks.t (v)").await;
        assert_eq!(
            h.rows("SELECT p FROM ks.t WHERE v = 10").await,
            table(&[&["1"]]),
            "{d}"
        );
        h.exec("DROP INDEX ks.t_v_idx;\nINSERT INTO ks.t (p, v) VALUES (2, 10);\nCREATE INDEX ON ks.t (v)")
            .await;
        assert_eq!(
            sorted(&mut h, "SELECT p FROM ks.t WHERE v = 10").await,
            table(&[&["1"], &["2"]]),
            "{d}: a third incarnation"
        );
        let mv = format!(
            "CREATE MATERIALIZED VIEW ks.mv AS SELECT * FROM ks.t WHERE p IS NOT NULL AND v IS NOT NULL \
             PRIMARY KEY (v, p) WITH ID = {ID}"
        );
        if d.is_scylla() {
            h.ok(&mv).await;
            h.ok("DROP MATERIALIZED VIEW ks.mv").await;
            h.ok("DELETE FROM ks.t WHERE p = 2").await;
            h.ok(&mv).await;
            assert_eq!(
                h.rows("SELECT v, p FROM ks.mv").await,
                table(&[&["10", "1"]]),
                "{d}"
            );
            assert_eq!(
                h.rows("SELECT id FROM system_schema.views WHERE keyspace_name = 'ks' AND view_name = 'mv'").await,
                table(&[&[ID]]),
                "{d}"
            );
        }
        h.ok(&format!(
            "CREATE TABLE ks.u (p int PRIMARY KEY, v int) WITH ID = {TID}"
        ))
        .await;
        h.ok("INSERT INTO ks.u (p, v) VALUES (1, 1)").await;
        h.ok("DROP TABLE ks.u").await;
        h.ok(&format!(
            "CREATE TABLE ks.u (p int PRIMARY KEY, v int) WITH ID = {TID}"
        ))
        .await;
        assert_eq!(
            h.rows("SELECT * FROM ks.u").await,
            table(&[]),
            "{d}: the old rows are gone"
        );
        h.ok("INSERT INTO ks.u (p, v) VALUES (2, 2)").await;
        h.db.flush().await.unwrap();
        let store = h.store.clone();
        drop(h);
        let mut h = open(store, d).await;
        assert_eq!(
            h.rows("SELECT * FROM ks.u").await,
            table(&[&["2", "2"]]),
            "{d}: reopened"
        );
        h.ok("INSERT INTO ks.t (p, v) VALUES (3, 10)").await;
        let want: &[&[&str]] = if d.is_scylla() {
            &[&["1"], &["3"]]
        } else {
            &[&["1"], &["2"], &["3"]]
        };
        assert_eq!(
            sorted(&mut h, "SELECT p FROM ks.t WHERE v = 10").await,
            table(want),
            "{d}: the index after a reopen"
        );
        if d.is_scylla() {
            assert_eq!(
                sorted(&mut h, "SELECT v, p FROM ks.mv").await,
                table(&[&["10", "1"], &["10", "3"]]),
                "{d}: the view after a reopen"
            );
        }
    }
}

/// SASI errors the live capture leaves out: a partition key component (single-column keys fail
/// earlier), and a multi-character delimiter (the reference stores that index, then fails to
/// start it; we refuse it with its answer).
#[tokio::test]
async fn sasi_errors_beyond_the_capture() {
    let store = Store::start(nosql_storage::test_objects("engine"), small_store_config())
        .await
        .unwrap();
    let handle = store
        .open("db", nosql_storage::db::DbConfig::default())
        .await
        .unwrap();
    let mut cfg = DbConfig::new("db", Dialect::Cassandra);
    cfg.settings.sasi_indexes_enabled = true;
    let db = Database::open(cfg, handle).await.unwrap();
    let s = db.connect(Default::default());
    let mut h = H { db, s, store };
    h.exec(&format!(
        "{KS};\nCREATE TABLE ks.t (a int, b int, v text, PRIMARY KEY ((a, b)))"
    ))
    .await;
    let sasi = "USING 'org.apache.cassandra.index.sasi.SASIIndex'";
    let e = h.err(&format!("CREATE CUSTOM INDEX ON ks.t (b) {sasi}")).await;
    assert_eq!(
        (e.code, e.message.as_str()),
        (
            ErrorCode::Config,
            "partition key columns are not yet supported by SASI"
        )
    );
    let e = h
        .err(&format!(
            "CREATE CUSTOM INDEX ON ks.t (v) {sasi} WITH OPTIONS = {{'analyzer_class': 'org.apache.cassandra.index.sasi.analyzer.DelimiterAnalyzer', 'delimiter': 'ab'}}"
        ))
        .await;
    assert_eq!(
        (e.code, e.message.as_str()),
        (
            ErrorCode::Server,
            "java.lang.RuntimeException: java.lang.reflect.InvocationTargetException"
        )
    );
    // A quoted column in the options a failed validation prints.
    h.ok("ALTER TABLE ks.t ADD \"Odd\" text").await;
    let e = h
        .err(&format!(
            "CREATE CUSTOM INDEX ON ks.t (\"Odd\") {sasi} WITH OPTIONS = {{'max_compaction_flush_memory_in_mb': '1x'}}"
        ))
        .await;
    assert_eq!(
        e.message,
        "Failed to validate custom indexer options: {max_compaction_flush_memory_in_mb=1x, class_name=org.apache.cassandra.index.sasi.SASIIndex, target=\"Odd\"}"
    );
}
