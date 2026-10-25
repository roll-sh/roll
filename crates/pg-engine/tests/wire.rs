//! The wire protocol server, driven by a real client (tokio-postgres): SCRAM, the extended
//! protocol with binary parameters and results, portals, COPY, cancel and terminate,
//! databases opening and closing, LISTEN / NOTIFY between connections.

use futures::{SinkExt, StreamExt};
use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::server::{AuthMethod, ServerOptions};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio_postgres::{AsyncMessage, Client, NoTls, SimpleQueryMessage};

#[path = "common/raw.rs"]
mod raw;

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
        shared_preload_libraries: Vec::new(),
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
        // The same object changed by both: the second waits for the first's lock, then finds
        // no t2 by that name any more (Postgres's RangeVarGetRelidExtended).
        a.batch_execute("BEGIN; ALTER TABLE t2 RENAME TO t3").await.unwrap();
        b.batch_execute("BEGIN").await.unwrap();
        let (r, ()) = tokio::join!(b.batch_execute("ALTER TABLE t2 RENAME TO t4"), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            a.batch_execute("COMMIT").await.unwrap();
        });
        assert_eq!(code(r.unwrap_err()).as_deref(), Some("42P01"));
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

/// ALTER DATABASE ... SET / RESET: pg_db_role_setting (setrole 0), and every new session
/// there starts with it (a role's own settings in it win).
#[test]
fn database_settings() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        a.batch_execute(
            "CREATE DATABASE setdb;
             ALTER DATABASE setdb SET work_mem = '2MB';
             ALTER DATABASE setdb SET search_path = a, \"b c\";",
        )
        .await
        .unwrap();
        let conf: Vec<String> = a
            .query_one(
                "SELECT setconfig FROM pg_db_role_setting s JOIN pg_database d
                   ON d.oid = s.setdatabase WHERE d.datname = 'setdb' AND s.setrole = 0",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(conf, ["work_mem=2MB", "search_path=a, \"b c\""]);
        {
            let c = s.connect("setdb").await;
            let row = c.query_one("SELECT current_setting('work_mem'), current_setting('search_path')", &[]).await.unwrap();
            assert_eq!((row.get::<_, String>(0), row.get::<_, String>(1)), ("2MB".into(), "a, \"b c\"".into()));
        }
        a.batch_execute("ALTER ROLE postgres IN DATABASE setdb SET work_mem = '3MB'; ALTER DATABASE setdb RESET search_path")
            .await
            .unwrap();
        {
            let c = s.connect("setdb").await;
            let row = c.query_one("SELECT current_setting('work_mem'), current_setting('search_path')", &[]).await.unwrap();
            assert_eq!((row.get::<_, String>(0), row.get::<_, String>(1)), ("3MB".into(), "\"$user\", public".into()));
        }
        let e = a.batch_execute("ALTER DATABASE setdb SET work_mem = 'lots'").await.unwrap_err();
        assert_eq!(e.code().map(|c| c.code()), Some("22023"));
        a.batch_execute("ALTER DATABASE setdb RESET ALL; ALTER ROLE postgres IN DATABASE setdb RESET ALL")
            .await
            .unwrap();
        let n: i64 = a
            .query_one(
                "SELECT count(*) FROM pg_db_role_setting s JOIN pg_database d ON d.oid = s.setdatabase
                   WHERE d.datname = 'setdb'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(n, 0);
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

/// A connection idle for a while runs the server's idle hook (`server::set_idle_hook`: the
/// binary returns the thread's free memory there) and goes on as before: the wait for the
/// client's next message, and a message arriving in pieces, outlast the hook's timeout.
#[test]
fn idle_hook_runs_and_the_connection_goes_on() {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    pg_engine::server::set_idle_hook(|| {
        RUNS.fetch_add(1, Ordering::SeqCst);
    });
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let c = s.connect("postgres").await;
        c.simple_query("CREATE TABLE idle_t (a int)").await.unwrap();
        tokio::time::sleep(Duration::from_millis(4500)).await;
        assert!(RUNS.load(Ordering::SeqCst) >= 1, "the idle hook never ran");
        c.simple_query("INSERT INTO idle_t VALUES (1), (2)").await.unwrap();
        let rows = c.query("SELECT sum(a) FROM idle_t", &[]).await.unwrap();
        assert_eq!(rows[0].get::<_, i64>(0), 3);
    });
    // A startup message split by a pause longer than the hook's timeout (a slow client).
    let mut st = std::net::TcpStream::connect(&s.addr).unwrap();
    let mut params = Vec::new();
    for kv in ["user", "postgres", "database", "postgres"] {
        params.extend_from_slice(kv.as_bytes());
        params.push(0);
    }
    params.push(0);
    let mut msg = ((params.len() + 8) as i32).to_be_bytes().to_vec();
    msg.extend_from_slice(&196608i32.to_be_bytes());
    msg.extend_from_slice(&params);
    st.write_all(&msg[..5]).unwrap();
    std::thread::sleep(Duration::from_millis(2500));
    st.write_all(&msg[5..]).unwrap();
    // (SCRAM: the server asks for the password, in step with the client.)
    let mut b = [0u8; 1];
    st.read_exact(&mut b).unwrap();
    assert_eq!(b[0], b'R');
    s.engine.shutdown();
}

// ── what client libraries' own test suites found (tests/clients) ──

/// A cursor DECLARE made is a portal too (psycopg's server-side cursors describe it; JDBC
/// and others execute it): Describe, Execute with a row limit (suspending), Close.
#[test]
fn cursors_are_portals() {
    let s = server(AuthMethod::Trust);
    let mut c = raw::Raw::connect(&s.addr, "postgres");
    c.query("CREATE TABLE cp (a int); INSERT INTO cp SELECT generate_series(1, 5)");
    c.query("BEGIN");
    c.query("DECLARE \"foo\" CURSOR FOR SELECT a, a::text AS b FROM cp ORDER BY a");
    c.describe(b'P', "foo");
    assert_eq!(c.sync(), ["T a:23/-1 b:25/-1"]);
    c.execute("foo", 2);
    c.execute("foo", 2);
    c.execute("foo", 2);
    c.execute("foo", 0);
    c.close(b'P', "foo");
    assert_eq!(
        c.sync(),
        ["D 1|1", "D 2|2", "s", "D 3|3", "D 4|4", "s", "D 5|5", "C SELECT 1", "C SELECT 0"]
    );
    // Closed by the Close message.
    assert_eq!(c.query("FETCH 1 FROM foo"), ["E 34000"]);
    c.query("ROLLBACK");
    // An unknown portal's Describe aborts the transaction, as any error does.
    c.query("BEGIN");
    c.describe(b'P', "nope");
    assert_eq!(c.sync(), ["E 34000"]);
    assert_eq!(c.status, b'E');
    c.query("ROLLBACK");
    s.engine.shutdown();
}

