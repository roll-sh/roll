//! PostgreSQL 18's log_connections lines (`server.rs`): receipt, authentication,
//! authorization and setup_durations, in PostgreSQL's order and layout, with the server's
//! value for the first three and the session's (startup options count) for the last; an
//! authentication failure's FATAL with its log-only DETAIL; the engine's connections to
//! itself logged and reported as PostgreSQL's Unix-socket ones are ("[local]", no
//! addresses).

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::server::{AuthMethod, ServerOptions};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static LOG: Mutex<String> = Mutex::new(String::new());

fn taken() -> String {
    std::mem::take(&mut *LOG.lock().unwrap())
}

/// The log's messages without their prefix ("%m [%p] "), one per line.
fn messages(log: &str) -> Vec<String> {
    log.lines().map(|l| l.split_once("] ").map_or(l, |(_, m)| m).to_string()).collect()
}

#[test]
fn connection_log() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions {
        data_dir: dir.path().to_path_buf(),
        sync: false,
        idle_close: Duration::from_millis(100),
        outbound: vec!["local".into()],
        settings: vec![("log_connections".into(), "receipt,authentication,authorization".into())],
        auth_files: pg_engine::sysconf::AuthFiles {
            hba_lines: vec![
                "local all all trust".into(),
                "host all postgres 127.0.0.1/32 trust".into(),
                "host all all 127.0.0.1/32 scram-sha-256".into(),
            ],
            ..Default::default()
        },
        ..EngineOptions::default()
    })
    .unwrap();
    pg_engine::log::set_sink(Some(Arc::new(|s: &str| LOG.lock().unwrap().push_str(s))));
    let addr = pg_engine::server::spawn(engine.clone(), "127.0.0.1:0", ServerOptions { auth: AuthMethod::ScramSha256 })
        .unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    // A connection as `user` with `password`, startup `options` (Err: the connection's error).
    let run = |user: &str, password: &str, options: &str, sql: &str| -> (Result<Vec<String>, String>, Vec<String>) {
        let r = rt.block_on(async {
            let mut c = tokio_postgres::Config::new();
            c.host("127.0.0.1").port(addr.port()).user(user).password(password).dbname("postgres");
            c.application_name("app\u{e9}").options(options);
            let (client, conn) = c.connect(tokio_postgres::NoTls).await.map_err(|e| e.as_db_error().map_or_else(|| e.to_string(), |d| d.message().to_string()))?;
            let h = tokio::spawn(conn);
            let rows = client.simple_query(sql).await.map_err(|e| e.as_db_error().map_or_else(|| e.to_string(), |d| d.message().to_string()))?;
            drop(client);
            let _ = h.await;
            Ok(rows
                .iter()
                .filter_map(|m| match m {
                    tokio_postgres::SimpleQueryMessage::Row(r) => {
                        Some((0..r.len()).map(|i| r.get(i).unwrap_or("NULL")).collect::<Vec<_>>().join("|"))
                    }
                    _ => None,
                })
                .collect())
        });
        std::thread::sleep(Duration::from_millis(100));
        (r, messages(&taken()))
    };
    let file = "/var/lib/postgresql/18/docker/pg_hba.conf";
    let _ = run("postgres", "", "", "CREATE ROLE alice LOGIN PASSWORD 'pw'; CREATE EXTENSION dblink");

    // The server's aspects; the startup packet's application_name as received, cleaned.
    let (r, log) = run("alice", "pw", "", "SELECT 1");
    assert_eq!(r.unwrap(), ["1"]);
    assert!(log[0].starts_with("LOG:  connection received: host=127.0.0.1 port="), "{log:?}");
    assert_eq!(log[1], format!("LOG:  connection authenticated: identity=\"alice\" method=scram-sha-256 ({file}:3)"));
    assert_eq!(log[2], "LOG:  connection authorized: user=alice database=postgres application_name=app\\xc3\\xa9");
    assert_eq!(log.len(), 3, "{log:?}");

    // A wrong password: the FATAL, with why and the rule it matched only in the log.
    let (r, log) = run("alice", "nope", "", "SELECT 1");
    assert!(r.unwrap_err().contains("password authentication failed for user \"alice\""));
    assert_eq!(log[1], "FATAL:  password authentication failed for user \"alice\"", "{log:?}");
    assert_eq!(log[2], "DETAIL:  Connection matched file \"/var/lib/postgresql/18/docker/pg_hba.conf\" line 3: \"host all all 127.0.0.1/32 scram-sha-256\"");
    let (_, log) = run("nobody", "pw", "", "SELECT 1");
    assert_eq!(log[2], "DETAIL:  Role \"nobody\" does not exist.", "{log:?}");
    assert!(log[3].starts_with("\tConnection matched file"), "{log:?}");

    // Trust: "user=", then the role's own check after the authorization line.
    let (_, log) = run("postgres", "", "-c log_connections=all", "SELECT 1");
    assert_eq!(log[1], format!("LOG:  connection authenticated: user=\"postgres\" method=trust ({file}:2)"));
    // setup_durations is the session's: the startup options turned it on.
    let ready = log.iter().find(|l| l.starts_with("LOG:  connection ready: ")).expect("ready line");
    let rest = ready.strip_prefix("LOG:  connection ready: setup total=").unwrap();
    let parts: Vec<&str> = rest.split(" ms").collect();
    assert_eq!(parts.len(), 4, "{ready}");
    assert!(parts[1].starts_with(", fork=") && parts[2].starts_with(", authentication="), "{ready}");
    let (_, log) = run("postgres", "", "-c log_connections=bogus", "SELECT 1");
    assert_eq!(log[3], "FATAL:  invalid value for parameter \"log_connections\": \"bogus\"", "{log:?}");
    assert_eq!(log[4], "DETAIL:  Invalid option \"bogus\".");

    // The engine's connection to itself without a host: a Unix-socket one.
    let q = "SELECT * FROM dblink('dbname=postgres user=postgres', \
             $$SELECT inet_client_addr(), inet_client_port(), inet_server_addr(), inet_server_port(), \
             (SELECT client_port FROM pg_stat_activity WHERE pid = pg_backend_pid())$$) \
             AS t(a inet, b int, c inet, d int, e int)";
    let (r, log) = run("postgres", "", "-c log_disconnections=on", q);
    assert_eq!(r.unwrap(), ["NULL|NULL|NULL|NULL|-1"]);
    assert!(log.contains(&"LOG:  connection received: host=[local]".to_string()), "{log:?}");
    assert!(log.contains(&format!("LOG:  connection authenticated: user=\"postgres\" method=trust ({file}:1)")), "{log:?}");
    // (No application_name in libpq's startup packet: none in the line.)
    assert!(log.contains(&"LOG:  connection authorized: user=postgres database=postgres".to_string()), "{log:?}");
    pg_engine::log::set_sink(None);
    engine.shutdown();
}
