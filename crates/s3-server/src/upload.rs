//! Writing an object from a plain byte stream, outside the S3 API (the control plane's uploads).
//!
//! The body is hashed while it streams to object storage: MD5 for the ETag, and CRC64NVME, the
//! checksum S3 gives objects uploaded without one. It must be exactly `len` bytes: a body that
//! ends short or runs long fails the backend upload, so nothing is committed.

use crate::object::InflightBlob;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use s3_proto::checksum::{Algorithm, BodyDigest, BodyHasher};
use s3_store::{Bucket, ByteStream, Checksum, NewObject, ObjectHeaders, ObjectMeta, Store, StoreError, WriteCondition};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

type Outcome = Arc<Mutex<Option<Result<BodyDigest, String>>>>;

struct Hashing {
    inner: ByteStream,
    hasher: Option<BodyHasher>,
    len: u64,
    seen: u64,
    outcome: Outcome,
}

impl Hashing {
    fn fail(&mut self, why: String) -> Poll<Option<std::io::Result<Bytes>>> {
        self.hasher = None;
        *self.outcome.lock().unwrap() = Some(Err(why.clone()));
        Poll::Ready(Some(Err(std::io::Error::new(std::io::ErrorKind::InvalidData, why))))
    }
}

impl Stream for Hashing {
    type Item = std::io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.hasher.is_none() {
            return Poll::Ready(None);
        }
        match self.inner.as_mut().poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(b))) => {
                self.seen += b.len() as u64;
                if self.seen > self.len {
                    let len = self.len;
                    return self.fail(format!("the body is longer than its declared {len} bytes"));
                }
                if let Some(h) = self.hasher.as_mut() {
                    h.update(&b);
                }
                Poll::Ready(Some(Ok(b)))
            }
            Poll::Ready(Some(Err(e))) => {
                let why = format!("reading the body failed: {e}");
                self.fail(why)
            }
            Poll::Ready(None) => {
                if self.seen != self.len {
                    let (seen, len) = (self.seen, self.len);
                    return self.fail(format!("the body ended after {seen} of its declared {len} bytes"));
                }
                let digest = self.hasher.take().map(BodyHasher::finish);
                *self.outcome.lock().unwrap() = digest.map(Ok);
                Poll::Ready(None)
            }
        }
    }
}

/// Store `body` (exactly `len` bytes) as `bucket/key`, the way PutObject does: a fresh blob,
/// committed only once it's fully uploaded and hashed. A body of the wrong length is an
/// `InvalidArgument`, and nothing is committed.
pub async fn put_object(store: &Arc<Store>, bucket: &Bucket, key: &str, headers: ObjectHeaders, body: ByteStream, len: u64, cond: &WriteCondition) -> Result<ObjectMeta, StoreError> {
    s3_store::validate::key(key)?;
    s3_store::validate::headers(&headers)?;
    let outcome: Outcome = Arc::new(Mutex::new(None));
    let stream = Hashing { inner: body, hasher: Some(BodyHasher::new(Some(Algorithm::Crc64Nvme), false)), len, seen: 0, outcome: outcome.clone() }.boxed();
    let guard = InflightBlob::new(store);
    let blob = guard.id.clone();
    let sse = crate::sse::default_sse(bucket);
    let uploaded = store.put_blob_sse(&blob, &bucket.id, key, stream, len, headers.content_type.as_deref(), sse.as_ref()).await;
    let result = outcome.lock().unwrap().take();
    let (up, digest) = match (uploaded, result) {
        (Ok(u), Some(Ok(d))) => (u, d),
        (Ok(_), other) => {
            store.discard_blob(&blob).await;
            return Err(StoreError::InvalidArgument(other.and_then(Result::err).unwrap_or_else(|| "the body didn't finish".into())));
        }
        (Err(_), Some(Err(why))) => return Err(StoreError::InvalidArgument(why)),
        (Err(e), _) => return Err(e),
    };
    let checksum = digest.checksum_b64().map(|(a, v)| Checksum { algorithm: a.as_str().into(), kind: "FULL_OBJECT".into(), value: v });
    let enc = sse.as_ref().map(s3_store::Encryption::of);
    let obj = NewObject { blob_id: blob, size: up.size, crc32c: up.crc32c, etag: crate::sse::etag(enc.as_ref(), digest.etag()), headers, checksum };
    let meta = store.commit_put_sse(&bucket.name, key, obj, cond, enc.as_ref()).await?;
    drop(guard);
    Ok(meta)
}
