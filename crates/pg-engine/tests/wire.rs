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

#[path = "common/protocol.rs"]
mod protocol;

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
        server_programs: false,
        shared_preload_libraries: Vec::new(),
        locale: Default::default(),
        outbound: Vec::new(),
        max_prepared_transactions: 0,
        auth_files: Default::default(),
        settings: Vec::new(),
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
    let (pid, _, _) = s.engine.register_backend(None, None, 0);
    let mut c = s.engine.connect(pid, "postgres", "postgres", "t").unwrap();
    let relkind = |c: &mut pg_engine::session::Session, name: &str| -> Result<String, String> {
        c.parse_message("", "SELECT relkind FROM pg_catalog.pg_class WHERE oid = $1::pg_catalog.regclass", &[0])
            .and_then(|_| c.bind_message("", "", vec![Some(name.as_bytes().to_vec())], &[], Vec::new(), false))
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
        // Over rows (pg_restore -j's indexes): both set the table's pg_class statistics, an
        // in-place update in Postgres, which no concurrent index build conflicts with.
        a.batch_execute("INSERT INTO t1 SELECT g, g FROM generate_series(1, 2000) g").await.unwrap();
        both(
            "CREATE INDEX t1_h ON t1 USING hash (x) WITH (fillfactor = 60)",
            "CREATE UNIQUE INDEX t1_e ON t1 ((x + y))",
        )
        .await
        .unwrap();
        // Indexes made at once have storage of their own (their ids were a per-table
        // counter both took the same value of).
        a.batch_execute("SET enable_seqscan = off; SET enable_bitmapscan = off").await.unwrap();
        for q in [
            "SELECT count(*) FROM t1 WHERE y = 7",
            "SELECT count(*) FROM t1 WHERE x = 7 AND y = 7",
            "SELECT count(*) FROM t1 WHERE x + y = 14",
            "SELECT count(*) FROM t1 WHERE x = 7",
        ] {
            let n: i64 = a.query_one(q, &[]).await.unwrap().get(0);
            assert_eq!(n, 1, "{q}");
        }
        a.batch_execute("RESET enable_seqscan; RESET enable_bitmapscan").await.unwrap();
        a.batch_execute("DROP INDEX t1_h, t1_e").await.unwrap();
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
        // The same name taken by both: the second waits for the first's uncommitted catalog
        // row, then fails with its unique index's violation once the first commits.
        a.batch_execute("BEGIN; CREATE TABLE n (a int)").await.unwrap();
        b.batch_execute("BEGIN").await.unwrap();
        let (r, ()) = tokio::join!(b.batch_execute("CREATE TABLE n (b text)"), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            a.batch_execute("COMMIT").await.unwrap();
        });
        assert_eq!(code(r.unwrap_err()).as_deref(), Some("23505"));
        b.batch_execute("ROLLBACK").await.ok();
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

/// A database's own options (pg_database's datistemplate, datallowconn, datconnlimit): kept,
/// shown, put back by a rollback, and enforced as PostgreSQL 18 does (messages and codes
/// recorded from it): refused connections, connection limits, template copies and drops,
/// invalid databases (datconnlimit -2).
#[test]
fn database_options() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        let opts = |name: &'static str| {
            let a = &a;
            async move {
                let r = a
                    .query_one(
                        "SELECT datistemplate, datallowconn, datconnlimit FROM pg_database WHERE datname = $1",
                        &[&name],
                    )
                    .await
                    .unwrap();
                (r.get::<_, bool>(0), r.get::<_, bool>(1), r.get::<_, i32>(2))
            }
        };
        let fails = |sql: &'static str, code: &'static str, msg: &'static str| {
            let a = &a;
            async move {
                let e = a.batch_execute(sql).await.unwrap_err();
                let d = e.as_db_error().expect("db error");
                assert_eq!((d.code().code(), d.message()), (code, msg), "{sql}");
            }
        };
        assert_eq!(opts("template0").await, (true, false, -1));
        assert_eq!(opts("template1").await, (true, true, -1));
        assert_eq!(opts("postgres").await, (false, true, -1));
        a.batch_execute(
            "CREATE ROLE dbo LOGIN PASSWORD 'pw'; CREATE ROLE other LOGIN PASSWORD 'pw' CREATEDB",
        )
        .await
        .unwrap();
        a.batch_execute(
            "CREATE DATABASE optdb OWNER dbo CONNECTION LIMIT 1 IS_TEMPLATE true ALLOW_CONNECTIONS true",
        )
        .await
        .unwrap();
        assert_eq!(opts("optdb").await, (true, true, 1));
        fails("CREATE DATABASE bad CONNECTION LIMIT -3", "22023", "invalid connection limit: -3").await;
        fails("CREATE DATABASE bad IS_TEMPLATE maybe", "42601", "is_template requires a Boolean value").await;
        fails("CREATE DATABASE bad IS_TEMPLATE 2", "42601", "is_template requires a Boolean value").await;
        fails("CREATE DATABASE bad CONNECTION LIMIT 'x'", "42601", "connection_limit requires an integer value").await;
        fails("CREATE DATABASE bad CONNECTION LIMIT 1 CONNECTION LIMIT 2", "42601", "conflicting or redundant options").await;
        fails("ALTER DATABASE optdb CONNECTION LIMIT -3", "22023", "invalid connection limit: -3").await;
        fails("ALTER DATABASE optdb WITH foo 1", "42601", "option \"foo\" not recognized").await;
        fails("ALTER DATABASE nope CONNECTION LIMIT 3", "3D000", "database \"nope\" does not exist").await;
        fails("ALTER DATABASE postgres ALLOW_CONNECTIONS false", "22023", "cannot disallow connections for current database").await;
        fails("DROP DATABASE optdb", "42809", "cannot drop a template database").await;

        // A connection limit counts the database's sessions (a superuser's aren't limited).
        let mut dbo = s.config("optdb", "pw");
        dbo.user("dbo");
        let (c1, conn) = dbo.connect(NoTls).await.unwrap();
        tokio::spawn(conn);
        let e = dbo.connect(NoTls).await.err().unwrap();
        assert_eq!(e.as_db_error().map(|d| (d.code().code(), d.message())), Some(("53300", "too many connections for database \"optdb\"")));
        drop(s.connect("optdb").await);
        drop(c1);
        // (Their sessions end on their own threads; a template copy needs them gone.)
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Rolled back, the options are back.
        a.batch_execute("ALTER DATABASE optdb CONNECTION LIMIT 5 IS_TEMPLATE false").await.unwrap();
        assert_eq!(opts("optdb").await, (false, true, 5));
        a.batch_execute("BEGIN; ALTER DATABASE optdb ALLOW_CONNECTIONS false CONNECTION LIMIT DEFAULT").await.unwrap();
        assert_eq!(opts("optdb").await, (false, false, -1));
        a.batch_execute("ROLLBACK").await.unwrap();
        assert_eq!(opts("optdb").await, (false, true, 5));
        a.batch_execute("ALTER DATABASE optdb ALLOW_CONNECTIONS false").await.unwrap();
        let e = s.config("optdb", "secret").connect(NoTls).await.err().unwrap();
        assert_eq!(e.as_db_error().map(|d| (d.code().code(), d.message())), Some(("55000", "database \"optdb\" is not currently accepting connections")));
        a.batch_execute("ALTER DATABASE optdb ALLOW_CONNECTIONS true").await.unwrap();

        // Only a template, or one's own database, may be copied.
        let mut oc = s.config("postgres", "pw");
        oc.user("other");
        let (o, conn) = oc.connect(NoTls).await.unwrap();
        tokio::spawn(conn);
        let e = o.batch_execute("CREATE DATABASE copy1 TEMPLATE optdb").await.unwrap_err();
        assert_eq!(e.as_db_error().map(|d| (d.code().code(), d.message())), Some(("42501", "permission denied to copy database \"optdb\"")));
        let e = o.batch_execute("ALTER DATABASE optdb CONNECTION LIMIT 3").await.unwrap_err();
        assert_eq!(e.as_db_error().map(|d| (d.code().code(), d.message())), Some(("42501", "must be owner of database optdb")));
        a.batch_execute("ALTER DATABASE optdb IS_TEMPLATE true").await.unwrap();
        o.batch_execute("CREATE DATABASE copy1 TEMPLATE optdb").await.unwrap();
        a.batch_execute("DROP DATABASE copy1").await.unwrap();

        // pg_dump --clean's way to drop a template; an invalid database (datconnlimit -2)
        // takes no connections, can't be a template or be altered, only dropped.
        a.batch_execute("UPDATE pg_database SET datistemplate = false WHERE datname = 'optdb'").await.unwrap();
        a.batch_execute("UPDATE pg_database SET datconnlimit = -2 WHERE datname = 'optdb'").await.unwrap();
        assert_eq!(opts("optdb").await, (false, true, -2));
        fails("CREATE DATABASE copy2 TEMPLATE optdb", "55000", "cannot use invalid database \"optdb\" as template").await;
        let e = s.config("optdb", "secret").connect(NoTls).await.err().unwrap();
        let d = e.as_db_error().unwrap();
        assert_eq!((d.code().code(), d.message(), d.hint()), ("55000", "cannot connect to invalid database \"optdb\"", Some("Use DROP DATABASE to drop invalid databases.")));
        let e = a.batch_execute("ALTER DATABASE optdb CONNECTION LIMIT 3").await.unwrap_err();
        assert_eq!(e.as_db_error().map(|d| (d.severity(), d.message())), Some(("FATAL", "cannot alter invalid database \"optdb\"")));
        let a = s.connect("postgres").await;
        a.batch_execute("DROP DATABASE optdb").await.unwrap();
        a.batch_execute("DROP ROLE dbo; DROP ROLE other").await.unwrap();
    });
    s.engine.shutdown();
}

#[test]
fn copy_in_and_out() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let c = s.connect("postgres").await;
        c.batch_execute("CREATE TABLE cp (id int, name text)").await.unwrap();
        c.batch_execute("BEGIN").await.unwrap();
        let sink = c.copy_in("COPY cp FROM STDIN").await.unwrap();
        futures::pin_mut!(sink);
        let mut data = String::new();
        for i in 0..1010 {
            data.push_str(&format!("{i}\tname {i}\n"));
        }
        // Several CopyData messages, split mid-line.
        for chunk in data.as_bytes().chunks(777) {
            sink.send(bytes::Bytes::copy_from_slice(chunk)).await.unwrap();
        }
        let n = sink.finish().await.unwrap();
        assert_eq!(n, 1010);
        assert_eq!(c.query_one("SELECT n_tup_ins FROM pg_stat_xact_user_tables WHERE relname = 'cp'", &[]).await.unwrap().get::<_, i64>(0), 1010);
        c.batch_execute("COMMIT").await.unwrap();
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
        assert_eq!(c.query_one("SELECT count(*) FROM cp", &[]).await.unwrap().get::<_, i64>(0), 1010);
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

