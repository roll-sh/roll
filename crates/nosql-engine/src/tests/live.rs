//! What changes while a database is open (`crate::live`): settings applied live, the read-only
//! flag and its guard (`hooks::ReadOnly`), recent warnings; plus the per-request session the
//! server creates (`Database::session`) and the AUTH_SUCCESS token per dialect.

use std::time::{Duration, Instant};

use super::captured_auth_harness::{auth_config, open, plain};
use super::*;
use crate::hooks::Access;
use crate::live::{RECENT_WARNINGS, STATEMENT_CHARS};
use crate::response::{Batch, BatchKind, BatchStatement};
use crate::{AuthStep, ClientInfo, SessionState};

const KS: &str = "CREATE KEYSPACE ks WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}";

async fn login(db: &Arc<Database>, user: &str, pw: &str) -> Session {
    let mut s = db.connect(Default::default());
    s.authenticate(&plain(user, pw))
        .await
        .unwrap_or_else(|e| panic!("login {user}: {e}"));
    s
}

async fn q(s: &mut Session, cql: &str) -> Result<Response, Error> {
    s.query(cql, QueryParams::default()).await
}

async fn ok(s: &mut Session, cql: &str) -> Response {
    q(s, cql)
        .await
        .unwrap_or_else(|e| panic!("{}: {cql}: {e}", s.database().dialect()))
}

/// The error of a statement that must fail: (code, message).
async fn err(s: &mut Session, cql: &str) -> (ErrorCode, String) {
    match q(s, cql).await {
        Ok(r) => panic!("{cql}: expected an error, got {:?}", r.body),
        Err(e) => (e.code, e.message),
    }
}

const BATCH: &str = "BEGIN UNLOGGED BATCH INSERT INTO ks.t (k, v) VALUES (1, 'aaaaaaaaaaaaaaaaaaaa'); INSERT INTO ks.t (k, v) VALUES (2, 'bbbbbbbbbbbbbbbbbbbb'); APPLY BATCH";

#[tokio::test]
async fn thresholds_and_guardrails_apply_live() {
    let mut h = H::new(Dialect::Cassandra).await;
    h.exec(&format!(
        "{KS};\nCREATE TABLE ks.t (k int PRIMARY KEY, v text)"
    ))
    .await;
    assert!(h.ok(BATCH).await.warnings.is_empty());
    let mut s = h.db.settings().as_ref().clone();
    s.batch_size_warn_threshold = 10;
    s.unlogged_batch_across_partitions_warn_threshold = 1;
    s.tombstone_failure_threshold = 7;
    h.db.set_settings(s).unwrap();
    assert_eq!(h.db.settings().batch_size_warn_threshold, 10);
    // `config()` keeps what the database opened with.
    assert_eq!(h.db.config().settings.batch_size_warn_threshold, 5 * 1024);
    let w = h.ok(BATCH).await.warnings;
    assert_eq!(w.len(), 2, "{w:?}");
    assert!(
        w[0].starts_with("Batch for [ks.t] is of size") && w[0].contains("threshold of 10 by"),
        "{w:?}"
    );
    assert!(
        w[1].starts_with("Unlogged batch covering 2 partitions"),
        "{w:?}"
    );
    // The settings table shows the live values.
    let get = |name: &str| format!("SELECT value FROM system_views.settings WHERE name = '{name}'");
    assert_eq!(
        h.rows(&get("batch_size_warn_threshold")).await,
        table(&[&["10B"]])
    );
    assert_eq!(
        h.rows(&get("tombstone_failure_threshold")).await,
        table(&[&["7"]])
    );
    // Settings read at open need a reload: nothing is applied.
    let mut s = h.db.settings().as_ref().clone();
    s.batch_size_warn_threshold = 99;
    s.shards = 4;
    s.authenticator = "PasswordAuthenticator".into();
    assert_eq!(
        h.db.set_settings(s).unwrap_err(),
        ["authenticator", "shards"]
    );
    assert_eq!(h.db.settings().batch_size_warn_threshold, 10);

    // ScyllaDB's restrictions-per-query guardrail.
    let mut h = H::new(Dialect::Scylla).await;
    h.exec(&format!(
        "{KS};\nCREATE TABLE ks.t (k int PRIMARY KEY, v text)"
    ))
    .await;
    h.ok("SELECT * FROM ks.t WHERE k IN (1, 2, 3)").await;
    let mut s = h.db.settings().as_ref().clone();
    s.max_partition_key_restrictions_per_query = 2;
    h.db.set_settings(s).unwrap();
    let e = h.err("SELECT * FROM ks.t WHERE k IN (1, 2, 3)").await;
    assert!(
        e.message.contains("is greater than maximum 2"),
        "{}",
        e.message
    );
    let config = h
        .rows("SELECT value FROM system.config WHERE name = 'max_partition_key_restrictions_per_query'")
        .await;
    assert_eq!(config, table(&[&["2"]]));
}