/// exec_parse_message: a parameter nothing gives a type (an "any" argument, IS NULL, a ROW()
/// field) is refused; the select list and operators type theirs.
#[test]
fn parameters_left_without_a_type() {
    let s = server(AuthMethod::Trust);
    let mut c = raw::Raw::connect(&s.addr, "postgres");
    for (sql, want) in [
        ("SELECT concat($1, $2)", "E 42P18"),
        ("SELECT format($1, $2)", "E 42P18"),
        ("SELECT $1 IS NULL", "E 42P18"),
        ("SELECT row($1, $2)", "E 42P18"),
        ("SELECT pg_typeof($1)", "E 42P18"),
        ("SELECT $1", "T ?column?:25/-1"),
        ("SELECT coalesce($1, 'x')", "T coalesce:25/-1"),
        ("SELECT concat($1::int, 'a'), $1 IS NULL", "T concat:25/-1 ?column?:16/-1"),
    ] {
        c.parse("", sql);
        c.describe(b'S', "");
        assert_eq!(c.sync()[0], want, "{sql}");
    }
    // An array column's type modifier is its element's.
    c.parse("", "SELECT NULL::varchar(3)[], NULL::numeric(10,2)[]");
    c.describe(b'S', "");
    assert_eq!(c.sync(), ["T varchar:1015/7 numeric:1231/655366"]);
    s.engine.shutdown();
}

/// A failed transaction still parses and binds its end (a pipelining client's ROLLBACK), and
/// nothing else.
#[test]
fn failed_transaction_parses_its_end() {
    let s = server(AuthMethod::Trust);
    let mut c = raw::Raw::connect(&s.addr, "postgres");
    c.query("BEGIN");
    assert_eq!(c.query("SELECT 1/0"), ["E 22012"]);
    c.parse("", "SELECT 1");
    assert_eq!(c.sync(), ["E 25P02"]);
    c.parse("", "ROLLBACK");
    c.bind("", "");
    c.execute("", 0);
    assert_eq!(c.sync(), ["C ROLLBACK"]);
    assert_eq!(c.status, b'I');
    s.engine.shutdown();
}

/// Binary send / receive of the types that fell back to text (inet, cidr, timetz, macaddr,
/// bit, money, pg_lsn, tid, enum), parameters naming a user type's oid (composite, an array of
/// an enum, one made in the open transaction), and binary COPY's framing (the header goes out
/// with the first row, one CopyData per row).
#[test]
fn binary_formats_and_user_types() {
    use tokio_postgres::types::Type;
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let mut c = s.connect("postgres").await;
        let ip: std::net::IpAddr = "192.168.0.1".parse().unwrap();
        let back: std::net::IpAddr = c.query_one("SELECT $1::inet", &[&ip]).await.unwrap().get(0);
        assert_eq!(back, ip);
        let v6: std::net::IpAddr = "2001:db8::1".parse().unwrap();
        let row = c.query_one("SELECT $1::inet, host($1::inet)", &[&v6]).await.unwrap();
        assert_eq!((row.get::<_, std::net::IpAddr>(0), row.get::<_, String>(1)), (v6, "2001:db8::1".into()));
        // Binary results go through each type's send function: re-read through its receive.
        c.batch_execute(
            "CREATE TYPE mood AS ENUM ('sad', 'ok'); CREATE TYPE pair AS (a int, b text);
             CREATE TABLE bt (i inet, n cidr, t timetz, m macaddr, m8 macaddr8, b bit(3), v varbit,
               mo money, l pg_lsn, d tid, e mood, p pair);
             INSERT INTO bt VALUES ('10.1.2.3/8', '10.0.0.0/8', '03:04:05.6-02:30', '08:00:2b:01:02:03',
               '08:00:2b:01:02:03:04:05', B'101', B'11', 12.34, '16/B374D848', '(3,7)', 'ok', ROW(1, 'x'))",
        )
        .await
        .unwrap();
        let cols = "i, n, t, m, m8, b, v, mo, l, d, e, p";
        let want: Vec<String> = c.simple_query(&format!("SELECT {cols} FROM bt")).await.unwrap().iter()
            .filter_map(|m| match m { SimpleQueryMessage::Row(r) => Some((0..12).map(|i| r.get(i).unwrap().to_string()).collect::<Vec<_>>()), _ => None })
            .next()
            .unwrap();
        let mut copy = Vec::new();
        let stream = c.copy_out(&format!("COPY (SELECT {cols} FROM bt) TO STDOUT (FORMAT binary)")).await.unwrap();
        futures::pin_mut!(stream);
        while let Some(chunk) = stream.next().await {
            copy.push(chunk.unwrap());
        }
        // The signature and the one row in one message, the trailer in the next.
        assert_eq!(copy.len(), 2);
        assert!(copy[0].starts_with(b"PGCOPY\n\xff\r\n\0"));
        assert_eq!(&copy[1][..], b"\xff\xff");
        c.batch_execute("CREATE TABLE bt2 (LIKE bt)").await.unwrap();
        let sink = c.copy_in("COPY bt2 FROM STDIN (FORMAT binary)").await.unwrap();
        futures::pin_mut!(sink);
        for chunk in &copy {
            sink.send(chunk.clone()).await.unwrap();
        }
        assert_eq!(sink.finish().await.unwrap(), 1);
        let got: Vec<String> = c.simple_query(&format!("SELECT {cols} FROM bt2")).await.unwrap().iter()
            .filter_map(|m| match m { SimpleQueryMessage::Row(r) => Some((0..12).map(|i| r.get(i).unwrap().to_string()).collect::<Vec<_>>()), _ => None })
            .next()
            .unwrap();
        assert_eq!(got, want);

        // A parameter typed by a user type's oid: Parse resolves it (also in the transaction
        // that made it).
        let tx = c.transaction().await.unwrap();
        tx.batch_execute("CREATE TYPE pair2 AS (x int)").await.unwrap();
        let oid: u32 = tx.query_one("SELECT 'pair2'::regtype::oid", &[]).await.unwrap().get(0);
        let arr: u32 = tx.query_one("SELECT '_mood'::regtype::oid", &[]).await.unwrap().get(0);
        for o in [oid, arr] {
            let ty = Type::new("x".into(), o, tokio_postgres::types::Kind::Pseudo, "public".into());
            let st = tx.prepare_typed("SELECT $1::text", &[ty]).await.unwrap();
            assert_eq!(st.params()[0].oid(), o);
        }
        tx.rollback().await.unwrap();
    });
    s.engine.shutdown();
}

