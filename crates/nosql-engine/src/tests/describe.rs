//! `DESCRIBE` behaviour the captures don't show (verified on both references with the Python
//! driver): ScyllaDB's roles, grants and service levels, paging, `DESCRIBE CLUSTER`'s ranges,
//! listings of the current keyspace, name resolution, errors, dropped and re-added columns,
//! type dependency order, and the [`crate::describe`] API.

use super::*;
use crate::describe::{self, Kind, Options};

/// `(keyspace, type, name, statement)` rows.
async fn describe(h: &mut H, cql: &str) -> Vec<(String, String, String, String)> {
    let r = h.ok(cql).await;
    let rows = r.rows().expect("rows");
    rows.rows
        .iter()
        .map(|r| {
            let s = |i: usize| {
                r.get(i)
                    .cloned()
                    .flatten()
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    .unwrap_or_else(|| "null".into())
            };
            (s(0), s(1), s(2), s(3))
        })
        .collect()
}

async fn auth_db(d: Dialect) -> H {
    let store = nosql_storage::store::Store::start(
        nosql_storage::test_objects("engine"),
        small_store_config(),
    )
    .await
    .unwrap();
    let handle = store
        .open("db", nosql_storage::db::DbConfig::default())
        .await
        .unwrap();
    let mut cfg = DbConfig::new("db", d);
    cfg.settings.authenticator = "PasswordAuthenticator".into();
    cfg.settings.authorizer = "CassandraAuthorizer".into();
    cfg.superuser = Some(crate::config::Superuser {
        name: "cassandra".into(),
        password: "cassandra".into(),
    });
    let db = Database::open(cfg, handle).await.unwrap();
    let mut s = db.connect(Default::default());
    s.authenticate(b"\0cassandra\0cassandra").await.unwrap();
    H { db, s, store }
}

