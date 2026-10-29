//! The server log as PostgreSQL writes it (`pg_engine::log`): log_line_prefix, statement and
//! duration lines, errors with their STATEMENT, and auto_explain's plans. The auto_explain part
//! follows contrib/auto_explain/t/001_auto_explain.pl.

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::server::{AuthMethod, ServerOptions};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static LOG: Mutex<String> = Mutex::new(String::new());

fn taken() -> String {
    std::mem::take(&mut *LOG.lock().unwrap())
}

#[test]
fn server_log() {
    let dir = tempfile::tempdir().unwrap();
    let settings = [
        ("session_preload_libraries", "auto_explain"),
        ("auto_explain.log_min_duration", "0"),
        ("auto_explain.log_analyze", "on"),
    ];
    let engine = Engine::open(EngineOptions {
        data_dir: dir.path().to_path_buf(),
        sync: false,
        idle_close: Duration::from_millis(100),
        password: Some("secret".into()),
        settings: settings.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        ..EngineOptions::default()
    })
    .unwrap();
    pg_engine::log::set_sink(Some(Arc::new(|s: &str| LOG.lock().unwrap().push_str(s))));
    let addr = pg_engine::server::spawn(engine.clone(), "127.0.0.1:0", ServerOptions { auth: AuthMethod::ScramSha256 })
        .unwrap()
        .to_string();
    let (host, port) = addr.rsplit_once(':').unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let run = |options: &str, sql: &str| -> String {
        rt.block_on(async {
            let mut c = tokio_postgres::Config::new();
            c.host(host).port(port.parse().unwrap()).user("postgres").password("secret").dbname("postgres");
            c.options(options);
            let (client, conn) = c.connect(tokio_postgres::NoTls).await.expect("connect");
            let h = tokio::spawn(conn);
            taken();
            let _ = client.simple_query(sql).await;
            drop(client);
            let _ = h.await;
        });
        std::thread::sleep(Duration::from_millis(50));
        taken()
    };

    let log = run("", "SELECT * FROM pg_class;");
    assert!(log.contains("Query Text: SELECT * FROM pg_class;"), "{log}");
    assert!(!log.contains("Query Parameters:"), "{log}");
    assert!(log.contains("Seq Scan on pg_class"), "{log}");
    assert!(log.contains("  plan:\n\tQuery Text:"), "plan lines are continued with a tab: {log}");
    // log_line_prefix's default, '%m [%p] '.
    let first = log.lines().next().unwrap_or_default();
    assert!(prefixed(first), "prefix: {first}");

    let log = run("", "PREPARE get_proc(name) AS SELECT * FROM pg_proc WHERE proname = $1; EXECUTE get_proc('int4pl');");
    assert!(log.contains("Query Text: PREPARE get_proc(name) AS SELECT * FROM pg_proc WHERE proname = $1;"), "{log}");
    assert!(log.contains("Query Parameters: $1 = 'int4pl'"), "{log}");
    assert!(log.contains("Index Scan using pg_proc_proname_args_nsp_index on pg_proc"), "{log}");

    let q = "PREPARE get_type(name) AS SELECT * FROM pg_type WHERE typname = $1; EXECUTE get_type('float8');";
    let log = run("-c auto_explain.log_parameter_max_length=3", q);
    assert!(log.contains("Query Parameters: $1 = 'flo...'"), "{log}");
    let log = run("-c auto_explain.log_parameter_max_length=0", q);
    assert!(!log.contains("Query Parameters:"), "{log}");

    let log = run("-c auto_explain.log_format=json", "SELECT * FROM pg_class;");
    assert!(log.contains("\"Query Text\": \"SELECT * FROM pg_class;\""), "{log}");
    assert!(log.contains("\"Node Type\": \"Seq Scan\""), "{log}");
    assert!(log.contains("\"Relation Name\": \"pg_class\""), "{log}");

    // log_statement, log_min_duration_statement, errors.
    let log = run("-c auto_explain.log_min_duration=-1 -c log_statement=all", "select 1");
    assert!(log.contains("LOG:  statement: select 1\n"), "{log}");
    let log = run("-c auto_explain.log_min_duration=-1 -c log_min_duration_statement=0", "select 2");
    assert!(log.contains(" ms  statement: select 2\n") && log.contains("LOG:  duration: "), "{log}");
    let log = run("-c auto_explain.log_min_duration=-1", "select * from nope");
    assert!(log.contains("ERROR:  relation \"nope\" does not exist"), "{log}");
    assert!(log.contains("STATEMENT:  select * from nope\n"), "{log}");
    let log = run("-c auto_explain.log_min_duration=-1 -c log_error_verbosity=verbose", "select 1/0");
    assert!(log.contains("ERROR:  22012: division by zero\n") && log.contains("LOCATION:  "), "{log}");

    // auto_explain.log_nested_statements: a PL/pgSQL query's plan (DO itself has none).
    let q = "DO $$ BEGIN PERFORM count(*) FROM pg_class; END $$";
    let log = run("", q);
    assert!(!log.contains("Seq Scan on pg_class"), "{log}");
    let log = run("-c auto_explain.log_nested_statements=on", q);
    assert!(log.contains("Query Text: ") && log.contains("Seq Scan on pg_class"), "{log}");

    // log_lock_waits: a wait past deadlock_timeout, then the lock acquired (ProcSleep).
    let log = rt.block_on(async {
        let connect = |opts: &'static str| {
            let mut c = tokio_postgres::Config::new();
            c.host(host).port(port.parse().unwrap()).user("postgres").password("secret").dbname("postgres");
            c.options(opts);
            async move {
                let (client, conn) = c.connect(tokio_postgres::NoTls).await.expect("connect");
                tokio::spawn(conn);
                client
            }
        };
        let a = connect("-c auto_explain.log_min_duration=-1").await;
        let b = connect("-c auto_explain.log_min_duration=-1 -c log_lock_waits=on -c deadlock_timeout=100").await;
        a.batch_execute("CREATE TABLE lw (x int)").await.unwrap();
        a.batch_execute("BEGIN; LOCK TABLE lw").await.unwrap();
        taken();
        let waiter = tokio::spawn(async move { b.batch_execute("BEGIN; LOCK TABLE lw; COMMIT").await.map(|_| b) });
        tokio::time::sleep(Duration::from_millis(400)).await;
        a.batch_execute("COMMIT; DROP TABLE IF EXISTS nothing").await.unwrap();
        let b = waiter.await.unwrap().unwrap();
        // log_checkpoints: the checkpointer's two lines.
        b.batch_execute("CHECKPOINT").await.unwrap();
        a.batch_execute("DROP TABLE lw").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        taken()
    });
    assert!(log.contains("still waiting for AccessExclusiveLock on relation "), "{log}");
    assert!(log.contains("DETAIL:  Process holding the lock: "), "{log}");
    assert!(log.contains(" acquired AccessExclusiveLock on relation "), "{log}");
    assert!(log.contains("LOG:  checkpoint starting: immediate force wait\n"), "{log}");
    assert!(log.contains("LOG:  checkpoint complete: wrote "), "{log}");
    pg_engine::log::set_sink(None);
}

/// `%m [%p] LEVEL:  `: "2026-01-02 03:04:05.678 UTC [123] LOG:  ".
fn prefixed(line: &str) -> bool {
    let b = line.as_bytes();
    let digits = |r: std::ops::Range<usize>| r.clone().all(|i| b.get(i).is_some_and(u8::is_ascii_digit));
    digits(0..4)
        && b.get(4) == Some(&b'-')
        && digits(5..7)
        && digits(8..10)
        && digits(11..13)
        && digits(14..16)
        && digits(17..19)
        && b.get(19) == Some(&b'.')
        && digits(20..23)
        && line.get(23..).is_some_and(|r| r.starts_with(" UTC ["))
        && line.contains("] LOG:  ")
}
