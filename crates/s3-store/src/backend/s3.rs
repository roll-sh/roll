//! AWS S3 (or an S3-compatible service) through `aws-sdk-s3`. Every blob and part carries a
//! CRC32C that S3 verifies on receipt, so bytes can't be corrupted between the gateway and S3.
//!
//! Server-side encryption is passed through: SSE-C keys (per request, never kept) and SSE-KMS
//! parameters go on the blob's own requests, and S3 does the crypto. Every write checks that
//! the service confirmed the encryption it was asked for: an S3-compatible service that ignores
//! the headers would otherwise store plaintext, so what it stored is deleted again and the
//! request fails with [`SseUnsupported`].

use super::{Backend, BlobAttrs, BlobSse, ByteStream, Checked, CopySource, Listed, S3Location, SseUnsupported, Uploaded, UploadedPart};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use aws_sdk_s3::Client;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::{ByteStream as SdkStream, SdkBody};
use aws_sdk_s3::types::{ChecksumAlgorithm, ChecksumMode, CompletedMultipartUpload, CompletedPart, MetadataDirective, ServerSideEncryption, StorageClass};
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
    /// SSE-KMS requests are passed through (real AWS: on by default without a custom endpoint).
    kms: bool,
    /// Real AWS S3 (no custom endpoint), which encrypts every object with SSE-S3.
    aws: bool,
    endpoint: Option<String>,
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

/// SSE-C headers for the object itself, on any request builder that has them.
macro_rules! customer_key {
    ($req:expr, $sse:expr) => {{
        let mut r = $req;
        if let Some(k) = $sse.and_then(BlobSse::customer) {
            r = r.sse_customer_algorithm("AES256").sse_customer_key(k.b64()).sse_customer_key_md5(k.md5_b64());
        }
        r
    }};
}

/// SSE-C headers for a copy's source.
macro_rules! copy_source_key {
    ($req:expr, $sse:expr) => {{
        let mut r = $req;
        if let Some(k) = $sse.and_then(BlobSse::customer) {
            r = r.copy_source_sse_customer_algorithm("AES256").copy_source_sse_customer_key(k.b64()).copy_source_sse_customer_key_md5(k.md5_b64());
        }
        r
    }};
}

/// SSE-KMS headers on a write.
macro_rules! kms_params {
    ($req:expr, $sse:expr) => {{
        let mut r = $req;
        if let Some(BlobSse::Kms(p)) = $sse {
            r = r
                .server_side_encryption(if p.dsse { ServerSideEncryption::AwsKmsDsse } else { ServerSideEncryption::AwsKms })
                .set_ssekms_key_id(p.key_id.clone())
                .set_ssekms_encryption_context(p.context.clone())
                .set_bucket_key_enabled(p.bucket_key);
        }
        r
    }};
}

/// Whether a write's response confirms the encryption asked for.
fn confirmed(want: Option<&BlobSse>, customer_algorithm: Option<&str>, customer_md5: Option<&str>, sse: Option<&ServerSideEncryption>) -> bool {
    match want {
        None => true,
        Some(BlobSse::Customer(k)) => customer_algorithm == Some("AES256") && customer_md5.is_none_or(|m| m == k.md5_b64()),
        Some(BlobSse::Kms(p)) => sse == Some(if p.dsse { &ServerSideEncryption::AwsKmsDsse } else { &ServerSideEncryption::AwsKms }),
    }
}

fn unconfirmed(what: &str, want: Option<&BlobSse>) -> anyhow::Error {
    let kind = match want {
        Some(BlobSse::Kms(_)) => "SSE-KMS",
        _ => "SSE-C",
    };
    SseUnsupported(format!("the backing object store didn't confirm {kind} for a {what}; it doesn't support it")).into()
}

/// A refusal of the encryption itself (S3-compatible services without SSE answer 501, or 400
/// naming it) is [`SseUnsupported`]; anything else stays an error.
fn sse_err<E: std::error::Error + Send + Sync + 'static>(what: &str, want: Option<&BlobSse>, e: SdkError<E, aws_sdk_s3::config::http::HttpResponse>) -> anyhow::Error {
    let code = status(&e);
    let text = format!("{}", aws_sdk_s3::error::DisplayErrorContext(e));
    if want.is_some() && (code == Some(501) || (code == Some(400) && (text.contains("ncryption") || text.contains("SSE")))) {
        return SseUnsupported(format!("the backing object store refused server-side encryption for a {what}: {text}")).into();
    }
    anyhow!("{what}: {text}")
}

/// Some S3-compatible services (SeaweedFS) repeat SSE response headers, which the SDK refuses
/// for a header it expects once. Exact duplicates are collapsed; S3 itself never sends them.
#[derive(Debug)]
struct OneSseHeader;