#[tokio::test]
async fn scylla_schema_with_internals_lists_roles_grants_and_service_levels() {
    let mut h = auth_db(Dialect::Scylla).await;
    h.exec(
        "CREATE ROLE desc_r1 WITH PASSWORD = 'pw' AND LOGIN = true;\n\
         CREATE ROLE desc_r2;\n\
         CREATE ROLE \"Desc R3\" WITH SUPERUSER = true;\n\
         GRANT desc_r2 TO desc_r1;\n\
         GRANT desc_r2 TO \"Desc R3\";\n\
         CREATE KEYSPACE desc_ks WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1};\n\
         CREATE TABLE desc_ks.t (k int PRIMARY KEY);\n\
         GRANT SELECT ON ALL KEYSPACES TO desc_r1;\n\
         GRANT MODIFY ON KEYSPACE desc_ks TO desc_r1;\n\
         GRANT SELECT ON desc_ks.t TO desc_r2;\n\
         GRANT ALTER ON ROLE desc_r2 TO desc_r1;\n\
         GRANT CREATE ON ALL ROLES TO desc_r1;\n\
         GRANT EXECUTE ON ALL FUNCTIONS TO desc_r1;\n\
         GRANT EXECUTE ON ALL FUNCTIONS IN KEYSPACE desc_ks TO desc_r2;\n\
         CREATE SERVICE LEVEL desc_sl WITH timeout = 1s AND workload_type = 'interactive' AND shares = 300;\n\
         ATTACH SERVICE LEVEL desc_sl TO desc_r1",
    )
    .await;
    let rows: Vec<(String, String, String, String)> =
        describe(&mut h, "DESCRIBE SCHEMA WITH INTERNALS")
            .await
            .into_iter()
            .filter(|r| r.0 == "null")
            .collect();
    let own: Vec<(&str, &str, &str)> = rows
        .iter()
        .map(|r| (r.1.as_str(), r.2.as_str(), r.3.as_str()))
        .filter(|(_, _, s)| !s.contains("TO cassandra;"))
        .collect();
    assert_eq!(
        own,
        [
            (
                "role",
                "\"Desc R3\"",
                "CREATE ROLE \"Desc R3\" WITH LOGIN = false AND SUPERUSER = true;"
            ),
            (
                "role",
                "cassandra",
                "CREATE ROLE IF NOT EXISTS cassandra WITH LOGIN = true AND SUPERUSER = true;"
            ),
            (
                "role",
                "desc_r1",
                "CREATE ROLE desc_r1 WITH LOGIN = true AND SUPERUSER = false;"
            ),
            (
                "role",
                "desc_r2",
                "CREATE ROLE desc_r2 WITH LOGIN = false AND SUPERUSER = false;"
            ),
            ("grant_role", "desc_r2", "GRANT desc_r2 TO \"Desc R3\";"),
            ("grant_role", "desc_r2", "GRANT desc_r2 TO desc_r1;"),
            (
                "grant_permission",
                "desc_r1",
                "GRANT ALTER ON ROLE desc_r2 TO desc_r1;"
            ),
            (
                "grant_permission",
                "desc_r1",
                "GRANT CREATE ON ALL ROLES TO desc_r1;"
            ),
            (
                "grant_permission",
                "desc_r1",
                "GRANT EXECUTE ON ALL FUNCTIONS TO desc_r1;"
            ),
            (
                "grant_permission",
                "desc_r1",
                "GRANT MODIFY ON KEYSPACE desc_ks TO desc_r1;"
            ),
            (
                "grant_permission",
                "desc_r1",
                "GRANT SELECT ON ALL KEYSPACES TO desc_r1;"
            ),
            (
                "grant_permission",
                "desc_r2",
                "GRANT EXECUTE ON ALL FUNCTIONS IN KEYSPACE desc_ks TO desc_r2;"
            ),
            (
                "grant_permission",
                "desc_r2",
                "GRANT SELECT ON desc_ks.t TO desc_r2;"
            ),
            (
                "service_level",
                "driver",
                "CREATE SERVICE LEVEL IF NOT EXISTS driver WITH WORKLOAD_TYPE = 'batch' AND SHARES = 200;"
            ),
            (
                "service_level",
                "driver",
                "ALTER SERVICE LEVEL driver WITH WORKLOAD_TYPE = 'batch' AND SHARES = 200;"
            ),
            (
                "service_level",
                "desc_sl",
                "CREATE SERVICE LEVEL desc_sl WITH TIMEOUT = 1000ms AND WORKLOAD_TYPE = 'interactive' AND SHARES = 300;"
            ),
            (
                "service_level_attachment",
                "desc_sl",
                "ATTACH SERVICE LEVEL desc_sl TO desc_r1;"
            ),
        ]
    );
    // The creator's permissions, in statement order (keyspaces, roles, tables).
    let creator: Vec<&str> = rows
        .iter()
        .filter(|r| r.3.contains("TO cassandra;"))
        .map(|r| r.3.as_str())
        .take(6)
        .collect();
    assert_eq!(
        creator,
        [
            "GRANT ALTER ON KEYSPACE desc_ks TO cassandra;",
            "GRANT ALTER ON ROLE \"Desc R3\" TO cassandra;",
            "GRANT ALTER ON ROLE desc_r1 TO cassandra;",
            "GRANT ALTER ON ROLE desc_r2 TO cassandra;",
            "GRANT ALTER ON desc_ks.t TO cassandra;",
            "GRANT AUTHORIZE ON KEYSPACE desc_ks TO cassandra;",
        ]
    );
    // AND PASSWORDS: the hashes of roles with passwords (superusers only).
    let rows = describe(&mut h, "DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS").await;
    let r1 = rows
        .iter()
        .find(|r| r.1 == "role" && r.2 == "desc_r1")
        .unwrap();
    assert!(
        r1.3.starts_with("CREATE ROLE desc_r1 WITH HASHED PASSWORD = '$"),
        "{}",
        r1.3
    );
    assert!(r1.3.ends_with("' AND LOGIN = true AND SUPERUSER = false;"));
    let r2 = rows
        .iter()
        .find(|r| r.1 == "role" && r.2 == "desc_r2")
        .unwrap();
    assert_eq!(
        r2.3,
        "CREATE ROLE desc_r2 WITH LOGIN = false AND SUPERUSER = false;"
    );
    // A non-superuser sees roles and grants, but not passwords.
    let mut alice = h.db.connect(Default::default());
    alice.authenticate(b"\0desc_r1\0pw").await.unwrap();
    let r = alice
        .query("DESCRIBE SCHEMA WITH INTERNALS", QueryParams::default())
        .await
        .unwrap();
    assert!(
        r.rows()
            .unwrap()
            .rows
            .iter()
            .any(|r| r[1].as_deref() == Some(&b"grant_role"[..]))
    );
    let e = alice
        .query(
            "DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS",
            QueryParams::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Unauthorized);
    assert_eq!(
        e.message,
        "DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS can only be issued by a superuser"
    );
    // Without INTERNALS: no auth rows.
    assert!(
        describe(&mut h, "DESCRIBE SCHEMA")
            .await
            .iter()
            .all(|r| r.0 != "null")
    );
}

