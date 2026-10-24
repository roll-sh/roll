//! The wire protocol server, driven by a real client (tokio-postgres): SCRAM, the extended
//! protocol with binary parameters and results, portals, COPY, cancel and terminate,
//! databases opening and closing, LISTEN / NOTIFY between connections.

use futures::{SinkExt, StreamExt};
use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::server::{AuthMethod, ServerOptions};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio_postgres::{AsyncMessage, Client, NoTls, SimpleQueryMessage};

struct Server {
    engine: Arc<Engine>,
    addr: String,
    _dir: tempfile::TempDir,
}

fn server(auth: AuthMethod) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let opts = EngineOptions {
        data_dir: dir.path().to_path_buf(),
        cache_bytes: 4 << 20,
        memtable_bytes: 1 << 20,
        sync: false,
        idle_close: Duration::from_millis(100),
        superuser: "postgres".into(),
        password: Some("secret".into()),
        server_files: None,
    };
    let engine = Engine::open(opts).unwrap();
    let addr = pg_engine::server::spawn(engine.clone(), "127.0.0.1:0", ServerOptions { auth })
        .unwrap()
        .to_string();
    Server {
        engine,
        addr,
        _dir: dir,
    }
}

impl Server {
    fn config(&self, db: &str, password: &str) -> tokio_postgres::Config {
        let (host, port) = self.addr.rsplit_once(':').unwrap();
        let mut c = tokio_postgres::Config::new();
        c.host(host)
            .port(port.parse().unwrap())
            .user("postgres")
            .password(password)
            .dbname(db);
        c
    }

    async fn connect(&self, db: &str) -> Client {
        let (client, conn) = self
            .config(db, "secret")
            .connect(NoTls)
            .await
            .expect("connect");
        tokio::spawn(conn);
        client
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn authentication() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let c = s.connect("postgres").await;
        let row = c
            .query_one("SELECT current_user, current_database(), session_user", &[])
            .await
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "postgres");
        assert_eq!(row.get::<_, String>(1), "postgres");

        let err = s
            .config("postgres", "wrong")
            .connect(NoTls)
            .await
            .err()
            .expect("bad password must fail");
        assert_eq!(err.code().map(|c| c.code()), Some("28P01"));
        let mut bad_user = s.config("postgres", "secret");
        bad_user.user("nobody");
        let err = bad_user
            .connect(NoTls)
            .await
            .err()
            .expect("unknown role must fail");
        assert_eq!(err.code().map(|c| c.code()), Some("28P01"));
        let err = s
            .config("nope", "secret")
            .connect(NoTls)
            .await
            .err()
            .expect("unknown database must fail");
        assert_eq!(err.code().map(|c| c.code()), Some("3D000"));
        let err = s
            .config("template0", "secret")
            .connect(NoTls)
            .await
            .err()
            .expect("template0 refuses connections");
        assert_eq!(err.code().map(|c| c.code()), Some("55000"));

        // A role made with a password can log in; one without LOGIN can't.
        c.batch_execute("CREATE ROLE app LOGIN PASSWORD 'apppw'; CREATE ROLE nologin PASSWORD 'x'")
            .await
            .unwrap();
        let mut app = s.config("postgres", "apppw");
        app.user("app");
        let (ac, conn) = app.connect(NoTls).await.unwrap();
        tokio::spawn(conn);
        assert_eq!(
            ac.query_one("SELECT current_user", &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            "app"
        );
        let mut nl = s.config("postgres", "x");
        nl.user("nologin");
        let err = nl.connect(NoTls).await.err().unwrap();
        assert_eq!(err.code().map(|c| c.code()), Some("28000"));

        // An expired password fails like a wrong one.
        c.batch_execute("CREATE ROLE expired LOGIN PASSWORD 'pw' VALID UNTIL '2000-01-01'")
            .await
            .unwrap();
        let mut ex = s.config("postgres", "pw");
        ex.user("expired");
        let err = ex.connect(NoTls).await.err().unwrap();
        assert_eq!(err.code().map(|c| c.code()), Some("28P01"));
        c.batch_execute("ALTER ROLE expired VALID UNTIL 'infinity'")
            .await
            .unwrap();
        let (_ec, conn) = ex.connect(NoTls).await.unwrap();
        tokio::spawn(conn);
    });
    s.engine.shutdown();
}

#[test]
fn trust_and_startup_options() {
    let s = server(AuthMethod::Trust);
    runtime().block_on(async {
        let mut cfg = s.config("postgres", "");
        cfg.options("-c search_path=myschema,public -c statement_timeout=5s").application_name("wiretest");
        let (c, conn) = cfg.connect(NoTls).await.unwrap();
        tokio::spawn(conn);
        let row = c.query_one("SELECT current_setting('search_path'), current_setting('statement_timeout'), current_setting('application_name')", &[]).await.unwrap();
        assert_eq!(row.get::<_, String>(0), "myschema,public");
        assert_eq!(row.get::<_, String>(1), "5s");
        assert_eq!(row.get::<_, String>(2), "wiretest");
        let mut bad = s.config("postgres", "");
        bad.options("-c no_such_setting=1");
        let err = bad.connect(NoTls).await.err().unwrap();
        assert_eq!(err.code().map(|c| c.code()), Some("42704"));
    });
    s.engine.shutdown();
}

