//! Dynamic data masking (Cassandra 5) and the platform masking policy: what the captures can't
//! show. The references' answers with masking enabled (DDL, `system_schema.column_masks`,
//! DESCRIBE, reads per permission, restrictions, LWT, views) are replayed by
//! `captured_masking`; `dynamic_data_masking_enabled: false` and the plain masking functions come
//! from the default images (`functions_c5.rs`). Here: the disabled setting, restarts, prepared
//! statements, live setting changes and the platform policy (roll's own).

use super::captured_auth_harness::{auth_config, open, plain};
use super::*;
use crate::config::MaskCall;
use crate::masking::DISABLED;

async fn login(db: &Arc<Database>, user: &str, pw: &str) -> Session {
    let mut s = db.connect(Default::default());
    s.authenticate(&plain(user, pw))
        .await
        .unwrap_or_else(|e| panic!("login {user}: {e}"));
    s
}

async fn ok(s: &mut Session, cql: &str) -> Response {
    s.query(cql, QueryParams::default())
        .await
        .unwrap_or_else(|e| panic!("{cql}: {e}"))
}

async fn err(s: &mut Session, cql: &str) -> Error {
    match s.query(cql, QueryParams::default()).await {
        Ok(r) => panic!("{cql}: expected an error, got {:?}", r.body),
        Err(e) => e,
    }
}

async fn rows(s: &mut Session, cql: &str) -> Vec<Vec<String>> {
    fmt_rows(&ok(s, cql).await)
}

/// A Cassandra database with masking enabled (no auth).
async fn enabled() -> H {
    let store = nosql_storage::store::Store::start(
        nosql_storage::test_objects("engine"),
        small_store_config(),
    )
    .await
    .unwrap();
    let mut h = H::on(store, "db", Dialect::Cassandra).await;
    let mut s = (*h.db.settings()).clone();
    s.dynamic_data_masking_enabled = true;
    h.db.set_settings(s).unwrap();
    h.ok(KS).await;
    h
}

#[tokio::test]
async fn disabled_by_default() {
    let mut h = H::new(Dialect::Cassandra).await;
    h.ok(KS).await;
    assert_eq!(
        h.msg("CREATE TABLE ks.t (id int PRIMARY KEY, email text MASKED WITH mask_inner(1, 1))")
            .await,
        DISABLED
    );
    // Checked before the keyspace exists (`validate` runs before `apply`).
    assert_eq!(
        h.msg("CREATE TABLE nope.t (id int PRIMARY KEY, v text MASKED WITH DEFAULT)")
            .await,
        DISABLED
    );
    h.ok("CREATE TABLE ks.t (id int PRIMARY KEY, email text)")
        .await;
    assert_eq!(
        h.msg("ALTER TABLE ks.t ALTER email MASKED WITH DEFAULT")
            .await,
        DISABLED
    );
    assert_eq!(
        h.msg("ALTER TABLE ks.nope ALTER email MASKED WITH DEFAULT")
            .await,
        DISABLED
    );
    // Dropping a mask is always allowed (here: nothing to drop), and `ADD` isn't gated.
    assert_eq!(
        body_kind(&h.ok("ALTER TABLE ks.t ALTER email DROP MASKED").await),
        "Void"
    );
    h.ok("ALTER TABLE ks.t ADD ssn text MASKED WITH DEFAULT")
        .await;
    assert_eq!(
        h.rows("SELECT * FROM system_schema.column_masks WHERE keyspace_name = 'ks'")
            .await,
        table(&[&["ks", "t", "ssn", "[]", "[]", "[]", "system", "mask_default"]])
    );
    // A stored mask isn't applied while the setting is off.
    h.ok("INSERT INTO ks.t (id, ssn) VALUES (1, 'secret')")
        .await;
    assert_eq!(h.rows("SELECT ssn FROM ks.t").await, table(&[&["secret"]]));
}