#[tokio::test]
async fn passwords_option_rules() {
    let mut h = H::new(Dialect::Scylla).await;
    h.exec(&format!("{KS};\nCREATE TABLE ks.t (k int PRIMARY KEY)"))
        .await;
    let e = h
        .err("DESCRIBE TABLE ks.t WITH INTERNALS AND PASSWORDS")
        .await;
    assert_eq!(e.code, ErrorCode::Invalid);
    assert_eq!(
        e.message,
        "Option WITH PASSWORDS is only allowed with DESC SCHEMA"
    );
    // Without authentication nobody is a superuser.
    let e = h.err("DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS").await;
    assert_eq!(e.code, ErrorCode::Unauthorized);
    let e = h.err("DESCRIBE SCHEMA WITH PASSWORDS").await;
    assert_eq!(e.code, ErrorCode::Syntax);
    assert_eq!(e.message, "line 1:21 : Missing K_INTERNALS");
    let mut c = H::new(Dialect::Cassandra).await;
    let e = c.err("DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS").await;
    assert_eq!(
        e.message,
        "line 1:31 mismatched input 'AND' expecting EOF (DESCRIBE SCHEMA WITH INTERNALS [AND]...)"
    );
}

#[tokio::test]
async fn cassandra_pages_and_scylla_does_not() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.a (k int PRIMARY KEY);\nCREATE TABLE ks.b (k int PRIMARY KEY)"
        ))
        .await;
        let all = describe(&mut h, "DESCRIBE KEYSPACES").await;
        let (first, state) = h
            .rows_with("DESCRIBE KEYSPACES", QueryParams::default().paged(3))
            .await;
        match d {
            Dialect::Scylla => {
                assert_eq!(first.len(), all.len());
                assert_eq!(state, None);
            }
            Dialect::Cassandra => {
                assert_eq!(first.len(), 3);
                let state = state.expect("paging state");
                assert_eq!(&state[..10], b"!\x00\x01\x00\x055.0.9");
                let (second, next) = h
                    .rows_with(
                        "DESCRIBE KEYSPACES",
                        QueryParams {
                            paging_state: Some(state.clone()),
                            ..QueryParams::default().paged(3)
                        },
                    )
                    .await;
                assert_eq!(second[0][0], all[3].0);
                assert!(next.is_some());
                // The schema changed since the first page.
                h.ok("CREATE TABLE ks.c (k int PRIMARY KEY)").await;
                let e =
                    h.s.query(
                        "DESCRIBE KEYSPACES",
                        QueryParams {
                            paging_state: Some(state),
                            ..QueryParams::default().paged(3)
                        },
                    )
                    .await
                    .unwrap_err();
                assert_eq!(
                    e.message,
                    "The schema has changed since the previous page of the DESCRIBE statement result. Please retry the DESCRIBE statement."
                );
                // The last page has no state.
                let (rows, state) = h
                    .rows_with("DESCRIBE KEYSPACE ks", QueryParams::default().paged(10))
                    .await;
                assert_eq!(rows.len(), 4);
                assert_eq!(state, None);
            }
        }
    }
}