#[tokio::test]
async fn authorizer_and_auth_caches_apply_live() {
    // Cassandra caches permissions for `permissions_validity`; a new validity drops the cache.
    let mut cfg = auth_config("db", Dialect::Cassandra);
    cfg.settings.permissions_validity_ms = 600_000;
    cfg.settings.roles_validity_ms = 600_000;
    let (db, _store) = open(cfg).await;
    let mut su = login(&db, "cassandra", "cassandra").await;
    ok(&mut su, KS).await;
    ok(&mut su, "CREATE TABLE ks.t (k int PRIMARY KEY)").await;
    ok(
        &mut su,
        "CREATE ROLE a WITH PASSWORD = 'a' AND LOGIN = true",
    )
    .await;
    let mut a = login(&db, "a", "a").await;
    assert_eq!(
        err(&mut a, "SELECT * FROM ks.t").await.0,
        ErrorCode::Unauthorized
    );
    ok(&mut su, "GRANT SELECT ON ks.t TO a").await;
    assert_eq!(
        err(&mut a, "SELECT * FROM ks.t").await.0,
        ErrorCode::Unauthorized,
        "cached"
    );
    let mut s = db.settings().as_ref().clone();
    s.permissions_validity_ms = 0;
    s.roles_validity_ms = 0;
    db.set_settings(s).unwrap();
    ok(&mut a, "SELECT * FROM ks.t").await;
    ok(&mut su, "REVOKE SELECT ON ks.t FROM a").await;
    assert_eq!(
        err(&mut a, "SELECT * FROM ks.t").await.0,
        ErrorCode::Unauthorized
    );

    // Switching the authorizer.
    for d in BOTH {
        let mut cfg = auth_config("db", d);
        cfg.settings.authorizer = "AllowAllAuthorizer".into();
        let (db, _store) = open(cfg).await;
        let mut su = login(&db, "cassandra", "cassandra").await;
        ok(&mut su, KS).await;
        ok(&mut su, "CREATE TABLE ks.t (k int PRIMARY KEY)").await;
        ok(
            &mut su,
            "CREATE ROLE a WITH PASSWORD = 'a' AND LOGIN = true",
        )
        .await;
        let mut a = login(&db, "a", "a").await;
        ok(&mut a, "SELECT * FROM ks.t").await;
        let mut s = db.settings().as_ref().clone();
        s.authorizer = "CassandraAuthorizer".into();
        db.set_settings(s).unwrap();
        assert_eq!(
            err(&mut a, "SELECT * FROM ks.t").await,
            (
                ErrorCode::Unauthorized,
                "User a has no SELECT permission on <table ks.t> or any of its parents".to_string()
            ),
            "{d}"
        );
    }
}