/// DROP DATABASE without FORCE waits up to 5 s for the database's other sessions to go
/// (CountOtherDBBackends), so one exiting meanwhile doesn't fail it; one staying does,
/// after the wait.
#[test]
fn drop_database_waits_for_exiting_sessions() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let admin = s.connect("postgres").await;
        admin.batch_execute("CREATE DATABASE dw").await.unwrap();
        let a = s.connect("dw").await;
        a.batch_execute("SELECT 1").await.unwrap();
        let leave = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            drop(a);
        });
        let t = Instant::now();
        admin.batch_execute("DROP DATABASE dw").await.unwrap();
        assert!(t.elapsed() >= Duration::from_millis(900), "{:?}", t.elapsed());
        leave.await.unwrap();

        admin.batch_execute("CREATE DATABASE dw").await.unwrap();
        let _a = s.connect("dw").await;
        let t = Instant::now();
        let err = admin.batch_execute("DROP DATABASE dw").await.unwrap_err();
        assert_eq!(err.code().map(|c| c.code()), Some("55006"));
        assert!(t.elapsed() >= Duration::from_millis(4500), "{:?}", t.elapsed());
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
fn extension_types_binary_format() {
    let s = server(AuthMethod::Trust);
    runtime().block_on(async {
        let c = s.connect("postgres").await;
        c.batch_execute(
            "CREATE EXTENSION hstore; CREATE EXTENSION ltree; CREATE EXTENSION cube; \
             CREATE EXTENSION citext; CREATE EXTENSION seg",
        )
        .await
        .unwrap();
        // hstore_send, ltree_send / lquery_send / ltxtq_send, cube_send and citext's, as
        // PostgreSQL 18 produces them.
        let r = c
            .query_one(
                "SELECT 'b=>2, a=>1, \"k k\"=>NULL'::hstore, 'a.b.c'::ltree, '*.b.*'::lquery, \
                 'a & b'::ltxtquery, '(1,2),(3,4)'::cube, '(5,6)'::cube, 'MiXed'::citext",
                &[],
            )
            .await
            .unwrap();
        let want = [
            "000000030000000161000000013100000001620000000132000000036b206bffffffff",
            "01612e622e63",
            "012a2e622e2a",
            "016120262062",
            "000000023ff000000000000040000000000000004008000000000000\
             4010000000000000",
            "8000000240140000000000004018000000000000",
            "4d69586564",
        ];
        for (i, w) in want.iter().enumerate() {
            let w: String = w.split_whitespace().collect();
            assert_eq!(r.get::<_, Raw>(i), hex(&w), "column {i}");
        }
        // The receive functions: parameters in binary.
        let st = c
            .prepare("SELECT $1::hstore::text, $2::ltree::text, $3::cube::text")
            .await
            .unwrap();
        let r = c
            .query_one(
                &st,
                &[
                    &hex("000000020000000162000000013200000001610000000131"),
                    &hex("01782e79"),
                    &hex("800000013ff8000000000000"),
                ],
            )
            .await
            .unwrap();
        assert_eq!(r.get::<_, String>(0), "\"a\"=>\"1\", \"b\"=>\"2\"");
        assert_eq!(r.get::<_, String>(1), "x.y");
        assert_eq!(r.get::<_, String>(2), "(1.5)");
        // seg has no send function.
        let e = c
            .query_one("SELECT '1 .. 2'::seg", &[])
            .await
            .unwrap_err();
        assert_eq!(
            e.as_db_error().map(|d| d.message()),
            Some("no binary output function available for type seg")
        );
    });
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

/// array_send of an array of a domain names the domain as its element type (the column's
/// type is the domain's array type).
#[test]
fn domain_array_binary_element_type() {
    let s = server(AuthMethod::Trust);
    runtime().block_on(async {
        // array_recv's header, a field at a time (checked against PostgreSQL 18.6): too many
        // dimensions, a negative number of them, bad flags, a header cut short.
        let c = s.connect("postgres").await;
        let st = c.prepare("SELECT $1::int4[]").await.unwrap();
        for (bytes, code) in [
            ("000000070000000000000017", "54000"),
            ("ffffffff0000000000000017", "22P03"),
            ("000000010000000200000017", "22P03"),
            ("00000001", "08P01"),
        ] {
            let e = c.query_one(&st, &[&hex(bytes)]).await.unwrap_err();
            assert_eq!(e.code().map(|c| c.code()), Some(code), "{bytes}: {e:?}");
        }
    });
    runtime().block_on(async {
        let c = s.connect("postgres").await;
        c.batch_execute("CREATE DOMAIN wire_posint AS int CHECK (VALUE > 0)").await.unwrap();
        let d: u32 = c.query_one("SELECT 'wire_posint'::regtype::oid", &[]).await.unwrap().get(0);
        for q in ["SELECT '{1,2}'::wire_posint[]", "SELECT '{}'::wire_posint[]", "SELECT '{{1},{2}}'::wire_posint[]"] {
            let r = c.query_one(q, &[]).await.unwrap();
            let raw = r.get::<_, Raw>(0).0;
            assert_eq!(u32::from_be_bytes(raw[8..12].try_into().unwrap()), d, "{q}");
        }
        // (An array of the base type: int4's.)
        let r = c.query_one("SELECT '{1,2}'::int[]", &[]).await.unwrap();
        assert_eq!(u32::from_be_bytes(r.get::<_, Raw>(0).0[8..12].try_into().unwrap()), 23);
        c.batch_execute("DROP DOMAIN wire_posint").await.unwrap();
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

/// A kept index scan's rows sent from their stored values (`RowSink::row_stored`) are the
/// bytes the decoded rows make: each statement's DataRows and CommandComplete, executed again
/// and again, in binary, text and mixed result formats, as those of the same statement under a
/// LIMIT (whose rows are decoded first). Fixed-width values, NULLs, a column added since the
/// rows were stored, text, a repeated column and columns out of their stored order; strings
/// of the string types (multibyte ones too) and a date, for a UTF-8 and a LATIN1 client.
#[test]
fn stored_rows_go_out_as_decoded_ones() {
    let s = server(AuthMethod::Trust);
    let mut c = raw::Raw::connect(&s.addr, "postgres");
    c.query(
        "CREATE TABLE sr (id int PRIMARY KEY, i2 int2, i8 int8, f4 real, f8 float8, b bool, o oid, t text, n int,
           vc varchar(8), bp char(4), nm name, d date, ut text);
         INSERT INTO sr SELECT g, (g - 150)::int2, (150 - g) * 1000003, g / 7.0, g / 3.0, g % 2 = 0, g + 7, 'v' || g,
           CASE WHEN g % 5 = 0 THEN NULL ELSE -g END, 'c' || g, 'p' || g % 10, ('m' || g)::name,
           date '2024-01-01' + g, CASE WHEN g % 3 = 0 THEN 'é' || g WHEN g % 3 = 1 THEN '' END
           FROM generate_series(1, 300) g;
         ANALYZE sr;
         ALTER TABLE sr ADD COLUMN late int DEFAULT 77;
         INSERT INTO sr VALUES (301, 1, 2, 3, 4, true, 5, 'new', NULL, 'x', 'y', 'z', NULL, 'ü', 9)",
    );
    let lists = ["id, i2, i8, f4, f8, b, o, n", "n, id", "id, late", "id, t", "id, id", "i8", "id, vc, bp, nm, ut", "ut, d", "b, o"];
    for (i, cols) in lists.iter().enumerate() {
        let q = format!("SELECT {cols} FROM sr WHERE id BETWEEN $1 AND $1 + 12 ORDER BY id");
        c.parse(&format!("s{i}"), &q);
        c.parse(&format!("l{i}"), &format!("{q} LIMIT 1000"));
        assert_eq!(c.sync(), Vec::<String>::new());
        // (Two result formats for the two-column lists.)
        let two = cols.matches(',').count() == 1;
        for formats in [&[1][..], &[0], &[1, 0], &[0, 1]].into_iter().filter(|f| f.len() == 1 || two) {
            for v in ["1", "5", "295", "1", "290"] {
                let mut got = Vec::new();
                for stmt in [format!("s{i}"), format!("l{i}")] {
                    c.bind_with("", &stmt, &[v], formats);
                    c.execute("", 0);
                    got.push(c.sync_messages());
                }
                assert_eq!(got[0], got[1], "{q} ({v}, formats {formats:?})");
                assert!(got[0].iter().any(|(t, _)| *t == b'D'), "{q} ({v}): rows");
            }
        }
    }
    // (Strings converted to the client's encoding: decoded.)
    c.query("SET client_encoding = 'LATIN1'");
    for i in [3, 6, 7] {
        for v in ["1", "290", "1"] {
            let mut got = Vec::new();
            for stmt in [format!("s{i}"), format!("l{i}")] {
                c.bind_with("", &stmt, &[v], &[0]);
                c.execute("", 0);
                got.push(c.sync_messages());
            }
            assert_eq!(got[0], got[1], "{} ({v}, LATIN1)", lists[i]);
        }
    }
    s.engine.shutdown();
}

/// A prepared unique-key lookup ends at its probe's row, the row sent from its stored values
/// (text, varchar and date in binary too): every DataRow and CommandComplete the same as the
/// same lookup's under a LIMIT, which decodes its row, in binary, text and mixed formats; for
/// a missing key, an updated, a deleted and a NULL-valued row, a non-ASCII string, a value
/// longer than a stack row, an Execute with a row limit and an int8 parameter.
#[test]
fn point_lookups_go_out_as_decoded_ones() {
    use std::sync::atomic::Ordering::Relaxed;
    let s = server(AuthMethod::Trust);
    let mut c = raw::Raw::connect(&s.addr, "postgres");
    c.query(
        "CREATE TABLE pk (id int PRIMARY KEY, owner int NOT NULL, t text, v varchar(20), d date, n int8);
         INSERT INTO pk SELECT g, g % 7, 'note ' || g, 'v' || g, date '2024-01-01' + g, CASE WHEN g % 5 = 0 THEN NULL ELSE -g END
           FROM generate_series(1, 5000) g;
         UPDATE pk SET t = 'grüße ' || id, d = NULL WHERE id = 7;
         UPDATE pk SET t = repeat('x', 300) WHERE id = 8;
         DELETE FROM pk WHERE id = 9;
         ANALYZE pk",
    );
    let before = pg_engine::exec::scan::POINT_PICKS.load(Relaxed);
    for (i, cols) in ["id, owner, t, v, d, n", "t, id", "d", "n, v"].iter().enumerate() {
        for (j, ty) in ["", "::int8"].iter().enumerate() {
            let q = format!("SELECT {cols} FROM pk WHERE id = $1{ty}");
            c.parse(&format!("p{i}{j}"), &q);
            c.parse(&format!("l{i}{j}"), &format!("SELECT {cols} FROM pk WHERE id = $1{ty} LIMIT 1000"));
            assert_eq!(c.sync(), Vec::<String>::new());
            let two = cols.matches(',').count() == 1;
            for formats in [&[1][..], &[0], &[1, 0], &[0, 1]].into_iter().filter(|f| f.len() == 1 || two) {
                for v in ["1", "5", "7", "8", "9", "400", "10", "1"] {
                    for max_rows in [0, 1] {
                        let mut got = Vec::new();
                        for stmt in [format!("p{i}{j}"), format!("l{i}{j}")] {
                            c.bind_with("", &stmt, &[v], formats);
                            c.execute("", max_rows);
                            got.push(c.sync_messages());
                        }
                        assert_eq!(got[0], got[1], "{q} ({v}, formats {formats:?}, max_rows {max_rows})");
                    }
                }
            }
        }
    }
    // Rows the open transaction wrote (its update of a row, a key it inserted), and a row
    // with a value stored out of line (the table's rows then fetched as stored).
    let same = |c: &mut raw::Raw, v: &str, what: &str| {
        let mut got = Vec::new();
        for stmt in ["p00", "l00", "p10", "l10"] {
            c.bind_with("", stmt, &[v], &[1]);
            c.execute("", 0);
            got.push(c.sync_messages());
        }
        assert_eq!(got[0], got[1], "{what} ({v})");
        assert_eq!(got[2], got[3], "{what} ({v}), t first");
    };
    c.query("BEGIN; UPDATE pk SET t = 'mine', n = 1 WHERE id = 11; INSERT INTO pk VALUES (6001, 1, 'new', 'v', NULL, 2)");
    for v in ["11", "6001", "12"] {
        same(&mut c, v, "in a transaction that wrote");
    }
    c.query("ROLLBACK");
    c.query("UPDATE pk SET t = (SELECT string_agg(md5(g::text), '') FROM generate_series(1, 300) g) WHERE id = 12");
    for v in ["12", "13", "9", "6001"] {
        same(&mut c, v, "a value out of line");
    }
    assert!(pg_engine::exec::scan::POINT_PICKS.load(Relaxed) > before + 60, "point lookups ended at their probe");
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

/// Missing unnamed and named statements have distinct errors in Bind and Describe.
#[test]
fn missing_unnamed_statement() {
    let s = server(AuthMethod::Trust);
    let mut c = raw::Raw::connect(&s.addr, "postgres");
    let errors = |c: &mut raw::Raw| {
        c.send(b'S', b"");
        let mut out = Vec::new();
        while let Some((t, b)) = c.next_within(Duration::from_secs(10)) {
            match t {
                b'E' => out.extend(
                    b.split(|c| *c == 0)
                        .filter(|f| matches!(f.first(), Some(b'C' | b'M')))
                        .map(|f| String::from_utf8_lossy(&f[1..]).into_owned()),
                ),
                b'Z' => break,
                _ => {}
            }
        }
        out
    };
    c.bind("", "");
    assert_eq!(errors(&mut c), ["26000", "unnamed prepared statement does not exist"]);
    c.describe(b'S', "");
    assert_eq!(errors(&mut c), ["26000", "unnamed prepared statement does not exist"]);
    c.bind("", "nope");
    assert_eq!(errors(&mut c), ["26000", "prepared statement \"nope\" does not exist"]);
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

        // Two tables of one name: the second waits for the first and fails once it commits.
        a.batch_execute("BEGIN; CREATE TABLE twice (x int)").await.unwrap();
        b.batch_execute("BEGIN").await.unwrap();
        let (r, ()) = tokio::join!(b.batch_execute("CREATE TABLE twice (y text)"), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            a.batch_execute("COMMIT").await.unwrap();
        });
        assert!(r.is_err());
        b.batch_execute("ROLLBACK").await.ok();
        let cols: i64 = a
            .query_one("SELECT count(*) FROM pg_attribute WHERE attrelid = 'twice'::regclass AND attnum > 0", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(cols, 1);
    });
    s.engine.shutdown();
}

/// Two sessions creating tables with foreign keys at once (Kysely's with-schema setup) never
/// give one oid to two catalog rows: a key's internal triggers take the oids right after the
/// constraint's, allocated as one block from the cluster's counter (another session's DDL
/// used to take one in between), and both tables then drop, at once too.
#[test]
fn concurrent_foreign_keys_take_distinct_oids() {
    let s = server(AuthMethod::Trust);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        let b = s.connect("postgres").await;
        let c = s.connect("postgres").await;
        let mk = |t: &str| {
            format!(
                "CREATE TABLE mammals.{t} (id serial PRIMARY KEY, name varchar(50) UNIQUE,
                 owner_id integer REFERENCES public.person(id) ON DELETE CASCADE)"
            )
        };
        let dup = "SELECT count(*) FROM (SELECT oid FROM (
            SELECT oid FROM pg_class UNION ALL SELECT oid FROM pg_constraint
            UNION ALL SELECT oid FROM pg_trigger UNION ALL SELECT oid FROM pg_type
            UNION ALL SELECT oid FROM pg_attrdef) o WHERE oid >= 16384 GROUP BY oid HAVING count(*) > 1) d";
        let (pet, staging) = (mk("pet"), mk("pet_staging"));
        for round in 0..20 {
            c.batch_execute(
                "DROP TABLE IF EXISTS mammals.pet, mammals.pet_staging, public.person;
                 DROP SCHEMA IF EXISTS mammals; CREATE TABLE public.person (id serial PRIMARY KEY);
                 CREATE SCHEMA mammals",
            )
            .await
            .unwrap();
            let (x, y) = tokio::join!(a.batch_execute(&pet), b.batch_execute(&staging));
            x.unwrap();
            y.unwrap();
            let n: i64 = c.query_one(dup, &[]).await.unwrap().get(0);
            assert_eq!(n, 0, "round {round}: oids shared by two catalog rows");
            let (x, y) = tokio::join!(
                a.batch_execute("DROP TABLE mammals.pet"),
                b.batch_execute("DROP TABLE mammals.pet_staging")
            );
            x.unwrap();
            y.unwrap();
            c.batch_execute("DROP TABLE public.person").await.unwrap();
        }
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

/// pg_stat_activity.query keeps track_activity_query_size - 1 bytes (1023 by default) of a
/// longer statement, cut at a character boundary (pgstat_report_activity, pg_mbcliplen).
#[test]
fn activity_query_clipped_to_track_activity_query_size() {
    let s = server(AuthMethod::Trust);
    runtime().block_on(async {
        let c = s.connect("postgres").await;
        // ASCII, then two-byte characters across the 1,023rd byte.
        let sql = format!(
            "SELECT octet_length(query), right(query, 3) FROM pg_stat_activity WHERE pid = pg_backend_pid() AND '{}{}' <> ''",
            "x".repeat(800),
            "é".repeat(100)
        );
        let pre = sql.find('é').unwrap();
        let want = pre + (1023 - pre) / 2 * 2;
        let rows = c.query(sql.as_str(), &[]).await.unwrap();
        assert_eq!(rows[0].get::<_, i32>(0) as usize, want);
        assert_eq!(rows[0].get::<_, String>(1), "ééé");
        let short = lines(&c, "SELECT query FROM pg_stat_activity WHERE pid = pg_backend_pid()").await.unwrap();
        assert_eq!(short, ["SELECT query FROM pg_stat_activity WHERE pid = pg_backend_pid()"]);
    });
    s.engine.shutdown();
}

