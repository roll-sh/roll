//! MIGRATE between two roll servers; every reply is what Redis 8.10.2 answers in the same
//! situation (recorded with two `redis:8.10.3-alpine` containers).

use redis_engine::{Config, Reply};
use redis_server::client::RespClient;
use redis_server::{Database, start_single};

async fn cmd(c: &mut RespClient, line: &str) -> Reply {
    c.cmd(redis_server::client::args(line)).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migrate_like_redis() {
    let a = start_single(Database::new("a", Config::default(), 1), None).await.unwrap();
    let b = start_single(Database::new("b", Config::default(), 1), None).await.unwrap();
    let mut ca = RespClient::connect(a).await.unwrap();
    let mut cb = RespClient::connect(b).await.unwrap();
    let port = b.port();
    let err = |s: &str| Reply::Error(s.to_string());

    cmd(&mut ca, "SET k1 v1").await;
    cmd(&mut ca, "SET k2 v2 PX 100000").await;
    cmd(&mut ca, "RPUSH l a b c").await;
    assert_eq!(cmd(&mut ca, &format!("MIGRATE 127.0.0.1 {port} k1 0 1000")).await, Reply::Ok);
    assert_eq!(cmd(&mut ca, "EXISTS k1").await, Reply::Int(0));
    assert_eq!(cmd(&mut ca, &format!("MIGRATE 127.0.0.1 {port} nokey 0 1000")).await, Reply::status("NOKEY"));
    assert_eq!(cmd(&mut ca, &format!("MIGRATE 127.0.0.1 {port} \"\" 3 1000 COPY KEYS k2 l nokey")).await, Reply::Ok);
    assert_eq!(cmd(&mut ca, "EXISTS k2 l").await, Reply::Int(2));
    cmd(&mut ca, "SET k1 again").await;
    assert_eq!(
        cmd(&mut ca, &format!("MIGRATE 127.0.0.1 {port} k1 0 1000")).await,
        err("ERR Target instance replied with error: BUSYKEY Target key name already exists.")
    );
    assert_eq!(cmd(&mut ca, "EXISTS k1").await, Reply::Int(1));
    assert_eq!(cmd(&mut ca, &format!("MIGRATE 127.0.0.1 {port} k1 0 1000 REPLACE")).await, Reply::Ok);
    assert_eq!(cmd(&mut ca, &format!("MIGRATE 127.0.0.1 {port} k1 0 1000 AUTH pw")).await, Reply::status("NOKEY"));
    cmd(&mut ca, "SET k3 x").await;
    assert_eq!(
        cmd(&mut ca, &format!("MIGRATE 127.0.0.1 {port} k3 0 1000 AUTH pw")).await,
        err("ERR Target instance replied with error: ERR AUTH <password> called without any password configured for the default user. Are you sure your configuration is correct?")
    );
    assert_eq!(
        cmd(&mut ca, &format!("MIGRATE 127.0.0.1 {port} k3 0 1000 AUTH2 u pw")).await,
        err("ERR Target instance replied with error: WRONGPASS invalid username-password pair or user is disabled.")
    );
    assert_eq!(cmd(&mut ca, "MIGRATE 127.0.0.1 1 k3 0 100").await, err("IOERR error or timeout writing to target instance"));
    assert_eq!(cmd(&mut ca, "MIGRATE nosuchhost.invalid 6379 k3 0 300").await, err("IOERR error or timeout connecting to the client"));
    assert_eq!(cmd(&mut ca, &format!("MIGRATE 127.0.0.1 {port} k3 0 abc")).await, err("ERR value is not an integer or out of range"));
    assert_eq!(cmd(&mut ca, &format!("MIGRATE 127.0.0.1 {port} k3 abc 100")).await, err("ERR value is not an integer or out of range"));
    assert_eq!(
        cmd(&mut ca, &format!("MIGRATE 127.0.0.1 {port} k3 0 100 KEYS k3")).await,
        err("ERR When using MIGRATE KEYS option, the key argument must be set to the empty string")
    );
    for bad in ["BOGUS", "AUTH", "AUTH2 u"] {
        assert_eq!(cmd(&mut ca, &format!("MIGRATE 127.0.0.1 {port} k3 0 100 {bad}")).await, err("ERR syntax error"));
    }
    assert_eq!(
        cmd(&mut ca, &format!("MIGRATE 127.0.0.1 {port} k3 99 100")).await,
        err("ERR Target instance replied with error: ERR DB index is out of range")
    );
    assert_eq!(cmd(&mut ca, &format!("MIGRATE 127.0.0.1 {port} \"\" 0 100 KEYS")).await, Reply::status("NOKEY"));
    assert_eq!(cmd(&mut ca, "EXISTS k3").await, Reply::Int(1));

    assert_eq!(cmd(&mut cb, "GET k1").await, Reply::bulk(b"again"));
    cmd(&mut cb, "SELECT 3").await;
    assert_eq!(cmd(&mut cb, "LRANGE l 0 -1").await, Reply::Array(vec![Reply::bulk(b"a"), Reply::bulk(b"b"), Reply::bulk(b"c")]));
    assert!(matches!(cmd(&mut cb, "PTTL k2").await, Reply::Int(t) if t > 90_000));
    // Keys moved out replicate as DEL; COPY doesn't touch the source.
    assert_eq!(cmd(&mut ca, "COMMAND GETKEYS MIGRATE h 1 \"\" 0 1 COPY KEYS a b").await, Reply::Array(vec![Reply::bulk(b"a"), Reply::bulk(b"b")]));
}
