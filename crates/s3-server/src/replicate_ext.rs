//! Replication to buckets outside roll: real AWS S3 buckets (another account or region) and
//! any S3-compatible endpoint (another roll deployment included), configured as replication
//! targets (`s3_store::targets`).
//!
//! It runs off the same durable outbox as replication between roll buckets: a version is
//! `PENDING` until its row is handled here, then `COMPLETED` (or `FAILED`, with an
//! `OperationFailedReplication` event), in one durable write that also deletes the row
//! (`Store::finish_external_replication`). A crash before that write replicates the version
//! again: writing the same bytes to the same key twice is harmless (the target's history gets
//! one more version), and nothing is ever lost.
//!
//! **The bytes.** Unlike replication inside roll (metadata only), a target needs a copy:
//! - **Server-side** when the target is the backing store's own service with the same
//!   credentials (no access key of its own, same endpoint): S3 `CopyObject` from the blob
//!   (`UploadPartCopy` beyond 5 GiB). The bytes never pass through roll.
//! - **Streamed** otherwise: read from the blob and written to the target, with a multipart
//!   upload for large objects, part by part (each part a ranged read, retried on its own).
//!   Memory stays bounded by the HTTP buffers whatever the size.
//!
//! **Integrity.** The blob's CRC32C (S3 verified it when it was written) must match what the
//! target stores: a single PUT sends it as `x-amz-checksum-crc32c`, so the target refuses
//! anything else; a multipart upload sends each part with its own CRC32C (the target verifies
//! each), and the parts' CRC32Cs, combined here as they stream, must equal the blob's before the
//! upload is completed; a server-side copy's CRC32C must equal it before the version counts as
//! replicated.
//!
//! **What's replicated** is S3's: the bytes, `Content-Type` and the other content headers,
//! user metadata and tags; `StorageClass` and `AccessControlTranslation`
//! (`bucket-owner-full-control`) are applied on the target; delete markers become a
//! `DeleteObject` on the target. The target assigns its own version ids. SSE-KMS versions
//! replicate only with `SourceSelectionCriteria` `SseKmsEncryptedObjects` (encrypted on the
//! target with the destination's `ReplicaKmsKeyID`, else the target's default); SSE-C versions
//! can't (roll never keeps a customer key) and fail.

use crate::notify::Notifier;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::{ByteStream as SdkStream, DateTime, DateTimeFormat, SdkBody};
use aws_sdk_s3::types::{ChecksumAlgorithm, CompletedMultipartUpload, CompletedPart, MetadataDirective, ObjectCannedAcl, ServerSideEncryption, StorageClass, TaggingDirective};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use futures::Stream;
use s3_store::{ByteStream, Encryption, PendingEvent, ReplicaSource, ReplicationTarget, Replicated, Store};
use std::collections::{BTreeMap, HashMap};
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

/// S3's largest single `CopyObject`.
const MAX_SINGLE_COPY: u64 = 5 << 30;
/// Attempts per part before the whole object is retried later.
const PART_ATTEMPTS: u32 = 3;

/// Replication latency to one destination (Replication Time Control's `ReplicationLatency`).
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Latency {
    /// Of the last version replicated, from its write to the replica, ms.
    pub last_ms: u64,
    /// The largest since the deployment started, ms.
    pub max_ms: u64,
    /// Versions replicated since the deployment started.
    pub replicated: u64,
    /// When the last one was (Unix ms).
    pub last_at: i64,
}

/// Target clients and replication metrics, one per [`Notifier`].
pub(crate) struct External {
    store: Arc<Store>,
    /// By target name, with the target's `updated_at` (a changed target gets a new client).
    clients: tokio::sync::Mutex<HashMap<String, (i64, Client)>>,
    /// By (source bucket id, destination key).
    latency: Mutex<HashMap<(String, String), Latency>>,
    pub(crate) part_size: AtomicU64,
    pub(crate) multipart_threshold: AtomicU64,
    /// Server-side copies made (for tests and stats).
    pub(crate) server_side: AtomicU32,
}

/// Why a replication didn't happen: try again, or it never will (`FAILED`).
enum Fail {
    Retry(String),
    Permanent(String),
}