/// A SELECT's rows go out as the executor makes them (printtup): the first while the
/// statement still runs, the rows before an error ahead of it, notices in order with them;
/// COPY TO's data too. What PostgreSQL runs to completion first (EXECUTE, RETURNING, a
/// data-modifying WITH) sends nothing before its error.
#[test]
fn rows_stream_as_the_executor_makes_them() {
    let s = server(AuthMethod::Trust);
    let mut c = raw::Raw::connect(&s.addr, "postgres");
    let wait = Duration::from_millis(1500);
    let slow = "SELECT g::text FROM generate_series(1, 2000) g UNION ALL SELECT pg_sleep(3)::text";
    for copy in [false, true] {
        let start = Instant::now();
        let sql = if copy { format!("COPY ({slow}) TO STDOUT\0") } else { format!("{slow}\0") };
        c.send(b'Q', sql.as_bytes());
        let first = if copy { b'H' } else { b'T' };
        assert_eq!(c.next_within(wait).map(|m| m.0), Some(first));
        let (t, b) = c.next_within(wait).expect("a row, before the statement ends");
        assert_eq!((t, b.ends_with(b"1\n") || b.ends_with(b"\x011")), (if copy { b'd' } else { b'D' }, true));
        assert!(start.elapsed() < Duration::from_secs(3));
        let rest = c.read();
        assert_eq!(rest.last().map(String::as_str), Some(if copy { "C COPY 2001" } else { "C SELECT 2001" }));
    }
    // The extended protocol's Execute.
    let start = Instant::now();
    c.parse("", slow);
    c.bind("", "");
    c.execute("", 0);
    c.send(b'S', b"");
    assert_eq!(c.next_within(wait).map(|m| m.0), Some(b'1'));
    assert_eq!(c.next_within(wait).map(|m| m.0), Some(b'2'));
    assert_eq!(c.next_within(wait).map(|m| m.0), Some(b'D'));
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(c.read().last().map(String::as_str), Some("C SELECT 2001"));

    let div = "SELECT 10 / (4 - i) FROM generate_series(1, 6) i";
    assert_eq!(c.query(div), ["T ?column?:23/-1", "D 3", "D 5", "D 10", "E 22012"]);
    assert_eq!(c.query("SELECT 1 / (i - 1) FROM generate_series(1, 3) i"), ["T ?column?:23/-1", "E 22012"]);
    c.parse("", div);
    c.bind("", "");
    c.execute("", 0);
    assert_eq!(c.sync(), ["D 3", "D 5", "D 10", "E 22012"]);
    assert_eq!(
        c.query(&format!("COPY ({div}) TO STDOUT")),
        ["H", "d", "d", "d", "E 22012"]
    );
    c.query("CREATE TABLE rs (a int)");
    assert_eq!(c.query(&format!("PREPARE rsp AS {div}; EXECUTE rsp")), ["C PREPARE", "E 22012"]);
    assert_eq!(c.query(&format!("INSERT INTO rs {div} RETURNING a")), ["E 22012"]);
    assert_eq!(c.query(&format!("WITH w AS (INSERT INTO rs VALUES (1) RETURNING a) {div}")), ["E 22012"]);
    assert_eq!(c.query("SELECT count(*) FROM rs"), ["T count:20/-1", "D 0", "C SELECT 1"]);
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

/// Types and settings clients read from the protocol (sqlx's EXPLAIN (FORMAT JSON), lib/pq's
/// startup options, tokio-postgres's domains), and COPY's NOT NULL failure as the row is
/// read (pgx's CopyFrom of a NULL into a NOT NULL column).
#[test]
fn described_types_startup_options_and_copy_not_null() {
    let s = server(AuthMethod::Trust);
    let mut c = raw::Raw::connect(&s.addr, "postgres");
    let wait = Duration::from_millis(1500);
    // ParameterDescription and RowDescription of a statement, as raw bodies.
    let describe = |c: &mut raw::Raw, sql: &str| -> (Vec<u8>, Vec<u8>) {
        c.parse("", sql);
        c.describe(b'S', "");
        c.send(b'S', b"");
        let (mut t, mut row) = (Vec::new(), Vec::new());
        while let Some((k, b)) = c.next_within(wait) {
            match k {
                b't' => t = b,
                b'T' => row = b,
                b'Z' => break,
                _ => {}
            }
        }
        (t, row)
    };
    let type_of = |row: &[u8]| {
        let e = 2 + row[2..].iter().position(|b| *b == 0).unwrap();
        u32::from_be_bytes(row[e + 7..e + 11].try_into().unwrap())
    };
    // ExplainResultDesc: json for FORMAT JSON, xml for XML.
    assert_eq!(type_of(&describe(&mut c, "EXPLAIN (FORMAT JSON) SELECT 1").1), 114);
    assert_eq!(type_of(&describe(&mut c, "EXPLAIN (FORMAT XML) SELECT 1").1), 142);
    assert_eq!(type_of(&describe(&mut c, "EXPLAIN SELECT 1").1), 25);
    assert_eq!(type_of(&describe(&mut c, "SELECT NULL::anyarray").1), 2277);
    // A suspended portal goes on under MOVE / FETCH (asyncpg's cursor.forward).
    c.query("BEGIN");
    c.parse("", "SELECT generate_series(0, 20)");
    c.bind("pm", "");
    c.execute("pm", 2);
    assert_eq!(c.sync(), ["D 0", "D 1", "s"]);
    assert_eq!(c.query("MOVE FORWARD 5 IN pm"), ["C MOVE 5"]);
    assert_eq!(c.query("FETCH 1 FROM pm"), ["T generate_series:23/-1", "D 7", "C FETCH 1"]);
    c.query("COMMIT");
    // EXPLAIN EXECUTE of a CALL prepared over the protocol: ExplainOneUtility.
    c.query("CREATE PROCEDURE p42(INOUT x int) LANGUAGE plpgsql AS $$BEGIN x := 42; END$$");
    c.parse("p42s", "CALL p42(null)");
    c.sync();
    assert_eq!(
        c.query("EXPLAIN EXECUTE p42s"),
        ["T QUERY PLAN:25/-1", "D Utility statements have no plan structure", "C EXPLAIN"]
    );
    // A parameter assigned to a domain column is of the domain.
    c.query("CREATE DOMAIN dpos AS int CHECK (VALUE > 0); CREATE TABLE dt (p dpos)");
    let r = c.query("SELECT 'dpos'::regtype::oid");
    let dom: u32 = r[1].strip_prefix("D ").unwrap().parse().unwrap();
    let mut want = 1i16.to_be_bytes().to_vec();
    want.extend_from_slice(&dom.to_be_bytes());
    assert_eq!(describe(&mut c, "INSERT INTO dt (p) VALUES ($1)").0, want);
    assert_eq!(describe(&mut c, "UPDATE dt SET p = $1").0, want);

    // COPY: the row with a NULL for a NOT NULL column fails as it is read.
    c.query("CREATE TABLE cnn (a int NOT NULL, b text)");
    c.send(b'Q', b"COPY cnn FROM STDIN\0");
    assert_eq!(c.next_within(wait).map(|m| m.0), Some(b'G'));
    c.send(b'd', b"1\tx\n\\N\ty\n");
    let (t, b) = c.next_within(wait).expect("the error, before CopyDone");
    assert_eq!(t, b'E');
    assert!(String::from_utf8_lossy(&b).contains("23502"));
    c.read();
    c.send(b'c', b"");
    assert_eq!(c.query("SELECT count(*) FROM cnn"), ["T count:20/-1", "D 0", "C SELECT 1"]);
    s.engine.shutdown();

    // process_startup_options: an explicit parameter wins over `options`' -c.
    let s = server(AuthMethod::Trust);
    runtime().block_on(async {
        let mut cfg = s.config("postgres", "secret");
        cfg.options("-c application_name=fromoptions -c search_path=x");
        cfg.application_name("explicit");
        let (c, conn) = cfg.connect(NoTls).await.unwrap();
        tokio::spawn(conn);
        let r = c
            .query_one("SELECT current_setting('application_name'), current_setting('search_path')", &[])
            .await
            .unwrap();
        assert_eq!(r.get::<_, String>(0), "explicit");
        assert_eq!(r.get::<_, String>(1), "x");
    });
    s.engine.shutdown();
}

/// ALTER TABLE ... DETACH PARTITION ... CONCURRENTLY: its first transaction commits the
/// partition's "detach pending" mark, after which other sessions' new snapshots leave the
/// partition out (a REPEATABLE READ snapshot taken before still has it), and its second
/// waits for every transaction holding a lock on the parent. Canceled there, the mark stays
/// (inserts aren't routed to the partition, another concurrent detach fails, ALTER TABLE on
/// the partition fails) until DETACH ... FINALIZE.
#[test]
fn detach_partition_concurrently() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        let b = s.connect("postgres").await;
        let c = s.connect("postgres").await;
        a.batch_execute(
            "CREATE TABLE dp (a int) PARTITION BY LIST (a);
             CREATE TABLE dp1 PARTITION OF dp FOR VALUES IN (1);
             CREATE TABLE dp2 PARTITION OF dp FOR VALUES IN (2);
             INSERT INTO dp VALUES (1), (2)",
        )
        .await
        .unwrap();
        let pa = lines(&a, "SELECT pg_backend_pid()").await.unwrap().remove(0);
        let pb = lines(&b, "SELECT pg_backend_pid()").await.unwrap().remove(0);
        // A's REPEATABLE READ snapshot and its lock on the parent predate the detach.
        a.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ").await.unwrap();
        assert_eq!(lines(&a, "SELECT a FROM dp ORDER BY a").await.unwrap(), ["1", "2"]);
        let detach = tokio::spawn(async move {
            let r = b.batch_execute("ALTER TABLE dp DETACH PARTITION dp2 CONCURRENTLY").await;
            (b, r)
        });
        let q = format!("SELECT pg_isolation_test_session_is_blocked({pb}, '{{{pa}}}')");
        let mut blocked = false;
        for _ in 0..200 {
            if lines(&c, &q).await.unwrap() == ["t"] {
                blocked = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(blocked, "the detach waits for the reader");
        // The mark is committed: new snapshots leave dp2 out, A's still has it.
        assert_eq!(lines(&c, "SELECT a FROM dp ORDER BY a").await.unwrap(), ["1"]);
        assert_eq!(
            lines(&c, "SELECT inhdetachpending FROM pg_inherits WHERE inhrelid = 'dp2'::regclass")
                .await
                .unwrap(),
            ["t"]
        );
        let err = c.batch_execute("INSERT INTO dp VALUES (2)").await.unwrap_err();
        assert_eq!(err.as_db_error().unwrap().message(), "no partition of relation \"dp\" found for row");
        assert_eq!(lines(&a, "SELECT a FROM dp ORDER BY a").await.unwrap(), ["1", "2"]);
        // Canceled while it waits: the partition stays pending detach.
        assert_eq!(lines(&c, &format!("SELECT pg_cancel_backend({pb})")).await.unwrap(), ["t"]);
        let (b, r) = detach.await.unwrap();
        assert_eq!(r.unwrap_err().code().map(|c| c.code()), Some("57014"));
        a.batch_execute("COMMIT").await.unwrap();
        let err = b
            .batch_execute("ALTER TABLE dp DETACH PARTITION dp1 CONCURRENTLY")
            .await
            .unwrap_err();
        let db = err.as_db_error().unwrap();
        assert_eq!(
            db.message(),
            "partition \"dp2\" already pending detach in partitioned table \"public.dp\""
        );
        assert_eq!(
            db.hint(),
            Some("Use ALTER TABLE ... DETACH PARTITION ... FINALIZE to complete the pending detach operation.")
        );
        let err = b.batch_execute("ALTER TABLE dp2 ADD COLUMN z int").await.unwrap_err();
        assert_eq!(
            err.as_db_error().unwrap().message(),
            "cannot alter partition \"dp2\" with an incomplete detach"
        );
        // FINALIZE completes it.
        b.batch_execute("ALTER TABLE dp DETACH PARTITION dp2 FINALIZE").await.unwrap();
        assert_eq!(
            lines(&b, "SELECT count(*) FROM pg_inherits WHERE inhrelid = 'dp2'::regclass")
                .await
                .unwrap(),
            ["0"]
        );
        assert_eq!(lines(&b, "SELECT a FROM dp2").await.unwrap(), ["2"]);
        let err = b
            .batch_execute("ALTER TABLE dp DETACH PARTITION dp1 FINALIZE")
            .await
            .unwrap_err();
        assert_eq!(
            err.as_db_error().unwrap().message(),
            "cannot complete detaching partition \"dp1\""
        );
        // Uncontended, both transactions run straight through.
        b.batch_execute("ALTER TABLE dp DETACH PARTITION dp1 CONCURRENTLY").await.unwrap();
        assert!(lines(&c, "SELECT a FROM dp").await.unwrap().is_empty());
        assert_eq!(
            lines(&c, "SELECT relname, relispartition FROM pg_class WHERE relname IN ('dp1', 'dp2') ORDER BY 1")
                .await
                .unwrap(),
            ["dp1|f", "dp2|f"]
        );
    });
    s.engine.shutdown();
}

/// A cancel while a SELECT's rows or COPY TO's data stream out stops them, whichever node
/// makes them (generate_series checks for no interrupts of its own), as the executor's
/// CHECK_FOR_INTERRUPTS does: psycopg's test_copy_out_error_with_copy_not_finished.
#[test]
fn cancel_while_rows_stream() {
    let s = server(AuthMethod::Trust);
    runtime().block_on(async {
        let c = s.connect("postgres").await;
        for copy in [true, false] {
            let token = c.cancel_token();
            let sql = "SELECT g FROM generate_series(1, 20000000) g";
            let r: Result<(), tokio_postgres::Error> = async {
                if copy {
                    let st = c.copy_out(&format!("COPY ({sql}) TO STDOUT")).await?;
                    futures::pin_mut!(st);
                    st.next().await.expect("data")?;
                    token.cancel_query(NoTls).await?;
                    while let Some(d) = st.next().await {
                        d?;
                    }
                } else {
                    let st = c.simple_query_raw(sql).await?;
                    futures::pin_mut!(st);
                    st.next().await.expect("a row")?;
                    token.cancel_query(NoTls).await?;
                    while let Some(m) = st.next().await {
                        m?;
                    }
                }
                Ok(())
            }
            .await;
            assert_eq!(r.unwrap_err().code().map(|c| c.code()), Some("57014"), "copy {copy}");
        }
    });
    s.engine.shutdown();
}

