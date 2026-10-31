//! Round 7's sessions over the wire: one engine for the whole process (a database per seed),
//! served on a port of DIFFTEST_WIRE_PORTS (52761-52779, the first free one), so sessions 2
//! and 3 of a seed reach both servers through the same client code, statements run over the
//! extended protocol, and whatever the engine keeps across sessions and databases (templates,
//! plans) is shared as in a server.
//!
//! A statement of a case that starts with a backslash is a step of these sessions:
//!
//! - `\s2 <sql>`: a simple query on session 2 (or 3); its tag shows the transaction status
//!   after it (`[I]`, `[T]`, `[E]`);
//! - `\s2& <sql>`: sent without waiting: it either finishes or waits for a lock (the tag
//!   `(waiting)`, decided by `pg_isolation_test_session_is_blocked`, never by a timer);
//! - `\s2.`: the result of the statement session 2 was waiting on;
//! - `\x2 <options>` and the statement on the lines after: the extended protocol. Options:
//!   `name=<statement>` (default unnamed), `types=<oid>,...` (Parse's parameter types),
//!   `rf=<format>,...` (result formats), `rows=<n>` (Execute's row limit), `describe`
//!   (Describe the statement), `p=<param>` once per parameter: `null`, `t'<text>'` or
//!   `b<hex>` (binary). A statement of `-` binds the statement prepared before.

use super::{Field, Out, PgClient, SESSION_SETUP, env, out_of};
use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::server::{AuthMethod, ServerOptions};
use pg_engine::session::Session;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub struct Shared {
    pub engine: Arc<Engine>,
    pub addr: String,
}

/// The process's engine and the address it serves.
pub fn shared() -> &'static Shared {
    static S: OnceLock<Shared> = OnceLock::new();
    S.get_or_init(|| {
        // (Leaked: the engine lives as long as the process.)
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        let engine = Engine::open(EngineOptions {
            data_dir: dir.path().to_path_buf(),
            cache_bytes: 4 << 20,
            memtable_bytes: 2 << 20,
            sync: false,
            idle_close: Duration::from_secs(3600),
            superuser: "postgres".into(),
            password: None,
            server_files: None,
            server_programs: false,
            shared_preload_libraries: Vec::new(),
            locale: Default::default(),
            outbound: Vec::new(),
            max_prepared_transactions: 0,
            auth_files: Default::default(),
            settings: Vec::new(),
        })
        .expect("open the shared engine");
        let ports = env("DIFFTEST_WIRE_PORTS", "52761-52779");
        let (a, b) = ports.split_once('-').unwrap_or((&ports, &ports));
        let (a, b): (u16, u16) = (a.parse().unwrap(), b.parse().unwrap());
        let addr = (a..=b)
            .find_map(|p| pg_engine::server::spawn(engine.clone(), &format!("127.0.0.1:{p}"), ServerOptions { auth: AuthMethod::Trust }).ok())
            .expect("a free port in DIFFTEST_WIRE_PORTS")
            .to_string();
        Shared { engine, addr }
    })
}

static PIDS: AtomicI32 = AtomicI32::new(30000);

/// An in-process session of the shared engine on `db`, and its pid.
pub fn connect(db: &str) -> (Session, i32) {
    let pid = PIDS.fetch_add(1, Ordering::Relaxed);
    let s = shared().engine.connect(pid, db, "postgres", "difftest").expect("connect to the shared engine");
    (s, pid)
}

/// A fresh database `db` on the shared engine (made as the reference's is), and session 1 on it.
pub fn create_db(db: &str) -> (Session, i32) {
    let (mut admin, _) = connect("postgres");
    for q in [
        format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"),
        format!("CREATE DATABASE {db} TEMPLATE template0 LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8' LOCALE 'C' ENCODING 'UTF8'"),
    ] {
        admin.query(&q).unwrap_or_else(|e| panic!("{q}: {}", e.message));
    }
    connect(db)
}

