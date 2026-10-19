//! QuorumStore against three in-process keepers: replication, failures, fencing, recovery and
//! tiering.

mod common;

use bytes::Bytes;
use common::{Cluster, entries, eventually};
use mq_core::log::{Log, LogStore};
use mq_storage::proto::{Reply, Request};
use mq_storage::{FencedError, QuorumStore, TierConfig};
use std::sync::Arc;
use std::time::Duration;

async fn read_all(log: &dyn Log) -> Vec<(u64, Bytes)> {
    let (first, next) = log.bounds();
    let mut out = Vec::new();
    let mut at = first;
    while at < next {
        let b = log.read(at, 37, 1 << 20).await.unwrap();
        assert!(!b.is_empty(), "read stalled at {at}");
        at = b.last().unwrap().0 + 1;
        out.extend(b);
    }
    out
}

#[tokio::test]
async fn append_read_roundtrip() {
    let c = Cluster::start_with(3, true).await;
    let store = QuorumStore::connect(c.config()).await.unwrap();
    let log = store.open("q/%2F/a/1", 0).await.unwrap();
    assert_eq!(log.bounds(), (0, 0));
    assert_eq!(log.append(entries("e", 0..100)).await.unwrap(), 0);
    assert_eq!(log.append(vec![Bytes::from_static(b"x")]).await.unwrap(), 100);
    assert_eq!(log.append(vec![]).await.unwrap(), 101);
    let r = log.read(98, 10, 1 << 20).await.unwrap();
    assert_eq!(r.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [98, 99, 100]);
    assert_eq!(&r[2].1[..], b"x");
    assert_eq!(log.bounds(), (0, 101));
    // max_bytes stops early but always returns at least one entry.
    assert_eq!(log.read(0, 100, 1).await.unwrap().len(), 1);
    assert_eq!(log.read(0, 100, 7).await.unwrap().len(), 3);
    assert!(log.bytes() > 0);
    // Every keeper has the full log.
    for i in 0..3 {
        eventually(5, "replication", async || c.contents(i, "q/%2F/a/1", 0).await.len() == 101).await;
    }
    // A fresh store (new process) sees the same log.
    drop(log);
    let store2 = QuorumStore::connect(c.config()).await.unwrap();
    let log = store2.open("q/%2F/a/1", 0).await.unwrap();
    assert_eq!(log.bounds(), (0, 101));
    let all = read_all(log.as_ref()).await;
    assert_eq!(all.len(), 101);
    assert_eq!(&all[57].1[..], b"e57");
    let s = store2.stats();
    assert_eq!(s.logs, 1);
    assert!(s.bytes_hot > 0);
    log.destroy().await.unwrap();
    assert_eq!(c.state(0, "q/%2F/a/1").await.next, 0);
}

/// Keepers only take writes from brokers that know the shared secret.
#[tokio::test]
async fn keepers_refuse_writers_without_the_secret() {
    let c = Cluster::start(3).await;
    for wrong in [common::secret("not-it"), mq_core::transport::Transport::plain("keeper")] {
        let client = mq_storage::client::KeeperClient::with_transport(c.addrs[0], "", Duration::from_secs(2), wrong.clone());
        let e = client.call(&Request::State { log: "q/x".into() }).await.unwrap_err();
        assert!(e.to_string().contains("secret"), "{e}");
        let cfg = mq_storage::QuorumConfig { transport: wrong, append_timeout: Duration::from_millis(500), ..c.config() };
        let opened = match QuorumStore::connect(cfg).await {
            Ok(store) => store.open("q/%2F/locked/1", 0).await.map(|_| ()),
            Err(e) => Err(e),
        };
        assert!(opened.is_err(), "a writer without the secret can't open a log");
    }
    assert!(matches!(c.client(0).call(&Request::State { log: "q/x".into() }).await, Ok(Reply::State(_))));
}

#[tokio::test]
async fn one_keeper_down_then_repaired() {
    let mut c = Cluster::start(3).await;
    let store = QuorumStore::connect(c.config()).await.unwrap();
    let log = store.open_log("s/one", 0).await.unwrap();
    for b in 0..20 {
        log.append(entries("a", b * 10..b * 10 + 10)).await.unwrap();
    }
    c.kill(2).await;
    // Appends keep succeeding on the remaining quorum.
    for b in 20..60 {
        log.append(entries("a", b * 10..b * 10 + 10)).await.unwrap();
    }
    assert_eq!(log.committed(), 600);
    assert!(log.replicas()[2].acked < 600);
    c.restart(2).await;
    eventually(10, "keeper 2 repaired", async || log.replicas()[2].acked == 600).await;
    let got = c.contents(2, "s/one", 0).await;
    assert_eq!(got.len(), 600);
    assert_eq!(&got[599][..], b"a599");
    // And it takes part in new appends again.
    log.append(entries("b", 0..5)).await.unwrap();
    eventually(5, "keeper 2 current", async || log.replicas()[2].acked == 605).await;
    let s = store.stats();
    assert!(s.append_latency.count >= 61, "{s:?}");
}

