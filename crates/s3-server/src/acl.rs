//! ACLs and Object Ownership.
//!
//! **Ownership.** A bucket's `ObjectOwnership` is `BucketOwnerEnforced` unless set otherwise
//! (S3's default since 2023): ACLs are then disabled. Reads of an ACL show the owner's full
//! control, and writes carrying any ACL but `private`/`bucket-owner-full-control` (or the
//! equivalent grant) fail with `AccessControlListNotSupported`. With `BucketOwnerPreferred` or
//! `ObjectWriter` ACLs work, and are enforced. Deleting the ownership controls leaves the bucket
//! without any, which on S3 behaves like `ObjectWriter`.
//!
//! **Who's who.** Every bucket and object is owned by the deployment: its canonical owner id
//! ([`s3_store::Store::owner_id`]). Grants name either a group (`AllUsers`,
//! `AuthenticatedUsers`, `LogDelivery`) or a canonical user: the owner, or a roll access key
//! whose canonical id is [`s3_store::Store::canonical_user`] (a grant may also give the access
//! key id itself; it's stored as the key). Email grantees don't exist here
//! (`UnresolvableGrantByEmailAddress`).
//!
//! **Enforcement.** Grants add to what keys may do: `READ` on an object lets the grantee
//! `GetObject` it, `READ` on a bucket lists it, `WRITE` on a bucket writes and deletes objects,
//! `READ_ACP`/`WRITE_ACP` read and write the ACL, `FULL_CONTROL` is all of these. Grants to the
//! owner don't widen any key's own permission. `AllUsers` grants are anonymous access, unless
//! the public access block ignores public ACLs.

use crate::auth::Need;
use crate::ctx::{Ctx, read_small};
use crate::error::StoreResult;
use crate::policy::Principal;
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
    /// The deployment's owner.
    Owner,
    /// A roll access key (by access key id).
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
}

fn grant(grantee: Grantee, permission: &str) -> Grant {
    Grant { grantee, permission: permission.into() }
}

impl Acl {
    /// The owner has full control; nobody else has anything.
    pub fn private() -> Self {
        Self { grants: vec![grant(Grantee::Owner, "FULL_CONTROL")] }
    }