/// Drop `db` on the shared engine; `s1` (session 1 on it) is closed first.
pub fn drop_db(s1: &mut Session, db: &str) {
    let (admin, _) = connect("postgres");
    drop(std::mem::replace(s1, admin));
    let _ = s1.query(&format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"));
}

struct Side {
    c: [PgClient; 2],
    pids: [i32; 2],
    /// Whether session 2 / 3's last statement is waiting for a lock.
    waiting: [bool; 2],
    mon: PgClient,
    /// The pids `pg_isolation_test_session_is_blocked` looks for blockers among.
    all: String,
}

pub struct Wire {
    /// (None once dropped while hung.)
    eng: Option<Side>,
    refs: Side,
    /// A read on the engine's side timed out: its sessions are left as they are.
    pub hung: bool,
}

fn client(addr: &str, db: &str, password: &str) -> PgClient {
    let mut c = PgClient::connect(addr, "postgres", password, db).expect("connect a wire session");
    for s in SESSION_SETUP {
        c.simple_query(s).unwrap();
    }
    c
}

fn pid_of(c: &mut PgClient) -> i32 {
    let ev = c.simple_query("SELECT pg_backend_pid()").unwrap();
    out_of(ev).rows.first().and_then(|r| r.parse().ok()).unwrap_or(0)
}

impl Side {
    fn open(addr: &str, db: &str, password: &str, s1_pid: i32) -> Side {
        let mut c = [client(addr, db, password), client(addr, db, password)];
        let pids = [pid_of(&mut c[0]), pid_of(&mut c[1])];
        let hang = Duration::from_secs(env("DIFFTEST_HANG_SECS", "60").parse().unwrap());
        for x in &mut c {
            x.set_timeout(Some(hang)).unwrap();
        }
        let mut mon = client(addr, db, password);
        mon.set_timeout(Some(hang)).unwrap();
        Side { c, pids, waiting: [false; 2], mon, all: format!("{s1_pid},{},{}", pids[0], pids[1]) }
    }
}

impl Wire {
    pub fn open(db: &str, eng_s1: i32, ref_s1: i32, ref_addr: &str, password: &str) -> Wire {
        Wire {
            eng: Some(Side::open(&shared().addr, db, "", eng_s1)),
            refs: Side::open(ref_addr, db, password, ref_s1),
            hung: false,
        }
    }

    /// Run a step (the statement after its backslash) on the reference, then on the engine.
    pub fn run(&mut self, step: &str) -> (Out, Out) {
        // (A step the reference can't finish is the case's, not the engine's: a reduction that
        // took the other session's COMMIT away. The reducer rejects it.)
        let r = run_side(&mut self.refs, step).unwrap_or_else(|e| Out {
            error: Some(Field { code: "XX000".into(), message: format!("reference session: {e}"), ..Field::default() }),
            ..Out::default()
        });
        let Some(eng) = self.eng.as_mut() else {
            return (hang("hung before"), r);
        };
        let e = match run_side(eng, step) {
            Ok(o) => o,
            Err(err) => {
                self.hung = true;
                hang(&err.to_string())
            }
        };
        (e, r)
    }
}

/// The engine's side of a step that didn't finish (`Class::Hang`).
fn hang(msg: &str) -> Out {
    Out { error: Some(Field { code: "HANG".into(), message: format!("wire: {msg}"), ..Field::default() }), ..Out::default() }
}

impl Drop for Wire {
    fn drop(&mut self) {
        if self.hung {
            // (A session stuck in the engine: closing it could wait as long.)
            std::mem::forget(self.eng.take());
        }
    }
}

fn status(mut o: Out, c: &PgClient) -> Out {
    o.tag = format!("{} [{}]", o.tag, c.status as char);
    // (A deadlock's DETAIL names pids and transaction ids.)
    if let Some(f) = o.error.as_mut().filter(|f| f.code == "40P01") {
        f.detail = None;
    }
    o
}

/// One step on one side.
fn run_side(s: &mut Side, step: &str) -> std::io::Result<Out> {
    let (head, rest) = step.split_once(char::is_whitespace).unwrap_or((step, ""));
    let mut ch = head.chars();
    let kind = ch.next().unwrap_or(' ');
    let bad = || std::io::Error::other(format!("bad step: {head}"));
    let i = match ch.next() {
        Some('2') => 0,
        Some('3') => 1,
        _ => return Err(bad()),
    };
    let rest = rest.trim();
    match (kind, ch.as_str()) {
        ('s', "") => {
            let ev = s.c[i].simple_query(rest)?;
            Ok(status(out_of(ev), &s.c[i]))
        }
        ('s', "&") => {
            s.c[i].send_query(rest)?;
            wait_or_block(s, i)
        }
        ('s', ".") => {
            if !s.waiting[i] {
                return Ok(Out { tag: "(not waiting)".into(), ..Out::default() });
            }
            s.waiting[i] = false;
            let ev = s.c[i].results()?;
            Ok(status(out_of(ev), &s.c[i]))
        }
        ('x', "") => {
            let x = Ext::parse(rest);
            let sql = (x.sql != "-").then_some(x.sql.as_str());
            let ev = s.c[i].extended(&x.name, sql, &x.types, x.describe, &x.params, &x.rf, x.rows)?;
            Ok(status(out_of(ev), &s.c[i]))
        }
        _ => Err(bad()),
    }
}

/// A statement just sent on session `i`: its result, or `(waiting)` once the session waits
/// for a lock (asked of the server, so it doesn't depend on timing).
fn wait_or_block(s: &mut Side, i: usize) -> std::io::Result<Out> {
    let hang = Duration::from_secs(env("DIFFTEST_HANG_SECS", "60").parse().unwrap());
    let start = std::time::Instant::now();
    let q = format!("SELECT pg_isolation_test_session_is_blocked({}, '{{{}}}')", s.pids[i], s.all);
    loop {
        if s.c[i].readable_within(Duration::from_millis(5))? {
            let ev = s.c[i].results()?;
            return Ok(status(out_of(ev), &s.c[i]));
        }
        let o = out_of(s.mon.simple_query(&q)?);
        if o.rows.first().map(String::as_str) == Some("t") {
            s.waiting[i] = true;
            return Ok(Out { tag: "(waiting)".into(), ..Out::default() });
        }
        if start.elapsed() > hang {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "neither finished nor waiting for a lock"));
        }
    }
}