#[tokio::test]
async fn describe_cluster_and_ranges() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS}{};\nUSE ks",
            if d.is_scylla() {
                " AND tablets = {'enabled': false}"
            } else {
                ""
            }
        ))
        .await;
        let r = h.ok("DESCRIBE CLUSTER").await;
        let rows = r.rows().unwrap();
        let names: Vec<&str> = rows
            .metadata
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["cluster", "partitioner", "snitch", "range_ownership"]
        );
        let row = fmt_rows(&r);
        match d {
            Dialect::Scylla => {
                assert_eq!(
                    row[0][..3],
                    [
                        "Test Cluster",
                        "org.apache.cassandra.dht.Murmur3Partitioner",
                        "org.apache.cassandra.locator.SimpleSnitch"
                    ]
                );
                assert!(row[0][3].contains(": ['127.0.0.1']"));
            }
            Dialect::Cassandra => {
                assert_eq!(
                    row[0][..3],
                    [
                        "Test Cluster",
                        "Murmur3Partitioner",
                        "DynamicEndpointSnitch"
                    ]
                );
                assert!(row[0][3].contains(": ['/127.0.0.1:7000']"));
            }
        }
        assert_eq!(
            nosql_types::cql_name(&rows.metadata.columns[3].ty),
            "map<text, list<text>>"
        );
        // A system keyspace (and ScyllaDB's tablets keyspaces): no ranges.
        h.ok("USE system").await;
        assert_eq!(h.columns("DESCRIBE CLUSTER").await.len(), 3);
        if d.is_scylla() {
            h.exec("CREATE KEYSPACE tk WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1};\nUSE tk")
                .await;
            assert_eq!(h.columns("DESCRIBE CLUSTER").await.len(), 3);
        }
    }
}

#[tokio::test]
async fn listings_follow_the_current_keyspace() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TYPE ks.zz (a int);\nCREATE TYPE ks.aa (a int);\nCREATE TYPE ks.mm (a int, z frozen<zz>);\nCREATE TYPE ks.bb (a int);\nCREATE TYPE ks.c1 (m frozen<mm>);\nCREATE TABLE ks.\"Up\" (k int PRIMARY KEY);\nCREATE TABLE ks.low (k int PRIMARY KEY)"
        ))
        .await;
        // DESCRIBE KEYSPACE: types in dependency order (depth first over the names).
        let types: Vec<String> = describe(&mut h, "DESCRIBE KEYSPACE ks")
            .await
            .into_iter()
            .filter(|r| r.1 == "type")
            .map(|r| r.2)
            .collect();
        assert_eq!(types, ["aa", "bb", "zz", "mm", "c1"]);
        h.ok("USE ks").await;
        let listed: Vec<String> = describe(&mut h, "DESC TYPES")
            .await
            .into_iter()
            .map(|r| r.2)
            .collect();
        match d {
            // Cassandra sorts listings by name.
            Dialect::Cassandra => assert_eq!(listed, ["aa", "bb", "c1", "mm", "zz"]),
            Dialect::Scylla => assert_eq!(listed, ["aa", "bb", "zz", "mm", "c1"]),
        }
        let tables: Vec<String> = describe(&mut h, "DESC TABLES")
            .await
            .into_iter()
            .map(|r| r.2)
            .collect();
        match d {
            Dialect::Cassandra => assert_eq!(tables, ["low", "\"Up\""]),
            Dialect::Scylla => assert_eq!(tables, ["\"Up\"", "low"]),
        }
        assert_eq!(
            h.columns("DESC TABLES").await,
            ["keyspace_name", "type", "name"]
        );
        // DESCRIBE TABLE without a keyspace uses the current one.
        assert_eq!(describe(&mut h, "DESC TABLE low").await.len(), 1);
        assert!(describe(&mut h, "DESC FUNCTIONS").await.is_empty());
        assert!(describe(&mut h, "DESC AGGREGATES").await.is_empty());
    }
}