#[tokio::test]
async fn read_only_rejects_every_change_and_allows_reads() {
    for d in BOTH {
        let (db, _store) = open(auth_config("db", d)).await;
        let mut su = login(&db, "cassandra", "cassandra").await;
        ok(&mut su, KS).await;
        ok(&mut su, "CREATE TABLE ks.t (k int PRIMARY KEY, v text)").await;
        ok(&mut su, "INSERT INTO ks.t (k, v) VALUES (1, 'one')").await;
        ok(
            &mut su,
            "CREATE ROLE a WITH PASSWORD = 'a' AND LOGIN = true",
        )
        .await;
        ok(&mut su, "GRANT SELECT ON ks.t TO a").await;
        if d.is_scylla() {
            ok(&mut su, "CREATE SERVICE LEVEL sl").await;
        }
        db.set_read_only(true);
        assert!(db.is_read_only());
        let read_only = (
            ErrorCode::Unauthorized,
            "This database is read-only".to_string(),
        );
        let mut changes = vec![
            "INSERT INTO ks.t (k, v) VALUES (2, 'two')",
            "UPDATE ks.t SET v = 'x' WHERE k = 1",
            "DELETE FROM ks.t WHERE k = 1",
            "INSERT INTO ks.t (k, v) VALUES (3, 'x') IF NOT EXISTS",
            "BEGIN BATCH INSERT INTO ks.t (k, v) VALUES (4, 'x') APPLY BATCH",
            "TRUNCATE ks.t",
            "CREATE TABLE ks.u (k int PRIMARY KEY)",
            "ALTER TABLE ks.t ADD w int",
            "DROP TABLE ks.t",
            "CREATE KEYSPACE ks2 WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}",
            "DROP KEYSPACE ks",
            "CREATE TYPE ks.ty (a int)",
            "CREATE INDEX ON ks.t (v)",
            "CREATE ROLE b WITH PASSWORD = 'b' AND LOGIN = true",
            "ALTER ROLE a WITH PASSWORD = 'z'",
            "DROP ROLE a",
            "GRANT MODIFY ON ks.t TO a",
            "REVOKE SELECT ON ks.t FROM a",
            "GRANT cassandra TO a",
            "CREATE USER u WITH PASSWORD 'u'",
        ];
        if d.is_scylla() {
            changes.extend([
                "CREATE SERVICE LEVEL sl2",
                "ALTER SERVICE LEVEL sl WITH timeout = 10ms",
                "DROP SERVICE LEVEL sl",
                "ATTACH SERVICE LEVEL sl TO a",
                "DETACH SERVICE LEVEL FROM a",
            ]);
        }
        for cql in changes {
            assert_eq!(err(&mut su, cql).await, read_only, "{d}: {cql}");
        }
        // Reads, USE, LIST and DESCRIBE work.
        assert_eq!(
            fmt_rows(&ok(&mut su, "SELECT v FROM ks.t").await),
            table(&[&["one"]])
        );
        ok(&mut su, "USE ks").await;
        ok(&mut su, "LIST ROLES").await;
        ok(&mut su, "LIST USERS").await;
        ok(&mut su, "LIST ALL PERMISSIONS").await;
        ok(&mut su, "DESCRIBE KEYSPACES").await;
        ok(&mut su, "DESCRIBE TABLE ks.t").await;
        if d.is_scylla() {
            ok(&mut su, "LIST ALL SERVICE LEVELS").await;
        }
        let mut a = login(&db, "a", "a").await;
        ok(&mut a, "SELECT v FROM ks.t").await;
        ok(&mut a, "LIST ROLES OF a").await;
        // Permission checks of other subsystems: reading ones pass to the authorizer.
        let authz = db.hooks().authorizer;
        let f = crate::auth::Resource::FunctionsIn("ks".into());
        let check = |p| {
            authz.authorize(
                &db,
                &su.state,
                &Access::Permission {
                    permission: p,
                    resource: f.clone(),
                },
            )
        };
        use nosql_cql::ast::Permission as P;
        for p in [P::Execute, P::Describe, P::Select] {
            check(p).unwrap_or_else(|e| panic!("{d}: {p:?}: {e}"));
        }
        for p in [P::Create, P::Alter, P::Drop, P::Authorize, P::Modify] {
            assert_eq!(check(p).unwrap_err().message, read_only.1, "{d}: {p:?}");
        }
        // The platform's password rotation is internal: it still works.
        assert!(crate::auth::set_password(&db, "a", "new").await.unwrap());
        login(&db, "a", "new").await;
        // Writable again at once.
        db.set_read_only(false);
        ok(&mut su, "INSERT INTO ks.t (k, v) VALUES (2, 'two')").await;
        ok(
            &mut su,
            "CREATE ROLE b WITH PASSWORD = 'b' AND LOGIN = true",
        )
        .await;
    }
}

#[tokio::test]
async fn read_only_survives_set_hooks() {
    let mut h = H::new(Dialect::Scylla).await;
    h.exec(&format!("{KS};\nCREATE TABLE ks.t (k int PRIMARY KEY)"))
        .await;
    h.db.set_hooks(h.db.hooks());
    h.db.set_read_only(true);
    assert_eq!(
        h.err("INSERT INTO ks.t (k) VALUES (1)").await.message,
        "This database is read-only"
    );
    h.ok("SELECT * FROM ks.t").await;
}

