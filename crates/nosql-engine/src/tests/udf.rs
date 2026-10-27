//! User-defined functions and aggregates (`crate::udf`).
//!
//! - **Disabled** (both images' default): every statement form answers as the references do;
//!   these texts were probed on `nosql-ref-scylla` / `nosql-ref-cassandra` (`udf_` keyspaces) or
//!   come from `docs/nosql/reference/errors.md`.
//! - **Enabled**: ScyllaDB runs Lua; Cassandra stores Java. Every message and conversion is
//!   pinned against the UDF-enabled references by the captured batteries
//!   (`captured_udf_harness.rs`); these tests cover what a capture can't show: events, bodies of
//!   schema changes, prepared statements, persistence, permissions.

use super::captured_auth_harness::{auth_config, plain};
use super::*;
use crate::response::{ChangeTarget, ChangeType, Event, SchemaChange};

async fn open_cfg(cfg: DbConfig) -> H {
    let store = Store::start(nosql_storage::test_objects("udf"), small_store_config())
        .await
        .unwrap();
    let handle = store
        .open(&cfg.id, nosql_storage::db::DbConfig::default())
        .await
        .unwrap();
    let db = Database::open(cfg, handle).await.unwrap();
    let s = db.connect(Default::default());
    H { db, s, store }
}

fn enabled(d: Dialect) -> DbConfig {
    let mut cfg = DbConfig::new("db", d);
    cfg.settings.user_defined_functions_enabled = true;
    cfg
}

async fn udf(d: Dialect) -> H {
    let mut h = open_cfg(enabled(d)).await;
    h.ok(KS).await;
    h
}

/// `(code, message)` of an error.
async fn fail(h: &mut H, cql: &str) -> (u16, String) {
    let e = h.err(cql).await;
    (e.code.code() as u16, e.message)
}

const SCYLLA_OFF: &str = "User defined functions are disabled. Set enable_user_defined_functions and experimental_features:udf to enable them";
const CASSANDRA_OFF: &str = "User-defined functions are disabled in cassandra.yaml - set user_defined_functions_enabled=true to enable";
const NO_KS: &str =
    "No keyspace has been specified. USE a keyspace, or explicitly specify keyspace.tablename";

/// Every statement with UDFs disabled, as probed on both references.
#[tokio::test]
async fn disabled_statements() {
    type Case<'a> = (&'a str, (u16, &'a str), (u16, &'a str));
    let cases: &[Case] = &[
        (
            "CREATE FUNCTION ks.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS 'return x;'",
            (0x2200, SCYLLA_OFF),
            (0x2200, CASSANDRA_OFF),
        ),
        (
            "CREATE FUNCTION ks.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x'",
            (0x2200, SCYLLA_OFF),
            (0x2200, CASSANDRA_OFF),
        ),
        (
            "CREATE FUNCTION ks.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE cobol AS 'x'",
            (0x2200, SCYLLA_OFF),
            (0x2200, CASSANDRA_OFF),
        ),
        (
            "CREATE FUNCTION ks.f(x int) CALLED ON NULL INPUT RETURNS int LANGUAGE wasm AS '(module)'",
            (0x2200, SCYLLA_OFF),
            (0x2200, CASSANDRA_OFF),
        ),
        (
            "CREATE FUNCTION nokeyspace.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x'",
            (0x2200, SCYLLA_OFF),
            (0x2200, CASSANDRA_OFF),
        ),
        (
            "CREATE FUNCTION f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x'",
            (0x2200, NO_KS),
            (0x2200, NO_KS),
        ),
        (
            "CREATE FUNCTION system.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x'",
            (0x2100, "system keyspace is not user-modifiable."),
            (0x2200, "System keyspace 'system' is not user-modifiable"),
        ),
        (
            "CREATE FUNCTION system_schema.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x'",
            (0x2100, "system_schema keyspace is not user-modifiable."),
            (
                0x2200,
                "System keyspace 'system_schema' is not user-modifiable",
            ),
        ),
        (
            "DROP FUNCTION ks.nope",
            (0x2200, SCYLLA_OFF),
            (0x2200, "Function 'ks.nope' doesn't exist"),
        ),
        ("DROP FUNCTION nope", (0x2200, NO_KS), (0x2200, NO_KS)),
        (
            "DROP FUNCTION ks.nope(int)",
            (0x2200, SCYLLA_OFF),
            (0x2200, "Function 'ks.nope(int)' doesn't exist"),
        ),
        (
            "DROP FUNCTION nokeyspace.nope",
            (0x2200, SCYLLA_OFF),
            (0x2200, "Function 'nokeyspace.nope' doesn't exist"),
        ),
        (
            "DROP FUNCTION system.now",
            (0x2200, SCYLLA_OFF),
            (0x2200, "System keyspace 'system' is not user-modifiable"),
        ),
        (
            "DROP AGGREGATE ks.nope",
            (0x2200, SCYLLA_OFF),
            (0x2200, "Aggregate 'ks.nope' doesn't exist"),
        ),
        (
            "DROP AGGREGATE nokeyspace.nope",
            (0x2200, SCYLLA_OFF),
            (0x2200, "Aggregate 'nokeyspace.nope' doesn't exist"),
        ),
        (
            "DROP AGGREGATE ks.nope(int, text)",
            (0x2200, SCYLLA_OFF),
            (0x2200, "Aggregate 'ks.nope(int, text)' doesn't exist"),
        ),
        (
            "DROP AGGREGATE system.max",
            (0x2200, SCYLLA_OFF),
            (0x2200, "System keyspace 'system' is not user-modifiable"),
        ),
        (
            "DROP FUNCTION ks.nope(frozen<list<int>>)",
            (0x2200, SCYLLA_OFF),
            (
                0x2200,
                "Argument 'frozen<list<int>>' cannot be frozen; remove frozen<> modifier from 'frozen<list<int>>'",
            ),
        ),
        (
            "CREATE OR REPLACE AGGREGATE IF NOT EXISTS ks.a(int) SFUNC nope STYPE int",
            (0x2200, SCYLLA_OFF),
            (
                0x2200,
                "Cannot use both 'OR REPLACE' and 'IF NOT EXISTS' directives",
            ),
        ),
        (
            "CREATE AGGREGATE nokeyspace.a(int) SFUNC nope STYPE int",
            (0x2200, SCYLLA_OFF),
            (0x2300, "Keyspace nokeyspace doesn't exist"),
        ),
        (
            "CREATE AGGREGATE a(int) SFUNC nope STYPE int",
            (0x2200, NO_KS),
            (0x2200, NO_KS),
        ),
        (
            "CREATE AGGREGATE ks.a(frozen<list<int>>) SFUNC nope STYPE int",
            (0x2200, SCYLLA_OFF),
            (
                0x2200,
                "Argument 'frozen<list<int>>' cannot be frozen; remove frozen<> modifier from 'frozen<list<int>>'",
            ),
        ),
        (
            "CREATE AGGREGATE ks.a(int) SFUNC nope STYPE frozen<list<int>>",
            (0x2200, SCYLLA_OFF),
            (
                0x2200,
                "State type 'frozen<list<int>>' cannot be frozen; remove frozen<> modifier from 'frozen<list<int>>'",
            ),
        ),
        (
            "CREATE AGGREGATE ks.a(int) SFUNC max STYPE int",
            (0x2200, SCYLLA_OFF),
            (0x2200, "State function ks.max(int, int) doesn't exist"),
        ),
        (
            "CREATE AGGREGATE ks.a(int) SFUNC nope STYPE int FINALFUNC nope2 INITCOND 0",
            (0x2200, SCYLLA_OFF),
            (0x2200, "State function ks.nope(int, int) doesn't exist"),
        ),
        (
            "CREATE AGGREGATE ks.\"a b\"(int) SFUNC nope STYPE int",
            (0x2200, SCYLLA_OFF),
            (0x2200, "State function ks.nope(int, int) doesn't exist"),
        ),
        (
            "CREATE AGGREGATE ks.a(int, list<int>) SFUNC nope STYPE map<int, text>",
            (0x2200, SCYLLA_OFF),
            (
                0x2200,
                "State function ks.nope(map<int, text>, int, list<int>) doesn't exist",
            ),
        ),
        (
            "CREATE AGGREGATE ks.a(nosuchtype) SFUNC nope STYPE int",
            (0x2200, SCYLLA_OFF),
            (0x2200, "Unknown type ks.nosuchtype"),
        ),
        (
            "CREATE AGGREGATE system.a(int) SFUNC nope STYPE int",
            (0x2100, "system keyspace is not user-modifiable."),
            (0x2200, "System keyspace 'system' is not user-modifiable"),
        ),
    ];
    for d in BOTH {
        let mut h = H::new(d).await;
        h.ok(KS).await;
        for (cql, s, c) in cases {
            let want = if d.is_scylla() { s } else { c };
            assert_eq!(
                fail(&mut h, cql).await,
                (want.0, want.1.to_string()),
                "{d}: {cql}"
            );
        }
        // IF EXISTS of a missing object (Cassandra; ScyllaDB is disabled first).
        for cql in [
            "DROP FUNCTION IF EXISTS ks.nope",
            "DROP FUNCTION IF EXISTS nokeyspace.nope",
            "DROP AGGREGATE IF EXISTS ks.nope",
        ] {
            match d {
                Dialect::Scylla => {
                    assert_eq!(
                        fail(&mut h, cql).await,
                        (0x2200, SCYLLA_OFF.into()),
                        "{cql}"
                    )
                }
                Dialect::Cassandra => assert_eq!(body_kind(&h.ok(cql).await), "Void", "{cql}"),
            }
        }
        // Cassandra checks the directives before the setting.
        if !d.is_scylla() {
            assert_eq!(
                fail(&mut h, "CREATE OR REPLACE FUNCTION IF NOT EXISTS ks.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x'").await,
                (0x2200, "Cannot use both 'OR REPLACE' and 'IF NOT EXISTS' directives".into())
            );
        } else {
            // ScyllaDB's parser rejects it.
            assert_eq!(
                fail(&mut h, "CREATE OR REPLACE FUNCTION IF NOT EXISTS ks.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x'").await,
                (0x2000, "line 1:27 no viable alternative at input 'IF'".into())
            );
        }
        // REDUCEFUNC is ScyllaDB's.
        let e = h
            .err("CREATE AGGREGATE ks.a(int) SFUNC nope STYPE int REDUCEFUNC r")
            .await;
        match d {
            Dialect::Scylla => assert_eq!(e.message, SCYLLA_OFF),
            Dialect::Cassandra => assert_eq!(
                e.message,
                "line 1:48 no viable alternative at input 'REDUCEFUNC' (...(int) SFUNC nope STYPE [int] REDUCEFUNC...)"
            ),
        }
        // Nothing was created, and an unknown function call is the core's answer.
        assert!(
            h.rows("SELECT * FROM system_schema.functions WHERE keyspace_name = 'ks'")
                .await
                .is_empty()
        );
    }
}

