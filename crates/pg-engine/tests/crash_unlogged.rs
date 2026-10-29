//! UNLOGGED relations across a crash and a clean restart. PostgreSQL resets them at the end of
//! crash recovery: their init forks replace their data, so they come back empty and their
//! sequences start over. A clean shutdown keeps them. A child process (this test binary run
//! again) fills them and kills itself with SIGKILL.

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::session::Session;
use std::path::Path;
use std::time::Duration;

fn open(dir: &Path) -> std::sync::Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: dir.to_path_buf(),
        sync: true,
        idle_close: Duration::from_secs(600),
        superuser: "postgres".into(),
        ..Default::default()
    })
    .unwrap()
}

fn one(s: &mut Session, sql: &str) -> String {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    format!("{:?}", r.last().unwrap().rows)
}

const SETUP: &str = "CREATE UNLOGGED TABLE u (id serial PRIMARY KEY, v text);
    CREATE UNLOGGED TABLE uc (id int REFERENCES u, w int);
    CREATE INDEX uc_w ON uc (w);
    CREATE UNLOGGED SEQUENCE us;
    CREATE TABLE l (id serial PRIMARY KEY, v text);
    INSERT INTO u (v) SELECT 'x' FROM generate_series(1, 500);
    INSERT INTO uc SELECT g, g FROM generate_series(1, 500) g;
    INSERT INTO l (v) SELECT 'y' FROM generate_series(1, 500);
    SELECT nextval('us') FROM generate_series(1, 7)";

/// The child: the tables filled, a clean shutdown, more rows, then SIGKILL.
#[test]
fn crash_unlogged_child() {
    let Ok(dir) = std::env::var("CRASH_UNLOGGED_DIR") else {
        return;
    };
    let dir = Path::new(&dir);
    let eng = open(dir);
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    s.query(SETUP).unwrap();
    drop(s);
    eng.shutdown();
    drop(eng);
    let eng = open(dir);
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    // (A clean shutdown keeps them.)
    assert_eq!(one(&mut s, "SELECT count(*) FROM u"), "[[Int8(500)]]");
    assert_eq!(one(&mut s, "SELECT nextval('us')"), "[[Int8(8)]]");
    s.query("INSERT INTO u (v) VALUES ('z'); INSERT INTO l (v) VALUES ('z')").unwrap();
    std::fs::write(dir.join("ready"), "").unwrap();
    let me = std::process::id().to_string();
    std::process::Command::new("kill").args(["-9", &me]).status().unwrap();
    unreachable!("still alive after SIGKILL");
}

#[test]
fn unlogged_relations_reset_after_a_crash() {
    let tmp = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_unlogged_child", "--nocapture", "--test-threads=1"])
        .env("CRASH_UNLOGGED_DIR", tmp.path())
        .output()
        .unwrap();
    assert!(tmp.path().join("ready").exists(), "{}", String::from_utf8_lossy(&out.stderr));
    let eng = open(tmp.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    assert_eq!(one(&mut s, "SELECT count(*) FROM l"), "[[Int8(501)]]");
    assert_eq!(one(&mut s, "SELECT count(*) FROM u"), "[[Int8(0)]]");
    assert_eq!(one(&mut s, "SELECT count(*) FROM uc"), "[[Int8(0)]]");
    s.query("SET enable_seqscan = off").unwrap();
    assert_eq!(one(&mut s, "SELECT count(*) FROM uc WHERE w > 0"), "[[Int8(0)]]");
    s.query("RESET enable_seqscan").unwrap();
    // Their sequences start over; the logged table's doesn't.
    assert_eq!(one(&mut s, "SELECT nextval('us'), nextval('u_id_seq'), nextval('l_id_seq') > 501"), "[[Int8(1), Int8(1), Bool(true)]]");
    s.query("INSERT INTO u (id, v) VALUES (1, 'again'); INSERT INTO uc VALUES (1, 1)").unwrap();
    assert_eq!(one(&mut s, "SELECT count(*) FROM uc JOIN u USING (id)"), "[[Int8(1)]]");
    drop(s);
    eng.shutdown();
    drop(eng);
    // Reset once: a clean restart after it keeps what was written since.
    let eng = open(tmp.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    assert_eq!(one(&mut s, "SELECT count(*) FROM u"), "[[Int8(1)]]");
    drop(s);
    eng.shutdown();
}
