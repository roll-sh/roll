//! ScyllaDB's audit logging of CQL (`crate::audit`): the default categories of the reference
//! image, the live filters set through `system.config`, the rows written to `audit.audit_log`,
//! logins, and the listeners that follow live options (Alternator's).
//!
//! The reference image audits `DCL,DDL,AUTH,ADMIN` with no keyspace (probed on
//! `nosql-ref-scylla`: `LIST ALL SERVICE LEVELS` is logged as ADMIN with an empty keyspace, a
//! DDL statement on a keyspace that isn't audited is not, a refusal for missing permissions is
//! not). The filter semantics come from `docs/operating-scylla/security/auditing.rst`.

use super::captured_auth_harness::{auth_config, plain};
use super::*;

const KS: &str = "CREATE KEYSPACE ks WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}";

const ROWS: &str = "SELECT category, consistency, error, keyspace_name, table_name, operation, username FROM audit.audit_log";

async fn set(h: &mut H, name: &str, value: &str) {
    h.ok(&format!(
        "UPDATE system.config SET value = '{value}' WHERE name = '{name}'"
    ))
    .await;
}

fn row(r: &[&str]) -> Vec<String> {
    r.iter().map(|s| s.to_string()).collect()
}

/// The audit rows written since the last call (`seen`: how many there were).
async fn new_rows(h: &mut H, query: &str, seen: &mut usize) -> Vec<Vec<String>> {
    let all = h.rows(query).await;
    let out = all[*seen..].to_vec();
    *seen = all.len();
    out
}

#[tokio::test]
async fn cql_statements() {
    let mut h = H::new(Dialect::Scylla).await;
    h.ok(KS).await;
    h.ok("CREATE TABLE ks.t (p int PRIMARY KEY, v int)").await;
    // The image's categories: DDL on a keyspace not audited is not logged, ADMIN is.
    h.ok("LIST ALL SERVICE LEVELS").await;
    let mut seen = 0;
    assert_eq!(
        new_rows(&mut h, ROWS, &mut seen).await,
        vec![row(&[
            "ADMIN",
            "ONE",
            "False",
            "",
            "",
            "LIST ALL SERVICE LEVELS",
            "anonymous"
        ])]
    );
    // The live options read back as set.
    for (name, value, shown) in [
        ("audit_categories", "DML,QUERY,DDL", "\"DML,QUERY,DDL\""),
        ("audit_keyspaces", "ks", "\"ks\""),
        ("audit_tables", "", "\"\""),
        ("audit_rules", "[]", "[]"),
    ] {
        set(&mut h, name, value).await;
        let got = h
            .rows(&format!(
                "SELECT source, value FROM system.config WHERE name = '{name}'"
            ))
            .await;
        assert_eq!(got, vec![row(&["cql", shown])], "{name}");
    }
    h.ok("INSERT INTO ks.t (p, v) VALUES (1, 1)").await;
    h.ok("SELECT * FROM ks.t WHERE p = 1").await;
    h.err("INSERT INTO ks.t (p, v) VALUES (1, 'x')").await;
    h.ok("BEGIN BATCH INSERT INTO ks.t (p, v) VALUES (2, 2); DELETE FROM ks.t WHERE p = 3; APPLY BATCH")
        .await;
    h.ok("ALTER TABLE ks.t WITH comment = 'c'").await;
    // A keyspace that isn't audited.
    h.ok("CREATE KEYSPACE other WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}")
        .await;
    h.ok("CREATE TABLE other.t (p int PRIMARY KEY)").await;
    h.ok("INSERT INTO other.t (p) VALUES (1)").await;
    let batch = "BEGIN BATCH INSERT INTO ks.t (p, v) VALUES (2, 2); DELETE FROM ks.t WHERE p = 3; APPLY BATCH";
    assert_eq!(
        new_rows(&mut h, ROWS, &mut seen).await,
        vec![
            row(&[
                "DML",
                "ONE",
                "False",
                "ks",
                "t",
                "INSERT INTO ks.t (p, v) VALUES (1, 1)",
                "anonymous"
            ]),
            row(&[
                "QUERY",
                "ONE",
                "False",
                "ks",
                "t",
                "SELECT * FROM ks.t WHERE p = 1",
                "anonymous"
            ]),
            row(&[
                "DML",
                "ONE",
                "True",
                "ks",
                "t",
                "INSERT INTO ks.t (p, v) VALUES (1, 'x')",
                "anonymous"
            ]),
            row(&["DML", "ONE", "False", "ks", "t", batch, "anonymous"]),
            row(&["DML", "ONE", "False", "ks", "t", batch, "anonymous"]),
            row(&[
                "DDL",
                "ONE",
                "False",
                "ks",
                "t",
                "ALTER TABLE ks.t WITH comment = 'c'",
                "anonymous"
            ]),
        ]
    );
    // `audit_tables` (by table) and `audit_rules` (by pattern) instead of keyspaces.
    set(&mut h, "audit_keyspaces", "").await;
    set(&mut h, "audit_tables", "other.t").await;
    h.ok("INSERT INTO ks.t (p, v) VALUES (5, 5)").await;
    h.ok("INSERT INTO other.t (p) VALUES (5)").await;
    set(&mut h, "audit_tables", "").await;
    set(&mut h, "audit_categories", "").await;
    set(
        &mut h,
        "audit_rules",
        r#"[{"sinks": ["table"], "categories": ["QUERY"], "qualified_table_names": ["k?.*"], "roles": ["*"]}]"#,
    )
    .await;
    h.ok("SELECT * FROM ks.t").await;
    h.ok("SELECT * FROM other.t").await;
    h.ok("INSERT INTO ks.t (p, v) VALUES (6, 6)").await;
    assert_eq!(
        new_rows(&mut h, ROWS, &mut seen).await,
        vec![
            row(&[
                "DML",
                "ONE",
                "False",
                "other",
                "t",
                "INSERT INTO other.t (p) VALUES (5)",
                "anonymous"
            ]),
            row(&[
                "QUERY",
                "ONE",
                "False",
                "ks",
                "t",
                "SELECT * FROM ks.t",
                "anonymous"
            ]),
        ]
    );
    assert_eq!(
        h.rows("SELECT value FROM system.config WHERE name = 'audit_rules'")
            .await,
        vec![row(&[
            r#"[{"sinks":["table"],"categories":["QUERY"],"qualified_table_names":["k?.*"],"roles":["*"]}]"#
        ])]
    );
    let e = h
        .err("UPDATE system.config SET value = '[1]' WHERE name = 'audit_rules'")
        .await;
    assert_eq!(e.message, "cannot parse option value");
}

