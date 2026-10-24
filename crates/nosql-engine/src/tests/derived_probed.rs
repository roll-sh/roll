//! Derived data (views, indexes, CDC, `fulltext_index`) cases probed on the references with the
//! Python driver: the `mv`, `cdc`, `index_props`, `vector_index` and `fulltext` categories of
//! `docs/nosql/conformance-triage.md`. Each expectation was checked on `scylladb/scylla:2026.3.3`
//! (default configuration) and on `cassandra:5.0.9` with `materialized_views_enabled`.

use std::sync::Arc;

use nosql_storage::store::Store;

use super::*;

/// A fresh database with Cassandra's `materialized_views_enabled` on, keyspace `ks` in use.
async fn fresh(d: Dialect) -> H {
    let store = Store::start(nosql_storage::test_objects("engine"), small_store_config())
        .await
        .unwrap();
    open(store, d).await
}

async fn open(store: Arc<Store>, d: Dialect) -> H {
    let handle = store
        .open("db", nosql_storage::db::DbConfig::default())
        .await
        .unwrap();
    let mut cfg = DbConfig::new("db", d);
    cfg.settings.materialized_views_enabled = true;
    let db = Database::open(cfg, handle).await.unwrap();
    let s = db.connect(Default::default());
    let mut h = H { db, s, store };
    h.exec(&format!("{KS};\nUSE ks")).await;
    h
}

const NONE: &[&[&str]] = &[];

/// A base row shadowed by a tombstone already stored (partition or range, with a newer
/// timestamp) has no view entry, and neither has one shadowed by a tombstone of an earlier
/// statement of the same batch (`test_views_with_future_tombstones`,
/// `test_mv_partition_tombstone_does_not_resurrect_range_deleted_row`,
/// `test_mv_range_delete_and_insert_in_same_batch`).
#[tokio::test]
async fn views_respect_stored_and_batched_tombstones() {
    for d in BOTH {
        let mut h = fresh(d).await;
        h.exec(
            "CREATE TABLE f (a int, b int, c int, d int, e int, PRIMARY KEY (a, b, c));\n\
             CREATE MATERIALIZED VIEW fmv AS SELECT * FROM f WHERE a IS NOT NULL AND b IS NOT NULL AND c IS NOT NULL PRIMARY KEY (b, a, c);\n\
             DELETE FROM f USING TIMESTAMP 10 WHERE a = 1;\n\
             INSERT INTO f (a, b, c, d, e) VALUES (1, 2, 3, 4, 5) USING TIMESTAMP 8;\n\
             CREATE TABLE r (p int, c int, v int, w int, PRIMARY KEY (p, c));\n\
             CREATE MATERIALIZED VIEW rmv AS SELECT * FROM r WHERE v IS NOT NULL AND p IS NOT NULL AND c IS NOT NULL PRIMARY KEY (v, p, c);\n\
             DELETE FROM r USING TIMESTAMP 150 WHERE p = 1 AND c >= 1 AND c <= 3;\n\
             INSERT INTO r (p, c, v) VALUES (1, 2, 3) USING TIMESTAMP 100;\n\
             UPDATE r USING TIMESTAMP 30 SET w = 4 WHERE p = 1 AND c = 2;\n\
             BEGIN BATCH DELETE FROM r WHERE p = 2 AND c >= 1 AND c <= 3; INSERT INTO r (p, c, v) VALUES (2, 3, 3); APPLY BATCH;\n\
             BEGIN BATCH DELETE FROM r USING TIMESTAMP 10 WHERE p = 3 AND c >= 1; INSERT INTO r (p, c, v) VALUES (3, 3, 3) USING TIMESTAMP 20; APPLY BATCH;\n\
             INSERT INTO r (p, c, v) VALUES (4, 2, 3) USING TIMESTAMP 5;\n\
             DELETE FROM r USING TIMESTAMP 10 WHERE p = 4 AND c > 1;\n\
             INSERT INTO r (p, c, v) VALUES (4, 2, 3) USING TIMESTAMP 20;\n\
             DELETE FROM r USING TIMESTAMP 15 WHERE p = 4",
        )
        .await;
        assert_eq!(h.rows("SELECT * FROM f").await, table(NONE), "{d}");
        assert_eq!(h.rows("SELECT * FROM fmv").await, table(NONE), "{d}");
        let want = table(&[&["3", "3", "3", "null"], &["3", "4", "2", "null"]]);
        assert_eq!(
            h.rows("SELECT v, p, c, w FROM r").await.len(),
            2,
            "{d}: base"
        );
        let mut got = h.rows("SELECT * FROM rmv").await;
        got.sort();
        assert_eq!(got, want, "{d}: view");
    }
}

/// A stored row tombstone or cell tombstone newer than the write shadows it in the views and
/// indexes too (`test_views_with_future_tombstones`).
#[tokio::test]
async fn views_respect_stored_row_and_cell_tombstones() {
    for d in BOTH {
        let mut h = fresh(d).await;
        h.exec(
            "CREATE TABLE r (p int, c int, v int, w int, s int static, PRIMARY KEY (p, c));\n\
             CREATE MATERIALIZED VIEW rmv AS SELECT p, c, v, w FROM r WHERE v IS NOT NULL AND p IS NOT NULL AND c IS NOT NULL PRIMARY KEY (v, p, c);\n\
             CREATE MATERIALIZED VIEW rmw AS SELECT p, c, v, w FROM r WHERE p IS NOT NULL AND c IS NOT NULL PRIMARY KEY (c, p);\n\
             CREATE INDEX ON r (s);\n\
             CREATE INDEX ON r (w);\n\
             DELETE FROM r USING TIMESTAMP 24 WHERE p = 3 AND c = 5;\n\
             INSERT INTO r (p, c, v) VALUES (3, 5, 6) USING TIMESTAMP 18;\n\
             DELETE v FROM r USING TIMESTAMP 20 WHERE p = 5 AND c = 1;\n\
             UPDATE r USING TIMESTAMP 10 SET v = 3 WHERE p = 5 AND c = 1;\n\
             DELETE w FROM r USING TIMESTAMP 20 WHERE p = 6 AND c = 1;\n\
             INSERT INTO r (p, c, v, w) VALUES (6, 1, 7, 8) USING TIMESTAMP 10;\n\
             DELETE s FROM r USING TIMESTAMP 20 WHERE p = 7;\n\
             UPDATE r USING TIMESTAMP 10 SET s = 9 WHERE p = 7",
        )
        .await;
        assert_eq!(
            h.rows("SELECT * FROM r").await,
            table(&[&["6", "1", "null", "7", "null"]]),
            "{d}"
        );
        assert_eq!(
            h.rows("SELECT * FROM rmv").await,
            table(&[&["7", "6", "1", "null"]]),
            "{d}"
        );
        assert_eq!(
            h.rows("SELECT * FROM rmw").await,
            table(&[&["1", "6", "7", "null"]]),
            "{d}"
        );
        assert_eq!(
            h.rows("SELECT p FROM r WHERE s = 9").await,
            table(NONE),
            "{d}"
        );
        assert_eq!(
            h.rows("SELECT p FROM r WHERE w = 8").await,
            table(NONE),
            "{d}"
        );
        if d.is_scylla() {
            assert_eq!(h.rows("SELECT * FROM r_s_idx_index").await, table(NONE));
            assert_eq!(h.rows("SELECT * FROM r_w_idx_index").await, table(NONE));
        }
    }
}

/// A view row whose partition key is an empty string is kept (ScyllaDB: token 0, readable by
/// key; Cassandra: the minimum token, and reading it by key is refused), and so is a ScyllaDB
/// index entry of an empty value (`test_mv_empty_string_partition_key*`).
#[tokio::test]
async fn views_keep_empty_partition_keys() {
    for d in BOTH {
        let mut h = fresh(d).await;
        h.exec(
            "CREATE TABLE t (p int PRIMARY KEY, v text);\n\
             CREATE MATERIALIZED VIEW mv AS SELECT * FROM t WHERE v IS NOT NULL AND p IS NOT NULL PRIMARY KEY (v, p);\n\
             INSERT INTO t (p, v) VALUES (123, '');\n\
             INSERT INTO t (p, v) VALUES (1, 'dog');\n\
             INSERT INTO t (p, v) VALUES (2, 'cat')",
        )
        .await;
        let empty = if d.is_scylla() {
            "0"
        } else {
            "-9223372036854775808"
        };
        let all = h.rows("SELECT token(v), v, p FROM mv").await;
        let want: Vec<Vec<String>> = match d {
            Dialect::Scylla => table(&[
                &["-1628323127093689546", "cat", "2"],
                &[empty, "", "123"],
                &["3525077494269131793", "dog", "1"],
            ]),
            Dialect::Cassandra => table(&[
                &[empty, "", "123"],
                &["-1628323127093689546", "cat", "2"],
                &["3525077494269131793", "dog", "1"],
            ]),
        };
        assert_eq!(all, want, "{d}");
        assert_eq!(
            h.rows("SELECT count(*) FROM mv").await,
            table(&[&["3"]]),
            "{d}"
        );
        if d.is_scylla() {
            assert_eq!(
                h.rows("SELECT v, p FROM mv WHERE v = ''").await,
                table(&[&["", "123"]])
            );
            assert_eq!(
                h.rows("SELECT v FROM mv WHERE token(v) = 0").await,
                table(&[&[""]])
            );
            assert_eq!(
                h.rows("SELECT v FROM mv WHERE token(v) > token('')").await,
                table(&[&["dog"]])
            );
            assert_eq!(
                h.rows("SELECT v FROM mv WHERE v IN ('', 'dog')").await,
                table(&[&[""], &["dog"]])
            );
            h.exec("CREATE INDEX ON t (v)").await;
            assert_eq!(
                h.rows("SELECT v, p FROM t_v_idx_index WHERE v = ''").await,
                table(&[&["", "123"]])
            );
        } else {
            assert_eq!(
                h.msg("SELECT v, p FROM mv WHERE v = ''").await,
                "Key may not be empty"
            );
        }
        h.ok("UPDATE t SET v = 'x' WHERE p = 123").await;
        assert_eq!(
            h.rows("SELECT count(*) FROM mv").await,
            table(&[&["3"]]),
            "{d}"
        );
        h.ok("UPDATE t SET v = '' WHERE p = 123").await;
        h.ok("DELETE FROM t WHERE p = 1").await;
        let mut got = h.rows("SELECT v, p FROM mv").await;
        got.sort();
        assert_eq!(got, table(&[&["", "123"], &["cat", "2"]]), "{d}");
        h.ok("DELETE v FROM t WHERE p = 123").await;
        assert_eq!(
            h.rows("SELECT v, p FROM mv").await,
            table(&[&["cat", "2"]]),
            "{d}"
        );
    }
}

