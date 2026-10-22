//! Temporary credentials (STS): roles, sessions and their self-contained session tokens.
//!
//! **Roles** are metadata rows (`roles`), per account: a name, a trust (the roll keys and OIDC
//! providers that may assume the role, and an optional external id, or an IAM trust policy
//! document), a permission and bucket scope like a key's (control-plane roles) and/or IAM
//! policies (inline and managed, a permissions boundary), and a maximum session duration.
//!
//! **Sessions are never stored.** A session token is the session's claims (temporary access
//! key id, secret, role, permission, scope, session policy, tags, expiry) encrypted and
//! authenticated with AES-256-GCM under a deployment key kept in `state`
//! (`sts_session_keys`, created once). Every token gets its own AES key, derived from the
//! deployment key and a random 128-bit salt with HMAC-SHA256, so random nonces never repeat
//! under one AES key however many tokens are minted. Checking a request takes no lookup and
//! no write; only the role (or the key behind a `GetSessionToken` session) is read, a local
//! SQLite read, so a session acts with its role's *current* permissions, as on AWS:
//!
//! - changing a role changes what its sessions may do, from the next request on;
//! - deleting a role leaves its sessions without any permission (`AccessDenied`), and a new
//!   role of the same name (another `id`) doesn't give them any;
//! - [`Store::revoke_role_sessions`] denies the sessions issued until now: AWS's
//!   `AWSRevokeOlderSessions` inline policy (a `Deny` on `aws:TokenIssueTime`), and
//!   `revoked_before`;
//! - a `GetSessionToken` session acts with its key's current permission, and has none once the
//!   key is gone.
//!
//! The deployment key rotates ([`Store::rotate_session_key`]): tokens sealed with the current
//! or the previous key are accepted, so rotating once every session lifetime (36 h at most)
//! never cuts a live session short.
//!
//! Token layout (base64url, no padding):
//! `version (1) | key id (4, BE) | salt (16) | nonce (12) | AES-256-GCM(claims JSON) + tag (16)`,
//! with the first 33 bytes as associated data.

use crate::error::{Result, StoreError};
use crate::ids;
use crate::model::{AccessKey, Permission};
use crate::rows;
use crate::store::Store;
use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::BTreeMap;
use std::sync::Arc;
use zeroize::Zeroize;

/// Longest session token accepted (the request's, before anything is decoded).
pub const MAX_TOKEN_BYTES: usize = 12 * 1024;
/// Largest session policy (AWS's limit for inline session policies).
pub const MAX_SESSION_POLICY_BYTES: usize = 2048;
/// Largest inline role policy (AWS's limit for a role's inline policies).
pub const MAX_ROLE_POLICY_BYTES: usize = 10 * 1024;
/// Session tags: at most 50, with at most this many bytes of keys and values together.
pub const MAX_TAGS: usize = 50;
pub const MAX_TAG_BYTES: usize = 2048;
/// Session durations, in seconds.
pub const MIN_SESSION_S: i64 = 900;
pub const GET_SESSION_TOKEN_DEFAULT_S: i64 = 43_200;
pub const GET_SESSION_TOKEN_MAX_S: i64 = 129_600;
pub const ROLE_DEFAULT_SESSION_S: i64 = 3600;
pub const ROLE_MAX_SESSION_S: i64 = 43_200;
/// Trust entries per role.
pub const MAX_TRUSTED_KEYS: usize = 100;
pub const MAX_OIDC_PROVIDERS: usize = 20;

const KEYS_STATE: &str = "sts_session_keys";
const VERSION: u8 = 1;
const HEADER: usize = 1 + 4 + 16 + 12;
const TAG: usize = 16;

// ---- Roles ----

/// An OpenID Connect provider a role trusts for `AssumeRoleWithWebIdentity`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcProvider {
    /// The tokens' `iss`, exactly (`https://accounts.google.com`, `https://<tenant>.auth0.com/`).
    pub issuer: String,
    /// Accepted `aud` values (client ids); a token must name one of them.
    pub audiences: Vec<String>,
    /// A pattern (`*`, `?` wildcards) the token's `sub` must match; none: any subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Where the signing keys are; none: the issuer's OpenID discovery document says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwks_uri: Option<String>,
}

impl OidcProvider {
    /// The issuer without its scheme and trailing slash: how IAM names the provider in
    /// condition keys (`<provider>:sub`) and ARNs.
    pub fn name(&self) -> String {
        provider_name(&self.issuer)
    }
}

/// `https://accounts.google.com/` → `accounts.google.com`.
pub fn provider_name(issuer: &str) -> String {
    let s = issuer.split_once("://").map_or(issuer, |(_, r)| r);
    s.trim_end_matches('/').to_string()
}

/// Who may assume a role.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trust {
    /// Permanent roll access key ids (`AssumeRole`).
    #[serde(default)]
    pub keys: Vec<String>,
    /// OIDC providers (`AssumeRoleWithWebIdentity`).
    #[serde(default)]
    pub oidc: Vec<OidcProvider>,
    /// `AssumeRole` must pass this `ExternalId` (the confused-deputy guard of third-party
    /// access).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Role {
    /// The account it belongs to (12 digits).
    pub account: String,
    pub name: String,
    /// `RR…` (`AROA…` for IAM-created roles): what `aws:userid` starts with for its sessions.
    pub id: String,
    pub description: String,
    /// A roll permission level its sessions have (control-plane roles); `None`: only its IAM
    /// policies (IAM-created roles).
    pub permission: Option<Permission>,
    /// Bucket names the permission is limited to; `None` = all.
    pub buckets: Option<Vec<String>>,
    /// A control-plane role's inline IAM policy (JSON): an upper bound on what its sessions may
    /// do, like a permissions boundary.
    pub policy: Option<String>,
    pub trust: Trust,
    /// IAM's side: path, trust policy, policies, boundary, tags.
    pub iam: RoleIam,
    pub max_session_s: i64,
    /// Goes up on every change (sessions keep working: they act with the role as it is now).
    pub generation: i64,
    /// Sessions issued at or before this (Unix ms) are denied everything; 0: none.
    pub revoked_before: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// An IAM role's own settings (`CreateRole`, `PutRolePolicy`, `AttachRolePolicy`, …).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoleIam {
    /// `/` or `/a/b/`; empty means `/`.
    pub path: String,
    /// The `AssumeRolePolicyDocument` (JSON). When set it decides who may assume the role,
    /// instead of `trust`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust_policy: Option<String>,
    /// Inline policies by name.
    pub policies: BTreeMap<String, String>,
    /// Managed policy ARNs.
    pub attached: Vec<String>,
    /// The permissions boundary (a managed policy ARN).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary: Option<String>,
    pub tags: Vec<(String, String)>,
}

