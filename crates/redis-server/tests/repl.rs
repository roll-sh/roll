//! Read replicas, WAIT, storage-tailing replicas, resync and promotion.

use bytes::Bytes;
use redis_engine::{Client, Config, Reply};
use redis_server::Database;
use redis_server::client::RespClient;
use redis_server::persist::{self, Objects, Shipper, ShipperConfig};
use redis_server::repl::{self, Follower, PrimaryMap};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

fn objects(name: &str) -> Objects {
    persist::test_objects(name)
}

fn run(db: &Database, c: &mut Client, line: &str) -> Reply {
    let args: Vec<Bytes> = line.split(' ').map(|s| Bytes::copy_from_slice(s.as_bytes())).collect();
    db.engine(0).call(c, &args).unwrap_reply()
}

async fn repl_server(map: Arc<PrimaryMap>, durable: u64) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(repl::serve(l, map, "s3cret".into(), Arc::new(move |_: &str| durable)));
    addr
}

async fn eventually(mut f: impl FnMut() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached");
}

#[tokio::test]
async fn streaming_replica_and_wait() {
    let primary = Database::new("p", Config::default(), 1);
    let map = Arc::new(PrimaryMap::default());
    let backlog = map.add(primary.clone(), 1 << 20);
    let addr = repl_server(map, 0).await;

    let replica_db = Database::new("p", Config::default(), 1);
    let f = Follower::new(replica_db.clone());
    tokio::spawn(f.clone().stream(addr, "p".into(), "s3cret".into(), None));

    let mut c = Client::new(1);
    run(&primary, &mut c, "SET a 1");
    run(&primary, &mut c, "SELECT 2");
    run(&primary, &mut c, "SET b 2 PX 100000");
    run(&primary, &mut c, "INCRBYFLOAT f 1.5");
    let end = primary.repl_offset();
    // WAIT 1 returns once the replica acknowledged everything written so far.
    let n = repl::wait_acks(&backlog, end, 1, Duration::from_secs(5)).await;
    assert_eq!(n, 1);
    assert_eq!(f.applied(), end);

    let mut rc = Client::new(2);
    assert_eq!(run(&replica_db, &mut rc, "GET a"), Reply::bulk(b"1"));
    run(&replica_db, &mut rc, "SELECT 2");
    assert_eq!(run(&replica_db, &mut rc, "GET f"), Reply::bulk(b"1.5"));
    assert!(matches!(run(&replica_db, &mut rc, "PTTL b"), Reply::Int(t) if t > 90_000));
    // Read-only.
    assert_eq!(run(&replica_db, &mut rc, "SET x 1"), Reply::error("READONLY You can't write against a read only replica."));

    // Failover: promote, and the new primary is writable and continues the stream offset.
    f.promote();
    assert_eq!(run(&replica_db, &mut rc, "SET x 1"), Reply::Ok);
    assert!(replica_db.repl_offset() > end);
}

#[tokio::test]
async fn wait_over_resp() {
    let primary = Database::new("p", Config::default(), 1);
    let map = Arc::new(PrimaryMap::default());
    map.add(primary.clone(), 1 << 20);
    let raddr = repl_server(map, 0).await;
    let addr = redis_server::start_single(primary.clone(), None).await.unwrap();
    let mut c = RespClient::connect(addr).await.unwrap();
    assert_eq!(c.cmd(["SET", "k", "v"]).await.unwrap(), Reply::Ok);
    // No replica yet: times out with 0.
    assert_eq!(c.cmd(["WAIT", "1", "50"]).await.unwrap(), Reply::Int(0));
    let f = Follower::new(Database::new("p", Config::default(), 1));
    tokio::spawn(f.clone().stream(raddr, "p".into(), "s3cret".into(), None));
    assert_eq!(c.cmd(["SET", "k", "v2"]).await.unwrap(), Reply::Ok);
    assert_eq!(c.cmd(["WAIT", "1", "5000"]).await.unwrap(), Reply::Int(1));
    assert_eq!(c.cmd(["WAIT", "0", "0"]).await.unwrap(), Reply::Int(1));
}

#[tokio::test]
async fn bad_secret_is_refused() {
    let primary = Database::new("p", Config::default(), 1);
    let map = Arc::new(PrimaryMap::default());
    map.add(primary, 1 << 20);
    let addr = repl_server(map, 0).await;
    let mut c = RespClient::connect(addr).await.unwrap();
    assert_eq!(c.cmd(["REPLICATE", "p", "0", "nope"]).await.unwrap(), Reply::error("NOAUTH bad replication secret"));
}

#[tokio::test]
async fn resync_from_storage_when_backlog_is_outrun() {
    let o = objects("resync");
    let shipper = Shipper::start("n1", o.clone(), ShipperConfig { flush_interval: Duration::from_secs(3600), ..Default::default() }).await.unwrap();
    let primary = Database::new("p", Config::default(), 1);
    shipper.attach(&primary, Default::default()).await.unwrap();
    // A tiny backlog: the replica can't catch up from offset 0 through it.
    let map = Arc::new(PrimaryMap::default());
    map.add(primary.clone(), 64);
    let mut c = Client::new(1);
    for i in 0..200 {
        run(&primary, &mut c, &format!("SET k{i} {i}"));
    }
    shipper.flush().await.unwrap();
    let durable = primary.repl_offset();
    let addr = repl_server(map, durable).await;

    let f = Follower::new(Database::new("p", Config::default(), 1));
    tokio::spawn(f.clone().stream(addr, "p".into(), "s3cret".into(), Some(o.clone())));
    eventually(|| f.connected.load(std::sync::atomic::Ordering::SeqCst)).await;
    assert_eq!(f.applied(), durable);
    run(&primary, &mut c, "SET after resync");
    let end = primary.repl_offset();
    eventually(|| f.applied() == end).await;
    assert_eq!(f.db.total_keys(), 201);
}

#[tokio::test]
async fn storage_tailing_replica() {
    let o = objects("tail");
    let shipper = Shipper::start("n1", o.clone(), ShipperConfig { flush_interval: Duration::from_millis(10), ..Default::default() }).await.unwrap();
    let primary = Database::new("p", Config::default(), 1);
    let m = persist::restore(&o, &primary, None).await.unwrap();
    shipper.attach(&primary, m).await.unwrap();

    let f = Follower::new(Database::new("p", Config::default(), 1));
    tokio::spawn(f.clone().tail_storage(o.clone(), "p".into(), Duration::from_millis(10)));
    let mut c = Client::new(1);
    for i in 0..50 {
        run(&primary, &mut c, &format!("SET k{i} {i}"));
    }
    let end = primary.repl_offset();
    eventually(|| f.applied() == end).await;
    assert_eq!(f.db.total_keys(), 50);
    run(&primary, &mut c, "DEL k0");
    let end = primary.repl_offset();
    eventually(|| f.applied() == end).await;
    assert_eq!(f.db.total_keys(), 49);
}
