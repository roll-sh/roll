//! The storage developer API's callers (`s3-api`, the Supabase-Storage-compatible REST API), and
//! what they may do: decided by the S3 API's own evaluator (`access.rs`), never a copy of it.
//!
//! - **The service key** acts as the project's account's root user (Supabase's `service_role`
//!   bypasses row-level security; here it's the root user's own side that allows everything in
//!   its account, as for any root request, and a bucket policy's explicit `Deny` still applies).
//! - **The anon key and app users** (JWTs of the project's auth providers) are [`Kind::App`]
//!   identities with no identity policies of their own: in their account, a bucket's resource
//!   grants decide. Bucket policies name them with `{"App": "<role>"}` principals (`anon`,
//!   `authenticated`, the JWT's own `role`, or `*`), `{"AWS": "*"}`, or the `public` flag, and
//!   their claims are condition keys and policy variables: `jwt:sub`, `jwt:role`, `jwt:email`,
//!   `jwt:<claim>` (`jwt:app_metadata.plan` for nested claims), so
//!   `arn:aws:s3:::avatars/${jwt:sub}/*` is a user's own folder. `roll:ObjectOwner` is the
//!   current object's owner (the `sub` that wrote it; `""` for objects written otherwise), for
//!   "owner only" rules. Another account's buckets grant them nothing: their own side never
//!   allows anything, and across accounts both sides must.
//! - **Anonymous** requests (public URLs) are the S3 API's anonymous requester.
//!
//! A [`Checker`] is one request's view: it keeps the access rules of the buckets it has looked
//! at, so checking every entry of a listing costs one policy parse.

use crate::Server;
use crate::access;
use crate::auth::{Auth, Need};
use crate::ctx::Ctx;
use crate::identity::{Identity, Kind};
use crate::policy::{Principal, Requester};
use axum::http::{HeaderMap, Method};
use s3_proto::{Code, S3Error};
use s3_store::sts::OidcProvider;
use s3_store::{AccessKey, BlobSse, Bucket, Permission, Store, StoreError};
use serde_json::{Map, Value};
use std::net::IpAddr;
use std::sync::Arc;

/// A developer API caller's JWT claims.
#[derive(Debug, Default, Clone)]
pub struct Claims(Map<String, Value>);

impl Claims {
    pub fn new(claims: Map<String, Value>) -> Self {
        Self(claims)
    }

    /// A claim by name (case-insensitively, as condition keys are), or by a dotted path into
    /// nested objects (`app_metadata.plan`).
    pub fn get(&self, name: &str) -> Option<&Value> {
        fn find<'a>(m: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
            m.get(name).or_else(|| m.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v))
        }
        if let Some(v) = find(&self.0, name) {
            return Some(v);
        }
        let mut cur = &self.0;
        let mut parts = name.split('.').peekable();
        while let Some(p) = parts.next() {
            let v = find(cur, p)?;
            if parts.peek().is_none() {
                return Some(v);
            }
            cur = v.as_object()?;
        }
        None
    }

    /// A claim's values as a condition key's: a string, number or boolean is one value, an
    /// array of them many; objects and `null` are absent.
    pub(crate) fn values(&self, name: &str) -> Option<Vec<String>> {
        fn scalar(v: &Value) -> Option<String> {
            match v {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                Value::Bool(b) => Some(b.to_string()),
                _ => None,
            }
        }
        match self.get(name)? {
            Value::Array(a) => Some(a.iter().filter_map(scalar).collect()),
            v => scalar(v).map(|s| vec![s]),
        }
    }

    pub fn str(&self, name: &str) -> Option<&str> {
        self.0.get(name).and_then(Value::as_str)
    }
}

#[derive(Debug, Clone)]
enum Who {
    Anonymous,
    Service { account: String },
    App { account: String, role: String, sub: Option<String> },
}

/// Who a developer API request comes from.
#[derive(Clone)]
pub struct AppCaller {
    who: Who,
    auth: Option<(AccessKey, Arc<Identity>)>,
}

impl std::fmt::Debug for AppCaller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppCaller").field("who", &self.who).finish_non_exhaustive()
    }
}

/// A key that's never stored and never authenticates anything: what the evaluator sees behind
/// a developer API caller.
fn caller_key(account: &str, root: bool) -> AccessKey {
    AccessKey {
        id: if root { "RKROLLAPISERVICE0000" } else { "RKROLLAPICALLER00000" }.into(),
        secret: String::new(),
        name: if root { "service_role" } else { "app" }.into(),
        permission: if root { Permission::Admin } else { Permission::Read },
        // An app caller's key reaches no bucket by itself: only resource grants let it in.
        buckets: if root { None } else { Some(Vec::new()) },
        created_at: 0,
        last_used_at: None,
        account: account.to_string(),
        user: None,
        active: true,
        session: None,
    }
}

