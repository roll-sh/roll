//! ScyllaDB service levels in use: `SELECT ... USING SERVICE LEVEL`, the scheduling group limit,
//! and DESCRIBE after the `driver` level was dropped (reference texts; the limit from
//! `docs/features/workload-prioritization.rst`).

use super::*;

#[tokio::test]
async fn select_using_service_level() {
    let mut h = H::new(Dialect::Scylla).await;
    h.exec(&format!(
        "{KS};\nCREATE TABLE ks.t (p int PRIMARY KEY, v int);\nCREATE SERVICE LEVEL sl"
    ))
    .await;
    for cql in [
        "SELECT * FROM ks.t USING SERVICE LEVEL sl",
        "SELECT * FROM ks.t USING SERVICE LEVEL driver",
        "SELECT * FROM ks.t USING SERVICE LEVEL 'driver'",
        "SELECT * FROM ks.t USING SERVICE LEVEL default",
        "SELECT * FROM ks.t USING TIMEOUT 1s AND SERVICE LEVEL driver",
        "SELECT * FROM ks.t ALLOW FILTERING USING SERVICE LEVEL driver",
    ] {
        h.ok(cql).await;
    }
    assert_eq!(
        h.msg("SELECT * FROM ks.t WHERE p = 1 USING SERVICE LEVEL nonexist")
            .await,
        "Service level nonexist doesn't exist"
    );
    // Checked when the statement runs.
    let p =
        h.s.prepare("SELECT * FROM ks.t USING SERVICE LEVEL later", None)
            .await
            .unwrap();
    assert!(h.s.execute(&p.id, QueryParams::default()).await.is_err());
    h.ok("CREATE SERVICE LEVEL later").await;
    h.s.execute(&p.id, QueryParams::default()).await.unwrap();
    assert_eq!(
        code(
            &h.err("INSERT INTO ks.t (p, v) VALUES (1, 1) USING SERVICE LEVEL driver")
                .await
        ),
        ErrorCode::Syntax
    );
}

#[tokio::test]
async fn scheduling_group_limit_and_dropped_driver() {
    let mut h = H::new(Dialect::Scylla).await;
    // `default` and `driver` leave room for eight.
    for i in 0..8 {
        h.ok(&format!("CREATE SERVICE LEVEL sl{i}")).await;
    }
    assert_eq!(
        h.msg("CREATE SERVICE LEVEL sl8").await,
        "Can't create service level - no more scheduling groups exist"
    );
    h.ok("CREATE SERVICE LEVEL IF NOT EXISTS sl0").await;
    h.ok("DROP SERVICE LEVEL sl7").await;
    h.ok("CREATE SERVICE LEVEL sl8").await;
    // A dropped `driver` (created by the system) is described as dropped.
    let desc = |rows: Vec<Vec<String>>| {
        rows.into_iter()
            .map(|r| r[3].clone())
            .filter(|s| s.contains("SERVICE LEVEL"))
            .collect::<Vec<_>>()
    };
    let before = desc(h.rows("DESC SCHEMA WITH INTERNALS").await);
    assert!(before.iter().all(|s| !s.starts_with("DROP")), "{before:?}");
    h.ok("DROP SERVICE LEVEL driver").await;
    let after = desc(h.rows("DESC SCHEMA WITH INTERNALS").await);
    assert_eq!(after[0], "DROP SERVICE LEVEL IF EXISTS driver;");
    assert!(
        after
            .iter()
            .all(|s| !s.contains("LEVEL IF NOT EXISTS driver")),
        "{after:?}"
    );
    // Its group is free again.
    h.ok("CREATE SERVICE LEVEL sl7").await;
}

/// The scheduling group a role's requests run in (`scheduling_group_name` of ScyllaDB's
/// metrics): its effective level's shares decide, `sl:default` without one.
#[tokio::test]
async fn scheduling_groups() {
    let store = Store::start(nosql_storage::test_objects("sl"), small_store_config())
        .await
        .unwrap();
    let cfg = super::captured_auth_harness::auth_config("db", Dialect::Scylla);
    let mut h = H::on_config(store, cfg).await;
    h.s.authenticate(&super::captured_auth_harness::plain("cassandra", "cassandra"))
        .await
        .unwrap();
    h.exec(
        "CREATE ROLE r;\nCREATE ROLE q;\nCREATE ROLE n;\nGRANT q TO r;\nCREATE SERVICE LEVEL big WITH SHARES = 500;\n\
         CREATE SERVICE LEVEL small WITH SHARES = 100;\nATTACH SERVICE LEVEL big TO r;\nATTACH SERVICE LEVEL small TO q",
    )
    .await;
    let group = |role: &'static str| crate::auth::service_levels::scheduling_group(&h.db, role);
    assert_eq!(group("q").await, "sl:small");
    assert_eq!(group("r").await, "sl:small", "the fewest shares win");
    assert_eq!(group("n").await, "sl:default");
}
