//! Auth behaviour beyond the captured cases (`captured_auth.rs`): the default
//! `AllowAllAuthenticator` answers (`docs/nosql/reference/errors.md`), ScyllaDB service-level
//! validation (probed on the reference without auth), bootstrap, persistence, Cassandra's
//! caches and network permissions, direct writes to the auth tables, creator grants and cleanup
//! for functions, prepared statements, and the APIs other subsystems use.

use std::time::Duration;

use super::captured_auth_harness::{auth_config, open, plain};
use super::*;
use crate::auth;
use crate::hooks::Access;
use crate::response::{ChangeTarget, ChangeType, SchemaChange};

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
    q(s, cql).await.unwrap_or_else(|e| panic!("{cql}: {e}"))
}

async fn err(s: &mut Session, cql: &str) -> (u16, String) {
    match q(s, cql).await {
        Ok(r) => panic!("{cql}: expected an error, got {:?}", r.body),
        Err(e) => (e.code.code() as u16, e.message),
    }
}

async fn rows(s: &mut Session, cql: &str) -> Vec<Vec<String>> {
    fmt_rows(&ok(s, cql).await)
}

/// The `auth-off` cases of `errors.md`: role statements with the image's AllowAllAuthenticator.
#[tokio::test]
async fn allow_all_authenticator_cases() {
    const ANON: &str = "You have to be logged in and not anonymous to perform this request";
    const PW: &str = "org.apache.cassandra.auth.CassandraRoleManager doesn't support PASSWORD";
    type Case<'a> = (&'a str, (u16, &'a str), (u16, &'a str));
    let cases: &[Case] = &[
        (
            "GRANT SELECT ON e.t TO nope",
            (0x2100, ANON),
            (0x2100, ANON),
        ),
        (
            "REVOKE SELECT ON e.t FROM nope",
            (0x2100, ANON),
            (0x2100, ANON),
        ),
        (
            "CREATE ROLE r1 WITH PASSWORD = 'x' AND LOGIN = true",
            (0x2100, ANON),
            (0x2200, PW),
        ),
        ("CREATE ROLE r2", (0x2100, ANON), (0x2100, ANON)),
        (
            "CREATE USER u1 WITH PASSWORD 'x'",
            (0x2100, ANON),
            (0x2200, PW),
        ),
        (
            "ALTER ROLE cassandra WITH PASSWORD = 'x'",
            (0x2100, ANON),
            (0x2200, PW),
        ),
        ("DROP ROLE nope", (0x2100, ANON), (0x2100, ANON)),
        ("LIST ROLES", (0x2100, ANON), (0x2100, ANON)),
        (
            "LIST ALL PERMISSIONS",
            (
                0x2100,
                "You are not authorized to view everyone's permissions",
            ),
            (0x2100, ANON),
        ),
        ("LIST USERS", (0x2100, ANON), (0x2100, ANON)),
        ("GRANT r1 TO r2", (0x2100, ANON), (0x2100, ANON)),
    ];
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{};\nCREATE TABLE e.t (pk int PRIMARY KEY)",
            KS.replace(" ks ", " e ")
        ))
        .await;
        for (cql, s, c) in cases {
            let (code, m) = if d.is_scylla() { s } else { c };
            let e = h.err(cql).await;
            assert_eq!(
                (e.code.code() as u16, e.message.as_str()),
                (*code, *m),
                "{d}: {cql}"
            );
        }
        assert!(!h.s.needs_auth(), "{d}");
        // Credentials are ignored: the session stays anonymous.
        h.s.authenticate(&plain("cassandra", "wrong"))
            .await
            .unwrap();
        assert_eq!(h.s.state.user, None);
    }
    // ScyllaDB's service levels need no login.
    let mut h = H::new(Dialect::Scylla).await;
    assert_eq!(
        body_kind(&h.ok("CREATE SERVICE LEVEL sl1 WITH timeout = 10s").await),
        "Void"
    );
    assert_eq!(
        h.columns("LIST ALL SERVICE LEVELS").await,
        [
            "service_level",
            "timeout",
            "workload_type",
            "shares",
            "percentage of all service level shares"
        ]
    );
    assert_eq!(
        h.msg("ATTACH SERVICE LEVEL sl1 TO cassandra").await,
        "Role cassandra doesn't exist."
    );
}

