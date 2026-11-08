//! Object storage: AWS S3 (or any S3-compatible service) in production, a local directory in dev.
//!
//! Layout, per branch:
//! ```text
//! branches/<branch>/wal/<segment>.zst           completed 16 MiB WAL segments (zstd)
//! branches/<branch>/wal/<segment>.partial.zst   the segment being written, refreshed every few seconds
//! branches/<branch>/wal/<timeline>.history      timeline history files
//! branches/<branch>/base/<name>.tar.zst         base images (online backups and cold snapshots)
//! branches/<branch>/base/<name>.json            their metadata, so state can be rebuilt from S3 alone
//! ```

use bytes::Bytes;
use futures::StreamExt;
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjPath;
use object_store::prefix::PrefixStore;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload, WriteMultipart};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Clone, Debug)]
pub enum StorageConfig {
    /// A directory on this machine (zero-setup development).
    Local(PathBuf),
    /// S3 or an S3-compatible endpoint (MinIO, R2, …). Credentials come from the usual AWS env vars
    /// or the instance role.
    S3 { bucket: String, region: Option<String>, endpoint: Option<String>, prefix: Option<String> },
}

#[derive(Clone)]
pub struct Objects {
    store: Arc<dyn ObjectStore>,
    pub description: String,
    durable: bool,
    /// A local directory: conditional replaces are emulated ([`Objects::put_replacing`]).
    local: bool,
}

/// Which version of an object a conditional write expects to replace (its ETag, and its version
/// id on a versioned bucket).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectVersion {
    pub e_tag: Option<String>,
    pub version: Option<String>,
}

impl ObjectVersion {
    fn of_put(r: object_store::PutResult) -> Self {
        Self { e_tag: r.e_tag, version: r.version }
    }

    fn update(&self) -> object_store::UpdateVersion {
        object_store::UpdateVersion { e_tag: self.e_tag.clone(), version: self.version.clone() }
    }
}

/// Serialises a local directory's emulated compare-and-swap: as strong as one process.
fn local_cas_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

const PART_SIZE: usize = 8 * 1024 * 1024;

impl Objects {
    /// A store over `config`. A local directory doesn't fsync: a blob can be lost on a crash or
    /// power loss after its write returned. See [`Objects::new_durable`].
    pub fn new(config: &StorageConfig) -> anyhow::Result<Self> {
        Self::open(config, false)
    }

    /// Like [`Objects::new`], but a local directory fsyncs every blob it writes, and the
    /// directories it creates, before the write returns, so whatever references a blob after a
    /// successful write can rely on it surviving power loss. S3 is unchanged: its writes are
    /// durable when they return.
    pub fn new_durable(config: &StorageConfig) -> anyhow::Result<Self> {
        Self::open(config, true)
    }

    /// Whether a write is on stable storage when it returns.
    pub fn durable(&self) -> bool {
        self.durable
    }

    fn open(config: &StorageConfig, fsync: bool) -> anyhow::Result<Self> {
        Ok(match config {
            StorageConfig::Local(dir) => {
                std::fs::create_dir_all(dir)?;
                let store = LocalFileSystem::new_with_prefix(dir)?.with_automatic_cleanup(true).with_fsync(fsync);
                Self { store: Arc::new(store), description: format!("file://{}", dir.display()), durable: fsync, local: true }
            }
            StorageConfig::S3 { bucket, region, endpoint, prefix } => {
                let mut b = AmazonS3Builder::from_env().with_bucket_name(bucket);
                if let Some(r) = region {
                    b = b.with_region(r);
                }
                if let Some(e) = endpoint {
                    b = b.with_endpoint(e).with_allow_http(e.starts_with("http://"));
                }
                let s3 = b.build()?;
                let description = match prefix {
                    Some(p) => format!("s3://{bucket}/{p}"),
                    None => format!("s3://{bucket}"),
                };
                let store: Arc<dyn ObjectStore> = match prefix {
                    Some(p) => Arc::new(PrefixStore::new(s3, p.as_str())),
                    None => Arc::new(s3),
                };
                Self { store, description, durable: true, local: false }
            }
        })
    }

    pub async fn put(&self, key: &str, data: Bytes) -> anyhow::Result<()> {
        self.store.put(&ObjPath::from(key), PutPayload::from_bytes(data)).await?;
        Ok(())
    }

