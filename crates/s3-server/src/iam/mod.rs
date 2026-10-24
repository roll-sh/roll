//! The IAM query API on the S3 listener (service `iam`), like STS: `POST /` with
//! `Action=…&Version=2010-05-08`, SigV4 with service `iam` (any region), answered in IAM's XML,
//! errors in IAM's `<ErrorResponse>` with IAM's codes (`NoSuchEntity`, `EntityAlreadyExists`,
//! `MalformedPolicyDocument`, `LimitExceeded`, `DeleteConflict`, `ValidationError`,
//! `AccessDenied`). SDKs reach it with `endpoint_url` for `iam`.
//!
//! Every call acts in the caller's account and needs `iam:<Action>` on the entity's ARN from
//! the caller's identity policies; the account's root user may do everything. Federated users
//! can't call IAM, and `GetSessionToken` sessions only with MFA, as on AWS.
//!
//! - [`users`]: users, groups, access keys, inline policies, permissions boundaries, tags.
//! - [`policies`]: customer-managed policies and their versions, attachments; AWS's
//!   `AmazonS3FullAccess` and `AmazonS3ReadOnlyAccess` are built in.
//! - [`roles`]: roles with trust policies (`AssumeRolePolicyDocument`).
//! - [`account`]: virtual MFA devices, OpenID Connect providers, `GetAccountSummary`.
//! - [`login`]: login profiles (console passwords), the password policy, `ChangePassword`,
//!   account aliases, and signing in with a password (`console.rs`).
//! - [`saml`]: SAML identity providers (`AssumeRoleWithSAML` is `sts/saml.rs`).
//! - [`certs`]: server and signing certificates, SSH public keys.
//! - [`simulate`]: the policy simulator, on the real evaluator.
//! - [`report`]: `GetAccountAuthorizationDetails` and the credential report.

mod account;
mod certs;
pub(crate) mod login;
mod policies;
mod report;
pub(crate) mod roles;
mod saml;
mod simulate;
mod users;

use crate::ctx::Ctx;
use crate::identity::{Identity, Kind};
use crate::policy::Policy;
use crate::sts::{Params, StsError, access_denied, validation};
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use s3_proto::xml::escape;
use s3_store::{Store, StoreError};
use std::sync::Arc;

pub(crate) const XMLNS: &str = "https://iam.amazonaws.com/doc/2010-05-08/";
const VERSION: &str = "2010-05-08";
/// A role trust policy's largest size (IAM's default quota is 2048 characters; up to 4096 on
/// request).
pub(crate) const MAX_TRUST_POLICY_BYTES: usize = 4096;
/// The control plane's own key never shows through IAM.
const CONTROL_KEY_PREFIX: &str = "RKROLLCONTROLPLANE";

/// Whether an STS-shaped request (see [`crate::sts::is_sts`]) is for IAM: signed for service
/// `iam` (the `Authorization` header's or a presigned URL's credential scope).
pub(crate) fn is_iam(ctx: &Ctx) -> bool {
    let scope = ctx.h("authorization").and_then(|a| a.split("Credential=").nth(1)).or_else(|| ctx.q("X-Amz-Credential"));
    scope.is_some_and(|c| c.split([',', ' ']).next().unwrap_or_default().split('/').nth(3) == Some("iam"))
}

/// An IAM error from the store's.
pub(crate) fn store_error(e: StoreError) -> StsError {
    match e {
        StoreError::NotFound(m) => StsError::new("NoSuchEntity", 404, m),
        StoreError::Conflict(m) => StsError::new("EntityAlreadyExists", 409, m),
        StoreError::LimitExceeded(m) => StsError::new("LimitExceeded", 409, m),
        StoreError::DeleteConflict(m) => StsError::new("DeleteConflict", 409, m),
        StoreError::InvalidArgument(m) | StoreError::InvalidRequest(m) => validation(m),
        e => crate::sts::store_failure(e),
    }
}

pub(crate) fn no_such(m: impl Into<String>) -> StsError {
    StsError::new("NoSuchEntity", 404, m)
}

pub(crate) fn exists(m: impl Into<String>) -> StsError {
    StsError::new("EntityAlreadyExists", 409, m)
}

pub(crate) fn limit(m: impl Into<String>) -> StsError {
    StsError::new("LimitExceeded", 409, m)
}