/// Catalog rows locked as Postgres's catalog tuples are: GRANT updates the relation's
/// pg_class row (waiting for SELECT ... FOR UPDATE of it), an index build's in-place update
/// of it waits for the GRANT, two UPDATEs of pg_class serialize (the second over the
/// first's result), and a second DROP OWNED waits for the first and then finds nothing left.
#[test]
fn catalog_row_locks() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        let b = s.connect("postgres").await;
        let c = s.connect("postgres").await;
        a.batch_execute(
            "CREATE TABLE crl (c int);
             CREATE ROLE regress_crl_role; CREATE ROLE regress_crl_member;
             CREATE ROLE regress_crl_grantor;
             GRANT regress_crl_role TO regress_crl_grantor WITH ADMIN OPTION;
             SET ROLE regress_crl_grantor; GRANT regress_crl_role TO regress_crl_member; RESET ROLE",
        )
        .await
        .unwrap();
        let pa = lines(&a, "SELECT pg_backend_pid()").await.unwrap().remove(0);
        let pb = lines(&b, "SELECT pg_backend_pid()").await.unwrap().remove(0);
        // FOR UPDATE of the pg_class row holds off GRANT; FOR KEY SHARE doesn't.
        a.batch_execute("BEGIN; SELECT relhasindex FROM pg_class WHERE oid = 'crl'::regclass FOR KEY SHARE")
            .await
            .unwrap();
        b.batch_execute("GRANT SELECT ON crl TO PUBLIC").await.unwrap();
        a.batch_execute("ROLLBACK; BEGIN; SELECT relhasindex FROM pg_class WHERE oid = 'crl'::regclass FOR UPDATE")
            .await
            .unwrap();
        let w = tokio::spawn(async move {
            let r = b.batch_execute("BEGIN; REVOKE SELECT ON crl FROM PUBLIC").await;
            (b, r)
        });
        until_blocked(&c, &pb).await;
        a.batch_execute("ROLLBACK").await.unwrap();
        let (b, r) = w.await.unwrap();
        r.unwrap();
        // The REVOKE in progress: ADD PRIMARY KEY's in-place update of relhasindex waits.
        let w = tokio::spawn(async move {
            let r = a.batch_execute("ALTER TABLE crl ADD PRIMARY KEY (c)").await;
            (a, r)
        });
        until_blocked(&c, &pa).await;
        b.batch_execute("COMMIT").await.unwrap();
        let (a, r) = w.await.unwrap();
        r.unwrap();
        assert_eq!(
            lines(&c, "SELECT relhasindex, relacl IS NULL OR relacl::text NOT LIKE '%=r/%' FROM pg_class WHERE oid = 'crl'::regclass")
                .await
                .unwrap(),
            ["t|t"]
        );
        // UPDATE pg_class twice at once: the second applies to the first's result.
        a.batch_execute("BEGIN; UPDATE pg_class SET reltuples = 123 WHERE oid = 'crl'::regclass")
            .await
            .unwrap();
        let w = tokio::spawn(async move {
            let r = b
                .batch_execute("UPDATE pg_class SET reltuples = reltuples * 2 WHERE oid = 'crl'::regclass")
                .await;
            (b, r)
        });
        until_blocked(&c, &pb).await;
        a.batch_execute("COMMIT").await.unwrap();
        let (b, r) = w.await.unwrap();
        r.unwrap();
        assert_eq!(
            lines(&c, "SELECT reltuples FROM pg_class WHERE oid = 'crl'::regclass").await.unwrap(),
            ["246"]
        );
        // DROP OWNED twice at once: the second waits, then has nothing to do.
        a.batch_execute("BEGIN; DROP OWNED BY regress_crl_grantor").await.unwrap();
        let w = tokio::spawn(async move {
            let r = b.batch_execute("DROP OWNED BY regress_crl_grantor").await;
            (b, r)
        });
        until_blocked(&c, &pb).await;
        a.batch_execute("COMMIT").await.unwrap();
        let (b, r) = w.await.unwrap();
        r.unwrap();
        b.batch_execute(
            "DROP ROLE regress_crl_member; DROP ROLE regress_crl_grantor; DROP ROLE regress_crl_role",
        )
        .await
        .unwrap();
    });
    s.engine.shutdown();
}

/// What a statement finds once a lock it waited for is granted: a row routed to a default
/// partition is checked against that partition's constraint as an ATTACH left it, an
/// inheritance child dropped meanwhile is left out, a foreign key added by ATTACH sees the
/// referenced rows a delete it waited for removed; and temporary objects go quietly at
/// DISCARD TEMP and when the session ends.
#[test]
fn after_waits_for_ddl() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        let b = s.connect("postgres").await;
        let c = s.connect("postgres").await;
        a.batch_execute(
            "CREATE TABLE tp (i int, j text) PARTITION BY RANGE (i);
             CREATE TABLE tp_def PARTITION OF tp DEFAULT;
             CREATE TABLE tp_2 (LIKE tp);
             CREATE TABLE ip (a int); INSERT INTO ip VALUES (1);
             CREATE TABLE ic () INHERITS (ip); INSERT INTO ic VALUES (10);
             CREATE TABLE ppk (a int PRIMARY KEY) PARTITION BY LIST (a);
             CREATE TABLE ppk1 PARTITION OF ppk FOR VALUES IN (1);
             INSERT INTO ppk VALUES (1);
             CREATE TABLE pfk (a int REFERENCES ppk) PARTITION BY LIST (a);
             CREATE TABLE pfk1 (a int NOT NULL); INSERT INTO pfk1 VALUES (1)",
        )
        .await
        .unwrap();
        let pb = lines(&b, "SELECT pg_backend_pid()").await.unwrap().remove(0);
        // ATTACH takes 100..200 from the default partition while the insert waits.
        a.batch_execute("BEGIN; ALTER TABLE tp ATTACH PARTITION tp_2 FOR VALUES FROM (100) TO (200)")
            .await
            .unwrap();
        let w = tokio::spawn(async move {
            let r = b.batch_execute("INSERT INTO tp VALUES (110, 'x')").await;
            (b, r)
        });
        until_blocked(&c, &pb).await;
        a.batch_execute("COMMIT").await.unwrap();
        let (b, r) = w.await.unwrap();
        let err = r.unwrap_err();
        assert_eq!(err.code().map(|c| c.code()), Some("23514"));
        assert_eq!(
            err.as_db_error().unwrap().message(),
            "new row for relation \"tp_def\" violates partition constraint"
        );
        assert_eq!(lines(&c, "SELECT count(*) FROM tp").await.unwrap(), ["0"]);
        // A child dropped while the query waited for it is left out.
        a.batch_execute("BEGIN; DROP TABLE ic").await.unwrap();
        let w = tokio::spawn(async move {
            let r = lines(&b, "SELECT sum(a) FROM ip").await;
            (b, r)
        });
        until_blocked(&c, &pb).await;
        a.batch_execute("COMMIT").await.unwrap();
        let (b, r) = w.await.unwrap();
        assert_eq!(r.unwrap(), ["1"]);
        // ATTACH of a partition with a foreign key waits for the referenced partition's
        // writer, then checks against what it committed.
        a.batch_execute("BEGIN; DELETE FROM ppk1 WHERE a = 1").await.unwrap();
        let w = tokio::spawn(async move {
            let r = b.batch_execute("ALTER TABLE pfk ATTACH PARTITION pfk1 FOR VALUES IN (1)").await;
            (b, r)
        });
        until_blocked(&c, &pb).await;
        a.batch_execute("COMMIT").await.unwrap();
        let (b, r) = w.await.unwrap();
        assert_eq!(r.unwrap_err().code().map(|c| c.code()), Some("23503"));
        // DISCARD TEMP drops temporary functions too, without notices; a session's
        // temporary objects are gone once it ends.
        b.batch_execute(
            "CREATE TEMP TABLE tt (id serial PRIMARY KEY);
             CREATE FUNCTION pg_temp.tf() RETURNS int LANGUAGE sql AS 'SELECT 1';
             CREATE FUNCTION uses_tt(tt) RETURNS int LANGUAGE sql AS 'SELECT 1'",
        )
        .await
        .unwrap();
        let schema = lines(&b, "SELECT pg_my_temp_schema()::regnamespace").await.unwrap().remove(0);
        b.batch_execute("DISCARD TEMP").await.unwrap();
        let left = format!(
            "SELECT (SELECT count(*) FROM pg_class WHERE relnamespace = '{schema}'::regnamespace)
                  + (SELECT count(*) FROM pg_proc WHERE pronamespace = '{schema}'::regnamespace
                     OR proname = 'uses_tt')"
        );
        assert_eq!(lines(&c, &left).await.unwrap(), ["0"]);
        b.batch_execute(
            "CREATE TEMP TABLE tt2 (x int);
             CREATE FUNCTION pg_temp.tf2() RETURNS int LANGUAGE sql AS 'SELECT 1'",
        )
        .await
        .unwrap();
        drop(b);
        let mut n = Vec::new();
        for _ in 0..200 {
            n = lines(&c, &left).await.unwrap();
            if n == ["0"] {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(n, ["0"]);
    });
    s.engine.shutdown();
}

/// CREATE INDEX CONCURRENTLY waits for transactions holding snapshots older than its
/// validation's, whatever they read (WaitForOlderSnapshots); REINDEX CONCURRENTLY waits for
/// the table's lockers again before the old index goes.
#[test]
fn concurrent_index_builds_wait_for_old_snapshots() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        let b = s.connect("postgres").await;
        let c = s.connect("postgres").await;
        a.batch_execute("CREATE TABLE cic1 (x int); CREATE TABLE cic2 (y int)").await.unwrap();
        let pb = lines(&b, "SELECT pg_backend_pid()").await.unwrap().remove(0);
        a.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT * FROM cic2").await.unwrap();
        let w = tokio::spawn(async move {
            let r = b.batch_execute("CREATE INDEX CONCURRENTLY cic1_x ON cic1 ((x + 1))").await;
            (b, r)
        });
        until_blocked(&c, &pb).await;
        a.batch_execute("COMMIT").await.unwrap();
        let (b, r) = w.await.unwrap();
        r.unwrap();
        // A reader holding the table before the swap, and a writer coming in after.
        a.batch_execute("BEGIN; SELECT * FROM cic1").await.unwrap();
        let w = tokio::spawn(async move {
            let r = b.batch_execute("REINDEX TABLE CONCURRENTLY cic1").await;
            (b, r)
        });
        until_blocked(&c, &pb).await;
        c.batch_execute("BEGIN; INSERT INTO cic1 VALUES (1)").await.unwrap();
        a.batch_execute("COMMIT").await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!w.is_finished(), "REINDEX waits for the writer that came in");
        c.batch_execute("COMMIT").await.unwrap();
        let (_b, r) = w.await.unwrap();
        r.unwrap();
    });
    s.engine.shutdown();
}