/// Logins (AUTH), and refusals for missing permissions (not logged).
#[tokio::test]
async fn logins() {
    let store = Store::start(nosql_storage::test_objects("audit"), small_store_config())
        .await
        .unwrap();
    let mut h = H::on_config(store, auth_config("db", Dialect::Scylla)).await;
    h.s.authenticate(&plain("cassandra", "cassandra"))
        .await
        .unwrap();
    h.ok(KS).await;
    h.ok("CREATE ROLE a WITH PASSWORD = 'a' AND LOGIN = true")
        .await;
    const AUTH_ROWS: &str = "SELECT category, consistency, error, keyspace_name, operation, username FROM audit.audit_log";
    let mut seen = 0;
    assert_eq!(
        new_rows(&mut h, AUTH_ROWS, &mut seen).await,
        vec![
            row(&["AUTH", "", "False", "", "LOGIN", "cassandra"]),
            row(&[
                "DCL",
                "ONE",
                "False",
                "",
                "CREATE ROLE a WITH PASSWORD = 'a' AND LOGIN = true",
                "cassandra"
            ]),
        ]
    );
    let mut a = h.db.connect(Default::default());
    a.authenticate(&plain("a", "a")).await.unwrap();
    let mut b = h.db.connect(Default::default());
    b.authenticate(&plain("a", "wrong")).await.unwrap_err();
    // DCL refused for lack of permission: not logged.
    a.query("CREATE ROLE b", QueryParams::default())
        .await
        .unwrap_err();
    assert_eq!(
        new_rows(&mut h, AUTH_ROWS, &mut seen).await,
        vec![
            row(&["AUTH", "", "False", "", "LOGIN", "a"]),
            row(&["AUTH", "", "True", "", "LOGIN", "a"]),
        ]
    );
}

/// Cassandra has no ScyllaDB audit: no keyspace, nothing written, the options don't exist.
#[tokio::test]
async fn cassandra_has_no_audit() {
    let mut h = H::new(Dialect::Cassandra).await;
    h.ok("CREATE KEYSPACE ks WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}")
        .await;
    h.ok("CREATE TABLE ks.t (p int PRIMARY KEY)").await;
    assert!(!h.db.audit_may_log(crate::audit::Category::Dcl));
    let e = h.err("SELECT * FROM audit.audit_log").await;
    assert_eq!(e.code, ErrorCode::Invalid);
}

/// Live options set with CQL reach the registered listeners (Alternator follows its own
/// options this way); options that aren't live still fail.
#[tokio::test]
async fn option_listeners() {
    let mut h = H::new(Dialect::Scylla).await;
    let heard = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = heard.clone();
    h.db.on_option_set(Arc::new(move |n: &str, v: &str| {
        sink.lock().push((n.to_string(), v.to_string()))
    }));
    set(
        &mut h,
        "alternator_max_expression_cache_entries_per_shard",
        "2",
    )
    .await;
    set(&mut h, "max_concurrent_requests_per_shard", "0").await;
    set(&mut h, "alternator_force_read_before_write", "true").await;
    set(&mut h, "alternator_http_response_server_header", "srv").await;
    assert_eq!(
        *heard.lock(),
        vec![
            (
                "alternator_max_expression_cache_entries_per_shard".into(),
                "2".into()
            ),
            ("max_concurrent_requests_per_shard".into(), "0".into()),
            ("alternator_force_read_before_write".into(), "true".into()),
            (
                "alternator_http_response_server_header".into(),
                "\"srv\"".into()
            ),
        ]
    );
    let e = h
        .err("UPDATE system.config SET value = '1' WHERE name = 'alternator_port'")
        .await;
    assert_eq!(e.message, "option is not live-updateable");
}