/// COPY TO's row errors come after CopyOutResponse (a row that can't be converted to the
/// client's encoding); the transaction characteristics a new transaction starts with; text
/// parameters arrive in the client's encoding.
#[test]
fn copy_errors_encodings_and_transaction_defaults() {
    let s = server(AuthMethod::Trust);
    let mut c = raw::Raw::connect(&s.addr, "postgres");
    c.query("SET client_encoding TO latin1");
    assert_eq!(c.query("COPY (SELECT 'a' UNION ALL SELECT chr(8364)) TO STDOUT"), ["H", "d", "E 22P05"]);
    // é in Latin-1, as a text parameter.
    c.send(b'P', b"\0SELECT $1::text = chr(233)\0\0\0");
    c.send(b'B', b"\0\0\0\0\0\x01\0\0\0\x01\xe9\0\0");
    c.execute("", 0);
    assert_eq!(c.sync(), ["D t", "C SELECT 1"]);
    c.query("RESET client_encoding");
    // A NUL byte is no valid text.
    c.send(b'P', b"\0SELECT $1::text\0\0\0");
    c.send(b'B', b"\0\0\0\0\0\x01\0\0\0\x03a\0b\0\0");
    assert_eq!(c.sync(), ["E 22021"]);

    c.query("SET default_transaction_deferrable = on; SET default_transaction_read_only = on");
    for (q, want) in [
        ("SHOW transaction_deferrable", "D on"),
        ("SHOW transaction_read_only", "D on"),
        ("BEGIN; SHOW transaction_deferrable", "D on"),
        ("SHOW transaction_read_only", "D on"),
        ("ROLLBACK; BEGIN NOT DEFERRABLE; SHOW transaction_deferrable", "D off"),
        ("ROLLBACK; SELECT current_setting('transaction_deferrable')", "D on"),
    ] {
        let got = c.query(q);
        assert!(got.iter().any(|l| l == want), "{q}: {got:?}");
    }
    s.engine.shutdown();
}

/// Sessions changing unrelated catalog objects at once all commit (each session's CREATE
/// TEMP TABLE, tables of their own), as in Postgres, where only the objects' locks
/// serialize DDL (txn/catmerge.rs), and none loses what the other committed meanwhile; two
/// objects given one name still conflict.
#[test]
fn concurrent_ddl_on_unrelated_objects() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        let b = s.connect("postgres").await;
        a.batch_execute("CREATE TABLE shared_t (x int)").await.unwrap();
        for c in [&a, &b] {
            c.batch_execute("BEGIN; CREATE TEMP TABLE foo (n int); INSERT INTO foo VALUES (1)").await.unwrap();
        }
        a.batch_execute("CREATE TABLE a_only (x int); COMMENT ON TABLE a_only IS 'a'").await.unwrap();
        b.batch_execute("CREATE TABLE b_only (y serial PRIMARY KEY); COMMENT ON TABLE b_only IS 'b'").await.unwrap();
        a.batch_execute("COMMIT").await.unwrap();
        b.batch_execute("COMMIT").await.unwrap();
        for c in [&a, &b] {
            let n: i64 = c.query_one("SELECT count(*) FROM foo", &[]).await.unwrap().get(0);
            assert_eq!(n, 1);
            let names: Vec<String> = c
                .query("SELECT relname::text || ':' || coalesce(obj_description(oid, 'pg_class'), '') FROM pg_class WHERE relname IN ('a_only', 'b_only', 'b_only_pkey', 'b_only_y_seq') ORDER BY 1", &[])
                .await
                .unwrap()
                .iter()
                .map(|r| r.get(0))
                .collect();
            assert_eq!(names, ["a_only:a", "b_only:b", "b_only_pkey:", "b_only_y_seq:"]);
        }
        b.batch_execute("INSERT INTO b_only DEFAULT VALUES").await.unwrap();

        // Two tables of one name: likewise.
        a.batch_execute("BEGIN; CREATE TABLE twice (x int)").await.unwrap();
        b.batch_execute("BEGIN; CREATE TABLE twice (y text)").await.unwrap();
        a.batch_execute("COMMIT").await.unwrap();
        assert!(b.batch_execute("COMMIT").await.is_err());
        let cols: i64 = a
            .query_one("SELECT count(*) FROM pg_attribute WHERE attrelid = 'twice'::regclass AND attnum > 0", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(cols, 1);
    });
    s.engine.shutdown();
}

/// pg_hba's md5 and password methods: an MD5 challenge for a role with an MD5 password (SCRAM
/// for one with a SCRAM secret), the password in clear checked against either kind.
#[test]
fn md5_and_password_authentication() {
    for method in [AuthMethod::Md5, AuthMethod::Password] {
        let s = server(method);
        runtime().block_on(async {
            let c = s.connect("postgres").await;
            c.batch_execute(
                "SET password_encryption = md5; CREATE ROLE m LOGIN PASSWORD 'mpw';
                 SET password_encryption = 'scram-sha-256'; CREATE ROLE sc LOGIN PASSWORD 'spw'",
            )
            .await
            .unwrap();
            for (user, pw) in [("m", "mpw"), ("sc", "spw")] {
                let mut cfg = s.config("postgres", pw);
                cfg.user(user);
                let (u, conn) = cfg.connect(NoTls).await.expect(user);
                tokio::spawn(conn);
                let who: String = u.query_one("SELECT system_user", &[]).await.unwrap().get(0);
                let want = match method {
                    AuthMethod::Md5 => format!("md5:{user}"),
                    _ => format!("password:{user}"),
                };
                assert_eq!(who, want);
                let mut bad = s.config("postgres", "nope");
                bad.user(user);
                let e = bad.connect(NoTls).await.err().expect("wrong password");
                assert_eq!(e.code().map(|c| c.code()), Some("28P01"));
            }
        });
        s.engine.shutdown();
    }
}