impl AppCaller {
    /// No key at all (public URLs).
    pub fn anonymous() -> Self {
        Self { who: Who::Anonymous, auth: None }
    }

    /// The project's service key: the account's root user.
    pub fn service(store: &Store, account: &str) -> Result<Self, StoreError> {
        let key = caller_key(account, true);
        let id = crate::identity::resolve(store, &key)?;
        Ok(Self { who: Who::Service { account: account.to_string() }, auth: Some((key, Arc::new(id))) })
    }

    /// The anon role (`claims` empty) or an app user, in `account`, acting as `role`.
    pub fn app(store: &Store, account: &str, role: &str, claims: Map<String, Value>) -> Result<Self, StoreError> {
        let acct = store.require_account(account)?;
        let claims = Claims::new(claims);
        let sub = claims.str("sub").filter(|s| !s.is_empty()).map(str::to_string);
        let req = Requester { account: acct.id.clone(), account_canonical: acct.canonical.clone(), app_role: Some(role.to_string()), ..Default::default() };
        let arn = format!("arn:roll:storage-api::{}:role/{role}", acct.id);
        let id = Identity {
            account: acct.clone(),
            kind: Kind::App { role: role.to_string(), sub: sub.clone() },
            principal: Principal::Known(Box::new(req)),
            principal_arn: arn.clone(),
            caller_arn: arn,
            user_id: sub.clone().unwrap_or_else(|| role.to_string()),
            username: None,
            tags: Vec::new(),
            policies: Vec::new(),
            boundaries: Vec::new(),
            session_policies: Vec::new(),
            session: None,
            dead: false,
            app: Some(Arc::new(claims)),
        };
        Ok(Self { who: Who::App { account: acct.id, role: role.to_string(), sub }, auth: Some((caller_key(account, false), Arc::new(id))) })
    }

    /// The project (account) it acts in; `None` when anonymous.
    pub fn account(&self) -> Option<&str> {
        match &self.who {
            Who::Anonymous => None,
            Who::Service { account } | Who::App { account, .. } => Some(account),
        }
    }

    /// `service_role`, `anon`, `authenticated` or the JWT's own; `None` when anonymous.
    pub fn role(&self) -> Option<&str> {
        match &self.who {
            Who::Anonymous => None,
            Who::Service { .. } => Some("service_role"),
            Who::App { role, .. } => Some(role),
        }
    }

    /// The JWT's `sub`: an object's owner when this caller writes it.
    pub fn sub(&self) -> Option<&str> {
        match &self.who {
            Who::App { sub, .. } => sub.as_deref(),
            _ => None,
        }
    }

    pub fn is_service(&self) -> bool {
        matches!(self.who, Who::Service { .. })
    }

    pub fn is_anonymous(&self) -> bool {
        matches!(self.who, Who::Anonymous)
    }

    /// Who did it, in event records (`userIdentity.principalId`).
    pub fn event_principal(&self) -> String {
        match &self.who {
            Who::Anonymous => "anonymous".into(),
            Who::Service { account } => format!("{account}:service_role"),
            Who::App { role, sub, .. } => sub.clone().unwrap_or_else(|| role.clone()),
        }
    }

    /// A checker for one request from `peer` with these headers (`aws:SourceIp`,
    /// `aws:UserAgent`, `aws:Referer` and the like).
    pub fn checker(&self, server: &Arc<Server>, peer: Option<IpAddr>, headers: &HeaderMap) -> Checker {
        let auth = self.auth.as_ref().map(|(k, id)| Auth {
            key: k.clone(),
            verified: s3_proto::sigv4::Verified { access_key: k.id.clone(), payload: s3_proto::sigv4::Payload::Unsigned, stream: None },
            identity: id.clone(),
        });
        let ctx = Ctx {
            server: server.clone(),
            method: Method::POST,
            raw_path: "/".into(),
            raw_query: String::new(),
            query: Vec::new(),
            headers: headers.clone(),
            bucket: None,
            key: None,
            vhost: false,
            peer,
            auth,
            website: false,
            access_cache: Default::default(),
            started: std::time::Instant::now(),
            access_point: None,
            payers: Default::default(),
        };
        Checker { ctx }
    }
}

/// One request's authorization checks.
pub struct Checker {
    ctx: Ctx,
}