#[tokio::test]
async fn generic_names_and_errors() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.base (id int PRIMARY KEY, v int);\nCREATE INDEX base_idx ON ks.base (v)"
        ))
        .await;
        // A keyspace name, a table, an index.
        assert_eq!(describe(&mut h, "DESC ks").await[0].1, "keyspace");
        assert_eq!(describe(&mut h, "DESC ks.base").await[0].1, "table");
        let idx = describe(&mut h, "DESC ks.base_idx").await;
        assert_eq!(idx.len(), 1);
        assert_eq!(idx[0].1, "index");
        match d {
            Dialect::Cassandra => assert_eq!(idx[0].3, "CREATE INDEX base_idx ON ks.base (v);"),
            Dialect::Scylla => {
                assert!(idx[0].3.starts_with(
                    "CREATE INDEX base_idx ON ks.base(v) WITH bloom_filter_fp_chance = 0.01\n"
                ))
            }
        }
        // ScyllaDB also finds user types (a table of the same name wins); Cassandra doesn't.
        h.exec("CREATE TYPE ks.onlytype (a int);\nCREATE TYPE ks.base (a int)").await;
        match d {
            Dialect::Scylla => {
                let ty = describe(&mut h, "DESC ks.onlytype").await;
                assert_eq!((ty[0].1.as_str(), ty[0].3.as_str()), ("type", "CREATE TYPE ks.onlytype (\n    a int\n);"));
                assert_eq!(describe(&mut h, "DESC ks.base").await[0].1, "table");
            }
            Dialect::Cassandra => assert_eq!(h.msg("DESC ks.onlytype").await, "'onlytype' not found in keyspace 'ks'"),
        }
        h.exec("DROP TYPE ks.onlytype;\nDROP TYPE ks.base").await;
        h.ok("USE ks").await;
        assert_eq!(
            h.msg("DESC nope").await,
            "'nope' not found in keyspace 'ks'"
        );
        assert_eq!(
            h.msg("DESC TABLE nope.t").await,
            "'nope' not found in keyspaces"
        );
        assert_eq!(
            h.msg("DESC TYPE nope.t").await,
            "'nope' not found in keyspaces"
        );
        assert_eq!(
            h.msg("DESC INDEX ks.base").await,
            "Table for existing index 'base' not found in 'ks'"
        );
        let view_missing = match d {
            Dialect::Scylla => "Materialized view 'base' not found in keyspace 'ks'",
            Dialect::Cassandra => "Materialized view 'base' not found in 'ks'",
        };
        assert_eq!(h.msg("DESC MATERIALIZED VIEW ks.base").await, view_missing);
        let ty = match d {
            Dialect::Scylla => "Type 'base' not found in keyspace 'ks'",
            Dialect::Cassandra => "User defined type 'base' not found in 'ks'",
        };
        assert_eq!(h.msg("DESC TYPE ks.base").await, ty);
        let f = match d {
            Dialect::Scylla => "Function 'nope' not found in keyspace 'ks'",
            Dialect::Cassandra => "User defined function 'nope' not found in 'ks'",
        };
        assert_eq!(h.msg("DESC FUNCTION ks.nope").await, f);
        assert_eq!(
            h.msg("DESC AGGREGATE nope.nope").await,
            "'nope' not found in keyspaces"
        );
        let a = match d {
            Dialect::Scylla => "Aggregate 'nope' not found in keyspace 'ks'",
            Dialect::Cassandra => "User defined aggregate 'nope' not found in 'ks'",
        };
        assert_eq!(h.msg("DESC AGGREGATE nope").await, a);
        if d.is_scylla() {
            // The index's internal view: informational only.
            let v = describe(&mut h, "DESC MATERIALIZED VIEW ks.base_idx_index").await;
            assert_eq!(v[0].1, "view");
            assert!(v[0].3.starts_with(
                "/* Do NOT execute this statement! It's only for informational purposes.\n   This materialized view is the underlying materialized view of a secondary\n   index. It can be restored via restoring the index.\n\nCREATE MATERIALIZED VIEW ks.base_idx_index AS\n"
            ));
            assert!(v[0].3.ends_with(";\n\n*/"));
            assert_eq!(
                h.msg("DESC TABLE ks.base_idx_index").await,
                "Cannot use DESC TABLE on materialized View. (Did you mean DESC MATERIALIZED VIEW)?"
            );
            assert_eq!(describe(&mut h, "DESC base_idx_index").await[0].1, "view");
            // Not listed with the keyspace.
            assert!(
                describe(&mut h, "DESC KEYSPACE ks")
                    .await
                    .iter()
                    .all(|r| r.2 != "base_idx_index")
            );
        } else {
            assert_eq!(
                h.msg("DESC TABLE ks.base_idx_index").await,
                "Table 'base_idx_index' not found in keyspace 'ks'"
            );
        }
    }
}

