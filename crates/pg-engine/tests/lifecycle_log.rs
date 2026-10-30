//! The server's start and end in its log (`log::lifecycle`, `roll-engine`'s signals): the
//! startup process's lines after a clean shutdown and after a crash (with the end-of-recovery
//! checkpoint), and smart (SIGTERM), fast (SIGINT) and immediate (SIGQUIT) shutdowns with
//! the shutdown checkpoint, in PostgreSQL's order.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    port: u16,
    log: Arc<Mutex<Vec<String>>>,
}

/// The log's messages without their prefix ("%m [%p] ").
fn message(l: &str) -> String {
    l.split_once("] ").map_or(l, |(_, m)| m).to_string()
}

fn start(data: &std::path::Path) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_roll-engine"))
        .args(["--data", data.to_str().unwrap(), "--listen", "127.0.0.1:0", "--no-sync"])
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let err = child.stderr.take().unwrap();
    let l = log.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            l.lock().unwrap().push(message(&line));
        }
    });
    let mut s = Server { child, port: 0, log };
    s.wait_for("database system is ready to accept connections");
    let lines = s.log.lock().unwrap().clone();
    let listening = lines.iter().find(|l| l.contains("listening on IPv4")).unwrap();
    s.port = listening.rsplit_once("port ").unwrap().1.parse().unwrap();
    s
}

impl Server {
    fn wait_for(&self, text: &str) {
        let t = Instant::now();
        while !self.log.lock().unwrap().iter().any(|l| l.ends_with(text)) {
            assert!(t.elapsed() < Duration::from_secs(60), "no \"{text}\": {:?}", self.log.lock().unwrap());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn signal(&mut self, sig: &str) -> Vec<String> {
        let before = self.log.lock().unwrap().len();
        Command::new("kill").args([sig, &self.child.id().to_string()]).status().unwrap();
        let _ = self.child.wait();
        std::thread::sleep(Duration::from_millis(100));
        self.log.lock().unwrap()[before..].to_vec()
    }

    /// The lines from the startup process (after "listening on") to "ready".
    fn startup_lines(&self) -> Vec<String> {
        let log = self.log.lock().unwrap();
        let from = log.iter().position(|l| l.contains("listening on IPv4")).unwrap() + 1;
        log[from..].iter().take_while(|l| !l.ends_with("ready to accept connections")).cloned().collect()
    }

    fn query(&self, sql: &str) -> Result<(), String> {
        query(self.port, sql)
    }
}

fn query(port: u16, sql: &str) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let conn = format!("host=127.0.0.1 port={port} user=postgres dbname=postgres");
        let (c, conn) = tokio_postgres::connect(&conn, tokio_postgres::NoTls).await.map_err(|e| e.as_db_error().map_or_else(|| e.to_string(), |d| d.message().to_string()))?;
        let h = tokio::spawn(conn);
        let r = c.batch_execute(sql).await.map_err(|e| e.as_db_error().map_or_else(|| e.to_string(), |d| d.message().to_string()));
        drop(c);
        let _ = h.await;
        r
    })
}

fn strip_numbers(l: &str) -> String {
    l.split(" at ").next().unwrap().split(": wrote").next().unwrap().to_string()
}

#[test]
fn startup_and_shutdown_lines() {
    let data = tempfile::tempdir().unwrap();
    let mut s = start(data.path());
    let log = s.log.lock().unwrap().clone();
    assert!(log[0].starts_with("LOG:  starting PostgreSQL 18.6"), "{log:?}");
    assert_eq!(s.startup_lines().iter().map(|l| strip_numbers(l)).collect::<Vec<_>>(), ["LOG:  database system was shut down"]);
    s.query("CREATE TABLE t (a int); INSERT INTO t VALUES (1)").unwrap();

    // Smart: a session still running is waited for; a new one is refused meanwhile.
    let port = s.port;
    let busy = std::thread::spawn(move || query(port, "SELECT pg_sleep(1)"));
    std::thread::sleep(Duration::from_millis(300));
    Command::new("kill").args(["-TERM", &s.child.id().to_string()]).status().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let refused = s.query("SELECT 1").unwrap_err();
    assert!(refused.contains("the database system is shutting down"), "{refused}");
    busy.join().unwrap().unwrap();
    let end = s.signal("-0"); // (waits for the exit)
    let want = [
        "LOG:  received smart shutdown request",
        "FATAL:  the database system is shutting down",
        "LOG:  background worker \"logical replication launcher\" (PID 35) exited with exit code 1",
        "LOG:  shutting down",
        "LOG:  checkpoint starting: shutdown immediate",
        "LOG:  checkpoint complete",
        "LOG:  database system is shut down",
    ];
    let log = s.log.lock().unwrap().clone();
    let from = log.iter().position(|l| l == want[0]).unwrap();
    assert_eq!(log[from..].iter().map(|l| strip_numbers(l)).collect::<Vec<_>>(), want, "{end:?}");

    // Fast: the session ends with its FATAL first.
    let mut s = start(data.path());
    assert_eq!(s.startup_lines().iter().map(|l| strip_numbers(l)).collect::<Vec<_>>(), ["LOG:  database system was shut down"]);
    let port = s.port;
    let busy = std::thread::spawn(move || query(port, "SELECT pg_sleep(30)"));
    std::thread::sleep(Duration::from_millis(300));
    let end: Vec<String> = s.signal("-INT").iter().map(|l| strip_numbers(l)).collect();
    assert_eq!(
        end,
        [
            "LOG:  received fast shutdown request",
            "LOG:  aborting any active transactions",
            "FATAL:  terminating connection due to administrator command",
            "STATEMENT:  SELECT pg_sleep(30)",
            "LOG:  background worker \"logical replication launcher\" (PID 35) exited with exit code 1",
            "LOG:  shutting down",
            "LOG:  checkpoint starting: shutdown immediate",
            "LOG:  checkpoint complete",
            "LOG:  database system is shut down",
        ]
    );
    assert!(busy.join().unwrap().unwrap_err().contains("terminating connection due to administrator command"));

    // Immediate: no checkpoint; the next start recovers.
    let mut s = start(data.path());
    s.query("INSERT INTO t VALUES (2)").unwrap();
    let end = s.signal("-QUIT");
    assert_eq!(end, ["LOG:  received immediate shutdown request", "LOG:  database system is shut down"]);
    let mut s = start(data.path());
    let recovery: Vec<String> = s
        .startup_lines()
        .iter()
        .map(|l| l.split(" at ").next().unwrap().split(" system usage").next().unwrap().split(": wrote").next().unwrap().to_string())
        .collect();
    let crash = [
        "LOG:  database system was interrupted; last known up",
        "LOG:  database system was not properly shut down; automatic recovery in progress",
        "LOG:  redo starts",
        "LOG:  invalid record length",
        "LOG:  redo done",
        "LOG:  checkpoint starting: end-of-recovery immediate wait",
        "LOG:  checkpoint complete",
    ];
    assert_eq!(recovery, crash);

    // kill -9 as well.
    s.query("INSERT INTO t VALUES (3)").unwrap();
    assert!(s.signal("-KILL").is_empty());
    let mut s = start(data.path());
    assert_eq!(s.startup_lines().len(), crash.len());
    s.query("SELECT count(*) FROM t").unwrap();
    s.signal("-TERM");
}
