//! Shared helpers for search-store integration tests.
#![allow(dead_code)]

use search_store::{Objects, ShardId, StorageConfig, Store, StoreConfig, WalConfig, WalRecord};
use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// Fails any single allocation over 1 GiB, as Linux does, so a buffer sized from a length read
/// off disk or the network aborts the tests on every platform.
struct Capped;

const MAX_ALLOC: usize = 1 << 30;

unsafe impl GlobalAlloc for Capped {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.alloc(layout) } }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.size() > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.alloc_zeroed(layout) } }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.realloc(ptr, layout, new_size) } }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Capped = Capped;

pub fn temp_dir(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = search_store::test_dir().join(format!("{name}-{}", N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Local-directory object storage (for tests that copy or inspect it).
pub fn local_objects(dir: &Path) -> Objects {
    Objects::new(&StorageConfig::Local(dir.to_path_buf())).unwrap()
}

/// A config with short timers so tests run fast.
pub fn fast_config(node: &str) -> StoreConfig {
    StoreConfig {
        node: node.into(),
        wal: WalConfig { flush_delay: Duration::from_millis(1), ship_interval: Duration::from_millis(50), ..WalConfig::default() },
        ..StoreConfig::default()
    }
}

pub async fn open(objects: &Objects, dir: &Path, cfg: StoreConfig) -> Store {
    Store::open(objects.clone(), dir, cfg).await.unwrap()
}

pub fn shard(uuid: &str, n: u32) -> ShardId {
    ShardId::new(uuid, n)
}

/// Recover, replay and attach a shard; returns the replayed `(seq_no, payload)` pairs.
pub async fn recover(store: &Store, cluster: &str, s: &ShardId) -> (search_store::WalShard, search_store::ShardManifest, Vec<(i64, Vec<u8>)>) {
    let mut rec = store.recover_shard(cluster, s).await.unwrap();
    let mut ops = Vec::new();
    store.replay(&mut rec, &mut |r: &WalRecord<'_>| {
        ops.push((r.seq_no, r.payload.to_vec()));
        Ok(())
    })
    .await
    .unwrap();
    let (wal, m) = store.attach_shard(rec).await.unwrap();
    (wal, m, ops)
}

/// Replay without attaching.
pub async fn replay_only(store: &Store, cluster: &str, s: &ShardId) -> Vec<(i64, Vec<u8>)> {
    let mut rec = store.recover_shard(cluster, s).await.unwrap();
    let mut ops = Vec::new();
    store.replay(&mut rec, &mut |r: &WalRecord<'_>| {
        ops.push((r.seq_no, r.payload.to_vec()));
        Ok(())
    })
    .await
    .unwrap();
    ops
}

pub fn payload(cluster: &str, seq: i64) -> Vec<u8> {
    format!("{cluster}:op:{seq}:{}", "x".repeat((seq % 50) as usize)).into_bytes()
}

pub fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dst = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dst);
        } else {
            std::fs::copy(e.path(), dst).unwrap();
        }
    }
}

/// Poll until `f` holds (or panic after a while).
pub async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..500 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

/// A node's WAL objects (`search/wal/<node>/*.zst`), sorted by key: run markers aside.
pub async fn wal_files(objects: &Objects, node: &str) -> Vec<(String, u64)> {
    let mut v: Vec<(String, u64)> =
        objects.list(&format!("search/wal/{node}/")).await.unwrap().into_iter().filter(|(k, _)| search_store::keys::parse_wal_key(k).is_some()).collect();
    v.sort();
    v
}