#[tokio::test]
async fn dropped_and_re_added_columns() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.dr2 (k int, c int, s int static, a int, b int, PRIMARY KEY (k, c));\nALTER TABLE ks.dr2 DROP a;\nALTER TABLE ks.dr2 DROP s;\nALTER TABLE ks.dr2 ADD a int"
        ))
        .await;
        let plain = describe(&mut h, "DESCRIBE TABLE ks.dr2").await;
        assert!(plain[0].3.starts_with(
            "CREATE TABLE ks.dr2 (\n    k int,\n    c int,\n    a int,\n    b int,\n    PRIMARY KEY (k, c)\n) WITH CLUSTERING ORDER BY (c ASC)\n"
        ));
        let s = describe(&mut h, "DESCRIBE TABLE ks.dr2 WITH INTERNALS").await;
        let stmt = &s[0].3;
        let id = crate::uuid::format(&h.db.schema().table("ks", "dr2").unwrap().id);
        let schema = h.db.schema();
        let t = schema.table("ks", "dr2").unwrap();
        let (ta, ts) = (t.dropped["a"].dropped_at, t.dropped["s"].dropped_at);
        match d {
            Dialect::Scylla => {
                assert!(stmt.starts_with(&format!(
                    "CREATE TABLE ks.dr2 (\n    k int,\n    c int,\n    b int,\n    s int,\n    a int,\n    PRIMARY KEY (k, c)\n) WITH ID = {id}\nAND CLUSTERING ORDER BY (c ASC)\n    AND bloom_filter_fp_chance = 0.01\n"
                )), "{stmt}");
                // The reference's order (its unordered map of dropped columns).
                assert!(stmt.ends_with(&format!(
                    "'3600'}};\n\nALTER TABLE ks.dr2 DROP s USING TIMESTAMP {ts};\nALTER TABLE ks.dr2 DROP a USING TIMESTAMP {ta};\nALTER TABLE ks.dr2 ADD a int;"
                )), "{stmt}");
            }
            Dialect::Cassandra => {
                let (ta, ts) = (ta / 1000 * 1000, ts / 1000 * 1000);
                assert!(stmt.starts_with(&format!(
                    "CREATE TABLE ks.dr2 (\n    k int,\n    c int,\n    b int,\n    a int,\n    s int static,\n    PRIMARY KEY (k, c)\n) WITH ID = {id}\n    AND CLUSTERING ORDER BY (c ASC)\n    AND additional_write_policy = '99p'\n"
                )), "{stmt}");
                assert!(stmt.ends_with(&format!(
                    "AND speculative_retry = '99p';\nALTER TABLE ks.dr2 DROP a USING TIMESTAMP {ta};\nALTER TABLE ks.dr2 ADD a int;\nALTER TABLE ks.dr2 DROP s USING TIMESTAMP {ts};"
                )), "{stmt}");
            }
        }
    }
}

#[tokio::test]
async fn single_column_primary_keys_and_quoting() {
    let mut h = H::new(Dialect::Cassandra).await;
    h.exec(&format!(
        "{KS};\nCREATE TABLE ks.\"Odd\" (\"K\" int PRIMARY KEY, \"select\" text) WITH comment = 'it''s'"
    ))
    .await;
    let r = describe(&mut h, "DESCRIBE KEYSPACE ks").await;
    assert_eq!(r[1].2, "\"Odd\"");
    assert!(r[1].3.starts_with(
        "CREATE TABLE ks.\"Odd\" (\n    \"K\" int PRIMARY KEY,\n    \"select\" text\n) WITH additional_write_policy = '99p'\n"
    ));
    assert!(r[1].3.contains("\n    AND comment = 'it''s'\n"));
}