/// Client suites, round 3 (agent 66): a truncated application_name's NOTICE at startup
/// (tokio-postgres `notices`), pg_database's first row, an empty array received in binary
/// (array_recv: no dimensions; sqlx), pg_class / pg_description DML over the extended
/// protocol (pgjdbc).
#[test]
fn startup_notice_empty_arrays_and_catalog_dml() {
    let s = server(AuthMethod::Trust);
    runtime().block_on(async {
        let mut cfg = s.config("postgres", "");
        cfg.application_name("x".repeat(65));
        let (c, mut conn) = cfg.connect(NoTls).await.unwrap();
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        tokio::spawn(async move {
            let mut st = futures::stream::poll_fn(move |cx| conn.poll_message(cx));
            while let Some(Ok(m)) = st.next().await {
                if let AsyncMessage::Notice(n) = m {
                    let _ = tx.unbounded_send(n.message().to_string());
                }
            }
        });
        let name: String = c.query_one("SELECT current_setting('application_name')", &[]).await.unwrap().get(0);
        assert_eq!(name.len(), 63);
        assert_eq!(
            rx.next().await.unwrap(),
            format!("identifier \"{}\" will be truncated to \"{}\"", "x".repeat(65), "x".repeat(63))
        );
        let first: String = c.query("SELECT datname FROM pg_database", &[]).await.unwrap()[0].get(0);
        assert_eq!(first, "postgres");
        let empty: Vec<i32> = Vec::new();
        let row = c
            .query_one("SELECT array_ndims($1::int4[]), $1::int4[] = '{}'", &[&empty])
            .await
            .unwrap();
        assert_eq!(row.get::<_, Option<i32>>(0), None);
        assert!(row.get::<_, bool>(1));
        // pgjdbc: an updatable result set writes relname back; a comment moved by objoid.
        let st = c.prepare("UPDATE pg_class SET \"relname\" = $1 WHERE \"oid\" = $2").await.unwrap();
        assert_eq!(st.params(), &[tokio_postgres::types::Type::NAME, tokio_postgres::types::Type::OID]);
        assert!(st.columns().is_empty());
        assert_eq!(c.execute(&st, &[&"pg_class", &1259u32]).await.unwrap(), 1);
        c.batch_execute(
            "CREATE TABLE d (x int); COMMENT ON TABLE d IS 'table'; \
             CREATE FUNCTION f() RETURNS int LANGUAGE sql AS 'SELECT 1'; COMMENT ON FUNCTION f() IS 'function'",
        )
        .await
        .unwrap();
        let sql = "UPDATE pg_description SET objoid = 'd'::regclass WHERE objoid = 'f'::regproc";
        assert_eq!(c.execute(sql, &[]).await.unwrap(), 1);
        let rows = c
            .query(
                "SELECT classoid::regclass::text, description FROM pg_description WHERE objoid = 'd'::regclass ORDER BY 2",
                &[],
            )
            .await
            .unwrap();
        let got: Vec<(String, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
        assert_eq!(got, [("pg_proc".into(), "function".into()), ("pg_class".into(), "table".into())]);
    });
    s.engine.shutdown();
}

/// What monitoring tools read (postgres_exporter, pgAdmin, pg_activity, pgwatch): a lock wait
/// in another database as pg_stat_activity (wait events, transaction start), pg_locks and
/// pg_blocking_pids show it; pg_stat_database of a database that closed while idle; and the
/// cluster's WAL position, one for every database, growing as pg_stat_wal's bytes do and
/// going on after a restart.
#[test]
fn monitoring_views() {
    let s = server(AuthMethod::ScramSha256);
    let lsn_before_restart = runtime().block_on(async {
        let a = s.connect("postgres").await;
        a.batch_execute("CREATE DATABASE other").await.unwrap();
        let b = s.connect("other").await;
        b.batch_execute("CREATE TABLE t (a int); INSERT INTO t VALUES (1)").await.unwrap();
        // One WAL position for the cluster, moving by the WAL bytes written (as pg_stat_wal
        // counts them once the writers report).
        let flush = "SELECT pg_stat_force_next_flush(); SELECT 1";
        a.batch_execute(flush).await.unwrap();
        b.batch_execute(flush).await.unwrap();
        let q = "SELECT pg_current_wal_lsn(), (SELECT wal_bytes FROM pg_stat_wal)";
        let before = lines(&a, q).await.unwrap().remove(0);
        let (lsn, bytes) = before.split_once('|').unwrap();
        assert_eq!(lines(&b, "SELECT pg_current_wal_lsn()").await.unwrap(), [lsn]);
        b.batch_execute("INSERT INTO t SELECT generate_series(1, 100)").await.unwrap();
        let lsn_moved = format!("SELECT pg_current_wal_lsn() > '{lsn}'");
        assert_eq!(lines(&a, &lsn_moved).await.unwrap(), ["t"]);
        b.batch_execute(flush).await.unwrap();
        let moved = format!(
            "SELECT pg_wal_lsn_diff(pg_current_wal_lsn(), '{lsn}') = (SELECT wal_bytes FROM pg_stat_wal) - {bytes}"
        );
        assert_eq!(lines(&a, &moved).await.unwrap(), ["t"]);
        // A lock wait in database "other", seen from "postgres".
        b.batch_execute("SET application_name = 'holder'; BEGIN; LOCK TABLE t").await.unwrap();
        let w = s.connect("other").await;
        let pw = lines(&w, "SELECT pg_backend_pid()").await.unwrap().remove(0);
        let pb = lines(&b, "SELECT pg_backend_pid()").await.unwrap().remove(0);
        let waiter = tokio::spawn(async move {
            let r = lines(&w, "SELECT count(*) FROM t").await;
            (w, r)
        });
        until_blocked(&a, &pw).await;
        let activity = format!(
            "SELECT pid, state, wait_event_type, wait_event, xact_start IS NOT NULL, \
             backend_xmin IS NOT NULL FROM pg_stat_activity WHERE pid IN ({pw}, {pb}) \
             ORDER BY application_name"
        );
        assert_eq!(
            lines(&a, &activity).await.unwrap(),
            [
                format!("{pw}|active|Lock|relation|t|t"),
                format!("{pb}|idle in transaction|Client|ClientRead|t|f")
            ]
        );
        let blocking = format!("SELECT pg_blocking_pids({pw})");
        assert_eq!(lines(&a, &blocking).await.unwrap(), [format!("{{{pb}}}")]);
        let locks = format!("SELECT mode, granted FROM pg_locks WHERE pid = {pw} AND locktype = 'relation'");
        assert_eq!(lines(&a, &locks).await.unwrap(), ["AccessShareLock|f"]);
        b.batch_execute("COMMIT").await.unwrap();
        let (w, r) = waiter.await.unwrap();
        assert_eq!(r.unwrap(), ["101"]);
        // pg_stat_database's counts go on across the database closing while idle.
        let other = "SELECT xact_commit > 0, numbackends FROM pg_stat_database WHERE datname = 'other'";
        assert_eq!(lines(&a, other).await.unwrap(), ["t|2"]);
        let xacts = "SELECT xact_commit FROM pg_stat_database WHERE datname = 'other'";
        let seen = lines(&a, xacts).await.unwrap();
        drop((b, w));
        let start = Instant::now();
        while s.engine.open_databases() > 1 {
            assert!(start.elapsed() < Duration::from_secs(20), "database never closed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // (The sessions' last counts were flushed as they ended.)
        let after: i64 = lines(&a, xacts).await.unwrap()[0].parse().unwrap();
        assert!(after >= seen[0].parse::<i64>().unwrap(), "{after} after {seen:?}");
        lines(&a, "SELECT pg_current_wal_lsn()").await.unwrap().remove(0)
    });
    s.engine.shutdown();
    // After a restart, the position is past where it was.
    let opts = EngineOptions {
        data_dir: s._dir.path().to_path_buf(),
        cache_bytes: 4 << 20,
        memtable_bytes: 1 << 20,
        sync: false,
        password: Some("secret".into()),
        ..EngineOptions::default()
    };
    let engine = Engine::open(opts).unwrap();
    let mut session = engine.connect(1, "postgres", "postgres", "").unwrap();
    let q = format!("SELECT pg_current_wal_lsn() > '{lsn_before_restart}'");
    let r = session.query(&q).unwrap();
    assert_eq!(format!("{:?}", r[0].rows[0]), "[Bool(true)]");
    drop(session);
    engine.shutdown();
}

#[test]
fn semi_join_right_side_not_a_parameter() {
    // Hibernate's BulkManipulationTest.testUpdateWithWhereExistsSubquery (pgjdbc: Parse /
    // Bind / Execute, a named statement once executed often): an EXISTS pulled up as a semi
    // join of one of three inner-joined tables. The join search took a column of the semi
    // join's right side (Human_friends.human1, equated with Human.mammal) as the parameter of
    // an index scan above it, which the engine's semi join doesn't return ("column #32 out of
    // range").
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let c = s.connect("postgres").await;
        c.batch_execute(
            "CREATE TABLE Animal (id bigint PRIMARY KEY, description varchar(255), body_weight float4, \
               mother_id bigint REFERENCES Animal, father_id bigint REFERENCES Animal, zoo_id bigint, \
               serialNumber varchar(255)); \
             CREATE TABLE Mammal (animal bigint PRIMARY KEY REFERENCES Animal, pregnant boolean, \
               birthdate date, mammalZoo_id bigint, name varchar(255)); \
             CREATE TABLE Human (mammal bigint PRIMARY KEY REFERENCES Mammal, name_first varchar(255), \
               name_initial char(1), name_last varchar(255), nickName varchar(255), \
               height_centimeters float(53) NOT NULL, intValue integer, floatValue float4, \
               bigDecimalValue numeric(38,2), bigIntegerValue numeric(38,0)); \
             CREATE TABLE Human_friends (human1 bigint REFERENCES Human, human2 bigint NOT NULL REFERENCES Human); \
             INSERT INTO Animal (id, description) VALUES (1, 'a'), (2, 'b'), (3, 'c'); \
             INSERT INTO Mammal (animal) VALUES (1), (2), (3); \
             INSERT INTO Human (mammal, name_last, height_centimeters) \
               VALUES (1, 'Public', 1), (2, 'Doe', 2), (3, 'Public', 3); \
             INSERT INTO Human_friends VALUES (1, 3), (2, 1), (3, 1)",
        )
        .await
        .unwrap();
        let sql = "with id_cte (id) as materialized (select h1_0.mammal from Human h1_0 \
            join Animal h1_1 on h1_0.mammal=h1_1.id join Mammal h1_2 on h1_0.mammal=h1_2.animal \
            where exists(select f1_0.human2 from Human_friends f1_0 join Human f1_1 on f1_1.mammal=f1_0.human2 \
            where f1_1.name_last=$1 and h1_0.mammal=f1_0.human1)), \
            update_cte_Animal (id) as (update Animal h1_1 set description='updated' \
            where h1_1.id in (select id.id from id_cte id) returning id) select count(*) from id_cte id";
        let st = c.prepare(sql).await.unwrap();
        for _ in 0..7 {
            let n: i64 = c.query_one(&st, &[&"Public"]).await.unwrap().get(0);
            assert_eq!(n, 3);
        }
        let n: i64 = c.query_one(sql.replace("$1", "'Public'").as_str(), &[]).await.unwrap().get(0);
        assert_eq!(n, 3);
        let updated: i64 = c
            .query_one("SELECT count(*) FROM Animal WHERE description = 'updated'", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(updated, 3);
    });
    s.engine.shutdown();
}

/// PortalRun: a RETURNING statement (or one that writes in WITH) runs to its end on its
/// portal's first Execute, whatever the row count (FillPortalStore); the Executes hand out
/// the rows it holds, suspended when they reach the count (PortalRunSelect doesn't look
/// ahead) and done with no rows once they're out. A statement without rows runs once
/// (MarkPortalDone), and so does one that failed: Execute again is refused.
#[test]
fn portals_hold_returning_rows_and_run_once() {
    let s = server(AuthMethod::Trust);
    let mut c = raw::Raw::connect(&s.addr, "postgres");
    c.query("CREATE TABLE hp (id int, title text)");
    c.query("BEGIN");
    c.parse("ins", "INSERT INTO hp (title) VALUES ('x'), ('y') RETURNING title");
    c.bind("", "ins");
    c.execute("", 2);
    c.execute("", 2);
    c.execute("", 0);
    assert_eq!(c.sync(), ["D x", "D y", "s", "C INSERT 0 0", "C INSERT 0 0"]);
    c.bind("", "ins");
    c.execute("", 1);
    c.execute("", 0);
    assert_eq!(c.sync(), ["D x", "s", "D y", "C INSERT 0 1"]);
    c.bind("", "ins");
    c.execute("", 3);
    c.execute("", 1);
    assert_eq!(c.sync(), ["D x", "D y", "C INSERT 0 2", "C INSERT 0 0"]);
    assert_eq!(c.query("SELECT count(*) FROM hp"), ["T count:20/-1", "D 6", "C SELECT 1"]);
    c.parse("", "UPDATE hp SET id = 1 WHERE title = 'x' RETURNING title");
    c.bind("", "");
    c.execute("", 1);
    c.execute("", 0);
    assert_eq!(c.sync(), ["D x", "s", "D x", "D x", "C UPDATE 2"]);
    c.parse("", "DELETE FROM hp WHERE id = 1 RETURNING title");
    c.bind("", "");
    c.execute("", 0);
    c.execute("", 0);
    assert_eq!(c.sync(), ["D x", "D x", "D x", "C DELETE 3", "C DELETE 0"]);
    c.parse("", "WITH w AS (INSERT INTO hp VALUES (5, 'a') RETURNING title) SELECT * FROM w");
    c.bind("", "");
    c.execute("", 1);
    c.execute("", 1);
    assert_eq!(c.sync(), ["D a", "s", "C SELECT 0"]);
    c.parse("", "SHOW search_path");
    c.bind("", "");
    c.execute("", 1);
    c.execute("", 1);
    assert_eq!(c.sync(), ["D \"$user\", public", "s", "C SHOW"]);
    c.parse("", "INSERT INTO hp VALUES (9, 'z')");
    c.bind("pp", "");
    c.execute("pp", 0);
    assert_eq!(c.sync(), ["C INSERT 0 1"]);
    c.execute("pp", 0);
    assert_eq!(c.sync(), ["E 55000"]);
    c.query("ROLLBACK");
    // A portal whose run failed, after the rollback of the savepoint it failed in.
    c.query("BEGIN");
    c.query("INSERT INTO hp VALUES (5, 'f')");
    c.parse("", "SELECT 1/(id - 5) FROM hp");
    c.bind("pf", "");
    assert_eq!(c.sync(), Vec::<String>::new());
    c.query("SAVEPOINT s");
    c.execute("pf", 0);
    assert_eq!(c.sync(), ["E 22012"]);
    c.execute("pf", 0);
    assert_eq!(c.sync(), ["E 25P02"]);
    c.query("ROLLBACK TO s");
    c.execute("pf", 0);
    assert_eq!(c.sync(), ["E 55000"]);
    c.query("ROLLBACK");
    assert_eq!(c.query("SELECT count(*) FROM hp"), ["T count:20/-1", "D 0", "C SELECT 1"]);
    s.engine.shutdown();
}

/// A utility statement's ParameterDescription lists only the parameters of the query parse
/// analysis looks into (EXPLAIN's, CREATE TABLE AS's, DECLARE's): PREPARE ... AS and COPY
/// (query) have none (Ecto's explain with a fallback generic plan).
#[test]
fn utility_statements_describe_their_analyzed_parameters() {
    let s = server(AuthMethod::Trust);
    let mut c = raw::Raw::connect(&s.addr, "postgres");
    let mut params = |sql: &str| -> Vec<u8> {
        c.parse("", sql);
        c.describe(b'S', "");
        c.send(b'S', b"");
        let mut t = Vec::new();
        while let Some((k, b)) = c.next_within(Duration::from_millis(1500)) {
            match k {
                b't' => t = b,
                b'Z' => break,
                _ => {}
            }
        }
        t
    };
    let none = vec![0u8, 0];
    let int4 = vec![0u8, 1, 0, 0, 0, 23];
    assert_eq!(params("PREPARE ecto_explain_statement(unknown, unknown) AS SELECT $1::int + $2::int"), none);
    assert_eq!(params("COPY (SELECT $1::int) TO STDOUT"), none);
    assert_eq!(params("CREATE VIEW v AS SELECT $1::int AS a"), none);
    assert_eq!(params("EXPLAIN EXECUTE p($1)"), none);
    assert_eq!(params("CREATE TABLE t AS SELECT $1::int AS a"), int4);
    assert_eq!(params("SELECT $1::int AS a INTO t"), int4);
    assert_eq!(params("DECLARE c CURSOR FOR SELECT $1::int"), int4);
    assert_eq!(params("EXPLAIN SELECT $1::int"), int4);
    s.engine.shutdown();
}

/// What sqlc's managed-db replay reads from Parse / Describe: a sequence's columns come from
/// the sequence (RowDescription's table oid and attnum, so their attnotnull is found), and an
/// ON CONFLICT whose arbiter no constraint matches describes as analysis types it: the error
/// is the planner's, at execution (infer_arbiter_indexes).
#[test]
fn sequence_columns_and_unplanned_on_conflict_describe() {
    use tokio_postgres::types::Type;
    let s = server(AuthMethod::Trust);
    runtime().block_on(async {
        let c = s.connect("postgres").await;
        c.batch_execute("CREATE SEQUENCE my_sequence; CREATE TABLE authors (id bigserial PRIMARY KEY, name text NOT NULL, bio text)")
            .await
            .unwrap();
        let oid: u32 = c.query_one("SELECT 'my_sequence'::regclass::oid", &[]).await.unwrap().get(0);
        let st = c.prepare("SELECT last_value, is_called FROM my_sequence").await.unwrap();
        let cols: Vec<(u32, i16)> = st.columns().iter().map(|c| (c.table_oid().unwrap_or(0), c.column_id().unwrap_or(0))).collect();
        assert_eq!(cols, [(oid, 1), (oid, 3)]);
        let st = c
            .prepare("INSERT INTO authors (name, bio) VALUES ($1, $2) ON CONFLICT (name) DO UPDATE SET bio = $2 RETURNING id, bio")
            .await
            .unwrap();
        assert_eq!(st.params(), [Type::TEXT, Type::TEXT]);
        let types: Vec<Type> = st.columns().iter().map(|c| c.type_().clone()).collect();
        assert_eq!(types, [Type::INT8, Type::TEXT]);
        let e = c.query(&st, &[&"a", &"b"]).await.unwrap_err();
        assert_eq!(e.code().unwrap().code(), "42P10");
    });
    s.engine.shutdown();
}

/// Binary date / timestamp / timestamptz input takes what the output function can print, or
/// infinity (date_recv's IS_VALID_DATE, timestamp_recv's IS_VALID_TIMESTAMP).
#[test]
fn binary_datetime_input_in_range() {
    use tokio_postgres::types::Type;
    let s = server(AuthMethod::Trust);
    runtime().block_on(async {
        let c = s.connect("postgres").await;
        let code = |e: tokio_postgres::Error| e.code().unwrap().code().to_string();
        for ty in [Type::TIMESTAMP, Type::TIMESTAMPTZ] {
            let st = c.prepare_typed("SELECT $1 IS NOT NULL", &[ty]).await.unwrap();
            let at = |us: i64| Raw(us.to_be_bytes().to_vec());
            let min = -211_813_488_000_000_000i64;
            assert!(c.query_one(&st, &[&at(min)]).await.unwrap().get::<_, bool>(0));
            assert!(c.query_one(&st, &[&at(i64::MAX)]).await.unwrap().get::<_, bool>(0));
            assert_eq!(code(c.query_one(&st, &[&at(min - 1)]).await.unwrap_err()), "22008");
        }
        let st = c.prepare_typed("SELECT $1::text", &[Type::DATE]).await.unwrap();
        let day = |d: i32| Raw(d.to_be_bytes().to_vec());
        let text = |r: tokio_postgres::Row| r.get::<_, String>(0);
        assert_eq!(text(c.query_one(&st, &[&day(-2_451_545)]).await.unwrap()), "4714-11-24 BC");
        assert_eq!(text(c.query_one(&st, &[&day(i32::MIN)]).await.unwrap()), "-infinity");
        assert_eq!(code(c.query_one(&st, &[&day(-2_451_546)]).await.unwrap_err()), "22008");
    });
    s.engine.shutdown();
}

/// ProcessStartupPacket: the database and user names are cut to NAMEDATALEN - 1 bytes, so
/// an overlength name finds what CREATE truncated it to.
#[test]
fn startup_names_cut_to_namedatalen() {
    let s = server(AuthMethod::Trust);
    runtime().block_on(async {
        let c = s.connect("postgres").await;
        let (db, user) = ("d".repeat(66), "u".repeat(70));
        c.batch_execute(&format!("CREATE DATABASE \"{db}\"; CREATE ROLE \"{user}\" LOGIN"))
            .await
            .unwrap();
        let mut cfg = s.config(&db, "");
        cfg.user(&user);
        let (c, conn) = cfg.connect(NoTls).await.unwrap();
        tokio::spawn(conn);
        let r = c.query_one("SELECT current_database()::text, current_user::text", &[]).await.unwrap();
        assert_eq!((r.get::<_, String>(0), r.get::<_, String>(1)), ("d".repeat(63), "u".repeat(63)));
    });
    s.engine.shutdown();
}

/// A big value goes out at the speed of a copy: a custom plan's cost doesn't print its
/// constants (a parameter's value), and byteaout writes the whole value at once. 64 MB took
/// 30 s (binary) and longer as text in a debug build; now well under one.
#[test]
fn big_bytea_values_go_out_in_linear_time() {
    let s = server(AuthMethod::Trust);
    runtime().block_on(async {
        let c = s.connect("postgres").await;
        let big: Vec<u8> = (0..64usize << 20).map(|i| (i % 251) as u8).collect();
        let started = Instant::now();
        let st = c.prepare_typed("SELECT $1::bytea", &[tokio_postgres::types::Type::BYTEA]).await.unwrap();
        let back: Vec<u8> = c.query_one(&st, &[&big]).await.unwrap().get(0);
        assert!(back == big);
        let text = c.simple_query("SELECT decode(repeat('ff01', 8 << 20), 'hex')").await.unwrap();
        let hex = text
            .iter()
            .find_map(|m| match m {
                SimpleQueryMessage::Row(r) => r.get(0),
                _ => None,
            })
            .unwrap();
        assert_eq!((hex.len(), &hex[..8]), (2 + (32 << 20), "\\xff01ff"));
        assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
    });
    s.engine.shutdown();
}

/// port and listen_addresses are where the server listens (as `postgres -h -p` sets them), and
/// inet_server_* / inet_client_* are the connection's two ends (round 21, agent 97).
#[test]
fn listen_settings_and_connection_addresses() {
    let s = server(AuthMethod::ScramSha256);
    let port = s.addr.rsplit_once(':').unwrap().1.to_string();
    runtime().block_on(async {
        let c = s.connect("postgres").await;
        let q = "SELECT name, setting, source FROM pg_settings WHERE name IN ('port', 'listen_addresses') ORDER BY 1";
        assert_eq!(
            lines(&c, q).await.unwrap(),
            ["listen_addresses|127.0.0.1|command line".to_string(), format!("port|{port}|command line")]
        );
        assert_eq!(lines(&c, "SHOW port").await.unwrap(), std::slice::from_ref(&port));
        let q = "SELECT inet_server_addr(), inet_server_port(), inet_client_addr(), inet_client_port() > 0";
        assert_eq!(lines(&c, q).await.unwrap(), [format!("127.0.0.1|{port}|127.0.0.1|t")]);
        let q = "SELECT inet_client_port() = client_port FROM pg_stat_activity WHERE pid = pg_backend_pid()";
        assert_eq!(lines(&c, q).await.unwrap(), ["t"]);
    });
    s.engine.shutdown();
}

/// A prepared statement whose result row a later ALTER TABLE changed fails at Bind with
/// RevalidateCachedQuery's error, before the Bind's result formats (one per described column)
/// are checked against the new columns: pgx's statement cache drops the statement on it and
/// prepares it again (its TestStmtCacheInvalidationConn).
#[test]
fn cached_plan_result_change_before_result_formats() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let c = s.connect("postgres").await;
        c.batch_execute(
            "CREATE TABLE drop_cols (id serial PRIMARY KEY, f1 int NOT NULL, f2 int NOT NULL); INSERT INTO drop_cols (f1, f2) VALUES (1, 2)",
        )
        .await
        .unwrap();
        let st = c.prepare("SELECT * FROM drop_cols WHERE id = $1").await.unwrap();
        assert_eq!(c.query(&st, &[&1i32]).await.unwrap().len(), 1);
        c.batch_execute("ALTER TABLE drop_cols DROP COLUMN f1").await.unwrap();
        let e = c.query(&st, &[&1i32]).await.unwrap_err();
        let d = e.as_db_error().expect("db error");
        assert_eq!((d.code().code(), d.message()), ("0A000", "cached plan must not change result type"));
        // Prepared again, it runs over the new columns.
        let st = c.prepare("SELECT * FROM drop_cols WHERE id = $1").await.unwrap();
        assert_eq!(c.query(&st, &[&1i32]).await.unwrap()[0].len(), 2);
    });
    s.engine.shutdown();
}