#[tokio::test]
async fn scylla_rejects_masking_syntax() {
    let mut h = H::new(Dialect::Scylla).await;
    h.ok(KS).await;
    let e = h
        .err("CREATE TABLE ks.t (id int PRIMARY KEY, email text MASKED WITH mask_inner(1, 1))")
        .await;
    assert_eq!(
        (e.code, e.message.as_str()),
        // The reference's `ref.t_mask` statement fails at 1:51 (`captured_schema`).
        (
            ErrorCode::Syntax,
            "line 1:45 no viable alternative at input 'text'"
        )
    );
    h.ok("CREATE TABLE ks.t (id int PRIMARY KEY, email text)")
        .await;
    for cql in [
        "ALTER TABLE ks.t ALTER email MASKED WITH DEFAULT",
        "ALTER TABLE ks.t ALTER email DROP MASKED",
        "ALTER TABLE ks.t ADD ssn text MASKED WITH DEFAULT",
    ] {
        assert_eq!(h.err(cql).await.code, ErrorCode::Syntax, "{cql}");
    }
}

#[tokio::test]
async fn ddl_schema_rows_and_describe() {
    let mut h = enabled().await;
    let create = "CREATE TABLE ks.t_mask (id int PRIMARY KEY, email text MASKED WITH mask_inner(1, 1), ssn text MASKED WITH DEFAULT)";
    h.ok(create).await;
    // `cassandra-catalog-suite.json`: `ks ref column_masks`.
    let masks = "SELECT * FROM system_schema.column_masks WHERE keyspace_name = 'ks'";
    assert_eq!(
        h.columns(masks).await,
        [
            "keyspace_name",
            "table_name",
            "column_name",
            "function_argument_nulls",
            "function_argument_types",
            "function_argument_values",
            "function_keyspace",
            "function_name"
        ]
    );
    assert_eq!(
        h.rows(masks).await,
        table(&[
            &[
                "ks",
                "t_mask",
                "email",
                "[False, False]",
                "['int', 'int']",
                "['1', '1']",
                "system",
                "mask_inner"
            ],
            &[
                "ks",
                "t_mask",
                "ssn",
                "[]",
                "[]",
                "[]",
                "system",
                "mask_default"
            ],
        ])
    );
    let desc = h.rows("DESCRIBE TABLE ks.t_mask").await;
    assert!(
        desc[0][3].starts_with("CREATE TABLE ks.t_mask (\n    id int PRIMARY KEY,\n    email text MASKED WITH system.mask_inner(1, 1),\n    ssn text MASKED WITH system.mask_default()\n) WITH"),
        "{}",
        desc[0][3]
    );
    // ALTER: replace, the same mask again (no change: Void), drop.
    let r = h
        .ok("ALTER TABLE ks.t_mask ALTER email MASKED WITH mask_outer(2, null, '#')")
        .await;
    assert_eq!(body_kind(&r), "SchemaChange");
    assert_eq!(
        h.rows(masks).await[0],
        [
            "ks",
            "t_mask",
            "email",
            "[False, True, False]",
            "['int', 'int', 'text']",
            "['2', '', '#']",
            "system",
            "mask_outer"
        ]
    );
    let desc = h.rows("DESCRIBE TABLE ks.t_mask").await;
    assert!(
        desc[0][3].contains("email text MASKED WITH system.mask_outer(2, null, '#'),"),
        "{}",
        desc[0][3]
    );
    assert_eq!(
        body_kind(
            &h.ok("ALTER TABLE ks.t_mask ALTER email MASKED WITH mask_outer(2, null, '#')")
                .await
        ),
        "Void"
    );
    assert_eq!(
        body_kind(&h.ok("ALTER TABLE ks.t_mask ALTER ssn DROP MASKED").await),
        "SchemaChange"
    );
    assert_eq!(
        body_kind(&h.ok("ALTER TABLE ks.t_mask ALTER ssn DROP MASKED").await),
        "Void"
    );
    assert_eq!(
        body_kind(
            &h.ok("ALTER TABLE ks.t_mask ALTER IF EXISTS nope MASKED WITH DEFAULT")
                .await
        ),
        "Void"
    );
    assert_eq!(h.rows(masks).await.len(), 1);
    // ADD with a mask, masks on key columns, renames carry the mask. Cassandra never deletes
    // the row of a renamed or dropped column, or of a dropped table (`captured_masking`:
    // `ddl/masks_alt`, `masks_after_drop`); writing the column again deletes it.
    h.ok("ALTER TABLE ks.t_mask ADD (a text MASKED WITH mask_replace('x'), b int)")
        .await;
    h.ok("CREATE TABLE ks.k (pk int, ck int MASKED WITH DEFAULT, v int, PRIMARY KEY (pk, ck))")
        .await;
    h.ok("ALTER TABLE ks.k RENAME ck TO ck2").await;
    h.ok("ALTER TABLE ks.t_mask DROP a").await;
    let names = "SELECT table_name, column_name, function_name FROM system_schema.column_masks WHERE keyspace_name = 'ks'";
    assert_eq!(
        h.rows(names).await,
        table(&[
            &["k", "ck", "mask_default"],
            &["k", "ck2", "mask_default"],
            &["t_mask", "a", "mask_replace"],
            &["t_mask", "email", "mask_outer"]
        ])
    );
    h.ok("DROP TABLE ks.k").await;
    assert_eq!(h.rows(masks).await.len(), 4);
    // Leftovers survive a reopen and never mask a column of the same name.
    let store = h.store.clone();
    drop(h);
    let mut h = H::on(store, "db", Dialect::Cassandra).await;
    assert_eq!(h.rows(masks).await.len(), 4);
    let desc = h.rows("DESCRIBE TABLE ks.t_mask").await;
    assert!(desc[0][3].contains("MASKED WITH system.mask_outer(2, null, '#')"));
    h.ok("ALTER TABLE ks.t_mask ADD a text").await;
    h.ok("CREATE TABLE ks.k (pk int, ck int, PRIMARY KEY (pk, ck))")
        .await;
    assert_eq!(
        h.rows(names).await,
        table(&[
            &["k", "ck2", "mask_default"],
            &["t_mask", "email", "mask_outer"]
        ])
    );
    assert!(!h.rows("DESCRIBE TABLE ks.t_mask").await[0][3].contains("a text MASKED"));
    h.ok("ALTER TABLE ks.t_mask DROP a").await;
    assert_eq!(h.rows(names).await.len(), 2);
    // A keyspace drop removes them all.
    h.ok("DROP KEYSPACE ks").await;
    assert_eq!(h.rows(masks).await.len(), 0);
}