impl Checker {
    /// May the caller do `action` (an S3 action: `s3:GetObject`, `s3:PutObject`,
    /// `s3:DeleteObject`, `s3:ListBucket`, …) on `bucket` (and `key`)? `AccessDenied` or
    /// `NoSuchBucket` when not.
    pub fn allows(&self, action: &str, bucket: &str, key: Option<&str>) -> Result<(), S3Error> {
        access::authorize_as(&self.ctx, self.ctx.who(), bucket, key, action, Need::Admin, None)
    }

    /// May the caller list `bucket` under `prefix` (`s3:ListBucket` with `s3:prefix`)?
    pub fn may_list(&self, bucket: &str, prefix: &str) -> bool {
        access::authorize_as(&self.ctx, self.ctx.who(), bucket, None, "s3:ListBucket", Need::Read, Some(prefix)).is_ok()
    }

    /// Forget the cached rules of `bucket` (after changing its policy or flags).
    pub fn forget(&self, bucket: &str) {
        self.ctx.forget_access(bucket);
    }
}

/// Verify an OIDC token of one of `providers` (JWKS fetched and cached as for
/// `AssumeRoleWithWebIdentity`): its claims, or why not (never the token itself).
pub async fn verify_oidc(server: &Server, providers: &[OidcProvider], token: &str) -> Result<Map<String, Value>, String> {
    let now_s = server.store.now().div_euclid(1000);
    match server.oidc.verify(providers, token, now_s).await {
        Ok(v) => Ok(v.claims),
        Err(crate::sts::oidc::OidcError::Invalid(m)) => Err(m),
        Err(crate::sts::oidc::OidcError::Expired) => Err("jwt expired".into()),
        Err(crate::sts::oidc::OidcError::Denied) => Err("the token's subject isn't accepted".into()),
        Err(crate::sts::oidc::OidcError::Idp(m)) => {
            tracing::warn!(error = m, "fetching an OIDC provider's keys failed");
            Err("couldn't retrieve the identity provider's signing keys; try again".into())
        }
    }
}

/// The encryption new blobs of `bucket` get by default (its default encryption, else the
/// backing bucket's).
pub fn default_sse(store: &Store, bucket: &Bucket) -> Option<BlobSse> {
    crate::sse::default_sse(store, bucket)
}

/// The bucket's size limit allows `len` bytes at `key` (an early check before any bytes
/// move; the store checks again when the version is written).
pub(crate) fn check_size_limit(store: &Store, bucket: &Bucket, key: &str, len: u64) -> Result<(), S3Error> {
    store.bucket_limits(&bucket.name).and_then(|l| l.check_size(key, len)).map_err(crate::error::store_error)
}

/// The bucket's allowed media types allow `content_type` at `key` (early, as above).
pub(crate) fn check_type_limit(store: &Store, bucket: &Bucket, key: &str, content_type: Option<&str>) -> Result<(), S3Error> {
    store.bucket_limits(&bucket.name).and_then(|l| l.check_type(key, content_type)).map_err(crate::error::store_error)
}

/// `AccessDenied` (for callers matching on codes).
pub fn is_denied(e: &S3Error) -> bool {
    e.code == Code::AccessDenied
}

/// The largest bucket policy (PutBucketPolicy's limit).
pub const MAX_POLICY_BYTES: usize = crate::policy::MAX_POLICY_BYTES;

/// A rule preset: a bucket policy for a common case (the control plane and the developer API
/// generate the same ones).
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
}

pub const PRESETS: &[Preset] = &[
    Preset { id: "public-read", name: "Public read", description: "Anyone can download objects, with or without a key (public URLs work). Uploads need the service key." },
    Preset { id: "authenticated-read", name: "Authenticated read", description: "Signed-in users (any valid user token) can list and download objects. Uploads need the service key." },
    Preset { id: "user-folders", name: "User folders", description: "Signed-in users list, read, write and delete only under <bucket>/<their user id>/." },
    Preset { id: "owner-only", name: "Owner only", description: "Signed-in users upload objects, and read, replace and delete only the objects they uploaded (the object's owner is the uploader's user id)." },
];