#[test]
fn login_checks_and_role_settings() {
    let s = server(AuthMethod::Trust);
    runtime().block_on(async {
        let c = s.connect("postgres").await;
        c.batch_execute(
            "CREATE ROLE lim LOGIN CONNECTION LIMIT 1;
             ALTER ROLE lim SET work_mem = '8MB';
             ALTER ROLE lim IN DATABASE postgres SET statement_timeout = '7s';
             ALTER ROLE lim SET statement_timeout = '1s';
             CREATE ROLE other LOGIN",
        )
        .await
        .unwrap();
        let as_role = |user: &str| {
            let mut cfg = s.config("postgres", "");
            cfg.user(user);
            cfg
        };
        // Role settings apply at login; the database-specific one wins; RESET goes back to them.
        let (lc, conn) = as_role("lim").connect(NoTls).await.unwrap();
        tokio::spawn(conn);
        let row = lc
            .query_one(
                "SELECT current_setting('work_mem'), current_setting('statement_timeout')",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "8MB");
        assert_eq!(row.get::<_, String>(1), "7s");
        lc.batch_execute("SET work_mem = '1MB'; RESET work_mem")
            .await
            .unwrap();
        assert_eq!(
            lc.query_one("SHOW work_mem", &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            "8MB"
        );
        // CONNECTION LIMIT 1: a second session is refused.
        let err = as_role("lim")
            .connect(NoTls)
            .await
            .err()
            .expect("connection limit");
        assert_eq!(err.code().map(|c| c.code()), Some("53300"));
        drop(lc);
        // CONNECT privilege.
        c.batch_execute("REVOKE CONNECT ON DATABASE postgres FROM PUBLIC")
            .await
            .unwrap();
        let err = as_role("other")
            .connect(NoTls)
            .await
            .err()
            .expect("no CONNECT");
        assert_eq!(err.code().map(|c| c.code()), Some("42501"));
        c.batch_execute("GRANT CONNECT ON DATABASE postgres TO other")
            .await
            .unwrap();
        let (oc, conn) = as_role("other").connect(NoTls).await.unwrap();
        tokio::spawn(conn);
        assert_eq!(
            oc.query_one("SELECT current_user", &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            "other"
        );
        // The superuser always connects.
        let _again = s.connect("postgres").await;
    });
    s.engine.shutdown();
}

#[test]
fn extended_protocol_types() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let c = s.connect("postgres").await;
        c.batch_execute(
            "CREATE TABLE t (id int8 PRIMARY KEY, name text, ok bool, score float8, n int4, data bytea, tags int4[], at timestamptz, j jsonb)",
        )
        .await
        .unwrap();
        let ins = c.prepare("INSERT INTO t VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::text::jsonb)").await.unwrap();
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        for i in 0..5i64 {
            let tags: Vec<i32> = (0..i as i32).collect();
            let n = c
                .execute(&ins, &[&i, &format!("row {i}"), &(i % 2 == 0), &(i as f64 / 2.0), &(i as i32 * 10), &vec![i as u8, 0xff], &tags, &at, &format!("{{\"i\": {i}}}")])
                .await
                .unwrap();
            assert_eq!(n, 1);
        }
        let none: Option<String> = None;
        c.execute("INSERT INTO t (id, name) VALUES ($1, $2)", &[&99i64, &none]).await.unwrap();

        let rows = c.query("SELECT id, name, ok, score, n, data, tags, at, j::text FROM t WHERE id < $1 ORDER BY id", &[&3i64]).await.unwrap();
        assert_eq!(rows.len(), 3);
        let r = &rows[2];
        assert_eq!(r.get::<_, i64>(0), 2);
        assert_eq!(r.get::<_, String>(1), "row 2");
        assert!(r.get::<_, bool>(2));
        assert_eq!(r.get::<_, f64>(3), 1.0);
        assert_eq!(r.get::<_, i32>(4), 20);
        assert_eq!(r.get::<_, Vec<u8>>(5), vec![2, 0xff]);
        assert_eq!(r.get::<_, Vec<i32>>(6), vec![0, 1]);
        assert_eq!(r.get::<_, SystemTime>(7), at);
        assert_eq!(r.get::<_, String>(8), "{\"i\": 2}");
        let null = c.query_one("SELECT name, ok FROM t WHERE id = 99", &[]).await.unwrap();
        assert_eq!(null.get::<_, Option<String>>(0), None);
        assert_eq!(null.get::<_, Option<bool>>(1), None);

        // Statement metadata: parameter and column types.
        let st = c.prepare("SELECT name, n FROM t WHERE id = $1 AND ok = $2").await.unwrap();
        assert_eq!(st.params(), &[tokio_postgres::types::Type::INT8, tokio_postgres::types::Type::BOOL]);
        assert_eq!(st.columns().iter().map(|c| c.name()).collect::<Vec<_>>(), ["name", "n"]);
        assert_eq!(st.columns()[1].type_(), &tokio_postgres::types::Type::INT4);

        // Domain-typed columns report the base type, as Postgres's printtup does.
        c.batch_execute("CREATE DOMAIN pos AS int CHECK (VALUE > 0); CREATE TABLE dt (x pos); INSERT INTO dt VALUES (3)").await.unwrap();
        for q in ["SELECT x FROM dt", "SELECT 3::pos"] {
            assert_eq!(c.prepare(q).await.unwrap().columns()[0].type_(), &tokio_postgres::types::Type::INT4);
        }

        // An error, then the connection carries on.
        let err = c.query("SELECT 1/0", &[]).await.unwrap_err();
        assert_eq!(err.code().map(|c| c.code()), Some("22012"));
        let err = c.prepare("SELECT nope FROM t").await.unwrap_err();
        assert_eq!(err.code().map(|c| c.code()), Some("42703"));
        assert_eq!(c.query_one("SELECT count(*) FROM t", &[]).await.unwrap().get::<_, i64>(0), 6);

        // Simple query protocol: several statements, text results.
        let msgs = c.simple_query("SELECT 1 AS a; SELECT 'x' AS b").await.unwrap();
        let vals: Vec<String> = msgs
            .iter()
            .filter_map(|m| match m {
                SimpleQueryMessage::Row(r) => Some(r.get(0).unwrap().to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(vals, ["1", "x"]);
    });
    s.engine.shutdown();
}

/// A text-format parameter of an object identifier type (libpq's PQexecParams, as pgbench -i
/// sends `SELECT relkind FROM pg_class WHERE oid = $1::regclass`) is looked up by name in
/// the transaction's catalog, as regclassin does.
#[test]
fn text_parameters_of_reg_types() {
    let s = server(AuthMethod::ScramSha256);
    let (pid, _, _) = s.engine.register_backend(None, None);
    let mut c = s.engine.connect(pid, "postgres", "postgres", "t").unwrap();
    let relkind = |c: &mut pg_engine::session::Session, name: &str| -> Result<String, String> {
        c.parse_message("", "SELECT relkind FROM pg_catalog.pg_class WHERE oid = $1::pg_catalog.regclass", &[0])
            .and_then(|_| c.bind_message("", "", vec![Some(name.as_bytes().to_vec())], &[], Vec::new()))
            .and_then(|_| c.execute_portal("", 0))
            .map(|(o, _)| match o {
                pg_engine::session::Outcome::Done(r) => format!("{:?}", r.rows),
                _ => String::new(),
            })
            .map_err(|e| format!("{}: {}", e.code, e.message))
    };
    c.query("CREATE TABLE before_txn (a int)").unwrap();
    assert_eq!(relkind(&mut c, "before_txn").unwrap(), relkind(&mut c, "public.before_txn").unwrap());
    c.sync().unwrap();
    c.query("BEGIN; CREATE TABLE in_txn (a int)").unwrap();
    assert!(relkind(&mut c, "in_txn").is_ok());
    c.sync().unwrap();
    assert_eq!(relkind(&mut c, "nope").unwrap_err(), "42P01: relation \"nope\" does not exist");
    c.sync().unwrap();
    c.query("ROLLBACK").unwrap();
    s.engine.shutdown();
}

#[test]
fn rows_before_an_error() {
    // A SELECT's rows go out as they are made: those before an error reach the client
    // first (psql's FETCH_COUNT prints the full chunks of them).
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let c = s.connect("postgres").await;
        let stream = c
            .simple_query_raw("SELECT 1 / (3 - g) FROM generate_series(1, 5) g")
            .await
            .unwrap();
        futures::pin_mut!(stream);
        let mut rows = 0;
        let mut code = None;
        while let Some(m) = stream.next().await {
            match m {
                Ok(SimpleQueryMessage::Row(_)) => rows += 1,
                Ok(_) => {}
                Err(e) => {
                    code = e.code().map(|c| c.code().to_string());
                    break;
                }
            }
        }
        assert_eq!(rows, 2);
        assert_eq!(code.as_deref(), Some("22012"));
        // The connection goes on.
        let r = c.query_one("SELECT 1 + 1", &[]).await.unwrap();
        assert_eq!(r.get::<_, i32>(0), 2);
    });
    s.engine.shutdown();
}