#[tokio::test]
async fn repair_beyond_the_retransmit_buffer() {
    let mut c = Cluster::start(3).await;
    let mut cfg = c.config();
    cfg.retain_bytes = 64;
    let store = QuorumStore::connect(cfg).await.unwrap();
    let log = store.open_log("q/big", 0).await.unwrap();
    log.append(entries("x", 0..10)).await.unwrap();
    c.kill(1).await;
    for b in 1..50 {
        log.append(entries("x", b * 10..b * 10 + 10)).await.unwrap();
    }
    c.restart(1).await;
    // Entries no longer buffered are copied from another keeper.
    eventually(10, "keeper 1 repaired", async || log.replicas()[1].acked == 500).await;
    assert_eq!(c.contents(1, "q/big", 0).await, entries("x", 0..500));
}

#[tokio::test]
async fn two_keepers_down_never_confirms() {
    let mut c = Cluster::start(3).await;
    let store = QuorumStore::connect(c.config()).await.unwrap();
    let log = store.open_log("q/two", 0).await.unwrap();
    log.append(entries("a", 0..10)).await.unwrap();
    c.kill(1).await;
    c.kill(2).await;
    let r = tokio::time::timeout(Duration::from_secs(10), log.append(entries("b", 0..10))).await.expect("append must not hang");
    assert!(r.is_err(), "append confirmed without a quorum");
    let r = log.append(entries("c", 0..10)).await;
    assert!(r.is_err());
    assert!(store.stats().append_errors >= 2);
    // Once a keeper is back, appends succeed again (and the earlier batches become durable).
    c.restart(1).await;
    eventually(10, "quorum back", async || log.append(entries("d", 0..1)).await.is_ok()).await;
    assert_eq!(log.committed(), log.bounds().1);
}

#[tokio::test]
async fn stale_writer_is_fenced() {
    let c = Cluster::start(3).await;
    let a = QuorumStore::connect(c.config()).await.unwrap();
    let b = QuorumStore::connect(c.config()).await.unwrap();
    let la = a.open_log("q/fence", 0).await.unwrap();
    la.append(entries("a", 0..10)).await.unwrap();
    let lb = b.open_log("q/fence", 0).await.unwrap();
    assert!(lb.epoch() > la.epoch());
    assert_eq!(lb.bounds(), (0, 10));
    let err = la.append(entries("a", 10..20)).await.unwrap_err();
    assert!(err.downcast_ref::<FencedError>().is_some(), "{err:#}");
    assert!(la.append(entries("a", 10..20)).await.is_err());
    lb.append(entries("b", 0..5)).await.unwrap();
    assert!(la.truncate_prefix(5).await.is_err());
    for i in 0..3 {
        eventually(5, "replication", async || c.contents(i, "q/fence", 0).await.len() == 15).await;
        assert_eq!(&c.contents(i, "q/fence", 0).await[10][..], b"b0");
    }
    // Lease epochs: an older lease can't take the log back.
    let c2 = QuorumStore::connect(c.config()).await.unwrap();
    let l5 = c2.open_log("q/lease", 5).await.unwrap();
    l5.append(entries("x", 0..1)).await.unwrap();
    let d = QuorumStore::connect(c.config()).await.unwrap();
    let err = d.open_log("q/lease", 3).await.err().expect("older lease must be refused");
    assert!(err.downcast_ref::<FencedError>().is_some(), "{err:#}");
    // The same lease reopening (owner restarted) works and fences the old handle.
    let again = d.open_log("q/lease", 5).await.unwrap();
    assert!(again.epoch() > l5.epoch());
    assert!(l5.append(entries("x", 1..2)).await.is_err());
}