/// record_recv / record_send name a composite's domain field by the domain's oid (pgx's and
/// Npgsql's composite codecs send it so).
#[test]
fn composite_domain_fields_in_binary() {
    let s = server(AuthMethod::Trust);
    let mut c = raw::Raw::connect(&s.addr, "postgres");
    c.query("CREATE DOMAIN cdd AS text; CREATE TYPE cdt AS (a text, b cdd)");
    let r = c.query("SELECT 'cdd'::regtype::oid");
    let oid: u32 = r[1].strip_prefix("D ").unwrap().parse().unwrap();
    let mut rec = 2i32.to_be_bytes().to_vec();
    for (o, v) in [(25u32, b"x"), (oid, b"y")] {
        rec.extend_from_slice(&o.to_be_bytes());
        rec.extend_from_slice(&1i32.to_be_bytes());
        rec.extend_from_slice(v);
    }
    c.send(b'P', b"\0SELECT ($1::cdt).b\0\0\0");
    let mut b = b"\0\0\0\x01\0\x01\0\x01".to_vec();
    b.extend_from_slice(&(rec.len() as i32).to_be_bytes());
    b.extend_from_slice(&rec);
    b.extend_from_slice(b"\0\0");
    c.send(b'B', &b);
    c.execute("", 0);
    assert_eq!(c.sync(), ["D y", "C SELECT 1"]);
    s.engine.shutdown();
}

/// Postgres's heavyweight lock conflicts between sessions: LOCK TABLE modes, a session's
/// waiting reported by pg_isolation_test_session_is_blocked / pg_blocking_pids, statements
/// taking their relations' locks (an ALTER TABLE waits for a reader's transaction), and a
/// two-session deadlock failing one of them with Postgres's DETAIL.
#[test]
fn heavyweight_locks_and_deadlocks() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        let b = s.connect("postgres").await;
        let c = s.connect("postgres").await;
        a.batch_execute("CREATE TABLE hl1 (x int); CREATE TABLE hl2 (x int)").await.unwrap();
        let pa = lines(&a, "SELECT pg_backend_pid()").await.unwrap().remove(0);
        let pb = lines(&b, "SELECT pg_backend_pid()").await.unwrap().remove(0);
        // ROW EXCLUSIVE and SHARE UPDATE EXCLUSIVE don't conflict; SHARE and ROW EXCLUSIVE do.
        a.batch_execute("BEGIN; LOCK hl1 IN ROW EXCLUSIVE MODE").await.unwrap();
        b.batch_execute("BEGIN; LOCK hl1 IN SHARE UPDATE EXCLUSIVE MODE").await.unwrap();
        let err = lines(&b, "LOCK hl1 IN SHARE MODE NOWAIT").await.unwrap_err();
        assert_eq!(err.code().map(|c| c.code()), Some("55P03"));
        a.batch_execute("ROLLBACK").await.unwrap();
        b.batch_execute("ROLLBACK").await.unwrap();
        // A reader's AccessShareLock holds off ALTER TABLE until its transaction ends.
        a.batch_execute("BEGIN").await.unwrap();
        assert!(lines(&a, "SELECT * FROM hl1").await.unwrap().is_empty());
        let waiter = tokio::spawn(async move {
            let r = b.batch_execute("ALTER TABLE hl1 ADD COLUMN y int").await;
            (b, r)
        });
        let q = format!("SELECT pg_isolation_test_session_is_blocked({pb}, '{{{pa}}}'), pg_blocking_pids({pb})");
        let mut seen = Vec::new();
        for _ in 0..100 {
            seen = lines(&c, &q).await.unwrap();
            if seen[0].starts_with('t') {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(seen, [format!("t|{{{pa}}}")]);
        a.batch_execute("COMMIT").await.unwrap();
        let (b, r) = waiter.await.unwrap();
        r.unwrap();
        // A deadlock: the one that checks (after deadlock_timeout) fails.
        a.batch_execute("BEGIN; SET deadlock_timeout = '10s'; LOCK hl1").await.unwrap();
        b.batch_execute("BEGIN; SET deadlock_timeout = '10ms'; LOCK hl2").await.unwrap();
        let waiter = tokio::spawn(async move {
            let r = a.batch_execute("LOCK hl2").await;
            (a, r)
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        let err = b.batch_execute("LOCK hl1").await.unwrap_err();
        let db = err.as_db_error().unwrap();
        assert_eq!(db.code().code(), "40P01");
        assert_eq!(db.message(), "deadlock detected");
        let detail = db.detail().unwrap();
        assert!(detail.starts_with(&format!("Process {pb} waits for AccessExclusiveLock on relation ")), "{detail}");
        assert!(detail.contains(&format!("; blocked by process {pa}.\nProcess {pa} waits for AccessExclusiveLock on relation ")), "{detail}");
        assert_eq!(db.hint(), Some("See server log for query details."));
        b.batch_execute("ROLLBACK").await.unwrap();
        let (a, r) = waiter.await.unwrap();
        r.unwrap();
        a.batch_execute("COMMIT").await.unwrap();
    });
    s.engine.shutdown();
}

/// SERIALIZABLE transactions: write skew (each reads what the other writes) fails one of
/// them with Postgres's serialization failure; under REPEATABLE READ both commit.
#[test]
fn serializable_write_skew() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        let b = s.connect("postgres").await;
        a.batch_execute("CREATE TABLE ws (id int PRIMARY KEY, on_call bool); INSERT INTO ws VALUES (1, true), (2, true)")
            .await
            .unwrap();
        for (level, fails) in [("REPEATABLE READ", false), ("SERIALIZABLE", true)] {
            a.batch_execute("UPDATE ws SET on_call = true").await.unwrap();
            for cl in [&a, &b] {
                cl.batch_execute(&format!("BEGIN ISOLATION LEVEL {level}")).await.unwrap();
                assert_eq!(lines(cl, "SELECT count(*) FROM ws WHERE on_call").await.unwrap(), ["2"]);
            }
            a.batch_execute("UPDATE ws SET on_call = false WHERE id = 1").await.unwrap();
            b.batch_execute("UPDATE ws SET on_call = false WHERE id = 2").await.unwrap();
            a.batch_execute("COMMIT").await.unwrap();
            let r = b.batch_execute("COMMIT").await;
            if fails {
                let err = r.unwrap_err();
                let db = err.as_db_error().unwrap();
                assert_eq!(db.code().code(), "40001");
                assert_eq!(
                    db.message(),
                    "could not serialize access due to read/write dependencies among transactions"
                );
                assert_eq!(db.hint(), Some("The transaction might succeed if retried."));
            } else {
                r.unwrap();
            }
        }
    });
    s.engine.shutdown();
}

