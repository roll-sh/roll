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
}

const PART_SIZE: usize = 8 * 1024 * 1024;

impl Objects {
    pub fn new(config: &StorageConfig) -> anyhow::Result<Self> {
        Ok(match config {
            StorageConfig::Local(dir) => {
                std::fs::create_dir_all(dir)?;
                let store = LocalFileSystem::new_with_prefix(dir)?.with_automatic_cleanup(true);
                Self { store: Arc::new(store), description: format!("file://{}", dir.display()) }
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
                Self { store, description }
            }
        })
    }

    pub async fn put(&self, key: &str, data: Bytes) -> anyhow::Result<()> {
        self.store.put(&ObjPath::from(key), PutPayload::from_bytes(data)).await?;
        Ok(())
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

    #[test]
    fn keys() {
        assert_eq!(wal_key("br_1", "000000010000000000000001.zst"), "branches/br_1/wal/000000010000000000000001.zst");
        assert_eq!(base_key("br_1", "x.tar.zst"), "branches/br_1/base/x.tar.zst");
        assert_eq!(branch_prefix("br_1"), "branches/br_1/");
    }
}
