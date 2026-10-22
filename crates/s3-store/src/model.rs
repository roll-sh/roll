//! What the store hands out.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize)]
pub struct Bucket {
    pub id: String,
    pub name: String,
    pub region: String,
    /// Unix ms.
    pub created_at: i64,
    /// Anonymous GET/HEAD of objects.
    pub public: bool,
    /// Deleted and overwritten objects stay restorable this long.
    pub recover_days: u32,
    pub cors: Vec<CorsRule>,
    pub lifecycle: Vec<LifecycleRule>,
    pub parent: Option<ParentRef>,
    /// Names of the buckets branched from this one.
    pub branches: Vec<String>,
    /// Own (non-inherited) objects and their bytes: what the bucket is billed for.
    pub objects: u64,
    pub bytes: u64,
    /// Bytes in the trash (restorable, also billed).
    pub trash_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ParentRef {
    pub id: String,
    pub name: String,
    /// When the branch was made (Unix ms).
    pub at: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CorsRule {
    pub id: Option<String>,
    pub allowed_origins: Vec<String>,
    pub allowed_methods: Vec<String>,
    pub allowed_headers: Vec<String>,
    pub expose_headers: Vec<String>,
    pub max_age_s: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LifecycleRule {
    pub id: String,
    /// Applies to keys starting with this.
    pub prefix: String,
    pub enabled: bool,
    /// Delete (to the trash) objects last modified this many days ago.
    pub expire_days: Option<u32>,
    /// Abort multipart uploads started this many days ago.
    pub abort_multipart_days: Option<u32>,
}

/// Settings at creation.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct BucketOptions {
    pub public: Option<bool>,
    pub recover_days: Option<u32>,
}

/// Settings to change; `None` leaves a setting as it is. `cors: Some(vec![])` removes CORS.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct BucketUpdate {
    pub public: Option<bool>,
    pub recover_days: Option<u32>,
    pub cors: Option<Vec<CorsRule>>,
    pub lifecycle: Option<Vec<LifecycleRule>>,
}

/// Standard headers stored with an object, its user metadata (`x-amz-meta-*`, names
/// lowercase without the prefix) and tags.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectHeaders {
    pub content_type: Option<String>,
    pub cache_control: Option<String>,
    pub content_disposition: Option<String>,
    pub content_encoding: Option<String>,
    pub content_language: Option<String>,
    pub expires: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub tags: BTreeMap<String, String>,
    /// The object's ACL grants (opaque JSON owned by `s3-server`); `None` = the owner has full
    /// control and nobody else has anything (S3's `private`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acl: Option<String>,
    /// `x-amz-website-redirect-location`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website_redirect: Option<String>,
}

/// An `x-amz-checksum-*` value: `algorithm` is `CRC32`, `CRC32C`, `CRC64NVME`, `SHA1` or
/// `SHA256`; `kind` is `FULL_OBJECT` or `COMPOSITE`; `value` as S3 shows it (base64, with `-N`
/// for composite multipart checksums).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checksum {
    pub algorithm: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ObjectMeta {
    pub key: String,
    #[serde(skip)]
    pub blob_id: String,
    pub size: u64,
    /// Without quotes; `<md5 hex>` or `<hex>-<parts>`.
    pub etag: String,
    #[serde(flatten)]
    pub headers: ObjectHeaders,
    pub checksum: Option<Checksum>,
    /// 0 for a single PUT, else the number of parts.
    pub parts: u32,
    /// Sizes of the parts (for `GET ?partNumber=`).
    pub part_sizes: Vec<u64>,
    /// Last-Modified, Unix ms.
    pub modified_at: i64,
}

/// A new object version whose blob is already uploaded.
#[derive(Clone, Debug)]
pub struct NewObject {
    pub blob_id: String,
    pub size: u64,
    pub crc32c: u32,
    pub etag: String,
    pub headers: ObjectHeaders,
    pub checksum: Option<Checksum>,
}

/// Conditions checked atomically with a write (`If-Match` / `If-None-Match`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteCondition {
    /// The current object must have this ETag (`*` = must exist).
    pub if_match: Option<String>,
    /// `*`: the object must not exist. An ETag: the current object must not have it.
    pub if_none_match: Option<String>,
}

impl WriteCondition {
    pub fn none() -> Self {
        Self::default()
    }
}