pub(crate) fn delete_conflict(m: impl Into<String>) -> StsError {
    StsError::new("DeleteConflict", 409, m)
}

pub(crate) fn invalid_input(m: impl Into<String>) -> StsError {
    StsError::new("InvalidInput", 400, m)
}

/// One IAM call.
pub(crate) struct Call<'a> {
    pub ctx: &'a Ctx,
    pub store: &'a Arc<Store>,
    pub id: &'a Identity,
    pub p: &'a Params,
    /// The caller's account (12 digits).
    pub account: String,
}

/// IAM's name rules: `[\w+=,.@-]+`, 1 to `max` characters.
pub(crate) fn valid_name(n: &str, max: usize) -> bool {
    !n.is_empty() && n.len() <= max && n.chars().all(|c| c.is_ascii_alphanumeric() || "_+=,.@-".contains(c))
}

/// An ISO 8601 date as IAM writes them (`2026-01-02T03:04:05Z`).
pub(crate) fn date(ms: i64) -> String {
    let s = s3_proto::time::iso8601_ms(ms.div_euclid(1000) * 1000);
    s.strip_suffix(".000Z").map_or(s.clone(), |b| format!("{b}Z"))
}

/// RFC 3986 percent-encoding, as IAM returns policy documents.
pub(crate) fn url_encode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

pub(crate) fn el(name: &str, value: &str) -> String {
    format!("<{name}>{}</{name}>", escape(value))
}

pub(crate) fn tags_xml(tags: &[(String, String)]) -> String {
    if tags.is_empty() {
        return String::new();
    }
    let members: String = tags.iter().map(|(k, v)| format!("<member>{}{}</member>", el("Key", k), el("Value", v))).collect();
    format!("<Tags>{members}</Tags>")
}

pub(crate) fn boundary_xml(b: &Option<String>) -> String {
    b.as_ref().map_or_else(String::new, |arn| format!("<PermissionsBoundary><PermissionsBoundaryType>Policy</PermissionsBoundaryType>{}</PermissionsBoundary>", el("PermissionsBoundaryArn", arn)))
}

/// A list's members and paging (`Marker`, `MaxItems`): items after the marker, at most
/// `MaxItems` (default 100, at most 1000).
pub(crate) fn page<T>(p: &Params, items: Vec<T>, key: impl Fn(&T) -> String, render: impl Fn(&T) -> String, list: &str) -> Result<String, StsError> {
    let max = match p.get("MaxItems") {
        None => 100,
        Some(m) => m.trim().parse::<usize>().ok().filter(|n| (1..=1000).contains(n)).ok_or_else(|| validation("1 validation error detected: Value at 'maxItems' failed to satisfy constraint: Member must have value between 1 and 1000"))?,
    };
    let marker = p.get("Marker");
    let mut rest: Vec<T> = items.into_iter().filter(|i| marker.is_none_or(|m| key(i).as_str() > m)).collect();
    let truncated = rest.len() > max;
    rest.truncate(max);
    let members: String = rest.iter().map(|i| format!("<member>{}</member>", render(i))).collect();
    let mut out = format!("<{list}>{members}</{list}><IsTruncated>{truncated}</IsTruncated>");
    if truncated && let Some(last) = rest.last() {
        out.push_str(&el("Marker", &key(last)));
    }
    Ok(out)
}

