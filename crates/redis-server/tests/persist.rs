//! Durability on object storage: log shipping, restore, crash recovery.

use bytes::Bytes;
use redis_engine::{Client, Config, Reply};
use redis_server::Database;
use redis_server::persist::{self, Objects, Shipper, ShipperConfig, Until};
use std::sync::Arc;
use std::time::Duration;

fn objects(name: &str) -> Objects {
    persist::test_objects(name)
}

fn cfg() -> ShipperConfig {
    // Long intervals: the tests flush explicitly.
    ShipperConfig { flush_interval: Duration::from_secs(3600), manifest_interval: Duration::from_secs(3600), ..Default::default() }
}

fn run(db: &Database, c: &mut Client, line: &str) -> Reply {
    let args: Vec<Bytes> = line.split(' ').map(|s| Bytes::copy_from_slice(s.as_bytes())).collect();
    db.engine(0).call(c, &args).unwrap_reply()
}

#[tokio::test]
async fn ships_and_restores() {
    let o = objects("restore");
    let shipper = Shipper::start("n1", o.clone(), cfg()).await.unwrap();
    let db = Database::new("db1", Config::default(), 1);
    let m = persist::restore(&o, &db, None).await.unwrap();
    shipper.attach(&db, m).await.unwrap();

    let mut c = Client::new(1);
    run(&db, &mut c, "SET a 1");
    run(&db, &mut c, "INCRBY a 41");
    run(&db, &mut c, "SET tmp x EX 1000");
    run(&db, &mut c, "SELECT 3");
    run(&db, &mut c, "SET b in3");
    run(&db, &mut c, "GET b"); // reads don't reach the log
    let offset = db.repl_offset();
    shipper.flush().await.unwrap();
    assert_eq!(*shipper.durable("db1").unwrap().borrow(), offset);
    run(&db, &mut c, "SELECT 0");
    run(&db, &mut c, "DEL tmp");
    let m = shipper.detach(&db).await.unwrap();
    assert_eq!(m.end, db.repl_offset());

    let db2 = Database::new("db1", Config::default(), 1);
    let m2 = persist::restore(&o, &db2, None).await.unwrap();
    assert_eq!(m2.end, db.repl_offset());
    assert_eq!(db2.repl_offset(), db.repl_offset());
    let mut c = Client::new(1);
    assert_eq!(run(&db2, &mut c, "GET a"), Reply::bulk(b"42"));
    assert_eq!(run(&db2, &mut c, "EXISTS tmp"), Reply::Int(0));
    run(&db2, &mut c, "SELECT 3");
    assert_eq!(run(&db2, &mut c, "GET b"), Reply::bulk(b"in3"));

    // Continue writing after the restore: the stream picks up where it left off.
    shipper.attach(&db2, m2).await.unwrap();
    run(&db2, &mut c, "SET c 3");
    shipper.detach(&db2).await.unwrap();
    let db3 = Database::new("db1", Config::default(), 1);
    persist::restore(&o, &db3, None).await.unwrap();
    let mut c = Client::new(1);
    run(&db3, &mut c, "SELECT 3");
    assert_eq!(run(&db3, &mut c, "GET c"), Reply::bulk(b"3"));
    assert_eq!(run(&db3, &mut c, "GET b"), Reply::bulk(b"in3"));
}

#[tokio::test]
async fn crash_recovery_uses_segments_after_the_manifest() {
    let o = objects("crash");
    let shipper = Shipper::start("n1", o.clone(), cfg()).await.unwrap();
    let a = Database::new("a", Config::default(), 1);
    let b = Database::new("b", Config::default(), 1);
    shipper.attach(&a, Default::default()).await.unwrap();
    shipper.attach(&b, Default::default()).await.unwrap();
    let mut c = Client::new(1);
    for i in 0..100 {
        run(&a, &mut c, &format!("SET k{i} {i}"));
        run(&b, &mut c, &format!("RPUSHX nothing {i}")); // errors/no-ops aren't logged
        run(&b, &mut c, &format!("SET b{i} x"));
        if i % 30 == 0 {
            shipper.flush().await.unwrap();
        }
    }
    shipper.flush().await.unwrap();
    // One segment per flush holds both databases.
    assert_eq!(shipper.objects_put.load(std::sync::atomic::Ordering::Relaxed), 5);
    // The node dies here: manifests were written only at attach time.
    drop(shipper);

    let a2 = Database::new("a", Config::default(), 1);
    persist::restore(&o, &a2, None).await.unwrap();
    assert_eq!(a2.total_keys(), 100);
    assert_eq!(a2.repl_offset(), a.repl_offset());
    let b2 = Database::new("b", Config::default(), 1);
    persist::restore(&o, &b2, None).await.unwrap();
    assert_eq!(b2.total_keys(), 100);

    // A restarted node continues the segment numbering.
    let s2 = Shipper::start("n1", o.clone(), cfg()).await.unwrap();
    assert_eq!(s2.last_seq(), 5);
}

