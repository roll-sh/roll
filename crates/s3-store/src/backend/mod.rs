//! Where bytes live: S3 (or an S3-compatible service) in production, a local directory in dev
//! and tests. Blobs, metadata log entries and snapshots all go through this trait.

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use pg_storage::StorageConfig;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

mod local;
mod s3;

pub use local::LocalBackend;
pub use s3::S3Backend;

/// A body: chunks of bytes, or the I/O error that ended it.
pub type ByteStream = BoxStream<'static, std::io::Result<Bytes>>;

/// What a blob is, for the record (`x-amz-meta-roll-bucket` / `-key` on S3).
#[derive(Clone, Debug, Default)]
pub struct BlobAttrs {
    pub bucket_id: String,
    pub key: String,
    pub content_type: Option<String>,
}

/// One object in a listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    /// Relative to the backend's root (its prefix stripped).
    pub key: String,
    pub size: u64,
    pub modified_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Uploaded {
    pub size: u64,
    /// CRC32C of the bytes stored (S3 verified the same value on its side).
    pub crc32c: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadedPart {
    /// The backend's ETag for the part, as `complete_multipart` needs it back.
    pub etag: String,
    pub size: u64,
    pub crc32c: u32,
}

#[async_trait]
pub trait Backend: Send + Sync + 'static {
    fn describe(&self) -> String;

    /// Store a blob (with the configured storage class). `len` must be the body's exact length;
    /// a body that is shorter, longer or ends in an error stores nothing.
    async fn put(&self, key: &str, body: ByteStream, len: u64, attrs: &BlobAttrs) -> Result<Uploaded>;

    /// A small object (metadata snapshots, markers), overwriting. Standard storage class.
    async fn put_small(&self, key: &str, data: Bytes) -> Result<()>;

    /// Create `key` only if it doesn't exist (`If-None-Match: *`). `Ok(false)`: it exists.
    async fn put_create_only(&self, key: &str, data: Bytes) -> Result<bool>;

    /// The object (or `range` of it, clamped to its size) and how many bytes the stream holds.
    async fn get(&self, key: &str, range: Option<Range<u64>>) -> Result<Option<(u64, ByteStream)>>;

    async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>> {
        let Some((len, mut body)) = self.get(key, None).await? else { return Ok(None) };
        let mut out = Vec::with_capacity(len.min(64 << 20) as usize);
        while let Some(chunk) = body.next().await {
            out.extend_from_slice(&chunk?);
        }
        Ok(Some(out.into()))
    }

    async fn head(&self, key: &str) -> Result<Option<Listed>>;

    /// Idempotent: deleting a missing object succeeds.
    async fn delete(&self, key: &str) -> Result<()>;

    /// One page of objects under `prefix`, in key order, after `start_after`. The flag says
    /// whether there are more.
    async fn list_page(&self, prefix: &str, start_after: Option<&str>, max: usize) -> Result<(Vec<Listed>, bool)>;

    /// Start a multipart upload of `key`; returns the backend's upload id.
    async fn create_multipart(&self, key: &str, attrs: &BlobAttrs) -> Result<String>;

    async fn upload_part(&self, key: &str, upload_id: &str, part: u32, body: ByteStream, len: u64) -> Result<UploadedPart>;

    /// A part copied from (a range of) another object, server-side where the backend can.
    async fn upload_part_copy(&self, key: &str, upload_id: &str, part: u32, src_key: &str, range: Option<Range<u64>>) -> Result<UploadedPart>;

    /// `parts`: `(part number, backend etag)` in ascending order.
    async fn complete_multipart(&self, key: &str, upload_id: &str, parts: &[(u32, String)]) -> Result<()>;

    /// Idempotent.
    async fn abort_multipart(&self, key: &str, upload_id: &str) -> Result<()>;
}

/// Every object under `prefix`, all pages.
pub async fn list_all(backend: &dyn Backend, prefix: &str) -> Result<Vec<Listed>> {
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let (page, more) = backend.list_page(prefix, after.as_deref(), 1000).await?;
        after = page.last().map(|l| l.key.clone()).or(after);
        out.extend(page);
        if !more {
            return Ok(out);
        }
    }
}

/// The backend for rolld's storage settings. `storage_class` applies to blobs on S3
/// (`INTELLIGENT_TIERING` in production, `STANDARD` for MinIO/SeaweedFS).
pub async fn from_config(config: &StorageConfig, storage_class: Option<&str>) -> Result<Arc<dyn Backend>> {
    Ok(match config {
        StorageConfig::Local(dir) => Arc::new(LocalBackend::new(dir)?),
        StorageConfig::S3 { bucket, region, endpoint, prefix } => {
            Arc::new(S3Backend::new(bucket, region.as_deref(), endpoint.as_deref(), prefix.as_deref(), storage_class).await?)
        }
    })
}

