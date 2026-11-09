//! S3ExpressStore semantics on object_store's `InMemory` (which supports put-if-absent).

mod common;

use bytes::Bytes;
use common::entries;
use futures::StreamExt;
use mq_core::log::{Log, LogStore};
use mq_storage::{ExpressConfig, FencedError, S3ExpressStore};
use object_store::ObjectStore;
use object_store::memory::InMemory;
use std::sync::Arc;

pub async fn read_all(log: &dyn Log) -> Vec<(u64, Bytes)> {
    let (first, next) = log.bounds();
    let mut out = Vec::new();
    let mut at = first;
    while at < next {
        let b = log.read(at, 13, 1 << 20).await.unwrap();
        if b.is_empty() {
            break;
        }
        at = b.last().unwrap().0 + 1;
        out.extend(b);
    }
    out
}

fn config(os: Arc<dyn ObjectStore>) -> ExpressConfig {
    ExpressConfig { compact_min_batches: 4, segment_bytes: 1 << 20, tail_bytes: 64, ..ExpressConfig::new(os, "express") }
}

#[tokio::test]
async fn express_log_semantics() {
    common::init_tracing();
    let os: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let store = S3ExpressStore::new(config(os.clone()));
    let log = store.open_log("q/%2F/a/1", 0).await.unwrap();
    // The fence marker of a fresh open occupies index 0.
    assert_eq!(log.bounds(), (0, 1));
    let a = log.append(entries("e", 0..10)).await.unwrap();
    assert_eq!(a, 1);
    assert_eq!(log.append(entries("e", 10..20)).await.unwrap(), 11);
    assert_eq!(log.append(vec![]).await.unwrap(), 21);
    let all = read_all(log.as_ref()).await;
    assert_eq!(all.iter().map(|(i, _)| *i).collect::<Vec<_>>(), (1..21).collect::<Vec<_>>());
    assert_eq!(all.into_iter().map(|(_, e)| e).collect::<Vec<_>>(), entries("e", 0..20));
    assert_eq!(log.read(5, 2, 1 << 20).await.unwrap().iter().map(|(i, _)| *i).collect::<Vec<_>>(), [5, 6]);
    assert_eq!(log.read(5, 100, 1).await.unwrap().len(), 1);

    // A second owner takes over: the old one is fenced on its next append, nothing is lost.
    let store2 = S3ExpressStore::new(config(os.clone()));
    let log2 = store2.open_log("q/%2F/a/1", 0).await.unwrap();
    assert!(log2.epoch() > log.epoch());
    assert_eq!(log2.bounds(), (0, 22));
    let err = log.append(entries("stale", 0..1)).await.unwrap_err();
    assert!(err.downcast_ref::<FencedError>().is_some(), "{err:#}");
    assert_eq!(log2.append(entries("n", 0..5)).await.unwrap(), 22);
    for b in 0..6 {
        log2.append(entries("m", b * 3..b * 3 + 3)).await.unwrap();
    }
    let before = read_all(log2.as_ref()).await;
    assert_eq!(before.len(), 20 + 5 + 18);

    // Truncation removes whole objects.
    let first = log2.truncate_prefix(15).await.unwrap();
    assert_eq!(first, 11);
    let before: Vec<_> = before.into_iter().filter(|(i, _)| *i >= first).collect();
    assert_eq!(read_all(log2.as_ref()).await, before);

    // Compaction merges batches into segments without changing what reads see.
    let objects = log2.objects();
    while log2.compact().await.unwrap() {}
    assert!(log2.objects() < objects, "{} -> {}", objects, log2.objects());
    assert_eq!(read_all(log2.as_ref()).await, before);
    let (f, next) = log2.bounds();
    assert_eq!(f, first);
    drop(log2);
    drop(log);
    let store3 = S3ExpressStore::new(config(os.clone()));
    let log3 = store3.open_log("q/%2F/a/1", 0).await.unwrap();
    assert_eq!(log3.bounds(), (first, next + 1));
    let after = read_all(log3.as_ref()).await;
    assert_eq!(after, before);
    assert!(log3.bytes() > 0);
    assert!(store3.stats().bytes_hot > 0);
    // Old lease epochs are refused.
    let l7 = S3ExpressStore::new(config(os.clone())).open_log("q/lease", 7).await.unwrap();
    l7.append(entries("x", 0..1)).await.unwrap();
    let err = S3ExpressStore::new(config(os.clone())).open_log("q/lease", 2).await.err().expect("older lease");
    assert!(err.downcast_ref::<FencedError>().is_some());
    log3.destroy().await.unwrap();
    let left: Vec<_> = futures::TryStreamExt::try_collect::<Vec<_>>(os.list(Some(&"express/q=2F=252F=2Fa=2F1".into()))).await.unwrap();
    assert!(left.is_empty(), "{left:?}");
}

