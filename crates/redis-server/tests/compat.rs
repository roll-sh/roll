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
//! !mask a,b CMD           replace the values following the fields `a` and `b` (in maps and flat
//!                         key/value arrays, at any depth, `a=…` tokens in text replies such
//!                         as CLIENT LIST, and `a:…` words in status replies such as DEBUG
//!                         OBJECT) before comparing (timestamps, ids…)
//! !info a,b* INFO …       compare INFO's sections and field names; values only for fields `a`
//!                         and those starting with `b` (`!info - …`: names only)
//! !ids CMD                in the reply, integers and `id=`/`redir=` tokens equal to a
//!                         connection's client id become `id:N` (ids differ between servers)
//! CLIENT KILL ID {id:2}   `{id:N}` expands to connection N's client id on each server
//! !send BLPOP l 5         send without reading the reply (read it later with @recv)
//! @recv                   read one reply on the current connection and compare
//! @recv ids               … with `!ids` applied
//! @conn 2                 switch to connection 2 (opened on first use)
//! @resp 3                 HELLO 3 on the current connection (both sides)
//! @sleep 100              pause (ms)
//! RPUSH l {9000*x}        `{N*text}` expands to `text` repeated N times (long values, many args)
//! ```
//!
//! A command after which both servers closed the connection counts as a match.
//!
//! `ROLL_COMPAT_REDIS=host:port` uses an existing reference server (`ROLL_COMPAT_DEBUG_REDIS` for
//! `tests/compat_debug`, one with `enable-debug-command yes`); otherwise a private Docker
//! container is started for the run. `ROLL_COMPAT_FILTER=lists` runs matching files only. Without
//! Docker the test is skipped unless `ROLL_COMPAT_REQUIRED=1`. `ROLL_COMPAT_DIR` runs scripts from
//! another directory. `ROLL_COMPAT_TRACE=1` prints each line
//! before running it (to find a hang).

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

/// The reference server: `env` names a running one, otherwise a container is started with
/// `args` for `redis-server`.
fn reference(env: &str, args: &[&str]) -> Option<(String, Option<Container>)> {
    if let Ok(addr) = std::env::var(env) {
        return Some((addr, None));
    }
    let name = format!("roll-compat-{}-{}", std::process::id(), redis_engine::util::now_us() % 1_000_000);
    let image = std::env::var("ROLL_COMPAT_IMAGE").unwrap_or_else(|_| "redis:8-alpine".into());
    let out = Command::new("docker")
        .args(["run", "-d", "--rm", "--name", &name, "-p", "127.0.0.1::6379", &image])
        .args(if args.is_empty() { vec![] } else { [&["redis-server"][..], args].concat() })
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
    Json,
}

/// Bulk strings holding JSON objects with their members in key order (RedisJSON answers a
/// multi-path JSON.GET from a hash map, so the order differs from run to run).
fn json_reply(r: &Reply) -> Reply {
    match r {
        Reply::Bulk(b) => match serde_json::from_slice::<serde_json::Value>(b) {
            Ok(v) => Reply::Bulk(serde_json::to_vec(&v).unwrap().into()),
            Err(_) => r.clone(),
        },
        Reply::Array(items) => Reply::Array(items.iter().map(json_reply).collect()),
        other => other.clone(),
    }
}

