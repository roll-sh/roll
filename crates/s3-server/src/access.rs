//! Authorization: may this request do this to this bucket or object? AWS's evaluation logic
//! (<https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_policies_evaluation-logic.html>,
//! and S3's for object operations), with roll's account keys standing in for IAM identities
//! through their permission levels:
//!
//! 1. An explicit `Deny` anywhere refuses: the bucket policy, the requester's identity policies,
//!    its permissions boundary, its session policies. A session that was revoked or whose role
//!    is gone is refused everything.
//! 2. The requester's own side ([`crate::identity::Identity::verdict`]): its identity policies,
//!    its roll permission level (account keys and control-plane roles), or being the account's
//!    root user, *bounded* by its permissions boundary and session policies.
//! 3. The resource's side: the bucket policy, ACL grants (when Object Ownership enables ACLs),
//!    the `public` flag. They name the requester directly (or anyone), its role, or its account
//!    ([`Via`]).
//! 4. **Same account** (the requester's account owns the resource): its own side allows, or a
//!    resource grant names it directly (not bounded), or names its role (bounded). A grant to the
//!    account alone delegates to the account's own policies.
//!    **Another account**: both its own side and a resource grant must allow.
//!
//! The resource owner is the bucket's account, except for object reads and ACL changes on an
//! object another account owns (ACLs on, the object written by another account): the object's
//! owner, whose ACL is then the only resource grant (the bucket owner's policy can still deny,
//! not grant), as on S3.
//!
//! The bucket owner's root user (and its admin keys covering the bucket) can always read,
//! replace and delete the bucket policy, so a policy can't lock its owner out (as on AWS).
//!
//! **Requester Pays**: on a bucket whose payer is `Requester`, a request from another account
//! must say `x-amz-request-payer: requester` (else `AccessDenied`, anonymous requests too), and
//! its request and egress charges go to the requester's account; the bucket owner pays for its
//! own account's requests.

use crate::acl::{self, Acl, Ownership};
use crate::auth::Need;
use crate::ctx::Ctx;
use crate::error::StoreResult;
use crate::identity::Identity;
use crate::policy::{Context, Policy, Principal, Via};
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
    /// The payer is `Requester`.
    pub requester_pays: bool,
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
        let requester_pays = settings.get(kind::REQUEST_PAYMENT).is_some_and(|p| p == "Requester");
        Ok(Self { policy_public: policy.as_ref().is_some_and(Policy::is_public), bucket, settings, policy, block, ownership, acls_enabled, bucket_acl, requester_pays })
    }

    /// Whether anonymous users can read through the `public` flag or a public policy (what
    /// `GetBucketPolicyStatus` reports).
    pub fn is_public(&self) -> bool {
        self.policy_public || self.bucket.public
    }

    /// The bucket owner's account id.
    pub fn owner(&self) -> &str {
        &self.bucket.account_id
    }
}

/// Per-request cache of [`Access`] by bucket name.
#[derive(Default)]
pub(crate) struct AccessCache(std::sync::Mutex<HashMap<String, Arc<Access>>>);

/// Who a request comes from.
#[derive(Clone, Copy)]
pub(crate) enum Who<'a> {
    Anonymous,
    Key(&'a AccessKey, &'a Identity),
}

impl<'a> Who<'a> {
    pub fn identity(&self) -> Option<&'a Identity> {
        match self {
            Who::Key(_, id) => Some(id),
            Who::Anonymous => None,
        }
    }

    /// The requester's account.
    pub fn account(&self) -> Option<&'a str> {
        self.identity().map(|i| i.account.id.as_str())
    }

    pub(crate) fn principal(&self) -> &'a Principal {
        static ANON: Principal = Principal::Anonymous;
        match self {
            Who::Key(_, id) => &id.principal,
            Who::Anonymous => &ANON,
        }
    }

    /// An account key whose bucket scope leaves `bucket` out: it learns nothing about it.
    fn scoped_out(&self, bucket: &str) -> bool {
        matches!(self, Who::Key(k, id) if id.kind == crate::identity::Kind::Key && !k.covers(bucket))
    }
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
            Some(a) => Who::Key(&a.key, &a.identity),
            None => Who::Anonymous,
        }
    }

    /// The bucket isn't to be revealed to the requester (an account key scoped to other
    /// buckets): `AccessDenied` rather than `NoSuchBucket`.
    pub(crate) fn hides(&self, bucket: &str) -> bool {
        self.who().scoped_out(bucket)
    }

    /// For operations no resource policy governs (CreateBucket, ListBuckets): whether the
    /// requester's own side allows `action` (on `bucket`, or every bucket when `None`).
    /// `level`: whether its roll permission level does.
    pub(crate) fn require_identity(&self, action: &str, bucket: Option<&str>, level: bool) -> Result<(), S3Error> {
        let Who::Key(_, id) = self.who() else { return Err(S3Error::new(Code::AccessDenied)) };
        let b = bucket.unwrap_or("*");
        let cond = Cond { ctx: self, who: self.who(), bucket: bucket.unwrap_or_default(), key: None, resource_account: id.account.id.clone(), tags: OnceLock::new(), prefix: None };
        if id.verdict(action, &resource_arn(b, None), &cond, level).allowed() { Ok(()) } else { Err(S3Error::new(Code::AccessDenied)) }
    }
}

