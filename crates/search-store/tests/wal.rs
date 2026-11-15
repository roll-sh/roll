//! WAL: group commit, durability marks, shipping, replay, crash recovery and takeover.

mod common;

use common::*;
use search_store::{ShardManifest, test_objects};
use std::collections::BTreeSet;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn appends_survive_a_clean_restart() {
    let objects = test_objects("wal-restart");
    let dir = temp_dir("wal-restart");
    let s = shard("idxAAAA", 0);
    {
        let store = open(&objects, &dir, fast_config("n1")).await;
        let (wal, m, ops) = recover(&store, "c1", &s).await;
        assert!(ops.is_empty());
        assert_eq!(m.generation, 1);
        assert_eq!(m.wal.len(), 1);
        let mut last = 0;
        for seq in 0..100 {
            last = wal.append(seq, &payload("c1", seq)).unwrap();
        }
        store.wal().wait_ack(last).await.unwrap();
        assert!(store.wal().durable_lsn() >= last);
        let st = wal.stats();
        assert_eq!(st.uncommitted_ops, 100);
        assert!(st.oldest_ms.is_some());
        drop(wal);
        store.close().await.unwrap();
        assert!(store.wal().remote_lsn() >= last, "close ships everything");
    }
    let store = open(&objects, &dir, fast_config("n1")).await;
    let (_wal, m, ops) = recover(&store, "c1", &s).await;
    assert_eq!(ops.len(), 100);
    for (i, (seq, p)) in ops.iter().enumerate() {
        assert_eq!(*seq, i as i64);
        assert_eq!(p, &payload("c1", *seq));
    }
    // The earlier run's range is frozen; this run writes a new one.
    assert_eq!(m.generation, 2);
    assert_eq!(m.wal.len(), 2);
    assert!(m.wal[0].to.is_some());
    assert_eq!(m.wal[0].max_seq_no, Some(99));
    assert!(m.wal[1].to.is_none());
    store.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_appenders_get_every_op_exactly_once() {
    let objects = test_objects("wal-concurrent");
    let dir = temp_dir("wal-concurrent");
    let store = open(&objects, &dir, fast_config("n1")).await;
    const TASKS: usize = 8;
    const OPS: i64 = 500;
    let mut shards = Vec::new();
    for t in 0..TASKS {
        let (wal, _, _) = recover(&store, &format!("c{}", t % 3), &shard(&format!("idx{t}"), t as u32)).await;
        shards.push(wal);
    }
    let mut handles = Vec::new();
    for wal in shards {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            let mut lsns = Vec::new();
            for seq in 0..OPS {
                let lsn = wal.append(seq, &payload(wal.cluster(), seq)).unwrap();
                lsns.push(lsn);
                if seq % 50 == 49 {
                    store.wal().wait_ack(lsn).await.unwrap();
                }
            }
            store.wal().wait_ack(*lsns.last().unwrap()).await.unwrap();
            lsns
        }));
    }
    let mut all = BTreeSet::new();
    for h in handles {
        let lsns = h.await.unwrap();
        assert!(lsns.windows(2).all(|w| w[0] < w[1]), "one appender's LSNs increase");
        for l in lsns {
            assert!(all.insert(l), "LSN {l} handed out twice");
        }
    }
    assert_eq!(all.len(), TASKS * OPS as usize);
    let stats = store.wal().stats();
    assert_eq!(stats.appends, (TASKS as u64) * OPS as u64);
    assert!(stats.commits < stats.appends, "group commit batches appends ({} commits)", stats.commits);
    store.close().await.unwrap();
    drop(store);

    // Replay on another node from object storage only.
    let other = open(&objects, &temp_dir("wal-concurrent-b"), fast_config("n2")).await;
    for t in 0..TASKS {
        let cluster = format!("c{}", t % 3);
        let ops = replay_only(&other, &cluster, &shard(&format!("idx{t}"), t as u32)).await;
        let seqs: Vec<i64> = ops.iter().map(|o| o.0).collect();
        assert_eq!(seqs, (0..OPS).collect::<Vec<_>>(), "shard {t}");
        assert!(ops.iter().all(|(s, p)| *p == payload(&cluster, *s)));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_mid_group_commit_keeps_every_acknowledged_op() {
    let base = temp_dir("wal-crash");
    let (obj_dir, cache_dir) = (base.join("objects"), base.join("cache"));
    let objects = local_objects(&obj_dir);
    let s = shard("idxCRASH", 0);
    let mut cfg = fast_config("n1");
    // Nothing ships during the test: recovery has to come from local files.
    cfg.wal.ship_interval = Duration::from_secs(3600);
    let acked;
    let appended;
    {
        let store = open(&objects, &cache_dir, cfg.clone()).await;
        let (wal, _, _) = recover(&store, "c1", &s).await;
        let mut last = 0;
        for seq in 0..200 {
            last = wal.append(seq, &payload("c1", seq)).unwrap();
            if seq % 20 == 19 {
                store.wal().durable(last).await.unwrap();
            }
        }
        store.wal().durable(last).await.unwrap();
        acked = 200;
        // A batch that may or may not make it: the crash happens mid group commit.
        for seq in 200..260 {
            wal.append(seq, &payload("c1", seq)).unwrap();
        }
        appended = 260;
        store.wal().abandon().await;
    }
    let files: Vec<_> = std::fs::read_dir(cache_dir.join("wal")).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "wal")).collect();
    assert_eq!(files.len(), 1);
    let original = std::fs::read(&files[0]).unwrap();
    let durable_len = {
        // Find where the 200th record ends: everything after it is the unacknowledged tail.
        let mut n = 0;
        let mut pos = search_store::wal::record::FILE_HEADER;
        while n < acked {
            let len = u32::from_le_bytes(original[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 8 + len;
            n += 1;
        }
        pos
    };
    // Cut the file at many points in the unacknowledged tail (and add garbage after some).
    let mut cuts: Vec<usize> = (durable_len..=original.len()).step_by(37).collect();
    cuts.push(original.len());
    for (i, cut) in cuts.into_iter().enumerate() {
        let trial = base.join(format!("trial-{i}"));
        copy_dir(&obj_dir, &trial.join("objects"));
        copy_dir(&cache_dir, &trial.join("cache"));
        let mut data = original[..cut].to_vec();
        if i % 3 == 1 {
            data.extend_from_slice(&[0xA5; 13]);
        }
        let name = files[0].file_name().unwrap();
        std::fs::write(trial.join("cache/wal").join(name), &data).unwrap();
        let _ = std::fs::remove_file(trial.join("cache/LOCK"));
        let objects = local_objects(&trial.join("objects"));
        let store = open(&objects, &trial.join("cache"), cfg.clone()).await;
        let (wal, _, ops) = recover(&store, "c1", &s).await;
        let n = ops.len();
        assert!((acked..=appended).contains(&n), "cut {cut}: replayed {n}");
        for (j, (seq, p)) in ops.iter().enumerate() {
            assert_eq!(*seq, j as i64, "cut {cut}: replay is a prefix");
            assert_eq!(p, &payload("c1", *seq));
        }
        // The recovered log takes new appends and survives another restart.
        let lsn = wal.append(n as i64, &payload("c1", n as i64)).unwrap();
        store.wal().wait_ack(lsn).await.unwrap();
        drop(wal);
        store.close().await.unwrap();
        drop(store);
        let store = open(&objects, &trial.join("cache"), cfg.clone()).await;
        let ops = replay_only(&store, "c1", &s).await;
        assert_eq!(ops.len(), n + 1, "cut {cut}");
        store.close().await.unwrap();
        if i > 12 {
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sync_objects_acks_only_once_in_object_storage_and_another_node_takes_over() {
    let objects = test_objects("wal-sync");
    let mut cfg = fast_config("na");
    cfg.wal.sync_objects = true;
    cfg.wal.ship_interval = Duration::from_secs(3600);
    let s = shard("idxSYNC", 2);
    {
        let store = open(&objects, &temp_dir("wal-sync-a"), cfg.clone()).await;
        let (wal, _, _) = recover(&store, "c1", &s).await;
        let mut tasks = Vec::new();
        for seq in 0..64 {
            let lsn = wal.append(seq, &payload("c1", seq)).unwrap();
            let store = store.clone();
            tasks.push(tokio::spawn(async move { store.wal().wait_ack(lsn).await.map(|_| lsn) }));
        }
        for t in tasks {
            let lsn = t.await.unwrap().unwrap();
            assert!(store.wal().remote_lsn() >= lsn);
        }
        assert!(!wal_files(&objects, "na").await.is_empty());
        // The node and its disk are gone.
        store.wal().abandon().await;
    }
    let b = open(&objects, &temp_dir("wal-sync-b"), fast_config("nb")).await;
    let (wal, m, ops) = recover(&b, "c1", &s).await;
    assert_eq!(ops.iter().map(|o| o.0).collect::<Vec<_>>(), (0..64).collect::<Vec<_>>());
    assert_eq!(m.wal.len(), 2);
    assert_eq!(m.wal[0].node, "na");
    assert_eq!(m.wal[0].max_seq_no, Some(63));
    assert!(m.wal[0].to.is_some());
    assert_eq!((m.wal[1].node.as_str(), m.wal[1].to), ("nb", None));
    // New writes on node b, then a flush covering everything drops node a's range.
    let mut lsn = 0;
    for seq in 64..80 {
        lsn = wal.append(seq, &payload("c1", seq)).unwrap();
    }
    b.wal().wait_ack(lsn).await.unwrap();
    let flushed = b.write_manifest("c1", &s, &ShardManifest { max_seq_no: 79, local_checkpoint: 79, ..m.clone() }).await.unwrap();
    assert_eq!(flushed.wal.len(), 1);
    assert_eq!(flushed.wal[0].node, "nb");
    assert_eq!(wal.stats().uncommitted_ops, 0);
    // Partial flush: only ops after the checkpoint replay.
    for seq in 80..90 {
        lsn = wal.append(seq, &payload("c1", seq)).unwrap();
    }
    b.wal().wait_ack(lsn).await.unwrap();
    b.write_manifest("c1", &s, &ShardManifest { max_seq_no: 85, local_checkpoint: 84, ..flushed.clone() }).await.unwrap();
    drop(wal);
    b.close().await.unwrap();
    let c = open(&objects, &temp_dir("wal-sync-c"), fast_config("nc")).await;
    let ops = replay_only(&c, "c1", &s).await;
    assert_eq!(ops.iter().map(|o| o.0).collect::<Vec<_>>(), (85..90).collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shipped_files_are_trimmed_after_flush() {
    let objects = test_objects("wal-trim");
    let mut cfg = fast_config("n1");
    cfg.wal.file_bytes = 4096;
    let store = open(&objects, &temp_dir("wal-trim"), cfg).await;
    let s = shard("idxTRIM", 0);
    let (wal, m, _) = recover(&store, "c1", &s).await;
    let mut lsn = 0;
    for seq in 0..400 {
        lsn = wal.append(seq, &payload("c1", seq)).unwrap();
        if seq % 40 == 0 {
            store.wal().wait_ack(lsn).await.unwrap();
        }
    }
    store.wal().durable_remote(lsn).await.unwrap();
    assert!(store.wal().stats().files_shipped >= 3);
    assert!(store.wal().local_files().len() >= 3, "unflushed files stay local");
    let pins = store.wal().pinning_shards();
    assert_eq!(pins.len(), 1);
    assert_eq!((pins[0].cluster.as_str(), &pins[0].shard, pins[0].stats.uncommitted_ops), ("c1", &s, 400));
    let first = pins[0].first_file;
    assert_eq!(first, store.wal().local_files()[0]);
    // A partial flush moves the pin forward, by whole files.
    let m = store.write_manifest("c1", &s, &ShardManifest { max_seq_no: 199, local_checkpoint: 199, ..m }).await.unwrap();
    let pins = store.wal().pinning_shards();
    assert!(pins[0].first_file > first, "{} > {first}", pins[0].first_file);
    assert_eq!(m.wal[0].from, pins[0].first_file, "the commit point replays from the pinned file");
    assert!((200..400).contains(&pins[0].stats.uncommitted_ops), "{:?}", pins[0].stats);
    let m = store.write_manifest("c1", &s, &ShardManifest { max_seq_no: 399, local_checkpoint: 399, ..m }).await.unwrap();
    assert!(store.wal().pinning_shards().is_empty());
    assert!(m.wal[0].from > 1);
    eventually("local trim", || store.wal().local_files().is_empty()).await;
    assert!(store.wal().stats().files_trimmed >= 3);
    // Only the new range replays (nothing).
    drop(wal);
    store.close().await.unwrap();
    let other = open(&objects, &temp_dir("wal-trim-b"), fast_config("n2")).await;
    assert!(replay_only(&other, "c1", &s).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backpressure_and_large_ops() {
    let objects = test_objects("wal-backpressure");
    let mut cfg = fast_config("n1");
    cfg.wal.max_pending_bytes = 64 << 10;
    cfg.wal.flush_bytes = 16 << 10;
    cfg.wal.file_bytes = 256 << 10;
    let store = open(&objects, &temp_dir("wal-backpressure"), cfg).await;
    let s = shard("idxBP", 0);
    let (wal, _, _) = recover(&store, "c1", &s).await;
    let big = vec![7u8; 200 << 10];
    let w = std::thread::spawn(move || {
        let mut last = 0;
        for seq in 0..300 {
            let p = if seq % 50 == 0 { big.clone() } else { vec![seq as u8; 1000] };
            last = wal.append(seq, &p).unwrap();
        }
        (wal, last)
    });
    let (wal, last) = tokio::task::spawn_blocking(move || w.join().unwrap()).await.unwrap();
    store.wal().wait_ack(last).await.unwrap();
    drop(wal);
    store.close().await.unwrap();
    let other = open(&objects, &temp_dir("wal-backpressure-b"), fast_config("n2")).await;
    let ops = replay_only(&other, "c1", &s).await;
    assert_eq!(ops.len(), 300);
    assert_eq!(ops[50].1.len(), 200 << 10);
    assert_eq!(ops[51].1, vec![51u8; 1000]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_wal_refuses_appends_and_bad_names_are_rejected() {
    let objects = test_objects("wal-closed");
    let store = open(&objects, &temp_dir("wal-closed"), fast_config("n1")).await;
    assert!(store.wal().shard("bad/name", &shard("u", 0)).is_err());
    assert!(store.wal().shard("c", &shard("../u", 0)).is_err());
    let s = shard("idx", 0);
    let (wal, _, _) = recover(&store, "c1", &s).await;
    assert!(store.recover_shard("c1", &s).await.is_err(), "a shard opens once per node");
    let lsn = wal.append(0, b"x").unwrap();
    assert!(store.wal().durable_remote(lsn + 10).await.is_err());
    store.close().await.unwrap();
    assert!(wal.append(1, b"y").is_err());
    assert!(store.wal().durable(lsn).await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cache_dir_is_used_by_one_store_at_a_time() {
    let objects = test_objects("wal-lock");
    let dir = temp_dir("wal-lock");
    let a = open(&objects, &dir, fast_config("n1")).await;
    assert!(search_store::Store::open(objects.clone(), &dir, fast_config("n1")).await.is_err());
    a.close().await.unwrap();
    drop(a);
    let b = open(&objects, &dir, fast_config("n1")).await;
    b.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_wal_does_no_work() {
    let objects = test_objects("wal-idle");
    let store = open(&objects, &temp_dir("wal-idle"), fast_config("n1")).await;
    let (wal, _, _) = recover(&store, "c1", &shard("idx", 0)).await;
    let lsn = wal.append(0, b"x").unwrap();
    store.wal().wait_ack(lsn).await.unwrap();
    // Let the file seal and ship, then nothing should happen any more.
    eventually("ship", || store.wal().remote_lsn() >= lsn).await;
    let before = store.wal().stats();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after = store.wal().stats();
    assert_eq!(before.commits, after.commits);
    assert_eq!(before.files_sealed, after.files_sealed);
    assert_eq!(before.files_shipped, after.files_shipped);
    drop(wal);
    store.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detached_shards_do_not_pin_the_node_wal() {
    let objects = test_objects("wal-detach");
    let mut cfg = fast_config("n1");
    cfg.wal.file_bytes = 2048;
    let store = open(&objects, &temp_dir("wal-detach"), cfg).await;
    let (a, b, c) = (shard("idxA", 0), shard("idxB", 0), shard("idxC", 0));

    // A writes a little, flushes and unloads.
    let (wa, ma, _) = recover(&store, "c1", &a).await;
    let mut lsn = 0;
    for seq in 0..10 {
        lsn = wa.append(seq, &payload("c1", seq)).unwrap();
    }
    store.wal().wait_ack(lsn).await.unwrap();
    store.write_manifest("c1", &a, &ShardManifest { max_seq_no: 9, local_checkpoint: 9, ..ma }).await.unwrap();
    store.detach_shard(wa).await.unwrap();
    let ma = store.read_manifest("c1", &a).await.unwrap().unwrap();
    assert!(ma.wal.is_empty(), "nothing to replay: {:?}", ma.wal);

    // C unloads without flushing: its operations stay replayable from anywhere.
    let (wc, _, _) = recover(&store, "c2", &c).await;
    for seq in 0..5 {
        lsn = wc.append(seq, &payload("c2", seq)).unwrap();
    }
    store.wal().wait_ack(lsn).await.unwrap();
    store.detach_shard(wc).await.unwrap();
    let mc = store.read_manifest("c2", &c).await.unwrap().unwrap();
    assert_eq!(mc.wal.len(), 1);
    assert!(mc.wal[0].to.is_some());

    // B keeps writing many files, then flushes.
    let (wb, mb, _) = recover(&store, "c3", &b).await;
    for seq in 0..300 {
        lsn = wb.append(seq, &payload("c3", seq)).unwrap();
        if seq % 10 == 9 {
            store.wal().wait_ack(lsn).await.unwrap();
        }
    }
    store.write_manifest("c3", &b, &ShardManifest { max_seq_no: 299, local_checkpoint: 299, ..mb }).await.unwrap();
    store.wal().sync_remote().await.unwrap();
    let before = wal_files(&objects, "n1").await.len();
    let gc = store.collect_garbage(Duration::ZERO).await.unwrap();
    let after = wal_files(&objects, "n1").await;
    assert!(before >= 10, "{before}");
    assert!(after.len() < before / 2, "A's retained manifests must not pin B's files: {} of {before} left, {gc:?}", after.len());
    let other = open(&objects, &temp_dir("wal-detach-b"), fast_config("n2")).await;
    assert_eq!(replay_only(&other, "c2", &c).await.iter().map(|o| o.0).collect::<Vec<_>>(), (0..5).collect::<Vec<_>>());
    assert!(replay_only(&other, "c1", &a).await.is_empty());
    assert!(replay_only(&other, "c3", &b).await.is_empty());
    drop(wb);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mass_recovery_after_node_loss_downloads_each_wal_object_once() {
    let objects = test_objects("wal-mass");
    let mut cfg = fast_config("na");
    cfg.wal.file_bytes = 16 << 10;
    const SHARDS: usize = 40;
    let ids: Vec<_> = (0..SHARDS).map(|i| shard(&format!("idx{i}"), 0)).collect();
    {
        let a = open(&objects, &temp_dir("wal-mass-a"), cfg).await;
        let mut ws = Vec::new();
        for s in &ids {
            ws.push(recover(&a, "c1", s).await.0);
        }
        let mut lsn = 0;
        for seq in 0..30 {
            for w in &ws {
                lsn = w.append(seq, &payload("c1", seq)).unwrap();
            }
            a.wal().wait_ack(lsn).await.unwrap();
        }
        a.wal().sync_remote().await.unwrap();
        a.wal().abandon().await;
    }
    let files = wal_files(&objects, "na").await.len() as u64;
    assert!(files >= 4, "{files}");
    let b = open(&objects, &temp_dir("wal-mass-b"), fast_config("nb")).await;
    let tasks: Vec<_> = ids
        .iter()
        .cloned()
        .map(|s| {
            let b = b.clone();
            tokio::spawn(async move { replay_only(&b, "c1", &s).await })
        })
        .collect();
    for t in tasks {
        let ops = t.await.unwrap();
        assert_eq!(ops.iter().map(|o| o.0).collect::<Vec<_>>(), (0..30).collect::<Vec<_>>());
    }
    let st = b.wal().stats();
    assert_eq!(st.replayed_records, 30 * SHARDS as u64);
    assert!(st.replay_fetches < files * 4, "{} fetches of {files} files for {SHARDS} shards", st.replay_fetches);
    assert!(st.replay_cache_hits > 0);
    assert_eq!(st.replay_cache_bytes, 0, "the cache empties when replays finish");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_object_store_outage_caps_local_disk() {
    use search_store::WalFull;
    use std::os::unix::fs::PermissionsExt;
    let obj_dir = temp_dir("wal-full-objects");
    let objects = local_objects(&obj_dir);
    let mut cfg = fast_config("n1");
    cfg.wal.max_local_bytes = 64 << 10;
    cfg.wal.file_bytes = 8 << 10;
    let store = open(&objects, &temp_dir("wal-full"), cfg).await;
    let s = shard("idxFULL", 0);
    let (wal, m, _) = recover(&store, "c1", &s).await;
    // Object storage goes away: uploads fail and are retried.
    let wal_objects = obj_dir.join("search/wal/n1");
    std::fs::create_dir_all(&wal_objects).unwrap();
    std::fs::set_permissions(&wal_objects, std::fs::Permissions::from_mode(0o555)).unwrap();
    let op = vec![3u8; 1000];
    let mut seq = 0;
    let err = loop {
        match wal.append(seq, &op) {
            Ok(lsn) => store.wal().durable(lsn).await.unwrap(),
            Err(e) => break e,
        }
        seq += 1;
        assert!(seq < 10_000, "the limit never kicked in");
    };
    let full = err.downcast_ref::<WalFull>().expect("WalFull");
    assert_eq!(full.limit, 64 << 10);
    let st = store.wal().stats();
    assert!(st.unshipped_bytes >= 64 << 10 && st.unshipped_bytes < (64 << 10) + (16 << 10), "{st:?}");
    assert_eq!(st.full_rejections, 1);
    // Back online: the backlog ships and appends resume. The shard keeps its files pinned (no
    // flush), but local disk stays near the limit because shipped files may go anyway.
    eventually("a failed upload", || store.wal().stats().ship_retries > 0).await;
    std::fs::set_permissions(&wal_objects, std::fs::Permissions::from_mode(0o755)).unwrap();
    eventually("backlog shipped", || store.wal().stats().unshipped_bytes < 16 << 10).await;
    // Appending faster than local-directory "S3" ships can hit the limit again: that is
    // transient, retry.
    let mut rejected = 0;
    while seq < 300 {
        match wal.append(seq, &op) {
            Ok(lsn) => {
                store.wal().durable(lsn).await.unwrap();
                seq += 1;
            }
            Err(e) => {
                assert!(e.downcast_ref::<WalFull>().is_some(), "{e:#}");
                rejected += 1;
                assert!(rejected < 1000);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }
    store.wal().sync_remote().await.unwrap();
    eventually("local trimmed to the limit", || store.wal().stats().local_bytes <= (64 << 10) + (8 << 10)).await;
    drop(wal);
    store.close().await.unwrap();
    let other = open(&objects, &temp_dir("wal-full-b"), fast_config("n2")).await;
    let ops = replay_only(&other, "c1", &s).await;
    assert_eq!(ops.iter().map(|o| o.0).collect::<Vec<_>>(), (0..seq).collect::<Vec<_>>());
    drop(m);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_uploads_do_not_change_a_frozen_history() {
    let objects = test_objects("wal-late");
    let mut cfg = fast_config("na");
    cfg.wal.file_bytes = 4096;
    let s = shard("idxLATE", 0);
    let a = open(&objects, &temp_dir("wal-late-a"), cfg).await;
    let (wal, _, _) = recover(&a, "c1", &s).await;
    let mut lsn = 0;
    for seq in 0..200 {
        lsn = wal.append(seq, &payload("c1", seq)).unwrap();
        if seq % 20 == 0 {
            a.wal().wait_ack(lsn).await.unwrap();
        }
    }
    a.wal().durable_remote(lsn).await.unwrap();
    a.wal().abandon().await;
    // One of a's uploads hasn't landed yet when b takes over.
    let mut files = wal_files(&objects, "na").await;
    files.sort();
    assert!(files.len() >= 4, "{files:?}");
    let (late_key, _) = files[files.len() / 2].clone();
    let late = objects.get(&late_key).await.unwrap().unwrap();
    objects.delete(&late_key).await.unwrap();
    let b = open(&objects, &temp_dir("wal-late-b"), fast_config("nb")).await;
    let (wb, mb, seen) = recover(&b, "c1", &s).await;
    assert!(seen.len() < 200);
    assert_eq!(mb.wal[0].skip.len(), 1, "{:?}", mb.wal[0]);
    // The upload lands; b dies before flushing. Whoever recovers next sees what b saw.
    objects.put(&late_key, late).await.unwrap();
    drop(wb);
    b.wal().abandon().await;
    let c = open(&objects, &temp_dir("wal-late-c"), fast_config("nc")).await;
    let again = replay_only(&c, "c1", &s).await;
    assert_eq!(again, seen);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batched_appends_are_all_or_nothing() {
    let objects = test_objects("wal-batch");
    let store = open(&objects, &temp_dir("wal-batch"), fast_config("n1")).await;
    let s = shard("idxBATCH", 0);
    let (wal, _, _) = recover(&store, "c1", &s).await;
    assert_eq!(wal.append_many::<&[u8]>(&[]).unwrap(), 0);
    let ops: Vec<(i64, Vec<u8>)> = (0..50).map(|i| (i, payload("c1", i))).collect();
    let last = wal.append_many(&ops).unwrap();
    assert_eq!(last, 50);
    // One operation over the record limit: nothing of the batch goes in.
    let too_big = vec![0u8; (256 << 20) + 1];
    let bad: Vec<(i64, &[u8])> = vec![(50, b"fine"), (51, &too_big)];
    assert!(wal.append_many(&bad).is_err());
    assert_eq!(wal.append(50, &payload("c1", 50)).unwrap(), 51);
    store.wal().wait_ack(51).await.unwrap();
    assert_eq!(wal.stats().uncommitted_ops, 51);
    assert_eq!(store.wal().stats().appends, 51);
    drop(wal);
    store.close().await.unwrap();
    let other = open(&objects, &temp_dir("wal-batch-b"), fast_config("n2")).await;
    let replayed = replay_only(&other, "c1", &s).await;
    assert_eq!(replayed, (0..51).map(|i| (i, payload("c1", i))).collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_objects_replay_reads_only_the_shards_blocks() {
    // Incompressible payloads make each shard's block large: replay must read the directory
    // and its own block with ranged reads, not the whole object, and inflate on the blocking
    // pool.
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut noise = |n: usize| -> Vec<u8> {
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    };
    let objects = test_objects("wal-large");
    let mut cfg = fast_config("na");
    cfg.wal.file_bytes = 1 << 20;
    let s = shard("idxLARGE", 0);
    let ops: Vec<(i64, Vec<u8>)> = (0..40).map(|i| (i, noise(50_000 + i as usize))).collect();
    let other: Vec<(i64, Vec<u8>)> = (0..40).map(|i| (i, noise(50_000))).collect();
    {
        let a = open(&objects, &temp_dir("wal-large-a"), cfg).await;
        let (wal, _, _) = recover(&a, "c1", &s).await;
        let (other_wal, _, _) = recover(&a, "c1", &shard("idxOTHER", 0)).await;
        let mut lsn = 0;
        // One commit per chunk, so the 1 MiB file limit splits the ops over several files.
        for (mine, theirs) in ops.chunks(4).zip(other.chunks(4)) {
            wal.append_many(mine).unwrap();
            lsn = other_wal.append_many(theirs).unwrap();
            a.wal().durable(lsn).await.unwrap();
        }
        a.wal().durable_remote(lsn).await.unwrap();
        drop((wal, other_wal));
        a.close().await.unwrap();
    }
    let stored: u64 = wal_files(&objects, "na").await.iter().map(|(_, n)| n).sum();
    let b = open(&objects, &temp_dir("wal-large-b"), fast_config("nb")).await;
    assert_eq!(replay_only(&b, "c1", &s).await, ops);
    let st = b.wal().stats();
    assert!(st.replay_fetches >= 2 && st.replay_block_fetches >= 2, "{st:?}");
    assert!(st.replay_fetched_bytes < stored * 2 / 3, "read {} of {stored} bytes", st.replay_fetched_bytes);
}

fn wal_seq(key: &str) -> u64 {
    search_store::keys::parse_wal_key(key).unwrap().1
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crashed_run_does_not_pin_the_nodes_later_wal() {
    let objects = test_objects("wal-runs");
    let mut cfg = fast_config("n1");
    cfg.wal.file_bytes = 512;
    let cache = temp_dir("wal-runs");
    let (x, y) = (shard("idxX", 0), shard("idxY", 0));
    {
        // An idle tenant's shard is attached when the node crashes: its range stays open.
        let n1 = open(&objects, &cache, cfg.clone()).await;
        let (wx, _, _) = recover(&n1, "idle", &x).await;
        let lsn = wx.append(0, &payload("idle", 0)).unwrap();
        n1.wal().durable_remote(lsn).await.unwrap();
        n1.wal().abandon().await;
        drop(wx);
    }
    let run1: Vec<u64> = wal_files(&objects, "n1").await.iter().map(|(k, _)| wal_seq(k)).collect();
    assert!(!run1.is_empty());
    // The node restarts under the same id and writes a busy tenant, flushed and unloaded.
    let n1 = open(&objects, &cache, cfg.clone()).await;
    let (wy, my, _) = recover(&n1, "busy", &y).await;
    for seq in 0..200 {
        let lsn = wy.append(seq, &payload("busy", seq)).unwrap();
        if seq % 10 == 9 {
            n1.wal().wait_ack(lsn).await.unwrap();
        }
    }
    n1.write_manifest("busy", &y, &ShardManifest { max_seq_no: 199, local_checkpoint: 199, ..my }).await.unwrap();
    n1.detach_shard(wy).await.unwrap();
    n1.wal().sync_remote().await.unwrap();
    let before = wal_files(&objects, "n1").await;
    assert!(before.len() >= run1.len() + 10, "{} files", before.len());
    let gc = n1.collect_garbage(Duration::ZERO).await.unwrap();
    // Only the crashed run's files stay: the idle shard's open range ends with its run.
    let after: Vec<u64> = wal_files(&objects, "n1").await.iter().map(|(k, _)| wal_seq(k)).collect();
    assert_eq!(after, run1, "{gc:?}");
    let markers: Vec<String> = objects.list("search/wal/n1/").await.unwrap().into_iter().map(|(k, _)| k).filter(|k| k.ends_with(".run")).collect();
    let run2_first = run1.last().unwrap() + 1;
    assert_eq!(markers, vec![search_store::keys::run_key("n1", run2_first)], "the first run's marker ends no open range: {gc:?}");
    assert_eq!(gc.run_markers_deleted, 1);
    // The idle shard recovers elsewhere, reading only its own run's files.
    let n2 = open(&objects, &temp_dir("wal-runs-b"), fast_config("n2")).await;
    let mut rec = n2.recover_shard("idle", &x).await.unwrap();
    let mut ops = Vec::new();
    let st = n2
        .replay(&mut rec, &mut |r: &search_store::WalRecord<'_>| {
            ops.push((r.seq_no, r.payload.to_vec()));
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(ops, vec![(0, payload("idle", 0))]);
    assert!(st.files <= run1.len() as u64, "{st:?}");
    assert_eq!(rec.sources()[0].to, Some(run2_first - 1));
    // And this node, restarted again, still replays it from its first run.
    drop(n1);
    let n1 = open(&objects, &temp_dir("wal-runs-c"), cfg).await;
    assert_eq!(replay_only(&n1, "idle", &x).await, vec![(0, payload("idle", 0))]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_wal_object_in_a_frozen_range_fails_replay() {
    let objects = test_objects("wal-lost");
    let mut cfg = fast_config("na");
    cfg.wal.file_bytes = 512;
    let s = shard("idxLOST", 0);
    let a = open(&objects, &temp_dir("wal-lost-a"), cfg).await;
    let (wal, _, _) = recover(&a, "c1", &s).await;
    for seq in 0..40 {
        let lsn = wal.append(seq, &payload("c1", seq)).unwrap();
        a.wal().wait_ack(lsn).await.unwrap();
    }
    a.wal().sync_remote().await.unwrap();
    a.wal().abandon().await;
    drop(wal);
    let files = wal_files(&objects, "na").await;
    assert!(files.len() >= 4, "{files:?}");
    // b takes over: the crashed node's range is frozen, and known to be complete.
    let b = open(&objects, &temp_dir("wal-lost-b"), fast_config("nb")).await;
    let (wb, mb, seen) = recover(&b, "c1", &s).await;
    assert_eq!(seen.len(), 40);
    let frozen = mb.wal.iter().find(|w| w.node == "na").unwrap();
    assert!(frozen.exact && frozen.skip.is_empty(), "{frozen:?}");
    // One of its objects is lost (deleted too early, by hand, …).
    let (lost, _) = &files[files.len() / 2];
    objects.delete(lost).await.unwrap();
    let c = open(&objects, &temp_dir("wal-lost-c"), fast_config("nc")).await;
    let mut rec = c.recover_shard("c1", &s).await.unwrap();
    let err = c.replay(&mut rec, &mut |_: &search_store::WalRecord<'_>| Ok(())).await.unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains(&format!("WAL files {} of node [na] are missing", wal_seq(lost))) && msg.contains("acknowledged operations would be lost"), "{msg}");
    drop(wb);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_start_past_every_earlier_run_even_after_their_files_are_collected() {
    let objects = test_objects("wal-run-order");
    let mut cfg = fast_config("n1");
    cfg.wal.file_bytes = 512;
    let cache = temp_dir("wal-run-order");
    let x = shard("idxO", 0);
    let append = |w: &search_store::WalShard, range: std::ops::Range<i64>| {
        let mut lsn = 0;
        for seq in range {
            lsn = w.append(seq, &payload("c1", seq)).unwrap();
        }
        lsn
    };
    {
        // Run 1 leaves acknowledged operations behind: its files stay.
        let n1 = open(&objects, &cache, cfg.clone()).await;
        let (w, _, _) = recover(&n1, "c1", &x).await;
        for chunk in 0..5 {
            let lsn = append(&w, chunk * 10..chunk * 10 + 10);
            n1.wal().wait_ack(lsn).await.unwrap();
        }
        drop(w);
        n1.close().await.unwrap();
    }
    {
        // Run 2 flushes everything and unloads: garbage collection deletes every WAL file.
        let n1 = open(&objects, &cache, cfg.clone()).await;
        let (w, m, ops) = recover(&n1, "c1", &x).await;
        assert_eq!(ops.len(), 50);
        let lsn = append(&w, 50..60);
        n1.wal().wait_ack(lsn).await.unwrap();
        n1.write_manifest("c1", &x, &ShardManifest { max_seq_no: 59, local_checkpoint: 59, ..m }).await.unwrap();
        n1.detach_shard(w).await.unwrap();
        n1.wal().sync_remote().await.unwrap();
        n1.collect_garbage(Duration::ZERO).await.unwrap();
        assert!(wal_files(&objects, "n1").await.is_empty());
        n1.close().await.unwrap();
    }
    let markers = |objects: search_store::Objects| async move {
        let mut v: Vec<u64> = objects.list("search/wal/n1/").await.unwrap().iter().filter_map(|(k, _)| search_store::keys::parse_run_key(k).map(|r| r.1)).collect();
        v.sort();
        v
    };
    let before = markers(objects.clone()).await;
    // Run 3 writes many files and crashes with them unflushed. Its numbering continues past
    // run 2's marker, so the marker can't end run 3's own range.
    let n1 = open(&objects, &cache, cfg.clone()).await;
    let after = markers(objects.clone()).await;
    assert!(after.last() > before.last(), "{before:?} then {after:?}");
    let (w, m, _) = recover(&n1, "c1", &x).await;
    assert_eq!(m.local_checkpoint, 59);
    for chunk in 6..30 {
        let lsn = append(&w, chunk * 10..chunk * 10 + 10);
        n1.wal().wait_ack(lsn).await.unwrap();
    }
    n1.wal().sync_remote().await.unwrap();
    n1.collect_garbage(Duration::ZERO).await.unwrap();
    n1.wal().abandon().await;
    drop(w);
    let n2 = open(&objects, &temp_dir("wal-run-order-b"), fast_config("n2")).await;
    assert_eq!(replay_only(&n2, "c1", &x).await.iter().map(|o| o.0).collect::<Vec<_>>(), (60..300).collect::<Vec<_>>());
}