/// The bucket policy of a preset for `bucket` (`None`: no such preset).
pub fn preset_policy(preset: &str, bucket: &str) -> Option<Value> {
    let objects = format!("arn:aws:s3:::{bucket}/*");
    let bucket_arn = format!("arn:aws:s3:::{bucket}");
    let user = serde_json::json!({ "App": "authenticated" });
    let statements = match preset {
        "public-read" => serde_json::json!([{ "Sid": "PublicRead", "Effect": "Allow", "Principal": "*", "Action": "s3:GetObject", "Resource": objects }]),
        "authenticated-read" => serde_json::json!([
            { "Sid": "AuthenticatedRead", "Effect": "Allow", "Principal": user, "Action": "s3:GetObject", "Resource": objects },
            { "Sid": "AuthenticatedList", "Effect": "Allow", "Principal": user, "Action": "s3:ListBucket", "Resource": bucket_arn },
        ]),
        "user-folders" => serde_json::json!([
            { "Sid": "OwnFolder", "Effect": "Allow", "Principal": user, "Action": ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"], "Resource": format!("arn:aws:s3:::{bucket}/${{jwt:sub}}/*") },
            { "Sid": "ListOwnFolder", "Effect": "Allow", "Principal": user, "Action": "s3:ListBucket", "Resource": bucket_arn, "Condition": { "StringLike": { "s3:prefix": ["${jwt:sub}/*"] } } },
        ]),
        "owner-only" => serde_json::json!([
            { "Sid": "OwnerReadDelete", "Effect": "Allow", "Principal": user, "Action": ["s3:GetObject", "s3:DeleteObject"], "Resource": objects, "Condition": { "StringEquals": { "roll:ObjectOwner": "${jwt:sub}" } } },
            { "Sid": "OwnerWrite", "Effect": "Allow", "Principal": user, "Action": "s3:PutObject", "Resource": objects, "Condition": { "StringEqualsIfExists": { "roll:ObjectOwner": "${jwt:sub}" } } },
        ]),
        _ => return None,
    };
    Some(serde_json::json!({ "Version": "2012-10-17", "Statement": statements }))
}

/// Why a bucket policy wasn't set.
#[derive(Debug)]
pub enum PolicyError {
    /// The document is refused (PutBucketPolicy's `MalformedPolicy` and the like).
    Invalid(String),
    Store(StoreError),
}

/// Set (`Some`) or delete (`None`) a bucket's policy, checked as PutBucketPolicy checks it: the
/// size, the document, Block Public Access and the principals it names.
pub async fn put_bucket_policy(store: &Store, bucket: &str, doc: Option<&str>) -> Result<(), PolicyError> {
    let Some(doc) = doc else { return store.set_bucket_policy(bucket, None).await.map_err(PolicyError::Store) };
    let b = store.bucket(bucket).map_err(PolicyError::Store)?;
    let public = crate::validate_bucket_policy(doc, bucket).map_err(PolicyError::Invalid)?;
    let (_, block) = store.public_access_block(&b.id).map_err(PolicyError::Store)?;
    if public && block.block_public_policy {
        return Err(PolicyError::Invalid("This policy grants public access, which the bucket's Block Public Access settings block (BlockPublicPolicy).".into()));
    }
    let principals = crate::bucket_policy_principals(store, doc).map_err(PolicyError::Invalid)?;
    store.set_bucket_policy(bucket, Some((doc, &principals))).await.map_err(PolicyError::Store)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_valid_policies() {
        for p in PRESETS {
            let doc = preset_policy(p.id, "photos").unwrap().to_string();
            let public = crate::validate_bucket_policy(&doc, "photos").unwrap_or_else(|e| panic!("{}: {e}", p.id));
            assert_eq!(public, p.id == "public-read", "{}", p.id);
        }
        assert!(preset_policy("nope", "photos").is_none());
        // An anon-role grant is public too (the anon key is public).
        let anon = r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"App":"anon"},"Action":"s3:GetObject","Resource":"arn:aws:s3:::photos/*"}]}"#;
        assert!(crate::validate_bucket_policy(anon, "photos").unwrap());
        let bad = r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"App":"a b"},"Action":"s3:GetObject","Resource":"arn:aws:s3:::photos/*"}]}"#;
        assert!(crate::validate_bucket_policy(bad, "photos").is_err());
    }

    #[test]
    fn claims_as_condition_values() {
        let c = Claims::new(serde_json::from_str(r#"{"sub":"u1","Email":"a@b.c","roles":["x","y"],"n":3,"ok":true,"app_metadata":{"plan":"pro"},"o":{}}"#).unwrap());
        assert_eq!(c.values("sub"), Some(vec!["u1".into()]));
        assert_eq!(c.values("email"), Some(vec!["a@b.c".into()]));
        assert_eq!(c.values("roles"), Some(vec!["x".into(), "y".into()]));
        assert_eq!(c.values("n"), Some(vec!["3".into()]));
        assert_eq!(c.values("ok"), Some(vec!["true".into()]));
        assert_eq!(c.values("app_metadata.plan"), Some(vec!["pro".into()]));
        assert_eq!(c.values("o"), None);
        assert_eq!(c.values("missing"), None);
    }
}
