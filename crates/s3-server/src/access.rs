//! Authorization: may this request do this to this bucket or object?
//!
//! In order (S3's evaluation, with roll keys standing in for IAM identities):
//!
//! 1. A bucket policy `Deny` that matches refuses the request, whoever asks.
//! 2. The key's own permission (`read`/`write`/`admin`, scoped to buckets) allows it.
//! 3. A bucket policy `Allow` that matches allows it, even for anonymous requests or keys
//!    without that permission. With `RestrictPublicBuckets`, a public policy only allows roll
//!    keys.
//! 4. ACL grants allow it, when the bucket's Object Ownership enables ACLs. With
//!    `IgnorePublicAcls`, grants to `AllUsers`/`AuthenticatedUsers` count for nothing.
//! 5. The bucket's `public` flag allows anonymous `GetObject` (it counts as a public policy,
//!    so `RestrictPublicBuckets` turns it off).
//!
//! Admin keys covering a bucket can always read, write and delete its policy, like an AWS
//! account's root user, so a policy can't lock its owner out.
//!
//! Temporary credentials are bounded first: whatever allows a request, a session's policies
//! (its role's inline policy, its session policy) must allow it too (see `temporary.rs`).

use crate::acl::{self, Acl, Ownership};
use crate::auth::Need;
use crate::ctx::Ctx;
use crate::error::StoreResult;
use crate::policy::{Account, Context, Decision, Policy, Principal};
use axum::http::Method;
use s3_proto::{Code, S3Error};
use s3_store::config::kind;
use s3_store::{AccessKey, Bucket, PublicAccessBlock};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};

/// What decides access to one bucket.
pub(crate) struct Access {
    pub bucket: Bucket,
    pub settings: HashMap<String, String>,
    pub policy: Option<Policy>,
    pub policy_public: bool,
    /// In force: the bucket's own block with the deployment's on top.
    pub block: PublicAccessBlock,
    /// `None`: the ownership controls were deleted.
    pub ownership: Option<Ownership>,
    pub acls_enabled: bool,
    pub bucket_acl: Acl,
}

impl Access {
    fn load(ctx: &Ctx, bucket: Bucket) -> Result<Self, S3Error> {
        let store = ctx.store();
        let settings = store.bucket_settings(&bucket.id).s3()?;
        // A stored policy was valid when it was put; if it no longer parses, deny rather than
        // silently drop it.
        let policy = match settings.get(kind::POLICY) {
            Some(p) => Some(Policy::load(p.as_bytes()).map_err(|e| {
                tracing::error!(bucket = bucket.name, error = %e, "a stored bucket policy doesn't parse");
                S3Error::new(Code::AccessDenied)
            })?),
            None => None,
        };
        let own: PublicAccessBlock = settings.get(kind::PUBLIC_ACCESS_BLOCK).and_then(|v| serde_json::from_str(v).ok()).unwrap_or_default();
        let block = if store.cfg.block_public_access { own.or(PublicAccessBlock::ALL) } else { own };
        let ownership = Ownership::from_setting(settings.get(kind::OWNERSHIP).map(String::as_str));
        let acls_enabled = ownership != Some(Ownership::BucketOwnerEnforced);
        let bucket_acl = acl::bucket_acl(settings.get(kind::ACL).map(String::as_str));
        Ok(Self { policy_public: policy.as_ref().is_some_and(Policy::is_public), bucket, settings, policy, block, ownership, acls_enabled, bucket_acl })
    }

    /// Whether anonymous users can read through the `public` flag or a public policy (what
    /// `GetBucketPolicyStatus` reports).
    pub fn is_public(&self) -> bool {
        self.policy_public || self.bucket.public
    }
}

/// Per-request cache of [`Access`] by bucket name.
#[derive(Default)]
pub(crate) struct AccessCache(std::sync::Mutex<HashMap<String, Arc<Access>>>);

/// Who a request comes from.
#[derive(Clone, Copy)]
pub(crate) enum Who<'a> {
    Anonymous,
    Key(&'a AccessKey),
}