#[tokio::test]
async fn idle_databases_cost_nothing() {
    let o = objects("idle");
    let shipper = Shipper::start("n1", o.clone(), cfg()).await.unwrap();
    let dbs: Vec<_> = (0..50).map(|i| Database::new(format!("d{i}"), Config::default(), 1)).collect();
    for d in &dbs {
        shipper.attach(d, Default::default()).await.unwrap();
    }
    shipper.flush().await.unwrap();
    assert_eq!(shipper.objects_put.load(std::sync::atomic::Ordering::Relaxed), 0);
    let mut c = Client::new(1);
    run(&dbs[7], &mut c, "SET only me");
    shipper.flush().await.unwrap();
    assert_eq!(shipper.objects_put.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[tokio::test]
async fn sync_writes_reply_after_durable() {
    let o = objects("sync");
    let shipper = Shipper::start("n1", o.clone(), cfg()).await.unwrap();
    let db = Database::new("s", Config::default(), 1);
    shipper.attach(&db, Default::default()).await.unwrap();
    db.sync_writes.store(true, std::sync::atomic::Ordering::Relaxed);
    let addr = redis_server::start_single(db.clone(), None).await.unwrap();
    let mut c = redis_server::client::RespClient::connect(addr).await.unwrap();
    // The flush interval is an hour: the write's reply forces an early flush.
    let r = tokio::time::timeout(Duration::from_secs(5), c.cmd(["SET", "k", "v"])).await.unwrap().unwrap();
    assert_eq!(r, Reply::Ok);
    assert_eq!(*shipper.durable("s").unwrap().borrow(), db.repl_offset());
    // Reads don't wait.
    assert_eq!(c.cmd(["GET", "k"]).await.unwrap(), Reply::bulk(b"v"));
    // WAITAOF reports the object-storage log as local durability.
    assert_eq!(c.cmd(["WAITAOF", "1", "0", "1000"]).await.unwrap(), Reply::Array(vec![Reply::Int(1), Reply::Int(0)]));
    assert_eq!(shipper.objects_put.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[tokio::test]
async fn point_in_time_and_branches() {
    use redis_server::node::{Node, NodeConfig, StaticCatalog};
    let o = objects("pitr");
    let shipper = Shipper::start("n1", o.clone(), cfg()).await.unwrap();
    let node = Node::new(Arc::new(StaticCatalog::default()), Some(shipper.clone()), NodeConfig { idle_timeout: None, domain: None });
    let db = Database::new("main", Config::default(), 1);
    shipper.attach(&db, Default::default()).await.unwrap();
    let mut c = Client::new(1);
    let mut marks = Vec::new();
    for v in 1..=3 {
        run(&db, &mut c, &format!("SET a {v}"));
        run(&db, &mut c, &format!("RPUSHX nolist {v}"));
        run(&db, &mut c, &format!("SET v{v} x"));
        shipper.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(15)).await;
        marks.push(redis_engine::util::now_ms());
        tokio::time::sleep(Duration::from_millis(15)).await;
    }

    // Branch as of the first mark: only the first round of writes.
    node.branch("main", "dev", Until::time(marks[0])).await.unwrap();
    let dev = Database::new("dev", Config::default(), 1);
    let m = persist::restore(&o, &dev, None).await.unwrap();
    let mut c2 = Client::new(2);
    assert_eq!(run(&dev, &mut c2, "GET a"), Reply::bulk(b"1"));
    assert_eq!(dev.total_keys(), 2);

    // The branch is independent: its writes don't touch the parent, and survive a restore.
    shipper.attach(&dev, m).await.unwrap();
    run(&dev, &mut c2, "SET a dev");
    run(&dev, &mut c2, "SET only-dev 1");
    shipper.detach(&dev).await.unwrap();
    assert_eq!(run(&db, &mut c, "GET a"), Reply::bulk(b"3"));
    let dev2 = Database::new("dev", Config::default(), 1);
    persist::restore(&o, &dev2, None).await.unwrap();
    assert_eq!(run(&dev2, &mut c2, "GET a"), Reply::bulk(b"dev"));
    assert_eq!(dev2.total_keys(), 3);

    // A branch of a branch.
    node.branch("dev", "dev-feature", Until::LATEST).await.unwrap();
    let f = Database::new("dev-feature", Config::default(), 1);
    persist::restore(&o, &f, None).await.unwrap();
    assert_eq!(run(&f, &mut c2, "GET only-dev"), Reply::bulk(b"1"));
    assert_eq!(run(&f, &mut c2, "GET v1"), Reply::bulk(b"x"));
    assert_eq!(run(&f, &mut c2, "GET v2"), Reply::Nil);

    // Point-in-time view of the parent itself.
    let past = Database::new("scratch", Config::default(), 1);
    persist::restore_at(&o, "main", &past, Until::time(marks[1]), None).await.unwrap();
    assert_eq!(run(&past, &mut c2, "GET a"), Reply::bulk(b"2"));
    assert_eq!(node.branch("main", "dev", Until::LATEST).await.unwrap_err().to_string(), "database dev already exists");
}
