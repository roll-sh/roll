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
    let node = Node::new(Arc::new(StaticCatalog::default()), Some(shipper.clone()), NodeConfig { idle_timeout: None, domain: None, ..Default::default() });
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

/// Strings only: enough to test the snapshot pipeline without the RDB codec.
struct StringCodec;

impl persist::SnapshotCodec for StringCodec {
    fn save(&self, engine: &redis_engine::Engine, out: &mut Vec<u8>) -> anyhow::Result<()> {
        for (i, db) in engine.dbs.iter().enumerate() {
            for (k, e) in db.keys.iter() {
                let redis_engine::value::Value::Str(v) = &e.value else { anyhow::bail!("strings only") };
                for part in [&(i as u32).to_be_bytes()[..], &(k.len() as u32).to_be_bytes(), k, &(v.len() as u32).to_be_bytes(), v, &e.expire.to_be_bytes()] {
                    out.extend_from_slice(part);
                }
            }
        }
        Ok(())
    }

    fn load(&self, db: &Arc<Database>, mut data: &[u8]) -> anyhow::Result<()> {
        use bytes::Buf;
        let mut eng = db.engine(0);
        while data.has_remaining() {
            let i = data.get_u32() as usize;
            let n = data.get_u32() as usize;
            let k = data.copy_to_bytes(n);
            let n = data.get_u32() as usize;
            let v = data.copy_to_bytes(n);
            let mut entry = redis_engine::value::Entry::new(redis_engine::value::Value::str(&v), 0);
            entry.expire = data.get_i64();
            eng.dbs[i].insert_new(&k, entry);
        }
        Ok(())
    }
}

#[tokio::test]
async fn snapshots_retention_and_garbage_collection() {
    use redis_server::node::{Node, NodeConfig, StaticCatalog};
    let o = objects("gc");
    let codec: Arc<dyn persist::SnapshotCodec> = Arc::new(StringCodec);
    let shipper = Shipper::start("n1", o.clone(), ShipperConfig { codec: Some(codec.clone()), ..cfg() }).await.unwrap();
    let node = Node::new(Arc::new(StaticCatalog::default()), Some(shipper.clone()), NodeConfig { idle_timeout: None, ..Default::default() });
    let db = Database::new("main", Config::default(), 1);
    shipper.attach(&db, Default::default()).await.unwrap();
    let mut c = Client::new(1);
    for i in 0..5 {
        run(&db, &mut c, &format!("SET k{i} {i}"));
        shipper.flush().await.unwrap();
    }
    assert_eq!(shipper.log_stats("main").unwrap(), persist::LogStats { bytes: db.repl_offset(), segments: 5, based: false });

    // A branch made now must survive whatever the parent's retention drops later.
    node.branch("main", "dev", Until::LATEST).await.unwrap();
    run(&db, &mut c, "SET k0 changed");
    run(&db, &mut c, "SET 2 1");
    let s1 = shipper.snapshot(&db, 0).await.unwrap().unwrap();
    assert_eq!(s1.offset, db.repl_offset());
    assert_eq!(shipper.snapshot(&db, 0).await.unwrap(), None, "nothing changed since");
    assert_eq!(shipper.log_stats("main").unwrap().segments, 0);
    run(&db, &mut c, "SET after snap");
    shipper.flush().await.unwrap();
    // A newer snapshot with zero retention drops the first one from the manifest.
    run(&db, &mut c, "SET k1 changed");
    let s2 = shipper.snapshot(&db, 0).await.unwrap().unwrap();
    let m = shipper.manifest("main").unwrap();
    assert_eq!(m.snapshots, vec![s2.clone()]);
    assert!(m.segments.is_empty() && m.base.is_none());

    // Two-phase: the first pass only notes what's unreferenced.
    let window = Duration::from_millis(100);
    let first = persist::collect_garbage(&o, window).await.unwrap();
    assert_eq!(first.deleted, 0);
    assert!(first.unreferenced > 0);
    tokio::time::sleep(window).await;
    let second = persist::collect_garbage(&o, window).await.unwrap();
    assert_eq!(second.deleted, first.unreferenced);
    assert!(!o.exists(&s1.key).await.unwrap(), "the dropped snapshot is gone");
    assert!(o.exists(&s2.key).await.unwrap());

    // The parent loads from its snapshot plus nothing else…
    let main2 = Database::new("main", Config::default(), 1);
    persist::restore(&o, &main2, Some(&*codec)).await.unwrap();
    let mut c2 = Client::new(2);
    assert_eq!(run(&main2, &mut c2, "MGET k0 k1 after k4"), run(&db, &mut c, "MGET k0 k1 after k4"));
    assert_eq!(main2.repl_offset(), db.repl_offset());
    // …and the branch still has the history it was made from.
    let dev = Database::new("dev", Config::default(), 1);
    persist::restore(&o, &dev, Some(&*codec)).await.unwrap();
    assert_eq!(run(&dev, &mut c2, "MGET k0 k1 after"), Reply::Array(vec![Reply::bulk(b"0"), Reply::bulk(b"1"), Reply::Nil]));
    assert_eq!(dev.total_keys(), 5);

    // Writes after a snapshot replay on top of it.
    run(&db, &mut c, "SET k2 later");
    shipper.detach(&db).await.unwrap();
    let main3 = Database::new("main", Config::default(), 1);
    persist::restore(&o, &main3, Some(&*codec)).await.unwrap();
    assert_eq!(run(&main3, &mut c2, "GET k2"), Reply::bulk(b"later"));
}

