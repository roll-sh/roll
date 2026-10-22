//! ACLs and Object Ownership.
//!
//! **Ownership.** A bucket's `ObjectOwnership` is `BucketOwnerEnforced` unless set otherwise
//! (S3's default since 2023): ACLs are then disabled, the bucket owner owns every object, reads
//! of an ACL show the owner's full control, and writes carrying any ACL but
//! `bucket-owner-full-control` (or `private` from the bucket owner's own account, or grants to
//! the bucket owner only) fail with `AccessControlListNotSupported`. With `BucketOwnerPreferred`
//! an object written with `bucket-owner-full-control` is the bucket owner's and any other is its
//! writer's account's; with `ObjectWriter` (or no controls, after DeleteBucketOwnershipControls)
//! every object is its writer's. An object's owner is kept in its ACL document (S3's
//! `AccessControlPolicy` has an `Owner`), so every copy of the row carries it.
//!
//! **Who's who.** Owners are accounts (canonical user id, display name). Grants name a group
//! (`AllUsers`, `AuthenticatedUsers`, `LogDelivery`), an account (by canonical id, or by email
//! address: `emailAddress=…` resolves to the account with that email), the resource's owner, or
//! a roll account key (its canonical id `s3_store::config::canonical_user`, or its access key
//! id).
//!
//! **Enforcement** (`access.rs`): grants are resource grants. `READ` on an object lets the
//! grantee `GetObject` it, `READ` on a bucket lists it, `WRITE` on a bucket writes and deletes
//! objects, `READ_ACP`/`WRITE_ACP` read and write the ACL, `FULL_CONTROL` is all of these. A
//! grant to an account delegates to the account (its users need their own policies to allow
//! too), as on S3. `AllUsers` grants are anonymous access, unless the public access block
//! ignores public ACLs.

use crate::auth::Need;
use crate::ctx::{Ctx, read_small};
use crate::error::StoreResult;
use crate::policy::{Principal, Via};
use crate::response::{self, Reply};
use axum::body::Body;
use axum::http::StatusCode;
use s3_proto::xml::{self, Node};
use s3_proto::{Code, S3Error};
use s3_store::config::kind;
use s3_store::{Bucket, Class, Store};
use serde::{Deserialize, Serialize};