#[test]
fn transactions_and_portals() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let mut c = s.connect("postgres").await;
        c.batch_execute("CREATE TABLE n (v int)").await.unwrap();
        {
            let tx = c.transaction().await.unwrap();
            tx.execute("INSERT INTO n SELECT generate_series(1, 10)", &[]).await.unwrap();
            // A portal fetched a few rows at a time (Execute with a row limit).
            let st = tx.prepare("SELECT v FROM n ORDER BY v").await.unwrap();
            let portal = tx.bind(&st, &[]).await.unwrap();
            let first = tx.query_portal(&portal, 4).await.unwrap();
            let rest = tx.query_portal(&portal, 100).await.unwrap();
            assert_eq!(first.iter().map(|r| r.get::<_, i32>(0)).collect::<Vec<_>>(), [1, 2, 3, 4]);
            assert_eq!(rest.len(), 6);
            tx.rollback().await.unwrap();
        }
        assert_eq!(c.query_one("SELECT count(*) FROM n", &[]).await.unwrap().get::<_, i64>(0), 0);
        {
            let tx = c.transaction().await.unwrap();
            tx.execute("INSERT INTO n VALUES (1)", &[]).await.unwrap();
            // A failed statement aborts the block until rollback.
            assert!(tx.execute("INSERT INTO n VALUES ('x')", &[]).await.is_err());
            let err = tx.execute("INSERT INTO n VALUES (2)", &[]).await.unwrap_err();
            assert_eq!(err.code().map(|c| c.code()), Some("25P02"));
        }
        let tx = c.transaction().await.unwrap();
        tx.execute("INSERT INTO n VALUES (7)", &[]).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(c.query_one("SELECT sum(v)::int4 FROM n", &[]).await.unwrap().get::<_, i32>(0), 7);

        // Procedures: output parameters over the extended protocol, and COMMIT inside one.
        c.batch_execute(
            "CREATE PROCEDURE twice(INOUT x int4, OUT label text) LANGUAGE plpgsql AS $$ BEGIN x := x * 2; label := 'x' || x; END $$;
             CREATE PROCEDURE add_committed(v int4) LANGUAGE plpgsql AS $$ BEGIN INSERT INTO n VALUES (v); COMMIT; END $$",
        )
        .await
        .unwrap();
        let none: Option<String> = None;
        let row = c.query_one("CALL twice($1, $2)", &[&21i32, &none]).await.unwrap();
        assert_eq!(row.get::<_, i32>(0), 42);
        assert_eq!(row.get::<_, String>(1), "x42");
        c.execute("CALL add_committed($1)", &[&1i32]).await.unwrap();
        // Several statements in one query string form a transaction block: no COMMIT inside.
        let err = c.batch_execute("CALL add_committed(2); SELECT 1").await.unwrap_err();
        assert_eq!(err.code().map(|c| c.code()), Some("2D000"));
        assert_eq!(c.query_one("SELECT sum(v)::int4 FROM n", &[]).await.unwrap().get::<_, i32>(0), 8);
    });
    s.engine.shutdown();
}

/// pg_export_snapshot() / SET TRANSACTION SNAPSHOT, as `pg_dump -j` uses them: workers
/// importing the leader's snapshot see its data as of the export, not later commits, until
/// the exporting transaction ends.
#[test]
fn exported_snapshots() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let leader = s.connect("postgres").await;
        let worker = s.connect("postgres").await;
        let writer = s.connect("postgres").await;
        leader.batch_execute("CREATE TABLE snap (v int); INSERT INTO snap VALUES (1)").await.unwrap();
        leader
            .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .await
            .unwrap();
        let id: String = leader.query_one("SELECT pg_export_snapshot()", &[]).await.unwrap().get(0);
        assert!(id.ends_with("-1"), "{id}");
        writer.batch_execute("INSERT INTO snap VALUES (2)").await.unwrap();
        let count = "SELECT count(*) FROM snap";
        worker
            .batch_execute(&format!(
                "BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY; SET TRANSACTION SNAPSHOT '{id}'"
            ))
            .await
            .unwrap();
        assert_eq!(worker.query_one(count, &[]).await.unwrap().get::<_, i64>(0), 1);
        worker.batch_execute("COMMIT").await.unwrap();
        // Errors: READ COMMITTED, a query first, a malformed or unknown identifier, a
        // serializable importer of a non-serializable snapshot.
        let code = |e: tokio_postgres::Error| e.code().map(|c| c.code().to_string());
        let set = format!("SET TRANSACTION SNAPSHOT '{id}'");
        worker.batch_execute("BEGIN").await.unwrap();
        let e = worker.batch_execute(&set).await.unwrap_err();
        assert_eq!(code(e).as_deref(), Some("0A000"));
        worker.batch_execute("ROLLBACK; BEGIN ISOLATION LEVEL REPEATABLE READ").await.unwrap();
        worker.batch_execute("SELECT pg_current_xact_id()").await.unwrap();
        let e = worker.batch_execute(&set).await.unwrap_err();
        assert_eq!(code(e).as_deref(), Some("25001"));
        worker.batch_execute("ROLLBACK; BEGIN ISOLATION LEVEL REPEATABLE READ").await.unwrap();
        let e = worker.batch_execute("SET TRANSACTION SNAPSHOT 'x/y'").await.unwrap_err();
        assert_eq!(code(e).as_deref(), Some("22023"));
        worker.batch_execute("ROLLBACK; BEGIN ISOLATION LEVEL SERIALIZABLE").await.unwrap();
        let e = worker.batch_execute(&set).await.unwrap_err();
        assert_eq!(code(e).as_deref(), Some("0A000"));
        worker.batch_execute("ROLLBACK").await.unwrap();
        // A second export of the same transaction counts on; a subtransaction can't export.
        let id2: String = leader.query_one("SELECT pg_export_snapshot()", &[]).await.unwrap().get(0);
        assert!(id2.ends_with("-2") && id2[..id2.len() - 2] == id[..id.len() - 2], "{id2}");
        leader.batch_execute("SAVEPOINT a").await.unwrap();
        let e = leader.query_one("SELECT pg_export_snapshot()", &[]).await.unwrap_err();
        assert_eq!(code(e).as_deref(), Some("25001"));
        // Once the exporter ends, its snapshots are gone.
        leader.batch_execute("ROLLBACK").await.unwrap();
        worker.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ").await.unwrap();
        let e = worker.batch_execute(&set).await.unwrap_err();
        assert_eq!(code(e).as_deref(), Some("42704"));
        worker.batch_execute("ROLLBACK").await.unwrap();
        assert_eq!(worker.query_one(count, &[]).await.unwrap().get::<_, i64>(0), 2);
    });
    s.engine.shutdown();
}

/// DDL of transactions running at once commits as in Postgres (`pg_restore -j` makes a
/// database's indexes and constraints that way): changes to different objects, and indexes
/// added to one table, all stay; a name both took fails the later commit; one object both
/// changed fails it as a serialization failure.
#[test]
fn concurrent_ddl() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        let b = s.connect("postgres").await;
        a.batch_execute(
            "CREATE TABLE t1 (x int, y int); CREATE TABLE t2 (x int);
             CREATE TABLE p (k int, v int) PARTITION BY HASH (k);
             CREATE TABLE p1 PARTITION OF p FOR VALUES WITH (MODULUS 2, REMAINDER 0)",
        )
        .await
        .unwrap();
        let code = |e: tokio_postgres::Error| e.code().map(|c| c.code().to_string());
        let both = |sa: &'static str, sb: &'static str| {
            let (a, b) = (&a, &b);
            async move {
                a.batch_execute(&format!("BEGIN; {sa}")).await.unwrap();
                b.batch_execute(&format!("BEGIN; {sb}")).await.unwrap();
                a.batch_execute("COMMIT").await.unwrap();
                b.batch_execute("COMMIT").await
            }
        };
        both("CREATE INDEX t1_x ON t1 (x)", "CREATE INDEX t2_x ON t2 (x); COMMENT ON TABLE t2 IS 'c'")
            .await
            .unwrap();
        both("CREATE INDEX t1_y ON t1 (y)", "CREATE INDEX t1_xy ON t1 (x, y)").await.unwrap();
        both(
            "ALTER TABLE ONLY p ADD CONSTRAINT p_key UNIQUE (k, v)",
            "ALTER TABLE ONLY p1 ADD CONSTRAINT p1_key UNIQUE (k, v)",
        )
        .await
        .unwrap();
        a.batch_execute("ALTER INDEX p_key ATTACH PARTITION p1_key").await.unwrap();
        let names: Vec<String> = a
            .query(
                "SELECT relname::text FROM pg_class WHERE relname ~ '^(t1_|t2_|p_key|p1_key)' ORDER BY 1",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
        assert_eq!(names, ["p1_key", "p_key", "t1_x", "t1_xy", "t1_y", "t2_x"]);
        let c: String = a.query_one("SELECT obj_description('t2'::regclass)", &[]).await.unwrap().get(0);
        assert_eq!(c, "c");
        // The same name taken by both.
        let e = both("CREATE TABLE n (a int)", "CREATE TABLE n (b text)").await.unwrap_err();
        assert_eq!(code(e).as_deref(), Some("23505"));
        // The same object changed by both.
        let e = both("ALTER TABLE t2 RENAME TO t3", "ALTER TABLE t2 RENAME TO t4").await.unwrap_err();
        assert_eq!(code(e).as_deref(), Some("40001"));
        b.batch_execute("ROLLBACK").await.ok();
        let n: i64 = a
            .query_one("SELECT count(*) FROM pg_class WHERE relname IN ('n', 't3', 't4')", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(n, 2);
    });
    s.engine.shutdown();
}

