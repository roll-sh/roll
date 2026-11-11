//! Client authentication rules (`roll-engine --hba-file` / `--hba`, `EngineOptions::auth_files`):
//! pg_hba.conf's first matching line chooses the method, `reject` and no line refuse,
//! pg_reload_conf reads the file again, pg_hba_file_rules / pg_ident_file_mappings show it.

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::server::{AuthMethod, ServerOptions};
use std::time::Duration;

fn connect(rt: &tokio::runtime::Runtime, addr: &str, user: &str, password: Option<&str>) -> Result<tokio_postgres::Client, String> {
    let (host, port) = addr.rsplit_once(':').unwrap();
    rt.block_on(async {
        let mut c = tokio_postgres::Config::new();
        c.host(host).port(port.parse().unwrap()).user(user).dbname("postgres");
        if let Some(p) = password {
            c.password(p);
        }
        match c.connect(tokio_postgres::NoTls).await {
            Ok((client, conn)) => {
                tokio::spawn(conn);
                Ok(client)
            }
            Err(e) => Err(e.as_db_error().map_or_else(|| e.to_string(), |d| d.message().to_string())),
        }
    })
}

fn rows(rt: &tokio::runtime::Runtime, c: &tokio_postgres::Client, sql: &str) -> Vec<String> {
    rt.block_on(c.simple_query(sql))
        .unwrap()
        .into_iter()
        .filter_map(|m| match m {
            tokio_postgres::SimpleQueryMessage::Row(r) => {
                Some((0..r.len()).map(|i| r.get(i).unwrap_or("NULL").to_string()).collect::<Vec<_>>().join("|"))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn hba_rules() {
    let dir = tempfile::tempdir().unwrap();
    let hba = dir.path().join("hba.conf");
    let ident = dir.path().join("ident.conf");
    std::fs::write(&hba, "# rules\nhost all regress_hba_no 127.0.0.1/32 reject\nhost all all 127.0.0.1/32 trust\n").unwrap();
    std::fs::write(&ident, "m1 alice regress_hba_no\nbroken\n").unwrap();
    let engine = Engine::open(EngineOptions {
        data_dir: dir.path().join("data"),
        sync: false,
        idle_close: Duration::from_millis(100),
        password: Some("secret".into()),
        auth_files: pg_engine::sysconf::AuthFiles {
            hba_file: Some(hba.clone()),
            hba_lines: Vec::new(),
            ident_file: Some(ident),
        },
        ..EngineOptions::default()
    })
    .unwrap();
    let addr = pg_engine::server::spawn(engine.clone(), "127.0.0.1:0", ServerOptions { auth: AuthMethod::ScramSha256 })
        .unwrap()
        .to_string();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    // trust: no password asked.
    let c = connect(&rt, &addr, "postgres", None).expect("trusted");
    rows(&rt, &c, "CREATE ROLE regress_hba_no LOGIN PASSWORD 'pw'");
    let e = connect(&rt, &addr, "regress_hba_no", Some("pw")).unwrap_err();
    assert_eq!(
        e,
        "pg_hba.conf rejects connection for host \"127.0.0.1\", user \"regress_hba_no\", database \"postgres\", no encryption"
    );
    assert_eq!(
        rows(&rt, &c, "SELECT rule_number, line_number, type, database, user_name, address, netmask, auth_method, error FROM pg_hba_file_rules"),
        vec![
            "1|2|host|{all}|{regress_hba_no}|127.0.0.1|255.255.255.255|reject|NULL",
            "2|3|host|{all}|{all}|127.0.0.1|255.255.255.255|trust|NULL",
        ]
    );
    assert_eq!(
        rows(&rt, &c, "SELECT map_number, line_number, map_name, sys_name, pg_username, error FROM pg_ident_file_mappings"),
        vec!["1|1|m1|alice|regress_hba_no|NULL", "NULL|2|NULL|NULL|NULL|missing entry at end of line"]
    );
    // A reload takes the file's new rules; one with errors leaves them as they were.
    std::fs::write(&hba, "host all all 127.0.0.1/32 scram-sha-256\n").unwrap();
    rows(&rt, &c, "SELECT pg_reload_conf()");
    assert!(connect(&rt, &addr, "regress_hba_no", Some("pw")).is_ok());
    assert!(connect(&rt, &addr, "postgres", Some("wrong")).is_err());
    std::fs::write(&hba, "hostx all all all trust\n").unwrap();
    rows(&rt, &c, "SELECT pg_reload_conf()");
    assert!(connect(&rt, &addr, "postgres", None).is_err(), "the rules with errors weren't taken");
    assert_eq!(
        rows(&rt, &c, "SELECT rule_number, line_number, error FROM pg_hba_file_rules"),
        vec!["NULL|1|invalid connection type \"hostx\""]
    );
    std::fs::write(&hba, "host all postgres 127.0.0.1/32 trust\n").unwrap();
    rows(&rt, &c, "SELECT pg_reload_conf()");
    assert_eq!(
        connect(&rt, &addr, "regress_hba_no", Some("pw")).unwrap_err(),
        "no pg_hba.conf entry for host \"127.0.0.1\", user \"regress_hba_no\", database \"postgres\", no encryption"
    );
    rows(&rt, &c, "DROP ROLE regress_hba_no");
}
