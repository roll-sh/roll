//! S3 Express One Zone directory buckets (`<base>--<zone id>--x-s3`) on the S3 API.
//!
//! - **CreateBucket** with `CreateBucketConfiguration` `Location` (`AvailabilityZone` or
//!   `LocalZone`, any zone id: it must be the one in the name) and `Bucket` (`Directory`,
//!   `SingleAvailabilityZone` or `SingleLocalZone`). **ListDirectoryBuckets** (`GET
//!   /?x-id=ListDirectoryBuckets`, or `GET /` on the `s3express-control` endpoint),
//!   HeadBucket, DeleteBucket.
//! - **CreateSession** (`GET /<bucket>?session`): 5-minute credentials scoped to the bucket,
//!   `ReadWrite` or `ReadOnly`, a new kind of roll session token (`s3_store::express`). Requests
//!   carry it in `x-amz-s3session-token` and sign with its secret; it's accepted nowhere else.
//! - **What directory buckets don't have** answers `NotImplemented`, as S3's zonal endpoints do:
//!   versioning, Object Lock, ACLs, CORS, website, logging, replication, notifications,
//!   tagging, accelerate, request payment, analytics, metrics, intelligent-tiering, ownership
//!   and public access block settings, ListObjects (v1), ListObjectVersions, S3 Select,
//!   torrents, SSE-C and DSSE-KMS. ListObjectsV2 takes only `/` as its delimiter; its order is
//!   lexicographic (S3 promises no order for directory buckets, so that's one valid order).
//!   Multipart part numbers must be consecutive from 1.
//! - **Policies** of directory buckets use S3 Express's actions: every data operation is
//!   `s3express:CreateSession` (with `s3express:SessionMode` `ReadOnly` for reads, `ReadWrite`
//!   for writes, or the session's own mode), bucket administration `s3express:<operation>`,
//!   on the resource `arn:aws:s3express:<region>:<account>:bucket/<bucket>`.
//!
//! Underneath, a directory bucket is an ordinary roll bucket: one object, one blob; overwrites
//! and deletes go to the trash; branches work (see `s3_store::express`).