/// A database closed cleanly (idle, or at shutdown) keeps its sequences' positions, as
/// Postgres's shutdown checkpoint does; only a crash skips the values logged ahead.
#[test]
fn sequences_survive_a_clean_close() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        a.batch_execute("CREATE DATABASE seqs").await.unwrap();
        {
            let c = s.connect("seqs").await;
            c.batch_execute("CREATE SEQUENCE s1; CREATE TABLE t (id serial, v int)").await.unwrap();
            c.batch_execute("SELECT nextval('s1'), nextval('s1'), nextval('s1'); INSERT INTO t (v) VALUES (1), (2)")
                .await
                .unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while s.engine.open_databases() > 1 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(s.engine.open_databases(), 1);
        let c = s.connect("seqs").await;
        let row = c
            .query_one("SELECT last_value, is_called FROM s1", &[])
            .await
            .unwrap();
        assert_eq!((row.get::<_, i64>(0), row.get::<_, bool>(1)), (3, true));
        let n: i64 = c.query_one("SELECT nextval('s1')", &[]).await.unwrap().get(0);
        assert_eq!(n, 4);
        let id: i32 = c.query_one("INSERT INTO t (v) VALUES (3) RETURNING id", &[]).await.unwrap().get(0);
        assert_eq!(id, 3);
    });
    s.engine.shutdown();
}

#[test]
fn copy_in_and_out() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let c = s.connect("postgres").await;
        c.batch_execute("CREATE TABLE cp (id int, name text)").await.unwrap();
        let sink = c.copy_in("COPY cp FROM STDIN").await.unwrap();
        futures::pin_mut!(sink);
        let mut data = String::new();
        for i in 0..1000 {
            data.push_str(&format!("{i}\tname {i}\n"));
        }
        // Several CopyData messages, split mid-line.
        for chunk in data.as_bytes().chunks(777) {
            sink.send(bytes::Bytes::copy_from_slice(chunk)).await.unwrap();
        }
        let n = sink.finish().await.unwrap();
        assert_eq!(n, 1000);
        let out = c.copy_out("COPY (SELECT id, name FROM cp WHERE id < 3 ORDER BY id) TO STDOUT WITH (FORMAT csv)").await.unwrap();
        futures::pin_mut!(out);
        let mut text = Vec::new();
        while let Some(chunk) = out.next().await {
            text.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(String::from_utf8(text).unwrap(), "0,name 0\n1,name 1\n2,name 2\n");
        // Bad data fails the COPY and the connection stays usable.
        let sink = c.copy_in("COPY cp FROM STDIN").await.unwrap();
        futures::pin_mut!(sink);
        sink.send(bytes::Bytes::from_static(b"x\ty\n")).await.unwrap();
        let err = sink.finish().await.unwrap_err();
        assert_eq!(err.code().map(|c| c.code()), Some("22P02"));
        assert_eq!(c.query_one("SELECT count(*) FROM cp", &[]).await.unwrap().get::<_, i64>(0), 1000);
    });
    s.engine.shutdown();
}

#[test]
fn cancel_and_terminate() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let c = s.connect("postgres").await;
        let token = c.cancel_token();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            token.cancel_query(NoTls).await.unwrap();
        });
        let start = Instant::now();
        let err = c.simple_query("SELECT pg_sleep(30)").await.unwrap_err();
        canceller.await.unwrap();
        assert_eq!(err.code().map(|c| c.code()), Some("57014"));
        assert!(start.elapsed() < Duration::from_secs(10));
        assert_eq!(
            c.query_one("SELECT 1::int4", &[])
                .await
                .unwrap()
                .get::<_, i32>(0),
            1
        );

        // pg_stat_activity shows the other session; pg_terminate_backend ends it.
        let victim = s.connect("postgres").await;
        let pid: i32 = victim
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        let seen: i64 = c
            .query_one(
                "SELECT count(*) FROM pg_stat_activity WHERE pid = $1",
                &[&pid],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(seen, 1);
        assert!(
            c.query_one("SELECT pg_terminate_backend($1)", &[&pid])
                .await
                .unwrap()
                .get::<_, bool>(0)
        );
        let err = victim.simple_query("SELECT 1").await.unwrap_err();
        assert!(
            err.is_closed() || err.code().map(|c| c.code()) == Some("57P01"),
            "{err:?}"
        );
    });
    s.engine.shutdown();
}

#[test]
fn databases_open_and_close() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let admin = s.connect("postgres").await;
        admin
            .batch_execute("CREATE DATABASE app1; CREATE DATABASE app2")
            .await
            .unwrap();
        let err = admin
            .batch_execute("CREATE DATABASE app1")
            .await
            .unwrap_err();
        assert_eq!(err.code().map(|c| c.code()), Some("42P04"));
        let names: Vec<String> = admin
            .query("SELECT datname::text FROM pg_database ORDER BY 1", &[])
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
        assert_eq!(
            names,
            ["app1", "app2", "postgres", "template0", "template1"]
        );

        {
            let a = s.connect("app1").await;
            a.batch_execute("CREATE TABLE k (v text); INSERT INTO k VALUES ('in app1')")
                .await
                .unwrap();
            // Other sessions keep DROP DATABASE out, unless FORCE.
            let err = admin.batch_execute("DROP DATABASE app1").await.unwrap_err();
            assert_eq!(err.code().map(|c| c.code()), Some("55006"));
        }
        // Once app1's session is gone it gets closed, then reopened on demand with its data.
        let deadline = Instant::now() + Duration::from_secs(5);
        while s.engine.open_databases() > 1 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(s.engine.open_databases(), 1, "only postgres stays open");
        let a = s.connect("app1").await;
        assert_eq!(
            a.query_one("SELECT v FROM k", &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            "in app1"
        );
        let b = s.connect("app2").await;
        assert_eq!(
            b.query("SELECT 1 FROM pg_class WHERE relname = 'k'", &[])
                .await
                .unwrap()
                .len(),
            0
        );

        // A database made from another one starts as a copy of it.
        drop(a);
        tokio::time::sleep(Duration::from_millis(100)).await;
        admin
            .batch_execute("CREATE DATABASE app3 TEMPLATE app1")
            .await
            .unwrap();
        let c3 = s.connect("app3").await;
        assert_eq!(
            c3.query_one("SELECT v FROM k", &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            "in app1"
        );

        admin
            .batch_execute("ALTER DATABASE app2 RENAME TO app2b")
            .await
            .unwrap_err();
        drop(b);
        tokio::time::sleep(Duration::from_millis(100)).await;
        admin
            .batch_execute("ALTER DATABASE app2 RENAME TO app2b")
            .await
            .unwrap();
        admin
            .batch_execute("DROP DATABASE app3 WITH (FORCE)")
            .await
            .unwrap();
        let err = c3.simple_query("SELECT 1").await.unwrap_err();
        assert!(err.is_closed() || err.code().is_some(), "{err:?}");
        let names: Vec<String> = admin
            .query(
                "SELECT datname::text FROM pg_database WHERE datname LIKE 'app%' ORDER BY 1",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
        assert_eq!(names, ["app1", "app2b"]);
        let err = admin
            .batch_execute("DROP DATABASE postgres")
            .await
            .unwrap_err();
        assert_eq!(err.code().map(|c| c.code()), Some("55006"));
    });
    s.engine.shutdown();
}

