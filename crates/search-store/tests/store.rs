//! Segments, the cache, manifests, cluster state, branching, deletion and garbage collection.

mod common;

use bytes::Bytes;
use common::*;
use search_store::{ClusterState, SegmentId, SegmentRef, ShardManifest, Store, test_objects};
use std::time::Duration;

fn segment_bytes(id: &SegmentId, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut x = u64::from_le_bytes(id.0[..8].try_into().unwrap()) | 1;
    while v.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(len);
    v
}

async fn make_segment(store: &Store, len: usize) -> (SegmentId, Vec<u8>) {
    let id = SegmentId::random();
    let data = segment_bytes(&id, len);
    let path = store.segment_path(&id);
    std::fs::write(&path, &data).unwrap();
    let key = store.upload_segment(&path).await.unwrap();
    assert_eq!(key, id);
    assert!(!path.exists(), "the store took the file");
    (id, data)
}

fn seg_ref(id: SegmentId, bytes: usize) -> SegmentRef {
    SegmentRef { id, bytes: bytes as u64, max_doc: 10, del_count: 0, live: None }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn segments_upload_open_and_fetch_once() {
    let objects = test_objects("seg-fetch");
    let mut cfg = fast_config("n1");
    cfg.single_put_bytes = 64 << 10;
    let a = open(&objects, &temp_dir("seg-a"), cfg.clone()).await;
    let (small, small_data) = make_segment(&a, 10_000).await;
    // Over two download parts, ending mid-part: fetched as parallel ranged reads.
    const BIG: usize = (2 * search_store::cache::FETCH_PART as usize) + 12_345;
    let (big, big_data) = make_segment(&a, BIG).await;
    assert!(a.is_cached(&small) && a.is_cached(&big));
    assert_eq!(&*a.open_segment(&small).await.unwrap(), &small_data[..]);
    let st = a.stats();
    assert_eq!(st.cache.hits, 1);
    assert_eq!(st.segments_uploaded, 2);
    assert_eq!(st.segment_bytes_uploaded, (10_000 + BIG) as u64);
    assert_eq!(objects.get(&big.key()).await.unwrap().unwrap().len(), BIG);

    // Another node: concurrent opens of a missing segment share one download.
    let b = open(&objects, &temp_dir("seg-b"), cfg.clone()).await;
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let b = b.clone();
        tasks.push(tokio::spawn(async move { b.open_segment(&big).await.unwrap() }));
    }
    let mut maps = Vec::new();
    for t in tasks {
        let d = t.await.unwrap();
        assert_eq!(&*d, &big_data[..]);
        maps.push(d);
    }
    let st = b.stats().cache;
    assert_eq!(st.misses, 1, "one download for 16 concurrent opens");
    assert_eq!(st.fetched_bytes, BIG as u64);
    assert_eq!(st.mapped_files, 1);
    let bytes = maps[0].to_bytes();
    drop(maps);
    assert_eq!(b.stats().cache.mapped_files, 1, "a Bytes view keeps the map alive");
    assert_eq!(bytes.len(), BIG);
    drop(bytes);
    assert_eq!(b.stats().cache.mapped_files, 0);
    let all = b.open_segments(&[small, big]).await.unwrap();
    assert_eq!(&*all[0], &small_data[..]);
    // A missing object is an error, not a hang, and isn't cached.
    let missing = SegmentId::random();
    assert!(b.open_segment(&missing).await.is_err());
    assert!(!b.is_cached(&missing));
    // Bad file names are refused.
    let bad = b.segment_path(&missing).with_file_name("nope.seg");
    std::fs::write(&bad, b"x").unwrap();
    assert!(b.upload_segment(&bad).await.is_err());
    drop(all);
    b.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_segments_are_readable_while_downloading() {
    let objects = test_objects("seg-progressive");
    let a = open(&objects, &temp_dir("seg-prog-a"), fast_config("n1")).await;
    const BIG: usize = (3 * search_store::cache::FETCH_PART as usize) + 4321;
    let (big, data) = make_segment(&a, BIG).await;
    let (small, small_data) = make_segment(&a, 5000).await;
    // Cached: complete at once.
    let f = a.fetch_segment(&big).await.unwrap();
    assert!(f.data().is_some() && f.is_ready(0..BIG as u64));
    assert_eq!(&f.read(10..20).await.unwrap()[..], &data[10..20]);

    let b = open(&objects, &temp_dir("seg-prog-b"), fast_config("n2")).await;
    let f = b.fetch_segment(&big).await.unwrap();
    assert_eq!(f.len(), BIG as u64);
    // The whole size is reserved in the budget from the first response on.
    let st = b.stats().cache;
    assert_eq!(st.downloading + st.used, BIG as u64, "{st:?}");
    // The whole file as one map right away; ranges read once they are ready.
    // SAFETY: only ranges `wait_ready` resolved for are read.
    let partial = unsafe { f.map_partial() }.unwrap();
    assert_eq!(partial.len(), BIG);
    f.wait_ready(&[0..24, BIG as u64 - 100..BIG as u64]).await.unwrap();
    assert_eq!((&partial[..24], &partial[BIG - 100..]), (&data[..24], &data[BIG - 100..]));
    // The first round trip brought the end of the segment (footer and table of contents).
    let end = BIG as u64;
    assert!(f.is_ready(end - 24..end));
    assert_eq!(&f.read(end - 24..end).await.unwrap()[..], &data[BIG - 24..]);
    // Any range, across part boundaries, as soon as its parts are in.
    let part = search_store::cache::FETCH_PART;
    for r in [0..1, 4000..4400, part - 3..part + 3, 2 * part..2 * part + 100_000, 1..end] {
        let got = f.read(r.clone()).await.unwrap();
        assert_eq!(&got[..], &data[r.start as usize..r.end as usize], "{r:?}");
    }
    f.prioritize(&[0..end, end + 5..end + 10]);
    assert!(f.read(end - 1..end + 1).await.is_err(), "past the end");
    assert!(f.read(5..5).await.unwrap().is_empty());
    // Joins concurrent opens: one download.
    let opened = b.open_segment(&big).await.unwrap();
    let done = f.complete().await.unwrap();
    assert_eq!(&*done, &data[..]);
    assert_eq!(&*opened, &data[..]);
    assert_eq!(f.downloaded(), BIG as u64);
    assert!(f.data().is_some() && b.is_cached(&big));
    let st = b.stats().cache;
    assert_eq!((st.misses, st.fetched_bytes, st.downloading, st.used), (1, BIG as u64, 0, BIG as u64));
    // The early map became the cache's: one map, counted as in use, never evicted while held.
    assert_eq!(partial.as_slice().as_ptr(), done.as_slice().as_ptr());
    assert_eq!(&*partial, &data[..]);
    drop((done, opened));
    drop(f);
    assert_eq!((st.mapped_files, st.mapped_bytes), (1, BIG as u64));
    b.set_cache_budget(0);
    assert!(b.is_cached(&big), "mapped through the early map");
    drop(partial);
    b.set_cache_budget(0);
    assert!(!b.is_cached(&big));
    b.set_cache_budget(search_store::StoreConfig::default().cache_bytes);
    // A small segment is one request; a missing one is an error.
    let s = b.fetch_segment(&small).await.unwrap();
    assert_eq!(&s.complete().await.unwrap()[..], &small_data[..]);
    drop(s);
    assert!(b.fetch_segment(&SegmentId::random()).await.is_err());
    // A download makes room for itself before its parts arrive: with a budget for the big
    // segment alone, starting it evicts the small one at once.
    b.set_cache_budget(BIG as u64 + 1000);
    assert!(!b.is_cached(&big) && b.is_cached(&small));
    let f = b.fetch_segment(&big).await.unwrap();
    assert!(!b.is_cached(&small), "evicted for the download's reservation");
    let st = b.stats().cache;
    assert!(st.used + st.downloading <= BIG as u64 + 1000, "{st:?}");
    f.complete().await.unwrap();
    drop(f);
    assert_eq!(b.stats().cache.downloading, 0);
    b.close().await.unwrap();
    a.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn damaged_downloads_are_refused_by_the_verifier() {
    let objects = test_objects("seg-verify");
    let a = open(&objects, &temp_dir("seg-verify-a"), fast_config("n1")).await;
    const BIG: usize = (2 * search_store::cache::FETCH_PART as usize) + 999;
    let (big, data) = make_segment(&a, BIG).await;
    let (small, small_data) = make_segment(&a, 3000).await;
    // Stands in for the codec's whole-file checksum: test segments are a function of their id.
    let mut cfg = fast_config("n2");
    cfg.verify = Some(search_store::Verifier::new(|d| {
        let id = SegmentId::parse(d.name().trim_end_matches(".seg")).ok_or_else(|| anyhow::anyhow!("not a segment: {}", d.name()))?;
        anyhow::ensure!(d[..] == segment_bytes(&id, d.len())[..], "checksum mismatch");
        Ok(())
    }));
    let b = open(&objects, &temp_dir("seg-verify-b"), cfg).await;

    // Damage one byte in the middle part of the big object and the end of the small one.
    let mut bad = data.clone();
    bad[search_store::cache::FETCH_PART as usize + 7] ^= 1;
    objects.put(&big.key(), Bytes::from(bad)).await.unwrap();
    let mut bad_small = small_data.clone();
    *bad_small.last_mut().unwrap() ^= 0x80;
    objects.put(&small.key(), Bytes::from(bad_small)).await.unwrap();

    // A progressive reader gets its ranges, then the whole download fails at the check.
    let f = b.fetch_segment(&big).await.unwrap();
    assert_eq!(&f.read(0..16).await.unwrap()[..], &data[..16]);
    let err = f.complete().await.unwrap_err();
    assert!(format!("{err:#}").contains("damaged") && format!("{err:#}").contains("checksum mismatch"), "{err:#}");
    assert!(f.data().is_none());
    assert!(f.read(1..2).await.is_err(), "a failed download fails later reads");
    let err = b.open_segment(&small).await.unwrap_err();
    assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
    assert!(!b.is_cached(&big) && !b.is_cached(&small), "nothing damaged is cached");
    let st = b.stats().cache;
    assert_eq!((st.verified, st.verify_failures, st.files, st.used, st.downloading), (0, 2, 0, 0, 0));

    // Repaired: the next open downloads again and the check passes.
    objects.put(&big.key(), Bytes::from(data.clone())).await.unwrap();
    objects.put(&small.key(), Bytes::from(small_data.clone())).await.unwrap();
    assert_eq!(&*b.open_segment(&big).await.unwrap(), &data[..]);
    assert_eq!(&*b.open_segment(&small).await.unwrap(), &small_data[..]);
    let st = b.stats().cache;
    assert_eq!((st.verified, st.verify_failures, st.misses), (2, 2, 4));
    // Hits and the uploader's own files aren't checked again.
    b.open_segment(&big).await.unwrap();
    assert_eq!(b.stats().cache.verified, 2);
    b.close().await.unwrap();
    a.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_survives_restart() {
    let objects = test_objects("seg-restart");
    let dir = temp_dir("seg-restart");
    let a = open(&objects, &dir, fast_config("n1")).await;
    let (id, data) = make_segment(&a, 50_000).await;
    a.close().await.unwrap();
    drop(a);
    let a = open(&objects, &dir, fast_config("n1")).await;
    assert!(a.is_cached(&id));
    assert_eq!(&*a.open_segment(&id).await.unwrap(), &data[..]);
    assert_eq!(a.stats().cache.misses, 0);
    assert_eq!(a.stats().cache.used, 50_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_evicts_lru_under_budget_but_never_mapped_files() {
    let objects = test_objects("seg-evict");
    let mut cfg = fast_config("n1");
    cfg.cache_bytes = 3 << 20;
    let writer = open(&objects, &temp_dir("seg-evict-w"), fast_config("w")).await;
    let mut segs = Vec::new();
    for _ in 0..10 {
        segs.push(make_segment(&writer, 1 << 20).await);
    }
    let reader_dir = temp_dir("seg-evict-r");
    let reader = open(&objects, &reader_dir, cfg).await;
    // Hold the first two open: they must never be evicted.
    let held: Vec<_> = futures::future::join_all(segs[..2].iter().map(|(id, _)| reader.open_segment(id))).await.into_iter().map(Result::unwrap).collect();
    for (id, data) in &segs[2..] {
        let d = reader.open_segment(id).await.unwrap();
        assert_eq!(&*d, &data[..]);
    }
    let st = reader.stats().cache;
    assert!(st.used <= 3 << 20, "used {} over budget", st.used);
    assert!(st.evictions >= 7);
    assert!(reader.is_cached(&segs[0].0) && reader.is_cached(&segs[1].0));
    assert_eq!(&*held[0], &segs[0].1[..]);
    assert!(reader.is_cached(&segs[9].0), "the newest stays");
    assert!(!reader.is_cached(&segs[2].0), "the least recently used goes");
    let seg_files = std::fs::read_dir(reader_dir.join("seg")).unwrap().count() as u64;
    assert_eq!(seg_files, st.files, "evicted files are gone from disk");
    assert_eq!(std::fs::read_dir(reader_dir.join("tmp")).unwrap().count(), 0, "nothing left set aside");
    // Everything mapped and over budget: reads still work, and the overrun is counted.
    let all: Vec<_> = futures::future::join_all(segs.iter().map(|(id, _)| reader.open_segment(id))).await.into_iter().map(Result::unwrap).collect();
    assert!(reader.stats().cache.over_budget > 0);
    drop(all);
    drop(held);
    reader.set_cache_budget(1 << 20);
    assert!(reader.stats().cache.used <= 1 << 20);
    // Shrinking the budget again keeps working on re-open.
    assert_eq!(&*reader.open_segment(&segs[5].0).await.unwrap(), &segs[5].1[..]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manifests_are_generational_with_retention_and_fallback() {
    let objects = test_objects("manifests");
    let mut cfg = fast_config("n1");
    cfg.manifest_retain = 3;
    let store = open(&objects, &temp_dir("manifests"), cfg).await;
    let s = shard("idxM", 1);
    assert!(store.read_manifest("c1", &s).await.unwrap().is_none());
    for i in 0..10i64 {
        let m = ShardManifest { max_seq_no: i, local_checkpoint: i, mapping_version: i as u64, ..Default::default() };
        let w = store.write_manifest("c1", &s, &m).await.unwrap();
        assert_eq!(w.generation, i as u64 + 1);
    }
    let latest = store.read_manifest("c1", &s).await.unwrap().unwrap();
    assert_eq!((latest.generation, latest.max_seq_no), (10, 9));
    let all = store.read_manifests("c1", &s).await.unwrap();
    assert_eq!(all.iter().map(|m| m.generation).collect::<Vec<_>>(), vec![8, 9, 10]);
    assert_eq!(store.stats().manifests_deleted, 7);
    // A damaged newer generation is an error, never silently the previous one.
    let damaged = search_store::keys::manifest_key("c1", &s, search_store::Version::new(0, 11, 0));
    objects.put(&damaged, Bytes::from_static(b"{garbage")).await.unwrap();
    for err in [store.read_manifest("c1", &s).await.unwrap_err(), store.recover_shard("c1", &s).await.unwrap_err(), store.read_manifests("c1", &s).await.unwrap_err()] {
        assert!(format!("{err:#}").contains(&format!("shard manifest {damaged} is damaged")), "{err:#}");
    }
    assert!(store.write_manifest("c1", &s, &ShardManifest { max_seq_no: 1, local_checkpoint: 2, ..Default::default() }).await.is_err());
    // A fresh store (another node) continues the numbering.
    let other = open(&objects, &temp_dir("manifests-b"), fast_config("n2")).await;
    let w = other.write_manifest("c1", &s, &ShardManifest::default()).await.unwrap();
    assert_eq!(w.generation, 12);
    assert_eq!(other.list_shards("c1").await.unwrap(), vec![s.clone()]);
    objects.delete(&damaged).await.unwrap();
    assert_eq!(store.read_manifest("c1", &s).await.unwrap().unwrap().generation, 12);
    other.delete_shard("c1", &s).await.unwrap();
    assert!(other.read_manifest("c1", &s).await.unwrap().is_none());
}

/// Makes an object unreadable (local object storage): an I/O error, not a missing object.
#[cfg(unix)]
struct Unreadable(std::path::PathBuf);

#[cfg(unix)]
impl Unreadable {
    /// `None` when permissions don't stop this process (root).
    fn new(path: std::path::PathBuf) -> Option<Self> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let u = Unreadable(path);
        std::fs::read(&u.0).is_err().then_some(u)
    }
}

#[cfg(unix)]
impl Drop for Unreadable {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o644));
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreadable_latest_generations_are_errors_not_older_ones() {
    let obj_dir = temp_dir("unreadable-objects");
    let objects = local_objects(&obj_dir);
    let store = open(&objects, &temp_dir("unreadable"), fast_config("n1")).await;
    let s = shard("idxU", 0);
    for i in 0..3i64 {
        store.write_manifest("c1", &s, &ShardManifest { max_seq_no: i * 5 + 4, local_checkpoint: i * 5 + 4, ..Default::default() }).await.unwrap();
    }
    store.write_cluster("c1", &ClusterState::new("{\"v\":1}")).await.unwrap();
    store.write_cluster("c1", &ClusterState::new("{\"v\":2}")).await.unwrap();
    let newest = |prefix: String| {
        let mut keys: Vec<_> = std::fs::read_dir(obj_dir.join(prefix)).unwrap().map(|e| e.unwrap().path()).collect();
        keys.sort();
        keys.pop().unwrap()
    };
    let manifest = newest(format!("search/c/c1/i/{}/{}", s.index_uuid, s.shard));
    let state = newest("search/c/c1/state".into());
    {
        let (Some(_m), Some(_s)) = (Unreadable::new(manifest), Unreadable::new(state)) else {
            eprintln!("skipped: file permissions don't apply to this user");
            return;
        };
        for err in [store.read_manifest("c1", &s).await.unwrap_err(), store.recover_shard("c1", &s).await.unwrap_err()] {
            assert!(format!("{err:#}").contains("reading shard manifest"), "{err:#}");
        }
        assert!(store.read_latest_manifests("c1").await.is_err());
        assert!(store.recover_shards("c1").await.is_err());
        let err = store.read_cluster("c1").await.unwrap_err();
        assert!(format!("{err:#}").contains("reading cluster state"), "{err:#}");
        // A branch can't start from the older state either.
        assert!(store.branch("c1", "c1-copy", None).await.is_err());
        assert!(!store.cluster_exists("c1-copy").await.unwrap());
    }
    // Readable again: the newest generations, as written.
    let m = store.read_manifest("c1", &s).await.unwrap().unwrap();
    assert_eq!((m.generation, m.local_checkpoint), (3, 14));
    assert_eq!(&store.read_cluster("c1").await.unwrap().unwrap().json[..], b"{\"v\":2}");
    // A damaged state is an error too.
    objects.put(&search_store::keys::state_key("c1", search_store::Version::new(0, 9, 0)), Bytes::from_static(b"not zstd")).await.unwrap();
    let err = store.read_cluster("c1").await.unwrap_err();
    assert!(format!("{err:#}").contains("is damaged"), "{err:#}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_point_in_time_branch_restores_a_deleted_index() {
    let objects = test_objects("pit-deleted");
    let mut cfg = fast_config("n1");
    cfg.manifest_retain_ms = 3_600_000;
    let store = open(&objects, &temp_dir("pit-deleted"), cfg).await;
    let a = shard("idxA", 0);
    store.write_cluster("prod", &ClusterState::new("{\"indices\":[\"A\"]}")).await.unwrap();
    let (wal, m, _) = recover(&store, "prod", &a).await;
    let lsn = wal.append(0, b"doc").unwrap();
    store.wal().wait_ack(lsn).await.unwrap();
    let (seg, data) = make_segment(&store, 3000).await;
    store.write_manifest("prod", &a, &ShardManifest { segments: vec![seg_ref(seg, 3000)], max_seq_no: 0, local_checkpoint: 0, ..m }).await.unwrap();
    // One more operation, not flushed: the branch replays it.
    let lsn = wal.append(1, b"unflushed").unwrap();
    store.wal().wait_ack(lsn).await.unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let t = search_store_now();
    tokio::time::sleep(Duration::from_millis(5)).await;

    // DELETE /A while a flush is about to land.
    store.delete_shard("prod", &a).await.unwrap();
    store.write_cluster("prod", &ClusterState::new("{\"indices\":[]}")).await.unwrap();
    let err = store.write_manifest("prod", &a, &ShardManifest { max_seq_no: 1, local_checkpoint: 1, ..Default::default() }).await.unwrap_err();
    assert!(err.to_string().contains("was deleted"), "{err:#}");
    store.detach_shard(wal).await.unwrap();
    store.delete_shard("prod", &a).await.unwrap();
    // Gone for every reader, here and on other nodes, and it can't be written back.
    let other = open(&objects, &temp_dir("pit-deleted-b"), fast_config("n2")).await;
    for st in [&store, &other] {
        assert!(st.list_shards("prod").await.unwrap().is_empty());
        assert!(st.read_manifest("prod", &a).await.unwrap().is_none());
        assert!(st.read_latest_manifests("prod").await.unwrap().is_empty());
        assert!(st.recover_shards("prod").await.unwrap().is_empty());
    }
    let rec = other.recover_shard("prod", &a).await.unwrap();
    assert!(rec.manifest.is_none());
    let Err(err) = other.attach_shard(rec).await else { panic!("attached a deleted shard") };
    assert!(err.to_string().contains("was deleted"), "{err:#}");

    // Restore to just before the deletion: metadata and data.
    store.branch("prod", "restore", Some(t)).await.unwrap();
    assert_eq!(&store.read_cluster("restore").await.unwrap().unwrap().json[..], b"{\"indices\":[\"A\"]}");
    assert_eq!(store.list_shards("restore").await.unwrap(), vec![a.clone()]);
    let rm = store.read_manifest("restore", &a).await.unwrap().unwrap();
    assert_eq!((rm.segments.len(), rm.local_checkpoint), (1, 0));
    assert_eq!(&*other.open_segment(&seg).await.unwrap(), &data[..]);
    let replayed = replay_only(&other, "restore", &a).await;
    assert_eq!(replayed, vec![(1, b"unflushed".to_vec())]);
    // Branching from now (or after the deletion) has no such shard.
    store.branch("prod", "now", None).await.unwrap();
    assert!(store.list_shards("now").await.unwrap().is_empty());
    store.branch("prod", "after", Some(search_store_now())).await.unwrap();
    assert!(store.list_shards("after").await.unwrap().is_empty());

    // Inside the window the deleted shard's objects stay.
    let gc = store.collect_garbage(Duration::ZERO).await.unwrap();
    assert_eq!(gc.manifests_deleted, 0);
    store.delete_cluster("restore").await.unwrap();
    store.collect_garbage(Duration::ZERO).await.unwrap();
    assert!(objects.exists(&seg.key()).await.unwrap(), "prod's history still references it");
    // Past the window (a collector with a short one), the history goes, then its segment.
    let mut short = fast_config("n3");
    short.manifest_retain_ms = 1;
    let collector = open(&objects, &temp_dir("pit-deleted-c"), short).await;
    let gc = collector.collect_garbage(Duration::ZERO).await.unwrap();
    assert!(gc.manifests_deleted >= 2, "{gc:?}");
    assert!(objects.list(&format!("search/c/prod/i/{}/", a.index_uuid)).await.unwrap().is_empty());
    assert!(!objects.exists(&seg.key()).await.unwrap(), "{gc:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flush_after_delete_does_not_bring_the_shard_back() {
    let objects = test_objects("delete-race");
    let store = open(&objects, &temp_dir("delete-race"), fast_config("n1")).await;
    let r = shard("idxR", 0);
    let (wal, m, _) = recover(&store, "c1", &r).await;
    let lsn = wal.append(0, b"x").unwrap();
    store.wal().wait_ack(lsn).await.unwrap();
    // The flush waits for the shard's lock while the deletion holds it.
    let flush = {
        let store = store.clone();
        let r = r.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            store.write_manifest("c1", &r, &ShardManifest { max_seq_no: 0, local_checkpoint: 0, ..m }).await
        })
    };
    store.delete_shard("c1", &r).await.unwrap();
    // Whichever ran first, the shard ends deleted; a flush after the deletion fails.
    let _ = flush.await.unwrap();
    store.delete_shard("c1", &r).await.unwrap();
    let err = store.write_manifest("c1", &r, &ShardManifest { max_seq_no: 0, local_checkpoint: 0, ..Default::default() }).await.unwrap_err();
    assert!(err.to_string().contains("was deleted"), "{err:#}");
    assert!(store.list_shards("c1").await.unwrap().is_empty());
    assert!(objects.list("search/c/c1/").await.unwrap().is_empty());
    store.detach_shard(wal).await.unwrap();
    assert!(store.list_shards("c1").await.unwrap().is_empty());
    // Releasing the cluster forgets the deletion (a recreated index has a new uuid anyway).
    store.release_cluster("c1");
    assert_eq!(store.stats().tracked_shards, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn epochs_of_any_size_are_readable() {
    let objects = test_objects("big-epoch");
    let store = open(&objects, &temp_dir("big-epoch"), fast_config("n1")).await;
    let s = shard("idxBIG", 0);
    for epoch in [7, 10_000_000_000_000_000, u64::MAX] {
        store.set_epoch("big", epoch);
        let g = store.write_cluster("big", &ClusterState::new(format!("{{\"e\":\"{epoch}\"}}"))).await.unwrap();
        let st = store.read_cluster("big").await.unwrap().unwrap();
        assert_eq!((st.epoch, st.generation, st.json.clone()), (epoch, g, Bytes::from(format!("{{\"e\":\"{epoch}\"}}"))));
        let m = store.write_manifest("big", &s, &ShardManifest::default()).await.unwrap();
        let back = store.read_manifest("big", &s).await.unwrap().unwrap();
        assert_eq!((back.epoch, back.generation), (epoch, m.generation));
    }
    assert!(store.cluster_exists("big").await.unwrap());
    assert_eq!(store.read_cluster_history("big").await.unwrap().len(), 3);
    // An older epoch is fenced by the largest.
    store.set_epoch("big", 10_000_000_000_000_000);
    assert!(store.write_cluster("big", &ClusterState::new("{}")).await.unwrap_err().downcast_ref::<search_store::Fenced>().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uppercase_segment_file_names_are_cached_under_the_canonical_name() {
    let objects = test_objects("seg-case");
    let store = open(&objects, &temp_dir("seg-case"), fast_config("n1")).await;
    let id = SegmentId::random();
    let data = segment_bytes(&id, 4096);
    let path = store.segment_path(&id).with_file_name(format!("{}.seg", id.to_hex().to_uppercase()));
    std::fs::write(&path, &data).unwrap();
    assert_eq!(store.upload_segment(&path).await.unwrap(), id);
    assert!(store.is_cached(&id));
    assert_eq!(&*store.open_segment(&id).await.unwrap(), &data[..]);
    let st = store.stats().cache;
    assert_eq!((st.hits, st.misses, st.files, st.used), (1, 0, 1, 4096), "{st:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn point_in_time_window_retains_young_manifests() {
    let objects = test_objects("manifest-window");
    let mut cfg = fast_config("n1");
    cfg.manifest_retain = 1;
    cfg.manifest_retain_ms = 60_000;
    let store = open(&objects, &temp_dir("manifest-window"), cfg).await;
    let s = shard("idxW", 0);
    for _ in 0..5 {
        store.write_manifest("c1", &s, &ShardManifest::default()).await.unwrap();
    }
    assert_eq!(store.read_manifests("c1", &s).await.unwrap().len(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn window_retention_keeps_the_commit_point_current_at_the_window_start() {
    let objects = test_objects("window-edge");
    let mut cfg = fast_config("n1");
    cfg.manifest_retain = 1;
    cfg.state_retain = 1;
    cfg.manifest_retain_ms = 400;
    let store = open(&objects, &temp_dir("window-edge"), cfg).await;
    let s = shard("idxE", 0);
    for g in 1..=3 {
        store.write_manifest("c1", &s, &ShardManifest { mapping_version: g, ..Default::default() }).await.unwrap();
        store.write_cluster("c1", &ClusterState::new(format!("{{\"g\":{g}}}"))).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    let m4 = store.write_manifest("c1", &s, &ShardManifest { mapping_version: 4, ..Default::default() }).await.unwrap();
    store.write_cluster("c1", &ClusterState::new("{\"g\":4}")).await.unwrap();
    // Generations 1 and 2 were superseded before the window started; 3 was current at its start.
    let kept: Vec<u64> = store.read_manifests("c1", &s).await.unwrap().iter().map(|m| m.mapping_version).collect();
    assert_eq!(kept, vec![3, 4]);
    let states: Vec<_> = store.read_cluster_history("c1").await.unwrap().iter().map(|st| st.json.clone()).collect();
    assert_eq!(states, vec![Bytes::from_static(b"{\"g\":3}"), Bytes::from_static(b"{\"g\":4}")]);
    // A point in time just inside the window branches from generation 3 and its state.
    store.branch("c1", "c1-pit", Some(m4.written_ms - 300)).await.unwrap();
    let child = store.read_manifest("c1-pit", &s).await.unwrap().unwrap();
    assert_eq!(child.mapping_version, 3);
    assert_eq!(&store.read_cluster("c1-pit").await.unwrap().unwrap().json[..], b"{\"g\":3}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_state_generations() {
    let objects = test_objects("state");
    let mut cfg = fast_config("n1");
    cfg.state_retain = 2;
    let store = open(&objects, &temp_dir("state"), cfg).await;
    assert!(store.read_cluster("c1").await.unwrap().is_none());
    assert!(!store.cluster_exists("c1").await.unwrap());
    for i in 0..5 {
        let g = store.write_cluster("c1", &ClusterState::from_value(&serde_json::json!({"v": i})).unwrap()).await.unwrap();
        assert_eq!(g, i + 1);
    }
    let s = store.read_cluster("c1").await.unwrap().unwrap();
    assert_eq!(s.generation, 5);
    assert_eq!(s.parse::<serde_json::Value>().unwrap()["v"], 4);
    assert!(s.written_ms > 0);
    assert_eq!(store.read_cluster_history("c1").await.unwrap().len(), 2);
    assert!(store.write_cluster("c1", &ClusterState::new("nope")).await.is_err());
    assert!(store.write_cluster("bad/name", &ClusterState::new("{}")).await.is_err());
    store.delete_cluster("c1").await.unwrap();
    assert!(store.read_cluster("c1").await.unwrap().is_none());
    assert!(objects.list("search/c/c1/").await.unwrap().is_empty());
}

/// A cluster with one shard: segments `segs`, live docs on the first one.
async fn seed_cluster(store: &Store, cluster: &str, s: &search_store::ShardId, segs: &[(SegmentId, Vec<u8>)]) -> ShardManifest {
    store.write_cluster(cluster, &ClusterState::from_value(&serde_json::json!({"name": cluster})).unwrap()).await.unwrap();
    let (wal, m, _) = recover(store, cluster, s).await;
    let live = store.write_live_docs(&segs[0].0, Bytes::from_static(&[0xff, 0x7f])).await.unwrap();
    let mut refs: Vec<SegmentRef> = segs.iter().map(|(id, d)| seg_ref(*id, d.len())).collect();
    refs[0].live = Some(live);
    refs[0].del_count = 1;
    let m = store.write_manifest(cluster, s, &ShardManifest { segments: refs, max_seq_no: 9, local_checkpoint: 9, ..m }).await.unwrap();
    drop(wal);
    m
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn branches_share_segments_and_delete_independently() {
    let objects = test_objects("branch");
    let store = open(&objects, &temp_dir("branch"), fast_config("n1")).await;
    let s = shard("idxB", 0);
    let a = make_segment(&store, 5000).await;
    let b = make_segment(&store, 7000).await;
    let parent_m = seed_cluster(&store, "prod", &s, &[a.clone(), b.clone()]).await;
    store.branch("prod", "preview", None).await.unwrap();
    assert!(store.branch("prod", "preview", None).await.is_err(), "branch name taken");
    assert!(store.branch("nope", "x", None).await.is_err(), "no such parent");
    assert!(store.branch("prod", "prod", None).await.is_err());
    let child_state = store.read_cluster("preview").await.unwrap().unwrap();
    assert_eq!(child_state.parse::<serde_json::Value>().unwrap()["name"], "prod");
    let child_m = store.read_manifest("preview", &s).await.unwrap().unwrap();
    assert_eq!(child_m.segments, parent_m.segments);
    assert_eq!(child_m.generation, 1);
    assert_eq!(child_m.base.as_ref().unwrap().cluster, "prod");
    // Usage counts shared segments in full; on this node they're all cached (uploads adopt
    // the file), on a fresh node none are.
    let usage = store.cluster_usage("preview").await.unwrap();
    assert_eq!((usage.segments, usage.segment_bytes, usage.docs, usage.deleted_docs), (2, 12_000, 19, 1));
    assert_eq!(usage.cached_bytes, 12_002, "segments plus the 2-byte live docs");

    // The child opens (on another node), deletes a document in segment b and adds a segment.
    let other = open(&objects, &temp_dir("branch-b"), fast_config("n2")).await;
    let (cwal, cm, ops) = recover(&other, "preview", &s).await;
    assert!(ops.is_empty());
    assert_eq!(&*other.open_live_docs(&a.0, cm.segments[0].live.unwrap()).await.unwrap(), &[0xff, 0x7f]);
    let c = make_segment(&other, 3000).await;
    let child_live = other.write_live_docs(&b.0, Bytes::from_static(&[0x01])).await.unwrap();
    let mut segs = cm.segments.clone();
    segs[1].live = Some(child_live);
    segs.push(seg_ref(c.0, 3000));
    other.write_manifest("preview", &s, &ShardManifest { segments: segs, ..cm }).await.unwrap();
    drop(cwal);

    // The parent is untouched.
    let pm = store.read_manifest("prod", &s).await.unwrap().unwrap();
    assert_eq!(pm.segments, parent_m.segments);
    assert!(pm.segments[1].live.is_none());

    // Deleting the parent keeps everything the child references.
    store.delete_cluster("prod").await.unwrap();
    let gc = store.collect_garbage(Duration::ZERO).await.unwrap();
    assert_eq!(gc.deleted, 0, "{gc:?}");
    for key in [a.0.key(), b.0.key(), c.0.key(), a.0.live_key(parent_m.segments[0].live.unwrap()), b.0.live_key(child_live)] {
        assert!(objects.exists(&key).await.unwrap(), "{key} must survive");
    }
    assert_eq!(&*other.open_segment(&a.0).await.unwrap(), &a.1[..]);

    // Deleting the child too makes everything garbage.
    other.delete_cluster("preview").await.unwrap();
    let gc = store.collect_garbage(Duration::ZERO).await.unwrap();
    assert!(gc.deleted >= 5, "{gc:?}");
    assert_eq!(gc.manifests, 0);
    for key in [a.0.key(), b.0.key(), c.0.key()] {
        assert!(!objects.exists(&key).await.unwrap(), "{key} must be collected");
    }
    assert!(objects.list("search/seg/").await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn point_in_time_branches_replay_up_to_the_time() {
    let objects = test_objects("branch-pit");
    let store = open(&objects, &temp_dir("branch-pit"), fast_config("n1")).await;
    let s = shard("idxP", 0);
    store.write_cluster("prod", &ClusterState::new("{}")).await.unwrap();
    let before_shard = search_store_now();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let (wal, m, _) = recover(&store, "prod", &s).await;
    let mut lsn = 0;
    for seq in 0..5 {
        lsn = wal.append(seq, &payload("prod", seq)).unwrap();
    }
    store.wal().wait_ack(lsn).await.unwrap();
    // A flush covering ops up to 2.
    let m = store.write_manifest("prod", &s, &ShardManifest { max_seq_no: 2, local_checkpoint: 2, ..m }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let t_mid = search_store_now();
    tokio::time::sleep(Duration::from_millis(5)).await;
    for seq in 5..10 {
        lsn = wal.append(seq, &payload("prod", seq)).unwrap();
    }
    store.wal().wait_ack(lsn).await.unwrap();

    store.branch("prod", "at-mid", Some(t_mid)).await.unwrap();
    store.branch("prod", "now", None).await.unwrap();
    store.branch("prod", "before", Some(before_shard)).await.unwrap();
    // The parent keeps writing after the branches: they must not see it.
    for seq in 10..15 {
        lsn = wal.append(seq, &payload("prod", seq)).unwrap();
    }
    store.wal().wait_ack(lsn).await.unwrap();
    let _ = m;

    let other = open(&objects, &temp_dir("branch-pit-b"), fast_config("n2")).await;
    let seqs = |ops: Vec<(i64, Vec<u8>)>| ops.into_iter().map(|o| o.0).collect::<Vec<_>>();
    assert_eq!(seqs(replay_only(&other, "at-mid", &s).await), vec![3, 4]);
    assert_eq!(seqs(replay_only(&other, "now", &s).await), (3..10).collect::<Vec<_>>());
    assert!(other.list_shards("before").await.unwrap().is_empty(), "the shard didn't exist yet");
    // A branch replays into its own commit point; then it no longer needs the parent's WAL.
    let (bw, bm, ops) = recover(&other, "now", &s).await;
    assert_eq!(ops.len(), 7);
    assert_eq!(bm.wal.len(), 2);
    let flushed = other.write_manifest("now", &s, &ShardManifest { max_seq_no: 9, local_checkpoint: 9, ..bm }).await.unwrap();
    assert_eq!(flushed.wal.len(), 1);
    assert_eq!(flushed.wal[0].node, "n2");
    drop(bw);
    drop(wal);
}

fn search_store_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn garbage_collection_is_two_phase_and_spares_retained_manifests() {
    let objects = test_objects("gc");
    let mut cfg = fast_config("n1");
    cfg.manifest_retain = 2;
    cfg.wal.file_bytes = 2048;
    let store = open(&objects, &temp_dir("gc"), cfg).await;
    let s = shard("idxG", 0);
    let (wal, m, _) = recover(&store, "c1", &s).await;
    let (s1, _) = make_segment(&store, 1000).await;
    let (s2, _) = make_segment(&store, 1000).await;
    let (s3, _) = make_segment(&store, 1000).await;
    let (orphan, _) = make_segment(&store, 1000).await;
    let mut lsn = 0;
    for seq in 0..100 {
        lsn = wal.append(seq, &payload("c1", seq)).unwrap();
        if seq % 10 == 9 {
            store.wal().wait_ack(lsn).await.unwrap();
        }
    }
    store.wal().durable_remote(lsn).await.unwrap();
    let wal_objects = wal_files(&objects, "n1").await.len();
    assert!(wal_objects >= 3);
    // gen 2 references s1, gen 3 s2 (merge), gen 4 s3: s1 falls out of retention (2 kept).
    let m = store.write_manifest("c1", &s, &ShardManifest { segments: vec![seg_ref(s1, 1000)], max_seq_no: 49, local_checkpoint: 49, ..m }).await.unwrap();
    let m = store.write_manifest("c1", &s, &ShardManifest { segments: vec![seg_ref(s2, 1000)], ..m }).await.unwrap();
    let m = store.write_manifest("c1", &s, &ShardManifest { segments: vec![seg_ref(s3, 1000)], max_seq_no: 99, local_checkpoint: 99, ..m }).await.unwrap();
    assert_eq!(store.read_manifests("c1", &s).await.unwrap().len(), 2);

    // First pass with a grace period: found, not deleted.
    let gc = store.collect_garbage(Duration::from_secs(3600)).await.unwrap();
    assert_eq!(gc.deleted, 0);
    assert!(gc.unreferenced >= 2, "{gc:?}");
    assert!(objects.exists(&s1.key()).await.unwrap());
    // A reference appearing before the next pass rescues an object.
    let m = store.write_manifest("c1", &s, &ShardManifest { segments: vec![seg_ref(s3, 1000), seg_ref(orphan, 1000)], ..m }).await.unwrap();
    let gc = store.collect_garbage(Duration::ZERO).await.unwrap();
    assert!(gc.deleted >= 1, "{gc:?}");
    assert!(!objects.exists(&s1.key()).await.unwrap(), "only an expired manifest referenced s1");
    assert!(!objects.exists(&s2.key()).await.unwrap(), "s2 fell out of retention with gen 3");
    assert_eq!(m.generation, 5);
    assert!(objects.exists(&s3.key()).await.unwrap());
    assert!(objects.exists(&orphan.key()).await.unwrap());
    // WAL files before the oldest retained manifest's range are gone; the live range stays.
    let remaining = wal_files(&objects, "n1").await;
    assert!(remaining.len() < wal_objects, "{} of {wal_objects}", remaining.len());
    let from = store.read_manifests("c1", &s).await.unwrap().iter().flat_map(|m| m.wal.iter().map(|w| w.from)).min().unwrap();
    for (k, _) in &remaining {
        let seq: u64 = k.rsplit('/').next().unwrap().trim_end_matches(".zst").parse().unwrap();
        assert!(seq >= from, "{k} before {from}");
    }
    // Recovery still works from what's left.
    drop(wal);
    store.close().await.unwrap();
    let other = open(&objects, &temp_dir("gc-b"), fast_config("n2")).await;
    assert!(replay_only(&other, "c1", &s).await.is_empty());
    // GC never touches keys it doesn't name.
    objects.put("search/seg/README", Bytes::from_static(b"x")).await.unwrap();
    other.collect_garbage(Duration::ZERO).await.unwrap();
    assert!(objects.exists("search/seg/README").await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_wal_can_live_in_its_own_object_store() {
    let objects = test_objects("split-main");
    let wal_objects = test_objects("split-wal");
    let cfg = fast_config("n1");
    let a = Store::open_with_wal_objects(objects.clone(), wal_objects.clone(), temp_dir("split-a"), cfg).await.unwrap();
    let s = shard("idxS", 0);
    let (wal, _, _) = recover(&a, "c1", &s).await;
    let mut lsn = 0;
    for seq in 0..50 {
        lsn = wal.append(seq, &payload("c1", seq)).unwrap();
    }
    a.wal().durable_remote(lsn).await.unwrap();
    assert!(objects.list("search/wal/").await.unwrap().is_empty(), "no WAL in the main store");
    assert!(!wal_files(&wal_objects, "n1").await.is_empty());
    assert!(wal_objects.list("search/c/").await.unwrap().is_empty() && wal_objects.list("search/seg/").await.unwrap().is_empty());
    assert!(!objects.list("search/c/").await.unwrap().is_empty());
    // Crash; another node with the same split replays everything from the WAL store.
    a.wal().abandon().await;
    drop(wal);
    let b = Store::open_with_wal_objects(objects.clone(), wal_objects.clone(), temp_dir("split-b"), fast_config("n2")).await.unwrap();
    assert_eq!(b.wal_objects().description, wal_objects.description);
    let (wal, m2, ops) = recover(&b, "c1", &s).await;
    assert_eq!(ops, (0..50).map(|seq| (seq, payload("c1", seq))).collect::<Vec<_>>());
    // A flush covering everything: garbage collection deletes the WAL objects where they are.
    let (seg, _) = make_segment(&b, 500).await;
    b.write_manifest("c1", &s, &ShardManifest { segments: vec![seg_ref(seg, 500)], max_seq_no: 49, local_checkpoint: 49, ..m2 }).await.unwrap();
    drop(wal);
    let gc = b.collect_garbage(Duration::ZERO).await.unwrap();
    assert!(gc.deleted >= 1, "{gc:?}");
    assert!(wal_files(&wal_objects, "n1").await.is_empty(), "n1's WAL is no longer referenced");
    assert!(objects.exists(&seg.key()).await.unwrap());
    assert!(replay_only(&b, "c1", &s).await.is_empty());
    b.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_tenants_share_one_wal() {
    let objects = test_objects("tenants");
    let dir = temp_dir("tenants");
    const CLUSTERS: usize = 120;
    let s0 = shard("idx0", 0);
    let s1 = shard("idx1", 0);
    {
        // A strict group-commit window (no eager commits) so grouping doesn't depend on how
        // fast this machine's fsync is relative to the appends.
        let mut cfg = fast_config("n1");
        cfg.wal.eager_below = 0;
        let store = open(&objects, &dir, cfg).await;
        let mut handles = Vec::new();
        for c in 0..CLUSTERS {
            let cluster = format!("tenant-{c}");
            store.write_cluster(&cluster, &ClusterState::from_value(&serde_json::json!({ "c": c })).unwrap()).await.unwrap();
            handles.push((cluster.clone(), recover(&store, &cluster, &s0).await.0, recover(&store, &cluster, &s1).await.0));
        }
        let mut last = 0;
        for round in 0..10i64 {
            for (c, (cluster, a, b)) in handles.iter().enumerate() {
                if (c as i64 + round) % 3 == 0 {
                    continue;
                }
                last = a.append(round, &payload(cluster, round)).unwrap();
                last = b.append(round, &payload(cluster, round + 1000)).unwrap().max(last);
            }
        }
        store.wal().wait_ack(last).await.unwrap();
        let st = store.wal().stats();
        assert_eq!(st.shards, 2 * CLUSTERS as u64);
        let cs = store.wal().cluster_stats("tenant-1");
        let rounds = (0..10).filter(|r| (1 + r) % 3 != 0).count() as u64;
        assert_eq!((cs.shards, cs.uncommitted_ops), (2, 2 * rounds));
        assert!(cs.uncommitted_bytes > 0 && cs.oldest_ms.is_some());
        assert_eq!(store.wal().cluster_stats("nobody"), Default::default());
        assert!(st.commits < st.appends / 10, "{st:?}");
        // Half the tenants flush.
        for (c, (cluster, a, _)) in handles.iter().enumerate().filter(|(c, _)| c % 2 == 0) {
            let m = store.read_manifest(cluster, &s0).await.unwrap().unwrap();
            store.write_manifest(cluster, &s0, &ShardManifest { max_seq_no: 9, local_checkpoint: 9, ..m }).await.unwrap();
            assert_eq!(a.stats().uncommitted_ops, 0, "tenant {c}");
        }
        drop(handles);
        store.close().await.unwrap();
    }
    let store = open(&objects, &dir, fast_config("n1")).await;
    for c in 0..CLUSTERS {
        let cluster = format!("tenant-{c}");
        let expect: Vec<i64> = (0..10).filter(|r| (c as i64 + r) % 3 != 0).collect();
        let ops0 = replay_only(&store, &cluster, &s0).await;
        let ops1 = replay_only(&store, &cluster, &s1).await;
        if c % 2 == 0 {
            assert!(ops0.is_empty(), "tenant {c} flushed");
        } else {
            assert_eq!(ops0.iter().map(|o| o.0).collect::<Vec<_>>(), expect, "tenant {c}");
            assert!(ops0.iter().all(|(s, p)| *p == payload(&cluster, *s)));
        }
        assert_eq!(ops1.iter().map(|o| o.0).collect::<Vec<_>>(), expect, "tenant {c}");
        assert!(ops1.iter().all(|(s, p)| *p == payload(&cluster, *s + 1000)));
        assert_eq!(store.read_cluster(&cluster).await.unwrap().unwrap().parse::<serde_json::Value>().unwrap()["c"], c);
    }
    let usage = store.cluster_usage("tenant-0").await.unwrap();
    assert_eq!(usage.shards, 2);
    assert_eq!(usage.state_generation, Some(1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn point_in_time_branches_from_older_commit_points_inside_the_window() {
    for window in [60_000u64, 0] {
        let objects = test_objects("branch-window");
        let mut cfg = fast_config("n1");
        cfg.manifest_retain_ms = window;
        let store = open(&objects, &temp_dir("branch-window"), cfg).await;
        let s = shard("idxPW", 0);
        store.write_cluster("prod", &ClusterState::new("{}")).await.unwrap();
        let (wal, m, _) = recover(&store, "prod", &s).await;
        let mut lsn = 0;
        for seq in 0..5 {
            lsn = wal.append(seq, &payload("prod", seq)).unwrap();
        }
        store.wal().wait_ack(lsn).await.unwrap();
        let m = store.write_manifest("prod", &s, &ShardManifest { max_seq_no: 2, local_checkpoint: 2, ..m }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let t1 = search_store_now();
        tokio::time::sleep(Duration::from_millis(5)).await;
        for seq in 5..10 {
            lsn = wal.append(seq, &payload("prod", seq)).unwrap();
        }
        store.wal().wait_ack(lsn).await.unwrap();
        store.write_manifest("prod", &s, &ShardManifest { max_seq_no: 9, local_checkpoint: 9, ..m }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let r = store.branch("prod", "past", Some(t1)).await;
        if window == 0 {
            assert!(r.is_err(), "outside the window");
            assert!(!store.cluster_exists("past").await.unwrap());
            continue;
        }
        r.unwrap();
        store.collect_garbage(Duration::ZERO).await.unwrap();
        let other = open(&objects, &temp_dir("branch-window-b"), fast_config("n2")).await;
        let ops = replay_only(&other, "past", &s).await;
        assert_eq!(ops.iter().map(|o| o.0).collect::<Vec<_>>(), vec![3, 4]);
        let m = other.read_manifest("past", &s).await.unwrap().unwrap();
        assert_eq!(m.local_checkpoint, 2);
        drop(wal);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deposed_owner_is_fenced() {
    use search_store::{Fenced, Version, keys};
    let objects = test_objects("fence");
    let a = open(&objects, &temp_dir("fence-a"), fast_config("na")).await;
    a.set_epoch("c1", 1);
    let s = shard("idxFENCE", 0);
    a.write_cluster("c1", &ClusterState::new("{\"v\":1}")).await.unwrap();
    let (wa, ma, _) = recover(&a, "c1", &s).await;
    assert_eq!((ma.epoch, ma.generation), (1, 1));
    let mut lsn = 0;
    for seq in 0..5 {
        lsn = wa.append(seq, &payload("c1", seq)).unwrap();
    }
    a.wal().durable_remote(lsn).await.unwrap();

    // Node b takes the cluster over under epoch 2 while a still thinks it owns it.
    let b = open(&objects, &temp_dir("fence-b"), fast_config("nb")).await;
    b.set_epoch("c1", 2);
    let (wb, mb, ops) = recover(&b, "c1", &s).await;
    assert_eq!(ops.len(), 5);
    assert_eq!((mb.epoch, mb.generation), (2, 2));

    // a's flush is refused, and from then on so are its appends.
    let err = a.write_manifest("c1", &s, &ShardManifest { max_seq_no: 4, local_checkpoint: 4, ..ma.clone() }).await.unwrap_err();
    assert_eq!(err.downcast_ref::<Fenced>(), Some(&Fenced { cluster: "c1".into(), epoch: 1, owner_epoch: 2 }));
    assert!(wa.append(5, b"late").unwrap_err().downcast_ref::<Fenced>().is_some());
    assert_eq!(a.stats().fenced, 1);
    // Other clusters on a are unaffected.
    let (other, _, _) = recover(&a, "c2", &s).await;
    other.append(0, b"fine").unwrap();
    // Cluster state: once b writes it, a can't.
    b.write_cluster("c1", &ClusterState::new("{\"v\":2}")).await.unwrap();
    assert!(a.write_cluster("c1", &ClusterState::new("{\"v\":3}")).await.unwrap_err().downcast_ref::<Fenced>().is_some());
    let st = b.read_cluster("c1").await.unwrap().unwrap();
    assert_eq!((&st.json[..], st.epoch, st.generation), (&b"{\"v\":2}"[..], 2, 2));
    assert!(a.delete_shard("c1", &s).await.unwrap_err().downcast_ref::<Fenced>().is_some());

    // A stale write that raced in (lower epoch, higher generation) is never the latest.
    let stale = ShardManifest { epoch: 1, generation: 99, max_seq_no: 1000, local_checkpoint: 1000, ..Default::default() };
    objects.put(&keys::manifest_key("c1", &s, Version::new(1, 99, 0)), Bytes::from(stale.to_json())).await.unwrap();
    let latest = b.read_manifest("c1", &s).await.unwrap().unwrap();
    assert_eq!((latest.epoch, latest.generation, latest.local_checkpoint), (2, 2, mb.local_checkpoint));
    // The owner's generations continue past it, and retention removes the stale one first.
    let mut w = b.write_manifest("c1", &s, &ShardManifest { max_seq_no: 4, local_checkpoint: 4, ..mb.clone() }).await.unwrap();
    assert_eq!((w.epoch, w.generation), (2, 100));
    for _ in 0..4 {
        w = b.write_manifest("c1", &s, &w).await.unwrap();
    }
    let all = b.read_manifests("c1", &s).await.unwrap();
    assert!(all.iter().all(|m| m.epoch == 2), "{:?}", all.iter().map(|m| (m.epoch, m.generation)).collect::<Vec<_>>());

    // a can't even start recovering under its old epoch; with a newer lease it takes back over.
    drop(wa);
    assert!(a.recover_shard("c1", &s).await.unwrap_err().downcast_ref::<Fenced>().is_some());
    a.set_epoch("c1", 3);
    let (wa, ma, _) = recover(&a, "c1", &s).await;
    assert_eq!(ma.epoch, 3);
    wa.append(5, b"again").unwrap();
    assert!(b.write_manifest("c1", &s, &w).await.unwrap_err().downcast_ref::<Fenced>().is_some());
    assert!(wb.append(5, b"b is out").unwrap_err().downcast_ref::<Fenced>().is_some());
    drop((wa, wb, other));
    a.close().await.unwrap();
    b.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recreated_branch_never_reuses_manifest_keys() {
    // GC caches what each manifest key references. A branch deleted and made again under the
    // same name starts its generations over; if its keys repeated the old ones, GC would go by
    // the old branch's references and delete the new one's segments.
    let objects = test_objects("rebranch");
    let store = open(&objects, &temp_dir("rebranch"), fast_config("n1")).await;
    let s = shard("idxRE", 0);
    let base = make_segment(&store, 1000).await;
    seed_cluster(&store, "prod", &s, std::slice::from_ref(&base)).await;
    let mut kept = Vec::new();
    for round in 0..2 {
        store.branch("prod", "preview", None).await.unwrap();
        let (w, m, _) = recover(&store, "preview", &s).await;
        let seg = make_segment(&store, 2000 + round).await;
        let mut segs = m.segments.clone();
        segs.push(seg_ref(seg.0, seg.1.len()));
        let m = store.write_manifest("preview", &s, &ShardManifest { segments: segs, ..m }).await.unwrap();
        assert_eq!(m.generation, 3, "generations restart with the branch");
        drop(w);
        store.collect_garbage(Duration::ZERO).await.unwrap();
        kept.push(seg);
        if round == 0 {
            store.release_cluster("preview");
            store.delete_cluster("preview").await.unwrap();
        }
    }
    store.collect_garbage(Duration::ZERO).await.unwrap();
    let gc = store.collect_garbage(Duration::ZERO).await.unwrap();
    assert!(objects.exists(&kept[1].0.key()).await.unwrap(), "the live branch's segment must survive: {gc:?}");
    assert!(!objects.exists(&kept[0].0.key()).await.unwrap(), "the deleted branch's segment is garbage");
    assert!(objects.exists(&base.0.key()).await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waking_a_cluster_reads_every_shard_at_once() {
    let objects = test_objects("wake");
    let store = open(&objects, &temp_dir("wake"), fast_config("n1")).await;
    let shards: Vec<_> = (0..6).map(|i| shard(&format!("idxW{}", i / 2), i % 2)).collect();
    let (seg, _) = make_segment(&store, 777).await;
    for (i, s) in shards.iter().enumerate() {
        let (wal, m, _) = recover(&store, "w1", s).await;
        wal.append(0, b"op").unwrap();
        let m = store.write_manifest("w1", s, &ShardManifest { segments: vec![seg_ref(seg, 777)], max_seq_no: 0, local_checkpoint: 0, mapping_version: i as u64, ..m }).await.unwrap();
        wal.append(1, b"after the flush").unwrap();
        assert_eq!(m.generation, 2);
        store.detach_shard(wal).await.unwrap();
    }
    // A damaged newest generation of one shard fails the wake (never an older commit point).
    let damaged = search_store::keys::manifest_key("w1", &shards[3], search_store::Version::new(0, 99, 7));
    objects.put(&damaged, Bytes::from_static(b"{not json")).await.unwrap();
    let other = open(&objects, &temp_dir("wake-b"), fast_config("n2")).await;
    for err in [other.read_latest_manifests("w1").await.unwrap_err(), other.recover_shards("w1").await.unwrap_err()] {
        assert!(format!("{err:#}").contains(&format!("shard manifest {damaged} is damaged")), "{err:#}");
    }
    // Repaired (by hand): the wake goes ahead.
    objects.delete(&damaged).await.unwrap();
    let latest = other.read_latest_manifests("w1").await.unwrap();
    assert_eq!(latest.iter().map(|(s, _)| s.clone()).collect::<Vec<_>>(), {
        let mut v = shards.clone();
        v.sort();
        v
    });
    for (s, m) in &latest {
        let i = shards.iter().position(|x| x == s).unwrap();
        assert_eq!((m.mapping_version, m.local_checkpoint), (i as u64, 0));
    }
    let recs = other.recover_shards("w1").await.unwrap();
    assert_eq!(recs.len(), 6);
    let mut open_shards = Vec::new();
    for mut rec in recs {
        let mut seen = Vec::new();
        other
            .replay(&mut rec, &mut |r: &search_store::WalRecord<'_>| {
                seen.push((r.seq_no, r.payload.to_vec()));
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(seen, vec![(1, b"after the flush".to_vec())], "{}", rec.shard);
        let (wal, m) = other.attach_shard(rec).await.unwrap();
        assert!(m.generation >= 3);
        open_shards.push(wal);
    }
    let usage = other.cluster_usage("w1").await.unwrap();
    assert_eq!((usage.shards, usage.segments, usage.segment_bytes, usage.docs), (6, 6, 6 * 777, 60));
    assert!(other.recover_shards("w1").await.is_err(), "already open on this node");
    assert!(other.recover_shards("nobody").await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_collector_starts_from_its_marks_snapshot() {
    let objects = test_objects("gc-snap");
    let store = open(&objects, &temp_dir("gc-snap"), fast_config("n1")).await;
    let shards: Vec<_> = (0..5).map(|i| shard("idxGS", i)).collect();
    let mut segs = Vec::new();
    for s in &shards {
        let seg = make_segment(&store, 500).await;
        let (wal, m, _) = recover(&store, "gs", s).await;
        store.write_manifest("gs", s, &ShardManifest { segments: vec![seg_ref(seg.0, 500)], ..m }).await.unwrap();
        store.detach_shard(wal).await.unwrap();
        segs.push(seg.0);
    }
    let gc = store.collect_garbage(Duration::ZERO).await.unwrap();
    assert_eq!(gc.manifests_read, gc.manifests, "a fresh collector reads every manifest: {gc:?}");
    assert!(gc.manifests >= 5);
    assert_eq!(store.collect_garbage(Duration::ZERO).await.unwrap().manifests_read, 0, "known manifests aren't read again");
    assert!(objects.exists(search_store::keys::GC_MARKS).await.unwrap());

    // Another process (a restarted collector) only reads what is new since the snapshot, and
    // still keeps everything referenced.
    let orphan = make_segment(&store, 300).await.0;
    let fresh = make_segment(&store, 400).await.0;
    let (wal, m, _) = recover(&store, "gs", &shards[0]).await;
    store.write_manifest("gs", &shards[0], &ShardManifest { segments: vec![seg_ref(segs[0], 500), seg_ref(fresh, 400)], ..m }).await.unwrap();
    drop(wal);
    let total = store.collect_garbage(Duration::from_secs(3600)).await.unwrap().manifests;
    let restarted = open(&objects, &temp_dir("gc-snap-b"), fast_config("gc")).await;
    let gc = restarted.collect_garbage(Duration::ZERO).await.unwrap();
    assert_eq!(gc.manifests, total);
    assert_eq!(gc.manifests_read, 0, "everything was in the snapshot: {gc:?}");
    for id in segs.iter().chain([&fresh]) {
        assert!(objects.exists(&id.key()).await.unwrap(), "{id} is referenced");
    }
    assert!(!objects.exists(&orphan.key()).await.unwrap(), "the orphan was pending since the first pass");

    // A damaged snapshot costs reads, not correctness.
    objects.put(search_store::keys::GC_MARKS, Bytes::from_static(b"RSGM\x01garbage")).await.unwrap();
    let again = open(&objects, &temp_dir("gc-snap-c"), fast_config("gc2")).await;
    let gc = again.collect_garbage(Duration::ZERO).await.unwrap();
    assert_eq!((gc.manifests_read, gc.manifests), (total, total));
    for id in segs.iter().chain([&fresh]) {
        assert!(objects.exists(&id.key()).await.unwrap(), "{id} is referenced");
    }
    let gc = again.collect_garbage(Duration::ZERO).await.unwrap();
    assert_eq!(gc.manifests_read, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn point_in_time_branches_span_a_restart_of_the_writing_node() {
    let objects = test_objects("pit-restart");
    let mut cfg = fast_config("n1");
    cfg.manifest_retain_ms = 3_600_000;
    cfg.wal.file_bytes = 512;
    let cache = temp_dir("pit-restart");
    let x = shard("idxPR", 0);
    let append = |w: &search_store::WalShard, range: std::ops::Range<i64>| {
        let mut lsn = 0;
        for seq in range {
            lsn = w.append(seq, &payload("prod", seq)).unwrap();
        }
        lsn
    };
    {
        let n1 = open(&objects, &cache, cfg.clone()).await;
        n1.write_cluster("prod", &ClusterState::new("{}")).await.unwrap();
        let (w, _, _) = recover(&n1, "prod", &x).await;
        let lsn = append(&w, 0..10);
        n1.wal().durable_remote(lsn).await.unwrap();
        n1.wal().abandon().await;
        drop(w);
    }
    // The node restarts and goes on writing the shard: its history now spans two runs.
    let n1 = open(&objects, &cache, cfg.clone()).await;
    let (w, m, ops) = recover(&n1, "prod", &x).await;
    assert_eq!(ops.len(), 10);
    let lsn = append(&w, 10..30);
    n1.wal().durable_remote(lsn).await.unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let t = search_store_now();
    tokio::time::sleep(Duration::from_millis(5)).await;
    n1.write_manifest("prod", &x, &m).await.unwrap();
    let lsn = append(&w, 30..40);
    n1.wal().durable_remote(lsn).await.unwrap();
    // Another node branches to `t`: everything up to it, from both runs, and nothing after.
    let n2 = open(&objects, &temp_dir("pit-restart-b"), search_store::StoreConfig { node: "n2".into(), ..cfg.clone() }).await;
    n2.branch("prod", "then", Some(t)).await.unwrap();
    let got: Vec<i64> = replay_only(&n2, "then", &x).await.iter().map(|o| o.0).collect();
    assert_eq!(got, (0..30).collect::<Vec<_>>());
    drop(w);
}
