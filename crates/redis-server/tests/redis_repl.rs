//! Redis' replication protocol against real Redis 8 (Docker): a Redis replica following roll,
//! `redis-cli --rdb`, roll following a Redis primary with REPLICAOF, and failover both ways with
//! partial resynchronization. Skipped without Docker unless ROLL_COMPAT_REQUIRED is set.

use redis_engine::{Config, Reply};
use redis_server::client::RespClient;
use redis_server::Database;
use std::net::SocketAddr;
use std::process::Command;
use std::time::Duration;

struct Container(String);

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}

impl Container {
    fn start() -> Option<(Self, String)> {
        let name = format!("roll-psync-{}-{}", std::process::id(), redis_engine::util::now_us() % 1_000_000);
        let image = std::env::var("ROLL_COMPAT_IMAGE").unwrap_or_else(|_| "redis:8-alpine".into());
        let out = Command::new("docker")
            .args(["run", "-d", "--rm", "--name", &name, "-p", "127.0.0.1::6379", &image, "redis-server", "--save", "", "--repl-diskless-sync-delay", "0"])
            .output()
            .ok()?;
        if !out.status.success() {
            eprintln!("redis_repl: docker run failed: {}", String::from_utf8_lossy(&out.stderr));
            return None;
        }
        let c = Container(name);
        let port = Command::new("docker").args(["port", &c.0, "6379/tcp"]).output().ok()?;
        let addr = String::from_utf8_lossy(&port.stdout).lines().next()?.trim().replace("0.0.0.0", "127.0.0.1");
        Some((c, addr))
    }