impl Call<'_> {
    /// The caller may do `iam:<action>` on `resource`.
    pub fn allow(&self, action: &str, resource: &str) -> Result<(), StsError> {
        let cond = IamCond { ctx: self.ctx, id: self.id };
        if self.id.verdict(&format!("iam:{action}"), resource, &cond, false).allowed() {
            Ok(())
        } else {
            Err(access_denied(format!("User: {} is not authorized to perform: iam:{action} on resource: {resource}", self.id.caller_arn)))
        }
    }

    /// A required name parameter.
    pub fn name(&self, param: &str, max: usize) -> Result<&str, StsError> {
        let field = lower_first(param);
        let v = self.p.required(param, &field)?;
        if !valid_name(v, max) {
            return Err(validation(format!("1 validation error detected: Value '{v}' at '{field}' failed to satisfy constraint: Member must satisfy regular expression pattern: [\\w+=,.@-]+ and have length between 1 and {max}")));
        }
        Ok(v)
    }

    /// `Path` (default `/`): `/` or `/a/b/`, at most 512 characters.
    pub fn path(&self, param: &str) -> Result<String, StsError> {
        let Some(p) = self.p.get(param) else { return Ok("/".into()) };
        if p.is_empty() || p.len() > 512 || !p.starts_with('/') || !p.ends_with('/') || p.contains("//") || !p.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
            return Err(validation(format!("1 validation error detected: Value '{p}' at '{}' failed to satisfy constraint: Member must satisfy regular expression pattern: (\\u002F)|(\\u002F[\\u0021-\\u007F]+\\u002F)", lower_first(param))));
        }
        Ok(p.to_string())
    }

    /// `Tags.member.N` (IAM's rules: at most 50; keys 1-128, values up to 256 characters).
    pub fn tags(&self) -> Result<Vec<(String, String)>, StsError> {
        let tags = self.p.tags("Tags")?;
        check_tags(&tags)?;
        Ok(tags)
    }

    /// An identity policy document parameter, minified: `MalformedPolicyDocument` if it isn't
    /// one.
    pub fn document(&self, param: &str, max: usize) -> Result<String, StsError> {
        let doc = self.p.required(param, &lower_first(param))?;
        if doc.len() > max {
            return Err(limit(format!("Cannot exceed quota for PolicySize: {max}")));
        }
        Policy::parse_identity(doc.as_bytes(), max).map_err(|e| StsError::new("MalformedPolicyDocument", 400, e.message))?;
        serde_json::from_str::<serde_json::Value>(doc).map(|v| v.to_string()).map_err(|e| StsError::new("MalformedPolicyDocument", 400, e.to_string()))
    }

    pub fn arn(&self, kind: &str, path: &str, name: &str) -> String {
        let path = if path.is_empty() { "/" } else { path };
        format!("arn:aws:iam::{}:{kind}{path}{name}", self.account)
    }

    /// The user a call names, or the caller itself (`None`: the root user).
    pub fn user_or_self(&self) -> Result<Option<String>, StsError> {
        match self.p.get("UserName") {
            Some(_) => Ok(Some(self.name("UserName", 64)?.to_string())),
            None => match &self.id.kind {
                Kind::Root => Ok(None),
                Kind::User { name, .. } => Ok(Some(name.clone())),
                _ => Err(validation("Must specify userName when calling with non-User credentials")),
            },
        }
    }
}

pub(crate) fn check_tags(tags: &[(String, String)]) -> Result<(), StsError> {
    if tags.len() > 50 {
        return Err(limit("Cannot exceed quota for TagsPerEntity: 50"));
    }
    let mut seen = std::collections::HashSet::new();
    for (k, v) in tags {
        if k.is_empty() || k.chars().count() > 128 || v.chars().count() > 256 || k.to_ascii_lowercase().starts_with("aws:") {
            return Err(validation(format!("invalid tag {k:?}: keys are 1-128 characters (not aws:), values up to 256")));
        }
        if !seen.insert(k.to_lowercase()) {
            return Err(invalid_input(format!("Duplicate tag keys found: {k}")));
        }
    }
    Ok(())
}

/// Merge tags in (replacing values of the same key, case-insensitively).
pub(crate) fn merge_tags(have: &mut Vec<(String, String)>, add: Vec<(String, String)>) -> Result<(), StsError> {
    for (k, v) in add {
        match have.iter_mut().find(|(h, _)| h.eq_ignore_ascii_case(&k)) {
            Some(e) => e.1 = v,
            None => have.push((k, v)),
        }
    }
    check_tags(have)
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_lowercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// Condition keys of an IAM call: the caller's, and the request's context.
struct IamCond<'a> {
    ctx: &'a Ctx,
    id: &'a Identity,
}

impl crate::policy::Context for IamCond<'_> {
    fn values(&self, key: &str) -> Option<Vec<String>> {
        if let Some(v) = self.id.condition(key, self.ctx.store().now()) {
            return v;
        }
        match key {
            "aws:sourceip" => self.ctx.peer.map(|p| vec![p.to_string()]),
            "aws:securetransport" => Some(vec![crate::access::secure(self.ctx).to_string()]),
            "aws:currenttime" => Some(vec![s3_proto::time::iso8601_ms(self.ctx.store().now())]),
            "aws:epochtime" => Some(vec![self.ctx.now_s().to_string()]),
            "aws:useragent" => self.ctx.h("user-agent").map(|u| vec![u.to_string()]),
            _ => None,
        }
    }
}

