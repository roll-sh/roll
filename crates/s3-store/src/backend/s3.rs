//! AWS S3 (or an S3-compatible service) through `aws-sdk-s3`. Every blob and part carries a
//! CRC32C that S3 verifies on receipt, so bytes can't be corrupted between the gateway and S3.

use super::{Backend, BlobAttrs, ByteStream, Checked, Listed, Uploaded, UploadedPart};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use aws_sdk_s3::Client;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::{ByteStream as SdkStream, SdkBody};
use aws_sdk_s3::types::{ChecksumAlgorithm, CompletedMultipartUpload, CompletedPart, StorageClass};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use bytes::Bytes;
use futures::StreamExt;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context as TaskContext, Poll};

pub struct S3Backend {
    client: Client,
    bucket: String,
    /// `""` or `"<prefix>/"`.
    prefix: String,
    storage_class: Option<StorageClass>,
}

/// A body of known length for the SDK (which wants `Sync`).
struct SyncBody {
    inner: Mutex<Checked>,
    len: u64,
}

impl http_body::Body for SyncBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<std::result::Result<http_body::Frame<Bytes>, std::io::Error>>> {
        let inner = self.get_mut().inner.get_mut().unwrap_or_else(|e| e.into_inner());
        match futures::Stream::poll_next(Pin::new(inner), cx) {
            Poll::Ready(Some(Ok(b))) => Poll::Ready(Some(Ok(http_body::Frame::data(b)))),
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::SizeHint::with_exact(self.len)
    }
}

fn sdk_body(body: ByteStream, len: u64) -> (SdkStream, std::sync::Arc<super::CheckState>) {
    let (checked, state) = Checked::new(body, len);
    (SdkStream::new(SdkBody::from_body_1_x(SyncBody { inner: Mutex::new(checked), len })), state)
}

fn into_stream(body: SdkStream) -> ByteStream {
    futures::stream::unfold(body, |mut b| async move { b.next().await.map(|r| (r.map_err(std::io::Error::other), b)) }).boxed()
}

fn status<E, R>(e: &SdkError<E, R>) -> Option<u16>
where
    R: HasStatus,
{
    e.raw_response().map(|r| r.status_code())
}

trait HasStatus {
    fn status_code(&self) -> u16;
}

impl HasStatus for aws_sdk_s3::config::http::HttpResponse {
    fn status_code(&self) -> u16 {
        self.status().as_u16()
    }
}

fn err<E: std::error::Error + Send + Sync + 'static, R: std::fmt::Debug + Send + Sync + 'static>(what: &str, e: SdkError<E, R>) -> anyhow::Error {
    anyhow!("{what}: {}", aws_sdk_s3::error::DisplayErrorContext(e))
}

