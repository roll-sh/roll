//! Differential compatibility tests: every script in `tests/compat/*.txt` runs against real Redis
//! (Docker, `redis:8-alpine`) and against roll, and every reply must be identical.
//!
//! Script syntax (one command per line, `redis-cli` quoting):
//!
//! ```text
//! # comment
//! SET k v                 compare replies exactly
//! !sort SMEMBERS s        sort array elements (recursively at the top level) before comparing
//! !type RANDOMKEY         compare only the reply's shape (type and length)
//! !err CMD                both must be errors with the same code (ERR, WRONGTYPE…)
//! !skip CMD               run on both, don't compare
//! !send BLPOP l 5         send without reading the reply (read it later with @recv)
//! @recv                   read one reply on the current connection and compare
//! @conn 2                 switch to connection 2 (opened on first use)
//! @resp 3                 HELLO 3 on the current connection (both sides)
//! @sleep 100              pause (ms)
//! RPUSH l {9000*x}        `{N*text}` expands to `text` repeated N times (long values, many args)
//! ```
//!
//! `ROLL_COMPAT_REDIS=host:port` uses an existing reference server; otherwise a private Docker
//! container is started for the run. `ROLL_COMPAT_FILTER=lists` runs matching files only. Without
//! Docker the test is skipped unless `ROLL_COMPAT_REQUIRED=1`.

use redis_engine::{Config, Reply};
use redis_server::client::RespClient;
use redis_server::{Database, start_single};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

struct Container(String);

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}

fn reference() -> Option<(String, Option<Container>)> {
    if let Ok(addr) = std::env::var("ROLL_COMPAT_REDIS") {
        return Some((addr, None));
    }
    let name = format!("roll-compat-{}-{}", std::process::id(), redis_engine::util::now_us() % 1_000_000);
    let image = std::env::var("ROLL_COMPAT_IMAGE").unwrap_or_else(|_| "redis:8-alpine".into());
    let out = Command::new("docker")
        .args(["run", "-d", "--rm", "--name", &name, "-p", "127.0.0.1::6379", &image])
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!("compat: docker run failed: {}", String::from_utf8_lossy(&out.stderr));
        return None;
    }
    let guard = Container(name.clone());
    let port = Command::new("docker").args(["port", &name, "6379/tcp"]).output().ok()?;
    let port = String::from_utf8_lossy(&port.stdout);
    let addr = port.lines().next()?.trim().replace("0.0.0.0", "127.0.0.1");
    Some((addr, Some(guard)))
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Cmp {
    Exact,
    Sort,
    Type,
    Err,
    Skip,
}

fn sort_reply(r: &Reply) -> Reply {
    match r {
        Reply::Array(items) | Reply::Set(items) => {
            let mut v = items.clone();
            v.sort_by_key(|x| format!("{x:?}"));
            Reply::Array(v)
        }
        Reply::Map(pairs) => {
            let mut v = pairs.clone();
            v.sort_by_key(|(k, _)| format!("{k:?}"));
            Reply::Map(v)
        }
        Reply::Seq(items) => Reply::Seq(items.iter().map(sort_reply).collect()),
        other => other.clone(),
    }
}

fn shape(r: &Reply) -> String {
    match r {
        Reply::Array(v) => format!("array({})", v.len()),
        Reply::Set(v) => format!("set({})", v.len()),
        Reply::Map(v) => format!("map({})", v.len()),
        Reply::Bulk(_) => "bulk".into(),
        Reply::Int(_) => "int".into(),
        Reply::Error(_) => "error".into(),
        Reply::Status(_) | Reply::Ok => "status".into(),
        Reply::Nil => "nil".into(),
        Reply::NilArray => "nilarray".into(),
        Reply::Double(_) => "double".into(),
        other => format!("{other:?}"),
    }
}

fn err_code(r: &Reply) -> Option<String> {
    match r {
        Reply::Error(e) => Some(e.split(' ').next().unwrap_or("").to_string()),
        _ => None,
    }
}

fn same(cmp: Cmp, a: &Reply, b: &Reply) -> bool {
    match cmp {
        Cmp::Exact => a == b,
        Cmp::Sort => sort_reply(a) == sort_reply(b),
        Cmp::Type => shape(a) == shape(b),
        Cmp::Err => err_code(a).is_some() && err_code(a) == err_code(b),
        Cmp::Skip => true,
    }
}