/// Cumulative statistics reach other sessions when pgstat_report_stat sends them: a backend
/// going idle flushes its counts unless it flushed less than PGSTAT_MIN_INTERVAL (1 s) ago,
/// pg_stat_force_next_flush() overrides that, and counts kept go once the backend has been
/// idle for PGSTAT_IDLE_INTERVAL (10 s); the backend's own pg_stat_get_xact_* see them
/// meanwhile. A reset keeps zeroed entries; stats_fetch_consistency = snapshot covers
/// pg_stat_slru, whose notify SLRU counts the queue pages committed NOTIFYs zero.
#[test]
fn statistics_reach_other_sessions_when_flushed() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        let b = s.connect("postgres").await;
        let ins = "SELECT n_tup_ins FROM pg_stat_user_tables WHERE relname = 'st'";
        a.batch_execute("CREATE TABLE st (x int)").await.unwrap();
        a.batch_execute("SELECT pg_stat_force_next_flush()").await.unwrap();
        // Within a second of the last flush: kept by the backend.
        let t = Instant::now();
        a.batch_execute("INSERT INTO st VALUES (1)").await.unwrap();
        let seen = lines(&b, ins).await.unwrap();
        if t.elapsed() < Duration::from_millis(900) {
            assert_eq!(seen, ["0"]);
        }
        assert_eq!(
            lines(&a, "SELECT pg_stat_get_xact_tuples_inserted('st'::regclass)").await.unwrap(),
            ["1"]
        );
        // Forced: flushed at once.
        a.batch_execute("SELECT pg_stat_force_next_flush()").await.unwrap();
        assert_eq!(lines(&b, ins).await.unwrap(), ["1"]);
        // Kept, then flushed by the idle timeout.
        let t = Instant::now();
        a.batch_execute("INSERT INTO st VALUES (2)").await.unwrap();
        let seen = lines(&b, ins).await.unwrap();
        if t.elapsed() < Duration::from_millis(900) {
            assert_eq!(seen, ["1"]);
        }
        tokio::time::sleep(Duration::from_millis(10_500)).await;
        assert_eq!(lines(&b, ins).await.unwrap(), ["2"]);

        // A reset function's entry stays, zeroed.
        a.batch_execute(
            "CREATE FUNCTION stf() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$; \
             SET track_functions = 'all'",
        )
        .await
        .unwrap();
        a.batch_execute("SELECT stf()").await.unwrap();
        a.batch_execute("SELECT pg_stat_force_next_flush()").await.unwrap();
        let calls = "SELECT pg_stat_get_function_calls('stf'::regproc)";
        assert_eq!(lines(&b, calls).await.unwrap(), ["1"]);
        b.batch_execute("SELECT pg_stat_reset_single_function_counters('stf'::regproc)").await.unwrap();
        assert_eq!(lines(&b, calls).await.unwrap(), ["0"]);
        b.batch_execute("SELECT pg_stat_reset()").await.unwrap();
        assert_eq!(lines(&b, calls).await.unwrap(), ["0"]);

        // The notify queue's new pages, as of the reader's snapshot.
        let zeroed = "SELECT blks_zeroed FROM pg_stat_slru WHERE name = 'notify'";
        b.batch_execute("SET stats_fetch_consistency = snapshot").await.unwrap();
        b.batch_execute("BEGIN").await.unwrap();
        let before: i64 = lines(&b, zeroed).await.unwrap()[0].parse().unwrap();
        a.batch_execute("SELECT pg_notify('big', repeat('x', 4000)) FROM generate_series(1, 3)")
            .await
            .unwrap();
        a.batch_execute("SELECT pg_stat_force_next_flush()").await.unwrap();
        assert_eq!(lines(&b, zeroed).await.unwrap(), [before.to_string()]);
        b.batch_execute("COMMIT").await.unwrap();
        let after: i64 = lines(&b, zeroed).await.unwrap()[0].parse().unwrap();
        assert!(after > before, "{after} > {before}");

        // No transaction is ever prepared.
        let err = a.batch_execute("COMMIT PREPARED 'nope'").await.unwrap_err();
        let db = err.as_db_error().unwrap();
        assert_eq!(db.code().code(), "42704");
        assert_eq!(db.message(), "prepared transaction with identifier \"nope\" does not exist");
    });
    s.engine.shutdown();
}

/// What an index-only scan finds dead and what VACUUM removes follow the pruning horizon:
/// another session's snapshot keeps deleted rows (their index entries still send the scan to
/// the heap: Heap Fetches), a temporary table's horizon is its own session's only.
#[test]
fn pruning_horizons_follow_snapshots() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        let lifeline = s.connect("postgres").await;
        async fn fetches(c: &Client, t: &str) -> String {
            let plan = lines(
                c,
                &format!(
                    "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) \
                     SELECT * FROM {t} ORDER BY data"
                ),
            )
            .await
            .unwrap();
            plan.iter()
                .find_map(|l| l.trim().strip_prefix("Heap Fetches: ").map(str::to_string))
                .unwrap_or_else(|| panic!("{plan:?}"))
        }
        a.batch_execute("SET enable_seqscan = off; SET enable_bitmapscan = off").await.unwrap();
        for temp in [false, true] {
            let t = if temp { "hz_temp" } else { "hz" };
            let kind = if temp { "TEMPORARY" } else { "" };
            a.batch_execute(&format!(
                "CREATE {kind} TABLE {t} (data int unique) WITH (autovacuum_enabled = off); \
                 INSERT INTO {t} VALUES (1), (2)"
            ))
            .await
            .unwrap();
            lifeline.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT 1").await.unwrap();
            assert_eq!(fetches(&a, t).await, "2");
            a.batch_execute(&format!("DELETE FROM {t}")).await.unwrap();
            a.batch_execute("SELECT pg_stat_force_next_flush()").await.unwrap();
            if temp {
                // The first scan finds them dead to everyone and marks their entries.
                assert_eq!(fetches(&a, t).await, "2");
                assert_eq!(fetches(&a, t).await, "0");
            } else {
                assert_eq!(fetches(&a, t).await, "2");
                assert_eq!(fetches(&a, t).await, "2");
                // VACUUM can't remove what the other snapshot may see (and reports it as
                // the table's dead tuples).
                a.batch_execute(&format!("VACUUM {t}")).await.unwrap();
                assert_eq!(fetches(&a, t).await, "2");
                a.batch_execute("SELECT pg_stat_force_next_flush()").await.unwrap();
                let tup = "SELECT n_live_tup, n_dead_tup FROM pg_stat_user_tables WHERE relname = 'hz'";
                assert_eq!(lines(&a, tup).await.unwrap(), ["0|2"]);
                lifeline.batch_execute("COMMIT").await.unwrap();
                a.batch_execute(&format!("VACUUM {t}")).await.unwrap();
                assert_eq!(lines(&a, tup).await.unwrap(), ["0|0"]);
                lifeline.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT 1").await.unwrap();
            }
            lifeline.batch_execute("COMMIT").await.unwrap();
            a.batch_execute(&format!("VACUUM {t}")).await.unwrap();
            assert_eq!(fetches(&a, t).await, "0");
        }
        // Rows the session's own open transaction deleted aren't dead yet.
        a.batch_execute(
            "CREATE TEMPORARY TABLE hz_own (data int unique) WITH (autovacuum_enabled = off); \
             INSERT INTO hz_own VALUES (1), (2)",
        )
        .await
        .unwrap();
        a.batch_execute("BEGIN; DELETE FROM hz_own").await.unwrap();
        assert_eq!(fetches(&a, "hz_own").await, "2");
        assert_eq!(fetches(&a, "hz_own").await, "2");
        a.batch_execute("COMMIT").await.unwrap();
    });
    s.engine.shutdown();
}