#[tokio::test]
async fn recent_warnings_keep_statement_role_and_text() {
    let (db, _store) = open(auth_config("db", Dialect::Cassandra)).await;
    let mut su = login(&db, "cassandra", "cassandra").await;
    ok(&mut su, KS).await;
    ok(&mut su, "CREATE TABLE ks.t (k int PRIMARY KEY, v text)").await;
    assert!(db.recent_warnings().is_empty());
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    // An aggregation without a partition key.
    let r = ok(&mut su, "SELECT count(*) FROM ks.t").await;
    assert_eq!(r.warnings, ["Aggregation query used without partition key"]);
    let w = db.recent_warnings();
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].statement, "SELECT count(*) FROM ks.t");
    assert_eq!(w[0].role.as_deref(), Some("cassandra"));
    assert_eq!(w[0].warning, r.warnings[0]);
    assert!(w[0].at_ms >= started, "{w:?}");
    // Prepared statements name their text; protocol batches their first statement.
    let p = su.prepare("SELECT count(*) FROM ks.t", None).await.unwrap();
    su.execute(&p.id, QueryParams::default()).await.unwrap();
    let mut s = db.settings().as_ref().clone();
    s.batch_size_warn_threshold = 10;
    db.set_settings(s).unwrap();
    let statements = (1..=2)
        .map(|k| BatchStatement::Query {
            text: format!("INSERT INTO ks.t (k, v) VALUES ({k}, 'aaaaaaaaaaaaaaaaaaaa')"),
            values: Vec::new(),
        })
        .collect();
    let r = su
        .batch(Batch {
            kind: BatchKind::Logged,
            statements,
            params: QueryParams::default(),
        })
        .await
        .unwrap();
    assert_eq!(r.warnings.len(), 1);
    let w = db.recent_warnings();
    assert_eq!(w[1].statement, "SELECT count(*) FROM ks.t");
    assert_eq!(
        w[2].statement,
        "BATCH of 2: INSERT INTO ks.t (k, v) VALUES (1, 'aaaaaaaaaaaaaaaaaaaa')"
    );
    assert!(w[2].warning.starts_with("Batch for [ks.t]"), "{w:?}");
    assert_eq!(w.len(), 3, "{w:?}");

    // Long statements are cut, and the ring is bounded (oldest dropped).
    let long = format!(
        "SELECT count(*) FROM ks.t WHERE v = '{}' ALLOW FILTERING",
        "x".repeat(400)
    );
    ok(&mut su, &long).await;
    let last = db.recent_warnings().pop().unwrap();
    assert_eq!(last.statement.chars().count(), STATEMENT_CHARS + 1);
    assert!(last.statement.ends_with('…'));
    for _ in 0..RECENT_WARNINGS {
        ok(&mut su, "SELECT count(*) FROM ks.t").await;
    }
    let w = db.recent_warnings();
    assert_eq!(w.len(), RECENT_WARNINGS);
    assert!(w.iter().all(|w| w.statement == "SELECT count(*) FROM ks.t"));
}

#[tokio::test]
async fn auth_success_token_per_dialect() {
    for d in BOTH {
        let want = match d {
            Dialect::Scylla => AuthStep::Success(Some(Bytes::new())),
            Dialect::Cassandra => AuthStep::Success(None),
        };
        let (db, _store) = open(auth_config("db", d)).await;
        let mut s = db.connect(Default::default());
        assert_eq!(
            s.authenticate(&plain("cassandra", "cassandra"))
                .await
                .unwrap(),
            want,
            "{d}"
        );
        // AllowAllAuthenticator answers the same way.
        let h = H::new(d).await;
        let mut s = h.db.connect(Default::default());
        assert_eq!(s.authenticate(&plain("x", "y")).await.unwrap(), want, "{d}");
        assert_eq!(s.state.user, None);
    }
}

