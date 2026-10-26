//! Waits a client can end: `statement_timeout`, `lock_timeout` (also `SET LOCAL` in a
//! transaction block) and a CancelRequest from another connection, over the simple and the
//! extended protocol (Parse / Bind / Execute; portals), for every kind of wait: row locks,
//! unique-key waits, relation locks, advisory locks, a SERIALIZABLE READ ONLY DEFERRABLE
//! transaction's wait for a safe snapshot, pg_sleep and a long scan.
//!
//! Each case leaves one transcript line (the waiter's error, and whether its connection and
//! the lock it waited for are usable afterwards); the lines expected are PostgreSQL 18.6's.
//! Re-record them by running the cases against a reference:
//!
//!     WIRE_WAITS_REF=127.0.0.1:55499 WIRE_WAITS_PASSWORD=pw \
//!       cargo test -p pg-engine --test wire_waits -- --ignored --nocapture
//!
//! (it makes and drops a database `wire_waits` there and prints the transcript).

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::server::{AuthMethod, ServerOptions};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, NoTls};

struct Target {
    host: String,
    port: u16,
    password: String,
    _engine: Option<(Arc<Engine>, tempfile::TempDir)>,
}

impl Target {
    fn engine() -> Target {
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
        };
        let engine = Engine::open(opts).unwrap();
        let opts = ServerOptions { auth: AuthMethod::ScramSha256 };
        let addr = pg_engine::server::spawn(engine.clone(), "127.0.0.1:0", opts).unwrap();
        Target {
            host: addr.ip().to_string(),
            port: addr.port(),
            password: "secret".into(),
            _engine: Some((engine, dir)),
        }
    }

    fn reference(addr: &str) -> Target {
        let (host, port) = addr.rsplit_once(':').expect("WIRE_WAITS_REF=host:port");
        Target {
            host: host.into(),
            port: port.parse().unwrap(),
            password: std::env::var("WIRE_WAITS_PASSWORD").unwrap_or_else(|_| "pw".into()),
            _engine: None,
        }
    }

    async fn connect(&self, db: &str) -> Client {
        let mut c = tokio_postgres::Config::new();
        c.host(&self.host)
            .port(self.port)
            .user("postgres")
            .password(&self.password)
            .dbname(db);
        let (client, conn) = c.connect(NoTls).await.expect("connect");
        tokio::spawn(conn);
        client
    }
}

const DB: &str = "wire_waits";