impl Ctx {
    /// The access rules of `bucket` (cached for the request). `NoSuchBucket` if it doesn't
    /// exist.
    pub(crate) fn access(&self, bucket: &str) -> Result<Arc<Access>, S3Error> {
        if let Some(a) = self.access_cache.0.lock().unwrap().get(bucket) {
            return Ok(a.clone());
        }
        let b = self.store().bucket(bucket).s3()?;
        let a = Arc::new(Access::load(self, b)?);
        self.access_cache.0.lock().unwrap().insert(bucket.to_string(), a.clone());
        Ok(a)
    }

    /// Forget cached access rules (after this request changed them).
    pub(crate) fn forget_access(&self, bucket: &str) {
        self.access_cache.0.lock().unwrap().remove(bucket);
    }

    pub(crate) fn who(&self) -> Who<'_> {
        match &self.auth {
            Some(a) => Who::Key(&a.key),
            None => Who::Anonymous,
        }
    }
}

fn principal(ctx: &Ctx, who: Who) -> Principal {
    match who {
        Who::Anonymous => Principal::Anonymous,
        Who::Key(k) => crate::temporary::principal(ctx.store(), k),
    }
}

fn resource_arn(bucket: &str, key: Option<&str>) -> String {
    match key {
        Some(k) => format!("arn:aws:s3:::{bucket}/{k}"),
        None => format!("arn:aws:s3:::{bucket}"),
    }
}

impl Ctx {
    /// For checks outside [`authorize`] (CreateBucket, ListBuckets, bypassing governance
    /// retention): refuse if the request's session policies don't allow `action` on the bucket
    /// (or object), or on every bucket when `bucket` is `None`.
    pub(crate) fn require_session(&self, action: &str, bucket: Option<&str>, key: Option<&str>) -> Result<(), S3Error> {
        let Some(k) = self.auth.as_ref().map(|a| &a.key).filter(|k| k.is_temporary()) else { return Ok(()) };
        let account = Account { id: self.store().account_id(), owner: self.store().owner_id() };
        let cond = Cond { ctx: self, who: Who::Key(k), bucket: bucket.unwrap_or_default(), key, account: &account, tags: OnceLock::new(), prefix: None };
        let resource = resource_arn(bucket.unwrap_or("*"), key);
        if crate::temporary::bounded(k, &account, action, &resource, &cond) { Ok(()) } else { Err(S3Error::new(Code::AccessDenied)) }
    }
}

fn key_allows(who: Who, bucket: &str, need: Need) -> bool {
    match who {
        Who::Anonymous => false,
        Who::Key(k) => match need {
            Need::Read => k.can_read(bucket),
            Need::Write => k.can_write(bucket),
            Need::Admin => k.is_admin() && k.covers(bucket),
        },
    }
}

/// The ACL permission an action needs, and whether it's checked on the object (else the
/// bucket).
fn acl_permission(action: &str) -> Option<(bool, &'static str)> {
    Some(match action {
        "s3:GetObject" | "s3:GetObjectVersion" => (true, "READ"),
        "s3:GetObjectAcl" | "s3:GetObjectVersionAcl" => (true, "READ_ACP"),
        "s3:PutObjectAcl" | "s3:PutObjectVersionAcl" => (true, "WRITE_ACP"),
        "s3:ListBucket" | "s3:ListBucketVersions" | "s3:ListBucketMultipartUploads" => (false, "READ"),
        "s3:PutObject" | "s3:DeleteObject" | "s3:AbortMultipartUpload" => (false, "WRITE"),
        "s3:GetBucketAcl" => (false, "READ_ACP"),
        "s3:PutBucketAcl" => (false, "WRITE_ACP"),
        _ => return None,
    })
}

/// May `who` do `action` (an IAM action name) on `bucket` (and `key`)? `need` is the key
/// permission that allows it without any policy or ACL.
pub(crate) fn authorize(ctx: &Ctx, who: Who, bucket: &str, key: Option<&str>, action: &str, need: Need) -> Result<(), S3Error> {
    authorize_as(ctx, who, bucket, key, action, need, None)
}

/// Whether `who` may list `bucket` with `prefix` (`s3:ListBucket`, the policy's `s3:prefix`
/// being `prefix`): S3 tells such a requester that a key it can't read doesn't exist (404)
/// rather than 403, since a listing would show that anyway.
pub(crate) fn may_list_prefix(ctx: &Ctx, who: Who, bucket: &str, prefix: &str) -> bool {
    authorize_as(ctx, who, bucket, None, "s3:ListBucket", Need::Read, Some(prefix)).is_ok()
}

