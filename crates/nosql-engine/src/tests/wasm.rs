//! ScyllaDB's WASM UDFs through CQL (`crate::udf::wasm`), with the behaviours
//! `test/cqlpy/test_wasm.py` pins on the reference: compilation errors, native and serialized
//! arguments, signature checks at the call, fuel exhaustion, and the instance metric.

use super::*;

async fn scylla() -> H {
    let store = Store::start(nosql_storage::test_objects("wasm"), small_store_config())
        .await
        .unwrap();
    let mut cfg = DbConfig::new("db", Dialect::Scylla);
    cfg.settings.user_defined_functions_enabled = true;
    let mut h = H::on_config(store, cfg).await;
    h.exec(&format!(
        "{KS};\nCREATE TABLE ks.t (p bigint PRIMARY KEY, i int, f float, b boolean, txt text)"
    ))
    .await;
    h
}

const INC_FLOAT: &str = r#"(module
  (func $inc (param f32) (result f32) local.get 0 f32.const 1 f32.add)
  (memory (;0;) 17)
  (global (;0;) i32 (i32.const 1024))
  (export "memory" (memory 0))
  (export "inc" (func $inc))
  (export "_scylla_abi" (global 0))
  (data (i32.const 1024) "\01"))"#;

const LOOP: &str = r#"(module
  (func $spin (param i32) (result i32) (loop (result i32) br 0))
  (memory 1)
  (export "memory" (memory 0))
  (export "spin" (func $spin)))"#;

#[tokio::test]
async fn compile_call_and_fail() {
    let mut h = scylla().await;
    for (cql, want) in [
        (
            "CREATE FUNCTION ks.nope(x float) RETURNS NULL ON NULL INPUT RETURNS float LANGUAGE wasm AS 'not a module'",
            "Compilation failed",
        ),
        (
            "CREATE FUNCTION ks.nope(x float) RETURNS NULL ON NULL INPUT RETURNS float LANGUAGE wasm AS '(module)'",
            "Compilation failed: Function nope was not found in given wasm source code",
        ),
        (
            "CREATE FUNCTION ks.memory(x float) RETURNS NULL ON NULL INPUT RETURNS float LANGUAGE wasm AS '(module (memory 1) (export \"memory\" (memory 0)))'",
            "Compilation failed: Exported symbol memory is not a function",
        ),
    ] {
        let e = h.err(cql).await;
        assert_eq!(e.code, ErrorCode::Invalid, "{cql}");
        assert!(e.message.starts_with(want), "{cql}: {}", e.message);
    }
    h.exec(&format!(
        "CREATE FUNCTION ks.inc(x float) RETURNS NULL ON NULL INPUT RETURNS float LANGUAGE wasm AS '{INC_FLOAT}';\n\
         CREATE FUNCTION ks.spin(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE wasm AS '{LOOP}';\n\
         INSERT INTO ks.t (p, i, f, txt) VALUES (1, 7, 121.00390625, 'oi')"
    ))
    .await;
    assert_eq!(
        h.rows("SELECT ks.inc(f), ks.inc(null) FROM ks.t WHERE p = 1")
            .await,
        vec![vec!["122.00391".to_string(), "null".to_string()]]
    );
    // Signatures are checked when called.
    h.ok(&format!(
        "CREATE FUNCTION ks.inc(x int) RETURNS NULL ON NULL INPUT RETURNS float LANGUAGE wasm AS '{INC_FLOAT}'"
    ))
    .await;
    let e = h.err("SELECT ks.inc(i) FROM ks.t WHERE p = 1").await;
    assert_eq!(
        (e.code, e.message.as_str()),
        (
            ErrorCode::Invalid,
            "wasm UDF inc failed: type mismatch with parameters"
        )
    );
    h.ok(&format!(
        "CREATE FUNCTION ks.inc(x text) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE wasm AS '{INC_FLOAT}'"
    ))
    .await;
    let e = h.err("SELECT ks.inc(txt) FROM ks.t WHERE p = 1").await;
    assert!(e.message.contains("failed"), "{}", e.message);
    h.ok(&format!(
        "CREATE OR REPLACE FUNCTION ks.inc(x float) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE wasm AS '{INC_FLOAT}'"
    ))
    .await;
    let e = h.err("SELECT ks.inc(f) FROM ks.t WHERE p = 1").await;
    assert_eq!(e.message, "wasm UDF inc failed: Expected i32, got f32");
    let e = h.err("SELECT ks.spin(i) FROM ks.t WHERE p = 1").await;
    assert_eq!(
        e.message,
        "wasm UDF spin failed: all fuel consumed by WebAssembly"
    );
}

/// `scylla_user_functions_cache_instace_count_any`: instances of functions that still exist.
#[tokio::test]
async fn instances_follow_the_functions() {
    let mut h = scylla().await;
    h.exec(&format!(
        "CREATE FUNCTION ks.inc_count(x float) RETURNS NULL ON NULL INPUT RETURNS float LANGUAGE wasm AS '{}';\n\
         INSERT INTO ks.t (p, f) VALUES (1, 1)",
        INC_FLOAT.replace("\"inc\"", "\"inc_count\"")
    ))
    .await;
    let count = |h: &H| crate::udf::wasm::instance_count(&h.db.schema());
    let before = count(&h);
    h.ok("SELECT ks.inc_count(f) FROM ks.t WHERE p = 1").await;
    assert_eq!(count(&h), before + 1);
    h.ok("DROP FUNCTION ks.inc_count").await;
    assert_eq!(count(&h), before);
}