#[tokio::test]
async fn writer_crash_mid_append_is_recovered() {
    let mut c = Cluster::start(3).await;
    let key = "q/crash";
    // Writer A (epoch 1) replicated 10 entries everywhere, then crashed after its next batch
    // reached only keeper 0.
    let raw: Vec<_> = (0..3).map(|i| c.client(i)).collect();
    for k in &raw {
        assert!(matches!(k.call(&Request::Fence { log: key.into(), epoch: 1, strict: true }).await.unwrap(), Reply::State(_)));
        let r = Request::Append { log: key.into(), epoch: 1, first: 0, prev_epoch: 0, entry_epoch: 1, seal: false, entries: entries("a", 0..10) };
        assert!(matches!(k.call(&r).await.unwrap(), Reply::Appended { next: 10 }));
    }
    let partial = Request::Append { log: key.into(), epoch: 1, first: 10, prev_epoch: 1, entry_epoch: 1, seal: false, entries: entries("a", 10..12) };
    assert!(matches!(raw[0].call(&partial).await.unwrap(), Reply::Appended { next: 12 }));

    // Writer B opens while keeper 0 is down: the partial tail is invisible, B builds on 10.
    c.kill(0).await;
    let b = QuorumStore::connect(c.config()).await.unwrap();
    let lb = b.open_log(key, 0).await.unwrap();
    assert_eq!(lb.bounds(), (0, 10));
    assert_eq!(lb.append(entries("b", 0..5)).await.unwrap(), 10);
    // Keeper 0 returns with its divergent suffix; it is truncated and repaired.
    c.restart(0).await;
    eventually(10, "keeper 0 repaired", async || lb.replicas()[0].acked == 15).await;
    let want: Vec<Bytes> = entries("a", 0..10).into_iter().chain(entries("b", 0..5)).collect();
    for i in 0..3 {
        assert_eq!(c.contents(i, key, 0).await, want, "keeper {i}");
    }
    drop(lb);

    // A partially replicated tail that *is* visible at open is adopted and copied everywhere.
    let key = "q/crash2";
    for k in &raw {
        k.call(&Request::Fence { log: key.into(), epoch: 1, strict: true }).await.unwrap();
        let r = Request::Append { log: key.into(), epoch: 1, first: 0, prev_epoch: 0, entry_epoch: 1, seal: false, entries: entries("a", 0..4) };
        k.call(&r).await.unwrap();
    }
    let partial = Request::Append { log: key.into(), epoch: 1, first: 4, prev_epoch: 1, entry_epoch: 1, seal: false, entries: entries("a", 4..6) };
    assert!(matches!(raw[2].call(&partial).await.unwrap(), Reply::Appended { next: 6 }));
    let l = b.open_log(key, 0).await.unwrap();
    assert_eq!(l.bounds(), (0, 6));
    let all = read_all(l.as_ref()).await;
    assert_eq!(all.into_iter().map(|(_, e)| e).collect::<Vec<_>>(), entries("a", 0..6));
    for i in 0..3 {
        eventually(5, "copied", async || c.contents(i, key, 0).await.len() == 6).await;
    }
}

#[tokio::test]
async fn truncate_prefix_drops_whole_segments() {
    let c = Cluster::start(3).await;
    let mut cfg = c.config();
    cfg.segment_bytes = 200;
    let store = QuorumStore::connect(cfg).await.unwrap();
    let log = store.open_log("q/trunc", 0).await.unwrap();
    for b in 0..10 {
        log.append(entries("t", b * 10..b * 10 + 10)).await.unwrap();
    }
    let first = log.truncate_prefix(55).await.unwrap();
    assert!(first > 0 && first <= 55, "{first}");
    assert_eq!(log.bounds(), (first, 100));
    assert_eq!(log.read(0, 1, 1 << 20).await.unwrap()[0].0, first);
    eventually(5, "keepers trimmed", async || {
        let mut ok = true;
        for i in 0..3 {
            ok &= c.state(i, "q/trunc").await.trim == first;
        }
        ok
    })
    .await;
    // Everything consumed: the log restarts empty at its end.
    assert_eq!(log.truncate_prefix(100).await.unwrap(), 100);
    assert_eq!(log.bounds(), (100, 100));
    assert_eq!(log.append(entries("u", 0..1)).await.unwrap(), 100);
    drop(log);
    let log = QuorumStore::connect(c.config()).await.unwrap().open_log("q/trunc", 0).await.unwrap();
    assert_eq!(log.bounds(), (100, 101));
}