use crate::auth::Need;
use crate::ctx::Ctx;
use crate::error::StoreResult;
use crate::response::{self, Reply, set};
use axum::http::{Method, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use s3_proto::xml::{self, escape};
use s3_proto::{Code, S3Error, XMLNS};
use s3_store::express::{is_directory_bucket, zone_of};
use s3_store::{Bucket, BucketOptions, Class};

pub(crate) fn is_directory(bucket: &str) -> bool {
    is_directory_bucket(bucket)
}

fn unsupported() -> S3Error {
    S3Error::msg(Code::NotImplemented, "This operation is not supported by directory buckets.")
}

/// Bucket subresources directory buckets don't have.
const NO_BUCKET_SUBRESOURCES: &[&str] = &[
    "versioning",
    "versions",
    "object-lock",
    "cors",
    "website",
    "logging",
    "accelerate",
    "requestPayment",
    "replication",
    "notification",
    "analytics",
    "metrics",
    "intelligent-tiering",
    "tagging",
    "acl",
    "publicAccessBlock",
    "ownershipControls",
    "policyStatus",
    "location",
];

/// Object subresources directory buckets don't have.
const NO_OBJECT_SUBRESOURCES: &[&str] = &["tagging", "acl", "retention", "legal-hold", "restore", "torrent", "select"];

/// Refuse what a directory bucket doesn't support (bucket requests).
pub(crate) fn check_bucket_request(ctx: &Ctx) -> Result<(), S3Error> {
    if NO_BUCKET_SUBRESOURCES.iter().any(|s| ctx.has(s)) {
        return Err(unsupported());
    }
    if ctx.method == Method::GET && !ctx.has("session") && !ctx.has("uploads") && !ctx.has("policy") && !ctx.has("encryption") && !ctx.has("lifecycle") && !ctx.has("inventory") {
        // A listing: ListObjectsV2 only, and `/` the only delimiter.
        if ctx.q("list-type") != Some("2") {
            return Err(unsupported());
        }
        if ctx.q("delimiter").is_some_and(|d| !d.is_empty() && d != "/") {
            return Err(S3Error::msg(Code::InvalidArgument, "For directory buckets, / is the only supported delimiter.").with("ArgumentName", "delimiter"));
        }
    }
    Ok(())
}

/// Refuse what a directory bucket doesn't support (object requests).
pub(crate) fn check_object_request(ctx: &Ctx) -> Result<(), S3Error> {
    if NO_OBJECT_SUBRESOURCES.iter().any(|s| ctx.has(s)) {
        return Err(unsupported());
    }
    if ctx.q("versionId").is_some_and(|v| v != "null") {
        return Err(S3Error::msg(Code::InvalidArgument, "Directory buckets have no versions other than null.").with("ArgumentName", "versionId"));
    }
    if ctx.h("x-amz-server-side-encryption-customer-algorithm").is_some() || ctx.h("x-amz-copy-source-server-side-encryption-customer-algorithm").is_some() {
        return Err(S3Error::msg(Code::InvalidRequest, "Server-side encryption with customer-provided keys (SSE-C) isn't supported for directory buckets."));
    }
    if let Some(a) = ctx.h("x-amz-server-side-encryption")
        && !matches!(a.trim(), "AES256" | "aws:kms")
    {
        return Err(S3Error::msg(Code::InvalidArgument, format!("Server-side encryption {a} isn't supported for directory buckets: use AES256 or aws:kms.")).with("ArgumentName", "x-amz-server-side-encryption"));
    }
    for h in ["x-amz-object-lock-mode", "x-amz-object-lock-retain-until-date", "x-amz-object-lock-legal-hold", "x-amz-tagging", "x-amz-website-redirect-location"] {
        if ctx.h(h).is_some() {
            return Err(S3Error::msg(Code::NotImplemented, format!("{h} isn't supported for directory buckets.")).with("Header", h.to_string()));
        }
    }
    Ok(())
}

/// Multipart uploads to directory buckets number their parts 1, 2, 3, … with no gaps.
pub(crate) fn check_consecutive_parts(numbers: impl Iterator<Item = u32>) -> Result<(), S3Error> {
    for (i, n) in numbers.enumerate() {
        if n as usize != i + 1 {
            return Err(S3Error::msg(Code::InvalidPartOrder, "For directory buckets, part numbers must be consecutive, starting at 1."));
        }
    }
    Ok(())
}

/// Whether a CreateBucket is for a directory bucket: by its name, or by a configuration that
/// says `Directory` (refused below if the name isn't a directory bucket's).
pub(crate) fn wanted(name: &str, body: &[u8]) -> bool {
    is_directory(name) || (!body.is_empty() && xml::parse(body).ok().and_then(|n| n.child("Bucket").and_then(|b| b.text_of("Type").map(|t| t == "Directory"))).unwrap_or(false))
}

/// CreateBucket of a directory bucket (`raw`: the request body, already read).
pub(crate) async fn create(ctx: &mut Ctx, name: &str, raw: &[u8]) -> Reply {
    ctx.require_session("s3express:CreateBucket", Some(name), None)?;
    let invalid = |m: &str| S3Error::msg(Code::InvalidArgument, m.to_string());
    let Some(zone) = zone_of(name) else {
        return Err(S3Error::msg(Code::InvalidBucketName, "Directory bucket names end in --<zone id>--x-s3.").with("BucketName", name.to_string()));
    };
    if raw.is_empty() {
        return Err(S3Error::msg(Code::InvalidRequest, "Directory buckets need a CreateBucketConfiguration with Location and Bucket.").with("BucketName", name.to_string()));
    }
    let n = xml::parse(raw)?;
    if n.name != "CreateBucketConfiguration" {
        return Err(S3Error::new(Code::MalformedXML));
    }
    if n.child("LocationConstraint").is_some() {
        return Err(invalid("Directory buckets take a Location, not a LocationConstraint."));
    }
    let location = n.child("Location").ok_or_else(|| invalid("Location is required for directory buckets."))?;
    let bucket = n.child("Bucket").ok_or_else(|| invalid("Bucket is required for directory buckets."))?;
    let loc_type = location.text_of("Type").unwrap_or_default();
    let redundancy_for = match loc_type {
        "AvailabilityZone" => "SingleAvailabilityZone",
        "LocalZone" => "SingleLocalZone",
        _ => return Err(invalid("Location Type is AvailabilityZone or LocalZone.")),
    };
    if location.text_of("Name") != Some(zone) {
        return Err(S3Error::msg(Code::InvalidBucketName, format!("The bucket name's zone id ({zone}) must be the Location Name.")).with("BucketName", name.to_string()));
    }
    if bucket.text_of("Type") != Some("Directory") {
        return Err(invalid("Bucket Type must be Directory for a directory bucket."));
    }
    if bucket.text_of("DataRedundancy").is_some_and(|r| r != redundancy_for) {
        return Err(invalid(&format!("DataRedundancy must be {redundancy_for} for a {loc_type}.")));
    }
    if ctx.h("x-amz-bucket-object-lock-enabled").is_some_and(|v| v.trim().eq_ignore_ascii_case("true")) {
        return Err(invalid("Directory buckets don't support Object Lock."));
    }
    if ctx.h("x-amz-object-ownership").is_some_and(|o| o != "BucketOwnerEnforced") {
        return Err(invalid("Directory buckets are always BucketOwnerEnforced."));
    }
    let b = ctx.store().create_bucket(name, BucketOptions::default()).await.s3()?;
    ctx.meter(&b.id, Class::A);
    let mut r = response::empty(StatusCode::OK);
    set(&mut r, "location", &format!("/{name}"));
    Ok(r)
}

/// `arn:aws:s3express:<region>:<account>:bucket/<bucket>`.
pub(crate) fn bucket_arn(ctx: &Ctx, bucket: &str) -> String {
    format!("arn:aws:s3express:{}:{}:bucket/{bucket}", ctx.server.cfg.region, ctx.store().account_id())
}

/// Whether the request is signed (SigV4) for the `s3express` service. A bare `GET /` is
/// ListDirectoryBuckets when it is (botocore sends it so), ListBuckets otherwise. Only called
/// after the signature was verified, and the signature covers the credential scope.
pub(crate) fn signed_for_express(ctx: &Ctx) -> bool {
    let header = ctx.h("authorization").and_then(|a| a.split_once("Credential=")).map(|(_, rest)| rest.split([',', ' ']).next().unwrap_or_default());
    let credential = header.or_else(|| ctx.q("X-Amz-Credential"));
    credential.is_some_and(|c| c.split('/').nth(3) == Some("s3express"))
}

/// ListDirectoryBuckets.
pub(crate) fn list(ctx: &mut Ctx) -> Reply {
    let Some(a) = &ctx.auth else { return Err(S3Error::new(Code::AccessDenied)) };
    ctx.require_session("s3express:ListAllMyDirectoryBuckets", None, None)?;
    let invalid = |name: &str, msg: &str| S3Error::msg(Code::InvalidArgument, msg.to_string()).with("ArgumentName", name.to_string());
    let max = match ctx.q("max-directory-buckets") {
        None => 1000,
        Some(v) => v.parse::<usize>().ok().filter(|n| *n <= 1000).ok_or_else(|| invalid("max-directory-buckets", "max-directory-buckets must be an integer between 0 and 1000"))?,
    };
    let max = if max == 0 { 1000 } else { max };
    let after = match ctx.q("continuation-token").filter(|t| !t.is_empty()) {
        None => None,
        Some(t) => Some(URL_SAFE_NO_PAD.decode(t).ok().and_then(|b| String::from_utf8(b).ok()).ok_or_else(|| invalid("continuation-token", "The continuation token provided is incorrect"))?),
    };
    let mut buckets: Vec<Bucket> = ctx
        .store()
        .buckets()
        .s3()?
        .into_iter()
        .filter(|b| is_directory(&b.name) && a.key.covers(&b.name))
        .filter(|b| after.as_ref().is_none_or(|t| b.name.as_str() > t.as_str()))
        .collect();
    let next = if buckets.len() > max {
        buckets.truncate(max);
        buckets.last().map(|b| URL_SAFE_NO_PAD.encode(b.name.as_bytes()))
    } else {
        None
    };
    let mut x = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ListAllMyDirectoryBucketsResult xmlns=\"{XMLNS}\"><Buckets>");
    for b in &buckets {
        x.push_str(&format!(
            "<Bucket><Name>{}</Name><CreationDate>{}</CreationDate><BucketRegion>{}</BucketRegion><BucketArn>{}</BucketArn></Bucket>",
            escape(&b.name),
            s3_proto::time::iso8601_ms(b.created_at),
            escape(&ctx.server.cfg.region),
            escape(&bucket_arn(ctx, &b.name))
        ));
    }
    x.push_str("</Buckets>");
    if let Some(t) = next {
        x.push_str(&format!("<ContinuationToken>{t}</ContinuationToken>"));
    }
    x.push_str("</ListAllMyDirectoryBucketsResult>");
    response::ok_xml(x)
}