/// Service-level validation, probed on the ScyllaDB reference (no auth).
#[tokio::test]
async fn scylla_service_level_validation() {
    let mut h = H::new(Dialect::Scylla).await;
    let syntax = 0x2000u16;
    let invalid = 0x2200u16;
    let cases: &[(&str, Option<(u16, &str)>)] = &[
        (
            "CREATE SERVICE LEVEL auth_sl1 WITH timeout = 10s AND workload_type = 'interactive'",
            None,
        ),
        (
            "CREATE SERVICE LEVEL auth_sl1",
            Some((invalid, "The service level 'auth_sl1' already exists.")),
        ),
        ("CREATE SERVICE LEVEL IF NOT EXISTS auth_sl1", None),
        (
            "CREATE SERVICE LEVEL auth_sl4 WITH shares = 0",
            Some((syntax, "'SHARES' can only take values of 1-1000 (given 0)")),
        ),
        (
            "CREATE SERVICE LEVEL auth_sl4 WITH shares = 1001",
            Some((
                syntax,
                "'SHARES' can only take values of 1-1000 (given 1001)",
            )),
        ),
        (
            "CREATE SERVICE LEVEL auth_sl4 WITH shares = 'x'",
            Some((syntax, "Invalid integer value x for 'shares'")),
        ),
        (
            "CREATE SERVICE LEVEL auth_sl4 WITH timeout = 5",
            Some((
                syntax,
                "Failed parsing statement: [CREATE SERVICE LEVEL auth_sl4 WITH timeout = 5] reason: marshaling error: Unable to convert '5' to a duration",
            )),
        ),
        (
            "CREATE SERVICE LEVEL auth_sl4 WITH timeout = 'abc'",
            Some((
                syntax,
                "Failed parsing statement: [CREATE SERVICE LEVEL auth_sl4 WITH timeout = 'abc'] reason: marshaling error: Unable to convert 'abc' to a duration",
            )),
        ),
        (
            "CREATE SERVICE LEVEL auth_sl4 WITH timeout = 1500us",
            Some((
                invalid,
                "Timeout values must be expressed in millisecond granularity",
            )),
        ),
        (
            "CREATE SERVICE LEVEL auth_sl4 WITH timeout = 1mo",
            Some((invalid, "Timeout values cannot be expressed in days/months")),
        ),
        (
            "CREATE SERVICE LEVEL auth_sl4 WITH timeout = 1d",
            Some((invalid, "Timeout values cannot be expressed in days/months")),
        ),
        (
            "CREATE SERVICE LEVEL auth_sl4 WITH workload_type = 'nope'",
            Some((invalid, "Invalid workload type: nope")),
        ),
        (
            "CREATE SERVICE LEVEL auth_sl4 WITH workload_type = 'unspecified'",
            Some((invalid, "Invalid workload type: unspecified")),
        ),
        (
            "CREATE SERVICE LEVEL auth_sl4 WITH SHARES = 5 AND TIMEOUT = 2500ms AND WORKLOAD_TYPE = 'BATCH'",
            Some((invalid, "Invalid workload type: BATCH")),
        ),
        (
            "CREATE SERVICE LEVEL auth_sl4 WITH workload_type = 'batch' AND workload_type = 'batch'",
            Some((syntax, "Multiple definition for property 'workload_type'")),
        ),
        (
            "CREATE SERVICE LEVEL auth_sl4 WITH nope = 1",
            Some((syntax, "Unknown property 'nope'")),
        ),
        (
            "CREATE SERVICE LEVEL auth_sl3 WITH timeout = {'a': 'b'}",
            Some((
                syntax,
                "Invalid value for property 'timeout'. It should be a string",
            )),
        ),
        (
            "CREATE SERVICE LEVEL auth_sl3 WITH workload_type = true",
            Some((invalid, "Invalid workload type: true")),
        ),
        (
            "CREATE SERVICE LEVEL auth_sl4 WITH shares = -1",
            Some((syntax, "'SHARES' can only take values of 1-1000 (given -1)")),
        ),
        (
            "CREATE SERVICE LEVEL auth_sl4 WITH timeout = -1s",
            Some((invalid, "Timeout values must be nonnegative")),
        ),
        ("CREATE SERVICE LEVEL auth_sl4 WITH timeout = 0s", None),
        ("ALTER SERVICE LEVEL auth_sl4 WITH timeout = null", None),
        (
            "ALTER SERVICE LEVEL auth_sl4 WITH workload_type = null",
            None,
        ),
        (
            "ALTER SERVICE LEVEL auth_sl4 WITH shares = null",
            Some((syntax, "Invalid integer value null for 'shares'")),
        ),
        (
            "ALTER SERVICE LEVEL nope WITH shares = 5",
            Some((invalid, "The service level 'nope' doesn't exist.")),
        ),
        ("DROP SERVICE LEVEL IF EXISTS nope", None),
        (
            "DROP SERVICE LEVEL nope",
            Some((invalid, "Service Level nope doesn't exists.")),
        ),
        (
            "ATTACH SERVICE LEVEL nope TO nope",
            Some((invalid, "Service Level nope doesn't exists.")),
        ),
        (
            "DETACH SERVICE LEVEL FROM nope",
            Some((invalid, "Role nope doesn't exist.")),
        ),
        (
            "LIST EFFECTIVE SERVICE LEVEL OF nope",
            Some((invalid, "Role nope doesn't exist.")),
        ),
        ("CREATE SERVICE LEVEL \"Auth_SL5\"", None),
        ("CREATE SERVICE LEVEL 'Auth_SL6'", None),
        ("CREATE SERVICE LEVEL Auth_SL7", None),
        (
            "CREATE SERVICE LEVEL auth_sl8 WITH timeout = 1h AND shares = '7'",
            None,
        ),
        ("CREATE SERVICE LEVEL auth_sl9 WITH shares = 1.5", None),
    ];
    for (cql, want) in cases {
        match (h.run(cql).await, want) {
            (Ok(_), None) => {}
            (Err(e), Some((c, m))) => {
                assert_eq!(
                    (e.code.code() as u16, e.message.as_str()),
                    (*c, *m),
                    "{cql}"
                )
            }
            (r, w) => panic!("{cql}: want {w:?}, got {r:?}"),
        }
    }
    assert_eq!(
        h.rows("LIST SERVICE LEVEL auth_sl4").await,
        table(&[&["auth_sl4", "null", "null", "1000"]])
    );
    assert_eq!(
        h.rows("LIST SERVICE LEVEL auth_sl8").await,
        table(&[&["auth_sl8", "1h", "null", "7"]])
    );
    assert_eq!(h.rows("LIST SERVICE LEVEL auth_sl9").await[0][3], "1");
    assert!(h.rows("LIST SERVICE LEVEL nope").await.is_empty());
    let names: Vec<String> = h
        .rows("LIST ALL SERVICE LEVELS")
        .await
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    assert_eq!(
        names,
        [
            "Auth_SL5", "Auth_SL6", "auth_sl1", "auth_sl4", "auth_sl7", "auth_sl8", "auth_sl9",
            "driver"
        ]
    );
    // Percentages over every level's shares (observed: driver 200 + sl1 1000).
    let mut h = H::new(Dialect::Scylla).await;
    h.ok("CREATE SERVICE LEVEL sl1").await;
    let r = h.rows("LIST ALL SERVICE LEVELS").await;
    assert_eq!(r[0][4], "16.67%");
    assert_eq!(r[1][4], "83.33%");
    assert_eq!(
        h.rows("SELECT service_level, shares, workload_type FROM system.service_levels_v2 WHERE service_level = 'driver'").await,
        table(&[&["driver", "200", "batch"]])
    );
}