#[test]
fn roles_are_cluster_wide() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let admin = s.connect("postgres").await;
        for q in ["CREATE ROLE gx_r LOGIN PASSWORD 'pw'", "CREATE DATABASE gx_db2 OWNER gx_r", "COMMENT ON ROLE gx_r IS 'shared note'", "GRANT CONNECT ON DATABASE postgres TO gx_r", "COMMENT ON DATABASE gx_db2 IS 'db note'"] {
            admin.batch_execute(q).await.unwrap();
        }
        // A role made in one database exists in every other one, and can log in to them.
        let other = s.connect("gx_db2").await;
        other
            .batch_execute(
                "CREATE TABLE gx_t (a int); ALTER TABLE gx_t OWNER TO gx_r; CREATE TABLE gx_t2 (); \
                 GRANT SELECT ON gx_t2 TO gx_r; ALTER ROLE gx_r IN DATABASE gx_db2 SET work_mem = '8MB'",
            )
            .await
            .unwrap();
        let row = other
            .query_one("SELECT rolname::text, description FROM pg_roles r LEFT JOIN pg_shdescription d ON d.objoid = r.oid WHERE rolname = 'gx_r'", &[])
            .await
            .unwrap();
        assert_eq!((row.get::<_, String>(0), row.get::<_, String>(1)), ("gx_r".into(), "shared note".into()));
        let note: String = other.query_one("SELECT shobj_description(oid, 'pg_database') FROM pg_database WHERE datname = current_database()", &[]).await.unwrap().get(0);
        assert_eq!(note, "db note");
        let (c, conn) = s.config("gx_db2", "pw").user("gx_r").connect(NoTls).await.expect("gx_r connects");
        tokio::spawn(conn);
        let row = c.query_one("SELECT current_user::text, current_setting('work_mem')", &[]).await.unwrap();
        assert_eq!((row.get::<_, String>(0), row.get::<_, String>(1)), ("gx_r".into(), "8MB".into()));
        drop(c);

        admin.batch_execute("CREATE TABLE gx_t3 (a int); ALTER TABLE gx_t3 OWNER TO gx_r").await.unwrap();
        let err = admin.batch_execute("DROP ROLE gx_r").await.unwrap_err();
        let db = err.as_db_error().expect("db error");
        assert_eq!(db.message(), "role \"gx_r\" cannot be dropped because some objects depend on it");
        assert_eq!(db.detail(), Some("privileges for database postgres\nowner of database gx_db2\nowner of table gx_t3\n2 objects in database gx_db2"));
        let rows: Vec<(String, String, String)> = admin
            .query(
                "SELECT CASE WHEN dbid = 0 THEN '-' ELSE (SELECT datname::text FROM pg_database WHERE oid = dbid) END, classid::regclass::text, deptype::text \
                 FROM pg_shdepend WHERE refobjid = 'gx_r'::regrole ORDER BY 1, 2, 3",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2)))
            .collect();
        let want = [("-", "pg_database", "a"), ("-", "pg_database", "o"), ("gx_db2", "pg_class", "a"), ("gx_db2", "pg_class", "o"), ("postgres", "pg_class", "o")];
        assert_eq!(rows, want.map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string())));

        let setting = "SELECT d.datname::text, s.setconfig FROM pg_db_role_setting s JOIN pg_database d ON d.oid = s.setdatabase WHERE setrole = 'gx_r'::regrole";
        let row = admin.query_one(setting, &[]).await.unwrap();
        assert_eq!((row.get::<_, String>(0), row.get::<_, Vec<String>>(1)), ("gx_db2".into(), vec!["work_mem=8MB".to_string()]));
        drop(other);
        tokio::time::sleep(Duration::from_millis(100)).await;
        admin.batch_execute("ALTER DATABASE gx_db2 RENAME TO gx_db3").await.unwrap();
        let row = admin.query_one(setting, &[]).await.unwrap();
        assert_eq!(row.get::<_, String>(0), "gx_db3");
        admin.batch_execute("DROP TABLE gx_t3").await.unwrap();
        admin.batch_execute("DROP DATABASE gx_db3").await.unwrap();
        let n: i64 = admin.query_one("SELECT count(*) FROM pg_db_role_setting WHERE setrole = 'gx_r'::regrole", &[]).await.unwrap().get(0);
        assert_eq!(n, 0);
        let n: i64 = admin.query_one("SELECT count(*) FROM pg_shdescription WHERE description = 'db note'", &[]).await.unwrap().get(0);
        assert_eq!(n, 0);
        let err = admin.batch_execute("DROP ROLE gx_r").await.unwrap_err();
        assert_eq!(err.as_db_error().and_then(|d| d.detail()), Some("privileges for database postgres"));
        admin.batch_execute("REVOKE CONNECT ON DATABASE postgres FROM gx_r; DROP ROLE gx_r").await.unwrap();

        // Renaming a role that owns a database keeps it the owner.
        for q in ["CREATE ROLE gx_o", "CREATE DATABASE gx_db4 OWNER gx_o", "ALTER ROLE gx_o RENAME TO gx_o2"] {
            admin.batch_execute(q).await.unwrap();
        }
        let owner: String = admin.query_one("SELECT pg_get_userbyid(datdba)::text FROM pg_database WHERE datname = 'gx_db4'", &[]).await.unwrap().get(0);
        assert_eq!(owner, "gx_o2");
        // New objects never reuse an oid another database has.
        let a: u32 = admin.query_one("SELECT 'gx_o2'::regrole::oid", &[]).await.unwrap().get(0);
        let c4 = s.connect("gx_db4").await;
        c4.batch_execute("CREATE TABLE gx_k2 ()").await.unwrap();
        let b: u32 = c4.query_one("SELECT 'gx_k2'::regclass::oid", &[]).await.unwrap().get(0);
        assert!(b > a, "{b} > {a}");

        // Role changes from two databases at once both stay.
        c4.batch_execute("BEGIN; CREATE ROLE gx_m1; CREATE TABLE gx_k3 ()").await.unwrap();
        admin.batch_execute("CREATE ROLE gx_m2").await.unwrap();
        c4.batch_execute("COMMIT").await.unwrap();
        for c in [&admin, &c4] {
            let n: i64 = c.query_one("SELECT count(*) FROM pg_roles WHERE rolname IN ('gx_m1', 'gx_m2')", &[]).await.unwrap().get(0);
            assert_eq!(n, 2);
        }
        // So does a table made while another database changed roles.
        c4.batch_execute("BEGIN; CREATE TABLE gx_k4 ()").await.unwrap();
        admin.batch_execute("DROP ROLE gx_m2").await.unwrap();
        c4.batch_execute("COMMIT").await.unwrap();
        let n: i64 = c4.query_one("SELECT count(*) FROM pg_roles WHERE rolname IN ('gx_m1', 'gx_m2')", &[]).await.unwrap().get(0);
        assert_eq!(n, 1);
        c4.batch_execute("BEGIN; CREATE ROLE gx_dup").await.unwrap();
        admin.batch_execute("CREATE ROLE gx_dup").await.unwrap();
        let err = c4.batch_execute("COMMIT").await.unwrap_err();
        assert_eq!(err.code().map(|c| c.code()), Some("23505"));
    });
    s.engine.shutdown();
}