/// Wait until session `pid` waits for a lock (as isolationtester does).
async fn until_blocked(c: &Client, pid: &str) {
    let q = format!("SELECT EXISTS (SELECT FROM pg_locks WHERE pid = {pid} AND NOT granted)");
    for _ in 0..500 {
        if lines(c, &q).await.unwrap() == ["t"] {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("session {pid} never waited");
}

/// What a unique check sees of other transactions' writes in progress (Postgres's dirty
/// snapshot): an INSERT of a key whose row another transaction is deleting waits for it,
/// and ON CONFLICT does too (it took DO NOTHING / a unique violation at once); an ON
/// CONFLICT DO UPDATE starting over after its row changed doesn't fire BEFORE INSERT
/// triggers again; pg_locks shows the waits on transactions.
#[test]
fn unique_checks_wait_for_writers_in_progress() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        let b = s.connect("postgres").await;
        let c = s.connect("postgres").await;
        a.batch_execute(
            "CREATE TABLE uq (k int PRIMARY KEY, d text);
             CREATE TABLE uq_log (n int);
             CREATE FUNCTION uq_note() RETURNS trigger LANGUAGE plpgsql AS
               $$ BEGIN INSERT INTO uq_log VALUES (NEW.k); RETURN NEW; END $$;
             CREATE TRIGGER uq_bi BEFORE INSERT ON uq FOR EACH ROW EXECUTE FUNCTION uq_note();
             INSERT INTO uq VALUES (1, 'one'), (2, 'two')",
        )
        .await
        .unwrap();
        let pb = lines(&b, "SELECT pg_backend_pid()").await.unwrap().remove(0);
        // A delete in progress: the insert waits, then the key is free.
        a.batch_execute("BEGIN; DELETE FROM uq WHERE k = 1").await.unwrap();
        let w = tokio::spawn(async move {
            let r = b.batch_execute("INSERT INTO uq VALUES (1, 'again')").await;
            (b, r)
        });
        until_blocked(&c, &pb).await;
        assert_eq!(
            lines(&c, &format!("SELECT locktype, mode FROM pg_locks WHERE pid = {pb} AND NOT granted"))
                .await
                .unwrap(),
            ["transactionid|ShareLock"]
        );
        a.batch_execute("COMMIT").await.unwrap();
        let (b, r) = w.await.unwrap();
        r.unwrap();
        assert_eq!(lines(&c, "SELECT d FROM uq WHERE k = 1").await.unwrap(), ["again"]);
        // An update in progress: ON CONFLICT DO NOTHING waits for it, then does nothing.
        a.batch_execute("BEGIN; UPDATE uq SET d = 'changed' WHERE k = 2").await.unwrap();
        let w = tokio::spawn(async move {
            let r = lines(&b, "INSERT INTO uq VALUES (2, 'no') ON CONFLICT DO NOTHING RETURNING k").await;
            (b, r)
        });
        until_blocked(&c, &pb).await;
        a.batch_execute("COMMIT").await.unwrap();
        let (b, r) = w.await.unwrap();
        assert!(r.unwrap().is_empty());
        // DO UPDATE whose row changed while it waited starts over at its unique checks:
        // the BEFORE INSERT trigger ran once.
        a.batch_execute("TRUNCATE uq_log; BEGIN; UPDATE uq SET d = d || '+a' WHERE k = 2")
            .await
            .unwrap();
        let w = tokio::spawn(async move {
            let r = lines(
                &b,
                "INSERT INTO uq VALUES (2, 'b') ON CONFLICT (k) DO UPDATE SET d = uq.d || '+b' RETURNING d",
            )
            .await;
            (b, r)
        });
        until_blocked(&c, &pb).await;
        a.batch_execute("COMMIT").await.unwrap();
        let (_b, r) = w.await.unwrap();
        assert_eq!(r.unwrap(), ["changed+a+b"]);
        assert_eq!(lines(&c, "SELECT count(*) FROM uq_log").await.unwrap(), ["1"]);
    });
    s.engine.shutdown();
}