/// CreateSession.
///
/// SDKs pointed at a custom endpoint sign even CreateBucket with a session, so they ask for one
/// before the bucket exists: a key that may create the bucket (admin, covering the name) gets
/// one for a valid directory bucket name that doesn't exist yet. Anyone else gets `NoSuchBucket`.
pub(crate) async fn create_session(ctx: &mut Ctx, name: &str) -> Reply {
    if !is_directory(name) {
        return Err(S3Error::msg(Code::NotImplemented, "CreateSession is only supported for directory buckets."));
    }
    let Some(auth) = &ctx.auth else { return Err(S3Error::new(Code::AccessDenied)) };
    let bucket = match ctx.store().bucket(name) {
        Ok(b) => Some(b),
        Err(s3_store::StoreError::NoSuchBucket(_)) if auth.key.is_admin() && auth.key.covers(name) && s3_store::express::validate_name(name).is_ok() => None,
        Err(e) => {
            return Err(if auth.key.covers(name) { crate::error::store_error(e) } else { S3Error::new(Code::AccessDenied) });
        }
    };
    if auth.key.session.as_ref().is_some_and(|s| s.express.is_some()) {
        return Err(S3Error::msg(Code::AccessDenied, "Session credentials can't create sessions."));
    }
    let read_only = match ctx.h("x-amz-create-session-mode").map(str::trim) {
        Some("ReadOnly") => Some(true),
        Some("ReadWrite") => Some(false),
        None | Some("") => None,
        Some(m) => return Err(S3Error::msg(Code::InvalidArgument, format!("Invalid session mode {m}: ReadWrite or ReadOnly.")).with("ArgumentName", "x-amz-create-session-mode")),
    };
    // Encryption is the bucket's default (S3 Express doesn't let a session override it).
    if let Some(a) = ctx.h("x-amz-server-side-encryption")
        && !matches!(a.trim(), "AES256" | "aws:kms")
    {
        return Err(S3Error::msg(Code::InvalidArgument, format!("Server-side encryption {a} isn't supported for directory buckets.")).with("ArgumentName", "x-amz-server-side-encryption"));
    }
    // No mode: ReadWrite if allowed, else ReadOnly. A bucket that doesn't exist yet has no
    // policy: the admin key's own permission allows either.
    let read_only = match (&bucket, read_only) {
        (None, ro) => {
            ctx.require_session("s3express:CreateSession", Some(name), None)?;
            ro.unwrap_or(false)
        }
        (Some(_), Some(ro)) => {
            crate::access::authorize(ctx, ctx.who(), name, None, "s3express:CreateSession", if ro { Need::Read } else { Need::Write })?;
            ro
        }
        (Some(_), None) => match crate::access::authorize(ctx, ctx.who(), name, None, "s3express:CreateSession", Need::Write) {
            Ok(()) => false,
            Err(_) => {
                crate::access::authorize(ctx, ctx.who(), name, None, "s3express:CreateSession", Need::Read)?;
                true
            }
        },
    };
    if let Some(b) = &bucket {
        ctx.meter(&b.id, Class::B);
    }
    let key = ctx.auth.as_ref().map(|a| a.key.clone()).ok_or_else(|| S3Error::new(Code::AccessDenied))?;
    let creds = ctx.store().create_express_session(&key, name, read_only).await.s3()?;
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<CreateSessionResult xmlns=\"{XMLNS}\"><Credentials><SessionToken>{}</SessionToken><SecretAccessKey>{}</SecretAccessKey><AccessKeyId>{}</AccessKeyId><Expiration>{}</Expiration></Credentials></CreateSessionResult>",
        escape(&creds.session_token),
        escape(&creds.secret_access_key),
        escape(&creds.access_key_id),
        iso8601_s(creds.expiration)
    );
    let mut r = response::xml(StatusCode::OK, body);
    match bucket.as_ref().and_then(|b| crate::config::default_encryption(ctx, b)) {
        Some((alg, kms_key, _)) if alg == "aws:kms" => {
            set(&mut r, "x-amz-server-side-encryption", "aws:kms");
            if let Some(k) = kms_key {
                set(&mut r, "x-amz-server-side-encryption-aws-kms-key-id", &k);
            }
            set(&mut r, "x-amz-server-side-encryption-bucket-key-enabled", "true");
        }
        _ => set(&mut r, "x-amz-server-side-encryption", "AES256"),
    }
    Ok(r)
}