/// ScyllaDB's Lua functions: creation events and bodies, metadata rows, calls in every position,
/// overloads, replacement and drops (the texts of every error form are in the captured
/// `lua_ddl` battery, `captured_udf.rs`).
#[tokio::test]
async fn scylla_lua_functions() {
    let mut h = udf(Dialect::Scylla).await;
    let mut events = h.db.events();
    h.exec(
        "CREATE TABLE ks.t (pk int, ck int, v int, txt text, PRIMARY KEY (pk, ck));
         INSERT INTO ks.t (pk, ck, v, txt) VALUES (1, 1, 10, 'a');
         INSERT INTO ks.t (pk, ck, v, txt) VALUES (1, 2, 20, 'bb');
         INSERT INTO ks.t (pk, ck) VALUES (1, 3)",
    )
    .await;
    while events.try_recv().is_ok() {}
    let r = h
        .ok("CREATE FUNCTION ks.plus1_lua(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x + 1'")
        .await;
    let created = SchemaChange {
        change: ChangeType::Created,
        target: ChangeTarget::Function,
        keyspace: "ks".into(),
        name: Some("plus1_lua".into()),
        arg_types: Some(vec!["int".into()]),
    };
    assert_eq!(r.body, ResponseBody::SchemaChange(created.clone()));
    assert!(matches!(events.try_recv(), Ok(Event::SchemaChange(c)) if c == created));
    h.ok("CREATE FUNCTION ks.acc_lua(s int, x int) CALLED ON NULL INPUT RETURNS int LANGUAGE lua AS 'return s + x'").await;
    assert_eq!(
        h.rows("SELECT * FROM system_schema.functions WHERE keyspace_name = 'ks'")
            .await,
        table(&[
            &[
                "ks",
                "acc_lua",
                "['int', 'int']",
                "['s', 'x']",
                "return s + x",
                "True",
                "lua",
                "int"
            ],
            &[
                "ks",
                "plus1_lua",
                "['int']",
                "['x']",
                "return x + 1",
                "False",
                "lua",
                "int"
            ],
        ])
    );
    assert_eq!(
        h.columns("SELECT plus1_lua(v) FROM ks.t WHERE pk = 1")
            .await,
        ["ks.plus1_lua(v)"]
    );
    assert_eq!(
        h.rows("SELECT ck, ks.plus1_lua(v) FROM ks.t WHERE pk = 1")
            .await,
        table(&[&["1", "11"], &["2", "21"], &["3", "null"]])
    );
    // A call in WHERE and in INSERT values; unqualified names resolve in the statement's keyspace.
    assert_eq!(
        h.rows("SELECT ck FROM ks.t WHERE pk = 1 AND ck = plus1_lua(1)")
            .await,
        table(&[&["2"]])
    );
    h.ok("INSERT INTO ks.t (pk, ck, v) VALUES (2, 1, ks.plus1_lua(41))")
        .await;
    assert_eq!(
        h.rows("SELECT v FROM ks.t WHERE pk = 2").await,
        table(&[&["42"]])
    );
    // A bound argument.
    let r = h
        .run_with("SELECT plus1_lua(?) FROM ks.t WHERE pk = 2", vec![int(6)])
        .await
        .unwrap();
    assert_eq!(fmt_rows(&r), table(&[&["7"]]));
    // CALLED ON NULL INPUT runs with nil; Lua errors are invalid requests without a position
    // (the bytecode is stripped).
    assert_eq!(
        fail(
            &mut h,
            "SELECT acc_lua(v, 1) FROM ks.t WHERE pk = 1 AND ck = 3"
        )
        .await,
        (
            0x2200,
            "lua execution failed: ?:-1: attempt to perform arithmetic on a nil value".into()
        )
    );
    // Overloads: resolution picks by argument type.
    h.ok("CREATE FUNCTION ks.plus1_lua(x text) RETURNS NULL ON NULL INPUT RETURNS text LANGUAGE lua AS 'return x .. \"1\"'").await;
    assert_eq!(
        h.rows("SELECT plus1_lua(txt), plus1_lua(v) FROM ks.t WHERE pk = 1 AND ck = 2")
            .await,
        table(&[&["bb1", "21"]])
    );
    let e = h
        .err("SELECT plus1_lua(ck, ck) FROM ks.t WHERE pk = 1")
        .await;
    assert!(
        e.message
            .starts_with("Invalid call to function plus1_lua, none of its type signatures match"),
        "{e}"
    );
    // OR REPLACE updates (another directive or return type is fine); IF NOT EXISTS changes
    // nothing.
    let r = h.ok("CREATE OR REPLACE FUNCTION ks.plus1_lua(x int) CALLED ON NULL INPUT RETURNS int LANGUAGE lua AS 'return (x or 0) + 2'").await;
    assert!(matches!(&r.body, ResponseBody::SchemaChange(c) if c.change == ChangeType::Updated));
    assert_eq!(
        h.rows("SELECT ck, plus1_lua(v) FROM ks.t WHERE pk = 1")
            .await,
        table(&[&["1", "12"], &["2", "22"], &["3", "2"]])
    );
    let r = h.ok("CREATE FUNCTION IF NOT EXISTS ks.plus1_lua(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return 0'").await;
    assert_eq!(body_kind(&r), "Void");
    assert_eq!(
        fail(&mut h, "CREATE FUNCTION ks.plus1_lua(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x'").await,
        (
            0x2200,
            "The function 'ks.plus1_lua : (int) -> int' already exists".into()
        )
    );
    assert_eq!(
        fail(&mut h, "CREATE FUNCTION ks.bad(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return ('").await,
        (
            0x2200,
            "could not compile: [string \"<internal>\"]:2: unexpected symbol near <eof>".into()
        )
    );
    // A tuple argument is implicitly frozen.
    h.ok("CREATE FUNCTION ks.first(p frozen<tuple<int, text>>) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return p[1]'").await;
    assert_eq!(
        h.rows("SELECT first((1, 'x')) FROM ks.t WHERE pk = 2")
            .await,
        table(&[&["1"]])
    );
    // DROP: by signature, ambiguity, missing.
    assert_eq!(
        fail(&mut h, "DROP FUNCTION ks.plus1_lua").await.1,
        "There are multiple functions named ks.plus1_lua"
    );
    assert_eq!(
        fail(&mut h, "DROP FUNCTION ks.plus1_lua(bigint)").await.1,
        "User function ks.plus1_lua(bigint) doesn't exist"
    );
    assert_eq!(
        body_kind(&h.ok("DROP FUNCTION IF EXISTS ks.plus1_lua(bigint)").await),
        "Void"
    );
    let r = h.ok("DROP FUNCTION ks.plus1_lua(text)").await;
    assert_eq!(
        r.body,
        ResponseBody::SchemaChange(SchemaChange {
            change: ChangeType::Dropped,
            target: ChangeTarget::Function,
            keyspace: "ks".into(),
            name: Some("plus1_lua".into()),
            arg_types: Some(vec!["text".into()]),
        })
    );
    h.ok("DROP FUNCTION ks.plus1_lua").await;
    let e = h.err("SELECT plus1_lua(v) FROM ks.t WHERE pk = 1").await;
    assert_eq!(e.message, "Unknown function plus1_lua called");
}