#[tokio::test]
async fn databases_snapshot_on_unload() {
    use redis_server::node::{DbSpec, Node, NodeConfig, SnapshotPolicy, StaticCatalog};
    let o = objects("unload-snap");
    let codec: Arc<dyn persist::SnapshotCodec> = Arc::new(StringCodec);
    let shipper = Shipper::start("n1", o.clone(), ShipperConfig { codec: Some(codec), ..cfg() }).await.unwrap();
    let catalog = Arc::new(StaticCatalog::default());
    catalog.insert(DbSpec::new("a", "pw"));
    let snapshots = SnapshotPolicy { unload_segments: 2, ..Default::default() };
    let node = Node::new(catalog, Some(shipper.clone()), NodeConfig { idle_timeout: None, snapshots, ..Default::default() });
    let spec = DbSpec::new("a", "pw");
    let db = node.open(&spec).await.unwrap();
    let mut c = Client::new(1);
    for i in 0..2 {
        run(&db, &mut c, &format!("SET k{i} {i}"));
        shipper.flush().await.unwrap();
    }
    assert!(node.unload("a").await.unwrap());
    assert!(persist::read_manifest(&o, "a").await.unwrap().unwrap().snapshots.is_empty(), "2 segments: under the threshold");
    let db = node.open(&spec).await.unwrap();
    run(&db, &mut c, "SET k2 2");
    shipper.flush().await.unwrap();
    drop(db);
    assert!(node.unload("a").await.unwrap());
    let m = persist::read_manifest(&o, "a").await.unwrap().unwrap();
    assert_eq!(m.snapshots.len(), 1);
    assert!(m.segments.is_empty());
    let db = node.open(&spec).await.unwrap();
    assert_eq!(run(&db, &mut c, "MGET k0 k1 k2"), Reply::Array(vec![Reply::bulk(b"0"), Reply::bulk(b"1"), Reply::bulk(b"2")]));
}

/// Order-insensitive form of a reply (hashtable sets and hashes iterate in hash order).
fn sorted(r: Reply) -> Vec<String> {
    let mut v: Vec<String> = match r {
        Reply::Array(v) | Reply::Set(v) => v.iter().map(|r| format!("{r:?}")).collect(),
        Reply::Map(v) => v.iter().map(|p| format!("{p:?}")).collect(),
        r => vec![format!("{r:?}")],
    };
    v.sort();
    v
}