pub(crate) const ALL_USERS: &str = "http://acs.amazonaws.com/groups/global/AllUsers";
pub(crate) const AUTHENTICATED_USERS: &str = "http://acs.amazonaws.com/groups/global/AuthenticatedUsers";
pub(crate) const LOG_DELIVERY: &str = "http://acs.amazonaws.com/groups/s3/LogDelivery";
const PERMISSIONS: [&str; 5] = ["FULL_CONTROL", "READ", "WRITE", "READ_ACP", "WRITE_ACP"];
const MAX_GRANTS: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Grantee {
    /// The resource's owner (the bucket's, or the object's).
    Owner,
    /// An account, by its 12-digit id.
    Account { id: String },
    /// A roll account key (by access key id).
    Key { id: String },
    Group { uri: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Grant {
    pub grantee: Grantee,
    pub permission: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Acl {
    pub grants: Vec<Grant>,
    /// An object's owner when it isn't the bucket's (its writer's account).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

fn grant(grantee: Grantee, permission: &str) -> Grant {
    Grant { grantee, permission: permission.into() }
}

/// An ACL a request asks for, and the canned ACL it came from (if it did).
#[derive(Clone, Debug)]
pub(crate) struct Requested {
    pub acl: Acl,
    pub canned: Option<String>,
}

impl Acl {
    /// The owner has full control; nobody else has anything.
    pub fn private() -> Self {
        Self { grants: vec![grant(Grantee::Owner, "FULL_CONTROL")], owner: None }
    }

    /// A canned ACL (`x-amz-acl`). `bucket_owner` is the bucket's owner (what
    /// `bucket-owner-full-control` and `bucket-owner-read` grant to).
    pub fn canned(name: &str, bucket: bool, bucket_owner: &str) -> Result<Self, S3Error> {
        let mut acl = Self::private();
        let group = |uri: &str| Grantee::Group { uri: uri.into() };
        let owner = || Grantee::Account { id: bucket_owner.to_string() };
        match name.trim() {
            "private" | "aws-exec-read" => {}
            "bucket-owner-full-control" => acl.grants.push(grant(owner(), "FULL_CONTROL")),
            "bucket-owner-read" => acl.grants.push(grant(owner(), "READ")),
            "public-read" => acl.grants.push(grant(group(ALL_USERS), "READ")),
            "public-read-write" => {
                acl.grants.push(grant(group(ALL_USERS), "READ"));
                acl.grants.push(grant(group(ALL_USERS), "WRITE"));
            }
            "authenticated-read" => acl.grants.push(grant(group(AUTHENTICATED_USERS), "READ")),
            "log-delivery-write" if bucket => {
                acl.grants.push(grant(group(LOG_DELIVERY), "WRITE"));
                acl.grants.push(grant(group(LOG_DELIVERY), "READ_ACP"));
            }
            _ => return Err(S3Error::new(Code::InvalidArgument).with("ArgumentName", "x-amz-acl").with("ArgumentValue", name.to_string())),
        }
        Ok(acl)
    }

    fn from_json(s: Option<&str>) -> Self {
        s.and_then(|s| serde_json::from_str(s).ok()).unwrap_or_else(Self::private)
    }

    /// Grants to the resource's owner account are grants to `Owner`; duplicates go.
    pub fn normalize(&mut self, owner: &str) {
        let mut out: Vec<Grant> = Vec::with_capacity(self.grants.len());
        for mut g in std::mem::take(&mut self.grants) {
            if matches!(&g.grantee, Grantee::Account { id } if id == owner) {
                g.grantee = Grantee::Owner;
            }
            if !out.contains(&g) {
                out.push(g);
            }
        }
        self.grants = out;
    }

    /// Grants to everyone or every authenticated user (S3 counts both as public).
    pub fn is_public(&self) -> bool {
        self.grants.iter().any(|g| matches!(&g.grantee, Grantee::Group { uri } if uri == ALL_USERS || uri == AUTHENTICATED_USERS))
    }

    /// Only the owner has anything: what `private` means.
    pub fn owner_only(&self) -> bool {
        self.grants.iter().all(|g| g.grantee == Grantee::Owner)
    }

    /// How the ACL grants `who` the permission `perm` (`FULL_CONTROL` gives every one), if it
    /// does. `owner` is the resource owner's account.
    pub fn grants(&self, who: &Principal, perm: &str, ignore_public: bool, owner: &str) -> Option<Via> {
        let r = who.requester();
        let account = |id: &str| r.filter(|r| r.account == id).map(|r| if r.root { Via::Direct } else { Via::Account });
        self.grants
            .iter()
            .filter(|g| g.permission == perm || g.permission == "FULL_CONTROL")
            .filter_map(|g| match &g.grantee {
                Grantee::Group { uri } if uri == ALL_USERS => (!ignore_public).then_some(Via::Direct),
                Grantee::Group { uri } if uri == AUTHENTICATED_USERS => (!ignore_public && r.is_some()).then_some(Via::Direct),
                Grantee::Group { .. } => None,
                Grantee::Key { id } => r.filter(|r| r.direct.iter().any(|d| d == id)).map(|_| Via::Direct),
                Grantee::Account { id } => account(id),
                Grantee::Owner => account(owner),
            })
            .max()
    }

    /// What goes in the metadata: `None` for plain `private` of the bucket owner.
    pub fn stored(&self) -> Option<String> {
        if self.grants == Self::private().grants && self.owner.is_none() { None } else { serde_json::to_string(self).ok() }
    }
}

/// `ObjectOwnership`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ownership {
    BucketOwnerEnforced,
    BucketOwnerPreferred,
    ObjectWriter,
}

impl Ownership {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "BucketOwnerEnforced" => Some(Self::BucketOwnerEnforced),
            "BucketOwnerPreferred" => Some(Self::BucketOwnerPreferred),
            "ObjectWriter" => Some(Self::ObjectWriter),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::BucketOwnerEnforced => "BucketOwnerEnforced",
            Self::BucketOwnerPreferred => "BucketOwnerPreferred",
            Self::ObjectWriter => "ObjectWriter",
        }
    }

    /// From the stored setting: absent is the default (`BucketOwnerEnforced`); `""` means the
    /// controls were deleted (`None`: no controls, which behaves as `ObjectWriter`).
    pub fn from_setting(v: Option<&str>) -> Option<Self> {
        match v {
            None => Some(Self::BucketOwnerEnforced),
            Some(s) => Self::parse(s),
        }
    }
}

/// An account as an `<Owner>` element. An account that's gone shows its id.
pub(crate) fn account_owner(store: &Store, account: &str) -> xml::Owner {
    match store.account(account) {
        Ok(Some(a)) => xml::Owner { id: a.canonical, display_name: a.name },
        _ => xml::Owner { id: account.to_string(), display_name: account.to_string() },
    }
}

/// The bucket's owner, for `<Owner>` elements.
pub(crate) fn bucket_owner(store: &Store, bucket: &Bucket) -> xml::Owner {
    account_owner(store, &bucket.account_id)
}

/// The owner of an object (its ACL's, or the bucket's).
pub(crate) fn object_owner(store: &Store, bucket: &Bucket, acls_enabled: bool, stored: Option<&str>) -> xml::Owner {
    match object_acl(acls_enabled, stored).owner {
        Some(o) => account_owner(store, &o),
        None => bucket_owner(store, bucket),
    }
}

/// A canonical user id (an account's, or a roll key's), or an access key id, to a grantee.
fn resolve_id(store: &Store, id: &str) -> Result<Grantee, S3Error> {
    let id = id.trim();
    let invalid = || S3Error::msg(Code::InvalidArgument, "Invalid id").with("ArgumentName", "CanonicalUser/ID").with("ArgumentValue", id.to_string());
    if let Some(a) = store.account_by_canonical(id).s3()? {
        return Ok(Grantee::Account { id: a.id });
    }
    if let Some(k) = store.access_key(id).s3()?
        && k.user.is_none()
    {
        return Ok(Grantee::Key { id: k.id });
    }
    let lower = id.to_ascii_lowercase();
    let owner = store.owner_id();
    let keys = store.keys().s3()?;
    keys.iter().filter(|k| k.user.is_none()).find(|k| s3_store::config::canonical_user(&owner, &k.id) == lower).map(|k| Grantee::Key { id: k.id.clone() }).ok_or_else(invalid)
}

fn resolve_email(store: &Store, email: &str) -> Result<Grantee, S3Error> {
    match store.account_by_email(email).s3()? {
        Some(a) => Ok(Grantee::Account { id: a.id }),
        None => Err(S3Error::new(Code::UnresolvableGrantByEmailAddress).with("EmailAddress", email.trim().to_string())),
    }
}

fn resolve_group(uri: &str) -> Result<Grantee, S3Error> {
    match uri.trim() {
        u @ (ALL_USERS | AUTHENTICATED_USERS | LOG_DELIVERY) => Ok(Grantee::Group { uri: u.into() }),
        u => Err(S3Error::msg(Code::InvalidArgument, "Invalid group uri").with("ArgumentName", "Group/URI").with("ArgumentValue", u.to_string())),
    }
}

/// `x-amz-grant-*` headers: `id="…", uri="…", emailAddress="…"`.
fn grant_headers(ctx: &Ctx) -> Result<Option<Acl>, S3Error> {
    let mut grants = Vec::new();
    for (header, perm) in [("x-amz-grant-full-control", "FULL_CONTROL"), ("x-amz-grant-read", "READ"), ("x-amz-grant-write", "WRITE"), ("x-amz-grant-read-acp", "READ_ACP"), ("x-amz-grant-write-acp", "WRITE_ACP")] {
        let Some(v) = ctx.h(header) else { continue };
        for item in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let bad = || S3Error::msg(Code::InvalidArgument, format!("Invalid grant: {item}")).with("ArgumentName", header.to_string());
            let (t, val) = item.split_once('=').ok_or_else(bad)?;
            let val = val.trim().trim_matches('"');
            let grantee = match t.trim().to_ascii_lowercase().as_str() {
                "id" => resolve_id(ctx.store(), val)?,
                "uri" => resolve_group(val)?,
                "emailaddress" => resolve_email(ctx.store(), val)?,
                _ => return Err(bad()),
            };
            grants.push(grant(grantee, perm));
            if grants.len() > MAX_GRANTS {
                return Err(S3Error::msg(Code::MalformedACLError, format!("An ACL can have at most {MAX_GRANTS} grants")));
            }
        }
    }
    Ok((!grants.is_empty()).then_some(Acl { grants, owner: None }))
}