struct Pair {
    redis: RespClient,
    roll: RespClient,
    pending: Vec<(Cmp, String, usize)>,
}

async fn run_file(path: &PathBuf, ref_addr: &str, roll_addr: std::net::SocketAddr) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap();
    let fname = path.file_name().unwrap().to_string_lossy().to_string();
    let mut fails = Vec::new();
    let mut conns: HashMap<u32, Pair> = HashMap::new();
    let mut cur = 1u32;

    // Fresh state on both sides.
    let mut admin_r = RespClient::connect(ref_addr).await.unwrap();
    let mut admin_o = RespClient::connect(roll_addr).await.unwrap();
    for c in [&mut admin_r, &mut admin_o] {
        c.cmd(["FLUSHALL"]).await.unwrap();
        c.cmd(["CONFIG", "SET", "notify-keyspace-events", ""]).await.unwrap();
    }

    for (lineno, raw) in text.lines().enumerate() {
        let lineno = lineno + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let std::collections::hash_map::Entry::Vacant(e) = conns.entry(cur) {
            let p = Pair {
                redis: RespClient::connect(ref_addr).await.unwrap(),
                roll: RespClient::connect(roll_addr).await.unwrap(),
                pending: Vec::new(),
            };
            e.insert(p);
        }
        if let Some(rest) = line.strip_prefix('@') {
            let (dir, arg) = rest.split_once(' ').unwrap_or((rest, ""));
            match dir {
                "conn" => {
                    cur = arg.trim().parse().unwrap();
                    if let std::collections::hash_map::Entry::Vacant(e) = conns.entry(cur) {
                        let p = Pair {
                            redis: RespClient::connect(ref_addr).await.unwrap(),
                            roll: RespClient::connect(roll_addr).await.unwrap(),
                            pending: Vec::new(),
                        };
                        e.insert(p);
                    }
                }
                "sleep" => tokio::time::sleep(std::time::Duration::from_millis(arg.trim().parse().unwrap())).await,
                "resp" => {
                    let p = conns.get_mut(&cur).unwrap();
                    let a = p.redis.cmd(["HELLO", arg.trim()]).await.unwrap();
                    let b = p.roll.cmd(["HELLO", arg.trim()]).await.unwrap();
                    if shape(&a) != shape(&b) {
                        fails.push(format!("{fname}:{lineno}: HELLO {arg}\n    redis: {a:?}\n    roll:  {b:?}"));
                    }
                }
                "recv" => {
                    let p = conns.get_mut(&cur).unwrap();
                    let (cmp, what, at) = if p.pending.is_empty() { (Cmp::Exact, "(push)".to_string(), lineno) } else { p.pending.remove(0) };
                    let a = p.redis.read_timeout(5000).await.unwrap();
                    let b = p.roll.read_timeout(5000).await.unwrap();
                    match (a, b) {
                        (Some(a), Some(b)) if same(cmp, &a, &b) => {}
                        (a, b) => fails.push(format!("{fname}:{lineno} (sent at {at}): {what}\n    redis: {a:?}\n    roll:  {b:?}")),
                    }
                }
                other => panic!("{fname}:{lineno}: unknown directive @{other}"),
            }
            continue;
        }
        let (cmp, send_only, cmdline) = {
            let mut cmp = Cmp::Exact;
            let mut send = false;
            let mut l = line;
            while let Some(rest) = l.strip_prefix('!') {
                let (m, tail) = rest.split_once(' ').unwrap_or((rest, ""));
                match m {
                    "sort" => cmp = Cmp::Sort,
                    "type" => cmp = Cmp::Type,
                    "err" => cmp = Cmp::Err,
                    "skip" => cmp = Cmp::Skip,
                    "send" => send = true,
                    other => panic!("{fname}:{lineno}: unknown modifier !{other}"),
                }
                l = tail.trim_start();
            }
            (cmp, send, l)
        };
        let args = redis_server::client::args(&expand(cmdline));
        let p = conns.get_mut(&cur).unwrap();
        if send_only {
            p.redis.send(&args).await.unwrap();
            p.roll.send(&args).await.unwrap();
            p.pending.push((cmp, cmdline.to_string(), lineno));
            continue;
        }
        let a = p.redis.cmd(&args).await;
        let b = p.roll.read_timeout_after_send(&args).await;
        match (a, b) {
            (Ok(a), Ok(Some(b))) => {
                if !same(cmp, &a, &b) {
                    fails.push(format!("{fname}:{lineno}: {cmdline}\n    redis: {a:?}\n    roll:  {b:?}"));
                }
            }
            (a, b) => fails.push(format!("{fname}:{lineno}: {cmdline}\n    redis: {a:?}\n    roll:  {b:?} (no reply / connection error)")),
        }
        // Commands like SUBSCRIBE a b reply several times: drain extra frames pairwise.
        if cmdline.to_ascii_lowercase().starts_with("subscribe")
            || cmdline.to_ascii_lowercase().starts_with("psubscribe")
            || cmdline.to_ascii_lowercase().starts_with("ssubscribe")
            || cmdline.to_ascii_lowercase().starts_with("unsubscribe")
            || cmdline.to_ascii_lowercase().starts_with("punsubscribe")
            || cmdline.to_ascii_lowercase().starts_with("sunsubscribe")
        {
            let extra = args.len().saturating_sub(2);
            for _ in 0..extra {
                let a = p.redis.read_timeout(2000).await.unwrap();
                let b = p.roll.read_timeout(2000).await.unwrap();
                if a != b {
                    fails.push(format!("{fname}:{lineno}: {cmdline} (extra reply)\n    redis: {a:?}\n    roll:  {b:?}"));
                }
            }
        }
    }
    fails
}