#[test]
fn listen_notify() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        // The listener's connection task forwards its notifications.
        let (listener, mut conn) = s.config("postgres", "secret").connect(NoTls).await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut messages = futures::stream::poll_fn(move |cx| conn.poll_message(cx));
            while let Some(Ok(m)) = messages.next().await {
                if let AsyncMessage::Notification(n) = m {
                    let _ = tx.send((n.channel().to_string(), n.payload().to_string(), n.process_id()));
                }
            }
        });
        type Note = (String, String, i32);
        async fn next(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Note>) -> Note {
            tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("a notification").unwrap()
        }
        let listener_pid: i32 = listener.query_one("SELECT pg_backend_pid()", &[]).await.unwrap().get(0);
        listener.batch_execute("LISTEN jobs; LISTEN other").await.unwrap();
        let notifier = s.connect("postgres").await;
        let pid: i32 = notifier.query_one("SELECT pg_backend_pid()", &[]).await.unwrap().get(0);

        // The listener is idle: the notification arrives without it sending anything.
        notifier.batch_execute("NOTIFY jobs, 'one'").await.unwrap();
        assert_eq!(next(&mut rx).await, ("jobs".into(), "one".into(), pid));

        // Only committed transactions notify; duplicates collapse; other channels aren't heard.
        notifier.batch_execute("BEGIN; NOTIFY jobs, 'lost'; ROLLBACK").await.unwrap();
        notifier
            .batch_execute("BEGIN; NOTIFY jobs, 'a'; NOTIFY jobs, 'a'; SELECT pg_notify('other', 'b'); NOTIFY unheard, 'c'; SAVEPOINT s; NOTIFY jobs, 'undone'; ROLLBACK TO s; COMMIT")
            .await
            .unwrap();
        assert_eq!(next(&mut rx).await, ("jobs".into(), "a".into(), pid));
        assert_eq!(next(&mut rx).await, ("other".into(), "b".into(), pid));

        // A listener in a transaction block gets them once it ends.
        listener.batch_execute("BEGIN").await.unwrap();
        notifier.batch_execute("NOTIFY jobs, 'later'").await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(rx.try_recv().is_err(), "nothing inside the listener's transaction");
        listener.batch_execute("COMMIT").await.unwrap();
        assert_eq!(next(&mut rx).await, ("jobs".into(), "later".into(), pid));

        // Its own notifications, from a procedure that commits; UNLISTEN stops a channel.
        listener
            .batch_execute("CREATE PROCEDURE ping() LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_notify('jobs', 'proc'); COMMIT; END $$")
            .await
            .unwrap();
        listener.batch_execute("CALL ping()").await.unwrap();
        assert_eq!(next(&mut rx).await, ("jobs".into(), "proc".into(), listener_pid));
        listener.batch_execute("UNLISTEN jobs").await.unwrap();
        notifier.batch_execute("NOTIFY jobs, 'gone'; NOTIFY other, 'still'").await.unwrap();
        assert_eq!(next(&mut rx).await, ("other".into(), "still".into(), pid));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(rx.try_recv().is_err());
    });
    s.engine.shutdown();
}

/// Any value's binary form, as sent or received.
#[derive(Debug, PartialEq)]
struct Raw(Vec<u8>);

impl<'a> tokio_postgres::types::FromSql<'a> for Raw {
    fn from_sql(
        _: &tokio_postgres::types::Type,
        raw: &'a [u8],
    ) -> Result<Raw, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Raw(raw.to_vec()))
    }

    fn accepts(_: &tokio_postgres::types::Type) -> bool {
        true
    }
}

impl tokio_postgres::types::ToSql for Raw {
    fn to_sql(
        &self,
        _: &tokio_postgres::types::Type,
        out: &mut bytes::BytesMut,
    ) -> Result<tokio_postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        out.extend_from_slice(&self.0);
        Ok(tokio_postgres::types::IsNull::No)
    }

    fn accepts(_: &tokio_postgres::types::Type) -> bool {
        true
    }

    tokio_postgres::types::to_sql_checked!();
}

fn hex(s: &str) -> Raw {
    Raw((0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect())
}

#[test]
fn range_binary_format() {
    let s = server(AuthMethod::Trust);
    runtime().block_on(async {
        let c = s.connect("postgres").await;
        // range_send / multirange_send, as Postgres 18 produces them.
        let r = c
            .query_one(
                "SELECT int4range(1,5), '{[1,3),[5,7)}'::int4multirange, 'empty'::int4range, numrange(NULL, 2.5, '(]'), tstzrange('2020-01-01 00:00+00', NULL)",
                &[],
            )
            .await
            .unwrap();
        let want = [
            "0200000004000000010000000400000005",
            "00000002000000110200000004000000010000000400000003000000110200000004000000050000000400000007",
            "01",
            "0c0000000c000200000000000100021388",
            "120000000800023e0786c26000",
        ];
        for (i, w) in want.iter().enumerate() {
            assert_eq!(r.get::<_, Raw>(i), hex(w), "column {i}");
        }
        // range_recv: bound parameters in binary, canonicalised and checked.
        let st = c.prepare("SELECT $1::int4range::text, $2::int4multirange::text, $3::numrange::text").await.unwrap();
        let r = c
            .query_one(&st, &[&hex("0600000004000000010000000400000005"), &hex("00000002000000110200000004000000050000000400000007000000010100"), &hex("0c0000000c000200000000000100021388")])
            .await
            .unwrap();
        assert_eq!(r.get::<_, String>(0), "[1,6)");
        assert_eq!(r.get::<_, String>(1), "{[5,7)}");
        assert_eq!(r.get::<_, String>(2), "(,2.5]");
        let err = c.query_one(&st, &[&hex("0200000004000000050000000400000001"), &hex("00000000"), &hex("01")]).await.unwrap_err();
        assert_eq!(err.code().map(|c| c.code()), Some("22000"));
    });
    s.engine.shutdown();
}

async fn heap_positions(c: &Client) -> Vec<(i64, i32)> {
    c.query(
        "SELECT n::int8, id FROM (SELECT id, row_number() OVER () n FROM t) s \
         WHERE id IN (-1, 0, 200, 999) ORDER BY n",
        &[],
    )
    .await
    .unwrap()
    .iter()
    .map(|r| (r.get::<_, i64>(0), r.get::<_, i32>(1)))
    .collect()
}

/// Rows' physical order (what a sequential scan returns) as Postgres has it: each session
/// keeps its own insertion target page, as backends do; a database closed and reopened
/// lays its heaps out again in the order of the TIDs the rows were written with.
#[test]
fn heap_order_sessions_and_reopen() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let admin = s.connect("postgres").await;
        admin.batch_execute("CREATE DATABASE heapord").await.unwrap();
        {
            let a = s.connect("heapord").await;
            let b = s.connect("heapord").await;
            a.batch_execute("CREATE TABLE t (id int, pad text); INSERT INTO t VALUES (0, 'a')")
                .await
                .unwrap();
            b.batch_execute(
                "INSERT INTO t SELECT g, repeat('x', 100) FROM generate_series(1, 200) g",
            )
            .await
            .unwrap();
            // A's target page (0) is full: the free space map sends it to page 1's
            // leftover room; B's target is still the last page.
            a.batch_execute("INSERT INTO t VALUES (-1, 'a')").await.unwrap();
            b.batch_execute("INSERT INTO t VALUES (999, 'b')").await.unwrap();
            // (Postgres 18: (0,1), (1,59), (3,27); 4 pages.)
            assert_eq!(
                heap_positions(&a).await,
                [(1, 0), (118, -1), (202, 200), (203, 999)]
            );
            let size: i64 = a
                .query_one("SELECT pg_relation_size('t')", &[])
                .await
                .unwrap()
                .get(0);
            assert_eq!(size, 32768);
            a.batch_execute(
                "CREATE TABLE p (id int PRIMARY KEY, v text); \
                 INSERT INTO p VALUES (5, 'a'), (1, 'b'), (3, 'c'), (2, 'd'); \
                 UPDATE p SET v = 'x' WHERE id = 1",
            )
            .await
            .unwrap();
        }
        // Closed once its sessions are gone, then reopened: the heap is laid out afresh.
        let deadline = Instant::now() + Duration::from_secs(5);
        while s.engine.open_databases() > 1 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(s.engine.open_databases(), 1);
        let c = s.connect("heapord").await;
        assert_eq!(
            heap_positions(&c).await,
            [(1, 0), (118, -1), (202, 200), (203, 999)]
        );
        let ids: Vec<i32> = c
            .query("SELECT id FROM p", &[])
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
        assert_eq!(ids, [5, 3, 2, 1]);
    });
    s.engine.shutdown();
}