impl Role {
    /// `arn:aws:iam::<account>:role<path><name>`.
    pub fn arn(&self, account: &str) -> String {
        role_arn_with_path(account, &self.iam.path, &self.name)
    }

    /// This role's ARN in its own account.
    pub fn own_arn(&self) -> String {
        self.arn(&self.account)
    }
}

pub fn role_arn(account: &str, name: &str) -> String {
    format!("arn:aws:iam::{account}:role/{name}")
}

/// A role's ARN with its IAM path (`/` when empty).
pub fn role_arn_with_path(account: &str, path: &str, name: &str) -> String {
    let path = if path.is_empty() { "/" } else { path };
    format!("arn:aws:iam::{account}:role{path}{name}")
}

/// The inline policy AWS's console puts on a role to revoke its sessions.
pub const REVOKE_POLICY_NAME: &str = "AWSRevokeOlderSessions";

/// Deny everything to sessions issued before `before_ms` (Unix ms).
pub fn revoke_policy(before_ms: i64) -> String {
    serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [{"Effect": "Deny", "Action": ["*"], "Resource": ["*"],
                       "Condition": {"DateLessThan": {"aws:TokenIssueTime": iso_ms(before_ms)}}}]
    })
    .to_string()
}

/// ISO 8601 with milliseconds, UTC (what `aws:TokenIssueTime` is compared as).
pub fn iso_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let sod = secs.rem_euclid(86_400);
    format!("{}T{:02}:{:02}:{:02}.{:03}Z", ids::day_of(ms), sod / 3600, sod / 60 % 60, sod % 60, ms.rem_euclid(1000))
}

/// What a role is made of (create, or replace on update).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleSpec {
    #[serde(default)]
    pub description: String,
    pub permission: Permission,
    #[serde(default)]
    pub buckets: Option<Vec<String>>,
    /// The inline policy, as JSON text. Callers check it's a valid IAM policy first
    /// (`s3_server::validate_identity_policy`); the store only checks size and JSON.
    #[serde(default)]
    pub policy: Option<String>,
    #[serde(default)]
    pub trust: Trust,
    #[serde(default)]
    pub max_session_s: Option<i64>,
}

fn charset_ok(s: &str, extra: &str) -> bool {
    s.chars().all(|c| c.is_ascii_alphanumeric() || "_+=,.@-".contains(c) || extra.contains(c))
}

/// IAM's rules: 1–64 characters of letters, digits and `_+=,.@-`.
pub fn validate_role_name(name: &str) -> std::result::Result<(), String> {
    if name.is_empty() || name.len() > 64 || !charset_ok(name, "") {
        return Err(format!("role names are 1-64 letters, digits and _+=,.@- (got {name:?})"));
    }
    Ok(())
}

/// `RoleSessionName`: 2–64 characters of letters, digits and `_+=,.@-`.
pub fn validate_session_name(name: &str) -> std::result::Result<(), String> {
    if name.len() < 2 || name.len() > 64 || !charset_ok(name, "") {
        return Err("RoleSessionName must be 2-64 characters of letters, digits and _+=,.@-".into());
    }
    Ok(())
}

/// `SourceIdentity`: like a session name, and not starting with `aws:`.
pub fn validate_source_identity(v: &str) -> std::result::Result<(), String> {
    if v.len() < 2 || v.len() > 64 || !charset_ok(v, "") || v.to_ascii_lowercase().starts_with("aws:") {
        return Err("SourceIdentity must be 2-64 characters of letters, digits and _+=,.@-, not starting with aws:".into());
    }
    Ok(())
}

/// `ExternalId`: 2–1224 characters of letters, digits and `_+=,.@:/-`.
pub fn validate_external_id(v: &str) -> std::result::Result<(), String> {
    if v.len() < 2 || v.len() > 1224 || !charset_ok(v, ":/") {
        return Err("ExternalId must be 2-1224 characters of letters, digits and _+=,.@:/-".into());
    }
    Ok(())
}

/// Session tags and the keys among them that are transitive.
pub type SessionTags = (Vec<(String, String)>, Vec<String>);

/// Session tags (IAM's rules plus a size cap that keeps tokens small), and transitive tag keys
/// that must name session tags.
pub fn validate_tags(tags: &[(String, String)], transitive: &[String]) -> std::result::Result<(), String> {
    if tags.len() > MAX_TAGS {
        return Err(format!("at most {MAX_TAGS} session tags"));
    }
    let tag_char = |c: char| c.is_alphanumeric() || c == ' ' || "_.:/=+-@".contains(c);
    let mut seen = std::collections::HashSet::new();
    let mut bytes = 0;
    for (k, v) in tags {
        if k.is_empty() || k.chars().count() > 128 || !k.chars().all(tag_char) {
            return Err(format!("invalid session tag key {k:?}: 1-128 letters, digits, spaces and _.:/=+-@"));
        }
        if k.to_ascii_lowercase().starts_with("aws:") {
            return Err("session tag keys can't start with aws:".into());
        }
        if v.chars().count() > 256 || !v.chars().all(tag_char) {
            return Err(format!("invalid value for session tag {k:?}: up to 256 letters, digits, spaces and _.:/=+-@"));
        }
        if !seen.insert(k.to_lowercase()) {
            return Err(format!("duplicate session tag {k:?} (tag keys are case-insensitive)"));
        }
        bytes += k.len() + v.len();
    }
    if bytes > MAX_TAG_BYTES {
        return Err(format!("session tags are limited to {MAX_TAG_BYTES} bytes of keys and values"));
    }
    if transitive.len() > MAX_TAGS {
        return Err(format!("at most {MAX_TAGS} transitive tag keys"));
    }
    for t in transitive {
        if !seen.contains(&t.to_lowercase()) {
            return Err(format!("transitive tag key {t:?} isn't a session tag"));
        }
    }
    Ok(())
}