fn sort_reply(r: &Reply) -> Reply {
    match r {
        Reply::Array(items) | Reply::Set(items) => {
            // Nested arrays too (e.g. the elements of a hashtable HSCAN page).
            let mut v: Vec<Reply> = items.iter().map(sort_reply).collect();
            v.sort_by_key(|x| format!("{x:?}"));
            Reply::Array(v)
        }
        Reply::Map(pairs) => {
            let mut v: Vec<(Reply, Reply)> = pairs.iter().map(|(k, v)| (k.clone(), sort_reply(v))).collect();
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
        Reply::Verbatim(..) => "verbatim".into(),
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

/// Replace the value after each field named in `fields` (recursively).
fn mask(r: &Reply, fields: &[String]) -> Reply {
    let is_field = |x: &Reply| match x {
        Reply::Bulk(b) => fields.iter().any(|f| f.as_bytes() == &b[..]),
        Reply::Status(s) => fields.iter().any(|f| f == s),
        _ => false,
    };
    match r {
        Reply::Array(items) | Reply::Set(items) | Reply::Push(items) => {
            let mut out = Vec::with_capacity(items.len());
            let mut hide = false;
            for x in items {
                out.push(if hide { Reply::Nil } else { mask(x, fields) });
                hide = !hide && is_field(x);
            }
            Reply::Array(out)
        }
        Reply::Map(pairs) => {
            Reply::Map(pairs.iter().map(|(k, v)| (k.clone(), if is_field(k) { Reply::Nil } else { mask(v, fields) })).collect())
        }
        Reply::Bulk(b) | Reply::Verbatim(_, b) if b.contains(&b'=') => Reply::Bulk(map_tokens(b, |k, v| {
            if fields.iter().any(|f| f == k) { "*".to_string() } else { v.to_string() }
        })),
        Reply::Status(st) if st.contains(':') => Reply::Status(
            st.split(' ')
                .map(|w| match w.split_once(':') {
                    Some((k, _)) if fields.iter().any(|f| f == k) => format!("{k}:*"),
                    _ => w.to_string(),
                })
                .collect::<Vec<_>>()
                .join(" "),
        ),
        other => other.clone(),
    }
}

/// INFO text: keep section headers and field names (and their order); values only for the fields
/// matching `keep` (`name`, or `prefix*`).
fn info_fields(r: &Reply, keep: &[String], masked: &[String], sort: bool) -> Reply {
    let (Reply::Bulk(b) | Reply::Verbatim(_, b)) = r else { return r.clone() };
    let kept = |k: &str| keep.iter().any(|p| p.strip_suffix('*').map_or(p == k, |pre| k.starts_with(pre)));
    // `a=1,b=2` values: masked sub-fields become `*`.
    let sub = |v: &str| {
        v.split(',')
            .map(|t| match t.split_once('=') {
                Some((k, _)) if masked.iter().any(|m| m == k) => format!("{k}=*"),
                _ => t.to_string(),
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    let text = String::from_utf8_lossy(b);
    let mut lines: Vec<String> = text
        .split("\r\n")
        .map(|line| match line.split_once(':') {
            Some((k, v)) if !line.starts_with('#') => format!("{k}:{}", if kept(k) { sub(v) } else { "*".to_string() }),
            _ => line.to_string(),
        })
        .collect();
    if sort {
        // Within each section (Redis lists commands in hash-table order).
        let mut start = 0;
        for i in 0..=lines.len() {
            if i == lines.len() || lines[i].starts_with('#') || lines[i].is_empty() {
                lines[start..i].sort();
                start = i + 1;
            }
        }
    }
    Reply::Bulk(bytes::Bytes::from(lines.join("\r\n")))
}

/// Rewrite the values of `key=value` tokens in a text reply (CLIENT LIST lines).
fn map_tokens(b: &[u8], f: impl Fn(&str, &str) -> String) -> bytes::Bytes {
    let text = String::from_utf8_lossy(b);
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| {
            // `k=v` tokens, and `a=1,b=2` lists (INFO keyspace) item by item.
            line.split(' ')
                .map(|tok| {
                    tok.split(',')
                        .map(|item| match item.split_once('=') {
                            Some((k, v)) => format!("{k}={}", f(k, v)),
                            None => item.to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    bytes::Bytes::from(lines.join("\n"))
}

/// Replace known client ids (`ids[n]` is connection n's) with `id:n`.
fn name_ids(r: &Reply, ids: &HashMap<u32, i64>) -> Reply {
    let find = |v: i64| ids.iter().find(|(_, id)| **id == v).map(|(n, _)| format!("id:{n}"));
    match r {
        Reply::Int(v) => find(*v).map_or(Reply::Int(*v), Reply::Status),
        Reply::Array(items) | Reply::Set(items) | Reply::Push(items) => Reply::Array(items.iter().map(|x| name_ids(x, ids)).collect()),
        Reply::Map(pairs) => Reply::Map(pairs.iter().map(|(k, v)| (name_ids(k, ids), name_ids(v, ids))).collect()),
        Reply::Bulk(b) | Reply::Verbatim(_, b) if b.contains(&b'=') => Reply::Bulk(map_tokens(b, |k, v| match (k, v.parse::<i64>()) {
            ("id" | "redir", Ok(n)) => find(n).unwrap_or_else(|| v.to_string()),
            _ => v.to_string(),
        })),
        other => other.clone(),
    }
}

fn same(cmp: Cmp, a: &Reply, b: &Reply) -> bool {
    match cmp {
        // NaN doubles (RESP3) are the same reply.
        Cmp::Exact => a == b || format!("{a:?}") == format!("{b:?}"),
        Cmp::Sort => sort_reply(a) == sort_reply(b),
        Cmp::Type => shape(a) == shape(b),
        Cmp::Err => err_code(a).is_some() && err_code(a) == err_code(b),
        Cmp::Skip => true,
        Cmp::Json => json_reply(a) == json_reply(b),
    }
}

struct Pair {
    redis: RespClient,
    roll: RespClient,
    pending: Vec<(Cmp, String, usize)>,
    /// `CLIENT ID` on each side.
    ids: (i64, i64),
}

async fn open_pair(ref_addr: &str, roll_addr: std::net::SocketAddr) -> Pair {
    let mut redis = RespClient::connect(ref_addr).await.unwrap();
    let mut roll = RespClient::connect(roll_addr).await.unwrap();
    // Not authenticated yet (a script changed the default user) or BUSY: no id to substitute.
    let id = |r: Reply| match r {
        Reply::Int(n) => n,
        _ => -1,
    };
    let ids = (id(redis.cmd(["CLIENT", "ID"]).await.unwrap()), id(roll.cmd(["CLIENT", "ID"]).await.unwrap()));
    Pair { redis, roll, pending: Vec::new(), ids }
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
        c.cmd(["CLIENT", "UNPAUSE"]).await.unwrap();
        c.cmd(["FLUSHALL"]).await.unwrap();
        c.cmd(["SCRIPT", "FLUSH"]).await.unwrap();
        c.cmd(["FUNCTION", "FLUSH"]).await.unwrap();
        c.cmd(["CONFIG", "SET", "notify-keyspace-events", ""]).await.unwrap();
        // Connection limits a script may have changed and left behind if it failed midway.
        c.cmd([
            "CONFIG",
            "SET",
            "timeout",
            "0",
            "maxmemory-clients",
            "0",
            "proto-max-bulk-len",
            "512mb",
            "client-query-buffer-limit",
            "1gb",
            "client-output-buffer-limit",
            "normal 0 0 0 replica 256mb 64mb 60 pubsub 32mb 8mb 60",
        ])
        .await
        .unwrap();
        // ACL users and log.
        if let Ok(Reply::Array(users)) = c.cmd(["ACL", "USERS"]).await {
            for u in users {
                if let Reply::Bulk(name) = u
                    && &name[..] != b"default"
                {
                    c.cmd([&b"ACL"[..], b"DELUSER", &name]).await.unwrap();
                }
            }
        }
        c.cmd(["ACL", "SETUSER", "default", "reset", "on", "nopass", "~*", "&*", "+@all"]).await.unwrap();
        c.cmd(["ACL", "LOG", "RESET"]).await.unwrap();
    }

    for (lineno, raw) in text.lines().enumerate() {
        let lineno = lineno + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if std::env::var("ROLL_COMPAT_TRACE").is_ok() {
            eprintln!("{fname}:{lineno}: {line}");
        }
        if let std::collections::hash_map::Entry::Vacant(e) = conns.entry(cur) {
            e.insert(open_pair(ref_addr, roll_addr).await);
        }
        if let Some(rest) = line.strip_prefix('@') {
            let (dir, arg) = rest.split_once(' ').unwrap_or((rest, ""));
            match dir {
                "conn" => {
                    cur = arg.trim().parse().unwrap();
                    if let std::collections::hash_map::Entry::Vacant(e) = conns.entry(cur) {
                        e.insert(open_pair(ref_addr, roll_addr).await);
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
                    let ids_r: HashMap<u32, i64> = conns.iter().map(|(n, p)| (*n, p.ids.0)).collect();
                    let ids_o: HashMap<u32, i64> = conns.iter().map(|(n, p)| (*n, p.ids.1)).collect();
                    let p = conns.get_mut(&cur).unwrap();
                    let (cmp, what, at) = if p.pending.is_empty() { (Cmp::Exact, "(push)".to_string(), lineno) } else { p.pending.remove(0) };
                    let mut a = p.redis.read_timeout(5000).await.unwrap();
                    let mut b = p.roll.read_timeout(5000).await.unwrap();
                    if arg.trim() == "ids" {
                        a = a.map(|r| name_ids(&r, &ids_r));
                        b = b.map(|r| name_ids(&r, &ids_o));
                    }
                    match (a, b) {
                        (Some(a), Some(b)) if same(cmp, &a, &b) => {}
                        (a, b) => fails.push(format!("{fname}:{lineno} (sent at {at}): {what}\n    redis: {a:?}\n    roll:  {b:?}")),
                    }
                }
                other => panic!("{fname}:{lineno}: unknown directive @{other}"),
            }
            continue;
        }
        let mut info_keep: Option<Vec<String>> = None;
        let (cmp, send_only, cmdline, masked, by_id) = {
            let mut cmp = Cmp::Exact;
            let mut send = false;
            let mut by_id = false;
            let mut masked: Vec<String> = Vec::new();
            let mut l = line;
            while let Some(rest) = l.strip_prefix('!') {
                let (m, mut tail) = rest.split_once(' ').unwrap_or((rest, ""));
                match m {
                    "mask" => {
                        let (fields, t) = tail.split_once(' ').unwrap_or((tail, ""));
                        masked.extend(fields.split(',').map(str::to_string));
                        tail = t;
                    }
                    "sort" => cmp = Cmp::Sort,
                    "type" => cmp = Cmp::Type,
                    "err" => cmp = Cmp::Err,
                    "skip" => cmp = Cmp::Skip,
                    "json" => cmp = Cmp::Json,
                    "send" => send = true,
                    "ids" => by_id = true,
                    "info" => {
                        let (fields, t) = tail.split_once(' ').unwrap_or((tail, ""));
                        info_keep = Some(fields.split(',').filter(|f| *f != "-").map(str::to_string).collect());
                        tail = t;
                    }
                    other => panic!("{fname}:{lineno}: unknown modifier !{other}"),
                }
                l = tail.trim_start();
            }
            (cmp, send, l, masked, by_id)
        };
        let ids_r: HashMap<u32, i64> = conns.iter().map(|(n, p)| (*n, p.ids.0)).collect();
        let ids_o: HashMap<u32, i64> = conns.iter().map(|(n, p)| (*n, p.ids.1)).collect();
        let args_r = redis_server::client::args(&with_ids(&expand(cmdline), &ids_r));
        let args = redis_server::client::args(&with_ids(&expand(cmdline), &ids_o));
        assert!(!args.is_empty(), "{fname}:{lineno}: unbalanced quotes in {cmdline}");
        let p = conns.get_mut(&cur).unwrap();
        if send_only {
            p.redis.send(&args_r).await.unwrap();
            p.roll.send(&args).await.unwrap();
            p.pending.push((cmp, cmdline.to_string(), lineno));
            continue;
        }
        let shown = |r: Reply| match &info_keep {
            Some(k) => info_fields(&r, k, &masked, cmp == Cmp::Sort),
            None if masked.is_empty() => r,
            None => mask(&r, &masked),
        };
        let a = p.redis.cmd(&args_r).await.map(shown);
        let b = p.roll.read_timeout_after_send(&args).await.map(|r| r.map(shown));
        let (a, b) = if by_id { (a.map(|r| name_ids(&r, &ids_r)), b.map(|r| r.map(|r| name_ids(&r, &ids_o)))) } else { (a, b) };

        match (a, b) {
            (Ok(a), Ok(Some(b))) => {
                if !same(cmp, &a, &b) {
                    fails.push(format!("{fname}:{lineno}: {cmdline}\n    redis: {a:?}\n    roll:  {b:?}"));
                }
            }
            // Both closed the connection (e.g. its ACL user was deleted).
            (Err(_), Err(_)) => {}
            // Redis replied with an error and closed while the request was still being written:
            // whether the client sees the error or a broken pipe is a race in Redis too.
            (Err(e), Ok(Some(Reply::Error(_))))
                if matches!(e.kind(), std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset) => {}
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

/// Expand `{id:N}` into connection N's client id.
fn with_ids(line: &str, ids: &HashMap<u32, i64>) -> String {
    let mut out = line.to_string();
    for (n, id) in ids {
        out = out.replace(&format!("{{id:{n}}}"), &id.to_string());
    }
    out
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
    run_suite("ROLL_COMPAT_REDIS", &[], "tests/compat", Config::default()).await;
}

/// DEBUG needs `enable-debug-command` (immutable, so a server of its own): `tests/compat_debug`
/// runs against `ROLL_COMPAT_DEBUG_REDIS` or a container started with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compat_debug_with_redis() {
    let cfg = Config { enable_debug_command: redis_engine::config::PROTECTED_YES, ..Config::default() };
    run_suite("ROLL_COMPAT_DEBUG_REDIS", &["--enable-debug-command", "yes"], "tests/compat_debug", cfg).await;
}

async fn run_suite(env: &str, args: &[&str], scripts: &str, cfg: Config) {
    let Some((ref_addr, _guard)) = reference(env, args) else {
        if std::env::var("ROLL_COMPAT_REQUIRED").is_ok() {
            panic!("compat: no reference Redis (Docker unavailable)");
        }
        eprintln!("compat: skipped (no Docker); set {env}=host:port to use a running Redis");
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

    let db = Database::new("compat", cfg, 1);
    // Active expiry and INFO's samples, like a node runs.
    db.start_cron();
    let roll = start_single(db, None).await.unwrap();
    // RediSearch and RedisTimeSeries subscribe to keyspace events with their first index or
    // series and never unsubscribe; from then on their handlers' key lookups show (`keymiss`,
    // hit/miss stats, LFU). A long-running reference may already be subscribed, so both sides
    // start subscribed.
    if scripts == "tests/compat" {
        for addr in [ref_addr.clone(), roll.to_string()] {
            let mut c = RespClient::connect(&addr).await.unwrap();
            c.cmd(["FT.CREATE", "compat-subscribe", "ON", "HASH", "PREFIX", "1", "compat-subscribe:", "SCHEMA", "t", "TEXT"]).await.unwrap();
            c.cmd(["FT.DROPINDEX", "compat-subscribe"]).await.unwrap();
            c.cmd(["TS.CREATE", "compat-subscribe"]).await.unwrap();
            c.cmd(["DEL", "compat-subscribe"]).await.unwrap();
        }
    }

    let dir = std::env::var_os("ROLL_COMPAT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(scripts));
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