/// What a statement gives on one reference.
#[derive(Clone, Copy, Debug)]
enum X {
    Ok,
    /// These rows (cqlsh rendering), in order.
    Rows(&'static [&'static [&'static str]]),
    /// Invalid (0x2200) with this message.
    Inv(&'static str),
    /// ConfigurationException (0x2300) with this message.
    Conf(&'static str),
    /// SyntaxException (0x2000) with this message.
    Syn(&'static str),
    /// Any error with this code.
    Code(i32),
}

/// A statement and its outcome on ScyllaDB and on Cassandra.
struct Case(&'static str, X, X);

fn same(cql: &'static str, x: X) -> Case {
    Case(cql, x, x)
}

/// Runs `setup` (`;\n`-separated) then the cases on a fresh database of each dialect.
async fn check(setup: &str, cases: &[Case]) {
    let mut bad = Vec::new();
    for d in BOTH {
        let mut h = fresh(d).await;
        h.exec(setup).await;
        for Case(cql, sx, cx) in cases {
            let want = if d.is_scylla() { sx } else { cx };
            let got = h.run(cql).await;
            let ok = match (want, &got) {
                (X::Ok, Ok(_)) => true,
                (X::Rows(r), Ok(resp)) => fmt_rows(resp) == table(r),
                (X::Inv(m), Err(e)) => e.code == ErrorCode::Invalid && e.message == *m,
                (X::Conf(m), Err(e)) => e.code == ErrorCode::Config && e.message == *m,
                (X::Syn(m), Err(e)) => e.code == ErrorCode::Syntax && e.message == *m,
                (X::Code(c), Err(e)) => e.code.code() == *c,
                _ => false,
            };
            if !ok {
                let got = match &got {
                    Ok(r) => format!("rows {:?}", fmt_rows(r)),
                    Err(e) => format!("error {:?} {}", e.code, e.message),
                };
                bad.push(format!("{d}: {cql}\n    want {want:?}\n    got  {got}"));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} mismatches:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// Cassandra 4+ refuses `COMPACT STORAGE` while parsing.
const C4: X = X::Syn("COMPACT STORAGE tables are not allowed starting with version 4.0");

const VIEW_NAME: &str = "(must contain alphanumeric character only: [0-9A-Za-z] or the underscore)";

/// `CREATE MATERIALIZED VIEW` validation in each reference's order: bind markers, ScyllaDB's
/// early id / `COMPACT STORAGE` / `cdc` / name checks, filtering on view key columns, and
/// `ALTER TABLE DROP` of a column no view needs (`test_create_mv_with`,
/// `test_creating_mv_with_existing_id`, `test_create_materialized_view_*_name`,
/// `test_mv_name_length_500`, `test_filter_with_unused_static_column`,
/// `test_alter_table_drop_allowed`, `test_mv_select_stmt_bound_values`).
#[tokio::test]
async fn view_ddl_validation() {
    let long = Box::leak(format!(
        "CREATE MATERIALIZED VIEW {} AS SELECT p, c FROM t6 WHERE p IS NOT NULL AND c IS NOT NULL PRIMARY KEY (p, c)",
        "x".repeat(193)
    ).into_boxed_str());
    let long_err = Box::leak(
        format!(
            "materialized view names shouldn't be more than 192 characters long (got \"{}\")",
            "x".repeat(193)
        )
        .into_boxed_str(),
    );
    let bad = |n: &str| -> &'static str {
        Box::leak(
            format!("\"{n}\" is not a valid materialized view name {VIEW_NAME}").into_boxed_str(),
        )
    };
    let cbad = |n: &str| -> &'static str {
        Box::leak(format!(
            "ks.{n}: Table name must not be empty or not contain non-alphanumeric-underscore characters (got \"{n}\")"
        ).into_boxed_str())
    };
    const ID: &str = "d14dbd3a-ccf6-11f1-adc3-9bf70ca69634";
    let dup_id: &'static str =
        Box::leak(format!("Table with ID {ID} already exists: ks.mve").into_boxed_str());
    let compact = X::Inv("Cannot use 'COMPACT STORAGE' when defining a materialized view");
    let cdc = X::Inv("Cannot enable CDC for a materialized view");
    let bind_s = X::Inv("Cannot use query parameters in CREATE MATERIALIZED VIEW statements");
    let bind_c = X::Inv("Bind variables are not allowed in CREATE MATERIALIZED VIEW statements");
    let cassandra_non_pk = |c: &str| -> X {
        X::Inv(Box::leak(format!(
            "Non-primary key columns can only be restricted with 'IS NOT NULL' (got: {c} restricted illegally)"
        ).into_boxed_str()))
    };
    let setup = "CREATE TABLE t1 (p int PRIMARY KEY);\n\
        CREATE TABLE t2 (p int, c int, v int, s int static, PRIMARY KEY (p, c));\n\
        CREATE TABLE t3 (p int PRIMARY KEY, v int);\n\
        CREATE TABLE t6 (p int, c int, s int static, v int, PRIMARY KEY (p, c));\n\
        CREATE TABLE t8 (p int PRIMARY KEY, a int, b int, c int)";
    check(
        setup,
        &[
            Case("CREATE MATERIALIZED VIEW mv1 AS SELECT * FROM t1 WHERE p = ? PRIMARY KEY (p)", bind_s, bind_c),
            Case("CREATE MATERIALIZED VIEW mv1 AS SELECT * FROM t1 WHERE p IS NOT NULL AND token(p) > ? PRIMARY KEY (p)", bind_s, bind_c),
            Case("CREATE MATERIALIZED VIEW mv1 AS SELECT * FROM t1 WHERE p IN ? PRIMARY KEY (p)", bind_s, bind_c),
            // ScyllaDB filters on any view key column.
            Case("CREATE MATERIALIZED VIEW mv2 AS SELECT p, c, v FROM t2 WHERE p = 42 AND c = 43 AND v = 44 PRIMARY KEY (p, c, v)", X::Ok, cassandra_non_pk("v")),
            same("INSERT INTO t2 (p, c, v, s) VALUES (42, 43, 44, 1)", X::Ok),
            same("INSERT INTO t2 (p, c, v) VALUES (42, 44, 44)", X::Ok),
            same("INSERT INTO t2 (p, c, v) VALUES (42, 43, 45)", X::Ok),
            same("INSERT INTO t2 (p, c, v) VALUES (42, 43, 44)", X::Ok),
            Case("SELECT * FROM mv2", X::Rows(&[&["42", "43", "44"]]), X::Inv("table mv2 does not exist")),
            Case("CREATE MATERIALIZED VIEW mv3 AS SELECT * FROM t3 WHERE v >= 0 AND p IS NOT NULL PRIMARY KEY (v, p)", X::Ok, cassandra_non_pk("v")),
            same("INSERT INTO t3 (p, v) VALUES (1, -1)", X::Ok),
            same("INSERT INTO t3 (p, v) VALUES (2, 2)", X::Ok),
            same("INSERT INTO t3 (p, v) VALUES (3, 3)", X::Ok),
            same("UPDATE t3 SET v = -3 WHERE p = 2", X::Ok),
            Case("SELECT * FROM mv3", X::Rows(&[&["3", "3"]]), X::Inv("table mv3 does not exist")),
            Case(
                "CREATE MATERIALIZED VIEW mv2b AS SELECT p, c, v FROM t2 WHERE p IS NOT NULL AND c IS NOT NULL AND v = 44 PRIMARY KEY (p, c)",
                X::Inv("Non-primary key columns cannot be restricted in the SELECT statement used for materialized view mv2b creation (got restrictions on: v)"),
                cassandra_non_pk("v"),
            ),
            // Early ScyllaDB checks: the id, COMPACT STORAGE, cdc, then the name.
            Case("CREATE MATERIALIZED VIEW mv4 AS SELECT * FROM nosuch WHERE p IS NOT NULL AND c IS NOT NULL PRIMARY KEY (p, c) WITH compact storage", compact, C4),
            Case("CREATE MATERIALIZED VIEW mv4 AS SELECT * FROM nosuch WHERE p IS NOT NULL AND c IS NOT NULL PRIMARY KEY (p, c) WITH cdc = {'enabled': false}", cdc, X::Inv("Base table 'nosuch' doesn't exist")),
            Case("CREATE MATERIALIZED VIEW mv4 AS SELECT * FROM nosuch WHERE p IS NOT NULL AND c IS NOT NULL PRIMARY KEY (p, c) WITH id = 'garbage'", X::Conf("Invalid table id"), X::Inv("Base table 'nosuch' doesn't exist")),
            Case("CREATE MATERIALIZED VIEW mv4 AS SELECT * FROM t6 WHERE p IS NOT NULL AND c IS NOT NULL PRIMARY KEY (p, c) WITH id = 'garbage' AND compact storage", X::Conf("Invalid table id"), C4),
            Case("CREATE MATERIALIZED VIEW mv4 AS SELECT p, c FROM t6 WHERE p IS NOT NULL PRIMARY KEY (p, c) WITH cdc = {'enabled': true} AND compact storage", compact, C4),
            Case("CREATE MATERIALIZED VIEW mv4 AS SELECT p, c FROM t6 WHERE p IS NOT NULL AND c IS NOT NULL PRIMARY KEY (p, c) WITH cdc = {'enabled': true} AND default_time_to_live = 1", cdc, X::Syn("Invalid value for property 'cdc'. It should be a string")),
            Case("CREATE MATERIALIZED VIEW \"xyz!123\" AS SELECT * FROM nosuch WHERE p IS NOT NULL PRIMARY KEY (p, c)", X::Inv(bad("xyz!123")), X::Inv("Base table 'nosuch' doesn't exist")),
            Case("CREATE MATERIALIZED VIEW \"xyz!123\" AS SELECT p, c FROM t6 WHERE p IS NOT NULL AND c IS NOT NULL PRIMARY KEY (p, c)", X::Inv(bad("xyz!123")), X::Conf(cbad("xyz!123"))),
            Case("CREATE MATERIALIZED VIEW \"/xyz/\" AS SELECT p, c FROM t6 WHERE p IS NOT NULL AND c IS NOT NULL PRIMARY KEY (p, c)", X::Inv(bad("/xyz/")), X::Conf(cbad("/xyz/"))),
            Case("CREATE MATERIALIZED VIEW \"a b\" AS SELECT p, c FROM t6 WHERE p IS NOT NULL AND c IS NOT NULL PRIMARY KEY (p, c)", X::Inv(bad("a b")), X::Conf(cbad("a b"))),
            Case("CREATE MATERIALIZED VIEW \"xyz!123\" AS SELECT p, c FROM t6 WHERE p IS NOT NULL AND c IS NOT NULL PRIMARY KEY (p, c) WITH garbage = 1", X::Syn("Unknown property 'garbage'"), X::Syn("Unknown property 'garbage'")),
            Case("CREATE MATERIALIZED VIEW \"xyz!123\" AS SELECT p, c FROM t6 WHERE p IS NOT NULL AND c IS NOT NULL PRIMARY KEY (p, c) WITH default_time_to_live = 1", X::Inv(bad("xyz!123")), X::Inv("Cannot set default_time_to_live for a materialized view. Data in a materialized view always expire at the same time than the corresponding data in the parent table.")),
            Case(long, X::Inv(long_err), X::Ok),
            // Ids.
            same(&*Box::leak(format!("CREATE MATERIALIZED VIEW mve AS SELECT p, c FROM t6 WHERE p IS NOT NULL AND c IS NOT NULL PRIMARY KEY (p, c) WITH ID = {ID}").into_boxed_str()), X::Ok),
            Case(Box::leak(format!("CREATE MATERIALIZED VIEW mvf AS SELECT p, c FROM t6 WHERE p IS NOT NULL AND c IS NOT NULL PRIMARY KEY (p, c) WITH ID = {ID}").into_boxed_str()), X::Inv(dup_id), X::Code(0x0000)),
            Case(Box::leak(format!("CREATE MATERIALIZED VIEW IF NOT EXISTS mve AS SELECT p, c FROM t6 WHERE p IS NOT NULL AND c IS NOT NULL PRIMARY KEY (p, c) WITH ID = {ID}").into_boxed_str()), X::Inv(dup_id), X::Ok),
            // ALTER MATERIALIZED VIEW.
            Case("ALTER MATERIALIZED VIEW mve WITH cdc = {'enabled': true}", cdc, X::Syn("Invalid value for property 'cdc'. It should be a string")),
            Case("ALTER MATERIALIZED VIEW mve WITH synchronous_updates = true", X::Ok, X::Syn("Unknown property 'synchronous_updates'")),
            // ALTER TABLE DROP of a column no view selects (Cassandra refuses any).
            same("CREATE MATERIALIZED VIEW mv8 AS SELECT p, a, b FROM t8 WHERE a IS NOT NULL AND p IS NOT NULL PRIMARY KEY (a, p)", X::Ok),
            Case("ALTER TABLE t8 DROP c", X::Ok, X::Inv("Cannot drop column c on base table t8 with materialized views")),
            Case("ALTER TABLE t8 DROP b", X::Inv("Cannot drop column b from base table ks.t8: materialized view mv8 needs this column"), X::Inv("Cannot drop column b on base table t8 with materialized views")),
        ],
    )
    .await;
}

/// ScyllaDB's `synchronous_updates` is the `system:synchronous_view_updates` tag (`true` for
/// `true` in any case or `1`), shown by DESCRIBE, and names the view in the trace of a base
/// write (`test_mv_synchronous_updates`).
#[tokio::test]
async fn synchronous_view_updates() {
    let mut h = fresh(Dialect::Scylla).await;
    h.exec(
        "CREATE TABLE t (p int PRIMARY KEY, v text);\n\
         CREATE MATERIALIZED VIEW a AS SELECT * FROM t WHERE v IS NOT NULL AND p IS NOT NULL PRIMARY KEY (v, p);\n\
         CREATE MATERIALIZED VIEW b AS SELECT * FROM t WHERE v IS NOT NULL AND p IS NOT NULL PRIMARY KEY (v, p) WITH synchronous_updates = 'xyz';\n\
         CREATE MATERIALIZED VIEW c AS SELECT * FROM t WHERE v IS NOT NULL AND p IS NOT NULL PRIMARY KEY (v, p) WITH synchronous_updates = true;\n\
         CREATE MATERIALIZED VIEW d AS SELECT * FROM t WHERE v IS NOT NULL AND p IS NOT NULL PRIMARY KEY (v, p) WITH synchronous_updates = 'TRUE';\n\
         CREATE MATERIALIZED VIEW e AS SELECT * FROM t WHERE v IS NOT NULL AND p IS NOT NULL PRIMARY KEY (v, p) WITH synchronous_updates = 1;\n\
         ALTER MATERIALIZED VIEW a WITH synchronous_updates = true;\n\
         ALTER MATERIALIZED VIEW d WITH synchronous_updates = false",
    )
    .await;
    let tags = h
        .rows("SELECT view_name, extensions['scylla_tags'] FROM system_schema.views WHERE keyspace_name = 'ks'")
        .await;
    let on = "0x010000001f00000073797374656d3a73796e6368726f6e6f75735f766965775f75706461746573040000007472756";
    let flags: Vec<(String, bool)> = tags
        .iter()
        .map(|r| (r[0].clone(), r[1].starts_with(on)))
        .collect();
    assert_eq!(
        flags,
        [
            ("a", true),
            ("b", false),
            ("c", true),
            ("d", false),
            ("e", true)
        ]
        .map(|(n, b)| (n.to_string(), b))
        .to_vec()
    );
    let desc = h.rows("DESC MATERIALIZED VIEW c").await;
    assert!(desc[0][3].ends_with(
        "AND scylla_tags = {'system:synchronous_view_updates': 'true'}\n    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'}\n    AND synchronous_updates = true;\n"
    ), "{}", desc[0][3]);
    let desc = h.rows("DESC MATERIALIZED VIEW d").await;
    assert!(
        desc[0][3].contains("AND scylla_tags = {'system:synchronous_view_updates': 'false'}\n")
    );
    assert!(desc[0][3].ends_with("\n    AND synchronous_updates = false;\n"));
    h.s.set_tracing(true);
    let r =
        h.s.query(
            "INSERT INTO t (p, v) VALUES (123, 'dog')",
            QueryParams::default(),
        )
        .await
        .unwrap();
    h.s.set_tracing(false);
    let id = r.tracing_id.expect("tracing id");
    let events: Vec<String> = h
        .rows(&format!(
            "SELECT activity FROM system_traces.events WHERE session_id = {}",
            crate::uuid::format(&id)
        ))
        .await
        .into_iter()
        .map(|r| r[0].clone())
        .filter(|a| a.contains("view update"))
        .collect();
    assert_eq!(
        events,
        [
            "Forcing ks.a view update to be synchronous (synchronous_updates property was set)",
            "Forcing ks.c view update to be synchronous (synchronous_updates property was set)",
            "Forcing ks.e view update to be synchronous (synchronous_updates property was set)",
        ]
    );
}

const LOG_COLUMNS: &str = "You cannot modify the set of columns of a CDC log table directly. Modify the base table instead.";
const LOG_COLUMNS_INACTIVE: &str = "You cannot modify the set of columns of a CDC log table directly. Although the base table has deactivated CDC, this table will continue being a CDC log table until it is dropped. If you want to modify the columns in it, you can only do that by reenabling CDC on the base table, which will reattach this log table. Then you will be able to modify the columns in the base table, and that will have effect on the log table too. Modifying the columns of a CDC log table directly is never allowed.";
const LOG_RENAME: &str = "Cannot rename a column of a CDC log table.";
const LOG_RENAME_INACTIVE: &str = "You cannot rename a column of a CDC log table. Although the base table has deactivated CDC, this table will continue being a CDC log table until it is dropped.";

/// `ALTER TABLE` of a CDC log's columns is refused, with a longer text while the base has CDC
/// disabled; its options may change (`test_alter_column_of_*cdc_log_table`,
/// `test_rename_column_of_*cdc_log_table`).
#[tokio::test]
async fn cdc_log_columns_cannot_be_altered() {
    let mut h = fresh(Dialect::Scylla).await;
    h.exec("CREATE TABLE t3 (p int PRIMARY KEY, v int, u int) WITH cdc = {'enabled': true}")
        .await;
    for (cql, want) in [
        ("ALTER TABLE t3_scylla_cdc_log ADD c int", LOG_COLUMNS),
        ("ALTER TABLE t3_scylla_cdc_log DROP u", LOG_COLUMNS),
        (
            "ALTER TABLE t3_scylla_cdc_log ALTER u TYPE blob",
            LOG_COLUMNS,
        ),
        ("ALTER TABLE t3_scylla_cdc_log RENAME u TO c", LOG_RENAME),
        ("ALTER TABLE t3_scylla_cdc_log RENAME p TO q", LOG_RENAME),
    ] {
        assert_eq!(h.msg(cql).await, want, "{cql}");
    }
    h.ok("ALTER TABLE t3_scylla_cdc_log WITH comment = 'x'")
        .await;
    h.ok("ALTER TABLE t3 WITH cdc = {'enabled': false}").await;
    for (cql, want) in [
        (
            "ALTER TABLE t3_scylla_cdc_log ADD c int",
            LOG_COLUMNS_INACTIVE,
        ),
        ("ALTER TABLE t3_scylla_cdc_log DROP u", LOG_COLUMNS_INACTIVE),
        (
            "ALTER TABLE t3_scylla_cdc_log RENAME u TO c",
            LOG_RENAME_INACTIVE,
        ),
    ] {
        assert_eq!(h.msg(cql).await, want, "{cql}");
    }
    h.ok("ALTER TABLE t3_scylla_cdc_log WITH comment = 'y'")
        .await;
    // A table named like a log, of another shape, is an ordinary table.
    h.exec("CREATE TABLE x_scylla_cdc_log (p int PRIMARY KEY, v int);\nALTER TABLE x_scylla_cdc_log ADD c int")
        .await;
}

/// A CDC log described by name is its `CREATE TABLE` in a comment while the base logs; with
/// CDC disabled the log is detached (it is an ordinary table to `DESCRIBE` and does not follow
/// the base), and re-enabling CDC drops the log columns the base lost
/// (`test_describe_cdc_log_table_format`, `test_reattach_cdc_log_table_after_altering_base`).
#[tokio::test]
async fn cdc_log_describe_and_reattach() {
    let mut h = fresh(Dialect::Scylla).await;
    h.exec(
        "CREATE TABLE t (p int, v int, u int, a int, PRIMARY KEY (p, v)) WITH cdc = {'enabled': true};\n\
         CREATE TABLE w (p int PRIMARY KEY, v vector<float, 3>);\n\
         CREATE CUSTOM INDEX ON w (v) USING 'vector_index'",
    )
    .await;
    let note = crate::describe::scylla::CDC_LOG_NOTE;
    for log in ["t_scylla_cdc_log", "w_scylla_cdc_log"] {
        for cql in [format!("DESC TABLE {log}"), format!("DESC {log}")] {
            let s = h.rows(&cql).await[0][3].clone();
            assert!(s.starts_with(note), "{cql}: {s}");
            assert!(s.ends_with(";\n\n*/"), "{cql}: {s}");
        }
    }
    // DESC TABLE of the base adds the log's options.
    assert_eq!(h.rows("DESC TABLE t").await.len(), 2);
    h.exec(
        "ALTER TABLE t WITH cdc = {'enabled': false};\n\
         ALTER TABLE t DROP a;\n\
         ALTER TABLE t ADD b double;\n\
         ALTER TABLE t RENAME v TO v2;\n\
         INSERT INTO t (p, v2, u, b) VALUES (5, 6, 7, 9.2)",
    )
    .await;
    assert_eq!(h.rows("DESC TABLE t").await.len(), 1);
    let s = h.rows("DESC TABLE t_scylla_cdc_log").await[0][3].clone();
    assert!(s.starts_with("CREATE TABLE ks.t_scylla_cdc_log (\n"), "{s}");
    assert!(s.contains("\n    a int,\n"), "detached: {s}");
    assert_eq!(
        h.rows("SELECT * FROM t_scylla_cdc_log WHERE p = 5 ALLOW FILTERING")
            .await,
        table(NONE)
    );
    h.ok("ALTER TABLE t WITH cdc = {'enabled': true}").await;
    let s = h.rows("DESC TABLE t_scylla_cdc_log WITH INTERNALS").await[0][3].clone();
    assert!(
        s.contains(
            "    \"cdc$ttl\" bigint,\n    p int,\n    u int,\n    v2 int,\n    v int,\n    \"cdc$deleted_a\" boolean,\n    a int,\n    PRIMARY KEY (\"cdc$stream_id\", \"cdc$time\", \"cdc$batch_seq_no\")\n)"
        ),
        "{s}"
    );
    assert!(
        s.contains("\nALTER TABLE ks.t_scylla_cdc_log DROP v USING TIMESTAMP "),
        "{s}"
    );
    assert_eq!(
        h.msg("SELECT v FROM t_scylla_cdc_log").await,
        "Unrecognized name v"
    );
    // While CDC is on, the log's dropped columns are recorded with the base's drop.
    h.exec("ALTER TABLE t DROP u").await;
    let rows = h
        .rows("SELECT column_name, type FROM system_schema.dropped_columns WHERE keyspace_name = 'ks' AND table_name = 't_scylla_cdc_log'")
        .await;
    let names: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(names, ["a", "cdc$deleted_a", "cdc$deleted_u", "u", "v"]);
}

/// Columns of an index view's `system_schema.views` row.
async fn view_props(h: &mut H, cols: &str, index: &str) -> Vec<Vec<String>> {
    h.rows(&format!(
        "SELECT {cols} FROM system_schema.views WHERE keyspace_name = 'ks' AND view_name = '{index}_index'"
    ))
    .await
}

/// The ScyllaDB view properties of `CREATE INDEX` (applied to the index view, shown by
/// `system_schema.views` and `DESCRIBE INDEX`), their validation and its order; Cassandra knows
/// none (`test_secondary_index_properties.py`, `test_describe_index_*`).
#[tokio::test]
async fn index_view_properties() {
    let mut h = fresh(Dialect::Scylla).await;
    h.exec(
        "CREATE TABLE t (p int, c int, v int, u int, w int, x int, f frozen<list<int>>, vv vector<float, 3>, PRIMARY KEY (p, c));\n\
         CREATE TABLE t2 (p int PRIMARY KEY) WITH ID = 018ad550-b25d-09d0-7e90-ea5438411dc8",
    )
    .await;
    h.ok("CREATE INDEX i1 ON t(v) WITH bloom_filter_fp_chance = 0.13")
        .await;
    assert_eq!(
        view_props(&mut h, "bloom_filter_fp_chance", "i1").await,
        table(&[&["0.13"]])
    );
    h.ok("DROP INDEX i1").await;
    h.ok("CREATE INDEX i1 ON t(v) WITH caching = {'enabled': true, 'keys': 'ALL', 'rows_per_partition': 'ALL'} AND comment = 'abc' AND gc_grace_seconds = 0").await;
    assert_eq!(
        view_props(&mut h, "caching, comment, gc_grace_seconds", "i1").await,
        table(&[&["{'keys': 'ALL', 'rows_per_partition': 'ALL'}", "abc", "0"]])
    );
    h.ok("DROP INDEX i1").await;
    h.ok("CREATE INDEX i1 ON t(v) WITH compaction = {'class': 'TimeWindowCompactionStrategy'} AND tombstone_gc = {'mode': 'timeout', 'propagation_delay_in_seconds': '4200'}").await;
    let d = h.rows("DESC INDEX i1").await[0][3].clone();
    assert!(
        d.starts_with("CREATE INDEX i1 ON ks.t(v) WITH bloom_filter_fp_chance = 0.01\n"),
        "{d}"
    );
    assert!(
        d.contains("\n    AND compaction = {'class': 'TimeWindowCompactionStrategy'}\n"),
        "{d}"
    );
    assert!(d.ends_with("\n    AND tombstone_gc = {'mode': 'timeout', 'propagation_delay_in_seconds': '4200'};\n"), "{d}");
    h.ok("DROP INDEX i1").await;
    h.ok("CREATE INDEX i1 ON t(v) WITH ID = 018ad550-b25d-09d0-7e90-ea5438411dc7")
        .await;
    assert_eq!(
        view_props(&mut h, "id", "i1").await,
        table(&[&["018ad550-b25d-09d0-7e90-ea5438411dc7"]])
    );
    let d = h.rows("DESC INDEX i1 WITH INTERNALS").await[0][3].clone();
    assert!(d.starts_with("CREATE INDEX i1 ON ks.t(v) WITH ID = 018ad550-b25d-09d0-7e90-ea5438411dc7\n    AND bloom_filter_fp_chance = 0.01\n"), "{d}");
    h.ok("DROP INDEX i1").await;
    h.ok("CREATE INDEX i1 ON t(v) WITH synchronous_updates = true")
        .await;
    let d = h.rows("DESC INDEX i1").await[0][3].clone();
    assert!(d.ends_with("    AND scylla_tags = {'system:synchronous_view_updates': 'true'}\n    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'}\n    AND synchronous_updates = true;\n"), "{d}");
    h.ok("DROP INDEX i1").await;
    // A local index is synchronous unless told otherwise; its frozen collection is `full()`.
    h.ok("CREATE INDEX i2 ON t((p), x) WITH synchronous_updates = false AND comment = 'loc'")
        .await;
    let d = h.rows("DESC INDEX i2").await[0][3].clone();
    assert!(d.contains("    AND comment = 'loc'\n"), "{d}");
    assert!(d.ends_with("    AND scylla_tags = {'system:synchronous_view_updates': 'false'}\n    AND tombstone_gc = {'mode': 'timeout', 'propagation_delay_in_seconds': '3600'}\n    AND synchronous_updates = false;\n"), "{d}");
    h.ok("CREATE INDEX i3 ON t((p), full(f))").await;
    let d = h.rows("DESC INDEX i3").await[0][3].clone();
    assert!(
        d.starts_with("CREATE INDEX i3 ON ks.t((p), full(f)) WITH"),
        "{d}"
    );
    const DTL: &str = "Cannot set or alter default_time_to_live for a materialized view. Data in a materialized view always expire at the same time than the corresponding data in the parent table.";
    for (cql, want) in [
        (
            "CREATE INDEX i9 ON t(u) WITH default_time_to_live = 13",
            X::Inv(DTL),
        ),
        (
            "CREATE INDEX i9 ON t(u) WITH COMPACT STORAGE",
            X::Inv("Cannot use 'COMPACT STORAGE' when defining a materialized view"),
        ),
        (
            "CREATE INDEX i9 ON t(u) WITH CLUSTERING ORDER BY (p ASC)",
            X::Inv("Indexes do not allow for specifying the clustering order"),
        ),
        (
            "CREATE INDEX i9 ON t(u) WITH CLUSTERING ORDER BY (p ASC) AND COMPACT STORAGE",
            X::Inv("Cannot use 'COMPACT STORAGE' when defining a materialized view"),
        ),
        (
            "CREATE INDEX i9 ON t(u) WITH cdc = {'enabled': true}",
            X::Inv("Cannot enable CDC for a materialized view"),
        ),
        (
            "CREATE INDEX i9 ON t(u) WITH default_time_to_live = 1 AND cdc = {'enabled': true}",
            X::Inv("Cannot enable CDC for a materialized view"),
        ),
        (
            "CREATE INDEX i9 ON t(u) WITH id = 'xx' AND COMPACT STORAGE",
            X::Conf("Invalid table id"),
        ),
        (
            "CREATE INDEX i9 ON t(u) WITH ID = 018ad550-b25d-09d0-7e90-ea5438411dc8",
            X::Inv("Table with ID 018ad550-b25d-09d0-7e90-ea5438411dc8 already exists: ks.t2"),
        ),
        (
            "CREATE INDEX i9 ON t(nosuch) WITH COMPACT STORAGE",
            X::Inv("Cannot use 'COMPACT STORAGE' when defining a materialized view"),
        ),
        (
            "CREATE INDEX i9 ON t(nosuch) WITH default_time_to_live = 1",
            X::Inv("No column definition found for column nosuch"),
        ),
        (
            "CREATE INDEX i9 ON t(p) WITH default_time_to_live = 1",
            X::Inv("Cannot create secondary index on partition key column p"),
        ),
        (
            "CREATE INDEX i9 ON t(u) WITH index_interval = 128",
            X::Syn("Unknown property 'index_interval'"),
        ),
        (
            "CREATE INDEX i9 ON t(u) WITH read_repair_chance = 0.0",
            X::Syn("Unknown property 'read_repair_chance'"),
        ),
        (
            "CREATE INDEX i9 ON t(u) WITH garbage = 1",
            X::Syn("Unknown property 'garbage'"),
        ),
        (
            "CREATE INDEX i9 ON t(u) WITH bloom_filter_fp_chance = 'not a number'",
            X::Syn("Invalid double value not a number for 'bloom_filter_fp_chance'"),
        ),
        (
            "CREATE INDEX i9 ON t(u) WITH gc_grace_seconds = 'x' AND default_time_to_live = 5",
            X::Syn("Invalid integer value x for 'gc_grace_seconds'"),
        ),
        (
            "CREATE INDEX i9 ON t(u) WITH bloom_filter_fp_chance = 1.23",
            X::Conf(
                "bloom_filter_fp_chance must be larger than 6.71e-05 and less than or equal to 1.0 (got 1.23)",
            ),
        ),
        (
            "CREATE INDEX i9 ON t(u) WITH OPTIONS = {'a': 'b'} AND default_time_to_live = 5",
            X::Inv("Cannot specify options for a non-CUSTOM index"),
        ),
        (
            "CREATE CUSTOM INDEX i9 ON t(u) USING 'foo' WITH comment = 'x'",
            X::Inv("Non-supported custom class 'foo' provided"),
        ),
        (
            "CREATE CUSTOM INDEX i9 ON t(vv) USING 'vector_index' WITH gc_grace_seconds = 13",
            X::Inv("You cannot use view properties with a vector_index"),
        ),
        (
            "CREATE CUSTOM INDEX i9 ON t(vv) USING 'vector_index' WITH garbage = 1",
            X::Inv("You cannot use view properties with a vector_index"),
        ),
        (
            "CREATE CUSTOM INDEX i9 ON t(vv) USING 'vector_index' WITH COMPACT STORAGE",
            X::Inv("You cannot use view properties with a vector_index"),
        ),
        (
            "CREATE CUSTOM INDEX i9 ON t(vv) USING 'vector_index' WITH OPTIONS = {'similarity_function': 'COSINE'} AND comment = 'x'",
            X::Inv("You cannot use view properties with a vector_index"),
        ),
        // Duplicates ignore the view properties.
        (
            "CREATE INDEX i2 ON t((p), x) WITH comment = 'other'",
            X::Inv("Index already exists"),
        ),
        (
            "CREATE INDEX IF NOT EXISTS i2 ON t((p), x) WITH comment = 'other'",
            X::Ok,
        ),
        (
            "CREATE INDEX i8 ON t((p), x) WITH speculative_retry = '99PERCENTILE'",
            X::Inv("Index i8 is a duplicate of existing index i2"),
        ),
    ] {
        let got = h.run(cql).await;
        let ok = match (want, &got) {
            (X::Ok, Ok(_)) => true,
            (X::Inv(m), Err(e)) => e.code == ErrorCode::Invalid && e.message == m,
            (X::Conf(m), Err(e)) => e.code == ErrorCode::Config && e.message == m,
            (X::Syn(m), Err(e)) => e.code == ErrorCode::Syntax && e.message == m,
            _ => false,
        };
        assert!(
            ok,
            "{cql}: want {want:?}, got {:?}",
            got.map(|r| fmt_rows(&r))
        );
    }
    // Prepared: validated on every execution.
    let p = h
        .s
        .prepare("CREATE INDEX i7 ON t(w) WITH compaction = {'class': 'SizeTieredCompactionStrategy'} AND min_index_interval = 0", None)
        .await
        .unwrap();
    for _ in 0..2 {
        let e =
            h.s.execute(&p.id, QueryParams::default())
                .await
                .unwrap_err();
        assert_eq!(e.message, "min_index_interval must be greater than 0");
    }
    // Cassandra has no index view properties.
    let mut c = fresh(Dialect::Cassandra).await;
    c.exec("CREATE TABLE t (p int PRIMARY KEY, u int)").await;
    assert_eq!(
        c.msg("CREATE INDEX i9 ON t(u) WITH comment = 'x'").await,
        "Unknown property 'comment'"
    );
    assert_eq!(
        c.err("CREATE INDEX i9 ON t(u) WITH COMPACT STORAGE")
            .await
            .code,
        ErrorCode::Syntax
    );
}

/// Runs `cases` in order on one database, each answering as given.
async fn expect(h: &mut H, cases: &[(&str, X)]) {
    let mut bad = Vec::new();
    for (cql, want) in cases {
        let got = h.run(cql).await;
        let ok = match (want, &got) {
            (X::Ok, Ok(_)) => true,
            (X::Rows(r), Ok(resp)) => fmt_rows(resp) == table(r),
            (X::Inv(m), Err(e)) => e.code == ErrorCode::Invalid && e.message == *m,
            (X::Conf(m), Err(e)) => e.code == ErrorCode::Config && e.message == *m,
            (X::Syn(m), Err(e)) => e.code == ErrorCode::Syntax && e.message == *m,
            (X::Code(c), Err(e)) => e.code.code() == *c,
            _ => false,
        };
        if !ok {
            let got = match &got {
                Ok(r) => format!("rows {:?}", fmt_rows(r)),
                Err(e) => format!("error {:?} {}", e.code, e.message),
            };
            bad.push(format!(
                "{}: {cql}\n    want {want:?}\n    got  {got}",
                h.d()
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{} mismatches:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

const VFLOAT: X = X::Inv("Vector indexes are only supported on columns of vectors of floats");
const SAI_VECTOR: X = X::Inv(
    "StorageAttachedIndex (SAI) is only supported on vector columns; use a secondary index for non-vector columns",
);

/// ScyllaDB vector index targets: local partitions and filtering columns, their validation,
/// stored form and DESCRIBE, default names, SAI class names (`test_vector_index.py`).
#[tokio::test]
async fn vector_index_targets() {
    let mut h = fresh(Dialect::Scylla).await;
    h.exec(
        "CREATE TABLE t (p int, c int, v vector<float, 3>, v2 vector<float, 3>, s int static, f int, g text, d duration, m map<int, int>, fl frozen<list<int>>, w vector<int, 3>, PRIMARY KEY (p, c));\n\
         CREATE TABLE tp (p frozen<map<int, int>>, c int, v vector<float, 3>, f int, PRIMARY KEY (p, c));\n\
         CREATE TABLE tt (p tuple<int, int>, c int, v vector<float, 3>, f int, PRIMARY KEY (p, c));\n\
         CREATE TABLE tm (p int PRIMARY KEY, m map<text, text>, x text)",
    )
    .await;
    let unsupported = |c: &'static str| -> X {
        X::Inv(Box::leak(
            format!("Unsupported vector index filtering column {c} type").into_boxed_str(),
        ))
    };
    let statics = X::Inv("Local indexes containing static columns are not supported.");
    expect(&mut h, &[
        ("CREATE CUSTOM INDEX ON t(v, s) USING 'vector_index'", statics),
        ("CREATE CUSTOM INDEX ON t(v, v) USING 'vector_index'", unsupported("v")),
        ("CREATE CUSTOM INDEX ON t(v, f, f) USING 'vector_index'", X::Inv("Duplicate column f in index target list")),
        ("CREATE CUSTOM INDEX ON t(v, nosuch) USING 'vector_index'", X::Inv("Column nosuch not found in schema")),
        ("CREATE CUSTOM INDEX ON t(nosuch, f) USING 'vector_index'", X::Inv("Column nosuch not found in schema")),
        ("CREATE CUSTOM INDEX ON t(nosuch) USING 'vector_index'", X::Inv("Column nosuch not found in schema")),
        ("CREATE CUSTOM INDEX ON t((wrongname), v) USING 'vector_index'", X::Inv("Local vector index definition must contain primary key or regular columns. Redundant column: wrongname")),
        ("CREATE CUSTOM INDEX ON t((p, wrongname), nosuch) USING 'vector_index'", X::Inv("Local vector index definition must contain primary key or regular columns. Redundant column: wrongname")),
        ("CREATE CUSTOM INDEX ON t((s), v) USING 'vector_index'", X::Inv("Local vector index definition must contain primary key or regular columns. Redundant column: s")),
        ("CREATE CUSTOM INDEX ON t((p, p), nosuch) USING 'vector_index'", X::Inv("Duplicate column definition in local vector index: p")),
        ("CREATE CUSTOM INDEX ON t((d), v) USING 'vector_index'", unsupported("d")),
        ("CREATE CUSTOM INDEX ON t((p), nosuch) USING 'vector_index'", X::Inv("Column nosuch not found in schema")),
        ("CREATE CUSTOM INDEX ON t(v, v2) USING 'vector_index'", unsupported("v2")),
        ("CREATE CUSTOM INDEX ON t(v, d) USING 'vector_index'", unsupported("d")),
        ("CREATE CUSTOM INDEX ON t(v, m) USING 'vector_index'", unsupported("m")),
        ("CREATE CUSTOM INDEX ON t(v, keys(m)) USING 'vector_index'", unsupported("m")),
        ("CREATE CUSTOM INDEX ON t(v, fl) USING 'vector_index'", unsupported("fl")),
        ("CREATE CUSTOM INDEX ON t(f, v) USING 'vector_index'", VFLOAT),
        ("CREATE CUSTOM INDEX ON t(w) USING 'vector_index'", VFLOAT),
        ("CREATE CUSTOM INDEX ON t(s) USING 'vector_index'", VFLOAT),
        ("CREATE CUSTOM INDEX ON t((p), f) USING 'vector_index'", VFLOAT),
        ("CREATE CUSTOM INDEX ON t((p), v, p) USING 'vector_index'", X::Inv("Cannot create secondary index on partition key column p")),
        ("CREATE CUSTOM INDEX ON t((p), v, v) USING 'vector_index'", unsupported("v")),
        ("CREATE CUSTOM INDEX ON t((p), v, s) USING 'vector_index'", statics),
        ("CREATE CUSTOM INDEX ON tp((p), v, f) USING 'vector_index'", unsupported("p")),
        ("CREATE CUSTOM INDEX ON tt((p), v, f) USING 'vector_index'", unsupported("p")),
        ("CREATE INDEX ON t(v, f)", X::Inv("Only CUSTOM indexes support multiple columns")),
        ("CREATE CUSTOM INDEX ON t(f, g) USING 'sai'", SAI_VECTOR),
        ("CREATE CUSTOM INDEX ON t(nosuch) USING 'sai'", SAI_VECTOR),
        ("CREATE CUSTOM INDEX ON t(f) USING 'sai' WITH OPTIONS = {'similarity_function': 'cosine'}", SAI_VECTOR),
        ("CREATE CUSTOM INDEX ON tm(ENTRIES(m)) USING 'sai'", X::Inv("StorageAttachedIndex (SAI) on ENTRIES of a map column is not supported; if this is a CassIO/LangChain workload, enable the enable_cassio_compatibility configuration option to rewrite this index as a regular secondary index")),
        ("CREATE CUSTOM INDEX ON tm(ENTRIES(m)) USING 'org.apache.cassandra.index.sai.StorageAttachedIndex' WITH OPTIONS = {'some_option': 'value'}", X::Inv("StorageAttachedIndex (SAI) on map entries cannot be rewritten to a regular secondary index when WITH OPTIONS are specified; remove the options or use a secondary index directly")),
        ("CREATE CUSTOM INDEX ON t(v) USING 'org.apache.cassandra.index.sai.STORAGEATTACHEDINDEX'", X::Inv("Non-supported custom class 'org.apache.cassandra.index.sai.STORAGEATTACHEDINDEX' provided")),
        ("CREATE CUSTOM INDEX ON t(v, c) USING 'vector_index'", X::Ok),
        ("CREATE CUSTOM INDEX ON t((p), v, c) USING 'VECTOR_INDEX'", X::Ok),
        ("CREATE CUSTOM INDEX ON t((p, c), v) USING 'vector_index'", X::Ok),
        ("CREATE CUSTOM INDEX ON t((c), v) USING 'Vector_Index'", X::Ok),
        ("CREATE CUSTOM INDEX ON t(v, f) USING 'sai'", X::Ok),
        ("CREATE CUSTOM INDEX ON t(v) USING 'StorageAttachedIndex' WITH OPTIONS = {'similarity_function': 'cosine'}", X::Ok),
        ("CREATE CUSTOM INDEX ON t(v2, f, g) USING 'SAI'", X::Ok),
        ("CREATE CUSTOM INDEX lv ON t((p), v, f, g) USING 'vector_index'", X::Ok),
    ]).await;
    let rows = h
        .rows("SELECT index_name, options['target'], options['class_name'] FROM system_schema.indexes WHERE keyspace_name = 'ks' AND table_name = 't'")
        .await;
    assert_eq!(
        rows,
        table(&[
            &[
                "lv",
                r#"{"tc":"v","pk":["p"],"fc":["f","g"]}"#,
                "vector_index"
            ],
            &["t_idx", r#"{"tc":"v","fc":["c"]}"#, "vector_index"],
            // The class is stored as written.
            &[
                "t_idx_1",
                r#"{"tc":"v","pk":["p"],"fc":["c"]}"#,
                "VECTOR_INDEX"
            ],
            &["t_idx_2", r#"{"tc":"v","fc":["f"]}"#, "vector_index"],
            &["t_idx_3", r#"{"tc":"v2","fc":["f","g"]}"#, "vector_index"],
            &["t_v_idx", r#"{"tc":"v","pk":["p","c"]}"#, "vector_index"],
            &["t_v_idx_1", r#"{"tc":"v","pk":["c"]}"#, "Vector_Index"],
            &["t_v_idx_2", "v", "vector_index"],
        ])
    );
    for (name, desc) in [
        (
            "lv",
            "CREATE CUSTOM INDEX lv ON ks.t((p), v, f, g) USING 'vector_index'",
        ),
        (
            "t_idx",
            "CREATE CUSTOM INDEX t_idx ON ks.t(v, c) USING 'vector_index'",
        ),
        (
            "t_v_idx",
            "CREATE CUSTOM INDEX t_v_idx ON ks.t((p, c), v) USING 'vector_index'",
        ),
        (
            "t_v_idx_2",
            "CREATE CUSTOM INDEX t_v_idx_2 ON ks.t(v) USING 'vector_index' WITH OPTIONS = {'similarity_function': 'cosine'}",
        ),
    ] {
        assert_eq!(h.rows(&format!("DESC INDEX {name}")).await[0][3], desc);
    }
    // The Vector Store keys rows by at most 255 primary key columns.
    for (npk, nck, ok) in [
        (255, 0, true),
        (256, 0, false),
        (1, 254, true),
        (1, 255, false),
        (100, 156, false),
    ] {
        let cols: Vec<String> = (0..npk)
            .map(|i| format!("p{i} int"))
            .chain((0..nck).map(|i| format!("c{i} int")))
            .collect();
        let pk: Vec<String> = (0..npk).map(|i| format!("p{i}")).collect();
        let ck: String = (0..nck).map(|i| format!(", c{i}")).collect();
        h.ok(&format!(
            "CREATE TABLE w{npk}_{nck} ({}, v vector<float, 3>, PRIMARY KEY (({}){ck}))",
            cols.join(", "),
            pk.join(", ")
        ))
        .await;
        let r = h
            .run(&format!(
                "CREATE CUSTOM INDEX ON w{npk}_{nck}(v) USING 'vector_index'"
            ))
            .await;
        match ok {
            true => assert!(r.is_ok(), "{npk}+{nck}: {:?}", r.err()),
            false => assert_eq!(
                r.unwrap_err().message,
                format!(
                    "Vector index requires at most 255 primary key columns, but {} were found",
                    npk + nck
                )
            ),
        }
    }
    // Cassandra: the SAI class needs its exact case too.
    let mut c = fresh(Dialect::Cassandra).await;
    c.exec("CREATE TABLE t (p int PRIMARY KEY, v vector<float, 3>, x int)")
        .await;
    expect(&mut c, &[
        ("CREATE CUSTOM INDEX ON t(v) USING 'org.apache.cassandra.index.sai.STORAGEATTACHEDINDEX'", X::Conf("Unable to find custom indexer class 'org.apache.cassandra.index.sai.STORAGEATTACHEDINDEX'")),
        ("CREATE CUSTOM INDEX ON t(x) USING 'storageattachedindex'", X::Ok),
    ]).await;
}

/// Duplicate vector indexes: an unnamed one matching an existing definition (`index_version`
/// aside) is refused, named ones are allowed; IF NOT EXISTS with a taken name does nothing,
/// whatever the table or column (`test_*duplicate*vector_index*`).
#[tokio::test]
async fn vector_index_duplicates() {
    let mut h = fresh(Dialect::Scylla).await;
    h.exec(
        "CREATE TABLE t (p int PRIMARY KEY, v vector<float, 3>, v2 vector<float, 3>);\n\
         CREATE TABLE t2 (p int PRIMARY KEY, v vector<float, 3>);\n\
         CREATE TABLE l (p1 int, p2 int, v vector<float, 3>, PRIMARY KEY ((p1, p2)))",
    )
    .await;
    expect(&mut h, &[
        ("CREATE CUSTOM INDEX idx1 ON t(v) USING 'sai'", X::Ok),
        ("CREATE CUSTOM INDEX idx1 ON t(v) USING 'sai'", X::Inv("Index already exists")),
        ("CREATE CUSTOM INDEX IF NOT EXISTS idx1 ON t(v) USING 'sai'", X::Ok),
        ("CREATE CUSTOM INDEX idx2 ON t(v) USING 'vector_index'", X::Ok),
        ("CREATE CUSTOM INDEX ON t(v) USING 'sai'", X::Inv("Index t_v_idx is a duplicate of existing index idx1")),
        ("CREATE CUSTOM INDEX IF NOT EXISTS ON t(v) USING 'sai'", X::Ok),
        ("CREATE CUSTOM INDEX IF NOT EXISTS idx1 ON t(v2) USING 'sai'", X::Ok),
        ("CREATE CUSTOM INDEX IF NOT EXISTS idx1 ON t2(v) USING 'sai'", X::Ok),
        ("CREATE CUSTOM INDEX idx1 ON t2(v) USING 'sai'", X::Inv("Index already exists")),
        ("CREATE CUSTOM INDEX ON t2(v) USING 'vector_index' WITH OPTIONS = {'similarity_function': 'COSINE'}", X::Ok),
        ("CREATE CUSTOM INDEX ON t2(v) USING 'vector_index'", X::Ok),
        ("CREATE CUSTOM INDEX ON t2(v) USING 'vector_index'", X::Inv("Index t2_v_idx_2 is a duplicate of existing index t2_v_idx_1")),
        ("CREATE CUSTOM INDEX ON l((p1, p2), v) USING 'sai'", X::Ok),
        ("CREATE CUSTOM INDEX ON l((p1, p2), v) USING 'sai'", X::Inv("Index l_v_idx_1 is a duplicate of existing index l_v_idx")),
        ("CREATE CUSTOM INDEX IF NOT EXISTS ON l((p1, p2), v) USING 'sai'", X::Ok),
        ("CREATE CUSTOM INDEX ON l(v) USING 'sai'", X::Ok),
    ]).await;
    let names = h
        .rows("SELECT table_name, index_name FROM system_schema.indexes WHERE keyspace_name = 'ks'")
        .await;
    assert_eq!(
        names,
        table(&[
            &["l", "l_v_idx"],
            &["l", "l_v_idx_1"],
            &["t", "idx1"],
            &["t", "idx2"],
            &["t2", "t2_v_idx"],
            &["t2", "t2_v_idx_1"],
        ])
    );
}

/// Vector index options: each checked in the reference's order, integers and floats read as C
/// does (`test_vector_index_option_*`, `test_create_vector_search_index_with_bad_numeric_value`,
/// `test_sai_vector_index_with_source_model`, `test_vector_index_version_fail_given_as_option`).
#[tokio::test]
async fn vector_index_options() {
    let mut h = fresh(Dialect::Scylla).await;
    h.exec("CREATE TABLE t (p int PRIMARY KEY, v vector<float, 3>, x int)")
        .await;
    let mut cases: Vec<(String, X)> = Vec::new();
    let opt = |o: &str| {
        format!("CREATE CUSTOM INDEX ON t(v) USING 'vector_index' WITH OPTIONS = {{{o}}}")
    };
    let range = |k: &str, v: &str, max: &str| -> X {
        X::Inv(Box::leak(format!("Invalid value in option '{k}' for vector index: '{v}' is out of valid range [1 - {max}]").into_boxed_str()))
    };
    let not = |k: &str, v: &str, what: &str| -> X {
        X::Inv(Box::leak(
            format!("Invalid value in option '{k}' for vector index: '{v}' is not {what}")
                .into_boxed_str(),
        ))
    };
    for (k, max) in [
        ("maximum_node_connections", "512"),
        ("construction_beam_width", "4096"),
        ("search_beam_width", "4096"),
    ] {
        for v in ["-1", "0", "-0"] {
            cases.push((opt(&format!("'{k}': '{v}'")), range(k, v, max)));
        }
        cases.push((
            opt(&format!(
                "'{k}': '{}'",
                if max == "512" { "513" } else { "4097" }
            )),
            range(k, if max == "512" { "513" } else { "4097" }, max),
        ));
        for v in ["abc", "1.5", " 7 ", "1e2", "0x10", "", "3000000000"] {
            cases.push((opt(&format!("'{k}': '{v}'")), not(k, v, "an integer")));
        }
        cases.push((opt(&format!("'{k}': 1.0")), not(k, "1.0", "an integer")));
        for v in ["'1'", "'+5'", "' 7'", "16", &format!("'{max}'")] {
            cases.push((opt(&format!("'{k}': {v}")), X::Ok));
            cases.push(("DROP INDEX t_v_idx".into(), X::Ok));
        }
    }
    for v in [
        "'1'",
        "'100'",
        "'0x10'",
        "'  9'",
        "'0x1p3'",
        "'1.'",
        "'.5e1'",
        "1e1",
        "0x10",
        "1000000e-4",
        "50.5",
    ] {
        cases.push((opt(&format!("'oversampling': {v}")), X::Ok));
        cases.push(("DROP INDEX t_v_idx".into(), X::Ok));
    }
    for v in ["100.1", "0.9", "nan", "inf", "INF", "Infinity", "-1"] {
        cases.push((
            opt(&format!("'oversampling': '{v}'")),
            range("oversampling", v, "100"),
        ));
    }
    for v in ["abc", "  9  ", "", "1e400", "1e39", "1e-50", "5f"] {
        cases.push((
            opt(&format!("'oversampling': '{v}'")),
            not("oversampling", v, "a float"),
        ));
    }
    let choice = |k: &str, v: &str, allowed: &str| -> X {
        X::Inv(Box::leak(format!("Invalid value in option '{k}' for vector index: '{v}'. Supported are case-insensitive: {allowed}").into_boxed_str()))
    };
    for v in ["f32", "f16", "bf16", "i8", "b1", "F32", "BF16", "B1"] {
        cases.push((opt(&format!("'quantization': '{v}'")), X::Ok));
        cases.push(("DROP INDEX t_v_idx".into(), X::Ok));
    }
    cases.push((
        opt("'quantization': 'invalid_value'"),
        choice("quantization", "invalid_value", "f32, f16, bf16, i8, b1"),
    ));
    for v in ["true", "false", "True", "FALSE"] {
        cases.push((opt(&format!("'rescoring': '{v}'")), X::Ok));
        cases.push(("DROP INDEX t_v_idx".into(), X::Ok));
    }
    cases.push((
        opt("'rescoring': 'invalid_value'"),
        choice("rescoring", "invalid_value", "false, true"),
    ));
    cases.push((
        opt("'similarity_function': 'manhattan'"),
        choice(
            "similarity_function",
            "manhattan",
            "cosine, euclidean, dot_product",
        ),
    ));
    // The order: similarity_function, search_beam_width, rescoring, quantization, oversampling,
    // maximum_node_connections, construction_beam_width, unknown options.
    let order = [
        ("similarity_function", "'zz'"),
        ("search_beam_width", "'0'"),
        ("rescoring", "'zz'"),
        ("quantization", "'zz'"),
        ("oversampling", "'0'"),
        ("maximum_node_connections", "'0'"),
        ("construction_beam_width", "'0'"),
        ("bogus", "'x'"),
    ];
    for i in 0..order.len() {
        for j in i + 1..order.len() {
            let (a, av) = order[i];
            let (b, bv) = order[j];
            let first = opt(&format!("'{a}': {av}"));
            let want = cases_want(&first, a);
            cases.push((opt(&format!("'{b}': {bv}, '{a}': {av}")), want));
        }
    }
    cases.push((
        opt("'bogus': 'x'"),
        X::Inv("Unsupported option bogus for vector index"),
    ));
    cases.push((
        opt("'target': 'x'"),
        X::Inv("Unsupported option target for vector index"),
    ));
    cases.push((
        opt("'index_version': 'x', 'similarity_function': 'zz'"),
        X::Inv("Cannot specify index_version as a CUSTOM option"),
    ));
    cases.push((
        opt("'class_name': 'x'"),
        X::Inv("Cannot specify class_name as a CUSTOM option"),
    ));
    cases.push((
        "CREATE CUSTOM INDEX ON t(x) USING 'vector_index' WITH OPTIONS = {'quantization': 'zz'}"
            .into(),
        VFLOAT,
    ));
    cases.push((
        opt("'source_model': 'ada002', 'similarity_function': 'COSINE'"),
        X::Ok,
    ));
    let cases: Vec<(&str, X)> = cases.iter().map(|(c, x)| (c.as_str(), *x)).collect();
    expect(&mut h, &cases).await;
    assert_eq!(
        h.rows("DESC INDEX t_v_idx").await[0][3],
        "CREATE CUSTOM INDEX t_v_idx ON ks.t(v) USING 'vector_index' WITH OPTIONS = {'similarity_function': 'COSINE', 'source_model': 'ada002'}"
    );
}

/// The error a single bad vector option gives (see `vector_index_options`).
fn cases_want(_cql: &str, option: &str) -> X {
    X::Inv(match option {
        "similarity_function" => {
            "Invalid value in option 'similarity_function' for vector index: 'zz'. Supported are case-insensitive: cosine, euclidean, dot_product"
        }
        "search_beam_width" => {
            "Invalid value in option 'search_beam_width' for vector index: '0' is out of valid range [1 - 4096]"
        }
        "rescoring" => {
            "Invalid value in option 'rescoring' for vector index: 'zz'. Supported are case-insensitive: false, true"
        }
        "quantization" => {
            "Invalid value in option 'quantization' for vector index: 'zz'. Supported are case-insensitive: f32, f16, bf16, i8, b1"
        }
        "oversampling" => {
            "Invalid value in option 'oversampling' for vector index: '0' is out of valid range [1 - 100]"
        }
        "maximum_node_connections" => {
            "Invalid value in option 'maximum_node_connections' for vector index: '0' is out of valid range [1 - 512]"
        }
        _ => {
            "Invalid value in option 'construction_beam_width' for vector index: '0' is out of valid range [1 - 4096]"
        }
    })
}

/// ANN query errors around the index (`vector_invalid_query_test.py`).
#[tokio::test]
async fn vector_queries() {
    let mut h = fresh(Dialect::Scylla).await;
    h.exec(
        "CREATE TABLE t (k int, c int, v vector<float, 1>, f int, PRIMARY KEY (k, c));\n\
         CREATE CUSTOM INDEX ON t(v) USING 'vector_index';\n\
         CREATE TABLE t2 (k int PRIMARY KEY, v1 vector<float, 1>, v2 vector<float, 1>);\n\
         CREATE CUSTOM INDEX ON t2(v1) USING 'vector_index' WITH OPTIONS = {'similarity_function': 'euclidean'};\n\
         CREATE TABLE t3 (k int PRIMARY KEY, v vector<float, 3>, f int, g text);\n\
         CREATE CUSTOM INDEX ON t3(v, f) USING 'vector_index'",
    )
    .await;
    const OFF: X = X::Inv("Vector Store is disabled");
    expect(
        &mut h,
        &[
            (
                "SELECT k FROM t ORDER BY bad_col ANN OF [1.0] LIMIT 1",
                X::Inv("Undefined column name bad_col"),
            ),
            (
                "SELECT * FROM t ORDER BY v ANN OF [2] PER PARTITION LIMIT 1",
                X::Inv("Vector ANN queries must have a limit specified"),
            ),
            (
                "SELECT * FROM t ORDER BY v ANN OF [2] PER PARTITION LIMIT 1 LIMIT 2000",
                X::Inv("Vector ANN queries do not support per-partition limits"),
            ),
            (
                "SELECT * FROM t ORDER BY v ANN OF [2] PER PARTITION LIMIT 1 LIMIT 3",
                X::Inv("Vector ANN queries do not support per-partition limits"),
            ),
            (
                "SELECT count(*) FROM t ORDER BY v ANN OF [2] PER PARTITION LIMIT 1 LIMIT 2",
                X::Inv("PER PARTITION LIMIT is not allowed with aggregate queries."),
            ),
            (
                "SELECT * FROM t WHERE f = 1 ORDER BY v ANN OF [2] LIMIT 2",
                OFF,
            ),
            (
                "SELECT * FROM t WHERE k = 1 AND c > 1 ORDER BY v ANN OF [2] LIMIT 2",
                OFF,
            ),
            (
                "SELECT * FROM t2 WHERE v1 = [1] ORDER BY v2 ANN OF [2] LIMIT 3 ALLOW FILTERING",
                X::Inv(
                    "ANN ordering by vector requires the column to be indexed using 'vector_index'",
                ),
            ),
            (
                "SELECT * FROM t3 WHERE f = 1 ORDER BY v ANN OF [1, 2, 3] LIMIT 3",
                OFF,
            ),
            ("SELECT * FROM t3 ORDER BY v ANN OF [1, 2, 3] LIMIT 3", OFF),
        ],
    )
    .await;
    let mut c = fresh(Dialect::Cassandra).await;
    c.exec(
        "CREATE TABLE t2 (k int PRIMARY KEY, v1 vector<float, 1>, v2 vector<float, 1>);\n\
         CREATE CUSTOM INDEX ON t2(v1) USING 'StorageAttachedIndex' WITH OPTIONS = {'similarity_function': 'euclidean'}",
    )
    .await;
    expect(
        &mut c,
        &[(
            "SELECT * FROM t2 WHERE v1 = [1] ORDER BY v2 ANN OF [2] LIMIT 3 ALLOW FILTERING",
            X::Inv("ANN ordering by vector requires the column to be indexed"),
        )],
    )
    .await;
}

const FT_TYPE: fn(&str) -> String = |c| {
    format!(
        "Fulltext index is only supported on text, varchar, or ascii columns, but column {c} has an incompatible type"
    )
};

/// ScyllaDB's `fulltext_index`: targets, options and their order, tablets and CDC requirements,
/// duplicates, the stored row, DESCRIBE, and the CDC log it brings (`test_fulltext_index.py`).
#[tokio::test]
async fn fulltext_index_ddl() {
    let mut h = fresh(Dialect::Scylla).await;
    h.exec(
        "CREATE TABLE t (p int, c int, v text, a ascii, vc varchar, i int, b blob, vec vector<float, 3>, s text static, PRIMARY KEY (p, c));\n\
         CREATE TABLE tk (p text, c text, v text, PRIMARY KEY (p, c));\n\
         CREATE TABLE t2 (p int PRIMARY KEY, v text, w text, x text, y text);\n\
         CREATE KEYSPACE vn WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1} AND tablets = {'enabled': false};\n\
         CREATE TABLE vn.t (p int PRIMARY KEY, v text);\n\
         CREATE TABLE t3 (p int PRIMARY KEY, v text) WITH cdc = {'enabled': true, 'ttl': 3600};\n\
         CREATE TABLE t4 (p int PRIMARY KEY, v text) WITH cdc = {'enabled': true, 'delta': 'keys'};\n\
         CREATE TABLE t5 (p int PRIMARY KEY, v text) WITH cdc = {'enabled': true, 'delta': 'keys', 'postimage': true};\n\
         CREATE TABLE t6 (p int PRIMARY KEY, v text) WITH cdc = {'enabled': true, 'delta': 'full', 'ttl': 0};\n\
         CREATE TABLE t7 (p int PRIMARY KEY, v text) WITH cdc = {'enabled': false}",
    )
    .await;
    let ty = |c: &str| X::Inv(Box::leak(FT_TYPE(c).into_boxed_str()));
    let analyzer = |v: &str| -> X {
        X::Inv(Box::leak(format!("Invalid value in option 'analyzer' for fulltext index: '{v}'. Supported are case-insensitive: standard, english, german, french, spanish, italian, portuguese, russian, simple, whitespace").into_boxed_str()))
    };
    const POSITIONS: X = X::Inv(
        "Invalid value in option 'positions' for fulltext index: 'maybe'. Supported are case-insensitive: false, true",
    );
    const CDC_FT: X = X::Inv(
        "To enable Full-Text Search on this table, the CDC log must meet the minimal requirements for external indexes.\nCDC's TTL must be at least 86400 seconds (24 hours), and the CDC's delta mode must be set to 'full' or postimage must be enabled.",
    );
    expect(&mut h, &[
        ("CREATE CUSTOM INDEX ON t(v) USING 'fulltext_index'", X::Ok),
        ("CREATE CUSTOM INDEX ON t(a) USING 'FULLTEXT_INDEX'", X::Ok),
        ("CREATE CUSTOM INDEX ON t(vc) USING 'FullText_Index'", X::Ok),
        ("CREATE CUSTOM INDEX ON t(s) USING 'fulltext_index'", X::Ok),
        ("CREATE CUSTOM INDEX ON t(i) USING 'fulltext_index'", ty("i")),
        ("CREATE CUSTOM INDEX ON t(b) USING 'fulltext_index'", ty("b")),
        ("CREATE CUSTOM INDEX ON t(vec) USING 'fulltext_index' WITH OPTIONS = {'positions': 'maybe'}", ty("vec")),
        ("CREATE CUSTOM INDEX ON t(p) USING 'fulltext_index'", ty("p")),
        ("CREATE CUSTOM INDEX ON t(c) USING 'fulltext_index'", ty("c")),
        ("CREATE CUSTOM INDEX ON tk(p) USING 'fulltext_index'", X::Inv("Cannot create secondary index on partition key column p")),
        ("CREATE CUSTOM INDEX ON tk(c) USING 'fulltext_index'", X::Ok),
        ("CREATE CUSTOM INDEX ON t(nosuch) USING 'fulltext_index' WITH OPTIONS = {'positions': 'maybe'}", X::Inv("Column nosuch not found in schema")),
        ("CREATE CUSTOM INDEX ON t(v, a) USING 'fulltext_index'", X::Inv("Fulltext index must have exactly one target column")),
        ("CREATE CUSTOM INDEX ON t((p), v) USING 'fulltext_index'", X::Inv("Fulltext index must have exactly one target column")),
        ("CREATE CUSTOM INDEX ON t(keys(v)) USING 'fulltext_index'", X::Inv("Cannot create index on keys() of column v; only non-frozen collections support keys() indexes")),
        ("CREATE CUSTOM INDEX ON t(full(v)) USING 'fulltext_index'", X::Inv("full() indexes can only be created on frozen collections")),
        ("CREATE CUSTOM INDEX ON t2(v) USING 'fulltext_index' WITH OPTIONS = {'analyzer': 'english'}", X::Ok),
        ("CREATE CUSTOM INDEX ON t2(w) USING 'fulltext_index' WITH OPTIONS = {'analyzer': 'ENGLISH', 'positions': 'FALSE'}", X::Ok),
        ("CREATE CUSTOM INDEX ON t2(x) USING 'fulltext_index' WITH OPTIONS = {'analyzer': 'chinese'}", analyzer("chinese")),
        ("CREATE CUSTOM INDEX ON t2(x) USING 'fulltext_index' WITH OPTIONS = {'analyzer': 'japanese'}", analyzer("japanese")),
        ("CREATE CUSTOM INDEX ON t2(x) USING 'fulltext_index' WITH OPTIONS = {'analyzer': 'korean'}", analyzer("korean")),
        ("CREATE CUSTOM INDEX ON t2(x) USING 'fulltext_index' WITH OPTIONS = {'analyzer': 'klingon'}", analyzer("klingon")),
        ("CREATE CUSTOM INDEX ON t2(x) USING 'fulltext_index' WITH OPTIONS = {'positions': 'maybe'}", POSITIONS),
        ("CREATE CUSTOM INDEX ON t2(x) USING 'fulltext_index' WITH OPTIONS = {'bad_option': 'x'}", X::Inv("Unsupported option bad_option for fulltext index")),
        ("CREATE CUSTOM INDEX ON t2(x) USING 'fulltext_index' WITH OPTIONS = {'bad_option': 'x', 'analyzer': 'klingon'}", X::Inv("Unsupported option bad_option for fulltext index")),
        ("CREATE CUSTOM INDEX ON t2(x) USING 'fulltext_index' WITH OPTIONS = {'positions': 'maybe', 'analyzer': 'klingon'}", POSITIONS),
        ("CREATE CUSTOM INDEX ON t2(x) USING 'fulltext_index' WITH OPTIONS = {'bad_option': 'x', 'positions': 'maybe'}", POSITIONS),
        ("CREATE CUSTOM INDEX ON t2(x) USING 'fulltext_index' WITH OPTIONS = {'positions': 'maybe', 'zzz': 'x'}", X::Inv("Unsupported option zzz for fulltext index")),
        ("CREATE CUSTOM INDEX ON t2(x) USING 'fulltext_index' WITH OPTIONS = {'similarity_function': 'cosine'}", X::Inv("Unsupported option similarity_function for fulltext index")),
        ("CREATE CUSTOM INDEX ON t2(x) USING 'fulltext_index' WITH OPTIONS = {'index_version': 'x'}", X::Inv("Cannot specify index_version as a CUSTOM option")),
        ("CREATE CUSTOM INDEX ON t2(x) USING 'fulltext_index' WITH comment = 'x'", X::Inv("You cannot use view properties with a fulltext_index")),
        ("CREATE CUSTOM INDEX ON t2(x) USING 'fulltext_index' WITH OPTIONS = {'analyzer': 'standard', 'positions': 'true'}", X::Ok),
        ("CREATE CUSTOM INDEX ON t2(y) USING 'fulltext_index' WITH OPTIONS = {'analyzer': 'Whitespace', 'positions': 'True'}", X::Ok),
        ("CREATE CUSTOM INDEX ON t2(y) USING 'fulltext_index' WITH OPTIONS = {'positions': 'True', 'analyzer': 'Whitespace'}", X::Inv("Index t2_y_idx_1 is a duplicate of existing index t2_y_idx")),
        // Another analyzer is another index; a name may be taken once per keyspace.
        ("CREATE CUSTOM INDEX ON t2(v) USING 'fulltext_index'", X::Ok),
        ("CREATE CUSTOM INDEX IF NOT EXISTS ON t2(v) USING 'fulltext_index'", X::Ok),
        ("CREATE CUSTOM INDEX ON t2(v) USING 'fulltext_index' WITH OPTIONS = {'analyzer': 'english'}", X::Inv("Index t2_v_idx_2 is a duplicate of existing index t2_v_idx")),
        ("CREATE CUSTOM INDEX named ON t2(v) USING 'fulltext_index'", X::Ok),
        ("CREATE CUSTOM INDEX named ON t2(w) USING 'fulltext_index'", X::Inv("Index already exists")),
        ("CREATE CUSTOM INDEX IF NOT EXISTS named ON t2(x) USING 'fulltext_index'", X::Ok),
        ("CREATE CUSTOM INDEX IF NOT EXISTS named ON t3(v) USING 'fulltext_index'", X::Ok),
        ("CREATE INDEX ON t2(v)", X::Ok),
        ("CREATE CUSTOM INDEX ON vn.t(v) USING 'fulltext_index'", X::Inv("Creating a fulltext index requires the base table's keyspace to use tablets.\nPlease alter the keyspace to use tablets and try again.")),
        ("CREATE CUSTOM INDEX ON t3(v) USING 'fulltext_index'", CDC_FT),
        ("CREATE CUSTOM INDEX ON t4(v) USING 'fulltext_index'", CDC_FT),
        ("CREATE CUSTOM INDEX ON t5(v) USING 'fulltext_index'", X::Ok),
        ("CREATE CUSTOM INDEX ON t6(v) USING 'fulltext_index'", X::Ok),
        ("CREATE CUSTOM INDEX ON t7(v) USING 'fulltext_index'", X::Ok),
        ("ALTER TABLE t2 WITH cdc = {'enabled': false}", X::Inv("Cannot disable CDC when Full-Text Search is enabled on the table.\nPlease drop the fulltext index first, then disable CDC.")),
        ("ALTER TABLE t2 WITH cdc = {'enabled': true, 'ttl': 100}", X::Inv("Full-Text Search is enabled on this table.\nThe CDC log must meet the minimal requirements for external indexes.\nThis means that the CDC's TTL must be at least 86400 seconds (24 hours), and the CDC's delta mode must be set to 'full' or postimage must be enabled.")),
        ("ALTER TABLE t2 WITH cdc = {'enabled': true, 'ttl': 90000}", X::Ok),
        ("SELECT view_name FROM system_schema.views WHERE keyspace_name = 'ks' AND base_table_name = 't' ALLOW FILTERING", X::Rows(NONE)),
        ("SELECT table_name FROM system_schema.tables WHERE keyspace_name = 'ks' AND table_name = 't7_scylla_cdc_log'", X::Rows(&[&["t7_scylla_cdc_log"]])),
    ]).await;
    let opts = h
        .rows("SELECT index_name, options FROM system_schema.indexes WHERE keyspace_name = 'ks' AND table_name = 't2' AND index_name IN ('t2_v_idx', 't2_w_idx')")
        .await;
    let v = &opts[0][1];
    assert!(
        v.starts_with("{'target': 'v', 'index_version': '")
            && v.ends_with("', 'class_name': 'fulltext_index', 'analyzer': 'english'}"),
        "{v}"
    );
    let w = &opts[1][1];
    assert!(
        w.ends_with(
            "', 'class_name': 'fulltext_index', 'positions': 'FALSE', 'analyzer': 'ENGLISH'}"
        ),
        "{w}"
    );
    for (name, desc) in [
        (
            "t_v_idx",
            "CREATE CUSTOM INDEX t_v_idx ON ks.t(v) USING 'fulltext_index'",
        ),
        (
            "t_a_idx",
            "CREATE CUSTOM INDEX t_a_idx ON ks.t(a) USING 'fulltext_index'",
        ),
        (
            "t2_w_idx",
            "CREATE CUSTOM INDEX t2_w_idx ON ks.t2(w) USING 'fulltext_index' WITH OPTIONS = {'analyzer': 'ENGLISH', 'positions': 'FALSE'}",
        ),
    ] {
        assert_eq!(h.rows(&format!("DESC INDEX {name}")).await[0][3], desc);
    }
    assert_eq!(
        h.rows("SELECT options['class_name'] FROM system_schema.indexes WHERE keyspace_name = 'ks' AND table_name = 't' AND index_name = 't_a_idx'").await,
        table(&[&["FULLTEXT_INDEX"]])
    );
    // Writes are logged for the index.
    h.ok("INSERT INTO t7 (p, v) VALUES (1, 'hello world')")
        .await;
    assert_eq!(
        h.rows("SELECT p, v FROM t7_scylla_cdc_log").await,
        table(&[&["1", "hello world"]])
    );
    h.ok("DROP INDEX t_v_idx").await;
    assert_eq!(h.rows("SELECT index_name FROM system_schema.indexes WHERE keyspace_name = 'ks' AND table_name = 't' AND index_name = 't_v_idx'").await, table(NONE));
    // Cassandra has no full-text index.
    let mut c = fresh(Dialect::Cassandra).await;
    c.exec("CREATE TABLE t (p int PRIMARY KEY, v text)").await;
    expect(
        &mut c,
        &[(
            "CREATE CUSTOM INDEX ON t(v) USING 'fulltext_index'",
            X::Conf("Unable to find custom indexer class 'fulltext_index'"),
        )],
    )
    .await;
}

const NO_STORE: X = X::Inv("Vector Store is disabled");

/// Prepares `cql` and executes it with `values`: the bind (name, type) pairs and the error.
async fn run_prepared(h: &mut H, cql: &str, values: Vec<Param>) -> (Vec<(String, String)>, String) {
    let p = h.s.prepare(cql, None).await.unwrap();
    let names: Vec<(String, String)> = p
        .bind
        .iter()
        .map(|c| (c.name.clone(), nosql_types::cql_name(&c.ty)))
        .collect();
    let e =
        h.s.execute(&p.id, QueryParams::with_values(values))
            .await
            .unwrap_err();
    (names, e.message)
}

/// `BM25()` searches: every validation error of the reference, then "Vector Store is disabled"
/// (`test_bm25_*`, `test_order_by_*bm25*`).
#[tokio::test]
async fn fulltext_queries() {
    let mut h = fresh(Dialect::Scylla).await;
    h.exec(
        "CREATE TABLE t (p int PRIMARY KEY, content text, vec vector<float, 2>, tag text, a ascii);\n\
         CREATE CUSTOM INDEX ON t(content) USING 'fulltext_index';\n\
         CREATE CUSTOM INDEX ON t(vec) USING 'vector_index';\n\
         CREATE CUSTOM INDEX ON t(a) USING 'fulltext_index';\n\
         CREATE INDEX ON t(tag);\n\
         CREATE TABLE u (p int PRIMARY KEY, col1 text, col2 text, plain text);\n\
         CREATE CUSTOM INDEX ON u(col1) USING 'fulltext_index';\n\
         CREATE CUSTOM INDEX ON u(col2) USING 'fulltext_index';\n\
         CREATE INDEX ON u(plain);\n\
         CREATE TABLE w (p int, c text, PRIMARY KEY (p, c));\n\
         CREATE CUSTOM INDEX ON w(c) USING 'fulltext_index';\n\
         CREATE TABLE b (p int PRIMARY KEY, bm25 text);\n\
         CREATE CUSTOM INDEX ON b(bm25) USING 'fulltext_index';\n\
         INSERT INTO t (p, content) VALUES (1, 'hello world')",
    )
    .await;
    let q = |w: &str, rest: &str| -> String { format!("SELECT * FROM t WHERE {w} {rest}") };
    let cases: Vec<(String, X)> = vec![
        ("SELECT BM25(content, 'hello') FROM t".into(), X::Inv("BM25() is not supported in the SELECT clause")),
        ("SELECT p, BM25(content, 'world') FROM t WHERE BM25(content, 'hello') > 0 ORDER BY BM25(content, 'hello') LIMIT 1".into(), X::Inv("BM25() is not supported in the SELECT clause")),
        ("SELECT count(*) FROM t WHERE BM25(content, 'hello') > 0 ORDER BY BM25(content, 'hello') LIMIT 10".into(), X::Inv("Full-text search queries cannot be run with aggregation")),
        ("SELECT p FROM t WHERE BM25(content, 'hello') > 0 GROUP BY p ORDER BY BM25(content, 'hello') LIMIT 1".into(), X::Inv("Full-text search queries cannot be run with aggregation")),
        ("SELECT * FROM t ORDER BY BM25(content, 'hello') LIMIT 1".into(), X::Inv("Full-text search queries require a WHERE BM25() > 0 clause")),
        ("SELECT * FROM t ORDER BY now() LIMIT 1".into(), X::Inv("Only BM25 scoring function is supported in ORDER BY")),
        ("SELECT * FROM t WHERE BM25(content, 'hello') > 0 ORDER BY BM25_SCORE(content, 'hello') LIMIT 1".into(), X::Inv("Unknown function bm25_score called")),
        ("SELECT * FROM t WHERE BM25_RANK(content, 'hello') > 0 ORDER BY BM25(content, 'hello') LIMIT 1".into(), X::Inv("Unknown function bm25_rank called")),
        ("SELECT * FROM b WHERE BM25(bm25, 'hello') > 0 ORDER BY BM25(bm25, 'hello') LIMIT 1".into(), NO_STORE),
        ("SELECT * FROM w WHERE BM25(c, 'hello') > 0 ORDER BY BM25(c, 'hello') LIMIT 1".into(), NO_STORE),
        ("SELECT * FROM u WHERE BM25(col1, 'hello') > 0 AND BM25(col2, 'hello') > 0 ORDER BY BM25(col1, 'hello') LIMIT 1".into(), X::Inv("Full-text search queries support only one WHERE BM25() restriction")),
        ("SELECT * FROM u WHERE BM25(col1, 'hello') > 0 ORDER BY BM25(col2, 'hello') LIMIT 1".into(), X::Inv("Full-text search queries must reference the same column in both WHERE and ORDER BY clauses")),
        ("SELECT * FROM u WHERE BM25(plain, 'hello') > 0 ORDER BY BM25(plain, 'hello') LIMIT 1".into(), X::Inv("No fulltext index found for full-text search query")),
        (q("BM25(content) > 0", "ORDER BY BM25(content, 'hello') LIMIT 1"), X::Inv("Invalid number of arguments in call to function system.bm25: 2 required but 1 provided")),
        (q("BM25(content, 'hello', 'x') > 0", "ORDER BY BM25(content, 'hello') LIMIT 1"), X::Inv("Invalid number of arguments in call to function system.bm25: 2 required but 3 provided")),
        (q("BM25(content, 'hello') > 0", "ORDER BY BM25(content) LIMIT 1"), X::Inv("Invalid number of arguments in call to function system.bm25: 2 required but 1 provided")),
        (q("BM25(content, content) > 0", "ORDER BY BM25(content, 'world') LIMIT 1"), X::Inv("Second argument to BM25() must not be a column reference")),
        (q("BM25('content', 'hello') > 0", "ORDER BY BM25('content', 'hello') LIMIT 1"), X::Inv("First argument to BM25 must be a column reference")),
        (q("BM25(nonexistent, 'hello') > 0", "ORDER BY BM25(nonexistent, 'hello') LIMIT 1"), X::Inv("Unrecognized name nonexistent")),
        (q("BM25(vec, 'hello') > 0", "ORDER BY BM25(vec, 'hello') LIMIT 1"), X::Inv("Type error: vec cannot be passed as argument 0 of function system.bm25 of type text")),
        (q("BM25(content, 1) > 0", "ORDER BY BM25(content, 1) LIMIT 1"), X::Inv("Type error: 1 cannot be passed as argument 1 of function system.bm25 of type text")),
        (q("BM25(content, 'hello') > 0", "LIMIT 1"), X::Inv("Full-text search queries require an ORDER BY BM25() clause")),
        (q("BM25(content, 'hello') > 0", "ORDER BY BM25(content, 'hello')"), X::Inv("Full-text search queries require a LIMIT")),
        (q("p = 1 AND BM25(content, 'hello') > 0", "ALLOW FILTERING"), X::Inv("Full-text search queries require a LIMIT")),
        (q("BM25(content, 'hello') > 0", "ORDER BY BM25(content, 'hello') LIMIT 1001"), X::Inv("Full-text search queries require a LIMIT that is not greater than 1000. LIMIT was 1001")),
        (q("BM25(content, 'hello') > 0", "ORDER BY BM25(content, 'hello') LIMIT 1000"), NO_STORE),
        (q("BM25(content, 'hello') > 0", "ORDER BY BM25(content, 'hello'), BM25(content, 'world') LIMIT 1"), X::Inv("bm25 ordering does not support any other ordering")),
        (q("BM25(content, 'hello') > 0", "ORDER BY BM25(content, 'hello'), p LIMIT 1"), X::Inv("Similarity ordering does not support any other ordering")),
        (q("BM25(content, 'hello') > 0", "ORDER BY BM25(content, 'hello'), vec ANN OF [1.0, 2.0] LIMIT 1"), X::Inv("Cannot specify more than one similarity ordering")),
        (q("BM25(content, 'hello') > 0", "ORDER BY vec ANN OF [1.0, 2.0], BM25(content, 'hello') LIMIT 1"), X::Inv("bm25 ordering does not support any other ordering")),
        (q("BM25(content, 'hello') > 0", "ORDER BY vec ANN OF [1.0, 2.0] LIMIT 1"), X::Inv("BM25 and ANN cannot be combined in the same query")),
        (q("BM25(content, 'hello') > 0", "ORDER BY BM25(content, 'world') LIMIT 1"), X::Inv("Full-text search queries must use the same search term in both WHERE and ORDER BY clauses")),
        (q("BM25(content, 'hello') > 0", "ORDER BY BM25(content, null) LIMIT 1"), X::Inv("Full-text search queries must use the same search term in both WHERE and ORDER BY clauses")),
        (q("BM25(content, null) > 0", "ORDER BY BM25(content, null) LIMIT 1"), X::Inv("Full-text search query term must not be null")),
        (q("BM25(content, 'hello') = 0", "ORDER BY BM25(content, 'hello') LIMIT 1"), X::Inv("Unsupported \"=\" relation for BM25 function restriction, only \">\" is supported")),
        (q("BM25(content, 'hello') >= 0", "ORDER BY BM25(content, 'hello') LIMIT 1"), X::Inv("Unsupported \">=\" relation for BM25 function restriction, only \">\" is supported")),
        (q("BM25(content, 'hello') < 0", "ORDER BY BM25(content, 'hello') LIMIT 1"), X::Inv("Unsupported \"<\" relation for BM25 function restriction, only \">\" is supported")),
        (q("BM25(content, 'hello') != 0", "ORDER BY BM25(content, 'hello') LIMIT 1"), X::Inv("Unsupported \"!=\" relation: bm25(content, 'hello') != 0")),
        (q("BM25(content, 'hello') > 5", "ORDER BY BM25(content, 'hello') LIMIT 1"), X::Inv("BM25 function comparison value must be the literal 0")),
        (q("BM25(content, 'hello') > 0.0", "ORDER BY BM25(content, 'hello') LIMIT 1"), NO_STORE),
        (q("BM25(content, 'hello') > -0", "ORDER BY BM25(content, 'hello') LIMIT 1"), NO_STORE),
        (q("BM25(content, 'hello') LIKE 'x'", "LIMIT 1"), X::Inv("LIKE is allowed only on string types, which system.bm25(content, 'hello') is not")),
        (q("BM25(content, 'hello')", "ORDER BY BM25(content, 'hello') LIMIT 1"), X::Code(0x2000)),
        (q("BM25(content, 'hello') > 0", "ORDER BY BM25(content, 'hello') DESC LIMIT 1"), X::Syn("line 1:81 : Syntax error")),
        (q("BM25(content, 'hello') > 0", "ORDER BY BM25(content, 'hello') PER PARTITION LIMIT 5 LIMIT 10"), X::Inv("Full-text search queries do not support per-partition limits")),
        (q("p = 1 AND BM25(content, 'hello') > 0", "ORDER BY BM25(content, 'hello') LIMIT 1"), X::Inv("Full-text search queries do not support additional WHERE restrictions")),
        (q("tag = 'x' AND BM25(content, 'hello') > 0", "ORDER BY BM25(content, 'hello') LIMIT 1"), X::Inv("Full-text search queries do not support additional WHERE restrictions")),
        (q("BM25(content, 'hello') > 0 AND p = 1", "ORDER BY BM25(content, 'hello') LIMIT 1"), X::Inv("Full-text search queries do not support additional WHERE restrictions")),
        (q("system.bm25(content, 'hello') > 0", "ORDER BY system.bm25(content, 'hello') LIMIT 1"), NO_STORE),
        (q("tounixtimestamp(now()) > 0", "LIMIT 1"), X::Inv("Only the token function and scoring functions are supported in function-call restrictions")),
        (q("BM25(a, 'hello') > 0", "ORDER BY BM25(a, 'hello') LIMIT 1"), NO_STORE),
        (q("BM25(tag, 'hello') > 0", "ORDER BY BM25(tag, 'hello') LIMIT 1"), X::Inv("No fulltext index found for full-text search query")),
        (q("BM25(content, 'hello') > 0", "ORDER BY BM25(content, 'hello') LIMIT 1 ALLOW FILTERING"), NO_STORE),
        ("SELECT DISTINCT p FROM t WHERE BM25(content, 'hello') > 0 ORDER BY BM25(content, 'hello') LIMIT 1".into(), NO_STORE),
        ("UPDATE t SET content = 'x' WHERE p = 1 AND BM25(content, 'hello') > 0".into(), X::Inv("BM25() is only supported in SELECT statements")),
        ("DELETE FROM t WHERE p = 1 AND BM25(content, 'hello') > 0".into(), X::Inv("BM25() is only supported in SELECT statements")),
    ];
    let cases: Vec<(&str, X)> = cases.iter().map(|(c, x)| (c.as_str(), *x)).collect();
    expect(&mut h, &cases).await;
    // Bound terms: named and positional markers, checked when executed.
    let arg = ("arg1(system.bm25)".to_string(), "text".to_string());
    let two = "SELECT * FROM t WHERE BM25(content, ?) > 0 ORDER BY BM25(content, ?) LIMIT 1";
    for (values, want) in [
        (
            vec![text("hello"), text("hello")],
            "Vector Store is disabled",
        ),
        (
            vec![text("hello"), text("world")],
            "Full-text search queries must use the same search term in both WHERE and ORDER BY clauses",
        ),
        (
            vec![Param::Null, Param::Null],
            "Full-text search query term must not be null",
        ),
        (
            vec![text("a"), Param::Null],
            "Full-text search query term must not be null",
        ),
    ] {
        assert_eq!(
            run_prepared(&mut h, two, values).await,
            (vec![arg.clone(), arg.clone()], want.to_string())
        );
    }
    let (names, e) = run_prepared(
        &mut h,
        "SELECT * FROM t WHERE BM25(content, :q) > 0 ORDER BY BM25(content, :q) LIMIT 1",
        vec![text("hello")],
    )
    .await;
    assert_eq!(
        (names, e.as_str()),
        (
            vec![("q".to_string(), "text".to_string())],
            "Vector Store is disabled"
        )
    );
    let (_, e) = run_prepared(
        &mut h,
        "SELECT * FROM t WHERE BM25(content, ?) > 0 ORDER BY BM25(content, 'x') LIMIT 1",
        vec![text("y")],
    )
    .await;
    assert_eq!(
        e,
        "Full-text search queries must use the same search term in both WHERE and ORDER BY clauses"
    );
    let (names, e) = run_prepared(
        &mut h,
        "SELECT * FROM t WHERE BM25(content, 'x') > 0 ORDER BY BM25(content, 'x') LIMIT ?",
        vec![int(2000)],
    )
    .await;
    assert_eq!(
        (names, e.as_str()),
        (
            vec![("[limit]".to_string(), "int".to_string())],
            "Full-text search queries require a LIMIT that is not greater than 1000. LIMIT was 2000"
        )
    );
    assert_eq!(
        h.s.prepare(
            "SELECT * FROM t WHERE BM25(content, 'x') > ? ORDER BY BM25(content, 'x') LIMIT 1",
            None
        )
        .await
        .unwrap_err()
        .message,
        "BM25 function comparison value must be the literal 0"
    );
}

/// Cassandra SAI vector parameters: refused unless `cassandra.sai.vector.allow_custom_parameters`
/// (the conformance suite sets it), then range-checked as `IndexWriterConfig.fromOptions` does
/// (`test_create_vector_search_index_with_bad_numeric_value`).
#[tokio::test]
async fn sai_vector_custom_parameters() {
    for allow in [false, true] {
        let store = Store::start(nosql_storage::test_objects("engine"), small_store_config())
            .await
            .unwrap();
        let handle = store
            .open("db", nosql_storage::db::DbConfig::default())
            .await
            .unwrap();
        let mut cfg = DbConfig::new("db", Dialect::Cassandra);
        cfg.settings.sai_vector_allow_custom_parameters = allow;
        let db = Database::open(cfg, handle).await.unwrap();
        let s = db.connect(Default::default());
        let mut h = H { db, s, store };
        h.exec(&format!(
            "{KS};\nUSE ks;\nCREATE TABLE t (p int PRIMARY KEY, v vector<float, 3>)"
        ))
        .await;
        let o = |opts: &str| {
            format!("CREATE CUSTOM INDEX ON t(v) USING 'sai' WITH OPTIONS = {{{opts}}}")
        };
        let cases: Vec<(String, X)> = if allow {
            vec![
                (
                    o("'maximum_node_connections': '-1'"),
                    X::Inv(
                        "Maximum number of connections for index null cannot be <= 0 or > 512, was -1",
                    ),
                ),
                (
                    o("'maximum_node_connections': '513'"),
                    X::Inv(
                        "Maximum number of connections for index null cannot be <= 0 or > 512, was 513",
                    ),
                ),
                (
                    o("'maximum_node_connections': 'x'"),
                    X::Inv("Maximum number of connections x is not a valid integer for index null"),
                ),
                (
                    o("'construction_beam_width': '0'"),
                    X::Inv(
                        "Construction beam width for index null cannot be <= 0 or > 3200, was 0",
                    ),
                ),
                (
                    o("'construction_beam_width': '1.5'"),
                    X::Inv("Construction beam width 1.5 is not a valid integer for index null"),
                ),
                (
                    o("'maximum_node_connections': '16', 'construction_beam_width': '100'"),
                    X::Ok,
                ),
            ]
        } else {
            vec![
                (
                    o("'maximum_node_connections': '-1'"),
                    X::Inv(
                        "Maximum node connections cannot be set without enabling SAI_VECTOR_ALLOW_CUSTOM_PARAMETERS",
                    ),
                ),
                (
                    o("'construction_beam_width': '10'"),
                    X::Inv(
                        "Construction beam width cannot be set without enabling SAI_VECTOR_ALLOW_CUSTOM_PARAMETERS",
                    ),
                ),
            ]
        };
        let cases: Vec<(&str, X)> = cases.iter().map(|(c, x)| (c.as_str(), *x)).collect();
        expect(&mut h, &cases).await;
    }
}