fn invalid(why: impl Into<String>) -> StoreError {
    StoreError::InvalidArgument(why.into())
}

impl RoleSpec {
    fn check(&self, store: &Store) -> Result<i64> {
        if self.description.len() > 1000 {
            return Err(invalid("descriptions are at most 1000 characters"));
        }
        if let Some(bs) = &self.buckets {
            if bs.is_empty() {
                return Err(invalid("buckets: give at least one, or leave it out for all"));
            }
            for b in bs {
                crate::validate::bucket_name(b)?;
            }
        }
        if let Some(p) = &self.policy {
            if p.len() > MAX_ROLE_POLICY_BYTES {
                return Err(invalid(format!("role policies are at most {MAX_ROLE_POLICY_BYTES} bytes")));
            }
            if !serde_json::from_str::<serde_json::Value>(p).is_ok_and(|v| v.is_object()) {
                return Err(invalid("the role policy must be a JSON object"));
            }
        }
        let t = &self.trust;
        if t.keys.len() > MAX_TRUSTED_KEYS {
            return Err(invalid(format!("a role trusts at most {MAX_TRUSTED_KEYS} keys")));
        }
        for k in &t.keys {
            if ids::is_temporary_key_id(k) {
                return Err(invalid(format!("{k} is a temporary key; trust names permanent keys")));
            }
            if store.access_key(k)?.is_none() {
                return Err(invalid(format!("no access key {k}")));
            }
        }
        if t.oidc.len() > MAX_OIDC_PROVIDERS {
            return Err(invalid(format!("a role trusts at most {MAX_OIDC_PROVIDERS} OIDC providers")));
        }
        for p in &t.oidc {
            let url_ok = |u: &str| (u.starts_with("https://") || u.starts_with("http://")) && u.len() <= 512 && !u.contains(char::is_whitespace);
            if !url_ok(&p.issuer) {
                return Err(invalid(format!("OIDC issuer {:?} must be an https:// URL", p.issuer)));
            }
            if let Some(j) = &p.jwks_uri
                && !url_ok(j)
            {
                return Err(invalid(format!("jwks_uri {j:?} must be an https:// URL")));
            }
            if p.audiences.is_empty() || p.audiences.len() > 100 || p.audiences.iter().any(|a| a.is_empty() || a.len() > 255) {
                return Err(invalid("each OIDC provider needs 1-100 audiences (client ids) of 1-255 characters"));
            }
            if p.subject.as_ref().is_some_and(|s| s.is_empty() || s.len() > 255) {
                return Err(invalid("subject patterns are 1-255 characters"));
            }
        }
        if let Some(e) = &t.external_id {
            validate_external_id(e).map_err(invalid)?;
        }
        let max = self.max_session_s.unwrap_or(ROLE_DEFAULT_SESSION_S);
        if !(MIN_SESSION_S..=ROLE_MAX_SESSION_S).contains(&max) {
            return Err(invalid(format!("max_session_s must be between {MIN_SESSION_S} and {ROLE_MAX_SESSION_S}")));
        }
        Ok(max)
    }
}

const ROLE_COLS: &str = "name, id, description, permission, buckets, policy, trust, max_session_s, generation, revoked_before, created_at, updated_at, account_id, iam";

fn role_row(r: &rusqlite::Row) -> rusqlite::Result<Role> {
    let perm: Option<String> = r.get(3)?;
    let buckets: Option<String> = r.get(4)?;
    let trust: String = r.get(6)?;
    let iam: Option<String> = r.get(13)?;
    Ok(Role {
        name: r.get(0)?,
        id: r.get(1)?,
        description: r.get(2)?,
        // A permission that no longer parses is the least one.
        permission: perm.map(|p| Permission::parse(&p).unwrap_or(Permission::Read)),
        // A scope that no longer parses covers nothing.
        buckets: buckets.map(|s| serde_json::from_str(&s).unwrap_or_default()),
        policy: r.get(5)?,
        // A trust that no longer parses trusts nobody.
        trust: serde_json::from_str(&trust).unwrap_or_default(),
        // IAM settings that no longer parse: no policies, and a trust policy that trusts nobody.
        iam: match iam {
            Some(s) => serde_json::from_str(&s).unwrap_or_else(|_| RoleIam { trust_policy: Some("{}".into()), ..Default::default() }),
            None => RoleIam::default(),
        },
        max_session_s: r.get(7)?,
        generation: r.get(8)?,
        revoked_before: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
        account: r.get(12)?,
    })
}

pub(crate) fn find_role(c: &Connection, account: &str, name: &str) -> Result<Option<Role>> {
    Ok(c.prepare_cached(&format!("SELECT {ROLE_COLS} FROM roles WHERE account_id = ?1 AND name = ?2"))?.query_row([account, name], role_row).optional()?)
}

/// What an IAM `CreateRole` sets.
#[derive(Clone, Debug, Default)]
pub struct IamRoleSpec {
    pub path: String,
    pub trust_policy: String,
    pub description: String,
    pub max_session_s: Option<i64>,
    pub boundary: Option<String>,
    pub tags: Vec<(String, String)>,
}

// ---- Sessions ----

/// The role a session belongs to, as it was when the session was issued.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRole {
    #[serde(rename = "n")]
    pub name: String,
    #[serde(rename = "i")]
    pub id: String,
    #[serde(rename = "g")]
    pub generation: i64,
}

/// A web identity a session was issued for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebIdentity {
    /// The token's `iss`.
    #[serde(rename = "i")]
    pub issuer: String,
    #[serde(rename = "s")]
    pub subject: String,
    /// The `aud` the role accepted.
    #[serde(rename = "a")]
    pub audience: String,
}

impl WebIdentity {
    /// See [`provider_name`].
    pub fn provider(&self) -> String {
        provider_name(&self.issuer)
    }
}