    /// A canned ACL (`x-amz-acl`).
    pub fn canned(name: &str, bucket: bool) -> Result<Self, S3Error> {
        let mut acl = Self::private();
        let group = |uri: &str| Grantee::Group { uri: uri.into() };
        match name.trim() {
            "private" | "bucket-owner-full-control" | "bucket-owner-read" | "aws-exec-read" => {}
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

    /// Grants to everyone or every authenticated user (S3 counts both as public).
    pub fn is_public(&self) -> bool {
        self.grants.iter().any(|g| matches!(&g.grantee, Grantee::Group { uri } if uri == ALL_USERS || uri == AUTHENTICATED_USERS))
    }

    /// Only the owner has anything: what `private` and `bucket-owner-full-control` mean.
    pub fn owner_only(&self) -> bool {
        self.grants.iter().all(|g| g.grantee == Grantee::Owner)
    }

    /// Whether the ACL gives `who` the permission `perm` (`FULL_CONTROL` gives every one).
    pub fn allows(&self, who: &Principal, perm: &str, ignore_public: bool) -> bool {
        self.grants.iter().any(|g| {
            (g.permission == perm || g.permission == "FULL_CONTROL")
                && match (&g.grantee, who) {
                    (Grantee::Group { uri }, _) if uri == ALL_USERS => !ignore_public,
                    (Grantee::Group { uri }, Principal::Key { .. } | Principal::Session { .. }) if uri == AUTHENTICATED_USERS => !ignore_public,
                    (Grantee::Key { id }, Principal::Key { id: k, .. }) => id == k,
                    _ => false,
                }
        })
    }

    /// What goes in the metadata: `None` for plain `private`.
    pub fn stored(&self) -> Option<String> {
        if *self == Self::private() { None } else { serde_json::to_string(self).ok() }
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

/// The deployment's owner, for `<Owner>` elements.
pub(crate) fn owner(store: &Store) -> xml::Owner {
    xml::Owner { id: store.owner_id(), display_name: "roll".into() }
}

/// A canonical user id (or an access key id) to a grantee.
fn resolve_id(store: &Store, id: &str) -> Result<Grantee, S3Error> {
    let id = id.trim();
    let invalid = || S3Error::msg(Code::InvalidArgument, "Invalid id").with("ArgumentName", "CanonicalUser/ID").with("ArgumentValue", id.to_string());
    let owner = store.owner_id();
    if id.eq_ignore_ascii_case(&owner) {
        return Ok(Grantee::Owner);
    }
    let keys = store.keys().s3()?;
    if let Some(k) = keys.iter().find(|k| k.id == id) {
        return Ok(Grantee::Key { id: k.id.clone() });
    }
    let lower = id.to_ascii_lowercase();
    keys.iter().find(|k| s3_store::config::canonical_user(&owner, &k.id) == lower).map(|k| Grantee::Key { id: k.id.clone() }).ok_or_else(invalid)
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
                "emailaddress" => return Err(S3Error::new(Code::UnresolvableGrantByEmailAddress).with("EmailAddress", val.to_string())),
                _ => return Err(bad()),
            };
            grants.push(grant(grantee, perm));
        }
    }
    if grants.len() > MAX_GRANTS {
        return Err(S3Error::msg(Code::MalformedACLError, format!("An ACL can have at most {MAX_GRANTS} grants")));
    }
    Ok((!grants.is_empty()).then_some(Acl { grants }))
}

/// The ACL a request asks for with `x-amz-acl` or `x-amz-grant-*` (not both), if any.
pub(crate) fn from_headers(ctx: &Ctx, bucket: bool) -> Result<Option<Acl>, S3Error> {
    let grants = grant_headers(ctx)?;
    match (ctx.h("x-amz-acl"), grants) {
        (Some(_), Some(_)) => Err(S3Error::msg(Code::InvalidRequest, "Specifying both Canned ACLs and Header Grants is not allowed")),
        (Some(c), None) => Acl::canned(c, bucket).map(Some),
        (None, g) => Ok(g),
    }
}

/// An `AccessControlPolicy` body.
fn parse_policy(ctx: &Ctx, body: &[u8]) -> Result<Acl, S3Error> {
    let malformed = || S3Error::new(Code::MalformedACLError);
    let n = xml::parse(body).map_err(|e| if e.code == Code::MalformedXML { malformed() } else { e })?;
    if n.name != "AccessControlPolicy" {
        return Err(malformed());
    }
    if let Some(id) = n.child("Owner").and_then(|o| o.text_of("ID"))
        && resolve_id(ctx.store(), id)? != Grantee::Owner
    {
        return Err(S3Error::msg(Code::AccessDenied, "The owner in the ACL must be the bucket owner"));
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
            return Err(S3Error::new(Code::UnresolvableGrantByEmailAddress).with("EmailAddress", email.to_string()));
        } else {
            return Err(malformed());
        };
        grants.push(grant(who, perm));
        if grants.len() > MAX_GRANTS {
            return Err(S3Error::msg(Code::MalformedACLError, format!("An ACL can have at most {MAX_GRANTS} grants")));
        }
    }
    Ok(Acl { grants })
}

/// `AccessControlPolicy` XML.
pub(crate) fn render(store: &Store, acl: &Acl) -> String {
    let o = owner(store);
    let keys = store.keys().unwrap_or_default();
    let mut s = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<AccessControlPolicy xmlns=\"{}\"><Owner><ID>{}</ID><DisplayName>{}</DisplayName></Owner><AccessControlList>",
        s3_proto::XMLNS,
        o.id,
        o.display_name
    );
    for g in &acl.grants {
        s.push_str("<Grant><Grantee xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" ");
        match &g.grantee {
            Grantee::Owner => s.push_str(&format!("xsi:type=\"CanonicalUser\"><ID>{}</ID><DisplayName>{}</DisplayName>", o.id, o.display_name)),
            Grantee::Key { id } => {
                let name = keys.iter().find(|k| &k.id == id).map_or(id.as_str(), |k| k.name.as_str());
                s.push_str(&format!("xsi:type=\"CanonicalUser\"><ID>{}</ID><DisplayName>{}</DisplayName>", s3_store::config::canonical_user(&o.id, id), xml::escape(name)));
            }
            Grantee::Group { uri } => s.push_str(&format!("xsi:type=\"Group\"><URI>{}</URI>", xml::escape(uri))),
        }
        s.push_str(&format!("</Grantee><Permission>{}</Permission></Grant>", g.permission));
    }
    s.push_str("</AccessControlList></AccessControlPolicy>");
    s
}

/// The ACL a write may set on a bucket or object of `bucket`: `None` for `private`. Fails as S3
/// does when ACLs are disabled (`AccessControlListNotSupported`) or the ACL is public and the
/// public access block blocks public ACLs (`AccessDenied`).
pub(crate) fn check_write(ctx: &Ctx, bucket: &str, acl: &Acl) -> Result<Option<String>, S3Error> {
    let a = ctx.access(bucket)?;
    if !a.acls_enabled {
        return if acl.owner_only() { Ok(None) } else { Err(S3Error::new(Code::AccessControlListNotSupported)) };
    }
    if acl.is_public() && a.block.block_public_acls {
        return Err(S3Error::msg(Code::AccessDenied, "Access Denied: the bucket's public access block blocks public ACLs"));
    }
    Ok(acl.stored())
}