/// How the waiting statement is ended.
#[derive(Clone, Copy, Debug)]
enum End {
    /// `SET statement_timeout`.
    StatementTimeout,
    /// `SET lock_timeout` (a statement_timeout behind it ends waits that aren't for locks).
    LockTimeout,
    /// `BEGIN; SET LOCAL lock_timeout` (and statement_timeout), then the statement.
    LocalLockTimeout,
    /// A CancelRequest from another connection.
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Proto {
    Simple,
    Extended,
}

/// A kind of wait: what the holder runs, the waiting statement (with `$n` parameters for the
/// extended protocol, the same values written in for the simple one), what ends the hold.
struct Wait {
    name: &'static str,
    hold: &'static str,
    /// Run by the waiter first (opening its transaction block when there is one).
    begin: &'static str,
    simple: &'static str,
    extended: &'static str,
    params: fn() -> Vec<Box<dyn ToSql + Sync + Send>>,
    release: &'static str,
}

fn waits() -> Vec<Wait> {
    vec![
        Wait {
            name: "row lock",
            hold: "BEGIN; SELECT v FROM t WHERE id = 1 FOR UPDATE",
            begin: "",
            simple: "UPDATE t SET v = v + 1 WHERE id = 1",
            extended: "UPDATE t SET v = v + 1 WHERE id = $1",
            params: || vec![Box::new(1i32)],
            release: "ROLLBACK",
        },
        Wait {
            name: "row lock (FOR SHARE)",
            hold: "BEGIN; UPDATE t SET v = v + 1 WHERE id = 2",
            begin: "",
            simple: "SELECT v FROM t WHERE id = 2 FOR SHARE",
            extended: "SELECT v FROM t WHERE id = $1 FOR SHARE",
            params: || vec![Box::new(2i32)],
            release: "ROLLBACK",
        },
        Wait {
            name: "unique key",
            hold: "BEGIN; INSERT INTO u VALUES (7)",
            begin: "",
            simple: "INSERT INTO u VALUES (7)",
            extended: "INSERT INTO u VALUES ($1)",
            params: || vec![Box::new(7i32)],
            release: "ROLLBACK",
        },
        Wait {
            name: "relation lock",
            hold: "BEGIN; LOCK TABLE t IN ACCESS EXCLUSIVE MODE",
            begin: "",
            simple: "SELECT v FROM t WHERE id = 3",
            extended: "SELECT v FROM t WHERE id = $1",
            params: || vec![Box::new(3i32)],
            release: "ROLLBACK",
        },
        Wait {
            name: "advisory lock",
            hold: "SELECT pg_advisory_lock(42)",
            begin: "",
            simple: "SELECT pg_advisory_lock(42)",
            extended: "SELECT pg_advisory_lock($1)",
            params: || vec![Box::new(42i64)],
            release: "SELECT pg_advisory_unlock(42)",
        },
        Wait {
            name: "advisory xact lock",
            hold: "BEGIN; SELECT pg_advisory_xact_lock(43)",
            begin: "",
            simple: "SELECT pg_advisory_xact_lock_shared(43)",
            extended: "SELECT pg_advisory_xact_lock_shared($1)",
            params: || vec![Box::new(43i64)],
            release: "ROLLBACK",
        },
        Wait {
            name: "safe snapshot",
            hold: "BEGIN ISOLATION LEVEL SERIALIZABLE; SELECT count(*) FROM t; UPDATE t SET v = v WHERE id = 4",
            begin: "BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY DEFERRABLE",
            simple: "SELECT count(*) FROM t WHERE id > 0",
            extended: "SELECT count(*) FROM t WHERE id > $1",
            params: || vec![Box::new(0i32)],
            release: "ROLLBACK",
        },
        Wait {
            name: "pg_sleep",
            hold: "",
            begin: "",
            simple: "SELECT pg_sleep(30)",
            extended: "SELECT pg_sleep($1)",
            params: || vec![Box::new(30f64)],
            release: "",
        },
        Wait {
            name: "long scan",
            hold: "",
            begin: "",
            simple: "SELECT count(*) FROM generate_series(1, 100000) a, generate_series(1, 100000) b",
            extended: "SELECT count(*) FROM generate_series(1, $1) a, generate_series(1, $1) b",
            params: || vec![Box::new(100000i32)],
            release: "",
        },
    ]
}

/// A CONTEXT's tuple position (`while updating tuple (0,7) ...`) without its numbers: row
/// versions' positions are storage internals.
fn mask_tuple(w: &str) -> String {
    match w.split_once("tuple (").and_then(|(a, b)| Some((a, b.split_once(')')?.1))) {
        Some((a, rest)) => format!("{a}tuple (…){rest}"),
        None => w.to_string(),
    }
}

fn outcome<T>(r: Result<T, tokio_postgres::Error>) -> String {
    match r {
        Ok(_) => "ok".into(),
        Err(e) => match e.as_db_error() {
            Some(d) => {
                let mut s = format!("{} {}", d.code().code(), d.message());
                if let Some(w) = d.where_() {
                    s.push_str(&format!(" (CONTEXT: {})", mask_tuple(w)));
                }
                s
            }
            None => format!("client error: {e}"),
        },
    }
}

async fn run_waiter(c: &Client, w: &Wait, proto: Proto) -> String {
    match proto {
        Proto::Simple => outcome(c.simple_query(w.simple).await),
        Proto::Extended => {
            let params = (w.params)();
            let refs: Vec<&(dyn ToSql + Sync)> =
                params.iter().map(|p| p.as_ref() as &(dyn ToSql + Sync)).collect();
            outcome(c.query(w.extended, &refs).await)
        }
    }
}

fn cancel_later(c: &Client) -> tokio::task::JoinHandle<()> {
    let token = c.cancel_token();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        token.cancel_query(NoTls).await.unwrap();
    })
}