/// A session's claims: everything its token carries.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    #[serde(rename = "ak")]
    pub access_key_id: String,
    #[serde(rename = "sk")]
    pub secret: String,
    /// Unix ms.
    #[serde(rename = "iat")]
    pub issued_at: i64,
    #[serde(rename = "exp")]
    pub expires_at: i64,
    /// `GetSessionToken`: the key it's a session of. `AssumeRole`: the key that assumed the role.
    #[serde(rename = "k", default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Role sessions only.
    #[serde(rename = "r", default, skip_serializing_if = "Option::is_none")]
    pub role: Option<SessionRole>,
    /// `RoleSessionName`.
    #[serde(rename = "n", default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Role sessions: the role's permission and scope, narrowed by the request (never more).
    /// `GetSessionToken` sessions use their key's current ones instead.
    #[serde(rename = "p")]
    pub permission: Permission,
    #[serde(rename = "b", default, skip_serializing_if = "Option::is_none")]
    pub buckets: Option<Vec<String>>,
    /// The inline session policy (minified JSON): an upper bound on what the session may do.
    #[serde(rename = "sp", default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    /// Session tags (`aws:PrincipalTag/<key>`).
    #[serde(rename = "t", default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<(String, String)>,
    #[serde(rename = "tt", default, skip_serializing_if = "Vec::is_empty")]
    pub transitive_tag_keys: Vec<String>,
    #[serde(rename = "si", default, skip_serializing_if = "Option::is_none")]
    pub source_identity: Option<String>,
    #[serde(rename = "w", default, skip_serializing_if = "Option::is_none")]
    pub web: Option<WebIdentity>,
    /// The account the session acts in: its role's, or its key's. Empty in tokens from before
    /// accounts: the default account.
    #[serde(rename = "a", default, skip_serializing_if = "String::is_empty")]
    pub account: String,
    /// The IAM user behind a `GetSessionToken` or `GetFederationToken` session.
    #[serde(rename = "u", default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// `GetFederationToken`: the federated user's name.
    #[serde(rename = "fu", default, skip_serializing_if = "Option::is_none")]
    pub federated: Option<String>,
    /// When MFA was presented for the session (Unix ms): `aws:MultiFactorAuthPresent` and
    /// `aws:MultiFactorAuthAge`.
    #[serde(rename = "m", default, skip_serializing_if = "Option::is_none")]
    pub mfa_at: Option<i64>,
    /// Managed session policies (`PolicyArns`), read when a request is checked.
    #[serde(rename = "pa", default, skip_serializing_if = "Vec::is_empty")]
    pub policy_arns: Vec<String>,
    /// The role as it is now, read when a request is checked (not in the token).
    #[serde(skip)]
    pub role_now: Option<Arc<Role>>,
    /// The session has no permissions at all: its role was deleted (or replaced), its
    /// sessions were revoked, or the key behind it is gone or inactive. Requests are
    /// `AccessDenied`, as on AWS.
    #[serde(skip)]
    pub dead: bool,
}

// The secret stays out of logs and panics.
impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("access_key_id", &self.access_key_id)
            .field("issued_at", &self.issued_at)
            .field("expires_at", &self.expires_at)
            .field("key", &self.key)
            .field("role", &self.role)
            .field("name", &self.name)
            .field("permission", &self.permission)
            .field("buckets", &self.buckets)
            .field("account", &self.account)
            .field("user", &self.user)
            .field("federated", &self.federated)
            .field("dead", &self.dead)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// A session to be minted ([`Store::mint_session`] fills in the key, secret and times).
    pub fn new(permission: Permission) -> Self {
        Self {
            access_key_id: String::new(),
            secret: String::new(),
            issued_at: 0,
            expires_at: 0,
            key: None,
            role: None,
            name: None,
            permission,
            buckets: None,
            policy: None,
            tags: Vec::new(),
            transitive_tag_keys: Vec::new(),
            source_identity: None,
            web: None,
            account: String::new(),
            user: None,
            federated: None,
            mfa_at: None,
            policy_arns: Vec::new(),
            role_now: None,
            dead: false,
        }
    }

    /// `arn:aws:iam::<account>:role<path>/<role>` for role sessions (the path of the role as it
    /// is now, if it still exists).
    pub fn role_arn(&self, account: &str) -> Option<String> {
        self.role.as_ref().map(|r| match &self.role_now {
            Some(now) => now.arn(account),
            None => role_arn(account, &r.name),
        })
    }

    /// `arn:aws:sts::<account>:federated-user/<name>` for `GetFederationToken` sessions.
    pub fn federated_arn(&self, account: &str) -> Option<String> {
        self.federated.as_ref().map(|n| format!("arn:aws:sts::{account}:federated-user/{n}"))
    }

    /// `arn:aws:sts::<account>:assumed-role/<role>/<session name>` for role sessions.
    pub fn assumed_role_arn(&self, account: &str) -> Option<String> {
        self.role.as_ref().map(|r| format!("arn:aws:sts::{account}:assumed-role/{}/{}", r.name, self.name.as_deref().unwrap_or_default()))
    }

    /// `aws:userid` of a role session: `<role id>:<session name>`.
    pub fn user_id(&self) -> Option<String> {
        self.role.as_ref().map(|r| format!("{}:{}", r.id, self.name.as_deref().unwrap_or_default()))
    }
}

/// Credentials as handed out.
#[derive(Clone, Serialize)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: String,
    /// Unix ms.
    pub expiration: i64,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").field("access_key_id", &self.access_key_id).field("expiration", &self.expiration).finish_non_exhaustive()
    }
}

/// Why a session token doesn't authenticate.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// Not a token of this deployment, tampered with, sealed with a retired key, or not for
    /// this access key id.
    #[error("the session token is malformed or otherwise invalid")]
    Invalid,
    #[error("the session token has expired")]
    Expired,
    /// Its role was deleted or changed, its sessions were revoked, or its key was deleted.
    #[error("the session has been revoked")]
    Revoked,
    #[error(transparent)]
    Store(#[from] StoreError),
}

// ---- The session-token keys ----