/// How `copy_object` treats the destination's headers.
#[derive(Clone, Debug, Default)]
pub struct CopyOptions {
    /// `REPLACE`: these headers (and metadata). `None`: the source's (`COPY`).
    pub headers: Option<ObjectHeaders>,
    /// Tags for the copy (`x-amz-tagging-directive: REPLACE`); `None` copies the source's.
    pub tags: Option<BTreeMap<String, String>>,
    /// The source must have this ETag (`x-amz-copy-source-if-match`).
    pub source_etag: Option<String>,
    pub condition: WriteCondition,
}

#[derive(Clone, Debug, Default)]
pub struct ListQuery {
    pub prefix: String,
    pub delimiter: Option<String>,
    /// Start after this key, or after the whole common prefix if it is one.
    pub start_after: Option<String>,
    pub max_keys: usize,
}

#[derive(Clone, Debug, Default)]
pub struct ListPage {
    pub objects: Vec<ObjectMeta>,
    pub prefixes: Vec<String>,
    pub truncated: bool,
    /// The last key or prefix returned: pass as `start_after` for the next page.
    pub next: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Upload {
    pub id: String,
    pub bucket_id: String,
    pub key: String,
    #[serde(skip)]
    pub blob_id: String,
    #[serde(skip)]
    pub backend_upload_id: String,
    pub headers: ObjectHeaders,
    pub checksum_algorithm: Option<String>,
    pub checksum_type: Option<String>,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Part {
    pub part_number: u32,
    pub size: u64,
    /// Without quotes (an MD5 hex for uploaded parts).
    pub etag: String,
    pub checksum: Option<String>,
    #[serde(skip)]
    pub backend_etag: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Default)]
pub struct UploadsQuery {
    pub prefix: String,
    pub delimiter: Option<String>,
    pub key_marker: Option<String>,
    pub upload_id_marker: Option<String>,
    pub max_uploads: usize,
}

#[derive(Clone, Debug, Default)]
pub struct UploadsPage {
    pub uploads: Vec<Upload>,
    pub prefixes: Vec<String>,
    pub truncated: bool,
    pub next_key_marker: Option<String>,
    pub next_upload_id_marker: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TrashEntry {
    pub key: String,
    pub size: u64,
    pub etag: String,
    pub deleted_at: i64,
    /// When it's gone for good (Unix ms).
    pub expires_at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    /// GET, HEAD, List*.
    Read,
    /// Read, plus PUT, DELETE, copy and multipart.
    Write,
    /// Write, plus creating and deleting buckets and their configuration.
    Admin,
}

impl Permission {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct AccessKey {
    /// The access key id.
    pub id: String,
    #[serde(skip)]
    pub secret: String,
    pub name: String,
    pub permission: Permission,
    /// Bucket names it's limited to; `None` = all.
    pub buckets: Option<Vec<String>>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

impl AccessKey {
    /// Whether the key may see `bucket` at all.
    pub fn covers(&self, bucket: &str) -> bool {
        self.buckets.as_ref().is_none_or(|b| b.iter().any(|n| n == bucket))
    }

    pub fn can_read(&self, bucket: &str) -> bool {
        self.covers(bucket)
    }

    pub fn can_write(&self, bucket: &str) -> bool {
        self.covers(bucket) && self.permission != Permission::Read
    }

    pub fn is_admin(&self) -> bool {
        self.permission == Permission::Admin
    }
}

/// One bucket's metered day.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct UsageDay {
    pub bucket_id: String,
    /// `YYYY-MM-DD` (UTC).
    pub day: String,
    pub class_a: u64,
    pub class_b: u64,
    pub bytes_in: u64,
    pub egress_internet: u64,
    pub egress_internal: u64,
    /// Stored bytes (own objects plus trash) × milliseconds: / (1e9 × ms per month) = GB-months.
    /// A float: a petabyte-day overflows 64-bit integers.
    pub byte_ms: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SweepStats {
    pub trash_expired: u64,
    pub blobs_deleted: u64,
    pub blob_bytes_deleted: u64,
    pub uploads_aborted: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct OrphanStats {
    pub scanned: u64,
    /// Unknown blobs recorded this scan, to delete on a later one.
    pub candidates: u64,
    pub deleted: u64,
    pub deleted_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrubReport {
    pub last_at: Option<i64>,
    pub checked: u64,
    /// Referenced blobs missing from object storage.
    pub missing: u64,
    /// Blobs whose size in object storage differs from the metadata.
    pub mismatched: u64,
    /// A few of the problem blob ids, for the logs.
    pub examples: Vec<String>,
}
