#![allow(dead_code)]

use bytes::Bytes;
use futures::StreamExt;
use md5::{Digest, Md5};
use s3_store::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

pub const DAY: i64 = 86_400_000;

/// A clock tests can move forward.
#[derive(Clone, Default)]
pub struct TestClock(pub Arc<AtomicI64>);

impl TestClock {
    pub fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }

    pub fn clock(&self) -> Clock {
        let off = self.0.clone();
        let sys = system_clock();
        Arc::new(move || sys() + off.load(Ordering::SeqCst))
    }
}

pub fn config(clock: &TestClock) -> StoreConfig {
    let dir = tempfile::tempdir().unwrap().keep();
    let mut c = StoreConfig::new(dir).manual();
    c.clock = clock.clock();
    c.min_part_size = 5;
    c.commit_ms = 2;
    c
}

pub async fn open(backend: &Arc<dyn Backend>, clock: &TestClock) -> Arc<Store> {
    Store::open(config(clock), backend.clone()).await.unwrap()
}

pub async fn env(name: &str) -> (Arc<dyn Backend>, TestClock, Arc<Store>) {
    let backend = test_backend(name).await;
    let clock = TestClock::default();
    let store = open(&backend, &clock).await;
    (backend, clock, store)
}

pub fn md5_hex(data: &[u8]) -> String {
    Md5::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

pub async fn put_with(store: &Store, bucket: &str, key: &str, data: &[u8], cond: &WriteCondition) -> Result<ObjectMeta> {
    let b = store.bucket(bucket)?;
    let blob = store.new_blob();
    let up = store.put_blob(&blob, &b.id, key, bytes_body(Bytes::copy_from_slice(data)), data.len() as u64, Some("text/plain")).await?;
    assert_eq!(up.size, data.len() as u64);
    assert_eq!(up.crc32c, crc32c::crc32c(data));
    let headers = ObjectHeaders { content_type: Some("text/plain".into()), ..Default::default() };
    store.commit_put(bucket, key, NewObject { blob_id: blob, size: up.size, crc32c: up.crc32c, etag: md5_hex(data), headers, checksum: None }, cond).await
}

pub async fn put(store: &Store, bucket: &str, key: &str, data: &[u8]) -> ObjectMeta {
    put_with(store, bucket, key, data, &WriteCondition::none()).await.unwrap()
}

pub async fn read(store: &Store, bucket: &str, key: &str) -> Option<Vec<u8>> {
    let meta = match store.get_object(bucket, key) {
        Ok(m) => m,
        Err(StoreError::NoSuchKey(_)) => return None,
        Err(e) => panic!("{e}"),
    };
    let (len, mut body) = store.open_blob(&meta, None).await.unwrap();
    let mut out = Vec::new();
    while let Some(c) = body.next().await {
        out.extend_from_slice(&c.unwrap());
    }
    assert_eq!(out.len() as u64, len);
    assert_eq!(len, meta.size);
    Some(out)
}

pub fn list(store: &Store, bucket: &str, prefix: &str, delimiter: Option<&str>) -> (Vec<String>, Vec<String>) {
    let mut keys = Vec::new();
    let mut prefixes = Vec::new();
    let mut after = None;
    loop {
        let page = store
            .list_objects(bucket, &ListQuery { prefix: prefix.into(), delimiter: delimiter.map(str::to_string), start_after: after.clone(), max_keys: 2 })
            .unwrap();
        keys.extend(page.objects.into_iter().map(|o| o.key));
        prefixes.extend(page.prefixes);
        if !page.truncated {
            return (keys, prefixes);
        }
        after = page.next;
    }
}

pub fn keys(store: &Store, bucket: &str) -> Vec<String> {
    list(store, bucket, "", None).0
}

/// Blob keys in object storage.
pub async fn blobs(backend: &Arc<dyn Backend>) -> Vec<String> {
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let (page, more) = backend.list_page("s3/blobs/", after.as_deref(), 1000).await.unwrap();
        after = page.last().map(|l| l.key.clone()).or(after);
        out.extend(page.into_iter().map(|l| l.key));
        if !more {
            return out;
        }
    }
}