fn sdk_err<E: std::error::Error + Send + Sync + 'static>(what: &str, e: SdkError<E, aws_sdk_s3::config::http::HttpResponse>) -> Fail {
    let status = e.raw_response().map(|r| r.status().as_u16());
    let text = format!("{what}: {}", aws_sdk_s3::error::DisplayErrorContext(&e));
    match status {
        // The target refuses (no such bucket, access denied, invalid): retrying won't help.
        Some(s) if (400..500).contains(&s) && !matches!(s, 408 | 409 | 429) => Fail::Permanent(text),
        _ => Fail::Retry(text),
    }
}

/// A body of known length for the SDK, computing the CRC32C of what it passes on.
struct Counted {
    inner: Mutex<ByteStream>,
    len: u64,
    seen: u64,
    /// The inner stream ended (or failed): it's never polled again.
    done: bool,
    crc: Arc<(AtomicU64, AtomicU32)>,
}

impl http_body::Body for Counted {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<http_body::Frame<Bytes>, std::io::Error>>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        let inner = this.inner.get_mut().unwrap_or_else(|e| e.into_inner());
        match Pin::new(inner).poll_next(cx) {
            Poll::Ready(Some(Ok(b))) => {
                this.seen += b.len() as u64;
                if this.seen > this.len {
                    this.done = true;
                    return Poll::Ready(Some(Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "the blob is longer than its metadata says"))));
                }
                let crc = crc32c::crc32c_append(this.crc.1.load(Ordering::SeqCst), &b);
                this.crc.1.store(crc, Ordering::SeqCst);
                this.crc.0.store(this.seen, Ordering::SeqCst);
                Poll::Ready(Some(Ok(http_body::Frame::data(b))))
            }
            Poll::Ready(Some(Err(e))) => {
                this.done = true;
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.done = true;
                if this.seen != this.len {
                    return Poll::Ready(Some(Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the blob ended early"))));
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::SizeHint::with_exact(self.len)
    }
}

/// `(body, (bytes seen, CRC32C))` for a stream of `len` bytes.
fn counted(stream: ByteStream, len: u64) -> (SdkStream, Arc<(AtomicU64, AtomicU32)>) {
    let crc = Arc::new((AtomicU64::new(0), AtomicU32::new(0)));
    let body = Counted { inner: Mutex::new(stream), len, seen: 0, done: false, crc: crc.clone() };
    (SdkStream::new(SdkBody::from_body_1_x(body)), crc)
}

fn crc_b64(crc: u32) -> String {
    STANDARD.encode(crc.to_be_bytes())
}

fn crc_of(b64: Option<&str>) -> Option<u32> {
    let b = STANDARD.decode(b64?).ok()?;
    Some(u32::from_be_bytes(b.as_slice().try_into().ok()?))
}

/// What the rule says about encryption on the target: `Some(Some(key))` re-encrypt with that
/// KMS key, `Some(None)` the target's default; `None`: an SSE-KMS version the rule doesn't
/// replicate.
fn kms_target(src: &ReplicaSource) -> Option<Option<String>> {
    let rule = src.rule.as_ref()?;
    let enabled = rule
        .source_selection
        .as_deref()
        .and_then(|x| s3_proto::xml::parse(x.as_bytes()).ok())
        .and_then(|n| n.child("SseKmsEncryptedObjects").and_then(|s| s.text_of("Status")).map(|s| s.trim() == "Enabled"))
        .unwrap_or(false);
    if !enabled {
        return None;
    }
    let key = rule
        .destination_extra
        .as_deref()
        .and_then(|x| s3_proto::xml::parse(format!("<D>{x}</D>").as_bytes()).ok())
        .and_then(|n| n.child("EncryptionConfiguration").and_then(|e| e.text_of("ReplicaKmsKeyID")).map(|k| k.trim().to_string()))
        .filter(|k| !k.is_empty());
    Some(key)
}

/// `x-amz-tagging` of a tag set.
fn tagging(tags: &BTreeMap<String, String>) -> Option<String> {
    if tags.is_empty() {
        return None;
    }
    Some(url::form_urlencoded::Serializer::new(String::new()).extend_pairs(tags.iter()).finish())
}

/// What every write of a replica carries.
struct Headers {
    meta: s3_store::ObjectMeta,
    storage_class: Option<StorageClass>,
    owner_full_control: bool,
    /// `Some(key)`: SSE-KMS on the target (with that key, or its default).
    kms: Option<Option<String>>,
}

macro_rules! object_headers {
    ($req:expr, $h:expr) => {{
        let h: &Headers = $h;
        let m = &h.meta.headers;
        let mut r = $req
            .set_content_type(m.content_type.clone())
            .set_cache_control(m.cache_control.clone())
            .set_content_disposition(m.content_disposition.clone())
            .set_content_encoding(m.content_encoding.clone())
            .set_content_language(m.content_language.clone())
            .set_expires(m.expires.as_deref().and_then(|e| DateTime::from_str(e, DateTimeFormat::HttpDate).ok()))
            .set_metadata(Some(m.metadata.clone().into_iter().collect()))
            .set_tagging(tagging(&m.tags))
            .set_website_redirect_location(m.website_redirect.clone())
            .set_storage_class(h.storage_class.clone());
        if h.owner_full_control {
            r = r.acl(ObjectCannedAcl::BucketOwnerFullControl);
        }
        if let Some(k) = &h.kms {
            r = r.server_side_encryption(ServerSideEncryption::AwsKms).set_ssekms_key_id(k.clone());
        }
        r
    }};
}

impl External {
    pub(crate) fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            clients: Default::default(),
            latency: Default::default(),
            part_size: AtomicU64::new(16 << 20),
            multipart_threshold: AtomicU64::new(64 << 20),
            server_side: AtomicU32::new(0),
        }
    }

    /// Note a version replicated to `dest` (inside roll or not), `queued_at` when its write
    /// was recorded.
    pub(crate) fn record(&self, bucket_id: &str, dest: &str, queued_at: i64, now: i64) {
        let ms = (now - queued_at).max(0) as u64;
        let mut m = self.latency.lock().unwrap();
        let l = m.entry((bucket_id.to_string(), dest.to_string())).or_default();
        l.last_ms = ms;
        l.max_ms = l.max_ms.max(ms);
        l.replicated += 1;
        l.last_at = now;
    }

    /// A bucket's replication latency by destination key.
    pub(crate) fn latency(&self, bucket_id: &str) -> BTreeMap<String, Latency> {
        self.latency.lock().unwrap().iter().filter(|((b, _), _)| b == bucket_id).map(|((_, d), l)| (d.clone(), l.clone())).collect()
    }

    /// A client for a target (cached until the target changes).
    pub(crate) async fn client(&self, t: &ReplicationTarget) -> Client {
        let mut clients = self.clients.lock().await;
        if let Some((at, c)) = clients.get(&t.name)
            && *at == t.updated_at
        {
            return c.clone();
        }
        let mut b = match (&t.access_key_id, &t.secret) {
            (Some(k), Some(s)) => aws_sdk_s3::Config::builder().behavior_version(BehaviorVersion::latest()).credentials_provider(Credentials::new(k, s.expose(), None, None, "roll-replication-target")),
            _ => aws_sdk_s3::config::Builder::from(&aws_config::defaults(BehaviorVersion::latest()).load().await),
        };
        b = b.region(Region::new(t.region.clone())).force_path_style(t.path_style);
        if let Some(e) = &t.endpoint {
            b = b.endpoint_url(e);
        }
        let c = Client::from_conf(b.build());
        clients.insert(t.name.clone(), (t.updated_at, c.clone()));
        c
    }

    /// A replication rule may name `bucket` on target `name`: the target exists and the bucket
    /// has versioning enabled there (as S3 requires of a destination).
    pub(crate) async fn check_destination(&self, name: &str, bucket: &str) -> Result<(), String> {
        let t = self.store.replication_target(name).map_err(|e| e.to_string())?.ok_or_else(|| format!("no replication target {name}"))?;
        let client = self.client(&t).await;
        let out = client.get_bucket_versioning().bucket(bucket).send().await.map_err(|e| format!("Destination bucket {bucket} on the replication target {name} can't be checked: {}", aws_sdk_s3::error::DisplayErrorContext(e)))?;
        if out.status() != Some(&aws_sdk_s3::types::BucketVersioningStatus::Enabled) {
            return Err(format!("Destination bucket must have versioning enabled: {bucket} on the replication target {name}"));
        }
        Ok(())
    }

    /// Handle one replication row for a bucket outside roll. `Err`: try again later.
    pub(crate) async fn replicate(&self, n: &Notifier, ev: &PendingEvent) -> Result<Replicated, String> {
        let Some(src) = self.store.replica_source(ev).map_err(|e| e.to_string())? else {
            return self.store.finish_external_replication(ev, None).await.map_err(|e| e.to_string());
        };
        let target_name = src.task.target.clone().unwrap_or_default();
        let outcome = match self.store.replication_target(&target_name).map_err(|e| e.to_string())? {
            None => Some(Err(format!("the replication target {target_name} doesn't exist"))),
            Some(t) => match self.copy(&t, &src).await {
                Ok(None) => None,
                Ok(Some(())) => Some(Ok(())),
                Err(Fail::Permanent(why)) => Some(Err(why)),
                Err(Fail::Retry(why)) => return Err(why),
            },
        };
        let ok = matches!(outcome, Some(Ok(())));
        let r = self.store.finish_external_replication(ev, outcome).await.map_err(|e| e.to_string())?;
        if ok {
            self.record(&ev.bucket_id, &src.task.dest_key(), ev.created_at, self.store.now());
        }
        let _ = n;
        Ok(r)
    }

    /// Put the version on the target. `Ok(None)`: out of scope after all (SSE-KMS without
    /// `SseKmsEncryptedObjects`).
    async fn copy(&self, t: &ReplicationTarget, src: &ReplicaSource) -> Result<Option<()>, Fail> {
        let client = self.client(t).await;
        let bucket = &src.task.destination;
        let key = &src.meta.key;
        if src.delete_marker {
            client.delete_object().bucket(bucket).key(key).send().await.map_err(|e| sdk_err("DeleteObject on the target", e))?;
            return Ok(Some(()));
        }
        let kms = match &src.encryption {
            Some(Encryption::Customer { .. }) => return Err(Fail::Permanent("an SSE-C object can't be replicated outside roll: roll never keeps customer keys".into())),
            Some(Encryption::Kms(_)) => match kms_target(src) {
                None => return Ok(None),
                Some(k) => Some(k),
            },
            None => None,
        };
        let rule = src.rule.as_ref();
        let h = Headers {
            meta: src.meta.clone(),
            storage_class: rule.and_then(|r| r.storage_class.clone()).map(|c| StorageClass::from(c.as_str())),
            owner_full_control: src.task.owner_override,
            kms,
        };
        let blob_key = s3_store::blob_key(&src.meta.blob_id);
        let backend = self.store.backend().s3_location(&blob_key);
        let same_service = t.access_key_id.is_none() && backend.as_ref().is_some_and(|b| b.endpoint.as_deref().map(|e| e.trim_end_matches('/')) == t.endpoint.as_deref().map(|e| e.trim_end_matches('/')));
        match backend {
            Some(b) if same_service => self.server_side(&client, &b, bucket, &h, src).await?,
            _ => self.streamed(&client, bucket, &h, src).await?,
        }
        Ok(Some(()))
    }

    /// `CopyObject` (or `UploadPartCopy`) from the blob on the same service.
    async fn server_side(&self, client: &Client, from: &s3_store::S3Location, bucket: &str, h: &Headers, src: &ReplicaSource) -> Result<(), Fail> {
        let source = copy_source(&from.bucket, &from.key);
        let size = src.meta.size;
        if size <= MAX_SINGLE_COPY {
            let req = client.copy_object().bucket(bucket).key(&src.meta.key).copy_source(&source).metadata_directive(MetadataDirective::Replace).tagging_directive(TaggingDirective::Replace).checksum_algorithm(ChecksumAlgorithm::Crc32C);
            let out = object_headers!(req, h).send().await.map_err(|e| sdk_err("CopyObject to the target", e))?;
            let crc = crc_of(out.copy_object_result().and_then(|r| r.checksum_crc32_c()));
            let crc = match crc {
                Some(c) => c,
                None => {
                    let head = client.head_object().bucket(bucket).key(&src.meta.key).checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled).send().await.map_err(|e| sdk_err("HeadObject on the target", e))?;
                    crc_of(head.checksum_crc32_c()).ok_or_else(|| Fail::Retry("the target didn't report the copy's CRC32C".into()))?
                }
            };
            if crc != src.crc32c {
                return Err(Fail::Retry(format!("the copy on the target has CRC32C {crc:08x}, not the source's {:08x}", src.crc32c)));
            }
        } else {
            let part = self.part_size(size);
            let req = client.create_multipart_upload().bucket(bucket).key(&src.meta.key).checksum_algorithm(ChecksumAlgorithm::Crc32C);
            let up = object_headers!(req, h).send().await.map_err(|e| sdk_err("CreateMultipartUpload on the target", e))?;
            let id = up.upload_id().unwrap_or_default().to_string();
            let r: Result<(), Fail> = async {
                let (mut parts, mut crc, mut at, mut n) = (Vec::new(), 0u32, 0u64, 1i32);
                while at < size {
                    let end = (at + part).min(size);
                    let out = client
                        .upload_part_copy()
                        .bucket(bucket)
                        .key(&src.meta.key)
                        .upload_id(&id)
                        .part_number(n)
                        .copy_source(&source)
                        .copy_source_range(format!("bytes={at}-{}", end - 1))
                        .send()
                        .await
                        .map_err(|e| sdk_err("UploadPartCopy on the target", e))?;
                    let res = out.copy_part_result().ok_or_else(|| Fail::Retry("no CopyPartResult".into()))?;
                    let pc = crc_of(res.checksum_crc32_c()).ok_or_else(|| Fail::Retry("the target didn't report a part's CRC32C".into()))?;
                    crc = crc32c::crc32c_combine(crc, pc, (end - at) as usize);
                    parts.push(CompletedPart::builder().part_number(n).set_e_tag(res.e_tag().map(str::to_string)).checksum_crc32_c(crc_b64(pc)).build());
                    at = end;
                    n += 1;
                }
                if crc != src.crc32c {
                    return Err(Fail::Retry(format!("the copy's parts have CRC32C {crc:08x}, not the source's {:08x}", src.crc32c)));
                }
                client.complete_multipart_upload().bucket(bucket).key(&src.meta.key).upload_id(&id).multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build()).send().await.map_err(|e| sdk_err("CompleteMultipartUpload on the target", e))?;
                Ok(())
            }
            .await;
            if r.is_err() {
                let _ = client.abort_multipart_upload().bucket(bucket).key(&src.meta.key).upload_id(&id).send().await;
            }
            r?;
        }
        self.server_side.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn part_size(&self, size: u64) -> u64 {
        self.part_size.load(Ordering::Relaxed).max(size.div_ceil(10_000)).max(5 << 20)
    }

    /// Read the blob and write it to the target, part by part when it's large.
    async fn streamed(&self, client: &Client, bucket: &str, h: &Headers, src: &ReplicaSource) -> Result<(), Fail> {
        let size = src.meta.size;
        let key = &src.meta.key;
        if size < self.multipart_threshold.load(Ordering::Relaxed) {
            let (body, crc) = self.open(src, None).await?;
            let req = client.put_object().bucket(bucket).key(key).content_length(size as i64).checksum_crc32_c(crc_b64(src.crc32c)).body(body);
            object_headers!(req, h).send().await.map_err(|e| sdk_err("PutObject to the target", e))?;
            let (seen, got) = (crc.0.load(Ordering::SeqCst), crc.1.load(Ordering::SeqCst));
            if seen != size || got != src.crc32c {
                // The target checked the header against what it received; this is the blob
                // itself disagreeing with its metadata.
                tracing::error!(blob = src.meta.blob_id, "a blob doesn't match its CRC32C while replicating");
                return Err(Fail::Retry("the source blob doesn't match its CRC32C".into()));
            }
            return Ok(());
        }
        let part = self.part_size(size);
        let req = client.create_multipart_upload().bucket(bucket).key(key).checksum_algorithm(ChecksumAlgorithm::Crc32C);
        let up = object_headers!(req, h).send().await.map_err(|e| sdk_err("CreateMultipartUpload on the target", e))?;
        let id = up.upload_id().unwrap_or_default().to_string();
        let r: Result<(), Fail> = async {
            let (mut parts, mut crc, mut at, mut n) = (Vec::new(), 0u32, 0u64, 1i32);
            while at < size {
                let end = (at + part).min(size);
                let mut attempt = 0;
                let (etag, pc) = loop {
                    attempt += 1;
                    match self.upload_part(client, bucket, key, &id, n, src, at..end).await {
                        Ok(p) => break p,
                        Err(Fail::Retry(why)) if attempt < PART_ATTEMPTS => tracing::debug!(part = n, why, "replicating a part failed; retrying it"),
                        Err(e) => return Err(e),
                    }
                };
                crc = crc32c::crc32c_combine(crc, pc, (end - at) as usize);
                parts.push(CompletedPart::builder().part_number(n).set_e_tag(etag).checksum_crc32_c(crc_b64(pc)).build());
                at = end;
                n += 1;
            }
            if crc != src.crc32c {
                tracing::error!(blob = src.meta.blob_id, "a blob doesn't match its CRC32C while replicating");
                return Err(Fail::Retry(format!("the parts have CRC32C {crc:08x}, not the source's {:08x}", src.crc32c)));
            }
            client.complete_multipart_upload().bucket(bucket).key(key).upload_id(&id).multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build()).send().await.map_err(|e| sdk_err("CompleteMultipartUpload on the target", e))?;
            Ok(())
        }
        .await;
        if r.is_err() {
            let _ = client.abort_multipart_upload().bucket(bucket).key(key).upload_id(&id).send().await;
        }
        r
    }

    /// One part: a ranged read of the blob, uploaded with its CRC32C (which the target
    /// verifies). Returns its ETag and CRC32C.
    #[allow(clippy::too_many_arguments)]
    async fn upload_part(&self, client: &Client, bucket: &str, key: &str, id: &str, n: i32, src: &ReplicaSource, range: std::ops::Range<u64>) -> Result<(Option<String>, u32), Fail> {
        let len = range.end - range.start;
        let (body, crc) = self.open(src, Some(range)).await?;
        let out = client.upload_part().bucket(bucket).key(key).upload_id(id).part_number(n).content_length(len as i64).checksum_algorithm(ChecksumAlgorithm::Crc32C).body(body).send().await.map_err(|e| sdk_err("UploadPart on the target", e))?;
        let (seen, local) = (crc.0.load(Ordering::SeqCst), crc.1.load(Ordering::SeqCst));
        if seen != len {
            return Err(Fail::Retry(format!("part {n}: read {seen} of {len} bytes")));
        }
        if let Some(remote) = crc_of(out.checksum_crc32_c())
            && remote != local
        {
            return Err(Fail::Retry(format!("part {n}: the target stored CRC32C {remote:08x}, not {local:08x}")));
        }
        Ok((out.e_tag().map(str::to_string), local))
    }

    async fn open(&self, src: &ReplicaSource, range: Option<std::ops::Range<u64>>) -> Result<(SdkStream, Arc<(AtomicU64, AtomicU32)>), Fail> {
        let len = range.as_ref().map_or(src.meta.size, |r| r.end - r.start);
        let (got, stream) = self.store.open_blob_sse(&src.meta, range, None).await.map_err(|e| Fail::Retry(format!("reading the source: {e}")))?;
        if got != len {
            return Err(Fail::Retry(format!("reading the source: {got} bytes, not {len}")));
        }
        Ok(counted(stream, len))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(crc_of(Some(&crc_b64(0xdeadbeef))), Some(0xdeadbeef));
        assert_eq!(crc_of(Some("x")), None);
        let mut tags = BTreeMap::new();
        assert_eq!(tagging(&tags), None);
        tags.insert("a b".to_string(), "c&d".to_string());
        assert_eq!(tagging(&tags).as_deref(), Some("a+b=c%26d"));
        assert_eq!(copy_source("b", "p/a b"), "b/p/a%20b");
    }
}