/// Sessions connecting to and leaving the same and different databases while the idle
/// closer runs flat out and other databases are created, renamed and dropped: every
/// database closes and reopens under them (a store must not be opened while its old one
/// is still closing), nothing they wrote is lost, and template0 always refuses with 55000.
#[test]
fn concurrent_connects_and_closes() {
    use std::sync::atomic::{AtomicBool, Ordering};
    const DBS: [&str; 3] = ["cc1", "cc2", "cc3"];
    const WORKERS: usize = 6;
    const ROUNDS: usize = 30;
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let admin = s.connect("postgres").await;
        for d in DBS {
            admin.batch_execute(&format!("CREATE DATABASE {d}")).await.unwrap();
            let c = s.connect(d).await;
            c.batch_execute("CREATE TABLE t (w int, i int)").await.unwrap();
        }
    });
    let stop = Arc::new(AtomicBool::new(false));
    let closer = {
        let (e, stop) = (s.engine.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                e.close_idle(Duration::ZERO);
                std::thread::yield_now();
            }
        })
    };
    let s = &s;
    std::thread::scope(|sc| {
        for w in 0..WORKERS {
            sc.spawn(move || {
                runtime().block_on(async move {
                    for i in 0..ROUNDS {
                        // Two workers share each database at a time; each moves on to the next.
                        let db = DBS[(w + i) % DBS.len()];
                        let c = s.connect(db).await;
                        c.execute("INSERT INTO t VALUES ($1, $2)", &[&(w as i32), &(i as i32)])
                            .await
                            .unwrap();
                        drop(c);
                        if i % 4 == w % 4 {
                            let err = s
                                .config("template0", "secret")
                                .connect(NoTls)
                                .await
                                .err()
                                .expect("template0 refuses connections");
                            assert_eq!(err.code().map(|c| c.code()), Some("55000"), "{err:?}");
                        }
                    }
                })
            });
        }
        // Databases coming and going beside them.
        sc.spawn(move || {
            runtime().block_on(async move {
                let admin = s.connect("postgres").await;
                for n in 0..8 {
                    admin.batch_execute(&format!("CREATE DATABASE churn{n}")).await.unwrap();
                    {
                        let c = s.connect(&format!("churn{n}")).await;
                        c.batch_execute("CREATE TABLE k AS SELECT 1 AS v").await.unwrap();
                    }
                    // Its session may not have ended yet.
                    let deadline = Instant::now() + Duration::from_secs(5);
                    loop {
                        match admin
                            .batch_execute(&format!("ALTER DATABASE churn{n} RENAME TO churned{n}"))
                            .await
                        {
                            Ok(()) => break,
                            Err(e) if e.code().map(|c| c.code()) == Some("55006")
                                && Instant::now() < deadline =>
                            {
                                tokio::time::sleep(Duration::from_millis(5)).await;
                            }
                            Err(e) => panic!("rename: {e:?}"),
                        }
                    }
                    let c = s.connect(&format!("churned{n}")).await;
                    let v: i32 = c.query_one("SELECT v FROM k", &[]).await.unwrap().get(0);
                    assert_eq!(v, 1);
                    drop(c);
                    admin
                        .batch_execute(&format!("DROP DATABASE churned{n} WITH (FORCE)"))
                        .await
                        .unwrap();
                }
            })
        });
    });
    stop.store(true, Ordering::Relaxed);
    closer.join().unwrap();
    runtime().block_on(async {
        let mut total = 0;
        for (d, db) in DBS.iter().enumerate() {
            let c = s.connect(db).await;
            let mut got: Vec<(i32, i32)> = c
                .query("SELECT w, i FROM t", &[])
                .await
                .unwrap()
                .iter()
                .map(|r| (r.get(0), r.get(1)))
                .collect();
            got.sort();
            let mut want: Vec<(i32, i32)> = (0..WORKERS)
                .flat_map(|w| (0..ROUNDS).map(move |i| (w, i)))
                .filter(|(w, i)| (w + i) % DBS.len() == d)
                .map(|(w, i)| (w as i32, i as i32))
                .collect();
            want.sort();
            assert_eq!(got, want, "rows of {db}");
            total += got.len();
        }
        assert_eq!(total, WORKERS * ROUNDS);
        let admin = s.connect("postgres").await;
        let n: i64 = admin
            .query_one("SELECT count(*) FROM pg_database WHERE datname LIKE 'churn%'", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(n, 0);
    });
    s.engine.shutdown();
}

/// The rows of a simple query's last result, each as its columns joined by `|`.
async fn lines(c: &Client, sql: &str) -> Result<Vec<String>, tokio_postgres::Error> {
    let mut out = Vec::new();
    for m in c.simple_query(sql).await? {
        if let SimpleQueryMessage::Row(r) = m {
            out.push((0..r.len()).map(|i| r.get(i).unwrap_or("NULL")).collect::<Vec<_>>().join("|"));
        }
    }
    Ok(out)
}

/// SELECT ... FOR UPDATE / SHARE locks the rows its query level returns once they are
/// joined, filtered, sorted and limited (Postgres's LockRows at the level's top), not every
/// row its scans read; a row a concurrent update changed is rechecked against the whole
/// level (EvalPlanQual), joins included.
#[test]
fn row_locks_at_the_query_level_top() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        let b = s.connect("postgres").await;
        a.batch_execute(
            "CREATE TABLE lq (id int PRIMARY KEY, x int, note text);
             CREATE TABLE lr (id int PRIMARY KEY, y int);
             INSERT INTO lq SELECT g, g, 'n' || g FROM generate_series(1, 10) g;
             INSERT INTO lr SELECT g, g FROM generate_series(1, 5) g;",
        )
        .await
        .unwrap();
        // A job queue: each worker takes the first row no one holds.
        let take = "SELECT id FROM lq ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED";
        a.batch_execute("BEGIN").await.unwrap();
        b.batch_execute("BEGIN").await.unwrap();
        assert_eq!(lines(&a, take).await.unwrap(), ["1"]);
        assert_eq!(lines(&b, take).await.unwrap(), ["2"]);
        assert_eq!(lines(&a, take).await.unwrap(), ["1"]);
        b.batch_execute("ROLLBACK").await.unwrap();
        a.batch_execute("ROLLBACK").await.unwrap();
        // Over a join: only the rows that join are locked, and only the named table's.
        a.batch_execute("BEGIN").await.unwrap();
        assert_eq!(
            lines(&a, "SELECT lq.id FROM lq JOIN lr ON lq.x = lr.y WHERE lq.id > 2 ORDER BY 1 FOR UPDATE OF lq")
                .await
                .unwrap(),
            ["3", "4", "5"]
        );
        assert_eq!(
            lines(&b, "SELECT id FROM lq WHERE id IN (6, 9) FOR UPDATE NOWAIT").await.unwrap(),
            ["6", "9"]
        );
        assert_eq!(lines(&b, "SELECT id FROM lr WHERE id = 4 FOR UPDATE NOWAIT").await.unwrap(), ["4"]);
        let err = lines(&b, "SELECT id FROM lq WHERE id = 4 FOR UPDATE NOWAIT").await.unwrap_err();
        assert_eq!(err.code().map(|c| c.code()), Some("55P03"));
        a.batch_execute("ROLLBACK").await.unwrap();
        // EvalPlanQual: the locked row changed while waiting for its lock; the join is
        // checked again over its new version (row 2 no longer joins, row 3's note is new).
        a.batch_execute(
            "BEGIN; UPDATE lq SET x = 100 WHERE id = 2; UPDATE lq SET note = 'changed' WHERE id = 3",
        )
        .await
        .unwrap();
        let waiter = tokio::spawn(async move {
            let r = lines(
                &b,
                "SELECT lq.id, lq.note, lr.id FROM lq JOIN lr ON lq.x = lr.y WHERE lq.id IN (2, 3) ORDER BY 1 FOR UPDATE OF lq",
            )
            .await;
            (b, r)
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        a.batch_execute("COMMIT").await.unwrap();
        let (_b, r) = waiter.await.unwrap();
        assert_eq!(r.unwrap(), ["3|changed|3"]);
    });
    s.engine.shutdown();
}