#[tokio::test]
async fn express_on_local_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let os: Arc<dyn ObjectStore> = Arc::new(object_store::local::LocalFileSystem::new_with_prefix(dir.path()).unwrap());
    let store = S3ExpressStore::new(config(os.clone()));
    let log = store.open("s/x", 0).await.unwrap();
    for b in 0..8 {
        log.append(entries("l", b * 4..b * 4 + 4)).await.unwrap();
    }
    let log2 = S3ExpressStore::new(config(os)).open_log("s/x", 0).await.unwrap();
    assert!(log.append(entries("z", 0..1)).await.is_err());
    assert_eq!(read_all(log2.as_ref()).await.into_iter().map(|(_, e)| e).collect::<Vec<_>>(), entries("l", 0..32));
}

fn grouped(os: Arc<dyn ObjectStore>) -> ExpressConfig {
    let group = mq_storage::GroupConfig { linger: std::time::Duration::from_millis(5), flush_after: std::time::Duration::from_secs(3600), ..Default::default() };
    ExpressConfig { group: Some(group), ..config(os) }
}

fn puts(store: &S3ExpressStore) -> u64 {
    store.requests().puts.load(std::sync::atomic::Ordering::Relaxed)
}

/// Appends of many logs share stream objects, so requests follow the broker, not the queues;
/// closed logs land in their own objects and read back from a fresh store.
#[tokio::test]
async fn grouped_appends_share_objects() {
    common::init_tracing();
    const LOGS: usize = 100;
    const APPENDS: usize = 5;
    let mut costs = Vec::new();
    for group in [false, true] {
        let os: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let cfg = if group { grouped(os.clone()) } else { config(os.clone()) };
        let store = S3ExpressStore::new(cfg.clone());
        let mut logs = Vec::new();
        for l in 0..LOGS {
            logs.push(store.open_log(&format!("q/%2F/g{l}/1"), 0).await.unwrap());
        }
        let before = puts(&store);
        let tasks: Vec<_> = logs
            .iter()
            .cloned()
            .enumerate()
            .map(|(l, log)| {
                tokio::spawn(async move {
                    for a in 0..APPENDS {
                        log.append(entries(&format!("l{l}-"), a * 2..a * 2 + 2)).await.unwrap();
                    }
                })
            })
            .collect();
        for t in tasks {
            t.await.unwrap();
        }
        costs.push(puts(&store) - before);
        // Close them all: grouped logs materialize what they staged and mark the close.
        drop(logs);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let fresh = S3ExpressStore::new(cfg);
        for l in [0, LOGS / 2, LOGS - 1] {
            let log = fresh.open_log(&format!("q/%2F/g{l}/1"), 0).await.unwrap();
            let got: Vec<Bytes> = read_all(log.as_ref()).await.into_iter().map(|(_, e)| e).collect();
            assert_eq!(got, entries(&format!("l{l}-"), 0..APPENDS * 2), "log {l}, grouped: {group}");
        }
        // No stream needed draining: every log was closed cleanly.
        assert_eq!(os.list(Some(&object_store::path::Path::from("express/_streams"))).filter(|m| futures::future::ready(m.as_ref().is_ok_and(|m| m.location.as_ref().ends_with("drained")))).count().await, 0);
    }
    let (plain, grouped) = (costs[0], costs[1]);
    assert_eq!(plain, (LOGS * APPENDS) as u64, "one put per append without grouping");
    // A link per log, plus a stream object per round of appends for all of them.
    assert!(grouped <= (LOGS + 4 * APPENDS) as u64, "grouped appends took {grouped} puts, plain {plain}");
}