/// The ACL a request asks for with `x-amz-acl` or `x-amz-grant-*` (not both), if any, on a
/// bucket owned by `bucket_owner`.
pub(crate) fn from_headers(ctx: &Ctx, bucket: bool, bucket_owner: &str) -> Result<Option<Requested>, S3Error> {
    let grants = grant_headers(ctx)?;
    match (ctx.h("x-amz-acl"), grants) {
        (Some(_), Some(_)) => Err(S3Error::msg(Code::InvalidRequest, "Specifying both Canned ACLs and Header Grants is not allowed")),
        (Some(c), None) => Ok(Some(Requested { acl: Acl::canned(c, bucket, bucket_owner)?, canned: Some(c.trim().to_string()) })),
        (None, g) => Ok(g.map(|acl| Requested { acl, canned: None })),
    }
}

/// An `AccessControlPolicy` body for a resource owned by `owner`.
fn parse_policy(ctx: &Ctx, body: &[u8], owner: &str) -> Result<Acl, S3Error> {
    let malformed = || S3Error::new(Code::MalformedACLError);
    let n = xml::parse(body).map_err(|e| if e.code == Code::MalformedXML { malformed() } else { e })?;
    if n.name != "AccessControlPolicy" {
        return Err(malformed());
    }
    if let Some(id) = n.child("Owner").and_then(|o| o.text_of("ID")) {
        let is_owner = matches!(resolve_id(ctx.store(), id)?, Grantee::Account { id } if id == owner);
        if !is_owner {
            return Err(S3Error::msg(Code::AccessDenied, "The owner in the ACL must be the resource's owner"));
        }
    }
    let list = n.child("AccessControlList").ok_or_else(malformed)?;
    let mut grants = Vec::new();
    for g in list.all("Grant") {
        let grantee: &Node = g.child("Grantee").ok_or_else(malformed)?;
        let perm = g.text_of("Permission").map(str::trim).filter(|p| PERMISSIONS.contains(p)).ok_or_else(malformed)?;
        let who = if let Some(id) = grantee.text_of("ID") {
            resolve_id(ctx.store(), id)?
        } else if let Some(uri) = grantee.text_of("URI") {
            resolve_group(uri)?
        } else if let Some(email) = grantee.text_of("EmailAddress") {
            resolve_email(ctx.store(), email)?
        } else {
            return Err(malformed());
        };
        grants.push(grant(who, perm));
        if grants.len() > MAX_GRANTS {
            return Err(S3Error::msg(Code::MalformedACLError, format!("An ACL can have at most {MAX_GRANTS} grants")));
        }
    }
    Ok(Acl { grants, owner: None })
}