#[tokio::test]
async fn bootstrap_roles_per_dialect() {
    // Cassandra creates `cassandra` even with AllowAllAuthenticator; ScyllaDB has none.
    let mut c = H::new(Dialect::Cassandra).await;
    assert_eq!(
        c.rows("SELECT role, can_login, is_superuser, member_of FROM system_auth.roles")
            .await,
        table(&[&["cassandra", "True", "True", "null"]])
    );
    assert!(
        c.rows("SELECT writetime(salted_hash) FROM system_auth.roles")
            .await[0][0]
            == "0",
        "written at timestamp 0"
    );
    let mut s = H::new(Dialect::Scylla).await;
    assert!(s.rows("SELECT role FROM system.roles").await.is_empty());
    assert_eq!(
        s.msg("SELECT * FROM system_auth.roles").await,
        "unconfigured table roles"
    );
    // The initial superuser replaces the default.
    for d in BOTH {
        let mut cfg = DbConfig::new("db", d);
        cfg.settings.authenticator = "org.apache.cassandra.auth.PasswordAuthenticator".into();
        cfg.settings.authorizer = "CassandraAuthorizer".into();
        cfg.settings.auth_bcrypt_log2_rounds = 4;
        cfg.superuser = Some(crate::config::Superuser {
            name: "admin".into(),
            password: "s3cret".into(),
        });
        let (db, _store) = open(cfg).await;
        let mut a = login(&db, "admin", "s3cret").await;
        assert!(!a.needs_auth());
        let ks = if d.is_scylla() {
            "system"
        } else {
            "system_auth"
        };
        assert_eq!(
            rows(
                &mut a,
                &format!("SELECT role, is_superuser, can_login FROM {ks}.roles")
            )
            .await,
            table(&[&["admin", "True", "True"]]),
            "{d}"
        );
        let mut anon = db.connect(Default::default());
        assert!(anon.needs_auth());
        let e = anon
            .authenticate(&plain("cassandra", "cassandra"))
            .await
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::BadCredentials, "{d}");
        // The hash format of each reference.
        let h = auth::salted_hash(&db, "admin").await.unwrap().unwrap();
        assert!(
            h.starts_with(if d.is_scylla() { "$6$" } else { "$2a$04$" }),
            "{h}"
        );
        // Password rotation.
        assert!(auth::set_password(&db, "admin", "n3w").await.unwrap());
        assert!(!auth::set_password(&db, "nobody", "x").await.unwrap());
        login(&db, "admin", "n3w").await;
        let e = db
            .connect(Default::default())
            .authenticate(&plain("admin", "s3cret"))
            .await
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::BadCredentials);
    }
}