async fn case(t: &Target, w: &Wait, end: End, proto: Proto) -> String {
    let holder = t.connect(DB).await;
    let waiter = t.connect(DB).await;
    if !w.hold.is_empty() {
        holder.batch_execute(w.hold).await.unwrap();
    }
    let begin = match (w.begin, end) {
        ("", End::LocalLockTimeout) => "BEGIN",
        (b, _) => b,
    };
    let prep = match end {
        End::StatementTimeout => format!("SET statement_timeout = 300; {begin}"),
        End::LockTimeout => format!("SET lock_timeout = 300; SET statement_timeout = 1500; {begin}"),
        End::LocalLockTimeout => {
            format!("{begin}; SET LOCAL lock_timeout = 300; SET LOCAL statement_timeout = 1500")
        }
        End::Cancel => begin.to_string(),
    };
    if !prep.trim().is_empty() {
        waiter.batch_execute(&prep).await.unwrap();
    }
    let canceller = matches!(end, End::Cancel).then(|| cancel_later(&waiter));
    let start = Instant::now();
    let got = run_waiter(&waiter, w, proto).await;
    let took = start.elapsed();
    if let Some(c) = canceller {
        c.await.unwrap();
    }
    let mut line = format!("{} / {end:?} / {proto:?}: {got}", w.name);
    if took > Duration::from_secs(10) {
        line.push_str(" [took over 10 s]");
    }
    line.push_str(&format!("; then {}", outcome(waiter.simple_query("SELECT 1").await)));
    if !begin.is_empty() {
        waiter.batch_execute("ROLLBACK").await.unwrap();
    }
    waiter.batch_execute("RESET ALL").await.unwrap();
    // The hold ends; the statement that waited runs at once (it left the wait queue).
    if !w.release.is_empty() {
        holder.batch_execute(w.release).await.unwrap();
    }
    if !w.hold.is_empty() {
        waiter.batch_execute("SET statement_timeout = 5000").await.unwrap();
        if !w.begin.is_empty() {
            waiter.batch_execute(w.begin).await.unwrap();
        }
        let again = run_waiter(&waiter, w, Proto::Simple).await;
        line.push_str(&format!("; after the hold: {again}"));
        waiter.batch_execute("ROLLBACK; RESET ALL").await.unwrap();
        // Leave the tables as they were.
        waiter.batch_execute("DELETE FROM u; SELECT pg_advisory_unlock_all()").await.unwrap();
    }
    line
}

/// The portal case: Execute fetches rows of a FOR UPDATE portal one at a time, and the second
/// one waits for a row the holder locked.
async fn portal_case(t: &Target, end: End) -> String {
    let holder = t.connect(DB).await;
    let mut waiter = t.connect(DB).await;
    holder.batch_execute("BEGIN; SELECT v FROM t WHERE id = 2 FOR UPDATE").await.unwrap();
    match end {
        End::StatementTimeout => waiter.batch_execute("SET statement_timeout = 300").await.unwrap(),
        End::LockTimeout | End::LocalLockTimeout => {
            waiter.batch_execute("SET lock_timeout = 300").await.unwrap()
        }
        End::Cancel => {}
    }
    let canceller = matches!(end, End::Cancel).then(|| cancel_later(&waiter));
    let tx = waiter.transaction().await.unwrap();
    let portal = tx.bind("SELECT id FROM t ORDER BY id FOR UPDATE", &[]).await.unwrap();
    let first = match tx.query_portal(&portal, 1).await {
        Ok(rows) => format!("{} row", rows.len()),
        Err(e) => outcome::<()>(Err(e)),
    };
    let second = outcome(tx.query_portal(&portal, 1).await);
    if let Some(c) = canceller {
        c.await.unwrap();
    }
    drop(tx);
    let after = outcome(waiter.simple_query("SELECT 1").await);
    holder.batch_execute("ROLLBACK").await.unwrap();
    format!("portal / {end:?}: first Execute {first}; second {second}; then {after}")
}

