//! The logging collector (`pg_engine::log::collector`): log_directory's files in stderr's,
//! csvlog's and jsonlog's formats, pg_current_logfile / current_logfiles, pg_ls_logdir,
//! pg_rotate_logfile and a reload's new log_filename. Its own test binary: the collector is
//! the process's.

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::server::{AuthMethod, ServerOptions};
use std::time::Duration;

/// csvlog's fields of one line (appendCSVLiteral's quoting).
fn csv_fields(line: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted, mut it) = (Vec::new(), String::new(), false, line.chars().peekable());
    while let Some(c) = it.next() {
        match c {
            '"' if quoted && it.peek() == Some(&'"') => {
                cur.push('"');
                it.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out
}

#[test]
fn logging_collector() {
    let dir = tempfile::tempdir().unwrap();
    // (log_destination from postgresql.auto.conf, so a reload can change it.)
    std::fs::write(dir.path().join("postgresql.auto.conf"), "log_destination = 'stderr,csvlog,jsonlog'\n").unwrap();
    let settings = [
        ("logging_collector", "on"),

        ("log_line_prefix", "%m [%p] %q%u@%d %l "),
        ("log_rotation_age", "0"),
    ];
    let engine = Engine::open(EngineOptions {
        data_dir: dir.path().to_path_buf(),
        sync: false,
        settings: settings.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        ..EngineOptions::default()
    })
    .unwrap();
    let files = engine.server_conf().loaded();
    let argv = engine.command_line_values().to_vec();
    assert!(pg_engine::log::collector::wanted_at_start(&argv, &files));
    let cfg = pg_engine::log::collector::Config::of_server(dir.path().to_path_buf(), &argv, &files);
    pg_engine::log::collector::start(cfg).unwrap();
    let addr = pg_engine::server::spawn(engine.clone(), "127.0.0.1:0", ServerOptions { auth: AuthMethod::Trust })
        .unwrap()
        .to_string();
    let (host, port) = addr.rsplit_once(':').unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let rows = |sql: &str| -> Vec<String> {
        rt.block_on(async {
            let mut c = tokio_postgres::Config::new();
            c.host(host).port(port.parse().unwrap()).user("postgres").dbname("postgres").application_name("app\"x");
            let (client, conn) = c.connect(tokio_postgres::NoTls).await.expect("connect");
            let h = tokio::spawn(conn);
            let mut out = Vec::new();
            for m in client.simple_query(sql).await.unwrap_or_default() {
                if let tokio_postgres::SimpleQueryMessage::Row(r) = m {
                    out.push((0..r.len()).map(|i| r.get(i).unwrap_or("NULL")).collect::<Vec<_>>().join("|"));
                }
            }
            drop(client);
            let _ = h.await;
            out
        })
    };
    let _ = rows("select * from nope");
    pg_engine::log::flush();
    let names = rows("SELECT pg_current_logfile(), pg_current_logfile('csvlog'), pg_current_logfile('jsonlog')");
    let names: Vec<String> = names[0].split('|').map(str::to_string).collect();
    assert!(names[0].starts_with("log/postgresql-") && names[0].ends_with(".log"), "{names:?}");
    assert_eq!(names[1], names[0].replace(".log", ".csv"));
    assert_eq!(names[2], names[0].replace(".log", ".json"));
    let listed = std::fs::read_to_string(dir.path().join("current_logfiles")).unwrap();
    assert_eq!(listed, format!("stderr {}\ncsvlog {}\njsonlog {}\n", names[0], names[1], names[2]));
    let mut ls = rows("SELECT name FROM pg_ls_logdir() ORDER BY 1");
    ls.sort();
    assert_eq!(ls.len(), 3, "{ls:?}");

    let read = |n: &str| std::fs::read_to_string(dir.path().join(n)).unwrap();
    // stderr's format: a prefix per line, %l counting each.
    let text = read(&names[0]);
    let err = text.lines().find(|l| l.contains("ERROR:  relation \"nope\" does not exist at character 15")).unwrap();
    assert!(err.contains("] postgres@postgres "), "{err}");
    let n: u64 = err.split(' ').nth(5).unwrap().parse().unwrap();
    let stmt = text.lines().find(|l| l.contains("STATEMENT:  select * from nope")).unwrap();
    assert_eq!(stmt.split(' ').nth(5).unwrap(), (n + 1).to_string(), "{text}");
    // csvlog: 26 fields in write_csvlog's order.
    let csv = read(&names[1]);
    let line = csv.lines().find(|l| l.contains("does not exist")).unwrap();
    let f = csv_fields(line);
    assert_eq!(f.len(), 26, "{line}");
    assert_eq!((&f[1][..], &f[2][..]), ("postgres", "postgres"));
    assert!(f[4].starts_with("127.0.0.1:"), "{line}");
    assert_eq!(f[6], "1", "the process's first csvlog line: {line}");
    assert_eq!((&f[11][..], &f[12][..], &f[13][..]), ("ERROR", "42P01", "relation \"nope\" does not exist"));
    assert_eq!((&f[19][..], &f[20][..]), ("select * from nope", "15"));
    assert_eq!((&f[22][..], &f[23][..], &f[25][..]), ("app\"x", "client backend", "0"));
    assert!(line.contains(",\"app\"\"x\",\"client backend\",,0"), "{line}");
    // jsonlog: appendJSONKeyValue's keys in order, NULLs left out.
    let json = read(&names[2]);
    let line = json.lines().find(|l| l.contains("does not exist")).unwrap();
    let keys: Vec<&str> = line.split("\":").filter_map(|s| s.rsplit("\"").next()).collect();
    let keys = &keys[..keys.len() - 1];
    let want = [
        "timestamp", "user", "dbname", "pid", "remote_host", "remote_port", "session_id", "line_num", "ps",
        "session_start", "vxid", "txid", "error_severity", "state_code", "message", "statement",
        "cursor_position", "application_name", "backend_type",
    ];
    assert!(keys.starts_with(&want), "{keys:?}\n{line}");
    assert!(line.contains("\"message\":\"relation \\\"nope\\\" does not exist\""), "{line}");
    assert!(line.ends_with("\"backend_type\":\"client backend\",\"query_id\":0}"), "{line}");

    // pg_rotate_logfile, and a reload's log_filename: new files, listed.
    assert_eq!(rows("SELECT pg_rotate_logfile()"), ["t"]);
    let _ = rows("ALTER SYSTEM SET log_filename = 'other-%Y.log'");
    let _ = rows("SELECT pg_reload_conf()");
    std::thread::sleep(Duration::from_millis(100));
    pg_engine::log::flush();
    let now = rows("SELECT pg_current_logfile(), pg_current_logfile('jsonlog')");
    assert!(now[0].starts_with("log/other-2") && now[0].contains(".log|log/other-2"), "{now:?}");
    assert!(now[0].ends_with(".json"), "{now:?}");
    let _ = rows("ALTER SYSTEM SET log_destination = 'csvlog'");
    let _ = rows("SELECT pg_reload_conf()");
    std::thread::sleep(Duration::from_millis(100));
    pg_engine::log::flush();
    assert_eq!(rows("SELECT pg_current_logfile('stderr') IS NULL, pg_current_logfile() LIKE '%.csv'"), ["t|t"]);
    assert_eq!(rows("SELECT pg_current_logfile('xml')"), Vec::<String>::new());
    pg_engine::log::collector::stop();
    engine.shutdown();
}