#[derive(Clone, Serialize, Deserialize)]
struct KeyEntry {
    id: u32,
    /// 32 bytes, hex.
    key: String,
    created_at: i64,
}

impl Drop for KeyEntry {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct SessionKeys {
    current: KeyEntry,
    previous: Option<KeyEntry>,
}

impl KeyEntry {
    fn new(id: u32, now: i64) -> Result<Self> {
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).map_err(|e| StoreError::internal(format!("randomness: {e}")))?;
        let key = hex::encode(k);
        k.zeroize();
        Ok(Self { id, key, created_at: now })
    }

    fn bytes(&self) -> Option<[u8; 32]> {
        let mut out = [0u8; 32];
        hex::decode_to_slice(&self.key, &mut out).ok()?;
        Some(out)
    }
}

impl SessionKeys {
    fn usable(&self) -> Vec<(u32, [u8; 32])> {
        std::iter::once(&self.current).chain(self.previous.as_ref()).filter_map(|e| Some((e.id, e.bytes()?))).collect()
    }
}

fn read_keys(c: &Connection) -> Result<Option<SessionKeys>> {
    let v: Option<String> = c.prepare_cached("SELECT v FROM state WHERE k = ?1")?.query_row([KEYS_STATE], |r| r.get(0)).optional()?;
    Ok(match v {
        Some(v) => Some(serde_json::from_str(&v).map_err(|_| StoreError::internal("the session-token keys don't parse"))?),
        None => None,
    })
}

fn subkey(master: &[u8; 32], salt: &[u8]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(master).expect("hmac takes any key length");
    m.update(b"roll sts session token v1\0");
    m.update(salt);
    m.finalize().into_bytes().into()
}

/// Encrypt and authenticate `claims` under the key `kid`.
pub(crate) fn seal(kid: u32, master: &[u8; 32], claims: &[u8]) -> String {
    let mut header = [0u8; HEADER];
    header[0] = VERSION;
    header[1..5].copy_from_slice(&kid.to_be_bytes());
    getrandom::fill(&mut header[5..]).expect("os randomness");
    let mut k = subkey(master, &header[5..21]);
    let cipher = Aes256Gcm::new_from_slice(&k).expect("32-byte key");
    k.zeroize();
    let ct = cipher.encrypt(Nonce::from_slice(&header[21..33]), Payload { msg: claims, aad: &header }).expect("AES-GCM encryption doesn't fail");
    let mut out = Vec::with_capacity(HEADER + ct.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&ct);
    URL_SAFE_NO_PAD.encode(out)
}

/// The claims of a token sealed with one of `keys`, if it's authentic.
pub(crate) fn open(keys: &[(u32, [u8; 32])], token: &str) -> Option<Vec<u8>> {
    if token.len() > MAX_TOKEN_BYTES || token.len() < (HEADER + TAG) * 4 / 3 {
        return None;
    }
    let raw = URL_SAFE_NO_PAD.decode(token.trim().as_bytes()).ok()?;
    if raw.len() < HEADER + TAG || raw[0] != VERSION {
        return None;
    }
    let kid = u32::from_be_bytes(raw[1..5].try_into().ok()?);
    let (_, master) = keys.iter().find(|(id, _)| *id == kid)?;
    let mut k = subkey(master, &raw[5..21]);
    let cipher = Aes256Gcm::new_from_slice(&k).ok()?;
    k.zeroize();
    cipher.decrypt(Nonce::from_slice(&raw[21..33]), Payload { msg: &raw[HEADER..], aad: &raw[..HEADER] }).ok()
}

/// IAM role names are unique per account regardless of case.
fn role_name_taken(c: &Connection, account: &str, name: &str) -> Result<bool> {
    Ok(c.prepare_cached("SELECT 1 FROM roles WHERE account_id = ?1 AND lower(name) = lower(?2)")?.exists([account, name])?)
}

fn not_found_role(e: StoreError, name: &str) -> StoreError {
    match e {
        StoreError::NotFound(_) => StoreError::NotFound(format!("no role {name}")),
        e => e,
    }
}