/// `AccessControlPolicy` XML of a resource owned by `owner` (an account id).
pub(crate) fn render(store: &Store, acl: &Acl, owner: &str) -> String {
    let o = account_owner(store, owner);
    let mut s = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<AccessControlPolicy xmlns=\"{}\"><Owner><ID>{}</ID><DisplayName>{}</DisplayName></Owner><AccessControlList>",
        s3_proto::XMLNS,
        o.id,
        xml::escape(&o.display_name)
    );
    for g in &acl.grants {
        s.push_str("<Grant><Grantee xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" ");
        let user = |s: &mut String, x: &xml::Owner| s.push_str(&format!("xsi:type=\"CanonicalUser\"><ID>{}</ID><DisplayName>{}</DisplayName>", x.id, xml::escape(&x.display_name)));
        match &g.grantee {
            Grantee::Owner => user(&mut s, &o),
            Grantee::Account { id } => user(&mut s, &account_owner(store, id)),
            Grantee::Key { id } => {
                let name = store.access_key(id).ok().flatten().map_or_else(|| id.clone(), |k| k.name);
                user(&mut s, &xml::Owner { id: s3_store::config::canonical_user(&store.owner_id(), id), display_name: name });
            }
            Grantee::Group { uri } => s.push_str(&format!("xsi:type=\"Group\"><URI>{}</URI>", xml::escape(uri))),
        }
        s.push_str(&format!("</Grantee><Permission>{}</Permission></Grant>", g.permission));
    }
    s.push_str("</AccessControlList></AccessControlPolicy>");
    s
}