#[tokio::test]
async fn roles_grants_and_levels_survive_a_reopen() {
    for d in BOTH {
        let cfg = auth_config("db", d);
        let (db, store) = open(cfg.clone()).await;
        let mut su = login(&db, "cassandra", "cassandra").await;
        ok(&mut su, KS).await;
        ok(&mut su, "CREATE TABLE ks.t (pk int PRIMARY KEY)").await;
        ok(&mut su, "CREATE ROLE r1").await;
        ok(
            &mut su,
            "CREATE ROLE u WITH PASSWORD = 'p' AND LOGIN = true",
        )
        .await;
        ok(&mut su, "GRANT r1 TO u").await;
        ok(&mut su, "GRANT SELECT ON ks.t TO r1").await;
        if d.is_scylla() {
            ok(&mut su, "CREATE SERVICE LEVEL sl WITH shares = 10").await;
            ok(&mut su, "ATTACH SERVICE LEVEL sl TO u").await;
            ok(&mut su, "DROP SERVICE LEVEL driver").await;
        }
        let perms = rows(&mut su, "LIST ALL PERMISSIONS OF u").await;
        db.flush().await.unwrap();
        drop((su, db));
        tokio::time::sleep(Duration::from_millis(30)).await;
        let handle = store
            .open("db", nosql_storage::db::DbConfig::default())
            .await
            .unwrap();
        let db = Database::open(cfg, handle).await.unwrap();
        let mut u = login(&db, "u", "p").await;
        ok(&mut u, "SELECT * FROM ks.t").await;
        let mut su = login(&db, "cassandra", "cassandra").await;
        assert_eq!(
            rows(&mut su, "LIST ALL PERMISSIONS OF u").await,
            perms,
            "{d}"
        );
        if d.is_scylla() {
            assert_eq!(
                rows(&mut su, "LIST ATTACHED SERVICE LEVEL OF u").await,
                table(&[&["u", "sl"]])
            );
            // A dropped `driver` stays dropped (its row is written at timestamp 0).
            assert!(rows(&mut su, "LIST SERVICE LEVEL driver").await.is_empty());
        }
    }
}

#[tokio::test]
async fn cassandra_permission_cache_delays_changes() {
    let mut cfg = auth_config("db", Dialect::Cassandra);
    cfg.settings.permissions_validity_ms = 300;
    cfg.settings.roles_validity_ms = 300;
    cfg.settings.credentials_validity_ms = 300;
    let (db, _store) = open(cfg).await;
    let mut su = login(&db, "cassandra", "cassandra").await;
    ok(&mut su, KS).await;
    ok(&mut su, "CREATE TABLE ks.t (pk int PRIMARY KEY)").await;
    ok(
        &mut su,
        "CREATE ROLE a WITH PASSWORD = 'a' AND LOGIN = true",
    )
    .await;
    let mut a = login(&db, "a", "a").await;
    assert_eq!(err(&mut a, "SELECT * FROM ks.t").await.0, 0x2100);
    ok(&mut su, "GRANT SELECT ON ks.t TO a").await;
    // The denied set is cached (as observed on the reference with 2 s validity).
    assert_eq!(err(&mut a, "SELECT * FROM ks.t").await.0, 0x2100);
    tokio::time::sleep(Duration::from_millis(350)).await;
    ok(&mut a, "SELECT * FROM ks.t").await;
    ok(&mut su, "REVOKE SELECT ON ks.t FROM a").await;
    ok(&mut a, "SELECT * FROM ks.t").await;
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(err(&mut a, "SELECT * FROM ks.t").await.0, 0x2100);
    // Credentials: the old hash is cached for its validity.
    tokio::time::sleep(Duration::from_millis(5)).await;
    db.auth
        .clock_skew_us
        .fetch_add(10_000_000, std::sync::atomic::Ordering::Relaxed);
    login(&db, "a", "a").await;
    ok(&mut su, "ALTER ROLE a WITH PASSWORD = 'b'").await;
    login(&db, "a", "a").await;
    tokio::time::sleep(Duration::from_millis(350)).await;
    login(&db, "a", "b").await;
    // ScyllaDB applies changes at once.
    let (db, _store) = open(auth_config("db", Dialect::Scylla)).await;
    let mut su = login(&db, "cassandra", "cassandra").await;
    ok(&mut su, KS).await;
    ok(&mut su, "CREATE TABLE ks.t (pk int PRIMARY KEY)").await;
    ok(
        &mut su,
        "CREATE ROLE a WITH PASSWORD = 'a' AND LOGIN = true",
    )
    .await;
    let mut a = login(&db, "a", "a").await;
    assert_eq!(err(&mut a, "SELECT * FROM ks.t").await.0, 0x2100);
    ok(&mut su, "GRANT SELECT ON ks.t TO a").await;
    ok(&mut a, "SELECT * FROM ks.t").await;
}