#[tokio::test]
async fn ddl_errors() {
    let mut h = enabled().await;
    h.ok("CREATE TABLE ks.t (id int PRIMARY KEY, v int, txt text)")
        .await;
    for (cql, msg) in [
        (
            "ALTER TABLE ks.t ALTER nope MASKED WITH DEFAULT",
            "Column with name 'nope' doesn't exist on table 't'",
        ),
        (
            "ALTER TABLE ks.t ALTER v MASKED WITH nope()",
            "Unable to find masking function for v, no declared function matches the signature nope()",
        ),
        (
            "ALTER TABLE ks.t ALTER v MASKED WITH ks.nope(1)",
            "Unable to find masking function for v, no declared function matches the signature ks.nope(1)",
        ),
        (
            "ALTER TABLE ks.t ALTER v MASKED WITH abs()",
            "Not-masking function abs() cannot be used for masking table columns",
        ),
        (
            "ALTER TABLE ks.t ALTER v MASKED WITH sum()",
            "Aggregate function sum() cannot be used for masking table columns",
        ),
        (
            "ALTER TABLE ks.t ALTER txt MASKED WITH mask_hash()",
            "Masking function mask_hash() return type is blob. This is different to the type of the masked column txt of type text. Masking functions can only be attached to table columns if they return the same data type as the masked column.",
        ),
        (
            "ALTER TABLE ks.t ALTER txt MASKED WITH mask_inner(1)",
            "Invalid number of arguments for function system.mask_inner([text|varchar|ascii], int, int, [text])",
        ),
        (
            "ALTER TABLE ks.t ALTER v MASKED WITH mask_inner(1, 1)",
            "Function system.mask_inner requires an argument of type [text|varchar|ascii], but found argument org.apache.cassandra.db.marshal.Int32Type of type int",
        ),
        (
            "ALTER TABLE ks.t ALTER v MASKED WITH mask_replace('x')",
            "Type error: 'x' cannot be passed as argument 1 of function system.mask_replace of type int",
        ),
        (
            "CREATE TABLE ks.t2 (id int PRIMARY KEY, v int MASKED WITH mask_null(1))",
            "Invalid number of arguments for function system.mask_null(any)",
        ),
    ] {
        assert_eq!(h.msg(cql).await, msg, "{cql}");
    }
}

