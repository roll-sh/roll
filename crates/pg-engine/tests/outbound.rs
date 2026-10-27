//! Outbound connections (`--allow-outbound`, `EngineOptions::outbound`): dblink and
//! postgres_fdw connect nowhere by default; with `local` they reach the engine itself
//! (a connection without a host, as through PostgreSQL's Unix socket); a host must be on the
//! list.

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::server::{AuthMethod, ServerOptions};
use pg_engine::session::Session;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

fn open(data: &Path, outbound: &[&str]) -> Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: data.to_path_buf(),
        cache_bytes: 4 << 20,
        memtable_bytes: 1 << 20,
        sync: false,
        idle_close: Duration::from_secs(60),
        superuser: "postgres".into(),
        password: None,
        server_files: None,
        server_programs: false,
        shared_preload_libraries: Vec::new(),
        locale: Default::default(),
        outbound: outbound.iter().map(|s| s.to_string()).collect(),
        max_prepared_transactions: 0,
        auth_files: Default::default(),
        settings: Vec::new(),
    })
    .unwrap()
}

fn run(s: &mut Session, sql: &str) -> Vec<String> {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    r.last().unwrap().rows.iter().map(|r| format!("{r:?}")).collect()
}

/// The error's SQLSTATE, message and detail.
fn err(s: &mut Session, sql: &str) -> String {
    match s.query(sql) {
        Ok(_) => panic!("{sql}: no error"),
        Err(e) => format!("{} {} DETAIL: {}", e.code, e.message, e.detail.unwrap_or_default()),
    }
}

/// A server for `engine` on a free loopback port: the port.
fn serve(engine: &Arc<Engine>) -> u16 {
    let addr = pg_engine::server::spawn(engine.clone(), "127.0.0.1:0", ServerOptions { auth: AuthMethod::Trust }).unwrap();
    addr.port()
}

#[test]
fn refused_by_default() {
    let data = tempfile::tempdir().unwrap();
    let engine = open(data.path(), &[]);
    serve(&engine);
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    run(&mut s, "CREATE EXTENSION dblink");
    let e = err(&mut s, "SELECT dblink_connect('dbname=postgres port=5432')");
    assert!(e.starts_with("08001 could not establish connection DETAIL: connection to server on socket"), "{e}");
    assert!(e.contains("outbound connections to this destination are not allowed by this server"), "{e}");
    let e = err(&mut s, "SELECT dblink_connect('host=192.0.2.1 port=5432 dbname=x connect_timeout=2')");
    assert!(e.contains("connection to server at \"192.0.2.1\", port 5432 failed: outbound connections"), "{e}");
    // postgres_fdw: the same, through its own error.
    run(&mut s, "CREATE EXTENSION postgres_fdw");
    run(&mut s, "CREATE SERVER lb FOREIGN DATA WRAPPER postgres_fdw OPTIONS (dbname 'postgres')");
    run(&mut s, "CREATE USER MAPPING FOR CURRENT_USER SERVER lb");
    run(&mut s, "CREATE FOREIGN TABLE ft (a int) SERVER lb OPTIONS (table_name 'pg_class')");
    let e = err(&mut s, "SELECT * FROM ft");
    assert!(e.starts_with("08001 could not connect to server \"lb\""), "{e}");
    assert!(e.contains("outbound connections"), "{e}");
}

#[test]
fn local_reaches_the_engine() {
    let data = tempfile::tempdir().unwrap();
    let engine = open(data.path(), &["local"]);
    serve(&engine);
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    run(&mut s, "CREATE EXTENSION dblink");
    run(&mut s, "CREATE TABLE t (a int)");
    run(&mut s, "INSERT INTO t VALUES (1), (2)");
    let rows = run(&mut s, "SELECT sum(a)::int FROM dblink('dbname=postgres port=5432', 'SELECT a FROM t') AS x(a int)");
    assert_eq!(rows, vec!["[Int4(3)]"]);
    // Another port is another socket: nothing there.
    let e = err(&mut s, "SELECT dblink_connect('dbname=postgres port=5433')");
    assert!(e.contains("No such file or directory"), "{e}");
    // A host stays refused.
    let e = err(&mut s, "SELECT dblink_connect('host=127.0.0.1 dbname=postgres')");
    assert!(e.contains("outbound connections"), "{e}");
}

#[test]
fn listed_hosts() {
    let data = tempfile::tempdir().unwrap();
    let engine = open(data.path(), &["127.0.0.1"]);
    let port = serve(&engine);
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    run(&mut s, "CREATE EXTENSION dblink");
    let rows = run(
        &mut s,
        &format!("SELECT * FROM dblink('host=127.0.0.1 port={port} dbname=postgres', 'SELECT 1') AS x(a int)"),
    );
    assert_eq!(rows, vec!["[Int4(1)]"]);
    let e = err(&mut s, "SELECT dblink_connect('dbname=postgres')");
    assert!(e.contains("outbound connections"), "{e}");
}