/// Concurrent updates seen by MERGE, foreign key checks and TID range scans
/// (EvalPlanQual), and pg_stat_activity's application_name after SET.
#[test]
fn rechecks_after_concurrent_updates() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        let b = s.connect("postgres").await;
        let c = s.connect("postgres").await;
        a.batch_execute(
            "CREATE TABLE mt (key int PRIMARY KEY, val text);
             INSERT INTO mt VALUES (1, 'setup');
             CREATE TABLE pk (a int PRIMARY KEY);
             CREATE TABLE fk (a int REFERENCES pk);
             CREATE TABLE rp (a int, b text) PARTITION BY RANGE (a);
             CREATE TABLE rp1 PARTITION OF rp FOR VALUES FROM (1) TO (10);
             CREATE TABLE rp2 PARTITION OF rp FOR VALUES FROM (10) TO (20);
             CREATE UNIQUE INDEX ON rp1 (a);
             INSERT INTO rp VALUES (7, 'x');
             CREATE TABLE rfk (a int REFERENCES rp1 (a));
             CREATE TABLE tr (id int, v int) WITH (fillfactor = 50);
             INSERT INTO tr VALUES (1, 0)",
        )
        .await
        .unwrap();
        let pb = lines(&b, "SELECT pg_backend_pid()").await.unwrap().remove(0);
        // MERGE: a matched row whose key a concurrent update changed goes to WHEN NOT MATCHED
        // BY SOURCE, and its source row to WHEN NOT MATCHED.
        a.batch_execute("BEGIN; UPDATE mt SET key = 2 WHERE key = 1").await.unwrap();
        let w = tokio::spawn(async move {
            let r = lines(
                &b,
                "MERGE INTO mt t USING (SELECT 1 AS key) s ON s.key = t.key
                 WHEN NOT MATCHED THEN INSERT VALUES (s.key, 'new')
                 WHEN MATCHED THEN UPDATE SET val = 'matched'
                 WHEN NOT MATCHED BY SOURCE THEN UPDATE SET val = 'not by source'
                 RETURNING merge_action(), t.*",
            )
            .await;
            (b, r)
        });
        until_blocked(&c, &pb).await;
        a.batch_execute("COMMIT").await.unwrap();
        let (b, r) = w.await.unwrap();
        assert_eq!(r.unwrap(), ["UPDATE|2|not by source", "INSERT|1|new"]);
        // RI_FKey_check over the transaction snapshot in REPEATABLE READ.
        b.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ").await.unwrap();
        assert!(lines(&b, "SELECT * FROM pk").await.unwrap().is_empty());
        a.batch_execute("INSERT INTO pk VALUES (1)").await.unwrap();
        let err = b.batch_execute("INSERT INTO fk VALUES (1)").await.unwrap_err();
        assert_eq!(err.code().map(|c| c.code()), Some("23503"));
        b.batch_execute("ROLLBACK; INSERT INTO fk VALUES (1)").await.unwrap();
        // ... and a referenced row moved to another partition while the check waited.
        a.batch_execute("BEGIN; UPDATE rp SET a = 11 WHERE a = 7").await.unwrap();
        let w = tokio::spawn(async move {
            let r = b.batch_execute("INSERT INTO rfk VALUES (7)").await;
            (b, r)
        });
        until_blocked(&c, &pb).await;
        a.batch_execute("COMMIT").await.unwrap();
        let (b, r) = w.await.unwrap();
        let err = r.unwrap_err();
        assert_eq!(err.code().map(|c| c.code()), Some("40001"));
        assert_eq!(
            err.as_db_error().unwrap().message(),
            "tuple to be locked was already moved to another partition due to concurrent update"
        );
        // A TID range scan's row whose new version is outside the range is skipped.
        a.batch_execute("BEGIN; UPDATE tr SET v = v + 1 WHERE ctid BETWEEN '(0,1)' AND '(0,1)'")
            .await
            .unwrap();
        let w = tokio::spawn(async move {
            let r = lines(
                &b,
                "UPDATE tr SET v = v + 10 WHERE ctid BETWEEN '(0,1)' AND '(0,1)' RETURNING v",
            )
            .await;
            (b, r)
        });
        until_blocked(&c, &pb).await;
        a.batch_execute("COMMIT").await.unwrap();
        let (b, r) = w.await.unwrap();
        assert!(r.unwrap().is_empty());
        assert_eq!(lines(&c, "SELECT v FROM tr").await.unwrap(), ["1"]);
        // SET application_name shows in pg_stat_activity.
        b.batch_execute("SET application_name = 'renamed'").await.unwrap();
        assert_eq!(
            lines(&c, &format!("SELECT application_name FROM pg_stat_activity WHERE pid = {pb}"))
                .await
                .unwrap(),
            ["renamed"]
        );
    });
    s.engine.shutdown();
}

/// What PostgreSQL sends as it happens rather than at a statement's end: a notice as it is
/// raised (send_message_to_frontend flushes it), a COPY FROM STDIN's bad line while the
/// client is still sending (CopyFrom reads lines as they come), and a portal's rows up to
/// each Execute's count, the query running no further (PortalRunSelect).
#[test]
fn notices_copy_lines_and_portal_rows_as_they_come() {
    let s = server(AuthMethod::Trust);
    let mut c = raw::Raw::connect(&s.addr, "postgres");
    let wait = Duration::from_millis(1500);
    // The first notice arrives while the DO block still sleeps.
    let start = Instant::now();
    c.send(b'Q', b"DO $$BEGIN RAISE NOTICE 'one'; PERFORM pg_sleep(3); END$$\0");
    let (t, b) = c.next_within(wait).expect("the notice, before the statement ends");
    assert_eq!(t, b'N');
    assert!(String::from_utf8_lossy(&b).contains("one"));
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(c.read(), ["C DO"]);

    // A bad first line fails the COPY before CopyDone.
    c.query("CREATE TABLE cl (a int)");
    c.send(b'Q', b"COPY cl FROM STDIN\0");
    assert_eq!(c.next_within(wait).map(|m| m.0), Some(b'G'));
    c.send(b'd', b"1\nx\n");
    let (t, b) = c.next_within(wait).expect("the error, before CopyDone");
    assert_eq!(t, b'E');
    assert!(String::from_utf8_lossy(&b).contains("22P02"));
    assert_eq!(c.read(), Vec::<String>::new());
    // (The rest of the client's copy data is ignored.)
    c.send(b'd', b"2\n");
    c.send(b'c', b"");
    assert_eq!(c.query("SELECT count(*) FROM cl"), ["T count:20/-1", "D 0", "C SELECT 1"]);
    // Binary: a field that fails as soon as it is there.
    c.send(b'Q', b"COPY cl FROM STDIN (FORMAT binary)\0");
    assert_eq!(c.next_within(wait).map(|m| m.0), Some(b'G'));
    let mut d = b"PGCOPY\n\xff\r\n\0\0\0\0\0\0\0\0\0".to_vec();
    d.extend_from_slice(&[0, 1, 0, 0, 0, 2, 0, 1]);
    c.send(b'd', &d);
    let (t, b) = c.next_within(wait).expect("the error, before CopyDone");
    assert_eq!(t, b'E');
    assert!(String::from_utf8_lossy(&b).contains("08P01"));
    c.read();
    c.send(b'c', b"");

    // Rows past an Execute's count aren't made yet: the third row's error comes at the
    // third Execute, after two suspended portals' rows.
    c.parse("", "SELECT 10 / (3 - g) FROM generate_series(1, 5) g");
    c.bind("", "");
    c.execute("", 1);
    c.execute("", 1);
    c.execute("", 1);
    assert_eq!(c.sync(), ["D 5", "s", "D 10", "s", "E 22012"]);
    // Exactly the count: suspended, then complete with none.
    c.parse("", "SELECT g FROM generate_series(1, 2) g");
    c.bind("", "");
    c.execute("", 2);
    c.execute("", 2);
    c.execute("", 2);
    assert_eq!(c.sync(), ["D 1", "D 2", "s", "C SELECT 0", "C SELECT 0"]);

    // The largest numeric in binary: 36864 base-10000 digits, weight 32767 (pgx's
    // longestNumeric), sent and read back.
    c.parse("", "SELECT ('1' || repeat('0', 131070) || '.' || repeat('0', 16382) || '1')::numeric");
    c.send(b'B', b"\0\0\0\0\0\0\0\x01\0\x01");
    c.execute("", 0);
    c.send(b'S', b"");
    let mut bin = None;
    // (Making and formatting it takes a debug build a while.)
    let wait = Duration::from_secs(60);
    while let Some((t, b)) = c.next_within(wait) {
        match t {
            b'D' => bin = Some(b[6..].to_vec()),
            b'Z' => break,
            _ => {}
        }
    }
    let bin = bin.expect("the row");
    assert_eq!(&bin[..4], &[0x90, 0x00, 0x7f, 0xff]);
    let mut p = b"\0SELECT $1::numeric = ('1' || repeat('0', 131070) || '.' || repeat('0', 16382) || '1')::numeric\0".to_vec();
    p.extend_from_slice(&1i16.to_be_bytes());
    p.extend_from_slice(&1700u32.to_be_bytes());
    c.send(b'P', &p);
    let mut b = b"\0\0\0\x01\0\x01\0\x01".to_vec();
    b.extend_from_slice(&(bin.len() as i32).to_be_bytes());
    b.extend_from_slice(&bin);
    b.extend_from_slice(b"\0\0");
    c.send(b'B', &b);
    c.execute("", 0);
    assert_eq!(c.sync(), ["D t", "C SELECT 1"]);

    // An INSERT ... VALUES ($1) a DO INSTEAD NOTHING rule drops still describes its
    // parameter (lib/pq TestIssue186): the VALUES lists' $n count.
    c.query("CREATE TABLE rn (f1 int); CREATE RULE rn_nothing AS ON INSERT TO rn DO INSTEAD NOTHING");
    c.parse("", "INSERT INTO rn VALUES ($1)");
    c.describe(b'S', "");
    c.send(b'S', b"");
    let mut desc = None;
    while let Some((t, b)) = c.next_within(wait) {
        match t {
            b't' => desc = Some(b),
            b'Z' => break,
            _ => {}
        }
    }
    assert_eq!(desc.as_deref(), Some(&[0, 1, 0, 0, 0, 23][..]));
    s.engine.shutdown();
}