/// Expand `{N*text}` into `text` repeated N times.
fn expand(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(open) = rest.find('{') {
        let tail = &rest[open + 1..];
        let parsed = tail.find('}').and_then(|close| {
            let (n, text) = tail[..close].split_once('*')?;
            Some((n.parse::<usize>().ok()?, text, close))
        });
        match parsed {
            Some((n, text, close)) => {
                out.push_str(&rest[..open]);
                out.push_str(&text.repeat(n));
                rest = &tail[close + 1..];
            }
            None => {
                out.push_str(&rest[..=open]);
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

trait SendRead {
    async fn read_timeout_after_send(&mut self, args: &[Vec<u8>]) -> std::io::Result<Option<Reply>>;
}

impl SendRead for RespClient {
    async fn read_timeout_after_send(&mut self, args: &[Vec<u8>]) -> std::io::Result<Option<Reply>> {
        self.send(args).await?;
        self.read_timeout(10_000).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compat_with_redis() {
    let Some((ref_addr, _guard)) = reference() else {
        if std::env::var("ROLL_COMPAT_REQUIRED").is_ok() {
            panic!("compat: no reference Redis (Docker unavailable)");
        }
        eprintln!("compat: skipped (no Docker); set ROLL_COMPAT_REDIS=host:port to use a running Redis");
        return;
    };
    // Wait for the reference to accept connections.
    let mut ready = false;
    for _ in 0..100 {
        if let Ok(mut c) = RespClient::connect(&ref_addr).await
            && let Ok(Reply::Status(s)) = c.cmd(["PING"]).await
            && s == "PONG"
        {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(ready, "reference Redis at {ref_addr} not ready");

    let db = Database::new("compat", Config::default(), 1);
    let roll = start_single(db, None).await.unwrap();

    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/compat");
    let filter = std::env::var("ROLL_COMPAT_FILTER").ok();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "txt"))
        .filter(|p| filter.as_ref().is_none_or(|f| p.to_string_lossy().contains(f.as_str())))
        .collect();
    files.sort();
    let mut all = Vec::new();
    for f in &files {
        let fails = run_file(f, &ref_addr, roll).await;
        eprintln!("compat: {} — {}", f.file_name().unwrap().to_string_lossy(), if fails.is_empty() { "ok".to_string() } else { format!("{} mismatches", fails.len()) });
        all.extend(fails);
    }
    if !all.is_empty() {
        panic!("{} compatibility mismatches:\n{}", all.len(), all.join("\n"));
    }
}