/// The buckets both scopes cover (`None` covers all).
fn intersect(a: Option<Vec<String>>, b: Option<Vec<String>>) -> Option<Vec<String>> {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(a), Some(b)) => Some(b.into_iter().filter(|x| a.contains(x)).collect()),
    }
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Store {
    // ---- Roles ----

    /// The default account's roles.
    pub fn roles(&self) -> Result<Vec<Role>> {
        self.roles_in(&self.default_account_id())
    }

    pub fn roles_in(&self, account: &str) -> Result<Vec<Role>> {
        self.read(|c| Ok(c.prepare(&format!("SELECT {ROLE_COLS} FROM roles WHERE account_id = ?1 ORDER BY name"))?.query_map([account], role_row)?.collect::<rusqlite::Result<Vec<_>>>()?))
    }

    /// A role of the default account.
    pub fn role(&self, name: &str) -> Result<Option<Role>> {
        self.role_in(&self.default_account_id(), name)
    }

    pub fn role_in(&self, account: &str, name: &str) -> Result<Option<Role>> {
        self.read(|c| find_role(c, account, name))
    }

    /// A control-plane role in the default account.
    pub async fn create_role(&self, name: &str, spec: RoleSpec) -> Result<Role> {
        self.create_role_in(&self.default_account_id(), name, spec).await
    }

    pub async fn create_role_in(&self, account: &str, name: &str, spec: RoleSpec) -> Result<Role> {
        validate_role_name(name).map_err(invalid)?;
        let max = spec.check(self)?;
        let now = self.now();
        let id = ids::role_id();
        self.write(|c| {
            if crate::accounts::find(c, account)?.is_none() {
                return Err(StoreError::NotFound(format!("no account {account}")));
            }
            if role_name_taken(c, account, name)? {
                return Err(StoreError::Conflict(format!("a role named {name} already exists")));
            }
            let buckets = spec.buckets.as_ref().map(rows::json).transpose()?;
            c.execute(
                &format!("INSERT INTO roles ({ROLE_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, 0, ?9, ?9, ?10, NULL)"),
                params![name, id, spec.description, spec.permission.as_str(), buckets, spec.policy, rows::json(&spec.trust)?, max, now, account],
            )?;
            Ok(())
        })
        .await?;
        self.role_in(account, name)?.ok_or_else(|| StoreError::internal("role vanished"))
    }

    /// An IAM role (`CreateRole`): no roll permission level, a trust policy document (the
    /// caller checked it).
    pub async fn create_iam_role(&self, account: &str, name: &str, spec: IamRoleSpec) -> Result<Role> {
        validate_role_name(name).map_err(invalid)?;
        let max = spec.max_session_s.unwrap_or(ROLE_DEFAULT_SESSION_S);
        if !(MIN_SESSION_S..=ROLE_MAX_SESSION_S).contains(&max) {
            return Err(invalid(format!("MaxSessionDuration must be between {MIN_SESSION_S} and {ROLE_MAX_SESSION_S}")));
        }
        let now = self.now();
        let id = crate::iam::entity_id("AROA");
        let iam = RoleIam { path: spec.path, trust_policy: Some(spec.trust_policy), boundary: spec.boundary, tags: spec.tags, ..Default::default() };
        self.write(|c| {
            if crate::accounts::find(c, account)?.is_none() {
                return Err(StoreError::NotFound(format!("no account {account}")));
            }
            if role_name_taken(c, account, name)? {
                return Err(StoreError::Conflict(format!("Role with name {name} already exists.")));
            }
            let roles: i64 = c.query_row("SELECT count(*) FROM roles WHERE account_id = ?1", [account], |r| r.get(0))?;
            if roles as usize >= crate::iam::MAX_ROLES {
                return Err(StoreError::LimitExceeded(format!("Cannot exceed quota for RolesPerAccount: {}", crate::iam::MAX_ROLES)));
            }
            c.execute(
                &format!("INSERT INTO roles ({ROLE_COLS}) VALUES (?1, ?2, ?3, NULL, NULL, NULL, ?4, ?5, 1, 0, ?6, ?6, ?7, ?8)"),
                params![name, id, spec.description, rows::json(&Trust::default())?, max, now, account, rows::json(&iam)?],
            )?;
            Ok(())
        })
        .await?;
        self.role_in(account, name)?.ok_or_else(|| StoreError::internal("role vanished"))
    }

    /// Change a role in one transaction (`f` edits it as it is). Its sessions keep working,
    /// with the role as it is now.
    pub async fn edit_role(&self, account: &str, name: &str, f: impl FnOnce(&Connection, &mut Role) -> Result<()>) -> Result<Role> {
        let now = self.now();
        self.write(|c| {
            let mut r = find_role(c, account, name)?.ok_or_else(|| StoreError::NotFound(format!("The role with name {name} cannot be found.")))?;
            f(c, &mut r)?;
            if !(MIN_SESSION_S..=ROLE_MAX_SESSION_S).contains(&r.max_session_s) {
                return Err(invalid(format!("MaxSessionDuration must be between {MIN_SESSION_S} and {ROLE_MAX_SESSION_S}")));
            }
            let buckets = r.buckets.as_ref().map(rows::json).transpose()?;
            let iam = if r.iam == RoleIam::default() { None } else { Some(rows::json(&r.iam)?) };
            c.execute(
                "UPDATE roles SET description = ?3, permission = ?4, buckets = ?5, policy = ?6, trust = ?7, max_session_s = ?8, iam = ?9,
                   revoked_before = ?10, generation = generation + 1, updated_at = ?11 WHERE account_id = ?1 AND name = ?2",
                params![account, name, r.description, r.permission.map(Permission::as_str), buckets, r.policy, rows::json(&r.trust)?, r.max_session_s, iam, r.revoked_before, now],
            )?;
            Ok(())
        })
        .await?;
        self.role_in(account, name)?.ok_or_else(|| StoreError::internal("role vanished"))
    }

    /// Replace a control-plane role's settings (default account). Its sessions keep working,
    /// with the new settings, as on AWS.
    pub async fn update_role(&self, name: &str, spec: RoleSpec) -> Result<Role> {
        self.update_role_in(&self.default_account_id(), name, spec).await
    }

    pub async fn update_role_in(&self, account: &str, name: &str, spec: RoleSpec) -> Result<Role> {
        let max = spec.check(self)?;
        self.edit_role(account, name, |_, r| {
            r.description = spec.description;
            r.permission = Some(spec.permission);
            r.buckets = spec.buckets;
            r.policy = spec.policy;
            r.trust = spec.trust;
            r.max_session_s = max;
            Ok(())
        })
        .await
        .map_err(|e| not_found_role(e, name))
    }

    /// Delete a role (default account); its sessions have no permissions from then on.
    pub async fn delete_role(&self, name: &str) -> Result<()> {
        self.delete_role_in(&self.default_account_id(), name).await
    }

    pub async fn delete_role_in(&self, account: &str, name: &str) -> Result<()> {
        let n = self.write(|c| Ok(c.execute("DELETE FROM roles WHERE account_id = ?1 AND name = ?2", [account, name])?)).await?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("no role {name}")));
        }
        Ok(())
    }

    /// Revoke every session of a role issued until now, as AWS does: an
    /// `AWSRevokeOlderSessions` inline policy denying sessions issued before now (and
    /// `revoked_before`). New sessions still work.
    pub async fn revoke_role_sessions(&self, name: &str) -> Result<Role> {
        self.revoke_role_sessions_in(&self.default_account_id(), name).await
    }

    pub async fn revoke_role_sessions_in(&self, account: &str, name: &str) -> Result<Role> {
        let now = self.now();
        self.edit_role(account, name, |_, r| {
            r.revoked_before = r.revoked_before.max(now);
            r.iam.policies.insert(REVOKE_POLICY_NAME.into(), revoke_policy(now + 1));
            Ok(())
        })
        .await
        .map_err(|e| not_found_role(e, name))
    }

    // ---- Session keys ----

    async fn ensure_session_keys(&self) -> Result<SessionKeys> {
        if let Some(k) = self.read(read_keys)? {
            return Ok(k);
        }
        let fresh = SessionKeys { current: KeyEntry::new(1, self.now())?, previous: None };
        let v = rows::json(&fresh)?;
        self.write(|c| {
            c.execute("INSERT INTO state (k, v) VALUES (?1, ?2) ON CONFLICT (k) DO NOTHING", params![KEYS_STATE, v])?;
            Ok(())
        })
        .await?;
        self.read(read_keys)?.ok_or_else(|| StoreError::internal("session keys vanished"))
    }

    /// Start sealing new session tokens with a new key. Tokens sealed with the key before
    /// keep working; older ones stop. Returns the new key's id.
    pub async fn rotate_session_key(&self) -> Result<u32> {
        self.ensure_session_keys().await?;
        let now = self.now();
        self.write(|c| {
            let old = read_keys(c)?.ok_or_else(|| StoreError::internal("no session keys"))?;
            let id = old.current.id.wrapping_add(1).max(1);
            let next = SessionKeys { current: KeyEntry::new(id, now)?, previous: Some(old.current.clone()) };
            c.execute("UPDATE state SET v = ?2 WHERE k = ?1", params![KEYS_STATE, rows::json(&next)?])?;
            Ok(id)
        })
        .await
    }

    /// The id of the key new session tokens are sealed with (`None` before the first session).
    pub fn session_key_id(&self) -> Result<Option<u32>> {
        Ok(self.read(read_keys)?.map(|k| k.current.id))
    }

    // ---- Sessions ----

    /// Issue credentials for `session` (its access key id, secret and times are filled in
    /// here), valid for `duration_s`. Nothing is stored.
    pub async fn mint_session(&self, mut session: Session, duration_s: i64) -> Result<(Session, Credentials)> {
        if duration_s < 1 {
            return Err(invalid("session durations are at least a second"));
        }
        let keys = self.ensure_session_keys().await?;
        let now = self.now();
        session.access_key_id = ids::temporary_key_id();
        session.secret = ids::secret_key();
        session.issued_at = now;
        session.expires_at = now + duration_s * 1000;
        session.role_now = None;
        session.dead = false;
        if session.account.is_empty() {
            session.account = self.default_account_id();
        }
        let master = keys.current.bytes().ok_or_else(|| StoreError::internal("the session-token key doesn't parse"))?;
        let mut claims = serde_json::to_vec(&session)?;
        let token = seal(keys.current.id, &master, &claims);
        claims.zeroize();
        if token.len() > MAX_TOKEN_BYTES {
            return Err(invalid("the session's policy and tags are too large"));
        }
        let creds = Credentials { access_key_id: session.access_key_id.clone(), secret_access_key: session.secret.clone(), session_token: token, expiration: session.expires_at };
        Ok((session, creds))
    }

    /// The temporary key `access_key_id` with session token `token`, if the token is
    /// authentic, for that key and unexpired. A session whose role or key is gone, or that was
    /// revoked, still authenticates (as on AWS) but is [`Session::dead`]: it may do nothing.
    pub fn session_key(&self, access_key_id: &str, token: &str) -> std::result::Result<AccessKey, SessionError> {
        if token.len() > MAX_TOKEN_BYTES {
            return Err(SessionError::Invalid);
        }
        let keys = self.read(read_keys)?.ok_or(SessionError::Invalid)?;
        let mut claims = open(&keys.usable(), token).ok_or(SessionError::Invalid)?;
        let parsed = serde_json::from_slice::<Session>(&claims);
        claims.zeroize();
        let mut s = parsed.map_err(|_| SessionError::Invalid)?;
        if !ct_eq(&s.access_key_id, access_key_id) {
            return Err(SessionError::Invalid);
        }
        if self.now() >= s.expires_at {
            return Err(SessionError::Expired);
        }
        if s.account.is_empty() {
            s.account = self.default_account_id();
        }
        // What the session may do through roll permission levels: the role's (or key's) as it
        // is now, never more than it was narrowed to when minted. An empty bucket scope covers
        // nothing: no permission level at all.
        let nothing = || (Permission::Read, Some(Vec::new()));
        let (permission, buckets) = match (&s.role, &s.key) {
            (Some(r), _) => match self.read(|c| find_role(c, &s.account, &r.name))? {
                Some(role) if role.id == r.id => {
                    if s.issued_at <= role.revoked_before {
                        s.dead = true;
                    }
                    let level = match role.permission {
                        Some(p) => (min_permission(p, s.permission), intersect(role.buckets.clone(), s.buckets.clone())),
                        None => nothing(),
                    };
                    s.role_now = Some(Arc::new(role));
                    level
                }
                _ => {
                    s.dead = true;
                    nothing()
                }
            },
            (None, Some(k)) => match self.access_key(k)? {
                Some(key) if key.active && key.account == s.account && key.user == s.user => {
                    if key.user.is_some() { nothing() } else { (key.permission, key.buckets) }
                }
                _ => {
                    s.dead = true;
                    nothing()
                }
            },
            (None, None) => return Err(SessionError::Invalid),
        };
        Ok(AccessKey {
            id: s.access_key_id.clone(),
            secret: s.secret.clone(),
            name: s.name.clone().unwrap_or_else(|| "session".into()),
            permission,
            buckets,
            created_at: s.issued_at,
            last_used_at: None,
            account: s.account.clone(),
            user: s.user.clone(),
            active: true,
            session: Some(Arc::new(s)),
        })
    }
}