/// User aggregates in ScyllaDB: state / final / reduce functions, INITCOND, nulls (the state
/// function runs on every row), groups, the stored rows and the drop rules.
#[tokio::test]
async fn scylla_lua_aggregates() {
    let mut h = udf(Dialect::Scylla).await;
    h.exec(
        "CREATE TABLE ks.t (pk int, ck int, v int, s text, PRIMARY KEY (pk, ck));
         INSERT INTO ks.t (pk, ck, v, s) VALUES (1, 1, 1, 'a');
         INSERT INTO ks.t (pk, ck, v, s) VALUES (1, 2, 2, 'bbb');
         INSERT INTO ks.t (pk, ck, v) VALUES (1, 3, 3);
         INSERT INTO ks.t (pk, ck) VALUES (2, 1);
         CREATE FUNCTION ks.acc_lua(s int, x int) CALLED ON NULL INPUT RETURNS int LANGUAGE lua AS 'return s + x';
         CREATE FUNCTION ks.add_rn(s int, x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return s + x';
         CREATE FUNCTION ks.merge(a int, b int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return a + b';
         CREATE FUNCTION ks.twice(s int) RETURNS NULL ON NULL INPUT RETURNS text LANGUAGE lua AS 'return \"total \" .. (s * 2)'",
    )
    .await;
    let r = h
        .ok("CREATE AGGREGATE ks.sum1_lua(int) SFUNC acc_lua STYPE int INITCOND 0")
        .await;
    assert_eq!(
        r.body,
        ResponseBody::SchemaChange(SchemaChange {
            change: ChangeType::Created,
            target: ChangeTarget::Aggregate,
            keyspace: "ks".into(),
            name: Some("sum1_lua".into()),
            arg_types: Some(vec!["int".into()]),
        })
    );
    h.ok("CREATE AGGREGATE ks.agg(int) SFUNC add_rn STYPE int REDUCEFUNC merge FINALFUNC twice INITCOND 0").await;
    assert_eq!(
        h.rows("SELECT * FROM system_schema.aggregates WHERE keyspace_name = 'ks'")
            .await,
        table(&[
            &[
                "ks", "agg", "['int']", "twice", "0", "text", "add_rn", "int"
            ],
            &[
                "ks", "sum1_lua", "['int']", "null", "0", "int", "acc_lua", "int"
            ],
        ])
    );
    assert_eq!(
        h.rows("SELECT * FROM system_schema.scylla_aggregates WHERE keyspace_name = 'ks'")
            .await,
        table(&[&["ks", "agg", "['int']", "merge", "int"]])
    );
    assert_eq!(
        h.columns("SELECT agg(v) FROM ks.t WHERE pk = 1").await,
        ["ks.agg(v)"]
    );
    // With no rows the INITCOND goes to the final function; a null row makes add_rn's state null.
    assert_eq!(
        h.rows("SELECT agg(v) FROM ks.t WHERE pk = 1").await,
        table(&[&["total 12"]])
    );
    assert_eq!(
        h.rows("SELECT agg(v) FROM ks.t WHERE pk = 9").await,
        table(&[&["total 0"]])
    );
    assert_eq!(
        h.rows("SELECT pk, agg(v) FROM ks.t GROUP BY pk").await,
        table(&[&["1", "total 12"], &["2", "null"]])
    );
    // acc_lua is called on the null row: nil arithmetic fails the query.
    assert_eq!(
        fail(&mut h, "SELECT sum1_lua(v) FROM ks.t WHERE pk = 2").await,
        (
            0x2200,
            "lua execution failed: ?:-1: attempt to perform arithmetic on a nil value".into()
        )
    );
    assert_eq!(
        h.rows("SELECT sum1_lua(v) FROM ks.t WHERE pk = 1 AND ck < 3")
            .await,
        table(&[&["3"]])
    );
    // Nested aggregates are rejected as for natives.
    assert_eq!(
        fail(&mut h, "SELECT agg(sum1_lua(v)) FROM ks.t").await.1,
        "SELECT clause contains aggeregation of an aggregation"
    );
    // No INITCOND is needed (the state starts null).
    h.ok("CREATE AGGREGATE ks.nul(int) SFUNC add_rn STYPE int")
        .await;
    assert_eq!(
        h.rows("SELECT nul(v) FROM ks.t WHERE pk = 1").await,
        table(&[&["null"]])
    );
    let cases: &[(&str, &str)] = &[
        (
            "CREATE AGGREGATE ks.a(int) SFUNC acc_lua STYPE int REDUCEFUNC nope",
            "Scalar reduce function nope for state type org.apache.cassandra.db.marshal.Int32Type not found.",
        ),
        (
            "CREATE AGGREGATE ks.a(int) SFUNC acc_lua STYPE int INITCOND 'x'",
            "Invalid STRING constant (x) for \"\" of type int",
        ),
        (
            "CREATE AGGREGATE ks.sum1_lua(int) SFUNC acc_lua STYPE int INITCOND 0",
            "The function 'ks.sum1_lua' already exists",
        ),
        (
            "CREATE AGGREGATE ks.twice(int) SFUNC acc_lua STYPE int INITCOND 0",
            "The function 'ks.twice : (int) -> text' already exists",
        ),
        (
            "CREATE FUNCTION ks.sum1_lua(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x'",
            "The function 'ks.sum1_lua' already exists",
        ),
        (
            "CREATE OR REPLACE FUNCTION ks.sum1_lua(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x'",
            "Cannot replace 'ks.sum1_lua' which is not a user defined function",
        ),
        (
            "DROP FUNCTION ks.acc_lua",
            "Cannot delete function ks.acc_lua : (int, int) -> int, as it is used by user-defined aggregate ks.sum1_lua",
        ),
        (
            "DROP FUNCTION ks.twice",
            "Cannot delete function ks.twice : (int) -> text, as it is used by user-defined aggregate ks.agg",
        ),
        (
            "DROP FUNCTION ks.merge",
            "Cannot delete function ks.merge : (int, int) -> int, as it is used by user-defined aggregate ks.agg",
        ),
        ("DROP AGGREGATE ks.nope", "No function named ks.nope found"),
        (
            "DROP AGGREGATE ks.merge",
            "'ks.merge : (int, int) -> int' is not a user defined aggregate",
        ),
        (
            "DROP FUNCTION ks.agg",
            "'ks.agg' is not a user defined function",
        ),
    ];
    for (cql, want) in cases {
        assert_eq!(fail(&mut h, cql).await, (0x2200, want.to_string()), "{cql}");
    }
    // OR REPLACE an aggregate.
    let r = h
        .ok("CREATE OR REPLACE AGGREGATE ks.sum1_lua(int) SFUNC acc_lua STYPE int INITCOND 100")
        .await;
    assert!(matches!(&r.body, ResponseBody::SchemaChange(c) if c.change == ChangeType::Updated));
    assert_eq!(
        h.rows("SELECT sum1_lua(v) FROM ks.t WHERE pk = 1 AND ck < 3")
            .await,
        table(&[&["103"]])
    );
    // Collection INITCOND and state.
    h.ok("CREATE FUNCTION ks.collect(acc list<text>, x text) CALLED ON NULL INPUT RETURNS list<text> LANGUAGE lua AS 'if x ~= nil then acc[#acc + 1] = x end return acc'").await;
    h.ok("CREATE AGGREGATE ks.all_s(text) SFUNC collect STYPE list<text> INITCOND ['start']")
        .await;
    assert_eq!(
        h.rows("SELECT all_s(s) FROM ks.t WHERE pk = 1").await,
        table(&[&["['start', 'a', 'bbb']"]])
    );
    assert_eq!(
        h.rows("SELECT initcond FROM system_schema.aggregates WHERE keyspace_name = 'ks' AND aggregate_name = 'all_s'").await,
        table(&[&["['start']"]])
    );
    // Drop in dependency order.
    h.ok("DROP AGGREGATE ks.agg(int)").await;
    h.ok("DROP FUNCTION ks.twice").await;
    assert!(
        h.rows("SELECT * FROM system_schema.scylla_aggregates WHERE keyspace_name = 'ks'")
            .await
            .is_empty()
    );
}

/// The CQL ↔ Lua type mapping through a table, in cqlsh's text form (the captured `lua_args` /
/// `lua_returns` batteries pin every conversion byte for byte).
#[tokio::test]
async fn scylla_lua_types() {
    let mut h = udf(Dialect::Scylla).await;
    h.exec(
        "CREATE TYPE ks.point (x int, y text);
         CREATE TABLE ks.ty (pk int PRIMARY KEY, a ascii, b blob, bi bigint, bo boolean, d date, de decimal, db double, du duration, f float, i inet, sm smallint, ti tinyint, tm time, ts timestamp, tu timeuuid, u uuid, vi varint, l list<int>, se set<text>, m map<text, int>, tup frozen<tuple<int, text>>, p frozen<point>, ve vector<float, 3>);
         INSERT INTO ks.ty (pk, a, b, bi, bo, d, de, db, du, f, i, sm, ti, tm, ts, tu, u, vi, l, se, m, tup, p, ve) VALUES (1, 'asc', 0x0102, 9000000000, true, '2024-02-29', 1.5, 2.25, 1mo2d3ns, 0.5, '127.0.0.1', 7, 8, '01:02:03', '2024-01-01 00:00:00+0000', 50554d6e-29bb-11e5-b345-feff819cdc9f, 123e4567-e89b-12d3-a456-426614174000, 12345, [3, 1, 2], {'x', 'y'}, {'k': 5}, (1, 'one'), {x: 2, y: 'two'}, [1.0, 2.0, 3.0])",
    )
    .await;
    // Identity functions: every type goes to Lua and back unchanged.
    let types = [
        ("a", "ascii"),
        ("b", "blob"),
        ("bi", "bigint"),
        ("bo", "boolean"),
        ("d", "date"),
        ("de", "decimal"),
        ("db", "double"),
        ("du", "duration"),
        ("f", "float"),
        ("i", "inet"),
        ("sm", "smallint"),
        ("ti", "tinyint"),
        ("tm", "time"),
        ("ts", "timestamp"),
        ("tu", "timeuuid"),
        ("u", "uuid"),
        ("vi", "varint"),
        ("l", "list<int>"),
        ("se", "set<text>"),
        ("m", "map<text, int>"),
        ("tup", "frozen<tuple<int, text>>"),
        ("p", "point"),
        ("ve", "vector<float, 3>"),
    ];
    for (col, ty) in types {
        h.ok(&format!(
            "CREATE FUNCTION ks.id_{col}(v {ty}) RETURNS NULL ON NULL INPUT RETURNS {ty} LANGUAGE lua AS 'return v'"
        ))
        .await;
        assert_eq!(
            h.rows(&format!("SELECT id_{col}({col}) FROM ks.ty WHERE pk = 1"))
                .await,
            h.rows(&format!("SELECT {col} FROM ks.ty WHERE pk = 1"))
                .await,
            "{ty}"
        );
    }
    // What Lua sees: integers for every integer-like type, nanoseconds for `time`.
    let seen: &[(&str, &str, &str)] = &[
        ("bi", "bigint", "number:9000000000"),
        ("d", "date", "number:2147503430"),
        ("tm", "time", "number:3723000000000"),
        ("ts", "timestamp", "number:1704067200000"),
        ("db", "double", "number:2.25"),
        ("u", "uuid", "string:123e4567-e89b-12d3-a456-426614174000"),
        ("i", "inet", "string:127.0.0.1"),
        ("bo", "boolean", "boolean:true"),
        ("vi", "varint", "userdata:Scylla.decimal"),
    ];
    for (col, ty, want) in seen {
        h.ok(&format!(
            "CREATE FUNCTION ks.show_{col}(v {ty}) RETURNS NULL ON NULL INPUT RETURNS text LANGUAGE lua AS 'return type(v) .. \":\" .. (type(v) == \"userdata\" and getmetatable(v).__name or tostring(v))'"
        ))
        .await;
        assert_eq!(
            h.rows(&format!("SELECT show_{col}({col}) FROM ks.ty WHERE pk = 1"))
                .await,
            table(&[&[want]]),
            "{ty}"
        );
    }
    h.ok("CREATE FUNCTION ks.du_parts(v duration) RETURNS NULL ON NULL INPUT RETURNS text LANGUAGE lua AS 'return v.months .. \"/\" .. v.days .. \"/\" .. v.nanoseconds'").await;
    assert_eq!(
        h.rows("SELECT du_parts(du) FROM ks.ty WHERE pk = 1").await,
        table(&[&["1/2/3"]])
    );
    h.ok("CREATE FUNCTION ks.set_has(v set<text>) RETURNS NULL ON NULL INPUT RETURNS boolean LANGUAGE lua AS 'return v.x == true and v.z == nil'").await;
    assert_eq!(
        h.rows("SELECT set_has(se) FROM ks.ty WHERE pk = 1").await,
        table(&[&["True"]])
    );
    h.ok("CREATE FUNCTION ks.udt_y(v point) RETURNS NULL ON NULL INPUT RETURNS text LANGUAGE lua AS 'return v.y .. v.x'").await;
    assert_eq!(
        h.rows("SELECT udt_y(p) FROM ks.ty WHERE pk = 1").await,
        table(&[&["two2"]])
    );
    h.ok("CREATE FUNCTION ks.big(v varint) RETURNS NULL ON NULL INPUT RETURNS varint LANGUAGE lua AS 'return v + \"100000000000000000000\"'").await;
    assert_eq!(
        h.rows("SELECT big(vi) FROM ks.ty WHERE pk = 1").await,
        table(&[&["100000000000000012345"]])
    );
    // Coercions of results.
    let results: &[(&str, &str, &str)] = &[
        ("bigint", "return '42'", "42"),
        ("int", "return 2^32 + 5", "5"),
        ("double", "return '2.5'", "2.5"),
        ("text", "return 12", "12"),
        ("text", "return 2.0", "2"),
        (
            "varint",
            "return '123456789012345678901234567890'",
            "123456789012345678901234567890",
        ),
        ("decimal", "return '0.25'", "0.25"),
        ("date", "return '2020-01-02'", "2020-01-02"),
        (
            "date",
            "return {year = 2021, month = 3, day = 4}",
            "2021-03-04",
        ),
        (
            "date",
            "return {year = 2021, month = 13, day = 1}",
            "2022-01-01",
        ),
        ("date", "return 2147483648", "1970-01-01"),
        ("timestamp", "return 0", "1970-01-01 00:00:00.000000+0000"),
        ("time", "return '00:00:01'", "00:00:01.000000000"),
        (
            "duration",
            "return {months = 1, days = 0, nanoseconds = 5}",
            "1mo5ns",
        ),
        ("duration", "return '3d'", "3d"),
        ("inet", "return '::1'", "::1"),
        ("set<int>", "return {[3] = true, [1] = 1}", "{1, 3}"),
        (
            "map<int, text>",
            "return {[2] = 'b', [1] = 'a'}",
            "{1: 'a', 2: 'b'}",
        ),
        ("list<int>", "return {}", "[]"),
        ("point", "return {x = 1, y = 'p'}", "{x: 1, y: 'p'}"),
        ("vector<float, 2>", "return {1, 2}", "[1, 2]"),
        ("boolean", "return 1", "True"),
        ("boolean", "return false", "False"),
    ];
    for (i, (ty, body, want)) in results.iter().enumerate() {
        h.ok(&format!(
            "CREATE FUNCTION ks.r{i}() CALLED ON NULL INPUT RETURNS {ty} LANGUAGE lua AS '{}'",
            body.replace('\'', "''")
        ))
        .await;
        assert_eq!(
            h.rows(&format!("SELECT r{i}() FROM ks.ty WHERE pk = 1"))
                .await,
            table(&[&[want]]),
            "{ty}: {body}"
        );
    }
    // Results that don't convert fail the query.
    let bad: &[(&str, &str, u16, &str)] = &[
        ("int", "return 1.5", 0x2200, "value is not an integer"),
        ("int", "return 'x'", 0x2200, "value is not a number"),
        ("int", "return true", 0x2200, "unexpected value"),
        ("decimal", "return 0.25", 0x2200, "value is not a decimal"),
        ("text", "return {}", 0x2200, "unexpected value"),
        ("ascii", "return 'é'", 0x2200, "value is not valid ascii"),
        (
            "list<int>",
            "return {1, nil, 3}",
            0x2200,
            "table is not a sequence",
        ),
        (
            "set<int>",
            "return {[1] = false}",
            0x2200,
            "sets are represented with tables with true values",
        ),
        (
            "frozen<tuple<int, int>>",
            "return {1}",
            0x2200,
            "key 2 missing in sequence of size 2",
        ),
        (
            "point",
            "return {x = 1}",
            0x2200,
            "key y missing in udt point",
        ),
        (
            "point",
            "return {x = 1, y = 'a', z = 2}",
            0x2200,
            "invalid UDT field 'z'",
        ),
        (
            "vector<float, 2>",
            "return {1, 2, 3}",
            0x2200,
            "key 3 is not valid for a sequence of size 2",
        ),
        (
            "uuid",
            "return 'nope'",
            0x0000,
            "marshaling error: Cannot parse uuid from 'nope'",
        ),
        (
            "int",
            "return 1, 2",
            0x2200,
            "2 values returned, expected 1",
        ),
    ];
    for (i, (ty, body, code, msg)) in bad.iter().enumerate() {
        h.ok(&format!(
            "CREATE FUNCTION ks.b{i}() CALLED ON NULL INPUT RETURNS {ty} LANGUAGE lua AS '{}'",
            body.replace('\'', "''")
        ))
        .await;
        assert_eq!(
            fail(&mut h, &format!("SELECT b{i}() FROM ks.ty WHERE pk = 1")).await,
            (*code, msg.to_string()),
            "{ty}: {body}"
        );
    }
}

/// Limits: a UDF can't run forever, allocate without bound or reach the file system.
#[tokio::test]
async fn scylla_lua_limits() {
    let mut h = udf(Dialect::Scylla).await;
    h.exec(
        "CREATE TABLE ks.t (pk int PRIMARY KEY, v int);
         INSERT INTO ks.t (pk, v) VALUES (1, 1);
         CREATE FUNCTION ks.spin(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'while true do end';
         CREATE FUNCTION ks.spin_pcall(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'while true do pcall(function() while true do end end) end';
         CREATE FUNCTION ks.hog(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'local t = {} for i = 1, 1e9 do t[i] = i end return 1';
         CREATE FUNCTION ks.files(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return dofile(\"/etc/passwd\")';
         CREATE FUNCTION ks.binary(x int) RETURNS NULL ON NULL INPUT RETURNS text LANGUAGE lua AS 'return tostring(select(2, load(string.dump(function() end))))';
         CREATE FUNCTION ks.globals(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'counter = (counter or 0) + 1 return counter'",
    )
    .await;
    for f in ["spin", "spin_pcall"] {
        let e = h
            .err(&format!("SELECT {f}(v) FROM ks.t WHERE pk = 1"))
            .await;
        assert_eq!(e.code.code(), 0x2200, "{f}: {e}");
        let ms = e
            .message
            .strip_prefix("lua execution timeout: ")
            .and_then(|m| m.strip_suffix("ms elapsed"))
            .and_then(|m| m.parse::<f64>().ok());
        assert!(ms.is_some_and(|ms| ms >= 10.0), "{f}: {e}");
    }
    for (f, msg) in [
        ("hog", "lua execution failed: not enough memory"),
        (
            "files",
            "lua execution failed: cannot open /etc/passwd: No such file or directory",
        ),
    ] {
        assert_eq!(
            fail(&mut h, &format!("SELECT {f}(v) FROM ks.t WHERE pk = 1")).await,
            (0x2200, msg.to_string()),
            "{f}"
        );
    }
    // Binary chunks are refused (unverified bytecode).
    assert_eq!(
        h.rows("SELECT binary(v) FROM ks.t WHERE pk = 1").await,
        table(&[&["attempt to load a binary chunk (mode is 't')"]])
    );
    // Every call runs in a fresh state.
    for _ in 0..2 {
        assert_eq!(
            h.rows("SELECT globals(v) FROM ks.t WHERE pk = 1").await,
            table(&[&["1"]])
        );
    }
}

/// Cassandra stores Java UDFs (and the aggregates over them) with the suite capture's rows; it
/// can't run them.
#[tokio::test]
async fn cassandra_java_functions() {
    let mut h = udf(Dialect::Cassandra).await;
    h.exec(
        "CREATE TABLE ks.t (pk int PRIMARY KEY, v int);
         INSERT INTO ks.t (pk, v) VALUES (1, 1)",
    )
    .await;
    h.ok("CREATE FUNCTION ks.plus1(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS 'return x + 1;'").await;
    h.ok("CREATE FUNCTION ks.acc(s int, x int) CALLED ON NULL INPUT RETURNS int LANGUAGE java AS 'return s + x;'").await;
    h.ok("CREATE AGGREGATE ks.sum1(int) SFUNC acc STYPE int INITCOND 0")
        .await;
    assert_eq!(
        h.rows("SELECT * FROM system_schema.functions WHERE keyspace_name = 'ks'")
            .await,
        table(&[
            &[
                "ks",
                "acc",
                "['int', 'int']",
                "['s', 'x']",
                "return s + x;",
                "True",
                "java",
                "int"
            ],
            &[
                "ks",
                "plus1",
                "['int']",
                "['x']",
                "return x + 1;",
                "False",
                "java",
                "int"
            ],
        ])
    );
    assert_eq!(
        h.rows("SELECT * FROM system_schema.aggregates WHERE keyspace_name = 'ks'")
            .await,
        table(&[&["ks", "sum1", "['int']", "null", "0", "int", "acc", "int"]])
    );
    let lang = "Currently only Java UDFs are available in Cassandra. For more information - CASSANDRA-18252 and CASSANDRA-17281";
    for l in ["lua", "javascript", "wasm"] {
        assert_eq!(
            fail(&mut h, &format!("CREATE FUNCTION ks.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE {l} AS 'x'")).await,
            (0x2200, lang.to_string())
        );
    }
    let e = h.err("SELECT plus1(v) FROM ks.t WHERE pk = 1").await;
    assert_eq!(e.code.code(), 0x1400);
    assert_eq!(
        e.message,
        "java.lang.UnsupportedOperationException: Java UDFs are not supported by roll"
    );
    // RETURNS NULL ON NULL INPUT never calls the body.
    h.ok("INSERT INTO ks.t (pk) VALUES (2)").await;
    assert_eq!(
        h.rows("SELECT plus1(v) FROM ks.t WHERE pk = 2").await,
        table(&[&["null"]])
    );
    assert_eq!(
        h.columns("SELECT plus1(v) FROM ks.t WHERE pk = 2").await,
        ["ks.plus1(v)"]
    );
    // Counters are bigints in function signatures (`udfType`).
    h.ok("CREATE FUNCTION ks.cnt(c counter) RETURNS NULL ON NULL INPUT RETURNS counter LANGUAGE java AS 'return c;'").await;
    assert_eq!(
        h.rows("SELECT argument_types, return_type FROM system_schema.functions WHERE keyspace_name = 'ks' AND function_name = 'cnt'").await,
        table(&[&["['bigint']", "bigint"]])
    );
    let cases: &[(&str, &str)] = &[
        (
            "CREATE AGGREGATE ks.a(int) SFUNC nope STYPE int",
            "State function ks.nope(int, int) doesn't exist",
        ),
        (
            "CREATE AGGREGATE ks.a(int) SFUNC acc STYPE int FINALFUNC nope",
            "Final function ks.nope(int) doesn't exist",
        ),
        (
            "CREATE AGGREGATE ks.a(int) SFUNC sum1 STYPE int",
            "State function ks.sum1(int, int) doesn't exist",
        ),
        (
            "CREATE AGGREGATE ks.a(int) SFUNC acc STYPE bigint",
            "State function ks.acc(bigint, int) doesn't exist",
        ),
        (
            "CREATE AGGREGATE ks.a(int) SFUNC acc STYPE int INITCOND 'x'",
            "Invalid STRING constant (x) for \"(dummy)\" of type int",
        ),
        (
            "CREATE AGGREGATE ks.\"a/b\"(int) SFUNC acc STYPE int",
            "Aggregate name 'a/b' is invalid",
        ),
        (
            "CREATE AGGREGATE ks.sum1(int) SFUNC acc STYPE int INITCOND 0",
            "Aggregate 'sum1' already exists",
        ),
        (
            "DROP FUNCTION ks.acc",
            "Function 'ks.acc' is still referenced by aggregates ks.sum1",
        ),
        (
            "CREATE FUNCTION ks.plus1(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS 'return x;'",
            "Function 'plus1' already exists",
        ),
    ];
    for (cql, want) in cases {
        assert_eq!(fail(&mut h, cql).await, (0x2200, want.to_string()), "{cql}");
    }
    // IF NOT EXISTS on an existing function changes nothing (VOID).
    let r = h.ok("CREATE FUNCTION IF NOT EXISTS ks.plus1(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS 'return 0;'").await;
    assert_eq!(body_kind(&r), "Void");
    // The aggregate runs its (Java) state function: a failure.
    let e = h.err("SELECT sum1(v) FROM ks.t WHERE pk = 1").await;
    assert_eq!(e.code.code(), 0x1400);
    h.ok("DROP AGGREGATE ks.sum1").await;
    h.ok("DROP FUNCTION ks.acc(int, int)").await;
}

/// Functions persist with the schema, DESCRIBE renders them, and DROP TYPE / DROP KEYSPACE see
/// them.
#[tokio::test]
async fn persistence_and_schema_rules() {
    for d in BOTH {
        let store = Store::start(nosql_storage::test_objects("udf"), small_store_config())
            .await
            .unwrap();
        let lang = if d.is_scylla() { "lua" } else { "java" };
        {
            let mut cfg = enabled(d);
            cfg.id = "p".into();
            let handle = store
                .open("p", nosql_storage::db::DbConfig::default())
                .await
                .unwrap();
            let db = Database::open(cfg, handle).await.unwrap();
            let mut h = H {
                s: db.connect(Default::default()),
                db,
                store: store.clone(),
            };
            h.ok(KS).await;
            h.exec(&format!(
                "CREATE TYPE ks.pt (x int);
                 CREATE FUNCTION ks.px(p pt) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE {lang} AS 'return p.x';
                 CREATE FUNCTION ks.acc(s int, x int) CALLED ON NULL INPUT RETURNS int LANGUAGE {lang} AS 'return s + x';
                 CREATE FUNCTION ks.half(s int) CALLED ON NULL INPUT RETURNS int LANGUAGE {lang} AS 'return s'"
            ))
            .await;
            let reduce = if d.is_scylla() { " REDUCEFUNC acc" } else { "" };
            h.ok(&format!(
                "CREATE AGGREGATE ks.tot(int) SFUNC acc STYPE int{reduce} FINALFUNC half INITCOND 0"
            ))
            .await;
            let want = match d {
                Dialect::Scylla => {
                    "Cannot drop user type ks.pt as it is still used by function ks.px"
                }
                Dialect::Cassandra => {
                    "Cannot drop user type 'ks.pt' as it is still used by functions ks.px"
                }
            };
            assert_eq!(h.msg("DROP TYPE ks.pt").await, want);
            h.db.flush().await.unwrap();
        }
        let mut cfg = enabled(d);
        cfg.id = "p".into();
        let handle = store
            .open("p", nosql_storage::db::DbConfig::default())
            .await
            .unwrap();
        let db = Database::open(cfg, handle).await.unwrap();
        let ks = db.schema().keyspace("ks").cloned().unwrap();
        assert_eq!(ks.functions.len(), 3, "{d}");
        assert_eq!(ks.aggregates.len(), 1, "{d}");
        assert_eq!(
            ks.aggregates[0].reduce_func.as_deref(),
            d.is_scylla().then_some("acc"),
            "{d}"
        );
        let mut h = H {
            s: db.connect(Default::default()),
            db,
            store: store.clone(),
        };
        let described = h.rows("DESCRIBE FUNCTION ks.px").await;
        assert!(
            described[0]
                .last()
                .unwrap()
                .contains("CREATE FUNCTION ks.px("),
            "{d}: {described:?}"
        );
        if d.is_scylla() {
            h.ok("CREATE TABLE ks.t (pk int PRIMARY KEY, v int, p frozen<pt>)")
                .await;
            h.ok("INSERT INTO ks.t (pk, v, p) VALUES (1, 4, {x: 7})")
                .await;
            assert_eq!(
                h.rows("SELECT px(p), tot(v) FROM ks.t").await,
                table(&[&["7", "4"]])
            );
        }
        h.ok("DROP KEYSPACE ks").await;
        assert!(
            h.rows("SELECT * FROM system_schema.functions")
                .await
                .is_empty()
        );
        assert!(
            h.rows("SELECT * FROM system_schema.aggregates")
                .await
                .is_empty()
        );
    }
}

/// A prepared statement calling a function is evicted when a function of that name changes, so
/// drivers re-prepare against the new definition.
#[tokio::test]
async fn prepared_statements_follow_functions() {
    let mut h = udf(Dialect::Scylla).await;
    h.exec(
        "CREATE TABLE ks.t (pk int PRIMARY KEY, v int);
         INSERT INTO ks.t (pk, v) VALUES (1, 1);
         CREATE FUNCTION ks.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x + 1'",
    )
    .await;
    let p =
        h.s.prepare("SELECT f(v) FROM ks.t WHERE pk = ?", None)
            .await
            .unwrap();
    let other =
        h.s.prepare("SELECT v FROM ks.t WHERE pk = ?", None)
            .await
            .unwrap();
    let r =
        h.s.execute(&p.id, QueryParams::with_values(vec![int(1)]))
            .await
            .unwrap();
    assert_eq!(fmt_rows(&r), table(&[&["2"]]));
    h.ok("CREATE OR REPLACE FUNCTION ks.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x + 10'").await;
    let e =
        h.s.execute(&p.id, QueryParams::with_values(vec![int(1)]))
            .await
            .unwrap_err();
    assert_eq!(e.code, ErrorCode::Unprepared);
    assert!(
        h.s.execute(&other.id, QueryParams::with_values(vec![int(1)]))
            .await
            .is_ok()
    );
    let p =
        h.s.prepare("SELECT f(v) FROM ks.t WHERE pk = ?", None)
            .await
            .unwrap();
    let r =
        h.s.execute(&p.id, QueryParams::with_values(vec![int(1)]))
            .await
            .unwrap();
    assert_eq!(fmt_rows(&r), table(&[&["11"]]));
    // An INSERT using it, then a drop.
    let ins =
        h.s.prepare("INSERT INTO ks.t (pk, v) VALUES (?, f(?))", None)
            .await
            .unwrap();
    h.ok("DROP FUNCTION ks.f").await;
    for id in [&p.id, &ins.id] {
        let e =
            h.s.execute(id, QueryParams::with_values(vec![int(1), int(1)]))
                .await
                .unwrap_err();
        assert_eq!(e.code, ErrorCode::Unprepared);
    }
}

/// EXECUTE is checked on every user function a statement calls (and on an aggregate's state and
/// final functions); creating needs CREATE on the keyspace's functions, replacing ALTER, dropping
/// DROP; the creator gets every permission on what it created.
#[tokio::test]
async fn permissions() {
    for d in BOTH {
        let mut cfg = auth_config("db", d);
        cfg.settings.user_defined_functions_enabled = true;
        let mut h = open_cfg(cfg).await;
        h.s.authenticate(&plain("cassandra", "cassandra"))
            .await
            .unwrap();
        let lang = if d.is_scylla() { "lua" } else { "java" };
        h.exec(&format!(
            "{KS};
             CREATE TABLE ks.t (pk int PRIMARY KEY, v int);
             INSERT INTO ks.t (pk, v) VALUES (1, 1);
             CREATE ROLE a WITH PASSWORD = 'a' AND LOGIN = true;
             GRANT SELECT ON ks.t TO a;
             GRANT MODIFY ON ks.t TO a;
             CREATE FUNCTION ks.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE {lang} AS 'return x';
             CREATE FUNCTION ks.acc(s int, x int) CALLED ON NULL INPUT RETURNS int LANGUAGE {lang} AS 'return s';
             CREATE FUNCTION ks.fin(s int) CALLED ON NULL INPUT RETURNS int LANGUAGE {lang} AS 'return s';
             CREATE AGGREGATE ks.agg(int) SFUNC acc STYPE int FINALFUNC fin INITCOND 0"
        ))
        .await;
        let mut a = h.db.connect(Default::default());
        a.authenticate(&plain("a", "a")).await.unwrap();
        let q = |s: &str| s.to_string();
        let no_exec = |f: &str| {
            format!("User a has no EXECUTE permission on <function ks.{f}> or any of its parents")
        };
        let cases = [
            (q("SELECT f(v) FROM ks.t WHERE pk = 1"), no_exec("f(int)")),
            (q("SELECT v FROM ks.t WHERE pk = f(1)"), no_exec("f(int)")),
            (
                q("INSERT INTO ks.t (pk, v) VALUES (2, f(1))"),
                no_exec("f(int)"),
            ),
            (
                q("UPDATE ks.t SET v = f(1) WHERE pk = 2"),
                no_exec("f(int)"),
            ),
            (q("DELETE FROM ks.t WHERE pk = f(2)"), no_exec("f(int)")),
            (q("SELECT agg(v) FROM ks.t"), no_exec("agg(int)")),
        ];
        for (cql, want) in &cases {
            let e = a.query(cql, QueryParams::default()).await.unwrap_err();
            assert_eq!(
                (e.code.code(), e.message.as_str()),
                (0x2100, want.as_str()),
                "{d}: {cql}"
            );
        }
        h.ok("GRANT EXECUTE ON FUNCTION ks.f(int) TO a").await;
        h.ok("GRANT EXECUTE ON FUNCTION ks.agg(int) TO a").await;
        let e = a
            .query("SELECT agg(v) FROM ks.t", QueryParams::default())
            .await
            .unwrap_err();
        assert_eq!(e.message, no_exec("acc(int, int)"), "{d}");
        h.ok("GRANT EXECUTE ON FUNCTION ks.acc(int, int) TO a")
            .await;
        let e = a
            .query("SELECT agg(v) FROM ks.t", QueryParams::default())
            .await
            .unwrap_err();
        assert_eq!(e.message, no_exec("fin(int)"), "{d}");
        h.ok("GRANT EXECUTE ON ALL FUNCTIONS IN KEYSPACE ks TO a")
            .await;
        if d.is_scylla() {
            a.query(
                "SELECT f(v), agg(v) FROM ks.t WHERE pk = 1",
                QueryParams::default(),
            )
            .await
            .unwrap_or_else(|e| panic!("{d}: {e}"));
        }
        // Native functions are always executable.
        assert!(
            a.query(
                "SELECT blobAsInt(intAsBlob(v)) FROM ks.t WHERE pk = 1",
                QueryParams::default()
            )
            .await
            .is_ok()
        );
        // DDL permissions.
        let ddl = [
            (
                format!(
                    "CREATE FUNCTION ks.g(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE {lang} AS 'return x'"
                ),
                "User a has no CREATE permission on <all functions in ks> or any of its parents"
                    .to_string(),
            ),
            (
                format!(
                    "CREATE OR REPLACE FUNCTION ks.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE {lang} AS 'return x'"
                ),
                "User a has no ALTER permission on <function ks.f(int)> or any of its parents"
                    .to_string(),
            ),
            (
                "DROP FUNCTION ks.f".to_string(),
                "User a has no DROP permission on <function ks.f(int)> or any of its parents"
                    .to_string(),
            ),
            (
                "CREATE AGGREGATE ks.agg2(int) SFUNC acc STYPE int INITCOND 0".to_string(),
                "User a has no CREATE permission on <all functions in ks> or any of its parents"
                    .to_string(),
            ),
        ];
        for (cql, want) in &ddl {
            let e = a.query(cql, QueryParams::default()).await.unwrap_err();
            assert_eq!(
                (e.code.code(), e.message.as_str()),
                (0x2100, want.as_str()),
                "{d}: {cql}"
            );
        }
        // The creator of a function gets its permissions in Cassandra, none in ScyllaDB.
        h.ok("GRANT CREATE ON ALL FUNCTIONS IN KEYSPACE ks TO a")
            .await;
        a.query(&format!("CREATE FUNCTION ks.mine(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE {lang} AS 'return x'"), QueryParams::default()).await.unwrap();
        let granted = h
            .rows("LIST ALL PERMISSIONS ON FUNCTION ks.mine(int)")
            .await;
        assert_eq!(
            granted.iter().any(
                |r| r.iter().any(|c| c.contains("ks.mine")) && r.iter().any(|c| c == "EXECUTE")
            ),
            !d.is_scylla(),
            "{d}: {granted:?}"
        );
        let dropped = a
            .query("DROP FUNCTION ks.mine", QueryParams::default())
            .await;
        match d {
            Dialect::Scylla => assert_eq!(
                dropped.unwrap_err().message,
                "User a has no DROP permission on <function ks.mine(int)> or any of its parents"
            ),
            Dialect::Cassandra => {
                dropped.unwrap();
            }
        }
        // Cassandra fails an assertion for a missing state function under an enforcing
        // authorizer (the suite capture's `uda_lua`).
        if !d.is_scylla() {
            assert_eq!(
                fail(&mut h, "CREATE AGGREGATE ks.x(int) SFUNC nope STYPE int").await,
                (0x0000, "java.lang.AssertionError: Unable to find function object for resource <function ks.nope(int, int)>".into())
            );
        }
    }
}

/// ScyllaDB: `CREATE OR REPLACE` of an existing function needs ALTER on it and CREATE on the
/// keyspace's functions (`test_permissions.py::test_grant_revoke_alter_udf_permissions`).
#[tokio::test]
async fn replacing_needs_alter_and_create() {
    let mut cfg = auth_config("db", Dialect::Scylla);
    cfg.settings.user_defined_functions_enabled = true;
    let mut h = open_cfg(cfg).await;
    h.s.authenticate(&plain("cassandra", "cassandra")).await.unwrap();
    h.exec(&format!(
        "{KS};\n\
         CREATE FUNCTION ks.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x';\n\
         CREATE ROLE a WITH PASSWORD = 'a' AND LOGIN = true"
    ))
    .await;
    let mut a = h.db.connect(Default::default());
    a.authenticate(&plain("a", "a")).await.unwrap();
    let replace = "CREATE OR REPLACE FUNCTION ks.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return 1'";
    async fn run(a: &mut Session, cql: &str) -> Option<String> {
        a.query(cql, QueryParams::default()).await.err().map(|e| e.message)
    }
    h.ok("GRANT ALTER ON ALL FUNCTIONS TO a").await;
    assert_eq!(
        run(&mut a, replace).await.as_deref(),
        Some("User a has no CREATE permission on <all functions in ks> or any of its parents")
    );
    h.ok("REVOKE ALTER ON ALL FUNCTIONS FROM a").await;
    h.ok("GRANT CREATE ON ALL FUNCTIONS IN KEYSPACE ks TO a").await;
    assert_eq!(
        run(&mut a, replace).await.as_deref(),
        Some("User a has no ALTER permission on <function ks.f(int)> or any of its parents")
    );
    h.ok("GRANT ALTER ON FUNCTION ks.f(int) TO a").await;
    assert_eq!(run(&mut a, replace).await, None);
}

/// A user function called inside a collection literal argument of another one types like any
/// other term: its result fits the element type (`test_selector_literals.py`).
#[tokio::test]
async fn user_function_inside_a_collection_argument() {
    let mut h = udf(Dialect::Scylla).await;
    h.exec(
        "CREATE FUNCTION ks.inner(m map<text, bigint>) RETURNS NULL ON NULL INPUT RETURNS bigint LANGUAGE lua AS 'return 42';\n\
         CREATE FUNCTION ks.outer(l list<bigint>) RETURNS NULL ON NULL INPUT RETURNS bigint LANGUAGE lua AS 'return l[1]'",
    )
    .await;
    assert_eq!(
        h.rows("SELECT ks.outer([ks.inner({'a': 1}), 2]) AS r FROM system.local").await,
        vec![vec!["42".to_string()]]
    );
}