/// A table's TOAST table as a relation of its own: writing a value out of line locks it
/// (RowExclusiveLock until the transaction ends), REINDEX ... CONCURRENTLY of it waits for
/// that, it can be renamed under allow_system_table_mods, and a DROP of its table holds it
/// so a REINDEX waiting for it then finds it gone.
#[test]
fn toast_relations() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        let b = s.connect("postgres").await;
        let c = s.connect("postgres").await;
        a.batch_execute(
            "CREATE TABLE tw (id int PRIMARY KEY, data text);
             INSERT INTO tw SELECT 1, string_agg(md5(g::text), '') FROM generate_series(1, 500) g",
        )
        .await
        .unwrap();
        let toast = lines(&a, "SELECT reltoastrelid::regclass::text FROM pg_class WHERE oid = 'tw'::regclass")
            .await
            .unwrap()
            .remove(0);
        // Renaming it is a system catalog change.
        let err = a
            .batch_execute(&format!("ALTER TABLE {toast} RENAME TO tw_toast"))
            .await
            .unwrap_err();
        assert_eq!(
            err.as_db_error().unwrap().message(),
            format!("permission denied: \"{}\" is a system catalog", toast.trim_start_matches("pg_toast."))
        );
        a.batch_execute(&format!(
            "SET allow_system_table_mods = on; ALTER TABLE {toast} RENAME TO tw_toast; RESET allow_system_table_mods"
        ))
        .await
        .unwrap();
        assert_eq!(
            lines(&a, "SELECT reltoastrelid::regclass::text FROM pg_class WHERE oid = 'tw'::regclass")
                .await
                .unwrap(),
            ["pg_toast.tw_toast"]
        );
        // An out-of-line value written: the TOAST table is locked.
        let pa = lines(&a, "SELECT pg_backend_pid()").await.unwrap().remove(0);
        let pb = lines(&b, "SELECT pg_backend_pid()").await.unwrap().remove(0);
        a.batch_execute(
            "BEGIN; INSERT INTO tw SELECT 2, string_agg(md5(g::text), '') FROM generate_series(1, 500) g",
        )
        .await
        .unwrap();
        assert_eq!(
            lines(
                &c,
                &format!("SELECT mode FROM pg_locks WHERE pid = {pa} AND relation = 'pg_toast.tw_toast'::regclass")
            )
            .await
            .unwrap(),
            ["RowExclusiveLock"]
        );
        let w = tokio::spawn(async move {
            let r = b.batch_execute("REINDEX TABLE CONCURRENTLY pg_toast.tw_toast").await;
            (b, r)
        });
        until_waiting(&c, &pb).await;
        a.batch_execute("COMMIT").await.unwrap();
        let (b, r) = w.await.unwrap();
        r.unwrap();
        // A DROP in progress holds it: the REINDEX waits, then finds it gone.
        a.batch_execute("BEGIN; DROP TABLE tw").await.unwrap();
        let w = tokio::spawn(async move {
            let r = b.batch_execute("REINDEX TABLE CONCURRENTLY pg_toast.tw_toast").await;
            (b, r)
        });
        until_waiting(&c, &pb).await;
        a.batch_execute("COMMIT").await.unwrap();
        let (_b, r) = w.await.unwrap();
        assert_eq!(
            r.unwrap_err().as_db_error().unwrap().message(),
            "relation \"pg_toast.tw_toast\" does not exist"
        );
    });
    s.engine.shutdown();
}

/// Wait until session `pid` waits for a lock.
async fn until_waiting(c: &Client, pid: &str) {
    let q = format!("SELECT EXISTS (SELECT FROM pg_locks WHERE pid = {pid} AND NOT granted)");
    for _ in 0..500 {
        if lines(c, &q).await.unwrap() == ["t"] {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("session {pid} never waited");
}