/// The session timeouts that end a connection with a FATAL (idle_in_transaction_session_
/// timeout, transaction_timeout, idle_session_timeout), and how they meet statement_timeout.
/// Each case: the steps' outcomes, then how the connection ended (its task's error).
async fn session_timeout_cases(t: &Target) -> Vec<String> {
    let cases: [(&str, &[&str], u64); 12] = [
        ("idle_in_transaction in a block", &["SET idle_in_transaction_session_timeout = 300", "BEGIN", "SELECT 1"], 800),
        ("idle_in_transaction outside a block", &["SET idle_in_transaction_session_timeout = 300", "SELECT 1"], 800),
        ("idle_in_transaction in a failed block", &["SET idle_in_transaction_session_timeout = 300", "BEGIN", "SELECT 1/0"], 800),
        ("transaction_timeout idle in a block", &["SET transaction_timeout = 300", "BEGIN", "SELECT 1"], 800),
        ("transaction_timeout while running", &["SET transaction_timeout = 300", "SELECT pg_sleep(3)"], 0),
        ("transaction_timeout while running, extended", &["SET transaction_timeout = 300", "$SELECT pg_sleep(3)"], 0),
        ("transaction_timeout in a lock wait", &["SET transaction_timeout = 300", "UPDATE t SET v = v WHERE id = 5"], 0),
        ("transaction_timeout = statement_timeout", &["SET transaction_timeout = 300", "SET statement_timeout = 300", "SELECT pg_sleep(3)"], 0),
        ("statement_timeout < transaction_timeout", &["SET transaction_timeout = 1000", "SET statement_timeout = 200", "SELECT pg_sleep(3)", "SELECT 2"], 0),
        ("transaction_timeout set in a block", &["BEGIN", "SET transaction_timeout = 300", "SELECT 1"], 800),
        ("idle_session outside a block", &["SET idle_session_timeout = 300", "SELECT 1"], 800),
        ("idle_session in a block", &["SET idle_session_timeout = 300", "BEGIN", "SELECT 1"], 800),
    ];
    let mut out = Vec::new();
    for (name, steps, idle) in cases {
        let holder = t.connect(DB).await;
        holder.batch_execute("BEGIN; SELECT v FROM t WHERE id = 5 FOR UPDATE").await.unwrap();
        let (c, ended) = connect_watched(t).await;
        let mut got = Vec::new();
        for s in steps {
            let r = match s.strip_prefix('$') {
                Some(q) => outcome(c.query(q, &[]).await),
                None => outcome(c.simple_query(s).await),
            };
            got.push(r);
        }
        tokio::time::sleep(Duration::from_millis(idle)).await;
        let next = outcome(c.simple_query("SELECT 2").await);
        drop(c);
        let end = match tokio::time::timeout(Duration::from_secs(5), ended).await {
            Ok(Ok(Ok(()))) => "closed cleanly".to_string(),
            Ok(Ok(Err(e))) => match e.as_db_error() {
                Some(d) => format!("{} {} {}", d.severity(), d.code().code(), d.message()),
                None => format!("client error: {e}"),
            },
            _ => "still open".to_string(),
        };
        holder.batch_execute("ROLLBACK").await.unwrap();
        // ("connection closed" is the client's word for a query on a closed connection.)
        let next = if next.starts_with("client error") { "connection closed".into() } else { next };
        out.push(format!("{name}: {}; then {next}; connection: {end}", got.join(", ")));
    }
    out
}

/// A connection whose task's end (the error it ended with: a FATAL sent while idle) is kept.
async fn connect_watched(
    t: &Target,
) -> (Client, tokio::task::JoinHandle<Result<(), tokio_postgres::Error>>) {
    let mut c = tokio_postgres::Config::new();
    c.host(&t.host).port(t.port).user("postgres").password(&t.password).dbname(DB);
    let (client, conn) = c.connect(NoTls).await.expect("connect");
    (client, tokio::spawn(conn))
}