/// With ACLs disabled (`BucketOwnerEnforced`), the only ACLs a write may carry: none,
/// `bucket-owner-full-control`, `private` from the bucket owner's account, or grants to the
/// bucket owner alone.
fn enforced_ok(ctx: &Ctx, bucket_owner: &str, r: &Requested) -> bool {
    let writer = ctx.who().account().unwrap_or(bucket_owner);
    match r.canned.as_deref() {
        Some("bucket-owner-full-control") => true,
        Some("private") => writer == bucket_owner,
        Some(_) => false,
        None => {
            let mut acl = r.acl.clone();
            acl.normalize(bucket_owner);
            acl.owner_only()
        }
    }
}

/// The ACL a write may set on `bucket` (or one of its objects, owned by `owner`): `None` for
/// the owner's `private`. Fails as S3 does when ACLs are disabled (`AccessControlListNotSupported`)
/// or the ACL is public and the public access block blocks public ACLs (`AccessDenied`).
fn check_write(ctx: &Ctx, bucket: &str, r: &Requested, owner: &str) -> Result<Option<String>, S3Error> {
    let a = ctx.access(bucket)?;
    if !a.acls_enabled {
        return if enforced_ok(ctx, a.owner(), r) { Ok(None) } else { Err(S3Error::new(Code::AccessControlListNotSupported)) };
    }
    let mut acl = r.acl.clone();
    acl.normalize(owner);
    acl.owner = (owner != a.owner()).then(|| owner.to_string());
    if acl.is_public() && a.block.block_public_acls {
        return Err(S3Error::msg(Code::AccessDenied, "Access Denied: the bucket's public access block blocks public ACLs"));
    }
    Ok(acl.stored())
}

/// The ACL (and so the owner) of a new object in `bucket` written by this request, asked for
/// with `requested` (`None`: private). Object Ownership decides the owner: the bucket's
/// (`BucketOwnerEnforced`, or `BucketOwnerPreferred` with `bucket-owner-full-control`), else the
/// writer's account.
pub(crate) fn new_object(ctx: &Ctx, bucket: &str, requested: Option<Requested>) -> Result<Option<String>, S3Error> {
    let a = ctx.access(bucket)?;
    let bucket_owner = a.owner().to_string();
    let writer = ctx.who().account().unwrap_or(&bucket_owner).to_string();
    let owner = match a.ownership {
        Some(Ownership::BucketOwnerEnforced) => bucket_owner.clone(),
        Some(Ownership::BucketOwnerPreferred) if requested.as_ref().and_then(|r| r.canned.as_deref()) == Some("bucket-owner-full-control") => bucket_owner.clone(),
        _ => writer,
    };
    let r = requested.unwrap_or(Requested { acl: Acl::private(), canned: None });
    check_write(ctx, bucket, &r, &owner)
}

/// The ACL for a new object from the request's headers.
pub(crate) fn for_new_object(ctx: &Ctx, bucket: &str) -> Result<Option<String>, S3Error> {
    let owner = ctx.access(bucket)?.owner().to_string();
    let requested = from_headers(ctx, false, &owner)?;
    new_object(ctx, bucket, requested)
}

/// The ACL for a new object from a canned ACL (a POST form's `acl` field).
pub(crate) fn for_new_object_canned(ctx: &Ctx, bucket: &str, canned: Option<&str>) -> Result<Option<String>, S3Error> {
    let owner = ctx.access(bucket)?.owner().to_string();
    let requested = canned.map(|c| Acl::canned(c, false, &owner).map(|acl| Requested { acl, canned: Some(c.trim().to_string()) })).transpose()?;
    new_object(ctx, bucket, requested)
}