fn authorize_as(ctx: &Ctx, who: Who, bucket: &str, key: Option<&str>, action: &str, need: Need, prefix: Option<&str>) -> Result<(), S3Error> {
    let denied = || S3Error::new(Code::AccessDenied);
    let a = match ctx.access(bucket) {
        Ok(a) => a,
        // A key that can't see the bucket learns nothing about it.
        Err(e) if e.code == Code::NoSuchBucket => {
            return Err(match who {
                Who::Key(k) if k.covers(bucket) => e,
                Who::Anonymous => e,
                _ => denied(),
            });
        }
        Err(e) => return Err(e),
    };
    if let Who::Key(k) = who
        && k.is_temporary()
    {
        let account = Account { id: ctx.store().account_id(), owner: ctx.store().owner_id() };
        let cond = Cond { ctx, who, bucket, key, account: &account, tags: OnceLock::new(), prefix };
        if !crate::temporary::bounded(k, &account, action, &resource_arn(bucket, key), &cond) {
            return Err(denied());
        }
    }
    if let Who::Key(k) = who
        && k.is_admin()
        && k.covers(bucket)
        && matches!(action, "s3:GetBucketPolicy" | "s3:PutBucketPolicy" | "s3:DeleteBucketPolicy" | "s3:GetBucketPolicyStatus")
    {
        return Ok(());
    }
    let p = OnceLock::new();
    let p = || p.get_or_init(|| principal(ctx, who));
    let decision = match &a.policy {
        Some(policy) => {
            let p = p();
            let account = Account { id: ctx.store().account_id(), owner: ctx.store().owner_id() };
            let resource = resource_arn(bucket, key);
            let cond = Cond { ctx, who, bucket, key, account: &account, tags: OnceLock::new(), prefix };
            policy.evaluate(p, &account, action, &resource, &cond)
        }
        None => Decision::None,
    };
    if decision == Decision::Deny {
        return Err(denied());
    }
    if key_allows(who, bucket, need) {
        return Ok(());
    }
    if decision == Decision::Allow && !(matches!(who, Who::Anonymous) && a.policy_public && a.block.restrict_public_buckets) {
        return Ok(());
    }
    if a.acls_enabled
        && let Some((on_object, perm)) = acl_permission(action)
    {
        let ignore = a.block.ignore_public_acls;
        let granted = match (on_object, key) {
            (true, Some(k)) => match ctx.store().get_object(bucket, k) {
                Ok(m) => acl::object_acl(true, m.headers.acl.as_deref()).allows(p(), perm, ignore),
                Err(_) => false,
            },
            _ => a.bucket_acl.allows(p(), perm, ignore),
        };
        if granted {
            return Ok(());
        }
    }
    if a.bucket.public && matches!(action, "s3:GetObject") && !a.block.restrict_public_buckets {
        return Ok(());
    }
    Err(denied())
}

/// Condition keys of a request.
struct Cond<'a> {
    ctx: &'a Ctx,
    who: Who<'a>,
    bucket: &'a str,
    key: Option<&'a str>,
    account: &'a Account,
    /// The object's current tags, read on first use.
    tags: OnceLock<Option<BTreeMap<String, String>>>,
    /// `s3:prefix` when it isn't the request's `prefix` parameter.
    prefix: Option<&'a str>,
}

impl Cond<'_> {
    fn request_tags(&self) -> Option<Vec<(String, String)>> {
        self.ctx.h("x-amz-tagging").map(|t| s3_proto::http::parse_query(t.trim()))
    }
}