/// A CancelRequest's connection closes once the canceled statement's error is on its way:
/// Postgrex closes its connection as soon as the cancel's closes, and reads the error first
/// (its "active client timeout"). Postgres's cancel is a signal its backend acts on while
/// the process that took the request is still exiting.
#[test]
fn cancel_request_closes_after_the_error() {
    let s = server(AuthMethod::Trust);
    let mut c = raw::Raw::connect(&s.addr, "postgres");
    let mut watch = raw::Raw::connect(&s.addr, "postgres");
    let sleeping = "SELECT count(*) FROM pg_stat_activity WHERE wait_event = 'PgSleep'";
    for _ in 0..3 {
        // Two statements in one pipeline: the cancel lands in the first, once it runs (a
        // cancel that comes while the backend waits for its client cancels nothing).
        c.parse("", "SELECT pg_sleep(30)");
        c.bind("", "");
        c.execute("", 0);
        c.parse("", "SELECT 1");
        c.bind("", "");
        c.execute("", 0);
        let deadline = Instant::now() + Duration::from_secs(20);
        while !watch.query(sleeping).contains(&"D 1".to_string()) {
            assert!(Instant::now() < deadline, "the statement never ran");
        }
        let start = Instant::now();
        c.cancel(&s.addr);
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(c.has_input(), "the error comes before the cancel's connection closes");
        assert_eq!(c.sync(), ["E 57014"]);
    }
    assert_eq!(c.query("SELECT 1"), ["T ?column?:23/-1", "D 1", "C SELECT 1"]);
    s.engine.shutdown();
}

/// Bind with text parameters (NULL for None) and `result_formats`.
fn bind_text(c: &mut raw::Raw, params: &[Option<&str>], result_formats: &[i16]) {
    let mut b = b"\0\0".to_vec();
    b.extend_from_slice(&0i16.to_be_bytes());
    b.extend_from_slice(&(params.len() as i16).to_be_bytes());
    for p in params {
        match p {
            None => b.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(p) => {
                b.extend_from_slice(&(p.len() as i32).to_be_bytes());
                b.extend_from_slice(p.as_bytes());
            }
        }
    }
    b.extend_from_slice(&(result_formats.len() as i16).to_be_bytes());
    for f in result_formats {
        b.extend_from_slice(&f.to_be_bytes());
    }
    c.send(b'B', &b);
}

/// Difftest round 7 (agent 138), checked against PostgreSQL 18.6: a parameter of a domain
/// type is checked at Bind (domain_in), whether the statement reads it or not, and before the
/// result formats are; EXPLAIN with bound parameters plans them as constants (a custom plan:
/// partitions pruned and `= NULL` folded while planning).
#[test]
fn bound_parameters_of_domains_and_explain() {
    let s = server(AuthMethod::Trust);
    let mut c = raw::Raw::connect(&s.addr, "postgres");
    c.query("CREATE DOMAIN wd AS text CHECK (VALUE ~ '^[A-Z]$')");
    c.query("CREATE TABLE wp (id int, r text) PARTITION BY LIST (r)");
    c.query("CREATE TABLE wp_a PARTITION OF wp FOR VALUES IN ('a')");
    c.query("CREATE TABLE wp_b PARTITION OF wp FOR VALUES IN ('b')");
    for (sql, params, formats, want) in [
        ("SELECT $1::wd WHERE false", vec![Some("x")], vec![], vec!["E 23514"]),
        ("SELECT $1::wd", vec![Some("x")], vec![0, 0], vec!["E 23514"]),
        ("SELECT $1::wd", vec![Some("Q")], vec![], vec!["D Q", "C SELECT 1"]),
        (
            "EXPLAIN (COSTS OFF) SELECT * FROM wp WHERE r = $1",
            vec![Some("a")],
            vec![],
            vec!["D Seq Scan on wp_a wp", "D   Filter: (r = 'a'::text)", "C EXPLAIN"],
        ),
        (
            "EXPLAIN (COSTS OFF) SELECT * FROM wp WHERE r = $1::text",
            vec![None],
            vec![],
            vec!["D Result", "D   One-Time Filter: false", "C EXPLAIN"],
        ),
    ] {
        c.parse("", sql);
        bind_text(&mut c, &params, &formats);
        c.execute("", 0);
        assert_eq!(c.sync(), want, "{sql}");
    }
    s.engine.shutdown();
}

/// Difftest round 10 (agent 162), checked against PostgreSQL 18.6: a custom plan keeps a text
/// parameter's cast through a STABLE input routine and its selectivity estimate evaluates it,
/// so a bad value fails the Bind while planning, before the result formats are checked.
#[test]
fn custom_plan_stable_casts_fail_at_bind() {
    let s = server(AuthMethod::Trust);
    let mut c = raw::Raw::connect(&s.addr, "postgres");
    c.query("CREATE TABLE wcast (id int PRIMARY KEY, d timestamptz)");
    for (value, want) in [("2026-02-30", vec!["E 22008"]), ("2026-01-30", vec!["E 08P01"])] {
        c.parse("", "SELECT id FROM wcast WHERE d > $1::text::timestamptz");
        bind_text(&mut c, &[Some(value)], &[0, 0]);
        c.execute("", 0);
        assert_eq!(c.sync(), want, "{value}");
    }
    s.engine.shutdown();
}

/// Virtual transaction ids as PostgreSQL 18.6 numbers them (checked against it): a proc
/// array slot per backend, taken from the head of a free list its backend returns it to the
/// tail of, and local ids the slot counts on (the startup transaction 1, then one per
/// transaction, a Parse / Bind / Execute cycle's, VACUUM's per relation). pg_locks, the
/// temporary schema's name and pg_stat_get_backend_idset show them.
#[test]
fn virtual_transaction_ids() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        let first = own_vxid(&a).await;
        let (slot, lxid) = first.split_once('/').unwrap();
        assert_eq!(lxid, "2", "{first}");
        assert_eq!(own_vxid(&a).await, format!("{slot}/3"));
        // A transaction block is one; an extended-protocol cycle (Parse, Bind, Execute,
        // Sync) is one.
        a.batch_execute("BEGIN; SELECT 1").await.unwrap();
        assert_eq!(own_vxid(&a).await, format!("{slot}/4"));
        a.batch_execute("COMMIT").await.unwrap();
        a.query("SELECT 1", &[]).await.unwrap();
        assert_eq!(own_vxid(&a).await, format!("{slot}/7"));
        // VACUUM of one table: the statement's, the table's, the closing one.
        a.batch_execute("CREATE TABLE vx (a int)").await.unwrap();
        a.batch_execute("VACUUM vx").await.unwrap();
        assert_eq!(own_vxid(&a).await, format!("{slot}/12"));
        // Another backend takes the next slot; its transactions' locks show its id.
        let b = s.connect("postgres").await;
        let n: i32 = slot.parse().unwrap();
        assert_eq!(own_vxid(&b).await, format!("{}/2", n + 1));
        b.batch_execute("BEGIN; LOCK TABLE vx").await.unwrap();
        let seen = "SELECT DISTINCT virtualtransaction FROM pg_locks WHERE relation = 'vx'::regclass";
        assert_eq!(lines(&a, seen).await.unwrap(), [format!("{}/3", n + 1)]);
        b.batch_execute("COMMIT").await.unwrap();
        // pg_stat_get_backend_idset's ids are the slots; the temporary schema is the slot's.
        let pid = format!("SELECT pg_stat_get_backend_pid({}) = pg_backend_pid()", n + 1);
        assert_eq!(lines(&b, &pid).await.unwrap(), ["t"]);
        b.batch_execute("CREATE TEMP TABLE tt (a int)").await.unwrap();
        let temp = "SELECT nspname FROM pg_namespace WHERE oid = pg_my_temp_schema()";
        assert_eq!(lines(&b, temp).await.unwrap(), [format!("pg_temp_{}", n + 1)]);
        // A backend leaving puts its slot at the free list's tail: the next one takes a
        // slot not used yet, and the slot's ids go on when it is used again.
        drop(b);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let c = s.connect("postgres").await;
        assert_eq!(own_vxid(&c).await, format!("{}/2", n + 2));
    });
    s.engine.shutdown();
}

/// The session's own virtual transaction id, as pg_locks shows it.
async fn own_vxid(c: &Client) -> String {
    let own = "SELECT virtualtransaction FROM pg_locks WHERE pid = pg_backend_pid() AND locktype = 'virtualxid'";
    lines(c, own).await.unwrap().remove(0)
}