/// ISO 8601 to the second (`2024-01-01T00:00:00Z`), as STS and S3 Express write expirations.
fn iso8601_s(ms: i64) -> String {
    let s = s3_proto::time::iso8601_ms(ms);
    match s.split_once('.') {
        Some((head, _)) => format!("{head}Z"),
        None => s,
    }
}

/// The S3 Express action a directory bucket's policy names for an S3 action: bucket
/// administration keeps its name in the `s3express` namespace, every data operation is
/// `s3express:CreateSession`.
pub(crate) fn express_action(action: &str) -> &'static str {
    match action {
        "s3:CreateBucket" => "s3express:CreateBucket",
        "s3:DeleteBucket" => "s3express:DeleteBucket",
        "s3:GetBucketPolicy" => "s3express:GetBucketPolicy",
        "s3:PutBucketPolicy" => "s3express:PutBucketPolicy",
        "s3:DeleteBucketPolicy" => "s3express:DeleteBucketPolicy",
        "s3:GetEncryptionConfiguration" => "s3express:GetEncryptionConfiguration",
        "s3:PutEncryptionConfiguration" => "s3express:PutEncryptionConfiguration",
        "s3:GetLifecycleConfiguration" => "s3express:GetLifecycleConfiguration",
        "s3:PutLifecycleConfiguration" => "s3express:PutLifecycleConfiguration",
        "s3:GetInventoryConfiguration" => "s3express:GetInventoryConfiguration",
        "s3:PutInventoryConfiguration" => "s3express:PutInventoryConfiguration",
        "s3:ListAllMyBuckets" => "s3express:ListAllMyDirectoryBuckets",
        "s3express:CreateSession" => "s3express:CreateSession",
        _ => "s3express:CreateSession",
    }
}