/// What a [`Checked`] body saw, readable after it was handed to an uploader.
#[derive(Default)]
pub(crate) struct CheckState {
    seen: std::sync::atomic::AtomicU64,
    crc: std::sync::atomic::AtomicU32,
    /// The body ended cleanly with exactly the declared length.
    complete: std::sync::atomic::AtomicBool,
}

impl CheckState {
    /// `(size, crc32c)` if the body was read completely and had the declared length.
    pub(crate) fn result(&self) -> Option<(u64, u32)> {
        use std::sync::atomic::Ordering::SeqCst;
        self.complete.load(SeqCst).then(|| (self.seen.load(SeqCst), self.crc.load(SeqCst)))
    }
}

/// Wraps a body to check its length and compute its CRC32C as it goes.
pub(crate) struct Checked {
    inner: ByteStream,
    expected: u64,
    seen: u64,
    crc: u32,
    done: bool,
    state: Arc<CheckState>,
}

impl Checked {
    pub(crate) fn new(inner: ByteStream, expected: u64) -> (Self, Arc<CheckState>) {
        let state = Arc::new(CheckState::default());
        (Self { inner, expected, seen: 0, crc: 0, done: false, state: state.clone() }, state)
    }
}

impl Stream for Checked {
    type Item = std::io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        use std::sync::atomic::Ordering::SeqCst;
        if self.done {
            return Poll::Ready(None);
        }
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                self.seen += chunk.len() as u64;
                if self.seen > self.expected {
                    self.done = true;
                    return Poll::Ready(Some(Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "body longer than its declared length"))));
                }
                self.crc = crc32c::crc32c_append(self.crc, &chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                self.done = true;
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                self.done = true;
                if self.seen != self.expected {
                    return Poll::Ready(Some(Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!("body ended after {} of {} bytes", self.seen, self.expected),
                    ))));
                }
                self.state.seen.store(self.seen, SeqCst);
                self.state.crc.store(self.crc, SeqCst);
                self.state.complete.store(true, SeqCst);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A body from bytes in memory.
pub fn bytes_body(data: Bytes) -> ByteStream {
    futures::stream::once(async move { Ok(data) }).boxed()
}

/// Object storage for tests: S3 under a unique prefix when `ROLL_E2E_S3_BUCKET` is set (with
/// `ROLL_E2E_S3_ENDPOINT` and the usual AWS credentials; storage class `STANDARD`), otherwise
/// a fresh temporary directory.
pub async fn test_backend(name: &str) -> Arc<dyn Backend> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let unique = format!("{name}-{}-{n}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
    let cfg = match std::env::var("ROLL_E2E_S3_BUCKET") {
        Ok(bucket) => StorageConfig::S3 {
            bucket,
            region: std::env::var("AWS_REGION").ok(),
            endpoint: std::env::var("ROLL_E2E_S3_ENDPOINT").ok(),
            prefix: Some(format!("s3-tests/{unique}")),
        },
        Err(_) => StorageConfig::Local(std::env::temp_dir().join(format!("roll-s3-{unique}"))),
    };
    from_config(&cfg, Some("STANDARD")).await.expect("test object storage")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn drain(chunks: Vec<std::io::Result<Bytes>>, expected: u64) -> (Vec<std::io::Result<Bytes>>, Option<(u64, u32)>) {
        let (checked, state) = Checked::new(futures::stream::iter(chunks).boxed(), expected);
        let out = checked.collect::<Vec<_>>().await;
        (out, state.result())
    }

    #[tokio::test]
    async fn checked_bodies_only_complete_at_their_declared_length() {
        let ok = |s: &'static str| Ok(Bytes::from_static(s.as_bytes()));
        let (out, r) = drain(vec![ok("hello "), ok("world")], 11).await;
        assert!(out.iter().all(|c| c.is_ok()));
        assert_eq!(r, Some((11, crc32c::crc32c(b"hello world"))));
        let (out, r) = drain(vec![], 0).await;
        assert!(out.is_empty());
        assert_eq!(r, Some((0, 0)));
        // Shorter: an error at the end, never "complete".
        let (out, r) = drain(vec![ok("hello")], 11).await;
        assert_eq!(out.last().unwrap().as_ref().unwrap_err().kind(), std::io::ErrorKind::UnexpectedEof);
        assert_eq!(r, None);
        // Longer: the extra chunk is never passed on.
        let (out, r) = drain(vec![ok("hello"), ok(" world")], 5).await;
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].as_ref().unwrap_err().kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(r, None);
        // A failing body stops at the error.
        let (out, r) = drain(vec![ok("hel"), Err(std::io::Error::other("gone")), ok("lo")], 5).await;
        assert_eq!(out.len(), 2);
        assert_eq!(r, None);
    }
}