impl Context for Cond<'_> {
    fn values(&self, key: &str) -> Option<Vec<String>> {
        let ctx = self.ctx;
        if let Who::Key(k) = self.who
            && let Some(v) = crate::temporary::condition(k, &self.account.id, key)
        {
            return v;
        }
        let one = |v: Option<String>| v.map(|v| vec![v]);
        let header = |n: &str| one(ctx.h(n).map(str::to_string));
        let key_id = match self.who {
            Who::Key(k) => Some(k.id.clone()),
            Who::Anonymous => None,
        };
        match key {
            "aws:sourceip" => one(ctx.peer.map(|p| p.to_string())),
            "aws:securetransport" => one(Some(secure(ctx).to_string())),
            "aws:currenttime" => one(Some(s3_proto::time::iso8601_ms(ctx.store().now()))),
            "aws:epochtime" => one(Some(ctx.now_s().to_string())),
            "aws:referer" => header("referer"),
            "aws:useragent" => header("user-agent"),
            "aws:username" | "aws:userid" => one(key_id),
            "aws:principalaccount" => one(key_id.map(|_| self.account.id.clone())),
            "aws:principalarn" => one(key_id.map(|k| format!("arn:aws:iam::{}:user/{k}", self.account.id))),
            "aws:principaltype" => one(Some(if key_id.is_some() { "User" } else { "Anonymous" }.into())),
            "s3:prefix" if self.prefix.is_some() => one(self.prefix.map(str::to_string)),
            "s3:prefix" | "s3:delimiter" | "s3:max-keys" => one(ctx.q(&key[3..]).map(str::to_string)),
            "s3:versionid" => one(ctx.q("versionId").map(str::to_string)),
            "s3:signatureversion" => one(ctx.auth.as_ref().map(|_| "AWS4-HMAC-SHA256".to_string())),
            "s3:authtype" => one(ctx.auth.as_ref().map(|_| if ctx.query.iter().any(|(k, _)| k.eq_ignore_ascii_case("X-Amz-Signature")) { "REST-QUERY-STRING" } else { "REST-HEADER" }.to_string())),
            "s3:requestobjecttagkeys" => self.request_tags().map(|t| t.into_iter().map(|(k, _)| k).collect()),
            k if k.starts_with("s3:requestobjecttag/") => {
                let name = &k["s3:requestobjecttag/".len()..];
                self.request_tags()?.into_iter().find(|(t, _)| t.eq_ignore_ascii_case(name)).map(|(_, v)| vec![v])
            }
            k if k.starts_with("s3:existingobjecttag/") => {
                let name = &k["s3:existingobjecttag/".len()..];
                let tags = self.tags.get_or_init(|| self.key.and_then(|key| ctx.store().get_tags(self.bucket, key).ok()));
                tags.as_ref()?.iter().find(|(t, _)| t.eq_ignore_ascii_case(name)).map(|(_, v)| vec![v.clone()])
            }
            k if k.starts_with("s3:x-amz-") => header(&k[3..]),
            _ => None,
        }
    }
}

/// HTTPS as seen by a TLS-terminating proxy inside roll (`X-Forwarded-Proto: https` from an
/// internal peer). roll itself serves plain HTTP.
pub(crate) fn secure(ctx: &Ctx) -> bool {
    ctx.internal() && ctx.h("x-forwarded-proto").is_some_and(|p| p.trim().eq_ignore_ascii_case("https"))
}