/// The actions directory bucket policies may name (IAM's list for the `s3express` service).
pub(crate) const ACTIONS: &[&str] = &[
    "CreateAccessPoint",
    "CreateBucket",
    "CreateSession",
    "DeleteAccessPoint",
    "DeleteAccessPointPolicy",
    "DeleteAccessPointScope",
    "DeleteBucket",
    "DeleteBucketPolicy",
    "GetAccessPoint",
    "GetAccessPointPolicy",
    "GetAccessPointScope",
    "GetBucketPolicy",
    "GetEncryptionConfiguration",
    "GetInventoryConfiguration",
    "GetLifecycleConfiguration",
    "ListAccessPointsForDirectoryBuckets",
    "ListAllMyDirectoryBuckets",
    "PutAccessPointPolicy",
    "PutAccessPointScope",
    "PutBucketPolicy",
    "PutEncryptionConfiguration",
    "PutInventoryConfiguration",
    "PutLifecycleConfiguration",
    "TagResource",
    "UntagResource",
];

/// `s3express:SessionMode` for a request: its session's mode, or what the request needs.
pub(crate) fn session_mode(ctx: &Ctx, need: Need) -> &'static str {
    if let Some(x) = ctx.auth.as_ref().and_then(|a| a.key.session.as_ref()).and_then(|s| s.express.as_ref()) {
        return x.mode();
    }
    if need == Need::Read { "ReadOnly" } else { "ReadWrite" }
}

/// Whether a ListObjectsV2 entry's storage class is S3 Express's.
pub(crate) fn storage_class(bucket: &str) -> &'static str {
    if is_directory(bucket) { "EXPRESS_ONEZONE" } else { "STANDARD" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consecutive_parts() {
        assert!(check_consecutive_parts([1, 2, 3].into_iter()).is_ok());
        assert!(check_consecutive_parts([2, 3].into_iter()).is_err());
        assert!(check_consecutive_parts([1, 3].into_iter()).is_err());
    }

    #[test]
    fn actions() {
        assert_eq!(express_action("s3:GetObject"), "s3express:CreateSession");
        assert_eq!(express_action("s3:PutBucketPolicy"), "s3express:PutBucketPolicy");
        assert!(iso8601_s(0).ends_with("T00:00:00Z"), "{}", iso8601_s(0));
        assert!(wanted("plain", b"<CreateBucketConfiguration><Bucket><Type>Directory</Type></Bucket></CreateBucketConfiguration>"));
        assert!(!wanted("plain", b"<CreateBucketConfiguration><LocationConstraint>eu-west-1</LocationConstraint></CreateBucketConfiguration>"));
    }
}