/// A broker that dies with entries only in its stream loses none of them: the next owner
/// drains the stream. The old writer can't confirm anything for that log afterwards, but its
/// other logs carry on on a new stream.
#[tokio::test]
async fn a_new_owner_drains_the_old_writers_stream() {
    common::init_tracing();
    let os: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let old = S3ExpressStore::new(grouped(os.clone()));
    let moved = old.open_log("q/%2F/moved/1", 0).await.unwrap();
    let kept = old.open_log("q/%2F/kept/1", 0).await.unwrap();
    for b in 0..4 {
        moved.append(entries("m", b * 5..b * 5 + 5)).await.unwrap();
        kept.append(entries("k", b..b + 1)).await.unwrap();
    }
    // Nothing reached the logs' own objects yet: it's all in the stream.
    let own: Vec<_> = os.list(Some(&object_store::path::Path::from("express"))).filter_map(|m| async move { m.ok() }).collect().await;
    assert!(own.iter().filter(|m| m.location.as_ref().contains("moved") && m.size > 40).count() == 0, "{own:?}");

    // The old owner is presumed dead; a new one opens the log (still held open by the old one).
    let new = S3ExpressStore::new(grouped(os.clone()));
    let taken = new.open_log("q/%2F/moved/1", 0).await.unwrap();
    let got: Vec<Bytes> = read_all(taken.as_ref()).await.into_iter().map(|(_, e)| e).collect();
    assert_eq!(got, entries("m", 0..20));
    let at = taken.append(entries("n", 0..1)).await.unwrap();

    // The old writer is fenced for the moved log...
    let err = moved.append(entries("zombie", 0..1)).await.unwrap_err();
    assert!(err.downcast_ref::<FencedError>().is_some(), "{err:#}");
    // ...but its other log carries on, on a fresh stream, without losing anything.
    kept.append(entries("k", 4..6)).await.unwrap();
    drop(kept);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let check = S3ExpressStore::new(grouped(os.clone()));
    let kept = check.open_log("q/%2F/kept/1", 0).await.unwrap();
    let got: Vec<Bytes> = read_all(kept.as_ref()).await.into_iter().map(|(_, e)| e).collect();
    assert_eq!(got, entries("k", 0..6));
    // And the new owner's log has the old entries, then its own.
    let got = read_all(taken.as_ref()).await;
    assert_eq!(got.last().unwrap(), &(at, entries("n", 0..1).remove(0)));
}

/// Truncation in grouped mode waits for nothing (the epoch marker goes out with the next flush)
/// and a reopened log starts where it should.
#[tokio::test]
async fn grouped_truncation_survives_a_reopen() {
    common::init_tracing();
    let os: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let store = S3ExpressStore::new(grouped(os.clone()));
    let log = store.open_log("q/%2F/t/1", 0).await.unwrap();
    let mut idx = Vec::new();
    for b in 0..10 {
        idx.push(log.append(entries("t", b..b + 1)).await.unwrap());
    }
    let before = puts(&store);
    assert_eq!(log.truncate_prefix(idx[6]).await.unwrap(), idx[6]);
    assert_eq!(puts(&store), before, "truncation in grouped mode is free until the next flush");
    assert_eq!(log.bounds().0, idx[6]);
    let got: Vec<Bytes> = read_all(log.as_ref()).await.into_iter().map(|(_, e)| e).collect();
    assert_eq!(got, entries("t", 6..10));
    drop(log);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let log = S3ExpressStore::new(grouped(os.clone())).open_log("q/%2F/t/1", 0).await.unwrap();
    assert_eq!(log.bounds().0, idx[6]);
    let got: Vec<Bytes> = read_all(log.as_ref()).await.into_iter().filter(|(_, e)| !e.is_empty()).map(|(_, e)| e).collect();
    assert_eq!(got, entries("t", 6..10));
}