/// Answer an IAM request.
pub(crate) async fn handle(ctx: &mut Ctx, body: Body) -> Response {
    let request_id = crate::sts::request_id();
    let resp = match run(ctx, body, &request_id).await {
        Ok(xml) => crate::sts::xml_response(StatusCode::OK, xml),
        Err(e) => crate::sts::error_response(XMLNS, &e, &request_id),
    };
    crate::sts::decorate(ctx, resp, &request_id)
}

async fn run(ctx: &mut Ctx, body: Body, request_id: &str) -> Result<String, StsError> {
    let (p, body) = Params::read(ctx, body).await?;
    let action = p.get("Action").ok_or_else(|| StsError::new("MissingAction", 400, "Missing Action"))?.to_string();
    if let Some(v) = p.get("Version")
        && v != VERSION
    {
        return Err(StsError::new("InvalidAction", 400, format!("Could not find operation {action} for version {v}")));
    }
    let key = crate::sts::authenticate(ctx, &body, "iam")?;
    let store = ctx.store().clone();
    let id = crate::identity::resolve(&store, &key).map_err(crate::sts::store_failure)?;
    let result = execute(ctx, &store, &id, &p, &action).await?;
    Ok(match result {
        Some(r) => format!("<{action}Response xmlns=\"{XMLNS}\"><{action}Result>{r}</{action}Result><ResponseMetadata><RequestId>{request_id}</RequestId></ResponseMetadata></{action}Response>"),
        None => format!("<{action}Response xmlns=\"{XMLNS}\"><ResponseMetadata><RequestId>{request_id}</RequestId></ResponseMetadata></{action}Response>"),
    })
}