#[tokio::test]
async fn cassandra_network_authorizer() {
    let mut cfg = auth_config("db", Dialect::Cassandra);
    cfg.settings.network_authorizer = "CassandraNetworkAuthorizer".into();
    let (db, _store) = open(cfg).await;
    let mut su = login(&db, "cassandra", "cassandra").await;
    ok(&mut su, KS).await;
    ok(&mut su, "CREATE TABLE ks.t (pk int PRIMARY KEY)").await;
    ok(
        &mut su,
        "CREATE ROLE dc WITH PASSWORD = 'dc' AND LOGIN = true AND ACCESS TO DATACENTERS {'datacenter1'}",
    )
    .await;
    ok(&mut su, "CREATE ROLE nl").await;
    ok(&mut su, "CREATE ROLE all WITH LOGIN = true").await;
    assert_eq!(
        rows(
            &mut su,
            "SELECT role, dcs FROM system_auth.network_permissions"
        )
        .await,
        table(&[
            &["roles/all", "{}"],
            &["roles/nl", "{}"],
            &["roles/dc", "{'datacenter1'}"]
        ])
    );
    let listed = rows(&mut su, "LIST ROLES").await;
    let dcs: Vec<(&str, &str)> = listed
        .iter()
        .map(|r| (r[0].as_str(), r[4].as_str()))
        .collect();
    assert_eq!(
        dcs,
        [
            ("all", "ALL"),
            ("cassandra", "ALL"),
            ("dc", "datacenter1"),
            ("nl", "n/a")
        ]
    );
    ok(&mut su, "GRANT SELECT ON ks.t TO dc").await;
    let mut dc = login(&db, "dc", "dc").await;
    ok(&mut dc, "SELECT * FROM ks.t").await;
    // Restricted to another datacenter (written directly: only known DCs pass validation).
    ok(
        &mut su,
        "UPDATE system_auth.network_permissions SET dcs = {'dc2'} WHERE role = 'roles/dc'",
    )
    .await;
    auth::store::ensure_fresh(&db).await.unwrap();
    let e = err(&mut dc, "SELECT * FROM ks.t").await;
    assert_eq!(
        e,
        (
            0x2100,
            "You do not have access to this datacenter (datacenter1)".into()
        )
    );
    assert_eq!(err(&mut dc, "USE ks").await.0, 0x2100);
    assert_eq!(err(&mut dc, "LIST ROLES").await.0, 0x2100);
    ok(&mut su, "ALTER ROLE dc WITH ACCESS TO ALL DATACENTERS").await;
    ok(&mut dc, "SELECT * FROM ks.t").await;
    assert_eq!(
        err(&mut su, "ALTER ROLE dc WITH ACCESS TO DATACENTERS {'nope'}")
            .await
            .1,
        "Invalid value(s) for DATACENTERS '[nope]',All values must be valid datacenters"
    );
    assert_eq!(
        err(&mut dc, "ALTER ROLE dc WITH ACCESS TO ALL DATACENTERS")
            .await
            .1,
        "Only superusers are allowed to alter access to datacenters."
    );
    assert_eq!(
        err(&mut dc, "ALTER ROLE dc WITH ACCESS FROM ALL CIDRS")
            .await
            .1,
        "Only superusers are allowed to alter access from CIDR groups."
    );
    ok(&mut su, "DROP ROLE dc").await;
    assert!(
        rows(
            &mut su,
            "SELECT role FROM system_auth.network_permissions WHERE role = 'roles/dc'"
        )
        .await
        .is_empty()
    );
}

#[tokio::test]
async fn direct_writes_to_auth_tables_apply() {
    let (db, _store) = open(auth_config("db", Dialect::Cassandra)).await;
    let mut su = login(&db, "cassandra", "cassandra").await;
    let h = auth::hash::hash("pw", Dialect::Cassandra, 4);
    ok(
        &mut su,
        &format!("INSERT INTO system_auth.roles (role, can_login, is_superuser, salted_hash) VALUES ('direct', true, false, '{h}')"),
    )
    .await;
    // The observer refreshes the snapshot in the background.
    tokio::time::sleep(Duration::from_millis(100)).await;
    login(&db, "direct", "pw").await;
    assert!(
        rows(&mut su, "LIST ROLES")
            .await
            .iter()
            .any(|r| r[0] == "direct")
    );
}