#[tokio::test]
async fn the_api_returns_elements() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TYPE ks.addr (street text);\nCREATE TABLE ks.t (k int PRIMARY KEY, a frozen<addr>)"
        ))
        .await;
        let db = h.db.clone();
        let schema = db.schema();
        let all = describe::schema(&db, &schema, false, Options::default());
        let kinds: Vec<(Option<&str>, Kind, &str)> = all
            .iter()
            .map(|e| (e.keyspace.as_deref(), e.kind, e.name.as_str()))
            .collect();
        let mut want = vec![
            (Some("ks"), Kind::Keyspace, "ks"),
            (Some("ks"), Kind::Type, "addr"),
            (Some("ks"), Kind::Table, "t"),
        ];
        if d.is_scylla() {
            want.insert(0, (Some("audit"), Kind::Table, "audit_log"));
            want.insert(0, (Some("audit"), Kind::Keyspace, "audit"));
        }
        assert_eq!(kinds, want);
        let full = describe::schema(&db, &schema, true, Options::default());
        assert!(full.len() > 90);
        let t = describe::table(&db, &schema, "ks", "t", Options::default()).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].statement, all.last().unwrap().statement);
        assert_eq!(
            describe::keyspace(&db, &schema, "nope", false, Options::default())
                .unwrap_err()
                .message,
            "'nope' not found in keyspaces"
        );
        assert_eq!(
            Kind::View.name(d),
            if d.is_scylla() {
                "view"
            } else {
                "materialized_view"
            }
        );
        let row = all[if d.is_scylla() { 2 } else { 0 }].row(d);
        assert_eq!(row[1].as_deref(), Some(&b"keyspace"[..]));
    }
}

#[tokio::test]
async fn scylla_cdc_logs_print_as_alter_table() {
    let mut h = H::new(Dialect::Scylla).await;
    h.exec(&format!(
        "{KS};\nCREATE TABLE ks.t (k int PRIMARY KEY, v int) WITH cdc = {{'enabled': true}}"
    ))
    .await;
    if h.db.schema().table("ks", "t_scylla_cdc_log").is_none() {
        // Until the CDC subsystem creates the log, stand one in with the log's options.
        h.ok("CREATE TABLE ks.t_scylla_cdc_log (k int PRIMARY KEY) WITH comment = 'CDC log for ks.t' AND gc_grace_seconds = 0")
            .await;
    }
    let ks = describe(&mut h, "DESCRIBE KEYSPACE ks").await;
    let log = ks.iter().find(|r| r.2 == "t_scylla_cdc_log").unwrap();
    assert_eq!(log.1, "table");
    assert!(
        log.3.starts_with(
            "ALTER TABLE ks.t_scylla_cdc_log WITH bloom_filter_fp_chance = 0.01\n    AND caching = "
        ),
        "{}",
        log.3
    );
    assert!(log.3.contains("\n    AND comment = 'CDC log for ks.t'\n"));
    assert!(log.3.ends_with(";\n"));
    let t = describe(&mut h, "DESCRIBE TABLE ks.t").await;
    assert_eq!(t.len(), 2);
    assert_eq!(t[1].2, "t_scylla_cdc_log");
    assert!(
        t[0].3
            .contains("\n    AND cdc = {'delta': 'full', 'enabled': 'true', 'postimage': 'false', 'preimage': 'false', 'ttl': '86400'}\n")
    );
    h.ok("USE ks").await;
    let tables: Vec<String> = describe(&mut h, "DESC TABLES")
        .await
        .into_iter()
        .map(|r| r.2)
        .collect();
    assert_eq!(tables, ["t"]);
}