/// FOR UPDATE / SHARE with no relation list, or naming a FROM subquery or a view, reaches
/// the relations inside it (transformLockingClause's pushed-down marks): a subquery planned
/// apart locks every row it returns (its own LockRows), one pulled up only the rows the
/// statement returns; a set operation can't be locked, a VALUES list has nothing to lock.
#[test]
fn row_locks_pushed_into_subqueries_and_views() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        let b = s.connect("postgres").await;
        a.batch_execute(
            "CREATE TABLE pp (f1 int, f2 text);
             CREATE TABLE pj (f2j text, other int);
             INSERT INTO pp VALUES (1, 'one');
             INSERT INTO pj VALUES ('one', 1), ('two', 2);
             CREATE VIEW pv AS SELECT * FROM pj WHERE other > 1;
             CREATE VIEW pva AS SELECT f2j, count(*) FROM pj GROUP BY f2j;",
        )
        .await
        .unwrap();
        let held = |row: &'static str| format!("SELECT f2j FROM pj WHERE f2j = '{row}' FOR UPDATE NOWAIT");
        // (Each case: what session a's statement leaves locked, of pj's two rows.)
        let cases: [(&str, [bool; 2]); 6] = [
            ("SELECT p.f1 FROM pp p, (SELECT * FROM pj OFFSET 0) s WHERE p.f2 = s.f2j FOR SHARE", [true, true]),
            ("SELECT p.f1 FROM pp p, (SELECT * FROM pj OFFSET 0) s WHERE p.f2 = s.f2j FOR SHARE OF p", [false, false]),
            ("SELECT * FROM (SELECT * FROM pj OFFSET 0) s FOR UPDATE OF s", [true, true]),
            ("SELECT * FROM (SELECT * FROM pj) s WHERE f2j = 'two' FOR UPDATE", [false, true]),
            ("SELECT * FROM pv FOR UPDATE", [false, true]),
            ("SELECT * FROM pp p, pv v FOR UPDATE OF p", [false, false]),
        ];
        for (q, locked) in cases {
            a.batch_execute("BEGIN").await.unwrap();
            lines(&a, q).await.unwrap();
            for (row, want) in ["one", "two"].into_iter().zip(locked) {
                let r = lines(&b, &held(row)).await;
                match r {
                    Ok(_) => assert!(!want, "{q}: row {row} not locked"),
                    Err(e) => {
                        assert!(want, "{q}: row {row} locked");
                        assert_eq!(e.code().map(|c| c.code()), Some("55P03"));
                    }
                }
            }
            a.batch_execute("ROLLBACK").await.unwrap();
        }
        for (q, msg) in [
            ("SELECT * FROM (SELECT count(*) FROM pj) s FOR UPDATE", "FOR UPDATE is not allowed with aggregate functions"),
            ("SELECT * FROM (SELECT f2j FROM pj UNION SELECT 'x') s FOR SHARE", "FOR SHARE is not allowed with UNION/INTERSECT/EXCEPT"),
            ("SELECT * FROM pva FOR UPDATE", "FOR UPDATE is not allowed with GROUP BY clause"),
            ("SELECT * FROM (SELECT * FROM (SELECT count(*) c FROM pj) a) b FOR UPDATE OF b", "FOR UPDATE is not allowed with aggregate functions"),
        ] {
            let e = lines(&a, q).await.unwrap_err();
            assert_eq!(e.as_db_error().map(|d| d.message()), Some(msg), "{q}");
        }
        assert_eq!(lines(&a, "SELECT v.column1, f1 FROM (VALUES (1)) v, pp FOR UPDATE").await.unwrap(), ["1|1"]);
        assert_eq!(
            lines(&a, "SELECT * FROM (SELECT f2j FROM pj UNION SELECT 'x') s, pp p WHERE p.f2 = s.f2j FOR SHARE OF p").await.unwrap(),
            ["one|1|one"]
        );
    });
    s.engine.shutdown();
}

/// Several locking clauses of one level (transformLockingClause per clause,
/// applyLockingClause): each relation takes the strongest strength and the strictest wait
/// policy of the clauses naming it (or naming none), not only the last clause's.
#[test]
fn several_locking_clauses() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        let b = s.connect("postgres").await;
        a.batch_execute(
            "CREATE TABLE lc1 (a int, b text);
             CREATE TABLE lc3 (p int, q int);
             INSERT INTO lc1 VALUES (1, 'x'), (2, 'y');
             INSERT INTO lc3 VALUES (1, 1), (2, 2);",
        )
        .await
        .unwrap();
        // (Each case: session a's statement, then b's NOWAIT probes and whether each fails.)
        let cases: [(&str, [(&str, bool); 3]); 3] = [
            (
                "SELECT * FROM lc1, lc3 WHERE lc3.p = lc1.a FOR UPDATE OF lc1 FOR SHARE OF lc3",
                [
                    ("SELECT * FROM lc1 WHERE a = 1 FOR UPDATE NOWAIT", true),
                    ("SELECT * FROM lc3 WHERE p = 1 FOR UPDATE NOWAIT", true),
                    ("SELECT * FROM lc3 WHERE p = 1 FOR SHARE NOWAIT", false),
                ],
            ),
            (
                "SELECT * FROM lc1, lc3 WHERE lc3.p = lc1.a FOR SHARE OF lc1 FOR UPDATE OF lc1",
                [
                    ("SELECT * FROM lc1 WHERE a = 1 FOR KEY SHARE NOWAIT", true),
                    ("SELECT * FROM lc3 WHERE p = 1 FOR UPDATE NOWAIT", false),
                    ("SELECT * FROM lc1 WHERE a = 2 FOR KEY SHARE NOWAIT", true),
                ],
            ),
            (
                "SELECT * FROM lc1, lc3 WHERE lc3.p = lc1.a FOR SHARE FOR UPDATE OF lc3",
                [
                    ("SELECT * FROM lc1 WHERE a = 1 FOR SHARE NOWAIT", false),
                    ("SELECT * FROM lc1 WHERE a = 1 FOR UPDATE NOWAIT", true),
                    ("SELECT * FROM lc3 WHERE p = 1 FOR KEY SHARE NOWAIT", true),
                ],
            ),
        ];
        for (q, probes) in cases {
            a.batch_execute("BEGIN").await.unwrap();
            lines(&a, q).await.unwrap();
            for (probe, fails) in probes {
                b.batch_execute("BEGIN").await.unwrap();
                match lines(&b, probe).await {
                    Ok(_) => assert!(!fails, "{q}: {probe} got the lock"),
                    Err(e) => {
                        assert!(fails, "{q}: {probe}: {e}");
                        assert_eq!(e.code().map(|c| c.code()), Some("55P03"));
                    }
                }
                b.batch_execute("ROLLBACK").await.unwrap();
            }
            a.batch_execute("ROLLBACK").await.unwrap();
        }
        // The strictest wait policy: NOWAIT over SKIP LOCKED, of one clause or of one pushed
        // down from the level above.
        b.batch_execute("BEGIN").await.unwrap();
        lines(&b, "SELECT * FROM lc1 WHERE a = 1 FOR UPDATE").await.unwrap();
        assert_eq!(lines(&a, "SELECT count(*) FROM (SELECT * FROM lc1 FOR UPDATE SKIP LOCKED) z").await.unwrap(), ["1"]);
        for q in [
            "SELECT count(*) FROM (SELECT * FROM lc1 FOR UPDATE SKIP LOCKED FOR UPDATE NOWAIT) z",
            "SELECT count(*) FROM (SELECT * FROM (SELECT * FROM lc1 FOR UPDATE SKIP LOCKED) s FOR UPDATE OF s NOWAIT) z",
        ] {
            let e = lines(&a, q).await.unwrap_err();
            assert_eq!(e.code().map(|c| c.code()), Some("55P03"), "{q}");
        }
        b.batch_execute("ROLLBACK").await.unwrap();
    });
    s.engine.shutdown();
}