#[tokio::test]
async fn allow_all_authorizer_with_passwords() {
    for d in BOTH {
        let mut cfg = auth_config("db", d);
        cfg.settings.authorizer = "AllowAllAuthorizer".into();
        let (db, _store) = open(cfg).await;
        let mut su = login(&db, "cassandra", "cassandra").await;
        ok(&mut su, KS).await;
        ok(&mut su, "CREATE TABLE ks.t (pk int PRIMARY KEY)").await;
        ok(
            &mut su,
            "CREATE ROLE a WITH PASSWORD = 'a' AND LOGIN = true",
        )
        .await;
        let mut a = login(&db, "a", "a").await;
        // Everything is allowed, nothing is granted.
        ok(&mut a, "SELECT * FROM ks.t").await;
        ok(&mut a, "DROP TABLE ks.t").await;
        let e = q(&mut su, "GRANT SELECT ON KEYSPACE ks TO a")
            .await
            .unwrap_err();
        assert_eq!(
            e.message,
            "java.lang.UnsupportedOperationException: GRANT operation is not supported by AllowAllAuthorizer"
        );
        if d == Dialect::Cassandra {
            assert_eq!(
                err(&mut su, "LIST ALL PERMISSIONS").await.1,
                "LIST PERMISSIONS operation is not supported by AllowAllAuthorizer"
            );
            // The creator gets nothing (`grant` is unsupported).
            assert!(
                rows(&mut su, "SELECT * FROM system_auth.role_permissions")
                    .await
                    .is_empty()
            );
        }
    }
}

#[tokio::test]
async fn prepared_statements_are_authorized_at_execute() {
    for d in BOTH {
        let (db, _store) = open(auth_config("db", d)).await;
        let mut su = login(&db, "cassandra", "cassandra").await;
        ok(&mut su, KS).await;
        ok(&mut su, "CREATE TABLE ks.t (pk int PRIMARY KEY, v int)").await;
        ok(
            &mut su,
            "CREATE ROLE a WITH PASSWORD = 'a' AND LOGIN = true",
        )
        .await;
        let mut a = login(&db, "a", "a").await;
        let p = a
            .prepare("SELECT * FROM ks.t WHERE pk = ?", None)
            .await
            .unwrap();
        let ins = a
            .prepare("INSERT INTO ks.t (pk, v) VALUES (?, ?)", None)
            .await
            .unwrap();
        let exec = |id: Bytes, n: usize| (id, QueryParams::with_values(vec![int(1); n]));
        let (id, params) = exec(p.id.clone(), 1);
        assert_eq!(
            a.execute(&id, params).await.unwrap_err().code,
            ErrorCode::Unauthorized
        );
        ok(&mut su, "GRANT SELECT ON KEYSPACE ks TO a").await;
        let (id, params) = exec(p.id.clone(), 1);
        a.execute(&id, params).await.unwrap();
        let batch = crate::Batch {
            kind: crate::BatchKind::Logged,
            statements: vec![crate::BatchStatement::Prepared {
                id: ins.id.clone(),
                values: vec![int(1), int(2)],
            }],
            params: QueryParams::default(),
        };
        let e = a.batch(batch.clone()).await.unwrap_err();
        assert_eq!(
            e.message, "User a has no MODIFY permission on <table ks.t> or any of its parents",
            "{d}"
        );
        ok(&mut su, "GRANT MODIFY ON ks.t TO a").await;
        a.batch(batch).await.unwrap();
    }
}

#[tokio::test]
async fn cassandra_system_keyspaces_exclude_root_grants() {
    let (db, _store) = open(auth_config("db", Dialect::Cassandra)).await;
    let mut su = login(&db, "cassandra", "cassandra").await;
    ok(
        &mut su,
        "CREATE ROLE a WITH PASSWORD = 'a' AND LOGIN = true",
    )
    .await;
    ok(&mut su, "GRANT SELECT ON ALL KEYSPACES TO a").await;
    let mut a = login(&db, "a", "a").await;
    ok(&mut a, "SELECT * FROM system.local").await;
    assert_eq!(
        err(&mut a, "SELECT role FROM system_auth.roles").await.1,
        "User a has no SELECT permission on <table system_auth.roles> or any of its parents"
    );
    ok(&mut su, "GRANT SELECT ON KEYSPACE system_auth TO a").await;
    ok(&mut a, "SELECT role FROM system_auth.roles").await;
    // ScyllaDB: ALL KEYSPACES covers system tables.
    let (db, _store) = open(auth_config("db", Dialect::Scylla)).await;
    let mut su = login(&db, "cassandra", "cassandra").await;
    ok(
        &mut su,
        "CREATE ROLE a WITH PASSWORD = 'a' AND LOGIN = true",
    )
    .await;
    let mut a = login(&db, "a", "a").await;
    assert_eq!(
        err(&mut a, "SELECT * FROM system.versions").await.1,
        "User a has none of the permissions (SELECT, TEXT_SEARCH_INDEXING, VECTOR_SEARCH_INDEXING) on <table system.versions> or any of its parents"
    );
    ok(&mut su, "GRANT SELECT ON ALL KEYSPACES TO a").await;
    ok(&mut a, "SELECT role FROM system.roles").await;
}