/// A cancel that arrives while the session is idle in a transaction block cancels nothing:
/// the next statement runs.
async fn idle_cancel_case(t: &Target) -> String {
    let c = t.connect(DB).await;
    c.batch_execute("BEGIN").await.unwrap();
    c.cancel_token().cancel_query(NoTls).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let simple = outcome(c.simple_query("SELECT count(*) FROM generate_series(1, 300000)").await);
    c.cancel_token().cancel_query(NoTls).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let ext = outcome(c.query("SELECT count(*) FROM generate_series(1, $1)", &[&300000i32]).await);
    c.batch_execute("COMMIT").await.unwrap();
    format!("cancel while idle in a block: then simple {simple}; then extended {ext}")
}

/// A wait that never ends is a transcript line of its own, not a hung test.
async fn bounded(name: String, f: impl std::future::Future<Output = String>) -> String {
    tokio::time::timeout(Duration::from_secs(20), f)
        .await
        .unwrap_or_else(|_| format!("{name}: still waiting after 20 s"))
}

async fn transcript(t: &Target) -> Vec<String> {
    let admin = t.connect("postgres").await;
    admin.batch_execute(&format!("DROP DATABASE IF EXISTS {DB}")).await.unwrap();
    admin.batch_execute(&format!("CREATE DATABASE {DB}")).await.unwrap();
    let c = t.connect(DB).await;
    c.batch_execute(
        "CREATE TABLE t (id int PRIMARY KEY, v int);
         INSERT INTO t SELECT g, 0 FROM generate_series(1, 5) g;
         CREATE TABLE u (id int PRIMARY KEY);",
    )
    .await
    .unwrap();
    drop(c);
    let mut lines = Vec::new();
    let mut note = |line: String| {
        eprintln!("{line}");
        lines.push(line);
    };
    for w in waits() {
        for end in [End::StatementTimeout, End::LockTimeout, End::LocalLockTimeout, End::Cancel] {
            for proto in [Proto::Simple, Proto::Extended] {
                let name = format!("{} / {end:?} / {proto:?}", w.name);
                note(bounded(name, case(t, &w, end, proto)).await);
            }
        }
    }
    for end in [End::StatementTimeout, End::LockTimeout, End::Cancel] {
        note(bounded(format!("portal / {end:?}"), portal_case(t, end)).await);
    }
    note(bounded("idle cancel".into(), idle_cancel_case(t)).await);
    for line in session_timeout_cases(t).await {
        note(line);
    }
    // Let the sessions end before dropping the database.
    tokio::time::sleep(Duration::from_millis(300)).await;
    admin.batch_execute(&format!("DROP DATABASE {DB} WITH (FORCE)")).await.unwrap();
    lines
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

const EXPECTED: &str = include_str!("wire_waits.expected");

#[test]
fn waits_end_by_timeouts_and_cancel() {
    let t = Target::engine();
    let got = runtime().block_on(transcript(&t)).join("\n") + "\n";
    if got != EXPECTED {
        let (g, e): (Vec<&str>, Vec<&str>) = (got.lines().collect(), EXPECTED.lines().collect());
        let mut diff: Vec<String> = (0..g.len().max(e.len()))
            .filter(|&i| g.get(i) != e.get(i))
            .map(|i| format!("-{}\n+{}", e.get(i).unwrap_or(&""), g.get(i).unwrap_or(&"")))
            .collect();
        diff.truncate(20);
        panic!("transcript differs from PostgreSQL's:\n{}", diff.join("\n"));
    }
}

#[test]
#[ignore]
fn record_reference_transcript() {
    let Ok(addr) = std::env::var("WIRE_WAITS_REF") else { return };
    let t = Target::reference(&addr);
    let got = runtime().block_on(transcript(&t));
    println!("{}", got.join("\n"));
}