/// The IAM action of the request (what a policy's `Action` names).
pub(crate) fn action_of(ctx: &Ctx) -> &'static str {
    let m = &ctx.method;
    let read = *m == Method::GET || *m == Method::HEAD;
    let del = *m == Method::DELETE;
    if ctx.bucket.is_none() {
        return "s3:ListAllMyBuckets";
    }
    if ctx.key.is_some() {
        let versioned = ctx.has("versionId");
        return match () {
            _ if ctx.has("tagging") => {
                if read {
                    if versioned { "s3:GetObjectVersionTagging" } else { "s3:GetObjectTagging" }
                } else if del {
                    "s3:DeleteObjectTagging"
                } else {
                    "s3:PutObjectTagging"
                }
            }
            _ if ctx.has("acl") => {
                if read {
                    if versioned { "s3:GetObjectVersionAcl" } else { "s3:GetObjectAcl" }
                } else {
                    "s3:PutObjectAcl"
                }
            }
            _ if ctx.has("retention") => if read { "s3:GetObjectRetention" } else { "s3:PutObjectRetention" },
            _ if ctx.has("legal-hold") => if read { "s3:GetObjectLegalHold" } else { "s3:PutObjectLegalHold" },
            _ if ctx.has("attributes") => if versioned { "s3:GetObjectVersionAttributes" } else { "s3:GetObjectAttributes" },
            _ if ctx.has("restore") => "s3:RestoreObject",
            _ if ctx.has("torrent") => "s3:GetObjectTorrent",
            _ if ctx.has("uploadId") && read => "s3:ListMultipartUploadParts",
            _ if ctx.has("uploadId") && del => "s3:AbortMultipartUpload",
            _ if read => if versioned { "s3:GetObjectVersion" } else { "s3:GetObject" },
            _ if del => if versioned { "s3:DeleteObjectVersion" } else { "s3:DeleteObject" },
            _ => "s3:PutObject",
        };
    }
    // (subresource, read, write, delete)
    const SUBS: &[(&str, &str, &str, &str)] = &[
        ("policyStatus", "s3:GetBucketPolicyStatus", "s3:GetBucketPolicyStatus", "s3:GetBucketPolicyStatus"),
        ("policy", "s3:GetBucketPolicy", "s3:PutBucketPolicy", "s3:DeleteBucketPolicy"),
        ("acl", "s3:GetBucketAcl", "s3:PutBucketAcl", "s3:PutBucketAcl"),
        ("cors", "s3:GetBucketCORS", "s3:PutBucketCORS", "s3:PutBucketCORS"),
        ("lifecycle", "s3:GetLifecycleConfiguration", "s3:PutLifecycleConfiguration", "s3:PutLifecycleConfiguration"),
        ("tagging", "s3:GetBucketTagging", "s3:PutBucketTagging", "s3:PutBucketTagging"),
        ("website", "s3:GetBucketWebsite", "s3:PutBucketWebsite", "s3:DeleteBucketWebsite"),
        ("logging", "s3:GetBucketLogging", "s3:PutBucketLogging", "s3:PutBucketLogging"),
        ("versioning", "s3:GetBucketVersioning", "s3:PutBucketVersioning", "s3:PutBucketVersioning"),
        ("location", "s3:GetBucketLocation", "s3:GetBucketLocation", "s3:GetBucketLocation"),
        ("publicAccessBlock", "s3:GetBucketPublicAccessBlock", "s3:PutBucketPublicAccessBlock", "s3:PutBucketPublicAccessBlock"),
        ("ownershipControls", "s3:GetBucketOwnershipControls", "s3:PutBucketOwnershipControls", "s3:PutBucketOwnershipControls"),
        ("encryption", "s3:GetEncryptionConfiguration", "s3:PutEncryptionConfiguration", "s3:PutEncryptionConfiguration"),
        ("accelerate", "s3:GetAccelerateConfiguration", "s3:PutAccelerateConfiguration", "s3:PutAccelerateConfiguration"),
        ("requestPayment", "s3:GetBucketRequestPayment", "s3:PutBucketRequestPayment", "s3:PutBucketRequestPayment"),
        ("replication", "s3:GetReplicationConfiguration", "s3:PutReplicationConfiguration", "s3:PutReplicationConfiguration"),
        ("notification", "s3:GetBucketNotification", "s3:PutBucketNotification", "s3:PutBucketNotification"),
        ("analytics", "s3:GetAnalyticsConfiguration", "s3:PutAnalyticsConfiguration", "s3:PutAnalyticsConfiguration"),
        ("metrics", "s3:GetMetricsConfiguration", "s3:PutMetricsConfiguration", "s3:PutMetricsConfiguration"),
        ("inventory", "s3:GetInventoryConfiguration", "s3:PutInventoryConfiguration", "s3:PutInventoryConfiguration"),
        ("intelligent-tiering", "s3:GetIntelligentTieringConfiguration", "s3:PutIntelligentTieringConfiguration", "s3:PutIntelligentTieringConfiguration"),
        ("object-lock", "s3:GetBucketObjectLockConfiguration", "s3:PutBucketObjectLockConfiguration", "s3:PutBucketObjectLockConfiguration"),
        ("versions", "s3:ListBucketVersions", "s3:ListBucketVersions", "s3:ListBucketVersions"),
        ("uploads", "s3:ListBucketMultipartUploads", "s3:ListBucketMultipartUploads", "s3:ListBucketMultipartUploads"),
        ("delete", "s3:DeleteObject", "s3:DeleteObject", "s3:DeleteObject"),
    ];
    if let Some((_, r, w, d)) = SUBS.iter().find(|(s, ..)| ctx.has(s)) {
        return if read {
            r
        } else if del {
            d
        } else {
            w
        };
    }
    match *m {
        Method::PUT => "s3:CreateBucket",
        Method::DELETE => "s3:DeleteBucket",
        Method::POST => "s3:PutObject",
        _ => "s3:ListBucket",
    }
}