#[tokio::test]
async fn per_request_sessions_are_cheap() {
    let mut h = H::new(Dialect::Scylla).await;
    h.exec(&format!("{KS};\nCREATE TABLE ks.t (k int PRIMARY KEY)"))
        .await;
    // A connection's state as the server keeps it (a driver's STARTUP options).
    let client = ClientInfo {
        address: Some("10.1.2.3:40000".parse().unwrap()),
        driver_name: Some("ScyllaDB Rust Driver".into()),
        driver_version: Some("1.9.0".into()),
        protocol_version: 4,
        options: [
            ("CQL_VERSION", "4.0.0"),
            ("DRIVER_NAME", "ScyllaDB Rust Driver"),
            ("DRIVER_VERSION", "1.9.0"),
            (
                "SCYLLA_LWT_ADD_METADATA_MARK",
                "LWT_OPTIMIZATION_META_BIT_MASK=2147483648",
            ),
            ("SCYLLA_RATE_LIMIT_ERROR", ""),
            ("TABLETS_ROUTING_V1", ""),
            ("APPLICATION_NAME", "app"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
        client_type: None,
        certificate: None,
    };
    let state = SessionState {
        keyspace: Some("ks".into()),
        user: Some("cassandra".into()),
        client,
    };
    // The session continues the state.
    let mut s = h.db.session(state.clone());
    assert_eq!(s.keyspace(), Some("ks"));
    assert_eq!(s.state.user.as_deref(), Some("cassandra"));
    s.query("SELECT * FROM t", QueryParams::default())
        .await
        .unwrap();
    // Measured: creating one (clone of the connection's state included) against a trivial query.
    const N: u32 = 20_000;
    let t = Instant::now();
    for _ in 0..N {
        std::hint::black_box(h.db.session(state.clone()));
    }
    let per_session = t.elapsed() / N;
    let t = Instant::now();
    for _ in 0..200 {
        s.query("SELECT * FROM t WHERE k = 1", QueryParams::default())
            .await
            .unwrap();
    }
    let per_query = t.elapsed() / 200;
    eprintln!("session: {per_session:?}, point query: {per_query:?}");
    assert!(per_session < Duration::from_micros(50), "{per_session:?}");
    assert!(
        per_session * 5 < per_query,
        "{per_session:?} vs {per_query:?}"
    );
}

/// Cassandra's `tombstone_failure_threshold` (`ReadCommand`'s `TombstoneOverwhelmingException`,
/// answered as the coordinator's `ReadFailureException`): a read that scans more tombstones than
/// the threshold fails with READ_FAILURE, `READ_TOO_MANY_TOMBSTONES` (reason 1) from the node, at
/// the request's consistency. Captured at the default 100000 on cassandra:5.0.9
/// (`captured_cases.rs`, `tombstones_100100`); ScyllaDB has no such limit.
#[tokio::test]
async fn tombstone_failure_threshold() {
    for d in [Dialect::Cassandra, Dialect::Scylla] {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (k int, c int, v int, PRIMARY KEY (k, c))"
        ))
        .await;
        for c in 0..8 {
            h.ok(&format!("DELETE FROM ks.t WHERE k = 1 AND c = {c}")).await;
        }
        h.ok("INSERT INTO ks.t (k, c, v) VALUES (1, 100, 1)").await;
        let mut s = h.db.settings().as_ref().clone();
        s.tombstone_failure_threshold = 7;
        h.db.set_settings(s).unwrap();
        // Up to the threshold (c = 1..7) reads fine.
        assert_eq!(h.rows("SELECT v FROM ks.t WHERE k = 1 AND c > 0").await, table(&[&["1"]]));
        // All 8 row tombstones: a partition read and a scan.
        for cql in ["SELECT v FROM ks.t WHERE k = 1", "SELECT v FROM ks.t"] {
            if d == Dialect::Scylla {
                assert_eq!(h.rows(cql).await, table(&[&["1"]]), "{cql}");
                continue;
            }
            for cl in [crate::Consistency::One, crate::Consistency::Quorum, crate::Consistency::LocalOne] {
                let p = QueryParams { consistency: cl, ..Default::default() };
                let e = h.s.query(cql, p).await.unwrap_err();
                assert_eq!(e.code, ErrorCode::ReadFailure, "{cql}");
                assert_eq!(e.message, "Operation failed - received 0 responses and 1 failures: READ_TOO_MANY_TOMBSTONES from /127.0.0.1:7000");
                assert_eq!(
                    e.detail,
                    crate::ErrorDetail::ReadFailure {
                        consistency: cl,
                        received: 0,
                        block_for: 1,
                        num_failures: 1,
                        reasons: vec![(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 1)],
                        data_present: false,
                    },
                    "{cql}"
                );
            }
        }
        // Paged too, and the default threshold is far away.
        if d == Dialect::Cassandra {
            let p = QueryParams { page_size: Some(2), ..Default::default() };
            assert_eq!(h.s.query("SELECT v FROM ks.t WHERE k = 1", p).await.unwrap_err().code, ErrorCode::ReadFailure);
        }
        let mut s = h.db.settings().as_ref().clone();
        s.tombstone_failure_threshold = 100_000;
        h.db.set_settings(s).unwrap();
        assert_eq!(h.rows("SELECT v FROM ks.t WHERE k = 1").await, table(&[&["1"]]));
    }
}