/// An auth-enabled Cassandra database with masking on, a masked table and a reader role.
async fn with_reader() -> (
    Arc<Database>,
    Arc<nosql_storage::store::Store>,
    Session,
    Session,
) {
    let mut cfg = auth_config("db", Dialect::Cassandra);
    cfg.settings.dynamic_data_masking_enabled = true;
    let (db, store) = open(cfg).await;
    let mut su = login(&db, "cassandra", "cassandra").await;
    ok(&mut su, KS).await;
    ok(&mut su, "CREATE TABLE ks.t (pk int, ck int MASKED WITH DEFAULT, email text MASKED WITH mask_inner(1, 1), n int MASKED WITH mask_replace(-1), plain text, PRIMARY KEY (pk, ck))").await;
    ok(&mut su, "INSERT INTO ks.t (pk, ck, email, n, plain) VALUES (1, 5, 'alice@example.com', 10, 'p') USING TIMESTAMP 1000").await;
    ok(&mut su, "INSERT INTO ks.t (pk, ck, email, n, plain) VALUES (1, 6, 'bob@example.com', 20, 'q') USING TIMESTAMP 1000").await;
    ok(
        &mut su,
        "CREATE ROLE r WITH PASSWORD = 'r' AND LOGIN = true",
    )
    .await;
    ok(&mut su, "GRANT SELECT ON ks.t TO r").await;
    let r = login(&db, "r", "r").await;
    (db, store, su, r)
}

#[tokio::test]
async fn enforcement_per_permission() {
    let (db, _store, mut su, mut r) = with_reader().await;
    let all = "SELECT ck, email, n, plain FROM ks.t WHERE pk = 1";
    assert_eq!(
        rows(&mut su, all).await,
        table(&[
            &["5", "alice@example.com", "10", "p"],
            &["6", "bob@example.com", "20", "q"],
        ])
    );
    assert_eq!(
        rows(&mut r, all).await,
        table(&[
            &["0", "a***************m", "-1", "p"],
            &["0", "b*************m", "-1", "q"],
        ])
    );
    // Functions and aggregates see the masked value; writetime / ttl the real cells.
    assert_eq!(
        rows(&mut r, "SELECT max(n), count(email), writetime(email), blobAsText(textAsBlob(email)) FROM ks.t WHERE pk = 1").await,
        table(&[&["-1", "2", "1000", "a***************m"]])
    );
    assert_eq!(
        rows(
            &mut r,
            "SELECT JSON ck, email FROM ks.t WHERE pk = 1 LIMIT 1"
        )
        .await,
        table(&[&["{\"ck\": 0, \"email\": \"a***************m\"}"]])
    );
    // Ordering uses the real values.
    assert_eq!(
        rows(
            &mut r,
            "SELECT email FROM ks.t WHERE pk = 1 ORDER BY ck DESC"
        )
        .await,
        table(&[&["b*************m"], &["a***************m"]])
    );
    // Restricting a masked column needs UNMASK or SELECT_MASKED.
    let e = err(&mut r, "SELECT * FROM ks.t WHERE pk = 1 AND ck = 5").await;
    assert_eq!(
        (e.code, e.message.as_str()),
        (
            ErrorCode::Unauthorized,
            "User r has no UNMASK nor SELECT_MASKED permission on table ks.t, cannot query masked columns [ck]"
        )
    );
    let e = err(
        &mut r,
        "SELECT * FROM ks.t WHERE pk = 1 AND ck > 0 AND n = 10 AND email = 'x' ALLOW FILTERING",
    )
    .await;
    assert_eq!(
        e.message,
        "User r has no UNMASK nor SELECT_MASKED permission on table ks.t, cannot query masked columns [ck, email, n]"
    );
    assert_eq!(
        rows(
            &mut r,
            "SELECT plain FROM ks.t WHERE pk = 1 AND plain = 'p' ALLOW FILTERING"
        )
        .await,
        table(&[&["p"]])
    );
    ok(&mut su, "GRANT SELECT_MASKED ON ks.t TO r").await;
    assert_eq!(
        rows(&mut r, "SELECT ck, n FROM ks.t WHERE pk = 1 AND ck = 5").await,
        table(&[&["0", "-1"]])
    );
    ok(&mut su, "GRANT UNMASK ON KEYSPACE ks TO r").await;
    assert_eq!(
        rows(&mut r, "SELECT ck, n FROM ks.t WHERE pk = 1 AND ck = 5").await,
        table(&[&["5", "10"]])
    );
    ok(&mut su, "REVOKE UNMASK ON KEYSPACE ks FROM r").await;
    // Turning the setting off shows the data (masks stay defined).
    let mut s = (*db.settings()).clone();
    s.dynamic_data_masking_enabled = false;
    db.set_settings(s).unwrap();
    assert_eq!(
        rows(&mut r, "SELECT ck, n FROM ks.t WHERE pk = 1 AND ck = 5").await,
        table(&[&["5", "10"]])
    );
    // Prepared statements decide per execution.
    let mut s = (*db.settings()).clone();
    s.dynamic_data_masking_enabled = true;
    db.set_settings(s).unwrap();
    let p = r
        .prepare("SELECT n FROM ks.t WHERE pk = ?", None)
        .await
        .unwrap();
    for (s, want) in [(&mut r, ["-1", "-1"]), (&mut su, ["10", "20"])] {
        let got = s
            .execute(&p.id, QueryParams::with_values(vec![int(1)]))
            .await
            .unwrap();
        assert_eq!(fmt_rows(&got), table(&[&[want[0]], &[want[1]]]));
    }
}