pub(crate) fn resource_arn(bucket: &str, key: Option<&str>) -> String {
    match key {
        Some(k) => format!("arn:aws:s3:::{bucket}/{k}"),
        None => format!("arn:aws:s3:::{bucket}"),
    }
}

/// What a roll permission level allows (the compatibility layer, equivalent to an attached
/// policy: `read` = `s3:Get*`/`s3:List*`, `write` adds the object writes, `admin` = `s3:*`, on
/// the key's buckets).
pub(crate) fn key_allows(k: &AccessKey, bucket: &str, need: Need) -> bool {
    match need {
        Need::Read => k.can_read(bucket),
        Need::Write => k.can_write(bucket),
        Need::Admin => k.is_admin() && k.covers(bucket),
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

/// Operations on an existing object that its owner (not the bucket's) governs.
fn object_owned_action(action: &str) -> bool {
    action.starts_with("s3:GetObject") || matches!(action, "s3:PutObjectAcl" | "s3:PutObjectVersionAcl" | "s3:PutObjectTagging" | "s3:PutObjectVersionTagging" | "s3:DeleteObjectTagging" | "s3:DeleteObjectVersionTagging")
}

/// May `who` do `action` (an IAM action name) on `bucket` (and `key`)? `need` is the roll
/// permission level that allows it.
pub(crate) fn authorize(ctx: &Ctx, who: Who, bucket: &str, key: Option<&str>, action: &str, need: Need) -> Result<(), S3Error> {
    authorize_as(ctx, who, bucket, key, action, need, None)
}

/// Whether `who` may list `bucket` with `prefix` (`s3:ListBucket`, the policy's `s3:prefix`
/// being `prefix`): S3 tells such a requester that a key it can't read doesn't exist (404)
/// rather than 403, since a listing would show that anyway.
pub(crate) fn may_list_prefix(ctx: &Ctx, who: Who, bucket: &str, prefix: &str) -> bool {
    authorize_as(ctx, who, bucket, None, "s3:ListBucket", Need::Read, Some(prefix)).is_ok()
}

/// Requester Pays: a request from another account must accept the charges, which then go to
/// it.
fn requester_pays(ctx: &Ctx, who: Who, a: &Access) -> Result<(), S3Error> {
    if !a.requester_pays || who.account() == Some(a.owner()) {
        return Ok(());
    }
    let accepted = ctx.h("x-amz-request-payer").is_some_and(|v| v.trim().eq_ignore_ascii_case("requester"));
    match who.account() {
        Some(payer) if accepted => {
            ctx.charge(&a.bucket.id, payer);
            Ok(())
        }
        _ => Err(S3Error::msg(Code::AccessDenied, "Access Denied: this bucket is Requester Pays; send x-amz-request-payer: requester to accept the charges.")),
    }
}

fn authorize_as(ctx: &Ctx, who: Who, bucket: &str, key: Option<&str>, action: &str, need: Need, prefix: Option<&str>) -> Result<(), S3Error> {
    let denied = || S3Error::new(Code::AccessDenied);
    let a = match ctx.access(bucket) {
        Ok(a) => a,
        // An account key scoped to other buckets learns nothing about this one.
        Err(e) if e.code == Code::NoSuchBucket => return Err(if who.scoped_out(bucket) { denied() } else { e }),
        Err(e) => return Err(e),
    };
    // Through an access point: its policy and the bucket's (`accesspoint/mod.rs`).
    if let Some(r) = crate::accesspoint::authorize(ctx, who, &a, bucket, key, action, need, prefix) {
        return r;
    }
    // The bucket owner can always manage the bucket's policy.
    if let Who::Key(k, id) = who
        && id.account.id == a.owner()
        && !id.dead
        && (id.is_root() || (id.kind == crate::identity::Kind::Key && k.is_admin() && k.covers(bucket)))
        && matches!(action, "s3:GetBucketPolicy" | "s3:PutBucketPolicy" | "s3:DeleteBucketPolicy" | "s3:GetBucketPolicyStatus")
    {
        return Ok(());
    }
    requester_pays(ctx, who, &a)?;
    // The object's owner governs reading it and its ACL when it isn't the bucket's.
    let mut resource_owner = a.owner().to_string();
    let mut object_acl: Option<Acl> = None;
    if a.acls_enabled
        && object_owned_action(action)
        && let Some(k) = key
        && let Ok(m) = ctx.store().get_object_version(bucket, k, ctx.q("versionId").filter(|_| ctx.key.as_deref() == Some(k)))
    {
        let acl = acl::object_acl(true, m.headers.acl.as_deref());
        if let Some(o) = &acl.owner {
            resource_owner = o.clone();
        }
        object_acl = Some(acl);
    }
    let resource = resource_arn(bucket, key);
    let cond = Cond { ctx, who, bucket, key, resource_account: a.owner().to_string(), tags: OnceLock::new(), prefix };
    let principal = who.principal();
    let bucket_policy = a.policy.as_ref().map(|p| p.evaluate(principal, action, &resource, &cond)).unwrap_or_default();
    if bucket_policy.deny {
        return Err(denied());
    }
    let own = match who {
        Who::Key(k, id) => Some(id.verdict(action, &resource, &cond, key_allows(k, bucket, need))),
        Who::Anonymous => None,
    };
    if own.is_some_and(|v| v.deny) {
        return Err(denied());
    }
    // The resource's grants.
    let same_account_as_bucket = who.account() == Some(a.owner());
    let restricted = a.policy_public && a.block.restrict_public_buckets && !same_account_as_bucket;
    let mut grant: Option<Via> = None;
    if resource_owner == a.owner() && !restricted {
        grant = grant.max(bucket_policy.allow);
    }
    if a.acls_enabled
        && let Some((on_object, perm)) = acl_permission(action)
    {
        let ignore = a.block.ignore_public_acls;
        let g = match (on_object, key) {
            (true, Some(_)) => object_acl.as_ref().and_then(|acl| acl.grants(principal, perm, ignore, &resource_owner)),
            (true, None) => None,
            _ => a.bucket_acl.grants(principal, perm, ignore, a.owner()),
        };
        grant = grant.max(g);
    }
    if a.bucket.public && action == "s3:GetObject" && !a.block.restrict_public_buckets && resource_owner == a.owner() {
        grant = Some(Via::Direct);
    }
    let allowed = match (who, own) {
        (Who::Key(_, id), Some(v)) => {
            if id.account.id == resource_owner {
                v.allowed() || grant == Some(Via::Direct) || (grant == Some(Via::Role) && v.bounded)
            } else {
                v.allowed() && grant.is_some()
            }
        }
        _ => grant == Some(Via::Direct),
    };
    if !allowed {
        return Err(denied());
    }
    // As on S3, only the bucket owner's account may read or change its policy, whatever a
    // policy grants another account (405, not 403).
    if matches!(action, "s3:GetBucketPolicy" | "s3:PutBucketPolicy" | "s3:DeleteBucketPolicy") && who.account() != Some(a.owner()) {
        return Err(S3Error::msg(Code::MethodNotAllowed, "The bucket policy can only be managed by the bucket owner's account."));
    }
    Ok(())
}

/// Condition keys of a request.
pub(crate) struct Cond<'a> {
    pub ctx: &'a Ctx,
    pub who: Who<'a>,
    pub bucket: &'a str,
    pub key: Option<&'a str>,
    /// The bucket owner's account (`aws:ResourceAccount`, `s3:ResourceAccount`).
    pub resource_account: String,
    /// The object's current tags, read on first use.
    pub tags: OnceLock<Option<BTreeMap<String, String>>>,
    /// `s3:prefix` when it isn't the request's `prefix` parameter.
    pub prefix: Option<&'a str>,
}

impl Cond<'_> {
    fn request_tags(&self) -> Option<Vec<(String, String)>> {
        self.ctx.h("x-amz-tagging").map(|t| s3_proto::http::parse_query(t.trim()))
    }
}

impl Context for Cond<'_> {
    fn values(&self, key: &str) -> Option<Vec<String>> {
        let ctx = self.ctx;
        if let Some(id) = self.who.identity()
            && let Some(v) = id.condition(key, ctx.store().now())
        {
            return v;
        }
        if let Some(v) = crate::accesspoint::condition(ctx, key) {
            return v;
        }
        let one = |v: Option<String>| v.map(|v| vec![v]);
        let header = |n: &str| one(ctx.h(n).map(str::to_string));
        match key {
            "aws:sourceip" => one(ctx.peer.map(|p| p.to_string())),
            "aws:securetransport" => one(Some(secure(ctx).to_string())),
            "aws:currenttime" => one(Some(s3_proto::time::iso8601_ms(ctx.store().now()))),
            "aws:epochtime" => one(Some(ctx.now_s().to_string())),
            "aws:referer" => header("referer"),
            "aws:useragent" => header("user-agent"),
            "aws:principaltype" => one(Some("Anonymous".into())),
            "aws:resourceaccount" | "s3:resourceaccount" => one(Some(self.resource_account.clone())),
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