/// A parallel query's workers as another session sees them while it runs (checked against
/// PostgreSQL 18.6): pg_stat_activity's "parallel worker" rows with the leader's pid, query
/// and wait event, their AccessShareLock and virtual transaction id locks in pg_locks, and
/// pg_stat_database's parallel worker counts.
#[test]
fn parallel_workers_seen_by_others() {
    let s = server(AuthMethod::ScramSha256);
    runtime().block_on(async {
        let a = s.connect("postgres").await;
        a.batch_execute(
            "CREATE TABLE pw AS SELECT g AS a, repeat('x', 100) AS b FROM generate_series(1, 3000) g; ANALYZE pw;
             SET parallel_setup_cost = 0; SET parallel_tuple_cost = 0;
             SET min_parallel_table_scan_size = 0; SET max_parallel_workers_per_gather = 2;
             SET application_name = 'leader'",
        )
        .await
        .unwrap();
        let leader = lines(&a, "SELECT pg_backend_pid()").await.unwrap().remove(0);
        let o = s.connect("postgres").await;
        let run = tokio::spawn(async move {
            let r = lines(&a, "SELECT count(*) FROM pw WHERE pg_sleep(0.001) IS NOT NULL").await;
            (a, r)
        });
        let workers = "SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'parallel worker'";
        for _ in 0..200 {
            if lines(&o, workers).await.unwrap() == ["2"] {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let rows = format!(
            "SELECT backend_type, leader_pid = {leader}, application_name, state, wait_event, query, client_port \
             FROM pg_stat_activity WHERE application_name = 'leader' ORDER BY backend_type, pid"
        );
        let q = "SELECT count(*) FROM pw WHERE pg_sleep(0.001) IS NOT NULL";
        assert_eq!(
            lines(&o, &rows).await.unwrap(),
            [
                format!("client backend|NULL|leader|active|PgSleep|{q}|{}", lines(&o, &format!("SELECT client_port FROM pg_stat_activity WHERE pid = {leader}")).await.unwrap()[0]),
                format!("parallel worker|t|leader|active|PgSleep|{q}|NULL"),
                format!("parallel worker|t|leader|active|PgSleep|{q}|NULL"),
            ]
        );
        let locks = "SELECT l.locktype, l.relation::regclass, l.mode, l.fastpath, l.virtualtransaction = coalesce(l.virtualxid, l.virtualtransaction) \
             FROM pg_locks l JOIN pg_stat_activity a USING (pid) WHERE a.backend_type = 'parallel worker' ORDER BY 1, 2";
        assert_eq!(
            lines(&o, locks).await.unwrap(),
            ["relation|pw|AccessShareLock|t|t", "relation|pw|AccessShareLock|t|t", "virtualxid|NULL|ExclusiveLock|t|t", "virtualxid|NULL|ExclusiveLock|t|t"]
        );
        let (a, r) = run.await.unwrap();
        assert_eq!(r.unwrap(), ["3000"]);
        assert_eq!(lines(&o, workers).await.unwrap(), ["0"]);
        a.batch_execute("SELECT pg_stat_force_next_flush()").await.unwrap();
        let counts = "SELECT parallel_workers_to_launch, parallel_workers_launched FROM pg_stat_database WHERE datname = 'postgres'";
        assert_eq!(lines(&o, counts).await.unwrap(), ["2|2"]);
        // A prepared statement's plan launches its workers at every execution (its Gathers
        // found once: `BoundQuery::gathers`).
        let st = a.prepare(q).await.unwrap();
        for _ in 0..2 {
            assert_eq!(a.query_one(&st, &[]).await.unwrap().get::<_, i64>(0), 3000);
        }
        a.batch_execute("SELECT pg_stat_force_next_flush()").await.unwrap();
        assert_eq!(lines(&o, counts).await.unwrap(), ["6|6"]);
    });
    s.engine.shutdown();
}

/// The extended protocol's errors carry PostgreSQL 18.6's SQLSTATE, routine, file and line
/// (recorded from it): each named by the routine that raises it where several do.
#[test]
fn protocol_error_locations() {
    let s = server(AuthMethod::Trust);
    let mut c = raw::Raw::connect(&s.addr, "postgres");
    // The first ErrorResponse before ReadyForQuery: "C R F:L".
    fn err(msgs: Vec<(u8, Vec<u8>)>) -> String {
        let (_, b) = msgs.into_iter().find(|(t, _)| *t == b'E').expect("an error");
        let f = |k: u8| {
            b.split(|c| *c == 0)
                .find(|p| p.first() == Some(&k))
                .map(|p| String::from_utf8_lossy(&p[1..]).into_owned())
                .unwrap_or_default()
        };
        format!("{} {} {}:{}", f(b'C'), f(b'R'), f(b'F'), f(b'L'))
    }
    c.parse("", "SELECT 1, 2");
    c.bind_with("", "", &[], &[0, 0, 0]);
    assert_eq!(err(c.sync_messages()), "08P01 PortalSetResultFormat pquery.c:640");
    c.close(b'S', "");
    c.sync();
    c.bind("", "");
    assert_eq!(err(c.sync_messages()), "26000 exec_bind_message postgres.c:1670");
    c.describe(b'S', "");
    assert_eq!(err(c.sync_messages()), "26000 exec_describe_statement_message postgres.c:2668");
    c.describe(b'P', "nope");
    assert_eq!(err(c.sync_messages()), "34000 exec_describe_portal_message postgres.c:2750");
    c.execute("nope", 0);
    assert_eq!(err(c.sync_messages()), "34000 exec_execute_message postgres.c:2136");
    c.describe(b'X', "");
    assert_eq!(err(c.sync_messages()), "08P01 PostgresMain postgres.c:4947");
    c.query("PREPARE q AS SELECT 1");
    c.query("BEGIN");
    c.query("DECLARE k CURSOR FOR SELECT 1");
    c.query("SELECT 1/0");
    c.parse("", "SELECT 1");
    assert_eq!(err(c.sync_messages()), "25P02 exec_parse_message postgres.c:1493");
    c.bind("", "q");
    assert_eq!(err(c.sync_messages()), "25P02 exec_bind_message postgres.c:1745");
    c.describe(b'S', "q");
    assert_eq!(err(c.sync_messages()), "25P02 exec_describe_statement_message postgres.c:2687");
    c.describe(b'P', "k");
    assert_eq!(err(c.sync_messages()), "25P02 exec_describe_portal_message postgres.c:2764");
    c.execute("k", 0);
    assert_eq!(err(c.sync_messages()), "25P02 exec_execute_message postgres.c:2250");
    s.engine.shutdown();
}

/// Raw-protocol cases with PostgreSQL 18.6's replies (`common/protocol.rs`): result format
/// codes checked at the first row, COPY's lost synchronization, duplicate portals,
/// trailing bytes, startup packet layout, unknown parameter type oids, protocol 3.2 and its
/// cancel key, message types and lengths, SSL / GSS requests.
#[test]
fn protocol_cases() {
    use protocol::*;
    type Case = (&'static str, Box<dyn Fn(&str) -> String>);
    fn c(name: &'static str, f: impl Fn(&str) -> String + 'static) -> Case {
        (name, Box::new(f))
    }
    const RF7: &[i16] = &[7];
    const NONE: &[Option<&[u8]>] = &[];
    const ONE: &[Option<&[u8]>] = &[Some(b"1".as_slice())];
    let cases: Vec<Case> = vec![
        // Result format codes: printtup_prepare_info, at the first row sent.
        c("rf7_exec", |a| ext(a, &[], &[parse("", "select 1", &[]), bind("", "", &[], NONE, RF7), execute("", 0)])),
        c("rf7_norows", |a| ext(a, &[], &[parse("", "select 1 where false", &[]), bind("", "", &[], NONE, RF7), execute("", 0)])),
        c("rf7_describe", |a| ext(a, &[], &[parse("", "select 1", &[]), bind("", "", &[], NONE, RF7), describe(b'P', ""), execute("", 0)])),
        c("rf7_dml", |a| ext(a, &["create temp table rt(a int)"], &[parse("", "insert into rt values (1)", &[]), bind("", "", &[], NONE, RF7), execute("", 0)])),
        c("rf7_returning_txn", |a| session(a, &["create temp table rt(a int)", "begin"], &[
            [parse("", "insert into rt values (1) returning a", &[]), bind("", "", &[], NONE, RF7), execute("", 0), sync()].concat(),
            query("rollback; select count(*) from rt"),
        ])),
        c("rf_mixed", |a| ext(a, &[], &[parse("", "select 1, 2", &[]), bind("", "", &[], NONE, &[1, 7]), execute("", 0)])),
        c("rf_neg", |a| ext(a, &[], &[parse("", "select 1", &[]), bind("", "", &[], NONE, &[-1]), execute("", 0)])),
        c("rf7_maxrows", |a| ext(a, &[], &[parse("", "select generate_series(1, 3)", &[]), bind("", "", &[], NONE, RF7), execute("", 1)])),
        c("rf7_show", |a| ext(a, &[], &[parse("", "show work_mem", &[]), bind("", "", &[], NONE, RF7), execute("", 0)])),
        c("rf7_named_again", |a| session(a, &["begin"], &[
            [parse("s", "select generate_series(1, 3)", &[]), bind("p", "s", &[], NONE, RF7), describe(b'P', "p"), sync()].concat(),
            [execute("p", 1), sync()].concat(),
        ])),
        c("rf7_fetch_none", |a| session(a, &["begin", "declare c cursor for select 1 where false"], &[
            [parse("", "fetch c", &[]), bind("", "", &[], NONE, RF7), execute("", 0), sync()].concat(),
        ])),
        c("rf7_copyout", |a| ext(a, &[], &[parse("", "copy (select 1) to stdout", &[]), bind("", "", &[], NONE, RF7), execute("", 0)])),
        // COPY FROM STDIN: a message it doesn't take, or a length out of range, ends the
        // connection after the error.
        c("copy_bad_parse", |a| session(a, &["create temp table cp(a int)"], &[query("copy cp from stdin"), parse("", "select 1", &[])])),
        c("copy_bad_unknown", |a| session(a, &["create temp table cp(a int)"], &[query("copy cp from stdin"), msg(b'y', b"")])),
        c("copy_bad_terminate", |a| session(a, &["create temp table cp(a int)"], &[query("copy cp from stdin"), msg(b'X', b"")])),
        c("copy_badlen", |a| session(a, &["create temp table cp(a int)"], &[query("copy cp from stdin"), rawlen(b'd', 2, b"")])),
        c("copy_done_long", |a| session(a, &["create temp table cp(a int)"], &[query("copy cp from stdin"), msg(b'c', &[b'x'; 10000])])),
        c("copy_done_trailing", |a| session(a, &["create temp table cp(a int)"], &[query("copy cp from stdin"), msg(b'c', b"x")])),
        c("copy_fail_nostr", |a| session(a, &["create temp table cp(a int)"], &[query("copy cp from stdin"), msg(b'f', b"boom")])),
        c("copy_flush_sync", |a| session(a, &["create temp table cp(a int)"], &[query("copy cp from stdin"), [flush(), sync(), msg(b'd', b"1\n"), msg(b'c', b"")].concat()])),
        c("copy_ext_bad", |a| session(a, &["create temp table cp(a int)"], &[
            [parse("", "copy cp from stdin", &[]), bind("", "", &[], NONE, &[]), execute("", 0), sync()].concat(),
            parse("", "select 1", &[]),
        ])),
        // Duplicate portals (CreatePortal), and DECLARE over a Bind's portal.
        c("bind_dup_portal", |a| session(a, &["begin"], &[[parse("s", "select 1", &[]), bind("p", "s", &[], NONE, &[]), bind("p", "s", &[], NONE, &[]), sync()].concat()])),
        c("bind_dup_implicit", |a| ext(a, &[], &[parse("s", "select 1", &[]), bind("p", "s", &[], NONE, &[]), bind("p", "s", &[], NONE, &[])])),
        c("bind_dup_cursor", |a| session(a, &["begin", "declare c cursor for select 1"], &[[parse("s", "select 1", &[]), bind("c", "s", &[], NONE, &[]), sync()].concat()])),
        c("bind_dup_badvalue", |a| session(a, &["begin"], &[[
            parse("s", "select $1::int", &[]), bind("p", "s", &[], ONE, &[]), bind("p", "s", &[], &[Some(b"x".as_slice())], &[]), sync(),
        ].concat()])),
        c("bind_dup_unnamed", |a| ext(a, &[], &[parse("s", "select 1", &[]), bind("", "s", &[], NONE, &[]), bind("", "s", &[], NONE, &[]), execute("", 0)])),
        c("bind_dup_after_close", |a| session(a, &["begin"], &[[
            parse("s", "select 1", &[]), bind("p", "s", &[], NONE, &[]), close(b'P', "p"), bind("p", "s", &[], NONE, &[]), execute("p", 0), sync(),
        ].concat()])),
        c("declare_after_bind", |a| session(a, &["begin"], &[[parse("s", "select 1", &[]), bind("c", "s", &[], NONE, &[]), sync()].concat(), query("declare c cursor for select 1")])),
        c("bind_aborted_badcount", |a| session(a, &["begin", "select 1/0"], &[[parse("", "rollback", &[]), bind("", "", &[], ONE, &[]), sync()].concat()])),
        // Trailing bytes (pq_getmsgend).
        c("sync_trailing", |a| session(a, &[], &[msg(b'S', b"x"), sync()])),
        c("sync_trailing_txn", |a| session(a, &["begin"], &[msg(b'S', b"x"), sync()])),
        c("sync_trailing_implicit", |a| session(a, &["create temp table st(a int)"], &[
            [parse("", "insert into st values (1)", &[]), bind("", "", &[], NONE, &[]), execute("", 0), msg(b'S', b"x")].concat(),
            sync(),
            query("select count(*) from st"),
        ])),
        c("flush_trailing", |a| session(a, &[], &[[msg(b'H', b"x"), sync()].concat()])),
        c("parse_trailing", |a| ext(a, &[], &[msg(b'P', &[cstr(""), cstr("select 1"), vec![0, 0, b'x']].concat())])),
        c("bind_trailing", |a| ext(a, &[], &[parse("", "select 1", &[]), msg(b'B', &[cstr(""), cstr(""), vec![0, 0, 0, 0, 0, 0, b'x']].concat())])),
        c("bind_trailing_badval", |a| ext(a, &[], &[
            parse("", "select $1::int", &[]),
            msg(b'B', &[cstr(""), cstr(""), vec![0, 0, 0, 1, 0, 0, 0, 1, b'x', 0, 0, b'z', b'z']].concat()),
        ])),
        c("execute_trailing", |a| ext(a, &[], &[parse("", "select 1", &[]), bind("", "", &[], NONE, &[]), msg(b'E', &[cstr(""), vec![0, 0, 0, 0, b'x']].concat())])),
        c("describe_trailing", |a| ext(a, &[], &[parse("", "select 1", &[]), msg(b'D', &[vec![b'S'], cstr(""), vec![b'x']].concat())])),
        c("describe_bad_kind_trailing", |a| ext(a, &[], &[msg(b'D', &[vec![b'X'], cstr(""), vec![b'x']].concat())])),
        c("close_trailing", |a| ext(a, &[], &[msg(b'C', &[vec![b'S'], cstr(""), vec![b'x']].concat())])),
        c("query_trailing_txn", |a| session(a, &["begin"], &[msg(b'Q', &[cstr("select 1"), vec![b'x']].concat()), query("select 1")])),
        c("terminate_trailing", |a| session(a, &[], &[msg(b'X', b"x"), query("select 1")])),
        // The startup packet's layout and length.
        c("startup_noterm", |a| startup(a, &[15i32.to_be_bytes().to_vec(), P30.to_be_bytes().to_vec(), b"user\0abc".to_vec()].concat())),
        c("startup_novalue", |a| startup(a, &[14i32.to_be_bytes().to_vec(), P30.to_be_bytes().to_vec(), b"user\0\0".to_vec()].concat())),
        c("startup_only_version", |a| startup(a, &[8i32.to_be_bytes().to_vec(), P30.to_be_bytes().to_vec()].concat())),
        c("startup_extra_after_term", |a| startup(a, &startup_packet(P30, &[], b"\0junk"))),
        c("startup_two_terms", |a| startup(a, &startup_packet(P30, &[], b"\0\0"))),
        c("startup_len_10004", |a| startup(a, &[10004i32.to_be_bytes().to_vec(), P30.to_be_bytes().to_vec(), vec![0; 9996]].concat())),
        c("startup_len_10005", |a| startup(a, &[10005i32.to_be_bytes().to_vec(), P30.to_be_bytes().to_vec(), vec![0; 9997]].concat())),
        c("startup_short", |a| startup(a, &[7i32.to_be_bytes().to_vec(), P30.to_be_bytes().to_vec()].concat())),
        // Parameter types no type has.
        c("parse_badtype", |a| ext(a, &[], &[parse("", "select $1", &[999999])])),
        c("parse_badtype_unused", |a| ext(a, &[], &[parse("s", "select 1", &[999999]), describe(b'S', "s")])),
        c("parse_badtype_cast", |a| ext(a, &[], &[parse("", "select $1::int", &[999999])])),
        c("parse_badtype_insert", |a| ext(a, &["create temp table bt(a int)"], &[parse("", "insert into bt values ($1)", &[999999])])),
        c("parse_badtype_where", |a| ext(a, &[], &[parse("", "select 1 where $1 = 1", &[999999])])),
        c("parse_badtype_second", |a| ext(a, &[], &[parse("", "select $1, $2", &[23, 999999])])),
        c("parse_badtype_sub", |a| ext(a, &[], &[parse("", "select (select $1)", &[999999])])),
        c("parse_badtype_relation", |a| ext(a, &[], &[parse("", "select $1", &[1259])])),
        c("parse_badtype_utility", |a| ext(a, &[], &[parse("", "set work_mem = '5MB'", &[999999])])),
        c("parse_badtype_bind_text", |a| ext(a, &[], &[parse("", "select 1", &[999999]), bind("", "", &[], ONE, &[]), execute("", 0)])),
        c("parse_badtype_bind_null", |a| ext(a, &[], &[parse("", "select 1", &[999999]), bind("", "", &[], &[None], &[]), execute("", 0)])),
        c("parse_badtype_bind_binary", |a| ext(a, &[], &[parse("", "select 1", &[999999]), bind("", "", &[1], ONE, &[]), execute("", 0)])),
        // Protocol versions, NegotiateProtocolVersion, the cancel key.
        c("proto_30", |a| startup(a, &startup_packet(P30, &[], b"\0"))),
        c("proto_31", |a| startup(a, &startup_packet(P30 | 1, &[], b"\0"))),
        c("proto_32", |a| startup(a, &startup_packet(P32, &[], b"\0"))),
        c("proto_33", |a| startup(a, &startup_packet(P30 | 3, &[], b"\0"))),
        c("proto_3ffff", |a| startup(a, &startup_packet(P30 | 0xffff, &[], b"\0"))),
        c("proto_30_pq", |a| startup(a, &startup_packet(P30, &[("_pq_.foo", "1"), ("_pq_.bar", "2")], b"\0"))),
        c("proto_32_pq", |a| startup(a, &startup_packet(P32, &[("_pq_.foo", "1")], b"\0"))),
        c("proto_35_pq", |a| startup(a, &startup_packet(P30 | 5, &[("_pq_.foo", "1")], b"\0"))),
        c("proto_20", |a| {
            let mut k = Conn::open(a, &startup_packet(2 << 16, &[], b"\0"));
            let mut out = String::new();
            loop {
                let b = k.byte();
                if b.len() != 1 {
                    break;
                }
                out.push_str(&b);
            }
            out.escape_default().to_string()
        }),
        c("proto_40", |a| startup(a, &startup_packet(4 << 16, &[], b"\0"))),
        c("proto_32_query", |a| session_at(a, P32, &[query("select 1")])),
        c("cancel_30", |a| cancel(a, P30, cancel_packet)),
        c("cancel_32", |a| cancel(a, P32, cancel_packet)),
        c("cancel_32_short_key", |a| cancel(a, P32, |p, k| cancel_packet(p, &k[..4]))),
        c("cancel_30_long_key", |a| cancel(a, P30, |p, k| cancel_packet(p, &[k, &[0u8; 28][..]].concat()))),
        c("cancel_key_257", |a| cancel(a, P30, |p, _| cancel_packet(p, &[0u8; 257]))),
        // Message types and lengths.
        c("len_sync_10000", |a| session(a, &[], &[rawlen(b'S', 10000, &[0; 9996])])),
        c("len_sync_10001", |a| session(a, &[], &[rawlen(b'S', 10001, &[0; 9997])])),
        c("len_execute_10001", |a| session(a, &[], &[[parse("", "select 1", &[]), bind("", "", &[], NONE, &[]), rawlen(b'E', 10001, &[0; 9997])].concat()])),
        c("len_query_3", |a| session(a, &[], &[rawlen(b'Q', 3, b"")])),
        c("len_parse_huge", |a| session(a, &[], &[rawlen(b'P', 0x4000_0000, b"")])),
        c("len_unknown_huge", |a| session(a, &[], &[rawlen(b'y', 0x7fff_ffff, b"")])),
        c("unknown_in_skip", |a| session(a, &[], &[[parse("", "selec 1", &[]), msg(b'y', b""), sync()].concat()])),
        c("unknown_in_failed", |a| session(a, &["begin", "select 1/0"], &[msg(b'y', b"")])),
        c("copydata_outside", |a| session(a, &[], &[[msg(b'd', b"x"), msg(b'c', b""), msg(b'f', &cstr("x")), query("select 1")].concat()])),
        c("copydata_in_skip", |a| session(a, &[], &[[parse("", "selec 1", &[]), msg(b'd', b"x"), sync()].concat()])),
        c("failed_flush", |a| session(a, &[], &[[parse("", "selec 1", &[]), flush()].concat(), [parse("", "select 1", &[]), flush()].concat(), sync()])),
        c("close_missing", |a| ext(a, &[], &[close(b'S', "nope"), close(b'P', "nope"), close(b'S', ""), close(b'P', "")])),
        c("close_in_failed", |a| session(a, &["prepare q as select 1", "begin", "select 1/0"], &[[close(b'S', "q"), sync()].concat(), query("rollback"), query("execute q")])),
        c("maxrows_suspend_sync", |a| session(a, &[], &[
            [parse("", "select generate_series(1, 3)", &[]), bind("p", "", &[], NONE, &[]), execute("p", 1), sync()].concat(),
            [execute("p", 1), sync()].concat(),
        ])),
        c("maxrows_dml_txn", |a| session(a, &["create temp table mt(a int)", "begin"], &[
            [parse("", "insert into mt select generate_series(1, 3) returning a", &[]), bind("p", "", &[], NONE, &[]), execute("p", 1), execute("p", 1), sync()].concat(),
            [execute("p", 0), sync()].concat(),
            query("select count(*) from mt"),
        ])),
        c("maxrows_exact", |a| ext(a, &[], &[parse("", "select generate_series(1, 3)", &[]), bind("", "", &[], NONE, &[]), execute("", 3), execute("", 1)])),
        // SSLRequest and GSSENCRequest (neither is supported).
        c("ssl_n", |a| byte_replies(a, &[ssl_request(b"")], Some(&startup_packet(P30, &[], b"\0")))),
        c("ssl_ssl", |a| byte_replies(a, &[ssl_request(b""), ssl_request(b"")], None)),
        c("ssl_extra", |a| byte_replies(a, &[ssl_request(b"abcd")], Some(&startup_packet(P30, &[], b"\0")))),
        c("ssl_pipelined", |a| byte_replies(a, &[[ssl_request(b""), startup_packet(P30, &[], b"\0")].concat()], None)),
        // Startup parameters.
        c("startup_unknown_guc", |a| startup(a, &startup_packet(P30, &[("nosuch", "1")], b"\0"))),
        c("startup_ext_guc", |a| startup(a, &startup_packet(P30, &[("foo.bar", "1")], b"\0"))),
        c("startup_pgc_postmaster", |a| startup(a, &startup_packet(P30, &[("shared_buffers", "1MB")], b"\0"))),
        c("startup_internal", |a| startup(a, &startup_packet(P30, &[("server_version", "1")], b"\0"))),
        c("startup_options_and_param", |a| startup(a, &startup_packet(P30, &[("options", "-c application_name=fromopt"), ("application_name", "fromparam")], b"\0"))),
        c("startup_datestyle", |a| startup(a, &startup_packet(P30, &[("datestyle", "German"), ("client_encoding", "LATIN1"), ("intervalstyle", "iso_8601")], b"\0"))),
        c("startup_search_path", |a| startup(a, &startup_packet(P30, &[("search_path", "foo")], b"\0"))),
        // ParameterStatus after SET.
        c("ps_set_same", |a| session(a, &["set application_name = 'x'"], &[query("set application_name = 'x'")])),
        c("ps_set_local", |a| session(a, &["begin"], &[query("set local application_name = 'loc'"), query("commit")])),
        c("ps_set_rollback", |a| session(a, &["begin"], &[query("set application_name = 'rb'"), query("rollback")])),
        c("ps_set_and_back", |a| session(a, &[], &[query("begin; set application_name = 'tmp'; set application_name = ''; commit")])),
        c("ps_reset_all", |a| session(a, &["set application_name = 'x'", "set datestyle = 'SQL, DMY'"], &[query("reset all")])),
        c("ps_ext_set_flush", |a| session(a, &[], &[[parse("", "set application_name = 'ext'", &[]), bind("", "", &[], NONE, &[]), execute("", 0), flush()].concat(), sync()])),
        c("ps_set_config", |a| session(a, &[], &[query("select set_config('application_name', 'cfg', false)")])),
        c("ps_set_role", |a| session(a, &["create role ps_r"], &[query("set role ps_r"), query("reset role"), query("drop role ps_r")])),
        c("ps_discard_all", |a| session(a, &["set application_name = 'x'"], &[query("discard all")])),
        // A multi-statement query's implicit transaction block (and the extended protocol's
        // transaction, Execute to Sync): its first statement opens it, a later error undoes
        // what came before (SETs too), a BEGIN in it turns it into the block with its options
        // checked as SET TRANSACTION's, an error in a block undoes its level's settings then.
        c("ib_set_fail", |a| session(a, &[], &[query("set application_name = 'f'; select * from nope_t"), query("show application_name")])),
        c("ib_set_set_fail", |a| session(a, &[], &[query("set application_name = 'f'; set work_mem = '7MB'; select 1/0"), query("show application_name; show work_mem")])),
        c("ib_set_begin_deferrable", |a| session(a, &[], &[query(
            "set work_mem = '5MB'; begin isolation level serializable read only deferrable; show transaction_isolation; show transaction_read_only; show transaction_deferrable; commit",
        )])),
        c("ib_select_begin_iso", |a| session(a, &[], &[query("select 1; begin isolation level serializable; select 2"), query("show transaction_isolation")])),
        c("ib_select_begin_ro", |a| session(a, &[], &[query("select 1; begin read only; show transaction_read_only; commit")])),
        c("ib_select_begin_not_deferrable", |a| session(a, &[], &[query("select 1; begin isolation level read committed read write not deferrable; commit")])),
        c("ib_settxn_begin", |a| session(a, &[], &[query("set transaction read only; begin isolation level repeatable read; show transaction_read_only; show transaction_isolation; commit")])),
        c("ib_set_begin_rollback", |a| session(a, &[], &[query("set application_name = 'a'; begin; rollback"), query("show application_name")])),
        c("ib_set_begin_open", |a| session(a, &[], &[query("set application_name = 'a'; begin"), query("rollback"), query("show application_name")])),
        c("ib_set_local", |a| session(a, &[], &[query("set local work_mem = '5MB'; begin isolation level serializable; show transaction_isolation; show work_mem; commit"), query("show work_mem")])),
        c("ib_set_commit_fail", |a| session(a, &[], &[query("set application_name = 'a'; commit; select 1/0"), query("show application_name")])),
        c("ib_set_savepoint", |a| session(a, &[], &[query("set application_name = 'a'; savepoint s"), query("show application_name")])),
        c("ib_set_txn", |a| session(a, &[], &[query("set transaction isolation level serializable; show transaction_isolation"), query("show transaction_isolation")])),
        c("ib_set_vacuum", |a| session(a, &[], &[query("set application_name = 'a'; vacuum"), query("show application_name")])),
        c("ib_others_fail", |a| session(a, &["prepare q2 as select 2"], &[
            query("set client_encoding = 'LATIN1'; set session characteristics as transaction isolation level serializable; reset all; listen ch; prepare q1 as select 1; deallocate q2; select 1/0"),
            query("show client_encoding; show default_transaction_isolation; select pg_listening_channels(); execute q1"),
            query("execute q2"),
        ])),
        c("ib_lock", |a| session(a, &["create temp table lt(a int)"], &[query("lock table lt; begin isolation level serializable; show transaction_isolation; commit")])),
        c("ib_ps_reported", |a| session(a, &[], &[query("set application_name = 'r'"), query("set application_name = 'f'; select 1/0"), query("show application_name")])),
        c("ib_ext_set_fail", |a| session(a, &[], &[
            [parse("", "set application_name = 'e'", &[]), bind("", "", &[], NONE, &[]), execute("", 0), parse("", "select * from nope_t", &[]), sync()].concat(),
            query("show application_name"),
        ])),
        c("ib_ext_select_begin_iso", |a| session(a, &[], &[
            [parse("", "select 1", &[]), bind("", "", &[], NONE, &[]), execute("", 0), parse("", "begin isolation level serializable", &[]), bind("", "", &[], NONE, &[]), execute("", 0), sync()].concat(),
            query("rollback"),
        ])),
        c("ib_settxn_after_query", |a| session(a, &[], &[query("begin; select 1; set transaction deferrable"), query("rollback"), query("begin; savepoint s; set transaction isolation level serializable"), query("rollback")])),
        c("ib_block_fail", |a| session(a, &[], &[
            query("begin; set application_name = 'outer'; savepoint s; set application_name = 'inner'"),
            query("select 1/0"),
            query("rollback to s; show application_name"),
            query("select 1/0"),
            query("rollback"),
        ])),
        c("ib_chain_after_error", |a| session(a, &[], &[
            query("begin isolation level serializable read only deferrable; select 1/0"),
            query("rollback and chain"),
            query("show transaction_isolation; show transaction_read_only; show transaction_deferrable"),
            query("rollback"),
        ])),
        // The startup packet's role; SET SESSION AUTHORIZATION's ParameterStatus order.
        c("role_setup", |a| session(a, &["create role pc_r1 login", "create role pc_su superuser login"], &[])),
        c("startup_role", |a| startup(a, &startup_packet(P30, &[("role", "pc_r1")], b"\0"))),
        c("startup_role_missing", |a| startup(a, &startup_packet(P30, &[("role", "pc_nosuch")], b"\0"))),
        c("startup_role_reset", |a| session_params(a, &[("role", "pc_r1")], &[
            query("select current_user, session_user, current_setting('is_superuser')"),
            query("set role none"),
            query("reset role"),
            query("select current_user"),
        ])),
        c("sessauth_ps", |a| session(a, &[], &[query("set session authorization pc_r1"), query("reset session authorization"), query("set session authorization pc_su")])),
        c("sessauth_rollback", |a| session(a, &[], &[query("begin; set session authorization pc_r1; rollback"), query("begin; set local session authorization pc_r1"), query("commit")])),
        c("sessauth_fail", |a| session(a, &[], &[query("set role pc_r1; select 1/0"), query("set session authorization pc_r1; select 1/0"), query("select current_user, current_setting('is_superuser')")])),
    ];
    let file = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/protocol_cases.out");
    if let Ok(addr) = std::env::var("PROTOCOL_RECORD") {
        let out: String = cases.iter().map(|(n, f)| format!("{n}\t{}\n", f(&addr))).collect();
        std::fs::write(file, out).unwrap();
        return;
    }
    // (PROTOCOL_SERVER=host:port: the cases against a running server that trusts the
    // connection, a build without a change say, instead of this one's.)
    let external = std::env::var("PROTOCOL_SERVER").ok();
    let s = external.is_none().then(|| server(AuthMethod::Trust));
    let addr = external.unwrap_or_else(|| s.as_ref().unwrap().addr.clone());
    let expected: std::collections::HashMap<String, String> = std::fs::read_to_string(file)
        .unwrap()
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let mut differ = Vec::new();
    for (name, f) in &cases {
        let got = f(&addr);
        let want = expected.get(*name).map(String::as_str).unwrap_or("(not recorded)");
        if got != want {
            differ.push(format!("{name}\n  pg:   {want}\n  roll: {got}"));
        }
    }
    if let Some(s) = s {
        s.engine.shutdown();
    }
    assert!(differ.is_empty(), "{} of {} cases differ:\n{}", differ.len(), cases.len(), differ.join("\n"));
}