/// `bucket/key` for `x-amz-copy-source`, percent-encoded.
fn copy_source(bucket: &str, key: &str) -> String {
    let mut out = String::with_capacity(bucket.len() + key.len() + 1);
    for b in format!("{bucket}/{key}").bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn crc_b64(crc: u32) -> String {
    STANDARD.encode(crc.to_be_bytes())
}

fn crc_from_b64(s: &str) -> Result<u32> {
    let b = STANDARD.decode(s)?;
    let arr: [u8; 4] = b.as_slice().try_into().map_err(|_| anyhow!("bad CRC32C {s:?}"))?;
    Ok(u32::from_be_bytes(arr))
}

/// Backend part etags carry the part's CRC32C (needed again to complete the upload).
fn part_token(etag: &str, crc: u32) -> String {
    format!("{etag}\n{}", crc_b64(crc))
}

fn split_token(token: &str) -> Result<(&str, &str)> {
    token.split_once('\n').context("malformed part token")
}

impl S3Backend {
    pub async fn new(bucket: &str, region: Option<&str>, endpoint: Option<&str>, prefix: Option<&str>, storage_class: Option<&str>) -> Result<Self> {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(r) = region {
            loader = loader.region(aws_config::Region::new(r.to_string()));
        }
        let sdk = loader.load().await;
        let mut b = aws_sdk_s3::config::Builder::from(&sdk);
        if sdk.region().is_none() {
            b = b.region(aws_config::Region::new("us-east-1"));
        }
        if let Some(e) = endpoint {
            b = b.endpoint_url(e).force_path_style(true);
        }
        let prefix = prefix.map(|p| p.trim_matches('/')).filter(|p| !p.is_empty()).map(|p| format!("{p}/")).unwrap_or_default();
        Ok(Self {
            client: Client::from_conf(b.build()),
            bucket: bucket.to_string(),
            prefix,
            storage_class: storage_class.filter(|s| !s.is_empty()).map(StorageClass::from),
        })
    }

    fn full(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
    }

    fn meta(attrs: &BlobAttrs) -> std::collections::HashMap<String, String> {
        let mut m = std::collections::HashMap::new();
        if !attrs.bucket_id.is_empty() {
            m.insert("roll-bucket".to_string(), attrs.bucket_id.clone());
        }
        if !attrs.key.is_empty() {
            m.insert("roll-key".to_string(), URL_SAFE_NO_PAD.encode(attrs.key.as_bytes()));
        }
        m
    }
}

#[async_trait]
impl Backend for S3Backend {
    fn describe(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.prefix.trim_end_matches('/'))
    }

    async fn put(&self, key: &str, body: ByteStream, len: u64, attrs: &BlobAttrs) -> Result<Uploaded> {
        let (body, state) = sdk_body(body, len);
        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(self.full(key))
            .content_length(len as i64)
            .checksum_algorithm(ChecksumAlgorithm::Crc32C)
            .set_metadata(Some(Self::meta(attrs)))
            .body(body);
        if let Some(c) = &self.storage_class {
            req = req.storage_class(c.clone());
        }
        if let Some(ct) = &attrs.content_type {
            req = req.content_type(ct);
        }
        req.send().await.map_err(|e| err("put", e))?;
        let (size, crc32c) = state.result().context("body incomplete after upload")?;
        Ok(Uploaded { size, crc32c })
    }

    async fn put_small(&self, key: &str, data: Bytes) -> Result<()> {
        self.client.put_object().bucket(&self.bucket).key(self.full(key)).body(SdkStream::from(data)).send().await.map_err(|e| err("put", e))?;
        Ok(())
    }

    async fn put_create_only(&self, key: &str, data: Bytes) -> Result<bool> {
        match self.client.put_object().bucket(&self.bucket).key(self.full(key)).if_none_match("*").body(SdkStream::from(data)).send().await {
            Ok(_) => Ok(true),
            Err(e) if status(&e) == Some(412) => Ok(false),
            Err(e) => Err(err("create-only put", e)),
        }
    }

    async fn get(&self, key: &str, range: Option<Range<u64>>) -> Result<Option<(u64, ByteStream)>> {
        if let Some(r) = &range
            && r.start >= r.end
        {
            return Ok(self.head(key).await?.map(|_| (0, futures::stream::empty().boxed())));
        }
        let mut req = self.client.get_object().bucket(&self.bucket).key(self.full(key));
        if let Some(r) = &range {
            req = req.range(format!("bytes={}-{}", r.start, r.end - 1));
        }
        match req.send().await {
            Ok(out) => {
                let len = out.content_length().unwrap_or(0).max(0) as u64;
                Ok(Some((len, into_stream(out.body))))
            }
            Err(e) if e.as_service_error().is_some_and(|s| s.is_no_such_key()) || status(&e) == Some(404) => Ok(None),
            // A range starting at or past the end.
            Err(e) if status(&e) == Some(416) => Ok(self.head(key).await?.map(|_| (0, futures::stream::empty().boxed()))),
            Err(e) => Err(err("get", e)),
        }
    }

    async fn head(&self, key: &str) -> Result<Option<Listed>> {
        match self.client.head_object().bucket(&self.bucket).key(self.full(key)).send().await {
            Ok(out) => Ok(Some(Listed {
                key: key.to_string(),
                size: out.content_length().unwrap_or(0).max(0) as u64,
                modified_ms: out.last_modified().and_then(|t| t.to_millis().ok()).unwrap_or(0),
            })),
            Err(e) if e.as_service_error().is_some_and(|s| s.is_not_found()) || status(&e) == Some(404) => Ok(None),
            Err(e) => Err(err("head", e)),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        match self.client.delete_object().bucket(&self.bucket).key(self.full(key)).send().await {
            Ok(_) => Ok(()),
            Err(e) if status(&e) == Some(404) => Ok(()),
            Err(e) => Err(err("delete", e)),
        }
    }

    async fn list_page(&self, prefix: &str, start_after: Option<&str>, max: usize) -> Result<(Vec<Listed>, bool)> {
        let mut req = self.client.list_objects_v2().bucket(&self.bucket).prefix(self.full(prefix)).max_keys(max.clamp(1, 1000) as i32);
        if let Some(a) = start_after {
            req = req.start_after(self.full(a));
        }
        let out = req.send().await.map_err(|e| err("list", e))?;
        let mut page = Vec::new();
        for o in out.contents() {
            let Some(k) = o.key().and_then(|k| k.strip_prefix(&self.prefix)) else { continue };
            page.push(Listed {
                key: k.to_string(),
                size: o.size().unwrap_or(0).max(0) as u64,
                modified_ms: o.last_modified().and_then(|t| t.to_millis().ok()).unwrap_or(0),
            });
        }
        let more = out.is_truncated().unwrap_or(false);
        if more && page.is_empty() {
            bail!("listing {prefix}: truncated page without keys");
        }
        Ok((page, more))
    }

    async fn create_multipart(&self, key: &str, attrs: &BlobAttrs) -> Result<String> {
        let mut req = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(self.full(key))
            .checksum_algorithm(ChecksumAlgorithm::Crc32C)
            .set_metadata(Some(Self::meta(attrs)));
        if let Some(c) = &self.storage_class {
            req = req.storage_class(c.clone());
        }
        if let Some(ct) = &attrs.content_type {
            req = req.content_type(ct);
        }
        let out = req.send().await.map_err(|e| err("create multipart upload", e))?;
        out.upload_id().map(str::to_string).context("no upload id in CreateMultipartUpload response")
    }

    async fn upload_part(&self, key: &str, upload_id: &str, part: u32, body: ByteStream, len: u64) -> Result<UploadedPart> {
        let (body, state) = sdk_body(body, len);
        let out = self
            .client
            .upload_part()
            .bucket(&self.bucket)
            .key(self.full(key))
            .upload_id(upload_id)
            .part_number(part as i32)
            .content_length(len as i64)
            .checksum_algorithm(ChecksumAlgorithm::Crc32C)
            .body(body)
            .send()
            .await
            .map_err(|e| err("upload part", e))?;
        let (size, crc32c) = state.result().context("part body incomplete after upload")?;
        if let Some(c) = out.checksum_crc32_c() {
            anyhow::ensure!(crc_from_b64(c)? == crc32c, "S3 stored part {part} with another CRC32C");
        }
        let etag = out.e_tag().context("no ETag for uploaded part")?;
        Ok(UploadedPart { etag: part_token(etag, crc32c), size, crc32c })
    }

    async fn upload_part_copy(&self, key: &str, upload_id: &str, part: u32, src_key: &str, range: Option<Range<u64>>) -> Result<UploadedPart> {
        let src = self.head(src_key).await?.with_context(|| format!("copy source {src_key} not found"))?;
        let mut req = self
            .client
            .upload_part_copy()
            .bucket(&self.bucket)
            .key(self.full(key))
            .upload_id(upload_id)
            .part_number(part as i32)
            .copy_source(copy_source(&self.bucket, &self.full(src_key)));
        let size = match &range {
            Some(r) => {
                anyhow::ensure!(r.start < r.end && r.end <= src.size, "copy range outside the source");
                req = req.copy_source_range(format!("bytes={}-{}", r.start, r.end - 1));
                r.end - r.start
            }
            None => src.size,
        };
        let out = req.send().await.map_err(|e| err("upload part copy", e))?;
        let res = out.copy_part_result().context("no CopyPartResult")?;
        let etag = res.e_tag().context("no ETag for copied part")?;
        let crc32c = crc_from_b64(res.checksum_crc32_c().context("no CRC32C for copied part")?)?;
        Ok(UploadedPart { etag: part_token(etag, crc32c), size, crc32c })
    }

    async fn complete_multipart(&self, key: &str, upload_id: &str, parts: &[(u32, String)]) -> Result<()> {
        let mut done = Vec::with_capacity(parts.len());
        for (n, token) in parts {
            let (etag, crc) = split_token(token)?;
            done.push(CompletedPart::builder().part_number(*n as i32).e_tag(etag).checksum_crc32_c(crc).build());
        }
        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(self.full(key))
            .upload_id(upload_id)
            .multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(done)).build())
            .send()
            .await
            .map_err(|e| err("complete multipart upload", e))?;
        Ok(())
    }

    async fn abort_multipart(&self, key: &str, upload_id: &str) -> Result<()> {
        match self.client.abort_multipart_upload().bucket(&self.bucket).key(self.full(key)).upload_id(upload_id).send().await {
            Ok(_) => Ok(()),
            Err(e) if status(&e) == Some(404) => Ok(()),
            Err(e) => Err(err("abort multipart upload", e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_source_is_encoded() {
        assert_eq!(copy_source("b", "p/s3/blobs/ab/abc"), "b/p/s3/blobs/ab/abc");
        assert_eq!(copy_source("b", "a b+c"), "b/a%20b%2Bc");
    }

    #[test]
    fn part_tokens_round_trip() {
        let t = part_token("\"abc\"", 0xdeadbeef);
        let (etag, crc) = split_token(&t).unwrap();
        assert_eq!(etag, "\"abc\"");
        assert_eq!(crc_from_b64(crc).unwrap(), 0xdeadbeef);
    }
}