/// The ACL for a new object from the request's headers (`None`: private).
pub(crate) fn for_new_object(ctx: &Ctx, bucket: &str) -> Result<Option<String>, S3Error> {
    match from_headers(ctx, false)? {
        Some(acl) => check_write(ctx, bucket, &acl),
        None => Ok(None),
    }
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
    response::ok_xml(render(ctx.store(), &acl))
}

/// The ACL of a Put*Acl request: headers or an `AccessControlPolicy` body, not both.
async fn requested(ctx: &Ctx, body: Body, for_bucket: bool) -> Result<Acl, S3Error> {
    let raw = read_small(ctx, body, xml::MAX_XML_BYTES).await?;
    let from_body = !raw.iter().all(|b| b.is_ascii_whitespace());
    match (from_headers(ctx, for_bucket)?, from_body) {
        (Some(_), true) => Err(S3Error::msg(Code::InvalidRequest, "Specify an ACL either in headers or in the request body, not both")),
        (Some(acl), false) => Ok(acl),
        (None, true) => parse_policy(ctx, &raw),
        (None, false) => Err(S3Error::msg(Code::MissingSecurityHeader, "Your request was missing a required header").with("MissingHeaderName", "x-amz-acl")),
    }
}

pub(crate) async fn put_bucket_acl(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    ctx.require(&bucket.name, Need::Admin)?;
    ctx.meter(&bucket.id, Class::A);
    let acl = requested(ctx, body, true).await?;
    let stored = check_write(ctx, &bucket.name, &acl)?;
    ctx.store().set_bucket_config(&bucket.name, kind::ACL, "", stored.as_deref(), None).await.s3()?;
    Ok(response::empty(StatusCode::OK))
}

pub(crate) fn get_object_acl(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Read)?;
    let meta = ctx.store().get_object_version(&bucket.name, ctx.key_name(), ctx.q("versionId")).s3()?;
    ctx.meter(&bucket.id, Class::B);
    let a = ctx.access(&bucket.name)?;
    response::ok_xml(render(ctx.store(), &object_acl(a.acls_enabled, meta.headers.acl.as_deref())))
}

pub(crate) async fn put_object_acl(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    ctx.require(&bucket.name, Need::Write)?;
    ctx.meter(&bucket.id, Class::A);
    let acl = requested(ctx, body, false).await?;
    let version = ctx.q("versionId").map(str::to_string);
    ctx.store().get_object_version(&bucket.name, ctx.key_name(), version.as_deref()).s3()?;
    let stored = check_write(ctx, &bucket.name, &acl)?;
    if ctx.access(&bucket.name)?.acls_enabled {
        ctx.store().set_object_acl(&bucket.name, ctx.key_name(), version.as_deref(), stored).await.s3()?;
    }
    Ok(response::empty(StatusCode::OK))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str) -> Principal {
        Principal::Key { id: id.into(), canonical: String::new() }
    }

    #[test]
    fn canned_acls() {
        assert!(Acl::canned("private", false).unwrap().owner_only());
        assert!(Acl::canned("bucket-owner-full-control", false).unwrap().owner_only());
        assert!(Acl::canned("public-read", false).unwrap().is_public());
        assert!(Acl::canned("authenticated-read", false).unwrap().is_public());
        assert!(Acl::canned("log-delivery-write", true).unwrap().grants.len() == 3);
        assert!(Acl::canned("log-delivery-write", false).is_err());
        assert_eq!(Acl::canned("everyone", false).unwrap_err().code, Code::InvalidArgument);
        assert_eq!(Acl::private().stored(), None);
        assert!(Acl::canned("public-read", false).unwrap().stored().is_some());
    }

    #[test]
    fn enforcement() {
        let pr = Acl::canned("public-read", false).unwrap();
        assert!(pr.allows(&Principal::Anonymous, "READ", false));
        assert!(!pr.allows(&Principal::Anonymous, "READ", true), "public grants ignored");
        assert!(!pr.allows(&Principal::Anonymous, "WRITE", false));
        let ar = Acl::canned("authenticated-read", false).unwrap();
        assert!(!ar.allows(&Principal::Anonymous, "READ", false));
        assert!(ar.allows(&key("RK1"), "READ", false));
        let k = Acl { grants: vec![grant(Grantee::Key { id: "RK1".into() }, "FULL_CONTROL")] };
        assert!(k.allows(&key("RK1"), "WRITE_ACP", true));
        assert!(!k.allows(&key("RK2"), "READ", false));
        assert!(!Acl::private().allows(&key("RK1"), "READ", false), "owner grants don't widen keys");
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
}