/// An extended-protocol step's options and statement.
struct Ext {
    name: String,
    types: Vec<u32>,
    rf: Vec<i16>,
    rows: i32,
    describe: bool,
    params: Vec<(i16, Option<Vec<u8>>)>,
    sql: String,
}

impl Ext {
    fn parse(src: &str) -> Ext {
        let (head, sql) = src.split_once('\n').unwrap_or((src, "-"));
        let mut x = Ext { name: String::new(), types: Vec::new(), rf: Vec::new(), rows: 0, describe: false, params: Vec::new(), sql: sql.trim().to_string() };
        for tok in words(head) {
            let (k, v) = tok.split_once('=').unwrap_or((tok.as_str(), ""));
            match k {
                "name" => x.name = v.to_string(),
                "types" => x.types = v.split(',').filter_map(|t| t.parse().ok()).collect(),
                "rf" => x.rf = v.split(',').filter_map(|t| t.parse().ok()).collect(),
                "rows" => x.rows = v.parse().unwrap_or(0),
                "describe" => x.describe = true,
                "p" => x.params.push(param(v)),
                _ => {}
            }
        }
        x
    }
}

/// `null`, `t'<text>'` (quotes doubled) or `b<hex>`.
fn param(v: &str) -> (i16, Option<Vec<u8>>) {
    if let Some(t) = v.strip_prefix("t'") {
        return (0, Some(t.strip_suffix('\'').unwrap_or(t).replace("''", "'").into_bytes()));
    }
    if let Some(h) = v.strip_prefix('b') {
        let digits: Vec<u8> = h.bytes().filter(u8::is_ascii_hexdigit).collect();
        let bytes = digits.chunks(2).map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap()).collect();
        return (1, Some(bytes));
    }
    (0, None)
}

/// The header's words: split on spaces outside `'...'`.
fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        if c == '\'' {
            quoted = !quoted;
        }
        if c.is_whitespace() && !quoted {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}