/// The narrower of two permissions.
pub fn min_permission(a: Permission, b: Permission) -> Permission {
    let rank = |p: Permission| match p {
        Permission::Read => 0,
        Permission::Write => 1,
        Permission::Admin => 2,
    };
    if rank(a) <= rank(b) { a } else { b }
}

/// A request's bucket scope within a role's: `Err` names a bucket outside it.
pub fn narrow_buckets(role: Option<&[String]>, requested: Option<&[String]>) -> std::result::Result<Option<Vec<String>>, String> {
    match (role, requested) {
        (r, None) => Ok(r.map(<[String]>::to_vec)),
        (_, Some([])) => Err("buckets: give at least one, or leave it out for the role's".into()),
        (None, Some(q)) => Ok(Some(q.to_vec())),
        (Some(r), Some(q)) => match q.iter().find(|b| !r.contains(b)) {
            Some(b) => Err(format!("bucket {b} is outside the role's scope")),
            None => Ok(Some(q.to_vec())),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b: u8) -> [u8; 32] {
        [b; 32]
    }

    #[test]
    fn seal_and_open() {
        let t = seal(7, &key(1), b"{\"a\":1}");
        assert!(t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'), "URL-safe");
        assert_eq!(open(&[(7, key(1))], &t).as_deref(), Some(&b"{\"a\":1}"[..]));
        assert_ne!(seal(7, &key(1), b"x"), seal(7, &key(1), b"x"), "fresh salt and nonce every time");
        // The wrong key, an unknown key id, and garbage.
        assert_eq!(open(&[(7, key(2))], &t), None);
        assert_eq!(open(&[(8, key(1))], &t), None);
        assert_eq!(open(&[(7, key(1))], "not a token"), None);
        assert_eq!(open(&[(7, key(1))], ""), None);
        assert_eq!(open(&[(7, key(1))], &"A".repeat(MAX_TOKEN_BYTES + 1)), None);
    }

    #[test]
    fn tampering_is_detected() {
        let t = seal(1, &key(9), b"{\"perm\":\"read\"}");
        let raw = URL_SAFE_NO_PAD.decode(&t).unwrap();
        // Flip each byte in turn: header (version, key id, salt, nonce), ciphertext and tag.
        for i in 0..raw.len() {
            let mut r = raw.clone();
            r[i] ^= 0x01;
            assert_eq!(open(&[(1, key(9)), (0x0100_0001, key(9))], &URL_SAFE_NO_PAD.encode(&r)), None, "byte {i}");
        }
        // Truncated or extended.
        assert_eq!(open(&[(1, key(9))], &URL_SAFE_NO_PAD.encode(&raw[..raw.len() - 1])), None);
        let mut longer = raw.clone();
        longer.push(0);
        assert_eq!(open(&[(1, key(9))], &URL_SAFE_NO_PAD.encode(&longer)), None);
    }

    #[test]
    fn rotation_keeps_the_previous_key() {
        let k1 = SessionKeys { current: KeyEntry { id: 1, key: hex::encode(key(1)), created_at: 0 }, previous: None };
        let t1 = seal(1, &key(1), b"one");
        let k2 = SessionKeys { current: KeyEntry { id: 2, key: hex::encode(key(2)), created_at: 1 }, previous: Some(k1.current.clone()) };
        let t2 = seal(2, &key(2), b"two");
        assert_eq!(open(&k2.usable(), &t1).as_deref(), Some(&b"one"[..]), "the previous key still opens");
        assert_eq!(open(&k2.usable(), &t2).as_deref(), Some(&b"two"[..]));
        assert_eq!(open(&k1.usable(), &t2), None, "a newer key isn't known before the rotation");
        let k3 = SessionKeys { current: KeyEntry { id: 3, key: hex::encode(key(3)), created_at: 2 }, previous: Some(k2.current.clone()) };
        assert_eq!(open(&k3.usable(), &t1), None, "two rotations retire a key");
        assert_eq!(open(&k3.usable(), &t2).as_deref(), Some(&b"two"[..]));
    }

    #[test]
    fn names_and_tags() {
        assert!(validate_role_name("uploader").is_ok());
        assert!(validate_role_name("a+b=c,d.e@f-g_h").is_ok());
        assert!(validate_role_name("").is_err());
        assert!(validate_role_name(&"a".repeat(65)).is_err());
        assert!(validate_role_name("no/slash").is_err());
        assert!(validate_session_name("x").is_err());
        assert!(validate_session_name("web-123").is_ok());
        assert!(validate_source_identity("aws:x").is_err());
        assert!(validate_external_id("a").is_err());
        assert!(validate_external_id("arn:aws:x/y").is_ok());
        let t = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert!(validate_tags(&[t("team", "a b"), t("Project", "x:y")], &["team".into()]).is_ok());
        assert!(validate_tags(&[t("a", "1"), t("A", "2")], &[]).is_err(), "case-insensitive keys");
        assert!(validate_tags(&[t("aws:x", "1")], &[]).is_err());
        assert!(validate_tags(&[t("a", "1")], &["b".into()]).is_err());
        assert!(validate_tags(&(0..51).map(|i| t(&format!("k{i}"), "")).collect::<Vec<_>>(), &[]).is_err());
        assert!(validate_tags(&(0..20).map(|i| t(&format!("k{i}"), &"v".repeat(200))).collect::<Vec<_>>(), &[]).is_err(), "byte cap");
    }

    #[test]
    fn narrowing() {
        use Permission::*;
        assert_eq!(min_permission(Admin, Read), Read);
        assert_eq!(min_permission(Write, Admin), Write);
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(narrow_buckets(None, None), Ok(None));
        assert_eq!(narrow_buckets(Some(&s(&["a"])), None), Ok(Some(s(&["a"]))));
        assert_eq!(narrow_buckets(None, Some(&s(&["x"]))), Ok(Some(s(&["x"]))));
        assert_eq!(narrow_buckets(Some(&s(&["a", "b"])), Some(&s(&["b"]))), Ok(Some(s(&["b"]))));
        assert!(narrow_buckets(Some(&s(&["a"])), Some(&s(&["b"]))).is_err());
        assert!(narrow_buckets(None, Some(&[])).is_err());
        assert_eq!(provider_name("https://accounts.google.com/"), "accounts.google.com");
        assert_eq!(provider_name("https://x.auth0.com/tenant/"), "x.auth0.com/tenant");
    }

    #[test]
    fn debug_hides_secrets() {
        let mut s = Session::new(Permission::Read);
        s.secret = "supersecret".into();
        assert!(!format!("{s:?}").contains("supersecret"));
        let c = Credentials { access_key_id: "RT".into(), secret_access_key: "supersecret".into(), session_token: "tok".into(), expiration: 0 };
        assert!(!format!("{c:?}").contains("supersecret") && !format!("{c:?}").contains("tok"));
    }
}