    /// An object and its version (for [`Objects::put_replacing`]).
    pub async fn get_versioned(&self, key: &str) -> anyhow::Result<Option<(Bytes, ObjectVersion)>> {
        match self.store.get(&ObjPath::from(key)).await {
            Ok(r) => {
                let v = ObjectVersion { e_tag: r.meta.e_tag.clone(), version: r.meta.version.clone() };
                Ok(Some((r.bytes().await?, v)))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The current version of an object (one HEAD), `None` when it doesn't exist.
    pub async fn version(&self, key: &str) -> anyhow::Result<Option<ObjectVersion>> {
        match self.store.head(&ObjPath::from(key)).await {
            Ok(m) => Ok(Some(ObjectVersion { e_tag: m.e_tag, version: m.version })),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Create `key` only if no object is there (S3 `If-None-Match: *`). `None` when another
    /// object is there. A retried request that finds its own earlier attempt stored (same bytes)
    /// counts as created.
    pub async fn put_new(&self, key: &str, data: Bytes) -> anyhow::Result<Option<ObjectVersion>> {
        let opts = object_store::PutOptions { mode: object_store::PutMode::Create, ..Default::default() };
        match self.store.put_opts(&ObjPath::from(key), PutPayload::from_bytes(data.clone()), opts).await {
            Ok(r) => Ok(Some(ObjectVersion::of_put(r))),
            Err(object_store::Error::AlreadyExists { .. }) => self.same_bytes(key, &data).await,
            Err(e) => Err(e.into()),
        }
    }

    /// Replace `key` only if it's still at version `expected` (S3 `If-Match`): a compare-and-swap.
    /// `None` when it changed (or is gone). A retried request whose earlier attempt went through
    /// (the object holds these bytes) counts as replaced.
    ///
    /// A local directory has no conditional replace; it's emulated under a lock, which makes it a
    /// compare-and-swap between the writers of one process only.
    pub async fn put_replacing(&self, key: &str, data: Bytes, expected: &ObjectVersion) -> anyhow::Result<Option<ObjectVersion>> {
        if self.local {
            let _cas = local_cas_lock().lock().await;
            if self.version(key).await?.is_none_or(|v| v.e_tag != expected.e_tag) {
                return Ok(None);
            }
            let r = self.store.put(&ObjPath::from(key), PutPayload::from_bytes(data)).await?;
            return Ok(Some(ObjectVersion::of_put(r)));
        }
        let opts = object_store::PutOptions { mode: object_store::PutMode::Update(expected.update()), ..Default::default() };
        match self.store.put_opts(&ObjPath::from(key), PutPayload::from_bytes(data.clone()), opts).await {
            Ok(r) => Ok(Some(ObjectVersion::of_put(r))),
            Err(object_store::Error::Precondition { .. }) => self.same_bytes(key, &data).await,
            Err(e) => Err(e.into()),
        }
    }

    /// The object's version if it holds exactly `data` (an earlier attempt of this write).
    async fn same_bytes(&self, key: &str, data: &Bytes) -> anyhow::Result<Option<ObjectVersion>> {
        Ok(self.get_versioned(key).await?.filter(|(b, _)| b == data).map(|(_, v)| v))
    }

    pub async fn get(&self, key: &str) -> anyhow::Result<Option<Bytes>> {
        match self.store.get(&ObjPath::from(key)).await {
            Ok(r) => Ok(Some(r.bytes().await?)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Bytes `range` of an object (one ranged GET). `None` when the object doesn't exist.
    pub async fn get_range(&self, key: &str, range: std::ops::Range<u64>) -> anyhow::Result<Option<Bytes>> {
        match self.store.get_range(&ObjPath::from(key), range).await {
            Ok(b) => Ok(Some(b)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The size of an object, `None` when it doesn't exist.
    pub async fn size(&self, key: &str) -> anyhow::Result<Option<u64>> {
        match self.store.head(&ObjPath::from(key)).await {
            Ok(m) => Ok(Some(m.size)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn exists(&self, key: &str) -> anyhow::Result<bool> {
        match self.store.head(&ObjPath::from(key)).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn delete(&self, key: &str) -> anyhow::Result<()> {
        match self.store.delete(&ObjPath::from(key)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// All objects under `prefix` as `(key, size)`.
    pub async fn list(&self, prefix: &str) -> anyhow::Result<Vec<(String, u64)>> {
        let prefix = ObjPath::from(prefix.trim_end_matches('/'));
        let mut out = Vec::new();
        let mut stream = self.store.list(Some(&prefix));
        while let Some(meta) = stream.next().await {
            match meta {
                Ok(m) => out.push((m.location.to_string(), m.size)),
                Err(object_store::Error::NotFound { .. }) => break,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(out)
    }

    pub async fn delete_prefix(&self, prefix: &str) -> anyhow::Result<usize> {
        let keys = self.list(prefix).await?;
        let n = keys.len();
        let futs = keys.into_iter().map(|(k, _)| async move { self.delete(&k).await });
        futures::stream::iter(futs).buffer_unordered(16).collect::<Vec<_>>().await.into_iter().collect::<anyhow::Result<Vec<_>>>()?;
        Ok(n)
    }

    /// Stream a reader into one object (multipart). Returns the number of bytes stored.
    pub async fn upload(&self, key: &str, mut reader: impl AsyncRead + Unpin) -> anyhow::Result<u64> {
        let upload = self.store.put_multipart(&ObjPath::from(key)).await?;
        let mut writer = WriteMultipart::new_with_chunk_size(upload, PART_SIZE);
        let mut buf = vec![0u8; 1024 * 1024];
        let mut total = 0u64;
        loop {
            let n = match reader.read(&mut buf).await {
                Ok(n) => n,
                Err(e) => {
                    let _ = writer.abort().await;
                    return Err(e.into());
                }
            };
            if n == 0 {
                break;
            }
            writer.wait_for_capacity(4).await?;
            writer.write(&buf[..n]);
            total += n as u64;
        }
        writer.finish().await?;
        Ok(total)
    }

    /// Stream a reader into one zstd-compressed object, letting `inspect` see the raw bytes.
    /// Returns `(raw bytes, stored bytes)`.
    pub async fn upload_zstd(
        &self,
        key: &str,
        mut reader: impl AsyncRead + Unpin,
        mut inspect: impl FnMut(&[u8]),
    ) -> anyhow::Result<(u64, u64)> {
        use std::io::Write;
        let upload = self.store.put_multipart(&ObjPath::from(key)).await?;
        let mut writer = WriteMultipart::new_with_chunk_size(upload, PART_SIZE);
        let mut encoder = zstd::stream::Encoder::new(Vec::with_capacity(PART_SIZE * 2), 1)?;
        let mut buf = vec![0u8; 1024 * 1024];
        let (mut raw, mut stored) = (0u64, 0u64);
        let result: anyhow::Result<()> = async {
            loop {
                let n = reader.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                inspect(&buf[..n]);
                raw += n as u64;
                encoder.write_all(&buf[..n])?;
                if encoder.get_ref().len() >= PART_SIZE {
                    let chunk = std::mem::take(encoder.get_mut());
                    stored += chunk.len() as u64;
                    writer.wait_for_capacity(4).await?;
                    writer.write(&chunk);
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            let _ = writer.abort().await;
            return Err(e);
        }
        let rest = encoder.finish()?;
        stored += rest.len() as u64;
        writer.write(&rest);
        writer.finish().await?;
        Ok((raw, stored))
    }

    /// Stream one object into a writer. Returns the number of bytes written.
    pub async fn download(&self, key: &str, mut writer: impl AsyncWrite + Unpin) -> anyhow::Result<u64> {
        let mut stream = self.store.get(&ObjPath::from(key)).await?.into_stream();
        let mut total = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            writer.write_all(&chunk).await?;
            total += chunk.len() as u64;
        }
        writer.flush().await?;
        Ok(total)
    }
}

pub fn branch_prefix(branch: &str) -> String {
    format!("branches/{branch}/")
}

pub fn wal_key(branch: &str, file: &str) -> String {
    format!("branches/{branch}/wal/{file}")
}

pub fn base_key(branch: &str, name: &str) -> String {
    format!("branches/{branch}/base/{name}")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn temp_objects(name: &str) -> (Objects, PathBuf) {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("roll-objects-{name}-{n}"));
        (Objects::new(&StorageConfig::Local(dir.clone())).unwrap(), dir)
    }

    #[tokio::test]
    async fn put_get_list_delete() {
        let (o, _) = temp_objects("crud");
        assert_eq!(o.get("a/b").await.unwrap(), None);
        assert!(!o.exists("a/b").await.unwrap());
        o.put("a/b", Bytes::from_static(b"hello")).await.unwrap();
        o.put("a/c/d", Bytes::from_static(b"!")).await.unwrap();
        o.put("z", Bytes::from_static(b"z")).await.unwrap();
        assert_eq!(o.get("a/b").await.unwrap().unwrap(), "hello");
        let mut listed = o.list("a/").await.unwrap();
        listed.sort();
        assert_eq!(listed, vec![("a/b".to_string(), 5), ("a/c/d".to_string(), 1)]);
        assert_eq!(o.list("missing/").await.unwrap(), vec![]);
        assert_eq!(o.delete_prefix("a/").await.unwrap(), 2);
        assert!(o.get("a/b").await.unwrap().is_none());
        assert!(o.get("z").await.unwrap().is_some());
        o.delete("never-existed").await.unwrap();
    }

    #[tokio::test]
    async fn conditional_writes() {
        let (o, _) = temp_objects("cas");
        let v1 = o.put_new("m", Bytes::from_static(b"one")).await.unwrap().expect("created");
        assert!(o.put_new("m", Bytes::from_static(b"other")).await.unwrap().is_none(), "create-only");
        assert_eq!(o.put_new("m", Bytes::from_static(b"one")).await.unwrap(), Some(v1.clone()), "a retry of the same create");
        let v2 = o.put_replacing("m", Bytes::from_static(b"two"), &v1).await.unwrap().expect("swapped");
        assert!(o.put_replacing("m", Bytes::from_static(b"three"), &v1).await.unwrap().is_none(), "stale version");
        assert_eq!(o.get_versioned("m").await.unwrap().unwrap(), (Bytes::from_static(b"two"), v2.clone()));
        assert_eq!(o.version("m").await.unwrap(), Some(v2.clone()));
        o.delete("m").await.unwrap();
        assert!(o.put_replacing("m", Bytes::from_static(b"four"), &v2).await.unwrap().is_none(), "gone");
    }

    #[tokio::test]
    async fn ranges_and_sizes() {
        let (o, _) = temp_objects("range");
        o.put("r", Bytes::from_static(b"0123456789")).await.unwrap();
        assert_eq!(o.get_range("r", 2..5).await.unwrap().unwrap(), "234");
        assert_eq!(o.size("r").await.unwrap(), Some(10));
        assert_eq!(o.size("missing").await.unwrap(), None);
        assert!(o.get_range("missing", 0..1).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn streaming_upload_and_download() {
        let (o, _) = temp_objects("stream");
        let data: Vec<u8> = (0..20 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(o.upload("big", &data[..]).await.unwrap(), data.len() as u64);
        let mut out = Vec::new();
        assert_eq!(o.download("big", &mut out).await.unwrap(), data.len() as u64);
        assert_eq!(out, data);
    }

    #[tokio::test]
    async fn zstd_upload_sees_raw_bytes() {
        let (o, _) = temp_objects("zstd");
        let data = vec![7u8; 12 * 1024 * 1024];
        let mut seen = 0usize;
        let (raw, stored) = o.upload_zstd("z.zst", &data[..], |c| seen += c.len()).await.unwrap();
        assert_eq!((raw as usize, seen), (data.len(), data.len()));
        assert!(stored < raw / 100, "repetitive data should compress well");
        let back = zstd::decode_all(&o.get("z.zst").await.unwrap().unwrap()[..]).unwrap();
        assert_eq!(back, data);
    }

    #[tokio::test]
    async fn durable_local_store_fsyncs_and_works_the_same() {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("roll-objects-durable-{n}"));
        let o = Objects::new_durable(&StorageConfig::Local(dir.clone())).unwrap();
        assert!(o.durable());
        assert!(!Objects::new(&StorageConfig::Local(dir.clone())).unwrap().durable(), "plain local stores don't fsync");
        // Puts into new directories, multipart uploads and zstd uploads all go through the
        // fsyncing paths and read back the same.
        o.put("code/blobs/ab/cdef", Bytes::from_static(b"blob")).await.unwrap();
        assert_eq!(o.get("code/blobs/ab/cdef").await.unwrap().unwrap(), "blob");
        let data: Vec<u8> = (0..(PART_SIZE as u32 * 2 + 12345)).map(|i| (i % 253) as u8).collect();
        assert_eq!(o.upload("code/packs/big", &data[..]).await.unwrap(), data.len() as u64);
        let mut out = Vec::new();
        o.download("code/packs/big", &mut out).await.unwrap();
        assert_eq!(out, data);
        o.upload_zstd("z.zst", &data[..], |_| {}).await.unwrap();
        assert_eq!(zstd::decode_all(&o.get("z.zst").await.unwrap().unwrap()[..]).unwrap(), data);
        // Overwrites replace atomically; nothing staged is left behind.
        o.put("code/blobs/ab/cdef", Bytes::from_static(b"again")).await.unwrap();
        assert_eq!(o.get("code/blobs/ab/cdef").await.unwrap().unwrap(), "again");
        let mut listed: Vec<String> = o.list("").await.unwrap().into_iter().map(|(k, _)| k).collect();
        listed.sort();
        assert_eq!(listed, ["code/blobs/ab/cdef", "code/packs/big", "z.zst"]);
        o.delete_prefix("code/").await.unwrap();
        assert!(o.get("code/packs/big").await.unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn s3_is_durable_either_way() {
        let config = StorageConfig::S3 {
            bucket: "b".into(),
            region: Some("us-east-1".into()),
            endpoint: Some("http://[IP]:9".into()),
            prefix: Some("p".into()),
        };
        let (plain, durable) = (Objects::new(&config).unwrap(), Objects::new_durable(&config).unwrap());
        assert!(plain.durable() && durable.durable());
        assert_eq!(plain.description, durable.description);
    }

    #[test]
    fn keys() {
        assert_eq!(wal_key("br_1", "000000010000000000000001.zst"), "branches/br_1/wal/000000010000000000000001.zst");
        assert_eq!(base_key("br_1", "x.tar.zst"), "branches/br_1/base/x.tar.zst");
        assert_eq!(branch_prefix("br_1"), "branches/br_1/");
    }
}
