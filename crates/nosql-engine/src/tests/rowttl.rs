//! ScyllaDB's per-row TTL ([`crate::rowttl`]): the DDL texts probed on the reference
//! (ScyllaDB 2026.3.3), DESCRIBE, and the expiration pass (deletions reach views and the CDC
//! log as service operations).

use super::*;

const CREATE: &str = "CREATE TABLE ks.t (p int PRIMARY KEY, v int, e timestamp TTL)";

#[tokio::test]
async fn create_table_ttl_column() {
    let mut h = H::new(Dialect::Scylla).await;
    h.exec(&format!("{KS};\n{CREATE}")).await;
    for (cql, want) in [
        (
            "CREATE TABLE ks.a (p int PRIMARY KEY, v int, e timestamp TTL, f bigint TTL)",
            "Cannot have more than one TTL column in a table. Saw e and f",
        ),
        (
            "CREATE TABLE ks.a (p int PRIMARY KEY, v text TTL)",
            "TTL column 'v' must be of type timestamp, bigint or int, can't be text",
        ),
        (
            "CREATE TABLE ks.a (p int PRIMARY KEY, v frozen<list<int>> TTL)",
            "TTL column 'v' must be of type timestamp, bigint or int, can't be frozen<list<int>>",
        ),
        (
            "CREATE TABLE ks.a (p int PRIMARY KEY, v counter TTL)",
            "TTL column 'v' must be of type timestamp, bigint or int, can't be counter",
        ),
        // Declaration order: the first column's type, then a second TTL column.
        (
            "CREATE TABLE ks.a (p int PRIMARY KEY, v text TTL, w int TTL)",
            "TTL column 'v' must be of type timestamp, bigint or int, can't be text",
        ),
        (
            "CREATE TABLE ks.a (p int PRIMARY KEY, v int TTL, w text TTL)",
            "Cannot have more than one TTL column in a table. Saw v and w",
        ),
        (
            "CREATE TABLE ks.a (p text TTL, v int, PRIMARY KEY (p))",
            "TTL column 'p' must be of type timestamp, bigint or int, can't be text",
        ),
        (
            "CREATE TABLE ks.a (p int, c int TTL, PRIMARY KEY (p, c))",
            "TTL column c cannot be part of the PRIMARY KEY",
        ),
        (
            "CREATE TABLE ks.a (p int TTL, c int, PRIMARY KEY (p, c))",
            "TTL column p cannot be part of the PRIMARY KEY",
        ),
        (
            "CREATE TABLE ks.a (p int, c int, s bigint TTL STATIC, PRIMARY KEY (p, c))",
            "TTL column s cannot be a static column",
        ),
        (
            "CREATE TABLE ks.a (p int, c int, s text TTL STATIC, PRIMARY KEY (p, c))",
            "TTL column 's' must be of type timestamp, bigint or int, can't be text",
        ),
        (
            "CREATE TABLE ks.a (p int, s int TTL STATIC, PRIMARY KEY (p))",
            "Static columns are only useful (and thus allowed) if the table has at least one clustering column",
        ),
    ] {
        assert_eq!(h.msg(cql).await, want, "{cql}");
    }
    let e = h
        .err("CREATE TABLE ks.a (p int PRIMARY KEY TTL, v int)")
        .await;
    assert_eq!(
        (e.code, e.message.as_str()),
        (ErrorCode::Syntax, "line 1:37 : Missing ')'")
    );
    // The tag, in `scylla_tags`.
    assert!(
        h.rows("SELECT extensions FROM system_schema.tables WHERE keyspace_name = 'ks' AND table_name = 't'").await[0][0]
            .contains("'scylla_tags': 0x0100000014000000")
    );
    h.ok(r#"CREATE TABLE ks.q (p int PRIMARY KEY, "Quoted" bigint TTL)"#)
        .await;
    let desc = &h.rows("DESC TABLE ks.q").await[0][3];
    assert!(desc.contains("    \"Quoted\" bigint TTL,\n"), "{desc}");
    assert!(
        desc.contains("    AND scylla_tags = {'system:ttl_attribute': 'Quoted'}\n"),
        "{desc}"
    );
    // Cassandra has no TTL columns.
    let mut c = H::new(Dialect::Cassandra).await;
    c.exec(KS).await;
    assert_eq!(code(&c.err(CREATE).await), ErrorCode::Syntax);
    assert_eq!(
        code(&c.err("ALTER TABLE ks.t TTL NULL").await),
        ErrorCode::Syntax
    );
}

#[tokio::test]
async fn alter_table_ttl() {
    let mut h = H::new(Dialect::Scylla).await;
    h.exec(&format!("{KS};\n{CREATE};\nCREATE TABLE ks.k (p int, c int, s bigint static, v bigint, z text, PRIMARY KEY (p, c))")).await;
    for (cql, want) in [
        (
            "ALTER TABLE ks.t TTL v",
            "Cannot set TTL column, table ks.t already has a TTL column defined: e",
        ),
        (
            "ALTER TABLE ks.t TTL zz",
            "Column 'zz' does not exist in table ks.t",
        ),
        (
            "ALTER TABLE ks.t TTL p",
            "Cannot use a primary key column p as a TTL column",
        ),
        (
            "ALTER TABLE ks.k TTL c",
            "Cannot use a primary key column c as a TTL column",
        ),
        (
            "ALTER TABLE ks.k TTL s",
            "Cannot use a static column s as a TTL column",
        ),
        (
            "ALTER TABLE ks.k TTL z",
            "TTL column z must be of type timestamp, bigint or int, can't be text",
        ),
        (
            "ALTER TABLE ks.k TTL NULL",
            "Cannot unset TTL column, table ks.k does not have a TTL column set",
        ),
        (
            "ALTER TABLE ks.k TTL Zq",
            "Column 'zq' does not exist in table ks.k",
        ),
        (
            r#"ALTER TABLE ks.k TTL "Z""#,
            "Column 'Z' does not exist in table ks.k",
        ),
        ("ALTER TABLE ks.nope TTL v", "unconfigured table nope"),
    ] {
        assert_eq!(h.msg(cql).await, want, "{cql}");
    }
    h.ok("ALTER TABLE ks.t TTL NULL").await;
    let desc = &h.rows("DESC TABLE ks.t").await[0][3];
    assert!(
        desc.contains("    e timestamp,\n") && desc.contains("    AND scylla_tags = {}\n"),
        "{desc}"
    );
    h.ok("ALTER TABLE ks.t TTL v").await;
    let desc = &h.rows("DESC TABLE ks.t").await[0][3];
    assert!(desc.contains("    v int TTL,\n"), "{desc}");
    // Dropping the column turns it off; dropping another column doesn't.
    h.ok("ALTER TABLE ks.t DROP e").await;
    assert!(h.rows("DESC TABLE ks.t").await[0][3].contains("    v int TTL,\n"));
    h.ok("ALTER TABLE ks.t DROP v").await;
    assert!(h.rows("DESC TABLE ks.t").await[0][3].contains("    AND scylla_tags = {}\n"));
    assert_eq!(
        h.msg("ALTER TABLE ks.t TTL NULL").await,
        "Cannot unset TTL column, table ks.t does not have a TTL column set"
    );
    h.ok("ALTER TABLE ks.k TTL v").await;
    assert!(crate::rowttl::column(&h.db.schema().keyspace("ks").unwrap().tables["k"]).is_some());
    // The choice survives a restart.
    let store = h.store.clone();
    drop(h);
    let mut h = H::on(store, "db", Dialect::Scylla).await;
    assert!(h.rows("DESC TABLE ks.k").await[0][3].contains("    v bigint TTL,\n"));
}

#[tokio::test]
async fn expiration() {
    let mut h = H::new(Dialect::Scylla).await;
    h.exec(&format!(
        "{KS};\nCREATE TABLE ks.t (p int PRIMARY KEY, v int, e timestamp TTL) WITH cdc = {{'enabled': true}};\n\
         CREATE TABLE ks.b (p int, c int, v int, e bigint TTL, PRIMARY KEY (p, c)) WITH cdc = {{'enabled': true}};\n\
         CREATE TABLE ks.i (p int PRIMARY KEY, e int TTL);\n\
         CREATE MATERIALIZED VIEW ks.mv AS SELECT * FROM ks.b WHERE p IS NOT NULL AND c IS NOT NULL AND v IS NOT NULL PRIMARY KEY (v, p, c)"
    ))
    .await;
    let now = crate::rowttl::now_millis();
    let (past, future) = (now - 1000, now + 3_600_000);
    h.exec(&format!(
        "INSERT INTO ks.t (p, v, e) VALUES (1, 1, {past});\nINSERT INTO ks.t (p, v, e) VALUES (2, 2, {future});\n\
         INSERT INTO ks.t (p, v) VALUES (3, 3);\n\
         INSERT INTO ks.b (p, c, v, e) VALUES (1, 1, 10, {});\nINSERT INTO ks.b (p, c, v, e) VALUES (1, 2, 20, {});\n\
         INSERT INTO ks.i (p, e) VALUES (1, {});\nINSERT INTO ks.i (p, e) VALUES (2, 162777600);\nINSERT INTO ks.i (p, e) VALUES (3, -5)",
        past / 1000,
        future / 1000,
        past / 1000
    ))
    .await;
    // Rows read until a pass deletes them.
    assert_eq!(h.rows("SELECT p FROM ks.t").await.len(), 3);
    assert_eq!(h.db.expire_rows(now).await.unwrap(), 3);
    assert_eq!(h.rows("SELECT p FROM ks.t").await, table(&[&["2"], &["3"]]));
    assert_eq!(h.rows("SELECT p, c FROM ks.b").await, table(&[&["1", "2"]]));
    assert_eq!(h.rows("SELECT v FROM ks.mv").await, table(&[&["20"]]));
    // More than five years old: never expires.
    assert_eq!(h.rows("SELECT p FROM ks.i").await, table(&[&["2"], &["3"]]));
    // Service deletions in the CDC log.
    assert_eq!(
        h.rows(r#"SELECT p, "cdc$operation" FROM ks.t_scylla_cdc_log WHERE "cdc$operation" < 0 ALLOW FILTERING"#).await,
        table(&[&["1", "-4"]])
    );
    assert_eq!(
        h.rows(r#"SELECT p, c, "cdc$operation" FROM ks.b_scylla_cdc_log WHERE "cdc$operation" < 0 ALLOW FILTERING"#).await,
        table(&[&["1", "1", "-3"]])
    );
    // Changing the expiration time keeps a row.
    h.ok(&format!("UPDATE ks.t SET e = {past} WHERE p = 2"))
        .await;
    h.ok(&format!("UPDATE ks.t SET e = {future} WHERE p = 2"))
        .await;
    assert_eq!(h.db.expire_rows(now).await.unwrap(), 0);
    let m = h.db.expiration_metrics().counters();
    assert_eq!(m.map(|(_, v)| v), [3, 2, 6, 0]);
    // A user delete is still operation 4.
    h.ok("DELETE FROM ks.t WHERE p = 3").await;
    assert_eq!(
        h.rows(r#"SELECT "cdc$operation" FROM ks.t_scylla_cdc_log WHERE p = 3 AND "cdc$operation" = 4 ALLOW FILTERING"#).await.len(),
        1
    );
}

#[tokio::test]
async fn expiration_loop() {
    // A database opened with a short period expires rows by itself.
    let store = Store::start(nosql_storage::test_objects("engine"), small_store_config())
        .await
        .unwrap();
    let handle = store.open("db", Default::default()).await.unwrap();
    let mut cfg = DbConfig::new("db", Dialect::Scylla);
    cfg.settings.scylla.alternator_ttl_period_in_seconds = 0.05;
    let db = Database::open(cfg, handle).await.unwrap();
    let mut h = H {
        s: db.connect(Default::default()),
        db,
        store,
    };
    h.exec(&format!("{KS};\n{CREATE}")).await;
    h.ok(&format!(
        "INSERT INTO ks.t (p, e) VALUES (1, {})",
        crate::rowttl::now_millis() - 1
    ))
    .await;
    for _ in 0..200 {
        if h.rows("SELECT p FROM ks.t").await.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(h.rows("SELECT p FROM ks.t").await.is_empty());
    assert_eq!(
        h.rows("SELECT value FROM system.config WHERE name = 'alternator_ttl_period_in_seconds'")
            .await,
        table(&[&["0.05"]])
    );
}