#[tokio::test]
async fn hashed_passwords_log_in() {
    let (db, _store) = open(auth_config("db", Dialect::Scylla)).await;
    let mut su = login(&db, "cassandra", "cassandra").await;
    // The example of ScyllaDB's documentation (`crypt("myPassword", "$5$")`) and a reference hash.
    ok(
        &mut su,
        "CREATE ROLE doc WITH HASHED PASSWORD = '$5$$QdpObVOY40UOyo.BuWdpRq0Cr/DPkw7ckBEs2NqWVn5' AND LOGIN = true",
    )
    .await;
    ok(
        &mut su,
        "CREATE ROLE ref WITH HASHED PASSWORD = '$6$vyPzWOYrRHKwqRkm$bYWoOqzmzCmJBpSOwKJabQqFPxgwfkVqUzNekIxcFda4pkFgYiR/Sktbqkzd97LsbKnRduIwr2mhUMqqyNHFP1' AND LOGIN = true",
    )
    .await;
    login(&db, "doc", "myPassword").await;
    login(&db, "ref", "alice").await;
    let (db, _store) = open(auth_config("db", Dialect::Cassandra)).await;
    let mut su = login(&db, "cassandra", "cassandra").await;
    ok(
        &mut su,
        "CREATE ROLE ref WITH HASHED PASSWORD = '$2a$10$9tOsOB7hv7yuTpOaO0rnZ.8PkletfBblQxxL38KQdry4bTgbUyaiS' AND LOGIN = true",
    )
    .await;
    login(&db, "ref", "cassandra").await;
    assert_eq!(
        err(&mut su, "CREATE USER u WITH HASHED PASSWORD 'notahash'")
            .await
            .1,
        "Invalid hashed password value. Please use jBcrypt."
    );
}

#[tokio::test]
async fn masking_permissions_api() {
    let (db, _store) = open(auth_config("db", Dialect::Cassandra)).await;
    let mut su = login(&db, "cassandra", "cassandra").await;
    ok(&mut su, KS).await;
    ok(&mut su, "CREATE TABLE ks.t (pk int PRIMARY KEY)").await;
    ok(
        &mut su,
        "CREATE ROLE a WITH PASSWORD = 'a' AND LOGIN = true",
    )
    .await;
    let a = login(&db, "a", "a").await;
    assert!(auth::can_unmask(&db, &su.state, "ks", "t"));
    assert!(!auth::can_unmask(&db, &a.state, "ks", "t"));
    assert!(!auth::can_select_masked(&db, &a.state, "ks", "t"));
    ok(&mut su, "GRANT UNMASK ON KEYSPACE ks TO a").await;
    ok(&mut su, "GRANT SELECT_MASKED ON ks.t TO a").await;
    assert!(auth::can_unmask(&db, &a.state, "ks", "t"));
    assert!(auth::can_select_masked(&db, &a.state, "ks", "t"));
    let (db, _store) = open(auth_config("db", Dialect::Scylla)).await;
    let s = db.connect(Default::default());
    assert!(auth::can_unmask(&db, &s.state, "ks", "t"));
}

