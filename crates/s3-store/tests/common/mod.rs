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

/// The store's metadata database (a read-only view of the working copy).
pub fn meta_db(store: &Store) -> rusqlite::Connection {
    let c = rusqlite::Connection::open_with_flags(store.cfg.dir.join("meta.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    c
}

/// `(size, crc32c, refs, unreferenced_at)` of a blob's row.
pub fn blob_row(store: &Store, id: &str) -> Option<(u64, u32, i64, Option<i64>)> {
    use rusqlite::OptionalExtension;
    meta_db(store)
        .query_row("SELECT size, crc32c, refs, unreferenced_at FROM blobs WHERE id = ?1", [id], |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u32, r.get(2)?, r.get(3)?)))
        .optional()
        .unwrap()
}

fn rows<T: rusqlite::types::FromSql>(c: &rusqlite::Connection, sql: &str) -> Vec<(String, T)> {
    c.prepare(sql).unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap()
}

/// The bookkeeping invariants: `refs` counts exactly the `objects`, `versions`, `trash` and
/// `derived` rows pointing at each blob (a derived image's blob has exactly one, and its source
/// is a known blob), `unreferenced_at` is set exactly when it's zero, no row points at a
/// blob without metadata, bucket counters match their rows, tombstones only exist in branches,
/// noncurrent versions only exist behind a current version the bucket owns, a key has at most
/// one null version, nothing dangles, and every referenced blob is in object storage with the
/// recorded size.
pub async fn assert_consistent(store: &Store, backend: &Arc<dyn Backend>) {
    let c = meta_db(store);
    let refs: std::collections::BTreeMap<String, i64> = rows(&c, "SELECT id, refs FROM blobs").into_iter().collect();
    let counted: std::collections::BTreeMap<String, i64> = rows(
        &c,
        "SELECT blob_id, count(*) FROM (SELECT blob_id FROM objects WHERE blob_id IS NOT NULL UNION ALL SELECT blob_id FROM versions WHERE blob_id IS NOT NULL
           UNION ALL SELECT blob_id FROM trash UNION ALL SELECT blob_id FROM derived) GROUP BY blob_id",
    )
    .into_iter()
    .collect();
    for (blob, n) in &counted {
        assert_eq!(refs.get(blob), Some(n), "blob {blob}: refs vs rows pointing at it");
    }
    for (blob, n) in &refs {
        assert_eq!(counted.get(blob).copied().unwrap_or(0), *n, "blob {blob}: refs vs rows pointing at it");
    }
    let bad: Vec<(String, i64)> = rows(&c, "SELECT id, refs FROM blobs WHERE (refs = 0) != (unreferenced_at IS NOT NULL) OR refs < 0");
    assert!(bad.is_empty(), "unreferenced_at out of step with refs: {bad:?}");
    let counters: Vec<(String, String)> = rows(
        &c,
        "SELECT b.name, b.objects || ' ' || b.bytes || ' ' || b.trash_bytes || ' ' || b.versions || ' ' || b.version_bytes || ' / ' ||
                (SELECT count(*) FROM objects o WHERE o.bucket_id = b.id AND o.blob_id IS NOT NULL) || ' ' ||
                (SELECT coalesce(sum(size), 0) FROM objects o WHERE o.bucket_id = b.id AND o.blob_id IS NOT NULL) || ' ' ||
                (SELECT coalesce(sum(size), 0) FROM trash t WHERE t.bucket_id = b.id) || ' ' ||
                (SELECT count(*) FROM versions v WHERE v.bucket_id = b.id) || ' ' ||
                (SELECT coalesce(sum(size), 0) FROM versions v WHERE v.bucket_id = b.id) FROM buckets b",
    );
    for (name, v) in counters {
        let (stored, actual) = v.split_once(" / ").unwrap();
        assert_eq!(stored, actual, "bucket {name}: objects bytes trash_bytes versions version_bytes counters vs rows");
    }
    let dangling: Vec<(String, String)> = rows(
        &c,
        "SELECT 'object', bucket_id || '/' || key FROM objects WHERE bucket_id NOT IN (SELECT id FROM buckets)
         UNION ALL SELECT 'version', bucket_id || '/' || key FROM versions WHERE bucket_id NOT IN (SELECT id FROM buckets)
         UNION ALL SELECT 'trash', bucket_id || '/' || key FROM trash WHERE bucket_id NOT IN (SELECT id FROM buckets)
         UNION ALL SELECT 'upload', id FROM uploads WHERE bucket_id NOT IN (SELECT id FROM buckets)
         UNION ALL SELECT 'part', upload_id FROM parts WHERE upload_id NOT IN (SELECT id FROM uploads)
         UNION ALL SELECT 'encryption', blob_id FROM blob_sse WHERE blob_id NOT IN (SELECT id FROM blobs) AND blob_id NOT IN (SELECT blob_id FROM uploads)
         UNION ALL SELECT 'derived image of an unknown source', blob_id FROM derived WHERE source_blob NOT IN (SELECT id FROM blobs)
         UNION ALL SELECT 'derived image whose blob is also an object', d.blob_id FROM derived d WHERE EXISTS (SELECT 1 FROM objects o WHERE o.blob_id = d.blob_id)
           OR EXISTS (SELECT 1 FROM versions v WHERE v.blob_id = d.blob_id) OR EXISTS (SELECT 1 FROM trash t WHERE t.blob_id = d.blob_id)
         UNION ALL SELECT 'tombstone', bucket_id || '/' || key FROM objects WHERE blob_id IS NULL AND delete_marker = 0 AND bucket_id IN (SELECT id FROM buckets WHERE parent_id IS NULL)
         UNION ALL SELECT 'version without its current one', v.bucket_id || '/' || v.key FROM versions v
           WHERE NOT EXISTS (SELECT 1 FROM objects o WHERE o.bucket_id = v.bucket_id AND o.key = v.key AND (o.blob_id IS NOT NULL OR o.delete_marker = 1))
         UNION ALL SELECT 'two null versions', bucket_id || '/' || key FROM (SELECT bucket_id, key FROM objects WHERE is_null = 1 AND (blob_id IS NOT NULL OR delete_marker = 1)
           UNION ALL SELECT bucket_id, key FROM versions WHERE is_null = 1) GROUP BY bucket_id, key HAVING count(*) > 1
         UNION ALL SELECT 'version without an id', bucket_id || '/' || key FROM objects WHERE vid = '' AND (blob_id IS NOT NULL OR delete_marker = 1)
         UNION ALL SELECT 'version without an id', bucket_id || '/' || key FROM versions WHERE vid = ''
         UNION ALL SELECT 'trashed delete marker', bucket_id || '/' || key FROM trash WHERE delete_marker = 1
         UNION ALL SELECT 'live delete marker', bucket_id || '/' || key FROM objects WHERE delete_marker = 1 AND blob_id IS NOT NULL
         UNION ALL SELECT 'noncurrent newer than current', v.bucket_id || '/' || v.key FROM versions v JOIN objects o ON o.bucket_id = v.bucket_id AND o.key = v.key WHERE v.vid <= o.vid",
    );
    assert!(dangling.is_empty(), "dangling rows: {dangling:?}");
    let live: Vec<(String, i64)> = rows(&c, "SELECT id, size FROM blobs WHERE refs > 0");
    for (id, size) in live {
        let found = backend.head(&blob_key(&id)).await.unwrap();
        assert_eq!(found.map(|l| l.size), Some(size as u64), "referenced blob {id} in object storage");
    }
}

/// A body that yields `chunks`, then fails with an I/O error if `fail`.
pub fn chunked_body(chunks: Vec<&'static [u8]>, fail: bool) -> ByteStream {
    let mut items: Vec<std::io::Result<Bytes>> = chunks.into_iter().map(|c| Ok(Bytes::from_static(c))).collect();
    if fail {
        items.push(Err(std::io::Error::other("client went away")));
    }
    futures::stream::iter(items).boxed()
}