/// The ACL a stored object effectively has.
pub(crate) fn object_acl(acls_enabled: bool, stored: Option<&str>) -> Acl {
    if acls_enabled { Acl::from_json(stored) } else { Acl::private() }
}

/// The bucket's own ACL, from its settings.
pub(crate) fn bucket_acl(stored: Option<&str>) -> Acl {
    Acl::from_json(stored)
}

pub(crate) fn get_bucket_acl(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Read)?;
    ctx.meter(&bucket.id, Class::B);
    let a = ctx.access(&bucket.name)?;
    let acl = if a.acls_enabled { a.bucket_acl.clone() } else { Acl::private() };
    response::ok_xml(render(ctx.store(), &acl, a.owner()))
}

/// The ACL of a Put*Acl request: headers or an `AccessControlPolicy` body, not both. `owner`
/// is the resource's owner.
async fn requested(ctx: &Ctx, body: Body, for_bucket: bool, bucket_owner: &str, owner: &str) -> Result<Requested, S3Error> {
    let raw = read_small(ctx, body, xml::MAX_XML_BYTES).await?;
    let from_body = !raw.iter().all(|b| b.is_ascii_whitespace());
    match (from_headers(ctx, for_bucket, bucket_owner)?, from_body) {
        (Some(_), true) => Err(S3Error::msg(Code::InvalidRequest, "Specify an ACL either in headers or in the request body, not both")),
        (Some(r), false) => Ok(r),
        (None, true) => Ok(Requested { acl: parse_policy(ctx, &raw, owner)?, canned: None }),
        (None, false) => Err(S3Error::msg(Code::MissingSecurityHeader, "Your request was missing a required header").with("MissingHeaderName", "x-amz-acl")),
    }
}

pub(crate) async fn put_bucket_acl(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    ctx.require(&bucket.name, Need::Admin)?;
    ctx.meter(&bucket.id, Class::A);
    let owner = bucket.account_id.clone();
    let r = requested(ctx, body, true, &owner, &owner).await?;
    let stored = check_write(ctx, &bucket.name, &r, &owner)?;
    ctx.store().set_bucket_config(&bucket.name, kind::ACL, "", stored.as_deref(), None).await.s3()?;
    Ok(response::empty(StatusCode::OK))
}

pub(crate) fn get_object_acl(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Read)?;
    let meta = ctx.store().get_object_version(&bucket.name, ctx.key_name(), ctx.q("versionId")).s3()?;
    ctx.meter(&bucket.id, Class::B);
    let a = ctx.access(&bucket.name)?;
    let acl = object_acl(a.acls_enabled, meta.headers.acl.as_deref());
    let owner = acl.owner.clone().unwrap_or_else(|| a.owner().to_string());
    response::ok_xml(render(ctx.store(), &acl, &owner))
}