#[tokio::test]
async fn allow_all_authorizer_unmasks() {
    let mut h = enabled().await;
    h.ok("CREATE TABLE ks.t (pk int PRIMARY KEY, v text MASKED WITH DEFAULT)")
        .await;
    h.ok("INSERT INTO ks.t (pk, v) VALUES (1, 'x')").await;
    assert_eq!(
        h.rows("SELECT v FROM ks.t WHERE v = 'x' ALLOW FILTERING")
            .await,
        table(&[&["x"]])
    );
}

fn policy(entries: &[(&str, &str, &str, &str)]) -> crate::masking::Policy {
    entries
        .iter()
        .map(|(k, t, c, call)| {
            (
                (k.to_string(), t.to_string(), c.to_string()),
                MaskCall::parse(call).unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn platform_policy_masks_every_role() {
    for d in BOTH {
        let mut cfg = auth_config("db", d);
        cfg.masking = policy(&[
            ("ks", "t", "email", "mask_inner(1, 1)"),
            ("ks", "t", "n", "mask_hash('MD5')"),
            ("ks", "t", "ssn", "DEFAULT"),
            ("ks", "t", "pk", "mask_replace(0)"),
        ]);
        let (db, _store) = open(cfg).await;
        let mut su = login(&db, "cassandra", "cassandra").await;
        ok(&mut su, KS).await;
        ok(
            &mut su,
            "CREATE TABLE ks.t (pk int PRIMARY KEY, email text, n int, ssn text, plain text)",
        )
        .await;
        ok(&mut su, "INSERT INTO ks.t (pk, email, n, ssn, plain) VALUES (1, 'alice@example.com', 7, '123', 'p')").await;
        // `mask_hash` returns a blob: it doesn't fit an int column, which gets mask_default.
        assert_eq!(
            rows(&mut su, "SELECT pk, email, n, ssn, plain FROM ks.t").await,
            table(&[&["0", "a***************m", "0", "****", "p"]]),
            "{d}"
        );
        // Filtering and keys use the real values.
        assert_eq!(
            rows(&mut su, "SELECT email FROM ks.t WHERE pk = 1").await,
            table(&[&["a***************m"]])
        );
        assert_eq!(
            rows(
                &mut su,
                "SELECT plain FROM ks.t WHERE email = 'alice@example.com' ALLOW FILTERING"
            )
            .await,
            table(&[&["p"]])
        );
        // Live changes.
        db.set_masking(policy(&[("ks", "t", "plain", "mask_null")]));
        assert_eq!(
            rows(&mut su, "SELECT email, plain FROM ks.t").await,
            table(&[&["alice@example.com", "null"]])
        );
        db.set_masking(Default::default());
        assert_eq!(
            rows(&mut su, "SELECT plain FROM ks.t").await,
            table(&[&["p"]])
        );
    }
}

#[tokio::test]
async fn platform_policy_wins_over_column_masks() {
    let mut cfg = auth_config("db", Dialect::Cassandra);
    cfg.settings.dynamic_data_masking_enabled = true;
    cfg.masking = policy(&[("ks", "t", "v", "mask_replace('platform')")]);
    let (db, _store) = open(cfg).await;
    let mut su = login(&db, "cassandra", "cassandra").await;
    ok(&mut su, KS).await;
    ok(&mut su, "CREATE TABLE ks.t (pk int PRIMARY KEY, v text MASKED WITH DEFAULT, w text MASKED WITH DEFAULT)").await;
    ok(&mut su, "INSERT INTO ks.t (pk, v, w) VALUES (1, 'a', 'b')").await;
    ok(
        &mut su,
        "CREATE ROLE r WITH PASSWORD = 'r' AND LOGIN = true",
    )
    .await;
    ok(&mut su, "GRANT SELECT ON ks.t TO r").await;
    let mut r = login(&db, "r", "r").await;
    assert_eq!(
        rows(&mut su, "SELECT v, w FROM ks.t").await,
        table(&[&["platform", "b"]])
    );
    assert_eq!(
        rows(&mut r, "SELECT v, w FROM ks.t").await,
        table(&[&["platform", "****"]])
    );
}

#[test]
fn mask_calls() {
    let c = MaskCall::parse("mask_inner(1, 1)").unwrap();
    assert_eq!(
        (c.function.as_str(), c.args.clone()),
        ("mask_inner", vec!["1".to_string(), "1".to_string()])
    );
    assert_eq!(c.to_string(), "mask_inner(1, 1)");
    assert_eq!(MaskCall::parse("DEFAULT").unwrap().function, "mask_default");
    assert_eq!(
        MaskCall::parse("mask_null").unwrap().to_string(),
        "mask_null()"
    );
    assert_eq!(
        MaskCall::parse("system.mask_outer(0, 2, '#')")
            .unwrap()
            .args,
        ["0", "2", "'#'"]
    );
    assert_eq!(
        MaskCall::parse(" mask_hash('SHA-1') ").unwrap().args,
        ["'SHA-1'"]
    );
    for bad in [
        "abs(1)",
        "ks.mask_null()",
        "mask_inner(x, 1)",
        "mask_inner(1",
        "",
        "mask_replace(?)",
    ] {
        assert!(MaskCall::parse(bad).is_err(), "{bad}");
    }
}

/// Reopens database `db` of `store` with `cfg` (a restart).
async fn reopen(store: &Arc<nosql_storage::store::Store>, cfg: crate::DbConfig) -> Arc<Database> {
    let handle = store
        .open(&cfg.id, nosql_storage::db::DbConfig::default())
        .await
        .unwrap();
    Database::open(cfg, handle).await.unwrap()
}

fn masking_config() -> crate::DbConfig {
    let mut cfg = auth_config("db", Dialect::Cassandra);
    cfg.settings.dynamic_data_masking_enabled = true;
    cfg.settings.materialized_views_enabled = true;
    cfg
}

#[test]
fn argument_values_round_trip() {
    use crate::masking::{from_get_string, get_string};
    use nosql_types::{CqlType, UdtDef};
    let d = Dialect::Cassandra;
    let udt = CqlType::Udt(
        Arc::new(UdtDef::new(
            "ks",
            "a",
            vec![
                ("s".into(), CqlType::Text),
                ("n".into(), CqlType::Int),
                ("b".into(), CqlType::Blob),
            ],
        )),
        true,
    );
    let tuple = CqlType::Tuple(vec![CqlType::Int, CqlType::Text, udt.clone()]);
    let list = CqlType::List {
        elem: Arc::new(CqlType::Int),
        frozen: false,
    };
    let cases: Vec<(CqlType, &str)> = vec![
        // `captured_masking` `ddl/masks_coll`: as Cassandra stores them.
        (udt.clone(), "x:0"),
        (list.clone(), "0000000200000004000000010000000400000002"),
        (CqlType::Blob, "cafe"),
        (CqlType::Timestamp, "2020-01-01T00:00:00.000Z"),
        (CqlType::Duration, "1h"),
        (CqlType::Decimal, "1.5"),
        // Escapes and nulls.
        (udt.clone(), "a\\:b\\@c:@:ff"),
        (tuple.clone(), "1:@:x\\:1"),
        (tuple.clone(), "@"),
    ];
    for (ty, text) in cases {
        let b = from_get_string(&ty, text, d).unwrap_or_else(|e| panic!("{text}: {e:?}"));
        assert_eq!(get_string(&ty, &b, d), text);
    }
    // Fewer fields than the type: the value ends there.
    let b = from_get_string(&udt, "q", d).unwrap();
    assert_eq!(&b[..], b"\x00\x00\x00\x01q");
    assert_eq!(get_string(&udt, &b, d), "q");
    assert!(from_get_string(&tuple, "1:2:3:4", d).is_err());
    assert!(from_get_string(&list, "zz", d).is_err());
}

/// Masks with compound arguments, and views' masks, keep working after a restart.
#[tokio::test]
async fn stored_masks_survive_reopen() {
    let (db, store) = open(masking_config()).await;
    let mut su = login(&db, "cassandra", "cassandra").await;
    ok(&mut su, KS).await;
    ok(&mut su, "CREATE TYPE ks.a (s text, n int)").await;
    ok(&mut su, "CREATE TABLE ks.t (pk int, ck int, l list<int> MASKED WITH mask_replace([1, 2]), u frozen<a> MASKED WITH mask_replace({s: 'x:y@', n: null}), tp tuple<int, text> MASKED WITH mask_replace((null, 'q')), b blob MASKED WITH mask_replace(0xcafe), v text MASKED WITH mask_inner(1, 1), PRIMARY KEY (pk, ck))").await;
    ok(&mut su, "CREATE MATERIALIZED VIEW ks.tv AS SELECT pk, ck, v FROM ks.t WHERE pk IS NOT NULL AND ck IS NOT NULL AND v IS NOT NULL PRIMARY KEY (v, pk, ck)").await;
    ok(&mut su, "INSERT INTO ks.t (pk, ck, l, u, tp, b, v) VALUES (1, 1, [9], {s: 'real', n: 3}, (7, 'r'), 0x01, 'secret')").await;
    ok(
        &mut su,
        "CREATE ROLE r WITH PASSWORD = 'r' AND LOGIN = true",
    )
    .await;
    ok(&mut su, "GRANT SELECT ON KEYSPACE ks TO r").await;
    let masks = "SELECT table_name, column_name, function_argument_nulls, function_argument_values FROM system_schema.column_masks WHERE keyspace_name = 'ks'";
    let rows_before = rows(&mut su, masks).await;
    assert_eq!(
        rows_before,
        table(&[
            &["t", "b", "[False]", "['cafe']"],
            &[
                "t",
                "l",
                "[False]",
                "['0000000200000004000000010000000400000002']"
            ],
            &["t", "tp", "[False]", "['@:q']"],
            &["t", "u", "[False]", "['x\\:y\\@:@']"],
            &["t", "v", "[False, False]", "['1', '1']"],
            &["tv", "v", "[False, False]", "['1', '1']"],
        ])
    );
    let read = "SELECT l, u, tp, b, v FROM ks.t";
    let mut r = login(&db, "r", "r").await;
    let masked = rows(&mut r, read).await;
    assert_eq!(
        masked,
        table(&[&[
            "[1, 2]",
            "{s: 'x:y@', n: None}",
            "(None, 'q')",
            "0xcafe",
            "s****t"
        ]])
    );
    let view = rows(&mut r, "SELECT v FROM ks.tv").await;
    assert_eq!(view, table(&[&["s****t"]]));
    drop((su, r));
    drop(db);
    let db = reopen(&store, masking_config()).await;
    let mut su = login(&db, "cassandra", "cassandra").await;
    let mut r = login(&db, "r", "r").await;
    assert_eq!(rows(&mut su, masks).await, rows_before);
    assert_eq!(rows(&mut r, read).await, masked);
    assert_eq!(rows(&mut r, "SELECT v FROM ks.tv").await, view);
    // A prepared read of the view is re-prepared when the base's mask changes it.
    let p = r.prepare("SELECT v FROM ks.tv", None).await.unwrap();
    ok(&mut su, "ALTER TABLE ks.t ALTER v DROP MASKED").await;
    let e = r.execute(&p.id, QueryParams::default()).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Unprepared);
    assert_eq!(
        rows(&mut r, "SELECT v FROM ks.tv").await,
        table(&[&["secret"]])
    );
}

/// The platform policy of a table covers its views, index views and CDC log, and the rows of a
/// failed conditional statement.
#[tokio::test]
async fn platform_policy_covers_derived_tables_and_lwt() {
    for d in BOTH {
        let mut cfg = auth_config("db", d);
        cfg.settings.materialized_views_enabled = true;
        cfg.masking = policy(&[
            ("ks", "t", "email", "mask_inner(1, 1)"),
            ("ks", "t", "tags", "mask_inner(1, 1)"),
        ]);
        let (db, _store) = open(cfg).await;
        let mut su = login(&db, "cassandra", "cassandra").await;
        ok(&mut su, KS).await;
        let cdc = if d.is_scylla() {
            " WITH cdc = {'enabled': true}"
        } else {
            ""
        };
        ok(
            &mut su,
            &format!(
                "CREATE TABLE ks.t (pk int PRIMARY KEY, email text, tags set<text>, n int){cdc}"
            ),
        )
        .await;
        ok(&mut su, "CREATE MATERIALIZED VIEW ks.tv AS SELECT pk, email FROM ks.t WHERE pk IS NOT NULL AND email IS NOT NULL PRIMARY KEY (email, pk)").await;
        ok(
            &mut su,
            "INSERT INTO ks.t (pk, email, tags, n) VALUES (1, 'alice@example.com', {'x'}, 5)",
        )
        .await;
        assert_eq!(
            rows(&mut su, "SELECT email, pk FROM ks.tv").await,
            table(&[&["a***************m", "1"]]),
            "{d}"
        );
        // The policy doesn't change the keys or the filtering.
        assert_eq!(
            rows(
                &mut su,
                "SELECT pk FROM ks.tv WHERE email = 'alice@example.com'"
            )
            .await,
            table(&[&["1"]]),
            "{d}"
        );
        let lwt = fmt_rows(
            &ok(
                &mut su,
                "UPDATE ks.t SET n = 6 WHERE pk = 1 IF email = 'nope' AND n = 5",
            )
            .await,
        );
        assert_eq!(lwt, table(&[&["False", "a***************m", "5"]]), "{d}");
        if d.is_scylla() {
            ok(&mut su, "CREATE INDEX t_email ON ks.t (email)").await;
            assert_eq!(
                rows(&mut su, "SELECT email FROM ks.t_email_index").await,
                table(&[&["a***************m"]])
            );
            ok(&mut su, "UPDATE ks.t SET tags = tags - {'x'} WHERE pk = 1").await;
            let log = rows(
                &mut su,
                "SELECT email, \"cdc$deleted_elements_tags\" FROM ks.t_scylla_cdc_log",
            )
            .await;
            assert_eq!(
                log,
                // `mask_default` masks nulls too.
                table(&[&["a***************m", "{}"], &["null", "{}"]]),
                "{d}"
            );
        }
    }
}