#[tokio::test]
async fn rdb_snapshots_round_trip_every_type() {
    use redis_server::RdbCodec;
    let o = objects("rdb-snap");
    let codec: Arc<dyn persist::SnapshotCodec> = Arc::new(RdbCodec);
    let shipper = Shipper::start("n1", o.clone(), ShipperConfig { codec: Some(codec.clone()), ..cfg() }).await.unwrap();
    let db = Database::new("rdb", Config::default(), 1);
    shipper.attach(&db, Default::default()).await.unwrap();
    let mut c = Client::new(1);
    let many = |prefix: &str, n: usize| (0..n).map(|i| format!("{prefix}{i}")).collect::<Vec<_>>().join(" ");
    let pairs = |n: usize| (0..n).map(|i| format!("f{i} v{i}")).collect::<Vec<_>>().join(" ");
    let scored = |n: usize| (0..n).map(|i| format!("{} m{i}", i as f64 / 4.0)).collect::<Vec<_>>().join(" ");
    let far = "4102444800000";
    let lines = [
        "SET str hello".to_string(),
        "SET int -12345".into(),
        format!("SET lzf {}", "abc".repeat(40)),
        format!("SET ttl x PXAT {far}"),
        "RPUSH list a b 1 -2".into(),
        format!("RPUSH quick {}", many("item-", 1200)),
        "SADD intset 1 2 3 100000".into(),
        "SADD setlp x y z".into(),
        format!("SADD setht {}", many("m", 300)),
        "ZADD zlp 1 a 2.5 b -inf c".into(),
        format!("ZADD zskip {}", scored(300)),
        "HSET hlp f v n 42".into(),
        format!("HSET hht {}", pairs(600)),
        "HSET hlpex a 1 b 2".into(),
        format!("HPEXPIREAT hlpex {far} FIELDS 1 a"),
        format!("HSET hhtex {}", pairs(600)),
        format!("HPEXPIREAT hhtex {far} FIELDS 2 f1 f2"),
        "XADD stream 1-0 n 1".into(),
        "XADD stream 2-0 n 2".into(),
        "XADD stream 3-0 n 3 extra field".into(),
        "XGROUP CREATE stream g 0".into(),
        "XREADGROUP GROUP g c1 COUNT 2 STREAMS stream >".into(),
        format!("PEXPIREAT list {far}"),
        "SELECT 1".into(),
        "SET db1key one".into(),
        "SADD db1set q".into(),
        format!("PEXPIREAT db1set {far}"),
        "SELECT 5".into(),
        "SET db5key 5".into(),
        "SET gone x PX 1".into(),
    ];
    for l in &lines {
        assert!(!matches!(run(&db, &mut c, l), Reply::Error(_)), "{l}");
    }
    tokio::time::sleep(Duration::from_millis(5)).await;
    let snap = shipper.snapshot(&db, 0).await.unwrap().unwrap();
    assert_eq!(snap.offset, db.repl_offset());
    shipper.detach(&db).await.unwrap();

    // Loaded from the snapshot alone (no segments after it).
    let back = Database::new("rdb", Config::default(), 1);
    persist::restore(&o, &back, Some(&*codec)).await.unwrap();
    let keys = |d: &Database| -> Vec<(usize, Vec<u8>)> {
        let eng = d.engine(0);
        let mut v: Vec<_> = eng.dbs.iter().enumerate().flat_map(|(i, db)| db.keys.iter().map(move |(k, _)| (i, k.to_vec()))).collect();
        v.sort();
        v
    };
    let mut want = keys(&db);
    want.retain(|(_, k)| k != b"gone");
    assert_eq!(want.len(), 19);
    assert_eq!(keys(&back), want, "the expired key is not written");
    let mut c2 = Client::new(2);
    let mut reordered = 0;
    for (i, k) in &want {
        let k = String::from_utf8_lossy(k);
        run(&db, &mut c, &format!("SELECT {i}"));
        run(&back, &mut c2, &format!("SELECT {i}"));
        for cmd in ["PEXPIRETIME", "OBJECT ENCODING", "TYPE"] {
            assert_eq!(run(&back, &mut c2, &format!("{cmd} {k}")), run(&db, &mut c, &format!("{cmd} {k}")), "{cmd} {k}");
        }
        let (a, b) = (run(&db, &mut c, &format!("DUMP {k}")), run(&back, &mut c2, &format!("DUMP {k}")));
        assert!(matches!(a, Reply::Bulk(_)), "DUMP {k}");
        if a == b {
            continue;
        }
        // Only hashtables may serialize in another order: same contents.
        assert_eq!(run(&db, &mut c, &format!("OBJECT ENCODING {k}")), Reply::bulk(b"hashtable"), "DUMP {k}");
        reordered += 1;
        let cmd = if run(&db, &mut c, &format!("TYPE {k}")) == Reply::status("set") { "SMEMBERS" } else { "HGETALL" };
        assert_eq!(sorted(run(&back, &mut c2, &format!("{cmd} {k}"))), sorted(run(&db, &mut c, &format!("{cmd} {k}"))), "{k}");
    }
    assert!(reordered <= 3);
    run(&back, &mut c2, "SELECT 0");
    run(&db, &mut c, "SELECT 0");
    let fields = run(&back, &mut c2, "HPEXPIRETIME hhtex FIELDS 3 f1 f2 f3");
    assert_eq!(fields, Reply::Array(vec![Reply::Int(far.parse().unwrap()), Reply::Int(far.parse().unwrap()), Reply::Int(-1)]));
    assert_eq!(run(&back, &mut c2, "XPENDING stream g"), run(&db, &mut c, "XPENDING stream g"));
}