pub(crate) async fn put_object_acl(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    ctx.require(&bucket.name, Need::Write)?;
    ctx.meter(&bucket.id, Class::A);
    let version = ctx.q("versionId").map(str::to_string);
    let meta = ctx.store().get_object_version(&bucket.name, ctx.key_name(), version.as_deref()).s3()?;
    let a = ctx.access(&bucket.name)?;
    // The object keeps its owner.
    let owner = object_acl(a.acls_enabled, meta.headers.acl.as_deref()).owner.unwrap_or_else(|| a.owner().to_string());
    let r = requested(ctx, body, false, a.owner(), &owner).await?;
    let stored = check_write(ctx, &bucket.name, &r, &owner)?;
    if a.acls_enabled {
        ctx.store().set_object_acl(&bucket.name, ctx.key_name(), version.as_deref(), stored).await.s3()?;
    }
    Ok(response::empty(StatusCode::OK))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Requester;

    const B: &str = "111111111111";

    fn key(id: &str) -> Principal {
        Principal::Known(Box::new(Requester { account: B.into(), direct: vec![id.into()], ..Default::default() }))
    }

    fn account(id: &str, root: bool) -> Principal {
        Principal::Known(Box::new(Requester { account: id.into(), root, ..Default::default() }))
    }

    #[test]
    fn canned_acls() {
        assert!(Acl::canned("private", false, B).unwrap().owner_only());
        let mut full = Acl::canned("bucket-owner-full-control", false, B).unwrap();
        assert!(!full.owner_only(), "names the bucket owner");
        full.normalize(B);
        assert!(full.owner_only(), "the bucket owner's own object");
        assert!(Acl::canned("public-read", false, B).unwrap().is_public());
        assert!(Acl::canned("authenticated-read", false, B).unwrap().is_public());
        assert!(Acl::canned("log-delivery-write", true, B).unwrap().grants.len() == 3);
        assert!(Acl::canned("log-delivery-write", false, B).is_err());
        assert_eq!(Acl::canned("everyone", false, B).unwrap_err().code, Code::InvalidArgument);
        assert_eq!(Acl::private().stored(), None);
        assert!(Acl::canned("public-read", false, B).unwrap().stored().is_some());
        let owned = Acl { owner: Some("222222222222".into()), ..Acl::private() };
        assert!(owned.stored().is_some(), "another account's object keeps its owner");
        assert_eq!(Acl::from_json(owned.stored().as_deref()), owned);
    }

    #[test]
    fn enforcement() {
        let pr = Acl::canned("public-read", false, B).unwrap();
        assert_eq!(pr.grants(&Principal::Anonymous, "READ", false, B), Some(Via::Direct));
        assert_eq!(pr.grants(&Principal::Anonymous, "READ", true, B), None, "public grants ignored");
        assert_eq!(pr.grants(&Principal::Anonymous, "WRITE", false, B), None);
        let ar = Acl::canned("authenticated-read", false, B).unwrap();
        assert_eq!(ar.grants(&Principal::Anonymous, "READ", false, B), None);
        assert_eq!(ar.grants(&key("RK1"), "READ", false, B), Some(Via::Direct));
        let k = Acl { grants: vec![grant(Grantee::Key { id: "RK1".into() }, "FULL_CONTROL")], owner: None };
        assert_eq!(k.grants(&key("RK1"), "WRITE_ACP", true, B), Some(Via::Direct));
        assert_eq!(k.grants(&key("RK2"), "READ", false, B), None);
        // Grants to an account delegate to it; its root is the account itself.
        let other = Acl { grants: vec![grant(Grantee::Owner, "FULL_CONTROL"), grant(Grantee::Account { id: "222222222222".into() }, "READ")], owner: None };
        assert_eq!(other.grants(&account("222222222222", false), "READ", false, B), Some(Via::Account));
        assert_eq!(other.grants(&account("222222222222", true), "READ", false, B), Some(Via::Direct));
        assert_eq!(other.grants(&account("222222222222", true), "WRITE", false, B), None);
        assert_eq!(other.grants(&account(B, true), "WRITE", false, B), Some(Via::Direct), "the owner's full control");
        assert_eq!(other.grants(&account("333333333333", true), "READ", false, B), None);
        assert_eq!(Acl::from_json(pr.stored().as_deref()), pr);
        assert_eq!(Acl::from_json(None), Acl::private());
    }

    #[test]
    fn ownership() {
        assert_eq!(Ownership::from_setting(None), Some(Ownership::BucketOwnerEnforced));
        assert_eq!(Ownership::from_setting(Some("")), None);
        assert_eq!(Ownership::from_setting(Some("ObjectWriter")), Some(Ownership::ObjectWriter));
        assert_eq!(Ownership::parse("Nope"), None);
    }

    #[test]
    fn stored_acls_from_before_accounts_still_read() {
        let old = r#"{"grants":[{"grantee":{"type":"owner"},"permission":"FULL_CONTROL"},{"grantee":{"type":"key","id":"RK1"},"permission":"READ"}]}"#;
        let acl = Acl::from_json(Some(old));
        assert_eq!(acl.owner, None);
        assert_eq!(acl.grants.len(), 2);
    }
}