/// Run an IAM action for `id` (authenticated already): its `Result` element's content, `None`
/// for actions without one. The dashboard's IAM pages come through here too (`console.rs`).
pub(crate) async fn execute(ctx: &Ctx, store: &Arc<Store>, id: &Identity, p: &Params, action: &str) -> Result<Option<String>, StsError> {
    // Who may call IAM at all, as on AWS.
    if matches!(id.kind, Kind::Federated { .. }) || id.dead {
        return Err(access_denied(format!("User: {} is not authorized to call IAM", id.caller_arn)));
    }
    if let Some(s) = &id.session
        && s.role.is_none()
        && s.mfa_at.is_none()
        && !s.console
    {
        return Err(StsError::new("InvalidClientTokenId", 403, "The security token included in the request is invalid: GetSessionToken credentials can call IAM only when MFA authenticated."));
    }
    let action = action.to_string();
    let call = Call { ctx, store, id, p, account: id.account.id.clone() };
    let result = match action.as_str() {
        "CreateUser" | "GetUser" | "ListUsers" | "DeleteUser" | "UpdateUser" | "TagUser" | "UntagUser" | "ListUserTags" | "PutUserPermissionsBoundary" | "DeleteUserPermissionsBoundary" | "CreateGroup" | "GetGroup"
        | "ListGroups" | "DeleteGroup" | "AddUserToGroup" | "RemoveUserFromGroup" | "ListGroupsForUser" | "CreateAccessKey" | "ListAccessKeys" | "UpdateAccessKey" | "DeleteAccessKey" | "GetAccessKeyLastUsed"
        | "PutUserPolicy" | "GetUserPolicy" | "ListUserPolicies" | "DeleteUserPolicy" | "PutGroupPolicy" | "GetGroupPolicy" | "ListGroupPolicies" | "DeleteGroupPolicy" => users::run(&call, &action).await?,
        "CreatePolicy" | "GetPolicy" | "ListPolicies" | "DeletePolicy" | "CreatePolicyVersion" | "GetPolicyVersion" | "ListPolicyVersions" | "SetDefaultPolicyVersion" | "DeletePolicyVersion" | "AttachUserPolicy"
        | "DetachUserPolicy" | "ListAttachedUserPolicies" | "AttachGroupPolicy" | "DetachGroupPolicy" | "ListAttachedGroupPolicies" | "AttachRolePolicy" | "DetachRolePolicy" | "ListAttachedRolePolicies"
        | "ListEntitiesForPolicy" => policies::run(&call, &action).await?,
        "CreateRole" | "GetRole" | "ListRoles" | "DeleteRole" | "UpdateRole" | "UpdateRoleDescription" | "UpdateAssumeRolePolicy" | "PutRolePolicy" | "GetRolePolicy" | "ListRolePolicies" | "DeleteRolePolicy"
        | "PutRolePermissionsBoundary" | "DeleteRolePermissionsBoundary" | "TagRole" | "UntagRole" | "ListRoleTags" => roles::run(&call, &action).await?,
        "CreateVirtualMFADevice" | "EnableMFADevice" | "DeactivateMFADevice" | "ListMFADevices" | "ListVirtualMFADevices" | "DeleteVirtualMFADevice" | "ResyncMFADevice" | "CreateOpenIDConnectProvider"
        | "GetOpenIDConnectProvider" | "ListOpenIDConnectProviders" | "DeleteOpenIDConnectProvider" | "GetAccountSummary" => account::run(&call, &action).await?,
        "CreateLoginProfile" | "GetLoginProfile" | "UpdateLoginProfile" | "DeleteLoginProfile" | "ChangePassword" | "GetAccountPasswordPolicy" | "UpdateAccountPasswordPolicy" | "DeleteAccountPasswordPolicy"
        | "CreateAccountAlias" | "ListAccountAliases" | "DeleteAccountAlias" => login::run(&call, &action).await?,
        "CreateSAMLProvider" | "GetSAMLProvider" | "ListSAMLProviders" | "UpdateSAMLProvider" | "DeleteSAMLProvider" | "TagSAMLProvider" | "UntagSAMLProvider" | "ListSAMLProviderTags" => saml::run(&call, &action).await?,
        "UploadServerCertificate" | "GetServerCertificate" | "ListServerCertificates" | "UpdateServerCertificate" | "DeleteServerCertificate" | "TagServerCertificate" | "UntagServerCertificate"
        | "ListServerCertificateTags" | "UploadSigningCertificate" | "ListSigningCertificates" | "UpdateSigningCertificate" | "DeleteSigningCertificate" | "UploadSSHPublicKey" | "GetSSHPublicKey"
        | "ListSSHPublicKeys" | "UpdateSSHPublicKey" | "DeleteSSHPublicKey" => certs::run(&call, &action).await?,
        "SimulatePrincipalPolicy" | "SimulateCustomPolicy" | "GetContextKeysForPrincipalPolicy" | "GetContextKeysForCustomPolicy" => simulate::run(&call, &action).await?,
        "GetAccountAuthorizationDetails" | "GenerateCredentialReport" | "GetCredentialReport" => report::run(&call, &action).await?,
        _ => return Err(StsError::new("InvalidAction", 400, format!("Could not find operation {action} for version {VERSION} (roll's IAM: see docs/storage/compatibility.md)"))),
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_dates_and_encoding() {
        assert!(valid_name("alice+ops@x.y-z_1=,", 64));
        assert!(!valid_name("", 64));
        assert!(!valid_name("has space", 64));
        assert!(!valid_name(&"a".repeat(65), 64));
        assert_eq!(date(1_700_000_000_999), "2023-11-14T22:13:20Z");
        assert_eq!(url_encode("{\"a\": 1}"), "%7B%22a%22%3A%201%7D");
        assert_eq!(lower_first("UserName"), "userName");
        assert!(check_tags(&[("a".into(), "1".into()), ("A".into(), "2".into())]).is_err());
        let mut t = vec![("team".to_string(), "a".to_string())];
        merge_tags(&mut t, vec![("TEAM".into(), "b".into()), ("env".into(), "x".into())]).unwrap();
        assert_eq!(t, vec![("team".to_string(), "b".to_string()), ("env".to_string(), "x".to_string())]);
    }

    #[test]
    fn paging() {
        let p = Params(crate::sts::form("MaxItems=2"));
        let out = page(&p, vec!["a", "b", "c"], |s| s.to_string(), |s| s.to_string(), "L").unwrap();
        assert_eq!(out, "<L><member>a</member><member>b</member></L><IsTruncated>true</IsTruncated><Marker>b</Marker>");
        let p = Params(crate::sts::form("Marker=b"));
        assert_eq!(page(&p, vec!["a", "b", "c"], |s| s.to_string(), |s| s.to_string(), "L").unwrap(), "<L><member>c</member></L><IsTruncated>false</IsTruncated>");
    }
}