/// ScyllaDB's Paxos state table (`t$paxos`), probed on the reference: it exists after an LWT
/// operation in a tablets keyspace and describes as an informational comment.
#[tokio::test]
async fn scylla_paxos_table() {
    let mut h = H::new(Dialect::Scylla).await;
    h.exec(
        "CREATE KEYSPACE tb WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1} AND tablets = {'enabled': true};\n\
         CREATE TABLE tb.t (p int PRIMARY KEY, x int);\n\
         CREATE TABLE tb.s (p int PRIMARY KEY, x int);\n\
         CREATE KEYSPACE vn WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1} AND tablets = {'enabled': false};\n\
         CREATE TABLE vn.t (p int PRIMARY KEY, x int)",
    )
    .await;
    assert_eq!(h.msg(r#"DESC TABLE tb."t$paxos""#).await, "Table 't$paxos' not found in keyspace 'tb'");
    h.ok("INSERT INTO tb.t (p, x) VALUES (1, 2) IF NOT EXISTS").await;
    h.ok("INSERT INTO vn.t (p, x) VALUES (1, 2) IF NOT EXISTS").await;
    let want = "/* Do NOT execute this statement! It's only for informational purposes.\n   A paxos state table is created automatically when enabling LWT on a base table.\n\nCREATE TABLE tb.\"t$paxos\" (\n    row_key blob,\n    most_recent_commit blob,\n    most_recent_commit_at timeuuid,\n    promise timeuuid,\n    proposal blob,\n    proposal_ballot timeuuid,\n    PRIMARY KEY (row_key)\n) WITH bloom_filter_fp_chance = 0.01\n    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}\n    AND comment = 'Paxos state for tb.t'\n    AND compaction = {'class': 'IncrementalCompactionStrategy'}\n    AND compression = {'sstable_compression': 'LZ4WithDictsCompressor'}\n    AND crc_check_chance = 1\n    AND default_time_to_live = 0\n    AND gc_grace_seconds = 0\n    AND max_index_interval = 2048\n    AND memtable_flush_period_in_ms = 0\n    AND min_index_interval = 128\n    AND speculative_retry = '99.0PERCENTILE';\n\n*/";
    for cql in [r#"DESC TABLE tb."t$paxos""#, r#"DESC tb."t$paxos""#] {
        let rows = h.rows(cql).await;
        assert_eq!(rows.len(), 1, "{cql}");
        assert_eq!((rows[0][1].as_str(), rows[0][2].as_str(), rows[0][3].as_str()), ("table", "\"t$paxos\"", want), "{cql}");
    }
    // Not in DESC KEYSPACE; vnodes keyspaces have none; a SERIAL read creates it too.
    assert!(h.rows("DESC KEYSPACE tb").await.iter().all(|r| !r[2].contains("paxos")));
    assert!(h.err(r#"DESC TABLE vn."t$paxos""#).await.message.contains("not found"));
    h.ok("USE tb").await;
    let p = QueryParams {
        consistency: crate::error::Consistency::Serial,
        ..Default::default()
    };
    h.s.query("SELECT * FROM tb.s WHERE p = 1", p).await.unwrap();
    assert_eq!(h.rows(r#"DESC TABLE tb."s$paxos""#).await.len(), 1);
}

/// ScyllaDB's `scylla_tags` property, as DESCRIBE prints it (so its output restores), probed
/// on the reference.
#[tokio::test]
async fn scylla_tags_property() {
    let mut h = H::new(Dialect::Scylla).await;
    h.exec(&format!(
        "{KS};\nCREATE TABLE ks.tg (p int PRIMARY KEY, v int) WITH scylla_tags = {{'a': 'b', 'system:ttl_attribute': 'v'}};\n\
         CREATE TABLE ks.e (p int PRIMARY KEY) WITH scylla_tags = {{}};\n\
         CREATE INDEX tgi ON ks.tg (v) WITH scylla_tags = {{'system:synchronous_view_updates': 'true'}}"
    ))
    .await;
    let d = &h.rows("DESC TABLE ks.tg").await[0][3];
    assert!(d.contains("    v int TTL,\n") && d.contains("    AND scylla_tags = {'a': 'b', 'system:ttl_attribute': 'v'}\n"), "{d}");
    assert!(h.rows("DESC TABLE ks.e").await[0][3].contains("    AND scylla_tags = {}\n"));
    let i = &h.rows("DESC INDEX ks.tgi").await[0][3];
    assert!(i.contains("    AND scylla_tags = {'system:synchronous_view_updates': 'true'}\n") && i.ends_with("    AND synchronous_updates = true;\n"), "{i}");
    let e = h.err("CREATE TABLE ks.x (p int PRIMARY KEY) WITH scylla_tags = 'x'").await;
    assert_eq!((e.code, e.message.as_str()), (ErrorCode::Server, "Cannot create tags from string"));
    // The index statement DESCRIBE prints runs again.
    let stmt = i.clone();
    h.ok("DROP INDEX ks.tgi").await;
    h.ok(stmt.trim_end_matches(";\n")).await;
    assert_eq!(&h.rows("DESC INDEX ks.tgi").await[0][3], i);
    // ALTER replaces the map.
    h.ok("ALTER TABLE ks.tg WITH scylla_tags = {'c': 'd'}").await;
    let d = &h.rows("DESC TABLE ks.tg").await[0][3];
    assert!(d.contains("    v int,\n") && d.contains("    AND scylla_tags = {'c': 'd'}\n"), "{d}");
}