impl aws_sdk_s3::config::Intercept for OneSseHeader {
    fn name(&self) -> &'static str {
        "OneSseHeader"
    }

    fn modify_before_deserialization(
        &self,
        ctx: &mut aws_sdk_s3::config::interceptors::BeforeDeserializationInterceptorContextMut<'_>,
        _rc: &aws_sdk_s3::config::RuntimeComponents,
        _cfg: &mut aws_sdk_s3::config::ConfigBag,
    ) -> std::result::Result<(), aws_sdk_s3::error::BoxError> {
        let headers = ctx.response_mut().headers_mut();
        for name in [
            "x-amz-server-side-encryption",
            "x-amz-server-side-encryption-customer-algorithm",
            "x-amz-server-side-encryption-customer-key-md5",
            "x-amz-server-side-encryption-aws-kms-key-id",
            "x-amz-server-side-encryption-context",
            "x-amz-server-side-encryption-bucket-key-enabled",
        ] {
            let values: Vec<String> = headers.get_all(name).map(str::to_string).collect();
            if values.len() > 1 && values.iter().all(|v| *v == values[0]) {
                headers.insert(name, values[0].clone());
            }
        }
        Ok(())
    }
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
        let mut b = aws_sdk_s3::config::Builder::from(&sdk).interceptor(OneSseHeader);
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
            kms: endpoint.is_none(),
            aws: endpoint.is_none(),
            endpoint: endpoint.map(|e| e.trim_end_matches('/').to_string()),
        })
    }

    /// Pass SSE-KMS through or not (default: only on real AWS, without a custom endpoint).
    pub fn with_kms(mut self, on: bool) -> Self {
        self.kms = on;
        self
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

    fn supports_kms(&self) -> bool {
        self.kms
    }

    fn encrypts_at_rest(&self) -> bool {
        self.aws
    }

    fn s3_location(&self, key: &str) -> Option<S3Location> {
        Some(S3Location { bucket: self.bucket.clone(), key: self.full(key), endpoint: self.endpoint.clone(), region: self.client.config().region().map(|r| r.to_string()) })
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
        let sse = attrs.sse.as_ref();
        let req = kms_params!(customer_key!(req, sse), sse);
        let out = req.send().await.map_err(|e| sse_err("put", sse, e))?;
        if !confirmed(sse, out.sse_customer_algorithm(), out.sse_customer_key_md5(), out.server_side_encryption()) {
            // Stored, but not encrypted as asked: never keep that.
            let _ = self.delete(key).await;
            return Err(unconfirmed("put", sse));
        }
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

    async fn get_blob(&self, key: &str, range: Option<Range<u64>>, sse: Option<&BlobSse>) -> Result<Option<(u64, ByteStream)>> {
        if let Some(r) = &range
            && r.start >= r.end
        {
            return Ok(self.head(key).await?.map(|_| (0, futures::stream::empty().boxed())));
        }
        let mut req = customer_key!(self.client.get_object().bucket(&self.bucket).key(self.full(key)), sse);
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
            // An SSE-C object can't be HEADed without its key; its listing entry says the same.
            Err(e) if status(&e) == Some(400) => {
                let (page, _) = self.list_page(key, None, 1).await?;
                match page.into_iter().next().filter(|l| l.key == key) {
                    Some(l) => Ok(Some(l)),
                    None => Err(err("head", e)),
                }
            }
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
        let sse = attrs.sse.as_ref();
        let req = kms_params!(customer_key!(req, sse), sse);
        let out = req.send().await.map_err(|e| sse_err("multipart upload", sse, e))?;
        let id = out.upload_id().map(str::to_string).context("no upload id in CreateMultipartUpload response")?;
        if !confirmed(sse, out.sse_customer_algorithm(), out.sse_customer_key_md5(), out.server_side_encryption()) {
            let _ = self.abort_multipart(key, &id).await;
            return Err(unconfirmed("multipart upload", sse));
        }
        Ok(id)
    }

    async fn upload_part(&self, key: &str, upload_id: &str, part: u32, body: ByteStream, len: u64, sse: Option<&BlobSse>) -> Result<UploadedPart> {
        let (body, state) = sdk_body(body, len);
        let req = self
            .client
            .upload_part()
            .bucket(&self.bucket)
            .key(self.full(key))
            .upload_id(upload_id)
            .part_number(part as i32)
            .content_length(len as i64)
            .checksum_algorithm(ChecksumAlgorithm::Crc32C)
            .body(body);
        let out = customer_key!(req, sse).send().await.map_err(|e| sse_err("upload part", sse, e))?;
        if sse.and_then(BlobSse::customer).is_some() && !confirmed(sse, out.sse_customer_algorithm(), out.sse_customer_key_md5(), None) {
            return Err(unconfirmed("part", sse));
        }
        let (size, crc32c) = state.result().context("part body incomplete after upload")?;
        if let Some(c) = out.checksum_crc32_c() {
            anyhow::ensure!(crc_from_b64(c)? == crc32c, "S3 stored part {part} with another CRC32C");
        }
        let etag = out.e_tag().context("no ETag for uploaded part")?;
        Ok(UploadedPart { etag: part_token(etag, crc32c), size, crc32c })
    }

    async fn upload_part_copy(&self, key: &str, upload_id: &str, part: u32, src: &CopySource<'_>, sse: Option<&BlobSse>) -> Result<UploadedPart> {
        let mut req = self
            .client
            .upload_part_copy()
            .bucket(&self.bucket)
            .key(self.full(key))
            .upload_id(upload_id)
            .part_number(part as i32)
            .copy_source(copy_source(&self.bucket, &self.full(src.key)));
        let size = match &src.range {
            Some(r) => {
                anyhow::ensure!(r.start < r.end && r.end <= src.size, "copy range outside the source");
                req = req.copy_source_range(format!("bytes={}-{}", r.start, r.end - 1));
                r.end - r.start
            }
            None => src.size,
        };
        let req = copy_source_key!(customer_key!(req, sse), src.sse);
        let out = req.send().await.map_err(|e| sse_err("upload part copy", sse.or(src.sse), e))?;
        if sse.and_then(BlobSse::customer).is_some() && !confirmed(sse, out.sse_customer_algorithm(), out.sse_customer_key_md5(), None) {
            return Err(unconfirmed("part copy", sse));
        }
        let res = out.copy_part_result().context("no CopyPartResult")?;
        let etag = res.e_tag().context("no ETag for copied part")?;
        let crc32c = crc_from_b64(res.checksum_crc32_c().context("no CRC32C for copied part")?)?;
        Ok(UploadedPart { etag: part_token(etag, crc32c), size, crc32c })
    }

    async fn copy(&self, src: &CopySource<'_>, key: &str, attrs: &BlobAttrs) -> Result<Uploaded> {
        anyhow::ensure!(src.range.is_none(), "a whole-object copy has no range");
        let mut req = self
            .client
            .copy_object()
            .bucket(&self.bucket)
            .key(self.full(key))
            .copy_source(copy_source(&self.bucket, &self.full(src.key)))
            .metadata_directive(MetadataDirective::Replace)
            .set_metadata(Some(Self::meta(attrs)))
            .checksum_algorithm(ChecksumAlgorithm::Crc32C);
        if let Some(c) = &self.storage_class {
            req = req.storage_class(c.clone());
        }
        if let Some(ct) = &attrs.content_type {
            req = req.content_type(ct);
        }
        let sse = attrs.sse.as_ref();
        let req = copy_source_key!(kms_params!(customer_key!(req, sse), sse), src.sse);
        let out = req.send().await.map_err(|e| sse_err("copy", sse.or(src.sse), e))?;
        if !confirmed(sse, out.sse_customer_algorithm(), out.sse_customer_key_md5(), out.server_side_encryption()) {
            let _ = self.delete(key).await;
            return Err(unconfirmed("copy", sse));
        }
        let crc = match out.copy_object_result().and_then(|r| r.checksum_crc32_c()) {
            Some(c) => crc_from_b64(c)?,
            // Not in the copy's result: S3 keeps it with the object.
            None => {
                let head = customer_key!(self.client.head_object().bucket(&self.bucket).key(self.full(key)).checksum_mode(ChecksumMode::Enabled), sse);
                let h = head.send().await.map_err(|e| err("head of a copy", e))?;
                crc_from_b64(h.checksum_crc32_c().context("no CRC32C for the copied object")?)?
            }
        };
        Ok(Uploaded { size: src.size, crc32c: crc })
    }

    async fn complete_multipart(&self, key: &str, upload_id: &str, parts: &[(u32, String)], sse: Option<&BlobSse>) -> Result<()> {
        let mut done = Vec::with_capacity(parts.len());
        for (n, token) in parts {
            let (etag, crc) = split_token(token)?;
            done.push(CompletedPart::builder().part_number(*n as i32).e_tag(etag).checksum_crc32_c(crc).build());
        }
        let req = self
            .client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(self.full(key))
            .upload_id(upload_id)
            .multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(done)).build());
        customer_key!(req, sse).send().await.map_err(|e| err("complete multipart upload", e))?;
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