    /// `redis-cli` inside the container (`-p 6390`: the replica started there).
    fn cli(&self, args: &[&str]) -> String {
        let out = Command::new("docker").args(["exec", &self.0, "redis-cli"]).args(args).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn sh(&self, script: &str) -> String {
        let out = Command::new("docker").args(["exec", &self.0, "sh", "-c", script]).output().unwrap();
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
    }
}

async fn cmd(c: &mut RespClient, args: &[&str]) -> Reply {
    c.cmd(args.iter().copied()).await.unwrap()
}

fn text(r: &Reply) -> String {
    match r {
        Reply::Bulk(b) | Reply::Verbatim(_, b) => String::from_utf8_lossy(b).into_owned(),
        Reply::Status(s) => s.clone(),
        Reply::Error(e) => e.clone(),
        Reply::Int(i) => i.to_string(),
        Reply::Ok => "OK".into(),
        Reply::Nil => "(nil)".into(),
        other => format!("{other:?}"),
    }
}

/// An INFO field.
fn field(info: &str, name: &str) -> Option<String> {
    info.lines().find_map(|l| l.strip_prefix(&format!("{name}:")).map(|v| v.trim().to_string()))
}

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..300 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn eventually_async<F: std::future::Future<Output = bool>>(what: &str, mut f: impl FnMut() -> F) {
    for _ in 0..300 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn info(c: &mut RespClient, section: &str) -> String {
    text(&cmd(c, &["INFO", section]).await)
}

async fn roll_server() -> (SocketAddr, RespClient) {
    let db = Database::new("psync", Config::default(), 1);
    db.start_cron();
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    // Reachable from the container through host.docker.internal.
    let addr = start_single_on(db, port).await;
    let c = RespClient::connect(addr).await.unwrap();
    (addr, c)
}

async fn start_single_on(db: std::sync::Arc<Database>, port: u16) -> SocketAddr {
    redis_server::start_single_at(db, None, SocketAddr::from(([0, 0, 0, 0], port))).await.unwrap();
    SocketAddr::from(([127, 0, 0, 1], port))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redis_replication_protocol() {
    let Some((ctr, master_addr)) = Container::start() else {
        if std::env::var("ROLL_COMPAT_REQUIRED").is_ok() {
            panic!("redis_repl: Docker unavailable");
        }
        eprintln!("redis_repl: skipped (no Docker)");
        return;
    };
    let mut master = None;
    for _ in 0..100 {
        if let Ok(mut c) = RespClient::connect(&master_addr).await
            && matches!(c.cmd(["PING"]).await, Ok(Reply::Status(s)) if s == "PONG")
        {
            master = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut m = master.expect("Redis in Docker not ready");

    // ---- a Redis replica follows roll ----
    let (roll_addr, mut r) = roll_server().await;
    let rport = roll_addr.port().to_string();
    assert_eq!(text(&cmd(&mut r, &["SET", "a", "1"]).await), "OK");
    cmd(&mut r, &["HSET", "h", "f", "v"]).await;
    cmd(&mut r, &["RPUSH", "l", "x", "y"]).await;
    cmd(&mut r, &["SET", "exp", "v", "EX", "1000"]).await;
    // Template hashes: the snapshot has template records and keys referencing them.
    cmd(&mut r, &["HIMPORT", "PREPARE", "fs", "b", "a", "ccc"]).await;
    cmd(&mut r, &["HIMPORT", "SET", "t1", "fs", "1", "2", "3"]).await;
    cmd(&mut r, &["HIMPORT", "SET", "t2", "fs", "1", &"v".repeat(100), "3"]).await;
    cmd(&mut r, &["FUNCTION", "LOAD", "#!lua name=rlib\nredis.register_function('rf', function() return 1 end)"]).await;
    cmd(&mut r, &["SELECT", "2"]).await;
    cmd(&mut r, &["SET", "b", "2"]).await;
    cmd(&mut r, &["SELECT", "0"]).await;
    // Time series: a compaction rule (its links travel in the snapshot), labels to reindex.
    cmd(&mut r, &["TS.CREATE", "tsrc", "LABELS", "k", "v"]).await;
    cmd(&mut r, &["TS.CREATE", "tdst"]).await;
    cmd(&mut r, &["TS.CREATERULE", "tsrc", "tdst", "AGGREGATION", "sum", "10"]).await;
    cmd(&mut r, &["TS.MADD", "tsrc", "1", "1", "tsrc", "5", "2", "tsrc", "12", "3"]).await;
    // The image's modules (its entrypoint loads them for the main server), for the module values.
    let modules = "$(for m in /usr/local/lib/redis/modules/*.so; do printf -- '--loadmodule %s ' \"$m\"; done)";
    let started = ctr.sh(&format!("redis-server --port 6390 {modules} --replicaof host.docker.internal {rport} --daemonize yes --save '' --logfile /tmp/r.log"));
    let synced = || ctr.cli(&["-p", "6390", "INFO", "replication"]).contains("master_link_status:up");
    for _ in 0..300 {
        if synced() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(synced(), "the Redis replica didn't sync:\n{}", ctr.sh("tail -30 /tmp/r.log"));
    assert!(ctr.cli(&["-p", "6390", "GET", "a"]) == "1", "{started}{}", ctr.sh("cat /tmp/r.log"));
    assert_eq!(ctr.cli(&["-p", "6390", "HGET", "h", "f"]), "v");
    assert_eq!(ctr.cli(&["-p", "6390", "LRANGE", "l", "0", "-1"]), "x\ny");
    assert_eq!(ctr.cli(&["-p", "6390", "-n", "2", "GET", "b"]), "2");
    assert!(ctr.cli(&["-p", "6390", "TTL", "exp"]).parse::<i64>().unwrap() > 900);
    assert!(ctr.cli(&["-p", "6390", "FUNCTION", "LIST"]).contains("rlib"));
    assert_eq!(ctr.cli(&["-p", "6390", "OBJECT", "ENCODING", "t1"]), "template-listpack");
    assert_eq!(ctr.cli(&["-p", "6390", "HGETALL", "t1"]), "a\n2\nb\n1\nccc\n3");
    assert_eq!(ctr.cli(&["-p", "6390", "OBJECT", "ENCODING", "t2"]), "template-array");
    assert_eq!(field(&ctr.cli(&["-p", "6390", "INFO", "stats"]), "hash_template_keys").as_deref(), Some("2"));
    assert_eq!(ctr.cli(&["-p", "6390", "TS.RANGE", "tdst", "-", "+"]), "0\n3");
    assert_eq!(ctr.cli(&["-p", "6390", "TS.RANGE", "tdst", "-", "+", "LATEST"]), "0\n3\n10\n3");
    assert_eq!(ctr.cli(&["-p", "6390", "TS.QUERYINDEX", "k=v"]), "tsrc");
    // The stream after the snapshot (HIMPORT SET goes as a RESTORE).
    cmd(&mut r, &["HIMPORT", "SET", "t3", "fs", "4", "5", "6"]).await;
    cmd(&mut r, &["SET", "c", "3"]).await;
    cmd(&mut r, &["DEL", "a"]).await;
    cmd(&mut r, &["TS.ADD", "tsrc", "25", "4"]).await;
    cmd(&mut r, &["TS.INCRBY", "tsrc", "1", "TIMESTAMP", "26"]).await;
    cmd(&mut r, &["INCR", "n"]).await;
    cmd(&mut r, &["INCR", "n"]).await;
    cmd(&mut r, &["SELECT", "2"]).await;
    cmd(&mut r, &["DEL", "b"]).await;
    cmd(&mut r, &["SELECT", "0"]).await;
    eventually("writes to reach the Redis replica", || ctr.cli(&["-p", "6390", "GET", "n"]) == "2").await;
    assert_eq!(ctr.cli(&["-p", "6390", "GET", "c"]), "3");
    assert_eq!(ctr.cli(&["-p", "6390", "OBJECT", "ENCODING", "t3"]), "template-listpack");
    assert_eq!(ctr.cli(&["-p", "6390", "HGETALL", "t3"]), "a\n5\nb\n4\nccc\n6");
    assert_eq!(ctr.cli(&["-p", "6390", "EXISTS", "a"]), "0");
    assert_eq!(ctr.cli(&["-p", "6390", "-n", "2", "EXISTS", "b"]), "0");
    assert_eq!(ctr.cli(&["-p", "6390", "TS.RANGE", "tdst", "-", "+"]), "0\n3\n10\n3");
    assert_eq!(ctr.cli(&["-p", "6390", "TS.GET", "tsrc"]), "26\n5");
    // The primary's view of it.
    let i = info(&mut r, "replication").await;
    assert_eq!(field(&i, "connected_slaves").as_deref(), Some("1"), "{i}");
    assert!(field(&i, "slave0").is_some_and(|s| s.contains("port=6390") && s.contains("state=online")), "{i}");
    assert!(text(&cmd(&mut r, &["CLIENT", "LIST", "TYPE", "replica"]).await).contains("flags=S"));
    let role = cmd(&mut r, &["ROLE"]).await;
    assert!(format!("{role:?}").contains("6390"), "{role:?}");
    // WAIT gets its acknowledgement through REPLCONF GETACK, well before the 1s ACK period.
    cmd(&mut r, &["SET", "w", "1"]).await;
    let t = std::time::Instant::now();
    assert_eq!(cmd(&mut r, &["WAIT", "1", "5000"]).await, Reply::Int(1));
    assert!(t.elapsed() < Duration::from_millis(900), "WAIT took {:?}", t.elapsed());
    // A dropped link resumes with a partial resynchronization.
    ctr.cli(&["-p", "6390", "CLIENT", "KILL", "TYPE", "master"]);
    eventually_async("a partial resync", || {
        let addr = roll_addr;
        async move {
            let mut c = RespClient::connect(addr).await.unwrap();
            field(&info(&mut c, "stats").await, "sync_partial_ok").as_deref() == Some("1")
        }
    })
    .await;
    cmd(&mut r, &["SET", "after", "resume"]).await;
    eventually("writes after the resync", || ctr.cli(&["-p", "6390", "GET", "after"]) == "resume").await;
    assert_eq!(field(&info(&mut r, "stats").await, "sync_full").as_deref(), Some("1"));
    ctr.cli(&["-p", "6390", "SHUTDOWN", "NOSAVE"]);
    // (redis-check-rdb has no modules: keep module values out of the snapshot it checks.)
    cmd(&mut r, &["DEL", "tsrc", "tdst"]).await;

    // ---- redis-cli --rdb: a snapshot Redis' own checker accepts ----
    let out = ctr.sh(&format!("redis-cli -h host.docker.internal -p {rport} --rdb /tmp/roll.rdb && redis-check-rdb /tmp/roll.rdb"));
    assert!(out.contains("RDB looks OK"), "{out}");
    let out = ctr.sh(&format!("redis-cli -h host.docker.internal -p {rport} --functions-rdb /tmp/f.rdb && redis-check-rdb /tmp/f.rdb"));
    assert!(out.contains("RDB looks OK") && out.contains("functions"), "{out}");

    // ---- roll follows a Redis primary ----
    cmd(&mut m, &["FLUSHALL"]).await;
    cmd(&mut m, &["SET", "m1", "v"]).await;
    cmd(&mut m, &["HSET", "mh", "a", "1", "b", "2"]).await;
    cmd(&mut m, &["ZADD", "mz", "1", "a", "2", "b"]).await;
    cmd(&mut m, &["XADD", "ms", "1-1", "f", "v"]).await;
    cmd(&mut m, &["SET", "mexp", "v", "EX", "1000"]).await;
    cmd(&mut m, &["HIMPORT", "PREPARE", "fs", "y", "x"]).await;
    cmd(&mut m, &["HIMPORT", "SET", "mt", "fs", "1", "2"]).await;
    cmd(&mut m, &["TS.CREATE", "msrc", "LABELS", "q", "1"]).await;
    cmd(&mut m, &["TS.CREATE", "mdst"]).await;
    cmd(&mut m, &["TS.CREATERULE", "msrc", "mdst", "AGGREGATION", "max", "10"]).await;
    cmd(&mut m, &["TS.MADD", "msrc", "1", "7", "msrc", "11", "2"]).await;
    cmd(&mut m, &["FUNCTION", "LOAD", "REPLACE", "#!lua name=mlib\nredis.register_function{function_name='mf', callback=function() return 2 end, flags={'no-writes'}}"]).await;
    let (r2_addr, mut r2) = roll_server().await;
    let mport = master_addr.rsplit(':').next().unwrap().to_string();
    cmd(&mut r2, &["SET", "stale", "x"]).await;
    assert_eq!(text(&cmd(&mut r2, &["REPLICAOF", "127.0.0.1", &mport]).await), "OK");
    assert_eq!(text(&cmd(&mut r2, &["REPLICAOF", "127.0.0.1", &mport]).await), "OK Already connected to specified master");
    eventually_async("roll to sync from Redis", || {
        let addr = r2_addr;
        async move {
            let mut c = RespClient::connect(addr).await.unwrap();
            field(&info(&mut c, "replication").await, "master_link_status").as_deref() == Some("up")
        }
    })
    .await;
    assert_eq!(text(&cmd(&mut r2, &["GET", "m1"]).await), "v");
    assert_eq!(cmd(&mut r2, &["EXISTS", "stale"]).await, Reply::Int(0));
    assert_eq!(text(&cmd(&mut r2, &["HGET", "mh", "b"]).await), "2");
    assert_eq!(text(&cmd(&mut r2, &["ZSCORE", "mz", "b"]).await), "2");
    assert_eq!(cmd(&mut r2, &["XLEN", "ms"]).await, Reply::Int(1));
    assert_eq!(text(&cmd(&mut r2, &["OBJECT", "ENCODING", "mt"]).await), "template-listpack");
    assert_eq!(text(&cmd(&mut r2, &["HGET", "mt", "y"]).await), "1");
    assert_eq!(cmd(&mut r2, &["FCALL_RO", "mf", "0"]).await, Reply::Int(2));
    assert_eq!(format!("{:?}", cmd(&mut r2, &["TS.RANGE", "mdst", "-", "+"]).await), "Array([Array([Int(0), Status(\"7\")])])");
    assert_eq!(format!("{:?}", cmd(&mut r2, &["TS.QUERYINDEX", "q=1"]).await), "Array([Bulk(b\"msrc\")])");
    assert!(text(&cmd(&mut r2, &["SET", "x", "1"]).await).starts_with("READONLY"));
    let i = info(&mut r2, "replication").await;
    assert_eq!(field(&i, "role").as_deref(), Some("slave"), "{i}");
    assert_eq!(field(&i, "master_port").as_deref(), Some(mport.as_str()), "{i}");
    let mi = info(&mut m, "replication").await;
    assert_eq!(field(&mi, "connected_slaves").as_deref(), Some("1"), "{mi}");
    assert_eq!(field(&i, "master_replid"), field(&mi, "master_replid"));
    // The stream: plain writes, a transaction, a script's effects, expiry.
    cmd(&mut m, &["SET", "m2", "w"]).await;
    cmd(&mut m, &["HIMPORT", "SET", "mt2", "fs", "3", "4"]).await;
    cmd(&mut m, &["DEL", "m1"]).await;
    cmd(&mut m, &["MULTI"]).await;
    cmd(&mut m, &["INCR", "tx"]).await;
    cmd(&mut m, &["INCR", "tx"]).await;
    cmd(&mut m, &["EXEC"]).await;
    cmd(&mut m, &["EVAL", "redis.call('SET', 'ev', 'x') return 1", "0"]).await;
    cmd(&mut m, &["SET", "gone", "v", "PX", "50"]).await;
    cmd(&mut m, &["TS.ADD", "msrc", "21", "9"]).await;
    eventually_async("the stream from Redis", || {
        let addr = r2_addr;
        async move {
            let mut c = RespClient::connect(addr).await.unwrap();
            text(&cmd(&mut c, &["GET", "ev"]).await) == "x"
        }
    })
    .await;
    assert_eq!(text(&cmd(&mut r2, &["GET", "m2"]).await), "w");
    assert_eq!(text(&cmd(&mut r2, &["OBJECT", "ENCODING", "mt2"]).await), "template-listpack");
    assert_eq!(text(&cmd(&mut r2, &["HGET", "mt2", "x"]).await), "4");
    assert_eq!(cmd(&mut r2, &["EXISTS", "m1"]).await, Reply::Int(0));
    assert_eq!(text(&cmd(&mut r2, &["GET", "tx"]).await), "2");
    assert_eq!(format!("{:?}", cmd(&mut r2, &["TS.RANGE", "mdst", "-", "+"]).await), "Array([Array([Int(0), Status(\"7\")]), Array([Int(10), Status(\"2\")])])");
    let role = format!("{:?}", cmd(&mut r2, &["ROLE"]).await);
    assert!(role.contains("slave") && role.contains("connected"), "{role}");
    // Redis' WAIT is answered through our ACKs.
    assert_eq!(cmd(&mut m, &["WAIT", "1", "3000"]).await, Reply::Int(1));
    // Offsets agree once quiet.
    eventually_async("offsets to converge", || {
        let (addr, maddr) = (r2_addr, master_addr.clone());
        async move {
            let mut c = RespClient::connect(addr).await.unwrap();
            let mut mc = RespClient::connect(maddr).await.unwrap();
            field(&info(&mut c, "replication").await, "master_repl_offset") == field(&info(&mut mc, "replication").await, "master_repl_offset")
        }
    })
    .await;
    // Redis drops the link: roll continues with PSYNC.
    cmd(&mut m, &["CLIENT", "KILL", "TYPE", "replica"]).await;
    eventually_async("roll's partial resync", || {
        let maddr = master_addr.clone();
        async move {
            let mut mc = RespClient::connect(maddr).await.unwrap();
            field(&info(&mut mc, "stats").await, "sync_partial_ok").as_deref() == Some("1")
        }
    })
    .await;
    cmd(&mut m, &["SET", "m3", "after-psync"]).await;
    eventually_async("writes after roll's resync", || {
        let addr = r2_addr;
        async move {
            let mut c = RespClient::connect(addr).await.unwrap();
            text(&cmd(&mut c, &["GET", "m3"]).await) == "after-psync"
        }
    })
    .await;

    // ---- failover: roll becomes the primary, Redis follows it and continues partially ----
    let redis_id = field(&info(&mut m, "replication").await, "master_replid").unwrap();
    assert_eq!(text(&cmd(&mut r2, &["REPLICAOF", "NO", "ONE"]).await), "OK");
    let i = info(&mut r2, "replication").await;
    assert_eq!(field(&i, "role").as_deref(), Some("master"));
    assert_eq!(field(&i, "master_replid2").as_deref(), Some(redis_id.as_str()), "{i}");
    assert_eq!(text(&cmd(&mut r2, &["SET", "promoted", "yes"]).await), "OK");
    assert_eq!(text(&cmd(&mut r2, &["GET", "m3"]).await), "after-psync");
    let r2port = r2_addr.port().to_string();
    assert_eq!(text(&cmd(&mut m, &["REPLICAOF", "host.docker.internal", &r2port]).await), "OK");
    eventually("Redis to follow roll", || ctr.cli(&["GET", "promoted"]) == "yes").await;
    assert_eq!(field(&info(&mut r2, "stats").await, "sync_partial_ok").as_deref(), Some("1"), "expected a partial resync after failover");
    assert_eq!(field(&info(&mut r2, "stats").await, "sync_full").as_deref(), Some("0"));
    cmd(&mut r2, &["SET", "last", "1"]).await;
    eventually("writes after failover", || ctr.cli(&["GET", "last"]) == "1").await;
    cmd(&mut m, &["REPLICAOF", "NO", "ONE"]).await;
    drop(ctr);
}