/// Creator grants and cleanup for functions (the UDF subsystem returns these changes) and the
/// `Access` forms other subsystems authorize with.
#[tokio::test]
async fn function_permissions_and_access_forms() {
    for d in BOTH {
        let (db, _store) = open(auth_config("db", d)).await;
        let mut su = login(&db, "cassandra", "cassandra").await;
        ok(&mut su, KS).await;
        ok(&mut su, "CREATE TABLE ks.t (pk int PRIMARY KEY)").await;
        ok(
            &mut su,
            "CREATE ROLE a WITH PASSWORD = 'a' AND LOGIN = true",
        )
        .await;
        let a = login(&db, "a", "a").await;
        let authz = db.hooks().authorizer;
        let created = SchemaChange {
            change: ChangeType::Created,
            target: ChangeTarget::Function,
            keyspace: "ks".into(),
            name: Some("f".into()),
            arg_types: Some(vec!["int".into()]),
        };
        authz
            .schema_changed(&db, &a.state, &db.schema(), &created)
            .await
            .unwrap();
        let f = auth::Resource::Function {
            keyspace: "ks".into(),
            name: "f".into(),
            marshal: vec!["org.apache.cassandra.db.marshal.Int32Type".into()],
            cql: vec!["int".into()],
        };
        let got = auth::snapshot(&db).granted("a", &f.name());
        // ScyllaDB grants a function's creator nothing (probed: `captured_udf`,
        // `permissions_scylla/replace_own`).
        let want: &[&str] = if d.is_scylla() {
            &[]
        } else {
            &["ALTER", "AUTHORIZE", "DROP", "EXECUTE"]
        };
        assert_eq!(got.into_iter().collect::<Vec<_>>(), want, "{d}");
        let check = |p, r: &auth::Resource| {
            authz.authorize(
                &db,
                &a.state,
                &Access::Permission {
                    permission: p,
                    resource: r.clone(),
                },
            )
        };
        let execute = check(nosql_cql::ast::Permission::Execute, &f);
        match d {
            Dialect::Scylla => assert_eq!(
                execute.unwrap_err().message,
                "User a has no EXECUTE permission on <function ks.f(int)> or any of its parents"
            ),
            Dialect::Cassandra => execute.unwrap(),
        }
        let other = auth::Resource::FunctionsIn("ks".into());
        assert_eq!(
            check(nosql_cql::ast::Permission::Create, &other)
                .unwrap_err()
                .message,
            "User a has no CREATE permission on <all functions in ks> or any of its parents"
        );
        let schema = |s: &'static str| {
            authz.authorize(
                &db,
                &a.state,
                &Access::Schema {
                    keyspace: "ks",
                    table: "t",
                    statement: s,
                },
            )
        };
        for (stmt, msg) in [
            (
                "CREATE INDEX",
                "User a has no ALTER permission on <table ks.t> or any of its parents",
            ),
            (
                "CREATE MATERIALIZED VIEW",
                "User a has no ALTER permission on <table ks.t> or any of its parents",
            ),
            (
                "CREATE FUNCTION",
                "User a has no CREATE permission on <all functions in ks> or any of its parents",
            ),
            (
                "CREATE TRIGGER",
                "Only superusers are allowed to perform CREATE TRIGGER queries",
            ),
            (
                "DROP TRIGGER",
                "Only superusers are allowed to perfrom DROP TRIGGER queries",
            ),
        ] {
            assert_eq!(schema(stmt).unwrap_err().message, msg, "{d}: {stmt}");
        }
        authz
            .authorize(
                &db,
                &su.state,
                &Access::Schema {
                    keyspace: "ks",
                    table: "t",
                    statement: "CREATE TRIGGER",
                },
            )
            .unwrap();
        let dropped = SchemaChange {
            change: ChangeType::Dropped,
            ..created.clone()
        };
        authz
            .schema_changed(&db, &a.state, &db.schema(), &dropped)
            .await
            .unwrap();
        assert!(
            auth::snapshot(&db).granted("a", &f.name()).is_empty(),
            "{d}"
        );
    }
}

#[tokio::test]
async fn truncate_and_cassandra_grant_warnings() {
    let (db, _store) = open(auth_config("db", Dialect::Cassandra)).await;
    let mut su = login(&db, "cassandra", "cassandra").await;
    ok(&mut su, KS).await;
    ok(&mut su, "CREATE TABLE ks.t (pk int PRIMARY KEY)").await;
    ok(
        &mut su,
        "CREATE ROLE a WITH PASSWORD = 'a' AND LOGIN = true",
    )
    .await;
    let mut a = login(&db, "a", "a").await;
    assert_eq!(err(&mut a, "TRUNCATE ks.nope").await.0, 0x2200);
    assert_eq!(
        err(&mut a, "TRUNCATE ks.t").await.1,
        "User a has no MODIFY permission on <table ks.t> or any of its parents"
    );
    let r = ok(&mut su, "GRANT SELECT, MODIFY ON ks.t TO a").await;
    assert!(r.warnings.is_empty());
    ok(&mut a, "TRUNCATE ks.t").await;
    let r = ok(&mut su, "GRANT SELECT, ALTER ON ks.t TO a").await;
    assert_eq!(
        r.warnings,
        ["Role 'a' was already granted SELECT on <table ks.t>"]
    );
    let r = ok(&mut su, "REVOKE DROP, ALTER ON ks.t FROM a").await;
    assert_eq!(
        r.warnings,
        ["Role 'a' was not granted DROP on <table ks.t>"]
    );
}