#[tokio::test]
async fn tiering_to_object_storage() {
    let c = Cluster::start(3).await;
    let bucket = tempfile::tempdir().unwrap();
    let os: Arc<dyn object_store::ObjectStore> = Arc::new(object_store::local::LocalFileSystem::new_with_prefix(bucket.path()).unwrap());
    let mut cfg = c.config();
    cfg.segment_bytes = 1024;
    cfg.tier = Some(TierConfig { cache_bytes: 1 << 20, ..TierConfig::new(os.clone(), "tier") });
    let store = QuorumStore::connect(cfg.clone()).await.unwrap();
    let key = "s/%2F/events/1";
    let log = store.open_log(key, 0).await.unwrap();
    for b in 0..40 {
        log.append(entries("m", b * 25..b * 25 + 25)).await.unwrap();
    }
    log.roll().await.unwrap();
    eventually(10, "segments tiered", async || {
        let (hot, cold) = log.tier_bytes();
        cold > 0 && hot < 1500
    })
    .await;
    // Keepers dropped their copies.
    eventually(5, "keepers dropped tiered data", async || c.state(0, key).await.hot_first > 0).await;
    let seg_files = || walk(bucket.path());
    let uploaded = seg_files();
    assert!(uploaded.len() > 5, "{uploaded:?}");
    assert!(uploaded.iter().all(|p| p.to_string_lossy().contains("s=2F=252F=2Fevents=2F1")), "{uploaded:?}");
    // Reads below the hot tail come from object storage.
    let all = read_all(log.as_ref()).await;
    assert_eq!(all.len(), 1000);
    assert_eq!(all.iter().map(|(_, e)| e.clone()).collect::<Vec<_>>(), entries("m", 0..1000));
    let s = store.stats();
    assert!(s.bytes_cold > 0 && s.cache_bytes > 0, "{s:?}");
    assert_eq!(log.bytes(), s.bytes_hot + s.bytes_cold);
    // A new owner finds the cold segments.
    drop(log);
    let store2 = QuorumStore::connect(cfg.clone()).await.unwrap();
    let log = store2.open_log(key, 0).await.unwrap();
    assert_eq!(log.bounds(), (0, 1000));
    assert_eq!(read_all(log.as_ref()).await.len(), 1000);
    // Truncation deletes cold objects.
    let first = log.truncate_prefix(600).await.unwrap();
    assert!(first > 0 && first <= 600);
    assert!(seg_files().len() < uploaded.len());
    assert_eq!(read_all(log.as_ref()).await.len() as u64, 1000 - first);
    log.destroy().await.unwrap();
    assert!(seg_files().is_empty());
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else if p.extension().is_some_and(|x| x == "seg") {
                out.push(p);
            }
        }
    }
    out
}

/// Keepers live in other AZs, where every byte is billed: compressible payloads cross the
/// network compressed both ways, and come back exactly as written.
#[tokio::test]
async fn replication_traffic_is_compressed() {
    let c = Cluster::start(3).await;
    let body = |i: usize| Bytes::from(format!("{{\"order\":{i},\"customer\":\"c-{}\",\"status\":\"shipped\",\"items\":[{{\"sku\":\"A-1\",\"qty\":2}}]}}", i % 97).repeat(8));
    let mut sent = 0u64;
    let mut wire = 0u64;
    for compress in [true, false] {
        let store = QuorumStore::connect(mq_storage::QuorumConfig { compress, ..c.config() }).await.unwrap();
        let key = format!("q/%2F/zstd-{compress}/1");
        let log = store.open(&key, 0).await.unwrap();
        for batch in 0..20 {
            log.append((0..50).map(|i| body(batch * 50 + i)).collect()).await.unwrap();
        }
        // Read back through a fresh store so the entries come from a keeper, not the cache.
        drop(log);
        let reader = QuorumStore::connect(mq_storage::QuorumConfig { compress, ..c.config() }).await.unwrap();
        let log = reader.open(&key, 0).await.unwrap();
        let all = read_all(log.as_ref()).await;
        assert_eq!(all.len(), 1000);
        assert_eq!(all[123].1, body(123));
        let s = store.stats();
        let r = reader.stats();
        (sent, wire) = s.keepers.iter().fold((0, 0), |(a, b), k| (a + k.sent_bytes, b + k.sent_wire_bytes));
        let (got, got_wire) = r.keepers.iter().fold((0, 0), |(a, b), k| (a + k.received_bytes, b + k.received_wire_bytes));
        if compress {
            assert!(wire * 4 < sent, "appends: {sent} bytes went out as {wire}");
            assert!(got_wire * 4 < got, "reads: {got} bytes came in as {got_wire}");
        } else {
            assert_eq!(wire, sent);
            assert_eq!(got_wire, got);
        }
    }
    assert!(sent > 0 && wire > 0);
}
